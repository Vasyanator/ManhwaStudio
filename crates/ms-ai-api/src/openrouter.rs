/*
File: crates/ms-ai-api/src/openrouter.rs

Purpose:
OpenRouter account status (usage, credit limit, remaining credit, rate limit) for the
connection UI. Native only (`ureq`, `serde_json`).

Key functions:
- fetch_account_status()  : blocking GET `https://openrouter.ai/api/v1/key` over the crate's
                            one transport (`crate::http::UreqTransport`: connect + whole-request
                            timeouts, body cap, no redirects with a key).
- fetch_account_status_with() : the same over any `HttpTransport` (test seam).
- format_account_status() : pure formatter of that response, tested without network.

Notes:
The key travels only in the `Authorization` header; it never enters a URL, an error or a log.
Not retried: a status query the user re-runs on demand, not a download.
The `"OpenRouter: "` prefix and the `"{requests} req/{interval}"` part are not localized.
*/

use std::time::Duration;

use ms_log::runtime_log;
use serde_json::Value;

use crate::error::AiApiError;
use crate::http::{AuthScheme, HttpBody, HttpMethod, HttpRequestSpec, HttpTransport, SendOptions, UreqTransport, log_body};

/// `OpenRouter`'s "current key" endpoint.
const OPENROUTER_KEY_URL: &str = "https://openrouter.ai/api/v1/key";
/// TCP / TLS connect timeout of the status request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request timeout (connect, send and body read) of the status request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Body cap of the status answer: a small JSON object; anything larger is not that answer.
const RESPONSE_CAP_BYTES: u64 = 256 * 1024;

/// Fetches and formats the account status of the `OpenRouter` `api_key`. Blocking network
/// I/O: worker threads only.
///
/// # Errors
/// As [`fetch_account_status_with`].
pub fn fetch_account_status(api_key: &str) -> Result<String, AiApiError> {
    fetch_account_status_with(&UreqTransport::new(CONNECT_TIMEOUT), api_key)
}

/// [`fetch_account_status`] over `transport` (the test seam): one GET with the key as a
/// Bearer header, bounded by [`REQUEST_TIMEOUT`] and [`RESPONSE_CAP_BYTES`].
///
/// # Errors
/// `OpenRouterRequest` when the exchange fails (network, timeout, oversized body) or the
/// answer is not a 2xx status; `OpenRouterNonJson` when a 2xx body is not JSON.
pub(crate) fn fetch_account_status_with(transport: &dyn HttpTransport, api_key: &str) -> Result<String, AiApiError> {
    let spec = HttpRequestSpec { method: HttpMethod::Get, url: OPENROUTER_KEY_URL.to_string(), headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::Bearer) };
    let options = SendOptions { timeout: REQUEST_TIMEOUT, cap: RESPONSE_CAP_BYTES, stage: "openrouter_key" };
    let response = transport.send(&spec, Some((AuthScheme::Bearer, api_key)), options).map_err(|err| {
        runtime_log::log_warn(format!("[AI API] OpenRouter account status request failed: {err}"));
        AiApiError::OpenRouterRequest { detail: err.to_string() }
    })?;
    if !(200..300).contains(&response.status) {
        runtime_log::log_warn(format!("[AI API] OpenRouter account status answered HTTP {}: {}", response.status, log_body(&response.body)));
        return Err(AiApiError::OpenRouterRequest { detail: format!("HTTP {}", response.status) });
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|err| {
        runtime_log::log_warn(format!("[AI API] OpenRouter account status response is not JSON: {err}"));
        AiApiError::OpenRouterNonJson { detail: err.to_string() }
    })?;
    Ok(format_account_status(&value))
}

