//! RTK-lite compression engine (parity: `open-sse/services/compression/lite.ts`
//! — the RTK "minimal" tier techniques applied without command filters).

use crate::compression::estimate::estimate_tokens;
use crate::compression::caveman::CompressionStats;
use serde_json::Value;

/// Collapse 3+ newlines → 2 and trailing spaces per line
/// (parity: `collapseWhitespace`).
pub fn normalize_message_whitespace(content: &str) -> String {
    static RE: once_cell::sync::Lazy<fancy_regex::Regex> =
        once_cell::sync::Lazy::new(|| fancy_regex::Regex::new(r"\n{3,}").unwrap());
    let out = RE.replace_all(content, "\n\n").to_string();
    // strip trailing [ \t] per line
    let mut per_line = String::new();
    for line in out.split('\n') {
        per_line.push_str(line.trim_end());
        per_line.push('\n');
    }
    if content.ends_with('\n') && !out.ends_with('\n') {
        // keep original trailing newline semantics simple: drop the extra
    }
    per_line.trim_end_matches('\n').to_string() + (if content.ends_with('\n') { "\n" } else { "" })
}

/// Dedup identical system prompts (first 200 chars as the key)
/// (parity: `dedupSystemPrompt`).
fn dedup_system_prompt(messages: &[Value], applied: &mut bool) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for msg in messages {
        let is_system = msg.get("role").and_then(|r| r.as_str()) == Some("system");
        let content = msg.get("content").and_then(|c| c.as_str());
        if is_system {
            if let Some(text) = content {
                let key: String = text.trim().chars().take(200).collect();
                if !seen.insert(key) {
                    *applied = true;
                    continue; // drop duplicate system message
                }
            }
        }
        out.push(msg.clone());
    }
    out
}

fn is_word_char(c: Option<char>) -> bool {
    match c {
        Some(c) => !c.is_whitespace(),
        None => false,
    }
}

fn find_whitespace_backward(content: &str, cut_index: usize) -> Option<usize> {
    let chars: Vec<char> = content.chars().collect();
    let window_start = cut_index.saturating_sub(80);
    (window_start..cut_index)
        .rev()
        .find(|i| !is_word_char(chars.get(*i).copied()))

}

fn find_whitespace_forward(content: &str, cut_index: usize) -> Option<usize> {
    let chars: Vec<char> = content.chars().collect();
    let window_end = chars.len().min(cut_index + 80);
    (cut_index..window_end).find(|i| !is_word_char(chars.get(*i).copied()))

}

fn smart_truncate_index(content: &str, cut_index: usize) -> usize {
    let chars: Vec<char> = content.chars().collect();
    let cut = cut_index.min(chars.len());
    // on a word boundary → keep as-is
    if !is_word_char(chars.get(cut.wrapping_sub(1)).copied())
        || !is_word_char(chars.get(cut).copied())
    {
        return cut;
    }
    if let Some(b) = find_whitespace_backward(content, cut) {
        return b;
    }
    if let Some(f) = find_whitespace_forward(content, cut) {
        return f;
    }
    cut
}

/// Truncate long tool results at a word boundary
/// (parity: `compressToolResults`, MAX_TOOL_LENGTH=2000).
fn compress_tool_results(messages: &[Value], applied: &mut bool) -> Vec<Value> {
    const MAX_TOOL_LENGTH: usize = 2000;
    messages
        .iter()
        .map(|msg| {
            if msg.get("role").and_then(|r| r.as_str()) != Some("tool") {
                return msg.clone();
            }
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            if content.chars().count() <= MAX_TOOL_LENGTH {
                return msg.clone();
            }
            let cut_char_index = content
                .char_indices()
                .nth(MAX_TOOL_LENGTH)
                .map(|(i, _)| i)
                .unwrap_or(content.len());
            let cut = smart_truncate_index(content, cut_char_index);
            let truncated: String = content.chars().take(cut).collect();
            *applied = true;
            let mut m = msg.clone();
            m["content"] = Value::String(format!("{truncated}\n...[truncated]"));
            m
        })
        .collect()
}

