use std::collections::{BTreeMap, BTreeSet};

use http::Method;
use rust_provider_kit_core::{
    CapabilitySupport, ProviderCapabilities, ProviderCompletion, ProviderContinuation,
    ProviderCoreError, ProviderCredentialLease, ProviderCredentialMaterial, ProviderFailure,
    ProviderFailureCode, ProviderFailureEvidence, ProviderInstant, ProviderJsonValue,
    ProviderModelCatalogResult, ProviderModelDescriptor, ProviderModelId, ProviderNativeState,
    ProviderRateLimitEvidence, ProviderRequestConstraints, ProviderToolCall, ProviderUsage,
};
use sha2::{Digest, Sha256};
use url::Url;

use crate::http_transport::{
    ProviderHttpRequest, ProviderHttpUnaryResponse, ProviderTransportError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCompletionDraft {
    pub(crate) response_id: Option<String>,
    pub(crate) continuation: Option<ProviderContinuation>,
    pub(crate) native_state: Option<ProviderNativeState>,
    pub(crate) usage: Option<ProviderUsage>,
}
impl ProviderCompletionDraft {
    pub(crate) fn materialize(
        self,
        at: ProviderInstant,
    ) -> Result<ProviderCompletion, ProviderFailure> {
        let completion =
            ProviderCompletion::new(self.response_id, self.continuation, self.usage, at)
                .map_err(core_error_failure)?;
        match self.native_state {
            Some(native_state) => completion
                .with_native_state(native_state)
                .map_err(core_error_failure),
            None => Ok(completion),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderToolArgumentAccumulator {
    maximum_bytes: usize,
    bytes: Vec<u8>,
}
impl Default for ProviderToolArgumentAccumulator {
    fn default() -> Self {
        Self::with_default_bound()
    }
}

impl ProviderToolArgumentAccumulator {
    #[cfg(test)]
    pub(crate) fn new(maximum_bytes: usize) -> Result<Self, ProviderFailure> {
        if maximum_bytes == 0 {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "tool argument byte limit must be positive",
            ));
        }
        Ok(Self::bounded(maximum_bytes))
    }
    #[must_use]
    pub(crate) fn with_default_bound() -> Self {
        Self::bounded(ProviderJsonValue::MAXIMUM_ENCODED_BYTES)
    }
    #[must_use]
    pub(crate) fn byte_count(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn append(&mut self, fragment: &str) -> Result<(), ProviderFailure> {
        self.write(fragment.as_bytes(), false)
    }
    pub(crate) fn replace(&mut self, fragment: &str) -> Result<(), ProviderFailure> {
        self.write(fragment.as_bytes(), true)
    }
    pub(crate) fn decode_object(&self) -> Result<ProviderJsonValue, ProviderFailure> {
        let value = decode_json(&self.bytes, "tool arguments")?;
        if value.as_object().is_none() {
            return Err(malformed("tool arguments must be a JSON object"));
        }
        Ok(value)
    }
    fn write(&mut self, fragment: &[u8], replacing: bool) -> Result<(), ProviderFailure> {
        let existing = if replacing { 0 } else { self.bytes.len() };
        let next = existing.checked_add(fragment.len()).ok_or_else(|| {
            response_too_large("provider tool arguments exceeded their byte limit")
        })?;
        if next > self.maximum_bytes {
            return Err(response_too_large(
                "provider tool arguments exceeded their byte limit",
            ));
        }
        if replacing {
            self.bytes.clear();
        }
        self.bytes.extend_from_slice(fragment);
        Ok(())
    }
    fn bounded(maximum_bytes: usize) -> Self {
        Self {
            maximum_bytes,
            bytes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProviderDecodedEvent {
    Reasoning(String),
    Text(String),
    ToolCall(ProviderToolCall),
    Completion(ProviderCompletionDraft),
}

pub(crate) fn decode_json(data: &[u8], field: &str) -> Result<ProviderJsonValue, ProviderFailure> {
    ProviderJsonValue::decode(data).map_err(|_| malformed(format!("{field} is malformed JSON")))
}
pub(crate) fn json_to_serde(
    value: &ProviderJsonValue,
) -> Result<serde_json::Value, ProviderFailure> {
    serde_json::to_value(value).map_err(|_| malformed("provider JSON conversion failed"))
}

/// Core validates these account-scoped integration headers before an adapter
/// sees them. Authentication and transport-critical header names are rejected
/// there, so an adapter can merge this metadata after its own protocol headers.
pub(crate) fn merge_account_headers(
    mut headers: BTreeMap<String, String>,
    credential: &ProviderCredentialLease,
) -> BTreeMap<String, String> {
    if let Some(endpoint) = credential.record().endpoint() {
        headers.extend(endpoint.headers().clone());
    }
    headers
}

/// Tool results are text on every supported provider wire. Preserve a caller's
/// textual result as text; serialize structured values only when the caller
/// deliberately supplied structured JSON.
pub(crate) fn tool_result_text(value: &ProviderJsonValue) -> Result<String, ProviderFailure> {
    if let Some(text) = value.as_str() {
        return Ok(text.to_owned());
    }
    String::from_utf8(value.encoded_vec().map_err(core_error_failure)?).map_err(|_| {
        ProviderFailure::new(
            ProviderFailureCode::InternalInvariant,
            "encoded JSON is not UTF-8",
        )
    })
}
pub(crate) fn serde_to_json(
    value: serde_json::Value,
) -> Result<ProviderJsonValue, ProviderFailure> {
    ProviderJsonValue::try_from(value).map_err(core_error_failure)
}
pub(crate) fn object<'a>(
    value: &'a ProviderJsonValue,
    field: &str,
) -> Result<&'a BTreeMap<String, ProviderJsonValue>, ProviderFailure> {
    value
        .as_object()
        .ok_or_else(|| malformed(format!("{field} must be an object")))
}
pub(crate) fn array<'a>(
    value: &'a ProviderJsonValue,
    field: &str,
) -> Result<&'a [ProviderJsonValue], ProviderFailure> {
    value
        .as_array()
        .ok_or_else(|| malformed(format!("{field} must be an array")))
}
pub(crate) fn optional_nonnegative_u64(
    value: Option<&ProviderJsonValue>,
    field: &str,
) -> Result<Option<u64>, ProviderFailure> {
    // Swift's `Int(exactly:)` is the source contract. The supported Swift
    // target is 64-bit, and 2^63 itself is not representable even though its
    // `f64` representation is exact.
    const I64_MAX_EXCLUSIVE_AS_F64: f64 = 9_223_372_036_854_775_808.0;
    match value {
        None | Some(ProviderJsonValue::Null) => Ok(None),
        Some(ProviderJsonValue::Integer(number)) if *number >= 0 => Ok(Some(*number as u64)),
        Some(ProviderJsonValue::UnsignedInteger(number)) if *number <= i64::MAX as u64 => {
            Ok(Some(*number))
        }
        Some(ProviderJsonValue::Number(number))
            if number.is_finite()
                && *number >= 0.0
                && number.fract() == 0.0
                && *number < I64_MAX_EXCLUSIVE_AS_F64 =>
        {
            Ok(Some(*number as u64))
        }
        _ => Err(malformed(format!("{field} must be a nonnegative integer"))),
    }
}
pub(crate) fn optional_string<'a>(
    value: Option<&'a ProviderJsonValue>,
    field: &str,
) -> Result<Option<&'a str>, ProviderFailure> {
    match value {
        None | Some(ProviderJsonValue::Null) => Ok(None),
        Some(ProviderJsonValue::String(value)) => Ok(Some(value)),
        _ => Err(malformed(format!("{field} must be a string"))),
    }
}

