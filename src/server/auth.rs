//! Authentication policies (parity with the original authz pipeline's route
//! classes: PUBLIC / CLIENT_API / MANAGEMENT).
//!
//! - PUBLIC (health etc.) — never here, routes are unprotected upstream.
//! - INFERENCE (`/v1/chat|completions|messages|responses|...`): a Bearer key
//!   is required when any key is configured (static `OMNIROUTE_API_KEY` or
//!   dashboard-issued API keys); open access when none configured (dev).
//! - MANAGEMENT (provider admin, logs, stats, settings, key CRUD):
//!   accepted credentials = valid dashboard session token (`sess_*`) or an
//!   `admin`-role API key or the static `OMNIROUTE_API_KEY`.

use crate::state::AppState;
use axum::http::HeaderMap;
use std::sync::Arc;

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Inference authorization: admitted keys = managed enabled key, or the
/// static master key; open access when none is configured (dev default).
pub fn inference_allowed(state: &Arc<AppState>, headers: &HeaderMap) -> bool {
    if let Some(b) = bearer(headers) {
        if state.api_keys.matches_enabled(&b) {
            state.api_keys.touch(&b);
            return true;
        }
        if let Some(expected) = state.config.api_key.as_deref() {
            if crate::constant_time_eq(&b, expected) {
                return true;
            }
        }
        return false;
    }
    state.config.api_key.is_none() && state.api_keys.list().iter().all(|k| !k.enabled)
}

/// Session token: `Authorization: Bearer sess_...` or cookie
/// `omniroute_session=...`.
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(b) = bearer(headers) {
        if b.starts_with("sess_") {
            return Some(b);
        }
        return None;
    }
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';').find_map(|part| {
                let p = part.trim();
                p.strip_prefix("omniroute_session=").map(str::to_string)
            })
        })
}

/// Management authorization: session token, admin API key, or master key.
pub fn management_allowed(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<(), crate::errors::ApiError> {
    if let Some(t) = session_token(headers) {
        if state.auth.session_valid(&t) {
            return Ok(());
        }
    }
    if let Some(b) = bearer(headers) {
        if let Some(expected) = state.config.api_key.as_deref() {
            if crate::constant_time_eq(&b, expected) {
                return Ok(());
            }
        }
        if let Some(e) = state.api_keys.by_key(&b) {
            if e.enabled && e.role == "admin" {
                state.api_keys.touch(&b);
                return Ok(());
            }
        }
    }
    let keys_empty = state.api_keys.list().iter().all(|k| !k.enabled);
    eprintln!("MGMT-GUARD: api_key={:?} keys_empty={} login_enabled={}", state.config.api_key.is_some(), keys_empty, state.auth.login_enabled);
    if state.config.api_key.is_none()
        && keys_empty
        && !state.auth.login_enabled
    {
        return Ok(()); // nothing configured → local dev default
    }
    Err(crate::errors::ApiError::new(
        401,
        "management access denied: log in via /v1/auth/login or use an admin API key",
    ))
}

/// Inference check.
pub fn require(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), crate::errors::ApiError> {
    if inference_allowed(state, headers) {
        Ok(())
    } else {
        Err(crate::errors::ApiError::new(401, "invalid or missing API key"))
    }
}

/// Management check (Err shape identical to `require`).
pub fn require_management(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<(), crate::errors::ApiError> {
    management_allowed(state, headers)
}
