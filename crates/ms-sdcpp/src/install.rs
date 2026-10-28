/*
File: crates/ms-sdcpp/src/install.rs

Purpose:
Installation of one sd.cpp build as a pinned native package: its directory
(`data_dir()/sdcpp/<build>/<tag>/`), the "is it installed" rule, the explicit install
(download + verify + extract, resumable, cancellable) and the removal, all over
`ms-native-package`.

Key items:
- MARKER_FILE / MARKER_FORMAT : the completion marker and its format line
- busy_key()                  : `sdcpp:<build>:<tag>`, one install / remove per build
- package_dir() / is_installed() / install() / remove()
- set_before_remove_hook()    : lets the worker client stop a worker using a build first

Notes:
- Nothing installs implicitly: only `install` downloads, and only when called.
- `remove` never waits for an install: while one of that build runs it refuses with `Busy`.
  It first stops (and reaps) every worker of that build through the before-remove hook and
  aborts when that fails.
- Every entry point does blocking disk (and network) I/O: worker thread only. `install` and
  `remove` ask the status worker to refresh afterwards.
*/

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ms_download::resumable::{RangeFetcher, RetryPolicy, UreqFetcher};
use ms_log::runtime_log;
use ms_native_package::{FetchEnv, PackageError, PackageLayout, PackageMember, PackageProgress, PackageRecipe, PackageSource};
use ms_platform::PlatformFeature;

use crate::catalog::{PINNED, PackageRow, SdCppBuild, row_here};
use crate::error::SdCppError;

/// The completion marker file inside a package directory.
pub const MARKER_FILE: &str = ".sdcpp-package-complete";
/// The marker's format line; bump it when the meaning of an installed package changes.
pub const MARKER_FORMAT: &str = "ms-sdcpp package v1";
/// Log tag of every package line.
const LOG_TAG: &str = "[sdcpp]";

/// The process-wide busy key of `build`'s package (namespace of `ms_download::busy`).
#[must_use]
pub fn busy_key(build: SdCppBuild) -> String {
    format!("sdcpp:{}:{}", build.wire(), PINNED.tag)
}

/// The package directory of `build` under the runtime root `root`:
/// `<root>/sdcpp/<build wire>/<release tag>`.
#[must_use]
pub fn package_dir_in(root: &Path, build: SdCppBuild) -> PathBuf {
    root.join("sdcpp").join(build.wire()).join(PINNED.tag)
}

/// The package directory of `build` (`data_dir()/sdcpp/<build>/<tag>/`), or `None` when the
/// catalog has no row for it on this machine.
#[must_use]
pub fn package_dir(build: SdCppBuild) -> Option<PathBuf> {
    row_here(build).map(|_| package_dir_in(&ms_config::data_dir(), build))
}

/// Owned parts of a row's package recipe (the recipe borrows from them).
struct RowRecipe {
    members: Vec<PackageMember<'static>>,
    fields: [(&'static str, &'static str); 2],
}

impl RowRecipe {
    fn new(row: &'static PackageRow) -> Self {
        Self {
            members: row.members.iter().map(|member| PackageMember { archive_path: member, file_name: member }).collect(),
            fields: [("build", row.build.wire()), ("release", PINNED.tag)],
        }
    }

    /// Runs `body` with the recipe of `row`.
    fn with<T>(&self, row: &'static PackageRow, body: impl FnOnce(&PackageRecipe<'_>) -> T) -> T {
        let sources = [PackageSource { url: row.url, file_name: row.archive_file_name(), size: row.size, sha256: row.sha256, archive: row.archive, members: &self.members }];
        body(&PackageRecipe { fields: &self.fields, sources: &sources })
    }
}

fn layout(dir: &Path) -> PackageLayout<'_> {
    PackageLayout { dir, marker_file: MARKER_FILE, marker_format: MARKER_FORMAT }
}

/// Whether `row`'s package is installed in `dir` (stat-only rule of `ms-native-package`).
fn is_installed_in(dir: &Path, row: &'static PackageRow) -> bool {
    RowRecipe::new(row).with(row, |recipe| ms_native_package::is_installed(&layout(dir), recipe))
}

/// Whether `build` is installed on this machine: the platform has the runtime, the catalog has
/// a row, and its marker matches with every member at its recorded size. Stat-only (no
/// hashing), but disk I/O: worker thread only.
#[must_use]
pub fn is_installed(build: SdCppBuild) -> bool {
    if PlatformFeature::SdCppRuntime.require().is_err() {
        return false;
    }
    row_here(build).is_some_and(|row| is_installed_in(&package_dir_in(&ms_config::data_dir(), build), row))
}

