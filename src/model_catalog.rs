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

/// Upstream model-catalog endpoint (also the default `OMNIROUTE_MODEL_CATALOG_URL`).
pub const CATALOG_URL: &str = "https://models.dev/api.json";

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
    pub modalities: Vec<String>,
    pub reasoning: bool,
    /// models.dev cost verdict: `Some(true)` when `cost.input` and
    /// `cost.output` are both zero, `Some(false)` when the model charges,
    /// `None` when the metadata does not speak for this id (built-in snapshot
    /// rows, or an id models.dev does not list). Drives the OpenCode free lane.
    pub free: Option<bool>,
    /// models.dev `deprecated` marker (also set for a retired/disabled status
    /// or a published retirement date).
    pub deprecated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub provider: String,
    pub model: String,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub modalities: Vec<String>,
    pub reasoning: bool,
    /// Defaulted so caches written before the cost verdict existed still load.
    #[serde(default)]
    pub free: Option<bool>,
    #[serde(default)]
    pub deprecated: bool,
}

/// Model-keyed capability index.
#[derive(Debug, Default)]
pub struct Index {
    by_model: std::collections::BTreeMap<String, Vec<Row>>,
}

impl Index {
    fn from_rows(rows: Vec<Row>) -> Self {
        let mut by_model: std::collections::BTreeMap<String, Vec<Row>> = std::collections::BTreeMap::new();
        for row in rows {
            by_model.entry(row.model.clone()).or_default().push(row);
        }
        Self { by_model }
    }

    /// Number of distinct model ids in the index.
    pub fn model_count(&self) -> usize {
        self.by_model.len()
    }

    /// Model-first lookup: an exact `provider|model` row wins, otherwise the same
    /// model id on any provider resolves (relays and gateways re-export ids).
    /// Disagreeing rows take the most common value, ties resolved conservatively
    /// (smaller window / fewer modalities) so a relay is never over-advertised.
    pub fn lookup(&self, provider: &str, model: &str) -> Option<CatalogEntry> {
        let model = model.rsplit('/').next().unwrap_or(model);
        let rows = self.by_model.get(model)?;

        if let Some(row) = rows.iter().find(|row| row.provider == provider) {
            return Some(row.clone().into());
        }
        if rows.len() == 1 {
            return Some(rows[0].clone().into());
        }

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
        let mut modality_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for row in rows {
            *modality_counts.entry(row.modalities.join("+")).or_default() += 1;
        }
        let modalities = modality_counts
            .into_iter()
            .max_by_key(|(key, n)| (*n, std::cmp::Reverse(key.matches('+').count())))
            .map(|(key, _)| key)
            .unwrap_or_default();
        let reasoning = rows.iter().filter(|row| row.reasoning).count() * 2 >= rows.len();

        Some(CatalogEntry {
            context_window: pick(rows.iter().map(|row| row.context_window).collect()),
            max_output_tokens: pick(rows.iter().map(|row| row.max_output_tokens).collect()),
            modalities: modalities
                .split('+')
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect(),
            reasoning,
            // Only a unanimous cost verdict survives the model-first fallback:
            // a model free on one relay and charged on another is unknown, not
            // free (the exact `provider|model` row above is the precise path).
            free: {
                let spoken: Vec<bool> = rows.iter().filter_map(|row| row.free).collect();
                if spoken.is_empty() {
                    None
                } else if spoken.iter().all(|value| *value) {
                    Some(true)
                } else {
                    Some(false)
                }
            },
            deprecated: rows.iter().all(|row| row.deprecated),
        })
    }

