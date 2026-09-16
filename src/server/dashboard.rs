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
