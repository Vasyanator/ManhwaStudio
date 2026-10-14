/*
File: crates/ms-sysprobe/src/ai_models/resumable.rs

Purpose:
The one HTTP download mechanism of `ai_models`: Hugging Face resolve URLs and the resumable,
retrying, cancellable per-file fetch into a `.part` staging file. Both downloaders use it —
the pinned external-model downloader (`external.rs`, size + sha256 known up front) and the
app-managed main-repository downloader (`ai_models.rs`, size learned from the server).
Native-only (the parent mounts it under `cfg(not(wasm32))`).

Key items:
- `hf_resolve_url()`: `{HF_ENDPOINT or https://huggingface.co}/{repo}/resolve/{rev}/{path}`
  with every component percent-encoded. Replaces `hf_hub::ApiBuilder::from_env`, which
  panics without a home directory (Android app processes have none).
- `RangeFetcher` + `FetchResponse` + `FetchError` + `classify_status()`: the HTTP seam;
  `UreqFetcher` is the production implementation, tests inject in-memory ones.
- `RetryPolicy`: capped exponential backoff with an injected sleep.
- `ResumableFetch::fetch_part()`: streams one file into its `.part` from the staged length
  until the whole file is staged and fsynced; the caller verifies and publishes it.

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
  logged, and ureq drops `Authorization` on every redirect.
*/

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::Digest;

/// Environment variable that redirects Hugging Face requests (mirrors, proxies, tests);
/// the same name `huggingface_hub` and `hf-hub` honour.
pub(crate) const HF_ENDPOINT_ENV: &str = "HF_ENDPOINT";
/// Endpoint used when `HF_ENDPOINT` is unset or empty.
pub(crate) const DEFAULT_HF_ENDPOINT: &str = "https://huggingface.co";

/// Resolve URL of `path` in the model repository `repo_id` at `revision`, on the endpoint
/// named by `HF_ENDPOINT` (default `https://huggingface.co`). Reads the environment on every
/// call and never touches the home directory.
#[must_use]
pub(crate) fn hf_resolve_url(repo_id: &str, revision: &str, path: &str) -> String {
    let configured = std::env::var(HF_ENDPOINT_ENV).ok();
    hf_resolve_url_at(endpoint_from_env_value(configured.as_deref()), repo_id, revision, path)
}

/// The endpoint a raw `HF_ENDPOINT` value selects: the default for an absent or blank value,
/// otherwise the trimmed value without trailing slashes.
#[must_use]
pub(crate) fn endpoint_from_env_value(value: Option<&str>) -> &str {
    match value.map(str::trim) {
        Some(endpoint) if !endpoint.is_empty() => endpoint.trim_end_matches('/'),
        Some(_) | None => DEFAULT_HF_ENDPOINT,
    }
}

/// [`hf_resolve_url`] with an explicit endpoint (no trailing slash). `repo_id` (`owner/name`)
/// and `path` are `/`-separated: each segment is percent-encoded and the separators kept;
/// `revision` is encoded as ONE segment (a `/` in a branch name becomes `%2F`, as the Hub
/// expects).
#[must_use]
pub(crate) fn hf_resolve_url_at(endpoint: &str, repo_id: &str, revision: &str, path: &str) -> String {
    format!("{endpoint}/{}/resolve/{}/{}", encode_segments(repo_id), percent_encode(revision), encode_segments(path))
}

/// Percent-encodes every `/`-separated segment of `value`, keeping the separators.
fn encode_segments(value: &str) -> String {
    value.split('/').map(percent_encode).collect::<Vec<_>>().join("/")
}

/// RFC 3986 percent-encoding of one path segment: unreserved characters stay, every other
/// UTF-8 byte becomes `%XX` (uppercase hex).
fn percent_encode(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// Response status the downloader distinguishes: a whole body or the requested suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// HTTP 200: the body is the whole file, whatever offset was asked for.
    Full,
    /// HTTP 206: the body starts at the requested offset.
    Partial,
}

/// An opened HTTP response body.
pub(crate) struct FetchResponse {
    /// How `body` relates to the requested offset.
    pub status: FetchStatus,
    /// Size of the WHOLE file as the server announced it: `Content-Length` of a 200, the
    /// total of a 206's `Content-Range`; `None` when not announced (or `*`).
    pub total_len: Option<u64>,
    /// The streamed body.
    pub body: Box<dyn Read + Send>,
}

/// Why a request could not be opened; decides whether the downloader retries it. `detail`
/// is English diagnostic text without secrets (it reaches the log only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    /// Worth retrying: connect / DNS / I/O transport failures, HTTP 408, 429 and 5xx.
    /// `retry_after` is the server's `Retry-After` when it sent one in delta-seconds form.
    Transient { detail: String, retry_after: Option<Duration> },
    /// Retrying cannot help: any other HTTP status, a malformed URL or proxy setting, a
    /// redirect loop, a 206 for another range.
    Permanent { detail: String },
    /// HTTP 416 to a request with a non-zero offset: the offset is at or past the end of the
    /// remote file. `total` is the file size from `Content-Range: bytes */N`, if sent.
    RangeNotSatisfiable { total: Option<u64> },
}

/// Classifies an HTTP error status: 408 (request timeout), 429 (rate limit) and every 5xx
/// are transient, everything else permanent. `retry_after` is the raw `Retry-After` header;
/// only the delta-seconds form is honoured (an HTTP-date falls back to the backoff).
#[must_use]
pub(crate) fn classify_status(code: u16, retry_after: Option<&str>) -> FetchError {
    let detail = format!("HTTP status {code}");
    if code == 408 || code == 429 || (500..=599).contains(&code) {
        let retry_after = retry_after.and_then(|value| value.trim().parse::<u64>().ok()).map(Duration::from_secs);
        FetchError::Transient { detail, retry_after }
    } else {
        FetchError::Permanent { detail }
    }
}

