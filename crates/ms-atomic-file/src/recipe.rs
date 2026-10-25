/*
File: crates/ms-atomic-file/src/recipe.rs

Purpose:
The atomic replace recipe: sibling temp, `write_all`, optional `sync_all`, CLOSE the handle,
`rename` over the target (bounded retry on Windows sharing violations), optional directory
fsync; the temp is removed on every failure and the old file is never deleted first.

Key types:
- `Durability` (what is fsynced: nothing, the contents, or contents + directory)
- `AtomicWriteOptions` (durability + parent creation, unix mode, identical-bytes skip,
  unique-per-call temp name)

Key functions:
- write_atomic() / write_atomic_with(): the whole recipe
- write_temp_file() / rename_with_retry() / sync_directory(): its steps, public for multi-step
  commits (validate a staged temp, then rename) and publishes of already-staged files

Notes:
DIRECTORY DURABILITY IS PLATFORM-ASYMMETRIC, deliberately. On Unix the parent directory is
opened and `sync_all`ed, because a `rename` may be durable-in-page-cache only: after a power
loss the new name can be missing while the old content is already gone. On Windows a
directory cannot be flushed at all (`FlushFileBuffers` is not supported for directories, and
`File::open` on a directory fails without `FILE_FLAG_BACKUP_SEMANTICS`), so the step is a
documented no-op there and the rename's durability rests on the filesystem's metadata
journal. Either way the CONTRACT the callers rely on is the same: a caller may delete the
data source it just migrated only after a `ContentsAndDirectory` write returned `Ok`.
WINDOWS SHARING VIOLATIONS: a rename over a file that another process holds open without
`FILE_SHARE_DELETE` (the Python backend's CRT `open`, antivirus scanners, indexers) fails
transiently, where an in-place write would have succeeded. The rename is therefore retried a
bounded number of times on those error codes only (Windows only; on other targets a failed
rename is final). The temp file is removed after the final failure.
wasm32: std's file system is unsupported there, so the first file operation fails with kind
`Unsupported` and the call returns `AtomicWriteError::TempWrite`; the retry sleep is never
reached (no error is transient off Windows).
*/

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::AtomicWriteError;
use crate::naming::{temp_path_for, unique_temp_path_for};
use crate::test_hooks::{FaultPoint, WriteStep, injected_fault, record_step};

/// How durable a replacement must be before a write returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// Nothing is fsynced: still an atomic temp + rename (a crash never tears the file),
    /// but a crash shortly after the call may lose THIS write and leave the previous
    /// version. For callers on the GUI thread, which must never wait for a disk flush
    /// (CLAUDE.md §5); never for a write after which an old copy is deleted.
    None,
    /// Only the new CONTENTS are fsynced. Enough for a file that is rewritten by the
    /// next mutation anyway and whose loss costs at most one cached value.
    #[default]
    Contents,
    /// The contents AND the containing directory (Unix; see the file header for Windows).
    /// Required whenever the caller DELETES the data's previous home after this returns:
    /// without it a power loss can leave neither the old source nor the new file.
    ContentsAndDirectory,
}

/// Per-call choices of [`write_atomic_with`]. [`AtomicWriteOptions::new`] is exactly the
/// [`write_atomic`] recipe; callers override single fields with struct-update syntax
/// (`AtomicWriteOptions { create_parent: true, ..AtomicWriteOptions::new(durability) }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtomicWriteOptions {
    /// What is fsynced before the call returns.
    pub durability: Durability,
    /// Create the parent directory (and its ancestors) first. Off: a missing parent fails at
    /// the temp step.
    pub create_parent: bool,
    /// Unix permission bits of the new file (e.g. `0o600` for a secret). Applied when the temp
    /// is created AND forced on its handle before any byte is written (a leftover temp of a
    /// crashed run keeps its old mode otherwise; `set_permissions` also ignores the umask).
    /// `None`: default permissions of a new file. Ignored on non-Unix targets.
    pub unix_mode: Option<u32>,
    /// When the file already holds exactly these bytes (and, with `unix_mode`, exactly that
    /// mode), nothing is rewritten; only `durability` is applied to the existing file.
    pub skip_if_identical: bool,
    /// Use a temp name unique to this call (`.{name}.{pid}.{serial}.tmp`, opened with
    /// `create_new`) instead of the per-process [`temp_path_for`] name. Needed when two
    /// threads of one process may write the same target without a lock of their own.
    pub unique_temp: bool,
}

impl AtomicWriteOptions {
    /// The [`write_atomic`] recipe: the given durability, an existing parent, default
    /// permissions, the identical-bytes skip on, the per-process temp name.
    #[must_use]
    pub const fn new(durability: Durability) -> Self {
        Self { durability, create_parent: false, unix_mode: None, skip_if_identical: true, unique_temp: false }
    }
}

