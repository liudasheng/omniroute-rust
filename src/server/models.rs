//! Catalog + management read handlers: /v1/models, /v1/providers,
//! /v1/quotas, /v1/combos.

use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;

/// Model-aware context metadata exposed to model pickers.
pub fn context_length_for(provider: &str, model: &str) -> i64 {
    crate::registry::model_context_length(provider, model)
}

fn visible_combo_candidates(state: &AppState, name: &str) -> Vec<crate::router::combo::Candidate> {
    let connected = state.providers_with_keys();
    crate::router::combo::resolve_candidates(state, name)
        .iter()
        .filter(|candidate| {
            connected.iter().any(|provider| provider == &candidate.provider)
                && !state.model_is_hidden(&candidate.provider, &candidate.model)
        })
        .cloned()
        .collect()
}

fn combo_capabilities(state: &AppState, name: &str) -> Vec<crate::registry::ModelCapabilities> {
    visible_combo_candidates(state, name)
        .iter()
        .map(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model))
        .collect()
}

/// Advertise the largest context window available in a combo. Dispatch filters
/// candidates against the actual request size before trying the chain.
pub fn combo_context_length(state: &AppState, name: &str) -> i64 {
    combo_capabilities(state, name)
        .iter()
        .map(|c| c.context_window)
        .max()
        .unwrap_or(262_144)
}

fn efforts_json(efforts: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for level in efforts {
        out.insert(level.clone(), if level == "off" { Value::Null } else { json!(level) });
    }
    Value::Object(out)
}

fn reasoning_compat(state: &AppState, provider: &str, model: &str, reasoning: bool) -> Value {
    if !reasoning {
        return Value::Null;
    }
    match state.format_for_model(provider, model) {
        crate::registry::Format::OpenAI if provider == "openrouter" => json!({
            "supportsReasoningEffort": true,
            "thinkingFormat": "openrouter"
        }),
        crate::registry::Format::OpenAI => json!({
            "supportsReasoningEffort": true,
            "thinkingFormat": "openai"
        }),
        _ => Value::Null,
    }
}

