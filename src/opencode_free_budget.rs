//! OpenCode Zen free-lane budget: UTC-day request counters per credential,
//! learned caps, adaptive 429 scope and the per-credential in-flight limiter.
//!
//! The anonymous Zen free tier publishes no balance endpoint, and its real
//! daily ceiling is not documented. Two observations bound the design:
//!
//! * OmniRoute's own resilience notes treat the budget as **per egress IP**
//!   (`docs/architecture/RESILIENCE_GUIDE.md`, "Egress-bucketed lock"), which
//!   would make a multi-account pool useless without multiple egresses;
//! * a public report (`anomalyco/opencode#33495`) saw the limit trigger after
//!   roughly 200 requests and kept triggering after the reporter changed IP,
//!   which looks **per account/workspace** instead.
//!
//! Because the answer is not knowable up front, this module is adaptive:
//!
//! * every credential (each managed `opencode-zen` account, the env/config key,
//!   and the anonymous `public` credential) owns a bucket counted per UTC day,
//!   with a configured ceiling and a locally persisted window;
//! * the first `429 FreeUsageLimitError` on a bucket in a window records
//!   `learned_limit = max(1, used_at_429)`, so the rest of that window is
//!   skipped locally instead of burning one guaranteed 429 per account per day;
//! * when a *different* account of the same model answers 429 within
//!   [`EGRESS_PROBE_WINDOW_MS`], the failure is re-attributed to the shared
//!   egress (IP) bucket: every bucket is cooled to the window reset and the
//!   scope is remembered as [`BucketScope::Egress`], so the rest of the window
//!   neither probes accounts nor spends upstream requests. A 200 after
//!   switching accounts instead settles the scope as [`BucketScope::Account`].
//!
//! Phase 2 adds an **egress dimension**. Each proxy exit is its own IP and
//! therefore its own upstream bucket, so a proxied bucket key becomes
//! `credential\0scheme://host:port` while a direct dial keeps the historical
//! key untouched. Scope probing and the 429 cooldown run per exit: a daily 429
//! parks only that exit's buckets, and a proxy/tunnel failure cools just that
//! exit briefly ([`BudgetLimits::proxy_cooldown_ms`], default 30 s). The
//! anonymous proxy pool, its sticky rotation and masking live in
//! [`crate::opencode_free_egress`].
//!
//! The counters live in `<data dir>/opencode-free-budget.json` (atomic
//! temp-file + rename, `0600`). They are process-wide like
//! [`crate::opencode_free`]'s roster snapshot; a background saver flushes the
//! dirty flag every couple of seconds and the CLI flushes once more on
//! shutdown, so a restart resumes the same window instead of re-spending it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

use crate::config::Config;
use crate::errors::ApiError;
use crate::state::{AccountCredential, AppState};
use crate::upstream::bun::Egress;

/// Atomic snapshot file inside the data dir.
pub const BUDGET_FILE: &str = "opencode-free-budget.json";
/// Length of the accounting window: one UTC day, the cadence the upstream's
/// `Retry-After` points at (roughly the next 00:00 UTC).
pub const DAY_MS: u128 = 86_400_000;
/// Configured per-account ceiling. 200 is the figure the public report
/// observed and what OmniRoute's egress-bucket accounting expects per bucket;
/// the learned limit adjusts it per window as soon as the upstream disagrees.
pub const DEFAULT_DAILY_LIMIT_PER_ACCOUNT: u64 = 200;
/// Configured ceiling of the anonymous `public` credential. The same 200 the
/// per-account bucket starts from; it is a separate bucket because the
/// upstream may account for it differently.
pub const DEFAULT_DAILY_LIMIT_PUBLIC: u64 = 200;
/// Managed accounts in flight at once before a request rotates to the next
/// account (0 disables the limit). Two keeps one account's traffic smooth
/// without starving siblings; the free lane is a latency-insensitive chat lane.
pub const DEFAULT_MAX_IN_FLIGHT_PER_ACCOUNT: usize = 2;
/// A `429` whose `Retry-After` is at most this long is a *burst* rate limit
/// (per-minute window, retry shortly) rather than the UTC-day free budget: it
/// cools only the credential it answered and teaches no learned ceiling.
pub const SHORT_LIMIT_MAX_MS: u128 = 300_000;
/// Minimum number of accepted requests a daily-class 429 must follow before it
/// is trusted as a genuine quota wall. A 429 after one or two requests on a
/// near-fresh window is far more likely the upstream's session-admission gate
/// (new sessions refused while accepted ones keep working) than a 1-request
/// daily quota; treating it as the latter used to write `learned_limit = 1`
/// for a week and park the whole bucket until UTC midnight.
pub const DEFAULT_LEARNED_MIN_SAMPLES: u64 = 10;
/// Low-usage 429s only cool the credential for this long (seconds) instead of
/// parking it for the UTC window, so a transient admission state clears on its
/// own. `0` would mean "no cooldown"; the default keeps a short back-off.
pub const DEFAULT_SHORT_QUOTA_COOLDOWN_SECS: u64 = 60;
/// How many low-usage 429s in one UTC window turn the suspicion into the normal
/// daily-quota handling. This keeps a genuinely tiny quota account from being
/// ignored forever while still not parking a lane on the first admission 429.
pub const LOW_USAGE_UPGRADE_SAMPLES: u64 = 3;
/// Default lifetime of a learned ceiling. A learned limit is a conservative
/// floor (it only ever moves down), so it must not outlive the evidence
/// forever: after this long without a fresh 429 the ceiling is raised by 20 %
/// per elapsed period, which re-opens the window slowly if the upstream
/// relaxed its quota. `0` disables the re-evaluation.
pub const DEFAULT_LEARNED_LIMIT_TTL_DAYS: u64 = 7;
/// How long after a 429 on one account a 429 on another account of the same
/// model is re-attributed to the shared egress. Long enough to cover the
/// account switch (one round trip), short enough that an unrelated later
/// window does not poison the scope.
pub const EGRESS_PROBE_WINDOW_MS: u128 = 60_000;
/// Sleep between in-flight-slot retries while a request queues for an account.
pub const SLOT_POLL_MS: u64 = 20;
/// Longest a request waits for an in-flight slot before falling through to the
/// combo's next candidate. Bounded so a saturated pool never stalls a turn.
pub const SLOT_WAIT_MS: u64 = 500;
/// Error code of a budget refusal that never reached the upstream.
pub const QUOTA_ERROR_CODE: &str = "free_lane_budget";
/// Separator between a credential bucket and its egress label. `\0` cannot
/// appear in a connection id or in an egress label, so a bucket key stays
/// unambiguous.
pub const EGRESS_KEY_SEPARATOR: char = '\u{0}';
/// Bucket key of the anonymous `public` credential (shared by both provider
/// aliases: it is one egress identity upstream).
pub const PUBLIC_BUCKET: &str = "public";
/// Prefix of a managed-account bucket (`conn:<connection id>`).
pub const ACCOUNT_BUCKET_PREFIX: &str = "conn:";
/// Prefix of the env/config credential bucket (`config:<provider id>`).
pub const CONFIG_BUCKET_PREFIX: &str = "config:";
/// Flush cadence of the background saver.
const SAVE_INTERVAL_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// Configuration surface
// ---------------------------------------------------------------------------

/// How the free lane authenticates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CredentialMode {
    /// Round-robin the provider's managed account keys; fall back to the
    /// anonymous credential when the pool is empty, exhausted or cooling.
    #[default]
    Auto,
    /// Rotate account keys like `Auto`, but a provider without a managed pool
    /// may also use its env/config key (the legacy `*_USE_KEY=true` behaviour).
    Key,
    /// Always the literal `public` credential; account pools are ignored.
    Public,
}

impl CredentialMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "key" | "keyed" => Some(Self::Key),
            "public" | "anonymous" => Some(Self::Public),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Key => "key",
            Self::Public => "public",
        }
    }

    /// Does this mode ever send a key (an account's or the provider's)?
    pub fn uses_key(&self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// Which accounting level the upstream's budget turned out to live at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketScope {
    /// No 429 seen in this window yet.
    #[default]
    Unknown,
    /// The 429 was specific to the credential that saw it: its siblings may
    /// still serve.
    Account,
    /// A second account on the same model hit 429 in the same short window:
    /// the budget follows the egress (IP), so every credential is exhausted
    /// together and per-account probing is suspended for the window.
    Egress,
}

impl BucketScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Account => "account",
            Self::Egress => "egress",
        }
    }
}

/// Limits and knobs of one accounting decision, resolved from [`Config`].
#[derive(Debug, Clone, Copy)]
pub struct BudgetLimits {
    pub per_account: u64,
    pub public: u64,
    pub learned: bool,
    /// Lifetime of a learned ceiling before the conservative +20 % step;
    /// 0 disables re-evaluation.
    pub learned_limit_ttl_ms: u128,
    /// Daily 429s observed after fewer accepted requests than this are treated
    /// as suspected session-admission refusals (short cooldown only). 0 keeps
    /// the old behaviour.
    pub learned_min_samples: u64,
    /// Cooldown of one suspected low-usage refusal.
    pub short_quota_cooldown_ms: u128,
    /// Short cooldown of a proxy exit whose tunnel failed (unreachable proxy,
    /// refused CONNECT, SOCKS negotiation failure). The failed attempt is
    /// refunded, so `0` disables the cooldown and the exit is retried at once.
    pub proxy_cooldown_ms: u128,
    pub max_in_flight: usize,
}

impl BudgetLimits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            per_account: config.opencode_free_daily_limit_per_account.max(1),
            public: config.opencode_free_daily_limit_public.max(1),
            learned: config.opencode_free_learned_limit,
            learned_limit_ttl_ms: u128::from(
                config.opencode_free_learned_limit_ttl_days,
            ) * DAY_MS,
            learned_min_samples: config.opencode_free_learned_min_samples,
            short_quota_cooldown_ms: u128::from(config.opencode_free_short_quota_cooldown_secs)
                * 1000,
            proxy_cooldown_ms: u128::from(config.opencode_free_proxy_cooldown_secs) * 1000,
            max_in_flight: config.opencode_free_max_in_flight_per_account,
        }
    }

    fn limit_for(&self, bucket: &str) -> u64 {
        if is_public_bucket(bucket) {
            self.public
        } else {
            self.per_account
        }
    }
}

// ---------------------------------------------------------------------------
// Persisted / live state
// ---------------------------------------------------------------------------

/// One credential's accounting within the current UTC window.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    /// Requests charged to this credential in the window (a request is charged
    /// when it is dispatched, before the upstream answer).
    #[serde(default)]
    pub used: u64,
    /// Configured ceiling this bucket was last evaluated with.
    #[serde(default)]
    pub limit: u64,
    /// Ceiling learned from observed 429s across windows (`0` = nothing
    /// learned). Unlike `used` it survives the UTC-day rollover: the upstream
    /// quota does not necessarily change at midnight, and forgetting it would
    /// burn one guaranteed 429 per account per day.
    #[serde(default)]
    pub learned_limit: u64,
    /// Epoch ms of the last 429 that informed `learned_limit`.
    #[serde(default)]
    pub learned_at_ms: u128,
    /// Value `learned_limit` had before this window's provisional update.
    ///
    /// A daily 429 is recorded immediately (the rest of the window must skip the
    /// credential), but its *attribution* is only proven later: if a second
    /// account of the same model answers 429 inside the probe window, the
    /// budget is egress-scoped and the per-account number was never the
    /// account's own ceiling. The previous value is restored in that case and
    /// committed at the next window rollover otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_learned_prev: Option<LearnedPrev>,
    /// Epoch ms of the window reset.
    #[serde(default)]
    pub reset_at_ms: u128,
    /// Epoch ms until which this credential must not be used.
    #[serde(default)]
    pub cooldown_until_ms: u128,
    /// Whether the 90 % warning was already emitted for this window.
    #[serde(default)]
    pub warned: bool,
}

