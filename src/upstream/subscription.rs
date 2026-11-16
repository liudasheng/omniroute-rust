//! Subscription-only provider request shaping. Responses and Gemini SSE are
//! left streaming; the normal gateway pipeline translates them for all clients.
use crate::{errors::ApiError, oauth::OAuthCredentials, state::AppState};
use serde_json::{json, Value};
use std::sync::Arc;
use super::executor::UpstreamResponse;

pub fn shape_codex(body: &mut Value, model: &str) {
    body["model"] = json!(model);
    body["stream"] = json!(true);
    body["store"] = json!(false);
    if body.get("instructions").and_then(Value::as_str).is_none_or(str::is_empty) {
        body["instructions"] = json!("You are a helpful coding assistant.");
    }
    if let Some(object) = body.as_object_mut() {
        for field in ["max_tokens", "max_output_tokens", "max_completion_tokens", "temperature", "top_p", "stream_options", "user", "metadata", "truncation", "background", "session_id", "conversation_id"] {
            object.remove(field);
        }
    }
}

pub fn shape_antigravity(body: &mut Value, model: &str, oauth: &OAuthCredentials, account_id: &str) -> Result<(), ApiError> {
    let project = oauth.project_id.as_deref().filter(|p| !p.trim().is_empty())
        .or_else(|| oauth.metadata_str("projectId"))
        .filter(|p| *p != "__REQUIRES_GCP_PROJECT__")
        .ok_or_else(|| ApiError::new(422, "Antigravity requires a Cloud Code project ID; configure projectId on this account"))?;
    let upstream_model = match model {
        "gemini-3.1-pro-high" => "gemini-pro-agent",
        "gemini-3.7-flash" | "gemini-3.7-flash-high" | "gemini-3.7-flash-medium" | "gemini-3.7-flash-low" => "gemini-3.7-flash-tiered",
        "gemini-3.8-flash" | "gemini-3.8-flash-tiered" => "gemini-3.8-flash-high",
        "gemini-claude-sonnet-4-5" | "gemini-claude-sonnet-4-5-thinking" => "claude-sonnet-4-6",
        "gemini-claude-opus-4-5-thinking" => "claude-opus-4-6-thinking",
        other => other,
    };
    let mut request = body.clone();
    if let Some(object) = request.as_object_mut() { object.remove("model"); object.remove("stream"); }
    // Native Cloud Code session IDs are signed FNV-1a account hashes.
    let mut hash = 0xcbf29ce484222325u64;
    for b in account_id.bytes() { hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3); }
    request["sessionId"] = json!((hash as i64).to_string());
    if let Some(contents) = request.get_mut("contents").and_then(Value::as_array_mut) {
        for content in contents.iter_mut() {
            if let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) {
                for part in parts {
                    // Tool calls replayed without native signatures require
                    // the same sentinel used by npm OmniRoute.
                    if part.get("functionCall").is_some() && part.get("thoughtSignature").is_none() {
                        part["thoughtSignature"] = json!("skip_thought_signature_validator");
                    }
                }
            }
        }
        // New Gemini and Claude endpoints reject a trailing assistant turn.
        if contents.len() > 1 && contents.last().and_then(|c| c.get("role")).and_then(Value::as_str) == Some("model") {
            contents.pop();
        }
    }
    if request.get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty()) {
        request["toolConfig"] = json!({"functionCallingConfig":{"mode":"VALIDATED"}});
    }
    *body = json!({
        "project": project, "model": upstream_model, "request": request,
        "requestId": format!("agent/{}/{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]),
        "userAgent": "antigravity", "requestType": "agent"
    });
    Ok(())
}

