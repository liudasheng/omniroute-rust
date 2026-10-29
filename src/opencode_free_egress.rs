//! Free-lane egress selection: the anonymous proxy pool and its observability
//! view.
//!
//! The upstream counts the free budget per **egress IP** (measured 2026-10-29:
//! a direct dial, a WARP exit and a second WARP `/32` each had independent
//! quota state). A single host usually gets one WARP `/32`, so scaling the free
//! lane means adding exits: HTTP CONNECT proxies, SOCKS5 proxies, other hosts.
//! This module owns the *policy* — which pool entries are valid, which exit a
//! conversation starts on, how many a request may try — while
//! [`crate::upstream::bun`] owns the tunneling and
//! [`crate::opencode_free_budget`] owns per-egress accounting.
//!
//! Semantics:
//!
//! * The pool is only used by the **anonymous** (`public`) credential. A
//!   managed account keeps its own optional `proxy_url` and dials directly when
//!   it has none, so per-account egress stays an explicit operator decision.
//! * Selection is **sticky per conversation**: the session seed (the same seed
//!   that derives the `ses_*` id, i.e. the first user message) hashes to a
//!   stable pool index, so consecutive turns of one conversation keep the same
//!   exit and the upstream's session/admission state stays coherent.
//! * A request tries a bounded slice of the pool (session-sticky start, then
//!   the following entries in order), capped by both the pool length and
//!   [`crate::config::Config::opencode_free_max_proxy_exits`]. A 429 or a
//!   tunnel failure moves to the next entry, which is a different IP and
//!   therefore a different upstream budget bucket.
//! * With no pool configured the lane keeps dialling directly, exactly like
//!   before proxies existed.

use crate::config::Config;
use crate::opencode_free_budget::{egress_masked_label, egress_mode, EgressView};
use crate::state::AccountCredential;
use crate::upstream::bun::{self, Egress};

/// Upper bound on proxy exits a single request may try, regardless of pool
/// size. Four covers a WARP exit plus a couple of fallbacks without turning one
/// failed turn into a long serial probe.
pub const DEFAULT_MAX_PROXY_EXITS: usize = 4;
/// Short cooldown of an exit whose proxy/tunnel failed (unreachable proxy,
/// refused CONNECT, SOCKS negotiation error). Long enough to skip a dead proxy,
/// short enough that a transient blip does not retire a working exit.
pub const DEFAULT_PROXY_COOLDOWN_SECS: u64 = 30;

/// Parse and validate the configured pool, preserving order and dropping
/// invalid or duplicate entries.
///
/// An invalid entry is dropped rather than aborting the whole pool: a typo in
/// one line must not silently disable every working exit. Duplicates are
/// removed by egress label so a repeated entry cannot skew the rotation.
pub fn parse_pool(raw: &[String]) -> Vec<Egress> {
    let mut out: Vec<Egress> = Vec::new();
    let mut labels: Vec<Option<String>> = Vec::new();
    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if bun::validate_proxy_url(entry).is_err() {
            continue;
        }
        let egress = Egress::from_proxy_url(Some(entry));
        let label = egress.label();
        if labels.contains(&label) {
            continue;
        }
        labels.push(label);
        out.push(egress);
    }
    out
}

/// The configured anonymous proxy pool (empty = direct dial).
pub fn pool_egresses(config: &Config) -> Vec<Egress> {
    parse_pool(&config.opencode_free_proxy_pool)
}