impl Bucket {
    /// The ceiling actually enforced: the configured one, tightened by what the
    /// first 429 taught us about the real (undocumented) limit.
    pub fn effective_limit(&self, configured: u64) -> u64 {
        let configured = configured.max(1);
        if self.learned_limit > 0 {
            configured.min(self.learned_limit)
        } else {
            configured
        }
    }

    pub fn exhausted(&self, configured: u64) -> bool {
        self.used >= self.effective_limit(configured)
    }
}

/// The value `learned_limit` had before the current window's provisional
/// update (see [`Bucket::window_learned_prev`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LearnedPrev {
    pub limit: u64,
    pub at_ms: u128,
}

/// The last `429 FreeUsageLimitError` of the window, used to decide whether a
/// second account's 429 means "egress" rather than "that account".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Quota429 {
    model: String,
    bucket: String,
    at_ms: u128,
}

/// Learnings and health of one proxied exit.
///
/// Each proxy is its own IP and therefore its own upstream budget bucket, so
/// the scope lesson and the 429 probe are tracked per exit: a 429 on exit A
/// says nothing about exit B, and cooling A must never park B. The direct dial
/// keeps the process-wide [`BudgetFile::scope`] fields (there is only one local
/// address), which is exactly the pre-proxy behaviour.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ExitState {
    #[serde(default)]
    scope: BucketScope,
    #[serde(default)]
    last_429: Option<Quota429>,
    /// Epoch ms until which this exit is cooling after a tunnel failure.
    #[serde(default)]
    cooldown_until_ms: u128,
    /// Tunnel failures observed in the current window (observability only).
    #[serde(default)]
    failures: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct BudgetFile {
    /// UTC day index the counters belong to (`epoch_ms / DAY_MS`).
    #[serde(default)]
    window: i64,
    #[serde(default)]
    scope: BucketScope,
    #[serde(default)]
    buckets: BTreeMap<String, Bucket>,
    #[serde(default)]
    last_429: Option<Quota429>,
    /// Per-proxy learnings (keyed by `Egress::label`, e.g.
    /// `socks5h://host:port`). Empty while every attempt dials directly.
    #[serde(default)]
    exits: BTreeMap<String, ExitState>,
    /// A daily-class 429 arrived after barely any accepted requests: the
    /// upstream is likely refusing *fresh sessions* rather than enforcing the
    /// request quota (surfaced by the status endpoint and in the logs).
    #[serde(default)]
    session_admission_suspected: bool,
    /// Low-usage 429s seen in this window (drives [`LOW_USAGE_UPGRADE_SAMPLES`]).
    #[serde(default)]
    short_quota_refusals: u64,
}

/// Live state: the persisted view plus counters that must not survive a crash
/// (in-flight slots) or that are deliberately process-local (round-robin cursor,
/// dirty flag).
#[derive(Debug, Default)]
struct State {
    file: BudgetFile,
    in_flight: BTreeMap<String, u64>,
    cursor: u64,
    dirty: bool,
}

fn shared() -> &'static RwLock<State> {
    static SHARED: OnceLock<RwLock<State>> = OnceLock::new();
    SHARED.get_or_init(|| RwLock::new(State::default()))
}

fn with_state<R>(mutate: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = shared().write().unwrap_or_else(|e| e.into_inner());
    mutate(&mut guard)
}

/// UTC day index of an epoch-ms instant.
pub fn window_day(now_ms: u128) -> i64 {
    (now_ms / DAY_MS) as i64
}

/// Epoch ms of the next UTC midnight after `now_ms` (the window boundary).
pub fn window_reset_at(now_ms: u128) -> u128 {
    (now_ms / DAY_MS + 1) * DAY_MS
}

/// Raise a stale learned ceiling by one conservative step.
///
/// A learned limit only ever moves down while the upstream keeps refusing at
/// that level. If no fresh 429 arrived for `ttl_ms`, the floor is raised by
/// 20 % (at least one request) and the window re-opens gradually instead of
/// staying pinned to an old, possibly relaxed quota forever. `ttl_ms == 0`
/// disables the re-evaluation.
fn refresh_learned(bucket: &mut Bucket, configured: u64, now: u128, ttl_ms: u128) -> bool {
    if bucket.learned_limit == 0 || ttl_ms == 0 {
        return false;
    }
    let elapsed = now.saturating_sub(bucket.learned_at_ms);
    if bucket.learned_at_ms == 0 || elapsed <= ttl_ms {
        return false;
    }
    let raised = ((bucket.learned_limit as f64 * 1.2).ceil() as u64)
        .max(bucket.learned_limit.saturating_add(1))
        .min(configured.max(1));
    bucket.learned_at_ms = now;
    if raised >= configured.max(1) {
        // Nothing left to learn: the configured ceiling is in force again.
        bucket.learned_limit = 0;
        bucket.learned_at_ms = 0;
    } else {
        bucket.learned_limit = raised;
    }
    true
}

/// Reset a bucket when the UTC window has rolled over.
fn rollover(state: &mut State, now: u128) {
    let window = window_day(now);
    if state.file.window == window {
        return;
    }
    state.file.window = window;
    let reset_at = window_reset_at(now);
    for bucket in state.file.buckets.values_mut() {
        bucket.used = 0;
        bucket.warned = false;
        bucket.reset_at_ms = reset_at;
        // The learned ceiling is a long-lived property: the upstream quota does
        // not reset merely because our accounting day did, and dropping it here
        // would burn one guaranteed 429 per credential per day. Only the
        // provisional (unproven) update of the *old* window is committed now.
        bucket.window_learned_prev = None;
        // A cooldown that outlived the window (an upstream `Retry-After` past
        // midnight) keeps running; an expired one is simply gone.
        if bucket.cooldown_until_ms <= now {
            bucket.cooldown_until_ms = 0;
        }
    }
    state.file.scope = BucketScope::Unknown;
    state.file.last_429 = None;
    for exit in state.file.exits.values_mut() {
        exit.scope = BucketScope::Unknown;
        exit.last_429 = None;
        if exit.cooldown_until_ms <= now {
            exit.cooldown_until_ms = 0;
        }
    }
    state.file.session_admission_suspected = false;
    state.file.short_quota_refusals = 0;
}

// ---------------------------------------------------------------------------
// Egress-scope bookkeeping
// ---------------------------------------------------------------------------

/// Learned scope of one egress (`None` = direct dial, which keeps the
/// process-wide [`BudgetFile::scope`] field).
fn exit_scope(state: &State, egress: Option<&str>) -> BucketScope {
    match egress {
        None => state.file.scope,
        Some(label) => state
            .file
            .exits
            .get(label)
            .map(|exit| exit.scope)
            .unwrap_or_default(),
    }
}

fn set_exit_scope(state: &mut State, egress: Option<&str>, scope: BucketScope) {
    match egress {
        None => state.file.scope = scope,
        Some(label) => {
            state.file.exits.entry(label.to_string()).or_default().scope = scope;
        }
    }
}

/// The last daily 429 of this egress's current window.
fn exit_last_429(state: &State, egress: Option<&str>) -> Option<Quota429> {
    match egress {
        None => state.file.last_429.clone(),
        Some(label) => state
            .file
            .exits
            .get(label)
            .and_then(|exit| exit.last_429.clone()),
    }
}

fn set_exit_last_429(state: &mut State, egress: Option<&str>, last: Quota429) {
    match egress {
        None => state.file.last_429 = Some(last),
        Some(label) => {
            state.file.exits.entry(label.to_string()).or_default().last_429 = Some(last);
        }
    }
}

// ---------------------------------------------------------------------------
// Credential planning
// ---------------------------------------------------------------------------

/// One credential a free-lane request may use, with its accounting bucket.
#[derive(Debug, Clone)]
pub struct LaneCredential {
    /// Accounting bucket. Contains the egress dimension when the attempt leaves
    /// through a proxy (`credential\0scheme://host:port`); a direct dial keeps
    /// the bare credential key.
    pub bucket: String,
    /// The `Authorization: Bearer` value this request must send.
    pub secret: String,
    /// The managed account behind the credential, when there is one.
    pub account: Option<AccountCredential>,
    /// True for the anonymous `public` credential.
    pub anonymous: bool,
    /// How this attempt leaves the host. One credential may appear several
    /// times in a list, once per available exit.
    pub egress: Egress,
}

impl LaneCredential {
    /// Egress accounting label (`None` for a direct dial).
    pub fn egress_label(&self) -> Option<&str> {
        bucket_egress(&self.bucket)
    }
}

/// The bucket key of a managed account.
pub fn account_bucket(account: &AccountCredential, provider: &str) -> String {
    if account.connection_id.is_empty() {
        config_bucket(provider)
    } else {
        format!("{ACCOUNT_BUCKET_PREFIX}{}", account.connection_id)
    }
}

/// The bucket key of the env/config credential of `provider`.
pub fn config_bucket(provider: &str) -> String {
    format!("{CONFIG_BUCKET_PREFIX}{provider}")
}

/// Compose the accounting key of a credential on a specific egress.
///
/// A direct dial keeps the historical key exactly (`conn:…`, `public`,
/// `config:…`), so every pre-proxy counter and test is untouched. A proxied
/// attempt appends the egress label: the upstream counts the free budget per
/// egress IP, so the same credential through another exit is a fresh bucket.
pub fn with_egress(bucket: String, egress_label: Option<&str>) -> String {
    match egress_label.filter(|label| !label.is_empty()) {
        Some(label) => format!("{bucket}{EGRESS_KEY_SEPARATOR}{label}"),
        None => bucket,
    }
}

/// Split a bucket key into `(credential bucket, egress label)`.
pub fn split_bucket(bucket: &str) -> (&str, Option<&str>) {
    match bucket.split_once(EGRESS_KEY_SEPARATOR) {
        Some((base, egress)) => (base, Some(egress)),
        None => (bucket, None),
    }
}

/// The credential half of a bucket key (the egress dimension removed).
pub fn bucket_base(bucket: &str) -> &str {
    split_bucket(bucket).0
}

/// The egress label of a bucket key (`None` for a direct dial).
pub fn bucket_egress(bucket: &str) -> Option<&str> {
    split_bucket(bucket).1
}

/// Is this bucket (or composite key) the anonymous `public` credential?
pub fn is_public_bucket(bucket: &str) -> bool {
    bucket_base(bucket) == PUBLIC_BUCKET
}

/// Mask a bucket key for the observability endpoint. The management API may
/// expose connection *names*, but ids are internal and a key hash must never
/// leak, so only a short stable prefix/suffix survives. The egress half (if
/// any) is shown as a masked `host:port`, never with credentials.
pub fn mask_bucket(bucket: &str) -> String {
    let (base, egress) = split_bucket(bucket);
    let masked = mask_credential_bucket(base);
    match egress {
        Some(label) => format!("{masked} @ {}", crate::upstream::bun::mask_egress_label(label)),
        None => masked,
    }
}

