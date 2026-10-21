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

    // The request's own size goes into candidate *resolution*, not just the
    // filter below: `auto/*` routers pick the best model a provider has that
    // can hold this request instead of the first one it lists.
    let request_fit = Some(combo::RequestFit {
        prompt_tokens: request_prompt_tokens(&req.body),
        output_tokens: request_output_tokens(&req.body),
    });
    let mut candidates = combo::resolve_candidates_with(&state, &req.model_str, request_fit);
    let resolved_any = !candidates.is_empty();
    let needs_image = request_contains_type(&req.body, &["image_url", "image", "input_image"]);
    let needs_pdf = request_contains_type(&req.body, &["file", "input_file", "document"]);
    if needs_image {
        candidates.retain(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_vision);
    }
    if needs_pdf {
        candidates.retain(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_pdf);
    }
    // The model exists but cannot take this attachment: say so. Reporting the
    // generic `model_not_found` (or a context-window error) misdescribes the
    // request and makes a client treat a fixable input as a missing model.
    if resolved_any && candidates.is_empty() {
        let modality = if needs_pdf && needs_image {
            "image or PDF input"
        } else if needs_pdf {
            "PDF input"
        } else {
            "image input"
        };
        let e = ApiError::new(
            400,
            format!(
                "model '{}' does not accept {modality} (code=unsupported_modality)",
                req.model_str
            ),
        );
        return e.into();
    }
    // Window fit is judged per candidate: the prompt must fit, and the output
    // reservation is clamped to that candidate's own ceiling. A client cap
    // larger than a candidate can emit must not disqualify that candidate.
    let prompt_context = request_prompt_tokens(&req.body);
    let requested_output = request_output_tokens(&req.body);
    let had_viable_candidates = !candidates.is_empty();
    let mut smallest_window: Option<i64> = None;
    let mut smallest_required: i64 = 0;
    if prompt_context > 0 || requested_output > 0 {
        candidates.retain(|candidate| {
            let caps = state.capabilities_for_model(&candidate.provider, &candidate.model);
            let required = combo::candidate_required_context_tokens(prompt_context, requested_output, caps.max_output_tokens);
            if caps.context_window >= required {
                true
            } else {
                smallest_window = Some(smallest_window.map_or(caps.context_window, |w: i64| w.max(caps.context_window)));
                smallest_required = smallest_required.max(required);
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
            let required = smallest_required.max(1);
            let e = ApiError::context_window_exceeded(required, window, &req.model_str);
            return e.into();
        }
        let e = ApiError::new(404, format!("model not found: {}", req.model_str));
        return e.into();
    }

    let started = Instant::now();
    let deadline = started + Duration::from_millis(crate::router::combo::COMBO_LOOP_SAFETY_TIMEOUT_MS);
    let mut attempts = 0usize;
    let mut last_err: Option<ApiError> = None;
    // Every upstream failure seen in this loop, used to pick the terminal
    // classification (see `terminal_error`).
    let mut errors: Vec<ApiError> = Vec::new();
    // Human-readable reasons for candidates that were never dispatched.
    let mut skipped: Vec<String> = Vec::new();
    // Candidates declined only because of a soft gate (provider cooldown /
    // open breaker / rate queue). They stay eligible for the half-open probe
    // pass below, so a transient breaker can no longer hard-fail a combo.
    let mut soft_skipped: Vec<(String, String)> = Vec::new();
    let combo_name = candidates.first().and_then(|c| c.combo.clone()).unwrap_or_default();
    let candidate_count = candidates.len();

    // pass 0 = normal dispatch; pass 1 = half-open probe (one candidate per
    // soft-skipped provider) run after every candidate of pass 0 failed.
    'passes: for pass in 0..2usize {
        let probe = pass == 1;
        // Half-open probing: at most one extra attempt per provider that the
        // normal pass declined. Without it a provider-wide cooldown / open
        // breaker silently deletes every one of its models from the fallback
        // chain for the whole cooldown window, which turns a single transient
        // upstream failure into "attempts: 0" hard failures for every combo
        // that happens to list that provider.
        let mut probed_providers: Vec<String> = Vec::new();
        for (index, cand) in candidates.iter().enumerate() {
            if attempts >= MAX_GLOBAL_ATTEMPTS || Instant::now() >= deadline {
                break 'passes;
            }
            let Some(entry) = state.registry.get(&cand.provider) else {
                last_err = Some(ApiError::new(500, format!("unknown provider '{}'", cand.provider)));
                continue;
            };
            if probe {
                // Hard gates still apply while probing: only candidates that the
                // normal pass skipped for a soft reason may be retried, and
                // only the first such candidate of each provider.
                if !soft_skipped.iter().any(|(p, m)| p == &cand.provider && m == &cand.model) {
                    continue;
                }
                if probed_providers.iter().any(|p| p == &cand.provider) {
                    continue;
                }
                probed_providers.push(cand.provider.clone());
            } else {
                if !state.circuits.is_available(&cand.provider) {
                    let wait = state.circuits.cooldown_remaining_ms(&cand.provider);
                    tracing::debug!(provider = %cand.provider, wait_ms = wait, "candidate skipped: circuit open / cooldown / concurrency");
                    skipped.push(format!("{}/{} (unavailable, retry in {}ms)", cand.provider, cand.model, wait));
                    soft_skipped.push((cand.provider.clone(), cand.model.clone()));
                    continue;
                }
                if state.circuits.is_model_banned(&cand.provider, &cand.model) {
                    tracing::debug!(provider = %cand.provider, model = %cand.model, "candidate skipped: model banned");
                    skipped.push(format!("{}/{} (model banned)", cand.provider, cand.model));
                    continue;
                }
                // Subscription quota discovered from the provider's own usage
                // endpoint: an exhausted rolling/weekly/monthly window sends the
                // combo to the next candidate instead of burning a request on a
                // guaranteed 429. A stale reading never gates.
                //
                // When the window published its reset time the tier provably
                // cannot serve before it, so the candidate is dropped hard —
                // the half-open probe would only spend one 429 per request
                // until then. A gate with no known reset stays soft-skipped, so
                // the probe can still escape it.
                if let Some(reset_ms) = crate::opencode_quota::gated(now_ms()) {
                    if cand.provider == "opencode-go" {
                        let known_reset = crate::opencode_quota::gate_has_known_reset(now_ms());
                        tracing::debug!(
                            provider = %cand.provider, reset_ms, known_reset,
                            "candidate skipped: subscription quota exhausted"
                        );
                        skipped.push(format!(
                            "{}/{} (subscription quota exhausted, resets in {}ms)",
                            cand.provider, cand.model, reset_ms
                        ));
                        if !known_reset {
                            soft_skipped.push((cand.provider.clone(), cand.model.clone()));
                        }
                        continue;
                    }
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
                        skipped.push(format!("{}/{} (rate queue wait exceeded)", cand.provider, cand.model));
                        soft_skipped.push((cand.provider.clone(), cand.model.clone()));
                        continue;
                    }
                }
            }
            attempts += 1;
            if probe {
                tracing::warn!(provider = %cand.provider, model = %cand.model, "half-open probe of soft-skipped candidate");
            }
            state.circuits.begin_request(&cand.provider);
            let result = try_candidate(&state, &req, cand, &entry, &AttemptContext { started, tokens_saved, compressed: compressed_flag }).await;
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
                        // An upstream 400 is an *upstream-side* rejection, not
                        // proof the caller's request is wrong: a free model may
                        // refuse a large body (`inference_failed`), a flapping
                        // registry may not know its own id, and the next
                        // candidate often serves the same request fine. Treat it
                        // as a model-level verdict (no provider cooldown) and
                        // keep walking the chain.
                        if crate::opencode_free::is_opencode_tier(&cand.provider) {
                            crate::opencode_free::record_verdict(&cand.model, &cand.provider, err.status, &err.message);
                        }
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model, status = err.status,
                            msg = %err.message, "candidate rejected the request; falling back"
                        );
                        errors.push(err.clone());
                        last_err = Some(err);
                        continue;
                    }
                    // A model-level answer from an OpenCode tier (this id is
                    // blocked in this country, unsupported, delisted) says
                    // nothing about the provider: cooling the whole provider
                    // would put every healthy sibling model in cooldown and
                    // make the Combo Studio show a tier-wide "cooling" state.
                    // The verdict is already learned per model by
                    // `opencode_free::record_verdict`.
                    let model_level = crate::opencode_free::is_opencode_tier(&cand.provider)
                        && crate::opencode_free::classify_failure(err.status, &err.message)
                            .is_some_and(|reason| reason.is_model_level());
                    if model_level {
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model, status = err.status,
                            "candidate rejected the model (no provider cooldown); falling back"
                        );
                    } else {
                        state
                            .circuits
                            .record_failure(&cand.provider, kind, entry.is_local, entry.auth_type);
                        tracing::warn!(provider = %cand.provider, status = err.status, msg = %err.message, "candidate failed; falling back");
                    }
                    errors.push(err.clone());
                    last_err = Some(err);
                }
            }
        }
    }

    // all candidates exhausted → combo diagnostics error (parity:
    // errorResponseWithComboDiagnostics)
    let terminal = terminal_error(&errors, last_err);
    let dispatched_anything = attempts > 0;
    // Nothing could even be dispatched: the honest upstream status is
    // "temporarily unavailable", not a bare bad gateway, and callers get a
    // Retry-After so they can wait for the cooldown instead of giving up.
    let status = terminal.as_ref().map(|e| e.status).unwrap_or(502);
    let status = if dispatched_anything { status } else { 503 };
    let mut message = format!(
        "All providers failed for model '{}' (combo '{}', attempts: {attempts}).",
        req.model_str,
        if combo_name.is_empty() { "-" } else { &combo_name }
    );
    if let Some(e) = &terminal {
        message.push_str(&format!(" Last error: [{} {}] {}", e.etype, e.code, e.message));
    }
    if !skipped.is_empty() {
        message.push_str(&format!(" Skipped {} candidate(s): {}.", skipped.len(), skipped.join("; ")));
    }
    let retry_after_ms = if dispatched_anything {
        0
    } else {
        let cooldown = candidates
            .iter()
            .map(|c| state.circuits.cooldown_remaining_ms(&c.provider))
            .max()
            .unwrap_or(0);
        // A subscription window that published its reset is the most precise
        // wait we can offer: tell the client when the tier comes back instead
        // of leaving it to retry blindly against a guaranteed 429.
        let quota_wait = if candidates.iter().any(|c| c.provider == "opencode-go") {
            crate::opencode_quota::gated(now_ms()).unwrap_or(0) as i64
        } else {
            0
        };
        cooldown.max(quota_wait)
    };
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
    let mut resp: axum::response::Response =
        ApiError { status, message, ..ApiError::new(status, "") }.into();
    if retry_after_ms > 0 {
        // Seconds-remaining hint so clients wait out a cooldown/open breaker
        // instead of treating the combo failure as permanent.
        let secs = ((retry_after_ms + 999) / 1000).max(1);
        if let Ok(hv) = axum::http::HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, hv);
        }
    }
    attach_compression_header(resp, compression_header_value)
}

