/*
File: crates/ms-launcher/src/new_project/quick_download/http.rs

Purpose:
Site-agnostic HTTP primitives of the quick downloader: one browser-like request with a shared
timeout/User-Agent, plus text/bytes/JSON readers on top of it, and the classified image fetch
the controller's retry/fallback loop is built on.

Key structures:
- FetchFailureKind, ImageFetchFailure - transient-vs-permanent verdict of a failed image fetch

Key constants:
- REQUEST_TIMEOUT, DEFAULT_USER_AGENT (native only)
- DOWNLOAD_PARALLELISM

Key functions:
- http_agent() - the process-wide `ureq` agent, i.e. the connection pool (native only)
- install_on_download_pool() - runs the parallel image download on the downloader's own pool
- execute_request() - the single request builder (headers + optional JSON body) all others use
- make_request(), fetch_text(), fetch_json_value()
- fetch_text_with_headers(), fetch_json_with_headers(), post_json_value()
- fetch_image_bytes() - one image GET whose failure says whether a retry may help
- classify_status(), classify_transport() - the retry policy's transient/permanent rule

Notes:
The native path uses `ureq`, which is not compiled for wasm; the wasm stubs return a clear
error instead of a fake response. Keep the `cfg` pairs in sync when editing.
All requests share ONE agent, so TCP/TLS connections (and the cookie jar) are reused across
the pages of a chapter instead of being renegotiated per image.
*/

use super::plan::QuickDownloadError;
use serde_json::Value;
use std::sync::OnceLock;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

#[cfg(not(target_arch = "wasm32"))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Number of images fetched concurrently by `install_on_download_pool`. This is a network
/// fan-out, not a CPU one: it is deliberately independent of the core count, since every
/// worker spends its time waiting on a socket.
pub(crate) const DOWNLOAD_PARALLELISM: usize = 16;
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36";

/// Process-wide HTTP agent of the quick downloader, i.e. its connection pool.
#[cfg(not(target_arch = "wasm32"))]
static HTTP_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// Returns the shared agent, building it on first use.
///
/// A per-request agent would renegotiate TCP+TLS for every page of a chapter; one shared
/// agent keeps the connections alive. `ureq` caps idle connections at ONE per host by
/// default, which would defeat the reuse exactly in the parallel case, so the cap is raised
/// to the download fan-out.
#[cfg(not(target_arch = "wasm32"))]
fn http_agent() -> &'static ureq::Agent {
    HTTP_AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(REQUEST_TIMEOUT)
            .timeout_read(REQUEST_TIMEOUT)
            .timeout_write(REQUEST_TIMEOUT)
            .max_idle_connections_per_host(DOWNLOAD_PARALLELISM)
            .build()
    })
}

/// The downloader's own worker pool, or the reason it could not be built. Built once per
/// process; a failure is reported to the caller rather than silently downgraded.
#[cfg(not(target_arch = "wasm32"))]
static DOWNLOAD_POOL: OnceLock<Result<rayon::ThreadPool, String>> = OnceLock::new();

/// Runs `op` on the quick downloader's own pool of `DOWNLOAD_PARALLELISM` threads.
///
/// The pool is separate from the global rayon pool on purpose: image downloads block on
/// sockets, and blocking the global pool - sized to the CPU count and shared with the app's
/// compute work - would both stall that work and cap the fan-out at the core count.
///
/// # Errors
/// Returns `QuickDownloadError` when the pool could not be created; `op` is not run then.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn install_on_download_pool<R, F>(op: F) -> Result<R, QuickDownloadError>
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    let pool = DOWNLOAD_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(DOWNLOAD_PARALLELISM)
            .thread_name(|index| format!("quick-dl-{index}"))
            .build()
            .map_err(|err| err.to_string())
    });
    match pool {
        Ok(pool) => Ok(pool.install(op)),
        Err(err) => Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.start_error").to_string(),
            log_message: format!(
                "failed to build the quick download thread pool of {DOWNLOAD_PARALLELISM} threads: {err}"
            ),
        }),
    }
}