fn mask_credential_bucket(bucket: &str) -> String {
    // The anonymous credential's bucket is a fixed, public name: useful in the
    // status endpoint and not an identifier of anything.
    if bucket == PUBLIC_BUCKET {
        return bucket.to_string();
    }
    let (prefix, rest) = match bucket.split_once(':') {
        Some((prefix, rest)) => (format!("{prefix}:"), rest),
        None => (String::new(), bucket),
    };
    let chars: Vec<char> = rest.chars().collect();
    if chars.len() <= 8 {
        return format!("{prefix}••••");
    }
    format!(
        "{prefix}{}••••{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// Tunnel mode of an egress label (`scheme://host:port`).
pub fn egress_mode(label: &str) -> String {
    match label.split_once("://").map(|(scheme, _)| scheme) {
        Some("http") | Some("https") => "http_connect".to_string(),
        Some("socks5") => "socks5".to_string(),
        Some("socks5h") => "socks5h".to_string(),
        Some(other) => other.to_string(),
        None => "direct".to_string(),
    }
}

/// Masked `host:port` of an egress label, for the status endpoint.
pub fn egress_masked_label(label: &str) -> String {
    let masked = crate::upstream::bun::mask_egress_label(label);
    masked
        .split_once("://")
        .map(|(_, authority)| authority.to_string())
        .unwrap_or(masked)
}

/// The managed accounts of the free lane.
///
/// Both Zen aliases (`opencode` and `opencode-zen`) resolve to the same
/// upstream, and an operator may have registered the pool under either name
/// while the candidate resolved through the other one, so the pools are
/// unioned. Duplicates are removed by connection id.
pub fn free_lane_accounts(state: &AppState) -> Vec<AccountCredential> {
    let mut out: Vec<AccountCredential> = Vec::new();
    for id in ["opencode", "opencode-zen"] {
        for account in state.accounts_for(id) {
            let duplicate = out.iter().any(|seen| {
                (!seen.connection_id.is_empty() && seen.connection_id == account.connection_id)
                    || (seen.connection_id.is_empty()
                        && account.connection_id.is_empty()
                        && seen.api_key == account.api_key)
            });
            if !duplicate {
                out.push(account);
            }
        }
    }
    out
}

/// The ordered credential candidates for one free-lane request: managed
/// accounts (skipping any the circuit store marked cooling), the env/config key
/// in [`CredentialMode::Key`], then the anonymous credential.
///
/// Egress is resolved per credential: a managed account tunnels through its own
/// `proxy_url` (or dials directly), and the anonymous credential is expanded
/// over the configured proxy pool in session-sticky rotation order. `None`
/// `session_seed` or an empty pool keeps the historical single direct
/// candidate.
pub fn lane_credentials(
    state: &AppState,
    provider: &str,
    model: &str,
    mode: CredentialMode,
    now: u128,
    session_seed: &str,
) -> Vec<LaneCredential> {
    let mut out: Vec<LaneCredential> = Vec::new();
    // A workspace that refused the keyed free endpoint (the "Allow free
    // endpoints" opt-in) is served anonymously until that observation expires:
    // sending the key again would only burn a guaranteed 400.
    let keyed_allowed = mode.uses_key() && !crate::opencode_free::consent_gate_active(now);
    if keyed_allowed {
        for account in free_lane_accounts(state) {
            let Some(key) = account
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| crate::config::is_usable_api_key(key))
            else {
                continue;
            };
            let circuit_key = account.circuit_key(provider);
            if state.circuits.cooldown_remaining_ms(&circuit_key) > 0 {
                continue;
            }
            if state
                .circuits
                .account_model_cooldown_remaining_ms(provider, model, &circuit_key)
                > 0
            {
                continue;
            }
            let egress = Egress::from_proxy_url(account.proxy_url.as_deref());
            let bucket = with_egress(account_bucket(&account, provider), egress.label().as_deref());
            out.push(LaneCredential {
                bucket,
                secret: key.to_string(),
                account: Some(account),
                anonymous: false,
                egress,
            });
        }
        // `Key` is the legacy "use the configured credential" mode: a provider
        // with no managed pool keeps working from its env/config key.
        if out.is_empty() && mode == CredentialMode::Key {
            if let Some(key) = state
                .api_key_for(provider)
                .filter(|key| crate::config::is_usable_api_key(key))
            {
                out.push(LaneCredential {
                    bucket: config_bucket(provider),
                    secret: key,
                    account: None,
                    anonymous: false,
                    egress: Egress::Direct,
                });
            }
        }
    }
    // The anonymous credential is always the last resort — also in `Key` and
    // `Auto`: the lane accepts it with the same request shape, and it may have
    // its own budget while the account pool is exhausted.
    //
    // Its egress comes from the anonymous proxy pool: one candidate per exit,
    // ordered by the conversation's stable start index and capped so a single
    // request never burns the whole pool. With no pool configured the candidate
    // stays a plain direct dial, exactly like before.
    let anonymous_egresses = crate::opencode_free_egress::ordered_pool_egresses(&state.config, session_seed);
    if anonymous_egresses.is_empty() {
        out.push(LaneCredential {
            bucket: PUBLIC_BUCKET.to_string(),
            secret: crate::opencode_free::ANONYMOUS_KEY.to_string(),
            account: None,
            anonymous: true,
            egress: Egress::Direct,
        });
    } else {
        for egress in anonymous_egresses {
            let bucket = with_egress(PUBLIC_BUCKET.to_string(), egress.label().as_deref());
            out.push(LaneCredential {
                bucket,
                secret: crate::opencode_free::ANONYMOUS_KEY.to_string(),
                account: None,
                anonymous: true,
                egress,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reservation / release
// ---------------------------------------------------------------------------

/// RAII slot of one dispatched free-lane request: the bucket's `used` counter
/// was already charged when the slot was taken, and dropping the guard returns
/// the in-flight slot even when the future is cancelled by a client
/// disconnect.
#[derive(Debug)]
pub struct InFlightGuard {
    bucket: String,
}

impl InFlightGuard {
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        with_state(|state| {
            if let Some(slots) = state.in_flight.get_mut(&self.bucket) {
                *slots = slots.saturating_sub(1);
                if *slots == 0 {
                    let bucket = self.bucket.clone();
                    state.in_flight.remove(&bucket);
                }
            }
        });
    }
}

/// Outcome of trying to reserve one credential for dispatch.
#[derive(Debug)]
pub enum Begin {
    /// Reserved; send the request with `credential.secret`. The credential is
    /// boxed because a rendered candidate is much larger than the other
    /// variants (it owns a parsed egress) and `begin` is called per attempt.
    Ready(Box<LaneCredential>, InFlightGuard),
    /// Every candidate was idle but all their in-flight slots are taken.
    /// The caller may wait a short while and retry.
    Busy,
    /// Nothing may be dispatched: every bucket is exhausted or cooling.
    Exhausted(Exhaustion),
}

/// Why no credential is dispatchable, and when that is expected to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhaustion {
    /// Epoch ms of the next window reset.
    pub reset_at_ms: u128,
    /// Epoch ms of the earliest cooldown end (0 when only the window end
    /// explains the exhaustion).
    pub cooldown_until_ms: u128,
    /// True when the scope was learned as egress: the whole window is parked.
    pub egress: bool,
}

impl Exhaustion {
    /// Milliseconds until this exhaustion is expected to lift.
    pub fn wait_ms(&self, now: u128) -> u128 {
        if self.cooldown_until_ms > now {
            self.cooldown_until_ms - now
        } else {
            self.reset_at_ms.saturating_sub(now)
        }
    }
}

/// Reserve the next usable credential, in round-robin order, skipping buckets
/// in `tried` (already attempted for this candidate) and any whose in-flight
/// slots are full.
pub fn begin(
    credentials: &[LaneCredential],
    limits: &BudgetLimits,
    tried: &[String],
    now: u128,
) -> Begin {
    with_state(|state| {
        rollover(state, now);
        let count = credentials.len();
        if count == 0 {
            return Begin::Exhausted(Exhaustion {
                reset_at_ms: window_reset_at(now),
                cooldown_until_ms: 0,
                egress: state.file.scope == BucketScope::Egress,
            });
        }
        // The anonymous credential is a fallback, never a round-robin starting
        // point: rotate the account prefix only, then append every anonymous
        // candidate (one per proxy exit, in session-sticky order). Letting the
        // cursor start on `public` would silently skip a usable account (and
        // spend the anonymous budget instead).
        let account_indexes: Vec<usize> = credentials
            .iter()
            .enumerate()
            .filter(|(_, credential)| !credential.anonymous)
            .map(|(index, _)| index)
            .collect();
        let anonymous_indexes: Vec<usize> = credentials
            .iter()
            .enumerate()
            .filter(|(_, credential)| credential.anonymous)
            .map(|(index, _)| index)
            .collect();
        let mut order: Vec<usize> = Vec::with_capacity(count);
        if account_indexes.is_empty() {
            order.extend(anonymous_indexes.iter().copied());
        } else {
            let start = (state.cursor as usize) % account_indexes.len();
            for offset in 0..account_indexes.len() {
                order.push(account_indexes[(start + offset) % account_indexes.len()]);
            }
            order.extend(anonymous_indexes.iter().copied());
        }
        let mut busy = false;
        let mut earliest_cooldown = u128::MAX;
        let mut window_reset = 0u128;
        let mut considered = 0usize;
        let mut scope_blocked = 0usize;
        for index in order {
            let credential = &credentials[index];
            if tried.iter().any(|bucket| bucket == &credential.bucket) {
                continue;
            }
            considered += 1;
            // Exit-level gates run before any bucket is created: an
            // egress-scoped window or a cooling proxy must not leak one probe
            // through a lazily created bucket (this was the "one probe per
            // egress lesson" bug when the budget was global).
            let egress = credential.egress_label();
            let (exit_scope, exit_cooldown) = match egress {
                None => (state.file.scope, 0),
                Some(label) => state
                    .file
                    .exits
                    .get(label)
                    .map(|exit| (exit.scope, exit.cooldown_until_ms))
                    .unwrap_or((BucketScope::Unknown, 0)),
            };
            if exit_scope == BucketScope::Egress {
                scope_blocked += 1;
                window_reset = window_reset.max(window_reset_at(now));
                continue;
            }
            if exit_cooldown > now {
                earliest_cooldown = earliest_cooldown.min(exit_cooldown);
                continue;
            }
            let configured = limits.limit_for(&credential.bucket);
            let bucket = state
                .file
                .buckets
                .entry(credential.bucket.clone())
                .or_default();
            if bucket.reset_at_ms == 0 {
                bucket.reset_at_ms = window_reset_at(now);
            }
            if bucket.limit != configured {
                bucket.limit = configured;
            }
            let refreshed = refresh_learned(bucket, configured, now, limits.learned_limit_ttl_ms);
            if refreshed {
                state.dirty = true;
            }
            if bucket.cooldown_until_ms > now {
                earliest_cooldown = earliest_cooldown.min(bucket.cooldown_until_ms);
                continue;
            }
            if bucket.exhausted(configured) {
                window_reset = window_reset.max(bucket.reset_at_ms);
                continue;
            }
            let in_flight = state
                .in_flight
                .get(&credential.bucket)
                .copied()
                .unwrap_or(0);
            if limits.max_in_flight > 0 && in_flight >= limits.max_in_flight as u64 {
                busy = true;
                continue;
            }
            let Some(bucket) = state.file.buckets.get_mut(&credential.bucket) else {
                continue;
            };
            bucket.used += 1;
            let effective = bucket.effective_limit(configured);
            let warn_at_ninety = !bucket.warned && effective > 0 && bucket.used * 10 >= effective * 9;
            if warn_at_ninety {
                bucket.warned = true;
            }
            let bucket_key = credential.bucket.clone();
            *state.in_flight.entry(bucket_key.clone()).or_insert(0) += 1;
            state.cursor = state.cursor.wrapping_add(1);
            state.dirty = true;
            if warn_at_ninety {
                tracing::warn!(
                    bucket = %mask_bucket(&bucket_key),
                    used = bucket.used,
                    limit = effective,
                    "opencode free lane: credential budget at 90% of its UTC-day ceiling"
                );
            }
            return Begin::Ready(
                Box::new(credential.clone()),
                InFlightGuard {
                    bucket: bucket_key,
                },
            );
        }
        if busy {
            return Begin::Busy;
        }
        let reset_at = if window_reset > 0 {
            window_reset
        } else {
            window_reset_at(now)
        };
        Begin::Exhausted(Exhaustion {
            reset_at_ms: reset_at,
            cooldown_until_ms: if earliest_cooldown == u128::MAX {
                0
            } else {
                earliest_cooldown
            },
            egress: scope_blocked > 0 && scope_blocked == considered,
        })
    })
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// How a free-quota 429 was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaClass {
    /// A short burst rate limit (`Retry-After <= SHORT_LIMIT_MAX_MS`): cool the
    /// credential briefly, teach nothing, learn nothing about egress scope.
    Short,
    /// The UTC-day free budget (a long/absent `Retry-After`): cool to the
    /// window reset, record the learned ceiling and drive scope learning.
    Daily,
}

impl QuotaClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Short => "short_burst",
            Self::Daily => "daily_quota",
        }
    }
}

/// Classify a 429 by the `Retry-After` it advertised.
///
/// `Retry-After` is the only signal the lane publishes: a burst window asks for
/// a short retry (seconds), while the daily free budget points at the next
/// 00:00 UTC (thousands of seconds). A missing header on a
/// `FreeUsageLimitError` is treated as the daily budget — that error type is the
/// daily-quota answer, and over-cooling one credential for a few hours is
/// cheaper than burning an upstream 429 per account per window.
pub fn classify_quota_429(retry_after_ms: u128) -> QuotaClass {
    if retry_after_ms > 0 && retry_after_ms <= SHORT_LIMIT_MAX_MS {
        QuotaClass::Short
    } else {
        QuotaClass::Daily
    }
}

/// What one `429 FreeUsageLimitError` taught the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaOutcome {
    pub class: QuotaClass,
    pub scope: BucketScope,
    /// Learned ceiling now enforced on the bucket (0 when learning is off or
    /// nothing was learned).
    pub learned_limit: u64,
    /// Epoch ms of the observation behind `learned_limit` (0 = none).
    pub learned_at_ms: u128,
    /// Epoch ms until which the bucket is cooled.
    pub cooldown_until_ms: u128,
    /// True when this 429 upgraded the window's scope to egress.
    pub egress_learned: bool,
    /// True when the 429 was classified as a suspected session-admission
    /// refusal (low usage, short cooldown only).
    pub low_usage_suspected: bool,
    /// Low-usage 429 count of the current window after this observation.
    pub short_quota_refusals: u64,
}