/// Remove consecutive same-role messages with identical string content
/// (parity: `removeRedundantContent`).
fn remove_redundant_content(messages: &[Value], preserve_system: bool, applied: &mut bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let is_system = msg.get("role").and_then(|r| r.as_str()) == Some("system");
        if preserve_system && is_system {
            out.push(msg.clone());
            continue;
        }
        if i > 0 {
            let prev = &messages[i - 1];
            if msg.get("role") == prev.get("role") {
                let cur = msg.get("content").cloned();
                let prev_c = prev.get("content").cloned();
                if let (Some(Value::String(c)), Some(Value::String(p))) = (&cur, &prev_c) {
                    if c == p {
                        *applied = true;
                        continue;
                    }
                }
            }
        }
        out.push(msg.clone());
    }
    out
}

/// Replace data: image URLs with text placeholders when the model has no
/// vision (parity: `replaceImageUrls`; vision detection delegated to config).
fn replace_image_urls(messages: &[Value], supports_vision: bool, applied: &mut bool) -> Vec<Value> {
    if supports_vision {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|msg| {
            let Some(parts) = msg.get("content").and_then(|c| c.as_array()) else {
                return msg.clone();
            };
            let mut changed = false;
            let new_parts: Vec<Value> = parts
                .iter()
                .map(|p| {
                    let is_image = p.get("type").and_then(|t| t.as_str()) == Some("image_url");
                    let url = p.pointer("/image_url/url").and_then(|u| u.as_str()).unwrap_or("");
                    if is_image && url.starts_with("data:image/") {
                        changed = true;
                        let format = url
                            .split('/')
                            .nth(1)
                            .and_then(|s| s.split(';').next())
                            .unwrap_or("unknown");
                        serde_json::json!({"type": "text", "text": format!("[image: {format}]")})
                    } else {
                        p.clone()
                    }
                })
                .collect();
            if !changed {
                return msg.clone();
            }
            *applied = true;
            let mut m = msg.clone();
            m["content"] = Value::Array(new_parts);
            m
        })
        .collect()
}

pub struct LiteOptions {
    pub preserve_system_prompt: bool,
    pub compress_tool_results: bool,
    pub supports_vision: Option<bool>,
}

impl Default for LiteOptions {
    fn default() -> Self {
        Self { preserve_system_prompt: false, compress_tool_results: true, supports_vision: None }
    }
}

/// Apply the full lite pipeline (parity: `applyLiteCompression`).
pub fn apply_lite_compression(messages: &Value, options: &LiteOptions) -> (Value, Option<CompressionStats>, Vec<String>) {
    let empty: Vec<Value> = Vec::new();
    let arr = messages.as_array().unwrap_or(&empty);
    let mut applied = false;
    let mut techniques: Vec<String> = Vec::new();
    let original_tokens: i64 = arr
        .iter()
        .map(|m| {
            let c = m.get("content");
            let s = c.and_then(|c| c.as_str()).unwrap_or("");
            if let Some(parts) = c.and_then(|c| c.as_array()) {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .map(estimate_tokens)
                    .sum()
            } else {
                estimate_tokens(s)
            }
        })
        .sum();

    let mut msgs: Vec<Value> = Vec::new();
    let mut ws_applied = false;
    for m in arr {
        let mut m = m.clone();
        // 1) whitespace collapse (string content only, parity)
        {
            let is_system = m.get("role").and_then(|r| r.as_str()) == Some("system");
            if !(options.preserve_system_prompt && is_system) {
                if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
                    let normalized = normalize_message_whitespace(s);
                    if normalized != s {
                        ws_applied = true;
                        m["content"] = Value::String(normalized);
                    }
                }
            }
        }
        msgs.push(m);
    }

    // 2) system dedup (original: dedupSystemPrompt runs when system is not preserved)
    if !options.preserve_system_prompt {
        let mut dedup_applied = false;
        msgs = dedup_system_prompt(&msgs, &mut dedup_applied);
        if dedup_applied {
            applied = true;
            techniques.push("system-dedup".to_string());
        }
    }

    // 3) tool-result truncation
    if options.compress_tool_results {
        let mut tool_applied = false;
        msgs = compress_tool_results(&msgs, &mut tool_applied);
        if tool_applied {
            applied = true;
            techniques.push("tool-compress".to_string());
        }
    }

    // 4) consecutive duplicate removal
    let mut red_applied = false;
    msgs = remove_redundant_content(&msgs, options.preserve_system_prompt, &mut red_applied);
    if red_applied {
        applied = true;
        techniques.push("redundant-remove".to_string());
    }

    // 5) image placeholders for non-vision models
    if let Some(sv) = options.supports_vision {
        let mut img_applied = false;
        msgs = replace_image_urls(&msgs, sv, &mut img_applied);
        if img_applied {
            applied = true;
            techniques.push("image-placeholder".to_string());
        }
    }

    let out = Value::Array(msgs);
    let compressed_tokens: i64 = out
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let c = m.get("content");
            if let Some(s) = c.and_then(|c| c.as_str()) {
                estimate_tokens(s)
            } else if let Some(parts) = c.and_then(|c| c.as_array()) {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .map(estimate_tokens)
                    .sum()
            } else {
                0
            }
        })
        .sum();
    if ws_applied {
        applied = true;
        techniques.push("whitespace".to_string());
    }
    let stats = if applied {
        Some(CompressionStats::compute(original_tokens, compressed_tokens, techniques.clone(), Vec::new()))
    } else {
        None
    };
    (out, stats, techniques)
}