/// Web stub twin of `install_on_download_pool`: the web build has no worker pool here, and
/// the fetch stubs fail before parallelism could matter, so `op` runs on the caller thread.
///
/// # Errors
/// Never fails; the signature matches the native twin.
#[cfg(target_arch = "wasm32")]
pub(crate) fn install_on_download_pool<R, F>(op: F) -> Result<R, QuickDownloadError>
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    Ok(op())
}

/// Fetches `url` and reads the response body as text.
///
/// # Errors
/// Returns `QuickDownloadError` on a transport/HTTP failure or if the body is not readable.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fetch_text(url: &str, referer: Option<&str>) -> Result<String, QuickDownloadError> {
    let response = make_request(url, referer)?
        .into_string()
        .map_err(|err| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.read_response_error").to_string(),
            log_message: format!("failed to read text response from '{url}': {err}"),
        })?;
    Ok(response)
}

/// Web stub: the direct downloader uses a native HTTP client (`ureq`) that is not
/// compiled for wasm. Returns a clear error instead of a fake response.
#[cfg(target_arch = "wasm32")]
pub(crate) fn fetch_text(_url: &str, _referer: Option<&str>) -> Result<String, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.download_web_unsupported").to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
    })
}

/// Fetches `url` with extra request headers and reads the response body as text.
///
/// `headers` is applied on top of the default `User-Agent`; an entry whose name matches a
/// default (case-insensitively) replaces it instead of being sent twice. A site that needs a
/// `Referer` passes it here like any other header.
///
/// # Errors
/// Returns `QuickDownloadError` on a transport/HTTP failure or if the body is not readable.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fetch_text_with_headers(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<String, QuickDownloadError> {
    execute_request(url, headers, None)?
        .into_string()
        .map_err(|err| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.read_response_error").to_string(),
            log_message: format!("failed to read text response from '{url}': {err}"),
        })
}

/// Web stub twin of `fetch_text_with_headers`; see `fetch_text`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn fetch_text_with_headers(
    _url: &str,
    _headers: &[(&str, &str)],
) -> Result<String, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.download_web_unsupported").to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
    })
}

/// Whether a failed fetch may succeed when the SAME URL is requested again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchFailureKind {
    /// Network trouble or an overloaded server (transport error, body cut short, HTTP
    /// 408/429/5xx): retrying the same URL after a backoff may help.
    Transient,
    /// The server answered definitively (404, other 4xx) or the request itself is unusable
    /// (bad URL/scheme, redirect loop, proxy credentials): retrying the same URL cannot help.
    Permanent,
}

/// Failure of `fetch_image_bytes`: the retry verdict plus the usual user/log message pair.
#[derive(Debug)]
pub(crate) struct ImageFetchFailure {
    pub(crate) kind: FetchFailureKind,
    pub(crate) error: QuickDownloadError,
}

/// Retry verdict for an HTTP error status, per the project "Network downloads" rule:
/// 408, 429 and every 5xx are transient, any other status is permanent. Native only, like the
/// `ureq` fetch that is its one caller.
#[cfg(not(target_arch = "wasm32"))]
fn classify_status(status: u16) -> FetchFailureKind {
    if status == 408 || status == 429 || (500..=599).contains(&status) {
        FetchFailureKind::Transient
    } else {
        FetchFailureKind::Permanent
    }
}

/// Retry verdict for a `ureq` transport error: connection/DNS/IO/protocol-garbage failures are
/// network weather and transient; a malformed request (URL, scheme, HTTPS-only, redirect loop,
/// proxy configuration or credentials) fails identically every time and is permanent.
/// TLS handshake and certificate errors arrive as `Io` and are therefore retried too; telling
/// them apart would mean inspecting error sources, and the cost is one short backoff.
#[cfg(not(target_arch = "wasm32"))]
fn classify_transport(kind: ureq::ErrorKind) -> FetchFailureKind {
    match kind {
        ureq::ErrorKind::Dns
        | ureq::ErrorKind::ConnectionFailed
        | ureq::ErrorKind::BadStatus
        | ureq::ErrorKind::BadHeader
        | ureq::ErrorKind::Io
        | ureq::ErrorKind::ProxyConnect => FetchFailureKind::Transient,
        ureq::ErrorKind::InvalidUrl
        | ureq::ErrorKind::UnknownScheme
        | ureq::ErrorKind::InsecureRequestHttpsOnly
        | ureq::ErrorKind::TooManyRedirects
        | ureq::ErrorKind::InvalidProxyUrl
        | ureq::ErrorKind::ProxyUnauthorized => FetchFailureKind::Permanent,
        // `HTTP` is the kind of `ureq::Error::Status`, which never reaches this function;
        // classifying it by its status is `classify_status`'s job.
        ureq::ErrorKind::HTTP => FetchFailureKind::Permanent,
    }
}

