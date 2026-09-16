//! Minimal high-performance mock upstream used for gateway benchmarking.
//!
//! Endpoints:
//!   POST /v1/chat/completions  → JSON chat completion (instant, no network)
//!   (stream=true)              → SSE chunks + [DONE]
//!   GET  /healthz              → "ok"
//!
//! Usage: cargo run --release --example mock_upstream -- [port]

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use axum::Json;
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(9900);
    let app = Router::new().route("/v1/chat/completions", post(chat)).route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    println!("mock_upstream listening on 127.0.0.1:{port}");
    axum::serve(listener, app).await.unwrap();
}

async fn chat(Json(body): Json<Value>) -> impl IntoResponse {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("mock").to_string();
    if body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false) {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        tokio::spawn(async move {
            let chunk = |delta: Value| {
                json!({"id":"cmpl-bench","object":"chat.completion.chunk","created":1,"model":model,
                       "choices":[{"index":0,"delta":delta,"finish_reason":null}]}).to_string()
            };
            for text in ["Hello ", "from ", "the ", "mock ", "upstream."] {
                let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {}\n\n", chunk(json!({"content": text})))))).await;
            }
            let fin = json!({"id":"cmpl-bench","object":"chat.completion.chunk","created":1,"model":model,
                             "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                             "usage":{"prompt_tokens":8,"completion_tokens":5}}).to_string();
            let _ = tx.send(Ok(bytes::Bytes::from(format!("data: {fin}\n\ndata: [DONE]\n\n")))).await;
        });
        axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
            .unwrap()
            .into_response()
    } else {
        (
            Json(json!({
                "id": "cmpl-bench",
                "object": "chat.completion",
                "created": 1,
                "model": model,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "Hello from the mock upstream."}}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 5, "total_tokens": 13}
            })),
        )
            .into_response()
    }
}
