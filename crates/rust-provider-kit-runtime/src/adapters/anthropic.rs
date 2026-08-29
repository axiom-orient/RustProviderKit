use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use http::Method;
use rust_provider_kit_core::{
    BuiltInProviderId, CapabilitySupport, ProviderAccountInspection, ProviderAccountReadiness,
    ProviderCapabilities, ProviderCredentialLease, ProviderDescriptor, ProviderFailure,
    ProviderFailureCode, ProviderInstant, ProviderJsonValue, ProviderMessageContent,
    ProviderMessageRole, ProviderModelCatalogResult, ProviderOutputRequirement,
    ProviderProtocolFamily, ProviderReasoningPolicy, ProviderRequestConstraints, ProviderToolCall,
    ProviderToolChoice, ProviderTurnRequest,
};
use serde_json::{Map, Value, json};

use crate::adapter::{ProviderAdapter, ProviderStreamDecoder};
use crate::http_transport::{ProviderHttpRequest, ProviderHttpTransport};
use crate::registry::ProviderEndpointCatalog;
use crate::sse::ServerSentEvent;
use crate::wire::{
    ProviderCompletionDraft, ProviderDecodedEvent, ProviderToolArgumentAccumulator, append_path,
    core_error_failure, default_capabilities, http_failure, json_to_serde, make_json_request,
    merge_account_headers, optional_nonnegative_u64, parse_model_catalog, provider_stream_failure,
    require_api_key, serde_to_json, tool_result_text, transport_failure, usage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnthropicMessagesKind {
    Anthropic,
    Zai,
    MiniMax,
}

#[derive(Debug, Clone)]
pub(crate) struct AnthropicMessagesAdapter {
    kind: AnthropicMessagesKind,
    descriptor: ProviderDescriptor,
}
impl AnthropicMessagesAdapter {
    pub(crate) fn new(kind: AnthropicMessagesKind) -> Result<Self, ProviderFailure> {
        let descriptor = match kind {
            AnthropicMessagesKind::Anthropic => ProviderDescriptor::new(
                BuiltInProviderId::anthropic(),
                "Anthropic",
                ProviderProtocolFamily::AnthropicMessages,
                true,
                false,
                false,
            ),
            AnthropicMessagesKind::Zai => ProviderDescriptor::new(
                BuiltInProviderId::zai(),
                "Z.AI",
                ProviderProtocolFamily::AnthropicMessages,
                true,
                false,
                false,
            ),
            AnthropicMessagesKind::MiniMax => ProviderDescriptor::new(
                BuiltInProviderId::mini_max(),
                "MiniMax",
                ProviderProtocolFamily::AnthropicMessages,
                true,
                false,
                false,
            ),
        }
        .map_err(core_error_failure)?;
        Ok(Self { kind, descriptor })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        if self.kind == AnthropicMessagesKind::Anthropic {
            let mut value = default_capabilities(CapabilitySupport::Declared(
                rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
            ));
            value.reasoning_continuity = CapabilitySupport::Declared(
                rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
            );
            value
        } else {
            default_capabilities(CapabilitySupport::Unknown)
        }
    }
    fn execution_base_url(
        &self,
        lease: &ProviderCredentialLease,
    ) -> Result<url::Url, ProviderFailure> {
        if let Some(endpoint) = lease.record().endpoint() {
            return Ok(endpoint.base_url().clone());
        }
        match self.kind {
            AnthropicMessagesKind::Anthropic => ProviderEndpointCatalog::anthropic(),
            AnthropicMessagesKind::Zai => ProviderEndpointCatalog::zai_anthropic(),
            AnthropicMessagesKind::MiniMax => ProviderEndpointCatalog::mini_max(),
        }
    }

    fn model_base_url(&self, lease: &ProviderCredentialLease) -> Result<url::Url, ProviderFailure> {
        if let Some(endpoint) = lease.record().endpoint() {
            return Ok(endpoint.base_url().clone());
        }
        match self.kind {
            AnthropicMessagesKind::Anthropic => ProviderEndpointCatalog::anthropic(),
            AnthropicMessagesKind::Zai => ProviderEndpointCatalog::zai_models(),
            AnthropicMessagesKind::MiniMax => ProviderEndpointCatalog::mini_max_models(),
        }
    }
    fn headers(&self, key: &str) -> BTreeMap<String, String> {
        match self.kind {
            AnthropicMessagesKind::Anthropic => BTreeMap::from([
                ("x-api-key".into(), key.to_owned()),
                ("anthropic-version".into(), "2023-06-01".into()),
                ("accept".into(), "text/event-stream".into()),
            ]),
            AnthropicMessagesKind::Zai | AnthropicMessagesKind::MiniMax => BTreeMap::from([
                ("authorization".into(), format!("Bearer {key}")),
                ("anthropic-version".into(), "2023-06-01".into()),
                ("accept".into(), "text/event-stream".into()),
            ]),
        }
    }
    fn encode_request(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<ProviderJsonValue, ProviderFailure> {
        if request.continuation().is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "Anthropic Messages does not accept a Responses continuation",
            ));
        }
        if self.kind != AnthropicMessagesKind::Anthropic
            && matches!(
                request.output(),
                ProviderOutputRequirement::JsonSchema { .. }
            )
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "native JSON Schema output is not qualified for this Messages provider",
            ));
        }
        if self.kind != AnthropicMessagesKind::Anthropic
            && matches!(request.reasoning(), ProviderReasoningPolicy::Effort(_))
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "explicit reasoning effort is not qualified for this Messages provider",
            ));
        }
        let mut system = String::new();
        let mut messages = Vec::new();
        for message in request.messages() {
            if message
                .content()
                .iter()
                .any(|item| item.native_state_value().is_some())
            {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::CapabilityMismatch,
                    "Anthropic Messages does not accept native state from another provider route",
                ));
            }
            if matches!(
                message.role(),
                ProviderMessageRole::System | ProviderMessageRole::Developer
            ) {
                for text in message
                    .content()
                    .iter()
                    .filter_map(ProviderMessageContent::text_value)
                {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(text);
                }
                continue;
            }
            if message.role() == ProviderMessageRole::Tool {
                let mut blocks = Vec::new();
                for item in message.content() {
                    if let Some((call_id, _, value)) = item.tool_result_value() {
                        let content = tool_result_text(value)?;
                        blocks.push(json!({
                            "type":"tool_result",
                            "tool_use_id":call_id,
                            "content":content,
                            "is_error":item.tool_result_is_error().unwrap_or(false)
                        }));
                    }
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role":"user","content":blocks}));
                }
                continue;
            }
            let role = if message.role() == ProviderMessageRole::Assistant {
                "assistant"
            } else {
                "user"
            };
            let mut blocks = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::text_value)
                .map(|text| json!({"type":"text","text":text}))
                .collect::<Vec<_>>();
            for item in message.content() {
                if let Some(call) = item.tool_call_value() {
                    blocks.push(json!({
                        "type":"tool_use",
                        "id":call.id(),
                        "name":call.name(),
                        "input":json_to_serde(call.arguments())?
                    }));
                }
            }
            if !blocks.is_empty() {
                messages.push(json!({"role":role,"content":blocks}));
            }
        }
        let mut body = Map::new();
        body.insert(
            "model".into(),
            Value::String(request.selection().model_id().as_str().to_owned()),
        );
        body.insert(
            "max_tokens".into(),
            json!(
                request
                    .constraints()
                    .maximum_output_tokens()
                    .unwrap_or(16_384)
            ),
        );
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), Value::Bool(true));
        if !system.is_empty() {
            body.insert("system".into(), Value::String(system));
        }
        if !request.tools().is_empty() {
            let tools = request
                .tools()
                .iter()
                .map(|tool| -> Result<Value, ProviderFailure> {
                    let mut object = Map::new();
                    object.insert("name".into(), Value::String(tool.name().to_owned()));
                    object.insert(
                        "description".into(),
                        Value::String(tool.description().to_owned()),
                    );
                    object.insert("input_schema".into(), json_to_serde(tool.input_schema())?);
                    if self.kind == AnthropicMessagesKind::Anthropic {
                        object.insert("strict".into(), Value::Bool(tool.strict()));
                    }
                    Ok(Value::Object(object))
                })
                .collect::<Result<Vec<_>, _>>()?;
            body.insert("tools".into(), Value::Array(tools));
            body.insert(
                "tool_choice".into(),
                match request.tool_choice() {
                    ProviderToolChoice::Automatic => json!({"type":"auto"}),
                    ProviderToolChoice::Required => json!({"type":"any"}),
                    ProviderToolChoice::Named { name } => json!({"type":"tool","name":name}),
                },
            );
        }
        let mut output_config = Map::new();
        match request.output() {
            ProviderOutputRequirement::Text => {}
            ProviderOutputRequirement::ApplicationValidatedJson { schema, .. } => {
                if self.kind == AnthropicMessagesKind::Anthropic {
                    output_config.insert(
                        "format".into(),
                        json!({"type":"json_schema","schema":json_to_serde(schema)?}),
                    );
                }
            }
            ProviderOutputRequirement::JsonSchema { schema, strict, .. } => {
                if self.kind != AnthropicMessagesKind::Anthropic || !strict {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "Anthropic structured output requires a strict native schema",
                    ));
                }
                output_config.insert(
                    "format".into(),
                    json!({"type":"json_schema","schema":json_to_serde(schema)?}),
                );
            }
        }
        match request.reasoning() {
            ProviderReasoningPolicy::Automatic => {}
            ProviderReasoningPolicy::Disabled if self.kind == AnthropicMessagesKind::Anthropic => {
                body.insert("thinking".into(), json!({"type":"disabled"}));
            }
            ProviderReasoningPolicy::Disabled => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::CapabilityMismatch,
                    "explicit reasoning control is not qualified for this Messages provider",
                ));
            }
            ProviderReasoningPolicy::Effort(effort)
                if self.kind == AnthropicMessagesKind::Anthropic =>
            {
                body.insert(
                    "thinking".into(),
                    json!({"type":"adaptive","display":"summarized"}),
                );
                output_config.insert("effort".into(), Value::String(effort.as_str().to_owned()));
            }
            ProviderReasoningPolicy::Effort(_) => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::CapabilityMismatch,
                    "explicit reasoning control is not qualified for this Messages provider",
                ));
            }
        }
        if !output_config.is_empty() {
            body.insert("output_config".into(), Value::Object(output_config));
        }
        serde_to_json(Value::Object(body))
    }
}

