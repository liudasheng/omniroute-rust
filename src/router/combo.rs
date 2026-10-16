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

/// Built-in auto-router model ids exposed by the original dashboard/catalog.
pub const AUTO_COMBO_NAMES: [&str; 18] = [
    "auto/best-coding",
    "auto/best-reasoning",
    "auto/best-fast",
    "auto/best-vision",
    "auto/best-chat",
    "auto/best-coding-fast",
    "auto/pro-coding",
    "auto/pro-reasoning",
    "auto/pro-vision",
    "auto/pro-chat",
    "auto/pro-fast",
    "auto/coding",
    "auto/fast",
    "auto/chat",
    "auto/claude-opus",
    "auto/claude-sonnet",
    "auto/best-free",
    "auto/free",
];

/// Auto ids that mean "zero-cost models only". They share one resolver: the
/// original catalogued `auto/best-free` but the generic `auto/*` branch picked
/// the first model of every provider, which happily routed a "free" request to
/// a paid model. Following the OpenCode free roster (see
/// [`crate::opencode_free`]) keeps the promise as the roster changes.
pub const AUTO_FREE_COMBO_NAMES: [&str; 2] = ["auto/best-free", "auto/free"];

/// Upper bound on the free chain: enough for the discovered Zen roster plus a
/// few free ids on other providers, without letting one request fan out across
/// every provider (the dispatch loop caps total attempts at
/// [`MAX_GLOBAL_ATTEMPTS`] anyway).
pub const AUTO_FREE_MAX_CANDIDATES: usize = 12;

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

#[allow(clippy::too_many_arguments)]
fn candidates_for_combo(
    state: &AppState,
    combo_name: &str,
    strategy: Option<&str>,
    providers: &[String],
    models: &[String],
    model_str: &str,
    parsed: &crate::model::ParsedModel,
    empty_matches_all: bool,
) -> Option<Vec<Candidate>> {
    let requested_name = model_str.strip_prefix("combo/").unwrap_or(model_str);
    let parsed_name = crate::model::parse_model(model_str).model;
    let matches_model = requested_name == combo_name
        || (empty_matches_all && models.is_empty())
        || (!models.is_empty() && models.iter().any(|m| m == model_str || parsed_name == *m));
    if !matches_model || providers.is_empty() {
        return None;
    }
    // A step may name a provider, a `provider/model`, another combo, or an
    // `auto/*` virtual combo: the last two expand here so an operator can build
    // `coding-free → auto/free` without copying the free roster by hand.
    let member_model = if parsed.provider.is_some() {
        parsed.model.clone()
    } else {
        model_str.to_string()
    };
    let mut chain = vec![combo_name.to_string()];
    let mut cands = expand_specs(state, providers, &member_model, 0, &mut chain, combo_name);
    // Hidden models are excluded from combo dispatch as well as catalog
    // discovery. Otherwise a failed visible target can fall through to a
    // model the operator explicitly disabled in the dashboard.
    cands.retain(|candidate| !state.model_is_hidden(&candidate.provider, &candidate.model));
    if cands.is_empty() {
        return None;
    }
    for (position, candidate) in cands.iter_mut().enumerate() {
        candidate.position = position;
    }
    Some(order_candidates(state, strategy.unwrap_or("priority"), cands))
}

/// One combo definition, as the resolver sees it.
struct ComboRef {
    providers: Vec<String>,
}

/// Look up a combo by name: dashboard-managed (enabled) first, then the
/// read-only config file. Accepts the `combo/<name>` spelling the request path
/// already supports.
fn combo_reference(state: &AppState, name: &str) -> Option<ComboRef> {
    let bare = name.strip_prefix("combo/").unwrap_or(name);
    for combo in state.combos.list().into_iter().filter(|c| c.enabled) {
        if combo.name == bare {
            return Some(ComboRef { providers: combo.providers });
        }
    }
    for combo in &state.config.combos {
        if combo.name == bare {
            return Some(ComboRef { providers: combo.providers.clone() });
        }
    }
    None
}

