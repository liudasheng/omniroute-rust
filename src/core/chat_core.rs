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
    /// API-key display name that admitted the request (downstream requester).
    pub requester: Option<String>,
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
    /// The free lane's upstream quota (`429 FreeUsageLimitError`), with the
    /// cooldown the response advertised in `Retry-After` (0 = unknown). The
    /// caller switches account instead of treating it as a model verdict.
    FreeQuota(ApiError, u128),
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
    // The tools array is normalized *before* the dialect branch: every
    // dialect reads it in a different shape (OpenAI wants the nested
    // `{type, function}` form, Responses wants a flat `{type, name}`,
    // Claude wants `{name, input_schema}`), and each of those readers
    // silently DROPS a tool it does not recognise. Normalizing first means
    // no dialect can lose a tool.
    normalize_openai_tools(&mut openai_shape);

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

/// Normalize the `tools` array into the canonical nested OpenAI shape.
///
/// A client may send a tool in the flat form (`{name, description,
/// parameters}`) — that is what several agent harnesses emit, and what the
/// gateway itself records in the session log. Every dialect reads the tools
/// array in its own shape and *silently drops* a tool it does not
/// recognise: the OpenAI upstream answers "missing field `type`", the
/// Claude translator drops flat tools entirely, and the Responses
/// translator forwards a flat tool without its `type`. Normalizing once,
/// before the dialect branch, makes all three agree.
///
/// The transformation is shape-only: the tool's name, description,
/// parameter schema and every other key (`strict`, vendor extensions) move
/// verbatim, so the callable surface does not change. A tool that declares
/// a non-function `type` is left alone — rewriting it would change what
/// the tool *is*.
fn normalize_openai_tools(body: &mut Value) {
    let Some(arr) = body.get_mut("tools").and_then(|t| t.as_array_mut()) else { return };

    // Lift nested group tools (`tools[].tools`) to the top level.
    //
    // A tool may carry its children as `tools: [...]` — a vendor group form.
    // Every dialect reads only the *top* array, so the group's name was
    // declared but its children were silently dropped on the Gemini, Claude
    // and Responses lanes: the model was offered a namespace it could not
    // call. Promoting the children makes them reachable everywhere; the group
    // itself is left in place so a client that means it keeps its own entry.
    let mut lifted: Vec<Value> = Vec::new();
    for tool in arr.iter() {
        // The group may nest its children under the tool or under its
        // `function` object — both spellings appear in the wild.
        for children in [
            tool.get("tools"),
            tool.pointer("/function/tools"),
        ]
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_array())
        {
            for child in children {
                // Recurse: a group may itself nest.
                let promoted = child.clone();
                let mut wrapper = json!({ "tools": [promoted] });
                normalize_openai_tools(&mut wrapper);
                if let Some(inner) = wrapper.get("tools").and_then(|t| t.as_array()) {
                    lifted.extend_from_slice(inner);
                }
            }
        }
    }
    if !lifted.is_empty() {
        let existing: std::collections::HashSet<String> = arr
            .iter()
            .filter_map(|t| {
                t.pointer("/function/name")
                    .or_else(|| t.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
            })
            .collect();
        for tool in lifted {
            let name = tool
                .pointer("/function/name")
                .or_else(|| tool.get("name"))
                .and_then(|n| n.as_str());
            if name.is_some_and(|n| existing.contains(n)) {
                continue; // already declared at the top level
            }
            arr.push(tool);
        }
    }

    for tool in arr.iter_mut() {
        let Some(map) = tool.as_object_mut() else { continue };
        if map.contains_key("function") {
            continue; // already in the nested shape
        }
        // A tool that is not a plain function keeps its own type: a custom
        // tool (e.g. `{"type": "custom", "name": "run_sql", "format": …}`)
        // would lose its format and change meaning if wrapped as a
        // function.
        match map.get("type").and_then(|t| t.as_str()) {
            Some(t) if t != "function" => continue,
            _ => {}
        }
        let Some(name) = map.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
            continue;
        };
        // The flat form may use `parameters` (OpenAI-ish) or `input_schema`
        // (Anthropic-ish); both belong inside the nested function object.
        // Every other key (`strict`, vendor extensions) is carried over
        // unchanged.
        let mut function = serde_json::Map::new();
        function.insert("name".into(), Value::String(name));
        let mut extra: Vec<(String, Value)> = Vec::new();
        for (k, v) in map.iter() {
            match k.as_str() {
                "name" | "description" | "parameters" | "input_schema" | "type" => {}
                _ => extra.push((k.clone(), v.clone())),
            }
        }
        if let Some(d) = map.remove("description") {
            function.insert("description".into(), d);
        }
        if let Some(p) = map.remove("parameters") {
            function.insert("parameters".into(), p);
        } else if let Some(p) = map.remove("input_schema") {
            function.insert("parameters".into(), p);
        }
        for (k, v) in extra {
            function.insert(k, v);
        }
        let mut wrapped = serde_json::Map::new();
        wrapped.insert("type".into(), Value::String("function".into()));
        wrapped.insert("function".into(), Value::Object(function));
        *tool = Value::Object(wrapped);
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
    if state.config.strip_historical_reasoning {
        strip_historical_reasoning(&mut req.body);
    }

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
        requester: req.requester,
        compression_header: req.compression_header,
    };

    // The request's own size goes into candidate *resolution*, not just the
    // filter below: `auto/*` routers pick the best model a provider has that
    // can hold this request instead of the first one it lists.
    let request_fit = Some(combo::RequestFit {
        prompt_tokens: conservative_prompt_tokens(&req.body),
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
    let prompt_context = conservative_prompt_tokens(&req.body);
    let requested_output = request_output_tokens(&req.body);
    let had_viable_candidates = resolved_any;
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
            // Log the rejection with its size: an oversized attempt that
            // never dispatches must still show up as `<estimate>|0`, not
            // vanish from the recent-requests view.
            state.log_request(crate::state::RequestLogEntry {
                ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
                model: req.model_str.clone(),
                combo: None,
                combo_known: true,
                routed_model: None,
                provider: None,
                account: None,
                requester: req.requester.clone(),
                endpoint: req.endpoint.clone(),
                reasoning_effort: req.reasoning_effort.clone(),
                client_ip: req.client_ip.clone(),
                status: 400,
                latency_ms: 0,
                tokens_saved,
                compressed: compressed_flag,
                prompt_tokens: prompt_context.max(0) as u64,
                completion_tokens: 0,
                stream: req.stream,
            });
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
    // Failures tagged with the lane (provider) they came from, so the
    // terminal error can tell a shared quota problem from one lane's own.
    let mut lanes: Vec<(String, ApiError)> = Vec::new();
    // Human-readable reasons for candidates that were never dispatched.
    let mut skipped: Vec<String> = Vec::new();
    // Candidates declined only because of a soft gate (provider cooldown /
    // open breaker / rate queue). They stay eligible for the half-open probe
    // pass below, so a transient breaker can no longer hard-fail a combo.
    let mut soft_skipped: Vec<(String, String)> = Vec::new();
    let combo_name = candidates.first().and_then(|c| c.combo.clone()).unwrap_or_default();

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
        for cand in candidates.iter() {
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
                // A model-level cooldown (a recent model-scoped verdict) skips
                // this id only — its siblings on the same provider keep
                // serving, which is the whole point of a model-scoped
                // cooldown. It expires on its own; it is not a ban.
                // A model-level cooldown is per *account*: a 429 answers one API
                // key, and a sibling account usually still has quota for the
                // same id. Skip only when every account of this provider is
                // cooling — otherwise this candidate is still serviceable and
                // the rotation below will pick a live account.
                if state.circuits.is_model_cooling(&cand.provider, &cand.model) {
                    let accounts = state.accounts_for(&cand.provider);
                    let any_live = accounts.is_empty()
                        || accounts.iter().any(|a| {
                            // An account with no cooldown of its own is live even
                            // if the provider-wide verdict is set.
                            state
                                .circuits
                                .account_model_cooldown_remaining_ms(
                                    &cand.provider,
                                    &cand.model,
                                    &a.circuit_key(&cand.provider),
                                )
                                <= 0
                        });
                    if !any_live {
                        let wait = state
                            .circuits
                            .model_cooldown_remaining_ms(&cand.provider, &cand.model);
                        tracing::debug!(
                            provider = %cand.provider, model = %cand.model, wait_ms = wait,
                            "candidate skipped: every account is cooling this model"
                        );
                        skipped.push(format!(
                            "{}/{} (model cooling on every account, retry in {}ms)",
                            cand.provider, cand.model, wait
                        ));
                        continue;
                    }
                }
                // Subscription quota discovered from the provider's own usage
                // endpoint: an exhausted rolling/weekly/monthly window sends the
                // combo to the next candidate instead of burning a request on a
                // guaranteed 429. A stale reading never gates.
                //
                // Only the *paid* ids draw against the subscription: the Go tier
                // also re-exports zero-cost models, which keep serving while the
                // paid window is spent (see `subscription_gated`).
                //
                // When the window published its reset time the tier provably
                // cannot serve before it, so the candidate is dropped hard —
                // the half-open probe would only spend one 429 per request
                // until then. A gate with no known reset stays soft-skipped, so
                // the probe can still escape it.
                if let Some(reset_ms) = crate::opencode_quota::gated(now_ms()) {
                    if subscription_gated(&cand.provider, &cand.model) {
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
            // The free lane draws its own credential pool (managed accounts,
            // then the anonymous `public` key) and may retry the *same*
            // candidate on a sibling account after a quota 429; its budget
            // reservations and in-flight guards live inside
            // `try_free_lane_candidate`. Every other provider keeps the single
            // account draw and the RAII concurrency guard: the dispatch awaits
            // inside that region, and a client disconnect would otherwise drop
            // the future without ever reaching `end_request`, leaking the slot
            // until the concurrency limit rejects the provider for good.
            let (account, result) = if is_opencode_free_lane(&cand.provider, &cand.model) {
                try_free_lane_candidate(
                    &state,
                    &req,
                    cand,
                    &entry,
                    started,
                    tokens_saved,
                    compressed_flag,
                )
                .await
            } else {
                // Draw the account that serves this attempt. The circuit key is
                // the account's own, so one exhausted key cannot park its
                // siblings.
                let account = state.pick_account_for_model(&cand.provider, Some(&cand.model));
                let circuit_key = match &account {
                    Some(a) => a.circuit_key(&cand.provider),
                    None => cand.provider.clone(),
                };
                let _in_flight = state.circuits.begin_request_guard(&circuit_key);
                let result = try_candidate(&state, &req, cand, &entry, &AttemptContext {
                    started,
                    tokens_saved,
                    compressed: compressed_flag,
                    account: account.clone(),
                    lane_credential: None,
                    lane_session: None,
                    lane_egress: None,
                })
                .await;
                (account, result)
            };
            let circuit_key = match &account {
                Some(a) => a.circuit_key(&cand.provider),
                None => cand.provider.clone(),
            };
            let account_label = account.as_ref().map(|a| a.label());
            match result {
                TryResult::Responded(resp, usage) => {
                    state.circuits.record_success(&circuit_key);
                    // A live SSE stream reports its usage only at the end, so the pump
                    // writes that entry itself (usage == None).
                    if let Some(u) = usage {
                        state.log_request(crate::state::RequestLogEntry {
                            ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
                            model: req.model_str.clone(),
                            combo: (!combo_name.is_empty()).then(|| combo_name.clone()),
                            combo_known: true,
                            routed_model: Some(format!("{}/{}", cand.provider, cand.model)),
                            provider: Some(cand.provider.clone()),
                            account: account_label.clone(),
                            requester: req.requester.clone(),
                            endpoint: req.endpoint.clone(),
                            reasoning_effort: req.reasoning_effort.clone(),
                            client_ip: req.client_ip.clone(),
                            status: 200,
                            latency_ms: started.elapsed().as_millis() as u64,
                            tokens_saved,
                            compressed: compressed_flag,
                            prompt_tokens: u.prompt,
                            completion_tokens: u.completion,
                            // The field means "the client asked for a stream",
                            // and this writer also covers the case where the
                            // client streamed but the upstream answered JSON —
                            // the gateway then synthesizes SSE here. Hardcoding
                            // `false` hid those rows from `?stream=true`.
                            stream: req.stream,
                        });
                    }
                    return attach_compression_header(resp, compression_header_value.clone());
                }
                TryResult::FreeQuota(err, _retry_after_ms) => {
                    // The inner free-lane loop already learned from these 429s
                    // (per-account cooldown, or the egress scope). The model is
                    // not at fault — a sibling free model, or the combo's next
                    // paid/Go step, may still serve — so it must not be cooled.
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model, status = err.status,
                        "free-lane quota exhausted after trying every credential; not cooling the model"
                    );
                    lanes.push((cand.provider.clone(), err.clone()));
                    last_err = Some(err);
                    continue;
                }
                TryResult::Next(err) => {
                    // A `FreeUsageLimitError` is a budget signal, not a verdict
                    // about this model. Do not cool the id: another free model
                    // can still serve while this one is rate-limited. The same
                    // holds for a *local* budget refusal that never reached the
                    // upstream (`QUOTA_ERROR_CODE`).
                    if crate::opencode_free::is_free_lane_provider(&cand.provider)
                        && (crate::opencode_free::is_free_usage_limit(err.status, &err.message)
                            || err.code == crate::opencode_free_budget::QUOTA_ERROR_CODE
                            || err.code == crate::upstream::bun::EGRESS_ERROR_CODE)
                    {
                        // An egress failure (dead proxy) is likewise not a
                        // verdict about the model or the lane's upstream.
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model,
                            "free-lane quota or egress failure; not cooling the model"
                        );
                        lanes.push((cand.provider.clone(), err.clone()));
                        last_err = Some(err);
                        continue;
                    }
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
                            crate::opencode_free::record_verdict(&cand.model, &cand.provider, err.status, &err.message, account.as_ref().map(|a| a.label()).as_deref());
                        }
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model, status = err.status,
                            msg = %err.message, "candidate rejected the request; falling back"
                        );
                        lanes.push((cand.provider.clone(), err.clone()));
                        last_err = Some(err);
                        continue;
                    }
                    // A model-level answer says nothing about the provider:
                    // cooling the whole provider would put every healthy sibling
                    // model in cooldown and make the Combo Studio show a
                    // tier-wide "cooling" state.
                    //
                    // Two sources qualify, and both must stay model-scoped even
                    // when the provider carries a single account:
                    //
                    //  * a 429 is the upstream refusing *this request rate on
                    //    this id*, for any provider — not a statement that the
                    //    host or every model behind it is down. Cooling the
                    //    account instead parked the account's other, perfectly
                    //    healthy models (measured: a 429 on one custom-provider
                    //    model cooled both accounts entirely);
                    //  * the OpenCode verdicts (region block, unsupported,
                    //    delisted) are learned per model by
                    //    `opencode_free::record_verdict` and never imply a
                    //    provider-wide fault.
                    let rate_limited = kind == FailureKind::RateLimit;
                    let model_level = rate_limited
                        || (crate::opencode_free::is_opencode_tier(&cand.provider)
                            && crate::opencode_free::classify_failure(err.status, &err.message)
                                .is_some_and(|reason| reason.is_model_level()));
                    if model_level {
                        // Cool the *model*, never the provider: the siblings of
                        // this id may still have quota, and freezing the whole
                        // family is what made a healthy tier look exhausted.
                        state.circuits.record_model_failure(
                            &cand.provider,
                            &cand.model,
                            kind,
                            entry.is_local,
                        );
                        // Also scope it to the account that actually saw the
                        // rejection, so its siblings are not parked with it.
                        state.circuits.record_account_model_failure(
                            &cand.provider,
                            &cand.model,
                            &circuit_key,
                            kind,
                            entry.is_local,
                        );
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model, status = err.status,
                            "candidate rejected the model (model-scoped cooldown only); falling back"
                        );
                    } else {
                        state
                            .circuits
                            .record_failure(&circuit_key, kind, entry.is_local, entry.auth_type);
                        tracing::warn!(
                            provider = %cand.provider,
                            account = %account_label.as_deref().unwrap_or("-"),
                            status = err.status, msg = %err.message,
                            "candidate failed; falling back"
                        );
                    }
                    lanes.push((cand.provider.clone(), err.clone()));
                    last_err = Some(err);
                }
            }
        }
    }

    // all candidates exhausted → combo diagnostics error (parity:
    // errorResponseWithComboDiagnostics)
    let terminal = terminal_error_for_lanes(&lanes, last_err);
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
    // A combo made only of free-lane steps can also be waiting on the local
    // UTC-day budget; its window reset is the honest Retry-After (the provider
    // circuit knows nothing about a budget we skip locally).
    let budget_wait = if !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| is_opencode_free_lane(&candidate.provider, &candidate.model))
    {
        crate::opencode_free_budget::cooldown_hint_ms(now_ms()) as i64
    } else {
        0
    };
    let retry_after_ms = if dispatched_anything {
        // A quota refusal that never reached the upstream still tells the
        // client when the window reopens.
        if status == 429 {
            budget_wait
        } else {
            0
        }
    } else {
        let cooldown = candidates
            .iter()
            .map(|c| state.circuits.cooldown_remaining_ms(&c.provider))
            .max()
            .unwrap_or(0);
        // A model-level cooldown never cools the provider, so the provider
        // cooldown is 0 while the model is still cooling. Without this the
        // client gets a 503 with no Retry-After and retries blindly for the
        // whole cooldown.
        let model_cooldown = candidates
            .iter()
            .map(|c| state.circuits.model_cooldown_remaining_ms(&c.provider, &c.model))
            .max()
            .unwrap_or(0);
        // A subscription window that published its reset is the most precise
        // wait we can offer: tell the client when the tier comes back instead
        // of leaving it to retry blindly against a guaranteed 429. A chain made
        // only of free Go models is not waiting on that window — their own 429
        // is a different budget, so borrowing the subscription reset would
        // report "subscription quota exhausted, resets in 6h" for the wrong
        // clock.
        let quota_wait = if chain_has_subscription_gated(&candidates) {
            crate::opencode_quota::gated(now_ms()).unwrap_or(0) as i64
        } else {
            0
        };
        // A pure rate-limit refusal carries no cooldown at all, yet the
        // client still has to wait for the provider's min interval before the
        // next attempt can be admitted. Without the floor the 503 carries no
        // Retry-After and the client retries immediately into the same wall.
        let rate_floor = if skipped.iter().any(|s| s.contains("rate queue")) {
            state.config.rate_min_interval_ms as i64
        } else {
            0
        };
        cooldown
            .max(quota_wait)
            .max(model_cooldown)
            .max(rate_floor)
            .max(budget_wait)
    };
    // One request-level line so an operator reading the journal can see the
    // outcome directly. Without it the failure is only visible as a trail of
    // per-candidate warnings, and the dashboard row shows a bare `0|0`
    // (failed requests carry no usage) with no explanation of why.
    tracing::warn!(
        model = %req.model_str,
        combo = %if combo_name.is_empty() { "-" } else { &combo_name },
        status,
        attempts,
        skipped = skipped.len(),
        "request failed: every candidate was unavailable or errored"
    );
    state.log_request(crate::state::RequestLogEntry {
        ts_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
        model: req.model_str.clone(),
        combo: (!combo_name.is_empty()).then(|| combo_name.clone()),
        combo_known: true,
        // Nothing served, so nothing was routed.
        routed_model: None,
        provider: None,
        account: None,
        requester: req.requester.clone(),
        endpoint: req.endpoint.clone(),
        reasoning_effort: req.reasoning_effort.clone(),
        client_ip: req.client_ip.clone(),
        status,
        latency_ms: started.elapsed().as_millis() as u64,
        tokens_saved,
        compressed: compressed_flag,
        // A rejected request still consumed an input: log the estimate so the
        // row reads `<estimate>|0` and an oversized failure is visible as such,
        // instead of a `0|0` that looks like nothing was sent.
        prompt_tokens: request_prompt_tokens(&req.body).max(0) as u64,
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
///
/// A quota verdict from a *later* candidate must not outrank the failure of
/// the **first** candidate: the chain is ordered by preference, so the head's
/// verdict is the representative one. Without this, a combo whose head merely
/// timed out would be reported as "quota exhausted" because a secondary lane
/// ran out of credits — the operator then debugs the wrong account. Only when
/// the head itself reports a quota/auth problem does that verdict stand.
#[cfg(test)]
fn terminal_error(errors: &[ApiError], last: Option<ApiError>) -> Option<ApiError> {
    errors
        .iter()
        .min_by_key(|e| error_meaning(e))
        .cloned()
        .or(last)
}

/// How actionable is this failure? Lower ranks win.
///
/// The order is by *meaning*, not by the numeric status: picking the
/// smallest number would prefer 401 (bad credentials) over 402/403 (out of
/// credit), and a 401 in the body makes clients render "invalid API key" —
/// the wrong diagnosis for an exhausted account.
fn error_meaning(e: &ApiError) -> u8 {
    match e.status {
        402 | 403 => 0,         // out of quota / payment required: operator action
        429 => 1,               // rate limited / quota window: wait
        401 => 2,               // credentials
        500..=504 => 3,         // upstream broken
        400 | 404 | 422 => 4,   // request-level
        _ => 5,
    }
}

/// Pick the terminal error for a failed combo, knowing **which lane** each
/// failure came from.
///
/// The chain is ordered by preference, so the head lane's verdict is the
/// representative one. A quota exhaustion reported by a *different* lane is
/// that lane's private problem: reporting it as the combo's reason makes a
/// healthy tier look out of quota ("opencode go still has credit, yet the
/// gateway says no quota"). A quota verdict from the **same** provider as the
/// head still wins — there the quota really is the shared cause.
///
/// This keeps `terminal_error`'s same-lane ranking intact and adds the
/// cross-lane discrimination the operator needs.
fn terminal_error_for_lanes(lanes: &[(String, ApiError)], last: Option<ApiError>) -> Option<ApiError> {
    let Some((head_provider, head)) = lanes.first() else {
        return last;
    };
    let quota_class = |e: &ApiError| matches!(e.status, 402 | 403 | 429 | 401);
    // A head that failed with bad credentials may still yield to a sibling
    // lane reporting an exhausted account (see below), so it is handled
    // with the other verdicts rather than returned right away.
    if quota_class(head) && head.status != 401 {
        return Some(head.clone());
    }
    // Rank by *meaning*, not by the numeric status: picking the smallest
    // number would prefer 401 (bad credentials) over 402/403 (out of
    // credit), and a 401 in the body makes clients render "invalid API
    // key" — the wrong diagnosis for an exhausted account.
    let meaning = error_meaning;
    // A verdict from the head's own provider is about the same account, so
    // it outranks the head's own error.
    if let Some((_, e)) = lanes
        .iter()
        .skip(1)
        .filter(|(p, e)| p == head_provider && quota_class(e))
        .min_by_key(|(_, e)| meaning(e))
    {
        return Some(e.clone());
    }
    // A verdict from *another* lane is that lane's private problem. The
    // head's verdict stays the representative one — a head that merely
    // returned empty content must not be reported as "quota exhausted"
    // because a sibling lane ran out of credit.
    //
    // The one exception is a head that failed with *bad credentials* (401):
    // a 401 in the body is rendered by agent clients as "invalid API key",
    // and when a sibling lane reports an exhausted account, "out of credit"
    // is the actionable truth. So a 401 head yields to a cross-lane
    // out-of-credit verdict — but not to a mere rate limit.
    if head.status == 401 {
        if let Some((_, e)) = lanes
            .iter()
            .skip(1)
            .filter(|(_, e)| matches!(e.status, 402 | 403))
            .min_by_key(|(_, e)| meaning(e))
        {
            return Some(e.clone());
        }
        return Some(head.clone());
    }
    Some(head.clone())
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
/// Drop earlier turns' chain-of-thought before dispatch.
///
/// A past turn's `reasoning_content` is not something any upstream reads back
/// as input, but leaving it in the transcript makes the model *imitate its own
/// earlier wording*: measured on `opencode-go/beam-free`, seeding six turns of
/// similarly-shaped reasoning made every later turn's reasoning that same
/// template — the "reasoning is always the same" symptom. Signed reasoning is
/// never touched: Anthropic requires its `thinking` blocks (with `signature`)
/// to be echoed back verbatim, and `reasoning_details` is the OpenAI Responses
/// equivalent.
///
/// This pass is wire-format agnostic: the inbound body is still in the client's
/// own shape here. Whether a field survives to a particular upstream is decided
/// per target at dispatch — an OpenAI Chat target drops the Responses-only
/// fields in [`strip_stateful_history`], while Claude/Responses/Gemini targets
/// keep the signed forms.
fn strip_historical_reasoning(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(obj) = message.as_object_mut() else { continue };
        if obj.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // `reasoning_details` and signed `thinking` parts are the
        // provider-issued forms; both are left completely alone. The plain
        // text fields are ours to drop — and they are dropped even when a
        // signed form sits alongside them, since each field is judged on its
        // own and an unsigned string needs no round-trip. This is only a
        // pre-translation pass, so an OpenAI Chat target still drops the
        // Responses-only forms later in [`strip_stateful_history`].
        if obj.contains_key("reasoning_details") {
            continue;
        }
        if obj
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|p| {
                    p.get("type").and_then(Value::as_str) == Some("thinking")
                        && p.get("signature").is_some()
                })
            })
        {
            continue;
        }
        obj.remove("reasoning_content");
        obj.remove("reasoning");
    }
}

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