#[async_trait]
impl ProviderAdapter for AnthropicMessagesAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    async fn make_execution_request(
        &self,
        request: &ProviderTurnRequest,
        credential: &ProviderCredentialLease,
    ) -> Result<ProviderHttpRequest, ProviderFailure> {
        let key = require_api_key(credential)?;
        let endpoint = append_path("/v1/messages", &self.execution_base_url(credential)?)?;
        make_json_request(
            Method::POST,
            endpoint,
            merge_account_headers(self.headers(key), credential),
            &self.encode_request(request)?,
            request.constraints(),
        )
    }
    fn make_decoder(
        &self,
        _request: &ProviderTurnRequest,
    ) -> Result<Box<dyn ProviderStreamDecoder>, ProviderFailure> {
        Ok(Box::new(AnthropicMessagesStreamDecoder::default()))
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
        let key = require_api_key(credential)?;
        let endpoint = append_path("/v1/models", &self.model_base_url(credential)?)?;
        let mut headers = self.headers(key);
        headers.insert("accept".into(), "application/json".into());
        if self.kind == AnthropicMessagesKind::MiniMax {
            headers.remove("anthropic-version");
        }
        let headers = merge_account_headers(headers, credential);
        let constraints = ProviderRequestConstraints::new(
            rust_provider_kit_core::ProviderDataCollectionPolicy::Deny,
            true,
            true,
            60_000,
            8 * 1_024 * 1_024,
            1,
            None,
        )
        .map_err(core_error_failure)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
    Ignored,
}
#[derive(Debug)]
struct AnthropicToolState {
    id: String,
    name: String,
    arguments: ProviderToolArgumentAccumulator,
    initial_input: Option<ProviderJsonValue>,
}
#[derive(Debug, Default)]
pub(crate) struct AnthropicMessagesStreamDecoder {
    response_id: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    tools: HashMap<usize, AnthropicToolState>,
    active_blocks: HashMap<usize, BlockKind>,
    emitted_tool_count: usize,
    message_started: bool,
    stop_reason: Option<String>,
    stopped: bool,
}
impl ProviderStreamDecoder for AnthropicMessagesStreamDecoder {
    fn consume(
        &mut self,
        event: &ServerSentEvent,
    ) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        let root = ProviderJsonValue::decode(event.data.as_bytes()).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "Anthropic stream JSON is malformed",
            )
        })?;
        let event_type = root
            .get("type")
            .and_then(ProviderJsonValue::as_str)
            .or(event.event.as_deref());
        match event_type {
            Some("message_start") => {
                if self.message_started || self.stopped {
                    return Err(malformed("Messages stream emitted duplicate message_start"));
                }
                self.message_started = true;
                self.response_id = root
                    .at(&["message", "id"])
                    .and_then(ProviderJsonValue::as_str)
                    .map(str::to_owned);
                if let Some(value) = root.at(&["message", "usage"]) {
                    self.update_usage(value)?;
                }
                Ok(Vec::new())
            }
            Some("content_block_start") => {
                if !self.message_started {
                    return Err(malformed(
                        "Messages stream content block started before message_start",
                    ));
                }
                let index = index(&root)?;
                if self.active_blocks.contains_key(&index) {
                    return Err(malformed(
                        "Messages stream has a duplicate content block start",
                    ));
                }
                let block_type = root
                    .at(&["content_block", "type"])
                    .and_then(ProviderJsonValue::as_str)
                    .ok_or_else(|| malformed("Messages content block has no type"))?;
                let kind = match block_type {
                    "text" => BlockKind::Text,
                    "thinking" | "redacted_thinking" | "fallback" => BlockKind::Ignored,
                    "tool_use" => {
                        let id = root
                            .at(&["content_block", "id"])
                            .and_then(ProviderJsonValue::as_str)
                            .ok_or_else(|| malformed("Messages tool block has no ID"))?;
                        let name = root
                            .at(&["content_block", "name"])
                            .and_then(ProviderJsonValue::as_str)
                            .ok_or_else(|| malformed("Messages tool block has no name"))?;
                        let initial_input = root.at(&["content_block", "input"]).cloned();
                        if self.emitted_tool_count.saturating_add(self.tools.len())
                            >= ProviderTurnRequest::MAXIMUM_TOOLS
                        {
                            return Err(malformed(
                                "Messages stream exceeded tool-call state limit",
                            ));
                        }
                        if self
                            .tools
                            .insert(
                                index,
                                AnthropicToolState {
                                    id: id.to_owned(),
                                    name: name.to_owned(),
                                    arguments: ProviderToolArgumentAccumulator::with_default_bound(
                                    ),
                                    initial_input,
                                },
                            )
                            .is_some()
                        {
                            return Err(malformed("Messages tool block was duplicated"));
                        }
                        BlockKind::Tool
                    }
                    _ => BlockKind::Ignored,
                };
                self.active_blocks.insert(index, kind);
                Ok(Vec::new())
            }
            Some("content_block_delta") => {
                let index = index(&root)?;
                let kind = *self.active_blocks.get(&index).ok_or_else(|| {
                    malformed("Messages stream delta has no active content block")
                })?;
                let delta_type = root
                    .at(&["delta", "type"])
                    .and_then(ProviderJsonValue::as_str);
                match (kind, delta_type) {
                    (BlockKind::Text, Some("text_delta")) => {
                        let text = root
                            .at(&["delta", "text"])
                            .and_then(ProviderJsonValue::as_str)
                            .ok_or_else(|| malformed("Messages text delta has no text"))?;
                        Ok(if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![ProviderDecodedEvent::Text(text.to_owned())]
                        })
                    }
                    (BlockKind::Tool, Some("input_json_delta")) => {
                        let partial = root
                            .at(&["delta", "partial_json"])
                            .and_then(ProviderJsonValue::as_str)
                            .ok_or_else(|| malformed("Messages tool delta has no partial JSON"))?;
                        self.tools
                            .get_mut(&index)
                            .ok_or_else(|| {
                                malformed("Messages tool delta has no active tool state")
                            })?
                            .arguments
                            .append(partial)?;
                        Ok(Vec::new())
                    }
                    (BlockKind::Ignored, _) => Ok(Vec::new()),
                    _ => Err(malformed(
                        "Messages content delta does not match its active block type",
                    )),
                }
            }
            Some("content_block_stop") => {
                let index = index(&root)?;
                let kind = self
                    .active_blocks
                    .remove(&index)
                    .ok_or_else(|| malformed("Messages stream stopped an unknown content block"))?;
                if kind != BlockKind::Tool {
                    return Ok(Vec::new());
                }
                let tool = self
                    .tools
                    .remove(&index)
                    .ok_or_else(|| malformed("Messages tool block lost its state"))?;
                let arguments = if tool.arguments.byte_count() > 0 {
                    tool.arguments.decode_object()?
                } else if let Some(value) = tool.initial_input {
                    if value.as_object().is_none() {
                        return Err(malformed("Messages tool input is not a JSON object"));
                    }
                    value
                } else {
                    ProviderJsonValue::Object(BTreeMap::new())
                };
                self.emitted_tool_count = self
                    .emitted_tool_count
                    .checked_add(1)
                    .ok_or_else(|| malformed("Anthropic emitted tool-call count overflow"))?;
                Ok(vec![ProviderDecodedEvent::ToolCall(
                    ProviderToolCall::new(tool.id, tool.name, arguments)
                        .map_err(core_error_failure)?,
                )])
            }
            Some("message_delta") => {
                if !self.message_started || !self.active_blocks.is_empty() {
                    return Err(malformed(
                        "Messages stream ended before its content blocks stopped",
                    ));
                }
                if let Some(value) = root.get("usage") {
                    self.update_usage(value)?;
                }
                if let Some(reason) = root
                    .at(&["delta", "stop_reason"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    if self
                        .stop_reason
                        .as_deref()
                        .is_some_and(|existing| existing != reason)
                    {
                        return Err(malformed(
                            "Messages stream emitted conflicting stop reasons",
                        ));
                    }
                    self.stop_reason = Some(reason.to_owned());
                }
                Ok(Vec::new())
            }
            Some("message_stop") => self.complete(),
            Some("error") => Err(provider_stream_failure(
                event.data.as_bytes(),
                ProviderFailureCode::TransportFailed,
                "Messages stream failed",
            )),
            Some("ping") => Ok(Vec::new()),
            None => Err(malformed("Messages SSE event has no type")),
            Some(_) => Ok(Vec::new()),
        }
    }
    fn finish(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if self.stopped {
            Ok(Vec::new())
        } else {
            Err(malformed("Messages stream ended before message_stop"))
        }
    }
}
impl AnthropicMessagesStreamDecoder {
    fn update_usage(&mut self, value: &ProviderJsonValue) -> Result<(), ProviderFailure> {
        if let Some(input) =
            optional_nonnegative_u64(value.get("input_tokens"), "Messages input token count")?
        {
            self.input_tokens = Some(input);
        }
        if let Some(output) =
            optional_nonnegative_u64(value.get("output_tokens"), "Messages output token count")?
        {
            self.output_tokens = Some(output);
        }
        if let Some(cached) = optional_nonnegative_u64(
            value.get("cache_read_input_tokens"),
            "Messages cached token count",
        )? {
            self.cached_tokens = Some(cached);
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if self.stopped {
            return Err(malformed("Messages stream emitted duplicate message_stop"));
        }
        if !self.message_started || !self.active_blocks.is_empty() || !self.tools.is_empty() {
            return Err(malformed(
                "Messages stream ended with an incomplete tool call",
            ));
        }
        let reason = self
            .stop_reason
            .as_deref()
            .ok_or_else(|| malformed("Messages stream ended without a stop reason"))?;
        match reason {
            "end_turn" | "stop_sequence" => {}
            "tool_use" if self.emitted_tool_count > 0 => {}
            "tool_use" => {
                return Err(malformed(
                    "Messages stream declared tool use without a tool call",
                ));
            }
            "max_tokens" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ServerFailed,
                    "Messages response was truncated by its token limit",
                ));
            }
            "refusal" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::PermissionDenied,
                    "Messages provider refused the response",
                ));
            }
            "pause_turn" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::CapabilityMismatch,
                    "Messages pause_turn continuation is not supported",
                ));
            }
            "model_context_window_exceeded" => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "Messages request exceeded the model context window",
                ));
            }
            _ => {
                return Err(malformed(
                    "Messages stream ended with an unsupported stop reason",
                ));
            }
        }
        self.stopped = true;
        let total = match (self.input_tokens, self.output_tokens) {
            (Some(left), Some(right)) => Some(
                left.checked_add(right)
                    .ok_or_else(|| malformed("Messages token usage overflowed"))?,
            ),
            _ => None,
        };
        let usage = usage(
            self.input_tokens,
            self.output_tokens,
            self.cached_tokens,
            total,
        )?;
        Ok(vec![ProviderDecodedEvent::Completion(
            ProviderCompletionDraft {
                response_id: self.response_id.clone(),
                continuation: None,
                native_state: None,
                usage,
            },
        )])
    }
}
fn index(root: &ProviderJsonValue) -> Result<usize, ProviderFailure> {
    let value = optional_nonnegative_u64(root.get("index"), "Messages content block index")?
        .ok_or_else(|| malformed("Messages content block index is missing"))?;
    usize::try_from(value).map_err(|_| malformed("Messages content block index is out of range"))
}
fn malformed(message: impl Into<String>) -> ProviderFailure {
    let message = message.into();
    ProviderFailure::new(ProviderFailureCode::MalformedResponse, &message)
}
