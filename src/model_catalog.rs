//! Built-in per-model capability catalog for providers whose upstream
//! `/models` listing carries no limits or modalities (the opencode /
//! opencode-zen / opencode-go families).
//!
//! The reference gateway resolves these from a shared model-catalog snapshot;
//! this is the Rust equivalent for the same provider ids, so models published
//! upstream still report a correct context window and input modalities even
//! though the provider itself only returns an id.
//!
//! Row format: `provider|model|context|output|modalities|reasoning`
//! - `context` is the effective input limit (`min(limit.context, limit.input)`)
//! - `output` is the max output-token limit
//! - `modalities` is a `+`-joined subset of `text` / `image` / `pdf`
//! - `reasoning` is `R` when the model exposes selectable efforts

/// `provider|model|context|output|modalities|reasoning` rows.
pub const CATALOG: &str = "
opencode-go|cat-jam-free|262144|131072|text|R
opencode-go|claude-haiku-5-5|1000000|128000|text+image+pdf|R
opencode-go|deepseek-v4-flash-vision-exp|1000000|384000|text+image|R
opencode-go|deepseek-v4-flash|1000000|384000|text|R
opencode-go|deepseek-v4-pro|1000000|384000|text|R
opencode-go|deepseek-v4.1-flash|1000000|384000|text+image|R
opencode-go|gemini-4-argon-contributor|1048576|65536|text+image+pdf|R
opencode-go|glm-5.2|1000000|131072|text|R
opencode-go|glm-5.3-flash|1000000|131072|text+image+pdf|R
opencode-go|glm-5.3|1000000|131072|text|R
opencode-go|gpt-5.6-luna|922000|128000|text+image+pdf|R
opencode-go|gpt-6-luna|922000|128000|text+image+pdf|R
opencode-go|grok-4.5|500000|500000|text+image|R
opencode-go|grok-4.6|500000|500000|text+image|R
opencode-go|grok-4.7|500000|500000|text+image+pdf|R
opencode-go|hy3|192000|128000|text|R
opencode-go|hy4-preview|1024000|64000|text|R
opencode-go|kimi-k2.6|262144|65536|text+image|R
opencode-go|kimi-k2.7-code|262144|262144|text+image|R
opencode-go|kimi-k3|1048576|131072|text+image|R
opencode-go|longcat-2.0|1000000|131072|text|R
opencode-go|longcat-2.5-preview-free|1000000|131072|text+image|R
opencode-go|mimo-v2.5-pro|1048576|128000|text|R
opencode-go|mimo-v2.5|1000000|128000|text+image|R
opencode-go|mimo-v2.6-flash|1048576|131072|text+image|R
opencode-go|mimo-v2.6-pro|1048576|131072|text+image|R
opencode-go|minimax-m2.7|204800|131072|text|R
opencode-go|minimax-m3|1000000|131072|text+image|R
opencode-go|muse-spark-1.2-contributor|1048576|131072|text+image+pdf|R
opencode-go|muse-spark-1.3-contributor|1048576|131072|text+image+pdf|R
opencode-go|qwen3.6-plus|1000000|65536|text+image|R
opencode-go|qwen3.7-max|1000000|65536|text|R
opencode-go|qwen3.7-plus|1000000|65536|text+image|R
opencode-go|qwen3.8-flash|1000000|131072|text+image|R
opencode-go|qwen3.8-max|1000000|131072|text+image|R
opencode-go|space-bunny-free|524288|524288|text+image|R
opencode-go|space-bunny|524288|524288|text+image|R
opencode-go|step-5-preview-free|1000000|65536|text+image|R
opencode|big-pickle|160000|32000|text|R
opencode|cat-jam-free|262144|131072|text|R
opencode|claude-3-5-haiku|200000|8192|text+image+pdf|-
opencode|claude-fable-5-1|1000000|128000|text+image+pdf|R
opencode|claude-fable-5|1000000|128000|text+image+pdf|R
opencode|claude-haiku-4-5|200000|64000|text+image+pdf|R
opencode|claude-haiku-5-5|1000000|128000|text+image+pdf|R
opencode|claude-opus-4-1|200000|32000|text+image+pdf|R
opencode|claude-opus-4-5|200000|64000|text+image+pdf|R
opencode|claude-opus-4-6|1000000|128000|text+image+pdf|R
opencode|claude-opus-4-7|1000000|128000|text+image+pdf|R
opencode|claude-opus-4-8|1000000|128000|text+image+pdf|R
opencode|claude-opus-5-5|1000000|128000|text+image+pdf|R
opencode|claude-opus-5|1000000|128000|text+image+pdf|R
opencode|claude-sonnet-4-5|1000000|64000|text+image+pdf|R
opencode|claude-sonnet-4-6|1000000|64000|text+image+pdf|R
opencode|claude-sonnet-4|1000000|64000|text+image+pdf|R
opencode|claude-sonnet-5-5|1000000|128000|text+image+pdf|R
opencode|claude-sonnet-5|1000000|128000|text+image+pdf|R
opencode|deepseek-v4-flash-free|200000|128000|text|R
opencode|deepseek-v4-flash-vision-exp|1000000|384000|text+image|R
opencode|deepseek-v4-flash|1000000|384000|text|R
opencode|deepseek-v4-pro|1000000|384000|text|R
opencode|deepseek-v4.1-flash|1000000|384000|text+image|R
opencode|exo-free|1048576|131072|text+image|R
opencode|fledge-alpha-free|1048576|131072|text+image|R
opencode|gemini-3-flash|1048576|65536|text+image+pdf|R
opencode|gemini-3-pro|1048576|65536|text+image+pdf|R
opencode|gemini-3.1-pro|1048576|65536|text+image+pdf|R
opencode|gemini-3.5-flash-lite|1048576|65536|text+image+pdf|R
opencode|gemini-3.5-flash|1048576|65536|text+image+pdf|R
opencode|gemini-3.6-flash|1048576|65536|text+image+pdf|R
opencode|gemini-3.7-flash|1048576|65536|text+image+pdf|R
opencode|gemini-3.8-flash|1048576|65536|text+image+pdf|R
opencode|gemini-4-argon|1048576|65536|text+image+pdf|R
opencode|glm-4.6|204800|131072|text|R
opencode|glm-4.7-free|204800|131072|text|R
opencode|glm-4.7|204800|131072|text|R
opencode|glm-5-free|204800|131072|text|R
opencode|glm-5.1|204800|131072|text|R
opencode|glm-5.2|1000000|131072|text|R
opencode|glm-5.3-flash|1000000|131072|text+image+pdf|R
opencode|glm-5.3|1000000|131072|text|R
opencode|glm-5|204800|131072|text|R
opencode|gpt-5-codex|272000|128000|text+image|R
opencode|gpt-5-nano|272000|128000|text+image|R
opencode|gpt-5.1-codex-max|272000|128000|text+image|R
opencode|gpt-5.1-codex-mini|272000|128000|text+image|R
opencode|gpt-5.1-codex|272000|128000|text+image|R
opencode|gpt-5.1|272000|128000|text+image|R
opencode|gpt-5.2-codex|272000|128000|text+image+pdf|R
opencode|gpt-5.2|272000|128000|text+image|R
opencode|gpt-5.3-codex-spark|128000|128000|text|R
opencode|gpt-5.3-codex|272000|128000|text+image+pdf|R
opencode|gpt-5.4-mini|272000|128000|text+image+pdf|R
opencode|gpt-5.4-nano|272000|128000|text+image+pdf|R
opencode|gpt-5.4-pro|922000|128000|text+image+pdf|R
opencode|gpt-5.4|922000|128000|text+image+pdf|R
opencode|gpt-5.5-pro|922000|128000|text+image+pdf|R
opencode|gpt-5.5|922000|128000|text+image+pdf|R
opencode|gpt-5.6-luna|922000|128000|text+image+pdf|R
opencode|gpt-5.6-sol|922000|128000|text+image+pdf|R
opencode|gpt-5.6-terra|922000|128000|text+image+pdf|R
opencode|gpt-5|272000|128000|text+image|R
opencode|gpt-6-astra|922000|128000|text+image+pdf|R
opencode|gpt-6-luna|922000|128000|text+image+pdf|R
opencode|gpt-6-sol|922000|128000|text+image+pdf|R
opencode|gpt-6.1-sol|922000|128000|text+image+pdf|R
opencode|grok-4.5|500000|500000|text+image|R
opencode|grok-4.6|500000|500000|text+image|R
opencode|grok-4.7|500000|500000|text+image+pdf|R
opencode|grok-build-0.1|256000|256000|text+image+pdf|R
opencode|grok-code|256000|256000|text|R
opencode|hy3-free|190000|64000|text|R
opencode|hy3-preview-free|256000|64000|text|R
opencode|kimi-k2-thinking|262144|262144|text|R
opencode|kimi-k2.5-free|262144|262144|text+image|R
opencode|kimi-k2.5|262144|65536|text+image|R
opencode|kimi-k2.6|262144|65536|text+image|R
opencode|kimi-k2.7-code|262144|262144|text+image|R
opencode|kimi-k2|262144|262144|text|-
opencode|kimi-k3|1048576|131072|text+image|R
opencode|laguna-s-2.1-free|256000|32000|text|R
opencode|ling-2.6-flash-free|262100|32800|text|-
opencode|ling-3.0-flash-fin-free|262144|32768|text|R
opencode|ling-3.0-flash-free|262144|32768|text|R
opencode|ling-3.0-tiny-free|262144|32768|text|R
opencode|ling-3.1-flash-free|262144|32768|text|R
opencode|longcat-2.0-free|1000000|131072|text|R
opencode|longcat-2.5-preview-free|1000000|131072|text+image|R
opencode|mimo-v2-flash-free|262144|65536|text|R
opencode|mimo-v2-omni-free|262144|64000|text+image+pdf|R
opencode|mimo-v2-pro-free|1048576|64000|text|R
opencode|mimo-v2.5-free|200000|32000|text+image|R
opencode|mimo-v2.6-flash-free|200000|32000|text+image|R
opencode|minimax-m2.1-free|204800|131072|text|R
opencode|minimax-m2.1|204800|131072|text|R
opencode|minimax-m2.5-free|204800|131072|text|R
opencode|minimax-m2.5|204800|131072|text|R
opencode|minimax-m2.7|204800|131072|text|R
opencode|minimax-m3-free|200000|32000|text+image|R
opencode|minimax-m3|512000|128000|text+image|R
opencode|mistral-large-4|524288|262144|text+image|R
opencode|muse-spark-1.2-contributor-free|1048576|131072|text+image+pdf|R
opencode|muse-spark-1.2|1048576|131072|text+image+pdf|R
opencode|muse-spark-1.3-contributor-free|1048576|131072|text+image+pdf|R
opencode|muse-spark-1.3|1048576|131072|text+image+pdf|R
opencode|nemotron-3-super-free|204800|128000|text|R
opencode|nemotron-3-ultra-free|1000000|128000|text|R
opencode|nemotron-3.5-lightning-free|262144|262144|text|R
opencode|north-mini-code-free|256000|64000|text|R
opencode|qwen3-coder|262144|65536|text|-
opencode|qwen3.5-plus|262144|65536|text+image|R
opencode|qwen3.6-plus-free|262144|65536|text+image|R
opencode|qwen3.6-plus|262144|65536|text+image|R
opencode|qwen3.8-flash|1000000|131072|text+image|R
opencode|qwen3.8-max|262144|131072|text+image|R
opencode|ring-2.6-1t-free|262000|66000|text|R
opencode|space-bunny-free|524288|524288|text+image|R
opencode|step-5-preview-free|1000000|65536|text+image|R
opencode|trinity-large-preview-free|131072|131072|text|-
opencode|x-preview-f-free|1000000|131072|text+image|R
";