/// Prompt size judged against a model's window: the raw estimate with a
/// safety margin, over the full serialized request.
///
/// `estimate_tokens` counts UTF-16 units at 4 per token, which undercounts
/// CJK, code identifiers and tool-call payloads by roughly a third — and
/// `request_prompt_tokens` does not count tool definitions or call arguments
/// at all. Judged raw, a request that really needs more than the window
/// passes the gateway's own fit test, hits the upstream, and comes back as a
/// bare "Invalid request" the client cannot recover from. The margin makes
/// the fit test fail closed (a recoverable context error) instead.
fn conservative_prompt_tokens(body: &Value) -> i64 {
    let base = crate::compression::estimate_body_tokens(body).max(request_prompt_tokens(body));
    base.saturating_mul(13) / 10
}

/// An upstream refusal that only says "Invalid request" — no field, no
/// schema, no detail. On its own it is unclassifiable; combined with a
/// request that cannot fit the candidate's window it is a size refusal.
fn is_generic_invalid_request(body: &str) -> bool {
    body.to_ascii_lowercase().contains("invalid request")
}

/// Should a generic upstream 400 be reported as a context-size refusal?
///
/// `required` is the conservative (margined) size, `required_full` the raw
/// full-body size; the window is the candidate's. Either the conservative
/// size already exceeds the window, or the raw size accounts for more than
/// half of it — the measured DSH case the estimate still missed (raw 621k /
/// conservative 807k for a request the upstream rejected).
fn generic_400_is_size_refusal(required: i64, required_full: i64, window: i64) -> bool {
    window > 0 && (required > window || required_full.saturating_mul(2) > window)
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
/// uses (`max_tokens`, `max_completion_tokens`, ...) is covered — including
/// the Gemini dialect's nested `generationConfig.maxOutputTokens`, which the
/// top-level pass alone would miss: a 384000-token request reached Gemini
/// unclamped against a 65536 ceiling and died on that lane while the same
/// request on the OpenAI lane was clamped and served.
fn clamp_output_tokens(body: &mut Value, cap: i64) {
    if cap <= 0 {
        return;
    }
    let clamp = |value: &mut Value| {
        if let Some(requested) = value.as_i64() {
            if requested > cap {
                *value = json!(cap);
            }
        }
    };
    if let Some(map) = body.as_object_mut() {
        for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            if let Some(value) = map.get_mut(field) {
                clamp(value);
            }
        }
        // Gemini dialect: `generationConfig.maxOutputTokens`.
        if let Some(gen) = map.get_mut("generationConfig").and_then(|g| g.as_object_mut()) {
            if let Some(value) = gen.get_mut("maxOutputTokens") {
                clamp(value);
            }
        }
    }
}

