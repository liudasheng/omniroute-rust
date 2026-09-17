//! Upstream HTTP execution (parity: `open-sse/executors/base.ts` +
//! provider-specific URL/header builders in `open-sse/config/providers/shared.ts`).

use crate::config::Config;
use crate::errors::ApiError;
use crate::registry::{AuthHeader, Format, Registry, RegistryEntry};
use bytes::Bytes;
use reqwest::{Client, Method, Response};
use serde_json::Value;
use std::time::Duration;

pub struct UpstreamClient {
    http: Client,
}

impl UpstreamClient {
    pub fn new(config: &Config) -> Self {
        let http = Client::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .expect("reqwest client");
        Self { http }
    }

    pub async fn execute(
        &self,
        url: &str,
        method: Method,
        headers: Vec<(String, String)>,
        body: Option<Bytes>,
        timeout_ms: u64,
    ) -> Result<Response, reqwest::Error> {
        let mut req = self.http.request(method, url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        req.timeout(Duration::from_millis(timeout_ms)).send().await
    }
}

/// Resolve the upstream URL + auth headers for (provider, model, stream).
pub fn build_upstream_request(
    cfg: &Config,
    reg: &Registry,
    entry: &RegistryEntry,
    provider: &str,
    model: &str,
    stream: bool,
    key_override: Option<String>,
    base_override: Option<String>,
) -> Result<(String, Vec<(String, String)>), ApiError> {
    // Managed dashboard connections (key/base stored per provider) win over
    // static config; blanks never shadow the registry defaults.
    let base = base_override
        .filter(|b| !b.trim().is_empty())
        .or_else(|| cfg.base_url_for(reg, provider))
        .ok_or_else(|| {
            ApiError::new(500, format!("no upstream configured for provider '{provider}'"))
        })?;
    let base = base.trim_end_matches('/');

    let key = key_override
        .filter(|k| !k.is_empty())
        .or_else(|| cfg.api_key_for(provider));

    let mut headers: Vec<(String, String)> = Vec::new();
    match entry.format {
        Format::Claude => {
            headers.push(("content-type".into(), "application/json".into()));
            headers.push(("accept".into(), "application/json".into()));
            if let Some(k) = key {
                match entry.auth_header {
                    AuthHeader::XApiKey => headers.push(("x-api-key".into(), k)),
                    AuthHeader::Bearer => headers.push(("authorization".into(), format!("Bearer {k}"))),
                    _ => headers.push(("authorization".into(), format!("Bearer {k}"))),
                }
            }
            for (k, v) in &entry.extra_headers {
                headers.push((k.clone(), v.clone()));
            }
            let path = entry.chat_path.clone().unwrap_or_else(|| "/messages".to_string());
            let mut url = format!("{base}{path}");
            if entry.url_suffix.is_empty() {
                // ensure trailing beta for anthropic-style providers handled by suffix
            } else {
                url.push_str(if url.contains('?') { "&" } else { "?" });
                url.push_str(entry.url_suffix.trim_start_matches('?'));
            }
            if stream {
                url.push_str(if url.contains('?') { "&" } else { "?" });
                url.push_str("stream=true");
            }
            Ok((url, headers))
        }
        Format::Gemini => {
            headers.push(("content-type".into(), "application/json".into()));
            if let Some(k) = key {
                headers.push(("x-goog-api-key".into(), k));
            }
            for (k, v) in &entry.extra_headers {
                headers.push((k.clone(), v.clone()));
            }
            let action = if stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
            let url = format!("{base}/models/{model}:{action}");
            Ok((url, headers))
        }
        Format::OpenAI | Format::OpenAIResponses => {
            headers.push(("content-type".into(), "application/json".into()));
            if let Some(k) = key {
                if entry.auth_header == AuthHeader::Bearer || entry.auth_header == AuthHeader::None {
                    headers.push(("authorization".into(), format!("Bearer {k}")));
                } else {
                    headers.push(("x-api-key".into(), k));
                }
            }
            for (k, v) in &entry.extra_headers {
                headers.push((k.clone(), v.clone()));
            }
            if entry.format == Format::OpenAIResponses {
                let path = entry.chat_path.clone().unwrap_or_else(|| "/responses".to_string());
                Ok((format!("{base}{path}"), headers))
            } else {
                let path = entry.chat_path.clone().unwrap_or_else(|| "/chat/completions".to_string());
                Ok((format!("{base}{path}"), headers))
            }
        }
    }
}

/// Extract status/body/error info from an upstream response into an ApiError.
pub fn upstream_error(status: u16, body: Option<Value>) -> ApiError {
    let message = body
        .as_ref()
        .and_then(|b| {
            b.pointer("/error/message")
                .and_then(|m| m.as_str())
                .or_else(|| b.get("message").and_then(|m| m.as_str()))
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("upstream error {status}"));
    ApiError::new(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::static_registry;

    fn cfg() -> Config {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("OMNIROUTE_DATA_DIR", dir.path().as_os_str()); }
        let mut c = Config::load(None).unwrap();
        unsafe { std::env::remove_var("OMNIROUTE_DATA_DIR"); }
        c.credentials.insert("anthropic".into(), crate::config::ProviderCredentials { api_key: Some("sk-ant".into()), ..Default::default() });
        c.credentials.insert("openai".into(), crate::config::ProviderCredentials { api_key: Some("sk-oai".into()), ..Default::default() });
        c.credentials.insert("gemini".into(), crate::config::ProviderCredentials { api_key: Some("g-key".into()), ..Default::default() });
        c
    }

    #[test]
    fn openai_url_and_bearer() {
        let c = cfg();
        let reg = Registry::new(static_registry());
        let e = reg.get("openai").unwrap();
        let (url, headers) = build_upstream_request(&c, &reg, &e, "openai", "gpt-4o", false, None, None).unwrap();
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        assert!(headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer sk-oai"));
    }

    #[test]
    fn anthropic_url_suffix_and_headers() {
        let c = cfg();
        let reg = Registry::new(static_registry());
        let e = reg.get("anthropic").unwrap();
        let (url, headers) = build_upstream_request(&c, &reg, &e, "anthropic", "claude-sonnet-4-5", true, None, None).unwrap();
        assert!(url.starts_with("https://api.anthropic.com/v1/messages"));
        assert!(url.contains("beta=true"));
        assert!(url.contains("stream=true"));
        assert!(headers.iter().any(|(k, v)| k == "x-api-key" && v == "sk-ant"));
        assert!(headers.iter().any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
    }

    #[test]
    fn gemini_url_shape() {
        let c = cfg();
        let reg = Registry::new(static_registry());
        let e = reg.get("gemini").unwrap();
        let (url, headers) = build_upstream_request(&c, &reg, &e, "gemini", "gemini-2.5-flash", true, None, None).unwrap();
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert!(headers.iter().any(|(k, v)| k == "x-goog-api-key" && v == "g-key"));

        let (url, _) = build_upstream_request(&c, &reg, &e, "gemini", "gemini-2.5-flash", false, None, None).unwrap();
        assert!(url.ends_with(":generateContent"));
    }

    #[test]
    fn managed_overrides_win_and_blanks_fall_through() {
        let c = cfg();
        let reg = Registry::new(static_registry());
        let e = reg.get("openai").unwrap();
        // managed key/base replace the static config
        let (url, headers) = build_upstream_request(
            &c, &reg, &e, "openai", "gpt-4o", false,
            Some("sk-managed".into()), Some("https://proxy.local/v1/".into()),
        )
        .unwrap();
        assert_eq!(url, "https://proxy.local/v1/chat/completions");
        assert!(headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer sk-managed"));
        // blank overrides never shadow the registry defaults
        let (url, headers) = build_upstream_request(
            &c, &reg, &e, "openai", "gpt-4o", false,
            Some("".into()), Some("  ".into()),
        )
        .unwrap();
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        assert!(headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer sk-oai"));
    }
}
