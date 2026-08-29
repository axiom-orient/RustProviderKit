//! Strict Codex-only provider process for the RGXAMK provider command contract.
//!
//! The binary is intentionally a small adapter.  It owns no agent state and
//! never executes an action; it translates one bounded request into one
//! immutable RustProviderKit turn and translates one validated tool call back
//! into the product response envelope.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use rust_provider_kit_core::{
    BuiltInProviderId, ProviderAccountId, ProviderAccountRegistrationRequest,
    ProviderCredentialLease, ProviderCredentialMaterial, ProviderCredentialRecord,
    ProviderCredentialRecordState, ProviderCredentialReference, ProviderCredentialStore,
    ProviderDataCollectionPolicy, ProviderEventStream, ProviderFailure, ProviderFailureCode,
    ProviderInstant, ProviderJsonValue, ProviderMessage, ProviderMessageRole, ProviderModelId,
    ProviderOutputRequirement, ProviderRateLimitEvidence, ProviderReasoningPolicy,
    ProviderRequestConstraints, ProviderRequestId, ProviderSelection, ProviderTerminal,
    ProviderToolCall, ProviderToolChoice, ProviderToolDefinition, ProviderTurnEvent,
    ProviderTurnRequest,
};
use rust_provider_kit_runtime::{ProviderRuntime, ProviderRuntimeOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const REQUEST_SCHEMA: &str = "rgx.agent.provider-request.v3";
pub const RESPONSE_SCHEMA: &str = "rgx.agent.provider-response.v2";
pub const ACTION_TOOL_NAME: &str = "rgxamk_action";

/// The product's maximum policy context is 64 MiB.  The process accepts one
/// request up to that bound plus one byte for a fail-closed overflow check.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_OPERATION_TIMEOUT_MS: u64 = 100_000;
pub const MIN_OPERATION_TIMEOUT_MS: u64 = 1_000;
pub const CLEANUP_RESERVE_MS: u64 = 10_000;
pub const MIN_RESPONSE_BYTES: usize = 1_024;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const FAILURE_DIAGNOSTIC_PREFIX: &str = "rgxamk-native-provider diagnostic-v1 ";
pub const MAX_FAILURE_DIAGNOSTIC_LINE_BYTES: usize = 2 * 1024;
const MAX_MESSAGE_BYTES: usize = 240_000;
const SAFE_REMOTE_ERROR_TOKENS: &[&str] = &[
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDiagnostics {
    Disabled,
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub account_id: ProviderAccountId,
    pub model_id: ProviderModelId,
    pub auth_file: PathBuf,
    pub codex_client_version: String,
    pub operation_timeout_ms: u64,
    pub upstream_max_response_bytes: usize,
    pub failure_diagnostics: FailureDiagnostics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    pub code: &'static str,
    pub message: &'static str,
    diagnostic_failure: Option<Box<ProviderFailure>>,
    diagnostic_line: Option<Box<str>>,
}

impl ProcessError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            diagnostic_failure: None,
            diagnostic_line: None,
        }
    }

    fn with_provider_failure(mut self, failure: &ProviderFailure) -> Self {
        self.diagnostic_failure = Some(Box::new(failure.clone()));
        self
    }

    fn attach_failure_diagnostic(mut self, mode: FailureDiagnostics) -> Self {
        if matches!(mode, FailureDiagnostics::V1)
            && let Some(failure) = self.diagnostic_failure.as_ref()
        {
            self.diagnostic_line = failure_diagnostic_line(failure).map(String::into_boxed_str);
        }
        self
    }

    #[must_use]
    pub fn diagnostic_line(&self) -> Option<&str> {
        self.diagnostic_line.as_deref()
    }

    #[must_use]
    pub fn output_failed() -> Self {
        Self::new("output_failed", "provider response could not be written")
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProcessError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Python,
    List,
    Read,
    Search,
    History,
    Delegate,
    Remember,
    Complete,
    Fail,
}