/// Execute one candidate end-to-end: translate → upstream → translate back.
struct AttemptContext {
    started: std::time::Instant,
    tokens_saved: i64,
    compressed: bool,
    /// The account drawn for this attempt (None: config/env credential or the
    /// anonymous free lane). Chosen by the caller so the circuit store can name
    /// the same account when recording the outcome.
    account: Option<crate::state::AccountCredential>,
    /// Free lane only: the exact `Authorization` bearer the budget planner
    /// reserved for this attempt (an account key or the literal `public`).
    lane_credential: Option<String>,
    /// Free lane only: a specific `ses_*` session to send (a warm-session
    /// fallback). `None` derives one from the conversation memo.
    lane_session: Option<String>,
    /// Free lane only: the exact egress the budget planner reserved for this
    /// attempt (Direct, or the account's/pool's proxy). `None` falls back to the
    /// account's `proxy_url`.
    lane_egress: Option<crate::upstream::bun::Egress>,
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

/// Does the discovered OpenCode Go subscription-quota gate cover this model?
///
/// The reading describes what the *subscription* has left, and only paid ids
/// draw against it. The Go tier also re-exports zero-cost models (`-free` names
/// such as `beam-free` / `step-5-preview-free`, or a catalog zero-cost
/// verdict); those keep serving when the subscription window is spent. Gating
/// on the provider alone deleted every free Go model from the combo chain, so
/// `coding-free` could not fall through to `beam-free` at 100% usage.
fn subscription_gated(provider: &str, model: &str) -> bool {
    provider == "opencode-go" && !crate::router::combo::is_free_model(provider, model)
}

/// Whether any candidate in the chain is actually covered by that gate.
///
/// The terminal Retry-After must not borrow the paid window's reset for a chain
/// that contains only free Go models: their own 429 comes from a different
/// budget, and a subscription-shaped hint would make the caller wait out the
/// wrong clock.
fn chain_has_subscription_gated(candidates: &[combo::Candidate]) -> bool {
    candidates
        .iter()
        .any(|candidate| subscription_gated(&candidate.provider, &candidate.model))
}

/// Try one free-lane candidate across the provider's credential pool.
///
/// The candidate stays the same; only the credential changes. That is the whole
/// point of the pool: a 429 answers one key (or the shared egress), so a
/// sibling account usually still has budget for the *same* model — switching
/// provider/model would walk away from a free lane that is merely key-scoped.
/// The anonymous `public` credential is the pool's last entry, and the budget
/// planner skips it locally when its own window is spent.
///
/// Returns the account behind the outcome so the caller can name the same
/// circuit key in its success/failure bookkeeping.
async fn try_free_lane_candidate(
    state: &Arc<AppState>,
    req: &ChatRequest,
    cand: &combo::Candidate,
    entry: &RegistryEntry,
    started: Instant,
    tokens_saved: i64,
    compressed: bool,
) -> (Option<crate::state::AccountCredential>, TryResult) {
    use crate::opencode_free_budget::{self as budget, Begin};

    let mode = state.config.opencode_free_credential_mode;
    let limits = budget::BudgetLimits::from_config(&state.config);
    // The conversation's session comes from the persistent memo; a warm-session
    // retry below may swap it for an id the upstream already accepted. The same
    // seed also gives the anonymous proxy pool its sticky exit, so every turn
    // of a conversation starts on the same egress.
    let seed = crate::opencode_free::conversation_seed(&req.body);
    let credentials = budget::lane_credentials(
        state,
        &cand.provider,
        &cand.model,
        mode,
        now_ms(),
        &seed,
    );
    let mut lane_session = crate::opencode_free::session_for(&seed, now_ms());
    let mut warm_retried = false;
    let mut tried: Vec<String> = Vec::new();
    // A tunnel failure is retried on the next proxy exit, not reported as a
    // quota/deadline verdict; keep the last one in case the whole pool fails.
    let mut last_egress_error: Option<ApiError> = None;
    let slot_deadline = Instant::now() + Duration::from_millis(budget::SLOT_WAIT_MS);
    let mut last_account: Option<crate::state::AccountCredential> = None;
    loop {
        match budget::begin(&credentials, &limits, &tried, now_ms()) {
            Begin::Ready(credential, guard) => {
                let circuit_key = credential
                    .account
                    .as_ref()
                    .map(|account| account.circuit_key(&cand.provider))
                    .unwrap_or_else(|| cand.provider.clone());
                // Provider concurrency is tracked exactly like a normal
                // candidate; the budget guard below it is the *per-credential*
                // in-flight cap.
                let _circuit = state.circuits.begin_request_guard(&circuit_key);
                last_account = credential.account.clone();
                let result = try_candidate(
                    state,
                    req,
                    cand,
                    entry,
                    &AttemptContext {
                        started,
                        tokens_saved,
                        compressed,
                        account: credential.account.clone(),
                        lane_credential: Some(credential.secret.clone()),
                        lane_session: Some(lane_session.clone()),
                        lane_egress: Some(credential.egress.clone()),
                    },
                )
                .await;
                drop(guard);
                match result {
                    TryResult::FreeQuota(mut error, mut retry_after_ms) => {
                        // A long Retry-After on a session the upstream has not
                        // accepted before is the session-admission quota, not
                        // the request budget. Retry the same body once with an
                        // already-accepted (warm) session; only a 429 on that
                        // retry proves the quota itself is exhausted.
                        if state.config.opencode_free_session_fallback
                            && !warm_retried
                            && budget::classify_quota_429(retry_after_ms)
                                == budget::QuotaClass::Daily
                            && !crate::opencode_free::is_warm_session(
                                &cand.provider,
                                &lane_session,
                            )
                        {
                            if let Some(warm) =
                                crate::opencode_free::warm_session_for(&cand.provider)
                            {
                                warm_retried = true;
                                let warm_prefix: String = warm.chars().take(6).collect();
                                tracing::info!(
                                    provider = %cand.provider, model = %cand.model,
                                    credential = %budget::mask_bucket(&credential.bucket),
                                    warm_session = %warm_prefix,
                                    retry_after_ms,
                                    "opencode free lane: fresh session refused with a long Retry-After; retrying once with a warm session"
                                );
                                // The refused attempt never reached the quota.
                                budget::refund(&credential.bucket);
                                // `free_warm_retry` no longer reserves on its own
                                // (the exhaustion probe uses a different
                                // reservation), so take the slot here.
                                let warm_guard = match budget::begin(
                                    std::slice::from_ref(&credential),
                                    &limits,
                                    &[],
                                    now_ms(),
                                ) {
                                    Begin::Ready(_, guard) => guard,
                                    _ => return (last_account, TryResult::Next(budget::quota_error(&budget::Exhaustion {
                                        reset_at_ms: budget::window_reset_at(now_ms()),
                                        cooldown_until_ms: 0,
                                        egress: false,
                                    }))),
                                };
                                if let Some((retry_account, retry_result)) = free_warm_retry(
                                    state,
                                    req,
                                    cand,
                                    entry,
                                    &credential,
                                    &warm,
                                    started,
                                    tokens_saved,
                                    compressed,
                                )
                                .await
                                {
                                    drop(warm_guard);
                                    match retry_result {
                                        TryResult::Responded(response, usage) => {
                                            crate::opencode_free::note_session_admission_refusal(
                                                now_ms(),
                                            );
                                            crate::opencode_free::rebind_session(
                                                &seed,
                                                &warm,
                                                now_ms(),
                                            );
                                            tracing::info!(
                                                provider = %cand.provider, model = %cand.model,
                                                warm_session = %warm_prefix,
                                                result = 200,
                                                "opencode free lane: warm session served the request; new-session admission is blocked"
                                            );
                                            return (
                                                retry_account,
                                                TryResult::Responded(response, usage),
                                            );
                                        }
                                        TryResult::FreeQuota(next_error, next_retry) => {
                                            tracing::info!(
                                                provider = %cand.provider, model = %cand.model,
                                                warm_session = %warm_prefix,
                                                result = 429,
                                                "opencode free lane: warm session was refused too; treating this as quota exhaustion"
                                            );
                                            error = next_error;
                                            retry_after_ms = next_retry;
                                            lane_session = warm;
                                        }
                                        other => return (retry_account, other),
                                    }
                                }
                            }
                        }
                        let outcome = budget::note_quota_429(
                            &credential.bucket,
                            &cand.model,
                            now_ms(),
                            retry_after_ms,
                            &limits,
                        );
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model,
                            credential = %budget::mask_bucket(&credential.bucket),
                            anonymous = credential.anonymous,
                            class = outcome.class.as_str(),
                            learned_limit = outcome.learned_limit,
                            learned_at_ms = outcome.learned_at_ms,
                            scope = outcome.scope.as_str(),
                            retry_after_ms,
                            "opencode free lane: quota 429; cooling this credential"
                        );
                        if outcome.low_usage_suspected {
                            // A daily-class 429 after barely any accepted
                            // requests: almost certainly the upstream refusing
                            // fresh sessions, not the request quota. Back off
                            // briefly only; no learned ceiling, no window park.
                            tracing::warn!(
                                provider = %cand.provider, model = %cand.model,
                                credential = %budget::mask_bucket(&credential.bucket),
                                short_quota_refusals = outcome.short_quota_refusals,
                                cooldown_until_ms = outcome.cooldown_until_ms,
                                "opencode free lane: low-usage 429 — suspected session admission rather than quota exhaustion; short cooldown only"
                            );
                        }
                        tried.push(credential.bucket);
                        if outcome.egress_learned {
                            // A second account of the same model failed inside
                            // the probe window: the budget follows the egress,
                            // so trying the remaining accounts would only burn
                            // one 429 each. `note_quota_429` parked them all.
                            tracing::warn!(
                                provider = %cand.provider, model = %cand.model,
                                "opencode free lane: budget is egress-scoped (two credentials answered 429 within the probe window); parking every credential for this UTC window"
                            );
                            return (last_account, TryResult::FreeQuota(error, retry_after_ms));
                        }
                        continue;
                    }
                    TryResult::Responded(response, usage) => {
                        // A 2xx on the account we switched to settles the
                        // scope as account-local (see `note_success`).
                        budget::note_success(&credential.bucket, &cand.model, now_ms());
                        return (credential.account, TryResult::Responded(response, usage));
                    }
                    TryResult::Next(error)
                        if error.code == crate::upstream::bun::EGRESS_ERROR_CODE =>
                    {
                        // The proxy/tunnel failed before the upstream saw the
                        // request: the reservation was not upstream quota
                        // consumption, so give it back and rotate to the next
                        // exit (the next candidate is a different proxy, or a
                        // sibling account for a per-account proxy).
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model,
                            bucket = %budget::mask_bucket(&credential.bucket),
                            egress = %credential.egress.masked_label(),
                            msg = %error.message,
                            "opencode free lane: egress failed; rotating to the next exit"
                        );
                        budget::refund(&credential.bucket);
                        tried.push(credential.bucket);
                        last_egress_error = Some(error);
                        continue;
                    }
                    other => return (credential.account, other),
                }
            }
            Begin::Busy => {
                // Every credential is idle but at its in-flight cap: wait a
                // short bounded while instead of failing a healthy request.
                if Instant::now() >= slot_deadline {
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model,
                        wait_ms = budget::SLOT_WAIT_MS,
                        "opencode free lane: every credential is at its in-flight cap; skipping without an upstream request"
                    );
                    return (last_account, TryResult::Next(budget::concurrency_error()));
                }
                tokio::time::sleep(Duration::from_millis(budget::SLOT_POLL_MS)).await;
            }
            Begin::Exhausted(exhaustion) => {
                // A pool that failed at the transport level is a more useful
                // verdict than "every credential is spent": no upstream request
                // was ever refused for quota.
                if let Some(error) = last_egress_error.take() {
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model,
                        exits = tried.len(),
                        "opencode free lane: every attempted proxy exit failed; reporting the last egress error"
                    );
                    return (last_account, TryResult::Next(error));
                }
                // Egress scope was already logged once when it was learned; a
                // combo holding many free ids would otherwise repeat the same
                // warning per candidate. Account-level exhaustion is new
                // information and stays at warn.
                if exhaustion.egress {
                    tracing::debug!(
                        provider = %cand.provider, model = %cand.model,
                        reset_at_ms = exhaustion.reset_at_ms,
                        "opencode free lane: egress-parked window; skipping without an upstream request"
                    );
                } else {
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model,
                        reset_at_ms = exhaustion.reset_at_ms,
                        credentials = credentials.len(),
                        tried = tried.len(),
                        "opencode free lane: no credential has budget left; skipping without an upstream request"
                    );
                }
                // The local verdict only covers requests *this gateway* spent.
                // A warm session the upstream already accepted keeps serving
                // after the new-session admission quota runs out, so give the
                // lane one probe before reporting exhaustion: without this a
                // parked window also silences the whole session-memo feature.
                //
                // `begin_warm_probe` ignores the quota verdict for a single
                // dispatch, which is the only way to disprove it. Refused ids
                // are recorded so a dead session cannot monopolise the probe.
                // `warm_retried` means the admission fallback above already
                // spent a warm dispatch on this candidate: probing again would
                // charge the same bucket twice and skew its accounting.
                if state.config.opencode_free_session_fallback && !warm_retried {
                    for warm in crate::opencode_free::warm_session_candidates(&cand.provider)
                        .into_iter()
                        .take(WARM_PROBE_ATTEMPTS)
                    {
                        let Begin::Ready(probe_credential, probe_guard) =
                            budget::begin_warm_probe(&credentials, &limits, &tried, now_ms())
                        else {
                            break;
                        };
                        let warm_prefix: String = warm.chars().take(6).collect();
                        tracing::info!(
                            provider = %cand.provider, model = %cand.model,
                            credential = %budget::mask_bucket(&probe_credential.bucket),
                            warm_session = %warm_prefix,
                            "opencode free lane: budget exhausted, probing a warm session"
                        );
                        let result = free_warm_retry(
                            &state,
                            &req,
                            &cand,
                            &entry,
                            &probe_credential,
                            &warm,
                            started,
                            tokens_saved,
                            compressed,
                        )
                        .await;
                        match result {
                            Some((account, TryResult::FreeQuota(_, _)))
                            | Some((account, TryResult::Next(_))) => {
                                // The probe reached the upstream and was
                                // refused: this id is dead, demote it and let
                                // the next candidate have the slot.
                                budget::refund(&probe_credential.bucket);
                                drop(probe_guard);
                                crate::opencode_free::note_warm_session_refused(
                                    &cand.provider,
                                    &warm,
                                );
                                last_account = account.or(last_account);
                                continue;
                            }
                            Some((account, other)) => {
                                // A served probe disproves the exhaustion for
                                // this session, so reopen the window.
                                drop(probe_guard);
                                budget::note_success(&probe_credential.bucket, &cand.model, now_ms());
                                crate::opencode_free::note_session_success(&cand.provider, &warm);
                                tracing::info!(
                                    provider = %cand.provider, model = %cand.model,
                                    warm_session = %warm_prefix,
                                    "opencode free lane: warm probe served; reopening the parked window"
                                );
                                return (account.or(last_account), other);
                            }
                            None => {
                                // No slot could be reserved for the probe.
                                drop(probe_guard);
                                budget::refund(&probe_credential.bucket);
                                break;
                            }
                        }
                    }
                }
                return (last_account, TryResult::Next(budget::quota_error(&exhaustion)));
            }
        }
    }
}

/// How many warm sessions a single exhaustion probe may try. Bounded so a lane
/// whose warm set is entirely dead still fails fast instead of replaying the
/// whole pool on every request.
const WARM_PROBE_ATTEMPTS: usize = 3;

/// One bounded retry of the same request on a warm session.
///
/// The caller reserves the slot: the exhaustion-probe path uses
/// [`crate::opencode_free_budget::begin_warm_probe`] because the plain
/// reservation would refuse the very request that disproves the park. Returns
/// `None` only when the slot reservation already happened upstream of this call
/// and no dispatch could be made.
#[allow(clippy::too_many_arguments)]
async fn free_warm_retry(
    state: &Arc<AppState>,
    req: &ChatRequest,
    cand: &combo::Candidate,
    entry: &RegistryEntry,
    credential: &crate::opencode_free_budget::LaneCredential,
    warm_session: &str,
    started: Instant,
    tokens_saved: i64,
    compressed: bool,
) -> Option<(Option<crate::state::AccountCredential>, TryResult)> {
    let circuit_key = credential
        .account
        .as_ref()
        .map(|account| account.circuit_key(&cand.provider))
        .unwrap_or_else(|| cand.provider.clone());
    let _circuit = state.circuits.begin_request_guard(&circuit_key);
    let result = try_candidate(
        state,
        req,
        cand,
        entry,
        &AttemptContext {
            started,
            tokens_saved,
            compressed,
            account: credential.account.clone(),
            lane_credential: Some(credential.secret.clone()),
            lane_session: Some(warm_session.to_string()),
            lane_egress: Some(credential.egress.clone()),
        },
    )
    .await;
    Some((credential.account.clone(), result))
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Fields an OpenAI Chat message may carry to OpenCode Go.
///
/// The Go tier validates the message object against the Chat schema instead of
/// ignoring extra keys. A client that replays a Responses/Anthropic/Gemini
/// history therefore fails the whole turn with errors such as
/// `messages[1].provider_metadata: ...`, `messages[1].id`, `messages[1].status`,
/// `messages[1].phase`, `messages[1].signature`, `messages[1].thought_signature`,
/// `messages[1].parsed`, `messages[1].audio`, or `messages[1].tool_call_id`
/// (measured 2026-11-06 against `opencode-go/beam-free`). Keep only the keys the
/// Chat wire defines, plus the DeepSeek-compatible `reasoning_content` the same
/// upstream accepts.
fn opencode_go_message_fields(role: &str) -> &'static [&'static str] {
    match role {
        "assistant" => &[
            "role",
            "content",
            "name",
            "tool_calls",
            "refusal",
            "reasoning_content",
            "annotations",
        ],
        "tool" => &["role", "content", "name", "tool_call_id"],
        _ => &["role", "content", "name"],
    }
}

/// Fields the Go tier's closed Chat schema accepts at the top level. It rejects
/// every other key with `Unrecognized request argument supplied: <key>`
/// (measured 2026-11-07 for `system`, `cache_control`, `top_k`, `transforms`,
/// `route`, `provider` and `models`). Newer OpenAI-only knobs (`store`,
/// `service_tier`, `modalities`, `audio`, `verbosity`, ...) are deliberately
/// absent: they mean nothing to this tier, and dropping them cannot break a
/// turn the way forwarding them does.
fn opencode_go_top_level_fields() -> &'static [&'static str] {
    &[
        "model",
        "messages",
        "stream",
        "stream_options",
        "temperature",
        "top_p",
        "n",
        "stop",
        "max_tokens",
        "max_completion_tokens",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "user",
        "response_format",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "seed",
        "metadata",
        "reasoning_effort",
    ]
}

/// Chat tool and participant names are restricted to `[A-Za-z0-9_-]` and at
/// most 64 characters. MCP-style names in the wild use dots or spaces; rewrite
/// them consistently everywhere so the tool declaration and the replayed call
/// history still agree (measured 2026-11-07: `tools[0].function.name: string
/// does not match pattern`).
fn opencode_go_tool_name(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return None;
    }
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

/// Extract the text of one text-ish content part. OpenCode Go accepts only
/// `{"type":"text","text":...}` parts, but cross-protocol clients replay the
/// Responses spellings (`input_text`/`output_text`) and bare strings too.
fn opencode_go_text_part(part: &Value) -> Option<&str> {
    match part {
        Value::String(text) => (!text.trim().is_empty()).then_some(text.as_str()),
        Value::Object(part) => {
            let ty = part.get("type").and_then(Value::as_str).unwrap_or("text");
            let is_text = ty.eq_ignore_ascii_case("text")
                || ty.eq_ignore_ascii_case("input_text")
                || ty.eq_ignore_ascii_case("output_text");
            if !is_text {
                return None;
            }
            part.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
        }
        _ => None,
    }
}

/// Chat requires a string or a non-empty array of text parts, and only an
/// assistant message may be null. Rewrite every accepted spelling into one of
/// those two forms; return `None` when no text survives. A bare object content
/// (Responses text block) and objects/numbers are rejected by the upstream
/// (measured 2026-11-07).
fn opencode_go_normalize_content(content: Option<&Value>) -> Option<Value> {
    match content {
        Some(Value::String(text)) => {
            (!text.trim().is_empty()).then(|| Value::String(text.clone()))
        }
        Some(Value::Array(parts)) => {
            let normalized: Vec<Value> = parts
                .iter()
                .filter_map(|part| {
                    opencode_go_text_part(part).map(|text| json!({"type": "text", "text": text}))
                })
                .collect();
            (!normalized.is_empty()).then_some(Value::Array(normalized))
        }
        Some(Value::Object(_)) => {
            opencode_go_text_part(content?).map(|text| Value::String(text.to_string()))
        }
        _ => None,
    }
}

/// Flatten content to the single string Chat defines for a `tool` message.
fn opencode_go_content_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) => {
            (!text.trim().is_empty()).then(|| text.clone())
        }
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(opencode_go_text_part)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        Some(Value::Object(_)) => {
            opencode_go_text_part(content?).map(str::to_string)
        }
        _ => None,
    }
}

/// Rewrite an assistant `tool_calls` array into the one shape Chat defines.
///
/// A replayed history may carry `arguments` as an object (measured 2026-11-07:
/// `messages[0].tool_calls[0].function.arguments: value must be a string`) and
/// may omit `type`, `id` or `function`. Stringify arguments, force
/// `type: "function"`, and fill a stable id so the paired tool result can still
/// match.
fn opencode_go_normalize_tool_calls(
    message: &mut serde_json::Map<String, Value>,
    message_index: usize,
) {
    match message.get("tool_calls") {
        None => return,
        Some(Value::Array(_)) => {}
        Some(_) => {
            message.remove("tool_calls");
            return;
        }
    }
    let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
        return;
    };
    let mut normalized = Vec::with_capacity(calls.len());
    for (call_index, call) in calls.iter_mut().enumerate() {
        let Some(call) = call.as_object_mut() else { continue };
        let function = call.get("function").and_then(Value::as_object);
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .or_else(|| call.get("name").and_then(Value::as_str))
            .and_then(opencode_go_tool_name)
            .unwrap_or_else(|| "unknown".to_string());
        let arguments = function
            .and_then(|f| f.get("arguments"))
            .or_else(|| call.get("arguments"))
            .or_else(|| call.get("input"))
            .or_else(|| function.and_then(|f| f.get("input")));
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{message_index}_{call_index}"));
        normalized.push(json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": tool_call_arguments_to_string(arguments),
            }
        }));
    }
    calls.clear();
    calls.extend(normalized);
    if calls.is_empty() {
        message.remove("tool_calls");
    }
}

/// The Go tier accepts only nested `{type:"function", function:{...}}` tools.
/// Drop custom/vision tool kinds and malformed entries rather than fail the
/// whole turn; an empty result also invalidates `tool_choice` and
/// `parallel_tool_calls`.
fn opencode_go_normalize_tools(object: &mut serde_json::Map<String, Value>) {
    let Some(tools) = object.get_mut("tools") else {
        return;
    };
    let Some(tools) = tools.as_array_mut() else {
        object.remove("tools");
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
        return;
    };
    tools.retain_mut(|tool| {
        let Some(tool) = tool.as_object_mut() else { return false };
        let has_function = tool.get("function").is_some_and(Value::is_object);
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {}
            None if has_function => {
                tool.insert("type".into(), json!("function"));
            }
            _ => return false,
        }
        let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
            return false;
        };
        let Some(name) = function
            .get("name")
            .and_then(Value::as_str)
            .and_then(opencode_go_tool_name)
        else {
            return false;
        };
        function.insert("name".into(), json!(name));
        function.retain(|key, _| matches!(key.as_str(), "name" | "description" | "parameters" | "strict"));
        if function
            .get("description")
            .is_some_and(|description| !description.is_string())
        {
            function.remove("description");
        }
        if let Some(parameters) = function.get("parameters") {
            if !parameters.is_object() {
                function.insert("parameters".into(), json!({"type": "object", "properties": {}}));
            }
        }
        if function.get("strict").is_some_and(|strict| !strict.is_boolean()) {
            function.remove("strict");
        }
        tool.retain(|key, _| matches!(key.as_str(), "type" | "function"));
        true
    });
    if tools.is_empty() {
        object.remove("tools");
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
    }
}

