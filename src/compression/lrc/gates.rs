//! LRC gates: token estimation, protected-fragment extraction/verification and
//! structural invariants (the "do not break the session" contract).
//!
//! Everything here is **content-intrinsic** (never depends on turn counters),
//! so a block always renders to the same bytes no matter which turn it is
//! replayed in — the property that keeps upstream prompt caches valid.

use crate::compression::estimate::estimate_tokens;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Calibrated token estimate: ASCII prose ~ 4 chars/token, CJK ~ 1 token/char,
/// other non-ASCII ~ 2 chars/token. The gateway's `estimate_tokens` (utf16/4)
/// under-reports CJK-heavy and JSON-heavy payloads by ~30%; the ledger uses this
/// one so savings percentages are not computed on a biased denominator.
pub fn estimate_tokens_mixed(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    let ascii_tokens = (ascii + 3) / 4;
    let other_tokens = (other + 1) / 2;
    (ascii_tokens + cjk + other_tokens) as i64
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2EFF | 0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F)
}

/// Both the raw (gateway-compatible) and the calibrated estimate of a string.
pub fn estimate_pair(text: &str) -> (i64, i64) {
    (estimate_tokens(text), estimate_tokens_mixed(text))
}

/// Estimate of a JSON value in calibrated tokens.
pub fn value_tokens(v: &Value) -> i64 {
    match serde_json::to_string(v) {
        Ok(s) => estimate_tokens_mixed(&s),
        Err(_) => 0,
    }
}

/// Stable short hash of a block, used by pointers and idempotency markers.
pub fn short_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let d = h.finalize();
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Markers that prove a block was already compressed by us (idempotency).
/// A compressed block must never be compressed twice: the second pass would
/// compound losses and change the rendering of an already-cached prefix.
pub fn is_marked(text: &str) -> bool {
    text.contains("[omni-elided") || text.contains("[reasoning elided") || text.contains("[args elided")
}

// ---------------------------------------------------------------------------
// Protected fragments
// ---------------------------------------------------------------------------

/// A fragment of the original block that must survive compression verbatim.
#[derive(Debug, Clone)]
pub struct Protected {
    /// lines that carry signal (errors, stack frames, diffs, exit codes)
    pub lines: Vec<String>,
    /// substrings (paths, URLs, hashes) that must remain present
    pub tokens: Vec<String>,
}

const CRITICAL_LINE_MARKERS: &[&str] = &[
    "error", "Error", "ERROR", "panic", "Panic", "PANIC", "Traceback", "FAILED", "Failed",
    "failed", "warning:", "Warning:", "WARNING", "assert", "Assertion", "segmentation fault",
    "Segmentation fault", "exit code", "exit status", "not found", "NotFound", "denied",
    "Permission denied", "refused", "timed out", "timeout", "Aborted", "core dumped",
    "错误", "失败", "异常", "警告", "注意", "无法", "超时", "拒绝",
];

const DIFF_MARKERS: &[&str] = &["diff --git", "+++ ", "--- ", "@@ ", "Index: ", "rename from"];

fn is_critical_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if CRITICAL_LINE_MARKERS.iter().any(|m| line.contains(m)) {
        return true;
    }
    DIFF_MARKERS.iter().any(|m| t.starts_with(m))
}

/// A diagnostic continuation line (`  --> src/lib.rs:12:5`, `   |`, `   ^`).
/// It carries the location of the error above it, so it is protected with it.
fn is_diagnostic_context(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    t.starts_with("-->")
        || t.starts_with("at ")
        || t.starts_with('|')
        || t.starts_with('^')
        || (t.contains(':') && t.split(':').count() >= 2 && t.chars().all(|c| !c.is_control()))
}

/// Extract the fragments that compression must preserve.
///
/// * `lines`: critical lines (errors, diffs, warnings) plus the diagnostic
///   continuation lines that belong to them.
/// * `tokens`: path/URL/hash references **inside those lines only** — a path
///   that occurs in ordinary log prose is not signal, and protecting every
///   path in the block would block compression of the whole block.
pub fn extract_protected(text: &str, max_lines: usize, max_tokens: usize) -> Protected {
    let all: Vec<&str> = text.lines().collect();
    let mut lines: Vec<String> = Vec::new();
    for (i, line) in all.iter().enumerate() {
        if lines.len() >= max_lines {
            break;
        }
        if !is_critical_line(line) {
            continue;
        }
        lines.push((*line).to_string());
        // keep up to two diagnostic continuation lines with the error
        for follow in all.iter().skip(i + 1).take(2) {
            if lines.len() >= max_lines {
                break;
            }
            if is_diagnostic_context(follow) {
                lines.push((*follow).to_string());
            } else {
                break;
            }
        }
    }
    let mut tokens: Vec<String> = Vec::new();
    for line in &lines {
        for word in line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | ',' | '"' | '\'' | '`' | '|' | '^')) {
            if tokens.len() >= max_tokens {
                break;
            }
            let w = word.trim();
            if w.len() < 4 {
                continue;
            }
            let looks_like_path = (w.contains('/') || w.contains('\\')) && w.contains('.');
            let looks_like_url = w.starts_with("http://") || w.starts_with("https://");
            let looks_like_hash = w.len() >= 12 && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if looks_like_path || looks_like_url || looks_like_hash {
                tokens.push(w.to_string());
            }
        }
    }
    tokens.sort();
    tokens.dedup();
    Protected { lines, tokens }
}

