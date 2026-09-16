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
    let mut output: Vec<Value> = Vec::new();
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
        "status": "completed",
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        },
        "incomplete_details": if finish == "length" { json!({"reason": "max_output_tokens"}) } else { json!(null) },
        "error": Value::Null,
    })
}

/// Stateful driver that turns openai chat chunks into responses SSE events.
#[derive(Default)]
pub struct ResponsesStreamState {
    pub started: bool,
    pub text: String,
    pub seq: i64,
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

        if !self.started && !end_stream {
            self.started = true;
            let mut resp = response_skeleton(response_id, &model, chunk.get("created").and_then(|c| c.as_i64()).unwrap_or(0));
            resp["sequence_number"] = json!(seq());
            out.push(json!({"type": "response.created", "sequence_number": seq(), "response": resp}));
            out.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "sequence_number": seq(),
                "item": {"type": "message", "id": "msg_0", "role": "assistant", "status": "in_progress", "content": []}
            }));
            out.push(json!({
                "type": "response.content_part.added",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "sequence_number": seq(),
                "part": {"type": "output_text", "text": "", "annotations": []}
            }));
        }

        if let Some(choice) = chunk.pointer("/choices/0") {
            if let Some(text) = choice.pointer("/delta/content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    self.text.push_str(text);
                    out.push(json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_0",
                        "output_index": 0,
                        "content_index": 0,
                        "sequence_number": seq(),
                        "delta": text
                    }));
                }
            }
        }

        if end_stream {
            let mut resp = response_skeleton(response_id, &model, 0);
            resp["status"] = json!("completed");
            resp["output"] = json!([{
                "type": "message",
                "id": "msg_0",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.text, "annotations": []}]
            }]);
            if let Some(u) = chunk.get("usage") {
                resp["usage"] = json!({
                    "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
                    "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
                    "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
                });
            }
            out.push(json!({
                "type": "response.output_text.done",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "sequence_number": seq(),
                "text": self.text
            }));
            out.push(json!({"type": "response.completed", "sequence_number": seq(), "response": resp}));
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
}
