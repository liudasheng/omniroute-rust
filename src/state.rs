//! Shared gateway state (single instance behind `Arc` in the axum server).

use crate::config::Config;
use std::sync::RwLock;
use crate::registry::Registry;
use crate::router::circuit::CircuitStore;
use crate::router::rate::RateLimiter;
use crate::upstream::executor::UpstreamClient;

/// One account of a provider: a single credential pair plus the identity the
/// dashboard shows.
///
/// A provider may carry several accounts. Routing still keys on
/// `provider/model`, but *which* credential serves the request is now a
/// per-request decision (round-robin over accounts, skipping any account whose
/// own cooldown is running). Each account keeps its own failure state so one
/// exhausted key cannot park its siblings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCredential {
    /// Managed connection id (`conn-…`). Empty for a config/env credential.
    pub connection_id: String,
    /// Operator-visible name; falls back to the connection id.
    pub name: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub api_type: Option<String>,
}

impl AccountCredential {
    /// Circuit-store key for this account. `provider` alone would let one
    /// exhausted key park every sibling, which is exactly the bug this type
    /// exists to avoid.
    pub fn circuit_key(&self, provider: &str) -> String {
        if self.connection_id.is_empty() {
            provider.to_string()
        } else {
            format!("{provider}#{}", self.connection_id)
        }
    }

    /// How the dashboard/log should refer to this account.
    pub fn label(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if !self.connection_id.is_empty() {
            self.connection_id.clone()
        } else {
            "default".to_string()
        }
    }
}

