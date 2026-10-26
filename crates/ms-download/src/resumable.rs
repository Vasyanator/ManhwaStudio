/*
File: crates/ms-download/src/resumable.rs

Purpose:
The resumable, retrying, cancellable per-file HTTP fetch into a `.part` staging file. Every
Rust downloader uses it: today the pinned external-model downloader
(`ms_sysprobe::ai_models::external`, size + sha256 known up front) and the app-managed
main-repository downloader (`ms_sysprobe::ai_models`, size learned from the server).

Key items:
- `RangeFetcher` + `FetchResponse` (+ `RemoteIdentity`) + `FetchError` + `classify_status()`: the HTTP seam;
  `UreqFetcher` (built from `FetcherConfig`: user agent, bearer, extra headers, timeouts) is
  the production implementation, tests inject in-memory ones.
- `RetryPolicy`: capped exponential backoff with an injected sleep.
- `ResumableFetch::fetch_part()`: streams one file into its `.part` from the staged length
  until the whole file is staged and fsynced; the caller verifies and publishes it.
- `sleep_cancellable()` (crate-internal): the sliced, cancel-polling wait shared by the
  in-memory fetch and the busy wait.

Notes:
- Size rule: a pinned size (`PartRequest::expected_size`) is authoritative. Without one the
  size comes from the server — `Content-Length` of a 200, the total of a 206's
  `Content-Range` — and a download whose size the server never announced is refused rather
  than published unverified. The fetcher asks for `Accept-Encoding: identity`, because ureq
  otherwise negotiates gzip, decompresses transparently and drops `Content-Length`, and a
  `Range` offset must count bytes of the file itself.
- Staged bytes are deleted only when PROVEN wrong: more bytes than the size, a 206 whose
  total differs from the size already learned (the remote file changed), or a 416 showing
  that the `.part` is longer than the remote file. A transfer that stops early keeps them.
- The bearer token (if a caller passes one) is only ever a request header; it is never
  logged and is sent only to the first hop of a redirect chain and to later hops on that
  hop's own origin (`bearer_allowed`: same scheme + authority, e.g. Hugging Face's same-host
  `/api/resolve-cache/…` redirect), never to another host (the LFS / xet CDN).
- `UreqFetcher` follows redirects itself so a response can carry its `RemoteIdentity`
  (`X-Linked-Etag` of a Hugging Face LFS redirect, else the final `ETag`).
*/

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::Digest;

/// Response status the downloader distinguishes: a whole body or the requested suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStatus {
    /// HTTP 200: the body is the whole file, whatever offset was asked for.
    Full,
    /// HTTP 206: the body starts at the requested offset.
    Partial,
}

/// An opened HTTP response body.
pub struct FetchResponse {
    /// How `body` relates to the requested offset.
    pub status: FetchStatus,
    /// Size of the WHOLE file as the server announced it: `Content-Length` of a 200, the
    /// total of a 206's `Content-Range`; `None` when not announced (or `*`).
    pub total_len: Option<u64>,
    /// Which version of the remote object answered, when the server said so (see
    /// [`RemoteIdentity`]); `None` when it sent no entity tag.
    pub identity: Option<RemoteIdentity>,
    /// The streamed body.
    pub body: Box<dyn Read + Send>,
}

/// The server's name for one version of a remote file: its entity tag, so two responses with
/// equal identities belong to the same bytes and a staged prefix may be extended across them.
///
/// `UreqFetcher` takes `etag` from the first `X-Linked-Etag` seen on the redirect chain
/// (Hugging Face sends it on the resolve hop of an LFS file, before redirecting to the CDN),
/// otherwise from the final response's `ETag`; both without quotes. Weak tags (`W/…`) are
/// ignored: they never make an identity. `lfs_sha256` is set ONLY from `X-Linked-Etag` and only when it is 64 hex digits:
/// for a Hugging Face LFS (or Xet) file that tag is the sha256 of the whole file, so it can
/// verify content that has no pinned hash. A plain `ETag` is never read as a hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIdentity {
    /// Normalized entity tag (never empty).
    pub etag: String,
    /// Lowercase sha256 of the whole file announced by `X-Linked-Etag`, if any.
    pub lfs_sha256: Option<String>,
}

impl RemoteIdentity {
    /// The identity a response's raw headers give: `linked_etag` is the first `X-Linked-Etag`
    /// of the redirect chain, `etag` the final response's `ETag`. `None` when neither holds a
    /// non-empty STRONG tag.
    #[must_use]
    pub fn from_headers(linked_etag: Option<&str>, etag: Option<&str>) -> Option<Self> {
        let linked = linked_etag.and_then(normalize_etag);
        let lfs_sha256 = linked
            .as_deref()
            .filter(|tag| tag.len() == 64 && tag.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase);
        let etag = linked.or_else(|| etag.and_then(normalize_etag))?;
        Some(Self { etag, lfs_sha256 })
    }
}