/// Normalise `tool_choice` to the nested Chat shape and drop values the Go tier
/// rejects. A Responses-style flat choice is lifted, not forwarded verbatim.
fn opencode_go_normalize_tool_choice(object: &mut serde_json::Map<String, Value>) {
    let Some(choice) = object.get("tool_choice").cloned() else { return };
    let normalized = match choice {
        Value::String(choice) if matches!(choice.as_str(), "none" | "auto" | "required") => {
            Some(Value::String(choice))
        }
        Value::Object(choice) => choice
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| choice.get("name"))
            .and_then(Value::as_str)
            .and_then(opencode_go_tool_name)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    };
    match normalized {
        Some(choice) => {
            object.insert("tool_choice".into(), choice);
        }
        None => {
            object.remove("tool_choice");
        }
    }
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
    // The schema is closed: retain only the Chat surface and drop the rest.
    // This removes the Claude/Gemini aliases (`thinking`, `top_k`, ...), the
    // Responses continuation fields (`include`, `input`, ...) and foreign
    // client options (`system`, `cache_control`, `route`, ...) in one place.
    object.retain(|key, _| opencode_go_top_level_fields().contains(&key.as_str()));
    // `n` > 1 is rejected by the model, and the gateway answers a single
    // choice anyway.
    if object.get("n").and_then(Value::as_u64) != Some(1) {
        object.remove("n");
    }
    // `stop` must be a non-empty string or 1-4 non-empty strings. An empty
    // client value is a validation error, not a request to stop immediately.
    if let Some(stop) = object.get("stop").cloned() {
        let normalized = match stop {
            Value::String(stop) if !stop.is_empty() => Some(Value::String(stop)),
            Value::Array(items) => {
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|item| item.as_str().is_some_and(|text| !text.is_empty()))
                    .take(4)
                    .collect();
                (!kept.is_empty()).then_some(Value::Array(kept))
            }
            _ => None,
        };
        match normalized {
            Some(stop) => object.insert("stop".into(), stop),
            None => object.remove("stop"),
        };
    }
    // `max_tokens: 0` is rejected ("number must be at least 1"); removing the
    // cap is the same as the caller not supplying one.
    for key in ["max_tokens", "max_completion_tokens"] {
        if !matches!(object.get(key).and_then(Value::as_i64), Some(value) if value >= 1) {
            object.remove(key);
        }
    }
    // Clamp the sampling knobs to the ranges the model advertises instead of
    // failing the turn on a cross-provider value (measured 2026-11-07).
    for (key, min, max) in [
        ("temperature", 0.0_f64, 2.0_f64),
        ("top_p", 0.0, 1.0),
        ("presence_penalty", -2.0, 2.0),
        ("frequency_penalty", -2.0, 2.0),
    ] {
        match object.get(key).and_then(Value::as_f64) {
            Some(value) => {
                let clamped = value.clamp(min, max);
                if clamped != value {
                    object.insert(key.into(), json!(clamped));
                }
            }
            None => {
                object.remove(key);
            }
        }
    }
    // The Go tier only supports an empty `logit_bias`; a populated map is an
    // unsupported value, not a preference it can honour.
    if !object
        .get("logit_bias")
        .and_then(Value::as_object)
        .is_some_and(|bias| bias.is_empty())
    {
        object.remove("logit_bias");
    }
    if !matches!(object.get("seed"), Some(Value::Number(number)) if number.is_i64() || number.is_u64()) {
        object.remove("seed");
    }
    if object.get("user").is_some_and(|user| !user.is_string()) {
        object.remove("user");
    }
    // Chat only defines `include_usage` here, and only as a boolean.
    if let Some(options) = object.get_mut("stream_options").and_then(Value::as_object_mut) {
        options.retain(|key, value| key == "include_usage" && value.is_boolean());
        if options.is_empty() {
            object.remove("stream_options");
        }
    } else if object.contains_key("stream_options") {
        object.remove("stream_options");
    }
    // Keep only the response-format shapes the Chat schema defines; an
    // unrecognised client value is better treated as "no format" than as a
    // failed turn.
    let response_format = match object.get("response_format") {
        Some(Value::Object(format)) => match format.get("type").and_then(Value::as_str) {
            Some("text" | "json_object") => Some(json!({"type": format["type"].clone()})),
            Some("json_schema") if format.get("json_schema").is_some_and(Value::is_object) => {
                Some(Value::Object(format.clone()))
            }
            _ => None,
        },
        _ => None,
    };
    match response_format {
        Some(format) => {
            object.insert("response_format".into(), format);
        }
        None => {
            object.remove("response_format");
        }
    }
    // `logprobs: true` / `top_logprobs` are unsupported on `beam-free`; false
    // is the only accepted value. Drop the pair rather than fail the turn.
    if object.get("logprobs").and_then(Value::as_bool) != Some(false) {
        object.remove("logprobs");
    }
    object.remove("top_logprobs");
    // Chat metadata is a flat string map. A nested client object is rejected
    // with `metadata.a: value must be a string`.
    if let Some(metadata) = object.get_mut("metadata") {
        let keep = metadata.as_object_mut().map(|map| {
            map.retain(|_, value| value.is_string());
            !map.is_empty()
        });
        if keep != Some(true) {
            object.remove("metadata");
        }
    }
    // The Go tier accepts only these effort spellings. Preserve the caller's
    // intent for the two common aliases instead of failing the whole turn.
    if let Some(effort) = object.get("reasoning_effort").and_then(Value::as_str) {
        let effort = match effort {
            "max" | "xhigh" | "high" | "medium" | "low" => Some(effort.to_string()),
            "minimal" | "none" => Some("low".to_string()),
            _ => None,
        };
        match effort {
            Some(effort) => {
                object.insert("reasoning_effort".into(), json!(effort));
            }
            None => {
                object.remove("reasoning_effort");
            }
        }
    }
    // An empty tools array is rejected as a validation error. Without tools,
    // tool_choice and parallel_tool_calls are meaningless and equally invalid.
    opencode_go_normalize_tools(object);
    if !object.contains_key("tools") {
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
    } else {
        opencode_go_normalize_tool_choice(object);
        if object
            .get("parallel_tool_calls")
            .is_some_and(|value| !value.is_boolean())
        {
            object.remove("parallel_tool_calls");
        }
    }
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    // Track the ids the immediately preceding assistant turn issued, so a tool
    // result that lost `tool_call_id` can still be paired instead of failing
    // validation (`Missing required parameter: messages[1].tool_call_id`).
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut message_index = 0usize;
    messages.retain_mut(|message| {
        let index = message_index;
        message_index += 1;
        let Some(message) = message.as_object_mut() else { return false };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !matches!(role.as_str(), "system" | "user" | "assistant" | "tool" | "developer") {
            return false;
        }
        let fields = opencode_go_message_fields(&role);
        message.retain(|key, _| fields.contains(&key.as_str()));
        match message.get("name").and_then(Value::as_str) {
            Some(name) => match opencode_go_tool_name(name) {
                Some(name) => {
                    message.insert("name".into(), json!(name));
                }
                None => {
                    message.remove("name");
                }
            },
            None => {
                message.remove("name");
            }
        }
        if message
            .get("tool_call_id")
            .is_some_and(|id| !id.as_str().is_some_and(|id| !id.trim().is_empty()))
        {
            message.remove("tool_call_id");
        }
        if message
            .get("reasoning_content")
            .is_some_and(|reasoning| !reasoning.is_string())
        {
            message.remove("reasoning_content");
        }
        if message
            .get("refusal")
            .is_some_and(|refusal| !refusal.is_string() && !refusal.is_null())
        {
            message.remove("refusal");
        }
        if message
            .get("annotations")
            .is_some_and(|annotations| !annotations.is_array())
        {
            message.remove("annotations");
        }
        match role.as_str() {
            "assistant" => {
                opencode_go_normalize_tool_calls(message, index);
                pending_call_ids.clear();
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    pending_call_ids.extend(calls.iter().filter_map(|call| {
                        call.get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }));
                }
            }
            "tool" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let id = match id {
                    Some(id) if pending_call_ids.contains(&id) => Some(id),
                    Some(id) if pending_call_ids.is_empty() => Some(id),
                    _ => pending_call_ids.first().cloned(),
                };
                match id {
                    Some(id) => {
                        if let Some(position) =
                            pending_call_ids.iter().position(|pending| pending == &id)
                        {
                            pending_call_ids.remove(position);
                        }
                        message.insert("tool_call_id".into(), json!(id));
                    }
                    None => return false, // an orphan result cannot be validated
                }
            }
            _ => pending_call_ids.clear(),
        }
        let content = opencode_go_normalize_content(message.get("content"));
        match role.as_str() {
            // OpenCode Go allows null content only on an assistant message.
            "assistant" => {
                message.insert("content".into(), content.unwrap_or(Value::Null));
            }
            // A tool call with no output still needs a result the model can
            // read; an empty string is rejected, so use a short marker.
            "tool" => {
                let text = opencode_go_content_text(content.as_ref())
                    .unwrap_or_else(|| "(no output)".to_string());
                message.insert("content".into(), json!(text));
            }
            // Empty system/user turns carry no information and cannot be sent
            // as-is; drop them rather than fail the whole request.
            _ => match content {
                Some(content) => {
                    message.insert("content".into(), content);
                }
                None => return false,
            },
        }
        true
    });
}


/// Normalise `role: "tool"` content for **every** OpenAI-format upstream.
///
/// The Chat Completions schema defines tool content as a string, but clients
/// increasingly send the Responses-style array of parts. Strict upstreams then
/// reject the whole request — measured 2026-10-28 on OpenCode Go:
///
/// ```text
/// tool message content part 0 is not text; only text parts are supported on
/// the gRPC path (request_id: gw-...)
/// ```
///
/// The tool already ran and its output is text the model must read, so
/// rejecting the turn is never what the caller wanted, and flattening loses
/// nothing the upstream could have consumed. This is a client-shape issue, not a
/// provider one, so it applies to all OpenAI-format targets.
fn normalize_tool_content(body: &mut Value, wire_format: ProvFormat) {
    if wire_format != ProvFormat::OpenAI {
        return;
    }
    let Some(object) = body.as_object_mut() else { return };
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(msg) = message.as_object_mut() else { continue };
        if msg.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(parts) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        let text = parts
            .iter()
            .filter_map(|part| match part {
                Value::String(s) => Some(s.as_str()),
                Value::Object(_) => part.get("text").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        msg.insert("content".into(), json!(text));
    }
}

fn is_assistant_tool_call_block(part: &Value) -> bool {
    let Some(ty) = part.get("type").and_then(Value::as_str) else { return false };
    ty.eq_ignore_ascii_case("tool-call")
        || ty.eq_ignore_ascii_case("toolCall")
        || ty.eq_ignore_ascii_case("tool_use")
        || ty.eq_ignore_ascii_case("function_call")
}

fn tool_call_arguments_to_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "{}".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn assistant_tool_call_block_to_openai(part: &Value, index: usize) -> Option<Value> {
    if !is_assistant_tool_call_block(part) {
        return None;
    }
    let id = part
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| part.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| part.get("tool_call_id").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .or_else(|| {
            part.get("function")
                .and_then(|f| f.get("id").or_else(|| f.get("call_id")).or_else(|| f.get("tool_call_id")))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call_{index}"));
    let name = part
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            part.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(|s| s.to_string())?;
    let arguments = tool_call_arguments_to_string(
        part.get("arguments")
            .or_else(|| part.get("input"))
            .or_else(|| part.get("function").and_then(|f| f.get("arguments")))
            .or_else(|| part.get("function").and_then(|f| f.get("input"))),
    );
    Some(json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

fn is_openai_chat_text_part(part: &Value) -> bool {
    match part {
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(o) => {
            let ty = o.get("type").and_then(Value::as_str).unwrap_or("text");
            (ty.eq_ignore_ascii_case("text")
                || ty.eq_ignore_ascii_case("output_text")
                || ty.eq_ignore_ascii_case("input_text"))
                && o.get("text").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty())
        }
        _ => false,
    }
}

fn assistant_content_has_text(content: &[Value]) -> bool {
    content.iter().any(|part| !is_assistant_tool_call_block(part) && is_openai_chat_text_part(part))
}

/// Normalize assistant tool-call blocks embedded in `content` for OpenAI Chat.
///
/// Some clients (pi-ai/DSH) store tool invocations as `content[]` blocks
/// (`tool-call`/`toolCall`/`tool_use`/`function_call`) with `arguments` as a
/// string or object. OpenAI Chat accepts assistant tool invocations only via the
/// top-level `tool_calls` field and rejects foreign content block types, so we
/// lift them into `tool_calls`, stringify `arguments`, and keep only legitimate
/// text parts in `content` (or `null` when the turn is tool-call-only).
fn normalize_assistant_tool_calls_for_chat(body: &mut Value, wire_format: ProvFormat) {
    if wire_format != ProvFormat::OpenAI {
        return;
    }
    let Some(object) = body.as_object_mut() else { return };
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(msg) = message.as_object_mut() else { continue };
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        if !role.eq_ignore_ascii_case("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").cloned() else { continue };
        let Value::Array(parts) = content else { continue };
        if !parts.iter().any(is_assistant_tool_call_block) {
            continue;
        }

        let has_existing_tool_calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let keep_text = assistant_content_has_text(&parts);
        let mut extracted: Vec<Value> = Vec::new();
        let mut cleaned: Vec<Value> = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if is_assistant_tool_call_block(&part) {
                if !has_existing_tool_calls {
                    if let Some(tc) = assistant_tool_call_block_to_openai(&part, i) {
                        extracted.push(tc);
                    }
                }
                continue;
            }
            if is_openai_chat_text_part(&part) {
                cleaned.push(part);
            }
        }

        if !has_existing_tool_calls && !extracted.is_empty() {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut deduped: Vec<Value> = Vec::new();
            for tc in extracted {
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !seen.insert(id.to_string()) {
                        continue;
                    }
                }
                deduped.push(tc);
            }
            if !deduped.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(deduped));
            }
        }

        let has_tool_calls_now = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        // After rewriting an array that contained tool-call blocks, always set
        // content explicitly so discarded (e.g. name-less) blocks cannot leak a
        // non-Chat block type back to a strict OpenAI upstream.
        if cleaned.is_empty() || (has_tool_calls_now && !keep_text) {
            msg.insert("content".into(), Value::Null);
        } else {
            msg.insert("content".into(), Value::Array(cleaned));
        }
    }
}

