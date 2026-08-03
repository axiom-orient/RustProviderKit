use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    ProviderAccountId, ProviderConformanceReceiptId, ProviderCoreError, ProviderId,
    ProviderInstant, ProviderJsonValue, ProviderModelId, ProviderRequestId, is_control,
    validate_trimmed_text,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocolFamily {
    CodexResponses,
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    OpenAiChatCompletions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderDescriptor {
    id: ProviderId,
    display_name: String,
    protocol_family: ProviderProtocolFamily,
    supports_api_key: bool,
    supports_oauth: bool,
    requires_explicit_endpoint: bool,
}

impl ProviderDescriptor {
    pub fn new(
        id: ProviderId,
        display_name: impl Into<String>,
        protocol_family: ProviderProtocolFamily,
        supports_api_key: bool,
        supports_oauth: bool,
        requires_explicit_endpoint: bool,
    ) -> Result<Self, ProviderCoreError> {
        let display_name = display_name.into();
        validate_trimmed_text(&display_name, 128, "provider display name")?;
        Ok(Self {
            id,
            display_name,
            protocol_family,
            supports_api_key,
            supports_oauth,
            requires_explicit_endpoint,
        })
    }

    #[must_use]
    pub fn id(&self) -> &ProviderId {
        &self.id
    }
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
    #[must_use]
    pub fn protocol_family(&self) -> ProviderProtocolFamily {
        self.protocol_family
    }
    #[must_use]
    pub fn supports_api_key(&self) -> bool {
        self.supports_api_key
    }
    #[must_use]
    pub fn supports_oauth(&self) -> bool {
        self.supports_oauth
    }
    #[must_use]
    pub fn requires_explicit_endpoint(&self) -> bool {
        self.requires_explicit_endpoint
    }
}

#[derive(Deserialize)]
struct ProviderDescriptorRaw {
    id: ProviderId,
    display_name: String,
    protocol_family: ProviderProtocolFamily,
    supports_api_key: bool,
    supports_oauth: bool,
    #[serde(default)]
    requires_explicit_endpoint: bool,
}

impl<'de> Deserialize<'de> for ProviderDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderDescriptorRaw::deserialize(deserializer)?;
        Self::new(
            raw.id,
            raw.display_name,
            raw.protocol_family,
            raw.supports_api_key,
            raw.supports_oauth,
            raw.requires_explicit_endpoint,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderSelection {
    provider_id: ProviderId,
    account_id: ProviderAccountId,
    model_id: ProviderModelId,
}

impl ProviderSelection {
    #[must_use]
    pub fn new(
        provider_id: ProviderId,
        account_id: ProviderAccountId,
        model_id: ProviderModelId,
    ) -> Self {
        Self {
            provider_id,
            account_id,
            model_id,
        }
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn model_id(&self) -> &ProviderModelId {
        &self.model_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMessageRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderMessageContent {
    Text {
        text: String,
    },
    /// A completed assistant tool call preserved for caller-owned history.
    ToolCall {
        call: ProviderToolCall,
    },
    /// Provider-owned opaque output retained by the caller for an exact
    /// same-route replay. It is never interpreted by the core or agent.
    NativeState {
        state: ProviderNativeState,
    },
    ToolResult {
        #[serde(rename = "call_id")]
        call_id: String,
        name: String,
        value: ProviderJsonValue,
        #[serde(default)]
        is_error: bool,
    },
}

impl ProviderMessageContent {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text { text: value.into() }
    }
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        value: ProviderJsonValue,
    ) -> Result<Self, ProviderCoreError> {
        Self::tool_result_with_status(call_id, name, value, false)
    }
    pub fn tool_result_with_status(
        call_id: impl Into<String>,
        name: impl Into<String>,
        value: ProviderJsonValue,
        is_error: bool,
    ) -> Result<Self, ProviderCoreError> {
        let call_id = call_id.into();
        let name = name.into();
        validate_tool_call_id(&call_id)?;
        validate_tool_name(&name)?;
        value.validated()?;
        Ok(Self::ToolResult {
            call_id,
            name,
            value,
            is_error,
        })
    }
    #[must_use]
    pub fn tool_call(call: ProviderToolCall) -> Self {
        Self::ToolCall { call }
    }
    #[must_use]
    pub fn native_state(state: ProviderNativeState) -> Self {
        Self::NativeState { state }
    }
    #[must_use]
    pub fn text_value(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
    #[must_use]
    pub fn tool_result_value(&self) -> Option<(&str, &str, &ProviderJsonValue)> {
        match self {
            Self::ToolResult {
                call_id,
                name,
                value,
                ..
            } => Some((call_id, name, value)),
            _ => None,
        }
    }
    #[must_use]
    pub fn tool_result_is_error(&self) -> Option<bool> {
        match self {
            Self::ToolResult { is_error, .. } => Some(*is_error),
            _ => None,
        }
    }
    #[must_use]
    pub fn tool_call_value(&self) -> Option<&ProviderToolCall> {
        match self {
            Self::ToolCall { call } => Some(call),
            _ => None,
        }
    }
    #[must_use]
    pub fn native_state_value(&self) -> Option<&ProviderNativeState> {
        match self {
            Self::NativeState { state } => Some(state),
            _ => None,
        }
    }
}

/// Opaque provider output for an exact same-route continuation.
///
/// Callers may persist this value only beside the assistant message it
/// describes. A different provider/model must reject it rather than trying to
/// reinterpret it as portable history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderNativeState {
    format: String,
    payload: ProviderJsonValue,
}
// Construction validates all contained JSON numbers as finite, so the stored
// payload has reflexive equality despite ProviderJsonValue's general f64 type.
impl Eq for ProviderNativeState {}
impl ProviderNativeState {
    pub fn new(
        format: impl Into<String>,
        payload: ProviderJsonValue,
    ) -> Result<Self, ProviderCoreError> {
        let format = format.into();
        if format.is_empty()
            || format.len() > 160
            || !format
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
            || payload.as_array().is_none() && payload.as_object().is_none()
        {
            return Err(ProviderCoreError::invalid_request(
                "provider native state is invalid",
            ));
        }
        payload.validated()?;
        Ok(Self { format, payload })
    }
    #[must_use]
    pub fn format(&self) -> &str {
        &self.format
    }
    #[must_use]
    pub fn payload(&self) -> &ProviderJsonValue {
        &self.payload
    }
}
#[derive(Deserialize)]
struct ProviderNativeStateRaw {
    format: String,
    payload: ProviderJsonValue,
}
impl<'de> Deserialize<'de> for ProviderNativeState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderNativeStateRaw::deserialize(deserializer)?;
        Self::new(raw.format, raw.payload).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderMessage {
    role: ProviderMessageRole,
    content: Vec<ProviderMessageContent>,
}

impl ProviderMessage {
    pub const MAXIMUM_CONTENT_ITEMS: usize = 64;
    pub const MAXIMUM_TEXT_SCALARS: usize = 262_144;

    pub fn text(
        role: ProviderMessageRole,
        text: impl Into<String>,
    ) -> Result<Self, ProviderCoreError> {
        Self::new(role, vec![ProviderMessageContent::text(text)])
    }

    pub fn new(
        role: ProviderMessageRole,
        content: Vec<ProviderMessageContent>,
    ) -> Result<Self, ProviderCoreError> {
        if content.is_empty() || content.len() > Self::MAXIMUM_CONTENT_ITEMS {
            return Err(ProviderCoreError::invalid_request(
                "provider message content is empty or oversized",
            ));
        }
        let mut text_scalars = 0usize;
        for item in &content {
            match item {
                ProviderMessageContent::Text { text } => {
                    if text.is_empty() || text.contains('\0') {
                        return Err(ProviderCoreError::invalid_request(
                            "provider message text is empty or contains NUL",
                        ));
                    }
                    text_scalars =
                        text_scalars
                            .checked_add(text.chars().count())
                            .ok_or_else(|| {
                                ProviderCoreError::invalid_request("provider message size overflow")
                            })?;
                    if text_scalars > Self::MAXIMUM_TEXT_SCALARS {
                        return Err(ProviderCoreError::invalid_request(
                            "provider message is oversized",
                        ));
                    }
                }
                ProviderMessageContent::ToolCall { call } => {
                    if role != ProviderMessageRole::Assistant {
                        return Err(ProviderCoreError::invalid_request(
                            "tool call content requires the assistant message role",
                        ));
                    }
                    call.arguments().validated()?;
                }
                ProviderMessageContent::NativeState { state } => {
                    if role != ProviderMessageRole::Assistant {
                        return Err(ProviderCoreError::invalid_request(
                            "native state requires the assistant message role",
                        ));
                    }
                    state.payload().validated()?;
                }
                ProviderMessageContent::ToolResult {
                    call_id,
                    name,
                    value,
                    ..
                } => {
                    validate_tool_call_id(call_id)?;
                    validate_tool_name(name)?;
                    value.validated()?;
                    if role != ProviderMessageRole::Tool {
                        return Err(ProviderCoreError::invalid_request(
                            "tool result content requires the tool message role",
                        ));
                    }
                }
            }
        }
        if role == ProviderMessageRole::Tool
            && content
                .iter()
                .any(|item| !matches!(item, ProviderMessageContent::ToolResult { .. }))
        {
            return Err(ProviderCoreError::invalid_request(
                "tool messages may contain only tool results",
            ));
        }
        if content
            .iter()
            .filter(|item| item.native_state_value().is_some())
            .nth(1)
            .is_some()
        {
            return Err(ProviderCoreError::invalid_request(
                "assistant message contains multiple native states",
            ));
        }
        Ok(Self { role, content })
    }

    #[must_use]
    pub fn role(&self) -> ProviderMessageRole {
        self.role
    }
    #[must_use]
    pub fn content(&self) -> &[ProviderMessageContent] {
        &self.content
    }
}

#[derive(Deserialize)]
struct ProviderMessageRaw {
    role: ProviderMessageRole,
    content: Vec<ProviderMessageContent>,
}
impl<'de> Deserialize<'de> for ProviderMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderMessageRaw::deserialize(deserializer)?;
        Self::new(raw.role, raw.content).map_err(serde::de::Error::custom)
    }
}

pub(crate) fn validate_tool_call_id(value: &str) -> Result<(), ProviderCoreError> {
    if value.is_empty() || value.len() > 192 || value.trim() != value || is_control(value) {
        return Err(ProviderCoreError::invalid_request(
            "provider tool call ID is invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_tool_name(value: &str) -> Result<(), ProviderCoreError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(ProviderCoreError::invalid_request(
            "provider tool name is invalid",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderToolDefinition {
    name: String,
    description: String,
    input_schema: ProviderJsonValue,
    strict: bool,
}

impl ProviderToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: ProviderJsonValue,
        strict: bool,
    ) -> Result<Self, ProviderCoreError> {
        let name = name.into();
        let description = description.into();
        validate_tool_name(&name)?;
        if description.is_empty()
            || description.len() > 8_192
            || is_control(&description)
            || input_schema.as_object().is_none()
        {
            return Err(ProviderCoreError::invalid_request(
                "provider tool definition is invalid",
            ));
        }
        input_schema.validated()?;
        Ok(Self {
            name,
            description,
            input_schema,
            strict,
        })
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    #[must_use]
    pub fn input_schema(&self) -> &ProviderJsonValue {
        &self.input_schema
    }
    #[must_use]
    pub fn strict(&self) -> bool {
        self.strict
    }
}
#[derive(Deserialize)]
struct ProviderToolDefinitionRaw {
    name: String,
    description: String,
    input_schema: ProviderJsonValue,
    strict: bool,
}
impl<'de> Deserialize<'de> for ProviderToolDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderToolDefinitionRaw::deserialize(deserializer)?;
        Self::new(raw.name, raw.description, raw.input_schema, raw.strict)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderToolCall {
    id: String,
    name: String,
    arguments: ProviderJsonValue,
}
impl ProviderToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: ProviderJsonValue,
    ) -> Result<Self, ProviderCoreError> {
        let id = id.into();
        let name = name.into();
        validate_tool_call_id(&id)?;
        validate_tool_name(&name)?;
        if arguments.as_object().is_none() {
            return Err(ProviderCoreError::invalid_value(
                "provider tool arguments must be an object",
            ));
        }
        arguments.validated()?;
        Ok(Self {
            id,
            name,
            arguments,
        })
    }
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn arguments(&self) -> &ProviderJsonValue {
        &self.arguments
    }
}
#[derive(Deserialize)]
struct ProviderToolCallRaw {
    id: String,
    name: String,
    arguments: ProviderJsonValue,
}
impl<'de> Deserialize<'de> for ProviderToolCall {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderToolCallRaw::deserialize(deserializer)?;
        Self::new(raw.id, raw.name, raw.arguments).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProviderToolChoice {
    #[default]
    Automatic,
    Required,
    Named {
        name: String,
    },
}

impl ProviderToolChoice {
    pub fn named(name: impl Into<String>) -> Result<Self, ProviderCoreError> {
        let name = name.into();
        validate_tool_name(&name)?;
        Ok(Self::Named { name })
    }

    #[must_use]
    pub fn named_value(&self) -> Option<&str> {
        match self {
            Self::Named { name } => Some(name),
            _ => None,
        }
    }

    fn validated(&self) -> Result<(), ProviderCoreError> {
        match self {
            Self::Automatic | Self::Required => Ok(()),
            Self::Named { name } => validate_tool_name(name),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProviderToolChoiceRef<'a> {
    Automatic,
    Required,
    Named { name: &'a str },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProviderToolChoiceOwned {
    Automatic,
    Required,
    Named { name: String },
}

impl Serialize for ProviderToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validated().map_err(serde::ser::Error::custom)?;
        match self {
            Self::Automatic => ProviderToolChoiceRef::Automatic.serialize(serializer),
            Self::Required => ProviderToolChoiceRef::Required.serialize(serializer),
            Self::Named { name } => ProviderToolChoiceRef::Named { name }.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ProviderToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ProviderToolChoiceOwned::deserialize(deserializer)? {
            ProviderToolChoiceOwned::Automatic => Ok(Self::Automatic),
            ProviderToolChoiceOwned::Required => Ok(Self::Required),
            ProviderToolChoiceOwned::Named { name } => {
                Self::named(name).map_err(serde::de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ProviderOutputRequirement {
    #[default]
    Text,
    ApplicationValidatedJson {
        name: String,
        schema: ProviderJsonValue,
    },
    JsonSchema {
        name: String,
        schema: ProviderJsonValue,
        strict: bool,
    },
}

impl ProviderOutputRequirement {
    pub fn application_validated_json(
        name: impl Into<String>,
        schema: ProviderJsonValue,
    ) -> Result<Self, ProviderCoreError> {
        let name = name.into();
        validate_output_schema(&name, &schema)?;
        Ok(Self::ApplicationValidatedJson { name, schema })
    }

    pub fn json_schema(
        name: impl Into<String>,
        schema: ProviderJsonValue,
        strict: bool,
    ) -> Result<Self, ProviderCoreError> {
        let name = name.into();
        validate_output_schema(&name, &schema)?;
        Ok(Self::JsonSchema {
            name,
            schema,
            strict,
        })
    }

    fn validated(&self) -> Result<(), ProviderCoreError> {
        match self {
            Self::Text => Ok(()),
            Self::ApplicationValidatedJson { name, schema }
            | Self::JsonSchema { name, schema, .. } => validate_output_schema(name, schema),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProviderOutputRequirementRef<'a> {
    Text,
    ApplicationValidatedJson {
        name: &'a str,
        schema: &'a ProviderJsonValue,
    },
    JsonSchema {
        name: &'a str,
        schema: &'a ProviderJsonValue,
        strict: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProviderOutputRequirementOwned {
    Text,
    ApplicationValidatedJson {
        name: String,
        schema: ProviderJsonValue,
    },
    JsonSchema {
        name: String,
        schema: ProviderJsonValue,
        strict: bool,
    },
}

impl Serialize for ProviderOutputRequirement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validated().map_err(serde::ser::Error::custom)?;
        match self {
            Self::Text => ProviderOutputRequirementRef::Text.serialize(serializer),
            Self::ApplicationValidatedJson { name, schema } => {
                ProviderOutputRequirementRef::ApplicationValidatedJson { name, schema }
                    .serialize(serializer)
            }
            Self::JsonSchema {
                name,
                schema,
                strict,
            } => ProviderOutputRequirementRef::JsonSchema {
                name,
                schema,
                strict: *strict,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ProviderOutputRequirement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ProviderOutputRequirementOwned::deserialize(deserializer)? {
            ProviderOutputRequirementOwned::Text => Ok(Self::Text),
            ProviderOutputRequirementOwned::ApplicationValidatedJson { name, schema } => {
                Self::application_validated_json(name, schema).map_err(serde::de::Error::custom)
            }
            ProviderOutputRequirementOwned::JsonSchema {
                name,
                schema,
                strict,
            } => Self::json_schema(name, schema, strict).map_err(serde::de::Error::custom),
        }
    }
}

fn validate_output_schema(name: &str, schema: &ProviderJsonValue) -> Result<(), ProviderCoreError> {
    validate_tool_name(name)?;
    if schema.as_object().is_none() {
        return Err(ProviderCoreError::invalid_request(
            "output schema must be an object",
        ));
    }
    schema.validated()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReasoningEffort {
    Low,
    Medium,
    High,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "effort", rename_all = "snake_case")]
pub enum ProviderReasoningPolicy {
    Disabled,
    #[default]
    Automatic,
    Effort(ProviderReasoningEffort),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderContinuation {
    provider_id: ProviderId,
    account_id: ProviderAccountId,
    value: String,
}
impl ProviderContinuation {
    pub fn new(
        provider_id: ProviderId,
        account_id: ProviderAccountId,
        value: impl Into<String>,
    ) -> Result<Self, ProviderCoreError> {
        let value = value.into();
        if value.is_empty() || value.len() > 8_192 || is_control(&value) {
            return Err(ProviderCoreError::invalid_request(
                "provider continuation is invalid",
            ));
        }
        Ok(Self {
            provider_id,
            account_id,
            value,
        })
    }
    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}
#[derive(Deserialize)]
struct ProviderContinuationRaw {
    provider_id: ProviderId,
    account_id: ProviderAccountId,
    value: String,
}
impl<'de> Deserialize<'de> for ProviderContinuation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderContinuationRaw::deserialize(deserializer)?;
        Self::new(raw.provider_id, raw.account_id, raw.value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDataCollectionPolicy {
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRequestConstraints {
    data_collection: ProviderDataCollectionPolicy,
    requires_zero_data_retention: bool,
    requires_parameter_support: bool,
    allows_provider_endpoint_fallbacks: bool,
    timeout_milliseconds: u64,
    maximum_response_bytes: usize,
    maximum_retry_attempts: usize,
    maximum_output_tokens: Option<usize>,
}
impl ProviderRequestConstraints {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_collection: ProviderDataCollectionPolicy,
        requires_zero_data_retention: bool,
        requires_parameter_support: bool,
        allows_provider_endpoint_fallbacks: bool,
        timeout_milliseconds: u64,
        maximum_response_bytes: usize,
        maximum_retry_attempts: usize,
        maximum_output_tokens: Option<usize>,
    ) -> Result<Self, ProviderCoreError> {
        if allows_provider_endpoint_fallbacks
            || !(1_000..=3_600_000).contains(&timeout_milliseconds)
            || !(1_024..=64 * 1_024 * 1_024).contains(&maximum_response_bytes)
            || !(1..=3).contains(&maximum_retry_attempts)
            || maximum_output_tokens.is_some_and(|value| !(1..=1_000_000).contains(&value))
        {
            return Err(ProviderCoreError::invalid_request(
                "provider constraints are invalid",
            ));
        }
        Ok(Self {
            data_collection,
            requires_zero_data_retention,
            requires_parameter_support,
            allows_provider_endpoint_fallbacks,
            timeout_milliseconds,
            maximum_response_bytes,
            maximum_retry_attempts,
            maximum_output_tokens,
        })
    }
    #[must_use]
    pub fn data_collection(&self) -> ProviderDataCollectionPolicy {
        self.data_collection
    }
    #[must_use]
    pub fn requires_zero_data_retention(&self) -> bool {
        self.requires_zero_data_retention
    }
    #[must_use]
    pub fn requires_parameter_support(&self) -> bool {
        self.requires_parameter_support
    }
    #[must_use]
    pub fn allows_provider_endpoint_fallbacks(&self) -> bool {
        self.allows_provider_endpoint_fallbacks
    }
    #[must_use]
    pub fn timeout_milliseconds(&self) -> u64 {
        self.timeout_milliseconds
    }
    #[must_use]
    pub fn maximum_response_bytes(&self) -> usize {
        self.maximum_response_bytes
    }
    #[must_use]
    pub fn maximum_retry_attempts(&self) -> usize {
        self.maximum_retry_attempts
    }
    #[must_use]
    pub fn maximum_output_tokens(&self) -> Option<usize> {
        self.maximum_output_tokens
    }
}
impl Default for ProviderRequestConstraints {
    fn default() -> Self {
        Self {
            data_collection: ProviderDataCollectionPolicy::Deny,
            requires_zero_data_retention: true,
            requires_parameter_support: true,
            allows_provider_endpoint_fallbacks: false,
            timeout_milliseconds: 300_000,
            maximum_response_bytes: 16 * 1_024 * 1_024,
            maximum_retry_attempts: 1,
            maximum_output_tokens: None,
        }
    }
}
#[derive(Deserialize)]
struct ProviderRequestConstraintsRaw {
    data_collection: ProviderDataCollectionPolicy,
    requires_zero_data_retention: bool,
    requires_parameter_support: bool,
    allows_provider_endpoint_fallbacks: bool,
    timeout_milliseconds: u64,
    maximum_response_bytes: usize,
    maximum_retry_attempts: usize,
    maximum_output_tokens: Option<usize>,
}
impl<'de> Deserialize<'de> for ProviderRequestConstraints {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderRequestConstraintsRaw::deserialize(deserializer)?;
        Self::new(
            raw.data_collection,
            raw.requires_zero_data_retention,
            raw.requires_parameter_support,
            raw.allows_provider_endpoint_fallbacks,
            raw.timeout_milliseconds,
            raw.maximum_response_bytes,
            raw.maximum_retry_attempts,
            raw.maximum_output_tokens,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq)]
pub struct ProviderTurnRequest {
    inner: Arc<ProviderTurnRequestData>,
}

#[derive(Debug, PartialEq, Serialize)]
struct ProviderTurnRequestData {
    id: ProviderRequestId,
    selection: ProviderSelection,
    messages: Vec<ProviderMessage>,
    tools: Vec<ProviderToolDefinition>,
    tool_choice: ProviderToolChoice,
    output: ProviderOutputRequirement,
    reasoning: ProviderReasoningPolicy,
    continuation: Option<ProviderContinuation>,
    constraints: ProviderRequestConstraints,
}
impl ProviderTurnRequest {
    pub const MAXIMUM_MESSAGES: usize = 512;
    pub const MAXIMUM_TOOLS: usize = 128;
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ProviderRequestId,
        selection: ProviderSelection,
        messages: Vec<ProviderMessage>,
        tools: Vec<ProviderToolDefinition>,
        tool_choice: ProviderToolChoice,
        output: ProviderOutputRequirement,
        reasoning: ProviderReasoningPolicy,
        continuation: Option<ProviderContinuation>,
        constraints: ProviderRequestConstraints,
    ) -> Result<Self, ProviderCoreError> {
        let unique = tools
            .iter()
            .map(|tool| tool.name())
            .collect::<BTreeSet<_>>();
        if messages.is_empty()
            || messages.len() > Self::MAXIMUM_MESSAGES
            || tools.len() > Self::MAXIMUM_TOOLS
            || unique.len() != tools.len()
            || !matches!(
                messages.last().map(ProviderMessage::role),
                Some(ProviderMessageRole::User | ProviderMessageRole::Tool)
            )
        {
            return Err(ProviderCoreError::invalid_request(
                "provider request messages or tools violate bounds",
            ));
        }
        if let Some(value) = &continuation
            && (value.provider_id() != selection.provider_id()
                || value.account_id() != selection.account_id())
        {
            return Err(ProviderCoreError::invalid_request(
                "provider continuation belongs to another provider account",
            ));
        }
        match &tool_choice {
            ProviderToolChoice::Automatic => {}
            ProviderToolChoice::Required if tools.is_empty() => {
                return Err(ProviderCoreError::invalid_request(
                    "required tool choice needs at least one tool",
                ));
            }
            ProviderToolChoice::Required => {}
            ProviderToolChoice::Named { name } => {
                validate_tool_name(name)?;
                if !unique.contains(name.as_str()) {
                    return Err(ProviderCoreError::invalid_request(
                        "named tool choice must reference a supplied tool",
                    ));
                }
            }
        }
        match &output {
            ProviderOutputRequirement::Text => {}
            ProviderOutputRequirement::ApplicationValidatedJson { name, schema }
            | ProviderOutputRequirement::JsonSchema { name, schema, .. } => {
                validate_output_schema(name, schema)?
            }
        }
        Ok(Self {
            inner: Arc::new(ProviderTurnRequestData {
                id,
                selection,
                messages,
                tools,
                tool_choice,
                output,
                reasoning,
                continuation,
                constraints,
            }),
        })
    }
    #[must_use]
    pub fn id(&self) -> &ProviderRequestId {
        &self.inner.id
    }
    #[must_use]
    pub fn selection(&self) -> &ProviderSelection {
        &self.inner.selection
    }
    #[must_use]
    pub fn messages(&self) -> &[ProviderMessage] {
        &self.inner.messages
    }
    #[must_use]
    pub fn tools(&self) -> &[ProviderToolDefinition] {
        &self.inner.tools
    }
    #[must_use]
    pub fn tool_choice(&self) -> &ProviderToolChoice {
        &self.inner.tool_choice
    }
    #[must_use]
    pub fn output(&self) -> &ProviderOutputRequirement {
        &self.inner.output
    }
    #[must_use]
    pub fn reasoning(&self) -> &ProviderReasoningPolicy {
        &self.inner.reasoning
    }
    #[must_use]
    pub fn continuation(&self) -> Option<&ProviderContinuation> {
        self.inner.continuation.as_ref()
    }
    #[must_use]
    pub fn constraints(&self) -> &ProviderRequestConstraints {
        &self.inner.constraints
    }
}
impl std::fmt::Debug for ProviderTurnRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderTurnRequest")
            .field("id", &self.inner.id)
            .field("selection", &self.inner.selection)
            .field("messages", &self.inner.messages)
            .field("tools", &self.inner.tools)
            .field("tool_choice", &self.inner.tool_choice)
            .field("output", &self.inner.output)
            .field("reasoning", &self.inner.reasoning)
            .field("continuation", &self.inner.continuation)
            .field("constraints", &self.inner.constraints)
            .finish()
    }
}
impl Serialize for ProviderTurnRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.inner.as_ref().serialize(serializer)
    }
}
#[derive(Deserialize)]
struct ProviderTurnRequestRaw {
    id: ProviderRequestId,
    selection: ProviderSelection,
    messages: Vec<ProviderMessage>,
    #[serde(default)]
    tools: Vec<ProviderToolDefinition>,
    #[serde(default)]
    tool_choice: ProviderToolChoice,
    #[serde(default)]
    output: ProviderOutputRequirement,
    #[serde(default)]
    reasoning: ProviderReasoningPolicy,
    continuation: Option<ProviderContinuation>,
    constraints: ProviderRequestConstraints,
}
impl<'de> Deserialize<'de> for ProviderTurnRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderTurnRequestRaw::deserialize(deserializer)?;
        Self::new(
            raw.id,
            raw.selection,
            raw.messages,
            raw.tools,
            raw.tool_choice,
            raw.output,
            raw.reasoning,
            raw.continuation,
            raw.constraints,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilitySource {
    ProviderDocumentation,
    ProviderModelCatalog,
    AccountInspection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CapabilitySupport {
    Verified(ProviderConformanceReceiptId),
    Declared(ProviderCapabilitySource),
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: CapabilitySupport,
    pub tool_calling: CapabilitySupport,
    pub parallel_tool_calling: CapabilitySupport,
    pub structured_output: CapabilitySupport,
    pub reasoning_continuity: CapabilitySupport,
    pub usage_reporting: CapabilitySupport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderModelDescriptor {
    id: ProviderModelId,
    display_name: Option<String>,
    capabilities: ProviderCapabilities,
    context_token_limit: Option<usize>,
    maximum_output_tokens: Option<usize>,
}
impl ProviderModelDescriptor {
    pub fn new(
        id: ProviderModelId,
        display_name: Option<String>,
        capabilities: ProviderCapabilities,
        context_token_limit: Option<usize>,
        maximum_output_tokens: Option<usize>,
    ) -> Result<Self, ProviderCoreError> {
        if let Some(value) = &display_name
            && (value.is_empty() || value.len() > 256 || is_control(value))
        {
            return Err(ProviderCoreError::invalid_value(
                "model display name is invalid",
            ));
        }
        if context_token_limit == Some(0) || maximum_output_tokens == Some(0) {
            return Err(ProviderCoreError::invalid_value(
                "model token limit is invalid",
            ));
        }
        Ok(Self {
            id,
            display_name,
            capabilities,
            context_token_limit,
            maximum_output_tokens,
        })
    }
    #[must_use]
    pub fn id(&self) -> &ProviderModelId {
        &self.id
    }
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }
    #[must_use]
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    #[must_use]
    pub fn context_token_limit(&self) -> Option<usize> {
        self.context_token_limit
    }
    #[must_use]
    pub fn maximum_output_tokens(&self) -> Option<usize> {
        self.maximum_output_tokens
    }
}
#[derive(Deserialize)]
struct ProviderModelDescriptorRaw {
    id: ProviderModelId,
    display_name: Option<String>,
    #[serde(default)]
    capabilities: ProviderCapabilities,
    context_token_limit: Option<usize>,
    maximum_output_tokens: Option<usize>,
}
impl<'de> Deserialize<'de> for ProviderModelDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderModelDescriptorRaw::deserialize(deserializer)?;
        Self::new(
            raw.id,
            raw.display_name,
            raw.capabilities,
            raw.context_token_limit,
            raw.maximum_output_tokens,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderModelCatalogResult {
    models: Vec<ProviderModelDescriptor>,
    refreshed_at: ProviderInstant,
}
impl ProviderModelCatalogResult {
    pub fn new(
        mut models: Vec<ProviderModelDescriptor>,
        refreshed_at: ProviderInstant,
    ) -> Result<Self, ProviderCoreError> {
        models.sort_by(|left, right| left.id().cmp(right.id()));
        if models.windows(2).any(|pair| pair[0].id() == pair[1].id()) {
            return Err(ProviderCoreError::invalid_value(
                "model catalog contains duplicates",
            ));
        }
        Ok(Self {
            models,
            refreshed_at,
        })
    }
    #[must_use]
    pub fn models(&self) -> &[ProviderModelDescriptor] {
        &self.models
    }
    #[must_use]
    pub fn refreshed_at(&self) -> ProviderInstant {
        self.refreshed_at
    }
}
#[derive(Deserialize)]
struct ProviderModelCatalogResultRaw {
    models: Vec<ProviderModelDescriptor>,
    refreshed_at: ProviderInstant,
}
impl<'de> Deserialize<'de> for ProviderModelCatalogResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ProviderModelCatalogResultRaw::deserialize(deserializer)?;
        Self::new(raw.models, raw.refreshed_at).map_err(serde::de::Error::custom)
    }
}

#[must_use]
pub fn json_object(
    entries: impl IntoIterator<Item = (impl Into<String>, ProviderJsonValue)>,
) -> ProviderJsonValue {
    ProviderJsonValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}
