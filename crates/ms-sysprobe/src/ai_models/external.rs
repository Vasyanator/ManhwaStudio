/*
File: crates/ms-sysprobe/src/ai_models/external.rs

Purpose:
Pinned, verified downloader for EXTERNAL (third-party) models, plus the stat-only status
probe that tells a caller whether such a model is installed. The specs themselves live in
`external_catalog.rs` (EasyOCR's language rule in `easyocr.rs`); this file owns only the
mechanics.

Key items:
- `ExternalModelSpec`: one pinned model — its `ExternalSource` (a Hugging Face repository
  at a commit, or a plain HTTPS base URL), the target directory relative to `side_models`,
  its `DirLayout` and the file allowlist.
- `ExternalFile` + `FileOrigin`: one pinned file (exact size + lowercase sha256), fetched
  directly or extracted from a pinned zip archive (`ZipMember`).
- `external_model_status()` / `installed_dir()`: stat-only probes (no hashing, no network).
- `verify_external_model()` + `VerifyReport`: the repair path — hash every file against its
  pin, delete mismatching ones and the markers of every spec listing them.
- `download_external_model()`: blocking download with byte progress, cancel, resume,
  automatic retry of transient network failures, size + sha256 verification, atomic
  per-file publish and a completion marker.
- `download_with()`: the testable core with an injected `RangeFetcher` and `RetryPolicy`
  (injected backoff sleep). One file is fetched and published by
  `ms_download::fetch_verified`, an archive member extracted by
  `ms_download::unpack::zip_member_atomic`; this file adds the pins, the staging layout and
  the markers.
- `ExternalModelError`: typed errors; `Display` is English diagnostic text for logs, the UI
  maps variants to localized messages.

On-disk layout of one model directory (`<side_models>/<spec.dir>`):
- the published files at their catalog paths;
- the completion marker, written last, so "installed" means "this exact spec was fully
  downloaded and verified": `.ms_model_complete.json` for an `Exclusive` directory (one
  spec owns it), `.ms_model_complete.<id>.json` for a `Shared` one (several specs publish
  into it, e.g. every EasyOCR script next to the one shared detector);
- staging while a download is unfinished — `.download/` (Exclusive) or `.download/<id>/`
  (Shared): `<remote path>.part` files (a zip-member file stages its archive), the verified
  archives themselves until their member is extracted, `in_progress.json` (identity of the
  spec the parts belong to), `previous_marker.json` (the retired marker of an older
  install) and `abandoned_files.json` (file paths of discarded in-progress identities of
  other pins); the last two feed the stale-file cleanup of an Exclusive directory.

Notes:
- Nothing here runs on the GUI thread: every function touches the filesystem, the
  download also the network. Callers own the worker.
- No implicit download anywhere: `installed_dir` only reports.
- Hugging Face URLs come from `hf_url::hf_resolve_url` (`HF_ENDPOINT` honoured, no home
  directory needed); the HF token is sent as a bearer header to the first hop of a Hugging
  Face request only and is never logged. `Url`-source requests carry no token at all.
- A Shared directory never deletes stale files: a file there may belong to another spec.
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
use super::hf_url::{hf_resolve_url, url_source_file_url};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::busy::BusyGuard;
#[cfg(not(target_arch = "wasm32"))]
use ms_download::resumable::{CHUNK, RangeFetcher, ResumableFetch, RetryPolicy, StreamEvent, UreqFetcher, chunk_len};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::unpack::zip_member_atomic;
#[cfg(not(target_arch = "wasm32"))]
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, fetch_verified};
// Test-only names of the shared HTTP seam, so `external/tests.rs` reaches them via `super::*`.
#[cfg(all(test, not(target_arch = "wasm32")))]
use ms_download::resumable::{FetchError, FetchResponse, FetchStatus, classify_status};

/// Log prefix of every line this module writes.
#[cfg(not(target_arch = "wasm32"))]
const LOG_TAG: &str = "[external-models]";

/// File name of the completion marker inside an `Exclusive` model directory.
pub const COMPLETE_MARKER: &str = ".ms_model_complete.json";
/// Name prefix every completion marker shares (`Shared` ones are `<prefix>.<id>.json`);
/// reserved, never a catalog path.
const COMPLETE_MARKER_PREFIX: &str = ".ms_model_complete";
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

/// Where an external model's files are downloaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    /// A Hugging Face model repository at a pinned commit; a file is fetched from its resolve
    /// URL (with the user's token, if any).
    HuggingFace {
        /// Repository id, `owner/name`.
        repo_id: &'static str,
        /// Pinned commit: 40 lowercase hex characters, never a branch name.
        revision: &'static str,
    },
    /// A plain HTTPS location (a GitHub release, a model host); a file is fetched from
    /// `<base_url>/<remote path>` without any token. The pins are what make it immutable.
    Url {
        /// `https://host[/path]`, without a trailing slash, query or fragment.
        base_url: &'static str,
    },
}

/// Who else may publish files into a spec's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirLayout {
    /// The spec owns its directory: one completion marker `.ms_model_complete.json`, staging
    /// in `.download/`, and files of an older pin are deleted once the new one is installed.
    Exclusive,
    /// Several specs publish into the same directory (a file may be shared, e.g. one
    /// detector next to many recognizers): per-spec marker `.ms_model_complete.<id>.json`,
    /// per-spec staging `.download/<id>/`, and no stale-file deletion.
    Shared,
}

/// How a pinned file reaches its catalog path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOrigin {
    /// Fetched as is from its own remote path (= its catalog path).
    Direct,
    /// Extracted from a pinned zip archive that is fetched from `archive` (a remote path
    /// relative to the source), verified, and deleted once the member is published.
    ZipMember {
        /// Remote path of the archive, `/`-separated (e.g. `v1.3/english_g2.zip`).
        archive: &'static str,
        /// Exact archive size in bytes.
        archive_size: u64,
        /// Lowercase hex sha256 of the archive.
        archive_sha256: &'static str,
        /// Exact name of the member inside the archive.
        member: &'static str,
    },
}

/// One pinned file of an external model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFile {
    /// Path inside the local model directory (and, for a `Direct` file, inside the source);
    /// `/`-separated, relative, without `.` / `..` components.
    pub path: &'static str,
    /// Exact size in bytes of the published file.
    pub size: u64,
    /// Lowercase hex sha256 of the whole published file (64 characters).
    pub sha256: &'static str,
    /// Direct download or zip member.
    pub origin: FileOrigin,
}

impl ExternalFile {
    /// A file fetched directly from `path` in the source.
    #[must_use]
    pub const fn direct(path: &'static str, size: u64, sha256: &'static str) -> Self {
        Self { path, size, sha256, origin: FileOrigin::Direct }
    }

    /// The remote path that is fetched for this file: its own path, or its archive's.
    #[must_use]
    pub fn remote_path(&self) -> &'static str {
        match self.origin {
            FileOrigin::Direct => self.path,
            FileOrigin::ZipMember { archive, .. } => archive,
        }
    }

    /// Bytes transferred over the network for this file: its size, or its archive's.
    #[must_use]
    pub fn download_size(&self) -> u64 {
        match self.origin {
            FileOrigin::Direct => self.size,
            FileOrigin::ZipMember { archive_size, .. } => archive_size,
        }
    }
}

/// One pinned external model and the files the application needs from it.
#[derive(Debug, PartialEq, Eq)]
pub struct ExternalModelSpec {
    /// Stable id (marker identity, busy guard, UI state key; the marker name of a `Shared`
    /// directory, so `[a-z0-9_]` only there).
    pub id: &'static str,
    /// Where the files come from.
    pub source: ExternalSource,
    /// Target directory relative to the `side_models` root, `/`-separated.
    pub dir: &'static str,
    /// Whether other specs publish into `dir` too.
    pub layout: DirLayout,
    /// The file allowlist; nothing outside it is downloaded.
    pub files: &'static [ExternalFile],
}

impl ExternalModelSpec {
    /// Bytes a full download transfers: the sum of every file's [`ExternalFile::download_size`]
    /// (saturating; real specs are far below `u64::MAX`). Progress and `Partial` counts use
    /// the same unit.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().fold(0_u64, |total, file| total.saturating_add(file.download_size()))
    }

    /// Absolute model directory under `side_models_root` (pass `ms_config::side_models_dir()`).
    #[must_use]
    pub fn local_dir(&self, side_models_root: &Path) -> PathBuf {
        join_relative(side_models_root, self.dir)
    }

    /// The completion marker of this spec (see [`DirLayout`]).
    fn marker_path(&self, side_models_root: &Path) -> PathBuf {
        let dir = self.local_dir(side_models_root);
        match self.layout {
            DirLayout::Exclusive => dir.join(COMPLETE_MARKER),
            DirLayout::Shared => dir.join(format!("{COMPLETE_MARKER_PREFIX}.{}.json", self.id)),
        }
    }

    /// The staging directory of this spec (see [`DirLayout`]).
    fn staging_dir(&self, side_models_root: &Path) -> PathBuf {
        let staging = self.local_dir(side_models_root).join(STAGING_DIR);
        match self.layout {
            DirLayout::Exclusive => staging,
            DirLayout::Shared => staging.join(self.id),
        }
    }
}

/// Stat-only install state of one spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalModelStatus {
    /// The completion marker matches this spec and every file has its exact size.
    Installed,
    /// A download of THIS spec was started and not finished; `bytes_present` counts, in
    /// [`ExternalModelSpec::total_bytes`] units, published files of exact size plus staged
    /// prefixes and verified archives (capped per file).
    Partial { bytes_present: u64 },
    /// Nothing usable for this spec (including an install of a different pin).
    Missing,
}

/// What the downloader is doing with the current file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalDownloadPhase {
    /// Streaming bytes from the network.
    Downloading,
    /// Hashing bytes already on disk (an existing file or a resumed `.part` prefix) or
    /// extracting a zip member.
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
    /// Bytes of the current file handled in this phase so far (hashing an existing zip
    /// member is scaled onto its archive size).
    pub file_done: u64,
    /// [`ExternalFile::download_size`] of the current file.
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
    /// file would have reached), or a zip member does not have its pinned size; the staged
    /// bytes were deleted. A body that ends EARLY is not this error: it is a transient
    /// failure, retried and resumed.
    #[error("size mismatch for '{file}': expected {expected} bytes, got {actual}")]
    SizeMismatch { file: &'static str, expected: u64, actual: u64 },
    /// The downloaded file (or its archive, `expected` then being the archive pin) did not
    /// have the pinned sha256; its staged bytes were deleted.
    #[error("sha256 mismatch for '{file}': expected {expected}, got {actual}")]
    HashMismatch { file: &'static str, expected: &'static str, actual: String },
    /// The spec (or a marker read from disk) violates the catalog rules, or a verified
    /// archive does not hold the pinned member.
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
    let marker_matches = read_json::<Marker>(&spec.marker_path(side_models_root)).as_ref() == Some(&identity);
    let staging = spec.staging_dir(side_models_root);
    let mut all_exact = true;
    let mut bytes_present = 0_u64;
    for file in spec.files {
        if file_len(&join_relative(&dir, file.path)) == Some(file.size) {
            bytes_present = bytes_present.saturating_add(file.download_size());
            continue;
        }
        all_exact = false;
        let archive_ready = matches!(file.origin, FileOrigin::ZipMember { archive, archive_size, .. }
            if file_len(&join_relative(&staging, archive)) == Some(archive_size));
        let staged = if archive_ready {
            file.download_size()
        } else {
            file_len(&part_path(&staging, file)).map_or(0, |len| len.min(file.download_size()))
        };
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
/// Transient network failures are retried per [`RetryPolicy::standard`]. A Hugging Face
/// source uses the cached token (`hf_token::hf_token()`) when non-empty; a `Url` source never
/// sends one.
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
    let remote_url = |file: &ExternalFile| match spec.source {
        ExternalSource::HuggingFace { repo_id, revision } => hf_resolve_url(repo_id, revision, file.remote_path()),
        ExternalSource::Url { base_url } => url_source_file_url(base_url, file.remote_path()),
    };
    let token = match spec.source {
        ExternalSource::HuggingFace { .. } => Some(crate::hf_token::hf_token()).filter(|token| !token.is_empty()),
        ExternalSource::Url { .. } => None,
    };
    let fetcher = UreqFetcher::new(token);
    download_with(&fetcher, &remote_url, side_models_root, spec, cancel, &RetryPolicy::standard(), progress)
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

/// Checks the catalog rules: non-empty id (`[a-z0-9_]` for a `Shared` directory) and files;
/// a Hugging Face source with an `owner/name` repo id and a 40-hex lowercase revision, or an
/// `https://` base URL without trailing slash, query, fragment, whitespace or `.`/`..`
/// segments; 64-hex lowercase sha256 pins; relative `/`-separated paths without
/// `.`/`..`/`\`/`:` components; unique file paths; no file inside the reserved staging
/// directory or named like a marker; for a zip member a safe archive path, a non-zero archive
/// size, a 64-hex archive pin and a non-empty member name.
///
/// # Errors
/// `ExternalModelError::InvalidSpec` naming the first violation.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn validate_spec(spec: &ExternalModelSpec) -> Result<(), ExternalModelError> {
    let invalid = |what: String| Err(ExternalModelError::InvalidSpec(format!("{}: {what}", spec.id)));
    if spec.id.trim().is_empty() {
        return invalid("empty id".to_owned());
    }
    if spec.layout == DirLayout::Shared && !spec.id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') {
        return invalid("a shared-directory id names its marker file: [a-z0-9_] only".to_owned());
    }
    match spec.source {
        ExternalSource::HuggingFace { repo_id, revision } => {
            let repo_parts: Vec<&str> = repo_id.split('/').collect();
            if repo_parts.len() != 2 || repo_parts.iter().any(|part| !is_safe_component(part)) {
                return invalid(format!("repository id '{repo_id}' is not owner/name"));
            }
            if !is_lower_hex(revision, 40) {
                return invalid(format!("revision '{revision}' is not a 40-character commit hash"));
            }
        }
        ExternalSource::Url { base_url } => {
            if !is_valid_base_url(base_url) {
                return invalid(format!("base URL '{base_url}' is not a plain https://host[/path] without trailing slash"));
            }
        }
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
        if first == STAGING_DIR || file.path.starts_with(COMPLETE_MARKER_PREFIX) {
            return invalid(format!("file path '{}' collides with a reserved name", file.path));
        }
        if !is_lower_hex(file.sha256, 64) {
            return invalid(format!("sha256 of '{}' is not 64 lowercase hex characters", file.path));
        }
        if spec.files[..index].iter().any(|earlier| earlier.path == file.path) {
            return invalid(format!("file path '{}' is listed twice", file.path));
        }
        if let FileOrigin::ZipMember { archive, archive_size, archive_sha256, member } = file.origin {
            if !is_safe_relative_path(archive) || archive.split('/').next() == Some(STAGING_DIR) {
                return invalid(format!("archive path '{archive}' of '{}' is not a safe relative path", file.path));
            }
            if archive_size == 0 || !is_lower_hex(archive_sha256, 64) {
                return invalid(format!("archive '{archive}' of '{}' needs a size and a 64-hex sha256", file.path));
            }
            if member.is_empty() || member.contains('\0') {
                return invalid(format!("archive member of '{}' is empty", file.path));
            }
        }
    }
    Ok(())
}

/// Whether `base_url` is `https://` + a host (optionally with port) + an optional path of
/// safe segments, with no trailing slash, query, fragment, credentials or whitespace.
#[cfg(not(target_arch = "wasm32"))]
fn is_valid_base_url(base_url: &str) -> bool {
    let Some(rest) = base_url.strip_prefix("https://") else { return false };
    if rest.is_empty() || rest.ends_with('/') || rest.contains(['?', '#', '@', '\\', ' ', '\t', '\n', '\r', '\0']) {
        return false;
    }
    let mut segments = rest.split('/');
    let host = segments.next().unwrap_or_default();
    !host.is_empty() && segments.all(is_safe_component)
}

/// Testable download core; see [`download_external_model`] for the contract. `remote_url`
/// maps a catalog file to the URL of its [`ExternalFile::remote_path`]; `retry` schedules
/// transient-failure retries.
///
/// # Errors
/// As [`download_external_model`] (never `Unsupported`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn download_with(
    fetcher: &dyn RangeFetcher,
    remote_url: &dyn Fn(&ExternalFile) -> String,
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    retry: &RetryPolicy<'_>,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    validate_spec(spec)?;
    // Process-wide "one download per spec" guard (separate from `ai_models::DOWNLOAD_LOCK`).
    let _active = BusyGuard::try_acquire(spec.id).map_err(|_busy| ExternalModelError::Busy { id: spec.id })?; // RAII: held until return
    let result = run_download(fetcher, remote_url, side_models_root, spec, cancel, retry, progress);
    match &result {
        Ok(dir) => ms_log::runtime_log::log_info(format!(
            "{LOG_TAG} download complete: id={} source={} bytes={} dir={}",
            spec.id,
            source_label(spec),
            spec.total_bytes(),
            dir.display()
        )),
        Err(ExternalModelError::Cancelled) => ms_log::runtime_log::log_info(format!(
            "{LOG_TAG} download cancelled: id={} source={} (staged parts kept for resume)",
            spec.id,
            source_label(spec)
        )),
        Err(error) => log_failure(spec, error),
    }
    result
}

/// Body of [`download_with`] under the busy guard; logging stays with the caller.
#[cfg(not(target_arch = "wasm32"))]
fn run_download(
    fetcher: &dyn RangeFetcher,
    remote_url: &dyn Fn(&ExternalFile) -> String,
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    retry: &RetryPolicy<'_>,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    let dir = spec.local_dir(side_models_root);
    let staging = spec.staging_dir(side_models_root);
    let marker = spec.marker_path(side_models_root);
    ms_log::runtime_log::log_info(format!(
        "{LOG_TAG} download start: id={} source={} files={} bytes={} dir={}",
        spec.id,
        source_label(spec),
        spec.files.len(),
        spec.total_bytes(),
        dir.display()
    ));
    create_dir_all(&staging)?;
    retire_complete_marker(&marker, &staging)?;
    prepare_staging(&staging, spec)?;

    let mut job = FileJob { fetcher, cancel, retry, progress, spec, dir: &dir, staging: &staging, done_before: 0 };
    for (index, file) in spec.files.iter().enumerate() {
        job.fetch_file(index, file, &remote_url(file))?;
        job.done_before = job.done_before.saturating_add(file.download_size());
    }

    // In a shared directory a file this spec no longer lists may belong to another spec.
    if spec.layout == DirLayout::Exclusive {
        remove_stale_files(&dir, &staging, spec)?;
    }
    write_json_atomically(&marker, &Marker::for_spec(spec))?;
    // The marker is written, so the model IS installed: a leftover staging directory (e.g. a
    // Windows sharing violation) is not a failed download. The next download into this
    // directory reuses the leftover staging and removes it when it completes.
    if let Err(error) = std::fs::remove_dir_all(&staging) {
        ms_log::runtime_log::log_warn(format!(
            "{LOG_TAG} installed, but the staging directory could not be removed: id={} path={} error={error}",
            spec.id,
            staging.display()
        ));
    }
    if spec.layout == DirLayout::Shared {
        remove_shared_staging_root(&dir.join(STAGING_DIR), spec);
    }
    Ok(dir)
}

/// Removes the common `.download/` of a shared directory once no spec stages in it any more;
/// a non-empty one (another spec's unfinished download) is left alone.
#[cfg(not(target_arch = "wasm32"))]
fn remove_shared_staging_root(root: &Path, spec: &ExternalModelSpec) {
    match std::fs::remove_dir(root) {
        Ok(()) => {}
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty) => {}
        Err(error) => ms_log::runtime_log::log_warn(format!(
            "{LOG_TAG} installed, but the shared staging directory could not be removed: id={} path={} error={error}",
            spec.id,
            root.display()
        )),
    }
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
    /// Sum of the download sizes of the files already handled.
    done_before: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl FileJob<'_> {
    /// Makes `file` present and verified at its final path.
    ///
    /// A `Direct` file goes through `ms_download::fetch_verified`: an already-correct file is
    /// kept (after hashing it), otherwise its `.part` is resumed or restarted (retrying
    /// transient failures), verified by size then sha256 and replace-renamed into place.
    /// A `ZipMember` file goes through [`FileJob::fetch_member`].
    fn fetch_file(&mut self, index: usize, file: &'static ExternalFile, url: &str) -> Result<(), ExternalModelError> {
        let final_path = join_relative(self.dir, file.path);
        match file.origin {
            FileOrigin::Direct => {
                let part = part_path(self.staging, file);
                let request = FileRequest { url, part: &part, dest: &final_path, expected: Expected::SizeSha256 { size: file.size, sha256: file.sha256 } };
                self.fetch_verified_into(index, file, &request, file.sha256)
            }
            FileOrigin::ZipMember { archive, archive_size, archive_sha256, member } => {
                self.fetch_member(index, file, url, &final_path, ZipPin { archive, archive_size, archive_sha256, member })
            }
        }
    }

    /// Publishes the zip member `file` at `final_path`.
    ///
    /// Under a waiting busy id of its own (`external-member:<final path>`, so specs that share
    /// the file in a `Shared` directory serialize instead of failing): keep a published file
    /// that already hashes to its pin; otherwise fetch the archive with `fetch_verified`
    /// (size + sha256 pin) into this spec's staging, extract the member with
    /// `zip_member_atomic` (size + sha256 pin, atomic publish) and delete the archive.
    fn fetch_member(&mut self, index: usize, file: &'static ExternalFile, url: &str, final_path: &Path, pin: ZipPin) -> Result<(), ExternalModelError> {
        let key = member_key(final_path);
        let (cancel, retry) = (self.cancel, self.retry);
        let _guard = BusyGuard::acquire_waiting(&key, cancel, retry, &mut || self.report(index, file, ExternalDownloadPhase::Downloading, 0))
            .map_err(|error| map_download_error(self.spec, file, file.sha256, error))?;
        let staged_archive = join_relative(self.staging, pin.archive);
        if file_len(final_path) == Some(file.size) {
            if self.hashes_to(index, file, final_path, file.sha256)? {
                remove_file_if_exists(&staged_archive)?;
                remove_file_if_exists(&part_path(self.staging, file))?;
                return Ok(());
            }
            ms_log::runtime_log::log_info(format!(
                "{LOG_TAG} existing file differs from the pin, extracting it again: id={} file={}",
                self.spec.id, file.path
            ));
        }
        let part = part_path(self.staging, file);
        let request = FileRequest { url, part: &part, dest: &staged_archive, expected: Expected::SizeSha256 { size: pin.archive_size, sha256: pin.archive_sha256 } };
        self.fetch_verified_into(index, file, &request, pin.archive_sha256)?;
        self.report(index, file, ExternalDownloadPhase::Verifying, 0);
        zip_member_atomic(&staged_archive, pin.member, final_path, Expected::SizeSha256 { size: file.size, sha256: file.sha256 })
            .map_err(|error| map_download_error(self.spec, file, file.sha256, error))?;
        self.report(index, file, ExternalDownloadPhase::Verifying, file.download_size());
        remove_file_if_exists(&staged_archive)
    }

    /// One `fetch_verified` of `request` (catalog file `file`, the `index`-th of the spec), with
    /// its events mapped onto [`ExternalDownloadProgress`] and its errors onto
    /// [`ExternalModelError`] (`pin_sha256` is the pin a `HashMismatch` names).
    fn fetch_verified_into(&mut self, index: usize, file: &'static ExternalFile, request: &FileRequest<'_>, pin_sha256: &'static str) -> Result<(), ExternalModelError> {
        let log_context = format!("id={} file={}", self.spec.id, file.path);
        let engine = ResumableFetch { fetcher: self.fetcher, retry: self.retry, cancel: self.cancel, log_tag: LOG_TAG, log_context: &log_context };
        let result = fetch_verified(&engine, request, &mut |event| match event {
            FileEvent::Waiting => self.report(index, file, ExternalDownloadPhase::Downloading, 0),
            FileEvent::Verifying { done, .. } => self.report(index, file, ExternalDownloadPhase::Verifying, done),
            FileEvent::Stream(StreamEvent::Downloading { staged, .. }) => self.report(index, file, ExternalDownloadPhase::Downloading, staged),
            FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged }) => {
                self.report(index, file, ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs }, staged);
            }
        });
        result.map(|_size| ()).map_err(|error| map_download_error(self.spec, file, pin_sha256, error))
    }

    /// Whether the whole of `path` hashes to `sha256`, reporting `Verifying` progress scaled
    /// onto the file's download size and honouring cancel.
    fn hashes_to(&mut self, index: usize, file: &'static ExternalFile, path: &Path, sha256: &str) -> Result<bool, ExternalModelError> {
        let cancel = self.cancel;
        hash_matches(path, sha256, cancel, &mut |hashed| self.report(index, file, ExternalDownloadPhase::Verifying, scale(hashed, file.size, file.download_size())))
    }

    fn report(&mut self, index: usize, file: &'static ExternalFile, phase: ExternalDownloadPhase, file_done: u64) {
        let file_total = file.download_size();
        let file_done = file_done.min(file_total);
        (self.progress)(&ExternalDownloadProgress {
            file_index: index,
            file_count: self.spec.files.len(),
            file_path: file.path,
            phase,
            file_done,
            file_total,
            total_done: self.done_before.saturating_add(file_done),
            total_bytes: self.spec.total_bytes(),
        });
    }
}

/// Whether the whole of `path` hashes to `sha256` (lowercase hex); `on_hashed(bytes so far)`
/// is called first with 0 and after every read of at most [`CHUNK`] bytes; `cancel` is
/// polled before every read.
///
/// # Errors
/// `Cancelled`, `Io`.
#[cfg(not(target_arch = "wasm32"))]
fn hash_matches(path: &Path, sha256: &str, cancel: &AtomicBool, on_hashed: &mut dyn FnMut(u64)) -> Result<bool, ExternalModelError> {
    use sha2::Digest;
    use std::io::Read;

    let mut handle = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut hashed = 0_u64;
    on_hashed(0);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        let read = handle.read(&mut buffer).map_err(|source| io_error(path, source))?;
        if read == 0 {
            return Ok(hex_lower(&hasher.finalize()) == sha256);
        }
        hasher.update(&buffer[..read]);
        hashed = hashed.saturating_add(chunk_len(read));
        on_hashed(hashed);
    }
}

/// Result of [`verify_external_model`]: every file of the spec, by catalog path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Files that hash to their pin (kept).
    pub verified: Vec<&'static str>,
    /// Files that are absent (nothing to delete; a download fetches them).
    pub missing: Vec<&'static str>,
    /// Files whose size or content did not match the pin; they were deleted.
    pub removed: Vec<&'static str>,
    /// Ids of the specs whose completion marker was deleted because a file they list was
    /// removed (this spec, plus every spec sharing that file in a `Shared` directory).
    pub invalidated: Vec<&'static str>,
}

impl VerifyReport {
    /// Whether every file was present and matched its pin.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.missing.is_empty() && self.removed.is_empty()
    }
}

/// Hashes every file of `spec` against its pin (the repair path behind a "Verify files"
/// action; the stat-only [`external_model_status`] cannot see a same-size corruption).
///
/// A file whose size or sha256 differs is DELETED, together with the completion marker of
/// every spec that lists that file in the same directory (for a shared file, e.g. the EasyOCR
/// detector, all scripts using it), so each affected spec reads not-installed and a normal
/// [`download_external_model`] repairs it. Matching files and absent files are left alone.
/// Blocking, worker threads only: reads every byte of the model. `progress` reports
/// `Verifying` per read (in `download_size` units); `cancel` is polled before every read.
///
/// # Errors
/// `InvalidSpec`, `Busy` (a download of this spec is running), `Cancelled` (files deleted so
/// far stay deleted, their markers already removed), `Io`; on wasm always `Unsupported`.
#[cfg(not(target_arch = "wasm32"))]
pub fn verify_external_model(
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<VerifyReport, ExternalModelError> {
    let related = super::external_catalog::all_specs();
    verify_with(side_models_root, spec, &related, cancel, &RetryPolicy::standard(), progress)
}

/// Web build: nothing to verify against a blocking file read here.
///
/// # Errors
/// Always `ExternalModelError::Unsupported`.
#[cfg(target_arch = "wasm32")]
pub fn verify_external_model(
    _side_models_root: &Path,
    _spec: &'static ExternalModelSpec,
    _cancel: &AtomicBool,
    _progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<VerifyReport, ExternalModelError> {
    Err(ExternalModelError::Unsupported)
}

/// Core of [`verify_external_model`]; `related` are the specs whose markers a removed shared
/// file invalidates (the whole catalog in production), `retry` paces the wait for another
/// holder of a shared member.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn verify_with(
    side_models_root: &Path,
    spec: &'static ExternalModelSpec,
    related: &[&'static ExternalModelSpec],
    cancel: &AtomicBool,
    retry: &RetryPolicy<'_>,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<VerifyReport, ExternalModelError> {
    validate_spec(spec)?;
    // The same guard as a download of this spec: never hash files a download is replacing.
    let _active = BusyGuard::try_acquire(spec.id).map_err(|_busy| ExternalModelError::Busy { id: spec.id })?;
    let dir = spec.local_dir(side_models_root);
    let mut report = VerifyReport::default();
    let mut done_before = 0_u64;
    for (index, file) in spec.files.iter().enumerate() {
        let path = join_relative(&dir, file.path);
        let mut emit = |file_done: u64| {
            let file_done = file_done.min(file.download_size());
            progress(&ExternalDownloadProgress {
                file_index: index,
                file_count: spec.files.len(),
                file_path: file.path,
                phase: ExternalDownloadPhase::Verifying,
                file_done,
                file_total: file.download_size(),
                total_done: done_before.saturating_add(file_done),
                total_bytes: spec.total_bytes(),
            });
        };
        // A shared member may be being published by another spec's download: wait for it.
        let _member = match file.origin {
            FileOrigin::ZipMember { .. } => Some(
                BusyGuard::acquire_waiting(&member_key(&path), cancel, retry, &mut || emit(0))
                    .map_err(|error| map_download_error(spec, file, file.sha256, error))?,
            ),
            FileOrigin::Direct => None,
        };
        let intact = match file_len(&path) {
            None => {
                report.missing.push(file.path);
                false
            }
            Some(len) if len != file.size => false,
            Some(_) => hash_matches(&path, file.sha256, cancel, &mut |hashed| emit(scale(hashed, file.size, file.download_size())))?,
        };
        if intact {
            report.verified.push(file.path);
        } else if file_len(&path).is_some() {
            remove_file_if_exists(&path)?;
            report.removed.push(file.path);
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} verify: removed a file that does not match its pin: id={} file={}", spec.id, file.path));
            invalidate_markers(side_models_root, &dir, file, related, &mut report.invalidated)?;
        }
        emit(file.download_size());
        done_before = done_before.saturating_add(file.download_size());
    }
    ms_log::runtime_log::log_info(format!(
        "{LOG_TAG} verify finished: id={} verified={} missing={} removed={} invalidated={:?}",
        spec.id,
        report.verified.len(),
        report.missing.len(),
        report.removed.len(),
        report.invalidated
    ));
    Ok(report)
}

/// Deletes the completion marker of every spec in `related` that lists `file` (same path,
/// same directory `dir`), recording each id once in `invalidated`.
#[cfg(not(target_arch = "wasm32"))]
fn invalidate_markers(
    side_models_root: &Path,
    dir: &Path,
    file: &ExternalFile,
    related: &[&'static ExternalModelSpec],
    invalidated: &mut Vec<&'static str>,
) -> Result<(), ExternalModelError> {
    for other in related {
        if other.local_dir(side_models_root) != dir || !other.files.iter().any(|candidate| candidate.path == file.path) {
            continue;
        }
        remove_file_if_exists(&other.marker_path(side_models_root))?;
        if !invalidated.contains(&other.id) {
            invalidated.push(other.id);
        }
    }
    Ok(())
}

/// Busy id under which a zip member's final path is published (download) or hashed
/// (verify), so specs sharing the file serialize.
#[cfg(not(target_arch = "wasm32"))]
fn member_key(final_path: &Path) -> String {
    // `absolute` fails only for an empty path or an unreadable current directory; the path
    // as given is then still a stable key for every caller spelling it the same way.
    format!("external-member:{}", std::path::absolute(final_path).unwrap_or_else(|_| final_path.to_path_buf()).display())
}

/// The archive pin of a zip-member file (the fields of `FileOrigin::ZipMember`).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct ZipPin {
    archive: &'static str,
    archive_size: u64,
    archive_sha256: &'static str,
    member: &'static str,
}

/// `done` of `from` expressed in units of `to` (`to` when `from` is 0).
#[cfg(not(target_arch = "wasm32"))]
fn scale(done: u64, from: u64, to: u64) -> u64 {
    if from == 0 {
        return to;
    }
    let scaled = u128::from(done).saturating_mul(u128::from(to)) / u128::from(from);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Maps an `ms_download` failure while handling `file` onto the module's typed error;
/// `pin_sha256` is the pin a hash mismatch was checked against.
#[cfg(not(target_arch = "wasm32"))]
fn map_download_error(spec: &'static ExternalModelSpec, file: &'static ExternalFile, pin_sha256: &'static str, error: DownloadError) -> ExternalModelError {
    match error {
        DownloadError::Cancelled => ExternalModelError::Cancelled,
        DownloadError::Busy { .. } => ExternalModelError::Busy { id: spec.id },
        DownloadError::InvalidRequest { detail } => ExternalModelError::InvalidSpec(format!("{}: {detail}", spec.id)),
        DownloadError::Io { path, source } => ExternalModelError::Io { path, source },
        DownloadError::Http { detail } => ExternalModelError::Http { file: file.path, detail },
        DownloadError::SizeMismatch { expected, actual } => ExternalModelError::SizeMismatch { file: file.path, expected, actual },
        DownloadError::HashMismatch { actual, .. } => ExternalModelError::HashMismatch { file: file.path, expected: pin_sha256, actual },
        DownloadError::Archive { detail } => ExternalModelError::InvalidSpec(format!("{}: archive of '{}': {detail}", spec.id, file.path)),
    }
}

/// `repo@revision` or the base URL of a spec, for log lines.
#[cfg(not(target_arch = "wasm32"))]
fn source_label(spec: &ExternalModelSpec) -> String {
    match spec.source {
        ExternalSource::HuggingFace { repo_id, revision } => format!("{repo_id}@{revision}"),
        ExternalSource::Url { base_url } => base_url.to_owned(),
    }
}

/// Moves an existing completion marker into staging, so no reader sees "installed" while
/// files are being replaced, while its file list survives a cancel or crash for the
/// stale-file cleanup at the end.
#[cfg(not(target_arch = "wasm32"))]
fn retire_complete_marker(marker: &Path, staging: &Path) -> Result<(), ExternalModelError> {
    match std::fs::rename(marker, staging.join(PREVIOUS_MARKER)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(marker, source)),
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
    write_json_atomically(&in_progress, &identity)
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
    write_json_atomically(&path, &abandoned)
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
        if !is_safe_relative_path(old) || old.split('/').next() == Some(STAGING_DIR) || old.starts_with(COMPLETE_MARKER_PREFIX) {
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} ignoring unsafe stale path in a staging record: id={} path={old:?}", spec.id));
            continue;
        }
        remove_file_if_exists(&join_relative(dir, old))?;
        ms_log::runtime_log::log_info(format!("{LOG_TAG} removed stale file: id={} file={old}", spec.id));
    }
    Ok(())
}

/// Serializes `value` and replaces `target` with it through the owner recipe
/// (`ms_atomic_file::write_atomic`: hidden sibling temp `.{name}.{pid}.tmp` in `target`'s own
/// directory, contents fsynced, rename; the temp is removed on every failure). The parent must
/// exist. Identical bytes are kept as they are (only re-fsynced): every record here is read for
/// its contents only. Worker thread only.
#[cfg(not(target_arch = "wasm32"))]
fn write_json_atomically<T: Serialize>(target: &Path, value: &T) -> Result<(), ExternalModelError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| ExternalModelError::InvalidSpec(format!("marker serialization failed: {err}")))?;
    ms_atomic_file::write_atomic(target, &bytes, ms_atomic_file::Durability::Contents).map_err(|err| io_error(target, err.into()))
}

#[cfg(not(target_arch = "wasm32"))]
fn log_failure(spec: &ExternalModelSpec, error: &ExternalModelError) {
    ms_log::runtime_log::log_error(format!("{LOG_TAG} download failed: id={} source={} error={error}", spec.id, source_label(spec)));
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
/// pin (source, file path, size, hash or archive) makes two markers unequal.
///
/// Byte compatibility: a Hugging Face spec of plain direct files serializes exactly as the
/// markers written before `Url` sources and zip members existed (`{id, repo_id, revision,
/// files: [{path, size, sha256}]}`, pretty-printed), because every newer field is omitted
/// when absent; existing installs therefore keep reading `Installed`.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    files: Vec<MarkerFile>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MarkerFile {
    path: String,
    size: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<MarkerArchive>,
}

/// The archive pin of a zip-member file inside a marker.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MarkerArchive {
    path: String,
    size: u64,
    sha256: String,
    member: String,
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
        let (repo_id, revision, url) = match spec.source {
            ExternalSource::HuggingFace { repo_id, revision } => (Some(repo_id.to_owned()), Some(revision.to_owned()), None),
            ExternalSource::Url { base_url } => (None, None, Some(base_url.to_owned())),
        };
        Self {
            id: spec.id.to_owned(),
            repo_id,
            revision,
            url,
            files: spec
                .files
                .iter()
                .map(|file| MarkerFile {
                    path: file.path.to_owned(),
                    size: file.size,
                    sha256: file.sha256.to_owned(),
                    archive: match file.origin {
                        FileOrigin::Direct => None,
                        FileOrigin::ZipMember { archive, archive_size, archive_sha256, member } => Some(MarkerArchive {
                            path: archive.to_owned(),
                            size: archive_size,
                            sha256: archive_sha256.to_owned(),
                            member: member.to_owned(),
                        }),
                    },
                })
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

/// Staging path of `file`'s partial download: `<staging>/<remote path>.part` (the archive's
/// path for a zip member).
fn part_path(staging: &Path, file: &ExternalFile) -> PathBuf {
    join_relative(staging, &format!("{}.part", file.remote_path()))
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
