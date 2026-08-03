use crate::{
    ProviderAccountInspection, ProviderAccountReadiness, ProviderAccountRegistrationRequest,
    ProviderAccountSummary, ProviderCoreError, ProviderCoreErrorCode, ProviderCredentialRecord,
    ProviderCredentialRecordState, ProviderFailure, ProviderFailureCode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountState {
    generation: u64,
    phase: ProviderAccountPhase,
}
impl ProviderAccountState {
    #[must_use]
    pub fn new(generation: u64, phase: ProviderAccountPhase) -> Self {
        Self { generation, phase }
    }
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn phase(&self) -> &ProviderAccountPhase {
        &self.phase
    }
}
impl Default for ProviderAccountState {
    fn default() -> Self {
        Self {
            generation: 0,
            phase: ProviderAccountPhase::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAccountPhase {
    Idle,
    StagingCredential(ProviderAccountRegistrationRequest),
    VerifyingAccount(ProviderAccountRegistrationRequest, ProviderCredentialRecord),
    ActivatingCredential(
        ProviderAccountRegistrationRequest,
        ProviderCredentialRecord,
        ProviderAccountInspection,
    ),
    Ready(ProviderAccountSummary),
    Compensating(
        ProviderAccountRegistrationRequest,
        ProviderCredentialRecord,
        ProviderFailure,
    ),
    Failed(ProviderFailure),
    RecoveryRequired(ProviderCredentialRecord, ProviderFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAccountEvent {
    RegistrationRequested(ProviderAccountRegistrationRequest),
    CredentialStaged(ProviderCredentialRecord),
    VerificationSucceeded(ProviderAccountInspection),
    ActivationSucceeded,
    CancellationRequested,
    OperationFailed(ProviderFailure),
    CompensationSucceeded,
    CompensationFailed(ProviderFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAccountPublicEvent {
    Staging,
    Verifying,
    Activating,
    Ready(ProviderAccountSummary),
    Failed(ProviderFailure),
    RecoveryRequired(ProviderFailure),
}
impl ProviderAccountPublicEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Ready(_) | Self::Failed(_) | Self::RecoveryRequired(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
// Effects are short-lived reducer values and all embedded request/record fields
// are independently bounded. Boxing the occasional verification effect would
// allocate on every account transition without shrinking any retained state.
#[allow(clippy::large_enum_variant)]
pub enum ProviderAccountEffect {
    StageCredential(ProviderAccountRegistrationRequest, u64),
    VerifyCredential(
        ProviderAccountRegistrationRequest,
        ProviderCredentialRecord,
        u64,
    ),
    ActivateCredential(ProviderCredentialRecord, u64),
    RemoveStagedCredential(ProviderCredentialRecord, u64),
    Publish(ProviderAccountPublicEvent),
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderAccountReducer;
impl ProviderAccountReducer {
    pub fn reduce(
        state: ProviderAccountState,
        event: ProviderAccountEvent,
    ) -> Result<(ProviderAccountState, Vec<ProviderAccountEffect>), ProviderCoreError> {
        let generation = state.generation;
        match (state.phase, event) {
            (
                ProviderAccountPhase::Idle | ProviderAccountPhase::Failed(_),
                ProviderAccountEvent::RegistrationRequested(request),
            ) => {
                let next = generation.checked_add(1).ok_or_else(generation_exhausted)?;
                Ok((
                    ProviderAccountState::new(
                        next,
                        ProviderAccountPhase::StagingCredential(request.clone()),
                    ),
                    vec![
                        ProviderAccountEffect::Publish(ProviderAccountPublicEvent::Staging),
                        ProviderAccountEffect::StageCredential(request, next),
                    ],
                ))
            }
            (
                ProviderAccountPhase::StagingCredential(request),
                ProviderAccountEvent::CredentialStaged(record),
            ) => {
                let consistent = record.account_id() == request.account_id()
                    && record.provider_id() == request.provider_id()
                    && record.label() == request.label()
                    && record.source() == request.credential().source()
                    && record.state() == ProviderCredentialRecordState::Staged
                    && record.endpoint() == request.endpoint();
                if !consistent {
                    let failure = ProviderFailure::new(
                        ProviderFailureCode::InternalInvariant,
                        "credential vault returned an inconsistent staged record",
                    );
                    return Ok((
                        ProviderAccountState::new(
                            generation,
                            ProviderAccountPhase::Compensating(request, record.clone(), failure),
                        ),
                        vec![ProviderAccountEffect::RemoveStagedCredential(
                            record, generation,
                        )],
                    ));
                }
                Ok((
                    ProviderAccountState::new(
                        generation,
                        ProviderAccountPhase::VerifyingAccount(request.clone(), record.clone()),
                    ),
                    vec![
                        ProviderAccountEffect::Publish(ProviderAccountPublicEvent::Verifying),
                        ProviderAccountEffect::VerifyCredential(request, record, generation),
                    ],
                ))
            }
            (
                ProviderAccountPhase::VerifyingAccount(request, record),
                ProviderAccountEvent::VerificationSucceeded(inspection),
            ) => {
                if inspection.account_id() != request.account_id()
                    || inspection.provider_id() != request.provider_id()
                    || inspection.readiness() != ProviderAccountReadiness::Ready
                {
                    return Err(ProviderCoreError::invalid_transition(
                        "account verification did not produce a ready inspection",
                    ));
                }
                Ok((
                    ProviderAccountState::new(
                        generation,
                        ProviderAccountPhase::ActivatingCredential(
                            request.clone(),
                            record.clone(),
                            inspection,
                        ),
                    ),
                    vec![
                        ProviderAccountEffect::Publish(ProviderAccountPublicEvent::Activating),
                        ProviderAccountEffect::ActivateCredential(record, generation),
                    ],
                ))
            }
            (
                ProviderAccountPhase::ActivatingCredential(request, record, inspection),
                ProviderAccountEvent::ActivationSucceeded,
            ) => {
                let summary = ProviderAccountSummary::new(
                    request.account_id().clone(),
                    request.provider_id().clone(),
                    request.label(),
                    record.source(),
                    ProviderAccountReadiness::Ready,
                    request.endpoint().cloned(),
                    Some(inspection.inspected_at()),
                )?;
                Ok((
                    ProviderAccountState::new(
                        generation,
                        ProviderAccountPhase::Ready(summary.clone()),
                    ),
                    vec![ProviderAccountEffect::Publish(
                        ProviderAccountPublicEvent::Ready(summary),
                    )],
                ))
            }
            (
                ProviderAccountPhase::StagingCredential(_),
                ProviderAccountEvent::OperationFailed(failure),
            ) => Ok((
                ProviderAccountState::new(
                    generation,
                    ProviderAccountPhase::Failed(failure.clone()),
                ),
                vec![ProviderAccountEffect::Publish(
                    ProviderAccountPublicEvent::Failed(failure),
                )],
            )),
            (
                ProviderAccountPhase::VerifyingAccount(request, record),
                ProviderAccountEvent::OperationFailed(failure),
            )
            | (
                ProviderAccountPhase::ActivatingCredential(request, record, _),
                ProviderAccountEvent::OperationFailed(failure),
            ) => Ok((
                ProviderAccountState::new(
                    generation,
                    ProviderAccountPhase::Compensating(request, record.clone(), failure),
                ),
                vec![ProviderAccountEffect::RemoveStagedCredential(
                    record, generation,
                )],
            )),
            (
                ProviderAccountPhase::VerifyingAccount(request, record),
                ProviderAccountEvent::CancellationRequested,
            )
            | (
                ProviderAccountPhase::ActivatingCredential(request, record, _),
                ProviderAccountEvent::CancellationRequested,
            ) => {
                let failure = ProviderFailure::new(
                    ProviderFailureCode::Cancelled,
                    "account registration cancelled",
                );
                Ok((
                    ProviderAccountState::new(
                        generation,
                        ProviderAccountPhase::Compensating(request, record.clone(), failure),
                    ),
                    vec![ProviderAccountEffect::RemoveStagedCredential(
                        record, generation,
                    )],
                ))
            }
            (
                ProviderAccountPhase::StagingCredential(_),
                ProviderAccountEvent::CancellationRequested,
            ) => {
                let failure = ProviderFailure::new(
                    ProviderFailureCode::Cancelled,
                    "account registration cancelled",
                );
                Ok((
                    ProviderAccountState::new(
                        generation,
                        ProviderAccountPhase::Failed(failure.clone()),
                    ),
                    vec![ProviderAccountEffect::Publish(
                        ProviderAccountPublicEvent::Failed(failure),
                    )],
                ))
            }
            (
                ProviderAccountPhase::Compensating(_, _, original),
                ProviderAccountEvent::CompensationSucceeded,
            ) => Ok((
                ProviderAccountState::new(
                    generation,
                    ProviderAccountPhase::Failed(original.clone()),
                ),
                vec![ProviderAccountEffect::Publish(
                    ProviderAccountPublicEvent::Failed(original),
                )],
            )),
            (
                ProviderAccountPhase::Compensating(_, record, _),
                ProviderAccountEvent::CompensationFailed(failure),
            ) => Ok((
                ProviderAccountState::new(
                    generation,
                    ProviderAccountPhase::RecoveryRequired(record, failure.clone()),
                ),
                vec![ProviderAccountEffect::Publish(
                    ProviderAccountPublicEvent::RecoveryRequired(failure),
                )],
            )),
            (
                phase @ ProviderAccountPhase::Ready(_),
                ProviderAccountEvent::CancellationRequested,
            )
            | (
                phase @ ProviderAccountPhase::Failed(_),
                ProviderAccountEvent::CancellationRequested,
            ) => Ok((ProviderAccountState::new(generation, phase), Vec::new())),
            (phase, _) => Err(ProviderCoreError::invalid_transition(format!(
                "provider account event is invalid for phase {phase:?}"
            ))),
        }
    }
}

fn generation_exhausted() -> ProviderCoreError {
    ProviderCoreError::new(
        ProviderCoreErrorCode::GenerationExhausted,
        "provider account generation exhausted",
    )
}