/// Every protected fragment must still appear in the compressed text.
pub fn protected_survives(protected: &Protected, compressed: &str) -> bool {
    protected.lines.iter().all(|l| compressed.contains(l.trim()))
        && protected.tokens.iter().all(|t| compressed.contains(t))
}

// ---------------------------------------------------------------------------
// Structural invariants
// ---------------------------------------------------------------------------

/// Snapshot of the parts of a request body that compression is not allowed to
/// change. Compared after the pipeline ran; any mismatch reverts the block or
/// the whole request (fail-open).
#[derive(Debug, Default, PartialEq)]
pub struct Invariants {
    src_len: usize,
    system: Vec<String>,
    last_user: Option<String>,
    call_ids: Vec<String>,
    /// (call id, tool name) pairs, in order
    calls: Vec<(String, String)>,
    /// tool_call_id of every tool/function result message, in order
    result_ids: Vec<String>,
}

fn assistant_calls(msg: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for c in arr {
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = c
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push((id, name));
        }
    }
    // Anthropic-style tool_use blocks inside an assistant message
    if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            if p.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                out.push((id, name));
            }
        }
    }
    out
}

fn result_id(msg: &Value) -> Option<String> {
    if let Some(id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    // Claude: user message containing tool_result blocks
    let parts = msg.get("content").and_then(|c| c.as_array())?;
    for p in parts {
        if p.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            if let Some(id) = p.get("tool_use_id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    None
}

impl Invariants {
    pub fn snapshot(messages: &[Value]) -> Self {
        let mut inv = Invariants {
            src_len: messages.len(),
            ..Default::default()
        };
        for m in messages {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            match role {
                "system" | "developer" => {
                    inv.system.push(serde_json::to_string(m).unwrap_or_default());
                }
                "user" => {
                    inv.last_user = Some(serde_json::to_string(m).unwrap_or_default());
                }
                "assistant" => {
                    inv.calls.extend(assistant_calls(m));
                }
                "tool" | "function" => {
                    if let Some(id) = result_id(m) {
                        inv.result_ids.push(id);
                    }
                }
                _ => {}
            }
        }
        inv.call_ids = inv.calls.iter().map(|(id, _)| id.clone()).collect();
        inv
    }

    /// Compare after compression. Returns the list of violated invariants
    /// (empty = the compression preserved the conversation skeleton).
    pub fn violations(&self, after: &[Value]) -> Vec<&'static str> {
        let mut bad = Vec::new();
        let now = Invariants::snapshot(after);
        if self.src_len != now.src_len {
            bad.push("message-count");
        }
        if self.system != now.system {
            bad.push("system-changed");
        }
        if self.last_user != now.last_user {
            bad.push("last-user-changed");
        }
        if self.calls.len() != now.calls.len() {
            bad.push("tool-call-count");
        } else if self.calls.iter().zip(now.calls.iter()).any(|(a, b)| a.0 != b.0 || a.1 != b.1) {
            bad.push("tool-call-identity");
        }
        if self.result_ids != now.result_ids {
            bad.push("tool-result-pairing");
        }
        bad
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mixed_estimate_penalises_cjk() {
        let ascii = "the quick brown fox jumps over the lazy dog";
        let cjk = "这是一段中文文本用于估算令牌数量";
        let (a_raw, a_mix) = estimate_pair(ascii);
        let (c_raw, c_mix) = estimate_pair(cjk);
        assert!(a_raw > 0 && c_raw > 0);
        // Raw estimator divides by four regardless of script; calibrated one
        // counts CJK at roughly one token per character.
        assert!(c_mix > c_raw * 2, "cjk calibrated {c_mix} vs raw {c_raw}");
        assert!(a_mix >= a_raw / 2 && a_mix <= a_raw * 2);
    }

    #[test]
    fn protected_fragments_are_detected() {
        let text = "compiling\nerror[E0432]: unresolved import `x`\n  --> src/main.rs:4:5\nwarning: unused value\nhttp://example.com/a/b\n";
        let p = extract_protected(text, 20, 20);
        assert!(p.lines.iter().any(|l| l.contains("E0432")));
        assert!(p.lines.iter().any(|l| l.contains("warning:")));
        assert!(p.tokens.iter().any(|t| t.contains("src/main.rs")));
        assert!(p.tokens.iter().any(|t| t.starts_with("http")));
        assert!(protected_survives(&p, text));
        assert!(!protected_survives(&p, "compiling\n"));
    }

    #[test]
    fn invariants_catch_broken_sessions() {
        let body = json!([
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "bash", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "user", "content": "next"}
        ]);
        let msgs = body.as_array().unwrap().clone();
        let inv = Invariants::snapshot(&msgs);
        assert!(inv.violations(&msgs).is_empty());

        // drop a tool result
        let mut broken = msgs.clone();
        broken.remove(3);
        assert!(inv.violations(&broken).contains(&"tool-result-pairing"));

        // rewrite the system prompt
        let mut broken2 = msgs.clone();
        broken2[0]["content"] = json!("sys2");
        assert!(inv.violations(&broken2).contains(&"system-changed"));

        // rewrite the last user message
        let mut broken3 = msgs.clone();
        broken3[4]["content"] = json!("different");
        assert!(inv.violations(&broken3).contains(&"last-user-changed"));
    }
}