impl ActionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::List => "list",
            Self::Read => "read",
            Self::Search => "search",
            Self::History => "history",
            Self::Delegate => "delegate",
            Self::Remember => "remember",
            Self::Complete => "complete",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextWindow {
    encoded_bytes: usize,
    events_omitted: usize,
    memories_omitted: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderRequest {
    pub schema_version: String,
    pub run_id: String,
    pub root_run_id: String,
    pub parent_run_id: Option<String>,
    pub task: String,
    pub repository_id: String,
    pub phase: RequestPhase,
    pub step: u32,
    pub depth: u8,
    pub policy_digest: String,
    pub remaining_steps: u32,
    pub capabilities: BTreeSet<ActionKind>,
    pub skills: Vec<Value>,
    pub memories: Vec<Value>,
    pub event_tail: Vec<Value>,
    pub context_window: ContextWindow,
}

impl AgentProviderRequest {
    pub fn parse(data: &[u8]) -> Result<Self, ProcessError> {
        if data.len() > MAX_REQUEST_BYTES {
            return Err(ProcessError::new(
                "request_too_large",
                "provider request exceeds its byte bound",
            ));
        }
        let root = ProviderJsonValue::decode(data).map_err(|_| {
            ProcessError::new("invalid_request", "provider request JSON is invalid")
        })?;
        if root.as_object().is_none() {
            return Err(ProcessError::new(
                "invalid_request",
                "provider request must be a JSON object",
            ));
        }
        let request: Self = serde_json::from_slice(data).map_err(|_| {
            ProcessError::new("invalid_request", "provider request shape is invalid")
        })?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ProcessError> {
        if self.schema_version != REQUEST_SCHEMA {
            return Err(ProcessError::new(
                "invalid_request_schema",
                "provider request schema is unsupported",
            ));
        }
        for (value, label) in [(&self.run_id, "run id"), (&self.root_run_id, "root run id")] {
            validate_product_identifier(value, label)?;
        }
        if let Some(parent) = &self.parent_run_id {
            validate_product_identifier(parent, "parent run id")?;
        }
        if self.task.trim().is_empty() || self.task.len() > 1024 * 1024 {
            return Err(ProcessError::new(
                "invalid_request",
                "provider task is empty or oversized",
            ));
        }
        if !is_sha256(&self.repository_id) || !is_sha256(&self.policy_digest) {
            return Err(ProcessError::new(
                "invalid_request",
                "provider request digest is invalid",
            ));
        }
        if self.capabilities.is_empty() {
            return Err(ProcessError::new(
                "invalid_capabilities",
                "provider capabilities must not be empty",
            ));
        }
        if self.skills.len() > 512 || self.memories.len() > 512 || self.event_tail.len() > 512 {
            return Err(ProcessError::new(
                "invalid_request",
                "provider request collection exceeds its bound",
            ));
        }
        for value in self
            .skills
            .iter()
            .chain(self.memories.iter())
            .chain(self.event_tail.iter())
        {
            let value = ProviderJsonValue::try_from(value.clone()).map_err(|_| {
                ProcessError::new("invalid_request", "provider request JSON value is invalid")
            })?;
            value.validated().map_err(|_| {
                ProcessError::new("invalid_request", "provider request JSON value is invalid")
            })?;
        }
        Ok(())
    }
}

fn validate_product_identifier(value: &str, label: &str) -> Result<(), ProcessError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ProcessError::new(
            "invalid_request",
            "provider identifier is invalid",
        ));
    };
    if value.len() > 128
        || !first.is_ascii_lowercase()
        || !chars.all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.')
        })
    {
        let _ = label;
        return Err(ProcessError::new(
            "invalid_request",
            "provider identifier is invalid",
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Python {
        code: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    List {
        path: String,
        #[serde(default = "default_list_entries")]
        max_entries: usize,
    },
    Read {
        path: String,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        max_bytes: Option<usize>,
    },
    Search {
        query: String,
        #[serde(default = "default_dot")]
        path: String,
        #[serde(default)]
        max_hits: Option<usize>,
    },
    History {
        #[serde(default)]
        after_seq: u64,
        #[serde(default = "default_history_limit")]
        limit: usize,
    },
    Delegate {
        task: String,
    },
    Remember {
        content: String,
        cues: Vec<String>,
    },
    Complete {
        answer: String,
    },
    Fail {
        reason: String,
    },
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Python { .. } => ActionKind::Python,
            Self::List { .. } => ActionKind::List,
            Self::Read { .. } => ActionKind::Read,
            Self::Search { .. } => ActionKind::Search,
            Self::History { .. } => ActionKind::History,
            Self::Delegate { .. } => ActionKind::Delegate,
            Self::Remember { .. } => ActionKind::Remember,
            Self::Complete { .. } => ActionKind::Complete,
            Self::Fail { .. } => ActionKind::Fail,
        }
    }

    pub fn validate(&self) -> Result<(), ProcessError> {
        match self {
            Self::Python { code, timeout_ms } => {
                if code.len() > 16 * 1024 * 1024
                    || timeout_ms.is_some_and(|value| !(1..=3_600_000).contains(&value))
                {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "python action is invalid",
                    ));
                }
            }
            Self::List { path, max_entries } => {
                validate_relative_path(path)?;
                if !(1..=10_000).contains(max_entries) {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "list action is invalid",
                    ));
                }
            }
            Self::Read {
                path, max_bytes, ..
            } => {
                validate_relative_path(path)?;
                if max_bytes.is_some_and(|value| !(1..=16 * 1024 * 1024).contains(&value)) {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "read action is invalid",
                    ));
                }
            }
            Self::Search {
                query,
                path,
                max_hits,
            } => {
                validate_relative_path(path)?;
                if query.is_empty()
                    || query.len() > 4_096
                    || max_hits.is_some_and(|value| !(1..=100_000).contains(&value))
                {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "search action is invalid",
                    ));
                }
            }
            Self::History { limit, .. } => {
                if !(1..=512).contains(limit) {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "history action is invalid",
                    ));
                }
            }
            Self::Delegate { task } => {
                if task.trim().is_empty() || task.len() > 256 * 1024 {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "delegate action is invalid",
                    ));
                }
            }
            Self::Remember { content, cues } => {
                if content.trim().is_empty() || content.len() > 256 * 1024 || cues.len() > 64 {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "remember action is invalid",
                    ));
                }
                if cues
                    .iter()
                    .any(|cue| cue.trim().is_empty() || cue.len() > 256)
                {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "remember action is invalid",
                    ));
                }
            }
            Self::Complete { answer } => {
                if answer.trim().is_empty() || answer.len() > 4 * 1024 * 1024 {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "complete action is invalid",
                    ));
                }
            }
            Self::Fail { reason } => {
                if reason.trim().is_empty() || reason.len() > 64 * 1024 {
                    return Err(ProcessError::new(
                        "invalid_action",
                        "fail action is invalid",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn default_list_entries() -> usize {
    256
}

fn default_history_limit() -> usize {
    32
}

fn default_dot() -> String {
    ".".to_owned()
}

fn validate_relative_path(value: &str) -> Result<(), ProcessError> {
    if value.trim().is_empty() || Path::new(value).is_absolute() {
        return Err(ProcessError::new(
            "invalid_action",
            "action path is invalid",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(value).components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(ProcessError::new(
                    "invalid_action",
                    "action path is invalid",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(ProcessError::new(
            "invalid_action",
            "action path is invalid",
        ));
    }
    Ok(())
}

pub fn action_schema(
    capabilities: &BTreeSet<ActionKind>,
) -> Result<ProviderJsonValue, ProcessError> {
    if capabilities.is_empty() {
        return Err(ProcessError::new(
            "invalid_capabilities",
            "provider capabilities must not be empty",
        ));
    }
    if capabilities.len() == 1 {
        let kind = capabilities.iter().next().ok_or_else(|| {
            ProcessError::new("invalid_capabilities", "provider capabilities are invalid")
        })?;
        let variant = action_variant_schema(kind)?;
        return ProviderJsonValue::try_from(variant).map_err(|_| {
            ProcessError::new("internal_error", "action schema could not be encoded")
        });
    }
    let mut properties = Map::new();
    properties.insert(
        "type".to_owned(),
        json!({"type":"string","enum":capabilities.iter().map(|kind| kind.as_str()).collect::<Vec<_>>() }),
    );
    for kind in capabilities {
        let variant = action_variant_schema(kind)?;
        let Some(variant_properties) = variant.get("properties").and_then(Value::as_object) else {
            return Err(ProcessError::new(
                "internal_error",
                "action schema is malformed",
            ));
        };
        for (name, schema) in variant_properties {
            if name != "type" {
                properties
                    .entry(name.clone())
                    .or_insert_with(|| schema.clone());
            }
        }
    }
    let field_names = properties.keys().cloned().collect::<Vec<_>>();
    for name in field_names.iter().filter(|name| name.as_str() != "type") {
        let Some(schema) = properties.get_mut(name) else {
            return Err(ProcessError::new(
                "internal_error",
                "action schema is malformed",
            ));
        };
        if let Some(kind) = schema.get("type").and_then(Value::as_str) {
            schema["type"] = json!([kind, "null"]);
        }
    }
    let schema = json!({
        "type":"object",
        "properties":Value::Object(properties),
        "required":field_names,
        "additionalProperties":false
    });
    ProviderJsonValue::try_from(schema)
        .map_err(|_| ProcessError::new("internal_error", "action schema could not be encoded"))
}

fn action_variant_schema(kind: &ActionKind) -> Result<Value, ProcessError> {
    let kind_value = Value::String(kind.as_str().to_owned());
    let string = |max: usize| json!({"type":"string","maxLength":max});
    let integer =
        |minimum: u64, maximum: u64| json!({"type":"integer","minimum":minimum,"maximum":maximum});
    let nullable_integer = |minimum: u64, maximum: u64| json!({"type":["integer","null"],"minimum":minimum,"maximum":maximum});
    let (properties, required) = match kind {
        ActionKind::Python => (
            json!({"type":{"type":"string","enum":[kind_value]},"code":string(16*1024*1024),"timeout_ms":nullable_integer(1,3_600_000)}),
            json!(["type", "code", "timeout_ms"]),
        ),
        ActionKind::List => (
            json!({"type":{"type":"string","enum":[kind_value]},"path":string(4096),"max_entries":integer(1,10_000)}),
            json!(["type", "path", "max_entries"]),
        ),
        ActionKind::Read => (
            json!({"type":{"type":"string","enum":[kind_value]},"path":string(4096),"offset":integer(0,u64::MAX),"max_bytes":nullable_integer(1,16*1024*1024)}),
            json!(["type", "path", "offset", "max_bytes"]),
        ),
        ActionKind::Search => (
            json!({"type":{"type":"string","enum":[kind_value]},"query":string(4096),"path":string(4096),"max_hits":nullable_integer(1,100_000)}),
            json!(["type", "query", "path", "max_hits"]),
        ),
        ActionKind::History => (
            json!({"type":{"type":"string","enum":[kind_value]},"after_seq":integer(0,u64::MAX),"limit":integer(1,512)}),
            json!(["type", "after_seq", "limit"]),
        ),
        ActionKind::Delegate => (
            json!({"type":{"type":"string","enum":[kind_value]},"task":string(256*1024)}),
            json!(["type", "task"]),
        ),
        ActionKind::Remember => (
            json!({"type":{"type":"string","enum":[kind_value]},"content":string(256*1024),"cues":{"type":"array","maxItems":64,"items":string(256)}}),
            json!(["type", "content", "cues"]),
        ),
        ActionKind::Complete => (
            json!({"type":{"type":"string","enum":[kind_value]},"answer":string(4*1024*1024)}),
            json!(["type", "answer"]),
        ),
        ActionKind::Fail => (
            json!({"type":{"type":"string","enum":[kind_value]},"reason":string(64*1024)}),
            json!(["type", "reason"]),
        ),
    };
    Ok(json!({
        "type":"object",
        "properties":properties,
        "required":required,
        "additionalProperties":false
    }))
}

fn build_turn_request(
    request: &AgentProviderRequest,
    options: &CliOptions,
    timeout_milliseconds: u64,
) -> Result<ProviderTurnRequest, ProcessError> {
    if !(MIN_OPERATION_TIMEOUT_MS..=options.operation_timeout_ms).contains(&timeout_milliseconds) {
        return Err(ProcessError::new(
            "operation_timeout",
            "provider turn budget is outside the remaining operation deadline",
        ));
    }
    let schema = action_schema(&request.capabilities)?;
    let tool = ProviderToolDefinition::new(
        ACTION_TOOL_NAME,
        "Return exactly one RGXAMK action. Never emit text.",
        schema,
        true,
    )
    .map_err(|_| ProcessError::new("internal_error", "action tool could not be built"))?;
    let mut messages = Vec::new();
    messages.push(
        ProviderMessage::text(
            ProviderMessageRole::Developer,
            "You are the RGXAMK action planner. Use exactly one named rgxamk_action tool call. Do not emit any text. The tool action type must be one of the capabilities in the request.",
        )
        .map_err(|_| ProcessError::new("internal_error", "provider instruction is invalid"))?,
    );
    let request_json = serde_json::to_string(request)
        .map_err(|_| ProcessError::new("internal_error", "provider request context failed"))?;
    let user_context = format!(
        "Task:\n{}\n\nFull request JSON:\n{}",
        request.task, request_json
    );
    for chunk in split_text(&user_context, MAX_MESSAGE_BYTES) {
        messages.push(
            ProviderMessage::text(ProviderMessageRole::User, chunk).map_err(|_| {
                ProcessError::new("invalid_request", "provider context is oversized")
            })?,
        );
    }
    let constraints = ProviderRequestConstraints::new(
        ProviderDataCollectionPolicy::Deny,
        true,
        true,
        timeout_milliseconds,
        options.upstream_max_response_bytes,
        3,
        None,
    )
    .map_err(|_| ProcessError::new("invalid_arguments", "provider turn bounds are invalid"))?;
    ProviderTurnRequest::new(
        ProviderRequestId::new(request.run_id.clone())
            .map_err(|_| ProcessError::new("invalid_request", "provider request id is invalid"))?,
        ProviderSelection::new(
            BuiltInProviderId::codex(),
            options.account_id.clone(),
            options.model_id.clone(),
        ),
        messages,
        vec![tool],
        ProviderToolChoice::named(ACTION_TOOL_NAME)
            .map_err(|_| ProcessError::new("internal_error", "provider tool choice is invalid"))?,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Effort(rust_provider_kit_core::ProviderReasoningEffort::Low),
        None,
        constraints,
    )
    .map_err(|_| ProcessError::new("invalid_request", "provider turn request is invalid"))
}

fn split_text(value: &str, maximum_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < value.len() {
        let mut end = (start + maximum_bytes).min(value.len());
        while end > start && !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = value[start..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(index, _)| start + index);
        }
        chunks.push(value[start..end].to_owned());
        start = end;
    }
    chunks
}

#[derive(Debug, Default)]
pub struct EventAccumulator {
    started: bool,
    tool_call: Option<ProviderToolCall>,
    terminal: Option<ProviderTerminal>,
}

impl EventAccumulator {
    pub fn consume(&mut self, event: ProviderTurnEvent) -> Result<(), ProcessError> {
        if self.terminal.is_some() {
            return Err(ProcessError::new(
                "malformed_response",
                "provider emitted an event after terminal",
            ));
        }
        match event {
            ProviderTurnEvent::Started(_) => {
                if self.started {
                    return Err(ProcessError::new(
                        "malformed_response",
                        "duplicate start event",
                    ));
                }
                self.started = true;
            }
            ProviderTurnEvent::ReasoningDelta(_) => {}
            ProviderTurnEvent::TextDelta(_) => {
                return Err(ProcessError::new(
                    "unexpected_text",
                    "provider emitted text instead of the required action",
                ));
            }
            ProviderTurnEvent::ToolCall(call) => {
                if !self.started {
                    return Err(ProcessError::new(
                        "malformed_response",
                        "provider emitted a tool call before start",
                    ));
                }
                if self.tool_call.is_some() {
                    return Err(ProcessError::new(
                        "multiple_tool_calls",
                        "provider emitted multiple tool calls",
                    ));
                }
                if call.name() != ACTION_TOOL_NAME {
                    return Err(ProcessError::new(
                        "wrong_tool",
                        "provider emitted an unexpected tool",
                    ));
                }
                self.tool_call = Some(call);
            }
            ProviderTurnEvent::Terminal(terminal) => {
                if self.terminal.is_some() {
                    return Err(ProcessError::new(
                        "multiple_terminals",
                        "provider emitted multiple terminal events",
                    ));
                }
                if let ProviderTerminal::Failed(failure) = &terminal {
                    return Err(provider_failure_error(failure));
                }
                if matches!(terminal, ProviderTerminal::Cancelled) {
                    return Err(ProcessError::new(
                        "cancelled",
                        "provider turn did not complete successfully",
                    ));
                }
                self.terminal = Some(terminal);
            }
        }
        Ok(())
    }

    pub fn response(&self, capabilities: &BTreeSet<ActionKind>) -> Result<Vec<u8>, ProcessError> {
        if !self.started {
            return Err(ProcessError::new(
                "missing_start",
                "provider turn did not start",
            ));
        }
        let Some(ProviderTerminal::Completed(_completion)) = &self.terminal else {
            return Err(ProcessError::new(
                "missing_completion",
                "provider turn did not complete",
            ));
        };
        let Some(call) = &self.tool_call else {
            return Err(ProcessError::new(
                "missing_tool_call",
                "provider did not return the required action",
            ));
        };
        let value = serde_json::to_value(call.arguments())
            .map_err(|_| ProcessError::new("malformed_response", "tool arguments are invalid"))?;
        let value = normalize_action_value(value, capabilities)?;
        let action: Action = serde_json::from_value(value)
            .map_err(|_| ProcessError::new("malformed_response", "tool arguments are invalid"))?;
        action.validate()?;
        if !capabilities.contains(&action.kind()) {
            return Err(ProcessError::new(
                "capability_violation",
                "provider action is outside the request capabilities",
            ));
        }
        let response = json!({
            "schema_version": RESPONSE_SCHEMA,
            "action": action,
        });
        let mut encoded = serde_json::to_vec(&response).map_err(|_| {
            ProcessError::new("internal_error", "provider response encoding failed")
        })?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(ProcessError::new(
                "response_too_large",
                "provider response exceeds its byte bound",
            ));
        }
        encoded.push(b'\n');
        Ok(encoded)
    }
}

fn normalize_action_value(
    mut value: Value,
    capabilities: &BTreeSet<ActionKind>,
) -> Result<Value, ProcessError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| ProcessError::new("malformed_response", "tool arguments are invalid"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .and_then(parse_action_kind)
        .ok_or_else(|| ProcessError::new("malformed_response", "tool arguments are invalid"))?;
    if !capabilities.contains(&kind) {
        return Err(ProcessError::new(
            "capability_violation",
            "provider action is outside the request capabilities",
        ));
    }
    let expected = capabilities
        .iter()
        .flat_map(|candidate| action_fields(*candidate).iter().copied())
        .collect::<BTreeSet<_>>();
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(ProcessError::new(
            "malformed_response",
            "tool arguments are invalid",
        ));
    }
    let allowed = action_fields(kind);
    for key in expected.iter().filter(|key| !allowed.contains(key)) {
        if object.get(*key) != Some(&Value::Null) {
            return Err(ProcessError::new(
                "malformed_response",
                "tool arguments are invalid",
            ));
        }
        object.remove(*key);
    }
    Ok(value)
}

fn parse_action_kind(value: &str) -> Option<ActionKind> {
    match value {
        "python" => Some(ActionKind::Python),
        "list" => Some(ActionKind::List),
        "read" => Some(ActionKind::Read),
        "search" => Some(ActionKind::Search),
        "history" => Some(ActionKind::History),
        "delegate" => Some(ActionKind::Delegate),
        "remember" => Some(ActionKind::Remember),
        "complete" => Some(ActionKind::Complete),
        "fail" => Some(ActionKind::Fail),
        _ => None,
    }
}

fn action_fields(kind: ActionKind) -> &'static [&'static str] {
    match kind {
        ActionKind::Python => &["type", "code", "timeout_ms"],
        ActionKind::List => &["type", "path", "max_entries"],
        ActionKind::Read => &["type", "path", "offset", "max_bytes"],
        ActionKind::Search => &["type", "query", "path", "max_hits"],
        ActionKind::History => &["type", "after_seq", "limit"],
        ActionKind::Delegate => &["type", "task"],
        ActionKind::Remember => &["type", "content", "cues"],
        ActionKind::Complete => &["type", "answer"],
        ActionKind::Fail => &["type", "reason"],
    }
}

fn provider_failure_error(failure: &ProviderFailure) -> ProcessError {
    let code = match failure.provider_status_code() {
        Some(400) => "provider_http_400",
        Some(401) => "provider_http_401",
        Some(402) => "provider_http_402",
        Some(403) => "provider_http_403",
        Some(404) => "provider_http_404",
        Some(408) => "provider_http_408",
        Some(409) => "provider_http_409",
        Some(422) => "provider_http_422",
        Some(429) => "provider_http_429",
        Some(500) => "provider_http_500",
        Some(502) => "provider_http_502",
        Some(503) => "provider_http_503",
        Some(504) => "provider_http_504",
        Some(400..=499) => "provider_http_4xx",
        Some(500..=599) => "provider_http_5xx",
        _ => failure.code().as_str(),
    };
    ProcessError::new(code, "provider turn did not complete successfully")
        .with_provider_failure(failure)
}

fn failure_diagnostic_line(failure: &ProviderFailure) -> Option<String> {
    let mut value = Map::new();
    value.insert("version".to_owned(), Value::from(1_u8));
    value.insert(
        "failure_code".to_owned(),
        Value::String(failure.code().as_str().to_owned()),
    );
    value.insert(
        "retry_disposition".to_owned(),
        Value::String(failure_retry_disposition(failure).to_owned()),
    );
    if let Some(status) = failure.provider_status_code() {
        value.insert("status".to_owned(), Value::from(status));
    }
    if let Some(retry_after_ms) = failure.retry_after_milliseconds() {
        value.insert("retry_after_ms".to_owned(), Value::from(retry_after_ms));
    }
    if let Some(evidence) = failure.evidence() {
        if let Some(body_bytes) = evidence.body_bytes() {
            value.insert("body_bytes".to_owned(), Value::from(body_bytes));
        }
        if let Some(body_sha256) = evidence.body_sha256() {
            value.insert(
                "body_sha256".to_owned(),
                Value::String(body_sha256.to_owned()),
            );
        }
        if let Some(request_id) = evidence.upstream_request_id() {
            value.insert(
                "upstream_request_id".to_owned(),
                Value::String(request_id.to_owned()),
            );
        }
        if let Some(reset_at) = evidence.reset_at_unix_seconds() {
            value.insert("retry_at_unix_seconds".to_owned(), Value::from(reset_at));
        }
        if let Some(should_retry) = evidence.provider_should_retry() {
            value.insert(
                "provider_should_retry".to_owned(),
                Value::from(should_retry),
            );
        }
        if let Some(error_type) = evidence
            .remote_error_type()
            .filter(|value| SAFE_REMOTE_ERROR_TOKENS.contains(value))
        {
            value.insert(
                "remote_error_type".to_owned(),
                Value::String(error_type.to_owned()),
            );
        }
        if let Some(error_code) = evidence
            .remote_error_code()
            .filter(|value| SAFE_REMOTE_ERROR_TOKENS.contains(value))
        {
            value.insert(
                "remote_error_code".to_owned(),
                Value::String(error_code.to_owned()),
            );
        }
        if let Some(limit) = evidence.codex_primary() {
            value.insert("codex_primary".to_owned(), rate_limit_json(limit));
        }
        if let Some(limit) = evidence.codex_secondary() {
            value.insert("codex_secondary".to_owned(), rate_limit_json(limit));
        }
    }
    let encoded = serde_json::to_string(&value).ok()?;
    let line = format!("{FAILURE_DIAGNOSTIC_PREFIX}{encoded}");
    (line.len() <= MAX_FAILURE_DIAGNOSTIC_LINE_BYTES).then_some(line)
}

fn failure_retry_disposition(failure: &ProviderFailure) -> &'static str {
    let remote_token = |candidate: &str| {
        failure.evidence().is_some_and(|evidence| {
            evidence.remote_error_type() == Some(candidate)
                || evidence.remote_error_code() == Some(candidate)
        })
    };
    if failure
        .evidence()
        .and_then(|evidence| evidence.reset_at_unix_seconds())
        .is_some()
        || ["usage_limit_reached", "rate_limit_reached"]
            .into_iter()
            .any(remote_token)
        || failure
            .retry_after_milliseconds()
            .is_some_and(|delay| delay > 60_000)
    {
        return "wait_until_reset";
    }
    if [
        "authentication_error",
        "invalid_api_key",
        "insufficient_quota",
        "quota_exceeded",
        "billing_hard_limit_reached",
        "billing_error",
        "permission_error",
    ]
    .into_iter()
    .any(remote_token)
        || matches!(
            failure.code(),
            ProviderFailureCode::AuthenticationFailed
                | ProviderFailureCode::BillingUnavailable
                | ProviderFailureCode::PermissionDenied
                | ProviderFailureCode::CredentialRecoveryRequired
        )
    {
        return "user_action";
    }
    if failure
        .evidence()
        .and_then(|evidence| evidence.provider_should_retry())
        == Some(false)
    {
        return "do_not_retry";
    }
    if matches!(
        failure.code(),
        ProviderFailureCode::RateLimited
            | ProviderFailureCode::ServerFailed
            | ProviderFailureCode::TransportFailed
            | ProviderFailureCode::TimedOut
    ) {
        return "retry_soon";
    }
    "do_not_retry"
}

fn rate_limit_json(limit: &ProviderRateLimitEvidence) -> Value {
    let mut value = Map::new();
    if let Some(used) = limit.used() {
        value.insert("used".to_owned(), Value::from(used));
    }
    if let Some(window) = limit.window() {
        value.insert("window".to_owned(), Value::from(window));
    }
    if let Some(reset) = limit.reset() {
        value.insert("reset".to_owned(), Value::from(reset));
    }
    if let Some(credits) = limit.credits() {
        value.insert("credits".to_owned(), Value::from(credits));
    }
    if let Some(used_percent_millis) = limit.used_percent_millis() {
        value.insert(
            "used_percent_millis".to_owned(),
            Value::from(used_percent_millis),
        );
    }
    if let Some(window_minutes) = limit.window_minutes() {
        value.insert("window_minutes".to_owned(), Value::from(window_minutes));
    }
    Value::Object(value)
}

#[derive(Debug, Default)]
struct EphemeralCredentialStore {
    state: Mutex<EphemeralCredentialState>,
}

#[derive(Debug, Default)]
struct EphemeralCredentialState {
    record: Option<ProviderCredentialRecord>,
    material: Option<ProviderCredentialMaterial>,
}

#[async_trait]
impl ProviderCredentialStore for EphemeralCredentialStore {
    async fn stage(
        &self,
        request: &ProviderAccountRegistrationRequest,
        at: ProviderInstant,
    ) -> Result<ProviderCredentialRecord, ProviderFailure> {
        let mut state = self.state.lock();
        if state.record.is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "provider account already exists",
            ));
        }
        let reference = ProviderCredentialReference::new("rgxamk-ephemeral-1").map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "ephemeral credential reference is invalid",
            )
        })?;
        let record = ProviderCredentialRecord::new(
            reference,
            request.account_id().clone(),
            request.provider_id().clone(),
            request.label(),
            request.credential().source(),
            ProviderCredentialRecordState::Staged,
            request.endpoint().cloned(),
            at,
            at,
        )
        .map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "ephemeral credential record is invalid",
            )
        })?;
        state.record = Some(record.clone());
        state.material = Some(request.credential().clone());
        Ok(record)
    }

    async fn activate(
        &self,
        record: &ProviderCredentialRecord,
        at: ProviderInstant,
    ) -> Result<(), ProviderFailure> {
        let mut state = self.state.lock();
        let Some(current) = state.record.as_ref() else {
            return Err(recovery_failure());
        };
        if current != record || record.state() != ProviderCredentialRecordState::Staged {
            return Err(recovery_failure());
        }
        let active = ProviderCredentialRecord::new(
            record.reference().clone(),
            record.account_id().clone(),
            record.provider_id().clone(),
            record.label(),
            record.source(),
            ProviderCredentialRecordState::Active,
            record.endpoint().cloned(),
            record.created_at(),
            at,
        )
        .map_err(|_| recovery_failure())?;
        state.record = Some(active);
        Ok(())
    }

    async fn remove(&self, record: &ProviderCredentialRecord) -> Result<(), ProviderFailure> {
        let mut state = self.state.lock();
        if state
            .record
            .as_ref()
            .is_some_and(|current| current.reference() != record.reference())
        {
            return Err(recovery_failure());
        }
        state.record = None;
        state.material = None;
        Ok(())
    }

    async fn record(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderCredentialRecord>, ProviderFailure> {
        Ok(self
            .state
            .lock()
            .record
            .as_ref()
            .filter(|record| record.account_id() == account_id)
            .cloned())
    }

    async fn lease(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderCredentialLease, ProviderFailure> {
        let state = self.state.lock();
        let (Some(record), Some(material)) = (&state.record, &state.material) else {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is unavailable",
            ));
        };
        if record.account_id() != account_id
            || record.state() != ProviderCredentialRecordState::Active
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is unavailable",
            ));
        }
        ProviderCredentialLease::new(record.clone(), material.clone())
            .map_err(|_| recovery_failure())
    }

    async fn records(&self) -> Result<Vec<ProviderCredentialRecord>, ProviderFailure> {
        Ok(self.state.lock().record.iter().cloned().collect())
    }
}

