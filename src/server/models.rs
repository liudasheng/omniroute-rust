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
/// ids are `provider/model`.
pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let mut data: Vec<Value> = Vec::new();
    for id in state.config.providers_with_keys() {
        let Some(entry) = state.registry.get(&id) else { continue };
        let mut models = entry.default_models.clone();
        models.extend(
            state
                .config
                .credentials
                .get(&id)
                .map(|c| c.model_list.clone())
                .unwrap_or_default(),
        );
        models.extend(
            state
                .config
                .tuning
                .get(&id)
                .map(|t| t.models.clone())
                .unwrap_or_default(),
        );
        models.sort();
        models.dedup();
        for m in models {
            data.push(json!({
                "id": format!("{id}/{m}"),
                "name": m,
                "provider": id,
                "contextLength": context_length_for(&id, &m),
                "supportsReasoning": true,
                "supportsVision": false,
            }));
        }
    }
    let body = json!({"object": "list", "data": data});
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

/// `GET /v1/providers` — live connection health per provider.
pub async fn providers(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let mut data: Vec<Value> = Vec::new();
    for id in state.config.providers_with_keys() {
        let Some(entry) = state.registry.get(&id) else { continue };
        let base = state.config.base_url_for(&state.registry, &id).unwrap_or_default();
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
            "hasKey": state.config.api_key_for(&id).is_some(),
            "inFlight": snapshot.as_ref().map(|(_, i, _)| *i).unwrap_or(0),
            "cooldownMs": snapshot.as_ref().map(|(_, _, c)| *c).unwrap_or(0),
        }));
    }
    (axum::http::StatusCode::OK, axum::Json(json!({ "providers": data }))).into_response()
}

/// `GET /v1/quotas` — request counters per provider.
pub async fn quotas(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
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
    if let Err(e) = crate::server::auth::require(&state, &headers) {
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
