use std::collections::BTreeSet;

use rust_provider_kit_core::{
    ProviderAccountId, ProviderCredentialLease, ProviderCredentialRecord,
    ProviderCredentialRecordState, ProviderCredentialReference, ProviderFailure,
    ProviderFailureCode, ProviderId,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderCredentialContract;

impl ProviderCredentialContract {
    pub(crate) fn validate_active_from_staged(
        staged: &ProviderCredentialRecord,
        active: &ProviderCredentialRecord,
    ) -> Result<(), ProviderFailure> {
        if active.reference() != staged.reference()
            || active.account_id() != staged.account_id()
            || active.provider_id() != staged.provider_id()
            || active.label() != staged.label()
            || active.source() != staged.source()
            || active.endpoint() != staged.endpoint()
            || active.state() != ProviderCredentialRecordState::Active
            || active.created_at() != staged.created_at()
        {
            return Err(recovery(
                "credential activation read-back did not match the staged record",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_lease(
        lease: &ProviderCredentialLease,
        expected_account: &ProviderAccountId,
        expected_provider: &ProviderId,
    ) -> Result<(), ProviderFailure> {
        let record = lease.record();
        if record.state() != ProviderCredentialRecordState::Active
            || record.source() != lease.material().source()
        {
            return Err(recovery("credential vault returned an invalid lease"));
        }
        if record.account_id() != expected_account {
            return Err(recovery(
                "credential vault returned a lease for a different account",
            ));
        }
        if record.provider_id() != expected_provider {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account does not belong to the selected provider",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_lease_account(
        lease: &ProviderCredentialLease,
        expected_account: &ProviderAccountId,
    ) -> Result<(), ProviderFailure> {
        let record = lease.record();
        if record.state() != ProviderCredentialRecordState::Active
            || record.source() != lease.material().source()
        {
            return Err(recovery("credential vault returned an invalid lease"));
        }
        if record.account_id() != expected_account {
            return Err(recovery(
                "credential vault returned a lease for a different account",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_records(
        records: &[ProviderCredentialRecord],
    ) -> Result<(), ProviderFailure> {
        let mut accounts = BTreeSet::<ProviderAccountId>::new();
        let mut references = BTreeSet::<ProviderCredentialReference>::new();
        for record in records {
            if !accounts.insert(record.account_id().clone())
                || !references.insert(record.reference().clone())
            {
                return Err(recovery("credential vault returned an invalid record set"));
            }
        }
        Ok(())
    }
}

fn recovery(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::CredentialRecoveryRequired, message)
}
