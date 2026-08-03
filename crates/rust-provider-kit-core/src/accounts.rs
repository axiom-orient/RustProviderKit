use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    ProviderAccountId, ProviderCapabilities, ProviderCoreError, ProviderCredentialReference,
    ProviderFailure, ProviderId, ProviderInstant, is_control, validate_trimmed_text,
};

#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SensitiveValue(String);
impl SensitiveValue {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderCoreError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64 * 1_024
            || value.trim() != value
            || is_control(&value)
        {
            return Err(ProviderCoreError::invalid_value(
                "credential value is invalid",
            ));
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SensitiveValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveValue(<redacted>)")
    }
}
impl fmt::Display for SensitiveValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCredentialSource {
    ApiKey,
    OauthDerivedKey,
    ExternalAuthFileReference,
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderCredentialMaterial {
    ApiKey(SensitiveValue),
    OauthDerivedKey(SensitiveValue),
    ExternalAuthFile(PathBuf),
}
impl fmt::Debug for ProviderCredentialMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ApiKey(<redacted>)"),
            Self::OauthDerivedKey(_) => formatter.write_str("OauthDerivedKey(<redacted>)"),
            Self::ExternalAuthFile(_) => formatter.write_str("ExternalAuthFile(<redacted>)"),
        }
    }
}
impl ProviderCredentialMaterial {
    pub fn api_key(value: impl Into<String>) -> Result<Self, ProviderCoreError> {
        SensitiveValue::new(value).map(Self::ApiKey)
    }
    pub fn oauth_derived_key(value: impl Into<String>) -> Result<Self, ProviderCoreError> {
        SensitiveValue::new(value).map(Self::OauthDerivedKey)
    }
    pub fn external_auth_file(path: impl Into<PathBuf>) -> Result<Self, ProviderCoreError> {
        let path = path.into();
        let text = path
            .to_str()
            .ok_or_else(|| ProviderCoreError::invalid_value("external auth path is not UTF-8"))?;
        if !path.is_absolute() || text.len() > 4_096 || is_control(text) {
            return Err(ProviderCoreError::invalid_value(
                "external auth path is invalid",
            ));
        }
        Ok(Self::ExternalAuthFile(path))
    }
    #[must_use]
    pub fn source(&self) -> ProviderCredentialSource {
        match self {
            Self::ApiKey(_) => ProviderCredentialSource::ApiKey,
            Self::OauthDerivedKey(_) => ProviderCredentialSource::OauthDerivedKey,
            Self::ExternalAuthFile(_) => ProviderCredentialSource::ExternalAuthFileReference,
        }
    }
    #[must_use]
    pub fn secret(&self) -> Option<&SensitiveValue> {
        match self {
            Self::ApiKey(value) | Self::OauthDerivedKey(value) => Some(value),
            Self::ExternalAuthFile(_) => None,
        }
    }
    #[must_use]
    pub fn external_path(&self) -> Option<&Path> {
        match self {
            Self::ExternalAuthFile(path) => Some(path),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderEndpointConfiguration {
    base_url: Url,
    headers: BTreeMap<String, String>,
}
impl ProviderEndpointConfiguration {
    pub fn new(base_url: Url) -> Result<Self, ProviderCoreError> {
        Self::with_headers(base_url, BTreeMap::new())
    }
    pub fn with_headers(
        base_url: Url,
        headers: BTreeMap<String, String>,
    ) -> Result<Self, ProviderCoreError> {
        if base_url.scheme() != "https"
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.fragment().is_some()
            || base_url.query().is_some()
        {
            return Err(ProviderCoreError::invalid_value(
                "provider endpoint must be an absolute HTTPS URL without user information, query, or fragment",
            ));
        }
        validate_request_headers(&headers)?;
        Ok(Self { base_url, headers })
    }
    pub fn parse(value: &str) -> Result<Self, ProviderCoreError> {
        Self::parse_with_headers(value, BTreeMap::new())
    }
    pub fn parse_with_headers(
        value: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<Self, ProviderCoreError> {
        let url = Url::parse(value)
            .map_err(|_| ProviderCoreError::invalid_value("provider endpoint URL is invalid"))?;
        Self::with_headers(url, headers)
    }
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }
    /// Account-scoped, non-sensitive headers for provider-defined integration
    /// metadata. Authentication and transport-critical headers are forbidden.
    #[must_use]
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }
}
impl<'de> Deserialize<'de> for ProviderEndpointConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            base_url: Url,
            #[serde(default)]
            headers: BTreeMap<String, String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::with_headers(raw.base_url, raw.headers).map_err(serde::de::Error::custom)
    }
}

