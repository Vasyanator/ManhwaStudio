/*
File: crates/ms-sysprobe/src/ai_models/external.rs

Purpose:
Pinned, verified downloader for EXTERNAL (third-party Hugging Face) models, plus the
stat-only status probe that tells a caller whether such a model is installed. The specs
themselves live in `external_catalog.rs`; this file owns only the mechanics.

Key items:
- `ExternalFile` / `ExternalModelSpec`: one pinned repository revision and its file
  allowlist (exact size + lowercase sha256), and the directory relative to `side_models`.
- `external_model_status()` / `installed_dir()`: stat-only probes (no hashing, no network).
- `download_external_model()`: blocking download with byte progress, cancel, resume,
  automatic retry of transient network failures, size + sha256 verification, atomic
  per-file publish and a completion marker.
- `RangeFetcher` + `FetchError` + `RetryPolicy` + `download_with()`: the testable core with
  an injected HTTP fetcher and an injected backoff sleep.
- `ExternalModelError`: typed errors; `Display` is English diagnostic text for logs, the UI
  maps variants to localized messages.

On-disk layout of one model directory (`<side_models>/<spec.dir>`):
- the published files at their catalog paths;
- `.ms_model_complete.json`: the completion marker `{id, repo_id, revision, files}`, written
  last, so "installed" means "this exact spec was fully downloaded and verified";
- `.download/`: staging while a download is unfinished — `<path>.part` files,
  `in_progress.json` (identity of the spec the parts belong to), `previous_marker.json`
  (the retired marker of an older install) and `abandoned_files.json` (file paths of
  discarded in-progress identities of other pins); the last two feed the stale-file cleanup
  at the end.

Notes:
- Nothing here runs on the GUI thread: every function touches the filesystem, the
  download also the network. Callers own the worker.
- No implicit download anywhere: `installed_dir` only reports.
- The HF token is sent as a bearer header only to the resolve URL; ureq drops it on a
  cross-host redirect. It is never logged.
- Retry: a TRANSIENT failure (connect / DNS / reset / read timeout, a body that ends before
  the pinned size, HTTP 408 / 429 / 5xx) is retried with exponential backoff, resuming the
  `.part` with `Range`; the counter resets whenever an attempt staged new bytes. Permanent
  failures (other HTTP statuses, an unexpected `Content-Range`, local I/O, size overflow,
  hash mismatch) end the download at once. The backoff wait polls the cancel flag.
- Durability: a `.part` is fsynced only when its stream completes. An APPLICATION crash
  loses nothing (the bytes are in the OS page cache); after an OS crash or power loss the
  unsynced tail of a `.part` may hold garbage on some filesystems. That is not detected
  while resuming (no durable checkpoint is kept); the final sha256 check catches it, deletes
  the `.part` and the next download fetches that file again. Accepted cost: one file.
- The network half is native-only; on wasm `download_external_model` returns
  `ExternalModelError::Unsupported` and the stat probes keep working on whatever `std::fs`
  offers there.
*/

use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::Ordering;
use std::sync::atomic::AtomicBool;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// File name of the completion marker inside a model directory.
pub const COMPLETE_MARKER: &str = ".ms_model_complete.json";
/// Staging directory name inside a model directory (reserved, never a catalog path).
const STAGING_DIR: &str = ".download";
/// Identity of the spec the staged `.part` files belong to.
const IN_PROGRESS_MARKER: &str = "in_progress.json";
/// Retired completion marker of the previous install (stale-file list).
#[cfg(not(target_arch = "wasm32"))]
const PREVIOUS_MARKER: &str = "previous_marker.json";
/// Paths of every discarded foreign in-progress identity (stale-file list): an interrupted
/// download of another pin may already have published some of its files.
#[cfg(not(target_arch = "wasm32"))]
const ABANDONED_FILES: &str = "abandoned_files.json";

/// One pinned file of an external model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFile {
    /// Path inside the repository AND inside the local model directory; `/`-separated,
    /// relative, without `.` / `..` components.
    pub path: &'static str,
    /// Exact size in bytes.
    pub size: u64,
    /// Lowercase hex sha256 of the whole file (64 characters).
    pub sha256: &'static str,
}

/// One pinned Hugging Face model revision and the files the application needs from it.
#[derive(Debug, PartialEq, Eq)]
pub struct ExternalModelSpec {
    /// Stable id (marker identity, busy guard, UI state key).
    pub id: &'static str,
    /// Hugging Face repository id, `owner/name`.
    pub repo_id: &'static str,
    /// Pinned commit: 40 lowercase hex characters, never a branch name.
    pub revision: &'static str,
    /// Target directory relative to the `side_models` root, `/`-separated.
    pub dir: &'static str,
    /// The file allowlist; nothing outside it is downloaded.
    pub files: &'static [ExternalFile],
}

impl ExternalModelSpec {
    /// Sum of all file sizes in bytes (saturating; real specs are far below `u64::MAX`).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().fold(0_u64, |total, file| total.saturating_add(file.size))
    }

    /// Absolute model directory under `side_models_root` (pass `ms_config::side_models_dir()`).
    #[must_use]
    pub fn local_dir(&self, side_models_root: &Path) -> PathBuf {
        join_relative(side_models_root, self.dir)
    }
}

/// Stat-only install state of one spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalModelStatus {
    /// The completion marker matches this spec and every file has its exact size.
    Installed,
    /// A download of THIS spec was started and not finished; `bytes_present` counts
    /// published files of exact size plus staged `.part` prefixes (capped per file).
    Partial { bytes_present: u64 },
    /// Nothing usable for this spec (including an install of a different pin).
    Missing,
}

