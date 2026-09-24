//! HTTP layer: axum router + handlers.

pub mod admin;
pub mod auth;
pub mod body;
pub mod providers_admin;
pub mod combos_admin;
pub mod security;
pub mod chat;
pub mod dashboard;
pub mod health;
pub mod messages;
pub mod misc;
pub mod models;

use crate::state::AppState;
use axum::extract::DefaultBodyLimit;
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
    // axum's built-in default (2 MiB) is far below a long agent context; see
    // `config::DEFAULT_MAX_BODY_BYTES`.
    let max_body_bytes = state.config.max_body_bytes;

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
        .route("/v1/api-keys/{id}/rotate", post(admin::api_keys_rotate))
        .route("/v1/provider-connections", get(admin::provider_connections_list))
        .route("/v1/provider-connections", post(admin::provider_connections_create))
        .route("/v1/provider-connections/{id}", axum::routing::patch(admin::provider_connections_update))
        .route("/v1/provider-connections/{id}", delete(admin::provider_connections_delete))
        .route("/v1/provider-connections/{id}/test", post(admin::provider_connections_test))
        .route("/v1/provider-connections/{id}/sync-models", post(admin::provider_connections_sync_models))
        .route("/v1/provider-connections/{id}/models", get(admin::provider_connections_models))
        .route("/v1/provider-connections/{id}/models/{*model}", delete(admin::provider_connection_model_delete))
        .route("/v1/provider-catalog", get(dashboard::provider_catalog))
        .route("/v1/providers/test-batch", post(dashboard::providers_test_batch))
        .route("/v1/free-tiers", get(dashboard::free_tiers))
        .route("/v1/provider-connections/test-all", post(dashboard::provider_connections_test_all))
        .route("/v1/provider-connections/import", post(dashboard::provider_connections_import))
        .route("/v1/compression", get(models::compression_config))
        .route("/v1/compression", post(models::compression_config_update))
        .route("/v1/stats", get(dashboard::stats))
        .route("/v1/logs", get(dashboard::logs))
        .route("/v1/settings", get(models::settings))
        .route("/v1/stats/providers", get(dashboard::stats_providers))
        .route("/v1/usage/analytics", get(dashboard::usage_analytics))
        .route("/v1/combo-health", get(dashboard::combo_health))
        .route("/v1/combos/managed", get(dashboard::combos_managed))
        .route("/v1/combos/managed", post(dashboard::combos_upsert))
        .route("/v1/combos/managed/{id}", axum::routing::patch(dashboard::combos_patch))
        .route("/v1/combos/managed/{id}", delete(dashboard::combos_delete))
        .route("/v1/combo-presets", get(dashboard::combo_presets))
        .route("/v1/combo-studio", get(dashboard::combo_studio))
        .route("/v1/routing/trace", get(dashboard::routing_trace))
        .route("/v1/embedded-services", get(dashboard::embedded_services))
        .route("/v1/quota-share", get(dashboard::quota_share))
        .route("/v1/cache/health", get(dashboard::cache_health))
        .route("/v1/endpoints", get(dashboard::endpoints_overview))
        .route("/v1/settings/custom-system-prompt", post(dashboard::custom_system_prompt_set))
        .route("/v1/provider-quotas", get(dashboard::provider_quotas))
        .route("/v1/provider-quotas/{provider}", post(dashboard::provider_quotas_upsert))
        .route("/v1/audit", get(dashboard::audit))
        .route("/v1/logs/export", get(dashboard::logs_export))
        .route("/v1/admin/service/restart", post(admin::service_restart))
        .route("/v1/admin/service/stop", post(admin::service_stop))
        // embedded web dashboard + PWA shell
        .route("/dashboard", get(dashboard::index))
        .route("/dashboard/{*path}", get(dashboard::asset))
        .route("/", get(dashboard::root_redirect))
        .route("/v1/combos/test", post(misc::combos_test))
        .fallback(misc::not_found)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(axum::Extension(body::MaxBodyBytes(max_body_bytes)))
        .layer(cors)
        .with_state(state)
}
