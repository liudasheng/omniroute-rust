//! RTK compression engine (simplified parity:
//! `open-sse/services/compression/engines/rtk/*`).
//!
//! Full parity notes: the original ships a filter registry per command type
//! (npm/make/docker/test, ...), custom project filters, raw-output pointers
//! and a learn/verify subsystem. This implementation keeps the generic
//! pipeline (ANSI strip, progress-bar line removal, consecutive-duplicate
//! dedupe, head+tail max-lines cap with elision marker, document-read guard
//! #4559) and records applied rules; per-command custom filters remain out of
//! scope (documented in PARITY.md).

use crate::compression::estimate::estimate_tokens;
use serde_json::Value;

pub struct RtkConfig {
    /// max lines kept per tool result (head+tail, elided middle)
    pub max_lines_per_result: usize,
    pub enabled_filters: bool,
}

impl Default for RtkConfig {
    fn default() -> Self {
        Self { max_lines_per_result: 200, enabled_filters: true }
    }
}

fn is_progress_bar_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // percent progress: [####----] 50% | 33% | 12/20 files
    if t.starts_with('[') && (t.contains('%') || t.contains(']')) && t.contains(['#', '=', '-']) {
        return true;
    }
    let barish = t
        .chars()
        .filter(|c| matches!(*c, '#' | '=' | '-' | '|' | '/' | '\\' | '>' | '<' | '_' | '.'))
        .count();
    let total = t.chars().count();
    total > 0 && barish * 100 / total >= 60
}


fn is_document_like_read(text: &str) -> bool {
    // parity #4559: unknown content without command markers or error markers
    !has_command_signature(text) && !has_command_marker(text) && !has_generic_error_markers(text)
}


fn has_generic_error_markers(text: &str) -> bool {
    text.contains("Error:") || text.contains("Exception:") || text.contains("Traceback (most recent call last):")
}



fn has_command_signature(text: &str) -> bool {
    ["npm ", "cargo ", "make ", "docker ", "go build", "pnpm "]
        .iter()
        .any(|c| text.contains(c))
}

fn has_command_marker(text: &str) -> bool {
    text.contains("$ ") || text.contains("> ")
}

/// Head+tail cap with an elided marker (parity: smartTruncate/maxLines).
fn cap_lines(text: &str, max_lines: usize) -> (String, bool) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= max_lines {
        return (text.to_string(), false);
    }
    let head = max_lines * 2 / 3;
    let tail = max_lines - head;
    let elided = lines.len() - head - tail;
    let out = format!(
        "{}\n... [{} lines elided] ...\n{}",
        lines[..head].join("\n"),
        elided,
        lines[lines.len() - tail..].join("\n")
    );
    (out, true)
}

fn dedupe_consecutive_lines(text: &str) -> (String, bool) {
    let mut out: Vec<&str> = Vec::new();
    let mut changed = false;
    for line in text.split('\n') {
        if out.last() == Some(&line) {
            changed = true;
            continue;
        }
        out.push(line);
    }
    (out.join("\n"), changed)
}

/// Apply the simplified RTK engine to tool/assistant command outputs.
pub fn rtk_compress_tool_text(text: &str, cfg: &RtkConfig) -> (String, Vec<String>) {
    let mut techniques: Vec<String> = Vec::new();
    if text.is_empty() {
        return (text.to_string(), techniques);
    }
    let document_like = is_document_like_read(text);
    let mut out = text.to_string();

    // ANSI strip
    if out.contains('\x1b') {
        let mut cleaned = String::with_capacity(out.len());
        let mut chars = out.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                }
                continue;
            }
            cleaned.push(c);
        }
        out = cleaned;
        techniques.push("rtk-ansi-strip".to_string());
    }

    // progress-bar line filter
    let lines: Vec<&str> = out.split('\n').collect();
    let filtered: Vec<&str> = lines.iter().copied().filter(|l| !is_progress_bar_line(l)).collect();
    if filtered.len() != lines.len() {
        out = filtered.join("\n");
        techniques.push("rtk-filter".to_string());
    }

    // consecutive dedupe
    let (deduped, changed) = dedupe_consecutive_lines(&out);
    if changed {
        out = deduped;
        techniques.push("rtk-dedup".to_string());
    }

    // head+tail cap — skipped for document-like reads (#4559)
    if !document_like && cfg.enabled_filters {
        let (capped, changed) = cap_lines(&out, cfg.max_lines_per_result);
        if changed {
            out = capped;
            techniques.push("rtk-cap".to_string());
        }
    }

    (out, techniques)
}