/// What the downloader is doing with the current file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalDownloadPhase {
    /// Streaming bytes from the network.
    Downloading,
    /// Hashing bytes already on disk (an existing file or a resumed `.part` prefix).
    Verifying,
    /// A transient network failure interrupted the current file; the downloader waits
    /// before retry `attempt` of `max_attempts` (both 1-based counts of retries without
    /// progress). `delay_secs` is the remaining wait, rounded up; re-reported whenever it
    /// changes, and `0` right before the request is sent again. `file_done` is the staged
    /// length the retry resumes from.
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64 },
}

/// One progress report; emitted after every read of at most 128 KiB (in practice per socket
/// read, a few KiB, or per 128 KiB block when hashing a file on disk), and once per second
/// of a retry wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalDownloadProgress {
    /// Zero-based index of the current file in `spec.files`.
    pub file_index: usize,
    /// `spec.files.len()`.
    pub file_count: usize,
    /// Catalog path of the current file.
    pub file_path: &'static str,
    /// Current activity.
    pub phase: ExternalDownloadPhase,
    /// Bytes of the current file handled in this phase so far.
    pub file_done: u64,
    /// Exact size of the current file.
    pub file_total: u64,
    /// Bytes of all earlier files plus `file_done`.
    pub total_done: u64,
    /// `spec.total_bytes()`.
    pub total_bytes: u64,
}

