/*
File: onnx_runtime/mod.rs

Purpose:
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform/provider so the `ms-onnx` crate can dlopen it. On first
use it probes a portable, per-provider cache directory; if the library is absent
it downloads the pinned official release archive, verifies its SHA256, extracts
the loadable library, and returns its on-disk path. The app starts fine without
the library present — resolution happens only when a caller asks for it.

Key types:
- OrtRuntimeError    : typed failure surface for probe/download/verify/extract
- OrtDownloadStage   : coarse progress stage (Probing/Downloading/.../Done)
- OrtDownloadProgress: byte counters + stage reported through the callback

Key functions:
- ort_dylib_dir                : pure per-provider/version cache directory
- resolve_or_download_ort_dylib: probe or download+verify+extract, worker-thread only

Notes:
The onnxruntime version is per manifest ENTRY, not global: the resolved cache dir
and versioned library name come from the looked-up entry's `version` (WebGPU on
1.27.0 coexists with the 1.20.1 providers). An entry may pull extra libraries from
`additional_sources` (extra archives) into the same cache directory; the primary
and every additional source share one download+verify+extract path. Downloads and
disk I/O are blocking; `resolve_or_download_ort_dylib` MUST run on a worker thread,
never on the GUI thread. This module owns dylib RESOLUTION only; it never links,
initializes, or runs onnxruntime — that is `ms-onnx`'s job, and it consumes the
path returned here. Storage lives under `config::data_dir()`.
*/

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use tar::Archive as TarArchive;
use zip::ZipArchive;

use crate::config;
use crate::runtime_log;

pub mod builds;
mod manifest;

// Build-catalog + manifest surface consumed by `native_runtime` (the guard-scope key
// and the resolve call site) and, in the next task, by the AI backend panel's build
// picker. `build_version` is the build-keyed primary; `provider_version` is the
// back-compat shim mapping a provider to its default build. The allow suppresses the
// unused-import warning only until the build-selection UI consumes `build_version`
// directly.
#[allow(unused_imports)]
pub use manifest::{ORT_VERSION, build_version, provider_version};

/// Read buffer size for streaming the archive download to disk.
const DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;

/// Minimum number of freshly downloaded bytes between throttled progress reports.
const PROGRESS_EMIT_INTERVAL_BYTES: u64 = 512 * 1024;

/// TCP connect timeout for the archive download.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-read timeout for the archive download.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Errors from probing, downloading, verifying, or extracting ONNX Runtime.
///
/// `Display` is hand-written (see the `impl` below) and localized through the
/// `ms-i18n` catalog (`onnx_runtime.error.*` keys), so the user-facing message
/// follows the selected UI language instead of a compile-time Russian literal.
/// `Debug` (derived) stays a stable English variant name and is what the log sites
/// format, keeping logs language-independent and grep-able. Wrapped strings add
/// diagnostic context, rendered inline through the frame's `Display`.
#[derive(Debug)]
pub enum OrtRuntimeError {
    /// No manifest row exists for this platform/build (e.g. a GPU build not shipped
    /// on this OS). This is a hard error, never a silent CPU substitution.
    NoManifestEntry {
        /// Normalized OS key that was looked up.
        os: &'static str,
        /// Normalized architecture key that was looked up.
        arch: &'static str,
        /// Build slug that was looked up.
        build: String,
    },

    /// The archive could not be downloaded (network/transport/HTTP status).
    Download(String),

    /// The downloaded archive's SHA256 did not match the pinned value; the file
    /// is deleted and resolution is aborted.
    ChecksumMismatch {
        /// Expected lowercase hex digest from the manifest.
        expected: String,
        /// Actual lowercase hex digest of the downloaded archive.
        actual: String,
    },

    /// The archive could not be opened or unpacked.
    Extraction(String),

    /// The expected onnxruntime library was not found inside the archive.
    DylibNotFoundInArchive(String),

    /// A filesystem operation (create dir, write, rename, remove) failed.
    Io(String),
}

