/*
File: crates/ms-onnx-runtime/src/lib.rs

Purpose:
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform/provider so the `ms-onnx` crate can dlopen it. On first
use it probes a portable, per-build cache directory; if the library is absent it
downloads the pinned official release archive through `ms-download` (resumable `.part`,
retry with backoff, cancel, exact size + SHA256), extracts the loadable libraries
atomically, and returns its on-disk path. The app starts fine without the library
present — resolution happens only when a caller asks for it.

Key types:
- OrtRuntimeError    : typed failure surface for probe/download/verify/extract and the
  bundled-library lookup, plus `Unavailable` where the platform has no native runtime (wasm)
  and `Cancelled`
- OrtDownloadStage   : coarse progress stage (Probing/WaitingForOtherDownload/Downloading/
  Retrying/Verifying/Extracting/Done)
- OrtDownloadProgress: byte counters + stage reported through the callback

Key functions:
- ort_dylib_dir                : pure per-provider/version cache directory
- resolve_or_download_ort_dylib: probe or download+verify+extract (download rows), or
  locate the library the app package ships (bundled rows); worker-thread only
- ort_build_installed          : the one "is this build ready without a download" answer
  (the resolver's fast-path rule; used by the settings panel's presence probe)
- resolve_download_entry       : the download-row half over an injected fetcher (tests)
- fetch_source / claim_part    : one archive via `ms_download::fetch_verified`, with the
  `.part.id` identity that keeps a `.part` from resuming another row's bytes
- extract_wanted               : members staged to `<dest>.tmp` + fsync, renamed only when
  all were found, primary library last
- build_source_kind_here / ort_downloads_offered_here: download vs bundled, for the UI

Notes:
The onnxruntime version is per manifest ENTRY, not global: the resolved cache dir
and versioned library name come from the looked-up entry's `version` (WebGPU on
1.27.0 coexists with the 1.20.1 providers). An entry may pull extra libraries from
`additional_sources` (extra archives) into the same cache directory; the primary
and every additional source share one download+verify+extract path. One resolution per
build runs at a time in the process (busy id `ort:<build>:<version>`), so the settings
panel's auto-download and a native OCR load never write the same files. Downloads and
disk I/O are blocking; `resolve_or_download_ort_dylib` MUST run on a worker thread,
never on the GUI thread. This module owns dylib RESOLUTION only; it never links,
initializes, or runs onnxruntime — that is `ms-onnx`'s job, and it consumes the
path returned here. A BUNDLED build (Android: the `onnxruntime-android` AAR merged into the
APK) has no recipe: resolution returns `<package native library dir>/<library>` after an
existence check, with no network and no cache directory. Storage lives under `ms_config::data_dir()`. The binary names this crate directly as
`ms_onnx_runtime::`.
*/

#![warn(clippy::all)]

// `t!` / `tf!` for the localized failure strings of every stage.
#[macro_use]
extern crate ms_i18n;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use ms_download::busy::BusyGuard;
use ms_download::resumable::{RangeFetcher, ResumableFetch, RetryPolicy, StreamEvent, UreqFetcher};
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, fetch_verified};
use tar::Archive as TarArchive;
use zip::ZipArchive;

use ms_config as config;
use ms_log::runtime_log;

pub mod builds;
// `allow(dead_code)`: a few manifest items are kept for completeness and are not yet
// consumed inside this crate (the `Manifest::ort_version` field, the generic `lookup`
// helper). The binary carried the same allow on the module
// declaration before the extraction; it is narrowed to `manifest` here.
#[allow(dead_code)]
mod manifest;

// Build-catalog + manifest surface consumed by `native_runtime` (the guard-scope key
// and the resolve call site) and, in the next task, by the AI backend panel's build
// picker. `build_version` is the build-keyed primary; `provider_version` is the
// back-compat shim mapping a provider to its default build. The allow suppresses the
// unused-import warning only until the build-selection UI consumes `build_version`
// directly.
#[allow(unused_imports)]
pub use manifest::{ORT_VERSION, OrtBuildSourceKind, build_shipped_here, build_source_kind_here, build_version, ort_downloads_offered_here, provider_version};

/// Minimum number of bytes between throttled progress reports (transfer and hashing).
const PROGRESS_EMIT_INTERVAL_BYTES: u64 = 512 * 1024;

/// Log prefix of this crate (also the `ms_download` engine's tag for its retry lines).
const LOG_TAG: &str = "[onnx-runtime]";

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

    /// The archive could not be downloaded: a permanent HTTP failure, retries exhausted
    /// after transient failures, or a size that contradicts the pin. A partial `.part` is
    /// kept for the next attempt unless its bytes were proven wrong.
    Download(String),

    /// The caller's cancel flag stopped the busy wait, the transfer, a retry wait or the
    /// hash pass; the staged `.part` is kept so the next call resumes it.
    Cancelled,

    /// The downloaded archive's SHA256 did not match the pinned value; the `.part` is
    /// deleted and resolution is aborted.
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

    /// The platform has no native ONNX Runtime (`PlatformFeature::NativeOnnxRuntime`);
    /// returned before any manifest lookup, disk or network access.
    Unavailable(ms_platform::Unavailable),

    /// A bundled build's library folder (the app package's native library directory)
    /// could not be determined. Carries the technical detail (JNI failure text).
    NativeLibraryDir(String),

    /// A bundled build's library is not in the app package's native library directory
    /// (a broken or foreign APK). Never answered with a download.
    BundledLibraryMissing(PathBuf),
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
            Self::Cancelled => f.write_str(t!("onnx_runtime.error.cancelled")),
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
            // Already localized by `ms-platform`.
            Self::Unavailable(err) => std::fmt::Display::fmt(err, f),
            Self::NativeLibraryDir(detail) => {
                f.write_str(&tf!("onnx_runtime.error.native_library_dir", detail = detail))
            }
            Self::BundledLibraryMissing(path) => f.write_str(&tf!(
                "onnx_runtime.error.bundled_library_missing",
                path = path.display()
            )),
        }
    }
}

// No variant wraps an underlying cause (Strings, plain fields, or the self-contained
// `Unavailable` whose `Display` this type forwards), so the default `source()` (None)
// is correct and no message is printed twice in a chain; the impl is required so this type can be an
// `Error` source for `NativeRuntimeError`.
impl std::error::Error for OrtRuntimeError {}

/// Coarse stage of an ONNX Runtime resolution, reported via the progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtDownloadStage {
    /// Checking the on-disk cache before any network access.
    Probing,
    /// Another task of this process (the settings panel or a native OCR load) is
    /// preparing the same build; waiting for it, then re-probing the cache.
    WaitingForOtherDownload,
    /// Streaming the release archive to disk.
    Downloading,
    /// A transient network failure: waiting `delay_secs` (whole seconds, rounded up;
    /// re-reported as it counts down, last with `0`) before retry `attempt` of
    /// `max_attempts` (consecutive retries without progress before giving up).
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64 },
    /// Hashing an already staged prefix or archive against the pinned SHA256.
    Verifying,
    /// Unpacking the library out of the archive.
    Extracting,
    /// Resolution finished; the dylib path is available.
    Done,
}

