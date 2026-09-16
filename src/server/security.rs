//! Dashboard auth + API-key store (parity-lite of the original dashboard
//! JWT/session and `/api/keys` management).
//!
//! Bootstrap policy (parity with the original's first deployment): the admin
//! password defaults to "CHANGEME" until it is changed. `OMNIROUTE_ADMIN_PASSWORD`
//! env pins the password at boot. Records persist in
//! `$DATA_DIR/dashboard-auth.json` as salted sha256. Sessions are in-memory
//! bearer tokens (7-day TTL).

use sha2::{Digest, Sha256};
use std::sync::RwLock;

pub const CHANGEME: &str = "CHANGEME";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DashboardAuth {
    pub salt: String,
    pub password_sha256: String,
    pub created_at_ms: u128,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    /// "all" | "restricted" (parity: modelAccessMode)
    #[serde(default)]
    pub model_access_mode: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub allowed_combos: Vec<String>,
    #[serde(default, alias = "role")]
    pub role: String,
    #[serde(default = "default_key_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub no_log: bool,
    #[serde(default)]
    pub allow_usage_command: bool,
    #[serde(default)]
    pub usage_limit_enabled: bool,
    #[serde(default)]
    pub daily_usage_limit_usd: Option<f64>,
    #[serde(default)]
    pub weekly_usage_limit_usd: Option<f64>,
    #[serde(default)]
    pub chaos_mode_enabled: bool,
    /// standard | admin | restricted (parity: the original's key types)
    #[serde(default)]
    pub key_type: String,
    /// epoch ms; None = never expires
    #[serde(default)]
    pub expires_at_ms: Option<u128>,
    /// accumulated spend attributed to this key (USD, 0.0 when unpriced)
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub created_at_ms: u128,
    #[serde(default)]
    pub last_used_at_ms: Option<u128>,
    #[serde(default)]
    pub total_requests: u64,
}