/// Pick the terminal error for a failed combo.
///
/// The *last* upstream error is not necessarily the most useful one: a stray
/// 401 from a single tail candidate would otherwise be reported as a gateway
/// authentication failure (client UIs render that as "invalid API key"), while
/// the real cause — every provider out of quota — is hidden. Rank by operator
/// actionability, keeping the most recent error as the tie-breaker.
fn terminal_error(errors: &[ApiError], last: Option<ApiError>) -> Option<ApiError> {
    let rank = |e: &ApiError| match e.status {
        402 | 403 => 0u8,      // out of quota / payment required: operator action
        429 => 1,              // rate limited / quota window: wait
        401 => 2,              // credentials
        500..=504 => 3,        // upstream broken
        400 | 404 | 422 => 4,  // request-level
        _ => 5,
    };
    errors
        .iter()
        .min_by_key(|e| rank(e))
        .cloned()
        .or(last)
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

fn request_prompt_tokens(body: &Value) -> i64 {
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
    crate::compression::estimate_message_tokens(body).max(text_tokens(body, None))
}

/// Output tokens the client asked to reserve, in its own wire spelling.
fn request_output_tokens(body: &Value) -> i64 {
    body.get("max_completion_tokens")
        .or_else(|| body.get("max_output_tokens"))
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0)
}

