//! Embedded web dashboard + PWA shell (parity-lite of the original's
//! Next.js dashboard at `src/app/(dashboard)`; served directly by the
//! gateway so no Node runtime is needed).

use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("dashboard_assets/index.html");
const APP_CSS: &str = include_str!("dashboard_assets/app.css");
const APP_JS: &str = include_str!("dashboard_assets/app.js");
const MANIFEST: &str = include_str!("dashboard_assets/manifest.webmanifest");
const SW_JS: &str = include_str!("dashboard_assets/sw.js");
const ICON_SVG: &str = include_str!("dashboard_assets/icon.svg");
/// Self-hosted Material Symbols Outlined (parity: `@import "material-symbols/outlined.css"`)
const MATERIAL_SYMBOLS_CSS: &str = include_str!("dashboard_assets/fonts/outlined.css");
const MATERIAL_SYMBOLS_WOFF2: &[u8] = include_bytes!("dashboard_assets/fonts/material-symbols-outlined.woff2");
static LOCALE_PACKS: [(&str, &str); 66] = [
    ("am", include_str!("dashboard_assets/locales/am.json")),     ("ar", include_str!("dashboard_assets/locales/ar.json")),     ("az", include_str!("dashboard_assets/locales/az.json")),     ("bg", include_str!("dashboard_assets/locales/bg.json")),     ("bn", include_str!("dashboard_assets/locales/bn.json")),     ("cs", include_str!("dashboard_assets/locales/cs.json")),     ("da", include_str!("dashboard_assets/locales/da.json")),     ("de", include_str!("dashboard_assets/locales/de.json")),     ("el", include_str!("dashboard_assets/locales/el.json")),     ("en", include_str!("dashboard_assets/locales/en.json")),     ("es", include_str!("dashboard_assets/locales/es.json")),     ("et", include_str!("dashboard_assets/locales/et.json")),     ("fa", include_str!("dashboard_assets/locales/fa.json")),     ("fi", include_str!("dashboard_assets/locales/fi.json")),     ("fr", include_str!("dashboard_assets/locales/fr.json")),     ("ga", include_str!("dashboard_assets/locales/ga.json")),     ("gu", include_str!("dashboard_assets/locales/gu.json")),     ("ha", include_str!("dashboard_assets/locales/ha.json")),     ("he", include_str!("dashboard_assets/locales/he.json")),     ("hi", include_str!("dashboard_assets/locales/hi.json")),     ("hr", include_str!("dashboard_assets/locales/hr.json")),     ("hu", include_str!("dashboard_assets/locales/hu.json")),     ("hy", include_str!("dashboard_assets/locales/hy.json")),     ("id", include_str!("dashboard_assets/locales/id.json")),     ("ig", include_str!("dashboard_assets/locales/ig.json")),     ("it", include_str!("dashboard_assets/locales/it.json")),     ("ja", include_str!("dashboard_assets/locales/ja.json")),     ("ka", include_str!("dashboard_assets/locales/ka.json")),     ("km", include_str!("dashboard_assets/locales/km.json")),     ("kn", include_str!("dashboard_assets/locales/kn.json")),     ("ko", include_str!("dashboard_assets/locales/ko.json")),     ("lt", include_str!("dashboard_assets/locales/lt.json")),     ("lv", include_str!("dashboard_assets/locales/lv.json")),     ("ml", include_str!("dashboard_assets/locales/ml.json")),     ("mr", include_str!("dashboard_assets/locales/mr.json")),     ("ms", include_str!("dashboard_assets/locales/ms.json")),     ("mt", include_str!("dashboard_assets/locales/mt.json")),     ("my", include_str!("dashboard_assets/locales/my.json")),     ("ne", include_str!("dashboard_assets/locales/ne.json")),     ("nl", include_str!("dashboard_assets/locales/nl.json")),     ("no", include_str!("dashboard_assets/locales/no.json")),     ("or", include_str!("dashboard_assets/locales/or.json")),     ("pa", include_str!("dashboard_assets/locales/pa.json")),     ("phi", include_str!("dashboard_assets/locales/phi.json")),     ("pl", include_str!("dashboard_assets/locales/pl.json")),     ("pt-BR", include_str!("dashboard_assets/locales/pt-BR.json")),     ("pt", include_str!("dashboard_assets/locales/pt.json")),     ("ro", include_str!("dashboard_assets/locales/ro.json")),     ("ru", include_str!("dashboard_assets/locales/ru.json")),     ("si", include_str!("dashboard_assets/locales/si.json")),     ("sk", include_str!("dashboard_assets/locales/sk.json")),     ("sl", include_str!("dashboard_assets/locales/sl.json")),     ("sr", include_str!("dashboard_assets/locales/sr.json")),     ("sv", include_str!("dashboard_assets/locales/sv.json")),     ("sw", include_str!("dashboard_assets/locales/sw.json")),     ("ta", include_str!("dashboard_assets/locales/ta.json")),     ("te", include_str!("dashboard_assets/locales/te.json")),     ("th", include_str!("dashboard_assets/locales/th.json")),     ("tr", include_str!("dashboard_assets/locales/tr.json")),     ("uk-UA", include_str!("dashboard_assets/locales/uk-UA.json")),     ("ur", include_str!("dashboard_assets/locales/ur.json")),     ("uz", include_str!("dashboard_assets/locales/uz.json")),     ("vi", include_str!("dashboard_assets/locales/vi.json")),     ("yo", include_str!("dashboard_assets/locales/yo.json")),     ("zh-CN", include_str!("dashboard_assets/locales/zh-CN.json")),     ("zh-TW", include_str!("dashboard_assets/locales/zh-TW.json")), ];

const LANGUAGES_JSON: &str = include_str!("dashboard_assets/languages.json");
/// Upstream provider catalog (id/name/alias/icon/color/category/auth flags),
/// extracted from `src/shared/constants/providers/**` of the original.
const PROVIDER_CATALOG: &str = include_str!("dashboard_assets/providers.json");

/// `GET /dashboard` (+ `/` redirect target).
pub async fn index() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        INDEX_HTML,
    )
}

/// Serve dashboard static assets (`/dashboard/{*path}`).
pub async fn asset(Path(path): Path<String>) -> axum::response::Response {
    if let Some((_, pack)) = LOCALE_PACKS.iter().find(|(c, _)| format!("locale:{c}.json") == path || path == format!("locales/{c}.json")) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-cache")],
            *pack,
        )
            .into_response();
    }
    if path == "fonts/material-symbols-outlined.woff2" {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "font/woff2"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            MATERIAL_SYMBOLS_WOFF2,
        )
            .into_response();
    }
    let (ctype, body): (&str, &str) = match path.as_str() {
        "app.css" => ("text/css; charset=utf-8", APP_CSS),
        "app.js" => ("application/javascript; charset=utf-8", APP_JS),
        "fonts/outlined.css" => ("text/css; charset=utf-8", MATERIAL_SYMBOLS_CSS),
        "manifest.webmanifest" => ("application/manifest+json", MANIFEST),
        "sw.js" => ("application/javascript; charset=utf-8", SW_JS),
        "icon.svg" => ("image/svg+xml", ICON_SVG),
        "languages.json" => ("application/json", LANGUAGES_JSON),
        "providers.json" => ("application/json", PROVIDER_CATALOG),
        "locales/zh.json" => ("application/json", LOCALE_PACKS.iter().find(|(c, _)| *c == "zh").map(|(_, p)| *p).unwrap_or("{}")),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"message":"not found","type":"not_found","code":"unknown_route"}}"#,
            )
                .into_response()
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

/// `GET /` → dashboard redirect.
pub async fn root_redirect() -> impl IntoResponse {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, "/dashboard")],
        "",
    )
}

