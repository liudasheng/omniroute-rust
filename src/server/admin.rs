//! Dashboard/management endpoints: login/logout, API-key CRUD,
//! provider-connection CRUD + connection test (parity: original dashboard
//! auth + `/api/keys` + `/api/providers` management).

use crate::server::providers_admin::{mask_key, ProviderConnection};
use crate::state::AppState;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;

// ── auth ────────────────────────────────────────────────────────────────

/// `POST /v1/auth/login` {password} → session token (+ cookie).
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<std::net::SocketAddr>,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if !state.auth.login_allowed(remote.ip(), now) {
        return crate::errors::ApiError::new(429, "too many failed login attempts; try again later").into();
    }
    let password = body.get("password").and_then(|p| p.as_str()).unwrap_or("");
    match state.auth.login(password) {
        Some(token) => {
            // Path=/ so the browser sends the session to /v1/* as well as
            // /dashboard (parity: the original's persistent dashboard
            // session — a refresh must not drop the login).
            let cookie = format!(
                "omniroute_session={token}; HttpOnly; Path=/; Max-Age=604800; SameSite=Lax"
            );
            state.audit("auth.login", "password accepted", true);
            (
                axum::http::StatusCode::OK,
                [
                    (axum::http::header::SET_COOKIE, cookie),
                    (axum::http::header::CONTENT_TYPE, "application/json".to_string()),
                ],
                json!({"ok": true, "token": token, "expires_in_days": 7}).to_string(),
            )
                .into_response()
        }
        None => {
            state.audit("auth.login", "invalid password", false);
            crate::errors::ApiError::new(401, "invalid password").into()
        }
    }
}

/// `POST /v1/auth/logout` — revoke the presented session (and expire the
/// session cookie so a stale cookie cannot resurrect the login).
pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(t) = crate::server::auth::session_token(&headers) {
        state.auth.logout(&t);
    }
    (
        axum::http::StatusCode::OK,
        [
            (
                axum::http::header::SET_COOKIE,
                "omniroute_session=; HttpOnly; Path=/; Max-Age=0; SameSite=Lax".to_string(),
            ),
            (
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            ),
        ],
        json!({"ok": true}).to_string(),
    )
        .into_response()
}

/// `POST /v1/auth/change-password` {current_password, new_password}
/// Requires an authenticated session or admin key; rejects the default
/// password when unchanged.
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return crate::errors::ApiError::new(400, "invalid JSON body").into();
    };
    let new_password = body.get("new_password").and_then(|p| p.as_str()).unwrap_or("");
    if new_password.chars().count() < 8 {
        return crate::errors::ApiError::new(400, "new password too short (min 8 chars)").into();
    }
    let current = body.get("current_password").and_then(|p| p.as_str()).unwrap_or("");
    // An untouched default password may be replaced without re-verification (the
    // forced-change flow). Any real password must be proved first — otherwise a
    // stale literal "CHANGEME" would let a hijacked session rotate the password.
    if !state.auth.is_default_password() && !state.auth.verify(current) {
        return crate::errors::ApiError::new(401, "current_password does not match").into();
    }
    state.audit("auth.change_password", "admin password rotated", true);
    state.auth.change_password(new_password);
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "ok": true,
            "using_default_password": false,
            "message": "password updated",
        })),
    )
        .into_response()
}

/// `GET /v1/auth/me` — session state for dashboard boot.
pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let authed = crate::server::auth::management_allowed(&state, &headers).is_ok();
    let method = if crate::server::auth::session_token(&headers)
        .map(|t| state.auth.session_valid(&t))
        .unwrap_or(false)
    {
        "session"
    } else if state.config.api_key.is_some() {
        "api-key"
    } else {
        "open"
    };
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "authenticated": authed,
            "method": method,
            "login_required": !authed,
            "using_default_password": state.auth.is_default_password(),
        })),
    )
        .into_response()
}

// ── API keys ────────────────────────────────────────────────────────────

fn key_json(k: &crate::server::security::ApiKeyEntry, key_display: &str) -> Value {
    json!({
        "id": k.id, "name": k.name, "key": key_display, "role": k.role,
        "modelAccessMode": k.model_access_mode, "allowedModels": k.allowed_models,
        "allowedCombos": k.allowed_combos, "noLog": k.no_log,
        "allowUsageCommand": k.allow_usage_command,
        "usageLimitEnabled": k.usage_limit_enabled,
        "dailyUsageLimitUsd": k.daily_usage_limit_usd,
        "weeklyUsageLimitUsd": k.weekly_usage_limit_usd,
        "chaosModeEnabled": k.chaos_mode_enabled,
        "enabled": k.enabled, "created_at_ms": k.created_at_ms,
        "last_used_at_ms": k.last_used_at_ms, "total_requests": k.total_requests,
        "type": k.key_type, "expiresAtMs": k.expires_at_ms, "cost_usd": k.cost_usd,
        "status": crate::server::security::ApiKeyStore::status_of(k, crate::server::security::now_ms()),
    })
}

/// `GET /v1/api-keys` — list (keys masked).
pub async fn api_keys_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let keys: Vec<Value> = state
        .api_keys
        .list()
        .iter()
        .map(|k| key_json(k, &mask_key(&k.key)))
        .collect();
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "api_keys": keys })),
    )
        .into_response()
}