/// Formats an `/api/v1/key` response (the object itself or wrapped in `data`) as
/// `"OpenRouter: <part>, <part>, ..."`: usage when present, then exactly one of the four
/// limit/remaining texts, then `"{requests} req/{interval}"` when the rate limit is complete.
#[must_use]
pub fn format_account_status(value: &Value) -> String {
    let data = value.get("data").unwrap_or(value);
    let usage = data.get("usage").and_then(Value::as_f64);
    let limit = data.get("limit").and_then(Value::as_f64);
    let remaining = data.get("limit_remaining").and_then(Value::as_f64);
    let rate = data.get("rate_limit").and_then(Value::as_object);

    let mut parts = Vec::new();
    if let Some(usage) = usage {
        parts.push(tf!(
            "ai_api.openrouter.usage_status",
            usage = format!("{usage:.2}")
        ));
    }
    match (limit, remaining) {
        (Some(limit), Some(remaining)) => {
            parts.push(tf!(
                "ai_api.openrouter.limit_remaining_status",
                limit = format!("{limit:.2}"),
                remaining = format!("{remaining:.2}")
            ));
        }
        (None, Some(remaining)) => {
            parts.push(tf!(
                "ai_api.openrouter.remaining_status",
                remaining = format!("{remaining:.2}")
            ));
        }
        (None, None) => {
            parts.push(t!("ai_api.openrouter.limit_not_set_status").to_string());
        }
        (Some(limit), None) => {
            parts.push(tf!(
                "ai_api.openrouter.limit_status",
                limit = format!("{limit:.2}")
            ));
        }
    }
    if let Some(rate) = rate {
        let requests = rate.get("requests").and_then(Value::as_u64);
        let interval = rate.get("interval").and_then(Value::as_str);
        if let (Some(requests), Some(interval)) = (requests, interval) {
            parts.push(format!("{requests} req/{interval}"));
        }
    }
    format!("OpenRouter: {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{OPENROUTER_KEY_URL, REQUEST_TIMEOUT, RESPONSE_CAP_BYTES, fetch_account_status_with, format_account_status};
    use crate::error::AiApiError;
    use crate::http::{AuthScheme, HttpError, HttpMethod, HttpRequestSpec, HttpResponse, HttpTransport, SendOptions};
    use serde_json::json;
    use std::sync::Mutex;

    /// One request as the fake transport saw it.
    struct SeenRequest {
        method: HttpMethod,
        url: String,
        auth: Option<AuthScheme>,
        /// The test key was handed to the transport.
        key_attached: bool,
        options: SendOptions,
    }

    /// Answers every request with `answer` and records what was asked of it.
    struct FakeTransport {
        answer: Result<HttpResponse, HttpError>,
        seen: Mutex<Vec<SeenRequest>>,
    }

    impl FakeTransport {
        fn new(answer: Result<HttpResponse, HttpError>) -> Self {
            Self { answer, seen: Mutex::new(Vec::new()) }
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, HttpError> {
            let key_attached = auth.is_some_and(|(_, key)| key == "sk-test");
            self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(SeenRequest { method: request.method, url: request.url.clone(), auth: request.auth, key_attached, options });
            self.answer.clone()
        }
    }

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse { status, content_type: Some("application/json".to_string()), body: body.as_bytes().to_vec() }
    }

    /// The status request goes through the transport with the key as Bearer auth and the
    /// explicit timeout and body cap (never ureq's unbounded default agent).
    #[test]
    fn status_request_carries_key_timeout_and_cap() {
        let transport = FakeTransport::new(Ok(response(200, r#"{"data":{"usage":1.5}}"#)));
        let status = fetch_account_status_with(&transport, "sk-test").expect("a 2xx JSON answer formats");
        assert!(status.starts_with("OpenRouter: "));
        let seen = transport.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let request = &seen[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.url, OPENROUTER_KEY_URL);
        assert_eq!(request.auth, Some(AuthScheme::Bearer));
        assert!(request.key_attached);
        assert_eq!(request.options.timeout, REQUEST_TIMEOUT);
        assert_eq!(request.options.cap, RESPONSE_CAP_BYTES);
    }

    #[test]
    fn non_2xx_status_is_a_request_error() {
        let transport = FakeTransport::new(Ok(response(401, r#"{"error":"bad key"}"#)));
        assert!(matches!(fetch_account_status_with(&transport, "sk-test"), Err(AiApiError::OpenRouterRequest { detail }) if detail == "HTTP 401"));
    }

    #[test]
    fn transport_timeout_is_a_request_error() {
        let transport = FakeTransport::new(Err(HttpError::Timeout { stage: "openrouter_key".to_string() }));
        assert!(matches!(fetch_account_status_with(&transport, "sk-test"), Err(AiApiError::OpenRouterRequest { .. })));
    }

    #[test]
    fn non_json_body_is_reported() {
        let transport = FakeTransport::new(Ok(response(200, "<html>")));
        assert!(matches!(fetch_account_status_with(&transport, "sk-test"), Err(AiApiError::OpenRouterNonJson { .. })));
    }

    // No UI locale is installed in this test binary, so these assert the structure (prefix,
    // part count, the unlocalized rate-limit part), never localized text.
    fn parts(status: &str) -> Vec<&str> {
        status.strip_prefix("OpenRouter: ").map(|rest| rest.split(", ").collect()).unwrap_or_default()
    }

    #[test]
    fn openrouter_format_all_fields_with_data_wrapper() {
        let value = json!({"data": {"usage": 1.5, "limit": 10.0, "limit_remaining": 8.5, "rate_limit": {"requests": 20, "interval": "10s"}}});
        let status = format_account_status(&value);
        assert!(status.starts_with("OpenRouter: "));
        let parts = parts(&status);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2], "20 req/10s");
    }

    #[test]
    fn openrouter_format_each_limit_arm_yields_one_part() {
        let cases = [
            json!({"limit": 10.0, "limit_remaining": 3.0}),
            json!({"limit_remaining": 3.0}),
            json!({}),
            json!({"limit": 10.0}),
        ];
        let mut rendered = Vec::new();
        for value in &cases {
            let status = format_account_status(value);
            assert!(status.starts_with("OpenRouter: "));
            assert_eq!(parts(&status).len(), 1, "{status}");
            rendered.push(status);
        }
        // The four arms render four different texts (keys differ even without a locale).
        rendered.sort();
        rendered.dedup();
        assert_eq!(rendered.len(), 4);
    }

    #[test]
    fn openrouter_format_skips_incomplete_rate_limit() {
        let value = json!({"usage": 2.0, "rate_limit": {"requests": 5}});
        let status = format_account_status(&value);
        assert_eq!(parts(&status).len(), 2);
        assert!(!status.contains("req/"));
    }
}
