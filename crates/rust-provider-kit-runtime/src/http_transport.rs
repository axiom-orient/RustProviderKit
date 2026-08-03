use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use http::Method;
use http::header::{HeaderName, HeaderValue};
use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};
use thiserror::Error;
use url::Url;

pub(crate) type ProviderByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ProviderTransportError>> + Send>>;

const DEFAULT_CHUNK_CAPACITY: usize = 64;
#[cfg(test)]
const DEFAULT_TIMEOUT_MILLISECONDS: u64 = 300_000;
const MINIMUM_TIMEOUT_MILLISECONDS: u64 = 1_000;
const MAXIMUM_TIMEOUT_MILLISECONDS: u64 = 3_600_000;
const MINIMUM_RESPONSE_BYTES: usize = 1_024;
const MAXIMUM_RESPONSE_BYTES: usize = 64 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ProviderTransportError {
    #[error("transport response exceeded its byte limit")]
    ResponseTooLarge,
    #[error("transport consumer backpressure exceeded")]
    BackpressureExceeded,
    #[error("transport timed out")]
    TimedOut,
    #[error("transport returned an invalid response")]
    InvalidResponse,
    #[error("transport failed")]
    Failed,
}

#[derive(Clone)]
pub(crate) struct ProviderHttpRequest {
    method: Method,
    url: Url,
    headers: BTreeMap<String, String>,
    body: Bytes,
    timeout_milliseconds: u64,
    maximum_response_bytes: usize,
    chunk_capacity: usize,
}

impl fmt::Debug for ProviderHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpRequest")
            .field("method", &self.method)
            .field("url", &redacted_url(&self.url))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_bytes", &self.body.len())
            .field("timeout_milliseconds", &self.timeout_milliseconds)
            .field("maximum_response_bytes", &self.maximum_response_bytes)
            .field("chunk_capacity", &self.chunk_capacity)
            .finish()
    }
}

impl ProviderHttpRequest {
    #[cfg(test)]
    pub(crate) fn new(
        method: Method,
        url: Url,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
        maximum_response_bytes: usize,
    ) -> Result<Self, ProviderFailure> {
        Self::with_limits(
            method,
            url,
            headers,
            body,
            DEFAULT_TIMEOUT_MILLISECONDS,
            maximum_response_bytes,
            DEFAULT_CHUNK_CAPACITY,
        )
    }

    pub(crate) fn with_timeout(
        method: Method,
        url: Url,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
        timeout_milliseconds: u64,
        maximum_response_bytes: usize,
    ) -> Result<Self, ProviderFailure> {
        Self::with_limits(
            method,
            url,
            headers,
            body,
            timeout_milliseconds,
            maximum_response_bytes,
            DEFAULT_CHUNK_CAPACITY,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_limits(
        method: Method,
        url: Url,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
        timeout_milliseconds: u64,
        maximum_response_bytes: usize,
        chunk_capacity: usize,
    ) -> Result<Self, ProviderFailure> {
        if !(MINIMUM_TIMEOUT_MILLISECONDS..=MAXIMUM_TIMEOUT_MILLISECONDS)
            .contains(&timeout_milliseconds)
        {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "HTTP timeout is invalid",
            ));
        }
        if !(MINIMUM_RESPONSE_BYTES..=MAXIMUM_RESPONSE_BYTES).contains(&maximum_response_bytes) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "HTTP response bound is invalid",
            ));
        }
        if !(2..=4_096).contains(&chunk_capacity) {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "HTTP chunk backlog bound is invalid",
            ));
        }
        validate_https_url(&url)?;
        validate_headers(&headers)?;
        Ok(Self {
            method,
            url,
            headers,
            body: Bytes::from(body),
            timeout_milliseconds,
            maximum_response_bytes,
            chunk_capacity,
        })
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn timeout_milliseconds(&self) -> u64 {
        self.timeout_milliseconds
    }

    #[must_use]
    pub(crate) fn maximum_response_bytes(&self) -> usize {
        self.maximum_response_bytes
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Method,
        Url,
        BTreeMap<String, String>,
        Bytes,
        u64,
        usize,
        usize,
    ) {
        (
            self.method,
            self.url,
            self.headers,
            self.body,
            self.timeout_milliseconds,
            self.maximum_response_bytes,
            self.chunk_capacity,
        )
    }
}