/// A STRONG entity tag without surrounding whitespace and quotes; `None` when nothing is
/// left or the tag is weak (`W/…`): a weak validator does not promise byte-equal content
/// (RFC 9110 §8.8.1), so it can never bind a staged prefix to the remote bytes.
fn normalize_etag(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("W/") || trimmed.starts_with("w/") {
        return None;
    }
    let unquoted = trimmed.trim_matches('"').trim();
    (!unquoted.is_empty()).then(|| unquoted.to_owned())
}

/// Why a request could not be opened; decides whether the downloader retries it. `detail`
/// is English diagnostic text without secrets (it reaches the log only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Worth retrying: connect / DNS / I/O transport failures, HTTP 408, 429 and 5xx.
    /// `retry_after` is the server's `Retry-After` when it sent one in delta-seconds form.
    Transient { detail: String, retry_after: Option<Duration> },
    /// Retrying cannot help: any other HTTP status, a malformed URL or proxy setting, a
    /// redirect loop, a 206 for another range. `status` is the HTTP status code when the
    /// failure IS an HTTP answer (e.g. `Some(401)`), `None` for transport and protocol
    /// failures, so a consumer can map an access refusal without parsing `detail`.
    Permanent { detail: String, status: Option<u16> },
    /// HTTP 416 to a request with a non-zero offset: the offset is at or past the end of the
    /// remote file. `total` is the file size from `Content-Range: bytes */N`, if sent.
    RangeNotSatisfiable { total: Option<u64> },
}

/// Classifies an HTTP error status: 408 (request timeout), 429 (rate limit) and every 5xx
/// are transient, everything else permanent (carrying `status: Some(code)`). `retry_after` is
/// the raw `Retry-After` header; only the delta-seconds form is honoured (an HTTP-date falls
/// back to the backoff).
#[must_use]
pub fn classify_status(code: u16, retry_after: Option<&str>) -> FetchError {
    let detail = format!("HTTP status {code}");
    if code == 408 || code == 429 || (500..=599).contains(&code) {
        let retry_after = retry_after.and_then(|value| value.trim().parse::<u64>().ok()).map(Duration::from_secs);
        FetchError::Transient { detail, retry_after }
    } else {
        FetchError::Permanent { detail, status: Some(code) }
    }
}

/// The HTTP seam of the downloaders; tests inject an in-memory implementation.
pub trait RangeFetcher {
    /// Opens `url`, asking for the bytes from `offset` on when `offset > 0`. A `Partial`
    /// answer must start exactly at `offset` (the implementation checks it). Errors while
    /// READING the returned body are always treated as transient by the downloader.
    ///
    /// # Errors
    /// A classified [`FetchError`] for any transport or status failure;
    /// `RangeNotSatisfiable` only for `offset > 0`.
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError>;
}

/// Retry schedule for transient failures, with the backoff sleep injected so tests never
/// wait real time.
///
/// Retry `n` (1-based count of consecutive failed attempts that staged no new bytes) waits
/// `first_delay * 2^(n-1)`, capped at `max_delay`, or the server's `Retry-After` when that is
/// longer (itself capped at `retry_after_cap`). After `max_retries` such retries the last
/// failure is returned as [`StreamError::Http`].
pub struct RetryPolicy<'a> {
    /// Retries allowed in a row without progress.
    pub max_retries: u32,
    /// Wait before the first retry.
    pub first_delay: Duration,
    /// Upper bound of the exponential backoff.
    pub max_delay: Duration,
    /// Upper bound of a server-requested `Retry-After` wait.
    pub retry_after_cap: Duration,
    /// Longest single sleep; the cancel flag is polled between slices.
    pub wait_slice: Duration,
    /// Sleeps for one slice (production: `std::thread::sleep`).
    pub sleep: &'a dyn Fn(Duration),
}

impl RetryPolicy<'static> {
    /// Production schedule: 5 retries without progress, waiting 2, 4, 8, 16 and 30 s
    /// (`Retry-After` honoured up to 60 s), cancel polled every 100 ms. With the ureq read
    /// timeout of 60 s, a link that is completely down fails after about 6 minutes.
    pub fn standard() -> Self {
        Self {
            max_retries: 5,
            first_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            retry_after_cap: Duration::from_secs(60),
            wait_slice: Duration::from_millis(100),
            sleep: &std::thread::sleep,
        }
    }
}

