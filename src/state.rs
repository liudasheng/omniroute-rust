//! Shared gateway state (single instance behind `Arc` in the axum server).

use crate::config::Config;
use crate::registry::Registry;
use crate::router::circuit::CircuitStore;
use crate::router::rate::RateLimiter;
use crate::upstream::executor::UpstreamClient;
use std::path::PathBuf;

/// One request-log entry (dashboard/logs view).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLogEntry {
    pub ts_ms: u128,
    pub model: String,
    pub provider: Option<String>,
    pub status: u16,
    pub latency_ms: u64,
    pub tokens_saved: i64,
    pub compressed: bool,
}

/// Everything the handlers need, one instance.
pub struct AppState {
    pub config: Config,
    pub registry: Registry,
    pub circuits: CircuitStore,
    pub rate: RateLimiter,
    pub upstream: UpstreamClient,
    /// gateway boot time (uptime / dashboard)
    pub started_at: std::time::Instant,
    /// runtime-mutable compression settings (dashboard edits; boot source = toml/env)
    pub compression_config: std::sync::RwLock<crate::compression::CompressionConfig>,
    /// bounded recent-request ring buffer
    request_log: std::sync::Mutex<std::collections::VecDeque<RequestLogEntry>>,
    pub request_log_total: std::sync::atomic::AtomicU64,
    pub request_log_failures: std::sync::atomic::AtomicU64,
}

impl AppState {
    /// Append a request-log entry (ring buffer cap 500).
    pub fn log_request(&self, entry: RequestLogEntry) {
        use std::sync::atomic::Ordering;
        self.request_log_total.fetch_add(1, Ordering::Relaxed);
        if entry.status >= 400 {
            self.request_log_failures.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut q) = self.request_log.lock() {
            q.push_back(entry);
            while q.len() > 500 {
                q.pop_front();
            }
        }
    }

    pub fn request_log_snapshot(&self, limit: usize) -> Vec<RequestLogEntry> {
        self.request_log
            .lock()
            .map(|q| q.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }
}

impl AppState {
    /// Build from a loaded config (registry includes dynamic families).
    pub fn new(config: Config) -> Self {
        let registry = (*config.effective_registry()).clone();
        let upstream = UpstreamClient::new(&config);
        let circuits = CircuitStore::with_limits(config.rate_concurrent_requests);
        let rate = RateLimiter::with_limits(config.rate_rpm, config.rate_min_interval_ms);
        let compression = config.compression.clone();
        Self {
            config,
            registry,
            circuits,
            rate,
            upstream,
            started_at: std::time::Instant::now(),
            compression_config: std::sync::RwLock::new(compression),
            request_log: std::sync::Mutex::new(std::collections::VecDeque::new()),
            request_log_total: std::sync::atomic::AtomicU64::new(0),
            request_log_failures: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Test constructor: no env dependency, optional explicit data dir.
    pub fn for_tests(combos: Vec<crate::config::ComboConfig>, data_dir: Option<PathBuf>) -> Self {
        use crate::config::*;
        let config = Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: data_dir.unwrap_or_else(|| PathBuf::from("/tmp/omniroute-rust-tests")),
            api_key: None,
            request_timeout_ms: REQUEST_TIMEOUT_MS,
            connect_timeout_ms: CONNECT_TIMEOUT_MS,
            stream_idle_timeout_ms: STREAM_IDLE_TIMEOUT_MS,
            heartbeat_ms: SSE_HEARTBEAT_INTERVAL_MS,
            readiness_timeout_ms: STREAM_READINESS_TIMEOUT_MS,
            readiness_max_timeout_ms: STREAM_READINESS_MAX_TIMEOUT_MS,
            disconnect_grace_ms: DISCONNECT_GRACE_MS,
            rate_rpm: DEFAULT_RATE_RPM,
            rate_min_interval_ms: DEFAULT_RATE_MIN_INTERVAL_MS,
            rate_concurrent_requests: DEFAULT_RATE_CONCURRENCY,
            rate_max_wait_ms: DEFAULT_RATE_MAX_WAIT_MS,
            rate_auto_enable_api_key_providers: true,
            credentials: std::collections::HashMap::new(),
            tuning: std::collections::HashMap::new(),
            combos,
            compression: crate::compression::CompressionConfig::default(),
            log_level: "info".into(),
        };
        let registry = (*config.effective_registry()).clone();
        let upstream = UpstreamClient::new(&config);
        let circuits = CircuitStore::with_limits(config.rate_concurrent_requests);
        let rate = RateLimiter::with_limits(config.rate_rpm, config.rate_min_interval_ms);
        let compression = config.compression.clone();
        Self {
            config,
            registry,
            circuits,
            rate,
            upstream,
            started_at: std::time::Instant::now(),
            compression_config: std::sync::RwLock::new(compression),
            request_log: std::sync::Mutex::new(std::collections::VecDeque::new()),
            request_log_total: std::sync::atomic::AtomicU64::new(0),
            request_log_failures: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
