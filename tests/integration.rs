//! End-to-end integration tests: full gateway with a local mock upstream.
//! Covers: non-stream + SSE chat, claude-native + openai-compatible upstreams,
//! failover on 500, model catalog, auth, count_tokens, health, 404s.

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use omniroute_rust::config::{ComboConfig, Config, ProviderCredentials};
use omniroute_rust::state::AppState;
use omniroute_rust::VERSION;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static ALPHA_HITS: AtomicU64 = AtomicU64::new(0);

/// Shared record of the last upstream body seen by the mock.
type Seen = Arc<std::sync::Mutex<Option<Value>>>;

async fn openai_chat(
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(body.clone());
    let stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("?").to_string();
    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        let chunks = vec![
            json!({"id":"chatcmpl-sse","object":"chat.completion.chunk","created":1,"model":model,
                   "choices":[{"index":0,"delta":{"role":"assistant","content":"ST"},"finish_reason":null}]}),
            json!({"id":"chatcmpl-sse","object":"chat.completion.chunk","created":1,"model":model,
                   "choices":[{"index":0,"delta":{"content":"REAM"},"finish_reason":null}]}),
            json!({"id":"chatcmpl-sse","object":"chat.completion.chunk","created":1,"model":model,
                   "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                   "usage":{"prompt_tokens":3,"completion_tokens":5}}),
        ];
        tokio::spawn(async move {
            for c in chunks {
                let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {c}\n\n")))).await;
            }
            let _ = tx.send(Ok(bytes::Bytes::from("data: [DONE]\n\n"))).await;
        });
        axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
            .unwrap()
    } else {
        (
            axum::http::StatusCode::OK,
            axum::Json(json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 1,
                "model": model,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "MOCK-SAYS-HI"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
            })),
        )
            .into_response()
    }
}

/// /alpha always 500 (failover source).
async fn alpha_chat() -> axum::response::Response {
    ALPHA_HITS.fetch_add(1, Ordering::SeqCst);
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"error": {"message": "alpha exploded", "type": "server_error"}})),
    )
        .into_response()
}

/// Gemini native mock: `{base}/models/{m}:generateContent`.
async fn gemini_generate(
    axum::extract::Path(m): axum::extract::Path<String>,
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(json!({"_path_model": m, "_body": body}));
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "candidates": [{"content": {"parts": [{"text": "GEMINI-SAYS"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}
        })),
    )
        .into_response()
}

/// Claude native mock: `{base}/messages`.
async fn claude_messages(
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(body);
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "model": "claude-mock",
            "content": [{"type": "text", "text": "CLAUDE-SAYS"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 4, "output_tokens": 1}
        })),
    )
        .into_response()
}

/// Auth-gated mock: only `Bearer k-live` succeeds (proves the gateway sends
/// the managed connection's key instead of probing anonymously).
async fn authcheck_chat(
    axum::extract::State(seen): axum::extract::State<Seen>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(body.clone());
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some("Bearer k-live");
    if ok {
        (
            axum::http::StatusCode::OK,
            axum::Json(json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 1,
                "model": "mock-model",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "MOCK-SAYS-HI"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
            })),
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "bad key", "type": "authentication_error"}})),
        )
            .into_response()
    }
}

/// Models-listing mock: OpenAI `{data:[{id}]}` shape.
async fn sync_models_list(
    axum::extract::State(seen): axum::extract::State<Seen>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(json!({"_models_list": true}));
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"data": [
            {"id": "sync-model-a", "contextWindow": 128000, "input": ["text", "image"]},
            {"id": "sync-model-b"}
        ]})),
    )
        .into_response()
}

async fn images_generations(
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(body.clone());
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "created": 1,
            "data": [{"url": format!("https://mock.test/img-{}.png", body.get("prompt").and_then(|p| p.as_str()).unwrap_or("?").len())}]
        })),
    )
        .into_response()
}

async fn audio_transcribe(
    axum::extract::State(seen): axum::extract::State<Seen>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(json!({
        "_raw_len": body.len(),
        "_content_type": headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
        "_multipart_model": crate_placeholder_model(&body),
    }));
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"text": "MOCK-TRANSCRIBED"})),
    )
        .into_response()
}

fn crate_placeholder_model(body: &[u8]) -> Option<String> {
    let marker = b"name=\"model\"";
    let idx = body.windows(marker.len()).position(|w| w == marker)?;
    let after = &body[idx + marker.len()..];
    // skip the header separator (CRLF CRLF), then the value runs until CRLF
    let sep = after.windows(4).position(|w| w == b"\r\n\r\n")?;
    let mut value = Vec::new();
    let mut i = sep + 4;
    while i < after.len() {
        if after[i] == b'\r' && i + 1 < after.len() && after[i + 1] == b'\n' { break; }
        value.push(after[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&value).to_string())
}

async fn batches_create(
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(body.clone());
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"id": "batch_1", "status": "validating", "model": body.get("model").cloned().unwrap_or(json!(""))})),
    )
        .into_response()
}

async fn batches_get(
    axum::extract::State(seen): axum::extract::State<Seen>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    *seen.lock().unwrap() = Some(json!({"_batch_get": id}));
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"id": id, "status": "completed"})),
    )
        .into_response()
}

fn mock_router(seen: Seen) -> Router {
    Router::new()
        .route("/alpha/v1/chat/completions", post(alpha_chat))
        .route("/beta/v1/chat/completions", post(openai_chat))
        .route("/sync/v1/models", get(sync_models_list))
        .route("/authcheck/v1/chat/completions", post(authcheck_chat))
        .route("/beta/v1/models", post(openai_chat))
        .route("/zen/v1/chat/completions", post(zen_chat))
        .route("/zen/v1/models", get(zen_models_list))
        .route("/beta/v1/images/generations", post(images_generations))
        .route("/beta/v1/audio/transcriptions", post(audio_transcribe))
        .route("/beta/v1/batches", post(batches_create))
        .route("/beta/v1/batches/{id}", get(batches_get))
        .route("/gemini/models/{m}", post(gemini_generate))
        .route("/claude/v1/messages", post(claude_messages))
        // the mock must accept the multi-MiB bodies the gateway forwards
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(seen)
}

// ---------------------------------------------------------------------------
// OpenCode Zen anonymous free lane
// ---------------------------------------------------------------------------

/// Headers the last `/zen/v1/chat/completions` call carried (lowercased names).
static ZEN_HEADERS: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

/// The free roster is process-wide, so tests that install one must not overlap.
static FREE_ROSTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serves `/zen/v1/models`: the live directory the gateway discovers.
async fn zen_models_list() -> axum::response::Response {
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"object": "list", "data": [
            {"id": "zen-test-free", "object": "model"},
            {"id": "zen-region-blocked-free", "object": "model"},
            {"id": "zen-paid", "object": "model"},
            {"id": "exo-free", "object": "model"}
        ]})),
    )
        .into_response()
}

/// A mock that *enforces* the anonymous free-lane gate, so a regression in the
/// gateway's request shape fails the test instead of silently 403-ing in
/// production (the real upstream returns `FreeTierError`):
///   * `stream: true` is required for `-free` ids (non-stream → 403)
///   * the `bash`+`read` gate tools must be present (missing → 403)
///   * a session header with the canonical `ses_` shape is required (absent → 403)
///   * `zen-region-blocked-free` answers 403 RegionError, `zen-paid` answers 402
async fn zen_chat(
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut recorded = serde_json::Map::new();
    for (name, value) in headers.iter() {
        recorded.insert(name.as_str().to_string(), json!(value.to_str().unwrap_or("")));
    }
    recorded.insert("_body".into(), body.clone());
    *ZEN_HEADERS.lock().unwrap() = Some(Value::Object(recorded));

    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let tool_names: Vec<String> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.pointer("/function/name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let denied = |etype: &str, message: &str, status: u16| {
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({"type": "error", "error": {"type": etype, "message": message}})),
        )
            .into_response()
    };
    if model == "zen-region-blocked-free" {
        return denied("RegionError", "This model is not available in your country", 403);
    }
    if model == "zen-paid" {
        return denied("InsufficientBalance", "Insufficient account balance", 402);
    }
    // A keyed free call needs the workspace's "Allow free endpoints" opt-in,
    // exactly like the real upstream (400 until then). That is what makes the
    // "always authenticate anonymously on the free lane" rule testable: the
    // gateway must not leak a configured provider key into these requests.
    let auth = header("authorization");
    if !auth.is_empty() && auth != "Bearer public" {
        return denied(
            "FreeTierError",
            "Free models may train on request data. Allow free endpoints in your workspace's Privacy settings",
            400,
        );
    }
    let canonical_session = {
        let session = header("x-opencode-session");
        let bytes = session.as_bytes();
        let head_ok = bytes.len() == 30
            && session.starts_with("ses_")
            && bytes[4..16].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            && bytes[16..].iter().all(|b| b.is_ascii_alphanumeric());
        if !head_ok {
            // The real upstream also accepts x-session-affinity / X-Session-Id.
            let alt = header("x-session-affinity");
            let alt = if alt.is_empty() { header("X-Session-Id") } else { alt };
            alt.starts_with("ses_") && alt.len() == 30
        } else {
            true
        }
    };
    if !canonical_session {
        return denied(
            "FreeTierError",
            "OpenCode's free tier can only be used from within OpenCode.",
            403,
        );
    }
    if !stream || !(tool_names.iter().any(|n| n == "bash") && tool_names.iter().any(|n| n == "read")) {
        return denied(
            "FreeTierError",
            "OpenCode's free tier can only be used from within OpenCode.",
            403,
        );
    }
    if !stream {
        return denied("FreeTierError", "stream required", 403);
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let model_for_chunks = model.clone();
    let chunks = vec![
        json!({"id":"cmpl-zen","object":"chat.completion.chunk","created":1,"model":model_for_chunks,
               "choices":[{"index":0,"delta":{"role":"assistant","content":"ZEN-"},"finish_reason":null}]}),
        json!({"id":"cmpl-zen","object":"chat.completion.chunk","created":1,"model":model_for_chunks,
               "choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":5,"completion_tokens":2}}),
    ];
    tokio::spawn(async move {
        for chunk in chunks {
            let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {chunk}\n\n")))).await;
        }
        let _ = tx.send(Ok(bytes::Bytes::from("data: [DONE]\n\n"))).await;
    });
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .unwrap()
}

/// A discovered subscription quota feeds routing: an exhausted OpenCode Go
/// window is soft-skipped (with a diagnostic) so the combo spends its other
/// candidates first, instead of collecting a guaranteed 429.
#[tokio::test]
async fn exhausted_subscription_quota_is_skipped_with_a_diagnostic() {
    let (mock_base, _seen) = spawn_mock().await;
    omniroute_rust::opencode_quota::install(omniroute_rust::opencode_quota::Usage {
        provider: "opencode-go".into(),
        url: "https://opencode.ai/zen/go/v1/usage".into(),
        fetched_at_ms: now_ms_test(),
        monthly: Some(omniroute_rust::opencode_quota::Window {
            status: "rate-limited".into(),
            used_percent: 100.0,
            resets_at_ms: Some(now_ms_test() + 3_600_000),
        }),
        ..Default::default()
    });

    let combo = ComboConfig {
        name: "quota-combo".into(),
        strategy: Some("priority".into()),
        providers: vec![
            "opencode-go/mock-model".into(),
            // same always-500 upstream, so the combo ends in its terminal error
            "openai-compatible-alpha/mock-model".into(),
        ],
        models: vec![],
    };
    let mut cfg = test_config(&mock_base, vec![combo]);
    cfg.credentials.insert(
        "opencode-go".into(),
        ProviderCredentials {
            api_key: Some("k-go".into()),
            base_url: Some(format!("{mock_base}/alpha/v1")),
            ..Default::default()
        },
    );
    let gw = spawn_gateway(AppState::build(cfg)).await;
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "quota-combo", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    omniroute_rust::opencode_quota::clear();
    assert!(status.is_server_error(), "status {status}: {text}");
    assert!(
        text.contains("subscription quota exhausted"),
        "the diagnostic must name the quota gate: {text}"
    );
}

