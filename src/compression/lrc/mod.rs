//! LRC — Layered Reversible Compression.
//!
//! The existing engines all rewrite `messages[].content` strings. In real agent
//! traffic that is a small slice of the payload: 90% of the prompt tokens are
//! history, and the three biggest history buckets (`tools` schema, historical
//! `tool_calls` arguments, historical tool results) plus replayed reasoning are
//! untouched by every other mode.
//!
//! LRC works on the *request as a whole*, bucket by bucket, and keeps a hard
//! fail-open contract:
//!
//! - never touches `system`, the last `user` message, or the newest messages;
//! - never changes `tool_call` ↔ `tool_result` pairing;
//! - every lossy block keeps all protected fragments (errors, diffs, paths,
//!   URLs, hashes) or is reverted;
//! - never grows a block (`never-worse`);
//! - idempotent (already-marked blocks are skipped) and deterministic
//!   (rendering depends only on block content and position, never on turn
//!   counters) so the upstream prompt cache stays valid.
//!
//! Pointer dumps make the loss reversible: elided text is written to
//! `<data_dir>/compression-dumps/<sha8>.txt` and referenced from the marker.

pub mod filter;
pub mod gates;
pub mod schema;

use crate::compression::caveman::CompressionStats;
use filter::{FilterCfg, FilterMode};
use gates::{estimate_tokens_mixed, is_marked, short_hash, Invariants};
use schema::SchemaMode;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Conservative,
    Balanced,
    Aggressive,
    Extreme,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "conservative" | "safe" => Some(Self::Conservative),
            "balanced" | "default" | "neutral" => Some(Self::Balanced),
            "aggressive" | "strong" => Some(Self::Aggressive),
            "extreme" | "max" => Some(Self::Extreme),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
            Self::Extreme => "extreme",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimReasoning {
    Off,
    Auto,
    On,
}

impl TrimReasoning {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            "on" | "true" | "force" => Some(Self::On),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::On => "on",
        }
    }
}

/// Dump store for reversible pointers (content-addressed, bounded).
#[derive(Debug, Clone)]
pub struct DumpStore {
    pub dir: PathBuf,
    pub max_files: usize,
    pub min_chars: usize,
}

impl DumpStore {
    pub fn new(dir: PathBuf, max_files: usize, min_chars: usize) -> Self {
        Self { dir, max_files, min_chars }
    }

    /// Store the original block; returns the path to reference in the marker.
    pub fn put(&self, content: &str) -> Option<String> {
        if content.chars().count() < self.min_chars {
            return None;
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return None;
        }
        prune(&self.dir, self.max_files);
        let path = self.dir.join(format!("{}.txt", short_hash(content)));
        if path.exists() {
            return Some(path.display().to_string());
        }
        std::fs::write(&path, content).ok()?;
        Some(path.display().to_string())
    }
}

fn prune(dir: &std::path::Path, max_files: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if meta.is_file() {
                Some((meta.modified().ok()?, e.path()))
            } else {
                None
            }
        })
        .collect();
    if files.len() < max_files {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    let drop_count = files.len() / 3 + 1;
    for (_, p) in files.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(p);
    }
}

/// Everything the pipeline needs. Built from `CompressionConfig` presets.
#[derive(Debug, Clone)]
pub struct LrcConfig {
    pub profile: Profile,
    /// newest N messages are never touched (last user/system are always safe)
    /// number of *assistant turns* kept verbatim (a turn = one assistant
    /// message plus its tool results). Counting turns, not messages, matters:
    /// an agent step is two messages, so a message-based window would leave
    /// the current step's tool output unprotected.
    pub recent_turns: usize,
    pub schema_mode: SchemaMode,
    pub trim_reasoning: TrimReasoning,
    pub retire_args: bool,
    pub filter: FilterCfg,
    pub dedupe_results: bool,
    pub trim_assistant_text: bool,
    pub args_min_tokens: i64,
    pub text_min_tokens: i64,
    pub dump: Option<DumpStore>,
}

impl Default for LrcConfig {
    fn default() -> Self {
        Self::preset(Profile::Balanced)
    }
}

