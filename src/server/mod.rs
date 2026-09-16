//! HTTP layer: axum router + handlers.

pub mod auth;
pub mod chat;
pub mod health;
pub mod messages;
pub mod misc;
pub mod models;

use crate::state::AppState;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

/// Assemble the full gateway router.
pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    Router::new()
        // health
        .route("/healthz", get(health::healthz).head(health::healthz_head))
        .route("/readyz", get(health::healthz))
        .route("/livez", get(health::livez))
        .route("/api/health", get(health::api_health))
        .route("/api/health/ping", get(health::api_health_ping))
        // openai-compatible API
        .route("/v1", get(models::list))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/completions", post(chat::completions))
        .route("/v1/messages", post(messages::messages))
        .route("/v1/messages/count_tokens", post(messages::count_tokens))
        .route("/v1/responses", post(chat::responses))
        .route("/v1/models", get(models::list))
        .route("/v1/embeddings", post(chat::passthrough_embeddings))
        .route("/v1/rerank", post(chat::passthrough_rerank))
        .route("/v1/moderations", post(chat::passthrough_moderations))
        // management-ish read endpoints
        .route("/v1/providers", get(models::providers))
        .route("/v1/quotas", get(models::quotas))
        .route("/v1/combos", get(models::combos))
        .route("/v1/combos/test", post(misc::combos_test))
        .fallback(misc::not_found)
        .layer(cors)
        .with_state(state)
}
