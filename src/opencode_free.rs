//! OpenCode Zen **anonymous free lane**: live model-directory discovery, the
//! models.dev free decision, learned per-model denials and the wire contract
//! the free tier actually enforces.
//!
//! Ported from the DSH community plugins `opencode2dsh`
//! (`packages/plugin/src/adapter/catalog.ts`, `adapter/ids.ts`,
//! `adapter/messages.ts`) and `dsh-opencode-zen`. The reference runs as one
//! DSH adapter; here the same three ideas are gateway-level so any
//! OpenAI-compatible client inherits them:
//!
//! * **S1 live directory** — `GET {zen}/v1/models` with the literal
//!   `Authorization: Bearer public` credential: the in-sale roster, free *and*
//!   paid, and the only part that changes without a redeploy.
//! * **S2 free decision** — `https://models.dev/api.json` `opencode` section:
//!   a model is free iff it is not deprecated and `cost.input == 0 &&
//!   cost.output == 0`. The `-free` name is only a fallback while the metadata
//!   cannot speak, because a delisted id can keep a `-free` suffix forever
//!   (the reference's `deepseek-v4-flash-free` case).
//! * **S3 verified bootstrap** — a compile-time list of ids that answered 200
//!   on the anonymous lane, used only when the live fetch has not succeeded.
//!
//! On top of the port, the gateway learns from its own traffic
//! ([`record_verdict`]): a region-blocked, protocol-unsupported, delisted or
//! rate-limited model is denied for a bounded window instead of being retried
//! forever, and a success clears the denial. That is the "quota-aware" half —
//! the free lane's budget is per-IP/per-session, so the honest signal is the
//! upstream status, not a balance endpoint (OpenCode publishes none).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// Upstream base of the Zen (non-Go) tier (the model *directory* still lives
/// here, and paid/Go traffic keeps using it).
pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen";
/// Where the free tier actually serves inference since 2026-10-14: the
/// models.dev catalog advertises the `opencode` provider at
/// `.../inference/v1`, the official client posts there, and the very same body
/// on `/zen/v1` is refused. See [`inference_url`].
pub const INFERENCE_BASE_URL: &str = "https://opencode.ai/inference";
/// Literal credential of the anonymous free lane (reference:
/// `anonymousZenKey = "public"`).
pub const ANONYMOUS_KEY: &str = "public";
/// Cached live-directory filename inside the data dir.
pub const CACHE_FILE: &str = "opencode-free.json";

/// The retired free-lane inference base (before the 2026-10-14 migration).
pub const LEGACY_ZEN_INFERENCE_BASE: &str = "https://opencode.ai/zen/v1";
/// What replaces it: the inference base that serves `/chat/completions`,
/// `/responses`, `/messages` **and** `/models` in one place (the narrower
/// `/inference/openai/v1` returns 404 for `/models` and `/messages`, which
/// would break model sync and Claude-format models).
pub const INFERENCE_API_BASE: &str = "https://opencode.ai/inference/v1";

/// Normalize a *configured* base URL for an OpenCode tier.
///
/// `opencode` / `opencode-zen` moved to the inference surface on 2026-10-14, so
/// a base URL still pinned to the retired `/zen/v1` endpoint would keep the
/// gateway (and its model sync) on a surface the free tier no longer serves.
/// Only that exact legacy base is rewritten; any other operator-configured base
/// is returned untouched (`None`), and the Go tier is never touched.
pub fn normalize_base(provider: &str, base: &str) -> Option<String> {
    if !is_free_lane_provider(provider) {
        return None;
    }
    if base.trim().trim_end_matches('/') != LEGACY_ZEN_INFERENCE_BASE {
        return None;
    }
    Some(INFERENCE_API_BASE.to_string())
}

/// Rewrite a legacy Zen free-lane URL onto the inference surface.
///
/// Upstream moved the free tier off `/zen/v1` on 2026-10-14: the client takes
/// its base URL from the models.dev catalog (`https://opencode.ai/inference/v1`)
/// and the same request is refused with `403 FreeTierError` on `/zen/v1`.
/// Paid models and `opencode-go` keep their registry base, so only free-lane
/// URLs are rewritten here.
///
/// `/zen/v1/chat/completions` → `/inference/v1/chat/completions`,
/// `/zen/v1/responses` → `/inference/v1/responses`,
/// `/zen/v1/messages` → `/inference/v1/messages`.
pub fn inference_url(url: &str) -> Option<String> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let rest = base.strip_prefix("https://opencode.ai/zen/v1")?;
    let mapped = match rest {
        "/chat/completions" => format!("{INFERENCE_API_BASE}/chat/completions"),
        "/responses" => format!("{INFERENCE_API_BASE}/responses"),
        "/messages" => format!("{INFERENCE_API_BASE}/messages"),
        _ => return None,
    };
    Some(match query {
        Some(query) if !query.is_empty() => format!("{mapped}?{query}"),
        _ => mapped,
    })
}

/// Minimal parameter schema for an injected gate tool.
///
/// Since 2026-10-14 the gate also inspects parameter schemas: a declared
/// `read` or `bash` with `properties: {}` is refused once the lane is open,
/// while the client's real argument names (`read{filePath}`, `bash{command}`)
/// pass. Names outside the quartet keep the empty schema, which still passes.
pub fn placeholder_parameters(name: &str) -> serde_json::Value {
    let (property, required) = match name {
        "bash" => ("command", "command"),
        "read" => ("filePath", "filePath"),
        "glob" | "grep" => ("pattern", "pattern"),
        "write" | "edit" => ("filePath", "filePath"),
        _ => return serde_json::json!({"type": "object", "properties": {}}),
    };
    serde_json::json!({
        "type": "object",
        "properties": { property: {"type": "string"} },
        "required": [required]
    })
}

/// Providers that share the Zen free lane.
pub fn is_free_lane_provider(provider: &str) -> bool {
    matches!(provider, "opencode" | "opencode-zen")
}

/// The alias to advertise when neither name has a connection but the lane is
/// still reachable with the literal `public` credential.
///
/// Both names resolve to the same base URL and catalogue, so exactly one may be
/// presented; a real connection always wins over this default.
pub const CANONICAL_LANE_PROVIDER: &str = "opencode";

/// Every OpenCode tier (the anonymous Zen lane and the Go subscription tier).
pub fn is_opencode_tier(provider: &str) -> bool {
    is_free_lane_provider(provider) || provider == "opencode-go"
}

/// Free-lane blocklist (reference `FREE_LANE_BLOCKLIST`): ids the live chat
/// lane can never serve even though the directory still lists them.
/// `exo-free` 410 ModelDeprecated, `jev-1.13-free` 400
/// ModelProtocolUnsupported, `ling-3.0-flash-fin-free` 400 unavailable,
/// `muse-spark-1.{2,3}-contributor-free` 403 RegionError. Verified again from
/// this host on 2026-10-16 for the last three.
pub const FREE_LANE_BLOCKLIST: &[&str] = &[
    "exo-free",
    "jev-1.13-free",
    "ling-3.0-flash-fin-free",
    "muse-spark-1.2-contributor-free",
    "muse-spark-1.3-contributor-free",
];

/// Compile-time verified ids (reference `staticFreeModels`): the bootstrap
/// roster used while the live directory is pending or unreachable.
pub const STATIC_VERIFIED_FREE: &[&str] = &[
    "big-pickle",
    "mimo-v2.6-flash-free",
    "nemotron-3.5-lightning-free",
];

/// Tool names the anonymous lane's agent-shape gate requires. Reference
/// `FREE_LANE_GATE_TOOL_NAMES`; re-probed 2026-10-16 from this host:
/// `bash`+`read` → 200 (5/5), `read` alone → 200, `bash` alone → 403,
/// no tools → 403, non-streaming → 403.
pub const GATE_TOOL_NAMES: [&str; 2] = ["bash", "read"];

/// Cap of the per-conversation session memo (reference `SESSION_MEMO_LIMIT`).
pub const SESSION_MEMO_LIMIT: usize = 1024;

/// What the free tier's gate says when it refuses a request outright.
///
/// Live-probed 2026-10-17: the Zen lane started answering this to *every*
/// shape this gateway can build — the anonymous `public` credential and the
/// subscription key alike, with and without the `ses_` session headers, the
/// agent tool gate, and either `x-opencode-client` value. It is therefore a
/// statement about the lane, not about one model, and it is not fixable from
/// here; the gateway's job is to stop misreporting it.
pub const LANE_GATE_MARKER: &str = "can only be used from within opencode";

/// How long a gate refusal parks the automatic zero-cost chain. Bounded, so a
/// reopened gate is picked up without a restart; long enough that a closed one
/// does not cost one refused round trip per request.
pub const LANE_GATE_TTL_MS: u128 = 300_000;