fn default_key_enabled() -> bool {
    true
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn hash_with_salt(password: &str, salt: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(b":");
    h.update(password.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Public helper for id generation (`key_`/`combo_` prefixes).
pub fn random_id(n: usize) -> String {
    random_hex(n)
}

fn random_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:x}", rand::random::<u8>() % 16)).collect()
}

fn hashed(password: &str) -> DashboardAuth {
    let salt = random_hex(16);
    DashboardAuth {
        password_sha256: hash_with_salt(password, &salt),
        salt,
        created_at_ms: now_ms(),
    }
}

/// Reset the persisted admin password without a running server
/// (parity: `bin/reset-password.mjs`). Returns the hash record written.
pub fn reset_password(data_dir: &std::path::Path, new_password: &str) -> std::io::Result<DashboardAuth> {
    let dir = if data_dir.as_os_str().is_empty() { std::path::Path::new(".") } else { data_dir };
    std::fs::create_dir_all(dir)?;
    let rec = hashed(new_password);
    let path = dir.join("dashboard-auth.json");
    std::fs::write(&path, serde_json::to_string_pretty(&rec).unwrap_or_default())?;
    let _ = crate::set_file_mode_600(&path);
    Ok(rec)
}

pub struct AuthStore {
    pub record: RwLock<DashboardAuth>,
    /// session token → expiry ms
    pub sessions: dashmap::DashMap<String, u128>,
    /// true when dashboard login is required (always, post-bootstrap)
    pub login_enabled: bool,
    record_data_dir: Option<std::path::PathBuf>,
}

impl AuthStore {
    /// Bootstrap: `OMNIROUTE_ADMIN_PASSWORD` env wins; otherwise the default
    /// password is "CHANGEME" until changed. The hash persists to
    /// `$DATA_DIR/dashboard-auth.json`.
    pub fn new(data_dir: &std::path::Path, admin_password_env: Option<String>) -> Self {
        let path = data_dir.join("dashboard-auth.json");
        let existing = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<DashboardAuth>(&t).ok());

        let mut using_default = false;
        let record = match admin_password_env.filter(|p| !p.is_empty()) {
            Some(pw) => Some(hashed(&pw)),
            None => existing,
        };
        let record = match record {
            Some(r) => r,
            None => {
                // first deployment: default password "CHANGEME"
                using_default = true;
                hashed(CHANGEME)
            }
        };
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap_or_default());
        if using_default {
            println!(
                "[DASHBOARD] default admin password is \"CHANGEME\" — change it at /dashboard (Settings) or via OMNIROUTE_ADMIN_PASSWORD + restart"
            );
        }
        Self {
            record: RwLock::new(record),
            sessions: dashmap::DashMap::new(),
            login_enabled: true,
            record_data_dir: Some(data_dir.to_path_buf()),
        }
    }

    /// Fresh record: prefer what is on disk (so `omniroute reset-password`, or a
    /// manual edit, takes effect without restarting), fall back to the cache.
    fn current_record(&self) -> DashboardAuth {
        if let Some(dir) = &self.record_data_dir {
            if let Ok(t) = std::fs::read_to_string(dir.join("dashboard-auth.json")) {
                if let Ok(rec) = serde_json::from_str::<DashboardAuth>(&t) {
                    let cached = self.record.read().unwrap_or_else(|e| e.into_inner());
                    if cached.salt != rec.salt || cached.password_sha256 != rec.password_sha256 {
                        drop(cached);
                        *self.record.write().unwrap_or_else(|e| e.into_inner()) = rec.clone();
                    }
                    return rec;
                }
            }
        }
        self.record
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_default_password(&self) -> bool {
        let rec = self.current_record();
        rec.password_sha256 == hash_with_salt(CHANGEME, &rec.salt)
    }

    pub fn verify(&self, password: &str) -> bool {
        let rec = self.current_record();
        rec.password_sha256 == hash_with_salt(password, &rec.salt)
    }

    /// Change the admin password and persist the hash.
    pub fn change_password(&self, new_password: &str) {
        let rec = hashed(new_password);
        if let Some(dir) = &self.record_data_dir {
            let _ = std::fs::write(
                dir.join("dashboard-auth.json"),
                serde_json::to_string_pretty(&rec).unwrap_or_default(),
            );
        }
        *self.record.write().unwrap_or_else(|e| e.into_inner()) = rec;
    }

    /// Login → session token valid for 7 days.
    pub fn login(&self, password: &str) -> Option<String> {
        if !self.verify(password) {
            return None;
        }
        Some(self.new_session())
    }

    pub fn new_session(&self) -> String {
        let token = format!("sess_{}", random_hex(32));
        let expiry = now_ms() + 7 * 24 * 3600 * 1000;
        self.sessions.insert(token.clone(), expiry);
        token
    }

    pub fn logout(&self, token: &str) {
        self.sessions.remove(token);
    }

    /// Session valid? (purges expired entries opportunistically)
    pub fn session_valid(&self, token: &str) -> bool {
        let now = now_ms();
        self.sessions.retain(|_, exp| *exp > now);
        self.sessions.get(token).map(|exp| *exp > now).unwrap_or(false)
    }
}

/// API-key store backed by `$DATA_DIR/api-keys.json` (parity: /api/keys CRUD).
pub struct ApiKeyStore {
    keys: RwLock<Vec<ApiKeyEntry>>,
    path: std::path::PathBuf,
}

