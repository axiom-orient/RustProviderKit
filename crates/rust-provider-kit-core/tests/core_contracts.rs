use std::collections::BTreeMap;
use std::error::Error;

use rust_provider_kit_core::*;

fn selection() -> Result<ProviderSelection, ProviderCoreError> {
    Ok(ProviderSelection::new(
        BuiltInProviderId::open_ai(),
        ProviderAccountId::new("account-main")?,
        ProviderModelId::new("gpt-test")?,
    ))
}

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

fn turn_request() -> Result<ProviderTurnRequest, ProviderCoreError> {
    let tool = ProviderToolDefinition::new("lookup", "Look up a bounded value", schema(), true)?;
    ProviderTurnRequest::new(
        ProviderRequestId::new("request-1")?,
        selection()?,
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        vec![tool],
        ProviderToolChoice::named("lookup")?,
        ProviderOutputRequirement::application_validated_json("answer", schema())?,
        ProviderReasoningPolicy::Effort(ProviderReasoningEffort::High),
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

fn registration() -> Result<ProviderAccountRegistrationRequest, ProviderCoreError> {
    ProviderAccountRegistrationRequest::new(
        ProviderAccountId::new("account-main")?,
        BuiltInProviderId::open_ai(),
        "Primary",
        ProviderCredentialMaterial::api_key("secret-value")?,
        None,
    )
}

fn staged_record() -> Result<ProviderCredentialRecord, ProviderCoreError> {
    ProviderCredentialRecord::new(
        ProviderCredentialReference::new("credential-1")?,
        ProviderAccountId::new("account-main")?,
        BuiltInProviderId::open_ai(),
        "Primary",
        ProviderCredentialSource::ApiKey,
        ProviderCredentialRecordState::Staged,
        None,
        ProviderInstant::from_unix_milliseconds(10),
        ProviderInstant::from_unix_milliseconds(10),
    )
}

fn ready_inspection() -> Result<ProviderAccountInspection, ProviderCoreError> {
    ProviderAccountInspection::new(
        ProviderAccountId::new("account-main")?,
        BuiltInProviderId::open_ai(),
        ProviderAccountReadiness::Ready,
        None,
        ProviderCapabilities::default(),
        ProviderInstant::from_unix_milliseconds(20),
    )
}

#[test]
fn identifiers_validate_and_round_trip() -> Result<(), Box<dyn Error>> {
    let value = ProviderId::new("openai-compatible")?;
    let encoded = serde_json::to_vec(&value)?;
    let decoded: ProviderId = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, value);
    assert!(ProviderId::new("OpenAI").is_err());
    assert!(ProviderRequestId::new(" contains-space ").is_err());
    assert_eq!(BuiltInProviderId::all().len(), 10);
    Ok(())
}

#[test]
fn json_is_deterministic_and_bounded() -> Result<(), Box<dyn Error>> {
    let left = json_object([
        ("z", ProviderJsonValue::from(1_i64)),
        ("a", ProviderJsonValue::from(true)),
    ]);
    let right = json_object([
        ("a", ProviderJsonValue::from(true)),
        ("z", ProviderJsonValue::from(1_i64)),
    ]);
    assert_eq!(left.encoded_vec()?, right.encoded_vec()?);
    assert_eq!(ProviderJsonValue::decode(&left.encoded_vec()?)?, left);
    let oversized =
        ProviderJsonValue::String("x".repeat(ProviderJsonValue::MAXIMUM_STRING_UTF8_BYTES + 1));
    assert!(oversized.validated().is_err());
    assert!(ProviderJsonValue::Number(f64::NAN).validated().is_err());
    Ok(())
}

#[test]
fn json_to_serde_preserves_integral_number_shape() -> Result<(), Box<dyn Error>> {
    let value = json_object([("count", ProviderJsonValue::from(4_096_i64))]);
    let converted = serde_json::Value::try_from(value)?;
    assert_eq!(converted, serde_json::json!({"count": 4_096}));
    Ok(())
}

#[test]
fn json_preserves_full_width_integers() -> Result<(), Box<dyn Error>> {
    let signed = i64::MAX;
    let unsigned = u64::MAX;
    let value = json_object([
        ("signed", ProviderJsonValue::from(signed)),
        (
            "unsigned",
            ProviderJsonValue::decode(unsigned.to_string().as_bytes())?,
        ),
    ]);
    assert_eq!(
        value.encoded_vec()?,
        format!(r#"{{"signed":{signed},"unsigned":{unsigned}}}"#).into_bytes()
    );
    assert_eq!(ProviderJsonValue::decode(&value.encoded_vec()?)?, value);
    Ok(())
}

#[test]
fn tool_history_preserves_call_identity_and_error_status() -> Result<(), Box<dyn Error>> {
    let call = ProviderToolCall::new(
        "call-1",
        "lookup",
        json_object([("query", ProviderJsonValue::from("value"))]),
    )?;
    let assistant = ProviderMessage::new(
        ProviderMessageRole::Assistant,
        vec![
            ProviderMessageContent::text("checking"),
            ProviderMessageContent::tool_call(call),
        ],
    )?;
    let tool = ProviderMessage::new(
        ProviderMessageRole::Tool,
        vec![ProviderMessageContent::tool_result_with_status(
            "call-1",
            "lookup",
            ProviderJsonValue::from("not found"),
            true,
        )?],
    )?;

    let decoded_assistant: ProviderMessage =
        serde_json::from_slice(&serde_json::to_vec(&assistant)?)?;
    let decoded_tool: ProviderMessage = serde_json::from_slice(&serde_json::to_vec(&tool)?)?;
    assert_eq!(
        decoded_assistant.content()[1]
            .tool_call_value()
            .map(ProviderToolCall::id),
        Some("call-1")
    );
    assert_eq!(decoded_tool.content()[0].tool_result_is_error(), Some(true));
    assert!(
        ProviderMessage::new(
            ProviderMessageRole::User,
            vec![ProviderMessageContent::tool_call(ProviderToolCall::new(
                "call-2",
                "lookup",
                json_object(std::iter::empty::<(String, ProviderJsonValue)>()),
            )?)],
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn turn_request_round_trips_every_policy() -> Result<(), Box<dyn Error>> {
    let request = turn_request()?;
    let encoded = serde_json::to_vec(&request)?;
    let decoded: ProviderTurnRequest = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, request);
    assert_eq!(
        decoded.selection().provider_id(),
        &BuiltInProviderId::open_ai()
    );
    assert_eq!(decoded.selection().account_id().as_str(), "account-main");
    assert_eq!(decoded.selection().model_id().as_str(), "gpt-test");
    assert_eq!(decoded.constraints().maximum_output_tokens(), Some(4_096));
    assert!(matches!(
        decoded.reasoning(),
        ProviderReasoningPolicy::Effort(ProviderReasoningEffort::High)
    ));
    assert_eq!(decoded.tool_choice().named_value(), Some("lookup"));
    Ok(())
}

#[test]
fn request_rejects_cross_account_continuation_and_invalid_constraints() -> Result<(), Box<dyn Error>>
{
    let selection = selection()?;
    let continuation = ProviderContinuation::new(
        selection.provider_id().clone(),
        ProviderAccountId::new("another-account")?,
        "continuation",
    )?;
    let request = ProviderTurnRequest::new(
        ProviderRequestId::new("request-2")?,
        selection,
        vec![ProviderMessage::text(ProviderMessageRole::User, "hello")?],
        Vec::new(),
        ProviderToolChoice::Automatic,
        ProviderOutputRequirement::Text,
        ProviderReasoningPolicy::Automatic,
        Some(continuation),
        ProviderRequestConstraints::default(),
    );
    assert!(request.is_err());
    assert!(
        ProviderRequestConstraints::new(
            ProviderDataCollectionPolicy::Deny,
            true,
            true,
            999,
            1_024,
            1,
            None,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn codable_input_cannot_bypass_invariants() {
    let invalid = br#"{"id":"request-1","selection":{"provider_id":"openai","account_id":"account-main","model_id":"gpt-test"},"messages":[],"tools":[],"tool_choice":{"type":"automatic"},"output":{"type":"text"},"reasoning":{"type":"automatic"},"continuation":null,"constraints":{"data_collection":"deny","requires_zero_data_retention":true,"requires_parameter_support":true,"timeout_milliseconds":30000,"maximum_response_bytes":1024,"maximum_retry_attempts":1,"maximum_output_tokens":null}}"#;
    let decoded = serde_json::from_slice::<ProviderTurnRequest>(invalid);
    assert!(decoded.is_err());
}

#[test]
fn removed_endpoint_fallback_field_is_rejected() {
    let removed_field = br#"{"data_collection":"deny","requires_zero_data_retention":true,"requires_parameter_support":true,"allows_provider_endpoint_fallbacks":false,"timeout_milliseconds":30000,"maximum_response_bytes":1024,"maximum_retry_attempts":1,"maximum_output_tokens":null}"#;
    assert!(serde_json::from_slice::<ProviderRequestConstraints>(removed_field).is_err());
}

#[test]
fn standalone_policy_serialization_cannot_bypass_invariants() -> Result<(), Box<dyn Error>> {
    let invalid_choice = ProviderToolChoice::Named {
        name: "invalid name".to_owned(),
    };
    assert!(serde_json::to_vec(&invalid_choice).is_err());
    assert!(
        serde_json::from_str::<ProviderToolChoice>(r#"{"type":"named","name":"invalid name"}"#,)
            .is_err()
    );

    let invalid_output = ProviderOutputRequirement::JsonSchema {
        name: "answer".to_owned(),
        schema: ProviderJsonValue::from("not-an-object"),
        strict: true,
    };
    assert!(serde_json::to_vec(&invalid_output).is_err());
    assert!(
        serde_json::from_str::<ProviderOutputRequirement>(
            r#"{"type":"json_schema","name":"answer","schema":"not-an-object","strict":true}"#,
        )
        .is_err()
    );

    let valid_choice = ProviderToolChoice::named("lookup")?;
    let encoded = serde_json::to_vec(&valid_choice)?;
    assert_eq!(
        serde_json::from_slice::<ProviderToolChoice>(&encoded)?,
        valid_choice,
    );

    let valid_output = ProviderOutputRequirement::json_schema("answer", schema(), true)?;
    let encoded = serde_json::to_vec(&valid_output)?;
    assert_eq!(
        serde_json::from_slice::<ProviderOutputRequirement>(&encoded)?,
        valid_output,
    );
    Ok(())
}

#[test]
fn sensitive_values_and_failures_are_redacted() -> Result<(), Box<dyn Error>> {
    let secret = SensitiveValue::new("actual-secret")?;
    assert_eq!(secret.to_string(), "<redacted>");
    assert!(!format!("{secret:?}").contains("actual-secret"));
    let auth_file = ProviderCredentialMaterial::external_auth_file("/private/auth.json")?;
    assert!(!format!("{auth_file:?}").contains("/private/auth.json"));
    let failure = ProviderFailure::new(
        ProviderFailureCode::AuthenticationFailed,
        "Bearer abc.def api_key = visible access token: hidden",
    );
    assert!(!failure.message().contains("abc.def"));
    assert!(!failure.message().contains("visible"));
    assert!(!failure.message().contains("hidden"));
    assert!(
        ProviderFailure::new(ProviderFailureCode::ServerFailed, "bad")
            .with_status(99)
            .is_err()
    );
    Ok(())
}

#[test]
fn endpoint_and_authorization_inputs_fail_closed() -> Result<(), Box<dyn Error>> {
    assert!(ProviderEndpointConfiguration::parse("http://example.com").is_err());
    assert!(ProviderEndpointConfiguration::parse("https://user@example.com").is_err());
    let endpoint = ProviderEndpointConfiguration::parse_with_headers(
        "https://example.com",
        BTreeMap::from([("x-title".to_owned(), "Arc".to_owned())]),
    )?;
    assert_eq!(endpoint.headers().get("x-title"), Some(&"Arc".to_owned()));
    assert!(
        ProviderEndpointConfiguration::parse_with_headers(
            "https://example.com",
            BTreeMap::from([("x-provider-token".to_owned(), "value".to_owned())]),
        )
        .is_err()
    );
    let url = url::Url::parse("https://example.com/authorize")?;
    assert!(
        ProviderAuthorizationRequest::new(
            BuiltInProviderId::open_router(),
            url.clone(),
            "1invalid",
            "state",
        )
        .is_err()
    );
    assert!(
        ProviderAuthorizationRequest::new(BuiltInProviderId::open_router(), url, "http", "",)
            .is_err()
    );
    Ok(())
}

#[test]
fn credential_lease_requires_active_matching_material() -> Result<(), Box<dyn Error>> {
    let staged = staged_record()?;
    let material = ProviderCredentialMaterial::api_key("secret-value")?;
    assert!(ProviderCredentialLease::new(staged.clone(), material.clone()).is_err());
    assert!(ProviderCredentialLease::for_verification(staged, material).is_ok());
    Ok(())
}

#[test]
fn account_reducer_success_is_staged_verified_then_active() -> Result<(), Box<dyn Error>> {
    let request = registration()?;
    let (state, effects) = ProviderAccountReducer::reduce(
        ProviderAccountState::default(),
        ProviderAccountEvent::RegistrationRequested(request.clone()),
    )?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::StagingCredential(_)
    ));
    assert_eq!(effects.len(), 2);

    let record = staged_record()?;
    let (state, effects) = ProviderAccountReducer::reduce(
        state,
        ProviderAccountEvent::CredentialStaged(record.clone()),
    )?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::VerifyingAccount(_, _)
    ));
    assert_eq!(effects.len(), 2);

    let (state, effects) = ProviderAccountReducer::reduce(
        state,
        ProviderAccountEvent::VerificationSucceeded(ready_inspection()?),
    )?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::ActivatingCredential(_, _, _)
    ));
    assert_eq!(effects.len(), 2);

    let (state, effects) =
        ProviderAccountReducer::reduce(state, ProviderAccountEvent::ActivationSucceeded)?;
    assert!(matches!(state.phase(), ProviderAccountPhase::Ready(_)));
    assert!(matches!(
        effects.as_slice(),
        [ProviderAccountEffect::Publish(
            ProviderAccountPublicEvent::Ready(_)
        )]
    ));
    Ok(())
}

#[test]
fn account_compensation_failure_is_recovery_required() -> Result<(), Box<dyn Error>> {
    let request = registration()?;
    let (state, _) = ProviderAccountReducer::reduce(
        ProviderAccountState::default(),
        ProviderAccountEvent::RegistrationRequested(request),
    )?;
    let (state, _) = ProviderAccountReducer::reduce(
        state,
        ProviderAccountEvent::CredentialStaged(staged_record()?),
    )?;
    let original = ProviderFailure::new(
        ProviderFailureCode::AuthenticationFailed,
        "invalid credential",
    );
    let (state, effects) =
        ProviderAccountReducer::reduce(state, ProviderAccountEvent::OperationFailed(original))?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::Compensating(_, _, _)
    ));
    assert!(matches!(
        effects.as_slice(),
        [ProviderAccountEffect::RemoveStagedCredential(_, _)]
    ));

    let cleanup = ProviderFailure::new(
        ProviderFailureCode::CredentialRecoveryRequired,
        "cleanup failed",
    );
    let (state, effects) =
        ProviderAccountReducer::reduce(state, ProviderAccountEvent::CompensationFailed(cleanup))?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::RecoveryRequired(_, _)
    ));
    assert!(matches!(
        effects.as_slice(),
        [ProviderAccountEffect::Publish(
            ProviderAccountPublicEvent::RecoveryRequired(_)
        )]
    ));
    Ok(())
}