/// `POST /v1/api-keys` {name?, role?} — full secret shown once.
pub async fn api_keys_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    let name = body.get("name").and_then(|n| n.as_str()).unwrap_or("default");
    let role = body.get("role").and_then(|r| r.as_str()).unwrap_or("default");
    // validate name (parity: MAX 200 chars)
    if name.chars().count() > 200 {
        return crate::errors::ApiError::new(400, "name too long (max 200)").into();
    }
    state.audit("api_key.create", format!("name={name} role={role}"), true);
    let entry = state.api_keys.create(
        name,
        role,
        body.get("modelAccessMode").and_then(|x| x.as_str()),
        body.get("allowedModels").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        body.get("allowedCombos").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        body.get("noLog").and_then(|x| x.as_bool()).unwrap_or(false),
        body.get("allowUsageCommand").and_then(|x| x.as_bool()).unwrap_or(false),
        body.get("usageLimitEnabled").and_then(|x| x.as_bool()).unwrap_or(false),
        body.get("dailyUsageLimitUsd").and_then(|x| x.as_f64()),
        body.get("weeklyUsageLimitUsd").and_then(|x| x.as_f64()),
        body.get("chaosModeEnabled").and_then(|x| x.as_bool()).unwrap_or(false),
    );
    (
        axum::http::StatusCode::CREATED,
        axum::Json(json!({ "api_key": key_json(&entry, &entry.key) })),
    )
        .into_response()
}

/// `PATCH /v1/api-keys/{id}` {enabled}.
pub async fn api_keys_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    let Some(enabled) = body.get("enabled").and_then(|x| x.as_bool()) else {
        return crate::errors::ApiError::new(400, "body must include {\"enabled\": bool}").into();
    };
    state.audit("api_key.toggle", format!("id={id} enabled={enabled}"), true);
    if state.api_keys.set_enabled(&id, enabled) {
        (
            axum::http::StatusCode::OK,
            axum::Json(json!({"ok": true, "id": id, "enabled": enabled})),
        )
            .into_response()
    } else {
        crate::errors::ApiError::new(404, format!("api key '{id}' not found")).into()
    }
}

/// `DELETE /v1/api-keys/{id}` — revoke.
pub async fn api_keys_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    state.audit("api_key.revoke", format!("id={id}"), true);
    if state.api_keys.revoke(&id) {
        (
            axum::http::StatusCode::OK,
            axum::Json(json!({"ok": true, "id": id})),
        )
            .into_response()
    } else {
        crate::errors::ApiError::new(404, format!("api key '{id}' not found")).into()
    }
}

// ── provider connections ────────────────────────────────────────────────

fn conn_json(c: &ProviderConnection) -> Value {
    json!({
        "id": c.id, "provider": c.provider, "name": c.name,
        "api_key": match c.api_key.as_ref() {
            Some(k) => Value::String(mask_key(k)),
            None => Value::Null,
        },
        "baseUrl": c.base_url, "apiType": c.api_type,
        "models": c.model_list, "enabled": c.enabled,
        "syncedModels": c.synced_models, "syncedAtMs": c.synced_at_ms,
        "modelMetadata": c.model_metadata,
        "hiddenModels": c.hidden_models,
        "created_at_ms": c.created_at_ms,
    })
}

/// `GET /v1/provider-connections` — list (api keys masked).
pub async fn provider_connections_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let conns: Vec<Value> = state
        .provider_connections
        .list(true)
        .iter()
        .map(conn_json)
        .collect();
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "connections": conns })),
    )
        .into_response()
}

/// `POST /v1/provider-connections` — add/upsert
/// {provider, name?, apiKey?, baseUrl?, apiType?, models?, enabled?}.
pub async fn provider_connections_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let raw: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid body: {e}")).into(),
    };
    let mut conn = match serde_json::from_value::<ProviderConnection>(raw.clone()) {
        Ok(c) => c,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid body: {e}")).into(),
    };
    if conn.provider.is_empty() {
        return crate::errors::ApiError::new(400, "missing required field: provider").into();
    }
    if conn.name.is_empty() {
        conn.name = conn.provider.clone();
    }
    // Distinguish "field absent" (a new account, inherit) from "explicit []"
    // (the operator cleared the mask) — serde alone cannot tell them apart.
    let requested_hidden = raw
        .get("hidden_models")
        .or_else(|| raw.get("hiddenModels"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect());
    if conn.id.is_empty() {
        conn.id = format!("conn-{}", crate::server::security::random_id(8));
    }
    let is_new = state.provider_connections.get(&conn.id).is_none();
    if is_new {
        inherit_hidden_models(&state, &mut conn, requested_hidden);
    } else if let Some(explicit) = requested_hidden {
        conn.hidden_models = explicit;
    }
    apply_connection(&state, &mut conn);
    state.audit(
        "provider_connection.upsert",
        format!("provider={} id={}", conn.provider, conn.id),
        true,
    );
    state.provider_connections.upsert(conn.clone());
    (
        axum::http::StatusCode::CREATED,
        axum::Json(json!({ "connection": conn_json(&conn) })),
    )
        .into_response()
}

/// `PATCH /v1/provider-connections/{id}`.
pub async fn provider_connections_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(mut conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    if let Some(k) = body.get("api_key").and_then(|x| x.as_str()) {
        conn.api_key = Some(k.to_string());
    }
    if let Some(b) = body.get("baseUrl").and_then(|x| x.as_str()) {
        conn.base_url = Some(b.to_string());
    }
    if let Some(n) = body.get("name").and_then(|x| x.as_str()) {
        conn.name = n.to_string();
    }
    if let Some(e) = body.get("enabled").and_then(|x| x.as_bool()) {
        conn.enabled = e;
    }
    if let Some(a) = body.get("api_type").and_then(|x| x.as_str()) {
        conn.api_type = Some(a.to_string());
    }
    if let Some(models) = body.get("models").and_then(|x| x.as_array()) {
        conn.model_list = models
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
    }
    if let Some(hidden) = body
        .get("hidden_models")
        .or_else(|| body.get("hiddenModels"))
        .and_then(|x| x.as_array())
    {
        conn.hidden_models = hidden
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
    }
    apply_connection(&state, &mut conn);
    state.provider_connections.upsert(conn.clone());
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "connection": conn_json(&conn) })),
    )
        .into_response()
}

