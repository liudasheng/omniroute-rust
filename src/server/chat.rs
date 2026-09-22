//! Chat-family handlers: `/v1/chat/completions`, `/v1/completions`,
//! `/v1/responses`, plus the generic single-provider passthrough
//! (embeddings / rerank / moderations).

use crate::core::chat_core::{handle_chat, ChatRequest};
use crate::errors::ApiError;
use crate::format::Format;
use crate::registry::RegistryEntry;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
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
    run(state, headers, bytes, Format::OpenAI, "/v1/chat/completions").await
}

/// `POST /v1/completions` (legacy prompt shape)
pub async fn completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    run(state, headers, bytes, Format::Completions, "/v1/completions").await
}

/// `POST /v1/responses`
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    run(state, headers, bytes, Format::OpenAIResponses, "/v1/responses").await
}

async fn run(state: Arc<AppState>, headers: HeaderMap, bytes: Bytes, inbound: Format, endpoint: &str) -> axum::response::Response {
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
    let compression_header = headers
        .get("x-omniroute-compression")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let req = ChatRequest {
        inbound_format: inbound,
        body,
        model_str: model,
        stream,
        endpoint: endpoint.to_string(),
        client_ip: crate::core::chat_core::client_ip_from_headers(&headers),
        reasoning_effort: None,
        compression_header,
    };
    handle_chat(state, req).await
}

/// Resolve the provider for passthrough endpoints, from (in priority order):
/// 1. `x-omniroute-provider` request header
/// 2. `model` field in the JSON body (`provider/model`)
/// 3. `model` form field in multipart bodies (`model=provider/...`)
fn resolve_passthrough_provider(state: &AppState, headers: &HeaderMap, bytes: &[u8]) -> Result<(String, Option<String>), ApiError> {
    if let Some(p) = headers.get("x-omniroute-provider").and_then(|v| v.to_str().ok()) {
        let parsed = crate::model::parse_model(p);
        return Ok((
            parsed.provider.unwrap_or_else(|| p.to_string()),
            Some(parsed.model),
        ));
    }
    // try JSON first
    if let Ok(body) = serde_json::from_slice::<Value>(bytes) {
        if let Some(m) = body.get("model").and_then(|m| m.as_str()) {
            let parsed = parse_model_with_registry(state, m);
            if let Some(provider) = parsed.provider {
                return Ok((provider, Some(m.to_string())));
            }
        }
        return Err(ApiError::new(400, "passthrough endpoints require a 'provider/model' model string or an x-omniroute-provider header"));
    }
    // multipart form-data: find name="model"
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.starts_with("multipart/form-data") {
        if let Some(m) = extract_multipart_model(bytes) {
            let parsed = parse_model_with_registry(state, &m);
            if let Some(provider) = parsed.provider {
                return Ok((provider, Some(m)));
            }
        }
        return Err(ApiError::new(400, "multipart requests need a model form field like 'provider/model' or an x-omniroute-provider header"));
    }
    Err(ApiError::new(400, "passthrough endpoints require a 'provider/model' model string or an x-omniroute-provider header"))
}

