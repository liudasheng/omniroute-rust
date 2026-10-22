//! LRC gates: token estimation, protected-fragment extraction/verification and
//! structural invariants (the "do not break the session" contract).
//!
//! Everything here is **content-intrinsic** (never depends on turn counters),
//! so a block always renders to the same bytes no matter which turn it is
//! replayed in — the property that keeps upstream prompt caches valid.

use crate::compression::estimate::estimate_tokens;
use std::collections::HashMap;
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
    // The marker shapes this pipeline writes today are `[omni-elided …]`,
    // `[reasoning elided …]` and `[args elided …]`. Older builds emitted a
    // leading `...` inside the bracket (`[... omni-elided …]`), so that form
    // is still accepted: a block compressed by an earlier build must not be
    // compressed a second time, and the two shapes must never disagree.
    //
    // The bracket markers are matched at the **start of a line**. A bare
    // substring test also fires on a block that merely *quotes* a marker —
    // an agent reading the filter's own source code is exactly that case,
    // and such a block would silently never be compressed. On the real
    // corpus that mis-detection cost 6.5% of the payload.
    let bracket_marker = text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("[omni-elided ")
            || t.starts_with("[omni-elided-json ")
            || t.starts_with("[... omni-elided ")
            || t.starts_with("[… omni-elided ")
            || t.starts_with("[reasoning elided ")
            || t.starts_with("[args elided ")
    });
    bracket_marker
        // The retired-argument marker is a JSON field, not a bracket note.
        || text.contains("\"_omni\":\"args-elided\"")
        || text.contains("\"_omni\": \"args-elided\"")
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

/// Markers that mean "this line *is* a diagnostic".
///
/// They are matched as a *word* at a diagnostic position, not as a bare
/// substring: "0 failed" in a test summary is not an error line, and
/// treating it as one let ordinary progress prose consume the whole
/// protection budget, squeezing the real error out.
/// Phrases that mark a diagnostic wherever they occur in the line.
///
/// They are multi-word (or otherwise distinctive), so a case-insensitive
/// search cannot fire on ordinary prose the way a bare keyword would.
/// Dropping them from the strong set left `exit code 1`, `timed out` and
/// `not found: …` unprotected — on the real corpus that was 22.4% of the
/// tool-result blocks.
const STRONG_CRITICAL_PHRASES: &[&str] = &[
    "segmentation fault", "core dumped", "assertion failed",
    "uncaught exception", "unhandled exception", "syntaxerror", "typeerror",
    "referenceerror", "stack trace:", "traceback (most recent call last)",
    "panic at", "panicked at", "exit code", "exit status", "not found", "timed out",
    "timedout", "致命错误",
];

/// Keywords that mark a diagnostic when they are the head word of the line.
///
/// Matched as a word boundary (`error[E0432]:`, `ERROR:`, `TimeoutError:`),
/// never as a substring, so `erroring` and `handled error:` are prose.
const STRONG_KEYWORDS: &[&str] = &[
    "error", "fatal", "panic", "panicked", "traceback", "exception", "assertion",
    "assertionerror", "timeouterror", "错误", "失败", "异常",
];

/// Drop the leading `thread '…'` / `task '…'` run so a Rust panic line can be
/// recognised by its keyword.
fn strip_quoted_prefix(head: &str) -> String {
    for marker in ["thread '", "task '"] {
        if let Some(rest) = head.strip_prefix(marker) {
            if let Some(i) = rest.find('\'') {
                return rest[i + 1..].trim_start().to_string();
            }
        }
    }
    head.to_string()
}

/// Markers that make a line *suspicious* but not certainly a diagnostic.
///
/// They are only used after the strong ones, and never displace them.
const WEAK_CRITICAL_MARKERS: &[&str] = &[
    "error", "panic", "traceback", "failed", "warning", "assert", "not found",
    "denied", "refused", "timed out", "timeout", "aborted", "exit code", "exit status",
    "错误", "失败", "异常", "警告", "注意", "无法", "超时", "拒绝",
];

const DIFF_MARKERS: &[&str] = &["diff --git", "+++ ", "--- ", "@@ ", "Index: ", "rename from"];

/// True when the line carries a strong diagnostic signal.
fn is_strong_critical_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // A diagnostic keyword must sit at a *diagnostic position*: the start of
    // the line, or right after a path/line-number prefix. A bare substring
    // test would make "handled error: retry" and "no error: all good" hard
    // errors, and since the hard-error set is budget-free those benign
    // lines would silently disable compression for the whole block.
    // The head word is everything before the first colon: `error[E0432]:`
    // yields "error[e0432]", `ERROR:` yields "error", and "handled error:"
    // yields "handled error" — which is prose, not a diagnostic.
    let head = t
        .split(':')
        .next()
        .unwrap_or(t)
        .trim()
        .to_ascii_lowercase();
    // A Rust panic line begins `thread 'main' panicked at …`; strip the
    // quoted run so the keyword is at the head.
    let head_body = strip_quoted_prefix(&head);
    for kw in STRONG_KEYWORDS {
        if head_body.starts_with(kw) {
            let rest = &head_body[kw.len()..];
            // The keyword must be the whole head word: "erroring" is prose.
            if rest.is_empty() || rest.starts_with('[') || rest.starts_with(':') {
                return true;
            }
        }
    }
    // Multi-word phrases cannot plausibly occur in ordinary prose, so a
    // case-insensitive search is safe here — and it catches a diagnostic
    // that is not the head word (`[status: completed, exit code: 1]`,
    // `pnpm was not found`).
    if STRONG_CRITICAL_PHRASES
        .iter()
        .any(|m| t.to_ascii_lowercase().contains(m))
    {
        return true;
    }
    DIFF_MARKERS.iter().any(|m| t.starts_with(m))
}

