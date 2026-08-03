use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use rust_provider_kit_core::{
    ProviderAccountId, ProviderClock, ProviderCredentialStore, ProviderEventStream,
    ProviderFailure, ProviderFailureCode, ProviderRequestId, ProviderTerminal, ProviderTurnRequest,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::execution_session::{ExecutionSession, ExecutionSessionResources};
use crate::http_transport::ProviderHttpTransport;
use crate::registry::BuiltInProviderRegistry;

#[derive(Clone)]
pub(crate) struct ProviderExecutionSupervisor {
    inner: Arc<ExecutionSupervisorInner>,
}

pub(crate) struct ExecutionSupervisorInner {
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
    state: Mutex<ExecutionSupervisorState>,
}

#[derive(Default)]
struct ExecutionSupervisorState {
    sessions: HashMap<Uuid, ActiveExecution>,
    request_index: HashMap<ProviderRequestId, Uuid>,
    account_index: HashMap<ProviderAccountId, HashSet<Uuid>>,
    blocked_accounts: HashSet<ProviderAccountId>,
    shutting_down: bool,
}

struct ActiveExecution {
    account_id: ProviderAccountId,
    control: Arc<ExecutionControl>,
}

pub(crate) struct ExecutionControl {
    pub(crate) cancellation: CancellationToken,
    finished: watch::Sender<bool>,
}

impl ExecutionControl {
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

impl ProviderExecutionSupervisor {
    pub(crate) fn new(
        registry: BuiltInProviderRegistry,
        vault: Arc<dyn ProviderCredentialStore>,
        transport: Arc<dyn ProviderHttpTransport>,
        clock: Arc<dyn ProviderClock>,
    ) -> Self {
        Self {
            inner: Arc::new(ExecutionSupervisorInner {
                registry,
                vault,
                transport,
                clock,
                state: Mutex::new(ExecutionSupervisorState::default()),
            }),
        }
    }

    pub(crate) async fn execute(&self, request: ProviderTurnRequest) -> ProviderEventStream {
        let (stream, sink) = ProviderEventStream::with_defaults();
        let execution_id = Uuid::new_v4();
        let control = Arc::new(ExecutionControl::new());
        {
            let mut state = self.inner.state.lock();
            let failure = if state.shutting_down {
                Some(ProviderFailure::new(
                    ProviderFailureCode::Cancelled,
                    "provider runtime is shutting down",
                ))
            } else if state
                .blocked_accounts
                .contains(request.selection().account_id())
            {
                Some(ProviderFailure::new(
                    ProviderFailureCode::AccountUnavailable,
                    "provider account is being revoked",
                ))
            } else if state.request_index.contains_key(request.id()) {
                Some(ProviderFailure::new(
                    ProviderFailureCode::InvalidRequest,
                    "provider request ID is already active",
                ))
            } else {
                None
            };
            if let Some(failure) = failure {
                let failure = failure.with_request_id(request.id().clone());
                let _ = sink.finish(ProviderTerminal::Failed(failure));
                return stream;
            }
            state
                .request_index
                .insert(request.id().clone(), execution_id);
            state.sessions.insert(
                execution_id,
                ActiveExecution {
                    account_id: request.selection().account_id().clone(),
                    control: Arc::clone(&control),
                },
            );
            state
                .account_index
                .entry(request.selection().account_id().clone())
                .or_default()
                .insert(execution_id);
        }

        let request_id = request.id().clone();
        let session = ExecutionSession::new(
            execution_id,
            request,
            ExecutionSessionResources::new(
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
                release_execution(&supervisor, &request_id, execution_id, true);
                let failure = ProviderFailure::new(
                    ProviderFailureCode::InternalInvariant,
                    "provider execution worker terminated unexpectedly",
                )
                .with_request_id(request_id);
                let _ = emergency_sink.finish(ProviderTerminal::Failed(failure));
                control.mark_finished();
            }
        }));
        stream
    }

    pub(crate) async fn cancel(&self, request_id: &ProviderRequestId) {
        let control = {
            let state = self.inner.state.lock();
            state
                .request_index
                .get(request_id)
                .and_then(|id| state.sessions.get(id))
                .map(|active| Arc::clone(&active.control))
        };
        if let Some(control) = control {
            control.cancel();
            control.wait_finished().await;
        }
    }

    pub(crate) async fn block_account(&self, account_id: &ProviderAccountId) {
        let controls = {
            let mut state = self.inner.state.lock();
            state.blocked_accounts.insert(account_id.clone());
            state
                .account_index
                .get(account_id)
                .into_iter()
                .flatten()
                .filter_map(|execution_id| state.sessions.get(execution_id))
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

    pub(crate) fn unblock_account(&self, account_id: &ProviderAccountId) {
        self.inner.state.lock().blocked_accounts.remove(account_id);
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

pub(crate) fn release_execution(
    supervisor: &Weak<ExecutionSupervisorInner>,
    request_id: &ProviderRequestId,
    execution_id: Uuid,
    remove_session: bool,
) {
    let Some(supervisor) = supervisor.upgrade() else {
        return;
    };
    let mut state = supervisor.state.lock();
    if state.request_index.get(request_id) == Some(&execution_id) {
        state.request_index.remove(request_id);
    }
    if remove_session && let Some(active) = state.sessions.remove(&execution_id) {
        let remove_account_index =
            state
                .account_index
                .get_mut(&active.account_id)
                .is_some_and(|execution_ids| {
                    execution_ids.remove(&execution_id);
                    execution_ids.is_empty()
                });
        if remove_account_index {
            state.account_index.remove(&active.account_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::ExecutionControl;

    #[tokio::test]
    async fn execution_completion_signal_releases_late_waiters() {
        let control = Arc::new(ExecutionControl::new());
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