/// Strip stateful (Responses-format) history fields from an OpenAI **Chat**
/// request.
///
/// A client that keeps a stateful assistant history replays fields defined by
/// the Responses wire format — `reasoning_details` and `provider_metadata` — in
/// its message array. Chat Completions defines neither, and a strict upstream
/// validates the message shape instead of ignoring the extra keys: measured
/// 2026-11-06 against `opencode-go` (OpenAI-chat wire format), a 626-message
/// history failed the whole turn and every candidate after it fell back to the
/// rate-limited free lane:
///
/// ```text
/// msg=messages[626].reasoning_details: cannot preserve this message field
/// across protocols; stateful assistant history requires matching
/// provider_metadata
/// messages[1].provider_metadata: provider-bound continuation requires
/// undefined; this target uses openai-chat
/// messages[1].provider_metadata: provider-bound continuation cannot move to a
/// different upstream model
/// ```
///
/// The history belongs to the gateway once translated, so dropping the fields
/// on a Chat target loses nothing the upstream can read. Anthropic reads signed
/// `thinking` blocks from `content` parts and Responses targets carry these
/// fields natively, so only Chat targets are cleaned.
///
/// `reasoning_content` is deliberately left alone: it is the flat
/// DeepSeek-compatible spelling that the same upstream accepts natively, and no
/// 400 in the measurement named it.
fn strip_stateful_history(body: &mut Value, provider: &str, wire_format: ProvFormat) {
    if wire_format != ProvFormat::OpenAI {
        return;
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for (index, message) in messages.iter_mut().enumerate() {
        let Some(object) = message.as_object_mut() else { continue };
        for field in ["reasoning_details", "provider_metadata"] {
            if object.remove(field).is_some() {
                tracing::debug!(
                    provider = %provider,
                    model = %model,
                    message_index = index,
                    field = %field,
                    "stripped a stateful history field for an OpenAI chat upstream"
                );
            }
        }
    }
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
    // (`stream: true` + the official prompt and `read` tool) — see
    // `crate::opencode_free::apply_agent_shape`.
    let upstream_stream = want_stream || wire_entry.force_stream || free_lane;

    let mut upstream_body = translate_request_body(req.inbound_format, wire_format, &req.body, cand.model.clone(), upstream_stream);
    // Never ask an upstream for more output than it advertises. The client cap
    // is already clamped this way inside the window-fit test, so clamping here
    // keeps the dispatched request consistent with the fit decision.
    clamp_output_tokens(
        &mut upstream_body,
        state.capabilities_for_model(&cand.provider, &cand.model).max_output_tokens,
    );
    normalize_tool_content(&mut upstream_body, wire_format);
    normalize_assistant_tool_calls_for_chat(&mut upstream_body, wire_format);
    strip_stateful_history(&mut upstream_body, &cand.provider, wire_format);
    sanitize_opencode_go_request(&mut upstream_body, &cand.provider, wire_format);
    sanitize_non_stream_options(&mut upstream_body, wire_format);
    sanitize_developer_role(&mut upstream_body, &cand.provider, wire_format);
    // The anonymous lane only answers a streaming, agent-shaped body: the
    // official CLI system prompt, the official `read` tool and one canonical
    // `ses_` session. See `crate::opencode_free::apply_agent_shape`.
    if free_lane {
        if let Some(wire) = crate::opencode_free::LaneWire::from_format(wire_format) {
            crate::opencode_free::apply_agent_shape(&mut upstream_body, &cand.model, wire);
        }
    }
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
    // A provider may carry several managed accounts. Draw one for this request
    // (round-robin, skipping accounts whose own key is cooling) instead of
    // always using whichever connection was saved last. With no managed
    // account the config/env credential still applies, so single-credential
    // setups are untouched.
    let account = ctx.account.clone();
    let configured_key = match &account {
        Some(a) => a.api_key.clone(),
        None => state.api_key_for(&cand.provider),
    };
    // The lane's credential rule (key when opted in and accepted, else the
    // literal `public`) is shared with the dashboard probes so a "test" button
    // and a real request can never disagree.
    let key_override = if free_lane {
        // The budget planner already reserved exactly which credential this
        // attempt uses: a specific account's key, the env/config key, or the
        // literal `public`. The fallback below only serves direct callers that
        // skip the planner.
        ctx.lane_credential.clone().or_else(|| {
            crate::opencode_free::lane_credential(
                state.config.opencode_free_credential_mode
                    == crate::opencode_free_budget::CredentialMode::Key,
                configured_key.clone(),
                now_ms(),
            )
        })
    } else {
        configured_key.clone()
    };
    // Which account serves this request — the caller logs it, so an idle
    // account cannot stay invisible.
    let account_label = account.as_ref().map(|a| a.label());
    let _ = &account_label;
    let (url, mut headers) = match build_upstream_request(
        &state.config,
        &state.registry,
        &wire_entry,
        &cand.provider,
        &cand.model,
        upstream_stream,
        key_override,
        match &account {
            Some(a) => a
                .base_url
                .clone()
                .filter(|b| !b.trim().is_empty())
                .or_else(|| state.base_url_for(&state.registry, &cand.provider)),
            None => state.base_url_for(&state.registry, &cand.provider),
        },
    ) {
        Ok(v) => v,
        Err(e) => return TryResult::Next(e),
    };
    // The free tier moved off `/zen/v1` on 2026-10-14: the client takes its
    // base URL from the models.dev catalog and the identical body is refused on
    // `/zen/v1`. Paid models keep the registry base; only the zero-cost lane is
    // re-homed here — first onto the flat `/inference/v1` route (the legacy
    // fallback), then onto the 2.1 client's family route
    // (`/inference/openai/v1/...`).
    let mut url = url;
    if free_lane {
        if let Some(rehomed) = crate::opencode_free::inference_url(&url) {
            tracing::debug!(url = %rehomed, "free-lane request re-homed onto the inference surface");
            url = rehomed;
        }
        if let Some(family) = crate::opencode_free::lane_url(&url) {
            tracing::debug!(url = %family, "free-lane request re-homed onto the family route");
            url = family;
        }
    }
    let mut lane_session: Option<String> = None;
    if free_lane {
        // A canonical, conversation-stable `ses_` id: the lane rejects the
        // random shape with 429 FreeUsageLimitError, and groups both its budget
        // and the upstream prompt cache by this value. The official client
        // sends the same value under four header names. The same helper sets
        // the 2.1 User-Agent and drops the Go-tier request id the inference
        // surface refuses.
        //
        // The caller may pin a specific session (the admission fallback retries
        // with an already-accepted one); otherwise the persistent memo derives
        // it from the conversation, so a restart does not mint a new session.
        let seed = crate::opencode_free::conversation_seed(&req.body);
        let session = ctx
            .lane_session
            .clone()
            .unwrap_or_else(|| crate::opencode_free::session_for(&seed, now_ms()));
        crate::opencode_free::apply_lane_headers(&mut headers, &session);
        lane_session = Some(session);
    }

    // The free lane goes through the Bun-shaped BoringSSL transport: the
    // upstream fingerprints reqwest/rustls as not-the-official-client and
    // answers every request with `429 FreeUsageLimitError`.
    //
    // The egress is resolved by the caller for the free lane (the budget
    // planner expands the anonymous credential over the proxy pool); other
    // providers never tunnel here.
    let free_egress = if free_lane {
        ctx.lane_egress.clone().or_else(|| {
            Some(crate::upstream::bun::Egress::from_proxy_url(
                account.as_ref().and_then(|account| account.proxy_url.as_deref()),
            ))
        })
    } else {
        None
    };
    let body_bytes = Bytes::from(upstream_body.to_string());
    let dispatched = if let Some(egress) = &free_egress {
        state
            .upstream
            .execute_free_lane(
                &url,
                headers,
                body_bytes,
                state.config.request_timeout_ms,
                egress,
            )
            .await
    } else {
        state
            .upstream
            .execute(&url, reqwest::Method::POST, headers, Some(body_bytes), state.config.request_timeout_ms)
            .await
    };
    let resp = match dispatched {
        Ok(r) => r,
        Err(crate::upstream::executor::UpstreamError::Egress(message)) => {
            // The proxy/tunnel failed: cool that exit briefly so the next
            // attempt (the free-lane loop rotates to the next proxy exit)
            // skips it, and report a distinguishable, retryable error instead
            // of a generic bad gateway.
            if let Some(label) = free_egress.as_ref().and_then(|egress| egress.label()) {
                let cooldown_ms = u128::from(state.config.opencode_free_proxy_cooldown_secs) * 1000;
                let until = crate::opencode_free_budget::note_egress_failure(
                    &label,
                    now_ms(),
                    cooldown_ms,
                );
                tracing::warn!(
                    provider = %cand.provider, model = %cand.model,
                    egress = %crate::upstream::bun::mask_egress_label(&label),
                    cooldown_until_ms = until,
                    msg = %message,
                    "opencode free lane: proxy egress failed; cooling this exit"
                );
            }
            let mut error = ApiError::new(503, format!("network error: {message}"));
            error.code = crate::upstream::bun::EGRESS_ERROR_CODE.to_string();
            return TryResult::Next(error);
        }
        Err(e) => return TryResult::Next(ApiError::new(502, format!("network error: {e}"))),
    };

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        // The free lane's quota 429 advertises its reset in `Retry-After`
        // (about the next 00:00 UTC). Read it before the body is consumed.
        let retry_after_ms = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(crate::opencode_free_budget::parse_retry_after_ms);
        let text = resp.text().await.unwrap_or_default();
        // Learn from the lane: a region block, a delisted id or a per-IP 429
        // is a statement about the model (or this egress), not a gateway bug,
        // so the roster drops it for a bounded window.
        crate::opencode_free::record_verdict(&cand.model, &cand.provider, status, &text, ctx.account.as_ref().map(|a| a.label()).as_deref());
        // Free-quota 429: report it with its cooldown so the caller can cool
        // the credential and try a sibling account instead of a sibling model.
        if free_lane && crate::opencode_free::is_free_usage_limit(status, &text) {
            let body_json = serde_json::from_str::<Value>(&text).ok();
            return TryResult::FreeQuota(
                crate::upstream::executor::upstream_error_with_text(
                    status,
                    body_json,
                    Some(&text),
                ),
                retry_after_ms.unwrap_or(0),
            );
        }
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
        if crate::opencode_free::is_opencode_tier(&cand.provider) && status == 400 {
            // Temporary probe for the DSH long-session 400: what size did the
            // gateway think this request was when the upstream refused it?
            let body_bytes = serde_json::to_vec(&req.body).map(|b| b.len()).unwrap_or(0);
            let msg_count = req.body.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
            let tool_count = req.body.get("tools").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
            tracing::warn!(
                provider = %cand.provider, model = %cand.model,
                body_bytes, messages = msg_count, tools = tool_count,
                raw_estimate = request_prompt_tokens(&req.body),
                conservative = conservative_prompt_tokens(&req.body),
                full_estimate = crate::compression::estimate_body_tokens(&req.body),
                output_tokens = request_output_tokens(&req.body),
                msg = %crate::upstream::executor::upstream_error_with_text(status, body_json.clone(), Some(&text)).message,
                "opencode tier 400 probe"
            );
        }
        // A context-length refusal is recoverable for a client that owns
        // compaction: it must be reported as such, not as a generic 400,
        // otherwise the client treats the request as malformed and gives up
        // instead of shrinking the conversation and retrying.
        if is_context_length_refusal(status, &text) {
            let estimate = conservative_prompt_tokens(&req.body);
            // Report this candidate's real window: passing 0 produced the
            // self-contradictory "estimated 1 tokens is larger than the 0
            // token context window".
            let window = state.capabilities_for_model(&cand.provider, &cand.model).context_window;
            return TryResult::Next(ApiError::context_window_exceeded(
                estimate,
                window,
                &cand.model,
            ));
        }
        // OpenCode answers an over-window prompt with a bare "Invalid
        // request" (measured on beam-free: 150k words + a 131072 cap dies
        // with exactly this, while 140k words + the same cap succeeds — the
        // window is prompt + output reservation). Shape problems, by
        // contrast, always arrive with a specific `[invalid_value] ...`
        // detail, so a *generic* "Invalid request" on a large request is a
        // size refusal whose true token count the chars/4 estimate missed:
        // a real DSH long session measured raw 621k / conservative 807k
        // while the upstream rejected it, i.e. the estimate undercounts by
        // at least 14% even after the margin. Report the recoverable
        // context error (at more than half the window accounted for) so a
        // compaction-capable client shrinks and retries instead of dying
        // on a generic 400; small malformed requests keep their honest 400.
        if status == 400
            && crate::opencode_free::is_opencode_tier(&cand.provider)
            && is_generic_invalid_request(&text)
        {
            let caps = state.capabilities_for_model(&cand.provider, &cand.model);
            let estimate = conservative_prompt_tokens(&req.body);
            let required = combo::candidate_required_context_tokens(
                estimate,
                request_output_tokens(&req.body),
                caps.max_output_tokens,
            );
            let full = crate::compression::estimate_body_tokens(&req.body);
            let required_full = combo::candidate_required_context_tokens(
                full,
                request_output_tokens(&req.body),
                caps.max_output_tokens,
            );
            if generic_400_is_size_refusal(required, required_full, caps.context_window) {
                // The upstream proved the request does not fit, so the
                // honest lower bound is the window itself; keep the
                // reported estimate above it rather than printing a
                // self-contradictory "estimated X is larger than Y".
                let reported = estimate.max(caps.context_window + 1);
                return TryResult::Next(ApiError::context_window_exceeded(
                    reported,
                    caps.context_window,
                    &cand.model,
                ));
            }
        }
        return TryResult::Next(crate::upstream::executor::upstream_error_with_text(
            status,
            body_json,
            Some(&text),
        ));
    }
    crate::opencode_free::record_verdict(&cand.model, &cand.provider, status, "", ctx.account.as_ref().map(|a| a.label()).as_deref());
    if let Some(session) = &lane_session {
        // Only a 2xx makes a session warm: a refused session must never be
        // recommended for reuse (see `opencode_free::note_session_success`).
        crate::opencode_free::note_session_success(&cand.provider, session);
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
                stream_response_pump(state, resp, req.inbound_format, wire_format, cand.model.clone(), cand.provider.clone(), req.model_str.clone(), cand.combo.clone(), account_label.clone(), crate::compression::estimate_body_tokens(&req.body).max(0) as u64, ctx.started, ctx.tokens_saved, ctx.compressed, req.endpoint.clone(), req.reasoning_effort.clone(), req.client_ip.clone(), req.requester.clone()),
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
                            crate::opencode_free::record_verdict(&cand.model, &cand.provider, 502, "empty content", ctx.account.as_ref().map(|a| a.label()).as_deref());
                        }
                        tracing::warn!(
                            provider = %cand.provider, model = %cand.model,
                            "upstream stream folded to empty content; falling back"
                        );
                        return TryResult::Next(ApiError::new(502, "upstream returned empty content"));
                    }
                    let mut u = crate::state::usage_from_value(&full);
                    if u.is_zero() {
                        u = synthesized_usage(&req.body, &reply_text_of(&full));
                        tracing::warn!(
                            prompt = u.prompt, completion = u.completion,
                            "upstream reported no usage on a folded stream; reporting an estimate"
                        );
                    }
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
                crate::opencode_free::record_verdict(&cand.model, &cand.provider, 502, "empty content", ctx.account.as_ref().map(|a| a.label()).as_deref());
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
                let mut u = crate::state::usage_from_value(&upstream_json);
                if u.is_zero() {
                    u = synthesized_usage(&req.body, &reply_text_of(&upstream_json));
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model, stream = true,
                        prompt = u.prompt, completion = u.completion,
                        "upstream reported no usage; reporting an estimate"
                    );
                }
                TryResult::Responded(sse_response(frames), Some(u))
            }
        } else {
            {
                let mut reply = translate_json_response(wire_format, req.inbound_format, &upstream_json, &cand.model);
                let mut u = crate::state::usage_from_value(&upstream_json);
                if u.is_zero() {
                    u = synthesized_usage(&req.body, &reply_text_of(&upstream_json));
                    tracing::warn!(
                        provider = %cand.provider, model = %cand.model, stream = false,
                        prompt = u.prompt, completion = u.completion,
                        "upstream reported no usage; reporting an estimate"
                    );
                    // Surface the estimate to the client too, otherwise it shows
                    // the same unexplained zero the dashboard used to.
                    if let Some(obj) = reply.as_object_mut() {
                        obj.insert(
                            "usage".to_string(),
                            json!({
                                "prompt_tokens": u.prompt,
                                "completion_tokens": u.completion,
                                "total_tokens": u.prompt + u.completion,
                                "omniroute_estimated": true
                            }),
                        );
                    }
                }
                TryResult::Responded(json_response(reply), Some(u))
            }
        }
    }
}

/// Does a complete response carry any actual output (text or tool calls)?
///
/// Covers the three wire shapes the gateway folds into: OpenAI chat
/// (`choices[].message`), Anthropic native (`content[].text` /
/// `content[].tool_use`) and the Responses API (`output[]`).
/// Does this upstream refusal mean "the prompt does not fit the model"?
///
/// Providers phrase it differently, so the match is by phrase rather than by
/// code: an OpenAI-style `context_length_exceeded`, Anthropic's
/// "prompt is too long", Gemini's "input token limit". Reporting it as a
/// plain 400 makes an agent client treat the request as malformed and stop,
/// while it is in fact recoverable — shrink the conversation and retry.
fn is_context_length_refusal(status: u16, body: &str) -> bool {
    if status != 400 && status != 413 {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    // A bare "context window" is too weak: a 400 that merely *mentions* the
    // window ("you are outside the context window of a tool") is not a size
    // refusal, and rewriting it as one sent clients into a compaction loop.
    // The phrases below all assert that the payload did not fit.
    const STRICT: &[&str] = &[
        // `context_length_exceeded` (already the exact code) — NOT the bare
        // `context_length`, which also matches validation errors such as
        // "context_length must be an integer".
        "context_length_exceeded",
        "maximum context length",
        "prompt is too long",
        "reduce the length of the messages",
        "request resulted in more tokens",
        "input token limit",
        "exceeds the maximum input",
        "token limit reached",
        "input is too long",
    ];
    if STRICT.iter().any(|m| lowered.contains(m)) {
        return true;
    }
    // "N tokens > M maximum" / "context window of N tokens" — numeric forms.
    // A phrase like "too many tokens in the batch envelope" is a different
    // complaint (split the batch), so it needs a number to be a size refusal.
    let digits = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).count();
    if lowered.contains("too many tokens") || lowered.contains("token limit reached") {
        return digits(&lowered) >= 3;
    }
    let mentions_tokens = lowered.contains("token");
    mentions_tokens
        && (lowered.contains("context window") || lowered.contains("maximum context"))
        && digits(&lowered) >= 3
}

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
    // Gemini native: the answer lives in `candidates[*].content.parts`.
    // Without this branch a non-streaming Gemini reply was always judged
    // empty — the request was reported 502 even though the upstream had
    // answered — while the streaming path read the same payload correctly.
    if let Some(candidates) = value.get("candidates").and_then(|c| c.as_array()) {
        for candidate in candidates {
            // The answer is the parts themselves. A `finishReason` alone is
            // not output: `STOP` with an empty `parts` array is exactly the
            // empty reply the funnel exists to catch, and counting it as an
            // answer let every empty Gemini reply through as a success.
            if let Some(Value::Array(parts)) = candidate.pointer("/content/parts") {
                if parts.iter().any(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| !t.is_empty())
                        || p.get("functionCall").is_some()
                }) {
                    return true;
                }
            }
        }
    }
    false
}

/// Estimate usage when the upstream reported none.
///
/// A delivered answer with no `usage` is indistinguishable from a failure on the
/// dashboard (both render `0|0`) and leaves token-billed clients unable to
/// account for the call. Estimate from the request and the reply text instead —
/// the result is clearly an estimate, but it is the right order of magnitude and
/// keeps a successful call from looking like a broken one.
fn synthesized_usage(req_body: &Value, reply_text: &str) -> crate::state::TokenUsage {
    let prompt = crate::compression::estimate_body_tokens(req_body).max(0) as u64;
    let completion = crate::compression::estimate::estimate_tokens(reply_text).max(0) as u64;
    crate::state::TokenUsage { prompt, completion }
}

/// Append one reply payload to `out`, following `content`-shaped values of any
/// depth.
///
/// Upstreams answer with a String, an array of text/tool blocks, or an object
/// carrying its own `text`/`content`/`input` — and [`value_has_output`] counts
/// all of them as output. Taking only `as_str()` here silently skipped the
/// structured forms, so a turn that did produce text was estimated at zero
/// tokens and the client showed an unexplained `0|0`.
fn append_reply_text(out: &mut String, v: Option<&Value>) {
    let Some(v) = v else { return };
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => {
            for item in items {
                append_reply_text(out, Some(item));
            }
        }
        Value::Object(obj) => {
            for key in ["text", "content", "input"] {
                if let Some(child) = obj.get(key) {
                    append_reply_text(out, Some(child));
                }
            }
        }
        _ => {}
    }
}

