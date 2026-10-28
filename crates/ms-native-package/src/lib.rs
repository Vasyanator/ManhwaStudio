/*
File: crates/ms-native-package/src/lib.rs

Purpose:
Crate root of `ms-native-package` — the one owner of the "pinned native package" transaction:
a directory holding the files extracted from one or more pinned archives, trusted only through
a completion marker. Download a pinned archive -> verify size + sha256 -> stage the wanted
members -> publish them by rename -> write the completion marker; probe it; remove it.

Key types:
- PackageLayout   : the directory, the marker file name and the marker's format line
- PackageRecipe   : `<key> <value>` fields + the ordered sources (url, pins, members)
- PackageSource / PackageMember : one pinned archive and one member to place
- FetchEnv        : the HTTP seam, retry schedule, cancel flag and log labels of one call
- PackageStage / PackageProgress : progress reported to the caller
- InstallOutcome  : how `ensure_installed` found / made the package
- PackageError / PackageIoOp     : typed failures (`error.rs`)

Key functions:
- flatten_member_name() : the file-name component of an archive member path
- is_installed()        : the stat-only "installed" rule (marker == recipe, sizes match)
- ensure_installed()    : probe -> busy wait -> re-probe -> marker removed -> per source fetch
                          + extract + publish -> marker written atomically
- remove_installed()    : marker removed durably first, then the whole directory

Notes:
GUI-free, no i18n, no config, no threads: the caller runs it on a worker thread, chooses the
directory, the recipe, the busy key and the marker name / format line, and maps
`PackageError` onto its own user-facing text. HTTP, retry, resume and verification are
`ms-download`'s; atomic marker replacement is `ms-atomic-file`'s. Native-only: compiled out on
wasm, like `ms-download`. Consumer: `ms-onnx-runtime` (its cache bytes, `.part.id` bytes and
busy key are this crate's formats).
*/

#![cfg(not(target_arch = "wasm32"))]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate's public names (`PackageError`, `PackageLayout`, …) intentionally repeat the
// crate's subject: they are re-exported and read at call sites without the module path.
#![allow(clippy::module_name_repetitions)]

mod error;
mod fetch;
mod marker;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use ms_download::busy::BusyGuard;
use ms_download::resumable::{RangeFetcher, RetryPolicy};

pub use error::{PackageError, PackageIoOp};
pub use ms_download::unpack::ArchiveKind;

/// Name of the staging directory inside the package directory: `.part` files, their identity
/// sidecars and verified archives awaiting extraction live there.
pub const DOWNLOAD_DIR: &str = ".download";

/// Where a package lives and how its completion marker is named.
#[derive(Debug, Clone, Copy)]
pub struct PackageLayout<'a> {
    /// The package directory (created on install); members are placed directly inside it.
    pub dir: &'a Path,
    /// File name of the completion marker inside `dir` (e.g. `.ort-cache-complete`); must be
    /// one plain path component other than `.download` (checked before any I/O).
    pub marker_file: &'a str,
    /// First line of the marker; bump it when the marker's meaning changes for a consumer.
    pub marker_format: &'a str,
}

/// One member of a source archive to place in the package directory.
#[derive(Debug, Clone, Copy)]
pub struct PackageMember<'a> {
    /// Exact member path inside the archive (tar keys are compared without a leading `./`).
    pub archive_path: &'a str,
    /// Plain file name the member is published under in the package directory (usually
    /// [`flatten_member_name`] of `archive_path`; may differ, e.g. a versioned library name).
    pub file_name: &'a str,
}

/// One pinned archive of a package.
#[derive(Debug, Clone, Copy)]
pub struct PackageSource<'a> {
    /// Download URL.
    pub url: &'a str,
    /// Plain file name the verified archive is staged under in [`DOWNLOAD_DIR`] (its `.part`
    /// and `.part.id` derive from it).
    pub file_name: &'a str,
    /// Pinned exact byte size.
    pub size: u64,
    /// Pinned lowercase hex sha256.
    pub sha256: &'a str,
    /// Packing format.
    pub archive: ArchiveKind,
    /// Members to place, in publication order.
    pub members: &'a [PackageMember<'a>],
}