/// Lower every output-cap field in place to `cap`, ignoring absent or smaller
/// values. Applied after translation so whichever spelling the upstream wire
/// uses (`max_tokens`, `max_completion_tokens`, ...) is covered.
fn clamp_output_tokens(body: &mut Value, cap: i64) {
    if cap <= 0 {
        return;
    }
    let Some(map) = body.as_object_mut() else { return };
    for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let Some(value) = map.get_mut(field) else { continue };
        if let Some(requested) = value.as_i64() {
            if requested > cap {
                *value = json!(cap);
            }
        }
    }
}

/// Execute one candidate end-to-end: translate → upstream → translate back.
struct AttemptContext {
    started: std::time::Instant,
    tokens_saved: i64,
    compressed: bool,
}

/// Is this candidate on the anonymous OpenCode Zen free lane?
///
/// The roster is discovered from the live Zen directory and models.dev cost
/// data (see `crate::opencode_free`), so this stays true after a rollout
/// removes a model — unlike a hardcoded suffix list.
fn is_opencode_free_lane(provider: &str, model: &str) -> bool {
    crate::opencode_free::is_free_lane_provider(provider)
        && crate::opencode_free::decide(model, provider).allowed
}

/// Replace (or append) one upstream header without duplicating it: the
/// executor seeds its own session id for the OpenCode tiers, and the free lane
/// needs the conversation-derived one instead.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    let lowered = name.to_ascii_lowercase();
    if let Some(slot) = headers
        .iter_mut()
        .find(|(key, _)| key.to_ascii_lowercase() == lowered)
    {
        slot.1 = value.to_string();
        return;
    }
    headers.push((name.to_string(), value.to_string()));
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
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
    let free_lane = is_opencode_free_lane(&cand.provider, &cand.model);
    // The anonymous lane only answers a streaming, agent-shaped body
    // (`stream: true` + the `bash`/`read` gate tools) — see
    // `crate::opencode_free::apply_lane_contract`.
    let upstream_stream = want_stream || wire_entry.force_stream || free_lane;

    let mut upstream_body = translate_request_body(req.inbound_format, wire_format, &req.body, cand.model.clone(), upstream_stream);
    // Never ask an upstream for more output than it advertises. The client cap
    // is already clamped this way inside the window-fit test, so clamping here
    // keeps the dispatched request consistent with the fit decision.
    clamp_output_tokens(
        &mut upstream_body,
        state.capabilities_for_model(&cand.provider, &cand.model).max_output_tokens,
    );
    if free_lane {
        crate::opencode_free::apply_lane_contract(&mut upstream_body, wire_format == ProvFormat::OpenAIResponses);
    }
    sanitize_opencode_go_request(&mut upstream_body, &cand.provider, wire_format);
    sanitize_non_stream_options(&mut upstream_body, wire_format);
    sanitize_developer_role(&mut upstream_body, &cand.provider, wire_format);
    // Managed dashboard connections participate in routing: their stored
    // key/base (overlay) win over static config; blanks fall through.
    //
    // Credential choice on the free lane is deliberate: a *keyed* call to a Zen
    // free model is gated by the workspace's "Allow free endpoints" privacy
    // setting and answers 400 `Free models may train on request data` until the
    // operator opts in (live-checked 2026-10-16), while the literal `public`
    // credential answers 200 for that same model. So the lane authenticates
    // anonymously by default even when the provider carries a key;
    // `OMNIROUTE_OPENCODE_FREE_USE_KEY=true` uses the configured key instead,
    // for workspaces that have enabled the opt-in.
    let configured_key = state.api_key_for(&cand.provider);
    // The lane's credential rule (key when opted in and accepted, else the
    // literal `public`) is shared with the dashboard probes so a "test" button
    // and a real request can never disagree.
    let key_override = if free_lane {
        crate::opencode_free::lane_credential(
            state.config.opencode_free_use_key,
            configured_key.clone(),
            now_ms(),
        )
    } else {
        configured_key.clone()
    };
    let (url, mut headers) = match build_upstream_request(
        &state.config,
        &state.registry,
        &wire_entry,
        &cand.provider,
        &cand.model,
        upstream_stream,
        key_override,
        state.base_url_for(&state.registry, &cand.provider),
    ) {
        Ok(v) => v,
        Err(e) => return TryResult::Next(e),
    };
    // The free tier moved off `/zen/v1` on 2026-10-14: the client takes its
    // base URL from the models.dev catalog (`https://opencode.ai/inference/v1`)
    // and the identical body is refused on `/zen/v1`. Paid models keep the
    // registry base; only the zero-cost lane is re-homed here.
    let rehomed = if free_lane {
        crate::opencode_free::inference_url(&url)
    } else {
        None
    };
    let url = match rehomed {
        Some(rehomed) => {
            tracing::debug!(url = %rehomed, "free-lane request re-homed onto the inference surface");
            rehomed
        }
        None => url,
    };
    if free_lane {
        // A canonical, conversation-stable `ses_` id: the lane rejects the
        // random shape with 403 FreeTierError, and groups both its budget and
        // the upstream prompt cache by this value.
        let seed = crate::opencode_free::conversation_seed(&req.body);
        let session = crate::opencode_free::session_for(&seed, now_ms());
        set_header(&mut headers, "x-opencode-session", &session);
        set_header(&mut headers, "x-session-affinity", &session);
        set_header(&mut headers, "X-Session-Id", &session);
        set_header(&mut headers, "x-opencode-client", "cli");
    }

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
        // Learn from the lane: a region block, a delisted id or a per-IP 429
        // is a statement about the model (or this egress), not a gateway bug,
        // so the roster drops it for a bounded window.
        crate::opencode_free::record_verdict(&cand.model, &cand.provider, status, &text);
        // An OpenCode tier's gate refusal is a statement about the lane, not
        // about this caller's credentials: forward it as a retryable
        // "lane unavailable" so the client does not read the upstream 401/403
        // class as a bad API key (see `ApiError::free_lane_unavailable`).
        if crate::opencode_free::is_opencode_tier(&cand.provider)
            && crate::opencode_free::is_lane_gate_refusal(status, &text)
        {
            return TryResult::Next(ApiError::free_lane_unavailable(&text));
        }
        let body_json = serde_json::from_str::<Value>(&text).ok();
        return TryResult::Next(crate::upstream::executor::upstream_error_with_text(
            status,
            body_json,
            Some(&text),
        ));
    }
    crate::opencode_free::record_verdict(&cand.model, &cand.provider, status, "");

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
                    // A folded stream can be as empty as a plain JSON answer.
                    // Nothing is downstream yet, so fall through instead of
                    // delivering a silent `0|0` turn.
                    if upstream_content_is_empty(&full) || !value_has_output(&full) {
                        if crate::opencode_free::is_opencode_tier(&cand.provider) {
                            crate::opencode_free::record_verdict(&cand.model, &cand.provider, 502, "empty content");
                        }
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model,
                            "upstream stream folded to empty content; falling back"
                        );
                        return TryResult::Next(ApiError::new(502, "upstream returned empty content"));
                    }
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
        // An upstream may answer 200 with an empty body. Forwarding it hands
        // the client a "success" with zero output, which agent UIs render as an
        // input/output `0|0` row while the turn silently produces nothing. Fail
        // the candidate so the combo walks on to a model that answers.
        if upstream_content_is_empty(&upstream_json) || !value_has_output(&upstream_json) {
            if crate::opencode_free::is_opencode_tier(&cand.provider) {
                crate::opencode_free::record_verdict(&cand.model, &cand.provider, 502, "empty content");
            }
            tracing::warn!(
                provider = %cand.provider, model = %cand.model,
                "upstream returned empty content; falling back"
            );
            return TryResult::Next(ApiError::new(502, "upstream returned empty content"));
        }
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