/// Stable 64-bit FNV-1a hash of the conversation session seed.
pub fn session_seed_hash(seed: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Order a pool for one request: a session-sticky start index, then the
/// following entries in pool order, truncated to the per-request exit cap.
pub fn order_pool(pool: Vec<Egress>, max_exits: usize, session_seed: &str) -> Vec<Egress> {
    if pool.is_empty() {
        return Vec::new();
    }
    let limit = max_exits.max(1).min(pool.len());
    let start = (session_seed_hash(session_seed) as usize) % pool.len();
    (0..limit)
        .map(|offset| pool[(start + offset) % pool.len()].clone())
        .collect()
}

/// The anonymous credential's egress candidates for one request, in the order
/// they must be tried. Empty means "no pool configured → dial directly".
pub fn ordered_pool_egresses(config: &Config, session_seed: &str) -> Vec<Egress> {
    order_pool(
        pool_egresses(config),
        config.opencode_free_max_proxy_exits,
        session_seed,
    )
}

/// One row of the status endpoint's `egress` view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitStatus {
    /// Masked `host:port` (`192.0.*.*:3128`) or `direct`.
    pub label: String,
    /// `direct`, `http_connect`, `socks5` or `socks5h`.
    pub mode: String,
    /// Where the exit came from: `pool`, `account` or `direct`.
    pub source: String,
    pub used: u64,
    pub limit: u64,
    pub in_flight: u64,
    pub cooldown_until_ms: u128,
    /// `unknown`, `account` or `egress`.
    pub scope: String,
    pub healthy: bool,
}

fn configured_row(egress: &Egress, source: &str, limit: u64) -> Option<ExitStatus> {
    let raw_label = egress.label()?;
    Some(ExitStatus {
        label: egress_masked_label(&raw_label),
        mode: egress_mode(&raw_label),
        source: source.to_string(),
        used: 0,
        limit,
        in_flight: 0,
        cooldown_until_ms: 0,
        scope: "unknown".to_string(),
        healthy: true,
    })
}

