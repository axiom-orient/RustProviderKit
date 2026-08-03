use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    ProviderContinuation, ProviderCoreError, ProviderId, ProviderInstant, ProviderModelId,
    ProviderNativeState, ProviderRequestId, ProviderToolCall, is_control,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderResponseMetadata {
    request_id: ProviderRequestId,
    provider_request_id: Option<String>,
    provider_id: ProviderId,
    model_id: ProviderModelId,
    started_at: ProviderInstant,
}
impl ProviderResponseMetadata {
    pub fn new(
        request_id: ProviderRequestId,
        provider_request_id: Option<String>,
        provider_id: ProviderId,
        model_id: ProviderModelId,
        started_at: ProviderInstant,
    ) -> Result<Self, ProviderCoreError> {
        if let Some(value) = &provider_request_id
            && (value.is_empty() || value.len() > 512 || is_control(value))
        {
            return Err(ProviderCoreError::invalid_value(
                "provider request metadata is invalid",
            ));
        }
        Ok(Self {
            request_id,
            provider_request_id,
            provider_id,
            model_id,
            started_at,
        })
    }
    #[must_use]
    pub fn request_id(&self) -> &ProviderRequestId {
        &self.request_id
    }
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn model_id(&self) -> &ProviderModelId {
        &self.model_id
    }
    #[must_use]
    pub fn started_at(&self) -> ProviderInstant {
        self.started_at
    }
}
#[derive(Deserialize)]
struct ProviderResponseMetadataRaw {
    request_id: ProviderRequestId,
    provider_request_id: Option<String>,
    provider_id: ProviderId,
    model_id: ProviderModelId,
    started_at: ProviderInstant,
}
impl<'de> Deserialize<'de> for ProviderResponseMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderResponseMetadataRaw::deserialize(deserializer)?;
        Self::new(
            raw.request_id,
            raw.provider_request_id,
            raw.provider_id,
            raw.model_id,
            raw.started_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ProviderUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    total_tokens: Option<u64>,
}
impl ProviderUsage {
    pub fn new(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        total_tokens: Option<u64>,
    ) -> Result<Self, ProviderCoreError> {
        const SWIFT_INT_MAX: u64 = i64::MAX as u64;
        if [
            input_tokens,
            output_tokens,
            cached_input_tokens,
            total_tokens,
        ]
        .into_iter()
        .flatten()
        .any(|value| value > SWIFT_INT_MAX)
        {
            return Err(ProviderCoreError::invalid_value(
                "provider token count exceeds the source contract range",
            ));
        }
        Ok(Self {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            total_tokens,
        })
    }
    #[must_use]
    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }
    #[must_use]
    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }
    #[must_use]
    pub fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }
    #[must_use]
    pub fn total_tokens(&self) -> Option<u64> {
        self.total_tokens
    }
}
impl<'de> Deserialize<'de> for ProviderUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            input_tokens: Option<u64>,
            output_tokens: Option<u64>,
            cached_input_tokens: Option<u64>,
            total_tokens: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.input_tokens,
            raw.output_tokens,
            raw.cached_input_tokens,
            raw.total_tokens,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCompletion {
    response_id: Option<String>,
    continuation: Option<ProviderContinuation>,
    native_state: Option<ProviderNativeState>,
    usage: Option<ProviderUsage>,
    finished_at: ProviderInstant,
}
impl ProviderCompletion {
    pub fn new(
        response_id: Option<String>,
        continuation: Option<ProviderContinuation>,
        usage: Option<ProviderUsage>,
        finished_at: ProviderInstant,
    ) -> Result<Self, ProviderCoreError> {
        if let Some(value) = &response_id
            && (value.is_empty() || value.len() > 512 || is_control(value))
        {
            return Err(ProviderCoreError::invalid_value(
                "provider response ID is invalid",
            ));
        }
        Ok(Self {
            response_id,
            continuation,
            native_state: None,
            usage,
            finished_at,
        })
    }
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }
    #[must_use]
    pub fn continuation(&self) -> Option<&ProviderContinuation> {
        self.continuation.as_ref()
    }
    pub fn with_native_state(
        mut self,
        native_state: ProviderNativeState,
    ) -> Result<Self, ProviderCoreError> {
        native_state.payload().validated()?;
        self.native_state = Some(native_state);
        Ok(self)
    }
    #[must_use]
    pub fn native_state(&self) -> Option<&ProviderNativeState> {
        self.native_state.as_ref()
    }
    #[must_use]
    pub fn usage(&self) -> Option<&ProviderUsage> {
        self.usage.as_ref()
    }
    #[must_use]
    pub fn finished_at(&self) -> ProviderInstant {
        self.finished_at
    }
}
impl<'de> Deserialize<'de> for ProviderCompletion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            response_id: Option<String>,
            continuation: Option<ProviderContinuation>,
            native_state: Option<ProviderNativeState>,
            usage: Option<ProviderUsage>,
            finished_at: ProviderInstant,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut value = Self::new(
            raw.response_id,
            raw.continuation,
            raw.usage,
            raw.finished_at,
        )
        .map_err(serde::de::Error::custom)?;
        if let Some(native_state) = raw.native_state {
            value = value
                .with_native_state(native_state)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureCode {
    InvalidRequest,
    ProviderUnsupported,
    AccountUnavailable,
    AuthenticationFailed,
    PermissionDenied,
    ModelUnavailable,
    CapabilityMismatch,
    RateLimited,
    BillingUnavailable,
    TransportFailed,
    ResponseTooLarge,
    MalformedResponse,
    ConsumerBackpressureExceeded,
    TimedOut,
    Cancelled,
    ServerFailed,
    CredentialRecoveryRequired,
    InternalInvariant,
}
impl ProviderFailureCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ProviderUnsupported => "provider_unsupported",
            Self::AccountUnavailable => "account_unavailable",
            Self::AuthenticationFailed => "authentication_failed",
            Self::PermissionDenied => "permission_denied",
            Self::ModelUnavailable => "model_unavailable",
            Self::CapabilityMismatch => "capability_mismatch",
            Self::RateLimited => "rate_limited",
            Self::BillingUnavailable => "billing_unavailable",
            Self::TransportFailed => "transport_failed",
            Self::ResponseTooLarge => "response_too_large",
            Self::MalformedResponse => "malformed_response",
            Self::ConsumerBackpressureExceeded => "consumer_backpressure_exceeded",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::ServerFailed => "server_failed",
            Self::CredentialRecoveryRequired => "credential_recovery_required",
            Self::InternalInvariant => "internal_invariant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize)]
#[error("{message}")]
pub struct ProviderFailure {
    code: ProviderFailureCode,
    message: String,
    provider_status_code: Option<u16>,
    retry_after_milliseconds: Option<u64>,
    request_id: Option<ProviderRequestId>,
}
impl ProviderFailure {
    #[must_use]
    pub fn new(code: ProviderFailureCode, message: impl AsRef<str>) -> Self {
        let clean = redact_failure(message.as_ref());
        let message = if clean.is_empty() {
            code.as_str().to_owned()
        } else {
            clean.chars().take(1_024).collect()
        };
        Self {
            code,
            message,
            provider_status_code: None,
            retry_after_milliseconds: None,
            request_id: None,
        }
    }
    pub fn with_status(mut self, status: u16) -> Result<Self, ProviderCoreError> {
        if !(100..=599).contains(&status) {
            return Err(ProviderCoreError::invalid_value(
                "provider status code is invalid",
            ));
        }
        self.provider_status_code = Some(status);
        Ok(self)
    }
    #[must_use]
    pub fn with_retry_after(mut self, milliseconds: u64) -> Self {
        self.retry_after_milliseconds = Some(milliseconds);
        self
    }
    #[must_use]
    pub fn with_request_id(mut self, request_id: ProviderRequestId) -> Self {
        self.request_id = Some(request_id);
        self
    }
    #[must_use]
    pub fn code(&self) -> ProviderFailureCode {
        self.code
    }
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
    #[must_use]
    pub fn provider_status_code(&self) -> Option<u16> {
        self.provider_status_code
    }
    #[must_use]
    pub fn retry_after_milliseconds(&self) -> Option<u64> {
        self.retry_after_milliseconds
    }
    #[must_use]
    pub fn request_id(&self) -> Option<&ProviderRequestId> {
        self.request_id.as_ref()
    }
}
impl<'de> Deserialize<'de> for ProviderFailure {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            code: ProviderFailureCode,
            message: String,
            provider_status_code: Option<u16>,
            retry_after_milliseconds: Option<u64>,
            request_id: Option<ProviderRequestId>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut value = Self::new(raw.code, raw.message);
        if let Some(status) = raw.provider_status_code {
            value = value
                .with_status(status)
                .map_err(serde::de::Error::custom)?;
        }
        value.retry_after_milliseconds = raw.retry_after_milliseconds;
        value.request_id = raw.request_id;
        Ok(value)
    }
}

fn redact_failure(input: &str) -> String {
    static BEARER: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static SECRET: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    let bearer = BEARER.get_or_init(|| Regex::new(r"(?i)bearer\s+[a-z0-9._~+/=-]+"));
    let secret = SECRET.get_or_init(|| {
        Regex::new(r"(?i)(api[_ -]?key|access[_ -]?token|refresh[_ -]?token)\s*[:=]\s*[^\s,;]+")
    });
    let (Ok(bearer), Ok(secret)) = (bearer, secret) else {
        return "provider failure details were redacted".to_owned();
    };
    let output = bearer.replace_all(input, "Bearer <redacted>");
    secret.replace_all(&output, "$1=<redacted>").into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ProviderTerminal {
    Completed(ProviderCompletion),
    Cancelled,
    Failed(ProviderFailure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ProviderTurnEvent {
    Started(ProviderResponseMetadata),
    /// A provider-exposed reasoning summary or reasoning-text fragment.
    /// Consumers must treat this as display-only: the durable native state
    /// remains in `ProviderCompletion`.
    ReasoningDelta(String),
    TextDelta(String),
    ToolCall(ProviderToolCall),
    Terminal(ProviderTerminal),
}
impl ProviderTurnEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }
}
