use std::error::Error;
use std::sync::Arc;

use crate::loopback::{BrowserOpener, LoopbackAuthorizationSession};
use crate::loopback_callback::{HttpHeaderTerminatorScanner, LoopbackOAuthCallbackParser};
use crate::pkce::ProviderPkceGenerator;
use async_trait::async_trait;
use rust_provider_kit_core::{
    BuiltInProviderId, ProviderAuthorizationRequest, ProviderAuthorizationSession, ProviderFailure,
    ProviderFailureCode,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use url::Url;

#[test]
fn pkce_matches_rfc_7636_s256_vector_and_redacts_verifier() -> Result<(), Box<dyn Error>> {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let state = "abcdefghijklmnopqrstuvwxyzABCDEF";
    let pkce = ProviderPkceGenerator::make(verifier, state)?;
    assert_eq!(
        pkce.code_challenge(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(pkce.state(), state);
    assert!(!format!("{pkce:?}").contains(verifier));
    assert!(!format!("{pkce}").contains(verifier));
    Ok(())
}

#[test]
fn pkce_generation_is_bounded_and_unique() -> Result<(), Box<dyn Error>> {
    let first = ProviderPkceGenerator::generate()?;
    let second = ProviderPkceGenerator::generate()?;
    assert!((43..=128).contains(&first.code_verifier().expose().len()));
    assert_eq!(first.code_challenge().len(), 43);
    assert!((32..=512).contains(&first.state().len()));
    assert_ne!(
        first.code_verifier().expose(),
        second.code_verifier().expose()
    );
    assert_ne!(first.state(), second.state());
    Ok(())
}

#[test]
fn pkce_validation_rejects_invalid_material() {
    assert!(ProviderPkceGenerator::make("short", "abcdefghijklmnopqrstuvwxyzABCDEF").is_err());
    assert!(
        ProviderPkceGenerator::make("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "short",)
            .is_err()
    );
}

#[test]
fn authorization_debug_output_redacts_state_and_query() -> Result<(), Box<dyn Error>> {
    let request = ProviderAuthorizationRequest::new(
        BuiltInProviderId::open_router(),
        Url::parse("https://example.com/authorize?code_challenge=query-secret")?,
        "http",
        "state-secret",
    )?;
    let rendered = format!("{request:?}");
    assert!(!rendered.contains("query-secret"));
    assert!(!rendered.contains("state-secret"));
    Ok(())
}

#[test]
fn loopback_callback_parser_accepts_only_exact_origin_path_host_and_state()
-> Result<(), Box<dyn Error>> {
    let base = Url::parse("http://127.0.0.1:49321/oauth/callback")?;
    let valid = b"GET /oauth/callback?code=abc&state=expected HTTP/1.1\r\nHost: 127.0.0.1:49321\r\nConnection: close\r\n\r\n";
    let parsed = LoopbackOAuthCallbackParser::parse(
        valid,
        &base,
        "/oauth/callback",
        "expected",
        32 * 1_024,
    )?;
    assert_eq!(
        parsed
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .as_deref(),
        Some("abc")
    );

    let wrong_state =
        b"GET /oauth/callback?code=abc&state=wrong HTTP/1.1\r\nHost: 127.0.0.1:49321\r\n\r\n";
    assert!(
        LoopbackOAuthCallbackParser::parse(
            wrong_state,
            &base,
            "/oauth/callback",
            "expected",
            32 * 1_024
        )
        .is_err()
    );

    let duplicate_state = b"GET /oauth/callback?code=abc&state=expected&state=expected HTTP/1.1\r\nHost: 127.0.0.1:49321\r\n\r\n";
    assert!(
        LoopbackOAuthCallbackParser::parse(
            duplicate_state,
            &base,
            "/oauth/callback",
            "expected",
            32 * 1_024
        )
        .is_err()
    );

    let wrong_host =
        b"GET /oauth/callback?code=abc&state=expected HTTP/1.1\r\nHost: localhost:49321\r\n\r\n";
    assert!(
        LoopbackOAuthCallbackParser::parse(
            wrong_host,
            &base,
            "/oauth/callback",
            "expected",
            32 * 1_024
        )
        .is_err()
    );

    let malformed_percent =
        b"GET /oauth/callback?code=%GG&state=expected HTTP/1.1\r\nHost: 127.0.0.1:49321\r\n\r\n";
    assert!(
        LoopbackOAuthCallbackParser::parse(
            malformed_percent,
            &base,
            "/oauth/callback",
            "expected",
            32 * 1_024,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn header_terminator_scanner_handles_fragmentation_and_overlap_linearly()
-> Result<(), Box<dyn Error>> {
    let mut scanner = HttpHeaderTerminatorScanner::default();
    assert!(!scanner.feed(b"GET / HTTP/1.1\r\nHost: x\r")?);
    assert!(!scanner.feed(b"\n\r")?);
    assert!(scanner.feed(b"\n")?);
    let scanned = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".len();
    assert_eq!(scanner.scanned_byte_count(), scanned);
    assert!(scanner.feed(b"ignored after completion")?);
    assert_eq!(scanner.scanned_byte_count(), scanned);
    Ok(())
}

#[derive(Clone, Default)]
struct CallbackBrowserOpener {
    callback: Arc<Mutex<Option<Url>>>,
    state: Arc<Mutex<Option<String>>>,
}

impl CallbackBrowserOpener {
    async fn configure(&self, callback: Url, state: &str) {
        *self.callback.lock().await = Some(callback);
        *self.state.lock().await = Some(state.to_owned());
    }
}

#[async_trait]
impl BrowserOpener for CallbackBrowserOpener {
    async fn open(&self, _url: &Url) -> Result<(), ProviderFailure> {
        let callback = self.callback.lock().await.clone().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "test callback was not configured",
            )
        })?;
        let state = self.state.lock().await.clone().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "test callback state was not configured",
            )
        })?;
        let host = callback.host_str().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "callback host missing",
            )
        })?;
        let port = callback.port().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "callback port missing",
            )
        })?;
        let mut stream = TcpStream::connect((host, port)).await.map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "test callback connection failed",
            )
        })?;
        let request = format!(
            "GET {}?code=code-1&state={} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
            callback.path(),
            state,
            host,
            port
        );
        stream.write_all(request.as_bytes()).await.map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::AuthenticationFailed,
                "test callback write failed",
            )
        })?;
        Ok(())
    }
}