/// Expand a combo's step list into concrete `provider/model` candidates.
///
/// Each step is one of:
/// * `provider` or `provider/model` (optionally `=weight`) — a leaf target;
/// * an `auto/*` virtual combo id — expanded by [`auto_candidates`];
/// * another combo's name (or `combo/<name>`) — expanded recursively.
///
/// Nested expansion is bounded by [`MAX_COMBO_DEPTH`] and by the chain already
/// on the stack, so a combo that references itself (directly or through a
/// cycle) resolves to nothing extra instead of looping forever. Weights
/// multiply down the chain.
fn expand_specs(
    state: &AppState,
    specs: &[String],
    member_model: &str,
    depth: usize,
    chain: &mut Vec<String>,
    combo_name: &str,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for spec in specs {
        let (provider, model, weight) = parse_provider_spec(spec);
        let spec_name = spec
            .rsplit_once('=')
            .map(|(head, _)| head)
            .filter(|head| !head.is_empty())
            .unwrap_or(spec.as_str());
        // A registered provider always wins: `openai-compatible-*` ids can look
        // like anything, and a leaf target must never be reinterpreted.
        let is_provider = state.registry.contains(&provider);

        if !is_provider && AUTO_COMBO_NAMES.contains(&spec_name) {
            if depth >= MAX_COMBO_DEPTH {
                tracing::warn!(combo = %combo_name, step = %spec, "combo step exceeds the nesting depth; skipped");
                continue;
            }
            for mut candidate in auto_candidates(state, spec_name) {
                candidate.weight *= weight;
                candidate.combo = Some(combo_name.to_string());
                out.push(candidate);
            }
            continue;
        }

        if !is_provider {
            if let Some(nested) = combo_reference(state, spec_name) {
                if chain.iter().any(|name| name == spec_name) {
                    tracing::warn!(combo = %combo_name, step = %spec, "combo step would form a cycle; skipped");
                    continue;
                }
                if depth >= MAX_COMBO_DEPTH {
                    tracing::warn!(combo = %combo_name, step = %spec, "combo step exceeds the nesting depth; skipped");
                    continue;
                }
                chain.push(spec_name.to_string());
                for mut candidate in expand_specs(state, &nested.providers, member_model, depth + 1, chain, combo_name) {
                    candidate.weight *= weight;
                    out.push(candidate);
                }
                chain.pop();
                continue;
            }
        }

        out.push(Candidate {
            provider,
            model: model.unwrap_or_else(|| member_model.to_string()),
            combo: Some(combo_name.to_string()),
            position: out.len(),
            weight,
        });
    }
    out
}