/// Does a complete response carry any actual output (text or tool calls)?
///
/// Covers the three wire shapes the gateway folds into: OpenAI chat
/// (`choices[].message`), Anthropic native (`content[].text` /
/// `content[].tool_use`) and the Responses API (`output[]`).
fn value_has_output(value: &Value) -> bool {
    if let Some(choices) = value.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            for holder in [choice.get("message"), choice.get("delta"), Some(choice)] {
                let Some(obj) = holder.and_then(|h| h.as_object()) else { continue };
                for key in ["content", "tool_calls", "text"] {
                    match obj.get(key) {
                        Some(Value::String(v)) if !v.is_empty() => return true,
                        Some(Value::Array(v)) if !v.is_empty() => return true,
                        _ => {}
                    }
                }
            }
        }
    }
    // Anthropic native: content is an array of blocks.
    if let Some(blocks) = value.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if block.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty()) {
                        return true;
                    }
                }
                Some("tool_use") => return true,
                _ => {
                    if block.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty()) {
                        return true;
                    }
                }
            }
        }
    }
    // Responses API: output is an array of items.
    if let Some(items) = value.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if item
                .get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        b.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty())
                    })
                })
            {
                return true;
            }
            if matches!(item.get("type").and_then(|t| t.as_str()), Some("function_call")) {
                return true;
            }
        }
    }
    false
}

