//! Shared gateway state (single instance behind `Arc` in the axum server).

use crate::config::Config;
use crate::registry::Registry;
use crate::router::circuit::CircuitStore;
use crate::router::rate::RateLimiter;
use crate::upstream::executor::UpstreamClient;
use std::path::PathBuf;

/// Everything the handlers need, one instance.
pub struct AppState {
    pub config: Config,
    pub registry: Registry,
    pub circuits: CircuitStore,
    pub rate: RateLimiter,
    pub upstream: UpstreamClient,
}

impl AppState {
    /// Build from a loaded config (registry includes dynamic families).
    pub fn new(config: Config) -> Self {
        let registry = (*config.effective_registry()).clone();
        let upstream = UpstreamClient::new(&config);
        let circuits = CircuitStore::with_limits(config.rate_concurrent_requests);
        let rate = RateLimiter::with_limits(config.rate_rpm, config.rate_min_interval_ms);
        Self {
            config,
            registry,
            circuits,
            rate,
            upstream,
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
            log_level: "info".into(),
        };
        let registry = (*config.effective_registry()).clone();
        let upstream = UpstreamClient::new(&config);
        let circuits = CircuitStore::with_limits(config.rate_concurrent_requests);
        let rate = RateLimiter::with_limits(config.rate_rpm, config.rate_min_interval_ms);
        Self {
            config,
            registry,
            circuits,
            rate,
            upstream,
        }
    }
}
