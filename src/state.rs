//! Shared gateway state (single instance behind `Arc` in the axum server).

use crate::config::Config;
use std::sync::RwLock;
use crate::registry::Registry;
use crate::router::circuit::CircuitStore;
use crate::router::rate::RateLimiter;
use crate::upstream::executor::UpstreamClient;

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
    /// real upstream usage (0 when the provider reported none)
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// true when the client asked for a stream
    #[serde(default)]
    pub stream: bool,
}

/// Token usage reported by an upstream provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
}

impl TokenUsage {
    pub fn is_zero(&self) -> bool {
        self.prompt == 0 && self.completion == 0
    }
}

/// Pull usage out of any provider's response shape.
pub fn usage_from_value(v: &serde_json::Value) -> TokenUsage {
    let num = |x: &serde_json::Value| x.as_u64().or_else(|| x.as_i64().map(|i| i.max(0) as u64)).unwrap_or(0);
    // openai / openai-compatible
    if let Some(u) = v.get("usage") {
        let prompt = num(u.get("prompt_tokens").unwrap_or(&serde_json::Value::Null));
        let completion = num(u.get("completion_tokens").unwrap_or(&serde_json::Value::Null));
        if prompt > 0 || completion > 0 {
            return TokenUsage { prompt, completion };
        }
        // anthropic naming
        let input = num(u.get("input_tokens").unwrap_or(&serde_json::Value::Null));
        let output = num(u.get("output_tokens").unwrap_or(&serde_json::Value::Null));
        if input > 0 || output > 0 {
            return TokenUsage { prompt: input, completion: output };
        }
    }
    // gemini
    if let Some(u) = v.get("usageMetadata") {
        let prompt = num(u.get("promptTokenCount").unwrap_or(&serde_json::Value::Null));
        let completion = num(u.get("candidatesTokenCount").unwrap_or(&serde_json::Value::Null));
        if prompt > 0 || completion > 0 {
            return TokenUsage { prompt, completion };
        }
    }
    TokenUsage::default()
}

/// One management action, for the audit page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub ts_ms: u128,
    pub action: String,
    pub detail: String,
    pub ok: bool,
}

/// Live runtime view of one provider connection.
#[derive(Debug, Clone)]
pub struct ProviderRuntime {
    pub id: String,
    pub format: String,
    pub has_key: bool,
    pub in_flight: u64,
    pub cooldown_ms: u64,
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
    request_log: std::sync::Mutex<std::collections::VecDeque<RequestLogEntry>>,
    /// management-action audit ring (parity: audit-log page), cap 500
    audit_log: std::sync::Mutex<std::collections::VecDeque<AuditEntry>>,
    pub request_log_total: std::sync::atomic::AtomicU64,
    pub request_log_failures: std::sync::atomic::AtomicU64,
    /// dashboard auth (session tokens + password hash + change handler)
    pub auth: crate::server::security::AuthStore,
    /// dashboard-issued API keys
    pub api_keys: crate::server::security::ApiKeyStore,
    /// managed provider connections (dashboard CRUD, persisted)
    pub provider_connections: crate::server::providers_admin::ProviderConnectionStore,
    /// dashboard-managed combos (merged over config combos)
    pub combos: crate::server::combos_admin::ComboStore,
    /// per-provider quota overrides for the quota page
    pub quota_overrides: crate::server::combos_admin::QuotaStore,
    /// gateway-wide system prompt injected into every chat request
    system_prompt: RwLock<Option<String>>,
    /// runtime credential overlays (managed connections) consulted first
    pub credentials_overlay: std::sync::RwLock<std::collections::HashMap<String, crate::config::ProviderCredentials>>,
}