/// Progress snapshot passed to the caller's callback during resolution.
///
/// `downloaded`/`total` are meaningful during [`OrtDownloadStage::Downloading`] and
/// [`OrtDownloadStage::Retrying`] (bytes staged / pinned archive size) and
/// [`OrtDownloadStage::Verifying`] (bytes hashed / size of the hashed file); other
/// stages report `downloaded = 0`, `total = None`.
#[derive(Debug, Clone, Copy)]
pub struct OrtDownloadProgress {
    /// Bytes staged (or hashed) so far.
    pub downloaded: u64,
    /// Total size of the current operation, if known.
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
/// `"cuda13"`, `"openvino"`). A BUNDLED row (Android) resolves to the library inside the
/// app package's native library directory after an existence check — no network, no
/// cache, progress `Probing` then `Done`. For a download row it probes [`ort_dylib_dir`]
/// and returns at once (no network) when EVERY expected library is present. Otherwise it
/// takes the per-build busy id `ort:<build>:<version>` (waiting, cancellable, reported as
/// [`OrtDownloadStage::WaitingForOtherDownload`], so the settings panel and the native
/// runtime never stage one build twice), re-probes, downloads each pinned archive into
/// `.download/<archive>.part` through `ms_download::fetch_verified` (resume across runs,
/// transient failures retried with backoff and reported as
/// [`OrtDownloadStage::Retrying`], exact size + SHA256 verified), extracts the needed
/// members atomically and returns the primary dylib path.
///
/// `cancel` is polled during the busy wait, the transfer, the backoff and the hash pass;
/// a cancelled download keeps its `.part` for the next call. `progress` is invoked on the
/// calling (worker) thread, throttled during transfer and hashing.
///
/// # Threading
/// Performs blocking network and disk I/O. It MUST be called from a worker thread,
/// never from the GUI thread.
///
/// # Errors
/// - [`OrtRuntimeError::Unavailable`] if the platform has no native ONNX Runtime
///   (checked first, before the manifest, the disk or the network).
/// - [`OrtRuntimeError::NoManifestEntry`] if no archive is pinned for this platform/build.
/// - [`OrtRuntimeError::Cancelled`] when `cancel` was set (staged bytes are kept).
/// - [`OrtRuntimeError::Download`] on a permanent network/HTTP failure, exhausted
///   retries or a size mismatch.
/// - [`OrtRuntimeError::ChecksumMismatch`] if the archive fails SHA256 verification
///   (its `.part` is deleted).
/// - [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::DylibNotFoundInArchive`] on unpack failure.
/// - [`OrtRuntimeError::Io`] on filesystem failure.
/// - [`OrtRuntimeError::NativeLibraryDir`] / [`OrtRuntimeError::BundledLibraryMissing`] for a
///   bundled build whose package folder is unknown or lacks the library.
pub fn resolve_or_download_ort_dylib(
    build: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PathBuf, OrtRuntimeError> {
    // The capability table is the one owner of "is there a native runtime here"; refuse
    // before touching the manifest, the cache or the network.
    ms_platform::PlatformFeature::NativeOnnxRuntime.require().map_err(OrtRuntimeError::Unavailable)?;
    // The version is per-entry, so the manifest lookup must run BEFORE computing the
    // cache dir and probing: each build resolves under its own version. The lookup is
    // an in-memory scan (no network), and a cached library can only exist after a
    // successful download (which requires an entry), so probing after the lookup never
    // regresses the offline fast path.
    let (os, arch) = manifest::current_platform();
    let source = manifest::lookup_build(os, arch, build).ok_or_else(|| {
        OrtRuntimeError::NoManifestEntry {
            os,
            arch,
            build: build.to_string(),
        }
    })?;
    let entry = match source {
        manifest::BuildSource::Download(entry) => entry,
        manifest::BuildSource::Bundled(entry) => {
            emit(progress, OrtDownloadStage::Probing, 0, None);
            let path = bundled_library_path(package_native_library_dir()?, entry)?;
            runtime_log::log_info(format!(
                "[onnx-runtime] using bundled ONNX Runtime {} for build '{build}' at {}",
                entry.version,
                path.display()
            ));
            emit(progress, OrtDownloadStage::Done, 0, None);
            return Ok(path);
        }
    };

    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    let env = DownloadEnv { fetcher: &fetcher, retry: &retry, cancel };
    // Cache is scoped by BUILD SLUG so builds sharing a provider (cuda12 / cuda13) do
    // not collide.
    resolve_download_entry(&env, build, entry, &ort_dylib_dir(build, &entry.version), progress)
}

/// The HTTP seam, retry schedule and cancel flag one resolution downloads with
/// (production: `UreqFetcher` + `RetryPolicy::standard`; tests: a scripted fetcher).
struct DownloadEnv<'a> {
    fetcher: &'a dyn RangeFetcher,
    retry: &'a RetryPolicy<'a>,
    cancel: &'a AtomicBool,
}

/// One pinned archive of a manifest row (the primary source or an additional one).
#[derive(Debug, Clone, Copy)]
struct SourceRecipe<'a> {
    url: &'a str,
    size: u64,
    sha256: &'a str,
    archive: manifest::ArchiveKind,
}

/// Download-row half of [`resolve_or_download_ort_dylib`] for `entry` into the cache
/// directory `dir`: probe (completion marker + member sizes), per-build busy wait,
/// re-probe, marker invalidated, then every source fetched and extracted (additional
/// sources first, the primary archive LAST, so the primary library is the last member
/// published), and finally the completion marker written atomically.
///
/// # Errors
/// As [`resolve_or_download_ort_dylib`] for download rows.
fn resolve_download_entry(
    env: &DownloadEnv<'_>,
    build: &str,
    entry: &manifest::ManifestEntry,
    dir: &Path,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PathBuf, OrtRuntimeError> {
    let provider_id = entry.provider.as_str();
    let version = entry.version.as_str();
    let primary_name = expected_primary_dylib_filename(version);
    let primary_path = dir.join(&primary_name);
    let expected_members = expected_member_paths(dir, &primary_name, entry)?;
    let recipe = cache_recipe(build, entry);

    emit(progress, OrtDownloadStage::Probing, 0, None);
    // Fast path: the one "installed" rule (`installed_primary`, also behind the public
    // `ort_build_installed`); anything short of a complete cache is rebuilt below.
    if installed_primary(build, entry, dir)?.is_some() {
        runtime_log::log_info(format!(
            "[onnx-runtime] found cached library for build '{build}' (provider '{provider_id}') at {}",
            primary_path.display()
        ));
        emit(progress, OrtDownloadStage::Done, 0, None);
        return Ok(primary_path);
    }

    // One resolution of this build per process: the settings panel's auto-download and
    // a native OCR load may ask for the same build at once, and two writers of one
    // `.part` / one set of extracted libraries would corrupt each other. The second
    // caller waits, then normally finds the cache complete below.
    let busy_key = format!("ort:{build}:{version}");
    let _busy = BusyGuard::acquire_waiting(&busy_key, env.cancel, env.retry, &mut || {
        runtime_log::log_info(format!("[onnx-runtime] build '{build}' {version} is being prepared by another task; waiting"));
        emit(progress, OrtDownloadStage::WaitingForOtherDownload, 0, None);
    })
    .map_err(|err| map_download_error(&entry.url, err))?;
    if installed_primary(build, entry, dir)?.is_some() {
        runtime_log::log_info(format!(
            "[onnx-runtime] build '{build}' was prepared by another task: {}",
            primary_path.display()
        ));
        emit(progress, OrtDownloadStage::Done, 0, None);
        return Ok(primary_path);
    }

    fs::create_dir_all(dir).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.create_dir_error", dir = dir.display(), err = err))
    })?;
    let download_dir = dir.join(".download");
    fs::create_dir_all(&download_dir).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.create_download_dir_error", download_dir = download_dir.display(), err = err))
    })?;
    // The cache is about to be (re)published member by member: drop the marker durably
    // FIRST, so a crash between two member renames leaves no marker and the next call
    // repairs the set instead of trusting a mixed generation.
    invalidate_cache_marker(dir)?;
    let mut published = Vec::with_capacity(expected_members.len());

    // Additional sources: extra archives (e.g. sidecar runtime DLLs) extracted into the
    // SAME cache directory via the exact same fetch path as the primary source.
    for source in &entry.additional_sources {
        let mut wanted = Vec::with_capacity(source.members.len());
        add_flattened_members(&mut wanted, &source.members, dir)?;
        runtime_log::log_info(format!(
            "[onnx-runtime] downloading additional source for build '{build}' from {}",
            source.url
        ));
        let recipe = SourceRecipe { url: &source.url, size: source.size, sha256: &source.sha256, archive: source.archive };
        published.extend(fetch_source(env, recipe, &wanted, &download_dir, build, progress)?);
    }

    // Primary source: the onnxruntime archive itself. Its `dylib_member` is renamed to
    // the version-scoped `primary_name` and listed LAST, so it is published after every
    // extra library of the archive; `extra_members` keep their filenames.
    let mut primary_wanted = Vec::with_capacity(entry.extra_members.len().saturating_add(1));
    add_flattened_members(&mut primary_wanted, &entry.extra_members, dir)?;
    primary_wanted.push((entry.dylib_member.clone(), primary_path.clone()));
    runtime_log::log_info(format!(
        "[onnx-runtime] downloading ONNX Runtime {version} for build '{build}' (provider '{provider_id}') from {}",
        entry.url
    ));
    let source = SourceRecipe { url: &entry.url, size: entry.size, sha256: &entry.sha256, archive: entry.archive };
    published.extend(fetch_source(env, source, &primary_wanted, &download_dir, build, progress)?);
    // Only now, with every member renamed into place, does the cache become trusted.
    write_cache_marker(dir, &cache_marker_text(&recipe, &published))?;

    runtime_log::log_info(format!(
        "[onnx-runtime] ready for build '{build}' (provider '{provider_id}'): {}",
        primary_path.display()
    ));
    emit(progress, OrtDownloadStage::Done, 0, None);
    Ok(primary_path)
}