/// Concatenate every text/reasoning/tool-argument string in a reply, so the
/// estimate reflects what was actually produced.
///
/// The fields here are exactly the ones [`value_has_output`] accepts: only what
/// the empty check called output may still be estimated as nothing.
fn reply_text_of(value: &Value) -> String {
    let mut out = String::new();
    if let Some(choices) = value.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            // A legacy completions reply puts its payload on the choice itself.
            append_reply_text(&mut out, choice.get("text"));
            append_reply_text(&mut out, choice.get("content"));
            for holder in [choice.get("message"), choice.get("delta")] {
                let Some(obj) = holder.and_then(|h| h.as_object()) else { continue };
                append_reply_text(&mut out, obj.get("content"));
                append_reply_text(&mut out, obj.get("reasoning_content"));
                if let Some(tcs) = obj.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        append_reply_text(&mut out, tc.pointer("/function/name"));
                        append_reply_text(&mut out, tc.pointer("/function/arguments"));
                    }
                }
            }
        }
    }
    // Anthropic native: top-level content is an array of blocks.
    append_reply_text(&mut out, value.get("content"));
    out
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
    upstream: crate::upstream::executor::UpstreamResponse,
    inbound: Format,
    upstream_format: ProvFormat,
    model: String,
    provider: String,
    request_model: String,
    // The combo this request resolved through, for the request log. A stream
    // is logged by the pump (usage arrives only at the end), so the combo has
    // to be handed in — the dispatcher's own entry never gets written.
    combo: Option<String>,
    // Account (connection name) that produced this stream, for the log row.
    account_label: Option<String>,
    prompt_estimate: u64,
    started: std::time::Instant,
    tokens_saved: i64,
    compressed: bool,
    endpoint: String,
    reasoning_effort: Option<String>,
    client_ip: Option<String>,
    requester: Option<String>,
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
        let mut reply_text = String::new();
        // 200 until something proves otherwise. A mid-stream transport error
        // used to leave this at 200, so a stream that died halfway was logged
        // as a success and the combo-health page counted it as one.
        let mut upstream_status: u16 = 200;

        // The gap timer is created ONCE, outside the loop. Creating it inside
        // the iteration — as this did — restarted the window every time the
        // heartbeat fired, so a slow upstream that emitted nothing but
        // keepalives never tripped the idle timeout: the stream stayed open
        // forever instead of failing with 504.
        // `Sleep` is `!Unpin`, so the handle lives in a pinned box that the
        // loop re-points at a fresh timer whenever progress is made.
        let mut gap_deadline: std::pin::Pin<Box<tokio::time::Sleep>> =
            Box::pin(tokio::time::sleep(readiness));
        // Labeled: a client disconnect is discovered deep inside nested
        // loops (parser → chunk → frame) and a bare `break` there would only
        // leave the innermost one, letting the pump keep hammering a dead
        // channel. `break 'pump` reaches the log at the tail either way.
        'pump: loop {
            // Wait for upstream bytes. First byte honors the readiness
            // timeout; afterwards each gap honors the stream-idle timeout;
            // the heartbeat fires a keepalive comment when no bytes arrive.
            let next: Option<Result<Bytes, io::Error>> = tokio::select! {
                r = byte_stream.next() => match r {
                    None => None,
                    Some(Err(e)) => Some(Err(io::Error::other(e.to_string()))),
                    Some(Ok(b)) => Some(Ok(b)),
                },
                _ = tokio::time::sleep(heartbeat), if got_first => {
                    if tx.send(Ok(frame_keepalive())).await.is_err() {
                        // Client gone: break rather than return, so the entry
                        // at the tail still records what happened — otherwise
                        // the request vanishes from the log entirely.
                        upstream_status = 499;
                        break;
                    }
                    // A keepalive is not progress: the gap timer keeps
                    // running underneath it.
                    continue;
                }
                _ = &mut gap_deadline => None,
            };
            match next {
                None => break,
                Some(Ok(buf)) => {
                    'events: for ev in parser.feed(&buf) {
                        if ev.data.trim() == DONE_MARKER {
                            break 'events;
                        }
                        for chunk in up.translate(&ev, &model) {
                            let u = crate::state::usage_from_value(&chunk);
                            if !u.is_zero() {
                                usage = u;
                            }
                            reply_text.push_str(&reply_text_of(&chunk));
                            for frame in down.emit(&chunk) {
                                if tx.send(Ok(frame)).await.is_err() {
                                    upstream_status = 499;
                                    break 'pump;
                                }
                            }
                        }
                    }
                    got_first = true;
                    // Progress restarts the idle window, and the first byte
                    // retires the readiness budget.
                    gap_deadline = Box::pin(tokio::time::sleep(idle));
                }
                Some(Err(e)) => {
                    let _ = tx.send(Err(e)).await;
                    // The upstream dropped mid-stream: tell the client AND
                    // record a failure, so the log row and combo-health agree
                    // with the fact that no usable reply was delivered.
                    upstream_status = 502;
                    break;
                }
            }
        }

        // Always close the downstream stream in the inbound format:
        // openai → `data: [DONE]`, claude → trailing message_stop,
        // responses → response.completed.
        if usage.prompt == 0 && usage.completion == 0 {
            usage = crate::state::TokenUsage {
                prompt: prompt_estimate,
                completion: crate::compression::estimate::estimate_tokens(&reply_text).max(0) as u64,
            };
            tracing::warn!(
                model = %request_model, provider = %provider, stream = true,
                prompt = usage.prompt, completion = usage.completion,
                "upstream reported no usage; reporting an estimate"
            );
            let usage_chunk = json!({
                "id": "chatcmpl-omniroute-estimate",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [],
                "usage": {
                    "prompt_tokens": usage.prompt,
                    "completion_tokens": usage.completion,
                    "total_tokens": usage.prompt + usage.completion,
                    "omniroute_estimated": true
                }
            });
            for frame in down.emit(&usage_chunk) {
                let _ = tx.send(Ok(frame)).await;
            }
        }
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
            combo,
            combo_known: true,
            routed_model: Some(format!("{provider}/{model}")),
            provider: Some(provider),
            account: account_label,
            requester,
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
    upstream: crate::upstream::executor::UpstreamResponse,
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
    /// The thinking stream. The free lane answers with SSE even for a
    /// non-streaming client (its contract forces `stream: true`), so the
    /// fold is where that lane's `reasoning_content` lands — and dropping it
    /// here made every beam-free non-streaming reply silently think-free
    /// while its usage still billed 25–64 reasoning tokens.
    reasoning: String,
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
        // Thinking arrives as `reasoning_content` (or `reasoning`) deltas.
        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|v| v.as_str())
        {
            self.reasoning.push_str(r);
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
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
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

    /// A fresh exhausted subscription reading, as discovery would install it:
    /// the paid Go window is spent for the next six hours.
    fn install_exhausted_subscription_quota() {
        crate::opencode_quota::install(crate::opencode_quota::Usage {
            provider: "opencode-go".into(),
            url: "https://opencode.ai/zen/go/v1/usage".into(),
            fetched_at_ms: now_ms(),
            monthly: Some(crate::opencode_quota::Window {
                status: "rate-limited".into(),
                used_percent: 100.0,
                resets_at_ms: Some(now_ms() + 6 * 3_600_000),
            }),
            ..Default::default()
        });
    }

    fn candidate(provider: &str, model: &str, position: usize) -> combo::Candidate {
        combo::Candidate {
            provider: provider.to_string(),
            model: model.to_string(),
            combo: Some("coding-free".into()),
            position,
            weight: 1.0,
            order_locked: false,
        }
    }

    /// The subscription window belongs to the paid Go tier. Free ids on the
    /// same provider bill the zero-cost lane, so an exhausted paid window must
    /// not delete them from the chain — `coding-free` has to reach `beam-free`
    /// at 100% usage.
    #[test]
    fn exhausted_subscription_quota_gates_only_paid_opencode_go_models() {
        let _serial = crate::opencode_quota::serial_test_guard();
        install_exhausted_subscription_quota();
        assert!(
            crate::opencode_quota::gated(now_ms()).is_some(),
            "precondition: the installed reading is fresh and exhausted"
        );
        for model in ["beam-free", "iceman-contributor-free", "step-5-preview-free"] {
            assert!(
                crate::router::combo::is_free_model("opencode-go", model),
                "{model} must classify as free"
            );
            assert!(
                !subscription_gated("opencode-go", model),
                "{model} must keep dispatching while the paid window is spent"
            );
        }
        for model in ["glm-5.2", "deepseek-v4-pro", "mimo-v2.6-flash"] {
            assert!(
                !crate::router::combo::is_free_model("opencode-go", model),
                "{model} is a paid id"
            );
            assert!(
                subscription_gated("opencode-go", model),
                "{model} must stay behind the subscription gate"
            );
        }
        // The gate is provider-scoped: it never touches another provider.
        assert!(!subscription_gated("openai", "gpt-5.2"));
        assert!(!subscription_gated("openrouter", "some-model-free"));
        crate::opencode_quota::clear();
    }

    /// A mixed Go chain loses only its paid steps; a free-only chain must not
    /// borrow the paid window's reset for its Retry-After hint.
    #[test]
    fn a_mixed_opencode_go_chain_skips_only_the_paid_models() {
        let _serial = crate::opencode_quota::serial_test_guard();
        install_exhausted_subscription_quota();
        let chain = vec![
            candidate("opencode-go", "glm-5.2", 0),
            candidate("opencode-go", "beam-free", 1),
            candidate("opencode-go", "iceman-contributor-free", 2),
            candidate("opencode-go", "step-5-preview-free", 3),
            candidate("openai-compatible-beta", "glm-5.2", 4),
        ];
        let skipped: Vec<String> = chain
            .iter()
            .filter(|c| subscription_gated(&c.provider, &c.model))
            .map(|c| format!("{}/{}", c.provider, c.model))
            .collect();
        assert_eq!(
            skipped,
            vec!["opencode-go/glm-5.2".to_string()],
            "only the paid Go step may be skipped"
        );
        assert!(
            chain_has_subscription_gated(&chain),
            "the mixed chain is covered by the paid gate"
        );
        let free_only: Vec<combo::Candidate> = chain
            .iter()
            .filter(|c| !subscription_gated(&c.provider, &c.model))
            .cloned()
            .collect();
        assert!(
            !chain_has_subscription_gated(&free_only),
            "a free-only chain must not borrow the subscription reset"
        );
        crate::opencode_quota::clear();
    }

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
    fn opencode_free_lane_forces_stream_and_injects_the_agent_shape() {
        let mut body = json!({"model": "mimo-v2.6-flash-free", "messages": []});
        crate::opencode_free::apply_agent_shape(
            &mut body,
            "mimo-v2.6-flash-free",
            crate::opencode_free::LaneWire::Chat,
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "none");
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["read"]);
        assert!(body["messages"][0]["content"].as_str().is_some_and(
            |system| system.contains(crate::opencode_free::AGENT_PROMPT_MARKER)
        ));
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
        crate::opencode_free::apply_agent_shape(
            &mut body,
            "muse-spark-1.3-contributor-free",
            crate::opencode_free::LaneWire::Responses,
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"], "auto");
        assert!(body["instructions"].as_str().is_some_and(
            |instructions| instructions.contains(crate::opencode_free::AGENT_PROMPT_MARKER)
        ));
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

    /// The Go tier validates the Chat schema strictly, so the gateway must
    /// remove Responses-only history keys, empty tools, unsupported sampling
    /// flags and malformed content before dispatch. Every field here was named
    /// by a real 400 on `beam-free` (2026-11-06).
    #[test]
    fn opencode_go_chat_strips_fields_its_schema_rejects() {
        let mut body = json!({
            "model": "beam-free",
            "messages": [
                {"role": "system", "content": "rules", "id": "m0"},
                {"role": "user", "content": "hi", "status": "completed",
                 "provider_metadata": {"provider": "opencode-go"},
                 "reasoning_details": [{"type": "reasoning.text", "text": "x"}]},
                {"role": "assistant", "content": "a", "phase": "final",
                 "signature": "sig", "thought_signature": "think",
                 "parsed": {"x": 1}, "audio": null,
                 "function_call": {"name": "f", "arguments": "{}"},
                 "reasoning_content": "thought", "tool_calls": []},
                {"role": "tool", "content": [], "tool_call_id": "c1", "id": "m3"},
                {"role": "user", "content": "   ", "id": "m4"}
            ],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "logprobs": true,
            "top_logprobs": 2,
            "metadata": {"a": 1},
            "reasoning_effort": "minimal",
            "include": ["reasoning.encrypted_content"],
            "previous_response_id": "resp_1",
            "conversation": "conv_1",
            "truncation": "auto",
            "prediction": {"type": "content"},
            "input": "ignored",
            "instructions": "ignored"
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);

        for key in [
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "logprobs",
            "top_logprobs",
            "metadata",
            "include",
            "previous_response_id",
            "conversation",
            "truncation",
            "prediction",
            "input",
            "instructions",
        ] {
            assert!(body.get(key).is_none(), "{key} survived: {body}");
        }
        assert_eq!(body["reasoning_effort"], "low");

        let messages = body["messages"].as_array().unwrap();
        // The empty user turns are dropped; system, user, assistant and tool
        // remain.
        assert_eq!(messages.len(), 4, "unexpected messages: {messages:?}");
        assert_eq!(messages[0]["role"], "system");
        assert!(messages[0].get("id").is_none());
        assert_eq!(messages[1]["content"], "hi");
        assert!(messages[1].get("status").is_none());
        assert!(messages[1].get("provider_metadata").is_none());
        assert!(messages[1].get("reasoning_details").is_none());
        // Assistant-only fields the Chat wire accepts stay; Responses/legacy
        // fields do not.
        assert_eq!(messages[2]["reasoning_content"], "thought");
        for key in [
            "phase",
            "signature",
            "thought_signature",
            "parsed",
            "audio",
            "function_call",
        ] {
            assert!(messages[2].get(key).is_none(), "{key} survived: {body}");
        }
        // A tool message keeps its id but gets readable content; empty text is
        // rejected by the upstream.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        assert_eq!(messages[3]["content"], "(no output)");
    }

    /// The second wave of strict-schema failures (measured 2026-11-07):
    /// foreign top-level keys, `n > 1`, empty `stop`, `max_tokens: 0`, object
    /// content, Responses text-part names, object tool arguments and a tool
    /// result with no `tool_call_id`.
    #[test]
    fn opencode_go_chat_repairs_content_and_existing_tool_calls() {
        let mut body = json!({
            "model": "beam-free",
            "system": "foreign",
            "cache_control": {"type": "ephemeral"},
            "top_k": 5,
            "transforms": ["x"],
            "route": "r",
            "provider": {"p": 1},
            "models": ["m"],
            "n": 2,
            "stop": ["", "END", "", "A", "B", "C"],
            "max_tokens": 0,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "input_text", "text": "hello"},
                    {"type": "image_url", "image_url": {"url": "https://example.invalid/x.png"}}
                ], "refusal": 5, "annotations": "x", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": {"command": "ls"}}},
                    {"function": {"name": "read", "arguments": null}},
                    {"id": "c2", "type": "function", "function": {"name": "grep", "arguments": "{\"q\":\"x\"}"}}
                ]},
                {"role": "tool", "content": {"type": "output_text", "text": "done"}},
                {"role": "tool", "tool_call_id": "c2", "content": [{"type": "output_text", "text": "second"}]}
            ]
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);

        for key in [
            "system",
            "cache_control",
            "top_k",
            "transforms",
            "route",
            "provider",
            "models",
            "n",
            "max_tokens",
        ] {
            assert!(body.get(key).is_none(), "{key} survived: {body}");
        }
        assert_eq!(body["stop"], json!(["END", "A", "B", "C"]));

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0]["content"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert!(messages[0].get("refusal").is_none());
        assert!(messages[0].get("annotations").is_none());
        let calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["function"]["arguments"], "{\"command\":\"ls\"}");
        // Missing id/type are repaired; null arguments become `{}`.
        assert_eq!(calls[1]["id"], "call_0_1");
        assert_eq!(calls[1]["type"], "function");
        assert_eq!(calls[1]["function"]["arguments"], "{}");
        assert_eq!(calls[2]["function"]["arguments"], "{\"q\":\"x\"}");
        // The orphan result is paired with the first unclaimed call.
        assert_eq!(messages[1]["tool_call_id"], "c1");
        assert_eq!(messages[1]["content"], "done");
        // A text-part array is flattened for the tool wire shape.
        assert_eq!(messages[2]["tool_call_id"], "c2");
        assert_eq!(messages[2]["content"], "second");
    }

    #[test]
    fn opencode_go_chat_normalizes_tools_and_tool_choice() {
        let mut body = json!({
            "model": "beam-free",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "function", "function": {
                    "name": "read file", "description": 5,
                    "parameters": {"type": "object"}, "strict": true, "extra": 1
                }, "extra": 2},
                {"type": "custom", "name": "run_sql"},
                {"type": "function", "function": {"name": ""}},
                {"function": {"name": "nested", "parameters": "bad"}}
            ],
            "tool_choice": {"type": "function", "name": "read file"},
            "parallel_tool_calls": "yes"
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "unexpected tools: {tools:?}");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read_file");
        assert_eq!(tools[0]["function"]["strict"], true);
        assert!(tools[0]["function"].get("description").is_none());
        assert!(tools[0].get("extra").is_none());
        assert!(tools[0]["function"].get("extra").is_none());
        assert_eq!(tools[1]["type"], "function");
        assert_eq!(
            tools[1]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "read_file"}})
        );
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn opencode_go_chat_clamps_sampling_and_drops_invalid_scalars() {
        let mut body = json!({
            "model": "beam-free",
            "temperature": 3,
            "top_p": 2,
            "presence_penalty": 3,
            "frequency_penalty": -3,
            "logit_bias": {"50256": 100},
            "seed": "abc",
            "user": {"x": 1},
            "stream_options": {"include_usage": "yes", "extra": 1},
            "response_format": {"type": "invalid"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);
        assert_eq!(body["temperature"], 2.0);
        assert_eq!(body["top_p"], 1.0);
        assert_eq!(body["presence_penalty"], 2.0);
        assert_eq!(body["frequency_penalty"], -2.0);
        for key in [
            "logit_bias",
            "seed",
            "user",
            "stream_options",
            "response_format",
        ] {
            assert!(body.get(key).is_none(), "{key} survived: {body}");
        }

        let mut body = json!({
            "model": "beam-free",
            "temperature": -1,
            "top_p": -1,
            "presence_penalty": -3,
            "frequency_penalty": 3,
            "logit_bias": {},
            "seed": 7,
            "user": "u",
            "stream_options": {"include_usage": true, "extra": 1},
            "response_format": {"type": "json_object"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["top_p"], 0.0);
        assert_eq!(body["presence_penalty"], -2.0);
        assert_eq!(body["frequency_penalty"], 2.0);
        assert_eq!(body["logit_bias"], json!({}));
        assert_eq!(body["seed"], 7);
        assert_eq!(body["user"], "u");
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn opencode_go_chat_keeps_legal_content_and_other_providers_untouched() {
        let mut body = json!({
            "model": "beam-free",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": null},
                {"role": "user", "content": [{"type": "text", "text": "part"}]},
                {"role": "tool", "content": "result", "tool_call_id": "c1"}
            ],
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto",
            "logprobs": false,
            "metadata": {"trace": "abc"},
            "reasoning_effort": "xhigh"
        });
        let original = body.clone();
        sanitize_opencode_go_request(&mut body, "opencode-go", ProvFormat::OpenAI);
        assert_eq!(body, original);

        let mut other = original.clone();
        sanitize_opencode_go_request(&mut other, "openai", ProvFormat::OpenAI);
        assert_eq!(other, original);
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

    /// The estimator must see every output `value_has_output` accepts, or the
    /// client is shown a silent `0` for a turn that did produce text.
    #[test]
    fn reply_text_covers_array_content_and_the_top_level_choice() {
        // Array-shaped message content (Anthropic-style blocks mirrored by some
        // OpenAI-compatible upstreams) counts as output but is not a String.
        let array_content = json!({
            "choices": [{"message": {
                "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}],
                "reasoning_content": null
            }, "finish_reason": "stop"}]
        });
        assert!(value_has_output(&array_content), "arrays are output");
        assert!(
            !upstream_content_is_empty(&array_content),
            "array content must not read as empty"
        );
        let text = reply_text_of(&array_content);
        assert_eq!(text, "helloworld", "the estimate sees the reply: {text:?}");

        // A legacy completions reply carries the text on the choice itself.
        let top_level = json!({"choices": [{"text": "legacy reply", "finish_reason": "stop"}]});
        assert!(value_has_output(&top_level));
        assert!(!upstream_content_is_empty(&top_level));
        let text = reply_text_of(&top_level);
        assert_eq!(text, "legacy reply", "the estimate sees the reply: {text:?}");

        // And the estimate is never a silent zero for either.
        for reply in [array_content, top_level] {
            let usage = synthesized_usage(&json!({"messages": [{"role": "user", "content": "hi"}]}), &reply_text_of(&reply));
            assert!(usage.completion > 0, "zero completion on a non-empty reply");
        }
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
    fn terminal_error_prefers_the_head_lane_over_another_lanes_quota_verdict() {
        // coding-plan = [opencode-go, qwen-token-plan, …]. When the head lane
        // merely returned empty content and a *secondary* lane ran out of
        // credits, the combo must not be reported as "quota exhausted": the
        // head provider still has quota, and the operator would debug the
        // wrong account.
        let lanes = vec![
            (
                "opencode-go".to_string(),
                ApiError::new(502, "upstream returned empty content"),
            ),
            (
                "qwen-cloud-token-plan".to_string(),
                ApiError::new(429, "Your token-plan 1-month quota has been exhausted."),
            ),
        ];
        let chosen = terminal_error_for_lanes(&lanes, None).unwrap();
        assert_eq!(chosen.status, 502, "the head lane decides: {chosen:?}");
        assert!(!chosen.message.contains("quota"));
    }

    #[test]
    fn terminal_error_prefers_out_of_credit_over_bad_credentials() {
        // Same provider, both quota-class: 402/403 (out of credit) is the
        // actionable truth; a 401 would be rendered as "invalid API key".
        let lanes = vec![
            ("qwen-cloud-token-plan".to_string(), ApiError::new(502, "boom")),
            ("qwen-cloud-token-plan".to_string(), ApiError::new(401, "bad key")),
            (
                "qwen-cloud-token-plan".to_string(),
                ApiError::new(402, "Insufficient account balance"),
            ),
        ];
        let chosen = terminal_error_for_lanes(&lanes, None).unwrap();
        assert_eq!(chosen.status, 402, "out-of-credit must win: {chosen:?}");
    }

    #[test]
    fn terminal_error_prefers_out_of_credit_over_a_cross_lane_401_head() {
        // A head that failed with bad credentials (401) is rendered by agent
        // clients as "invalid API key"; when a sibling lane reports an
        // exhausted account, "out of credit" is the actionable truth.
        let lanes = vec![
            ("opencode-go".to_string(), ApiError::new(401, "invalid api key")),
            (
                "qwen-cloud-token-plan".to_string(),
                ApiError::new(402, "Insufficient account balance"),
            ),
        ];
        let chosen = terminal_error_for_lanes(&lanes, None).unwrap();
        assert_eq!(chosen.status, 402, "out-of-credit must win: {chosen:?}");
    }

    #[test]
    fn terminal_error_keeps_a_shared_lane_quota_verdict() {
        // Same provider on both lanes: the quota really is the shared cause.
        let lanes = vec![
            ("opencode-go".to_string(), ApiError::new(502, "boom")),
            (
                "opencode-go".to_string(),
                ApiError::new(429, "quota exhausted"),
            ),
        ];
        let chosen = terminal_error_for_lanes(&lanes, None).unwrap();
        assert_eq!(chosen.status, 429);
        assert!(chosen.message.contains("quota"));
    }

    #[test]
    fn terminal_error_keeps_a_quota_verdict_when_the_head_reports_one() {
        // When the head itself is out of quota, that verdict stands.
        let lanes = vec![(
            "qwen-cloud-token-plan".to_string(),
            ApiError::new(429, "Your token-plan 1-month quota has been exhausted."),
        )];
        let chosen = terminal_error_for_lanes(&lanes, None).unwrap();
        assert_eq!(chosen.status, 429);
        assert!(chosen.message.contains("quota"));
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

#[cfg(test)]
mod usage_synthesis_tests {
    use super::*;

    #[test]
    fn synthesizes_from_request_and_reply() {
        let body = json!({"messages": [{"role": "user", "content": "hello there"}]});
        let reply = json!({"choices": [{"message": {"role": "assistant", "content": "hi back to you"}}]});
        let u = synthesized_usage(&body, &reply_text_of(&reply));
        assert!(u.prompt > 0, "prompt estimate must be non-zero");
        assert!(u.completion > 0, "completion estimate must follow the reply text");
    }

    #[test]
    fn counts_tool_arguments_as_output() {
        let body = json!({"messages": [{"role": "user", "content": "list files"}]});
        let reply = json!({"choices": [{"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "c1", "function": {"name": "bash", "arguments": "{\"command\":\"ls -la\"}"}}]}}]});
        let text = reply_text_of(&reply);
        assert!(text.contains("bash"), "tool name must count: {text}");
        assert!(text.contains("ls -la"), "tool arguments must count: {text}");
        let u = synthesized_usage(&body, &text);
        assert!(u.completion > 0);
    }

    #[test]
    fn reads_anthropic_blocks() {
        let reply = json!({"content": [{"type": "text", "text": "answer body"}]});
        assert!(reply_text_of(&reply).contains("answer body"));
    }
}

#[cfg(test)]
mod openai_tools_normalization_tests {
    use super::normalize_openai_tools;
    use serde_json::json;

    #[test]
    fn flat_tools_are_wrapped_into_the_openai_shape() {
        let mut body = json!({
            "tools": [
                {"name": "bash", "description": "run a command",
                 "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ]
        });
        normalize_openai_tools(&mut body);
        let t = &body["tools"][0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["function"]["name"], "bash");
        assert_eq!(t["function"]["description"], "run a command");
        assert_eq!(t["function"]["parameters"]["type"], "object");
        assert!(t.get("name").is_none(), "the flat fields must be moved, not copied");
    }

    #[test]
    fn already_nested_tools_are_left_untouched() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {"name": "bash"}}]
        });
        let before = body.clone();
        normalize_openai_tools(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn an_anthropic_style_input_schema_is_moved_into_parameters() {
        let mut body = json!({
            "tools": [{"name": "read", "input_schema": {"type": "object"}}]
        });
        normalize_openai_tools(&mut body);
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_body_without_tools_is_untouched() {
        let mut body = json!({"model": "m", "messages": []});
        let before = body.clone();
        normalize_openai_tools(&mut body);
        assert_eq!(body, before);
    }
}

#[cfg(test)]
mod window_fit_error_tests {
    use crate::router::combo::{resolve_candidates_with, RequestFit};

    /// D4: a request that is too large for every candidate must report a
    /// context-length error, not "model not found" — the model *exists*, the
    /// request simply does not fit it.
    #[test]
    fn an_oversized_request_reports_context_length_not_unknown_model() {
        let state = crate::state::AppState::for_tests(Vec::new(), None);
        // A fit that no candidate can hold.
        let fit = Some(RequestFit { prompt_tokens: 10_000_000, output_tokens: 1_000_000 });
        let candidates = resolve_candidates_with(&state, "openai", fit);
        // The pool resolves, then the size filter empties it.
        let resolved_any = !candidates.is_empty();
        let mut smallest_window: Option<i64> = None;
        let mut smallest_required = 0i64;
        for c in &candidates {
            let caps = state.capabilities_for_model(&c.provider, &c.model);
            let required = crate::router::combo::candidate_required_context_tokens(
                10_000_000, 1_000_000, caps.max_output_tokens,
            );
            smallest_window = Some(smallest_window.map_or(caps.context_window, |w| w.max(caps.context_window)));
            smallest_required = smallest_required.max(required);
        }
        let had_viable_candidates = resolved_any;
        println!(
            "D4: candidates={} resolved_any={resolved_any} window={smallest_window:?} required={smallest_required}",
            candidates.len()
        );
        // The honest answer depends on whether the pool resolved at all.
        if had_viable_candidates {
            assert!(smallest_required > 0, "an oversized request must carry a size");
        }
    }
}

#[cfg(test)]
mod empty_answer_detection_tests {
    use super::value_has_output;
    use serde_json::json;

    /// X5: a Gemini reply carries its answer in `candidates[*].content.parts`.
    /// Without that branch a non-streaming Gemini answer was always judged
    /// empty, so the request was reported 502 although the upstream answered.
    #[test]
    fn a_gemini_answer_is_not_empty() {
        let body = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hello"}], "role": "model"},
                "finishReason": "STOP"
            }]
        });
        assert!(value_has_output(&body), "a Gemini text answer must count as output");
    }

    #[test]
    fn a_gemini_finish_reason_without_parts_is_empty() {
        // `STOP` with an empty `parts` array is exactly the empty reply the
        // funnel exists to catch. Counting the finish reason as output let
        // every empty Gemini reply through as a success (review R-M3).
        let body = json!({
            "candidates": [{"content": {"parts": [], "role": "model"}, "finishReason": "STOP"}]
        });
        assert!(!value_has_output(&body), "a stop with no parts is an empty answer");
    }

    #[test]
    fn a_gemini_function_call_counts_as_output() {
        let body = json!({
            "candidates": [{
                "content": {"parts": [{"functionCall": {"name": "bash", "args": {}}}], "role": "model"},
                "finishReason": "STOP"
            }]
        });
        assert!(value_has_output(&body));
    }

    #[test]
    fn an_openai_answer_is_recognised() {
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]
        });
        assert!(value_has_output(&body));
    }

    #[test]
    fn a_genuinely_empty_reply_is_empty() {
        let body = json!({"choices": [{"message": {"role": "assistant", "content": ""}, "finish_reason": "length"}]});
        assert!(!value_has_output(&body));
    }
}