impl RetryPolicy<'_> {
    /// Wait before retry `attempt` (1-based); see the type comment for the formula.
    pub fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        // 2^(attempt-1) saturates at u32::MAX for absurd attempt counts; the cap applies anyway.
        let factor = 1_u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        let backoff = self.first_delay.saturating_mul(factor).min(self.max_delay);
        retry_after.map_or(backoff, |requested| requested.min(self.retry_after_cap).max(backoff))
    }
}

/// One file to stage.
#[derive(Debug, Clone, Copy)]
pub struct PartRequest<'a> {
    /// Resolve URL of the file.
    pub url: &'a str,
    /// The `.part` staging file; its parent directory must exist.
    pub part: &'a Path,
    /// Bytes already staged in `part` (and already fed to the caller's hasher, if any).
    pub offset: u64,
    /// The pinned size, authoritative when known; `None` = learn it from the server.
    pub expected_size: Option<u64>,
}

/// Progress of [`ResumableFetch::fetch_part`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEvent {
    /// `staged` bytes of the file are in the `.part` (after every read of at most 128 KiB);
    /// `total` is the file size (pinned, or as announced by the server so far), `None` while
    /// no size is known.
    Downloading { staged: u64, total: Option<u64> },
    /// A transient failure; waiting before retry `attempt` of `max_attempts`, `delay_secs`
    /// left (rounded up; re-sent whenever it changes, last with `0`). `staged` is the length
    /// the retry resumes from.
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64, staged: u64 },
}

/// Failure of [`ResumableFetch::fetch_part`]. Callers map it into their own typed error.
#[derive(Debug)]
pub enum StreamError {
    /// The cancel flag was set (also seen during a retry wait); the `.part` is kept.
    Cancelled,
    /// A filesystem operation on `path` failed; the `.part` is kept.
    Io { path: PathBuf, source: std::io::Error },
    /// A permanent HTTP failure, retries exhausted, or a size the server never announced;
    /// the `.part` is kept for a later resume. English diagnostic `detail`, no secrets.
    /// `status` is the HTTP status of a permanent HTTP answer (from
    /// [`FetchError::Permanent`], or `Some(416)` for an unsatisfiable resume of a pinned
    /// file); `None` for transport / protocol failures, exhausted retries and an unannounced size.
    Http { detail: String, status: Option<u16> },
    /// The staged bytes are proven wrong — more bytes than the size, or a 206 announcing a
    /// different total than the size already known — and the `.part` was deleted.
    /// `actual` is the length (or announced total) that contradicted `expected`.
    SizeMismatch { expected: u64, actual: u64 },
}

/// The per-file download engine: fetcher, retry schedule, cancel flag and log labels.
pub struct ResumableFetch<'a> {
    /// The HTTP seam every request goes through.
    pub fetcher: &'a dyn RangeFetcher,
    /// Schedule of transient-failure retries.
    pub retry: &'a RetryPolicy<'a>,
    /// Polled before every request, after every read and every `wait_slice` of a backoff.
    pub cancel: &'a AtomicBool,
    /// Log prefix, e.g. `[external-models]`.
    pub log_tag: &'a str,
    /// Identifies the file in log lines (`id=… file=…`); never a token or full URL query.
    pub log_context: &'a str,
}

/// Read buffer size: the upper bound of one read, hence the coarsest cancel and progress
/// granularity (a network read usually returns far less).
pub const CHUNK: usize = 128 * 1024;

/// Mutable state of one `fetch_part` call.
struct Staged {
    /// Bytes in the `.part`.
    written: u64,
    /// The file size: pinned, or learned from the server; `None` until announced.
    total: Option<u64>,
}

/// How one attempt ended without an error.
enum AttemptEnd {
    /// Exactly `total` bytes are staged and fsynced.
    Complete,
    /// The body ended cleanly before `total` (truncated chunked / close-delimited body).
    EndedEarly,
    /// The `.part` was proven longer than the remote file and deleted: start again at 0.
    Restarted,
}

/// Outcome of one failed request attempt.
enum AttemptError {
    /// Returned to the caller as is.
    Fatal(StreamError),
    /// The staged prefix is valid and the failure may pass: retry after a backoff.
    Transient { detail: String, retry_after: Option<Duration> },
}

impl From<StreamError> for AttemptError {
    fn from(error: StreamError) -> Self {
        Self::Fatal(error)
    }
}

