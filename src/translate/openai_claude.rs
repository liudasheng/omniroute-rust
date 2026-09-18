//! `openai ⇄ claude` translation (parity: `open-sse/translator/openai-to-claude.ts`,
//! `open-sse/translator/claude-to-openai.ts` and their response counterparts).

use serde_json::{json, Value};

pub fn reasoning_budget_for_effort(effort: &str) -> i64 {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" | "low" => 1_024,
        "medium" => 8_192,
        "high" => 32_768,
        "xhigh" | "max" => 65_536,
        _ => 1_024,
    }
}

// ---------- requests: claude → openai ----------

/// Convert an Anthropic Messages request body into an OpenAI Chat
/// Completions request body.
pub fn claude_request_to_openai(body: &Value) -> Value {
    let mut out = json!({
        "model": body.get("model").cloned().unwrap_or(json!("claude")),
        "max_tokens": body.get("max_tokens").cloned()
            .or_else(|| body.get("max_completion_tokens").cloned())
            .unwrap_or(json!(4096)),
    });
    for key in ["temperature", "top_p", "stop", "user", "metadata", "stream"] {
        if let Some(v) = body.get(key) {
            if !v.is_null() {
                out[key] = v.clone();
            }
        }
    }
    if let Some(ss) = body.get("stop_sequences") {
        out["stop"] = ss.clone();
    }
    if let Some(thinking) = body.get("thinking") {
        if thinking.get("type").and_then(|v| v.as_str()) == Some("enabled") {
            out["reasoning"] = json!({
                "enabled": true,
                "max_tokens": thinking.get("budget_tokens").cloned().unwrap_or(json!(1024)),
            });
        }
    }

    let mut messages: Vec<Value> = Vec::new();

    // system (string | blocks)
    if let Some(sys) = body.get("system") {
        let text = flatten_content(sys);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").cloned().unwrap_or(json!(""));
            match role {
                "user" => push_claude_user_to_openai(&mut messages, &content),
                "assistant" => push_claude_assistant_to_openai(&mut messages, &content),
                _ => push_claude_user_to_openai(&mut messages, &content),
            }
        }
    }
    out["messages"] = Value::Array(messages);

    // tools
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let oai_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?;
                let mut f = json!({"name": name});
                if let Some(d) = t.get("description") { f["description"] = d.clone(); }
                if let Some(s) = t.get("input_schema") { f["parameters"] = s.clone(); }
                Some(json!({"type": "function", "function": f}))
            })
            .collect();
        if !oai_tools.is_empty() {
            out["tools"] = Value::Array(oai_tools);
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        let t = tc.get("type").and_then(|v| v.as_str()).unwrap_or("auto");
        let mapped = match t {
            "any" => json!("required"),
            "auto" => json!("auto"),
            "none" => json!("none"),
            "tool" => json!({"type": "function", "function": {"name": tc["name"].clone()}}),
            _ => json!("auto"),
        };
        out["tool_choice"] = mapped;
    }

    // drop openai-incompatible keys already handled
    out
}

fn push_claude_user_to_openai(messages: &mut Vec<Value>, content: &Value) {
    match content {
        Value::String(s) => messages.push(json!({"role": "user", "content": s})),
        Value::Array(blocks) => {
            // group: text parts into a user message; tool_result blocks become
            // role:"tool" messages (openai tool results).
            let mut text_parts: Vec<Value> = Vec::new();
            for b in blocks {
                let btype = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        text_parts.push(json!({"type": "text", "text": b["text"].clone()}));
                    }
                    "image" => {
                        // claude image block → openai image_url part
                        text_parts.push(json!({
                            "type": "image_url",
                            "image_url": claude_image_source_to_url(&b["source"]),
                        }));
                    }
                    "document" => {
                        text_parts.push(claude_document_to_openai_part(b));
                    }
                    "tool_result" => {
                        if !text_parts.is_empty() {
                            messages.push(json!({"role": "user", "content": text_parts.clone()}));
                            text_parts.clear();
                        }
                        let tool_use_id = b.get("tool_use_id").cloned().unwrap_or(json!(""));
                        let inner = b.get("content").cloned().unwrap_or(json!(""));
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": flatten_content(&inner),
                        }));
                    }
                    _ => text_parts.push(json!({"type": "text", "text": flatten_content(b)})),
                }
            }
            if !text_parts.is_empty() {
                messages.push(json!({"role": "user", "content": text_parts}));
            }
        }
        other => messages.push(json!({"role": "user", "content": other.clone()})),
    }
}