/// One resolved catalog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub modalities: Vec<&'static str>,
    pub reasoning: bool,
}

fn parse_row(row: &str) -> Option<(&str, &str, CatalogEntry)> {
    let mut parts = row.split('|');
    let provider = parts.next()?;
    let model = parts.next()?;
    let context = parts.next()?.parse::<i64>().ok()?;
    let output = parts.next()?.parse::<i64>().ok()?;
    let modalities = parts.next()?;
    let reasoning = parts.next()? == "R";
    let modalities: Vec<&'static str> = modalities
        .split('+')
        .filter(|m| !m.is_empty())
        .map(|m| match m {
            "text" => "text",
            "image" => "image",
            "pdf" => "pdf",
            _ => "text",
        })
        .collect();
    Some((provider, model, CatalogEntry {
        context_window: context,
        max_output_tokens: output,
        modalities,
        reasoning,
    }))
}

/// Look up one exact provider/model pair.
pub fn lookup(provider: &str, model: &str) -> Option<CatalogEntry> {
    let model = model.rsplit('/').next().unwrap_or(model);
    CATALOG
        .lines()
        .filter_map(parse_row)
        .find(|(p, m, _)| *p == provider && *m == model)
        .map(|(_, _, entry)| entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_modalities_for_dynamic_provider_models() {
        let bunny = lookup("opencode-go", "space-bunny").expect("space-bunny catalog row");
        assert_eq!(bunny.context_window, 524_288);
        assert_eq!(bunny.modalities, vec!["text", "image"]);
        assert!(bunny.reasoning);

        let step = lookup("opencode-go", "step-5-preview-free").expect("step-5 catalog row");
        assert_eq!(step.context_window, 1_000_000);
        assert_eq!(step.max_output_tokens, 65_536);
        assert!(step.modalities.contains(&"image"));
    }

    #[test]
    fn unknown_models_fall_through() {
        assert!(lookup("opencode-go", "not-a-real-model").is_none());
        assert!(lookup("openai", "space-bunny").is_none());
    }
}