/// One request-log entry (dashboard/logs view).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLogEntry {
    pub ts_ms: u128,
    /// The model string the *client* asked for. When it named a combo this
    /// is the combo's name, not a model — the one that actually served is
    /// `routed_model`.
    pub model: String,
    /// The combo this request resolved through, when it resolved through one.
    #[serde(default)]
    pub combo: Option<String>,
    /// True once the writer has asserted what `combo` means — including
    /// "none": a plain `provider/model` request has no combo. Without this,
    /// a fresh `combo: None` row is indistinguishable from one written before
    /// the field existed, and the legacy model-based match would attribute
    /// plain traffic to whichever combo happens to name that model.
    #[serde(default)]
    pub combo_known: bool,
    /// The concrete `provider/model` that served the request, distinct from
    /// `model` whenever a combo was involved.
    #[serde(default)]
    pub routed_model: Option<String>,
    pub provider: Option<String>,
    /// The account (connection name) that served the request, when the provider
    /// carries managed accounts. Without it an idle account is invisible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub client_ip: Option<String>,
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
    /// Managed accounts per provider, in stable (file) order. This is the pool a
    /// request draws its credential from; `credentials_overlay` stays as the
    /// "last writer wins" single-credential view for the paths that still want
    /// exactly one (probes, diagnostics).
    pub account_pools: std::sync::RwLock<std::collections::HashMap<String, Vec<AccountCredential>>>,
    /// Per-request cursor for account round-robin.
    account_cursor: std::sync::atomic::AtomicU64,
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
        let mut account_pools: std::collections::HashMap<String, Vec<AccountCredential>> =
            std::collections::HashMap::new();
        for c in &managed_connections {
            if !c.enabled {
                continue;
            }
            let mut model_list = c.model_list.clone();
            model_list.extend(c.synced_models.clone());
            account_pools
                .entry(c.provider.clone())
                .or_default()
                .push(AccountCredential {
                    connection_id: c.id.clone(),
                    name: c.name.clone(),
                    api_key: c.api_key.clone(),
                    base_url: c.base_url.clone(),
                    api_type: c.api_type.clone(),
                });
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
            account_pools: std::sync::RwLock::new(account_pools),
            account_cursor: std::sync::atomic::AtomicU64::new(0),
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
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
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
            thinking_mode: "passthrough".into(),
            thinking_budget: None,
            strip_historical_reasoning: true,
            model_catalog_auto_update: false,
            model_catalog_ttl_hours: 24,
            model_catalog_url: crate::model_catalog::CATALOG_URL.to_string(),
            opencode_free_auto_update: false,
            opencode_free_ttl_seconds: crate::opencode_free::DEFAULT_REFRESH_SECONDS,
            opencode_free_url: crate::opencode_free::ZEN_BASE_URL.to_string(),
            opencode_free_use_key: false,
            opencode_quota_auto_update: false,
            opencode_quota_ttl_seconds: 300,
            provider_sync_auto_update: false,
            provider_sync_ttl_seconds: crate::config::DEFAULT_PROVIDER_SYNC_TTL_SECONDS,
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
        // The OpenCode Zen free lane is usable with the literal `public`
        // credential and no registration at all (DSH reference:
        // `opencode2dsh` catalog.ts `Bearer public`), so the provider must be
        // routable even when no key is configured.
        for id in self.registry.ids() {
            if crate::opencode_free::is_free_lane_provider(&id)
                && self.registry.get(&id).is_some()
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        for c in self.provider_connections.all_unmasked() {
            if !c.enabled {
                continue;
            }
            let usable = c.api_key.as_ref().is_some_and(|k| crate::config::is_usable_api_key(k))
                || c.base_url.as_ref().is_some_and(|b| crate::config::is_usable_base_url(b))
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
        let connections: Vec<_> = self
            .provider_connections
            .all_unmasked()
            .into_iter()
            .filter(|c| c.enabled && c.provider == provider)
            .collect();
        // After a successful upstream sync, the provider's returned list is
        // authoritative. Before that, static registry seeds remain useful for
        // configured first-party providers and hand-declared connections.
        let synced_authoritative = connections.iter().any(|c| c.synced_at_ms > 0);
        let mut models = if synced_authoritative {
            Vec::new()
        } else {
            self.registry.models_for(provider)
        };
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
        for c in connections {
            models.extend(c.model_list);
            models.extend(c.synced_models);
            hidden.extend(c.hidden_models);
        }
        // OpenCode Zen: the live directory is the only part of the roster that
        // changes without a redeploy, so the discovered free ids ride along
        // with the static seeds and any hand-declared models — but only when
        // the provider has NOT yet synced its own authoritative model list.
        if crate::opencode_free::is_free_lane_provider(provider) && !synced_authoritative {
            models.extend(crate::opencode_free::models());
        }
        models.retain(|m| !hidden.contains(m));
        models.sort();
        models.dedup();
        models
    }

    /// Resolve inferred model capabilities and overlay the latest metadata
    /// discovered from a provider's `/models` endpoint when available.
    pub fn capabilities_for_model(&self, provider: &str, model: &str) -> crate::registry::ModelCapabilities {
        let mut capabilities = crate::registry::capabilities(provider, model);
        for connection in self.provider_connections.all_unmasked() {
            if connection.enabled && connection.provider == provider {
                if let Some(metadata) = connection.model_metadata.get(model) {
                    capabilities.apply_metadata(metadata);
                }
            }
        }
        capabilities
    }

    pub fn format_for_model(&self, provider: &str, model: &str) -> crate::registry::Format {
        let mut format = self.registry.format_for_model(provider, model);
        for connection in self.provider_connections.all_unmasked() {
            if !connection.enabled || connection.provider != provider { continue; }
            if let Some(metadata) = connection.model_metadata.get(model) {
                if let Some(candidate) = metadata.format.as_deref().and_then(crate::registry::Format::parse) {
                    return candidate;
                }
            }
            if let Some(candidate) = connection.api_type.as_deref().and_then(crate::registry::Format::parse) {
                format = candidate;
            }
        }
        format
    }

    pub fn model_is_hidden(&self, provider: &str, model: &str) -> bool {
        self.provider_connections
            .all_unmasked()
            .into_iter()
            .any(|connection| {
                connection.enabled
                    && connection.provider == provider
                    && connection.hidden_models.iter().any(|hidden| hidden == model)
            })
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

    /// Replace the account pool of one provider from the managed connections.
    ///
    /// Called after any connection create/update/delete so requests see the new
    /// roster immediately instead of at the next restart.
    pub fn refresh_account_pool(&self, provider: &str, accounts: Vec<AccountCredential>) {
        let mut pools = self.account_pools.write().unwrap_or_else(|e| e.into_inner());
        if accounts.is_empty() {
            pools.remove(provider);
        } else {
            pools.insert(provider.to_string(), accounts);
        }
    }

    /// All accounts of a provider, in stable order (empty when the provider is
    /// served from static config/env only).
    pub fn accounts_for(&self, provider: &str) -> Vec<AccountCredential> {
        self.account_pools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .cloned()
            .unwrap_or_default()
    }

    /// Pick the account that should serve the next request for `provider`.
    ///
    /// Round-robin across accounts, skipping any whose own circuit is
    /// cooling. Returns `None` when the provider has no managed accounts, which
    /// tells the caller to fall back to the single config/env credential.
    ///
    /// Rotation is process-wide (`account_cursor`), which matches how the
    /// combo `RR_COUNTER` behaves: even spreading across accounts for
    /// consecutive requests, without adding a lock per request.
    pub fn pick_account(&self, provider: &str) -> Option<AccountCredential> {
        let accounts = self.accounts_for(provider);
        if accounts.is_empty() {
            return None;
        }
        if accounts.len() == 1 {
            return accounts.into_iter().next();
        }
        // Prefer accounts that are not cooling down on their own key.
        let usable: Vec<AccountCredential> = accounts
            .iter()
            .filter(|a| self.circuits.cooldown_remaining_ms(&a.circuit_key(provider)) <= 0)
            .cloned()
            .collect();
        let pool = if usable.is_empty() { accounts } else { usable };
        let n = pool.len() as u64;
        if n == 0 {
            return None;
        }
        let offset = self
            .account_cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % n;
        pool.get(offset as usize).cloned()
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
                    // A dashboard connection may still point at the retired
                    // OpenCode Zen inference surface; the registry default is
                    // the supported one now.
                    return Some(
                        crate::opencode_free::normalize_base(provider, &b).unwrap_or(b),
                    );
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

    /// Write one provider connection file and return a state built on it.
    fn state_with_connection(conn: serde_json::Value) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("provider-connections.json"),
            serde_json::json!([conn]).to_string(),
        )
        .unwrap();
        let state = AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf()));
        (dir, state)
    }

    #[test]
    fn a_synced_connection_hides_the_builtin_registry_models() {
        // `opencode-zen` carries a static built-in seed list in the registry.
        // Once a connection has synced its upstream list, that input list is
        // authoritative: the seeds must not be listed next to the synced ids.
        let (_dir, state) = state_with_connection(serde_json::json!({
            "id": "zen",
            "provider": "opencode-zen",
            "name": "Zen",
            "api_key": "k",
            "baseUrl": "https://opencode.ai/inference/openai/v1",
            "models": [],
            "synced_models": ["synced-only-model"],
            "synced_at_ms": 1_790_000_000_000_u64,
            "enabled": true
        }));
        let builtin = state.registry.models_for("opencode-zen");
        assert!(!builtin.is_empty(), "fixture needs registry seeds to be meaningful");
        let models = state.models_for_provider("opencode-zen");
        assert_eq!(models, vec!["synced-only-model".to_string()], "only synced ids may remain");
        for seed in builtin {
            assert!(!models.contains(&seed), "built-in seed {seed} leaked: {models:?}");
        }
    }

    #[test]
    fn a_synced_provider_also_drops_the_live_free_roster() {
        // The free-lane live directory is a second built-in source. It must
        // also stand down once the provider has an authoritative synced list,
        // otherwise the roster ids reappear beside the synced ones.
        let _guard = crate::opencode_free::test_lock();
        crate::opencode_free::install_live(vec!["roster-only-free".to_string()]);
        let unsynced = state_with_connection(serde_json::json!({
            "id": "zen",
            "provider": "opencode-zen",
            "name": "Zen",
            "api_key": "k",
            "synced_models": [],
            "synced_at_ms": 0,
            "enabled": true
        }))
        .1;
        assert!(
            unsynced.models_for_provider("opencode-zen").contains(&"roster-only-free".to_string()),
            "before a sync the live roster is a legitimate source"
        );

        let synced = state_with_connection(serde_json::json!({
            "id": "zen",
            "provider": "opencode-zen",
            "name": "Zen",
            "api_key": "k",
            "synced_models": ["synced-only-model"],
            "synced_at_ms": 1_790_000_000_000_u64,
            "enabled": true
        }))
        .1;
        let models = synced.models_for_provider("opencode-zen");
        assert!(!models.contains(&"roster-only-free".to_string()), "roster leaked: {models:?}");
        assert_eq!(models, vec!["synced-only-model".to_string()]);
    }

    #[test]
    fn a_connection_that_never_synced_keeps_the_registry_seeds() {
        let (_dir, state) = state_with_connection(serde_json::json!({
            "id": "kaopu",
            "provider": "openai-compatible-kaopu",
            "name": "考谱AI",
            "api_key": "k",
            "baseUrl": "https://example.test/v1",
            "models": ["hand-added"],
            "synced_models": ["upstream-a"],
            "synced_at_ms": 0,
            "enabled": true
        }));
        let models = state.models_for_provider("openai-compatible-kaopu");
        // No sync stamp: the registry seeds (registered from the connection's
        // own lists) are the only source, so nothing is filtered out.
        assert!(models.contains(&"hand-added".to_string()));
        assert!(models.contains(&"upstream-a".to_string()));
    }

    fn state_with_connections(conns: serde_json::Value) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("provider-connections.json"),
            conns.to_string(),
        )
        .unwrap();
        let state = AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf()));
        (dir, state)
    }

    fn two_accounts() -> (tempfile::TempDir, AppState) {
        state_with_connections(serde_json::json!([
            {"id": "conn-a", "provider": "opencode-go", "name": "primary",
             "api_key": "key-a", "enabled": true},
            {"id": "conn-b", "provider": "opencode-go", "name": "backup",
             "api_key": "key-b", "enabled": true}
        ]))
    }

    /// Both accounts must be reachable: round-robin hands consecutive requests
    /// to different keys instead of always using whichever was saved last.
    #[test]
    fn consecutive_requests_rotate_across_accounts() {
        let (_dir, state) = two_accounts();
        let mut seen: Vec<String> = (0..4)
            .map(|_| state.pick_account("opencode-go").expect("an account").label())
            .collect();
        assert_eq!(seen.len(), 4);
        // Two picks of each within four draws.
        seen.sort();
        assert_eq!(seen, vec!["backup", "backup", "primary", "primary"]);
    }

    /// A provider with no managed account returns None so the caller keeps
    /// using the config/env credential.
    #[test]
    fn an_unmanaged_provider_has_no_account_pool() {
        let (_dir, state) = two_accounts();
        assert!(state.pick_account("openrouter").is_none());
        assert!(state.accounts_for("openrouter").is_empty());
    }

    /// A cooling account is skipped rather than repeatedly chosen: the whole
    /// point of per-account state is that one exhausted key must not consume
    /// the requests its sibling could serve.
    #[test]
    fn a_cooling_account_is_skipped() {
        let (_dir, state) = two_accounts();
        let accounts = state.accounts_for("opencode-go");
        let cooling = accounts
            .iter()
            .find(|a| a.label() == "primary")
            .unwrap()
            .clone();
        // Park only that account's own circuit key.
        state.circuits.record_failure(
            &cooling.circuit_key("opencode-go"),
            crate::errors::FailureKind::RateLimit,
            false,
            crate::registry::AuthType::ApiKey,
        );
        for _ in 0..4 {
            let picked = state.pick_account("opencode-go").expect("an account");
            assert_eq!(picked.label(), "backup", "a cooling account must be skipped");
        }
    }

    /// The account circuit key must be namespaced: a shared key would let one
    /// account's cooldown park its siblings (the bug this replaces).
    #[test]
    fn account_circuit_keys_do_not_collide() {
        let (_dir, state) = two_accounts();
        let keys: Vec<String> = state
            .accounts_for("opencode-go")
            .iter()
            .map(|a| a.circuit_key("opencode-go"))
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
        assert!(keys.iter().all(|k| k.starts_with("opencode-go#")));
        assert!(!keys.iter().any(|k| k == "opencode-go"), "must not reuse the provider key");
    }

    /// The request log must name the account that served the request: the
    /// dashboard's "recent requests" shows it as its own column, and without it
    /// an idle account of a multi-account provider stays invisible.
    #[test]
    fn a_log_entry_can_name_the_serving_account() {
        let (_dir, state) = two_accounts();
        state.log_request(RequestLogEntry {
            ts_ms: 1,
            model: "combo-x".into(),
            combo: Some("combo-x".into()),
            combo_known: true,
            routed_model: Some("opencode-go/m".into()),
            provider: Some("opencode-go".into()),
            account: Some("backup".into()),
            endpoint: "/v1/chat/completions".into(),
            reasoning_effort: None,
            client_ip: None,
            status: 200,
            latency_ms: 5,
            tokens_saved: 0,
            compressed: false,
            prompt_tokens: 1,
            completion_tokens: 1,
            stream: false,
        });
        let row = state.request_log_snapshot(1).into_iter().next().expect("a row");
        assert_eq!(row.account.as_deref(), Some("backup"));
        // Serialized for /v1/logs: the dashboard reads `account` directly.
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["account"], serde_json::json!("backup"));
    }

    /// A single-credential provider omits the field rather than sending null, so
    /// the dashboard's column stays clean for the common case.
    #[test]
    fn an_accountless_log_entry_omits_the_field() {
        let row = RequestLogEntry {
            ts_ms: 1,
            model: "m".into(),
            combo: None,
            combo_known: true,
            routed_model: None,
            provider: Some("openrouter".into()),
            account: None,
            endpoint: String::new(),
            reasoning_effort: None,
            client_ip: None,
            status: 200,
            latency_ms: 1,
            tokens_saved: 0,
            compressed: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            stream: false,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert!(json.get("account").is_none(), "unmanaged traffic must not carry an account");
    }

    /// The pool follows the connection store: a deleted account stops being
    /// handed out.
    #[test]
    fn removing_an_account_drops_it_from_the_pool() {
        let (_dir, state) = two_accounts();
        assert_eq!(state.accounts_for("opencode-go").len(), 2);
        state.refresh_account_pool(
            "opencode-go",
            vec![AccountCredential {
                connection_id: "conn-a".into(),
                name: "primary".into(),
                api_key: Some("key-a".into()),
                base_url: None,
                api_type: None,
            }],
        );
        let remaining = state.accounts_for("opencode-go");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].label(), "primary");
    }
}
