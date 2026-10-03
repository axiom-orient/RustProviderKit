use std::collections::BTreeMap;

use async_trait::async_trait;
use http::Method;
use rust_provider_kit_core::{
    BuiltInProviderId, CapabilitySupport, ProviderAccountInspection, ProviderAccountReadiness,
    ProviderCapabilities, ProviderCredentialLease, ProviderCredentialRecord, ProviderDescriptor,
    ProviderFailure, ProviderFailureCode, ProviderInstant, ProviderJsonValue,
    ProviderMessageContent, ProviderMessageRole, ProviderModelCatalogResult, ProviderNativeState,
    ProviderOutputRequirement, ProviderProtocolFamily, ProviderReasoningPolicy,
    ProviderRequestConstraints, ProviderToolCall, ProviderToolChoice, ProviderTurnRequest,
};
use serde_json::{Map, Value, json};
use url::Url;

use crate::adapter::{ProviderAdapter, ProviderStreamDecoder};
use crate::http_transport::{ProviderHttpRequest, ProviderHttpTransport};
use crate::registry::ProviderEndpointCatalog;
use crate::sse::ServerSentEvent;
use crate::wire::{
    ProviderCompletionDraft, ProviderDecodedEvent, ProviderToolArgumentAccumulator, append_path,
    array, core_error_failure, default_capabilities, http_failure, json_to_serde,
    make_json_request, merge_account_headers, optional_nonnegative_u64, optional_string,
    parse_model_catalog, provider_stream_failure, require_oauth_bearer, serde_to_json,
    tool_result_text, transport_failure, usage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiChatKind {
    OpenRouter,
    DeepSeek,
    Qwen,
    Kimi,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenAiChatAdapter {
    kind: OpenAiChatKind,
    descriptor: ProviderDescriptor,
}
impl OpenAiChatAdapter {
    pub(crate) fn new(kind: OpenAiChatKind) -> Result<Self, ProviderFailure> {
        let (id, name) = match kind {
            OpenAiChatKind::OpenRouter => (BuiltInProviderId::open_router(), "OpenRouter"),
            OpenAiChatKind::DeepSeek => (BuiltInProviderId::deep_seek(), "DeepSeek"),
            OpenAiChatKind::Qwen => (BuiltInProviderId::qwen(), "Qwen Portal"),
            OpenAiChatKind::Kimi => (BuiltInProviderId::kimi(), "Kimi Code"),
        };
        let descriptor = ProviderDescriptor::new(
            id,
            name,
            ProviderProtocolFamily::OpenAiChatCompletions,
            true,
            false,
        )
        .map_err(core_error_failure)?;
        Ok(Self { kind, descriptor })
    }
    fn base_url(&self, record: &ProviderCredentialRecord) -> Result<Url, ProviderFailure> {
        if let Some(endpoint) = record.endpoint() {
            return Ok(endpoint.base_url().clone());
        }
        match self.kind {
            OpenAiChatKind::OpenRouter => ProviderEndpointCatalog::open_router(),
            OpenAiChatKind::DeepSeek => ProviderEndpointCatalog::deep_seek(),
            OpenAiChatKind::Qwen => ProviderEndpointCatalog::qwen(),
            OpenAiChatKind::Kimi => ProviderEndpointCatalog::kimi(),
        }
    }
    fn capabilities(&self) -> ProviderCapabilities {
        let mut capabilities = default_capabilities(CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        ));
        capabilities.reasoning_continuity = match self.kind {
            OpenAiChatKind::OpenRouter | OpenAiChatKind::DeepSeek | OpenAiChatKind::Kimi => {
                CapabilitySupport::Declared(
                    rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
                )
            }
            OpenAiChatKind::Qwen => CapabilitySupport::Unknown,
        };
        capabilities
    }
    fn encode_request(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<ProviderJsonValue, ProviderFailure> {
        if request.continuation().is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "chat-completions providers do not accept a Responses continuation",
            ));
        }
        if matches!(
            request.output(),
            ProviderOutputRequirement::JsonSchema { .. }
        ) && !matches!(self.kind, OpenAiChatKind::OpenRouter | OpenAiChatKind::Kimi)
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "strict JSON schema output is not qualified for this provider dialect",
            ));
        }
        let schema_instruction = match request.output() {
            ProviderOutputRequirement::ApplicationValidatedJson { schema, .. }
                if matches!(self.kind, OpenAiChatKind::DeepSeek | OpenAiChatKind::Qwen) =>
            {
                let schema_text = String::from_utf8(
                    schema.encoded_vec().map_err(core_error_failure)?,
                )
                .map_err(|_| {
                    ProviderFailure::new(
                        ProviderFailureCode::InternalInvariant,
                        "encoded JSON is not UTF-8",
                    )
                })?;
                Some(format!(
                    "Return exactly one valid JSON object matching this application schema and no other text: {schema_text}"
                ))
            }
            _ => None,
        };
        let mut messages = Vec::with_capacity(
            request.messages().len() + usize::from(schema_instruction.is_some()),
        );
        let mut coalesced_system = schema_instruction.into_iter().collect::<Vec<_>>();
        for message in request.messages() {
            if message.role() == ProviderMessageRole::Tool {
                for item in message.content() {
                    if let Some((call_id, _, value)) = item.tool_result_value() {
                        let content = tool_result_text(value)?;
                        messages
                            .push(json!({"role":"tool","tool_call_id":call_id,"content":content}));
                    }
                }
                continue;
            }
            let role = match message.role() {
                ProviderMessageRole::Developer => "system",
                ProviderMessageRole::System => "system",
                ProviderMessageRole::User => "user",
                ProviderMessageRole::Assistant => "assistant",
                ProviderMessageRole::Tool => "tool",
            };
            if role == "system" && matches!(self.kind, OpenAiChatKind::Qwen | OpenAiChatKind::Kimi)
            {
                coalesced_system.extend(
                    message
                        .content()
                        .iter()
                        .filter_map(ProviderMessageContent::text_value)
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned),
                );
                continue;
            }
            let mut text = String::new();
            for value in message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::text_value)
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            let calls = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::tool_call_value)
                .map(|call| -> Result<Value, ProviderFailure> {
                    let arguments = String::from_utf8(
                        call.arguments().encoded_vec().map_err(core_error_failure)?,
                    )
                    .map_err(|_| {
                        ProviderFailure::new(
                            ProviderFailureCode::InternalInvariant,
                            "encoded JSON is not UTF-8",
                        )
                    })?;
                    Ok(json!({
                        "id":call.id(),
                        "type":"function",
                        "function":{"name":call.name(),"arguments":arguments}
                    }))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let native_reasoning = chat_reasoning_state(message, request.selection())?;
            if !text.is_empty() || !calls.is_empty() {
                let mut value = json!({"role":role,"content":text});
                if !calls.is_empty() {
                    value["tool_calls"] = Value::Array(calls);
                }
                if let Some((field, content)) = native_reasoning {
                    value[field] = Value::String(content);
                } else if message.role() == ProviderMessageRole::Assistant {
                    let synthetic = match self.kind {
                        OpenAiChatKind::DeepSeek
                            if !matches!(
                                request.reasoning(),
                                ProviderReasoningPolicy::Automatic
                            ) =>
                        {
                            Some("")
                        }
                        OpenAiChatKind::Kimi if value.get("tool_calls").is_some() => Some("."),
                        OpenAiChatKind::OpenRouter
                            if value.get("tool_calls").is_some()
                                && !matches!(
                                    request.reasoning(),
                                    ProviderReasoningPolicy::Automatic
                                ) =>
                        {
                            Some(".")
                        }
                        _ => None,
                    };
                    if let Some(content) = synthetic {
                        value["reasoning_content"] = Value::String(content.into());
                    }
                }
                messages.push(value);
            }
        }
        if !coalesced_system.is_empty() {
            let mut ordered = Vec::with_capacity(messages.len().saturating_add(1));
            ordered.push(json!({"role":"system","content":coalesced_system.join("\n\n")}));
            ordered.extend(messages);
            messages = ordered;
        }
        let mut body = Map::new();
        body.insert(
            "model".into(),
            Value::String(request.selection().model_id().as_str().to_owned()),
        );
        body.insert("stream".into(), Value::Bool(true));
        body.insert("stream_options".into(), json!({"include_usage":true}));
        if let Some(limit) = request.constraints().maximum_output_tokens() {
            let key = if self.kind == OpenAiChatKind::DeepSeek {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            body.insert(key.into(), json!(limit));
        }
        if !request.tools().is_empty() {
            let tools = request.tools().iter().map(|tool| -> Result<Value, ProviderFailure> {
                Ok(json!({"type":"function","function":{"name":tool.name(),"description":tool.description(),"parameters":json_to_serde(tool.input_schema())?,"strict":tool.strict()}}))
            }).collect::<Result<Vec<_>, _>>()?;
            body.insert("tools".into(), Value::Array(tools));
            let choice = match request.tool_choice() {
                ProviderToolChoice::Automatic => Value::String("auto".into()),
                ProviderToolChoice::Required => Value::String("required".into()),
                ProviderToolChoice::Named { name } => {
                    json!({"type":"function","function":{"name":name}})
                }
            };
            if self.kind == OpenAiChatKind::DeepSeek {
                if !matches!(request.tool_choice(), ProviderToolChoice::Automatic) {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "DeepSeek does not qualify forced tool choice while thinking is enabled",
                    ));
                }
            } else {
                body.insert("tool_choice".into(), choice);
            }
        }
        match request.output() {
            ProviderOutputRequirement::Text => {}
            ProviderOutputRequirement::ApplicationValidatedJson { name, schema } => {
                match self.kind {
                    OpenAiChatKind::OpenRouter | OpenAiChatKind::Kimi => {
                        body.insert("response_format".into(), json!({"type":"json_schema","json_schema":{"name":name,"schema":json_to_serde(schema)?,"strict":true}}));
                    }
                    OpenAiChatKind::DeepSeek | OpenAiChatKind::Qwen => {
                        body.insert("response_format".into(), json!({"type":"json_object"}));
                    }
                }
            }
            ProviderOutputRequirement::JsonSchema {
                name,
                schema,
                strict,
            } => {
                body.insert("response_format".into(), json!({"type":"json_schema","json_schema":{"name":name,"schema":json_to_serde(schema)?,"strict":strict}}));
            }
        }
        body.insert("messages".into(), Value::Array(messages));
        apply_reasoning_policy(self.kind, request, &mut body);
        if self.kind == OpenAiChatKind::OpenRouter {
            body.insert("provider".into(), json!({
                "allow_fallbacks": false,
                "require_parameters": request.constraints().requires_parameter_support(),
                "data_collection": match request.constraints().data_collection() { rust_provider_kit_core::ProviderDataCollectionPolicy::Deny => "deny", rust_provider_kit_core::ProviderDataCollectionPolicy::Allow => "allow" },
                "zdr": request.constraints().requires_zero_data_retention()
            }));
        }
        serde_to_json(Value::Object(body))
    }
}