fn push_claude_assistant_to_openai(messages: &mut Vec<Value>, content: &Value) {
    match content {
        Value::String(s) => messages.push(json!({"role": "assistant", "content": s})),
        Value::Array(blocks) => {
            let mut text_parts: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut reasoning_parts: Vec<String> = Vec::new();
            for b in blocks {
                let btype = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match btype {
                    "text" => text_parts.push(json!({"type": "text", "text": b.get("text").cloned().unwrap_or(json!(""))})),
                    "tool_use" => {
                        let arguments = match b.get("input") {
                            Some(v @ Value::Object(_)) => v.to_string(),
                            Some(v @ Value::String(_)) => v.as_str().unwrap_or("{}").to_string(),
                            _ => "{}".to_string(),
                        };
                        tool_calls.push(json!({
                            "id": b.get("id").cloned().unwrap_or(json!("call_0")),
                            "type": "function",
                            "function": {"name": b.get("name").cloned().unwrap_or(json!("")), "arguments": arguments}
                        }));
                    }
                    "thinking" => {
                        if let Some(text) = b.get("thinking").and_then(|v| v.as_str()) {
                            reasoning_parts.push(text.to_string());
                        }
                    }
                    "redacted_thinking" => {}
                    _ => text_parts.push(json!({"type": "text", "text": flatten_content(b)})),
                }
            }
            let mut msg = json!({"role": "assistant"});
            if !text_parts.is_empty() {
                if text_parts.len() == 1 {
                    msg["content"] = text_parts[0]["text"].clone();
                } else {
                    msg["content"] = Value::Array(text_parts);
                }
            }
            if !tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(tool_calls);
            }
            if !reasoning_parts.is_empty() {
                msg["reasoning_content"] = json!(reasoning_parts.join(""));
            }
            messages.push(msg);
        }
        other => messages.push(json!({"role": "assistant", "content": other.clone()})),
    }
}

/// Convert an openai `image_url` URL into a claude image block: `data:` URLs
/// become base64 sources; http(s) URLs become url sources.
pub fn openai_image_to_claude_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some(sep) = rest.find(";base64,") {
            let media = &rest[..sep];
            let data = &rest[sep + ";base64,".len()..];
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media, "data": data}
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

fn claude_image_source_to_url(src: &Value) -> Value {
    let stype = src.get("type").and_then(|t| t.as_str()).unwrap_or("url");
    match stype {
        "base64" => {
            let media = src.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png");
            let data = src.get("data").and_then(|d| d.as_str()).unwrap_or("");
            json!({"url": format!("data:{media};base64,{data}")})
        }
        _ => json!({"url": src.get("url").cloned().unwrap_or(json!(""))}),
    }
}

fn claude_document_to_openai_part(block: &Value) -> Value {
    let source = block.get("source").cloned().unwrap_or(json!({}));
    match source.get("type").and_then(Value::as_str).unwrap_or("url") {
        "base64" => {
            let media = source.get("media_type").and_then(Value::as_str).unwrap_or("application/pdf");
            let data = source.get("data").and_then(Value::as_str).unwrap_or("");
            json!({"type": "file", "file": {"filename": "document.pdf", "file_data": format!("data:{media};base64,{data}")}})
        }
        _ => json!({"type": "file", "file": {"filename": "document.pdf", "file_url": source.get("url").cloned().unwrap_or(json!(""))}}),
    }
}

fn openai_file_to_claude_block(part: &Value) -> Option<Value> {
    let file = part.get("file").unwrap_or(part);
    let data = file.get("file_data").or_else(|| file.get("file_data_url")).and_then(Value::as_str);
    if let Some(value) = data {
        if let Some(rest) = value.strip_prefix("data:") {
            if let Some(sep) = rest.find(";base64,") {
                return Some(json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": &rest[..sep], "data": &rest[sep + ";base64,".len()..]}
                }));
            }
        }
    }
    let url = file.get("file_url").or_else(|| file.get("url")).and_then(Value::as_str)?;
    Some(json!({"type": "document", "source": {"type": "url", "url": url}}))
}

