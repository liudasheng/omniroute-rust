//! Unified error type + OpenAI error shape mapping (parity with
//! `open-sse/config/errorConfig.ts#ERROR_TYPES`).

use serde_json::{Value, json};
use std::fmt;

/// Error taxonomy per HTTP status, mirroring ERROR_TYPES.
pub fn error_type_for(status: u16) -> (&'static str, &'static str) {
    match status {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("payment_required", "payment_required"),
        403 => ("insufficient_quota", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("invalid_request_error", "request_timeout"),
        413 => ("invalid_request_error", "context_length_exceeded"),
        422 => ("invalid_request_error", "unprocessable_entity"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        499 => ("invalid_request_error", "client_disconnected"),
        500 => ("server_error", "internal_error"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        _ => ("server_error", "internal_error"),
    }
}

pub fn default_error_message(status: u16) -> &'static str {
    match status {
        400 => "The request was malformed or missing required parameters.",
        401 => "Authentication failed: invalid or missing credentials.",
        402 => "Payment required: the provider account has no active billing.",
        403 => "Quota or permission denied for this request.",
        404 => "The requested resource or model was not found.",
        413 => "Request body exceeds the maximum context length accepted by this endpoint.",
        429 => "Rate limit exceeded, please retry later.",
        499 => "Client disconnected before the response completed.",
        500 => "Internal server error.",
        502 => "Bad gateway: upstream returned an invalid response.",
        503 => "Service unavailable: upstream is temporarily down.",
        504 => "Gateway timeout: upstream did not respond in time.",
        _ => "Internal server error.",
    }
}

/// Gateway-level API error rendered in OpenAI shape.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub etype: String,
    pub code: String,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        let (etype, code) = error_type_for(status);
        Self {
            status,
            message: message.into(),
            etype: etype.to_string(),
            code: code.to_string(),
        }
    }

    /// Over-limit request body, rendered as a context-size error.
    ///
    /// A bare 413 reads as an unretryable bad request to agent clients, which
    /// then fail the turn; wording and code here name a context bound, so a
    /// client with context compaction (the DSH harness, Claude Code, ...)
    /// compacts the conversation and retries instead of giving up.
    pub fn request_too_large(limit_bytes: usize, declared_bytes: Option<usize>) -> Self {
        let got = declared_bytes
            .map(|n| format!("{n} bytes"))
            .unwrap_or_else(|| "unknown size".to_string());
        Self {
            status: 413,
            message: format!(
                "Request body exceeds the maximum context length accepted by this gateway \
                 (limit {limit_bytes} bytes, request {got}). Compact the conversation context \
                 and retry, or raise OMNIROUTE_MAX_BODY_BYTES."
            ),
            etype: "invalid_request_error".into(),
            code: "context_length_exceeded".into(),
        }
    }

    /// Request larger than every candidate model's context window.
    ///
    /// Distinct from `404 model_not_found`: the model exists, the conversation
    /// does not fit. Clients that own context compaction (the DSH harness,
    /// Claude Code, ...) route on this and retry after shrinking the context.
    pub fn context_window_exceeded(required_tokens: i64, window_tokens: i64, model: &str) -> Self {
        Self {
            status: 400,
            message: format!(
                "Request exceeds the model context window: estimated {required_tokens} tokens \
                 is larger than the {window_tokens} token context window of '{model}'. \
                 Reduce the conversation context and retry."
            ),
            etype: "invalid_request_error".into(),
            code: "context_length_exceeded".into(),
        }
    }

    /// The upstream zero-cost lane's own gate refused this gateway.
    ///
    /// OpenCode's free tier answers `403 FreeTierError` ("the free tier can
    /// only be used from within OpenCode") to every request shape a gateway can
    /// produce. Forwarding that status is actively misleading: clients classify
    /// a bare 401/403 as an authentication failure — the DSH harness renders it
    /// as "API 密钥无效" and stops the turn — while the truth is an upstream
    /// policy on a free lane, with the caller's key and this gateway's quota
    /// both untouched. Report it as a retryable `503 service_unavailable`
    /// instead, and keep the wording free of the tokens `401`/`403` because
    /// that same client matches those substrings inside the message text.
    pub fn free_lane_unavailable(detail: &str) -> Self {
        let detail = redact_auth_status_tokens(detail.trim());
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(" Upstream said: {detail}")
        };
        Self {
            status: 503,
            message: format!(
                "The zero-cost OpenCode lane is unavailable: its upstream gate refused this \
                 request (that tier may only be driven from inside the OpenCode client, or the \
                 upstream is restricting this network/region). No API key or gateway quota is at \
                 fault — retry later, or select a keyed model.{suffix}"
            ),
            etype: "service_unavailable".into(),
            code: "free_lane_unavailable".into(),
        }
    }

    pub fn not_found_unknown_route(path: &str) -> Self {
        Self {
            status: 404,
            message: format!("Unknown route: {path}"),
            etype: "not_found".into(),
            code: "unknown_route".into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.etype,
                "code": self.code,
            }
        })
    }
}