/// `GET /v1/stats` — gateway runtime stats (uptime, request counters, RSS).
pub async fn stats(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "pid": std::process::id(),
            "uptime_s": state.started_at.elapsed().as_secs(),
            "requests": state.request_log_total.load(std::sync::atomic::Ordering::Relaxed),
            "failures": state.request_log_failures.load(std::sync::atomic::Ordering::Relaxed),
            "providers_with_keys": state.providers_with_keys().len(),
            "models": state
                .providers_with_keys()
                .iter()
                .map(|id| state.models_for_provider(id).len())
                .sum::<usize>(),
            "memory_kb": read_self_rss_kb(),
        })),
    )
        .into_response()
}

/// `GET /v1/logs` — recent request log ring buffer.
pub async fn logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let q = Query::from_uri(&uri);
    let limit: usize = q.parse("limit").unwrap_or(100).min(1000);
    let entries = filter_logs(&state.request_log_snapshot(1000), &q, limit);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "logs": entries })),
    )
        .into_response()
}

/// Parsed `?a=b` query helper shared by the dashboard endpoints.
pub struct Query(std::collections::HashMap<String, String>);

impl Query {
    pub fn from_uri(uri: &axum::http::Uri) -> Self {
        let mut m = std::collections::HashMap::new();
        if let Some(q) = uri.query() {
            for kv in q.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    m.insert(
                        k.to_string(),
                        percent_decode(v),
                    );
                }
            }
        }
        Self(m)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }
    pub fn parse<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.get(k).and_then(|v| v.parse().ok())
    }
    pub fn has(&self, k: &str) -> bool {
        self.0.get(k).map(|v| v != "false" && v != "0").unwrap_or(false)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Apply the request-log filters (provider / model / status class / errors only).
fn filter_logs(
    entries: &[crate::state::RequestLogEntry],
    q: &Query,
    limit: usize,
) -> Vec<crate::state::RequestLogEntry> {
    let provider = q.get("provider").filter(|p| !p.is_empty());
    let model = q.get("model").filter(|m| !m.is_empty()).map(str::to_lowercase);
    let status = q.get("status").and_then(|s| s.parse::<u16>().ok());
    let status_class = q.get("class").and_then(|c| c.parse::<u16>().ok()); // 2/4/5
    let errors_only = q.has("errors");
    let stream = q.get("stream").map(|s| s == "true" || s == "1");
    entries
        .iter()
        .filter(|e| provider.is_none_or(|p| e.provider.as_deref() == Some(p)))
        .filter(|e| model.as_ref().is_none_or(|m| e.model.to_lowercase().contains(m)))
        .filter(|e| status.is_none_or(|s| e.status == s))
        .filter(|e| status_class.is_none_or(|c| e.status / 100 == c))
        .filter(|e| !errors_only || e.status >= 400)
        .filter(|e| stream.is_none_or(|s| e.stream == s))
        .take(limit)
        .cloned()
        .collect()
}

/// `GET /v1/logs/export?format=csv|json[&provider=&model=&errors=]`
pub async fn logs_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let q = Query::from_uri(&uri);
    let limit = q.parse("limit").unwrap_or(1000).min(10_000);
    let rows = filter_logs(&state.request_log_snapshot(1000), &q, limit);
    let format = q.get("format").unwrap_or("json");
    if format == "csv" {
        let mut out = String::from("ts_ms,model,provider,status,latency_ms,prompt_tokens,completion_tokens,tokens_saved,compressed,stream\n");
        for r in &rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                r.ts_ms,
                csv_cell(&r.model),
                csv_cell(r.provider.as_deref().unwrap_or("")),
                r.status,
                r.latency_ms,
                r.prompt_tokens,
                r.completion_tokens,
                r.tokens_saved,
                r.compressed,
                r.stream
            ));
        }
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"omniroute-requests.csv\"",
                ),
            ],
            out,
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into()),
    )
        .into_response()
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// `GET /v1/stats/providers` — per-provider aggregates over the request log plus
/// live circuit state (parity: provider-stats analytics page).
pub async fn stats_providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let entries = state.request_log_snapshot(1000);
    // provider -> (requests, errors, latency sum, prompt tokens, completion tokens)
    let mut agg: std::collections::BTreeMap<String, (u64, u64, u64, u64, i64)> = std::collections::BTreeMap::new();
    for e in &entries {
        let key = e.provider.clone().unwrap_or_else(|| "(unrouted)".into());
        let a = agg.entry(key).or_insert((0, 0, 0, 0, 0));
        a.0 += 1;
        if e.status >= 400 {
            a.1 += 1;
        }
        a.2 += e.latency_ms;
        a.3 += e.prompt_tokens;
        a.4 += e.completion_tokens as i64;
    }
    let live = state.provider_runtime_snapshot();
    let mut providers: Vec<serde_json::Value> = Vec::new();
    for (name, (requests, errors, lat_sum, tin, tout)) in agg {
        let rt = live.iter().find(|p| p.id == name);
        providers.push(serde_json::json!({
            "provider": name,
            "requests": requests,
            "errors": errors,
            "success_rate": if requests > 0 { ((requests - errors) as f64 / requests as f64 * 100.0).round() } else { 0.0 },
            "avg_latency_ms": lat_sum.checked_div(requests).unwrap_or(0),
            "prompt_tokens": tin,
            "completion_tokens": tout,
            "cooldown_ms": rt.map(|p| p.cooldown_ms).unwrap_or(0),
            "in_flight": rt.map(|p| p.in_flight).unwrap_or(0),
            "has_key": rt.map(|p| p.has_key).unwrap_or(false),
            "format": rt.map(|p| p.format.clone()).unwrap_or_else(|| "unknown".into()),
        }));
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "providers": providers, "sampled": entries.len() })),
    )
        .into_response()
}

/// `GET /v1/combo-health` — per-combo success/latency from the request log.
pub async fn combo_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let combos = state.config.combos.clone();
    let entries = state.request_log_snapshot(1000);
    let out: Vec<serde_json::Value> = combos
        .iter()
        .map(|c| {
            let hits: Vec<_> = entries
                .iter()
                .filter(|e| {
                    e.model == c.name
                        || c.models.iter().any(|m| m == &e.model)
                        || c.providers
                            .iter()
                            .any(|p| e.model == *p || e.model.starts_with(&format!("{p}/")))
                })
                .collect();
            let errors = hits.iter().filter(|e| e.status >= 400).count();
            let lat: u64 = hits.iter().map(|e| e.latency_ms).sum();
            serde_json::json!({
                "combo": c.name,
                "strategy": c.strategy.clone().unwrap_or_else(|| "priority".into()),
                "members": c.providers,
                "models": c.models,
                "requests": hits.len(),
                "errors": errors,
                "success_rate": if hits.is_empty() { 0.0 } else { (((hits.len() - errors) as f64 / hits.len() as f64) * 100.0).round() },
                "avg_latency_ms": if hits.is_empty() { 0 } else { lat / hits.len() as u64 },
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "combos": out, "sampled": entries.len() })),
    )
        .into_response()
}

/// `GET /v1/audit` — management-action audit ring (parity: audit-log page).
pub async fn audit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let q = Query::from_uri(&uri);
    let limit = q.parse("limit").unwrap_or(200).min(1000);
    let action = q.get("action").filter(|a| !a.is_empty());
    let entries: Vec<_> = state
        .audit_snapshot(limit * 4)
        .into_iter()
        .filter(|e| action.is_none_or(|a| e.action == a))
        .take(limit)
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "audit": entries })),
    )
        .into_response()
}