impl ResumableFetch<'_> {
    /// Streams the rest of `request.url` into `request.part` from `request.offset` until the
    /// whole file is staged, fsyncs the `.part` and returns the file size. `hasher` (when
    /// given) already holds the staged prefix and receives every new byte; it is reset when
    /// the server restarts the file from zero.
    ///
    /// A transient failure (connect / DNS / I/O, any error while reading a body, a body that
    /// ends before the size, HTTP 408 / 429 / 5xx) keeps the staged bytes and is retried per
    /// `self.retry`, resuming with `Range` from the staged length; the retry counter resets
    /// whenever an attempt staged new bytes. Verifying the CONTENT (hash) and publishing the
    /// `.part` stay with the caller.
    ///
    /// # Errors
    /// `Cancelled`, `Io`, `SizeMismatch` (`.part` deleted), `Http` (permanent failure,
    /// retries exhausted, or no size known at the end of the body; `.part` kept).
    pub fn fetch_part(
        &self,
        request: &PartRequest<'_>,
        mut hasher: Option<&mut sha2::Sha256>,
        progress: &mut dyn FnMut(StreamEvent),
    ) -> Result<u64, StreamError> {
        let mut state = Staged { written: request.offset, total: request.expected_size };
        if state.total == Some(state.written) {
            // A complete `.part` left by an interrupted earlier run: make it durable before
            // the caller publishes it.
            sync_file(request.part)?;
            return Ok(state.written);
        }
        let mut failures = 0_u32;
        loop {
            let before = state.written;
            let (detail, retry_after) = match self.attempt(request, &mut state, hasher.as_deref_mut(), progress) {
                Ok(AttemptEnd::Complete) => return Ok(state.written),
                Ok(AttemptEnd::Restarted) => continue,
                Ok(AttemptEnd::EndedEarly) => {
                    let total = state.total.map_or_else(|| "?".to_owned(), |total| total.to_string());
                    (format!("the response body ended at byte {} of {total}", state.written), None)
                }
                Err(AttemptError::Fatal(error)) => return Err(error),
                Err(AttemptError::Transient { detail, retry_after }) => (detail, retry_after),
            };
            if state.written > before {
                failures = 0;
            }
            failures = failures.saturating_add(1);
            if failures > self.retry.max_retries {
                return Err(StreamError::Http {
                    detail: format!("{detail} (gave up after {} retries without progress)", self.retry.max_retries),
                    status: None,
                });
            }
            let delay = self.retry.delay_for(failures, retry_after);
            ms_log::runtime_log::log_warn(format!(
                "{} transient download failure, retrying: {} offset={} attempt={failures}/{} delay_ms={} error={detail}",
                self.log_tag,
                self.log_context,
                state.written,
                self.retry.max_retries,
                delay.as_millis()
            ));
            self.wait_before_retry(state.written, failures, delay, progress)?;
        }
    }

    /// One request for the rest of the file, appending to the `.part` and advancing
    /// `state.written` (and `hasher`) per chunk, so the staged length stays correct when the
    /// attempt fails. A 200 answer restarts the file from zero.
    fn attempt(
        &self,
        request: &PartRequest<'_>,
        state: &mut Staged,
        mut hasher: Option<&mut sha2::Sha256>,
        progress: &mut dyn FnMut(StreamEvent),
    ) -> Result<AttemptEnd, AttemptError> {
        self.check_cancel()?;
        let offset = state.written;
        let part = request.part;
        let response = match self.fetcher.fetch(request.url, offset) {
            Ok(response) => response,
            Err(FetchError::Transient { detail, retry_after }) => return Err(AttemptError::Transient { detail, retry_after }),
            Err(FetchError::Permanent { detail, status }) => return Err(AttemptError::Fatal(StreamError::Http { detail, status })),
            Err(FetchError::RangeNotSatisfiable { total }) => return self.range_not_satisfiable(request, state, total, hasher),
        };
        let mut handle = match response.status {
            FetchStatus::Partial => {
                // Without a pin, a changed total means the remote file changed between
                // requests: the staged prefix belongs to another version. With a pin, the pin
                // is authoritative and the overflow / short-body checks below decide.
                if request.expected_size.is_none()
                    && let (Some(known), Some(announced)) = (state.total, response.total_len)
                    && known != announced
                {
                    remove_file_if_exists(part)?;
                    return Err(AttemptError::Fatal(StreamError::SizeMismatch { expected: known, actual: announced }));
                }
                if state.total.is_none() {
                    state.total = response.total_len;
                }
                // Offset 0 means no `.part` exists yet; otherwise the staged prefix is extended.
                let handle = if offset == 0 { std::fs::File::create(part) } else { std::fs::OpenOptions::new().append(true).open(part) };
                handle.map_err(|source| io_error(part, source))?
            }
            FetchStatus::Full => {
                if offset > 0 {
                    ms_log::runtime_log::log_info(format!(
                        "{} server ignored the range request, restarting: {} offset={offset}",
                        self.log_tag, self.log_context
                    ));
                }
                let handle = std::fs::File::create(part).map_err(|source| io_error(part, source))?;
                if let Some(hasher) = hasher.as_deref_mut() {
                    *hasher = sha2::Sha256::new();
                }
                state.written = 0;
                if request.expected_size.is_none() {
                    // A whole body is a fresh copy of the file: its announcement replaces any
                    // size learned from an earlier response.
                    state.total = response.total_len;
                }
                handle
            }
        };
        let mut body = response.body;
        let mut buffer = vec![0_u8; CHUNK];
        loop {
            self.check_cancel()?;
            // Any failure of the body stream (reset, read timeout, ureq's `UnexpectedEof` for
            // a body shorter than its Content-Length) leaves a valid staged prefix: retry it.
            let read = body
                .read(&mut buffer)
                .map_err(|err| AttemptError::Transient { detail: format!("reading the response failed: {err}"), retry_after: None })?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            let next = state.written.saturating_add(chunk_len(read));
            if let Some(total) = state.total
                && next > total
            {
                drop(handle);
                remove_file_if_exists(part)?;
                return Err(AttemptError::Fatal(StreamError::SizeMismatch { expected: total, actual: next }));
            }
            handle.write_all(chunk).map_err(|source| io_error(part, source))?;
            if let Some(hasher) = hasher.as_deref_mut() {
                hasher.update(chunk);
            }
            state.written = next;
            progress(StreamEvent::Downloading { staged: next, total: state.total });
        }
        match state.total {
            Some(total) if state.written == total => {
                handle.sync_all().map_err(|source| io_error(part, source))?;
                Ok(AttemptEnd::Complete)
            }
            Some(_) => Ok(AttemptEnd::EndedEarly),
            // Nothing to compare the staged length with: publishing it could publish a
            // truncated file. The `.part` stays; a later resume usually gets a
            // `Content-Range` total.
            None => Err(AttemptError::Fatal(StreamError::Http {
                detail: format!("the server announced no file size, so the {} downloaded bytes cannot be verified", state.written),
                status: None,
            })),
        }
    }

    /// Handles a 416 to a resume request. With a pinned size it is a permanent failure (the
    /// `.part` is shorter than the pin, so the server's file is not the pinned one). Without
    /// one: a total equal to the staged length means the `.part` already holds the whole
    /// file; anything else proves the `.part` longer than the remote file, so it is deleted
    /// and the file restarts from zero (offset 0 can never yield another 416 here).
    fn range_not_satisfiable(
        &self,
        request: &PartRequest<'_>,
        state: &mut Staged,
        total: Option<u64>,
        hasher: Option<&mut sha2::Sha256>,
    ) -> Result<AttemptEnd, AttemptError> {
        let offset = state.written;
        if offset == 0 || request.expected_size.is_some() {
            return Err(AttemptError::Fatal(StreamError::Http { detail: format!("HTTP status 416: range from byte {offset} is not satisfiable"), status: Some(416) }));
        }
        if total == Some(offset) {
            sync_file(request.part)?;
            state.total = total;
            return Ok(AttemptEnd::Complete);
        }
        ms_log::runtime_log::log_warn(format!(
            "{} staged part is longer than the remote file, restarting: {} staged={offset} remote_total={total:?}",
            self.log_tag, self.log_context
        ));
        remove_file_if_exists(request.part)?;
        if let Some(hasher) = hasher {
            *hasher = sha2::Sha256::new();
        }
        state.written = 0;
        state.total = None;
        Ok(AttemptEnd::Restarted)
    }

    /// Waits `delay` before retry `attempt`, sleeping in `wait_slice` steps and polling the
    /// cancel flag between them. Reports `Retrying` whenever the remaining whole seconds
    /// change, last with `delay_secs: 0` right before returning.
    ///
    /// # Errors
    /// `Cancelled` as soon as the flag is seen.
    fn wait_before_retry(&self, staged: u64, attempt: u32, delay: Duration, progress: &mut dyn FnMut(StreamEvent)) -> Result<(), StreamError> {
        let mut remaining = delay;
        let mut shown = None;
        loop {
            self.check_cancel()?;
            // Whole seconds, rounded up, so "in 1 s" is shown until the wait is really over.
            let delay_secs = remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0));
            if shown != Some(delay_secs) {
                shown = Some(delay_secs);
                progress(StreamEvent::Retrying { attempt, max_attempts: self.retry.max_retries, delay_secs, staged });
            }
            if remaining.is_zero() {
                return Ok(());
            }
            // A zero slice would never advance the wait; one millisecond is the floor.
            let slice = remaining.min(self.retry.wait_slice.max(Duration::from_millis(1)));
            (self.retry.sleep)(slice);
            remaining = remaining.saturating_sub(slice);
        }
    }

    fn check_cancel(&self) -> Result<(), StreamError> {
        if self.cancel.load(Ordering::Relaxed) { Err(StreamError::Cancelled) } else { Ok(()) }
    }
}

