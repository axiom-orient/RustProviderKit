use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use http::Method;
use rust_provider_kit_core::{
    BuiltInProviderId, CapabilitySupport, ProviderAccountInspection, ProviderAccountReadiness,
    ProviderCapabilities, ProviderCredentialLease, ProviderDescriptor, ProviderFailure,
    ProviderFailureCode, ProviderInstant, ProviderJsonValue, ProviderMessageContent,
    ProviderMessageRole, ProviderModelCatalogResult, ProviderModelDescriptor, ProviderModelId,
    ProviderNativeState, ProviderOutputRequirement, ProviderProtocolFamily,
    ProviderReasoningPolicy, ProviderRequestConstraints, ProviderToolCall, ProviderToolChoice,
    ProviderTurnRequest,
};
use serde_json::{Map, Value, json};

use crate::adapter::{ProviderAdapter, ProviderStreamDecoder};
use crate::http_transport::{ProviderHttpRequest, ProviderHttpTransport};
use crate::registry::ProviderEndpointCatalog;
use crate::sse::ServerSentEvent;
use crate::wire::{
    ProviderCompletionDraft, ProviderDecodedEvent, append_path, array, core_error_failure,
    http_failure, json_to_serde, make_json_request, merge_account_headers,
    optional_nonnegative_u64, optional_string, provider_stream_failure, require_oauth_bearer,
    serde_to_json, tool_result_text, transport_failure, usage,
};

const NATIVE_STATE_FORMAT: &str = "google.generate-content.parts.v1";

#[derive(Debug, Clone)]
pub(crate) struct GeminiGenerateContentAdapter {
    descriptor: ProviderDescriptor,
}

