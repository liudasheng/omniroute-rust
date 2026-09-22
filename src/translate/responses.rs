//! `openai-responses ⇄ openai-chat` translation (parity:
//! `open-sse/translator/openai-responses/*`).

use serde_json::{json, Value};

/// Convert a Chat Completions request body into an OpenAI Responses body
/// (reverse direction, used when the upstream speaks `openai-responses`).
pub fn chat_request_to_responses(chat: &Value) -> Value {
    let mut out = json!({
        "model": chat.get("model").cloned().unwrap_or(json!("gpt-5")),
    });
    for key in ["temperature", "top_p", "user", "stream", "metadata"] {
        if let Some(v) = chat.get(key) {
            if !v.is_null() {
                out[key] = v.clone();
            }
        }
    }
    if let Some(mt) = chat.get("max_tokens") {
        out["max_output_tokens"] = mt.clone();
    }
    if let Some(reasoning) = chat.get("reasoning") {
        out["reasoning"] = reasoning.clone();
    } else if let Some(effort) = chat.get("reasoning_effort") {
        out["reasoning"] = json!({"effort": effort});
    }

    let mut instructions: Vec<String> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    if let Some(arr) = chat.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match role {
                "system" | "developer" => instructions.push(crate::translate::openai_claude::flatten_content(&msg["content"])),
                "tool" => items.push(json!({
                    "type": "function_call_output",
                    "call_id": msg.get("tool_call_id").cloned().unwrap_or(json!("")),
                    "output": msg.get("content").cloned().unwrap_or(json!("")),
                })),
                "assistant" => {
                    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            items.push(json!({
                                "type": "function_call",
                                "call_id": tc.get("id").cloned().unwrap_or(json!("call_0")),
                                "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                                "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),
                            }));
                        }
                    }
                    if let Some(c) = msg.get("content") {
                        let text = crate::translate::openai_claude::flatten_content(c);
                        if !text.is_empty() {
                            items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}]
                            }));
                        }
                    }
                }
                _ => {
                    let text = match msg.get("content") {
                        Some(Value::String(s)) => json!(s),
                        Some(Value::Array(parts)) => {
                            let mapped: Vec<Value> = parts
                                .iter()
                                .filter_map(|p| {
                                    let t = p.get("type").and_then(|x| x.as_str())?;
                                    match t {
                                        "text" => Some(json!({"type": "input_text", "text": p.get("text").cloned().unwrap_or(json!(""))})),
                                        "image_url" => Some(json!({
                                            "type": "input_image",
                                            "image_url": p.pointer("/image_url/url").cloned().unwrap_or(json!("")),
                                        })),
                                        _ => None,
                                    }
                                })
                                .collect();
                            if mapped.len() == 1 && mapped[0]["type"] == "input_text" {
                                mapped[0]["text"].clone()
                            } else if mapped.is_empty() {
                                json!("")
                            } else {
                                Value::Array(mapped)
                            }
                        }
                        other => other.cloned().unwrap_or(json!("")),
                    };
                    items.push(json!({
                        "type": "message",
                        "role": role,
                        "content": text,
                    }));
                }
            }
        }
    }
    if !instructions.is_empty() {
        out["instructions"] = json!(instructions.join("\n"));
    }
    out["input"] = Value::Array(items);
    out
}

