//! Aggressive compression (parity: `open-sse/services/compression/aggressive.ts`
//! + `toolResultCompressor.ts` + `summarizer.ts`).
//!
//! Pipeline: tool-result compression -> extractive summarizer for long
//! messages -> downgrade chain (caveman -> lite when savings fall below the
//! threshold). Divergence: progressive aging and SLM summarizer tiers are
//! approximated by a single extractive summarizer (documented in PARITY.md).

use crate::compression::caveman::{caveman_compress, CavemanConfig, CompressionStats, Intensity};
use crate::compression::estimate::estimate_tokens;
use crate::compression::lite::{apply_lite_compression_body, LiteOptions};
use once_cell::sync::Lazy;
use serde_json::Value;

pub const COMPRESSED_MARKER_PREFIX: &str = "[COMPRESSED:";

static FILE_PATH_RE: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(
        r"(?:[\w./\-]+/)?[\w.-]+\.(?:rs|ts|tsx|js|jsx|py|go|java|c|h|cpp|hpp|md|json|toml|yaml|yml|sh|lock)\b",
    )
    .unwrap()
});

#[derive(Debug, Clone)]
pub struct AggressiveConfig {
    pub preserve_system_prompt: bool,
    pub summarizer_enabled: bool,
    pub max_tokens_per_message: i64,
    pub min_savings_threshold: f64,
    pub tool_file_content: bool,
    pub tool_grep_search: bool,
    pub tool_shell_output: bool,
    pub tool_json: bool,
    pub tool_error_message: bool,
}

impl Default for AggressiveConfig {
    fn default() -> Self {
        Self {
            preserve_system_prompt: true,
            summarizer_enabled: true,
            max_tokens_per_message: 2048,
            min_savings_threshold: 0.05,
            tool_file_content: true,
            tool_grep_search: true,
            tool_shell_output: true,
            tool_json: true,
            tool_error_message: true,
        }
    }
}

pub struct ToolCompressResult {
    pub compressed: String,
    pub strategy: &'static str,
    pub saved: i64,
}