impl GeminiGenerateContentAdapter {
    pub(crate) fn new() -> Result<Self, ProviderFailure> {
        let descriptor = ProviderDescriptor::new(
            BuiltInProviderId::gemini(),
            "Google Gemini",
            ProviderProtocolFamily::GeminiGenerateContent,
            true,
            false,
        )
        .map_err(core_error_failure)?;
        Ok(Self { descriptor })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        let mut value = default_capabilities();
        value.reasoning_continuity = CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        );
        value
    }

    fn base_url(&self, lease: &ProviderCredentialLease) -> Result<url::Url, ProviderFailure> {
        match lease.record().endpoint() {
            Some(value) => Ok(value.base_url().clone()),
            None => ProviderEndpointCatalog::gemini(),
        }
    }

    fn execution_endpoint(
        &self,
        request: &ProviderTurnRequest,
        lease: &ProviderCredentialLease,
    ) -> Result<url::Url, ProviderFailure> {
        let mut endpoint = self.base_url(lease)?;
        let model_segment = format!(
            "{}:streamGenerateContent",
            request.selection().model_id().as_str()
        );
        endpoint
            .path_segments_mut()
            .map_err(|_| {
                ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "Gemini endpoint cannot accept path segments",
                )
            })?
            .pop_if_empty()
            .push("models")
            .push(&model_segment);
        endpoint.query_pairs_mut().append_pair("alt", "sse");
        Ok(endpoint)
    }

    fn encode_request(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<ProviderJsonValue, ProviderFailure> {
        if request.continuation().is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "Gemini GenerateContent has no server-side continuation identifier",
            ));
        }

        let include_tool_call_id = request
            .selection()
            .model_id()
            .as_str()
            .starts_with("claude-");
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();
        for message in request.messages() {
            if matches!(
                message.role(),
                ProviderMessageRole::System | ProviderMessageRole::Developer
            ) {
                for text in message
                    .content()
                    .iter()
                    .filter_map(ProviderMessageContent::text_value)
                {
                    if !text.trim().is_empty() {
                        system_parts.push(json!({"text": text}));
                    }
                }
                continue;
            }

            let mut native_parts = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::native_state_value);
            if let Some(native_state) = native_parts.next() {
                if message.role() != ProviderMessageRole::Assistant
                    || native_state.format() != NATIVE_STATE_FORMAT
                    || native_parts.next().is_some()
                {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "Gemini native state does not match this provider route",
                    ));
                }
                if native_state
                    .payload()
                    .get("provider")
                    .and_then(ProviderJsonValue::as_str)
                    != Some(request.selection().provider_id().as_str())
                    || native_state
                        .payload()
                        .get("model")
                        .and_then(ProviderJsonValue::as_str)
                        != Some(request.selection().model_id().as_str())
                {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "Gemini native state is bound to a different provider or model",
                    ));
                }
                let parts = native_state
                    .payload()
                    .get("parts")
                    .and_then(ProviderJsonValue::as_array)
                    .ok_or_else(|| {
                        ProviderFailure::new(
                            ProviderFailureCode::MalformedResponse,
                            "Gemini native state payload must contain a parts array",
                        )
                    })?;
                let parts = parts
                    .iter()
                    .map(json_to_serde)
                    .collect::<Result<Vec<_>, _>>()?;
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
                continue;
            }

            if message.role() == ProviderMessageRole::Tool {
                let mut parts = Vec::new();
                for item in message.content() {
                    if let Some((call_id, name, value)) = item.tool_result_value() {
                        let text = tool_result_text(value)?;
                        let response_key = if item.tool_result_is_error().unwrap_or(false) {
                            "error"
                        } else {
                            "output"
                        };
                        let mut response = Map::new();
                        response.insert(response_key.into(), Value::String(text));
                        let mut function_response = Map::new();
                        function_response.insert("name".into(), Value::String(name.to_owned()));
                        function_response.insert("response".into(), Value::Object(response));
                        if include_tool_call_id {
                            function_response
                                .insert("id".into(), Value::String(call_id.to_owned()));
                        }
                        parts.push(json!({"functionResponse":function_response}));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role":"user","parts":parts}));
                }
                continue;
            }

            let mut parts = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::text_value)
                .filter(|text| !text.trim().is_empty())
                .map(|text| json!({"text":text}))
                .collect::<Vec<_>>();
            for item in message.content() {
                if let Some(call) = item.tool_call_value() {
                    let mut function_call = Map::new();
                    function_call.insert("name".into(), Value::String(call.name().to_owned()));
                    function_call.insert("args".into(), json_to_serde(call.arguments())?);
                    if include_tool_call_id {
                        function_call.insert("id".into(), Value::String(call.id().to_owned()));
                    }
                    parts.push(json!({"functionCall":function_call}));
                }
            }
            if !parts.is_empty() {
                contents.push(json!({
                    "role": if message.role() == ProviderMessageRole::Assistant { "model" } else { "user" },
                    "parts": parts
                }));
            }
        }

        let mut body = Map::new();
        body.insert("contents".into(), Value::Array(contents));
        if !system_parts.is_empty() {
            body.insert("systemInstruction".into(), json!({"parts":system_parts}));
        }
        if !request.tools().is_empty() {
            let declarations = request
                .tools()
                .iter()
                .map(|tool| -> Result<Value, ProviderFailure> {
                    Ok(json!({
                        "name":tool.name(),
                        "description":tool.description(),
                        "parametersJsonSchema":json_to_serde(tool.input_schema())?
                    }))
                })
                .collect::<Result<Vec<_>, _>>()?;
            body.insert(
                "tools".into(),
                json!([{"functionDeclarations":declarations}]),
            );
            let function_calling_config = match request.tool_choice() {
                ProviderToolChoice::Automatic => json!({"mode":"AUTO"}),
                ProviderToolChoice::Required => json!({"mode":"ANY"}),
                ProviderToolChoice::Named { name } => {
                    json!({"mode":"ANY","allowedFunctionNames":[name]})
                }
            };
            body.insert(
                "toolConfig".into(),
                json!({"functionCallingConfig":function_calling_config}),
            );
        }

        let mut generation = Map::new();
        if let Some(limit) = request.constraints().maximum_output_tokens() {
            generation.insert("maxOutputTokens".into(), json!(limit));
        }
        match request.output() {
            ProviderOutputRequirement::Text => {}
            ProviderOutputRequirement::ApplicationValidatedJson { schema, .. } => {
                generation.insert(
                    "responseMimeType".into(),
                    Value::String("application/json".into()),
                );
                generation.insert("responseJsonSchema".into(), json_to_serde(schema)?);
            }
            ProviderOutputRequirement::JsonSchema { schema, strict, .. } => {
                if !strict {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "Gemini structured output requires a strict schema contract",
                    ));
                }
                generation.insert(
                    "responseMimeType".into(),
                    Value::String("application/json".into()),
                );
                generation.insert("responseJsonSchema".into(), json_to_serde(schema)?);
            }
        }
        match request.reasoning() {
            ProviderReasoningPolicy::Automatic => {}
            ProviderReasoningPolicy::Disabled => {
                generation.insert("thinkingConfig".into(), json!({"thinkingBudget":0}));
            }
            ProviderReasoningPolicy::Effort(effort) => {
                generation.insert(
                    "thinkingConfig".into(),
                    json!({
                        "includeThoughts":true,
                        "thinkingLevel":effort.as_str()
                    }),
                );
            }
        }
        if !generation.is_empty() {
            body.insert("generationConfig".into(), Value::Object(generation));
        }
        serde_to_json(Value::Object(body))
    }
}