/// Crude multipart scan: find the `model` field value in a multipart body.
fn extract_multipart_model(body: &[u8]) -> Option<String> {
    let marker = b"name=\"model\"";
    let idx = body
        .windows(marker.len())
        .position(|w| w == marker)?;
    let after = &body[idx + marker.len()..];
    // skip past the header separator, the value runs until CRLF
    let b = after;
    // skip past the header separator (CRLF CRLF), then the value runs until CRLF
    let sep = b.windows(4).position(|w| w == b"\r\n\r\n")?;
    let mut value: Vec<u8> = Vec::new();
    let mut i = sep + 4;
    while i < b.len() {
        if b[i] == b'\r' && i + 1 < b.len() && b[i + 1] == b'\n' {
            break;
        }
        value.push(b[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&value).to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn auth_headers_for(state: &AppState, entry: &RegistryEntry, provider: &str) -> Vec<(String, String)> {
    let mut req_headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    if let Some(k) = state.api_key_for(provider) {
        match entry.auth_header {
            crate::registry::AuthHeader::XApiKey => req_headers.push(("x-api-key".into(), k)),
            crate::registry::AuthHeader::Key => req_headers.push(("Key".into(), k)),
            crate::registry::AuthHeader::XGoogApiKey => req_headers.push(("x-goog-api-key".into(), k)),
            _ => req_headers.push(("authorization".into(), format!("Bearer {k}"))),
        }
    }
    for (k, v) in &entry.extra_headers {
        req_headers.push((k.clone(), v.clone()));
    }
    req_headers
}

async fn forward_single_provider(
    state: Arc<AppState>,
    provider: String,
    sub: &'static str,
    bytes: Bytes,
    content_type: Option<String>,
    method: reqwest::Method,
) -> axum::response::Response {
    let Some(entry) = state.registry.get(&provider) else {
        return ApiError::new(404, format!("unknown provider '{provider}'")).into_response();
    };
    let Some(base) = state.base_url_for(&state.registry, &provider) else {
        return ApiError::new(500, format!("no upstream configured for provider '{provider}'")).into_response();
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), sub.trim_start_matches('/'));

    let mut req_headers: Vec<(String, String)> = auth_headers_for(&state, &entry, &provider);
    // preserve the client's content-type when it is not plain JSON
    if let Some(ct) = content_type {
        req_headers.retain(|(k, _)| k != "content-type");
        req_headers.push(("content-type".into(), ct));
    }

    let body = if bytes.is_empty() { None } else { Some(bytes) };
    match state
        .upstream
        .execute(&url, method, req_headers, body, state.config.request_timeout_ms)
        .await
    {
        Ok(r) => {
            let status = r.status();
            let ct = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body = r.bytes().await.unwrap_or_default();
            let mut out = axum::http::Response::builder().status(status);
            out = out.header("content-type", ct);
            out.body(axum::body::Body::from(body)).unwrap()
        }
        Err(e) => ApiError::new(502, format!("network error: {e}")).into_response(),
    }
}

/// parse_model extended with the live registry (dynamic `openai-compatible-*`
/// families are registry ids, not static aliases).
fn parse_model_with_registry(state: &AppState, model: &str) -> crate::model::ParsedModel {
    let parsed = crate::model::parse_model(model);
    if parsed.provider.is_none() {
        if let Some(idx) = model.find('/') {
            let (prov, rest) = model.split_at(idx);
            if !prov.is_empty() && state.registry.contains(prov) {
                return crate::model::ParsedModel {
                    provider: Some(prov.to_string()),
                    model: rest[1..].to_string(),
                    extended_context: parsed.extended_context,
                    is_alias: false,
                };
            }
        }
    }
    parsed
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
    // provider resolution: header override first, then body
    if let Some(p) = headers.get("x-omniroute-provider").and_then(|v| v.to_str().ok()) {
        let parsed = crate::model::parse_model(p);
        let provider = parsed.provider.unwrap_or_else(|| p.to_string());
        return forward_single_provider(state, provider, sub, bytes, None, reqwest::Method::POST).await;
    }
    let body = match parse_body(&bytes) {
        Ok(b) => b,
        Err(e) => return e.into(),
    };
    let model = match extract_model(&body) {
        Ok(m) => m,
        Err(e) => return e.into(),
    };
    let parsed = parse_model_with_registry(&state, &model);
    let Some(provider) = parsed.provider else {
        return ApiError::new(400, "passthrough endpoints require a 'provider/model' model string or an x-omniroute-provider header").into();
    };
    forward_single_provider(state, provider, sub, Bytes::from(body.to_string()), None, reqwest::Method::POST).await
}

// thin wrappers: JSON passthrough surfaces
macro_rules! json_passthrough_handler {
    ($fn_name:ident, $sub:literal) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            headers: HeaderMap,
            bytes: Bytes,
        ) -> axum::response::Response {
            passthrough(state, headers, bytes, $sub).await
        }
    };
}

json_passthrough_handler!(passthrough_embeddings, "embeddings");
json_passthrough_handler!(passthrough_rerank, "rerank");
json_passthrough_handler!(passthrough_moderations, "moderations");
json_passthrough_handler!(passthrough_images_generations, "images/generations");
json_passthrough_handler!(passthrough_images_edits, "images/edits");
json_passthrough_handler!(passthrough_images_upscale, "images/upscale");
json_passthrough_handler!(passthrough_videos, "videos");
json_passthrough_handler!(passthrough_batches_create, "batches");
json_passthrough_handler!(passthrough_speech_to_text_alias, "audio/transcriptions");

