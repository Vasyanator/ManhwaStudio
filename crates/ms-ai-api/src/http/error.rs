/*
File: crates/ms-ai-api/src/http/error.rs

Purpose:
`HttpError`, the failure of building or executing one HTTP request in `crate::http`. It is a
transport-level error, not a user message: each REST client of the crate maps it onto its own
localized domain error (`impl From<HttpError> for ImageEditError`) and only that error's
`Display` is ever shown to the user.

Notes:
Target-neutral, `Clone` and `Send`: source errors (`ureq`, `std::io`, `serde_json`) are kept as
`detail` text. No variant ever carries an API key or a request body.
*/

use std::fmt;

/// Failure of one HTTP request below the HTTP status level (a non-2xx answer is an `Ok`
/// `HttpResponse`, never an `HttpError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The request could not be built (a multipart field name, a JSON body that does not
    /// serialize).
    RequestBuild { detail: String },
    /// The exchange failed (connection, DNS, TLS, IO while reading the body).
    Network { detail: String },
    /// The request exceeded its time limit; `stage` is the caller's technical step name.
    Timeout { stage: String },
    /// The response body exceeded the caller's cap of `limit_bytes`.
    BodyTooLarge { limit_bytes: u64 },
}

impl fmt::Display for HttpError {
    /// Technical, non-localized text for logs; never shown to the user as is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestBuild { detail } => write!(f, "HTTP request could not be built: {detail}"),
            Self::Network { detail } => write!(f, "HTTP exchange failed: {detail}"),
            Self::Timeout { stage } => write!(f, "HTTP request timed out ({stage})"),
            Self::BodyTooLarge { limit_bytes } => write!(f, "HTTP response body exceeds {limit_bytes} bytes"),
        }
    }
}

impl std::error::Error for HttpError {}
