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
    /// Raw bytes, not text: a multi-byte character split across two upstream
    /// chunks must not be decoded until it is whole. Decoding each chunk with
    /// `from_utf8_lossy` wrote U+FFFD in the middle of every CJK payload that
    /// crossed a chunk boundary.
    buf: Vec<u8>,
    cur_event: Option<String>,
    cur_data: String,
    /// True when we saw a data line since the last flush.
    saw_data: bool,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        // Process one complete line at a time. Decoding is safe here because
        // `0x0A` can never appear as a UTF-8 continuation byte, so every byte
        // up to and including a newline ends on a character boundary — the
        // incomplete tail stays as bytes until its continuation arrives.
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let line: String = match std::str::from_utf8(&raw) {
                Ok(text) => text.to_string(),
                Err(_) => String::from_utf8_lossy(&raw).into_owned(),
            };
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

#[cfg(test)]
mod cross_chunk_utf8_tests {
    use super::*;

    /// A multi-byte character split across two upstream chunks must survive:
    /// decoding each chunk lossily would write U+FFFD into the middle of the
    /// payload (every CJK line that crossed a chunk boundary was affected).
    #[test]
    fn a_character_split_across_chunks_survives() {
        let payload = format!("data: {}\n\n", "汉".repeat(40));
        let bytes = payload.as_bytes();
        // "data: " is 6 bytes; the next 3 bytes are the first 汉 — split one
        // byte into it so the boundary lands inside a character.
        let (first, second) = bytes.split_at(7);
        assert!(
            std::str::from_utf8(second).is_err(),
            "the split must land inside a multi-byte sequence for this probe"
        );

        let mut parser = SseParser::default();
        let mut events = parser.feed(first);
        events.extend(parser.feed(second));
        if let Some(done) = parser.finish() {
            events.push(done);
        }

        assert_eq!(events.len(), 1, "one event expected, got {:?}", events);
        let data = &events[0].data;
        assert!(!data.contains('\u{FFFD}'), "the payload was corrupted: {data:?}");
        assert!(data.contains(&"汉".repeat(40)), "payload incomplete: {data:?}");
    }

    /// The common case still works: whole chunks decode as before.
    #[test]
    fn whole_chunks_decode_normally() {
        let mut parser = SseParser::default();
        let mut events = parser.feed("data: {\"a\":1}\n\n".as_bytes());
        if let Some(done) = parser.finish() {
            events.push(done);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }
}