/// True when the line merely *mentions* a diagnostic word.
///
/// The word must appear as a whole token, otherwise "0 failed" and
/// "error handling" count as failures.
fn is_weak_critical_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || is_strong_critical_line(line) {
        return false;
    }
    WEAK_CRITICAL_MARKERS.iter().any(|m| {
        t.split(|c: char| !c.is_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case(m))
    })
}

/// True when a line carries a diagnostic of any strength.
pub fn is_critical_line(line: &str) -> bool {
    is_strong_critical_line(line) || is_weak_critical_line(line)
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
    // Two passes: the strong diagnostics claim the budget first, so ordinary
    // prose ("0 failed", "no error") can never crowd out the real failure.
    for strong_pass in [true, false] {
        for (i, line) in all.iter().enumerate() {
            if lines.len() >= max_lines {
                break;
            }
            let critical = if strong_pass {
                is_strong_critical_line(line)
            } else {
                is_weak_critical_line(line)
            };
            if !critical {
                continue;
            }
            // A line already collected as a *continuation* of an earlier
            // diagnostic must still contribute its own continuations, so the
            // scan is not skipped — only the duplicate push is.
            if !lines.iter().any(|l| l.trim() == line.trim()) {
                lines.push((*line).to_string());
            }
            // keep up to two diagnostic continuation lines with the error
            for follow in all.iter().skip(i + 1).take(2) {
                if lines.len() >= max_lines {
                    break;
                }
                if is_diagnostic_context(follow) {
                    if !lines.iter().any(|l| l.trim() == follow.trim()) {
                        lines.push((*follow).to_string());
                    }
                } else {
                    break;
                }
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
    // Document order: the two-pass scan collects the strong diagnostics
    // first, but the block must *read* in the order it was written — a
    // warning that happened before an error must stay before it.
    let order: HashMap<&str, usize> = all
        .iter()
        .enumerate()
        .map(|(i, l)| (l.trim(), i))
        .collect();
    lines.sort_by_key(|l| order.get(l.trim()).copied().unwrap_or(usize::MAX));
    Protected { lines, tokens }
}

/// Every protected fragment must still appear in the compressed text.
pub fn protected_survives(protected: &Protected, compressed: &str) -> bool {
    protected.lines.iter().all(|l| compressed.contains(l.trim()))
        && protected.tokens.iter().all(|t| compressed.contains(t))
}

/// True when a line looks like *source code* rather than a diagnostic.
///
/// A block that merely mentions a diagnostic word inside code — a string
/// literal `"error:"`, a comment, a list of markers — is not reporting a
/// failure. Counting those as hard errors makes the whole block
/// uncompressible: on the real corpus the largest refused block was the
/// filter's own source file, whose marker list spells out "error[",
/// "error:", "error -".
fn looks_like_code(t: &str) -> bool {
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with('#')
        || t.contains('"')
        || t.contains("':")
        || t.ends_with("],")
        || t.ends_with("\",")
        || t.ends_with(");")
}

/// The hard errors a block contains: lines that *are* a diagnostic, together
/// with the `--> file:line` location that points at the cause.
///
/// Unlike the protected set, this list is not subject to a budget: a block
/// that reports an error must keep the error body *and* its location. The
/// protected set answers "which fragments did we decide to keep"; this one
/// answers "may we ship a block whose error vanished".
pub fn hard_errors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if is_strong_critical_line(line) && !looks_like_code(t) {
            out.push(t.to_string());
            continue;
        }
        // A location line only counts when an error was seen before it.
        if !out.is_empty() && (t.starts_with("-->") || t.starts_with("at ")) {
            out.push(t.to_string());
        }
    }
    out
}

/// Do the hard errors of the original survive in the compressed text?
pub fn hard_errors_survive(original: &str, compressed: &str) -> bool {
    hard_errors(original)
        .iter()
        .all(|e| compressed.contains(e))
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

#[cfg(test)]
mod diagnostic_detection_tests {
    use super::*;

    /// The detector must recognise the diagnostic shapes an agent actually
    /// produces. A regression here silently drops protection from real
    /// failures (it was 22.4% of the corpus blocks).
    #[test]
    fn real_diagnostic_shapes_are_detected() {
        let real = [
            "exit code 1",
            "exit status 2",
            "not found: no such file or directory",
            "timed out",
            "thread 'main' panicked at src/lib.rs:9:5",
            "Segmentation fault",
            "致命错误: something went wrong",
            "TimeoutError: timed out",
            "AssertionError: strong_loc missing",
            "error[E0432]: unresolved import",
            "Traceback (most recent call last):",
            "ERROR: connection refused",
            "pnpm was not found",
        ];
        for line in real {
            assert!(
                is_strong_critical_line(line),
                "a real diagnostic is no longer recognised: {line}"
            );
        }
    }

    /// Prose that merely mentions a diagnostic word must not become one.
    #[test]
    fn benign_prose_is_not_a_diagnostic() {
        let benign = [
            "handled error: retry 3 of the batch",
            "no error: everything is fine",
            "the erroring task finished",
            "retrying after the first error handling",
        ];
        for line in benign {
            assert!(
                !is_strong_critical_line(line),
                "benign prose was taken for a diagnostic: {line}"
            );
        }
    }
}
