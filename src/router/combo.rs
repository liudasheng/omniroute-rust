//! Combo routing strategies (parity: `src/shared/constants/routingStrategies.ts`
//! + `open-sse/services/combo.ts`).
//!
//! A "combo" is an ordered list of provider targets tried until one succeeds.
//! The strategy decides the candidate ordering; MAX_COMBO_DEPTH bounds how
//! deep a chain may recurse; MAX_GLOBAL_ATTEMPTS bounds total upstream tries.

use crate::state::AppState;
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
    /// The candidate came from the scored free roster (`auto/best-free` /
    /// `auto/free` or a combo that expands one). That order *is* the ranking,
    /// so the enclosing combo's strategy must not rotate/shuffle it — but only
    /// this step is locked, never the whole combo (see `order_candidates`).
    pub order_locked: bool,
}

/// Shared round-robin cursor across combos.
static RR_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Request-size fit
// ---------------------------------------------------------------------------

/// How big the incoming request is, so an `auto/*` router can skip — and
/// within a provider choose — models that cannot hold it.
///
/// The single source of truth for the arithmetic: `chat_core` fits candidates
/// with these helpers before dispatch, and `auto_candidates` uses the same
/// ones while *building* a candidate list, so the two can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFit {
    pub prompt_tokens: i64,
    pub output_tokens: i64,
}

impl RequestFit {
    /// Nothing to judge (a request with no measurable text and no cap).
    pub fn is_empty(&self) -> bool {
        self.prompt_tokens <= 0 && self.output_tokens <= 0
    }
}

/// Output tokens a candidate must actually reserve: the client's declared cap
/// is a request for generation room, not a demand that every candidate reserve
/// it, so it is clamped to that candidate's own ceiling.
pub fn candidate_output_tokens(requested_output: i64, candidate_cap: i64) -> i64 {
    if candidate_cap > 0 && requested_output > candidate_cap {
        candidate_cap
    } else {
        requested_output.max(0)
    }
}

/// Context tokens `candidate` needs: prompt plus its own effective output
/// reservation.
pub fn candidate_required_context_tokens(prompt: i64, requested_output: i64, candidate_cap: i64) -> i64 {
    prompt
        .saturating_add(candidate_output_tokens(requested_output, candidate_cap))
        .max(0)
}

