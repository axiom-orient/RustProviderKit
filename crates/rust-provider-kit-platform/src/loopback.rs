use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use rust_provider_kit_core::{
    ProviderAuthorizationRequest, ProviderAuthorizationResult, ProviderAuthorizationSession,
    ProviderCoreError, ProviderFailure, ProviderFailureCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::loopback_callback::{HttpHeaderTerminatorScanner, LoopbackOAuthCallbackParser};

const MAXIMUM_REQUEST_BYTES: usize = 32 * 1_024;
const MAXIMUM_CONCURRENT_CONNECTIONS: usize = 8;
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(300);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

#[async_trait]
pub(crate) trait BrowserOpener: Send + Sync {
    async fn open(&self, url: &Url) -> Result<(), ProviderFailure>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SystemBrowserOpener;

#[async_trait]
impl BrowserOpener for SystemBrowserOpener {
    async fn open(&self, url: &Url) -> Result<(), ProviderFailure> {
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("open");
            command.arg(url.as_str());
            command
        };
        #[cfg(target_os = "linux")]
        let mut command = {
            let mut command = Command::new("xdg-open");
            command.arg(url.as_str());
            command
        };
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("rundll32");
            command.args(["url.dll,FileProtocolHandler", url.as_str()]);
            command
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        return Err(ProviderFailure::new(
            ProviderFailureCode::ProviderUnsupported,
            "system browser launch is unsupported on this platform",
        ));

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            command.kill_on_drop(true);
            let status = tokio::time::timeout(Duration::from_secs(10), command.status())
                .await
                .map_err(|_| {
                    ProviderFailure::new(
                        ProviderFailureCode::TimedOut,
                        "system browser launch timed out",
                    )
                })?
                .map_err(|_| {
                    ProviderFailure::new(
                        ProviderFailureCode::AuthenticationFailed,
                        "system browser could not be launched",
                    )
                })?;
            if !status.success() {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::AuthenticationFailed,
                    "system browser launch failed",
                ));
            }
            Ok(())
        }
    }
}

pub struct PreparedLoopbackAuthorization {
    pub session: Arc<LoopbackAuthorizationSession>,
    pub callback_url: Url,
}

impl std::fmt::Debug for PreparedLoopbackAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedLoopbackAuthorization")
            .field("callback_url", &self.callback_url)
            .finish_non_exhaustive()
    }
}

pub struct LoopbackAuthorizationSession {
    callback_url: Url,
    callback_path: String,
    listener: Mutex<Option<Arc<TcpListener>>>,
    opener: Arc<dyn BrowserOpener>,
    cancellation: CancellationToken,
    active: AtomicBool,
    finished: AtomicBool,
}

