use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rust_provider_kit_core::{
    ProviderCoreError, ProviderFailure, ProviderFailureCode, ProviderPkce, SensitiveValue,
};
use sha2::{Digest, Sha256};

/// RFC 7636 S256 PKCE generator backed by the operating system CSPRNG.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPkceGenerator;

impl ProviderPkceGenerator {
    pub fn generate() -> Result<ProviderPkce, ProviderFailure> {
        let mut verifier_bytes = [0u8; 32];
        let mut state_bytes = [0u8; 32];
        getrandom::fill(&mut verifier_bytes).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "secure random generation failed",
            )
        })?;
        getrandom::fill(&mut state_bytes).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "secure random generation failed",
            )
        })?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        Self::make(&verifier, &state).map_err(|error| {
            ProviderFailure::new(ProviderFailureCode::InternalInvariant, error.message)
        })
    }

    pub fn make(verifier: &str, state: &str) -> Result<ProviderPkce, ProviderCoreError> {
        let digest = Sha256::digest(verifier.as_bytes());
        ProviderPkce::new(
            SensitiveValue::new(verifier)?,
            URL_SAFE_NO_PAD.encode(digest),
            state,
        )
    }
}
