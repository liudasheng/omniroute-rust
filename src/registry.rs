//! Static provider registry (parity with `open-sse/config/providerRegistry.ts`
//! + `open-sse/config/providers/registry/<id>/index.ts`) and the dynamic
//! `openai-compatible-*` / `anthropic-compatible-*` provider families from
//! `open-sse/services/provider.ts`.

use std::collections::HashMap;
use std::sync::Arc;

/// Wire format spoken by the provider upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    OpenAI,
    OpenAIResponses,
    Claude,
    Gemini,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::OpenAI => "openai",
            Format::OpenAIResponses => "openai-responses",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
        }
    }
}

/// Where the credential goes (parity with `authHeader` variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthHeader {
    Bearer,
    XApiKey,
    Key,
    XGoogApiKey,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    Oauth,
    ApiKey,
    Local,
}

#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: String,
    pub aliases: Vec<String>,
    pub format: Format,
    pub base_url: String,
    pub auth_header: AuthHeader,
    pub auth_type: AuthType,
    pub is_local: bool,
    /// Force SSE towards the upstream even when the client asked for JSON
    /// (parity: kimi `forceStream:true`); the gateway aggregates back to JSON.
    pub force_stream: bool,
    /// URL suffix appended, e.g. anthropic `?beta=true`.
    pub url_suffix: &'static str,
    /// Extra static headers (e.g. anthropic-version, HTTP-Referer).
    pub extra_headers: Vec<(String, String)>,
    /// Seed models for the catalog.
    pub default_models: Vec<String>,
    /// Override the chat path instead of the format default.
    pub chat_path: Option<String>,
    /// Strip the anthropic-compatible path quirk (cc- family).
    pub is_cc: bool,
}

fn entry(id: &str, format: Format, base_url: &str, auth_header: AuthHeader, models: &[&str]) -> RegistryEntry {
    RegistryEntry {
        id: id.to_string(),
        aliases: vec![],
        format,
        base_url: base_url.to_string(),
        auth_header,
        auth_type: AuthType::ApiKey,
        is_local: false,
        force_stream: false,
        url_suffix: "",
        extra_headers: Vec::new(),
        default_models: models.iter().map(|s| s.to_string()).collect(),
        chat_path: None,
        is_cc: false,
    }
}

/// `isLocalProvider()` parity: localhost / 127.x / private nets / docker host.
pub fn is_local_hostname(url: &str) -> bool {
    let host = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    matches!(
        host,
        "localhost" | "host.docker.internal" | "172.17.0.1" | "172.17.0.2"
    ) || host.starts_with("127.")
        || host.starts_with("192.168.")
        || host.starts_with("10.")
        || host == "localhost"
}