fn recovery_failure() -> ProviderFailure {
    ProviderFailure::new(
        ProviderFailureCode::CredentialRecoveryRequired,
        "ephemeral credential state is inconsistent",
    )
}

async fn consume_registration(
    stream: &mut rust_provider_kit_core::ProviderAccountEventStream,
) -> Result<(), ProcessError> {
    let mut ready = false;
    while let Some(event) = stream.next().await {
        match event {
            rust_provider_kit_core::ProviderAccountPublicEvent::Ready(_) => {
                if ready {
                    return Err(ProcessError::new(
                        "registration_failed",
                        "duplicate registration terminal",
                    ));
                }
                ready = true;
            }
            rust_provider_kit_core::ProviderAccountPublicEvent::Failed(_)
            | rust_provider_kit_core::ProviderAccountPublicEvent::RecoveryRequired(_) => {
                return Err(ProcessError::new(
                    "registration_failed",
                    "provider registration failed",
                ));
            }
            rust_provider_kit_core::ProviderAccountPublicEvent::Staging
            | rust_provider_kit_core::ProviderAccountPublicEvent::Verifying
            | rust_provider_kit_core::ProviderAccountPublicEvent::Activating => {}
        }
    }
    if ready {
        Ok(())
    } else {
        Err(ProcessError::new(
            "registration_failed",
            "provider registration did not complete",
        ))
    }
}