/// Can this provider/model hold the request? `true` when no fit was supplied —
/// an unknown size must never disqualify a candidate.
pub fn candidate_fits(
    state: &AppState,
    provider: &str,
    model: &str,
    fit: Option<RequestFit>,
) -> bool {
    let Some(fit) = fit.filter(|f| !f.is_empty()) else {
        return true;
    };
    let caps = state.capabilities_for_model(provider, model);
    caps.context_window >= candidate_required_context_tokens(fit.prompt_tokens, fit.output_tokens, caps.max_output_tokens)
}

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
///
/// Candidates expanded from the scored free roster carry `order_locked`: that
/// step's order *is* the ranking, so the strategy is applied to the remaining
/// (leaf) candidates only, and those are written back into their own declared
/// slots. Locking the whole combo instead would silently turn `round-robin`
/// into `priority` for any combo that merely *contains* an `auto/best-free`
/// step — which is exactly what made `coding-free` always start at candidate
/// #1.
pub fn order_candidates(state: &AppState, strategy: &str, mut cands: Vec<Candidate>) -> Vec<Candidate> {
    if cands.len() <= 1 {
        return cands;
    }
    let slots: Vec<usize> = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.order_locked)
        .map(|(i, _)| i)
        .collect();
    if slots.len() <= 1 {
        return cands;
    }
    let mut sub: Vec<Candidate> = slots.iter().map(|&i| cands[i].clone()).collect();
    match normalize_strategy(strategy) {
        "round-robin" => {
            let n = sub.len() as u64;
            let offset = RR_COUNTER.fetch_add(1, Ordering::Relaxed) % n;
            sub.rotate_left(offset as usize);
        }
        "random" => {
            use rand::seq::SliceRandom;
            sub.shuffle(&mut rand::rng());
        }
        "p2c" => {
            // power-of-two-choices over the least-loaded pair
            let len = sub.len();
            let mut rng = rand::rng();
            let a = rand::Rng::random_range(&mut rng, 0..len);
            let b = rand::Rng::random_range(&mut rng, 0..len);
            let load = |c: &Candidate| state.circuits.inflight(&c.provider);
            // The lighter of the two choices must end up at the *front* of the
            // chain, not merely swapped between two sampled slots — otherwise
            // the busiest candidate keeps its declared position and the
            // load-balancing never reaches the dispatch order.
            let lighter = if a != b && load(&sub[a]) > load(&sub[b]) { b } else { a };
            sub.swap(0, lighter);
        }
        "weighted" => {
            // Select one candidate and keep the rest in their declared order.
            //
            // The old form used `drain(..i)` to drop everything before the
            // pick, which shortened `sub` while the rewrite loop below still
            // indexed it with the original slot count — with three or more
            // candidates the rewrite panicked on the out-of-range index, and
            // the request died with a closed connection.
            let total: f64 = sub.iter().map(|c| c.weight.max(0.001)).sum();
            let mut pick = rand::Rng::random::<f64>(&mut rand::rng()) * total;
            let mut selected = 0usize;
            for (i, c) in sub.iter().enumerate() {
                pick -= c.weight.max(0.001);
                if pick <= 0.0 {
                    selected = i;
                    break;
                }
            }
            // Rotate the picked candidate to the front, keeping the others.
            sub.rotate_left(selected);
        }
        "least-used" => {
            // Lightest first: a `Reverse` here put the *busiest* candidate at
            // the head, so the strategy was sending load the wrong way.
            sub.sort_by_key(|c| state.circuits.inflight(&c.provider));
        }
        "lkgp" => {
            // Most-recent success first. `Reverse(elapsed)` put the provider
            // that had *not* succeeded for the longest time at the head, which
            // is exactly the opposite of last-known-good preference; a
            // provider that has never succeeded sorts last.
            sub.sort_by_key(|c| {
                state
                    .circuits
                    .last_success(&c.provider)
                    .map(|t| t.elapsed().as_nanos())
                    .unwrap_or(u128::MAX)
            });
        }
        "cost-optimized" => {
            sub.sort_by_key(|c| provider_cost(&c.provider));
        }
        // priority / fill-first / auto → keep declared order
        _ => {}
    }
    for (slot, idx) in slots.iter().enumerate() {
        cands[*idx] = sub[slot].clone();
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
    fit: Option<RequestFit>,
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
    let mut cands = expand_specs(state, providers, &member_model, 0, &mut chain, combo_name, fit);
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
    // The combo's declared strategy applies as-is. Steps that expanded the
    // scored free roster are marked `order_locked` while expanding, so
    // `order_candidates` protects their ranking instead of overriding the
    // whole combo — which is what used to freeze `round-robin` into
    // `priority` for every combo that merely *contained* an `auto/best-free`
    // step, making it always start at candidate #1.
    Some(order_candidates(state, strategy.unwrap_or("priority"), cands))
}

/// Does `name` refer to a combo whose own steps expand the scored free roster?
/// Retained for nested-step diagnostics; `order_locked` is now stamped during
/// expansion instead of overriding the enclosing combo's strategy.
#[allow(dead_code)]
fn free_combo_reference(state: &AppState, name: &str) -> bool {
    combo_reference(state, name)
        .map(|reference| {
            reference.providers.iter().any(|spec| {
                let bare = spec.rsplit_once('=').map(|(head, _)| head).unwrap_or(spec.as_str());
                AUTO_FREE_COMBO_NAMES.contains(&bare)
            })
        })
        .unwrap_or(false)
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
    fit: Option<RequestFit>,
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
            // The scored free roster's order *is* its ranking: mark those
            // candidates so the enclosing combo's strategy cannot rotate or
            // shuffle them away (only this step, not the whole combo).
            let roster_locked = AUTO_FREE_COMBO_NAMES.contains(&spec_name);
            for mut candidate in auto_candidates(state, spec_name, fit) {
                candidate.weight *= weight;
                candidate.combo = Some(combo_name.to_string());
                candidate.order_locked = roster_locked;
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
                for mut candidate in expand_specs(state, &nested.providers, member_model, depth + 1, chain, combo_name, fit) {
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
            order_locked: false,
        });
    }
    out
}