/// Build the static registry table (the ~20 highest-value providers of the
/// original 240-entry registry; remaining providers are reachable through the
/// dynamic `openai-compatible-*` / `anthropic-compatible-*` families).
pub fn static_registry() -> Vec<RegistryEntry> {
    let mut v = Vec::new();

    let mut e = entry("anthropic", Format::Claude, "https://api.anthropic.com/v1", AuthHeader::XApiKey,
        &["claude-sonnet-4-5-20250929", "claude-opus-4-1-20250529", "claude-haiku-4-5-20251001", "claude-3-5-haiku-20241022"]);
    e.url_suffix = "?beta=true";
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.aliases = vec!["claude".into(), "anthropic".into()];
    v.push(e);

    let mut e = entry("openai", Format::OpenAI, "https://api.openai.com/v1", AuthHeader::Bearer,
        &["gpt-5.2", "gpt-5.1", "gpt-5", "gpt-4.1", "gpt-4o", "gpt-4o-mini", "o3", "o4-mini"]);
    e.auth_type = AuthType::Oauth;
    e.aliases = vec!["gpt".into(), "openai".into()];
    v.push(e);

    let mut e = entry("gemini", Format::Gemini, "https://generativelanguage.googleapis.com/v1beta", AuthHeader::XGoogApiKey,
        &["gemini-3-pro-preview", "gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite"]);
    e.aliases = vec!["google".into(), "bard".into()];
    v.push(e);

    let mut e = entry("glm", Format::OpenAI, "https://api.z.ai/api/coding/paas/v4", AuthHeader::Bearer,
        &["glm-4.6", "glm-4.5", "glm-4.5-air", "glm-4.5-flash"]);
    e.aliases = vec!["zhipu".into(), "bigmodel".into()];
    v.push(e);

    let mut e = entry("zai", Format::Claude, "https://api.z.ai/api/anthropic/v1", AuthHeader::XApiKey,
        &["glm-4.6", "glm-4.5"]);
    e.url_suffix = "?beta=true";
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.aliases = vec!["z.ai".into()];
    v.push(e);

    let mut e = entry("kimi", Format::OpenAI, "https://api.moonshot.ai/v1", AuthHeader::Bearer,
        &["kimi-k2-thinking", "kimi-k2-0905-preview", "kimi-latest"]);
    e.force_stream = true;
    e.aliases = vec!["moonshot".into()];
    v.push(e);

    v.push(entry("deepseek", Format::OpenAI, "https://api.deepseek.com/v1", AuthHeader::Bearer,
        &["deepseek-chat", "deepseek-reasoner"]));

    let mut e = entry("openrouter", Format::OpenAI, "https://openrouter.ai/api/v1", AuthHeader::Bearer, &[]);
    e.extra_headers.push(("HTTP-Referer".into(), "https://github.com/diegosouzapw/OmniRoute".into()));
    e.extra_headers.push(("X-Title".into(), "OmniRoute".into()));
    v.push(e);

    v.push(entry("groq", Format::OpenAI, "https://api.groq.com/openai/v1", AuthHeader::Bearer,
        &["llama-3.3-70b-versatile", "llama-3.1-8b-instant", "qwen-2.5-32b"]));

    v.push(entry("xai", Format::OpenAI, "https://api.x.ai/v1", AuthHeader::Bearer,
        &["grok-4", "grok-3", "grok-3-mini"]));

    v.push(entry("mistral", Format::OpenAI, "https://api.mistral.ai/v1", AuthHeader::Bearer,
        &["mistral-large-latest", "mistral-small-latest", "codestral-latest"]));

    v.push(entry("together", Format::OpenAI, "https://api.together.xyz/v1", AuthHeader::Bearer, &[]));

    v.push(entry("fireworks", Format::OpenAI, "https://api.fireworks.ai/inference/v1", AuthHeader::Bearer, &[]));

    v.push(entry("perplexity", Format::OpenAI, "https://api.perplexity.ai", AuthHeader::Bearer,
        &["sonar", "sonar-pro", "sonar-reasoning"]));

    v.push(entry("minimax", Format::OpenAI, "https://api.minimax.chat/v1", AuthHeader::Bearer,
        &["MiniMax-Text-01", "MiniMax-M1"]));

    v.push(entry("siliconflow", Format::OpenAI, "https://api.siliconflow.cn/v1", AuthHeader::Bearer, &[]));

    v.push(entry("dashscope", Format::OpenAI, "https://dashscope.aliyuncs.com/compatible-mode/v1", AuthHeader::Bearer,
        &["qwen-max", "qwen-plus", "qwen-turbo"]));
    v.last_mut().unwrap().aliases = vec!["qwen".into(), "aliyun".into()];

    v.push(entry("doubao", Format::OpenAI, "https://ark.cn-beijing.volces.com/api/v3", AuthHeader::Bearer, &[]));
    v.last_mut().unwrap().aliases = vec!["volcengine".into(), "ark".into()];

    let mut e = entry("ollama", Format::OpenAI, "http://127.0.0.1:11434/v1", AuthHeader::None, &[]);
    e.is_local = true;
    e.auth_type = AuthType::Local;
    v.push(e);

    let mut e = entry("lmstudio", Format::OpenAI, "http://127.0.0.1:1234/v1", AuthHeader::None, &[]);
    e.is_local = true;
    e.auth_type = AuthType::Local;
    v.push(e);

    let mut e = entry("ollama-cloud", Format::OpenAI, "https://ollama.com/v1", AuthHeader::Bearer,
        &["gpt-oss:120b", "qwen3-coder:480b", "deepseek-v3.1:671b"]);
    e.extra_headers.push(("Authorization".into(), "Bearer".into()));
    v.push(e);

    v
}