/// Charge one upstream free-quota 429 to `bucket`.
///
/// The request was already counted when it was dispatched, so
/// `used_at_429 = bucket.used` includes it; that is the number the upstream
/// accepted before refusing, which is exactly the observed ceiling. The rule is
/// deliberately one-way — `learned_limit` only ever moves down — so a single
/// bad observation cannot raise the budget above what was proven.
pub fn note_quota_429(
    bucket_key: &str,
    model: &str,
    now: u128,
    retry_after_ms: u128,
    limits: &BudgetLimits,
) -> QuotaOutcome {
    let class = classify_quota_429(retry_after_ms);
    with_state(|state| {
        rollover(state, now);
        let reset_at = window_reset_at(now);
        let configured = limits.limit_for(bucket_key);
        // The egress dimension: `None` keeps the historical direct-dial
        // accounting (process-wide scope + `last_429`), `Some(label)` uses that
        // proxy's own scope/probe state — each exit is a distinct IP bucket.
        let (_, egress) = split_bucket(bucket_key);
        // 1) Ensure the bucket exists and read what the upstream accepted before
        //    refusing this request.
        let observed;
        {
            let bucket = state.file.buckets.entry(bucket_key.to_string()).or_default();
            if bucket.reset_at_ms == 0 {
                bucket.reset_at_ms = reset_at;
            }
            if bucket.limit != configured {
                bucket.limit = configured;
            }
            refresh_learned(bucket, configured, now, limits.learned_limit_ttl_ms);
            observed = bucket.used.max(1);
        }
        // 2) A daily-class 429 after barely any accepted requests is far more
        //    likely the upstream refusing *fresh sessions* (its admission quota)
        //    than a one-request daily quota. Treat it as a suspicion: short
        //    cooldown only, no learned ceiling, no egress lesson, no whole-window
        //    park. Three such refusals in the window upgrade to the real daily
        //    handling, so a genuinely tiny quota account is still honoured.
        let mut effective_class = class;
        let mut low_usage_suspected = false;
        if class == QuotaClass::Daily
            && limits.learned_min_samples > 0
            && observed < limits.learned_min_samples
        {
            state.file.session_admission_suspected = true;
            state.file.short_quota_refusals =
                state.file.short_quota_refusals.saturating_add(1);
            if state.file.short_quota_refusals < LOW_USAGE_UPGRADE_SAMPLES {
                effective_class = QuotaClass::Short;
                low_usage_suspected = true;
            }
        }
        let short_quota_refusals = state.file.short_quota_refusals;
        // 3) Cooldown: a suspicion backs off briefly; a burst answers exactly
        //    what it asked; a real daily quota parks until its reset.
        let mut learned_limit;
        let mut learned_at_ms;
        let mut cooldown_until_ms;
        {
            let bucket = state
                .file
                .buckets
                .get_mut(bucket_key)
                .expect("the bucket was just created");
            let cooldown_until = if low_usage_suspected {
                let cap = limits.short_quota_cooldown_ms.max(1);
                let backoff = if retry_after_ms > 0 {
                    retry_after_ms.min(cap)
                } else {
                    cap
                };
                now.saturating_add(backoff)
            } else {
                match effective_class {
                    // Short burst: cool for exactly what the upstream asked for.
                    QuotaClass::Short => now.saturating_add(retry_after_ms),
                    // Daily budget: park until the reset the response named (or
                    // the UTC window, when it named none).
                    QuotaClass::Daily => {
                        if retry_after_ms > 0 {
                            now.saturating_add(retry_after_ms)
                        } else {
                            reset_at
                        }
                    }
                }
            };
            bucket.cooldown_until_ms = bucket.cooldown_until_ms.max(cooldown_until);
            // 4) Learned ceiling only for a trusted daily quota.
            if effective_class == QuotaClass::Daily && limits.learned {
                // Record what the upstream accepted before refusing. The
                // ceiling only ever moves down (conservative), and the value
                // this window's first update replaced is kept for the egress
                // rollback below — including the "nothing learned yet" value,
                // so a first-ever egress-scoped 429 cannot leave a bogus
                // long-lived floor behind.
                if bucket.window_learned_prev.is_none() {
                    bucket.window_learned_prev = Some(LearnedPrev {
                        limit: bucket.learned_limit,
                        at_ms: bucket.learned_at_ms,
                    });
                }
                bucket.learned_limit = if bucket.learned_limit > 0 {
                    bucket.learned_limit.min(observed)
                } else {
                    observed
                };
                bucket.learned_at_ms = now;
            }
            learned_limit = bucket.learned_limit;
            learned_at_ms = bucket.learned_at_ms;
            cooldown_until_ms = bucket.cooldown_until_ms;
        }
        if effective_class == QuotaClass::Short {
            // A burst limit or an admission suspicion says nothing about the
            // day's budget: do not touch the scope tracker, so a following
            // genuine daily 429 still learns egress from a clean slate.
            state.dirty = true;
            return QuotaOutcome {
                class: effective_class,
                scope: exit_scope(state, egress),
                learned_limit,
                learned_at_ms,
                cooldown_until_ms,
                egress_learned: false,
                low_usage_suspected,
                short_quota_refusals,
            };
        }
        // 5) Daily quota: scope learning. Another credential of the *same model*
        //    answered a daily 429 within the probe window, so a shared egress
        //    budget is the only honest explanation left: stop probing and park
        //    every bucket of that egress.
        let scope_before = exit_scope(state, egress);
        let previous_429 = exit_last_429(state, egress);
        let learn_egress = scope_before != BucketScope::Egress
            && previous_429.as_ref().is_some_and(|last| {
                last.model == model
                    && last.bucket != bucket_key
                    && now.saturating_sub(last.at_ms) <= EGRESS_PROBE_WINDOW_MS
            });
        let mut egress_learned = false;
        if learn_egress {
            set_exit_scope(state, egress, BucketScope::Egress);
            egress_learned = true;
            for (key, bucket) in state.file.buckets.iter_mut() {
                // Park only the buckets that left through the same exit: a
                // direct-egress lesson must not freeze the proxy pool, and a
                // proxy's lesson says nothing about the local address.
                if bucket_egress(key) != egress {
                    continue;
                }
                bucket.cooldown_until_ms = bucket
                    .cooldown_until_ms
                    .max(reset_at)
                    .max(now.saturating_add(1));
                // The per-account ceiling of the 429s that produced this
                // lesson was never the account's own: restore whatever was
                // learned before this window and drop the provisional value.
                if let Some(previous) = bucket.window_learned_prev.take() {
                    bucket.learned_limit = previous.limit;
                    bucket.learned_at_ms = previous.at_ms;
                }
            }
            learned_limit = state
                .file
                .buckets
                .get(bucket_key)
                .map(|bucket| bucket.learned_limit)
                .unwrap_or(0);
            learned_at_ms = state
                .file
                .buckets
                .get(bucket_key)
                .map(|bucket| bucket.learned_at_ms)
                .unwrap_or(0);
            cooldown_until_ms = state
                .file
                .buckets
                .get(bucket_key)
                .map(|bucket| bucket.cooldown_until_ms)
                .unwrap_or(cooldown_until_ms);
        } else if scope_before != BucketScope::Egress {
            set_exit_scope(state, egress, BucketScope::Account);
        }
        set_exit_last_429(
            state,
            egress,
            Quota429 {
                model: model.to_string(),
                bucket: bucket_key.to_string(),
                at_ms: now,
            },
        );
        state.dirty = true;
        QuotaOutcome {
            class: effective_class,
            scope: exit_scope(state, egress),
            learned_limit,
            learned_at_ms,
            cooldown_until_ms,
            egress_learned,
            low_usage_suspected: false,
            short_quota_refusals,
        }
    })
}

/// Give back one reservation: the attempt reached no billable answer.
///
/// A fresh session refused by the upstream's session-admission gate is not
/// quota consumption — the credential's daily budget must not be charged for
/// it — so the caller refunds the reservation it took before dispatch.
pub fn refund(bucket_key: &str) {
    with_state(|state| {
        if let Some(bucket) = state.file.buckets.get_mut(bucket_key) {
            bucket.used = bucket.used.saturating_sub(1);
            // A provisional learned-ceiling update cannot have happened: the
            // caller refunds before `note_quota_429`.
            state.dirty = true;
        }
    });
}

/// A free-lane request answered 2xx on `bucket`: when it was the account
/// switch after a 429 of the same model, the scope settles as account-local.
pub fn note_success(bucket_key: &str, model: &str, now: u128) -> BucketScope {
    with_state(|state| {
        rollover(state, now);
        let (_, egress) = split_bucket(bucket_key);
        if exit_scope(state, egress) != BucketScope::Egress {
            let switched_after_429 = exit_last_429(state, egress).is_some_and(|last| {
                last.model == model
                    && last.bucket != bucket_key
                    && now.saturating_sub(last.at_ms) <= EGRESS_PROBE_WINDOW_MS
            });
            if switched_after_429 {
                set_exit_scope(state, egress, BucketScope::Account);
                // The 429 was account-scoped after all: the provisional learned
                // value is now proven, so commit it instead of restoring it at
                // the next rollover.
                if let Some(last) = exit_last_429(state, egress) {
                    let bucket = last.bucket;
                    if let Some(bucket) = state.file.buckets.get_mut(&bucket) {
                        bucket.window_learned_prev = None;
                    }
                }
            }
        }
        match egress {
            None => state.file.last_429 = None,
            Some(label) => {
                if let Some(exit) = state.file.exits.get_mut(label) {
                    exit.last_429 = None;
                }
            }
        }
        state.dirty = true;
        exit_scope(state, egress)
    })
}

/// Record a proxy/tunnel failure for `egress_label`: cool that exit briefly so
/// the next request uses another one instead of retrying a dead proxy.
///
/// Returns the epoch ms until which the exit is cooling (0 when the cooldown is
/// disabled). The direct dial never reaches this path (there is no exit to
/// cool); an upstream-side failure is a different classification.
pub fn note_egress_failure(egress_label: &str, now: u128, cooldown_ms: u128) -> u128 {
    if egress_label.is_empty() {
        return 0;
    }
    with_state(|state| {
        rollover(state, now);
        let exit = state.file.exits.entry(egress_label.to_string()).or_default();
        exit.failures = exit.failures.saturating_add(1);
        if cooldown_ms > 0 {
            exit.cooldown_until_ms = exit.cooldown_until_ms.max(now.saturating_add(cooldown_ms));
        }
        state.dirty = true;
        exit.cooldown_until_ms
    })
}