/// The full recipe of a package. Its marker header is the layout's format line, one
/// `<key> <value>` line per field in order, and one `source <url> <sha256>` line per source in
/// fetch order; member lines follow in publication order (sources in order, members in
/// order, so the LAST member of the LAST source is published last).
#[derive(Debug, Clone, Copy)]
pub struct PackageRecipe<'a> {
    /// Identity fields of the recipe (e.g. `[("build", "cpu"), ("version", "1.27.0")]`); keys
    /// and values must not contain a newline.
    pub fields: &'a [(&'a str, &'a str)],
    /// Sources in fetch order.
    pub sources: &'a [PackageSource<'a>],
}

/// The HTTP seam, retry schedule, cancel flag and log labels one call works with
/// (production: `ms_download::resumable::UreqFetcher` + `RetryPolicy::standard`; tests: a
/// scripted fetcher).
#[derive(Clone, Copy)]
pub struct FetchEnv<'a> {
    /// Every request goes through it.
    pub fetcher: &'a dyn RangeFetcher,
    /// Transient-failure retries; its `wait_slice` also paces the busy wait.
    pub retry: &'a RetryPolicy<'a>,
    /// Polled during the busy wait, the transfer, the backoff and the hash pass.
    pub cancel: &'a AtomicBool,
    /// Log prefix of the consumer, e.g. `[onnx-runtime]`.
    pub log_tag: &'a str,
    /// Identifies the package in log lines (e.g. `build=cpu version=1.27.0`); no secrets.
    pub log_context: &'a str,
}

/// Coarse stage of [`ensure_installed`], reported through its progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageStage {
    /// Checking the completion marker before any network access.
    Probing,
    /// Another task of this process holds the busy key; waiting, then re-probing.
    WaitingForOtherDownload,
    /// Streaming an archive to disk.
    Downloading,
    /// A transient network failure: waiting `delay_secs` (whole seconds, rounded up;
    /// re-reported as it counts down, last with `0`) before retry `attempt` of
    /// `max_attempts` (consecutive retries without progress before giving up).
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64 },
    /// Hashing an already staged prefix or archive against the pinned sha256.
    Verifying,
    /// Staging and publishing the members of a verified archive.
    Extracting,
    /// The package is installed.
    Done,
}

/// Progress snapshot passed to the caller's callback.
///
/// `downloaded` / `total` are meaningful during `Downloading` and `Retrying` (bytes staged /
/// pinned archive size) and `Verifying` (bytes hashed / size of the hashed file); the other
/// stages report `downloaded = 0`, `total = None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageProgress {
    /// Bytes staged (or hashed) so far.
    pub downloaded: u64,
    /// Total size of the current operation, if known.
    pub total: Option<u64>,
    /// Current stage.
    pub stage: PackageStage,
}

/// How [`ensure_installed`] ended successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The package was already complete; no network, nothing written.
    AlreadyInstalled,
    /// Another task of this process completed it while this call waited for the busy key.
    InstalledByOther,
    /// This call downloaded (or repaired from a staged archive) and published it.
    Installed,
}

/// The file-name component of the archive member path `archive_path` (`a/b/lib.so` ->
/// `lib.so`), so a member can never be placed outside the package directory.
///
/// # Errors
/// [`PackageError::InvalidMemberPath`] when the path has no file-name component (`/`, `a/..`).
pub fn flatten_member_name(archive_path: &str) -> Result<&str, PackageError> {
    Path::new(archive_path)
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| PackageError::InvalidMemberPath { member: archive_path.to_owned() })
}

/// Whether the package of `recipe` is installed in `layout.dir`: the completion marker starts
/// with exactly this recipe's header, lists exactly the recipe's member file names, and each
/// is a regular file of the recorded size.
///
/// The ONE "installed" rule (also [`ensure_installed`]'s fast path). Stat-only: a marker read
/// plus one stat per member, no hashing, no network — still disk I/O, so call it off the GUI
/// thread. A layout or recipe with an invalid or colliding file name, and every unreadable
/// state, is `false` (nothing is read outside `layout.dir`).
#[must_use]
pub fn is_installed(layout: &PackageLayout<'_>, recipe: &PackageRecipe<'_>) -> bool {
    if validate(layout, recipe).is_err() {
        return false;
    }
    marker::marker_matches(layout, &marker::recipe_header(layout, recipe), &member_names(recipe), "[native-package]")
}

