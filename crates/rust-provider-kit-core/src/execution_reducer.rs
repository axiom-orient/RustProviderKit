use crate::{
    ProviderCompletion, ProviderCoreError, ProviderCoreErrorCode, ProviderFailure,
    ProviderFailureCode, ProviderResponseMetadata, ProviderTerminal, ProviderToolCall,
    ProviderTurnEvent, ProviderTurnRequest,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderExecutionState {
    generation: u64,
    has_published_started: bool,
    phase: ProviderExecutionPhase,
}
impl ProviderExecutionState {
    #[must_use]
    pub fn new(
        generation: u64,
        has_published_started: bool,
        phase: ProviderExecutionPhase,
    ) -> Self {
        Self {
            generation,
            has_published_started,
            phase,
        }
    }
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn has_published_started(&self) -> bool {
        self.has_published_started
    }
    #[must_use]
    pub fn phase(&self) -> &ProviderExecutionPhase {
        &self.phase
    }
}
impl Default for ProviderExecutionState {
    fn default() -> Self {
        Self::new(0, false, ProviderExecutionPhase::Idle)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderExecutionPhase {
    Idle,
    Opening(ProviderTurnRequest),
    Streaming(ProviderTurnRequest, ProviderResponseMetadata, bool),
    Terminating(ProviderTurnRequest, ProviderTerminal, bool),
    Terminal(ProviderTerminal),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderExecutionEvent {
    RequestAdmitted(ProviderTurnRequest),
    TransportOpened(ProviderResponseMetadata),
    RetryRequested,
    ReasoningDeltaReceived(String),
    TextDeltaReceived(String),
    ToolCallCompleted(ProviderToolCall),
    TransportCompleted(ProviderCompletion),
    TransportFailed(ProviderFailure),
    CancelRequested,
    TimeoutExpired,
    MailboxOverflowed,
    CleanupCompleted,
}
#[derive(Debug, Clone, PartialEq)]
// A publish effect is consumed immediately by the execution session. Its event
// payload is bounded at the public contract, so keeping this internal reducer
// value inline avoids an allocation per streamed event.
#[allow(clippy::large_enum_variant)]
pub enum ProviderExecutionEffect {
    OpenTransport(ProviderTurnRequest, u64),
    Publish(ProviderTurnEvent),
    BeginCleanup(bool, u64),
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderExecutionReducer;
impl ProviderExecutionReducer {
    pub fn reduce(
        state: ProviderExecutionState,
        event: ProviderExecutionEvent,
    ) -> Result<(ProviderExecutionState, Vec<ProviderExecutionEffect>), ProviderCoreError> {
        let generation = state.generation;
        let started = state.has_published_started;
        match (state.phase, event) {
            (ProviderExecutionPhase::Idle, ProviderExecutionEvent::RequestAdmitted(request)) => {
                let next = generation.checked_add(1).ok_or_else(generation_exhausted)?;
                Ok((
                    ProviderExecutionState::new(
                        next,
                        false,
                        ProviderExecutionPhase::Opening(request.clone()),
                    ),
                    vec![ProviderExecutionEffect::OpenTransport(request, next)],
                ))
            }
            (
                ProviderExecutionPhase::Opening(request),
                ProviderExecutionEvent::TransportOpened(metadata),
            ) => {
                if metadata.request_id() != request.id()
                    || metadata.provider_id() != request.selection().provider_id()
                    || metadata.model_id() != request.selection().model_id()
                {
                    return Err(ProviderCoreError::invalid_transition(
                        "transport metadata does not match the admitted request",
                    ));
                }
                let effects = if started {
                    Vec::new()
                } else {
                    vec![ProviderExecutionEffect::Publish(
                        ProviderTurnEvent::Started(metadata.clone()),
                    )]
                };
                Ok((
                    ProviderExecutionState::new(
                        generation,
                        true,
                        ProviderExecutionPhase::Streaming(request, metadata, false),
                    ),
                    effects,
                ))
            }
            (
                ProviderExecutionPhase::Streaming(request, metadata, _),
                ProviderExecutionEvent::ReasoningDeltaReceived(delta),
            ) => {
                validate_delta(&delta, "reasoning")?;
                Ok((
                    ProviderExecutionState::new(
                        generation,
                        started,
                        ProviderExecutionPhase::Streaming(request, metadata, true),
                    ),
                    vec![ProviderExecutionEffect::Publish(
                        ProviderTurnEvent::ReasoningDelta(delta),
                    )],
                ))
            }
            (
                ProviderExecutionPhase::Streaming(request, metadata, _),
                ProviderExecutionEvent::TextDeltaReceived(delta),
            ) => {
                validate_delta(&delta, "text")?;
                Ok((
                    ProviderExecutionState::new(
                        generation,
                        started,
                        ProviderExecutionPhase::Streaming(request, metadata, true),
                    ),
                    vec![ProviderExecutionEffect::Publish(
                        ProviderTurnEvent::TextDelta(delta),
                    )],
                ))
            }
            (
                ProviderExecutionPhase::Streaming(request, metadata, _),
                ProviderExecutionEvent::ToolCallCompleted(call),
            ) => Ok((
                ProviderExecutionState::new(
                    generation,
                    started,
                    ProviderExecutionPhase::Streaming(request, metadata, true),
                ),
                vec![ProviderExecutionEffect::Publish(
                    ProviderTurnEvent::ToolCall(call),
                )],
            )),
            (
                ProviderExecutionPhase::Streaming(request, metadata, _),
                ProviderExecutionEvent::TransportCompleted(completion),
            ) => {
                let mut effects = Vec::new();
                if !started {
                    effects.push(ProviderExecutionEffect::Publish(
                        ProviderTurnEvent::Started(metadata),
                    ));
                }
                effects.push(ProviderExecutionEffect::BeginCleanup(false, generation));
                let terminal = ProviderTerminal::Completed(completion);
                Ok((
                    ProviderExecutionState::new(
                        generation,
                        true,
                        ProviderExecutionPhase::Terminating(request, terminal, false),
                    ),
                    effects,
                ))
            }
            (
                ProviderExecutionPhase::Streaming(request, _, false),
                ProviderExecutionEvent::RetryRequested,
            ) => Ok((
                ProviderExecutionState::new(
                    generation,
                    started,
                    ProviderExecutionPhase::Opening(request),
                ),
                Vec::new(),
            )),
            (
                ProviderExecutionPhase::Opening(request),
                ProviderExecutionEvent::TransportFailed(failure),
            )
            | (
                ProviderExecutionPhase::Streaming(request, _, _),
                ProviderExecutionEvent::TransportFailed(failure),
            ) => terminating(
                generation,
                started,
                request,
                ProviderTerminal::Failed(failure),
                true,
            ),
            (ProviderExecutionPhase::Opening(request), ProviderExecutionEvent::CancelRequested)
            | (
                ProviderExecutionPhase::Streaming(request, _, _),
                ProviderExecutionEvent::CancelRequested,
            ) => terminating(
                generation,
                started,
                request,
                ProviderTerminal::Cancelled,
                true,
            ),
            (ProviderExecutionPhase::Opening(request), ProviderExecutionEvent::TimeoutExpired)
            | (
                ProviderExecutionPhase::Streaming(request, _, _),
                ProviderExecutionEvent::TimeoutExpired,
            ) => {
                let failure = ProviderFailure::new(
                    ProviderFailureCode::TimedOut,
                    "provider request timed out",
                )
                .with_request_id(request.id().clone());
                terminating(
                    generation,
                    started,
                    request,
                    ProviderTerminal::Failed(failure),
                    true,
                )
            }
            (
                ProviderExecutionPhase::Opening(request),
                ProviderExecutionEvent::MailboxOverflowed,
            )
            | (
                ProviderExecutionPhase::Streaming(request, _, _),
                ProviderExecutionEvent::MailboxOverflowed,
            ) => {
                let failure = ProviderFailure::new(
                    ProviderFailureCode::ConsumerBackpressureExceeded,
                    "provider event consumer exceeded the bounded backlog",
                )
                .with_request_id(request.id().clone());
                terminating(
                    generation,
                    started,
                    request,
                    ProviderTerminal::Failed(failure),
                    true,
                )
            }
            (
                ProviderExecutionPhase::Terminating(_, terminal, _),
                ProviderExecutionEvent::CleanupCompleted,
            ) => Ok((
                ProviderExecutionState::new(
                    generation,
                    started,
                    ProviderExecutionPhase::Terminal(terminal.clone()),
                ),
                vec![ProviderExecutionEffect::Publish(
                    ProviderTurnEvent::Terminal(terminal),
                )],
            )),
            (
                phase @ (ProviderExecutionPhase::Terminating(_, _, _)
                | ProviderExecutionPhase::Terminal(_)),
                ProviderExecutionEvent::CancelRequested,
            ) => Ok((
                ProviderExecutionState::new(generation, started, phase),
                Vec::new(),
            )),
            (phase @ ProviderExecutionPhase::Terminal(_), _) => Ok((
                ProviderExecutionState::new(generation, started, phase),
                Vec::new(),
            )),
            (phase, _) => Err(ProviderCoreError::invalid_transition(format!(
                "provider execution event is invalid for phase {phase:?}"
            ))),
        }
    }
}
fn terminating(
    generation: u64,
    started: bool,
    request: ProviderTurnRequest,
    terminal: ProviderTerminal,
    cancel: bool,
) -> Result<(ProviderExecutionState, Vec<ProviderExecutionEffect>), ProviderCoreError> {
    Ok((
        ProviderExecutionState::new(
            generation,
            started,
            ProviderExecutionPhase::Terminating(request, terminal, cancel),
        ),
        vec![ProviderExecutionEffect::BeginCleanup(cancel, generation)],
    ))
}

fn validate_delta(delta: &str, kind: &str) -> Result<(), ProviderCoreError> {
    if delta.is_empty() || delta.chars().count() > 1_048_576 || delta.contains('\0') {
        return Err(ProviderCoreError::invalid_transition(format!(
            "provider {kind} delta is empty or invalid"
        )));
    }
    Ok(())
}

fn generation_exhausted() -> ProviderCoreError {
    ProviderCoreError::new(
        ProviderCoreErrorCode::GenerationExhausted,
        "provider execution generation exhausted",
    )
}
