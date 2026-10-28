/*
File: crates/ms-onnx-runtime/src/lib.rs

Purpose:
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform/provider so the `ms-onnx` crate can dlopen it. On first
use it probes a portable, per-build cache directory; if the library is absent it
installs the pinned official release archive as an `ms-native-package` package
(resumable `.part`, retry with backoff, cancel, exact size + SHA256, atomic extraction,
completion marker), and returns its on-disk path. The app starts fine without the library
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
- resolve_download_entry       : the download-row half over an injected fetcher (tests):
  an adapter over `ms_native_package::ensure_installed`
- with_package                 : the manifest row as a package (layout, recipe fields,
  sources in fetch order, primary library last)
- map_package_error            : `PackageError` -> the localized `onnx_runtime.download.*`
  texts the resolver has always produced (table in its doc comment)
- build_source_kind_here / ort_downloads_offered_here: download vs bundled, for the UI

Notes:
The onnxruntime version is per manifest ENTRY, not global: the resolved cache dir
and versioned library name come from the looked-up entry's `version` (WebGPU on
1.27.0 coexists with the 1.20.1 providers). An entry may pull extra libraries from
`additional_sources` (extra archives) into the same cache directory; the primary
and every additional source are sources of one package. One resolution per
build runs at a time in the process (busy id `ort:<build>:<version>`), so the settings
panel's auto-download and a native OCR load never write the same files. Downloads and
disk I/O are blocking; `resolve_or_download_ort_dylib` MUST run on a worker thread,
never on the GUI thread. This module owns dylib RESOLUTION only; it never links,
initializes, or runs onnxruntime — that is `ms-onnx`'s job, and it consumes the
path returned here. A BUNDLED build (Android: the `onnxruntime-android` AAR merged into the
APK) has no recipe: resolution returns `<package native library dir>/<library>` after an
existence check, with no network and no cache directory. Storage lives under
`ms_config::data_dir()`. The cache layout (`.ort-cache-complete` marker with format line
`ms-onnx-runtime cache v1`, `.download/<archive>.part` + `.part.id`) is a persistence
contract: caches written by earlier program versions stay trusted. The binary names this
crate directly as `ms_onnx_runtime::`.
*/

#![warn(clippy::all)]

// `t!` / `tf!` for the localized failure strings of every stage.
#[macro_use]
extern crate ms_i18n;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use ms_download::DownloadError;
use ms_download::resumable::{RangeFetcher, RetryPolicy, UreqFetcher};
use ms_download::unpack::UnpackError;
use ms_native_package::{
    ArchiveKind, FetchEnv, InstallOutcome, PackageError, PackageIoOp, PackageLayout, PackageMember, PackageProgress, PackageRecipe, PackageSource,
    PackageStage,
};

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

/// Download-row half of [`resolve_or_download_ort_dylib`] for `entry` into the cache
/// directory `dir`, as an adapter over `ms_native_package::ensure_installed`: the package is
/// this row's recipe ([`with_package`]: fields `build` + `version`, the additional sources
/// first and the primary archive LAST, so the primary library is the last member published),
/// the busy key is `ort:<build>:<version>` and the marker is [`CACHE_MARKER`] with format line
/// [`CACHE_MARKER_FORMAT`]. Progress maps 1:1 ([`ort_progress`]), failures through
/// [`map_package_error`].
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
    let primary_path = dir.join(expected_primary_dylib_filename(version));
    // One resolution of this build per process: the settings panel's auto-download and a
    // native OCR load may ask for the same build at once; the second caller waits, then
    // normally finds the cache complete.
    let busy_key = format!("ort:{build}:{version}");
    let log_context = format!("build={build} version={version}");
    let fetch_env = FetchEnv { fetcher: env.fetcher, retry: env.retry, cancel: env.cancel, log_tag: LOG_TAG, log_context: &log_context };
    let outcome = with_package(build, entry, dir, |layout, recipe| {
        ms_native_package::ensure_installed(&fetch_env, layout, recipe, &busy_key, &mut |update| progress(ort_progress(update)))
    })?
    .map_err(map_package_error)?;
    match outcome {
        InstallOutcome::AlreadyInstalled => runtime_log::log_info(format!(
            "[onnx-runtime] found cached library for build '{build}' (provider '{provider_id}') at {}",
            primary_path.display()
        )),
        InstallOutcome::InstalledByOther => runtime_log::log_info(format!(
            "[onnx-runtime] build '{build}' was prepared by another task: {}",
            primary_path.display()
        )),
        InstallOutcome::Installed => runtime_log::log_info(format!(
            "[onnx-runtime] ready for build '{build}' (provider '{provider_id}'): {}",
            primary_path.display()
        )),
    }
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
/// complete for it, else `None`: `ms_native_package::is_installed` over this row's package
/// ([`with_package`]) — the completion marker records EXACTLY this manifest row (build,
/// version, every source's url + sha256) and every expected member (primary + extras +
/// additional-source members) is on disk at the recorded size. A partial, truncated,
/// mixed-generation or foreign cache is `None` (the resolver rebuilds it).
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if a manifest member path has no file name.
fn installed_primary(build: &str, entry: &manifest::ManifestEntry, dir: &Path) -> Result<Option<PathBuf>, OrtRuntimeError> {
    let installed = with_package(build, entry, dir, ms_native_package::is_installed)?;
    Ok(installed.then(|| dir.join(expected_primary_dylib_filename(&entry.version))))
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

/// Runs `f` over the `ms-native-package` view of download row `entry` cached in `dir`:
/// layout `dir` + [`CACHE_MARKER`] + [`CACHE_MARKER_FORMAT`]; recipe fields
/// `[("build", build), ("version", entry.version)]`; sources = every `additional_sources`
/// archive, then the primary archive (staged under the last URL segment,
/// [`archive_file_name`]); members flattened to their file names, except the primary
/// `dylib_member`, which is published as [`expected_primary_dylib_filename`] and listed LAST
/// of the primary archive (after its `extra_members`).
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] (`invalid_member_path`) for the first member path without
/// a file name, `extra_members` checked before `additional_sources` members.
fn with_package<R>(build: &str, entry: &manifest::ManifestEntry, dir: &Path, f: impl FnOnce(&PackageLayout<'_>, &PackageRecipe<'_>) -> R) -> Result<R, OrtRuntimeError> {
    /// `members` as package members published under their flattened file names.
    fn flattened(members: &[String]) -> Result<Vec<PackageMember<'_>>, OrtRuntimeError> {
        members.iter().map(|member| Ok(PackageMember { archive_path: member, file_name: member_file_name(member)? })).collect()
    }
    let primary_name = expected_primary_dylib_filename(&entry.version);
    let mut primary_members = flattened(&entry.extra_members)?;
    let additional_members = entry.additional_sources.iter().map(|source| flattened(&source.members)).collect::<Result<Vec<_>, _>>()?;
    primary_members.push(PackageMember { archive_path: &entry.dylib_member, file_name: &primary_name });
    let additional_names: Vec<String> = entry.additional_sources.iter().map(|source| archive_file_name(&source.url)).collect();
    let primary_archive = archive_file_name(&entry.url);

    let mut sources: Vec<PackageSource<'_>> = entry
        .additional_sources
        .iter()
        .zip(&additional_names)
        .zip(&additional_members)
        .map(|((source, file_name), members)| PackageSource { url: &source.url, file_name, size: source.size, sha256: &source.sha256, archive: package_archive_kind(source.archive), members })
        .collect();
    sources.push(PackageSource { url: &entry.url, file_name: &primary_archive, size: entry.size, sha256: &entry.sha256, archive: package_archive_kind(entry.archive), members: &primary_members });
    let fields = [("build", build), ("version", entry.version.as_str())];
    let layout = PackageLayout { dir, marker_file: CACHE_MARKER, marker_format: CACHE_MARKER_FORMAT };
    Ok(f(&layout, &PackageRecipe { fields: &fields, sources: &sources }))
}

