use std::sync::{Arc, Weak};

use futures_util::StreamExt;
use rust_provider_kit_core::{
    ProviderClock, ProviderCompletion, ProviderCredentialStore, ProviderEventSink,
    ProviderExecutionEffect, ProviderExecutionEvent, ProviderExecutionPhase,
    ProviderExecutionReducer, ProviderExecutionState, ProviderFailure, ProviderFailureCode,
    ProviderMailboxSendResult, ProviderResponseMetadata, ProviderTerminal, ProviderTurnEvent,
    ProviderTurnRequest,
};
use uuid::Uuid;

use crate::adapter::{ProviderAdapter, ProviderStreamDecoder};
use crate::credential_contract::ProviderCredentialContract;
use crate::execution_supervisor::{ExecutionControl, ExecutionSupervisorInner, release_execution};
use crate::http_transport::{
    ProviderByteStream, ProviderHttpResponseControl, ProviderHttpTransport,
};
use crate::registry::BuiltInProviderRegistry;
use crate::sse::{ServerSentEvent, ServerSentEventDecoder};
use crate::wire::{ProviderDecodedEvent, http_failure_parts, transport_failure};

pub(crate) struct ExecutionSessionResources {
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
}

impl ExecutionSessionResources {
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

pub(crate) struct ExecutionSession {
    execution_id: Uuid,
    request: ProviderTurnRequest,
    registry: BuiltInProviderRegistry,
    vault: Arc<dyn ProviderCredentialStore>,
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
    sink: ProviderEventSink,
    control: Arc<ExecutionControl>,
    supervisor: Weak<ExecutionSupervisorInner>,
    state: ProviderExecutionState,
    current_transport: Option<ProviderHttpResponseControl>,
    supervisor_released: bool,
}

impl ExecutionSession {
    pub(crate) fn new(
        execution_id: Uuid,
        request: ProviderTurnRequest,
        resources: ExecutionSessionResources,
        sink: ProviderEventSink,
        control: Arc<ExecutionControl>,
        supervisor: Weak<ExecutionSupervisorInner>,
    ) -> Self {
        Self {
            execution_id,
            request,
            registry: resources.registry,
            vault: resources.vault,
            transport: resources.transport,
            clock: resources.clock,
            sink,
            control,
            supervisor,
            state: ProviderExecutionState::default(),
            current_transport: None,
            supervisor_released: false,
        }
    }

    pub(crate) async fn run(mut self) {
        let admission = ProviderExecutionReducer::reduce(
            self.state.clone(),
            ProviderExecutionEvent::RequestAdmitted(self.request.clone()),
        );
        match admission {
            Ok((state, effects)) => {
                self.state = state;
                if !effects.iter().any(|effect| {
                    matches!(effect, ProviderExecutionEffect::OpenTransport(request, generation)
                        if request.id() == self.request.id() && *generation == self.state.generation())
                }) {
                    self.finish_direct(invariant_failure(
                        &self.request,
                        "provider execution admission emitted no transport effect",
                    ));
                    return;
                }
            }
            Err(error) => {
                self.finish_direct(
                    crate::wire::core_error_failure(error)
                        .with_request_id(self.request.id().clone()),
                );
                return;
            }
        }

        let cancellation = self.control.cancellation.clone();
        let clock = Arc::clone(&self.clock);
        let timeout = self.request.constraints().timeout_milliseconds();
        let request_id = self.request.id().clone();
        let terminal_event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => ProviderExecutionEvent::CancelRequested,
            deadline = clock.sleep(timeout) => {
                match deadline {
                    Ok(()) => ProviderExecutionEvent::TimeoutExpired,
                    Err(failure) => ProviderExecutionEvent::TransportFailed(
                        failure.with_request_id(request_id)
                    ),
                }
            }
            result = self.open_and_consume() => {
                match result {
                    Ok(completion) => ProviderExecutionEvent::TransportCompleted(completion),
                    Err(failure) if failure.code() == ProviderFailureCode::ConsumerBackpressureExceeded => ProviderExecutionEvent::MailboxOverflowed,
                    Err(failure) if failure.code() == ProviderFailureCode::Cancelled => ProviderExecutionEvent::CancelRequested,
                    Err(failure) => ProviderExecutionEvent::TransportFailed(failure),
                }
            }
        };

        self.terminate(terminal_event).await;
        self.control.mark_finished();
        release_execution(&self.supervisor, self.request.id(), self.execution_id, true);
    }

