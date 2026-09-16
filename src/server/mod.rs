//! HTTP layer: axum router + handlers.

pub mod admin;
pub mod auth;
pub mod providers_admin;
pub mod security;
pub mod chat;
pub mod dashboard;
pub mod health;
pub mod messages;
pub mod misc;
pub mod models;

use crate::state::AppState;
use axum::routing::{delete, get, post};
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
        // multimodal generation surfaces (single-provider passthrough)
        .route("/v1/images/generations", post(chat::passthrough_images_generations))
        .route("/v1/images/edits", post(chat::passthrough_images_edits))
        .route("/v1/images/upscale", post(chat::passthrough_images_upscale))
        .route("/v1/videos", post(chat::passthrough_videos))
        .route("/v1/audio/transcriptions", post(chat::passthrough_audio_transcriptions))
        .route("/v1/audio/translations", post(chat::passthrough_audio_translations))
        .route("/v1/audio/speech", post(chat::passthrough_audio_speech))
        .route("/v1/speech-to-text", post(chat::passthrough_speech_to_text))
        .route("/v1/text-to-speech", post(chat::passthrough_text_to_speech))
        .route("/v1/ocr", post(chat::passthrough_ocr))
        .route("/v1/files", post(chat::passthrough_files))
        .route("/v1/batches", post(chat::passthrough_batches))
        .route("/v1/batches", get(chat::passthrough_batches_list))
        .route("/v1/batches/{id}", get(chat::passthrough_batches_get))
        // management-ish read endpoints
        .route("/v1/providers", get(models::providers))
        .route("/v1/quotas", get(models::quotas))
        .route("/v1/combos", get(models::combos))
        .route("/v1/auth/login", post(admin::login))
        .route("/v1/auth/logout", post(admin::logout))
        .route("/v1/auth/change-password", post(admin::change_password))
        .route("/v1/auth/me", get(admin::me))
        .route("/v1/api-keys", get(admin::api_keys_list))
        .route("/v1/api-keys", post(admin::api_keys_create))
        .route("/v1/api-keys/{id}", axum::routing::patch(admin::api_keys_update))
        .route("/v1/api-keys/{id}", delete(admin::api_keys_revoke))
        .route("/v1/provider-connections", get(admin::provider_connections_list))
        .route("/v1/provider-connections", post(admin::provider_connections_create))
        .route("/v1/provider-connections/{id}", axum::routing::patch(admin::provider_connections_update))
        .route("/v1/provider-connections/{id}", delete(admin::provider_connections_delete))
        .route("/v1/provider-connections/{id}/test", post(admin::provider_connections_test))
        .route("/v1/compression", get(models::compression_config))
        .route("/v1/compression", post(models::compression_config_update))
        .route("/v1/stats", get(dashboard::stats))
        .route("/v1/logs", get(dashboard::logs))
        .route("/v1/settings", get(models::settings))
        .route("/v1/admin/service/restart", post(admin::service_restart))
        .route("/v1/admin/service/stop", post(admin::service_stop))
        // embedded web dashboard + PWA shell
        .route("/dashboard", get(dashboard::index))
        .route("/dashboard/{*path}", get(dashboard::asset))
        .route("/", get(dashboard::root_redirect))
        .route("/v1/combos/test", post(misc::combos_test))
        .fallback(misc::not_found)
        .layer(cors)
        .with_state(state)
}