impl std::fmt::Display for OrtRuntimeError {
    /// Renders the localized user-facing message for the active UI language via
    /// `ms-i18n` (`onnx_runtime.error.*`), interpolating each wrapped detail so the
    /// old derived `#[error]` information is preserved. The `match` is exhaustive so
    /// a new variant forces a new catalog key here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoManifestEntry { os, arch, build } => f.write_str(&tf!(
                "onnx_runtime.error.no_manifest_entry",
                os = os,
                arch = arch,
                build = build
            )),
            Self::Download(detail) => {
                f.write_str(&tf!("onnx_runtime.error.download", detail = detail))
            }
            Self::ChecksumMismatch { expected, actual } => f.write_str(&tf!(
                "onnx_runtime.error.checksum_mismatch",
                expected = expected,
                actual = actual
            )),
            Self::Extraction(detail) => {
                f.write_str(&tf!("onnx_runtime.error.extraction", detail = detail))
            }
            Self::DylibNotFoundInArchive(member) => {
                f.write_str(&tf!("onnx_runtime.error.dylib_not_found", member = member))
            }
            Self::Io(detail) => f.write_str(&tf!("onnx_runtime.error.io", detail = detail)),
        }
    }
}

// No variant wraps another error (all carry Strings or plain fields), so the
// default `source()` (None) is correct; the impl is required so this type can be an
// `Error` source for `NativeRuntimeError`.
impl std::error::Error for OrtRuntimeError {}

/// Coarse stage of an ONNX Runtime resolution, reported via the progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtDownloadStage {
    /// Checking the on-disk cache before any network access.
    Probing,
    /// Streaming the release archive to disk.
    Downloading,
    /// Verifying (or computing) the archive SHA256.
    Verifying,
    /// Unpacking the library out of the archive.
    Extracting,
    /// Resolution finished; the dylib path is available.
    Done,
}

/// Progress snapshot passed to the caller's callback during resolution.
///
/// `downloaded`/`total` are meaningful during [`OrtDownloadStage::Downloading`]
/// (`total` is `None` when the server omits `Content-Length`); other stages report
/// `downloaded = 0`, `total = None`.
#[derive(Debug, Clone, Copy)]
pub struct OrtDownloadProgress {
    /// Bytes downloaded so far (download stage only).
    pub downloaded: u64,
    /// Total archive size if known.
    pub total: Option<u64>,
    /// Current resolution stage.
    pub stage: OrtDownloadStage,
}

/// Portable per-build, per-version cache directory for the onnxruntime library.
///
/// Pure path construction: `data_dir()/onnxruntime/<build_slug>/<ort_version>/`.
/// Scoping by BUILD SLUG (not provider id) keeps builds that share a provider but
/// differ by version — e.g. `cuda12` (1.24.1) and `cuda13` (1.27.0) — in separate
/// cache directories. Creates nothing on disk.
#[must_use]
pub fn ort_dylib_dir(build_slug: &str, ort_version: &str) -> PathBuf {
    config::data_dir()
        .join("onnxruntime")
        .join(build_slug)
        .join(ort_version)
}