/// The candidate pool of one `auto/*` virtual combo.
///
/// Every provider contributes its **best** model that can actually hold this
/// request, not merely the first one it happens to list: the old behaviour
/// picked `models_for_provider(..).first()` (alphabetical) and paid no attention
/// to size, so a short prompt could land on the largest model and a long one on
/// a model that cannot fit it at all.
fn auto_candidates(state: &AppState, model_str: &str, fit: Option<RequestFit>) -> Vec<Candidate> {
    // `auto/best-free` / `auto/free` mean zero-cost only: the roster is
    // discovered (OpenCode Zen) or inferred (models.dev cost + name), so a
    // newly published free model joins the chain without any edit here.
    if AUTO_FREE_COMBO_NAMES.contains(&model_str) {
        let free = free_candidates(state, model_str);
        // The zero-cost promise is the whole point of this virtual combo: a
        // request that asks for `auto/best-free` must never be routed to a
        // paid model. When the roster is empty — its own gate parked the lane,
        // or the directory lists nothing — the honest answer is "no free
        // model available", not a silent fall-through to the paid pool.
        if free.is_empty() {
            tracing::warn!(
                combo = %model_str,
                "no free candidate is available (lane parked or roster empty); refusing to fall back to paid models"
            );
            return Vec::new();
        }
        let fitting = free
            .into_iter()
            .filter(|candidate| candidate_fits(state, &candidate.provider, &candidate.model, fit))
            .collect::<Vec<_>>();
        if !fitting.is_empty() {
            return fitting;
        }
        // Nothing fits: keep the scored chain so the caller's own fit test
        // can report the honest context-size error instead of "no model".
        return free_candidates(state, model_str);
    }
    let now = now_epoch_ms();
    let mut candidates = Vec::new();
    for provider in state.providers_with_keys() {
        let models: Vec<String> = state
            .models_for_provider(&provider)
            .into_iter()
            .filter(|model| !state.model_is_hidden(&provider, model))
            .filter(|model| candidate_fits(state, &provider, model, fit))
            .collect();
        if models.is_empty() {
            // No model of this provider can hold the request: proposing one
            // would only produce a guaranteed context failure.
            continue;
        }
        // Rank by the same composite score the free chain uses (newness has no
        // directory for these providers, so context/multimodality/recent
        // success decide), then take the provider's best.
        let Some(model) = crate::opencode_free::order_models_for(&provider, &models, now).into_iter().next()
        else {
            continue;
        };
        candidates.push(Candidate {
            provider,
            model,
            combo: Some(model_str.to_string()),
            position: candidates.len(),
            weight: 1.0,
        order_locked: false,
        });
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
    // A parked lane gate is upstream policy, not a property of one model: while
    // it is closed the zero-cost chain skips the whole Zen roster instead of
    // spending one refused round trip per id. The roster keeps listing the
    // models and the gate reopens by itself after `LANE_GATE_TTL_MS`, so a
    // recovered lane is picked up without a restart.
    if crate::opencode_free::lane_gate_active(now) {
        tracing::debug!("auto/free: OpenCode free lane parked by its own gate; skipping its roster");
    } else if let Some(provider) = zen_provider {
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
            order_locked: false,
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
            order_locked: false,
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
    resolve_candidates_with(state, model_str, None)
}

/// [`resolve_candidates`] with the incoming request's size, so `auto/*` routers
/// can choose models that actually fit it.
pub fn resolve_candidates_with(
    state: &AppState,
    model_str: &str,
    fit: Option<RequestFit>,
) -> Vec<Candidate> {
    let parsed = parse_with_registry(state, model_str);

    // Built-in auto/* models are virtual combos. The Rust build does not have
    // the original's scoring/radar service, so use the first effective model
    // from each usable provider as a deterministic candidate pool rather than
    // treating `auto` as an unknown provider.
    if AUTO_COMBO_NAMES.contains(&model_str) {
        let candidates = auto_candidates(state, model_str, fit);
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
            fit,
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
            fit,
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
        order_locked: false,
        });
        // fallback chain: other keyed providers with the same wire format
        //
        // Exception — the anonymous OpenCode Zen free lane: the caller asked
        // for that lane by name, so a failure must be reported as a failure.
        // Fanning out would (a) spend another provider's paid quota for a
        // request priced as free, and (b) chain through unreachable local
        // providers (LM Studio / Ollama default ports) whose 30s connect
        // timeouts turn a fast region 403 into a two-minute hang.
        // The OpenCode tiers are named deliberately by operators and by the
        // dashboard's provider/model pickers: an explicit `opencode-go/...` or
        // `opencode/...` request must fail with that tier's own error instead
        // of chaining through unrelated providers (which also mixed a foreign
        // `insufficient_quota` into the terminal message).
        if crate::opencode_free::is_free_lane_provider(prov) || prov == "opencode-go" {
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
                order_locked: false,
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
            order_locked: false,
            });
        }
    }
    out
}

