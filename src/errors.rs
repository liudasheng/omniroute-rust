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

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} {} {}] {}", self.status, self.etype, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

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
