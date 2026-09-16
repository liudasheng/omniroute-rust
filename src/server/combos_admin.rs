//! Runtime combo definitions + quota overrides (dashboard "Combos" and
//! "Provider quota" pages).
//!
//! Combos configured in `omniroute.toml` stay authoritative; entries created in
//! the dashboard are persisted to `$DATA_DIR/combos.json` and merged over them
//! (same pattern as provider connections). Quota overrides live in
//! `$DATA_DIR/quotas.json`: per-provider rpm/concurrency plus the optional
//! balance + cutoff the quota page displays.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// One dashboard-managed combo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedCombo {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// priority | round-robin | fill-first | weighted | random | least-used |
    /// p2c | cost-optimized | lkgp | auto
    #[serde(default)]
    pub strategy: Option<String>,
    /// `provider` or `provider/model` entries, in failover order
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub default_model: Option<String>,
    /// context-handoff / thinking badges shown on the row
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created_at_ms: u128,
}

fn yes() -> bool {
    true
}

impl ManagedCombo {
    /// Deterministic strategies keep a fixed order; the rest are smart routers.
    pub fn is_deterministic(&self) -> bool {
        matches!(
            self.strategy.as_deref().unwrap_or("priority"),
            "priority" | "failover" | "round-robin" | "fill-first" | "weighted"
        )
    }
}

pub struct ComboStore {
    combos: RwLock<Vec<ManagedCombo>>,
    path: PathBuf,
}

impl ComboStore {
    pub fn new(data_dir: &Path) -> Self {
        let path = data_dir.join("combos.json");
        let combos = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<ManagedCombo>>(&t).ok())
            .unwrap_or_default();
        Self { combos: RwLock::new(combos), path }
    }

    fn persist(&self, list: &[ManagedCombo]) {
        let _ = std::fs::write(&self.path, serde_json::to_string_pretty(list).unwrap_or_default());
    }

    pub fn list(&self) -> Vec<ManagedCombo> {
        self.combos.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn get(&self, id: &str) -> Option<ManagedCombo> {
        self.list().into_iter().find(|c| c.id == id || c.name == id)
    }

    pub fn upsert(&self, mut combo: ManagedCombo) -> ManagedCombo {
        let mut list = self.list();
        if combo.id.is_empty() {
            combo.id = format!("combo_{}", crate::server::security::random_id(8));
        }
        if combo.created_at_ms == 0 {
            combo.created_at_ms = crate::server::security::now_ms();
        }
        match list.iter_mut().find(|c| c.id == combo.id) {
            Some(slot) => *slot = combo.clone(),
            None => list.push(combo.clone()),
        }
        self.persist(&list);
        *self.combos.write().unwrap_or_else(|e| e.into_inner()) = list;
        combo
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut list = self.list();
        let before = list.len();
        list.retain(|c| c.id != id && c.name != id);
        let removed = list.len() != before;
        if removed {
            self.persist(&list);
            *self.combos.write().unwrap_or_else(|e| e.into_inner()) = list;
        }
        removed
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut list = self.list();
        let found = match list.iter_mut().find(|c| c.id == id || c.name == id) {
            Some(c) => {
                c.enabled = enabled;
                true
            }
            None => false,
        };
        if found {
            self.persist(&list);
            *self.combos.write().unwrap_or_else(|e| e.into_inner()) = list;
        }
        found
    }
}

/// Per-provider quota override shown on the quota page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaOverride {
    pub provider: String,
    #[serde(default)]
    pub label: Option<String>,
    /// account tier shown as a chip: 未知 | 免费 | 付费
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub auth_kind: Option<String>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    /// request/token ceiling used to derive the severity
    #[serde(default)]
    pub cutoff: Option<f64>,
    #[serde(default)]
    pub rpm: Option<u64>,
    #[serde(default)]
    pub concurrent: Option<u64>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub updated_at_ms: u128,
}

pub struct QuotaStore {
    overrides: RwLock<Vec<QuotaOverride>>,
    path: PathBuf,
}

impl QuotaStore {
    pub fn new(data_dir: &Path) -> Self {
        let path = data_dir.join("quotas.json");
        let overrides = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<QuotaOverride>>(&t).ok())
            .unwrap_or_default();
        Self { overrides: RwLock::new(overrides), path }
    }

    fn persist(&self, list: &[QuotaOverride]) {
        let _ = std::fs::write(&self.path, serde_json::to_string_pretty(list).unwrap_or_default());
    }

    pub fn list(&self) -> Vec<QuotaOverride> {
        self.overrides.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn upsert(&self, mut o: QuotaOverride) -> QuotaOverride {
        let mut list = self.list();
        o.updated_at_ms = crate::server::security::now_ms();
        match list.iter_mut().find(|x| x.provider == o.provider) {
            Some(slot) => *slot = o.clone(),
            None => list.push(o.clone()),
        }
        self.persist(&list);
        *self.overrides.write().unwrap_or_else(|e| e.into_inner()) = list;
        o
    }

    /// Severity from the live window usage vs the configured cutoff.
    pub fn severity(window_hits: u64, cutoff: Option<f64>) -> &'static str {
        match cutoff {
            Some(c) if c > 0.0 => {
                let ratio = window_hits as f64 / c;
                if ratio >= 0.9 {
                    "critical"
                } else if ratio >= 0.7 {
                    "warning"
                } else {
                    "healthy"
                }
            }
            _ => "unknown",
        }
    }
}