fn apply_reasoning_policy(
    kind: OpenAiChatKind,
    request: &ProviderTurnRequest,
    body: &mut Map<String, Value>,
) {
    match (kind, request.reasoning()) {
        (_, ProviderReasoningPolicy::Automatic) => {}
        (OpenAiChatKind::OpenRouter, ProviderReasoningPolicy::Disabled) => {
            body.insert("reasoning".into(), json!({"enabled":false}));
        }
        (OpenAiChatKind::OpenRouter, ProviderReasoningPolicy::Effort(effort)) => {
            body.insert("reasoning".into(), json!({"effort":effort.as_str()}));
        }
        (OpenAiChatKind::DeepSeek, ProviderReasoningPolicy::Disabled) => {
            body.insert("thinking".into(), json!({"type":"disabled"}));
        }
        (OpenAiChatKind::DeepSeek, ProviderReasoningPolicy::Effort(_)) => {
            body.insert("thinking".into(), json!({"type":"enabled"}));
            body.insert("reasoning_effort".into(), Value::String("high".into()));
        }
        (OpenAiChatKind::Qwen, ProviderReasoningPolicy::Disabled) => {
            body.insert("enable_thinking".into(), Value::Bool(false));
        }
        (OpenAiChatKind::Qwen, ProviderReasoningPolicy::Effort(_)) => {
            body.insert("enable_thinking".into(), Value::Bool(true));
        }
        (OpenAiChatKind::Kimi, ProviderReasoningPolicy::Disabled) => {
            body.insert("thinking".into(), json!({"type":"disabled"}));
        }
        (OpenAiChatKind::Kimi, ProviderReasoningPolicy::Effort(_)) => {
            let forced = !matches!(request.tool_choice(), ProviderToolChoice::Automatic)
                && !request.tools().is_empty();
            body.insert(
                "thinking".into(),
                json!({"type":if forced {"disabled"} else {"enabled"}}),
            );
        }
    }
}

