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
pub mod model_catalog;
pub mod opencode_free;
pub mod opencode_free_budget;
pub mod opencode_free_egress;
pub mod opencode_quota;
pub mod provider_sync;
pub mod registry;
pub mod router;
pub mod server;
pub mod sse;
pub mod state;
pub mod translate;
pub mod upstream;
pub mod usage_probe;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static JSON_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_json_output(v: bool) {
    JSON_OUTPUT.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// chmod 600 (unix) — used for the dashboard password file.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    constant_time_eq_bytes(a.as_bytes(), b.as_bytes())
}

fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn set_file_mode_600(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Write a file that holds a secret (an API key, a password hash).
///
/// A plain `fs::write` creates the file with the process umask — 0644 on a
/// typical host — and `chmod` afterwards leaves a window in which the secret
/// is world-readable. This helper creates the file with the restrictive mode
/// already in place and renames it over the target, so the secret is never
/// exposed and the replace stays atomic.
pub fn write_secret_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;

    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    file.write_all(contents.as_bytes())?;
    drop(file);
    // The mode above only applies on creation; make it explicit so a
    // pre-existing `tmp` from an earlier run is tightened as well.
    let _ = set_file_mode_600(&tmp);
    std::fs::rename(&tmp, path)
}

pub fn is_json_output() -> bool {
    JSON_OUTPUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is stdin attached to a terminal? (used by the CLI to pick interactive vs piped paths)
pub fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc_isatty(0) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}
#[cfg(unix)]
extern "C" {
    #[link_name = "isatty"]
    fn libc_isatty(fd: i32) -> i32;
}
