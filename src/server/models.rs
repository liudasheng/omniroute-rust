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
    // `opencode` and `opencode-zen` are two names for one upstream: same base
    // URL, same catalogue, identical routing. `providers_with_keys` admits both
    // (either may be the configured one, and the lane must stay reachable with
    // no connection at all), so presenting every id it returns listed each free
    // model twice under two ids the client cannot tell apart. Show one alias:
    // the id that actually has a connection, else the canonical one.
    let connected_ids: Vec<String> = state
        .provider_connections
        .all_unmasked()
        .into_iter()
        .filter(|c| c.enabled)
        .map(|c| c.provider)
        .collect();
    let lane_alias: Option<&str> = if crate::opencode_free::is_free_lane_provider(
        &connected_ids
            .iter()
            .find(|p| crate::opencode_free::is_free_lane_provider(p))
            .cloned()
            .unwrap_or_default(),
    ) {
        connected_ids
            .iter()
            .find(|p| crate::opencode_free::is_free_lane_provider(p))
            .map(String::as_str)
    } else {
        None
    };
    for id in state.providers_with_keys() {
        if crate::opencode_free::is_free_lane_provider(&id) {
            let keep = lane_alias.unwrap_or(crate::opencode_free::CANONICAL_LANE_PROVIDER);
            if id != keep {
                continue;
            }
        }
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
        // A keyed call refused by the workspace opt-in parks the lane on the
        // anonymous credential until this moment.
        "consentGateUntilMs": free.consent_gate_until_ms,
        // The lane's own gate refusing every shape this gateway can build parks
        // the automatic zero-cost chain until this moment (0 = gate open).
        "laneGateUntilMs": free.lane_gate_until_ms,
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
    // Local UTC-day budget: credential mode, learned scope and per-credential
    // counters. Ids are masked (`conn:ab••••yz`); the label is the
    // operator-visible connection name, so the dashboard can tell which
    // account is spent without exposing its internal id or its key.
    let limits = crate::opencode_free_budget::BudgetLimits::from_config(&state.config);
    let budget = crate::opencode_free_budget::snapshot(now_ms(), &limits);
    let sessions = crate::opencode_free::session_runtime();
    let warm_sessions: Vec<Value> = sessions
        .warm
        .iter()
        .map(|lane| {
            json!({
                "lane": lane.lane,
                // First six characters only: enough to correlate a session in
                // the log, not enough to reuse it.
                "sessions": lane.sessions,
            })
        })
        .collect();
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for provider in ["opencode", "opencode-zen"] {
        for account in state.accounts_for(provider) {
            let bucket = crate::opencode_free_budget::account_bucket(&account, provider);
            labels
                .entry(crate::opencode_free_budget::mask_bucket(&bucket))
                .or_insert_with(|| account.label());
        }
    }
    // A proxied bucket id carries the masked egress (`conn:ab••••yz @
    // 104.28.*.*:3128`); the account label is keyed by the credential half.
    let credentials: Vec<Value> = budget
        .buckets
        .iter()
        .map(|bucket| {
            let base_id = bucket.id.split(" @ ").next().unwrap_or(&bucket.id);
            let label = if bucket.anonymous {
                "anonymous".to_string()
            } else if base_id.starts_with("config:") {
                "env/config credential".to_string()
            } else {
                labels
                    .get(base_id)
                    .cloned()
                    .unwrap_or_else(|| "unavailable connection".to_string())
            };
            json!({
                "id": bucket.id,
                "label": label,
                "anonymous": bucket.anonymous,
                "used": bucket.used,
                "limit": bucket.limit,
                "learnedLimit": bucket.learned_limit,
                "learnedAtMs": bucket.learned_at_ms,
                "learnedAtIso": if bucket.learned_at_ms == 0 {
                    Value::Null
                } else {
                    json!(crate::opencode_free_budget::utc_iso(bucket.learned_at_ms))
                },
                "effectiveLimit": bucket.effective_limit,
                "resetAtMs": bucket.reset_at_ms,
                // UTC ISO alongside the epoch ms: the window boundary is the
                // next 00:00 UTC, which is what an operator actually reads.
                "resetAtIso": crate::opencode_free_budget::utc_iso(bucket.reset_at_ms),
                "cooldownUntilMs": bucket.cooldown_until_ms,
                "cooldownUntilIso": if bucket.cooldown_until_ms == 0 {
                    Value::Null
                } else {
                    json!(crate::opencode_free_budget::utc_iso(bucket.cooldown_until_ms))
                },
                "inFlight": bucket.in_flight,
                "exhausted": bucket.exhausted,
            })
        })
        .collect();
    // Egress view: configured exits (pool + per-account proxy_url) merged with
    // whatever the budget has already observed. Proxy credentials never appear:
    // the labels are masked `host:port` and the mode is a scheme name.
    let free_accounts = crate::opencode_free_budget::free_lane_accounts(&state);
    let exits = crate::opencode_free_egress::exit_statuses(
        &state.config,
        &free_accounts,
        &budget.egresses,
    );
    let pool_size = crate::opencode_free_egress::pool_egresses(&state.config).len();
    let payload = json!({
        "provider": "opencode",
        "alias": "opencode-zen",
        "credentialMode": state.config.opencode_free_credential_mode.as_str(),
        "egress": {
            "poolSize": pool_size,
            "maxExitsPerRequest": state.config.opencode_free_max_proxy_exits,
            "proxyCooldownSecs": state.config.opencode_free_proxy_cooldown_secs,
            // The anonymous credential anchors its start exit on the
            // conversation seed, so consecutive turns keep the same IP.
            "sticky": "session",
            "exits": exits,
        },
        "bucketScope": budget.scope.as_str(),
        "sessionFallback": state.config.opencode_free_session_fallback,
        "sessionAdmissionSuspected": budget.session_admission_suspected,
        "shortQuotaRefusals": budget.short_quota_refusals,
        "sessionAdmissionBlocked": sessions.admission_blocked,
        "newSessionRefusals": sessions.new_session_refusals,
        "sessionMemoEntries": sessions.memo_entries,
        "warmSessions": warm_sessions,
        "budget": {
            "dailyLimitPerAccount": limits.per_account,
            "dailyLimitPublic": limits.public,
            "learnedLimit": limits.learned,
            "learnedLimitTtlDays": state.config.opencode_free_learned_limit_ttl_days,
            "learnedMinSamples": limits.learned_min_samples,
            "shortQuotaCooldownSecs": state.config.opencode_free_short_quota_cooldown_secs,
            "maxInFlightPerAccount": limits.max_in_flight,
            "windowResetAtMs": budget.window_reset_at_ms,
            "windowResetAtIso": crate::opencode_free_budget::utc_iso(budget.window_reset_at_ms),
            "scope": budget.scope.as_str(),
            "credentials": credentials,
        },
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

/// `POST /v1/opencode-usage/refresh` — re-read every credential's usage now.
pub async fn opencode_usage_refresh(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let client = reqwest::Client::new();
    let updated = crate::opencode_quota::refresh_all_credentials(&state, &client).await;
    state.audit(
        "opencode_usage.refresh",
        format!("credentials refreshed: {updated}"),
        updated > 0,
    );
    // Keep the legacy single-reading shape for existing callers, and add the
    // per-credential view the new section renders.
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "ok": true,
            "updated": updated,
            "usage": crate::opencode_quota::snapshot().as_deref().cloned(),
            "gatedForMs": crate::opencode_quota::gated(now_ms()).unwrap_or(0),
            "groups": account_quota_groups(&state),
        })),
    )
        .into_response()
}

