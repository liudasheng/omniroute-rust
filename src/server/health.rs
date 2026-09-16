//! Health endpoints (parity: `/healthz`, `/readyz`, `/livez`,
//! `/api/health(/ping)` in `src/app/healthz/route.ts`).

use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::Arc;

/// `GET /healthz` and `GET /readyz` → `ok\n` (200).
pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// `HEAD /healthz` → 200 empty.
pub async fn healthz_head() -> impl IntoResponse {
    StatusCode::OK
}

/// `GET /livez` → process liveness, always `ok\n` (200).
pub async fn livez() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// `GET /api/health` → JSON status.
pub async fn api_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let _ = state;
    (
        StatusCode::OK,
        axum::Json(json!({
            "ok": true,
            "status": "healthy",
            "version": crate::VERSION,
        })),
    )
}

/// `GET /api/health/ping` → DB-readiness probe; gateway has no DB, so 200.
pub async fn api_health_ping() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(json!({"ok": true, "pong": true})))
}
