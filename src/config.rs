//! Configuration layer (parity: `bin/cli/data-dir.mjs` +
//! `open-sse/config/credentialLoader.ts` + `bin/cli/commands/serve.mjs`).
//!
//! Layering, first-wins per key like the original `loadEnvFile`:
//!   1. `$DATA_DIR/.env`
//!   2. `~/.omniroute-rust/.env`
//!   3. `cwd/.env`
//!   4. process environment
//!
//! Credentials: `$DATA_DIR/provider-credentials.json` (same schema as the
//! original) + `<PROVIDER>_API_KEY` env vars. Runtime tuning: `$DATA_DIR/omniroute.toml`.

use crate::registry::{AuthType, Format, Registry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DEFAULT_PORT: u16 = 20128;

pub fn is_usable_api_key(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    !value.is_empty()
        && !value.contains("your-")
        && !value.contains("placeholder")
        && !value.contains("change-me")
        && !value.contains("changeme")
}

pub fn is_usable_base_url(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    !value.is_empty()
        && !value.contains("example.")
        && !value.contains("your-")
        && !value.contains("placeholder")
}

// Rate limit defaults (parity: DEFAULT_API_LIMITS in `open-sse/config/constants.ts`
// — 60 RPM / 350ms min interval / 6 concurrent, applied to api-key providers;
// local providers bypass the interval limiter).
pub const DEFAULT_RATE_RPM: u64 = 60;
pub const DEFAULT_RATE_MIN_INTERVAL_MS: u64 = 350;
pub const DEFAULT_RATE_CONCURRENCY: i64 = 6;
pub const DEFAULT_RATE_MAX_WAIT_MS: u64 = 30_000; // RATE_LIMIT_MAX_WAIT_MS parity

/// Largest request body the router accepts (bytes). Axum's built-in default is
/// 2 MiB, which rejects a long agent context — a 1M-token window is several
/// MiB of JSON — with a bare 413 before any routing happens.
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub const CONNECT_TIMEOUT_MS: u64 = 30_000;
pub const REQUEST_TIMEOUT_MS: u64 = 600_000;
pub const STREAM_IDLE_TIMEOUT_MS: u64 = 600_000;
pub const STREAM_READINESS_TIMEOUT_MS: u64 = 80_000;
pub const STREAM_READINESS_MAX_TIMEOUT_MS: u64 = 180_000;
pub const SSE_HEARTBEAT_INTERVAL_MS: u64 = 15_000;
pub const DISCONNECT_GRACE_MS: u64 = 10_000;

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProviderTuning {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_type: Option<String>,
    #[serde(default)]
    pub chat_path: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ComboConfig {
    pub name: String,
    /// strategy id (priority|round-robin|fill-first|weighted|random|
    /// least-used|p2c|cost-optimized|auto|lkgp|...)
    pub strategy: Option<String>,
    /// entries are `provider` or `provider/model`
    pub providers: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ServerTuning {
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub host: Option<String>,
    /// `[server] max_body_bytes` — request body cap, overridden by
    /// `OMNIROUTE_MAX_BODY_BYTES`.
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ThinkingTuning {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    budget: Option<i64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TomlFile {
    #[serde(default)]
    server: ServerTuning,
    #[serde(default)]
    providers: HashMap<String, ProviderTuning>,
    #[serde(default)]
    combos: Vec<ComboConfig>,
    /// `[compression]` table parsed as a raw value (schema owned by
    /// `compression::CompressionConfig`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compression: Option<serde_json::Value>,
    #[serde(default)]
    thinking: ThinkingTuning,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ProviderCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "apiKey")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "apiType")]
    pub api_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "models")]
    pub model_list: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Request body cap for the whole router (`OMNIROUTE_MAX_BODY_BYTES`).
    pub max_body_bytes: usize,
    pub data_dir: PathBuf,
    pub api_key: Option<String>,
    pub request_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub stream_idle_timeout_ms: u64,
    pub heartbeat_ms: u64,
    pub readiness_timeout_ms: u64,
    pub readiness_max_timeout_ms: u64,
    pub disconnect_grace_ms: u64,
    /// provider id → credentials
    /// rate limit knobs (env-overridable, parity with the original's request queue)
    pub rate_rpm: u64,
    pub rate_min_interval_ms: u64,
    pub rate_concurrent_requests: i64,
    pub rate_max_wait_ms: u64,
    pub rate_auto_enable_api_key_providers: bool,
    pub credentials: HashMap<String, ProviderCredentials>,
    /// provider id → tuning from omniroute.toml
    pub tuning: HashMap<String, ProviderTuning>,
    pub combos: Vec<ComboConfig>,
    pub compression: crate::compression::CompressionConfig,
    /// Client reasoning policy: passthrough|auto|custom|adaptive.
    pub thinking_mode: String,
    pub thinking_budget: Option<i64>,
    pub log_level: String,
}

