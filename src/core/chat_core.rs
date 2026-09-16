//! Core chat orchestration (parity: `open-sse/handlers/chatCore.ts`).
//!
//! Pipeline: detect inbound format → parse model → resolve candidates →
//! per-candidate: translate request → upstream execute → translate response
//! (JSON or streaming SSE) → on failure classify + cooldown + next candidate.
//! Bounded by MAX_GLOBAL_ATTEMPTS and COMBO_LOOP_SAFETY_TIMEOUT_MS.

use crate::errors::{ApiError, FailureKind};
use crate::format::Format;
use crate::registry::{Format as ProvFormat, RegistryEntry};
use crate::router::combo::{self, MAX_GLOBAL_ATTEMPTS};
use crate::state::AppState;
use crate::sse::{frame_data, frame_event, frame_keepalive, SseParser, SseEvent, DONE_MARKER};
use crate::translate::gemini::{
    gemini_response_to_openai, gemini_stream_to_openai_chunks, openai_chunk, GeminiStreamState,
};
use crate::translate::openai_claude::{
    claude_request_to_openai, claude_response_to_openai, openai_request_to_claude, openai_response_to_claude,
};
use crate::translate::responses::{chat_response_to_responses, responses_request_to_chat, ResponsesStreamState};
use crate::translate::stream::{ClaudeToOpenaiStream, OpenaiToClaudeStream};
use crate::upstream::executor::build_upstream_request;
use axum::body::Body;
use bytes::Bytes;
use futures::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Incoming request context after endpoint parsing.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// inbound wire format (derived from the request path)
    pub inbound_format: Format,
    /// inbound body (openai / claude / responses shape)
    pub body: Value,
    /// model string as received (provider/model, alias, or bare)
    pub model_str: String,
    pub stream: bool,
    /// `x-omniroute-compression` request header value (per-request override)
    pub compression_header: Option<String>,
}

/// Result of one candidate attempt.
enum TryResult {
    /// fully-formed downstream reply
    Responded(axum::response::Response),
    /// try the next candidate with this failure
    Next(ApiError),
}

// ---------------------------------------------------------------------------
// request translation
// ---------------------------------------------------------------------------

/// Translate a request body between wire formats and aim it at `model`.
fn translate_request_body(from: Format, to: ProvFormat, body: &Value, model: String, stream: bool) -> Value {
    let mut openai_shape: Value = match from {
        Format::OpenAI => body.clone(),
        Format::Claude => claude_request_to_openai(body),
        Format::OpenAIResponses => responses_request_to_chat(body),
        Format::Gemini => body.clone(),
        Format::Completions => completions_request_to_chat(body),
    };
    openai_shape["model"] = json!(model);
    openai_shape["stream"] = json!(stream);

    match to {
        ProvFormat::OpenAI => openai_shape,
        ProvFormat::Claude => {
            let mut cl = openai_request_to_claude(&openai_shape);
            cl["model"] = json!(model);
            cl["stream"] = json!(stream);
            cl
        }
        ProvFormat::Gemini => crate::translate::gemini::openai_request_to_gemini(&openai_shape),
        ProvFormat::OpenAIResponses => {
            let mut r = crate::translate::responses::chat_request_to_responses(&openai_shape);
            r["model"] = json!(model);
            r["stream"] = json!(stream);
            r
        }
    }
}

/// Translate an upstream JSON response back into the inbound format.
fn translate_json_response(from: ProvFormat, to: Format, resp: &Value, model: &str) -> Value {
    // normalize upstream → openai chat completion, then re-shape for inbound
    let openai_json = match from {
        ProvFormat::OpenAI | ProvFormat::OpenAIResponses => resp.clone(),
        ProvFormat::Claude => claude_response_to_openai(resp, model),
        ProvFormat::Gemini => gemini_response_to_openai(resp, model),
    };
    match to {
        Format::OpenAI => openai_json,
        Format::Claude => openai_response_to_claude(&openai_json, model),
        Format::OpenAIResponses => chat_response_to_responses(&openai_json, model),
        Format::Gemini => openai_json,
        Format::Completions => chat_completion_to_completions(&openai_json),
    }
}