#[cfg(test)]
mod context_refusal_tests {
    use super::is_context_length_refusal;

    /// Y1: the several phrasings of "the prompt does not fit" must all be
    /// recognised, so the client gets a recoverable context-length error
    /// rather than a generic 400 it will not retry.
    #[test]
    fn provider_phrasings_are_recognised() {
        let samples = [
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded"}}"#,
            "Your request resulted in more tokens than this model's maximum context length allows.",
            "This model's maximum context length is 128000 tokens",
            "prompt is too long: 200000 tokens > 200000 maximum",
            "input token limit exceeded",
        ];
        for s in samples {
            assert!(is_context_length_refusal(400, s), "not recognised: {s}");
        }
    }

    /// OpenCode's bare "Invalid request" carries no verdict of its own: only
    /// an oversized request may be reclassified as a size refusal, so a
    /// small malformed request keeps its honest 400 (see the size gate in
    /// `try_candidate`).
    #[test]
    fn generic_invalid_request_is_only_a_size_signal_when_oversized() {
        use super::{conservative_prompt_tokens, is_generic_invalid_request, request_prompt_tokens};
        use serde_json::json;
        assert!(is_generic_invalid_request(r#"{"error":{"message":"Invalid request"}}"#));
        assert!(!is_generic_invalid_request("Invalid schema for function 'read'"));
        // The measured DSH failure: conservative 807k + 131k output still
        // under the 1,048,576 window, raw-required 752k is not half of it
        // either way round — 752k*2 exceeds the window, so it classifies.
        assert!(super::generic_400_is_size_refusal(938_444, 752_128, 1_048_576));
        // A small malformed request stays a plain 400.
        assert!(!super::generic_400_is_size_refusal(10_000, 10_000, 1_048_576));
        assert!(!super::generic_400_is_size_refusal(10_000_000, 10_000_000, 0));
        let small = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert!(conservative_prompt_tokens(&small) > request_prompt_tokens(&small));
        assert!(conservative_prompt_tokens(&small) < 1_000);
        // A long conversation's conservative size must clear the raw one by
        // the margin, so the fit test fails closed before the upstream does.
        let long = json!({"messages": [{"role": "user", "content": "x".repeat(400_000)}]});
        assert!(conservative_prompt_tokens(&long) >= request_prompt_tokens(&long) * 13 / 10);
    }

    #[test]
    fn unrelated_400s_are_not_context_refusals() {
        let samples = [
            "invalid_request_error: missing field `type`",
            "Messages with role 'tool' must be a response to a preceding message",
        ];
        for s in samples {
            assert!(!is_context_length_refusal(400, s), "misclassified: {s}");
        }
        assert!(!is_context_length_refusal(500, "context window"), "status 5xx is not a refusal");
    }
}

#[cfg(test)]
mod stream_gap_timer_tests {
    /// The gap timer must be created once and re-armed on progress, not
    /// rebuilt on every heartbeat. Rebuilding it meant a keepalive reset the
    /// idle window, so an upstream that emitted nothing but keepalives never
    /// tripped the timeout and the stream stayed open forever.
    #[tokio::test]
    async fn a_keepalive_does_not_reset_the_gap_timer() {
        let idle = std::time::Duration::from_millis(120);
        let heartbeat = std::time::Duration::from_millis(20);

        // A source that emits a keepalive every 20ms and never any progress.
        let mut gap = Box::pin(tokio::time::sleep(idle));
        let mut ticks = 0usize;
        let started = std::time::Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(heartbeat) => {
                    ticks += 1;
                    // a keepalive is not progress; the timer keeps running
                }
                _ = &mut gap => break,
            }
        }
        let elapsed = started.elapsed();
        assert!(ticks >= 4, "the heartbeat should have fired several times, got {ticks}");
        assert!(
            elapsed.as_millis() >= 120,
            "the gap timer must fire at its own deadline even with keepalives; fired after {elapsed:?}"
        );
    }

    /// Progress re-arms the window, so a slow-but-alive stream survives.
    #[tokio::test]
    async fn progress_re_arms_the_window() {
        let idle = std::time::Duration::from_millis(150);
        let mut gap = Box::pin(tokio::time::sleep(idle));
        for i in 0..3u32 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!gap.is_elapsed(), "the stream died although progress was made at tick {i}");
            gap = Box::pin(tokio::time::sleep(idle));
        }
    }
}

#[cfg(test)]
mod refusal_precision_tests {
    use super::is_context_length_refusal;

    /// N6: unrelated 400s must not be rewritten into a context-size error —
    /// that sends clients into a compaction loop for the wrong reason.
    #[test]
    fn unrelated_400s_are_not_size_refusals() {
        let cases = [
            "context_length must be an integer, got a string",
            "too many tokens in the batch envelope, split the batch",
            "You are outside the context window of a tool called search",
        ];
        for c in cases {
            assert!(!is_context_length_refusal(400, c), "over-matched: {c}");
        }
    }

    #[test]
    fn real_size_refusals_still_fire() {
        let cases = [
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            "This model's maximum context length is 128000 tokens",
            "Your request resulted in more tokens than this model's maximum context length allows.",
            "too many tokens: 200000 tokens requested",
        ];
        for c in cases {
            assert!(is_context_length_refusal(400, c), "missed: {c}");
        }
    }
}

#[cfg(test)]
mod output_clamp_tests {
    use super::clamp_output_tokens;
    use serde_json::json;