/// Is the free lane parked for the rest of the UTC window because its budget
/// turned out to be egress-scoped? Routing reads this to skip the whole Zen
/// roster instead of spending one local refusal per candidate.
pub fn egress_parked(now: u128) -> bool {
    with_state(|state| {
        rollover(state, now);
        state.file.scope == BucketScope::Egress
    })
}

/// For how long the free lane expects to be unusable (0 = usable). Used for the
/// downstream `Retry-After` of a combo that consists only of free-lane steps.
///
/// Includes the proxy exits: a cooling exit is a real wait for the next
/// request, even when the local bucket is still fresh.
pub fn cooldown_hint_ms(now: u128) -> u128 {
    with_state(|state| {
        rollover(state, now);
        let direct_park = if state.file.scope == BucketScope::Egress {
            window_reset_at(now).saturating_sub(now)
        } else {
            0
        };
        let bucket_wait = state
            .file
            .buckets
            .values()
            .map(|bucket| bucket.cooldown_until_ms.saturating_sub(now))
            .max()
            .unwrap_or(0);
        let exit_wait = state
            .file
            .exits
            .values()
            .map(|exit| {
                if exit.scope == BucketScope::Egress {
                    window_reset_at(now).saturating_sub(now)
                } else {
                    exit.cooldown_until_ms.saturating_sub(now)
                }
            })
            .max()
            .unwrap_or(0);
        direct_park.max(bucket_wait).max(exit_wait)
    })
}

/// Render a budget refusal. Status stays 429 so the downstream keeps
/// rate-limit semantics; the message names the local skip so an operator does
/// not blame the upstream request shape.
pub fn quota_error(exhaustion: &Exhaustion) -> ApiError {
    let until = if exhaustion.cooldown_until_ms > 0 {
        exhaustion.cooldown_until_ms
    } else {
        exhaustion.reset_at_ms
    };
    let scope = if exhaustion.egress {
        "the free budget is shared by this egress (all accounts are exhausted together)"
    } else {
        "every free-lane credential is exhausted or cooling"
    };
    let mut error = ApiError::new(
        429,
        format!(
            "OpenCode free lane skipped locally: {scope}; retry after {}. No upstream request was sent.",
            utc_iso(until)
        ),
    );
    error.code = QUOTA_ERROR_CODE.to_string();
    error
}

/// Render a budget refusal for a saturated pool: every credential is idle but
/// at its in-flight cap. Same local-skip semantics as [`quota_error`].
pub fn concurrency_error() -> ApiError {
    let mut error = ApiError::new(
        429,
        format!(
            "OpenCode free lane skipped locally: every credential is at its in-flight cap ({SLOT_WAIT_MS} ms wait exceeded). No upstream request was sent; retry shortly."
        ),
    );
    error.code = QUOTA_ERROR_CODE.to_string();
    error
}

/// Parse a `Retry-After` header value (delta-seconds; HTTP-date is rare enough
/// on this lane that the window reset is the safer fallback). Values above one
/// day are clamped, so a bad upstream header cannot park the lane for weeks.
pub fn parse_retry_after_ms(value: &str) -> Option<u128> {
    let seconds: u128 = value.trim().parse().ok()?;
    if seconds == 0 {
        return None;
    }
    // One day is the longest park the upstream can honestly ask for; anything
    // longer is a broken header, not a real quota window.
    Some(seconds.min(DAY_MS / 1000) * 1000)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for an epoch-ms instant (used in operator messages).
pub fn utc_iso(ms: u128) -> String {
    let seconds = (ms / 1000) as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

/// Civil date of a count of days since the Unix epoch (Howard Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}

// ---------------------------------------------------------------------------
// Observability snapshot
// ---------------------------------------------------------------------------

/// One credential view of the status endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BucketView {
    /// Masked bucket id (`conn:ab••••yz`, `config:opencode-zen`, `public`).
    pub id: String,
    pub used: u64,
    pub limit: u64,
    pub learned_limit: u64,
    /// Epoch ms of the observation behind `learned_limit` (0 = none).
    pub learned_at_ms: u128,
    pub effective_limit: u64,
    pub reset_at_ms: u128,
    pub cooldown_until_ms: u128,
    pub in_flight: u64,
    pub exhausted: bool,
    pub anonymous: bool,
}

/// One egress view of the status endpoint: the masked exit label, its
/// aggregated budget counters and its health.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EgressView {
    /// Masked `host:port` (`192.0.*.*:3128`), or `direct`.
    pub label: String,
    /// Tunnel mode: `direct`, `http_connect`, `socks5` or `socks5h`.
    pub mode: String,
    /// Requests charged to every credential through this exit in the window.
    pub used: u64,
    /// Sum of the configured ceilings of the buckets on this exit.
    pub limit: u64,
    pub in_flight: u64,
    /// Epoch ms until which the exit is cooling (tunnel failure, or the park
    /// of an egress-scoped 429 window).
    pub cooldown_until_ms: u128,
    /// Learned scope of this exit's budget (`unknown`/`account`/`egress`).
    pub scope: BucketScope,
    /// Usable right now: not cooling, not parked and not `egress`-scoped.
    pub healthy: bool,
}

/// The whole budget view of the status endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BudgetSnapshot {
    pub window_reset_at_ms: u128,
    pub scope: BucketScope,
    /// Low-usage 429s suggest the upstream is refusing fresh sessions rather
    /// than enforcing the request quota.
    pub session_admission_suspected: bool,
    /// Low-usage 429 count of the window (upgrades to daily handling at
    /// [`LOW_USAGE_UPGRADE_SAMPLES`]).
    pub short_quota_refusals: u64,
    pub buckets: Vec<BucketView>,
    /// Per-egress aggregation of the same counters (empty while the lane only
    /// dials directly and no bucket was charged yet).
    pub egresses: Vec<EgressView>,
}

/// Clone the budget for the status endpoint. Applies the window rollover first,
/// so a reader never sees yesterday's counters.
pub fn snapshot(now: u128, limits: &BudgetLimits) -> BudgetSnapshot {
    with_state(|state| {
        rollover(state, now);
        // A stale ceiling is raised lazily here too, so the status endpoint
        // shows the same number the next request will be judged against.
        for (key, bucket) in state.file.buckets.iter_mut() {
            let configured = limits.limit_for(key);
            if refresh_learned(bucket, configured, now, limits.learned_limit_ttl_ms) {
                state.dirty = true;
            }
        }
        let mut buckets: Vec<BucketView> = state
            .file
            .buckets
            .iter()
            .map(|(key, bucket)| {
                let configured = limits.limit_for(key);
                BucketView {
                    id: mask_bucket(key),
                    used: bucket.used,
                    limit: configured,
                    learned_limit: bucket.learned_limit,
                    learned_at_ms: bucket.learned_at_ms,
                    effective_limit: bucket.effective_limit(configured),
                    reset_at_ms: bucket.reset_at_ms,
                    cooldown_until_ms: bucket.cooldown_until_ms,
                    in_flight: state.in_flight.get(key).copied().unwrap_or(0),
                    exhausted: bucket.exhausted(configured),
                    // The key may carry an egress suffix
                    // (`public\0socks5h://host:port`); the credential half
                    // decides, not an exact string match.
                    anonymous: is_public_bucket(key),
                }
            })
            .collect();
        buckets.sort_by(|left, right| left.id.cmp(&right.id));
        // Egress aggregation: one row per exit a bucket was charged on. The
        // direct dial keeps its `direct` row so an operator sees the local
        // address bucket separately from the proxy pool.
        let mut egresses: BTreeMap<String, EgressView> = BTreeMap::new();
        for (key, bucket) in state.file.buckets.iter() {
            let label = bucket_egress(key);
            let (raw_label, view_label, mode) = match label {
                None => (
                    "direct".to_string(),
                    "direct".to_string(),
                    "direct".to_string(),
                ),
                Some(raw) => (
                    raw.to_string(),
                    egress_masked_label(raw),
                    egress_mode(raw),
                ),
            };
            let (scope, exit_cooldown) = match label {
                None => (state.file.scope, 0),
                Some(raw) => state
                    .file
                    .exits
                    .get(raw)
                    .map(|exit| (exit.scope, exit.cooldown_until_ms))
                    .unwrap_or((BucketScope::Unknown, 0)),
            };
            let row = egresses.entry(raw_label).or_insert_with(|| EgressView {
                label: view_label,
                mode,
                used: 0,
                limit: 0,
                in_flight: 0,
                cooldown_until_ms: exit_cooldown,
                scope,
                healthy: false,
            });
            row.used = row.used.saturating_add(bucket.used);
            row.limit = row
                .limit
                .saturating_add(bucket.effective_limit(limits.limit_for(key)));
            row.in_flight = row
                .in_flight
                .saturating_add(state.in_flight.get(key).copied().unwrap_or(0));
            row.cooldown_until_ms = row.cooldown_until_ms.max(bucket.cooldown_until_ms);
        }
        // An exit can carry state (a failure cooldown, a learned scope) before
        // any request was ever charged to it: show it too, so the operator can
        // see a dead proxy without waiting for a bucket.
        for (label, exit) in state.file.exits.iter() {
            if egresses.contains_key(label) {
                continue;
            }
            egresses.insert(
                label.clone(),
                EgressView {
                    label: egress_masked_label(label),
                    mode: egress_mode(label),
                    used: 0,
                    limit: 0,
                    in_flight: 0,
                    cooldown_until_ms: exit.cooldown_until_ms,
                    scope: exit.scope,
                    healthy: false,
                },
            );
        }
        let mut egresses: Vec<EgressView> = egresses.into_values().collect();
        for row in &mut egresses {
            row.healthy = row.scope != BucketScope::Egress && row.cooldown_until_ms <= now;
        }
        egresses.sort_by(|left, right| left.label.cmp(&right.label));
        BudgetSnapshot {
            window_reset_at_ms: window_reset_at(now),
            scope: state.file.scope,
            session_admission_suspected: state.file.session_admission_suspected,
            short_quota_refusals: state.file.short_quota_refusals,
            buckets,
            egresses,
        }
    })
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Load the on-disk counters (tolerates a missing or corrupt file: a fresh
/// window is a safe default, a wrong count is not).
pub fn bootstrap(data_dir: &Path) -> usize {
    let path = data_dir.join(BUDGET_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return 0;
    };
    let Ok(file) = serde_json::from_str::<BudgetFile>(&text) else {
        tracing::warn!(
            path = %path.display(),
            "opencode free lane: budget file is unreadable; starting a fresh window"
        );
        return 0;
    };
    let count = file.buckets.len();
    with_state(|state| {
        state.file = file;
        state.in_flight.clear();
        state.cursor = 0;
        state.dirty = false;
    });
    count
}

/// Persist the counters (atomic temp-file + rename, `0600`: the file is only
/// counters, but it lives next to credentials and there is no reason for it to
/// be world-readable).
pub fn save_disk(data_dir: &Path) -> std::io::Result<()> {
    static SAVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = SAVE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let file = with_state(|state| {
        state.dirty = false;
        state.file.clone()
    });
    let result: std::io::Result<()> = (|| {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(BUDGET_FILE);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        let _ = crate::set_file_mode_600(&tmp);
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        with_state(|state| state.dirty = true);
    }
    result
}

fn is_dirty() -> bool {
    with_state(|state| state.dirty)
}

/// Flush the dirty counters every couple of seconds. The write is cheap (a
/// ~1 KiB JSON file) and rare, so it can stay on the blocking side.
pub fn spawn_background(data_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(SAVE_INTERVAL_MS)).await;
            if !is_dirty() {
                continue;
            }
            if let Err(error) = save_disk(&data_dir) {
                tracing::warn!(
                    path = %data_dir.join(BUDGET_FILE).display(),
                    "opencode free lane: persisting budget failed: {error}"
                );
            }
        }
    });
}

/// Clear every counter and the learned scope. Manual reset hook (and a test
/// hook: the accounting is process-wide like the roster snapshot).
pub fn reset() {
    with_state(|state| {
        *state = State::default();
    });
}