/// Read VmRSS from /proc/self/status (Linux); 0 elsewhere.
pub fn read_self_rss_kb() -> i64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
            })
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// `GET /v1/provider-catalog` — upstream provider catalog joined with live
/// connection state (drives the Providers page chips/sections/cards).
///
/// Parity: the original's Providers page builds per-card stats via
/// `getProviderStats` (connected/error/warning/total + errorCode/errorTime +
/// allDisabled + expiry + codex tier) over `/api/providers` connections, plus
/// compatible provider nodes, expirations, blocked no-auth ids and OpenRouter
/// popularity stats. The Rust build has no OAuth/web-cookie executors, expiry
/// tracking or OpenRouter feed, so those fields are returned honestly
/// (warning 0, expiry null, stats []) — never faked — while the shape matches
/// the original so the page renders identically.
pub async fn provider_catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let catalog: serde_json::Value = serde_json::from_str(PROVIDER_CATALOG).unwrap_or(serde_json::json!([]));
    let connections = state.provider_connections.all_unmasked();
    let runtime = state.provider_runtime_snapshot();
    let listed: Vec<serde_json::Value> = catalog
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let matching: Vec<_> =
                        connections.iter().filter(|c| c.provider == id).collect();
                    let conn = matching.first();
                    let live = runtime.iter().find(|r| r.id == id || r.id.ends_with(&format!("-{id}")));
                    let has_key = live.map(|l| l.has_key).unwrap_or(false)
                        || matching.iter().any(|c| c.api_key.is_some());
                    let total = matching.len();
                    let connected = matching.iter().filter(|c| c.enabled).count();
                    let all_disabled = total > 0 && connected == 0;
                    let cooldown_ms = live.map(|l| l.cooldown_ms).unwrap_or(0);
                    // models drivers the "search by model" filter (parity:
                    // static registry + live connection manual/synced lists,
                    // minus per-connection hidden models).
                    let mut models = state.registry.models_for(id);
                    let mut hidden: std::collections::HashSet<&str> =
                        std::collections::HashSet::new();
                    for c in &matching {
                        models.extend(c.model_list.clone());
                        models.extend(c.synced_models.clone());
                        hidden.extend(c.hidden_models.iter().map(String::as_str));
                    }
                    models.retain(|m| !hidden.contains(m.as_str()));
                    models.sort();
                    models.dedup();
                    let mut v = p.clone();
                    v["connected"] = serde_json::json!(connected > 0);
                    v["hasKey"] = serde_json::json!(has_key);
                    v["cooldownMs"] = serde_json::json!(cooldown_ms);
                    v["inFlight"] = serde_json::json!(live.map(|l| l.in_flight).unwrap_or(0));
                    v["connectionId"] = serde_json::json!(conn.map(|c| c.id.clone()));
                    v["connectionIds"] = serde_json::json!(matching.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
                    v["enabled"] = serde_json::json!(conn.map(|c| c.enabled).unwrap_or(false));
                    v["models"] = serde_json::json!(models);
                    // Original `getProviderStats` shape for ProviderCard.
                    v["stats"] = serde_json::json!({
                        "total": total,
                        "connected": connected,
                        "error": if cooldown_ms > 0 && connected == 0 { total } else { 0 },
                        "warning": 0,
                        "warningMaxFailures": 0,
                        "warningLastFailureRelative": null,
                        "errorCode": null,
                        "errorTime": null,
                        "allDisabled": all_disabled,
                        "expiryStatus": null,
                        "codexServiceTier": null,
                    });
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    // Dynamic compatible nodes (parity: providerNodes of type
    // openai-compatible / anthropic-compatible / cc). These are user-created
    // connections whose provider id carries the family prefix.
    let compatible_nodes: Vec<serde_json::Value> = connections
        .iter()
        .filter(|c| {
            c.provider.starts_with("openai-compatible")
                || c.provider.starts_with("anthropic-compatible")
        })
        .map(|c| {
            let is_cc = c.provider.starts_with("anthropic-compatible-cc-");
            let kind = if c.provider.starts_with("openai-compatible") {
                "openai"
            } else if is_cc {
                "claudeCode"
            } else {
                "anthropic"
            };
            serde_json::json!({
                "id": c.provider,
                "connectionId": c.id,
                "name": if c.name.is_empty() { c.provider.clone() } else { c.name.clone() },
                "kind": kind,
                "apiType": c.api_type,
                "enabled": c.enabled,
                "hasKey": c.api_key.is_some(),
                "models": c.model_list,
                "stats": {
                    "total": 1,
                    "connected": if c.enabled { 1 } else { 0 },
                    "error": 0, "warning": 0,
                    "allDisabled": !c.enabled,
                    "expiryStatus": null,
                },
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "providers": listed, "total": listed.len(),
            "compatibleNodes": compatible_nodes,
            // Honest stubs: the Rust build tracks no expirations, no blocked
            // no-auth list and no OpenRouter popularity feed.
            "expirations": {"summary": {"expired": 0, "expiringSoon": 0}, "list": []},
            "blockedProviders": [],
            "openRouterStats": [],
        })),
    )
        .into_response()
}

/// Category lookup for the batch-test mode filter (built from the catalog).
fn provider_category_of(catalog: &serde_json::Value, id: &str) -> Option<String> {
    catalog.as_array()?.iter().find_map(|p| {
        if p.get("id").and_then(|v| v.as_str()) == Some(id) {
            p.get("category").and_then(|v| v.as_str()).map(str::to_string)
        } else {
            None
        }
    })
}

fn is_compatible_provider_id(id: &str) -> bool {
    id.starts_with("openai-compatible") || id.starts_with("anthropic-compatible")
}

