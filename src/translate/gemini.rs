//! `openai ⇄ gemini` translation (parity: `open-sse/translator/openai-to-gemini.ts`
//! + gemini SSE stream → openai chunks).

use serde_json::{json, Value};

// ---------- request: openai → gemini ----------

/// Convert an openai `image_url` part into a gemini part: data URLs become
/// inlineData (base64); http(s) URLs become fileData (fileUri).
pub fn openai_image_to_gemini_part(p: &Value) -> Option<Value> {
    let url = p.pointer("/image_url/url").and_then(|u| u.as_str())?;
    if let Some(rest) = url.strip_prefix("data:") {
        let sep = rest.find(";base64,")?;
        let media = &rest[..sep];
        let data = &rest[sep + ";base64,".len()..];
        return Some(json!({"inlineData": {"mimeType": media, "data": data}}));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({"fileData": {"fileUri": url, "mimeType": "image/png"}}));
    }
    None
}

pub fn openai_file_to_gemini_part(p: &Value) -> Option<Value> {
    let file = p.get("file").unwrap_or(p);
    if let Some(value) = file.get("file_data").or_else(|| file.get("file_data_url")).and_then(Value::as_str) {
        if let Some(rest) = value.strip_prefix("data:") {
            let sep = rest.find(";base64,")?;
            return Some(json!({"inlineData": {
                "mimeType": &rest[..sep],
                "data": &rest[sep + ";base64,".len()..]
            }}));
        }
    }
    let url = file.get("file_url").or_else(|| file.get("url")).and_then(Value::as_str)?;
    Some(json!({"fileData": {"fileUri": url, "mimeType": "application/pdf"}}))
}