pub(crate) fn require_api_key(lease: &ProviderCredentialLease) -> Result<&str, ProviderFailure> {
    match lease.material() {
        ProviderCredentialMaterial::ApiKey(value)
        | ProviderCredentialMaterial::OauthDerivedKey(value) => Ok(value.expose()),
        ProviderCredentialMaterial::ExternalAuthFile(_) => Err(ProviderFailure::new(
            ProviderFailureCode::AuthenticationFailed,
            "provider requires an API key credential",
        )),
    }
}

/// Server-side continuations depend on provider-retained response state. Keep
/// the default request privacy-preserving; callers must opt in explicitly.
pub(crate) fn stores_server_side_response(
    request: &rust_provider_kit_core::ProviderTurnRequest,
) -> bool {
    request.constraints().data_collection()
        == rust_provider_kit_core::ProviderDataCollectionPolicy::Allow
        && !request.constraints().requires_zero_data_retention()
}

pub(crate) fn require_server_side_continuation_opt_in(
    request: &rust_provider_kit_core::ProviderTurnRequest,
) -> Result<(), ProviderFailure> {
    if request.continuation().is_none() || stores_server_side_response(request) {
        return Ok(());
    }
    Err(ProviderFailure::new(
        ProviderFailureCode::CapabilityMismatch,
        "provider continuation requires explicit server-side retention and data-collection opt-in",
    ))
}

