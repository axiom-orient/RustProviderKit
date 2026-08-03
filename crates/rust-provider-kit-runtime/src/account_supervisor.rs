use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use rust_provider_kit_core::{
    ProviderAccountEventStream, ProviderAccountId, ProviderAccountInspection,
    ProviderAccountPublicEvent, ProviderAccountReadiness, ProviderAccountRegistrationRequest,
    ProviderAccountSummary, ProviderClock, ProviderCredentialReconciliationIssue,
    ProviderCredentialReconciliationReport, ProviderCredentialRecordState, ProviderCredentialStore,
    ProviderFailure, ProviderFailureCode, ProviderModelCatalogResult,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::credential_contract::ProviderCredentialContract;
use crate::http_transport::ProviderHttpTransport;
use crate::registration_session::{RegistrationSession, RegistrationSessionResources};
use crate::registry::BuiltInProviderRegistry;

#[derive(Clone)]
pub(crate) struct ProviderAccountSupervisor {
    inner: Arc<AccountSupervisorInner>,
}

pub(crate) struct AccountSupervisorInner {
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
    state: Mutex<AccountSupervisorState>,
}

#[derive(Default)]
struct AccountSupervisorState {
    sessions: HashMap<Uuid, ActiveRegistration>,
    account_index: HashMap<ProviderAccountId, Uuid>,
    blocked_accounts: HashSet<ProviderAccountId>,
    inspections: HashMap<ProviderAccountId, ProviderAccountInspection>,
    reconciling: bool,
    shutting_down: bool,
}

struct ActiveRegistration {
    control: Arc<RegistrationControl>,
}

pub(crate) struct RegistrationControl {
    pub(crate) cancellation: CancellationToken,
    finished: watch::Sender<bool>,
}

impl RegistrationControl {
    fn new() -> Self {
        let (finished, _receiver) = watch::channel(false);
        Self {
            cancellation: CancellationToken::new(),
            finished,
        }
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn mark_finished(&self) {
        let _previous = self.finished.send_replace(true);
    }

    async fn wait_finished(&self) {
        let mut receiver = self.finished.subscribe();
        loop {
            if *receiver.borrow() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

impl ProviderAccountSupervisor {
    pub(crate) fn new(
        registry: BuiltInProviderRegistry,
        vault: Arc<dyn ProviderCredentialStore>,
        transport: Arc<dyn ProviderHttpTransport>,
        clock: Arc<dyn ProviderClock>,
    ) -> Self {
        Self {
            inner: Arc::new(AccountSupervisorInner {
                registry,
                vault,
                transport,
                clock,
                state: Mutex::new(AccountSupervisorState::default()),
            }),
        }
    }

    pub(crate) async fn reconcile_credentials(
        &self,
    ) -> Result<ProviderCredentialReconciliationReport, ProviderFailure> {
        let _reconciliation = self.begin_reconciliation()?;

        let mut records = self.inner.vault.records().await?;
        ProviderCredentialContract::validate_records(&records)?;
        records.sort_by(|left, right| {
            left.account_id()
                .cmp(right.account_id())
                .then_with(|| left.reference().cmp(right.reference()))
        });

        let mut active_record_count = 0usize;
        let mut removed = Vec::new();
        let mut issues = Vec::new();
        for record in records {
            match record.state() {
                ProviderCredentialRecordState::Active => {
                    active_record_count = active_record_count.checked_add(1).ok_or_else(|| {
                        ProviderFailure::new(
                            ProviderFailureCode::InternalInvariant,
                            "credential reconciliation count overflow",
                        )
                    })?;
                }
                ProviderCredentialRecordState::Staged => {
                    match self.inner.vault.remove(&record).await {
                        Ok(()) => {
                            removed.push(record.reference().clone());
                            self.inner
                                .state
                                .lock()
                                .inspections
                                .remove(record.account_id());
                        }
                        Err(failure) => issues.push(ProviderCredentialReconciliationIssue {
                            reference: record.reference().clone(),
                            account_id: record.account_id().clone(),
                            failure: ProviderFailure::new(
                                ProviderFailureCode::CredentialRecoveryRequired,
                                failure.message(),
                            ),
                        }),
                    }
                }
            }
        }
        Ok(ProviderCredentialReconciliationReport::new(
            active_record_count,
            removed,
            issues,
        ))
    }

    fn begin_reconciliation(&self) -> Result<CredentialReconciliation, ProviderFailure> {
        let mut state = self.inner.state.lock();
        if state.shutting_down {
            return Err(shutdown_failure());
        }
        if state.reconciling {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "credential reconciliation is already active",
            ));
        }
        if !state.sessions.is_empty() || !state.blocked_accounts.is_empty() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "credential reconciliation requires no active account mutation",
            ));
        }
        state.reconciling = true;
        Ok(CredentialReconciliation {
            supervisor: Arc::clone(&self.inner),
        })
    }

    pub(crate) async fn accounts(&self) -> Result<Vec<ProviderAccountSummary>, ProviderFailure> {
        let records = self.inner.vault.records().await?;
        ProviderCredentialContract::validate_records(&records)?;
        let mut summaries = Vec::with_capacity(records.len());
        {
            let state = self.inner.state.lock();
            for record in records {
                let inspection = state.inspections.get(record.account_id());
                let readiness = if record.state() == ProviderCredentialRecordState::Active {
                    inspection
                        .map(ProviderAccountInspection::readiness)
                        .unwrap_or(ProviderAccountReadiness::VerificationRequired)
                } else {
                    ProviderAccountReadiness::RecoveryRequired
                };
                summaries.push(
                    ProviderAccountSummary::new(
                        record.account_id().clone(),
                        record.provider_id().clone(),
                        record.label(),
                        record.source(),
                        readiness,
                        record.endpoint().cloned(),
                        inspection.map(ProviderAccountInspection::inspected_at),
                    )
                    .map_err(crate::wire::core_error_failure)?,
                );
            }
        }
        summaries.sort_by(|left, right| {
            left.provider_id()
                .cmp(right.provider_id())
                .then_with(|| left.account_id().cmp(right.account_id()))
        });
        Ok(summaries)
    }

    pub(crate) async fn register(
        &self,
        request: ProviderAccountRegistrationRequest,
    ) -> ProviderAccountEventStream {
        let (stream, sink) = ProviderAccountEventStream::with_defaults();
        let registration_id = Uuid::new_v4();
        let control = Arc::new(RegistrationControl::new());
        {
            let mut state = self.inner.state.lock();
            if state.shutting_down {
                let _ = sink.send(ProviderAccountPublicEvent::Failed(shutdown_failure()));
                return stream;
            }
            if state.reconciling {
                let _ = sink.send(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                    ProviderFailureCode::AccountUnavailable,
                    "credential reconciliation is active",
                )));
                return stream;
            }
            if state.blocked_accounts.contains(request.account_id()) {
                let _ = sink.send(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                    ProviderFailureCode::AccountUnavailable,
                    "provider account is being revoked",
                )));
                return stream;
            }
            if state.account_index.contains_key(request.account_id()) {
                let _ = sink.send(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "provider account registration is already active",
                )));
                return stream;
            }
            state
                .account_index
                .insert(request.account_id().clone(), registration_id);
            state.sessions.insert(
                registration_id,
                ActiveRegistration {
                    control: Arc::clone(&control),
                },
            );
        }

        let account_id = request.account_id().clone();
        let session = RegistrationSession::new(
            registration_id,
            request,
            RegistrationSessionResources::new(
                self.inner.registry.clone(),
                Arc::clone(&self.inner.vault),
                Arc::clone(&self.inner.transport),
                Arc::clone(&self.inner.clock),
            ),
            sink.clone(),
            Arc::clone(&control),
            Arc::downgrade(&self.inner),
        );

        let emergency_sink = sink;
        let supervisor = Arc::downgrade(&self.inner);
        let task = tokio::spawn(async move {
            session.run().await;
        });
        drop(tokio::spawn(async move {
            if task.await.is_err() {
                release_registration(&supervisor, &account_id, registration_id, true);
                let _ =
                    emergency_sink.send(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                        ProviderFailureCode::InternalInvariant,
                        "provider account registration worker terminated unexpectedly",
                    )));
                control.mark_finished();
            }
        }));
        stream
    }

    pub(crate) async fn cancel(&self, account_id: &ProviderAccountId) {
        let control = {
            let state = self.inner.state.lock();
            state
                .account_index
                .get(account_id)
                .and_then(|id| state.sessions.get(id))
                .map(|active| Arc::clone(&active.control))
        };
        if let Some(control) = control {
            control.cancel();
            control.wait_finished().await;
        }
    }

    pub(crate) async fn revoke(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<(), ProviderFailure> {
        self.cancel(account_id).await;
        let record = self.inner.vault.record(account_id).await?;
        if let Some(record) = record {
            self.inner.vault.remove(&record).await?;
        }
        self.inner.state.lock().inspections.remove(account_id);
        Ok(())
    }

    pub(crate) async fn block_account(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<(), ProviderFailure> {
        let control = {
            let mut state = self.inner.state.lock();
            if state.reconciling {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "provider account cannot be revoked during credential reconciliation",
                ));
            }
            state.blocked_accounts.insert(account_id.clone());
            state
                .account_index
                .get(account_id)
                .and_then(|registration_id| state.sessions.get(registration_id))
                .map(|active| Arc::clone(&active.control))
        };
        if let Some(control) = control {
            control.cancel();
            control.wait_finished().await;
        }
        Ok(())
    }

    pub(crate) fn unblock_account(&self, account_id: &ProviderAccountId) {
        self.inner.state.lock().blocked_accounts.remove(account_id);
    }

    pub(crate) async fn inspect(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderAccountInspection, ProviderFailure> {
        self.ensure_account_available(account_id)?;
        let lease = self.inner.vault.lease(account_id).await?;
        ProviderCredentialContract::validate_lease_account(&lease, account_id)?;
        let adapter = self.inner.registry.adapter(lease.record().provider_id())?;
        let now = self.inner.clock.now().await?;
        let inspection = adapter
            .inspect(&lease, self.inner.transport.as_ref(), now)
            .await?;
        if inspection.account_id() != account_id
            || inspection.provider_id() != lease.record().provider_id()
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "provider inspection identity does not match the credential lease",
            ));
        }
        self.inner
            .state
            .lock()
            .inspections
            .insert(account_id.clone(), inspection.clone());
        Ok(inspection)
    }

    pub(crate) async fn models(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderModelCatalogResult, ProviderFailure> {
        self.ensure_account_available(account_id)?;
        let lease = self.inner.vault.lease(account_id).await?;
        ProviderCredentialContract::validate_lease_account(&lease, account_id)?;
        let adapter = self.inner.registry.adapter(lease.record().provider_id())?;
        let now = self.inner.clock.now().await?;
        adapter
            .models(&lease, self.inner.transport.as_ref(), now)
            .await
    }

    fn ensure_account_available(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<(), ProviderFailure> {
        let state = self.inner.state.lock();
        if state.shutting_down {
            return Err(shutdown_failure());
        }
        if state.blocked_accounts.contains(account_id) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is being revoked",
            ));
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let controls = {
            let mut state = self.inner.state.lock();
            state.shutting_down = true;
            state
                .sessions
                .values()
                .map(|active| Arc::clone(&active.control))
                .collect::<Vec<_>>()
        };
        for control in &controls {
            control.cancel();
        }
        for control in controls {
            control.wait_finished().await;
        }
    }
}