/// `POST /v1/providers/test-batch` — test many connections by group.
///
/// Parity: the original's `/api/providers/test-batch` (`mode` = all |
/// provider | oauth | free | no-auth | apikey | compatible | web-cookie |
/// search | audio | local | upstream-proxy | cloud-agent | ide | selected).
/// Probes run sequentially with the shared 1-token ping; the response shape
/// (`mode` / `results[]` / `summary{total,passed,failed}` / `testedAt`)
/// matches the original so the dashboard's TestResults modal renders as-is.
pub async fn providers_test_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    let mode = body.get("mode").and_then(|m| m.as_str()).unwrap_or("all");
    let provider_id = body.get("providerId").and_then(|p| p.as_str()).unwrap_or_default();
    let wanted_ids: std::collections::HashSet<String> = body
        .get("connectionIds")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let catalog: serde_json::Value =
        serde_json::from_str(PROVIDER_CATALOG).unwrap_or(serde_json::json!([]));
    let all = state.provider_connections.all_unmasked();
    // mode=selected reaches explicit ids (even disabled); every other mode
    // tests enabled connections only (original semantics).
    let mut targets: Vec<_> = if mode == "selected" {
        all.iter().filter(|c| wanted_ids.contains(&c.id)).cloned().collect()
    } else {
        all.iter().filter(|c| c.enabled).cloned().collect()
    };
    let norm = mode.replace('_', "-");
    if mode == "provider" && !provider_id.is_empty() {
        targets.retain(|c| c.provider == provider_id);
    } else if norm == "compatible" {
        targets.retain(|c| is_compatible_provider_id(&c.provider));
    } else if norm == "free" {
        targets.retain(|c| {
            catalog
                .as_array()
                .map(|a| {
                    a.iter().any(|p| {
                        p.get("id").and_then(|v| v.as_str()) == Some(c.provider.as_str())
                            && p.get("freeTier").and_then(|v| v.as_bool()).unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
    } else if norm == "ide" {
        targets.retain(|c| {
            catalog
                .as_array()
                .map(|a| {
                    a.iter().any(|p| {
                        p.get("id").and_then(|v| v.as_str()) == Some(c.provider.as_str())
                            && p.get("ide").and_then(|v| v.as_bool()).unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
    } else if !["all", "provider", "selected"].contains(&norm.as_str()) {
        let want = norm.clone();
        targets.retain(|c| {
            if is_compatible_provider_id(&c.provider) {
                return false;
            }
            match provider_category_of(&catalog, &c.provider).as_deref() {
                Some("noauth") => want == "no-auth" || want == "noauth",
                Some("web-cookie") => want == "web-cookie" || want == "webcookie",
                Some("upstream-proxy") => want == "upstream-proxy" || want == "upstreamproxy",
                Some("cloud-agent") => want == "cloud-agent" || want == "cloudagent",
                Some(cat) => cat == want,
                None => false,
            }
        });
    }
    if targets.is_empty() {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "mode": mode, "providerId": if provider_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(provider_id.to_string()) },
                "results": [], "testedAt": chrono_now(),
                "summary": {"total": 0, "passed": 0, "failed": 0},
            })),
        )
            .into_response();
    }
    let mut results = Vec::new();
    for conn in targets.iter().take(200) {
        let (ok, latency, detail) = crate::server::admin::probe_connection(&state, conn).await;
        let dtype = if ok {
            serde_json::Value::Null
        } else if detail.contains("401") || detail.contains("403") || detail.to_lowercase().contains("auth") {
            serde_json::json!({"type": "upstream_auth_error"})
        } else if detail.contains("429") || detail.to_lowercase().contains("rate") {
            serde_json::json!({"type": "upstream_rate_limited"})
        } else if detail.contains("500") || detail.contains("502") || detail.contains("503") {
            serde_json::json!({"type": "upstream_unavailable"})
        } else {
            serde_json::json!({"type": "network_error"})
        };
        results.push(serde_json::json!({
            "provider": conn.provider,
            "connectionId": conn.id,
            "connectionName": if conn.name.is_empty() { conn.provider.clone() } else { conn.name.clone() },
            "valid": ok,
            "latencyMs": latency,
            "error": if ok { serde_json::Value::Null } else { serde_json::Value::String(detail.clone()) },
            "diagnosis": dtype,
            "statusCode": null,
        }));
    }
    let passed = results.iter().filter(|r| r["valid"].as_bool().unwrap_or(false)).count();
    let total = results.len();
    state.audit(
        "provider_connection.test_batch",
        format!("mode={mode} total={total} passed={passed}"),
        true,
    );
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "mode": mode,
            "providerId": if provider_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(provider_id.to_string()) },
            "results": results,
            "testedAt": chrono_now(),
            "summary": {"total": total, "passed": passed, "failed": total - passed},
        })),
    )
        .into_response()
}

fn chrono_now() -> String {
    // ISO-8601 without pulling in chrono: seconds since epoch is enough for
    // the dashboard's "testedAt" display; keep the shape stable.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// `GET /v1/free-tiers` — free-tier catalog for the Free-tiers page.
///
/// Parity: the original's `/dashboard/free-tiers` budget card
/// (`/api/free-tier/summary`). The original sums pool budgets into a headline
/// token figure (~1.47B/mo); that accounting lives in its radar/free-tier
/// service and is deliberately NOT reproduced here — the Rust build reports
/// the catalog (151 free-tier entries with per-provider notes) plus live
/// connection state, and never a summed figure it cannot verify.
pub async fn free_tiers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let catalog: serde_json::Value =
        serde_json::from_str(PROVIDER_CATALOG).unwrap_or(serde_json::json!([]));
    let connections = state.provider_connections.all_unmasked();
    let mut tiers = Vec::new();
    let mut by_category: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for p in catalog.as_array().map(|a| a.iter()).into_iter().flatten() {
        if p.get("freeTier").and_then(|v| v.as_bool()).unwrap_or(false) {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let matching: Vec<_> = connections.iter().filter(|c| c.provider == id).collect();
            let connected = matching.iter().any(|c| c.enabled);
            let mut models = state.registry.models_for(id);
            for c in &matching {
                models.extend(c.model_list.clone());
            }
            models.sort();
            models.dedup();
            *by_category
                .entry(p.get("category").and_then(|v| v.as_str()).unwrap_or("apikey").to_string())
                .or_default() += 1;
            tiers.push(serde_json::json!({
                "provider": id,
                "name": p.get("name"),
                "category": p.get("category"),
                "icon": p.get("icon"),
                "iconText": p.get("iconText"),
                "color": p.get("color"),
                "website": p.get("website"),
                "freeNote": p.get("freeNote"),
                "serviceKinds": p.get("serviceKinds"),
                "connected": connected,
                "enabledConnections": matching.iter().filter(|c| c.enabled).count(),
                "totalConnections": matching.len(),
                "models": models,
            }));
        }
    }
    tiers.sort_by(|a, b| {
        (b["connected"].as_bool().unwrap_or(false))
            .cmp(&a["connected"].as_bool().unwrap_or(false))
            .then_with(|| {
                a["provider"].as_str().unwrap_or_default()
                    .cmp(b["provider"].as_str().unwrap_or_default())
            })
    });
    let connected = tiers.iter().filter(|t| t["connected"].as_bool().unwrap_or(false)).count();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "summary": {
                "freeProviders": tiers.len(),
                "connected": connected,
                "byCategory": by_category,
            },
            "tiers": tiers,
            "headlineTokensPerMonth": null,
            "headlineReason": "the Rust build reports the free-tier catalog, never a summed pool-budget figure",
        })),
    )
        .into_response()
}

/// `POST /v1/provider-connections/test-all` — probe every enabled connection.
pub async fn provider_connections_test_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let mut results = Vec::new();
    for conn in state.provider_connections.all_unmasked() {
        if !conn.enabled {
            continue;
        }
        let (ok, latency, detail) = crate::server::admin::probe_connection(&state, &conn).await;
        results.push(serde_json::json!({
            "id": conn.id, "provider": conn.provider, "ok": ok,
            "latency_ms": latency, "detail": detail,
        }));
    }
    state.audit("provider_connection.test_all", format!("{} probed", results.len()), true);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "results": results })),
    )
        .into_response()
}

/// `POST /v1/provider-connections/import` — bulk upsert from a JSON array
/// (parity: the original's "import from file").
pub async fn provider_connections_import(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid JSON body: {e}")).into(),
    };
    let list = body
        .get("connections")
        .and_then(|c| c.as_array())
        .or_else(|| body.as_array())
        .cloned()
        .unwrap_or_default();
    let mut imported = 0usize;
    let mut errors = Vec::new();
    for (i, item) in list.iter().enumerate() {
        match serde_json::from_value::<crate::server::providers_admin::ProviderConnection>(item.clone()) {
            Ok(mut conn) => {
                if conn.provider.is_empty() {
                    errors.push(serde_json::json!({"index": i, "error": "missing provider"}));
                    continue;
                }
                if conn.name.is_empty() {
                    conn.name = conn.provider.clone();
                }
                crate::server::admin::apply_connection(&state, &mut conn);
                state.provider_connections.upsert(conn);
                imported += 1;
            }
            Err(e) => errors.push(serde_json::json!({"index": i, "error": e.to_string()})),
        }
    }
    state.audit("provider_connection.import", format!("imported={imported}"), true);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "imported": imported, "errors": errors })),
    )
        .into_response()
}

