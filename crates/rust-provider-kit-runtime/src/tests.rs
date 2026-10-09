use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::Method;
use parking_lot::Mutex as ParkingMutex;
use rust_provider_kit_core::*;
use tempfile::tempdir;
use url::Url;

use crate::adapter::ProviderAdapter;
use crate::adapters::{OpenAiResponsesAdapter, OpenAiResponsesKind};
use crate::http_transport::{
    ProviderHttpRequest, ProviderHttpResponse, ProviderHttpResponseControl, ProviderHttpTransport,
    ProviderHttpUnaryResponse, ProviderTransportControl, ProviderTransportError,
};
use crate::in_memory_credential_store::InMemoryProviderCredentialStore;
use crate::openrouter_oauth::OpenRouterOAuthRegistrationRequest;
use crate::registry::BuiltInProviderRegistry;
use crate::runtime::{ProviderRuntime, ProviderRuntimeOptions};
use crate::secure_file::SecureRegularFileReader;
use crate::sse::{ServerSentEvent, ServerSentEventDecoder};
use crate::wire::{
    ProviderDecodedEvent, ProviderToolArgumentAccumulator, http_failure_parts, make_json_request,
    parse_model_catalog, transport_failure,
};

fn schema() -> ProviderJsonValue {
    json_object([
        ("type", ProviderJsonValue::from("object")),
        (
            "properties",
            json_object([(
                "answer",
                json_object([("type", ProviderJsonValue::from("string"))]),
            )]),
        ),
    ])
}

fn request_for(provider_id: ProviderId) -> Result<ProviderTurnRequest, ProviderCoreError> {
    request_for_reasoning(
        provider_id,
        ProviderReasoningPolicy::Effort(ProviderReasoningEffort::High),
    )
}

fn request_for_reasoning(
    provider_id: ProviderId,
    reasoning: ProviderReasoningPolicy,
) -> Result<ProviderTurnRequest, ProviderCoreError> {
    request_for_reasoning_and_tool_choice(
        provider_id,
        reasoning,
        ProviderToolChoice::named("lookup")?,
    )
}

fn request_for_reasoning_and_tool_choice(
    provider_id: ProviderId,
    reasoning: ProviderReasoningPolicy,
    tool_choice: ProviderToolChoice,
) -> Result<ProviderTurnRequest, ProviderCoreError> {
    let account_id = ProviderAccountId::new(format!("{}-account", provider_id.as_str()))?;
    let selection =
        ProviderSelection::new(provider_id, account_id, ProviderModelId::new("model-test")?);
    let tool = ProviderToolDefinition::new("lookup", "Look up a value", schema(), true)?;
    ProviderTurnRequest::new(
        ProviderRequestId::new("request-runtime-1")?,
        selection,
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        vec![tool],
        tool_choice,
        ProviderOutputRequirement::application_validated_json("answer", schema())?,
        reasoning,
        None,
        ProviderRequestConstraints::new(
            ProviderDataCollectionPolicy::Deny,
            true,
            true,
            30_000,
            2 * 1_024 * 1_024,
            2,
            Some(4_096),
        )?,
    )
}

fn request_with_tools(
    provider_id: ProviderId,
    request_id: &str,
    tool_names: &[&str],
    tool_choice: ProviderToolChoice,
) -> Result<ProviderTurnRequest, ProviderCoreError> {
    let account_id = ProviderAccountId::new(format!("{}-account", provider_id.as_str()))?;
    let selection =
        ProviderSelection::new(provider_id, account_id, ProviderModelId::new("model-test")?);
    let tools = tool_names
        .iter()
        .map(|name| ProviderToolDefinition::new(*name, format!("Use {name}"), schema(), true))
        .collect::<Result<Vec<_>, _>>()?;
    ProviderTurnRequest::new(
        ProviderRequestId::new(request_id)?,
        selection,
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        tools,
        tool_choice,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )
}

#[derive(Clone)]
struct ScriptedChatToolTransport {
    tool_names: Arc<ParkingMutex<VecDeque<String>>>,
}

struct NoopTransportControl;

#[async_trait]
impl ProviderTransportControl for NoopTransportControl {
    fn cancel(&self) {}

    async fn wait_for_termination(&self) {}
}

#[async_trait]
impl ProviderHttpTransport for ScriptedChatToolTransport {
    async fn open(
        &self,
        _request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError> {
        let tool_name = self
            .tool_names
            .lock()
            .pop_front()
            .ok_or(ProviderTransportError::Failed)?;
        let tool_event = serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"}
                }]},
                "finish_reason": null
            }]
        });
        let finish_event = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        });
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::to_string(&tool_event).map_err(|_| ProviderTransportError::Failed)?,
            serde_json::to_string(&finish_event).map_err(|_| ProviderTransportError::Failed)?,
        );
        ProviderHttpResponse::new(
            200,
            BTreeMap::from([("content-type".to_owned(), "text/event-stream".to_owned())]),
            Box::pin(stream::iter([Ok::<_, ProviderTransportError>(
                Bytes::from(body),
            )])),
            ProviderHttpResponseControl::new(Arc::new(NoopTransportControl)),
        )
    }
}

fn request_for_account(
    provider_id: ProviderId,
    account_id: ProviderAccountId,
    request_id: &str,
) -> Result<ProviderTurnRequest, ProviderCoreError> {
    let selection =
        ProviderSelection::new(provider_id, account_id, ProviderModelId::new("model-test")?);
    ProviderTurnRequest::new(
        ProviderRequestId::new(request_id)?,
        selection,
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )
}

fn active_lease(
    provider_id: ProviderId,
    endpoint: Option<ProviderEndpointConfiguration>,
) -> Result<ProviderCredentialLease, ProviderCoreError> {
    let account_id = ProviderAccountId::new(format!("{}-account", provider_id.as_str()))?;
    let material = ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?;
    let record = ProviderCredentialRecord::new(
        ProviderCredentialReference::new(format!("{}-credential", provider_id.as_str()))?,
        account_id,
        provider_id,
        "Primary",
        material.source(),
        ProviderCredentialRecordState::Active,
        endpoint,
        ProviderInstant::from_unix_milliseconds(10),
        ProviderInstant::from_unix_milliseconds(20),
    )?;
    ProviderCredentialLease::new(record, material)
}

fn event(event: Option<&str>, data: &str) -> ServerSentEvent {
    ServerSentEvent {
        event: event.map(str::to_owned),
        data: data.to_owned(),
    }
}

#[derive(Clone, Default)]
struct CapturingUnaryTransport {
    request: Arc<Mutex<Option<ProviderHttpRequest>>>,
}

impl CapturingUnaryTransport {
    fn captured_request(&self) -> Result<ProviderHttpRequest, Box<dyn Error>> {
        let guard = self
            .request
            .lock()
            .map_err(|_| "capturing transport lock was poisoned")?;
        guard
            .clone()
            .ok_or_else(|| "transport did not receive a request".into())
    }
}