/// Resolves the onnxruntime library path for BUILD `build`, downloading it on first use.
///
/// `build` is a stable build slug from the [`builds`] catalog (e.g. `"cpu"`,
/// `"cuda13"`, `"openvino"`). Probes [`ort_dylib_dir`] for an already-extracted
/// library and returns its path immediately (no network) if present. Otherwise it
/// looks up the manifest entry for the current platform and this build, downloads the
/// official archive, verifies its SHA256 (mismatch is a hard error — the bad file is
/// deleted), extracts the loadable library into the cache directory, and returns the
/// primary dylib path.
///
/// `progress` is invoked with stage/byte updates (throttled during download). It
/// is called on the calling (worker) thread.
///
/// # Threading
/// Performs blocking network and disk I/O. It MUST be called from a worker thread,
/// never from the GUI thread.
///
/// # Errors
/// - [`OrtRuntimeError::NoManifestEntry`] if no archive is pinned for this platform/build.
/// - [`OrtRuntimeError::Download`] on network/HTTP failure.
/// - [`OrtRuntimeError::ChecksumMismatch`] if the archive fails SHA256 verification.
/// - [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::DylibNotFoundInArchive`] on unpack failure.
/// - [`OrtRuntimeError::Io`] on filesystem failure.
pub fn resolve_or_download_ort_dylib(
    build: &str,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PathBuf, OrtRuntimeError> {
    // The version is per-entry, so the manifest lookup must run BEFORE computing the
    // cache dir and probing: each build resolves under its own version. The lookup is
    // an in-memory scan (no network), and a cached library can only exist after a
    // successful download (which requires an entry), so probing after the lookup never
    // regresses the offline fast path.
    let (os, arch) = manifest::current_platform();
    let entry = manifest::lookup_build(os, arch, build).ok_or_else(|| {
        OrtRuntimeError::NoManifestEntry {
            os,
            arch,
            build: build.to_string(),
        }
    })?;

    let provider_id = entry.provider.as_str();
    let version = entry.version.as_str();
    // Cache is scoped by BUILD SLUG so builds sharing a provider (cuda12 / cuda13) do
    // not collide.
    let dir = ort_dylib_dir(build, version);
    let primary_name = expected_primary_dylib_filename(version);
    let primary_path = dir.join(&primary_name);

    emit(progress, OrtDownloadStage::Probing, 0, None);
    // Fast path: skip the download only when EVERY expected member (primary +
    // extras + every additional-source member) is already on disk. A partial cache
    // (e.g. a prior run that failed before extracting an additional source) must
    // fall through and re-fetch, so a missing sidecar is never silently ignored.
    let expected_members = expected_member_paths(&dir, &primary_name, entry)?;
    if expected_members.iter().all(|path| path.is_file()) {
        runtime_log::log_info(format!(
            "[onnx-runtime] found cached library for provider '{provider_id}' at {}",
            primary_path.display()
        ));
        emit(progress, OrtDownloadStage::Done, 0, None);
        return Ok(primary_path);
    }

    fs::create_dir_all(&dir).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.create_dir_error", dir = dir.display(), err = err))
    })?;
    let download_dir = dir.join(".download");
    fs::create_dir_all(&download_dir).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.create_download_dir_error", download_dir = download_dir.display(), err = err))
    })?;

    // Primary source: the onnxruntime archive itself. Its `dylib_member` is renamed
    // to the version-scoped `primary_name`; `extra_members` keep their filenames.
    let mut primary_wanted: HashMap<String, PathBuf> = HashMap::new();
    primary_wanted.insert(entry.dylib_member.clone(), dir.join(&primary_name));
    add_flattened_members(&mut primary_wanted, &entry.extra_members, &dir)?;

    runtime_log::log_info(format!(
        "[onnx-runtime] downloading ONNX Runtime {version} for provider '{provider_id}' from {}",
        entry.url
    ));
    fetch_source(
        &entry.url,
        &entry.sha256,
        entry.archive,
        &primary_wanted,
        &download_dir,
        progress,
    )?;

    if !primary_path.is_file() {
        return Err(OrtRuntimeError::DylibNotFoundInArchive(
            entry.dylib_member.clone(),
        ));
    }

    // Additional sources: extra archives (e.g. sidecar runtime DLLs) extracted into
    // the SAME cache directory via the exact same fetch path as the primary source.
    for source in &entry.additional_sources {
        let mut wanted: HashMap<String, PathBuf> = HashMap::new();
        add_flattened_members(&mut wanted, &source.members, &dir)?;
        runtime_log::log_info(format!(
            "[onnx-runtime] downloading additional source for provider '{provider_id}' from {}",
            source.url
        ));
        fetch_source(
            &source.url,
            &source.sha256,
            source.archive,
            &wanted,
            &download_dir,
            progress,
        )?;
    }

    runtime_log::log_info(format!(
        "[onnx-runtime] ready for provider '{provider_id}': {}",
        primary_path.display()
    ));
    emit(progress, OrtDownloadStage::Done, 0, None);
    Ok(primary_path)
}