/// alias → canonical id table (parity: `PROVIDER_ID_TO_ALIAS`).
pub fn provider_aliases() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("anthropic", "anthropic");
    m.insert("claude", "anthropic");
    m.insert("openai", "openai");
    m.insert("gpt", "openai");
    m.insert("chatgpt", "openai");
    m.insert("gemini", "gemini");
    m.insert("google", "gemini");
    m.insert("bard", "gemini");
    m.insert("glm", "glm");
    m.insert("zhipu", "glm");
    m.insert("bigmodel", "glm");
    m.insert("zai", "zai");
    m.insert("z.ai", "zai");
    m.insert("kimi", "kimi");
    m.insert("moonshot", "kimi");
    m.insert("deepseek", "deepseek");
    m.insert("openrouter", "openrouter");
    m.insert("groq", "groq");
    m.insert("xai", "xai");
    m.insert("grok", "xai");
    m.insert("mistral", "mistral");
    m.insert("together", "together");
    m.insert("fireworks", "fireworks");
    m.insert("perplexity", "perplexity");
    m.insert("perplexityai", "perplexity");
    m.insert("minimax", "minimax");
    m.insert("siliconflow", "siliconflow");
    m.insert("dashscope", "dashscope");
    m.insert("qwen", "dashscope");
    m.insert("aliyun", "dashscope");
    m.insert("doubao", "doubao");
    m.insert("volcengine", "doubao");
    m.insert("ark", "doubao");
    m.insert("ollama", "ollama");
    m.insert("lmstudio", "lmstudio");
    m.insert("ollama-cloud", "ollama-cloud");
    m
}

/// Resolve a provider token (id or alias) to canonical id.
pub fn resolve_provider_alias(token: &str) -> Option<&'static str> {
    provider_aliases().get(token).copied()
}

/// The live registry: static entries + dynamic compatible families built from
/// credentials (`openai-compatible-<name>`, `anthropic-compatible-<name>`,
/// `anthropic-compatible-cc-<name>`).
#[derive(Clone, Default)]
pub struct Registry {
    entries: HashMap<String, Arc<RegistryEntry>>,
}

impl Registry {
    pub fn new(statics: Vec<RegistryEntry>) -> Self {
        let mut entries = HashMap::new();
        for e in statics {
            entries.insert(e.id.clone(), Arc::new(e));
        }
        Self { entries }
    }