/// Typed failure of the external-model probes and downloader.
///
/// `Display` is stable English diagnostic text for logs; user-facing text is chosen by the
/// UI layer from the variant.
#[derive(Debug, thiserror::Error)]
pub enum ExternalModelError {
    /// The model is not (completely) installed; nothing was downloaded.
    #[error("external model is not downloaded: {}", dir.display())]
    NotDownloaded { dir: PathBuf },
    /// Another download of the same spec is running in this process.
    #[error("a download of external model '{id}' is already running")]
    Busy { id: &'static str },
    /// The caller's cancel flag was set; staged `.part` files are kept for resume.
    #[error("external model download was cancelled")]
    Cancelled,
    /// A filesystem operation failed on `path`.
    #[error("file operation failed on {}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    /// The HTTP request or the response stream failed permanently, or transient failures
    /// exhausted the retries; the `.part` is kept for a later resume. `file` is the catalog
    /// path, or the spec id when the failure precedes any file (client construction).
    #[error("download of '{file}' failed: {detail}")]
    Http { file: &'static str, detail: String },
    /// The server sent more bytes than the pinned size (`actual` is the length the staged
    /// file would have reached); its `.part` was deleted. A body that ends EARLY is not
    /// this error: it is a transient failure, retried and resumed.
    #[error("size mismatch for '{file}': expected {expected} bytes, got {actual}")]
    SizeMismatch { file: &'static str, expected: u64, actual: u64 },
    /// The downloaded file did not have the pinned sha256; its `.part` was deleted.
    #[error("sha256 mismatch for '{file}': expected {expected}, got {actual}")]
    HashMismatch { file: &'static str, expected: &'static str, actual: String },
    /// The spec (or a marker read from disk) violates the catalog rules.
    #[error("invalid external model spec: {0}")]
    InvalidSpec(String),
    /// Downloading is not available on this build target (web).
    #[error("external model download is not supported on this platform")]
    Unsupported,
}

/// Returns the stat-only install state of `spec` under `side_models_root`.
///
/// Never hashes and never touches the network, so it is cheap enough for a worker to run on
/// every engine/variant change; still filesystem I/O, so never on the GUI thread.
#[must_use]
pub fn external_model_status(side_models_root: &Path, spec: &ExternalModelSpec) -> ExternalModelStatus {
    let dir = spec.local_dir(side_models_root);
    let identity = Marker::for_spec(spec);
    let marker_matches = read_json::<Marker>(&dir.join(COMPLETE_MARKER)).as_ref() == Some(&identity);
    let staging = dir.join(STAGING_DIR);
    let mut all_exact = true;
    let mut bytes_present = 0_u64;
    for file in spec.files {
        if file_len(&join_relative(&dir, file.path)) == Some(file.size) {
            bytes_present = bytes_present.saturating_add(file.size);
            continue;
        }
        all_exact = false;
        let staged = file_len(&part_path(&staging, file)).map_or(0, |len| len.min(file.size));
        bytes_present = bytes_present.saturating_add(staged);
    }
    if marker_matches && all_exact {
        return ExternalModelStatus::Installed;
    }
    // Files left by a different pin are not progress towards this one: only a staging
    // directory that belongs to this exact spec makes the state Partial.
    let in_progress_matches = read_json::<Marker>(&staging.join(IN_PROGRESS_MARKER)).as_ref() == Some(&identity);
    if in_progress_matches && bytes_present > 0 {
        ExternalModelStatus::Partial { bytes_present }
    } else {
        ExternalModelStatus::Missing
    }
}

/// Returns the model directory iff `spec` is [`ExternalModelStatus::Installed`]. Never
/// downloads.
///
/// # Errors
/// `ExternalModelError::NotDownloaded { dir }` for any other state.
pub fn installed_dir(side_models_root: &Path, spec: &ExternalModelSpec) -> Result<PathBuf, ExternalModelError> {
    let dir = spec.local_dir(side_models_root);
    match external_model_status(side_models_root, spec) {
        ExternalModelStatus::Installed => Ok(dir),
        ExternalModelStatus::Partial { .. } | ExternalModelStatus::Missing => Err(ExternalModelError::NotDownloaded { dir }),
    }
}

/// Downloads (or resumes, or re-verifies) `spec` into `<side_models_root>/<spec.dir>` and
/// returns that directory once the completion marker is written.
///
/// Blocking: worker threads only. `cancel` is polled (also every 100 ms of a retry wait) and
/// `progress` is called after every read of at most 128 KiB (in practice per socket read).
/// Transient network failures are retried per [`RetryPolicy::standard`]. Uses the cached
/// Hugging Face token (`hf_token::hf_token()`) when non-empty.
///
/// # Errors
/// `InvalidSpec`, `Busy` (same spec already downloading in this process), `Cancelled`,
/// `Io`, `Http` (permanent, or retries exhausted), `SizeMismatch`, `HashMismatch`; on wasm
/// always `Unsupported`.
#[cfg(not(target_arch = "wasm32"))]
pub fn download_external_model(
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    use hf_hub::api::sync::ApiBuilder;
    use hf_hub::{Repo, RepoType};

    validate_spec(spec)?;
    let api = ApiBuilder::from_env().with_progress(false).build().map_err(|err| {
        let error = ExternalModelError::Http { file: spec.id, detail: format!("could not build the Hugging Face client: {err}") };
        log_failure(spec, &error);
        error
    })?;
    let repo = api.repo(Repo::with_revision(spec.repo_id.to_owned(), RepoType::Model, spec.revision.to_owned()));
    let base_url = |file: &ExternalFile| repo.url(file.path);
    let token = crate::hf_token::hf_token();
    let fetcher = ureq_fetcher::UreqFetcher::new((!token.is_empty()).then_some(token));
    download_with(&fetcher, &base_url, side_models_root, spec, cancel, &RetryPolicy::standard(), progress)
}

/// Web build: there is no blocking HTTP stack, so downloading is rejected explicitly.
///
/// # Errors
/// Always `ExternalModelError::Unsupported`.
#[cfg(target_arch = "wasm32")]
pub fn download_external_model(
    _side_models_root: &Path,
    _spec: &'static ExternalModelSpec,
    _cancel: &AtomicBool,
    _progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    Err(ExternalModelError::Unsupported)
}

/// Checks the catalog rules: non-empty id and files, `owner/name` repo id, 40-hex lowercase
/// revision, 64-hex lowercase sha256, relative `/`-separated paths without `.`/`..`/`\`/`:`
/// components, unique file paths, and no file inside the reserved staging directory or
/// named like the marker.
///
/// # Errors
/// `ExternalModelError::InvalidSpec` naming the first violation.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn validate_spec(spec: &ExternalModelSpec) -> Result<(), ExternalModelError> {
    let invalid = |what: String| Err(ExternalModelError::InvalidSpec(format!("{}: {what}", spec.id)));
    if spec.id.trim().is_empty() {
        return invalid("empty id".to_owned());
    }
    let repo_parts: Vec<&str> = spec.repo_id.split('/').collect();
    if repo_parts.len() != 2 || repo_parts.iter().any(|part| !is_safe_component(part)) {
        return invalid(format!("repository id '{}' is not owner/name", spec.repo_id));
    }
    if !is_lower_hex(spec.revision, 40) {
        return invalid(format!("revision '{}' is not a 40-character commit hash", spec.revision));
    }
    if !is_safe_relative_path(spec.dir) {
        return invalid(format!("directory '{}' is not a safe relative path", spec.dir));
    }
    if spec.files.is_empty() {
        return invalid("empty file list".to_owned());
    }
    for (index, file) in spec.files.iter().enumerate() {
        if !is_safe_relative_path(file.path) {
            return invalid(format!("file path '{}' is not a safe relative path", file.path));
        }
        let first = file.path.split('/').next().unwrap_or_default();
        if first == STAGING_DIR || file.path == COMPLETE_MARKER {
            return invalid(format!("file path '{}' collides with a reserved name", file.path));
        }
        if !is_lower_hex(file.sha256, 64) {
            return invalid(format!("sha256 of '{}' is not 64 lowercase hex characters", file.path));
        }
        if spec.files[..index].iter().any(|earlier| earlier.path == file.path) {
            return invalid(format!("file path '{}' is listed twice", file.path));
        }
    }
    Ok(())
}

/// Response status the downloader distinguishes: a whole body or the requested suffix.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// HTTP 200: the body is the whole file, whatever offset was asked for.
    Full,
    /// HTTP 206: the body starts at the requested offset.
    Partial,
}

/// An opened HTTP response body.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct FetchResponse {
    /// How `body` relates to the requested offset.
    pub status: FetchStatus,
    /// The streamed body.
    pub body: Box<dyn std::io::Read + Send>,
}

/// Why a request could not be opened; decides whether the downloader retries it. `detail`
/// is English diagnostic text without secrets (it reaches the log only).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    /// Worth retrying: connect / DNS / I/O transport failures, HTTP 408, 429 and 5xx.
    /// `retry_after` is the server's `Retry-After` when it sent one in delta-seconds form.
    Transient { detail: String, retry_after: Option<Duration> },
    /// Retrying cannot help: any other HTTP status, a malformed URL or proxy setting, a
    /// redirect loop, a 206 for another range.
    Permanent { detail: String },
}