/// The candidate pool of one `auto/*` virtual combo.
fn auto_candidates(state: &AppState, model_str: &str) -> Vec<Candidate> {
    // `auto/best-free` / `auto/free` mean zero-cost only: the roster is
    // discovered (OpenCode Zen) or inferred (models.dev cost + name), so a
    // newly published free model joins the chain without any edit here.
    if AUTO_FREE_COMBO_NAMES.contains(&model_str) {
        let free = free_candidates(state, model_str);
        if !free.is_empty() {
            return free;
        }
    }
    let mut candidates = Vec::new();
    for provider in state.providers_with_keys() {
        if let Some(model) = state.models_for_provider(&provider).into_iter().next() {
            candidates.push(Candidate {
                provider,
                model,
                combo: Some(model_str.to_string()),
                position: candidates.len(),
                weight: 1.0,
            });
        }
    }
    candidates
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

/// Is this `provider/model` a zero-cost model?
///
/// Two sources, in order of authority: the OpenCode Zen free lane's discovered
/// roster (live directory ∩ models.dev zero cost, minus learned denials), and
/// for every other provider the model id (`-free` suffix / `free` name) or the
/// models.dev cost verdict.
pub fn is_free_model(provider: &str, model: &str) -> bool {
    if crate::opencode_free::is_free_lane_provider(provider) {
        return crate::opencode_free::models().iter().any(|m| m == model);
    }
    if crate::opencode_free::is_free_name(model) {
        return true;
    }
    crate::model_catalog::lookup(provider, model)
        .map(|entry| entry.free == Some(true))
        .unwrap_or(false)
}

/// Epoch milliseconds, for the roster's age-based ranking.
fn now_epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Candidate pool for `auto/free`: the Zen anonymous lane first (zero cost and
/// no credential), then any other provider's free ids. Models this gateway has
/// already used successfully on the free lane sort first, so the chain starts
/// from a known-good model instead of whatever sorts alphabetically.
pub fn free_candidates(state: &AppState, combo_name: &str) -> Vec<Candidate> {
    let now = now_epoch_ms();
    let mut candidates: Vec<Candidate> = Vec::new();

    // Anonymous Zen lane. Prefer the provider the operator actually
    // configured: a configured key first, then a base-URL override. The
    // executor falls back to the literal `public` credential when no key is
    // set, so a keyless deployment still routes here and the plain `opencode`
    // id is the last resort.
    let free_lane_ids = ["opencode", "opencode-zen"];
    let mut zen_provider: Option<&str> = None;
    let mut best: Option<(u8, i32)> = None;
    for (index, id) in free_lane_ids.iter().enumerate() {
        if !state.registry.contains(id) {
            continue;
        }
        let registry_base = state.registry.get(id).map(|entry| entry.base_url.clone());
        let base_overridden = state
            .base_url_for(&state.registry, id)
            .map(|base| Some(base) != registry_base)
            .unwrap_or(false);
        let score = if state.api_key_for(id).is_some() {
            2
        } else if base_overridden {
            1
        } else {
            0
        };
        let rank = (score, -(index as i32));
        if best.is_none_or(|current| rank > current) {
            best = Some(rank);
            zen_provider = Some(id);
        }
    }
    if let Some(provider) = zen_provider {
        // Composite score: newest / widest context / most modalities, with a
        // small reliability nudge — see `opencode_free::rank_score`. Recomputed
        // here on every resolve, so the chain follows the roster continuously.
        let roster = crate::opencode_free::order_roster(&crate::opencode_free::models(), now);
        for model in roster {
            if state.model_is_hidden(provider, &model) {
                continue;
            }
            candidates.push(Candidate {
                provider: provider.to_string(),
                model,
                combo: Some(combo_name.to_string()),
                position: candidates.len(),
                weight: 1.0,
            });
            if candidates.len() >= AUTO_FREE_MAX_CANDIDATES {
                return candidates;
            }
        }
    }

    // Free ids on every other usable provider, scored the same way (they have
    // no directory of their own, so their newness component is 0).
    for provider in state.providers_with_keys() {
        if crate::opencode_free::is_free_lane_provider(&provider) {
            continue;
        }
        let free: Vec<String> = state
            .models_for_provider(&provider)
            .into_iter()
            .filter(|model| is_free_model(&provider, model) && !state.model_is_hidden(&provider, model))
            .collect();
        for model in crate::opencode_free::order_models_for(&provider, &free, now) {
            candidates.push(Candidate {
                provider: provider.clone(),
                model,
                combo: Some(combo_name.to_string()),
                position: candidates.len(),
                weight: 1.0,
            });
            if candidates.len() >= AUTO_FREE_MAX_CANDIDATES {
                return candidates;
            }
        }
    }
    candidates
}

/// Build the candidate list for an incoming model string.
pub fn resolve_candidates(state: &AppState, model_str: &str) -> Vec<Candidate> {
    let parsed = parse_with_registry(state, model_str);

    // Built-in auto/* models are virtual combos. The Rust build does not have
    // the original's scoring/radar service, so use the first effective model
    // from each usable provider as a deterministic candidate pool rather than
    // treating `auto` as an unknown provider.
    if AUTO_COMBO_NAMES.contains(&model_str) {
        let candidates = auto_candidates(state, model_str);
        if !candidates.is_empty() {
            return order_candidates(state, "priority", candidates);
        }
    }

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
            false,
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
            true,
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
        //
        // Exception — the anonymous OpenCode Zen free lane: the caller asked
        // for that lane by name, so a failure must be reported as a failure.
        // Fanning out would (a) spend another provider's paid quota for a
        // request priced as free, and (b) chain through unreachable local
        // providers (LM Studio / Ollama default ports) whose 30s connect
        // timeouts turn a fast region 403 into a two-minute hang.
        if crate::opencode_free::is_free_lane_provider(prov) {
            return out;
        }
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
    fn free_names_are_advertised_and_alias_each_other() {
        assert!(AUTO_COMBO_NAMES.contains(&"auto/free"));
        assert!(AUTO_FREE_COMBO_NAMES.contains(&"auto/best-free"));
    }

    #[test]
    fn free_auto_combos_resolve_to_the_discovered_zero_cost_roster() {
        let _guard = crate::opencode_free::test_lock();
        // The discovered roster is process-wide; pin it for this test.
        crate::opencode_free::install_live(vec![
            "zen-alpha-free".into(),
            "zen-beta-free".into(),
            "zen-paid".into(),
        ]);
        crate::opencode_free::record_verdict("zen-beta-free", "opencode", 200, "");
        let state = AppState::for_tests(Vec::new(), None);

        for name in AUTO_FREE_COMBO_NAMES {
            let candidates = resolve_candidates(&state, name);
            let models: Vec<String> = candidates.iter().map(|c| c.model.clone()).collect();
            assert!(!candidates.is_empty(), "{name} resolved to nothing");
            assert!(
                models.contains(&"zen-alpha-free".to_string()),
                "{name}: {models:?}"
            );
            assert!(
                !models.contains(&"zen-paid".to_string()),
                "{name} must never spend a paid model: {models:?}"
            );
            // A model this gateway already answered with sorts first.
            assert_eq!(models[0], "zen-beta-free", "{name}: {models:?}");
            assert!(candidates.iter().all(|c| c.combo.as_deref() == Some(name)));
        }
    }

    /// Register a dashboard-managed combo (the dashboard path matches a combo
    /// by its exact name, unlike config combos which double as catch-alls).
    fn managed_combo(state: &AppState, name: &str, providers: Vec<String>) {
        state.combos.upsert(crate::server::combos_admin::ManagedCombo {
            id: format!("combo_test_{name}"),
            name: name.to_string(),
            strategy: Some("priority".into()),
            providers,
            models: vec![],
            enabled: true,
            default_model: None,
            tags: vec![],
            created_at_ms: 0,
        });
    }

    #[test]
    fn a_combo_step_can_reference_an_auto_virtual_combo() {
        let _guard = crate::opencode_free::test_lock();
        crate::opencode_free::install_live(vec!["zen-one-free".into(), "zen-two-free".into()]);
        // `coding-free → auto/free`: the free roster is spliced in as steps.
        let state = AppState::for_tests(Vec::new(), None);
        managed_combo(&state, "coding-free", vec!["auto/free".into()]);
        let candidates = resolve_candidates(&state, "coding-free");
        let models: Vec<String> = candidates.iter().map(|c| c.model.clone()).collect();
        assert!(models.contains(&"zen-one-free".to_string()), "{models:?}");
        assert!(models.contains(&"zen-two-free".to_string()), "{models:?}");
        // The virtual id itself is never dispatched as a provider.
        assert!(
            candidates.iter().all(|c| state.registry.contains(&c.provider)),
            "{:?}",
            candidates.iter().map(|c| c.provider.clone()).collect::<Vec<_>>()
        );
        assert!(candidates.iter().all(|c| c.combo.as_deref() == Some("coding-free")));
    }

    #[test]
    fn a_combo_step_can_reference_another_combo() {
        let state = AppState::for_tests(Vec::new(), None);
        managed_combo(&state, "leaf-combo", vec!["openai/gpt-4o".into()]);
        managed_combo(
            &state,
            "parent-combo",
            vec!["leaf-combo".into(), "groq/llama-3.1-8b-instant=2".into()],
        );
        let candidates = resolve_candidates(&state, "parent-combo");
        let ids: Vec<String> = candidates.iter().map(|c| format!("{}/{}", c.provider, c.model)).collect();
        assert_eq!(ids[0], "openai/gpt-4o", "{ids:?}");
        assert!(ids.contains(&"groq/llama-3.1-8b-instant".to_string()), "{ids:?}");
        // the weight on the referencing step scales the expanded leaves
        let weighted = candidates.iter().find(|c| c.provider == "groq").unwrap();
        assert_eq!(weighted.weight, 2.0);
    }

    #[test]
    fn nested_combo_cycles_terminate_and_are_skipped() {
        let state = AppState::for_tests(Vec::new(), None);
        // a → b → a, plus a plain tail so the cycle is not the only step.
        managed_combo(&state, "cycle-a", vec!["cycle-b".into(), "openai/gpt-4o".into()]);
        managed_combo(&state, "cycle-b", vec!["cycle-a".into()]);
        let candidates = resolve_candidates(&state, "cycle-a");
        let ids: Vec<String> = candidates.iter().map(|c| c.model.clone()).collect();
        assert_eq!(ids, vec!["gpt-4o".to_string()], "{ids:?}");
    }

    #[test]
    fn nesting_is_bounded_by_max_combo_depth() {
        let state = AppState::for_tests(Vec::new(), None);
        // Three nested references are allowed (MAX_COMBO_DEPTH); a fourth is
        // skipped instead of looping, so the deep leaf never dispatches.
        managed_combo(&state, "d0", vec!["d1".into()]);
        managed_combo(&state, "d1", vec!["d2".into()]);
        managed_combo(&state, "d2", vec!["d3".into()]);
        managed_combo(&state, "d3", vec!["d4".into()]);
        managed_combo(&state, "d4", vec!["openai/gpt-4o".into()]);
        let candidates = resolve_candidates(&state, "d0");
        assert!(
            candidates.is_empty(),
            "a chain deeper than MAX_COMBO_DEPTH must resolve to nothing, got {:?}",
            candidates.iter().map(|c| c.provider.clone()).collect::<Vec<_>>()
        );
        // ...and a chain within the bound does reach its leaf.
        managed_combo(&state, "e0", vec!["e1".into()]);
        managed_combo(&state, "e1", vec!["e2".into()]);
        managed_combo(&state, "e2", vec!["openai/gpt-4o".into()]);
        let shallow = resolve_candidates(&state, "e0");
        let ids: Vec<String> = shallow.iter().map(|c| c.model.clone()).collect();
        assert_eq!(ids, vec!["gpt-4o".to_string()], "{ids:?}");
    }

    #[test]
    fn free_model_detection_uses_the_roster_then_the_name_then_metadata() {
        let _guard = crate::opencode_free::test_lock();
        crate::opencode_free::install_live(vec!["roster-free".into()]);
        // On the Zen lane only the discovered roster counts: a paid id with a
        // `-free`-looking name must not slip through.
        assert!(is_free_model("opencode", "roster-free"));
        assert!(!is_free_model("opencode", "not-listed-free"));
        // Elsewhere the name rule and the models.dev cost verdict still apply.
        assert!(is_free_model("openrouter", "some-model-free"));
        assert!(!is_free_model("openai", "gpt-5.2"));
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

    #[test]
    fn combo_skips_hidden_fallback_models() {
        let dir = tempfile::tempdir().unwrap();
        let combo = ComboConfig {
            name: "code".into(),
            strategy: Some("priority".into()),
            providers: vec!["openai/gpt-4o".into(), "groq/gpt-4o".into()],
            models: vec![],
        };
        let mut st = AppState::for_tests(vec![combo], Some(dir.path().to_path_buf()));
        st.config.credentials.insert("openai".into(), crate::config::ProviderCredentials {
            api_key: Some("k-openai".into()),
            ..Default::default()
        });
        st.config.credentials.insert("groq".into(), crate::config::ProviderCredentials {
            api_key: Some("k-groq".into()),
            ..Default::default()
        });
        st.provider_connections.upsert(crate::server::providers_admin::ProviderConnection {
            id: "hidden-openai-model".into(),
            provider: "openai".into(),
            name: "OpenAI".into(),
            api_key: Some("k-openai".into()),
            base_url: None,
            api_type: None,
            model_list: vec!["gpt-4o".into()],
            synced_models: Vec::new(),
            model_metadata: std::collections::HashMap::new(),
            synced_at_ms: 0,
            hidden_models: vec!["gpt-4o".into()],
            enabled: true,
            created_at_ms: 0,
        });

        let cands = resolve_candidates(&st, "code");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "groq");
        assert_eq!(cands[0].position, 0);
    }
}
