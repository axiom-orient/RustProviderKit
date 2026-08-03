use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::{
    ProviderAccountPublicEvent, ProviderCoreError, ProviderFailure, ProviderFailureCode,
    ProviderTerminal, ProviderTurnEvent,
};

const DEFAULT_TURN_CAPACITY: usize = 64;
const DEFAULT_TEXT_BATCH_SCALARS: usize = 16_384;
const DEFAULT_ACCOUNT_CAPACITY: usize = 16;

/// Runtime-side result for the bounded mailbox producer.
///
/// This is public only because `rust-provider-kit-runtime` is a separate crate.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMailboxSendResult {
    Accepted,
    Coalesced,
    Overflow,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Reasoning,
    Text,
}

#[derive(Debug)]
struct DeltaBatch {
    kind: DeltaKind,
    text: String,
    scalar_count: usize,
}

impl DeltaBatch {
    fn new(kind: DeltaKind, text: String) -> Self {
        let scalar_count = text.chars().count();
        Self {
            kind,
            text,
            scalar_count,
        }
    }

    fn append(&mut self, text: &str, next_scalar_count: usize) {
        self.text.push_str(text);
        self.scalar_count = next_scalar_count;
    }

    fn into_event(self) -> ProviderTurnEvent {
        match self.kind {
            DeltaKind::Reasoning => ProviderTurnEvent::ReasoningDelta(self.text),
            DeltaKind::Text => ProviderTurnEvent::TextDelta(self.text),
        }
    }
}

#[derive(Debug)]
enum BufferedTurnEvent {
    Event(ProviderTurnEvent),
    Delta(DeltaBatch),
}

#[derive(Debug)]
struct TurnMailboxState {
    buffer: VecDeque<BufferedTurnEvent>,
    terminal_committed: bool,
    drained: bool,
    producer_count: usize,
}

#[derive(Debug)]
struct TurnMailbox {
    capacity: usize,
    maximum_text_scalars: usize,
    state: Mutex<TurnMailboxState>,
    notify: Notify,
}