/// Build canonical openai chunks from any upstream JSON response, for
/// synthesizing an SSE stream when the provider ignored `stream`.
fn synthesize_canonical_chunks(upstream_json: &Value, model: &str) -> Vec<Value> {
    let openai_json = match upstream_json.get("object").and_then(|o| o.as_str()) {
        Some("message") => claude_response_to_openai(upstream_json, model),
        Some("response") => upstream_json.clone(), // already handled upstream-side
        _ => upstream_json.clone(),
    };
    let created = openai_json.get("created").and_then(|c| c.as_i64()).unwrap_or(0);
    let choice = openai_json.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let content = choice.pointer("/message/content").cloned().unwrap_or(json!(""));
    let finish = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("stop").to_string();
    let mut chunks = vec![
        openai_chunk("chatcmpl-synth", created, model, json!({"role": "assistant"}), None),
        openai_chunk("chatcmpl-synth", created, model, json!({"content": content}), None),
    ];
    let mut last = openai_chunk("chatcmpl-synth", created, model, json!({}), Some(finish));
    if let Some(u) = openai_json.get("usage") {
        last["usage"] = u.clone();
    }
    chunks.push(last);
    chunks
}

// ---------------------------------------------------------------------------
// responses (responses-format helpers re-exported for handlers)
// ---------------------------------------------------------------------------

pub fn responses_skeleton(response_id: &str, model: &str, created: i64) -> Value {
    crate::translate::responses::response_skeleton(response_id, model, created)
}

// ---------------------------------------------------------------------------
// main entry
// ---------------------------------------------------------------------------