/// Is this upstream answer the lane's own gate refusing the request shape?
///
/// The literal is the common case, but a differently-worded refusal from the
/// same lane is the same verdict: a 403 the gateway forwards verbatim is
/// rendered by agent clients as "invalid API key", which is the wrong
/// diagnosis for a lane that is simply closed. So the match is by *phrase*
/// rather than by the exact sentence, and any 403 from an OpenCode tier that
/// mentions the client or the gate is treated as a lane refusal.
pub fn is_lane_gate_refusal(status: u16, body: &str) -> bool {
    if status != 403 {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    if lowered.contains(LANE_GATE_MARKER) {
        return true;
    }
    // Other phrasings of the same gate: the lane refuses anything that is
    // not coming from the official client.
    [
        "within opencode",
        "opencode client",
        "only be used from",
        "not authorized to use this model",
        "free models may train",
    ]
    .iter()
    .any(|m| lowered.contains(m))
}

/// Park the lane after its gate refused us; returns the new deadline.
pub fn note_lane_gate(now: u128) -> u128 {
    let until = now.saturating_add(LANE_GATE_TTL_MS);
    write_state(|state| {
        state.lane_gate_until_ms = state.lane_gate_until_ms.max(until);
    });
    until
}

/// Does this upstream response mean the free budget is exhausted, rather than
/// one model being briefly rate-limited?
///
/// This is deliberately a *lane-wide* signal, so it must not be turned into a
/// per-model denial: a sibling model can still serve (the OpenCode client has
/// been observed answering `flint-40b-free` while other free ids return 429).
pub fn is_free_usage_limit(status: u16, body: &str) -> bool {
    if status != 429 {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    lowered.contains("freeusagelimit") || lowered.contains("free usage limit")
}

/// Until when the lane's gate is parked, or `None` while it is open.
pub fn lane_gate_until(now: u128) -> Option<u128> {
    let until = snapshot().lane_gate_until_ms;
    (until > now).then_some(until)
}

/// Is the lane's gate parked right now?
pub fn lane_gate_active(now: u128) -> bool {
    lane_gate_until(now).is_some()
}

/// Sentinel expiry for a denial that never lifts on its own (blocklist / paid /
/// deprecated). Kept inside `u64` so the on-disk cache stays plain JSON.
pub const PERMANENT_MS: u128 = u64::MAX as u128;

/// How long a successful free-lane call keeps contributing to the ranking
/// score. Without a ceiling, one success months ago would outrank every newer
/// model forever; the upstream roster turns over far faster than that.
pub const VERIFIED_TTL_MS: u128 = 24 * 3_600_000;

/// How long the lane stays on the anonymous credential after the workspace's
/// "Allow free endpoints" opt-in rejects a keyed call. Long enough not to burn a
/// failing request on every turn, short enough that re-enabling the opt-in in
/// the console is picked up on its own.
pub const CONSENT_RETRY_MS: u128 = 6 * 3_600_000;

/// Marker the upstream uses when a keyed free call needs the workspace opt-in.
pub const CONSENT_MARKER: &str = "free models may train on request data";

/// How often the live directory is re-read by default. The roster is a
/// slow-moving list (~7 KB); per-request availability learning and the
/// read-time ranking both keep working between refreshes, so 12h is enough.
pub const DEFAULT_REFRESH_SECONDS: u64 = 12 * 3_600;

// ---------------------------------------------------------------------------
// Composite free-chain ranking
// ---------------------------------------------------------------------------
//
// The chain order is a live score, not a static list: it is recomputed on every
// resolve from four components, so a newly published model, a bigger context
// window or a newly multimodal sibling changes the order without an edit here.
//
//   newness    1 - age(first seen in our directory) / NEWNESS_WINDOW
//   context    min(context_window / CONTEXT_REFERENCE, 1)
//   modalities 0.5 * accepts image + 0.5 * accepts pdf
//   verified   1 - age(last 2xx from this gateway) / VERIFIED_TTL_MS
//
// `total` is their weighted sum. The three published criteria dominate; the
// verification term is a 10% reliability nudge, so a model this gateway just
// answered with does not outrank a strictly newer and larger one — it only
// breaks near-ties in favour of something known to work.

/// Weight of "how recently this id first appeared in our directory".
pub const WEIGHT_NEWNESS: f64 = 0.40;
/// Weight of the context window, normalised against [`CONTEXT_REFERENCE`].
pub const WEIGHT_CONTEXT: f64 = 0.30;
/// Weight of multimodal input support.
pub const WEIGHT_MODALITIES: f64 = 0.20;
/// Weight of a recent successful call through this gateway.
pub const WEIGHT_VERIFIED: f64 = 0.10;
/// Context window that scores a full `1.0` on the context component.
pub const CONTEXT_REFERENCE: i64 = 1_000_000;
/// Age at which the newness component reaches zero.
pub const NEWNESS_WINDOW_MS: u128 = 7 * 24 * 3_600_000;

/// One model's ranking breakdown (also the payload of the status endpoint).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct RankScore {
    pub newness: f64,
    pub context: f64,
    pub modalities: f64,
    pub verified: f64,
    pub total: f64,
    /// Context window the score was computed from (0 when unknown).
    pub context_window: i64,
}

impl RankScore {
    /// Which component carries the score — a one-word explanation for the UI.
    pub fn reason(&self) -> &'static str {
        let components = [
            (self.newness * WEIGHT_NEWNESS, "newness"),
            (self.context * WEIGHT_CONTEXT, "context"),
            (self.modalities * WEIGHT_MODALITIES, "multimodal"),
            (self.verified * WEIGHT_VERIFIED, "verified"),
        ];
        let best = components
            .iter()
            .copied()
            .fold((0.0f64, "known"), |acc, item| if item.0 > acc.0 { item } else { acc });
        if best.0 <= 0.0 {
            "known"
        } else {
            best.1
        }
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Score one `provider/model` for the free chain. `now` is injected so tests
/// and the ranking are reproducible.
pub fn rank_score(provider: &str, model: &str, now: u128) -> RankScore {
    let state = snapshot();
    let first_seen = state.first_seen.get(model).copied().unwrap_or(0);
    let newness = if first_seen == 0 {
        // Never seen in a live fetch (static bootstrap list, or a non-Zen
        // provider): no newness evidence either way.
        0.0
    } else {
        (1.0 - (now.saturating_sub(first_seen) as f64) / (NEWNESS_WINDOW_MS as f64)).clamp(0.0, 1.0)
    };
    let entry = crate::model_catalog::lookup(provider, model);
    let context_window = entry.as_ref().map(|e| e.context_window).unwrap_or(0);
    let context = if context_window <= 0 {
        0.0
    } else {
        (context_window as f64 / CONTEXT_REFERENCE as f64).clamp(0.0, 1.0)
    };
    let modalities = entry
        .as_ref()
        .map(|e| {
            let has_image = e.modalities.iter().any(|m| m == "image");
            let has_pdf = e.modalities.iter().any(|m| m == "pdf");
            0.5 * f64::from(has_image) + 0.5 * f64::from(has_pdf)
        })
        .unwrap_or(0.0);
    let verified_at = state.verified.get(model).copied().unwrap_or(0);
    let verified = if verified_at == 0 {
        0.0
    } else {
        (1.0 - (now.saturating_sub(verified_at) as f64) / (VERIFIED_TTL_MS as f64)).clamp(0.0, 1.0)
    };
    let total = WEIGHT_NEWNESS * newness
        + WEIGHT_CONTEXT * context
        + WEIGHT_MODALITIES * modalities
        + WEIGHT_VERIFIED * verified;
    RankScore {
        newness: round2(newness),
        context: round2(context),
        modalities: round2(modalities),
        verified: round2(verified),
        total: round2(total),
        context_window,
    }
}

/// Order models by [`rank_score`], highest first, ties broken alphabetically so
/// the chain is stable between recomputations.
pub fn order_models_for(provider: &str, models: &[String], now: u128) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = models
        .iter()
        .map(|model| (rank_score(provider, model, now).total, model))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(b.1))
    });
    scored.into_iter().map(|(_, model)| model.clone()).collect()
}

/// Order the Zen free roster by the composite score.
pub fn order_roster(models: &[String], now: u128) -> Vec<String> {
    order_models_for("opencode", models, now)
}

/// One-word reason for a model's placement (status endpoint).
pub fn rank_reason(model: &str) -> &'static str {
    rank_score("opencode", model, now_epoch_ms()).reason()
}

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

// ---------------------------------------------------------------------------
// Wire contract: ids, session, gate shape
// ---------------------------------------------------------------------------

/// `base62Fixed` (reference `ids.ts`): big-endian base62 of `value`, exactly
/// `width` characters.
pub fn base62_fixed(mut value: u128, width: usize) -> String {
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = BASE62[(value % 62) as usize];
        value /= 62;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Per-millisecond counter shared by every OpenCode-shaped id, mirroring
/// upstream `packages/schema/src/identifier.ts` (the first id in a millisecond
/// has counter 1).
fn oc_id_clock() -> &'static Mutex<(u64, u64)> {
    static CLOCK: OnceLock<Mutex<(u64, u64)>> = OnceLock::new();
    CLOCK.get_or_init(|| Mutex::new((0, 0)))
}

fn next_oc_counter(now_ms: u128) -> u64 {
    let now = now_ms.min(u128::from(u64::MAX)) as u64;
    let mut clock = oc_id_clock().lock().unwrap_or_else(|e| e.into_inner());
    if clock.0 != now {
        clock.0 = now;
        clock.1 = 0;
    }
    clock.1 = clock.1.wrapping_add(1);
    clock.1
}

