//! OmniRoute Rust — AI gateway core library.
//!
//! A full Rust rewrite of the OmniRoute gateway core (originally TypeScript,
//! see https://github.com/diegosouzapw/OmniRoute). This crate implements:
//! - OpenAI-compatible HTTP API (`/v1/chat/completions`, `/v1/messages`,
//!   `/v1/responses`, `/v1/completions`, `/v1/models`, ...)
//! - Multi-provider registry with dynamic `openai-compatible-*` /
//!   `anthropic-compatible-*` provider families
//! - Combo routing strategies with quota-aware automatic fallback
//! - Per-connection cooldowns / circuit breakers / exponential backoff
//! - SSE streaming with format translation (openai <-> claude <-> gemini)
//! - CLI (`omniroute serve|status|stop|models|providers|combos|doctor`)

pub mod cli;
pub mod compression;
pub mod config;
pub mod core;
pub mod errors;
pub mod format;
pub mod model;
pub mod registry;
pub mod router;
pub mod server;
pub mod sse;
pub mod state;
pub mod translate;
pub mod upstream;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static JSON_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_json_output(v: bool) {
    JSON_OUTPUT.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_json_output() -> bool {
    JSON_OUTPUT.load(std::sync::atomic::Ordering::Relaxed)
}