/// The on-disk paths of every library an entry is expected to place in `dir`.
///
/// Includes the primary library (as `primary_name`) plus each `extra_members` and
/// `additional_sources` member, flattened to its filename. Used by the probe fast
/// path to require a COMPLETE cache before skipping the download.
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if any member path has no filename component.
fn expected_member_paths(
    dir: &Path,
    primary_name: &str,
    entry: &manifest::ManifestEntry,
) -> Result<Vec<PathBuf>, OrtRuntimeError> {
    let mut paths = vec![dir.join(primary_name)];
    for member in &entry.extra_members {
        paths.push(dir.join(member_file_name(member)?));
    }
    for source in &entry.additional_sources {
        for member in &source.members {
            paths.push(dir.join(member_file_name(member)?));
        }
    }
    Ok(paths)
}

/// Inserts each `members` archive path into `wanted`, flattened to its filename in
/// `dir` (so archive-path traversal is impossible).
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if a member path has no filename component.
fn add_flattened_members(
    wanted: &mut HashMap<String, PathBuf>,
    members: &[String],
    dir: &Path,
) -> Result<(), OrtRuntimeError> {
    for member in members {
        wanted.insert(member.clone(), dir.join(member_file_name(member)?));
    }
    Ok(())
}

/// The trailing filename of an archive-internal member path.
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if `member` has no filename component.
fn member_file_name(member: &str) -> Result<PathBuf, OrtRuntimeError> {
    Path::new(member)
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.invalid_member_path", member = member))
        })
}

