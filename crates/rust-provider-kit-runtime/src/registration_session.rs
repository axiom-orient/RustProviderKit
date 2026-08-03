use std::collections::VecDeque;
use std::sync::{Arc, Weak};

use rust_provider_kit_core::{
    ProviderAccountEffect, ProviderAccountEvent, ProviderAccountEventSink, ProviderAccountPhase,
    ProviderAccountPublicEvent, ProviderAccountReducer, ProviderAccountRegistrationRequest,
    ProviderAccountState, ProviderClock, ProviderCredentialLease, ProviderCredentialRecord,
    ProviderCredentialStore, ProviderFailure, ProviderFailureCode,
};
use uuid::Uuid;

use crate::account_supervisor::{
    AccountSupervisorInner, RegistrationControl, record_inspection, release_registration,
};
use crate::credential_contract::ProviderCredentialContract;
use crate::http_transport::ProviderHttpTransport;
use crate::registry::BuiltInProviderRegistry;

pub(crate) struct RegistrationSessionResources {
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
}

impl RegistrationSessionResources {
    pub(crate) fn new(
        registry: BuiltInProviderRegistry,
        vault: Arc<dyn ProviderCredentialStore>,
        transport: Arc<dyn ProviderHttpTransport>,
        clock: Arc<dyn ProviderClock>,
    ) -> Self {
        Self {
            registry,
            vault,
            transport,
            clock,
        }
    }
}

pub(crate) struct RegistrationSession {
    registration_id: Uuid,
    request: ProviderAccountRegistrationRequest,
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
    sink: ProviderAccountEventSink,
    control: Arc<RegistrationControl>,
    supervisor: Weak<AccountSupervisorInner>,
    state: ProviderAccountState,
    activation_committed: bool,
    account_released: bool,
}

#[derive(Default)]
struct EffectProgress {
    terminal_sent: bool,
    stop_effects: bool,
}

fn should_interrupt_for_cancellation_after(
    phase: &ProviderAccountPhase,
    event: &ProviderAccountEvent,
    cancellation_requested: bool,
    activation_committed: bool,
) -> bool {
    if activation_committed || !cancellation_requested {
        return false;
    }
    matches!(
        event,
        ProviderAccountEvent::RegistrationRequested(_)
            | ProviderAccountEvent::CredentialStaged(_)
            | ProviderAccountEvent::VerificationSucceeded(_)
    ) && matches!(
        phase,
        ProviderAccountPhase::StagingCredential(_)
            | ProviderAccountPhase::VerifyingAccount(_, _)
            | ProviderAccountPhase::ActivatingCredential(_, _, _)
    )
}