impl AppState {
    pub fn build(config: Config) -> Self {
        let connection_store =
            crate::server::providers_admin::ProviderConnectionStore::new(&config.data_dir);
        let managed_connections = connection_store.all_unmasked();
        let registry = (*config.effective_registry()).clone();

        // Re-register dashboard-created compatible providers on boot. Without
        // this, a connection works until the first live PATCH/POST registers
        // it, then becomes an unknown provider after a service restart.
        for c in &managed_connections {
            if c.provider.starts_with("openai-compatible")
                || c.provider.starts_with("anthropic-compatible")
            {
                let mut models = c.model_list.clone();
                models.extend(c.synced_models.clone());
                let base = c
                    .base_url
                    .clone()
                    .filter(|b| !b.trim().is_empty());
                registry.register_dynamic(&c.provider, base, c.api_type.clone(), models);
            }
        }

        // dashboard-managed connection overlays (consulted before config creds)
        let mut credential_overlays = std::collections::HashMap::new();
        for c in &managed_connections {
            if !c.enabled {
                continue;
            }
            let mut model_list = c.model_list.clone();
            model_list.extend(c.synced_models.clone());
            let cred = crate::config::ProviderCredentials {
                api_key: c.api_key.clone(),
                base_url: c.base_url.clone(),
                api_type: c.api_type.clone(),
                model_list,
                enabled: Some(true),
            };
            credential_overlays.insert(c.provider.clone(), cred);
        }
        let upstream = UpstreamClient::new(&config);
        let circuits = CircuitStore::with_limits(config.rate_concurrent_requests);
        let rate = RateLimiter::with_limits(config.rate_rpm, config.rate_min_interval_ms);
        let compression = config.compression.clone();
        let auth_store = crate::server::security::AuthStore::new(
            &config.data_dir,
            std::env::var("OMNIROUTE_ADMIN_PASSWORD").ok(),
        );
        let api_key_store = crate::server::security::ApiKeyStore::new(&config.data_dir);
        let data_dir_for_stores = config.data_dir.clone();
        Self {
            config,
            registry,
            circuits,
            rate,
            upstream,
            started_at: std::time::Instant::now(),
            compression_config: std::sync::RwLock::new(compression),
            request_log: std::sync::Mutex::new(std::collections::VecDeque::new()),
            audit_log: std::sync::Mutex::new(std::collections::VecDeque::new()),
            request_log_total: std::sync::atomic::AtomicU64::new(0),
            request_log_failures: std::sync::atomic::AtomicU64::new(0),
            auth: auth_store,
            api_keys: api_key_store,
            provider_connections: connection_store,
            combos: crate::server::combos_admin::ComboStore::new(&data_dir_for_stores),
            quota_overrides: crate::server::combos_admin::QuotaStore::new(&data_dir_for_stores.clone()),
            system_prompt: RwLock::new(load_custom_system_prompt(&data_dir_for_stores)),
            credentials_overlay: std::sync::RwLock::new(credential_overlays),
        }
    }

    /// Test constructor: no env dependency, optional explicit data dir.
    pub fn new(config: Config) -> Self {
        Self::build(config)
    }

