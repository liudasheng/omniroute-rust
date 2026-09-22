//! Anthropic-native handlers: `/v1/messages` (+ `count_tokens`).

use crate::core::chat_core::{handle_chat, ChatRequest};
use crate::errors::ApiError;
use crate::format::Format;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;
use std::net::SocketAddr;

/// `POST /v1/messages` — Anthropic wire format in/out.
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    bytes: Bytes,
) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("").trim().to_string();
    if model.is_empty() {
        return ApiError::new(400, "missing required field: model").into();
    }
    let stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let compression_header = headers
        .get("x-omniroute-compression")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let req = ChatRequest {
        inbound_format: Format::Claude,
        body,
        model_str: model,
        stream,
        endpoint: "/v1/messages".into(),
        client_ip: crate::core::chat_core::client_ip_from_headers(&headers, remote),
        reasoning_effort: None,
        compression_header,
    };
    handle_chat(state, req).await
}

/// `POST /v1/messages/count_tokens` — local estimation
/// (tokens ≈ utf8 bytes / 4), no upstream round-trip.
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    let _ = &state;
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
    let mut total = 0usize;
    if let Some(s) = body.get("system").and_then(|s| s.as_str()) {
        total += s.len();
    }
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            total += serde_json::to_string(m).map(|s| s.len()).unwrap_or(0);
        }
    }
    let tokens = (total as f64 / 4.0).ceil() as i64;
    (axum::http::StatusCode::OK, axum::Json(json!({ "input_tokens": tokens }))).into_response()
}
