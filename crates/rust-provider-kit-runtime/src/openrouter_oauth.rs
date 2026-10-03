use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use http::Method;
use percent_encoding::percent_decode_str;
use rust_provider_kit_core::{
    BuiltInProviderId, ProviderAccountId, ProviderAccountRegistrationRequest,
    ProviderAuthorizationRequest, ProviderAuthorizationSession, ProviderClock, ProviderCoreError,
    ProviderCredentialMaterial, ProviderDataCollectionPolicy, ProviderFailure, ProviderFailureCode,
    ProviderJsonValue, ProviderPkce, ProviderRequestConstraints, SensitiveValue,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::http_transport::ProviderHttpTransport;
use crate::wire::{
    core_error_failure, http_failure, make_json_request, serde_to_json, transport_failure,
};

#[derive(Clone)]
pub struct OpenRouterOAuthRegistrationRequest {
    account_id: ProviderAccountId,
    label: String,
    callback_url: Url,
    pkce: ProviderPkce,
}

impl std::fmt::Debug for OpenRouterOAuthRegistrationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut callback_url = self.callback_url.clone();
        callback_url.set_query(None);
        callback_url.set_fragment(None);
        formatter
            .debug_struct("OpenRouterOAuthRegistrationRequest")
            .field("account_id", &self.account_id)
            .field("label", &self.label)
            .field("callback_url", &callback_url)
            .field("pkce", &self.pkce)
            .finish()
    }
}

impl OpenRouterOAuthRegistrationRequest {
    pub fn new(
        account_id: ProviderAccountId,
        label: impl Into<String>,
        callback_url: Url,
        pkce: ProviderPkce,
    ) -> Result<Self, ProviderCoreError> {
        let label = label.into();
        if label.is_empty()
            || label.len() > 128
            || label.trim() != label
            || label.chars().any(char::is_control)
        {
            return Err(ProviderCoreError::invalid_value(
                "OpenRouter account label is invalid",
            ));
        }
        validate_callback_base(&callback_url)?;
        Ok(Self {
            account_id,
            label,
            callback_url,
            pkce,
        })
    }

    #[must_use]
    pub fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn callback_url(&self) -> &Url {
        &self.callback_url
    }

    #[must_use]
    pub fn pkce(&self) -> &ProviderPkce {
        &self.pkce
    }
}

#[derive(Clone)]
pub(crate) struct OpenRouterOAuthBroker {
    transport: Arc<dyn ProviderHttpTransport>,
    clock: Arc<dyn ProviderClock>,
}

impl std::fmt::Debug for OpenRouterOAuthBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenRouterOAuthBroker")
    }
}

impl OpenRouterOAuthBroker {
    #[must_use]
    pub(crate) fn new(
        transport: Arc<dyn ProviderHttpTransport>,
        clock: Arc<dyn ProviderClock>,
    ) -> Self {
        Self { transport, clock }
    }