/// `GET /v1/account-quotas` — usage per *credential*, grouped by probe family.
///
/// Only providers the operator actually connected are reported, and a key
/// registered under several provider ids appears once: one subscription is one
/// quota, so listing it twice under two provider names would invent a choice
/// that does not exist.
pub async fn account_quotas(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "groups": account_quota_groups(&state) })),
    )
        .into_response()
}

/// Build the grouped view shared by both endpoints.
fn account_quota_groups(state: &Arc<AppState>) -> Vec<Value> {
    let readings = crate::opencode_quota::snapshot_all();
    let overrides = state.quota_overrides.list();

    // credential fingerprint → the connection names registered with it
    let mut by_credential: Vec<(String, String, Vec<Value>)> = Vec::new();
    for conn in state.provider_connections.all_unmasked() {
        if !conn.enabled {
            continue;
        }
        let Some(probe) = crate::usage_probe::probe_for(&conn.provider) else {
            continue;
        };
        let Some(key) = conn.api_key.as_deref().filter(|k| !k.trim().is_empty()) else {
            continue;
        };
        let credential = crate::usage_probe::credential_id(key);
        let label = if conn.name.is_empty() {
            conn.id.clone()
        } else {
            conn.name.clone()
        };
        let entry = json!({"account": label, "connectionId": conn.id, "provider": conn.provider});
        match by_credential.iter_mut().find(|(c, _, _)| c == &credential) {
            Some((_, _, accounts)) => accounts.push(entry),
            None => by_credential.push((
                credential,
                probe.id().to_string(),
                vec![entry],
            )),
        }
    }

    // Group the credentials under their probe family.
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for (credential, probe_id, accounts) in by_credential {
        let reading = readings.get(&credential);
        let windows: Vec<Value> = match reading {
            Some(u) => [("rolling", u.rolling.as_ref()), ("weekly", u.weekly.as_ref()), ("monthly", u.monthly.as_ref())]
                .into_iter()
                .filter_map(|(kind, w)| {
                    let w = w?;
                    Some(json!({
                        "kind": kind,
                        "status": w.status,
                        "usedPercent": w.used_percent,
                        "remainingPercent": w.remaining_percent(),
                        "resetsAtMs": w.resets_at_ms,
                    }))
                })
                .collect(),
            None => Vec::new(),
        };
        let error = reading.as_ref().map(|u| u.error.clone()).unwrap_or_default();
        let probe_shape = accounts
            .iter()
            .filter_map(|a| a.get("provider").and_then(|p| p.as_str()))
            .find_map(crate::usage_probe::probe_for)
            .map(|p| p.shape().as_str())
            .unwrap_or("windowed");
        let source = if reading.is_none() {
            "unknown"
        } else if !error.is_empty() {
            "error"
        } else {
            "upstream"
        };
        let providers: Vec<String> = {
            let mut ps: Vec<String> = accounts
                .iter()
                .filter_map(|a| a.get("provider").and_then(|p| p.as_str()).map(str::to_string))
                .collect();
            ps.sort();
            ps.dedup();
            ps
        };
        let row = json!({
            "credential": credential,
            "accounts": accounts,
            "providers": providers,
            "providerCount": providers.len(),
            "source": source,
            "error": error,
            "windows": windows,
            "shape": probe_shape,
            "balance": reading.as_ref().and_then(|u| u.balance),
            "currency": reading.as_ref().and_then(|u| u.currency.clone()),
            "fetchedAtMs": reading.as_ref().map(|u| u.fetched_at_ms).unwrap_or(0),
            // Manual overrides belong to the provider, not the credential; expose
            // them so a provider without a usage endpoint still shows something.
            "manualBalance": overrides
                .iter()
                .find(|o| providers.iter().any(|p| p == &o.provider))
                .and_then(|o| o.balance),
        });
        match groups.iter_mut().find(|(g, _)| g == &probe_id) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((probe_id, vec![row])),
        }
    }

    groups
        .into_iter()
        .map(|(probe, rows)| {
            let providers: Vec<String> = rows
                .iter()
                .flat_map(|r| {
                    r.get("providers")
                        .and_then(|p| p.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            json!({
                "probe": probe,
                "source": "upstream",
                "providers": providers,
                "accountCount": rows.len(),
                "accounts": rows,
            })
        })
        .collect()
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
        "lrc_profile": c.lrc_profile,
        "lrc_schema": c.lrc_schema_mode.clone().unwrap_or_else(|| "preset".to_string()),
        "lrc_reasoning": c.lrc_trim_reasoning.clone().unwrap_or_else(|| "preset".to_string()),
        "lrc_filter": c.lrc_filter.clone().unwrap_or_else(|| "preset".to_string()),
        "lrc_retire_args": c.lrc_retire_args,
        "lrc_dedupe_results": c.lrc_dedupe_results,
        "lrc_trim_assistant_text": c.lrc_trim_assistant_text,
        "lrc_dump": c.lrc_dump,
        "lrc_dump_dir": c.lrc_dump_dir.as_ref().map(|p| p.display().to_string()),
        "lrc_dump_max_files": c.lrc_dump_max_files,
        "lrc_recent_turns": c.lrc_recent_turns,
        "lrc_args_min_tokens": c.lrc_args_min_tokens,
        "lrc_text_min_tokens": c.lrc_text_min_tokens,
        "lrc_head_lines": c.lrc_head_lines,
        "lrc_tail_lines": c.lrc_tail_lines,
        "modes": ["off", "lite", "standard", "aggressive", "ultra", "rtk", "lrc"],
        "lrc_profiles": ["conservative", "balanced", "aggressive", "extreme"],
        "persisted_path": state.config.data_dir.join("omniroute.toml").display().to_string(),
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
    // LRC fields
    #[serde(default, rename = "lrc_profile")]
    pub lrc_profile: Option<String>,
    #[serde(default, rename = "lrc_schema")]
    pub lrc_schema_mode: Option<String>,
    #[serde(default, rename = "lrc_reasoning")]
    pub lrc_trim_reasoning: Option<String>,
    #[serde(default, rename = "lrc_filter")]
    pub lrc_filter: Option<String>,
    #[serde(default, rename = "lrc_retire_args")]
    pub lrc_retire_args: Option<bool>,
    #[serde(default, rename = "lrc_dedupe_results")]
    pub lrc_dedupe_results: Option<bool>,
    #[serde(default, rename = "lrc_trim_assistant_text")]
    pub lrc_trim_assistant_text: Option<bool>,
    #[serde(default, rename = "lrc_args_min_tokens")]
    pub lrc_args_min_tokens: Option<i64>,
    #[serde(default, rename = "lrc_text_min_tokens")]
    pub lrc_text_min_tokens: Option<i64>,
    #[serde(default, rename = "lrc_recent_turns")]
    pub lrc_recent_turns: Option<usize>,
    #[serde(default, rename = "lrc_head_lines")]
    pub lrc_head_lines: Option<usize>,
    #[serde(default, rename = "lrc_tail_lines")]
    pub lrc_tail_lines: Option<usize>,
    #[serde(default, rename = "lrc_dump")]
    pub lrc_dump: Option<bool>,
    #[serde(default, rename = "lrc_dump_dir")]
    pub lrc_dump_dir: Option<String>,
    #[serde(default, rename = "lrc_dump_max_files")]
    pub lrc_dump_max_files: Option<usize>,
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
    if let Some(p) = &b.lrc_profile {
        if crate::compression::lrc::Profile::parse(p).is_none() {
            errors.push(format!("unknown lrc_profile: {p}"));
        }
    }
    if let Some(m) = &b.lrc_schema_mode {
        if crate::compression::lrc::schema::SchemaMode::parse(m).is_none() {
            errors.push(format!("unknown lrc_schema: {m}"));
        }
    }
    if let Some(m) = &b.lrc_trim_reasoning {
        if crate::compression::lrc::TrimReasoning::parse(m).is_none() {
            errors.push(format!("unknown lrc_reasoning: {m}"));
        }
    }
    if let Some(m) = &b.lrc_filter {
        if crate::compression::lrc::filter::FilterMode::parse(m).is_none() {
            errors.push(format!("unknown lrc_filter: {m}"));
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
    let previous;
    if let Ok(mut c) = state.compression_config.write() {
        previous = c.clone();
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
        if let Some(v) = b.lrc_profile {
            c.lrc_profile = v;
        }
        if let Some(v) = b.lrc_schema_mode {
            c.lrc_schema_mode = Some(v);
        }
        if let Some(v) = b.lrc_trim_reasoning {
            c.lrc_trim_reasoning = Some(v);
        }
        if let Some(v) = b.lrc_filter {
            c.lrc_filter = Some(v);
        }
        if let Some(v) = b.lrc_retire_args {
            c.lrc_retire_args = Some(v);
        }
        if let Some(v) = b.lrc_dedupe_results {
            c.lrc_dedupe_results = Some(v);
        }
        if let Some(v) = b.lrc_trim_assistant_text {
            c.lrc_trim_assistant_text = Some(v);
        }
        if let Some(v) = b.lrc_args_min_tokens {
            c.lrc_args_min_tokens = Some(v);
        }
        if let Some(v) = b.lrc_text_min_tokens {
            c.lrc_text_min_tokens = Some(v);
        }
        if let Some(v) = b.lrc_recent_turns {
            c.lrc_recent_turns = Some(v);
        }
        if let Some(v) = b.lrc_head_lines {
            c.lrc_head_lines = Some(v);
        }
        if let Some(v) = b.lrc_tail_lines {
            c.lrc_tail_lines = Some(v);
        }
        if let Some(v) = b.lrc_dump {
            c.lrc_dump = v;
        }
        if let Some(v) = b.lrc_dump_dir {
            c.lrc_dump_dir = Some(std::path::PathBuf::from(v));
        }
        if let Some(v) = b.lrc_dump_max_files {
            c.lrc_dump_max_files = v.clamp(10, 100_000);
        }
        applied = c.clone();
    } else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": { "message": "config lock poisoned", "type": "server_error", "code": "internal_error" } })),
        )
            .into_response();
    }

    // Persist before publishing: the dashboard must survive a restart. If the
    // write fails, roll the in-memory config back so the runtime never claims
    // a setting that is not on disk.
    let persisted = crate::config::persist_compression_config(&state.config.data_dir, &applied);
    let persisted_path = match persisted {
        Ok(p) => p,
        Err(e) => {
            if let Ok(mut c) = state.compression_config.write() {
                *c = previous;
            }
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": { "message": format!("persist compression settings failed: {e}"), "type": "server_error", "code": "persist_failed" } })),
            )
                .into_response();
        }
    };
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "persisted": true,
            "persisted_path": persisted_path.display().to_string(),
            "enabled": applied.enabled,
            "default_mode": applied.default_mode.as_str(),
            "auto_trigger_tokens": applied.auto_trigger_tokens,
            "caveman_intensity": applied.caveman_intensity,
            "lrc_profile": applied.lrc_profile,
            "lrc_schema": applied.lrc_schema_mode.clone().unwrap_or_else(|| "preset".to_string()),
            "lrc_reasoning": applied.lrc_trim_reasoning.clone().unwrap_or_else(|| "preset".to_string()),
            "lrc_filter": applied.lrc_filter.clone().unwrap_or_else(|| "preset".to_string()),
            "lrc_retire_args": applied.lrc_retire_args,
            "lrc_dedupe_results": applied.lrc_dedupe_results,
            "lrc_trim_assistant_text": applied.lrc_trim_assistant_text,
            "lrc_dump": applied.lrc_dump,
            "lrc_dump_dir": applied.lrc_dump_dir.as_ref().map(|p| p.display().to_string()),
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

    /// `opencode` and `opencode-zen` are one upstream under two names: same base
    /// URL, same catalogue, identical routing. Listing both made every free
    /// model appear twice under ids a client cannot distinguish.
    #[test]
    fn only_one_free_lane_alias_is_presented() {
        for connected in [None, Some("opencode"), Some("opencode-zen")] {
            let keep = connected.unwrap_or(crate::opencode_free::CANONICAL_LANE_PROVIDER);
            for id in ["opencode", "opencode-zen"] {
                let presented = id == keep;
                // A connected alias always wins; otherwise the canonical one
                // stands in. Exactly one of the two may be presented.
                let expected = match connected {
                    Some(c) => id == c,
                    None => id == crate::opencode_free::CANONICAL_LANE_PROVIDER,
                };
                assert_eq!(
                    presented, expected,
                    "connected={connected:?} id={id}: exactly one alias must be shown"
                );
            }
        }
    }
}