/// Classifies an HTTP error status: 408 (request timeout), 429 (rate limit) and every 5xx
/// are transient, everything else permanent. `retry_after` is the raw `Retry-After` header;
/// only the delta-seconds form is honoured (an HTTP-date falls back to the backoff).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn classify_status(code: u16, retry_after: Option<&str>) -> FetchError {
    let detail = format!("HTTP status {code}");
    if code == 408 || code == 429 || (500..=599).contains(&code) {
        let retry_after = retry_after.and_then(|value| value.trim().parse::<u64>().ok()).map(Duration::from_secs);
        FetchError::Transient { detail, retry_after }
    } else {
        FetchError::Permanent { detail }
    }
}

/// The HTTP seam of the downloader; tests inject an in-memory implementation.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) trait RangeFetcher {
    /// Opens `url`, asking for the bytes from `offset` on when `offset > 0`. A `Partial`
    /// answer must start exactly at `offset` (the implementation checks it). Errors while
    /// READING the returned body are always treated as transient by the downloader.
    ///
    /// # Errors
    /// A classified [`FetchError`] for any transport or status failure.
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError>;
}

/// Retry schedule for transient failures, with the backoff sleep injected so tests never
/// wait real time.
///
/// Retry `n` (1-based count of consecutive failed attempts that staged no new bytes) waits
/// `first_delay * 2^(n-1)`, capped at `max_delay`, or the server's `Retry-After` when that is
/// longer (itself capped at `retry_after_cap`). After `max_retries` such retries the last
/// failure is returned as `ExternalModelError::Http`.
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
impl RetryPolicy<'_> {
    /// Wait before retry `attempt` (1-based); see the type comment for the formula.
    pub(crate) fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        // 2^(attempt-1) saturates at u32::MAX for absurd attempt counts; the cap applies anyway.
        let factor = 1_u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        let backoff = self.first_delay.saturating_mul(factor).min(self.max_delay);
        retry_after.map_or(backoff, |requested| requested.min(self.retry_after_cap).max(backoff))
    }
}

/// Testable download core; see [`download_external_model`] for the contract. `base_url`
/// maps a catalog file to its resolve URL; `retry` schedules transient-failure retries.
///
/// # Errors
/// As [`download_external_model`] (never `Unsupported`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn download_with(
    fetcher: &dyn RangeFetcher,
    base_url: &dyn Fn(&ExternalFile) -> String,
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    retry: &RetryPolicy<'_>,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    validate_spec(spec)?;
    let _active = active_guard::ActiveDownload::acquire(spec.id)?; // RAII: held until return
    let result = run_download(fetcher, base_url, side_models_root, spec, cancel, retry, progress);
    match &result {
        Ok(dir) => ms_log::runtime_log::log_info(format!(
            "[external-models] download complete: id={} repo={} rev={} bytes={} dir={}",
            spec.id,
            spec.repo_id,
            spec.revision,
            spec.total_bytes(),
            dir.display()
        )),
        Err(ExternalModelError::Cancelled) => ms_log::runtime_log::log_info(format!(
            "[external-models] download cancelled: id={} repo={} rev={} (staged parts kept for resume)",
            spec.id, spec.repo_id, spec.revision
        )),
        Err(error) => log_failure(spec, error),
    }
    result
}

/// Body of [`download_with`] under the busy guard; logging stays with the caller.
#[cfg(not(target_arch = "wasm32"))]
fn run_download(
    fetcher: &dyn RangeFetcher,
    base_url: &dyn Fn(&ExternalFile) -> String,
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    retry: &RetryPolicy<'_>,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    let dir = spec.local_dir(side_models_root);
    let staging = dir.join(STAGING_DIR);
    ms_log::runtime_log::log_info(format!(
        "[external-models] download start: id={} repo={} rev={} files={} bytes={} dir={}",
        spec.id,
        spec.repo_id,
        spec.revision,
        spec.files.len(),
        spec.total_bytes(),
        dir.display()
    ));
    create_dir_all(&staging)?;
    retire_complete_marker(&dir, &staging)?;
    prepare_staging(&staging, spec)?;

    let mut job = FileJob { fetcher, cancel, retry, progress, spec, dir: &dir, staging: &staging, done_before: 0 };
    for (index, file) in spec.files.iter().enumerate() {
        job.fetch_file(index, file, &base_url(file))?;
        job.done_before = job.done_before.saturating_add(file.size);
    }

    remove_stale_files(&dir, &staging, spec)?;
    write_json_atomically(&dir.join(COMPLETE_MARKER), &staging.join("complete.json.tmp"), &Marker::for_spec(spec))?;
    // The marker is written, so the model IS installed: a leftover staging directory (e.g. a
    // Windows sharing violation) is not a failed download. The next download into this
    // directory reuses the leftover staging and removes it when it completes.
    if let Err(error) = std::fs::remove_dir_all(&staging) {
        ms_log::runtime_log::log_warn(format!(
            "[external-models] installed, but the staging directory could not be removed: id={} path={} error={error}",
            spec.id,
            staging.display()
        ));
    }
    Ok(dir)
}

/// Per-download context for fetching the files one by one.
#[cfg(not(target_arch = "wasm32"))]
struct FileJob<'a> {
    fetcher: &'a dyn RangeFetcher,
    cancel: &'a AtomicBool,
    retry: &'a RetryPolicy<'a>,
    progress: &'a mut dyn FnMut(&ExternalDownloadProgress),
    spec: &'static ExternalModelSpec,
    dir: &'a Path,
    staging: &'a Path,
    /// Sum of the sizes of the files already handled.
    done_before: u64,
}

