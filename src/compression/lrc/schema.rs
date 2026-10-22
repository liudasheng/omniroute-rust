//! LRC tools-schema compaction.
//!
//! The schema is the only part of an agent request that is *byte-identical on
//! every single turn*, so it is pure overhead: the median real request carries
//! ~21.5k tokens of tool definitions (up to 40k with MCP servers attached).
//!
//! Three deterministic modes:
//! - `spec`: strip noise keys, drop nested property descriptions, truncate
//!   top-level descriptions. Parameter names / required sets never change.
//! - `stub`: additionally flatten each parameter object to `{"type": ...}` and
//!   keep a one-line description (names + required still exact).
//! - `prune`: additionally drop tools whose namespace was never used in this
//!   conversation (opt-in: the model then cannot call them at all).

use crate::compression::lrc::gates::estimate_tokens_mixed;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode {
    Off,
    Spec,
    Stub,
    Prune,
}

impl SchemaMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "spec" | "lite" => Some(Self::Spec),
            "stub" => Some(Self::Stub),
            "prune" | "aggressive" => Some(Self::Prune),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Spec => "spec",
            Self::Stub => "stub",
            Self::Prune => "prune",
        }
    }
}

const NOISE_KEYS: &[&str] = &[
    "additionalProperties",
    "$schema",
    "$id",
    "$comment",
    "title",
    "deprecated",
    "examples",
    "example",
    "default",
];

/// OpenAI: `{type:"function", function:{...}}`; Anthropic: `{name, description, input_schema}`.
fn tool_core(tool: &Value) -> Option<&Value> {
    if let Some(f) = tool.get("function") {
        return Some(f);
    }
    if tool.get("name").is_some() {
        return Some(tool);
    }
    None
}

fn tool_name(tool: &Value) -> String {
    tool_core(tool)
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn param_key(tool: &Value) -> &'static str {
    let core = match tool_core(tool) {
        Some(c) => c,
        None => return "parameters",
    };
    if core.get("parameters").is_some() {
        "parameters"
    } else if core.get("input_schema").is_some() {
        "input_schema"
    } else {
        "parameters"
    }
}

/// Shape fingerprint that compression must preserve exactly.
fn fingerprint(tool: &Value) -> (String, Vec<String>, Vec<String>) {
    let core = match tool_core(tool) {
        Some(c) => c,
        None => return (String::new(), Vec::new(), Vec::new()),
    };
    let name = core.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let key = param_key(tool);
    let schema = core.get(key);
    let mut props: Vec<String> = schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    props.sort();
    let mut required: Vec<String> = schema
        .and_then(|s| s.get("required"))
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    required.sort();
    (name, props, required)
}

fn truncate_description(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        let mut replace: Option<String> = None;
        if let Some(d) = obj.get("description").and_then(|d| d.as_str()) {
            if d.chars().count() > 240 {
                let mut cut: String = d.chars().take(240).collect();
                if let Some(idx) = cut.rfind(' ') {
                    cut.truncate(idx);
                }
                cut.push('…');
                replace = Some(cut);
            }
        }
        if let Some(cut) = replace {
            obj.insert("description".to_string(), Value::String(cut));
        }
    }
}

/// Depth-limited rewrite: noise keys removed everywhere, nested descriptions
/// dropped below the schema root, `stub` flattens property schemas.
fn rewrite_schema(node: &Value, mode: SchemaMode, depth: usize, is_root: bool) -> Value {
    match node {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if NOISE_KEYS.contains(&k.as_str()) {
                    continue;
                }
                if k == "description" && depth >= 2 {
                    continue;
                }
                if mode == SchemaMode::Stub && depth >= 3 {
                    // property bodies collapse to their type only
                    if k != "type" && k != "enum" && k != "required" {
                        continue;
                    }
                }
                let child_is_root = false;
                out.insert(k.clone(), rewrite_schema(v, mode, depth + 1, child_is_root));
            }
            if is_root && mode != SchemaMode::Stub {
                // root descriptions are kept but truncated
                let mut val = Value::Object(out);
                truncate_description(&mut val);
                return val;
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| rewrite_schema(v, mode, depth, false)).collect()),
        other => other.clone(),
    }
}

fn rewrite_tool(tool: &Value, mode: SchemaMode) -> Value {
    let mut out = tool.clone();
    if let Some(core) = out.get("function").cloned() {
        let mut new_core = rewrite_schema(&core, mode, 0, true);
        // stub: properties collapse to `{"type": ...}`
        if mode == SchemaMode::Stub {
            let key = if core.get("parameters").is_some() { "parameters" } else { "input_schema" };
            if let Some(schema) = new_core.get_mut(key).and_then(|s| s.as_object_mut()) {
                if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                    for (_, pv) in props.iter_mut() {
                        if let Some(ty) = pv.get("type").cloned() {
                            *pv = json!({"type": ty});
                        }
                    }
                }
            }
        }
        out["function"] = new_core;
        return out;
    }
    // Anthropic-shaped tool
    let mut new_tool = rewrite_schema(tool, mode, 0, true);
    if mode == SchemaMode::Stub {
        let key = if tool.get("input_schema").is_some() { "input_schema" } else { "parameters" };
        if let Some(schema) = new_tool.get_mut(key).and_then(|s| s.as_object_mut()) {
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                for (_, pv) in props.iter_mut() {
                    if let Some(ty) = pv.get("type").cloned() {
                        *pv = json!({"type": ty});
                    }
                }
            }
        }
    }
    new_tool
}