/// Sleeps `delay` in `retry.wait_slice` steps (at least 1 ms each), polling `cancel` before
/// every slice. Returns `true` as soon as the flag is seen (the wait is abandoned), `false`
/// after the whole delay.
pub(crate) fn sleep_cancellable(retry: &RetryPolicy<'_>, cancel: &AtomicBool, delay: Duration) -> bool {
    let mut remaining = delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        if remaining.is_zero() {
            return false;
        }
        let slice = remaining.min(retry.wait_slice.max(Duration::from_millis(1)));
        (retry.sleep)(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// Opens an existing file for writing and fsyncs it.
fn sync_file(path: &Path) -> Result<(), StreamError> {
    let handle = std::fs::OpenOptions::new().write(true).open(path).map_err(|source| io_error(path, source))?;
    handle.sync_all().map_err(|source| io_error(path, source))
}

fn remove_file_if_exists(path: &Path) -> Result<(), StreamError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> StreamError {
    StreamError::Io { path: path.to_path_buf(), source }
}

/// A read length as `u64`. `usize` is at most 64 bits on every supported target, so the
/// saturating fallback is unreachable; it only avoids a lossy `as`.
pub fn chunk_len(read: usize) -> u64 {
    u64::try_from(read).unwrap_or(u64::MAX)
}

/// Total file size from a `Content-Range` value (`bytes a-b/N` or `bytes */N`); `None` for
/// an unknown (`*`) or malformed total.
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/').and_then(|(_, total)| total.trim().parse::<u64>().ok())
}

/// Construction options of [`UreqFetcher`]. `Default` is the project's network rule: 15 s
/// connect, 60 s read, no token, no extra headers, ureq's own user agent.
#[derive(Clone)]
pub struct FetcherConfig {
    /// `User-Agent` of every request; `None` keeps ureq's default.
    pub user_agent: Option<String>,
    /// Access token sent as `Authorization: Bearer …`; never logged (see [`UreqFetcher`]).
    pub bearer: Option<String>,
    /// Extra request headers (name, value), sent on every request after the built-in ones.
    /// Values may be secrets: they are never logged and `Debug` prints only the names.
    pub headers: Vec<(String, String)>,
    /// Upper bound of DNS + TCP/TLS connect.
    pub connect_timeout: Duration,
    /// Upper bound of one socket read; a stalled body fails (transiently) after it.
    pub read_timeout: Duration,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self {
            user_agent: None,
            bearer: None,
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(60),
        }
    }
}