/// `GET /v1/models` — combined catalog of configured providers
/// (`{object:"list", data:[{id, name, provider, contextLength, ...}]}`),
/// ids are `provider/model`. Managed dashboard connections contribute their
/// manual + synced models (minus per-connection hidden models), so a provider
/// added purely through the UI is routable by model id.
pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // `/v1/models` is part of the client-facing OpenAI contract. Dashboard
    // sessions may read it too, but ordinary client API keys (including SDKs,
    // OpenAI SDKs and other model pickers) must not need the admin role.
    if crate::server::auth::management_allowed(&state, &headers).is_err() {
        if let Err(e) = crate::server::auth::require(&state, &headers) {
            return e.into();
        }
    }
    let mut data: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push_models = |id: &str, models: Vec<String>, data: &mut Vec<Value>, seen: &mut std::collections::HashSet<String>| {
        for m in models {
            let full = format!("{id}/{m}");
            if !seen.insert(full.clone()) {
                continue;
            }
            let caps = state.capabilities_for_model(id, &m);
            let model_format = state.format_for_model(id, &m);
            let free_lane = crate::opencode_free::is_free_lane_provider(id)
                && crate::opencode_free::decide(&m, id).allowed;
            data.push(json!({
                "id": full,
                "name": m,
                "provider": id,
                "api": model_format.as_str(),
                "contextLength": caps.context_window,
                "contextWindow": caps.context_window,
                "maxOutputTokens": caps.max_output_tokens,
                "maxTokens": caps.max_output_tokens,
                "supportsReasoning": caps.supports_reasoning,
                "reasoning": caps.supports_reasoning,
                "supportsThinking": caps.supports_reasoning,
                "reasoningEfforts": efforts_json(&caps.reasoning_efforts),
                "thinkingLevels": caps.reasoning_efforts,
                "compat": reasoning_compat(&state, id, &m, caps.supports_reasoning),
                "supportsVision": caps.supports_vision,
                "supportsPdf": caps.supports_pdf,
                "modalities": caps.input.clone(),
                "input": caps.input,
                "free": free_lane,
                "freeLane": free_lane,
            }));
        }
    };
    // One state-aware source of truth: this includes static config plus
    // dashboard connections and applies connection-level hidden models.
    // Keeping this as one pass prevents a synced model re-entering through a
    // second registry/config path after a connection PATCH.
    for id in state.providers_with_keys() {
        let mut models = state.models_for_provider(&id);
        // A keyless OpenCode Zen provider exists only because of the anonymous
        // free lane, so expose exactly what the lane can actually serve —
        // the live directory ∩ the free verdict — rather than the static seed
        // list, whose delisted `*-free` ids would 404 on the first call.
        if state.api_key_for(&id).is_none() && crate::opencode_free::is_free_lane_provider(&id) {
            let roster: std::collections::HashSet<String> =
                crate::opencode_free::models().into_iter().collect();
            models.retain(|m| roster.contains(m));
        }
        push_models(&id, models, &mut data, &mut seen);
    }
    // Combos are first-class model ids for OpenAI-compatible clients. Include
    // persisted dashboard combos, TOML combos and the built-in auto/* catalog
    // so clients can discover and select routing policies from `/v1/models`.
    for name in state
        .combos
        .list()
        .into_iter()
        .filter(|c| c.enabled)
        .map(|c| c.name)
        .chain(state.config.combos.iter().map(|c| c.name.clone()))
        .chain(crate::router::combo::AUTO_COMBO_NAMES.iter().map(|s| s.to_string()))
    {
        if seen.insert(name.clone()) {
            if visible_combo_candidates(&state, &name).is_empty() {
                continue;
            }
            let caps = combo_capabilities(&state, &name);
            let combo_vision = caps.iter().any(|c| c.supports_vision);
            let combo_pdf = caps.iter().any(|c| c.supports_pdf);
            let combo_reasoning = caps.iter().any(|c| c.supports_reasoning);
            let mut efforts: Vec<String> = Vec::new();
            for level in caps.iter().flat_map(|c| c.reasoning_efforts.iter().cloned()) {
                if !efforts.contains(&level) { efforts.push(level); }
            }
            let mut input = vec!["text".to_string()];
            if combo_vision { input.push("image".into()); }
            if combo_pdf { input.push("pdf".into()); }
            let max_tokens = caps.iter().map(|c| c.max_output_tokens).max().unwrap_or(32_768);
            data.push(json!({
                "id": name,
                "name": name,
                "provider": "combo",
                "contextLength": combo_context_length(&state, &name),
                "contextWindow": combo_context_length(&state, &name),
                "maxOutputTokens": max_tokens,
                "maxTokens": max_tokens,
                "supportsReasoning": combo_reasoning,
                "reasoning": combo_reasoning,
                "supportsThinking": combo_reasoning,
                "reasoningEfforts": efforts_json(&efforts),
                "thinkingLevels": efforts,
                "supportsVision": combo_vision,
                "supportsPdf": combo_pdf,
                "modalities": input.clone(),
                "input": input,
                "isCombo": true,
            }));
        }
    }
    let body = json!({"object": "list", "data": data});
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

/// `GET /v1/providers` — live connection health per provider.
pub async fn providers(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let mut data: Vec<Value> = Vec::new();
    for id in state.providers_with_keys() {
        let Some(entry) = state.registry.get(&id) else { continue };
        let connected = state.api_key_for(&id).is_some()
            || state
                .provider_connections
                .all_unmasked()
                .iter()
                .any(|connection| {
                    connection.enabled
                        && connection.provider == id
                        && (connection.api_key.as_ref().is_some_and(|key| crate::config::is_usable_api_key(key))
                            || connection.base_url.as_ref().is_some_and(|url| crate::config::is_usable_base_url(url)))
                })
            || state
                .config
                .tuning
                .get(&id)
                .and_then(|tuning| tuning.base_url.as_ref())
                .is_some_and(|url| crate::config::is_usable_base_url(url));
        let base = state.base_url_for(&state.registry, &id).unwrap_or_default();
        let snapshot = state
            .circuits
            .snapshot()
            .into_iter()
            .find(|(k, _, _)| k == &id);
        data.push(json!({
            "id": id,
            "format": entry.format.as_str(),
            "baseUrl": base,
            "authType": format!("{:?}", entry.auth_type).to_lowercase(),
            "isLocal": entry.is_local,
            "hasKey": state.api_key_for(&id).is_some(),
            "connected": connected,
            "inFlight": snapshot.as_ref().map(|(_, i, _)| *i).unwrap_or(0),
            "cooldownMs": snapshot.as_ref().map(|(_, _, c)| *c).unwrap_or(0),
        }));
    }
    (axum::http::StatusCode::OK, axum::Json(json!({ "providers": data }))).into_response()
}