/// Find the data dir: `$OMNIROUTE_DATA_DIR` → `$DATA_DIR` → `~/.omniroute-rust`.
pub fn resolve_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("OMNIROUTE_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".omniroute-rust")
}

/// Parse an `.env` file (KEY=VALUE lines, `#` comments, quotes stripped).
pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let mut v = v.trim().to_string();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v = v[1..v.len() - 1].to_string();
        }
        if !k.is_empty() {
            map.insert(k.to_string(), v);
        }
    }
    map
}

/// loadEnvFile parity: first file wins per variable.
pub fn load_env_layers(data_dir: &Path) -> HashMap<String, String> {
    let mut merged = HashMap::new();
    let candidates = [
        data_dir.join(".env"),
        home_dir().join(".omniroute-rust").join(".env"),
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(".env"),
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for (k, v) in parse_env_file(&text) {
                merged.entry(k).or_insert(v);
            }
        }
    }
    merged
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Read provider-credentials.json from the data dir (same filename/schema as
/// the TS gateway). Accepts both:
///   {"providers": {"<id>": {...}}} and flat {"<id>": {...}}
pub fn load_credentials_file(data_dir: &Path) -> anyhow::Result<HashMap<String, ProviderCredentials>> {
    let path = data_dir.join("provider-credentials.json");
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let root: serde_json::Value = serde_json::from_str(&text)?;
    let obj = root.get("providers").and_then(|p| p.as_object()).cloned()
        .or_else(|| root.as_object().cloned())
        .unwrap_or_default();
    let mut out = HashMap::new();
    for (id, v) in obj {
        let creds: ProviderCredentials = serde_json::from_value(v)?;
        out.insert(id, creds);
    }
    Ok(out)
}

impl Config {
    /// Load full configuration. `port_override` comes from the CLI `--port`.
    pub fn load(port_override: Option<u16>) -> anyhow::Result<Self> {
        let data_dir = resolve_data_dir();
        let env = load_env_layers(&data_dir);
        let env_get = |k: &str| -> Option<String> {
            std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| env.get(k).cloned())
        };

        // port: CLI --port > PORT env > toml > 20128
        let toml_path = data_dir.join("omniroute.toml");
        let toml_file: TomlFile = if toml_path.exists() {
            let text = std::fs::read_to_string(&toml_path)?;
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", toml_path.display()))?
        } else {
            TomlFile::default()
        };

        let port = port_override
            .or_else(|| env_get("PORT").and_then(|v| v.parse().ok()))
            .or(toml_file.server.port)
            .unwrap_or(DEFAULT_PORT);

        let host = env_get("HOST")
            .or_else(|| toml_file.server.host.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string());

        // request body cap: OMNIROUTE_MAX_BODY_BYTES env > [server] toml > 32 MiB
        let max_body_bytes = env_get("OMNIROUTE_MAX_BODY_BYTES")
            .and_then(|v| v.parse().ok())
            .or(toml_file.server.max_body_bytes)
            .unwrap_or(DEFAULT_MAX_BODY_BYTES);

        let mut credentials = load_credentials_file(&data_dir).unwrap_or_default();

        // Env-var keys: <PROVIDER>_API_KEY with provider ids uppercased and
        // dashes kept (e.g. OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY,
        // OPENROUTER_API_KEY, ANTHROPIC_API_KEY...).
        for id in known_provider_ids() {
            let var = format!("{}_API_KEY", id.to_uppercase());
            if let Some(key) = env_get(&var) {
                credentials.entry(id.to_string()).or_default().api_key = Some(key);
            }
        }

        let tuning = toml_file.providers;
        let combos = toml_file.combos;
        let compression = crate::compression::CompressionConfig::from_toml_and_env(
            toml_file.compression.as_ref(),
            &|k| env_get(k),
        );
        let thinking_mode = env_get("OMNIROUTE_THINKING_MODE")
            .or(toml_file.thinking.mode)
            .unwrap_or_else(|| "passthrough".into());
        let thinking_budget = env_get("OMNIROUTE_THINKING_BUDGET")
            .and_then(|v| v.parse().ok())
            .or(toml_file.thinking.budget);
        let log_level = env_get("OMNIROUTE_LOG").unwrap_or_else(|| "info".into());

        Ok(Self {
            host,
            port,
            max_body_bytes,
            data_dir,
            api_key: env_get("OMNIROUTE_API_KEY"),
            request_timeout_ms: env_get("REQUEST_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(REQUEST_TIMEOUT_MS),
            connect_timeout_ms: env_get("FETCH_CONNECT_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(CONNECT_TIMEOUT_MS),
            stream_idle_timeout_ms: env_get("STREAM_IDLE_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(STREAM_IDLE_TIMEOUT_MS),
            heartbeat_ms: env_get("SSE_HEARTBEAT_INTERVAL_MS").and_then(|v| v.parse().ok()).unwrap_or(SSE_HEARTBEAT_INTERVAL_MS),
            readiness_timeout_ms: env_get("STREAM_READINESS_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(STREAM_READINESS_TIMEOUT_MS),
            readiness_max_timeout_ms: env_get("STREAM_READINESS_MAX_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(STREAM_READINESS_MAX_TIMEOUT_MS),
            disconnect_grace_ms: env_get("STREAM_DISCONNECT_GRACE_PERIOD_MS").and_then(|v| v.parse().ok()).unwrap_or(DISCONNECT_GRACE_MS),
            rate_rpm: env_get("OMNIROUTE_REQUESTS_PER_MINUTE").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_RATE_RPM),
            rate_min_interval_ms: env_get("OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_RATE_MIN_INTERVAL_MS),
            rate_concurrent_requests: env_get("OMNIROUTE_CONCURRENT_REQUESTS").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_RATE_CONCURRENCY),
            rate_max_wait_ms: env_get("RATE_LIMIT_MAX_WAIT_MS").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_RATE_MAX_WAIT_MS),
            rate_auto_enable_api_key_providers: env_get("RATE_LIMIT_AUTO_ENABLE_API_KEY")
                .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
                .unwrap_or(true),
            credentials,
            tuning,
            combos,
            compression,
            thinking_mode,
            thinking_budget,
            log_level,
        })
    }

    /// Build the effective registry: static entries + dynamic families from
    /// credentials/tuning, skipping disabled providers and those without keys
    /// (except local providers which need no auth).
    pub fn effective_registry(&self) -> Arc<Registry> {
        let reg = Registry::new(crate::registry::static_registry());

        // dynamic families from credentials
        let mut dyn_ids: Vec<String> = self.credentials.keys().cloned().collect();
        dyn_ids.sort();
        for id in dyn_ids {
            if id.starts_with("openai-compatible") || id.starts_with("anthropic-compatible") {
                let c = self.credentials.get(&id).cloned().unwrap_or_default();
                if c.enabled == Some(false) {
                    continue;
                }
                reg.register_dynamic(&id, c.base_url.clone(), c.api_type.clone(), c.model_list.clone());
            }
        }
        // dynamic families from toml tuning
        let mut tune_ids: Vec<String> = self.tuning.keys().cloned().collect();
        tune_ids.sort();
        for id in tune_ids {
            if id.starts_with("openai-compatible") || id.starts_with("anthropic-compatible") {
                let t = self.tuning.get(&id).cloned().unwrap_or_default();
                if t.enabled == Some(false) {
                    continue;
                }
                reg.register_dynamic(&id, t.base_url.clone(), t.api_type.clone(), t.models.clone());
            }
        }
        Arc::new(reg)
    }

    /// Providers with a usable credential (api key or local no-auth).
    pub fn providers_with_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let reg = self.effective_registry();
        for id in reg.ids() {
            let local = reg.get(&id).map(|e| e.is_local).unwrap_or(false);
            let has_key = self.api_key_for(&id).is_some();
            let placeholder_base = self
                .credentials
                .get(&id)
                .and_then(|c| c.base_url.as_deref())
                .or_else(|| self.tuning.get(&id).and_then(|t| t.base_url.as_deref()))
                .is_some_and(|base| !is_usable_base_url(base));
            if (local || has_key) && !placeholder_base {
                out.push(id);
            }
        }
        out
    }

    /// Resolve API key for a provider: credentials file/env, else tuning.
    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        if let Some(c) = self.credentials.get(provider) {
            if c.enabled != Some(false) {
                if let Some(k) = c.api_key.clone().filter(|k| is_usable_api_key(k)) {
                    return Some(k);
                }
            }
        }
        if let Some(t) = self.tuning.get(provider) {
            if t.enabled != Some(false) {
                if let Some(k) = t.api_key.clone().filter(|k| is_usable_api_key(k)) {
                    return Some(k);
                }
            }
        }
        None
    }

    pub fn tuning_for(&self, provider: &str) -> Option<&ProviderTuning> {
        self.tuning.get(provider)
    }

    pub fn base_url_for(&self, reg: &Registry, provider: &str) -> Option<String> {
        if let Some(t) = self.tuning.get(provider) {
            if let Some(b) = t.base_url.as_ref().filter(|b| is_usable_base_url(b)) {
                return Some(b.clone());
            }
        }
        if let Some(c) = self.credentials.get(provider) {
            if let Some(b) = c.base_url.as_ref().filter(|b| is_usable_base_url(b)) {
                return Some(b.clone());
            }
        }
        reg.get(provider).map(|e| e.base_url.clone())
    }

    pub fn auth_type_for(&self, reg: &Registry, provider: &str) -> AuthType {
        reg.get(provider).map(|e| e.auth_type).unwrap_or(AuthType::ApiKey)
    }

    pub fn format_for(&self, reg: &Registry, provider: &str) -> Format {
        reg.get(provider).map(|e| e.format).unwrap_or(Format::OpenAI)
    }
}

/// Ids that participate in `<ID>_API_KEY` env lookup (static registry ids).
pub fn known_provider_ids() -> Vec<&'static str> {
    crate::registry::static_registry().iter().map(|e| Box::leak(e.id.clone().into_boxed_str()) as &'static str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let text = "# comment\nFOO=bar\nexport BAZ=\"quoted val\"\nEMPTY=\nQ='single'\nBAD LINE\nNUM=123\n";
        let m = parse_env_file(text);
        assert_eq!(m.get("FOO").unwrap(), "bar");
        assert_eq!(m.get("BAZ").unwrap(), "quoted val");
        assert_eq!(m.get("Q").unwrap(), "single");
        assert_eq!(m.get("NUM").unwrap(), "123");
    }

    #[test]
    fn credentials_file_both_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("provider-credentials.json");
        std::fs::write(&p1, r#"{"providers":{"openai":{"apiKey":"sk-1"}}}"#).unwrap();
        let m = load_credentials_file(dir.path()).unwrap();
        assert_eq!(m.get("openai").unwrap().api_key.as_deref(), Some("sk-1"));

        let p2 = dir.path().join("flat.json");
        std::fs::write(&p2, r#"{"gemini":{"api_key":"g-1","baseUrl":"https://x.y"}}"#).unwrap();
        // flat shape at another path is not loaded by name; verify via rename
        std::fs::rename(&p2, dir.path().join("provider-credentials.json")).unwrap();
        let m = load_credentials_file(dir.path()).unwrap();
        assert_eq!(m.get("gemini").unwrap().api_key.as_deref(), Some("g-1"));
        assert_eq!(m.get("gemini").unwrap().base_url.as_deref(), Some("https://x.y"));
    }

    #[test]
    fn env_api_key_mapping() {
        // ZAI_API_KEY maps to provider "zai" (uppercased id + _API_KEY)
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "OMNIROUTE_API_KEY=sk-master\nZAI_API_KEY=z-key\nPORT=21234\n").unwrap();
        unsafe { std::env::set_var("OMNIROUTE_DATA_DIR", dir.path()); }
        unsafe { std::env::remove_var("ZAI_API_KEY"); }
        unsafe { std::env::remove_var("PORT"); }
        let cfg = Config::load(None).unwrap();
        assert_eq!(cfg.port, 21234);
        assert_eq!(cfg.api_key.as_deref(), Some("sk-master"));
        assert_eq!(cfg.api_key_for("zai").as_deref(), Some("z-key"));
        unsafe { std::env::remove_var("OMNIROUTE_DATA_DIR"); }
    }
}
