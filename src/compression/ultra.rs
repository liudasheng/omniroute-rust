//! Ultra compression — Tier-A heuristic token pruner (parity:
//! `open-sse/services/compression/ultraHeuristic.ts` + `ultra.ts`).
//!
//! Scores tokens by information density and prunes the lowest-scored tokens
//! to hit a keep-rate target. Polarity/modality words are never prunable
//! (#13454 parity), and preserved structures (code/URLs/paths) are
//! tombstoned so only prose is ever pruned.

use crate::compression::caveman::CompressionStats;
use crate::compression::estimate::estimate_tokens;
use crate::compression::preserve::extract_preserved_blocks;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashSet;

static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        // do/does/did removed — polarity carriers (#13454)
        "will", "would", "could", "may", "might", "shall", "dare", "ought", "used",
        "i", "we", "you", "he", "she", "it", "they", "me", "us", "him", "her", "them",
        "my", "our", "your", "his", "its", "their", "this", "that", "these", "those",
        "and", "but", "or", "for", "yet", "so", "as", "at", "by", "in", "of", "on", "to",
        "up", "via", "with", "from", "into",
    ]
    .into_iter()
    .collect()
});

static POLARITY_WORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "never", "always", "no", "not", "nor", "must", "shall", "shall not",
        "do", "does", "did", "don't", "doesn't", "didn't", "can", "cannot", "can't",
        "should", "shouldn't", "need", "needs", "mustn't", "won't", "wouldn't",
        "could", "couldn't",
    ]
    .into_iter()
    .collect()
});

/// force-preserve: digits, URLs, path/qualifier chars, error markers, fences
/// (parity: FORCE_PRESERVE_RE).
fn force_preserve(token: &str) -> bool {
    if token.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    if token.contains("http") {
        return true;
    }
    if token.chars().any(|c| matches!(c, '.' | '_' | '/' | '\\')) {
        return true;
    }
    token.contains("Error:") || token.contains("Exception:") || token.contains("```")
}

/// Score a token 0.0 (prune candidate) .. 1.0 (must keep) — parity scoreToken.
pub fn score_token(token: &str) -> f64 {
    if force_preserve(token) {
        return 1.0;
    }
    let lower = token.to_lowercase();
    if POLARITY_WORDS.contains(lower.as_str()) {
        return 1.0;
    }
    if STOPWORDS.contains(lower.as_str()) {
        return 0.1;
    }
    if token.chars().count() <= 2 {
        return 0.2;
    }
    if token.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return 0.8; // proper nouns / identifiers
    }
    if token.chars().count() >= 6 {
        return 0.7;
    }
    0.5
}

