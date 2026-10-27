//! OpenCode **subscription quota discovery** (`/zen/go/v1/usage`).
//!
//! The DSH community quota plugins (`dsh-plugin-balance`, `dsh-quota-panel`,
//! `dsh-quota`, `dsh-hub-oauth-gateway`, `dsh-cost-meter`, Tokdash, EchoBird,
//! `dsh-opencode-go`) all read the same public endpoint:
//!
//! ```text
//! GET https://opencode.ai/zen/go/v1/usage          (alias: /inference/go/v1/usage)
//! Authorization: Bearer <OPENCODE_GO_API_KEY>
//! -> {"usage":{"rolling":{...},"weekly":{...},"monthly":{...}}}
//! ```
//!
//! Each window is `{status: "ok"|"rate-limited", percent: <used 0..100>,
//! resetsAt: <ISO-8601>}`; several plugins also accept the relative
//! `resetInSec`/`resetsInSeconds` spellings. The anonymous **Zen free lane has
//! no quota endpoint at all** — three independent sources state it explicitly
//! (EchoBird `usage_providers/opencode.rs:1`, Tokdash
//! `docs/reference/QUOTA.md:158-159`, `dsh-usage-stats lib/balance.js:7-9`) —
//! so the free lane's budget is only observable through 429/403 feedback,
//! which [`crate::opencode_free`] learns from. This module covers the tier that
//! *does* publish a reading, and turns an exhausted window into a bounded
//! routing gate so the combo falls through to a provider that still has budget.

use serde_json::Value;
use std::sync::{Arc, OnceLock, RwLock};

/// Relative paths tried in order (the second is the newer alias).
pub const USAGE_PATHS: [&str; 2] = ["/usage", "/usage?source=omniroute"];

/// How long a reading stays actionable for routing.
pub const FRESH_MS: u128 = 900_000;
/// Upper bound of the routing gate, even if the reset is further out.
pub const MAX_GATE_MS: u128 = 6 * 3_600_000;
/// Default gate when a window reports exhaustion without a reset time.
pub const DEFAULT_GATE_MS: u128 = 120_000;

/// One rolling/weekly/monthly quota window.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Window {
    pub status: String,
    /// Percentage **already used** (0–100), as the upstream reports it.
    pub used_percent: f64,
    pub resets_at_ms: Option<u128>,
}

impl Window {
    pub fn exhausted(&self) -> bool {
        self.status.eq_ignore_ascii_case("rate-limited") || self.used_percent >= 100.0
    }

    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }
}

/// Last known usage, plus where it came from.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Usage {
    pub provider: String,
    pub url: String,
    pub fetched_at_ms: u128,
    pub error: String,
    /// Prepaid balance reported by providers that work this way (OpenRouter,
    /// DeepSeek, Moonshot, SiliconFlow). `None` for windowed subscriptions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolling: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly: Option<Window>,
}

impl Usage {
    pub fn fresh(&self, now_ms: u128) -> bool {
        self.fetched_at_ms > 0 && now_ms.saturating_sub(self.fetched_at_ms) <= FRESH_MS
    }

    /// The tightest exhausted window, if any.
    pub fn exhausted_window(&self) -> Option<&Window> {
        [self.monthly.as_ref(), self.weekly.as_ref(), self.rolling.as_ref()]
            .into_iter()
            .flatten()
            .find(|window| window.exhausted())
    }
}

fn shared() -> &'static RwLock<Option<Arc<Usage>>> {
    static SHARED: OnceLock<RwLock<Option<Arc<Usage>>>> = OnceLock::new();
    SHARED.get_or_init(|| RwLock::new(None))
}

/// Readings per *credential*, keyed by [`crate::usage_probe::credential_id`].
///
/// A subscription belongs to a key, not to a provider id: the same key
/// registered under two provider names reports one quota. The dashboard groups
/// by this id so those registrations collapse into a single row, and one
/// upstream request per key is enough.
fn per_credential() -> &'static RwLock<std::collections::BTreeMap<String, Arc<Usage>>> {
    static PER_CREDENTIAL: OnceLock<RwLock<std::collections::BTreeMap<String, Arc<Usage>>>> =
        OnceLock::new();
    PER_CREDENTIAL.get_or_init(|| RwLock::new(std::collections::BTreeMap::new()))
}