pub(crate) fn append_path(path: &str, base: &Url) -> Result<Url, ProviderFailure> {
    if !path.starts_with('/') || path.contains('?') || path.contains('#') {
        return Err(ProviderFailure::new(
            ProviderFailureCode::InvalidRequest,
            "provider path is invalid",
        ));
    }
    let mut url = base.clone();
    let base_path = url.path().trim_end_matches('/');
    let combined = format!("{base_path}{path}");
    url.set_path(&combined);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub(crate) fn make_json_request(
    method: Method,
    url: Url,
    mut headers: BTreeMap<String, String>,
    value: &ProviderJsonValue,
    constraints: &ProviderRequestConstraints,
) -> Result<ProviderHttpRequest, ProviderFailure> {
    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("accept"))
    {
        headers.insert("accept".to_owned(), "application/json".to_owned());
    }
    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("content-type"))
    {
        headers.insert("content-type".to_owned(), "application/json".to_owned());
    }
    let body = value.encoded_vec().map_err(core_error_failure)?;
    ProviderHttpRequest::with_timeout(
        method,
        url,
        headers,
        body,
        constraints.timeout_milliseconds(),
        constraints.maximum_response_bytes(),
    )
}

pub(crate) fn transport_failure(error: ProviderTransportError) -> ProviderFailure {
    match error {
        ProviderTransportError::ResponseTooLarge => ProviderFailure::new(
            ProviderFailureCode::ResponseTooLarge,
            "provider response exceeded its byte limit",
        ),
        ProviderTransportError::BackpressureExceeded => ProviderFailure::new(
            ProviderFailureCode::ConsumerBackpressureExceeded,
            "provider transport backlog exceeded",
        ),
        ProviderTransportError::TimedOut => ProviderFailure::new(
            ProviderFailureCode::TimedOut,
            "provider network request timed out",
        ),
        ProviderTransportError::InvalidResponse => ProviderFailure::new(
            ProviderFailureCode::MalformedResponse,
            "provider returned an invalid HTTP response",
        ),
        ProviderTransportError::Failed => ProviderFailure::new(
            ProviderFailureCode::TransportFailed,
            "provider transport failed",
        ),
    }
}

pub(crate) fn http_failure(
    response: &ProviderHttpUnaryResponse,
    now: ProviderInstant,
) -> ProviderFailure {
    http_failure_parts(response.status_code, &response.headers, &response.body, now)
}
pub(crate) fn http_failure_parts(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    now: ProviderInstant,
) -> ProviderFailure {
    let code = failure_code_for_status(status);
    let message = format!("provider HTTP request failed with status {status}");
    let mut failure = match ProviderFailure::new(code, message).with_status(status) {
        Ok(value) => value,
        Err(_) => ProviderFailure::new(
            ProviderFailureCode::InternalInvariant,
            "provider returned an invalid HTTP status",
        ),
    };
    if let Some(value) = retry_after_from_headers(headers, Some(now)) {
        failure = failure.with_retry_after(value);
    }
    if let Some(evidence) = http_failure_evidence(headers, body, Some(now)) {
        failure = failure.with_evidence(evidence);
    }
    failure
}

