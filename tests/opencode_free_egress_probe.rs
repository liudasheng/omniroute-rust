//! Integration probes for the free-lane egress proxy support (phase 2).
//!
// The free-lane roster/budget mutex intentionally spans these async tests: the
// state it guards is process-wide, so it must be held across the awaits.
#![allow(clippy::await_holding_lock)]

//! These tests run the real gateway against a local permissive Zen mock and
//! local proxy implementations (a CONNECT proxy and a SOCKS5 proxy), so the
//! whole path is exercised: budget reservation → anonymous proxy-pool choice →
//! tunnel → Bun transport → upstream, plus rotation when an exit fails and the
//! per-account `proxy_url` isolation.
//!
//! They live in their own test binary because the free-lane roster and budget
//! are process-wide; a separate process keeps them from interlacing with the
//! main integration suite's free-lane tests.

use axum::routing::post;
use axum::Router;
use omniroute_rust::config::Config;
use omniroute_rust::state::{AccountCredential, AppState};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The free-lane roster and budget are process-wide, so the tests in this
/// binary must not run concurrently.
static FREE_LANE_LOCK: Mutex<()> = Mutex::new(());

fn free_lock() -> std::sync::MutexGuard<'static, ()> {
    let guard = FREE_LANE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    omniroute_rust::opencode_free_budget::reset();
    omniroute_rust::opencode_free::clear_consent_rejection();
    omniroute_rust::opencode_free::reset_sessions();
    guard
}

// ---------------------------------------------------------------------------
// Local upstream (permissive Zen mock: one SSE completion per call)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct UpstreamState {
    hits: AtomicU64,
    authorizations: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Upstream {
    state: Arc<UpstreamState>,
    base: String,
}

async fn free_chat(
    axum::extract::State(upstream): axum::extract::State<Upstream>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    upstream.state.hits.fetch_add(1, Ordering::SeqCst);
    upstream.state.authorizations.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    let model = body
        .get("model")
        .and_then(|model| model.as_str())
        .unwrap_or("zen-test-free")
        .to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let chunks = vec![
        json!({"id":"cmpl-zen","object":"chat.completion.chunk","created":1,"model":model,
               "choices":[{"index":0,"delta":{"role":"assistant","content":"ZEN-"},"finish_reason":null}]}),
        json!({"id":"cmpl-zen","object":"chat.completion.chunk","created":1,"model":model,
               "choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":5,"completion_tokens":2}}),
    ];
    tokio::spawn(async move {
        for chunk in chunks {
            let _ = tx
                .send(Ok(bytes::Bytes::from(format!("data: {chunk}\n\n"))))
                .await;
        }
        let _ = tx
            .send(Ok(bytes::Bytes::from("data: [DONE]\n\n")))
            .await;
    });
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

