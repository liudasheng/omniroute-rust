//! Misc handlers: catch-all 404 (JSON, never HTML) and combo test replay.

use crate::errors::ApiError;
use crate::router::combo;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;

/// Catch-all: unknown /v1/* & /api/* → OpenAI-shaped JSON 404
/// (parity: catch-all `route.ts`, `unknown_route`).
pub async fn not_found(uri: axum::http::Uri) -> axum::response::Response {
    ApiError::not_found_unknown_route(uri.path()).into()
}

/// `POST /v1/combos/test` — resolve a combo candidate chain. With
/// `{"execute":true}` it also sends a one-token probe to every model in the
/// resolved combo (the dashboard's Test button); without it this remains a
/// cheap dry-run used by auto-combo duplication.
pub async fn combos_test(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    // This endpoint is a dashboard dry-run, not an inference request. The
    // original dashboard calls it with its management session; keep master
    // and admin API keys valid through the same management policy.
    if let Err(e) = crate::server::auth::require_management(&state, &_headers) {
        return e.into();
    }
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
    let Some(model) = body.get("model").and_then(|m| m.as_str()) else {
        return ApiError::new(400, "missing required field: model").into();
    };
    let execute = body.get("execute").and_then(|v| v.as_bool()).unwrap_or(false);
    let candidates = combo::resolve_candidates(&state, model);
    let mut chain: Vec<Value> = candidates
        .iter()
        .map(|c| {
            json!({
                "provider": c.provider,
                "model": c.model,
                "combo": c.combo,
                "position": c.position,
                "available": state.circuits.is_available(&c.provider),
                "modelBanned": state.circuits.is_model_banned(&c.provider, &c.model),
            })
        })
        .collect();
    if execute {
        for (candidate, result) in candidates.iter().zip(chain.iter_mut()) {
            let connection = state
                .provider_connections
                .all_unmasked()
                .into_iter()
                .find(|c| c.enabled && c.provider == candidate.provider)
                .unwrap_or_else(|| crate::server::providers_admin::ProviderConnection {
                    id: format!("probe-{}", candidate.provider),
                    provider: candidate.provider.clone(),
                    name: candidate.provider.clone(),
                    api_key: None,
                    base_url: None,
                    api_type: None,
                    model_list: vec![candidate.model.clone()],
                    synced_models: Vec::new(),
                    synced_at_ms: 0,
                    hidden_models: Vec::new(),
                    enabled: true,
                    created_at_ms: 0,
                });
            let (ok, latency, detail) = crate::server::admin::probe_connection_with_model(
                &state,
                &connection,
                Some(candidate.model.clone()),
            )
            .await;
            result["tested"] = json!(true);
            result["ok"] = json!(ok);
            result["latency_ms"] = json!(latency);
            result["detail"] = json!(detail);
        }
    }
    let tested = chain.iter().filter(|c| c["tested"].as_bool().unwrap_or(false)).count();
    let passed = chain.iter().filter(|c| c["ok"].as_bool().unwrap_or(false)).count();
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "model": model,
            "executed": execute,
            "candidates": chain,
            "summary": if execute { json!({"total": tested, "passed": passed, "failed": tested - passed}) } else { Value::Null },
        })),
    )
        .into_response()
}