/// Fetches one page image whole, with the optional `Referer`, and tells the caller whether a
/// failure is worth retrying on the same URL.
///
/// No HTTP `Range` resume: a page image is small and fetched in one piece, so a transfer cut
/// short is simply retried from the start (classified `Transient`).
///
/// # Errors
/// Returns `ImageFetchFailure` carrying the `FetchFailureKind` (see `classify_status` /
/// `classify_transport`; an interrupted body read is transient) and the same user/log messages
/// as the other fetch helpers.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fetch_image_bytes(
    url: &str,
    referer: Option<&str>,
) -> Result<Vec<u8>, ImageFetchFailure> {
    let headers: Vec<(&str, &str)> = referer.map(|value| ("Referer", value)).into_iter().collect();
    let response = send_request(url, &headers, None).map_err(|err| {
        let kind = match err.as_ref() {
            ureq::Error::Status(status, _) => classify_status(*status),
            ureq::Error::Transport(transport) => classify_transport(transport.kind()),
        };
        ImageFetchFailure {
            kind,
            error: request_error(url, err),
        }
    })?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|err| ImageFetchFailure {
        kind: FetchFailureKind::Transient,
        error: QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.download_page_error").to_string(),
            log_message: format!("failed to read binary response from '{url}': {err}"),
        },
    })?;
    Ok(bytes)
}

/// Web stub twin of `fetch_image_bytes`; see `fetch_text`. The missing client is `Permanent`,
/// so the controller neither retries nor sleeps on the web build.
#[cfg(target_arch = "wasm32")]
pub(crate) fn fetch_image_bytes(
    _url: &str,
    _referer: Option<&str>,
) -> Result<Vec<u8>, ImageFetchFailure> {
    Err(ImageFetchFailure {
        kind: FetchFailureKind::Permanent,
        error: QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.download_web_unsupported").to_string(),
            log_message: "quick download HTTP client is not available on the web build".to_string(),
        },
    })
}

/// Fetches `url` and parses the body as JSON.
///
/// # Errors
/// Returns `QuickDownloadError` if the fetch failed or the body is not valid JSON; the log
/// message then carries the raw body for diagnosis.
pub(crate) fn fetch_json_value(
    url: &str,
    referer: Option<&str>,
) -> Result<Value, QuickDownloadError> {
    let text = fetch_text(url, referer)?;
    serde_json::from_str::<Value>(&text).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.unexpected_json_error").to_string(),
        log_message: format!("failed to parse json from '{url}': {err}; body={text}"),
    })
}

/// Fetches `url` with extra request headers and parses the body as JSON.
///
/// Header semantics are those of `fetch_text_with_headers`. Like `fetch_json_value`, this is a
/// thin parser over the text fetch and therefore needs no `cfg` pair of its own: on wasm the
/// underlying `fetch_text_with_headers` stub already returns the "unsupported on web" error.
///
/// # Errors
/// Returns `QuickDownloadError` if the fetch failed or the body is not valid JSON; the log
/// message then carries the raw body for diagnosis.
pub(crate) fn fetch_json_with_headers(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<Value, QuickDownloadError> {
    let text = fetch_text_with_headers(url, headers)?;
    serde_json::from_str::<Value>(&text).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.unexpected_json_error").to_string(),
        log_message: format!("failed to parse json from '{url}': {err}; body={text}"),
    })
}

