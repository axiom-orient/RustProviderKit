use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use http::Method;
use rust_provider_kit_core::{
    BuiltInProviderId, CapabilitySupport, ProviderAccountInspection, ProviderAccountReadiness,
    ProviderCredentialLease, ProviderCredentialMaterial, ProviderDescriptor, ProviderFailure,
    ProviderFailureCode, ProviderInstant, ProviderJsonValue, ProviderMessageContent,
    ProviderMessageRole, ProviderModelCatalogResult, ProviderNativeState,
    ProviderOutputRequirement, ProviderProtocolFamily, ProviderReasoningPolicy,
    ProviderRequestConstraints, ProviderSelection, ProviderToolCall, ProviderToolChoice,
    ProviderTurnRequest, SensitiveValue,
};
use serde_json::{Map, Value, json};

use crate::adapter::{ProviderAdapter, ProviderStreamDecoder};
use crate::codex_version::{CODEX_CLIENT_VERSION_OVERRIDE, codex_client_version};
use crate::http_transport::{ProviderHttpRequest, ProviderHttpTransport};
use crate::registry::ProviderEndpointCatalog;
use crate::secure_file::SecureRegularFileReader;
use crate::sse::ServerSentEvent;
use crate::wire::{
    ProviderCompletionDraft, ProviderDecodedEvent, ProviderToolArgumentAccumulator, append_path,
    core_error_failure, default_capabilities, http_failure, json_to_serde, make_json_request,
    malformed, merge_account_headers, optional_nonnegative_u64, parse_model_catalog,
    provider_stream_failure, require_oauth_bearer, require_server_side_continuation_opt_in,
    serde_to_json, stores_server_side_response, tool_result_text, transport_failure, usage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiResponsesKind {
    Codex,
    OpenAi,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenAiResponsesAdapter {
    kind: OpenAiResponsesKind,
    descriptor: ProviderDescriptor,
    /// Explicitly configured Codex client version, if the caller set one.
    client_version: Option<String>,
}
#[allow(clippy::unused_self)]
impl OpenAiResponsesAdapter {
    /// Build the Codex adapter with the client version this process declares.
    pub(crate) fn codex(client_version: Option<String>) -> Result<Self, ProviderFailure> {
        if let Some(value) = client_version.as_deref() {
            crate::codex_version::validate_client_version(value)?;
        }
        let mut adapter = Self::new(OpenAiResponsesKind::Codex)?;
        adapter.client_version = client_version;
        Ok(adapter)
    }
    pub(crate) fn new(kind: OpenAiResponsesKind) -> Result<Self, ProviderFailure> {
        let descriptor = match kind {
            OpenAiResponsesKind::Codex => ProviderDescriptor::new(
                BuiltInProviderId::codex(),
                "Codex (ChatGPT subscription)",
                ProviderProtocolFamily::CodexResponses,
                true,
                false,
            ),
            OpenAiResponsesKind::OpenAi => ProviderDescriptor::new(
                BuiltInProviderId::open_ai(),
                "OpenAI",
                ProviderProtocolFamily::OpenAiResponses,
                true,
                false,
            ),
        }
        .map_err(core_error_failure)?;
        Ok(Self {
            kind,
            descriptor,
            client_version: None,
        })
    }
    fn capabilities(&self) -> rust_provider_kit_core::ProviderCapabilities {
        let mut value = default_capabilities(CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        ));
        value.reasoning_continuity = CapabilitySupport::Declared(
            rust_provider_kit_core::ProviderCapabilitySource::ProviderDocumentation,
        );
        value
    }
    fn supports_server_side_continuation(&self) -> bool {
        self.kind == OpenAiResponsesKind::OpenAi
    }
    fn encode_request(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<ProviderJsonValue, ProviderFailure> {
        if self.supports_server_side_continuation() {
            require_server_side_continuation_opt_in(request)?;
        }
        let mut input = Vec::new();
        let mut instructions = String::new();
        for message in request.messages() {
            if self.kind == OpenAiResponsesKind::Codex
                && matches!(
                    message.role(),
                    ProviderMessageRole::System | ProviderMessageRole::Developer
                )
            {
                for text in message
                    .content()
                    .iter()
                    .filter_map(ProviderMessageContent::text_value)
                {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(text);
                }
                continue;
            }
            if let Some(native_state) = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::native_state_value)
                .next()
            {
                if native_state.format() != "openai.responses.output.v1" {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::CapabilityMismatch,
                        "Responses native state format does not match this provider route",
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
                        "Responses native state is bound to a different provider or model",
                    ));
                }
                let output = native_state
                    .payload()
                    .get("output")
                    .and_then(ProviderJsonValue::as_array)
                    .ok_or_else(|| {
                        ProviderFailure::new(
                            ProviderFailureCode::MalformedResponse,
                            "Responses native state payload must contain an output array",
                        )
                    })?;
                input.extend(
                    output
                        .iter()
                        .map(json_to_serde)
                        .collect::<Result<Vec<_>, _>>()?,
                );
                continue;
            }
            if message.role() == ProviderMessageRole::Tool {
                for content in message.content() {
                    if let Some((call_id, _, value)) = content.tool_result_value() {
                        let output = tool_result_text(value)?;
                        input.push(json!({"type":"function_call_output","call_id":call_id,"output":output}));
                    }
                }
                continue;
            }
            let content_type = if message.role() == ProviderMessageRole::Assistant {
                "output_text"
            } else {
                "input_text"
            };
            let items = message
                .content()
                .iter()
                .filter_map(ProviderMessageContent::text_value)
                .map(|text| json!({"type":content_type,"text":text}))
                .collect::<Vec<_>>();
            if !items.is_empty() {
                let role = match message.role() {
                    ProviderMessageRole::System => "system",
                    ProviderMessageRole::Developer => "developer",
                    ProviderMessageRole::User => "user",
                    ProviderMessageRole::Assistant => "assistant",
                    ProviderMessageRole::Tool => "tool",
                };
                input.push(json!({"type":"message","role":role,"content":items}));
            }
            for content in message.content() {
                if let Some(call) = content.tool_call_value() {
                    let arguments = String::from_utf8(
                        call.arguments().encoded_vec().map_err(core_error_failure)?,
                    )
                    .map_err(|_| {
                        ProviderFailure::new(
                            ProviderFailureCode::InternalInvariant,
                            "encoded JSON is not UTF-8",
                        )
                    })?;
                    input.push(json!({
                        "type":"function_call",
                        "call_id":call.id(),
                        "name":call.name(),
                        "arguments":arguments
                    }));
                }
            }
        }
        let mut body = Map::new();
        body.insert(
            "model".into(),
            Value::String(request.selection().model_id().as_str().to_owned()),
        );
        body.insert("input".into(), Value::Array(input));
        body.insert("stream".into(), Value::Bool(true));
        body.insert(
            "store".into(),
            Value::Bool(
                self.supports_server_side_continuation() && stores_server_side_response(request),
            ),
        );
        if self.kind == OpenAiResponsesKind::OpenAi
            && let Some(limit) = request.constraints().maximum_output_tokens()
        {
            body.insert("max_output_tokens".into(), json!(limit));
        }
        if self.kind == OpenAiResponsesKind::Codex && !instructions.is_empty() {
            body.insert("instructions".into(), Value::String(instructions));
        }
        if !request.tools().is_empty() {
            let tools = request.tools().iter().map(|tool| -> Result<Value, ProviderFailure> {
                Ok(json!({"type":"function","name":tool.name(),"description":tool.description(),"parameters":json_to_serde(tool.input_schema())?,"strict":tool.strict()}))
            }).collect::<Result<Vec<_>, _>>()?;
            body.insert("tools".into(), Value::Array(tools));
            body.insert(
                "tool_choice".into(),
                match request.tool_choice() {
                    ProviderToolChoice::Automatic => Value::String("auto".into()),
                    ProviderToolChoice::Required => Value::String("required".into()),
                    ProviderToolChoice::Named { name } => json!({"type":"function","name":name}),
                },
            );
            body.insert("parallel_tool_calls".into(), Value::Bool(false));
        }
        match request.output() {
            ProviderOutputRequirement::Text => {}
            ProviderOutputRequirement::ApplicationValidatedJson { name, schema } => {
                body.insert("text".into(), json!({"format":{"type":"json_schema","name":name,"schema":json_to_serde(schema)?,"strict":true}}));
            }
            ProviderOutputRequirement::JsonSchema {
                name,
                schema,
                strict,
            } => {
                body.insert("text".into(), json!({"format":{"type":"json_schema","name":name,"schema":json_to_serde(schema)?,"strict":strict}}));
            }
        }
        match request.reasoning() {
            ProviderReasoningPolicy::Automatic => {}
            ProviderReasoningPolicy::Disabled => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::CapabilityMismatch,
                    "disabling reasoning is not qualified for the Responses dialect",
                ));
            }
            ProviderReasoningPolicy::Effort(effort) => {
                body.insert("reasoning".into(), json!({"effort":effort.as_str()}));
            }
        }
        if self.supports_server_side_continuation()
            && let Some(continuation) = request.continuation()
        {
            body.insert(
                "previous_response_id".into(),
                Value::String(continuation.value().to_owned()),
            );
        }
        serde_to_json(Value::Object(body))
    }
    async fn resolved_credential(
        &self,
        lease: &ProviderCredentialLease,
    ) -> Result<CodexResolvedCredential, ProviderFailure> {
        let ProviderCredentialMaterial::ExternalAuthFile(path) = lease.material() else {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "Codex requires an external auth.json reference",
            ));
        };
        let data = SecureRegularFileReader::read_async(path, 1_024 * 1_024).await?;
        let root = ProviderJsonValue::decode(&data).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "Codex auth.json is invalid",
            )
        })?;
        let token = root
            .at(&["tokens", "access_token"])
            .and_then(ProviderJsonValue::as_str)
            .ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::AuthenticationFailed,
                    "Codex auth.json does not contain an access token",
                )
            })?;
        let account_id = root
            .at(&["tokens", "account_id"])
            .and_then(ProviderJsonValue::as_str)
            .ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::AuthenticationFailed,
                    "Codex auth.json does not contain an account ID",
                )
            })?;
        if account_id.is_empty()
            || account_id.len() > 512
            || account_id.trim() != account_id
            || account_id.chars().any(char::is_control)
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "Codex auth.json account ID is invalid",
            ));
        }
        let access_token = SensitiveValue::new(token).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "Codex auth.json access token is invalid",
            )
        })?;
        let client_version = codex_client_version(self.client_version.as_deref())?;
        Ok(CodexResolvedCredential {
            access_token,
            account_id: account_id.to_owned(),
            client_version,
        })
    }
    async fn endpoint_headers(
        &self,
        lease: &ProviderCredentialLease,
        models: bool,
    ) -> Result<(url::Url, BTreeMap<String, String>), ProviderFailure> {
        match self.kind {
            OpenAiResponsesKind::OpenAi => {
                let key = require_oauth_bearer(lease)?;
                let base = match lease.record().endpoint() {
                    Some(value) => value.base_url().clone(),
                    None => ProviderEndpointCatalog::open_ai()?,
                };
                let path = if lease.record().endpoint().is_some() {
                    if models { "/models" } else { "/responses" }
                } else if models {
                    "/v1/models"
                } else {
                    "/v1/responses"
                };
                let headers = merge_account_headers(
                    BTreeMap::from([
                        ("authorization".into(), format!("Bearer {key}")),
                        ("accept".into(), "text/event-stream".into()),
                    ]),
                    lease,
                );
                Ok((append_path(path, &base)?, headers))
            }
            OpenAiResponsesKind::Codex => {
                let auth = self.resolved_credential(lease).await?;
                let base = match lease.record().endpoint() {
                    Some(value) => value.base_url().clone(),
                    None => ProviderEndpointCatalog::codex()?,
                };
                let mut endpoint =
                    append_path(if models { "/models" } else { "/responses" }, &base)?;
                if models {
                    endpoint
                        .query_pairs_mut()
                        .append_pair("client_version", &auth.client_version);
                }
                let headers = merge_account_headers(
                    BTreeMap::from([
                        (
                            "authorization".into(),
                            format!("Bearer {}", auth.access_token.expose()),
                        ),
                        ("chatgpt-account-id".into(), auth.account_id),
                        ("originator".into(), "codex_cli_rs".into()),
                        ("version".into(), auth.client_version.clone()),
                        (
                            "user-agent".into(),
                            format!("codex-cli/{}", auth.client_version),
                        ),
                        ("accept".into(), "text/event-stream".into()),
                    ]),
                    lease,
                );
                Ok((endpoint, headers))
            }
        }
    }
}
struct CodexResolvedCredential {
    access_token: SensitiveValue,
    account_id: String,
    client_version: String,
}