impl std::fmt::Debug for FetcherConfig {
    // Redacts the token and the header values, which may carry credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("FetcherConfig")
            .field("user_agent", &self.user_agent)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .field("header_names", &header_names)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

/// Redirect hops [`UreqFetcher`] follows per request (ureq's own default).
const MAX_REDIRECTS: usize = 5;

/// The production [`RangeFetcher`] over a ureq agent.
///
/// Redirects are followed HERE, not by ureq (the agent is built with `redirects(0)`), because
/// the response identity of a Hugging Face LFS file (`X-Linked-Etag`) is a header of the
/// redirect hop that ureq would discard. Hop rules: at most [`MAX_REDIRECTS`] hops, targets
/// resolved per RFC 3986, only same-scheme or `http` -> `https` (an `https` -> `http`
/// downgrade is a permanent, logged failure), `Authorization` only on the first request and on
/// hops of the first request's own origin ([`bearer_allowed`]), every other header (`Range`,
/// `Accept-Encoding`, the configured extras) on every hop.
pub struct UreqFetcher {
    agent: ureq::Agent,
    /// Access token (e.g. the Hugging Face token); sent as a bearer header to the first hop and
    /// to same-origin hops only (the LFS CDN is another host), never logged.
    bearer: Option<String>,
    /// Extra headers of [`FetcherConfig::headers`], set on every request and every hop.
    headers: Vec<(String, String)>,
}

impl UreqFetcher {
    /// An agent with a 15 s connect and 60 s read timeout that honours the proxy
    /// environment; `bearer` is an access token (e.g. the Hugging Face token), if the caller uses one.
    pub fn new(bearer: Option<String>) -> Self {
        Self::with_config(FetcherConfig { bearer, ..FetcherConfig::default() })
    }