/// POSTs `body` as JSON to `url` and parses the response as JSON.
///
/// The request carries `Content-Type: application/json` and the default `User-Agent`; `headers`
/// is applied on top of both, so a caller entry with the same name (case-insensitively) wins.
///
/// # Errors
/// Returns `QuickDownloadError` on a transport/HTTP failure, if the body is not readable, or if
/// the response is not valid JSON; the log message then carries the raw body for diagnosis.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn post_json_value(
    url: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> Result<Value, QuickDownloadError> {
    let text = execute_request(url, headers, Some(body))?
        .into_string()
        .map_err(|err| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.read_response_error").to_string(),
            log_message: format!("failed to read text response from '{url}': {err}"),
        })?;
    serde_json::from_str::<Value>(&text).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.unexpected_json_error").to_string(),
        log_message: format!("failed to parse json from '{url}': {err}; body={text}"),
    })
}

/// Web stub twin of `post_json_value`; see `fetch_text`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn post_json_value(
    _url: &str,
    _body: &Value,
    _headers: &[(&str, &str)],
) -> Result<Value, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.download_web_unsupported").to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
    })
}

/// Performs the single GET used by the `Referer`-based fetch helpers: shared timeouts, a
/// desktop-browser `User-Agent`, and the optional `Referer` some CDNs require.
///
/// Thin wrapper over `execute_request`; the `Referer` is just a header.
///
/// # Errors
/// Returns `QuickDownloadError` with the HTTP status (and body excerpt in the log) for a
/// status error, or the transport error otherwise.
#[cfg(not(target_arch = "wasm32"))]
fn make_request(url: &str, referer: Option<&str>) -> Result<ureq::Response, QuickDownloadError> {
    match referer {
        Some(referer) => execute_request(url, &[("Referer", referer)], None),
        None => execute_request(url, &[], None),
    }
}

/// Performs a request through `send_request` and maps its failure with `request_error`.
///
/// # Errors
/// Returns `QuickDownloadError` with the HTTP status (and body excerpt in the log) for a
/// status error, or the transport error otherwise.
#[cfg(not(target_arch = "wasm32"))]
fn execute_request(
    url: &str,
    headers: &[(&str, &str)],
    json_body: Option<&Value>,
) -> Result<ureq::Response, QuickDownloadError> {
    send_request(url, headers, json_body).map_err(|err| request_error(url, err))
}

/// Builds and performs the single request every helper in this file goes through: shared
/// connect/read/write timeout, a desktop-browser `User-Agent`, and the caller's extra headers.
///
/// `json_body` selects the method: `None` issues a GET, `Some(body)` a POST carrying the
/// serialized value with `Content-Type: application/json`. Caller headers are applied after the
/// defaults, so a header the caller supplies (`User-Agent`, `Content-Type`, ...) wins.
///
/// # Errors
/// Returns the raw `ureq::Error` (boxed: it is several hundred bytes), so a caller can
/// classify it before mapping it.
#[cfg(not(target_arch = "wasm32"))]
fn send_request(
    url: &str,
    headers: &[(&str, &str)],
    json_body: Option<&Value>,
) -> Result<ureq::Response, Box<ureq::Error>> {
    let agent = http_agent();
    // A JSON body implies a POST; every other request in this module is a plain GET.
    let mut request = if json_body.is_some() {
        agent.post(url)
    } else {
        agent.get(url)
    };
    // `ureq::Request::set` only replaces a previously set header when the names match
    // byte-for-byte, so a caller header spelled `user-agent` would be sent *in addition* to the
    // default. Skip the default when the caller already brings one under any spelling.
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
    {
        request = request.set("User-Agent", DEFAULT_USER_AGENT);
    }
    for (name, value) in headers {
        request = request.set(name, value);
    }
    // `send_json` serializes the value and sets `Content-Type: application/json` unless the
    // caller already set that header above.
    match json_body {
        Some(body) => request.send_json(body),
        None => request.call(),
    }
    .map_err(Box::new)
}

/// Longest error-response body excerpt kept in a log message, in bytes. Image retries log
/// the same error several times, and a CDN challenge page can be tens of KB.
#[cfg(not(target_arch = "wasm32"))]
const ERROR_BODY_LOG_LIMIT: usize = 512;