/// Outcome of one failed request attempt for a file.
#[cfg(not(target_arch = "wasm32"))]
enum AttemptError {
    /// Returned to the caller as is (cancel, local I/O, permanent HTTP failure, overflow).
    Fatal(ExternalModelError),
    /// The staged prefix is valid and the failure may pass: retry after a backoff.
    /// `retry_after` is the server's requested wait, if any.
    Transient { detail: String, retry_after: Option<Duration> },
}

#[cfg(not(target_arch = "wasm32"))]
impl From<ExternalModelError> for AttemptError {
    fn from(error: ExternalModelError) -> Self {
        Self::Fatal(error)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl FileJob<'_> {
    /// Read buffer size: the upper bound of one read, hence the coarsest cancel and progress
    /// granularity (a network read usually returns far less).
    const CHUNK: usize = 128 * 1024;

    /// Makes `file` present and verified at its final path: keeps an already-correct file,
    /// otherwise resumes or restarts its `.part` (retrying transient failures), verifies
    /// size then sha256, fsyncs and renames it into place.
    fn fetch_file(&mut self, index: usize, file: &'static ExternalFile, url: &str) -> Result<(), ExternalModelError> {
        use sha2::{Digest, Sha256};

        let final_path = join_relative(self.dir, file.path);
        let part = part_path(self.staging, file);
        if file_len(&final_path) == Some(file.size) {
            let mut hasher = Sha256::new();
            self.hash_existing(index, file, &final_path, &mut hasher)?;
            if hex_lower(&hasher.finalize()) == file.sha256 {
                remove_file_if_exists(&part)?;
                return Ok(());
            }
            ms_log::runtime_log::log_info(format!(
                "[external-models] existing file differs from the pin, re-downloading: id={} file={}",
                self.spec.id, file.path
            ));
        }

        if let Some(parent) = part.parent() {
            create_dir_all(parent)?;
        }
        let mut written = file_len(&part).unwrap_or(0);
        if written > file.size {
            remove_file_if_exists(&part)?;
            written = 0;
        }
        let mut hasher = Sha256::new();
        if written > 0 {
            self.hash_existing(index, file, &part, &mut hasher)?;
        }
        if written < file.size {
            // Returns only once exactly `file.size` bytes are staged and fsynced.
            self.stream_with_retries(index, file, url, &part, written, &mut hasher)?;
        } else {
            // A complete `.part` left by an interrupted earlier run: make it durable before
            // it is published.
            let handle = std::fs::OpenOptions::new().write(true).open(&part).map_err(|source| io_error(&part, source))?;
            handle.sync_all().map_err(|source| io_error(&part, source))?;
        }

        let actual = hex_lower(&hasher.finalize());
        if actual != file.sha256 {
            remove_file_if_exists(&part)?;
            return Err(ExternalModelError::HashMismatch { file: file.path, expected: file.sha256, actual });
        }
        if let Some(parent) = final_path.parent() {
            create_dir_all(parent)?;
        }
        std::fs::rename(&part, &final_path).map_err(|source| io_error(&final_path, source))?;
        Ok(())
    }

    /// Streams the rest of `file` into `part` from `offset` (bytes already staged and already
    /// fed to `hasher`) until exactly `file.size` bytes are staged, then fsyncs the `.part`.
    ///
    /// A transient failure (see [`AttemptError::Transient`]) keeps the staged bytes and is
    /// retried per `self.retry`, resuming from the staged length; the retry counter resets
    /// whenever an attempt staged new bytes, so isolated drops over a long download never
    /// exhaust it.
    ///
    /// # Errors
    /// `Cancelled` (also during a retry wait), `Io`, `SizeMismatch` (overflow, `.part`
    /// deleted), `Http` for a permanent failure or once the retries are exhausted (`.part`
    /// kept).
    fn stream_with_retries(
        &mut self,
        index: usize,
        file: &'static ExternalFile,
        url: &str,
        part: &Path,
        offset: u64,
        hasher: &mut sha2::Sha256,
    ) -> Result<(), ExternalModelError> {
        let mut written = offset;
        let mut failures = 0_u32;
        loop {
            let before = written;
            let (detail, retry_after) = match self.stream_attempt(index, file, url, part, &mut written, hasher) {
                Ok(()) if written == file.size => return Ok(()),
                // The body ended cleanly but early (chunked or close-delimited response
                // cut off): the staged prefix is valid, so resume it.
                Ok(()) => (format!("the response body ended at byte {written} of {}", file.size), None),
                Err(AttemptError::Fatal(error)) => return Err(error),
                Err(AttemptError::Transient { detail, retry_after }) => (detail, retry_after),
            };
            if written > before {
                failures = 0;
            }
            failures = failures.saturating_add(1);
            if failures > self.retry.max_retries {
                return Err(ExternalModelError::Http {
                    file: file.path,
                    detail: format!("{detail} (gave up after {} retries without progress)", self.retry.max_retries),
                });
            }
            let delay = self.retry.delay_for(failures, retry_after);
            ms_log::runtime_log::log_warn(format!(
                "[external-models] transient download failure, retrying: id={} file={} offset={written} attempt={failures}/{} delay_ms={} error={detail}",
                self.spec.id,
                file.path,
                self.retry.max_retries,
                delay.as_millis()
            ));
            self.wait_before_retry(index, file, written, failures, delay)?;
        }
    }

    /// One request for the rest of `file`, appending to `part` and advancing `*written`
    /// (and `hasher`) per chunk, so the staged length stays correct when the attempt fails.
    /// A 200 answer to a range request restarts the file from zero. `Ok` means the body
    /// ended cleanly; the caller compares `*written` with the pinned size.
    fn stream_attempt(
        &mut self,
        index: usize,
        file: &'static ExternalFile,
        url: &str,
        part: &Path,
        written: &mut u64,
        hasher: &mut sha2::Sha256,
    ) -> Result<(), AttemptError> {
        use sha2::Digest;
        use std::io::{Read, Write};

        self.check_cancel()?;
        let offset = *written;
        let response = self.fetcher.fetch(url, offset).map_err(|error| match error {
            FetchError::Transient { detail, retry_after } => AttemptError::Transient { detail, retry_after },
            FetchError::Permanent { detail } => AttemptError::Fatal(ExternalModelError::Http { file: file.path, detail }),
        })?;
        let mut handle = match response.status {
            FetchStatus::Partial => {
                // Offset 0 means no `.part` exists yet; otherwise the staged prefix is extended.
                let handle = if offset == 0 { std::fs::File::create(part) } else { std::fs::OpenOptions::new().append(true).open(part) };
                handle.map_err(|source| io_error(part, source))?
            }
            FetchStatus::Full => {
                if offset > 0 {
                    ms_log::runtime_log::log_info(format!(
                        "[external-models] server ignored the range request, restarting: id={} file={} offset={offset}",
                        self.spec.id, file.path
                    ));
                }
                let handle = std::fs::File::create(part).map_err(|source| io_error(part, source))?;
                *hasher = sha2::Sha256::new();
                *written = 0;
                handle
            }
        };
        let mut body = response.body;
        let mut buffer = vec![0_u8; Self::CHUNK];
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
            let next = written.saturating_add(chunk_len(read));
            if next > file.size {
                drop(handle);
                remove_file_if_exists(part)?;
                return Err(AttemptError::Fatal(ExternalModelError::SizeMismatch { file: file.path, expected: file.size, actual: next }));
            }
            handle.write_all(chunk).map_err(|source| io_error(part, source))?;
            hasher.update(chunk);
            *written = next;
            self.report(index, file, ExternalDownloadPhase::Downloading, next);
        }
        if *written == file.size {
            handle.sync_all().map_err(|source| io_error(part, source))?;
        }
        Ok(())
    }