    /// Register a dynamic compatible provider from credentials.
    pub fn register_dynamic(
        &mut self,
        id: &str,
        base_url: Option<String>,
        api_type: Option<String>,
        models: Vec<String>,
    ) {
        if self.entries.contains_key(id) {
            return;
        }
        let (format, default_base, is_cc) = if let Some(rest) = id.strip_prefix("anthropic-compatible-cc-") {
            let _ = rest;
            (Format::Claude, "https://api.anthropic.com/v1", true)
        } else if id.starts_with("anthropic-compatible-") {
            (Format::Claude, "https://api.anthropic.com/v1", false)
        } else if let Some(rest) = id.strip_prefix("openai-compatible-") {
            let _ = rest;
            (Format::OpenAI, "https://api.openai.com/v1", false)
        } else {
            return;
        };
        let api_type = api_type.unwrap_or_default();
        let format = if format == Format::OpenAI {
            match api_type.as_str() {
                "openai-responses" => Format::OpenAIResponses,
                _ => Format::OpenAI,
            }
        } else {
            format
        };
        let base = base_url.clone().unwrap_or_else(|| default_base.to_string());
        let mut e = RegistryEntry {
            id: id.to_string(),
            aliases: vec![],
            format,
            base_url: base.clone(),
            auth_header: if format == Format::Claude { AuthHeader::XApiKey } else { AuthHeader::Bearer },
            auth_type: AuthType::ApiKey,
            is_local: is_local_hostname(&base),
            force_stream: false,
            url_suffix: "",
            extra_headers: Vec::new(),
            default_models: models,
            chat_path: None,
            is_cc,
        };
        if is_cc {
            e.chat_path = Some("/chat_completion".to_string());
            e.url_suffix = "?beta=true";
            e.extra_headers.push(("anthropic-beta".into(), "claude-code-20250219".into()));
        }
        self.entries.insert(id.to_string(), Arc::new(e));
    }

    pub fn get(&self, id: &str) -> Option<Arc<RegistryEntry>> {
        self.entries.get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.entries.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn all(&self) -> Vec<Arc<RegistryEntry>> {
        let mut v: Vec<Arc<RegistryEntry>> = self.entries.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_resolution() {
        assert_eq!(resolve_provider_alias("claude"), Some("anthropic"));
        assert_eq!(resolve_provider_alias("gpt"), Some("openai"));
        assert_eq!(resolve_provider_alias("z.ai"), Some("zai"));
        assert_eq!(resolve_provider_alias("moonshot"), Some("kimi"));
        assert_eq!(resolve_provider_alias("qwen"), Some("dashscope"));
        assert_eq!(resolve_provider_alias("nonexistent"), None);
    }

    #[test]
    fn static_entries_have_expected_shape() {
        let reg = Registry::new(static_registry());
        let anthropic = reg.get("anthropic").unwrap();
        assert_eq!(anthropic.format, Format::Claude);
        assert_eq!(anthropic.auth_header, AuthHeader::XApiKey);
        assert_eq!(anthropic.url_suffix, "?beta=true");
        assert_eq!(anthropic.extra_headers[0].0, "anthropic-version");

        let kimi = reg.get("kimi").unwrap();
        assert!(kimi.force_stream);

        let ollama = reg.get("ollama").unwrap();
        assert!(ollama.is_local);
        assert_eq!(ollama.auth_header, AuthHeader::None);

        assert!(is_local_hostname("http://127.0.0.1:8080/v1"));
        assert!(is_local_hostname("http://192.168.1.5:8080"));
        assert!(!is_local_hostname("https://api.openai.com/v1"));
    }

    #[test]
    fn dynamic_families() {
        let mut reg = Registry::new(static_registry());
        reg.register_dynamic(
            "openai-compatible-deepinfra",
            Some("https://api.deepinfra.com/v1/openai".into()),
            None,
            vec!["meta-llama/Meta-Llama-3.1-70B".into()],
        );
        reg.register_dynamic("anthropic-compatible-proxy", None, None, vec![]);
        reg.register_dynamic("anthropic-compatible-cc-mycc", None, None, vec![]);
        let d = reg.get("openai-compatible-deepinfra").unwrap();
        assert_eq!(d.format, Format::OpenAI);
        assert_eq!(d.base_url, "https://api.deepinfra.com/v1/openai");
        let a = reg.get("anthropic-compatible-proxy").unwrap();
        assert_eq!(a.format, Format::Claude);
        assert_eq!(a.auth_header, AuthHeader::XApiKey);
        let cc = reg.get("anthropic-compatible-cc-mycc").unwrap();
        assert!(cc.is_cc);
        assert_eq!(cc.chat_path.as_deref(), Some("/chat_completion"));
        assert_eq!(cc.url_suffix, "?beta=true");
    }
}
