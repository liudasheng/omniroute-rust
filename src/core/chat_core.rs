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
use crate::translate::responses::{chat_response_to_responses, responses_request_to_chat, responses_response_to_chat, ResponsesStreamState, ResponsesToOpenaiStream};
use crate::translate::stream::{ClaudeToOpenaiStream, OpenaiToClaudeStream};
use crate::upstream::executor::build_upstream_request;
use axum::body::Body;
use axum::http::HeaderMap;
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
    pub endpoint: String,
    pub client_ip: Option<String>,
    pub reasoning_effort: Option<String>,
    /// `x-omniroute-compression` request header value (per-request override)
    pub compression_header: Option<String>,
}

pub fn client_ip_from_headers(headers: &HeaderMap, remote: std::net::SocketAddr) -> Option<String> {
    for name in ["x-forwarded-for", "x-real-ip", "cf-connecting-ip"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(ip) = value.split(',').next().map(str::trim).filter(|v| !v.is_empty()) {
                return Some(ip.to_string());
            }
        }
    }
    Some(remote.ip().to_string())
}

fn reasoning_effort_from_body(body: &Value) -> Option<String> {
    body.get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .or_else(|| body.pointer("/thinking/effort").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| body.get("reasoning_budget").and_then(Value::as_i64).map(|v| format!("budget:{v}")))
}

/// Result of one candidate attempt.
enum TryResult {
    /// fully-formed downstream reply, with the usage the upstream reported
    /// (None ⇒ the response is a live SSE stream: the pump logs when it ends)
    Responded(axum::response::Response, Option<crate::state::TokenUsage>),
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
    if from == ProvFormat::OpenAIResponses && to == Format::OpenAIResponses {
        return resp.clone();
    }
    // normalize upstream → openai chat completion, then re-shape for inbound
    let openai_json = match from {
        ProvFormat::OpenAI => resp.clone(),
        ProvFormat::OpenAIResponses => responses_response_to_chat(resp, model),
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
        Some("response") => responses_response_to_chat(upstream_json, model),
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
pub async fn handle_chat(state: Arc<AppState>, mut req: ChatRequest) -> axum::response::Response {
    req.reasoning_effort = req.reasoning_effort.take().or_else(|| reasoning_effort_from_body(&req.body));
    // gateway-wide custom system prompt (Endpoints page toggle): injected only
    // when the caller did not supply a system message of its own.
    if let Some(prompt) = state.custom_system_prompt() {
        if req.inbound_format == Format::OpenAI {
            let empty = Vec::new();
            let messages = req.body.get("messages").and_then(|m| m.as_array()).unwrap_or(&empty);
            let has_system = messages
                .iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"));
            if !has_system {
                if let Some(arr) = req.body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                    arr.insert(0, json!({"role": "system", "content": prompt}));
                }
            }
        }
    }
    apply_thinking_policy(&mut req.body, &state.config.thinking_mode, state.config.thinking_budget);

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
        endpoint: req.endpoint,
        client_ip: req.client_ip,
        reasoning_effort: req.reasoning_effort,
        compression_header: req.compression_header,
    };