/// A brand-new account of a provider starts from the visibility the operator
/// already chose for that provider's existing accounts.
///
/// Hiding is a decision about *which models this provider is worth routing to*,
/// not about one API key: a second key for the same upstream serves the same
/// catalogue. Without this a freshly added account re-exposes every model the
/// operator had already hidden, and the routable set silently widens for a
/// reason no one chose.
///
/// An explicit list in the request always wins (including an explicit empty one,
/// which is how the dashboard's "show all" clears the mask). `requested` is
/// `None` when the field was absent, which is exactly the "new account, no
/// opinion yet" case that should inherit.
fn inherit_hidden_models(
    state: &Arc<AppState>,
    conn: &mut ProviderConnection,
    requested: Option<Vec<String>>,
) {
    if let Some(explicit) = requested {
        conn.hidden_models = explicit;
        return;
    }
    if !conn.hidden_models.is_empty() {
        return;
    }
    let inherited: Vec<String> = state
        .provider_connections
        .all_unmasked()
        .into_iter()
        .filter(|other| other.provider == conn.provider && other.id != conn.id)
        .flat_map(|other| other.hidden_models)
        .collect();
    if inherited.is_empty() {
        return;
    }
    let mut unique = inherited;
    unique.sort();
    unique.dedup();
    tracing::info!(
        provider = %conn.provider, id = %conn.id, models = unique.len(),
        "new account inherits the provider's hidden models"
    );
    conn.hidden_models = unique;
}

/// Register/refresh a connection in the live registry + credential overlay.
/// Blank key/base URLs are normalized to `None` so an empty form field can
/// never shadow the registry defaults (e.g. openrouter without a base URL).
pub fn apply_connection(state: &Arc<AppState>, conn: &mut ProviderConnection) {
    for field in [&mut conn.api_key, &mut conn.base_url] {
        if field.as_deref().map(str::trim).unwrap_or_default().is_empty() {
            *field = None;
        }
    }
    if conn.provider.starts_with("openai-compatible")
        || conn.provider.starts_with("anthropic-compatible")
    {
        let mut models = conn.model_list.clone();
        models.extend(conn.synced_models.clone());
        state.registry.register_dynamic(
            &conn.provider,
            conn.base_url.clone(),
            conn.api_type.clone(),
            models,
        );
    }
    if conn.api_key.is_some() || conn.base_url.is_some() {
        let cred = crate::config::ProviderCredentials {
            api_key: conn.api_key.clone(),
            base_url: conn.base_url.clone(),
            api_type: conn.api_type.clone(),
            model_list: [conn.model_list.clone(), conn.synced_models.clone()].concat(),
            enabled: Some(conn.enabled),
        };
        state
            .credentials_overlay
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conn.provider.clone(), cred);
    }
}

/// `DELETE /v1/provider-connections/{id}` — remove; dynamic families are
/// unregistered from the live registry.
pub async fn provider_connections_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    state.audit("provider_connection.remove", format!("id={id}"), true);
    state.provider_connections.remove(&id);
    if !crate::registry::static_registry().iter().any(|e| e.id == conn.provider) {
        state.registry.unregister(&conn.provider);
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"ok": true, "id": id})),
    )
        .into_response()
}

/// Model id a probe should use: the connection's own list first, then the
/// registry defaults (e.g. openrouter's `auto` — strict upstreams reject the
/// generic bare fallback), then a model the upstream actually published, then a
/// last-resort bare id.
fn probe_model(
    conn: &crate::server::providers_admin::ProviderConnection,
    entry: &crate::registry::RegistryEntry,
) -> String {
    // An explicit per-connection model always wins, then the registry default.
    // The public Go directory is *not* an entitlement list (a subscription key
    // can use ids it does not publish), so it must not narrow this choice.
    if let Some(saved) = conn.model_list.first().cloned() {
        return saved;
    }
    if let Some(default) = entry.default_models.first().cloned() {
        return default;
    }
    // A custom `openai-compatible-*` endpoint has no registry defaults, yet the
    // connection itself knows which ids the upstream serves (from `/models`).
    // Probing with a hardcoded id the upstream never heard of reports a failure
    // that says nothing about the credential — e.g. a chained gateway rejects
    // `gpt-4o-mini` with 403/404 while every synced model works.
    if let Some(synced) = conn
        .synced_models
        .iter()
        .find(|m| !conn.hidden_models.contains(m))
        .cloned()
    {
        return synced;
    }
    entry
        .default_models
        .first()
        .cloned()
        .unwrap_or_else(|| "gpt-4o-mini".to_string())
}

/// Credentials a probe must use: the tested connection's own key/base first
/// (blanks ignored), then the live overlay, then static config. Without this
/// the probe would test the registry default instead of what the user saved
/// (e.g. an openrouter key with no base URL override).
pub(crate) fn connection_probe_creds(
    state: &Arc<AppState>,
    conn: &crate::server::providers_admin::ProviderConnection,
) -> (Option<String>, Option<String>) {
    let key = conn
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| state.api_key_for(&conn.provider));
    let base = conn
        .base_url
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or_else(|| state.base_url_for(&state.registry, &conn.provider));
    (key, base)
}

/// Models-listing URL for a connection's upstream (parity: the original's
/// per-provider discovery in `[id]/models/route.ts`, reduced to the three
/// listable wire formats). Returns `None` when the base cannot yield one.
pub(crate) fn models_url_for(base: &str) -> String {
    let b = base.trim_end_matches('/');
    // full chat paths collapse back to their API root (idempotent when the
    // base already points at the listing)
    let root = if let Some(h) = b.strip_suffix("/models") {
        h.to_string()
    } else if let Some(i) = b.find("v1beta") {
        b[..i + 6].to_string()
    } else if let Some(i) = b.find("/v1/") {
        b[..i + 3].to_string()
    } else if b.ends_with("/v1") {
        b.to_string()
    } else if b.ends_with("/chat/completions") || b.ends_with("/responses") || b.ends_with("/chat") {
        b.rsplit_once('/')
            .and_then(|(h, _)| h.rsplit_once('/'))
            .map(|(hh, _)| hh.to_string())
            .unwrap_or_else(|| format!("{b}/v1"))
    } else if b.ends_with("/messages") {
        b.rsplit_once('/').map(|(h, _)| h.to_string()).unwrap_or_else(|| format!("{b}/v1"))
    } else {
        format!("{b}/v1")
    };
    format!("{root}/models")
}