pub(crate) fn provider_stream_failure(
    body: &[u8],
    fallback_code: ProviderFailureCode,
    message: &'static str,
) -> ProviderFailure {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ProviderFailure::new(fallback_code, message);
    };
    let headers = embedded_failure_headers(&value);
    let status = stream_failure_status(&value);
    let (remote_type, remote_code) = failure_remote_tokens(&value);
    let code = status.map(failure_code_for_status).unwrap_or_else(|| {
        failure_code_for_remote_tokens(remote_type.as_deref(), remote_code.as_deref())
            .unwrap_or(fallback_code)
    });
    let mut failure = ProviderFailure::new(code, message);
    if let Some(status) = status {
        failure = match failure.with_status(status) {
            Ok(value) => value,
            Err(_) => {
                return ProviderFailure::new(
                    ProviderFailureCode::InternalInvariant,
                    "provider stream contained an invalid HTTP status",
                );
            }
        };
    }
    if let Some(delay) = retry_after_from_headers(&headers, None)
        .or_else(|| relative_reset_delay_milliseconds(&value))
    {
        failure = failure.with_retry_after(delay);
    }
    if let Some(evidence) = http_failure_evidence(&headers, body, None) {
        failure = failure.with_evidence(evidence);
    }
    failure
}

fn failure_code_for_status(status: u16) -> ProviderFailureCode {
    match status {
        400 | 409 | 422 => ProviderFailureCode::InvalidRequest,
        401 => ProviderFailureCode::AuthenticationFailed,
        402 => ProviderFailureCode::BillingUnavailable,
        403 => ProviderFailureCode::PermissionDenied,
        404 => ProviderFailureCode::ModelUnavailable,
        408 | 504 => ProviderFailureCode::TimedOut,
        429 => ProviderFailureCode::RateLimited,
        500..=599 => ProviderFailureCode::ServerFailed,
        _ => ProviderFailureCode::TransportFailed,
    }
}

const MAX_RETRY_AFTER_MILLISECONDS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_SAFE_LIMIT_VALUE: u64 = i64::MAX as u64;

fn http_failure_evidence(
    headers: &BTreeMap<String, String>,
    body: &[u8],
    now: Option<ProviderInstant>,
) -> Option<ProviderFailureEvidence> {
    let mut digest = Sha256::new();
    digest.update(body);
    let body_sha256 = format!("{:x}", digest.finalize());
    let mut evidence =
        ProviderFailureEvidence::new(u64::try_from(body.len()).ok()?, body_sha256).ok()?;
    if let Some(request_id) = header_value(headers, "x-request-id")
        .or_else(|| header_value(headers, "request-id"))
        .or_else(|| header_value(headers, "x-goog-request-id"))
        .and_then(safe_request_id)
    {
        evidence = evidence.with_upstream_request_id(request_id).ok()?;
    }
    if let Some(should_retry) =
        header_value(headers, "x-should-retry").and_then(|value| {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
        })
    {
        evidence = evidence.with_provider_should_retry(should_retry);
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let (error_type, error_code) = failure_remote_tokens(&value);
        if let Some(error_type) = error_type {
            evidence = evidence.with_remote_error_type(error_type).ok()?;
        }
        if let Some(error_code) = error_code {
            evidence = evidence.with_remote_error_code(error_code).ok()?;
        }
        let reset_at = provider_reset_at(&value, now);
        if let Some(reset_at) = reset_at {
            evidence = evidence.with_reset_at_unix_seconds(reset_at).ok()?;
        }
    }
    if let Some(value) = codex_limit(headers, "primary") {
        evidence = evidence.with_codex_primary(value);
    }
    if let Some(value) = codex_limit(headers, "secondary") {
        evidence = evidence.with_codex_secondary(value);
    }
    Some(evidence)
}

fn failure_objects(
    value: &serde_json::Value,
) -> [Option<&serde_json::Map<String, serde_json::Value>>; 3] {
    let top = value.as_object();
    let response = top
        .and_then(|object| object.get("response"))
        .and_then(serde_json::Value::as_object);
    let error = top
        .and_then(|object| object.get("error"))
        .and_then(serde_json::Value::as_object)
        .or_else(|| {
            response
                .and_then(|object| object.get("error"))
                .and_then(serde_json::Value::as_object)
        });
    [error, response, top]
}

fn failure_remote_tokens(value: &serde_json::Value) -> (Option<String>, Option<String>) {
    let objects = failure_objects(value);
    let token = |key: &str| {
        objects.iter().flatten().find_map(|object| {
            object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(safe_remote_error_token)
        })
    };
    (token("type"), token("code"))
}

