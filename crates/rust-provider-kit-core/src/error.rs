use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCoreErrorCode {
    InvalidIdentifier,
    InvalidValue,
    InvalidRequest,
    InvalidTransition,
    GenerationExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct ProviderCoreError {
    pub code: ProviderCoreErrorCode,
    pub message: String,
}

impl ProviderCoreError {
    #[must_use]
    pub fn new(code: ProviderCoreErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn invalid_identifier(message: impl Into<String>) -> Self {
        Self::new(ProviderCoreErrorCode::InvalidIdentifier, message)
    }

    #[must_use]
    pub fn invalid_value(message: impl Into<String>) -> Self {
        Self::new(ProviderCoreErrorCode::InvalidValue, message)
    }

    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ProviderCoreErrorCode::InvalidRequest, message)
    }

    #[must_use]
    pub fn invalid_transition(message: impl Into<String>) -> Self {
        Self::new(ProviderCoreErrorCode::InvalidTransition, message)
    }
}

pub(crate) fn is_control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

pub(crate) fn validate_trimmed_text(
    value: &str,
    maximum_utf8_bytes: usize,
    field: &str,
) -> Result<(), ProviderCoreError> {
    if value.is_empty()
        || value.len() > maximum_utf8_bytes
        || value.trim() != value
        || is_control(value)
    {
        return Err(ProviderCoreError::invalid_value(format!(
            "{field} is invalid"
        )));
    }
    Ok(())
}