/// Downloads, verifies, and extracts one source archive into the cache directory.
///
/// Shared by the primary source and every additional source: streams `url` to a
/// `.part` file, atomically renames it, verifies (or logs) its SHA256, extracts
/// exactly the members in `wanted`, and removes the archive. `wanted` maps each
/// archive-internal member path to its destination path in the cache directory.
///
/// # Errors
/// - [`OrtRuntimeError::Download`] on network/HTTP failure.
/// - [`OrtRuntimeError::ChecksumMismatch`] if a pinned SHA256 fails to match.
/// - [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::DylibNotFoundInArchive`]
///   if the archive cannot be unpacked or a wanted member is absent.
/// - [`OrtRuntimeError::Io`] on filesystem failure.
fn fetch_source(
    url: &str,
    sha256: &Option<String>,
    archive: manifest::ArchiveKind,
    wanted: &HashMap<String, PathBuf>,
    download_dir: &Path,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<(), OrtRuntimeError> {
    let archive_name = archive_file_name(url);
    let archive_path = download_dir.join(&archive_name);
    let partial_path = download_dir.join(format!("{archive_name}.part"));

    download_to(url, &partial_path, &archive_path, progress)?;

    emit(progress, OrtDownloadStage::Verifying, 0, None);
    verify_or_report_hash(url, sha256, &archive_path)?;

    emit(progress, OrtDownloadStage::Extracting, 0, None);
    extract_wanted(&archive_path, archive, wanted)?;

    // The archive is no longer needed once its libraries are extracted; a cleanup
    // failure is non-fatal (the cache directory still holds the extracted files).
    if let Err(err) = fs::remove_file(&archive_path) {
        runtime_log::log_warn(format!(
            "[onnx-runtime] failed to remove archive '{}': {err}",
            archive_path.display()
        ));
    }
    Ok(())
}

/// Verifies the downloaded archive against its pinned hash, or — when no hash is
/// pinned — logs a clear "integrity NOT verified" warning plus the actual digest
/// so a maintainer can pin it. A mismatch deletes the file and aborts.
fn verify_or_report_hash(
    url: &str,
    sha256: &Option<String>,
    archive_path: &Path,
) -> Result<(), OrtRuntimeError> {
    match sha256 {
        Some(expected) => {
            if let Err(err) = manifest::verify_sha256_file(archive_path, expected) {
                // Corrupt/tampered download: never proceed to extraction. Delete the
                // bad file so the next attempt re-downloads from scratch.
                if let Err(remove_err) = fs::remove_file(archive_path) {
                    runtime_log::log_warn(format!(
                        "[onnx-runtime] failed to delete mismatched archive '{}': {remove_err}",
                        archive_path.display()
                    ));
                }
                runtime_log::log_error(format!(
                    "[onnx-runtime] SHA256 mismatch for '{}': {err}",
                    archive_path.display()
                ));
                return Err(err);
            }
            runtime_log::log_info(format!(
                "[onnx-runtime] SHA256 verified for '{}'",
                archive_path.display()
            ));
            Ok(())
        }
        None => {
            // Documented, VISIBLE limitation (not a silent fallback): no hash is
            // pinned for this entry. Compute and log the actual digest so a
            // maintainer can paste it into ort_manifest.json (the removal
            // condition for this gap; see MODULE_README).
            let actual = manifest::sha256_hex_of_file(archive_path)?;
            runtime_log::log_warn(format!(
                "[onnx-runtime] integrity NOT verified for '{url}' (no pinned SHA256). \
                 Downloaded archive SHA256 = {actual}. Pin this value in \
                 src/onnx_runtime/ort_manifest.json to enable verification."
            ));
            Ok(())
        }
    }
}

/// Streams `url` into `partial_path`, then atomically renames it to `final_path`.
///
/// Emits throttled [`OrtDownloadStage::Downloading`] progress. A stale `.part`
/// from an aborted attempt is removed first.
fn download_to(
    url: &str,
    partial_path: &Path,
    final_path: &Path,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<(), OrtRuntimeError> {
    if partial_path.exists() {
        fs::remove_file(partial_path).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = partial_path.display(), err = err))
        })?;
    }

    let response = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build()
        .get(url)
        .call()
        .map_err(|err| download_error(url, err))?;

    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok());

    emit(progress, OrtDownloadStage::Downloading, 0, total);

    let mut reader = response.into_reader();
    let mut output = File::create(partial_path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.create_archive_error", partial_path = partial_path.display(), err = err))
    })?;

    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
    let mut downloaded: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let read = reader.read(&mut buffer).map_err(|err| {
            OrtRuntimeError::Download(tf!("onnx_runtime.download.read_error", url = url, err = err))
        })?;
        if read == 0 {
            break;
        }
        output.write_all_chunk(&buffer[..read], partial_path)?;
        let read_u64 = u64::try_from(read).map_err(|err| {
            OrtRuntimeError::Download(tf!("onnx_runtime.download.invalid_chunk_size", read = read, err = err))
        })?;
        downloaded = downloaded.saturating_add(read_u64);
        // Throttle: only report every PROGRESS_EMIT_INTERVAL_BYTES to avoid
        // flooding the GUI channel behind the callback.
        if downloaded.saturating_sub(last_emit) >= PROGRESS_EMIT_INTERVAL_BYTES {
            last_emit = downloaded;
            emit(progress, OrtDownloadStage::Downloading, downloaded, total);
        }
    }

    flush_file(&mut output, partial_path)?;
    drop(output);
    emit(progress, OrtDownloadStage::Downloading, downloaded, total);

    fs::rename(partial_path, final_path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.move_archive_error", partial_path = partial_path.display(), final_path = final_path.display(), err = err))
    })?;
    Ok(())
}

/// A `File` write helper that maps I/O failures to a typed error with context.
trait ChunkWrite {
    /// Writes `chunk` fully, mapping failure to [`OrtRuntimeError::Io`] with `path`.
    fn write_all_chunk(&mut self, chunk: &[u8], path: &Path) -> Result<(), OrtRuntimeError>;
}

