//! Wire-format detection (parity: `open-sse/services/provider.ts#detectFormat`).

/// Inbound wire format derived from the request path/body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `/v1/chat/completions`
    OpenAI,
    /// `/v1/responses`
    OpenAIResponses,
    /// `/v1/messages`
    Claude,
    /// gemini-native bodies (body detection only)
    Gemini,
    /// legacy `/v1/completions` prompt shape (projected onto openai chat)
    Completions,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::OpenAI => "openai",
            Format::OpenAIResponses => "openai-responses",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
            Format::Completions => "completions",
        }
    }
}

/// Detect inbound format from the request path (parity:
/// `detectFormatFromEndpoint`).
pub fn detect_format_from_endpoint(path: &str) -> Format {
    let p = path.to_ascii_lowercase();
    if p.contains("responses") {
        Format::OpenAIResponses
    } else if p.contains("messages") {
        Format::Claude
    } else if p.contains("completions") && !p.contains("chat") {
        Format::Completions
    } else {
        Format::OpenAI
    }
}

/// Body-based fallback (parity: `detectFormat` body heuristics).
pub fn detect_format_from_body(body: &serde_json::Value) -> Format {
    if body.get("system").is_some() && body.get("messages").is_some() {
        Format::Claude
    } else if body.get("contents").is_some() {
        Format::Gemini
    } else if body.get("prompt").is_some() && body.get("messages").is_none() {
        Format::Completions
    } else if body.get("input").is_some() && body.get("messages").is_none() {
        Format::OpenAIResponses
    } else {
        Format::OpenAI
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_detection() {
        assert_eq!(detect_format_from_endpoint("/v1/chat/completions"), Format::OpenAI);
        assert_eq!(detect_format_from_endpoint("/v1/messages"), Format::Claude);
        assert_eq!(detect_format_from_endpoint("/v1/messages/count_tokens"), Format::Claude);
        assert_eq!(detect_format_from_endpoint("/v1/responses"), Format::OpenAIResponses);
        assert_eq!(detect_format_from_endpoint("/v1/completions"), Format::Completions);
        assert_eq!(detect_format_from_endpoint("/v1beta/chat/completions"), Format::OpenAI);
    }

    #[test]
    fn body_detection() {
        assert_eq!(detect_format_from_body(&json!({"system":"x","messages":[]})), Format::Claude);
        assert_eq!(detect_format_from_body(&json!({"contents":[]})), Format::Gemini);
        assert_eq!(detect_format_from_body(&json!({"input":"hi"})), Format::OpenAIResponses);
        assert_eq!(detect_format_from_body(&json!({"prompt":"hi"})), Format::Completions);
        assert_eq!(detect_format_from_body(&json!({"messages":[],"model":"m"})), Format::OpenAI);
    }

    #[test]
    fn format_names() {
        assert_eq!(Format::OpenAI.as_str(), "openai");
        assert_eq!(Format::Completions.as_str(), "completions");
    }
}