async fn spawn_upstream() -> Upstream {
    let state = Arc::new(UpstreamState::default());
    let app = Router::new()
        .route("/zen-accounts/v1/chat/completions", post(free_chat))
        .route("/zen/v1/chat/completions", post(free_chat))
        .with_state(Upstream { state: Arc::clone(&state), base: String::new() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { state, base: format!("http://{address}") }
}

impl Upstream {
    fn hits(&self) -> u64 {
        self.state.hits.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Local HTTP CONNECT proxy
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct ConnectProxyConfig {
    /// Answer CONNECT with this status instead of tunneling.
    reject: Option<&'static str>,
    /// Require this exact `Proxy-Authorization` value.
    expected_auth: Option<String>,
}

#[derive(Debug, Default)]
struct ConnectProxyLog {
    connects: Vec<String>,
    authorizations: Vec<Option<String>>,
}

#[derive(Clone)]
struct ConnectProxy {
    base: String,
    log: Arc<Mutex<ConnectProxyLog>>,
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut out: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while out.len() < 64 * 1024 {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        out.push(byte[0]);
        if out.ends_with(b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&out).to_string());
        }
    }
    None
}

async fn spawn_connect_proxy(config: ConnectProxyConfig) -> ConnectProxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log: Arc<Mutex<ConnectProxyLog>> = Arc::new(Mutex::new(ConnectProxyLog::default()));
    let recorded = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else { break };
            let log = Arc::clone(&recorded);
            let config = config.clone();
            tokio::spawn(async move {
                let Some(head) = read_head(&mut client).await else { return };
                let mut lines = head.split("\r\n");
                let connect_line = lines.next().unwrap_or_default().to_string();
                let mut authorization = None;
                for line in lines {
                    if let Some(value) = line.strip_prefix("Proxy-Authorization: ") {
                        authorization = Some(value.trim().to_string());
                    }
                }
                {
                    let mut log = log.lock().unwrap();
                    log.connects.push(connect_line.clone());
                    log.authorizations.push(authorization.clone());
                }
                if let Some(status) = config.reject {
                    let _ = client
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .await;
                    return;
                }
                if let Some(expected) = config.expected_auth.as_deref() {
                    if authorization.as_deref() != Some(expected) {
                        let _ = client
                            .write_all(
                                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                }
                let Some(authority) = connect_line.split_whitespace().nth(1) else {
                    return;
                };
                let Ok(mut upstream) = TcpStream::connect(authority).await else {
                    let _ = client
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    ConnectProxy { base: format!("http://{address}"), log }
}

impl ConnectProxy {
    fn connects(&self) -> Vec<String> {
        self.log.lock().unwrap().connects.clone()
    }

    fn authorizations(&self) -> Vec<Option<String>> {
        self.log.lock().unwrap().authorizations.clone()
    }
}

fn basic(user: &str, password: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

// ---------------------------------------------------------------------------
// Local SOCKS5 proxy (RFC 1928 no-auth + RFC 1929 user/password)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Socks5Mode {
    NoAuth,
    Password { username: &'static str, password: &'static str },
}

#[derive(Debug, Clone)]
struct Socks5Hit {
    atyp: u8,
    host: String,
    port: u16,
}

#[derive(Clone)]
struct Socks5Proxy {
    base: String,
    hits: Arc<Mutex<Vec<Socks5Hit>>>,
}

async fn spawn_socks5_proxy(mode: Socks5Mode) -> Socks5Proxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits: Arc<Mutex<Vec<Socks5Hit>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else { break };
            let hits = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut greeting = [0u8; 2];
                if client.read_exact(&mut greeting).await.is_err() || greeting[0] != 0x05 {
                    return;
                }
                let mut methods = vec![0u8; greeting[1] as usize];
                if client.read_exact(&mut methods).await.is_err() {
                    return;
                }
                let needs_password = matches!(mode, Socks5Mode::Password { .. });
                if needs_password {
                    if !methods.contains(&0x02) {
                        let _ = client.write_all(&[0x05, 0xff]).await;
                        return;
                    }
                    let _ = client.write_all(&[0x05, 0x02]).await;
                    let mut version = [0u8; 1];
                    if client.read_exact(&mut version).await.is_err() || version[0] != 0x01 {
                        return;
                    }
                    let mut length = [0u8; 1];
                    if client.read_exact(&mut length).await.is_err() {
                        return;
                    }
                    let mut user = vec![0u8; length[0] as usize];
                    if client.read_exact(&mut user).await.is_err() {
                        return;
                    }
                    if client.read_exact(&mut length).await.is_err() {
                        return;
                    }
                    let mut pass = vec![0u8; length[0] as usize];
                    if client.read_exact(&mut pass).await.is_err() {
                        return;
                    }
                    let accepted = match mode {
                        Socks5Mode::Password { username, password } => {
                            user == username.as_bytes() && pass == password.as_bytes()
                        }
                        Socks5Mode::NoAuth => false,
                    };
                    let _ = client
                        .write_all(&[0x01, if accepted { 0x00 } else { 0x01 }])
                        .await;
                    if !accepted {
                        return;
                    }
                } else {
                    if !methods.contains(&0x00) {
                        let _ = client.write_all(&[0x05, 0xff]).await;
                        return;
                    }
                    let _ = client.write_all(&[0x05, 0x00]).await;
                }
                let mut request = [0u8; 4];
                if client.read_exact(&mut request).await.is_err() || request[0] != 0x05 {
                    return;
                }
                let host = match request[3] {
                    0x01 => {
                        let mut address = [0u8; 4];
                        if client.read_exact(&mut address).await.is_err() {
                            return;
                        }
                        std::net::Ipv4Addr::from(address).to_string()
                    }
                    0x04 => {
                        let mut address = [0u8; 16];
                        if client.read_exact(&mut address).await.is_err() {
                            return;
                        }
                        std::net::Ipv6Addr::from(address).to_string()
                    }
                    0x03 => {
                        let mut length = [0u8; 1];
                        if client.read_exact(&mut length).await.is_err() {
                            return;
                        }
                        let mut name = vec![0u8; length[0] as usize];
                        if client.read_exact(&mut name).await.is_err() {
                            return;
                        }
                        String::from_utf8_lossy(&name).to_string()
                    }
                    _ => return,
                };
                let mut port = [0u8; 2];
                if client.read_exact(&mut port).await.is_err() {
                    return;
                }
                let port = u16::from_be_bytes(port);
                hits.lock().unwrap().push(Socks5Hit { atyp: request[3], host: host.clone(), port });
                let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
                    let _ = client
                        .write_all(&[0x05, 0x05, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                        .await;
                    return;
                };
                let _ = client
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    Socks5Proxy { base: format!("127.0.0.1:{}", address.port()), hits }
}

impl Socks5Proxy {
    fn hits(&self) -> Vec<Socks5Hit> {
        self.hits.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Gateway helpers
// ---------------------------------------------------------------------------

async fn spawn_gateway(state: AppState) -> String {
    let app = omniroute_rust::server::build_router(Arc::new(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    format!("http://127.0.0.1:{port}")
}

fn base_config(dir: &std::path::Path, upstream: &Upstream) -> Config {
    use omniroute_rust::config::*;
    let mut cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
        max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        data_dir: dir.to_path_buf(),
        api_key: Some("test-key".into()),
        request_timeout_ms: 30_000,
        connect_timeout_ms: 5_000,
        stream_idle_timeout_ms: 30_000,
        heartbeat_ms: 15_000,
        readiness_timeout_ms: 5_000,
        readiness_max_timeout_ms: 10_000,
        disconnect_grace_ms: 5_000,
        rate_rpm: 1_000_000,
        rate_min_interval_ms: 0,
        rate_concurrent_requests: 1024,
        rate_max_wait_ms: 5_000,
        rate_auto_enable_api_key_providers: true,
        compression: omniroute_rust::compression::CompressionConfig::default(),
        thinking_mode: "passthrough".into(),
        thinking_budget: None,
        strip_historical_reasoning: true,
        model_catalog_auto_update: false,
        model_catalog_ttl_hours: 24,
        model_catalog_url: omniroute_rust::model_catalog::CATALOG_URL.to_string(),
        opencode_free_auto_update: false,
        opencode_free_ttl_seconds: omniroute_rust::opencode_free::DEFAULT_REFRESH_SECONDS,
        opencode_free_url: omniroute_rust::opencode_free::ZEN_BASE_URL.to_string(),
        opencode_free_credential_mode: omniroute_rust::opencode_free_budget::CredentialMode::Public,
        opencode_free_daily_limit_per_account: 200,
        opencode_free_daily_limit_public: 200,
        opencode_free_learned_limit: true,
        opencode_free_learned_limit_ttl_days: 7,
        opencode_free_max_in_flight_per_account: 2,
        opencode_free_session_fallback: false,
        opencode_free_session_memo_file: omniroute_rust::opencode_free::SESSION_FILE.to_string(),
        opencode_free_learned_min_samples: 0,
        opencode_free_short_quota_cooldown_secs:
            omniroute_rust::opencode_free_budget::DEFAULT_SHORT_QUOTA_COOLDOWN_SECS,
        opencode_free_proxy_pool: Vec::new(),
        opencode_free_max_proxy_exits: omniroute_rust::opencode_free_egress::DEFAULT_MAX_PROXY_EXITS,
        opencode_free_proxy_cooldown_secs:
            omniroute_rust::opencode_free_egress::DEFAULT_PROXY_COOLDOWN_SECS,
        opencode_quota_auto_update: false,
        opencode_quota_ttl_seconds: 300,
        provider_sync_auto_update: false,
        provider_sync_ttl_seconds: DEFAULT_PROVIDER_SYNC_TTL_SECONDS,
        credentials: std::collections::HashMap::new(),
        tuning: std::collections::HashMap::new(),
        combos: Vec::new(),
        log_level: "info".into(),
    };
    cfg.credentials.insert(
        "opencode-zen".into(),
        ProviderCredentials {
            base_url: Some(format!("{}/zen-accounts/v1", upstream.base)),
            ..Default::default()
        },
    );
    cfg
}

/// Anonymous-lane state on the mock upstream with an optional proxy pool.
fn anonymous_state(config: &Config) -> AppState {
    AppState::new(config.clone())
}

fn account(id: &str, key: &str, upstream: &Upstream, proxy_url: Option<String>) -> AccountCredential {
    AccountCredential {
        connection_id: id.to_string(),
        name: format!("account {id}"),
        api_key: Some(key.to_string()),
        base_url: Some(format!("{}/zen-accounts/v1", upstream.base)),
        api_type: None,
        proxy_url,
    }
}

async fn management_token(gateway: &str) -> String {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/auth/login"))
        .json(&json!({"password": "CHANGEME"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn status(gateway: &str, token: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{gateway}/v1/opencode-free"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn chat_with(gateway: &str, text: &str) -> (reqwest::StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("authorization", "Bearer test-key")
        .json(&json!({
            "model": "opencode-zen/zen-test-free",
            "messages": [{"role": "user", "content": text}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// A first-user-message text whose conversation seed hashes to `wanted_start`
/// in a pool of `pool_len` exits: the sticky selection is hash-based, so a
/// rotation test must anchor its start deterministically.
fn sticky_text(pool_len: usize, wanted_start: usize) -> String {
    for index in 0..10_000 {
        let text = format!("egress-probe-{index}");
        let seed = omniroute_rust::opencode_free::conversation_seed(&json!({
            "messages": [{"role": "user", "content": text.clone()}]
        }));
        if omniroute_rust::opencode_free_egress::session_seed_hash(&seed) as usize % pool_len
            == wanted_start
        {
            return text;
        }
    }
    panic!("no conversation text hashes to {wanted_start} of {pool_len}");
}

fn install_roster() {
    omniroute_rust::opencode_free::install_live(vec!["zen-test-free".into()]);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_anonymous_lane_tunnels_through_the_connect_proxy_pool() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let proxy = spawn_connect_proxy(ConnectProxyConfig {
        reject: None,
        expected_auth: Some(basic("user", "s3cret")),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool = vec![proxy
        .base
        .replacen("http://", "http://user:s3cret@", 1)];
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    let (status_code, body) = chat_with(&gateway, "hello through the tunnel").await;
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");

    // The upstream request went through the proxy, not around it.
    let connects = proxy.connects();
    assert_eq!(connects.len(), 1, "{connects:?}");
    let upstream_authority = upstream.base.trim_start_matches("http://");
    assert!(
        connects[0].starts_with(&format!("CONNECT {upstream_authority}")),
        "{connects:?}"
    );
    assert_eq!(
        proxy.authorizations()[0].as_deref(),
        Some(basic("user", "s3cret").as_str()),
        "the proxy URL credentials become Proxy-Authorization"
    );
    assert_eq!(upstream.hits(), 1);
    assert_eq!(
        upstream.state.authorizations.lock().unwrap().as_slice(),
        ["Bearer public"],
        "the tunneled request keeps the anonymous credential"
    );

    // The status endpoint exposes the exit without its credentials.
    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    assert_eq!(payload["egress"]["poolSize"], 1, "{payload}");
    assert_eq!(payload["egress"]["maxExitsPerRequest"], 4);
    assert_eq!(payload["egress"]["sticky"], "session");
    let exits = payload["egress"]["exits"].as_array().unwrap();
    assert_eq!(exits.len(), 1, "{exits:?}");
    let exit = &exits[0];
    assert_eq!(exit["mode"], "http_connect");
    assert_eq!(exit["source"], "pool");
    assert_eq!(exit["used"], 1);
    assert_eq!(exit["healthy"], true);
    let label = exit["label"].as_str().unwrap();
    assert!(!label.contains("s3cret"), "the proxy password must never leak: {label}");
    assert!(!label.contains("user"), "the proxy username must never leak: {label}");
    assert!(!payload.to_string().contains("s3cret"), "{payload}");

    // D1 regression: the anonymous bucket carries an egress suffix
    // (`public @ http://host:port`) and must still render as anonymous.
    let buckets = payload["budget"]["credentials"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "{buckets:?}");
    assert_eq!(buckets[0]["label"], "anonymous", "{buckets:?}");
    assert_eq!(buckets[0]["anonymous"], true, "{buckets:?}");
    assert!(
        buckets[0]["id"].as_str().unwrap().starts_with("public @ http://"),
        "the anonymous bucket id carries the masked egress: {buckets:?}"
    );
}

#[tokio::test]
async fn a_refusing_exit_rotates_to_the_next_one() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let rejecting = spawn_connect_proxy(ConnectProxyConfig {
        reject: Some("502 Bad Gateway"),
        expected_auth: None,
    })
    .await;
    let working = spawn_connect_proxy(ConnectProxyConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool = vec![rejecting.base.clone(), working.base.clone()];
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    // Anchor the sticky start on the failing exit so the request must rotate.
    let text = sticky_text(2, 0);
    let (status_code, body) = chat_with(&gateway, &text).await;
    assert_eq!(status_code, 200, "the second exit must serve: {body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");
    assert_eq!(rejecting.connects().len(), 1, "the first exit was tried");
    assert_eq!(working.connects().len(), 1, "the request rotated to the next exit");
    assert_eq!(upstream.hits(), 1);
}

#[tokio::test]
async fn a_pool_where_every_exit_fails_returns_a_retryable_error_without_an_upstream_call() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let first = spawn_connect_proxy(ConnectProxyConfig {
        reject: Some("407 Proxy Authentication Required"),
        expected_auth: None,
    })
    .await;
    let second = spawn_connect_proxy(ConnectProxyConfig {
        reject: Some("502 Bad Gateway"),
        expected_auth: None,
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool = vec![first.base.clone(), second.base.clone()];
    cfg.opencode_free_proxy_cooldown_secs = 30;
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    let (status_code, body) = chat_with(&gateway, &sticky_text(2, 0)).await;
    assert!(
        status_code.is_server_error(),
        "a dead pool is a server-side failure, got {status_code}: {body}"
    );
    assert_ne!(status_code, 429, "a tunnel failure is not a quota refusal: {body}");
    assert_eq!(first.connects().len(), 1);
    assert_eq!(second.connects().len(), 1);
    assert_eq!(upstream.hits(), 0, "the upstream must never see a request through a dead pool");

    // Both exits carry a short failure cooldown, and the anonymous bucket was
    // refunded (the requests never reached the upstream).
    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    let exits = payload["egress"]["exits"].as_array().unwrap();
    assert_eq!(exits.len(), 2, "{exits:?}");
    for exit in exits {
        assert_eq!(exit["healthy"], false, "{exit}");
        assert!(exit["cooldownUntilMs"].as_u64().unwrap() > 0, "{exit}");
    }
    let buckets = payload["budget"]["credentials"].as_array().unwrap();
    assert!(
        buckets.iter().all(|bucket| bucket["used"] == 0),
        "an egress failure must refund the reservation: {buckets:?}"
    );
}

#[tokio::test]
async fn a_socks5_proxy_with_username_password_serves_the_lane() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let proxy = spawn_socks5_proxy(Socks5Mode::Password {
        username: "alice",
        password: "s3cret",
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool = vec![format!("socks5://alice:s3cret@{}", proxy.base)];
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    let (status_code, body) = chat_with(&gateway, "hello through socks5").await;
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ZEN-OK");
    let hits = proxy.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let upstream_authority = upstream.base.trim_start_matches("http://").to_string();
    assert_eq!(hits[0].host, upstream_authority.split(':').next().unwrap());
    assert_eq!(
        hits[0].port,
        upstream_authority.rsplit_once(':').unwrap().1.parse::<u16>().unwrap()
    );

    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    assert_eq!(payload["egress"]["exits"][0]["mode"], "socks5", "{payload}");
}

#[tokio::test]
async fn a_socks5_proxy_without_auth_serves_the_lane() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let proxy = spawn_socks5_proxy(Socks5Mode::NoAuth).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool = vec![format!("socks5://{}", proxy.base)];
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    let (status_code, body) = chat_with(&gateway, "hello through socks5 no-auth").await;
    assert_eq!(status_code, 200, "{body}");
    let hits = proxy.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].atyp, 0x01, "an IP target is sent as an IPv4 literal");
    assert_eq!(upstream.hits(), 1);
}

#[tokio::test]
async fn an_account_proxy_url_only_affects_that_account() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let proxy = spawn_connect_proxy(ConnectProxyConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_credential_mode = omniroute_rust::opencode_free_budget::CredentialMode::Auto;
    cfg.opencode_free_daily_limit_per_account = 1;
    cfg.opencode_free_daily_limit_public = 1;
    let state = AppState::new(cfg);
    state.refresh_account_pool(
        "opencode-zen",
        vec![
            // The proxied account is drawn first (cursor 0).
            account("conn-proxy-0001", "acct-proxy", &upstream, Some(proxy.base.clone())),
            account("conn-direct-0002", "acct-direct", &upstream, None),
        ],
    );
    let gateway = spawn_gateway(state).await;

    let (status_code, body) = chat_with(&gateway, "first account through its proxy").await;
    assert_eq!(status_code, 200, "{body}");
    let (status_code, body) = chat_with(&gateway, "second account dials directly").await;
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(upstream.hits(), 2);
    assert_eq!(
        proxy.connects().len(),
        1,
        "only the account that carries the proxy_url tunnels: {:?}",
        proxy.connects()
    );

    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    let exits = payload["egress"]["exits"].as_array().unwrap();
    assert!(
        exits.iter().any(|exit| exit["source"] == "account" && exit["used"] == 1),
        "the account proxy is in the egress view: {exits:?}"
    );
    assert!(
        exits.iter().any(|exit| exit["label"] == "direct" && exit["used"] == 1),
        "the direct dial is a separate egress row: {exits:?}"
    );
}

#[tokio::test]
async fn no_proxy_configuration_keeps_the_historical_direct_dial() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = base_config(dir.path(), &upstream);
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;

    let (status_code, body) = chat_with(&gateway, "plain direct").await;
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(upstream.hits(), 1);
    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    assert_eq!(payload["egress"]["poolSize"], 0);
    let exits = payload["egress"]["exits"].as_array().unwrap();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0]["label"], "direct");
    assert_eq!(exits[0]["mode"], "direct");
    assert_eq!(exits[0]["used"], 1);
}

/// The egress view must not leak a proxy credential through either the status
/// endpoint or the masked bucket id (`mask_bucket` is unit-tested; this asserts
/// the real endpoint payload).
#[tokio::test]
async fn proxy_credentials_are_redacted_in_the_status_payload() {
    let _guard = free_lock();
    install_roster();
    let upstream = spawn_upstream().await;
    let proxy = spawn_connect_proxy(ConnectProxyConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_config(dir.path(), &upstream);
    cfg.opencode_free_proxy_pool =
        vec![proxy.base.replacen("http://", "http://alice:top-secret@", 1)];
    let gateway = spawn_gateway(anonymous_state(&cfg)).await;
    let _ = chat_with(&gateway, "redaction probe").await;

    let token = management_token(&gateway).await;
    let payload = status(&gateway, &token).await;
    let text = payload.to_string();
    for secret in ["top-secret", "alice", "Basic "] {
        assert!(
            !text.contains(secret),
            "status payload leaked {secret:?}: {text}"
        );
    }
    assert_eq!(payload["egress"]["exits"][0]["mode"], "http_connect", "{payload}");
}