/// Installs `row`'s package into `dir` under the busy key `key` with the given HTTP seam and
/// retry schedule.
fn install_into(fetcher: &dyn RangeFetcher, retry: &RetryPolicy<'_>, cancel: &AtomicBool, dir: &Path, row: &'static PackageRow, key: &str, progress: &mut dyn FnMut(PackageProgress)) -> Result<(), SdCppError> {
    let log_context = format!("build={} release={} dir={}", row.build.wire(), PINNED.tag, dir.display());
    let env = FetchEnv { fetcher, retry, cancel, log_tag: LOG_TAG, log_context: &log_context };
    runtime_log::log_info(format!("{LOG_TAG} installing package ({log_context})"));
    let result = RowRecipe::new(row).with(row, |recipe| ms_native_package::ensure_installed(&env, &layout(dir), recipe, key, progress));
    match result {
        Ok(outcome) => {
            runtime_log::log_info(format!("{LOG_TAG} package ready: {outcome:?} ({log_context})"));
            Ok(())
        }
        Err(err) => {
            log_package_error("install", &err, &log_context);
            Err(SdCppError::from_package_error(&err))
        }
    }
}

/// Downloads, verifies and unpacks `build`'s package (or finds it complete), and returns its
/// directory. Resumable: an interrupted or cancelled run continues from its staged bytes on
/// the next call. Never called implicitly — only on an explicit user action.
///
/// `cancel` stops the busy wait, the transfer and retry waits; `progress` is called on this
/// thread. The status worker is asked to refresh afterwards, whatever the outcome.
///
/// # Errors
/// `Unavailable` (no sd.cpp on this platform), `NoBuildHere` (no catalog row), `Cancelled`,
/// `Package` (download / checksum / unpack / filesystem).
///
/// # Threading
/// Blocking network and disk I/O: worker thread only.
pub fn install(build: SdCppBuild, cancel: &AtomicBool, progress: &mut dyn FnMut(PackageProgress)) -> Result<PathBuf, SdCppError> {
    PlatformFeature::SdCppRuntime.require()?;
    let row = row_here(build).ok_or(SdCppError::NoBuildHere { build })?;
    let dir = package_dir_in(&ms_config::data_dir(), build);
    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    let result = install_into(&fetcher, &retry, cancel, &dir, row, &busy_key(build), progress);
    crate::status::request_refresh();
    result.map(|()| dir)
}

/// The function `remove` calls before touching a package; `Err` aborts the removal.
pub type BeforeRemoveHook = fn(SdCppBuild) -> Result<(), SdCppError>;

/// The hook `remove` calls before touching a package (set once by the worker client).
static BEFORE_REMOVE_HOOK: OnceLock<BeforeRemoveHook> = OnceLock::new();

/// Registers the function `remove` calls first, so the worker client can stop every worker
/// process that has `build`'s libraries loaded. It must return only once none is left, or fail
/// (the removal is then aborted with its error). Set once per process; a second call is logged
/// and ignored.
pub fn set_before_remove_hook(hook: BeforeRemoveHook) {
    if BEFORE_REMOVE_HOOK.set(hook).is_err() {
        runtime_log::log_warn(format!("{LOG_TAG} before-remove hook already set; the second registration is ignored"));
    }
}

/// Removes `row`'s package from `dir`; `before` runs first and its error aborts the removal
/// (nothing touched). Refuses with `Busy` instead of waiting while the busy key `key` is held.
fn remove_from(dir: &Path, row: &'static PackageRow, key: &str, before: Option<BeforeRemoveHook>) -> Result<(), SdCppError> {
    let log_context = format!("build={} release={} dir={}", row.build.wire(), PINNED.tag, dir.display());
    if let Some(hook) = before
        && let Err(err) = hook(row.build)
    {
        runtime_log::log_error(format!("{LOG_TAG} package not removed: its worker could not be stopped: {err:?} ({log_context})"));
        return Err(err);
    }
    // `remove_installed` takes the busy key with `acquire_waiting`, which checks `cancel`
    // before every attempt and sleeps through `retry.sleep` only when the key is held. Making
    // that sleep set `cancel` turns the wait into a single attempt: a held key ends in
    // `Cancelled`, reported as `Busy` (nothing was removed).
    let held = AtomicBool::new(false);
    let mark_held = |_: Duration| held.store(true, Ordering::Relaxed);
    let retry = RetryPolicy { max_retries: 0, first_delay: Duration::ZERO, max_delay: Duration::ZERO, retry_after_cap: Duration::ZERO, wait_slice: Duration::from_millis(1), sleep: &mark_held };
    match ms_native_package::remove_installed(&layout(dir), key, &held, &retry) {
        Ok(()) => {
            runtime_log::log_info(format!("{LOG_TAG} package removed ({log_context})"));
            Ok(())
        }
        Err(PackageError::Cancelled) => {
            runtime_log::log_info(format!("{LOG_TAG} package busy; not removed ({log_context})"));
            Err(SdCppError::Busy)
        }
        Err(err) => {
            log_package_error("remove", &err, &log_context);
            Err(SdCppError::from_package_error(&err))
        }
    }
}