/// The HTTP seam of the downloaders; tests inject an in-memory implementation.
pub(crate) trait RangeFetcher {
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
pub(crate) struct RetryPolicy<'a> {
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
    pub(crate) fn standard() -> Self {
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
    pub(crate) fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        // 2^(attempt-1) saturates at u32::MAX for absurd attempt counts; the cap applies anyway.
        let factor = 1_u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        let backoff = self.first_delay.saturating_mul(factor).min(self.max_delay);
        retry_after.map_or(backoff, |requested| requested.min(self.retry_after_cap).max(backoff))
    }
}

/// One file to stage.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PartRequest<'a> {
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
pub(crate) enum StreamEvent {
    /// `staged` bytes of the file are in the `.part` (after every read of at most 128 KiB).
    Downloading { staged: u64 },
    /// A transient failure; waiting before retry `attempt` of `max_attempts`, `delay_secs`
    /// left (rounded up; re-sent whenever it changes, last with `0`). `staged` is the length
    /// the retry resumes from.
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64, staged: u64 },
}

/// Failure of [`ResumableFetch::fetch_part`]. Callers map it into their own typed error.
#[derive(Debug)]
pub(crate) enum StreamError {
    /// The cancel flag was set (also seen during a retry wait); the `.part` is kept.
    Cancelled,
    /// A filesystem operation on `path` failed; the `.part` is kept.
    Io { path: PathBuf, source: std::io::Error },
    /// A permanent HTTP failure, retries exhausted, or a size the server never announced;
    /// the `.part` is kept for a later resume. English diagnostic `detail`, no secrets.
    Http { detail: String },
    /// The staged bytes are proven wrong — more bytes than the size, or a 206 announcing a
    /// different total than the size already known — and the `.part` was deleted.
    /// `actual` is the length (or announced total) that contradicted `expected`.
    SizeMismatch { expected: u64, actual: u64 },
}

/// The per-file download engine: fetcher, retry schedule, cancel flag and log labels.
pub(crate) struct ResumableFetch<'a> {
    pub fetcher: &'a dyn RangeFetcher,
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
pub(crate) const CHUNK: usize = 128 * 1024;

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
    pub(crate) fn fetch_part(
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
            Err(FetchError::Permanent { detail }) => return Err(AttemptError::Fatal(StreamError::Http { detail })),
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
            progress(StreamEvent::Downloading { staged: next });
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
            return Err(AttemptError::Fatal(StreamError::Http { detail: format!("HTTP status 416: range from byte {offset} is not satisfiable") }));
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
pub(crate) fn chunk_len(read: usize) -> u64 {
    u64::try_from(read).unwrap_or(u64::MAX)
}

/// Total file size from a `Content-Range` value (`bytes a-b/N` or `bytes */N`); `None` for
/// an unknown (`*`) or malformed total.
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/').and_then(|(_, total)| total.trim().parse::<u64>().ok())
}

/// The production [`RangeFetcher`] over a ureq agent.
pub(crate) struct UreqFetcher {
    agent: ureq::Agent,
    /// Hugging Face token; sent as a bearer header, never logged. ureq's default redirect
    /// policy drops `Authorization` on every redirect (the LFS CDN is another host), while
    /// keeping `Range`.
    bearer: Option<String>,
}

impl UreqFetcher {
    /// An agent with a 15 s connect and 60 s read timeout that honours the proxy
    /// environment; `bearer` is the Hugging Face token, if the caller uses one.
    pub(crate) fn new(bearer: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .try_proxy_from_env(true)
            .build();
        Self { agent, bearer }
    }
}

impl RangeFetcher for UreqFetcher {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        // Identity encoding: byte offsets and Content-Length must describe the file itself
        // (see the file header).
        let mut request = self.agent.get(url).set("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
        }
        if let Some(token) = &self.bearer {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(416, response)) if offset > 0 => {
                return Err(FetchError::RangeNotSatisfiable { total: response.header("Content-Range").and_then(content_range_total) });
            }
            Err(ureq::Error::Status(code, response)) => return Err(classify_status(code, response.header("Retry-After"))),
            Err(ureq::Error::Transport(transport)) => {
                let detail = transport.to_string();
                return Err(if transport_is_transient(transport.kind()) {
                    FetchError::Transient { detail, retry_after: None }
                } else {
                    FetchError::Permanent { detail }
                });
            }
        };
        let (status, total_len) = match response.status() {
            // ureq removes Content-Length when it decompresses a body, so a size read here
            // always counts the bytes the reader yields.
            200 => (FetchStatus::Full, response.header("Content-Length").and_then(|value| value.trim().parse::<u64>().ok())),
            206 => {
                // A 206 for another range would silently corrupt the resumed file.
                let range = response.header("Content-Range").unwrap_or_default();
                if !range.starts_with(&format!("bytes {offset}-")) {
                    return Err(FetchError::Permanent { detail: format!("unexpected Content-Range '{range}' for offset {offset}") });
                }
                (FetchStatus::Partial, content_range_total(range))
            }
            other => return Err(FetchError::Permanent { detail: format!("unexpected HTTP status {other}") }),
        };
        Ok(FetchResponse { status, total_len, body: Box::new(response.into_reader()) })
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
