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
        let _ = std::fs::write(
            &self.path,
            serde_json::to_string_pretty(connections).unwrap_or_default(),
        );
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
