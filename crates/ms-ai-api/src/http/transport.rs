/*
File: crates/ms-ai-api/src/http/transport.rs

Purpose:
The one native blocking HTTP transport of the crate's hand-rolled REST clients: it executes an
`HttpRequestSpec` over `ureq`, attaches an already-vetted key, applies the caller's per-request
timeout and body cap, and maps transport failures onto `HttpError`. Also the pure transport
policy every client shares: which requests may be retried, which hosts may receive the key and
the log-safe digest of a failing body.

Key structures:
- HttpTransport (the seam: `UreqTransport` in production, a scripted fake in tests)
- UreqTransport, SendOptions

Key functions:
- retry_allowed()    : only GET is ever retried (a POST is a paid request).
- auth_allowed()     : the key goes only to the origin of the call's base URL or to a host
                       the caller's data trusts (dot-boundary suffixes, https:443 only).
- log_body()         : lossy UTF-8 digest of a body with base64 runs elided, capped.

Notes:
Native only (not compiled on wasm). Blocking: worker threads only. A request already in flight
cannot be aborted. Requests that carry a key never follow redirects (a custom key header would
otherwise travel to the redirect target); unauthenticated requests do. Timeouts, caps, the
retry loop, polling and the key-origin decision per request belong to the caller (the
image-edit executor); this file never logs and never sees a key it was not handed.
*/

use std::io::Read;
use std::time::Duration;

use super::error::HttpError;
use super::request::{AuthScheme, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse};

/// Longest failing response body kept in a log line, in characters.
const LOG_BODY_CHARS: usize = 2048;
/// Shortest run of base64 characters that is elided from a logged body.
const LOG_BASE64_RUN: usize = 100;

/// Per-request options handed to the transport.
#[derive(Debug, Clone, Copy)]
pub struct SendOptions {
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Body cap in bytes; a longer body is `BodyTooLarge`.
    pub cap: u64,
    /// Step name for `Timeout` errors and logs (technical text).
    pub stage: &'static str,
}

/// One blocking HTTP exchange. `auth` is the already-vetted scheme and key. Any HTTP status,
/// error statuses included, is an `Ok` response; only transport failures are errors.
pub trait HttpTransport {
    /// Sends `request` and reads its body up to `options.cap`.
    ///
    /// # Errors
    /// `Network` (connection, TLS, IO), `Timeout { stage }`, `BodyTooLarge` over the cap,
    /// `RequestBuild` when the body cannot be serialized.
    fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, HttpError>;
}

/// The production transport over two `ureq` agents: one that never follows redirects (for
/// requests carrying a key) and one that does (unauthenticated downloads).
#[derive(Debug)]
pub struct UreqTransport {
    keyed: ureq::Agent,
    anonymous: ureq::Agent,
}

impl UreqTransport {
    /// Agents with the TCP / TLS `connect` timeout (the whole-request timeout is per request,
    /// `SendOptions::timeout`).
    #[must_use]
    pub fn new(connect: Duration) -> Self {
        Self { keyed: ureq::AgentBuilder::new().timeout_connect(connect).redirects(0).build(), anonymous: ureq::AgentBuilder::new().timeout_connect(connect).build() }
    }
}

impl HttpTransport for UreqTransport {
    fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, HttpError> {
        let agent = if auth.is_some() { &self.keyed } else { &self.anonymous };
        let method = match request.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        };
        let mut builder = agent.request(method, &request.url).timeout(options.timeout);
        for (name, value) in &request.headers {
            builder = builder.set(name, value);
        }
        builder = match auth {
            Some((AuthScheme::Bearer, key)) => builder.set("Authorization", &format!("Bearer {key}")),
            Some((AuthScheme::Header(name), key)) => builder.set(name, key),
            Some((AuthScheme::KeyPrefix(prefix), key)) => builder.set("Authorization", &format!("{prefix} {key}")),
            None => builder,
        };
        let result = match &request.body {
            HttpBody::Empty => builder.call(),
            HttpBody::Json(value) => {
                let bytes = serde_json::to_vec(value).map_err(|error| HttpError::RequestBuild { detail: error.to_string() })?;
                builder.set("Content-Type", "application/json").send_bytes(&bytes)
            }
            HttpBody::Multipart(form) => builder.set("Content-Type", &form.content_type).send_bytes(&form.bytes),
        };
        let response = match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(transport)) => return Err(transport_error(&transport, options.stage)),
        };
        let status = response.status();
        let content_type = response.header("Content-Type").map(str::to_string);
        let mut body = Vec::new();
        response.into_reader().take(options.cap.saturating_add(1)).read_to_end(&mut body).map_err(|error| io_error(&error, options.stage))?;
        if u64::try_from(body.len()).map_or(true, |len| len > options.cap) {
            return Err(HttpError::BodyTooLarge { limit_bytes: options.cap });
        }
        Ok(HttpResponse { status, content_type, body })
    }
}

/// Maps a `ureq` transport failure: a timed-out socket is `Timeout`, the rest `Network`.
fn transport_error(transport: &ureq::Transport, stage: &'static str) -> HttpError {
    let timed_out = std::error::Error::source(transport).and_then(|source| source.downcast_ref::<std::io::Error>()).is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock));
    if timed_out { HttpError::Timeout { stage: stage.to_string() } } else { HttpError::Network { detail: transport.to_string() } }
}