    /// An agent built from `config` that honours the proxy environment.
    pub fn with_config(config: FetcherConfig) -> Self {
        let FetcherConfig { user_agent, bearer, headers, connect_timeout, read_timeout } = config;
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(connect_timeout)
            .timeout_read(read_timeout)
            .try_proxy_from_env(true)
            // 3xx answers come back as responses; `fetch` follows them itself (type comment).
            .redirects(0);
        if let Some(user_agent) = &user_agent {
            builder = builder.user_agent(user_agent);
        }
        Self { agent: builder.build(), bearer, headers }
    }

    /// Sends one GET of `url` and maps a failure; `authorize` adds the bearer token (the
    /// caller decides per hop, see [`bearer_allowed`]).
    fn send(&self, url: &str, offset: u64, authorize: bool) -> Result<ureq::Response, FetchError> {
        // Identity encoding: byte offsets and Content-Length must describe the file itself
        // (see the file header).
        let mut request = self.agent.get(url).set("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
        }
        if authorize
            && let Some(token) = &self.bearer
        {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        for (name, value) in &self.headers {
            request = request.set(name, value);
        }
        match request.call() {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(416, response)) if offset > 0 => {
                Err(FetchError::RangeNotSatisfiable { total: response.header("Content-Range").and_then(content_range_total) })
            }
            Err(ureq::Error::Status(code, response)) => Err(classify_status(code, response.header("Retry-After"))),
            Err(ureq::Error::Transport(transport)) => {
                let detail = transport.to_string();
                Err(if transport_is_transient(transport.kind()) {
                    FetchError::Transient { detail, retry_after: None }
                } else {
                    FetchError::Permanent { detail, status: None }
                })
            }
        }
    }
}

impl RangeFetcher for UreqFetcher {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        let mut current = url.to_owned();
        let mut linked_etag: Option<String> = None;
        for hop in 0..=MAX_REDIRECTS {
            // The token follows a same-origin redirect (Hugging Face resolves small files of a
            // gated repository through a same-host hop) but never leaves for another host.
            let response = self.send(&current, offset, hop == 0 || bearer_allowed(url, &current))?;
            if linked_etag.is_none() {
                linked_etag = response.header("X-Linked-Etag").map(str::to_owned);
            }
            if (300..400).contains(&response.status()) {
                let location = response.header("Location").ok_or_else(|| FetchError::Permanent {
                    detail: format!("HTTP status {} without a Location header", response.status()),
                    status: Some(response.status()),
                })?;
                current = resolve_location(&current, location).map_err(|detail| {
                    // Never logs the target URL itself (a signed CDN URL); the reason is enough.
                    ms_log::runtime_log::log_warn(format!("[download] redirect not followed: hop={hop} reason={detail}"));
                    FetchError::Permanent { detail, status: None }
                })?;
                continue;
            }
            let (status, total_len) = match response.status() {
                // ureq removes Content-Length when it decompresses a body, so a size read here
                // always counts the bytes the reader yields.
                200 => (FetchStatus::Full, response.header("Content-Length").and_then(|value| value.trim().parse::<u64>().ok())),
                206 => {
                    // A 206 for another range would silently corrupt the resumed file.
                    let range = response.header("Content-Range").unwrap_or_default();
                    if !range.starts_with(&format!("bytes {offset}-")) {
                        return Err(FetchError::Permanent { detail: format!("unexpected Content-Range '{range}' for offset {offset}"), status: None });
                    }
                    (FetchStatus::Partial, content_range_total(range))
                }
                other => return Err(FetchError::Permanent { detail: format!("unexpected HTTP status {other}"), status: Some(other) }),
            };
            let identity = RemoteIdentity::from_headers(linked_etag.as_deref(), response.header("ETag"));
            return Ok(FetchResponse { status, total_len, identity, body: Box::new(response.into_reader()) });
        }
        Err(FetchError::Permanent { detail: format!("more than {MAX_REDIRECTS} redirects"), status: None })
    }
}

/// The components of a URI reference (RFC 3986 appendix B); the fragment is dropped
/// (requests never carry one).
struct UriRef<'a> {
    scheme: Option<&'a str>,
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
}

/// Splits `reference` per RFC 3986 appendix B
/// (`^(([^:/?#]+):)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?`).
fn parse_uri_ref(reference: &str) -> UriRef<'_> {
    let without_fragment = reference.split_once('#').map_or(reference, |(head, _)| head);
    let (scheme, rest) = match without_fragment.find([':', '/', '?']) {
        Some(index) if index > 0 && without_fragment.as_bytes()[index] == b':' => (Some(&without_fragment[..index]), &without_fragment[index + 1..]),
        _ => (None, without_fragment),
    };
    let (authority, rest) = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?']).unwrap_or(after.len());
            (Some(&after[..end]), &after[end..])
        }
        None => (None, rest),
    };
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    UriRef { scheme, authority, path, query }
}

