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
    // The record holds a password hash: write it without a world-readable
    // window (a plain `write` + `chmod` leaves one).
    crate::write_secret_file(&path, &serde_json::to_string_pretty(&rec).unwrap_or_default())?;
    Ok(rec)
}

pub struct AuthStore {
    pub record: RwLock<DashboardAuth>,
    /// session token → expiry ms
    pub sessions: dashmap::DashMap<String, u128>,
    /// true when dashboard login is required (always, post-bootstrap)
    pub login_enabled: bool,
    record_data_dir: Option<std::path::PathBuf>,
    /// per-IP login attempt counter; None means unlimited (tests).
    login_attempts: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, u128)>>,
}

impl AuthStore {
    /// Bootstrap: `OMNIROUTE_ADMIN_PASSWORD` env wins; otherwise the default
    /// password is "CHANGEME" until changed. The hash persists to
    /// `$DATA_DIR/dashboard-auth.json`.
    pub fn new(data_dir: &std::path::Path, admin_password_env: Option<String>) -> Self {
        let path = data_dir.join("dashboard-auth.json");
        let _ = std::fs::create_dir_all(data_dir);
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
                // First deployment: the documented default "CHANGEME".
                //
                // A random password was tried here and reverted: it was
                // printed once to stderr, so an operator who missed the log
                // line was locked out permanently with no way to recover. The
                // default is guessable, but it is *recoverable* — the
                // dashboard prompts for a change, `OMNIROUTE_ADMIN_PASSWORD`
                // overrides it at boot, and the login rate limiter bounds
                // guessing.
                using_default = true;
                hashed(CHANGEME)
            }
        };
        // The record holds the admin password hash — same rule as the other
        // credential stores: never world-readable, not even briefly.
        let _ = crate::write_secret_file(
            &path,
            &serde_json::to_string_pretty(&record).unwrap_or_default(),
        );
        if using_default {
            eprintln!(
                "[DASHBOARD] default admin password is \"CHANGEME\" — change it at /dashboard (Settings) or via OMNIROUTE_ADMIN_PASSWORD + restart"
            );
        }
        Self {
            record: RwLock::new(record),
            sessions: dashmap::DashMap::new(),
            login_enabled: true,
            record_data_dir: Some(data_dir.to_path_buf()),
            login_attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
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
            let _ = crate::write_secret_file(
                &dir.join("dashboard-auth.json"),
                &serde_json::to_string_pretty(&rec).unwrap_or_default(),
            );
        }
        *self.record.write().unwrap_or_else(|e| e.into_inner()) = rec;
    }

    /// Login → session token valid for 7 days.
    /// Login attempt allowed? Rate-limited per IP: 5 failures within 15
    /// minutes locks the IP out for 15 minutes. A successful login resets
    /// the counter, so a legitimate user who fat-fingers the password a few
    /// times is not locked out.
    pub fn login_allowed(&self, ip: std::net::IpAddr, now_ms: u128) -> bool {
        const WINDOW_MS: u128 = 15 * 60 * 1000;
        const MAX_FAILS: u32 = 5;
        const LOCK_MS: u128 = 15 * 60 * 1000;
        let mut map = self.login_attempts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(ip).or_insert((0, now_ms));
        if now_ms.saturating_sub(entry.1) > WINDOW_MS {
            *entry = (0, now_ms);
        }
        if entry.0 >= MAX_FAILS {
            if now_ms.saturating_sub(entry.1) < LOCK_MS {
                return false;
            }
            *entry = (0, now_ms);
        }
        true
    }

    /// Record a failed login attempt for `ip`.
    pub fn record_failed_login(&self, ip: std::net::IpAddr, now_ms: u128) {
        let mut map = self.login_attempts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(ip).or_insert((0, now_ms));
        entry.0 = entry.0.saturating_add(1);
        if entry.0 == 1 {
            entry.1 = now_ms;
        }
    }

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

