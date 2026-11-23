//! Error types and HTTP status mapping for `open-ci-cache`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

/// Errors returned by the cache service, authentication layer, and storage backends.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    #[error("range not satisfiable: {0}")]
    RangeNotSatisfiable(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    msg: String,
}

impl CacheError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidConfig(_) | Self::BadRequest(_) => {
                (StatusCode::BAD_REQUEST, "invalid_argument")
            }
            Self::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "permission_denied"),
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Self::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "resource_exhausted"),
            Self::RangeNotSatisfiable(_) => (StatusCode::RANGE_NOT_SATISFIABLE, "out_of_range"),
            Self::Storage(_) | Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl IntoResponse for CacheError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = ErrorBody {
            code,
            msg: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}
