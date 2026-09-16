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
            "providers_with_keys": state.config.providers_with_keys().len(),
            "models": state
                .config
                .providers_with_keys()
                .iter()
                .map(|id| state.registry.get(id).map(|e| e.default_models.len()).unwrap_or(0))
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
                    let conn = connections.iter().find(|c| c.provider == id || c.id == id);
                    let live = runtime.iter().find(|r| r.id == id || r.id.ends_with(&format!("-{id}")));
                    let mut v = p.clone();
                    v["connected"] = serde_json::json!(conn.is_some() || live.map(|l| l.has_key).unwrap_or(false));
                    v["hasKey"] = serde_json::json!(live.map(|l| l.has_key).unwrap_or(false) || conn.and_then(|c| c.api_key.clone()).is_some());
                    v["cooldownMs"] = serde_json::json!(live.map(|l| l.cooldown_ms).unwrap_or(0));
                    v["inFlight"] = serde_json::json!(live.map(|l| l.in_flight).unwrap_or(0));
                    v["connectionId"] = serde_json::json!(conn.map(|c| c.id.clone()));
                    v["enabled"] = serde_json::json!(conn.map(|c| c.enabled).unwrap_or(false));
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "providers": listed, "total": listed.len() })),
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
    let mut by_model: std::collections::BTreeMap<String, (String, u64, u64, u64, u64, u128, u64)> = Default::default();
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
        "avgLatencyMs": if total_requests > 0 { lat_sum / total_requests } else { 0 },
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
                "avgTokens": if weekly_counts[i] > 0 { weekly_tokens[i] / weekly_counts[i] } else { 0 },
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