    /// Waits `delay` before retry `attempt`, sleeping in `wait_slice` steps and polling the
    /// cancel flag between them. Reports `Retrying` whenever the remaining whole seconds
    /// change, last with `delay_secs: 0` right before returning.
    ///
    /// # Errors
    /// `Cancelled` as soon as the flag is seen.
    fn wait_before_retry(&mut self, index: usize, file: &'static ExternalFile, staged: u64, attempt: u32, delay: Duration) -> Result<(), ExternalModelError> {
        let mut remaining = delay;
        let mut shown = None;
        loop {
            self.check_cancel()?;
            // Whole seconds, rounded up, so "in 1 s" is shown until the wait is really over.
            let delay_secs = remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0));
            if shown != Some(delay_secs) {
                shown = Some(delay_secs);
                let phase = ExternalDownloadPhase::Retrying { attempt, max_attempts: self.retry.max_retries, delay_secs };
                self.report(index, file, phase, staged);
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

    /// Feeds the whole of `path` into `hasher`, reporting `Verifying` progress and honouring
    /// cancel.
    fn hash_existing(
        &mut self,
        index: usize,
        file: &'static ExternalFile,
        path: &Path,
        hasher: &mut sha2::Sha256,
    ) -> Result<(), ExternalModelError> {
        use sha2::Digest;
        use std::io::Read;

        let mut handle = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
        let mut buffer = vec![0_u8; Self::CHUNK];
        let mut hashed = 0_u64;
        self.report(index, file, ExternalDownloadPhase::Verifying, 0);
        loop {
            self.check_cancel()?;
            let read = handle.read(&mut buffer).map_err(|source| io_error(path, source))?;
            if read == 0 {
                return Ok(());
            }
            hasher.update(&buffer[..read]);
            hashed = hashed.saturating_add(chunk_len(read));
            self.report(index, file, ExternalDownloadPhase::Verifying, hashed);
        }
    }

    fn check_cancel(&self) -> Result<(), ExternalModelError> {
        if self.cancel.load(Ordering::Relaxed) { Err(ExternalModelError::Cancelled) } else { Ok(()) }
    }

    fn report(&mut self, index: usize, file: &'static ExternalFile, phase: ExternalDownloadPhase, file_done: u64) {
        (self.progress)(&ExternalDownloadProgress {
            file_index: index,
            file_count: self.spec.files.len(),
            file_path: file.path,
            phase,
            file_done,
            file_total: file.size,
            total_done: self.done_before.saturating_add(file_done),
            total_bytes: self.spec.total_bytes(),
        });
    }
}

/// Moves an existing completion marker into staging, so no reader sees "installed" while
/// files are being replaced, while its file list survives a cancel or crash for the
/// stale-file cleanup at the end.
#[cfg(not(target_arch = "wasm32"))]
fn retire_complete_marker(dir: &Path, staging: &Path) -> Result<(), ExternalModelError> {
    let marker = dir.join(COMPLETE_MARKER);
    match std::fs::rename(&marker, staging.join(PREVIOUS_MARKER)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(&marker, source)),
    }
}

