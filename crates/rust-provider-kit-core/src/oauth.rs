use std::fmt;

use crate::{ProviderCoreError, SensitiveValue};

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderPkce {
    code_verifier: SensitiveValue,
    code_challenge: String,
    state: String,
}
impl ProviderPkce {
    pub fn new(
        code_verifier: SensitiveValue,
        code_challenge: impl Into<String>,
        state: impl Into<String>,
    ) -> Result<Self, ProviderCoreError> {
        let code_challenge = code_challenge.into();
        let state = state.into();
        let verifier = code_verifier.expose();
        let unreserved =
            |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        let base64_url = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_');
        if !(43..=128).contains(&verifier.len())
            || !verifier.bytes().all(unreserved)
            || code_challenge.len() != 43
            || !code_challenge.bytes().all(base64_url)
            || !(32..=512).contains(&state.len())
            || !state.bytes().all(unreserved)
        {
            return Err(ProviderCoreError::invalid_value("PKCE material is invalid"));
        }
        Ok(Self {
            code_verifier,
            code_challenge,
            state,
        })
    }
    #[must_use]
    pub fn code_verifier(&self) -> &SensitiveValue {
        &self.code_verifier
    }
    #[must_use]
    pub fn code_challenge(&self) -> &str {
        &self.code_challenge
    }
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }
}
impl fmt::Debug for ProviderPkce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderPkce")
            .field("code_challenge", &self.code_challenge)
            .field("state", &"<redacted>")
            .finish()
    }
}
impl fmt::Display for ProviderPkce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ProviderPkce(code_challenge: {}, state: <redacted>)",
            self.code_challenge
        )
    }
}