fn chat_reasoning_state(
    message: &rust_provider_kit_core::ProviderMessage,
    selection: &rust_provider_kit_core::ProviderSelection,
) -> Result<Option<(String, String)>, ProviderFailure> {
    let mut states = message
        .content()
        .iter()
        .filter_map(ProviderMessageContent::native_state_value);
    let Some(state) = states.next() else {
        return Ok(None);
    };
    if message.role() != ProviderMessageRole::Assistant
        || state.format() != "openai.chat.reasoning.v1"
        || states.next().is_some()
    {
        return Err(ProviderFailure::new(
            ProviderFailureCode::CapabilityMismatch,
            "chat reasoning state does not match this provider route",
        ));
    }
    if state
        .payload()
        .get("provider")
        .and_then(ProviderJsonValue::as_str)
        != Some(selection.provider_id().as_str())
        || state
            .payload()
            .get("model")
            .and_then(ProviderJsonValue::as_str)
            != Some(selection.model_id().as_str())
    {
        return Err(ProviderFailure::new(
            ProviderFailureCode::CapabilityMismatch,
            "chat reasoning state is bound to a different provider or model",
        ));
    }
    let field = state
        .payload()
        .get("field")
        .and_then(ProviderJsonValue::as_str)
        .filter(|field| matches!(*field, "reasoning_content" | "reasoning" | "reasoning_text"))
        .ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "chat reasoning state has an invalid field",
            )
        })?;
    let content = state
        .payload()
        .get("content")
        .and_then(ProviderJsonValue::as_str)
        .ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "chat reasoning state has invalid content",
            )
        })?;
    Ok(Some((field.to_owned(), content.to_owned())))
}