/// Merge the configured exits (pool + per-account `proxy_url`) with the budget
/// counters the lane has already accumulated, for `GET /v1/opencode-free`.
///
/// Configured exits that were never used are healthy by definition (there is no
/// failure state for them yet).
pub fn exit_statuses(
    config: &Config,
    accounts: &[AccountCredential],
    observed: &[EgressView],
) -> Vec<ExitStatus> {
    let mut rows: Vec<ExitStatus> = Vec::new();
    let mut push = |row: ExitStatus| {
        if !rows.iter().any(|seen| seen.label == row.label) {
            rows.push(row);
        }
    };
    for egress in pool_egresses(config) {
        if let Some(row) = configured_row(
            &egress,
            "pool",
            config.opencode_free_daily_limit_public,
        ) {
            push(row);
        }
    }
    for account in accounts {
        let Some(proxy_url) = account.proxy_url.as_deref().map(str::trim).filter(|url| !url.is_empty()) else {
            continue;
        };
        let egress = Egress::from_proxy_url(Some(proxy_url));
        if let Some(row) = configured_row(
            &egress,
            "account",
            config.opencode_free_daily_limit_per_account,
        ) {
            push(row);
        }
    }
    for view in observed {
        let scope = view.scope.as_str().to_string();
        let row = ExitStatus {
            label: view.label.clone(),
            mode: view.mode.clone(),
            source: if view.label == "direct" { "direct" } else { "observed" }.to_string(),
            used: view.used,
            limit: view.limit,
            in_flight: view.in_flight,
            cooldown_until_ms: view.cooldown_until_ms,
            scope,
            healthy: view.healthy,
        };
        match rows.iter_mut().find(|seen| seen.label == row.label) {
            Some(existing) => {
                existing.used = row.used;
                existing.limit = row.limit.max(existing.limit);
                existing.in_flight = row.in_flight;
                existing.cooldown_until_ms = row.cooldown_until_ms;
                existing.scope = row.scope;
                existing.healthy = row.healthy;
            }
            None => rows.push(row),
        }
    }
    // The default deployment (no pool, no per-account proxy) still has one
    // egress worth showing: the host's own address. Only synthesize it when no
    // request has charged it yet, so a configured pool is never confused with a
    // direct fallback that the lane does not use.
    if rows.is_empty() {
        rows.push(ExitStatus {
            label: "direct".to_string(),
            mode: "direct".to_string(),
            source: "direct".to_string(),
            used: 0,
            limit: config.opencode_free_daily_limit_public,
            in_flight: 0,
            cooldown_until_ms: 0,
            scope: "unknown".to_string(),
            healthy: true,
        });
    }
    rows.sort_by(|left, right| left.label.cmp(&right.label));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(urls: &[&str]) -> Vec<Egress> {
        parse_pool(&urls.iter().map(|url| (*url).to_string()).collect::<Vec<_>>())
    }

    fn labels(egresses: &[Egress]) -> Vec<String> {
        egresses
            .iter()
            .map(|egress| egress.label().unwrap_or_else(|| "direct".to_string()))
            .collect()
    }

    #[test]
    fn an_empty_pool_keeps_the_direct_dial() {
        assert!(parse_pool(&[]).is_empty());
        assert!(parse_pool(&["".to_string(), "   ".to_string()]).is_empty());
        assert!(order_pool(Vec::new(), 4, "seed").is_empty());
    }

    #[test]
    fn invalid_and_duplicate_entries_are_dropped_not_fatal() {
        let parsed = pool(&[
            "http://127.0.0.1:3128",
            "not-a-url",
            "socks5://127.0.0.1", // no SOCKS port
            "http://127.0.0.1:3128", // duplicate
            "socks5h://user:pass@127.0.0.1:1080",
        ]);
        assert_eq!(
            labels(&parsed),
            vec![
                "http://127.0.0.1:3128".to_string(),
                "socks5h://127.0.0.1:1080".to_string()
            ]
        );
    }

    #[test]
    fn the_pool_is_sticky_per_session_seed() {
        let urls = [
            "http://127.0.0.1:3101",
            "http://127.0.0.1:3102",
            "http://127.0.0.1:3103",
        ];
        for seed in ["hello", "another conversation", ""] {
            let first = labels(&order_pool(pool(&urls), 4, seed));
            let second = labels(&order_pool(pool(&urls), 4, seed));
            assert_eq!(first, second, "the same seed must pick the same order");
            // The order starts at the sticky index and covers every exit once.
            assert_eq!(first.len(), 3);
            let mut sorted = first.clone();
            sorted.sort();
            assert_eq!(sorted, labels(&pool(&urls)));
        }
        // Different seeds spread across the pool (FNV over many seeds, not a
        // single pair, so one collision does not fail the test).
        let mut starts = std::collections::BTreeSet::new();
        for index in 0..64 {
            let picked = order_pool(pool(&urls), 1, &format!("seed-{index}"));
            starts.insert(picked[0].label().unwrap_or_default());
        }
        assert_eq!(starts.len(), 3, "all exits are reachable as a sticky start");
    }

    #[test]
    fn a_request_tries_at_most_the_configured_number_of_exits() {
        let urls = [
            "http://127.0.0.1:3101",
            "http://127.0.0.1:3102",
            "http://127.0.0.1:3103",
            "http://127.0.0.1:3104",
            "http://127.0.0.1:3105",
        ];
        assert_eq!(order_pool(pool(&urls), 4, "seed").len(), 4);
        assert_eq!(order_pool(pool(&urls), 2, "seed").len(), 2);
        // The cap clamps to the pool length.
        assert_eq!(order_pool(pool(&urls), 99, "seed").len(), 5);
        // A zero cap still means "one exit", never "none".
        assert_eq!(order_pool(pool(&urls), 0, "seed").len(), 1);
    }

    #[test]
    fn rotation_wraps_around_the_pool() {
        let urls = ["http://127.0.0.1:3101", "http://127.0.0.1:3102"];
        let ordered = order_pool(pool(&urls), 2, "seed");
        let reversed = order_pool(
            vec![ordered[1].clone(), ordered[0].clone()],
            2,
            "seed",
        );
        assert_ne!(labels(&ordered), labels(&reversed));
        assert_eq!(
            {
                let mut all = labels(&ordered);
                all.sort();
                all
            },
            {
                let mut all = labels(&reversed);
                all.sort();
                all
            }
        );
    }
}