/// Encode the 48-bit id prefix. `ses_` ids invert the word (descending);
/// `msg_` ids keep it ascending, exactly as the official client does.
fn oc_shape_hex(now_ms: u128, counter: u64, descending: bool) -> String {
    let counter = u128::from(counter % 64);
    let raw = ((now_ms << 12) + counter) & 0xffff_ffff_ffff;
    let value = if descending { (!raw) & 0xffff_ffff_ffff } else { raw };
    format!("{value:012x}")
}

/// The 12 hex characters a genuine OpenCode client emits in a `ses_` id
/// (reference `invertedTimestampHex`): the millisecond clock shifted left 12
/// bits, a per-millisecond counter in the low bits, the whole word inverted so
/// ids sort newest-first as plain strings.
///
/// The free tier reads the low 12 bits back as a counter and accepts only
/// `[0, 63]`, so the counter is clamped into that window. Anything wider is
/// rejected with 403 `FreeTierError`.
pub fn inverted_timestamp_hex(now_ms: u128, counter: u64) -> String {
    oc_shape_hex(now_ms, counter, true)
}

/// Ascending counterpart used by `msg_` ids.
pub fn ascending_timestamp_hex(now_ms: u128, counter: u64) -> String {
    oc_shape_hex(now_ms, counter, false)
}

fn random_base62_tail() -> String {
    use rand::Rng as _;
    let mut rng = rand::rng();
    (0..14)
        .map(|_| BASE62[rng.random_range(0..BASE62.len())] as char)
        .collect()
}

/// One 26-character OpenCode id body: the 12-hex timestamp prefix plus 14
/// random base62 characters. `descending` is true for `ses_`, false for `msg_`.
pub fn new_oc_shape_id(descending: bool) -> String {
    let now = now_epoch_ms();
    let counter = next_oc_counter(now);
    format!(
        "{}{}",
        oc_shape_hex(now, counter, descending),
        random_base62_tail()
    )
}

/// A fresh `ses_` id carrying the inverted-timestamp prefix the free tier
/// validates.
pub fn new_session_id() -> String {
    format!("ses_{}", new_oc_shape_id(true))
}

/// A fresh `msg_` id carrying the ascending-timestamp prefix the official
/// client uses for request ids.
pub fn new_request_id() -> String {
    format!("msg_{}", new_oc_shape_id(false))
}

/// The four header names the official client uses for the same session value.
pub const SESSION_HEADER_NAMES: [&str; 4] = [
    "x-opencode-session",
    "x-opencode-session-id",
    "x-session-id",
    "x-session-affinity",
];

/// Point every session header at `session`, adding any the caller did not set.
/// This is the shared header contract for the free lane, the connection probe
/// and the model-directory refresh.
pub fn set_session_headers(headers: &mut Vec<(String, String)>, session: &str) {
    for name in SESSION_HEADER_NAMES {
        match headers
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = session.to_string(),
            None => headers.push((name.to_string(), session.to_string())),
        }
    }
}

/// Remove one header case-insensitively (the inference surface rejects the
/// mere presence of `x-opencode-request`).
pub fn remove_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
}

fn canonical_session_with_counter(seed: &str, now_ms: u128, counter: u64) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(format!("ses\u{0}{seed}").as_bytes());
    let mut tail = [0u8; 16];
    tail.copy_from_slice(&digest[6..22]);
    let random_part = base62_fixed(u128::from_be_bytes(tail), 14);
    format!("ses_{}{random_part}", oc_shape_hex(now_ms, counter, true))
}

/// Canonical session id for one conversation: `ses_` + a real-clock hex
/// segment + a 14-character base62 tail derived from the conversation seed, so
/// distinct conversations never collide while retries of the same
/// conversation reuse the id (upstream groups both quota and prompt cache by
/// session).
pub fn canonical_session(seed: &str, now_ms: u128) -> String {
    if is_canonical_session(seed) {
        return seed.to_string();
    }
    let counter = next_oc_counter(now_ms);
    canonical_session_with_counter(seed, now_ms, counter)
}

/// `^ses_[0-9a-f]{12}[0-9a-zA-Z]{14}$` — an id already carrying the official
/// shape passes through untouched.
pub fn is_canonical_session(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 30 || !value.starts_with("ses_") {
        return false;
    }
    let hex = &bytes[4..16];
    if !hex.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
        return false;
    }
    bytes[16..].iter().all(|b| b.is_ascii_alphanumeric())
}

fn session_memo() -> &'static Mutex<Vec<(String, String)>> {
    static MEMO: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(Vec::new()))
}

/// Session id for a conversation seed, memoized (FIFO, capped) so a retry or a
/// later turn of the same conversation reuses the identical id.
pub fn session_for(seed: &str, now_ms: u128) -> String {
    if seed.is_empty() {
        return canonical_session(&format!("fallback-{now_ms}"), now_ms);
    }
    let memo = session_memo();
    if let Ok(guard) = memo.lock() {
        if let Some((_, session)) = guard.iter().find(|(key, _)| key == seed) {
            return session.clone();
        }
    }
    let session = canonical_session(seed, now_ms);
    if let Ok(mut guard) = memo.lock() {
        if guard.len() >= SESSION_MEMO_LIMIT {
            guard.remove(0);
        }
        guard.push((seed.to_string(), session.clone()));
    }
    session
}

/// Seed derived from the conversation itself (reference `conversationSeed`):
/// the JSON of the first user turn, which stays stable as history grows.
pub fn conversation_seed(body: &serde_json::Value) -> String {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        // Responses-shaped bodies: the first user input item, else the raw model.
        if let Some(items) = body.get("input").and_then(|i| i.as_array()) {
            for item in items {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                if role != "user" {
                    continue;
                }
                let encoded = item.to_string();
                if !encoded.is_empty() && encoded != "null" {
                    return encoded;
                }
            }
        }
        return String::new();
    };
    for message in messages {
        if message.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let encoded = message
            .get("content")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
            .to_string();
        if !encoded.is_empty() && encoded != "null" {
            return encoded;
        }
    }
    String::new()
}

/// The two gate stubs the free lane's agent-shape check looks for. They are
/// marked "do not call"; when the client sent no tools of its own the payload
/// also pins `tool_choice: "none"` so the model cannot invoke them.
pub fn gate_tools() -> Vec<serde_json::Value> {
    GATE_TOOL_NAMES
        .iter()
        .map(|name| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": "Reserved for the host runtime; do not call it.",
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        })
        .collect()
}

/// Make an outgoing payload satisfy the free-lane gate (reference
/// `ensureFreeLaneShape` / `ensureResponsesFreeLaneShape`): force `stream:
/// true` and append the gate tools the payload is missing.
///
/// Chat-completions bodies get the nested `function` shape and pin
/// `tool_choice: "none"` when the client carried no tools of its own (so the
/// model cannot call the stubs); Responses bodies get the flat shape its Zen
/// gateway expects and `tool_choice: "auto"`. A client-provided `tool_choice`
/// is always preserved.
pub fn apply_lane_contract(body: &mut serde_json::Value, responses: bool) {
    let Some(object) = body.as_object_mut() else { return };
    object.insert("stream".into(), serde_json::json!(true));
    let client_tools = object
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let present: BTreeSet<String> = client_tools
        .iter()
        .filter_map(|tool| {
            if responses {
                tool.get("name").and_then(|n| n.as_str())
            } else {
                tool.pointer("/function/name").and_then(|n| n.as_str())
            }
        })
        .map(str::to_string)
        .collect();
    let missing: Vec<&str> = GATE_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !present.contains(*name))
        .collect();
    if missing.is_empty() {
        return;
    }
    let had_client_tools = !client_tools.is_empty();
    let mut tools = client_tools;
    for name in missing {
        tools.push(gate_tool(name, responses));
    }
    object.insert("tools".into(), serde_json::Value::Array(tools));
    if !had_client_tools {
        object.insert(
            "tool_choice".into(),
            serde_json::json!(if responses { "auto" } else { "none" }),
        );
    }
}