/// Whether build `build` is ready to load on the current platform with no download: a
/// download row whose cache passes the same check as the resolver's fast path
/// ([`installed_primary`]: completion marker for this exact manifest row + every member
/// at its recorded size), or a bundled row whose library is in the app package.
///
/// The ONE answer to "is this build installed" (the settings panel's presence probe uses
/// it). No hashing and no network: a marker read plus one stat per member (and, for a
/// bundled row on Android, the JNI library-folder lookup) — call it off the GUI thread.
/// Every failure (unavailable platform, no manifest row, unreadable cache) is `false`.
#[must_use]
pub fn ort_build_installed(build: &str) -> bool {
    if !ms_platform::PlatformFeature::NativeOnnxRuntime.is_available() {
        return false;
    }
    let (os, arch) = manifest::current_platform();
    match manifest::lookup_build(os, arch, build) {
        None => false,
        Some(manifest::BuildSource::Bundled(entry)) => package_native_library_dir().is_ok_and(|dir| dir.join(&entry.library).is_file()),
        Some(manifest::BuildSource::Download(entry)) => match installed_primary(build, entry, &ort_dylib_dir(build, &entry.version)) {
            Ok(primary) => primary.is_some(),
            Err(err) => {
                runtime_log::log_warn(format!("{LOG_TAG} cannot check whether build '{build}' is installed: {err:?}"));
                false
            }
        },
    }
}

/// The primary library path of `entry` in cache directory `dir` when the cache is
/// complete for it, else `None`: the completion marker records EXACTLY this manifest row
/// (build, version, every source's url + sha256) and every expected member (primary +
/// extras + additional-source members) is on disk at the recorded size. A partial,
/// truncated, mixed-generation or foreign cache is `None` (the resolver rebuilds it).
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if a manifest member path has no file name.
fn installed_primary(build: &str, entry: &manifest::ManifestEntry, dir: &Path) -> Result<Option<PathBuf>, OrtRuntimeError> {
    let primary_name = expected_primary_dylib_filename(&entry.version);
    let expected_members = expected_member_paths(dir, &primary_name, entry)?;
    Ok(cache_complete(dir, &cache_recipe(build, entry), &expected_members).then(|| dir.join(primary_name)))
}

/// The app package's native library directory, where bundled libraries live.
///
/// Android: `ApplicationInfo.nativeLibraryDir` (JNI; worker thread only, which every
/// caller of the resolver already is).
///
/// # Errors
/// [`OrtRuntimeError::NativeLibraryDir`] when the JVM cannot answer.
#[cfg(target_os = "android")]
fn package_native_library_dir() -> Result<&'static Path, OrtRuntimeError> {
    ms_platform::native_libs::native_library_dir().map_err(|err| OrtRuntimeError::NativeLibraryDir(err.to_string()))
}

/// Other targets have no app package with a library folder. Unreachable through the
/// shipped manifest (bundled rows are Android-only, enforced by a manifest test); an
/// edited manifest gets a typed error, never a guessed path.
///
/// # Errors
/// Always [`OrtRuntimeError::NativeLibraryDir`].
#[cfg(not(target_os = "android"))]
fn package_native_library_dir() -> Result<&'static Path, OrtRuntimeError> {
    Err(OrtRuntimeError::NativeLibraryDir("this platform has no app package library folder".to_string()))
}

/// The bundled library of `entry` inside `dir`, after an existence check.
///
/// # Errors
/// [`OrtRuntimeError::BundledLibraryMissing`] when `dir/<library>` is not a regular file
/// (logged with the path: a packaging defect, not a user condition).
fn bundled_library_path(dir: &Path, entry: &manifest::BundledEntry) -> Result<PathBuf, OrtRuntimeError> {
    let path = dir.join(&entry.library);
    if path.is_file() {
        Ok(path)
    } else {
        runtime_log::log_error(format!(
            "[onnx-runtime] bundled ONNX Runtime library for build '{}' is missing at {}; \
             the APK was built without the onnxruntime-android AAR or with its library excluded.",
            entry.build,
            path.display()
        ));
        Err(OrtRuntimeError::BundledLibraryMissing(path))
    }
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

/// Appends each `members` archive path to `wanted`, flattened to its filename in `dir`
/// (so archive-path traversal is impossible), in `members` order.
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if a member path has no filename component.
fn add_flattened_members(
    wanted: &mut Vec<(String, PathBuf)>,
    members: &[String],
    dir: &Path,
) -> Result<(), OrtRuntimeError> {
    for member in members {
        wanted.push((member.clone(), dir.join(member_file_name(member)?)));
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
/// Shared by the primary source and every additional source: claims the `.part` for this
/// recipe (`claim_part`), lets `ms_download::fetch_verified` resume / stream / verify
/// (exact size + SHA256) / publish it as `.download/<archive>`, extracts exactly the
/// `wanted` members atomically, and removes the archive. `wanted` lists each
/// archive-internal member path with its destination in the cache directory; the members
/// are published in this order and returned with their size and SHA256 (for the cache
/// completion marker).
///
/// # Errors
/// - [`OrtRuntimeError::Cancelled`] when the cancel flag stopped the transfer.
/// - [`OrtRuntimeError::Download`] on a permanent failure, exhausted retries or a size
///   mismatch.
/// - [`OrtRuntimeError::ChecksumMismatch`] if the SHA256 does not match (`.part` deleted).
/// - [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::DylibNotFoundInArchive`]
///   if the archive cannot be unpacked or a wanted member is absent.
/// - [`OrtRuntimeError::Io`] on filesystem failure.
fn fetch_source(
    env: &DownloadEnv<'_>,
    source: SourceRecipe<'_>,
    wanted: &[(String, PathBuf)],
    download_dir: &Path,
    build: &str,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<Vec<PublishedMember>, OrtRuntimeError> {
    let archive_name = archive_file_name(source.url);
    let archive_path = download_dir.join(&archive_name);
    let part_path = download_dir.join(format!("{archive_name}.part"));
    let identity_path = download_dir.join(format!("{archive_name}.part.id"));
    claim_part(&part_path, &identity_path, source)?;

    let log_context = format!("build={build} archive={archive_name}");
    let engine = ResumableFetch { fetcher: env.fetcher, retry: env.retry, cancel: env.cancel, log_tag: LOG_TAG, log_context: &log_context };
    let request = FileRequest {
        url: source.url,
        part: &part_path,
        dest: &archive_path,
        expected: Expected::SizeSha256 { size: source.size, sha256: source.sha256 },
    };
    let mut throttle = ProgressThrottle::default();
    let fetched = fetch_verified(&engine, &request, &mut |event| forward_file_event(event, source.size, &mut throttle, progress));
    if let Err(err) = fetched {
        match &err {
            DownloadError::Cancelled => runtime_log::log_info(format!("{LOG_TAG} download cancelled ({log_context}); the staged bytes are kept")),
            _ => runtime_log::log_error(format!("{LOG_TAG} download failed ({log_context}): {err}")),
        }
        return Err(map_download_error(source.url, err));
    }
    runtime_log::log_info(format!("{LOG_TAG} size and SHA256 verified for '{}'", archive_path.display()));
    // The `.part` became the archive: its identity file is stale now. A leftover one is
    // harmless (the next `claim_part` rewrites it), so a failure only warns.
    remove_file_logged(&identity_path, "stale download identity");

    emit(progress, OrtDownloadStage::Extracting, 0, None);
    let published = extract_wanted(&archive_path, source.archive, wanted)?;

    // The archive is no longer needed once its libraries are extracted; a cleanup
    // failure is non-fatal (the cache directory still holds the extracted files).
    remove_file_logged(&archive_path, "archive");
    Ok(published)
}

/// Makes `part` belong to `source` before a download resumes it.
///
/// `identity` (`<archive>.part.id`) records the URL and SHA256 the `.part` was staged
/// for. A `.part` whose identity is missing, unreadable or names another recipe (same
/// archive name, e.g. a re-pinned row after a manifest change) is deleted, because its
/// bytes cannot be proven to belong to this archive; then the identity is (re)written.
/// The identity file is written non-atomically on purpose: a torn one only mismatches
/// next time, which discards the `.part` — never resumes foreign bytes.
///
/// # Errors
/// [`OrtRuntimeError::Io`] when the foreign `.part` cannot be removed or the identity
/// cannot be written.
fn claim_part(part: &Path, identity: &Path, source: SourceRecipe<'_>) -> Result<(), OrtRuntimeError> {
    let expected = format!("{}\n{}\n", source.url, source.sha256);
    let owned = match fs::read(identity) {
        Ok(bytes) => bytes == expected.as_bytes(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            runtime_log::log_warn(format!("{LOG_TAG} cannot read download identity '{}' ({err}); restarting that download", identity.display()));
            false
        }
    };
    if owned {
        return Ok(());
    }
    match fs::remove_file(part) {
        Ok(()) => runtime_log::log_info(format!("{LOG_TAG} discarded '{}': it was staged for another archive", part.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = part.display(), err = err)));
        }
    }
    fs::write(identity, expected.as_bytes()).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.write_error", path = identity.display(), err = err))
    })
}