/// Record a reading against the credential that produced it.
pub fn install_for(credential: &str, mut usage: Usage) {
    if usage.fetched_at_ms == 0 {
        usage.fetched_at_ms = now_ms();
    }
    let usage = Arc::new(usage);
    per_credential()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(credential.to_string(), usage);
}

/// Every credential reading taken so far, in stable key order.
pub fn snapshot_all() -> std::collections::BTreeMap<String, Arc<Usage>> {
    per_credential()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The reading for one credential, if any.
pub fn snapshot_for(credential: &str) -> Option<Arc<Usage>> {
    per_credential()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(credential)
        .cloned()
}

/// Forget every per-credential reading (tests, re-keying).
pub fn clear_per_credential() {
    per_credential()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Current reading, if one was ever taken.
pub fn snapshot() -> Option<Arc<Usage>> {
    shared().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Install a reading (also used by tests and the manual refresh endpoint).
pub fn install(mut usage: Usage) {
    if usage.fetched_at_ms == 0 {
        usage.fetched_at_ms = now_ms();
    }
    *shared().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(usage));
}

/// Forget the reading (tests, and a manual reset after re-keying).
pub fn clear() {
    *shared().write().unwrap_or_else(|e| e.into_inner()) = None;
    clear_per_credential();
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Build the usage URL from a provider base (`https://opencode.ai/zen/go/v1`).
pub fn usage_url(base: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), USAGE_PATHS[0])
}

fn parse_window(value: &Value) -> Option<Window> {
    let object = value.as_object()?;
    let status = object
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("ok")
        .to_string();
    let used_percent = object
        .get("percent")
        .or_else(|| object.get("usedPercent"))
        .or_else(|| object.get("used_percent"))
        .and_then(|p| p.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let resets_at_ms = object
        .get("resetsAt")
        .or_else(|| object.get("resets_at"))
        .and_then(|v| v.as_str())
        .and_then(parse_iso_ms)
        .or_else(|| {
            // Relative fallbacks used by some clients.
            object
                .get("resetInSec")
                .or_else(|| object.get("resetsInSeconds"))
                .or_else(|| object.get("reset_in_sec"))
                .and_then(|v| v.as_f64())
                .map(|secs| now_ms().saturating_add((secs.max(0.0) * 1000.0) as u128))
        });
    Some(Window { status, used_percent, resets_at_ms })
}

/// Parse the `usage` envelope; `None` when the payload carries no window at all.
pub fn parse_usage(provider: &str, url: &str, body: &Value) -> Option<Usage> {
    let usage = body.get("usage").unwrap_or(body);
    let rolling = usage.get("rolling").and_then(parse_window);
    let weekly = usage.get("weekly").and_then(parse_window);
    let monthly = usage.get("monthly").and_then(parse_window);
    if rolling.is_none() && weekly.is_none() && monthly.is_none() {
        return None;
    }
    Some(Usage {
        provider: provider.to_string(),
        url: url.to_string(),
        fetched_at_ms: now_ms(),
        error: String::new(),
        balance: None,
        currency: None,
        rolling,
        weekly,
        monthly,
    })
}

/// Parse a balance-style payload into a [`Usage`].
///
/// Vendors disagree on both the shape and the field names, so this accepts the
/// documented spellings rather than one exact path:
///
///  * OpenRouter `{"data":{"total_credits":10,"total_usage":9.57}}` → difference
///  * DeepSeek  `{"balance_infos":[{"total_balance":"12.34","currency":"CNY"}]}`
///  * Moonshot  `{"data":{"available_balance":12.34}}`
///  * SiliconFlow `{"data":{"balance":"12.34"}}`
///
/// A payload that yields no number returns `None`, so an unexpected vendor page
/// (an HTML error, a schema change) surfaces as "not fetched" instead of `0`.
pub fn parse_balance(provider: &str, url: &str, body: &Value) -> Option<Usage> {
    let num = |v: Option<&Value>| -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    let data = body.get("data").unwrap_or(body);

    // Money left: try explicit balances first.
    let direct = num(data.get("balance"))
        .or_else(|| num(data.get("available_balance")))
        .or_else(|| num(data.get("total_balance")))
        .or_else(|| num(body.get("balance")))
        .or_else(|| {
            // DeepSeek keeps balances in an array of currency entries.
            body.get("balance_infos")
                .and_then(|b| b.as_array())
                .and_then(|a| a.iter().find_map(|e| num(e.get("total_balance"))))
        });

    // Grant minus spend (OpenRouter).
    let granted = num(data.get("total_credits"));
    let spent = num(data.get("total_usage"));
    let from_grant = match (granted, spent) {
        (Some(g), Some(s)) => Some(g - s),
        _ => None,
    };

    let balance = direct.or(from_grant)?;
    let currency = body
        .get("balance_infos")
        .and_then(|b| b.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("currency"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .or_else(|| data.get("currency").and_then(|c| c.as_str()).map(str::to_string))
        .or_else(|| {
            // OpenRouter credits are USD by definition.
            (granted.is_some() || spent.is_some()).then(|| "USD".to_string())
        });

    Some(Usage {
        provider: provider.to_string(),
        url: url.to_string(),
        fetched_at_ms: now_ms(),
        error: String::new(),
        balance: Some(balance),
        currency,
        rolling: None,
        weekly: None,
        monthly: None,
    })
}

/// Fetch usage for an arbitrary probe target (windowed or balance).
///
/// The OpenCode path keeps its byte-identical request (the lane is
/// fingerprint-sensitive); everything else goes through this generic form.
pub async fn fetch_probe(
    client: &reqwest::Client,
    probe: &dyn crate::usage_probe::UsageProbe,
    provider: &str,
    base: &str,
    key: &str,
) -> Result<Usage, String> {
    let url = probe.usage_url(base);
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .header("user-agent", "omniroute-rust")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!(
            "usage endpoint returned HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))?;
    match probe.shape() {
        crate::usage_probe::UsageShape::Windowed => {
            parse_usage(provider, &url, &body).ok_or_else(|| "usage payload carried no window".to_string())
        }
        crate::usage_probe::UsageShape::Balance => {
            parse_balance(provider, &url, &body).ok_or_else(|| "payload carried no balance".to_string())
        }
    }
}

/// Minimal RFC 3339 → epoch ms (the upstream always sends `...Z`).
pub fn parse_iso_ms(value: &str) -> Option<u128> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // days since epoch (civil-from-days, Howard Hinnant's algorithm)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((seconds.max(0) as u128) * 1000)
}

/// GET the usage endpoint with a bearer key.
pub async fn fetch(client: &reqwest::Client, provider: &str, base: &str, key: &str) -> Result<Usage, String> {
    let url = usage_url(base);
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .header("user-agent", "opencode/latest/2.0.29/cli")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        // 401/403 mean "no subscription for this key" — a neutral state, not a
        // gateway error (parity with `dsh-cost-meter`'s soft handling).
        return Err(format!("usage endpoint returned HTTP {status}: {}", text.chars().take(200).collect::<String>()));
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))?;
    parse_usage(provider, &url, &body).ok_or_else(|| "usage payload carried no window".to_string())
}

/// One refresh round for `opencode-go`; records the reading or the error.
pub async fn refresh(client: &reqwest::Client, provider: &str, base: &str, key: Option<String>) -> Result<Usage, String> {
    let Some(key) = key.filter(|k| !k.trim().is_empty()) else {
        let error = "no credential for the subscription tier".to_string();
        install(Usage {
            provider: provider.to_string(),
            url: usage_url(base),
            fetched_at_ms: now_ms(),
            error: error.clone(),
            ..Default::default()
        });
        return Err(error);
    };
    match fetch(client, provider, base, &key).await {
        Ok(usage) => {
            install(usage.clone());
            Ok(usage)
        }
        Err(error) => {
            install(Usage {
                provider: provider.to_string(),
                url: usage_url(base),
                fetched_at_ms: now_ms(),
                error: error.clone(),
                ..Default::default()
            });
            Err(error)
        }
    }
}

/// Routing gate: when a fresh reading shows an exhausted window, hand back the
/// instant the gate should lift (the window reset, bounded). `None` means
/// "no quota evidence — dispatch normally", which is also what a stale reading
/// returns, so a broken probe can never black-hole a provider.
pub fn gate_until_ms(now: u128) -> Option<u128> {
    let usage = snapshot()?;
    if !usage.fresh(now) {
        return None;
    }
    let window = usage.exhausted_window()?;
    let until = window
        .resets_at_ms
        .filter(|reset| *reset > now)
        .unwrap_or_else(|| now.saturating_add(DEFAULT_GATE_MS));
    Some(until.min(now.saturating_add(MAX_GATE_MS)))
}

/// Is the subscription tier currently gated by its own quota reading?
pub fn gated(now: u128) -> Option<u128> {
    gate_until_ms(now).map(|until| until.saturating_sub(now))
}

/// Whether the gate's recovery time is *known* (the window published a future
/// `resetsAt`).
///
/// The pass-1 half-open probe exists to escape cooldowns whose recovery is
/// unknown — without it one transient failure would hard-fail the whole window.
/// A published reset is the opposite case: the tier cannot serve before it, so
/// probing only burns a guaranteed 429 on every request until then. Callers use
/// this to skip such a candidate outright.
pub fn gate_has_known_reset(now: u128) -> bool {
    snapshot()
        .filter(|usage| usage.fresh(now))
        .and_then(|usage| usage.exhausted_window().and_then(|window| window.resets_at_ms))
        .is_some_and(|reset| reset > now)
}

/// Background refresher: immediate reading, then the steady cadence.
pub fn spawn_background(state: Arc<crate::state::AppState>, ttl_seconds: u64) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("opencode quota: http client failed: {error}");
                return;
            }
        };
        let ttl = ttl_seconds.max(60);
        loop {
            refresh_all_credentials(&state, &client).await;
            tokio::time::sleep(std::time::Duration::from_secs(ttl)).await;
        }
    });
}

