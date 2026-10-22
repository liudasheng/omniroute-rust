//! Offline probe: does the LRC pipeline change the tool_call / tool_result
//! pairing of a real request body?
//!
//! It loads a request body from a JSON file, runs the pipeline for a given
//! profile, and compares the *structure* before and after: the sequence of
//! (role, tool_call_id, tool_call ids) triples. Only the content may change;
//! the shape may not.
//!
//! Run: cargo test --test lrc_pairing_probe -- --nocapture
//! The body path comes from the `LRC_PROBE_BODY` env var.

use omniroute_rust::compression::lrc::{self, Profile};
use serde_json::Value;

/// The structural fingerprint of a message list: role, the tool_call id it
/// answers, and the tool_call ids it announces.
fn fingerprint(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let answers = m
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let announces: Vec<&str> = m
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|tc| tc.get("id").and_then(|i| i.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            format!("{role}|answers={answers}|announces={}", announces.join(","))
        })
        .collect()
}

/// For every tool message, is the tool_call it answers announced by a
/// *preceding* assistant message?
fn unpaired(messages: &[Value]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "assistant" {
            if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        seen.push(id);
                    }
                }
            }
            continue;
        }
        if role == "tool" || role == "function" {
            let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            if !seen.iter().any(|s| *s == id) {
                out.push(format!("{id}"));
            }
        }
    }
    out
}

#[test]
fn lrc_preserves_the_tool_call_pairing_of_a_real_body() {
    let path = match std::env::var("LRC_PROBE_BODY") {
        Ok(p) => p,
        Err(_) => {
            println!("LRC_PROBE_BODY not set — nothing to probe");
            return;
        }
    };
    let raw = std::fs::read_to_string(&path).expect("read body");
    let body: Value = serde_json::from_str(&raw).expect("parse body");
    let before = body["messages"].as_array().cloned().unwrap_or_default();

    let before_fp = fingerprint(&before);
    let before_unpaired = unpaired(&before);
    println!("BEFORE: messages={} unpaired={}", before.len(), before_unpaired.len());
    if !before_unpaired.is_empty() {
        println!("  (the input itself already has unpaired tool results)");
    }

    for profile in [Profile::Conservative, Profile::Balanced, Profile::Aggressive, Profile::Extreme] {
        let mut cfg = lrc::LrcConfig::preset(profile);
        cfg.dump = None;
        let outcome = lrc::apply(&mut body.clone(), &cfg);
        let out = outcome
            .as_ref()
            .and_then(|o| o.body.get("messages").cloned())
            .and_then(|m| m.as_array().cloned())
            .unwrap_or_default();
        let after_fp = fingerprint(&out);
        let after_unpaired = unpaired(&out);

        let same_shape = before_fp == after_fp;
        let same_count = before.len() == out.len();
        let new_unpaired = after_unpaired.len() - before_unpaired.len();
        println!(
            "{profile:?}: reverted={} messages {}->{} shape_identical={} unpaired {}->{} (new={new_unpaired})",
            outcome.as_ref().map(|o| o.reverted).unwrap_or(true),
            before.len(),
            out.len(),
            same_shape,
            before_unpaired.len(),
            after_unpaired.len(),
        );
        if !same_shape {
            for (i, (a, b)) in before_fp.iter().zip(after_fp.iter()).enumerate() {
                if a != b {
                    println!("   first shape diff at msg {i}:");
                    println!("     before: {a}");
                    println!("     after : {b}");
                    break;
                }
            }
            if before_fp.len() != after_fp.len() {
                println!("   fingerprint length differs: {} vs {}", before_fp.len(), after_fp.len());
            }
        }
        assert!(same_count, "{profile:?}: the message count changed");
        assert!(same_shape, "{profile:?}: the message shape changed");
        assert_eq!(
            new_unpaired, 0,
            "{profile:?}: the pipeline created {new_unpaired} unpaired tool results"
        );
    }
}
