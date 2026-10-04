/*
File: crates/ms-ai-api/src/target.rs

Purpose:
Where the requests of one connection go: the service plus, for a compatible service, its
validated base URL. Pure and target-neutral (no `genai`), so the URL rules are unit-tested and
the connection state can validate before it queues a request.

Key structures:
- AiApiTarget

Key functions:
- AiApiTarget::new()
- normalize_base_url()
- normalize_authority()  (private: host / IPv6 literal / port validation)

Notes:
`genai` appends the endpoint path to the base URL (`models`, `chat/completions` for the OpenAI
adapter via `Url::join`, `messages` for the Anthropic adapter via plain concatenation), so the
normalized URL always ends with `/`; a URL without a path gets the conventional `/v1/` that
llama.cpp, vLLM, Ollama and LM Studio serve both protocols under. The authority is validated by
hand (no URL-parser dependency) to the subset `genai`'s `Url::parse` accepts, minus userinfo; the
normalized form doubles as the credential-store user-name suffix (`keys`), hence the lowercased
scheme and host.
*/

use crate::error::AiApiError;
use crate::service::AiApiService;

/// The validated destination of a connection's requests. For a hosted service the provider's
/// official endpoint is used and `endpoint()` is `None`; a compatible service always carries
/// its normalized base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiApiTarget {
    service: AiApiService,
    endpoint: Option<String>,
}

impl AiApiTarget {
    /// The target of `service`. `base_url` is validated and normalized (`normalize_base_url`)
    /// only when the service uses one, and ignored otherwise, so a URL left over from a
    /// compatible service never redirects a hosted provider.
    ///
    /// # Errors
    /// For a compatible service: `BaseUrlMissing` for a blank URL, `BaseUrlInvalid` for a
    /// malformed one. Never fails for a hosted service.
    pub fn new(service: AiApiService, base_url: &str) -> Result<Self, AiApiError> {
        let endpoint = if service.uses_base_url() {
            if base_url.trim().is_empty() {
                return Err(AiApiError::BaseUrlMissing { service });
            }
            Some(normalize_base_url(base_url)?)
        } else {
            None
        };
        Ok(Self { service, endpoint })
    }

    /// The service requests are sent for (adapter, credential-store user name, labels).
    #[must_use]
    pub fn service(&self) -> AiApiService {
        self.service
    }

    /// The normalized base URL (ending with `/`) of a compatible service; `None` for a hosted
    /// service, which uses `genai`'s default endpoint.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
}

/// Normalizes a user-typed server address into the base URL `genai` appends endpoint paths to:
/// trims it, requires an `http://` or `https://` scheme (case-insensitive) and a valid authority,
/// rejects whitespace, `?`, `#` (a query or fragment would end up in front of the appended path)
/// and `\`, lowercases the scheme and host (the path keeps its case), appends `/v1/` when the
/// URL has no path (or only `/`), and otherwise guarantees a trailing `/`.
///
/// A valid authority is a non-empty host (a name / IPv4 address of ASCII letters, digits, `-`,
/// `.`, `_`, or a bracketed IPv6 literal such as `[::1]`) with an optional numeric port in
/// `0..=65535`. Userinfo (`user@host`, `user:pw@host`) is rejected: a credential in the URL
/// would be persisted in plaintext settings and logged with listing failures.
///
/// `http://127.0.0.1:8080` -> `http://127.0.0.1:8080/v1/`;
/// `https://example.com/api/v1` -> `https://example.com/api/v1/`.
///
/// # Errors
/// `BaseUrlInvalid` (carrying the trimmed input) for every rejected form, including a blank one.
pub fn normalize_base_url(raw: &str) -> Result<String, AiApiError> {
    let trimmed = raw.trim();
    let invalid = || AiApiError::BaseUrlInvalid { url: trimmed.to_string() };
    let lower = trimmed.to_ascii_lowercase();
    let scheme = if lower.starts_with("https://") {
        "https://"
    } else if lower.starts_with("http://") {
        "http://"
    } else {
        return Err(invalid());
    };
    if trimmed.chars().any(|ch| ch.is_whitespace() || matches!(ch, '?' | '#' | '\\')) {
        return Err(invalid());
    }
    // The scheme prefix is ASCII, so its length is a char boundary of `trimmed`.
    let rest = &trimmed[scheme.len()..];
    let (authority, path) = rest.find('/').map_or((rest, ""), |slash| rest.split_at(slash));
    let authority = normalize_authority(authority).ok_or_else(invalid)?;
    if path.is_empty() || path == "/" {
        return Ok(format!("{scheme}{authority}/v1/"));
    }
    if path.ends_with('/') {
        Ok(format!("{scheme}{authority}{path}"))
    } else {
        Ok(format!("{scheme}{authority}{path}/"))
    }
}