impl LrcConfig {
    pub fn preset(profile: Profile) -> Self {
        let mut cfg = LrcConfig {
            profile,
            recent_turns: 3,
            schema_mode: SchemaMode::Spec,
            trim_reasoning: TrimReasoning::Auto,
            retire_args: true,
            filter: FilterCfg::default(),
            dedupe_results: true,
            trim_assistant_text: false,
            args_min_tokens: 48,
            text_min_tokens: 400,
            dump: None,
        };
        match profile {
            Profile::Conservative => {
                cfg.trim_reasoning = TrimReasoning::Off;
                cfg.retire_args = false;
                cfg.filter.mode = FilterMode::Light;
                cfg.dedupe_results = false;
                cfg.schema_mode = SchemaMode::Spec;
            }
            Profile::Balanced => {}
            Profile::Aggressive => {
                cfg.schema_mode = SchemaMode::Stub;
                cfg.trim_assistant_text = true;
                cfg.recent_turns = 3;
            }
            Profile::Extreme => {
                cfg.schema_mode = SchemaMode::Prune;
                cfg.trim_assistant_text = true;
                cfg.trim_reasoning = TrimReasoning::On;
                cfg.recent_turns = 2;
                cfg.filter.head_lines = 30;
                cfg.filter.tail_lines = 20;
            }
        }
        cfg
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// (bucket, original tokens, compressed tokens)
    pub buckets: Vec<(&'static str, i64, i64)>,
    pub counters: HashMap<&'static str, i64>,
}

impl Ledger {
    fn add(&mut self, bucket: &'static str, original: i64, compressed: i64) {
        if let Some(entry) = self.buckets.iter_mut().find(|(b, _, _)| *b == bucket) {
            entry.1 += original;
            entry.2 += compressed;
        } else {
            self.buckets.push((bucket, original, compressed));
        }
    }
    fn bump(&mut self, counter: &'static str) {
        *self.counters.entry(counter).or_insert(0) += 1;
    }
    pub fn saved(&self) -> i64 {
        self.buckets.iter().map(|(_, o, c)| (o - c).max(0)).sum()
    }
    /// Compact `buckets: name=o->c,...` annotation, bounded to `max_bytes`.
    pub fn annotation(&self, max_bytes: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        for (name, o, c) in self.buckets.iter().filter(|(_, o, c)| o > c) {
            let part = format!("{}={}->{}", name, o, c);
            if bytes + part.len() + 1 > max_bytes {
                break;
            }
            bytes += part.len() + 1;
            parts.push(part);
        }
        parts.join(",")
    }
}

pub struct Outcome {
    pub body: Value,
    pub stats: CompressionStats,
    pub ledger: Ledger,
    pub gates: Vec<&'static str>,
    /// compression did not shrink the body — normal condition, not a violation
    pub never_worse: bool,
    pub profile: Profile,
    pub reverted: bool,
}

fn role(msg: &Value) -> &str {
    msg.get("role").and_then(|r| r.as_str()).unwrap_or("")
}

/// Collect every tool name mentioned by assistant messages (used by schema
/// `prune` and by tool-result naming).
fn tool_names(messages: &[Value]) -> HashSet<String> {
    let mut set = HashSet::new();
    for m in messages {
        if role(m) != "assistant" {
            continue;
        }
        if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
            for c in calls {
                if let Some(n) = c.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()) {
                    set.insert(n.to_string());
                }
            }
        }
        if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    if let Some(n) = p.get("name").and_then(|v| v.as_str()) {
                        set.insert(n.to_string());
                    }
                }
            }
        }
    }
    set
}

fn call_names(messages: &[Value]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for m in messages {
        if role(m) != "assistant" {
            continue;
        }
        if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
            for c in calls {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = c.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                if !id.is_empty() {
                    map.insert(id.to_string(), name.to_string());
                }
            }
        }
        if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if !id.is_empty() {
                        map.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }
    map
}

/// True when the body carries signed/encrypted reasoning that must be replayed
/// verbatim (Anthropic thinking blocks, `reasoning_details`).
fn has_signed_reasoning(messages: &[Value]) -> bool {
    for m in messages {
        if role(m) != "assistant" {
            continue;
        }
        if m.get("reasoning_details").is_some() {
            return true;
        }
        if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
            for p in parts {
                let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "thinking" && p.get("signature").is_some() {
                    return true;
                }
            }
        }
    }
    false
}

fn first_sentence(text: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    for _ in 0..cap {
        match chars.next() {
            Some('.') | Some('。') | Some('!') | Some('！') | Some('\n') => break,
            Some(c) => out.push(c),
            None => break,
        }
    }
    out.trim().to_string()
}

