//! `openai ⇄ gemini` translation (parity: `open-sse/translator/openai-to-gemini.ts`
//! + gemini SSE stream → openai chunks).

use serde_json::{json, Value};

/// ---------- request: openai → gemini ----------

pub fn openai_request_to_gemini(body: &Value) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_text: Vec<String> = Vec::new();

    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
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
                    let name = msg.get("tool_call_id").cloned().unwrap_or(json!(""));
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
                                if t == "text" {
                                    parts.push(json!({"text": p.get("text").cloned().unwrap_or(json!(""))}));
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
    if cfg.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        out["generationConfig"] = cfg;
    }
    out
}

/// ---------- response: gemini (JSON) → openai (JSON) ----------

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

/// ---------- streaming: gemini SSE data → openai chunk values ----------

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