/// Convert an OpenAI Responses request body into a Chat Completions body.
pub fn responses_request_to_chat(body: &Value) -> Value {
    let mut out = json!({
        "model": body.get("model").cloned().unwrap_or(json!("gpt-5")),
    });
    for key in ["temperature", "top_p", "user", "stream", "tools", "metadata"] {
        if let Some(v) = body.get(key) {
            if !v.is_null() {
                out[key] = v.clone();
            }
        }
    }
    if let Some(mot) = body.get("max_output_tokens") {
        out["max_tokens"] = mot.clone();
    }
    if let Some(reasoning) = body.get("reasoning") {
        out["reasoning"] = reasoning.clone();
        if let Some(effort) = reasoning.get("effort").and_then(Value::as_str) {
            out["reasoning_effort"] = json!(effort);
        }
    }
    // Responses tools use a flat {"type":"function","name":...} shape; convert.
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let chat_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                if t.get("type").and_then(|x| x.as_str()) == Some("function") && t.get("function").is_none() {
                    let mut f = json!({"name": t.get("name").cloned().unwrap_or(json!(""))});
                    if let Some(d) = t.get("description") { f["description"] = d.clone(); }
                    if let Some(p) = t.get("parameters") { f["parameters"] = p.clone(); }
                    json!({"type": "function", "function": f})
                } else {
                    t.clone()
                }
            })
            .collect();
        if !chat_tools.is_empty() {
            out["tools"] = Value::Array(chat_tools);
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = body.get("instructions") {
        if !instr.is_null() {
            messages.push(json!({"role": "system", "content": instr}));
        }
    }
    match body.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for item in items {
                let itype = item.get("type").and_then(|t| t.as_str()).unwrap_or("message");
                match itype {
                    "message" => {
                        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                        let content = match item.get("content") {
                            Some(Value::String(s)) => json!(s),
                            Some(Value::Array(parts)) => {
                                let mapped: Vec<Value> = parts
                                    .iter()
                                    .filter_map(|p| {
                                        let ptype = p.get("type").and_then(|x| x.as_str())?;
                                        match ptype {
                                            "input_text" | "output_text" | "text" => Some(
                                                json!({"type": "text", "text": p.get("text").cloned().unwrap_or(json!(""))}),
                                            ),
                                            "input_image" => Some(json!({
                                                "type": "image_url",
                                                "image_url": {"url": p.pointer("/image_url").cloned().unwrap_or(json!(""))}
                                            })),
                                            "input_file" => Some(json!({
                                                "type": "file",
                                                "file": {
                                                    "filename": p.get("filename").cloned().unwrap_or(json!("document.pdf")),
                                                    "file_data": p.get("file_data").or_else(|| p.get("file_data_url")).cloned().unwrap_or(Value::Null),
                                                    "file_url": p.get("file_url").cloned().unwrap_or(Value::Null),
                                                }
                                            })),
                                            _ => None,
                                        }
                                    })
                                    .collect();
                                if mapped.len() == 1 && mapped[0]["type"] == "text" {
                                    mapped[0]["text"].clone()
                                } else if mapped.is_empty() {
                                    json!("")
                                } else {
                                    Value::Array(mapped)
                                }
                            }
                            other => other.cloned().unwrap_or(json!("")),
                        };
                        messages.push(json!({"role": role, "content": content}));
                    }
                    "function_call" => {
                        messages.push(json!({
                            "role": "assistant",
                            "tool_calls": [{
                                "id": item.get("call_id").cloned().unwrap_or(item.get("id").cloned().unwrap_or(json!("call_0"))),
                                "type": "function",
                                "function": {
                                    "name": item.get("name").cloned().unwrap_or(json!("")),
                                    "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                                }
                            }]
                        }));
                    }
                    "function_call_output" => {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or(json!("")),
                            "content": item.get("output").cloned().unwrap_or(json!("")),
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out["messages"] = Value::Array(messages);
    out
}

/// Convert an OpenAI Responses JSON object into a canonical Chat Completion.
/// Responses and Chat use different top-level envelopes even when they share
/// the same model, so treating a Responses object as a Chat object yields an
/// empty assistant message.
pub fn responses_response_to_chat(body: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(output) = body.get("output").and_then(|v| v.as_array()) {
        for item in output {
            match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "message" => {
                    if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                        for part in content {
                            if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                if let Some(value) = part.get("text").and_then(|v| v.as_str()) {
                                    text.push_str(value);
                                }
                            }
                        }
                    }
                }
                "reasoning" => {
                    if let Some(summary) = item.get("summary").and_then(|v| v.as_array()) {
                        for part in summary {
                            if let Some(value) = part.get("text").and_then(|v| v.as_str()) {
                                reasoning.push_str(value);
                            }
                        }
                    }
                }
                "function_call" => {
                    let id = item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(json!("call_0"));
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": item.get("name").cloned().unwrap_or(json!("")),
                            "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant"});
    if !text.is_empty() { message["content"] = json!(text); }
    if !reasoning.is_empty() { message["reasoning_content"] = json!(reasoning); }
    if !tool_calls.is_empty() { message["tool_calls"] = Value::Array(tool_calls); }
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("completed");
    let finish = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else if status == "incomplete" || body.pointer("/incomplete_details/reason").and_then(|v| v.as_str()) == Some("max_output_tokens") {
        "length"
    } else if status == "failed" {
        "content_filter"
    } else {
        "stop"
    };
    let usage = body.get("usage").cloned().unwrap_or(json!({}));
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("resp-omniroute");
    json!({
        "id": format!("chatcmpl-{}", id.trim_start_matches("resp_")),
        "object": "chat.completion",
        "created": body.get("created_at").cloned().unwrap_or(json!(0)),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        }
    })
}

/// Build the `response` skeleton shared by SSE events.
pub fn response_skeleton(response_id: &str, model: &str, created: i64) -> Value {
    json!({
        "id": response_id,
        "object": "response",
        "created_at": created,
        "status": "in_progress",
        "model": model,
        "output": [],
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "usage": Value::Null,
    })
}

/// Convert a Chat Completions response into an OpenAI Responses object.
pub fn chat_response_to_responses(chat: &Value, model: &str) -> Value {
    let choice = chat
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let text = message.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let usage = chat.get("usage").cloned().unwrap_or(json!({}));
    let finish = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("stop");
    let incomplete_reason = match finish {
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
    };
    let mut output: Vec<Value> = Vec::new();
    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        output.push(json!({
            "type": "reasoning",
            "id": "rs_0",
            "summary": [{"type": "summary_text", "text": reasoning}],
            "status": "completed"
        }));
    }
    if !text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": "msg_0",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            output.push(json!({
                "type": "function_call",
                "id": "fc_0",
                "call_id": tc.get("id").cloned().unwrap_or(json!("call_0")),
                "name": f.get("name").cloned().unwrap_or(json!("")),
                "arguments": f.get("arguments").cloned().unwrap_or(json!("{}")),
                "status": "completed",
            }));
        }
    }
    json!({
        "id": chat.get("id").and_then(|i| i.as_str()).unwrap_or("resp_omniroute"),
        "object": "response",
        "created_at": chat.get("created").cloned().unwrap_or(json!(0)),
        "status": if incomplete_reason.is_some() { "incomplete" } else { "completed" },
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        },
        "incomplete_details": incomplete_reason.map(|reason| json!({"reason": reason})).unwrap_or(Value::Null),
        "error": Value::Null,
    })
}

