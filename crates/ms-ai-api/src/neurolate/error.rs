/*
File: crates/ms-ai-api/src/neurolate/error.rs

Purpose:
`NeurolateError`, the one typed error of the NeuroLate client (`ms_ai_api::neurolate`). Its
`Display` is the localized user message (rendered at display time through `t!` / `tf!`), so a
consumer shows `to_string()` as is.

Notes:
Target-neutral, `Clone` and `Send`: transport and parser failures are kept as `detail` text.
No variant ever carries the API key or image bytes; `code` / `message` are NeuroLate's own
error code and text (`ExtOcrErrorDTO`), passed through so the user sees the service's reason.
Transport failures convert 1:1 from `crate::http::HttpError` (`From`).
*/

use std::fmt;

use crate::error::AiApiError;
use crate::http::HttpError;

/// The `code` `NeuroLate` answers with a 402 when the key's own spending limit is used up (the
/// other 402 code, `INSUFFICIENT_BALANCE`, is the team balance).
pub(crate) const KEY_LIMIT_EXCEEDED_CODE: &str = "API_KEY_LIMIT_EXCEEDED";

/// Failure of a `NeuroLate` request, from the key read through the HTTP exchange to parsing.
#[derive(Debug, Clone)]
pub enum NeurolateError {
    /// `NeuroLate` was requested on the web build, which has no HTTP client.
    WebUnavailable,
    /// No `NeuroLate` API key is stored (or it is blank).
    KeyMissing,
    /// The credential store failed while reading the key.
    Key(AiApiError),
    /// `NeuroLate` answered 401 / 403: the key is invalid, revoked or not a team API key. (The
    /// service also answers 401 for a missing form field, which this client never omits.)
    KeyRejected,
    /// 402: the team balance (`INSUFFICIENT_BALANCE`) or the key's spending limit
    /// (`API_KEY_LIMIT_EXCEEDED`) does not cover the request; `code` is `NeuroLate`'s.
    OutOfCredits { code: String },
    /// 400: `NeuroLate` refused a field, region or image; `index` is the image or region index
    /// it names, if any.
    InvalidRequest { code: String, message: String, index: Option<i32> },
    /// 409 `DUPLICATE_REQUEST`: a request with this `Idempotency-Key` was already made (it may
    /// have been charged); `first_request_id` names it, for the service's request log.
    DuplicateRequest { first_request_id: Option<String> },
    /// 413: the images exceed `NeuroLate`'s total upload limit.
    TooLarge,
    /// 502: recognition failed on `NeuroLate`'s side for every image; nothing was charged.
    ServiceFailed { message: String },
    /// Any other non-success status; `message` is `NeuroLate`'s text, empty when it sent none.
    Http { status: u16, message: String },
    /// The HTTP exchange failed (connection, DNS, TLS, IO).
    Network { detail: String },
    /// The request exceeded its time limit.
    Timeout,
    /// The response body exceeded the client's cap of `limit_bytes`.
    ResponseTooLarge { limit_bytes: u64 },
    /// The answer is not the documented JSON shape.
    Decode { detail: String },
    /// The request could not be built (no image, a multipart failure).
    RequestBuild { detail: String },
    /// The caller abandoned the request; a request already sent may still be charged.
    Cancelled,
    /// Earlier paid requests (running, or abandoned but not finished yet) already use every
    /// slot; this one was not sent.
    TooManyInFlight,
    /// The worker thread of a request could not be started.
    TaskSpawn { detail: String },
    /// The worker thread of a request ended without answering (it panicked).
    WorkerLost,
}

