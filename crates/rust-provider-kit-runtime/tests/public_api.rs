use std::error::Error;
use std::sync::Arc;

use rust_provider_kit_core::{
    ProviderAccountId, ProviderCredentialStore, ProviderFailure, ProviderPkce, SensitiveValue,
};
use rust_provider_kit_runtime::{
    OpenRouterOAuthRegistrationRequest, ProviderRuntime, ProviderRuntimeOptions,
};
use url::Url;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn public_facade_is_send_sync() {
    assert_send_sync::<ProviderRuntime>();
    assert_send_sync::<ProviderRuntimeOptions>();
    assert_send_sync::<OpenRouterOAuthRegistrationRequest>();

    let _constructor: fn(
        Arc<dyn ProviderCredentialStore>,
    ) -> Result<ProviderRuntime, ProviderFailure> = ProviderRuntime::new;
    let _configured_constructor: fn(
        Arc<dyn ProviderCredentialStore>,
        ProviderRuntimeOptions,
    ) -> Result<ProviderRuntime, ProviderFailure> = ProviderRuntime::with_options;
}

#[test]
fn oauth_registration_request_validates_and_redacts_sensitive_context() -> Result<(), Box<dyn Error>>
{
    let pkce = ProviderPkce::new(
        SensitiveValue::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")?,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        "abcdefghijklmnopqrstuvwxyzABCDEF",
    )?;
    let request = OpenRouterOAuthRegistrationRequest::new(
        ProviderAccountId::new("openrouter-account")?,
        "OpenRouter",
        Url::parse("http://127.0.0.1:49152/oauth/openrouter?local=visible")?,
        pkce,
    )?;

    let rendered = format!("{request:?}");
    assert!(!rendered.contains("abcdefghijklmnopqrstuvwxyzABCDEF"));
    assert!(!rendered.contains("local=visible"));
    Ok(())
}