#[test]
fn inconsistent_staged_record_enters_compensation() -> Result<(), Box<dyn Error>> {
    let request = registration()?;
    let (state, _) = ProviderAccountReducer::reduce(
        ProviderAccountState::default(),
        ProviderAccountEvent::RegistrationRequested(request),
    )?;
    let inconsistent = ProviderCredentialRecord::new(
        ProviderCredentialReference::new("credential-other")?,
        ProviderAccountId::new("different-account")?,
        BuiltInProviderId::open_ai(),
        "Primary",
        ProviderCredentialSource::ApiKey,
        ProviderCredentialRecordState::Staged,
        None,
        ProviderInstant::from_unix_milliseconds(10),
        ProviderInstant::from_unix_milliseconds(10),
    )?;
    let (state, effects) = ProviderAccountReducer::reduce(
        state,
        ProviderAccountEvent::CredentialStaged(inconsistent),
    )?;
    assert!(matches!(
        state.phase(),
        ProviderAccountPhase::Compensating(_, _, _)
    ));
    assert!(matches!(
        effects.as_slice(),
        [ProviderAccountEffect::RemoveStagedCredential(_, _)]
    ));
    Ok(())
}

#[test]
fn reducer_generations_never_wrap() -> Result<(), Box<dyn Error>> {
    let state = ProviderAccountState::new(
        u64::MAX,
        ProviderAccountPhase::Failed(ProviderFailure::new(
            ProviderFailureCode::InvalidRequest,
            "previous",
        )),
    );
    let result = ProviderAccountReducer::reduce(
        state,
        ProviderAccountEvent::RegistrationRequested(registration()?),
    );
    assert!(result.is_err());
    let result = ProviderExecutionReducer::reduce(
        ProviderExecutionState::new(u64::MAX, false, ProviderExecutionPhase::Idle),
        ProviderExecutionEvent::RequestAdmitted(turn_request()?),
    );
    assert!(result.is_err());
    Ok(())
}