/// Removes `path` if it exists; any other failure is logged as a warning naming `what`.
fn remove_file_logged(path: &Path, what: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => runtime_log::log_warn(format!("{LOG_TAG} failed to remove {what} '{}': {err}", path.display())),
    }
}

/// Rate limit of byte-counting progress reports: a report is sent when the stage
/// changes, when [`PROGRESS_EMIT_INTERVAL_BYTES`] more bytes passed, or at the end.
#[derive(Debug, Default)]
struct ProgressThrottle {
    /// Stage and byte count of the last report sent.
    last: Option<(OrtDownloadStage, u64)>,
}

impl ProgressThrottle {
    /// Whether a `(stage, bytes)` report of a `total`-byte operation should be sent; records it when so.
    fn admit(&mut self, stage: OrtDownloadStage, bytes: u64, total: u64) -> bool {
        let send = match self.last {
            Some((last_stage, last_bytes)) => last_stage != stage || bytes.saturating_sub(last_bytes) >= PROGRESS_EMIT_INTERVAL_BYTES || bytes >= total,
            None => true,
        };
        if send {
            self.last = Some((stage, bytes));
        }
        send
    }
}

/// Maps one `fetch_verified` event of a `size`-byte archive onto the caller's progress.
fn forward_file_event(event: FileEvent, size: u64, throttle: &mut ProgressThrottle, progress: &mut dyn FnMut(OrtDownloadProgress)) {
    match event {
        // `fetch_verified`'s own destination guard: unreachable while the per-build busy id
        // is held (every writer of this archive holds it first), reported anyway.
        FileEvent::Waiting => emit(progress, OrtDownloadStage::WaitingForOtherDownload, 0, None),
        FileEvent::Verifying { done, total } => {
            if throttle.admit(OrtDownloadStage::Verifying, done, total) {
                emit(progress, OrtDownloadStage::Verifying, done, Some(total));
            }
        }
        FileEvent::Stream(StreamEvent::Downloading { staged, total }) => {
            let total = total.unwrap_or(size);
            if throttle.admit(OrtDownloadStage::Downloading, staged, total) {
                emit(progress, OrtDownloadStage::Downloading, staged, Some(total));
            }
        }
        FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged }) => {
            let stage = OrtDownloadStage::Retrying { attempt, max_attempts, delay_secs };
            // Every countdown step is a distinct stage value, so each one is sent.
            if throttle.admit(stage, staged, size) {
                emit(progress, stage, staged, Some(size));
            }
        }
    }
}

/// Maps an `ms_download` failure of `url` onto the resolver's typed error. The wrapped
/// `DownloadError` text is English diagnostics (no secrets); the variant's frame is
/// localized by [`OrtRuntimeError`]'s `Display`.
fn map_download_error(url: &str, err: DownloadError) -> OrtRuntimeError {
    match err {
        DownloadError::Cancelled => OrtRuntimeError::Cancelled,
        DownloadError::HashMismatch { expected, actual } => OrtRuntimeError::ChecksumMismatch { expected, actual },
        DownloadError::Archive { detail } => OrtRuntimeError::Extraction(detail),
        err @ DownloadError::Io { .. } => OrtRuntimeError::Io(err.to_string()),
        err @ (DownloadError::Busy { .. } | DownloadError::InvalidRequest { .. } | DownloadError::Http { .. } | DownloadError::SizeMismatch { .. }) => {
            OrtRuntimeError::Download(tf!("onnx_runtime.download.transfer_error", url = url, detail = err))
        }
    }
}

/// Extracts exactly the `wanted` members from `archive_path` into their destinations,
/// atomically per member.
///
/// `wanted` lists each archive-internal member path with its destination path (already
/// flattened to a controlled filename inside the cache directory, so archive-path
/// traversal is impossible). Only the listed members are unpacked (so, e.g., the 300 MB
/// Windows `.pdb` is never touched); symlink and directory entries matching a member path
/// are skipped in favor of the real file. Every member is first written and fsynced to
/// `<dest>.tmp`; only when ALL of them were found are the temporaries renamed onto their
/// destinations in `wanted` order (the caller lists the primary library last) and the
/// directory fsynced (unix). On any failure every temporary is removed and no destination
/// is touched. Several renames are not one atomic step: the caller invalidates the cache
/// marker before and rewrites it after (see `resolve_download_entry`).
///
/// # Errors
/// [`OrtRuntimeError::DylibNotFoundInArchive`] names the first missing member;
/// [`OrtRuntimeError::Extraction`] / [`OrtRuntimeError::Io`] on read / write failure.
fn extract_wanted(
    archive_path: &Path,
    archive: manifest::ArchiveKind,
    wanted: &[(String, PathBuf)],
) -> Result<Vec<PublishedMember>, OrtRuntimeError> {
    let digests = match stage_members(archive_path, archive, wanted) {
        Ok(digests) => digests,
        Err(err) => {
            for (_, dest) in wanted {
                remove_file_logged(&staging_path(dest), "extraction temporary");
            }
            return Err(err);
        }
    };
    let mut published = Vec::with_capacity(wanted.len());
    for (member, dest) in wanted {
        let staged = staging_path(dest);
        fs::rename(&staged, dest).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.publish_member_error", from = staged.display(), to = dest.display(), err = err))
        })?;
        // `stage_members` returned Ok, so every wanted member has a digest.
        if let Some((size, sha256)) = digests.get(member.as_str()) {
            published.push(PublishedMember { file_name: dest.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(), size: *size, sha256: sha256.clone() });
        }
    }
    let mut synced: HashSet<&Path> = HashSet::new();
    for (_, dest) in wanted {
        if let Some(parent) = dest.parent()
            && synced.insert(parent)
        {
            sync_dir(parent)?;
        }
    }
    Ok(published)
}

