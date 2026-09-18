//! Catalog + management read handlers: /v1/models, /v1/providers,
//! /v1/quotas, /v1/combos.

use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;

/// Rough per-model context defaults (registry parity field).
fn context_length_for(provider: &str, _model: &str) -> i64 {
    match provider {
        "anthropic" | "zai" => 200_000,
        "gemini" => 1_000_000,
        "openai" => 400_000,
        "groq" => 131_072,
        "deepseek" => 164_000,
        "kimi" => 262_144,
        "glm" => 200_000,
        _ => 128_000,
    }
}

/// `GET /v1/models` — combined catalog of configured providers
/// (`{object:"list", data:[{id, name, provider, contextLength, ...}]}`),
/// ids are `provider/model`. Managed dashboard connections contribute their
/// manual + synced models (minus per-connection hidden models), so a provider
/// added purely through the UI is routable by model id.
pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // `/v1/models` is part of the client-facing OpenAI contract. Dashboard
    // sessions may read it too, but ordinary client API keys (including DSH,
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
            data.push(json!({
                "id": full,
                "name": m,
                "provider": id,
                "contextLength": context_length_for(id, &m),
                "supportsReasoning": true,
                "supportsVision": false,
            }));
        }
    };
    // One state-aware source of truth: this includes static config plus
    // dashboard connections and applies connection-level hidden models.
    // Keeping this as one pass prevents a synced model re-entering through a
    // second registry/config path after a connection PATCH.
    for id in state.providers_with_keys() {
        push_models(&id, state.models_for_provider(&id), &mut data, &mut seen);
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
            data.push(json!({
                "id": name,
                "name": name,
                "provider": "combo",
                "contextLength": 128000,
                "supportsReasoning": true,
                "supportsVision": true,
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
            "inFlight": snapshot.as_ref().map(|(_, i, _)| *i).unwrap_or(0),
            "cooldownMs": snapshot.as_ref().map(|(_, _, c)| *c).unwrap_or(0),
        }));
    }
    (axum::http::StatusCode::OK, axum::Json(json!({ "providers": data }))).into_response()
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
    (axum::http::StatusCode::OK, axum::Json(json!({ "quotas": rows }))).into_response()
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
            json!({
                "name": c.name,
                "strategy": c.strategy.clone().unwrap_or_else(|| "priority".into()),
                "providers": c.providers,
                "models": c.models,
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
