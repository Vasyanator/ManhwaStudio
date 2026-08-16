/*
File: src/launcher/new_project/quick_download/http.rs

Purpose:
Site-agnostic HTTP primitives of the quick downloader: one browser-like request with a shared
timeout/User-Agent, plus text/bytes/JSON readers on top of it.

Key constants:
- REQUEST_TIMEOUT, DEFAULT_USER_AGENT (native only)
- DOWNLOAD_PARALLELISM

Key functions:
- execute_request() - the single request builder (headers + optional JSON body) all others use
- make_request(), fetch_text(), fetch_bytes(), fetch_json_value()
- fetch_text_with_headers(), fetch_json_with_headers(), post_json_value()

Notes:
The native path uses `ureq`, which is not compiled for wasm; the wasm stubs return a clear
error instead of a fake response. Keep the `cfg` pairs in sync when editing.
*/

use super::plan::QuickDownloadError;
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

#[cfg(not(target_arch = "wasm32"))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound on images handed to one rayon task, i.e. the effective download fan-out.
pub(crate) const DOWNLOAD_PARALLELISM: usize = 8;
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36";

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

/// Fetches `url` and reads the whole response body into memory.
///
/// # Errors
/// Returns `QuickDownloadError` on a transport/HTTP failure or if the body is not readable.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fetch_bytes(url: &str, referer: Option<&str>) -> Result<Vec<u8>, QuickDownloadError> {
    let response = make_request(url, referer)?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.download_page_error").to_string(),
        log_message: format!("failed to read binary response from '{url}': {err}"),
    })?;
    Ok(bytes)
}

/// Web stub twin of `fetch_bytes`; see `fetch_text`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn fetch_bytes(
    _url: &str,
    _referer: Option<&str>,
) -> Result<Vec<u8>, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.download_web_unsupported").to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
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

/// Builds and performs the single request every helper in this file goes through: shared
/// connect/read/write timeout, a desktop-browser `User-Agent`, and the caller's extra headers.
///
/// `json_body` selects the method: `None` issues a GET, `Some(body)` a POST carrying the
/// serialized value with `Content-Type: application/json`. Caller headers are applied after the
/// defaults, so a header the caller supplies (`User-Agent`, `Content-Type`, ...) wins.
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
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build();
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
    let result = match json_body {
        Some(body) => request.send_json(body),
        None => request.call(),
    };
    result.map_err(|err| match err {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            QuickDownloadError {
                user_message: tf!("launcher.new_project.quick_dl.site_error_status", status = status),
                log_message: format!("request '{url}' failed with status {status}; body={body}"),
            }
        }
        ureq::Error::Transport(transport) => QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.connect_error").to_string(),
            log_message: format!("request '{url}' failed: {transport}"),
        },
    })
}