pub fn openai_request_to_gemini(body: &Value) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_text: Vec<String> = Vec::new();

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        // Gemini matches a `functionResponse` to its `functionDeclarations` by
        // the *tool name*. A `tool_call_id` is an OpenAI-side handle the model
        // has never seen, so a response that carried it could not be paired
        // with anything and the agent loop broke on this lane. Pre-scan the
        // assistant turns for id → tool name, then answer by name.
        let mut call_name: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for msg in arr {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) else {
                continue;
            };
            for tc in tcs {
                let (Some(id), Some(name)) = (
                    tc.get("id").and_then(|v| v.as_str()),
                    tc.pointer("/function/name").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                call_name.insert(id.to_string(), name.to_string());
            }
        }

        for msg in arr {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    system_text.push(crate::translate::openai_claude::flatten_content(&msg["content"]));
                }
                "assistant" => {
                    let mut parts: Vec<Value> = Vec::new();
                    match msg.get("content") {
                        Some(Value::String(s)) if !s.is_empty() => parts.push(json!({"text": s})),
                        Some(Value::Array(pieces)) => {
                            for p in pieces {
                                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    parts.push(json!({"text": p["text"].clone()}));
                                }
                            }
                        }
                        _ => {}
                    }
                    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let f = tc.get("function").cloned().unwrap_or(json!({}));
                            let args: Value = f
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(json!({}));
                            parts.push(json!({"functionCall": {"name": f.get("name").cloned().unwrap_or(json!("")), "args": args}}));
                        }
                    }
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                "tool" => {
                    // The name the *model* declared, not the id it never saw.
                    let name = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| call_name.get(id).cloned())
                        .map(Value::String)
                        .unwrap_or_else(|| msg.get("tool_call_id").cloned().unwrap_or(json!("")));
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"functionResponse": {"name": name, "response": {"result": msg.get("content").cloned().unwrap_or(json!(""))}}}]
                    }));
                }
                _ => {
                    match msg.get("content") {
                        Some(Value::String(s)) => contents.push(json!({"role": "user", "parts": [{"text": s}]})),
                        Some(Value::Array(pieces)) => {
                            let mut parts: Vec<Value> = Vec::new();
                            for p in pieces {
                                let t = p.get("type").and_then(|x| x.as_str()).unwrap_or("text");
                                match t {
                                    "text" => parts.push(json!({"text": p.get("text").cloned().unwrap_or(json!(""))})),
                                    "image_url" => {
                                        if let Some(part) = openai_image_to_gemini_part(p) {
                                            parts.push(part);
                                        }
                                    }
                                    "file" | "input_file" => {
                                        if let Some(part) = openai_file_to_gemini_part(p) {
                                            parts.push(part);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            contents.push(json!({"role": "user", "parts": parts}));
                        }
                        other => contents.push(json!({"role": "user", "parts": [{"text": other.cloned().unwrap_or(json!(""))}]})),
                    }
                }
            }
        }
    }

    let mut out = json!({"contents": contents});
    if !system_text.is_empty() {
        out["systemInstruction"] = json!({"parts": [{"text": system_text.join("\n")}]});
    }
    // Tool declarations: without them the model is offered no tools at all and
    // the request silently degrades to plain chat — the agent gets no
    // function calls back. The gateway normalizes the tools array before any
    // dialect translation, so each entry is the nested `{type, function}`
    // shape; a flat entry is accepted too.
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mut declarations: Vec<Value> = Vec::new();
        for tool in tools {
            let f = tool.get("function").unwrap_or(tool);
            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut decl = json!({"name": name});
            if let Some(d) = f.get("description") {
                decl["description"] = d.clone();
            }
            if let Some(p) = f.get("parameters").or_else(|| f.get("input_schema")) {
                decl["parameters"] = p.clone();
            }
            declarations.push(decl);
        }
        if !declarations.is_empty() {
            out["tools"] = json!([{"functionDeclarations": declarations}]);
            // AUTO keeps the model free to answer in prose when no tool is
            // needed, which is what an OpenAI `tool_choice: "auto"` means.
            out["toolConfig"] = json!({"functionCallingConfig": {"mode": "AUTO"}});
        }
    }
    let mut cfg = json!({});
    if let Some(t) = body.get("temperature") { cfg["temperature"] = t.clone(); }
    if let Some(tp) = body.get("top_p") { cfg["topP"] = tp.clone(); }
    if let Some(mt) = body.get("max_tokens") { cfg["maxOutputTokens"] = mt.clone(); }
    if let Some(stop) = body.get("stop") {
        match stop {
            Value::String(s) => cfg["stopSequences"] = json!([s]),
            Value::Array(_) => cfg["stopSequences"] = stop.clone(),
            _ => {}
        }
    }
    if let Some(reasoning) = body.get("reasoning") {
        if reasoning.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
            let mut thinking = json!({});
            if let Some(budget) = reasoning.get("max_tokens").or_else(|| body.get("reasoning_budget")) {
                thinking["thinkingBudget"] = budget.clone();
            } else if let Some(effort) = reasoning.get("effort").and_then(Value::as_str) {
                thinking["thinkingBudget"] = json!(crate::translate::openai_claude::reasoning_budget_for_effort(effort));
            }
            cfg["thinkingConfig"] = thinking;
        }
    } else if body.get("reasoning_effort").is_some() {
        let budget = body.get("reasoning_budget").cloned().or_else(|| body.get("reasoning_effort").and_then(Value::as_str).map(|e| json!(crate::translate::openai_claude::reasoning_budget_for_effort(e)))).unwrap_or(json!(1024));
        cfg["thinkingConfig"] = json!({"thinkingBudget": budget});
    }
    if cfg.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        out["generationConfig"] = cfg;
    }
    out
}

// ---------- response: gemini (JSON) → openai (JSON) ----------

pub fn gemini_response_to_openai(g: &Value, model: &str) -> Value {
    let cand = g
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(json!({}));
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tc_idx = 0usize;
    if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(fc) = p.get("functionCall") {
                tc_idx += 1;
                tool_calls.push(json!({
                    "id": format!("call_{}", tc_idx),
                    "type": "function",
                    "function": {
                        "name": fc.get("name").cloned().unwrap_or(json!("")),
                        "arguments": fc.get("args").cloned().unwrap_or(json!({})).to_string(),
                    }
                }));
            }
        }
    }
    let mut message = json!({"role": "assistant"});
    if !text.is_empty() {
        message["content"] = Value::String(text);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish = map_gemini_finish(cand.get("finishReason").and_then(|f| f.as_str()).unwrap_or("STOP"));
    let usage = g.get("usageMetadata").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("chatcmpl-gemini-{}", g.get("responseId").and_then(|r| r.as_str()).unwrap_or("0")),
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": usage.get("promptTokenCount").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
        }
    })
}

pub fn map_gemini_finish(fr: &str) -> &'static str {
    match fr {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content_filter",
        _ => "stop",
    }
}

// ---------- streaming: gemini SSE data → openai chunk values ----------

