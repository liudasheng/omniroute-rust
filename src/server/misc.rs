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

/// `POST /v1/combos/test` — dry-run: resolve the candidate chain for a model
/// string without calling any upstream. Body: `{"model": "..."}`.
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
    let candidates = combo::resolve_candidates(&state, model);
    let chain: Vec<Value> = candidates
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
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"model": model, "candidates": chain})),
    )
        .into_response()
}