/// One gate stub in the shape the target wire format uses. The parameters
/// mirror the client's real argument names: since 2026-10-14 a gate tool with
/// an empty `properties` object is refused on the inference surface, so the
/// stub carries `read{filePath}` / `bash{command}` instead of `{}`.
fn gate_tool(name: &str, responses: bool) -> serde_json::Value {
    let parameters = placeholder_parameters(name);
    if responses {
        serde_json::json!({
            "type": "function",
            "name": name,
            "description": "Reserved for the host runtime; do not call it.",
            "parameters": parameters
        })
    } else {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "Reserved for the host runtime; do not call it.",
                "parameters": parameters
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Free decision (S2 + S3)
// ---------------------------------------------------------------------------

/// Why a model is not usable on the anonymous lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// Locally configured blocklist (never served by the anonymous lane).
    Blocklist,
    /// models.dev says the model costs money.
    Paid,
    /// models.dev marks the model deprecated/retired.
    Deprecated,
    /// Upstream 403 `RegionError` — not available from this egress.
    Region,
    /// Upstream 400 `ModelProtocolUnsupported`.
    Protocol,
    /// Upstream 410 / "Model is unavailable".
    Gone,
    /// Upstream 404 for the model id.
    NotFound,
    /// Upstream 429 — free-lane budget for this IP/session.
    RateLimited,
}

impl DenyReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocklist => "blocklist",
            Self::Paid => "paid",
            Self::Deprecated => "deprecated",
            Self::Region => "region",
            Self::Protocol => "protocol",
            Self::Gone => "gone",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
        }
    }

    /// How long a learned denial (or rate-limit cooldown) lasts. Long enough
    /// not to burn requests on a dead id, short enough that a rollout or a
    /// region change is picked up without a restart.
    /// Is this verdict about the *model* (so it must not cool the provider)?
    ///
    /// `RateLimited` belongs here for the same reason as the rest: a 429 from
    /// an OpenCode tier is an answer about *that id* — upstream serves its
    /// models from different backends and one of them can be briefly
    /// unavailable while its siblings answer 200 (measured 2026-10-21:
    /// `opencode-go/deepseek-v4.1-flash` failed 3 of 10 straight calls with
    /// `Endpoint is unavailable` while `minimax-m3.1-flash-free` stayed 200).
    /// Cooling the whole provider would skip every healthy sibling — which is
    /// exactly how one flaky link turned `coding-plan` into `attempts: 3`
    /// and a blanket 429 even though its later `opencode-go` steps were fine.
    /// The per-model window (`ttl_ms`) still parks the id that failed.
    pub fn is_model_level(&self) -> bool {
        matches!(
            self,
            Self::Region | Self::Protocol | Self::Gone | Self::NotFound | Self::Blocklist | Self::RateLimited
        )
    }

    fn ttl_ms(&self) -> u128 {
        match self {
            // A country is a country: not going to change within the day.
            Self::Region => 6 * 3_600_000,
            // A hard schema/protocol answer.
            Self::Protocol => 24 * 3_600_000,
            // "Model is unavailable" / a 404 are ambiguous — they can be
            // transient or entitlement-related (the public directory is not an
            // entitlement list), so park them briefly and re-probe.
            Self::Gone | Self::NotFound => 3_600_000,
            Self::RateLimited => 60_000,
            Self::Blocklist | Self::Paid | Self::Deprecated => PERMANENT_MS,
        }
    }
}

/// One learned denial with its expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Denial {
    pub reason: DenyReason,
    pub status: u16,
    pub until_ms: u128,
}

/// How a model reached the free lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// In the live directory *and* models.dev reports zero cost.
    LiveAndFree,
    /// In the live directory, metadata pending — the `-free` name vouched.
    LiveNameFree,
    /// Metadata cannot speak and the id is on the verified bootstrap list.
    StaticVerified,
    /// Ids the directory still lists but the lane cannot serve.
    Blocklist,
    /// Metadata says paid.
    Paid,
    /// Metadata says deprecated.
    Deprecated,
    /// Learned from this gateway's own traffic.
    Learned,
}

/// The catalogue's verdict for one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub source: DecisionSource,
    pub denial: Option<Denial>,
}

/// `isFreeModel` (reference `catalog.ts`): the name-level fallback.
pub fn is_free_name(model: &str) -> bool {
    model.to_ascii_lowercase().contains("free")
}

/// The models.dev verdict for a single id, independent of the cache:
/// deprecated denies, zero-cost allows, a `-free` name never overrides a
/// negative metadata verdict.
pub fn metadata_decision(model: &str, entry: Option<&crate::model_catalog::CatalogEntry>) -> Decision {
    let name_free = is_free_name(model);
    let Some(entry) = entry else {
        return Decision {
            allowed: name_free,
            source: if name_free { DecisionSource::LiveNameFree } else { DecisionSource::Paid },
            denial: None,
        };
    };
    if entry.deprecated {
        return Decision {
            allowed: STATIC_VERIFIED_FREE.contains(&model),
            source: if STATIC_VERIFIED_FREE.contains(&model) {
                DecisionSource::StaticVerified
            } else {
                DecisionSource::Deprecated
            },
            denial: None,
        };
    }
    if entry.free == Some(true) {
        return Decision { allowed: true, source: DecisionSource::LiveAndFree, denial: None };
    }
    if entry.free == Some(false) {
        return Decision { allowed: false, source: DecisionSource::Paid, denial: None };
    }
    // Metadata present but silent on cost: keep the documented name fallback.
    Decision {
        allowed: name_free,
        source: if name_free { DecisionSource::LiveNameFree } else { DecisionSource::Paid },
        denial: None,
    }
}

/// Classify an upstream failure into a learned denial, if it is one.
pub fn classify_failure(status: u16, body: &str) -> Option<DenyReason> {
    let lowered = body.to_ascii_lowercase();
    match status {
        403 if lowered.contains("regionerror") || lowered.contains("not available in your country") => {
            Some(DenyReason::Region)
        }
        400 if lowered.contains("modelprotocolunsupported") => Some(DenyReason::Protocol),
        410 => Some(DenyReason::Gone),
        404 if lowered.contains("model") => Some(DenyReason::NotFound),
        429 => Some(DenyReason::RateLimited),
        _ if lowered.contains("modeldeprecated") => Some(DenyReason::Gone),
        _ if lowered.contains("model is unavailable") => Some(DenyReason::Gone),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Live directory + snapshot
// ---------------------------------------------------------------------------

/// Live state of the free lane.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// ids returned by the live directory (free and paid).
    pub live: BTreeSet<String>,
    /// epoch ms of the last successful live fetch (0 = never).
    pub live_at_ms: u128,
    /// last live-fetch error, empty when healthy.
    pub live_error: String,
    /// learned / configured denials, keyed by model id.
    pub denials: BTreeMap<String, Denial>,
    /// ids that answered 2xx on the anonymous lane, with the epoch ms.
    pub verified: BTreeMap<String, u128>,
    /// When each id first appeared in this gateway's directory. The upstream
    /// publishes no release date, so "first seen here" is the only honest
    /// newness signal available; it drives the newest-first half of the free
    /// routing order. Survives restarts through the on-disk cache.
    pub first_seen: BTreeMap<String, u128>,
    /// While set, the lane falls back to the anonymous `public` credential
    /// because a keyed call was rejected by the workspace privacy opt-in.
    pub consent_gate_until_ms: u128,
    /// While set, the lane's own gate is refusing every request shape this
    /// gateway can build (see `is_lane_gate_refusal`). Automatic zero-cost
    /// routing skips the Zen roster until this moment instead of paying a
    /// refused round trip per model; explicit requests still go through and get
    /// the honest `free_lane_unavailable` error.
    pub lane_gate_until_ms: u128,
    /// epoch ms of the last successful free-lane request seen by this gateway.
    pub last_success_ms: u128,
    /// successful / failed anonymous requests since boot.
    pub success_count: u64,
    pub failure_count: u64,
}

impl State {
    fn prune(&mut self, now_ms: u128) {
        self.denials.retain(|_, denial| denial.until_ms > now_ms);
    }

    /// Is the live directory usable (fresh enough to drive the roster)?
    pub fn live_ready(&self) -> bool {
        self.live_at_ms > 0 && !self.live.is_empty()
    }

}

/// Roster source used for `/v1/models` and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Ready,
    Stale,
    Error,
    Pending,
}

impl CatalogStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Stale => "stale",
            Self::Error => "error",
            Self::Pending => "pending",
        }
    }
}

fn shared() -> &'static RwLock<Arc<State>> {
    static SHARED: OnceLock<RwLock<Arc<State>>> = OnceLock::new();
    SHARED.get_or_init(|| RwLock::new(Arc::new(State::default())))
}

fn write_state(mutate: impl FnOnce(&mut State)) {
    let guard = shared();
    let mut current = guard.read().unwrap_or_else(|e| e.into_inner()).clone();
    let state = Arc::make_mut(&mut current);
    mutate(state);
    *guard.write().unwrap_or_else(|e| e.into_inner()) = current;
}

/// Current snapshot (cheap clone of the `Arc`).
pub fn snapshot() -> Arc<State> {
    shared().read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn now_epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    now_epoch_ms()
}

/// Is `at_ms` recent enough to still count as fresh? Drives the verified
/// ranking bonus and the "don't trust a stamp from long ago" rule.
pub fn is_fresh(at_ms: u128, now: u128) -> bool {
    at_ms > 0 && now.saturating_sub(at_ms) <= VERIFIED_TTL_MS
}

/// Drop stamps that stopped counting long ago, so an install that was offline
/// for weeks does not treat its whole roster as brand new. The exact age stays
/// truthful inside the TTL window, so a stamp is only discarded once it can no
/// longer influence ranking.
fn prune_stale_ages(state: &mut State) {
    let now = now_epoch_ms();
    let cutoff = VERIFIED_TTL_MS.saturating_mul(2);
    state.first_seen.retain(|_, at| now.saturating_sub(*at) <= cutoff);
    state.verified.retain(|_, at| now.saturating_sub(*at) <= cutoff);
}