impl ApiKeyStore {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let path = data_dir.join("api-keys.json");
        let keys = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<ApiKeyEntry>>(&t).ok())
            .unwrap_or_default();
        Self { keys: RwLock::new(keys), path }
    }

    fn persist(&self, keys: &[ApiKeyEntry]) {
        let _ = std::fs::write(&self.path, serde_json::to_string_pretty(keys).unwrap_or_default());
    }

    pub fn list(&self) -> Vec<ApiKeyEntry> {
        self.keys.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Create a key; returns the entry with its full secret (shown once).
    /// Field parity with the original `createKeySchema` (keys.ts).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        name: &str,
        role: &str,
        model_access_mode: Option<&str>,
        allowed_models: Vec<String>,
        allowed_combos: Vec<String>,
        no_log: bool,
        allow_usage_command: bool,
        _usage_limit_enabled: bool,
        daily_usage_limit_usd: Option<f64>,
        weekly_usage_limit_usd: Option<f64>,
        chaos_mode_enabled: bool,
    ) -> ApiKeyEntry {
        let entry = ApiKeyEntry {
            id: format!("key_{}", random_hex(8)),
            name: if name.is_empty() { "default".into() } else { name.to_string() },
            key: format!("sk-or-{}", random_hex(32)),
            model_access_mode: model_access_mode.unwrap_or("all").into(),
            allowed_models,
            allowed_combos,
            role: if role == "admin" { "admin".into() } else { "default".into() },
            enabled: true,
            no_log,
            allow_usage_command,
            usage_limit_enabled: usage_limit_enabled_marker(daily_usage_limit_usd, weekly_usage_limit_usd),
            daily_usage_limit_usd,
            weekly_usage_limit_usd,
            chaos_mode_enabled,
            key_type: if role == "admin" {
                "admin".into()
            } else if model_access_mode == Some("restricted") {
                "restricted".into()
            } else {
                "standard".into()
            },
            expires_at_ms: None,
            cost_usd: 0.0,
            created_at_ms: now_ms(),
            last_used_at_ms: None,
            total_requests: 0,
        };
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.push(entry.clone());
        self.persist(&snapshot);
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
        entry
    }

    pub fn revoke(&self, id: &str) -> bool {
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        let before = snapshot.len();
        snapshot.retain(|x| x.id != id);
        self.persist(&snapshot);
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        snapshot.len() != before
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(e) = snapshot.iter_mut().find(|x| x.id == id) {
            e.enabled = enabled;
            self.persist(&snapshot);
            *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
            return true;
        }
        false
    }

    /// Derived status (parity: 启用 / 已禁用 / 已封禁 / 已过期).
    pub fn status_of(k: &ApiKeyEntry, now: u128) -> &'static str {
        if let Some(exp) = k.expires_at_ms {
            if exp > 0 && exp <= now {
                return "expired";
            }
        }
        if !k.enabled {
            return if k.key_type == "restricted" { "revoked" } else { "disabled" };
        }
        "enabled"
    }

    /// Rotate the secret of one key (parity: the row's rotate action).
    pub fn rotate(&self, id: &str) -> Option<ApiKeyEntry> {
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        let entry = snapshot.iter_mut().find(|x| x.id == id)?;
        entry.key = format!("sk-or-{}", random_hex(32));
        entry.last_used_at_ms = None;
        let out = entry.clone();
        self.persist(&snapshot);
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
        Some(out)
    }

    /// Patch name / type / expiry (PATCH parity beyond `enabled`).
    pub fn update_fields(&self, id: &str, patch: &serde_json::Value) -> Option<ApiKeyEntry> {
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        let entry = snapshot.iter_mut().find(|x| x.id == id)?;
        if let Some(n) = patch.get("name").and_then(|v| v.as_str()) {
            if !n.trim().is_empty() {
                entry.name = n.to_string();
            }
        }
        if let Some(t) = patch.get("type").and_then(|v| v.as_str()) {
            entry.key_type = t.to_string();
            if t == "admin" {
                entry.role = "admin".into();
            }
        }
        if let Some(e) = patch.get("expiresAtMs") {
            entry.expires_at_ms = e.as_u64().map(|v| v as u128);
        }
        let out = entry.clone();
        self.persist(&snapshot);
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
        Some(out)
    }

    pub fn touch(&self, key: &str) {
        let mut snapshot = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(e) = snapshot.iter_mut().find(|x| x.key == key) {
            e.last_used_at_ms = Some(now_ms());
            e.total_requests += 1;
            self.persist(&snapshot);
            *self.keys.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
        }
    }

    pub fn matches_enabled(&self, key: &str) -> bool {
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|x| x.enabled && x.key == key)
    }

    pub fn by_key(&self, key: &str) -> Option<ApiKeyEntry> {
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|x| x.key == key)
            .cloned()
    }
}

fn usage_limit_enabled_marker(daily: Option<f64>, weekly: Option<f64>) -> bool {
    daily.is_some() || weekly.is_some()
}
