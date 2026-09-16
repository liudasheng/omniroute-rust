//! Dashboard auth + API-key store (parity-lite of the original dashboard
//! JWT/session and `/api/keys` management).
//!
//! Bootstrap: `$DATA_DIR/dashboard-auth.json` stores
//! `{salt, password_sha256, created}`; on first boot a random admin password
//! is generated and written to `$DATA_DIR/dashboard-password.txt` (mode 600),
//! also logged. `OMNIROUTE_ADMIN_PASSWORD` pins/overrides the hash at boot.
//! Sessions are in-memory bearer tokens (7-day TTL).

use sha2::{Digest, Sha256};
use std::sync::RwLock;

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
    pub role: String, // "default" | "admin"
    pub enabled: bool,
    pub created_at_ms: u128,
    #[serde(default)]
    pub last_used_at_ms: Option<u128>,
    #[serde(default)]
    pub total_requests: u64,
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

pub struct AuthStore {
    pub record: RwLock<DashboardAuth>,
    /// true when dashboard login is a real credential source (bootstrap file
    /// exists or OMNIROUTE_ADMIN_PASSWORD is set)
    pub login_enabled: bool,
    /// session token → expiry ms
    pub sessions: dashmap::DashMap<String, u128>,
}

impl AuthStore {
    /// Bootstrap auth: dashboard-auth.json first; OMNIROUTE_ADMIN_PASSWORD env
    /// wins; one-time random password generated + persisted otherwise.
    pub fn new(data_dir: &std::path::Path, admin_password_env: Option<String>) -> Self {
        let path = data_dir.join("dashboard-auth.json");
        let pw_path = data_dir.join("dashboard-password.txt");
        let mut bootstrap_generated = false;
        let existing = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<DashboardAuth>(&t).ok());

        let login_enabled_before = existing.is_some()
            || admin_password_env.as_ref().is_some_and(|p| !p.is_empty());
        let record = match admin_password_env.filter(|p| !p.is_empty()) {
            Some(pw) => Some(hashed(&pw)),
            None => existing,
        };
        let record = match record {
            Some(r) => r,
            None => {
                // first boot: generate a readable admin password
                bootstrap_generated = true;
                let pw = format!("om-{}", random_hex(12));
                let rec = hashed(&pw);
                let _ = std::fs::write(&pw_path, format!("{pw}\n"));
                let _ = crate::set_file_mode_600(&pw_path);
                println!("[DASHBOARD] initial admin password written: {}", pw_path.display());
                rec
            }
        };
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap_or_default());
        let login_enabled = login_enabled_before || bootstrap_generated;
        Self { record: RwLock::new(record), sessions: dashmap::DashMap::new(), login_enabled }
    }

    pub fn verify(&self, password: &str) -> bool {
        let rec = self
            .record
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        rec.password_sha256 == hash_with_salt(password, &rec.salt)
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
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Create a key; returns the entry with its full secret (shown once).
    pub fn create(&self, name: &str, role: &str) -> ApiKeyEntry {
        let entry = ApiKeyEntry {
            id: format!("key_{}", random_hex(8)),
            name: if name.is_empty() { "default".into() } else { name.to_string() },
            key: format!("sk-or-{}", random_hex(32)),
            role: if role == "admin" { "admin".into() } else { "default".into() },
            enabled: true,
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