/// Is this response an answer with no content at all?
///
/// A tool-calling turn legitimately has an empty `content` (the payload lives in
/// `tool_calls`), so tool calls always count as output. Callers combine this
/// with [`value_has_output`], which already refuses to call such a response
/// empty — keeping the check here as well makes the intent explicit and keeps
/// the two from drifting apart.
fn upstream_content_is_empty(value: &Value) -> bool {
    if let Some(choices) = value.get("choices").and_then(|c| c.as_array()) {
        if let Some(first) = choices.first() {
            let msg = first.get("message");
            let has_tools = msg
                .and_then(|m| m.get("tool_calls"))
                .and_then(|t| t.as_array())
                .is_some_and(|a| !a.is_empty());
            if has_tools {
                return false;
            }
            if let Some(content) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                return content.is_empty();
            }
        }
    }
    if let Some(arr) = value.get("content").and_then(|c| c.as_array()) {
        // Anthropic: a `tool_use` block is output even with empty text.
        if arr
            .iter()
            .any(|b| matches!(b.get("type").and_then(|t| t.as_str()), Some("tool_use")))
        {
            return false;
        }
        if let Some(text) = arr.first().and_then(|c| c.get("text")).and_then(|t| t.as_str()) {
            return text.is_empty();
        }
    }
    false
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
    fn candidate_output_reservation_clamps_to_candidate_ceiling() {
        // A client asking for 384k output must not disqualify a 200k-window
        // candidate whose own ceiling is 8k.
        assert_eq!(combo::candidate_output_tokens(384_000, 8_000), 8_000);
        assert_eq!(combo::candidate_output_tokens(4_096, 8_000), 4_096);
        // Unknown ceiling (0) keeps the client's request.
        assert_eq!(combo::candidate_output_tokens(384_000, 0), 384_000);
        assert_eq!(combo::candidate_required_context_tokens(10_000, 384_000, 8_000), 18_000);
        assert_eq!(combo::candidate_required_context_tokens(10_000, 384_000, 0), 394_000);
    }

    #[test]
    fn output_cap_fields_are_clamped_in_place() {
        let mut body = json!({"max_tokens": 384_000, "messages": []});
        clamp_output_tokens(&mut body, 131_072);
        assert_eq!(body["max_tokens"], 131_072);

        // Smaller client caps and unknown ceilings are left untouched.
        let mut small = json!({"max_completion_tokens": 900});
        clamp_output_tokens(&mut small, 131_072);
        assert_eq!(small["max_completion_tokens"], 900);

        let mut untouched = json!({"max_output_tokens": 500_000});
        clamp_output_tokens(&mut untouched, 0);
        assert_eq!(untouched["max_output_tokens"], 500_000);
    }

    #[test]
    fn request_output_tokens_reads_every_wire_spelling() {
        assert_eq!(request_output_tokens(&json!({"max_tokens": 12})), 12);
        assert_eq!(request_output_tokens(&json!({"max_completion_tokens": 34})), 34);
        assert_eq!(request_output_tokens(&json!({"max_output_tokens": 56})), 56);
        assert_eq!(request_output_tokens(&json!({"messages": []})), 0);
    }

    #[test]
    fn accumulator_folds_stream() {        let mut acc = CompletionAccumulator::default();
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
    fn opencode_free_lane_forces_stream_and_gate_tools() {
        let mut body = json!({"model": "mimo-v2.6-flash-free", "messages": []});
        crate::opencode_free::apply_lane_contract(&mut body, false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "none");
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn opencode_free_lane_membership_is_catalog_driven() {
        // The name rule covers a model whose metadata has not loaded yet...
        assert!(is_opencode_free_lane("opencode-zen", "mimo-v2.6-flash-free"));
        // ...and the verified bootstrap list covers an id without a `-free` name.
        assert!(is_opencode_free_lane("opencode", "big-pickle"));
        // The Go subscription tier and other providers are untouched.
        assert!(!is_opencode_free_lane("opencode-go", "glm-5.2"));
        assert!(!is_opencode_free_lane("openai", "gpt-5.2"));
        // A blocklisted id is never treated as a free-lane model.
        assert!(!is_opencode_free_lane("opencode-zen", "exo-free"));
    }

    #[test]
    fn opencode_free_lane_responses_shape_uses_flat_tools() {
        let mut body = json!({"model": "muse-spark-1.3-contributor-free", "input": []});
        crate::opencode_free::apply_lane_contract(&mut body, true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["name"], "bash");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"], "auto");
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

    #[test]
    fn terminal_error_prefers_quota_over_a_trailing_auth_failure() {
        // A combo whose real cause is exhausted quota must not be reported as
        // an authentication failure just because the last candidate 401'd.
        let errors = vec![
            ApiError::new(403, "Insufficient account balance"),
            ApiError::new(401, "invalid or missing API key"),
        ];
        let chosen = terminal_error(&errors, Some(ApiError::new(401, "tail 401"))).unwrap();
        assert_eq!(chosen.status, 403);
        assert!(chosen.message.contains("Insufficient account balance"));
    }

    #[test]
    fn terminal_error_keeps_auth_when_it_is_the_only_cause() {
        let errors = vec![ApiError::new(401, "bad key")];
        assert_eq!(terminal_error(&errors, None).unwrap().status, 401);
    }

    #[test]
    fn terminal_error_prefers_rate_limit_over_upstream_5xx() {
        let errors = vec![ApiError::new(500, "boom"), ApiError::new(429, "slow down")];
        assert_eq!(terminal_error(&errors, None).unwrap().status, 429);
    }

    #[test]
    fn terminal_error_prefers_rate_limit_over_a_parked_free_lane() {
        // A parked zero-cost lane is reported as 503; a real rate limit from the
        // one working candidate is the more actionable answer, and neither may
        // be mistaken for an authentication failure.
        let errors = vec![
            ApiError::free_lane_unavailable(r#"{"error":{"type":"FreeTierError"}}"#),
            ApiError::new(429, "slow down"),
        ];
        let chosen = terminal_error(&errors, None).unwrap();
        assert_eq!(chosen.status, 429);
        let only_lane = vec![ApiError::free_lane_unavailable(r#"{"error":{"type":"FreeTierError"}}"#)];
        let chosen = terminal_error(&only_lane, None).unwrap();
        assert_eq!(chosen.status, 503);
        assert_eq!(chosen.code, "free_lane_unavailable");
    }

    #[test]
    fn terminal_error_falls_back_to_last_when_nothing_was_attempted() {
        // attempts == 0 => no upstream error collected; the last known error
        // (e.g. an unknown provider) still has to be reported, and an empty
        // set yields None so the caller can pick 502/503.
        let errors: Vec<ApiError> = Vec::new();
        let chosen = terminal_error(&errors, Some(ApiError::new(500, "unknown provider 'x'"))).unwrap();
        assert_eq!(chosen.status, 500);
        assert!(terminal_error(&errors, None).is_none());
    }
}