#[derive(Default, Debug)]
pub struct GeminiStreamState {
    pub role_sent: bool,
    pub tool_count: i64,
    pub finish: Option<String>,
    pub usage: Option<Value>,
}

/// Convert one gemini streaming payload into openai chat chunk values.
pub fn gemini_stream_to_openai_chunks(
    data: &Value,
    model: &str,
    chunk_id: &str,
    created: i64,
    state: &mut GeminiStreamState,
) -> Vec<Value> {
    let mut chunks = Vec::new();
    let Some(cands) = data.get("candidates").and_then(|c| c.as_array()) else {
        return chunks;
    };
    for cand in cands {
        let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) else {
            continue;
        };
        for part in parts {
            let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if !text.is_empty() {
                let mut delta = json!({});
                if !state.role_sent {
                    delta["role"] = json!("assistant");
                    state.role_sent = true;
                }
                delta["content"] = json!(text);
                chunks.push(openai_chunk(chunk_id, created, model, delta, None));
            }
            if let Some(fc) = part.get("functionCall") {
                let mut delta = json!({});
                delta["tool_calls"] = json!([{
                    "index": state.tool_count,
                    "id": format!("call_{}", state.tool_count + 1),
                    "type": "function",
                    "function": {
                        "name": fc.get("name").cloned().unwrap_or(json!("")),
                        "arguments": fc.get("args").cloned().unwrap_or(json!({})).to_string(),
                    }
                }]);
                state.tool_count += 1;
                chunks.push(openai_chunk(chunk_id, created, model, delta, None));
            }
        }
        if let Some(fr) = cand.get("finishReason").and_then(|f| f.as_str()) {
            state.finish = Some(map_gemini_finish(fr).to_string());
        }
    }
    if let Some(u) = data.get("usageMetadata") {
        state.usage = Some(json!({
            "prompt_tokens": u.get("promptTokenCount").cloned().unwrap_or(json!(0)),
            "completion_tokens": u.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
        }));
    }
    chunks
}

pub fn openai_chunk(chunk_id: &str, created: i64, model: &str, delta: Value, finish: Option<String>) -> Value {
    json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish.map(Value::from).unwrap_or(Value::Null)}]
    })
}

#[cfg(test)]
mod tool_declarations_tests {
    use super::openai_request_to_gemini;
    use serde_json::json;

