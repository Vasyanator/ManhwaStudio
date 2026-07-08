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
Downloads and disk I/O are blocking; `resolve_or_download_ort_dylib` MUST run on a
worker thread, never on the GUI thread. This module owns dylib RESOLUTION only; it
never links, initializes, or runs onnxruntime — that is `ms-onnx`'s job, and it
consumes the path returned here. Storage lives under `config::data_dir()`.
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

mod manifest;

pub use manifest::ORT_VERSION;

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
/// Every variant carries a user-facing Russian message (matching `ms-onnx`'s
/// style) via `#[error]`; the wrapped strings add diagnostic context for logs.
#[derive(Debug, thiserror::Error)]
pub enum OrtRuntimeError {
    /// No manifest row exists for this platform/provider (e.g. an unshipped GPU
    /// provider). This is a hard error, never a silent CPU substitution.
    #[error(
        "Для платформы {os}/{arch} и провайдера «{provider}» нет записи о библиотеке ONNX Runtime."
    )]
    NoManifestEntry {
        /// Normalized OS key that was looked up.
        os: &'static str,
        /// Normalized architecture key that was looked up.
        arch: &'static str,
        /// Provider id that was looked up.
        provider: &'static str,
    },

    /// The archive could not be downloaded (network/transport/HTTP status).
    #[error("Не удалось скачать библиотеку ONNX Runtime. {0}")]
    Download(String),

    /// The downloaded archive's SHA256 did not match the pinned value; the file
    /// is deleted and resolution is aborted.
    #[error("Контрольная сумма архива ONNX Runtime не совпала (ожидалось {expected}, получено {actual}).")]
    ChecksumMismatch {
        /// Expected lowercase hex digest from the manifest.
        expected: String,
        /// Actual lowercase hex digest of the downloaded archive.
        actual: String,
    },

    /// The archive could not be opened or unpacked.
    #[error("Не удалось распаковать архив ONNX Runtime. {0}")]
    Extraction(String),

    /// The expected onnxruntime library was not found inside the archive.
    #[error("Библиотека ONNX Runtime не найдена в архиве (ожидался элемент «{0}»).")]
    DylibNotFoundInArchive(String),

    /// A filesystem operation (create dir, write, rename, remove) failed.
    #[error("Ошибка файловой системы при подготовке ONNX Runtime. {0}")]
    Io(String),
}

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

/// Portable per-provider, per-version cache directory for the onnxruntime library.
///
/// Pure path construction: `data_dir()/onnxruntime/<provider_id>/<ort_version>/`.
/// Creates nothing on disk.
#[must_use]
pub fn ort_dylib_dir(provider_id: &str, ort_version: &str) -> PathBuf {
    config::data_dir()
        .join("onnxruntime")
        .join(provider_id)
        .join(ort_version)
}

