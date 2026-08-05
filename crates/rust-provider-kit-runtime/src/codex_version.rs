use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};

/// The `codex-cli` wire version this kit implements.
///
/// Codex identifies its client through the `originator`, `version`, and
/// `user-agent` request fields. The kit owns that identity, so it declares the
/// version it implements instead of discovering a separate host installation.
/// Bump this constant when the endpoint requires a newer client contract.
pub(crate) const CODEX_CLIENT_VERSION: &str = "0.144.1";

/// Environment override for the declared version.
///
/// This is explicit operator configuration, not host discovery. It never
/// inspects a filesystem, executable, or process to determine the value.
pub(crate) const CODEX_CLIENT_VERSION_OVERRIDE: &str = "ARA_PROVIDER_KIT_CODEX_CLIENT_VERSION";

/// Validate a caller-supplied client version before it is stored.
pub(crate) fn validate_client_version(value: &str) -> Result<(), ProviderFailure> {
    if valid_version(value) {
        return Ok(());
    }
    Err(ProviderFailure::new(
        ProviderFailureCode::InvalidRequest,
        "codex client version must be 1..=128 characters of [A-Za-z0-9.+-_]",
    ))
}

/// Resolve the client version this process declares to the Codex endpoint.
///
/// Precedence is explicit configuration, then the environment override, then
/// the version this kit implements. Configuration wins because it is the
/// caller's stated intent for this process; the environment remains an
/// operator escape hatch for a deployment that cannot be rebuilt.
pub(crate) fn codex_client_version(configured: Option<&str>) -> Result<String, ProviderFailure> {
    let environment = std::env::var(CODEX_CLIENT_VERSION_OVERRIDE).ok();
    codex_client_version_from_override(configured, environment.as_deref())
}

fn codex_client_version_from_override(
    configured: Option<&str>,
    environment: Option<&str>,
) -> Result<String, ProviderFailure> {
    if let Some(value) = configured {
        validate_client_version(value)?;
        return Ok(value.to_owned());
    }
    match environment {
        Some(value) if valid_version(value) => Ok(value.to_owned()),
        Some(_) => Err(ProviderFailure::new(
            ProviderFailureCode::InvalidRequest,
            format!("{CODEX_CLIENT_VERSION_OVERRIDE} is not a valid client version"),
        )),
        None => Ok(CODEX_CLIENT_VERSION.to_owned()),
    }
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_version_is_a_valid_wire_value() {
        assert!(valid_version(CODEX_CLIENT_VERSION));
    }

    #[test]
    fn resolution_needs_nothing_from_the_host() -> Result<(), ProviderFailure> {
        // No auth file, PATH entry, installed Codex, or process is needed.
        let resolved = codex_client_version_from_override(None, None)?;
        assert_eq!(resolved, CODEX_CLIENT_VERSION);
        Ok(())
    }

    #[test]
    fn precedence_is_explicit_config_then_environment_then_built_in() -> Result<(), ProviderFailure>
    {
        let configured = codex_client_version_from_override(Some("1.2.3"), Some("2.3.4"))?;
        assert_eq!(configured, "1.2.3");

        let environment = codex_client_version_from_override(None, Some("2.3.4"))?;
        assert_eq!(environment, "2.3.4");

        let built_in = codex_client_version_from_override(None, None)?;
        assert_eq!(built_in, CODEX_CLIENT_VERSION);
        Ok(())
    }

    #[test]
    fn an_invalid_configured_version_is_refused_at_the_boundary() {
        for hostile in ["", "1.0 0", "codex/1.0", &"9".repeat(129)] {
            assert!(
                validate_client_version(hostile).is_err(),
                "accepted invalid configured version {hostile:?}"
            );
            assert!(codex_client_version_from_override(Some(hostile), None).is_err());
        }
        assert!(validate_client_version(CODEX_CLIENT_VERSION).is_ok());
    }

    #[test]
    fn an_invalid_environment_override_is_refused() {
        for hostile in ["", "1.0 0", "1.0\n", "codex/1.0", &"9".repeat(129)] {
            assert!(codex_client_version_from_override(None, Some(hostile)).is_err());
        }
    }

    #[test]
    fn a_configured_version_reaches_the_declared_identity() -> Result<(), ProviderFailure> {
        use crate::adapter::ProviderAdapter;
        use crate::adapters::OpenAiResponsesAdapter;

        let adapter = OpenAiResponsesAdapter::codex(Some("7.7.7".to_owned()))?;
        let context = adapter.failure_context(400).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "configured Codex refusal context is missing",
            )
        })?;
        assert!(context.contains("7.7.7"));

        assert!(
            OpenAiResponsesAdapter::codex(Some("not a version".to_owned())).is_err(),
            "an invalid configured version must be refused when the adapter is built"
        );
        Ok(())
    }

    #[test]
    fn the_codex_adapter_names_the_declared_version_on_a_refusal() -> Result<(), ProviderFailure> {
        use crate::adapter::ProviderAdapter;
        use crate::adapters::{OpenAiResponsesAdapter, OpenAiResponsesKind};

        let codex = OpenAiResponsesAdapter::new(OpenAiResponsesKind::Codex)?;
        let context = codex.failure_context(400).ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "Codex refusal context is missing",
            )
        })?;
        assert!(context.contains(CODEX_CLIENT_VERSION));
        assert!(context.contains(CODEX_CLIENT_VERSION_OVERRIDE));
        assert!(codex.failure_context(500).is_none());
        assert!(codex.failure_context(200).is_none());

        let openai = OpenAiResponsesAdapter::new(OpenAiResponsesKind::OpenAi)?;
        assert!(openai.failure_context(400).is_none());
        Ok(())
    }
}