async fn consume_execution(
    stream: &mut ProviderEventStream,
    capabilities: &BTreeSet<ActionKind>,
) -> Result<Vec<u8>, ProcessError> {
    let mut accumulator = EventAccumulator::default();
    while let Some(event) = stream.next().await {
        accumulator.consume(event)?;
    }
    accumulator.response(capabilities)
}

pub fn parse_args(args: &[String]) -> Result<CliOptions, ProcessError> {
    let mut values = BTreeMap::<&str, &str>::new();
    let mut index = 0usize;
    while index < args.len() {
        let flag = args[index].as_str();
        let key = match flag {
            "--account-id" => "account-id",
            "--model" => "model",
            "--auth-file" => "auth-file",
            "--codex-client-version" => "codex-client-version",
            "--operation-timeout-ms" => "operation-timeout-ms",
            "--upstream-max-response-bytes" => "upstream-max-response-bytes",
            "--failure-diagnostics" => "failure-diagnostics",
            _ => {
                return Err(ProcessError::new(
                    "invalid_arguments",
                    "unknown or malformed argument",
                ));
            }
        };
        let value = args
            .get(index + 1)
            .ok_or_else(|| ProcessError::new("invalid_arguments", "argument value is missing"))?;
        if value.is_empty() || value.contains('\0') || values.insert(key, value).is_some() {
            return Err(ProcessError::new(
                "invalid_arguments",
                "argument value is invalid",
            ));
        }
        index += 2;
    }
    let account_id = ProviderAccountId::new(required_arg(&values, "account-id")?)
        .map_err(|_| ProcessError::new("invalid_arguments", "account id is invalid"))?;
    let model_id = ProviderModelId::new(required_arg(&values, "model")?)
        .map_err(|_| ProcessError::new("invalid_arguments", "model is invalid"))?;
    let auth_file = PathBuf::from(required_arg(&values, "auth-file")?);
    if !auth_file.is_absolute() {
        return Err(ProcessError::new(
            "invalid_arguments",
            "auth file must be absolute",
        ));
    }
    let codex_client_version = required_arg(&values, "codex-client-version")?.to_owned();
    if codex_client_version.len() > 128
        || codex_client_version.is_empty()
        || !codex_client_version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return Err(ProcessError::new(
            "invalid_arguments",
            "codex client version is invalid",
        ));
    }
    let operation_timeout_ms = parse_u64_arg(&values, "operation-timeout-ms")?;
    if !(MIN_OPERATION_TIMEOUT_MS..=MAX_OPERATION_TIMEOUT_MS).contains(&operation_timeout_ms) {
        return Err(ProcessError::new(
            "invalid_arguments",
            "operation timeout is outside bounds",
        ));
    }
    let upstream_max_response_bytes = parse_usize_arg(&values, "upstream-max-response-bytes")?;
    if !(MIN_RESPONSE_BYTES..=MAX_RESPONSE_BYTES).contains(&upstream_max_response_bytes) {
        return Err(ProcessError::new(
            "invalid_arguments",
            "response bound is outside bounds",
        ));
    }
    let failure_diagnostics = match values.get("failure-diagnostics").copied() {
        None => FailureDiagnostics::Disabled,
        Some("v1") => FailureDiagnostics::V1,
        Some(_) => {
            return Err(ProcessError::new(
                "invalid_arguments",
                "failure diagnostics version is invalid",
            ));
        }
    };
    Ok(CliOptions {
        account_id,
        model_id,
        auth_file,
        codex_client_version,
        operation_timeout_ms,
        upstream_max_response_bytes,
        failure_diagnostics,
    })
}