impl TurnMailbox {
    fn open(capacity: usize, maximum_text_scalars: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            maximum_text_scalars,
            state: Mutex::new(TurnMailboxState {
                buffer: VecDeque::new(),
                terminal_committed: false,
                drained: false,
                producer_count: 1,
            }),
            notify: Notify::new(),
        })
    }

    fn failed(failure: ProviderFailure) -> Arc<Self> {
        Arc::new(Self {
            capacity: DEFAULT_TURN_CAPACITY,
            maximum_text_scalars: DEFAULT_TEXT_BATCH_SCALARS,
            state: Mutex::new(TurnMailboxState {
                buffer: VecDeque::from([BufferedTurnEvent::Event(ProviderTurnEvent::Terminal(
                    ProviderTerminal::Failed(failure),
                ))]),
                terminal_committed: true,
                drained: false,
                producer_count: 0,
            }),
            notify: Notify::new(),
        })
    }

    fn send(&self, event: ProviderTurnEvent) -> ProviderMailboxSendResult {
        let mut state = self.state.lock();
        if state.terminal_committed || state.drained {
            return ProviderMailboxSendResult::Closed;
        }
        if let ProviderTurnEvent::Terminal(terminal) = event {
            return if Self::commit_terminal(&mut state, terminal) {
                drop(state);
                self.notify.notify_one();
                ProviderMailboxSendResult::Accepted
            } else {
                ProviderMailboxSendResult::Closed
            };
        }
        let queued = match event {
            ProviderTurnEvent::ReasoningDelta(value) => Ok((DeltaKind::Reasoning, value)),
            ProviderTurnEvent::TextDelta(value) => Ok((DeltaKind::Text, value)),
            other => Err(other),
        };
        if let Ok((kind, text)) = queued {
            let incoming = text.chars().count();
            if incoming > 0
                && let Some(BufferedTurnEvent::Delta(batch)) = state.buffer.back_mut()
                && batch.kind == kind
                && let Some(next) = batch.scalar_count.checked_add(incoming)
                && next <= self.maximum_text_scalars
            {
                batch.append(&text, next);
                drop(state);
                self.notify.notify_one();
                return ProviderMailboxSendResult::Coalesced;
            }
            if state.buffer.len() >= self.capacity - 1 {
                return ProviderMailboxSendResult::Overflow;
            }
            state
                .buffer
                .push_back(BufferedTurnEvent::Delta(DeltaBatch::new(kind, text)));
        } else if let Err(event) = queued {
            if state.buffer.len() >= self.capacity - 1 {
                return ProviderMailboxSendResult::Overflow;
            }
            state.buffer.push_back(BufferedTurnEvent::Event(event));
        }
        drop(state);
        self.notify.notify_one();
        ProviderMailboxSendResult::Accepted
    }

    fn finish(&self, terminal: ProviderTerminal) -> bool {
        let mut state = self.state.lock();
        let accepted = Self::commit_terminal(&mut state, terminal);
        drop(state);
        if accepted {
            self.notify.notify_one();
        }
        accepted
    }

    fn commit_terminal(state: &mut TurnMailboxState, terminal: ProviderTerminal) -> bool {
        if state.terminal_committed || state.drained {
            return false;
        }
        state.terminal_committed = true;
        state
            .buffer
            .push_back(BufferedTurnEvent::Event(ProviderTurnEvent::Terminal(
                terminal,
            )));
        true
    }

    fn producer_cloned(&self) -> bool {
        let mut state = self.state.lock();
        match state.producer_count.checked_add(1) {
            Some(value) => {
                state.producer_count = value;
                true
            }
            None => {
                let failure = ProviderFailure::new(
                    ProviderFailureCode::InternalInvariant,
                    "provider event producer counter exhausted",
                );
                let accepted = Self::commit_terminal(&mut state, ProviderTerminal::Failed(failure));
                drop(state);
                if accepted {
                    self.notify.notify_one();
                }
                false
            }
        }
    }

    fn producer_dropped(&self) {
        let mut state = self.state.lock();
        if state.producer_count == 0 {
            let failure = ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "provider event producer counter underflow",
            );
            let accepted = Self::commit_terminal(&mut state, ProviderTerminal::Failed(failure));
            drop(state);
            if accepted {
                self.notify.notify_one();
            }
            return;
        }
        state.producer_count -= 1;
        let should_close = state.producer_count == 0 && !state.terminal_committed && !state.drained;
        if should_close {
            let failure = ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "provider event producer ended without a terminal event",
            );
            let accepted = Self::commit_terminal(&mut state, ProviderTerminal::Failed(failure));
            drop(state);
            if accepted {
                self.notify.notify_one();
            }
        }
    }
}

#[derive(Debug)]
pub struct ProviderEventStream {
    mailbox: Arc<TurnMailbox>,
    ended: bool,
}

/// Runtime-side producer for `ProviderEventStream`.
///
/// This is public only because `rust-provider-kit-runtime` is a separate crate.
#[doc(hidden)]
#[derive(Debug)]
pub struct ProviderEventSink {
    mailbox: Arc<TurnMailbox>,
    counted: bool,
}

impl Clone for ProviderEventSink {
    fn clone(&self) -> Self {
        let counted = self.mailbox.producer_cloned();
        Self {
            mailbox: Arc::clone(&self.mailbox),
            counted,
        }
    }
}

impl Drop for ProviderEventSink {
    fn drop(&mut self) {
        if self.counted {
            self.mailbox.producer_dropped();
        }
    }
}

impl ProviderEventStream {
    /// Creates the runtime-side bounded stream pair.
    #[doc(hidden)]
    pub fn make(
        capacity: usize,
        maximum_coalesced_text_scalars: usize,
    ) -> Result<(Self, ProviderEventSink), ProviderCoreError> {
        if capacity < 2 {
            return Err(ProviderCoreError::invalid_value(
                "provider event stream capacity must be at least two",
            ));
        }
        if maximum_coalesced_text_scalars == 0 {
            return Err(ProviderCoreError::invalid_value(
                "provider event text batch bound must be positive",
            ));
        }
        let mailbox = TurnMailbox::open(capacity, maximum_coalesced_text_scalars);
        Ok((
            Self {
                mailbox: Arc::clone(&mailbox),
                ended: false,
            },
            ProviderEventSink {
                mailbox,
                counted: true,
            },
        ))
    }