/// RFC 3986 §5.2.4 `remove_dot_segments`.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../").or_else(|| input.strip_prefix("./")) {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            let cut = output.rfind('/').unwrap_or(0);
            output.truncate(cut);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            // Move the first segment (with its leading `/`, if any) to the output.
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |index| start + index);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

/// The absolute URL a `Location` header names, resolved against the current request URL
/// `base` per RFC 3986 §5.2.2 (dot segments removed, fragment dropped).
///
/// # Errors
/// English detail for a `base` without scheme and authority, a target scheme other than
/// http(s), or an `https` -> `http` downgrade (only same-scheme and `http` -> `https`
/// redirects are followed).
fn resolve_location(base: &str, location: &str) -> Result<String, String> {
    let base_ref = parse_uri_ref(base.trim());
    let (Some(base_scheme), Some(base_authority)) = (base_ref.scheme, base_ref.authority) else {
        return Err(format!("cannot resolve a redirect against '{base}'"));
    };
    let reference = parse_uri_ref(location.trim());
    let (scheme, authority, path, query) = if let Some(scheme) = reference.scheme {
        (scheme, reference.authority, remove_dot_segments(reference.path), reference.query)
    } else if reference.authority.is_some() {
        (base_scheme, reference.authority, remove_dot_segments(reference.path), reference.query)
    } else if reference.path.is_empty() {
        (base_scheme, Some(base_authority), base_ref.path.to_owned(), reference.query.or(base_ref.query))
    } else if reference.path.starts_with('/') {
        (base_scheme, Some(base_authority), remove_dot_segments(reference.path), reference.query)
    } else {
        // §5.2.3 merge: an empty base path with an authority becomes "/".
        let merged = match base_ref.path.rfind('/') {
            Some(index) => format!("{}{}", &base_ref.path[..=index], reference.path),
            None => format!("/{}", reference.path),
        };
        (base_scheme, Some(base_authority), remove_dot_segments(&merged), reference.query)
    };
    let scheme_lower = scheme.to_ascii_lowercase();
    if scheme_lower != "https" && scheme_lower != "http" {
        return Err(format!("redirect to an unsupported scheme '{scheme}'"));
    }
    if base_scheme.eq_ignore_ascii_case("https") && scheme_lower == "http" {
        return Err("redirect from https to plain http refused".to_owned());
    }
    let Some(authority) = authority.filter(|authority| !authority.is_empty()) else {
        return Err(format!("redirect target '{location}' has no host"));
    };
    let mut url = format!("{scheme_lower}://{authority}{path}");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Ok(url)
}

/// Whether the bearer token may be sent to `target`, a hop of the redirect chain that started
/// at `first_url`: only when both have a scheme and a non-empty authority and these are equal
/// (ASCII case-insensitive; the authority includes the port, so an explicit default port
/// counts as another origin — the safe direction). The first URL is the endpoint the caller
/// chose to authenticate against (e.g. `huggingface.co` or `HF_ENDPOINT`'s host).
fn bearer_allowed(first_url: &str, target: &str) -> bool {
    let (first, next) = (parse_uri_ref(first_url.trim()), parse_uri_ref(target.trim()));
    match (first.scheme, first.authority, next.scheme, next.authority) {
        (Some(first_scheme), Some(first_authority), Some(next_scheme), Some(next_authority)) => {
            !first_authority.is_empty() && first_scheme.eq_ignore_ascii_case(next_scheme) && first_authority.eq_ignore_ascii_case(next_authority)
        }
        _ => false,
    }
}

/// Whether a ureq transport failure may pass on its own: network-level failures (DNS,
/// connect, I/O including timeouts and resets, a garbled status line or header from a flaky
/// proxy) are retried; configuration problems and redirect loops are not.
fn transport_is_transient(kind: ureq::ErrorKind) -> bool {
    use ureq::ErrorKind;

    match kind {
        ErrorKind::Dns | ErrorKind::ConnectionFailed | ErrorKind::Io | ErrorKind::BadStatus | ErrorKind::BadHeader | ErrorKind::ProxyConnect => true,
        ErrorKind::InvalidUrl
        | ErrorKind::UnknownScheme
        | ErrorKind::InsecureRequestHttpsOnly
        | ErrorKind::TooManyRedirects
        | ErrorKind::InvalidProxyUrl
        | ErrorKind::ProxyUnauthorized
        | ErrorKind::HTTP => false,
    }
}

#[cfg(test)]
mod tests;