/// Convert Responses SSE events into canonical OpenAI Chat chunks.
#[derive(Default)]
pub struct ResponsesToOpenaiStream {
    pub response_id: String,
    pub model: String,
    pub created: i64,
    role_sent: bool,
    next_tool: i64,
    tool_indexes: std::collections::HashMap<String, i64>,
    has_tool: bool,
}

impl ResponsesToOpenaiStream {
    pub fn translate(&mut self, event: &crate::sse::SseEvent) -> Vec<Value> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else { return Vec::new() };
        let kind = event.event.as_deref().or_else(|| data.get("type").and_then(|v| v.as_str())).unwrap_or("");
        let response = data.get("response").unwrap_or(&data);
        if self.response_id.is_empty() {
            self.response_id = response.get("id").and_then(|v| v.as_str()).unwrap_or("resp-omniroute").to_string();
            self.model = response.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
            self.created = response.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
        }
        let id = if self.response_id.starts_with("chatcmpl-") { self.response_id.clone() } else { format!("chatcmpl-{}", self.response_id.trim_start_matches("resp_")) };
        match kind {
            "response.created" | "response.in_progress" => {
                if self.role_sent { Vec::new() } else {
                    self.role_sent = true;
                    vec![crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({"role": "assistant"}), None)]
                }
            }
            "response.output_text.delta" => {
                let value = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if value.is_empty() { Vec::new() } else { self.role_sent = true; vec![crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({"content": value}), None)] }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let value = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if value.is_empty() { Vec::new() } else { vec![crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({"reasoning_content": value}), None)] }
            }
            "response.output_item.added" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(|v| v.as_str()) != Some("function_call") { return Vec::new(); }
                let item_id = item.get("id").or_else(|| item.get("call_id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let index = self.next_tool;
                self.next_tool += 1;
                self.tool_indexes.insert(item_id.clone(), index);
                self.has_tool = true;
                vec![crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({"tool_calls": [{"index": index, "id": item_id, "type": "function", "function": {"name": item.get("name").cloned().unwrap_or(json!("")), "arguments": ""}}]}), None)]
            }
            "response.function_call_arguments.delta" => {
                let item_id = data.get("item_id").or_else(|| data.get("call_id")).and_then(|v| v.as_str()).unwrap_or("");
                let index = *self.tool_indexes.entry(item_id.to_string()).or_insert_with(|| { let i = self.next_tool; self.next_tool += 1; i });
                let value = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                vec![crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({"tool_calls": [{"index": index, "function": {"arguments": value}}]}), None)]
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let finish = if kind == "response.incomplete" { "length" } else if self.has_tool { "tool_calls" } else { "stop" };
                let mut chunk = crate::translate::gemini::openai_chunk(&id, self.created, &self.model, json!({}), Some(finish.to_string()));
                if let Some(usage) = response.get("usage") { chunk["usage"] = json!({"prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)), "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)), "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0))}); }
                vec![chunk]
            }
            _ => Vec::new(),
        }
    }
}