/// `GET /v1/usage/analytics` — the Usage page aggregate. Mirrors the original's
/// `/api/usage/analytics` response shape (summary / dailyTrend / activityMap /
/// byModel / byProvider / byApiKey / byServiceTier / weekly* / errorBreakdown).
pub async fn usage_analytics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    // ring is newest-first; analyze oldest→newest
    let mut entries = state.request_log_snapshot(1000);
    entries.reverse();

    let mut prompt: u64 = 0;
    let mut completion: u64 = 0;
    let mut ok: u64 = 0;
    let mut lat_sum: u64 = 0;
    let mut fallbacks: u64 = 0;
    let mut models: std::collections::BTreeSet<String> = Default::default();
    let mut providers: std::collections::BTreeSet<String> = Default::default();
    let mut daily: std::collections::BTreeMap<String, (u64, u64, u64, u64, f64)> = Default::default();
    let mut activity: std::collections::BTreeMap<String, u64> = Default::default();
    // model -> (provider, requests, prompt, completion, total, last ts, avg latency)
    type ModelAgg = (String, u64, u64, u64, u64, u128, u64);
    let mut by_model: std::collections::BTreeMap<String, ModelAgg> = Default::default();
    let mut by_provider: std::collections::BTreeMap<String, (u64, u64, u64, u64, u64, u128)> = Default::default();
    let mut errors: std::collections::BTreeMap<String, u64> = Default::default();
    let mut weekly_tokens = [0u64; 7];
    let mut weekly_counts = [0u64; 7];
    let mut first_ts: Option<u128> = None;
    let mut last_ts: Option<u128> = None;

    for e in &entries {
        prompt += e.prompt_tokens;
        completion += e.completion_tokens;
        let total = e.prompt_tokens + e.completion_tokens;
        if e.status < 400 {
            ok += 1;
        } else {
            let kind = match e.status {
                400..=499 => format!("http_{}", e.status),
                _ => "unclassified".to_string(),
            };
            *errors.entry(kind).or_insert(0) += 1;
            fallbacks += 1;
        }
        lat_sum += e.latency_ms;
        models.insert(e.model.clone());
        let provider = e.provider.clone().unwrap_or_else(|| "(unrouted)".into());
        providers.insert(provider.clone());
        let day = day_key(e.ts_ms);
        let d = daily.entry(day.clone()).or_insert((0, 0, 0, 0, 0.0));
        d.0 += 1;
        d.1 += e.prompt_tokens;
        d.2 += e.completion_tokens;
        d.3 += total;
        *activity.entry(day).or_insert(0) += total;
        let m = by_model
            .entry(e.model.clone())
            .or_insert((provider.clone(), 0, 0, 0, 0, e.ts_ms, e.latency_ms));
        m.1 += 1;
        m.2 += e.prompt_tokens;
        m.3 += e.completion_tokens;
        m.4 += total;
        m.5 = m.5.max(e.ts_ms);
        m.6 = (m.6 + e.latency_ms) / 2;
        let p = by_provider.entry(provider).or_insert((0, 0, 0, 0, 0, e.ts_ms));
        p.0 += 1;
        p.1 += e.prompt_tokens;
        p.2 += e.completion_tokens;
        p.3 += total;
        p.4 = (p.4 + e.latency_ms) / 2;
        p.5 = p.5.max(e.ts_ms);
        let wd = weekday_index(e.ts_ms);
        weekly_tokens[wd] += total;
        weekly_counts[wd] += 1;
        first_ts = Some(first_ts.map_or(e.ts_ms, |v| v.min(e.ts_ms)));
        last_ts = Some(last_ts.map_or(e.ts_ms, |v| v.max(e.ts_ms)));
    }

    let total_requests = entries.len() as u64;
    let total_tokens = prompt + completion;
    let summary = serde_json::json!({
        "totalRequests": total_requests,
        "promptTokens": prompt,
        "completionTokens": completion,
        "totalTokens": total_tokens,
        "uniqueModels": models.len(),
        "uniqueAccounts": providers.len(),
        "uniqueApiKeys": state.api_keys.list().iter().filter(|k| k.enabled).count(),
        "successfulRequests": ok,
        "successRatePct": if total_requests > 0 { ((ok as f64 / total_requests as f64) * 10000.0).round() / 100.0 } else { 0.0 },
        "avgLatencyMs": lat_sum.checked_div(total_requests).unwrap_or(0),
        "totalCost": 0.0,
        "firstRequest": first_ts.map(iso_from_ms),
        "lastRequest": last_ts.map(iso_from_ms),
        "fallbackCount": fallbacks,
    });

    let daily_trend: Vec<serde_json::Value> = daily
        .iter()
        .map(|(date, (req, pt, ct, tt, cost))| {
            serde_json::json!({
                "date": date, "requests": req, "promptTokens": pt,
                "completionTokens": ct, "totalTokens": tt, "cost": cost,
            })
        })
        .collect();

    let model_rows: Vec<serde_json::Value> = by_model
        .iter()
        .map(|(model, (provider, req, pt, ct, tt, last, avg))| {
            serde_json::json!({
                "model": model, "provider": provider, "requests": req,
                "promptTokens": pt, "completionTokens": ct, "totalTokens": tt,
                "avgLatencyMs": avg, "lastUsed": iso_from_ms(*last), "cost": 0,
            })
        })
        .collect();
    let provider_rows: Vec<serde_json::Value> = by_provider
        .iter()
        .map(|(provider, (req, pt, ct, tt, avg, last))| {
            serde_json::json!({
                "provider": provider, "requests": req, "promptTokens": pt,
                "completionTokens": ct, "totalTokens": tt,
                "avgLatencyMs": avg, "lastUsed": iso_from_ms(*last), "cost": 0,
            })
        })
        .collect();
    let key_rows: Vec<serde_json::Value> = state
        .api_keys
        .list()
        .iter()
        .map(|k| {
            serde_json::json!({
                "apiKeyId": k.id, "name": k.name, "requests": k.total_requests,
                "lastUsed": k.last_used_at_ms.map(iso_from_ms), "cost": k.cost_usd,
            })
        })
        .collect();
    let error_rows: Vec<serde_json::Value> = errors
        .iter()
        .map(|(k, v)| serde_json::json!({"errorType": k, "count": v}))
        .collect();
    let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let weekly_pattern: Vec<serde_json::Value> = weekdays
        .iter()
        .enumerate()
        .map(|(i, d)| {
            serde_json::json!({
                "day": d,
                "avgTokens": weekly_tokens[i].checked_div(weekly_counts[i]).unwrap_or(0),
                "totalTokens": weekly_tokens[i],
            })
        })
        .collect();

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "summary": summary,
            "dailyTrend": daily_trend,
            "activityMap": activity,
            "byModel": model_rows,
            "byProvider": provider_rows,
            "byApiKey": key_rows,
            "byServiceTier": [{"serviceTier": "standard", "label": "standard",
                               "requests": total_requests, "savings": 0, "usageSavingsTokens": 0}],
            "weeklyPattern": weekly_pattern,
            "weeklyTokens": weekly_tokens.to_vec(),
            "weeklyCounts": weekly_counts.to_vec(),
            "modelNames": models.iter().cloned().collect::<Vec<_>>(),
            "errorBreakdown": error_rows,
            "range": "all",
        })),
    )
        .into_response()
}