/// Serializes unit tests that touch the process-wide budget.
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

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        let guard = test_lock();
        reset();
        guard
    }

    fn limits() -> BudgetLimits {
        BudgetLimits {
            per_account: 200,
            public: 200,
            learned: true,
            learned_limit_ttl_ms: 7 * DAY_MS,
            // The legacy tests pin the pre-low-usage behaviour; the gate has
            // dedicated tests below.
            learned_min_samples: 0,
            short_quota_cooldown_ms: 60_000,
            proxy_cooldown_ms: 30_000,
            max_in_flight: 2,
        }
    }

    /// Limits that enable the low-usage admission gate.
    fn guarded_limits() -> BudgetLimits {
        BudgetLimits {
            learned_min_samples: 10,
            ..limits()
        }
    }

    fn credential(bucket: &str) -> LaneCredential {
        LaneCredential {
            bucket: bucket.to_string(),
            secret: format!("key-{bucket}"),
            account: None,
            anonymous: bucket == PUBLIC_BUCKET,
            egress: Egress::Direct,
        }
    }

    /// A candidate for `bucket` that leaves through `egress`.
    fn proxied_credential(bucket: &str, egress: &str) -> LaneCredential {
        let egress = Egress::from_proxy_url(Some(egress));
        LaneCredential {
            bucket: with_egress(bucket.to_string(), egress.label().as_deref()),
            secret: format!("key-{bucket}"),
            account: None,
            anonymous: bucket == PUBLIC_BUCKET,
            egress,
        }
    }

    fn credentials(buckets: &[&str]) -> Vec<LaneCredential> {
        buckets.iter().map(|bucket| credential(bucket)).collect()
    }

    fn reserve(
        credentials: &[LaneCredential],
        limits: &BudgetLimits,
        tried: &[String],
        now: u128,
    ) -> InFlightGuard {
        match begin(credentials, limits, tried, now) {
            Begin::Ready(_, guard) => guard,
            other => panic!("expected a reservation, got {other:?}"),
        }
    }

    /// Mid-day instant that is stable for the UTC-window arithmetic:
    /// 2026-10-29T12:00:00Z.
    fn midday() -> u128 {
        1_793_275_200_000
    }

    #[test]
    fn utc_window_arithmetic_snaps_to_midnight() {
        let now = midday();
        assert_eq!(window_day(now), window_day(now + 3_600_000));
        let reset = window_reset_at(now);
        assert_eq!(reset % DAY_MS, 0);
        assert!(reset > now);
        assert_eq!(
            window_reset_at(reset),
            reset + DAY_MS,
            "next window starts at the boundary"
        );
        assert_eq!(utc_iso(reset), "2026-10-30T00:00:00Z");
        assert_eq!(utc_iso(now), "2026-10-29T12:00:00Z");
    }

    #[test]
    fn candidates_rotate_and_each_account_is_charged_once() {
        let _guard = serial();
        let creds = credentials(&["conn:a", "conn:b"]);
        let now = midday();
        let limits = limits();
        let guard = reserve(&creds, &limits, &[], now);
        let state = snapshot(now, &limits);
        assert_eq!(state.buckets.len(), 1, "one reservation charges one bucket");
        drop(guard);
        let guard = reserve(&creds, &limits, &["conn:a".to_string()], now);
        let snapshot = snapshot(now, &limits);
        let charged: Vec<&str> = snapshot
            .buckets
            .iter()
            .map(|bucket| bucket.id.as_str())
            .collect();
        assert!(
            charged.contains(&"conn:••••"),
            "the second reservation must land on the sibling account, got {charged:?}"
        );
        drop(guard);
    }

    #[test]
    fn configured_limit_is_enforced_and_the_public_bucket_is_separate() {
        let _guard = serial();
        let creds = credentials(&["conn:a", "public"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 1,
            public: 1,
            ..limits()
        };
        drop(reserve(&creds, &limits, &[], now));
        let (credential, guard) = match begin(&creds, &limits, &["conn:a".to_string()], now) {
            Begin::Ready(credential, guard) => (credential, guard),
            other => panic!("expected the anonymous fallback, got {other:?}"),
        };
        assert_eq!(
            credential.bucket, PUBLIC_BUCKET,
            "the anonymous credential is the fallback"
        );
        assert!(credential.anonymous);
        drop(guard);
        match begin(
            &creds,
            &limits,
            &["conn:a".to_string(), PUBLIC_BUCKET.to_string()],
            now,
        ) {
            Begin::Exhausted(exhaustion) => assert!(!exhaustion.egress),
            other => panic!("expected exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn quota_429_classification_splits_burst_from_daily_budget() {
        assert_eq!(classify_quota_429(1_000), QuotaClass::Short);
        assert_eq!(classify_quota_429(SHORT_LIMIT_MAX_MS), QuotaClass::Short);
        assert_eq!(
            classify_quota_429(SHORT_LIMIT_MAX_MS + 1),
            QuotaClass::Daily
        );
        assert_eq!(classify_quota_429(10 * 3_600_000), QuotaClass::Daily);
        // A `FreeUsageLimitError` without Retry-After is the daily answer.
        assert_eq!(classify_quota_429(0), QuotaClass::Daily);
    }

    #[test]
    fn learned_limit_stops_repeating_the_429() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = limits();
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now + 1_000, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        assert_eq!(
            outcome.learned_limit, 1,
            "one request was accepted before the daily 429"
        );
        assert_eq!(outcome.learned_at_ms, now + 1_000);
        assert_eq!(outcome.scope, BucketScope::Account);
        match begin(&creds, &limits, &[], now + 2_000) {
            Begin::Exhausted(exhaustion) => {
                assert_eq!(
                    exhaustion.cooldown_until_ms,
                    window_reset_at(now),
                    "with no Retry-After the credential cools until the window reset"
                );
                assert_eq!(exhaustion.reset_at_ms, window_reset_at(now));
                assert!(!exhaustion.egress);
            }
            other => panic!("the learned ceiling must skip the account, got {other:?}"),
        }
    }

    #[test]
    fn a_learned_ceiling_only_moves_down_and_survives_windows() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = limits();
        // Window 1: five accepted requests, then the daily 429 => 5.
        for _ in 0..5 {
            drop(reserve(&creds, &limits, &[], now));
        }
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.learned_limit, 5);
        // Window 2 keeps the persisted ceiling: exactly five requests, not 200.
        let day2 = window_reset_at(now) + 1_000;
        for _ in 0..5 {
            drop(reserve(&creds, &limits, &[], day2));
        }
        match begin(&creds, &limits, &[], day2) {
            Begin::Exhausted(_) => {}
            other => panic!("the learned ceiling must survive the rollover, got {other:?}"),
        }
        // Window 3: the upstream refuses after a single request. The learned
        // ceiling only moves down, so it becomes 1.
        let day3 = day2 + DAY_MS;
        drop(reserve(&creds, &limits, &[], day3));
        let outcome = note_quota_429("conn:a", "flint-40b-free", day3, 0, &limits);
        assert_eq!(outcome.learned_limit, 1, "the conservative rule wins");
        let snapshot = snapshot(day3, &limits);
        assert_eq!(snapshot.buckets[0].learned_limit, 1);
        assert_eq!(snapshot.buckets[0].learned_at_ms, day3);
    }

    #[test]
    fn a_stale_learned_ceiling_is_raised_by_twenty_percent_after_its_ttl() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 200,
            ..limits()
        };
        for _ in 0..100 {
            drop(reserve(&creds, &limits, &[], now));
        }
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(snapshot(now, &limits).buckets[0].learned_limit, 100);

        // Seven days later the ceiling is re-raised one conservative step.
        let later = now + 8 * DAY_MS;
        let snapshot = snapshot(later, &limits);
        assert_eq!(snapshot.buckets[0].learned_limit, 120, "100 + 20%");
        assert_eq!(snapshot.buckets[0].learned_at_ms, later);
        assert_eq!(snapshot.buckets[0].effective_limit, 120);
        // ...and the next 429 is judged against the new ceiling.
        match begin(&creds, &limits, &[], later) {
            Begin::Ready(_, guard) => drop(guard),
            other => panic!("the raised ceiling must re-open the bucket, got {other:?}"),
        }
    }

    #[test]
    fn an_old_learned_ceiling_returns_to_the_configured_limit() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 100,
            ..limits()
        };
        // 99 accepted requests then the daily 429 at the configured ceiling.
        for _ in 0..99 {
            drop(reserve(&creds, &limits, &[], now));
        }
        // Simulate the learned value being one step below the config ceiling.
        note_quota_429("conn:a", "m-free", now, 0, &limits);
        let snapshot = snapshot(now + 8 * DAY_MS, &limits);
        assert_eq!(
            snapshot.buckets[0].learned_limit, 0,
            "a raised ceiling that reaches the configured limit stops being a floor"
        );
    }

    #[test]
    fn a_low_usage_daily_429_is_suspected_session_admission() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = guarded_limits();
        drop(reserve(&creds, &limits, &[], now));
        // One accepted request, then the daily-style 429: far too little use to
        // be the daily budget.
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Short);
        assert!(outcome.low_usage_suspected);
        assert_eq!(outcome.learned_limit, 0, "no learned ceiling from a suspicion");
        assert_eq!(outcome.learned_at_ms, 0);
        assert_eq!(outcome.scope, BucketScope::Unknown, "no egress lesson");
        assert_eq!(outcome.short_quota_refusals, 1);
        assert_eq!(
            outcome.cooldown_until_ms,
            now + limits.short_quota_cooldown_ms,
            "only a short back-off, not a window park"
        );
        let snapshot = snapshot(now + 1_000, &limits);
        assert!(snapshot.session_admission_suspected);
        assert_eq!(snapshot.short_quota_refusals, 1);
        assert_eq!(snapshot.buckets[0].learned_limit, 0);
        // The short back-off expires and the credential is probed again.
        drop(reserve(&creds, &limits, &[], now + limits.short_quota_cooldown_ms + 1));
    }

    #[test]
    fn the_third_low_usage_429_upgrades_to_the_daily_quota() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let start = midday();
        let limits = guarded_limits();
        let mut now = start;
        for round in 1..=(LOW_USAGE_UPGRADE_SAMPLES - 1) {
            drop(reserve(&creds, &limits, &[], now));
            let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
            assert_eq!(outcome.class, QuotaClass::Short, "round {round}");
            assert!(outcome.low_usage_suspected);
            assert_eq!(outcome.short_quota_refusals, round);
            now += limits.short_quota_cooldown_ms + 1;
        }
        // The third low-usage 429 is the upgrade: from here on this is treated
        // as a real daily quota (learned ceiling + window cooldown).
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        assert!(!outcome.low_usage_suspected);
        assert_eq!(outcome.short_quota_refusals, LOW_USAGE_UPGRADE_SAMPLES);
        assert_eq!(outcome.learned_limit, 3, "three accepted requests observed");
        assert_eq!(outcome.cooldown_until_ms, window_reset_at(now));
        assert_eq!(outcome.scope, BucketScope::Account);
        let snapshot = snapshot(now, &limits);
        assert!(snapshot.session_admission_suspected, "the suspicion stays visible");
    }

    #[test]
    fn enough_samples_learn_the_ceiling_normally() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 200,
            ..guarded_limits()
        };
        for _ in 0..limits.learned_min_samples {
            drop(reserve(&creds, &limits, &[], now));
        }
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        assert!(!outcome.low_usage_suspected);
        assert_eq!(outcome.short_quota_refusals, 0, "the gate never fired");
        assert_eq!(outcome.learned_limit, limits.learned_min_samples);
        assert_eq!(outcome.cooldown_until_ms, window_reset_at(now));
        assert_eq!(outcome.scope, BucketScope::Account);
        assert!(!snapshot(now, &limits).session_admission_suspected);
    }

    #[test]
    fn a_zero_min_samples_keeps_the_legacy_daily_handling() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            learned_min_samples: 0,
            ..guarded_limits()
        };
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        assert!(!outcome.low_usage_suspected);
        assert_eq!(outcome.learned_limit, 1);
        assert_eq!(outcome.cooldown_until_ms, window_reset_at(now));
    }

    #[test]
    fn the_low_usage_suspicion_survives_a_restart_and_still_upgrades() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let start = midday();
        let limits = guarded_limits();
        let creds = credentials(&["conn:a"]);
        let mut now = start;
        // Two low-usage refusals in the window, then a restart.
        for _ in 0..(LOW_USAGE_UPGRADE_SAMPLES - 1) {
            drop(reserve(&creds, &limits, &[], now));
            note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
            now += limits.short_quota_cooldown_ms + 1;
        }
        save_disk(dir.path()).unwrap();
        reset();
        assert_eq!(bootstrap(dir.path()), 1);
        let restored = snapshot(now, &limits);
        assert!(restored.session_admission_suspected);
        assert_eq!(restored.short_quota_refusals, LOW_USAGE_UPGRADE_SAMPLES - 1);
        // The third refusal (after the restart) still upgrades.
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        assert_eq!(outcome.short_quota_refusals, LOW_USAGE_UPGRADE_SAMPLES);
        assert!(outcome.learned_limit >= 1);
    }

    #[test]
    fn a_new_window_clears_the_low_usage_suspicion() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let now = midday();
        let limits = guarded_limits();
        let creds = credentials(&["conn:a"]);
        drop(reserve(&creds, &limits, &[], now));
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        save_disk(dir.path()).unwrap();
        let next_day = window_reset_at(now) + 1_000;
        let snapshot = snapshot(next_day, &limits);
        assert!(!snapshot.session_admission_suspected);
        assert_eq!(snapshot.short_quota_refusals, 0);
    }

    #[test]
    fn a_short_burst_429_cools_briefly_and_teaches_nothing() {
        let _guard = serial();
        let creds = credentials(&["conn:a", "conn:b"]);
        let now = midday();
        let limits = limits();
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 90_000, &limits);
        assert_eq!(outcome.class, QuotaClass::Short);
        assert_eq!(outcome.learned_limit, 0, "a burst limit teaches no daily ceiling");
        assert_eq!(outcome.scope, BucketScope::Unknown);
        assert_eq!(outcome.cooldown_until_ms, now + 90_000);
        // A second short 429 on a sibling must not be read as an egress budget.
        drop(reserve(&creds, &limits, &[], now + 1_000));
        let second = note_quota_429("conn:b", "flint-40b-free", now + 1_000, 45_000, &limits);
        assert_eq!(second.class, QuotaClass::Short);
        assert!(!second.egress_learned);
        assert_eq!(second.scope, BucketScope::Unknown);
        assert_eq!(snapshot(now + 2_000, &limits).scope, BucketScope::Unknown);
        // When the advertised retry elapses the credential is used again.
        match begin(&creds, &limits, &[], now + 91_000) {
            Begin::Ready(_, guard) => drop(guard),
            other => panic!("a short cooldown must expire on time, got {other:?}"),
        }
    }

    #[test]
    fn short_cooldowns_do_not_pollute_the_window_scope() {
        let _guard = serial();
        let now = midday();
        let limits = limits();
        // A burst 429, then a genuine daily 429 on another account: the daily
        // one is still the first daily observation, so no egress lesson yet.
        note_quota_429("conn:a", "flint-40b-free", now, 30_000, &limits);
        let outcome = note_quota_429("conn:b", "flint-40b-free", now + 1_000, 0, &limits);
        assert!(!outcome.egress_learned);
        assert_eq!(outcome.scope, BucketScope::Account);
    }

    #[test]
    fn a_second_account_daily_429_learns_the_egress_scope() {
        let _guard = serial();
        let creds = credentials(&["conn:a", "conn:b"]);
        let now = midday();
        let limits = limits();
        drop(reserve(&creds, &limits, &[], now));
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        drop(reserve(&creds, &limits, &["conn:a".to_string()], now + 1_000));
        let outcome = note_quota_429("conn:b", "flint-40b-free", now + 1_000, 0, &limits);
        assert!(
            outcome.egress_learned,
            "same model, another account, same minute -> egress"
        );
        assert_eq!(outcome.scope, BucketScope::Egress);
        match begin(&creds, &limits, &[], now + 3_000) {
            Begin::Exhausted(exhaustion) => {
                assert!(exhaustion.egress);
                assert_eq!(
                    exhaustion.reset_at_ms,
                    window_reset_at(now),
                    "egress scope parks the whole window"
                );
            }
            other => panic!("egress scope must not probe accounts, got {other:?}"),
        }
        // Neither account's single-request observation may survive as a
        // learned ceiling: both were egress artifacts, not account limits.
        for bucket in snapshot(now + 4_000, &limits).buckets {
            assert_eq!(
                bucket.learned_limit, 0,
                "{} must not keep an egress artifact as its daily ceiling",
                bucket.id
            );
        }
    }

    #[test]
    fn a_success_after_the_account_switch_commits_the_learned_ceiling() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits {
            per_account: 200,
            ..limits()
        };
        // Window 1: prove a ceiling of 4 for conn:a.
        for _ in 0..4 {
            drop(reserve(&credentials(&["conn:a"]), &limits, &[], now));
        }
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(snapshot(now, &limits).buckets[0].learned_limit, 4);
        // Window 2: conn:a 429s after a single request (provisional 1), then a
        // sibling account answers 200: the 429 was account-scoped, so the
        // provisional value is committed...
        let day2 = window_reset_at(now) + 1_000;
        drop(reserve(&credentials(&["conn:a"]), &limits, &[], day2));
        let provisional = note_quota_429("conn:a", "flint-40b-free", day2, 0, &limits);
        assert_eq!(provisional.learned_limit, 1);
        let scope = note_success("conn:b", "flint-40b-free", day2 + 1_000);
        assert_eq!(scope, BucketScope::Account);
        // ...and the next window keeps it.
        let day3 = day2 + DAY_MS;
        let snapshot = snapshot(day3, &limits);
        assert_eq!(
            snapshot.buckets[0].learned_limit, 1,
            "a committed observation survives the rollover"
        );
        // A later unrelated 429 must not be re-attributed to egress.
        let outcome = note_quota_429("conn:c", "other-free", day3, 0, &limits);
        assert!(!outcome.egress_learned);
    }

    #[test]
    fn a_different_model_does_not_learn_egress() {
        let _guard = serial();
        let now = midday();
        let limits = limits();
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        note_quota_429("conn:b", "other-free", now + 1_000, 0, &limits);
        let outcome = snapshot(now + 2_000, &limits).scope;
        assert_eq!(
            outcome,
            BucketScope::Account,
            "a sibling model can have its own budget"
        );
    }

    #[test]
    fn the_window_rollover_resets_usage_but_keeps_the_learned_ceiling() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 1,
            ..limits()
        };
        drop(reserve(&creds, &limits, &[], now));
        note_quota_429("conn:a", "flint-40b-free", now + 1_000, 0, &limits);
        match begin(&creds, &limits, &[], now + 2_000) {
            Begin::Exhausted(_) => {}
            other => panic!("expected exhaustion in the old window, got {other:?}"),
        }
        let next_day = window_reset_at(now) + 1_000;
        // A fresh window allows a request again (usage is zero), while the
        // learned ceiling itself is remembered.
        drop(reserve(&creds, &limits, &[], next_day));
        let snapshot = snapshot(next_day, &limits);
        assert_eq!(snapshot.buckets[0].used, 1);
        assert_eq!(
            snapshot.buckets[0].learned_limit, 1,
            "learning is a property of the credential, not of the day"
        );
        assert_eq!(
            snapshot.buckets[0].learned_at_ms,
            now + 1_000,
            "the observation timestamp is kept until the ceiling is re-evaluated"
        );
        assert_eq!(snapshot.scope, BucketScope::Unknown, "scope is per window");
    }

    #[test]
    fn egress_scope_restores_the_pre_window_learned_ceiling() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits {
            per_account: 200,
            ..limits()
        };
        // Window 1: prove a genuine ceiling of 4 for conn:a.
        for _ in 0..4 {
            drop(reserve(&credentials(&["conn:a"]), &limits, &[], now));
        }
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        // Window 2: an egress-scoped 429 after a single request must NOT
        // overwrite that proven ceiling with the egress artifact (1).
        let day2 = window_reset_at(now) + 1_000;
        let creds = credentials(&["conn:a", "conn:b"]);
        drop(reserve(&creds, &limits, &[], day2));
        note_quota_429("conn:a", "flint-40b-free", day2, 0, &limits);
        drop(reserve(&creds, &limits, &["conn:a".to_string()], day2 + 1_000));
        let outcome = note_quota_429("conn:b", "flint-40b-free", day2 + 1_000, 0, &limits);
        assert!(outcome.egress_learned);
        let snapshot = snapshot(day2 + 2_000, &limits);
        assert_eq!(snapshot.scope, BucketScope::Egress);
        let restored = snapshot
            .buckets
            .iter()
            .find(|bucket| bucket.used == 1 && bucket.learned_limit == 4)
            .expect("the measured ceiling is restored, not the egress artifact");
        assert!(
            restored.cooldown_until_ms >= window_reset_at(day2),
            "all buckets are parked for the window"
        );
    }

    #[test]
    fn learned_limit_is_off_when_configured_off() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            learned: false,
            per_account: 1,
            ..limits()
        };
        drop(reserve(&creds, &limits, &[], now));
        note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        let snapshot = snapshot(now + 1_000, &limits);
        assert_eq!(snapshot.buckets[0].learned_limit, 0);
        // The credential is still cooled for the window, then re-probed at the
        // configured ceiling.
        match begin(&creds, &limits, &[], now + 2_000) {
            Begin::Exhausted(_) => {}
            other => panic!("the daily cooldown still applies, got {other:?}"),
        }
        drop(reserve(&creds, &limits, &[], window_reset_at(now) + 1_000));
    }

    #[test]
    fn the_in_flight_limit_caps_one_credential_and_releases_on_drop() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            max_in_flight: 1,
            ..limits()
        };
        let guard = reserve(&creds, &limits, &[], now);
        match begin(&creds, &limits, &[], now) {
            Begin::Busy => {}
            other => panic!("expected busy, got {other:?}"),
        }
        drop(guard);
        drop(reserve(&creds, &limits, &[], now));
    }

    #[test]
    fn zero_in_flight_limit_disables_the_cap() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            max_in_flight: 0,
            per_account: 10,
            ..limits()
        };
        let first = reserve(&creds, &limits, &[], now);
        let second = reserve(&creds, &limits, &[], now);
        drop((first, second));
    }

    #[test]
    fn a_refunded_attempt_does_not_consume_the_budget() {
        let _guard = serial();
        let creds = credentials(&["conn:a"]);
        let now = midday();
        let limits = BudgetLimits {
            per_account: 1,
            ..limits()
        };
        let guard = reserve(&creds, &limits, &[], now);
        drop(guard);
        // The fresh-session admission refusal is not quota consumption: the
        // caller refunds the reservation before treating it as one.
        refund("conn:a");
        assert_eq!(snapshot(now, &limits).buckets[0].used, 0);
        drop(reserve(&creds, &limits, &[], now));
        assert_eq!(snapshot(now, &limits).buckets[0].used, 1);
        match begin(&creds, &limits, &[], now) {
            Begin::Exhausted(_) => {}
            other => panic!("the refunded slot must have been reusable, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_parses_delta_seconds_and_clamps_absurd_values() {
        assert_eq!(parse_retry_after_ms("120"), Some(120_000));
        assert_eq!(parse_retry_after_ms(" 45 "), Some(45_000));
        assert_eq!(parse_retry_after_ms("0"), None);
        assert_eq!(parse_retry_after_ms("soon"), None);
        assert_eq!(
            parse_retry_after_ms("999999999"),
            Some(DAY_MS),
            "a bogus header must not park the lane for years"
        );
    }

    #[test]
    fn persistence_round_trips_the_window_scope_and_learned_ceiling() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let now = midday();
        let limits = limits();
        let creds = credentials(&["conn:a"]);
        drop(reserve(&creds, &limits, &[], now));
        note_quota_429("conn:a", "flint-40b-free", now + 1_000, 0, &limits);
        save_disk(dir.path()).unwrap();
        let before = snapshot(now + 2_000, &limits);

        reset();
        assert_eq!(snapshot(now + 2_000, &limits).buckets.len(), 0);
        assert_eq!(bootstrap(dir.path()), 1, "one bucket is restored");
        let after = snapshot(now + 2_000, &limits);
        assert_eq!(after.scope, before.scope);
        assert_eq!(after.buckets.len(), before.buckets.len());
        assert_eq!(after.buckets[0].used, before.buckets[0].used);
        assert_eq!(after.buckets[0].learned_limit, before.buckets[0].learned_limit);
        assert_eq!(after.buckets[0].learned_at_ms, before.buckets[0].learned_at_ms);
        assert_eq!(
            after.buckets[0].cooldown_until_ms,
            before.buckets[0].cooldown_until_ms
        );
        assert_eq!(after.window_reset_at_ms, before.window_reset_at_ms);
    }

    #[test]
    fn a_corrupt_budget_file_starts_a_fresh_window() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BUDGET_FILE), b"not json").unwrap();
        assert_eq!(bootstrap(dir.path()), 0);
        assert_eq!(snapshot(midday(), &limits()).buckets.len(), 0);
    }

    #[test]
    fn bucket_ids_are_masked_and_anonymous_is_recognised() {
        assert_eq!(mask_bucket(PUBLIC_BUCKET), "public");
        assert_eq!(
            mask_bucket("conn:abcdefghijklmnop"),
            "conn:abcd••••mnop",
            "only a short prefix/suffix of an internal id may leak"
        );
        assert_eq!(mask_bucket("conn:short"), "conn:••••");
        assert_eq!(config_bucket("opencode-zen"), "config:opencode-zen");
        assert_eq!(
            account_bucket(
                &AccountCredential {
                    connection_id: "conn-1".into(),
                    name: String::new(),
                    api_key: None,
                    base_url: None,
                    api_type: None,
                    proxy_url: None,
                },
                "opencode-zen"
            ),
            "conn:conn-1"
        );
    }

    #[test]
    fn the_budget_refusal_keeps_rate_limit_semantics() {
        let error = quota_error(&Exhaustion {
            reset_at_ms: midday() + 3_600_000,
            cooldown_until_ms: 0,
            egress: false,
        });
        assert_eq!(error.status, 429);
        assert_eq!(error.code, QUOTA_ERROR_CODE);
        assert_eq!(error.etype, "rate_limit_error");
        assert!(
            error.message.contains("2026-10-29T13:00:00Z"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("FreeUsageLimitError"));
    }

    // -----------------------------------------------------------------
    // Egress dimension: proxy buckets, exit cooldowns, per-exit scope
    // -----------------------------------------------------------------

    const EXIT_A: &str = "http://127.0.0.1:3101";
    const EXIT_B: &str = "http://127.0.0.1:3102";

    #[test]
    fn a_direct_bucket_keeps_its_historical_key_and_a_proxy_gets_its_own() {
        assert_eq!(with_egress(PUBLIC_BUCKET.to_string(), None), "public");
        assert_eq!(
            with_egress("conn:abc".to_string(), Some(EXIT_A)),
            format!("conn:abc{EGRESS_KEY_SEPARATOR}{EXIT_A}")
        );
        assert_eq!(split_bucket("public"), ("public", None));
        assert_eq!(
            split_bucket(&format!("public{EGRESS_KEY_SEPARATOR}{EXIT_A}")),
            ("public", Some(EXIT_A))
        );
        assert!(is_public_bucket(&format!(
            "public{EGRESS_KEY_SEPARATOR}{EXIT_A}"
        )));
        assert_eq!(mask_bucket("public"), "public");
        let masked = mask_bucket(&format!("public{EGRESS_KEY_SEPARATOR}{EXIT_A}"));
        assert!(masked.starts_with("public @ http://127.0.*.*:"), "{masked}");
        assert!(!masked.contains(EGRESS_KEY_SEPARATOR));
        let account = mask_bucket(&format!("conn:abcdefghijkl{EGRESS_KEY_SEPARATOR}{EXIT_A}"));
        assert!(account.starts_with("conn:abcd••••ijkl @ "), "{account}");
    }

    /// D1: an anonymous bucket that leaves through a proxy must stay
    /// anonymous in the status view (the composite key is
    /// `public\0socks5h://host:port`), and it must be judged against the
    /// `public` ceiling, not the per-account one.
    #[test]
    fn an_anonymous_bucket_on_a_proxy_stays_anonymous() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits {
            public: 1,
            per_account: 200,
            ..limits()
        };
        let creds = vec![proxied_credential(
            PUBLIC_BUCKET,
            "socks5h://127.0.0.1:1080",
        )];
        drop(reserve(&creds, &limits, &[], now));
        let snapshot = snapshot(now, &limits);
        assert_eq!(snapshot.buckets.len(), 1);
        let bucket = &snapshot.buckets[0];
        assert!(bucket.anonymous, "a proxied anonymous bucket must stay anonymous");
        assert!(
            bucket.id.starts_with("public @ socks5h://127.0."),
            "{}",
            bucket.id
        );
        assert_eq!(
            bucket.limit, 1,
            "the anonymous bucket is judged against daily_limit_public"
        );
        assert!(bucket.exhausted);
    }

    #[test]
    fn the_same_credential_has_one_budget_per_exit() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits { public: 1, per_account: 200, ..limits() };
        let creds = vec![
            proxied_credential(PUBLIC_BUCKET, EXIT_A),
            proxied_credential(PUBLIC_BUCKET, EXIT_B),
        ];
        let first = match begin(&creds, &limits, &[], now) {
            Begin::Ready(credential, guard) => {
                assert_eq!(credential.egress_label(), Some(EXIT_A));
                guard
            }
            other => panic!("expected the first exit, got {other:?}"),
        };
        drop(first);
        // Exit A is spent; the same credential on exit B is a fresh bucket.
        let second = match begin(&creds, &limits, &[], now) {
            Begin::Ready(credential, guard) => {
                assert_eq!(credential.egress_label(), Some(EXIT_B));
                guard
            }
            other => panic!("a different exit must be a different bucket, got {other:?}"),
        };
        drop(second);
        match begin(&creds, &limits, &[], now) {
            Begin::Exhausted(exhaustion) => assert!(!exhaustion.egress),
            other => panic!("both exits are spent, got {other:?}"),
        }
        let snapshot = snapshot(now, &limits);
        assert_eq!(snapshot.egresses.len(), 2, "{:?}", snapshot.egresses);
        assert!(snapshot.egresses.iter().all(|row| row.used == 1 && row.limit == 1));
    }

    #[test]
    fn a_daily_429_on_one_exit_rotates_instead_of_parking_the_others() {
        let _guard = serial();
        let now = midday();
        let limits = limits();
        let creds = vec![
            proxied_credential(PUBLIC_BUCKET, EXIT_A),
            proxied_credential(PUBLIC_BUCKET, EXIT_B),
        ];
        let first_exit = match begin(&creds, &limits, &[], now) {
            Begin::Ready(credential, guard) => {
                drop(guard);
                credential.bucket
            }
            other => panic!("expected a reservation, got {other:?}"),
        };
        let outcome = note_quota_429(&first_exit, "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.class, QuotaClass::Daily);
        // The next attempt must use the *other* exit, not the cooled bucket.
        let next = match begin(&creds, &limits, std::slice::from_ref(&first_exit), now) {
            Begin::Ready(credential, guard) => {
                assert_ne!(credential.bucket, first_exit);
                guard
            }
            other => panic!("the sibling exit must still serve, got {other:?}"),
        };
        drop(next);
        let snapshot = snapshot(now, &limits);
        let cooled = snapshot
            .egresses
            .iter()
            .find(|row| !row.healthy)
            .expect("the cooled exit is visible");
        assert_eq!(cooled.used, 1);
        assert!(cooled.cooldown_until_ms >= window_reset_at(now));
    }

    #[test]
    fn an_egress_scope_lesson_parks_only_that_exit() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits { learned_min_samples: 0, ..limits() };
        // Two credentials leaving through the same proxy: two 429s inside the
        // probe window mean *that exit's* IP budget is shared.
        let shared_a = proxied_credential("conn:aaaa1111", EXIT_A);
        let shared_b = proxied_credential("conn:bbbb2222", EXIT_A);
        let other = proxied_credential("conn:cccc3333", EXIT_B);
        let direct = credential("public");
        let _ = begin(
            std::slice::from_ref(&shared_a),
            &limits,
            &[],
            now,
        );
        let a_outcome = note_quota_429(&shared_a.bucket, "flint-40b-free", now, 0, &limits);
        assert_eq!(a_outcome.scope, BucketScope::Account);
        let _ = begin(
            std::slice::from_ref(&shared_b),
            &limits,
            &[],
            now + 1_000,
        );
        let b_outcome =
            note_quota_429(&shared_b.bucket, "flint-40b-free", now + 1_000, 0, &limits);
        assert!(b_outcome.egress_learned, "same exit, same model, same minute");
        assert_eq!(b_outcome.scope, BucketScope::Egress);
        // The whole exit A is parked...
        match begin(&[shared_a.clone(), shared_b.clone()], &limits, &[], now + 2_000) {
            Begin::Exhausted(exhaustion) => assert!(exhaustion.egress),
            other => panic!("exit A must be parked, got {other:?}"),
        }
        // ...but exit B and the direct dial are untouched.
        match begin(
            &[other.clone(), direct.clone()],
            &limits,
            &[],
            now + 2_000,
        ) {
            Begin::Ready(credential, guard) => {
                assert_eq!(credential.egress_label(), Some(EXIT_B));
                drop(guard);
            }
            other => panic!("another exit must stay usable, got {other:?}"),
        }
        assert_eq!(snapshot(now + 3_000, &limits).scope, BucketScope::Unknown);
    }

    #[test]
    fn a_tunnel_failure_cools_only_its_exit_briefly() {
        let _guard = serial();
        let now = midday();
        let limits = limits();
        let creds = vec![
            proxied_credential(PUBLIC_BUCKET, EXIT_A),
            proxied_credential(PUBLIC_BUCKET, EXIT_B),
        ];
        let until = note_egress_failure(EXIT_A, now, limits.proxy_cooldown_ms);
        assert_eq!(until, now + limits.proxy_cooldown_ms);
        // The cooling exit is skipped, the sibling serves.
        match begin(&creds, &limits, &[], now + 1_000) {
            Begin::Ready(credential, guard) => {
                assert_eq!(credential.egress_label(), Some(EXIT_B));
                drop(guard);
            }
            other => panic!("the healthy exit must serve, got {other:?}"),
        }
        // Once the short cooldown is over the exit is retried.
        match begin(
            &[creds[0].clone()],
            &limits,
            &[],
            now + limits.proxy_cooldown_ms + 1,
        ) {
            Begin::Ready(_, guard) => drop(guard),
            other => panic!("the cooldown must expire, got {other:?}"),
        }
        let snapshot = snapshot(now + 2_000, &limits);
        let exit_a = snapshot
            .egresses
            .iter()
            .find(|row| row.label.contains(":3101"))
            .expect("exit A is in the egress view");
        assert!(!exit_a.healthy);
        assert_eq!(exit_a.cooldown_until_ms, until);
    }

    #[test]
    fn the_direct_path_never_gains_an_egress_dimension() {
        let _guard = serial();
        let now = midday();
        let limits = BudgetLimits { per_account: 1, ..limits() };
        let creds = credentials(&["conn:a"]);
        drop(reserve(&creds, &limits, &[], now));
        let outcome = note_quota_429("conn:a", "flint-40b-free", now, 0, &limits);
        assert_eq!(outcome.scope, BucketScope::Account, "single direct egress stays account-local");
        assert!(!outcome.egress_learned);
        assert!(outcome.cooldown_until_ms >= window_reset_at(now));
        let snapshot = snapshot(now, &limits);
        assert_eq!(snapshot.egresses.len(), 1, "{:?}", snapshot.egresses);
        assert_eq!(snapshot.egresses[0].label, "direct");
        assert_eq!(snapshot.egresses[0].mode, "direct");
    }
}