/// Handle a chat-family request end-to-end and produce the downstream reply.
pub async fn handle_chat(state: Arc<AppState>, req: ChatRequest) -> axum::response::Response {
    // Proactive context compression (parity: chatCore compression setup).
    // Applied to the inbound body before candidate resolution; the effective
    // mode is echoed back via the x-omniroute-compression response header.
    // The runtime config (dashboard-editable) wins over the boot config.
    let runtime_cfg = state
        .compression_config
        .read()
        .map(|c| c.clone())
        .unwrap_or_else(|_| state.config.compression.clone());
    let compression = crate::compression::apply(&req.body, &runtime_cfg, req.compression_header.as_deref());
    let compression_header_value = compression.response_header.clone();
    let tokens_saved = compression.stats.as_ref().map(|s| (s.original_tokens - s.compressed_tokens).max(0)).unwrap_or(0);
    let compressed_flag = compression.stats.is_some_and(|s| s.compressed_tokens < s.original_tokens);
    let req = ChatRequest {
        inbound_format: req.inbound_format,
        body: compression.body,
        model_str: req.model_str,
        stream: req.stream,
        compression_header: req.compression_header,
    };

    let candidates = combo::resolve_candidates(&state, &req.model_str);
    if candidates.is_empty() {
        let e = ApiError::new(404, format!("model not found: {}", req.model_str));
        return e.into();
    }

    let started = Instant::now();
    let deadline = started + Duration::from_millis(crate::router::combo::COMBO_LOOP_SAFETY_TIMEOUT_MS);
    let mut attempts = 0usize;
    let mut last_err: Option<ApiError> = None;
    let combo_name = candidates.first().and_then(|c| c.combo.clone()).unwrap_or_default();

    for cand in candidates {
        if attempts >= MAX_GLOBAL_ATTEMPTS || Instant::now() >= deadline {
            break;
        }
        let Some(entry) = state.registry.get(&cand.provider) else {
            last_err = Some(ApiError::new(500, format!("unknown provider '{}'", cand.provider)));
            continue;
        };
        if !state.circuits.is_available(&cand.provider) {
            tracing::debug!(provider = %cand.provider, "candidate skipped: circuit open / cooldown / concurrency");
            continue;
        }
        if state.circuits.is_model_banned(&cand.provider, &cand.model) {
            tracing::debug!(provider = %cand.provider, model = %cand.model, "candidate skipped: model banned");
            continue;
        }
        // rate gate: the original queues up to maxWaitMs before failing the
        // dispatch; local providers bypass the interval limiter entirely.
        if state.config.rate_auto_enable_api_key_providers && !entry.is_local {
            let granted = state
                .rate
                .wait_permit(&cand.provider, Duration::from_millis(state.config.rate_max_wait_ms))
                .await;
            if !granted {
                tracing::debug!(provider = %cand.provider, "candidate skipped: rate queue wait exceeded");
                continue;
            }
        }
        attempts += 1;
        state.circuits.begin_request(&cand.provider);
        let result = try_candidate(&state, &req, &cand, &entry).await;
        state.circuits.end_request(&cand.provider);
        match result {
            TryResult::Responded(resp) => {
                state.circuits.record_success(&cand.provider);
                state.log_request(crate::state::RequestLogEntry {
                    ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
                    model: req.model_str.clone(),
                    provider: Some(cand.provider.clone()),
                    status: 200,
                    latency_ms: started.elapsed().as_millis() as u64,
                    tokens_saved,
                    compressed: compressed_flag,
                });
                return attach_compression_header(resp, compression_header_value.clone());
            }
            TryResult::Next(err) => {
                let kind = FailureKind::from_status(err.status);
                if kind == FailureKind::Client {
                    // 400 = user-fixable parameter issue; surface immediately,
                    // no provider penalty (parity: param-validation predicate).
                    return attach_compression_header(err.into(), compression_header_value.clone());
                }
                state
                    .circuits
                    .record_failure(&cand.provider, kind, entry.is_local, entry.auth_type);
                tracing::warn!(provider = %cand.provider, status = err.status, msg = %err.message, "candidate failed; falling back");
                last_err = Some(err);
            }
        }
    }

    // all candidates exhausted → combo diagnostics error (parity:
    // errorResponseWithComboDiagnostics)
    let status = last_err.as_ref().map(|e| e.status).unwrap_or(502);
    let mut message = format!(
        "All providers failed for model '{}' (combo '{}', attempts: {attempts}).",
        req.model_str,
        if combo_name.is_empty() { "-" } else { &combo_name }
    );
    if let Some(e) = &last_err {
        message.push_str(&format!(" Last error: [{} {}] {}", e.etype, e.code, e.message));
    }
    state.log_request(crate::state::RequestLogEntry {
        ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
        model: req.model_str.clone(),
        provider: None,
        status,
        latency_ms: started.elapsed().as_millis() as u64,
        tokens_saved,
        compressed: compressed_flag,
    });
    attach_compression_header(
        ApiError { status, message, ..ApiError::new(status, "") }.into(),
        compression_header_value,
    )
}

/// Append the compression meta header when compression ran.
pub fn attach_compression_header(mut resp: axum::response::Response, value: Option<String>) -> axum::response::Response {
    if let Some(v) = value {
        if let Ok(hv) = axum::http::HeaderValue::from_str(&v) {
            resp.headers_mut()
                .insert(axum::http::HeaderName::from_static("x-omniroute-compression"), hv);
        }
    }
    resp
}