impl RegistrationSession {
    pub(crate) fn new(
        registration_id: Uuid,
        request: ProviderAccountRegistrationRequest,
        resources: RegistrationSessionResources,
        sink: ProviderAccountEventSink,
        control: Arc<RegistrationControl>,
        supervisor: Weak<AccountSupervisorInner>,
    ) -> Self {
        Self {
            registration_id,
            request,
            registry: resources.registry,
            vault: resources.vault,
            transport: resources.transport,
            clock: resources.clock,
            sink,
            control,
            supervisor,
            state: ProviderAccountState::default(),
            activation_committed: false,
            account_released: false,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut events = VecDeque::from([ProviderAccountEvent::RegistrationRequested(
            self.request.clone(),
        )]);
        while let Some(event) = events.pop_front() {
            let transition = ProviderAccountReducer::reduce(self.state.clone(), event.clone());
            let (next_state, effects) = match transition {
                Ok(value) => value,
                Err(error) => {
                    self.publish_terminal_failure(crate::wire::core_error_failure(error));
                    break;
                }
            };
            self.state = next_state;

            // Match the Swift session's commit boundary: after a state-producing
            // event has been reduced, cancellation is checked before any public
            // event or side effect from that transition is executed. This keeps a
            // non-cooperative stage/verify result available for compensation while
            // preventing transient `.verifying`/`.activating` publication after
            // cancellation has already won.
            if should_interrupt_for_cancellation_after(
                self.state.phase(),
                &event,
                self.control.cancellation.is_cancelled(),
                self.activation_committed,
            ) {
                events.push_front(ProviderAccountEvent::CancellationRequested);
                continue;
            }

            let mut terminal_sent = false;
            for effect in effects {
                let progress = self.apply_effect(effect, &mut events).await;
                terminal_sent |= progress.terminal_sent;
                if progress.stop_effects {
                    break;
                }
            }
            if terminal_sent {
                break;
            }
        }

        self.release_account(true);
        self.control.mark_finished();
    }

    async fn apply_effect(
        &mut self,
        effect: ProviderAccountEffect,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) -> EffectProgress {
        match effect {
            ProviderAccountEffect::Publish(public_event) => self.publish(public_event, events),
            ProviderAccountEffect::StageCredential(request, generation) => {
                self.stage_credential(request, generation, events).await;
                EffectProgress::default()
            }
            ProviderAccountEffect::VerifyCredential(request, record, generation) => {
                self.verify_credential(request, record, generation, events)
                    .await;
                EffectProgress::default()
            }
            ProviderAccountEffect::ActivateCredential(record, generation) => {
                self.activate_credential(record, generation, events).await;
                EffectProgress::default()
            }
            ProviderAccountEffect::RemoveStagedCredential(record, generation) => {
                self.remove_staged_credential(record, generation, events)
                    .await;
                EffectProgress::default()
            }
        }
    }

    fn publish(
        &mut self,
        public_event: ProviderAccountPublicEvent,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) -> EffectProgress {
        let terminal = public_event.is_terminal();
        if terminal {
            self.release_account(false);
        }
        if self.sink.send(public_event) {
            return EffectProgress {
                terminal_sent: terminal,
                stop_effects: false,
            };
        }
        if terminal {
            EffectProgress {
                terminal_sent: true,
                stop_effects: true,
            }
        } else {
            // A slow consumer is an operation failure, not a reason to bypass
            // credential compensation. Re-enter the reducer so a staged record
            // is removed before the terminal is published.
            events.push_front(ProviderAccountEvent::OperationFailed(ProviderFailure::new(
                ProviderFailureCode::ConsumerBackpressureExceeded,
                "provider account event consumer exceeded its backlog",
            )));
            EffectProgress {
                terminal_sent: false,
                stop_effects: true,
            }
        }
    }

    async fn stage_credential(
        &self,
        request: ProviderAccountRegistrationRequest,
        generation: u64,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) {
        if !self.is_current_generation(generation) {
            return;
        }
        if self.queue_cancellation(events) {
            return;
        }
        let event = match self.clock.now().await {
            Ok(now) => match self.vault.stage(&request, now).await {
                Ok(record) => ProviderAccountEvent::CredentialStaged(record),
                Err(failure) => ProviderAccountEvent::OperationFailed(failure),
            },
            Err(failure) => ProviderAccountEvent::OperationFailed(failure),
        };
        events.push_back(event);
    }

    async fn verify_credential(
        &self,
        request: ProviderAccountRegistrationRequest,
        record: ProviderCredentialRecord,
        generation: u64,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) {
        if !self.is_current_generation(generation) {
            return;
        }
        if self.queue_cancellation(events) {
            return;
        }
        let result =
            match ProviderCredentialLease::for_verification(record, request.credential().clone())
                .map_err(crate::wire::core_error_failure)
            {
                Ok(lease) => match self.registry.adapter(request.provider_id()) {
                    Ok(adapter) => match self.clock.now().await {
                        Ok(now) => adapter.inspect(&lease, self.transport.as_ref(), now).await,
                        Err(failure) => Err(failure),
                    },
                    Err(failure) => Err(failure),
                },
                Err(failure) => Err(failure),
            };
        events.push_back(match result {
            Ok(inspection) => ProviderAccountEvent::VerificationSucceeded(inspection),
            Err(failure) => ProviderAccountEvent::OperationFailed(failure),
        });
    }

    async fn activate_credential(
        &mut self,
        record: ProviderCredentialRecord,
        generation: u64,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) {
        if !self.is_current_generation(generation) {
            return;
        }
        if self.queue_cancellation(events) {
            return;
        }
        let activation = match self.clock.now().await {
            Ok(now) => self.vault.activate(&record, now).await,
            Err(failure) => Err(failure),
        };
        if let Err(failure) = activation {
            events.push_back(ProviderAccountEvent::OperationFailed(failure));
            return;
        }
        if self.queue_cancellation(events) {
            return;
        }

        let Ok(lease) = self.vault.lease(record.account_id()).await else {
            events.push_back(ProviderAccountEvent::OperationFailed(ProviderFailure::new(
                ProviderFailureCode::CredentialRecoveryRequired,
                "credential activation could not be read back",
            )));
            return;
        };
        if let Err(failure) =
            ProviderCredentialContract::validate_active_from_staged(&record, lease.record())
        {
            events.push_back(ProviderAccountEvent::OperationFailed(failure));
            return;
        }
        if lease.material().source() != record.source() {
            events.push_back(ProviderAccountEvent::OperationFailed(ProviderFailure::new(
                ProviderFailureCode::CredentialRecoveryRequired,
                "activated credential material source changed",
            )));
            return;
        }
        if self.queue_cancellation(events) {
            return;
        }

        let inspection = match self.state.phase() {
            ProviderAccountPhase::ActivatingCredential(_, _, inspection) => inspection.clone(),
            _ => {
                events.push_back(ProviderAccountEvent::OperationFailed(ProviderFailure::new(
                    ProviderFailureCode::InternalInvariant,
                    "provider registration lost its verified inspection",
                )));
                return;
            }
        };
        record_inspection(&self.supervisor, inspection);
        self.activation_committed = true;
        events.push_back(ProviderAccountEvent::ActivationSucceeded);
    }

    async fn remove_staged_credential(
        &self,
        record: ProviderCredentialRecord,
        generation: u64,
        events: &mut VecDeque<ProviderAccountEvent>,
    ) {
        if !self.is_current_generation(generation) {
            return;
        }
        events.push_back(match self.vault.remove(&record).await {
            Ok(()) => ProviderAccountEvent::CompensationSucceeded,
            Err(failure) => ProviderAccountEvent::CompensationFailed(failure),
        });
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        generation == self.state.generation()
    }

    fn queue_cancellation(&self, events: &mut VecDeque<ProviderAccountEvent>) -> bool {
        if self.control.cancellation.is_cancelled() {
            events.push_back(ProviderAccountEvent::CancellationRequested);
            true
        } else {
            false
        }
    }

    fn publish_terminal_failure(&mut self, failure: ProviderFailure) {
        self.release_account(false);
        let _ = self.sink.send(ProviderAccountPublicEvent::Failed(failure));
    }

    fn release_account(&mut self, remove_session: bool) {
        if !self.account_released {
            release_registration(
                &self.supervisor,
                self.request.account_id(),
                self.registration_id,
                false,
            );
            self.account_released = true;
        }
        if remove_session {
            release_registration(
                &self.supervisor,
                self.request.account_id(),
                self.registration_id,
                true,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rust_provider_kit_core::{
        BuiltInProviderId, ProviderAccountEvent, ProviderAccountId, ProviderAccountInspection,
        ProviderAccountReadiness, ProviderAccountReducer, ProviderAccountRegistrationRequest,
        ProviderAccountState, ProviderCapabilities, ProviderCredentialMaterial,
        ProviderCredentialRecord, ProviderCredentialRecordState, ProviderCredentialReference,
        ProviderInstant,
    };

    use super::should_interrupt_for_cancellation_after;

    fn request() -> Result<ProviderAccountRegistrationRequest, Box<dyn Error>> {
        Ok(ProviderAccountRegistrationRequest::new(
            ProviderAccountId::new("cancel-stage-account")?,
            BuiltInProviderId::open_router(),
            "Primary",
            ProviderCredentialMaterial::api_key("test-secret")?,
            None,
        )?)
    }

    #[test]
    fn cancellation_checkpoint_runs_after_state_producing_events_before_effects()
    -> Result<(), Box<dyn Error>> {
        let request = request()?;
        let requested_event = ProviderAccountEvent::RegistrationRequested(request.clone());
        let (state, _) = ProviderAccountReducer::reduce(
            ProviderAccountState::default(),
            requested_event.clone(),
        )?;
        assert!(should_interrupt_for_cancellation_after(
            state.phase(),
            &requested_event,
            true,
            false,
        ));

        let record = ProviderCredentialRecord::new(
            ProviderCredentialReference::new("cancel-stage-record")?,
            request.account_id().clone(),
            request.provider_id().clone(),
            request.label(),
            request.credential().source(),
            ProviderCredentialRecordState::Staged,
            None,
            ProviderInstant::from_unix_milliseconds(1),
            ProviderInstant::from_unix_milliseconds(1),
        )?;
        let staged_event = ProviderAccountEvent::CredentialStaged(record);
        let (state, _) = ProviderAccountReducer::reduce(state, staged_event.clone())?;
        assert!(should_interrupt_for_cancellation_after(
            state.phase(),
            &staged_event,
            true,
            false,
        ));

        let inspection = ProviderAccountInspection::new(
            request.account_id().clone(),
            request.provider_id().clone(),
            ProviderAccountReadiness::Ready,
            None,
            ProviderCapabilities::default(),
            ProviderInstant::from_unix_milliseconds(2),
        )?;
        let verified_event = ProviderAccountEvent::VerificationSucceeded(inspection);
        let (state, _) = ProviderAccountReducer::reduce(state, verified_event.clone())?;
        assert!(should_interrupt_for_cancellation_after(
            state.phase(),
            &verified_event,
            true,
            false,
        ));
        assert!(!should_interrupt_for_cancellation_after(
            state.phase(),
            &verified_event,
            true,
            true,
        ));
        Ok(())
    }
}