fn provider_reset_at(value: &serde_json::Value, now: Option<ProviderInstant>) -> Option<u64> {
    let objects = failure_objects(value);
    let absolute = || {
        objects.iter().flatten().find_map(|object| {
            object
                .get("resets_at")
                .or_else(|| object.get("reset_at"))
                .and_then(serde_json::Value::as_u64)
        })
    };
    if let Some(value) = absolute().filter(|value| *value <= i64::MAX as u64) {
        return Some(value);
    }
    let relative = || {
        objects.iter().flatten().find_map(|object| {
            object
                .get("resets_in_seconds")
                .or_else(|| object.get("reset_in_seconds"))
                .and_then(serde_json::Value::as_u64)
        })
    };
    let now = now?;
    let now_seconds = u64::try_from(now.as_unix_milliseconds().div_euclid(1_000)).ok()?;
    now_seconds
        .checked_add(relative()?)
        .filter(|value| *value <= i64::MAX as u64)
}

fn relative_reset_delay_milliseconds(value: &serde_json::Value) -> Option<u64> {
    failure_objects(value)
        .iter()
        .flatten()
        .find_map(|object| {
            object
                .get("resets_in_seconds")
                .or_else(|| object.get("reset_in_seconds"))
                .and_then(serde_json::Value::as_u64)
        })
        .and_then(|seconds| seconds.checked_mul(1_000))
        .map(|milliseconds| milliseconds.min(MAX_RETRY_AFTER_MILLISECONDS))
}

fn stream_failure_status(value: &serde_json::Value) -> Option<u16> {
    failure_objects(value)
        .iter()
        .flatten()
        .find_map(|object| {
            ["status", "status_code", "code"]
                .into_iter()
                .find_map(|key| object.get(key).and_then(serde_json::Value::as_u64))
        })
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..=599).contains(status))
}

fn failure_code_for_remote_tokens(
    error_type: Option<&str>,
    error_code: Option<&str>,
) -> Option<ProviderFailureCode> {
    let has = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| error_type == Some(*candidate) || error_code == Some(*candidate))
    };
    if has(&[
        "rate_limit_error",
        "rate_limit_reached",
        "rate_limit_exceeded",
        "usage_limit_reached",
    ]) {
        Some(ProviderFailureCode::RateLimited)
    } else if has(&["authentication_error", "invalid_api_key"]) {
        Some(ProviderFailureCode::AuthenticationFailed)
    } else if has(&["permission_error"]) {
        Some(ProviderFailureCode::PermissionDenied)
    } else if has(&[
        "billing_hard_limit_reached",
        "billing_error",
        "insufficient_quota",
        "quota_exceeded",
    ]) {
        Some(ProviderFailureCode::BillingUnavailable)
    } else if has(&["model_not_found"]) {
        Some(ProviderFailureCode::ModelUnavailable)
    } else if has(&[
        "invalid_request",
        "invalid_request_error",
        "context_length_exceeded",
    ]) {
        Some(ProviderFailureCode::InvalidRequest)
    } else if has(&["overloaded_error", "server_error", "service_unavailable"]) {
        Some(ProviderFailureCode::ServerFailed)
    } else {
        None
    }
}

fn embedded_failure_headers(value: &serde_json::Value) -> BTreeMap<String, String> {
    let header_object = value
        .as_object()
        .and_then(|object| object.get("headers"))
        .and_then(serde_json::Value::as_object)
        .or_else(|| {
            value
                .get("response")
                .and_then(serde_json::Value::as_object)
                .and_then(|object| object.get("headers"))
                .and_then(serde_json::Value::as_object)
        });
    header_object
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(value) => value.clone(),
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                _ => return None,
            };
            Some((key.clone(), value))
        })
        .collect()
}

fn retry_after_from_headers(
    headers: &BTreeMap<String, String>,
    now: Option<ProviderInstant>,
) -> Option<u64> {
    header_value(headers, "retry-after-ms")
        .and_then(retry_after_millisecond_value)
        .or_else(|| {
            let value = header_value(headers, "retry-after")?;
            retry_after_seconds_value(value)
                .or_else(|| now.and_then(|now| retry_after_milliseconds(value, now)))
        })
        .or_else(|| rate_limit_reset_delay_milliseconds(headers))
}

