//! Request-body extraction for the model-facing endpoints.
//!
//! `axum::body::Bytes` buffers the whole body and rejects anything over the
//! router's `DefaultBodyLimit` with a bare text `413` — a response agent
//! clients classify as an unretryable bad request. Long agent contexts are a
//! *context size* problem, so the rejection is re-rendered in the OpenAI error
//! shape naming a context bound, which lets a client that owns context
//! compaction shrink the conversation and retry the turn.

use crate::errors::ApiError;
use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::{header::CONTENT_LENGTH, StatusCode};
use axum::response::{IntoResponse, Response};

/// Request body cap, mirrored into request extensions by the router so the
/// rejection path can report the configured limit.
#[derive(Debug, Clone, Copy)]
pub struct MaxBodyBytes(pub usize);

/// `Bytes` with an OpenAI-shaped over-limit rejection.
pub struct ApiBytes(pub Bytes);

impl<S> FromRequest<S> for ApiBytes
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let declared = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        let limit = req.extensions().get::<MaxBodyBytes>().map(|m| m.0);
        match Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes)),
            Err(rejection) => {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    let limit = limit.unwrap_or(crate::config::DEFAULT_MAX_BODY_BYTES);
                    return Err(ApiError::request_too_large(limit, declared).into_response());
                }
                Err((rejection.status(), rejection.body_text()).into_response())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_BODY_BYTES;

    #[test]
    fn over_limit_error_names_a_context_bound() {
        let e = ApiError::request_too_large(DEFAULT_MAX_BODY_BYTES, Some(DEFAULT_MAX_BODY_BYTES + 1));
        assert_eq!(e.status, 413);
        assert_eq!(e.code, "context_length_exceeded");
        let message = e.to_json()["error"]["message"].as_str().unwrap().to_string();
        assert!(message.contains("maximum context length"), "{message}");
        assert!(message.contains(&(DEFAULT_MAX_BODY_BYTES + 1).to_string()), "{message}");
    }
}
