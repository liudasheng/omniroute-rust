//! API-key authentication (parity: CLIENT_API strategy in
//! `src/server/authz/pipeline.ts` — Bearer token; also accepts `x-api-key`).

use crate::errors::ApiError;
use crate::state::AppState;
use axum::http::HeaderMap;
use std::sync::Arc;

/// Validate the request when an API key is configured. Local health endpoints
/// never reach this (no auth, like PUBLIC route class).
pub fn require(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.config.api_key.as_deref() else {
        return Ok(()); // no key configured → open access (dev default)
    };
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
        });
    match got {
        Some(k) if constant_time_eq(k, expected) => Ok(()),
        _ => Err(ApiError::new(401, "invalid or missing API key")),
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }
}