/// `GET /v1/model-catalog` — runtime model-capability catalog state.
pub async fn model_catalog(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let path = crate::model_catalog::cache_path(&state.config.data_dir);
    let cached_at = crate::model_catalog::load_disk(&path).map(|(_, at)| at);
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "models": crate::model_catalog::model_count(),
            "autoUpdate": state.config.model_catalog_auto_update,
            "ttlHours": state.config.model_catalog_ttl_hours,
            "url": state.config.model_catalog_url,
            "cachedAtMs": cached_at,
            "cacheFile": path,
        })),
    )
        .into_response()
}

/// `POST /v1/model-catalog/refresh` — force a catalog refresh now.
pub async fn model_catalog_refresh(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let client = reqwest::Client::new();
    match crate::model_catalog::refresh(&client, &state.config.model_catalog_url, &state.config.data_dir).await {
        Ok((rows, fetched_at)) => (
            axum::http::StatusCode::OK,
            axum::Json(json!({"ok": true, "rows": rows, "models": crate::model_catalog::model_count(), "cachedAtMs": fetched_at})),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_GATEWAY,
            axum::Json(json!({"ok": false, "detail": error.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/quotas` — request counters per provider.
pub async fn quotas(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let rows: Vec<Value> = state
        .circuits
        .snapshot()
        .into_iter()
        .map(|(id, inflight, cooldown)| {
            json!({
                "provider": id,
                "rpmBudget": crate::router::circuit::DEFAULT_RPM,
                "rpmWindowHits": state.rate.hit_count(&id),
                "concurrent": inflight,
                "cooldownMs": cooldown,
            })
        })
        .collect();
    // OpenCode discloses no anonymous-lane quota API (three independent DSH
    // quota plugins say so explicitly), so the free lane reports what it can
    // observe: the discovered roster, its learned denials and the counts.
    let free = crate::opencode_free::snapshot();
    let free_lane = json!({
        "status": crate::opencode_free::status().as_str(),
        "models": crate::opencode_free::models(),
        "directoryIds": free.live.len(),
        "lastRefreshMs": if free.live_at_ms == 0 { Value::Null } else { json!(free.live_at_ms) },
        "lastError": free.live_error,
        "denied": free.denials.iter().map(|(model, denial)| json!({
            "model": model,
            "reason": denial.reason.as_str(),
            "status": denial.status,
            "untilMs": denial.until_ms,
        })).collect::<Vec<Value>>(),
        "verified": free.verified.iter().map(|(model, at)| json!({"model": model, "atMs": at})).collect::<Vec<Value>>(),
        "successes": free.success_count,
        "failures": free.failure_count,
    });
    // Subscription tier: rolling/weekly/monthly percentages read straight from
    // the provider's own usage endpoint.
    let subscription = crate::opencode_quota::snapshot().map(|usage| {
        json!({
            "provider": usage.provider,
            "url": usage.url,
            "fetchedAtMs": usage.fetched_at_ms,
            "fresh": usage.fresh(now_ms()),
            "error": usage.error,
            "rolling": usage.rolling,
            "weekly": usage.weekly,
            "monthly": usage.monthly,
            "gatedForMs": crate::opencode_quota::gated(now_ms()).unwrap_or(0),
        })
    });
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "quotas": rows, "opencodeFreeLane": free_lane, "opencodeSubscription": subscription })),
    )
        .into_response()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `GET /v1/opencode-free` — the discovered anonymous free roster and the
/// learned per-model availability (management auth).
pub async fn opencode_free_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    crate::opencode_free::prune();
    let snapshot = crate::opencode_free::snapshot();
    let payload = json!({
        "provider": "opencode",
        "alias": "opencode-zen",
        "baseUrl": state
            .base_url_for(&state.registry, "opencode")
            .unwrap_or_else(|| crate::opencode_free::ZEN_BASE_URL.to_string()),
        "status": crate::opencode_free::status().as_str(),
        "autoUpdate": state.config.opencode_free_auto_update,
        "ttlSeconds": state.config.opencode_free_ttl_seconds,
        "directory": {
            "url": format!("{}/v1/models", state.config.opencode_free_url.trim_end_matches('/')),
            "count": snapshot.live.len(),
            "fetchedAtMs": if snapshot.live_at_ms == 0 { Value::Null } else { json!(snapshot.live_at_ms) },
            "error": snapshot.live_error,
        },
        "exposed": crate::opencode_free::models(),
        "roster": crate::opencode_free::roster_rows(),
        "totals": {
            "successes": snapshot.success_count,
            "failures": snapshot.failure_count,
            "verified": snapshot.verified.len(),
            "denied": snapshot.denials.len(),
        },
        "cacheFile": state.config.data_dir.join(crate::opencode_free::CACHE_FILE),
    });
    (axum::http::StatusCode::OK, axum::Json(payload)).into_response()
}

/// `POST /v1/opencode-free/refresh` — force a live-directory refresh now.
pub async fn opencode_free_refresh(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let client = reqwest::Client::new();
    let base = state
        .base_url_for(&state.registry, "opencode")
        .unwrap_or_else(|| state.config.opencode_free_url.clone());
    match crate::opencode_free::refresh(&client, &base).await {
        Ok(count) => {
            let _ = crate::opencode_free::save_disk(&state.config.data_dir);
            state.audit("opencode_free.refresh", format!("ids={count}"), true);
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({"ok": true, "directoryIds": count,
                    "exposed": crate::opencode_free::models(),
                    "status": crate::opencode_free::status().as_str()})),
            )
                .into_response()
        }
        Err(error) => {
            state.audit("opencode_free.refresh", format!("error: {error}"), false);
            (
                axum::http::StatusCode::BAD_GATEWAY,
                axum::Json(json!({"ok": false, "detail": error, "status": crate::opencode_free::status().as_str()})),
            )
                .into_response()
        }
    }
}

/// `GET /v1/opencode-usage` — discovered OpenCode Go subscription quota.
pub async fn opencode_usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let usage = crate::opencode_quota::snapshot();
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "autoUpdate": state.config.opencode_quota_auto_update,
            "ttlSeconds": state.config.opencode_quota_ttl_seconds,
            "usage": usage.as_ref().map(|u| json!({
                "provider": u.provider,
                "url": u.url,
                "fetchedAtMs": u.fetched_at_ms,
                "fresh": u.fresh(now_ms()),
                "error": u.error,
                "rolling": u.rolling,
                "weekly": u.weekly,
                "monthly": u.monthly,
            })),
            "gatedForMs": crate::opencode_quota::gated(now_ms()).unwrap_or(0),
        })),
    )
        .into_response()
}

/// `POST /v1/opencode-usage/refresh` — re-read the subscription usage now.
pub async fn opencode_usage_refresh(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let provider = "opencode-go";
    let base = state
        .base_url_for(&state.registry, provider)
        .unwrap_or_else(|| "https://opencode.ai/zen/go/v1".to_string());
    let key = state
        .api_key_for(provider)
        .or_else(|| std::env::var("OPENCODE_GO_API_KEY").ok());
    let client = reqwest::Client::new();
    match crate::opencode_quota::refresh(&client, provider, &base, key).await {
        Ok(usage) => {
            state.audit("opencode_usage.refresh", format!("url={}", usage.url), true);
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({"ok": true, "usage": usage, "gatedForMs": crate::opencode_quota::gated(now_ms()).unwrap_or(0)})),
            )
                .into_response()
        }
        Err(error) => {
            state.audit("opencode_usage.refresh", format!("error: {error}"), false);
            // 401/403 simply means "no subscription on this key" — a state, not
            // a gateway failure, so report it as a 200 with the error field.
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({"ok": false, "detail": error, "gatedForMs": 0})),
            )
                .into_response()
        }
    }
}

/// `GET /v1/combos` — configured combos.
pub async fn combos(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let combos: Vec<Value> = state
        .config
        .combos
        .iter()
        .map(|c| {
            let caps = combo_capabilities(&state, &c.name);
            let combo_vision = caps.iter().any(|cap| cap.supports_vision);
            let combo_pdf = caps.iter().any(|cap| cap.supports_pdf);
            json!({
                "name": c.name,
                "strategy": c.strategy.clone().unwrap_or_else(|| "priority".into()),
                "providers": c.providers,
                "models": c.models,
                "supportsVision": combo_vision,
                "supportsPdf": combo_pdf,
                "modalities": if combo_pdf { json!(["text", "image", "pdf"]) } else if combo_vision { json!(["text", "image"]) } else { json!(["text"]) },
                "contextWindow": caps.iter().map(|cap| cap.context_window).max().unwrap_or(262_144),
                "maxTokens": caps.iter().map(|cap| cap.max_output_tokens).max().unwrap_or(32_768),
            })
        })
        .collect();
    (axum::http::StatusCode::OK, axum::Json(json!({ "combos": combos }))).into_response()
}

/// `GET /v1/compression` — effective compression configuration
/// (runtime-mutable via POST; boot source = toml `[compression]` / env).
pub async fn compression_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let c = state
        .compression_config
        .read()
        .map(|c| c.clone())
        .unwrap_or_else(|_| state.config.compression.clone());
    let body = json!({
        "enabled": c.enabled,
        "default_mode": c.default_mode.as_str(),
        "auto_trigger_tokens": c.auto_trigger_tokens,
        "auto_trigger_mode": c.auto_trigger_mode.as_str(),
        "preserve_system_prompt": c.preserve_system_prompt,
        "caveman_intensity": c.caveman_intensity,
        "compress_roles": c.compress_roles,
        "skip_rules": c.skip_rules,
        "min_message_length": c.min_message_length,
        "ultra_compression_rate": c.ultra_compression_rate,
        "ultra_min_score": c.ultra_min_score,
        "aggressive_max_tokens_per_message": c.aggressive_max_tokens_per_message,
        "aggressive_min_savings": c.aggressive_min_savings,
        "rtk_max_lines": c.rtk_max_lines,
        "modes": ["off", "lite", "standard", "aggressive", "ultra", "rtk"],
        "per_request_header": "x-omniroute-compression",
    });
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

#[derive(serde::Deserialize)]
pub struct CompressionUpdate {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub default_mode: Option<String>,
    #[serde(default)]
    pub auto_trigger_tokens: Option<i64>,
    #[serde(default)]
    pub auto_trigger_mode: Option<String>,
    #[serde(default)]
    pub preserve_system_prompt: Option<bool>,
    #[serde(default)]
    pub caveman_intensity: Option<String>,
    #[serde(default)]
    pub min_message_length: Option<usize>,
    #[serde(default)]
    pub ultra_compression_rate: Option<f64>,
    #[serde(default)]
    pub ultra_min_score: Option<f64>,
    #[serde(default)]
    pub rtk_max_lines: Option<usize>,
}

/// `POST /v1/compression` — runtime-mutate compression settings
/// (dashboard "Save"; the toml file remains the boot source).
pub async fn compression_config_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::Json<CompressionUpdate>,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let b = body.0;
    let mut errors: Vec<String> = Vec::new();
    if let Some(m) = &b.default_mode {
        if crate::compression::CompressionMode::parse(m).is_none() {
            errors.push(format!("unknown mode: {m}"));
        }
    }
    if let Some(m) = &b.auto_trigger_mode {
        if crate::compression::CompressionMode::parse(m).is_none() {
            errors.push(format!("unknown auto_trigger_mode: {m}"));
        }
    }
    if let Some(i) = &b.caveman_intensity {
        if !["lite", "full", "ultra"].contains(&i.as_str()) {
            errors.push(format!("unknown caveman_intensity: {i}"));
        }
    }
    if !errors.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": { "message": errors.join("; "), "type": "invalid_request_error", "code": "bad_request" } })),
        )
            .into_response();
    }

    let applied;
    if let Ok(mut c) = state.compression_config.write() {
        if let Some(v) = b.enabled {
            c.enabled = v;
        }
        if let Some(m) = &b.default_mode {
            if let Some(m) = crate::compression::CompressionMode::parse(m) {
                c.default_mode = m;
            }
        }
        if let Some(v) = b.auto_trigger_tokens {
            c.auto_trigger_tokens = v;
        }
        if let Some(m) = &b.auto_trigger_mode {
            if let Some(m) = crate::compression::CompressionMode::parse(m) {
                c.auto_trigger_mode = m;
            }
        }
        if let Some(v) = b.preserve_system_prompt {
            c.preserve_system_prompt = v;
        }
        if let Some(i) = &b.caveman_intensity {
            c.caveman_intensity = i.clone();
        }
        if let Some(v) = b.min_message_length {
            c.min_message_length = v;
        }
        if let Some(v) = b.ultra_compression_rate {
            c.ultra_compression_rate = v.clamp(0.05, 1.0);
        }
        if let Some(v) = b.ultra_min_score {
            c.ultra_min_score = v.clamp(0.0, 1.0);
        }
        if let Some(v) = b.rtk_max_lines {
            c.rtk_max_lines = v.clamp(10, 5000);
        }
        applied = c.clone();
    } else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": { "message": "config lock poisoned", "type": "server_error", "code": "internal_error" } })),
        )
            .into_response();
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "enabled": applied.enabled,
            "default_mode": applied.default_mode.as_str(),
            "auto_trigger_tokens": applied.auto_trigger_tokens,
            "caveman_intensity": applied.caveman_intensity,
        })),
    )
        .into_response()
}

/// `GET /v1/settings` — runtime settings snapshot (rate limits + modes).
pub async fn settings(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "rate_rpm": state.config.rate_rpm,
            "rate_min_interval_ms": state.config.rate_min_interval_ms,
            "rate_concurrent_requests": state.config.rate_concurrent_requests,
            "rate_max_wait_ms": state.config.rate_max_wait_ms,
            "compression_default_mode": state.compression_config.read().map(|c| c.default_mode.as_str()).unwrap_or("off"),
            "thinking_mode": state.config.thinking_mode,
            "thinking_budget": state.config.thinking_budget,
            "api_auth": if state.config.api_key.is_some() || !state.api_keys.list().iter().all(|k| !k.enabled) { "key-required" } else { "open" },
            "timeouts_ms": {
                "request": state.config.request_timeout_ms,
                "connect": state.config.connect_timeout_ms,
                "stream_idle": state.config.stream_idle_timeout_ms,
                "stream_readiness": state.config.readiness_timeout_ms,
                "stream_readiness_max": state.config.readiness_max_timeout_ms,
                "sse_heartbeat": state.config.heartbeat_ms,
                "disconnect_grace": state.config.disconnect_grace_ms,
            },
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::context_length_for;

    #[test]
    fn context_length_uses_model_capability_before_provider_default() {
        assert_eq!(context_length_for("openai-compatible-test", "gpt-5.6-luna"), 1_000_000);
        assert_eq!(context_length_for("openai-compatible-test", "gpt-5.6-terra"), 1_000_000);
        assert_eq!(context_length_for("openai-compatible-test", "custom-model"), 262_144);
        assert_eq!(context_length_for("openai", "gpt-4.1"), 1_047_576);
    }
}