    /// Parse a `models.dev/api.json` payload, keeping models that publish a
    /// usable input limit. Returns the index and the number of rows parsed.
    pub fn from_models_dev(payload: &str) -> Result<(Self, usize), String> {
        let root: serde_json::Value = serde_json::from_str(payload).map_err(|e| e.to_string())?;
        let providers = root.as_object().ok_or_else(|| "payload root is not an object".to_string())?;
        let mut rows = Vec::new();
        for (provider, entry) in providers {
            let Some(models) = entry.get("models").and_then(|models| models.as_object()) else {
                continue;
            };
            for (id, model) in models {
                let limit = model.get("limit");
                let context = limit
                    .and_then(|limit| limit.get("context"))
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                let input = limit
                    .and_then(|limit| limit.get("input"))
                    .and_then(|value| value.as_i64())
                    .unwrap_or(context);
                let output = limit
                    .and_then(|limit| limit.get("output"))
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                // The effective input window is the stricter of the two.
                let context_window = match (context > 0, input > 0) {
                    (true, true) => context.min(input),
                    (true, false) => context,
                    (false, true) => input,
                    (false, false) => 0,
                };
                if context_window <= 0 {
                    continue;
                }
                let modalities: Vec<String> = model
                    .pointer("/modalities/input")
                    .and_then(|modalities| modalities.as_array())
                    .map(|modalities| {
                        modalities
                            .iter()
                            .filter_map(|modality| modality.as_str())
                            .filter(|modality| matches!(*modality, "text" | "image" | "pdf"))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["text".to_string()]);
                rows.push(Row {
                    provider: provider.clone(),
                    model: id.clone(),
                    context_window,
                    max_output_tokens: output.max(0),
                    modalities,
                    reasoning: model.get("reasoning").and_then(|value| value.as_bool()).unwrap_or(false),
                    free: cost_free(model),
                    deprecated: metadata_deprecated(model),
                });
            }
        }
        let parsed = rows.len();
        Ok((Self::from_rows(rows), parsed))
    }
}

/// models.dev cost verdict (`model_metadata.go Decide`, ported): a model is
/// free when both `cost.input` and `cost.output` are exactly zero. `None` when
/// either field is missing, so an unknown price is never mistaken for free.
fn cost_free(model: &serde_json::Value) -> Option<bool> {
    let cost = model.get("cost")?;
    let input = cost.get("input").and_then(|value| value.as_f64())?;
    let output = cost.get("output").and_then(|value| value.as_f64())?;
    Some(input == 0.0 && output == 0.0)
}

/// models.dev deprecation markers: the boolean flag, a retired/disabled
/// lifecycle status, or a published retirement date.
fn metadata_deprecated(model: &serde_json::Value) -> bool {
    if model.get("deprecated").and_then(|value| value.as_bool()) == Some(true) {
        return true;
    }
    let status = model
        .get("status")
        .or_else(|| model.get("lifecycle"))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(status.as_str(), "deprecated" | "retired" | "disabled") {
        return true;
    }
    model.get("deprecated_at").is_some_and(|value| !value.is_null())
        || model.get("retirement_date").is_some_and(|value| !value.is_null())
}

impl From<Row> for CatalogEntry {
    fn from(row: Row) -> Self {
        Self {
            context_window: row.context_window,
            max_output_tokens: row.max_output_tokens,
            modalities: row.modalities,
            reasoning: row.reasoning,
            free: row.free,
            deprecated: row.deprecated,
        }
    }
}

/// The built-in snapshot, used until the first network refresh succeeds so the
/// gateway is never without capability data (offline / air-gapped deploys).
pub fn builtin_index() -> Index {
    Index::from_rows(
        CATALOG
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('|');
                Some(Row {
                    provider: parts.next()?.to_string(),
                    model: parts.next()?.to_string(),
                    context_window: parts.next()?.parse().ok()?,
                    max_output_tokens: parts.next()?.parse().ok()?,
                    modalities: parts
                        .next()?
                        .split('+')
                        .filter(|modality| !modality.is_empty())
                        .map(str::to_string)
                        .collect(),
                    reasoning: parts.next()? == "R",
                    // The built-in snapshot predates the cost verdict: the free
                    // decision falls back to the name/verified rules until the
                    // first models.dev refresh lands.
                    free: None,
                    deprecated: false,
                })
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    fetched_at_ms: u128,
    rows: Vec<Row>,
}

/// Disk cache location inside the data dir.
pub fn cache_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("model-catalog.json")
}

/// Load the on-disk cache. Returns the index and its fetch timestamp.
pub fn load_disk(path: &std::path::Path) -> Option<(Index, u128)> {
    let text = std::fs::read_to_string(path).ok()?;
    let cache: CacheFile = serde_json::from_str(&text).ok()?;
    Some((Index::from_rows(cache.rows), cache.fetched_at_ms))
}

/// Persist the index atomically (temp file + rename).
pub fn save_disk(path: &std::path::Path, index: &Index) -> std::io::Result<()> {
    let rows: Vec<Row> = index
        .by_model
        .values()
        .flat_map(|rows| rows.iter().cloned())
        .collect();
    let cache = CacheFile {
        fetched_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0),
        rows,
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&tmp, path)
}

fn is_stale(fetched_at_ms: u128, ttl_hours: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    now.saturating_sub(fetched_at_ms) > u128::from(ttl_hours) * 3_600_000
}

/// Install the process-wide index used by `lookup`.
static INDEX: std::sync::OnceLock<std::sync::RwLock<std::sync::Arc<Index>>> = std::sync::OnceLock::new();

fn shared() -> &'static std::sync::RwLock<std::sync::Arc<Index>> {
    INDEX.get_or_init(|| std::sync::RwLock::new(std::sync::Arc::new(builtin_index())))
}

pub fn install(index: Index) {
    *shared().write().unwrap_or_else(|poison| poison.into_inner()) = std::sync::Arc::new(index);
}

/// Model ids currently resolvable (built-in snapshot until a refresh lands).
pub fn model_count() -> usize {
    shared().read().unwrap_or_else(|poison| poison.into_inner()).model_count()
}

/// Load the disk cache when present, else the built-in snapshot.
/// Returns `(model_count, from_disk)`.
pub fn bootstrap(data_dir: &std::path::Path) -> (usize, bool) {
    if let Some((index, _)) = load_disk(&cache_path(data_dir)) {
        install(index);
        return (model_count(), true);
    }
    install(builtin_index());
    (model_count(), false)
}