/// Replace authentication-looking status tokens inside free text.
///
/// Clients that classify a failure by scanning the message for `401`/`403`
/// (the DSH harness does exactly that, before any quota/rate-limit check) would
/// read an upstream body that merely *mentions* one of those codes as a bad API
/// key. Keeping them out of our own prose is what makes the honest `503` stick.
fn redact_auth_status_tokens(text: &str) -> String {
    let mut out: String = text.replace("403", "4xx").replace("401", "4xx");
    if out.chars().count() > 300 {
        out = out.chars().take(300).collect();
        out.push('…');
    }
    out
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} {} {}] {}", self.status, self.etype, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        Into::<axum::response::Response>::into(self)
    }
}

impl From<ApiError> for axum::response::Response {
    fn from(e: ApiError) -> Self {
        use axum::http::StatusCode;
        let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = axum::response::Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(e.to_json().to_string()))
            .unwrap();
        // X-Accel-Buffering off so proxies do not buffer SSE.
        resp.headers_mut().insert("x-accel-buffering", "no".parse().unwrap());
        resp
    }
}

/// Classification of an upstream failure, driving fallback/cooldown policy
/// (parity with `open-sse/services/accountFallback.ts#checkFallbackError` and
/// `RateLimitReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 401/403 auth error
    Auth,
    /// 402 payment required
    Payment,
    /// 404 model not found
    NotFound,
    /// 429 rate limit / quota
    RateLimit,
    /// 5xx transient server error
    Server,
    /// 400 bad request (usually client-fixable)
    Client,
    /// network/timeout, no status
    Network,
}

impl FailureKind {
    pub fn from_status(status: u16) -> Self {
        match status {
            400 => FailureKind::Client,
            401 | 403 => FailureKind::Auth,
            402 => FailureKind::Payment,
            404 => FailureKind::NotFound,
            408 => FailureKind::Server,
            429 => FailureKind::RateLimit,
            499 => FailureKind::Network,
            s if s >= 500 => FailureKind::Server,
            _ => FailureKind::Server,
        }
    }

    pub fn is_fallback_trigger(&self) -> bool {
        matches!(
            self,
            FailureKind::Auth
                | FailureKind::Payment
                | FailureKind::NotFound
                | FailureKind::RateLimit
                | FailureKind::Server
                | FailureKind::Network
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_status_to_openai_error_type() {
        assert_eq!(error_type_for(401).0, "authentication_error");
        assert_eq!(error_type_for(401).1, "invalid_api_key");
        assert_eq!(error_type_for(429).0, "rate_limit_error");
        assert_eq!(error_type_for(403).1, "insufficient_quota");
        assert_eq!(error_type_for(503).1, "service_unavailable");
        assert_eq!(error_type_for(404).1, "model_not_found");
    }

    #[test]
    fn free_lane_gate_refusal_is_never_rendered_as_an_auth_error() {
        let e = ApiError::free_lane_unavailable(
            r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode."}}"#,
        );
        assert_eq!(e.status, 503, "a lane gate is temporary unavailability, not a bad key");
        assert_eq!(e.code, "free_lane_unavailable");
        assert_eq!(e.etype, "service_unavailable");
        // The DSH harness classifies by scanning the message text for these
        // tokens *before* it looks at anything else; both the status and the
        // prose therefore have to stay out of the auth class.
        assert!(!e.message.contains("401") && !e.message.contains("403"), "{}", e.message);
        assert!(e.message.contains("No API key"));
        // An upstream body that mentions an auth code is redacted, not copied.
        let redacted = ApiError::free_lane_unavailable("upstream said 403 Forbidden");
        assert!(!redacted.message.contains("403"));
        assert!(redacted.message.contains("4xx"));
    }

    #[test]
    fn renders_openai_shape() {
        let e = ApiError::new(429, "slow down");
        let v = e.to_json();
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["code"], "rate_limit_exceeded");
        assert_eq!(v["error"]["message"], "slow down");
    }

    #[test]
    fn failure_classification() {
        assert_eq!(FailureKind::from_status(401), FailureKind::Auth);
        assert_eq!(FailureKind::from_status(402), FailureKind::Payment);
        assert_eq!(FailureKind::from_status(404), FailureKind::NotFound);
        assert_eq!(FailureKind::from_status(429), FailureKind::RateLimit);
        assert_eq!(FailureKind::from_status(502), FailureKind::Server);
        assert_eq!(FailureKind::from_status(400), FailureKind::Client);
        // 400 is client-fixable: not a fallback trigger, no provider penalty.
        assert!(!FailureKind::Client.is_fallback_trigger());
        assert!(FailureKind::Server.is_fallback_trigger());
    }

    #[test]
    fn unknown_route_404_shape() {
        let e = ApiError::not_found_unknown_route("/v1/nope");
        assert_eq!(e.status, 404);
        assert_eq!(e.etype, "not_found");
        assert_eq!(e.code, "unknown_route");
    }
}
