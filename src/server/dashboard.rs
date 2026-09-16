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
    let limit: usize = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("limit=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(1000);
    let entries = state.request_log_snapshot(limit);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "logs": entries })),
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