pub async fn execute(
    state: &Arc<AppState>, provider: &str, connection_id: &str, model: &str,
    base_url: &str, body: Value,
) -> Result<UpstreamResponse, ApiError> {
    let mut oauth = state.oauth.credentials(state, connection_id, None).await?;
    for attempt in 0..2 {
        let conn = state.provider_connections.get(connection_id)
            .ok_or_else(|| ApiError::new(401, "OAuth account removed"))?;
        let mut request = body.clone();
        let base = base_url.trim_end_matches('/');
        if provider == "antigravity" && oauth.project_id.as_deref().filter(|p| !p.trim().is_empty()).is_none()
            && oauth.metadata_str("projectId").is_none() {
            discover_project(state, connection_id, base, conn.proxy_url.as_deref(), &mut oauth).await?;
        }
        let url = if provider == "codex" {
            shape_codex(&mut request, model);
            if base.ends_with("/responses") { base.to_string() } else { format!("{base}/responses") }
        } else {
            shape_antigravity(&mut request, model, &oauth, connection_id)?;
            format!("{base}/v1internal:streamGenerateContent?alt=sse")
        };
        let token = oauth.access_token.as_deref().ok_or_else(|| ApiError::new(401, "Missing OAuth access token"))?;
        let client = state.upstream.oauth_client(conn.proxy_url.as_deref())?;
        let mut req = client.post(&url).bearer_auth(token).json(&request)
            .timeout(std::time::Duration::from_millis(state.config.request_timeout_ms));
        if provider == "codex" {
            let version = std::env::var("CODEX_CLIENT_VERSION").unwrap_or_else(|_| "0.165.0".into());
            req = req.header("accept", "text/event-stream")
                .header("originator", "codex_cli_rs")
                .header("version", &version)
                .header("user-agent", format!("codex_cli_rs/{version}"));
            if let Some(workspace) = oauth.metadata_str("workspaceId") { req = req.header("chatgpt-account-id", workspace); }
            if let Some(session) = request.get("prompt_cache_key").and_then(Value::as_str) { req = req.header("session_id", session); }
        } else if oauth.metadata_str("clientProfile") == Some("cli") {
            let version = std::env::var("ANTIGRAVITY_CLI_VERSION").unwrap_or_else(|_| "1.3.4".into());
            let os = if cfg!(target_os = "windows") { "windows" } else { std::env::consts::OS };
            let arch = if cfg!(target_arch = "x86_64") { "amd64" } else { std::env::consts::ARCH };
            // The official Go CLI negotiates SSE by ?alt=sse, not Accept.
            req = req.header("user-agent", format!("antigravity/cli/{version} (aidev_client; os_type={os}; arch={arch}; auth_method=oauth)"));
        } else {
            let version = std::env::var("ANTIGRAVITY_CLIENT_VERSION").unwrap_or_else(|_| "2.5.5".into());
            req = req.header("accept", "text/event-stream")
                .header("user-agent", format!("antigravity/ide/{version} darwin/arm64"));
        }
        let response = req.send().await.map_err(|_| ApiError::new(502, "Subscription upstream network error"))?;
        if response.status().as_u16() == 401 && attempt == 0 {
            oauth = state.oauth.credentials(state, connection_id, Some(token)).await?;
            continue;
        }
        return Ok(UpstreamResponse::from_reqwest(response));
    }
    Err(ApiError::new(401, "OAuth account rejected after refresh"))
}

/// Discover the authenticated account's live catalog using its native protocol.
/// Normalize IDs to OpenAI's listing shape for the shared metadata parser.
pub async fn discover_models(state: &Arc<AppState>, id: &str, base_url: &str) -> Result<Value, ApiError> {
    let mut oauth = state.oauth.credentials(state, id, None).await?;
    for attempt in 0..2 {
        let conn = state.provider_connections.get(id)
            .ok_or_else(|| ApiError::new(404, "OAuth account removed"))?;
        let base = base_url.trim_end_matches('/');
        let token = oauth.access_token.as_deref()
            .ok_or_else(|| ApiError::new(401, "Missing OAuth access token"))?;
        let client = state.upstream.oauth_client(conn.proxy_url.as_deref())?;
        let request = if conn.provider == "codex" {
            let base = base.strip_suffix("/responses").unwrap_or(base);
            let version = std::env::var("CODEX_CLIENT_VERSION").unwrap_or_else(|_| "0.165.0".into());
            let mut request = client.get(format!("{base}/models"))
                .query(&[("client_version", &version)])
                .header("originator", "codex_cli_rs")
                .header("version", &version)
                .header("user-agent", format!("codex_cli_rs/{version}"));
            if let Some(workspace) = oauth.metadata_str("workspaceId") {
                request = request.header("chatgpt-account-id", workspace);
            }
            request
        } else {
            let project = oauth.project_id.as_deref().or_else(|| oauth.metadata_str("projectId"))
                .filter(|p| !p.trim().is_empty() && *p != "__REQUIRES_GCP_PROJECT__");
            let mut request = client.post(format!("{base}/v1internal:fetchAvailableModels"))
                .json(&project.map(|p| json!({"project":p})).unwrap_or_else(|| json!({})));
            if oauth.metadata_str("clientProfile") == Some("cli") {
                let version = std::env::var("ANTIGRAVITY_CLI_VERSION").unwrap_or_else(|_| "1.3.4".into());
                let os = if cfg!(target_os = "windows") { "windows" } else { std::env::consts::OS };
                let arch = if cfg!(target_arch = "x86_64") { "amd64" } else { std::env::consts::ARCH };
                request = request.header("user-agent", format!("antigravity/cli/{version} (aidev_client; os_type={os}; arch={arch}; auth_method=oauth)"));
            } else {
                let version = std::env::var("ANTIGRAVITY_CLIENT_VERSION").unwrap_or_else(|_| "2.5.5".into());
                request = request.header("user-agent", format!("antigravity/ide/{version} darwin/arm64"));
            }
            request
        };
        let response = request.bearer_auth(token).header("accept", "application/json")
            .timeout(std::time::Duration::from_secs(30)).send().await
            .map_err(|_| ApiError::new(502, "Subscription model discovery network error"))?;
        if response.status().as_u16() == 401 && attempt == 0 {
            oauth = state.oauth.credentials(state, id, Some(token)).await?;
            continue;
        }
        if !response.status().is_success() {
            return Err(ApiError::new(502, format!("{} model discovery returned HTTP {}", conn.provider, response.status().as_u16())));
        }
        let body: Value = response.json().await
            .map_err(|_| ApiError::new(502, "Invalid subscription model discovery JSON"))?;
        return normalize_discovery_models(&conn.provider, &body);
    }
    Err(ApiError::new(401, "OAuth model discovery rejected after refresh"))
}