/// Removes `build`'s package: the registered before-remove hook runs first (stopping and
/// reaping every worker that uses it), then the marker is removed durably and the directory
/// deleted. A package that is not there is success. The status worker is asked to refresh
/// afterwards.
///
/// # Errors
/// `NoBuildHere` (no catalog row), `WorkerStillRunning` (a worker could not be stopped;
/// nothing removed), `Busy` (an install of this build is running; nothing removed), `Package`
/// (filesystem).
///
/// # Threading
/// Blocking disk I/O (and whatever the hook does): worker thread only.
pub fn remove(build: SdCppBuild) -> Result<(), SdCppError> {
    let row = row_here(build).ok_or(SdCppError::NoBuildHere { build })?;
    let result = remove_from(&package_dir_in(&ms_config::data_dir(), build), row, &busy_key(build), BEFORE_REMOVE_HOOK.get().copied());
    if result.is_ok() {
        crate::status::forget_probe(build);
    }
    crate::status::request_refresh();
    result
}

/// Logs a package failure with its typed details.
fn log_package_error(operation: &str, err: &PackageError, log_context: &str) {
    match err {
        PackageError::Cancelled => runtime_log::log_info(format!("{LOG_TAG} package {operation} cancelled ({log_context})")),
        PackageError::ChecksumMismatch { .. }
        | PackageError::Download { .. }
        | PackageError::Unpack { .. }
        | PackageError::Io { .. }
        | PackageError::Publish { .. }
        | PackageError::SyncDir { .. }
        | PackageError::WriteMarker { .. }
        | PackageError::InvalidMemberPath { .. }
        | PackageError::InvalidFileName { .. }
        | PackageError::DuplicateFileName { .. } => runtime_log::log_error(format!("{LOG_TAG} package {operation} failed: {err} ({log_context}; {err:?})")),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    use ms_download::busy::BusyGuard;
    use ms_download::test_support::ScriptedFetcher;
    use ms_download::unpack::ArchiveKind;
    use sha2::Digest;
    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

    const MEMBERS: &[&str] = &["libsd.so", "libggml.so.0", "ggml.txt"];

    /// A stored zip with every member of `MEMBERS` plus an unlisted extra file.
    fn fixture_zip() -> Vec<u8> {
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (index, name) in MEMBERS.iter().chain(std::iter::once(&"sd-cli")).enumerate() {
            zip.start_file(*name, opts).expect("start member");
            zip.write_all(format!("body {index} of {name}").as_bytes()).expect("write member");
        }
        zip.finish().expect("finish zip").into_inner()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        sha2::Sha256::digest(bytes).iter().flat_map(|byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0x0f)]]).map(char::from).collect()
    }

    /// A test row pinned to `body`, leaked so it has the catalog's `'static` lifetime.
    fn row_for(body: &[u8], build: SdCppBuild) -> &'static PackageRow {
        let sha: &'static str = Box::leak(sha_hex(body).into_boxed_str());
        Box::leak(Box::new(PackageRow {
            os: "test",
            arch: "test",
            build,
            url: "https://example.invalid/master-1017-be48649/sd-test.zip",
            size: u64::try_from(body.len()).expect("test size fits u64"),
            sha256: sha,
            archive: ArchiveKind::Zip,
            members: MEMBERS,
            sd_library: "libsd.so",
            ggml_library: "libggml.so.0",
            min_vc_runtime: None,
            min_glibc: None,
            needs_vulkan_loader: false,
        }))
    }

    fn no_wait() -> RetryPolicy<'static> {
        RetryPolicy { max_retries: 0, first_delay: Duration::ZERO, max_delay: Duration::ZERO, retry_after_cap: Duration::ZERO, wait_slice: Duration::from_millis(1), sleep: &std::thread::sleep }
    }

    #[test]
    fn package_dir_layout_and_busy_key() {
        let dir = package_dir_in(Path::new("/root"), SdCppBuild::Vulkan);
        assert_eq!(dir, Path::new("/root").join("sdcpp").join("vulkan").join(PINNED.tag));
        assert_eq!(busy_key(SdCppBuild::Cpu), format!("sdcpp:cpu:{}", PINNED.tag));
    }

    #[test]
    fn install_publishes_members_and_is_then_installed() {
        let scratch = tempfile::tempdir().expect("temp dir");
        let dir = scratch.path().join("pkg");
        let body = fixture_zip();
        let row = row_for(&body, SdCppBuild::Cpu);
        let key = "sdcpp-test:install";
        let fetcher = ScriptedFetcher::new(&body, &[]);
        let cancel = AtomicBool::new(false);
        let mut stages = Vec::new();
        assert!(!is_installed_in(&dir, row));
        install_into(&fetcher, &no_wait(), &cancel, &dir, row, key, &mut |update| stages.push(update.stage)).expect("install");
        assert!(is_installed_in(&dir, row));
        for member in MEMBERS {
            assert!(dir.join(member).is_file(), "{member}");
        }
        assert!(!dir.join("sd-cli").exists(), "unlisted members are not installed");
        assert_eq!(stages.last(), Some(&ms_native_package::PackageStage::Done));
        let marker = std::fs::read_to_string(dir.join(MARKER_FILE)).expect("marker");
        assert!(marker.starts_with(&format!("{MARKER_FORMAT}\nbuild cpu\nrelease {}\n", PINNED.tag)), "{marker}");

        // A second install takes the fast path without any request.
        let silent = ScriptedFetcher::new(b"", &[]);
        install_into(&silent, &no_wait(), &cancel, &dir, row, key, &mut |_| {}).expect("fast path");
        assert_eq!(silent.urls(), Vec::<String>::new());
    }

    #[test]
    fn checksum_mismatch_and_cancel_are_typed() {
        let scratch = tempfile::tempdir().expect("temp dir");
        let body = fixture_zip();
        let row = row_for(&body, SdCppBuild::Vulkan);
        let mut tampered = body.clone();
        if let Some(last) = tampered.last_mut() {
            *last ^= 0xFF;
        }
        let cancel = AtomicBool::new(false);
        let err = install_into(&ScriptedFetcher::new(&tampered, &[]), &no_wait(), &cancel, &scratch.path().join("bad"), row, "sdcpp-test:mismatch", &mut |_| {}).expect_err("mismatch");
        assert!(matches!(err, SdCppError::Package { kind: crate::error::PackageFailure::Checksum, .. }), "{err:?}");

        let cancelled = AtomicBool::new(true);
        let err = install_into(&ScriptedFetcher::new(&body, &[]), &no_wait(), &cancelled, &scratch.path().join("cancel"), row, "sdcpp-test:cancel", &mut |_| {}).expect_err("cancel");
        assert_eq!(err, SdCppError::Cancelled);
    }

    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Counts its calls and lets the removal proceed.
    const COUNTING_HOOK: BeforeRemoveHook = |_build| {
        HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };

    /// Refuses, like a worker that could not be stopped.
    const REFUSING_HOOK: BeforeRemoveHook = |_build| Err(SdCppError::WorkerStillRunning);

    #[test]
    fn remove_runs_the_hook_deletes_the_package_and_refuses_while_busy() {
        let scratch = tempfile::tempdir().expect("temp dir");
        let dir = scratch.path().join("pkg");
        let body = fixture_zip();
        let row = row_for(&body, SdCppBuild::Vulkan);
        // A key of its own: parallel tests share the process-wide busy namespace.
        let key = "sdcpp-test:remove";
        let cancel = AtomicBool::new(false);
        install_into(&ScriptedFetcher::new(&body, &[]), &no_wait(), &cancel, &dir, row, key, &mut |_| {}).expect("install");

        {
            let _held = BusyGuard::try_acquire(key).expect("key free");
            assert_eq!(remove_from(&dir, row, key, None), Err(SdCppError::Busy));
            assert!(is_installed_in(&dir, row), "a refused remove touches nothing");
        }

        assert_eq!(remove_from(&dir, row, key, Some(REFUSING_HOOK)), Err(SdCppError::WorkerStillRunning));
        assert!(is_installed_in(&dir, row), "a hook failure aborts the removal");

        let before = HOOK_CALLS.load(Ordering::SeqCst);
        remove_from(&dir, row, key, Some(COUNTING_HOOK)).expect("remove");
        assert_eq!(HOOK_CALLS.load(Ordering::SeqCst), before + 1);
        assert!(!dir.exists());
        assert!(!is_installed_in(&dir, row));
        // Removing what is not there is success.
        remove_from(&dir, row, key, None).expect("remove again");
    }
}