impl ChunkWrite for File {
    fn write_all_chunk(&mut self, chunk: &[u8], path: &Path) -> Result<(), OrtRuntimeError> {
        use std::io::Write;
        self.write_all(chunk).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.write_error", path = path.display(), err = err))
        })
    }
}

/// Flushes `file`, mapping failure to a typed error with context.
fn flush_file(file: &mut File, path: &Path) -> Result<(), OrtRuntimeError> {
    use std::io::Write;
    file.flush().map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = path.display(), err = err))
    })
}

/// Maps a `ureq` download error to a typed, user-facing [`OrtRuntimeError::Download`].
fn download_error(url: &str, err: ureq::Error) -> OrtRuntimeError {
    match err {
        ureq::Error::Status(status, response) => OrtRuntimeError::Download(tf!("onnx_runtime.download.http_error", status = status, url = url, response = response.status_text())),
        ureq::Error::Transport(transport) => OrtRuntimeError::Download(tf!("onnx_runtime.download.connection_error", url = url, transport = transport)),
    }
}

/// Extracts exactly the `wanted` members from `archive_path` into their mapped
/// destination paths.
///
/// `wanted` maps each archive-internal member path to its destination path (already
/// flattened to a controlled filename inside the cache directory, so archive-path
/// traversal is impossible). Only the listed members are unpacked (so, e.g., the
/// 300 MB Windows `.pdb` is never touched). Symlink and directory entries matching a
/// member path are skipped in favor of the real file. Every wanted member must be
/// present, or [`OrtRuntimeError::DylibNotFoundInArchive`] names the first missing one.
fn extract_wanted(
    archive_path: &Path,
    archive: manifest::ArchiveKind,
    wanted: &HashMap<String, PathBuf>,
) -> Result<(), OrtRuntimeError> {
    let mut found: HashSet<String> = HashSet::new();

    match archive {
        manifest::ArchiveKind::Zip => {
            let file = File::open(archive_path).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = archive_path.display(), err = err))
            })?;
            let mut zip = ZipArchive::new(file).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_read_error", err = err))
            })?;
            for index in 0..zip.len() {
                let mut file_in_zip = zip.by_index(index).map_err(|err| {
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_entry_error", index = index, err = err))
                })?;
                let name = file_in_zip.name().to_string();
                if let Some(out_path) = wanted.get(name.as_str()) {
                    if file_in_zip.is_dir() {
                        continue;
                    }
                    write_stream(&mut file_in_zip, out_path)?;
                    found.insert(name);
                }
            }
        }
        manifest::ArchiveKind::TarGz => {
            let file = File::open(archive_path).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = archive_path.display(), err = err))
            })?;
            extract_tar_members(GzDecoder::new(file), wanted, &mut found)?;
        }
        manifest::ArchiveKind::TarZst => {
            let file = File::open(archive_path).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = archive_path.display(), err = err))
            })?;
            let decoder = zstd::stream::read::Decoder::new(file).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zstd_open_error", err = err))
            })?;
            extract_tar_members(decoder, wanted, &mut found)?;
        }
    }

    // Every requested member must have been extracted; report the first miss.
    for member in wanted.keys() {
        if !found.contains(member) {
            return Err(OrtRuntimeError::DylibNotFoundInArchive(member.clone()));
        }
    }
    Ok(())
}