/// Discards staged parts that belong to a different spec identity (a changed pin would
/// otherwise resume a part of the wrong revision), then records this spec as in progress.
///
/// The discarded identity's file paths are merged into `abandoned_files.json` first: its
/// download may already have PUBLISHED some files, which only the stale-file cleanup can
/// remove. A malformed foreign `in_progress.json` has no readable file list; its parts are
/// discarded all the same.
#[cfg(not(target_arch = "wasm32"))]
fn prepare_staging(staging: &Path, spec: &ExternalModelSpec) -> Result<(), ExternalModelError> {
    let identity = Marker::for_spec(spec);
    let in_progress = staging.join(IN_PROGRESS_MARKER);
    let staged = if in_progress.exists() { Some(read_json::<Marker>(&in_progress)) } else { None };
    if let Some(staged) = staged.filter(|staged| staged.as_ref() != Some(&identity)) {
        if let Some(foreign) = staged {
            record_abandoned_files(staging, &foreign)?;
        }
        let entries = std::fs::read_dir(staging).map_err(|source| io_error(staging, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| io_error(staging, source))?;
            if entry.file_name() == PREVIOUS_MARKER || entry.file_name() == ABANDONED_FILES {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            removed.map_err(|source| io_error(&path, source))?;
        }
    }
    write_json_atomically(&in_progress, &staging.join("in_progress.json.tmp"), &identity)
}

/// Merges the file paths of a discarded foreign in-progress identity into
/// `abandoned_files.json`, so several abandoned pins in a row all stay on the stale list.
#[cfg(not(target_arch = "wasm32"))]
fn record_abandoned_files(staging: &Path, foreign: &Marker) -> Result<(), ExternalModelError> {
    let path = staging.join(ABANDONED_FILES);
    let mut abandoned = read_json::<AbandonedFiles>(&path).unwrap_or_default();
    abandoned.paths.extend(foreign.files.iter().map(|file| file.path.clone()));
    abandoned.paths.sort_unstable();
    abandoned.paths.dedup();
    write_json_atomically(&path, &staging.join("abandoned_files.json.tmp"), &abandoned)
}

/// Deletes the files listed by the retired marker or by an abandoned in-progress identity
/// that the current spec does not list. Paths read from disk are re-validated, so a tampered
/// record cannot delete outside `dir`.
#[cfg(not(target_arch = "wasm32"))]
fn remove_stale_files(dir: &Path, staging: &Path, spec: &ExternalModelSpec) -> Result<(), ExternalModelError> {
    let mut candidates = read_json::<AbandonedFiles>(&staging.join(ABANDONED_FILES)).unwrap_or_default().paths;
    if let Some(previous) = read_json::<Marker>(&staging.join(PREVIOUS_MARKER)) {
        candidates.extend(previous.files.into_iter().map(|file| file.path));
    }
    for old in &candidates {
        if spec.files.iter().any(|file| file.path == old.as_str()) {
            continue;
        }
        if !is_safe_relative_path(old) || old.split('/').next() == Some(STAGING_DIR) || old == COMPLETE_MARKER {
            ms_log::runtime_log::log_warn(format!("[external-models] ignoring unsafe stale path in a staging record: id={} path={old:?}", spec.id));
            continue;
        }
        remove_file_if_exists(&join_relative(dir, old))?;
        ms_log::runtime_log::log_info(format!("[external-models] removed stale file: id={} file={old}", spec.id));
    }
    Ok(())
}

/// Serializes `value` to `tmp`, fsyncs it and renames it onto `target`.
#[cfg(not(target_arch = "wasm32"))]
fn write_json_atomically<T: Serialize>(target: &Path, tmp: &Path, value: &T) -> Result<(), ExternalModelError> {
    use std::io::Write;

    let bytes = serde_json::to_vec_pretty(value).map_err(|err| ExternalModelError::InvalidSpec(format!("marker serialization failed: {err}")))?;
    let mut handle = std::fs::File::create(tmp).map_err(|source| io_error(tmp, source))?;
    handle.write_all(&bytes).map_err(|source| io_error(tmp, source))?;
    handle.sync_all().map_err(|source| io_error(tmp, source))?;
    drop(handle);
    std::fs::rename(tmp, target).map_err(|source| io_error(target, source))
}

#[cfg(not(target_arch = "wasm32"))]
fn log_failure(spec: &ExternalModelSpec, error: &ExternalModelError) {
    ms_log::runtime_log::log_error(format!(
        "[external-models] download failed: id={} repo={} rev={} error={error}",
        spec.id, spec.repo_id, spec.revision
    ));
}

#[cfg(not(target_arch = "wasm32"))]
fn create_dir_all(path: &Path) -> Result<(), ExternalModelError> {
    std::fs::create_dir_all(path).map_err(|source| io_error(path, source))
}

#[cfg(not(target_arch = "wasm32"))]
fn remove_file_if_exists(path: &Path) -> Result<(), ExternalModelError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn io_error(path: &Path, source: std::io::Error) -> ExternalModelError {
    ExternalModelError::Io { path: path.to_path_buf(), source }
}

/// A read length as `u64`. `usize` is at most 64 bits on every supported target, so the
/// saturating fallback is unreachable; it only avoids a lossy `as`.
#[cfg(not(target_arch = "wasm32"))]
fn chunk_len(read: usize) -> u64 {
    u64::try_from(read).unwrap_or(u64::MAX)
}

/// Lowercase hex of a digest.
#[cfg(not(target_arch = "wasm32"))]
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Identity of a spec as recorded in the completion and in-progress markers. Any change of
/// pin (revision, file path, size or hash) makes two markers unequal.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    id: String,
    repo_id: String,
    revision: String,
    files: Vec<MarkerFile>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MarkerFile {
    path: String,
    size: u64,
    sha256: String,
}

/// Content of `abandoned_files.json`: catalog paths of discarded in-progress identities,
/// sorted and unique.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Serialize, Deserialize)]
struct AbandonedFiles {
    paths: Vec<String>,
}

impl Marker {
    fn for_spec(spec: &ExternalModelSpec) -> Self {
        Self {
            id: spec.id.to_owned(),
            repo_id: spec.repo_id.to_owned(),
            revision: spec.revision.to_owned(),
            files: spec
                .files
                .iter()
                .map(|file| MarkerFile { path: file.path.to_owned(), size: file.size, sha256: file.sha256.to_owned() })
                .collect(),
        }
    }
}

/// Reads a marker or staging record; `None` when it is absent. An unreadable or malformed file
/// is logged and also reads as `None` ("not installed" / "nothing recorded"), which only ever
/// leads to a re-verify, never to trusting it.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            ms_log::runtime_log::log_warn(format!("[external-models] could not read marker {}: {error}", path.display()));
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(marker) => Some(marker),
        Err(error) => {
            ms_log::runtime_log::log_warn(format!("[external-models] malformed marker {}: {error}", path.display()));
            None
        }
    }
}

