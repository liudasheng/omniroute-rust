//! Combo routing strategies (parity: `src/shared/constants/routingStrategies.ts`
//! + `open-sse/services/combo.ts`).
//!
//! A "combo" is an ordered list of provider targets tried until one succeeds.
//! The strategy decides the candidate ordering; MAX_COMBO_DEPTH bounds how
//! deep a chain may recurse; MAX_GLOBAL_ATTEMPTS bounds total upstream tries.

use crate::state::AppState;
use std::cmp::Reverse;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hard limits (comboPredicates.ts).
pub const MAX_COMBO_DEPTH: usize = 3;
pub const MAX_COMBO_DEPTH_HARD: usize = 10;
pub const MAX_GLOBAL_ATTEMPTS: usize = 30;
pub const MAX_GLOBAL_ATTEMPTS_HARD: usize = 200;
pub const MAX_FALLBACK_WAIT_MS: u64 = 5_000;
pub const COMBO_LOOP_SAFETY_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
pub const COMBO_SAFETY_DRAIN_MS: u64 = 2_000;
pub const UNAVAILABLE_LABEL_GRACE_MS: u64 = 60_000;

/// Supported strategy ids (for /v1 + validation). `failover` is an accepted
/// alias of `priority`.
pub const STRATEGIES: [&str; 10] = [
    "priority",
    "round-robin",
    "fill-first",
    "weighted",
    "random",
    "least-used",
    "p2c",
    "cost-optimized",
    "lkgp",
    "auto",
];

/// Alias table parity: `failover`→priority, `usage`→least-used, `rr`→round-robin…
pub fn normalize_strategy(s: &str) -> &'static str {
    match s.trim().to_ascii_lowercase().as_str() {
        "failover" | "priority" => "priority",
        "weighted" => "weighted",
        "round-robin" | "roundrobin" | "rr" => "round-robin",
        "fill-first" => "fill-first",
        "p2c" => "p2c",
        "random" => "random",
        "least-used" | "usage" => "least-used",
        "cost-optimized" | "cost" => "cost-optimized",
        "lkgp" | "last-known-good" => "lkgp",
        "auto" => "auto",
        _ => "priority", // unknown → priority
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    /// which combo (if any) produced this candidate
    pub combo: Option<String>,
    /// priority position (lower first)
    pub position: usize,
    /// weight for the `weighted` strategy (combo syntax: "openai=2")
    pub weight: f64,
}

/// Shared round-robin cursor across combos.
static RR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Approximate relative unit cost used by `cost-optimized`.
fn provider_cost(provider: &str) -> u64 {
    match provider {
        "ollama" | "lmstudio" | "vllm" => 0,
        "groq" | "deepseek" | "glm" | "kimi" | "zai" | "ollama-cloud" | "dashscope" => 1,
        "openai" | "anthropic" | "gemini" | "openrouter" | "xai" | "mistral" | "together" | "fireworks" => 2,
        _ => 1,
    }
}

pub fn provider_cost_for_test(provider: &str) -> u64 {
    provider_cost(provider)
}

/// Order candidates according to the strategy.
pub fn order_candidates(state: &AppState, strategy: &str, mut cands: Vec<Candidate>) -> Vec<Candidate> {
    if cands.len() <= 1 {
        return cands;
    }
    match normalize_strategy(strategy) {
        "round-robin" => {
            let n = cands.len() as u64;
            let offset = RR_COUNTER.fetch_add(1, Ordering::Relaxed) % n;
            cands.rotate_left(offset as usize);
        }
        "random" => {
            use rand::seq::SliceRandom;
            cands.shuffle(&mut rand::rng());
        }
        "p2c" => {
            // power-of-two-choices over the least-loaded pair
            let len = cands.len();
            let mut rng = rand::rng();
            let a = rand::Rng::random_range(&mut rng, 0..len);
            let b = rand::Rng::random_range(&mut rng, 0..len);
            let load = |c: &Candidate| state.circuits.inflight(&c.provider);
            if a != b && load(&cands[a]) > load(&cands[b]) {
                cands.swap(a, b);
            }
        }
        "weighted" => {
            let total: f64 = cands.iter().map(|c| c.weight.max(0.001)).sum();
            let mut pick = rand::Rng::random::<f64>(&mut rand::rng()) * total;
            for i in 0..cands.len() {
                pick -= cands[i].weight.max(0.001);
                if pick <= 0.0 {
                    cands.drain(..i);
                    break;
                }
            }
        }
        "least-used" => {
            cands.sort_by_key(|c| Reverse(state.circuits.inflight(&c.provider)));
        }
        "lkgp" => {
            cands.sort_by_key(|c| Reverse(state.circuits.last_success(&c.provider).map(|t| t.elapsed().as_nanos())));
        }
        "cost-optimized" => {
            cands.sort_by_key(|c| provider_cost(&c.provider));
        }
        // priority / fill-first / auto → keep declared order
        _ => {}
    }
    cands
}