    pub fn for_tests(combos: Vec<crate::config::ComboConfig>, data_dir: Option<std::path::PathBuf>) -> Self {
        use crate::config::*;
        let config = Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: data_dir.unwrap_or_else(|| std::path::PathBuf::from("/tmp/omniroute-rust-tests")),
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
            compression: crate::compression::CompressionConfig::default(),
            credentials: std::collections::HashMap::new(),
            tuning: std::collections::HashMap::new(),
            combos,
            log_level: "info".into(),
        };
        Self::build(config)
    }

    /// Provider ids usable by the running gateway, including dashboard-managed
    /// connections. This is the state-aware counterpart to
    /// `Config::providers_with_keys`, which only knows env/file credentials.
    pub fn providers_with_keys(&self) -> Vec<String> {
        let mut ids = self.config.providers_with_keys();
        for c in self.provider_connections.all_unmasked() {
            if !c.enabled {
                continue;
            }
            let usable = c.api_key.as_ref().is_some_and(|k| !k.is_empty())
                || c.base_url.as_ref().is_some_and(|b| !b.trim().is_empty())
                || self.registry.get(&c.provider).is_some_and(|e| e.is_local);
            if usable && self.registry.get(&c.provider).is_some() {
                ids.push(c.provider);
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// Effective model ids for one running provider, including static,
    /// configured and dashboard-synced models, minus connection-local hides.
    pub fn models_for_provider(&self, provider: &str) -> Vec<String> {
        let mut models = self.registry.models_for(provider);
        models.extend(
            self.config
                .credentials
                .get(provider)
                .map(|c| c.model_list.clone())
                .unwrap_or_default(),
        );
        models.extend(
            self.config
                .tuning
                .get(provider)
                .map(|t| t.models.clone())
                .unwrap_or_default(),
        );
        let mut hidden = std::collections::HashSet::new();
        for c in self.provider_connections.all_unmasked() {
            if c.enabled && c.provider == provider {
                models.extend(c.model_list);
                models.extend(c.synced_models);
                hidden.extend(c.hidden_models);
            }
        }
        models.retain(|m| !hidden.contains(m));
        models.sort();
        models.dedup();
        models
    }

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

    /// Record a management action (login, key/provider change, service action…).
    pub fn audit(&self, action: &str, detail: impl Into<String>, ok: bool) {
        if let Ok(mut q) = self.audit_log.lock() {
            q.push_back(AuditEntry {
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                action: action.to_string(),
                detail: detail.into(),
                ok,
            });
            while q.len() > 500 {
                q.pop_front();
            }
        }
    }

    pub fn audit_snapshot(&self, limit: usize) -> Vec<AuditEntry> {
        self.audit_log
            .lock()
            .map(|q| q.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Live per-provider runtime view (registry × circuit state × key presence),
    /// shared by `/v1/providers` and `/v1/stats/providers`.
    pub fn provider_runtime_snapshot(&self) -> Vec<ProviderRuntime> {
        self.registry
            .ids()
            .into_iter()
            .filter_map(|id| {
                let entry = self.registry.get(&id)?;
                let format = entry.format.as_str().to_string();
                let s = self
                    .circuits
                    .snapshot()
                    .into_iter()
                    .find(|(k, _, _)| k == &id);
                Some(ProviderRuntime {
                    has_key: self.api_key_for(&id).is_some(),
                    format,
                    id,
                    in_flight: s.as_ref().map(|(_, i, _)| *i).unwrap_or(0).max(0) as u64,
                    cooldown_ms: s.as_ref().map(|(_, _, c)| *c).unwrap_or(0).max(0) as u64,
                })
            })
            .collect()
    }

    /// api key resolution: overlay (managed connection) → config.
    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        if let Some(c) = self
            .credentials_overlay
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
        {
            if c.enabled != Some(false) {
                if let Some(k) = c.api_key.clone().filter(|k| !k.is_empty()) {
                    return Some(k);
                }
            }
        }
        self.config.api_key_for(provider)
    }

    /// base url resolution: overlay → config (toml/credentials/registry).
    pub fn base_url_for(&self, registry: &Registry, provider: &str) -> Option<String> {
        if let Some(c) = self
            .credentials_overlay
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
        {
            if c.enabled != Some(false) {
                if let Some(b) = c.base_url.clone().filter(|b| !b.trim().is_empty()) {
                    return Some(b);
                }
            }
        }
        self.config.base_url_for(registry, provider)
    }
}

/// Path of the gateway-wide system prompt setting.
fn custom_prompt_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("custom-system-prompt.txt")
}

fn load_custom_system_prompt(data_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(custom_prompt_path(data_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl AppState {
    pub fn custom_system_prompt(&self) -> Option<String> {
        self.system_prompt.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Persist the gateway-wide system prompt (empty/None clears it).
    pub fn set_custom_system_prompt(&self, value: Option<String>) {
        *self.system_prompt.write().unwrap_or_else(|e| e.into_inner()) = value.clone();
        let path = custom_prompt_path(&self.config.data_dir);
        match value {
            Some(v) => {
                let _ = std::fs::write(&path, v);
                let _ = crate::set_file_mode_600(&path);
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_compatible_connections_are_registered_on_boot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("provider-connections.json"),
            serde_json::json!([{
                "id": "kaopu",
                "provider": "openai-compatible-kaopu",
                "name": "考谱AI",
                "api_key": "k",
                "baseUrl": "https://example.test/v1",
                "models": ["kaopu-model"],
                "synced_models": ["kaopu-synced"],
                "enabled": true
            }])
            .to_string(),
        )
        .unwrap();
        let state = AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf()));
        assert!(state.registry.get("openai-compatible-kaopu").is_some());
        assert!(state.providers_with_keys().contains(&"openai-compatible-kaopu".to_string()));
        assert_eq!(
            state.models_for_provider("openai-compatible-kaopu"),
            vec!["kaopu-model", "kaopu-synced"]
        );
    }
}