/// `GET {base}/v1/models` with the anonymous credential; returns the ids.
pub async fn fetch_live_models(client: &reqwest::Client, base: &str) -> Result<Vec<String>, String> {
    fetch_models_at(client, &format!("{}/v1/models", base.trim_end_matches('/'))).await
}

fn models_request_headers(url: &str) -> Vec<(String, String)> {
    let go_surface = url.contains("/zen/go/") || url.contains("/go/v1/");
    let session = new_session_id();
    let mut headers = vec![
        ("authorization".to_string(), format!("Bearer {ANONYMOUS_KEY}")),
        ("x-opencode-client".to_string(), "cli".to_string()),
        ("x-opencode-project".to_string(), "global".to_string()),
        (
            "user-agent".to_string(),
            "opencode/latest/2.0.29/cli".to_string(),
        ),
        ("accept".to_string(), "application/json".to_string()),
    ];
    set_session_headers(&mut headers, &session);
    if go_surface {
        // The Go directory still expects the ascending message id; the
        // inference surface rejects the header's mere presence.
        headers.push(("x-opencode-request".to_string(), new_request_id()));
    }
    headers
}

/// `GET <url>` with the anonymous credential; returns the ids. Used directly
/// for the Go directory, whose path is `{zen}/go/v1/models` — appending the
/// Zen suffix to that base would double the `/v1` segment.
pub async fn fetch_models_at(client: &reqwest::Client, url: &str) -> Result<Vec<String>, String> {
    let mut request = client
        .get(url)
        .timeout(std::time::Duration::from_secs(30));
    for (name, value) in models_request_headers(url) {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("models endpoint {url} returned HTTP {status}"));
    }
    let payload: serde_json::Value = response.json().await.map_err(|e| format!("invalid JSON: {e}"))?;
    let ids: Vec<String> = payload
        .get("data")
        .and_then(|d| d.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Err("models endpoint returned an empty list".into());
    }
    Ok(ids)
}

/// Install a live roster (also used by tests).
pub fn install_live(ids: Vec<String>) {
    write_state(|state| {
        state.live = ids.into_iter().collect();
        state.live_at_ms = now_ms();
        state.live_error.clear();
        prune_first_seen(state);
    });
}

/// Stamp ids that are new to this gateway and drop the stamps of ids that left
/// the directory, so `first_seen` only ever describes the current roster.
fn prune_first_seen(state: &mut State) {
    let now = now_epoch_ms();
    for id in &state.live {
        state.first_seen.entry(id.clone()).or_insert(now);
    }
    let live = state.live.clone();
    state.first_seen.retain(|id, _| live.contains(id));
}

/// One refresh round: S1 live directory. Cheap enough to run on a short TTL
/// (the payload is ~7 KB) and it is the only thing that notices a rollout.
pub async fn refresh(client: &reqwest::Client, base: &str) -> Result<usize, String> {
    match fetch_live_models(client, base).await {
        Ok(ids) => {
            let count = ids.len();
            install_live(ids);
            Ok(count)
        }
        Err(error) => {
            write_state(|state| {
                state.live_error = error.clone();
            });
            Err(error)
        }
    }
}

/// Load the on-disk roster so a cold boot answers instantly; the network
/// refresh then replaces it (stale-while-revalidate).
pub fn bootstrap(data_dir: &std::path::Path) -> usize {
    let path = data_dir.join(CACHE_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else { return 0 };
    let Ok(cache) = serde_json::from_str::<CacheFile>(&text) else { return 0 };
    let count = cache.live.len();
    write_state(|state| {
        if !cache.live.is_empty() {
            state.live = cache.live.into_iter().collect();
            state.live_at_ms = cache.live_at_ms;
        }
        state.denials = cache.denials;
        state.verified = cache.verified;
        state.first_seen = cache.first_seen;
        state.lane_gate_until_ms = cache.lane_gate_until_ms;
        prune_stale_ages(state);
    });
    count
}

/// Persist the roster (temp file + rename, so a crash never truncates it).
pub fn save_disk(data_dir: &std::path::Path) -> std::io::Result<()> {
    let state = snapshot();
    let cache = CacheFile {
        live_at_ms: state.live_at_ms,
        live: state.live.iter().cloned().collect(),
        denials: state.denials.clone(),
        verified: state.verified.clone(),
        first_seen: state.first_seen.clone(),
        lane_gate_until_ms: state.lane_gate_until_ms,
    };
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(CACHE_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&tmp, &path)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    #[serde(default)]
    live_at_ms: u128,
    #[serde(default)]
    live: Vec<String>,
    #[serde(default)]
    denials: BTreeMap<String, Denial>,
    #[serde(default)]
    verified: BTreeMap<String, u128>,
    #[serde(default)]
    first_seen: BTreeMap<String, u128>,
    #[serde(default)]
    lane_gate_until_ms: u128,
}

/// Decide one model against the current roster, metadata and learned denials.
pub fn decide(model: &str, provider: &str) -> Decision {
    if FREE_LANE_BLOCKLIST.contains(&model) {
        return Decision {
            allowed: false,
            source: DecisionSource::Blocklist,
            denial: Some(Denial {
                reason: DenyReason::Blocklist,
                status: 0,
                until_ms: PERMANENT_MS,
            }),
        };
    }
    let state = snapshot();
    let now = now_ms();
    if let Some(denial) = state.denials.get(model) {
        if denial.until_ms > now {
            return Decision {
                allowed: false,
                source: DecisionSource::Learned,
                denial: Some(*denial),
            };
        }
    }
    let entry = crate::model_catalog::lookup(provider, model);
    let mut decision = metadata_decision(model, entry.as_ref());
    if !decision.allowed
        && decision.source != DecisionSource::Deprecated
        && STATIC_VERIFIED_FREE.contains(&model)
    {
        decision = Decision {
            allowed: true,
            source: DecisionSource::StaticVerified,
            denial: None,
        };
    }
    decision
}

/// The free roster exposed to clients: live directory ∩ allowed, minus learned
/// denials; the verified bootstrap list while the live directory is unavailable.
pub fn models() -> Vec<String> {
    let state = snapshot();
    let source: Vec<String> = if state.live_ready() {
        state.live.iter().cloned().collect()
    } else {
        STATIC_VERIFIED_FREE.iter().map(|m| (*m).to_string()).collect()
    };
    let mut out: Vec<String> = source
        .into_iter()
        .filter(|model| decide(model, "opencode").allowed)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Status of the roster for observability endpoints.
pub fn status() -> CatalogStatus {
    let state = snapshot();
    if state.live_at_ms == 0 {
        return if state.live_error.is_empty() {
            CatalogStatus::Pending
        } else {
            CatalogStatus::Error
        };
    }
    if !state.live_error.is_empty() {
        return CatalogStatus::Stale;
    }
    CatalogStatus::Ready
}

/// Record the upstream verdict of one anonymous request.
///
/// The free lane publishes no balance API; its budget is per-IP/per-session and
/// shows up as a status code. Learning the verdict here is what lets the roster
/// self-heal: a region-blocked or delisted model leaves `/v1/models` at once
/// and comes back when its denial expires.
/// The credential the free lane must use right now: the provider key when the
/// operator opted in *and* the workspace accepted it, otherwise the literal
/// anonymous credential the lane always accepts. Single source of truth for
/// both the chat path and the dashboard probes.
pub fn lane_credential(use_key: bool, provider_key: Option<String>, now: u128) -> Option<String> {
    let keyed_ok = use_key && !consent_gate_active(now);
    if keyed_ok {
        provider_key.or_else(|| Some(ANONYMOUS_KEY.to_string()))
    } else {
        Some(ANONYMOUS_KEY.to_string())
    }
}

/// Is the keyed path currently suspended by a consent rejection?
pub fn consent_gate_active(now: u128) -> bool {
    snapshot().consent_gate_until_ms > now
}

/// Suspend the keyed path after the workspace opt-in refused it.
pub fn note_consent_rejection(now: u128) {
    let until = now.saturating_add(CONSENT_RETRY_MS);
    write_state(|state| {
        state.consent_gate_until_ms = until;
    });
    tracing::warn!(
        "opencode free lane: keyed access needs the workspace's \"Allow free endpoints\" opt-in;          falling back to the anonymous credential for {} min",
        CONSENT_RETRY_MS / 60_000
    );
}

/// Drop the suspension (manual reset after enabling the console opt-in).
pub fn clear_consent_rejection() {
    write_state(|state| {
        state.consent_gate_until_ms = 0;
    });
}

pub fn record_verdict(model: &str, provider: &str, status: u16, body: &str) {
    // The Go tier shares the model namespace (and the egress): a region block
    // or a delisted id is a property of model+country, not of the tier, so the
    // same denial map serves both and `/v1/models` stops offering either.
    let go_tier = provider == "opencode-go";
    if !is_free_lane_provider(provider) && !go_tier {
        return;
    }
    if go_tier {
        if (200..300).contains(&status) {
            write_state(|state| {
                state.denials.remove(model);
            });
            return;
        }
        if let Some(reason) = classify_failure(status, body) {
            let until = now_ms().saturating_add(reason.ttl_ms());
            write_state(|state| {
                state
                    .denials
                    .insert(model.to_string(), Denial { reason, status, until_ms: until });
            });
            tracing::warn!(
                "opencode go tier: {model} denied for {} ms (status {status}, {})",
                reason.ttl_ms(),
                reason.as_str()
            );
        }
        return;
    }
    let now = now_ms();
    if (200..300).contains(&status) {
        write_state(|state| {
            state.verified.insert(model.to_string(), now);
            state.first_seen.entry(model.to_string()).or_insert(now);
            state.denials.remove(model);
            state.last_success_ms = now;
            state.success_count += 1;
        });
        return;
    }
    // The gate itself is not a statement about the model: a `FreeTierError`
    // means our request shape regressed, and hiding the model would hide the
    // regression too. Keep it exposed and let the operator see the failures.
    let lowered = body.to_ascii_lowercase();
    write_state(|state| {
        state.failure_count += 1;
    });
    if is_free_usage_limit(status, body) {
        // A free-quota 429 is a statement about the budget, not about this
        // model id. Keep the id visible and do not cool it: another free model
        // (or the same model on another credential) may still answer.
        return;
    }
    if lowered.contains(CONSENT_MARKER) {
        note_consent_rejection(now);
        return;
    }
    if lowered.contains("freetiererror") {
        // A gate refusal is a lane-wide verdict: park the automatic chain, but
        // keep the models in the roster so the regression stays visible and the
        // gate can be re-probed once the window expires.
        if is_lane_gate_refusal(status, body) {
            let until = note_lane_gate(now);
            tracing::warn!(
                "opencode free lane gate refused the request shape (status {status}); lane parked until {until} ({}s), models stay exposed: {}",
                LANE_GATE_TTL_MS / 1000,
                body.chars().take(200).collect::<String>()
            );
            return;
        }
        tracing::warn!(
            "opencode free lane rejected the request shape (status {status}); model {model} stays exposed: {}",
            body.chars().take(200).collect::<String>()
        );
        return;
    }
    let Some(reason) = classify_failure(status, body) else { return };
    let until_ms = now.saturating_add(reason.ttl_ms());
    write_state(|state| {
        state
            .denials
            .insert(model.to_string(), Denial { reason, status, until_ms });
        state.verified.remove(model);
    });
    tracing::warn!(
        "opencode free lane: {model} denied for {} ms (status {status}, {})",
        reason.ttl_ms(),
        reason.as_str()
    );
}

/// Roster rows for the observability endpoint.
pub fn roster_rows() -> Vec<serde_json::Value> {
    let state = snapshot();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let ids: BTreeSet<String> = if state.live_ready() {
        state.live.clone()
    } else {
        STATIC_VERIFIED_FREE.iter().map(|m| (*m).to_string()).collect()
    };
    for model in ids {
        let decision = decide(&model, "opencode");
        rows.push(serde_json::json!({
            "model": model,
            "inDirectory": state.live.contains(&model),
            "allowed": decision.allowed,
            "source": decision.source,
            "deniedUntilMs": decision.denial.map(|d| d.until_ms).unwrap_or(0),
            "denyReason": decision.denial.map(|d| d.reason.as_str()).unwrap_or(""),
            "verifiedAtMs": state.verified.get(&model).copied().unwrap_or(0),
            // Where this id sits in the free routing chain, and why.
            "score": rank_score("opencode", &model, now_epoch_ms()),
            "scoreReason": rank_reason(&model),
            "firstSeenMs": state.first_seen.get(&model).copied().unwrap_or(0),
        }));
    }
    rows
}

/// Background refresh loop: an immediate round, fast retries while the
/// directory is still empty (a cold boot often races the network), then the
/// steady cadence. The roster is persisted after every round.
pub fn spawn_background(data_dir: std::path::PathBuf, base: String, ttl_seconds: u64) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("opencode free lane: http client failed: {error}");
                return;
            }
        };
        let ttl = ttl_seconds.max(30);
        let mut attempts = 0;
        loop {
            match refresh(&client, &base).await {
                Ok(count) => {
                    tracing::info!("opencode free lane: live directory refreshed ({count} ids)");
                    let _ = save_disk(&data_dir);
                    attempts = 0;
                }
                Err(error) => {
                    attempts += 1;
                    tracing::warn!("opencode free lane: live directory refresh failed: {error}");
                }
            }
            // While the directory has never loaded, retry fast (4 tries), then
            // fall back to the normal cadence so a dead network stays quiet.
            let wait = if attempts > 0 && attempts <= 4 {
                std::time::Duration::from_secs(15)
            } else {
                std::time::Duration::from_secs(ttl)
            };
            tokio::time::sleep(wait).await;
        }
    });
}