fn normalize_discovery_models(provider: &str, body: &Value) -> Result<Value, ApiError> {
    let models = body.get("models").or_else(|| body.get("data")).unwrap_or(body);
    let mut data = Vec::new();
    if body.get("error").is_some() {
        return Err(ApiError::new(502, "Subscription discovery returned an error envelope"));
    }
    if let Some(items) = models.as_array() {
        for item in items {
            if provider == "codex" && item.get("supported_in_api").and_then(Value::as_bool) == Some(false) { continue; }
            let id = item.get("slug").or_else(|| item.get("id")).or_else(|| item.get("name")).and_then(Value::as_str);
            if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                let mut model = item.clone();
                model["id"] = json!(id.strip_prefix("models/").unwrap_or(id));
                data.push(model);
            }
        }
        if !items.is_empty() && data.is_empty() {
            return Err(ApiError::new(502, "Subscription catalog contained no usable model IDs"));
        }
    } else if provider == "antigravity" && body.get("models").is_some() && models.is_object() {
        for (id, item) in models.as_object().unwrap() {
            if id.trim().is_empty() || !item.is_object() { continue; }
            let mut model = item.clone();
            model["id"] = json!(id);
            data.push(model);
        }
    } else {
        return Err(ApiError::new(502, "Unrecognized subscription model catalog; existing models kept"));
    }
    Ok(json!({"data":data}))
}

/// Recover project assignment for existing accounts without fabricating IDs or
/// onboarding a new Google project. Manual-project accounts get a typed 422.
async fn discover_project(
    state: &Arc<AppState>, id: &str, base: &str, proxy: Option<&str>, oauth: &mut OAuthCredentials,
) -> Result<(), ApiError> {
    let platform = if cfg!(target_os = "windows") { 5 } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") { 2 } else { 1 }
    } else if cfg!(target_arch = "aarch64") { 4 } else { 3 };
    let response = state.upstream.oauth_client(proxy)?.post(format!("{base}/v1internal:loadCodeAssist"))
        .bearer_auth(oauth.access_token.as_deref().unwrap_or_default())
        .header("user-agent", "google-api-nodejs-client/10.3.0")
        .header("x-goog-api-client", "gl-node/22.21.1")
        .json(&json!({"metadata":{"ideType":9,"platform":platform,"pluginType":2}}))
        .send().await.map_err(|_| ApiError::new(502, "Antigravity project discovery unavailable"))?;
    if !response.status().is_success() {
        return Err(ApiError::new(422, "Antigravity project discovery failed; configure projectId on this account"));
    }
    let body: Value = response.json().await.map_err(|_| ApiError::new(502, "Invalid project discovery response"))?;
    let project = body.get("cloudaicompanionProject").and_then(|p| p.as_str().or_else(|| p.get("id").and_then(Value::as_str)))
        .filter(|s| !s.trim().is_empty() && *s != "__REQUIRES_GCP_PROJECT__")
        .ok_or_else(|| ApiError::new(422, "Antigravity requires a Cloud Code project ID; configure projectId on this account"))?;
    *oauth = state.provider_connections.update_project(id, project.to_string())
        .ok_or_else(|| ApiError::new(500, "Cannot persist discovered Antigravity project"))?;
    Ok(())
}