#[test]
fn execution_publishes_terminal_only_after_cleanup() -> Result<(), Box<dyn Error>> {
    let request = turn_request()?;
    let (state, _) = ProviderExecutionReducer::reduce(
        ProviderExecutionState::default(),
        ProviderExecutionEvent::RequestAdmitted(request.clone()),
    )?;
    let metadata = ProviderResponseMetadata::new(
        request.id().clone(),
        Some("upstream-1".to_owned()),
        request.selection().provider_id().clone(),
        request.selection().model_id().clone(),
        ProviderInstant::from_unix_milliseconds(100),
    )?;
    let (state, effects) =
        ProviderExecutionReducer::reduce(state, ProviderExecutionEvent::TransportOpened(metadata))?;
    assert!(matches!(
        effects.as_slice(),
        [ProviderExecutionEffect::Publish(
            ProviderTurnEvent::Started(_)
        )]
    ));
    let completion = ProviderCompletion::new(
        Some("response-1".to_owned()),
        None,
        None,
        ProviderInstant::from_unix_milliseconds(200),
    )?;
    let (state, effects) = ProviderExecutionReducer::reduce(
        state,
        ProviderExecutionEvent::TransportCompleted(completion),
    )?;
    assert!(matches!(
        state.phase(),
        ProviderExecutionPhase::Terminating(_, _, false)
    ));
    assert!(matches!(
        effects.as_slice(),
        [ProviderExecutionEffect::BeginCleanup(false, _)]
    ));
    let (state, effects) =
        ProviderExecutionReducer::reduce(state, ProviderExecutionEvent::CleanupCompleted)?;
    assert!(matches!(state.phase(), ProviderExecutionPhase::Terminal(_)));
    assert!(matches!(
        effects.as_slice(),
        [ProviderExecutionEffect::Publish(
            ProviderTurnEvent::Terminal(_)
        )]
    ));
    Ok(())
}