    pub(crate) async fn authorize(
        &self,
        request: OpenRouterOAuthRegistrationRequest,
        session: &dyn ProviderAuthorizationSession,
        cancellation: &CancellationToken,
    ) -> Result<ProviderAccountRegistrationRequest, ProviderFailure> {
        let callback = callback_with_state(request.callback_url(), request.pkce().state());
        let authorization_url = authorization_url(&callback, request.pkce())?;
        let authorization_request = ProviderAuthorizationRequest::new(
            BuiltInProviderId::open_router(),
            authorization_url,
            callback.scheme(),
            request.pkce().state(),
        )
        .map_err(core_error_failure)?;

        let mut cancellation_guard = AuthorizationCancellationGuard::new(session);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                session.cancel();
                return Err(ProviderFailure::new(
                    ProviderFailureCode::Cancelled,
                    "OpenRouter OAuth authorization was cancelled",
                ));
            }
            result = session.authorize(authorization_request) => result?,
        };
        cancellation_guard.disarm();

        let code = validate_callback(result.callback_url(), &callback, request.pkce().state())?;
        let credential = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(ProviderFailure::new(
                    ProviderFailureCode::Cancelled,
                    "OpenRouter OAuth exchange was cancelled",
                ));
            }
            result = self.exchange(&code, request.pkce()) => result?,
        };
        ProviderAccountRegistrationRequest::new(
            request.account_id().clone(),
            BuiltInProviderId::open_router(),
            request.label(),
            ProviderCredentialMaterial::OauthDerivedKey(credential),
            None,
        )
        .map_err(core_error_failure)
    }

    async fn exchange(
        &self,
        code: &str,
        pkce: &ProviderPkce,
    ) -> Result<SensitiveValue, ProviderFailure> {
        let body = serde_to_json(serde_json::json!({
            "code": code,
            "code_verifier": pkce.code_verifier().expose(),
            "code_challenge_method": "S256"
        }))?;
        let constraints = ProviderRequestConstraints::new(
            ProviderDataCollectionPolicy::Deny,
            true,
            true,
            60_000,
            1_024 * 1_024,
            1,
            None,
        )
        .map_err(core_error_failure)?;
        let endpoint = Url::parse("https://openrouter.ai/api/v1/auth/keys").map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "OpenRouter OAuth endpoint is invalid",
            )
        })?;
        let request =
            make_json_request(Method::POST, endpoint, BTreeMap::new(), &body, &constraints)?;
        let response = self
            .transport
            .send(request)
            .await
            .map_err(transport_failure)?;
        if !(200..300).contains(&response.status_code) {
            return Err(http_failure(&response, self.clock.now().await?));
        }
        let root = ProviderJsonValue::decode(&response.body).map_err(|_| {
            ProviderFailure::new(
                ProviderFailureCode::MalformedResponse,
                "OpenRouter OAuth response JSON is malformed",
            )
        })?;
        let credential = root
            .get("key")
            .and_then(ProviderJsonValue::as_str)
            .ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::MalformedResponse,
                    "OpenRouter OAuth exchange returned no credential",
                )
            })?;
        SensitiveValue::new(credential).map_err(core_error_failure)
    }
}

struct AuthorizationCancellationGuard<'a> {
    session: &'a dyn ProviderAuthorizationSession,
    armed: bool,
}

impl<'a> AuthorizationCancellationGuard<'a> {
    fn new(session: &'a dyn ProviderAuthorizationSession) -> Self {
        Self {
            session,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AuthorizationCancellationGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.cancel();
        }
    }
}

fn validate_callback_base(url: &Url) -> Result<(), ProviderCoreError> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(ProviderCoreError::invalid_value(
            "OpenRouter callback URL is invalid",
        ));
    }

    let query = parse_query_items(url.query().unwrap_or(""))
        .map_err(|_| ProviderCoreError::invalid_value("OpenRouter callback query is invalid"))?;
    if query
        .iter()
        .any(|item| matches!(item.name.as_str(), "code" | "error" | "state"))
    {
        return Err(ProviderCoreError::invalid_value(
            "OpenRouter callback URL reserves code, error, and state",
        ));
    }

    match url.scheme() {
        "https" if url.host_str().is_some() => Ok(()),
        "http"
            if url.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
            }) =>
        {
            Ok(())
        }
        "https" => Err(ProviderCoreError::invalid_value(
            "HTTPS callback URL has no host",
        )),
        "http" => Err(ProviderCoreError::invalid_value(
            "plain HTTP callback URL is restricted to loopback hosts",
        )),
        _ => Err(ProviderCoreError::invalid_value(
            "OpenRouter callback must use HTTPS or a loopback HTTP URL",
        )),
    }
}

fn callback_with_state(base: &Url, state: &str) -> Url {
    let mut url = base.clone();
    url.query_pairs_mut().append_pair("state", state);
    url
}

fn authorization_url(callback: &Url, pkce: &ProviderPkce) -> Result<Url, ProviderFailure> {
    let mut url = Url::parse("https://openrouter.ai/auth").map_err(|_| {
        ProviderFailure::new(
            ProviderFailureCode::InternalInvariant,
            "OpenRouter authorization endpoint is invalid",
        )
    })?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback.as_str())
        .append_pair("code_challenge", pkce.code_challenge())
        .append_pair("code_challenge_method", "S256");
    Ok(url)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryItem {
    name: String,
    value: Option<String>,
}

