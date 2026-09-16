//! Per-provider rate limiting (parity: DEFAULT_API_LIMITS 60 RPM /
//! 350ms min interval / 6 concurrent in `open-sse/config/constants.ts`).

use dashmap::DashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct Window {
    hits: VecDeque<Instant>,
}

pub struct RateLimiter {
    windows: DashMap<String, Window>,
    rpm: u64,
    min_interval: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_limits(crate::config::DEFAULT_RATE_RPM, crate::config::DEFAULT_RATE_MIN_INTERVAL_MS)
    }

    pub fn with_limits(rpm: u64, min_interval_ms: u64) -> Self {
        Self {
            windows: DashMap::new(),
            rpm,
            min_interval: Duration::from_millis(min_interval_ms),
        }
    }

    /// True when the provider is below its requests-per-minute budget.
    pub fn allow(&self, provider: &str) -> bool {
        let rpm = self.rpm;
        let min_interval = self.min_interval;
        let now = Instant::now();
        let mut w = self.windows.entry(provider.to_string()).or_insert_with(|| Window { hits: VecDeque::new() });
        // drop hits older than 60s
        while let Some(front) = w.hits.front() {
            if now.duration_since(*front) > Duration::from_secs(60) {
                w.hits.pop_front();
            } else {
                break;
            }
        }
        if w.hits.len() >= rpm as usize {
            return false;
        }
        if let Some(last) = w.hits.back() {
            if now.duration_since(*last) < min_interval {
                return false;
            }
        }
        w.hits.push_back(now);
        true
    }

    pub fn hit_count(&self, provider: &str) -> usize {
        self.windows.get(provider).map(|w| w.hits.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_under_rpm() {
        let rl = RateLimiter::new();
        // min-interval 350ms means at most 2 quick hits are rejected after 1
        assert!(rl.allow("fast"));
        assert!(!rl.allow("fast"));
    }

    #[test]
    fn counts_hits() {
        let rl = RateLimiter::new();
        rl.allow("p");
        assert_eq!(rl.hit_count("p"), 1);
        assert_eq!(rl.hit_count("other"), 0);
    }
}

impl RateLimiter {
    /// Queue-like wait for a rate slot (parity: the original's request queue
    /// waits up to `maxWaitMs` before failing the dispatch). Returns true and
    /// records the hit when a slot was acquired.
    pub async fn wait_permit(&self, provider: &str, max_wait: Duration) -> bool {
        let deadline = std::time::Instant::now() + max_wait;
        loop {
            if self.allow(provider) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
}