    async fn open_and_consume(&mut self) -> Result<ProviderCompletion, ProviderFailure> {
        let lease = self
            .vault
            .lease(self.request.selection().account_id())
            .await?;
        ProviderCredentialContract::validate_lease(
            &lease,
            self.request.selection().account_id(),
            self.request.selection().provider_id(),
        )?;
        let adapter = self
            .registry
            .adapter(self.request.selection().provider_id())?;
        let wire_request = adapter
            .make_execution_request(&self.request, &lease)
            .await?;

        let mut attempt = 1usize;
        loop {
            if self.control.cancellation.is_cancelled() {
                return Err(cancelled_failure(&self.request));
            }
            let response = match self.transport.open(wire_request.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    let failure =
                        transport_failure(error).with_request_id(self.request.id().clone());
                    if self.retry_before_visible(&failure, attempt).await? {
                        attempt = attempt.checked_add(1).ok_or_else(|| {
                            invariant_failure(&self.request, "provider retry counter overflow")
                        })?;
                        continue;
                    }
                    return Err(failure);
                }
            };

            let (status, headers, body, control) = response.into_parts();
            self.attach_transport(control).await?;

            if !(200..300).contains(&status) {
                let body_result = self
                    .collect_body(body, wire_request.maximum_response_bytes())
                    .await;
                let body = match body_result {
                    Ok(body) => {
                        self.close_current_transport(false).await;
                        body
                    }
                    Err(failure) => {
                        self.close_current_transport(true).await;
                        if self.retry_before_visible(&failure, attempt).await? {
                            attempt = attempt.checked_add(1).ok_or_else(|| {
                                invariant_failure(&self.request, "provider retry counter overflow")
                            })?;
                            continue;
                        }
                        return Err(failure);
                    }
                };
                let failure = http_failure_parts(status, &headers, &body, self.clock.now().await?)
                    .with_request_id(self.request.id().clone());
                if self.retry_before_visible(&failure, attempt).await? {
                    attempt = attempt.checked_add(1).ok_or_else(|| {
                        invariant_failure(&self.request, "provider retry counter overflow")
                    })?;
                    continue;
                }
                return Err(failure);
            }

            match self
                .consume_successful(headers, body, adapter.as_ref())
                .await
            {
                Ok(completion) => {
                    self.close_current_transport(false).await;
                    return Ok(completion);
                }
                Err(failure) => {
                    self.close_current_transport(true).await;
                    if self.retry_before_visible(&failure, attempt).await? {
                        attempt = attempt.checked_add(1).ok_or_else(|| {
                            invariant_failure(&self.request, "provider retry counter overflow")
                        })?;
                        continue;
                    }
                    return Err(failure);
                }
            }
        }
    }

    async fn attach_transport(
        &mut self,
        control: ProviderHttpResponseControl,
    ) -> Result<(), ProviderFailure> {
        if self.current_transport.is_some() {
            control.cancel();
            control.wait_for_termination().await;
            self.close_current_transport(true).await;
            return Err(invariant_failure(
                &self.request,
                "provider execution attempted to attach a second active transport",
            ));
        }
        self.current_transport = Some(control);
        Ok(())
    }

    async fn close_current_transport(&mut self, cancel: bool) {
        let Some(control) = self.current_transport.take() else {
            return;
        };
        if cancel {
            control.cancel();
        }
        control.wait_for_termination().await;
    }

    async fn consume_successful(
        &mut self,
        headers: std::collections::BTreeMap<String, String>,
        mut body: ProviderByteStream,
        adapter: &dyn ProviderAdapter,
    ) -> Result<ProviderCompletion, ProviderFailure> {
        let provider_request_id = headers
            .get("x-request-id")
            .or_else(|| headers.get("request-id"))
            .or_else(|| headers.get("x-goog-request-id"))
            .cloned();
        let metadata = ProviderResponseMetadata::new(
            self.request.id().clone(),
            provider_request_id,
            self.request.selection().provider_id().clone(),
            self.request.selection().model_id().clone(),
            self.clock.now().await?,
        )
        .map_err(crate::wire::core_error_failure)?;
        self.transition_and_publish(ProviderExecutionEvent::TransportOpened(metadata))?;

        let maximum = self.request.constraints().maximum_response_bytes();
        let mut received = 0usize;
        let mut sse = ServerSentEventDecoder::defaults();
        let mut decoder = adapter.make_decoder(&self.request)?;
        let mut completion: Option<ProviderCompletion> = None;

        while let Some(chunk) = body.next().await {
            if self.control.cancellation.is_cancelled() {
                return Err(cancelled_failure(&self.request));
            }
            let chunk = chunk.map_err(|error| {
                transport_failure(error).with_request_id(self.request.id().clone())
            })?;
            received = received.checked_add(chunk.len()).ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "provider response size overflow",
                )
                .with_request_id(self.request.id().clone())
            })?;
            if received > maximum {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "provider response exceeds byte limit",
                )
                .with_request_id(self.request.id().clone()));
            }
            for event in sse.push(&chunk)? {
                self.consume_decoded_events(decoder.as_mut(), &event, &mut completion)
                    .await?;
            }
        }
        for event in sse.finish()? {
            self.consume_decoded_events(decoder.as_mut(), &event, &mut completion)
                .await?;
        }
        for decoded in decoder.finish()? {
            self.consume_decoded(decoded, &mut completion).await?;
        }
        completion.ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider stream ended without a completion event",
            )
            .with_request_id(self.request.id().clone())
        })
    }

    async fn consume_decoded_events(
        &mut self,
        decoder: &mut dyn ProviderStreamDecoder,
        event: &ServerSentEvent,
        completion: &mut Option<ProviderCompletion>,
    ) -> Result<(), ProviderFailure> {
        for decoded in decoder.consume(event)? {
            self.consume_decoded(decoded, completion).await?;
        }
        Ok(())
    }

    async fn consume_decoded(
        &mut self,
        decoded: ProviderDecodedEvent,
        completion: &mut Option<ProviderCompletion>,
    ) -> Result<(), ProviderFailure> {
        if completion.is_some() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "provider emitted data after its completion event",
            )
            .with_request_id(self.request.id().clone()));
        }
        match decoded {
            ProviderDecodedEvent::Reasoning(value) => {
                self.transition_and_publish(ProviderExecutionEvent::ReasoningDeltaReceived(value))?;
            }
            ProviderDecodedEvent::Text(value) => {
                self.transition_and_publish(ProviderExecutionEvent::TextDeltaReceived(value))?;
            }
            ProviderDecodedEvent::ToolCall(call) => {
                self.transition_and_publish(ProviderExecutionEvent::ToolCallCompleted(call))?;
            }
            ProviderDecodedEvent::Completion(draft) => {
                *completion = Some(draft.materialize(self.clock.now().await?)?);
            }
        }
        Ok(())
    }

    fn transition_and_publish(
        &mut self,
        event: ProviderExecutionEvent,
    ) -> Result<(), ProviderFailure> {
        let (state, effects) = ProviderExecutionReducer::reduce(self.state.clone(), event)
            .map_err(crate::wire::core_error_failure)?;
        self.state = state;
        for effect in effects {
            if let ProviderExecutionEffect::Publish(event) = effect {
                self.publish(event)?;
            }
        }
        Ok(())
    }

    fn publish(&self, event: ProviderTurnEvent) -> Result<(), ProviderFailure> {
        match self.sink.send(event) {
            ProviderMailboxSendResult::Accepted | ProviderMailboxSendResult::Coalesced => Ok(()),
            ProviderMailboxSendResult::Overflow => Err(ProviderFailure::new(
                ProviderFailureCode::ConsumerBackpressureExceeded,
                "provider event consumer exceeded the bounded backlog",
            )
            .with_request_id(self.request.id().clone())),
            ProviderMailboxSendResult::Closed => Err(invariant_failure(
                &self.request,
                "provider event stream closed before execution cleanup",
            )),
        }
    }

    async fn retry_before_visible(
        &mut self,
        failure: &ProviderFailure,
        attempt: usize,
    ) -> Result<bool, ProviderFailure> {
        if attempt >= self.request.constraints().maximum_retry_attempts()
            || !is_retryable(failure.code())
            || !self.is_before_visible_output()
        {
            return Ok(false);
        }
        if matches!(
            self.state.phase(),
            ProviderExecutionPhase::Streaming(_, _, false)
        ) {
            let (state, effects) = ProviderExecutionReducer::reduce(
                self.state.clone(),
                ProviderExecutionEvent::RetryRequested,
            )
            .map_err(crate::wire::core_error_failure)?;
            if !effects.is_empty() {
                return Err(invariant_failure(
                    &self.request,
                    "provider retry unexpectedly emitted an effect",
                ));
            }
            self.state = state;
        }
        let shift = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        let exponential = 250u64.checked_shl(shift).unwrap_or(u64::MAX);
        let requested = match failure.retry_after_milliseconds() {
            Some(value) => value,
            None => exponential,
        };
        self.clock.sleep(requested.min(60_000)).await?;
        Ok(true)
    }

    fn is_before_visible_output(&self) -> bool {
        match self.state.phase() {
            ProviderExecutionPhase::Opening(_) => true,
            ProviderExecutionPhase::Streaming(_, _, visible) => !visible,
            ProviderExecutionPhase::Idle
            | ProviderExecutionPhase::Terminating(_, _, _)
            | ProviderExecutionPhase::Terminal(_) => false,
        }
    }

    async fn collect_body(
        &self,
        mut stream: ProviderByteStream,
        maximum: usize,
    ) -> Result<Vec<u8>, ProviderFailure> {
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            if self.control.cancellation.is_cancelled() {
                return Err(cancelled_failure(&self.request));
            }
            let chunk = chunk.map_err(|error| {
                transport_failure(error).with_request_id(self.request.id().clone())
            })?;
            let next = body.len().checked_add(chunk.len()).ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "provider response size overflow",
                )
                .with_request_id(self.request.id().clone())
            })?;
            if next > maximum {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "provider response exceeds byte limit",
                )
                .with_request_id(self.request.id().clone()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn terminate(&mut self, event: ProviderExecutionEvent) {
        if matches!(
            self.state.phase(),
            ProviderExecutionPhase::Terminating(_, _, _) | ProviderExecutionPhase::Terminal(_)
        ) {
            return;
        }
        let transition = ProviderExecutionReducer::reduce(self.state.clone(), event);
        let (state, effects) = match transition {
            Ok(value) => value,
            Err(error) => {
                self.close_current_transport(true).await;
                self.finish_direct(
                    crate::wire::core_error_failure(error)
                        .with_request_id(self.request.id().clone()),
                );
                return;
            }
        };
        self.state = state;
        for effect in effects {
            match effect {
                ProviderExecutionEffect::Publish(public_event) => {
                    if let Err(failure) = self.publish(public_event) {
                        self.close_current_transport(true).await;
                        self.finish_direct(failure);
                        return;
                    }
                }
                ProviderExecutionEffect::BeginCleanup(should_cancel, generation) => {
                    if generation != self.state.generation() {
                        continue;
                    }
                    self.close_current_transport(should_cancel).await;
                    let terminal = ProviderExecutionReducer::reduce(
                        self.state.clone(),
                        ProviderExecutionEvent::CleanupCompleted,
                    );
                    let (state, terminal_effects) = match terminal {
                        Ok(value) => value,
                        Err(error) => {
                            self.finish_direct(
                                crate::wire::core_error_failure(error)
                                    .with_request_id(self.request.id().clone()),
                            );
                            return;
                        }
                    };
                    self.state = state;
                    for terminal_effect in terminal_effects {
                        if let ProviderExecutionEffect::Publish(ProviderTurnEvent::Terminal(
                            terminal,
                        )) = terminal_effect
                        {
                            self.release_supervisor();
                            let _accepted = self.sink.finish(terminal);
                        }
                    }
                }
                ProviderExecutionEffect::OpenTransport(_, _) => {
                    self.close_current_transport(true).await;
                    self.finish_direct(invariant_failure(
                        &self.request,
                        "provider cleanup attempted to reopen transport",
                    ));
                    return;
                }
            }
        }
    }

    fn finish_direct(&mut self, failure: ProviderFailure) {
        self.release_supervisor();
        let _ = self.sink.finish(ProviderTerminal::Failed(failure));
        self.control.mark_finished();
        release_execution(&self.supervisor, self.request.id(), self.execution_id, true);
    }

    fn release_supervisor(&mut self) {
        if self.supervisor_released {
            return;
        }
        release_execution(
            &self.supervisor,
            self.request.id(),
            self.execution_id,
            false,
        );
        self.supervisor_released = true;
    }
}

fn is_retryable(code: ProviderFailureCode) -> bool {
    matches!(
        code,
        ProviderFailureCode::RateLimited
            | ProviderFailureCode::ServerFailed
            | ProviderFailureCode::TransportFailed
            | ProviderFailureCode::TimedOut
    )
}

fn cancelled_failure(request: &ProviderTurnRequest) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::Cancelled, "provider request cancelled")
        .with_request_id(request.id().clone())
}

fn invariant_failure(request: &ProviderTurnRequest, message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::InternalInvariant, message)
        .with_request_id(request.id().clone())
}
