use std::error::Error;

use rust_provider_kit_platform::{
    LoopbackAuthorizationSession, PreparedLoopbackAuthorization, ProviderPkceGenerator,
};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn public_platform_types_are_send_sync() {
    assert_send_sync::<LoopbackAuthorizationSession>();
    assert_send_sync::<PreparedLoopbackAuthorization>();
}

#[test]
fn public_pkce_surface_matches_rfc_7636_s256() -> Result<(), Box<dyn Error>> {
    let pkce = ProviderPkceGenerator::make(
        "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        "abcdefghijklmnopqrstuvwxyzABCDEF",
    )?;
    assert_eq!(
        pkce.code_challenge(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    Ok(())
}