fn last_sentence(text: &str, cap: usize) -> String {
    let tail: String = text.chars().rev().take(cap).collect::<Vec<_>>().into_iter().rev().collect();
    tail.trim().to_string()
}

/// Summarise historical assistant prose: first sentence + last fragment.
fn summarise_text(text: &str, min_tokens: i64) -> Option<String> {
    if estimate_tokens_mixed(text) < min_tokens || is_marked(text) {
        return None;
    }
    let head = first_sentence(text, 220);
    let tail = last_sentence(text, 160);
    if head.is_empty() {
        return None;
    }
    let out = if tail.is_empty() || tail == head {
        format!("{head} [omni-elided {n} chars]", n = text.chars().count())
    } else {
        format!(
            "{head} [omni-elided {n} chars] {tail}",
            n = text.chars().count()
        )
    };
    if estimate_tokens_mixed(&out) >= estimate_tokens_mixed(text) {
        return None;
    }
    Some(out)
}

/// True when the argument object carries an edit anchor (`old_string`,
/// `old_text`, …). The model may re-issue the edit later using that exact
/// text, so such arguments are never retired (rtk's "Anchor" rule).
fn has_edit_anchor(parsed: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.iter().any(|(k, v)| k.starts_with("old_") || walk(v)),
            Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    walk(parsed)
}

