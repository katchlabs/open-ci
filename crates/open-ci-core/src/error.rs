//! Typed domain and configuration errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration parse failed: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("invalid field `{field}`: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
}

impl ConfigError {
    pub fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            message: message.into(),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("capacity overflow while summing resource dimensions")]
    CapacityOverflow,

    #[error("unknown pool `{0}`")]
    UnknownPool(String),

    #[error("invalid admission request: {0}")]
    InvalidRequest(String),

    #[error("invalid worker phase transition from {from:?} to {to:?}")]
    InvalidPhaseTransition {
        from: crate::WorkerPhase,
        to: crate::WorkerPhase,
    },
}
