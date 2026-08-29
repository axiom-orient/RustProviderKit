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

const MAX_FAILURE_EVIDENCE_TOKEN_BYTES: usize = 128;

/// Bounded numeric provider limit information.  The values are deliberately
/// numbers only: the diagnostic surface must not carry arbitrary provider
/// strings or a response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRateLimitEvidence {
    used: Option<u64>,
    window: Option<u64>,
    reset: Option<u64>,
    credits: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_percent_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_minutes: Option<u64>,
}

impl ProviderRateLimitEvidence {
    pub fn new(
        used: Option<u64>,
        window: Option<u64>,
        reset: Option<u64>,
        credits: Option<u64>,
    ) -> Result<Self, ProviderCoreError> {
        const SAFE_MAX: u64 = i64::MAX as u64;
        if [used, window, reset, credits]
            .into_iter()
            .flatten()
            .any(|value| value > SAFE_MAX)
        {
            return Err(ProviderCoreError::invalid_value(
                "provider rate-limit evidence exceeds the source contract range",
            ));
        }
        Ok(Self {
            used,
            window,
            reset,
            credits,
            used_percent_millis: None,
            window_minutes: None,
        })
    }
    pub fn with_used_percent_millis(mut self, value: u64) -> Result<Self, ProviderCoreError> {
        if value > 100_000 {
            return Err(ProviderCoreError::invalid_value(
                "provider used-percent evidence exceeds 100 percent",
            ));
        }
        self.used_percent_millis = Some(value);
        Ok(self)
    }
    pub fn with_window_minutes(mut self, value: u64) -> Result<Self, ProviderCoreError> {
        if value > i64::MAX as u64 {
            return Err(ProviderCoreError::invalid_value(
                "provider rate-limit window exceeds the source contract range",
            ));
        }
        self.window_minutes = Some(value);
        Ok(self)
    }
    #[must_use]
    pub fn used(&self) -> Option<u64> {
        self.used
    }
    #[must_use]
    pub fn window(&self) -> Option<u64> {
        self.window
    }
    #[must_use]
    pub fn reset(&self) -> Option<u64> {
        self.reset
    }
    #[must_use]
    pub fn credits(&self) -> Option<u64> {
        self.credits
    }
    #[must_use]
    pub fn used_percent_millis(&self) -> Option<u64> {
        self.used_percent_millis
    }
    #[must_use]
    pub fn window_minutes(&self) -> Option<u64> {
        self.window_minutes
    }
}

impl<'de> Deserialize<'de> for ProviderRateLimitEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            used: Option<u64>,
            window: Option<u64>,
            reset: Option<u64>,
            credits: Option<u64>,
            #[serde(default)]
            used_percent_millis: Option<u64>,
            #[serde(default)]
            window_minutes: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut value = Self::new(raw.used, raw.window, raw.reset, raw.credits)
            .map_err(serde::de::Error::custom)?;
        if let Some(used) = raw.used_percent_millis {
            value = value
                .with_used_percent_millis(used)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(window) = raw.window_minutes {
            value = value
                .with_window_minutes(window)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(value)
    }
}

/// Safe, bounded evidence extracted from a provider HTTP failure.  It is
/// intentionally optional so older serialized `ProviderFailure` values remain
/// valid and retain their previous wire shape when no evidence is available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderFailureEvidence {
    body_bytes: Option<u64>,
    body_sha256: Option<String>,
    upstream_request_id: Option<String>,
    remote_error_type: Option<String>,
    remote_error_code: Option<String>,
    codex_primary: Option<ProviderRateLimitEvidence>,
    codex_secondary: Option<ProviderRateLimitEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reset_at_unix_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_should_retry: Option<bool>,
}

impl ProviderFailureEvidence {
    pub fn new(body_bytes: u64, body_sha256: impl AsRef<str>) -> Result<Self, ProviderCoreError> {
        let body_sha256 = body_sha256.as_ref();
        if body_sha256.len() != 64
            || !body_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ProviderCoreError::invalid_value(
                "provider failure body digest is invalid",
            ));
        }
        Ok(Self {
            body_bytes: Some(body_bytes),
            body_sha256: Some(body_sha256.to_owned()),
            upstream_request_id: None,
            remote_error_type: None,
            remote_error_code: None,
            codex_primary: None,
            codex_secondary: None,
            reset_at_unix_seconds: None,
            provider_should_retry: None,
        })
    }
    pub fn with_upstream_request_id(
        mut self,
        value: impl AsRef<str>,
    ) -> Result<Self, ProviderCoreError> {
        self.upstream_request_id = Some(valid_failure_request_id(value.as_ref())?);
        Ok(self)
    }
    pub fn with_remote_error_type(
        mut self,
        value: impl AsRef<str>,
    ) -> Result<Self, ProviderCoreError> {
        self.remote_error_type = Some(valid_failure_evidence_token(
            value.as_ref(),
            "provider remote error type",
        )?);
        Ok(self)
    }
    pub fn with_remote_error_code(
        mut self,
        value: impl AsRef<str>,
    ) -> Result<Self, ProviderCoreError> {
        self.remote_error_code = Some(valid_failure_evidence_token(
            value.as_ref(),
            "provider remote error code",
        )?);
        Ok(self)
    }
    #[must_use]
    pub fn with_codex_primary(mut self, value: ProviderRateLimitEvidence) -> Self {
        self.codex_primary = Some(value);
        self
    }
    #[must_use]
    pub fn with_codex_secondary(mut self, value: ProviderRateLimitEvidence) -> Self {
        self.codex_secondary = Some(value);
        self
    }
    pub fn with_reset_at_unix_seconds(mut self, value: u64) -> Result<Self, ProviderCoreError> {
        if value > i64::MAX as u64 {
            return Err(ProviderCoreError::invalid_value(
                "provider reset timestamp exceeds the source contract range",
            ));
        }
        self.reset_at_unix_seconds = Some(value);
        Ok(self)
    }
    #[must_use]
    pub fn with_provider_should_retry(mut self, value: bool) -> Self {
        self.provider_should_retry = Some(value);
        self
    }
    #[must_use]
    pub fn body_bytes(&self) -> Option<u64> {
        self.body_bytes
    }
    #[must_use]
    pub fn body_sha256(&self) -> Option<&str> {
        self.body_sha256.as_deref()
    }
    #[must_use]
    pub fn upstream_request_id(&self) -> Option<&str> {
        self.upstream_request_id.as_deref()
    }
    #[must_use]
    pub fn remote_error_type(&self) -> Option<&str> {
        self.remote_error_type.as_deref()
    }
    #[must_use]
    pub fn remote_error_code(&self) -> Option<&str> {
        self.remote_error_code.as_deref()
    }
    #[must_use]
    pub fn codex_primary(&self) -> Option<&ProviderRateLimitEvidence> {
        self.codex_primary.as_ref()
    }
    #[must_use]
    pub fn codex_secondary(&self) -> Option<&ProviderRateLimitEvidence> {
        self.codex_secondary.as_ref()
    }
    #[must_use]
    pub fn reset_at_unix_seconds(&self) -> Option<u64> {
        self.reset_at_unix_seconds
    }
    #[must_use]
    pub fn provider_should_retry(&self) -> Option<bool> {
        self.provider_should_retry
    }
}