/// Drop expired denials (used by the status endpoint so the roster prunes on
/// read even when no request fails).
pub fn prune() {
    let now = now_ms();
    write_state(|state| state.prune(now));
}

/// Serializes every unit test that touches the process-wide roster, including
/// tests in other modules (they reach it through `crate::opencode_free::`).
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        test_lock()
    }

    fn reset(state: &mut State) {
        *state = State::default();
    }

    #[test]
    fn inverted_timestamp_hex_encodes_a_real_clock_with_a_small_counter() {
        // 2026-10-15T23:20:00Z
        let now = 1_792_099_200_000u128;
        let hex = inverted_timestamp_hex(now, 1);
        assert_eq!(hex.len(), 12);
        let decoded = u128::from_str_radix(&hex, 16).unwrap();
        let counter = (!decoded) & 0xfff;
        assert!(counter <= 63, "counter {counter} must be in [0,63]");
        // The time field is 36 bits wide, so the clock is truncated (the
        // upstream only reads the low 12 counter bits) but must survive the
        // inversion intact.
        let clock = ((!decoded) & 0xffff_ffff_ffff) >> 12;
        assert_eq!(clock, now & 0xf_ffff_ffff);
    }

    #[test]
    fn counter_is_clamped_into_the_accepted_window() {
        for counter in 0u64..4096 {
            let hex = inverted_timestamp_hex(1_792_099_200_000, counter);
            let decoded = u128::from_str_radix(&hex, 16).unwrap();
            assert!(((!decoded) & 0xfff) <= 63);
        }
    }

    #[test]
    fn canonical_session_shape_matches_the_upstream_pattern() {
        let session = canonical_session_with_counter("hello", 1_792_099_200_000, 1);
        assert_eq!(session.len(), 30);
        assert!(is_canonical_session(&session));
        assert_eq!(&session[4..16], inverted_timestamp_hex(1_792_099_200_000, 1));
    }

    #[test]
    fn generated_session_ids_are_descending_and_well_formed() {
        for _ in 0..100 {
            let session = new_session_id();
            assert_eq!(session.len(), 30);
            assert!(is_canonical_session(&session));
            let decoded = u128::from_str_radix(&session[4..16], 16).unwrap();
            assert!(((!decoded) & 0xfff) <= 63);
        }
    }

    #[test]
    fn generated_request_ids_are_ascending_and_well_formed() {
        for _ in 0..100 {
            let request = new_request_id();
            assert_eq!(request.len(), 30);
            assert!(request.starts_with("msg_"));
            let decoded = u128::from_str_radix(&request[4..16], 16).unwrap();
            assert!((decoded & 0xfff) <= 63);
            let clock = decoded >> 12;
            let now = now_epoch_ms() & 0xf_ffff_ffff;
            assert!(clock == now || clock + 1 == now, "{clock} vs {now}");
        }
    }

    #[test]
    fn generated_session_ids_do_not_repeat() {
        let ids: BTreeSet<String> = (0..200).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 200);
    }

    #[test]
    fn session_header_names_all_carry_the_same_value() {
        let mut headers = vec![
            ("x-opencode-session".to_string(), "old".to_string()),
            ("x-other".to_string(), "keep".to_string()),
        ];
        set_session_headers(&mut headers, "ses_new");
        for name in SESSION_HEADER_NAMES {
            let values: Vec<&str> = headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
                .collect();
            assert_eq!(values, vec!["ses_new"], "{name}");
        }
        assert!(headers.iter().any(|(k, v)| k == "x-other" && v == "keep"));
    }

    #[test]
    fn models_headers_use_the_full_fingerprint() {
        let inference = models_request_headers("https://opencode.ai/zen/v1/models");
        for name in SESSION_HEADER_NAMES {
            assert!(
                inference
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case(name)),
                "{name}"
            );
        }
        assert!(inference
            .iter()
            .any(|(k, v)| k == "x-opencode-project" && v == "global"));
        assert!(!inference
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request")));

        let go = models_request_headers("https://opencode.ai/zen/go/v1/models");
        assert!(go
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request")));
    }

    #[test]
    fn canonical_session_passes_an_official_id_through() {
        let official = "ses_0123456789abABCDEFGHIJKLMN";
        assert!(is_canonical_session(official));
        assert_eq!(canonical_session(official, 0), official);
    }

    #[test]
    fn session_is_stable_per_conversation() {
        let first = session_for("seed-A", 1_792_099_200_000);
        let second = session_for("seed-A", 1_792_099_999_000);
        assert_eq!(first, second, "a conversation must reuse its session id");
        let other = session_for("seed-B", 1_792_099_200_000);
        assert_ne!(first, other);
    }

    #[test]
    fn conversation_seed_uses_the_first_user_turn() {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "first"},
                {"role": "user", "content": "second"}
            ]
        });
        assert_eq!(conversation_seed(&body), "\"first\"");
    }

    #[test]
    fn lane_contract_forces_stream_and_adds_the_gate_tools() {
        let mut body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        apply_lane_contract(&mut body, false);
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["tool_choice"], serde_json::json!("none"));
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn lane_contract_preserves_client_tools_and_tool_choice() {
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {}}}],
            "tool_choice": "auto"
        });
        apply_lane_contract(&mut body, false);
        assert_eq!(body["tool_choice"], serde_json::json!("auto"));
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["read", "bash"]);
    }

    #[test]
    fn lane_contract_is_idempotent() {
        let mut body = serde_json::json!({"messages": [], "tools": [{"type": "function", "function": {"name": "bash"}}]});
        apply_lane_contract(&mut body, false);
        let after_first = body.clone();
        apply_lane_contract(&mut body, false);
        assert_eq!(after_first, body);
    }

    #[test]
    fn blocklist_denies_before_any_metadata() {
        let _guard = serial();
        write_state(reset);
        let decision = decide("exo-free", "opencode");
        assert!(!decision.allowed);
        assert_eq!(decision.source, DecisionSource::Blocklist);
    }

    #[test]
    fn metadata_cost_drives_the_free_decision() {
        let free = crate::model_catalog::CatalogEntry {
            context_window: 1000,
            max_output_tokens: 100,
            modalities: vec!["text".into()],
            reasoning: true,
            free: Some(true),
            deprecated: false,
        };
        assert_eq!(metadata_decision("some-model", Some(&free)).source, DecisionSource::LiveAndFree);

        let paid = crate::model_catalog::CatalogEntry { free: Some(false), ..free.clone() };
        let decision = metadata_decision("some-model", Some(&paid));
        assert!(!decision.allowed);
        assert_eq!(decision.source, DecisionSource::Paid);
        // ...and a `-free` name never overrides a paid metadata verdict
        assert!(!metadata_decision("weird-free", Some(&paid)).allowed);

        let deprecated = crate::model_catalog::CatalogEntry { free: Some(true), deprecated: true, ..free };
        assert!(!metadata_decision("dead-free", Some(&deprecated)).allowed);
        assert!(metadata_decision("big-pickle", Some(&deprecated)).allowed);
    }

    #[test]
    fn failure_classification_matches_the_live_error_bodies() {
        assert_eq!(
            classify_failure(403, r#"{"error":{"type":"RegionError","message":"This model is not available in your country"}}"#),
            Some(DenyReason::Region)
        );
        assert_eq!(
            classify_failure(400, r#"{"error":{"type":"ModelProtocolUnsupported"}}"#),
            Some(DenyReason::Protocol)
        );
        assert_eq!(classify_failure(429, "GoUsageLimitError"), Some(DenyReason::RateLimited));
        assert_eq!(classify_failure(403, r#"{"error":{"type":"FreeTierError"}}"#), None);
    }

    #[test]
    fn every_verdict_that_names_one_id_is_model_level() {
        // A verdict is model-level when cooling the whole provider would put
        // healthy sibling models in the same penalty box. `RateLimited` belongs
        // in this list: measured 2026-10-21, `opencode-go/deepseek-v4.1-flash`
        // answered 429 `Endpoint is unavailable` on 3 of 10 direct calls while
        // `minimax-m3.1-flash-free` on the same tier stayed 200 — one flaky id
        // must not skip the rest of its provider (`coding-plan` regressed that
        // way to `attempts: 3` and a blanket 429).
        let model_level = [
            DenyReason::Region,
            DenyReason::Protocol,
            DenyReason::Gone,
            DenyReason::NotFound,
            DenyReason::Blocklist,
            DenyReason::RateLimited,
        ];
        for reason in model_level {
            assert!(
                reason.is_model_level(),
                "{} must stay model-level so sibling models keep falling back",
                reason.as_str()
            );
        }
    }

    #[test]
    fn a_rate_limited_model_is_parked_while_its_siblings_stay_routable() {
        let _guard = serial();
        write_state(reset);
        let flaky = "unit-flaky-go-free";
        let healthy = "unit-healthy-go-free";
        install_live(vec![flaky.to_string(), healthy.to_string()]);

        record_verdict(flaky, "opencode-go", 429, r#"{"error":{"type":"server_error","message":"Upstream request failed: Endpoint is unavailable."}}"#);

        // The id that answered 429 is parked for the short window...
        assert!(!decide(flaky, "opencode-go").allowed);
        assert_eq!(decide(flaky, "opencode-go").source, DecisionSource::Learned);
        // ...and its sibling on the same tier is untouched.
        assert!(
            decide(healthy, "opencode-go").allowed,
            "a 429 on one id must not deny its sibling"
        );
    }

    #[test]
    fn learned_denial_removes_and_restores_a_model() {
        let _guard = serial();
        write_state(reset);
        let model = "unit-test-free-model";
        install_live(vec![model.to_string(), "keep-me-free".to_string()]);
        assert!(models().contains(&model.to_string()));

        record_verdict(model, "opencode", 403, r#"{"error":{"type":"RegionError"}}"#);
        assert!(!decide(model, "opencode").allowed);
        assert_eq!(decide(model, "opencode").source, DecisionSource::Learned);
        assert!(!models().contains(&model.to_string()));

        // a success clears the denial
        record_verdict(model, "opencode", 200, "");
        assert!(decide(model, "opencode").allowed);
        assert!(models().contains(&model.to_string()));
    }

    #[test]
    fn free_tier_error_never_hides_a_model() {
        let _guard = serial();
        write_state(reset);
        let model = "shape-test-free";
        install_live(vec![model.to_string()]);
        record_verdict(model, "opencode", 403, r#"{"error":{"type":"FreeTierError"}}"#);
        assert!(decide(model, "opencode").allowed, "a gate rejection must not mask the roster");
    }

    #[test]
    fn a_free_usage_limit_is_not_a_model_denial() {
        let _guard = serial();
        write_state(reset);
        let model = "quota-test-free";
        install_live(vec![model.to_string()]);
        let body = r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"Free usage limit reached."}}"#;
        assert!(is_free_usage_limit(429, body));
        assert!(!is_free_usage_limit(429, r#"{"error":{"type":"rate_limit_error"}}"#));
        assert!(!is_free_usage_limit(403, body));
        record_verdict(model, "opencode", 429, body);
        assert!(
            decide(model, "opencode").allowed,
            "a budget 429 must not hide or cool the model; a sibling may still serve"
        );
    }

    #[test]
    fn a_configured_base_on_the_retired_zen_surface_is_normalized() {
        // A stored override that still pins `/zen/v1` must not keep the tier on
        // the retired inference surface...
        assert_eq!(
            normalize_base("opencode-zen", "https://opencode.ai/zen/v1").as_deref(),
            Some("https://opencode.ai/inference/v1")
        );
        assert_eq!(
            normalize_base("opencode", "https://opencode.ai/zen/v1/").as_deref(),
            Some("https://opencode.ai/inference/v1")
        );
        // ...while the Go tier, an already-migrated base and any other custom
        // base are left exactly as the operator wrote them.
        assert_eq!(normalize_base("opencode-go", "https://opencode.ai/zen/v1"), None);
        assert_eq!(
            normalize_base("opencode-zen", "https://opencode.ai/inference/v1"),
            None
        );
        assert_eq!(normalize_base("opencode-zen", "https://my-proxy.test/v1"), None);
        assert_eq!(normalize_base("openai", "https://opencode.ai/zen/v1"), None);
    }

    #[test]
    fn free_tier_inference_url_rehomes_the_zen_paths() {
        assert_eq!(
            inference_url("https://opencode.ai/zen/v1/chat/completions").as_deref(),
            Some("https://opencode.ai/inference/v1/chat/completions")
        );
        assert_eq!(
            inference_url("https://opencode.ai/zen/v1/responses").as_deref(),
            Some("https://opencode.ai/inference/v1/responses")
        );
        assert_eq!(
            inference_url("https://opencode.ai/zen/v1/messages").as_deref(),
            Some("https://opencode.ai/inference/v1/messages")
        );
        // A query string survives the rewrite.
        assert_eq!(
            inference_url("https://opencode.ai/zen/v1/chat/completions?x=1").as_deref(),
            Some("https://opencode.ai/inference/v1/chat/completions?x=1")
        );
        // The Go tier and an already-inference URL keep their base.
        assert_eq!(inference_url("https://opencode.ai/zen/go/v1/chat/completions"), None);
        assert_eq!(inference_url("https://opencode.ai/inference/v1/chat/completions"), None);
    }

    #[test]
    fn gate_tool_parameters_mirror_the_client_argument_names() {
        let read = placeholder_parameters("read");
        assert!(read["properties"].as_object().map_or(false, |p| p.contains_key("filePath")));
        let bash = placeholder_parameters("bash");
        assert!(bash["properties"].as_object().map_or(false, |p| p.contains_key("command")));
        assert_eq!(
            placeholder_parameters("_noop"),
            serde_json::json!({"type": "object", "properties": {}})
        );

        // A body with no client tools gets stubs whose schemas are non-empty:
        // an empty `properties` object is refused once the lane is open.
        let mut body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        apply_lane_contract(&mut body, false);
        for tool in body["tools"].as_array().unwrap() {
            let params = &tool["function"]["parameters"];
            assert!(
                params["properties"].as_object().map_or(false, |p| !p.is_empty()),
                "gate tools must carry non-empty properties: {tool}"
            );
        }
    }

    #[test]
    fn lane_gate_refusal_is_recognized_and_parks_only_the_automatic_chain() {
        let _guard = test_lock();
        write_state(reset);
        // The live body (2026-10-17) must be recognized; other 4xx verdicts
        // (a region block, a plain FreeTierError without the marker) must not
        // park the lane.
        assert!(is_lane_gate_refusal(
            403,
            r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode. Download it at https://opencode.ai and keep building."}}"#
        ));
        assert!(!is_lane_gate_refusal(403, r#"{"error":{"type":"RegionError"}}"#));
        assert!(!is_lane_gate_refusal(200, LANE_GATE_MARKER));
        assert!(!lane_gate_active(now_epoch_ms()), "a fresh gateway has an open gate");

        let now = now_epoch_ms();
        record_verdict(
            "gate-test-free",
            "opencode",
            403,
            r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode."}}"#,
        );
        assert!(lane_gate_active(now), "the refusal must park the chain");
        assert!(lane_gate_active(now + LANE_GATE_TTL_MS - 1), "still parked just before the window ends");
        assert!(
            !lane_gate_active(now + LANE_GATE_TTL_MS + 1),
            "the gate is re-probed after the window so a reopened lane needs no restart"
        );
        // The model stays in the roster: parking hides no regression.
        assert!(decide("gate-test-free", "opencode").allowed);

        // A per-model verdict must not park the lane.
        write_state(reset);
        record_verdict("region-test-free", "opencode", 403, r#"{"error":{"type":"RegionError"}}"#);
        assert!(!lane_gate_active(now_epoch_ms()));
    }

    #[test]
    fn a_consent_rejection_parks_the_lane_on_the_anonymous_credential() {
        let _guard = test_lock();
        write_state(reset);
        let now = now_epoch_ms();
        assert!(!consent_gate_active(now));
        record_verdict(
            "consent-free",
            "opencode",
            400,
            r#"{"error":{"type":"FreeTierError","message":"Free models may train on request data. Allow free endpoints in your workspace's Privacy settings"}}"#,
        );
        assert!(consent_gate_active(now), "the keyed path must be suspended");
        assert!(
            consent_gate_active(now + CONSENT_RETRY_MS - 1),
            "still suspended just before the retry window"
        );
        assert!(
            !consent_gate_active(now + CONSENT_RETRY_MS + 1),
            "the key is retried after the window so a console change needs no restart"
        );
        // A plain shape rejection (no consent marker) must not park it.
        clear_consent_rejection();
        record_verdict("consent-free", "opencode", 403, r#"{"error":{"type":"FreeTierError"}}"#);
        assert!(!consent_gate_active(now_epoch_ms()));
        // nor must a per-model failure
        record_verdict("consent-free", "opencode", 403, r#"{"error":{"type":"RegionError"}}"#);
        assert!(!consent_gate_active(now_epoch_ms()));
    }

    #[test]
    fn first_seen_is_stamped_once_per_id_and_pruned_away() {
        let _guard = test_lock();
        write_state(reset);
        let now = now_epoch_ms();
        write_state(|state| {
            // A cached roster: one id left the directory, two remain.
            state.live = ["old-free".into(), "keep-free".into()].into_iter().collect();
            state.first_seen = BTreeMap::from([
                ("gone-free".to_string(), now),
                ("old-free".to_string(), now),
            ]);
        });
        install_live(vec!["old-free".into(), "keep-free".into(), "new-free".into()]);
        let state = snapshot();
        assert!(state.first_seen.contains_key("old-free"), "existing stamps survive a refresh");
        assert!(state.first_seen.contains_key("keep-free"));
        assert!(state.first_seen.contains_key("new-free"));
        assert!(
            !state.first_seen.contains_key("gone-free"),
            "ids that left the directory lose their stamp"
        );
    }

    #[test]
    fn rank_score_weighs_newness_context_and_modalities() {
        let _guard = test_lock();
        write_state(reset);
        let now = 1_792_100_000_000u128;
        write_state(|state| {
            state.first_seen = BTreeMap::from([
                ("fresh-free".to_string(), now - 3_600_000),
                ("old-free".to_string(), now - NEWNESS_WINDOW_MS),
            ]);
        });
        // context + modalities come from the model catalog snapshot; ids it
        // cannot speak for score 0 on both, which is what the bootstrap list
        // needs (it has no directory history either).
        let fresh = rank_score("opencode", "fresh-free", now);
        let old = rank_score("opencode", "old-free", now);
        assert!(fresh.newness > 0.9, "{fresh:?}");
        assert_eq!(old.newness, 0.0, "{old:?}");
        assert_eq!(fresh.reason(), "newness");
        assert!(fresh.total > old.total);
        write_state(|state| state.first_seen.clear());
        let unknown = rank_score("opencode", "unknown-free", now);
        assert_eq!(unknown.total, 0.0, "{unknown:?}");
        assert_eq!(unknown.reason(), "known");
    }

    #[test]
    fn context_and_modalities_outweigh_a_brand_new_small_model() {
        let _guard = test_lock();
        write_state(reset);
        let now = 1_792_100_000_000u128;
        // Built-in catalog rows: big-pickle 160k text-only vs
        // longcat-2.5-preview-free 1M text+image.
        write_state(|state| {
            state.first_seen = BTreeMap::from([
                ("big-pickle".to_string(), now - 1_000),
                ("longcat-2.5-preview-free".to_string(), now - 3 * 24 * 3_600_000),
            ]);
        });
        let brand_new = rank_score("opencode", "big-pickle", now);
        let bigger = rank_score("opencode", "longcat-2.5-preview-free", now);
        assert_eq!(brand_new.newness, 1.0, "{brand_new:?}");
        assert_eq!(bigger.context, 1.0, "{bigger:?}");
        assert_eq!(bigger.modalities, 0.5, "{bigger:?}");
        assert!(
            bigger.total > brand_new.total,
            "a 3-day-old 1M multimodal model must outrank a brand-new 160k text-only one: {bigger:?} vs {brand_new:?}"
        );
        let ordered = order_models_for(
            "opencode",
            &["big-pickle".to_string(), "longcat-2.5-preview-free".to_string()],
            now,
        );
        assert_eq!(ordered[0], "longcat-2.5-preview-free", "{ordered:?}");
    }

    #[test]
    fn order_models_for_uses_the_total_then_alphabetical_for_stability() {
        let _guard = test_lock();
        write_state(reset);
        let now = 1_792_100_000_000u128;
        write_state(|state| {
            // No catalog entries for these ids, so only newness + verified move.
            state.first_seen = BTreeMap::from([
                ("newest-free".to_string(), now - 60_000),
                ("verified-old".to_string(), now - 5 * 24 * 3_600_000),
                ("plain-old".to_string(), now - 6 * 24 * 3_600_000),
            ]);
            state.verified = BTreeMap::from([("verified-old".to_string(), now - 60_000)]);
        });
        let roster: Vec<String> = ["newest-free", "verified-old", "plain-old"]
            .iter()
            .map(|m| m.to_string())
            .collect();
        let ordered = order_models_for("opencode", &roster, now);
        assert_eq!(
            ordered,
            vec![
                "newest-free".to_string(),
                // the recent 2xx bonus lifts it over the slightly older stamp
                "verified-old".to_string(),
                "plain-old".to_string()
            ]
        );
        // Equal totals fall back to the alphabetical order so a re-resolve is
        // stable (and never flaps between two runs).
        write_state(|state| {
            state.first_seen.clear();
            state.verified.clear();
        });
        assert_eq!(
            order_models_for("opencode", &roster, now),
            vec!["newest-free".to_string(), "plain-old".to_string(), "verified-old".to_string()]
        );
    }

    #[test]
    fn roster_falls_back_to_the_verified_list_when_the_directory_is_down() {
        let _guard = serial();
        write_state(|state| {
            state.live.clear();
            state.live_at_ms = 0;
            state.live_error = "offline".into();
        });
        let roster = models();
        assert_eq!(roster, STATIC_VERIFIED_FREE.iter().map(|m| m.to_string()).collect::<Vec<_>>());
        assert_eq!(status(), CatalogStatus::Error);
    }

    #[test]
    fn disk_cache_round_trips() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        write_state(reset);
        write_state(|state| {
            state.live.clear();
            state.live.insert("cached-free".into());
            state.live_at_ms = 42;
            state.live_error.clear();
            state.denials.clear();
        });
        save_disk(dir.path()).unwrap();
        write_state(|state| {
            state.live.clear();
            state.live_at_ms = 0;
        });
        assert_eq!(bootstrap(dir.path()), 1);
        assert_eq!(snapshot().live_at_ms, 42);
        assert!(snapshot().live.contains("cached-free"));
    }
}