#[cfg(test)]
mod load_order_tests {
    use super::*;

    fn cand(provider: &str) -> Candidate {
        Candidate {
            provider: provider.to_string(),
            model: "m".to_string(),
            combo: None,
            position: 0,
            weight: 1.0,
            order_locked: false,
        }
    }

    /// D3: the load-based strategies must send load *away* from a busy
    /// provider — a `Reverse` put the busiest candidate at the head.
    #[test]
    fn least_used_prefers_the_lightest_candidate() {
        let state = crate::state::AppState::for_tests(Vec::new(), None);
        // Give p0 a load the others do not have.
        let _guard_p0: Vec<_> = (0..5).map(|_| state.circuits.begin_request_guard("p0")).collect();
        state.circuits.begin_request("p1");
        let out = order_candidates(&state, "least-used", vec![cand("p0"), cand("p1"), cand("p2")]);
        let head = out[0].provider.clone();
        assert_ne!(head, "p0", "the busiest provider must not lead the chain");
        assert!(head == "p1" || head == "p2", "an idle or light provider must lead, got {head}");
    }

    #[test]
    fn lkgp_prefers_the_most_recent_success() {
        let state = crate::state::AppState::for_tests(Vec::new(), None);
        state.circuits.record_success("p2"); // p2 has the most recent success
        let out = order_candidates(&state, "lkgp", vec![cand("p0"), cand("p1"), cand("p2")]);
        assert_eq!(out[0].provider, "p2", "the most recent success must lead the chain");
    }