/// Writes every `wanted` member of the archive to its `<dest>.tmp` (fsynced), checks
/// that all of them were found, and returns each member's `(size, sha256)`.
fn stage_members(archive_path: &Path, archive: manifest::ArchiveKind, wanted: &[(String, PathBuf)]) -> Result<HashMap<String, (u64, String)>, OrtRuntimeError> {
    let lookup: HashMap<&str, &Path> = wanted.iter().map(|(member, dest)| (member.as_str(), dest.as_path())).collect();
    let mut found: HashMap<String, (u64, String)> = HashMap::new();
    let open = || {
        File::open(archive_path).map_err(|err| {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = archive_path.display(), err = err))
        })
    };

    match archive {
        manifest::ArchiveKind::Zip => {
            let mut zip = ZipArchive::new(open()?).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_read_error", err = err))
            })?;
            for index in 0..zip.len() {
                let mut file_in_zip = zip.by_index(index).map_err(|err| {
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_entry_error", index = index, err = err))
                })?;
                let name = file_in_zip.name().to_string();
                if let Some(dest) = lookup.get(name.as_str()) {
                    if file_in_zip.is_dir() {
                        continue;
                    }
                    let digest = write_stream(&mut file_in_zip, &staging_path(dest))?;
                    found.insert(name, digest);
                }
            }
        }
        manifest::ArchiveKind::TarGz => {
            extract_tar_members(GzDecoder::new(open()?), &lookup, &mut found)?;
        }
        manifest::ArchiveKind::TarZst => {
            let decoder = zstd::stream::read::Decoder::new(open()?).map_err(|err| {
                OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zstd_open_error", err = err))
            })?;
            extract_tar_members(decoder, &lookup, &mut found)?;
        }
    }

    // Every requested member must have been extracted; report the first miss in order.
    match wanted.iter().find(|(member, _)| !found.contains_key(member)) {
        Some((member, _)) => Err(OrtRuntimeError::DylibNotFoundInArchive(member.clone())),
        None => Ok(found),
    }
}

/// Stages the wanted members of a tar stream `reader` (see [`stage_members`]), recording
/// found keys.
///
/// Directory and symlink entries are skipped so only the real library files land
/// in the cache directory (the versioned `.so`/`.dylib` file, not its symlinks).
fn extract_tar_members<R: Read>(
    reader: R,
    wanted: &HashMap<&str, &Path>,
    found: &mut HashMap<String, (u64, String)>,
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
        if let Some(dest) = wanted.get(key) {
            // Only the real regular file is wanted; the archive's `.so`/`.dylib`
            // symlinks point at it and are intentionally ignored.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let digest = write_stream(&mut entry, &staging_path(dest))?;
            found.insert(key.to_string(), digest);
        }
    }
    Ok(())
}

/// The temporary sibling `<dest file name>.tmp` an extracted member is staged in.
fn staging_path(dest: &Path) -> PathBuf {
    // Every destination is `cache dir / member_file_name(..)`, which always has a file
    // name; an empty name would still give a distinct `.tmp` sibling.
    let mut name = dest.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".tmp");
    dest.with_file_name(name)
}

/// Copies a readable archive member into a fresh `out_path` (creating parent dirs),
/// fsyncs it and returns its `(size, lowercase sha256)`.
fn write_stream<R: Read>(reader: &mut R, out_path: &Path) -> Result<(u64, String), OrtRuntimeError> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_dir_error", parent = parent.display(), err = err))
        })?;
    }
    let mut output = File::create(out_path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_file_error", out_path = out_path.display(), err = err))
    })?;
    let mut hashing = HashingWriter { inner: &mut output, hasher: Sha256::new() };
    let size = std::io::copy(reader, &mut hashing).map_err(|err| {
        OrtRuntimeError::Extraction(tf!("onnx_runtime.download.extract_element_error", out_path = out_path.display(), err = err))
    })?;
    let sha256 = hex_lower(&hashing.hasher.finalize());
    output.sync_all().map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = out_path.display(), err = err))
    })?;
    Ok((size, sha256))
}

/// A writer that hashes everything it forwards to `inner`.
struct HashingWriter<'a> {
    inner: &'a mut File,
    hasher: Sha256,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        // Only the bytes the file accepted are hashed; `io::copy` resubmits the rest.
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Lowercase hex form of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // Both nibbles are < 16, so the indexing is always in bounds.
    bytes.iter().flat_map(|byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0x0f)]]).map(char::from).collect()
}

/// Fsyncs directory `dir` so the renames / removals inside it are durable (unix; a no-op
/// elsewhere, where directory handles cannot be fsynced).
fn sync_dir(dir: &Path) -> Result<(), OrtRuntimeError> {
    #[cfg(unix)]
    {
        File::open(dir).and_then(|handle| handle.sync_all()).map_err(|err| {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = dir.display(), err = err))
        })?;
    }
    #[cfg(not(unix))]
    {
        // Infallible: only unix fsyncs directory handles, so `dir` is otherwise unused.
        let _ = dir;
    }
    Ok(())
}

/// File name of a build cache directory's completion marker.
const CACHE_MARKER: &str = ".ort-cache-complete";

/// First line of the completion marker: bump it when the marker's meaning changes.
const CACHE_MARKER_FORMAT: &str = "ms-onnx-runtime cache v1";

/// One library recorded in the completion marker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedMember {
    /// File name inside the cache directory.
    file_name: String,
    /// Exact byte size at publication.
    size: u64,
    /// Lowercase sha256 at publication (diagnostic; the fast path checks sizes only, so
    /// a cached probe never hashes hundreds of MB).
    sha256: String,
}

/// The recipe header of the completion marker for `entry`: format, build, version and
/// every source's url + sha256 in fetch order. A marker is trusted only when it starts
/// with exactly this text, so any manifest change (re-pin, version bump, new source)
/// makes the cache stale.
fn cache_recipe(build: &str, entry: &manifest::ManifestEntry) -> String {
    let mut recipe = format!("{CACHE_MARKER_FORMAT}\nbuild {build}\nversion {}\n", entry.version);
    for source in &entry.additional_sources {
        recipe.push_str(&format!("source {} {}\n", source.url, source.sha256));
    }
    recipe.push_str(&format!("source {} {}\n", entry.url, entry.sha256));
    recipe
}

/// The full marker: the recipe followed by one `member <name> <size> <sha256>` line per
/// published library.
fn cache_marker_text(recipe: &str, members: &[PublishedMember]) -> String {
    let mut text = recipe.to_owned();
    for member in members {
        text.push_str(&format!("member {} {} {}\n", member.file_name, member.size, member.sha256));
    }
    text
}