    /// Creates the runtime-side stream pair with product defaults.
    #[doc(hidden)]
    #[must_use]
    pub fn with_defaults() -> (Self, ProviderEventSink) {
        let mailbox = TurnMailbox::open(DEFAULT_TURN_CAPACITY, DEFAULT_TEXT_BATCH_SCALARS);
        (
            Self {
                mailbox: Arc::clone(&mailbox),
                ended: false,
            },
            ProviderEventSink {
                mailbox,
                counted: true,
            },
        )
    }

    #[doc(hidden)]
    #[must_use]
    pub fn failed(failure: ProviderFailure) -> Self {
        Self {
            mailbox: TurnMailbox::failed(failure),
            ended: false,
        }
    }

    pub async fn next(&mut self) -> Option<ProviderTurnEvent> {
        if self.ended {
            return None;
        }
        loop {
            let notified = self.mailbox.notify.notified();
            {
                let mut state = self.mailbox.state.lock();
                if let Some(buffered) = state.buffer.pop_front() {
                    let event = match buffered {
                        BufferedTurnEvent::Event(value) => value,
                        BufferedTurnEvent::Delta(batch) => batch.into_event(),
                    };
                    if event.is_terminal() {
                        state.drained = true;
                        self.ended = true;
                    }
                    return Some(event);
                }
                if state.terminal_committed || state.drained {
                    self.ended = true;
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl ProviderEventSink {
    #[doc(hidden)]
    #[must_use]
    pub fn send(&self, event: ProviderTurnEvent) -> ProviderMailboxSendResult {
        self.mailbox.send(event)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn finish(&self, terminal: ProviderTerminal) -> bool {
        self.mailbox.finish(terminal)
    }
}

#[derive(Debug)]
struct AccountMailboxState {
    buffer: VecDeque<ProviderAccountPublicEvent>,
    finished: bool,
    drained: bool,
    producer_count: usize,
}

#[derive(Debug)]
struct AccountMailbox {
    capacity: usize,
    state: Mutex<AccountMailboxState>,
    notify: Notify,
}

impl AccountMailbox {
    fn open(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            state: Mutex::new(AccountMailboxState {
                buffer: VecDeque::new(),
                finished: false,
                drained: false,
                producer_count: 1,
            }),
            notify: Notify::new(),
        })
    }

    fn failed(failure: ProviderFailure) -> Arc<Self> {
        Arc::new(Self {
            capacity: DEFAULT_ACCOUNT_CAPACITY,
            state: Mutex::new(AccountMailboxState {
                buffer: VecDeque::from([ProviderAccountPublicEvent::Failed(failure)]),
                finished: true,
                drained: false,
                producer_count: 0,
            }),
            notify: Notify::new(),
        })
    }

    fn send(&self, event: ProviderAccountPublicEvent) -> bool {
        let terminal = event.is_terminal();
        let mut state = self.state.lock();
        if state.finished || state.drained {
            return false;
        }
        let bound = if terminal {
            self.capacity
        } else {
            self.capacity - 1
        };
        if state.buffer.len() >= bound {
            return false;
        }
        state.buffer.push_back(event);
        if terminal {
            state.finished = true;
        }
        drop(state);
        self.notify.notify_one();
        true
    }

    fn producer_cloned(&self) -> bool {
        let mut state = self.state.lock();
        match state.producer_count.checked_add(1) {
            Some(value) => {
                state.producer_count = value;
                true
            }
            None => {
                if !state.finished && !state.drained {
                    state.finished = true;
                    state.buffer.truncate(self.capacity.saturating_sub(1));
                    state.buffer.push_back(ProviderAccountPublicEvent::Failed(
                        ProviderFailure::new(
                            ProviderFailureCode::InternalInvariant,
                            "provider account producer counter exhausted",
                        ),
                    ));
                    drop(state);
                    self.notify.notify_one();
                }
                false
            }
        }
    }

    fn producer_dropped(&self) {
        let mut state = self.state.lock();
        if state.producer_count == 0 {
            if !state.finished && !state.drained {
                state.finished = true;
                state
                    .buffer
                    .push_back(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                        ProviderFailureCode::InternalInvariant,
                        "provider account producer counter underflow",
                    )));
                drop(state);
                self.notify.notify_one();
            }
            return;
        }
        state.producer_count -= 1;
        let should_close = state.producer_count == 0 && !state.finished && !state.drained;
        if should_close {
            state.finished = true;
            state
                .buffer
                .push_back(ProviderAccountPublicEvent::Failed(ProviderFailure::new(
                    ProviderFailureCode::InternalInvariant,
                    "provider account producer ended without a terminal event",
                )));
            drop(state);
            self.notify.notify_one();
        }
    }
}

#[derive(Debug)]
pub struct ProviderAccountEventStream {
    mailbox: Arc<AccountMailbox>,
    ended: bool,
}

/// Runtime-side producer for `ProviderAccountEventStream`.
///
/// This is public only because `rust-provider-kit-runtime` is a separate crate.
#[doc(hidden)]
#[derive(Debug)]
pub struct ProviderAccountEventSink {
    mailbox: Arc<AccountMailbox>,
    counted: bool,
}

impl Clone for ProviderAccountEventSink {
    fn clone(&self) -> Self {
        let counted = self.mailbox.producer_cloned();
        Self {
            mailbox: Arc::clone(&self.mailbox),
            counted,
        }
    }
}

impl Drop for ProviderAccountEventSink {
    fn drop(&mut self) {
        if self.counted {
            self.mailbox.producer_dropped();
        }
    }
}

impl ProviderAccountEventStream {
    /// Creates the runtime-side bounded stream pair.
    #[doc(hidden)]
    pub fn make(capacity: usize) -> Result<(Self, ProviderAccountEventSink), ProviderCoreError> {
        if capacity < 2 {
            return Err(ProviderCoreError::invalid_value(
                "provider account event stream capacity must be at least two",
            ));
        }
        let mailbox = AccountMailbox::open(capacity);
        Ok((
            Self {
                mailbox: Arc::clone(&mailbox),
                ended: false,
            },
            ProviderAccountEventSink {
                mailbox,
                counted: true,
            },
        ))
    }

    /// Creates the runtime-side stream pair with product defaults.
    #[doc(hidden)]
    #[must_use]
    pub fn with_defaults() -> (Self, ProviderAccountEventSink) {
        let mailbox = AccountMailbox::open(DEFAULT_ACCOUNT_CAPACITY);
        (
            Self {
                mailbox: Arc::clone(&mailbox),
                ended: false,
            },
            ProviderAccountEventSink {
                mailbox,
                counted: true,
            },
        )
    }

    #[doc(hidden)]
    #[must_use]
    pub fn failed(failure: ProviderFailure) -> Self {
        Self {
            mailbox: AccountMailbox::failed(failure),
            ended: false,
        }
    }

    pub async fn next(&mut self) -> Option<ProviderAccountPublicEvent> {
        if self.ended {
            return None;
        }
        loop {
            let notified = self.mailbox.notify.notified();
            {
                let mut state = self.mailbox.state.lock();
                if let Some(event) = state.buffer.pop_front() {
                    if event.is_terminal() {
                        state.drained = true;
                        self.ended = true;
                    }
                    return Some(event);
                }
                if state.finished || state.drained {
                    self.ended = true;
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl ProviderAccountEventSink {
    #[doc(hidden)]
    #[must_use]
    pub fn send(&self, event: ProviderAccountPublicEvent) -> bool {
        self.mailbox.send(event)
    }
}