fn required_arg<'a>(values: &'a BTreeMap<&str, &str>, key: &str) -> Result<&'a str, ProcessError> {
    values
        .get(key)
        .copied()
        .ok_or_else(|| ProcessError::new("invalid_arguments", "required argument is missing"))
}

fn parse_u64_arg(values: &BTreeMap<&str, &str>, key: &str) -> Result<u64, ProcessError> {
    required_arg(values, key)?
        .parse::<u64>()
        .map_err(|_| ProcessError::new("invalid_arguments", "numeric argument is invalid"))
}

fn parse_usize_arg(values: &BTreeMap<&str, &str>, key: &str) -> Result<usize, ProcessError> {
    required_arg(values, key)?
        .parse::<usize>()
        .map_err(|_| ProcessError::new("invalid_arguments", "numeric argument is invalid"))
}

async fn execute_live(
    request: &AgentProviderRequest,
    options: &CliOptions,
) -> Result<Vec<u8>, ProcessError> {
    let operation_deadline = Instant::now()
        .checked_add(Duration::from_millis(options.operation_timeout_ms))
        .ok_or_else(|| {
            ProcessError::new(
                "invalid_arguments",
                "operation timeout cannot be represented",
            )
        })?;
    let store = Arc::new(EphemeralCredentialStore::default());
    let runtime = ProviderRuntime::with_options(
        Arc::clone(&store) as Arc<dyn ProviderCredentialStore>,
        ProviderRuntimeOptions {
            codex_client_version: Some(options.codex_client_version.clone()),
        },
    )
    .map_err(|_| ProcessError::new("registration_failed", "provider runtime could not start"))?;
    let result = execute_with_runtime(&runtime, &store, request, options, operation_deadline).await;
    finalize_with_bounded_shutdown(
        result,
        operation_deadline,
        Duration::from_millis(CLEANUP_RESERVE_MS),
        runtime.shutdown(),
    )
    .await
}

async fn execute_with_runtime(
    runtime: &ProviderRuntime,
    store: &Arc<EphemeralCredentialStore>,
    request: &AgentProviderRequest,
    options: &CliOptions,
    operation_deadline: Instant,
) -> Result<Vec<u8>, ProcessError> {
    let credential = ProviderCredentialMaterial::external_auth_file(&options.auth_file)
        .map_err(|_| ProcessError::new("invalid_arguments", "auth file reference is invalid"))?;
    let registration = ProviderAccountRegistrationRequest::new(
        options.account_id.clone(),
        BuiltInProviderId::codex(),
        "rgxamk-native-provider",
        credential,
        None,
    )
    .map_err(|_| {
        ProcessError::new(
            "invalid_arguments",
            "provider registration request is invalid",
        )
    })?;
    let mut registration_stream =
        await_before_deadline(runtime.register(registration), operation_deadline).await?;
    await_before_deadline(
        consume_registration(&mut registration_stream),
        operation_deadline,
    )
    .await??;
    let lease = await_before_deadline(store.lease(&options.account_id), operation_deadline)
        .await?
        .map_err(|_| {
            ProcessError::new("registration_failed", "active credential read-back failed")
        })?;
    if lease.record().state() != ProviderCredentialRecordState::Active
        || lease.material().source()
            != rust_provider_kit_core::ProviderCredentialSource::ExternalAuthFileReference
    {
        return Err(ProcessError::new(
            "registration_failed",
            "active credential read-back failed",
        ));
    }
    let turn_timeout_ms = remaining_operation_timeout_ms(operation_deadline, options)?;
    let turn = build_turn_request(request, options, turn_timeout_ms)?;
    let mut events = await_before_deadline(runtime.execute(turn), operation_deadline).await?;
    await_before_deadline(
        consume_execution(&mut events, &request.capabilities),
        operation_deadline,
    )
    .await?
}