/// Apply RTK to an openai-shaped messages array: tool messages and assistant
/// messages whose content looks like command output.
pub fn rtk_compress(messages: &Value, cfg: &RtkConfig) -> (Value, crate::compression::caveman::CompressionStats) {
    let empty: Vec<Value> = Vec::new();
    let arr = messages.as_array().unwrap_or(&empty);
    let mut techniques: Vec<String> = Vec::new();
    let original_tokens: i64 = arr
        .iter()
        .map(|m| estimate_tokens(m.get("content").and_then(|c| c.as_str()).unwrap_or("")))
        .sum();

    let new_msgs: Vec<Value> = arr
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "tool" && role != "function" {
                return m.clone();
            }
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            if text.is_empty() {
                return m.clone();
            }
            let (compressed, t) = rtk_compress_tool_text(text, cfg);
            techniques.extend(t);
            let mut nm = m.clone();
            nm["content"] = Value::String(compressed);
            nm
        })
        .collect();

    let compressed_tokens: i64 = new_msgs
        .iter()
        .map(|m| estimate_tokens(m.get("content").and_then(|c| c.as_str()).unwrap_or("")))
        .sum();
    let stats = crate::compression::caveman::CompressionStats::compute(
        original_tokens,
        compressed_tokens,
        techniques,
        Vec::new(),
    );
    (Value::Array(new_msgs), stats)
}

/// Apply RTK to a whole request body.
pub fn rtk_compress_body(body: &Value, cfg: &RtkConfig) -> (Value, crate::compression::caveman::CompressionStats) {
    if body.get("messages").is_none() {
        let empty_msgs = Value::Array(Vec::new());
        let (_, stats) = rtk_compress(&empty_msgs, cfg);
        return (body.clone(), stats);
    }
    let mut out = body.clone();
    let (msgs, stats) = rtk_compress(&body["messages"], cfg);
    out["messages"] = msgs;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_lines_filtered() {
        let text = "npm install started\n[####----] 50%\n[######--] 75%\nadded 128 packages in 3s";
        let (out, techniques) = rtk_compress_tool_text(text, &RtkConfig::default());
        assert!(!out.contains("50%"));
        assert!(out.contains("added 128 packages"));
        assert!(techniques.iter().any(|t| t == "rtk-filter"));
    }

    #[test]
    fn consecutive_dupes_removed() {
        let text = "step a\nstep a\nstep a\nstep b";
        let (out, techniques) = rtk_compress_tool_text(text, &RtkConfig::default());
        assert_eq!(out.matches("step a").count(), 1);
        assert!(techniques.iter().any(|t| t == "rtk-dedup"));
    }

    #[test]
    fn long_log_capped() {
        let text = std::iter::once("$ npm run build".to_string())
            .chain((0..500).map(|i| format!("line {i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, techniques) = rtk_compress_tool_text(&text, &RtkConfig::default());
        assert!(out.lines().count() < 500);
        assert!(out.contains("lines elided"));
        assert!(techniques.iter().any(|t| t == "rtk-cap"));
    }

    #[test]
    fn document_read_not_capped() {
        // no command marker + no error markers → document read (#4559)
        let text = (0..400).map(|i| format!("doc line {i} with prose")).collect::<Vec<_>>().join("\n");
        let (out, techniques) = rtk_compress_tool_text(&text, &RtkConfig::default());
        assert!(!out.contains("lines elided"), "document reads keep their middle");
        assert!(techniques.iter().all(|t| t != "rtk-cap"));
    }

    #[test]
    fn ansi_stripped() {
        let text = "\x1b[31merror\x1b[0m: file missing\nresult: 0";
        let (out, techniques) = rtk_compress_tool_text(text, &RtkConfig::default());
        assert!(!out.contains('\x1b'));
        assert!(techniques.iter().any(|t| t == "rtk-ansi-strip"));
    }
}
