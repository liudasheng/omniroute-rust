//! Chat-family handlers: `/v1/chat/completions`, `/v1/completions`,
//! `/v1/responses`, plus the generic single-provider passthrough
//! (embeddings / rerank / moderations).

use crate::core::chat_core::{handle_chat, ChatRequest};
use crate::errors::ApiError;
use crate::format::Format;
use crate::registry::AuthHeader;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

fn parse_body(bytes: &Bytes) -> Result<Value, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::new(400, format!("invalid JSON body: {e}")))
}

fn extract_model(body: &Value) -> Result<String, ApiError> {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("").trim();
    if model.is_empty() {
        return Err(ApiError::new(400, "missing required field: model"));
    }
    Ok(model.to_string())
}

fn stream_flag(body: &Value) -> bool {
    body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false)
}

/// `POST /v1/chat/completions`
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    run(state, headers, bytes, Format::OpenAI).await
}

/// `POST /v1/completions` (legacy prompt shape)
pub async fn completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    run(state, headers, bytes, Format::Completions).await
}

/// `POST /v1/responses`
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    run(state, headers, bytes, Format::OpenAIResponses).await
}

async fn run(state: Arc<AppState>, headers: HeaderMap, bytes: Bytes, inbound: Format) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let body = match parse_body(&bytes) {
        Ok(b) => b,
        Err(e) => return e.into(),
    };
    let model = match extract_model(&body) {
        Ok(m) => m,
        Err(e) => return e.into(),
    };
    let stream = stream_flag(&body);
    let req = ChatRequest {
        inbound_format: inbound,
        body,
        model_str: model,
        stream,
    };
    handle_chat(state, req).await
}

/// Generic passthrough (`/v1/embeddings`, `/v1/rerank`, `/v1/moderations`):
/// single provider resolved from the `provider/...` model prefix, body
/// forwarded verbatim to `{base}/{sub}`.
pub async fn passthrough_embeddings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    passthrough(state, headers, bytes, "embeddings").await
}

pub async fn passthrough_rerank(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    passthrough(state, headers, bytes, "rerank").await
}

pub async fn passthrough_moderations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    passthrough(state, headers, bytes, "moderations").await
}

async fn passthrough(
    state: Arc<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
    sub: &'static str,
) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let body = match parse_body(&bytes) {
        Ok(b) => b,
        Err(e) => return e.into(),
    };
    let model = match extract_model(&body) {
        Ok(m) => m,
        Err(e) => return e.into(),
    };
    let parsed = crate::model::parse_model(&model);
    let Some(provider) = parsed.provider else {
        return ApiError::new(400, "passthrough endpoints require a 'provider/model' model string").into();
    };
    let Some(entry) = state.registry.get(&provider) else {
        return ApiError::new(404, format!("unknown provider '{provider}'")).into();
    };
    let Some(base) = state.config.base_url_for(&state.registry, &provider) else {
        return ApiError::new(500, format!("no upstream configured for provider '{provider}'")).into();
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), sub.trim_start_matches('/'));

    let mut req_headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    if let Some(k) = state.config.api_key_for(&provider) {
        match entry.auth_header {
            AuthHeader::XApiKey => req_headers.push(("x-api-key".into(), k)),
            AuthHeader::Key => req_headers.push(("Key".into(), k)),
            AuthHeader::XGoogApiKey => req_headers.push(("x-goog-api-key".into(), k)),
            _ => req_headers.push(("authorization".into(), format!("Bearer {k}"))),
        }
    }
    for (k, v) in &entry.extra_headers {
        req_headers.push((k.clone(), v.clone()));
    }

    let resp = state
        .upstream
        .execute(
            &url,
            reqwest::Method::POST,
            req_headers,
            Some(Bytes::from(body.to_string())),
            state.config.request_timeout_ms,
        )
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            let mut out = axum::http::Response::builder().status(status);
            out = out.header("content-type", "application/json");
            out.body(axum::body::Body::from(text)).unwrap()
        }
        Err(e) => ApiError::new(502, format!("network error: {e}")).into(),
    }
}