/// Extracts the wanted members from a tar stream `reader`, recording found keys.
///
/// Directory and symlink entries are skipped so only the real library files land
/// in the cache directory (the versioned `.so`/`.dylib` file, not its symlinks).
fn extract_tar_members<R: Read>(
    reader: R,
    wanted: &HashMap<String, PathBuf>,
    found: &mut HashSet<String>,
) -> Result<(), OrtRuntimeError> {
    let mut archive = TarArchive::new(reader);
    let entries = archive.entries().map_err(|err| {
        OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_read_error", err = err))
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|err| {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_entry_error", err = err))
        })?;
        let path = entry.path().map_err(|err| {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_invalid_path", err = err))
        })?;
        // Normalize separators and strip a leading `./`. GNU-tar-packed archives store
        // members as `./onnxruntime-.../lib/...` (the onnxruntime 1.27.0 macOS archives
        // do this; 1.20.1 did not), and the manifest `dylib_member`/`extra_members`
        // paths are written without the `./` prefix. Stripping it here lets one clean
        // manifest path match both archive styles.
        let normalized = path.to_string_lossy().replace('\\', "/");
        let key = normalized.strip_prefix("./").unwrap_or(&normalized);
        if let Some(out_path) = wanted.get(key) {
            // Only the real regular file is wanted; the archive's `.so`/`.dylib`
            // symlinks point at it and are intentionally ignored.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            write_stream(&mut entry, out_path)?;
            found.insert(key.to_string());
        }
    }
    Ok(())
}

/// Copies a readable archive member into `out_path`, creating parent dirs.
fn write_stream<R: Read>(reader: &mut R, out_path: &Path) -> Result<(), OrtRuntimeError> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_dir_error", parent = parent.display(), err = err))
        })?;
    }
    let mut output = File::create(out_path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_file_error", out_path = out_path.display(), err = err))
    })?;
    std::io::copy(reader, &mut output).map_err(|err| {
        OrtRuntimeError::Extraction(tf!("onnx_runtime.download.extract_element_error", out_path = out_path.display(), err = err))
    })?;
    Ok(())
}

/// The onnxruntime library filename to place and return for this build target.
///
/// The returned name is what [`resolve_or_download_ort_dylib`] both probes for and
/// writes the primary member to, so probe and extraction always agree. Linux and
/// macOS embed the version in the soname; Windows does not.
#[must_use]
fn expected_primary_dylib_filename(ort_version: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        // The Windows DLL name is unversioned; the version param is unused here.
        let _ = ort_version;
        "onnxruntime.dll".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        format!("libonnxruntime.{ort_version}.dylib")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        format!("libonnxruntime.so.{ort_version}")
    }
}

/// The archive filename (last URL path segment).
fn archive_file_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("onnxruntime-archive")
        .to_string()
}