fn rate_limit_reset_delay_milliseconds(headers: &BTreeMap<String, String>) -> Option<u64> {
    let dimensions = ["requests", "tokens", "project-tokens"];
    let exhausted = dimensions.into_iter().filter_map(|dimension| {
        let remaining = header_value(headers, &format!("x-ratelimit-remaining-{dimension}"))?
            .trim()
            .parse::<u128>()
            .ok()?;
        (remaining == 0).then_some(dimension)
    });
    let resets = |dimensions: &mut dyn Iterator<Item = &str>| {
        dimensions
            .filter_map(|dimension| {
                header_value(headers, &format!("x-ratelimit-reset-{dimension}"))
                    .and_then(rate_limit_duration_milliseconds)
            })
            .max()
    };
    let mut exhausted = exhausted.peekable();
    if exhausted.peek().is_some() {
        return resets(&mut exhausted);
    }
    let mut all = dimensions.into_iter();
    resets(&mut all)
}

fn rate_limit_duration_milliseconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return retry_after_seconds_value(value);
    }

    let bytes = value.as_bytes();
    let mut offset = 0usize;
    let mut total = 0u128;
    while offset < bytes.len() {
        let start = offset;
        let mut dot = None;
        while offset < bytes.len() && (bytes[offset].is_ascii_digit() || bytes[offset] == b'.') {
            if bytes[offset] == b'.' && dot.replace(offset).is_some() {
                return None;
            }
            offset += 1;
        }
        if offset == start || bytes[start] == b'.' || bytes[offset - 1] == b'.' {
            return None;
        }
        let number_end = offset;
        let unit_milliseconds = if bytes.get(offset..offset + 2) == Some(b"ms") {
            offset += 2;
            1u128
        } else {
            let unit = *bytes.get(offset)?;
            offset += 1;
            match unit {
                b's' => 1_000,
                b'm' => 60_000,
                b'h' => 3_600_000,
                _ => return None,
            }
        };
        let (whole, fraction) = match dot {
            Some(dot) => (&value[start..dot], &value[dot + 1..number_end]),
            None => (&value[start..number_end], ""),
        };
        if fraction.len() > 3 {
            return None;
        }
        let whole = whole.parse::<u128>().ok()?;
        let fraction = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u128>().ok()?
                * 10_u128.pow(u32::try_from(3_usize.checked_sub(fraction.len())?).ok()?)
        };
        total = total
            .saturating_add(whole.saturating_mul(unit_milliseconds))
            .saturating_add(fraction.saturating_mul(unit_milliseconds) / 1_000)
            .min(u128::from(MAX_RETRY_AFTER_MILLISECONDS));
    }
    u64::try_from(total).ok().filter(|value| *value > 0)
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn safe_evidence_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        None
    } else {
        Some(value.to_owned())
    }
}

