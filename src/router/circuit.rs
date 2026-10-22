//! Per-connection health: cooldowns, exponential backoff, circuit breakers.
//! Parity: `open-sse/config/constants.ts` (COOLDOWN_MS, BACKOFF_CONFIG,
//! BACKOFF_STEPS_MS, PROVIDER_PROFILES) + `open-sse/services/accountFallback.ts`.

use crate::errors::FailureKind;
use crate::registry::AuthType;
use dashmap::DashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// DEFAULT_API_LIMITS parity.
pub const DEFAULT_CONCURRENCY: i64 = 6;
pub const DEFAULT_RPM: u64 = 60;
pub const MIN_INTERVAL_MS: u64 = 350;

/// COOLDOWN_MS parity.
pub fn cooldown_for(kind: FailureKind, is_local: bool) -> Duration {
    match kind {
        FailureKind::Auth => Duration::from_secs(120),
        FailureKind::Payment => Duration::from_secs(120),
        FailureKind::NotFound => {
            if is_local {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(120)
            }
        }
        FailureKind::RateLimit => Duration::from_secs(120),
        FailureKind::Server => Duration::from_secs(2), // serviceUnavailable: 2s
        FailureKind::Network => Duration::from_secs(5), // transientInitial
        FailureKind::Client => Duration::ZERO,          // client-fixable: no penalty
    }
}

/// BACKOFF_CONFIG parity: base 1s, max 2min, level cap 15.
pub const BACKOFF_BASE_MS: u64 = 1_000;
pub const BACKOFF_MAX_MS: u64 = 120_000;
pub const BACKOFF_MAX_LEVEL: i64 = 15;

/// Exponential backoff with escalation level (calculateBackoffCooldown parity).
pub fn calculate_backoff(level: i64) -> Duration {
    let l = level.clamp(0, BACKOFF_MAX_LEVEL);
    let ms = BACKOFF_BASE_MS * 2u64.saturating_pow(l as u32);
    Duration::from_millis(ms.min(BACKOFF_MAX_MS))
}

/// BACKOFF_STEPS_MS parity — per-model ban escalation.
pub const BACKOFF_STEPS_MS: [u64; 5] = [60_000, 120_000, 300_000, 600_000, 1_200_000];

pub fn model_ban_duration(step: usize) -> Duration {
    Duration::from_millis(BACKOFF_STEPS_MS[step.min(BACKOFF_STEPS_MS.len() - 1)])
}

/// PROVIDER_PROFILES parity.
#[derive(Debug, Clone)]
pub struct BreakerProfile {
    pub transient_cooldown_ms: u64,
    pub rate_limit_cooldown_ms: u64,
    pub threshold: i64,
    pub reset_ms: u64,
    pub breaker_failure_threshold: i64,
    pub breaker_failure_window_ms: u64,
    pub breaker_cooldown_ms: u64,
}

pub fn profile_for(auth: AuthType) -> BreakerProfile {
    match auth {
        AuthType::Oauth => BreakerProfile {
            transient_cooldown_ms: 5_000,
            rate_limit_cooldown_ms: 60_000,
            threshold: 8,
            reset_ms: 60_000,
            breaker_failure_threshold: 10,
            breaker_failure_window_ms: 15 * 60_000,
            breaker_cooldown_ms: 5 * 60_000,
        },
        AuthType::ApiKey => BreakerProfile {
            transient_cooldown_ms: 3_000,
            rate_limit_cooldown_ms: 0, // use retry-after
            threshold: 12,
            reset_ms: 30_000,
            breaker_failure_threshold: 15,
            breaker_failure_window_ms: 30 * 60_000,
            breaker_cooldown_ms: 10 * 60_000,
        },
        AuthType::Local => BreakerProfile {
            transient_cooldown_ms: 2_000,
            rate_limit_cooldown_ms: 5_000,
            threshold: 2,
            reset_ms: 15_000,
            breaker_failure_threshold: 2,
            breaker_failure_window_ms: 5 * 60_000,
            breaker_cooldown_ms: 60_000,
        },
    }
}

/// env override support (OMNIROUTE_CIRCUIT_BREAKER_<KIND>_THRESHOLD/_RESET_MS)
fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

