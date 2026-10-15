//! Built-in per-model capability catalog for models whose provider listing
//! carries no limits or modalities (the opencode / opencode-zen / opencode-go
//! families and relays re-exporting the same model ids).
//!
//! The reference gateway resolves these from a shared model-catalog snapshot;
//! this is the Rust equivalent. Rows originate from
//! <https://models.dev/api.json>, where capabilities are keyed by provider, but
//! resolution here is **model-first**: an exact `provider|model` row wins,
//! otherwise the same model id on any provider resolves, because relays and
//! gateways re-export the same ids and the capability belongs to the model.
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
///
/// Resolution is model-first: a row for the exact provider wins, otherwise the
/// same model id on any provider resolves (capabilities belong to the model, and
/// relays/gateways re-export the same ids). When rows disagree the most common
/// value wins, with ties resolved conservatively (smaller window / fewer
/// modalities) so an unknown relay is never over-advertised.
pub fn lookup(provider: &str, model: &str) -> Option<CatalogEntry> {
    let model = model.rsplit('/').next().unwrap_or(model);
    let rows: Vec<(&str, &str, CatalogEntry)> = CATALOG.lines().filter_map(parse_row).collect();

    if let Some((_, _, entry)) = rows.iter().find(|(p, m, _)| *p == provider && *m == model) {
        return Some(entry.clone());
    }

    let matches: Vec<&(&str, &str, CatalogEntry)> = rows.iter().filter(|(_, m, _)| *m == model).collect();
    if matches.is_empty() {
        return None;
    }
    if matches.len() == 1 {
        return Some(matches[0].2.clone());
    }

    // Most common value, ties resolved conservatively.
    let pick = |values: Vec<i64>| -> i64 {
        let mut counts: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
        for v in values {
            *counts.entry(v).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by_key(|(v, n)| (*n, std::cmp::Reverse(*v)))
            .map(|(v, _)| v)
            .unwrap_or_default()
    };
    let modality_key = |entry: &CatalogEntry| -> String { entry.modalities.join("+") };
    let mut modality_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in &matches {
        *modality_counts.entry(modality_key(&entry.2)).or_default() += 1;
    }
    let modalities = modality_counts
        .into_iter()
        .max_by_key(|(key, n)| (*n, std::cmp::Reverse(key.matches('+').count())))
        .map(|(key, _)| key)
        .unwrap_or_default();
    let reasoning = matches.iter().filter(|(_, _, e)| e.reasoning).count() * 2 >= matches.len();

    Some(CatalogEntry {
        context_window: pick(matches.iter().map(|(_, _, e)| e.context_window).collect()),
        max_output_tokens: pick(matches.iter().map(|(_, _, e)| e.max_output_tokens).collect()),
        modalities: modalities
            .split('+')
            .filter(|m| !m.is_empty())
            .map(|m| match m {
                "image" => "image",
                "pdf" => "pdf",
                _ => "text",
            })
            .collect(),
        reasoning,
    })
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
    fn same_model_resolves_on_any_provider() {
        // A relay/gateway re-exporting the model id must not fall back to the
        // unknown-model defaults.
        let relay = lookup("openai-compatible-my-relay", "space-bunny").expect("global model row");
        assert_eq!(relay.context_window, 524_288);
        assert!(relay.modalities.contains(&"image"));
        let gw = lookup("openrouter", "step-5-preview-free").expect("global model row");
        assert_eq!(gw.context_window, 1_000_000);
        assert_eq!(gw.max_output_tokens, 65_536);
    }

    #[test]
    fn unknown_models_fall_through() {
        assert!(lookup("opencode-go", "not-a-real-model").is_none());
        assert!(lookup("openai", "not-a-real-model").is_none());
    }
}