/// Execute one candidate end-to-end: translate → upstream → translate back.
async fn try_candidate(
    state: &Arc<AppState>,
    req: &ChatRequest,
    cand: &combo::Candidate,
    entry: &RegistryEntry,
) -> TryResult {
    let want_stream = req.stream;
    let upstream_stream = want_stream || entry.force_stream;

    let upstream_body = translate_request_body(req.inbound_format, entry.format, &req.body, cand.model.clone(), upstream_stream);
    let (url, headers) = match build_upstream_request(&state.config, &state.registry, entry, &cand.provider, &cand.model, upstream_stream) {
        Ok(v) => v,
        Err(e) => return TryResult::Next(e),
    };

    let resp = match state
        .upstream
        .execute(&url, reqwest::Method::POST, headers, Some(Bytes::from(upstream_body.to_string())), state.config.request_timeout_ms)
        .await
    {
        Ok(r) => r,
        Err(e) => return TryResult::Next(ApiError::new(502, format!("network error: {e}"))),
    };

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let body_json = resp.json::<Value>().await.ok();
        return TryResult::Next(crate::upstream::executor::upstream_error(status, body_json));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_sse = content_type.contains("text/event-stream");

    if is_sse {
        if want_stream {
            TryResult::Responded(stream_response_pump(state, resp, req.inbound_format, entry.format, cand.model.clone()))
        } else {
            // forceStream provider (kimi-style): fold upstream SSE into JSON
            match accumulate_stream_json(state, resp, entry.format, cand.model.clone()).await {
                Ok(full) => TryResult::Responded(json_response(translate_json_response(entry.format, req.inbound_format, &full, &cand.model))),
                Err(e) => TryResult::Next(e),
            }
        }
    } else {
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => return TryResult::Next(ApiError::new(502, format!("upstream read error: {e}"))),
        };
        let upstream_json: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return TryResult::Next(ApiError::new(502, "upstream returned invalid JSON")),
        };
        if want_stream {
            // provider ignored `stream` → synthesize SSE downstream
            let chunks = synthesize_canonical_chunks(&upstream_json, &cand.model);
            let mut sink = InboundSink::new(req.inbound_format, &cand.model);
            let mut frames = Vec::new();
            for c in &chunks {
                frames.extend(sink.emit(c));
            }
            frames.extend(sink.finish());
            TryResult::Responded(sse_response(frames))
        } else {
            TryResult::Responded(json_response(translate_json_response(entry.format, req.inbound_format, &upstream_json, &cand.model)))
        }
    }
}

// ---------------------------------------------------------------------------
// legacy completions projection
// ---------------------------------------------------------------------------

/// Convert a legacy completions request `{model, prompt, max_tokens, ...}`
/// into an openai chat body.
pub fn completions_request_to_chat(body: &Value) -> Value {
    let prompt = body.get("prompt").cloned().unwrap_or(json!(""));
    let messages = match &prompt {
        Value::String(s) => json!([{"role": "user", "content": s}]),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|p| json!({"role": "user", "content": p}))
                .collect(),
        ),
        other => json!([{"role": "user", "content": other.clone()}]),
    };
    let mut out = body.clone();
    out["messages"] = messages;
    if let Some(o) = out.as_object_mut() {
        o.remove("prompt");
    }
    out
}

/// Project an openai chat.completion JSON onto the legacy completion shape.
pub fn chat_completion_to_completions(chat: &Value) -> Value {
    let choice = chat.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let text = choice.pointer("/message/content").and_then(|t| t.as_str()).unwrap_or("");
    json!({
        "id": chat.get("id").cloned().unwrap_or(json!("cmpl-omniroute")),
        "object": "text_completion",
        "created": chat.get("created").cloned().unwrap_or(json!(0)),
        "model": chat.get("model").cloned().unwrap_or(json!("unknown")),
        "choices": [{"index": 0, "text": text, "finish_reason": choice.get("finish_reason").cloned().unwrap_or(json!("stop"))}],
        "usage": chat.get("usage").cloned().unwrap_or(json!({}))
    })
}

