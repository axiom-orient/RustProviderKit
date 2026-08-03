use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};
use url::Url;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HttpHeaderTerminatorScanner {
    matched: u8,
    scanned: usize,
}

impl HttpHeaderTerminatorScanner {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn scanned_byte_count(&self) -> usize {
        self.scanned
    }

    pub(crate) fn feed(&mut self, data: &[u8]) -> Result<bool, ProviderFailure> {
        if self.matched == 4 {
            return Ok(true);
        }
        for &byte in data {
            self.scanned = self.scanned.checked_add(1).ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureCode::ResponseTooLarge,
                    "loopback callback scan count overflow",
                )
            })?;
            self.matched = match (self.matched, byte) {
                (0, b'\r') => 1,
                (1, b'\n') => 2,
                (1, b'\r') => 1,
                (2, b'\r') => 3,
                (3, b'\n') => {
                    self.matched = 4;
                    return Ok(true);
                }
                (_, b'\r') => 1,
                _ => 0,
            };
        }
        Ok(false)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LoopbackOAuthCallbackParser;

impl LoopbackOAuthCallbackParser {
    pub(crate) fn parse(
        data: &[u8],
        base_url: &Url,
        callback_path: &str,
        expected_state: &str,
        maximum_bytes: usize,
    ) -> Result<Url, ProviderFailure> {
        validate_context(data, base_url, callback_path, expected_state, maximum_bytes)?;

        let request = std::str::from_utf8(data)
            .map_err(|_| authentication("loopback OAuth callback is not UTF-8"))?;
        let header_end = request
            .find("\r\n\r\n")
            .ok_or_else(|| authentication("loopback OAuth callback headers are incomplete"))?;
        let mut lines = request[..header_end].split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| authentication("loopback OAuth request line is missing"))?;
        let request_target = parse_request_line(request_line, maximum_bytes)?;
        validate_host_header(lines, base_url)?;

        let target = base_url
            .join(request_target)
            .map_err(|_| authentication("loopback OAuth request target is invalid"))?;
        if target.scheme() != "http"
            || target.host_str() != Some("127.0.0.1")
            || target.port() != base_url.port()
            || target.path() != callback_path
            || target.fragment().is_some()
            || !target.username().is_empty()
            || target.password().is_some()
        {
            return Err(authentication(
                "loopback OAuth callback origin or path is invalid",
            ));
        }

        let mut state_count = 0usize;
        let mut state_matches = false;
        for (name, value) in target.query_pairs() {
            if name == "state" {
                state_count = state_count.checked_add(1).ok_or_else(|| {
                    authentication("loopback OAuth callback state count overflow")
                })?;
                state_matches = value == expected_state;
            }
        }
        if state_count != 1 || !state_matches {
            return Err(authentication("loopback OAuth callback state is invalid"));
        }
        Ok(target)
    }
}

fn validate_context(
    data: &[u8],
    base_url: &Url,
    callback_path: &str,
    expected_state: &str,
    maximum_bytes: usize,
) -> Result<(), ProviderFailure> {
    if maximum_bytes == 0
        || data.len() > maximum_bytes
        || expected_state.is_empty()
        || expected_state.len() > 512
        || base_url.scheme() != "http"
        || base_url.host_str() != Some("127.0.0.1")
        || base_url.port().is_none()
        || base_url.path() != callback_path
        || base_url.query().is_some()
        || base_url.fragment().is_some()
        || !base_url.username().is_empty()
        || base_url.password().is_some()
    {
        return Err(authentication("loopback OAuth callback is malformed"));
    }
    Ok(())
}

fn parse_request_line(request_line: &str, maximum_bytes: usize) -> Result<&str, ProviderFailure> {
    let mut fields = request_line.split_ascii_whitespace();
    let method = fields.next();
    let target = fields.next();
    let version = fields.next();
    if method != Some("GET")
        || !matches!(version, Some("HTTP/1.0" | "HTTP/1.1"))
        || fields.next().is_some()
    {
        return Err(authentication(
            "loopback OAuth callback origin or path is invalid",
        ));
    }
    let target = target
        .ok_or_else(|| authentication("loopback OAuth callback origin or path is invalid"))?;
    if target.len() > maximum_bytes || !has_valid_percent_encoding(target) {
        return Err(authentication(
            "loopback OAuth callback origin or path is invalid",
        ));
    }
    Ok(target)
}

fn validate_host_header<'a>(
    lines: impl Iterator<Item = &'a str>,
    base_url: &Url,
) -> Result<(), ProviderFailure> {
    let mut host: Option<&str> = None;
    for line in lines {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            return Err(authentication("loopback OAuth HTTP header is malformed"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(authentication("loopback OAuth HTTP header is malformed"));
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value.contains('\0')
        {
            return Err(authentication("loopback OAuth HTTP header is malformed"));
        }
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return Err(authentication("loopback OAuth Host header is invalid"));
            }
            host = Some(value.trim());
        }
    }

    let port = base_url
        .port()
        .ok_or_else(|| authentication("loopback OAuth callback port is unavailable"))?;
    let expected_host = format!("127.0.0.1:{port}");
    if !host.is_some_and(|value| value.eq_ignore_ascii_case(&expected_host)) {
        return Err(authentication("loopback OAuth Host header is invalid"));
    }
    Ok(())
}

fn authentication(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::AuthenticationFailed, message)
}

fn has_valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(first) = bytes.get(index + 1) else {
                return false;
            };
            let Some(second) = bytes.get(index + 2) else {
                return false;
            };
            if !first.is_ascii_hexdigit() || !second.is_ascii_hexdigit() {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}