#[async_trait]
impl ProviderAdapter for OpenAiResponsesAdapter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    async fn make_execution_request(
        &self,
        request: &ProviderTurnRequest,
        credential: &ProviderCredentialLease,
    ) -> Result<ProviderHttpRequest, ProviderFailure> {
        if self.kind == OpenAiResponsesKind::Codex && request.continuation().is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "Codex Responses continuation is not qualified",
            ));
        }
        if self.kind == OpenAiResponsesKind::Codex
            && request.constraints().maximum_output_tokens().is_some()
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CapabilityMismatch,
                "Codex Responses maximum output tokens are not qualified",
            ));
        }
        let (endpoint, headers) = self.endpoint_headers(credential, false).await?;
        make_json_request(
            Method::POST,
            endpoint,
            headers,
            &self.encode_request(request)?,
            request.constraints(),
        )
    }
    /// Name the declared client version when the endpoint refuses the request.
    ///
    /// The Codex endpoint enforces a minimum client version and can reject an
    /// older one with a bare 400. This context states the value declared by
    /// this route without claiming that every 4xx has that cause.
    fn failure_context(&self, status: u16) -> Option<String> {
        if self.kind != OpenAiResponsesKind::Codex || !(400..500).contains(&status) {
            return None;
        }
        let declared = codex_client_version(self.client_version.as_deref()).ok()?;
        Some(format!(
            "declared codex client version {declared}; if this endpoint now requires a newer client, configure a newer client version or set {CODEX_CLIENT_VERSION_OVERRIDE}"
        ))
    }
    fn make_decoder(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<Box<dyn ProviderStreamDecoder>, ProviderFailure> {
        Ok(Box::new(OpenAiResponsesStreamDecoder::new(
            request.selection().clone(),
            self.supports_server_side_continuation() && stores_server_side_response(request),
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
        let (endpoint, mut headers) = self.endpoint_headers(credential, true).await?;
        headers.insert("accept".into(), "application/json".into());
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
            &["id", "slug"],
            &self.capabilities(),
            now,
        )
    }
}

#[derive(Debug, Default)]
struct ResponsesToolState {
    call_id: Option<String>,
    name: Option<String>,
    arguments: ProviderToolArgumentAccumulator,
    emitted: bool,
}
#[derive(Debug)]
pub(crate) struct OpenAiResponsesStreamDecoder {
    selection: ProviderSelection,
    exposes_continuation: bool,
    response_id: Option<String>,
    usage: Option<rust_provider_kit_core::ProviderUsage>,
    tools: HashMap<String, ResponsesToolState>,
    completed: bool,
}
impl OpenAiResponsesStreamDecoder {
    #[must_use]
    pub(crate) fn new(selection: ProviderSelection, exposes_continuation: bool) -> Self {
        Self {
            selection,
            exposes_continuation,
            response_id: None,
            usage: None,
            tools: HashMap::new(),
            completed: false,
        }
    }
}
impl ProviderStreamDecoder for OpenAiResponsesStreamDecoder {
    fn consume(
        &mut self,
        event: &ServerSentEvent,
    ) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if event.data == "[DONE]" {
            return Ok(Vec::new());
        }
        let root = ProviderJsonValue::decode(event.data.as_bytes()).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "OpenAI Responses stream JSON is malformed",
            )
        })?;
        let event_type = root
            .get("type")
            .and_then(ProviderJsonValue::as_str)
            .or(event.event.as_deref());
        match event_type {
            Some("response.created" | "response.in_progress") => {
                let id = root
                    .at(&["response", "id"])
                    .and_then(ProviderJsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed("Responses lifecycle event has no response ID"))?;
                self.response_id = Some(id.to_owned());
                Ok(Vec::new())
            }
            Some("response.output_text.delta") => {
                let value = root
                    .get("delta")
                    .and_then(ProviderJsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed("Responses text delta has no text"))?;
                Ok(vec![ProviderDecodedEvent::Text(value.to_owned())])
            }
            // The reasoning text delta is provider-private chain-of-thought,
            // not the displayable summary contract exposed by ProviderKit.
            Some("response.reasoning_text.delta") => {
                let _ = root
                    .get("delta")
                    .and_then(ProviderJsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed("Responses private reasoning delta has no text"))?;
                Ok(Vec::new())
            }
            Some("response.reasoning_summary_text.delta") => {
                let value = root
                    .get("delta")
                    .and_then(ProviderJsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed("Responses reasoning delta has no text"))?;
                Ok(vec![ProviderDecodedEvent::Reasoning(value.to_owned())])
            }
            Some("response.output_item.added") => {
                let item_type = root
                    .at(&["item", "type"])
                    .and_then(ProviderJsonValue::as_str)
                    .ok_or_else(|| malformed("Responses output item has no type"))?;
                if item_type != "function_call" {
                    return Ok(Vec::new());
                }
                let key = tool_key(&root, self.tools.len())?;
                let state = self.tool_state(key)?;
                if let Some(value) = root
                    .at(&["item", "call_id"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    state.call_id = Some(value.to_owned());
                }
                if let Some(value) = root
                    .at(&["item", "name"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    state.name = Some(value.to_owned());
                }
                if let Some(value) = root
                    .at(&["item", "arguments"])
                    .and_then(ProviderJsonValue::as_str)
                    .filter(|value| !value.is_empty())
                {
                    state.arguments.replace(value)?;
                }
                Ok(Vec::new())
            }
            Some("response.function_call_arguments.delta") => {
                let key = tool_key(&root, 0)?;
                let state = self.tool_state(key)?;
                let value = root
                    .get("delta")
                    .and_then(ProviderJsonValue::as_str)
                    .ok_or_else(|| malformed("Responses tool argument delta has no text"))?;
                state.arguments.append(value)?;
                Ok(Vec::new())
            }
            Some("response.function_call_arguments.done") => {
                let key = tool_key(&root, 0)?;
                if let Some(value) = root.get("arguments").and_then(ProviderJsonValue::as_str) {
                    self.tool_state(key.clone())?.arguments.replace(value)?;
                }
                self.emit_tool(&key)
            }
            Some("response.output_item.done") => {
                let item_type = root
                    .at(&["item", "type"])
                    .and_then(ProviderJsonValue::as_str)
                    .ok_or_else(|| malformed("Responses output item has no type"))?;
                if item_type != "function_call" {
                    return Ok(Vec::new());
                }
                let key = tool_key(&root, 0)?;
                let state = self.tool_state(key.clone())?;
                if let Some(value) = root
                    .at(&["item", "call_id"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    state.call_id = Some(value.to_owned());
                }
                if let Some(value) = root
                    .at(&["item", "name"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    state.name = Some(value.to_owned());
                }
                if let Some(value) = root
                    .at(&["item", "arguments"])
                    .and_then(ProviderJsonValue::as_str)
                {
                    state.arguments.replace(value)?;
                }
                self.emit_tool(&key)
            }
            Some("response.completed") => {
                if self.completed {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::MalformedResponse,
                        "provider emitted duplicate completion",
                    ));
                }
                let response = root
                    .get("response")
                    .ok_or_else(|| malformed("Responses completion has no response object"))?;
                if response.get("status").and_then(ProviderJsonValue::as_str) != Some("completed") {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::MalformedResponse,
                        "provider emitted response.completed with unsuccessful status",
                    ));
                }
                if self.tools.values().any(|tool| !tool.emitted) {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::MalformedResponse,
                        "provider completed with an unfinished tool call",
                    ));
                }
                self.completed = true;
                if let Some(id) = response.get("id").and_then(ProviderJsonValue::as_str) {
                    self.response_id = Some(id.to_owned());
                }
                self.usage = parse_responses_usage(response.get("usage"))?;
                let continuation = self
                    .exposes_continuation
                    .then_some(())
                    .and(self.response_id.as_ref())
                    .map(|value| {
                        rust_provider_kit_core::ProviderContinuation::new(
                            self.selection.provider_id().clone(),
                            self.selection.account_id().clone(),
                            value.clone(),
                        )
                    })
                    .transpose()
                    .map_err(core_error_failure)?;
                let native_state = if let Some(output) = response.get("output") {
                    Some(
                        ProviderNativeState::new(
                            "openai.responses.output.v1",
                            serde_to_json(json!({
                                "provider":self.selection.provider_id().as_str(),
                                "model":self.selection.model_id().as_str(),
                                "output":json_to_serde(output)?
                            }))?,
                        )
                        .map_err(core_error_failure)?,
                    )
                } else {
                    None
                };
                Ok(vec![ProviderDecodedEvent::Completion(
                    ProviderCompletionDraft {
                        response_id: self.response_id.clone(),
                        continuation,
                        native_state,
                        usage: self.usage.clone(),
                    },
                )])
            }
            Some("response.failed" | "error") => Err(provider_stream_failure(
                event.data.as_bytes(),
                ProviderFailureCode::ServerFailed,
                "provider stream failed",
            )),
            Some("response.incomplete") => Err(ProviderFailure::new(
                ProviderFailureCode::ServerFailed,
                "provider response was incomplete",
            )),
            Some("response.refusal.delta" | "response.refusal.done") => Err(ProviderFailure::new(
                ProviderFailureCode::PermissionDenied,
                "provider refused the response",
            )),
            None => Err(ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider SSE event has no type",
            )),
            Some(_) => Ok(Vec::new()),
        }
    }
    fn finish(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        if self.completed {
            Ok(Vec::new())
        } else {
            Err(ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider stream ended before response.completed",
            ))
        }
    }
}
impl OpenAiResponsesStreamDecoder {
    fn tool_state(&mut self, key: String) -> Result<&mut ResponsesToolState, ProviderFailure> {
        if !self.tools.contains_key(&key) && self.tools.len() >= ProviderTurnRequest::MAXIMUM_TOOLS
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "Responses stream exceeded tool-call state limit",
            ));
        }
        Ok(self.tools.entry(key).or_default())
    }

    fn emit_tool(&mut self, key: &str) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure> {
        let state = self.tools.get_mut(key).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider tool call is missing",
            )
        })?;
        if state.emitted {
            return Ok(Vec::new());
        }
        let call_id = state.call_id.as_deref().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider tool call is missing its identity",
            )
        })?;
        let name = state.name.as_deref().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider tool call is missing its name",
            )
        })?;
        let call = ProviderToolCall::new(call_id, name, state.arguments.decode_object()?)
            .map_err(core_error_failure)?;
        state.emitted = true;
        Ok(vec![ProviderDecodedEvent::ToolCall(call)])
    }
}
fn tool_key(root: &ProviderJsonValue, fallback: usize) -> Result<String, ProviderFailure> {
    if let Some(value) = root
        .at(&["item", "id"])
        .or_else(|| root.get("item_id"))
        .and_then(ProviderJsonValue::as_str)
    {
        return Ok(value.to_owned());
    }
    let index = optional_nonnegative_u64(root.get("output_index"), "Responses output index")?
        .unwrap_or(fallback as u64);
    Ok(format!("index:{index}"))
}
fn parse_responses_usage(
    value: Option<&ProviderJsonValue>,
) -> Result<Option<rust_provider_kit_core::ProviderUsage>, ProviderFailure> {
    let Some(value) = value else { return Ok(None) };
    let input = optional_nonnegative_u64(value.get("input_tokens"), "Responses input token count")?;
    let output =
        optional_nonnegative_u64(value.get("output_tokens"), "Responses output token count")?;
    let cached = optional_nonnegative_u64(
        value.at(&["input_tokens_details", "cached_tokens"]),
        "Responses cached token count",
    )?;
    let total = optional_nonnegative_u64(value.get("total_tokens"), "Responses total token count")?;
    usage(input, output, cached, total)
}