fn safe_request_id(value: &str) -> Option<String> {
    let original = value;
    let value = value.trim();
    if original != value {
        return None;
    }
    let value = safe_evidence_token(value)?;
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("bearer")
        || is_jwt_like_request_id(&value)
        || !(lower.starts_with("req_")
            || lower.starts_with("req-")
            || lower.starts_with("request")
            || lower.starts_with("upstream-req"))
    {
        None
    } else {
        Some(value)
    }
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

fn safe_remote_error_token(value: &str) -> Option<String> {
    let value = safe_evidence_token(value)?;
    const ALLOWLIST: &[&str] = &[
        "authentication_error",
        "billing_hard_limit_reached",
        "billing_error",
        "context_length_exceeded",
        "insufficient_quota",
        "invalid_request",
        "invalid_request_error",
        "invalid_api_key",
        "model_not_found",
        "overloaded_error",
        "permission_error",
        "quota_exceeded",
        "rate_limit_error",
        "rate_limit_reached",
        "rate_limit_exceeded",
        "server_error",
        "service_unavailable",
        "usage_limit_reached",
    ];
    ALLOWLIST.contains(&value.as_str()).then_some(value)
}

fn codex_limit(
    headers: &BTreeMap<String, String>,
    side: &str,
) -> Option<ProviderRateLimitEvidence> {
    let field = |name: &str| {
        header_value(headers, &format!("x-codex-{side}-{name}"))
            .and_then(|value| value.trim().parse::<u128>().ok())
            .map(|value| value.min(u128::from(MAX_SAFE_LIMIT_VALUE)) as u64)
    };
    let mut value = ProviderRateLimitEvidence::new(
        field("used"),
        field("window"),
        field("reset"),
        field("credits"),
    )
    .ok()?;
    if let Some(used_percent) = header_value(headers, &format!("x-codex-{side}-used-percent"))
        .and_then(parse_percent_millis)
    {
        value = value.with_used_percent_millis(used_percent).ok()?;
    }
    if let Some(window_minutes) = header_value(headers, &format!("x-codex-{side}-window-minutes"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value <= i64::MAX as u64)
    {
        value = value.with_window_minutes(window_minutes).ok()?;
    }
    if value.used().is_none()
        && value.window().is_none()
        && value.reset().is_none()
        && value.credits().is_none()
        && value.used_percent_millis().is_none()
        && value.window_minutes().is_none()
    {
        None
    } else {
        Some(value)
    }
}

fn parse_percent_millis(value: &str) -> Option<u64> {
    let value = value.trim();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || fraction.len() > 3
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()? * 10_u64.pow(u32::try_from(3 - fraction.len()).ok()?)
    };
    whole
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(fraction))
        .filter(|value| *value <= 100_000)
}
fn retry_after_milliseconds(value: &str, now: ProviderInstant) -> Option<u64> {
    if let Some(milliseconds) = retry_after_seconds_value(value) {
        return Some(milliseconds);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let instant = ProviderInstant::from_system_time(date).ok()?;
    let delta = instant
        .as_unix_milliseconds()
        .saturating_sub(now.as_unix_milliseconds());
    u64::try_from(delta.max(0))
        .ok()
        .map(|value| value.min(MAX_RETRY_AFTER_MILLISECONDS))
}

fn retry_after_millisecond_value(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u128>()
        .ok()
        .map(|value| value.min(u128::from(MAX_RETRY_AFTER_MILLISECONDS)) as u64)
}

fn retry_after_seconds_value(value: &str) -> Option<u64> {
    let value = value.trim();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || fraction.len() > 3
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u128>().ok()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
            * 10_u128.pow(u32::try_from(3_usize.checked_sub(fraction.len())?).ok()?)
    };
    whole
        .saturating_mul(1_000)
        .saturating_add(fraction)
        .min(u128::from(MAX_RETRY_AFTER_MILLISECONDS))
        .try_into()
        .ok()
}

pub(crate) fn parse_model_catalog(
    data: &[u8],
    array_paths: &[&[&str]],
    id_keys: &[&str],
    capabilities: &ProviderCapabilities,
    refreshed_at: ProviderInstant,
) -> Result<ProviderModelCatalogResult, ProviderFailure> {
    let root = decode_json(data, "model catalog JSON")?;
    let values = array_paths
        .iter()
        .find_map(|path| root.at(path).and_then(ProviderJsonValue::as_array))
        .ok_or_else(|| malformed("provider model catalog is missing its model array"))?;

    let mut models = Vec::with_capacity(values.len());
    let mut seen = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let entry = object(value, &format!("model entry {index}"))?;
        let raw_id = required_catalog_string(entry, id_keys, index)?;
        let id = ProviderModelId::new(raw_id)
            .map_err(|_| malformed(format!("model entry {index} has no valid identifier")))?;
        if !seen.insert(id.clone()) {
            return Err(malformed("model catalog contains duplicate identifiers"));
        }

        let display_name =
            optional_catalog_string(entry, &["display_name", "displayName", "name"], index)?;
        let context_token_limit = optional_catalog_positive_integer(
            entry,
            &["context_length", "inputTokenLimit"],
            index,
        )?;
        let direct_output_limit = optional_catalog_positive_integer(
            entry,
            &["max_tokens", "max_completion_tokens", "outputTokenLimit"],
            index,
        )?;
        let nested_output_limit = match entry.get("top_provider") {
            None => None,
            Some(value) => {
                let top_provider = object(value, "model top provider")?;
                optional_catalog_positive_integer(top_provider, &["max_completion_tokens"], index)?
            }
        };
        let display_name = display_name.filter(|value| value.as_str() != raw_id);
        let model = ProviderModelDescriptor::new(
            id,
            display_name,
            capabilities.clone(),
            context_token_limit,
            direct_output_limit.or(nested_output_limit),
        )
        .map_err(core_error_failure)?;
        models.push(model);
    }
    ProviderModelCatalogResult::new(models, refreshed_at).map_err(core_error_failure)
}

