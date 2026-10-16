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
        rolling,
        weekly,
        monthly,
    })
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
        .header("user-agent", "opencode/1.19.0")
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
            let provider = "opencode-go";
            if let Some(entry) = state.registry.get(provider) {
                let base = state
                    .base_url_for(&state.registry, provider)
                    .unwrap_or_else(|| entry.base_url.clone());
                let key = state
                    .api_key_for(provider)
                    .or_else(|| std::env::var("OPENCODE_GO_API_KEY").ok());
                match refresh(&client, provider, &base, key).await {
                    Ok(usage) => tracing::info!(
                        "opencode quota: rolling {:?}% weekly {:?}% monthly {:?}%",
                        usage.rolling.as_ref().map(|w| w.used_percent),
                        usage.weekly.as_ref().map(|w| w.used_percent),
                        usage.monthly.as_ref().map(|w| w.used_percent)
                    ),
                    Err(error) => tracing::debug!("opencode quota: {error}"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(ttl)).await;
        }
    });
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
}