/// Prune tokens to achieve the target keep rate — parity `pruneByScore`.
pub fn prune_by_score(text: &str, keep_rate: f64, min_score: f64) -> String {
    if text.is_empty() || keep_rate >= 1.0 {
        return text.to_string();
    }

    // split into (whitespace-run | word) tokens preserving whitespace
    let mut tokens: Vec<(bool, String)> = Vec::new(); // (is_whitespace, text)
    let mut current = String::new();
    let mut current_ws = false;
    for c in text.chars() {
        let ws = c.is_whitespace();
        if ws != current_ws && !current.is_empty() {
            tokens.push((current_ws, std::mem::take(&mut current)));
            current_ws = ws;
        }
        if tokens.is_empty() || tokens.last().map(|(w, _)| *w) != Some(ws) || current.is_empty() {
            current_ws = ws;
        }
        current.push(c);
    }
    if !current.is_empty() {
        tokens.push((current_ws, current));
    }

    let word_indexes: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, (ws, _))| !*ws)
        .map(|(i, _)| i)
        .collect();
    let word_count = word_tokens_total(&tokens);
    let target_keep = ((word_count as f64 * keep_rate).ceil() as i64).max(0) as usize;

    // score word tokens, prune lowest scores below minScore first
    let mut scored: Vec<(usize, f64)> = word_indexes
        .into_iter()
        .map(|i| (i, score_token(&tokens[i].1)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut to_prune: HashSet<usize> = HashSet::new();
    let mut pruned = 0usize;
    for (i, score) in &scored {
        if pruned as i64 >= (word_count as i64 - target_keep as i64) {
            break;
        }
        if *score < min_score {
            to_prune.insert(*i);
            pruned += 1;
        }
    }

    let mut out = String::new();
    for (idx, (ws, text)) in tokens.iter().enumerate() {
        if *ws || !to_prune.contains(&idx) {
            out.push_str(text);
        }
    }
    // collapse runs of spaces/tabs (NOT newlines — #13454)
    let re = fancy_regex::Regex::new(r"[ \t]{2,}").unwrap();
    let out = re.replace_all(&out, " ").to_string();
    out.trim().to_string()
}

fn word_tokens_total(tokens: &[(bool, String)]) -> usize {
    tokens.iter().filter(|(ws, _)| !*ws).count()
}


/// Prune PROSE only — tombstone preserved structures, prune the rest,
/// re-stitch verbatim (parity: `pruneProseOnly`).
fn prune_prose_only(text: &str, rate: f64, min_score: f64) -> String {
    let (masked, blocks) = extract_preserved_blocks(text, &[]);
    if blocks.is_empty() {
        return prune_by_score(text, rate, min_score);
    }
    let ph_re = fancy_regex::Regex::new("\u{0}OMNI_CAVEMAN\\d+\u{0}").unwrap();
    let mut out = String::new();
    let mut last = 0usize;
    while let Ok(Some(m)) = ph_re.find_from_pos(&masked, last) {
        let (st, en) = (m.start(), m.end());
        out.push_str(&prune_by_score(&masked[last..st], rate, min_score));
        let placeholder = &masked[st..en];
        if let Some(b) = blocks.iter().find(|b| b.placeholder == placeholder) {
            out.push_str(&b.content); // verbatim — never pruned
        }
        last = en;
    }
    out.push_str(&prune_by_score(&masked[last..], rate, min_score));
    out
}

pub struct UltraConfig {
    pub compression_rate: f64,
    pub min_score_threshold: f64,
    pub preserve_system_prompt: bool,
    /// skip messages estimated ≤ this many tokens (0 = prune everything)
    pub max_tokens_per_message: i64,
}

impl Default for UltraConfig {
    fn default() -> Self {
        Self { compression_rate: 0.5, min_score_threshold: 0.3, preserve_system_prompt: true, max_tokens_per_message: 0 }
    }
}

const COMPRESSED_PREFIX: &str = "[COMPRESSED:";

/// Apply the ultra heuristic to an openai-shaped messages array
/// (parity: `ultraCompressHeuristic`; the SLM tier is optional in the original
/// and always falls back to this heuristic, so the heuristic IS the Rust
/// implementation of ultra).
pub fn ultra_compress(messages: &Value, config: &UltraConfig) -> (Value, CompressionStats) {
    let empty: Vec<Value> = Vec::new();
    let arr = messages.as_array().unwrap_or(&empty);
    let mut original_chars = 0usize;
    let mut compressed_chars = 0usize;

    let new_messages: Vec<Value> = arr
        .iter()
        .map(|msg| {
            if config.preserve_system_prompt
                && msg.get("role").and_then(|r| r.as_str()) == Some("system")
            {
                return msg.clone();
            }
            let mut msg = msg.clone();
            match msg.get_mut("content") {
                Some(Value::String(s)) => {
                    if s.is_empty() || s.starts_with(COMPRESSED_PREFIX) {
                        return msg;
                    }
                    if config.max_tokens_per_message > 0
                        && estimate_tokens(s) <= config.max_tokens_per_message
                    {
                        return msg;
                    }
                    let _o = s.chars().count();
                    let pruned = prune_prose_only(s, config.compression_rate, config.min_score_threshold);
                    original_chars += s.chars().count();
                    compressed_chars += pruned.chars().count();
                    msg["content"] = Value::String(pruned);
                }
                Some(Value::Array(parts)) => {
                    for p in parts.iter_mut() {
                        if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                                if text.is_empty() || text.starts_with(COMPRESSED_PREFIX) {
                                    continue;
                                }
                                if config.max_tokens_per_message > 0
                                    && estimate_tokens(text) <= config.max_tokens_per_message
                                {
                                    continue;
                                }
                                let o = text.chars().count();
                                let pruned = prune_prose_only(text, config.compression_rate, config.min_score_threshold);
                                original_chars += o;
                                compressed_chars += pruned.chars().count();
                                p["text"] = Value::String(pruned);
                            }
                        }
                    }
                }
                _ => {}
            }
            msg
        })
        .collect();

    let original_tokens = (original_chars as f64 / 4.0).ceil() as i64;
    let compressed_tokens = (compressed_chars as f64 / 4.0).ceil() as i64;
    let savings = if original_tokens > 0 {
        ((original_tokens - compressed_tokens) as f64 / original_tokens as f64 * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    let stats = CompressionStats {
        original_tokens,
        compressed_tokens,
        savings_percent: savings,
        techniques_used: vec!["ultra-heuristic-pruning".to_string()],
        rules_applied: Vec::new(),
    };
    (Value::Array(new_messages), stats)
}

/// Apply ultra to a whole request body.
pub fn ultra_compress_body(body: &Value, config: &UltraConfig) -> (Value, CompressionStats) {
    if body.get("messages").is_none() {
        let empty_msgs = Value::Array(Vec::new());
        let (_, stats) = ultra_compress(&empty_msgs, config);
        return (body.clone(), stats);
    }
    let mut out = body.clone();
    let (msgs, stats) = ultra_compress(&body["messages"], config);
    out["messages"] = msgs;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stopwords_pruned_meaning_words_kept() {
        let text = "the quick brown fox must never jump over the lazy dog again because the tests always fail today";
        let pruned = prune_by_score(text, 0.5, 0.3);
        assert!(!pruned.contains("the quick"), "stopwords pruned: {pruned}");
        assert!(pruned.contains("must"), "polarity kept: {pruned}");
        assert!(pruned.contains("never"), "polarity kept: {pruned}");
        assert!(pruned.contains("always") || pruned.contains("again") || pruned.contains("fail"));
    }

    #[test]
    fn code_and_numbers_never_pruned() {
        let text = "the implementation uses the config value 4096 and the endpoint https://a.b/c for the sync";
        let pruned = prune_by_score(text, 0.3, 0.3);
        assert!(pruned.contains("4096"), "digits kept: {pruned}");
        assert!(pruned.contains("https://a.b/c"), "url kept: {pruned}");
    }

    #[test]
    fn newlines_preserved_in_ultra() {
        let text = "first line of prose\n- bullet one\n- bullet two\nthird line of the prose here";
        let pruned = prune_by_score(text, 0.5, 0.3);
        assert_eq!(pruned.matches('\n').count(), text.matches('\n').count(), "newlines preserved");
    }

    #[test]
    fn ultra_skips_system_and_short() {
        let msgs = json!([
            {"role": "system", "content": "the system prompt is the system prompt is the system prompt is the system prompt is the system prompt and more filler words here to be long"},
            {"role": "user", "content": "short"}
        ]);
        let (out, _) = ultra_compress(&msgs, &UltraConfig::default());
        assert_eq!(out[0]["content"], msgs[0]["content"]);
        assert_eq!(out[1]["content"], msgs[1]["content"]);
    }

    #[test]
    fn ultra_compresses_long_prose() {
        let text = "the implementation of the database migration uses the config settings and the authentication layer and it was created for the purpose of testing whether the pruning algorithm removes filler tokens from the long message body here today".repeat(3);
        let msgs = json!([{"role": "user", "content": text}]);
        let (out, stats) = ultra_compress(&msgs, &UltraConfig { preserve_system_prompt: false, ..Default::default() });
        let pruned = out[0]["content"].as_str().unwrap();
        assert!(pruned.chars().count() < text.chars().count());
        assert_eq!(stats.techniques_used, vec!["ultra-heuristic-pruning"]);
        assert!(stats.savings_percent > 0.0);
    }

    #[test]
    fn preserved_blocks_verbatim_in_ultra() {
        let text = "please look at the code `fn_tick_rate_ms` and the value 30000 in the config file for the timeouts and retry policy details of the system that we discussed earlier today at length".repeat(2);
        let msgs = json!([{"role": "user", "content": text}]);
        let (out, _) = ultra_compress(&msgs, &UltraConfig { preserve_system_prompt: false, ..Default::default() });
        let pruned = out[0]["content"].as_str().unwrap();
        assert!(pruned.contains("`fn_tick_rate_ms`"), "inline code verbatim: {pruned}");
        assert!(pruned.contains("30000"), "numbers kept: {pruned}");
    }
}
