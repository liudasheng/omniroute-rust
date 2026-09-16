//! Model-string parsing (parity with `open-sse/services/model.ts#parseModel`).
//!
//! Accepted shapes:
//! - `provider/model`          → provider = resolved alias of token[0]
//! - `alias/model`             → same, alias table decides provider
//! - `model` (bare)            → resolved through the global model alias table
//!   plus per-family prefix heuristics (`claude-*` → anthropic, ...)
//! Suffix `[1m]` marks extended context (`extendedContext:true`).

use crate::registry::resolve_provider_alias;

/// Strip the `[1m]`-style context-window suffix, returning (base, extended).
pub fn strip_context_window_suffix(s: &str) -> (String, bool) {
    let s = s.trim();
    if let Some(base) = s.strip_suffix("[1m]") {
        return (base.to_string(), true);
    }
    // ":-1m" / ":1m" style used by some clients
    for suf in [":-1m", ":1m"] {
        if let Some(base) = s.strip_suffix(suf) {
            let base = base.trim_end();
            if !base.is_empty() {
                return (base.to_string(), true);
            }
        }
    }
    (s.to_string(), false)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedModel {
    /// Canonical provider id if the string (or alias map) determined one.
    pub provider: Option<String>,
    /// Model id as the provider should see it (never empty).
    pub model: String,
    /// `[1m]` extended-context marker requested.
    pub extended_context: bool,
    /// True when the model string was resolved through the bare-model alias
    /// table (`isAlias` in the original).
    pub is_alias: bool,
}

/// Bare-model alias table (parity: `MODEL ALIAS` map + bare-model resolver).
pub fn resolve_bare_model(model: &str) -> Option<(&'static str, String)> {
    let m = model;
    let (prov, target): (&str, String) = if m == "sonnet" || m == "opus" || m == "haiku" || m.starts_with("claude-") {
        match m {
            "sonnet" => ("anthropic", "claude-sonnet-4-5-20250929".into()),
            "opus" => ("anthropic", "claude-opus-4-1-20250529".into()),
            "haiku" => ("anthropic", "claude-haiku-4-5-20251001".into()),
            _ => ("anthropic", m.to_string()),
        }
    } else if m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("chatgpt-") {
        ("openai", m.to_string())
    } else if m.starts_with("gemini-") {
        ("gemini", m.to_string())
    } else if m.starts_with("glm-") {
        ("glm", m.to_string())
    } else if m.starts_with("kimi-") || m.starts_with("moonshot-") {
        ("kimi", m.to_string())
    } else if m.starts_with("deepseek-") {
        ("deepseek", m.to_string())
    } else if m.starts_with("grok-") {
        ("xai", m.to_string())
    } else if m.starts_with("qwen-") {
        ("dashscope", m.to_string())
    } else if m.starts_with("doubao-") {
        ("doubao", m.to_string())
    } else if m.starts_with("llama-3") && !m.contains('/') {
        ("groq", m.to_string())
    } else {
        return None;
    };
    Some((prov, target))
}

/// Parse a model string into (provider, model).
pub fn parse_model(input: &str) -> ParsedModel {
    let (base, extended) = strip_context_window_suffix(input);
    if base.is_empty() {
        return ParsedModel {
            provider: None,
            model: base,
            extended_context: extended,
            is_alias: false,
        };
    }

    // provider/model — split at the FIRST '/', only when the prefix resolves
    // to a known provider or alias (protects fireworks-style in-model slashes).
    if let Some(idx) = base.find('/') {
        let (prov_tok, rest) = base.split_at(idx);
        let rest = &rest[1..];
        if !prov_tok.is_empty() && !rest.is_empty() {
            if let Some(canonical) = resolve_provider_alias(prov_tok) {
                return ParsedModel {
                    provider: Some(canonical.to_string()),
                    model: rest.to_string(),
                    extended_context: extended,
                    is_alias: false,
                };
            }
        }
    }

    // Bare model → alias resolution.
    if let Some((prov, target)) = resolve_bare_model(&base) {
        return ParsedModel {
            provider: Some(prov.to_string()),
            model: target,
            extended_context: extended,
            is_alias: true,
        };
    }

    ParsedModel {
        provider: None,
        model: base,
        extended_context: extended,
        is_alias: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_model() {
        let p = parse_model("anthropic/claude-sonnet-4-5-20250929");
        assert_eq!(p.provider.as_deref(), Some("anthropic"));
        assert_eq!(p.model, "claude-sonnet-4-5-20250929");
        assert!(!p.extended_context);
        assert!(!p.is_alias);
    }

    #[test]
    fn parses_alias_provider() {
        let p = parse_model("claude/claude-sonnet-4-5");
        assert_eq!(p.provider.as_deref(), Some("anthropic"));
        assert_eq!(p.model, "claude-sonnet-4-5");

        let p = parse_model("google/gemini-2.5-pro");
        assert_eq!(p.provider.as_deref(), Some("gemini"));
    }

    #[test]
    fn extended_context_suffix() {
        let p = parse_model("anthropic/claude-sonnet-4-5[1m]");
        assert!(p.extended_context);
        assert_eq!(p.model, "claude-sonnet-4-5");

        let p = parse_model("gemini-2.5-pro:1m");
        assert!(p.extended_context);
        assert_eq!(p.model, "gemini-2.5-pro");

        let p = parse_model("gpt-4o");
        assert!(!p.extended_context);
    }

    #[test]
    fn bare_models_resolve_to_default_providers() {
        let p = parse_model("claude-sonnet-4-5-20250929");
        assert_eq!(p.provider.as_deref(), Some("anthropic"));
        assert!(p.is_alias);

        let p = parse_model("gpt-4o");
        assert_eq!(p.provider.as_deref(), Some("openai"));

        let p = parse_model("gemini-2.5-flash");
        assert_eq!(p.provider.as_deref(), Some("gemini"));

        let p = parse_model("kimi-k2-thinking");
        assert_eq!(p.provider.as_deref(), Some("kimi"));

        let p = parse_model("deepseek-reasoner");
        assert_eq!(p.provider.as_deref(), Some("deepseek"));

        // short aliases
        let p = parse_model("sonnet");
        assert_eq!(p.provider.as_deref(), Some("anthropic"));
        assert_eq!(p.model, "claude-sonnet-4-5-20250929");
        let p = parse_model("opus");
        assert_eq!(p.model, "claude-opus-4-1-20250529");
        let p = parse_model("haiku");
        assert_eq!(p.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn unknown_model_stays_unparsed() {
        let p = parse_model("some-custom-model");
        assert_eq!(p.provider, None);
        assert_eq!(p.model, "some-custom-model");
        assert!(!p.is_alias);
    }

    #[test]
    fn fireworks_style_inner_slashes_stay_bare() {
        // prefix not a known provider → whole string is the bare model
        let p = parse_model("accounts/fireworks/models/llama-v3");
        assert_eq!(p.provider, None);
        assert_eq!(p.model, "accounts/fireworks/models/llama-v3");
    }
}