/// Raw passthrough (multipart / binary): forwards the client's body and
/// content-type verbatim — audio transcriptions/speech/translations, files.
async fn passthrough_raw(
    state: Arc<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
    sub: &'static str,
    method: reqwest::Method,
) -> axum::response::Response {
    
    if let Err(e) = crate::server::auth::require(&state, &headers) {
        return e.into();
    }
    let (provider, _) = match resolve_passthrough_provider(&state, &headers, &bytes) {
        Ok(v) => v,
        Err(e) => return e.into(),
    };
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    forward_single_provider(state, provider, sub, bytes, ct, method).await
}

macro_rules! raw_passthrough_handler {
    ($fn_name:ident, $sub:literal) => {
        pub async fn $fn_name(
            State(state): State<Arc<AppState>>,
            headers: HeaderMap,
            bytes: Bytes,
        ) -> axum::response::Response {
            passthrough_raw(state, headers, bytes, $sub, reqwest::Method::POST).await
        }
    };
}

raw_passthrough_handler!(passthrough_audio_transcriptions, "audio/transcriptions");
raw_passthrough_handler!(passthrough_audio_translations, "audio/translations");
raw_passthrough_handler!(passthrough_audio_speech, "audio/speech");
raw_passthrough_handler!(passthrough_speech_to_text, "speech-to-text");
raw_passthrough_handler!(passthrough_text_to_speech, "text-to-speech");
raw_passthrough_handler!(passthrough_ocr, "ocr");
raw_passthrough_handler!(passthrough_files, "files");

/// `POST /v1/batches` (JSON) — alias of passthrough_batches_create.
pub async fn passthrough_batches(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    passthrough(state, headers, bytes, "batches").await
}

/// `GET /v1/batches` / `GET /v1/batches/{id}` — provider list/get via the
/// `x-omniroute-provider` header (batches have no model field).
pub async fn passthrough_batches_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let provider = match headers.get("x-omniroute-provider").and_then(|v| v.to_str().ok()) {
        Some(p) => p.to_string(),
        None => return ApiError::new(400, "batch listing requires an x-omniroute-provider header").into_response(),
    };
    forward_single_provider(state, provider, "batches", Bytes::new(), None, reqwest::Method::GET).await
}

pub async fn passthrough_batches_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(batch_id): axum::extract::Path<String>,
) -> axum::response::Response {
    let provider = match headers.get("x-omniroute-provider").and_then(|v| v.to_str().ok()) {
        Some(p) => p.to_string(),
        None => return ApiError::new(400, "batch lookup requires an x-omniroute-provider header").into_response(),
    };
    let sub = format!("batches/{batch_id}");
    forward_single_provider_owned(state, provider, sub, Bytes::new(), None, reqwest::Method::GET).await
}

async fn forward_single_provider_owned(
    state: Arc<AppState>,
    provider: String,
    sub: String,
    bytes: Bytes,
    _content_type: Option<String>,
    method: reqwest::Method,
) -> axum::response::Response {
    let Some(entry) = state.registry.get(&provider) else {
        return ApiError::new(404, format!("unknown provider '{provider}'")).into_response();
    };
    let Some(base) = state.base_url_for(&state.registry, &provider) else {
        return ApiError::new(500, format!("no upstream configured for provider '{provider}'")).into_response();
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), sub.trim_start_matches('/'));
    let headers_v = auth_headers_for(&state, &entry, &provider);
    let body = if bytes.is_empty() { None } else { Some(bytes) };
    match state.upstream.execute(&url, method, headers_v, body, state.config.request_timeout_ms).await {
        Ok(r) => {
            let status = r.status();
            let ct = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body = r.bytes().await.unwrap_or_default();
            axum::http::Response::builder()
                .status(status)
                .header("content-type", ct)
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        Err(e) => ApiError::new(502, format!("network error: {e}")).into_response(),
    }
}