fn validate_callback(
    actual: &Url,
    expected: &Url,
    expected_state: &str,
) -> Result<String, ProviderFailure> {
    if !actual.scheme().eq_ignore_ascii_case(expected.scheme())
        || actual.host_str().map(str::to_ascii_lowercase)
            != expected.host_str().map(str::to_ascii_lowercase)
        || actual.port_or_known_default() != expected.port_or_known_default()
        || actual.path() != expected.path()
        || !actual.username().is_empty()
        || actual.password().is_some()
        || actual.fragment().is_some()
    {
        return Err(authentication(
            "OpenRouter OAuth callback origin or path does not match the request",
        ));
    }

    let actual_items = parse_query_items(actual.query().unwrap_or(""))?;
    let expected_items = parse_query_items(expected.query().unwrap_or(""))?;
    if exactly_one(&actual_items, "state").as_deref() != Some(expected_state) {
        return Err(authentication(
            "OpenRouter OAuth state does not match the request",
        ));
    }

    let code = exactly_one(&actual_items, "code");
    let error = exactly_one(&actual_items, "error");
    if code.is_some() == error.is_some() {
        return Err(authentication(
            "OpenRouter OAuth callback has an invalid terminal result",
        ));
    }

    let response_item = match &code {
        Some(value) => QueryItem {
            name: "code".into(),
            value: Some(value.clone()),
        },
        None => QueryItem {
            name: "error".into(),
            value: error.clone(),
        },
    };
    let mut expected_terminal = expected_items;
    expected_terminal.push(response_item);
    if multiset(&actual_items) != multiset(&expected_terminal) {
        return Err(authentication(
            "OpenRouter OAuth callback query does not match the request",
        ));
    }

    if let Some(error) = error {
        if error.is_empty() || error.len() > 1_024 || error.chars().any(char::is_control) {
            return Err(authentication(
                "OpenRouter authorization returned an invalid error",
            ));
        }
        return Err(authentication(&format!(
            "OpenRouter authorization failed: {error}"
        )));
    }

    let code =
        code.ok_or_else(|| authentication("OpenRouter OAuth callback has no authorization code"))?;
    if code.is_empty() || code.len() > 4_096 || code.chars().any(char::is_control) {
        return Err(authentication(
            "OpenRouter OAuth callback has no valid authorization code",
        ));
    }
    Ok(code)
}

fn parse_query_items(raw: &str) -> Result<Vec<QueryItem>, ProviderFailure> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::new();
    for raw_item in raw.split('&') {
        let (name, value) = match raw_item.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (raw_item, None),
        };
        if !valid_percent_encoding(name)
            || value.is_some_and(|value| !valid_percent_encoding(value))
        {
            return Err(authentication(
                "OpenRouter OAuth callback contains malformed percent encoding",
            ));
        }
        let name = percent_decode_str(name)
            .decode_utf8()
            .map_err(|_| authentication("OpenRouter OAuth callback query is not UTF-8"))?
            .into_owned();
        let value = value
            .map(|value| {
                percent_decode_str(value)
                    .decode_utf8()
                    .map(|value| value.into_owned())
                    .map_err(|_| authentication("OpenRouter OAuth callback query is not UTF-8"))
            })
            .transpose()?;
        output.push(QueryItem { name, value });
    }
    Ok(output)
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn exactly_one(items: &[QueryItem], name: &str) -> Option<String> {
    let mut matches = items.iter().filter(|item| item.name == name);
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    first.value.clone()
}

fn multiset(items: &[QueryItem]) -> HashMap<&QueryItem, usize> {
    let mut result = HashMap::new();
    for item in items {
        *result.entry(item).or_insert(0) += 1;
    }
    result
}

fn authentication(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::AuthenticationFailed, message)
}
