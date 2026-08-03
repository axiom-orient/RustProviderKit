use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::http_transport::{
    ProviderHttpRequest, ProviderHttpResponse, ProviderHttpResponseControl, ProviderHttpTransport,
    ProviderTransportControl, ProviderTransportError, validate_response,
};

#[derive(Clone)]
pub(crate) struct ReqwestProviderHttpTransport {
    client: reqwest::Client,
}

impl fmt::Debug for ReqwestProviderHttpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReqwestProviderHttpTransport")
    }
}

impl ReqwestProviderHttpTransport {
    pub(crate) fn new() -> Result<Self, ProviderFailure> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderFailure::new(
                    ProviderFailureCode::TransportFailed,
                    "failed to initialize HTTP transport",
                )
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl ProviderHttpTransport for ReqwestProviderHttpTransport {
    async fn open(
        &self,
        request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError> {
        let (
            method,
            url,
            headers,
            body,
            timeout_milliseconds,
            maximum_response_bytes,
            chunk_capacity,
        ) = request.into_parts();
        let mut builder = self
            .client
            .request(method, url)
            .timeout(Duration::from_millis(timeout_milliseconds));
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| map_reqwest_error(&error))?;
        let status_code = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for (name, value) in response.headers() {
            let value = value
                .to_str()
                .map_err(|_| ProviderTransportError::InvalidResponse)?;
            headers.insert(name.as_str().to_ascii_lowercase(), value.to_owned());
        }
        validate_response(status_code, &headers)?;

        // One slot is reserved for the terminal transport error, matching the
        // public event stream's fail-closed bounded-backlog contract.
        let channel_capacity = chunk_capacity
            .checked_add(1)
            .ok_or(ProviderTransportError::BackpressureExceeded)?;
        let (sender, receiver) = mpsc::channel(channel_capacity);
        let cancellation = CancellationToken::new();
        let (finished, _finished_receiver) = watch::channel(false);
        let control = Arc::new(ReqwestOperationControl {
            cancellation: cancellation.clone(),
            finished: finished.clone(),
        });
        let mut upstream = response.bytes_stream();
        drop(tokio::spawn(async move {
            // The guard closes the join fence even if the transport worker is
            // cancelled or unwinds unexpectedly. Without it, callers waiting
            // to publish a terminal event could block forever.
            let _finish_signal = TransportFinishSignal(finished);
            let mut received = 0usize;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    value = upstream.next() => value,
                };
                let Some(value) = next else {
                    break;
                };
                let item = match value {
                    Ok(bytes) => {
                        let Some(next_total) = received.checked_add(bytes.len()) else {
                            let _result =
                                sender.try_send(Err(ProviderTransportError::ResponseTooLarge));
                            break;
                        };
                        if next_total > maximum_response_bytes {
                            let _result =
                                sender.try_send(Err(ProviderTransportError::ResponseTooLarge));
                            break;
                        }
                        received = next_total;
                        Ok(bytes)
                    }
                    Err(error) => Err(map_reqwest_error(&error)),
                };
                let terminal = item.is_err();
                if !terminal && sender.capacity() <= 1 {
                    let _result =
                        sender.try_send(Err(ProviderTransportError::BackpressureExceeded));
                    break;
                }
                if sender.try_send(item).is_err() || terminal {
                    break;
                }
            }
        }));

        ProviderHttpResponse::new(
            status_code,
            headers,
            Box::pin(ChannelByteStream { receiver }),
            ProviderHttpResponseControl::new(control),
        )
    }
}

#[derive(Debug)]
struct ChannelByteStream {
    receiver: mpsc::Receiver<Result<Bytes, ProviderTransportError>>,
}

impl Stream for ChannelByteStream {
    type Item = Result<Bytes, ProviderTransportError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

#[derive(Debug)]
struct ReqwestOperationControl {
    cancellation: CancellationToken,
    finished: watch::Sender<bool>,
}

#[derive(Debug)]
struct TransportFinishSignal(watch::Sender<bool>);

impl Drop for TransportFinishSignal {
    fn drop(&mut self) {
        let _previous = self.0.send_replace(true);
    }
}

#[async_trait]
impl ProviderTransportControl for ReqwestOperationControl {
    fn cancel(&self) {
        self.cancellation.cancel();
    }

    async fn wait_for_termination(&self) {
        let mut receiver = self.finished.subscribe();
        if *receiver.borrow() {
            return;
        }
        let _result = receiver.wait_for(|finished| *finished).await;
    }
}

fn map_reqwest_error(error: &reqwest::Error) -> ProviderTransportError {
    if error.is_timeout() {
        ProviderTransportError::TimedOut
    } else {
        ProviderTransportError::Failed
    }
}