fn is_code_like_line(line: &str) -> bool {
    let l = line.trim_start();
    [
        "import ", "export ", "function ", "class ", "const ", "let ", "var ", "return ",
        "if(", "if (", "for(", "for (", "while(", "while (",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

fn parse_grep_line_path(line: &str) -> Option<String> {
    let first = line.find(':')?;
    if first == 0 {
        return None;
    }
    let path = &line[..first];
    if path.is_empty() || path.contains(' ') || path.starts_with("http") {
        return None;
    }
    let second = line[first + 1..].find(':').map(|i| i + first + 1)?;
    let lineno = &line[first + 1..second];
    if lineno.is_empty() || !lineno.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(path.to_string())
}

fn compress_file_content(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() < 3 || !lines.iter().any(|l| is_code_like_line(l)) {
        return None;
    }
    let (keep, tail) = (20usize, 5usize);
    if lines.len() <= keep + tail {
        return None;
    }
    let elided = lines.len() - keep - tail;
    Some(format!(
        "{}\n... [{} lines elided] ...\n{}",
        lines[..keep].join("\n"),
        elided,
        lines[lines.len() - tail..].join("\n")
    ))
}

fn compress_grep_search(content: &str) -> Option<String> {
    let grep_lines: Vec<&str> = content
        .split('\n')
        .filter(|l| parse_grep_line_path(l).is_some())
        .collect();
    if grep_lines.is_empty() {
        return None;
    }
    let mut paths = std::collections::BTreeSet::new();
    for l in &grep_lines {
        if let Some(p) = parse_grep_line_path(l) {
            paths.insert(p);
        }
    }
    let top30: Vec<&str> = grep_lines.iter().take(30).copied().collect();
    let remaining = grep_lines.len().saturating_sub(30);
    let mut result = top30.join("\n");
    if remaining > 0 {
        result.push_str(&format!("\n... [{} more matches]", remaining));
    }
    result.push_str(&format!(
        "\nFiles: {}",
        paths.into_iter().collect::<Vec<_>>().join(", ")
    ));
    Some(result)
}

fn compress_shell_output(content: &str) -> Option<String> {
    let has_ansi = content.contains('\x1b');
    let has_prompt = content.contains("$ ");
    if !has_ansi && !has_prompt {
        return None;
    }
    let mut cleaned = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
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
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let start = lines.len().saturating_sub(50);
    let mut deduped: Vec<&str> = Vec::new();
    for line in &lines[start..] {
        if deduped.last() != Some(line) {
            deduped.push(line);
        }
    }
    Some(deduped.join("\n"))
}

fn compress_json(content: &str) -> Option<String> {
    if content.chars().count() <= 2000 {
        return None;
    }
    let trimmed = content.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return None;
    }
    let parsed: Value = serde_json::from_str(content).ok()?;
    match parsed {
        Value::Array(arr) => {
            if arr.len() <= 7 {
                return None;
            }
            let head: Vec<&Value> = arr.iter().take(5).collect();
            let tail: Vec<&Value> = arr[arr.len().saturating_sub(2)..].iter().collect();
            Some(
                serde_json::json!({"type": "array", "total": arr.len(), "first5": head, "last2": tail})
                    .to_string(),
            )
        }
        Value::Object(obj) => {
            let mut summary = serde_json::Map::new();
            for (i, (key, val)) in obj.iter().enumerate() {
                if i >= 20 {
                    break;
                }
                match val {
                    Value::Object(inner) => summary.insert(
                        key.clone(),
                        Value::String(format!("{{...{} keys}}", inner.len())),
                    ),
                    Value::Array(inner) => summary.insert(
                        key.clone(),
                        Value::String(format!("{{...{} items}}", inner.len())),
                    ),
                    other => summary.insert(key.clone(), other.clone()),
                };
            }
            if obj.len() > 20 {
                summary.insert(format!("_remaining_{}_keys", obj.len() - 20), Value::Bool(true));
            }
            Some(Value::Object(summary).to_string())
        }
        _ => None,
    }
}

fn has_error_like_output(content: &str) -> bool {
    content.contains("Error:")
        || content.contains("Exception:")
        || content.contains("Traceback (most recent call last):")
}

fn compress_error_message(content: &str) -> Option<String> {
    if !has_error_like_output(content) {
        return None;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let error_line = lines.first().copied().unwrap_or("");
    let stack: Vec<&str> = lines.get(1..).unwrap_or(&[]).to_vec();
    if stack.is_empty() {
        return None;
    }
    let head: Vec<&str> = stack.iter().take(10).copied().collect();
    let tail: Vec<&str> = if stack.len() > 10 {
        stack[stack.len() - 3..].to_vec()
    } else {
        Vec::new()
    };
    let middle: Vec<String> = if stack.len() > 13 {
        vec![format!("... [{} frames elided] ...", stack.len() - 13)]
    } else {
        Vec::new()
    };
    let mut out: Vec<String> = vec![error_line.to_string()];
    out.extend(head.iter().map(|s| s.to_string()));
    out.extend(middle);
    out.extend(tail.iter().map(|s| s.to_string()));
    Some(out.join("\n"))
}

/// Compress one tool-result string by trying strategies in order
/// (parity: `compressToolResult`).
pub fn compress_tool_result(content: &str, cfg: &AggressiveConfig) -> ToolCompressResult {
    let saved_of = |compressed: &String| estimate_tokens(content) - estimate_tokens(compressed);
    if cfg.tool_file_content {
        if let Some(c) = compress_file_content(content) {
            let saved = saved_of(&c);
            return ToolCompressResult { compressed: c.clone(), strategy: "fileContent", saved };
        }
    }
    if cfg.tool_grep_search {
        if let Some(c) = compress_grep_search(content) {
            let saved = saved_of(&c);
            return ToolCompressResult { compressed: c.clone(), strategy: "grepSearch", saved };
        }
    }
    if cfg.tool_shell_output {
        if let Some(c) = compress_shell_output(content) {
            let saved = saved_of(&c);
            return ToolCompressResult { compressed: c.clone(), strategy: "shellOutput", saved };
        }
    }
    if cfg.tool_json {
        if let Some(c) = compress_json(content) {
            let saved = saved_of(&c);
            return ToolCompressResult { compressed: c.clone(), strategy: "json", saved };
        }
    }
    if cfg.tool_error_message {
        if let Some(c) = compress_error_message(content) {
            let saved = saved_of(&c);
            return ToolCompressResult { compressed: c.clone(), strategy: "errorMessage", saved };
        }
    }
    ToolCompressResult { compressed: content.to_string(), strategy: "none", saved: 0 }
}

/// Extractive summarizer (parity: `RuleBasedSummarizer.summarize`): emits
/// `[COMPRESSED:summary]` + intents/files/errors/decision lines.
pub fn summarize_text(text: &str, max_len: usize) -> String {
    let mut files: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut intent: Option<String> = None;

    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with(COMPRESSED_MARKER_PREFIX) {
            continue;
        }
        if intent.is_none() {
            intent = Some(t.chars().take(160).collect());
        }
        if let Ok(Some(m)) = FILE_PATH_RE.find(t) {
            let f = m.as_str().to_string();
            if files.len() < 12 && !files.contains(&f) {
                files.push(f);
            }
        }
        if (t.contains("Error:") || t.contains("Exception:")) && errors.len() < 5 {
            errors.push(t.chars().take(120).collect());
        }
    }

    let mut parts: Vec<String> = vec![COMPRESSED_MARKER_PREFIX.to_string()];
    if let Some(i) = &intent {
        parts.push(format!("Intents: {i}."));
    }
    if !files.is_empty() {
        parts.push(format!("Files touched: {}.", files.join(", ")));
    }
    if !errors.is_empty() {
        parts.push(format!("Errors: {}.", errors.join("; ")));
    }
    if let Some(d) = text.lines().rev().map(|l| l.trim()).find(|l| !l.is_empty()) {
        parts.push(format!("Decision: {}", d.chars().take(160).collect::<String>()));
    }
    let out = parts.join(" ");
    if out.chars().count() > max_len {
        out.chars().take(max_len).collect()
    } else {
        out
    }
}

fn tokens_of_message(m: &Value) -> i64 {
    match m.get("content") {
        Some(Value::String(s)) => estimate_tokens(s),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .map(estimate_tokens)
            .sum(),
        _ => 0,
    }
}

fn compressed_tokens_for(msgs: &Value) -> i64 {
    msgs.as_array().map(|a| a.iter().map(tokens_of_message).sum()).unwrap_or(0)
}

/// Aggressive pipeline (parity: `compressAggressive`).
pub fn compress_aggressive(messages: &Value, cfg: &AggressiveConfig) -> (Value, CompressionStats) {
    let _empty: Vec<Value> = Vec::new();
    let empty_arr = Vec::new();
    let arr = messages.as_array().unwrap_or(&empty_arr);
    let original_tokens: i64 = arr.iter().map(tokens_of_message).sum();
    let last_user_idx = arr
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));

    // Step 1: tool-result compression (openAI-shape role=tool/function)
    let mut current: Vec<Value> = Vec::new();
    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if cfg.preserve_system_prompt && role == "system" {
            current.push(m.clone());
            continue;
        }
        if role == "tool" || role == "function" {
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            if !text.is_empty() && !text.starts_with(COMPRESSED_MARKER_PREFIX) {
                let result = compress_tool_result(text, cfg);
                if result.strategy != "none" && result.saved > 0 {
                    let mut nm = m.clone();
                    nm["content"] = Value::String(result.compressed);
                    current.push(nm);
                    continue;
                }
            }
        }
        current.push(m.clone());
    }

    // Step 2: summarizer for long non-system, non-last-user messages
    let mut summarizer_savings = 0i64;
    if cfg.summarizer_enabled {
        for (idx, m) in current.iter_mut().enumerate() {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if cfg.preserve_system_prompt && role == "system" {
                continue;
            }
            if Some(idx) == last_user_idx {
                continue;
            }
            let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            if text.is_empty() || text.starts_with(COMPRESSED_MARKER_PREFIX) {
                continue;
            }
            if estimate_tokens(&text) <= cfg.max_tokens_per_message {
                continue;
            }
            let summary = summarize_text(&text, cfg.max_tokens_per_message as usize);
            if summary.chars().count() < text.chars().count() {
                summarizer_savings += estimate_tokens(&text) - estimate_tokens(&summary);
                m["content"] = Value::String(summary);
            }
        }
    }

    // Step 3: downgrade chain when savings fall below the threshold
    let mut techniques: Vec<String> = Vec::new();
    if summarizer_savings > 0 {
        techniques.push("summarizer".to_string());
    }
    let mut final_msgs = Value::Array(current);
    let tokens_now: i64 = final_msgs.as_array().map(|a| a.iter().map(tokens_of_message).sum()).unwrap_or(0);
    let mut savings_percent = if original_tokens > 0 {
        (original_tokens - tokens_now) as f64 / original_tokens as f64 * 100.0
    } else {
        0.0
    };

    if savings_percent < cfg.min_savings_threshold * 100.0 {
        // caveman fallback — keep the last user message verbatim
        let cav_cfg = CavemanConfig {
            intensity: Intensity::Full,
            compress_roles: vec!["user".into(), "assistant".into(), "system".into()],
            ..Default::default()
        };
        let (cand, _) = caveman_compress(&final_msgs, &cav_cfg);
        let mut cand_msgs = cand;
        if let Some(lu) = last_user_idx {
            if let (Some(orig), Some(a)) =
                (final_msgs.as_array().and_then(|a| a.get(lu)).cloned(), cand_msgs.as_array_mut())
            {
                a[lu] = orig;
            }
        }
        let cand_tokens: i64 = cand_msgs.as_array().map(|a| a.iter().map(tokens_of_message).sum()).unwrap_or(0);
        let cand_savings = if original_tokens > 0 {
            (original_tokens - cand_tokens) as f64 / original_tokens as f64 * 100.0
        } else {
            0.0
        };
        if cand_savings > savings_percent {
            savings_percent = cand_savings;
            techniques.push("caveman-fallback".to_string());
            final_msgs = cand_msgs;
        }
    }
    if savings_percent < cfg.min_savings_threshold * 100.0 {
        let lite_opts = LiteOptions {
            preserve_system_prompt: cfg.preserve_system_prompt,
            compress_tool_results: false, // already applied in step 1
            supports_vision: None,
        };
        let (cand, cand_stats, _) =
            apply_lite_compression_body(&serde_json::json!({"messages": final_msgs}), &lite_opts);
        if let Some(s) = &cand_stats {
            if s.savings_percent > savings_percent {
                savings_percent = s.savings_percent;
                techniques.push("lite-fallback".to_string());
                final_msgs = cand["messages"].clone();
            }
        }
        let _ = savings_percent;
    }

    let stats = CompressionStats::compute(original_tokens, compressed_tokens_for(&final_msgs), techniques, Vec::new());
    (final_msgs, stats)
}

