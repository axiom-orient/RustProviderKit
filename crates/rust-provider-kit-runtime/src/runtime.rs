use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use rust_provider_kit_core::{
    BuiltInProviderId, ProviderAccountEventStream, ProviderAccountId, ProviderAccountInspection,
    ProviderAccountRegistrationRequest, ProviderAccountSummary, ProviderAuthorizationSession,
    ProviderClock, ProviderCredentialMaterial, ProviderCredentialReconciliationReport,
    ProviderCredentialStore, ProviderDescriptor, ProviderEventStream, ProviderFailure,
    ProviderFailureCode, ProviderModelCatalogResult, ProviderRequestId, ProviderTurnRequest,
    SystemProviderClock,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::account_supervisor::ProviderAccountSupervisor;
use crate::execution_supervisor::ProviderExecutionSupervisor;
use crate::http_transport::ProviderHttpTransport;
use crate::oauth_replay::OAuthStateReplayWindow;
use crate::openrouter_oauth::{OpenRouterOAuthBroker, OpenRouterOAuthRegistrationRequest};
use crate::registry::BuiltInProviderRegistry;
use crate::reqwest_transport::ReqwestProviderHttpTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeLifecycle {
    Running,
    ShuttingDown,
    ShutDown,
}

struct RuntimeState {
    lifecycle: RuntimeLifecycle,
    active_control_operations: usize,
    active_account_controls: HashMap<ProviderAccountId, usize>,
    active_registration_controls: usize,
    reconciling_credentials: bool,
    revoking_accounts: HashSet<ProviderAccountId>,
    oauth_cancellations: HashMap<ProviderAccountId, CancellationToken>,
    consumed_oauth_states: OAuthStateReplayWindow,
}

struct RuntimeInner {
    state: Mutex<RuntimeState>,
    change: watch::Sender<()>,
    registry: BuiltInProviderRegistry,
    account_supervisor: ProviderAccountSupervisor,
    execution_supervisor: ProviderExecutionSupervisor,
    open_router_oauth: OpenRouterOAuthBroker,
}

/// Public, thread-safe facade for account lifecycle and one provider turn.
///
/// Cloning this value shares the same lifecycle and supervisor state. Tool
/// execution, agent planning, durable run storage, and UI remain caller-owned.
/// Protocol configuration applies to every provider route in this runtime.
#[derive(Debug, Clone, Default)]
pub struct ProviderRuntimeOptions {
    /// Client version declared to the Codex subscription endpoint.
    ///
    /// `None` uses the version this kit implements. An explicit value is
    /// validated while the runtime is constructed.
    pub codex_client_version: Option<String>,
}

#[derive(Clone)]
pub struct ProviderRuntime {
    inner: Arc<RuntimeInner>,
}

impl std::fmt::Debug for ProviderRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.state.lock();
        formatter
            .debug_struct("ProviderRuntime")
            .field("lifecycle", &state.lifecycle)
            .field(
                "active_control_operations",
                &state.active_control_operations,
            )
            .field(
                "active_account_control_count",
                &state.active_account_controls.len(),
            )
            .field(
                "active_registration_controls",
                &state.active_registration_controls,
            )
            .field("reconciling_credentials", &state.reconciling_credentials)
            .field("revoking_account_count", &state.revoking_accounts.len())
            .finish_non_exhaustive()
    }
}

impl ProviderRuntime {
    pub fn new(
        credential_store: Arc<dyn ProviderCredentialStore>,
    ) -> Result<Self, ProviderFailure> {
        Self::with_options(credential_store, ProviderRuntimeOptions::default())
    }