struct CredentialReconciliation {
    supervisor: Arc<AccountSupervisorInner>,
}

impl Drop for CredentialReconciliation {
    fn drop(&mut self) {
        self.supervisor.state.lock().reconciling = false;
    }
}

pub(crate) fn record_inspection(
    supervisor: &Weak<AccountSupervisorInner>,
    inspection: ProviderAccountInspection,
) {
    if let Some(supervisor) = supervisor.upgrade() {
        supervisor
            .state
            .lock()
            .inspections
            .insert(inspection.account_id().clone(), inspection);
    }
}

pub(crate) fn release_registration(
    supervisor: &Weak<AccountSupervisorInner>,
    account_id: &ProviderAccountId,
    registration_id: Uuid,
    remove_session: bool,
) {
    let Some(supervisor) = supervisor.upgrade() else {
        return;
    };
    let mut state = supervisor.state.lock();
    if state.account_index.get(account_id) == Some(&registration_id) {
        state.account_index.remove(account_id);
    }
    if remove_session {
        state.sessions.remove(&registration_id);
    }
}

fn shutdown_failure() -> ProviderFailure {
    ProviderFailure::new(
        ProviderFailureCode::Cancelled,
        "provider runtime is shutting down",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::RegistrationControl;

    #[tokio::test]
    async fn registration_completion_signal_releases_late_waiters() {
        let control = Arc::new(RegistrationControl::new());
        control.mark_finished();
        let left = Arc::clone(&control);
        let right = Arc::clone(&control);
        let result = tokio::time::timeout(Duration::from_millis(250), async move {
            tokio::join!(left.wait_finished(), right.wait_finished());
        })
        .await;
        assert!(result.is_ok());
    }
}