#[test]
fn execution_completion_preserves_the_started_publication_state() -> Result<(), Box<dyn Error>> {
    let request = turn_request()?;
    let metadata = ProviderResponseMetadata::new(
        request.id().clone(),
        Some("upstream-1".to_owned()),
        request.selection().provider_id().clone(),
        request.selection().model_id().clone(),
        ProviderInstant::from_unix_milliseconds(100),
    )?;
    let state = ProviderExecutionState::new(
        7,
        false,
        ProviderExecutionPhase::Streaming(request, metadata, false),
    );
    let completion = ProviderCompletion::new(
        Some("response-1".to_owned()),
        None,
        None,
        ProviderInstant::from_unix_milliseconds(200),
    )?;
    let (state, effects) = ProviderExecutionReducer::reduce(
        state,
        ProviderExecutionEvent::TransportCompleted(completion),
    )?;

    assert!(state.has_published_started());
    assert!(matches!(
        effects.as_slice(),
        [
            ProviderExecutionEffect::Publish(ProviderTurnEvent::Started(_)),
            ProviderExecutionEffect::BeginCleanup(false, 7),
        ]
    ));
    Ok(())
}

#[test]
fn execution_retries_only_before_visible_output() -> Result<(), Box<dyn Error>> {
    let request = turn_request()?;
    let (state, _) = ProviderExecutionReducer::reduce(
        ProviderExecutionState::default(),
        ProviderExecutionEvent::RequestAdmitted(request.clone()),
    )?;
    let metadata = ProviderResponseMetadata::new(
        request.id().clone(),
        None,
        request.selection().provider_id().clone(),
        request.selection().model_id().clone(),
        ProviderInstant::from_unix_milliseconds(100),
    )?;
    let (state, _) =
        ProviderExecutionReducer::reduce(state, ProviderExecutionEvent::TransportOpened(metadata))?;
    let (retrying, effects) =
        ProviderExecutionReducer::reduce(state.clone(), ProviderExecutionEvent::RetryRequested)?;
    assert!(matches!(
        retrying.phase(),
        ProviderExecutionPhase::Opening(_)
    ));
    assert!(effects.is_empty());
    let (visible, _) = ProviderExecutionReducer::reduce(
        state,
        ProviderExecutionEvent::TextDeltaReceived("x".to_owned()),
    )?;
    assert!(
        ProviderExecutionReducer::reduce(visible, ProviderExecutionEvent::RetryRequested).is_err()
    );
    Ok(())
}