impl Default for AtomicWriteOptions {
    /// [`AtomicWriteOptions::new`] with the default [`Durability::Contents`].
    fn default() -> Self {
        Self::new(Durability::default())
    }
}

/// How many times a rename that failed with a transient sharing violation is attempted in
/// total (Windows only; see the file header). With [`RENAME_RETRY_BACKOFF`] the worst case
/// adds about 200 ms before the error is reported.
const RENAME_ATTEMPTS: u32 = 10;

/// Pause between two rename attempts.
const RENAME_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Atomically replaces `path` with `bytes`: [`write_atomic_with`] with
/// [`AtomicWriteOptions::new`]`(durability)`.
///
/// Contract:
/// - `path` names a file whose parent directory already EXISTS; nothing is created. A
///   missing parent fails at the temp step and leaves no temp file behind.
/// - The old file is never deleted before the rename; on every error it is intact (except
///   [`AtomicWriteError::DirSync`], where the new bytes are already in place) and the temp
///   file has been removed.
/// - The rename replaces the DIRECTORY ENTRY at `path`: a symlink there is replaced by a
///   regular file, not written through. A caller that must update a symlink's target
///   canonicalizes `path` first.
/// - When the file already holds exactly `bytes`, nothing is rewritten (its mtime is not
///   bumped); only `durability` is applied to the existing file.
/// - The new file gets default permissions for a newly created file, not the old file's.
/// - Within one process, writers of the same `path` must be serialized by the caller (the
///   temp name is per process); otherwise use `unique_temp`.
/// - Blocking I/O (fsync): never call it on the GUI thread unless `durability` is `None`.
///   Nothing is logged here; the caller logs the error with its own context.
///
/// # Errors
/// [`AtomicWriteError::TempWrite`] (temp could not be created/written/fsynced, including a
/// missing parent; kind `Unsupported` on wasm32), [`AtomicWriteError::Rename`] (e.g. `path`
/// is a directory), [`AtomicWriteError::DirSync`] (only with
/// [`Durability::ContentsAndDirectory`]).
pub fn write_atomic(path: &Path, bytes: &[u8], durability: Durability) -> Result<(), AtomicWriteError> {
    write_atomic_with(path, bytes, &AtomicWriteOptions::new(durability))
}

/// Atomically replaces `path` with `bytes` under `options`.
///
/// The recipe: optionally create the parent; optionally keep an identical existing file
/// (see [`keep_identical_existing`]); write a sibling temp file in the SAME directory (so the
/// rename never crosses a filesystem boundary), `write_all` + `sync_all` it (no fsync with
/// [`Durability::None`]), CLOSE the handle, then `rename` it over the target — an atomic
/// replace on both Unix (`rename(2)`) and Windows (`MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`), retried on Windows sharing violations. With
/// [`Durability::ContentsAndDirectory`] the containing directory is fsynced afterwards.
///
/// The handle is dropped BEFORE any cleanup or rename: deleting a file that is still open
/// fails on Windows, which would leave the orphaned temp behind on every failed write.
///
/// Every contract point of [`write_atomic`] holds, with the options' changes: a missing
/// parent is created with `create_parent`, `unix_mode` sets the new file's mode,
/// `skip_if_identical: false` always rewrites, `unique_temp` makes concurrent in-process
/// writers safe (last rename wins).
///
/// # Errors
/// As [`write_atomic`]; a failed parent creation is [`AtomicWriteError::TempWrite`] naming
/// the directory.
pub fn write_atomic_with(path: &Path, bytes: &[u8], options: &AtomicWriteOptions) -> Result<(), AtomicWriteError> {
    if options.create_parent
        && let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty())
    {
        fs::create_dir_all(dir).map_err(|err| AtomicWriteError::TempWrite { path: dir.to_path_buf(), reason: format!("cannot create directory {}: {err}", dir.display()), kind: err.kind() })?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if options.skip_if_identical && keep_identical_existing(path, parent, bytes, options) {
        return Ok(());
    }
    let temp = if options.unique_temp { unique_temp_path_for(path) } else { temp_path_for(path) };
    let create = TempCreate { create_new: options.unique_temp, unix_mode: options.unix_mode };

    if let Err(err) = write_temp_file_as(&temp, bytes, options.durability != Durability::None, create) {
        // The handle is already closed (see `write_temp_file_as`), so this cleanup can also
        // succeed on Windows. A failed cleanup cannot mask the real error — CLAUDE.md §7.
        remove_orphan_temp(&temp);
        return Err(err);
    }

    if let Err(err) = rename_with_retry(&temp, path) {
        remove_orphan_temp(&temp);
        return Err(AtomicWriteError::Rename { path: path.to_path_buf(), reason: err.to_string(), kind: err.kind() });
    }
    record_step(path, WriteStep::Renamed);

    match options.durability {
        Durability::None | Durability::Contents => Ok(()),
        Durability::ContentsAndDirectory => {
            sync_directory(parent)?;
            record_step(path, WriteStep::DirectoryDurable);
            Ok(())
        }
    }
}