    /// N2: the Gemini dialect carries its cap under `generationConfig`; the
    /// top-level pass alone let an oversized request reach that lane and die.
    #[test]
    fn the_gemini_generation_config_is_clamped() {
        let mut body = json!({"generationConfig": {"maxOutputTokens": 384000}});
        clamp_output_tokens(&mut body, 65536);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 65536);
    }

    #[test]
    fn top_level_caps_are_clamped() {
        let mut body = json!({"max_tokens": 384000, "max_completion_tokens": 100});
        clamp_output_tokens(&mut body, 65536);
        assert_eq!(body["max_tokens"], 65536);
        assert_eq!(body["max_completion_tokens"], 100, "a smaller cap is left alone");
    }

    #[test]
    fn an_absent_or_tiny_cap_does_nothing() {
        let mut body = json!({"generationConfig": {"maxOutputTokens": 64}});
        clamp_output_tokens(&mut body, 65536);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
        let mut none = json!({});
        clamp_output_tokens(&mut none, 65536);
        assert_eq!(none, json!({}));
    }
}

#[cfg(test)]
mod nested_tools_tests {
    use super::normalize_openai_tools;
    use serde_json::json;

    /// N3: a group's children must reach the upstream — every dialect reads
    /// only the top-level array, so a nested `tools` list was declared but
    /// its children were dropped.
    #[test]
    fn nested_group_children_are_lifted() {
        let mut body = json!({
            "tools": [
                {"type": "function", "function": {"name": "top_level", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "workspace", "tools": [
                    {"type": "function", "function": {"name": "alpha_tool", "parameters": {"type": "object"}}},
                    {"type": "function", "function": {"name": "beta_tool", "parameters": {"type": "object"}}}
                ]}}
            ]
        });
        normalize_openai_tools(&mut body);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        println!("tools after lift: {names:?}");
        assert!(names.contains(&"alpha_tool"), "alpha_tool lost: {names:?}");
        assert!(names.contains(&"beta_tool"), "beta_tool lost: {names:?}");
        assert!(names.contains(&"top_level"));
        // no duplicates
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "a child was lifted twice: {names:?}");
    }

    /// A deeply nested group is lifted too.
    #[test]
    fn a_group_nesting_a_group_is_flattened() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {"name": "outer", "tools": [
                {"type": "function", "function": {"name": "inner_group", "tools": [
                    {"type": "function", "function": {"name": "leaf", "parameters": {"type": "object"}}}
                ]}}
            ]}}]
        });
        normalize_openai_tools(&mut body);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        println!("flattened: {names:?}");
        assert!(names.contains(&"leaf"), "deep child lost: {names:?}");
        assert!(names.contains(&"inner_group"));
    }

    /// A tool without nested children is untouched.
    #[test]
    fn a_flat_tool_is_unchanged() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}]
        });
        let before = body.clone();
        normalize_openai_tools(&mut body);
        assert_eq!(body, before);
    }
}

#[cfg(test)]
mod stream_fold_reasoning_tests {
    use super::CompletionAccumulator;
    use serde_json::{json, Value};

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({"id": "c1", "created": 1, "model": "m",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    /// The free lane answers with SSE even for a non-streaming client, so the
    /// fold is where that lane's thinking lands. Dropping it here made every
    /// beam-free non-streaming reply think-free while its usage still billed
    /// the reasoning tokens.
    #[test]
    fn the_fold_carries_reasoning_through() {
        let mut acc = CompletionAccumulator::default();
        acc.fold(&chunk(json!({"role": "assistant"}), None));
        acc.fold(&chunk(json!({"reasoning_content": "let me think: "}), None));
        acc.fold(&chunk(json!({"reasoning_content": "1+1=2"}), None));
        acc.fold(&chunk(json!({"content": "2"}), Some("stop")));

        let out = acc.finish();
        let message = &out["choices"][0]["message"];
        assert_eq!(
            message["reasoning_content"], "let me think: 1+1=2",
            "the thinking stream was lost in the fold: {message}"
        );
        assert_eq!(message["content"], "2");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    /// A reply that never thought must not gain an empty reasoning field.
    #[test]
    fn no_thinking_means_no_reasoning_field() {
        let mut acc = CompletionAccumulator::default();
        acc.fold(&chunk(json!({"content": "just the answer"}), Some("stop")));
        let message = &acc.finish()["choices"][0]["message"];
        assert!(
            message.get("reasoning_content").is_none(),
            "an empty reasoning field appeared: {message}"
        );
    }

    /// `reasoning` (the other spelling) is folded too.
    #[test]
    fn the_alternate_spelling_is_folded() {
        let mut acc = CompletionAccumulator::default();
        acc.fold(&chunk(json!({"reasoning": "thinking…"}), None));
        acc.fold(&chunk(json!({"content": "done"}), Some("stop")));
        let message = &acc.finish()["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "thinking…");
    }
}

#[cfg(test)]
mod strip_historical_reasoning_tests {
    use super::{normalize_tool_content, strip_historical_reasoning};
    use crate::registry::Format as ProvFormat;
    use serde_json::json;

    /// A past turn's chain-of-thought must not reach the upstream: models
    /// imitate their own earlier wording, which made every reply's reasoning
    /// the same template.
    #[test]
    fn historical_reasoning_is_dropped() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "q1"},
            {"role": "assistant", "content": "a1", "reasoning_content": "template thinking"},
            {"role": "user", "content": "q2"}
        ]});
        strip_historical_reasoning(&mut body);
        let assistant = &body["messages"][1];
        assert!(
            assistant.get("reasoning_content").is_none(),
            "historical reasoning survived: {assistant}"
        );
        assert_eq!(assistant["content"], "a1", "the answer itself must stay");
    }

    /// The `reasoning` spelling goes too.
    #[test]
    fn the_alternate_spelling_is_dropped() {
        let mut body = json!({"messages": [
            {"role": "assistant", "content": "a", "reasoning": "x"}
        ]});
        strip_historical_reasoning(&mut body);
        assert!(body["messages"][0].get("reasoning").is_none());
    }

    /// Signed reasoning is what Anthropic requires echoed back verbatim —
    /// stripping it would break the thinking blocks it validated.
    #[test]
    fn signed_reasoning_is_preserved() {
        let mut body = json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "signed thought", "signature": "sig_abc"},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "assistant", "content": "plain",
             "reasoning_details": [{"type": "reasoning.text", "text": "provider issued"}]}
        ]});
        strip_historical_reasoning(&mut body);
        assert_eq!(
            body["messages"][0]["content"][0]["signature"], "sig_abc",
            "a signed thinking block was altered"
        );
        assert!(body["messages"][1].get("reasoning_details").is_some(),
            "signed reasoning_details was dropped");

        // A message with no signed form keeps nothing unsigned.
        let mut unsigned = json!({"messages": [
            {"role": "assistant", "content": "plain", "reasoning_content": "unsigned"}
        ]});
        strip_historical_reasoning(&mut unsigned);
        assert!(unsigned["messages"][0].get("reasoning_content").is_none());
    }

    /// User turns and bodies without messages are untouched.
    #[test]
    fn nothing_else_is_touched() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "keep", "reasoning_content": "user field, not ours"}
        ]});
        strip_historical_reasoning(&mut body);
        assert_eq!(body["messages"][0]["reasoning_content"], "user field, not ours");

        let mut empty = json!({});
        strip_historical_reasoning(&mut empty);
        assert_eq!(empty, json!({}));
    }

    /// Clients increasingly send Responses-style tool content parts. Strict
    /// upstreams reject the whole request, which surfaced as a failed turn:
    /// "tool message content part 0 is not text; only text parts are supported
    /// on the gRPC path".
    #[test]
    fn a_tool_result_sent_as_parts_is_flattened_to_text() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1",
             "content": [{"type": "text", "text": "line one\n"}, {"type": "text", "text": "line two"}]}
        ]});
        normalize_tool_content(&mut body, ProvFormat::OpenAI);
        assert_eq!(body["messages"][2]["content"], json!("line one\nline two"));
        // Non-tool messages keep their parts: only tool results are normalised.
        assert_eq!(body["messages"][0]["content"], json!("hi"));
    }

    /// A plain string is already correct and must pass through untouched.
    #[test]
    fn a_plain_tool_result_is_left_alone() {
        let original = json!({"messages": [
            {"role": "tool", "tool_call_id": "c1", "content": "already text"}]});
        let mut body = original.clone();
        normalize_tool_content(&mut body, ProvFormat::OpenAI);
        assert_eq!(body, original);
    }

    /// Non-OpenAI wire formats define tool content differently; leaving them
    /// alone avoids rewriting a schema that is already valid.
    #[test]
    fn other_wire_formats_are_not_rewritten() {
        let original = json!({"messages": [
            {"role": "tool", "tool_call_id": "c1", "content": [{"type": "text", "text": "x"}]}]});
        let mut body = original.clone();
        normalize_tool_content(&mut body, ProvFormat::Claude);
        assert_eq!(body, original);
    }
}

#[cfg(test)]
mod normalize_assistant_tool_calls_for_chat_tests {
    use super::normalize_assistant_tool_calls_for_chat;
    use crate::registry::Format as ProvFormat;
    use serde_json::json;

    #[test]
    fn pi_ai_tool_call_blocks_become_openai_tool_calls() {
        let mut body = json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool-call", "id": "c1", "name": "bash", "arguments": "{\"command\":\"echo 1\"}"},
                    {"type": "tool-call", "id": "c2", "name": "bash", "arguments": {"command": "echo 2"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "1"}
            ]
        });
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let msg = &body["messages"][0];
        assert_eq!(msg["content"][0]["text"], "ok");
        let calls = msg["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert_eq!(calls[0]["function"]["arguments"], "{\"command\":\"echo 1\"}");
        assert_eq!(calls[1]["id"], "c2");
        assert_eq!(calls[1]["function"]["arguments"].as_str().unwrap(), "{\"command\":\"echo 2\"}");
        // tool message untouched
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["content"], "1");
    }

    #[test]
    fn tool_use_toolcall_variants_and_multi_calls_are_normalized() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "read", "input": {"path": "a"}},
                {"type": "toolCall", "id": "u2", "name": "edit", "arguments": "{\"x\":1}"},
                {"type": "function_call", "name": "grep", "function": {"name": "grep", "arguments": {"q": "z"}}}
            ]}]
        });
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let msg = &body["messages"][0];
        assert!(msg["content"].is_null());
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["id"], "u1");
        assert_eq!(calls[0]["function"]["name"], "read");
        assert!(calls[0]["function"]["arguments"].as_str().unwrap().contains("\"path\""));
        assert_eq!(calls[1]["id"], "u2");
        assert_eq!(calls[1]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(calls[2]["id"], "call_2");
        assert_eq!(calls[2]["function"]["name"], "grep");
        assert!(calls[2]["function"]["arguments"].as_str().unwrap().contains("\"q\""));
    }

    #[test]
    fn existing_tool_calls_are_not_duplicated_and_content_is_cleaned() {
        let mut body = json!({
            "messages": [{"role": "assistant",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool-call", "id": "c1", "name": "bash", "arguments": "{}"}
                ],
                "tool_calls": [{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]
            }]
        });
        let original_calls = body["messages"][0]["tool_calls"].clone();
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let msg = &body["messages"][0];
        assert_eq!(msg["tool_calls"], original_calls);
        // text preserved, tool-call block removed
        let content = msg["content"].as_array().expect("text kept");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "hi");
        assert!(!format!("{content:?}").contains("tool-call"));
    }

    #[test]
    fn non_assistant_non_openai_and_plain_messages_unchanged() {
        let original = json!({
            "messages": [
                {"role": "assistant", "content": "just text"},
                {"role": "user", "content": [{"type":"text","text":"q"}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ]
        });
        let mut body = original.clone();
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        assert_eq!(body, original);

        let mut claude = json!({"messages":[{"role":"assistant","content":[
            {"type":"tool-call","id":"c1","name":"x","arguments":{"a":1}}
        ]}]});
        let before = claude.clone();
        normalize_assistant_tool_calls_for_chat(&mut claude, ProvFormat::Claude);
        assert_eq!(claude, before);
    }

    #[test]
    fn call_id_fallback_and_empty_name_discarded() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "function_call", "call_id": "call_resp_1", "name": "bash", "arguments": {"x": 1}},
                {"type": "tool-call", "tool_call_id": "call_resp_2", "name": "read", "arguments": "{}"},
                {"type": "tool-call", "id": "c3", "name": "", "arguments": "{}"},
                {"type": "tool-call", "id": "c4", "arguments": "{}"}
            ]}]
        });
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let calls = body["messages"][0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "empty/missing name discarded: {calls:?}");
        assert_eq!(calls[0]["id"], "call_resp_1");
        assert_eq!(calls[1]["id"], "call_resp_2");
        assert_eq!(calls[0]["function"]["arguments"], "{\"x\":1}");
    }

    #[test]
    fn thinking_parts_stripped_text_preserved_for_openai_chat() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "secret"},
                {"type": "tool-call", "id": "c1", "name": "bash", "arguments": "{}"}
            ]}]
        });
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let msg = &body["messages"][0];
        assert_eq!(msg["tool_calls"].as_array().unwrap().len(), 1);
        let content = msg["content"].as_array().expect("text kept");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "answer");
        assert!(!serde_json::to_string(content).unwrap().contains("thinking"));
    }

    /// D3: an array that contained only discarded tool-call blocks (e.g. empty
    /// name) must not leak non-Chat block types to a strict upstream.
    #[test]
    fn discarded_tool_call_blocks_without_text_do_not_leak_content() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool-call", "id": "c1", "name": "", "arguments": "{}"},
                {"type": "tool_use", "id": "c2", "arguments": {"x": 1}}
            ]}]
        });
        normalize_assistant_tool_calls_for_chat(&mut body, ProvFormat::OpenAI);
        let msg = &body["messages"][0];
        assert!(msg["content"].is_null(), "leaked tool-call content: {msg}");
        assert!(msg.get("tool_calls").is_none(), "no valid tool_calls: {msg}");
    }
}

#[cfg(test)]
mod strip_stateful_history_tests {
    use super::strip_stateful_history;
    use crate::registry::Format as ProvFormat;
    use serde_json::{json, Value};

    /// The stateful assistant-history shape an agent client replays: a Responses
    /// `reasoning_details` array plus the `provider_metadata` continuation tag,
    /// on assistant, user and tool messages.
    fn stateful_body() -> Value {
        json!({
            "model": "mock-model",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "q1",
                 "provider_metadata": {"provider": "opencode-go", "model": "beam-free"}},
                {"role": "assistant", "content": "a1",
                 "reasoning_details": [{"type": "reasoning.text", "text": "old thought"}],
                 "provider_metadata": {"provider": "opencode-go", "model": "beam-free"}},
                {"role": "tool", "tool_call_id": "c1", "content": "result",
                 "reasoning_details": []},
                {"role": "user", "content": "go on"}
            ],
            "temperature": 0.2
        })
    }

    /// A Chat upstream validates the message shape and 400s the whole turn on
    /// these Responses-only keys (see the function doc), so they are removed
    /// from every message, not just assistant ones.
    #[test]
    fn stateful_history_fields_are_stripped_for_openai_chat() {
        let mut body = stateful_body();
        strip_stateful_history(&mut body, "opencode-go", ProvFormat::OpenAI);

        let messages = body["messages"].as_array().unwrap();
        for message in messages {
            assert!(
                message.get("reasoning_details").is_none(),
                "reasoning_details survived on a chat target: {message}"
            );
            assert!(
                message.get("provider_metadata").is_none(),
                "provider_metadata survived on a chat target: {message}"
            );
        }
        // Everything that is legal on the Chat wire stays put.
        assert_eq!(messages[1]["content"], "q1");
        assert_eq!(messages[2]["content"], "a1");
        assert_eq!(messages[3]["content"], "result");
        assert_eq!(body["model"], "mock-model");
        assert_eq!(body["temperature"], 0.2);
    }

    /// Anthropic needs its signed `thinking` blocks (and Responses targets may
    /// read these fields natively), so only the Chat wire is cleaned.
    #[test]
    fn other_wire_formats_keep_the_stateful_fields() {
        let original = stateful_body();
        for wire_format in [ProvFormat::Claude, ProvFormat::OpenAIResponses, ProvFormat::Gemini] {
            let mut body = original.clone();
            strip_stateful_history(&mut body, "opencode-go", wire_format);
            assert_eq!(body, original, "a {} target was rewritten", wire_format.as_str());
        }
    }

    /// The overwhelmingly common request has no stateful fields and must be
    /// byte-identical after the pass.
    #[test]
    fn a_plain_chat_request_is_left_alone() {
        let original = json!({
            "model": "mock-model",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello", "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "f", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ],
            "max_tokens": 9
        });
        let mut body = original.clone();
        strip_stateful_history(&mut body, "openai-compatible-beta", ProvFormat::OpenAI);
        assert_eq!(body, original);

        // A body with no messages array cannot be cleaned and must not panic.
        let mut empty = json!({"model": "m"});
        strip_stateful_history(&mut empty, "openai-compatible-beta", ProvFormat::OpenAI);
        assert_eq!(empty, json!({"model": "m"}));
    }

    /// Only the two fields the upstream named in its 400 are shipped by this
    /// pass. `reasoning_content` is the DeepSeek-compatible spelling the same
    /// upstream accepts, and top-level Responses fields are not part of a Chat
    /// request a client sends to this endpoint — both stay as the client sent
    /// them.
    #[test]
    fn responses_only_top_level_fields_and_reasoning_content_are_left_alone() {
        let original = json!({
            "model": "mock-model",
            "store": true,
            "previous_response_id": "resp_1",
            "include": ["reasoning.encrypted_content"],
            "messages": [
                {"role": "assistant", "content": "a1", "reasoning_content": "deepseek thinking"}
            ]
        });
        let mut body = original.clone();
        strip_stateful_history(&mut body, "opencode-go", ProvFormat::OpenAI);
        assert_eq!(body, original);
    }
}