/// Stateful driver that turns openai chat chunks into responses SSE events.
#[derive(Debug)]
struct ResponseToolStream {
    output_index: i64,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct ResponsesStreamState {
    pub started: bool,
    pub text: String,
    pub seq: i64,
    model: String,
    message_output_index: Option<i64>,
    next_output_index: i64,
    tools: std::collections::BTreeMap<i64, ResponseToolStream>,
    usage: Option<Value>,
    finish_reason: Option<String>,
}

impl ResponsesStreamState {
    /// Produce responses-format SSE event values for one openai chat chunk.
    /// `end_stream` finalizes the response. Returned values are serialized as
    /// `event: <type>\ndata: <json>` frames by the caller.
    pub fn translate_chunk(&mut self, chunk: &Value, end_stream: bool, response_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut seq = || {
            self.seq += 1;
            self.seq
        };
        let model = chunk.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
        if self.model.is_empty() && model != "unknown" {
            self.model = model.clone();
        }
        if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }

        if !self.started && !end_stream {
            self.started = true;
            let mut resp = response_skeleton(response_id, &model, chunk.get("created").and_then(|c| c.as_i64()).unwrap_or(0));
            resp["sequence_number"] = json!(seq());
            out.push(json!({"type": "response.created", "sequence_number": seq(), "response": resp}));
        }