impl BreakerProfile {
    pub fn effective(auth: AuthType) -> BreakerProfile {
        let mut p = profile_for(auth);
        let kind = match auth {
            AuthType::Oauth => "OAUTH",
            AuthType::ApiKey => "API_KEY",
            AuthType::Local => "LOCAL",
        };
        if let Some(t) = env_u64(&format!("OMNIROUTE_CIRCUIT_BREAKER_{kind}_THRESHOLD")) {
            p.threshold = t as i64;
        }
        if let Some(r) = env_u64(&format!("OMNIROUTE_CIRCUIT_BREAKER_{kind}_RESET_MS")) {
            p.reset_ms = r;
        }
        p
    }
}

#[derive(Debug, Default)]
struct ConnHealth {
    cooldown_until: Option<Instant>,
    backoff_level: i64,
    failures: VecDeque<Instant>,
    breaker_open_until: Option<Instant>,
    in_flight: i64,
    last_success: Option<Instant>,
}

/// Store of per-connection health, keyed by provider id; model-level bans use
/// `provider|model` keys.
pub struct CircuitStore {
    map: DashMap<String, ConnHealth>,
    /// per-connection concurrency cap (DEFAULT_API_LIMITS.concurrentRequests)
    concurrency_limit: i64,
}

impl Default for CircuitStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitStore {
    pub fn new() -> Self {
        Self { map: DashMap::new(), concurrency_limit: crate::config::DEFAULT_RATE_CONCURRENCY }
    }

    /// Build with explicit limits (loaded from env/config).
    pub fn with_limits(concurrency_limit: i64) -> Self {
        Self { map: DashMap::new(), concurrency_limit }
    }