/// Project an openai chat chunk onto a legacy completion stream chunk.
pub fn chat_chunk_to_completions(chunk: &Value) -> Value {
    let delta_text = chunk
        .pointer("/choices/0/delta/content")
        .cloned()
        .unwrap_or(json!(""));
    let finish = chunk.pointer("/choices/0/finish_reason").cloned().unwrap_or(Value::Null);
    let mut out = json!({
        "id": chunk.get("id").cloned().unwrap_or(json!("cmpl-omniroute")),
        "object": "text_completion",
        "created": chunk.get("created").cloned().unwrap_or(json!(0)),
        "model": chunk.get("model").cloned().unwrap_or(json!("unknown")),
        "choices": [{"index": 0, "text": delta_text, "finish_reason": finish}]
    });
    if let Some(u) = chunk.get("usage") {
        if u.is_object() {
            out["usage"] = u.clone();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// streaming
// ---------------------------------------------------------------------------

/// Upstream SSE → canonical openai chunks.
enum UpstreamSource {
    Openai,
    Claude(ClaudeToOpenaiStream),
    Gemini(GeminiStreamState, String, i64),
}

impl UpstreamSource {
    fn new(format: ProvFormat, model: String) -> Self {
        match format {
            ProvFormat::Claude => UpstreamSource::Claude(ClaudeToOpenaiStream::new()),
            ProvFormat::Gemini => UpstreamSource::Gemini(GeminiStreamState::default(), model, 0),
            _ => UpstreamSource::Openai,
        }
    }

    fn translate(&mut self, ev: &SseEvent, model: &str) -> Vec<Value> {
        match self {
            UpstreamSource::Openai => {
                if ev.data.trim() == DONE_MARKER {
                    Vec::new()
                } else {
                    serde_json::from_str::<Value>(&ev.data).ok().into_iter().collect()
                }
            }
            UpstreamSource::Claude(inner) => inner.translate(ev),
            UpstreamSource::Gemini(state, _model, created) => {
                let Ok(data) = serde_json::from_str::<Value>(&ev.data) else {
                    return Vec::new();
                };
                gemini_stream_to_openai_chunks(&data, model, "chatcmpl-gemini", *created, state)
            }
        }
    }
}

/// Canonical openai chunks → inbound wire frames.
enum InboundSink {
    Openai,
    Claude(OpenaiToClaudeStream),
    Responses(ResponsesStreamState, String),
    Completions,
}

impl InboundSink {
    fn new(inbound: Format, response_id: &str) -> Self {
        match inbound {
            Format::Claude => InboundSink::Claude(OpenaiToClaudeStream::new()),
            Format::OpenAIResponses => InboundSink::Responses(ResponsesStreamState::default(), response_id.to_string()),
            Format::Completions => InboundSink::Completions,
            _ => InboundSink::Openai,
        }
    }

    fn emit(&mut self, chunk: &Value) -> Vec<Bytes> {
        match self {
            InboundSink::Openai => vec![frame_data(&chunk.to_string())],
            InboundSink::Claude(conv) => conv
                .translate(chunk, false)
                .into_iter()
                .map(|(name, data)| frame_event(&name, &data.to_string()))
                .collect(),
            InboundSink::Responses(st, resp_id) => st
                .translate_chunk(chunk, false, resp_id)
                .into_iter()
                .map(|v| frame_event(v["type"].as_str().unwrap_or("event"), &v.to_string()))
                .collect(),
            InboundSink::Completions => vec![frame_data(&chat_chunk_to_completions(chunk).to_string())],
        }
    }

    fn finish(&mut self) -> Vec<Bytes> {
        match self {
            InboundSink::Openai => vec![frame_data(DONE_MARKER)],
            InboundSink::Claude(conv) => conv
                .translate(&json!({"choices": []}), true)
                .into_iter()
                .map(|(name, data)| frame_event(&name, &data.to_string()))
                .collect(),
            InboundSink::Responses(st, resp_id) => st
                .translate_chunk(&json!({}), true, resp_id)
                .into_iter()
                .map(|v| frame_event(v["type"].as_str().unwrap_or("event"), &v.to_string()))
                .collect(),
            InboundSink::Completions => vec![frame_data(DONE_MARKER)],
        }
    }
}

/// Pump upstream SSE → translated downstream SSE with keepalive heartbeats.
fn stream_response_pump(
    state: &Arc<AppState>,
    upstream: reqwest::Response,
    inbound: Format,
    upstream_format: ProvFormat,
    model: String,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(32);
    let idle = Duration::from_millis(state.config.stream_idle_timeout_ms);
    let heartbeat = Duration::from_millis(state.config.heartbeat_ms);
    let readiness = Duration::from_millis(state.config.readiness_timeout_ms);

    tokio::spawn(async move {
        let mut byte_stream = upstream.bytes_stream();
        let mut parser = SseParser::default();
        let mut up = UpstreamSource::new(upstream_format, model.clone());
        let mut down = InboundSink::new(inbound, &model);
        let mut got_first = false;

        loop {
            // Wait for upstream bytes. First byte honors the readiness
            // timeout; afterwards each gap honors the stream-idle timeout;
            // the heartbeat fires a keepalive comment when no bytes arrive.
            let gap = if got_first { idle } else { readiness };
            let next: Option<Result<Bytes, io::Error>> = tokio::select! {
                r = byte_stream.next() => match r {
                    None => None,
                    Some(Err(e)) => Some(Err(io::Error::other(e.to_string()))),
                    Some(Ok(b)) => Some(Ok(b)),
                },
                _ = tokio::time::sleep(heartbeat), if got_first => {
                    if tx.send(Ok(frame_keepalive())).await.is_err() {
                        return; // client gone
                    }
                    continue;
                }
                _ = tokio::time::sleep(gap) => None,
            };
            match next {
                None => break,
                Some(Ok(buf)) => {
                    for ev in parser.feed(&buf) {
                        if ev.data.trim() == DONE_MARKER {
                            break;
                        }
                        for chunk in up.translate(&ev, &model) {
                            for frame in down.emit(&chunk) {
                                if tx.send(Ok(frame)).await.is_err() {
                                    return; // client disconnected
                                }
                            }
                        }
                    }
                    got_first = true;
                }
                Some(Err(e)) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }

        // Always close the downstream stream in the inbound format:
        // openai → `data: [DONE]`, claude → trailing message_stop,
        // responses → response.completed.
        for frame in down.finish() {
            let _ = tx.send(Ok(frame)).await;
        }
    });

    axum::http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .unwrap()
}

/// Accumulate a forced upstream SSE stream into one chat.completion JSON
/// (parity: `forceStream` providers when the client wants JSON).
async fn accumulate_stream_json(
    state: &Arc<AppState>,
    upstream: reqwest::Response,
    upstream_format: ProvFormat,
    model: String,
) -> Result<Value, ApiError> {
    let mut byte_stream = upstream.bytes_stream();
    let mut parser = SseParser::default();
    let mut up = UpstreamSource::new(upstream_format, model.clone());
    let mut acc = CompletionAccumulator::default();
    let idle = Duration::from_millis(state.config.stream_idle_timeout_ms);

    loop {
        match tokio::time::timeout(idle, byte_stream.next()).await {
            Err(_) => return Err(ApiError::new(504, "stream idle timeout")),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(ApiError::new(502, format!("stream read error: {e}"))),
            Ok(Some(Ok(buf))) => {
                for ev in parser.feed(&buf) {
                    if ev.data.trim() == DONE_MARKER {
                        break;
                    }
                    for chunk in up.translate(&ev, &model) {
                        acc.fold(&chunk);
                    }
                }
            }
        }
    }
    Ok(acc.finish())
}

/// Fold canonical openai chunks into a single chat.completion JSON.
#[derive(Default, Debug)]
pub struct CompletionAccumulator {
    id: String,
    created: i64,
    model: String,
    content: String,
    role: Option<String>,
    tool_calls: BTreeMap<i64, (String, String, String)>, // index → (id, name, args)
    finish: Option<String>,
    usage: Option<Value>,
}

impl CompletionAccumulator {
    pub fn fold(&mut self, chunk: &Value) {
        if self.id.is_empty() {
            self.id = chunk.get("id").and_then(|i| i.as_str()).unwrap_or("chatcmpl-omniroute").to_string();
            self.created = chunk.get("created").and_then(|c| c.as_i64()).unwrap_or(0);
            self.model = chunk.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
        }
        if let Some(u) = chunk.get("usage") {
            if u.is_object() && !u.as_object().map(|m| m.is_empty()).unwrap_or(false) {
                self.usage = Some(u.clone());
            }
        }
        let Some(choice) = chunk.pointer("/choices/0") else { return };
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if self.role.is_none() {
            self.role = delta.get("role").and_then(|r| r.as_str()).map(str::to_string);
        }
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            self.content.push_str(c);
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let entry = self.tool_calls.entry(idx).or_insert_with(|| {
                    (
                        tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                        String::new(),
                        String::new(),
                    )
                });
                if let Some(name) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        entry.1 = name.to_string();
                    }
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                    entry.2.push_str(args);
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.finish = Some(f.to_string());
        }
    }

    pub fn finish(self) -> Value {
        let mut message = json!({"role": self.role.as_deref().unwrap_or("assistant")});
        if !self.content.is_empty() {
            message["content"] = json!(self.content);
        }
        if !self.tool_calls.is_empty() {
            let tcs: Vec<Value> = self
                .tool_calls
                .iter()
                .map(|(idx, (id, name, args))| {
                    json!({
                        "id": if id.is_empty() { json!(format!("call_{idx}")) } else { json!(id) },
                        "type": "function",
                        "function": {"name": name, "arguments": args},
                        "index": idx,
                    })
                })
                .collect();
            message["tool_calls"] = Value::Array(tcs);
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": self.finish.map(Value::from).unwrap_or(Value::Null)
            }],
            "usage": self.usage.unwrap_or(json!({}))
        })
    }
}