fn operation_timeout_error() -> ProcessError {
    ProcessError::new(
        "operation_timeout",
        "provider operation exceeded its deadline",
    )
}

async fn await_before_deadline<T, F>(future: F, deadline: Instant) -> Result<T, ProcessError>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(remaining_duration(deadline)?, future)
        .await
        .map_err(|_| operation_timeout_error())
}

async fn finalize_with_bounded_shutdown<T, F>(
    result: Result<T, ProcessError>,
    operation_deadline: Instant,
    cleanup_reserve: Duration,
    shutdown: F,
) -> Result<T, ProcessError>
where
    F: Future<Output = ()>,
{
    let operation_timed_out = result
        .as_ref()
        .err()
        .is_some_and(|error| error.code == "operation_timeout");
    let deadline_elapsed = Instant::now() >= operation_deadline;
    let shutdown_window = if operation_timed_out || deadline_elapsed {
        cleanup_reserve
    } else {
        remaining_duration(operation_deadline).unwrap_or_default()
    };
    if tokio::time::timeout(shutdown_window, shutdown)
        .await
        .is_err()
    {
        return Err(ProcessError::new(
            "operation_timeout",
            "provider runtime cleanup exceeded its deadline",
        ));
    }
    if deadline_elapsed && result.is_ok() {
        return Err(operation_timeout_error());
    }
    result
}

fn remaining_duration(deadline: Instant) -> Result<Duration, ProcessError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(operation_timeout_error())
    } else {
        Ok(remaining)
    }
}

fn remaining_operation_timeout_ms(
    deadline: Instant,
    options: &CliOptions,
) -> Result<u64, ProcessError> {
    let remaining = remaining_duration(deadline)?;
    let milliseconds = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    let bounded = milliseconds.min(options.operation_timeout_ms);
    if bounded < MIN_OPERATION_TIMEOUT_MS {
        return Err(operation_timeout_error());
    }
    Ok(bounded)
}

/// Run one process invocation.  The caller is responsible for emitting the
/// stable failure line and exit code; this function never writes diagnostics.
pub async fn run(args: &[String]) -> Result<Vec<u8>, ProcessError> {
    let options = parse_args(args)?;
    let failure_diagnostics = options.failure_diagnostics;
    let mut bytes = Vec::new();
    let read_limit = u64::try_from(MAX_REQUEST_BYTES)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| ProcessError::new("internal_error", "request bound is invalid"))?;
    let mut input = std::io::stdin().lock();
    std::io::Read::take(&mut input, read_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| ProcessError::new("invalid_request", "provider request could not be read"))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ProcessError::new(
            "request_too_large",
            "provider request exceeds its byte bound",
        ));
    }
    let request = AgentProviderRequest::parse(&bytes)?;
    execute_live(&request, &options)
        .await
        .map_err(|error| error.attach_failure_diagnostic(failure_diagnostics))
}