#[test]
fn event_stream_bounds_are_typed_validation_errors() {
    assert!(matches!(
        ProviderEventStream::make(1, 64),
        Err(error) if error.code == ProviderCoreErrorCode::InvalidValue
    ));
    assert!(matches!(
        ProviderEventStream::make(2, 0),
        Err(error) if error.code == ProviderCoreErrorCode::InvalidValue
    ));
    assert!(matches!(
        ProviderAccountEventStream::make(1),
        Err(error) if error.code == ProviderCoreErrorCode::InvalidValue
    ));
}

#[tokio::test]
async fn failed_streams_publish_exactly_one_terminal_without_a_producer() {
    let failure = ProviderFailure::new(ProviderFailureCode::InvalidRequest, "request rejected");
    let mut turn = ProviderEventStream::failed(failure.clone());
    assert_eq!(
        turn.next().await,
        Some(ProviderTurnEvent::Terminal(ProviderTerminal::Failed(
            failure.clone(),
        )))
    );
    assert_eq!(turn.next().await, None);

    let mut account = ProviderAccountEventStream::failed(failure.clone());
    assert_eq!(
        account.next().await,
        Some(ProviderAccountPublicEvent::Failed(failure))
    );
    assert_eq!(account.next().await, None);
}

#[tokio::test]
async fn event_stream_coalesces_and_preserves_terminal() -> Result<(), Box<dyn Error>> {
    let (mut stream, sink) = ProviderEventStream::make(3, 64)?;
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta("a".to_owned())),
        ProviderMailboxSendResult::Accepted
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta("b".to_owned())),
        ProviderMailboxSendResult::Coalesced
    );
    assert!(sink.finish(ProviderTerminal::Cancelled));
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::TextDelta("ab".to_owned()))
    );
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::Terminal(ProviderTerminal::Cancelled))
    );
    assert_eq!(stream.next().await, None);
    Ok(())
}