/// Makes the package of `recipe` installed in `layout.dir` and reports how.
///
/// Steps: validate the file names; report `Probing`; return at once when [`is_installed`].
/// Otherwise take the process-wide busy id `busy_key` (waiting, cancellable, reported once as
/// `WaitingForOtherDownload`), re-probe, create `dir` and `dir/.download`, remove the
/// completion marker durably, then per source in order: claim `.download/<file>.part` (its
/// sidecar `.part.id` = `<url>\n<sha256>\n`; a foreign `.part` is deleted), let
/// `ms_download::fetch_verified` resume / retry / verify (exact size + sha256; a mismatch
/// deletes the `.part`) and publish `.download/<file>`, stage every member, rename them into
/// `dir` in member order, fsync `dir`, delete the archive. Finally the marker is written
/// atomically and `Done` reported. A verified archive left staged by an interrupted run is
/// reused without a request.
///
/// `busy_key` must be unique per package across the whole process (it shares the namespace
/// of `ms_download::busy`; never a `dest:` id). `progress` runs on the calling thread,
/// throttled during transfer and hashing.
///
/// # Threading
/// Blocking network and disk I/O: call it from a worker thread, never the GUI thread.
///
/// # Errors
/// `InvalidFileName` / `DuplicateFileName` (before any I/O), `Cancelled` (staged bytes kept), `ChecksumMismatch`,
/// `Download`, `Unpack`, `Io`, `Publish`, `SyncDir`, `WriteMarker` — see [`PackageError`].
/// After any error the marker is absent (or was never removed, when the failure came before
/// the removal), so the next call repairs the package.
pub fn ensure_installed(
    env: &FetchEnv<'_>,
    layout: &PackageLayout<'_>,
    recipe: &PackageRecipe<'_>,
    busy_key: &str,
    progress: &mut dyn FnMut(PackageProgress),
) -> Result<InstallOutcome, PackageError> {
    validate(layout, recipe)?;
    let header = marker::recipe_header(layout, recipe);
    let names = member_names(recipe);

    emit(progress, PackageStage::Probing, 0, None);
    if marker::marker_matches(layout, &header, &names, env.log_tag) {
        emit(progress, PackageStage::Done, 0, None);
        return Ok(InstallOutcome::AlreadyInstalled);
    }

    // One installation of this package per process: two writers of one `.part` / one set of
    // members would corrupt each other. The second caller waits, then normally finds the
    // package complete below.
    let _busy = BusyGuard::acquire_waiting(busy_key, env.cancel, env.retry, &mut || {
        ms_log::runtime_log::log_info(format!("{} package is being prepared by another task; waiting ({})", env.log_tag, env.log_context));
        emit(progress, PackageStage::WaitingForOtherDownload, 0, None);
    })
    // `acquire_waiting` fails only on cancel; any other error is attributed to the source
    // published last, as the whole-package failure.
    .map_err(|err| PackageError::from_download(recipe.sources.last().map_or("", |source| source.url), err))?;
    if marker::marker_matches(layout, &header, &names, env.log_tag) {
        emit(progress, PackageStage::Done, 0, None);
        return Ok(InstallOutcome::InstalledByOther);
    }

    fs::create_dir_all(layout.dir).map_err(|source| PackageError::Io { op: PackageIoOp::CreateDir, path: layout.dir.to_path_buf(), source })?;
    let download_dir = layout.dir.join(DOWNLOAD_DIR);
    fs::create_dir_all(&download_dir).map_err(|source| PackageError::Io { op: PackageIoOp::CreateDownloadDir, path: download_dir.clone(), source })?;
    // The package is about to be (re)published member by member: drop the marker durably
    // FIRST, so a crash between two member renames leaves no marker and the next call repairs
    // the set instead of trusting a mixed generation.
    marker::remove_marker(layout)?;

    let mut published = Vec::with_capacity(names.len());
    for source in recipe.sources {
        published.extend(fetch::fetch_source(env, source, layout.dir, &download_dir, progress)?);
    }
    // Only now, with every member renamed into place, does the package become trusted.
    marker::write_marker(layout, &marker::marker_text(&header, &published))?;
    emit(progress, PackageStage::Done, 0, None);
    Ok(InstallOutcome::Installed)
}