fn day_key(ts_ms: u128) -> String {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn weekday_index(ts_ms: u128) -> usize {
    // 1970-01-01 was a Thursday (index 4 with Sun = 0)
    let days = (ts_ms / 1000) as i64 / 86_400;
    ((days + 4).rem_euclid(7)) as usize
}

fn iso_from_ms(ts_ms: u128) -> String {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since the Unix epoch → (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `GET /v1/combos/managed` — config combos + dashboard-managed combos, tagged
/// with the category the Combos page filters on.
pub async fn combos_managed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let mut out: Vec<serde_json::Value> = state
        .config
        .combos
        .iter()
        .map(|c| {
            let strategy = c.strategy.clone().unwrap_or_else(|| "priority".into());
            let supports_vision = crate::router::combo::resolve_candidates(&state, &c.name)
                .iter()
                .any(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_vision);
            let supports_pdf = crate::router::combo::resolve_candidates(&state, &c.name)
                .iter()
                .any(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_pdf);
            serde_json::json!({
                "id": format!("config:{}", c.name),
                "name": c.name,
                "strategy": strategy,
                "providers": c.providers,
                "models": c.models,
                "enabled": true,
                "source": "config",
                "category": if matches!(strategy.as_str(), "priority" | "failover" | "round-robin" | "fill-first" | "weighted") { "deterministic" } else { "smart" },
                "tags": [],
                "default_model": c.models.first(),
                "supportsVision": supports_vision,
                "supportsPdf": supports_pdf,
                "modalities": if supports_pdf { serde_json::json!(["text", "image", "pdf"]) } else if supports_vision { serde_json::json!(["text", "image"]) } else { serde_json::json!(["text"]) },
                "contextLength": crate::server::models::combo_context_length(&state, &c.name),
                "contextWindow": crate::server::models::combo_context_length(&state, &c.name),
            })
        })
        .collect();
    for c in state.combos.list() {
        let strategy = c.strategy.clone().unwrap_or_else(|| "priority".into());
        let supports_vision = crate::router::combo::resolve_candidates(&state, &c.name)
            .iter()
            .any(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_vision);
        let supports_pdf = crate::router::combo::resolve_candidates(&state, &c.name)
            .iter()
            .any(|candidate| state.capabilities_for_model(&candidate.provider, &candidate.model).supports_pdf);
        out.push(serde_json::json!({
            "id": c.id, "name": c.name, "strategy": strategy,
            "providers": c.providers, "models": c.models, "enabled": c.enabled,
            "source": "managed",
            "category": if c.is_deterministic() { "deterministic" } else { "smart" },
            "tags": c.tags, "default_model": c.default_model,
            "supportsVision": supports_vision,
            "supportsPdf": supports_pdf,
            "modalities": if supports_pdf { serde_json::json!(["text", "image", "pdf"]) } else if supports_vision { serde_json::json!(["text", "image"]) } else { serde_json::json!(["text"]) },
            "contextLength": crate::server::models::combo_context_length(&state, &c.name),
            "contextWindow": crate::server::models::combo_context_length(&state, &c.name),
        }));
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "combos": out })),
    )
        .into_response()
}

/// `POST /v1/combos/managed` — create/update a dashboard combo.
pub async fn combos_upsert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let mut combo: crate::server::combos_admin::ManagedCombo = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => return crate::errors::ApiError::new(400, format!("invalid combo: {e}")).into(),
    };
    if combo.name.trim().is_empty() {
        return crate::errors::ApiError::new(400, "missing required field: name").into();
    }
    if combo.providers.is_empty() && combo.models.is_empty() {
        return crate::errors::ApiError::new(400, "a combo needs at least one provider or model").into();
    }
    combo.name = combo.name.trim().to_string();
    let saved = state.combos.upsert(combo);
    state.audit("combo.upsert", format!("name={}", saved.name), true);
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({ "combo": saved })),
    )
        .into_response()
}

/// `PATCH /v1/combos/managed/{id}` — toggle a managed combo.
pub async fn combos_patch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let patch: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    if let Some(enabled) = patch.get("enabled").and_then(|e| e.as_bool()) {
        if !state.combos.set_enabled(&id, enabled) {
            return crate::errors::ApiError::new(404, "combo not found (config combos are read-only)").into();
        }
        state.audit("combo.toggle", format!("id={id} enabled={enabled}"), true);
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// `DELETE /v1/combos/managed/{id}`
pub async fn combos_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    if state.combos.remove(&id) {
        state.audit("combo.remove", format!("id={id}"), true);
        (StatusCode::OK, axum::Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        crate::errors::ApiError::new(404, "combo not found").into()
    }
}

/// `GET /v1/combo-presets` — the auto-router catalogue plus the Kimi Coding preset
/// (parity: the Combos page's 自动路由目录 and preset banner).
pub async fn combo_presets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    // The 17 built-in auto/* templates exactly as the original catalogues them:
    // (id, strategy, display title, routing tags, optional system prompt hint)
    // (id, strategy, title, tags, prompt hint)
    type TemplateSpec<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], Option<&'a str>);
    let templates_src: [TemplateSpec; 17] = [
        ("auto/best-coding", "weighted", "Best Coding", &["coding", "premium", "balanced"],
         Some("You are an expert coding assistant. Write clean, efficient, well-documented code.")),
        ("auto/best-reasoning", "weighted", "Best Reasoning", &["reasoning_deep", "reasoning", "premium"],
         Some("You are a deep reasoning assistant. Think carefully step by step.")),
        ("auto/best-fast", "weighted", "Best Fast", &["fast", "fast", "balanced"], None),
        ("auto/best-vision", "weighted", "Best Vision", &["vision", "premium", "balanced"], None),
        ("auto/best-chat", "weighted", "Best Chat", &["chat", "balanced", "premium"], None),
        ("auto/best-coding-fast", "weighted", "Best Coding Fast", &["coding", "fast", "fast", "balanced"], None),
        ("auto/pro-coding", "priority", "Pro Coding", &["coding", "premium"],
         Some("You are an expert coding assistant. Write clean, efficient, well-documented code.")),
        ("auto/pro-reasoning", "priority", "Pro Reasoning", &["reasoning_deep", "premium"],
         Some("You are a deep reasoning assistant. Think carefully step by step.")),
        ("auto/pro-vision", "priority", "Pro Vision", &["vision", "premium"], None),
        ("auto/pro-chat", "priority", "Pro Chat", &["chat", "premium"], None),
        ("auto/pro-fast", "priority", "Pro Fast", &["fast", "fast"], None),
        ("auto/coding", "weighted", "Coding", &["coding", "balanced", "fast", "premium"], None),
        ("auto/fast", "weighted", "Fast", &["fast", "fast"], None),
        ("auto/chat", "weighted", "Chat", &["chat", "balanced", "fast"], None),
        ("auto/claude-opus", "priority", "Claude Opus",
         &["reasoning_deep", "coding", "reasoning", "premium"], None),
        ("auto/claude-sonnet", "priority", "Claude Sonnet",
         &["coding", "reasoning", "chat", "premium", "balanced"], None),
        ("auto/best-free", "weighted", "Best Free", &["coding", "chat", "fast", "free"],
         Some("You are a helpful coding assistant. Write clean, efficient code.")),
    ];
    let connected: Vec<String> = state
        .provider_runtime_snapshot()
        .into_iter()
        .filter(|p| p.has_key)
        .map(|p| p.id)
        .collect();
    let templates: Vec<serde_json::Value> = templates_src
        .iter()
        .map(|(id, strategy, title, tags, prompt)| {
            serde_json::json!({
                "id": id,
                "strategy": strategy,
                "title": title,
                "tags": tags,
                "prompt": prompt,
                "available": !connected.is_empty(),
                "resolves_from": connected,
            })
        })
        .collect();
    let kimi_ready = connected.iter().any(|c| c.contains("kimi"));
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "templates": templates,
            "total": templates.len(),
            "presets": [{
                "id": "kimi-coding",
                "name": "Kimi Coding preset",
                "primary": "kimi/moonshot-v1-8k",
                "fallbacks": ["kimi-coding", "kimi-web"],
                "ready": kimi_ready,
                "description": "Kimi K3 as the primary model (Moonshot API), falling back to your Kimi Code connections (kimi-coding, kimi-web) once configured.",
            }],
        })),
    )
        .into_response()
}

