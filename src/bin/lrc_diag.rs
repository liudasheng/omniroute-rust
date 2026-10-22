//! Offline diagnostic for the LRC tool-result bucket: attribute every tool
//! result block to the reason it was (or was not) compressed.
//!
//! Usage: `lrc_diag <messages.json>` — reads a JSON array of OpenAI-shaped
//! messages on stdin or from the file and prints a JSON report.

use omniroute_rust::compression::lrc::gates::{estimate_tokens_mixed, extract_protected, protected_survives};
use omniroute_rust::compression::lrc::LrcConfig;
use omniroute_rust::compression::lrc::Profile;
use serde_json::{json, Value};

fn role(m: &Value) -> &str {
    m.get("role").and_then(|r| r.as_str()).unwrap_or("")
}

fn main() {
    let mut input = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut input).ok();
    let messages: Vec<Value> = serde_json::from_str(&input).unwrap_or_default();
    if messages.is_empty() {
        eprintln!("no messages on stdin");
        std::process::exit(1);
    }

    let profile = std::env::args().nth(1).unwrap_or_else(|| "extreme".into());
    let cfg = LrcConfig::preset(Profile::parse(&profile).unwrap_or(Profile::Extreme));
    let mut call_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in &messages {
        if role(m) == "assistant" {
            if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
                for c in calls {
                    let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = c
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    call_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    // recent window: everything from the N-th last assistant message onwards
    let mut turns = 0usize;
    let mut cut = 0usize;
    for (i, m) in messages.iter().enumerate().rev() {
        if role(m) == "assistant" {
            turns += 1;
            if turns >= cfg.recent_turns {
                cut = i;
                break;
            }
        }
    }

    let mut buckets: std::collections::BTreeMap<&str, (i64, i64, usize)> = std::collections::BTreeMap::new();
    let mut per_tool: std::collections::BTreeMap<String, (i64, i64)> = std::collections::BTreeMap::new();
    for (i, m) in messages.iter().enumerate() {
        if role(m) != "tool" && role(m) != "function" {
            continue;
        }
        let text = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
        let tool = call_names.get(id).cloned().unwrap_or_else(|| "?".into());
        let tokens = estimate_tokens_mixed(text);
        let reason = if i >= cut {
            "recent-window"
        } else {
            let prot = extract_protected(text, cfg.filter.max_critical_lines, 60);
            let never = cfg.filter.never_lossy.iter().any(|n| n.eq_ignore_ascii_case(&tool));
            if never {
                "never-lossy-tool"
            } else if tokens < cfg.filter.min_block_tokens {
                "below-threshold"
            } else {
                // simulate the selection to see whether the gate would refuse
                let lines: Vec<&str> = text.split('\n').collect();
                let head = cfg.filter.head_lines.min(lines.len());
                let tail = cfg.filter.tail_lines.min(lines.len().saturating_sub(head));
                let tail_start = lines.len().saturating_sub(tail);
                let mut kept: Vec<String> = lines[..head].iter().map(|s| (*s).to_string()).collect();
                for l in &prot.lines {
                    if kept.iter().any(|k| k.trim() == l.trim()) {
                        continue;
                    }
                    if lines[head..tail_start.max(head)].iter().any(|x| x.trim() == l.trim()) {
                        kept.push(l.clone());
                    }
                }
                let mut out = kept.join("\n");
                out.push_str(&format!("\n[... omni-elided {} lines ...]", lines.len().saturating_sub(head + tail)));
                for l in &lines[tail_start..] {
                    out.push_str(l);
                    out.push('\n');
                }
                if protected_survives(&prot, &out) {
                    "compressed"
                } else {
                    "protected-gate-refused"
                }
            }
        };
        let e = buckets.entry(reason).or_insert((0, 0, 0));
        e.0 += tokens;
        e.1 += 1;
        let pt = per_tool.entry(tool).or_insert((0, 0));
        pt.0 += tokens;
        if reason == "compressed" {
            pt.1 += tokens;
        }
    }

    let total: i64 = buckets.values().map(|(t, _, _)| t).sum();
    let out = json!({
        "profile": profile,
        "messages": messages.len(),
        "recent_window_start": cut,
        "tool_result_total_tokens": total,
        "by_reason": buckets.iter().map(|(k, (t, n, _))| json!({"reason": k, "tokens": t, "blocks": n,
            "share_pct": (100.0 * (*t as f64) / total.max(1) as f64 * 10.0).round() / 10.0})).collect::<Vec<_>>(),
        "by_tool": per_tool.iter().map(|(k, (t, c))| json!({"tool": k, "tokens": t,
            "compressible_tokens": c})).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
