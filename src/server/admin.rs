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
            let cookie = format!(
                "omniroute_session={token}; HttpOnly; Path=/dashboard; Max-Age=604800; SameSite=Lax"
            );
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
        None => crate::errors::ApiError::new(401, "invalid password").into(),
    }
}

/// `POST /v1/auth/logout` — revoke the presented session.
pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(t) = crate::server::auth::session_token(&headers) {
        state.auth.logout(&t);
    }
    (axum::http::StatusCode::OK, axum::Json(json!({"ok": true}))).into_response()
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
        })),
    )
        .into_response()
}

// ── API keys ────────────────────────────────────────────────────────────

fn key_json(k: &crate::server::security::ApiKeyEntry, key_display: &str) -> Value {
    json!({
        "id": k.id, "name": k.name, "key": key_display, "role": k.role,
        "enabled": k.enabled, "created_at_ms": k.created_at_ms,
        "last_used_at_ms": k.last_used_at_ms, "total_requests": k.total_requests,
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
    let entry = state.api_keys.create(name, role);
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
    apply_connection(&state, &mut conn);
    state.provider_connections.upsert(conn.clone());
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "connection": conn_json(&conn) })),
    )
        .into_response()
}

/// Register/refresh a connection in the live registry + credential overlay.
fn apply_connection(state: &Arc<AppState>, conn: &mut ProviderConnection) {
    if conn.provider.starts_with("openai-compatible")
        || conn.provider.starts_with("anthropic-compatible")
    {
        state.registry.register_dynamic(
            &conn.provider,
            conn.base_url.clone(),
            conn.api_type.clone(),
            conn.model_list.clone(),
        );
    }
    if conn.api_key.is_some() || conn.base_url.is_some() {
        let cred = crate::config::ProviderCredentials {
            api_key: conn.api_key.clone(),
            base_url: conn.base_url.clone(),
            api_type: conn.api_type.clone(),
            model_list: conn.model_list.clone(),
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

/// `POST /v1/provider-connections/{id}/test` — 1-token chat ping.
pub async fn provider_connections_test(
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
    let Some(entry) = state.registry.get(&conn.provider) else {
        return crate::errors::ApiError::new(404, format!("provider '{}' not registered", conn.provider)).into();
    };
    let Some(_base) = state.base_url_for(&state.registry, &conn.provider) else {
        return crate::errors::ApiError::new(500, format!("no upstream base for '{}'", conn.provider)).into();
    };

    let model = conn
        .model_list
        .first()
        .cloned()
        .or_else(|| entry.default_models.first().cloned())
        .unwrap_or_else(|| "model".into());
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
    match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &entry,
        &conn.provider,
        &model,
        false,
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