/// `GET /v1/provider-quotas` — quota page data: per-provider severity summary,
/// live window usage and the configured overrides.
pub async fn provider_quotas(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let overrides = state.quota_overrides.list();
    let live = state.provider_runtime_snapshot();
    let mut accounts: Vec<serde_json::Value> = Vec::new();
    let mut totals = (0u32, 0u32, 0u32, 0u32); // total, critical, warning, healthy
    for p in &live {
        let ov = overrides.iter().find(|o| o.provider == p.id);
        let hits = state.rate.hit_count(&p.id);
        let severity = crate::server::combos_admin::QuotaStore::severity(
            hits as u64,
            ov.and_then(|o| o.cutoff),
        );
        match severity {
            "critical" => totals.1 += 1,
            "warning" => totals.2 += 1,
            "healthy" => totals.3 += 1,
            _ => {}
        }
        totals.0 += 1;
        accounts.push(serde_json::json!({
            "provider": p.id,
            "format": p.format,
            "hasKey": p.has_key,
            "active": p.cooldown_ms == 0 && p.has_key,
            "windowHits": hits as u64,
            "concurrent": p.in_flight,
            "cooldownMs": p.cooldown_ms,
            "severity": severity,
            "tier": ov.and_then(|o| o.tier.clone()).unwrap_or_else(|| "unknown".into()),
            "authKind": ov.and_then(|o| o.auth_kind.clone()).unwrap_or_else(|| "apikey".into()),
            "balance": ov.and_then(|o| o.balance),
            "currency": ov.and_then(|o| o.currency.clone()),
            "cutoff": ov.and_then(|o| o.cutoff),
            "note": ov.and_then(|o| o.note.clone()),
            "updatedAtMs": ov.map(|o| o.updated_at_ms).unwrap_or(0),
        }));
    }
    // overrides for providers with no live registration still render an account
    for ov in &overrides {
        if !live.iter().any(|p| p.id == ov.provider) {
            totals.0 += 1;
            accounts.push(serde_json::json!({
                "provider": ov.provider,
                "format": "configured",
                "hasKey": true,
                "active": false,
                "windowHits": 0,
                "concurrent": 0,
                "cooldownMs": 0,
                "severity": "unknown",
                "tier": ov.tier.clone().unwrap_or_else(|| "unknown".into()),
                "authKind": ov.auth_kind.clone().unwrap_or_else(|| "apikey".into()),
                "balance": ov.balance,
                "currency": ov.currency.clone(),
                "cutoff": ov.cutoff,
                "note": ov.note.clone(),
                "updatedAtMs": ov.updated_at_ms,
            }));
        }
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "accounts": accounts,
            "summary": {
                "total": totals.0, "critical": totals.1,
                "warning": totals.2, "healthy": totals.3,
            },
        })),
    )
        .into_response()
}

/// `POST /v1/provider-quotas/{provider}` — set an account's cutoff/balance/note.
pub async fn provider_quotas_upsert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    let mut o = state
        .quota_overrides
        .list()
        .into_iter()
        .find(|x| x.provider == provider)
        .unwrap_or_default();
    o.provider = provider.clone();
    if let Some(v) = body.get("cutoff") {
        o.cutoff = v.as_f64();
    }
    if let Some(v) = body.get("balance") {
        o.balance = v.as_f64();
    }
    if let Some(v) = body.get("currency").and_then(|v| v.as_str()) {
        o.currency = Some(v.to_string());
    }
    if let Some(v) = body.get("tier").and_then(|v| v.as_str()) {
        o.tier = Some(v.to_string());
    }
    if let Some(v) = body.get("note").and_then(|v| v.as_str()) {
        o.note = Some(v.to_string());
    }
    if let Some(v) = body.get("rpm").and_then(|v| v.as_u64()) {
        o.rpm = Some(v);
    }
    if let Some(v) = body.get("concurrent").and_then(|v| v.as_u64()) {
        o.concurrent = Some(v);
    }
    let saved = state.quota_overrides.upsert(o);
    state.audit("quota.upsert", format!("provider={provider}"), true);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "quota": saved })),
    )
        .into_response()
}

/// `GET /v1/combo-studio` — every combo with its resolved candidate chain and
/// live state (parity: the Combos Studio page's live routing view).
pub async fn combo_studio(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let mut names: Vec<String> = state.config.combos.iter().map(|c| c.name.clone()).collect();
    names.extend(state.combos.list().into_iter().filter(|c| c.enabled).map(|c| c.name));
    let live = state.provider_runtime_snapshot();
    let combos: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let candidates = crate::router::combo::resolve_candidates(&state, name);
            let chain: Vec<serde_json::Value> = candidates
                .iter()
                .map(|c| {
                    let rt = live.iter().find(|p| p.id == c.provider);
                    serde_json::json!({
                        "provider": c.provider, "model": c.model, "position": c.position,
                        "available": state.circuits.is_available(&c.provider),
                        "cooldownMs": rt.map(|p| p.cooldown_ms).unwrap_or(0),
                        "inFlight": rt.map(|p| p.in_flight).unwrap_or(0),
                        "hasKey": rt.map(|p| p.has_key).unwrap_or(false),
                        "modelBanned": state.circuits.is_model_banned(&c.provider, &c.model),
                        "supportsVision": state.capabilities_for_model(&c.provider, &c.model).supports_vision,
                    })
                })
                .collect();
            let selected = chain.iter().find(|c| {
                c["available"] == serde_json::json!(true) && c["modelBanned"] == serde_json::json!(false)
            });
            serde_json::json!({
                "combo": name,
                "candidates": chain,
                "supportsVision": chain.iter().any(|c| c["supportsVision"] == serde_json::json!(true)),
                "modalities": if chain.iter().any(|c| c["supportsVision"] == serde_json::json!(true)) { serde_json::json!(["text", "image"]) } else { serde_json::json!(["text"]) },
                "contextLength": crate::server::models::combo_context_length(&state, name),
                "selected": selected.map(|c| c["provider"].clone()),
                "healthy": selected.is_some(),
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "combos": combos })),
    )
        .into_response()
}

/// `GET /v1/routing/trace` — recent requests joined with the routing chain that
/// served (or was resolved for) them (parity: Usage → Route tracing).
pub async fn routing_trace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let q = Query::from_uri(&uri);
    let limit = q.parse("limit").unwrap_or(50).min(500);
    let live = state.provider_runtime_snapshot();
    let mut traces = Vec::new();
    for e in state.request_log_snapshot(limit) {
        let candidates = crate::router::combo::resolve_candidates(&state, &e.model);
        let chain: Vec<serde_json::Value> = candidates
            .iter()
            .take(6)
            .map(|c| serde_json::json!({"provider": c.provider, "model": c.model, "position": c.position}))
            .collect();
        traces.push(serde_json::json!({
            "ts_ms": e.ts_ms,
            "model": e.model,
            "provider": e.provider,
            "status": e.status,
            "latency_ms": e.latency_ms,
            "stream": e.stream,
            "compressed": e.compressed,
            "tokens_saved": e.tokens_saved,
            "prompt_tokens": e.prompt_tokens,
            "completion_tokens": e.completion_tokens,
            "fallback": e.status >= 400,
            "candidate_chain": chain,
            "chain_len": chain.len(),
            "served_position": e.provider.as_ref().and_then(|p| {
                candidates.iter().position(|c| &c.provider == p).map(|i| i + 1)
            }),
            "provider_healthy": live.iter().find(|p| Some(&p.id) == e.provider.as_ref()).map(|p| p.cooldown_ms == 0),
        }));
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "traces": traces, "count": traces.len() })),
    )
        .into_response()
}