/// Whether the cache in `dir` is complete for `recipe`: the marker exists, starts with
/// exactly `recipe`, lists exactly the file names of `expected_members`, and each of them
/// is a regular file of the recorded size. Every doubt (missing, unreadable or malformed
/// marker) answers `false`, which rebuilds the cache.
fn cache_complete(dir: &Path, recipe: &str, expected_members: &[PathBuf]) -> bool {
    let marker_path = dir.join(CACHE_MARKER);
    let text = match fs::read_to_string(&marker_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            runtime_log::log_warn(format!("{LOG_TAG} cannot read cache marker '{}' ({err}); rebuilding the cache", marker_path.display()));
            return false;
        }
    };
    let Some(members) = text.strip_prefix(recipe) else { return false };
    let mut recorded: Vec<(&str, u64)> = Vec::new();
    for line in members.lines() {
        // `member <name> <size> <sha256>`; names never contain spaces (archive file names).
        let mut fields = line.split(' ');
        let (Some("member"), Some(name), Some(size), Some(_sha256), None) = (fields.next(), fields.next(), fields.next(), fields.next(), fields.next()) else {
            return false;
        };
        let Ok(size) = size.parse::<u64>() else { return false };
        recorded.push((name, size));
    }
    let mut expected: Vec<String> = expected_members.iter().filter_map(|path| path.file_name()).map(|name| name.to_string_lossy().into_owned()).collect();
    expected.sort();
    let mut recorded_names: Vec<&str> = recorded.iter().map(|(name, _)| *name).collect();
    recorded_names.sort_unstable();
    if expected.len() != expected_members.len() || recorded_names != expected {
        return false;
    }
    recorded.iter().all(|(name, size)| fs::metadata(dir.join(name)).is_ok_and(|meta| meta.is_file() && meta.len() == *size))
}

/// Removes the completion marker of `dir` (if any) and fsyncs `dir`, so the removal is
/// durable before any member is replaced.
///
/// # Errors
/// [`OrtRuntimeError::Io`] when the marker exists but cannot be removed, or on the fsync.
fn invalidate_cache_marker(dir: &Path) -> Result<(), OrtRuntimeError> {
    let marker_path = dir.join(CACHE_MARKER);
    match fs::remove_file(&marker_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = marker_path.display(), err = err)));
        }
    }
    sync_dir(dir)
}

/// Publishes the completion marker `text` atomically: `<marker>.tmp`, fsync, rename,
/// fsync of `dir`.
///
/// # Errors
/// [`OrtRuntimeError::Io`] on any write, rename or fsync failure (the cache then stays
/// untrusted and the next call rebuilds it).
fn write_cache_marker(dir: &Path, text: &str) -> Result<(), OrtRuntimeError> {
    let marker_path = dir.join(CACHE_MARKER);
    let staged = staging_path(&marker_path);
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|err| OrtRuntimeError::Io(tf!("onnx_runtime.download.write_error", path = staged.display(), err = err)))?;
    fs::rename(&staged, &marker_path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.download.publish_member_error", from = staged.display(), to = marker_path.display(), err = err))
    })?;
    sync_dir(dir)
}