/// Flatten claude content (string | blocks array) into plain text.
pub fn flatten_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => b.get("text").and_then(|t| t.as_str()).map(str::to_string),
                _ => b.get("text").and_then(|t| t.as_str()).map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

// ---------- responses: openai (JSON) → claude (JSON) ----------

pub fn openai_response_to_claude(oai: &Value, model: &str) -> Value {
    let choice = oai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let mut blocks: Vec<Value> = Vec::new();
    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        blocks.push(json!({"type": "thinking", "thinking": reasoning}));
    }
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    } else if let Some(parts) = message.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                blocks.push(json!({"type": "text", "text": p["text"].clone()}));
            }
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            let input: Value = f
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": tc.get("id").cloned().unwrap_or(json!("toolu_0")),
                "name": f.get("name").cloned().unwrap_or(json!("")),
                "input": input,
            }));
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    let finish = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("stop");
    let usage = oai.get("usage").cloned().unwrap_or(json!({}));
    let id = oai.get("id").and_then(|i| i.as_str()).unwrap_or("chatcmpl-omniroute");
    let msg_id = if id.starts_with("msg_") { id.to_string() } else { format!("msg_{}", id.trim_start_matches("chatcmpl-")) };
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": finish_reason_to_claude_stop(finish),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
        }
    })
}

pub fn finish_reason_to_claude_stop(finish: &str) -> &'static str {
    match finish {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

pub fn claude_stop_to_finish_reason(stop: &str) -> &'static str {
    match stop {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

// ---------- requests: openai → claude ----------

/// Convert an OpenAI Chat Completions request body into an Anthropic Messages
/// request body.
pub fn openai_request_to_claude(body: &Value) -> Value {
    let mut out = json!({
        "model": body.get("model").cloned().unwrap_or(json!("claude")),
        "max_tokens": body.get("max_tokens").cloned().unwrap_or(json!(4096)),
    });
    for key in ["temperature", "top_p", "metadata", "user"] {
        if let Some(v) = body.get(key) {
            if !v.is_null() {
                out[key] = v.clone();
            }
        }
    }
    if let Some(stop) = body.get("stop") {
        match stop {
            Value::String(s) => out["stop_sequences"] = json!([s]),
            Value::Array(_a) => out["stop_sequences"] = stop.clone(),
            _ => {}
        }
    }
    // OpenAI-compatible clients use reasoning fields. Anthropic needs an
    // explicit thinking block, so preserve the intent when routing to Claude.
    if let Some(reasoning) = body.get("reasoning") {
        if reasoning.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
            let budget = reasoning.get("max_tokens").cloned()
                .or_else(|| body.get("reasoning_budget").cloned())
                .or_else(|| reasoning.get("effort").and_then(Value::as_str).map(|e| json!(reasoning_budget_for_effort(e))))
                .or_else(|| body.get("max_completion_tokens").cloned())
                .unwrap_or(json!(1024));
            out["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
    } else if body.get("reasoning_effort").is_some() {
        let budget = body.get("reasoning_budget").cloned().or_else(|| body.get("reasoning_effort").and_then(Value::as_str).map(|e| json!(reasoning_budget_for_effort(e)))).unwrap_or(json!(1024));
        out["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    system_parts.push(flatten_content(&msg.get("content").cloned().unwrap_or(json!(""))));
                }
                "tool" => {
                    let inner = msg.get("content").cloned().unwrap_or(json!(""));
                    messages.push(json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": msg.get("tool_call_id").cloned().unwrap_or(json!("")), "content": inner}],
                    }));
                }
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    match msg.get("content") {
                        Some(Value::String(s)) if !s.is_empty() => blocks.push(json!({"type": "text", "text": s})),
                        Some(Value::Array(parts)) => {
                            for p in parts {
                                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    blocks.push(json!({"type": "text", "text": p["text"].clone()}));
                                }
                            }
                        }
                        _ => {}
                    }
                    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let f = tc.get("function").cloned().unwrap_or(json!({}));
                            let input: Value = f
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(json!({}));
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.get("id").cloned().unwrap_or(json!("toolu_0")),
                                "name": f.get("name").cloned().unwrap_or(json!("")),
                                "input": input,
                            }));
                        }
                    }
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
                _ => {
                    match msg.get("content") {
                        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
                        Some(Value::Array(parts)) => {
                            let mut blocks: Vec<Value> = Vec::new();
                            for p in parts {
                                let ptype = p.get("type").and_then(|t| t.as_str()).unwrap_or("text");
                                match ptype {
                                    "text" => blocks.push(json!({"type": "text", "text": p.get("text").cloned().unwrap_or(json!(""))})),
                                    "image_url" => {
                                        let url = p.pointer("/image_url/url").and_then(|u| u.as_str()).unwrap_or("");
                                        blocks.push(openai_image_to_claude_block(url));
                                    }
                                    "file" | "input_file" => {
                                        if let Some(block) = openai_file_to_claude_block(p) {
                                            blocks.push(block);
                                        }
                                    }
                                     _ => blocks.push(json!({"type": "text", "text": flatten_content(p)})),
                                }
                            }
                            messages.push(json!({"role": "user", "content": blocks}));
                        }
                        other => messages.push(json!({"role": "user", "content": other.cloned().unwrap_or(json!(""))})),
                    }
                }
            }
        }
    }
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n"));
    }
    out["messages"] = Value::Array(messages);

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let cl_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name")?.as_str()?;
                let mut c = json!({"name": name});
                if let Some(d) = f.get("description") { c["description"] = d.clone(); }
                if let Some(p) = f.get("parameters") { c["input_schema"] = p.clone(); }
                Some(c)
            })
            .collect();
        if !cl_tools.is_empty() {
            out["tools"] = Value::Array(cl_tools);
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        match tc {
            Value::String(s) => match s.as_str() {
                "required" => out["tool_choice"] = json!({"type": "any"}),
                "none" => {}
                _ => out["tool_choice"] = json!({"type": "auto"}),
            },
            Value::Object(_) => {
                let name = tc.pointer("/function/name").cloned().unwrap_or(json!(""));
                out["tool_choice"] = json!({"type": "tool", "name": name});
            }
            _ => {}
        }
    }
    out
}