#[async_trait]
impl ProviderHttpTransport for CapturingUnaryTransport {
    async fn open(
        &self,
        _request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError> {
        Err(ProviderTransportError::Failed)
    }

    async fn send(
        &self,
        request: ProviderHttpRequest,
    ) -> Result<ProviderHttpUnaryResponse, ProviderTransportError> {
        let mut guard = self
            .request
            .lock()
            .map_err(|_| ProviderTransportError::Failed)?;
        *guard = Some(request);
        Ok(ProviderHttpUnaryResponse {
            status_code: 200,
            headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            body: br#"{"data":[{"id":"MiniMax-M2"}]}"#.to_vec(),
        })
    }
}

#[test]
fn registry_exposes_exact_supported_set_and_rejects_unknown() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let actual = registry
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.id().as_str().to_owned())
        .collect::<Vec<_>>();
    let mut expected = BuiltInProviderId::all()
        .iter()
        .map(|identifier| identifier.as_str().to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    let unknown = ProviderId::new("unknown-provider")?;
    let failure = registry
        .adapter(&unknown)
        .err()
        .ok_or("unknown provider was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ProviderUnsupported);
    let codex = registry
        .descriptors()
        .into_iter()
        .find(|descriptor| descriptor.id() == &BuiltInProviderId::codex())
        .ok_or("Codex descriptor is missing")?;
    assert_eq!(codex.display_name(), "Codex (ChatGPT subscription)");
    Ok(())
}

#[tokio::test]
async fn runtime_rejects_external_auth_files_outside_codex() -> Result<(), Box<dyn Error>> {
    let runtime = ProviderRuntime::with_components(
        Arc::new(InMemoryProviderCredentialStore::default()),
        Arc::new(CapturingUnaryTransport::default()),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let request = ProviderAccountRegistrationRequest::new(
        ProviderAccountId::new("openai-external-auth")?,
        BuiltInProviderId::open_ai(),
        "OpenAI",
        ProviderCredentialMaterial::external_auth_file("/tmp/openai-auth.json")?,
        None,
    )?;
    let mut events = runtime.register(request).await;
    let Some(ProviderAccountPublicEvent::Failed(failure)) = events.next().await else {
        return Err("non-Codex external auth file was not rejected".into());
    };
    assert_eq!(failure.code(), ProviderFailureCode::AuthenticationFailed);
    assert!(events.next().await.is_none());
    runtime.shutdown().await;
    Ok(())
}

#[test]
fn runtime_rejects_an_invalid_configured_codex_version_during_construction()
-> Result<(), Box<dyn Error>> {
    let failure = ProviderRuntime::with_components(
        Arc::new(InMemoryProviderCredentialStore::default()),
        Arc::new(CapturingUnaryTransport::default()),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions {
            codex_client_version: Some("not a version".to_owned()),
        },
    )
    .err()
    .ok_or("invalid Codex client version was accepted during construction")?;
    assert_eq!(failure.code(), ProviderFailureCode::InvalidRequest);
    Ok(())
}

#[tokio::test]
async fn codex_rejects_unqualified_continuation_and_output_limit_before_credentials()
-> Result<(), Box<dyn Error>> {
    let adapter = OpenAiResponsesAdapter::new(OpenAiResponsesKind::Codex)?;
    let lease = active_lease(BuiltInProviderId::codex(), None)?;

    let output_limited = request_for(BuiltInProviderId::codex())?;
    let failure = adapter
        .make_execution_request(&output_limited, &lease)
        .await
        .err()
        .ok_or("Codex accepted an unqualified output-token limit")?;
    assert_eq!(failure.code(), ProviderFailureCode::CapabilityMismatch);

    let account_id = ProviderAccountId::new("codex-continuation-account")?;
    let continuation =
        ProviderContinuation::new(BuiltInProviderId::codex(), account_id.clone(), "response-1")?;
    let continuation_request = ProviderTurnRequest::new(
        ProviderRequestId::new("codex-continuation-request")?,
        ProviderSelection::new(
            BuiltInProviderId::codex(),
            account_id,
            ProviderModelId::new("model-test")?,
        ),
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        Some(continuation),
        ProviderRequestConstraints::default(),
    )?;
    let failure = adapter
        .make_execution_request(&continuation_request, &lease)
        .await
        .err()
        .ok_or("Codex accepted an unqualified continuation")?;
    assert_eq!(failure.code(), ProviderFailureCode::CapabilityMismatch);
    Ok(())
}

#[tokio::test]
async fn codex_omits_empty_instructions_and_uses_caller_owned_history() -> Result<(), Box<dyn Error>>
{
    let directory = tempdir()?;
    let auth_path = directory.path().join("auth.json");
    fs::write(
        &auth_path,
        r#"{"tokens":{"access_token":"token","account_id":"account"}}"#,
    )?;
    let account_id = ProviderAccountId::new("codex-history-account")?;
    let material = ProviderCredentialMaterial::external_auth_file(&auth_path)?;
    let record = ProviderCredentialRecord::new(
        ProviderCredentialReference::new("codex-history-ref")?,
        account_id.clone(),
        BuiltInProviderId::codex(),
        "Codex",
        material.source(),
        ProviderCredentialRecordState::Active,
        None,
        ProviderInstant::from_unix_milliseconds(1),
        ProviderInstant::from_unix_milliseconds(1),
    )?;
    let lease = ProviderCredentialLease::new(record, material)?;
    let request = ProviderTurnRequest::new(
        ProviderRequestId::new("codex-history-request")?,
        ProviderSelection::new(
            BuiltInProviderId::codex(),
            account_id,
            ProviderModelId::new("model-test")?,
        ),
        vec![
            ProviderMessage::text(ProviderMessageRole::User, "remember token")?,
            ProviderMessage::text(ProviderMessageRole::Assistant, "token")?,
            ProviderMessage::text(ProviderMessageRole::User, "repeat token")?,
        ],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )?;
    let wire = OpenAiResponsesAdapter::new(OpenAiResponsesKind::Codex)?
        .make_execution_request(&request, &lease)
        .await?;
    let body = ProviderJsonValue::decode(wire.body())?;
    assert!(body.get("instructions").is_none());
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(
        body.get("input")
            .and_then(ProviderJsonValue::as_array)
            .map(|items| items.len()),
        Some(3)
    );
    Ok(())
}

#[tokio::test]
async fn responses_history_encodes_tool_call_and_raw_error_result() -> Result<(), Box<dyn Error>> {
    let account_id = ProviderAccountId::new("openai-history-account")?;
    let request = ProviderTurnRequest::new(
        ProviderRequestId::new("openai-history-request")?,
        ProviderSelection::new(
            BuiltInProviderId::open_ai(),
            account_id.clone(),
            ProviderModelId::new("model-test")?,
        ),
        vec![
            ProviderMessage::text(ProviderMessageRole::User, "inspect")?,
            ProviderMessage::new(
                ProviderMessageRole::Assistant,
                vec![ProviderMessageContent::tool_call(ProviderToolCall::new(
                    "call-1",
                    "lookup",
                    json_object([("path", ProviderJsonValue::from("README.md"))]),
                )?)],
            )?,
            ProviderMessage::new(
                ProviderMessageRole::Tool,
                vec![ProviderMessageContent::tool_result_with_status(
                    "call-1",
                    "lookup",
                    ProviderJsonValue::from("permission denied"),
                    true,
                )?],
            )?,
            ProviderMessage::text(ProviderMessageRole::User, "continue")?,
        ],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )?;
    let lease = active_lease(BuiltInProviderId::open_ai(), None)?;
    let wire = OpenAiResponsesAdapter::new(OpenAiResponsesKind::OpenAi)?
        .make_execution_request(&request, &lease)
        .await?;
    let body = ProviderJsonValue::decode(wire.body())?;
    let input = body
        .get("input")
        .and_then(ProviderJsonValue::as_array)
        .ok_or("Responses request lacks input")?;
    assert_eq!(input.len(), 4);
    assert_eq!(
        input[1].get("type").and_then(ProviderJsonValue::as_str),
        Some("function_call")
    );
    assert_eq!(
        input[1]
            .get("arguments")
            .and_then(ProviderJsonValue::as_str),
        Some(r#"{"path":"README.md"}"#)
    );
    assert_eq!(
        input[2].get("output").and_then(ProviderJsonValue::as_str),
        Some("permission denied")
    );
    Ok(())
}

#[tokio::test]
async fn responses_replays_only_matching_opaque_native_output() -> Result<(), Box<dyn Error>> {
    let account_id = ProviderAccountId::new("openai-native-history-account")?;
    let request = ProviderTurnRequest::new(
        ProviderRequestId::new("openai-native-history-request")?,
        ProviderSelection::new(
            BuiltInProviderId::open_ai(),
            account_id.clone(),
            ProviderModelId::new("model-test")?,
        ),
        vec![
            ProviderMessage::text(ProviderMessageRole::User, "inspect")?,
            ProviderMessage::new(
                ProviderMessageRole::Assistant,
                vec![
                    ProviderMessageContent::tool_call(ProviderToolCall::new(
                        "call-1",
                        "lookup",
                        ProviderJsonValue::decode(br#"{}"#)?,
                    )?),
                    ProviderMessageContent::native_state(ProviderNativeState::new(
                        "openai.responses.output.v1",
                        ProviderJsonValue::decode(
                            br#"{"provider":"openai","model":"model-test","output":[{"type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"}]}"#,
                        )?,
                    )?),
                ],
            )?,
            ProviderMessage::new(
                ProviderMessageRole::Tool,
                vec![ProviderMessageContent::tool_result(
                    "call-1",
                    "lookup",
                    ProviderJsonValue::from("done"),
                )?],
            )?,
            ProviderMessage::text(ProviderMessageRole::User, "continue")?,
        ],
        vec![ProviderToolDefinition::new("lookup", "Look up a value", schema(), true)?],
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )?;
    let lease = active_lease(BuiltInProviderId::open_ai(), None)?;
    let wire = OpenAiResponsesAdapter::new(OpenAiResponsesKind::OpenAi)?
        .make_execution_request(&request, &lease)
        .await?;
    let body = ProviderJsonValue::decode(wire.body())?;
    assert_eq!(
        body.get("input")
            .and_then(ProviderJsonValue::as_array)
            .and_then(|items| items.get(1))
            .and_then(|item| item.get("type"))
            .and_then(ProviderJsonValue::as_str),
        Some("function_call")
    );
    Ok(())
}

#[tokio::test]
async fn account_integration_headers_reach_the_provider_wire() -> Result<(), Box<dyn Error>> {
    let request = request_for(BuiltInProviderId::open_router())?;
    let endpoint = ProviderEndpointConfiguration::parse_with_headers(
        "https://openrouter.example/v1",
        BTreeMap::from([
            ("http-referer".to_owned(), "https://arc.example".to_owned()),
            ("x-title".to_owned(), "Arc".to_owned()),
        ]),
    )?;
    let lease = active_lease(BuiltInProviderId::open_router(), Some(endpoint))?;
    let wire = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?
        .adapter(request.selection().provider_id())?
        .make_execution_request(&request, &lease)
        .await?;
    assert_eq!(
        wire.headers().get("http-referer").map(String::as_str),
        Some("https://arc.example")
    );
    assert_eq!(
        wire.headers().get("x-title").map(String::as_str),
        Some("Arc")
    );
    Ok(())
}

#[test]
fn sse_decoder_is_incremental_multiline_and_bounded() -> Result<(), Box<dyn Error>> {
    let mut decoder = ServerSentEventDecoder::new(64, 256)?;
    assert!(decoder.push(b"\xef").is_ok());
    assert!(decoder.push(b"\xbb\xbfevent: delta\r").is_ok());
    let events = decoder.push(b"\ndata: one\r\ndata: two\r\n\r\n")?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("delta"));
    assert_eq!(events[0].data, "one\ntwo");

    let mut empty_data = ServerSentEventDecoder::new(64, 256)?;
    let events = empty_data.push(b"data:\n\n")?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "");

    let mut bounded = ServerSentEventDecoder::new(4, 8)?;
    let failure = bounded
        .push(b"data: oversized\n\n")
        .err()
        .ok_or("oversized line was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ResponseTooLarge);
    assert_eq!(bounded.scanned_byte_count(), 5);

    let large_unused_id = "x".repeat(32_768);
    let mut ignores_unused_reconnect_fields = ServerSentEventDecoder::new(64 * 1_024, 64 * 1_024)?;
    let input = format!("id: {large_unused_id}\nretry: 1000\ndata: payload\n\n");
    let events = ignores_unused_reconnect_fields.push(input.as_bytes())?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "payload");
    Ok(())
}

#[tokio::test]
async fn test_credential_store_has_a_complete_ephemeral_lifecycle() -> Result<(), Box<dyn Error>> {
    let vault = InMemoryProviderCredentialStore::default();
    let account_id = ProviderAccountId::new("ephemeral-account")?;
    let request = ProviderAccountRegistrationRequest::new(
        account_id.clone(),
        BuiltInProviderId::open_ai(),
        "Ephemeral OpenAI",
        ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?,
        None,
    )?;
    let staged = vault
        .stage(&request, ProviderInstant::from_unix_milliseconds(1))
        .await?;
    assert_eq!(staged.state(), ProviderCredentialRecordState::Staged);
    assert_eq!(vault.record(&account_id).await?, Some(staged.clone()));

    let duplicate = vault
        .stage(&request, ProviderInstant::from_unix_milliseconds(2))
        .await
        .err()
        .ok_or("duplicate account registration was accepted")?;
    assert_eq!(duplicate.code(), ProviderFailureCode::InvalidRequest);

    vault
        .activate(&staged, ProviderInstant::from_unix_milliseconds(2))
        .await?;
    let lease = vault.lease(&account_id).await?;
    assert_eq!(
        lease.record().state(),
        ProviderCredentialRecordState::Active
    );
    assert_eq!(lease.record().reference(), staged.reference());
    assert_eq!(lease.material(), request.credential());

    vault.remove(&staged).await?;
    assert!(vault.records().await?.is_empty());
    let restaged = vault
        .stage(&request, ProviderInstant::from_unix_milliseconds(3))
        .await?;
    assert_ne!(restaged.reference(), staged.reference());
    vault.remove(&restaged).await?;
    Ok(())
}

#[tokio::test]
async fn reconciliation_fences_new_registration_admission() -> Result<(), Box<dyn Error>> {
    let (vault, records_entered, records_release) =
        InMemoryProviderCredentialStore::with_records_gate();
    let vault = Arc::new(vault);
    let runtime = ProviderRuntime::with_components(
        vault.clone(),
        Arc::new(CapturingUnaryTransport::default()),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let reconcile_runtime = runtime.clone();
    let reconciliation =
        tokio::spawn(async move { reconcile_runtime.reconcile_credentials().await });
    records_entered.wait().await;

    let account_id = ProviderAccountId::new("reconciliation-race-account")?;
    let registration = ProviderAccountRegistrationRequest::new(
        account_id.clone(),
        BuiltInProviderId::open_ai(),
        "Primary",
        ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?,
        None,
    )?;
    let mut events = runtime.register(registration).await;
    let Some(ProviderAccountPublicEvent::Failed(failure)) = events.next().await else {
        return Err("registration was not rejected during reconciliation".into());
    };
    assert_eq!(failure.code(), ProviderFailureCode::AccountUnavailable);
    assert!(events.next().await.is_none());
    assert!(vault.record(&account_id).await?.is_none());

    records_release.wait().await;
    let report = reconciliation.await??;
    assert_eq!(report.active_record_count(), 0);
    assert!(report.removed_staged_references().is_empty());
    runtime.shutdown().await;
    Ok(())
}

#[derive(Clone)]
struct BlockingAuthorizationSession {
    entered: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl ProviderAuthorizationSession for BlockingAuthorizationSession {
    async fn authorize(
        &self,
        _request: ProviderAuthorizationRequest,
    ) -> Result<ProviderAuthorizationResult, ProviderFailure> {
        self.entered.wait().await;
        self.release.wait().await;
        Err(ProviderFailure::new(
            ProviderFailureCode::Cancelled,
            "test authorization cancelled",
        ))
    }

    fn cancel(&self) {}
}

#[derive(Clone)]
struct CancellableAuthorizationSession {
    entered: Arc<tokio::sync::Barrier>,
    cancelled: Arc<tokio::sync::Notify>,
    cancel_called: Arc<AtomicBool>,
}

#[async_trait]
impl ProviderAuthorizationSession for CancellableAuthorizationSession {
    async fn authorize(
        &self,
        _request: ProviderAuthorizationRequest,
    ) -> Result<ProviderAuthorizationResult, ProviderFailure> {
        self.entered.wait().await;
        self.cancelled.notified().await;
        Err(ProviderFailure::new(
            ProviderFailureCode::Cancelled,
            "test authorization cancelled",
        ))
    }

    fn cancel(&self) {
        self.cancel_called.store(true, Ordering::SeqCst);
        self.cancelled.notify_waiters();
    }
}

#[tokio::test]
async fn in_flight_oauth_registration_fences_reconciliation() -> Result<(), Box<dyn Error>> {
    let runtime = ProviderRuntime::with_components(
        Arc::new(InMemoryProviderCredentialStore::default()),
        Arc::new(CapturingUnaryTransport::default()),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let pkce = ProviderPkce::new(
        SensitiveValue::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")?,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        "abcdefghijklmnopqrstuvwxyzABCDEF",
    )?;
    let request = OpenRouterOAuthRegistrationRequest::new(
        ProviderAccountId::new("oauth-reconciliation-account")?,
        "OpenRouter",
        Url::parse("http://127.0.0.1:49152/oauth/openrouter")?,
        pkce,
    )?;
    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    let authorization = Arc::new(BlockingAuthorizationSession {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let oauth_runtime = runtime.clone();
    let oauth = tokio::spawn(async move {
        oauth_runtime
            .register_open_router_oauth(request, authorization.as_ref())
            .await
    });
    entered.wait().await;

    let failure = runtime
        .reconcile_credentials()
        .await
        .err()
        .ok_or("reconciliation was admitted during OAuth registration")?;
    assert_eq!(failure.code(), ProviderFailureCode::InvalidRequest);

    release.wait().await;
    let oauth_failure = oauth
        .await?
        .err()
        .ok_or("cancelled test authorization unexpectedly succeeded")?;
    assert_eq!(oauth_failure.code(), ProviderFailureCode::Cancelled);
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancel_registration_interrupts_in_flight_oauth_authorization() -> Result<(), Box<dyn Error>>
{
    let runtime = ProviderRuntime::with_components(
        Arc::new(InMemoryProviderCredentialStore::default()),
        Arc::new(CapturingUnaryTransport::default()),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let account_id = ProviderAccountId::new("oauth-cancel-account")?;
    let pkce = ProviderPkce::new(
        SensitiveValue::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")?,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        "abcdefghijklmnopqrstuvwxyzABCDEF",
    )?;
    let request = OpenRouterOAuthRegistrationRequest::new(
        account_id.clone(),
        "OpenRouter",
        Url::parse("http://127.0.0.1:49152/oauth/openrouter")?,
        pkce,
    )?;
    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let authorization = Arc::new(CancellableAuthorizationSession {
        entered: Arc::clone(&entered),
        cancelled: Arc::new(tokio::sync::Notify::new()),
        cancel_called: Arc::new(AtomicBool::new(false)),
    });
    let cancel_called = Arc::clone(&authorization.cancel_called);
    let oauth_runtime = runtime.clone();
    let oauth = tokio::spawn(async move {
        oauth_runtime
            .register_open_router_oauth(request, authorization.as_ref())
            .await
    });
    entered.wait().await;

    tokio::time::timeout(
        Duration::from_millis(250),
        runtime.cancel_registration(&account_id),
    )
    .await
    .map_err(|_| "OAuth cancellation did not complete")?;
    let oauth_failure = oauth
        .await?
        .err()
        .ok_or("cancelled OAuth authorization unexpectedly succeeded")?;
    assert_eq!(oauth_failure.code(), ProviderFailureCode::Cancelled);
    assert!(cancel_called.load(Ordering::SeqCst));
    runtime.shutdown().await;
    Ok(())
}

#[derive(Clone, Default)]
struct AccountScopedHangingTransport {
    cancelled_hosts: Arc<ParkingMutex<HashSet<String>>>,
}

struct AccountScopedTransportControl {
    host: String,
    cancelled_hosts: Arc<ParkingMutex<HashSet<String>>>,
}

#[async_trait]
impl ProviderTransportControl for AccountScopedTransportControl {
    fn cancel(&self) {
        self.cancelled_hosts.lock().insert(self.host.clone());
    }

    async fn wait_for_termination(&self) {}
}

#[async_trait]
impl ProviderHttpTransport for AccountScopedHangingTransport {
    async fn open(
        &self,
        request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError> {
        let host = request
            .url()
            .host_str()
            .ok_or(ProviderTransportError::InvalidResponse)?
            .to_owned();
        ProviderHttpResponse::new(
            200,
            BTreeMap::new(),
            Box::pin(stream::pending::<Result<Bytes, ProviderTransportError>>()),
            ProviderHttpResponseControl::new(Arc::new(AccountScopedTransportControl {
                host,
                cancelled_hosts: Arc::clone(&self.cancelled_hosts),
            })),
        )
    }
}

impl AccountScopedHangingTransport {
    fn was_cancelled(&self, host: &str) -> bool {
        self.cancelled_hosts.lock().contains(host)
    }
}

#[tokio::test]
async fn revoke_cancels_only_executions_for_the_selected_account() -> Result<(), Box<dyn Error>> {
    let vault = Arc::new(InMemoryProviderCredentialStore::default());
    let open_router_account = ProviderAccountId::new("openrouter-account")?;
    let open_ai_account = ProviderAccountId::new("openai-account")?;
    for (account_id, provider_id) in [
        (
            open_router_account.clone(),
            BuiltInProviderId::open_router(),
        ),
        (open_ai_account.clone(), BuiltInProviderId::open_ai()),
    ] {
        let request = ProviderAccountRegistrationRequest::new(
            account_id,
            provider_id,
            "Primary",
            ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?,
            None,
        )?;
        let staged = vault
            .stage(&request, ProviderInstant::from_unix_milliseconds(1))
            .await?;
        vault
            .activate(&staged, ProviderInstant::from_unix_milliseconds(2))
            .await?;
    }
    let transport = Arc::new(AccountScopedHangingTransport::default());
    let runtime = ProviderRuntime::with_components(
        vault.clone(),
        transport.clone(),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let open_router_request = request_for_account(
        BuiltInProviderId::open_router(),
        open_router_account.clone(),
        "openrouter-request",
    )?;
    let open_ai_request = request_for_account(
        BuiltInProviderId::open_ai(),
        open_ai_account.clone(),
        "openai-request",
    )?;

    let mut open_router = runtime.execute(open_router_request).await;
    let mut open_ai = runtime.execute(open_ai_request.clone()).await;
    assert!(matches!(
        open_router.next().await,
        Some(ProviderTurnEvent::Started(_))
    ));
    assert!(matches!(
        open_ai.next().await,
        Some(ProviderTurnEvent::Started(_))
    ));

    runtime.revoke(&open_router_account).await?;
    assert!(transport.was_cancelled("openrouter.ai"));
    assert!(!transport.was_cancelled("api.openai.com"));
    assert!(vault.record(&open_router_account).await?.is_none());
    assert!(vault.record(&open_ai_account).await?.is_some());

    runtime.cancel(open_ai_request.id()).await;
    assert!(transport.was_cancelled("api.openai.com"));
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn runtime_rejects_tool_calls_outside_declared_and_named_tool_scope()
-> Result<(), Box<dyn Error>> {
    let vault = Arc::new(InMemoryProviderCredentialStore::default());
    let account_id = ProviderAccountId::new("openrouter-account")?;
    let registration = ProviderAccountRegistrationRequest::new(
        account_id.clone(),
        BuiltInProviderId::open_router(),
        "Primary",
        ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?,
        None,
    )?;
    let staged = vault
        .stage(&registration, ProviderInstant::from_unix_milliseconds(1))
        .await?;
    vault
        .activate(&staged, ProviderInstant::from_unix_milliseconds(2))
        .await?;

    let transport = Arc::new(ScriptedChatToolTransport {
        tool_names: Arc::new(ParkingMutex::new(VecDeque::from([
            "unlisted".to_owned(),
            "archive".to_owned(),
        ]))),
    });
    let runtime = ProviderRuntime::with_components(
        vault,
        transport,
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let requests = [
        request_with_tools(
            BuiltInProviderId::open_router(),
            "tool-scope-unlisted",
            &["lookup"],
            ProviderToolChoice::named("lookup")?,
        )?,
        request_with_tools(
            BuiltInProviderId::open_router(),
            "tool-scope-not-selected",
            &["lookup", "archive"],
            ProviderToolChoice::named("lookup")?,
        )?,
    ];

    for request in requests {
        let mut events = runtime.execute(request).await;
        let mut saw_tool_call = false;
        let mut terminal = None;
        while let Some(event) = events.next().await {
            match event {
                ProviderTurnEvent::ToolCall(_) => saw_tool_call = true,
                ProviderTurnEvent::Terminal(value) => {
                    terminal = Some(value);
                    break;
                }
                ProviderTurnEvent::Started(_)
                | ProviderTurnEvent::ReasoningDelta(_)
                | ProviderTurnEvent::TextDelta(_) => {}
            }
        }
        assert!(!saw_tool_call, "out-of-scope tool call became public");
        assert!(matches!(
            terminal,
            Some(ProviderTerminal::Failed(failure))
                if failure.code() == ProviderFailureCode::CapabilityMismatch
        ));
    }

    runtime.shutdown().await;
    Ok(())
}

#[derive(Clone)]
struct BlockingModelTransport {
    entered: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl ProviderHttpTransport for BlockingModelTransport {
    async fn open(
        &self,
        _request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError> {
        Err(ProviderTransportError::Failed)
    }

    async fn send(
        &self,
        _request: ProviderHttpRequest,
    ) -> Result<ProviderHttpUnaryResponse, ProviderTransportError> {
        self.entered.wait().await;
        self.release.wait().await;
        Ok(ProviderHttpUnaryResponse {
            status_code: 200,
            headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            body: br#"{"data":[{"id":"model-test"}]}"#.to_vec(),
        })
    }
}

#[tokio::test]
async fn revoke_waits_for_in_flight_inspection_before_removing_credentials()
-> Result<(), Box<dyn Error>> {
    let vault = Arc::new(InMemoryProviderCredentialStore::default());
    let account_id = ProviderAccountId::new("inspected-account")?;
    let registration = ProviderAccountRegistrationRequest::new(
        account_id.clone(),
        BuiltInProviderId::open_router(),
        "Primary",
        ProviderCredentialMaterial::oauth_derived_key("oauth-secret")?,
        None,
    )?;
    let staged = vault
        .stage(&registration, ProviderInstant::from_unix_milliseconds(1))
        .await?;
    vault
        .activate(&staged, ProviderInstant::from_unix_milliseconds(2))
        .await?;

    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = ProviderRuntime::with_components(
        vault.clone(),
        Arc::new(BlockingModelTransport {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        Arc::new(SystemProviderClock),
        &ProviderRuntimeOptions::default(),
    )?;
    let inspect_runtime = runtime.clone();
    let inspected_account = account_id.clone();
    let inspection = tokio::spawn(async move { inspect_runtime.inspect(&inspected_account).await });
    entered.wait().await;

    let revoke_runtime = runtime.clone();
    let revoked_account = account_id.clone();
    let mut revocation = tokio::spawn(async move { revoke_runtime.revoke(&revoked_account).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut revocation)
            .await
            .is_err()
    );
    assert!(vault.record(&account_id).await?.is_some());

    release.wait().await;
    inspection.await??;
    revocation.await??;
    assert!(vault.record(&account_id).await?.is_none());
    assert!(runtime.accounts().await?.is_empty());
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn openrouter_request_preserves_policy_tool_choice_and_output_limit()
-> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let provider_id = BuiltInProviderId::open_router();
    let adapter = registry.adapter(&provider_id)?;
    let request = request_for(provider_id.clone())?;
    let lease = active_lease(provider_id, None)?;
    let wire = adapter.make_execution_request(&request, &lease).await?;
    assert_eq!(wire.method(), Method::POST);
    assert_eq!(
        wire.url().as_str(),
        "https://openrouter.ai/api/v1/chat/completions"
    );
    assert!(
        wire.headers()
            .get("authorization")
            .is_some_and(|value| value == "Bearer oauth-secret")
    );
    let body = ProviderJsonValue::decode(wire.body())?;
    assert_eq!(
        body.get("max_completion_tokens")
            .and_then(ProviderJsonValue::as_i64),
        Some(4_096)
    );
    assert_eq!(
        body.at(&["tool_choice", "function", "name"])
            .and_then(ProviderJsonValue::as_str),
        Some("lookup")
    );
    assert_eq!(
        body.at(&["provider", "allow_fallbacks"])
            .and_then(ProviderJsonValue::as_bool),
        Some(false)
    );
    assert_eq!(
        body.at(&["provider", "require_parameters"])
            .and_then(ProviderJsonValue::as_bool),
        Some(true)
    );
    assert_eq!(
        body.at(&["provider", "data_collection"])
            .and_then(ProviderJsonValue::as_str),
        Some("deny")
    );
    assert_eq!(
        body.at(&["provider", "zdr"])
            .and_then(ProviderJsonValue::as_bool),
        Some(true)
    );
    assert_eq!(
        body.at(&["reasoning", "effort"])
            .and_then(ProviderJsonValue::as_str),
        Some("high")
    );
    assert_eq!(
        body.at(&["response_format", "type"])
            .and_then(ProviderJsonValue::as_str),
        Some("json_schema")
    );
    Ok(())
}

#[tokio::test]
async fn openai_compatible_reasoning_state_replays_only_as_opaque_wire_history()
-> Result<(), Box<dyn Error>> {
    let provider_id = BuiltInProviderId::open_router();
    let account_id = ProviderAccountId::new("openrouter-account")?;
    let native = ProviderNativeState::new(
        "openai.chat.reasoning.v1",
        json_object([
            ("provider", ProviderJsonValue::from("openrouter")),
            ("model", ProviderJsonValue::from("model-test")),
            ("field", ProviderJsonValue::from("reasoning_content")),
            ("content", ProviderJsonValue::from("private-plan")),
        ]),
    )?;
    let request = ProviderTurnRequest::new(
        ProviderRequestId::new("reasoning-replay")?,
        ProviderSelection::new(
            provider_id.clone(),
            account_id,
            ProviderModelId::new("model-test")?,
        ),
        vec![
            ProviderMessage::new(
                ProviderMessageRole::Assistant,
                vec![
                    ProviderMessageContent::text("answer"),
                    ProviderMessageContent::native_state(native),
                ],
            )?,
            ProviderMessage::text(ProviderMessageRole::User, "continue")?,
        ],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        None,
        ProviderRequestConstraints::default(),
    )?;
    let adapter =
        BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?.adapter(&provider_id)?;
    let wire = adapter
        .make_execution_request(&request, &active_lease(provider_id, None)?)
        .await?;
    let body = ProviderJsonValue::decode(wire.body())?;
    let messages = body
        .get("messages")
        .and_then(ProviderJsonValue::as_array)
        .ok_or("chat request has no messages")?;
    assert_eq!(
        messages[0]
            .get("reasoning_content")
            .and_then(ProviderJsonValue::as_str),
        Some("private-plan")
    );
    Ok(())
}

#[tokio::test]
async fn native_state_is_rejected_outside_its_exact_provider_model_route()
-> Result<(), Box<dyn Error>> {
    let cases = [
        (
            BuiltInProviderId::open_ai(),
            "openai.responses.output.v1",
            r#"{"provider":"openai","model":"other-model","output":[]}"#,
        ),
        (
            BuiltInProviderId::open_router(),
            "openai.chat.reasoning.v1",
            r#"{"provider":"openrouter","model":"other-model","field":"reasoning_content","content":"private"}"#,
        ),
        (
            BuiltInProviderId::gemini(),
            "google.generate-content.parts.v1",
            r#"{"provider":"gemini","model":"other-model","parts":[]}"#,
        ),
        (
            BuiltInProviderId::anthropic(),
            "openai.chat.reasoning.v1",
            r#"{"provider":"openrouter","model":"model-test","field":"reasoning_content","content":"private"}"#,
        ),
    ];
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    for (index, (provider_id, format, payload)) in cases.into_iter().enumerate() {
        let request = ProviderTurnRequest::new(
            ProviderRequestId::new(format!("native-route-rejection-{index}"))?,
            ProviderSelection::new(
                provider_id.clone(),
                ProviderAccountId::new(format!("native-route-account-{index}"))?,
                ProviderModelId::new("model-test")?,
            ),
            vec![
                ProviderMessage::new(
                    ProviderMessageRole::Assistant,
                    vec![ProviderMessageContent::native_state(
                        ProviderNativeState::new(
                            format,
                            ProviderJsonValue::decode(payload.as_bytes())?,
                        )?,
                    )],
                )?,
                ProviderMessage::text(ProviderMessageRole::User, "continue")?,
            ],
            Vec::new(),
            ProviderToolChoice::Automatic,
            ProviderOutputRequirement::Text,
            ProviderReasoningPolicy::Automatic,
            None,
            ProviderRequestConstraints::default(),
        )?;
        let failure = registry
            .adapter(&provider_id)?
            .make_execution_request(&request, &active_lease(provider_id, None)?)
            .await
            .err()
            .ok_or("cross-route native state was accepted")?;
        assert_eq!(failure.code(), ProviderFailureCode::CapabilityMismatch);
    }
    Ok(())
}

#[tokio::test]
async fn application_validated_chat_instruction_precedes_user_messages()
-> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let provider_id = BuiltInProviderId::deep_seek();
    let adapter = registry.adapter(&provider_id)?;
    let request = request_for_reasoning_and_tool_choice(
        provider_id.clone(),
        ProviderReasoningPolicy::Automatic,
        ProviderToolChoice::Automatic,
    )?;
    let lease = active_lease(provider_id, None)?;
    let wire = adapter.make_execution_request(&request, &lease).await?;
    let body = ProviderJsonValue::decode(wire.body())?;
    let messages = body
        .get("messages")
        .and_then(ProviderJsonValue::as_array)
        .ok_or("chat request has no messages")?;
    assert!(messages.len() >= 2);
    assert_eq!(
        messages[0].get("role").and_then(ProviderJsonValue::as_str),
        Some("system"),
    );
    assert!(
        messages[0]
            .get("content")
            .and_then(ProviderJsonValue::as_str)
            .is_some_and(|value| value.contains("application schema"))
    );
    assert_eq!(
        messages[1].get("role").and_then(ProviderJsonValue::as_str),
        Some("user"),
    );
    Ok(())
}

#[tokio::test]
async fn qwen_uses_portal_by_default_and_allows_an_explicit_regional_endpoint()
-> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let provider_id = BuiltInProviderId::qwen();
    let adapter = registry.adapter(&provider_id)?;
    let request = request_for_reasoning(provider_id.clone(), ProviderReasoningPolicy::Automatic)?;
    let lease = active_lease(provider_id.clone(), None)?;
    let wire = adapter.make_execution_request(&request, &lease).await?;
    assert_eq!(
        wire.url().as_str(),
        "https://portal.qwen.ai/v1/chat/completions"
    );

    let endpoint =
        ProviderEndpointConfiguration::parse("https://dashscope.aliyuncs.com/compatible-mode/v1")?;
    let lease = active_lease(provider_id, Some(endpoint))?;
    let wire = adapter.make_execution_request(&request, &lease).await?;
    assert_eq!(
        wire.url().as_str(),
        "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
    );
    Ok(())
}

#[tokio::test]
async fn gemini_uses_generate_content_wire_contract() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let provider_id = BuiltInProviderId::gemini();
    let adapter = registry.adapter(&provider_id)?;
    let request = request_for(provider_id.clone())?;
    let wire = adapter
        .make_execution_request(&request, &active_lease(provider_id, None)?)
        .await?;
    assert_eq!(
        wire.url().as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/model-test:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        wire.headers().get("x-goog-api-key").map(String::as_str),
        Some("oauth-secret")
    );
    let body = ProviderJsonValue::decode(wire.body())?;
    assert!(body.get("input").is_none());
    let contents = body
        .get("contents")
        .and_then(ProviderJsonValue::as_array)
        .ok_or("Gemini request has no contents")?;
    assert_eq!(
        contents[0].get("role").and_then(ProviderJsonValue::as_str),
        Some("user")
    );
    assert_eq!(
        body.at(&["toolConfig", "functionCallingConfig", "mode"])
            .and_then(ProviderJsonValue::as_str),
        Some("ANY")
    );
    assert_eq!(
        body.at(&["generationConfig", "thinkingConfig", "thinkingLevel"])
            .and_then(ProviderJsonValue::as_str),
        Some("high")
    );
    Ok(())
}

#[tokio::test]
async fn deepseek_kimi_and_zai_use_gajae_aligned_dialects() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;

    let deepseek_id = BuiltInProviderId::deep_seek();
    let deepseek_request = request_for_reasoning_and_tool_choice(
        deepseek_id.clone(),
        ProviderReasoningPolicy::Effort(ProviderReasoningEffort::Low),
        ProviderToolChoice::Automatic,
    )?;
    let deepseek = registry
        .adapter(&deepseek_id)?
        .make_execution_request(&deepseek_request, &active_lease(deepseek_id, None)?)
        .await?;
    let deepseek_body = ProviderJsonValue::decode(deepseek.body())?;
    assert_eq!(
        deepseek_body
            .get("max_tokens")
            .and_then(ProviderJsonValue::as_i64),
        Some(4_096)
    );
    assert!(deepseek_body.get("tool_choice").is_none());
    assert_eq!(
        deepseek_body
            .get("reasoning_effort")
            .and_then(ProviderJsonValue::as_str),
        Some("high")
    );

    let kimi_id = BuiltInProviderId::kimi();
    let kimi = registry
        .adapter(&kimi_id)?
        .make_execution_request(
            &request_for(kimi_id.clone())?,
            &active_lease(kimi_id, None)?,
        )
        .await?;
    assert_eq!(
        kimi.url().as_str(),
        "https://api.kimi.com/coding/v1/chat/completions"
    );
    assert_eq!(
        kimi.headers().get("user-agent").map(String::as_str),
        Some("KimiCLI/1.0")
    );
    let kimi_body = ProviderJsonValue::decode(kimi.body())?;
    assert_eq!(
        kimi_body
            .get("max_completion_tokens")
            .and_then(ProviderJsonValue::as_i64),
        Some(4_096)
    );
    assert_eq!(
        kimi_body
            .at(&["thinking", "type"])
            .and_then(ProviderJsonValue::as_str),
        Some("disabled")
    );

    let zai_id = BuiltInProviderId::zai();
    let zai = registry
        .adapter(&zai_id)?
        .make_execution_request(
            &request_for_reasoning(zai_id.clone(), ProviderReasoningPolicy::Automatic)?,
            &active_lease(zai_id, None)?,
        )
        .await?;
    assert_eq!(
        zai.url().as_str(),
        "https://api.z.ai/api/anthropic/v1/messages"
    );
    assert_eq!(
        zai.headers().get("authorization").map(String::as_str),
        Some("Bearer oauth-secret")
    );
    Ok(())
}

#[tokio::test]
async fn minimax_uses_distinct_execution_and_model_catalog_bases() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let provider_id = BuiltInProviderId::mini_max();
    let adapter = registry.adapter(&provider_id)?;
    let request = request_for_reasoning(provider_id.clone(), ProviderReasoningPolicy::Automatic)?;
    let lease = active_lease(provider_id, None)?;

    let execution = adapter.make_execution_request(&request, &lease).await?;
    assert_eq!(
        execution.url().as_str(),
        "https://api.minimax.io/anthropic/v1/messages",
    );
    assert_eq!(execution.timeout_milliseconds(), 30_000);
    assert_eq!(
        execution.headers().get("accept").map(String::as_str),
        Some("text/event-stream"),
    );

    let transport = CapturingUnaryTransport::default();
    let catalog = adapter
        .models(
            &lease,
            &transport,
            ProviderInstant::from_unix_milliseconds(100),
        )
        .await?;
    assert_eq!(catalog.models().len(), 1);
    let models = transport.captured_request()?;
    assert_eq!(models.method(), Method::GET);
    assert_eq!(models.url().as_str(), "https://api.minimax.io/v1/models");
    assert_eq!(models.timeout_milliseconds(), 60_000);
    assert_eq!(
        models.headers().get("accept").map(String::as_str),
        Some("application/json"),
    );
    assert!(!models.headers().contains_key("anthropic-version"));
    Ok(())
}

#[test]
fn openai_chat_decoder_normalizes_text_usage_and_completion() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_router())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let first = decoder.consume(&event(
        None,
        r#"{"id":"response-1","choices":[{"index":0,"delta":{"content":"hello","reasoning_content":"plan"},"finish_reason":null}]}"#,
    ))?;
    assert!(matches!(
        first.as_slice(),
        [ProviderDecodedEvent::Text(text), ProviderDecodedEvent::Reasoning(reasoning)]
            if text == "hello" && reasoning == "plan"
    ));
    let usage_events = decoder.consume(&event(
        None,
        r#"{"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#,
    ))?;
    assert!(usage_events.is_empty());
    let final_marker = decoder.consume(&event(
        None,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ))?;
    assert!(final_marker.is_empty());
    let completed = decoder.consume(&event(None, "[DONE]"))?;
    let [ProviderDecodedEvent::Completion(completion)] = completed.as_slice() else {
        return Err("chat stream did not produce one completion".into());
    };
    assert!(completion.native_state.is_some());
    Ok(())
}

#[test]
fn openai_chat_decoder_accumulates_fragmented_tool_names_before_scope_check()
-> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_router())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    for payload in [
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"look","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"up","arguments":"{}"}}]},"finish_reason":null}]}"#,
    ] {
        assert!(decoder.consume(&event(None, payload))?.is_empty());
    }
    assert!(
        decoder
            .consume(&event(
                None,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ))?
            .is_empty()
    );
    let completed = decoder.consume(&event(None, "[DONE]"))?;
    assert!(matches!(
        completed.as_slice(),
        [ProviderDecodedEvent::ToolCall(call), ProviderDecodedEvent::Completion(_)]
            if call.name() == "lookup"
    ));
    Ok(())
}

#[test]
fn openai_chat_decoder_fails_closed_on_malformed_optional_fields() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_router())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let failure = decoder
        .consume(&event(
            None,
            r#"{"choices":[{"index":0,"delta":{"content":42},"finish_reason":null}]}"#,
        ))
        .err()
        .ok_or("malformed optional content was ignored")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn openai_responses_decoder_emits_text_tool_and_completion() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_ai())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    assert!(
        decoder
            .consume(&event(
                Some("response.created"),
                r#"{"type":"response.created","response":{"id":"resp-1"}}"#
            ))?
            .is_empty()
    );
    let text = decoder.consume(&event(
        Some("response.output_text.delta"),
        r#"{"type":"response.output_text.delta","delta":"hello"}"#,
    ))?;
    assert!(matches!(text.as_slice(), [ProviderDecodedEvent::Text(value)] if value == "hello"));
    let reasoning = decoder.consume(&event(
        Some("response.reasoning_summary_text.delta"),
        r#"{"type":"response.reasoning_summary_text.delta","delta":"plan"}"#,
    ))?;
    assert!(
        matches!(reasoning.as_slice(), [ProviderDecodedEvent::Reasoning(value)] if value == "plan")
    );
    let private_reasoning = decoder.consume(&event(
        Some("response.reasoning_text.delta"),
        r#"{"type":"response.reasoning_text.delta","delta":"private"}"#,
    ))?;
    assert!(private_reasoning.is_empty());
    assert!(decoder.consume(&event(Some("response.output_item.added"), r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"call-1","call_id":"call-1","name":"lookup"}}"#))?.is_empty());
    let tool = decoder.consume(&event(Some("response.function_call_arguments.done"), r#"{"type":"response.function_call_arguments.done","item_id":"call-1","output_index":0,"arguments":"{\"answer\":\"ok\"}"}"#))?;
    assert!(
        matches!(tool.as_slice(), [ProviderDecodedEvent::ToolCall(call)] if call.name() == "lookup")
    );
    let completed = decoder.consume(&event(Some("response.completed"), r#"{"type":"response.completed","response":{"id":"resp-1","status":"completed","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}"#))?;
    assert!(matches!(
        completed.as_slice(),
        [ProviderDecodedEvent::Completion(_)]
    ));
    Ok(())
}

#[test]
fn openai_responses_decoder_rejects_missing_delta_fields() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_ai())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    for event_type in [
        "response.output_text.delta",
        "response.reasoning_summary_text.delta",
        "response.function_call_arguments.delta",
    ] {
        let failure = decoder
            .consume(&event(
                Some(event_type),
                &format!(r#"{{"type":"{event_type}"}}"#),
            ))
            .err()
            .ok_or("missing Responses delta field was ignored")?;
        assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    }
    Ok(())
}

#[test]
fn openai_responses_decoder_rejects_item_id_as_a_tool_call_id() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_ai())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    decoder.consume(&event(
        Some("response.output_item.added"),
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","name":"lookup"}}"#,
    ))?;
    let failure = decoder
        .consume(&event(
            Some("response.function_call_arguments.done"),
            r#"{"type":"response.function_call_arguments.done","item_id":"item-1","arguments":"{}"}"#,
        ))
        .err()
        .ok_or("Responses accepted an item ID as a tool call ID")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn openai_responses_decoder_bounds_tool_call_state() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_ai())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    for index in 0..ProviderTurnRequest::MAXIMUM_TOOLS {
        let payload = format!(
            r#"{{"type":"response.output_item.added","item":{{"type":"function_call","id":"item-{index}","call_id":"call-{index}","name":"lookup"}}}}"#
        );
        assert!(
            decoder
                .consume(&event(Some("response.output_item.added"), &payload))?
                .is_empty()
        );
    }
    let failure = decoder
        .consume(&event(
            Some("response.output_item.added"),
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item-overflow","call_id":"call-overflow","name":"lookup"}}"#,
        ))
        .err()
        .ok_or("Responses accepted an unbounded number of tool states")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn gemini_decoder_bounds_function_calls() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::gemini())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let mut payload = String::from(r#"{"candidates":[{"content":{"role":"model","parts":["#);
    for index in 0..ProviderTurnRequest::MAXIMUM_TOOLS {
        if index > 0 {
            payload.push(',');
        }
        payload.push_str(r#"{"functionCall":{"name":"lookup","args":{}}}"#);
    }
    payload.push_str(r#"]}}]}"#);
    let emitted = decoder.consume(&event(None, &payload))?;
    assert_eq!(emitted.len(), ProviderTurnRequest::MAXIMUM_TOOLS);
    assert!(
        emitted
            .iter()
            .all(|event| matches!(event, ProviderDecodedEvent::ToolCall(_)))
    );

    let failure = decoder
        .consume(&event(
            None,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{}}}]}}]}"#,
        ))
        .err()
        .ok_or("Gemini accepted an unbounded number of function calls")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn anthropic_decoder_bounds_sequential_tool_blocks() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::anthropic())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    assert!(
        decoder
            .consume(&event(
                None,
                r#"{"type":"message_start","message":{"id":"bound","usage":{"input_tokens":1}}}"#,
            ))?
            .is_empty()
    );

    for index in 0..ProviderTurnRequest::MAXIMUM_TOOLS {
        let start = format!(
            r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"call-{index}","name":"lookup","input":{{}}}}}}"#
        );
        assert!(decoder.consume(&event(None, &start))?.is_empty());
        let stop = format!(r#"{{"type":"content_block_stop","index":{index}}}"#);
        let emitted = decoder.consume(&event(None, &stop))?;
        assert!(matches!(
            emitted.as_slice(),
            [ProviderDecodedEvent::ToolCall(_)]
        ));
    }

    let start = format!(
        r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"tool_use","id":"call-overflow","name":"lookup","input":{{}}}}}}"#,
        ProviderTurnRequest::MAXIMUM_TOOLS
    );
    let failure = decoder
        .consume(&event(None, &start))
        .err()
        .ok_or("Anthropic accepted more than the tool-call cap")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn openai_chat_decoder_bounds_distinct_tool_indices() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_router())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let mut calls = String::new();
    for index in 0..ProviderTurnRequest::MAXIMUM_TOOLS {
        if index > 0 {
            calls.push(',');
        }
        calls.push_str(&format!(
            r#"{{"index":{index},"id":"call-{index}","function":{{"name":"lookup","arguments":"{{}}"}}}}"#
        ));
    }
    let payload = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{calls}]}},"finish_reason":null}}]}}"#
    );
    assert!(decoder.consume(&event(None, &payload))?.is_empty());
    assert!(
        decoder
            .consume(&event(
                None,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ))?
            .is_empty()
    );
    let completed = decoder.consume(&event(None, "[DONE]"))?;
    assert_eq!(
        completed
            .iter()
            .filter(|event| matches!(event, ProviderDecodedEvent::ToolCall(_)))
            .count(),
        ProviderTurnRequest::MAXIMUM_TOOLS
    );
    assert!(
        completed
            .iter()
            .take(ProviderTurnRequest::MAXIMUM_TOOLS)
            .all(|event| matches!(event, ProviderDecodedEvent::ToolCall(_)))
    );
    assert!(matches!(
        completed.last(),
        Some(ProviderDecodedEvent::Completion(_))
    ));

    let mut overflow_decoder = adapter.make_decoder(&request)?;
    assert!(overflow_decoder.consume(&event(None, &payload))?.is_empty());
    let failure = overflow_decoder
        .consume(&event(
            None,
            &format!(
                r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{},"id":"call-overflow","function":{{"name":"lookup","arguments":"{{}}"}}}}]}},"finish_reason":null}}]}}"#,
                ProviderTurnRequest::MAXIMUM_TOOLS
            ),
        ))
        .err()
        .ok_or("OpenAI chat accepted more than the distinct tool-index cap")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn anthropic_decoder_binds_delta_to_opened_content_block_type() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::anthropic())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    assert!(
        decoder
            .consume(&event(
                Some("message_start"),
                r#"{"type":"message_start","message":{"id":"msg-1","usage":{"input_tokens":1}}}"#
            ))?
            .is_empty()
    );
    assert!(decoder.consume(&event(Some("content_block_start"), r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#))?.is_empty());
    let text = decoder.consume(&event(
        Some("content_block_delta"),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
    ))?;
    assert!(matches!(text.as_slice(), [ProviderDecodedEvent::Text(value)] if value == "hello"));
    let failure = decoder
        .consume(&event(Some("content_block_delta"), r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#))
        .err()
        .ok_or("Anthropic accepted a tool delta for a text block")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn gemini_decoder_normalizes_generate_content_text_usage_and_completion()
-> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::gemini())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let text = decoder.consume(&event(
        None,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello"}]}}],"usageMetadata":{"promptTokenCount":3,"cachedContentTokenCount":1,"candidatesTokenCount":1,"thoughtsTokenCount":1,"totalTokenCount":5}}"#,
    ))?;
    assert!(matches!(text.as_slice(), [ProviderDecodedEvent::Text(value)] if value == "hello"));
    let completion =
        decoder.consume(&event(None, r#"{"candidates":[{"finishReason":"STOP"}]}"#))?;
    let [ProviderDecodedEvent::Completion(completion)] = completion.as_slice() else {
        return Err("Gemini did not emit one completion".into());
    };
    assert_eq!(
        completion
            .usage
            .as_ref()
            .and_then(ProviderUsage::total_tokens),
        Some(5)
    );
    assert!(completion.native_state.is_some());
    Ok(())
}

#[test]
fn openai_chat_usage_only_terminal_chunk_preserves_usage() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_router())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    assert!(
        decoder
            .consume(&event(
                None,
                r#"{"id":"usage-only","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ))?
            .is_empty()
    );
    assert!(decoder.consume(&event(
        None,
        r#"{"id":"usage-only","usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#,
    ))?.is_empty());
    let completed = decoder.consume(&event(None, "[DONE]"))?;
    let [ProviderDecodedEvent::Completion(completion)] = completed.as_slice() else {
        return Err("usage-only chunk did not produce one completion".into());
    };
    assert_eq!(
        completion
            .usage
            .as_ref()
            .and_then(ProviderUsage::total_tokens),
        Some(5),
    );
    Ok(())
}

#[test]
fn retry_after_date_and_usage_overflow_are_deterministic() -> Result<(), Box<dyn Error>> {
    let failure = http_failure_parts(
        429,
        &BTreeMap::from([(
            "retry-after".to_owned(),
            "Thu, 01 Jan 1970 00:00:02 GMT".to_owned(),
        )]),
        &[],
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(1_000));

    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::anthropic())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    assert!(decoder.consume(&event(
        Some("message_start"),
        r#"{"type":"message_start","message":{"id":"overflow","usage":{"input_tokens":4611686018427387904}}}"#,
    ))?.is_empty());
    assert!(decoder.consume(&event(
        Some("message_delta"),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4611686018427387904}}"#,
    ))?.is_empty());
    let overflow = decoder
        .consume(&event(Some("message_stop"), r#"{"type":"message_stop"}"#))
        .err()
        .ok_or("token usage overflow was accepted")?;
    assert_eq!(overflow.code(), ProviderFailureCode::MalformedResponse);
    Ok(())
}

#[test]
fn retry_after_millisecond_header_and_fractional_seconds_are_bounded() {
    let headers = BTreeMap::from([
        ("retry-after-ms".to_owned(), "1500".to_owned()),
        ("retry-after".to_owned(), "9".to_owned()),
    ]);
    let failure = http_failure_parts(
        429,
        &headers,
        b"{}",
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(1_500));

    let headers = BTreeMap::from([("retry-after".to_owned(), "10.632".to_owned())]);
    let failure = http_failure_parts(
        429,
        &headers,
        b"{}",
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(10_632));
}

#[test]
fn openai_reset_headers_fill_missing_retry_after_without_overriding_it() {
    let headers = BTreeMap::from([
        ("x-ratelimit-remaining-requests".to_owned(), "0".to_owned()),
        ("x-ratelimit-reset-requests".to_owned(), "8.64s".to_owned()),
        ("x-ratelimit-remaining-tokens".to_owned(), "10".to_owned()),
        ("x-ratelimit-reset-tokens".to_owned(), "131ms".to_owned()),
    ]);
    let failure = http_failure_parts(
        429,
        &headers,
        br#"{"error":{"type":"rate_limit_error"}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(8_640));

    let project = BTreeMap::from([
        (
            "x-ratelimit-remaining-project-tokens".to_owned(),
            "0".to_owned(),
        ),
        (
            "x-ratelimit-reset-project-tokens".to_owned(),
            "45s".to_owned(),
        ),
    ]);
    let failure = http_failure_parts(
        429,
        &project,
        br#"{"error":{"type":"rate_limit_error"}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(45_000));

    let explicit = BTreeMap::from([
        ("retry-after-ms".to_owned(), "2000".to_owned()),
        ("x-ratelimit-remaining-requests".to_owned(), "0".to_owned()),
        ("x-ratelimit-reset-requests".to_owned(), "1m30s".to_owned()),
    ]);
    let failure = http_failure_parts(
        429,
        &explicit,
        br#"{"error":{"type":"rate_limit_error"}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(2_000));
}

#[test]
fn gemini_isolates_thoughts_and_generates_missing_function_call_ids() -> Result<(), Box<dyn Error>>
{
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::gemini())?;
    let adapter = registry.adapter(request.selection().provider_id())?;
    let mut decoder = adapter.make_decoder(&request)?;
    let emitted = decoder.consume(&event(
        None,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"private reasoning","thoughtSignature":"c2ln"},{"functionCall":{"name":"lookup","args":{"city":"Seoul"}},"thoughtSignature":"dG9vbA=="}]}}]}"#,
    ))?;
    let [
        ProviderDecodedEvent::Reasoning(reasoning),
        ProviderDecodedEvent::ToolCall(call),
    ] = emitted.as_slice()
    else {
        return Err("Gemini reasoning and function call were not emitted".into());
    };
    assert_eq!(reasoning, "private reasoning");
    assert_eq!(call.id(), "gemini-call-1");
    assert_eq!(call.name(), "lookup");
    assert_eq!(
        call.arguments()
            .get("city")
            .and_then(ProviderJsonValue::as_str),
        Some("Seoul"),
    );
    Ok(())
}

