//! Provider connection management (parity: the original `/api/providers`
//! CRUD + provider nodes — api-key connections persisted in
//! `$DATA_DIR/provider-connections.json` and registered into the live
//! registry at runtime).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConnection {
    #[serde(default)]
    pub id: String,
    /// provider: registry id (anthropic/openai/...) or compatible family id
    /// (`openai-compatible-<name>` / `anthropic-compatible-<name>`)
    pub provider: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "apiKey")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "apiType")]
    pub api_type: Option<String>,
    #[serde(default, alias = "models")]
    pub model_list: Vec<String>,
    /// models discovered from the upstream `/models` listing (see sync-models)
    #[serde(default)]
    pub synced_models: Vec<String>,
    /// Capability metadata discovered alongside `synced_models`.
    #[serde(default)]
    pub model_metadata: std::collections::HashMap<String, crate::registry::ModelMetadata>,
    /// epoch ms of the last successful sync (0 = never)
    #[serde(default)]
    pub synced_at_ms: u128,
    /// models hidden from `/v1/models`, the catalog and routing candidates
    #[serde(default)]
    pub hidden_models: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at_ms: u128,
}

fn default_enabled() -> bool {
    true
}

pub struct ProviderConnectionStore {
    connections: std::sync::RwLock<Vec<ProviderConnection>>,
    path: std::path::PathBuf,
}

impl ProviderConnectionStore {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let path = data_dir.join("provider-connections.json");
        let connections = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<ProviderConnection>>(&t).ok())
            .unwrap_or_default();
        Self {
            connections: std::sync::RwLock::new(connections),
            path,
        }
    }

    fn persist(&self, connections: &[ProviderConnection]) {
        // These files hold upstream API keys in plaintext, so they must not
        // be world-readable. The default umask created them 0644 — on a
        // shared host that is every credential on the box. Write to a
        // temporary file with 0600 and rename over the target, which also
        // makes the update atomic.
        let payload = serde_json::to_string_pretty(connections).unwrap_or_default();
        let tmp = self.path.with_extension("tmp");
        let written = {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&tmp) {
                Ok(mut f) => f.write_all(payload.as_bytes()).is_ok(),
                Err(_) => false,
            }
        };
        if written {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&tmp, &self.path);
        } else {
            let _ = std::fs::write(&self.path, payload);
        }
    }

    pub fn list(&self, mask: bool) -> Vec<ProviderConnection> {
        let snapshot = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        snapshot
            .into_iter()
            .map(|mut x| {
                if mask && x.api_key.is_some() {
                    x.api_key = Some(mask_key(x.api_key.as_deref().unwrap_or("")));
                }
                x
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<ProviderConnection> {
        let snapshot = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        snapshot.into_iter().find(|x| x.id == id)
    }

    pub fn upsert(&self, conn: ProviderConnection) {
        let mut snapshot = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(pos) = snapshot.iter().position(|x| x.id == conn.id) {
            snapshot[pos] = conn;
        } else {
            snapshot.push(conn);
        }
        self.persist(&snapshot);
        *self.connections.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut snapshot = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let before = snapshot.len();
        snapshot.retain(|x| x.id != id);
        self.persist(&snapshot);
        *self.connections.write().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        snapshot.len() != before
    }

    pub fn all_unmasked(&self) -> Vec<ProviderConnection> {
        let snapshot = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        snapshot
    }
}

pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        "••••".to_string()
    } else {
        format!(
            "{}••••{}",
            chars[..4].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    }
}

#[cfg(test)]
mod secret_file_permission_tests {
    #[cfg(unix)]
    #[test]
    fn credentials_are_written_without_world_read() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("omni-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let store = super::ProviderConnectionStore::new(&dir);
        store.upsert(super::ProviderConnection {
            id: "c".into(),
            provider: "openai".into(),
            name: "OpenAI".into(),
            api_key: Some("sk-secret".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: Default::default(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        });

        let mode = std::fs::metadata(dir.join("provider-connections.json"))
            .unwrap()
            .permissions()
            .mode();
        let world = mode & 0o077;
        assert_eq!(
            world, 0,
            "credentials file must not be group/world readable (mode {:o})",
            mode
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
