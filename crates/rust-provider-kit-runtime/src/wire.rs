use std::collections::{BTreeMap, BTreeSet};

use http::Method;
use rust_provider_kit_core::{
    CapabilitySupport, ProviderCapabilities, ProviderCompletion, ProviderContinuation,
    ProviderCoreError, ProviderCredentialLease, ProviderCredentialMaterial, ProviderFailure,
    ProviderFailureCode, ProviderInstant, ProviderJsonValue, ProviderModelCatalogResult,
    ProviderModelDescriptor, ProviderModelId, ProviderNativeState, ProviderRequestConstraints,
    ProviderToolCall, ProviderUsage,
};
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
    _body: &[u8],
    now: ProviderInstant,
) -> ProviderFailure {
    let code = match status {
        400 => ProviderFailureCode::InvalidRequest,
        401 => ProviderFailureCode::AuthenticationFailed,
        403 => ProviderFailureCode::PermissionDenied,
        404 => ProviderFailureCode::ModelUnavailable,
        402 => ProviderFailureCode::BillingUnavailable,
        408 | 504 => ProviderFailureCode::TimedOut,
        409 | 422 => ProviderFailureCode::InvalidRequest,
        429 => ProviderFailureCode::RateLimited,
        500..=599 => ProviderFailureCode::ServerFailed,
        _ => ProviderFailureCode::TransportFailed,
    };
    let message = format!("provider HTTP request failed with status {status}");
    let mut failure = match ProviderFailure::new(code, message).with_status(status) {
        Ok(value) => value,
        Err(_) => ProviderFailure::new(
            ProviderFailureCode::InternalInvariant,
            "provider returned an invalid HTTP status",
        ),
    };
    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| retry_after_milliseconds(value, now))
    {
        failure = failure.with_retry_after(value);
    }
    failure
}
fn retry_after_milliseconds(value: &str, now: ProviderInstant) -> Option<u64> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return seconds.checked_mul(1_000);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let instant = ProviderInstant::from_system_time(date).ok()?;
    let delta = instant
        .as_unix_milliseconds()
        .saturating_sub(now.as_unix_milliseconds());
    u64::try_from(delta.max(0)).ok()
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