pub fn redact_diagnostic(_failure: &ProviderFailure) -> ProcessError {
    ProcessError::new("provider_failed", "provider execution failed")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use rust_provider_kit_core::{
        ProviderCompletion, ProviderFailureEvidence, ProviderInstant, ProviderResponseMetadata,
        ProviderTerminal,
    };

    fn request_json(capabilities: &[&str]) -> Vec<u8> {
        let caps = capabilities
            .iter()
            .map(|value| Value::String((*value).to_owned()))
            .collect::<Vec<_>>();
        serde_json::to_vec(&json!({
            "schema_version": REQUEST_SCHEMA,
            "run_id": "run-native-provider",
            "root_run_id": "run-native-provider",
            "parent_run_id": null,
            "task": "Return native-provider-ok",
            "repository_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "phase": "running",
            "step": 0,
            "depth": 0,
            "policy_digest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "remaining_steps": 1,
            "capabilities": caps,
            "skills": [],
            "memories": [],
            "event_tail": [],
            "context_window": {"encoded_bytes": 0, "events_omitted": 0, "memories_omitted": 0}
        }))
        .unwrap_or_default()
    }

    #[test]
    fn rejects_v1_v2_and_unknown_top_level_fields() {
        let mut value: Value =
            serde_json::from_slice(&request_json(&["complete"])).unwrap_or(Value::Null);
        for version in [
            "rgx.agent.provider-request.v1",
            "rgx.agent.provider-request.v2",
        ] {
            value["schema_version"] = Value::String(version.to_owned());
            assert_eq!(
                AgentProviderRequest::parse(&serde_json::to_vec(&value).unwrap_or_default())
                    .err()
                    .map(|e| e.code),
                Some("invalid_request_schema")
            );
        }
        value["schema_version"] = Value::String(REQUEST_SCHEMA.to_owned());
        value["unknown"] = Value::Bool(true);
        assert_eq!(
            AgentProviderRequest::parse(&serde_json::to_vec(&value).unwrap_or_default())
                .err()
                .map(|e| e.code),
            Some("invalid_request")
        );
    }

    #[test]
    fn action_schema_is_restricted_to_capabilities() {
        let request = AgentProviderRequest::parse(&request_json(&["complete"]))
            .unwrap_or_else(|_| unreachable!());
        let schema = action_schema(&request.capabilities).unwrap_or_else(|_| unreachable!());
        let encoded = serde_json::to_string(&schema).unwrap_or_default();
        assert!(encoded.contains("complete"));
        assert!(!encoded.contains("python"));
    }

    #[test]
    fn accumulator_translates_one_completed_tool_call() {
        let request = AgentProviderRequest::parse(&request_json(&["complete"]))
            .unwrap_or_else(|_| unreachable!());
        let account = request_selection_account();
        let model = ProviderModelId::new("gpt-5.6-sol").unwrap_or_else(|_| unreachable!());
        let metadata = ProviderResponseMetadata::new(
            ProviderRequestId::new("run-native-provider").unwrap_or_else(|_| unreachable!()),
            None,
            BuiltInProviderId::codex(),
            model,
            ProviderInstant::from_unix_milliseconds(1),
        )
        .unwrap_or_else(|_| unreachable!());
        let call = ProviderToolCall::new(
            "call-1",
            ACTION_TOOL_NAME,
            ProviderJsonValue::object([
                (
                    String::from("type"),
                    ProviderJsonValue::String("complete".to_owned()),
                ),
                (
                    String::from("answer"),
                    ProviderJsonValue::String("native-provider-ok".to_owned()),
                ),
            ]),
        )
        .unwrap_or_else(|_| unreachable!());
        let completion =
            ProviderCompletion::new(None, None, None, ProviderInstant::from_unix_milliseconds(2))
                .unwrap_or_else(|_| unreachable!());
        let mut accumulator = EventAccumulator::default();
        accumulator
            .consume(ProviderTurnEvent::Started(metadata))
            .unwrap_or_else(|_| unreachable!());
        accumulator
            .consume(ProviderTurnEvent::ToolCall(call))
            .unwrap_or_else(|_| unreachable!());
        accumulator
            .consume(ProviderTurnEvent::Terminal(ProviderTerminal::Completed(
                completion,
            )))
            .unwrap_or_else(|_| unreachable!());
        let output = accumulator
            .response(&request.capabilities)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(output.last().copied(), Some(b'\n'));
        assert!(String::from_utf8_lossy(&output).contains("native-provider-ok"));
        let _ = account;
    }

    fn request_selection_account() -> ProviderAccountId {
        ProviderAccountId::new("account-native").unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn accumulator_rejects_text_multiple_tools_and_capability_violation() {
        let mut accumulator = EventAccumulator::default();
        assert_eq!(
            accumulator
                .consume(ProviderTurnEvent::TextDelta("secret".to_owned()))
                .err()
                .map(|e| e.code),
            Some("unexpected_text")
        );
        let first = ProviderToolCall::new("a", ACTION_TOOL_NAME, ProviderJsonValue::object([]))
            .unwrap_or_else(|_| unreachable!());
        let second = ProviderToolCall::new("b", ACTION_TOOL_NAME, ProviderJsonValue::object([]))
            .unwrap_or_else(|_| unreachable!());
        accumulator.started = true;
        accumulator.tool_call = Some(first);
        assert_eq!(
            accumulator
                .consume(ProviderTurnEvent::ToolCall(second))
                .err()
                .map(|e| e.code),
            Some("multiple_tool_calls")
        );
        let mut action_accumulator = EventAccumulator::default();
        let call = ProviderToolCall::new(
            "c",
            ACTION_TOOL_NAME,
            ProviderJsonValue::object([
                (
                    String::from("type"),
                    ProviderJsonValue::String("fail".to_owned()),
                ),
                (
                    String::from("reason"),
                    ProviderJsonValue::String("no".to_owned()),
                ),
            ]),
        )
        .unwrap_or_else(|_| unreachable!());
        let completion =
            ProviderCompletion::new(None, None, None, ProviderInstant::from_unix_milliseconds(2))
                .unwrap_or_else(|_| unreachable!());
        let metadata = ProviderResponseMetadata::new(
            ProviderRequestId::new("run-native-provider").unwrap_or_else(|_| unreachable!()),
            None,
            BuiltInProviderId::codex(),
            ProviderModelId::new("gpt-5.6-sol").unwrap_or_else(|_| unreachable!()),
            ProviderInstant::from_unix_milliseconds(1),
        )
        .unwrap_or_else(|_| unreachable!());
        action_accumulator
            .consume(ProviderTurnEvent::Started(metadata))
            .unwrap_or_else(|_| unreachable!());
        action_accumulator
            .consume(ProviderTurnEvent::ToolCall(call))
            .unwrap_or_else(|_| unreachable!());
        action_accumulator
            .consume(ProviderTurnEvent::Terminal(ProviderTerminal::Completed(
                completion,
            )))
            .unwrap_or_else(|_| unreachable!());
        let capabilities = [ActionKind::Complete].into_iter().collect();
        assert_eq!(
            action_accumulator
                .response(&capabilities)
                .err()
                .map(|e| e.code),
            Some("capability_violation")
        );
    }

    #[test]
    fn cli_bounds_and_duplicates_are_rejected() {
        let args = vec!["--account-id".to_owned(), "a".to_owned()];
        assert_eq!(
            parse_args(&args).err().map(|e| e.code),
            Some("invalid_arguments")
        );
        let args = vec![
            "--account-id".to_owned(),
            "account".to_owned(),
            "--account-id".to_owned(),
            "account2".to_owned(),
        ];
        assert_eq!(
            parse_args(&args).err().map(|e| e.code),
            Some("invalid_arguments")
        );
        let valid = vec![
            "--account-id".to_owned(),
            "account".to_owned(),
            "--model".to_owned(),
            "gpt-5.6-sol".to_owned(),
            "--auth-file".to_owned(),
            "/tmp/auth.json".to_owned(),
            "--codex-client-version".to_owned(),
            "0.144.1".to_owned(),
            "--operation-timeout-ms".to_owned(),
            "100000".to_owned(),
            "--upstream-max-response-bytes".to_owned(),
            "1048576".to_owned(),
        ];
        assert!(parse_args(&valid).is_ok());
        let mut diagnostics = valid.clone();
        diagnostics.extend(["--failure-diagnostics".to_owned(), "v1".to_owned()]);
        assert_eq!(
            parse_args(&diagnostics)
                .unwrap_or_else(|_| unreachable!())
                .failure_diagnostics,
            FailureDiagnostics::V1
        );
        let mut legacy = valid.clone();
        legacy[8] = "--turn-timeout-ms".to_owned();
        assert_eq!(
            parse_args(&legacy).err().map(|e| e.code),
            Some("invalid_arguments")
        );
        let mut too_long = valid.clone();
        too_long[9] = "100001".to_owned();
        assert_eq!(
            parse_args(&too_long).err().map(|e| e.code),
            Some("invalid_arguments")
        );
    }

    #[test]
    fn flat_strict_schema_nulls_are_removed_only_for_other_action_fields() {
        let complete_and_fail = [ActionKind::Complete, ActionKind::Fail]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let normalized = normalize_action_value(
            json!({
                "type": "complete",
                "answer": "ok",
                "reason": null
            }),
            &complete_and_fail,
        )
        .unwrap_or(Value::Null);
        let action: Action = serde_json::from_value(normalized).unwrap_or_else(|_| unreachable!());
        assert_eq!(action.kind(), ActionKind::Complete);
        assert!(
            normalize_action_value(
                json!({
                    "type": "complete",
                    "answer": "ok",
                    "reason": "unexpected"
                }),
                &complete_and_fail
            )
            .is_err()
        );
        let list_only = [ActionKind::List].into_iter().collect::<BTreeSet<_>>();
        assert_eq!(
            normalize_action_value(
                json!({
                    "type": "list",
                    "path": "src"
                }),
                &list_only
            )
            .err()
            .map(|error| error.code),
            Some("malformed_response")
        );
        assert_eq!(
            normalize_action_value(
                json!({
                    "type": "complete",
                    "answer": "ok"
                }),
                &complete_and_fail
            )
            .err()
            .map(|error| error.code),
            Some("malformed_response")
        );
        let complete_only = [ActionKind::Complete].into_iter().collect::<BTreeSet<_>>();
        assert!(
            normalize_action_value(
                json!({
                    "type": "complete",
                    "answer": "ok",
                    "reason": null
                }),
                &complete_only
            )
            .is_err()
        );
        assert!(
            normalize_action_value(
                json!({
                    "type": "complete",
                    "answer": "ok",
                    "unknown": null
                }),
                &complete_and_fail
            )
            .is_err()
        );
        assert_eq!(
            normalize_action_value(
                json!({
                    "type": "complete",
                    "answer": "ok",
                    "reason": null,
                    "unknown": null
                }),
                &complete_and_fail
            )
            .err()
            .map(|error| error.code),
            Some("malformed_response")
        );
    }

    #[tokio::test]
    async fn derived_turn_budget_is_positive_bounded_and_not_reset_after_a_stage() {
        let args = vec![
            "--account-id".to_owned(),
            "account".to_owned(),
            "--model".to_owned(),
            "gpt-5.6-sol".to_owned(),
            "--auth-file".to_owned(),
            "/tmp/auth.json".to_owned(),
            "--codex-client-version".to_owned(),
            "0.144.1".to_owned(),
            "--operation-timeout-ms".to_owned(),
            "100000".to_owned(),
            "--upstream-max-response-bytes".to_owned(),
            "1048576".to_owned(),
        ];
        let options = parse_args(&args).unwrap_or_else(|_| unreachable!());
        let deadline = Instant::now() + Duration::from_secs(2);
        let initial_budget =
            remaining_operation_timeout_ms(deadline, &options).unwrap_or_else(|_| unreachable!());
        assert!(initial_budget > 0);
        assert!(initial_budget <= options.operation_timeout_ms);
        await_before_deadline(tokio::time::sleep(Duration::from_millis(20)), deadline)
            .await
            .unwrap_or_else(|_| unreachable!());
        let remaining_budget =
            remaining_operation_timeout_ms(deadline, &options).unwrap_or_else(|_| unreachable!());
        assert!(remaining_budget > 0);
        assert!(remaining_budget < initial_budget);
        assert!(remaining_budget < options.operation_timeout_ms);
        let expired = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        assert_eq!(
            remaining_operation_timeout_ms(expired, &options)
                .err()
                .map(|error| error.code),
            Some("operation_timeout")
        );
    }

    #[test]
    fn live_turn_contract_bounds_automatic_retries() {
        let request = AgentProviderRequest::parse(&request_json(&["complete"]))
            .unwrap_or_else(|_| unreachable!());
        let args = vec![
            "--account-id".to_owned(),
            "account".to_owned(),
            "--model".to_owned(),
            "gpt-5.6-sol".to_owned(),
            "--auth-file".to_owned(),
            "/tmp/auth.json".to_owned(),
            "--codex-client-version".to_owned(),
            "0.144.1".to_owned(),
            "--operation-timeout-ms".to_owned(),
            "100000".to_owned(),
            "--upstream-max-response-bytes".to_owned(),
            "1048576".to_owned(),
        ];
        let options = parse_args(&args).unwrap_or_else(|_| unreachable!());
        let turn =
            build_turn_request(&request, &options, 100_000).unwrap_or_else(|_| unreachable!());
        assert_eq!(turn.constraints().maximum_retry_attempts(), 3);
    }

    #[test]
    fn usage_limit_diagnostic_requires_wait_until_reset() {
        let evidence = rust_provider_kit_core::ProviderFailureEvidence::new(2, "a".repeat(64))
            .unwrap_or_else(|_| unreachable!())
            .with_remote_error_type("usage_limit_reached")
            .unwrap_or_else(|_| unreachable!())
            .with_reset_at_unix_seconds(1_738_888_888)
            .unwrap_or_else(|_| unreachable!());
        let failure = ProviderFailure::new(ProviderFailureCode::RateLimited, "limited")
            .with_status(429)
            .unwrap_or_else(|_| unreachable!())
            .with_evidence(evidence);
        let line = failure_diagnostic_line(&failure).unwrap_or_else(|| unreachable!());
        let payload = line
            .strip_prefix(FAILURE_DIAGNOSTIC_PREFIX)
            .unwrap_or_else(|| unreachable!());
        let value: Value = serde_json::from_str(payload).unwrap_or_else(|_| unreachable!());
        assert_eq!(value["retry_disposition"], "wait_until_reset");
        assert_eq!(value["retry_at_unix_seconds"], 1_738_888_888_u64);
    }

    #[test]
    fn transient_and_account_failures_have_distinct_retry_dispositions() {
        let transient = ProviderFailure::new(ProviderFailureCode::ServerFailed, "unavailable")
            .with_status(503)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(failure_retry_disposition(&transient), "retry_soon");

        let account = ProviderFailure::new(
            ProviderFailureCode::CredentialRecoveryRequired,
            "reauthentication required",
        );
        assert_eq!(failure_retry_disposition(&account), "user_action");

        let quota = ProviderFailure::new(ProviderFailureCode::RateLimited, "quota")
            .with_status(429)
            .unwrap_or_else(|_| unreachable!())
            .with_evidence(
                ProviderFailureEvidence::new(2, "a".repeat(64))
                    .unwrap_or_else(|_| unreachable!())
                    .with_remote_error_type("insufficient_quota")
                    .unwrap_or_else(|_| unreachable!()),
            );
        assert_eq!(failure_retry_disposition(&quota), "user_action");

        let long_delay = ProviderFailure::new(ProviderFailureCode::RateLimited, "wait")
            .with_status(429)
            .unwrap_or_else(|_| unreachable!())
            .with_retry_after(60_001);
        assert_eq!(failure_retry_disposition(&long_delay), "wait_until_reset");

        let no_retry = ProviderFailure::new(ProviderFailureCode::ServerFailed, "stop")
            .with_status(503)
            .unwrap_or_else(|_| unreachable!())
            .with_evidence(
                ProviderFailureEvidence::new(2, "a".repeat(64))
                    .unwrap_or_else(|_| unreachable!())
                    .with_provider_should_retry(false),
            );
        assert_eq!(failure_retry_disposition(&no_retry), "do_not_retry");
    }

    #[tokio::test]
    async fn slow_stage_returns_operation_timeout_before_deadline() {
        let deadline = Instant::now() + Duration::from_millis(5);
        let result =
            await_before_deadline(tokio::time::sleep(Duration::from_millis(50)), deadline).await;
        assert_eq!(
            result.err().map(|error| error.code),
            Some("operation_timeout")
        );
    }

    #[tokio::test]
    async fn timeout_finalization_attempts_shutdown_with_bounded_reserve() {
        let attempted = Arc::new(AtomicBool::new(false));
        let attempted_by_shutdown = Arc::clone(&attempted);
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap_or_else(Instant::now);
        let started = Instant::now();
        let result = finalize_with_bounded_shutdown(
            Err::<(), _>(operation_timeout_error()),
            expired,
            Duration::from_millis(5),
            async move {
                attempted_by_shutdown.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            },
        )
        .await;
        assert!(attempted.load(Ordering::SeqCst));
        assert_eq!(
            result.err().map(|error| error.code),
            Some("operation_timeout")
        );
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn diagnostics_never_include_provider_failure_details() {
        let failure = ProviderFailure::new(
            ProviderFailureCode::AuthenticationFailed,
            "Bearer super-secret-token",
        );
        let diagnostic = redact_diagnostic(&failure);
        assert!(!diagnostic.message.contains("super-secret-token"));
        assert!(!diagnostic.message.contains("Bearer"));
    }

    #[test]
    fn opt_in_failure_diagnostic_is_compact_typed_and_prefixed() {
        for unsafe_request_id in [
            "sk-proj-secret-token",
            "sk_secret_token",
            "Bearer-token",
            "a.b.c",
            "unknown-id",
        ] {
            let evidence =
                ProviderFailureEvidence::new(12, "a".repeat(64)).unwrap_or_else(|_| unreachable!());
            assert!(
                evidence
                    .with_upstream_request_id(unsafe_request_id)
                    .is_err()
            );
        }
        let evidence = ProviderFailureEvidence::new(12, "a".repeat(64))
            .unwrap_or_else(|_| unreachable!())
            .with_upstream_request_id("upstream-req")
            .unwrap_or_else(|_| unreachable!())
            .with_remote_error_type("rate_limit_error")
            .unwrap_or_else(|_| unreachable!())
            .with_remote_error_code("usage_limit_reached")
            .unwrap_or_else(|_| unreachable!())
            .with_codex_primary(
                ProviderRateLimitEvidence::new(Some(1), Some(60), Some(1_700_000_000), Some(2))
                    .unwrap_or_else(|_| unreachable!()),
            );
        let failure = ProviderFailure::new(ProviderFailureCode::RateLimited, "secret body")
            .with_status(429)
            .unwrap_or_else(|_| unreachable!())
            .with_retry_after(2_000)
            .with_evidence(evidence);
        let line = failure_diagnostic_line(&failure).unwrap_or_else(|| unreachable!());
        assert!(line.starts_with(FAILURE_DIAGNOSTIC_PREFIX));
        assert!(line.len() <= MAX_FAILURE_DIAGNOSTIC_LINE_BYTES);
        assert!(!line.contains("secret body"));
        assert!(!line.contains("Bearer"));
        let json = &line[FAILURE_DIAGNOSTIC_PREFIX.len()..];
        let value: Value = serde_json::from_str(json).unwrap_or_else(|_| unreachable!());
        assert_eq!(
            value.get("failure_code").and_then(Value::as_str),
            Some("rate_limited")
        );
        assert_eq!(value.get("status").and_then(Value::as_u64), Some(429));
        assert_eq!(
            value.get("upstream_request_id").and_then(Value::as_str),
            Some("upstream-req")
        );
        assert_eq!(
            value.get("remote_error_code").and_then(Value::as_str),
            Some("usage_limit_reached")
        );
    }

    #[test]
    fn opt_in_failure_diagnostic_filters_direct_evidence_remote_fields() {
        let evidence = ProviderFailureEvidence::new(12, "a".repeat(64))
            .unwrap_or_else(|_| unreachable!())
            .with_upstream_request_id("upstream-req")
            .unwrap_or_else(|_| unreachable!())
            .with_remote_error_type("sk-proj-secret-token")
            .unwrap_or_else(|_| unreachable!())
            .with_remote_error_code("unknown_remote_code")
            .unwrap_or_else(|_| unreachable!())
            .with_codex_primary(
                ProviderRateLimitEvidence::new(Some(4), Some(60), Some(1_700_000_000), Some(2))
                    .unwrap_or_else(|_| unreachable!()),
            );
        let failure = ProviderFailure::new(ProviderFailureCode::RateLimited, "secret body")
            .with_status(429)
            .unwrap_or_else(|_| unreachable!())
            .with_retry_after(2_000)
            .with_evidence(evidence);
        let line = failure_diagnostic_line(&failure).unwrap_or_else(|| unreachable!());
        let json = &line[FAILURE_DIAGNOSTIC_PREFIX.len()..];
        let value: Value = serde_json::from_str(json).unwrap_or_else(|_| unreachable!());
        assert_eq!(value.get("body_bytes").and_then(Value::as_u64), Some(12));
        assert_eq!(
            value.get("body_sha256").and_then(Value::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            value.get("upstream_request_id").and_then(Value::as_str),
            Some("upstream-req")
        );
        assert_eq!(
            value.get("retry_after_ms").and_then(Value::as_u64),
            Some(2_000)
        );
        assert!(value.get("remote_error_type").is_none());
        assert!(value.get("remote_error_code").is_none());
        assert_eq!(
            value
                .get("codex_primary")
                .and_then(|value| value.get("used"))
                .and_then(Value::as_u64),
            Some(4)
        );
        assert!(!line.contains("sk-proj-secret-token"));
        assert!(!line.contains("unknown_remote_code"));
        assert!(!line.contains("secret body"));
    }
}
