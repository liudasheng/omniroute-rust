//! Compression subsystem (parity: `open-sse/services/compression/*`).
//!
//! Modes mirror the original's `CompressionMode`: `off | lite | standard |
//! aggressive | ultra | rtk` (the original's stacked pipelines and omniglyph
//! are out of scope; documented in PARITY.md). Selection precedence:
//!   1. master switch off (`[compression] enabled = false`) → off
//!   2. per-request header `x-omniroute-compression`
//!      (off | default | lite | standard | aggressive | ultra | rtk)
//!   3. auto-trigger when estimated tokens >= `auto_trigger_tokens`
//!   4. configured `default_mode`
//!
//! Response header: `x-omniroute-compression: <mode>; source=<src>; tokens=o->c; rules: namexN`.

pub mod aggressive;
pub mod caveman;
pub mod estimate;
pub mod lite;
pub mod preserve;
pub mod rtk;
pub mod ultra;

pub use caveman::CompressionStats;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionMode {
    Off,
    Lite,
    Standard,
    Aggressive,
    Ultra,
    Rtk,
}

impl CompressionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "lite" => Some(Self::Lite),
            "standard" | "caveman" => Some(Self::Standard),
            "aggressive" => Some(Self::Aggressive),
            "ultra" => Some(Self::Ultra),
            "rtk" => Some(Self::Rtk),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lite => "lite",
            Self::Standard => "standard",
            Self::Aggressive => "aggressive",
            Self::Ultra => "ultra",
            Self::Rtk => "rtk",
        }
    }
}

/// Compression settings (parity: `CompressionConfig` core fields).
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    pub enabled: bool,
    pub default_mode: CompressionMode,
    /// estimated tokens ≥ this triggers `auto_trigger_mode` (0 = off)
    pub auto_trigger_tokens: i64,
    pub auto_trigger_mode: CompressionMode,
    pub preserve_system_prompt: bool,
    /// caveman knobs
    pub caveman_intensity: String, // lite|full|ultra
    pub compress_roles: Vec<String>,
    pub skip_rules: Vec<String>,
    pub min_message_length: usize,
    /// ultra knobs
    pub ultra_compression_rate: f64,
    pub ultra_min_score: f64,
    /// aggressive knobs
    pub aggressive_max_tokens_per_message: i64,
    pub aggressive_min_savings: f64,
    /// rtk knobs
    pub rtk_max_lines: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false, // parity: compression is opt-in
            default_mode: CompressionMode::Off,
            auto_trigger_tokens: 0,
            auto_trigger_mode: CompressionMode::Lite,
            preserve_system_prompt: true,
            caveman_intensity: "lite".into(),
            compress_roles: vec!["user".into()],
            skip_rules: Vec::new(),
            min_message_length: 50,
            ultra_compression_rate: 0.5,
            ultra_min_score: 0.3,
            aggressive_max_tokens_per_message: 2048,
            aggressive_min_savings: 0.05,
            rtk_max_lines: 120,
        }
    }
}