/// The "identical bytes" shortcut of [`write_atomic_with`]: when `path` already holds exactly
/// `contents` (and, with `options.unix_mode` on Unix, exactly that mode), the replacement is a
/// no-op for the file, so no temp file, fsync or rename is spent on it (an unchanged flush
/// then costs one `stat` + one read from the page cache). The durability the caller asked for
/// is still honoured on the EXISTING file — it may itself come from an earlier
/// `Durability::None` write: `Contents` fsyncs it and `ContentsAndDirectory` also fsyncs the
/// directory. Returns `true` when the existing file now satisfies the write.
///
/// Every failure here (unreadable file, a Windows sharing violation on the write handle
/// `FlushFileBuffers` needs, a failed fsync) returns `false`, so the caller runs the full
/// recipe, which is then the one to report a real error. Never an error of its own.
fn keep_identical_existing(path: &Path, parent: &Path, contents: &[u8], options: &AtomicWriteOptions) -> bool {
    // Length first: a changed file almost always changes length, so the common "different"
    // case costs one stat and no read.
    let same_shape = fs::metadata(path).is_ok_and(|meta| meta.is_file() && u64::try_from(contents.len()).is_ok_and(|len| len == meta.len()) && mode_matches(&meta, options.unix_mode));
    if !same_shape || fs::read(path).map_or(true, |existing| existing != contents) {
        return false;
    }
    let durability = options.durability;
    let synced = match durability {
        Durability::None => true,
        // A WRITE handle: Windows `FlushFileBuffers` refuses a read-only one. No truncation.
        Durability::Contents | Durability::ContentsAndDirectory => fs::OpenOptions::new().write(true).open(path).and_then(|file| file.sync_all()).is_ok(),
    };
    if !synced {
        return false;
    }
    if durability == Durability::ContentsAndDirectory {
        if sync_directory(parent).is_err() {
            return false;
        }
        record_step(path, WriteStep::DirectoryDurable);
    }
    true
}

/// Whether the existing file's permission bits equal the requested `unix_mode` (always true
/// when none is requested).
#[cfg(unix)]
fn mode_matches(meta: &fs::Metadata, unix_mode: Option<u32>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    unix_mode.is_none_or(|mode| meta.permissions().mode() & 0o7777 == mode & 0o7777)
}

/// Non-Unix: there is no mode to compare (`unix_mode` is ignored).
#[cfg(not(unix))]
fn mode_matches(_meta: &fs::Metadata, _unix_mode: Option<u32>) -> bool {
    true
}

/// `fs::rename(from, to)`, retried on Windows sharing violations (`ERROR_ACCESS_DENIED`,
/// `ERROR_SHARING_VIOLATION`, `ERROR_LOCK_VIOLATION`: up to 10 attempts 20 ms apart); on other
/// targets a failed rename is final. Replaces an existing `to`; never deletes it first. The
/// caller cleans up `from` on failure.
///
/// # Errors
/// The I/O error of the last attempt.
pub fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_transient(RENAME_ATTEMPTS, RENAME_RETRY_BACKOFF, || fs::rename(from, to), is_transient_sharing_error)
}

/// How the temp file is opened.
#[derive(Debug, Clone, Copy)]
struct TempCreate {
    /// `create_new` (fail on an existing file) instead of create-or-truncate.
    create_new: bool,
    /// Unix mode set at creation and forced on the handle (see `AtomicWriteOptions`).
    #[cfg_attr(not(unix), expect(dead_code, reason = "Unix permission bits: carried on every target so callers stay cfg-free, read only on Unix"))]
    unix_mode: Option<u32>,
}

/// Creates (or truncates) `temp`, writes `contents` into it, fsyncs it when `sync` is set and
/// CLOSES it. The handle is dropped before this returns in every path, success or failure, so
/// the caller may delete or rename the file immediately (both fail on an open handle under
/// Windows). It does NOT remove `temp` on failure: the caller owns the cleanup (a staged temp
/// may carry companions such as an `SQLite` journal).
///
/// # Errors
/// [`AtomicWriteError::TempWrite`] naming `temp`, with the I/O error's kind.
pub fn write_temp_file(temp: &Path, contents: &[u8], sync: bool) -> Result<(), AtomicWriteError> {
    write_temp_file_as(temp, contents, sync, TempCreate { create_new: false, unix_mode: None })
}