#[async_trait]
pub(crate) trait ProviderTransportControl: Send + Sync {
    fn cancel(&self);
    async fn wait_for_termination(&self);
}

pub(crate) struct ProviderHttpResponseControl {
    inner: Arc<dyn ProviderTransportControl>,
}

impl fmt::Debug for ProviderHttpResponseControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderHttpResponseControl")
    }
}

impl Drop for ProviderHttpResponseControl {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

impl ProviderHttpResponseControl {
    #[must_use]
    pub(crate) fn new(inner: Arc<dyn ProviderTransportControl>) -> Self {
        Self { inner }
    }

    pub(crate) fn cancel(&self) {
        self.inner.cancel();
    }

    pub(crate) async fn wait_for_termination(&self) {
        self.inner.wait_for_termination().await;
    }
}

pub(crate) struct ProviderHttpResponse {
    status_code: u16,
    headers: BTreeMap<String, String>,
    body: ProviderByteStream,
    control: ProviderHttpResponseControl,
}

impl fmt::Debug for ProviderHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpResponse")
            .field("status_code", &self.status_code)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ProviderHttpResponse {
    pub(crate) fn new(
        status_code: u16,
        headers: BTreeMap<String, String>,
        body: ProviderByteStream,
        control: ProviderHttpResponseControl,
    ) -> Result<Self, ProviderTransportError> {
        validate_response(status_code, &headers)?;
        Ok(Self {
            status_code,
            headers,
            body,
            control,
        })
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        u16,
        BTreeMap<String, String>,
        ProviderByteStream,
        ProviderHttpResponseControl,
    ) {
        (self.status_code, self.headers, self.body, self.control)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProviderHttpUnaryResponse {
    pub(crate) status_code: u16,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Vec<u8>,
}

impl fmt::Debug for ProviderHttpUnaryResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpUnaryResponse")
            .field("status_code", &self.status_code)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[async_trait]
pub(crate) trait ProviderHttpTransport: Send + Sync {
    async fn open(
        &self,
        request: ProviderHttpRequest,
    ) -> Result<ProviderHttpResponse, ProviderTransportError>;

    async fn send(
        &self,
        request: ProviderHttpRequest,
    ) -> Result<ProviderHttpUnaryResponse, ProviderTransportError> {
        let maximum = request.maximum_response_bytes();
        let response = self.open(request).await?;
        let (status_code, headers, mut stream, control) = response.into_parts();
        let result = collect_unary_body(&mut stream, maximum).await;
        if result.is_err() {
            control.cancel();
        }
        control.wait_for_termination().await;
        result.map(|body| ProviderHttpUnaryResponse {
            status_code,
            headers,
            body,
        })
    }
}

async fn collect_unary_body(
    stream: &mut ProviderByteStream,
    maximum: usize,
) -> Result<Vec<u8>, ProviderTransportError> {
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or(ProviderTransportError::ResponseTooLarge)?;
        if next > maximum {
            return Err(ProviderTransportError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_https_url(url: &Url) -> Result<(), ProviderFailure> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderFailure::new(
            ProviderFailureCode::InvalidRequest,
            "provider HTTP URL must be an absolute HTTPS URL without user information",
        ));
    }
    Ok(())
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), ProviderFailure> {
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "HTTP header name is invalid",
            )
        })?;
        HeaderValue::from_str(value).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InvalidRequest,
                "HTTP header value is invalid",
            )
        })?;
    }
    Ok(())
}

pub(crate) fn validate_response(
    status_code: u16,
    headers: &BTreeMap<String, String>,
) -> Result<(), ProviderTransportError> {
    if !(100..=599).contains(&status_code) {
        return Err(ProviderTransportError::InvalidResponse);
    }
    for (name, value) in headers {
        if HeaderName::from_bytes(name.as_bytes()).is_err() || HeaderValue::from_str(value).is_err()
        {
            return Err(ProviderTransportError::InvalidResponse);
        }
    }
    Ok(())
}

fn redacted_url(url: &Url) -> String {
    let mut value = url.clone();
    value.set_query(None);
    value.set_fragment(None);
    value.to_string()
}