fn validate_request_headers(headers: &BTreeMap<String, String>) -> Result<(), ProviderCoreError> {
    const MAXIMUM_HEADERS: usize = 16;
    const MAXIMUM_NAME_BYTES: usize = 128;
    const MAXIMUM_VALUE_BYTES: usize = 4 * 1024;
    if headers.len() > MAXIMUM_HEADERS {
        return Err(ProviderCoreError::invalid_value(
            "provider endpoint has too many custom headers",
        ));
    }
    for (name, value) in headers {
        let normalized = name.to_ascii_lowercase();
        let valid_name = !name.is_empty()
            && name.len() <= MAXIMUM_NAME_BYTES
            && name.as_bytes().iter().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            });
        if !valid_name {
            return Err(ProviderCoreError::invalid_value(
                "provider endpoint custom header name is invalid",
            ));
        }
        let sensitive = ["auth", "token", "secret", "credential", "cookie", "api-key"]
            .iter()
            .any(|fragment| normalized.contains(fragment));
        if sensitive
            || matches!(
                normalized.as_str(),
                "accept"
                    | "connection"
                    | "content-length"
                    | "content-type"
                    | "host"
                    | "transfer-encoding"
            )
        {
            return Err(ProviderCoreError::invalid_value(
                "provider endpoint custom header cannot override authentication or transport",
            ));
        }
        if value.is_empty()
            || value.len() > MAXIMUM_VALUE_BYTES
            || value.trim() != value
            || is_control(value)
        {
            return Err(ProviderCoreError::invalid_value(
                "provider endpoint custom header value is invalid",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountRegistrationRequest {
    account_id: ProviderAccountId,
    provider_id: ProviderId,
    label: String,
    credential: ProviderCredentialMaterial,
    endpoint: Option<ProviderEndpointConfiguration>,
}
impl ProviderAccountRegistrationRequest {
    pub fn new(
        account_id: ProviderAccountId,
        provider_id: ProviderId,
        label: impl Into<String>,
        credential: ProviderCredentialMaterial,
        endpoint: Option<ProviderEndpointConfiguration>,
    ) -> Result<Self, ProviderCoreError> {
        let label = label.into();
        validate_trimmed_text(&label, 128, "provider account label")?;
        Ok(Self {
            account_id,
            provider_id,
            label,
            credential,
            endpoint,
        })
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
    #[must_use]
    pub fn credential(&self) -> &ProviderCredentialMaterial {
        &self.credential
    }
    #[must_use]
    pub fn endpoint(&self) -> Option<&ProviderEndpointConfiguration> {
        self.endpoint.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCredentialRecordState {
    Staged,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCredentialRecord {
    reference: ProviderCredentialReference,
    account_id: ProviderAccountId,
    provider_id: ProviderId,
    label: String,
    source: ProviderCredentialSource,
    state: ProviderCredentialRecordState,
    endpoint: Option<ProviderEndpointConfiguration>,
    created_at: ProviderInstant,
    updated_at: ProviderInstant,
}
impl ProviderCredentialRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reference: ProviderCredentialReference,
        account_id: ProviderAccountId,
        provider_id: ProviderId,
        label: impl Into<String>,
        source: ProviderCredentialSource,
        state: ProviderCredentialRecordState,
        endpoint: Option<ProviderEndpointConfiguration>,
        created_at: ProviderInstant,
        updated_at: ProviderInstant,
    ) -> Result<Self, ProviderCoreError> {
        let label = label.into();
        validate_trimmed_text(&label, 128, "provider account label")?;
        if updated_at < created_at {
            return Err(ProviderCoreError::invalid_value(
                "credential record update precedes creation",
            ));
        }
        Ok(Self {
            reference,
            account_id,
            provider_id,
            label,
            source,
            state,
            endpoint,
            created_at,
            updated_at,
        })
    }
    #[must_use]
    pub fn reference(&self) -> &ProviderCredentialReference {
        &self.reference
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
    #[must_use]
    pub fn source(&self) -> ProviderCredentialSource {
        self.source
    }
    #[must_use]
    pub fn state(&self) -> ProviderCredentialRecordState {
        self.state
    }
    #[must_use]
    pub fn endpoint(&self) -> Option<&ProviderEndpointConfiguration> {
        self.endpoint.as_ref()
    }
    #[must_use]
    pub fn created_at(&self) -> ProviderInstant {
        self.created_at
    }
    #[must_use]
    pub fn updated_at(&self) -> ProviderInstant {
        self.updated_at
    }
}
impl<'de> Deserialize<'de> for ProviderCredentialRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            reference: ProviderCredentialReference,
            account_id: ProviderAccountId,
            provider_id: ProviderId,
            label: String,
            source: ProviderCredentialSource,
            state: ProviderCredentialRecordState,
            endpoint: Option<ProviderEndpointConfiguration>,
            created_at: ProviderInstant,
            updated_at: ProviderInstant,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.reference,
            raw.account_id,
            raw.provider_id,
            raw.label,
            raw.source,
            raw.state,
            raw.endpoint,
            raw.created_at,
            raw.updated_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCredentialLease {
    record: ProviderCredentialRecord,
    material: ProviderCredentialMaterial,
}
impl ProviderCredentialLease {
    pub fn new(
        record: ProviderCredentialRecord,
        material: ProviderCredentialMaterial,
    ) -> Result<Self, ProviderCoreError> {
        Self::validate(&record, &material, ProviderCredentialRecordState::Active)?;
        Ok(Self { record, material })
    }

    /// Creates the bounded lease used only while verifying a staged credential.
    /// Runtime code must not expose this lease as an active account lease.
    pub fn for_verification(
        record: ProviderCredentialRecord,
        material: ProviderCredentialMaterial,
    ) -> Result<Self, ProviderCoreError> {
        Self::validate(&record, &material, ProviderCredentialRecordState::Staged)?;
        Ok(Self { record, material })
    }

    fn validate(
        record: &ProviderCredentialRecord,
        material: &ProviderCredentialMaterial,
        expected_state: ProviderCredentialRecordState,
    ) -> Result<(), ProviderCoreError> {
        if record.source() != material.source() || record.state() != expected_state {
            return Err(ProviderCoreError::invalid_value(
                "credential lease does not match its record",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn record(&self) -> &ProviderCredentialRecord {
        &self.record
    }
    #[must_use]
    pub fn material(&self) -> &ProviderCredentialMaterial {
        &self.material
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAccountReadiness {
    Ready,
    VerificationRequired,
    Unconfigured,
    Unavailable,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderAccountInspection {
    account_id: ProviderAccountId,
    provider_id: ProviderId,
    readiness: ProviderAccountReadiness,
    message: Option<String>,
    capabilities: ProviderCapabilities,
    inspected_at: ProviderInstant,
}
impl ProviderAccountInspection {
    pub fn new(
        account_id: ProviderAccountId,
        provider_id: ProviderId,
        readiness: ProviderAccountReadiness,
        message: Option<String>,
        capabilities: ProviderCapabilities,
        inspected_at: ProviderInstant,
    ) -> Result<Self, ProviderCoreError> {
        if let Some(value) = &message
            && (value.is_empty() || value.len() > 1_024 || is_control(value))
        {
            return Err(ProviderCoreError::invalid_value(
                "account inspection message is invalid",
            ));
        }
        Ok(Self {
            account_id,
            provider_id,
            readiness,
            message,
            capabilities,
            inspected_at,
        })
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn readiness(&self) -> ProviderAccountReadiness {
        self.readiness
    }
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
    #[must_use]
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    #[must_use]
    pub fn inspected_at(&self) -> ProviderInstant {
        self.inspected_at
    }
}
impl<'de> Deserialize<'de> for ProviderAccountInspection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            account_id: ProviderAccountId,
            provider_id: ProviderId,
            readiness: ProviderAccountReadiness,
            message: Option<String>,
            capabilities: ProviderCapabilities,
            inspected_at: ProviderInstant,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.account_id,
            raw.provider_id,
            raw.readiness,
            raw.message,
            raw.capabilities,
            raw.inspected_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderAccountSummary {
    account_id: ProviderAccountId,
    provider_id: ProviderId,
    label: String,
    credential_source: ProviderCredentialSource,
    readiness: ProviderAccountReadiness,
    endpoint: Option<ProviderEndpointConfiguration>,
    last_inspected_at: Option<ProviderInstant>,
}
impl ProviderAccountSummary {
    pub fn new(
        account_id: ProviderAccountId,
        provider_id: ProviderId,
        label: impl Into<String>,
        credential_source: ProviderCredentialSource,
        readiness: ProviderAccountReadiness,
        endpoint: Option<ProviderEndpointConfiguration>,
        last_inspected_at: Option<ProviderInstant>,
    ) -> Result<Self, ProviderCoreError> {
        let label = label.into();
        validate_trimmed_text(&label, 128, "provider account label")?;
        Ok(Self {
            account_id,
            provider_id,
            label,
            credential_source,
            readiness,
            endpoint,
            last_inspected_at,
        })
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
    #[must_use]
    pub fn credential_source(&self) -> ProviderCredentialSource {
        self.credential_source
    }
    #[must_use]
    pub fn readiness(&self) -> ProviderAccountReadiness {
        self.readiness
    }
    #[must_use]
    pub fn endpoint(&self) -> Option<&ProviderEndpointConfiguration> {
        self.endpoint.as_ref()
    }
    #[must_use]
    pub fn last_inspected_at(&self) -> Option<ProviderInstant> {
        self.last_inspected_at
    }
}
impl<'de> Deserialize<'de> for ProviderAccountSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            account_id: ProviderAccountId,
            provider_id: ProviderId,
            label: String,
            credential_source: ProviderCredentialSource,
            readiness: ProviderAccountReadiness,
            endpoint: Option<ProviderEndpointConfiguration>,
            last_inspected_at: Option<ProviderInstant>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.account_id,
            raw.provider_id,
            raw.label,
            raw.credential_source,
            raw.readiness,
            raw.endpoint,
            raw.last_inspected_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialReconciliationIssue {
    pub reference: ProviderCredentialReference,
    pub account_id: ProviderAccountId,
    pub failure: ProviderFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialReconciliationReport {
    active_record_count: usize,
    removed_staged_references: Vec<ProviderCredentialReference>,
    issues: Vec<ProviderCredentialReconciliationIssue>,
}
impl ProviderCredentialReconciliationReport {
    #[must_use]
    pub fn new(
        active_record_count: usize,
        mut removed_staged_references: Vec<ProviderCredentialReference>,
        mut issues: Vec<ProviderCredentialReconciliationIssue>,
    ) -> Self {
        removed_staged_references.sort();
        issues.sort_by(|left, right| {
            left.account_id
                .cmp(&right.account_id)
                .then_with(|| left.reference.cmp(&right.reference))
        });
        Self {
            active_record_count,
            removed_staged_references,
            issues,
        }
    }
    #[must_use]
    pub fn active_record_count(&self) -> usize {
        self.active_record_count
    }
    #[must_use]
    pub fn removed_staged_references(&self) -> &[ProviderCredentialReference] {
        &self.removed_staged_references
    }
    #[must_use]
    pub fn issues(&self) -> &[ProviderCredentialReconciliationIssue] {
        &self.issues
    }
    #[must_use]
    pub fn requires_recovery(&self) -> bool {
        !self.issues.is_empty()
    }
}

#[async_trait]
pub trait ProviderCredentialStore: Send + Sync {
    async fn stage(
        &self,
        request: &ProviderAccountRegistrationRequest,
        at: ProviderInstant,
    ) -> Result<ProviderCredentialRecord, ProviderFailure>;
    async fn activate(
        &self,
        record: &ProviderCredentialRecord,
        at: ProviderInstant,
    ) -> Result<(), ProviderFailure>;
    async fn remove(&self, record: &ProviderCredentialRecord) -> Result<(), ProviderFailure>;
    async fn record(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderCredentialRecord>, ProviderFailure>;
    async fn lease(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderCredentialLease, ProviderFailure>;
    async fn records(&self) -> Result<Vec<ProviderCredentialRecord>, ProviderFailure>;
}

#[derive(Clone)]
pub struct ProviderAuthorizationRequest {
    provider_id: ProviderId,
    authorization_url: Url,
    callback_scheme: String,
    state: String,
}
impl ProviderAuthorizationRequest {
    pub fn new(
        provider_id: ProviderId,
        authorization_url: Url,
        callback_scheme: impl Into<String>,
        state: impl Into<String>,
    ) -> Result<Self, ProviderCoreError> {
        let callback_scheme = callback_scheme.into();
        let state = state.into();
        let mut bytes = callback_scheme.bytes();
        let first = bytes.next();
        let scheme_ok = first.is_some_and(|byte| byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
        if authorization_url.scheme() != "https"
            || authorization_url.host_str().is_none()
            || !authorization_url.username().is_empty()
            || authorization_url.password().is_some()
            || authorization_url.fragment().is_some()
            || callback_scheme.is_empty()
            || callback_scheme.len() > 128
            || !scheme_ok
            || state.is_empty()
            || state.len() > 512
            || is_control(&state)
        {
            return Err(ProviderCoreError::invalid_value(
                "authorization request is invalid",
            ));
        }
        Ok(Self {
            provider_id,
            authorization_url,
            callback_scheme,
            state,
        })
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }
    #[must_use]
    pub fn callback_scheme(&self) -> &str {
        &self.callback_scheme
    }
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }
}

impl std::fmt::Debug for ProviderAuthorizationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut authorization_url = self.authorization_url.clone();
        authorization_url.set_query(None);
        authorization_url.set_fragment(None);
        formatter
            .debug_struct("ProviderAuthorizationRequest")
            .field("provider_id", &self.provider_id)
            .field("authorization_url", &authorization_url)
            .field("callback_scheme", &self.callback_scheme)
            .field("state", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthorizationResult {
    callback_url: Url,
}
impl ProviderAuthorizationResult {
    #[must_use]
    pub fn new(callback_url: Url) -> Self {
        Self { callback_url }
    }
    #[must_use]
    pub fn callback_url(&self) -> &Url {
        &self.callback_url
    }
}

impl std::fmt::Debug for ProviderAuthorizationResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut callback_url = self.callback_url.clone();
        callback_url.set_query(None);
        callback_url.set_fragment(None);
        formatter
            .debug_struct("ProviderAuthorizationResult")
            .field("callback_url", &callback_url)
            .finish()
    }
}

#[async_trait]
pub trait ProviderAuthorizationSession: Send + Sync {
    async fn authorize(
        &self,
        request: ProviderAuthorizationRequest,
    ) -> Result<ProviderAuthorizationResult, ProviderFailure>;
    fn cancel(&self);
}

#[async_trait]
pub trait ProviderClock: Send + Sync {
    async fn now(&self) -> Result<ProviderInstant, ProviderFailure>;
    async fn sleep(&self, milliseconds: u64) -> Result<(), ProviderFailure>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProviderClock;
#[async_trait]
impl ProviderClock for SystemProviderClock {
    async fn now(&self) -> Result<ProviderInstant, ProviderFailure> {
        ProviderInstant::from_system_time(SystemTime::now()).map_err(|_| {
            ProviderFailure::new(
                crate::ProviderFailureCode::InternalInvariant,
                "system clock is outside the supported provider timestamp range",
            )
        })
    }
    async fn sleep(&self, milliseconds: u64) -> Result<(), ProviderFailure> {
        milliseconds.checked_mul(1_000_000).ok_or_else(|| {
            ProviderFailure::new(
                crate::ProviderFailureCode::InvalidRequest,
                "sleep duration overflow",
            )
        })?;
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
        Ok(())
    }
}