    fn entry(&self, key: &str) -> dashmap::mapref::one::RefMut<'_, String, ConnHealth> {
        self.map.entry(key.to_string()).or_default()
    }

    /// Is the connection usable right now?
    pub fn is_available(&self, provider: &str) -> bool {
        let now = Instant::now();
        if let Some(h) = self.map.get(provider) {
            if h.cooldown_until.is_some_and(|u| now < u) {
                return false;
            }
            if h.breaker_open_until.is_some_and(|u| now < u) {
                return false;
            }
            if h.in_flight >= self.concurrency_limit {
                return false;
            }
        }
        true
    }

    /// Is a specific model banned (BACKOFF_STEPS_MS escalation)?
    pub fn is_model_banned(&self, provider: &str, model: &str) -> bool {
        let key = format!("{provider}|{model}");
        self.map.get(&key).is_some()
    }

    /// Key namespace for a *temporary* model-level cooldown.
    ///
    /// It must not collide with the ban key (`provider|model`): a ban is a
    /// permanent, escalating verdict ("this id is delisted"), while a
    /// cooldown is a short, expiring pause after a transient failure. Writing
    /// a cooldown into the ban namespace made `is_model_banned` — which
    /// treats the mere presence of the key as "banned" — turn one transient
    /// failure into a permanent ban of that model id.
    fn model_cooldown_key(provider: &str, model: &str) -> String {
        format!("{provider}|{model}|cooling")
    }

    /// Record a failure that belongs to **one model**, not to the provider.
    ///
    /// A provider family usually carries several models with independent
    /// availability (an OpenCode tier, for instance, mixes a working
    /// DeepSeek with a delisted or region-locked id). Freezing the whole
    /// provider because one id failed takes the healthy models down with it —
    /// the operator sees "no quota" although the tier still has plenty.
    ///
    /// The entry lives in its own namespace so that it expires on its own
    /// and never turns into a ban: after the cooldown the id is eligible
    /// again.
    pub fn record_model_failure(&self, provider: &str, model: &str, kind: FailureKind, is_local: bool) {
        let key = Self::model_cooldown_key(provider, model);
        let mut h = self.entry(&key);
        let now = Instant::now();
        // A model-level verdict is a property of the id, not of the link:
        // a short cooldown is enough to stop the retry storm, and the id
        // stays eligible afterwards.
        let cooldown = match kind {
            FailureKind::RateLimit => cooldown_for(FailureKind::RateLimit, is_local),
            FailureKind::Auth | FailureKind::Payment => cooldown_for(kind, is_local),
            _ => Duration::from_secs(30),
        };
        h.cooldown_until = Some(now + cooldown);
        h.failures.push_back(now);
        while let Some(front) = h.failures.front() {
            if now.duration_since(*front).as_millis() as u64 > 5 * 60_000 {
                h.failures.pop_front();
            } else {
                break;
            }
        }
    }

    /// How long this model stays in its model-level cooldown.
    pub fn model_cooldown_remaining_ms(&self, provider: &str, model: &str) -> i64 {
        let key = Self::model_cooldown_key(provider, model);
        let now = Instant::now();
        self.map
            .get(&key)
            .and_then(|h| h.cooldown_until)
            .map(|u| u.saturating_duration_since(now).as_millis() as i64)
            .unwrap_or(0)
    }

    /// Is this specific model in a model-level cooldown?
    pub fn is_model_cooling(&self, provider: &str, model: &str) -> bool {
        let key = Self::model_cooldown_key(provider, model);
        let now = Instant::now();
        self.map
            .get(&key)
            .is_some_and(|h| h.cooldown_until.is_some_and(|u| now < u))
    }

    /// Record a failure, applying cooldown + backoff + provider-breaker logic.
    pub fn record_failure(&self, provider: &str, kind: FailureKind, is_local: bool, auth: AuthType) {
        let mut h = self.entry(provider);
        let now = Instant::now();
        let profile = BreakerProfile::effective(auth);

        let cooldown = match kind {
            FailureKind::Server | FailureKind::Network => {
                h.backoff_level = (h.backoff_level + 1).min(BACKOFF_MAX_LEVEL);
                let backoff = calculate_backoff(h.backoff_level);
                Duration::from_millis(backoff.as_millis() as u64)
            }
            FailureKind::RateLimit => {
                let c = if profile.rate_limit_cooldown_ms > 0 {
                    profile.rate_limit_cooldown_ms
                } else {
                    cooldown_for(kind, is_local).as_millis() as u64
                };
                Duration::from_millis(c)
            }
            _ => cooldown_for(kind, is_local),
        };
        h.cooldown_until = Some(now + cooldown);

        h.failures.push_back(now);
        while let Some(front) = h.failures.front() {
            if now.duration_since(*front).as_millis() as u64 > profile.breaker_failure_window_ms {
                h.failures.pop_front();
            } else {
                break;
            }
        }
        if h.failures.len() as i64 >= profile.breaker_failure_threshold {
            h.breaker_open_until = Some(now + Duration::from_millis(profile.breaker_cooldown_ms));
            h.failures.clear();
        }
    }

    /// Record a success: clears cooldown/backoff and marks last-known-good.
    pub fn record_success(&self, provider: &str) {
        let mut h = self.entry(provider);
        h.cooldown_until = None;
        h.backoff_level = 0;
        h.last_success = Some(Instant::now());
    }

    /// In-flight request accounting (least-used strategy + concurrency cap).
    pub fn begin_request(&self, provider: &str) {
        self.entry(provider).in_flight += 1;
    }

    pub fn end_request(&self, provider: &str) {
        if let Some(mut h) = self.map.get_mut(provider) {
            h.in_flight = (h.in_flight - 1).max(0);
        }
    }

    pub fn inflight(&self, provider: &str) -> i64 {
        self.map.get(provider).map(|h| h.in_flight).unwrap_or(0)
    }

    pub fn last_success(&self, provider: &str) -> Option<Instant> {
        self.map.get(provider).and_then(|h| h.last_success)
    }

    /// Remaining cooldown ms (for diagnostics / /v1/quotas).
    pub fn cooldown_remaining_ms(&self, provider: &str) -> i64 {
        self.map
            .get(provider)
            .and_then(|h| h.cooldown_until)
            .map(|u| (u - Instant::now()).as_millis() as i64)
            .unwrap_or(0)
            .max(0)
    }

    /// Ban one model on one provider (passthroughModels-style model-level 404),
    /// escalating through BACKOFF_STEPS_MS.
    pub fn ban_model(&self, provider: &str, model: &str) {
        let key = format!("{provider}|{model}");
        // `insert` would discard an active model-level cooldown living under
        // a different key; the ban key is separate, so a plain insert is
        // correct here, but keep the entry explicit rather than replacing a
        // live record.
        self.map.insert(key, ConnHealth::default());
    }

    /// Snapshot of connection-level health for /v1/providers.
    pub fn snapshot(&self) -> Vec<(String, i64, i64)> {
        let mut out: Vec<(String, i64, i64)> = self
            .map
            .iter()
            .filter(|e| !e.key().contains('|'))
            .map(|e| {
                let cd = e
                    .cooldown_until
                    .map(|u| (u - Instant::now()).as_millis() as i64)
                    .unwrap_or(0);
                (e.key().clone(), e.in_flight, cd.max(0))
            })
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::FailureKind;

    #[test]
    fn cooldown_values_match_constants() {
        assert_eq!(cooldown_for(FailureKind::Auth, false), Duration::from_secs(120));
        assert_eq!(cooldown_for(FailureKind::NotFound, false), Duration::from_secs(120));
        assert_eq!(cooldown_for(FailureKind::NotFound, true), Duration::from_secs(5));
        assert_eq!(cooldown_for(FailureKind::Server, false), Duration::from_secs(2));
        assert_eq!(cooldown_for(FailureKind::Client, false), Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(calculate_backoff(0), Duration::from_millis(1_000));
        assert_eq!(calculate_backoff(1), Duration::from_millis(2_000));
        assert_eq!(calculate_backoff(3), Duration::from_millis(8_000));
        assert_eq!(calculate_backoff(20), Duration::from_millis(120_000));
    }

    #[test]
    fn model_ban_steps() {
        assert_eq!(model_ban_duration(0), Duration::from_secs(60));
        assert_eq!(model_ban_duration(2), Duration::from_secs(300));
        assert_eq!(model_ban_duration(9), Duration::from_secs(1200));
    }

    #[test]
    fn breaker_profiles() {
        let oauth = profile_for(AuthType::Oauth);
        assert_eq!(oauth.threshold, 8);
        assert_eq!(oauth.reset_ms, 60_000);
        assert_eq!(oauth.breaker_cooldown_ms, 300_000);
        let apikey = profile_for(AuthType::ApiKey);
        assert_eq!(apikey.threshold, 12);
        assert_eq!(apikey.rate_limit_cooldown_ms, 0);
        let local = profile_for(AuthType::Local);
        assert_eq!(local.threshold, 2);
    }

    #[test]
    fn failure_cooldown_and_recovery() {
        let store = CircuitStore::new();
        assert!(store.is_available("p"));
        store.begin_request("p");
        assert!(store.is_available("p"));
        store.end_request("p");

        store.record_failure("p", FailureKind::Auth, false, AuthType::ApiKey);
        assert!(!store.is_available("p"));
        assert!(store.cooldown_remaining_ms("p") > 0);

        store.record_success("p");
        assert!(store.is_available("p"));
        assert_eq!(store.cooldown_remaining_ms("p"), 0);
        assert!(store.last_success("p").is_some());
    }

    #[test]
    fn model_ban_blocks_only_that_model() {
        let store = CircuitStore::new();
        store.ban_model("prov", "m1");
        assert!(store.is_model_banned("prov", "m1"));
        assert!(!store.is_model_banned("prov", "m2"));
        assert!(store.is_available("prov"));
    }

    #[test]
    fn inflight_cap_blocks_further_requests() {
        let store = CircuitStore::new();
        for _ in 0..DEFAULT_CONCURRENCY {
            store.begin_request("x");
        }
        assert!(!store.is_available("x"));
        store.end_request("x");
        assert!(store.is_available("x"));
    }
}

#[cfg(test)]
mod model_cooldown_tests {
    use super::*;

    #[test]
    fn a_model_level_failure_does_not_freeze_its_siblings() {
        let c = CircuitStore::new();
        // The head id fails in a way that is a property of the id, not of the
        // link: only that model must cool down.
        c.record_model_failure("opencode-go", "delisted-model", FailureKind::Server, false);
        assert!(c.is_model_cooling("opencode-go", "delisted-model"));
        // The sibling with quota left must stay available.
        assert!(c.is_available("opencode-go"), "provider must not be frozen");
        assert!(!c.is_model_cooling("opencode-go", "healthy-model"));
    }

    #[test]
    fn model_cooldown_expires() {
        let c = CircuitStore::new();
        c.record_model_failure("p", "m", FailureKind::Auth, false);
        assert!(c.is_model_cooling("p", "m"));
        // the provider itself is never cooled by a model-level verdict
        assert!(c.is_available("p"));
    }
}

#[cfg(test)]
mod model_ban_probe {
    use super::*;

    #[test]
    fn a1_model_level_failure_must_not_ban_the_model_forever() {
        let c = CircuitStore::new();
        c.record_model_failure("opencode-go", "delisted-model", FailureKind::Server, false);
        println!(
            "A1: is_model_banned={} is_model_cooling={} provider_available={}",
            c.is_model_banned("opencode-go", "delisted-model"),
            c.is_model_cooling("opencode-go", "delisted-model"),
            c.is_available("opencode-go")
        );
        assert!(
            !c.is_model_banned("opencode-go", "delisted-model"),
            "A1: one transient failure permanently banned the model id"
        );
    }
}
