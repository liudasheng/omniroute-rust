//! Stateful streaming translation between openai chat chunks and anthropic
//! SSE events (parity: `open-sse/translator/response/claude-to-openai.ts` and
//! the reverse direction).

use crate::sse::SseEvent;
use crate::translate::gemini::openai_chunk;
use crate::translate::openai_claude::{claude_stop_to_finish_reason, finish_reason_to_claude_stop};
use serde_json::{json, Value};

// ---------- openai chunks → claude SSE events ----------

/// Stateful converter: feed openai `chat.completion.chunk` values, receive
/// anthropic SSE `(event_name, data_value)` frames.
#[derive(Default, Debug)]
pub struct OpenaiToClaudeStream {
    started: bool,
    reasoning_started: bool,
    reasoning_closed: bool,
    reasoning_index: i64,
    has_text_block: bool,
    text_closed: bool,
    text_index: i64,
    block_count: i64,
    tool_blocks: std::collections::HashMap<i64, i64>,
    finished: bool,
    input_tokens: i64,
    output_tokens: i64,
}

impl OpenaiToClaudeStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn translate(&mut self, chunk: &Value, end_stream: bool) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = Vec::new();
        if !self.started {
            self.started = true;
            let id = chunk.get("id").and_then(|i| i.as_str()).unwrap_or("chatcmpl-omniroute");
            let msg_id = format!("msg_{}", id.trim_start_matches("chatcmpl-"));
            let model = chunk.get("model").cloned().unwrap_or(json!("unknown"));
            out.push((
                "message_start".into(),
                json!({
                    "type": "message_start",
                    "message": {
                        "id": msg_id, "type": "message", "role": "assistant",
                        "model": model, "content": [], "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }),
            ));
            out.push(("ping".into(), json!({"type": "ping"})));
        }

        if let Some(choice) = chunk.pointer("/choices/0") {
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
                let idx = if self.reasoning_started {
                    self.reasoning_index
                } else if self.has_text_block {
                    let idx = self.block_count.max(1);
                    self.block_count = idx + 1;
                    idx
                } else {
                    self.block_count = self.block_count.max(1);
                    0
                };
                if !self.reasoning_started {
                    self.reasoning_started = true;
                    self.reasoning_index = idx;
                    self.reasoning_closed = false;
                    out.push((
                        "content_block_start".into(),
                        json!({"type": "content_block_start", "index": idx,
                               "content_block": {"type": "thinking", "thinking": ""}}),
                    ));
                }
                out.push((
                    "content_block_delta".into(),
                    json!({"type": "content_block_delta", "index": idx,
                           "delta": {"type": "thinking_delta", "thinking": reasoning}}),
                ));
            }
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    let idx = if self.has_text_block { self.text_index } else { self.block_count };
                    if !self.has_text_block {
                        self.has_text_block = true;
                        self.text_index = idx;
                        self.block_count = self.block_count.max(idx + 1);
                        out.push((
                            "content_block_start".into(),
                            json!({"type": "content_block_start", "index": idx,
                                   "content_block": {"type": "text", "text": ""}}),
                        ));
                    }
                    out.push((
                        "content_block_delta".into(),
                        json!({"type": "content_block_delta", "index": idx,
                               "delta": {"type": "text_delta", "text": text}}),
                    ));
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let tidx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    let block_idx = match self.tool_blocks.get(&tidx) {
                        Some(b) => *b,
                        None => {
                            // next free block index (text block = 0 if present)
                            let idx = if self.has_text_block {
                                let free = self.block_count.max(1);
                                self.block_count = free + 1;
                                free
                            } else {
                                let free = self.block_count;
                                self.block_count += 1;
                                free
                            };
                            let name = tc.pointer("/function/name").cloned().unwrap_or(json!(""));
                            let id = tc.get("id").cloned().unwrap_or(json!("toolu_0"));
                            out.push((
                                "content_block_start".into(),
                                json!({"type": "content_block_start", "index": idx,
                                       "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                            ));
                            self.tool_blocks.insert(tidx, idx);
                            idx
                        }
                    };
                    if let Some(args) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            out.push((
                                "content_block_delta".into(),
                                json!({"type": "content_block_delta", "index": block_idx,
                                       "delta": {"type": "input_json_delta", "partial_json": args}}),
                            ));
                        }
                    }
                }
            }
            if let Some(u) = chunk.get("usage") {
                if let Some(v) = u.get("prompt_tokens").and_then(|v| v.as_i64()) {
                    self.input_tokens = v;
                }
                if let Some(v) = u.get("completion_tokens").and_then(|v| v.as_i64()) {
                    self.output_tokens = v;
                }
            }
            if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                self.close_blocks(&mut out);
                let stop = finish_reason_to_claude_stop(finish);
                out.push((
                    "message_delta".into(),
                    json!({"type": "message_delta",
                           "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
                           "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}}),
                ));
                out.push(("message_stop".into(), json!({"type": "message_stop"})));
                self.finished = true;
            }
        }

        if end_stream && !self.finished {
            self.close_blocks(&mut out);
            out.push((
                "message_delta".into(),
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                       "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}}),
            ));
            out.push(("message_stop".into(), json!({"type": "message_stop"})));
            self.finished = true;
        }
        out
    }

    fn close_blocks(&mut self, out: &mut Vec<(String, Value)>) {
        if self.reasoning_started && !self.reasoning_closed {
            out.push(("content_block_stop".into(), json!({"type": "content_block_stop", "index": self.reasoning_index})));
            self.reasoning_closed = true;
        }
        if self.has_text_block && !self.text_closed {
            out.push(("content_block_stop".into(), json!({"type": "content_block_stop", "index": self.text_index})));
            self.text_closed = true;
        }
        let mut idxs: Vec<i64> = self.tool_blocks.values().copied().collect();
        idxs.sort();
        for bidx in idxs {
            out.push(("content_block_stop".into(), json!({"type": "content_block_stop", "index": bidx})));
        }
    }
}

// ---------- claude SSE events → openai chunks ----------

#[derive(Default, Debug)]
pub struct ClaudeToOpenaiStream {
    chunk_id: String,
    created: i64,
    model: String,
    block_type: std::collections::HashMap<i64, String>,
    tool_idx_by_block: std::collections::HashMap<i64, i64>,
    next_tool_idx: i64,
    finish: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
}

impl ClaudeToOpenaiStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one anthropic SSE event → openai chunk values (empty for no-ops).
    pub fn translate(&mut self, ev: &SseEvent) -> Vec<Value> {
        let Ok(data) = serde_json::from_str::<Value>(&ev.data) else {
            return Vec::new();
        };
        let t = data.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match t {
            "message_start" => {
                self.chunk_id = data
                    .pointer("/message/id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("msg_omniroute")
                    .to_string();
                self.model = data
                    .pointer("/message/model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("claude")
                    .to_string();
                self.input_tokens = data
                    .pointer("/message/usage/input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let cid = self.cid();
                vec![openai_chunk(
                    &cid,
                    self.created,
                    &self.model,
                    json!({"role": "assistant", "content": ""}),
                    None,
                )]
            }
            "content_block_start" => {
                let idx = data.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let btype = data.pointer("/content_block/type").and_then(|x| x.as_str()).unwrap_or("text").to_string();
                if btype == "tool_use" {
                    self.tool_idx_by_block.insert(idx, self.next_tool_idx);
                    self.next_tool_idx += 1;
                }
                self.block_type.insert(idx, btype);
                Vec::new()
            }
            "content_block_delta" => {
                let idx = data.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let dtype = data.pointer("/delta/type").and_then(|x| x.as_str()).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        let text = data.pointer("/delta/text").and_then(|t| t.as_str()).unwrap_or("");
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![openai_chunk(&self.chunk_id, self.created, &self.model, json!({"content": text}), None)]
                        }
                    }
                    "input_json_delta" => {
                        let Some(&tool_idx) = self.tool_idx_by_block.get(&idx) else {
                            return Vec::new();
                        };
                        let args = data.pointer("/delta/partial_json").and_then(|t| t.as_str()).unwrap_or("");
                        if args.is_empty() {
                            return Vec::new();
                        }
                        vec![openai_chunk(
                            &self.chunk_id,
                            self.created,
                            &self.model,
                            json!({"tool_calls": [{"index": tool_idx, "function": {"arguments": args}}]}),
                            None,
                        )]
                    }
                    "thinking_delta" => {
                        let thinking = data.pointer("/delta/thinking").and_then(|t| t.as_str()).unwrap_or("");
                        if thinking.is_empty() {
                            Vec::new()
                        } else {
                            vec![openai_chunk(&self.chunk_id, self.created, &self.model, json!({"reasoning_content": thinking}), None)]
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(sr) = data.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    self.finish = Some(claude_stop_to_finish_reason(sr).to_string());
                }
                if let Some(ot) = data.pointer("/usage/output_tokens").and_then(|x| x.as_i64()) {
                    self.output_tokens = ot;
                }
                Vec::new()
            }
            "message_stop" => {
                vec![openai_chunk(
                    &self.chunk_id,
                    self.created,
                    &self.model,
                    json!({}),
                    Some(self.finish.clone().unwrap_or_else(|| "stop".to_string())),
                )]
            }
            "ping" | "content_block_stop" => Vec::new(),
            "error" => {
                let msg = data.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("upstream error");
                vec![openai_chunk(
                    &self.chunk_id,
                    self.created,
                    &self.model,
                    json!({"content": format!("[error] {msg}")}),
                    Some("stop".into()),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn cid(&self) -> String {
        format!("chatcmpl-{}", self.chunk_id.trim_start_matches("msg_"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish.map(|f| json!(f))}]
        })
    }

    #[test]
    fn openai_stream_to_claude_events_text() {
        let mut t = OpenaiToClaudeStream::new();
        let evs = t.translate(&chunk(json!({"role": "assistant"}), None), false);
        assert_eq!(evs[0].0, "message_start");
        assert_eq!(evs[0].1["message"]["role"], "assistant");

        let evs = t.translate(&chunk(json!({"content": "Hi"}), None), false);
        assert_eq!(evs[0].0, "content_block_start");
        assert_eq!(evs[0].1["index"], 0);
        assert_eq!(evs[1].0, "content_block_delta");
        assert_eq!(evs[1].1["delta"]["text"], "Hi");

        let evs = t.translate(&chunk(json!({}), Some("stop")), false);
        assert!(evs.iter().any(|(e, _)| e == "message_stop"));
        let delta_ev = evs.iter().find(|(e, _)| e == "message_delta").unwrap();
        assert_eq!(delta_ev.1["delta"]["stop_reason"], "end_turn");
        assert!(t.is_finished());
    }

    #[test]
    fn openai_stream_to_claude_tool_calls() {
        let mut t = OpenaiToClaudeStream::new();
        let _ = t.translate(&chunk(json!({"role": "assistant"}), None), false);
        let evs = t.translate(
            &chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                       "function": {"name": "f", "arguments": ""}}]}),
                None,
            ),
            false,
        );
        assert_eq!(evs[0].0, "content_block_start");
        assert_eq!(evs[0].1["content_block"]["type"], "tool_use");
        assert_eq!(evs[0].1["content_block"]["name"], "f");

        let evs = t.translate(
            &chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":"}}]}), None),
            false,
        );
        assert_eq!(evs[0].1["delta"]["type"], "input_json_delta");
        assert_eq!(evs[0].1["delta"]["partial_json"], "{\"a\":");

        let evs = t.translate(&chunk(json!({}), Some("tool_calls")), false);
        assert!(evs.iter().any(|(e, d)| e == "message_delta" && d["delta"]["stop_reason"] == "tool_use"));
        assert!(evs.iter().any(|(e, _)| e == "message_stop"));
    }

    #[test]
    fn claude_stream_to_openai_chunks() {
        let mut t = ClaudeToOpenaiStream::new();
        let ms = SseEvent {
            event: Some("message_start".into()),
            data: json!({"type":"message_start","message":{"id":"msg_9","model":"claude-3","usage":{"input_tokens":5}}}).to_string(),
        };
        let c = t.translate(&ms);
        assert_eq!(c[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(c[0]["id"], "chatcmpl-9");

        let bs = SseEvent {
            event: Some("content_block_start".into()),
            data: json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string(),
        };
        assert!(t.translate(&bs).is_empty());

        let d = SseEvent {
            event: Some("content_block_delta".into()),
            data: json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}).to_string(),
        };
        let c = t.translate(&d);
        assert_eq!(c[0]["choices"][0]["delta"]["content"], "ok");

        let md = SseEvent {
            event: Some("message_delta".into()),
            data: json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":7}}).to_string(),
        };
        assert!(t.translate(&md).is_empty());
        assert_eq!(t.finish.as_deref(), Some("length"));
        assert_eq!(t.output_tokens, 7);

        let stop = SseEvent {
            event: Some("message_stop".into()),
            data: json!({"type":"message_stop"}).to_string(),
        };
        let c = t.translate(&stop);
        assert_eq!(c[0]["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn claude_tool_stream_to_openai() {
        let mut t = ClaudeToOpenaiStream::new();
        let ms = SseEvent {
            event: Some("message_start".into()),
            data: json!({"type":"message_start","message":{"id":"msg_a","model":"claude","usage":{"input_tokens":5}}}).to_string(),
        };
        let _ = t.translate(&ms);
        let bs = SseEvent {
            event: Some("content_block_start".into()),
            data: json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"get"}}).to_string(),
        };
        let _ = t.translate(&bs);
        let dj = SseEvent {
            event: Some("content_block_delta".into()),
            data: json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}).to_string(),
        };
        let c = t.translate(&dj);
        assert_eq!(c[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(c[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"x\":1}");
        let md = SseEvent {
            event: Some("message_delta".into()),
            data: json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}).to_string(),
        };
        assert!(t.translate(&md).is_empty());
        let stop = SseEvent {
            event: Some("message_stop".into()),
            data: json!({"type":"message_stop"}).to_string(),
        };
        let c = t.translate(&stop);
        assert_eq!(c[0]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn reasoning_stream_roundtrips_as_thinking_blocks() {
        let mut to_claude = OpenaiToClaudeStream::new();
        let events = to_claude.translate(&chunk(json!({"reasoning_content": "plan"}), None), false);
        assert!(events.iter().any(|(name, value)| {
            name == "content_block_start" && value["content_block"]["type"] == "thinking"
        }));
        assert!(events.iter().any(|(name, value)| {
            name == "content_block_delta" && value["delta"]["thinking"] == "plan"
        }));

        let mut to_openai = ClaudeToOpenaiStream::new();
        let start = SseEvent {
            event: Some("content_block_start".into()),
            data: json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}).to_string(),
        };
        let delta = SseEvent {
            event: Some("content_block_delta".into()),
            data: json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}).to_string(),
        };
        let _ = to_openai.translate(&start);
        let chunks = to_openai.translate(&delta);
        assert_eq!(chunks[0]["choices"][0]["delta"]["reasoning_content"], "plan");
    }
}