/// Removes the package in `layout.dir`: under the busy id `busy_key` (waiting, cancellable),
/// the completion marker is removed and the directory fsynced FIRST — so a crash midway
/// leaves an untrusted directory, never a trusted partial one — then the whole directory
/// tree (members, `.download/`, leftovers) is deleted. A missing directory is success.
///
/// # Threading
/// Blocking disk I/O: worker thread only.
///
/// # Errors
/// `InvalidFileName` (the marker name is not one plain component; nothing touched),
/// `Cancelled` (nothing removed), `Download` (a busy-wait failure other than cancel; `url`
/// empty), `Io` (`RemoveMarker` / `RemoveDir`), `SyncDir`.
pub fn remove_installed(layout: &PackageLayout<'_>, busy_key: &str, cancel: &AtomicBool, retry: &RetryPolicy<'_>) -> Result<(), PackageError> {
    validate_layout(layout)?;
    let _busy = BusyGuard::acquire_waiting(busy_key, cancel, retry, &mut || {}).map_err(|err| PackageError::from_download("", err))?;
    if !layout.dir.exists() {
        return Ok(());
    }
    marker::remove_marker(layout)?;
    match fs::remove_dir_all(layout.dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PackageError::Io { op: PackageIoOp::RemoveDir, path: layout.dir.to_path_buf(), source }),
    }
}

/// Every member file name of `recipe`, sources in order, members in order.
fn member_names<'a>(recipe: &PackageRecipe<'a>) -> Vec<&'a str> {
    recipe.sources.iter().flat_map(|source| source.members.iter().map(|member| member.file_name)).collect()
}

/// Whether `name` is one plain path component other than [`DOWNLOAD_DIR`] (not empty, not
/// absolute, no separator, not `.` / `..`).
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != DOWNLOAD_DIR && Path::new(name).file_name() == Some(OsStr::new(name))
}

/// Checks `layout.marker_file`: one plain component, so the marker can never be read, removed
/// or written outside `layout.dir`.
///
/// # Errors
/// [`PackageError::InvalidFileName`] naming the marker.
fn validate_layout(layout: &PackageLayout<'_>) -> Result<(), PackageError> {
    if is_plain_name(layout.marker_file) { Ok(()) } else { Err(PackageError::InvalidFileName { name: layout.marker_file.to_owned() }) }
}

/// Checks the layout ([`validate_layout`]) and every name of `recipe`, so no two files of one
/// installation can share a path:
/// - every member and archive file name is a plain component other than the marker;
/// - archive names are unique across sources, and no archive's staging names
///   (`<name>`, `.part`, `.part.id`, and `fetch_verified`'s `.part.identity`) equal another's;
/// - published member names are unique across ALL sources, and no member is named like
///   another member's staging temporary (`<name>.tmp`).
///
/// # Errors
/// [`PackageError::InvalidFileName`] for the first non-plain name;
/// [`PackageError::DuplicateFileName`] for the first colliding name.
fn validate(layout: &PackageLayout<'_>, recipe: &PackageRecipe<'_>) -> Result<(), PackageError> {
    validate_layout(layout)?;
    let names = recipe.sources.iter().flat_map(|source| std::iter::once(source.file_name).chain(source.members.iter().map(|member| member.file_name)));
    for name in names {
        if !is_plain_name(name) || name == layout.marker_file {
            return Err(PackageError::InvalidFileName { name: name.to_owned() });
        }
    }
    // Every path one source occupies in `.download/` (the names `fetch::fetch_source` and
    // `ms_download::fetch_verified` derive from the archive name).
    let archive_paths = |name: &str| [name.to_owned(), format!("{name}.part"), format!("{name}.part.id"), format!("{name}.part.identity")];
    let mut staging: HashSet<String> = HashSet::new();
    for source in recipe.sources {
        if archive_paths(source.file_name).iter().any(|path| staging.contains(path)) {
            return Err(PackageError::DuplicateFileName { name: source.file_name.to_owned() });
        }
        staging.extend(archive_paths(source.file_name));
    }
    // Every path one member occupies in the package directory (its final name and its
    // `stage_members` temporary).
    let member_paths = |name: &str| [name.to_owned(), format!("{name}.tmp")];
    let mut published: HashSet<String> = HashSet::new();
    for member in recipe.sources.iter().flat_map(|source| source.members) {
        if member_paths(member.file_name).iter().any(|path| published.contains(path)) {
            return Err(PackageError::DuplicateFileName { name: member.file_name.to_owned() });
        }
        published.extend(member_paths(member.file_name));
    }
    Ok(())
}

/// Reports one progress snapshot to the caller's callback.
pub(crate) fn emit(progress: &mut dyn FnMut(PackageProgress), stage: PackageStage, downloaded: u64, total: Option<u64>) {
    progress(PackageProgress { downloaded, total, stage });
}

#[cfg(test)]
mod tests;
