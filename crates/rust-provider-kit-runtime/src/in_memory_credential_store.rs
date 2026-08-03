use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use rust_provider_kit_core::{
    ProviderAccountId, ProviderAccountRegistrationRequest, ProviderCredentialLease,
    ProviderCredentialMaterial, ProviderCredentialRecord, ProviderCredentialRecordState,
    ProviderCredentialReference, ProviderCredentialStore, ProviderFailure, ProviderFailureCode,
    ProviderInstant,
};
use tokio::sync::Barrier;

use crate::wire::core_error_failure;

/// Process-lifetime credential store for tests, previews, and explicitly
/// ephemeral integrations. It never writes credential material to disk.
#[derive(Clone, Default)]
pub struct InMemoryProviderCredentialStore {
    state: Arc<Mutex<InMemoryCredentialStoreState>>,
    records_gate: Option<RecordsGate>,
}

#[derive(Clone)]
struct RecordsGate {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[derive(Default)]
struct InMemoryCredentialStoreState {
    credentials: HashMap<ProviderAccountId, StoredCredential>,
    reference_generation: u64,
}

#[derive(Clone)]
struct StoredCredential {
    record: ProviderCredentialRecord,
    material: ProviderCredentialMaterial,
}

impl std::fmt::Debug for InMemoryProviderCredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InMemoryProviderCredentialStore")
            .field("credential_count", &self.state.lock().credentials.len())
            .field("records_are_gated", &self.records_gate.is_some())
            .finish_non_exhaustive()
    }
}

impl InMemoryProviderCredentialStore {
    #[cfg(test)]
    pub(crate) fn with_records_gate() -> (Self, Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        (
            Self {
                state: Arc::new(Mutex::new(InMemoryCredentialStoreState::default())),
                records_gate: Some(RecordsGate {
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                }),
            },
            entered,
            release,
        )
    }
}

#[async_trait]
impl ProviderCredentialStore for InMemoryProviderCredentialStore {
    async fn stage(
        &self,
        request: &ProviderAccountRegistrationRequest,
        at: ProviderInstant,
    ) -> Result<ProviderCredentialRecord, ProviderFailure> {
        let mut state = self.state.lock();
        if state.credentials.contains_key(request.account_id()) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "provider account already exists",
            ));
        }
        let next_generation = state.reference_generation.checked_add(1).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "in-memory credential reference generation exhausted",
            )
        })?;
        let reference = ProviderCredentialReference::new(format!("memory-{next_generation}"))
            .map_err(core_error_failure)?;
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
        .map_err(core_error_failure)?;
        state.reference_generation = next_generation;
        state.credentials.insert(
            request.account_id().clone(),
            StoredCredential {
                record: record.clone(),
                material: request.credential().clone(),
            },
        );
        Ok(record)
    }

    async fn activate(
        &self,
        record: &ProviderCredentialRecord,
        at: ProviderInstant,
    ) -> Result<(), ProviderFailure> {
        let mut state = self.state.lock();
        let stored = state
            .credentials
            .get_mut(record.account_id())
            .ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::CredentialRecoveryRequired,
                    "staged credential identity does not match stored state",
                )
            })?;
        if stored.record != *record || record.state() != ProviderCredentialRecordState::Staged {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CredentialRecoveryRequired,
                "staged credential identity does not match stored state",
            ));
        }
        stored.record = ProviderCredentialRecord::new(
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
        .map_err(core_error_failure)?;
        Ok(())
    }

    async fn remove(&self, record: &ProviderCredentialRecord) -> Result<(), ProviderFailure> {
        let mut state = self.state.lock();
        let Some(stored) = state.credentials.get(record.account_id()) else {
            return Ok(());
        };
        if stored.record.reference() != record.reference() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::CredentialRecoveryRequired,
                "credential identity does not match stored state",
            ));
        }
        state.credentials.remove(record.account_id());
        Ok(())
    }

    async fn record(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderCredentialRecord>, ProviderFailure> {
        Ok(self
            .state
            .lock()
            .credentials
            .get(account_id)
            .map(|stored| stored.record.clone()))
    }

    async fn lease(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderCredentialLease, ProviderFailure> {
        let state = self.state.lock();
        let stored = state.credentials.get(account_id).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is unavailable",
            )
        })?;
        if stored.record.state() != ProviderCredentialRecordState::Active {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is unavailable",
            ));
        }
        ProviderCredentialLease::new(stored.record.clone(), stored.material.clone())
            .map_err(core_error_failure)
    }

    async fn records(&self) -> Result<Vec<ProviderCredentialRecord>, ProviderFailure> {
        if let Some(gate) = &self.records_gate {
            gate.entered.wait().await;
            gate.release.wait().await;
        }
        Ok(self
            .state
            .lock()
            .credentials
            .values()
            .map(|stored| stored.record.clone())
            .collect())
    }
}
