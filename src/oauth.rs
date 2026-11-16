//! Managed OAuth credentials and serialized refresh for subscription providers.
//! Protocols ported from OmniRoute's tokenRefresh/providers/codex.ts and
//! src/lib/oauth/providers/antigravity.ts. Never open the npm data directory.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::errors::ApiError;
use crate::state::AppState;
use std::sync::Arc;

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "accessToken")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "idToken")]
    pub id_token: Option<String>,
    /// Unix epoch milliseconds (not a relative duration).
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "expiresAt")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "projectId")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "providerSpecificData")]
    pub provider_specific_data: Option<Value>,
}

// Debug must never disclose bearer tokens, refresh tokens or arbitrary metadata.
impl std::fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("has_access_token", &self.access_token.is_some())
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

impl OAuthCredentials {
    pub fn redact(&mut self) {
        self.access_token = self.access_token.as_ref().map(|_| "[redacted]".into());
        self.refresh_token = self.refresh_token.as_ref().map(|_| "[redacted]".into());
        self.id_token = self.id_token.as_ref().map(|_| "[redacted]".into());
        self.provider_specific_data = None;
    }

    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.provider_specific_data.as_ref()?.get(key)?.as_str().filter(|s| !s.trim().is_empty())
    }
}

pub fn is_subscription_provider(provider: &str) -> bool {
    matches!(provider, "codex" | "antigravity")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as u64
}

/// A lock belongs to an account, not a provider: Codex refresh tokens are
/// single-use. Re-read the persisted token *after* acquiring the lock.
#[derive(Default)]
pub struct OAuthRuntime {
    locks: dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl OAuthRuntime {
    pub async fn credentials(
        &self, state: &Arc<AppState>, id: &str, rejected_token: Option<&str>,
    ) -> Result<OAuthCredentials, ApiError> {
        let lock = self.locks.entry(id.to_string()).or_default().clone();
        let _guard = lock.lock().await;
        let conn = state.provider_connections.get(id)
            .ok_or_else(|| ApiError::new(401, "OAuth connection no longer exists"))?;
        let mut oauth = conn.oauth.clone();
        let token = oauth.access_token.as_deref().filter(|s| !s.is_empty());
        let rejected = rejected_token.is_some() && rejected_token == token;
        let expired = oauth.expires_at.is_some_and(|expiry| expiry <= now_ms().saturating_add(60_000));
        if token.is_some() && !expired && !rejected { return Ok(oauth); }
        let refresh = oauth.refresh_token.as_deref().filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::new(401, "OAuth token expired; reconnect this account"))?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh.to_string()),
        ];
        let endpoint = match conn.provider.as_str() {
            "codex" => {
                form.push(("client_id", public_client("CODEX_OAUTH_CLIENT_ID", CODEX_CLIENT)));
                "https://auth.openai.com/oauth/token"
            }
            "antigravity" => {
                let client_id = oauth.metadata_str("oauthClientId").map(str::to_string)
                    .unwrap_or_else(|| public_client("ANTIGRAVITY_OAUTH_CLIENT_ID", ANTIGRAVITY_CLIENT));
                let client_secret = oauth.metadata_str("oauthClientSecret").map(str::to_string)
                    .unwrap_or_else(|| public_client("ANTIGRAVITY_OAUTH_CLIENT_SECRET", ANTIGRAVITY_SECRET));
                form.push(("client_id", client_id));
                form.push(("client_secret", client_secret));
                "https://oauth2.googleapis.com/token"
            }
            _ => return Err(ApiError::new(400, "Provider does not support OAuth refresh")),
        };
        // Only explicitly configured account metadata can replace a token URL;
        // request bodies cannot redirect refresh credentials.
        let endpoint = oauth.metadata_str("tokenUrl").unwrap_or(endpoint);
        let response = state.upstream.oauth_client(conn.proxy_url.as_deref())?
            .post(endpoint).header("accept", "application/json").form(&form).send().await
            .map_err(|_| ApiError::new(502, "OAuth token endpoint unavailable"))?;
        let status = response.status();
        if !status.is_success() {
            // Do not surface the server body: it may echo refresh credentials.
            return Err(ApiError::new(if status.as_u16() == 429 { 429 } else if status.is_server_error() { 502 } else { 401 },
                "OAuth refresh rejected; reconnect the account if the problem persists"));
        }
        let tokens: Value = response.json().await
            .map_err(|_| ApiError::new(502, "Invalid OAuth token response"))?;
        oauth.access_token = Some(tokens.get("access_token").and_then(Value::as_str)
            .filter(|s| !s.is_empty()).ok_or_else(|| ApiError::new(502, "OAuth response missing access token"))?.to_string());
        if let Some(refresh) = tokens.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            oauth.refresh_token = Some(refresh.to_string());
        }
        if let Some(id_token) = tokens.get("id_token").and_then(Value::as_str) { oauth.id_token = Some(id_token.to_string()); }
        oauth.expires_at = Some(now_ms().saturating_add(tokens.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_mul(1000)));
        if !state.provider_connections.update_oauth(id, oauth.clone()) {
            return Err(ApiError::new(500, "Cannot persist rotated OAuth credentials"));
        }
        Ok(oauth)
    }
}

// Public installed-client credentials, NOT user/account secrets. The byte
// encoding mirrors npm OmniRoute's publicCreds.ts; environment overrides allow
// deployment-owned OAuth clients. This is obfuscation, not encryption.
fn public_client(env: &str, bytes: &[u8]) -> String {
    std::env::var(env).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        bytes.iter().enumerate().map(|(i, b)| (b ^ PUBLIC_MASK[i % PUBLIC_MASK.len()]) as char).collect()
    })
}
const PUBLIC_MASK: &[u8] = b"omniroute-public-v1";
const CODEX_CLIENT: &[u8] = &[14, 29, 30, 54, 55, 34, 26, 21, 8, 104, 53, 47, 85, 95, 15, 83, 110, 29, 105, 14, 53, 30, 94,
    26, 29, 20, 26, 11,];
const ANTIGRAVITY_CLIENT: &[u8] = &[94, 93, 89, 88, 66, 95, 67, 68, 83, 29, 69, 76, 83, 65, 29, 14, 69, 5, 66, 6, 3, 92, 1, 64, 94,
    25, 23, 23, 72, 66, 70, 87, 26, 29, 12, 65, 25, 91, 7, 89, 9, 93, 66, 92, 16, 4, 75, 76, 0, 5,
    17, 66, 14, 12, 66, 17, 93, 10, 24, 29, 12, 0, 12, 26, 26, 17, 72, 30, 1, 76, 15, 6, 14,];
const ANTIGRAVITY_SECRET: &[u8] = &[40, 34, 45, 58, 34, 55, 88, 63, 80, 21, 54, 34, 48, 88, 81, 85, 97, 18, 125, 37, 92, 3, 37, 48,
    87, 6, 44, 38, 25, 10, 67, 19, 40, 40, 5,];