fn namespace_of(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some(idx) = rest.find("__") {
            return &name[..5 + idx];
        }
    }
    ""
}

/// Compact a `tools` array. Returns the new array, the applied technique names
/// and (original_tokens, compacted_tokens).
pub fn compact_tools(tools: &Value, mode: SchemaMode, used_tools: &HashSet<String>) -> (Value, Vec<String>, (i64, i64)) {
    let empty: Vec<Value> = Vec::new();
    let arr = tools.as_array().unwrap_or(&empty);
    if arr.is_empty() || mode == SchemaMode::Off {
        return (tools.clone(), Vec::new(), (0, 0));
    }
    let original_tokens = estimate_tokens_mixed(&serde_json::to_string(tools).unwrap_or_default());
    let mut techniques: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    let used_namespaces: HashSet<String> = used_tools
        .iter()
        .map(|n| namespace_of(n).to_string())
        .filter(|n| !n.is_empty())
        .collect();

    for tool in arr {
        let name = tool_name(tool);
        if mode == SchemaMode::Prune && !name.is_empty() {
            let ns = namespace_of(&name);
            if !ns.is_empty() && !used_namespaces.contains(ns) {
                techniques.push("lrc-schema-prune".to_string());
                continue;
            }
        }
        let before_fp = fingerprint(tool);
        let compacted = rewrite_tool(tool, mode);
        let after_fp = fingerprint(&compacted);
        if before_fp != after_fp {
            // Never ship a schema whose callable shape changed.
            techniques.push("lrc-schema-reverted".to_string());
            out.push(tool.clone());
            continue;
        }
        if compacted != *tool {
            techniques.push("lrc-schema".to_string());
        }
        out.push(compacted);
    }

    let value = Value::Array(out);
    let compacted_tokens = estimate_tokens_mixed(&serde_json::to_string(&value).unwrap_or_default());
    (value, techniques, (original_tokens, compacted_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_tools() -> Value {
        json!([
            {"type": "function", "function": {
                "name": "read",
                "description": "Read a file from disk. ".to_string() + &"x".repeat(400),
                "parameters": {"type": "object", "title": "read_args",
                    "additionalProperties": false,
                    "properties": {
                        "file_path": {"type": "string", "title": "File Path",
                                      "description": "Absolute path of the file to read"},
                        "limit": {"type": "integer", "default": 2000, "description": "max lines"}
                    },
                    "required": ["file_path"]}}},
            {"type": "function", "function": {
                "name": "mcp__docs__search",
                "description": "Search documents",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}},"required": ["q"]}}}
        ])
    }

    #[test]
    fn spec_mode_preserves_callable_shape() {
        let tools = openai_tools();
        let used: HashSet<String> = ["read".to_string()].into_iter().collect();
        let (out, tech, (before, after)) = compact_tools(&tools, SchemaMode::Spec, &used);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for (orig, new) in tools.as_array().unwrap().iter().zip(arr.iter()) {
            assert_eq!(fingerprint(orig), fingerprint(new), "shape must not change");
        }
        assert!(after < before, "expected savings: {before} -> {after}");
        assert!(tech.contains(&"lrc-schema".to_string()));
        // noise keys are gone
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("additionalProperties"));
        assert!(!s.contains("\"title\""));
    }

    #[test]
    fn prune_drops_unused_namespaces() {
        let tools = openai_tools();
        let used: HashSet<String> = ["read".to_string()].into_iter().collect();
        let (out, tech, _) = compact_tools(&tools, SchemaMode::Prune, &used);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(tool_name(&arr[0]), "read");
        assert!(tech.contains(&"lrc-schema-prune".to_string()));
    }

    #[test]
    fn stub_keeps_names_and_required() {
        let tools = openai_tools();
        let used: HashSet<String> = HashSet::new();
        let (out, _, _) = compact_tools(&tools, SchemaMode::Stub, &used);
        for (orig, new) in tools.as_array().unwrap().iter().zip(out.as_array().unwrap().iter()) {
            assert_eq!(fingerprint(orig), fingerprint(new));
        }
    }

    #[test]
    fn off_mode_is_noop() {
        let tools = openai_tools();
        let used = HashSet::new();
        let (out, tech, stats) = compact_tools(&tools, SchemaMode::Off, &used);
        assert_eq!(out, tools);
        assert!(tech.is_empty());
        assert_eq!(stats, (0, 0));
    }
}