/// Parse an upstream models listing into ids (openai/claude `{data:[{id}]}`,
/// gemini `{models:[{name:"models/x"}]}`).
#[allow(dead_code)]
pub(crate) fn parse_models_list(
    format: crate::registry::Format,
    body: &serde_json::Value,
) -> Vec<String> {
    parse_models_list_with_metadata(format, body).0
}

fn positive_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    value.and_then(|v| v.as_i64()).filter(|v| *v > 0)
}

fn metadata_from_model(format: crate::registry::Format, value: &serde_json::Value) -> crate::registry::ModelMetadata {
    let limit = value.get("limit");
    let context_window = positive_i64(value.get("contextWindow"))
        .or_else(|| positive_i64(value.get("context_window")))
        .or_else(|| positive_i64(value.get("context_length")))
        .or_else(|| positive_i64(value.get("max_input_tokens")))
        .or_else(|| positive_i64(value.get("inputTokenLimit")))
        .or_else(|| positive_i64(limit.and_then(|v| v.get("context"))));
    let max_output_tokens = positive_i64(value.get("maxOutputTokens"))
        .or_else(|| positive_i64(value.get("max_output_tokens")))
        .or_else(|| positive_i64(value.get("maxTokens")))
        .or_else(|| positive_i64(value.get("max_tokens")))
        .or_else(|| positive_i64(value.get("outputTokenLimit")))
        .or_else(|| positive_i64(limit.and_then(|v| v.get("output"))))
        .or_else(|| positive_i64(value.pointer("/top_provider/max_completion_tokens")));
    let input = value
        .get("input")
        .or_else(|| value.get("modalities"))
        .and_then(|v| v.as_array())
        .map(|values| values.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
        .filter(|values| !values.is_empty());
    let supports_vision = value.get("supportsVision").or_else(|| value.get("supports_vision")).and_then(|v| v.as_bool())
        .or_else(|| input.as_ref().map(|values| values.iter().any(|v| v == "image")));
    let supports_pdf = value.get("supportsPdf").or_else(|| value.get("supports_pdf")).and_then(|v| v.as_bool())
        .or_else(|| input.as_ref().map(|values| values.iter().any(|v| v == "pdf")));
    let supports_reasoning = value.get("supportsReasoning").or_else(|| value.get("supports_reasoning")).or_else(|| value.get("reasoning")).and_then(|v| v.as_bool());
    let reasoning_efforts = value
        .get("reasoningEfforts")
        .or_else(|| value.get("reasoning_efforts"))
        .or_else(|| value.get("thinkingLevels"))
        .and_then(|v| {
            v.as_object().map(|map| map.keys().cloned().collect())
                .or_else(|| v.as_array().map(|values| values.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()))
        });
    let wire_format = value
        .get("api")
        .or_else(|| value.get("format"))
        .or_else(|| value.get("wire_format"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let _ = format;
    crate::registry::ModelMetadata {
        format: wire_format,
        context_window,
        max_output_tokens,
        input,
        supports_vision,
        supports_pdf,
        supports_reasoning,
        reasoning_efforts,
    }
}

/// Parse ids and the optional capability records returned by `/models`.
pub(crate) fn parse_models_list_with_metadata(
    format: crate::registry::Format,
    body: &serde_json::Value,
) -> (Vec<String>, std::collections::HashMap<String, crate::registry::ModelMetadata>) {
    let mut metadata = std::collections::HashMap::new();
    if format == crate::registry::Format::Gemini {
        let models: Vec<(String, serde_json::Value)> = body
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().filter_map(|m| {
                let name = m.get("name").and_then(|n| n.as_str())?;
                Some((name.strip_prefix("models/").unwrap_or(name).to_string(), m.clone()))
            }).collect())
            .unwrap_or_default();
        let ids = models.iter().map(|(id, value)| { metadata.insert(id.clone(), metadata_from_model(format, value)); id.clone() }).collect();
        return (ids, metadata);
    }
    let models: Vec<(String, serde_json::Value)> = body.get("data")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|m| Some((m.get("id")?.as_str()?.to_string(), m.clone()))).collect())
        .unwrap_or_default();
    let ids = models.iter().map(|(id, value)| { metadata.insert(id.clone(), metadata_from_model(format, value)); id.clone() }).collect();
    (ids, metadata)
}

/// `POST /v1/provider-connections/{id}/test` — 1-token chat ping.
/// Probe one connection with a 1-token chat ping (shared by the single test
/// endpoint and the "test all" endpoint).
pub async fn probe_connection(
    state: &Arc<AppState>,
    conn: &crate::server::providers_admin::ProviderConnection,
) -> (bool, u64, String) {
    probe_connection_with_model(state, conn, None).await
}

fn probe_wire_entry(
    state: &Arc<AppState>,
    provider: &str,
    model: &str,
    entry: &crate::registry::RegistryEntry,
) -> (crate::registry::RegistryEntry, crate::registry::Format) {
    let format = state.format_for_model(provider, model);
    let mut wire_entry = entry.clone();
    wire_entry.format = format;
    (wire_entry, format)
}

/// Token budget for a probe.
///
/// A probe answers one question — "are the credential, the base URL and the
/// request shape accepted?" — so it must not be starved of budget. Reasoning
/// models spend their whole allowance on hidden thinking before emitting any
/// text: with `max_tokens: 1` such a model returns an empty `content`, and an
/// upstream gateway that (correctly) treats an empty answer as a failed
/// candidate turns the probe into `502 upstream returned empty content`. The
/// probe then reports a broken connection that is in fact healthy.
const PROBE_MAX_TOKENS: u32 = 256;