#[tokio::test]
async fn event_stream_coalesces_reasoning_without_mixing_it_with_text() -> Result<(), Box<dyn Error>>
{
    let (mut stream, sink) = ProviderEventStream::make(4, 64)?;
    assert_eq!(
        sink.send(ProviderTurnEvent::ReasoningDelta("plan ".to_owned())),
        ProviderMailboxSendResult::Accepted
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::ReasoningDelta("step".to_owned())),
        ProviderMailboxSendResult::Coalesced
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta("answer".to_owned())),
        ProviderMailboxSendResult::Accepted
    );
    assert!(sink.finish(ProviderTerminal::Cancelled));
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::ReasoningDelta("plan step".to_owned()))
    );
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::TextDelta("answer".to_owned()))
    );
    Ok(())
}

#[tokio::test]
async fn event_stream_overflow_is_explicit_and_terminal_slot_is_reserved()
-> Result<(), Box<dyn Error>> {
    let (mut stream, sink) = ProviderEventStream::make(2, 1)?;
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta("a".to_owned())),
        ProviderMailboxSendResult::Accepted
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta("b".to_owned())),
        ProviderMailboxSendResult::Overflow
    );
    assert!(sink.finish(ProviderTerminal::Cancelled));
    assert!(matches!(
        stream.next().await,
        Some(ProviderTurnEvent::TextDelta(_))
    ));
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::Terminal(ProviderTerminal::Cancelled))
    );
    Ok(())
}