#[tokio::test]
async fn loopback_listener_binds_before_browser_and_finishes_once() -> Result<(), Box<dyn Error>> {
    let opener = CallbackBrowserOpener::default();
    let prepared = LoopbackAuthorizationSession::prepare_with_opener(
        "/oauth/callback",
        Arc::new(opener.clone()),
    )
    .await?;
    let state = "abcdefghijklmnopqrstuvwxyzABCDEF";
    opener.configure(prepared.callback_url.clone(), state).await;
    let request = ProviderAuthorizationRequest::new(
        BuiltInProviderId::open_router(),
        Url::parse("https://example.com/authorize")?,
        "http",
        state,
    )?;
    let result = prepared.session.authorize(request.clone()).await?;
    assert_eq!(result.callback_url().host_str(), Some("127.0.0.1"));
    assert_eq!(result.callback_url().path(), "/oauth/callback");
    assert_eq!(
        result
            .callback_url()
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .as_deref(),
        Some("code-1")
    );
    let second = prepared.session.authorize(request).await;
    assert!(second.is_err());
    Ok(())
}

#[tokio::test]
async fn pre_cancelled_loopback_session_fails_without_browser_work() -> Result<(), Box<dyn Error>> {
    let prepared = LoopbackAuthorizationSession::prepare("/oauth/callback").await?;
    prepared.session.cancel();
    let request = ProviderAuthorizationRequest::new(
        BuiltInProviderId::open_router(),
        Url::parse("https://example.com/authorize")?,
        "http",
        "abcdefghijklmnopqrstuvwxyzABCDEF",
    )?;
    let failure = prepared
        .session
        .authorize(request)
        .await
        .err()
        .ok_or("cancelled session accepted work")?;
    assert!(matches!(
        failure.code(),
        ProviderFailureCode::AuthenticationFailed | ProviderFailureCode::Cancelled
    ));
    Ok(())
}