/// Parse "provider" | "provider/model" | "provider=model" | "provider/model=2".
pub fn parse_provider_spec(spec: &str) -> (String, Option<String>, f64) {
    let (head, weight) = match spec.rsplit_once('=') {
        Some((h, w)) if !h.is_empty() => (h, w.parse::<f64>().unwrap_or(1.0)),
        _ => (spec, 1.0),
    };
    match head.split_once('/') {
        Some((p, m)) => (p.to_string(), Some(m.to_string()), weight),
        None => (head.to_string(), None, weight),
    }
}

fn candidates_for_combo(
    state: &AppState,
    combo_name: &str,
    strategy: Option<&str>,
    providers: &[String],
    models: &[String],
    model_str: &str,
    parsed: &crate::model::ParsedModel,
) -> Option<Vec<Candidate>> {
    let matches_model = models.is_empty()
        || models.iter().any(|m| {
            m == model_str || crate::model::parse_model(model_str).model == *m
        });
    if !matches_model || providers.is_empty() {
        return None;
    }
    let cands: Vec<Candidate> = providers
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let (prov, mdl, weight) = parse_provider_spec(spec);
            Candidate {
                provider: prov,
                model: mdl.unwrap_or_else(|| {
                    if parsed.provider.is_some() {
                        parsed.model.clone()
                    } else {
                        model_str.to_string()
                    }
                }),
                combo: Some(combo_name.to_string()),
                position: i,
                weight,
            }
        })
        .collect();
    Some(order_candidates(state, strategy.unwrap_or("priority"), cands))
}

/// parse_model extended with the live registry: any `prefix/model` whose
/// prefix is a registered provider id resolves as provider/model even when it
/// is not in the static alias table (dynamic `openai-compatible-*` families).
fn parse_with_registry(state: &AppState, model_str: &str) -> crate::model::ParsedModel {
    let parsed = crate::model::parse_model(model_str);
    if parsed.provider.is_none() {
        if let Some(idx) = model_str.find('/') {
            let (prov, rest) = model_str.split_at(idx);
            let rest = &rest[1..];
            if !prov.is_empty() && !rest.is_empty() && state.registry.contains(prov) {
                return crate::model::ParsedModel {
                    provider: Some(prov.to_string()),
                    model: rest.to_string(),
                    extended_context: parsed.extended_context,
                    is_alias: false,
                };
            }
        }
    }
    parsed
}