/// `GET /v1/embedded-services` — local/bundled execution surfaces. The Rust build
/// ships the local provider family (ollama/lmstudio/…); the browser-driven
/// executors of the original are reported as unavailable rather than faked.
pub async fn embedded_services(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let live = state.provider_runtime_snapshot();
    let local: Vec<serde_json::Value> = live
        .iter()
        .filter(|p| {
            p.id.starts_with("ollama") || p.id.starts_with("lmstudio") || p.id.contains("local")
        })
        .map(|p| {
            serde_json::json!({
                "id": p.id, "format": p.format, "hasKey": p.has_key,
                "inFlight": p.in_flight, "cooldownMs": p.cooldown_ms,
                "baseUrl": state.base_url_for(&state.registry, &p.id),
                "kind": "local-inference",
            })
        })
        .collect();
    let bundled = [
        ("playwright-scraper", "browser automation for web providers"),
        ("codex-cli", "local Codex CLI bridge"),
        ("claude-code-cli", "local Claude Code bridge"),
        ("gemini-cli", "local Gemini CLI bridge"),
    ];
    let executors: Vec<serde_json::Value> = bundled
        .iter()
        .map(|(id, desc)| {
            serde_json::json!({
                "id": id, "description": desc, "available": false,
                "reason": "browser/CLI executors are not part of the Rust build",
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "localProviders": local,
            "bundledExecutors": executors,
            "pythonRuntime": false,
            "nodeRuntime": false,
        })),
    )
        .into_response()
}

/// `GET /v1/quota-share` — how a provider's quota is shared across keys
/// (parity: the 配额共享 page). The Rust gateway has a single global rate
/// limiter per provider, so the share policy is the provider-level budget.
pub async fn quota_share(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let overrides = state.quota_overrides.list();
    let live = state.provider_runtime_snapshot();
    let keys = state.api_keys.list();
    let enabled_keys = keys.iter().filter(|k| k.enabled).count();
    let shares: Vec<serde_json::Value> = overrides
        .iter()
        .map(|o| {
            let rt = live.iter().find(|p| p.id == o.provider);
            let rpm = o.rpm.unwrap_or(crate::router::circuit::DEFAULT_RPM);
            let hits = state.rate.hit_count(&o.provider) as u64;
            serde_json::json!({
                "provider": o.provider,
                "sharedRpm": rpm,
                "windowHits": hits,
                "keysInPool": enabled_keys,
                "perKeyRpm": if enabled_keys > 0 { rpm / enabled_keys as u64 } else { rpm },
                "concurrent": o.concurrent.unwrap_or(6),
                "inFlight": rt.map(|p| p.in_flight).unwrap_or(0),
                "cooldownMs": rt.map(|p| p.cooldown_ms).unwrap_or(0),
                "tier": o.tier.clone().unwrap_or_else(|| "unknown".into()),
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "shares": shares,
            "enabledKeys": enabled_keys,
            "totalKeys": keys.len(),
            "note": "the Rust gateway applies one shared budget per provider (rpm + concurrency); per-key split is advisory",
        })),
    )
        .into_response()
}

/// `GET /v1/cache/health` — cache/dedup effectiveness (parity: Usage → Cache
/// Health). The Rust build has no semantic cache; the numbers reported are the
/// real compression/dedup savings from the request ring.
pub async fn cache_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let entries = state.request_log_snapshot(1000);
    let compressed = entries.iter().filter(|e| e.compressed).count();
    let saved: i64 = entries.iter().map(|e| e.tokens_saved).sum();
    let prompt: u64 = entries.iter().map(|e| e.prompt_tokens).sum();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "semanticCache": {"enabled": false, "entries": 0, "hits": 0, "misses": entries.len(),
                              "reason": "semantic cache is not part of the Rust build"},
            "dedup": {"requestsCompressed": compressed,
                      "tokensSaved": saved,
                      "savedRatioPct": if prompt + saved as u64 > 0 { ((saved.max(0) as f64 / (prompt + saved.max(0) as u64) as f64) * 10000.0).round() / 100.0 } else { 0.0 }},
            "sampled": entries.len(),
        })),
    )
        .into_response()
}

/// `GET /v1/endpoints` — the Endpoints page: active endpoints, local server
/// identity, the endpoint catalogue with per-endpoint model counts, and the
/// gateway-wide custom system prompt (parity: the original's API 端点 page).
pub async fn endpoints_overview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let catalog: serde_json::Value =
        serde_json::from_str(PROVIDER_CATALOG).unwrap_or(serde_json::json!([]));
    let connected: Vec<String> = state
        .provider_runtime_snapshot()
        .into_iter()
        .filter(|p| p.has_key)
        .map(|p| p.id)
        .collect();
    // models exposed by connected providers (the gateway's own catalogue size)
    let models_total = state
        .registry
        .ids()
        .iter()
        .filter(|id| connected.contains(id))
        .flat_map(|id| state.registry.models_for(id))
        .count();
    let media_models = |kind: &str| -> usize {
        catalog
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|p| {
                        let id = p.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        connected.iter().any(|c| c == id)
                            && p.get("serviceKinds")
                                .and_then(|k| k.as_array())
                                .map(|ks| ks.iter().any(|k| k.as_str() == Some(kind)))
                                .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    let schemes = [
        ("chat", "/v1/chat/completions", "chat", models_total),
        ("responses", "/v1/responses", "chat", models_total),
        ("completions", "/v1/completions", "chat", models_total),
        ("messages", "/v1/messages", "chat", models_total),
        ("embeddings", "/v1/embeddings", "embedding", media_models("embedding")),
        ("images-generations", "/v1/images/generations", "image", media_models("image")),
        ("images-edits", "/v1/images/edits", "image", media_models("image")),
        ("audio-transcriptions", "/v1/audio/transcriptions", "audio", media_models("audio")),
        ("audio-speech", "/v1/audio/speech", "audio", media_models("audio")),
        ("music-generations", "/v1/music/generations", "music", media_models("music")),
        ("videos-generations", "/v1/videos/generations", "video", media_models("video")),
        ("search", "/v1/search", "search", media_models("webSearch")),
        ("rerank", "/v1/rerank", "rerank", 0),
        ("moderations", "/v1/moderations", "moderation", 0),
        ("batches", "/v1/batches", "batch", 0),
        ("files", "/v1/files", "file", 0),
        ("models-list", "/v1/models", "models", models_total),
    ];
    let endpoints: Vec<serde_json::Value> = schemes
        .iter()
        .map(|(id, path, kind, models)| {
            serde_json::json!({"id": id, "path": path, "kind": kind, "models": models})
        })
        .collect();
    let port = state.config.port;
    let host = state.config.host.clone();
    let server_id = format!(
        "{:08x}",
        std::process::id() as u64 ^ (crate::server::security::now_ms() as u64)
    );
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "active": {
                "public": format!("http://{}:{}/v1", host, port),
                "local": format!("http://localhost:{}/v1", port),
            },
            "localServer": {
                "id": server_id,
                "url": format!("http://{}:{}/v1", host, port),
                "running": true,
            },
            "endpoints": endpoints,
            "modelsTotal": models_total,
            "customSystemPrompt": state.custom_system_prompt(),
            "tunnels": [
                {"id": "cloud-router", "label": "Cloud router", "state": "disabled",
                 "reason": "the Rust build does not proxy through a cloud control plane"},
                {"id": "cloudflare", "label": "Cloudflare quick tunnel", "state": "not-installed",
                 "reason": "tunnel clients are not bundled"},
                {"id": "tailscale", "label": "Tailscale tunnel", "state": "not-installed",
                 "reason": "tunnel clients are not bundled"},
                {"id": "ngrok", "label": "ngrok tunnel", "state": "needs-auth",
                 "reason": "tunnel clients are not bundled"},
            ],
            "vscodeAlias": {"implemented": false,
                            "reason": "the /api/v1/vscode/<token>/ compatibility alias is not part of the Rust build"},
        })),
    )
        .into_response()
}

/// `POST /v1/settings/custom-system-prompt` — set (or clear) the gateway-wide
/// system prompt injected into every chat request.
pub async fn custom_system_prompt_set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(e) = crate::server::auth::require_management(&state, &headers) {
        return e.into();
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    let value = body
        .get("prompt")
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    state.set_custom_system_prompt(value.clone());
    state.audit("settings.custom_system_prompt", if value.is_some() { "set" } else { "cleared" }, true);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "customSystemPrompt": value })),
    )
        .into_response()
}