/// One credential that can be asked for usage.
#[derive(Clone)]
pub struct CredentialTarget {
    /// Fingerprint (`usage_probe::credential_id`); never the raw key.
    pub credential: String,
    /// A provider id this credential is registered under (used for base URL).
    pub provider: String,
    pub key: String,
    pub base: String,
    /// `None` for the OpenCode lane (which keeps its own request shape);
    /// `Some` for every other probe.
    pub probe: Option<&'static dyn crate::usage_probe::UsageProbe>,
}

impl std::fmt::Debug for CredentialTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The probe is a trait object, so print its id rather than deriving.
        f.debug_struct("CredentialTarget")
            .field("credential", &self.credential)
            .field("provider", &self.provider)
            .field("base", &self.base)
            .field("probe", &self.probe.map(|p| p.id()))
            .finish()
    }
}

/// Every distinct credential the dashboard should report on.
///
/// Deduplicated by key: a subscription registered under several provider ids is
/// one quota and must be fetched once. Only *connected* providers are included,
/// so the dashboard never advertises a provider the operator has not set up.
pub fn credential_targets(state: &crate::state::AppState) -> Vec<CredentialTarget> {
    let mut out: Vec<CredentialTarget> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for conn in state.provider_connections.all_unmasked() {
        if !conn.enabled {
            continue;
        }
        let Some(probe) = crate::usage_probe::probe_for(&conn.provider) else {
            continue;
        };
        let _ = probe;
        let Some(key) = conn.api_key.clone().filter(|k| !k.trim().is_empty()) else {
            continue;
        };
        let credential = crate::usage_probe::credential_id(&key);
        if !seen.insert(credential.clone()) {
            continue;
        }
        let base = conn
            .base_url
            .clone()
            .filter(|b| !b.trim().is_empty())
            .or_else(|| state.base_url_for(&state.registry, &conn.provider))
            .unwrap_or_else(|| "https://opencode.ai/zen/go/v1".to_string());
        // The OpenCode lane keeps its dedicated request path (the free lane is
        // fingerprint-sensitive), so it is flagged as "no probe" here.
        let probe = (probe.id() != "opencode").then_some(probe);
        out.push(CredentialTarget {
            credential,
            provider: conn.provider.clone(),
            key,
            base,
            probe,
        });
    }

    // A single-credential deployment (env key, no managed connection) keeps the
    // previous behaviour so the routing gate still has a reading.
    if out.is_empty() {
        if let Some(key) = state
            .api_key_for("opencode-go")
            .or_else(|| std::env::var("OPENCODE_GO_API_KEY").ok())
        {
            let base = state
                .base_url_for(&state.registry, "opencode-go")
                .unwrap_or_else(|| "https://opencode.ai/zen/go/v1".to_string());
            out.push(CredentialTarget {
                credential: crate::usage_probe::credential_id(&key),
                provider: "opencode-go".to_string(),
                key,
                base,
                probe: None,
            });
        }
    }
    out
}