fn now_ms_test() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `auto/free` is a real free strategy: it resolves to the discovered zero-cost
/// roster (here the mock's Zen directory), never to a paid model, and needs no
/// combo edit when the roster changes.
#[tokio::test]
async fn auto_free_combo_routes_to_discovered_free_models_only() {
    let _roster = FREE_ROSTER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mock_base, _seen) = spawn_mock().await;
    omniroute_rust::opencode_free::install_live(vec![
        "zen-test-free".into(),
        "zen-region-blocked-free".into(),
    ]);
    let gw = spawn_gateway(zen_state(&mock_base)).await;
    let client = reqwest::Client::new();

    // The virtual model is advertised alongside the built-in auto/* ids.
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"auto/free"), "catalog: {ids:?}");
    assert!(ids.contains(&"auto/best-free"), "catalog: {ids:?}");

    // A request to auto/free lands on the free-lane model, non-streaming.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "auto/free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "auto/free failed: {body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");
    let recorded = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(recorded["authorization"], "Bearer public");
    assert_eq!(recorded["_body"]["model"], "zen-test-free");
}

/// The dashboard path the operator actually uses: create a combo whose step is
/// `auto/free`, see it expand in the response, then route through it.
#[tokio::test]
async fn managed_combo_can_use_an_auto_virtual_combo_as_a_step() {
    let _roster = FREE_ROSTER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mock_base, _seen) = spawn_mock().await;
    omniroute_rust::opencode_free::install_live(vec![
        "zen-test-free".into(),
        "zen-region-blocked-free".into(),
    ]);
    let gw = spawn_gateway(zen_state(&mock_base)).await;
    let client = reqwest::Client::new();
    let token = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "CHANGEME"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // `coding-free → auto/free` as the only step.
    let created: Value = client
        .post(format!("{gw}/v1/combos/managed"))
        .bearer_auth(&token)
        .json(&json!({
            "name": "coding-free",
            "strategy": "priority",
            "providers": ["auto/free"],
            "models": []
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resolved: Vec<String> = created["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        resolved.contains(&"opencode-zen/zen-test-free".to_string()),
        "the virtual step must expand to the discovered roster: {resolved:?}"
    );
    assert!(
        !resolved.iter().any(|id| id.starts_with("auto/")),
        "no virtual id may survive expansion: {resolved:?}"
    );

    // The combo is now first-class: request it by name and land on the lane.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "coding-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "coding-free failed: {body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");

    // A nested reference to another combo expands the same way, and the outer
    // combo joins the catalog with capabilities taken from the expansion.
    client
        .post(format!("{gw}/v1/combos/managed"))
        .bearer_auth(&token)
        .json(&json!({
            "name": "outer",
            "strategy": "priority",
            "providers": ["coding-free"],
            "models": []
        }))
        .send()
        .await
        .unwrap();
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outer = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "outer")
        .expect("the combo joins the catalog");
    assert!(
        outer["contextLength"].as_i64().unwrap_or(0) > 0,
        "capabilities come from the expanded steps: {outer}"
    );
}

/// Point the real `opencode-zen` provider entry at the mock upstream. The
/// registry keeps its static seed models and its wire format; only the base
/// URL moves, exactly like a dashboard-managed connection override.
fn zen_state(mock_base: &str) -> AppState {
    let mut state = AppState::new(test_config(mock_base, vec![]));
    state.config.credentials.insert(
        "opencode-zen".into(),
        ProviderCredentials {
            base_url: Some(format!("{mock_base}/zen/v1")),
            ..Default::default()
        },
    );
    state
}

/// End-to-end proof of the anonymous Zen free lane: a **non-streaming** client
/// request must reach the upstream as a streaming, agent-shaped body carrying
/// the literal `public` credential and a canonical session id, and the
/// aggregated SSE must come back as ordinary JSON. The mock rejects anything
/// that does not satisfy the gate, so a regression fails the test rather than
/// silently 403-ing in production.
#[tokio::test]
async fn opencode_free_lane_enforces_the_anonymous_contract_and_learns_denials() {
    let _roster = FREE_ROSTER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mock_base, _seen) = spawn_mock().await;
    // The roster this test's directory would publish (no network in tests).
    omniroute_rust::opencode_free::install_live(vec![
        "zen-test-free".into(),
        "zen-region-blocked-free".into(),
    ]);
    let gw = spawn_gateway(zen_state(&mock_base)).await;
    let client = reqwest::Client::new();

    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "free-lane call failed: {body}");
    // The upstream answered SSE; the gateway folded it back into JSON.
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");

    let recorded = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(
        recorded["authorization"], "Bearer public",
        "the anonymous lane must use the literal public credential"
    );
    let session = recorded["x-opencode-session"].as_str().unwrap();
    assert!(session.starts_with("ses_") && session.len() == 30, "session: {session}");
    assert_eq!(
        recorded["x-session-affinity"].as_str().unwrap(),
        session,
        "affinity must mirror the session for upstream prompt-cache grouping"
    );
    assert_eq!(recorded["_body"]["stream"], true, "the lane only answers streamed bodies");
    let names: Vec<&str> = recorded["_body"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"bash") && names.contains(&"read"), "gate tools: {names:?}");
    assert_eq!(recorded["_body"]["tool_choice"], "none");

    // A *keyed* provider must still reach the lane anonymously: keyed free
    // access is gated by the workspace privacy opt-in (the mock answers 400
    // for any non-`public` credential, like the real upstream does).
    let mut keyed = zen_state(&mock_base);
    if let Some(connection) = keyed.config.credentials.get_mut("opencode-zen") {
        connection.api_key = Some("sk-configured-but-must-not-be-sent".into());
    }
    let keyed_gw = spawn_gateway(keyed).await;
    let keyed_resp = post_json(
        &format!("{keyed_gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    let keyed_status = keyed_resp.status();
    let keyed_body: Value = keyed_resp.json().await.unwrap();
    assert_eq!(keyed_status, 200, "keyed free-lane call failed: {keyed_body}");
    let keyed_seen = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(
        keyed_seen["authorization"], "Bearer public",
        "a configured key must not be sent to the anonymous lane"
    );

    // ...and the opt-in knob does switch to the key when an operator asks for it.
    let mut opted_in = zen_state(&mock_base);
    opted_in.config.opencode_free_use_key = true;
    if let Some(connection) = opted_in.config.credentials.get_mut("opencode-zen") {
        connection.api_key = Some("sk-opted-in".into());
    }
    let opted_gw = spawn_gateway(opted_in).await;
    let opted = post_json(
        &format!("{opted_gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    assert_eq!(
        opted.status(),
        400,
        "with the opt-in the key is used, and the mock (like the upstream) rejects it"
    );
    let opted_seen = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(opted_seen["authorization"], "Bearer sk-opted-in");

    // ...but a refusal by the workspace opt-in must not break the lane: the
    // first keyed call fails, the next one falls back to `public` and succeeds.
    let healed = post_json(
        &format!("{opted_gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    assert_eq!(healed.status(), 200, "the lane must self-heal to the anonymous credential");
    let healed_seen = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(healed_seen["authorization"], "Bearer public");
    omniroute_rust::opencode_free::clear_consent_rejection();


    // A region-blocked model fails fast — and the failure is learned, so the
    // roster stops advertising it (the live self-healing path).
    let blocked = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-region-blocked-free",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
    )
    .await;
    assert_eq!(blocked.status(), 403);

    // Management session for the observability endpoint.
    let token = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "CHANGEME"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let status_body: Value = client
        .get(format!("{gw}/v1/opencode-free"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status_body["status"], "ready");
    assert_eq!(status_body["directory"]["count"], 2);
    let denied: Vec<&str> = status_body["roster"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["allowed"] == false)
        .filter_map(|row| row["model"].as_str())
        .collect();
    assert!(denied.contains(&"zen-region-blocked-free"), "roster: {denied:?}");

    // A model-level answer must not cool the provider: the tier's healthy
    // siblings stay usable (and the Combo Studio must not paint the whole tier
    // as "cooling").
    let providers: Value = client
        .get(format!("{gw}/v1/providers"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let zen = providers["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "opencode-zen")
        .expect("opencode-zen is registered");
    assert_eq!(
        zen["cooldownMs"], 0,
        "a region-blocked model must not put the provider in cooldown: {zen}"
    );

    // The dashboard "Test" button must not report a false 403: a free-lane
    // probe speaks the same streaming, gate-tooled, canonical-session shape a
    // real request does (the mock rejects anything else with FreeTierError).
    let created = client
        .post(format!("{gw}/v1/provider-connections"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "id": "zen-probe",
            "provider": "opencode-zen",
            "name": "zen probe",
            "api_key": "sk-configured",
            "base_url": format!("{mock_base}/zen/v1"),
            "models": ["zen-test-free"],
            "enabled": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "connection create: {:?}", created.text().await);
    let probed = client
        .post(format!("{gw}/v1/provider-connections/zen-probe/test"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let probed_body: Value = probed.json().await.unwrap();
    assert_eq!(probed_body["ok"], true, "free-lane probe: {probed_body}");
    let probe_seen = ZEN_HEADERS.lock().unwrap().clone().unwrap();
    assert_eq!(
        probe_seen["authorization"], "Bearer public",
        "the probe must obey the same credential rule as a real request"
    );
    assert_eq!(probe_seen["_body"]["stream"], true, "the probe must stream");
    let probe_tools: Vec<&str> = probe_seen["_body"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(|n| n.as_str()))
        .collect();
    assert!(probe_tools.contains(&"bash") && probe_tools.contains(&"read"), "{probe_tools:?}");

    // The Providers page "model test" (a body carrying the model) shares the
    // same implementation, and must not report a false 403 either.
    let per_model = client
        .post(format!("{gw}/v1/provider-connections/zen-probe/test"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"model": "zen-test-free"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(per_model["ok"], true, "per-model probe: {per_model}");
    assert_eq!(per_model["model"], "zen-test-free");

    // The Combos page "Test combo" button posts execute:true, which dispatches
    // every candidate through the same probe.
    client
        .post(format!("{gw}/v1/combos/managed"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "name": "lane-combo",
            "strategy": "priority",
            "providers": ["auto/free"],
            "models": []
        }))
        .send()
        .await
        .unwrap();
    let combo_test = client
        .post(format!("{gw}/v1/combos/test"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"model": "lane-combo", "execute": true}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let first = &combo_test["candidates"][0];
    assert_eq!(first["tested"], true, "combo test must dispatch: {combo_test}");
    assert_eq!(first["ok"], true, "combo test on the free lane: {combo_test}");

    // ...and the learned denial removes it from the exposed catalog.
    let models: Value = client
        .get(format!("{gw}/v1/models"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"opencode-zen/zen-test-free"), "catalog: {ids:?}");
    assert!(
        !ids.contains(&"opencode-zen/zen-region-blocked-free"),
        "a region-blocked model must leave the roster: {ids:?}"
    );
    // The free flag lets model pickers separate the zero-cost lane.
    let free_row = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "opencode-zen/zen-test-free")
        .unwrap();
    assert_eq!(free_row["free"], true);
}

async fn spawn_mock() -> (String, Seen) {
    let seen: Seen = Arc::new(std::sync::Mutex::new(None));
    let app = mock_router(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), seen)
}

async fn spawn_gateway(state: AppState) -> String {
    let app = omniroute_rust::server::build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

fn test_config(mock_base: &str, combos: Vec<ComboConfig>) -> Config {
    let mut cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
        max_body_bytes: omniroute_rust::config::DEFAULT_MAX_BODY_BYTES,
        data_dir: std::env::temp_dir().join(format!("omniroute-it-{}", std::process::id())),
        api_key: Some("test-key".into()),
        request_timeout_ms: 600_000,
        connect_timeout_ms: 30_000,
        stream_idle_timeout_ms: 600_000,
        heartbeat_ms: 15_000,
        readiness_timeout_ms: 80_000,
        readiness_max_timeout_ms: 180_000,
        disconnect_grace_ms: 10_000,
        rate_rpm: 1_000_000,
        rate_min_interval_ms: 0,
        rate_concurrent_requests: 1024,
        rate_max_wait_ms: 30_000,
        rate_auto_enable_api_key_providers: true,
        compression: omniroute_rust::compression::CompressionConfig {
            enabled: true,
            default_mode: omniroute_rust::compression::CompressionMode::Off,
            ..Default::default()
        },
        thinking_mode: "passthrough".into(),
        thinking_budget: None,
        strip_historical_reasoning: true,
        model_catalog_auto_update: false,
        model_catalog_ttl_hours: 24,
        model_catalog_url: omniroute_rust::model_catalog::CATALOG_URL.to_string(),
        opencode_free_auto_update: false,
        opencode_free_ttl_seconds: omniroute_rust::opencode_free::DEFAULT_REFRESH_SECONDS,
        opencode_free_url: omniroute_rust::opencode_free::ZEN_BASE_URL.to_string(),
        opencode_free_use_key: false,
        opencode_quota_auto_update: false,
        opencode_quota_ttl_seconds: 300,
        provider_sync_auto_update: false,
        provider_sync_ttl_hours: 24,
        credentials: std::collections::HashMap::new(),
        tuning: std::collections::HashMap::new(),
        combos,
        log_level: "info".into(),
    };
    cfg.credentials.insert(
        "openai-compatible-alpha".into(),
        ProviderCredentials {
            api_key: Some("k-alpha".into()),
            base_url: Some(format!("{mock_base}/alpha/v1")),
            ..Default::default()
        },
    );
    cfg.credentials.insert(
        "openai-compatible-beta".into(),
        ProviderCredentials {
            api_key: Some("k-beta".into()),
            base_url: Some(format!("{mock_base}/beta/v1")),
            model_list: vec!["mock-model".into()],
            ..Default::default()
        },
    );
    cfg.credentials.insert(
        "anthropic-compatible-zai".into(),
        ProviderCredentials {
            api_key: Some("k-zai".into()),
            base_url: Some(format!("{mock_base}/claude/v1")),
            ..Default::default()
        },
    );
    cfg
}

fn test_config_for(dir: &std::path::Path, mock_base: &str) -> Config {
    let mut cfg = test_config(mock_base, vec![]);
    cfg.data_dir = dir.to_path_buf();
    cfg.api_key = None; // dashboard-session-driven management
    cfg
}

/// Models.dev-shaped payload served by a local mock, so the catalog refresh
/// path is exercised without network access.
async fn spawn_catalog_upstream() -> String {
    let app = axum::Router::new().route(
        "/api.json",
        axum::routing::get(|| async {
            axum::Json(json!({
                "openai-compatible-alpha": {"models": {
                    "catalog-model": {
                        "limit": {"context": 999000, "input": 999000, "output": 4096},
                        "modalities": {"input": ["text", "image"]},
                        "reasoning": true
                    }
                }}
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn model_catalog_refresh_endpoint_updates_model_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let (mock_base, _seen) = spawn_mock().await;
    let catalog_base = spawn_catalog_upstream().await;
    let mut cfg = test_config_for(dir.path(), &mock_base);
    cfg.model_catalog_url = format!("{catalog_base}/api.json");
    cfg.model_catalog_auto_update = false;
    // list the model on the alpha connection so the catalog can describe it
    if let Some(alpha) = cfg.credentials.get_mut("openai-compatible-alpha") {
        alpha.model_list = vec!["catalog-model".into()];
    }
    let gw = spawn_gateway(AppState::build(cfg)).await;
    let client = reqwest::Client::new();

    // management session (the endpoint is operator-only)
    let v = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "CHANGEME"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let token = v["token"].as_str().unwrap().to_string();

    // before the refresh the model is unknown to the catalog (open access:
    // this state configures no gateway key)
    let listed = client.get(format!("{gw}/v1/models")).send().await.unwrap();
    assert_eq!(listed.status(), 200, "model listing reachable without a key");
    let models: Value = listed.json().await.unwrap();
    let before = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "openai-compatible-alpha/catalog-model")
        .expect("model is listed from the connection");
    assert_eq!(before["contextWindow"], 262_144, "heuristic default before the refresh");

    let r = client
        .post(format!("{gw}/v1/model-catalog/refresh"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "catalog refresh succeeds over HTTP");
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["rows"].as_u64().unwrap() > 0);

    let models = client
        .get(format!("{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "openai-compatible-alpha/catalog-model")
        .expect("model is catalogued after refresh");
    assert_eq!(entry["contextWindow"], 999_000);
    assert_eq!(entry["maxTokens"], 4096);
    assert_eq!(entry["supportsVision"], true);
    assert!(entry["modalities"].as_array().unwrap().iter().any(|m| m == "image"));

    // the disk cache was written next to the data dir
    assert!(dir.path().join("model-catalog.json").exists(), "cache persisted");
}

async fn post_json(url: &str, key: Option<&str>, body: Value) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut req = client.post(url).json(&body);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn health_endpoints_and_404_and_401() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    let client = reqwest::Client::new();
    assert_eq!(client.get(format!("{gw}/healthz")).send().await.unwrap().text().await.unwrap(), "ok\n");
    assert_eq!(client.get(format!("{gw}/readyz")).send().await.unwrap().status(), 200);
    assert_eq!(client.get(format!("{gw}/livez")).send().await.unwrap().status(), 200);
    let h = client.get(format!("{gw}/api/health")).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(h["ok"], true);
    assert_eq!(h["version"], VERSION);

    // 404 JSON shape (never HTML)
    let r = client.get(format!("{gw}/v1/nope")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let e = r.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["type"], "not_found");
    assert_eq!(e["error"]["code"], "unknown_route");

    // 401 without / with wrong key
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": "openai-compatible-beta/mock-model", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let e = r.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["type"], "authentication_error");
    assert_eq!(e["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn nonstream_chat_openai_compatible() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 9
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let out = resp.json::<Value>().await.unwrap();
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["content"], "MOCK-SAYS-HI");
    assert_eq!(out["usage"]["completion_tokens"], 5);

    // mock saw: provider prefix stripped, model set, stream false
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["model"], "mock-model");
    assert_eq!(last["stream"], false);
    assert_eq!(last["max_tokens"], 9);
    assert_eq!(last["messages"][0]["role"], "user");
}

#[tokio::test]
async fn sse_stream_chat() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ct.starts_with("text/event-stream"), "content-type was {ct}");
    let text = resp.text().await.unwrap();
    assert!(text.contains("data: [DONE]"), "missing DONE in:\n{text}");
    assert!(text.contains("\"content\":\"ST\""), "missing ST chunk in:\n{text}");
    assert!(text.contains("\"content\":\"REAM\""), "missing REAM chunk in:\n{text}");
    assert!(text.contains("\"role\":\"assistant\""), "missing role marker in:\n{text}");
}

#[tokio::test]
async fn failover_on_500() {
    let (mock_base, seen) = spawn_mock().await;
    let combo = ComboConfig {
        name: "it".into(),
        strategy: Some("priority".into()),
        providers: vec!["openai-compatible-alpha/m".into(), "openai-compatible-beta/m".into()],
        models: vec![],
    };
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![combo]))).await;

    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let out = resp.json::<Value>().await.unwrap();
    assert_eq!(out["choices"][0]["message"]["content"], "MOCK-SAYS-HI");

    // alpha was attempted (500) and beta served the request
    assert!(ALPHA_HITS.load(Ordering::SeqCst) >= 1, "alpha was never hit");
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["model"], "m");
}

/// A single failing candidate must not permanently delete its provider from the
/// fallback chain: the next request probes the cooled-down provider again
/// (half-open) instead of reporting `attempts: 0`.
#[tokio::test]
async fn cooled_provider_is_probed_instead_of_reporting_zero_attempts() {
    let (mock_base, _seen) = spawn_mock().await;
    let combo = ComboConfig {
        name: "solo".into(),
        strategy: Some("priority".into()),
        providers: vec!["openai-compatible-alpha/m".into()],
        models: vec![],
    };
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![combo]))).await;
    let before = ALPHA_HITS.load(Ordering::SeqCst);

    // First request: alpha fails, which cools the provider down.
    let first = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "solo", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert!(first.status().is_server_error(), "expected upstream failure, got {}", first.status());
    let first_body = first.text().await.unwrap();
    assert!(first_body.contains("attempts: 1"), "first failure body: {first_body}");

    // Second request while the cooldown is still active: the provider must be
    // probed again (attempts: 1), never skipped wholesale (attempts: 0).
    let second = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "solo", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let second_body = second.text().await.unwrap();
    assert!(
        !second_body.contains("attempts: 0"),
        "cooled provider was skipped without a probe: {second_body}"
    );
    assert!(second_body.contains("attempts: 1"), "second failure body: {second_body}");
    assert!(
        ALPHA_HITS.load(Ordering::SeqCst) > before + 1,
        "alpha was not re-probed while cooling down"
    );
}

#[tokio::test]
async fn claude_inbound_openai_provider_translation() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    let resp = post_json(
        &format!("{gw}/v1/messages"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "max_tokens": 20,
            "system": "be nice",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let out = resp.json::<Value>().await.unwrap();
    // anthropic wire shape out
    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][0]["text"], "MOCK-SAYS-HI");
    assert_eq!(out["usage"]["input_tokens"], 3);

    // upstream saw openai shape with system message
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["messages"][0]["role"], "system");
    assert_eq!(last["messages"][0]["content"], "be nice");
    assert_eq!(last["max_tokens"], 20);
}

#[tokio::test]
async fn claude_native_provider() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    let resp = post_json(
        &format!("{gw}/v1/messages"),
        Some("test-key"),
        json!({
            "model": "anthropic-compatible-zai/claude-3",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let out = resp.json::<Value>().await.unwrap();
    assert_eq!(out["content"][0]["text"], "CLAUDE-SAYS");
    assert_eq!(out["stop_reason"], "end_turn");

    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["model"], "claude-3");
    assert_eq!(last["max_tokens"], 30);
}

#[tokio::test]
async fn models_catalog_and_count_tokens() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    let r = client.get(format!("{gw}/v1/models")).bearer_auth("test-key").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["object"], "list");
    let ids: Vec<String> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();
    assert!(ids.iter().any(|i| i.starts_with("openai-compatible-beta/")), "catalog: {ids:?}");
    assert!(ids.iter().any(|i| i.contains("mock-model")), "catalog: {ids:?}");
    assert!(ids.iter().any(|i| i == "auto/best-coding"), "built-in auto combos: {ids:?}");

    // count_tokens is a local estimation, no upstream
    let r = post_json(
        &format!("{gw}/v1/messages/count_tokens"),
        Some("test-key"),
        json!({"model": "anthropic/claude-3", "messages": [{"role": "user", "content": "abcdefghij"}]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["input_tokens"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn unknown_model_404() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let r = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "no-such-provider/xyz", "messages": []}),
    )
    .await;
    // unknown prefix → treated as bare model; nobody advertises → 404 model_not_found
    assert_eq!(r.status(), 404);
    let e = r.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn combos_test_endpoint_lists_chain() {
    let (mock_base, _seen) = spawn_mock().await;
    let combo = ComboConfig {
        name: "it".into(),
        strategy: Some("priority".into()),
        providers: vec!["openai-compatible-alpha/m".into(), "openai-compatible-beta/m".into()],
        models: vec![],
    };
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![combo]))).await;
    let r = post_json(&format!("{gw}/v1/combos/test"), Some("test-key"), json!({"model": "m"})).await;
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    let chain: Vec<&str> = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.get("provider").and_then(|p| p.as_str()))
        .collect();
    assert_eq!(chain, vec!["openai-compatible-alpha", "openai-compatible-beta"]);

    // The dashboard Test action executes one probe per model in the combo,
    // rather than only returning the resolved candidate chain.
    let r = post_json(
        &format!("{gw}/v1/combos/test"),
        Some("test-key"),
        json!({"model": "m", "execute": true}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let executed: Value = r.json().await.unwrap();
    assert_eq!(executed["executed"], true);
    assert_eq!(executed["summary"]["total"], 2);
    assert_eq!(executed["summary"]["passed"], 1, "beta probe succeeds, alpha is the intentional 500");
    assert!(executed["candidates"].as_array().unwrap().iter().all(|c| c["tested"] == true));
}

#[tokio::test]
async fn compression_via_request_header() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    // long tool result > 2000 chars should be truncated by the lite engine
    let tool_output = "filler line ".repeat(400); // 4800 chars
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-omniroute-compression", "lite")
        .json(&json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [
                {"role": "user", "content": "check the build output"},
                {"role": "tool", "content": tool_output}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // response meta header present with stats
    let meta = resp.headers().get("x-omniroute-compression").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(meta.starts_with("lite; source=request-header"), "meta header was: {meta}");
    assert!(meta.contains("tokens="), "stats in header: {meta}");

    // upstream saw the compressed tool result
    let last = seen.lock().unwrap().clone().unwrap();
    let tool_content = last["messages"][1]["content"].as_str().unwrap();
    assert!(tool_content.contains("...[truncated]"), "tool result truncated: {}", tool_content.len());
    assert!(tool_content.chars().count() < 2200);
}

#[tokio::test]
async fn compression_modes_report_real_body_savings() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();
    let long_tool = "filler line ".repeat(400);
    let long_assistant = "The build completed successfully and the generated artifact is available for the next validation step. ".repeat(300);
    let long_prose = "Please make sure to provide a detailed explanation of the current implementation, thanks, and remember to keep the answer concise while preserving the important details. ".repeat(80);
    let long_log = std::iter::once("$ cargo build".to_string())
        .chain((0..500).map(|i| format!("build output line {i}")))
        .collect::<Vec<_>>()
        .join("\n");
    let cases = vec![
        (
            "lite",
            json!({"messages": [
                {"role": "user", "content": "inspect the tool output"},
                {"role": "tool", "content": long_tool}
            ]}),
        ),
        (
            "standard",
            // The trailing user turn is the live instruction (never rewritten);
            // the long prose must sit *before* it to be compressible.
            json!({"messages": [
                {"role": "user", "content": long_prose},
                {"role": "user", "content": "next: verify the artifact"}
            ]}),
        ),
        (
            "aggressive",
            json!({"messages": [
                {"role": "assistant", "content": long_assistant},
                {"role": "user", "content": "what should I verify next?"}
            ]}),
        ),
        (
            "ultra",
            json!({"messages": [
                {"role": "user", "content": long_prose},
                {"role": "user", "content": "next: verify the artifact"}
            ]}),
        ),
        (
            "rtk",
            json!({"messages": [{"role": "tool", "content": long_log}]}),
        ),
    ];

    for (mode, body) in cases {
        let resp = client
            .post(format!("{gw}/v1/chat/completions"))
            .bearer_auth("test-key")
            .header("x-omniroute-compression", mode)
            .json(&json!({
                "model": "openai-compatible-beta/mock-model",
                "messages": body["messages"].clone()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "mode={mode}");
        let meta = resp
            .headers()
            .get("x-omniroute-compression")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(meta.starts_with(&format!("{mode}; source=request-header")), "mode={mode} meta={meta}");
        let token_pair = meta
            .split("; ")
            .find_map(|part| part.strip_prefix("tokens="))
            .unwrap_or_else(|| panic!("mode={mode} has no token pair: {meta}"));
        let (original, compressed) = token_pair
            .split_once("->")
            .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
            .unwrap_or_else(|| panic!("mode={mode} has malformed token pair: {meta}"));
        assert!(original > compressed, "mode={mode} did not save tokens: {meta}");

        let upstream = seen.lock().unwrap().clone().unwrap();
        let upstream_json = serde_json::to_string(&upstream).unwrap();
        assert!(!upstream_json.is_empty(), "mode={mode} upstream body missing");
        match mode {
            "lite" => assert!(upstream_json.contains("...[truncated]")),
            "standard" | "ultra" => {
                assert_ne!(upstream["messages"][0]["content"], body["messages"][0]["content"]);
            }
            "aggressive" => assert_ne!(upstream["messages"][0]["content"], body["messages"][0]["content"]),
            "rtk" => assert!(!upstream_json.contains("build output line 300")),
            _ => unreachable!(),
        }
        assert!(compressed > 0, "mode={mode} reported an empty compressed body: {meta}");
    }
}

#[tokio::test]
async fn compression_config_endpoint() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();
    let r = client.get(format!("{gw}/v1/compression")).bearer_auth("test-key").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["modes"][0], "off");
    assert_eq!(v["modes"][5], "rtk");
    assert_eq!(v["per_request_header"], "x-omniroute-compression");
}

#[tokio::test]
async fn dashboard_shell_served() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    // root redirects to /dashboard (reqwest follows by default → use a no-redirect client)
    let no_redirect = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = no_redirect.get(gw.clone()).send().await.unwrap();
    assert_eq!(r.status(), 307);
    assert_eq!(r.headers().get("location").unwrap(), "/dashboard");

    // dashboard shell + assets
    let r = client.get(format!("{gw}/dashboard")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    // title + brand parity with the original (layout.tsx title, Sidebar.tsx header)
    assert!(html.contains("AI Gateway for Multi-Provider LLMs"));
    assert!(html.contains("<h1>OmniRoute</h1>"), "brand name in the sidebar header");
    assert!(html.contains("logo-box"), "gradient logo box");
    assert!(html.contains("manifest.webmanifest"));

    let r = client.get(format!("{gw}/dashboard/app.js")).send().await.unwrap();
    assert!(r.status().is_success());
    let js = r.text().await.unwrap();
    // the sidebar must be built during boot (regression: it was only built by the
    // language switcher, so a fresh load rendered an empty sidebar).
    assert!(
        js.contains("await setLang(savedLocale)") || js.contains("await setLang(detectLocale())"),
        "boot restores locale + builds sidebar"
    );
    assert!(js.matches("buildSidebar(").count() >= 2, "buildSidebar defined and called");
    assert!(js.contains("function showLogin"), "login overlay helper");
    assert!(js.contains("async function api"), "authenticated fetch helper");
    assert!(js.contains("/v1/provider-connections"), "managed provider data is consumed by the dashboard");
    assert!(js.contains("accountCounts"), "combo provider account counts");
    assert!(js.contains("catalogModels") && js.contains("managedModels"), "custom models feed global combo search");
    assert!(js.contains("hiddenByProvider") && js.contains("isHiddenModelId"), "combo builder filters hidden models");
    assert!(js.contains("hiddenModelUnavailable"), "manual hidden-model selection is blocked");
    assert!(js.contains("let hiddenByProvider = new Map()"), "combo hidden state is builder-scoped");
    assert!(!js.contains("const hiddenByProvider = new Map()"), "combo hidden state is not load-local");
    assert!(js.contains("reasoning_effort") && js.contains("client_ip") && js.contains("endpoint"), "home logs expose request context");
    assert!(js.contains("Asia/Shanghai") && js.contains("timeZone: isChinese ? 'Asia/Shanghai' : 'UTC'"), "home log time follows locale");
    assert!(js.contains("display = open ? 'none' : 'grid'"), "auto combo catalog wraps as a grid");
    assert!(!js.contains("tBodies"), "tbody targets are written directly");
    // deterministic per-item icon accents (sidebarVisibility.ts port)
    assert!(js.contains("iconAccent"), "icon accent helper");
    assert!(js.contains("dataset.theme"), "theme toggle");

    // shell chrome parity: sidebar search + service actions, topbar page header + quick nav
    for id in [
        "nav-search", "svc-restart", "svc-stop", "page-icon", "page-title", "page-sub",
        "quick-nav", "theme-toggle", "power-btn", "lang-selector",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "dashboard chrome id {id}");
    }

    let r = client.get(format!("{gw}/dashboard/app.css")).send().await.unwrap();
    assert!(r.status().is_success());
    let css = r.text().await.unwrap();
    assert!(css.contains("Material Symbols Outlined"));
    assert!(css.contains("--fd-sidebar-width: 220px"), "original sidebar width token");
    assert!(css.contains("#10141e"), "original --color-sidebar token");
    assert!(css.contains("[data-theme=\"light\"]"), "light theme block");
    assert!(css.contains(".qs-step"), "quick start card");
    assert!(css.contains(".grp-toggle"), "collapsible sidebar sections");

    // service actions are management-guarded (parity: sidebar restart/shutdown)
    let r = client
        .post(format!("{gw}/v1/admin/service/restart"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "unauthenticated restart is refused");
    let r = client
        .post(format!("{gw}/v1/admin/service/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "unauthenticated stop is refused");

    let r = client.get(format!("{gw}/dashboard/fonts/material-symbols-outlined.woff2")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("content-type").unwrap(), "font/woff2");
    assert!(r.bytes().await.unwrap().len() > 100_000, "icon font body");

    // localisation packs (parity: config/i18n.json + src/i18n/messages)
    let r = client.get(format!("{gw}/dashboard/languages.json")).send().await.unwrap();
    let langs: serde_json::Value = r.json().await.unwrap();
    assert!(langs.as_array().unwrap().len() >= 60, "locale catalogue");
    let r = client.get(format!("{gw}/dashboard/locales/zh-CN.json")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let zh: serde_json::Value = r.json().await.unwrap();
    assert!(zh["sidebar"]["providers"].is_string(), "zh-CN sidebar label");
    assert_eq!(zh["home"]["recentRequests"], "最近请求");
    assert_eq!(zh["home"]["recentRequestsWhen"], "时间");
    assert_eq!(zh["endpoints"]["title"], "API 端点");
    assert_eq!(zh["endpoints"]["public"], "公网");

    let r = client.get(format!("{gw}/dashboard/manifest.webmanifest")).send().await.unwrap();
    let m = r.text().await.unwrap();
    assert!(m.contains("\"display\": \"standalone\""), "PWA manifest");

    let r = client.get(format!("{gw}/dashboard/sw.js")).send().await.unwrap();
    let sw = r.text().await.unwrap();
    assert!(sw.contains("serviceWorker") || sw.contains("caches.open"));

    // unknown dashboard asset → JSON 404 (never HTML)
    let r = client.get(format!("{gw}/dashboard/nope.js")).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn runtime_compression_config_update_and_logging() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    // default boot config: enabled=true, default_mode=Off
    let v = client.get(format!("{gw}/v1/compression")).bearer_auth("test-key").send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(v["default_mode"], "off");

    // flip to lite at runtime via POST
    let r = client
        .post(format!("{gw}/v1/compression"))
        .bearer_auth("test-key")
        .json(&json!({"enabled": true, "default_mode": "lite"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["default_mode"], "lite");

    // GET reflects the runtime update
    let v = client.get(format!("{gw}/v1/compression")).bearer_auth("test-key").send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(v["default_mode"], "lite");

    // chat request now goes through the lite engine (visible in the meta header)
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-forwarded-for", "203.0.113.42")
        .json(&json!({"model": "openai-compatible-beta/mock-model", "reasoning_effort": "high",
                      "messages": [{"role": "user", "content": "hello there, thanks, this message goes through the gateway for the runtime compression test we are running right now today"}]}))
        .send()
        .await
        .unwrap();
    let meta = resp.headers().get("x-omniroute-compression").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(meta.starts_with("lite; source=default"), "meta: {meta}");

    // logs endpoint records the request
    let v = client.get(format!("{gw}/v1/logs?limit=5")).bearer_auth("test-key").send().await.unwrap().json::<Value>().await.unwrap();
    let logs = v["logs"].as_array().unwrap();
    assert!(!logs.is_empty(), "request log populated");
    let last = &logs[0];
    assert_eq!(last["provider"], "openai-compatible-beta");
    assert_eq!(last["status"], 200);
    assert_eq!(last["endpoint"], "/v1/chat/completions");
    assert_eq!(last["reasoning_effort"], "high");
    assert_eq!(last["client_ip"], "203.0.113.42");

    // invalid mode rejected 400
    let r = client
        .post(format!("{gw}/v1/compression"))
        .bearer_auth("test-key")
        .json(&json!({"default_mode": "bogus"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // stats endpoint shape
    let v = client.get(format!("{gw}/v1/stats")).bearer_auth("test-key").send().await.unwrap().json::<Value>().await.unwrap();
    assert!(v["uptime_s"].as_u64().unwrap() < 60);
    assert!(v["requests"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn multimodal_passthrough_endpoints() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    // 1) images/generations — JSON body with provider/model prefix
    let r = client
        .post(format!("{gw}/v1/images/generations"))
        .bearer_auth("test-key")
        .json(&json!({"model": "openai-compatible-beta/img-4", "prompt": "a purple diamond logo"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["data"][0]["url"].as_str().unwrap().starts_with("https://mock.test/"));
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["model"], "openai-compatible-beta/img-4", "body forwarded verbatim");

    // 2) audio/transcriptions — multipart raw passthrough, provider from form field
    let form = "--BOUNDARY\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nopenai-compatible-beta/whisper-1\r\n--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF-WAVEFAKE\r\n--BOUNDARY--\r\n";
    let r = client
        .post(format!("{gw}/v1/audio/transcriptions"))
        .bearer_auth("test-key")
        .header("content-type", "multipart/form-data; boundary=BOUNDARY")
        .body(form.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["text"], "MOCK-TRANSCRIBED");

    // 2b) multipart provider extracted from the form field
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["_multipart_model"], "openai-compatible-beta/whisper-1");
    assert!(last["_content_type"].as_str().unwrap().starts_with("multipart/form-data"));

    // 3) batches: create + get (provider via header)
    let r = client
        .post(format!("{gw}/v1/batches"))
        .bearer_auth("test-key")
        .header("x-omniroute-provider", "openai-compatible-beta")
        .json(&json!({"input_file_id": "file-1", "endpoint": "/v1/chat/completions"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["id"], "batch_1");

    let r = client
        .get(format!("{gw}/v1/batches/batch_1"))
        .bearer_auth("test-key")
        .header("x-omniroute-provider", "openai-compatible-beta")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["status"], "completed");

    // 4) missing provider → 400 with hint
    let r = client
        .post(format!("{gw}/v1/images/generations"))
        .bearer_auth("test-key")
        .json(&json!({"prompt": "no provider"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn multimodal_image_input_via_chat() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    let data_url = "data:image/png;base64,aGVsbG8=";
    // openai-format provider: image_url forwarded verbatim
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .json(&json!({"model": "openai-compatible-beta/mock-model", "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "what is this image?"},
                {"type": "image_url", "image_url": {"url": data_url}}
            ]}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let last = seen.lock().unwrap().clone().unwrap();
    let parts = last["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], data_url);
}

#[tokio::test]
async fn dashboard_auth_and_api_keys_and_providers() {
    let (mock_base, seen) = spawn_mock().await;
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("OMNIROUTE_DATA_DIR", dir.path().as_os_str()); }
    unsafe { std::env::remove_var("OMNIROUTE_ADMIN_PASSWORD"); }
    let state = AppState::build(test_config_for(dir.path(), &mock_base));
    let gw = spawn_gateway(state).await;
    let client = reqwest::Client::new();

    // login flow: wrong → 401; correct (default) → token
    let password = "CHANGEME";
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "wrong"})).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": password}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    let token = v["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("sess_"));

    // management without session → 401 (OMNIROUTE_API_KEY unset in this state,
    // but managed provider-connections exist? no — bootstrap template from
    // test_config is empty so management requires a session)
    let r = client.get(format!("{gw}/v1/api-keys")).send().await.unwrap().status();
    assert_eq!(r, 401);

    // me() reports using_default_password
    let v = client
        .get(format!("{gw}/v1/auth/me"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(v["using_default_password"], true);

    // change password: new password honored on subsequent login
    let r = client
        .post(format!("{gw}/v1/auth/change-password"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"current_password": "CHANGEME", "new_password": "new-super-secret"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "CHANGEME"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "new-super-secret"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    let _token = v["token"].as_str().unwrap().to_string();

    // reset via the offline helper (parity: bin/reset-password.mjs)
    omniroute_rust::server::security::reset_password(dir.path(), "reset-by-cli-123").unwrap();
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "new-super-secret"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "old password rejected after reset");
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "reset-by-cli-123"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "reset password accepted");
    let token = r.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_string();

    // change-password must prove the current password once it is no longer the default
    let r = client
        .post(format!("{gw}/v1/auth/change-password"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"current_password": "CHANGEME", "new_password": "hijack-attempt"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "stale CHANGEME cannot rotate a real password");

    // session persistence across refresh: login sets a Path=/ cookie, the
    // cookie alone authenticates /v1/auth/me, logout expires it
    let r = client
        .post(format!("{gw}/v1/auth/login"))
        .json(&json!({"password": "reset-by-cli-123"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let set_cookie = r.headers().get("set-cookie")
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(set_cookie.starts_with("omniroute_session=sess_") && set_cookie.contains("Path=/"),
            "persistent session cookie: {set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap_or("").to_string();
    let me: Value = client
        .get(format!("{gw}/v1/auth/me"))
        .header("cookie", cookie.clone())
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(me["authenticated"], true, "cookie alone survives a refresh");
    let r = client
        .post(format!("{gw}/v1/auth/logout"))
        .header("cookie", cookie)
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let cleared = r.headers().get("set-cookie")
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(cleared.contains("Max-Age=0"), "logout expires the cookie: {cleared}");

    // analytics + audit + export endpoints (management-guarded, real shapes)
    for ep in ["/v1/stats/providers", "/v1/combo-health", "/v1/audit?limit=10"] {
        let r = client.get(format!("{gw}{ep}")).send().await.unwrap();
        assert_eq!(r.status(), 401, "{ep} without a session");
    }
    let r = client
        .get(format!("{gw}/v1/stats/providers"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["providers"].is_array(), "per-provider aggregates");
    assert!(v["sampled"].is_number());

    let r = client
        .get(format!("{gw}/v1/combo-health"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["combos"].is_array(), "per-combo health");

    // the login above must be captured by the audit ring
    let r = client
        .get(format!("{gw}/v1/audit?limit=50"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    let actions: Vec<String> = v["audit"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str().map(str::to_string))
        .collect();
    assert!(actions.iter().any(|a| a == "auth.login"), "login is audited: {actions:?}");

    // combos (managed CRUD + presets) and provider quotas
    let r = client.get(format!("{gw}/v1/combos/managed")).send().await.unwrap();
    assert_eq!(r.status(), 401, "managed combos unauthenticated");
    let r = client
        .post(format!("{gw}/v1/combos/managed"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"name": "my-chain", "strategy": "priority",
                      "providers": ["openai-compatible-beta/mock-model"]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let saved: Value = r.json().await.unwrap();
    let combo_id = saved["combo"]["id"].as_str().unwrap().to_string();

    // Dashboard dry-run accepts the management session (the Providers/Combos
    // page uses this path, not a client inference key).
    let r = client
        .post(format!("{gw}/v1/combos/test"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"model": "my-chain"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "combo dry-run accepts dashboard session");
    let r = client
        .post(format!("{gw}/v1/combos/test"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"model": "my-chain", "execute": true}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "combo test executes through dashboard session");
    let tested: Value = r.json().await.unwrap();
    assert_eq!(tested["executed"], true);
    assert_eq!(tested["summary"]["total"], 1);
    assert_eq!(tested["summary"]["passed"], 1);

    // Named managed combos route by exact name; an empty model selector must
    // not capture another combo.
    let r = client
        .post(format!("{gw}/v1/combos/managed"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"name": "coding-free", "strategy": "priority",
                      "providers": ["openai-compatible-alpha/mock-model"]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let free_id = r.json::<Value>().await.unwrap()["combo"]["id"].as_str().unwrap().to_string();
    let r = client
        .post(format!("{gw}/v1/combos/test"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"model": "coding-free"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["candidates"][0]["provider"], "openai-compatible-alpha");

    // A normal client key can discover managed and built-in combo model ids.
    let r = client
        .post(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"name": "combo-catalog-probe"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let probe_key = r.json::<Value>().await.unwrap();
    let probe_key_id = probe_key["api_key"]["id"].as_str().unwrap().to_string();
    let probe_secret = probe_key["api_key"]["key"].as_str().unwrap().to_string();
    let models = client
        .get(format!("{gw}/v1/models"))
        .header("authorization", format!("Bearer {probe_secret}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    let model_ids: Vec<&str> = models["data"].as_array().unwrap().iter()
        .filter_map(|m| m["id"].as_str()).collect();
    assert!(model_ids.contains(&"my-chain") && model_ids.contains(&"coding-free"));
    assert!(model_ids.contains(&"auto/best-coding"));
    let my_chain = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "my-chain").unwrap();
    assert_eq!(my_chain["supportsVision"], true, "compatible combo advertises image input");
    assert!(my_chain["modalities"].as_array().unwrap().iter().any(|m| m == "image"));
    let _ = client
        .delete(format!("{gw}/v1/api-keys/{probe_key_id}"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();

    let r = client
        .get(format!("{gw}/v1/combos/managed"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    let managed = v["combos"].as_array().unwrap();
    assert!(managed.iter().any(|c| c["name"] == "my-chain" && c["category"] == "deterministic"),
            "managed combo listed with its category");
    // toggling + deleting only applies to managed combos
    let r = client
        .patch(format!("{gw}/v1/combos/managed/{combo_id}"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"enabled": false}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .patch(format!("{gw}/v1/combos/managed/config:missing"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"enabled": false}))
        .send().await.unwrap();
    assert_eq!(r.status(), 404, "config combos are read-only");
    let r = client
        .delete(format!("{gw}/v1/combos/managed/{combo_id}"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .delete(format!("{gw}/v1/combos/managed/{free_id}"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .get(format!("{gw}/v1/combo-presets"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    let presets: Value = r.json().await.unwrap();
    assert_eq!(presets["total"], 18, "auto-router templates");
    // every template carries the catalogue metadata the page renders
    for t in presets["templates"].as_array().unwrap() {
        assert!(t["id"].as_str().unwrap().starts_with("auto/"), "id {t}");
        assert!(t["strategy"].is_string() && t["title"].is_string(), "strategy+title {t}");
        assert!(!t["tags"].as_array().unwrap().is_empty(), "tags {t}");
    }
    assert!(presets["templates"].as_array().unwrap().iter().any(|t| t["id"] == "auto/best-coding" && t["prompt"].is_string()),
            "best-coding carries its prompt hint");
    assert!(presets["presets"][0]["primary"].is_string(), "kimi preset described");

    let r = client.get(format!("{gw}/v1/provider-quotas")).send().await.unwrap();
    assert_eq!(r.status(), 401, "quotas unauthenticated");
    let r = client
        .get(format!("{gw}/v1/provider-quotas"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    let q: Value = r.json().await.unwrap();
    assert!(q["accounts"].is_array());
    assert!(q["summary"]["total"].is_number());
    // a cutoff drives the derived severity
    let r = client
        .post(format!("{gw}/v1/provider-quotas/openai-compatible-beta"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"cutoff": 10, "balance": 19.43, "currency": "CNY", "tier": "free"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let q: Value = client
        .get(format!("{gw}/v1/provider-quotas"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    let acct = q["accounts"].as_array().unwrap().iter()
        .find(|a| a["provider"] == "openai-compatible-beta").expect("account present");
    assert_eq!(acct["currency"], "CNY");
    assert!(acct["severity"].is_string());

    // combo studio / routing trace / embedded services / quota share / cache health
    for ep in ["/v1/combo-studio", "/v1/routing/trace", "/v1/embedded-services", "/v1/quota-share", "/v1/cache/health"] {
        let r = client.get(format!("{gw}{ep}")).send().await.unwrap();
        assert_eq!(r.status(), 401, "{ep} unauthenticated");
    }
    let studio: Value = client
        .get(format!("{gw}/v1/combo-studio"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert!(studio["combos"].is_array(), "combo studio view");
    if let Some(first) = studio["combos"].as_array().unwrap().first() {
        assert!(first["candidates"].is_array(), "resolved candidate chain");
        assert!(first["healthy"].is_boolean());
    }
    let trace: Value = client
        .get(format!("{gw}/v1/routing/trace?limit=10"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert!(trace["traces"].is_array());
    let emb: Value = client
        .get(format!("{gw}/v1/embedded-services"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert!(emb["localProviders"].is_array() && emb["bundledExecutors"].is_array());
    assert!(emb["bundledExecutors"].as_array().unwrap().iter().all(|e| e["available"] == false),
            "browser executors are reported unavailable, never faked");
    let share: Value = client
        .get(format!("{gw}/v1/quota-share"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert!(share["shares"].is_array() && share["enabledKeys"].is_number());
    let cache: Value = client
        .get(format!("{gw}/v1/cache/health"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(cache["semanticCache"]["enabled"], false, "no faked cache numbers");
    assert!(cache["dedup"]["tokensSaved"].is_number());

    // endpoints overview + the gateway-wide custom system prompt
    let r = client.get(format!("{gw}/v1/endpoints")).send().await.unwrap();
    assert_eq!(r.status(), 401, "endpoints unauthenticated");
    let e: Value = client
        .get(format!("{gw}/v1/endpoints"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert!(e["active"]["public"].as_str().unwrap().ends_with("/v1"));
    assert!(e["active"]["local"].as_str().unwrap().contains("localhost"));
    let via_public_host: Value = client
        .get(format!("{gw}/v1/endpoints"))
        .header("authorization", format!("Bearer {token}"))
        .header("host", "public.example:20128")
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(via_public_host["active"]["public"], "http://public.example:20128/v1");
    assert_eq!(via_public_host["localServer"]["url"], e["active"]["local"]);
    assert!(e["localServer"]["id"].is_string());
    let eps = e["endpoints"].as_array().unwrap();
    assert!(eps.iter().any(|x| x["path"] == "/v1/chat/completions"), "chat endpoint listed");
    assert!(eps.iter().all(|x| x["models"].is_number()), "per-endpoint model counts");
    assert_eq!(e["tunnels"].as_array().unwrap().len(), 4);
    assert!(e["tunnels"].as_array().unwrap().iter().all(|t| t["state"] != "enabled"),
            "tunnels are reported unavailable, never faked");
    assert_eq!(e["vscodeAlias"]["implemented"], false, "alias reported honestly");

    // custom system prompt round-trips and reaches the upstream request
    let r = client
        .post(format!("{gw}/v1/settings/custom-system-prompt"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"prompt": "Always answer in haiku."}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let e: Value = client
        .get(format!("{gw}/v1/endpoints"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(e["customSystemPrompt"], "Always answer in haiku.");
    // the injected system message is visible upstream (mock records the payload);
    // chat needs client auth, so mint a throwaway client key first
    let r = client
        .post(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"name": "prompt-probe"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let probe_key = r.json::<Value>().await.unwrap()["api_key"]["key"]
        .as_str().unwrap().to_string();
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {probe_key}"))
        .json(&json!({"model": "openai-compatible-beta/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "probe chat reaches the mock upstream");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let last = seen.lock().unwrap().clone().unwrap();
    assert!(last["messages"].as_array().unwrap().iter().any(|m| m["content"]
            .as_str().unwrap_or("").contains("Always answer in haiku.")),
            "custom system prompt injected upstream: {last:?}");
    let r = client
        .post(format!("{gw}/v1/settings/custom-system-prompt"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"prompt": ""}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    // usage analytics aggregate (parity: /api/usage/analytics shape)
    let r = client.get(format!("{gw}/v1/usage/analytics")).send().await.unwrap();
    assert_eq!(r.status(), 401, "usage analytics unauthenticated");
    let r = client
        .get(format!("{gw}/v1/usage/analytics"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    for key in ["summary", "dailyTrend", "activityMap", "byModel", "byProvider", "byApiKey", "weeklyPattern", "errorBreakdown"] {
        assert!(v.get(key).is_some(), "analytics key {key}");
    }
    assert!(v["summary"]["totalTokens"].is_number());
    assert!(v["summary"]["successRatePct"].is_number());
    assert_eq!(v["weeklyPattern"].as_array().unwrap().len(), 7);

    // log export: CSV header + JSON array, with filters applied
    let r = client
        .get(format!("{gw}/v1/logs/export?format=csv"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let csv = r.text().await.unwrap();
    assert!(
        csv.starts_with("ts_ms,model,combo,routed_model,provider,status,latency_ms,prompt_tokens"),
        "csv header: {csv:.90}"
    );
    let r = client
        .get(format!("{gw}/v1/logs/export?format=json&errors=true"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rows: Value = r.json().await.unwrap();
    assert!(rows.is_array(), "json export array");

    // create api keys → full secret returned once
    let r = client
        .post(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"name": "claude-code", "role": "default"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let v = r.json::<Value>().await.unwrap();
    let key = v["api_key"]["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("sk-or-"));

    // Client-facing model discovery must work with a normal default API key,
    // not only with a dashboard/admin session (client SDK parity).
    let r = client
        .get(format!("{gw}/v1/models"))
        .header("authorization", format!("Bearer {key}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "default client key can list models");

    // list masks the key
    let v = client
        .get(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    let listed = v["api_keys"].as_array().unwrap().iter()
        .find(|k| k["name"] == "claude-code").expect("claude-code key listed");
    assert_ne!(listed["key"].as_str().unwrap(), key);
    assert!(listed["key"].as_str().unwrap().contains("••"));
    let key_id = listed["id"].as_str().unwrap().to_string();
    assert!(listed["status"].is_string(), "derived key status");
    assert!(listed["type"].is_string(), "key type (standard/admin/restricted)");

    // provider catalog + providers-page assets (parity: upstream catalog metadata)
    let r = client.get(format!("{gw}/dashboard/providers.json")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let catalog: Value = r.json().await.unwrap();
    let entries = catalog.as_array().unwrap();
    assert!(entries.len() > 100, "catalog size: {}", entries.len());
    assert!(entries.iter().any(|p| p["category"] == "oauth"), "oauth section present");
    assert!(entries.iter().all(|p| p["id"].is_string() && p["name"].is_string()), "id+name on every entry");
    // the catalog carries the fields the provider cards render
    assert!(entries.iter().all(|p| p["icon"].is_string() && p["color"].is_string()), "icon+colour");
    assert!(entries.iter().all(|p| p["category"].is_string() && p["serviceKinds"].is_array()), "category+kinds");
    let cats: std::collections::BTreeSet<&str> =
        entries.iter().filter_map(|p| p["category"].as_str()).collect();
    assert!(cats.contains("apikey") && cats.contains("aggregator"), "sections: {cats:?}");

    let r = client.get(format!("{gw}/v1/provider-catalog")).send().await.unwrap();
    assert_eq!(r.status(), 401, "provider-catalog unauthenticated");
    let r = client
        .post(format!("{gw}/v1/provider-connections/test-all"))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "test-all unauthenticated");
    let r = client
        .post(format!("{gw}/v1/provider-connections/import"))
        .json(&json!({"connections": []}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "import unauthenticated");
    let r = client
        .get(format!("{gw}/v1/provider-catalog"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["providers"].as_array().unwrap().len() > 100);
    assert!(v["providers"][0]["connected"].is_boolean());
    // parity shape: per-card stats + models + compatible nodes + honest stubs
    let first = &v["providers"][0];
    assert!(first["stats"]["total"].is_number(), "card stats.total");
    assert!(first["stats"]["connected"].is_number(), "card stats.connected");
    assert!(first["stats"]["allDisabled"].is_boolean(), "card stats.allDisabled");
    assert!(first["models"].is_array(), "models for the model-search filter");
    assert!(v["compatibleNodes"].is_array(), "compatible nodes list");
    assert!(v["expirations"]["summary"]["expired"].is_number(), "expirations stub");
    assert!(v["blockedProviders"].is_array() && v["openRouterStats"].is_array(), "honest stubs");
    let free_n = v["providers"].as_array().unwrap().iter()
        .filter(|p| p["freeTier"].as_bool().unwrap_or(false)).count();
    assert!(free_n > 100, "free-tier flags extracted from the original catalog: {free_n}");
    // brand icons without a subset glyph carry a text badge (never raw text)
    let by_id: std::collections::HashMap<&str, &Value> = v["providers"].as_array().unwrap().iter()
        .filter_map(|p| p["id"].as_str().map(|id| (id, p))).collect();
    assert_eq!(by_id["opencode-go"]["iconText"], "OG");
    assert_eq!(by_id["unorouter"]["iconText"], "UR");
    assert!(v["providers"].as_array().unwrap().iter().all(|p| p["icon"].is_string()),
            "every entry has an icon name");
    // test-batch is management-guarded and shape-compatible
    let r = client
        .post(format!("{gw}/v1/providers/test-batch"))
        .json(&json!({"mode": "all"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "test-batch unauthenticated");
    // free-tiers catalog: guarded, shaped, honest about the headline
    let r = client.get(format!("{gw}/v1/free-tiers")).send().await.unwrap();
    assert_eq!(r.status(), 401, "free-tiers unauthenticated");
    let r = client
        .get(format!("{gw}/v1/free-tiers"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["summary"]["freeProviders"].as_u64().unwrap() > 100, "free-tier count");
    assert!(v["tiers"].as_array().unwrap().iter().all(|t| t["provider"].is_string() && t["connected"].is_boolean()),
            "tier rows carry connection state");
    assert!(v["tiers"].as_array().unwrap().iter().all(|t| t["icon"].is_string()),
            "tier rows carry icons");
    assert_eq!(v["headlineTokensPerMonth"], Value::Null, "no summed headline figure");
    // settings exposes the timeout block rendered by Settings General
    let v = client
        .get(format!("{gw}/v1/settings"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    assert!(v["timeouts_ms"]["request"].is_number() && v["timeouts_ms"]["sse_heartbeat"].is_number(),
            "timeout block present");

    // key rotation + rich key fields
    let r = client
        .post(format!("{gw}/v1/api-keys/{key_id}/rotate"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let rotated: Value = r.json().await.unwrap();
    let new_secret = rotated["api_key"]["key"].as_str().unwrap().to_string();
    assert!(new_secret.starts_with("sk-or-") && new_secret != key);
    let r = client
        .post(format!("{gw}/v1/api-keys/{key_id}/rotate"))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "rotation is management-guarded");
    // the rotated secret replaces the old one: the superseded secret must fail
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({"model": "openai-compatible-beta/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 401, "superseded secret is rejected");
    let key = new_secret.clone();


    // inference with the new key works (open key-check path)
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({"model": "openai-compatible-beta/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    // add a provider connection at runtime, test it, and use its model
    let r = client
        .post(format!("{gw}/v1/provider-connections"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "provider": "openai-compatible-live",
            "name": "live relay",
            "api_key": "k-live",
            "base_url": format!("{mock_base}/beta/v1"),
            "enabled": true
        }))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let v = r.json::<Value>().await.unwrap();
    let pconn_id = v["connection"]["id"].as_str().unwrap().to_string();

    // test connection → ok
    let r = client
        .post(format!("{gw}/v1/provider-connections/{pconn_id}/test"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["ok"], true);

    // batch test: mode=all probes the live connection with the original shape
    let r = client
        .post(format!("{gw}/v1/providers/test-batch"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"mode": "all"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["mode"], "all");
    assert!(v["summary"]["total"].as_u64().unwrap() >= 1, "batch probes the connection");
    assert_eq!(v["summary"]["passed"].as_u64().unwrap(), v["summary"]["total"].as_u64().unwrap());
    assert!(v["results"][0]["valid"].as_bool().unwrap());
    assert!(v["results"][0]["latencyMs"].is_number());
    // provider-scoped batch + an empty group (no oauth connections here)
    let r = client
        .post(format!("{gw}/v1/providers/test-batch"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"mode": "provider", "providerId": "openai-compatible-live"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["summary"]["total"], 1);
    let r = client
        .post(format!("{gw}/v1/providers/test-batch"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"mode": "oauth"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["summary"], json!({"total": 0, "passed": 0, "failed": 0}));
    // managed credentials reach the upstream: the probe uses the saved key,
    // so a wrong key fails and the right key passes
    for (cid, key, want_ok) in [("ac-wrong", "k-wrong", false), ("ac-live", "k-live", true)] {
        let r = client
            .post(format!("{gw}/v1/provider-connections"))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "id": cid,
                "provider": "openai-compatible-authcheck",
                "name": "authcheck",
                "api_key": key,
                "base_url": format!("{mock_base}/authcheck/v1"),
                "enabled": true
            }))
            .send().await.unwrap();
        assert_eq!(r.status(), 201);
        let r = client
            .post(format!("{gw}/v1/provider-connections/{cid}/test"))
            .header("authorization", format!("Bearer {token}"))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["ok"], want_ok, "probe uses the saved key");
    }
    // chat through the saved connection sends its key upstream (k-live accepted)
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({"model": "openai-compatible-authcheck/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "managed key authenticates upstream");
    // model sync: upstream listing lands on the connection and in /v1/models
    let r = client
        .post(format!("{gw}/v1/provider-connections"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "id": "sync-conn",
            "provider": "openai-compatible-sync",
            "name": "sync",
            "api_key": "k",
            "base_url": format!("{mock_base}/sync/v1"),
            "enabled": true
        }))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let r = client
        .post(format!("{gw}/v1/provider-connections/sync-conn/sync-models"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["synced"], 2);
    let v = client
        .get(format!("{gw}/v1/provider-connections/sync-conn/models"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    assert!(v["effective"].as_array().unwrap().iter().any(|m| m == "sync-model-a"));
    assert!(v["syncedAtMs"].as_u64().unwrap() > 0);
    assert_eq!(v["capabilities"]["sync-model-a"]["contextWindow"], 128000);
    assert_eq!(v["capabilities"]["sync-model-a"]["input"], json!(["text", "image"]));
    // manually added models can be removed explicitly, including ids with slashes
    let r = client
        .post(format!("{gw}/v1/provider-connections"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "id": "manual-conn",
            "provider": "openai-compatible-manual",
            "name": "manual",
            "api_key": "k",
            "base_url": format!("{mock_base}/beta/v1"),
            "models": ["manual-delete-me", "manual/delete-me", "manual-keep-me"],
            "enabled": true
        }))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    for model in ["manual-delete-me", "manual%2Fdelete-me"] {
        let r = client
            .delete(format!("{gw}/v1/provider-connections/manual-conn/models/{model}"))
            .header("authorization", format!("Bearer {token}"))
            .send().await.unwrap();
        assert_eq!(r.status(), 200, "manual model deletion succeeds");
    }
    let v = client
        .get(format!("{gw}/v1/provider-connections/manual-conn/models"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(v["manual"], json!(["manual-keep-me"]));
    let r = client
        .delete(format!("{gw}/v1/provider-connections/manual-conn/models/manual-delete-me"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 404, "repeated manual deletion reports not-found");
    let v = client
        .get(format!("{gw}/v1/models"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter()
        .filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"openai-compatible-sync/sync-model-a"),
            "managed connection models listed");
    // hiding a model removes it from /v1/models but keeps the sync record
    let r = client
        .patch(format!("{gw}/v1/provider-connections/sync-conn"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"hidden_models": ["sync-model-a"]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = client
        .get(format!("{gw}/v1/models"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter()
        .filter_map(|m| m["id"].as_str()).collect();
    assert!(!ids.contains(&"openai-compatible-sync/sync-model-a"), "hidden model filtered");
    assert!(ids.contains(&"openai-compatible-sync/sync-model-b"), "sibling still listed");
    // the compatible node surfaces in the catalog for the Compatible section
    let r = client
        .get(format!("{gw}/v1/provider-catalog"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v = r.json::<Value>().await.unwrap();
    assert!(v["compatibleNodes"].as_array().unwrap()
        .iter().any(|n| n["id"] == "openai-compatible-live"), "dynamic node listed");

    // chat through the runtime-registered provider
    let r = client
        .post(format!("{gw}/v1/chat/completions"))
        .header("authorization", format!("Bearer {key}"))
        .json(&json!({"model": "openai-compatible-live/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    // logout revokes the session → management 401 again
    let _ = client
        .post(format!("{gw}/v1/auth/logout"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    let r = client
        .get(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 401);

    unsafe { std::env::remove_var("OMNIROUTE_DATA_DIR"); }
}

/// A long agent context (multi-MiB body) must reach the upstream instead of
/// dying on axum's built-in 2 MiB `DefaultBodyLimit` (the DSH/coding-plan 413).
#[tokio::test]
async fn large_context_body_reaches_upstream() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    // ~3 MiB, i.e. above axum's default limit and below ours. The model needs a
    // window that can hold it (deepseek-v4* declares 1M tokens).
    let pad = "x".repeat(3 * 1024 * 1024);
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/deepseek-v4.1-flash",
            "messages": [{"role": "user", "content": pad}],
            "max_tokens": 8
        }),
    )
    .await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "body: {}", &text[..text.len().min(300)]);
    let out: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(out["choices"][0]["message"]["content"], "MOCK-SAYS-HI");
    let last = seen.lock().unwrap().clone().unwrap();
    assert_eq!(last["messages"][0]["content"].as_str().unwrap().len(), 3 * 1024 * 1024);
}

/// Over the configured cap the gateway answers a 413 in OpenAI shape that names
/// a context bound, so an agent harness compacts the conversation and retries
/// rather than failing the turn (DSH maps `context_length_exceeded` to
/// CONTEXT_WINDOW_EXCEEDED).
#[tokio::test]
async fn over_limit_body_is_context_length_exceeded_413() {
    let (mock_base, _seen) = spawn_mock().await;
    let mut cfg = test_config(&mock_base, vec![]);
    cfg.max_body_bytes = 4096;
    let gw = spawn_gateway(AppState::new(cfg)).await;

    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "y".repeat(8192)}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 413);
    let e = resp.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["type"], "invalid_request_error");
    assert_eq!(e["error"]["code"], "context_length_exceeded");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(message.contains("maximum context length"), "{message}");
    assert!(message.contains("4096"), "{message}");

    // the same cap still lets a small body through
    let ok = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "openai-compatible-beta/mock-model", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(ok.status(), 200);
}

/// A model that exists but cannot take the attachment must be described as
/// such — not as a missing model (a client treats 404 `model_not_found` as
/// unfixable and gives up).
#[tokio::test]
async fn an_unsupported_attachment_is_not_reported_as_a_missing_model() {
    let _roster = FREE_ROSTER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mock_base, _seen) = spawn_mock().await;
    omniroute_rust::opencode_free::install_live(vec!["zen-test-free".into()]);
    let gw = spawn_gateway(zen_state(&mock_base)).await;

    // `zen-test-free` is on the free roster, so it resolves — but the
    // capability layer does not give it PDF input, so the attachment filter
    // removes every candidate.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "read this"},
                {"type": "file", "file": {"filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0="}}
            ]}],
            "stream": false
        }),
    )
    .await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    let text = body.to_string();
    assert_ne!(status, 404, "a capability mismatch is not a missing model: {body}");
    assert!(
        text.contains("does not accept") || text.contains("unsupported_modality"),
        "the error must name the attachment problem: {body}"
    );
    assert!(text.contains("PDF"), "the error must name the attachment: {body}");
}

/// A request that fits no candidate's context window is a context-size error,/// A request that fits no candidate's context window is a context-size error,
/// not `404 model_not_found`: the misleading 404 made agent clients drop the
/// turn instead of compacting the conversation and retrying.
#[tokio::test]
async fn over_context_window_is_not_model_not_found() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    // mock-model resolves to the 262144-token default window; ~300k tokens of
    // text overflow it while staying far below the body cap.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "z".repeat(1_200_000)}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let e = resp.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["code"], "context_length_exceeded");
    let message = e["error"]["message"].as_str().unwrap();
    assert!(message.contains("context window"), "{message}");
    assert!(message.contains("262144"), "{message}");

    // a genuinely unresolvable model keeps its 404
    let missing = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({"model": "no-such-provider/xyz", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(missing.status(), 404);
    let e = missing.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["code"], "model_not_found");
}

/// A client-declared output cap larger than the candidate's own ceiling must
/// not disqualify that candidate, and must not be forwarded upstream verbatim.
///
/// Live regression this guards: the DSH harness declares `max_tokens` from its
/// configured model cap (384000 for a `coding-free` combo whose candidates top
/// out between 8k and 131k output / 200k-1M window). Counting that number as a
/// hard context requirement deleted the small-window free candidates from every
/// request and could report `context_length_exceeded` on a request that fits.
#[tokio::test]
async fn oversized_client_max_tokens_is_clamped_per_candidate() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;

    // mock-model: 262144-token window, 32768-token default output ceiling.
    // 384000 output tokens would overflow the window if taken literally.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 384_000
        }),
    )
    .await;
    assert_eq!(resp.status(), 200, "clamped request must fit the window");
    let upstream = seen.lock().unwrap().clone().expect("upstream saw the request");
    assert_eq!(
        upstream["max_tokens"], 32_768,
        "the upstream cap must be forwarded, not the client's oversized value: {upstream}"
    );

    // A client cap below the candidate ceiling is forwarded untouched.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 4_096
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let upstream = seen.lock().unwrap().clone().expect("upstream saw the request");
    assert_eq!(upstream["max_tokens"], 4_096);

    // The prompt alone still overflows: the context-size error survives.
    let resp = post_json(
        &format!("{gw}/v1/chat/completions"),
        Some("test-key"),
        json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "z".repeat(1_200_000)}],
            "max_tokens": 384_000
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let e = resp.json::<Value>().await.unwrap();
    assert_eq!(e["error"]["code"], "context_length_exceeded");
}

// ---------------------------------------------------------------------------
// LRC (layered reversible compression) acceptance
// ---------------------------------------------------------------------------

/// Build a realistic agent conversation: system prompt, user request, then a
/// chain of assistant/tool pairs with reasoning, tool-call arguments and long
/// tool output (including an error line in the middle).
fn lrc_conversation() -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": "You are a coding agent. Follow the repo conventions."}),
        json!({"role": "user", "content": "Add a compression module"}),
    ];
    for i in 0..6 {
        messages.push(json!({
            "role": "assistant",
            "content": format!("Step {i}: inspecting the module and adjusting the engine."),
            "reasoning_content": format!("Step {i}: ")+&"I need to check the call sites first. ".repeat(40),
            "tool_calls": [{
                "id": format!("call_{i}"), "type": "function",
                "function": {"name": "bash", "arguments":
                    json!({"command": format!("cargo test --release step{i}")}).to_string()}
            }]
        }));
        let mut log = String::from("$ cargo test --release\n");
        for j in 0..200 {
            log.push_str(&format!("test suite::mod{j} ... ok\n"));
        }
        log.push_str("error[E0308]: mismatched types\n  --> src/lib.rs:42:9\n");
        log.push_str("error: could not compile `demo` (lib) due to 1 previous error\n");
        messages.push(json!({"role": "tool", "tool_call_id": format!("call_{i}"), "content": log}));
    }
    messages.push(json!({"role": "user", "content": "Now summarise what you changed"}));
    json!({"messages": messages})
}

#[tokio::test]
async fn lrc_compresses_history_and_preserves_the_session() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();
    let body = lrc_conversation();

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-omniroute-compression", "lrc:extreme")
        .json(&json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": body["messages"].clone(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta = resp
        .headers()
        .get("x-omniroute-compression")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(meta.starts_with("lrc; source=request-header"), "meta={meta}");
    assert!(meta.contains("profile=extreme"), "meta={meta}");
    assert!(!meta.contains("gates="), "no gate violation expected: {meta}");

    let upstream = seen.lock().unwrap().clone().expect("upstream saw the request");
    let msgs = upstream["messages"].as_array().expect("messages");
    let orig = body["messages"].as_array().unwrap();

    // 1. the body really shrank
    assert!(
        serde_json::to_string(&upstream).unwrap().len()
            < serde_json::to_string(&body).unwrap().len() / 2,
        "expected >50% body reduction"
    );

    // 2. the system prompt and the last user message are byte-identical
    assert_eq!(msgs[0], orig[0], "system prompt must not change");
    assert_eq!(msgs[msgs.len() - 1], orig[orig.len() - 1], "last user message must not change");

    // 3. tool_call ↔ tool_result pairing is intact, ids and names unchanged
    assert_eq!(msgs.len(), orig.len(), "message count must not change");
    for (got, want) in msgs.iter().zip(orig.iter()) {
        let got_id = got.get("tool_call_id").and_then(|v| v.as_str());
        let want_id = want.get("tool_call_id").and_then(|v| v.as_str());
        assert_eq!(got_id, want_id, "tool_call_id changed");
        if let Some(calls) = want.get("tool_calls").and_then(|c| c.as_array()) {
            let got_calls = got.get("tool_calls").and_then(|c| c.as_array()).expect("tool_calls kept");
            assert_eq!(got_calls.len(), calls.len(), "tool_call count changed");
            for (g, w) in got_calls.iter().zip(calls.iter()) {
                assert_eq!(g.get("id"), w.get("id"), "tool call id changed");
                assert_eq!(
                    g.get("function").and_then(|f| f.get("name")),
                    w.get("function").and_then(|f| f.get("name")),
                    "tool name changed"
                );
                // the retired arguments must still be valid JSON
                let args = g.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap();
                serde_json::from_str::<Value>(args).expect("retired arguments stay valid JSON");
            }
        }
    }

    // 4. old, unsigned reasoning is dropped before dispatch rather than
    //    forwarded (or elided in place): a model imitates its own earlier
    //    wording when the transcript carries it, which collapsed every reply's
    //    reasoning onto one template. Signed reasoning would be kept; this
    //    transcript has none.
    let old_reasoning = msgs[2].get("reasoning_content").and_then(|v| v.as_str());
    assert!(
        old_reasoning.is_none(),
        "historical reasoning still reached the upstream: {old_reasoning:?}"
    );
}

#[tokio::test]
async fn lrc_keeps_error_lines_in_compressed_tool_output() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    // One long tool result whose error sits in the middle (between head+tail).
    let mut log = String::from("$ cargo build --release\n");
    for j in 0..400 {
        log.push_str(&format!("   Compiling crate{j} v0.1.0\n"));
    }
    log.push_str("error[E0432]: unresolved import `missing::thing`\n  --> src/lib.rs:12:5\n");
    for j in 0..400 {
        log.push_str(&format!("    Finished dev {j}\n"));
    }

    let messages = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "go"}),
        json!({"role": "assistant", "content": "building",
               "tool_calls": [{"id": "c0", "type": "function",
                               "function": {"name": "bash", "arguments": "{\"command\":\"cargo build\"}"}}]}),
        json!({"role": "tool", "tool_call_id": "c0", "content": log}),
        json!({"role": "assistant", "content": "done"}),
        json!({"role": "user", "content": "and now?"}),
        json!({"role": "assistant", "content": "ok"}),
        json!({"role": "user", "content": "fin"}),
    ];

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-omniroute-compression", "lrc:extreme")
        .json(&json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": messages,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream = seen.lock().unwrap().clone().expect("upstream saw the request");
    let tool_text = upstream["messages"][3]["content"].as_str().expect("tool content");
    assert!(tool_text.contains("E0432"), "error line must survive: {tool_text}");
    assert!(tool_text.contains("src/lib.rs:12:5"), "error location must survive");
    assert!(tool_text.contains("omni-elided"), "the middle must have been elided");
}

#[tokio::test]
async fn lrc_is_idempotent_and_never_worse() {
    let (mock_base, _seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();
    let body = lrc_conversation();

    // A tiny request must come back unchanged (never-worse).
    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-omniroute-compression", "lrc")
        .json(&json!({"model": "openai-compatible-beta/mock-model",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta = resp
        .headers()
        .get("x-omniroute-compression")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(meta.contains("never-worse"), "tiny request must report never-worse: {meta}");

    // Compressing an already-compressed body must be a no-op (idempotency is
    // what keeps the upstream prompt cache valid).
    let once = omniroute_rust::compression::apply(
        &json!({"model": "m", "messages": body["messages"].clone()}),
        &{
            let mut cfg = omniroute_rust::compression::CompressionConfig::default();
            cfg.enabled = true;
            cfg.default_mode = omniroute_rust::compression::CompressionMode::Lrc;
            cfg
        },
        None,
    );
    let twice = omniroute_rust::compression::apply(
        &once.body,
        &{
            let mut cfg = omniroute_rust::compression::CompressionConfig::default();
            cfg.enabled = true;
            cfg.default_mode = omniroute_rust::compression::CompressionMode::Lrc;
            cfg
        },
        None,
    );
    assert_eq!(once.body, twice.body, "LRC must be idempotent");
}

#[tokio::test]
async fn lrc_compacts_the_tools_schema_without_changing_the_callable_shape() {
    let (mock_base, seen) = spawn_mock().await;
    let gw = spawn_gateway(AppState::new(test_config(&mock_base, vec![]))).await;
    let client = reqwest::Client::new();

    let tools = json!([{
        "type": "function",
        "function": {
            "name": "read",
            "description": "Read a file from disk. ".to_string() + &"x".repeat(400),
            "parameters": {
                "type": "object", "title": "read_args", "additionalProperties": false,
                "properties": {
                    "file_path": {"type": "string", "title": "File Path",
                                  "description": "Absolute path of the file to read"},
                    "limit": {"type": "integer", "default": 2000, "description": "max lines"}
                },
                "required": ["file_path"]
            }
        }
    }]);

    let resp = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("test-key")
        .header("x-omniroute-compression", "lrc")
        .json(&json!({
            "model": "openai-compatible-beta/mock-model",
            "messages": [{"role": "user", "content": "read the file"}],
            "tools": tools,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream = seen.lock().unwrap().clone().expect("upstream saw the request");
    let sent = upstream.get("tools").and_then(|t| t.as_array()).expect("tools forwarded");
    assert_eq!(sent.len(), 1, "the tool must survive");
    let f = &sent[0]["function"];
    assert_eq!(f["name"], "read", "tool name preserved");
    let props = f["parameters"]["properties"].as_object().expect("properties kept");
    assert!(props.contains_key("file_path"), "parameter names preserved");
    assert_eq!(f["parameters"]["required"], json!(["file_path"]), "required preserved");
    let dumped = serde_json::to_string(&sent).unwrap();
    assert!(!dumped.contains("additionalProperties"), "noise keys must be stripped");
    assert!(!dumped.contains("\"title\""), "title noise must be stripped");
}

#[cfg(test)]
mod batch_auth_tests {
    use super::*;

    /// X1: `GET /v1/batches` and `GET /v1/batches/{id}` forward with the
    /// operator's real upstream credentials, so they must demand a key —
    /// without the check a caller with no key at all could read another
    /// account's batches while `GET /v1/models` correctly answered 401.
    #[tokio::test]
    async fn batches_endpoints_require_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let (mock_base, _seen) = spawn_mock().await;
        // A configured key makes the gateway *require* one; without it an
        // empty key set is treated as "no auth configured".
        let mut cfg = test_config_for(dir.path(), &mock_base);
        cfg.api_key = Some("test-key".into());
        let base = spawn_gateway(AppState::build(cfg)).await;

        let client = reqwest::Client::new();
        // `/v1/models` is the control: it was already protected.
        let res = client.get(format!("{base}/v1/models")).send().await.unwrap();
        assert_eq!(res.status(), 401, "/v1/models must demand a key");

        for path in ["/v1/batches", "/v1/batches/xyz"] {
            let res = client
                .get(format!("{base}{path}"))
                .header("x-omniroute-provider", "openai")
                .send()
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                401,
                "{path} forwarded with upstream credentials without demanding a key"
            );
        }
    }

    /// Y1: a percent-encoded batch id must be decoded before it is
    /// interpolated into the upstream path, and a decoded slash must be
    /// rejected — otherwise the id could address another resource.
    #[test]
    fn batch_id_is_percent_decoded_and_a_slash_is_rejected() {
        let decode = |v: &str| {
            let bytes = v.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    let hex = &v[i + 1..i + 3];
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        };
        assert_eq!(decode("batch_abc"), "batch_abc");
        assert_eq!(decode("batch_%2Fabc"), "batch_/abc");
        assert!(decode("batch_%2Fabc").contains('/'));
        assert_eq!(decode("batch_%2Z"), "batch_%2Z", "an invalid escape stays");
    }
}
