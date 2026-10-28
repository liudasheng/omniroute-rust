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

/// Provider `/models` background refresh cadence. Four hours keeps
/// dashboard-managed connections reasonably fresh without polling every
/// upstream on a tight loop; `OMNIROUTE_PROVIDER_SYNC_TTL_SECONDS` overrides it.
pub const DEFAULT_PROVIDER_SYNC_TTL_SECONDS: u64 = 4 * 3_600;

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

/// `[opencode_free]` table of `omniroute.toml`: credential mode and local
/// budget knobs of the OpenCode Zen free lane.
///
/// Everything here is also settable from the environment (which wins), so an
/// operator can roll a window limit onto a running host without editing TOML.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct OpenCodeFreeTuning {
    #[serde(default, alias = "credentialMode")]
    pub credential_mode: Option<String>,
    #[serde(default, alias = "dailyLimitPerAccount")]
    pub daily_limit_per_account: Option<u64>,
    #[serde(default, alias = "dailyLimitPublic")]
    pub daily_limit_public: Option<u64>,
    #[serde(default, alias = "learnedLimit")]
    pub learned_limit: Option<bool>,
    #[serde(default, alias = "learnedLimitTtlDays")]
    pub learned_limit_ttl_days: Option<u64>,
    #[serde(default, alias = "maxInFlightPerAccount")]
    pub max_in_flight_per_account: Option<usize>,
    #[serde(default, alias = "sessionFallback")]
    pub session_fallback: Option<bool>,
    #[serde(default, alias = "sessionMemoFile")]
    pub session_memo_file: Option<String>,
}

/// The resolved free-lane tuning (`env` > `[opencode_free]` > defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeFreeSettings {
    pub credential_mode: crate::opencode_free_budget::CredentialMode,
    pub daily_limit_per_account: u64,
    pub daily_limit_public: u64,
    pub learned_limit: bool,
    pub learned_limit_ttl_days: u64,
    pub max_in_flight_per_account: usize,
    pub session_fallback: bool,
    pub session_memo_file: String,
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "on" | "yes"
    )
}

fn falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "off" | "no"
    )
}