/// Refresh every credential, then publish the tightest reading to the
/// single-value slot the routing gate reads.
pub async fn refresh_all_credentials(
    state: &crate::state::AppState,
    client: &reqwest::Client,
) -> usize {
    let targets = credential_targets(state);
    let mut tightest: Option<Arc<Usage>> = None;
    let mut updated = 0usize;

    for target in &targets {
        let outcome = match target.probe {
            // The OpenCode lane keeps its byte-identical request; the lane is
            // fingerprint-sensitive and must not gain a different user-agent.
            None => fetch(client, &target.provider, &target.base, &target.key).await,
            Some(probe) => {
                fetch_probe(client, probe, &target.provider, &target.base, &target.key).await
            }
        };
        let usage = match outcome {
            Ok(usage) => usage,
            Err(error) => Usage {
                provider: target.provider.clone(),
                url: target
                    .probe
                    .map(|p| p.usage_url(&target.base))
                    .unwrap_or_else(|| usage_url(&target.base)),
                fetched_at_ms: now_ms(),
                error,
                ..Default::default()
            },
        };
        if usage.error.is_empty() {
            tracing::info!(
                provider = %target.provider,
                credential = %target.credential,
                balance = ?usage.balance,
                currency = ?usage.currency,
                "quota: rolling {:?}% weekly {:?}% monthly {:?}%",
                usage.rolling.as_ref().map(|w| w.used_percent),
                usage.weekly.as_ref().map(|w| w.used_percent),
                usage.monthly.as_ref().map(|w| w.used_percent)
            );
        } else {
            tracing::debug!(
                provider = %target.provider,
                credential = %target.credential,
                error = %usage.error,
                "opencode quota: reading failed"
            );
        }
        install_for(&target.credential, usage.clone());
        updated += 1;

        // The gate must be conservative: one exhausted subscription should stop
        // requests that would hit it, so keep the reading with the least
        // headroom (an error counts as "no headroom").
        let usage = Arc::new(usage);
        tightest = Some(match tightest {
            None => usage,
            Some(current) => {
                if headroom(&usage) < headroom(&current) {
                    usage
                } else {
                    current
                }
            }
        });
    }

    if let Some(usage) = tightest {
        install((*usage).clone());
    }
    updated
}