#[async_trait]
impl ProviderAdapter for GeminiGenerateContentAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    async fn make_execution_request(
        &self,
        request: &ProviderTurnRequest,
        credential: &ProviderCredentialLease,
    ) -> Result<ProviderHttpRequest, ProviderFailure> {
        let key = require_oauth_bearer(credential)?;
        make_json_request(
            Method::POST,
            self.execution_endpoint(request, credential)?,
            merge_account_headers(
                BTreeMap::from([
                    ("x-goog-api-key".into(), key.to_owned()),
                    ("accept".into(), "text/event-stream".into()),
                ]),
                credential,
            ),
            &self.encode_request(request)?,
            request.constraints(),
        )
    }

    fn make_decoder(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<Box<dyn ProviderStreamDecoder>, ProviderFailure> {
        Ok(Box::new(GeminiGenerateContentStreamDecoder::new(
            request.selection().clone(),
        )))
    }

    async fn inspect(
        &self,
        credential: &ProviderCredentialLease,
        transport: &dyn ProviderHttpTransport,
        now: ProviderInstant,
    ) -> Result<ProviderAccountInspection, ProviderFailure> {
        let _ = self.models(credential, transport, now).await?;
        ProviderAccountInspection::new(
            credential.record().account_id().clone(),
            self.descriptor.id().clone(),
            ProviderAccountReadiness::Ready,
            None,
            self.capabilities(),
            now,
        )
        .map_err(core_error_failure)
    }

    async fn models(
        &self,
        credential: &ProviderCredentialLease,
        transport: &dyn ProviderHttpTransport,
        now: ProviderInstant,
    ) -> Result<ProviderModelCatalogResult, ProviderFailure> {
        let key = require_oauth_bearer(credential)?;
        let endpoint = append_path("/models", &self.base_url(credential)?)?;
        let constraints = ProviderRequestConstraints::new(
            rust_provider_kit_core::ProviderDataCollectionPolicy::Deny,
            true,
            true,
            60_000,
            16 * 1_024 * 1_024,
            1,
            None,
        )
        .map_err(core_error_failure)?;
        let request = ProviderHttpRequest::with_timeout(
            Method::GET,
            endpoint,
            merge_account_headers(
                BTreeMap::from([
                    ("x-goog-api-key".into(), key.to_owned()),
                    ("accept".into(), "application/json".into()),
                ]),
                credential,
            ),
            Vec::new(),
            constraints.timeout_milliseconds(),
            constraints.maximum_response_bytes(),
        )?;
        let response = transport.send(request).await.map_err(transport_failure)?;
        if !(200..300).contains(&response.status_code) {
            return Err(http_failure(&response, now));
        }
        let root = ProviderJsonValue::decode(&response.body)
            .map_err(|_| malformed("Gemini model catalog JSON is malformed"))?;
        let values = root
            .get("models")
            .and_then(ProviderJsonValue::as_array)
            .ok_or_else(|| malformed("Gemini model catalog is missing models"))?;
        let mut models = Vec::new();
        let mut seen = BTreeSet::new();
        for value in values {
            let raw_name = value
                .get("name")
                .and_then(ProviderJsonValue::as_str)
                .ok_or_else(|| malformed("Gemini model entry has no valid name"))?;
            let raw_id = raw_name.strip_prefix("models/").unwrap_or(raw_name);
            let id = ProviderModelId::new(raw_id).map_err(core_error_failure)?;
            if !seen.insert(id.clone()) {
                return Err(malformed(
                    "Gemini model catalog contains duplicate identifier",
                ));
            }
            let display_name = match value.get("displayName") {
                None | Some(ProviderJsonValue::Null) => None,
                Some(ProviderJsonValue::String(text)) if text != raw_id => Some(text.clone()),
                Some(ProviderJsonValue::String(_)) => None,
                _ => return Err(malformed("Gemini model displayName is invalid")),
            };
            let input_limit =
                positive_usize(value.get("inputTokenLimit"), "Gemini inputTokenLimit")?;
            let output_limit =
                positive_usize(value.get("outputTokenLimit"), "Gemini outputTokenLimit")?;
            models.push(
                ProviderModelDescriptor::new(
                    id,
                    display_name,
                    self.capabilities(),
                    input_limit,
                    output_limit,
                )
                .map_err(core_error_failure)?,
            );
        }
        ProviderModelCatalogResult::new(models, now).map_err(core_error_failure)
    }
}