fn probe_body(format: crate::registry::Format, model: &str) -> Value {
    match format {
        crate::registry::Format::Claude => json!({
            "model": model, "stream": false, "max_tokens": PROBE_MAX_TOKENS,
            "messages": [{"role": "user", "content": "ping"}]
        }),
        crate::registry::Format::Gemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
            "generationConfig": {"maxOutputTokens": PROBE_MAX_TOKENS}
        }),
        crate::registry::Format::OpenAIResponses => json!({
            "model": model, "stream": false, "max_output_tokens": PROBE_MAX_TOKENS,
            "input": "ping"
        }),
        crate::registry::Format::OpenAI => json!({
            "model": model, "stream": false, "max_tokens": PROBE_MAX_TOKENS,
            "messages": [{"role": "user", "content": "ping"}]
        }),
    }
}

/// Probe one connection with a 1-token chat ping, optionally pinned to a
/// specific model (parity: the original's per-model Test button).
pub async fn probe_connection_with_model(
    state: &Arc<AppState>,
    conn: &crate::server::providers_admin::ProviderConnection,
    model_override: Option<String>,
) -> (bool, u64, String) {
    let entry = match state.registry.get(&conn.provider) {
        Some(e) => e,
        None => return (false, 0, format!("unknown provider '{}'", conn.provider)),
    };
    let model = model_override
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| probe_model(conn, &entry));
    let (wire_entry, wire_format) = probe_wire_entry(state, &conn.provider, &model, &entry);
    let mut body = probe_body(wire_format, &model);
    // The anonymous Zen lane answers only a streaming, agent-shaped body with a
    // canonical `ses_*` session (see `crate::opencode_free`). A probe that skips
    // that contract gets `403 FreeTierError`, which is a false alarm about the
    // operator's connection — so the probe speaks the same shape a real request
    // does, and reads only the status (a stream needs no JSON body to prove the
    // credential and the shape were accepted).
    let free_lane = crate::opencode_free::is_free_lane_provider(&conn.provider)
        && crate::opencode_free::decide(&model, &conn.provider).allowed;
    if free_lane {
        crate::opencode_free::apply_lane_contract(
            &mut body,
            wire_format == crate::registry::Format::OpenAIResponses,
        );
    }

    let started = std::time::Instant::now();
    let (key, base) = connection_probe_creds(state, conn);
    let key = if free_lane {
        crate::opencode_free::lane_credential(
            state.config.opencode_free_use_key,
            key,
            crate::server::security::now_ms(),
        )
    } else {
        key
    };
    match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &wire_entry,
        &conn.provider,
        &model,
        free_lane,
        key,
        base,
    ) {
        Ok((url, mut hdrs)) => {
            if free_lane {
                let session = crate::opencode_free::session_for(
                    &format!("probe:{}:{}", conn.provider, model),
                    crate::server::security::now_ms(),
                );
                crate::opencode_free::set_session_headers(&mut hdrs, &session);
                crate::opencode_free::remove_header(&mut hdrs, "x-opencode-request");
            }
            let r = state
                .upstream
                .execute(
                    &url,
                    reqwest::Method::POST,
                    hdrs,
                    Some(axum::body::Bytes::from(body.to_string())),
                    15_000,
                )
                .await;
            let latency = started.elapsed().as_millis() as u64;
            match r {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if free_lane && (200..300).contains(&status) {
                        // Streaming body: the status is the proof.
                        drop(resp);
                        return (true, latency, "connection ok".to_string());
                    }
                    let text = resp.text().await.unwrap_or_default();
                    let ok_flag = (200..300).contains(&status) && !text.contains("\"error\"");
                    if ok_flag {
                        (true, latency, "connection ok".to_string())
                    } else {
                        (false, latency, format!("HTTP {status}: {}", text.chars().take(200).collect::<String>()))
                    }
                }
                Err(e) => (false, latency, e.to_string()),
            }
        }
        Err(e) => (false, 0, e.message),
    }
}

pub async fn provider_connections_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    bytes: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    // optional per-model ping (parity: the original's per-model Test button)
    let model_override: Option<String> = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|b| b.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.trim().is_empty());
    let Some(entry) = state.registry.get(&conn.provider) else {
        return crate::errors::ApiError::new(404, format!("provider '{}' not registered", conn.provider)).into();
    };
    let Some(_base) = conn
        .base_url
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or_else(|| state.base_url_for(&state.registry, &conn.provider))
    else {
        return crate::errors::ApiError::new(500, format!("no upstream base for '{}'", conn.provider)).into();
    };

    // One probe implementation for every surface (this per-model Test button,
    // the plain Test button, "test all" and the combo dry-run): it is lane-aware,
    // so a free model is tested with the shape the anonymous lane requires
    // instead of reporting a misleading `403 FreeTierError`.
    let probed_model = model_override
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| probe_model(&conn, &entry));
    let (ok, latency, detail) = probe_connection_with_model(&state, &conn, model_override).await;
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "ok": ok,
            "provider": conn.provider,
            "model": probed_model,
            "latency_ms": latency,
            "detail": detail,
        })),
    )
        .into_response()
}