/// Length of a regular file, `None` when absent or not a file.
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().filter(std::fs::Metadata::is_file).map(|metadata| metadata.len())
}

/// Staging path of `file`'s partial download: `<staging>/<path>.part`.
fn part_path(staging: &Path, file: &ExternalFile) -> PathBuf {
    join_relative(staging, &format!("{}.part", file.path))
}

/// Joins a `/`-separated relative path component by component (platform separators).
fn join_relative(base: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(base.to_path_buf(), |path, part| path.join(part))
}

#[cfg(not(target_arch = "wasm32"))]
fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(not(target_arch = "wasm32"))]
/// A path component that cannot escape or re-root a join on any supported OS.
fn is_safe_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':', '\0'])
}

#[cfg(not(target_arch = "wasm32"))]
fn is_safe_relative_path(path: &str) -> bool {
    !path.starts_with('/') && path.split('/').all(is_safe_component)
}

/// Process-wide "one download per spec" guard, separate from `ai_models::DOWNLOAD_LOCK`.
#[cfg(not(target_arch = "wasm32"))]
mod active_guard {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, PoisonError};

    use super::ExternalModelError;

    static ACTIVE: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

    /// Marks a spec id as downloading until dropped.
    pub(super) struct ActiveDownload(&'static str);

    impl ActiveDownload {
        /// # Errors
        /// `ExternalModelError::Busy` when the id is already marked.
        pub(super) fn acquire(id: &'static str) -> Result<Self, ExternalModelError> {
            // A poisoned lock still holds a consistent set: insert/remove are single calls
            // that cannot panic halfway, so recovering the guard is sound.
            let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
            if active.insert(id) { Ok(Self(id)) } else { Err(ExternalModelError::Busy { id }) }
        }
    }

    impl Drop for ActiveDownload {
        fn drop(&mut self) {
            ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).remove(self.0);
        }
    }
}

/// The production `RangeFetcher` over a ureq agent.
#[cfg(not(target_arch = "wasm32"))]
mod ureq_fetcher {
    use std::time::Duration;

    use super::{FetchError, FetchResponse, FetchStatus, RangeFetcher, classify_status};

    pub(super) struct UreqFetcher {
        agent: ureq::Agent,
        /// Hugging Face token; sent as a bearer header, never logged. ureq's default
        /// redirect policy drops `Authorization` on every redirect (the LFS CDN is another
        /// host), while keeping `Range`.
        bearer: Option<String>,
    }

    impl UreqFetcher {
        pub(super) fn new(bearer: Option<String>) -> Self {
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
            let mut request = self.agent.get(url);
            if offset > 0 {
                request = request.set("Range", &format!("bytes={offset}-"));
            }
            if let Some(token) = &self.bearer {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            let response = match request.call() {
                Ok(response) => response,
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
            let status = match response.status() {
                200 => FetchStatus::Full,
                206 => {
                    // A 206 for another range would silently corrupt the resumed file.
                    let range = response.header("Content-Range").unwrap_or_default();
                    if !range.starts_with(&format!("bytes {offset}-")) {
                        return Err(FetchError::Permanent { detail: format!("unexpected Content-Range '{range}' for offset {offset}") });
                    }
                    FetchStatus::Partial
                }
                other => return Err(FetchError::Permanent { detail: format!("unexpected HTTP status {other}") }),
            };
            Ok(FetchResponse { status, body: Box::new(response.into_reader()) })
        }
    }

    /// Whether a ureq transport failure may pass on its own: network-level failures (DNS,
    /// connect, I/O including timeouts and resets, a garbled status line or header from a
    /// flaky proxy) are retried; configuration problems and redirect loops are not.
    fn transport_is_transient(kind: ureq::ErrorKind) -> bool {
        use ureq::ErrorKind;

        match kind {
            ErrorKind::Dns
            | ErrorKind::ConnectionFailed
            | ErrorKind::Io
            | ErrorKind::BadStatus
            | ErrorKind::BadHeader
            | ErrorKind::ProxyConnect => true,
            ErrorKind::InvalidUrl
            | ErrorKind::UnknownScheme
            | ErrorKind::InsecureRequestHttpsOnly
            | ErrorKind::TooManyRedirects
            | ErrorKind::InvalidProxyUrl
            | ErrorKind::ProxyUnauthorized
            | ErrorKind::HTTP => false,
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