/// The free lane answers to two provider names for the same upstream and
/// catalogue (`opencode` and `opencode-zen`). An allow entry may name either
/// spelling, so matching compares the canonical one — otherwise a key could
/// list a model, see it in the picker, and still be refused at request time.
fn canonical_lane(model: &str) -> String {
    match model.strip_prefix("opencode-zen/") {
        Some(rest) => format!("{}/{rest}", crate::opencode_free::CANONICAL_LANE_PROVIDER),
        None => model.to_string(),
    }
}

impl ApiKeyStore {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let path = data_dir.join("api-keys.json");
        let mut keys = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<ApiKeyEntry>>(&t).ok())
            .unwrap_or_default();
        // Older keys predate model scoping: a missing or empty mode means "all"
        // (the historical behaviour), not "restrict to nothing". Only an
        // unusable mode is corrected; a stale allow list next to `all` is
        // reported and left alone rather than silently deleted from disk.
        let mut normalized = false;
        for entry in keys.iter_mut() {
            if entry.model_access_mode != "all" && entry.model_access_mode != "restricted" {
                tracing::warn!(
                    key_id = %entry.id,
                    mode = %entry.model_access_mode,
                    "api key had an unknown model_access_mode; normalizing to \"all\""
                );
                entry.model_access_mode = "all".to_string();
                normalized = true;
            }
            if entry.model_access_mode == "all"
                && (!entry.allowed_models.is_empty() || !entry.allowed_combos.is_empty())
            {
                tracing::warn!(
                    key_id = %entry.id,
                    models = entry.allowed_models.len(),
                    combos = entry.allowed_combos.len(),
                    "api key is unrestricted but carries allow lists; keeping them (inactive)"
                );
            }
        }
        if normalized {
            Self::write_normalized(&path, &keys);
        }
        Self { keys: RwLock::new(keys), path }
    }

    fn persist(&self, keys: &[ApiKeyEntry]) {
        if let Err(e) = crate::write_secret_file(
            &self.path,
            &serde_json::to_string_pretty(keys).unwrap_or_default(),
        ) {
            tracing::warn!(path = %self.path.display(), error = %e,
                "failed to persist api keys; memory and disk may diverge");
        }
    }

    /// One-time normalization of pre-scoping key files (see `new`).
    fn write_normalized(path: &std::path::Path, keys: &[ApiKeyEntry]) {
        if let Err(e) = crate::write_secret_file(
            path,
            &serde_json::to_string_pretty(keys).unwrap_or_default(),
        ) {
            tracing::warn!(path = %path.display(), error = %e,
                "failed to rewrite normalized api keys; disk still carries the old shape");
        }
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

    /// Return one key's full secret. Unlike `list()` (which the dashboard masks),
    /// this is the explicit "reveal" read, so the caller can show it once the
    /// operator unlocks the row.
    pub fn reveal(&self, id: &str) -> Option<ApiKeyEntry> {
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|x| x.id == id)
            .cloned()
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
            // Keep `type` and `role` in step in both directions, so a scoping
            // PATCH that flips the type cannot leave an admin role behind (an
            // admin bypasses the allow list entirely — see model_access_entry).
            if t == "admin" {
                entry.role = "admin".into();
            } else if entry.role == "admin" {
                entry.role = "default".into();
            }
        }
        if let Some(e) = patch.get("expiresAtMs") {
            entry.expires_at_ms = e.as_u64().map(|v| v as u128);
        }
        // Model access scoping: `all` (default) or `restricted` with an allow
        // list. Absent keys leave the current values untouched, so an
        // enable-only PATCH stays compatible.
        if let Some(mode) = patch.get("model_access_mode").and_then(|v| v.as_str()) {
            let mode = mode.trim();
            if mode == "all" || mode == "restricted" {
                entry.model_access_mode = mode.to_string();
                if mode == "all" {
                    entry.allowed_models.clear();
                    entry.allowed_combos.clear();
                }
            }
        }
        if let Some(models) = patch.get("allowed_models").and_then(|v| v.as_array()) {
            entry.allowed_models = models
                .iter()
                .filter_map(|v| v.as_str())
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            entry.allowed_models.sort();
            entry.allowed_models.dedup();
        }
        if let Some(combos) = patch.get("allowed_combos").and_then(|v| v.as_array()) {
            entry.allowed_combos = combos
                .iter()
                .filter_map(|v| v.as_str())
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            entry.allowed_combos.sort();
            entry.allowed_combos.dedup();
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

    /// Does this key admit a request for `model` (a bare id or
    /// `provider/model`)? `all` mode admits everything; `restricted` matches the
    /// allow lists: a bare allow-list entry matches any provider that carries
    /// the id, a `provider/model` entry must match exactly, and combo names are
    /// checked separately against `allowed_combos`.
    pub fn admits_model(&self, entry: &ApiKeyEntry, model: &str, is_combo: bool) -> bool {
        // Fail closed: an unknown or empty mode must not read as "all".
        // (`all` and `restricted` are the only values create/update produce,
        // but hand-edited or older files can carry anything.) Admins bypass
        // every list; a restricted default key falls through to the lists.
        match entry.model_access_mode.as_str() {
            "all" => return true,
            "restricted" => {
                if entry.role == "admin" {
                    return true;
                }
            }
            _ => return entry.role == "admin",
        }
        if is_combo {
            // Exact combo name (case-insensitive to match the model branch).
            // No suffix matching: `free` must not silently grant `auto/free`
            // and every other combo ending in "free".
            let requested = model.to_ascii_lowercase();
            return entry
                .allowed_combos
                .iter()
                .any(|allowed| allowed.to_ascii_lowercase() == requested);
        }
        let requested = canonical_lane(&model.to_ascii_lowercase());
        entry
            .allowed_models
            .iter()
            .any(|allowed| {
                let allowed = canonical_lane(&allowed.to_ascii_lowercase());
                allowed == requested
                    || requested
                        .strip_suffix(&format!("/{allowed}"))
                        .map(|provider| !provider.is_empty())
                        .unwrap_or(false)
            })
    }

    /// Does this key admit *any* model of `provider`? Used by the provider-only
    /// passthrough paths (`x-omniroute-provider` header, audio/files): the
    /// header grants the whole provider, so an allow entry qualifies when its
    /// provider half matches — either a `provider/…` entry naming it exactly or
    /// a bare entry (which matches every provider carrying that id, and here
    /// that provider carries nothing yet, so it must not veto).
    pub fn admits_provider(&self, entry: &ApiKeyEntry, provider: &str) -> bool {
        if entry.role == "admin" || entry.model_access_mode == "all" {
            return true;
        }
        if entry.model_access_mode != "restricted" {
            return false;
        }
        entry
            .allowed_models
            .iter()
            .any(|allowed| allowed.split('/').next() == Some(provider))
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

#[cfg(test)]
mod auth_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn free_lane_spellings_are_interchangeable_in_an_allow_list() {
        // One upstream answers to two provider names. Naming the model under
        // either spelling (or bare) must grant the same access, on the request
        // path and in the picker alike.
        for spelling in [
            "opencode/beam-free",
            "opencode-zen/beam-free",
            "beam-free",
        ] {
            let dir = tempdir().unwrap();
            let store = ApiKeyStore::new(dir.path());
            let created = store.create(
                "lane",
                "default",
                Some("restricted"),
                vec![spelling.to_string()],
                vec![],
                false,
                false,
                false,
                None,
                None,
                false,
            );
            for requested in ["opencode/beam-free", "opencode-zen/beam-free"] {
                assert!(
                    store.admits_model(&created, requested, false),
                    "allow {spelling:?} must admit {requested:?}"
                );
            }
            // A different model of the same upstream stays refused.
            assert!(!store.admits_model(&created, "opencode/big-pickle", false));
        }
    }

    #[test]
    fn restricted_key_admits_only_its_allow_lists() {
        let dir = tempdir().unwrap();
        let store = ApiKeyStore::new(dir.path());
        let created = store.create(
            "scoped",
            "default",
            Some("restricted"),
            vec!["deepseek-v4.1-flash".into(), "openrouter/deepseek/deepseek-v4.1-flash".into()],
            vec!["coding-flash".into()],
            false,
            false,
            false,
            None,
            None,
            false,
        );

        // Bare allow-list entry matches any provider carrying the id…
        assert!(store.admits_model(&created, "opencode-go/deepseek-v4.1-flash", false));
        assert!(store.admits_model(&created, "deepseek-v4.1-flash", false));
        // …and an exact provider/model entry too.
        assert!(store.admits_model(&created, "openrouter/deepseek/deepseek-v4.1-flash", false));
        // A combo is gated by the combo list only.
        assert!(store.admits_model(&created, "coding-flash", true));
        assert!(!store.admits_model(&created, "coding-free", true));
        // Anything else is refused.
        assert!(!store.admits_model(&created, "opencode-zen/beam-free", false));
        assert!(!store.admits_model(&created, "kimi-k2.7-code", false));
    }

    #[test]
    fn all_mode_key_admits_everything() {
        let dir = tempdir().unwrap();
        let store = ApiKeyStore::new(dir.path());
        let created = store.create(
            "open",
            "default",
            Some("all"),
            vec![],
            vec![],
            false,
            false,
            false,
            None,
            None,
            false,
        );
        assert!(store.admits_model(&created, "anything/any-model", false));
        assert!(store.admits_model(&created, "any-combo", true));
    }

    #[test]
    fn loading_normalizes_pre_scoping_keys_to_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("api-keys.json");
        std::fs::write(
            &path,
            r#"[{"id":"legacy","name":"legacy","key":"sk-legacy","role":"default","enabled":true}]"#,
        )
        .unwrap();
        let store = ApiKeyStore::new(dir.path());
        let entry = store.by_key("sk-legacy").expect("legacy entry");
        assert_eq!(entry.model_access_mode, "all", "missing mode means all");
        assert!(store.admits_model(&entry, "any-model", false));
    }

    #[test]
    fn restricted_mode_survives_a_round_trip() {
        let dir = tempdir().unwrap();
        let store = ApiKeyStore::new(dir.path());
        let created = store.create(
            "scoped2",
            "default",
            Some("restricted"),
            vec!["m1".into()],
            vec![],
            false,
            false,
            false,
            None,
            None,
            false,
        );
        let reloaded = ApiKeyStore::new(dir.path());
        let entry = reloaded.by_key(&created.key).expect("persisted entry");
        assert_eq!(entry.model_access_mode, "restricted");
        assert!(!reloaded.admits_model(&entry, "not-allowed", false));
    }

    #[test]
    fn test_login_with_default_password() {
        let dir = tempdir().unwrap();
        let auth = AuthStore::new(dir.path(), None);
        assert!(auth.is_default_password(), "a fresh deployment starts on the default");
        assert!(auth.login("CHANGEME").is_some(), "Login should succeed with CHANGEME");
        // and the rate limiter bounds how fast that default can be guessed
        let ip = "198.51.100.1".parse().unwrap();
        for _ in 0..6 {
            auth.record_failed_login(ip, 1_000_000);
        }
        assert!(
            !auth.login_allowed(ip, 1_000_001),
            "six failed attempts must lock the address out"
        );
        assert!(
            auth.login_allowed("198.51.100.2".parse().unwrap(), 1_000_001),
            "the lock is per address"
        );
    }

    #[test]
    fn test_login_with_custom_password() {
        let dir = tempdir().unwrap();
        let auth = AuthStore::new(dir.path(), Some("custom123".into()));
        let result = auth.login("custom123");
        assert!(result.is_some(), "Login should succeed with custom password");
    }

    #[test]
    fn test_login_wrong_password() {
        let dir = tempdir().unwrap();
        let auth = AuthStore::new(dir.path(), None);
        let result = auth.login("wrong");
        assert!(result.is_none(), "Login should fail with wrong password");
    }
}