/// Resolve the free-lane tuning from an environment reader and the TOML table.
///
/// Split out of [`Config::load`] so the precedence and the defaults are testable
/// without mutating process environment (tests run in parallel).
pub fn resolve_opencode_free_settings(
    env_get: &dyn Fn(&str) -> Option<String>,
    toml: &OpenCodeFreeTuning,
) -> OpenCodeFreeSettings {
    use crate::opencode_free_budget::{
        CredentialMode, DEFAULT_DAILY_LIMIT_PER_ACCOUNT, DEFAULT_DAILY_LIMIT_PUBLIC,
        DEFAULT_MAX_IN_FLIGHT_PER_ACCOUNT,
    };
    // Both the documented bare name and the module-prefixed name are accepted;
    // the prefixed one wins, matching the rest of the OpenCode knobs.
    let env = |name: &str| -> Option<String> {
        env_get(&format!("OMNIROUTE_{name}")).or_else(|| env_get(name))
    };
    // `OMNIROUTE_OPENCODE_FREE_USE_KEY` predates the mode knob: map it so an
    // existing deployment keeps the behaviour it asked for.
    let legacy_mode = env("OPENCODE_FREE_USE_KEY").map(|value| {
        if truthy(&value) {
            CredentialMode::Key
        } else {
            CredentialMode::Public
        }
    });
    let credential_mode = env("OPENCODE_FREE_CREDENTIAL_MODE")
        .and_then(|value| CredentialMode::parse(&value))
        .or(legacy_mode)
        .or_else(|| {
            toml.credential_mode
                .as_deref()
                .and_then(CredentialMode::parse)
        })
        .unwrap_or_default();
    let daily_limit_per_account = env("OPENCODE_FREE_DAILY_LIMIT_PER_ACCOUNT")
        .and_then(|value| value.trim().parse().ok())
        .or(toml.daily_limit_per_account)
        .unwrap_or(DEFAULT_DAILY_LIMIT_PER_ACCOUNT)
        .max(1);
    let daily_limit_public = env("OPENCODE_FREE_DAILY_LIMIT_PUBLIC")
        .and_then(|value| value.trim().parse().ok())
        .or(toml.daily_limit_public)
        .unwrap_or(DEFAULT_DAILY_LIMIT_PUBLIC)
        .max(1);
    let learned_limit = env("OPENCODE_FREE_LEARNED_LIMIT")
        .map(|value| !falsy(&value))
        .or(toml.learned_limit)
        .unwrap_or(true);
    // A learned ceiling only moves down while 429s keep proving it; after this
    // many days without a fresh 429 it is raised by 20 % per elapsed period so
    // an upstream that relaxed its quota is picked up without a restart.
    // 0 disables the re-evaluation (the learned value then never expires).
    let learned_limit_ttl_days = env("OPENCODE_FREE_LEARNED_LIMIT_TTL_DAYS")
        .and_then(|value| value.trim().parse().ok())
        .or(toml.learned_limit_ttl_days)
        .unwrap_or(crate::opencode_free_budget::DEFAULT_LEARNED_LIMIT_TTL_DAYS);
    let max_in_flight_per_account = env("OPENCODE_FREE_MAX_IN_FLIGHT_PER_ACCOUNT")
        .and_then(|value| value.trim().parse().ok())
        .or(toml.max_in_flight_per_account)
        .unwrap_or(DEFAULT_MAX_IN_FLIGHT_PER_ACCOUNT);
    // A fresh session refused with a long Retry-After while an already-accepted
    // one still answers is the upstream's session-admission quota, not a budget
    // exhaustion: retry the same body once on a warm session instead of
    // cooling the credential.
    //
    // Default OFF until the upstream confirms that a warm id is accepted for a
    // *different* body (the observed admission behaviour is new and still being
    // validated); the persistent memo and the shared probe session are the
    // parts that are unconditionally safe.
    let session_fallback = env("OPENCODE_FREE_SESSION_FALLBACK")
        .map(|value| !falsy(&value))
        .or(toml.session_fallback)
        .unwrap_or(false);
    let session_memo_file = env("OPENCODE_FREE_SESSION_MEMO_FILE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| toml.session_memo_file.clone())
        .unwrap_or_else(|| crate::opencode_free::SESSION_FILE.to_string());
    OpenCodeFreeSettings {
        credential_mode,
        daily_limit_per_account,
        daily_limit_public,
        learned_limit,
        learned_limit_ttl_days,
        max_in_flight_per_account,
        session_fallback,
        session_memo_file,
    }
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
    /// `[opencode_free]` free-lane credential mode and local budget.
    #[serde(default)]
    opencode_free: OpenCodeFreeTuning,
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
    /// Drop historical `reasoning_content` before dispatching.
    ///
    /// A previous turn's chain-of-thought is not an input any upstream needs —
    /// neither OpenAI nor DeepSeek reads it back — but feeding it to a model
    /// makes it *imitate its own earlier wording*. Measured on
    /// `opencode-go/beam-free`: seeding six turns whose reasoning all followed
    /// "用户问X。…我需要简洁回答。" made every later reply's reasoning that same
    /// template, which is exactly the "reasoning is all the same" symptom.
    /// Signed reasoning (Claude `thinking` blocks with a signature,
    /// `reasoning_details`) is kept: Anthropic requires it verbatim.
    pub strip_historical_reasoning: bool,
    /// Runtime model-capability catalog (context windows / modalities).
    pub model_catalog_auto_update: bool,
    pub model_catalog_ttl_hours: u64,
    pub model_catalog_url: String,
    /// Discover the anonymous OpenCode Zen free roster from the live directory.
    pub opencode_free_auto_update: bool,
    pub opencode_free_ttl_seconds: u64,
    pub opencode_free_url: String,
    /// How free-lane models authenticate: `auto` rotates the provider's
    /// managed account keys and falls back to the anonymous credential,
    /// `key` also uses the env/config key, `public` is always anonymous.
    pub opencode_free_credential_mode: crate::opencode_free_budget::CredentialMode,
    /// Local UTC-day request ceiling per managed account (default 200). The
    /// upstream ceiling is undocumented, so this is a budget to spend against,
    /// tightened per window by `opencode_free_learned_limit`.
    pub opencode_free_daily_limit_per_account: u64,
    /// Local UTC-day request ceiling of the anonymous `public` credential.
    pub opencode_free_daily_limit_public: u64,
    /// Learn the real ceiling from observed `429 FreeUsageLimitError` answers
    /// (`learned_limit = min(learned, used_at_429)`, persisted across windows)
    /// and skip locally afterwards.
    pub opencode_free_learned_limit: bool,
    /// Days after which a stale learned ceiling is raised by 20 % per elapsed
    /// period (0 = never re-evaluate).
    pub opencode_free_learned_limit_ttl_days: u64,
    /// Requests a single account may have in flight before the request rotates
    /// to a sibling (0 = unlimited).
    pub opencode_free_max_in_flight_per_account: usize,
    /// Retry once on a warm (already accepted) session when a fresh session is
    /// refused with a long `Retry-After`, before treating it as quota
    /// exhaustion.
    pub opencode_free_session_fallback: bool,
    /// Filename of the persisted conversation-session memo inside the data dir.
    pub opencode_free_session_memo_file: String,
    /// Discover OpenCode Go rolling/weekly/monthly subscription usage.
    pub opencode_quota_auto_update: bool,
    pub opencode_quota_ttl_seconds: u64,
    /// Sync provider `synced_models` from upstream `/models` endpoints.
    pub provider_sync_auto_update: bool,
    /// Cadence in seconds. Sub-hour values are the point of the seconds knob:
    /// they make the background loop observable in a test or a smoke run.
    pub provider_sync_ttl_seconds: u64,
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

/// Persist compression settings into `<data_dir>/omniroute.toml` under the
/// `[compression]` table, so dashboard changes survive a restart.
///
/// The rest of the file (server / providers / combos / thinking) is preserved:
/// the document is parsed, only the `compression` key is replaced, and the
/// result is written back atomically (temp file + rename).
///
/// Returns the path of the written file.
pub fn persist_compression_config(
    data_dir: &std::path::Path,
    c: &crate::compression::CompressionConfig,
) -> Result<std::path::PathBuf, String> {
    let path = data_dir.join("omniroute.toml");

    let mut root: toml::Value = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|_| toml::Value::Table(Default::default())),
        Err(_) => toml::Value::Table(Default::default()),
    };
    if !root.is_table() {
        root = toml::Value::Table(Default::default());
    }

    let table = toml::Value::try_from(c).map_err(|e| format!("serialize compression config: {e}"))?;
    root.as_table_mut()
        .ok_or_else(|| "omniroute.toml is not a table".to_string())?
        .insert("compression".to_string(), table);

    let text = toml::to_string_pretty(&root).map_err(|e| format!("render omniroute.toml: {e}"))?;

    std::fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    Ok(path)
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
        // Default on: the flag only ever removes redundant input, and its
        // absence measurably degraded reasoning variety.
        let strip_historical_reasoning = env_get("OMNIROUTE_STRIP_HISTORICAL_REASONING")
            .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
            .unwrap_or(true);
        let model_catalog_auto_update = env_get("OMNIROUTE_MODEL_CATALOG_AUTOUPDATE")
            .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
            .unwrap_or(true);
        let model_catalog_ttl_hours = env_get("OMNIROUTE_MODEL_CATALOG_TTL_HOURS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(24);
        let model_catalog_url = env_get("OMNIROUTE_MODEL_CATALOG_URL")
            .unwrap_or_else(|| crate::model_catalog::CATALOG_URL.to_string());
        // OpenCode lanes: the anonymous Zen free roster is discovered from the
        // live directory (~7 KB), so its cadence can be much shorter than the
        // models.dev capability catalog.
        let opencode_free_auto_update = env_get("OMNIROUTE_OPENCODE_FREE_AUTOUPDATE")
            .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
            .unwrap_or(true);
        // The Zen directory is a slow-moving roster (~7 KB); 12h is plenty.
        // Availability is learned per request (`record_verdict`), and the
        // composite ranking is recomputed continuously at read time, so a
        // shorter directory cadence buys nothing.
        let opencode_free_ttl_seconds = env_get("OMNIROUTE_OPENCODE_FREE_TTL_SECONDS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::opencode_free::DEFAULT_REFRESH_SECONDS);
        let opencode_free_url = env_get("OMNIROUTE_OPENCODE_FREE_URL")
            .unwrap_or_else(|| crate::opencode_free::ZEN_BASE_URL.to_string());
        // Free-lane credential mode and local budget: env > [opencode_free] >
        // defaults. `OMNIROUTE_OPENCODE_FREE_USE_KEY` keeps working as a legacy
        // alias of the `key`/`public` modes.
        let opencode_free_settings = resolve_opencode_free_settings(&env_get, &toml_file.opencode_free);
        // OpenCode Go publishes rolling/weekly/monthly subscription usage.
        let opencode_quota_auto_update = env_get("OMNIROUTE_OPENCODE_QUOTA_AUTOUPDATE")
            .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
            .unwrap_or(true);
        let opencode_quota_ttl_seconds = env_get("OMNIROUTE_OPENCODE_QUOTA_TTL_SECONDS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        // Sync provider synced_models from upstream /models.
        let provider_sync_auto_update = env_get("OMNIROUTE_PROVIDER_SYNC_AUTOUPDATE")
            .map(|v| !matches!(v.as_str(), "false" | "0" | "off"))
            .unwrap_or(true);
        // Seconds win so the cadence can be dialled below one hour for tests
        // and smoke runs; the hours variable stays as the coarse-grained knob.
        let provider_sync_ttl_seconds = env_get("OMNIROUTE_PROVIDER_SYNC_TTL_SECONDS")
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                env_get("OMNIROUTE_PROVIDER_SYNC_TTL_HOURS")
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|h| h.max(1).saturating_mul(3_600))
            })
            .unwrap_or(DEFAULT_PROVIDER_SYNC_TTL_SECONDS)
            .max(1);
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
            strip_historical_reasoning,
            model_catalog_auto_update,
            model_catalog_ttl_hours,
            model_catalog_url,
            opencode_free_auto_update,
            opencode_free_ttl_seconds,
            opencode_free_url,
            opencode_free_credential_mode: opencode_free_settings.credential_mode,
            opencode_free_daily_limit_per_account: opencode_free_settings
                .daily_limit_per_account,
            opencode_free_daily_limit_public: opencode_free_settings.daily_limit_public,
            opencode_free_learned_limit: opencode_free_settings.learned_limit,
            opencode_free_learned_limit_ttl_days: opencode_free_settings
                .learned_limit_ttl_days,
            opencode_free_max_in_flight_per_account: opencode_free_settings
                .max_in_flight_per_account,
            opencode_free_session_fallback: opencode_free_settings.session_fallback,
            opencode_free_session_memo_file: opencode_free_settings.session_memo_file,
            opencode_quota_auto_update,
            opencode_quota_ttl_seconds,
            provider_sync_auto_update,
            provider_sync_ttl_seconds,
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
        // A stored base still pinned to the retired OpenCode Zen inference
        // surface is superseded by the registry default (see
        // `opencode_free::normalize_base`); any other configured base wins.
        let normalized = |b: &str| crate::opencode_free::normalize_base(provider, b);
        if let Some(t) = self.tuning.get(provider) {
            if let Some(b) = t.base_url.as_ref().filter(|b| is_usable_base_url(b)) {
                return Some(normalized(b).unwrap_or_else(|| b.clone()));
            }
        }
        if let Some(c) = self.credentials.get(provider) {
            if let Some(b) = c.base_url.as_ref().filter(|b| is_usable_base_url(b)) {
                return Some(normalized(b).unwrap_or_else(|| b.clone()));
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

    #[test]
    fn persist_compression_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = crate::compression::CompressionConfig::default();
        c.enabled = true;
        c.default_mode = crate::compression::CompressionMode::Lrc;
        c.lrc_profile = "aggressive".to_string();
        c.lrc_recent_turns = Some(5);
        c.lrc_retire_args = Some(true);
        c.lrc_dump = true;
        c.lrc_dump_dir = Some(std::path::PathBuf::from("/tmp/lrc-dumps"));

        let path = persist_compression_config(dir.path(), &c).unwrap();
        assert_eq!(path, dir.path().join("omniroute.toml"));

        // The written file must parse back into an equivalent config.
        let text = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&text).unwrap();
        let table = doc.get("compression").expect("[compression] table");
        assert_eq!(table.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(table.get("default_mode").and_then(|v| v.as_str()), Some("lrc"));
        assert_eq!(table.get("lrc_profile").and_then(|v| v.as_str()), Some("aggressive"));
        assert_eq!(table.get("lrc_recent_turns").and_then(|v| v.as_integer()), Some(5));
        assert_eq!(table.get("lrc_retire_args").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(table.get("lrc_dump").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            table.get("lrc_dump_dir").and_then(|v| v.as_str()),
            Some("/tmp/lrc-dumps")
        );

        // Other sections of the file must survive the write.
        let other = dir.path().join("omniroute.toml");
        std::fs::write(&other, "[server]\nport = 20128\n\n[thinking]\nmode = \"passthrough\"\n").unwrap();
        persist_compression_config(dir.path(), &c).unwrap();
        let text = std::fs::read_to_string(&other).unwrap();
        let doc: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(doc.get("server").and_then(|v| v.get("port")).and_then(|v| v.as_integer()), Some(20128));
        assert_eq!(
            doc.get("thinking").and_then(|v| v.get("mode")).and_then(|v| v.as_str()),
            Some("passthrough")
        );
        assert!(doc.get("compression").is_some());
    }

    #[test]
    fn persist_compression_config_is_read_back_by_config_load() {
        // End-to-end: what the dashboard saves must survive a restart.
        let dir = tempfile::tempdir().unwrap();
        let mut c = crate::compression::CompressionConfig::default();
        c.enabled = true;
        c.default_mode = crate::compression::CompressionMode::Lrc;
        c.lrc_profile = "extreme".to_string();
        persist_compression_config(dir.path(), &c).unwrap();

        unsafe { std::env::set_var("OMNIROUTE_DATA_DIR", dir.path()); }
        unsafe { std::env::remove_var("OMNIROUTE_COMPRESSION"); }
        let cfg = Config::load(None).unwrap();
        assert!(cfg.compression.enabled);
        assert_eq!(cfg.compression.default_mode, crate::compression::CompressionMode::Lrc);
        assert_eq!(cfg.compression.lrc_profile, "extreme");
        unsafe { std::env::remove_var("OMNIROUTE_DATA_DIR"); }
    }

    #[test]
    fn opencode_free_settings_default_to_the_documented_values() {
        let none = |_: &str| None;
        let settings = resolve_opencode_free_settings(&none, &OpenCodeFreeTuning::default());
        assert_eq!(
            settings.credential_mode,
            crate::opencode_free_budget::CredentialMode::Auto,
            "auto: rotate account keys when a pool exists, else anonymous"
        );
        assert_eq!(settings.daily_limit_per_account, 200);
        assert_eq!(settings.daily_limit_public, 200);
        assert!(settings.learned_limit);
        assert_eq!(settings.learned_limit_ttl_days, 7);
        assert_eq!(settings.max_in_flight_per_account, 2);
        assert!(
            !settings.session_fallback,
            "warm-session fallback ships off until the upstream validates cross-body reuse"
        );
        assert_eq!(
            settings.session_memo_file,
            crate::opencode_free::SESSION_FILE
        );
    }

    #[test]
    fn opencode_free_settings_take_env_over_toml_over_defaults() {
        let toml = OpenCodeFreeTuning {
            credential_mode: Some("public".into()),
            daily_limit_per_account: Some(50),
            daily_limit_public: Some(25),
            learned_limit: Some(false),
            learned_limit_ttl_days: Some(3),
            max_in_flight_per_account: Some(4),
            session_fallback: Some(false),
            session_memo_file: Some("sessions-test.json".into()),
        };
        // TOML only.
        let none = |_: &str| None;
        let settings = resolve_opencode_free_settings(&none, &toml);
        assert_eq!(
            settings.credential_mode,
            crate::opencode_free_budget::CredentialMode::Public
        );
        assert_eq!(settings.daily_limit_per_account, 50);
        assert_eq!(settings.daily_limit_public, 25);
        assert!(!settings.learned_limit);
        assert_eq!(settings.learned_limit_ttl_days, 3);
        assert_eq!(settings.max_in_flight_per_account, 4);
        assert!(!settings.session_fallback);
        assert_eq!(settings.session_memo_file, "sessions-test.json");

        // Environment wins, and the prefixed name wins over the bare one.
        let env = |name: &str| match name {
            "OMNIROUTE_OPENCODE_FREE_CREDENTIAL_MODE" => Some("key".to_string()),
            "OPENCODE_FREE_CREDENTIAL_MODE" => Some("public".to_string()),
            "OPENCODE_FREE_DAILY_LIMIT_PER_ACCOUNT" => Some("7".to_string()),
            "OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PUBLIC" => Some("3".to_string()),
            "OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT" => Some("true".to_string()),
            "OPENCODE_FREE_LEARNED_LIMIT_TTL_DAYS" => Some("0".to_string()),
            "OMNIROUTE_OPENCODE_FREE_MAX_IN_FLIGHT_PER_ACCOUNT" => Some("0".to_string()),
            "OPENCODE_FREE_SESSION_FALLBACK" => Some("false".to_string()),
            "OMNIROUTE_OPENCODE_FREE_SESSION_MEMO_FILE" => Some("sess-env.json".to_string()),
            _ => None,
        };
        let settings = resolve_opencode_free_settings(&env, &toml);
        assert_eq!(
            settings.credential_mode,
            crate::opencode_free_budget::CredentialMode::Key
        );
        assert_eq!(settings.daily_limit_per_account, 7);
        assert_eq!(settings.daily_limit_public, 3);
        assert!(settings.learned_limit);
        assert_eq!(
            settings.learned_limit_ttl_days, 0,
            "0 disables re-evaluating the learned ceiling"
        );
        assert_eq!(settings.max_in_flight_per_account, 0, "0 means unlimited");
        assert!(!settings.session_fallback, "a falsy env value disables the fallback");
        assert_eq!(settings.session_memo_file, "sess-env.json");
    }

    #[test]
    fn the_legacy_use_key_flag_maps_onto_the_mode() {
        let toml = OpenCodeFreeTuning::default();
        let on = |name: &str| (name == "OMNIROUTE_OPENCODE_FREE_USE_KEY").then(|| "true".into());
        assert_eq!(
            resolve_opencode_free_settings(&on, &toml).credential_mode,
            crate::opencode_free_budget::CredentialMode::Key
        );
        let off = |name: &str| (name == "OMNIROUTE_OPENCODE_FREE_USE_KEY").then(|| "false".into());
        assert_eq!(
            resolve_opencode_free_settings(&off, &toml).credential_mode,
            crate::opencode_free_budget::CredentialMode::Public,
            "an explicit false keeps the old anonymous behaviour"
        );
    }

    #[test]
    fn the_opencode_free_toml_table_parses() {
        let parsed: TomlFile = toml::from_str(
            "[opencode_free]\ncredential_mode = \"key\"\ndaily_limit_per_account = 120\ndaily_limit_public = 60\nlearned_limit = false\nlearned_limit_ttl_days = 14\nmax_in_flight_per_account = 3\nsession_fallback = false\nsession_memo_file = \"warm.json\"\n",
        )
        .unwrap();
        assert_eq!(parsed.opencode_free.credential_mode.as_deref(), Some("key"));
        assert_eq!(parsed.opencode_free.daily_limit_per_account, Some(120));
        assert_eq!(parsed.opencode_free.daily_limit_public, Some(60));
        assert_eq!(parsed.opencode_free.learned_limit, Some(false));
        assert_eq!(parsed.opencode_free.learned_limit_ttl_days, Some(14));
        assert_eq!(parsed.opencode_free.max_in_flight_per_account, Some(3));
        assert_eq!(parsed.opencode_free.session_fallback, Some(false));
        assert_eq!(parsed.opencode_free.session_memo_file.as_deref(), Some("warm.json"));
    }
}
