//! Static provider registry (parity with `open-sse/config/providerRegistry.ts`
//! and the per-provider registry directories) plus the dynamic
//! `openai-compatible-*` / `anthropic-compatible-*` provider families
//! from `open-sse/services/provider.ts`.

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

/// Build the static registry table (146 plain-HTTP API-key providers extracted
/// from the original ~240-entry registry; the rest need oauth/cookie/web
/// executors, stdio/websocket transports, custom key headers or non-HTTP
/// formats, plus the dynamic `openai-compatible-*` /
/// `anthropic-compatible-*` families for the long tail).
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

    let mut e = entry("glm", Format::OpenAI, "https://api.z.ai/api/coding/paas/v4/chat/completions", AuthHeader::Bearer,
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

    // NB: OpenRouter only accepts its `auto` router or provider-prefixed slugs —
    // a bare id (e.g. gpt-4o-mini) is a 404, so the probe default must be one.
    let mut e = entry("openrouter", Format::OpenAI, "https://openrouter.ai/api/v1", AuthHeader::Bearer, &["auto"]);
    e.extra_headers.push(("HTTP-Referer".into(), "https://github.com/diegosouzapw/OmniRoute".into()));
    e.extra_headers.push(("X-Title".into(), "OmniRoute".into()));
    v.push(e);

    // OpenCode tiers (parity: open-sse/config/providers/registry/opencode/*):
    // openai format + Bearer against the zen/go bases.
    v.push(entry("opencode", Format::OpenAI, "https://opencode.ai/zen/v1", AuthHeader::Bearer,
        &["big-pickle", "muse-spark-1.2", "muse-spark-1.2-contributor-free", "muse-spark-1.3", "muse-spark-1.3-contributor-free", "deepseek-v4-flash-free", "mimo-v2.5-free", "hy3-free", "nemotron-3-ultra-free", "north-mini-code-free"]));
    v.push(entry("opencode-zen", Format::OpenAI, "https://opencode.ai/zen/v1", AuthHeader::Bearer,
        &["big-pickle", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.4", "gpt-5.4-mini", "gpt-5.4-nano", "gpt-5.3-codex-spark", "gpt-5.1", "claude-fable-5", "claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5", "gemini-3.7-flash", "gemini-3.5-flash-lite", "gemini-3.1-pro", "gemini-3-flash", "grok-build-0.1", "grok-4.6", "muse-spark-1.2", "muse-spark-1.2-contributor-free", "muse-spark-1.3", "muse-spark-1.3-contributor-free", "deepseek-v4-pro", "deepseek-v4-flash", "glm-5.2", "minimax-m3", "kimi-k3", "deepseek-v4-flash-free", "mimo-v2.5-free", "hy3-free", "nemotron-3-ultra-free", "nemotron-3.5-lightning-free", "laguna-s-2.1-free"]));
    v.push(entry("opencode-go", Format::OpenAI, "https://opencode.ai/zen/go/v1", AuthHeader::Bearer,
        &["glm-5.2", "glm-5.2-high", "glm-5.2-max", "glm-5.1", "glm-5", "kimi-k2.6", "kimi-k2.5", "kimi-k3", "kimi-k3-max", "mimo-v2.5-pro", "mimo-v2.5", "mimo-v2.5-high", "mimo-v2.5-max", "minimax-m3", "minimax-m2.7", "minimax-m2.5", "qwen3.7-max", "qwen3.7-max-high", "qwen3.7-max-max", "qwen3.7-plus", "qwen3.7-plus-high", "qwen3.7-plus-max", "qwen3.6-plus-high", "qwen3.6-plus-max", "hy3", "hy3-none", "hy3-low", "hy3-high", "hy3-preview", "muse-spark-1.2-contributor", "muse-spark-1.2-contributor-minimal", "muse-spark-1.2-contributor-low", "muse-spark-1.2-contributor-medium", "muse-spark-1.2-contributor-high", "muse-spark-1.2-contributor-xhigh", "muse-spark-1.3-contributor", "muse-spark-1.3-contributor-minimal", "muse-spark-1.3-contributor-low", "muse-spark-1.3-contributor-medium", "muse-spark-1.3-contributor-high", "muse-spark-1.3-contributor-xhigh", "grok-4.5", "grok-4.5-low", "grok-4.5-medium", "grok-4.5-high", "deepseek-v4-pro", "deepseek-v4-flash", "gpt-5.6-luna", "ox-alpha-free"]));

    v.push(entry("groq", Format::OpenAI, "https://api.groq.com/openai/v1", AuthHeader::Bearer,
        &["llama-3.3-70b-versatile", "llama-3.1-8b-instant", "qwen-2.5-32b"]));

    v.push(entry("xai", Format::OpenAI, "https://api.x.ai/v1", AuthHeader::Bearer,
        &["grok-4", "grok-3", "grok-3-mini"]));

    // Bulk-extracted API-key providers (parity: open-sse/config/providers/registry/*).
    // Included only when the entry is plain HTTP: format openai/openai-responses/claude/gemini,
    // executor default (plain) or openai-compatible, authType apikey/optional/none, a baseUrl,
    // and a representable key header (bearer/authorization/x-api-key/none). Deliberately
    // excluded: oauth/cookie/web executors, stdio/websocket bases, custom key headers
    // (oneminai/ideogram), and non-HTTP formats (antigravity/cursor/kiro/clova/magnific-image/custom).
    let mut e = entry("agentrouter", Format::Claude, "https://agentrouter.org/v1/messages", AuthHeader::XApiKey, &["claude-opus-4-8", "claude-opus-5", "gpt-5.6-sol"]);
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.chat_path = Some("".into());
    v.push(e);
    v.push(entry("agnes", Format::OpenAI, "https://apihub.agnes-ai.com/v1/chat/completions", AuthHeader::Bearer, &["agnes-2.0-flash", "agnes-2.5-flash"]));
    v.push(entry("ai21", Format::OpenAI, "https://api.ai21.com/studio/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("aimlapi", Format::OpenAI, "https://api.aimlapi.com/v1/chat/completions", AuthHeader::Bearer, &["gpt-4o", "claude-3-5-sonnet-20241022", "gemini-1.5-pro", "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo", "deepseek-chat", "mistral-large-latest"]);
    e.aliases = vec!["aiml".into()];
    v.push(e);
    let mut e = entry("alibaba", Format::OpenAI, "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["ali".into()];
    v.push(e);
    let mut e = entry("alibaba-cn", Format::OpenAI, "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["ali-cn".into()];
    v.push(e);
    let mut e = entry("ant-ling", Format::OpenAI, "https://api.ant-ling.com/v1/chat/completions", AuthHeader::Bearer, &["Ling-2.6-1T", "Ring-2.6-1T", "Ling-2.6-flash"]);
    e.aliases = vec!["ling".into()];
    v.push(e);
    let mut e = entry("api-airforce", Format::OpenAI, "https://api.airforce/v1/chat/completions", AuthHeader::Bearer, &["x-ai/grok-3", "x-ai/grok-2-1212", "anthropic/claude-3.7-sonnet", "qwen/qwen3-32b", "moonshot/kimi-k2.6", "google/gemini-2.5-flash", "deepseek/deepseek-v3"]);
    e.aliases = vec!["af".into()];
    v.push(e);
    v.push(entry("bai", Format::OpenAI, "https://api.b.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("baichuan", Format::OpenAI, "https://api.baichuan-ai.com/v1/chat/completions", AuthHeader::Bearer, &["Baichuan4-Turbo", "Baichuan4-Air", "Baichuan4", "Baichuan3-Turbo", "Baichuan3-Turbo-128k"]));
    v.push(entry("baidu", Format::OpenAI, "https://qianfan.baidubce.com/v2/chat/completions", AuthHeader::Bearer, &["ernie-5.1", "ernie-5.0", "ernie-x1.1", "ernie-4.5-turbo-128k", "ernie-4.5-turbo-32k", "ernie-4.5-turbo-vl", "ernie-4.5-21b-a3b", "ernie-4.5-0.3b", "ernie-4.0-8k", "ernie-4.0-turbo-128k", "ernie-4.0-turbo-8k", "ernie-3.5-8k", "ernie-speed-128k", "ernie-speed-8k", "ernie-lite-8k", "ernie-tiny-8k"]));
    let mut e = entry("bailian-coding-plan", Format::Claude, "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic/v1", AuthHeader::XApiKey, &[]);
    e.aliases = vec!["bcp".into()];
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.chat_path = Some("/messages".into());
    v.push(e);
    v.push(entry("baseten", Format::OpenAI, "https://inference.baseten.co/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("bazaarlink", Format::OpenAI, "https://bazaarlink.ai/api/v1/chat/completions", AuthHeader::Bearer, &["auto:free", "claude-opus-4.7", "claude-sonnet-4.6", "claude-haiku-4.5", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.4-nano", "grok-4.3", "grok-4.20", "gemini-3.1-pro-preview", "gemini-3-flash-preview", "gemini-3.1-flash-lite-preview", "gemma-4-31b-it", "gemma-4-26b-a4b-it", "deepseek-v3.2", "kimi-k2.6", "kimi-k2.5", "glm-5.1", "glm-5", "mimo-v2.5-pro", "mimo-v2.5", "minimax-m3", "minimax-m2.7", "minimax-m2.5", "llama-4-maverick", "llama-4-scout", "llama-3.3-70b-instruct", "qwen3.6-plus", "mistral-large-2512", "mistral-medium-3.1", "mistral-small-2603", "nemotron-3-super-120b-a12b"]);
    e.aliases = vec!["bzl".into()];
    v.push(e);
    let mut e = entry("blackbox", Format::OpenAI, "https://api.blackbox.ai/v1/chat/completions", AuthHeader::Bearer, &["claude-fable-5", "claude-opus-4.8", "claude-sonnet-5", "claude-sonnet-4.6", "gpt-5.5", "gpt-5.4-pro", "gpt-5.4", "gpt-5.3-codex", "gpt-5.4-nano", "deepseek-v4-flash", "grok-4.3"]);
    e.aliases = vec!["bb".into()];
    v.push(e);
    let mut e = entry("bluesminds", Format::OpenAI, "https://api.bluesminds.com/v1/chat/completions", AuthHeader::Bearer, &["gpt-4o", "gpt-4o-mini", "gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano", "claude-sonnet-4-5", "claude-haiku-4-5", "gemini-2.0-flash", "gemini-2.0-flash-exp", "deepseek-reasoner", "deepseek-chat", "qwen-plus", "qwen-turbo", "kimi-k2", "kimi-k2-thinking", "glm-4.7", "glm-4-flash", "minimax-m2.5", "claude-opus-4-5", "gemini-2.5-pro", "grok-3", "qwen-max"]);
    e.aliases = vec!["bm".into()];
    v.push(e);
    let mut e = entry("byteplus", Format::OpenAI, "https://ark.ap-southeast.bytepluses.com/api/v3/chat/completions", AuthHeader::Bearer, &["seed-2.0", "kimi-k2-thinking", "glm-4.7", "gpt-oss-120b"]);
    e.aliases = vec!["bpm".into()];
    v.push(e);
    v.push(entry("bytez", Format::OpenAI, "https://api.bytez.com/models/v2/openai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("cerebras", Format::OpenAI, "https://api.cerebras.ai/v1/chat/completions", AuthHeader::Bearer, &["zai-glm-4.7", "gemma-4-31b", "gpt-oss-120b"]));
    v.push(entry("charm-hyper", Format::OpenAI, "https://hyper.charm.land/v1/chat/completions", AuthHeader::Bearer, &["hyper/auto"]));
    v.push(entry("chenzk", Format::OpenAI, "https://chenzk.top/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("chutes", Format::OpenAI, "https://llm.chutes.ai/v1/chat/completions", AuthHeader::Bearer, &["Qwen2.5-72B-Instruct"]));
    v.push(entry("codestral", Format::OpenAI, "https://codestral.mistral.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("cohere", Format::OpenAI, "https://api.cohere.com/compatibility/v1/chat/completions", AuthHeader::Bearer, &["command-a-reasoning-08-2025", "command-a-vision-07-2025", "command-a-03-2025", "command-r7b-12-2024", "command-r-plus-08-2024", "command-r-08-2024"]));
    v.push(entry("coze", Format::OpenAI, "https://api.coze.com/v1/chat/completions", AuthHeader::Bearer, &["claude-3-7-sonnet-20250514"]));
    v.push(entry("crof", Format::OpenAI, "https://crof.ai/v1/chat/completions", AuthHeader::Bearer, &["deepseek-v4-pro", "deepseek-v4-flash", "deepseek-v4-flash-0731", "deepseek-v3.2", "kimi-k2.6", "kimi-k2.7-code", "kimi-k3", "kimi-k3-eco", "glm-5.1", "glm-5.2", "mimo-v2.5-pro", "gemma-4-31b-it", "qwen3.6-27b", "qwen3.5-397b-a17b", "qwen3.5-9b"]));
    v.push(entry("dahl", Format::OpenAI, "https://inference.dahl.global/v1/chat/completions", AuthHeader::Bearer, &["MiniMaxAI/MiniMax-M2.7", "moonshotai/Kimi-K2.6"]));
    v.push(entry("databricks", Format::OpenAI, "https://adb-0000000000000000.0.azuredatabricks.net/serving-endpoints", AuthHeader::Bearer, &[]));
    v.push(entry("deepinfra", Format::OpenAI, "https://api.deepinfra.com/v1/openai/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("dgrid", Format::OpenAI, "https://api.dgrid.ai/v1/chat/completions", AuthHeader::Bearer, &["dgridai/free"]));
    v.push(entry("dify", Format::OpenAI, "https://api.dify.ai", AuthHeader::Bearer, &["auto"]));
    let mut e = entry("dit", Format::OpenAI, "https://api.dit.ai/v1/chat/completions", AuthHeader::Bearer, &["gpt-5.4", "claude-sonnet-4-6"]);
    e.aliases = vec!["dai".into()];
    v.push(e);
    v.push(entry("factory", Format::OpenAI, "https://api.factory.ai/v1/chat/completions", AuthHeader::Bearer, &["auto"]));
    let mut e = entry("featherless-ai", Format::OpenAI, "https://api.featherless.ai/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["featherless".into()];
    v.push(e);
    let mut e = entry("freeaiapikey", Format::OpenAI, "https://api.freeaiapikey.com/v1/chat/completions", AuthHeader::Bearer, &["openai/gpt-4o", "openai/gpt-5.4", "openai/gpt-5.5", "openai/gpt-5.6-sol", "anthropic/claude-opus-4.6", "anthropic/claude-opus-4.7", "anthropic/claude-opus-4.8", "anthropic/claude-opus-5", "anthropic/claude-sonnet-4.6", "anthropic/claude-sonnet-5"]);
    e.aliases = vec!["faik".into()];
    v.push(e);
    let mut e = entry("freemodel-dev", Format::OpenAI, "https://api.freemodel.dev/v1/chat/completions", AuthHeader::Bearer, &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"]);
    e.aliases = vec!["fmd".into()];
    v.push(e);
    let mut e = entry("freetheai", Format::OpenAI, "https://api.freetheai.xyz/v1/chat/completions", AuthHeader::Bearer, &["gpt-4o-mini", "llama-3.3-70b-instruct", "deepseek-chat"]);
    e.aliases = vec!["fta".into()];
    v.push(e);
    let mut e = entry("friendliai", Format::OpenAI, "https://api.friendli.ai/serverless/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["friendli".into()];
    v.push(e);
    let mut e = entry("g4f-gemini", Format::OpenAI, "https://g4f.space/api/gemini/v1/chat/completions", AuthHeader::Bearer, &["models/gemini-2.5-flash", "models/gemini-2.5-pro"]);
    e.aliases = vec!["g4fgem".into()];
    v.push(e);
    let mut e = entry("g4f-groq", Format::OpenAI, "https://g4f.space/api/groq/v1/chat/completions", AuthHeader::Bearer, &["llama-3.3-70b-versatile", "llama-3.1-8b-instant"]);
    e.aliases = vec!["g4fgroq".into()];
    v.push(e);
    let mut e = entry("g4f-nvidia", Format::OpenAI, "https://g4f.space/api/nvidia/v1/chat/completions", AuthHeader::Bearer, &["nvidia/nemotron-3-nano-30b-a3b", "z-ai/glm-5.2", "minimaxai/minimax-m2.7"]);
    e.aliases = vec!["g4fnv".into()];
    v.push(e);
    let mut e = entry("g4f-ollama", Format::OpenAI, "https://g4f.space/api/ollama/v1/chat/completions", AuthHeader::Bearer, &["gemma3:4b"]);
    e.aliases = vec!["g4foll".into()];
    v.push(e);
    let mut e = entry("g4f-pollinations", Format::OpenAI, "https://g4f.space/api/pollinations/v1/chat/completions", AuthHeader::Bearer, &["openai", "openai-fast"]);
    e.aliases = vec!["g4fpol".into()];
    v.push(e);
    v.push(entry("galadriel", Format::OpenAI, "https://api.galadriel.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("gigachat", Format::OpenAI, "https://gigachat.devices.sberbank.ru/api/v1", AuthHeader::Bearer, &[]));
    let mut e = entry("gitlawb", Format::OpenAI, "https://opengateway.gitlawb.com/v1/xiaomi-mimo", AuthHeader::Bearer, &[]);
    e.aliases = vec!["glb".into()];
    v.push(e);
    let mut e = entry("gitlawb-gmi", Format::OpenAI, "https://opengateway.gitlawb.com/v1/gmi-cloud", AuthHeader::Bearer, &[]);
    e.aliases = vec!["glb-gmi".into()];
    v.push(e);
    v.push(entry("heroku", Format::OpenAI, "https://us.inference.heroku.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("huggingface", Format::OpenAI, "https://router.huggingface.co/v1/chat/completions", AuthHeader::Bearer, &["meta-llama/llama-3.1-8b-instruct", "meta-llama/llama-3.2-11b-instruct", "mistralai/mistral-7b-instruct", "google/gemma-2-9b-it", "Qwen/Qwen2.5-7B-Instruct", "deepseek-ai/DeepSeek-V3"]);
    e.aliases = vec!["hf".into()];
    v.push(e);
    let mut e = entry("hyperbolic", Format::OpenAI, "https://api.hyperbolic.xyz/v1/chat/completions", AuthHeader::Bearer, &["Qwen/QwQ-32B", "deepseek-ai/DeepSeek-R1", "deepseek-ai/DeepSeek-V3", "meta-llama/Llama-3.3-70B-Instruct", "meta-llama/Llama-3.2-3B-Instruct", "Qwen/Qwen2.5-72B-Instruct", "Qwen/Qwen2.5-Coder-32B-Instruct", "NousResearch/Hermes-3-Llama-3.1-70B"]);
    e.aliases = vec!["hyp".into()];
    v.push(e);
    v.push(entry("iflytek", Format::OpenAI, "https://spark-api-open.xf-yun.com/v1/chat/completions", AuthHeader::Bearer, &["4.0Ultra", "generalv3.5", "max-32k", "generalv3", "pro-128k", "lite"]));
    v.push(entry("inception", Format::OpenAI, "https://api.inceptionlabs.ai/v1/chat/completions", AuthHeader::Bearer, &["mercury-2"]));
    let mut e = entry("inference-net", Format::OpenAI, "https://api.inference.net/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["inet".into()];
    v.push(e);
    v.push(entry("internlm", Format::OpenAI, "https://chat.intern-ai.org.cn/api/v1/chat/completions", AuthHeader::Bearer, &["intern-s1-pro", "intern-s1", "intern-s1-mini", "internvl3.5-latest", "intern-latest"]));
    v.push(entry("kenari", Format::OpenAI, "https://kenari.id/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("kie", Format::OpenAI, "https://api.kie.ai/v1/chat/completions", AuthHeader::Bearer, &["claude-fable-5", "claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5", "gpt-5-6-sol", "gpt-5-6-terra", "gpt-5-6-luna", "gemini-3-1-pro", "gemini-3-7-flash", "grok-4-6"]));
    let mut e = entry("kilo-gateway", Format::OpenAI, "https://api.kilo.ai/api/gateway/chat/completions", AuthHeader::Bearer, &["kilo-auto/frontier", "kilo-auto/balanced", "kilo-auto/free", "nvidia/nemotron-3-super-120b-a12b:free", "minimax/minimax-m2.5:free", "arcee-ai/trinity-large-preview:free"]);
    e.aliases = vec!["kg".into()];
    v.push(e);
    let mut e = entry("lambda-ai", Format::OpenAI, "https://api.lambda.ai/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["lambda".into()];
    v.push(e);
    let mut e = entry("leonardo", Format::OpenAI, "https://cloud.leonardo.ai/api/rest/v1", AuthHeader::Bearer, &["phoenix", "sdxl"]);
    e.aliases = vec!["leo".into()];
    v.push(e);
    v.push(entry("liquid", Format::OpenAI, "https://inference.liquid.ai/v1/chat/completions", AuthHeader::Bearer, &["liquid-lfm-40b"]));
    v.push(entry("llamagate", Format::OpenAI, "https://llamagate.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("llm7", Format::OpenAI, "https://api.llm7.io/v1/chat/completions", AuthHeader::Bearer, &["gpt-4o-mini-2024-07-18", "gpt-4.1-nano-2025-04-14", "deepseek-r1-0528", "qwen2.5-coder-32b-instruct"]));
    let mut e = entry("longcat", Format::OpenAI, "https://api.longcat.chat/openai/v1/chat/completions", AuthHeader::Bearer, &["LongCat-2.0"]);
    e.aliases = vec!["lc".into()];
    v.push(e);
    let mut e = entry("meta-llama", Format::OpenAI, "https://api.llama.com/compat/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["meta".into()];
    v.push(e);
    v.push(entry("minimax-cn", Format::OpenAI, "https://api.minimaxi.com/v1/chat/completions", AuthHeader::Bearer, &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed", "MiniMax-M2.5", "MiniMax-M2.5-highspeed"]));
    v.push(entry("modal", Format::OpenAI, "https://api.modal.ai/v1/chat/completions", AuthHeader::Bearer, &["google/gemini-2.0-flash"]));
    let mut e = entry("modelscope", Format::OpenAI, "https://api-inference.modelscope.cn/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["ms".into()];
    v.push(e);
    let mut e = entry("monsterapi", Format::OpenAI, "https://api.monsterapi.ai/v1/chat/completions", AuthHeader::Bearer, &["meta-llama/Meta-Llama-3.1-8B-Instruct", "meta-llama/Llama-3.3-70B-Instruct"]);
    e.aliases = vec!["monster".into()];
    v.push(e);
    v.push(entry("morph", Format::OpenAI, "https://api.morphllm.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("naga-ac", Format::OpenAI, "https://api.naga.ac/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["naga".into()];
    v.push(e);
    v.push(entry("nanogpt", Format::OpenAI, "https://nano-gpt.com/api/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("nlpcloud", Format::OpenAI, "https://api.nlpcloud.io/v1/chat/completions", AuthHeader::Bearer, &["chatdolphin", "dolphin", "finetuned-llama-3-70b", "llama-3-1-405b", "llama-3-8b-instruct"]);
    e.aliases = vec!["nlpc".into()];
    v.push(e);
    let mut e = entry("nous-research", Format::OpenAI, "https://inference-api.nousresearch.com/v1/chat/completions", AuthHeader::Bearer, &["Hermes-4-405B", "Hermes-4-70B"]);
    e.aliases = vec!["nous".into()];
    v.push(e);
    v.push(entry("novita", Format::OpenAI, "https://api.novita.ai/openai/v1/chat/completions", AuthHeader::Bearer, &["deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash", "deepseek/deepseek-v3.2", "moonshotai/kimi-k3", "moonshotai/kimi-k2.7-code", "moonshotai/kimi-k2.6", "zai-org/glm-5.2", "zai-org/glm-5.1", "zai-org/glm-4.7", "minimax/minimax-m3", "minimax/minimax-m2.7", "qwen/qwen3.7-max", "qwen/qwen3.6-plus", "qwen/qwen3.5-397b-a17b", "qwen/qwen3-coder-480b-a35b-instruct", "xiaomimimo/mimo-v2.5-pro", "google/gemma-4-31b-it", "meta-llama/llama-3.1-8b-instruct"]));
    v.push(entry("nscale", Format::OpenAI, "https://inference.api.nscale.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("nube", Format::OpenAI, "https://ai.nube.sh/api/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("nvidia", Format::OpenAI, "https://integrate.api.nvidia.com/v1/chat/completions", AuthHeader::Bearer, &["moonshotai/kimi-k3", "deepseek-ai/deepseek-v4-pro-0813", "deepseek-ai/deepseek-v4-flash-0731", "meta/muse-glimmer-30b", "poolside/laguna-xs-2.1", "google/gemma-4-31b-it", "google/diffusiongemma-26b-a4b-it", "nvidia/nemotron-3-ultra-550b-a55b", "nvidia/nemotron-3-super-120b-a12b", "nvidia/nemotron-3.5-lightning-30b-a3b", "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning", "openai/gpt-oss-120b"]));
    let mut e = entry("openadapter", Format::OpenAI, "https://api.openadapter.in/v1/chat/completions", AuthHeader::Bearer, &["glm-4.7"]);
    e.aliases = vec!["oad".into()];
    v.push(e);
    v.push(entry("orcarouter", Format::OpenAI, "https://api.orcarouter.ai/v1/chat/completions", AuthHeader::Bearer, &["orcarouter/auto", "openai/gpt-5.5", "google/gemini-3.6-flash", "anthropic/claude-opus-4.8", "grok/grok-4.3", "deepseek/deepseek-v4-pro", "minimax/minimax-m2.7", "qwen/qwen3.7-max"]));
    let mut e = entry("ovhcloud", Format::OpenAI, "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["ovh".into()];
    v.push(e);
    let mut e = entry("perplexity-agent", Format::OpenAIResponses, "https://api.perplexity.ai/v1/responses", AuthHeader::Bearer, &["openai/gpt-5.6-sol", "perplexity/kimi-k3"]);
    e.aliases = vec!["pplx-agent".into()];
    v.push(e);
    let mut e = entry("pioneer", Format::OpenAI, "https://api.pioneer.ai/v1/chat/completions", AuthHeader::XApiKey, &["Qwen/Qwen3-32B", "Qwen/Qwen3.6-27B", "Qwen/Qwen3.5-9B", "Qwen/Qwen3-8B", "Qwen/Qwen3-4B-Base", "Qwen/Qwen3-1.7B-Base", "meta-llama/Llama-3.1-8B-Instruct", "meta-llama/Llama-3.2-1B-Instruct", "google/gemma-3-4b-pt", "HuggingFaceTB/SmolLM3-3B-Base"]);
    e.aliases = vec!["pn".into()];
    v.push(e);
    v.push(entry("plamo", Format::OpenAI, "https://api.platform.preferredai.jp/v1/chat/completions", AuthHeader::Bearer, &["plamo-3.0-prime"]));
    v.push(entry("predibase", Format::OpenAI, "https://serving.app.predibase.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("publicai", Format::OpenAI, "https://api.publicai.co/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("qianfan", Format::OpenAI, "https://qianfan.baidubce.com/v2/chat/completions", AuthHeader::Bearer, &["ernie-5.1", "ernie-5.0-thinking-latest", "ernie-x1.1"]));
    v.push(entry("qiniu", Format::OpenAI, "https://api.qnaigc.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("qwen-cloud", Format::OpenAI, "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["qwc".into()];
    v.push(e);
    let mut e = entry("qwen-cloud-token-plan", Format::OpenAI, "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1/chat/completions", AuthHeader::Bearer, &["qwen3.8-max", "qwen3.7-max", "qwen3.7-plus", "qwen3.6-flash", "glm-5.2", "deepseek-v4-pro", "deepseek-v4-flash-0731"]);
    e.aliases = vec!["qct".into()];
    v.push(e);
    v.push(entry("regolo", Format::OpenAI, "https://api.regolo.ai", AuthHeader::Bearer, &["regolo-chat", "regolo-fast"]));
    v.push(entry("reka", Format::OpenAI, "https://api.reka.ai/v1/chat/completions", AuthHeader::Bearer, &["reka-flash-3", "reka-flash", "reka-edge-2603"]));
    let mut e = entry("sambanova", Format::OpenAI, "https://api.sambanova.ai/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["samba".into()];
    v.push(e);
    v.push(entry("sarvam", Format::OpenAI, "https://api.sarvam.ai/v1/chat/completions", AuthHeader::Bearer, &["sarvam-105b", "sarvam-30b"]));
    let mut e = entry("scaleway", Format::OpenAI, "https://api.scaleway.ai/v1/chat/completions", AuthHeader::Bearer, &["qwen3-235b-a22b-instruct-2507", "llama-3.1-70b-instruct", "llama-3.1-8b-instruct", "mistral-small-3.2-24b-instruct-2506", "deepseek-v3-0324", "gpt-oss-120b"]);
    e.aliases = vec!["scw".into()];
    v.push(e);
    v.push(entry("sensenova", Format::OpenAI, "https://token.sensenova.cn/v1/chat/completions", AuthHeader::Bearer, &["sensenova-6.7-flash-lite", "deepseek-v4-flash", "glm-5.2"]));
    v.push(entry("snowflake", Format::OpenAI, "https://{account}.snowflakecomputing.com/api/v2", AuthHeader::Bearer, &[]));
    v.push(entry("sparkdesk", Format::OpenAI, "https://spark-api-open.xf-yun.com/v1/chat/completions", AuthHeader::Bearer, &["4.0Ultra", "generalv3", "pro-128k", "lite"]));
    v.push(entry("stepfun", Format::OpenAI, "https://api.stepfun.com/v1/chat/completions", AuthHeader::Bearer, &["step-3.7-flash", "step-3.5-flash", "step-3.5-flash-2603", "step-1o-turbo-vision", "step-1v"]));
    v.push(entry("sumopod", Format::OpenAI, "https://ai.sumopod.com/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("synthetic", Format::OpenAI, "https://api.synthetic.new/openai/v1/chat/completions", AuthHeader::Bearer, &["hf:openai/gpt-oss-120b", "hf:zai-org/GLM-5.2", "hf:moonshotai/Kimi-K2.7-Code", "hf:Qwen/Qwen3.6-27B", "hf:MiniMaxAI/MiniMax-M3", "hf:zai-org/GLM-4.7-Flash", "hf:nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4"]));
    let mut e = entry("tabitoken", Format::Claude, "https://tabitoken.com/v1/messages", AuthHeader::XApiKey, &["claude-opus-5", "claude-opus-5-thinking", "claude-opus-4-8", "claude-opus-4-8-thinking"]);
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.chat_path = Some("".into());
    v.push(e);
    v.push(entry("tencent", Format::OpenAI, "https://api.hunyuan.cloud.tencent.com/v1/chat/completions", AuthHeader::Bearer, &["hunyuan-turbos-latest", "hunyuan-t1-latest", "hunyuan-pro", "hunyuan-vision", "hunyuan-functioncall", "hunyuan-lite"]));
    let mut e = entry("token-kiosk", Format::OpenAI, "https://agent-router.gaib.ai/v1/chat/completions", AuthHeader::Bearer, &["claude-3-5-sonnet", "deepseek-v3", "deepseek-r1", "kimi-k1.5", "minimax-m6"]);
    e.aliases = vec!["tk".into()];
    v.push(e);
    let mut e = entry("tokenrouter", Format::OpenAI, "https://api.tokenrouter.com/v1/chat/completions", AuthHeader::Bearer, &["minimax-3", "deepseek-v4-pro", "deepseek-v4-flash"]);
    e.aliases = vec!["trk".into()];
    v.push(e);
    v.push(entry("typhoon", Format::OpenAI, "https://api.opentyphoon.ai/v1/chat/completions", AuthHeader::Bearer, &["typhoon-v2.5-30b-a3b-instruct"]));
    let mut e = entry("uc-direct", Format::OpenAI, "https://api.uncensored.com/api/v1", AuthHeader::XApiKey, &["claude-opus-5", "claude-opus-5-fast", "claude-fable-5", "claude-opus-4.8", "claude-opus-4.5", "claude-sonnet-4.5", "claude-haiku-4.5", "claude-opus-4.7", "claude-opus-4.6", "claude-sonnet-4.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-4o", "gpt-4o-mini", "gpt-5.2", "gpt-5.2-codex", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini", "gpt-5.4-pro", "gpt-5.4-nano", "gpt-5.5", "gpt-5.5-pro", "gpt-5-mini", "gpt-5-nano", "openai-gpt-oss-120b", "gemini-3-6-flash", "gemini-3-flash-preview", "gemini-3.1-pro-preview", "gemini-3.1-flash-lite", "gemini-2.5-pro", "gemini-2.5-flash", "gemma-3-27b-it", "grok-4-6", "grok-4.5", "grok-4.20-beta", "grok-4.3", "deepseek-v4-flash-0731", "deepseek-v3.2", "deepseek-v4-pro", "deepseek-v4-flash", "deepseek-r1", "qwen-3-8-2-4t-a95b", "qwen-3-8-max", "qwen-3-6-35b-a3b", "qwen3-235b-a22b-2507", "qwen3-235b-a22b-thinking-2507", "qwen3.5-397b-a17b", "qwen3.6-27b", "qwen3-30b-a3b", "qwen3-5-35b-a3b", "qwen3-5-9b", "qwen3-coder", "qwen3-next-80b-a3b-instruct", "qwen3-vl-235b-a22b-thinking", "qwen3-vl-30b-a3b-thinking", "qwen3.5-flash", "qwen3.5-plus", "kimi-k3", "kimi-k2", "kimi-k2.5", "kimi-k2.6", "kimi-k2-thinking", "glm-5.2", "glm-4.7-flash", "glm-5", "glm-5.1", "glm-4.7", "glm-4.6", "minimax-m2.1", "minimax-m2.5", "minimax-m2.7", "mistral-large", "mistral-small-3.2-24b-instruct", "llama-3.2-3b-instruct", "llama-3.3-70b-instruct", "nvidia-nemotron-3-5-lightning-30b-a3b", "nvidia-nemotron-3-nano-30b-a3b", "hermes-3-llama-3.1-405b"]);
    e.aliases = vec!["ucd".into()];
    v.push(e);
    let mut e = entry("uncloseai", Format::OpenAI, "https://hermes.ai.unturf.com/v1/chat/completions", AuthHeader::Bearer, &["adamo1139/Hermes-3-Llama-3.1-8B-FP8-Dynamic", "qwen3.6:27b", "gemma4:31b"]);
    e.aliases = vec!["unc".into()];
    v.push(e);
    v.push(entry("upstage", Format::OpenAI, "https://api.upstage.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("v0-vercel", Format::OpenAI, "https://api.v0.dev/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["v0".into()];
    v.push(e);
    v.push(entry("venice", Format::OpenAI, "https://api.venice.ai/api/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("vercel-ai-gateway", Format::OpenAI, "https://ai-gateway.vercel.sh/v1/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["vag".into()];
    v.push(e);
    v.push(entry("volcengine", Format::OpenAI, "https://ark.cn-beijing.volces.com/api/v3/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("volcengine-agent-plan", Format::OpenAI, "https://ark.cn-beijing.volces.com/api/plan/v3/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["veap".into()];
    v.push(e);
    let mut e = entry("volcengine-coding-plan", Format::OpenAI, "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions", AuthHeader::Bearer, &[]);
    e.aliases = vec!["vecp".into()];
    v.push(e);
    let mut e = entry("wafer", Format::Claude, "https://pass.wafer.ai/v1/messages", AuthHeader::Bearer, &["DeepSeek-V4-Pro", "MiniMax-M2.7", "Qwen3.5-397B-A17B", "GLM-5.1"]);
    e.extra_headers.push(("anthropic-version".into(), "2023-06-01".into()));
    e.chat_path = Some("".into());
    v.push(e);
    v.push(entry("wandb", Format::OpenAI, "https://api.inference.wandb.ai/v1/chat/completions", AuthHeader::Bearer, &[]));
    v.push(entry("writer", Format::OpenAI, "https://api.writer.com/v1/chat/completions", AuthHeader::Bearer, &["palmyra-x5", "palmyra-x4"]));
    v.push(entry("x5lab", Format::OpenAI, "https://api.x5lab.dev/v1/chat/completions", AuthHeader::Bearer, &[]));
    let mut e = entry("xiaomi-mimo", Format::OpenAI, "https://api.xiaomimimo.com/v1", AuthHeader::Bearer, &[]);
    e.aliases = vec!["mimo".into()];
    v.push(e);
    let mut e = entry("xiaomi-mimo-token-plan", Format::OpenAI, "https://token-plan-sgp.xiaomimimo.com/v1", AuthHeader::Bearer, &[]);
    e.aliases = vec!["mimotp".into()];
    v.push(e);
    v.push(entry("yi", Format::OpenAI, "https://api.lingyiwanwu.com/v1/chat/completions", AuthHeader::Bearer, &["yi-large"]));
    let mut e = entry("zenmux", Format::OpenAI, "https://zenmux.ai/api/v1/chat/completions", AuthHeader::Bearer, &["google/gemini-3.1-pro-preview", "google/gemini-3-flash-preview", "openai/gpt-5", "anthropic/claude-sonnet-4.5", "anthropic/claude-opus-4.5", "deepseek/deepseek-chat", "x-ai/grok-4.1-fast", "mistralai/mistral-large-2512", "z-ai/glm-4.6v-flash"]);
    e.aliases = vec!["zm".into()];
    v.push(e);
    v.push(entry("mistral", Format::OpenAI, "https://api.mistral.ai/v1", AuthHeader::Bearer,
        &["mistral-large-latest", "mistral-small-latest", "codestral-latest"]));

    v.push(entry("together", Format::OpenAI, "https://api.together.xyz/v1", AuthHeader::Bearer, &[]));

    v.push(entry("fireworks", Format::OpenAI, "https://api.fireworks.ai/inference/v1", AuthHeader::Bearer, &[]));

    v.push(entry("perplexity", Format::OpenAI, "https://api.perplexity.ai/chat/completions", AuthHeader::Bearer,
        &["sonar", "sonar-pro", "sonar-reasoning"]));

    v.push(entry("minimax", Format::OpenAI, "https://api.minimax.chat/v1", AuthHeader::Bearer,
        &["MiniMax-Text-01", "MiniMax-M1"]));

    v.push(entry("siliconflow", Format::OpenAI, "https://api.siliconflow.cn/v1", AuthHeader::Bearer, &[]));

    v.push(entry("dashscope", Format::OpenAI, "https://dashscope.aliyuncs.com/compatible-mode/v1", AuthHeader::Bearer,
        &["qwen-max", "qwen-plus", "qwen-turbo"]));
    v.last_mut().unwrap().aliases = vec!["qwen".into(), "aliyun".into()];

    v.push(entry("doubao", Format::OpenAI, "https://ark.cn-beijing.volces.com/api/v3/chat/completions", AuthHeader::Bearer, &[]));
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
    // Bulk-extracted aliases (parity: per-entry `alias` in the original registry).
    m.insert("af", "api-airforce");
    m.insert("aiml", "aimlapi");
    m.insert("ali", "alibaba");
    m.insert("ali-cn", "alibaba-cn");
    m.insert("bb", "blackbox");
    m.insert("bcp", "bailian-coding-plan");
    m.insert("bm", "bluesminds");
    m.insert("bpm", "byteplus");
    m.insert("bzl", "bazaarlink");
    m.insert("dai", "dit");
    m.insert("faik", "freeaiapikey");
    m.insert("featherless", "featherless-ai");
    m.insert("fmd", "freemodel-dev");
    m.insert("friendli", "friendliai");
    m.insert("fta", "freetheai");
    m.insert("g4fgem", "g4f-gemini");
    m.insert("g4fgroq", "g4f-groq");
    m.insert("g4fnv", "g4f-nvidia");
    m.insert("g4foll", "g4f-ollama");
    m.insert("g4fpol", "g4f-pollinations");
    m.insert("glb", "gitlawb");
    m.insert("glb-gmi", "gitlawb-gmi");
    m.insert("hf", "huggingface");
    m.insert("hyp", "hyperbolic");
    m.insert("inet", "inference-net");
    m.insert("kg", "kilo-gateway");
    m.insert("lambda", "lambda-ai");
    m.insert("lc", "longcat");
    m.insert("leo", "leonardo");
    m.insert("ling", "ant-ling");
    m.insert("meta", "meta-llama");
    m.insert("mimo", "xiaomi-mimo");
    m.insert("mimotp", "xiaomi-mimo-token-plan");
    m.insert("monster", "monsterapi");
    m.insert("ms", "modelscope");
    m.insert("naga", "naga-ac");
    m.insert("nlpc", "nlpcloud");
    m.insert("nous", "nous-research");
    m.insert("oad", "openadapter");
    m.insert("ovh", "ovhcloud");
    m.insert("pn", "pioneer");
    m.insert("pplx-agent", "perplexity-agent");
    m.insert("qct", "qwen-cloud-token-plan");
    m.insert("qwc", "qwen-cloud");
    m.insert("samba", "sambanova");
    m.insert("scw", "scaleway");
    m.insert("tk", "token-kiosk");
    m.insert("trk", "tokenrouter");
    m.insert("ucd", "uc-direct");
    m.insert("unc", "uncloseai");
    m.insert("v0", "v0-vercel");
    m.insert("vag", "vercel-ai-gateway");
    m.insert("veap", "volcengine-agent-plan");
    m.insert("vecp", "volcengine-coding-plan");
    m.insert("zm", "zenmux");
    m
}

/// Resolve a provider token (id or alias) to canonical id.
pub fn resolve_provider_alias(token: &str) -> Option<&'static str> {
    provider_aliases().get(token).copied()
}

/// The live registry: static entries + dynamic compatible families built from
/// credentials (`openai-compatible-<name>`, `anthropic-compatible-<name>`,
/// `anthropic-compatible-cc-<name>`).
#[derive(Default)]
pub struct Registry {
    entries: std::sync::RwLock<HashMap<String, Arc<RegistryEntry>>>,
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        let snap = self.entries.read().unwrap_or_else(|e| e.into_inner()).clone();
        Self { entries: std::sync::RwLock::new(snap) }
    }
}

impl Registry {
    pub fn new(statics: Vec<RegistryEntry>) -> Self {
        let mut entries = HashMap::new();
        for e in statics {
            entries.insert(e.id.clone(), Arc::new(e));
        }
        Self { entries: std::sync::RwLock::new(entries) }
    }

    /// Register a dynamic compatible provider from credentials.
    pub fn register_dynamic(
        &self,
        id: &str,
        base_url: Option<String>,
        api_type: Option<String>,
        models: Vec<String>,
    ) {

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
        self.entries.write().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), Arc::new(e));
    }

    pub fn get(&self, id: &str) -> Option<Arc<RegistryEntry>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// Unregister a runtime-registered dynamic family (admin delete).
    /// Model ids declared by one provider (registry-side catalogue).
    pub fn models_for(&self, id: &str) -> Vec<String> {
        self.get(id).map(|e| e.default_models.clone()).unwrap_or_default()
    }

    pub fn unregister(&self, id: &str) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    pub fn all(&self) -> Vec<Arc<RegistryEntry>> {
        let mut v: Vec<Arc<RegistryEntry>> = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
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
        let reg = Registry::new(static_registry());
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

    #[test]
    fn bulk_extracted_catalog_stays_wired() {
        let reg = Registry::new(static_registry());
        // 24 hand-written + 122 bulk-extracted plain-HTTP entries
        assert_eq!(reg.ids().len(), 146);
        let cerebras = reg.get("cerebras").unwrap();
        assert_eq!(cerebras.format, Format::OpenAI);
        assert!(!cerebras.default_models.is_empty());
        // claude-format aggregator keeps its version header and full-path base
        let agentrouter = reg.get("agentrouter").unwrap();
        assert_eq!(agentrouter.format, Format::Claude);
        assert_eq!(agentrouter.chat_path.as_deref(), Some(""));
        assert!(agentrouter
            .extra_headers
            .iter()
            .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
        // responses-format entry keeps its full-path base
        let agent = reg.get("perplexity-agent").unwrap();
        assert_eq!(agent.format, Format::OpenAIResponses);
        assert!(agent.base_url.ends_with("/responses"));
        // aliases resolve alongside ids
        assert_eq!(crate::registry::resolve_provider_alias("aiml"), Some("aimlapi"));
        assert_eq!(crate::registry::resolve_provider_alias("hf"), Some("huggingface"));
    }
}