/// Validates `host[:port]` (see `normalize_base_url`) and returns it with the host lowercased;
/// `None` for an empty host, userinfo, a malformed IPv6 literal or a non-numeric / out-of-range
/// port.
fn normalize_authority(authority: &str) -> Option<String> {
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        // IPv6 literal: everything up to `]` is the address, then an optional `:port`.
        let (address, after) = bracketed.split_once(']')?;
        if address.is_empty() || !address.chars().all(|ch| ch.is_ascii_hexdigit() || matches!(ch, ':' | '.')) {
            return None;
        }
        let port = if after.is_empty() { None } else { Some(after.strip_prefix(':')?) };
        (&authority[..address.len() + 2], port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let host_valid = host.starts_with('[') || (!host.is_empty() && host.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_')));
    if !host_valid {
        return None;
    }
    let host = host.to_ascii_lowercase();
    match port {
        None => Some(host),
        Some(port) => {
            // `u16::from_str` would accept a leading `+`; a port is digits only.
            if port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            port.parse::<u16>().ok().map(|port| format!("{host}:{port}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AiApiTarget, normalize_base_url};
    use crate::error::AiApiError;
    use crate::service::AiApiService;

    #[test]
    fn bare_host_gets_the_v1_path() {
        assert_eq!(normalize_base_url("http://127.0.0.1:8080").ok().as_deref(), Some("http://127.0.0.1:8080/v1/"));
        assert_eq!(normalize_base_url("  http://localhost:8080/  ").ok().as_deref(), Some("http://localhost:8080/v1/"));
        assert_eq!(normalize_base_url("HTTPS://Example.com").ok().as_deref(), Some("https://example.com/v1/"));
    }

    #[test]
    fn explicit_path_is_kept_with_a_trailing_slash() {
        assert_eq!(normalize_base_url("http://127.0.0.1:8080/v1").ok().as_deref(), Some("http://127.0.0.1:8080/v1/"));
        assert_eq!(normalize_base_url("https://example.com/api/v1/").ok().as_deref(), Some("https://example.com/api/v1/"));
        assert_eq!(normalize_base_url("https://example.com/proxy/openai").ok().as_deref(), Some("https://example.com/proxy/openai/"));
        // Scheme and host are case-insensitive and lowercased; the path keeps its case.
        assert_eq!(normalize_base_url("HTTP://LocalHost:8080/API/V1").ok().as_deref(), Some("http://localhost:8080/API/V1/"));
    }

    #[test]
    fn ipv6_literals_and_ports_are_accepted() {
        assert_eq!(normalize_base_url("http://[::1]:8080").ok().as_deref(), Some("http://[::1]:8080/v1/"));
        assert_eq!(normalize_base_url("http://[FE80::1]/v1").ok().as_deref(), Some("http://[fe80::1]/v1/"));
        assert_eq!(normalize_base_url("http://[::ffff:127.0.0.1]").ok().as_deref(), Some("http://[::ffff:127.0.0.1]/v1/"));
        assert_eq!(normalize_base_url("http://my_host-1.lan:65535/").ok().as_deref(), Some("http://my_host-1.lan:65535/v1/"));
    }

    #[test]
    fn malformed_authorities_are_rejected() {
        for raw in [
            "http://:8080",
            "http://:8080/v1",
            "http://host:abc",
            "http://host:",
            "http://host:+80",
            "http://host:65536",
            "http://host:99999999999",
            "http://host:80:90",
            "http://host\\v1",
            "http://host/v1\\models",
            "http://user:pw@host",
            "http://user@host:8080/v1",
            "http://@host",
            "http://[::1",
            "http://[]:8080",
            "http://[::1]x",
            "http://[::1]:",
            "http://[zz::1]",
            "http://ho%st",
        ] {
            assert!(matches!(normalize_base_url(raw), Err(AiApiError::BaseUrlInvalid { .. })), "{raw:?}");
        }
    }

    #[test]
    fn malformed_urls_are_rejected() {
        for raw in ["", "   ", "127.0.0.1:8080", "ftp://host/", "http://", "http:///v1", "http://host/v1?x=1", "http://host/#frag", "http://ho st/v1"] {
            assert!(matches!(normalize_base_url(raw), Err(AiApiError::BaseUrlInvalid { .. })), "{raw:?}");
        }
    }

    #[test]
    fn compatible_target_requires_a_valid_url() {
        assert!(matches!(AiApiTarget::new(AiApiService::OpenAiCompatible, "  "), Err(AiApiError::BaseUrlMissing { service: AiApiService::OpenAiCompatible })));
        assert!(matches!(AiApiTarget::new(AiApiService::AnthropicCompatible, "localhost"), Err(AiApiError::BaseUrlInvalid { .. })));
        let target = AiApiTarget::new(AiApiService::AnthropicCompatible, "http://127.0.0.1:8080").ok();
        assert_eq!(target.as_ref().and_then(AiApiTarget::endpoint), Some("http://127.0.0.1:8080/v1/"));
        assert_eq!(target.map(|target| target.service()), Some(AiApiService::AnthropicCompatible));
    }

    #[test]
    fn hosted_target_ignores_the_url() {
        let target = AiApiTarget::new(AiApiService::OpenAi, "http://127.0.0.1:8080").ok();
        assert_eq!(target.as_ref().map(AiApiTarget::endpoint), Some(None));
        assert!(AiApiTarget::new(AiApiService::Groq, "not a url").is_ok());
    }
}