// ---------- responses: claude (JSON) → openai (JSON) ----------

pub fn claude_response_to_openai(cl: &Value, model: &str) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = cl.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => text_parts.push(b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Some("thinking") => reasoning_parts.push(b.get("thinking").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Some("tool_use") => {
                    tool_calls.push(json!({
                        "id": b.get("id").cloned().unwrap_or(json!("call_0")),
                        "type": "function",
                        "function": {
                            "name": b.get("name").cloned().unwrap_or(json!("")),
                            "arguments": b.get("input").cloned().unwrap_or(json!({})).to_string(),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant"});
    if !text_parts.is_empty() {
        message["content"] = Value::String(text_parts.join(""));
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    if !reasoning_parts.is_empty() {
        message["reasoning_content"] = json!(reasoning_parts.join(""));
    }
    let stop = cl.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("end_turn");
    let usage = cl.get("usage").cloned().unwrap_or(json!({}));
    let id = cl.get("id").and_then(|i| i.as_str()).unwrap_or("msg_omniroute");
    let cid = if id.starts_with("chatcmpl-") { id.to_string() } else { format!("chatcmpl-{}", id.trim_start_matches("msg_")) };
    json!({
        "id": cid,
        "object": "chat.completion",
        "created": cl.get("created").cloned().unwrap_or(json!(0)),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": claude_stop_to_finish_reason(stop)}],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)).as_i64().unwrap_or(0)
                + usage.get("output_tokens").cloned().unwrap_or(json!(0)).as_i64().unwrap_or(0),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_request_with_system_and_tools_translates() {
        let cl = json!({
            "model": "claude-3",
            "max_tokens": 100,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tu1", "name": "get", "input": {"a": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu1", "content": "42"}]}
            ],
            "tools": [{"name": "get", "description": "Get it", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "get"},
            "stop_sequences": ["STOP"]
        });
        let oai = claude_request_to_openai(&cl);
        assert_eq!(oai["model"], "claude-3");
        assert_eq!(oai["max_tokens"], 100);
        assert_eq!(oai["stop"], json!(["STOP"]));
        assert_eq!(oai["stop"], json!(["STOP"]));
        let msgs = oai["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "get");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "tu1");
        assert_eq!(oai["tools"][0]["function"]["name"], "get");
        assert_eq!(oai["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(oai["tool_choice"]["function"]["name"], "get");
    }

    #[test]
    fn openai_request_to_claude_system_and_tools() {
        let oai = json!({
            "model": "gpt-4o",
            "max_tokens": 55,
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "add", "arguments": "{\"x\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "2"}
            ],
            "tools": [{"type": "function", "function": {"name": "add", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "stop": ["END", "STOP"]
        });
        let cl = openai_request_to_claude(&oai);
        assert_eq!(cl["system"], "Be terse.");
        assert_eq!(cl["max_tokens"], 55);
        assert_eq!(cl["stop_sequences"], json!(["END", "STOP"]));
        let msgs = cl["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["x"], 1);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(cl["tools"][0]["name"], "add");
        assert_eq!(cl["tool_choice"]["type"], "any");
    }

    #[test]
    fn openai_reasoning_fields_map_to_claude_thinking() {
        let cl = openai_request_to_claude(&json!({
            "model": "claude-sonnet-4-5",
            "max_completion_tokens": 4096,
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(cl["max_tokens"], 4096);
        assert_eq!(cl["thinking"]["type"], "enabled");
    }

    #[test]
    fn openai_json_to_claude_json() {
        let oai = json!({
            "id": "chatcmpl-123",
            "model": "gpt-4o",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": "thinking...",
                "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "f", "arguments": "{\"k\":2}"}}]
            }}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let cl = openai_response_to_claude(&oai, "gpt-4o");
        assert_eq!(cl["id"], "msg_123");
        assert_eq!(cl["type"], "message");
        assert_eq!(cl["role"], "assistant");
        assert_eq!(cl["content"][0]["type"], "text");
        assert_eq!(cl["content"][1]["type"], "tool_use");
        assert_eq!(cl["content"][1]["input"]["k"], 2);
        assert_eq!(cl["stop_reason"], "tool_use");
        assert_eq!(cl["usage"]["input_tokens"], 10);
        assert_eq!(cl["usage"]["output_tokens"], 5);
    }

    #[test]
    fn claude_json_to_openai_json() {
        let cl = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_use", "id": "tu", "name": "f", "input": {"q": true}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 7, "output_tokens": 3}
        });
        let oai = claude_response_to_openai(&cl, "claude-3");
        assert_eq!(oai["id"], "chatcmpl-01");
        assert_eq!(oai["object"], "chat.completion");
        let choice = &oai["choices"][0];
        assert_eq!(choice["message"]["content"], "hello world");
        assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(oai["usage"]["prompt_tokens"], 7);
        assert_eq!(oai["usage"]["completion_tokens"], 3);
        assert_eq!(oai["usage"]["total_tokens"], 10);
    }

    #[test]
    fn image_url_to_claude_data_url() {
        let oai = json!({
            "model": "claude-3", "max_tokens": 100,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,aGVsbG8="}}
            ]}]
        });
        let cl = openai_request_to_claude(&oai);
        let block = &cl["messages"][0]["content"][1];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        assert_eq!(block["source"]["data"], "aGVsbG8=");
    }

    #[test]
    fn claude_image_block_to_openai() {
        let cl = json!({
            "model": "c", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image", "source": {"type": "url", "url": "https://x.test/img.png"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
            ]}]
        });
        let oai = claude_request_to_openai(&cl);
        let parts = &oai["messages"][0]["content"];
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "https://x.test/img.png");
        assert_eq!(parts[2]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn pdf_parts_map_between_openai_and_claude() {
        let oai = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "summarize"},
                {"type": "file", "file": {"filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0="}}
            ]}]
        });
        let cl = openai_request_to_claude(&oai);
        assert_eq!(cl["messages"][0]["content"][1]["type"], "document");
        assert_eq!(cl["messages"][0]["content"][1]["source"]["media_type"], "application/pdf");

        let back = claude_request_to_openai(&json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": [{
                "type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}
            }]}]
        }));
        assert_eq!(back["messages"][0]["content"][0]["type"], "file");
        assert_eq!(back["messages"][0]["content"][0]["file"]["filename"], "document.pdf");
    }

    #[test]
    fn stop_reason_roundtrip() {
        for (finish, stop) in [("stop", "end_turn"), ("length", "max_tokens"), ("tool_calls", "tool_use")] {
            assert_eq!(finish_reason_to_claude_stop(finish), stop);
            assert_eq!(claude_stop_to_finish_reason(stop), finish);
        }
    }
}