/// The `ms-download` archive kind of a manifest archive kind (1:1).
fn package_archive_kind(kind: manifest::ArchiveKind) -> ArchiveKind {
    match kind {
        manifest::ArchiveKind::Zip => ArchiveKind::Zip,
        manifest::ArchiveKind::TarGz => ArchiveKind::TarGz,
        manifest::ArchiveKind::TarZst => ArchiveKind::TarZst,
    }
}

/// The trailing filename of an archive-internal member path
/// (`ms_native_package::flatten_member_name`).
///
/// # Errors
/// [`OrtRuntimeError::Extraction`] if `member` has no filename component.
fn member_file_name(member: &str) -> Result<&str, OrtRuntimeError> {
    ms_native_package::flatten_member_name(member).map_err(map_package_error)
}

/// The resolver's progress snapshot of a package progress snapshot (stages map 1:1, byte
/// counters unchanged).
fn ort_progress(update: PackageProgress) -> OrtDownloadProgress {
    let stage = match update.stage {
        PackageStage::Probing => OrtDownloadStage::Probing,
        PackageStage::WaitingForOtherDownload => OrtDownloadStage::WaitingForOtherDownload,
        PackageStage::Downloading => OrtDownloadStage::Downloading,
        PackageStage::Retrying { attempt, max_attempts, delay_secs } => OrtDownloadStage::Retrying { attempt, max_attempts, delay_secs },
        PackageStage::Verifying => OrtDownloadStage::Verifying,
        PackageStage::Extracting => OrtDownloadStage::Extracting,
        PackageStage::Done => OrtDownloadStage::Done,
    };
    OrtDownloadProgress { downloaded: update.downloaded, total: update.total, stage }
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

/// Maps a package failure onto the resolver's typed error, reproducing the localized text the
/// resolver produced before the transaction moved to `ms-native-package` (same variant, same
/// `onnx_runtime.download.*` key, same placeholders, the same underlying error rendered):
///
/// | `PackageError` | `OrtRuntimeError` |
/// |---|---|
/// | `Cancelled` | `Cancelled` |
/// | `ChecksumMismatch` | `ChecksumMismatch { expected, actual }` |
/// | `Download { url, source }` | [`map_download_error`]`(url, source)` (`transfer_error`, or `Io` = the raw `DownloadError` text) |
/// | `Unpack`: `OpenArchive` | `Extraction(open_archive_error { archive_path, err })` |
/// | `Unpack`: `ZipRead` / `ZipEntry` / `ZstdOpen` | `Extraction(zip_read_error { err })` / `(zip_entry_error { index, err })` / `(zstd_open_error { err })` |
/// | `Unpack`: `TarRead` / `TarEntry` / `TarInvalidPath` | `Extraction(tar_read_error / tar_entry_error / tar_invalid_path { err })` |
/// | `Unpack`: `CreateDir` / `CreateFile` | `Io(extract_create_dir_error { parent, err })` / `Io(extract_create_file_error { out_path, err })` |
/// | `Unpack`: `CopyMember` | `Extraction(extract_element_error { out_path, err })` |
/// | `Unpack`: `SyncFile` | `Io(save_error { path, err })` |
/// | `Unpack`: `MemberMissing` | `DylibNotFoundInArchive(member)` |
/// | `Io`: `CreateDir` / `CreateDownloadDir` | `Io(create_dir_error { dir, err })` / `Io(create_download_dir_error { download_dir, err })` |
/// | `Io`: `RemovePart` / `RemoveMarker` | `Io(remove_temp_error { partial_path, err })` |
/// | `Io`: `WriteIdentity` | `Io(write_error { path, err })` |
/// | `Io`: `RemoveDir` (never reached: the resolver removes no package) | `Io(remove_temp_error { partial_path, err })` |
/// | `Publish` | `Io(publish_member_error { from, to, err })` |
/// | `SyncDir` | `Io(save_error { path: dir, err })` |
/// | `WriteMarker`: temp write / rename | `Io(write_error { path: marker, err })` |
/// | `WriteMarker`: directory fsync | `Io(save_error { path: dir, err })` |
/// | `InvalidMemberPath` / `InvalidFileName` / `DuplicateFileName` (the last two never reached: manifest names are flattened, plain and distinct) | `Extraction(invalid_member_path { member })` |
fn map_package_error(err: PackageError) -> OrtRuntimeError {
    match err {
        PackageError::Cancelled => OrtRuntimeError::Cancelled,
        PackageError::ChecksumMismatch { url: _, expected, actual } => OrtRuntimeError::ChecksumMismatch { expected, actual },
        PackageError::Download { url, source } => map_download_error(&url, *source),
        PackageError::Unpack { archive: _, source } => map_unpack_error(*source),
        PackageError::Io { op, path, source: err } => OrtRuntimeError::Io(match op {
            PackageIoOp::CreateDir => tf!("onnx_runtime.download.create_dir_error", dir = path.display(), err = err),
            PackageIoOp::CreateDownloadDir => tf!("onnx_runtime.download.create_download_dir_error", download_dir = path.display(), err = err),
            PackageIoOp::RemovePart | PackageIoOp::RemoveMarker | PackageIoOp::RemoveDir => {
                tf!("onnx_runtime.download.remove_temp_error", partial_path = path.display(), err = err)
            }
            PackageIoOp::WriteIdentity => tf!("onnx_runtime.download.write_error", path = path.display(), err = err),
        }),
        PackageError::Publish { from, to, source: err } => {
            OrtRuntimeError::Io(tf!("onnx_runtime.download.publish_member_error", from = from.display(), to = to.display(), err = err))
        }
        PackageError::SyncDir { dir, source: err } => OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = dir.display(), err = err)),
        PackageError::WriteMarker { path, dir, source: err } => match *err {
            ms_atomic_file::AtomicWriteError::TempWrite { .. } | ms_atomic_file::AtomicWriteError::Rename { .. } => {
                OrtRuntimeError::Io(tf!("onnx_runtime.download.write_error", path = path.display(), err = err))
            }
            ms_atomic_file::AtomicWriteError::DirSync { .. } => OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = dir.display(), err = err)),
        },
        PackageError::InvalidMemberPath { member } | PackageError::InvalidFileName { name: member } | PackageError::DuplicateFileName { name: member } => {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.invalid_member_path", member = member))
        }
    }
}