/// Returns `body` cut to at most `limit` bytes on a char boundary, with a note naming the
/// full length when anything was cut.
#[cfg(not(target_arch = "wasm32"))]
fn body_excerpt(body: &str, limit: usize) -> String {
    if body.len() <= limit {
        return body.to_string();
    }
    let mut end = limit;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated, {} bytes total)", &body[..end], body.len())
}

/// Maps a `ureq` failure of a request to `url` onto the module's user/log message pair:
/// `site_error_status` with a body excerpt (`ERROR_BODY_LOG_LIMIT`) in the log for a status
/// error, `connect_error` for a transport error.
#[cfg(not(target_arch = "wasm32"))]
fn request_error(url: &str, err: Box<ureq::Error>) -> QuickDownloadError {
    match *err {
        ureq::Error::Status(status, response) => {
            let body = body_excerpt(&response.into_string().unwrap_or_default(), ERROR_BODY_LOG_LIMIT);
            QuickDownloadError {
                user_message: tf!("launcher.new_project.quick_dl.site_error_status", status = status),
                log_message: format!("request '{url}' failed with status {status}; body={body}"),
            }
        }
        ureq::Error::Transport(transport) => QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.connect_error").to_string(),
            log_message: format!("request '{url}' failed: {transport}"),
        },
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{FetchFailureKind, body_excerpt, classify_status, classify_transport, fetch_image_bytes};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn classify_transport_retries_network_weather_only() {
        for kind in [
            ureq::ErrorKind::Dns,
            ureq::ErrorKind::ConnectionFailed,
            ureq::ErrorKind::Io,
            ureq::ErrorKind::BadStatus,
            ureq::ErrorKind::BadHeader,
            ureq::ErrorKind::ProxyConnect,
        ] {
            assert_eq!(classify_transport(kind), FetchFailureKind::Transient, "{kind:?}");
        }
        for kind in [
            ureq::ErrorKind::InvalidUrl,
            ureq::ErrorKind::UnknownScheme,
            ureq::ErrorKind::InsecureRequestHttpsOnly,
            ureq::ErrorKind::TooManyRedirects,
            ureq::ErrorKind::InvalidProxyUrl,
            ureq::ErrorKind::ProxyUnauthorized,
        ] {
            assert_eq!(classify_transport(kind), FetchFailureKind::Permanent, "{kind:?}");
        }
    }

    #[test]
    fn body_excerpt_cuts_on_a_char_boundary_and_names_the_length() {
        assert_eq!(body_excerpt("short", 512), "short");
        // "é" is two bytes: a 3-byte limit must not split the second one.
        assert_eq!(body_excerpt("éé", 3), "é... (truncated, 4 bytes total)");
        let long = "x".repeat(2000);
        let excerpt = body_excerpt(&long, 512);
        assert!(excerpt.starts_with(&"x".repeat(512)));
        assert!(excerpt.ends_with("(truncated, 2000 bytes total)"), "{excerpt}");
    }

    /// A loopback server that announces a 1000-byte body, sends 10 bytes and closes: the body
    /// read fails, which must be classified transient (a cut transfer is worth a retry).
    #[test]
    fn body_cut_short_is_a_transient_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let address = listener.local_addr().expect("loopback listener has an address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the test client connects");
            let mut request = [0_u8; 1024];
            // Only consume the request; its content is irrelevant to the test.
            let _read = stream.read(&mut request).expect("read the request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n0123456789")
                .expect("write the truncated response");
        });
        let Err(failure) = fetch_image_bytes(&format!("http://{address}/page.png"), None) else {
            panic!("a body shorter than Content-Length must fail");
        };
        server.join().expect("the loopback server thread finishes");
        assert_eq!(failure.kind, FetchFailureKind::Transient, "{}", failure.error.log_message);
        assert!(failure.error.log_message.contains("failed to read binary response"), "{}", failure.error.log_message);
    }

    #[test]
    fn classify_status_retries_only_timeouts_throttling_and_server_errors() {
        for status in [408, 429, 500, 502, 503, 504, 599] {
            assert_eq!(classify_status(status), FetchFailureKind::Transient, "{status}");
        }
        for status in [400, 401, 403, 404, 410, 418, 451, 301, 600] {
            assert_eq!(classify_status(status), FetchFailureKind::Permanent, "{status}");
        }
    }
}