        if let Some(choice) = chunk.pointer("/choices/0") {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            if let Some(text) = choice.pointer("/delta/content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    let output_index = if let Some(index) = self.message_output_index {
                        index
                    } else {
                        let index = self.next_output_index;
                        self.next_output_index += 1;
                        self.message_output_index = Some(index);
                        out.push(json!({
                            "type": "response.output_item.added",
                            "output_index": index,
                            "sequence_number": seq(),
                            "item": {"type": "message", "id": "msg_0", "role": "assistant", "status": "in_progress", "content": []}
                        }));
                        out.push(json!({
                            "type": "response.content_part.added",
                            "item_id": "msg_0",
                            "output_index": index,
                            "content_index": 0,
                            "sequence_number": seq(),
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }));
                        index
                    };
                    self.text.push_str(text);
                    out.push(json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_0",
                        "output_index": output_index,
                        "content_index": 0,
                        "sequence_number": seq(),
                        "delta": text
                    }));
                }
            }
            if let Some(tool_calls) = choice.pointer("/delta/tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let tool_index = tool_call.get("index").and_then(Value::as_i64).unwrap_or(0);
                    if !self.tools.contains_key(&tool_index) {
                        let call_id = tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .unwrap_or("call_0")
                            .to_string();
                        let item_id = if call_id.starts_with("fc_") {
                            call_id.clone()
                        } else {
                            format!("fc_{call_id}")
                        };
                        let name = tool_call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        self.tools.insert(
                            tool_index,
                            ResponseToolStream {
                                output_index,
                                item_id: item_id.clone(),
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: String::new(),
                            },
                        );
                        out.push(json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "sequence_number": seq(),
                            "item": {"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress"}
                        }));
                    }
                    if let Some(arguments) = tool_call.pointer("/function/arguments").and_then(Value::as_str).filter(|value| !value.is_empty()) {
                        if let Some(tool) = self.tools.get_mut(&tool_index) {
                            tool.arguments.push_str(arguments);
                            out.push(json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": tool.item_id,
                                "output_index": tool.output_index,
                                "sequence_number": seq(),
                                "delta": arguments
                            }));
                        }
                    }
                }
            }
        }

        if end_stream {
            let incomplete_reason = match self.finish_reason.as_deref() {
                Some("length") => Some("max_output_tokens"),
                Some("content_filter") => Some("content_filter"),
                _ => None,
            };
            let item_status = if incomplete_reason.is_some() { "incomplete" } else { "completed" };
            if let Some(output_index) = self.message_output_index {
                out.push(json!({
                    "type": "response.output_text.done",
                    "item_id": "msg_0",
                    "output_index": output_index,
                    "content_index": 0,
                    "sequence_number": seq(),
                    "text": self.text
                }));
                out.push(json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "sequence_number": seq(),
                    "item": {"type": "message", "id": "msg_0", "role": "assistant", "status": item_status, "content": [{"type": "output_text", "text": self.text, "annotations": []}]}
                }));
            }
            for tool in self.tools.values() {
                out.push(json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": tool.item_id,
                    "output_index": tool.output_index,
                    "sequence_number": seq(),
                    "arguments": tool.arguments
                }));
                out.push(json!({
                    "type": "response.output_item.done",
                    "output_index": tool.output_index,
                    "sequence_number": seq(),
                    "item": {"type": "function_call", "id": tool.item_id, "call_id": tool.call_id, "name": tool.name, "arguments": tool.arguments, "status": item_status}
                }));
            }
            let mut resp = response_skeleton(response_id, if self.model.is_empty() { &model } else { &self.model }, 0);
            resp["status"] = json!(if incomplete_reason.is_some() { "incomplete" } else { "completed" });
            if let Some(reason) = incomplete_reason {
                resp["incomplete_details"] = json!({"reason": reason});
            }
            let mut output = Vec::new();
            if self.message_output_index.is_some() {
                output.push(json!({
                    "type": "message", "id": "msg_0", "role": "assistant", "status": item_status,
                    "content": [{"type": "output_text", "text": self.text, "annotations": []}]
                }));
            }
            for tool in self.tools.values() {
                output.push(json!({
                    "type": "function_call", "id": tool.item_id, "call_id": tool.call_id,
                    "name": tool.name, "arguments": tool.arguments, "status": item_status
                }));
            }
            resp["output"] = Value::Array(output);
            if let Some(u) = self.usage.as_ref().or_else(|| chunk.get("usage")) {
                resp["usage"] = json!({
                    "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
                    "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
                    "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
                });
            }
            let event_type = if incomplete_reason.is_some() { "response.incomplete" } else { "response.completed" };
            out.push(json!({"type": event_type, "sequence_number": seq(), "response": resp}));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_request_string_input() {
        let body = json!({
            "model": "gpt-5",
            "instructions": "Be brief.",
            "input": "hello",
            "max_output_tokens": 77
        });
        let chat = responses_request_to_chat(&body);
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be brief.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hello");
        assert_eq!(chat["max_tokens"], 77);
    }

    #[test]
    fn responses_request_items_input() {
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]
        });
        let chat = responses_request_to_chat(&body);
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "c1");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        assert_eq!(msgs[2]["content"], "ok");
    }

    #[test]
    fn responses_reasoning_effort_maps_to_chat_and_back() {
        let chat = responses_request_to_chat(&json!({
            "model": "custom-model",
            "reasoning": {"effort": "high"},
            "input": "hello"
        }));
        assert_eq!(chat["reasoning_effort"], "high");

        let responses = chat_request_to_responses(&json!({
            "model": "custom-model",
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "hello"}]
        }));
        assert_eq!(responses["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_json_maps_assistant_text_and_usage() {
        let chat = responses_response_to_chat(&json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "model": "custom-model",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "2"}]}],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        }), "custom-model");
        assert_eq!(chat["choices"][0]["message"]["content"], "2");
        assert_eq!(chat["choices"][0]["finish_reason"], "stop");
        assert_eq!(chat["usage"]["total_tokens"], 4);
    }

    #[test]
    fn responses_stream_maps_text_to_chat_chunks() {
        let mut stream = ResponsesToOpenaiStream::default();
        let created = crate::sse::SseEvent { event: Some("response.created".into()), data: json!({"type":"response.created","response":{"id":"resp_1","model":"custom-model","created_at":1}}).to_string() };
        let delta = crate::sse::SseEvent { event: Some("response.output_text.delta".into()), data: json!({"type":"response.output_text.delta","delta":"2"}).to_string() };
        let completed = crate::sse::SseEvent { event: Some("response.completed".into()), data: json!({"type":"response.completed","response":{"id":"resp_1","model":"custom-model","usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}).to_string() };
        let first = stream.translate(&created);
        let text = stream.translate(&delta);
        let last = stream.translate(&completed);
        assert_eq!(first[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(text[0]["choices"][0]["delta"]["content"], "2");
        assert_eq!(last[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(last[0]["usage"]["total_tokens"], 4);
    }

    #[test]
    fn responses_input_file_projects_to_chat_file_part() {
        let chat = responses_request_to_chat(&json!({
            "model": "gpt-5.6-luna",
            "input": [{"type": "message", "role": "user", "content": [{
                "type": "input_file", "filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0="
            }]}]
        }));
        assert_eq!(chat["messages"][0]["content"][0]["type"], "file");
        assert_eq!(chat["messages"][0]["content"][0]["file"]["filename"], "a.pdf");
    }

    #[test]
    fn flat_tools_shape_converted() {
        let body = json!({
            "model": "gpt-5",
            "input": "x",
            "tools": [{"type": "function", "name": "f", "description": "d", "parameters": {"type": "object"}}]
        });
        let chat = responses_request_to_chat(&body);
        assert_eq!(chat["tools"][0]["type"], "function");
        assert_eq!(chat["tools"][0]["function"]["name"], "f");
        assert_eq!(chat["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn chat_json_to_responses() {
        let chat = json!({
            "id": "chatcmpl-1", "created": 5, "model": "gpt-5",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "hi there"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let resp = chat_response_to_responses(&chat, "gpt-5");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"][0]["content"][0]["text"], "hi there");
        assert_eq!(resp["usage"]["input_tokens"], 3);
        assert_eq!(resp["usage"]["output_tokens"], 4);
    }

    #[test]
    fn responses_stream_events() {
        let mut st = ResponsesStreamState::default();
        let chunk1 = json!({
            "id": "c1", "created": 5, "model": "m",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "he"}, "finish_reason": null}]
        });
        let evs = st.translate_chunk(&chunk1, false, "resp_1");
        assert_eq!(evs[0]["type"], "response.created");
        assert_eq!(evs[1]["type"], "response.output_item.added");
        assert_eq!(evs[2]["type"], "response.content_part.added");
        assert_eq!(evs[3]["type"], "response.output_text.delta");
        assert_eq!(evs[3]["delta"], "he");

        let chunk2 = json!({
            "id": "c1", "created": 5, "model": "m",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4}
        });
        let evs = st.translate_chunk(&chunk2, true, "resp_1");
        assert_eq!(evs.last().unwrap()["type"], "response.completed");
        assert_eq!(evs.last().unwrap()["response"]["output"][0]["content"][0]["text"], "he");
        assert_eq!(evs.last().unwrap()["response"]["usage"]["input_tokens"], 2);
    }

    #[test]
    fn responses_stream_events_preserve_tool_calls() {
        let mut st = ResponsesStreamState::default();
        let start = json!({
            "id": "c1", "created": 5, "model": "m",
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        });
        let call = json!({
            "id": "c1", "created": 5, "model": "m",
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "exec", "arguments": "{\"command\":\"printf ok\"}"}}]}, "finish_reason": null}]
        });
        let start_events = st.translate_chunk(&start, false, "resp_1");
        let call_events = st.translate_chunk(&call, false, "resp_1");
        let done_events = st.translate_chunk(&json!({"usage": {"prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4}}), true, "resp_1");
        assert_eq!(start_events[0]["type"], "response.created");
        let added = call_events.iter().find(|event| event["type"] == "response.output_item.added").unwrap();
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["name"], "exec");
        assert!(call_events.iter().any(|event| event["type"] == "response.function_call_arguments.delta"));
        assert!(done_events.iter().any(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"));
        let completed = done_events.iter().find(|event| event["type"] == "response.completed").unwrap();
        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
    }

    #[test]
    fn responses_stream_length_is_incomplete() {
        let mut st = ResponsesStreamState::default();
        st.translate_chunk(
            &json!({
                "id": "c1", "created": 5, "model": "m",
                "choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": null}]
            }),
            false,
            "resp_1",
        );
        let events = st.translate_chunk(
            &json!({
                "id": "c1", "created": 5, "model": "m",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]
            }),
            true,
            "resp_1",
        );
        let incomplete = events.iter().find(|event| event["type"] == "response.incomplete").unwrap();
        assert_eq!(incomplete["response"]["status"], "incomplete");
        assert_eq!(incomplete["response"]["incomplete_details"]["reason"], "max_output_tokens");
        assert!(!events.iter().any(|event| event["type"] == "response.completed"));
    }
}