/// `POST /v1/provider-connections/{id}/sync-models` — pull the upstream
/// `/models` listing into the connection's `synced_models` (parity: the
/// original's sync-models route; per-provider discovery adapters reduced to
/// the three listable wire formats).
pub async fn provider_connections_sync_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(mut conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    let Some(entry) = state.registry.get(&conn.provider) else {
        return crate::errors::ApiError::new(404, format!("provider '{}' not registered", conn.provider)).into();
    };
    let (key, base) = connection_probe_creds(&state, &conn);
    let Some(base) = base else {
        return crate::errors::ApiError::new(500, format!("no upstream base for '{}'", conn.provider)).into();
    };
    let url = models_url_for(&base);
    // headers mirror the chat path so auth behaves identically
    let (_, hdrs) = match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &entry,
        &conn.provider,
        "model",
        false,
        key,
        Some(base),
    ) {
        Ok(v) => v,
        Err(e) => return e.into(),
    };
    let started = std::time::Instant::now();
    let resp = match state
        .upstream
        .execute(&url, reqwest::Method::GET, hdrs, None, 30_000)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            state.audit("provider_connection.sync_models", format!("id={id} network: {e}"), false);
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"ok": false, "provider": conn.provider,
                    "detail": format!("network error: {e}")})),
            )
                .into_response();
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        state.audit("provider_connection.sync_models", format!("id={id} HTTP {status}"), false);
        return (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({"ok": false, "provider": conn.provider,
                "status": status,
                "detail": text.chars().take(300).collect::<String>()})),
        )
            .into_response();
    }
    let body: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
    let (mut models, metadata) = parse_models_list_with_metadata(entry.format, &body);
    models.sort();
    models.dedup();
    models.truncate(500);
    let latency = started.elapsed().as_millis() as u64;
    let metadata_count = metadata.len();
    conn.synced_models = models.clone();
    conn.model_metadata = metadata;
    conn.synced_at_ms = crate::server::security::now_ms();
    state.provider_connections.upsert(conn);
    state.audit(
        "provider_connection.sync_models",
        format!("id={id} synced={}", models.len()),
        true,
    );
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({"ok": true, "provider": entry.id,
            "synced": models.len(), "models": models, "latency_ms": latency,
            "capability_metadata": metadata_count,
            "synced_at_ms": crate::server::security::now_ms()})),
    )
        .into_response()
}

fn connection_models_view(state: &Arc<AppState>, conn: &crate::server::providers_admin::ProviderConnection) -> serde_json::Value {
    let registry = state.registry.models_for(&conn.provider);
    let hidden: std::collections::HashSet<&str> =
        conn.hidden_models.iter().map(String::as_str).collect();
    let synced_authoritative = conn.synced_at_ms > 0;
    // When the upstream list has been synced it is authoritative: built-in
    // registry seeds are dropped from the routable set (they would only
    // duplicate a synced id). Hand-added manual models survive either way —
    // dropping them would silently unroute a model the operator declared.
    let mut effective: Vec<String> = if synced_authoritative {
        conn.model_list
            .iter()
            .chain(conn.synced_models.iter())
            .filter(|m| !hidden.contains(m.as_str()))
            .cloned()
            .collect()
    } else {
        registry
            .iter()
            .chain(conn.model_list.iter())
            .chain(conn.synced_models.iter())
            .filter(|m| !hidden.contains(m.as_str()))
            .cloned()
            .collect()
    };
    effective.sort();
    effective.dedup();
    let mut chosen: std::collections::HashMap<String, (usize, String)> = std::collections::HashMap::new();
    for (rank, (source, models)) in [
        (0, ("manual", &conn.model_list)),
        (1, ("synced", &conn.synced_models)),
        (2, ("registry", &registry)),
    ] {
        // A synced upstream list is authoritative, so built-in seeds are not
        // offered as separate entries — only the synced ids and any hand-added
        // manual ones appear in the detail view.
        if synced_authoritative && rank == 2 {
            continue;
        }
        for model in models {
            chosen
                .entry(model.clone())
                .and_modify(|existing| {
                    if rank < existing.0 {
                        *existing = (rank, source.to_string());
                    }
                })
                .or_insert((rank, source.to_string()));
        }
    }
    let mut card_models: Vec<String> = chosen.keys().cloned().collect();
    card_models.sort();
    let mut capabilities = serde_json::Map::new();
    let mut cards = Vec::new();
    for model in card_models {
        let (rank, source) = chosen.get(&model).cloned().unwrap_or((3, "registry".to_string()));
        let _ = rank;
        let caps = state.capabilities_for_model(&conn.provider, &model);
        let capability = serde_json::json!({
            "contextWindow": caps.context_window,
            "maxOutputTokens": caps.max_output_tokens,
            "input": caps.input,
            "supportsVision": caps.supports_vision,
            "supportsPdf": caps.supports_pdf,
            "supportsReasoning": caps.supports_reasoning,
            "reasoningEfforts": caps.reasoning_efforts,
        });
        capabilities.insert(model.clone(), capability.clone());
        cards.push(serde_json::json!({
            "model": model,
            "source": source,
            "hidden": hidden.contains(model.as_str()),
            "capabilities": capability,
        }));
    }
    serde_json::json!({
        "id": conn.id, "provider": conn.provider,
        "manual": conn.model_list, "synced": conn.synced_models,
        "modelMetadata": conn.model_metadata,
        "syncedAtMs": conn.synced_at_ms, "registry": registry,
        "hidden": conn.hidden_models, "effective": effective,
        "capabilities": capabilities,
        "cards": cards,
    })
}

/// `GET /v1/provider-connections/{id}/models` — partitioned model view for
/// the detail page (parity: the original's `[id]/models` route): manual,
/// synced and registry seeds, hidden flags, effective routable list, and
/// resolved per-model capabilities.
pub async fn provider_connections_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    (
        axum::http::StatusCode::OK,
        axum::Json(connection_models_view(&state, &conn)),
    )
        .into_response()
}

/// `DELETE /v1/provider-connections/{id}/models/{model}` — remove one manually
/// added model. Synced and registry entries with the same id remain available
/// through their own sources, so this never deletes upstream state.
pub async fn provider_connection_model_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, model)): Path<(String, String)>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let Some(mut conn) = state.provider_connections.get(&id) else {
        return crate::errors::ApiError::new(404, format!("connection '{id}' not found")).into();
    };
    let before = conn.model_list.len();
    conn.model_list.retain(|candidate| candidate != &model);
    if conn.model_list.len() == before {
        return crate::errors::ApiError::new(
            404,
            format!("manual model '{model}' not found on connection '{id}'"),
        )
        .into();
    }
    state.provider_connections.upsert(conn.clone());
    state.audit(
        "provider_connection.model_delete",
        format!("id={id} model={model}"),
        true,
    );
    (
        axum::http::StatusCode::OK,
        axum::Json(connection_models_view(&state, &conn)),
    )
        .into_response()
}