#[derive(Debug)]
pub(crate) struct GeminiGenerateContentStreamDecoder {
    selection: rust_provider_kit_core::ProviderSelection,
    native_parts: Vec<Value>,
    usage: Option<rust_provider_kit_core::ProviderUsage>,
    finish_reason: Option<String>,
    emitted_tool_count: usize,
    generated_tool_call_count: usize,
    completed: bool,
}

impl GeminiGenerateContentStreamDecoder {
    fn new(selection: rust_provider_kit_core::ProviderSelection) -> Self {
        Self {
            selection,
            native_parts: Vec::new(),
            usage: None,
            finish_reason: None,
            emitted_tool_count: 0,
            generated_tool_call_count: 0,
            completed: false,
        }
    }
}

impl ProviderStreamDecoder for GeminiGenerateContentStreamDecoder {
    fn consume(
        &mut self,
        event: &ServerSentEvent,
    ) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if event.data == "[DONE]" {
            return self.complete();
        }
        let root = ProviderJsonValue::decode(event.data.as_bytes())
            .map_err(|_| malformed("Gemini stream JSON is malformed"))?;
        if root.get("error").is_some() {
            return Err(provider_stream_failure(
                event.data.as_bytes(),
                ProviderFailureCode::ServerFailed,
                "Gemini stream failed",
            ));
        }
        if let Some(value) = root.get("usageMetadata") {
            self.usage = parse_gemini_usage(value)?;
        }
        if let Some(feedback) = root.get("promptFeedback")
            && feedback.get("blockReason").is_some()
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::PermissionDenied,
                "Gemini blocked the prompt",
            ));
        }

        let mut output = Vec::new();
        if let Some(candidates_value) = root.get("candidates") {
            let candidates = array(candidates_value, "Gemini candidates")?;
            if candidates.len() > 1 {
                return Err(malformed(
                    "Gemini returned multiple candidates for a single-output request",
                ));
            }
            if let Some(candidate) = candidates.first() {
                if let Some(parts_value) = candidate.at(&["content", "parts"]) {
                    for part in array(parts_value, "Gemini content parts")? {
                        self.native_parts.push(json_to_serde(part)?);
                        if let Some(text) = optional_string(part.get("text"), "Gemini part text")?
                            && !text.is_empty()
                        {
                            if matches!(part.get("thought"), Some(ProviderJsonValue::Bool(true))) {
                                output.push(ProviderDecodedEvent::Reasoning(text.to_owned()));
                            } else {
                                output.push(ProviderDecodedEvent::Text(text.to_owned()));
                            }
                        }
                        if let Some(function_call) = part.get("functionCall") {
                            if self.emitted_tool_count >= ProviderTurnRequest::MAXIMUM_TOOLS {
                                return Err(malformed("Gemini stream exceeded tool-call limit"));
                            }
                            let name = function_call
                                .get("name")
                                .and_then(ProviderJsonValue::as_str)
                                .ok_or_else(|| {
                                    malformed("Gemini function call has no valid name")
                                })?;
                            let arguments = function_call
                                .get("args")
                                .cloned()
                                .unwrap_or_else(|| ProviderJsonValue::Object(BTreeMap::new()));
                            if arguments.as_object().is_none() {
                                return Err(malformed(
                                    "Gemini function-call arguments are not an object",
                                ));
                            }
                            self.generated_tool_call_count = self
                                .generated_tool_call_count
                                .checked_add(1)
                                .ok_or_else(|| malformed("Gemini tool-call count overflowed"))?;
                            let id = function_call
                                .get("id")
                                .and_then(ProviderJsonValue::as_str)
                                .filter(|value| valid_tool_call_id(value))
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    format!("gemini-call-{}", self.generated_tool_call_count)
                                });
                            self.emitted_tool_count =
                                self.emitted_tool_count.checked_add(1).ok_or_else(|| {
                                    malformed("Gemini emitted tool-call count overflowed")
                                })?;
                            output.push(ProviderDecodedEvent::ToolCall(
                                ProviderToolCall::new(id, name, arguments)
                                    .map_err(core_error_failure)?,
                            ));
                        }
                    }
                }
                if let Some(reason) =
                    optional_string(candidate.get("finishReason"), "Gemini finish reason")?
                {
                    if self
                        .finish_reason
                        .as_deref()
                        .is_some_and(|current| current != reason)
                    {
                        return Err(malformed(
                            "Gemini stream emitted conflicting finish reasons",
                        ));
                    }
                    self.finish_reason = Some(reason.to_owned());
                }
            }
        }
        if self.finish_reason.is_some() {
            output.extend(self.complete()?);
        }
        Ok(output)
    }

    fn finish(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        self.complete()
    }
}