#[tokio::test]
async fn empty_text_deltas_cannot_bypass_mailbox_capacity() -> Result<(), Box<dyn Error>> {
    let (mut stream, sink) = ProviderEventStream::make(3, 8)?;
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta(String::new())),
        ProviderMailboxSendResult::Accepted,
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta(String::new())),
        ProviderMailboxSendResult::Accepted,
    );
    assert_eq!(
        sink.send(ProviderTurnEvent::TextDelta(String::new())),
        ProviderMailboxSendResult::Overflow,
    );
    assert!(sink.finish(ProviderTerminal::Cancelled));
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::TextDelta(String::new()))
    );
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::TextDelta(String::new()))
    );
    assert_eq!(
        stream.next().await,
        Some(ProviderTurnEvent::Terminal(ProviderTerminal::Cancelled)),
    );
    Ok(())
}

#[tokio::test]
async fn producer_drop_is_an_explicit_terminal_failure() -> Result<(), Box<dyn Error>> {
    let (mut stream, sink) = ProviderEventStream::with_defaults();
    drop(sink);
    let event = stream.next().await;
    assert!(matches!(
        event,
        Some(ProviderTurnEvent::Terminal(ProviderTerminal::Failed(_)))
    ));
    Ok(())
}

#[tokio::test]
async fn account_stream_reserves_terminal_slot() -> Result<(), Box<dyn Error>> {
    let (mut stream, sink) = ProviderAccountEventStream::make(2)?;
    assert!(sink.send(ProviderAccountPublicEvent::Staging));
    assert!(!sink.send(ProviderAccountPublicEvent::Verifying));
    assert!(
        sink.send(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
            ProviderFailureCode::AuthenticationFailed,
            "failed",
        )))
    );
    assert_eq!(
        stream.next().await,
        Some(ProviderAccountPublicEvent::Staging)
    );
    assert!(matches!(
        stream.next().await,
        Some(ProviderAccountPublicEvent::Failed(_))
    ));
    assert_eq!(stream.next().await, None);
    Ok(())
}

#[test]
fn model_catalog_is_sorted_and_rejects_duplicates() -> Result<(), Box<dyn Error>> {
    let a = ProviderModelDescriptor::new(
        ProviderModelId::new("a")?,
        Some("A".to_owned()),
        ProviderCapabilities::default(),
        Some(1_000),
        Some(100),
    )?;
    let b = ProviderModelDescriptor::new(
        ProviderModelId::new("b")?,
        Some("B".to_owned()),
        ProviderCapabilities::default(),
        Some(2_000),
        Some(200),
    )?;
    let catalog = ProviderModelCatalogResult::new(
        vec![b.clone(), a.clone()],
        ProviderInstant::from_unix_milliseconds(1),
    )?;
    assert_eq!(catalog.models()[0].id().as_str(), "a");
    assert!(
        ProviderModelCatalogResult::new(
            vec![a.clone(), a],
            ProviderInstant::from_unix_milliseconds(1),
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn system_clock_rejects_unrepresentable_sleep_duration() {
    let clock = SystemProviderClock;
    let failure = clock.sleep(u64::MAX).await;
    assert!(matches!(failure, Err(value) if value.code() == ProviderFailureCode::InvalidRequest));
}
