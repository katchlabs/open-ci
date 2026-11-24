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

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("length required: {0}")]
    LengthRequired(String),

    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    #[error("range not satisfiable: {0}")]
    RangeNotSatisfiable(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("upstream unavailable: {0}")]
    BadGateway(String),
}

impl From<rusqlite::Error> for CacheError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Storage(format!("cache index: {err}"))
    }
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
            Self::AlreadyExists(_) => (StatusCode::CONFLICT, "already_exists"),
            Self::LengthRequired(_) => (StatusCode::LENGTH_REQUIRED, "invalid_argument"),
            Self::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "resource_exhausted"),
            Self::RangeNotSatisfiable(_) => (StatusCode::RANGE_NOT_SATISFIABLE, "out_of_range"),
            Self::Storage(_) | Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            Self::BadGateway(_) => (StatusCode::BAD_GATEWAY, "unavailable"),
        }
    }
}

impl IntoResponse for CacheError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let msg = if status.is_server_error() {
            tracing::error!(error = %self, "cache request failed");
            "cache service error".to_owned()
        } else {
            self.to_string()
        };
        let body = ErrorBody { code, msg };
        (status, Json(body)).into_response()
    }
}