#[async_trait]
impl ProviderAdapter for OpenAiChatAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    async fn make_execution_request(
        &self,
        request: &ProviderTurnRequest,
        credential: &ProviderCredentialLease,
    ) -> Result<ProviderHttpRequest, ProviderFailure> {
        let key = require_oauth_bearer(credential)?;
        let base = self.base_url(credential.record())?;
        let endpoint = append_path("/chat/completions", &base)?;
        let mut headers = BTreeMap::from([
            ("authorization".into(), format!("Bearer {key}")),
            ("accept".into(), "text/event-stream".into()),
        ]);
        if self.kind == OpenAiChatKind::OpenRouter {
            headers.insert("x-openrouter-title".into(), "RustProviderKit".into());
        } else if self.kind == OpenAiChatKind::Kimi {
            headers.insert("user-agent".into(), "KimiCLI/1.0".into());
            headers.insert("x-msh-platform".into(), "kimi_cli".into());
        }
        let headers = merge_account_headers(headers, credential);
        make_json_request(
            Method::POST,
            endpoint,
            headers,
            &self.encode_request(request)?,
            request.constraints(),
        )
    }
    fn make_decoder(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<Box<dyn ProviderStreamDecoder>, ProviderFailure> {
        Ok(Box::new(OpenAiChatStreamDecoder::new(
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
        let endpoint = append_path("/models", &self.base_url(credential.record())?)?;
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
        let headers = merge_account_headers(
            BTreeMap::from([
                ("authorization".into(), format!("Bearer {key}")),
                ("accept".into(), "application/json".into()),
            ]),
            credential,
        );
        let request = ProviderHttpRequest::with_timeout(
            Method::GET,
            endpoint,
            headers,
            Vec::new(),
            constraints.timeout_milliseconds(),
            constraints.maximum_response_bytes(),
        )?;
        let response = transport.send(request).await.map_err(transport_failure)?;
        if !(200..300).contains(&response.status_code) {
            return Err(http_failure(&response, now));
        }
        parse_model_catalog(
            &response.body,
            &[&["data"], &["models"]],
            &["id", "model", "name"],
            &self.capabilities(),
            now,
        )
    }
}

#[derive(Debug, Default)]
struct ChatToolState {
    id: Option<String>,
    name: Option<String>,
    arguments: ProviderToolArgumentAccumulator,
}
#[derive(Debug)]
pub(crate) struct OpenAiChatStreamDecoder {
    selection: rust_provider_kit_core::ProviderSelection,
    response_id: Option<String>,
    usage: Option<rust_provider_kit_core::ProviderUsage>,
    finish_reason: Option<String>,
    reasoning_field: Option<String>,
    reasoning_text: String,
    tools: BTreeMap<usize, ChatToolState>,
    completed: bool,
}
impl OpenAiChatStreamDecoder {
    fn new(selection: rust_provider_kit_core::ProviderSelection) -> Self {
        Self {
            selection,
            response_id: None,
            usage: None,
            finish_reason: None,
            reasoning_field: None,
            reasoning_text: String::new(),
            tools: BTreeMap::new(),
            completed: false,
        }
    }
}
impl ProviderStreamDecoder for OpenAiChatStreamDecoder {
    fn consume(
        &mut self,
        event: &ServerSentEvent,
    ) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if event.data == "[DONE]" {
            return self.complete();
        }
        let root = rust_provider_kit_core::ProviderJsonValue::decode(event.data.as_bytes())
            .map_err(|_| {
                ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "OpenAI chat stream JSON is malformed",
                )
            })?;
        if root.get("error").is_some() {
            return Err(provider_stream_failure(
                event.data.as_bytes(),
                ProviderFailureCode::ServerFailed,
                "provider stream failed",
            ));
        }
        if let Some(value) = root.get("id") {
            self.response_id = optional_string(Some(value), "chat response id")?
                .map(str::to_owned)
                .or(self.response_id.take());
        }
        let has_usage = root.get("usage").is_some();
        if let Some(value) = root.get("usage") {
            self.usage = Some(parse_chat_usage(value)?);
        }
        let choices_value = match root.get("choices") {
            Some(value) => value,
            None if has_usage => return Ok(Vec::new()),
            None => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat stream event is missing choices",
                ));
            }
        };
        let choices = array(choices_value, "chat choices")?;
        let mut output = Vec::new();
        for choice in choices {
            let index =
                optional_nonnegative_u64(choice.get("index"), "chat choice index")?.unwrap_or(0);
            if index != 0 {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat stream returned multiple choices for a single-output request",
                ));
            }
            if let Some(reason) =
                optional_string(choice.get("finish_reason"), "chat finish reason")?
            {
                self.record_finish_reason(reason)?;
            }
            let delta = match choice.get("delta") {
                Some(value) => value,
                None if choice.get("finish_reason").is_some() => continue,
                None => {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::MalformedResponse,
                        "chat choice is missing delta",
                    ));
                }
            };
            if let Some(content) = optional_string(delta.get("content"), "chat content")?
                && !content.is_empty()
            {
                output.push(ProviderDecodedEvent::Text(content.to_owned()));
            }
            for field in ["reasoning_content", "reasoning", "reasoning_text"] {
                if let Some(content) = optional_string(delta.get(field), "chat reasoning")?
                    && !content.is_empty()
                {
                    if self
                        .reasoning_field
                        .as_deref()
                        .is_some_and(|current| current != field)
                    {
                        return Err(ProviderFailure::new(
                            ProviderFailureCode::MalformedResponse,
                            "chat stream changed reasoning field mid-response",
                        ));
                    }
                    self.reasoning_field = Some(field.to_owned());
                    self.reasoning_text.push_str(content);
                    output.push(ProviderDecodedEvent::Reasoning(content.to_owned()));
                    break;
                }
            }
            if let Some(raw_calls) = delta.get("tool_calls")
                && !matches!(raw_calls, ProviderJsonValue::Null)
            {
                for raw_call in array(raw_calls, "chat tool calls")? {
                    let index = usize::try_from(
                        optional_nonnegative_u64(raw_call.get("index"), "chat tool-call index")?
                            .unwrap_or(0),
                    )
                    .map_err(|_| {
                        ProviderFailure::new(
                            ProviderFailureCode::MalformedResponse,
                            "chat tool-call index is out of range",
                        )
                    })?;
                    if !self.tools.contains_key(&index)
                        && self.tools.len() >= ProviderTurnRequest::MAXIMUM_TOOLS
                    {
                        return Err(ProviderFailure::new(
                            ProviderFailureCode::MalformedResponse,
                            "chat stream exceeded tool-call state limit",
                        ));
                    }
                    let state = self.tools.entry(index).or_default();
                    if let Some(id) = optional_string(raw_call.get("id"), "chat tool call id")? {
                        state.id = Some(id.to_owned());
                    }
                    if let Some(function) = raw_call.get("function") {
                        if let Some(name) = optional_string(function.get("name"), "chat tool name")?
                        {
                            state.name = Some(name.to_owned());
                        }
                        if let Some(arguments) =
                            optional_string(function.get("arguments"), "chat tool arguments")?
                        {
                            state.arguments.append(arguments)?;
                        }
                    }
                }
            }
        }
        Ok(output)
    }
    fn finish(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        self.complete()
    }
}
impl OpenAiChatStreamDecoder {
    fn record_finish_reason(&mut self, value: &str) -> Result<(), ProviderFailure> {
        if self
            .finish_reason
            .as_deref()
            .is_some_and(|existing| existing != value)
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "chat stream emitted conflicting finish reasons",
            ));
        }
        self.finish_reason = Some(value.to_owned());
        Ok(())
    }
    fn complete(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if self.completed {
            return Ok(Vec::new());
        }
        let reason = self.finish_reason.as_deref().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "chat stream ended without a completion marker",
            )
        })?;
        match reason {
            "stop" if !self.tools.is_empty() => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat stream completed with stop while tool calls were pending",
                ));
            }
            "stop" => {}
            "tool_calls" | "function_call" if self.tools.is_empty() => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat stream declared tool completion without a tool call",
                ));
            }
            "tool_calls" | "function_call" => {}
            "length" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ServerFailed,
                    "chat response was truncated by its token limit",
                ));
            }
            "content_filter" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::PermissionDenied,
                    "chat response was blocked by content filtering",
                ));
            }
            "insufficient_system_resource" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ServerFailed,
                    "chat provider reported insufficient system resources",
                ));
            }
            _ => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat stream ended with an unsupported finish reason",
                ));
            }
        }
        self.completed = true;
        let mut output = Vec::new();
        for state in self.tools.values() {
            let id = state.id.as_deref().ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat tool call is incomplete",
                )
            })?;
            let name = state.name.as_deref().ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "chat tool call is incomplete",
                )
            })?;
            output.push(ProviderDecodedEvent::ToolCall(
                ProviderToolCall::new(id, name, state.arguments.decode_object()?)
                    .map_err(core_error_failure)?,
            ));
        }
        let native_state = match (&self.reasoning_field, self.reasoning_text.is_empty()) {
            (Some(field), false) => Some(
                ProviderNativeState::new(
                    "openai.chat.reasoning.v1",
                    serde_to_json(json!({
                        "provider":self.selection.provider_id().as_str(),
                        "model":self.selection.model_id().as_str(),
                        "field":field,
                        "content":self.reasoning_text.clone()
                    }))?,
                )
                .map_err(core_error_failure)?,
            ),
            _ => None,
        };
        output.push(ProviderDecodedEvent::Completion(ProviderCompletionDraft {
            response_id: self.response_id.clone(),
            continuation: None,
            native_state,
            usage: self.usage.clone(),
        }));
        Ok(output)
    }
}
fn parse_chat_usage(
    value: &ProviderJsonValue,
) -> Result<rust_provider_kit_core::ProviderUsage, ProviderFailure> {
    let input = optional_nonnegative_u64(
        value
            .get("prompt_tokens")
            .or_else(|| value.get("input_tokens")),
        "chat input token count",
    )?;
    let output = optional_nonnegative_u64(
        value
            .get("completion_tokens")
            .or_else(|| value.get("output_tokens")),
        "chat output token count",
    )?;
    let cached = optional_nonnegative_u64(
        value
            .at(&["prompt_tokens_details", "cached_tokens"])
            .or_else(|| value.get("cached_tokens")),
        "chat cached token count",
    )?;
    let total = optional_nonnegative_u64(value.get("total_tokens"), "chat total token count")?;
    usage(input, output, cached, total)?.ok_or_else(|| {
        ProviderFailure::new(
            ProviderFailureCode::MalformedResponse,
            "chat usage is empty",
        )
    })
}