#[test]
fn provider_decoders_fail_closed_on_unsuccessful_terminal_states() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;

    let request = request_for(BuiltInProviderId::open_router())?;
    let mut chat = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    assert!(
        chat.consume(&event(
            None,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
        ))?
        .is_empty()
    );
    let failure = chat
        .consume(&event(None, "[DONE]"))
        .err()
        .ok_or("chat truncation was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);

    let request = request_for(BuiltInProviderId::open_router())?;
    let mut chat_error = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = chat_error
        .consume(&event(
            None,
            r#"{"error":{"message":"Incorrect API key provided: sk-proj-0123456789abcdef"}}"#,
        ))
        .err()
        .ok_or("chat stream failure was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);
    assert_eq!(failure.message(), "provider stream failed");
    assert!(!failure.message().contains("sk-proj-0123456789abcdef"));

    let request = request_for(BuiltInProviderId::open_router())?;
    let mut chat_limit = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = chat_limit
        .consume(&event(
            None,
            r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"secret"}}"#,
        ))
        .err()
        .ok_or("chat stream rate limit was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::RateLimited);
    assert_eq!(
        failure
            .evidence()
            .and_then(|value| value.remote_error_code()),
        Some("rate_limit_exceeded")
    );

    let request = request_for(BuiltInProviderId::open_ai())?;
    let mut responses = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = responses
        .consume(&event(
            Some("response.failed"),
            r#"{"type":"response.failed","response":{"error":{"message":"Incorrect API key provided: sk-proj-0123456789abcdef"}}}"#,
        ))
        .err()
        .ok_or("Responses failure was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);
    assert_eq!(failure.message(), "provider stream failed");
    assert!(!failure.message().contains("sk-proj-0123456789abcdef"));

    let request = request_for(BuiltInProviderId::anthropic())?;
    let mut messages_error = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = messages_error
        .consume(&event(
            Some("error"),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Incorrect API key provided: sk-proj-0123456789abcdef"}}"#,
        ))
        .err()
        .ok_or("Messages stream failure was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);
    assert_eq!(failure.message(), "Messages stream failed");
    assert!(!failure.message().contains("sk-proj-0123456789abcdef"));
    assert_eq!(
        failure
            .evidence()
            .and_then(|value| value.remote_error_type()),
        Some("overloaded_error")
    );

    let request = request_for(BuiltInProviderId::gemini())?;
    let mut gemini_limit = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = gemini_limit
        .consume(&event(
            None,
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"secret"}}"#,
        ))
        .err()
        .ok_or("Gemini stream quota was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::RateLimited);
    assert_eq!(failure.provider_status_code(), Some(429));
    assert!(failure.evidence().is_some());

    let request = request_for(BuiltInProviderId::anthropic())?;
    let mut messages = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    assert!(
        messages
            .consume(&event(
                Some("message_start"),
                r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1}}}"#,
            ))?
            .is_empty()
    );
    assert!(messages.consume(&event(
        Some("message_delta"),
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}}"#,
    ))?.is_empty());
    let failure = messages
        .consume(&event(Some("message_stop"), r#"{"type":"message_stop"}"#))
        .err()
        .ok_or("Messages truncation was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);

    let request = request_for(BuiltInProviderId::gemini())?;
    let mut gemini = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = gemini
        .consume(&event(
            None,
            r#"{"candidates":[{"finishReason":"SAFETY"}]}"#,
        ))
        .err()
        .ok_or("Gemini failure was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::PermissionDenied);
    assert_eq!(
        failure.message(),
        "Gemini blocked the response with finish reason SAFETY"
    );

    let request = request_for(BuiltInProviderId::gemini())?;
    let mut gemini_error = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = gemini_error
        .consume(&event(
            None,
            r#"{"error":{"message":"Incorrect API key provided: sk-proj-0123456789abcdef"}}"#,
        ))
        .err()
        .ok_or("Gemini stream failure was accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);
    assert_eq!(failure.message(), "Gemini stream failed");
    assert!(!failure.message().contains("sk-proj-0123456789abcdef"));
    Ok(())
}

#[test]
fn responses_stream_quota_failure_preserves_typed_reset_evidence() -> Result<(), Box<dyn Error>> {
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;
    let request = request_for(BuiltInProviderId::open_ai())?;
    let mut responses = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let body = r#"{"type":"response.failed","status":429,"response":{"error":{"type":"usage_limit_reached","code":"rate_limit_reached","message":"secret upstream detail","resets_at":1738888888}},"headers":{"x-request-id":"req_stream_42","x-should-retry":"false","x-codex-primary-used-percent":"100.0","x-codex-primary-window-minutes":300}}"#;
    let failure = responses
        .consume(&event(Some("response.failed"), body))
        .err()
        .ok_or("Responses quota failure was accepted")?;

    assert_eq!(failure.code(), ProviderFailureCode::RateLimited);
    assert_eq!(failure.provider_status_code(), Some(429));
    assert_eq!(failure.message(), "provider stream failed");
    assert!(!failure.message().contains("secret upstream detail"));
    let evidence = failure
        .evidence()
        .ok_or("Responses quota evidence was discarded")?;
    assert_eq!(evidence.remote_error_type(), Some("usage_limit_reached"));
    assert_eq!(evidence.remote_error_code(), Some("rate_limit_reached"));
    assert_eq!(evidence.reset_at_unix_seconds(), Some(1_738_888_888));
    assert_eq!(evidence.provider_should_retry(), Some(false));
    assert_eq!(evidence.upstream_request_id(), Some("req_stream_42"));
    assert_eq!(
        evidence
            .codex_primary()
            .and_then(ProviderRateLimitEvidence::used_percent_millis),
        Some(100_000)
    );
    assert_eq!(
        evidence
            .codex_primary()
            .and_then(ProviderRateLimitEvidence::window_minutes),
        Some(300)
    );
    assert_eq!(evidence.body_bytes(), Some(body.len() as u64));
    assert_eq!(evidence.body_sha256().map(str::len), Some(64));
    Ok(())
}

#[test]
fn provider_decoder_failures_redact_unrecognized_termination_values() -> Result<(), Box<dyn Error>>
{
    let registry = BuiltInProviderRegistry::new(&ProviderRuntimeOptions::default())?;

    let request = request_for(BuiltInProviderId::anthropic())?;
    let mut messages = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    assert!(messages.consume(&event(
        Some("message_start"),
        r#"{"type":"message_start","message":{"id":"m-marker","usage":{"input_tokens":1}}}"#,
    ))?.is_empty());
    assert!(
        messages
            .consume(&event(
                Some("message_delta"),
                r#"{"type":"message_delta","delta":{"stop_reason":"marker-secret"}}"#,
            ))?
            .is_empty()
    );
    let failure = messages
        .consume(&event(Some("message_stop"), r#"{"type":"message_stop"}"#))
        .err()
        .ok_or("Anthropic accepted an unsupported stop reason")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    assert!(!failure.message().contains("marker-secret"));

    let request = request_for(BuiltInProviderId::open_router())?;
    let mut chat = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    assert!(
        chat.consume(&event(
            None,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"marker-secret"}]}"#,
        ))?
        .is_empty()
    );
    let failure = chat
        .consume(&event(None, "[DONE]"))
        .err()
        .ok_or("OpenAI chat accepted an unsupported finish reason")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    assert!(!failure.message().contains("marker-secret"));

    let request = request_for(BuiltInProviderId::gemini())?;
    let mut gemini = registry
        .adapter(request.selection().provider_id())?
        .make_decoder(&request)?;
    let failure = gemini
        .consume(&event(
            None,
            r#"{"candidates":[{"finishReason":"marker-secret"}]}"#,
        ))
        .err()
        .ok_or("Gemini accepted an unsupported finish reason")?;
    assert_eq!(failure.code(), ProviderFailureCode::ServerFailed);
    assert!(!failure.message().contains("marker-secret"));

    Ok(())
}

#[test]
fn tool_argument_accumulator_is_bounded_and_requires_an_object() -> Result<(), Box<dyn Error>> {
    let mut accumulator = ProviderToolArgumentAccumulator::new(16)?;
    accumulator.append("{\"a\":")?;
    accumulator.append("1}")?;
    assert!(accumulator.decode_object()?.as_object().is_some());
    let mut array = ProviderToolArgumentAccumulator::new(16)?;
    array.append("[]")?;
    assert!(array.decode_object().is_err());
    assert!(ProviderToolArgumentAccumulator::new(0).is_err());
    let mut bounded = ProviderToolArgumentAccumulator::new(4)?;
    let failure = bounded
        .append("12345")
        .err()
        .ok_or("oversized arguments were accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::ResponseTooLarge);
    Ok(())
}

#[test]
fn strict_model_catalog_validation_rejects_duplicates_and_invalid_limits()
-> Result<(), Box<dyn Error>> {
    let capabilities = ProviderCapabilities::default();
    let duplicate = br#"{"data":[{"id":"marker-secret"},{"id":"marker-secret"}]}"#;
    let failure = parse_model_catalog(
        duplicate,
        &[&["data"]],
        &["id"],
        &capabilities,
        ProviderInstant::from_unix_milliseconds(1),
    )
    .err()
    .ok_or("duplicate model identifiers were accepted")?;
    assert_eq!(failure.code(), ProviderFailureCode::MalformedResponse);
    assert!(!failure.message().contains("marker-secret"));
    let invalid = br#"{"data":[{"id":"m1","context_length":0}]}"#;
    assert!(
        parse_model_catalog(
            invalid,
            &[&["data"]],
            &["id"],
            &capabilities,
            ProviderInstant::from_unix_milliseconds(1),
        )
        .is_err()
    );
    let wrong_identifier_type = br#"{"data":[{"id":42,"name":"fallback"}]}"#;
    assert!(
        parse_model_catalog(
            wrong_identifier_type,
            &[&["data"]],
            &["id", "name"],
            &capabilities,
            ProviderInstant::from_unix_milliseconds(1),
        )
        .is_err()
    );
    let malformed_nested_limit = br#"{"data":[{"id":"m1","top_provider":"invalid"}]}"#;
    assert!(
        parse_model_catalog(
            malformed_nested_limit,
            &[&["data"]],
            &["id"],
            &capabilities,
            ProviderInstant::from_unix_milliseconds(1),
        )
        .is_err()
    );
    let valid = br#"{"data":[{"id":"m1","display_name":"m1","context_length":8,"top_provider":{"max_completion_tokens":4}}]}"#;
    let catalog = parse_model_catalog(
        valid,
        &[&["data"]],
        &["id"],
        &capabilities,
        ProviderInstant::from_unix_milliseconds(1),
    )?;
    assert_eq!(catalog.models().len(), 1);
    assert_eq!(catalog.models()[0].display_name(), None);
    assert_eq!(catalog.models()[0].context_token_limit(), Some(8));
    assert_eq!(catalog.models()[0].maximum_output_tokens(), Some(4));
    Ok(())
}

#[test]
fn http_failures_are_typed_redacted_and_use_injected_time() -> Result<(), Box<dyn Error>> {
    let mut headers = BTreeMap::new();
    headers.insert("retry-after".to_owned(), "2".to_owned());
    headers.insert("x-request-id".to_owned(), "upstream-req-42".to_owned());
    headers.insert("x-codex-primary-used".to_owned(), "12".to_owned());
    headers.insert("x-codex-primary-window".to_owned(), "60".to_owned());
    headers.insert("x-codex-primary-reset".to_owned(), "1700000000".to_owned());
    headers.insert("x-codex-primary-credits".to_owned(), "88".to_owned());
    headers.insert("x-codex-secondary-used".to_owned(), "3".to_owned());
    let failure = http_failure_parts(
        429,
        &headers,
        br#"{"error":{"message":"Incorrect API key provided: sk-proj-0123456789abcdef","type":"rate_limit_error","code":"usage_limit_reached"}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.code(), ProviderFailureCode::RateLimited);
    assert_eq!(
        failure.message(),
        "provider HTTP request failed with status 429"
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(2_000));
    assert!(!failure.message().contains("sk-proj-0123456789abcdef"));
    let evidence = failure
        .evidence()
        .ok_or("HTTP failure evidence was discarded")?;
    assert_eq!(evidence.body_bytes(), Some(131));
    assert_eq!(evidence.upstream_request_id(), Some("upstream-req-42"));
    assert_eq!(evidence.remote_error_type(), Some("rate_limit_error"));
    assert_eq!(evidence.remote_error_code(), Some("usage_limit_reached"));
    assert_eq!(evidence.codex_primary().and_then(|v| v.used()), Some(12));
    assert_eq!(evidence.codex_primary().and_then(|v| v.window()), Some(60));
    assert_eq!(
        evidence.codex_primary().and_then(|v| v.reset()),
        Some(1_700_000_000)
    );
    assert_eq!(evidence.codex_primary().and_then(|v| v.credits()), Some(88));
    assert_eq!(evidence.codex_secondary().and_then(|v| v.used()), Some(3));
    assert_eq!(evidence.codex_secondary().and_then(|v| v.window()), None);
    assert_eq!(evidence.body_sha256().map(str::len), Some(64));
    assert_eq!(
        evidence.body_sha256(),
        Some("e5ddb50a8360400e397de65c7f805dfa6bbbb3cb259e75817859eca42d79bd77")
    );
    Ok(())
}

#[test]
fn codex_usage_limit_preserves_official_reset_and_window_evidence() {
    let headers = BTreeMap::from([
        (
            "x-codex-primary-used-percent".to_owned(),
            "100.0".to_owned(),
        ),
        (
            "x-codex-primary-window-minutes".to_owned(),
            "10080".to_owned(),
        ),
        ("x-should-retry".to_owned(), "false".to_owned()),
    ]);
    let failure = http_failure_parts(
        429,
        &headers,
        br#"{"error":{"type":"usage_limit_reached","message":"limit","resets_at":1738888888}}"#,
        ProviderInstant::from_unix_milliseconds(1_700_000_000_000),
    );
    let evidence = failure.evidence().unwrap_or_else(|| unreachable!());
    assert_eq!(evidence.reset_at_unix_seconds(), Some(1_738_888_888));
    assert_eq!(evidence.provider_should_retry(), Some(false));
    let primary = evidence.codex_primary().unwrap_or_else(|| unreachable!());
    assert_eq!(primary.used_percent_millis(), Some(100_000));
    assert_eq!(primary.window_minutes(), Some(10_080));
}

#[test]
fn codex_usage_limit_derives_reset_from_bounded_relative_seconds() {
    let failure = http_failure_parts(
        429,
        &BTreeMap::new(),
        br#"{"error":{"type":"usage_limit_reached","resets_in_seconds":90}}"#,
        ProviderInstant::from_unix_milliseconds(1_700_000_000_500),
    );
    let evidence = failure.evidence().unwrap_or_else(|| unreachable!());
    assert_eq!(evidence.reset_at_unix_seconds(), Some(1_700_000_090));
}

#[test]
fn http_failure_retry_after_is_clamped_and_untrusted_fields_are_omitted() {
    let headers = BTreeMap::from([
        ("retry-after".to_owned(), u64::MAX.to_string()),
        ("x-request-id".to_owned(), "unsafe request id".to_owned()),
        ("x-codex-primary-used".to_owned(), "not-a-number".to_owned()),
    ]);
    let failure = http_failure_parts(
        429,
        &headers,
        br#"{"error":{"type":"unsafe type!","code":"usage_limit_reached"}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    assert_eq!(failure.retry_after_milliseconds(), Some(604_800_000));
    let evidence = failure.evidence().unwrap_or_else(|| unreachable!());
    assert_eq!(evidence.upstream_request_id(), None);
    assert_eq!(evidence.remote_error_type(), None);
    assert_eq!(evidence.remote_error_code(), Some("usage_limit_reached"));
    assert_eq!(evidence.codex_primary(), None);
}

#[test]
fn http_failure_remote_error_allowlist_preserves_body_evidence() {
    let body = br#"{"error":{"type":"sk-proj-secret-token","code":"unknown_remote_code"}}"#;
    let failure = http_failure_parts(
        429,
        &BTreeMap::new(),
        body,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    let evidence = failure.evidence().unwrap_or_else(|| unreachable!());
    assert_eq!(evidence.body_bytes(), Some(body.len() as u64));
    assert!(evidence.body_sha256().is_some());
    assert_eq!(evidence.remote_error_type(), None);
    assert_eq!(evidence.remote_error_code(), None);
}

#[test]
fn http_failure_request_id_credential_is_omitted_without_losing_safe_fields() {
    let headers = BTreeMap::from([
        ("x-request-id".to_owned(), "sk-proj-secret-token".to_owned()),
        ("retry-after".to_owned(), "2".to_owned()),
        ("x-codex-primary-used".to_owned(), "4".to_owned()),
    ]);
    let body = br#"{"error":{"type":"rate_limit_error","code":"usage_limit_reached"}}"#;
    let failure = http_failure_parts(
        429,
        &headers,
        body,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    let evidence = failure.evidence().unwrap_or_else(|| unreachable!());
    assert_eq!(evidence.upstream_request_id(), None);
    assert_eq!(evidence.body_bytes(), Some(body.len() as u64));
    assert!(evidence.body_sha256().is_some());
    assert_eq!(failure.retry_after_milliseconds(), Some(2_000));
    assert_eq!(
        evidence.codex_primary().and_then(|value| value.used()),
        Some(4)
    );
    assert_eq!(evidence.remote_error_type(), Some("rate_limit_error"));
    assert_eq!(evidence.remote_error_code(), Some("usage_limit_reached"));
}

#[test]
fn http_failure_evidence_survives_context_enrichment_and_legacy_serde() -> Result<(), Box<dyn Error>>
{
    let mut headers = BTreeMap::new();
    headers.insert("x-request-id".to_owned(), "upstream-req".to_owned());
    headers.insert("x-should-retry".to_owned(), "false".to_owned());
    let failure = http_failure_parts(
        429,
        &headers,
        br#"{"error":{"type":"rate_limit_error","code":"usage_limit_reached","resets_at":1738888888}}"#,
        ProviderInstant::from_unix_milliseconds(1_000),
    );
    let enriched =
        crate::execution_session::with_failure_context(failure.clone(), Some("codex route"));
    assert_eq!(enriched.evidence(), failure.evidence());
    let encoded = serde_json::to_vec(&failure)?;
    let decoded: ProviderFailure = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded.evidence(), failure.evidence());
    assert_eq!(
        decoded
            .evidence()
            .and_then(|evidence| evidence.reset_at_unix_seconds()),
        Some(1_738_888_888)
    );
    assert_eq!(
        decoded
            .evidence()
            .and_then(|evidence| evidence.provider_should_retry()),
        Some(false)
    );
    let legacy = br#"{"code":"rate_limited","message":"old","provider_status_code":429,"retry_after_milliseconds":2000,"request_id":null}"#;
    let old: ProviderFailure = serde_json::from_slice(legacy)?;
    assert_eq!(old.evidence(), None);
    Ok(())
}

#[test]
fn transport_timeout_maps_to_the_public_timed_out_failure() {
    let failure = transport_failure(ProviderTransportError::TimedOut);
    assert_eq!(failure.code(), ProviderFailureCode::TimedOut);
    assert_eq!(failure.message(), "provider network request timed out");
}

#[test]
fn http_request_rejects_insecure_urls_and_header_injection() -> Result<(), Box<dyn Error>> {
    let insecure = ProviderHttpRequest::new(
        Method::GET,
        Url::parse("http://example.com")?,
        BTreeMap::new(),
        Vec::new(),
        1_024,
    );
    assert!(insecure.is_err());
    let injected = ProviderHttpRequest::new(
        Method::GET,
        Url::parse("https://example.com")?,
        BTreeMap::from([("authorization".to_owned(), "x\r\ny: z".to_owned())]),
        Vec::new(),
        1_024,
    );
    assert!(injected.is_err());
    Ok(())
}

#[test]
fn http_request_timeout_is_explicit_and_bounded() -> Result<(), Box<dyn Error>> {
    let request = ProviderHttpRequest::with_timeout(
        Method::GET,
        Url::parse("https://example.com")?,
        BTreeMap::new(),
        Vec::new(),
        60_000,
        1_024,
    )?;
    assert_eq!(request.timeout_milliseconds(), 60_000);

    assert!(
        ProviderHttpRequest::with_timeout(
            Method::GET,
            Url::parse("https://example.com")?,
            BTreeMap::new(),
            Vec::new(),
            999,
            1_024,
        )
        .is_err()
    );
    assert!(
        ProviderHttpRequest::with_timeout(
            Method::GET,
            Url::parse("https://example.com")?,
            BTreeMap::new(),
            Vec::new(),
            3_600_001,
            1_024,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn http_debug_output_redacts_query_headers_and_body() -> Result<(), Box<dyn Error>> {
    let request = ProviderHttpRequest::new(
        Method::POST,
        Url::parse("https://example.com/v1/run?token=query-secret")?,
        BTreeMap::from([
            (
                "authorization".to_owned(),
                "Bearer header-secret".to_owned(),
            ),
            ("x-safe".to_owned(), "visible-value".to_owned()),
        ]),
        b"body-secret".to_vec(),
        1_024,
    )?;
    let rendered = format!("{request:?}");
    assert!(rendered.contains("authorization"));
    assert!(!rendered.contains("query-secret"));
    assert!(!rendered.contains("header-secret"));
    assert!(!rendered.contains("visible-value"));
    assert!(!rendered.contains("body-secret"));
    Ok(())
}

#[test]
fn json_request_preserves_timeout_and_sets_default_headers() -> Result<(), Box<dyn Error>> {
    let constraints = ProviderRequestConstraints::default();
    let body = json_object([("ok", ProviderJsonValue::from(true))]);
    let request = make_json_request(
        Method::POST,
        Url::parse("https://example.com/v1/run")?,
        BTreeMap::new(),
        &body,
        &constraints,
    )?;
    assert_eq!(
        request.headers().get("content-type").map(String::as_str),
        Some("application/json"),
    );
    assert_eq!(
        request.headers().get("accept").map(String::as_str),
        Some("application/json"),
    );
    assert_eq!(
        request.timeout_milliseconds(),
        constraints.timeout_milliseconds(),
    );

    let request = make_json_request(
        Method::POST,
        Url::parse("https://example.com/v1/run")?,
        BTreeMap::from([(
            "Content-Type".to_owned(),
            "application/problem+json".to_owned(),
        )]),
        &body,
        &constraints,
    )?;
    assert_eq!(
        request.headers().get("Content-Type").map(String::as_str),
        Some("application/problem+json"),
    );
    assert_eq!(
        request.headers().get("accept").map(String::as_str),
        Some("application/json"),
    );
    assert_eq!(request.headers().len(), 2);
    Ok(())
}

#[tokio::test]
async fn secure_regular_file_reader_rejects_symlinks_and_oversize() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let regular = directory.path().join("auth.json");
    fs::write(&regular, b"{}")?;
    assert_eq!(SecureRegularFileReader::read(&regular, 16)?, b"{}");
    assert_eq!(
        SecureRegularFileReader::read_async(&regular, 16).await?,
        b"{}"
    );
    assert!(SecureRegularFileReader::read(&regular, 1).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = directory.path().join("auth-link.json");
        symlink(&regular, &link)?;
        assert!(SecureRegularFileReader::read(&link, 16).is_err());
    }
    Ok(())
}