fn required_catalog_string<'a>(
    entry: &'a BTreeMap<String, ProviderJsonValue>,
    keys: &[&str],
    index: usize,
) -> Result<&'a str, ProviderFailure> {
    for key in keys {
        let Some(value) = entry.get(*key) else {
            continue;
        };
        return value
            .as_str()
            .ok_or_else(|| malformed(format!("model entry {index} has a non-string {key}")));
    }
    Err(malformed(format!(
        "model entry {index} has no valid identifier"
    )))
}

fn optional_catalog_string(
    entry: &BTreeMap<String, ProviderJsonValue>,
    keys: &[&str],
    index: usize,
) -> Result<Option<String>, ProviderFailure> {
    for key in keys {
        let Some(value) = entry.get(*key) else {
            continue;
        };
        return match value {
            ProviderJsonValue::Null => Ok(None),
            ProviderJsonValue::String(value) => Ok(Some(value.clone())),
            _ => Err(malformed(format!(
                "model entry {index} has a non-string {key}"
            ))),
        };
    }
    Ok(None)
}

fn optional_catalog_positive_integer(
    entry: &BTreeMap<String, ProviderJsonValue>,
    keys: &[&str],
    index: usize,
) -> Result<Option<usize>, ProviderFailure> {
    for key in keys {
        let Some(value) = entry.get(*key) else {
            continue;
        };
        if matches!(value, ProviderJsonValue::Null) {
            return Ok(None);
        }
        let raw = optional_nonnegative_u64(Some(value), key)?
            .ok_or_else(|| malformed(format!("model entry {index} has an invalid {key}")))?;
        if raw == 0 {
            return Err(malformed(format!(
                "model entry {index} has an invalid {key}"
            )));
        }
        return usize::try_from(raw)
            .map(Some)
            .map_err(|_| malformed(format!("model entry {index} has an invalid {key}")));
    }
    Ok(None)
}

pub(crate) fn usage(
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    total: Option<u64>,
) -> Result<Option<ProviderUsage>, ProviderFailure> {
    if input.is_none() && output.is_none() && cached.is_none() && total.is_none() {
        return Ok(None);
    }
    ProviderUsage::new(input, output, cached, total)
        .map(Some)
        .map_err(|_| malformed("provider usage is invalid"))
}

#[must_use]
pub(crate) fn default_capabilities(structured: CapabilitySupport) -> ProviderCapabilities {
    ProviderCapabilities {
        streaming: CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        ),
        tool_calling: CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        ),
        parallel_tool_calling: CapabilitySupport::Unknown,
        structured_output: structured,
        reasoning_continuity: CapabilitySupport::Unknown,
        usage_reporting: CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        ),
    }
}

pub(crate) fn core_error_failure(error: ProviderCoreError) -> ProviderFailure {
    let ProviderCoreError {
        code,
        message: _redacted_message,
    } = error;
    let code = match code {
        rust_provider_kit_core::ProviderCoreErrorCode::InvalidIdentifier
        | rust_provider_kit_core::ProviderCoreErrorCode::InvalidValue
        | rust_provider_kit_core::ProviderCoreErrorCode::InvalidRequest => {
            ProviderFailureCode::InvalidRequest
        }
        rust_provider_kit_core::ProviderCoreErrorCode::InvalidTransition
        | rust_provider_kit_core::ProviderCoreErrorCode::GenerationExhausted => {
            ProviderFailureCode::InternalInvariant
        }
    };
    ProviderFailure::new(code, "provider value processing failed")
}
#[must_use]
pub(crate) fn malformed(message: impl Into<String>) -> ProviderFailure {
    let message = message.into();
    ProviderFailure::new(ProviderFailureCode::MalformedResponse, &message)
}
#[must_use]
fn response_too_large(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::ResponseTooLarge, message)
}
