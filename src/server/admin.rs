//! Dashboard/management endpoints: login/logout, API-key CRUD,
//! provider-connection CRUD + connection test (parity: original dashboard
//! auth + `/api/keys` + `/api/providers` management).

use crate::server::providers_admin::{mask_key, ProviderConnection};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use std::sync::Arc;

// ── auth ────────────────────────────────────────────────────────────────

/// `POST /v1/auth/login` {password} → session token (+ cookie).
pub async fn login(
    State(state): State<Arc<AppState>>,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
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
    let mut conn = match serde_json::from_slice::<ProviderConnection>(&bytes) {
        Ok(c) => c,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid body: {e}")).into(),
    };
    if conn.provider.is_empty() {
        return crate::errors::ApiError::new(400, "missing required field: provider").into();
    }
    if conn.name.is_empty() {
        conn.name = conn.provider.clone();
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
/// generic bare fallback), then a last-resort bare id.
fn probe_model(
    conn: &crate::server::providers_admin::ProviderConnection,
    entry: &crate::registry::RegistryEntry,
) -> String {
    conn.model_list
        .first()
        .cloned()
        .or_else(|| entry.default_models.first().cloned())
        .unwrap_or_else(|| "gpt-4o-mini".to_string())
}

/// Credentials a probe must use: the tested connection's own key/base first
/// (blanks ignored), then the live overlay, then static config. Without this
/// the probe would test the registry default instead of what the user saved
/// (e.g. an openrouter key with no base URL override).
fn connection_probe_creds(
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
pub(crate) fn parse_models_list(
    format: crate::registry::Format,
    body: &serde_json::Value,
) -> Vec<String> {
    if format == crate::registry::Format::Gemini {
        return body
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                    .collect()
            })
            .unwrap_or_default();
    }
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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
    let body = if entry.format == crate::registry::Format::Gemini {
        json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}],
               "generationConfig": {"maxOutputTokens": 1}})
    } else {
        json!({"model": model, "stream": false, "max_tokens": 1,
               "messages": [{"role": "user", "content": "ping"}]})
    };

    let started = std::time::Instant::now();
    let (key, base) = connection_probe_creds(state, conn);
    match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &entry,
        &conn.provider,
        &model,
        false,
        key,
        base,
    ) {
        Ok((url, hdrs)) => {
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

    let model = model_override
        .clone()
        .unwrap_or_else(|| probe_model(&conn, &entry));
    let body = if entry.format == crate::registry::Format::Claude {
        json!({"model": model, "stream": false, "max_tokens": 1,
               "messages": [{"role": "user", "content": "ping"}]})
    } else if entry.format == crate::registry::Format::Gemini {
        json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}],
               "generationConfig": {"maxOutputTokens": 1}})
    } else {
        json!({"model": model, "stream": false, "max_tokens": 1,
               "messages": [{"role": "user", "content": "ping"}]})
    };

    let started = std::time::Instant::now();
    let (key, base) = connection_probe_creds(&state, &conn);
    match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &entry,
        &conn.provider,
        &model,
        false,
        key,
        base,
    ) {
        Ok((url, hdrs)) => {
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
                    let text = resp.text().await.unwrap_or_default();
                    let ok_flag = (200..300).contains(&status) && !text.contains("\"error\"");
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "ok": ok_flag,
                            "provider": conn.provider,
                            "model": model,
                            "status": status,
                            "latency_ms": latency,
                            "detail": if ok_flag { "connection ok".to_string() }
                                      else { text.chars().take(300).collect::<String>() },
                        })),
                    )
                        .into_response()
                }
                Err(e) => (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "ok": false, "provider": conn.provider,
                        "detail": format!("network error: {e}")
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"ok": false, "provider": conn.provider, "detail": e.message})),
        )
            .into_response(),
    }
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
    let mut models = parse_models_list(entry.format, &body);
    models.sort();
    models.dedup();
    models.truncate(500);
    let latency = started.elapsed().as_millis() as u64;
    conn.synced_models = models.clone();
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
            "synced_at_ms": crate::server::security::now_ms()})),
    )
        .into_response()
}

/// `GET /v1/provider-connections/{id}/models` — partitioned model view for
/// the detail page (parity: the original's `[id]/models` route): manual,
/// synced and registry seeds, hidden flags, and the effective routable list.
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
    let registry = state.registry.models_for(&conn.provider);
    let hidden: std::collections::HashSet<&str> =
        conn.hidden_models.iter().map(String::as_str).collect();
    let mut effective: Vec<String> = registry
        .iter()
        .chain(conn.model_list.iter())
        .chain(conn.synced_models.iter())
        .filter(|m| !hidden.contains(m.as_str()))
        .cloned()
        .collect();
    effective.sort();
    effective.dedup();
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "id": conn.id, "provider": conn.provider,
            "manual": conn.model_list, "synced": conn.synced_models,
            "syncedAtMs": conn.synced_at_ms, "registry": registry,
            "hidden": conn.hidden_models, "effective": effective,
        })),
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
}
