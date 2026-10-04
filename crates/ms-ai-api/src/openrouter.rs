/*
File: crates/ms-ai-api/src/openrouter.rs

Purpose:
OpenRouter account status (usage, credit limit, remaining credit, rate limit) for the
connection UI. Native only (`ureq`, `serde_json`).

Key functions:
- fetch_account_status()  : blocking GET `https://openrouter.ai/api/v1/key`.
- format_account_status() : pure formatter of that response, tested without network.

Notes:
The key travels only in the `Authorization` header; it never enters a URL, an error or a log.
The `"OpenRouter: "` prefix and the `"{requests} req/{interval}"` part are not localized.
*/

use ms_log::runtime_log;
use serde_json::Value;

use crate::error::AiApiError;

/// `OpenRouter`'s "current key" endpoint.
const OPENROUTER_KEY_URL: &str = "https://openrouter.ai/api/v1/key";

/// Fetches and formats the account status of the `OpenRouter` `api_key`. Blocking network
/// I/O: worker threads only.
///
/// # Errors
/// `OpenRouterRequest` when the request fails, `OpenRouterNonJson` when the body is not JSON.
pub fn fetch_account_status(api_key: &str) -> Result<String, AiApiError> {
    let response = ureq::get(OPENROUTER_KEY_URL)
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
        .map_err(|err| {
            runtime_log::log_warn(format!("[AI API] OpenRouter account status request failed: {err}"));
            AiApiError::OpenRouterRequest { detail: err.to_string() }
        })?;
    let value: Value = response.into_json().map_err(|err| {
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
    use super::format_account_status;
    use serde_json::json;

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
