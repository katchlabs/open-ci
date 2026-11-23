//! Axum HTTP router combining public machine ingress and authenticated operator APIs.

pub mod ingress;
pub mod operator;

use crate::{ControlPlane, ServerError};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;

/// Clean, secret-free HTTP error envelope.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl From<ServerError> for ApiError {
    fn from(err: ServerError) -> Self {
        match err {
            ServerError::Unauthorized => {
                Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized")
            }
            ServerError::BootstrapUnavailable => Self::new(
                StatusCode::CONFLICT,
                "bootstrap_unavailable",
                "bootstrap capability unavailable",
            ),
            ServerError::BadRequest(msg) | ServerError::InvalidConfig(msg) => {
                Self::new(StatusCode::BAD_REQUEST, "bad_request", msg)
            }
            ServerError::Domain(e) => {
                Self::new(StatusCode::BAD_REQUEST, "domain_error", e.to_string())
            }
            ServerError::GitHub(_) | ServerError::Provider(_) | ServerError::Store(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "control plane dependency unavailable",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                }
            })),
        )
            .into_response()
    }
}

pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
}

/// Builds the complete `open-ci` Axum router (public machine ingress + UI-ready operator API).
pub fn build_router(state: ControlPlane) -> Router {
    let operator_router = if state.config.server.cors_allowed_origins.is_empty() {
        operator::routes(state.clone())
    } else {
        let origins: Vec<_> = state
            .config
            .server
            .cors_allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        let cors = CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);
        operator::routes(state.clone()).layer(cors)
    };

    Router::new()
        .merge(ingress::routes())
        .merge(operator_router)
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .with_state(state)
}