/// The onnxruntime library filename to place and return for this build target.
///
/// The returned name is what [`resolve_or_download_ort_dylib`] both probes for and
/// writes the primary member to, so probe and extraction always agree. Linux and
/// macOS embed the version in the soname; Windows and Android do not (Android's
/// `onnxruntime-android` library has the unversioned SONAME `libonnxruntime.so`; its
/// builds are bundled, so only an Android download row would ever reach this arm).
#[must_use]
fn expected_primary_dylib_filename(ort_version: &str) -> String {
    #[cfg(any(target_os = "windows", target_os = "android"))]
    {
        // The Windows DLL / Android library name is unversioned; the version param is
        // unused here.
        let _ = ort_version;
        if cfg!(target_os = "windows") { "onnxruntime.dll".to_string() } else { "libonnxruntime.so".to_string() }
    }
    #[cfg(target_os = "macos")]
    {
        format!("libonnxruntime.{ort_version}.dylib")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "android")))]
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
            "sha256": "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100",
            "size": 10,
            "archive": "zip",
            "dylib_member": "lib/primary.dll",
            "extra_members": ["lib/side.dll"],
            "additional_sources": [
                { "url": "https://example/extra.zip",
                  "sha256": "0011223344556677889900112233445566778899001122334455667788990011",
                  "size": 20, "archive": "zip", "members": ["bin/x64/dxil.dll"] }
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
    fn missing_build_entry_yields_typed_error_without_network() {
        // The `directml` build has no Linux archive (Windows-only); on the Linux test
        // host its resolution must fail with a typed no-entry error before any network
        // access, and must not panic. (The cache dir for `directml` does not exist in a
        // clean tree, so the probe misses and the lookup runs.) The CUDA builds resolve
        // on Windows/Linux — asserted in the manifest tests.
        let mut progress = |_p: OrtDownloadProgress| {};
        let result = resolve_or_download_ort_dylib("directml", &AtomicBool::new(false), &mut progress);
        assert!(matches!(
            &result,
            Err(OrtRuntimeError::NoManifestEntry { build, .. }) if build == "directml"
        ));
    }

    #[test]
    fn bundled_library_path_requires_the_file() {
        let base = std::env::temp_dir().join(format!("ort_bundled_test_{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("create test dir");
        let entry = manifest::BundledEntry {
            os: "android".to_string(),
            arch: "aarch64".to_string(),
            provider: "cpu".to_string(),
            build: "cpu".to_string(),
            version: "1.27.0".to_string(),
            library: "libonnxruntime.so".to_string(),
        };
        let missing = bundled_library_path(&base, &entry);
        assert!(matches!(&missing, Err(OrtRuntimeError::BundledLibraryMissing(path)) if path == &base.join("libonnxruntime.so")));
        std::fs::write(base.join("libonnxruntime.so"), b"elf").expect("write fixture");
        assert_eq!(bundled_library_path(&base, &entry).expect("present"), base.join("libonnxruntime.so"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn host_has_no_package_library_folder() {
        assert!(matches!(package_native_library_dir(), Err(OrtRuntimeError::NativeLibraryDir(_))));
    }

    #[test]
    fn unavailable_displays_the_platform_message() {
        let err = ms_platform::Unavailable::new(
            ms_platform::PlatformFeature::NativeOnnxRuntime,
            ms_platform::UnavailableReason::NotOnThisPlatform,
        );
        assert_eq!(OrtRuntimeError::Unavailable(err).to_string(), err.user_message());
    }

    /// Scripted-server tests of the download half (`resolve_download_entry`): no network,
    /// every file under a self-removing temp directory.
    mod download {
        use super::*;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use ms_download::test_support::{ScriptedFetcher, Step};
        use zip::write::{SimpleFileOptions, ZipWriter};

        const PRIMARY_MEMBER: &str = "pkg/lib/primary.bin";
        const SIDE_MEMBER: &str = "pkg/lib/side.so";
        const VERSION: &str = "9.9.9";

        /// A stored (uncompressed) zip holding `members`.
        fn zip_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
            let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (name, body) in members {
                zip.start_file(*name, opts).expect("start member");
                zip.write_all(body).expect("write member");
            }
            zip.finish().expect("finish zip").into_inner()
        }

        /// The standard test archive: a primary library of `primary_len` bytes and a side library.
        fn archive(primary_len: usize) -> (Vec<u8>, Vec<u8>) {
            let primary: Vec<u8> = (0..primary_len).map(|index| u8::try_from(index % 251).expect("below 251")).collect();
            (zip_bytes(&[(PRIMARY_MEMBER, primary.as_slice()), (SIDE_MEMBER, b"side".as_slice()), ("pkg/README", b"skip".as_slice())]), primary)
        }

        fn sha_hex(bytes: &[u8]) -> String {
            sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
        }

        /// A download row for `body` with the given pin (`sha256` defaults to the real one).
        fn entry_for(body: &[u8], sha256: Option<&str>) -> manifest::ManifestEntry {
            let json = serde_json::json!({
                "os": "linux", "arch": "x86_64", "provider": "cpu", "build": "test",
                "version": VERSION,
                "url": "https://example.invalid/ort-test.zip",
                "sha256": sha256.map_or_else(|| sha_hex(body), str::to_owned),
                "size": body.len(),
                "archive": "zip",
                "dylib_member": PRIMARY_MEMBER,
                "extra_members": [SIDE_MEMBER]
            });
            serde_json::from_value(json).expect("test entry parses")
        }

        /// Retry schedule without real waits (busy waits poll every millisecond).
        fn policy(max_retries: u32) -> RetryPolicy<'static> {
            RetryPolicy {
                max_retries,
                first_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                retry_after_cap: Duration::ZERO,
                wait_slice: Duration::from_millis(1),
                sleep: &std::thread::sleep,
            }
        }

        fn resolve(fetcher: &ScriptedFetcher, retry: &RetryPolicy<'_>, cancel: &AtomicBool, build: &str, entry: &manifest::ManifestEntry, dir: &Path, stages: &mut Vec<OrtDownloadProgress>) -> Result<PathBuf, OrtRuntimeError> {
            let env = DownloadEnv { fetcher, retry, cancel };
            resolve_download_entry(&env, build, entry, dir, &mut |update| stages.push(update))
        }

        fn primary_name() -> String {
            expected_primary_dylib_filename(VERSION)
        }

        fn download_dir(dir: &Path) -> PathBuf {
            dir.join(".download")
        }

        /// No `.tmp` extraction temporaries anywhere in the cache directory.
        fn assert_no_temporaries(dir: &Path) {
            for item in fs::read_dir(dir).expect("list cache dir") {
                let name = item.expect("dir entry").file_name();
                assert!(!name.to_string_lossy().ends_with(".tmp"), "leftover temporary {name:?}");
            }
        }

        #[test]
        fn fresh_download_verifies_extracts_and_cleans_up() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, primary) = archive(1000);
            let entry = entry_for(&body, None);
            let fetcher = ScriptedFetcher::new(&body, &[]);
            let mut stages = Vec::new();
            let path = resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-fresh", &entry, temp.path(), &mut stages).expect("resolves");

            assert_eq!(path, temp.path().join(primary_name()));
            assert_eq!(fs::read(&path).expect("primary"), primary);
            assert_eq!(fs::read(temp.path().join("side.so")).expect("side"), b"side");
            assert!(!temp.path().join("README").exists(), "unlisted member extracted");
            let leftovers: Vec<_> = fs::read_dir(download_dir(temp.path())).expect("download dir").collect();
            assert!(leftovers.is_empty(), "archive, .part or identity left behind");
            assert_no_temporaries(temp.path());
            let marker = fs::read_to_string(temp.path().join(CACHE_MARKER)).expect("completion marker");
            assert!(marker.starts_with(&cache_recipe("test-fresh", &entry)));
            assert!(marker.contains(&format!("member side.so 4 {}", sha_hex(b"side"))), "{marker}");
            assert!(stages.iter().any(|update| update.stage == OrtDownloadStage::Downloading && update.total == Some(chunk_u64(body.len()))));
            assert!(stages.iter().any(|update| update.stage == OrtDownloadStage::Extracting));
            assert_eq!(stages.last().map(|update| update.stage), Some(OrtDownloadStage::Done));

            // A second call takes the fast path: no request at all.
            let offline = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
            resolve(&offline, &policy(0), &AtomicBool::new(false), "test-fresh", &entry, temp.path(), &mut Vec::new()).expect("cached");
            assert!(offline.offsets().is_empty());
        }

        /// The marker record of a published file with `body`.
        fn member_record(file_name: &str, body: &[u8]) -> PublishedMember {
            PublishedMember { file_name: file_name.to_owned(), size: chunk_u64(body.len()), sha256: sha_hex(body) }
        }

        /// A fully resolved cache for `build` in a fresh temp dir.
        fn resolved_cache(build: &str) -> (tempfile::TempDir, Vec<u8>, manifest::ManifestEntry) {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), build, &entry, temp.path(), &mut Vec::new()).expect("first resolve");
            (temp, body, entry)
        }

        #[test]
        fn a_truncated_member_or_a_missing_marker_is_rebuilt() {
            let (temp, body, entry) = resolved_cache("test-truncated");
            // Marker intact, but a member no longer has the recorded size.
            fs::write(temp.path().join("side.so"), b"s").expect("truncate member");
            let fetcher = ScriptedFetcher::new(&body, &[]);
            resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-truncated", &entry, temp.path(), &mut Vec::new()).expect("repaired");
            assert_eq!(fetcher.offsets(), vec![0], "the truncated cache must be rebuilt");
            assert_eq!(fs::read(temp.path().join("side.so")).expect("side"), b"side");

            // Every member present but no marker (e.g. a cache of an older program): rebuilt.
            fs::remove_file(temp.path().join(CACHE_MARKER)).expect("drop marker");
            let fetcher = ScriptedFetcher::new(&body, &[]);
            resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-truncated", &entry, temp.path(), &mut Vec::new()).expect("rebuilt");
            assert_eq!(fetcher.offsets(), vec![0], "a cache without a marker must be rebuilt");
            assert!(temp.path().join(CACHE_MARKER).is_file());
        }

        #[test]
        fn installed_check_follows_the_marker_and_member_sizes() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            assert_eq!(installed_primary("test-installed", &entry, temp.path()).expect("check"), None);
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-installed", &entry, temp.path(), &mut Vec::new()).expect("resolve");
            assert_eq!(installed_primary("test-installed", &entry, temp.path()).expect("check"), Some(temp.path().join(primary_name())));
            // Another build slug's recipe does not match this marker.
            assert_eq!(installed_primary("test-other", &entry, temp.path()).expect("check"), None);
            fs::write(temp.path().join("side.so"), b"s").expect("truncate member");
            assert_eq!(installed_primary("test-installed", &entry, temp.path()).expect("check"), None);
        }

        #[test]
        fn builds_without_a_row_here_are_never_installed() {
            // Linux test host: DirectML is Windows-only, the slug below does not exist.
            assert!(!ort_build_installed("directml"));
            assert!(!ort_build_installed("no-such-build"));
        }

        #[test]
        fn a_marker_of_another_recipe_is_not_trusted() {
            let (temp, body, entry) = resolved_cache("test-recipe");
            // Same members and sizes, but recorded for a different pin of the archive.
            let mut other = entry.clone();
            other.sha256 = "0".repeat(64);
            let members = [member_record(&primary_name(), &fs::read(temp.path().join(primary_name())).expect("primary")), member_record("side.so", b"side")];
            write_cache_marker(temp.path(), &cache_marker_text(&cache_recipe("test-recipe", &other), &members)).expect("foreign marker");

            let fetcher = ScriptedFetcher::new(&body, &[]);
            resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-recipe", &entry, temp.path(), &mut Vec::new()).expect("rebuilt");
            assert_eq!(fetcher.offsets(), vec![0], "a foreign marker must not take the fast path");
            assert!(fs::read_to_string(temp.path().join(CACHE_MARKER)).expect("marker").starts_with(&cache_recipe("test-recipe", &entry)));
        }

        #[test]
        fn a_crash_between_member_renames_is_repaired_from_the_staged_archive() {
            let (temp, body, entry) = resolved_cache("test-crash");
            // State after a crash mid-publication: the marker was invalidated first, the
            // verified archive is still staged, one member is from the old generation.
            fs::remove_file(temp.path().join(CACHE_MARKER)).expect("marker invalidated");
            fs::write(download_dir(temp.path()).join("ort-test.zip"), &body).expect("staged archive");
            fs::write(temp.path().join(primary_name()), b"old generation").expect("old primary");

            // No network: the repair must come from the staged archive alone.
            let offline = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
            resolve(&offline, &policy(0), &AtomicBool::new(false), "test-crash", &entry, temp.path(), &mut Vec::new()).expect("repaired");
            assert!(offline.offsets().is_empty(), "the staged verified archive must be reused");
            let (_, primary) = archive(1000);
            assert_eq!(fs::read(temp.path().join(primary_name())).expect("primary"), primary);
            assert!(temp.path().join(CACHE_MARKER).is_file());
        }

        fn chunk_u64(len: usize) -> u64 {
            u64::try_from(len).expect("test sizes fit u64")
        }

        #[test]
        fn interrupted_download_resumes_on_the_next_call() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(4000);
            let entry = entry_for(&body, None);
            let first = ScriptedFetcher::new(&body, &[Step::Drop(1500)]);
            let err = resolve(&first, &policy(0), &AtomicBool::new(false), "test-resume", &entry, temp.path(), &mut Vec::new()).expect_err("retries exhausted");
            assert!(matches!(err, OrtRuntimeError::Download(_)), "{err:?}");
            let part = download_dir(temp.path()).join("ort-test.zip.part");
            assert_eq!(fs::metadata(&part).expect("part kept").len(), 1500);

            let second = ScriptedFetcher::new(&body, &[]);
            resolve(&second, &policy(0), &AtomicBool::new(false), "test-resume", &entry, temp.path(), &mut Vec::new()).expect("resumes");
            assert_eq!(second.offsets(), vec![1500]);
            assert!(temp.path().join(primary_name()).is_file());
        }

        #[test]
        fn transient_failure_is_retried_and_reported() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(2000);
            let entry = entry_for(&body, None);
            let fetcher = ScriptedFetcher::new(&body, &[Step::Status(503, None)]);
            let mut stages = Vec::new();
            resolve(&fetcher, &policy(2), &AtomicBool::new(false), "test-retry", &entry, temp.path(), &mut stages).expect("retried");
            assert!(stages.iter().any(|update| matches!(update.stage, OrtDownloadStage::Retrying { attempt: 1, max_attempts: 2, .. })), "{stages:?}");
        }

        #[test]
        fn hash_mismatch_deletes_the_part_and_extracts_nothing() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = entry_for(&body, Some(&"0".repeat(64)));
            let fetcher = ScriptedFetcher::new(&body, &[]);
            let err = resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-hash", &entry, temp.path(), &mut Vec::new()).expect_err("mismatch");
            assert!(matches!(err, OrtRuntimeError::ChecksumMismatch { .. }), "{err:?}");
            assert!(!download_dir(temp.path()).join("ort-test.zip.part").exists());
            assert!(!download_dir(temp.path()).join("ort-test.zip").exists());
            assert!(!temp.path().join(primary_name()).exists());
        }

        #[test]
        fn a_part_staged_for_another_archive_is_discarded() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            let staging = download_dir(temp.path());
            fs::create_dir_all(&staging).expect("download dir");
            fs::write(staging.join("ort-test.zip.part"), b"foreign bytes").expect("foreign part");
            fs::write(staging.join("ort-test.zip.part.id"), b"https://example.invalid/other.zip\nabc\n").expect("foreign identity");

            let fetcher = ScriptedFetcher::new(&body, &[]);
            resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-foreign", &entry, temp.path(), &mut Vec::new()).expect("resolves");
            assert_eq!(fetcher.offsets(), vec![0], "the foreign .part must not be resumed");
        }

        #[test]
        fn cancel_during_the_transfer_keeps_the_part() {
            let temp = tempfile::tempdir().expect("temp dir");
            // Larger than the progress interval, so a report with staged bytes arrives mid-transfer.
            let (body, _) = archive(1_600_000);
            let entry = entry_for(&body, None);
            let fetcher = ScriptedFetcher::new(&body, &[]);
            let cancel = AtomicBool::new(false);
            let retry = policy(0);
            let env = DownloadEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel };
            let err = resolve_download_entry(&env, "test-cancel", &entry, temp.path(), &mut |update| {
                if update.stage == OrtDownloadStage::Downloading && update.downloaded > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .expect_err("cancelled");
            assert!(matches!(err, OrtRuntimeError::Cancelled), "{err:?}");
            let staged = fs::metadata(download_dir(temp.path()).join("ort-test.zip.part")).expect("part kept").len();
            assert!(staged > 0 && staged < chunk_u64(body.len()), "staged {staged}");
            assert!(!temp.path().join(primary_name()).exists());
        }

        #[test]
        fn a_second_resolver_waits_then_takes_the_fast_path() {
            let temp = tempfile::tempdir().expect("temp dir");
            let dir = temp.path().to_path_buf();
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            let recipe = cache_recipe("test-concurrent", &entry);
            let holder = BusyGuard::try_acquire(&format!("ort:test-concurrent:{VERSION}")).expect("key free");

            let (stage_tx, stage_rx) = std::sync::mpsc::channel();
            let worker_dir = dir.clone();
            let worker = std::thread::spawn(move || {
                let fetcher = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
                let retry = policy(0);
                let cancel = AtomicBool::new(false);
                let env = DownloadEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel };
                // A send fails only once the test stopped listening; the stage is then moot.
                let result = resolve_download_entry(&env, "test-concurrent", &entry, &worker_dir, &mut |update| {
                    stage_tx.send(update.stage).ok();
                });
                (result, fetcher.offsets())
            });
            // Wait until the worker reports that it is blocked on the held key, then publish
            // the cache as the other holder would and release the key.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let stage = stage_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect("worker reports waiting");
                if stage == OrtDownloadStage::WaitingForOtherDownload {
                    break;
                }
            }
            fs::write(dir.join(primary_name()), b"primary").expect("publish primary");
            fs::write(dir.join("side.so"), b"side").expect("publish side");
            let members = [member_record(&primary_name(), b"primary"), member_record("side.so", b"side")];
            write_cache_marker(&dir, &cache_marker_text(&recipe, &members)).expect("publish marker");
            drop(holder);

            let (result, offsets) = worker.join().expect("worker thread");
            assert_eq!(result.expect("fast path after the wait"), dir.join(primary_name()));
            assert!(offsets.is_empty(), "no request after the other holder finished");
        }

        #[test]
        fn extraction_stages_every_member_before_publishing_any() {
            let temp = tempfile::tempdir().expect("temp dir");
            let zip_path = temp.path().join("fixture.zip");
            fs::write(&zip_path, zip_bytes(&[("capi/onnxruntime.dll", b"primary".as_slice()), ("capi/dxil.dll", b"dxil".as_slice())])).expect("write zip");
            let out = temp.path().join("out");

            // One wanted member is absent: nothing may be published, no temporary left.
            let incomplete = vec![("capi/dxil.dll".to_string(), out.join("dxil.dll")), ("capi/absent.dll".to_string(), out.join("absent.dll"))];
            let err = extract_wanted(&zip_path, manifest::ArchiveKind::Zip, &incomplete).expect_err("missing member");
            assert!(matches!(&err, OrtRuntimeError::DylibNotFoundInArchive(member) if member == "capi/absent.dll"), "{err:?}");
            assert!(!out.join("dxil.dll").exists(), "a member was published before the set was complete");
            assert_no_temporaries(&out);

            let complete = vec![("capi/dxil.dll".to_string(), out.join("dxil.dll")), ("capi/onnxruntime.dll".to_string(), out.join("onnxruntime.dll"))];
            extract_wanted(&zip_path, manifest::ArchiveKind::Zip, &complete).expect("extract");
            assert_eq!(fs::read(out.join("onnxruntime.dll")).expect("primary"), b"primary");
            assert_eq!(fs::read(out.join("dxil.dll")).expect("dxil"), b"dxil");
            assert_no_temporaries(&out);
        }

        #[test]
        fn download_errors_map_to_typed_variants() {
            assert!(matches!(map_download_error("u", DownloadError::Cancelled), OrtRuntimeError::Cancelled));
            assert!(matches!(map_download_error("u", DownloadError::HashMismatch { expected: "a".into(), actual: "b".into() }), OrtRuntimeError::ChecksumMismatch { .. }));
            assert!(matches!(map_download_error("u", DownloadError::SizeMismatch { expected: 1, actual: 2 }), OrtRuntimeError::Download(_)));
            assert!(matches!(map_download_error("u", DownloadError::Archive { detail: "x".into() }), OrtRuntimeError::Extraction(_)));
        }
    }
}