/// [`write_temp_file`] with the open mode of `create`.
fn write_temp_file_as(temp: &Path, contents: &[u8], sync: bool, create: TempCreate) -> Result<(), AtomicWriteError> {
    let temp_error = |kind, reason| AtomicWriteError::TempWrite { path: temp.to_path_buf(), reason, kind };
    let mut file = open_temp(temp, create).map_err(|err| temp_error(err.kind(), format!("cannot create temp file {}: {err}", temp.display())))?;
    let result = write_and_sync(&mut file, contents, sync).map_err(|err| temp_error(err.kind(), err.to_string()));
    // Explicit, not scope-implicit: "handle closed, THEN cleanup" is the contract of this
    // function rather than an accident of where a block happens to end.
    drop(file);
    record_step(temp, WriteStep::TempClosed);
    result
}

/// Opens `temp` for writing per `create`; with a unix mode the mode is also forced on the
/// handle, because `OpenOptions::mode` applies only to a newly created file.
fn open_temp(temp: &Path, create: TempCreate) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if create.create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    if let Some(mode) = create.unix_mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let file = options.open(temp)?;
    #[cfg(unix)]
    if let Some(mode) = create.unix_mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(file)
}

/// Writes `contents` into the open `file` and fsyncs it when `sync` is set. Split out only
/// so the failure paths stay linear and so a test can inject a failure at either step.
fn write_and_sync(file: &mut fs::File, contents: &[u8], sync: bool) -> std::io::Result<()> {
    if let Some(injected) = injected_fault(FaultPoint::TempWrite) {
        return injected;
    }
    file.write_all(contents)?;
    if !sync {
        return Ok(());
    }
    if let Some(injected) = injected_fault(FaultPoint::TempSync) {
        return injected;
    }
    file.sync_all()
}

/// Runs `op` up to `attempts` times (at least once), sleeping `backoff` between attempts,
/// while it fails with an error `transient` accepts. Returns the first success or the last
/// error. Any other error is returned immediately.
pub(crate) fn retry_transient(attempts: u32, backoff: std::time::Duration, mut op: impl FnMut() -> std::io::Result<()>, transient: fn(&std::io::Error) -> bool) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) if attempt < attempts && transient(&err) => {
                attempt += 1;
                std::thread::sleep(backoff);
            }
            Err(err) => return Err(err),
        }
    }
}

/// Windows: `ERROR_ACCESS_DENIED` (5), `ERROR_SHARING_VIOLATION` (32) and
/// `ERROR_LOCK_VIOLATION` (33) — what `MoveFileExW` reports while another process holds the
/// target (or the temp) open without `FILE_SHARE_DELETE`. They clear when that handle closes.
#[cfg(windows)]
pub(crate) fn is_transient_sharing_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// Other targets: `rename(2)` does not fail because a reader holds the file open, so no
/// error is transient and a failed rename is final.
#[cfg(not(windows))]
pub(crate) fn is_transient_sharing_error(_err: &std::io::Error) -> bool {
    false
}

/// Best-effort removal of a temp file left behind by a failed write. The removal result is
/// deliberately dropped: the write failure is the error worth reporting, and a failed
/// cleanup must not mask it (CLAUDE.md §7).
fn remove_orphan_temp(temp: &Path) {
    let _ = fs::remove_file(temp);
    record_step(temp, WriteStep::TempRemoved);
}

/// Fsyncs the directory `dir` so a rename inside it is on stable storage.
///
/// Unix: open the directory and `sync_all` it. Without this a crash right after a rename
/// can leave neither the renamed-away source nor the new name.
///
/// # Errors
/// [`AtomicWriteError::DirSync`] naming `dir` (open or fsync failed).
#[cfg(unix)]
pub fn sync_directory(dir: &Path) -> Result<(), AtomicWriteError> {
    let dir_sync = |reason| AtomicWriteError::DirSync { dir: dir.to_path_buf(), reason };
    let handle = fs::File::open(dir).map_err(|err| dir_sync(format!("cannot open {} for fsync: {err}", dir.display())))?;
    handle.sync_all().map_err(|err| dir_sync(format!("cannot fsync {}: {err}", dir.display())))
}

/// Windows (and every other non-Unix target, wasm32 included): a no-op, because a directory
/// handle cannot be flushed there — `FlushFileBuffers` is unsupported for directories and
/// `File::open` on a directory fails without `FILE_FLAG_BACKUP_SEMANTICS`. The rename's
/// durability rests on the filesystem's own metadata journal. See the file header; the
/// caller's contract ("only delete the old source after `Ok`") is unchanged.
///
/// # Errors
/// Never; the `Result` matches the Unix variant so callers are target-neutral.
#[cfg(not(unix))]
// The signature must match the Unix variant so callers are target-neutral.
#[allow(clippy::unnecessary_wraps)]
pub fn sync_directory(_dir: &Path) -> Result<(), AtomicWriteError> {
    Ok(())
}