/// Reports one progress snapshot to the caller's callback.
fn emit(
    progress: &mut dyn FnMut(OrtDownloadProgress),
    stage: OrtDownloadStage,
    downloaded: u64,
    total: Option<u64>,
) {
    progress(OrtDownloadProgress {
        downloaded,
        total,
        stage,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ort_dylib_dir_has_provider_and_version_tail() {
        let dir = ort_dylib_dir("cpu", "1.20.1");
        let tail: Vec<String> = dir
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // Reversed: [version, provider, "onnxruntime"].
        assert_eq!(tail, vec!["1.20.1", "cpu", "onnxruntime"]);
    }

    #[test]
    fn expected_primary_dylib_filename_matches_linux_soname() {
        // The primary check/test target is linux: versioned `.so` soname.
        assert_eq!(
            expected_primary_dylib_filename("1.20.1"),
            "libonnxruntime.so.1.20.1"
        );
    }

    #[test]
    fn archive_file_name_takes_last_segment() {
        assert_eq!(
            archive_file_name("https://example/onnxruntime-linux-x64-1.20.1.tgz"),
            "onnxruntime-linux-x64-1.20.1.tgz"
        );
        assert_eq!(archive_file_name("https://example/"), "onnxruntime-archive");
    }

    #[test]
    fn member_file_name_flattens_and_rejects_empty() {
        assert_eq!(
            member_file_name("onnxruntime/capi/dxil.dll").expect("has filename"),
            PathBuf::from("dxil.dll")
        );
        // A root path has no filename component (a trailing slash, by contrast, is
        // stripped by `Path`, so `"bin/x64/"` would still yield `"x64"`).
        assert!(matches!(
            member_file_name("/"),
            Err(OrtRuntimeError::Extraction(_))
        ));
    }

    #[test]
    fn expected_member_paths_covers_primary_extras_and_additional_sources() {
        // Deserialize a multi-source entry and confirm the fast-path probe demands
        // every library filename (primary + extras + additional-source members).
        let json = r#"{
            "os": "windows", "arch": "x86_64", "provider": "example", "build": "example",
            "version": "9.9.9",
            "url": "https://example/primary.zip",
            "sha256": null,
            "archive": "zip",
            "dylib_member": "lib/primary.dll",
            "extra_members": ["lib/side.dll"],
            "additional_sources": [
                { "url": "https://example/extra.zip", "sha256": null,
                  "archive": "zip", "members": ["bin/x64/dxil.dll"] }
            ]
        }"#;
        let entry: manifest::ManifestEntry =
            serde_json::from_str(json).expect("entry parses");
        let dir = Path::new("/cache");
        let paths = expected_member_paths(dir, "primary.dll", &entry).expect("paths");
        let names: HashSet<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            HashSet::from([
                "primary.dll".to_string(),
                "side.dll".to_string(),
                "dxil.dll".to_string(),
            ])
        );
        assert!(paths.iter().all(|p| p.starts_with(dir)));
    }

    #[test]
    fn extract_wanted_pulls_only_listed_members_from_a_zip() {
        use std::io::Write;
        use zip::write::{SimpleFileOptions, ZipWriter};

        let base = std::env::temp_dir().join(format!("ort_extract_test_{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("create test dir");
        let zip_path = base.join("fixture.zip");

        // Build a zip with three members; only two are wanted.
        {
            let file = File::create(&zip_path).expect("create zip");
            let mut zip = ZipWriter::new(file);
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, body) in [
                ("capi/onnxruntime.dll", b"primary".as_slice()),
                ("capi/dxil.dll", b"dxil".as_slice()),
                ("capi/ignored.txt", b"skip".as_slice()),
            ] {
                zip.start_file(name, opts).expect("start file");
                zip.write_all(body).expect("write member");
            }
            zip.finish().expect("finish zip");
        }

        let out_dir = base.join("out");
        std::fs::create_dir_all(&out_dir).expect("create out dir");
        let mut wanted: HashMap<String, PathBuf> = HashMap::new();
        wanted.insert(
            "capi/onnxruntime.dll".to_string(),
            out_dir.join("onnxruntime.dll"),
        );
        wanted.insert("capi/dxil.dll".to_string(), out_dir.join("dxil.dll"));

        extract_wanted(&zip_path, manifest::ArchiveKind::Zip, &wanted).expect("extract");

        assert_eq!(std::fs::read(out_dir.join("onnxruntime.dll")).unwrap(), b"primary");
        assert_eq!(std::fs::read(out_dir.join("dxil.dll")).unwrap(), b"dxil");
        // The unlisted member must NOT be extracted.
        assert!(!out_dir.join("ignored.txt").exists());

        // A wanted member absent from the archive is a hard error.
        let mut missing: HashMap<String, PathBuf> = HashMap::new();
        missing.insert("capi/absent.dll".to_string(), out_dir.join("absent.dll"));
        assert!(matches!(
            extract_wanted(&zip_path, manifest::ArchiveKind::Zip, &missing),
            Err(OrtRuntimeError::DylibNotFoundInArchive(_))
        ));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn missing_build_entry_yields_typed_error_without_network() {
        // The `directml` build has no Linux archive (Windows-only); on the Linux test
        // host its resolution must fail with a typed no-entry error before any network
        // access, and must not panic. (The cache dir for `directml` does not exist in a
        // clean tree, so the probe misses and the lookup runs.) The CUDA builds resolve
        // on Windows/Linux — asserted in the manifest tests.
        let mut progress = |_p: OrtDownloadProgress| {};
        let result = resolve_or_download_ort_dylib("directml", &mut progress);
        assert!(matches!(
            &result,
            Err(OrtRuntimeError::NoManifestEntry { build, .. }) if build == "directml"
        ));
    }
}