/// Build the candidate list for an incoming model string.
pub fn resolve_candidates(state: &AppState, model_str: &str) -> Vec<Candidate> {
    let parsed = parse_with_registry(state, model_str);

    // 1) dashboard-managed combos take precedence over static config combos.
    for combo in state.combos.list().into_iter().filter(|c| c.enabled) {
        if let Some(cands) = candidates_for_combo(
            state,
            &combo.name,
            combo.strategy.as_deref(),
            &combo.providers,
            &combo.models,
            model_str,
            &parsed,
        ) {
            return cands;
        }
    }

    // 2) matching config combos
    for combo in &state.config.combos {
        if let Some(cands) = candidates_for_combo(
            state,
            &combo.name,
            combo.strategy.as_deref(),
            &combo.providers,
            &combo.models,
            model_str,
            &parsed,
        ) {
            return cands;
        }
    }

    let mut out: Vec<Candidate> = Vec::new();

    // 2) explicit provider prefix
    if let Some(prov) = &parsed.provider {
        out.push(Candidate {
            provider: prov.clone(),
            model: parsed.model.clone(),
            combo: None,
            position: 0,
            weight: 1.0,
        });
        // fallback chain: other keyed providers with the same wire format
        let fmt = state.config.format_for(&state.registry, prov);
        for id in state.providers_with_keys() {
            if &id == prov || out.len() >= MAX_COMBO_DEPTH_HARD {
                continue;
            }
            if state.config.format_for(&state.registry, &id) == fmt {
                out.push(Candidate {
                    provider: id,
                    model: parsed.model.clone(),
                    combo: None,
                    position: out.len(),
                    weight: 1.0,
                });
            }
        }
        return out;
    }

    // 3) bare unknown model: every keyed provider advertising it
    for id in state.providers_with_keys() {
        let advertises = state
            .models_for_provider(&id)
            .iter()
            .any(|m| m == &parsed.model);
        if advertises {
            out.push(Candidate {
                provider: id,
                model: parsed.model.clone(),
                combo: None,
                position: out.len(),
                weight: 1.0,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ComboConfig;
    use crate::state::AppState;

    #[test]
    fn strategy_aliases_normalize() {
        assert_eq!(normalize_strategy("failover"), "priority");
        assert_eq!(normalize_strategy("usage"), "least-used");
        assert_eq!(normalize_strategy("rr"), "round-robin");
        assert_eq!(normalize_strategy("bogus"), "priority");
        assert_eq!(normalize_strategy("lkgp"), "lkgp");
    }

    #[test]
    fn provider_spec_parsing() {
        let (p, m, w) = parse_provider_spec("openai/gpt-4o=2");
        assert_eq!(p, "openai");
        assert_eq!(m.as_deref(), Some("gpt-4o"));
        assert_eq!(w, 2.0);
        let (p, m, w) = parse_provider_spec("anthropic");
        assert_eq!(p, "anthropic");
        assert!(m.is_none());
        assert_eq!(w, 1.0);
    }

    #[test]
    fn explicit_provider_wins_with_fallbacks() {
        let state = AppState::for_tests(Vec::new(), None);
        // credentials: two openai-format providers with keys
        let mut st = state;
        st.config.credentials.insert("openai".into(), crate::config::ProviderCredentials { api_key: Some("k".into()), ..Default::default() });
        st.config.credentials.insert("groq".into(), crate::config::ProviderCredentials { api_key: Some("k".into()), ..Default::default() });
        st.config.credentials.insert("anthropic".into(), crate::config::ProviderCredentials { api_key: Some("k".into()), ..Default::default() });
        let cands = resolve_candidates(&st, "openai/gpt-4o");
        assert_eq!(cands[0].provider, "openai");
        assert_eq!(cands[0].model, "gpt-4o");
        // groq (same openai format) follows; anthropic (claude format) excluded
        assert!(cands.iter().any(|c| c.provider == "groq"));
        assert!(!cands.iter().any(|c| c.provider == "anthropic"));
    }

    #[test]
    fn combo_matches_and_orders() {
        let combo = ComboConfig {
            name: "code".into(),
            strategy: Some("priority".into()),
            providers: vec!["anthropic/claude-sonnet-4-5".into(), "openai/gpt-4o".into()],
            models: vec!["claude-sonnet-4-5".into()],
        };
        let mut st = AppState::for_tests(vec![combo], None);
        st.config.credentials.insert("anthropic".into(), Default::default());
        st.config.credentials.insert("openai".into(), Default::default());
        let cands = resolve_candidates(&st, "claude-sonnet-4-5");
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].provider, "anthropic");
        assert_eq!(cands[1].provider, "openai");
        assert_eq!(cands[0].combo.as_deref(), Some("code"));
    }

    #[test]
    fn bare_model_uses_advertising_providers() {
        let mut st = AppState::for_tests(Vec::new(), None);
        st.config.credentials.insert("deepseek".into(), crate::config::ProviderCredentials {
            api_key: Some("k".into()),
            model_list: vec!["deepseek-chat".into(), "custom-model-x".into()],
            ..Default::default()
        });
        // a bare model with no known prefix that only deepseek advertises
        let cands = resolve_candidates(&st, "custom-model-x");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "deepseek");
        assert_eq!(cands[0].model, "custom-model-x");
    }
}