impl<'de> Deserialize<'de> for ProviderFailureEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            body_bytes: Option<u64>,
            body_sha256: Option<String>,
            upstream_request_id: Option<String>,
            remote_error_type: Option<String>,
            remote_error_code: Option<String>,
            codex_primary: Option<ProviderRateLimitEvidence>,
            codex_secondary: Option<ProviderRateLimitEvidence>,
            #[serde(default)]
            reset_at_unix_seconds: Option<u64>,
            #[serde(default)]
            provider_should_retry: Option<bool>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut value = match (raw.body_bytes, raw.body_sha256) {
            (Some(bytes), Some(digest)) => Self::new(bytes, digest),
            (None, None) => Ok(Self {
                body_bytes: None,
                body_sha256: None,
                upstream_request_id: None,
                remote_error_type: None,
                remote_error_code: None,
                codex_primary: None,
                codex_secondary: None,
                reset_at_unix_seconds: None,
                provider_should_retry: None,
            }),
            _ => Err(ProviderCoreError::invalid_value(
                "provider failure body evidence is incomplete",
            )),
        }
        .map_err(serde::de::Error::custom)?;
        if let Some(request_id) = raw.upstream_request_id {
            value = value
                .with_upstream_request_id(request_id)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(error_type) = raw.remote_error_type {
            value = value
                .with_remote_error_type(error_type)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(error_code) = raw.remote_error_code {
            value = value
                .with_remote_error_code(error_code)
                .map_err(serde::de::Error::custom)?;
        }
        value.codex_primary = raw.codex_primary;
        value.codex_secondary = raw.codex_secondary;
        if let Some(reset) = raw.reset_at_unix_seconds {
            value = value
                .with_reset_at_unix_seconds(reset)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(should_retry) = raw.provider_should_retry {
            value = value.with_provider_should_retry(should_retry);
        }
        Ok(value)
    }
}

fn valid_failure_evidence_token(
    value: &str,
    field: &'static str,
) -> Result<String, ProviderCoreError> {
    if value.is_empty()
        || value.len() > MAX_FAILURE_EVIDENCE_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(ProviderCoreError::invalid_value(field));
    }
    Ok(value.to_owned())
}

fn valid_failure_request_id(value: &str) -> Result<String, ProviderCoreError> {
    let original = value;
    let value = value.trim();
    if original != value
        || value.is_empty()
        || value.len() > MAX_FAILURE_EVIDENCE_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(ProviderCoreError::invalid_value(
            "provider upstream request ID",
        ));
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("bearer")
        || is_jwt_like_request_id(value)
        || !(lower.starts_with("req_")
            || lower.starts_with("req-")
            || lower.starts_with("request")
            || lower.starts_with("upstream-req"))
    {
        return Err(ProviderCoreError::invalid_value(
            "provider upstream request ID",
        ));
    }
    Ok(value.to_owned())
}

fn is_jwt_like_request_id(value: &str) -> bool {
    let mut segments = value.split('.');
    let (Some(first), Some(second), Some(third), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    !first.is_empty()
        && !second.is_empty()
        && !third.is_empty()
        && [first, second, third].into_iter().all(|segment| {
            segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize)]
#[error("{message}")]
pub struct ProviderFailure {
    code: ProviderFailureCode,
    message: String,
    provider_status_code: Option<u16>,
    retry_after_milliseconds: Option<u64>,
    request_id: Option<ProviderRequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<Box<ProviderFailureEvidence>>,
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
            evidence: None,
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
    pub fn with_evidence(mut self, evidence: ProviderFailureEvidence) -> Self {
        self.evidence = Some(Box::new(evidence));
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
    #[must_use]
    pub fn evidence(&self) -> Option<&ProviderFailureEvidence> {
        self.evidence.as_deref()
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
            evidence: Option<ProviderFailureEvidence>,
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
        value.evidence = raw.evidence.map(Box::new);
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