impl GeminiGenerateContentStreamDecoder {
    fn complete(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if self.completed {
            return Ok(Vec::new());
        }
        let reason = self
            .finish_reason
            .as_deref()
            .ok_or_else(|| malformed("Gemini stream ended without a finish reason"))?;
        match reason {
            "STOP" => {}
            "MAX_TOKENS" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ServerFailed,
                    "Gemini response was truncated by its token limit",
                ));
            }
            "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::PermissionDenied,
                    format!("Gemini blocked the response with finish reason {reason}"),
                ));
            }
            _ => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ServerFailed,
                    "Gemini ended with an unsupported finish reason",
                ));
            }
        }
        self.completed = true;
        let native_state = if self.native_parts.is_empty() {
            None
        } else {
            Some(
                ProviderNativeState::new(
                    NATIVE_STATE_FORMAT,
                    serde_to_json(json!({
                        "provider":self.selection.provider_id().as_str(),
                        "model":self.selection.model_id().as_str(),
                        "parts":self.native_parts.clone()
                    }))?,
                )
                .map_err(core_error_failure)?,
            )
        };
        Ok(vec![ProviderDecodedEvent::Completion(
            ProviderCompletionDraft {
                response_id: None,
                continuation: None,
                native_state,
                usage: self.usage.clone(),
            },
        )])
    }
}

fn parse_gemini_usage(
    value: &ProviderJsonValue,
) -> Result<Option<rust_provider_kit_core::ProviderUsage>, ProviderFailure> {
    let prompt = optional_nonnegative_u64(value.get("promptTokenCount"), "Gemini prompt tokens")?;
    let cached =
        optional_nonnegative_u64(value.get("cachedContentTokenCount"), "Gemini cached tokens")?
            .unwrap_or(0);
    let candidate =
        optional_nonnegative_u64(value.get("candidatesTokenCount"), "Gemini candidate tokens")?;
    let thoughts =
        optional_nonnegative_u64(value.get("thoughtsTokenCount"), "Gemini thought tokens")?
            .unwrap_or(0);
    let total = optional_nonnegative_u64(value.get("totalTokenCount"), "Gemini total tokens")?;
    let input = prompt
        .map(|count| {
            count
                .checked_sub(cached)
                .ok_or_else(|| malformed("Gemini cached tokens exceed prompt tokens"))
        })
        .transpose()?;
    let output = candidate
        .map(|count| {
            count
                .checked_add(thoughts)
                .ok_or_else(|| malformed("Gemini output token count overflowed"))
        })
        .transpose()?;
    usage(input, output, Some(cached), total)
}

fn positive_usize(
    value: Option<&ProviderJsonValue>,
    field: &str,
) -> Result<Option<usize>, ProviderFailure> {
    let value = optional_nonnegative_u64(value, field)?;
    match value {
        None => Ok(None),
        Some(0) => Err(malformed(format!("{field} must be positive"))),
        Some(value) => usize::try_from(value)
            .map(Some)
            .map_err(|_| malformed(format!("{field} is out of range"))),
    }
}

fn valid_tool_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 192
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn default_capabilities() -> ProviderCapabilities {
    crate::wire::default_capabilities(CapabilitySupport::Declared(
        rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
    ))
}

fn malformed(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::MalformedResponse, message.into())
}