/// The `Unpack` rows of [`map_package_error`].
fn map_unpack_error(err: UnpackError) -> OrtRuntimeError {
    match err {
        UnpackError::OpenArchive { path, source: err } => {
            OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = path.display(), err = err))
        }
        UnpackError::ZipRead { source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_read_error", err = err)),
        UnpackError::ZipEntry { index, source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_entry_error", index = index, err = err)),
        UnpackError::ZstdOpen { source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zstd_open_error", err = err)),
        UnpackError::TarRead { source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_read_error", err = err)),
        UnpackError::TarEntry { source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_entry_error", err = err)),
        UnpackError::TarInvalidPath { source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_invalid_path", err = err)),
        UnpackError::CreateDir { path, source: err } => OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_dir_error", parent = path.display(), err = err)),
        UnpackError::CreateFile { path, source: err } => OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_file_error", out_path = path.display(), err = err)),
        UnpackError::CopyMember { path, source: err } => OrtRuntimeError::Extraction(tf!("onnx_runtime.download.extract_element_error", out_path = path.display(), err = err)),
        UnpackError::SyncFile { path, source: err } => OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = path.display(), err = err)),
        UnpackError::MemberMissing { member } => OrtRuntimeError::DylibNotFoundInArchive(member),
    }
}

/// File name of a build cache directory's completion marker.
const CACHE_MARKER: &str = ".ort-cache-complete";

/// First line of the completion marker: bump it when the marker's meaning changes.
const CACHE_MARKER_FORMAT: &str = "ms-onnx-runtime cache v1";

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
    use std::collections::HashSet;
    use std::fs;
    use std::io::Write;

    use ms_download::busy::BusyGuard;
    use sha2::Digest;

    // Golden, independent writers of the `ms-onnx-runtime cache v1` completion marker (the
    // format a cache written by any earlier program version carries). The flow tests build
    // markers with them, so a change of the format in `ms-native-package` fails here.

    /// One library recorded in the completion marker.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PublishedMember {
        file_name: String,
        size: u64,
        sha256: String,
    }

    /// The recipe header of the marker for `entry`: format, build, version and every source's
    /// url + sha256 in fetch order (additional sources first).
    fn cache_recipe(build: &str, entry: &manifest::ManifestEntry) -> String {
        let mut recipe = format!("ms-onnx-runtime cache v1\nbuild {build}\nversion {}\n", entry.version);
        for source in &entry.additional_sources {
            recipe.push_str(&format!("source {} {}\n", source.url, source.sha256));
        }
        recipe.push_str(&format!("source {} {}\n", entry.url, entry.sha256));
        recipe
    }

    /// The full marker: the recipe followed by one `member <name> <size> <sha256>` line per library.
    fn cache_marker_text(recipe: &str, members: &[PublishedMember]) -> String {
        let mut text = recipe.to_owned();
        for member in members {
            text.push_str(&format!("member {} {} {}\n", member.file_name, member.size, member.sha256));
        }
        text
    }

    /// Writes `text` as the completion marker of `dir`.
    fn write_cache_marker(dir: &Path, text: &str) -> std::io::Result<()> {
        fs::write(dir.join(CACHE_MARKER), text)
    }

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
            "dxil.dll"
        );
        // A root path has no filename component (a trailing slash, by contrast, is
        // stripped by `Path`, so `"bin/x64/"` would still yield `"x64"`).
        assert!(matches!(
            member_file_name("/"),
            Err(OrtRuntimeError::Extraction(_))
        ));
    }

    #[test]
    fn package_view_covers_primary_extras_and_additional_sources() {
        // Deserialize a multi-source entry and confirm the package view demands every
        // library filename (primary + extras + additional-source members), fetches the
        // additional source first and publishes the primary library last.
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
        let (names, archives, fields, last) = with_package("example", &entry, dir, |layout, recipe| {
            assert_eq!(layout.dir, dir);
            assert_eq!((layout.marker_file, layout.marker_format), (CACHE_MARKER, CACHE_MARKER_FORMAT));
            let names: HashSet<String> = recipe.sources.iter().flat_map(|source| source.members.iter().map(|member| member.file_name.to_owned())).collect();
            let archives: Vec<String> = recipe.sources.iter().map(|source| source.file_name.to_owned()).collect();
            let fields: Vec<(String, String)> = recipe.fields.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();
            let last = recipe.sources.last().and_then(|source| source.members.last()).map(|member| (member.archive_path.to_owned(), member.file_name.to_owned()));
            (names, archives, fields, last)
        })
        .expect("package view");
        assert_eq!(
            names,
            HashSet::from([
                expected_primary_dylib_filename("9.9.9"),
                "side.dll".to_string(),
                "dxil.dll".to_string(),
            ])
        );
        assert_eq!(archives, vec!["extra.zip".to_owned(), "primary.zip".to_owned()]);
        assert_eq!(fields, vec![("build".to_owned(), "example".to_owned()), ("version".to_owned(), "9.9.9".to_owned())]);
        assert_eq!(last, Some(("lib/primary.dll".to_owned(), expected_primary_dylib_filename("9.9.9"))));
    }

    /// Every `tf!` / `t!` call site of an `onnx_runtime.*` key in this file, as
    /// `(key, supplied argument names)`, read from the real source text (balanced
    /// parentheses, string literals skipped), so the table cannot drift from the code.
    fn localized_call_sites(source: &str) -> Vec<(String, Vec<String>)> {
        let mut sites = Vec::new();
        for (macro_name, takes_args) in [("tf!(", true), ("t!(", false)] {
            let mut rest = source;
            while let Some(found) = rest.find(macro_name) {
                // `t!(` also matches the tail of `tf!(`'s neighbours like `wait!(`: require a
                // non-identifier character before the macro name.
                let before = rest[..found].chars().next_back();
                let body_start = found + macro_name.len();
                rest = &rest[body_start..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let args = split_top_level_args(rest);
                let Some(key) = args.first().and_then(|arg| arg.trim().strip_prefix('"')).and_then(|arg| arg.strip_suffix('"')) else { continue };
                if !key.starts_with("onnx_runtime.") {
                    continue;
                }
                let names: Vec<String> = if takes_args {
                    args[1..].iter().filter_map(|arg| arg.split_once('=')).map(|(name, _)| name.trim().to_owned()).filter(|name| !name.is_empty()).collect()
                } else {
                    Vec::new()
                };
                sites.push((key.to_owned(), names));
            }
        }
        sites
    }

    /// The top-level comma-separated arguments of a macro call whose body starts at `text`
    /// (just after the opening parenthesis), up to the matching closing parenthesis.
    fn split_top_level_args(text: &str) -> Vec<String> {
        let mut args = vec![String::new()];
        let mut depth = 0_u32;
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    let current = args.last_mut().expect("args never empty");
                    current.push(c);
                    while let Some(inner) = chars.next() {
                        current.push(inner);
                        if inner == '\\' {
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        } else if inner == '"' {
                            break;
                        }
                    }
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    args.last_mut().expect("args never empty").push(c);
                }
                ')' | ']' | '}' if depth == 0 => break,
                ')' | ']' | '}' => {
                    depth -= 1;
                    args.last_mut().expect("args never empty").push(c);
                }
                ',' if depth == 0 => args.push(String::new()),
                _ => args.last_mut().expect("args never empty").push(c),
            }
        }
        args.retain(|arg| !arg.trim().is_empty());
        args
    }

    /// The `{name}` placeholders of a catalog text.
    fn placeholders(text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('}') else { break };
            let name = &rest[..close];
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                names.push(name.to_owned());
            }
            rest = &rest[close + 1..];
        }
        names
    }

    /// The placeholder contract of every localized text this crate renders: each call site
    /// supplies EXACTLY the placeholders the catalog text of its key contains, in both required
    /// locales (`en`, `ru`). Independent of rendering (test binaries have no active catalog, so
    /// `tf!` there renders the bare key): the call sites come from this file's source, the
    /// texts from the tracked catalogs.
    #[test]
    fn localized_call_sites_supply_exactly_the_catalog_placeholders() {
        let sites = localized_call_sites(include_str!("lib.rs"));
        let locales = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ms-i18n/locales");
        for locale in ["en", "ru"] {
            let path = locales.join(format!("{locale}.json"));
            let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            let catalog: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).expect("catalog is a JSON object");
            for (key, supplied) in &sites {
                let template = catalog.get(key).and_then(serde_json::Value::as_str).unwrap_or_else(|| panic!("{locale}: key {key} missing"));
                let mut expected = placeholders(template);
                expected.sort();
                expected.dedup();
                let mut supplied = supplied.clone();
                supplied.sort();
                supplied.dedup();
                assert_eq!(supplied, expected, "{locale}: call site of {key} vs catalog text {template:?}");
            }
            // Every `onnx_runtime.download.*` / `onnx_runtime.error.*` text of the catalog has a call site here.
            for key in catalog.keys().filter(|key| key.starts_with("onnx_runtime.download.") || key.starts_with("onnx_runtime.error.")) {
                assert!(sites.iter().any(|(site, _)| site == key), "{locale}: no call site found for {key}");
            }
        }
        // Sanity: the scan sees the production mapping (e.g. the publish frame's three arguments).
        assert!(sites.iter().any(|(key, names)| key == "onnx_runtime.download.publish_member_error" && names.len() == 3), "{sites:?}");
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

        /// Compile-time guard of `map_package_error_covers_every_variant`: a new
        /// `PackageError`, `PackageIoOp` or `UnpackError` variant breaks this match until
        /// the table below gains its row.
        fn table_row_name(err: &PackageError) -> &'static str {
            match err {
                PackageError::Cancelled => "cancelled",
                PackageError::ChecksumMismatch { .. } => "checksum",
                PackageError::Download { source, .. } if matches!(source.as_ref(), DownloadError::Io { .. }) => "download-io",
                PackageError::Download { .. } => "download",
                PackageError::Unpack { source, .. } => match source.as_ref() {
                    UnpackError::OpenArchive { .. } => "unpack-open",
                    UnpackError::ZipRead { .. } => "unpack-zip-read",
                    UnpackError::ZipEntry { .. } => "unpack-zip-entry",
                    UnpackError::ZstdOpen { .. } => "unpack-zstd",
                    UnpackError::TarRead { .. } => "unpack-tar-read",
                    UnpackError::TarEntry { .. } => "unpack-tar-entry",
                    UnpackError::TarInvalidPath { .. } => "unpack-tar-path",
                    UnpackError::CreateDir { .. } => "unpack-create-dir",
                    UnpackError::CreateFile { .. } => "unpack-create-file",
                    UnpackError::CopyMember { .. } => "unpack-copy",
                    UnpackError::SyncFile { .. } => "unpack-sync",
                    UnpackError::MemberMissing { .. } => "unpack-missing",
                },
                PackageError::Io { op, .. } => match op {
                    PackageIoOp::CreateDir => "io-create-dir",
                    PackageIoOp::CreateDownloadDir => "io-create-download-dir",
                    PackageIoOp::RemovePart => "io-remove-part",
                    PackageIoOp::WriteIdentity => "io-write-identity",
                    PackageIoOp::RemoveMarker => "io-remove-marker",
                    PackageIoOp::RemoveDir => "io-remove-dir",
                },
                PackageError::Publish { .. } => "publish",
                PackageError::SyncDir { .. } => "sync-dir",
                PackageError::WriteMarker { source, .. } => match source.as_ref() {
                    ms_atomic_file::AtomicWriteError::TempWrite { .. } => "marker-temp",
                    ms_atomic_file::AtomicWriteError::Rename { .. } => "marker-rename",
                    ms_atomic_file::AtomicWriteError::DirSync { .. } => "marker-dir-sync",
                },
                PackageError::InvalidMemberPath { .. } => "invalid-member",
                PackageError::InvalidFileName { .. } => "invalid-name",
                PackageError::DuplicateFileName { .. } => "duplicate-name",
            }
        }

        #[test]
        fn map_package_error_covers_every_variant() {
            let io = || std::io::Error::other("os says no");
            let p = |name: &str| PathBuf::from(format!("/cache/{name}"));
            let dir_sync = || ms_atomic_file::AtomicWriteError::DirSync { dir: p(""), reason: "fsync".to_owned() };
            let unpack = |source: UnpackError| PackageError::Unpack { archive: p(".download/a.zip"), source: Box::new(source) };
            let rows: Vec<(PackageError, OrtRuntimeError)> = vec![
                (PackageError::Cancelled, OrtRuntimeError::Cancelled),
                (
                    PackageError::ChecksumMismatch { url: "u".to_owned(), expected: "e".to_owned(), actual: "a".to_owned() },
                    OrtRuntimeError::ChecksumMismatch { expected: "e".to_owned(), actual: "a".to_owned() },
                ),
                (
                    PackageError::Download { url: "https://x/a.zip".to_owned(), source: Box::new(DownloadError::SizeMismatch { expected: 1, actual: 2 }) },
                    OrtRuntimeError::Download(tf!("onnx_runtime.download.transfer_error", url = "https://x/a.zip", detail = DownloadError::SizeMismatch { expected: 1, actual: 2 })),
                ),
                (
                    PackageError::Download { url: "u".to_owned(), source: Box::new(DownloadError::Io { path: p("x"), source: io() }) },
                    OrtRuntimeError::Io(DownloadError::Io { path: p("x"), source: io() }.to_string()),
                ),
                (
                    unpack(UnpackError::OpenArchive { path: p("a.zip"), source: io() }),
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.open_archive_error", archive_path = p("a.zip").display(), err = io())),
                ),
                (
                    unpack(UnpackError::ZipRead { source: zip::result::ZipError::FileNotFound }),
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_read_error", err = zip::result::ZipError::FileNotFound)),
                ),
                (
                    unpack(UnpackError::ZipEntry { index: 7, source: zip::result::ZipError::FileNotFound }),
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zip_entry_error", index = 7_usize, err = zip::result::ZipError::FileNotFound)),
                ),
                (unpack(UnpackError::ZstdOpen { source: io() }), OrtRuntimeError::Extraction(tf!("onnx_runtime.download.zstd_open_error", err = io()))),
                (unpack(UnpackError::TarRead { source: io() }), OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_read_error", err = io()))),
                (unpack(UnpackError::TarEntry { source: io() }), OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_entry_error", err = io()))),
                (unpack(UnpackError::TarInvalidPath { source: io() }), OrtRuntimeError::Extraction(tf!("onnx_runtime.download.tar_invalid_path", err = io()))),
                (
                    unpack(UnpackError::CreateDir { path: p("d"), source: io() }),
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_dir_error", parent = p("d").display(), err = io())),
                ),
                (
                    unpack(UnpackError::CreateFile { path: p("f.tmp"), source: io() }),
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.extract_create_file_error", out_path = p("f.tmp").display(), err = io())),
                ),
                (
                    unpack(UnpackError::CopyMember { path: p("f.tmp"), source: io() }),
                    OrtRuntimeError::Extraction(tf!("onnx_runtime.download.extract_element_error", out_path = p("f.tmp").display(), err = io())),
                ),
                (
                    unpack(UnpackError::SyncFile { path: p("f.tmp"), source: io() }),
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = p("f.tmp").display(), err = io())),
                ),
                (unpack(UnpackError::MemberMissing { member: "lib/x.so".to_owned() }), OrtRuntimeError::DylibNotFoundInArchive("lib/x.so".to_owned())),
                (
                    PackageError::Io { op: PackageIoOp::CreateDir, path: p(""), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.create_dir_error", dir = p("").display(), err = io())),
                ),
                (
                    PackageError::Io { op: PackageIoOp::CreateDownloadDir, path: p(".download"), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.create_download_dir_error", download_dir = p(".download").display(), err = io())),
                ),
                (
                    PackageError::Io { op: PackageIoOp::RemovePart, path: p("a.part"), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = p("a.part").display(), err = io())),
                ),
                (
                    PackageError::Io { op: PackageIoOp::WriteIdentity, path: p("a.part.id"), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.write_error", path = p("a.part.id").display(), err = io())),
                ),
                (
                    PackageError::Io { op: PackageIoOp::RemoveMarker, path: p(CACHE_MARKER), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = p(CACHE_MARKER).display(), err = io())),
                ),
                (
                    PackageError::Io { op: PackageIoOp::RemoveDir, path: p(""), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.remove_temp_error", partial_path = p("").display(), err = io())),
                ),
                (
                    PackageError::Publish { from: p("x.tmp"), to: p("x"), source: io() },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.publish_member_error", from = p("x.tmp").display(), to = p("x").display(), err = io())),
                ),
                (PackageError::SyncDir { dir: p(""), source: Box::new(dir_sync()) }, OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = p("").display(), err = dir_sync()))),
                (
                    PackageError::WriteMarker {
                        path: p(CACHE_MARKER),
                        dir: p(""),
                        source: Box::new(ms_atomic_file::AtomicWriteError::TempWrite { path: p(".tmp"), reason: "full".to_owned(), kind: std::io::ErrorKind::Other }),
                    },
                    OrtRuntimeError::Io(tf!(
                        "onnx_runtime.download.write_error",
                        path = p(CACHE_MARKER).display(),
                        err = ms_atomic_file::AtomicWriteError::TempWrite { path: p(".tmp"), reason: "full".to_owned(), kind: std::io::ErrorKind::Other }
                    )),
                ),
                (
                    PackageError::WriteMarker {
                        path: p(CACHE_MARKER),
                        dir: p(""),
                        source: Box::new(ms_atomic_file::AtomicWriteError::Rename { path: p(CACHE_MARKER), reason: "busy".to_owned(), kind: std::io::ErrorKind::Other }),
                    },
                    OrtRuntimeError::Io(tf!(
                        "onnx_runtime.download.write_error",
                        path = p(CACHE_MARKER).display(),
                        err = ms_atomic_file::AtomicWriteError::Rename { path: p(CACHE_MARKER), reason: "busy".to_owned(), kind: std::io::ErrorKind::Other }
                    )),
                ),
                (
                    PackageError::WriteMarker { path: p(CACHE_MARKER), dir: p(""), source: Box::new(dir_sync()) },
                    OrtRuntimeError::Io(tf!("onnx_runtime.download.save_error", path = p("").display(), err = dir_sync())),
                ),
                (PackageError::InvalidMemberPath { member: "/".to_owned() }, OrtRuntimeError::Extraction(tf!("onnx_runtime.download.invalid_member_path", member = "/"))),
                (PackageError::InvalidFileName { name: "..".to_owned() }, OrtRuntimeError::Extraction(tf!("onnx_runtime.download.invalid_member_path", member = ".."))),
                (PackageError::DuplicateFileName { name: "a.dll".to_owned() }, OrtRuntimeError::Extraction(tf!("onnx_runtime.download.invalid_member_path", member = "a.dll"))),
            ];
            let names: HashSet<&str> = rows.iter().map(|(err, _)| table_row_name(err)).collect();
            assert_eq!(names.len(), rows.len(), "every row covers a distinct variant");
            assert_eq!(names.len(), 30, "one row per PackageError / PackageIoOp / UnpackError / marker-write variant (+ Download Io)");
            for (err, expected) in rows {
                let name = table_row_name(&err);
                assert_eq!(format!("{:?}", map_package_error(err)), format!("{expected:?}"), "row {name}");
            }
        }

        #[test]
        fn download_errors_map_to_typed_variants() {
            assert!(matches!(map_download_error("u", DownloadError::Cancelled), OrtRuntimeError::Cancelled));
            assert!(matches!(map_download_error("u", DownloadError::HashMismatch { expected: "a".into(), actual: "b".into() }), OrtRuntimeError::ChecksumMismatch { .. }));
            assert!(matches!(map_download_error("u", DownloadError::SizeMismatch { expected: 1, actual: 2 }), OrtRuntimeError::Download(_)));
            assert!(matches!(map_download_error("u", DownloadError::Archive { detail: "x".into() }), OrtRuntimeError::Extraction(_)));
        }

        // ---- Characterization (golden) tests: they pin the on-disk bytes, the progress
        // sequence, the busy key and the localized error keys of the resolver, so the
        // extraction of its transaction into `ms-native-package` is provably behaviour-
        // preserving. Expected texts are spelled out literally (or through the same `tf!`
        // key + placeholders), never through the code under test.

        const PRIMARY_URL: &str = "https://example.invalid/ort-test.zip";
        const EXTRA_URL: &str = "https://example.invalid/ort-extra.zip";

        /// A row with one additional source (the same scripted body under another URL; its
        /// member is the archive's README).
        fn two_source_entry(body: &[u8]) -> manifest::ManifestEntry {
            let sha = sha_hex(body);
            let json = serde_json::json!({
                "os": "linux", "arch": "x86_64", "provider": "cpu", "build": "test",
                "version": VERSION,
                "url": PRIMARY_URL, "sha256": sha, "size": body.len(), "archive": "zip",
                "dylib_member": PRIMARY_MEMBER,
                "extra_members": [SIDE_MEMBER],
                "additional_sources": [
                    { "url": EXTRA_URL, "sha256": sha, "size": body.len(), "archive": "zip", "members": ["pkg/README"] }
                ]
            });
            serde_json::from_value(json).expect("two-source entry parses")
        }

        /// Stages with consecutive duplicates removed (byte-progress repeats collapse).
        fn stage_sequence(updates: &[OrtDownloadProgress]) -> Vec<OrtDownloadStage> {
            let mut stages: Vec<OrtDownloadStage> = updates.iter().map(|update| update.stage).collect();
            stages.dedup();
            stages
        }

        /// Sorted file names inside `dir`.
        fn sorted_names(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir).expect("list dir").map(|item| item.expect("dir entry").file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        }

        #[test]
        fn golden_marker_bytes_two_sources() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, primary) = archive(1000);
            let entry = two_source_entry(&body);
            let fetcher = ScriptedFetcher::new(&body, &[]);
            resolve(&fetcher, &policy(0), &AtomicBool::new(false), "test-golden", &entry, temp.path(), &mut Vec::new()).expect("resolves");
            // Fetch order: additional sources first, the primary archive last.
            assert_eq!(fetcher.urls(), vec![EXTRA_URL.to_owned(), PRIMARY_URL.to_owned()]);
            let sha = sha_hex(&body);
            let expected = format!(
                "ms-onnx-runtime cache v1\nbuild test-golden\nversion 9.9.9\nsource {EXTRA_URL} {sha}\nsource {PRIMARY_URL} {sha}\nmember README 4 {}\nmember side.so 4 {}\nmember libonnxruntime.so.9.9.9 1000 {}\n",
                sha_hex(b"skip"),
                sha_hex(b"side"),
                sha_hex(&primary)
            );
            assert_eq!(fs::read_to_string(temp.path().join(".ort-cache-complete")).expect("marker"), expected);
        }

        #[test]
        fn golden_part_identity_bytes() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1_600_000);
            let entry = entry_for(&body, None);
            let fetcher = ScriptedFetcher::new(&body, &[]);
            let cancel = AtomicBool::new(false);
            let retry = policy(0);
            let env = DownloadEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel };
            let err = resolve_download_entry(&env, "test-identity", &entry, temp.path(), &mut |update| {
                if update.stage == OrtDownloadStage::Downloading && update.downloaded > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .expect_err("cancelled");
            assert!(matches!(err, OrtRuntimeError::Cancelled), "{err:?}");
            let identity = fs::read(download_dir(temp.path()).join("ort-test.zip.part.id")).expect("identity");
            assert_eq!(identity, format!("{PRIMARY_URL}\n{}\n", sha_hex(&body)).into_bytes());
            assert_eq!(sorted_names(&download_dir(temp.path())), vec!["ort-test.zip.part".to_owned(), "ort-test.zip.part.id".to_owned()]);
        }

        #[test]
        fn golden_stage_sequence_fresh_download() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            let mut updates = Vec::new();
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-stages", &entry, temp.path(), &mut updates).expect("resolves");
            assert_eq!(stage_sequence(&updates), vec![OrtDownloadStage::Probing, OrtDownloadStage::Downloading, OrtDownloadStage::Extracting, OrtDownloadStage::Done]);
            // Byte counters: Downloading reports staged / pinned size; the others report nothing.
            assert!(updates.iter().filter(|update| update.stage != OrtDownloadStage::Downloading).all(|update| update.downloaded == 0 && update.total.is_none()), "{updates:?}");
            assert_eq!(updates.iter().rev().find(|update| update.stage == OrtDownloadStage::Downloading).map(|update| (update.downloaded, update.total)), Some((chunk_u64(body.len()), Some(chunk_u64(body.len())))));

            // The fast path: Probing then Done, nothing else.
            let mut cached = Vec::new();
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-stages", &entry, temp.path(), &mut cached).expect("cached");
            assert_eq!(stage_sequence(&cached), vec![OrtDownloadStage::Probing, OrtDownloadStage::Done]);
        }

        #[test]
        fn golden_stage_sequence_resume_hashes_the_prefix() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(4000);
            let entry = entry_for(&body, None);
            resolve(&ScriptedFetcher::new(&body, &[Step::Drop(1500)]), &policy(0), &AtomicBool::new(false), "test-stages-resume", &entry, temp.path(), &mut Vec::new()).expect_err("dropped");
            let mut updates = Vec::new();
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-stages-resume", &entry, temp.path(), &mut updates).expect("resumes");
            assert_eq!(
                stage_sequence(&updates),
                vec![OrtDownloadStage::Probing, OrtDownloadStage::Verifying, OrtDownloadStage::Downloading, OrtDownloadStage::Extracting, OrtDownloadStage::Done]
            );
            assert!(updates.iter().any(|update| update.stage == OrtDownloadStage::Verifying && update.downloaded == 1500 && update.total == Some(1500)), "{updates:?}");
        }

        #[test]
        fn golden_cache_listing_after_success() {
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let entry = two_source_entry(&body);
            resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-listing", &entry, temp.path(), &mut Vec::new()).expect("resolves");
            assert_eq!(
                sorted_names(temp.path()),
                vec![".download".to_owned(), ".ort-cache-complete".to_owned(), "README".to_owned(), "libonnxruntime.so.9.9.9".to_owned(), "side.so".to_owned()]
            );
            assert!(sorted_names(&download_dir(temp.path())).is_empty());
        }

        #[test]
        fn busy_key_is_ort_build_version() {
            let temp = tempfile::tempdir().expect("temp dir");
            let dir = temp.path().to_path_buf();
            let (body, _) = archive(1000);
            let entry = entry_for(&body, None);
            let holder = BusyGuard::try_acquire("ort:test-busy-key:9.9.9").expect("key free");
            let cancel = std::sync::Arc::new(AtomicBool::new(false));
            let (stage_tx, stage_rx) = std::sync::mpsc::channel();
            let worker_cancel = std::sync::Arc::clone(&cancel);
            let worker = std::thread::spawn(move || {
                let fetcher = ScriptedFetcher::new(&body, &[]);
                let retry = policy(0);
                let env = DownloadEnv { fetcher: &fetcher, retry: &retry, cancel: &worker_cancel };
                // A send fails only once the test stopped listening; the stage is then moot.
                let result = resolve_download_entry(&env, "test-busy-key", &entry, &dir, &mut |update| {
                    stage_tx.send(update.stage).ok();
                });
                (result, fetcher.offsets())
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let stage = stage_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect("worker reports waiting");
                if stage == OrtDownloadStage::WaitingForOtherDownload {
                    break;
                }
            }
            cancel.store(true, Ordering::Relaxed);
            let (result, offsets) = worker.join().expect("worker thread");
            assert!(matches!(result, Err(OrtRuntimeError::Cancelled)), "{result:?}");
            assert!(offsets.is_empty(), "no request while the key was held");
            drop(holder);
        }

        /// Asserts that `text` is `template` rendered with one unknown `detail` part that
        /// contains `needle` (`template` renders the sentinel `@@DETAIL@@` in its place). A
        /// test binary has no active catalog, so `tf!` renders the bare key (no placeholders):
        /// then `text` must be exactly that key.
        fn assert_frame(text: &str, template: &str, needle: &str) {
            let Some((prefix, suffix)) = template.split_once("@@DETAIL@@") else {
                assert_eq!(text, template);
                return;
            };
            assert!(text.starts_with(prefix) && text.ends_with(suffix), "{text:?} does not match frame {template:?}");
            let middle = &text[prefix.len()..text.len() - suffix.len()];
            assert!(middle.contains(needle), "{middle:?} lacks {needle:?}");
        }

        #[test]
        fn error_mapping_golden() {
            // No manifest row: typed, before any I/O.
            let err = resolve_or_download_ort_dylib("no-such-build", &AtomicBool::new(false), &mut |_| {}).expect_err("no row");
            assert!(matches!(&err, OrtRuntimeError::NoManifestEntry { build, .. } if build == "no-such-build"), "{err:?}");
            let (os, arch) = manifest::current_platform();
            assert_eq!(err.to_string(), tf!("onnx_runtime.error.no_manifest_entry", os = os, arch = arch, build = "no-such-build"));

            // Hash mismatch.
            let temp = tempfile::tempdir().expect("temp dir");
            let (body, _) = archive(1000);
            let zeros = "0".repeat(64);
            let err = resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-err-hash", &entry_for(&body, Some(&zeros)), temp.path(), &mut Vec::new()).expect_err("mismatch");
            assert!(matches!(&err, OrtRuntimeError::ChecksumMismatch { expected, actual } if *expected == zeros && *actual == sha_hex(&body)), "{err:?}");
            assert_eq!(err.to_string(), tf!("onnx_runtime.error.checksum_mismatch", expected = zeros, actual = sha_hex(&body)));

            // A wanted member absent from the archive: the first missing one in publish order.
            let temp = tempfile::tempdir().expect("temp dir");
            let mut absent = entry_for(&body, None);
            absent.dylib_member = "pkg/lib/absent.bin".to_owned();
            let err = resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(false), "test-err-member", &absent, temp.path(), &mut Vec::new()).expect_err("member missing");
            assert!(matches!(&err, OrtRuntimeError::DylibNotFoundInArchive(member) if member == "pkg/lib/absent.bin"), "{err:?}");
            assert_eq!(err.to_string(), tf!("onnx_runtime.error.dylib_not_found", member = "pkg/lib/absent.bin"));
            assert!(!temp.path().join("side.so").exists(), "nothing is published when a member is missing");

            // HTTP 404: the transfer frame names the url and the engine's detail.
            let temp = tempfile::tempdir().expect("temp dir");
            let err = resolve(&ScriptedFetcher::new(&body, &[Step::Status(404, None)]), &policy(0), &AtomicBool::new(false), "test-err-404", &entry_for(&body, None), temp.path(), &mut Vec::new()).expect_err("404");
            let OrtRuntimeError::Download(detail) = &err else { panic!("expected Download, got {err:?}") };
            assert_frame(detail, &tf!("onnx_runtime.download.transfer_error", url = PRIMARY_URL, detail = "@@DETAIL@@"), "404");
            assert_eq!(err.to_string(), tf!("onnx_runtime.error.download", detail = detail));

            // Cancel before anything: the busy wait sees the flag.
            let temp = tempfile::tempdir().expect("temp dir");
            let err = resolve(&ScriptedFetcher::new(&body, &[]), &policy(0), &AtomicBool::new(true), "test-err-cancel", &entry_for(&body, None), temp.path(), &mut Vec::new()).expect_err("cancelled");
            assert!(matches!(err, OrtRuntimeError::Cancelled), "{err:?}");
            assert_eq!(err.to_string(), t!("onnx_runtime.error.cancelled"));

            // A verified archive that is not a zip: the zip-read key with the reader's error.
            let temp = tempfile::tempdir().expect("temp dir");
            let junk = b"definitely not a zip archive".to_vec();
            let err = resolve(&ScriptedFetcher::new(&junk, &[]), &policy(0), &AtomicBool::new(false), "test-err-zip", &entry_for(&junk, None), temp.path(), &mut Vec::new()).expect_err("not a zip");
            let zip_err = zip::ZipArchive::new(std::io::Cursor::new(junk.clone())).expect_err("junk is not a zip");
            let expected_detail = tf!("onnx_runtime.download.zip_read_error", err = zip_err);
            assert!(matches!(&err, OrtRuntimeError::Extraction(detail) if *detail == expected_detail), "{err:?}");
            assert_eq!(err.to_string(), tf!("onnx_runtime.error.extraction", detail = expected_detail));
        }
    }
}
