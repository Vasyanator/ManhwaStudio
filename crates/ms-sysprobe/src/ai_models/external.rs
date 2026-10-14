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
- `download_with()`: the testable core with an injected `RangeFetcher` and `RetryPolicy`
  (injected backoff sleep). The HTTP seam, the retry schedule and the per-file resumable
  stream live in `resumable.rs` (shared with the main-repository downloader); this file
  adds the pin (size + sha256), the staging layout and the markers.
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
- Resolve URLs come from `resumable::hf_resolve_url` (`HF_ENDPOINT` honoured, no home
  directory needed). The HF token is sent as a bearer header only to the resolve URL; ureq
  drops it on a cross-host redirect. It is never logged.
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
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use super::resumable::{
    CHUNK, PartRequest, RangeFetcher, ResumableFetch, RetryPolicy, StreamError, StreamEvent, UreqFetcher, chunk_len, hf_resolve_url,
};
// Test-only names of the shared HTTP seam, so `external/tests.rs` reaches them via `super::*`.
#[cfg(all(test, not(target_arch = "wasm32")))]
use super::resumable::{FetchError, FetchResponse, FetchStatus, classify_status};

/// Log prefix of every line this module writes.
#[cfg(not(target_arch = "wasm32"))]
const LOG_TAG: &str = "[external-models]";

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
    validate_spec(spec)?;
    let base_url = |file: &ExternalFile| hf_resolve_url(spec.repo_id, spec.revision, file.path);
    let token = crate::hf_token::hf_token();
    let fetcher = UreqFetcher::new((!token.is_empty()).then_some(token));
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

#[cfg(not(target_arch = "wasm32"))]
impl FileJob<'_> {
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
        // Returns only once exactly `file.size` bytes are staged and fsynced (a complete
        // `.part` left by an interrupted earlier run is only fsynced).
        self.stream_part(index, file, url, &part, written, &mut hasher)?;

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
    /// fed to `hasher`) through the shared resumable fetch, with the pinned size as the
    /// authoritative length, mapping its events onto [`ExternalDownloadProgress`].
    ///
    /// # Errors
    /// `Cancelled` (also during a retry wait), `Io`, `SizeMismatch` (overflow, `.part`
    /// deleted), `Http` for a permanent failure or once the retries are exhausted (`.part`
    /// kept).
    fn stream_part(
        &mut self,
        index: usize,
        file: &'static ExternalFile,
        url: &str,
        part: &Path,
        offset: u64,
        hasher: &mut sha2::Sha256,
    ) -> Result<(), ExternalModelError> {
        let log_context = format!("id={} file={}", self.spec.id, file.path);
        let engine = ResumableFetch { fetcher: self.fetcher, retry: self.retry, cancel: self.cancel, log_tag: LOG_TAG, log_context: &log_context };
        let request = PartRequest { url, part, offset, expected_size: Some(file.size) };
        let result = engine.fetch_part(&request, Some(hasher), &mut |event| match event {
            StreamEvent::Downloading { staged } => self.report(index, file, ExternalDownloadPhase::Downloading, staged),
            StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged } => {
                self.report(index, file, ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs }, staged);
            }
        });
        match result {
            Ok(_) => Ok(()),
            Err(StreamError::Cancelled) => Err(ExternalModelError::Cancelled),
            Err(StreamError::Io { path, source }) => Err(ExternalModelError::Io { path, source }),
            Err(StreamError::Http { detail }) => Err(ExternalModelError::Http { file: file.path, detail }),
            Err(StreamError::SizeMismatch { expected, actual }) => Err(ExternalModelError::SizeMismatch { file: file.path, expected, actual }),
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
        let mut buffer = vec![0_u8; CHUNK];
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
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) { Err(ExternalModelError::Cancelled) } else { Ok(()) }
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