/// Resolves the onnxruntime library path for `provider`, downloading it on first use.
///
/// Probes [`ort_dylib_dir`] for an already-extracted library and returns its path
/// immediately (no network) if present. Otherwise it looks up the manifest entry
/// for the current platform/provider, downloads the official archive, verifies its
/// SHA256 (mismatch is a hard error — the bad file is deleted), extracts the
/// loadable library into the cache directory, and returns the primary dylib path.
///
/// `progress` is invoked with stage/byte updates (throttled during download). It
/// is called on the calling (worker) thread.
///
/// # Threading
/// Performs blocking network and disk I/O. It MUST be called from a worker thread,
/// never from the GUI thread.
///
/// # Errors
/// - [`OrtRuntimeError::NoManifestEntry`] if no archive is pinned for this platform/provider.
/// - [`OrtRuntimeError::Download`] on network/HTTP failure.
/// - [`OrtRuntimeError::ChecksumMismatch`] if the archive fails SHA256 verification.
/// - [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::DylibNotFoundInArchive`] on unpack failure.
/// - [`OrtRuntimeError::Io`] on filesystem failure.
pub fn resolve_or_download_ort_dylib(
    provider: ms_onnx::ExecutionProvider,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PathBuf, OrtRuntimeError> {
    let provider_id = provider.id();
    let dir = ort_dylib_dir(provider_id, ORT_VERSION);
    let primary_name = expected_primary_dylib_filename(ORT_VERSION);
    let primary_path = dir.join(&primary_name);

    emit(progress, OrtDownloadStage::Probing, 0, None);
    if primary_path.is_file() {
        runtime_log::log_info(format!(
            "[onnx-runtime] found cached library for provider '{provider_id}' at {}",
            primary_path.display()
        ));
        emit(progress, OrtDownloadStage::Done, 0, None);
        return Ok(primary_path);
    }

    let (os, arch) = manifest::current_platform();
    let entry = manifest::lookup(os, arch, provider_id).ok_or(OrtRuntimeError::NoManifestEntry {
        os,
        arch,
        provider: provider_id,
    })?;

    fs::create_dir_all(&dir).map_err(|err| {
        OrtRuntimeError::Io(format!(
            "не удалось создать каталог '{}': {err}",
            dir.display()
        ))
    })?;
    let download_dir = dir.join(".download");
    fs::create_dir_all(&download_dir).map_err(|err| {
        OrtRuntimeError::Io(format!(
            "не удалось создать каталог загрузки '{}': {err}",
            download_dir.display()
        ))
    })?;

    let archive_name = archive_file_name(&entry.url);
    let archive_path = download_dir.join(&archive_name);
    let partial_path = download_dir.join(format!("{archive_name}.part"));

    runtime_log::log_info(format!(
        "[onnx-runtime] downloading ONNX Runtime {ORT_VERSION} for provider '{provider_id}' from {}",
        entry.url
    ));
    download_to(&entry.url, &partial_path, &archive_path, progress)?;

    emit(progress, OrtDownloadStage::Verifying, 0, None);
    verify_or_report_hash(entry, &archive_path)?;

    emit(progress, OrtDownloadStage::Extracting, 0, None);
    extract_members(&archive_path, entry, &dir, &primary_name)?;

    if !primary_path.is_file() {
        return Err(OrtRuntimeError::DylibNotFoundInArchive(
            entry.dylib_member.clone(),
        ));
    }

    // The archive is no longer needed once the library is extracted; a cleanup
    // failure is non-fatal (the cache directory still holds a working library).
    if let Err(err) = fs::remove_file(&archive_path) {
        runtime_log::log_warn(format!(
            "[onnx-runtime] failed to remove archive '{}': {err}",
            archive_path.display()
        ));
    }

    runtime_log::log_info(format!(
        "[onnx-runtime] ready for provider '{provider_id}': {}",
        primary_path.display()
    ));
    emit(progress, OrtDownloadStage::Done, 0, None);
    Ok(primary_path)
}

/// Verifies the downloaded archive against its pinned hash, or — when no hash is
/// pinned — logs a clear "integrity NOT verified" warning plus the actual digest
/// so a maintainer can pin it. A mismatch deletes the file and aborts.
fn verify_or_report_hash(
    entry: &manifest::ManifestEntry,
    archive_path: &Path,
) -> Result<(), OrtRuntimeError> {
    match &entry.sha256 {
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
                "[onnx-runtime] integrity NOT verified for '{}' (no pinned SHA256). \
                 Downloaded archive SHA256 = {actual}. Pin this value in \
                 src/onnx_runtime/ort_manifest.json to enable verification.",
                entry.url
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
            OrtRuntimeError::Io(format!(
                "не удалось удалить прежний временный файл '{}': {err}",
                partial_path.display()
            ))
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
        OrtRuntimeError::Io(format!(
            "не удалось создать файл архива '{}': {err}",
            partial_path.display()
        ))
    })?;

    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
    let mut downloaded: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let read = reader.read(&mut buffer).map_err(|err| {
            OrtRuntimeError::Download(format!("Ошибка чтения данных с '{url}': {err}"))
        })?;
        if read == 0 {
            break;
        }
        output.write_all_chunk(&buffer[..read], partial_path)?;
        let read_u64 = u64::try_from(read).map_err(|err| {
            OrtRuntimeError::Download(format!("некорректный размер блока {read}: {err}"))
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
        OrtRuntimeError::Io(format!(
            "не удалось переместить архив '{}' в '{}': {err}",
            partial_path.display(),
            final_path.display()
        ))
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
            OrtRuntimeError::Io(format!(
                "не удалось записать данные в '{}': {err}",
                path.display()
            ))
        })
    }
}

/// Flushes `file`, mapping failure to a typed error with context.
fn flush_file(file: &mut File, path: &Path) -> Result<(), OrtRuntimeError> {
    use std::io::Write;
    file.flush().map_err(|err| {
        OrtRuntimeError::Io(format!(
            "не удалось сохранить файл '{}': {err}",
            path.display()
        ))
    })
}

/// Maps a `ureq` download error to a typed, user-facing [`OrtRuntimeError::Download`].
fn download_error(url: &str, err: ureq::Error) -> OrtRuntimeError {
    match err {
        ureq::Error::Status(status, response) => OrtRuntimeError::Download(format!(
            "Сервер вернул HTTP {status} для '{url}': {}",
            response.status_text()
        )),
        ureq::Error::Transport(transport) => OrtRuntimeError::Download(format!(
            "Проверьте подключение к интернету. Ошибка соединения с '{url}': {transport}"
        )),
    }
}