/// Smallest remaining percentage across the windows; an error or an exhausted
/// window means zero headroom.
fn headroom(usage: &Usage) -> f64 {
    if !usage.error.is_empty() {
        return 0.0;
    }
    [usage.rolling.as_ref(), usage.weekly.as_ref(), usage.monthly.as_ref()]
        .into_iter()
        .flatten()
        .map(|w| w.remaining_percent())
        .fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_payload() {
        let body: Value = serde_json::from_str(
            r#"{"usage":{
                "rolling":{"status":"ok","percent":12.5,"resetsAt":"2026-10-16T12:00:00.000Z"},
                "weekly":{"status":"ok","percent":40.0,"resetsAt":"2026-10-20T00:00:00.000Z"},
                "monthly":{"status":"rate-limited","percent":100.0,"resetsAt":"2026-11-01T00:00:00.000Z"}}}"#,
        )
        .unwrap();
        let usage = parse_usage("opencode-go", "https://opencode.ai/zen/go/v1/usage", &body).unwrap();
        assert_eq!(usage.rolling.as_ref().unwrap().used_percent, 12.5);
        assert_eq!(usage.rolling.as_ref().unwrap().remaining_percent(), 87.5);
        assert!(usage.monthly.as_ref().unwrap().exhausted());
        assert!(usage.exhausted_window().is_some());
        assert!(usage.rolling.as_ref().unwrap().resets_at_ms.is_some());
    }

    #[test]
    fn relative_reset_spellings_are_accepted() {
        let body: Value = serde_json::from_str(
            r#"{"usage":{"rolling":{"percent":5,"resetInSec":600},"weekly":{"percent":0}}}"#,
        )
        .unwrap();
        let usage = parse_usage("opencode-go", "u", &body).unwrap();
        let reset = usage.rolling.as_ref().unwrap().resets_at_ms.unwrap();
        assert!(reset > now_ms());
    }

    #[test]
    fn a_window_without_a_window_is_not_a_reading() {
        assert!(parse_usage("opencode-go", "u", &serde_json::json!({"usage":{}})).is_none());
        assert!(parse_usage("opencode-go", "u", &serde_json::json!({})).is_none());
    }

    #[test]
    fn iso_timestamps_round_trip() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-10-16T12:00:00.000Z"), Some(1_792_152_000_000));
        assert_eq!(parse_iso_ms("not-a-date"), None);
    }

    #[test]
    fn gate_only_fires_on_a_fresh_exhausted_reading() {
        let now = now_ms();
        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            monthly: Some(Window { status: "ok".into(), used_percent: 10.0, resets_at_ms: None }),
            ..Default::default()
        });
        assert_eq!(gate_until_ms(now), None, "a healthy reading must not gate");

        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            monthly: Some(Window {
                status: "rate-limited".into(),
                used_percent: 100.0,
                resets_at_ms: Some(now + 3_600_000),
            }),
            ..Default::default()
        });
        assert_eq!(gate_until_ms(now), Some(now + 3_600_000));

        // A stale reading never gates: a broken probe must not black-hole the provider.
        assert_eq!(gate_until_ms(now + FRESH_MS + 1), None);
    }

    #[test]
    fn a_published_reset_is_a_known_recovery_point() {
        let now = now_ms();
        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            rolling: Some(Window {
                status: "rate-limited".into(),
                used_percent: 100.0,
                resets_at_ms: Some(now + 3_600_000),
            }),
            ..Default::default()
        });
        assert!(gate_has_known_reset(now), "a future resetsAt is a known recovery");
        assert_eq!(gate_until_ms(now), Some(now + 3_600_000));

        // No reset time → the gate still applies but probing must stay allowed.
        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            rolling: Some(Window { status: "rate-limited".into(), used_percent: 100.0, resets_at_ms: None }),
            ..Default::default()
        });
        assert!(gate_until_ms(now).is_some());
        assert!(!gate_has_known_reset(now));

        // A reset already in the past is not a gate at all.
        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            rolling: Some(Window {
                status: "rate-limited".into(),
                used_percent: 100.0,
                resets_at_ms: Some(now - 1),
            }),
            ..Default::default()
        });
        install(Usage { fetched_at_ms: now, ..Default::default() });
        assert!(!gate_has_known_reset(now));
    }

    #[test]
    fn gate_is_bounded_when_the_reset_is_far_out() {
        let now = now_ms();
        install(Usage {
            provider: "opencode-go".into(),
            url: "u".into(),
            fetched_at_ms: now,
            rolling: Some(Window {
                status: "rate-limited".into(),
                used_percent: 100.0,
                resets_at_ms: Some(now + 10 * 3_600_000),
            }),
            ..Default::default()
        });
        assert_eq!(gate_until_ms(now), Some(now + MAX_GATE_MS));
    }

    /// A subscription belongs to a key, not to a provider id: the same key
    /// registered under two providers is one quota and must be fetched once.
    #[test]
    fn one_key_registered_twice_yields_one_credential_target() {
        use crate::server::providers_admin::ProviderConnection;
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf()));
        for (id, provider, name) in [
            ("c1", "opencode-go", "go1"),
            ("c2", "opencode-zen", "zen1"),
            ("c3", "opencode-go", "go2"),
        ] {
            let key = if id == "c3" { "key-second" } else { "key-shared" };
            state.provider_connections.upsert(ProviderConnection {
                id: id.into(),
                provider: provider.into(),
                name: name.into(),
                api_key: Some(key.into()),
                base_url: None,
                api_type: None,
                model_list: Vec::new(),
                synced_models: Vec::new(),
                model_metadata: std::collections::HashMap::new(),
                synced_at_ms: 0,
                hidden_models: Vec::new(),
                enabled: true,
                created_at_ms: 0,
            });
        }
        let targets = credential_targets(&state);
        assert_eq!(targets.len(), 2, "two distinct keys → two targets, saw {targets:?}", );
        let creds: std::collections::HashSet<&String> =
            targets.iter().map(|t| &t.credential).collect();
        assert_eq!(creds.len(), 2);
    }

    /// A provider with no usage endpoint must not be asked, and must not appear.
    #[test]
    fn providers_without_a_probe_are_not_targets() {
        use crate::server::providers_admin::ProviderConnection;
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf()));
        state.provider_connections.upsert(ProviderConnection {
            id: "x".into(),
            provider: "openai-compatible-plain".into(),
            name: "plain".into(),
            api_key: Some("sk-plain".into()),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        });
        assert!(credential_targets(&state).is_empty(), "no probe → no target");
    }

    /// Readings are stored per credential: two keys never overwrite each other.
    #[test]
    fn per_credential_readings_do_not_overwrite_each_other() {
        clear();
        install_for(
            "aaa",
            Usage {
                provider: "opencode-go".into(),
                url: "u".into(),
                fetched_at_ms: 1,
                error: String::new(),
                rolling: Some(Window { status: "ok".into(), used_percent: 10.0, resets_at_ms: None }),
                ..Default::default()
            },
        );
        install_for(
            "bbb",
            Usage {
                provider: "opencode-zen".into(),
                url: "u".into(),
                fetched_at_ms: 2,
                error: String::new(),
                rolling: Some(Window { status: "ok".into(), used_percent: 90.0, resets_at_ms: None }),
                ..Default::default()
            },
        );
        let all = snapshot_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all["aaa"].rolling.as_ref().unwrap().used_percent, 10.0);
        assert_eq!(all["bbb"].rolling.as_ref().unwrap().used_percent, 90.0);
        assert!(snapshot_for("missing").is_none());
        clear();
    }

    /// A failed reading must not be mistaken for "zero used".
    #[test]
    fn an_error_reading_has_no_headroom() {
        let ok = Usage {
            rolling: Some(Window { status: "ok".into(), used_percent: 10.0, resets_at_ms: None }),
            ..Default::default()
        };
        let failed = Usage { error: "boom".into(), ..Default::default() };
        assert!(headroom(&failed) < headroom(&ok), "an error must rank as the tightest");
    }

    /// Vendors disagree on both shape and field names; each documented spelling
    /// must land on the same number.
    #[test]
    fn balance_payloads_from_every_supported_vendor_parse() {
        // OpenRouter: grant minus spend (live-checked 2026-10-27).
        let u = parse_balance(
            "openrouter",
            "u",
            &serde_json::json!({"data": {"total_credits": 10, "total_usage": 9.570476099}}),
        )
        .expect("openrouter credits");
        let bal = u.balance.expect("balance");
        assert!((bal - 0.429523901).abs() < 1e-6, "got {bal}");
        assert_eq!(u.currency.as_deref(), Some("USD"));

        // DeepSeek: balance array with a currency.
        let u = parse_balance(
            "deepseek",
            "u",
            &serde_json::json!({"balance_infos": [{"total_balance": "12.34", "currency": "CNY"}]}),
        )
        .expect("deepseek balance");
        assert_eq!(u.balance, Some(12.34));
        assert_eq!(u.currency.as_deref(), Some("CNY"));

        // Moonshot.
        let u = parse_balance(
            "moonshot",
            "u",
            &serde_json::json!({"data": {"available_balance": 5.5}}),
        )
        .expect("moonshot balance");
        assert_eq!(u.balance, Some(5.5));

        // SiliconFlow keeps the number as a string.
        let u = parse_balance(
            "siliconflow",
            "u",
            &serde_json::json!({"data": {"balance": "7.25"}}),
        )
        .expect("siliconflow balance");
        assert_eq!(u.balance, Some(7.25));
    }

    /// A payload we do not recognise must read as "no reading", never as zero:
    /// a bogus 0 would look like an exhausted account.
    #[test]
    fn an_unrecognised_payload_is_not_a_zero_balance() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"error": {"message": "unauthorized"}}),
            serde_json::json!({"data": {"unexpected": "shape"}}),
            serde_json::json!("<html>maintenance</html>"),
        ] {
            assert!(
                parse_balance("openrouter", "u", &body).is_none(),
                "must not invent a balance from {body}"
            );
        }
    }

    /// A windowed subscription must not be reported as a balance (and vice
    /// versa) — the dashboard picks its renderer from this.
    #[test]
    fn the_two_shapes_do_not_bleed_into_each_other() {
        let windowed = serde_json::json!({"usage": {"rolling": {"status": "ok", "percent": 5}}});
        assert!(parse_balance("opencode-go", "u", &windowed).is_none());
        let balance = serde_json::json!({"data": {"total_credits": 1, "total_usage": 0}});
        assert!(parse_usage("openrouter", "u", &balance).is_none());
    }
}