/// `POST /v1/admin/service/restart` — restart the gateway process.
///
/// Parity: the original sidebar's 重启服务 action. Under systemd
/// (`Restart=on-failure`) a non-zero exit triggers an automatic restart, so we
/// abort after the response is flushed.
pub async fn service_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    state.audit("service.restart", "requested from dashboard", true);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        eprintln!("[SERVICE] restart requested from the dashboard — exiting for systemd to restart");
        std::process::abort();
    });
    (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(json!({"ok": true, "action": "restart", "message": "restarting"})),
    )
        .into_response()
}

/// `POST /v1/admin/service/stop` — stop the gateway process.
///
/// Parity: the original sidebar's 停止服务 action. A clean exit(0) makes
/// systemd (`Restart=on-failure`) leave the unit stopped.
pub async fn service_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    state.audit("service.stop", "requested from dashboard", true);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        eprintln!("[SERVICE] stop requested from the dashboard — exiting");
        std::process::exit(0);
    });
    (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(json!({"ok": true, "action": "stop", "message": "stopping"})),
    )
        .into_response()
}

/// `POST /v1/api-keys/{id}/rotate` — issue a fresh secret for an existing key
/// (parity: the API Manager row's rotate action).
pub async fn api_keys_rotate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    match state.api_keys.rotate(&id) {
        Some(entry) => {
            state.audit("api_key.rotate", format!("id={id}"), true);
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "api_key": key_json(&entry, &entry.key) })),
            )
                .into_response()
        }
        None => crate::errors::ApiError::new(404, "api key not found").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_connection_fields_fall_back_to_registry_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        let mut conn = ProviderConnection {
            id: "c1".into(),
            provider: "openrouter".into(),
            name: "or".into(),
            api_key: Some("sk-or-live".into()),
            base_url: Some("".into()),
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        apply_connection(&state, &mut conn);
        assert!(conn.base_url.is_none(), "blank base URL normalized");
        assert_eq!(
            state.base_url_for(&state.registry, "openrouter").as_deref(),
            Some("https://openrouter.ai/api/v1"),
            "empty base falls back to the registry default"
        );
        assert_eq!(
            state.api_key_for("openrouter").as_deref(),
            Some("sk-or-live"),
            "managed key resolves through the overlay"
        );
    }

    #[test]
    fn models_url_collapses_chat_paths_to_the_listing() {
        assert_eq!(models_url_for("https://api.openai.com/v1"), "https://api.openai.com/v1/models");
        assert_eq!(
            models_url_for("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(models_url_for("https://api.dify.ai"), "https://api.dify.ai/v1/models");
        assert_eq!(
            models_url_for("https://tabitoken.com/v1/messages"),
            "https://tabitoken.com/v1/models"
        );
        assert_eq!(
            models_url_for("https://generativelanguage.googleapis.com/v1beta"),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        assert_eq!(
            models_url_for("https://generativelanguage.googleapis.com/v1beta/models"),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        // The OpenCode inference surface serves both chat and models on the same
        // base URL; it must not be rewritten to the legacy `/zen` or to the
        // narrower `/inference/openai/v1` (which returns 404 for `/models`).
        assert_eq!(
            models_url_for("https://opencode.ai/inference/v1/chat/completions"),
            "https://opencode.ai/inference/v1/models"
        );
        assert_eq!(models_url_for("https://opencode.ai/inference/v1"), "https://opencode.ai/inference/v1/models");
    }

    #[test]
    fn models_list_parses_all_three_wire_shapes() {
        use crate::registry::Format;
        let openai = serde_json::json!({"data": [{"id": "a"}, {"id": "b"}, {}]});
        assert_eq!(parse_models_list(Format::OpenAI, &openai), vec!["a", "b"]);
        let claude = serde_json::json!({"data": [{"id": "c"}]});
        assert_eq!(parse_models_list(Format::Claude, &claude), vec!["c"]);
        let gemini = serde_json::json!({"models": [{"name": "models/x"}, {"name": "y"}]});
        assert_eq!(parse_models_list(Format::Gemini, &gemini), vec!["x", "y"]);
        assert!(parse_models_list(Format::OpenAI, &serde_json::json!({})).is_empty());
    }

    #[test]
    fn models_list_preserves_provider_capability_metadata() {
        use crate::registry::Format;
        let body = serde_json::json!({"data": [{
            "id": "custom-long",
            "contextWindow": 1048576,
            "maxOutputTokens": 131072,
            "input": ["text", "image", "pdf"],
            "supportsReasoning": true,
            "reasoningEfforts": {"off": null, "high": "high"}
        }]});
        let (models, metadata) = parse_models_list_with_metadata(Format::OpenAI, &body);
        assert_eq!(models, vec!["custom-long"]);
        let m = metadata.get("custom-long").unwrap();
        assert_eq!(m.context_window, Some(1048576));
        assert_eq!(m.max_output_tokens, Some(131072));
        assert_eq!(m.supports_reasoning, Some(true));
        assert_eq!(m.input.as_ref().unwrap().len(), 3);
        assert_eq!(m.reasoning_efforts.as_ref().unwrap(), &vec!["off", "high"]);
    }

    #[test]
    fn model_cards_prefer_manual_then_synced_then_registry() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        let conn = ProviderConnection {
            id: "cards".into(),
            provider: "openrouter".into(),
            name: String::new(),
            api_key: None,
            base_url: None,
            api_type: None,
            model_list: vec!["auto".into(), "manual-only".into()],
            synced_models: vec!["auto".into(), "synced-only".into()],
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: vec!["manual-only".into()],
            enabled: true,
            created_at_ms: 0,
        };
        let view = connection_models_view(&state, &conn);
        let cards = view["cards"].as_array().unwrap();
        let card = |model: &str| {
            cards.iter().find(|card| card["model"] == model).unwrap_or_else(|| panic!("missing {model} card"))
        };
        assert_eq!(card("auto")["source"], "manual");
        assert_eq!(card("synced-only")["source"], "synced");
        assert_eq!(card("manual-only")["hidden"], true);
        assert!(card("auto")["capabilities"]["contextWindow"].is_number());
        assert_eq!(cards.len(), 3);
    }

    #[test]
    fn probe_model_prefers_saved_then_registry_then_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        let entry = state.registry.get("openrouter").unwrap();
        // registry default (openrouter's `auto`: bare ids are a 404 upstream)
        let bare = ProviderConnection {
            id: "c1".into(),
            provider: "openrouter".into(),
            name: String::new(),
            api_key: None,
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        assert_eq!(probe_model(&bare, &entry), "auto");
        // the connection's own list wins
        let mut custom = bare.clone();
        custom.model_list = vec!["openai/gpt-4o-mini".into()];
        assert_eq!(probe_model(&custom, &entry), "openai/gpt-4o-mini");
    }

    /// A chained gateway publishes its own models but has no curated defaults.
    /// The probe must ask for one of *those*, not a hardcoded id the upstream
    /// never heard of — that reports a broken credential when nothing is wrong.
    #[test]
    fn probe_model_uses_synced_models_for_custom_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        state.registry.register_dynamic(
            "openai-compatible-omniroute-server",
            Some("http://10.0.0.1:20128/v1".into()),
            None,
            Vec::new(),
        );
        let entry = state.registry.get("openai-compatible-omniroute-server").unwrap();
        let conn = ProviderConnection {
            id: "chain".into(),
            provider: "openai-compatible-omniroute-server".into(),
            name: "omniroute-server".into(),
            api_key: Some("k".into()),
            base_url: Some("http://10.0.0.1:20128/v1".into()),
            api_type: None,
            model_list: Vec::new(),
            synced_models: vec!["oversea-free".into(), "coding-free".into()],
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        assert_eq!(probe_model(&conn, &entry), "oversea-free");
        // hidden entries are not probed when a visible one exists
        let mut masked = conn.clone();
        masked.hidden_models = vec!["oversea-free".into()];
        assert_eq!(probe_model(&masked, &entry), "coding-free");
    }

    /// A second account of the same provider serves the same catalogue, so it
    /// must start from the visibility the operator already chose instead of
    /// re-exposing every model they had hidden.
    #[test]
    fn new_account_inherits_hidden_models_of_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        state.provider_connections.upsert(ProviderConnection {
            id: "first".into(),
            provider: "opencode-go".into(),
            name: "primary".into(),
            api_key: Some("k1".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: vec!["alpha".into(), "beta".into()],
            enabled: true,
            created_at_ms: 0,
        });

        // No `hidden_models` in the request: this is a new account with no
        // opinion yet, so it inherits.
        let mut second = ProviderConnection {
            id: "second".into(),
            provider: "opencode-go".into(),
            name: "backup".into(),
            api_key: Some("k2".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        inherit_hidden_models(&state, &mut second, None);
        assert_eq!(second.hidden_models, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// An explicit list — including an empty one — is the operator speaking,
    /// so it must not be overwritten by inheritance.
    #[test]
    fn explicit_hidden_list_wins_over_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        state.provider_connections.upsert(ProviderConnection {
            id: "first".into(),
            provider: "opencode-go".into(),
            name: "primary".into(),
            api_key: Some("k1".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: vec!["alpha".into()],
            enabled: true,
            created_at_ms: 0,
        });
        let mut conn = ProviderConnection {
            id: "second".into(),
            provider: "opencode-go".into(),
            name: "backup".into(),
            api_key: Some("k2".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        // "Show all" on the second account clears its mask and stays cleared.
        inherit_hidden_models(&state, &mut conn, Some(Vec::new()));
        assert!(conn.hidden_models.is_empty());

        let mut third = conn.clone();
        third.id = "third".into();
        inherit_hidden_models(&state, &mut third, Some(vec!["gamma".into()]));
        assert_eq!(third.hidden_models, vec!["gamma".to_string()]);
    }

    /// Providers are independent: hiding a model on one must not leak into
    /// another provider's brand-new account.
    #[test]
    fn inheritance_does_not_cross_providers() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::for_tests(Vec::new(), Some(dir.path().to_path_buf())));
        state.provider_connections.upsert(ProviderConnection {
            id: "zen".into(),
            provider: "opencode-zen".into(),
            name: "zen".into(),
            api_key: Some("k1".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: vec!["alpha".into()],
            enabled: true,
            created_at_ms: 0,
        });
        let mut go = ProviderConnection {
            id: "go".into(),
            provider: "opencode-go".into(),
            name: "go".into(),
            api_key: Some("k2".into()),
            base_url: None,
            api_type: None,
            model_list: Vec::new(),
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: Vec::new(),
            enabled: true,
            created_at_ms: 0,
        };
        inherit_hidden_models(&state, &mut go, None);
        assert!(go.hidden_models.is_empty(), "zen's mask must not reach opencode-go");
    }

    /// The probe must not starve a reasoning model: at `max_tokens: 1` the whole
    /// allowance goes to hidden thinking, the answer comes back empty, and an
    /// upstream gateway that treats empty answers as failed candidates reports
    /// `502` for a connection that is actually healthy.
    #[test]
    fn probe_body_allows_a_reasoning_model_to_answer() {
        for format in [
            crate::registry::Format::OpenAI,
            crate::registry::Format::Claude,
            crate::registry::Format::OpenAIResponses,
        ] {
            let body = probe_body(format, "m");
            let used = body
                .get("max_tokens")
                .or_else(|| body.get("max_output_tokens"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            assert!(
                used >= 64,
                "probe budget must leave room for reasoning, got {used} for {format:?}"
            );
        }
        let gemini = probe_body(crate::registry::Format::Gemini, "m");
        let used = gemini
            .pointer("/generationConfig/maxOutputTokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        assert!(used >= 64, "gemini probe budget must leave room, got {used}");
    }
}