    /// Build a runtime with explicit protocol configuration.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "ProviderRuntimeOptions is the owned construction contract for ProviderRuntime"
    )]
    pub fn with_options(
        credential_store: Arc<dyn ProviderCredentialStore>,
        options: ProviderRuntimeOptions,
    ) -> Result<Self, ProviderFailure> {
        Self::with_components(
            credential_store,
            Arc::new(ReqwestProviderHttpTransport::new()?),
            Arc::new(SystemProviderClock),
            &options,
        )
    }

    pub(crate) fn with_components(
        credential_store: Arc<dyn ProviderCredentialStore>,
        transport: Arc<dyn ProviderHttpTransport>,
        clock: Arc<dyn ProviderClock>,
        options: &ProviderRuntimeOptions,
    ) -> Result<Self, ProviderFailure> {
        let registry = BuiltInProviderRegistry::new(options)?;
        let (change, _receiver) = watch::channel(());
        let account_supervisor = ProviderAccountSupervisor::new(
            registry.clone(),
            Arc::clone(&credential_store),
            Arc::clone(&transport),
            Arc::clone(&clock),
        );
        let execution_supervisor = ProviderExecutionSupervisor::new(
            registry.clone(),
            credential_store,
            Arc::clone(&transport),
            Arc::clone(&clock),
        );
        let open_router_oauth = OpenRouterOAuthBroker::new(transport, clock);
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                state: Mutex::new(RuntimeState {
                    lifecycle: RuntimeLifecycle::Running,
                    active_control_operations: 0,
                    active_account_controls: HashMap::new(),
                    active_registration_controls: 0,
                    reconciling_credentials: false,
                    revoking_accounts: HashSet::new(),
                    oauth_cancellations: HashMap::new(),
                    consumed_oauth_states: OAuthStateReplayWindow::new(4_096).map_err(|_| {
                        ProviderFailure::new(
                            ProviderFailureCode::InternalInvariant,
                            "OAuth replay window configuration is invalid",
                        )
                    })?,
                }),
                change,
                registry,
                account_supervisor,
                execution_supervisor,
                open_router_oauth,
            }),
        })
    }

    #[must_use]
    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.inner.registry.descriptors()
    }

    pub async fn accounts(&self) -> Result<Vec<ProviderAccountSummary>, ProviderFailure> {
        let _guard = self.begin_control()?;
        self.inner.account_supervisor.accounts().await
    }

    pub async fn reconcile_credentials(
        &self,
    ) -> Result<ProviderCredentialReconciliationReport, ProviderFailure> {
        let _guard = self.begin_reconciliation()?;
        self.inner.account_supervisor.reconcile_credentials().await
    }

    pub async fn register(
        &self,
        request: ProviderAccountRegistrationRequest,
    ) -> ProviderAccountEventStream {
        let _guard = match self.begin_registration_control(request.account_id()) {
            Ok(guard) => guard,
            Err(failure) => {
                return ProviderAccountEventStream::failed(failure);
            }
        };
        if let Err(failure) = self.validate_credential_mode(&request) {
            return ProviderAccountEventStream::failed(failure);
        }
        self.inner.account_supervisor.register(request).await
    }

    fn validate_credential_mode(
        &self,
        request: &ProviderAccountRegistrationRequest,
    ) -> Result<(), ProviderFailure> {
        let descriptor = self
            .inner
            .registry
            .adapter(request.provider_id())?
            .descriptor()
            .clone();
        match request.credential() {
            ProviderCredentialMaterial::ExternalAuthFile(_) => {
                if request.provider_id() != &BuiltInProviderId::codex() {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::AuthenticationFailed,
                        "external auth files are supported only for Codex subscription accounts",
                    ));
                }
            }
            ProviderCredentialMaterial::OauthDerivedKey(_) => {
                if !descriptor.supports_oauth() {
                    return Err(ProviderFailure::new(
                        ProviderFailureCode::AuthenticationFailed,
                        "provider has no supported OAuth registration flow",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Register a Codex account backed by a user-authenticated ChatGPT
    /// subscription session. The path is only a reference; the runtime never
    /// copies or persists the auth file.
    pub async fn register_codex_subscription(
        &self,
        account_id: ProviderAccountId,
        label: impl Into<String>,
        auth_file: impl Into<PathBuf>,
    ) -> ProviderAccountEventStream {
        let request = ProviderCredentialMaterial::external_auth_file(auth_file.into()).and_then(
            |credential| {
                ProviderAccountRegistrationRequest::new(
                    account_id,
                    BuiltInProviderId::codex(),
                    label,
                    credential,
                    None,
                )
            },
        );
        match request {
            Ok(request) => self.register(request).await,
            Err(error) => ProviderAccountEventStream::failed(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                error.to_string(),
            )),
        }
    }

    pub async fn cancel_registration(&self, account_id: &ProviderAccountId) {
        let oauth_cancellation = self
            .inner
            .state
            .lock()
            .oauth_cancellations
            .get(account_id)
            .cloned();
        if let Some(cancellation) = oauth_cancellation {
            cancellation.cancel();
        }
        self.inner.account_supervisor.cancel(account_id).await;
    }

    pub async fn revoke(&self, account_id: &ProviderAccountId) -> Result<(), ProviderFailure> {
        let operation = self.begin_revocation(account_id)?;

        let runtime = Arc::clone(&self.inner);
        let account_id = account_id.clone();
        let task = tokio::spawn(async move {
            let _operation = operation;
            let _cleanup = RevocationCleanup {
                runtime: Arc::clone(&runtime),
                account_id: account_id.clone(),
            };
            runtime
                .account_supervisor
                .block_account(&account_id)
                .await?;
            wait_for_account_controls(&runtime, &account_id).await;
            runtime
                .execution_supervisor
                .block_account(&account_id)
                .await;
            runtime.account_supervisor.revoke(&account_id).await
        });

        task.await.map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "provider account revocation worker terminated unexpectedly",
            )
        })?
    }

    pub async fn register_open_router_oauth(
        &self,
        request: OpenRouterOAuthRegistrationRequest,
        authorization_session: &dyn ProviderAuthorizationSession,
    ) -> Result<ProviderAccountEventStream, ProviderFailure> {
        let _guard = self.begin_registration_control(request.account_id())?;
        let cancellation = CancellationToken::new();
        {
            let mut state = self.inner.state.lock();
            if state.oauth_cancellations.contains_key(request.account_id()) {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "provider account OAuth registration is already active",
                ));
            }
            state
                .oauth_cancellations
                .insert(request.account_id().clone(), cancellation.clone());
        }
        let _oauth_cleanup = OAuthCancellationCleanup {
            runtime: Arc::downgrade(&self.inner),
            account_id: request.account_id().clone(),
        };
        {
            let mut state = self.inner.state.lock();
            if !state.consumed_oauth_states.consume(request.pkce().state()) {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::AuthenticationFailed,
                    "OpenRouter OAuth state has already been consumed",
                ));
            }
        }
        let registration = self
            .inner
            .open_router_oauth
            .authorize(request, authorization_session, &cancellation)
            .await?;
        if self
            .inner
            .state
            .lock()
            .revoking_accounts
            .contains(registration.account_id())
        {
            return Ok(ProviderAccountEventStream::failed(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is being revoked",
            )));
        }
        Ok(self
            .inner
            .account_supervisor
            .register_with_cancellation(registration, cancellation)
            .await)
    }

    pub async fn inspect(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderAccountInspection, ProviderFailure> {
        let _guard = self.begin_account_control(account_id)?;
        self.inner.account_supervisor.inspect(account_id).await
    }

    pub async fn models(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderModelCatalogResult, ProviderFailure> {
        let _guard = self.begin_account_control(account_id)?;
        self.inner.account_supervisor.models(account_id).await
    }

    pub async fn execute(&self, request: ProviderTurnRequest) -> ProviderEventStream {
        let _guard = match self.begin_account_control(request.selection().account_id()) {
            Ok(guard) => guard,
            Err(failure) => {
                return ProviderEventStream::failed(failure.with_request_id(request.id().clone()));
            }
        };
        self.inner.execution_supervisor.execute(request).await
    }

    pub async fn cancel(&self, request_id: &ProviderRequestId) {
        self.inner.execution_supervisor.cancel(request_id).await;
    }

    pub async fn shutdown(&self) {
        let start_shutdown = {
            let mut state = self.inner.state.lock();
            match state.lifecycle {
                RuntimeLifecycle::ShutDown => return,
                RuntimeLifecycle::ShuttingDown => false,
                RuntimeLifecycle::Running => {
                    state.lifecycle = RuntimeLifecycle::ShuttingDown;
                    true
                }
            }
        };

        if start_shutdown {
            // Shutdown is runtime-owned. Dropping the caller future must not abort
            // cancellation and join of admitted operations.
            drop(tokio::spawn(finish_shutdown(Arc::clone(&self.inner))));
        }
        self.wait_until_shut_down().await;
    }

    fn begin_control(&self) -> Result<ControlOperation, ProviderFailure> {
        let mut state = self.inner.state.lock();
        ensure_running(&state)?;
        increment_control_operations(&mut state)?;
        Ok(ControlOperation {
            runtime: Arc::downgrade(&self.inner),
            account_id: None,
            registration_control: false,
            clears_reconciliation: false,
        })
    }

    fn begin_account_control(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ControlOperation, ProviderFailure> {
        self.begin_scoped_account_control(account_id, false)
    }

    fn begin_registration_control(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ControlOperation, ProviderFailure> {
        self.begin_scoped_account_control(account_id, true)
    }

    fn begin_scoped_account_control(
        &self,
        account_id: &ProviderAccountId,
        registration_control: bool,
    ) -> Result<ControlOperation, ProviderFailure> {
        let mut state = self.inner.state.lock();
        ensure_running(&state)?;
        if state.reconciling_credentials {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "credential reconciliation is active",
            ));
        }
        if state.revoking_accounts.contains(account_id) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::AccountUnavailable,
                "provider account is being revoked",
            ));
        }
        let next_control_count = state
            .active_control_operations
            .checked_add(1)
            .ok_or_else(control_counter_failure)?;
        let next_account_count = state
            .active_account_controls
            .get(account_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(control_counter_failure)?;
        let next_registration_count = if registration_control {
            Some(
                state
                    .active_registration_controls
                    .checked_add(1)
                    .ok_or_else(control_counter_failure)?,
            )
        } else {
            None
        };
        state.active_control_operations = next_control_count;
        state
            .active_account_controls
            .insert(account_id.clone(), next_account_count);
        if let Some(next) = next_registration_count {
            state.active_registration_controls = next;
        }
        Ok(ControlOperation {
            runtime: Arc::downgrade(&self.inner),
            account_id: Some(account_id.clone()),
            registration_control,
            clears_reconciliation: false,
        })
    }

    fn begin_reconciliation(&self) -> Result<ControlOperation, ProviderFailure> {
        let mut state = self.inner.state.lock();
        ensure_running(&state)?;
        if state.reconciling_credentials
            || state.active_registration_controls != 0
            || !state.revoking_accounts.is_empty()
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "credential reconciliation conflicts with another credential mutation",
            ));
        }
        let next_count = state
            .active_control_operations
            .checked_add(1)
            .ok_or_else(control_counter_failure)?;
        state.reconciling_credentials = true;
        state.active_control_operations = next_count;
        Ok(ControlOperation {
            runtime: Arc::downgrade(&self.inner),
            account_id: None,
            registration_control: false,
            clears_reconciliation: true,
        })
    }

    fn begin_revocation(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ControlOperation, ProviderFailure> {
        let mut state = self.inner.state.lock();
        ensure_running(&state)?;
        if state.reconciling_credentials {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "provider account cannot be revoked during credential reconciliation",
            ));
        }
        if state.revoking_accounts.contains(account_id) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "provider account revocation is already active",
            ));
        }
        let next_count = state
            .active_control_operations
            .checked_add(1)
            .ok_or_else(control_counter_failure)?;
        state.revoking_accounts.insert(account_id.clone());
        state.active_control_operations = next_count;
        Ok(ControlOperation {
            runtime: Arc::downgrade(&self.inner),
            account_id: None,
            registration_control: false,
            clears_reconciliation: false,
        })
    }

    async fn wait_until_shut_down(&self) {
        let mut receiver = self.inner.change.subscribe();
        loop {
            if self.inner.state.lock().lifecycle == RuntimeLifecycle::ShutDown {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

fn ensure_running(state: &RuntimeState) -> Result<(), ProviderFailure> {
    if state.lifecycle == RuntimeLifecycle::Running {
        Ok(())
    } else {
        Err(shutdown_failure())
    }
}

fn increment_control_operations(state: &mut RuntimeState) -> Result<(), ProviderFailure> {
    state.active_control_operations = state
        .active_control_operations
        .checked_add(1)
        .ok_or_else(control_counter_failure)?;
    Ok(())
}

fn control_counter_failure() -> ProviderFailure {
    ProviderFailure::new(
        ProviderFailureCode::InternalInvariant,
        "provider runtime control-operation counter exhausted",
    )
}

struct RevocationCleanup {
    runtime: Arc<RuntimeInner>,
    account_id: ProviderAccountId,
}

struct OAuthCancellationCleanup {
    runtime: Weak<RuntimeInner>,
    account_id: ProviderAccountId,
}

impl Drop for OAuthCancellationCleanup {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime
                .state
                .lock()
                .oauth_cancellations
                .remove(&self.account_id);
        }
    }
}

impl Drop for RevocationCleanup {
    fn drop(&mut self) {
        self.runtime
            .account_supervisor
            .unblock_account(&self.account_id);
        self.runtime
            .execution_supervisor
            .unblock_account(&self.account_id);
        self.runtime
            .state
            .lock()
            .revoking_accounts
            .remove(&self.account_id);
    }
}

async fn finish_shutdown(runtime: Arc<RuntimeInner>) {
    let _completion = ShutdownCompletion {
        runtime: Arc::clone(&runtime),
    };
    let mut receiver = runtime.change.subscribe();
    loop {
        if runtime.state.lock().active_control_operations == 0 {
            break;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
    runtime.account_supervisor.shutdown().await;
    runtime.execution_supervisor.shutdown().await;
}

struct ShutdownCompletion {
    runtime: Arc<RuntimeInner>,
}

impl Drop for ShutdownCompletion {
    fn drop(&mut self) {
        self.runtime.state.lock().lifecycle = RuntimeLifecycle::ShutDown;
        self.runtime.change.send_replace(());
    }
}

struct ControlOperation {
    runtime: Weak<RuntimeInner>,
    account_id: Option<ProviderAccountId>,
    registration_control: bool,
    clears_reconciliation: bool,
}

impl Drop for ControlOperation {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let mut state = runtime.state.lock();
        let mut account_drained = false;
        if let Some(account_id) = &self.account_id {
            let remove_account = state
                .active_account_controls
                .get_mut(account_id)
                .is_some_and(|count| {
                    if let Some(next) = count.checked_sub(1) {
                        *count = next;
                    }
                    *count == 0
                });
            if remove_account {
                state.active_account_controls.remove(account_id);
                account_drained = true;
            }
        }
        if self.registration_control
            && let Some(next) = state.active_registration_controls.checked_sub(1)
        {
            state.active_registration_controls = next;
        }
        if self.clears_reconciliation {
            state.reconciling_credentials = false;
        }
        if let Some(next) = state.active_control_operations.checked_sub(1) {
            state.active_control_operations = next;
        }
        let drained = state.active_control_operations == 0;
        drop(state);
        if drained || account_drained || self.clears_reconciliation {
            runtime.change.send_replace(());
        }
    }
}

async fn wait_for_account_controls(runtime: &RuntimeInner, account_id: &ProviderAccountId) {
    let mut receiver = runtime.change.subscribe();
    loop {
        if !runtime
            .state
            .lock()
            .active_account_controls
            .contains_key(account_id)
        {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn shutdown_failure() -> ProviderFailure {
    ProviderFailure::new(
        ProviderFailureCode::Cancelled,
        "provider runtime is shutting down",
    )
}