    let mut candidates = combo::resolve_candidates(&state, &req.model_str);
    let needs_image = request_contains_type(&req.body, &["image_url", "image", "input_image"]);
    let needs_pdf = request_contains_type(&req.body, &["file", "input_file", "document"]);
    if needs_image {
        candidates.retain(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_vision);
    }
    if needs_pdf {
        candidates.retain(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_pdf);
    }
    let required_context = request_required_context_tokens(&req.body);
    let had_viable_candidates = !candidates.is_empty();
    let mut smallest_window: Option<i64> = None;
    if required_context > 0 {
        candidates.retain(|candidate| {
            let window = state.capabilities_for_model(&candidate.provider, &candidate.model).context_window;
            if window >= required_context {
                true
            } else {
                smallest_window = Some(smallest_window.map_or(window, |w: i64| w.max(window)));
                false
            }
        });
    }
    if candidates.is_empty() {
        // A model that exists but cannot hold this request is a context-size
        // failure, not an unknown model: reporting 404 `model_not_found` makes
        // agent clients give up, while a context-length error lets a client
        // that owns compaction shrink the conversation and retry.
        if had_viable_candidates {
            let window = smallest_window.unwrap_or(0);
            let e = ApiError::context_window_exceeded(required_context, window, &req.model_str);
            return e.into();
        }
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
        let result = try_candidate(&state, &req, &cand, &entry, &AttemptContext { started, tokens_saved, compressed: compressed_flag }).await;
        state.circuits.end_request(&cand.provider);
        match result {
            TryResult::Responded(resp, usage) => {
                state.circuits.record_success(&cand.provider);
                // A live SSE stream reports its usage only at the end, so the pump
                // writes that entry itself (usage == None).
                if let Some(u) = usage {
                    state.log_request(crate::state::RequestLogEntry {
                        ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
                        model: req.model_str.clone(),
                        provider: Some(cand.provider.clone()),
                        endpoint: req.endpoint.clone(),
                        reasoning_effort: req.reasoning_effort.clone(),
                        client_ip: req.client_ip.clone(),
                        status: 200,
                        latency_ms: started.elapsed().as_millis() as u64,
                        tokens_saved,
                        compressed: compressed_flag,
                        prompt_tokens: u.prompt,
                        completion_tokens: u.completion,
                        stream: false,
                    });
                }
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
        endpoint: req.endpoint.clone(),
        reasoning_effort: req.reasoning_effort.clone(),
        client_ip: req.client_ip.clone(),
        status,
        latency_ms: started.elapsed().as_millis() as u64,
        tokens_saved,
        compressed: compressed_flag,
        prompt_tokens: 0,
        completion_tokens: 0,
        stream: req.stream,
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

/// Apply the gateway-wide reasoning policy before format translation.
fn apply_thinking_policy(body: &mut Value, mode: &str, budget: Option<i64>) {
    let mode = mode.trim().to_ascii_lowercase();
    if mode == "passthrough" || mode.is_empty() {
        return;
    }
    let Some(obj) = body.as_object_mut() else { return };
    if matches!(mode.as_str(), "auto" | "adaptive") {
        for key in ["reasoning", "reasoning_effort", "thinking", "thinking_config", "enable_thinking"] {
            obj.remove(key);
        }
        if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if let Some(message) = message.as_object_mut() {
                    message.remove("reasoning_content");
                    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                        content.retain(|block| block.get("type").and_then(Value::as_str) != Some("thinking"));
                    }
                }
            }
        }
    }
    if mode == "custom" {
        if let Some(budget) = budget.filter(|v| *v > 0) {
            obj.insert("reasoning_budget".into(), json!(budget));
            obj.entry("reasoning_effort").or_insert_with(|| json!("medium"));
        }
    }
}

fn request_contains_type(body: &Value, types: &[&str]) -> bool {
    fn contains_requested_type(value: &Value, types: &[&str]) -> bool {
        match value {
            Value::Array(values) => values.iter().any(|v| contains_requested_type(v, types)),
            Value::Object(map) => {
                map.get("type").and_then(Value::as_str).is_some_and(|t| types.contains(&t))
                    || map.values().any(|v| contains_requested_type(v, types))
            }
            _ => false,
        }
    }
    contains_requested_type(body.get("messages").unwrap_or(body), types)
}

fn request_required_context_tokens(body: &Value) -> i64 {
    fn text_tokens(value: &Value, key: Option<&str>) -> i64 {
        match value {
            Value::String(text) if matches!(key, Some("content" | "text" | "input" | "instructions")) => {
                crate::compression::estimate::estimate_tokens(text)
            }
            Value::Array(values) => values.iter().map(|v| text_tokens(v, key)).sum(),
            Value::Object(map) => map.iter().map(|(k, v)| text_tokens(v, Some(k))).sum(),
            _ => 0,
        }
    }
    let prompt = crate::compression::estimate_message_tokens(body).max(text_tokens(body, None));
    let output = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    prompt.saturating_add(output)
}

/// Execute one candidate end-to-end: translate → upstream → translate back.
struct AttemptContext {
    started: std::time::Instant,
    tokens_saved: i64,
    compressed: bool,
}

fn is_opencode_zen_free_model(provider: &str, model: &str) -> bool {
    if !matches!(provider, "opencode" | "opencode-zen") {
        return false;
    }
    model.ends_with("-free")
        || matches!(
            model,
            "big-pickle"
                | "deepseek-v4-flash-free"
                | "mimo-v2.5-free"
                | "hy3-free"
                | "nemotron-3-ultra-free"
                | "north-mini-code-free"
        )
}

/// OpenCode Zen free models require stream=true and at least one tool even
/// when the client did not request either. The upstream contract checks the
/// shape, not whether the placeholder is called, so use the same `_noop`
/// function shape as the original executor.
fn apply_opencode_zen_free_contract(
    body: &mut Value,
    provider: &str,
    model: &str,
    wire_format: ProvFormat,
) -> bool {
    if !is_opencode_zen_free_model(provider, model) {
        return false;
    }
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    object.insert("stream".into(), json!(true));
    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if !has_tools {
        let tool = if wire_format == ProvFormat::OpenAIResponses {
            json!({
                "type": "function",
                "name": "_noop",
                "description": "Do not call this tool. It exists only for API compatibility.",
                "parameters": {"type": "object", "properties": {}}
            })
        } else {
            json!({
                "type": "function",
                "function": {
                    "name": "_noop",
                    "description": "Do not call this tool. It exists only for API compatibility.",
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        };
        object.insert("tools".into(), json!([tool]));
    }
    true
}

fn sanitize_opencode_go_request(body: &mut Value, provider: &str, wire_format: ProvFormat) {
    if provider != "opencode-go" || wire_format != ProvFormat::OpenAI {
        return;
    }
    let Some(object) = body.as_object_mut() else { return };
    // DSH and other modern clients send the cross-provider reasoning object.
    // OpenCode Go's Chat Completions schema rejects that key; its native
    // DeepSeek-compatible field is the flat reasoning_effort string.
    if let Some(reasoning) = object.remove("reasoning") {
        if object.get("reasoning_effort").is_none() {
            if let Some(effort) = reasoning
                .get("effort")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
            {
                object.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }
    // These are Claude/Gemini-side aliases, not fields in OpenCode Go's
    // OpenAI-compatible request schema.
    object.remove("thinking");
    object.remove("thinking_config");
    object.remove("enable_thinking");
}

fn sanitize_non_stream_options(body: &mut Value, wire_format: ProvFormat) {
    if wire_format != ProvFormat::OpenAI {
        return;
    }
    let Some(object) = body.as_object_mut() else { return };
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        object.remove("stream_options");
    }
}

fn sanitize_developer_role(body: &mut Value, provider: &str, wire_format: ProvFormat) {
    if wire_format != ProvFormat::OpenAI {
        return;
    }
    let provider_id = provider.trim().to_ascii_lowercase();
    let preserves_developer = matches!(provider_id.as_str(), "openai" | "azure" | "azure-openai" | "github")
        || provider_id.contains("openai");
    if preserves_developer {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    for message in messages {
        if message.get("role").and_then(Value::as_str).is_some_and(|role| role.eq_ignore_ascii_case("developer")) {
            message["role"] = json!("system");
        }
    }
}

async fn try_candidate(
    state: &Arc<AppState>,
    req: &ChatRequest,
    cand: &combo::Candidate,
    entry: &RegistryEntry,
    ctx: &AttemptContext,
) -> TryResult {
    let wire_format = state.format_for_model(&cand.provider, &cand.model);
    let mut wire_entry = entry.clone();
    wire_entry.format = wire_format;
    let want_stream = req.stream;
    let free_contract = is_opencode_zen_free_model(&cand.provider, &cand.model);
    let upstream_stream = want_stream || wire_entry.force_stream || free_contract;

    let mut upstream_body = translate_request_body(req.inbound_format, wire_format, &req.body, cand.model.clone(), upstream_stream);
    apply_opencode_zen_free_contract(&mut upstream_body, &cand.provider, &cand.model, wire_format);
    sanitize_opencode_go_request(&mut upstream_body, &cand.provider, wire_format);
    sanitize_non_stream_options(&mut upstream_body, wire_format);
    sanitize_developer_role(&mut upstream_body, &cand.provider, wire_format);
    // Managed dashboard connections participate in routing: their stored
    // key/base (overlay) win over static config; blanks fall through.
    let (url, headers) = match build_upstream_request(
        &state.config,
        &state.registry,
        &wire_entry,
        &cand.provider,
        &cand.model,
        upstream_stream,
        state.api_key_for(&cand.provider),
        state.base_url_for(&state.registry, &cand.provider),
    ) {
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
        let text = resp.text().await.unwrap_or_default();
        let body_json = serde_json::from_str::<Value>(&text).ok();
        return TryResult::Next(crate::upstream::executor::upstream_error_with_text(
            status,
            body_json,
            Some(&text),
        ));
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
            TryResult::Responded(
                stream_response_pump(state, resp, req.inbound_format, wire_format, cand.model.clone(), cand.provider.clone(), req.model_str.clone(), ctx.started, ctx.tokens_saved, ctx.compressed, req.endpoint.clone(), req.reasoning_effort.clone(), req.client_ip.clone()),
                None,
            )
        } else {
            // forceStream provider (kimi-style): fold upstream SSE into JSON
            match accumulate_stream_json(state, resp, wire_format, cand.model.clone()).await {
                Ok(full) => {
                    let u = crate::state::usage_from_value(&full);
                    TryResult::Responded(json_response(translate_json_response(wire_format, req.inbound_format, &full, &cand.model)), Some(u))
                }
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
            {
                let u = crate::state::usage_from_value(&upstream_json);
                TryResult::Responded(sse_response(frames), Some(u))
            }
        } else {
            {
                let u = crate::state::usage_from_value(&upstream_json);
                TryResult::Responded(json_response(translate_json_response(wire_format, req.inbound_format, &upstream_json, &cand.model)), Some(u))
            }
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
    Responses(ResponsesToOpenaiStream),
}

impl UpstreamSource {
    fn new(format: ProvFormat, model: String) -> Self {
        match format {
            ProvFormat::Claude => UpstreamSource::Claude(ClaudeToOpenaiStream::new()),
            ProvFormat::Gemini => UpstreamSource::Gemini(GeminiStreamState::default(), model, 0),
            ProvFormat::OpenAIResponses => UpstreamSource::Responses(ResponsesToOpenaiStream::default()),
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
            UpstreamSource::Responses(inner) => inner.translate(ev),
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
#[allow(clippy::too_many_arguments)]
fn stream_response_pump(
    state: &Arc<AppState>,
    upstream: reqwest::Response,
    inbound: Format,
    upstream_format: ProvFormat,
    model: String,
    provider: String,
    request_model: String,
    started: std::time::Instant,
    tokens_saved: i64,
    compressed: bool,
    endpoint: String,
    reasoning_effort: Option<String>,
    client_ip: Option<String>,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(32);
    let idle = Duration::from_millis(state.config.stream_idle_timeout_ms);
    let heartbeat = Duration::from_millis(state.config.heartbeat_ms);
    let readiness = Duration::from_millis(state.config.readiness_timeout_ms);

    let state_for_log = state.clone();
    tokio::spawn(async move {
        let mut byte_stream = upstream.bytes_stream();
        let mut parser = SseParser::default();
        let mut up = UpstreamSource::new(upstream_format, model.clone());
        let mut down = InboundSink::new(inbound, &model);
        let mut got_first = false;
        let mut usage = crate::state::TokenUsage::default();
        let upstream_status: u16 = 200;

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
                            let u = crate::state::usage_from_value(&chunk);
                            if !u.is_zero() {
                                usage = u;
                            }
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

        // Streams report usage only on their final chunk, so the entry is written
        // here — after completion — instead of in the dispatcher.
        state_for_log.log_request(crate::state::RequestLogEntry {
            ts_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            model: request_model,
            provider: Some(provider),
            endpoint,
            reasoning_effort,
            client_ip,
            status: upstream_status,
            latency_ms: started.elapsed().as_millis() as u64,
            tokens_saved,
            compressed,
            prompt_tokens: usage.prompt,
            completion_tokens: usage.completion,
            stream: true,
        });
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
    fn thinking_policy_auto_removes_client_reasoning() {
        let mut body = json!({
            "reasoning_effort": "high",
            "messages": [{"role": "assistant", "reasoning_content": "hidden", "content": [
                {"type": "thinking", "thinking": "hidden"}, {"type": "text", "text": "answer"}
            ]}]
        });
        apply_thinking_policy(&mut body, "auto", None);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body["messages"][0].get("reasoning_content").is_none());
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn thinking_policy_custom_adds_budget() {
        let mut body = json!({"messages": []});
        apply_thinking_policy(&mut body, "custom", Some(2048));
        assert_eq!(body["reasoning_budget"], 2048);
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn json_response_translation_passthrough_same_format() {
        let j = json!({"object": "chat.completion", "choices": []});
        let out = translate_json_response(ProvFormat::OpenAI, Format::OpenAI, &j, "m");
        assert_eq!(out, j);
    }

    #[test]
    fn opencode_zen_free_contract_adds_stream_and_placeholder_tool() {
        let mut body = json!({"model": "deepseek-v4-flash-free", "messages": []});
        assert!(apply_opencode_zen_free_contract(
            &mut body,
            "opencode-zen",
            "deepseek-v4-flash-free",
            ProvFormat::OpenAI
        ));
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["function"]["name"], "_noop");
    }

    #[test]
    fn opencode_go_does_not_receive_the_zen_free_contract() {
        let mut body = json!({"model": "glm-5.2", "messages": []});
        assert!(!apply_opencode_zen_free_contract(
            &mut body,
            "opencode-go",
            "glm-5.2",
            ProvFormat::OpenAI
        ));
        assert!(body.get("stream").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn opencode_zen_responses_free_contract_uses_flat_tools() {
        let mut body = json!({"model": "muse-spark-1.3-contributor-free", "input": []});
        assert!(apply_opencode_zen_free_contract(
            &mut body,
            "opencode-zen",
            "muse-spark-1.3-contributor-free",
            ProvFormat::OpenAIResponses
        ));
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["name"], "_noop");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn opencode_go_chat_maps_reasoning_object_to_native_effort() {
        let mut body = json!({
            "model": "deepseek-v4-flash",
            "reasoning": {"effort": "high"},
            "thinking": {"enabled": true},
            "messages": []
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn reasoning_object_is_preserved_for_responses_wire_format() {
        let mut body = json!({"reasoning": {"effort": "high"}});
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAIResponses);
        assert!(body.get("reasoning").is_some());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn non_stream_openai_requests_drop_stream_options() {
        let mut body = json!({"stream": false, "stream_options": {"include_usage": true}});
        sanitize_non_stream_options(&mut body, ProvFormat::OpenAI);
        assert!(body.get("stream_options").is_none());

        let mut body = json!({"stream": true, "stream_options": {"include_usage": true}});
        sanitize_non_stream_options(&mut body, ProvFormat::OpenAI);
        assert!(body.get("stream_options").is_some());
    }

    #[test]
    fn non_openai_compatible_providers_map_developer_to_system() {
        let mut body = json!({"messages": [{"role": "developer", "content": "rules"}]});
        sanitize_developer_role(&mut body, "qwen-cloud-token-plan", ProvFormat::OpenAI);
        assert_eq!(body["messages"][0]["role"], "system");

        let mut body = json!({"messages": [{"role": "developer", "content": "rules"}]});
        sanitize_developer_role(&mut body, "openai", ProvFormat::OpenAI);
        assert_eq!(body["messages"][0]["role"], "developer");

        let mut body = json!({"messages": [{"role": "developer", "content": "rules"}]});
        sanitize_developer_role(&mut body, "qwen-cloud-token-plan", ProvFormat::OpenAIResponses);
        assert_eq!(body["messages"][0]["role"], "developer");
    }
}
