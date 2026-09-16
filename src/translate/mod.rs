//! Format translation (parity: `open-sse/translator/*`, hub-and-spoke).
//!
//! Only the `openai ⇄ claude` and `openai ⇄ gemini` spokes plus the
//! `openai-responses` request/response mapping are implemented — they cover
//! the core `/v1/chat/completions`, `/v1/messages`, `/v1/responses` surface.

pub mod gemini;
pub mod openai_claude;
pub mod responses;
pub mod stream;