/// Apply aggressive to a whole request body.
pub fn compress_aggressive_body(body: &Value, cfg: &AggressiveConfig) -> (Value, CompressionStats) {
    if body.get("messages").is_none() {
        let (_, stats) = compress_aggressive(&Value::Array(Vec::new()), cfg);
        return (body.clone(), stats);
    }
    let mut out = body.clone();
    let (msgs, stats) = compress_aggressive(&body["messages"], cfg);
    out["messages"] = msgs;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_content_compression() {
        let code = (0..40).map(|i| format!("const x{i} = {i};")).collect::<Vec<_>>().join("\n");
        let result = compress_tool_result(&code, &AggressiveConfig::default());
        assert_eq!(result.strategy, "fileContent");
        assert!(result.compressed.contains("lines elided"));
        assert!(result.saved > 0);
    }

    #[test]
    fn error_message_compression() {
        let mut frames: Vec<String> = vec!["Error: something failed".into()];
        for i in 0..30 {
            frames.push(format!("    at fn{i} (file{i}.rs:{i})"));
        }
        let content = frames.join("\n");
        let result = compress_tool_result(&content, &AggressiveConfig::default());
        assert_eq!(result.strategy, "errorMessage");
        assert!(result.compressed.lines().count() < content.lines().count());
        assert!(result.compressed.contains("frames elided"));
        assert!(result.compressed.starts_with("Error:"));
    }

    #[test]
    fn shell_output_strips_ansi() {
        let content = "\x1b[32mOK\x1b[0m done\n".repeat(60);
        let result = compress_tool_result(&content, &AggressiveConfig::default());
        assert_eq!(result.strategy, "shellOutput");
        assert!(!result.compressed.contains('\x1b'));
    }

    #[test]
    fn json_compression_shapes() {
        let arr: Vec<Value> = (0..40)
            .map(|i| json!({"i": i, "padding": "x".repeat(120)}))
            .collect();
        let content = serde_json::to_string(&arr).unwrap();
        assert!(content.chars().count() > 2000);
        let result = compress_tool_result(&content, &AggressiveConfig::default());
        assert_eq!(result.strategy, "json");
        let c: Value = serde_json::from_str(&result.compressed).unwrap();
        assert_eq!(c["total"], 40);
        assert!(c["first5"].is_array());
    }

    #[test]
    fn summarizer_marks_marker() {
        let long = "the implementation file src/compression/mod.rs handles the engine dispatch\n".repeat(100);
        let s = summarize_text(&long, 500);
        assert!(s.starts_with(COMPRESSED_MARKER_PREFIX));
        assert!(s.contains("src/compression/mod.rs"));
        assert!(s.chars().count() < long.chars().count());
    }

    #[test]
    fn aggressive_pipeline_compresses_long_tool_history() {
        let code = (0..60).map(|i| format!("const fillerVar{i} = {i};")).collect::<Vec<_>>().join("\n");
        let msgs = json!([
            {"role": "system", "content": "You are a coding agent. Keep answers short and precise. This system prompt must survive compression untouched at any intensity level for every request the gateway processes on the wire today and tomorrow."},
            {"role": "user", "content": "fix the bug"},
            {"role": "assistant", "content": "reading files"},
            {"role": "tool", "content": code},
            {"role": "user", "content": "and now run the tests and tell me exactly what the failing assertion says in the output above please"}
        ]);
        let (out, stats) = compress_aggressive(&msgs, &AggressiveConfig::default());
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["content"], msgs[0]["content"], "system prompt preserved");
        assert_eq!(arr[4]["content"], msgs[4]["content"], "last user message preserved");
        assert!(arr[3]["content"].as_str().unwrap().contains("lines elided"), "tool result compressed");
        assert!(stats.compressed_tokens < stats.original_tokens);
        assert!(stats.savings_percent > 0.0);
    }
}
