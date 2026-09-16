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
        .route("/beta/v1/models", post(openai_chat))
        .route("/beta/v1/images/generations", post(images_generations))
        .route("/beta/v1/audio/transcriptions", post(audio_transcribe))
        .route("/beta/v1/batches", post(batches_create))
        .route("/beta/v1/batches/{id}", get(batches_get))
        .route("/gemini/models/{m}", post(gemini_generate))
        .route("/claude/v1/messages", post(claude_messages))
        .with_state(seen)
}

async fn spawn_mock() -> (String, Seen) {
    let seen: Seen = Arc::new(std::sync::Mutex::new(None));
    let app = mock_router(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), seen)
}

async fn spawn_gateway(state: AppState) -> String {
    let app = omniroute_rust::server::build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

fn test_config(mock_base: &str, combos: Vec<ComboConfig>) -> Config {
    let mut cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
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
    assert!(js.contains("buildSidebar"));
    // the sidebar must be built on boot, not only on a language switch (regression)
    assert!(js.contains("buildSidebar();"));
    assert!(js.contains("function showLogin"), "login overlay helper");
    assert!(js.contains("async function api"), "authenticated fetch helper");
    assert!(!js.contains("tBodies"), "tbody targets are written directly");

    // self-hosted icon font (parity: globals.css material-symbols import)
    let r = client.get(format!("{gw}/dashboard/app.css")).send().await.unwrap();
    assert!(r.status().is_success());
    let css = r.text().await.unwrap();
    assert!(css.contains("Material Symbols Outlined"));
    assert!(css.contains("--fd-sidebar-width: 220px"), "original sidebar width token");
    assert!(css.contains("#10141e"), "original --color-sidebar token");

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
    let (mock_base, seen) = spawn_mock().await;
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
        .json(&json!({"model": "openai-compatible-beta/mock-model",
                      "messages": [{"role": "user", "content": "hello there, thanks, this message goes through the gateway for the runtime compression test we are running right now today"}]}))
        .send()
        .await
        .unwrap();
    let meta = resp.headers().get("x-omniroute-compression").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(meta.starts_with("lite; source=default"), "meta: {meta}");

    // logs endpoint records the request
    let v = client.get(format!("{gw}/v1/logs?limit=5")).bearer_auth("test-key").send().await.unwrap().json::<Value>().await.unwrap();
    let logs = v["logs"].as_array().unwrap();
    assert!(logs.len() >= 1, "request log populated");
    let last = &logs[0];
    assert_eq!(last["provider"], "openai-compatible-beta");
    assert_eq!(last["status"], 200);

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
    let token = v["token"].as_str().unwrap().to_string();

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

    // list masks the key
    let v = client
        .get(format!("{gw}/v1/api-keys"))
        .header("authorization", format!("Bearer {token}"))
        .send().await.unwrap().json::<Value>().await.unwrap();
    let listed = &v["api_keys"][0];
    assert_ne!(listed["key"].as_str().unwrap(), key);
    assert!(listed["key"].as_str().unwrap().contains("••"));

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