/// Apply lite to a whole request body (`{"messages": [...]}`).
pub fn apply_lite_compression_body(body: &Value, options: &LiteOptions) -> (Value, Option<CompressionStats>, Vec<String>) {
    if body.get("messages").is_none() {
        return (body.clone(), None, Vec::new());
    }
    let mut out = body.clone();
    let (msgs, stats, techniques) = apply_lite_compression(&body["messages"], options);
    out["messages"] = msgs;
    (out, stats, techniques)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn filler_tail(count: usize) -> String {
        " filler".repeat(count)
    }

    #[test]
    fn whitespace_collapsed() {
        let body = json!({"messages": [{"role": "user", "content": "a\n\n\n\n\nb   \nc"}]});
        let (out, _, t) = apply_lite_compression_body(&body, &LiteOptions::default());
        let text = out["messages"][0]["content"].as_str().unwrap();
        assert_eq!(text, "a\n\nb\nc");
        assert!(t.contains(&"whitespace".to_string()) || !t.is_empty());
    }

    #[test]
    fn duplicate_system_deduped() {
        let shared = format!("You are terse. Always answer briefly.{}", filler_tail(30));
        let body = json!({"messages": [
            {"role": "system", "content": shared},
            {"role": "system", "content": format!("{shared} A differing tail that goes past the dedup key window completely here.")}
        ]});
        let (out, _, t) = apply_lite_compression_body(&body, &LiteOptions::default());
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(t.contains(&"system-dedup".to_string()));
    }

    #[test]
    fn tool_result_truncated_at_word_boundary() {
        let long = "word ".repeat(600); // 3000 chars > 2000
        let body = json!({"messages": [{"role": "tool", "content": long}]});
        let (out, _, t) = apply_lite_compression_body(&body, &LiteOptions::default());
        let text = out["messages"][0]["content"].as_str().unwrap();
        assert!(text.contains("...[truncated]"));
        assert!(text.len() < long.len());
        assert!(t.contains(&"tool-compress".to_string()));
        // no mid-word garbling: last line before marker ends cleanly
        let before_marker = text.split("\n...[truncated]").next().unwrap();
        assert!(!before_marker.ends_with("wor"));
    }

    #[test]
    fn consecutive_duplicates_removed() {
        let body = json!({"messages": [
            {"role": "user", "content": "same content here"},
            {"role": "user", "content": "same content here"},
            {"role": "user", "content": "different"}
        ]});
        let (out, _, t) = apply_lite_compression_body(&body, &LiteOptions::default());
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(t.contains(&"redundant-remove".to_string()));
    }

    #[test]
    fn image_placeholder_for_non_vision() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]}]});
        let (out, _, t) = apply_lite_compression_body(
            &body,
            &LiteOptions { supports_vision: Some(false), ..Default::default() },
        );
        let parts = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[1]["text"], "[image: png]");
        assert!(t.contains(&"image-placeholder".to_string()));
    }

    #[test]
    fn no_op_when_nothing_applies() {
        let body = json!({"messages": [{"role": "user", "content": "short text"}]});
        let (_, stats, _) = apply_lite_compression_body(&body, &LiteOptions::default());
        assert!(stats.is_none());
    }
}