// ---------------------------------------------------------------------------
// response helpers
// ---------------------------------------------------------------------------

/// JSON response with standard headers.
pub fn json_response(v: Value) -> axum::response::Response {
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("x-accel-buffering", "no")
        .body(Body::from(v.to_string()))
        .unwrap()
}

/// SSE response from pre-rendered frames.
pub fn sse_response(frames: Vec<Bytes>) -> axum::response::Response {
    let s = stream::iter(frames.into_iter().map(Ok::<Bytes, io::Error>));
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(s))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_folds_stream() {
        let mut acc = CompletionAccumulator::default();
        acc.fold(&json!({
            "id": "c1", "created": 9, "model": "m",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hel"}, "finish_reason": null}]
        }));
        acc.fold(&json!({
            "id": "c1", "created": 9, "model": "m",
            "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": null}]
        }));
        acc.fold(&json!({
            "id": "c1", "created": 9, "model": "m",
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "t1", "function": {"name": "f", "arguments": "{\"x\":"}}]}, "finish_reason": null}]
        }));
        acc.fold(&json!({
            "id": "c1", "created": 9, "model": "m",
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}, "finish_reason": null}]
        }));
        acc.fold(&json!({
            "id": "c1", "created": 9, "model": "m",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        }));
        let out = acc.finish();
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["usage"]["prompt_tokens"], 4);
        assert_eq!(out["id"], "c1");
    }

    #[test]
    fn synthesize_from_openai_json() {
        let j = json!({
            "id": "chatcmpl-x", "created": 3, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "hey"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let chunks = synthesize_canonical_chunks(&j, "m");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hey");
        assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunks[2]["usage"]["prompt_tokens"], 1);
    }

    #[test]
    fn request_translation_openai_to_claude_sets_stream() {
        let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 12});
        let out = translate_request_body(Format::OpenAI, ProvFormat::Claude, &body, "claude-x".into(), true);
        assert_eq!(out["stream"], true);
        assert_eq!(out["max_tokens"], 12);
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn request_translation_openai_to_gemini() {
        let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
        let out = translate_request_body(Format::OpenAI, ProvFormat::Gemini, &body, "gemini-2.5-flash".into(), false);
        assert_eq!(out["contents"][0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn json_response_translation_passthrough_same_format() {
        let j = json!({"object": "chat.completion", "choices": []});
        let out = translate_json_response(ProvFormat::OpenAI, Format::OpenAI, &j, "m");
        assert_eq!(out, j);
    }
}