/// Extracts the primary library (renamed to `primary_out_name`) and every
/// `extra_members` library from `archive_path` into `out_dir`.
///
/// Only the explicitly listed archive members are unpacked (so, e.g., the 300 MB
/// Windows `.pdb` is never touched). Members are flattened to a controlled
/// filename inside `out_dir`, so archive-path traversal is impossible; symlink and
/// directory entries matching a member path are skipped in favor of the real file.
fn extract_members(
    archive_path: &Path,
    entry: &manifest::ManifestEntry,
    out_dir: &Path,
    primary_out_name: &str,
) -> Result<(), OrtRuntimeError> {
    // Map archive-internal member path -> destination path in the cache dir.
    let mut wanted: HashMap<String, PathBuf> = HashMap::new();
    wanted.insert(entry.dylib_member.clone(), out_dir.join(primary_out_name));
    for member in &entry.extra_members {
        let file_name = Path::new(member).file_name().ok_or_else(|| {
            OrtRuntimeError::Extraction(format!("некорректный путь элемента архива '{member}'"))
        })?;
        wanted.insert(member.clone(), out_dir.join(file_name));
    }

    let mut found: HashSet<String> = HashSet::new();

    match entry.archive {
        manifest::ArchiveKind::Zip => {
            let file = File::open(archive_path).map_err(|err| {
                OrtRuntimeError::Extraction(format!(
                    "не удалось открыть '{}': {err}",
                    archive_path.display()
                ))
            })?;
            let mut zip = ZipArchive::new(file).map_err(|err| {
                OrtRuntimeError::Extraction(format!("не удалось прочитать ZIP-архив: {err}"))
            })?;
            for index in 0..zip.len() {
                let mut file_in_zip = zip.by_index(index).map_err(|err| {
                    OrtRuntimeError::Extraction(format!("ошибка чтения ZIP-элемента {index}: {err}"))
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
                OrtRuntimeError::Extraction(format!(
                    "не удалось открыть '{}': {err}",
                    archive_path.display()
                ))
            })?;
            extract_tar_members(GzDecoder::new(file), &wanted, &mut found)?;
        }
        manifest::ArchiveKind::TarZst => {
            let file = File::open(archive_path).map_err(|err| {
                OrtRuntimeError::Extraction(format!(
                    "не удалось открыть '{}': {err}",
                    archive_path.display()
                ))
            })?;
            let decoder = zstd::stream::read::Decoder::new(file).map_err(|err| {
                OrtRuntimeError::Extraction(format!("не удалось открыть zstd-декодер: {err}"))
            })?;
            extract_tar_members(decoder, &wanted, &mut found)?;
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
        OrtRuntimeError::Extraction(format!("не удалось прочитать tar-архив: {err}"))
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|err| {
            OrtRuntimeError::Extraction(format!("ошибка чтения tar-элемента: {err}"))
        })?;
        let path = entry.path().map_err(|err| {
            OrtRuntimeError::Extraction(format!("некорректный путь tar-элемента: {err}"))
        })?;
        let key = path.to_string_lossy().replace('\\', "/");
        if let Some(out_path) = wanted.get(key.as_str()) {
            // Only the real regular file is wanted; the archive's `.so`/`.dylib`
            // symlinks point at it and are intentionally ignored.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            write_stream(&mut entry, out_path)?;
            found.insert(key);
        }
    }
    Ok(())
}

/// Copies a readable archive member into `out_path`, creating parent dirs.
fn write_stream<R: Read>(reader: &mut R, out_path: &Path) -> Result<(), OrtRuntimeError> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            OrtRuntimeError::Io(format!(
                "не удалось создать каталог '{}': {err}",
                parent.display()
            ))
        })?;
    }
    let mut output = File::create(out_path).map_err(|err| {
        OrtRuntimeError::Io(format!(
            "не удалось создать файл '{}': {err}",
            out_path.display()
        ))
    })?;
    std::io::copy(reader, &mut output).map_err(|err| {
        OrtRuntimeError::Extraction(format!(
            "не удалось распаковать элемент в '{}': {err}",
            out_path.display()
        ))
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
    fn missing_provider_entry_yields_typed_error_without_network() {
        // DirectML has no Linux archive (it is a Windows-only provider); on the
        // Linux test host its resolution must fail with a typed no-entry error
        // before any network access, and must not panic. (The cache dir for
        // `directml` does not exist in a clean tree, so the probe misses and the
        // lookup runs.) CUDA now resolves on Windows/Linux — that entry is asserted
        // in the manifest tests.
        let mut progress = |_p: OrtDownloadProgress| {};
        let result =
            resolve_or_download_ort_dylib(ms_onnx::ExecutionProvider::DirectMl, &mut progress);
        assert!(matches!(
            result,
            Err(OrtRuntimeError::NoManifestEntry {
                provider: "directml",
                ..
            })
        ));
    }
}
