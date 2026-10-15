/*
File: crates/ms-download/src/small.rs

Purpose:
In-memory download of a small body (API answers, manifests, metadata files, dictionaries)
under the same network rule as the file engine: transient failures retried with capped
exponential backoff, resume with `Range` from the bytes already received, cancel polled per
read and per backoff slice, and a hard size cap.

Key items:
- `SmallRequest`: URL + size cap.
- `fetch_small()`: the operation.

Notes:
- Unlike `ResumableFetch::fetch_part`, a body whose size was never announced is accepted at
  a clean end of stream: metadata endpoints often answer chunked, and the caller parses
  (and thereby validates) the result. A body cut by a transport error is still resumed.
- Retry warnings go to `ms_log::runtime_log` with the caller's tag and context; the caller
  logs start, finish and failure.
*/

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::DownloadError;
use crate::resumable::{CHUNK, FetchError, FetchStatus, RangeFetcher, RetryPolicy, chunk_len, sleep_cancellable};

/// One small body to fetch into memory.
#[derive(Debug, Clone, Copy)]
pub struct SmallRequest<'a> {
    /// URL of the body (never logged by this crate; `log_context` names it).
    pub url: &'a str,
    /// Largest accepted body; an announced or received size past it fails.
    pub max_bytes: u64,
}

/// How one request attempt ended.
enum Attempt {
    /// The whole body is in the buffer.
    Done,
    /// A failure that may pass: retry after a backoff.
    Transient { detail: String, retry_after: Option<Duration> },
}

/// Fetches `req.url` into memory and returns the body.
///
/// A transient failure (transport error, an error while reading the body, a body shorter
/// than its announced size, HTTP 408 / 429 / 5xx) is retried per `retry`, resuming with
/// `Range` from the bytes already received; the counter of retries without progress resets
/// whenever an attempt received new bytes. A 200 answer restarts the body; a 206 announcing
/// another total than the one already learned restarts it too (the resource changed).
///
/// # Errors
/// `Cancelled`; `SizeMismatch` when the announced or received size passes `req.max_bytes`
/// (`expected` = the cap) or the received bytes pass the announced size; `Http` for a
/// permanent failure or once the retries are exhausted.
pub fn fetch_small(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    cancel: &AtomicBool,
    req: &SmallRequest<'_>,
    log_tag: &str,
    log_context: &str,
) -> Result<Vec<u8>, DownloadError> {
    let mut buffer = Vec::new();
    let mut total: Option<u64> = None;
    let mut failures = 0_u32;
    loop {
        let before = buffer.len();
        let (detail, retry_after) = match attempt(fetcher, cancel, req, &mut buffer, &mut total, log_tag, log_context)? {
            Attempt::Done => return Ok(buffer),
            Attempt::Transient { detail, retry_after } => (detail, retry_after),
        };
        if buffer.len() > before {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        if failures > retry.max_retries {
            return Err(DownloadError::Http { detail: format!("{detail} (gave up after {} retries without progress)", retry.max_retries) });
        }
        let delay = retry.delay_for(failures, retry_after);
        ms_log::runtime_log::log_warn(format!(
            "{log_tag} transient download failure, retrying: {log_context} offset={} attempt={failures}/{} delay_ms={} error={detail}",
            buffer.len(),
            retry.max_retries,
            delay.as_millis()
        ));
        if sleep_cancellable(retry, cancel, delay) {
            return Err(DownloadError::Cancelled);
        }
    }
}

/// One request for the rest of the body, appending to `buffer`.
fn attempt(
    fetcher: &dyn RangeFetcher,
    cancel: &AtomicBool,
    req: &SmallRequest<'_>,
    buffer: &mut Vec<u8>,
    total: &mut Option<u64>,
    log_tag: &str,
    log_context: &str,
) -> Result<Attempt, DownloadError> {
    check_cancel(cancel)?;
    let offset = chunk_len(buffer.len());
    let response = match fetcher.fetch(req.url, offset) {
        Ok(response) => response,
        Err(FetchError::Transient { detail, retry_after }) => return Ok(Attempt::Transient { detail, retry_after }),
        Err(FetchError::Permanent { detail }) => return Err(DownloadError::Http { detail }),
        Err(FetchError::RangeNotSatisfiable { total: remote }) => {
            if remote == Some(offset) {
                return Ok(Attempt::Done);
            }
            // The received prefix is longer than the remote body: it belongs to another
            // version. Start over (offset 0 can never yield another 416).
            ms_log::runtime_log::log_warn(format!("{log_tag} received prefix is longer than the remote body, restarting: {log_context} received={offset} remote_total={remote:?}"));
            buffer.clear();
            *total = None;
            return Ok(Attempt::Transient { detail: format!("HTTP status 416 at byte {offset}"), retry_after: None });
        }
    };
    match response.status {
        FetchStatus::Full => {
            buffer.clear();
            *total = response.total_len;
        }
        FetchStatus::Partial => match (*total, response.total_len) {
            (Some(known), Some(announced)) if known != announced => {
                ms_log::runtime_log::log_warn(format!("{log_tag} remote body changed size, restarting: {log_context} known={known} announced={announced}"));
                buffer.clear();
                *total = None;
                return Ok(Attempt::Transient { detail: format!("the remote body changed size from {known} to {announced}"), retry_after: None });
            }
            (None, announced) => *total = announced,
            (Some(_), _) => {}
        },
    }
    if let Some(announced) = *total
        && announced > req.max_bytes
    {
        return Err(DownloadError::SizeMismatch { expected: req.max_bytes, actual: announced });
    }
    let mut body = response.body;
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        check_cancel(cancel)?;
        let read = match body.read(&mut chunk) {
            Ok(read) => read,
            Err(err) => return Ok(Attempt::Transient { detail: format!("reading the response failed: {err}"), retry_after: None }),
        };
        if read == 0 {
            break;
        }
        let next = chunk_len(buffer.len()).saturating_add(chunk_len(read));
        if next > req.max_bytes {
            return Err(DownloadError::SizeMismatch { expected: req.max_bytes, actual: next });
        }
        if let Some(announced) = *total
            && next > announced
        {
            return Err(DownloadError::SizeMismatch { expected: announced, actual: next });
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let received = chunk_len(buffer.len());
    match *total {
        // No announced size: a clean end of stream is the end of the body (see file header).
        None => Ok(Attempt::Done),
        Some(announced) if received == announced => Ok(Attempt::Done),
        Some(announced) => Ok(Attempt::Transient { detail: format!("the response body ended at byte {received} of {announced}"), retry_after: None }),
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), DownloadError> {
    if cancel.load(Ordering::Relaxed) { Err(DownloadError::Cancelled) } else { Ok(()) }
}

#[cfg(test)]
mod tests;