/// Retire historical tool-call arguments into a valid-JSON pointer.
///
/// The result stays a valid JSON object (the upstream validates tool-call
/// arguments), keeps the identifying fields (path / command) and points at the
/// original by hash. Retirement is only applied when it actually shrinks the
/// payload.
fn retire_arguments(args: &str, tool: &str, min_tokens: i64) -> Option<String> {
    if args.is_empty() || args.chars().count() < 40 || is_marked(args) {
        return None;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(args) else {
        return None;
    };
    if estimate_tokens_mixed(args) < min_tokens {
        return None;
    }
    if has_edit_anchor(&parsed) {
        return None;
    }
    let path = parsed
        .get("file_path")
        .or_else(|| parsed.get("path"))
        .or_else(|| parsed.get("notebook_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let command = parsed.get("command").and_then(|v| v.as_str());
    // Keep the summary as small as possible: every extra field eats into the
    // saving, and a bulky summary would make the retirement a no-op for the
    // many small `bash` argument objects an agent emits.
    let mut summary = json!({
        "_omni": "args-elided",
        "_sha8": short_hash(args),
    });
    let _ = tool;
    if let Some(p) = path {
        summary["path"] = Value::String(p);
    }
    if let Some(c) = command {
        // A truncated command must LOOK truncated: without the ellipsis the
        // model reads a cut-off command as a complete one and may run a
        // half-command.
        let cut: String = if c.chars().count() > 120 {
            let mut head: String = c.chars().take(117).collect();
            head.push('…');
            head
        } else {
            c.to_string()
        };
        summary["command"] = Value::String(cut);
    }
    let out = serde_json::to_string(&summary).ok()?;
    if estimate_tokens_mixed(&out) >= estimate_tokens_mixed(args) {
        return None;
    }
    Some(out)
}

/// Index of the first message that belongs to the verbatim window, counted in
/// assistant turns. Messages before it are "history" and may be compressed.
fn turn_cut_index(messages: &[Value], recent_turns: usize) -> usize {
    if recent_turns == 0 {
        return 0;
    }
    let mut turns = 0usize;
    for (i, m) in messages.iter().enumerate().rev() {
        if role(m) == "assistant" {
            turns += 1;
            if turns >= recent_turns {
                return i;
            }
        }
    }
    0
}

/// Run the LRC pipeline. `None` means "no OpenAI-shaped messages" (skip).
pub fn apply(body: &Value, cfg: &LrcConfig) -> Option<Outcome> {
    let messages = body.get("messages").and_then(|m| m.as_array())?;
    if messages.is_empty() {
        return None;
    }
    let mut out = body.clone();
    let mut ledger = Ledger::default();
    let mut techniques: Vec<String> = Vec::new();
    let mut gates: Vec<&'static str> = Vec::new();

    let used_tools = tool_names(messages);
    let call_map = call_names(messages);

    // ---- B1: tools schema -------------------------------------------------
    if let Some(tools) = out.get("tools").cloned() {
        let (compacted, tech, (before, after)) = schema::compact_tools(&tools, cfg.schema_mode, &used_tools);
        if after > 0 && after < before {
            ledger.add("tools", before, after);
            techniques.extend(tech);
            out["tools"] = compacted;
        }
    }

    // ---- message pass -----------------------------------------------------
    // The verbatim window is counted in assistant *turns*: everything from the
    // N-th-last assistant message onwards is left untouched.
    let recent_start = turn_cut_index(messages, cfg.recent_turns);
    let signed_reasoning = has_signed_reasoning(messages);
    let trim_reasoning = match cfg.trim_reasoning {
        TrimReasoning::Off => false,
        TrimReasoning::On => true,
        TrimReasoning::Auto => !signed_reasoning,
    };

    let mut seen_results: HashMap<String, usize> = HashMap::new();
    let before_msgs = messages.clone();

    if let Some(arr) = out.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for i in 0..arr.len() {
            let is_old = i < recent_start;
            if !is_old {
                continue;
            }
            let role_str = role(&arr[i]).to_string();
            match role_str.as_str() {
                "assistant" => {
                    // B2: historical reasoning (plain string fields only; signed
                    // or encrypted reasoning is left untouched)
                    if trim_reasoning {
                        for field in ["reasoning_content", "reasoning"] {
                            let Some(Value::String(text)) = arr[i].get(field).cloned() else { continue };
                            if text.trim().is_empty() || is_marked(&text) {
                                continue;
                            }
                            let before = estimate_tokens_mixed(&text);
                            let marker = format!(
                                "[reasoning elided · {} chars · sha8={}]",
                                text.chars().count(),
                                short_hash(&text)
                            );
                            let after = estimate_tokens_mixed(&marker);
                            if after < before {
                                arr[i][field] = Value::String(marker);
                                ledger.add("reasoning", before, after);
                                techniques.push("lrc-reasoning".into());
                            }
                        }
                    }
                    // B3: historical tool-call arguments
                    if cfg.retire_args {
                        if let Some(calls) = arr[i].get_mut("tool_calls").and_then(|c| c.as_array_mut()) {
                            for call in calls.iter_mut() {
                                let Some(func) = call.get_mut("function") else { continue };
                                let tool = func
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let Some(Value::String(args)) = func.get("arguments").cloned() else { continue };
                                if let Some(summary) = retire_arguments(&args, &tool, cfg.args_min_tokens) {
                                    let before = estimate_tokens_mixed(&args);
                                    let after = estimate_tokens_mixed(&summary);
                                    func["arguments"] = Value::String(summary);
                                    ledger.add("args", before, after);
                                    techniques.push("lrc-args".into());
                                }
                            }
                        }
                    }
                    // B5: historical assistant prose
                    if cfg.trim_assistant_text {
                        if let Some(Value::String(text)) = arr[i].get("content").cloned() {
                            if let Some(summary) = summarise_text(&text, cfg.text_min_tokens) {
                                let before = estimate_tokens_mixed(&text);
                                let after = estimate_tokens_mixed(&summary);
                                arr[i]["content"] = Value::String(summary);
                                ledger.add("text", before, after);
                                techniques.push("lrc-text".into());
                            }
                        }
                    }
                }
                "tool" | "function" => {
                    let Some(Value::String(text)) = arr[i].get("content").cloned() else { continue };
                    if text.is_empty() {
                        continue;
                    }
                    let id = arr[i].get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let tool = call_map.get(id).cloned();
                    // S2: exact duplicate results within the request
                    if cfg.dedupe_results && text.chars().count() > 400 {
                        let key = format!("{}:{}", tool.clone().unwrap_or_default(), short_hash(&text));
                        if let Some(_first) = seen_results.get(&key) {
                            let before = estimate_tokens_mixed(&text);
                            let marker = format!(
                                "[identical to an earlier result in this conversation · {} chars · sha8={}]",
                                text.chars().count(),
                                short_hash(&text)
                            );
                            let after = estimate_tokens_mixed(&marker);
                            if after < before {
                                arr[i]["content"] = Value::String(marker);
                                ledger.add("tool_result", before, after);
                                ledger.bump("duplicate-results");
                                techniques.push("lrc-result-dedup".into());
                            }
                            continue;
                        }
                        seen_results.insert(key, i);
                    }
                    let dump = cfg
                        .dump
                        .as_ref()
                        .and_then(|store| store.put(&text));
                    if let Some(filtered) = filter::filter_tool_output(&text, tool.as_deref(), &cfg.filter, dump.as_deref()) {
                        arr[i]["content"] = Value::String(filtered.text);
                        ledger.add("tool_result", filtered.original_tokens, filtered.filtered_tokens);
                        techniques.extend(filtered.techniques);
                    }
                }
                "user" => {
                    // Claude-shaped tool results live in user messages
                    let Some(parts) = arr[i].get("content").and_then(|c| c.as_array()).cloned() else { continue };
                    let mut changed = false;
                    let mut new_parts = Vec::with_capacity(parts.len());
                    for mut part in parts {
                        if part.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            let tool = part
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .and_then(|id| call_map.get(id).cloned());
                            let inner = part.get("content").cloned();
                            let text = match &inner {
                                Some(Value::String(s)) => Some(s.clone()),
                                Some(Value::Array(items)) => Some(
                                    items
                                        .iter()
                                        .filter_map(|it| {
                                            if it.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                it.get("text").and_then(|t| t.as_str())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                ),
                                _ => None,
                            };
                            if let Some(text) = text {
                                let dump = cfg.dump.as_ref().and_then(|store| store.put(&text));
                                if let Some(filtered) =
                                    filter::filter_tool_output(&text, tool.as_deref(), &cfg.filter, dump.as_deref())
                                {
                                    part["content"] = Value::String(filtered.text);
                                    ledger.add("tool_result", filtered.original_tokens, filtered.filtered_tokens);
                                    techniques.extend(filtered.techniques);
                                    changed = true;
                                }
                            }
                        }
                        new_parts.push(part);
                    }
                    if changed {
                        arr[i]["content"] = Value::Array(new_parts);
                    }
                }
                _ => {}
            }
        }
    }

    // ---- S4: gates --------------------------------------------------------
    let after_msgs = out["messages"].as_array().cloned().unwrap_or_default();
    let inv = Invariants::snapshot(&before_msgs);
    gates.extend(inv.violations(&after_msgs));

    let original_tokens = estimate_tokens_mixed(&serde_json::to_string(body).unwrap_or_default());
    let compressed_tokens = estimate_tokens_mixed(&serde_json::to_string(&out).unwrap_or_default());

    // `never_worse`: compression must not grow the body. This is a normal
    // condition (no savings), not a structural violation — do not fold it into
    // `gates`, which only tracks invariant breaches.
    let never_worse = compressed_tokens >= original_tokens;

    let mut reverted = false;
    if !gates.is_empty() || never_worse {
        // fail-open: send the request exactly as the client wrote it
        reverted = true;
        out = body.clone();
        ledger = Ledger::default();
        techniques.clear();
    }

    let original = if reverted { original_tokens } else { original_tokens };
    let compressed = if reverted { original_tokens } else { compressed_tokens };
    let mut stats = CompressionStats::compute(original, compressed, techniques, Vec::new());
    stats.calibrated = Some((original, compressed));
    stats.buckets = ledger.buckets.clone();

    Some(Outcome {
        body: out,
        stats,
        ledger,
        gates,
        never_worse,
        profile: cfg.profile,
        reverted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> LrcConfig {
        LrcConfig::preset(Profile::Balanced)
    }

    fn long_log(lines: usize) -> String {
        let mut s = String::from("$ cargo test\n");
        for i in 0..lines {
            s.push_str(&format!("test result: ok. {} passed; 0 failed\n", i));
        }
        s.push_str("error: 1 test failed\n  --> tests/x.rs:9:1\n");
        s
    }

    fn body_with_history() -> Value {
        let mut messages = vec![
            json!({"role":"system","content":"you are an agent"}),
            json!({"role":"user","content":"fix the build"}),
        ];
        for i in 0..8 {
            messages.push(json!({"role":"assistant","content":"looking at it",
                "reasoning_content": format!("step {}: thinking {}", i, "x".repeat(120)),
                "tool_calls":[{"id":format!("c{}" , i),"type":"function",
                    "function":{"name":"bash","arguments": format!("{{\"command\":\"cargo test {}\"}}", i)}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("c{}" , i),
                "content": long_log(120)}));
        }
        messages.push(json!({"role":"user","content":"now summarise"}));
        json!({"model":"m","messages":messages,
            "tools":[{"type":"function","function":{"name":"bash","description":"run",
                "parameters":{"type":"object","additionalProperties":false,
                    "properties":{"command":{"type":"string","title":"cmd","description":"the command"}},
                    "required":["command"]}}}]})
    }

    #[test]
    fn pipeline_compresses_history_but_not_the_tail() {
        let body = body_with_history();
        let outcome = apply(&body, &cfg()).expect("applies");
        assert!(outcome.gates.is_empty(), "gates: {:?}", outcome.gates);
        assert!(!outcome.reverted);
        assert!(outcome.stats.savings_percent > 20.0, "savings {}", outcome.stats.savings_percent);

        let out_msgs = outcome.body["messages"].as_array().unwrap();
        let orig_msgs = body["messages"].as_array().unwrap();
        // system untouched
        assert_eq!(out_msgs[0], orig_msgs[0]);
        // last user untouched
        assert_eq!(out_msgs.last().unwrap(), orig_msgs.last().unwrap());
        // the newest messages stay verbatim
        assert_eq!(out_msgs[out_msgs.len() - 2], orig_msgs[orig_msgs.len() - 2]);
        // old tool results were filtered
        assert!(out_msgs[3]["content"].as_str().unwrap().contains("omni-elided"));
        // old reasoning was elided
        assert!(out_msgs[2]["reasoning_content"].as_str().unwrap().contains("reasoning elided"));
        // tool_call identity preserved
        assert_eq!(out_msgs[2]["tool_calls"][0]["id"], orig_msgs[2]["tool_calls"][0]["id"]);
        assert_eq!(out_msgs[2]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn signed_reasoning_is_never_trimmed() {
        let mut body = body_with_history();
        body["messages"][2]["reasoning_details"] = json!([{"type":"reasoning.encrypted","data":"abc"}]);
        let outcome = apply(&body, &cfg()).expect("applies");
        let out_msgs = outcome.body["messages"].as_array().unwrap();
        assert!(out_msgs[2]["reasoning_content"]
            .as_str()
            .map(|s| !s.contains("reasoning elided"))
            .unwrap_or(true));
    }

    #[test]
    fn args_retirement_stays_valid_json() {
        let big = "x".repeat(4000);
        let body = json!({"model":"m","messages":[
            {"role":"user","content":"start"},
            {"role":"assistant","content":"writing","tool_calls":[{"id":"c1","type":"function",
                "function":{"name":"write","arguments": json!({"file_path":"src/a.rs","content": big}).to_string()}}]},
            {"role":"tool","tool_call_id":"c1","content":"written"},
            {"role":"user","content":"next"},
            {"role":"assistant","content":"ok"},{"role":"user","content":"go"},
            {"role":"assistant","content":"ok"},{"role":"user","content":"go"},
            {"role":"assistant","content":"ok"},{"role":"user","content":"finish"}]});
        let outcome = apply(&body, &cfg()).expect("applies");
        let args = outcome.body["messages"][1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(args).expect("still valid JSON");
        assert_eq!(parsed["_omni"], "args-elided");
        assert_eq!(parsed["path"], "src/a.rs");
        assert_eq!(outcome.body["messages"][1]["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn idempotent_second_pass_is_a_noop() {
        let body = body_with_history();
        let first = apply(&body, &cfg()).expect("applies");
        let second = apply(&first.body, &cfg()).expect("applies");
        assert_eq!(first.body, second.body);
        assert!(second.stats.savings_percent < 1.0);
    }

    #[test]
    fn tools_schema_is_compacted() {
        let body = body_with_history();
        let outcome = apply(&body, &cfg()).expect("applies");
        let s = serde_json::to_string(&outcome.body["tools"]).unwrap();
        assert!(!s.contains("additionalProperties"));
        assert!(!s.contains("\"title\""));
        assert!(s.contains("command"));
    }

    #[test]
    fn degenerate_bodies_do_not_panic() {
        for body in [
            json!({}),
            json!({"messages": []}),
            json!({"messages":[{"role":"user","content":"hi"}]}),
            json!({"messages":[{"role":"assistant","content":null}]}),
            json!({"messages":[{"role":"tool","content":123}]}),
        ] {
            let _ = apply(&body, &cfg());
        }
    }
}