/// Fetch the upstream catalog, persist it, and install it.
pub async fn refresh(
    client: &reqwest::Client,
    url: &str,
    data_dir: &std::path::Path,
) -> anyhow::Result<(usize, u128)> {
    let response = client.get(url).timeout(std::time::Duration::from_secs(30)).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("model catalog answered HTTP {}", response.status());
    }
    let payload = response.text().await?;
    let (index, rows) = Index::from_models_dev(&payload).map_err(anyhow::Error::msg)?;
    save_disk(&cache_path(data_dir), &index)?;
    let fetched_at = load_disk(&cache_path(data_dir)).map(|(_, at)| at).unwrap_or(0);
    install(index);
    Ok((rows, fetched_at))
}

/// Background refresher: refreshes when the cached snapshot is stale, then
/// re-checks hourly. Failures are logged and retried on the next tick; the
/// previous index stays in place.
pub fn spawn_background(data_dir: std::path::PathBuf, ttl_hours: u64, url: String) {
    let client = reqwest::Client::new();
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let stale = load_disk(&cache_path(&data_dir))
                .map(|(_, fetched_at)| is_stale(fetched_at, ttl_hours))
                .unwrap_or(true);
            if first || stale {
                match refresh(&client, &url, &data_dir).await {
                    Ok((rows, _)) => {
                        tracing::info!("model catalog refreshed: {rows} rows from {url}");
                        first = false;
                    }
                    Err(error) => {
                        tracing::warn!("model catalog refresh failed ({url}): {error}");
                        // Retry sooner after a failure, but never hot-loop.
                        if !first {
                            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                        }
                        first = false;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

/// Look up one provider/model pair in the active index.
pub fn lookup(provider: &str, model: &str) -> Option<CatalogEntry> {
    shared().read().unwrap_or_else(|poison| poison.into_inner()).lookup(provider, model)
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
        assert!(step.modalities.contains(&"image".to_string()));
    }

    #[test]
    fn same_model_resolves_on_any_provider() {
        // A relay/gateway re-exporting the model id must not fall back to the
        // unknown-model defaults.
        let relay = lookup("openai-compatible-my-relay", "space-bunny").expect("global model row");
        assert_eq!(relay.context_window, 524_288);
        assert!(relay.modalities.contains(&"image".to_string()));
        let gw = lookup("openrouter", "step-5-preview-free").expect("global model row");
        assert_eq!(gw.context_window, 1_000_000);
        assert_eq!(gw.max_output_tokens, 65_536);
    }

    #[test]
    fn unknown_models_fall_through() {
        assert!(lookup("opencode-go", "not-a-real-model").is_none());
        assert!(lookup("openai", "not-a-real-model").is_none());
    }

    #[test]
    fn models_dev_payload_becomes_rows() {
        let payload = r#"{
          "openai": {"models": {"gpt-4o": {"limit": {"context": 128000, "input": 128000, "output": 16384},
                        "modalities": {"input": ["text", "image"]}, "reasoning": false}}},
          "opencode-go": {"models": {"space-bunny": {"limit": {"context": 1048576, "input": 524288, "output": 524288},
                        "modalities": {"input": ["text", "image", "video"]}, "reasoning": true}}}
        }"#;
        let (index, rows) = Index::from_models_dev(payload).expect("payload parses");
        assert_eq!(rows, 2);
        assert_eq!(index.model_count(), 2);

        // min(context, input) is the effective window; video is not a supported
        // chat modality so it is dropped.
        let bunny = index.lookup("opencode-go", "space-bunny").expect("row");
        assert_eq!(bunny.context_window, 524_288);
        assert_eq!(bunny.modalities, vec!["text", "image"]);
        assert!(bunny.reasoning);

        // Cross-provider resolution reads the same model id.
        assert_eq!(index.model_count(), 2);
    }

    #[test]
    fn disk_cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path());
        let (index, _) = Index::from_models_dev(
            r#"{"openai":{"models":{"gpt-4o":{"limit":{"context":128000,"output":16384}}}}}"#,
        )
        .unwrap();
        save_disk(&path, &index).unwrap();
        let (loaded, fetched_at) = load_disk(&path).expect("cache loads");
        assert!(fetched_at > 0);
        let entry = loaded.lookup("openai", "gpt-4o").expect("row");
        assert_eq!(entry.context_window, 128_000);
        assert_eq!(entry.max_output_tokens, 16_384);
    }

    #[test]
    fn stale_detection_uses_ttl() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        // A snapshot fetched now is fresh for any positive ttl.
        assert!(!is_stale(now, 24));
        assert!(!is_stale(now, 24 * 365));
        // Older than the ttl is stale.
        assert!(is_stale(now.saturating_sub(25 * 3_600_000), 24));
        assert!(is_stale(0, 24));
    }
}
