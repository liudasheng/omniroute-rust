//! End-to-end integration tests: full gateway with a local mock upstream.
//! Covers: non-stream + SSE chat, claude-native + openai-compatible upstreams,
//! failover on 500, model catalog, auth, count_tokens, health, 404s.

use axum::response::IntoResponse;
use axum::routing::post;
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

fn mock_router(seen: Seen) -> Router {
    Router::new()
        .route("/alpha/v1/chat/completions", post(alpha_chat))
        .route("/beta/v1/chat/completions", post(openai_chat))
        .route("/beta/v1/models", post(openai_chat))
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
