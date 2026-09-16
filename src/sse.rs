//! SSE parsing/serialization primitives (parity: `open-sse/utils/stream.ts`,
//! `open-sse/utils/earlyStreamKeepalive.ts`).
//!
//! Frames are `<event: name>\ndata: payload\n\n`; keepalive uses an SSE
//! comment `: keepalive` which every compliant client ignores.

use bytes::Bytes;
use serde_json::Value;

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn parse_data_json(&self) -> Option<Value> {
        serde_json::from_str(&self.data).ok()
    }
}

/// Incremental SSE parser over raw upstream bytes.
#[derive(Default, Debug)]
pub struct SseParser {
    buf: String,
    cur_event: Option<String>,
    cur_data: String,
    /// True when we saw a data line since the last flush.
    saw_data: bool,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut events = Vec::new();
        // process line by line; keep incomplete trailing line in buf
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if self.saw_data {
                    events.push(SseEvent {
                        event: self.cur_event.take(),
                        data: std::mem::take(&mut self.cur_data),
                    });
                    self.saw_data = false;
                } else {
                    self.cur_event = None;
                }
            } else if let Some(v) = line.strip_prefix("data:") {
                let v = v.strip_prefix(' ').unwrap_or(v);
                if !self.cur_data.is_empty() {
                    self.cur_data.push('\n');
                }
                self.cur_data.push_str(v);
                self.saw_data = true;
            } else if let Some(v) = line.strip_prefix("event:") {
                let v = v.strip_prefix(' ').unwrap_or(v);
                self.cur_event = Some(v.to_string());
            }
            // `id:` and comment lines (`: ...`) are ignored
        }
        events
    }

    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.saw_data && !self.cur_data.is_empty() {
            self.saw_data = false;
            Some(SseEvent {
                event: self.cur_event.take(),
                data: std::mem::take(&mut self.cur_data),
            })
        } else {
            None
        }
    }
}

/// Serialize one `data:` frame.
pub fn frame_data(payload: &str) -> Bytes {
    Bytes::from(format!("data: {payload}\n\n"))
}

/// Serialize an event with an explicit `event:` name (anthropic wire style).
pub fn frame_event(event: &str, payload: &str) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}

/// Keepalive comment frame (ignored by SSE clients, resets proxy idle timers).
pub fn frame_keepalive() -> Bytes {
    Bytes::from(": keepalive\n\n")
}

pub const DONE_MARKER: &str = "[DONE]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_stream() {
        let mut p = SseParser::default();
        let evs = p.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "{\"a\":1}");
        assert_eq!(evs[1].data, "{\"b\":2}");
    }

    #[test]
    fn parses_named_events_and_multiline_data() {
        let mut p = SseParser::default();
        let evs = p.feed(b"event: message_start\ndata: {\"x\":\ndata:1}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"x\":\n1}");
    }

    #[test]
    fn handles_chunk_boundaries() {
        let mut p = SseParser::default();
        let e1 = p.feed(b"data: {\"a\":");
        assert!(e1.is_empty());
        let e2 = p.feed(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(e2.len(), 2);
        assert_eq!(e2[0].data, "{\"a\":1}");
        assert_eq!(e2[1].data, "[DONE]");
    }

    #[test]
    fn comments_and_ids_ignored() {
        let mut p = SseParser::default();
        let evs = p.feed(b": keepalive\nid: 1\ndata: hi\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "hi");
        assert_eq!(evs[0].event, None);
    }

    #[test]
    fn crlf_line_endings() {
        let mut p = SseParser::default();
        let evs = p.feed(b"data: ok\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "ok");
    }

    #[test]
    fn keepalive_frame_shape() {
        assert_eq!(frame_keepalive(), Bytes::from(": keepalive\n\n"));
        assert_eq!(frame_data("[DONE]"), Bytes::from("data: [DONE]\n\n"));
        assert_eq!(
            frame_event("ping", "{}"),
            Bytes::from("event: ping\ndata: {}\n\n")
        );
    }
}