    /// D5: a request that carries tools must produce `functionDeclarations`
    /// on the Gemini wire, otherwise the model is offered no tools at all.
    #[test]
    fn tools_are_translated_into_function_declarations() {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "run a command",
                    "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
                }
            }]
        });
        let out = openai_request_to_gemini(&body);
        let decls = &out["tools"][0]["functionDeclarations"];
        assert_eq!(decls[0]["name"], "bash");
        assert_eq!(decls[0]["description"], "run a command");
        assert_eq!(decls[0]["parameters"]["type"], "object");
        assert_eq!(
            out["toolConfig"]["functionCallingConfig"]["mode"], "AUTO",
            "the model must be free to answer without a tool"
        );
    }

    /// A flat (unwrapped) tool is accepted as well.
    #[test]
    fn a_flat_tool_is_accepted() {
        let body = json!({
            "messages": [],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}]
        });
        let out = openai_request_to_gemini(&body);
        assert_eq!(out["tools"][0]["functionDeclarations"][0]["name"], "read");
        assert_eq!(out["tools"][0]["functionDeclarations"][0]["parameters"]["type"], "object");
    }

    /// R-C1: a `functionResponse` must carry the *tool name* the model
    /// declared, not the `tool_call_id` it has never seen — otherwise Gemini
    /// cannot pair the result with its `functionDeclarations` and the agent
    /// loop breaks on this lane.
    #[test]
    fn tool_responses_carry_the_tool_name_not_the_call_id() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "run a command"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "file.txt"}
            ],
            "tools": [{"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}]
        });
        let out = openai_request_to_gemini(&body);
        // contents = [user, assistant, tool] → the response is the third entry
        let resp = &out["contents"][2]["parts"][0]["functionResponse"];
        assert_eq!(
            resp["name"], "bash",
            "the response must name the tool, got {:?}",
            resp["name"]
        );
        // and the functionCall the model saw carried the same name
        assert_eq!(out["contents"][1]["parts"][0]["functionCall"]["name"], "bash");
    }

    /// A response whose call id is unknown (a replayed transcript, say) falls
    /// back to the id rather than emitting nothing.
    #[test]
    fn an_unknown_call_id_falls_back_to_the_id() {
        let body = json!({
            "messages": [
                {"role": "tool", "tool_call_id": "orphan_1", "content": "x"}
            ]
        });
        let out = openai_request_to_gemini(&body);
        assert_eq!(out["contents"][0]["parts"][0]["functionResponse"]["name"], "orphan_1");
    }

    /// A body without tools must not gain a tools key.
    #[test]
    fn no_tools_means_no_declarations() {
        let out = openai_request_to_gemini(&json!({"messages": []}));
        assert!(out.get("tools").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_to_gemini_request() {
        let oai = json!({
            "model": "gemini-2.5-flash",
            "max_tokens": 100,
            "temperature": 0.5,
            "messages": [
                {"role": "system", "content": "Sys"},
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": "Hello"},
                {"role": "user", "content": "Bye"}
            ]
        });
        let g = openai_request_to_gemini(&oai);
        assert_eq!(g["systemInstruction"]["parts"][0]["text"], "Sys");
        let cs = g["contents"].as_array().unwrap();
        assert_eq!(cs[0]["role"], "user");
        assert_eq!(cs[0]["parts"][0]["text"], "Hi");
        assert_eq!(cs[1]["role"], "model");
        assert_eq!(cs[2]["role"], "user");
        assert_eq!(g["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(g["generationConfig"]["temperature"], 0.5);
    }

    #[test]
    fn image_parts_mapped_for_gemini() {
        let oai = json!({
            "model": "gemini-2.5-flash",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,SEk="}},
                {"type": "image_url", "image_url": {"url": "https://cdn.example/i.png"}}
            ]}]
        });
        let g = openai_request_to_gemini(&oai);
        let parts = g["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(parts[1]["inlineData"]["data"], "SEk=");
        assert_eq!(parts[2]["fileData"]["fileUri"], "https://cdn.example/i.png");
    }

    #[test]
    fn pdf_file_part_maps_to_gemini_inline_data() {
        let g = openai_request_to_gemini(&json!({
            "model": "gemini-2.5-pro",
            "messages": [{"role": "user", "content": [
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,JVBERi0="}}
            ]}]
        }));
        assert_eq!(g["contents"][0]["parts"][0]["inlineData"]["mimeType"], "application/pdf");
        assert_eq!(g["contents"][0]["parts"][0]["inlineData"]["data"], "JVBERi0=");
    }

    #[test]
    fn gemini_json_to_openai() {
        let g = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hey"}, {"functionCall": {"name": "f", "args": {"a": 1}}}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 9}
        });
        let oai = gemini_response_to_openai(&g, "gemini-2.5-flash");
        assert_eq!(oai["choices"][0]["message"]["content"], "hey");
        assert_eq!(oai["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(oai["choices"][0]["finish_reason"], "stop");
        assert_eq!(oai["usage"]["prompt_tokens"], 4);
        assert_eq!(oai["usage"]["completion_tokens"], 9);
    }

    #[test]
    fn gemini_stream_to_chunks() {
        let mut st = GeminiStreamState::default();
        let d1 = json!({"candidates": [{"content": {"parts": [{"text": "he"}], "role": "model"}}]});
        let c1 = gemini_stream_to_openai_chunks(&d1, "m", "id1", 0, &mut st);
        assert_eq!(c1.len(), 1);
        assert_eq!(c1[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(c1[0]["choices"][0]["delta"]["content"], "he");

        let d2 = json!({"candidates": [{"content": {"parts": [{"text": "y"}]}, "finishReason": "MAX_TOKENS"}],
                         "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 8}});
        let c2 = gemini_stream_to_openai_chunks(&d2, "m", "id1", 0, &mut st);
        assert_eq!(c2[0]["choices"][0]["delta"]["content"], "y");
        assert_eq!(st.finish.as_deref(), Some("length"));
        assert_eq!(st.usage.as_ref().unwrap()["prompt_tokens"], 3);
    }

    #[test]
    fn finish_mapping() {
        assert_eq!(map_gemini_finish("STOP"), "stop");
        assert_eq!(map_gemini_finish("MAX_TOKENS"), "length");
        assert_eq!(map_gemini_finish("SAFETY"), "content_filter");
        assert_eq!(map_gemini_finish("WEIRD"), "stop");
    }
}
