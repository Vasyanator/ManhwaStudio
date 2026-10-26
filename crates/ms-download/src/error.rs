/*
File: crates/ms-download/src/error.rs

Purpose:
`DownloadError`, the typed failure of the verified-file, small-body and archive-member
operations of this crate (`fetch_verified`, `fetch_small`, `BusyGuard::acquire_waiting`,
`unpack::zip_member_atomic`).

Notes:
- `Display` is English diagnostic text without secrets (no token, no URL query); consumers
  log it and map the variants to their own localized messages.
- `From<StreamError>` maps the lower-level `ResumableFetch::fetch_part` failure 1:1.
*/

use std::path::PathBuf;

use crate::resumable::StreamError;

/// Failure of a download, verification or unpack step. Which staged files survive is stated
/// per variant; nothing is ever published on an error.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The cancel flag was set (during a transfer, a backoff, a hash pass or a busy wait);
    /// staged bytes are kept for a later resume.
    #[error("the download was cancelled")]
    Cancelled,
    /// Another holder in this process owns `key` (see `busy::dest_key`); nothing was touched.
    #[error("another download in this process holds '{key}'")]
    Busy { key: String },
    /// The request itself is malformed (e.g. a sha256 pin that is not 64 hex digits, a
    /// destination without a file name); nothing was touched.
    #[error("invalid download request: {detail}")]
    InvalidRequest { detail: String },
    /// A filesystem operation on `path` failed; staged bytes are kept.
    #[error("file operation failed on {}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    /// A permanent HTTP failure, retries exhausted, or a size the server never announced;
    /// staged bytes are kept. `status` is the HTTP status code of a permanent HTTP answer
    /// (e.g. `Some(401)`, so a consumer can map an access refusal without parsing `detail`);
    /// `None` for transport / protocol failures, exhausted retries and an unannounced size.
    #[error("download failed: {detail}")]
    Http { detail: String, status: Option<u16> },
    /// The content is proven longer / shorter than allowed (pinned size, announced size or a
    /// `max_bytes` cap); the staged file was deleted.
    #[error("size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    /// The complete content does not hash to the pinned sha256; the staged file was deleted.
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    /// The archive is unreadable, lacks the member, or the member's data is corrupt.
    #[error("archive error: {detail}")]
    Archive { detail: String },
}

impl From<StreamError> for DownloadError {
    fn from(error: StreamError) -> Self {
        match error {
            StreamError::Cancelled => Self::Cancelled,
            StreamError::Io { path, source } => Self::Io { path, source },
            StreamError::Http { detail, status } => Self::Http { detail, status },
            StreamError::SizeMismatch { expected, actual } => Self::SizeMismatch { expected, actual },
        }
    }
}