impl NeurolateError {
    /// The localized, human-readable message for the active UI locale. This is the text
    /// `Display` writes.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::WebUnavailable => t!("ai_api.neurolate.error.web_unavailable_error").to_string(),
            Self::KeyMissing => t!("ai_api.neurolate.error.key_missing_error").to_string(),
            Self::Key(error) => error.user_message(),
            Self::KeyRejected => t!("ai_api.neurolate.error.key_rejected_error").to_string(),
            Self::OutOfCredits { code } if code == KEY_LIMIT_EXCEEDED_CODE => t!("ai_api.neurolate.error.key_limit_exceeded_error").to_string(),
            Self::OutOfCredits { .. } => t!("ai_api.neurolate.error.out_of_credits_error").to_string(),
            Self::InvalidRequest { code, message, .. } => tf!("ai_api.neurolate.error.invalid_request_error", code = code, message = message),
            Self::DuplicateRequest { first_request_id: Some(id) } => tf!("ai_api.neurolate.error.duplicate_request_with_id_error", id = id),
            Self::DuplicateRequest { first_request_id: None } => t!("ai_api.neurolate.error.duplicate_request_error").to_string(),
            Self::TooLarge => t!("ai_api.neurolate.error.too_large_error").to_string(),
            Self::ServiceFailed { message } => tf!("ai_api.neurolate.error.service_failed_error", message = message),
            Self::Http { status, message } => tf!("ai_api.neurolate.error.http_error", status = status, message = message),
            Self::Network { detail } => tf!("ai_api.neurolate.error.network_error", err = detail),
            Self::Timeout => t!("ai_api.neurolate.error.timeout_error").to_string(),
            Self::ResponseTooLarge { limit_bytes } => tf!("ai_api.neurolate.error.response_too_large_error", limit = limit_bytes / (1024 * 1024)),
            Self::Decode { detail } => tf!("ai_api.neurolate.error.decode_error", err = detail),
            Self::RequestBuild { detail } => tf!("ai_api.neurolate.error.request_build_error", err = detail),
            Self::Cancelled => t!("ai_api.neurolate.error.cancelled_error").to_string(),
            Self::TooManyInFlight => t!("ai_api.neurolate.error.too_many_in_flight_error").to_string(),
            Self::TaskSpawn { detail } => tf!("ai_api.neurolate.error.task_spawn_error", err = detail),
            Self::WorkerLost => t!("ai_api.neurolate.error.worker_lost_error").to_string(),
        }
    }
}

impl fmt::Display for NeurolateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for NeurolateError {}

impl From<AiApiError> for NeurolateError {
    fn from(error: AiApiError) -> Self {
        Self::Key(error)
    }
}

/// A failure of the shared transport (`crate::http`) maps 1:1 onto the `NeuroLate` variant with
/// the same meaning (the timeout's stage name is technical and dropped; the client logs it).
impl From<HttpError> for NeurolateError {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::RequestBuild { detail } => Self::RequestBuild { detail },
            HttpError::Network { detail } => Self::Network { detail },
            HttpError::Timeout { .. } => Self::Timeout,
            HttpError::BodyTooLarge { limit_bytes } => Self::ResponseTooLarge { limit_bytes },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NeurolateError;
    use crate::error::AiApiError;
    use crate::http::HttpError;

    // No UI locale is installed in this test binary, so only the Display == user_message
    // contract is asserted, never the localized text itself.
    #[test]
    fn display_is_user_message() {
        let errors = [
            NeurolateError::KeyMissing,
            NeurolateError::OutOfCredits { code: "API_KEY_LIMIT_EXCEEDED".to_string() },
            NeurolateError::InvalidRequest { code: "INVALID_LANGUAGE".to_string(), message: "bad".to_string(), index: None },
            NeurolateError::Http { status: 500, message: String::new() },
            NeurolateError::ResponseTooLarge { limit_bytes: 64 * 1024 * 1024 },
            NeurolateError::Cancelled,
            NeurolateError::TooManyInFlight,
            NeurolateError::DuplicateRequest { first_request_id: Some("r".to_string()) },
            NeurolateError::DuplicateRequest { first_request_id: None },
        ];
        for error in errors {
            assert_eq!(error.to_string(), error.user_message());
        }
    }

    #[test]
    fn key_errors_keep_the_key_store_message() {
        let source = AiApiError::EmptyKey;
        assert_eq!(NeurolateError::from(source.clone()).to_string(), source.to_string());
    }

    #[test]
    fn http_error_maps_onto_neurolate_error() {
        assert!(matches!(NeurolateError::from(HttpError::RequestBuild { detail: "x".to_string() }), NeurolateError::RequestBuild { detail } if detail == "x"));
        assert!(matches!(NeurolateError::from(HttpError::Network { detail: "reset".to_string() }), NeurolateError::Network { detail } if detail == "reset"));
        assert!(matches!(NeurolateError::from(HttpError::Timeout { stage: "ocr".to_string() }), NeurolateError::Timeout));
        assert!(matches!(NeurolateError::from(HttpError::BodyTooLarge { limit_bytes: 42 }), NeurolateError::ResponseTooLarge { limit_bytes: 42 }));
    }
}