impl std::fmt::Debug for LoopbackAuthorizationSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoopbackAuthorizationSession")
            .field("callback_url", &self.callback_url)
            .field("callback_path", &self.callback_path)
            .field("active", &self.active.load(Ordering::Acquire))
            .field("finished", &self.finished.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl LoopbackAuthorizationSession {
    pub async fn prepare(
        callback_path: &str,
    ) -> Result<PreparedLoopbackAuthorization, ProviderFailure> {
        Self::prepare_with_opener(callback_path, Arc::new(SystemBrowserOpener)).await
    }

    pub(crate) async fn prepare_with_opener(
        callback_path: &str,
        opener: Arc<dyn BrowserOpener>,
    ) -> Result<PreparedLoopbackAuthorization, ProviderFailure> {
        validate_callback_path(callback_path).map_err(|error| {
            ProviderFailure::new(ProviderFailureCode::InvalidRequest, error.message)
        })?;
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(|_| authentication("loopback OAuth listener could not be created"))?;
        let address = listener
            .local_addr()
            .map_err(|_| authentication("loopback OAuth listener address is unavailable"))?;
        let callback_url = Url::parse(&format!(
            "http://127.0.0.1:{}{}",
            address.port(),
            callback_path
        ))
        .map_err(|_| authentication("loopback OAuth callback URL is invalid"))?;
        let session = Arc::new(Self {
            callback_url: callback_url.clone(),
            callback_path: callback_path.to_owned(),
            listener: Mutex::new(Some(Arc::new(listener))),
            opener,
            cancellation: CancellationToken::new(),
            active: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        Ok(PreparedLoopbackAuthorization {
            session,
            callback_url,
        })
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.active.store(false, Ordering::Release);
        self.listener.lock().take();
    }
}

struct AuthorizationActivity<'a> {
    session: &'a LoopbackAuthorizationSession,
}

impl Drop for AuthorizationActivity<'_> {
    fn drop(&mut self) {
        self.session.finish();
    }
}

#[async_trait]
impl ProviderAuthorizationSession for LoopbackAuthorizationSession {
    async fn authorize(
        &self,
        request: ProviderAuthorizationRequest,
    ) -> Result<ProviderAuthorizationResult, ProviderFailure> {
        if self.finished.load(Ordering::Acquire)
            || self
                .active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(authentication(
                "loopback OAuth authorization is unavailable or already active",
            ));
        }
        let _activity = AuthorizationActivity { session: self };
        if request.callback_scheme() != "http" {
            return Err(authentication(
                "loopback OAuth authorization requires an HTTP callback",
            ));
        }
        let listener = {
            self.listener
                .lock()
                .as_ref()
                .cloned()
                .ok_or_else(|| authentication("loopback OAuth listener is closed"))?
        };
        if self.cancellation.is_cancelled() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::Cancelled,
                "loopback OAuth authorization was cancelled",
            ));
        }
        self.opener.open(request.authorization_url()).await?;

        let deadline = tokio::time::sleep(AUTHORIZATION_TIMEOUT);
        tokio::pin!(deadline);
        let mut connections = JoinSet::new();
        let result = loop {
            let can_accept = connections.len() < MAXIMUM_CONCURRENT_CONNECTIONS;
            let can_join = !connections.is_empty();
            tokio::select! {
                _ = self.cancellation.cancelled() => {
                    break Err(ProviderFailure::new(
                        ProviderFailureCode::Cancelled,
                        "loopback OAuth authorization was cancelled",
                    ));
                }
                _ = &mut deadline => {
                    break Err(ProviderFailure::new(
                        ProviderFailureCode::TimedOut,
                        "loopback OAuth authorization timed out",
                    ));
                }
                accepted = listener.accept(), if can_accept => {
                    let Ok((stream, _)) = accepted else {
                        break Err(authentication("loopback OAuth listener failed"));
                    };
                    let base = self.callback_url.clone();
                    let path = self.callback_path.clone();
                    let state = request.state().to_owned();
                    connections.spawn(async move {
                        handle_connection(stream, base, path, state).await
                    });
                }
                joined = connections.join_next(), if can_join => {
                    if let Some(Ok(Ok(Some(callback)))) = joined {
                        break Ok(callback);
                    }
                }
            }
        };
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        result.map(ProviderAuthorizationResult::new)
    }

    fn cancel(&self) {
        self.cancellation.cancel();
        self.finish();
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    base_url: Url,
    callback_path: String,
    expected_state: String,
) -> Result<Option<Url>, ProviderFailure> {
    let result = read_headers(&mut stream).await;
    let parsed = match result {
        Ok(data) => LoopbackOAuthCallbackParser::parse(
            &data,
            &base_url,
            &callback_path,
            &expected_state,
            MAXIMUM_REQUEST_BYTES,
        ),
        Err(failure) => Err(failure),
    };
    match parsed {
        Ok(callback) => {
            send_response(
                &mut stream,
                "200 OK",
                "RustProviderKit authorization completed. You can close this window.",
            )
            .await;
            Ok(Some(callback))
        }
        Err(_) => {
            send_response(
                &mut stream,
                "400 Bad Request",
                "Authorization callback was rejected.",
            )
            .await;
            Ok(None)
        }
    }
}

async fn read_headers(stream: &mut TcpStream) -> Result<Vec<u8>, ProviderFailure> {
    tokio::time::timeout(HEADER_TIMEOUT, async {
        let mut buffer = Vec::new();
        let mut scanner = HttpHeaderTerminatorScanner::default();
        let mut chunk = [0u8; 4 * 1_024];
        loop {
            let count = stream
                .read(&mut chunk)
                .await
                .map_err(|_| authentication("loopback callback could not be read"))?;
            if count == 0 {
                return Err(authentication(
                    "loopback callback ended before the HTTP headers completed",
                ));
            }
            let next = buffer.len().checked_add(count).ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "loopback callback request size overflow",
                )
            })?;
            if next > MAXIMUM_REQUEST_BYTES {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "loopback callback request exceeds its size limit",
                ));
            }
            buffer.extend_from_slice(&chunk[..count]);
            if scanner.feed(&chunk[..count])? {
                return Ok(buffer);
            }
        }
    })
    .await
    .map_err(|_| {
        ProviderFailure::new(
            ProviderFailureCode::TimedOut,
            "loopback callback HTTP headers timed out",
        )
    })?
}

async fn send_response(stream: &mut TcpStream, status: &str, body: &str) {
    let escaped = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>RustProviderKit</title><p>{escaped}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn validate_callback_path(path: &str) -> Result<(), ProviderCoreError> {
    if !path.starts_with('/')
        || path.len() > 512
        || path.contains('?')
        || path.contains('#')
        || path.chars().any(char::is_control)
    {
        return Err(ProviderCoreError::invalid_value(
            "loopback callback path is invalid",
        ));
    }
    Ok(())
}

fn authentication(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::AuthenticationFailed, message)
}