/// Maps a body read failure the same way.
fn io_error(error: &std::io::Error, stage: &'static str) -> HttpError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => HttpError::Timeout { stage: stage.to_string() },
        _ => HttpError::Network { detail: error.to_string() },
    }
}

/// Whether a failed request with `method` may be sent again. Only GET (status polls,
/// downloads): a POST submit is a paid request and is NEVER retried, whatever failed.
#[must_use]
pub fn retry_allowed(method: HttpMethod) -> bool {
    match method {
        HttpMethod::Get => true,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Delete => false,
    }
}

/// The scheme, lowercase host and port of an absolute http(s) URL; `None` for anything else
/// (other schemes, userinfo, whitespace, an empty host, a bad port).
fn origin(url: &str) -> Option<(String, String, u16)> {
    if url.chars().any(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    // An IPv6 literal keeps its brackets; the port follows the closing one.
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']')?;
        let (host, tail) = authority.split_at(close + 1);
        let port = if tail.is_empty() { None } else { Some(tail.strip_prefix(':')?) };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse::<u16>().ok()?,
        None => default_port,
    };
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// Whether the API key may be attached to `url`: when it has exactly the origin (scheme, host,
/// port) of the call's resolved `base_url` — the provider's own API host, or the user's own
/// server — or when it is https on port 443 and its host is one of the caller's
/// `trusted_suffixes` (the provider's data, e.g. image edit's `auth_host_suffixes`) or a
/// subdomain of one (dot boundary: `api.eu1.bfl.ai` matches `bfl.ai`,
/// `evilbfl.ai` does not). Signed result URLs on CDNs and any other host never get the key.
#[must_use]
pub fn auth_allowed(url: &str, base_url: &str, trusted_suffixes: &[&str]) -> bool {
    let Some(target) = origin(url) else { return false };
    if origin(base_url).is_some_and(|base| base == target) {
        return true;
    }
    let (scheme, host, port) = target;
    scheme == "https"
        && port == 443
        && trusted_suffixes.iter().any(|suffix| {
            let suffix = suffix.to_ascii_lowercase();
            host == suffix || host.strip_suffix(&suffix).is_some_and(|prefix| prefix.ends_with('.'))
        })
}

/// A failing response body for a log line: lossy UTF-8, runs of base64 characters of at least
/// `LOG_BASE64_RUN` replaced by `<base64: N chars>`, cut to `LOG_BODY_CHARS` characters.
#[must_use]
pub fn log_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= LOG_BASE64_RUN {
            out.push_str("<base64: ");
            out.push_str(&run.chars().count().to_string());
            out.push_str(" chars>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_b64(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    if out.chars().count() > LOG_BODY_CHARS {
        out = out.chars().take(LOG_BODY_CHARS).collect();
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{auth_allowed, log_body};

    #[test]
    fn the_key_goes_only_to_the_base_url_origin() {
        let base = "https://api.openai.com/v1";
        assert!(auth_allowed("https://api.openai.com/v1/images/edits", base, &[]));
        assert!(auth_allowed("HTTPS://API.OPENAI.COM:443/v1/x", base, &[]));
        assert!(!auth_allowed("http://api.openai.com/v1/images/edits", base, &[]));
        assert!(!auth_allowed("https://api.openai.com.evil.example/v1", base, &[]));
        assert!(!auth_allowed("https://api.openai.com:8443/v1", base, &[]));
        assert!(!auth_allowed("https://evil.example@api.openai.com/v1", base, &[]));
        assert!(!auth_allowed("https://cdn.aimlapi.com/generations/x.png", "https://api.aimlapi.com/v1", &[]));
        assert!(!auth_allowed("ftp://api.openai.com/v1", base, &[]));
        assert!(!auth_allowed("https://api.openai.com /v1", base, &[]));
        // The user's own server: plain http on its own port, IPv6 literals.
        assert!(auth_allowed("http://127.0.0.1:1234/v1/images/edits", "http://127.0.0.1:1234/v1", &[]));
        assert!(!auth_allowed("http://127.0.0.1:1235/v1/images/edits", "http://127.0.0.1:1234/v1", &[]));
        assert!(auth_allowed("http://[::1]:8080/v1/images/edits", "http://[::1]:8080/v1", &[]));
        assert!(!auth_allowed("http://[::1]/v1", "http://[::1]:8080/v1", &[]));
        // BFL: a regional polling host on a dot boundary, https:443 only.
        let bfl = ["bfl.ai"];
        assert!(auth_allowed("https://api.eu1.bfl.ai/v1/get_result?id=x", "https://api.bfl.ai", &bfl));
        assert!(auth_allowed("https://bfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://evilbfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.bfl.ai.evil.example/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("http://api.us1.bfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.us1.bfl.ai:8443/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.us1.bfl.ai/v1/x", "https://api.bfl.ai", &[]));
    }

    #[test]
    fn logged_bodies_elide_base64_and_are_capped() {
        let body = format!("{{\"error\":\"bad\",\"b64_json\":\"{}\"}}", "A".repeat(5000));
        assert_eq!(log_body(body.as_bytes()), "{\"error\":\"bad\",\"b64_json\":\"<base64: 5000 chars>\"}");
        let long = "word ".repeat(1000);
        assert_eq!(log_body(long.as_bytes()).chars().count(), 2048 + 3);
    }
}