impl CompressionConfig {
    /// Load from the toml `[compression]` table (values already parsed as
    /// a generic Value map by config.rs) + env overrides.
    pub fn from_toml_and_env(t: Option<&Value>, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut c = Self::default();
        if let Some(t) = t.and_then(|v| v.as_object()) {
            if let Some(v) = t.get("enabled").and_then(|v| v.as_bool()) {
                c.enabled = v;
            }
            if let Some(m) = t.get("default_mode").and_then(|v| v.as_str()) {
                if let Some(m) = CompressionMode::parse(m) {
                    c.default_mode = m;
                }
            }
            if let Some(v) = t.get("auto_trigger_tokens").and_then(|v| v.as_i64()) {
                c.auto_trigger_tokens = v;
            }
            if let Some(m) = t.get("auto_trigger_mode").and_then(|v| v.as_str()) {
                if let Some(m) = CompressionMode::parse(m) {
                    c.auto_trigger_mode = m;
                }
            }
            if let Some(v) = t.get("preserve_system_prompt").and_then(|v| v.as_bool()) {
                c.preserve_system_prompt = v;
            }
            if let Some(v) = t.get("caveman_intensity").and_then(|v| v.as_str()) {
                c.caveman_intensity = v.to_string();
            }
            if let Some(v) = t.get("compress_roles").and_then(|v| v.as_array()) {
                c.compress_roles = v
                    .iter()
                    .filter_map(|r| r.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(v) = t.get("skip_rules").and_then(|v| v.as_array()) {
                c.skip_rules = v
                    .iter()
                    .filter_map(|r| r.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(v) = t.get("min_message_length").and_then(|v| v.as_u64()) {
                c.min_message_length = v as usize;
            }
            if let Some(v) = t.get("ultra_compression_rate").and_then(|v| v.as_f64()) {
                c.ultra_compression_rate = v;
            }
            if let Some(v) = t.get("ultra_min_score").and_then(|v| v.as_f64()) {
                c.ultra_min_score = v;
            }
            if let Some(v) = t.get("aggressive_max_tokens_per_message").and_then(|v| v.as_i64()) {
                c.aggressive_max_tokens_per_message = v;
            }
            if let Some(v) = t.get("aggressive_min_savings").and_then(|v| v.as_f64()) {
                c.aggressive_min_savings = v;
            }
            if let Some(v) = t.get("rtk_max_lines").and_then(|v| v.as_u64()) {
                c.rtk_max_lines = v as usize;
            }
        }
        // env overrides (request-header equivalent applied globally)
        if let Some(v) = env("OMNIROUTE_COMPRESSION") {
            match CompressionMode::parse(&v) {
                Some(CompressionMode::Off) => {
                    c.enabled = false;
                    c.default_mode = CompressionMode::Off;
                }
                Some(m) => {
                    c.enabled = true;
                    c.default_mode = m;
                }
                None => {}
            }
        }
        if let Some(v) = env("OMNIROUTE_COMPRESSION_AUTO_TRIGGER_TOKENS") {
            if let Ok(n) = v.parse() {
                c.auto_trigger_tokens = n;
            }
        }
        c
    }

    fn caveman_config(&self) -> caveman::CavemanConfig {
        caveman::CavemanConfig {
            intensity: caveman::Intensity::parse(&self.caveman_intensity),
            compress_roles: if self.preserve_system_prompt {
                self.compress_roles.iter().filter(|r| r != &"system").cloned().collect()
            } else {
                self.compress_roles.clone()
            },
            skip_rules: self.skip_rules.clone(),
            min_message_length: self.min_message_length,
        }
    }
}

/// Resolved selection (mode + why), for the response meta header.
/// Resolved selection (mode + why), for the response meta header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSource {
    Off,
    RequestHeader,
    AutoTrigger,
    Default,
    Skipped,
}

/// Estimate a request body for compression selection and stats. The original
/// serializes the complete body before applying its chars/4 heuristic, so this
/// includes system prompts, tools, input fields, and JSON structure.
pub fn estimate_body_tokens(body: &Value) -> i64 {
    estimate::estimate_value_tokens(body)
}

/// Estimate only message text for routing context checks. Compression stats
/// intentionally use `estimate_body_tokens`, which includes the full request.
pub fn estimate_message_tokens(body: &Value) -> i64 {
    let Some(arr) = body.get("messages").and_then(|m| m.as_array()) else {
        return 0;
    };
    arr.iter()
        .map(|m| {
            let c = m.get("content");
            match c {
                Some(Value::String(s)) => estimate::estimate_tokens(s),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .map(estimate::estimate_tokens)
                    .sum(),
                _ => 0,
            }
        })
        .sum()
}

/// Resolve the effective mode (parity: `resolveBasePlan`).
pub fn resolve_mode(config: &CompressionConfig, header: Option<&str>, estimated_tokens: i64) -> (CompressionMode, PlanSource) {
    // 1. master off = hard kill
    if !config.enabled {
        return (CompressionMode::Off, PlanSource::Off);
    }
    // 2. explicit request header wins over every operator layer
    if let Some(h) = header.map(str::trim).filter(|h| !h.is_empty()) {
        let lower = h.to_ascii_lowercase();
        if lower == "off" {
            return (CompressionMode::Off, PlanSource::RequestHeader);
        }
        if lower == "default" {
            return (config.default_mode, PlanSource::RequestHeader);
        }
        if let Some(m) = CompressionMode::parse(&lower) {
            return (m, PlanSource::RequestHeader);
        }
        // unknown header values fall through (never error)
    }
    // 3. auto-trigger (large prompt)
    if config.auto_trigger_tokens > 0 && estimated_tokens >= config.auto_trigger_tokens {
        return (config.auto_trigger_mode, PlanSource::AutoTrigger);
    }
    (config.default_mode, PlanSource::Default)
}


/// Result of applying compression to a request body.
pub struct Applied {
    pub body: Value,
    pub mode: CompressionMode,
    pub source: PlanSource,
    pub stats: Option<CompressionStats>,
    /// `X-OmniRoute-Compression` response header value
    pub response_header: Option<String>,
}

/// Apply compression to an inbound chat body (pre-upstream-translation).
/// Engines operate on the `messages` array; claude `system` field passes
/// through untouched (the original preserves the system prompt separately).
pub fn apply(body: &Value, config: &CompressionConfig, header: Option<&str>) -> Applied {
    let (mode, source) = resolve_mode(config, header, estimate_body_tokens(body));
    if mode == CompressionMode::Off {
        return Applied {
            body: body.clone(),
            mode,
            source,
            stats: None,
            response_header: None,
        };
    }

    let messages_key = body.get("messages").is_some();
    if !messages_key {
        // responses/unknown shapes: engine no-op (documented divergence)
        return Applied {
            body: body.clone(),
            mode,
            source,
            stats: None,
            response_header: Some(format!("{}; source=skipped", mode.as_str())),
        };
    }

    let mut out = body.clone();
    let mut stats: Option<CompressionStats> = match mode {
        CompressionMode::Lite => {
            let opts = lite::LiteOptions {
                preserve_system_prompt: config.preserve_system_prompt,
                compress_tool_results: true,
                supports_vision: None,
            };
            let (msgs, stats, _) = lite::apply_lite_compression(&body["messages"], &opts);
            out["messages"] = msgs;
            stats
        }
        CompressionMode::Standard => {
            let (msgs, stats) = caveman::caveman_compress(&body["messages"], &config.caveman_config());
            out["messages"] = msgs;
            Some(stats)
        }
        CompressionMode::Ultra => {
            let uc = ultra::UltraConfig {
                compression_rate: config.ultra_compression_rate,
                min_score_threshold: config.ultra_min_score,
                preserve_system_prompt: config.preserve_system_prompt,
                max_tokens_per_message: 0,
            };
            let (msgs, stats) = ultra::ultra_compress(&body["messages"], &uc);
            out["messages"] = msgs;
            Some(stats)
        }
        CompressionMode::Aggressive => {
            let ac = aggressive::AggressiveConfig {
                preserve_system_prompt: config.preserve_system_prompt,
                summarizer_enabled: true,
                max_tokens_per_message: config.aggressive_max_tokens_per_message,
                min_savings_threshold: config.aggressive_min_savings,
                ..Default::default()
            };
            let (msgs, stats) = aggressive::compress_aggressive(&body["messages"], &ac);
            out["messages"] = msgs;
            Some(stats)
        }
        CompressionMode::Rtk => {
            let rc = rtk::RtkConfig { max_lines_per_result: config.rtk_max_lines, enabled_filters: true };
            let (msgs, stats) = rtk::rtk_compress(&body["messages"], &rc);
            out["messages"] = msgs;
            Some(stats)
        }
        CompressionMode::Off => unreachable!(),
    };

    if let Some(s) = stats.as_mut() {
        let original_tokens = estimate_body_tokens(body);
        let compressed_tokens = estimate_body_tokens(&out);
        s.original_tokens = original_tokens;
        s.compressed_tokens = compressed_tokens;
        s.savings_percent = if original_tokens > 0 {
            ((original_tokens - compressed_tokens) as f64 / original_tokens as f64 * 10000.0).round() / 100.0
        } else {
            0.0
        };
    }

    let mut response_header = format!("{}; source={}", mode.as_str(), source.as_str());
    if let Some(s) = &stats {
        if let Some(annotation) = format_compression_annotation(s) {
            response_header.push_str("; ");
            response_header.push_str(&annotation);
        } else if s.original_tokens > 0 {
            response_header.push_str(&format!("; tokens={}->{}", s.original_tokens, s.compressed_tokens));
        }
    }
    Applied {
        body: out,
        mode,
        source,
        stats,
        response_header: Some(response_header),
    }
}

/// Build the bounded ASCII annotation used by the original response header.
fn format_compression_annotation(stats: &CompressionStats) -> Option<String> {
    if stats.rules_applied.is_empty() {
        return None;
    }

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for rule in &stats.rules_applied {
        let safe = rule
            .chars()
            .map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { '?' })
            .collect::<String>();
        *counts.entry(safe).or_default() += 1;
    }
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|(a_name, a_count), (b_name, b_count)| {
        b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
    });

    let prefix = format!("tokens={}->{}; rules: ", stats.original_tokens, stats.compressed_tokens);
    let suffix = ", ...";
    let mut parts = Vec::new();
    let mut bytes = prefix.len();
    for (name, count) in sorted {
        let part = format!("{}x{}", name, count);
        let separator = if parts.is_empty() { "" } else { ", " };
        if bytes + separator.len() + part.len() > 768 - suffix.len() {
            if parts.is_empty() {
                return None;
            }
            return Some(format!("{}{}{}", prefix, parts.join(", "), suffix));
        }
        let part_len = part.len();
        parts.push(part);
        bytes += separator.len() + part_len;
    }
    Some(format!("{}{}", prefix, parts.join(", ")))
}

impl PlanSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanSource::Off => "off",
            PlanSource::RequestHeader => "request-header",
            PlanSource::AutoTrigger => "auto-trigger",
            PlanSource::Default => "default",
            PlanSource::Skipped => "skipped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            default_mode: CompressionMode::Lite,
            ..Default::default()
        }
    }

    #[test]
    fn master_off_is_hard_kill() {
        let mut c = config();
        c.enabled = false;
        let (m, s) = resolve_mode(&c, Some("ultra"), 100000);
        assert_eq!(m, CompressionMode::Off);
        assert_eq!(s, PlanSource::Off);
    }

    #[test]
    fn header_wins_over_default() {
        let c = config();
        let (m, s) = resolve_mode(&c, Some("rtk"), 100);
        assert_eq!(m, CompressionMode::Rtk);
        assert_eq!(s, PlanSource::RequestHeader);
        // off via header
        let (m, _) = resolve_mode(&c, Some("off"), 100);
        assert_eq!(m, CompressionMode::Off);
        // unknown header falls through to default
        let (m, s) = resolve_mode(&c, Some("bogus"), 100);
        assert_eq!(m, CompressionMode::Lite);
        assert_eq!(s, PlanSource::Default);
    }

    #[test]
    fn auto_trigger_precedes_default() {
        let mut c = config();
        c.auto_trigger_tokens = 100;
        c.auto_trigger_mode = CompressionMode::Ultra;
        let (m, s) = resolve_mode(&c, None, 5000);
        assert_eq!(m, CompressionMode::Ultra);
        assert_eq!(s, PlanSource::AutoTrigger);
        // below trigger → default
        let (m, s) = resolve_mode(&c, None, 10);
        assert_eq!(m, CompressionMode::Lite);
        assert_eq!(s, PlanSource::Default);
    }

    #[test]
    fn lite_applies_to_request_body() {
        let body = json!({"model": "m", "messages": [
            {"role": "user", "content": "please read the thing, thanks, the tool output follows and it matters a lot for the answer that the engine gives me today so keep it short anyway"}
        ]});
        let applied = apply(&body, &config(), Some("lite"));
        assert_eq!(applied.mode, CompressionMode::Lite);
        assert!(applied.response_header.is_some());
    }

    #[test]
    fn caveman_standard_via_header() {
        let body = json!({"messages": [{"role": "user", "content": "thanks, sure, absolutely please go ahead and fix the login bug quickly, the session token expiry breaks the automated nightly tests every single time and it is really quite extremely annoying to deal with today"}]});
        let applied = apply(&body, &config(), Some("standard"));
        let text = applied.body["messages"][0]["content"].as_str().unwrap();
        assert!(!text.contains("thanks"), "caveman applied: {text}");
        assert!(applied.stats.is_some());
        assert!(applied.stats.as_ref().unwrap().savings_percent > 0.0);
    }

    #[test]
    fn off_mode_noop() {
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let applied = apply(&body, &config(), Some("off"));
        assert_eq!(applied.body, body);
        assert!(applied.response_header.is_none());
    }
}