    #[test]
    fn p2c_moves_the_lighter_choice_to_the_front() {
        let state = crate::state::AppState::for_tests(Vec::new(), None);
        let _busy: Vec<_> = (0..8).map(|_| state.circuits.begin_request_guard("p0")).collect();
        let mut heads = std::collections::HashMap::new();
        for _ in 0..40 {
            let out = order_candidates(&state, "p2c", vec![cand("p0"), cand("p1"), cand("p2")]);
            *heads.entry(out[0].provider.clone()).or_insert(0usize) += 1;
        }
        let busiest = heads.get("p0").copied().unwrap_or(0);
        let others: usize = heads.iter().filter(|(k, _)| *k != "p0").map(|(_, v)| *v).sum();
        println!("D3/p2c heads={heads:?}");
        assert!(
            others > busiest,
            "the busiest provider must not lead the chain: {heads:?}"
        );
    }
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
    fn auto_routers_pick_a_fitting_model_instead_of_the_first_listed() {
        let _guard = crate::opencode_free::test_lock();
        let mut state = AppState::for_tests(Vec::new(), None);
        // `providers_with_keys()` only offers providers with a usable
        // credential, so give this one a key.
        state.config.credentials.insert(
            "opencode-go".into(),
            crate::config::ProviderCredentials { api_key: Some("k".into()), ..Default::default() },
        );
            // Two opencode-go models with very different windows: `grok-4.5` (500k)
        // and `kimi-k3` (1M). Alphabetically `grok-4.5` comes first, but the
        // bigger request must not pick it.
        let small = resolve_candidates_with(
            &state,
            "auto/fast",
            Some(RequestFit { prompt_tokens: 1_000, output_tokens: 1_000 }),
        );
        assert!(!small.is_empty(), "a small request resolves");
        let big = resolve_candidates_with(
            &state,
            "auto/fast",
            Some(RequestFit { prompt_tokens: 900_000, output_tokens: 64_000 }),
        );
        // Whatever each provider offers, every candidate must be able to hold
        // the request: resolution and the later dispatch filter now agree.
        for candidate in &big {
            assert!(
                candidate_fits(&state, &candidate.provider, &candidate.model, Some(RequestFit {
                    prompt_tokens: 900_000,
                    output_tokens: 64_000
                })),
                "{} / {} cannot hold 900k+64k",
                candidate.provider,
                candidate.model
            );
        }
        // The 500k-window model must be dropped for the big request...
        assert!(
            !big.iter().any(|c| c.model == "grok-4.5"),
            "grok-4.5 (500k) must not be offered for a 900k prompt: {:?}",
            big.iter().map(|c| c.model.clone()).collect::<Vec<_>>()
        );
        // ...while the small request may still use it.
        assert!(
            small.iter().any(|c| c.provider == "opencode-go"),
            "{:?}",
            small.iter().map(|c| format!("{}/{}", c.provider, c.model)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn auto_routers_skip_providers_that_cannot_hold_the_request() {
        let _guard = crate::opencode_free::test_lock();
        let state = AppState::for_tests(Vec::new(), None);
        // A provider whose every model is tiny is skipped rather than proposed
        // as a guaranteed context failure.
        let fit = RequestFit { prompt_tokens: 1_500_000, output_tokens: 8_000 };
        let candidates = resolve_candidates_with(&state, "auto/chat", Some(fit));
        for candidate in &candidates {
            assert!(
                candidate_fits(&state, &candidate.provider, &candidate.model, Some(fit)),
                "{} / {} was proposed but cannot fit",
                candidate.provider,
                candidate.model
            );
        }
    }

    #[test]
    fn both_free_ids_resolve_to_the_same_scored_chain() {
        let _guard = crate::opencode_free::test_lock();
        crate::opencode_free::install_live(vec![
            "zen-small-free".into(),
            "zen-big-free".into(),
        ]);
        let state = AppState::for_tests(Vec::new(), None);
        let free = resolve_candidates(&state, "auto/free");
        let best = resolve_candidates(&state, "auto/best-free");
        let ids = |c: &[Candidate]| c.iter().map(|x| x.model.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&free), ids(&best), "the two free ids must not diverge");
        assert_eq!(free.len(), 2);

        // ...and neither is re-ordered destructively when used as a combo step,
        // whatever strategy the combo declares (`weighted` would drop the head).
        for strategy in ["weighted", "random", "round-robin"] {
            managed_combo_with_strategy(
                &state,
                &format!("step-{strategy}"),
                strategy,
                vec!["auto/best-free".into()],
            );
            let expanded = resolve_candidates(&state, &format!("step-{strategy}"));
            assert_eq!(
                ids(&expanded),
                ids(&best),
                "strategy {strategy} must not reorder the scored free chain"
            );
        }
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

    /// Register a dashboard-managed combo with an explicit strategy.
    fn managed_combo_with_strategy(state: &AppState, name: &str, strategy: &str, providers: Vec<String>) {
        state.combos.upsert(crate::server::combos_admin::ManagedCombo {
            id: format!("combo_test_{name}"),
            name: name.to_string(),
            strategy: Some(strategy.to_string()),
            providers,
            models: vec![],
            enabled: true,
            default_model: None,
            tags: vec![],
            created_at_ms: 0,
        });
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

    /// Regression: a `round-robin` combo that also contains the scored free
    /// roster must still rotate its leaf steps. Overriding the whole combo to
    /// `priority` (the old fix for keeping the roster's ranking intact) froze
    /// the dispatch to candidate #1 for every request — `coding-free` on the
    /// remote gateway showed 15/15 requests landing on the same model.
    #[test]
    fn round_robin_still_rotates_leaf_steps_when_a_free_step_is_present() {
        let _guard = crate::opencode_free::test_lock();
        crate::opencode_free::install_live(vec!["zen-a-free".into(), "zen-b-free".into()]);
        let state = AppState::for_tests(Vec::new(), None);
        managed_combo_with_strategy(
            &state,
            "coding-free",
            "round-robin",
            vec![
                "groq/llama-3.1-8b-instant".into(),
                "groq/llama-3.1-70b-versatile".into(),
                "groq/gemma2-9b-it".into(),
                "auto/best-free".into(),
            ],
        );

        // The free roster keeps its scored order and stays at the end.
        let first = resolve_candidates(&state, "coding-free");
        let first_models: Vec<String> = first.iter().map(|c| c.model.clone()).collect();
        assert_eq!(
            &first_models[first_models.len() - 2..],
            &["zen-a-free".to_string(), "zen-b-free".to_string()],
            "the free roster must keep its scored ranking: {first_models:?}"
        );
        assert!(
            first.iter().skip(first.len() - 2).all(|c| c.order_locked),
            "free-roster candidates must be order_locked"
        );

        // …while the leaf steps rotate across calls.
        let mut heads = std::collections::HashSet::new();
        for _ in 0..6 {
            let candidates = resolve_candidates(&state, "coding-free");
            heads.insert(candidates.first().unwrap().model.clone());
        }
        assert!(
            heads.len() > 1,
            "round-robin must rotate the leaf steps, saw only {heads:?}"
        );
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
