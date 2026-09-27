/*
File: json.rs

Purpose:
The JSON codec's write recipe and the optimistic-concurrency vocabulary of every owned
document. Moved from `ms-tab-typing/src/panel/doc_store.rs`, where it served only
`fonts_data.json` and `presets.json`; it is now the one recipe for every document of the
store.

Main responsibilities:
- `write_atomic` (native only): replace a file crash-safely — sibling temp, `write_all`,
  `sync_all`, CLOSE the handle, `rename`, and — when the caller asks for it — fsync the
  containing DIRECTORY so the rename itself is on stable storage before the call returns;
  a file that already holds the exact bytes is kept (no temp/rename), with the requested
  durability applied to it;
- `Fingerprint` / `SaveBaseline`: the "is the file still what I last read?" check two
  running instances of the app need in order not to overwrite each other silently;
- `is_temp_artifact`: the one owner of the temp-name pattern, for callers that copy or
  scan document directories and must skip crash leftovers;
- the write-step journal and fault injector, compiled only for tests and for dependent
  crates' tests (`test-support` feature).

Key types:
- `Fingerprint` (byte length + 64-bit digest of one exact document state)
- `SaveBaseline` (what the caller expects to find on disk: Unchecked / Absent / Matching)
- `Durability` (what is fsynced: nothing, the contents, or contents + directory)
- `AtomicWriteError` (typed failure of the write recipe)
- `WriteStep` / `FaultPoint` (test observability of the recipe)

Notes:
DIRECTORY DURABILITY IS PLATFORM-ASYMMETRIC, deliberately. On Unix the parent directory is
opened and `sync_all`ed, because a `rename` may be durable-in-page-cache only: after a power
loss the new name can be missing while the old content is already gone. On Windows a
directory cannot be flushed at all (`FlushFileBuffers` is not supported for directories, and
`File::open` on a directory fails without `FILE_FLAG_BACKUP_SEMANTICS`), so the step is a
documented no-op there and the rename's durability rests on the filesystem's metadata
journal. Either way the CONTRACT the callers rely on is the same: a caller may delete the
data source it just migrated only after this function returned `Ok`.
WINDOWS SHARING VIOLATIONS: a rename over a file that another process holds open without
`FILE_SHARE_DELETE` (the Python backend's CRT `open`, antivirus scanners, indexers) fails
transiently, where the historical in-place write would have succeeded. The rename is
therefore retried a bounded number of times on those error codes only (Windows only; on
other targets a failed rename is final). The temp file is removed after the final failure.
On wasm32 there is no `std::fs`: the store writes through the `ms-storage` seam instead
(no atomicity, no fsync) and the recipe below is compiled out.
*/

use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::{fs, io::Write, path::Path};

/// How durable a replacement must be before a write returns. Ignored on wasm32, where the
/// storage seam offers no fsync at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// Nothing is fsynced: still an atomic temp + rename (a crash never tears the file),
    /// but a crash shortly after the call may lose THIS write and leave the previous
    /// version. For callers on the GUI thread, which must never wait for a disk flush
    /// (AGENTS.md §5); never for a write after which an old copy is deleted.
    None,
    /// Only the new CONTENTS are fsynced. Enough for a document that is rewritten by the
    /// next mutation anyway and whose loss costs at most one cached value.
    #[default]
    Contents,
    /// The contents AND the containing directory (Unix; see the file header for Windows).
    /// Required whenever the caller DELETES the data's previous home after this returns:
    /// without it a power loss can leave neither the old source nor the new file.
    ContentsAndDirectory,
}

/// Identity of one exact on-disk state of a document: its byte length plus a 64-bit digest
/// of its bytes.
///
/// It exists so a writer can tell "the file is still what I last read/wrote" from "another
/// process replaced it", WITHOUT depending on a filesystem timestamp (whose resolution is
/// coarse enough that two writes inside one second are indistinguishable, and which a copy
/// or a restore can move backwards). Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    /// Length of the document in bytes.
    len: u64,
    /// First 8 bytes of the SHA-256 digest of the document bytes, read big-endian.
    digest: u64,
}

/// What a caller expects to find on disk when it saves — the optimistic-concurrency check
/// that keeps two running app instances from silently overwriting each other's data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SaveBaseline {
    /// No expectation at all: overwrite whatever is there. Used before this process has
    /// read the document even once, and by documents without a conflict contract.
    #[default]
    Unchecked,
    /// The document is expected to be ABSENT (this process saw no file).
    Absent,
    /// The document is expected to hash exactly to this fingerprint.
    Matching(Fingerprint),
}

impl SaveBaseline {
    /// Whether a document currently hashing to `found` satisfies this expectation.
    /// `Absent` never does — an existing file is by definition not the absence we expected.
    /// (A file that is absent at write time never blocks a write; that rule lives in the
    /// store's write path, not here.)
    #[must_use]
    pub fn accepts(self, found: Fingerprint) -> bool {
        match self {
            Self::Unchecked => true,
            Self::Absent => false,
            Self::Matching(expected) => expected == found,
        }
    }
}

/// 64-bit fingerprint of `contents`: the first 8 bytes of its SHA-256, read big-endian,
/// plus the byte length.
#[must_use]
pub fn fingerprint(contents: &[u8]) -> Fingerprint {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(contents);
    // SHA-256 is 32 bytes, so the first 8 always exist; the fixed-size copy cannot panic.
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    Fingerprint {
        // The length is a cheap first discriminator beside the digest; a hypothetical
        // >2^64-byte document would saturate rather than wrap, which only ever makes the
        // comparison stricter (never falsely "unchanged").
        len: u64::try_from(contents.len()).unwrap_or(u64::MAX),
        digest: u64::from_be_bytes(head),
    }
}

/// Typed failure of the atomic write recipe. Every variant names the path it was working on
/// and the OS reason, so the log line and the user-facing message carry the same facts.
///
/// In EVERY variant the previous document is left untouched, except `DirSync`, where the
/// new document IS in place but its directory entry is not known to be durable yet — which
/// is why that case is still an error: the caller must not delete the data's old home.
///
/// The store logs every variant as a write failure EXCEPT `DirSync`: its severity depends on
/// what the caller does next (tolerable for a marker, fatal before deleting a source), so
/// the caller logs it, once.
#[derive(Debug, thiserror::Error)]
pub enum AtomicWriteError {
    /// The sibling temp file could not be created, written or fsynced.
    #[error("cannot write temp file {}: {reason}", path.display())]
    TempWrite {
        /// The temp file involved.
        path: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The finished temp file could not be renamed over the target.
    #[error("cannot replace {}: {reason}", path.display())]
    Rename {
        /// The target document.
        path: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The rename succeeded but the containing directory could not be fsynced.
    #[error("the new file is written but directory {} could not be fsynced: {reason}", dir.display())]
    DirSync {
        /// The directory that could not be fsynced.
        dir: PathBuf,
        /// OS reason.
        reason: String,
    },
}

/// The sibling temp path the recipe writes before renaming: `.{file_name}.{pid}.tmp` in
/// the target's directory. Per-process so two concurrent processes never collide on it; the
/// `.` prefix hides it from casual directory listings. Within one process the document lock
/// serializes writers of the same target, so the name is never shared concurrently.
#[must_use]
pub fn temp_path_for(path: &std::path::Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path
        .file_name()
        .map_or_else(|| "document.json".to_owned(), |name| name.to_string_lossy().into_owned());
    parent.join(format!(".{file_name}.{}.tmp", std::process::id()))
}

/// Whether `path` names a temp file of the write recipe (`.{file_name}.{pid}.tmp`, any pid)
/// — a leftover of a crash between the temp write and the rename — or the `SQLite` rollback
/// journal of such a temp (`.{file_name}.{pid}.tmp-journal`, left when a `.db` temp was
/// being written). Directory copies and scans of document trees use it to skip such files;
/// it is the one owner of the pattern produced by [`temp_path_for`]. A document's own
/// `<name>.db-journal` is NOT a temp artifact (it may be needed for crash recovery).
#[must_use]
pub fn is_temp_artifact(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let name = name.strip_suffix("-journal").unwrap_or(name);
    let Some(inner) = name.strip_prefix('.').and_then(|rest| rest.strip_suffix(".tmp")) else { return false };
    // `inner` is `{file_name}.{pid}`: a non-empty file name and an all-digit pid.
    inner.rsplit_once('.').is_some_and(|(file_name, pid)| !file_name.is_empty() && !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

/// How many times a rename that failed with a transient sharing violation is attempted in
/// total (Windows only; see the file header). With [`RENAME_RETRY_BACKOFF`] the worst case
/// adds about 200 ms before the error is reported.
#[cfg(not(target_arch = "wasm32"))]
const RENAME_ATTEMPTS: u32 = 10;

/// Pause between two rename attempts.
#[cfg(not(target_arch = "wasm32"))]
const RENAME_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Atomically replaces `path` with `contents`.
///
/// The recipe: write a sibling temp file in the SAME directory (so the rename never crosses
/// a filesystem boundary), `write_all` + `sync_all` it, CLOSE the handle, then `rename` it
/// over the target — an atomic replace on both Unix (`rename(2)`) and Windows
/// (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`). With
/// [`Durability::ContentsAndDirectory`] the containing directory is fsynced afterwards, so
/// the new directory entry is on stable storage before this returns; with
/// [`Durability::None`] even the temp file is not fsynced.
///
/// On Windows a rename that fails with a sharing violation is retried (bounded, see
/// [`RENAME_ATTEMPTS`]); the old document is never deleted first, so atomicity is kept.
///
/// The handle is dropped BEFORE any cleanup or rename: deleting a file that is still open
/// fails on Windows, which would have left the orphaned temp behind on every failed write.
///
/// When the file already holds exactly `contents` nothing is rewritten (no temp, no
/// rename); only the requested durability is applied to the existing file (see
/// [`keep_identical_existing`]).
///
/// The parent directory must already exist. Native only.
///
/// # Errors
/// Returns [`AtomicWriteError`]; see its variants for what is (and is not) already on disk.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_atomic(path: &Path, contents: &[u8], durability: Durability) -> Result<(), AtomicWriteError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if keep_identical_existing(path, parent, contents, durability) {
        return Ok(());
    }
    let temp = temp_path_for(path);

    if let Err(reason) = write_temp_file(&temp, contents, durability != Durability::None) {
        // The handle is already closed (see `write_temp_file`), so this cleanup can also
        // succeed on Windows. A failed cleanup cannot mask the real error — AGENTS.md §7.
        remove_orphan_temp(&temp);
        return Err(AtomicWriteError::TempWrite { path: temp, reason });
    }

    if let Err(err) = rename_with_retry(&temp, path) {
        remove_orphan_temp(&temp);
        return Err(AtomicWriteError::Rename { path: path.to_path_buf(), reason: err.to_string() });
    }
    record_step(path, WriteStep::Renamed);

    match durability {
        Durability::None | Durability::Contents => Ok(()),
        Durability::ContentsAndDirectory => {
            sync_directory(parent).map_err(|reason| AtomicWriteError::DirSync { dir: parent.to_path_buf(), reason })?;
            record_step(path, WriteStep::DirectoryDurable);
            Ok(())
        }
    }
}

/// The "identical bytes" shortcut of [`write_atomic`]: when `path` already holds exactly
/// `contents`, the replacement is a no-op for the document, so no temp file, fsync or
/// rename is spent on it (an unchanged page flush then costs one `stat` + one read from the
/// page cache). The durability the caller asked for is still honoured on the EXISTING
/// file — it may itself come from an earlier `Durability::None` write: `Contents` fsyncs it
/// (a clean file's fsync is ~0.1 ms, measured on the HDD) and `ContentsAndDirectory` also
/// fsyncs the directory. Returns `true` when the existing file now satisfies the write.
///
/// Every failure here (unreadable file, a Windows sharing violation on the write handle
/// `FlushFileBuffers` needs, a failed fsync) returns `false`, so the caller runs the full
/// recipe, which is then the one to report a real error. Never an error of its own.
#[cfg(not(target_arch = "wasm32"))]
fn keep_identical_existing(path: &Path, parent: &Path, contents: &[u8], durability: Durability) -> bool {
    // Length first: a changed document almost always changes length, so the common
    // "different" case costs one stat and no read.
    let same_length = fs::metadata(path).is_ok_and(|meta| meta.is_file() && u64::try_from(contents.len()).is_ok_and(|len| len == meta.len()));
    if !same_length || fs::read(path).map_or(true, |existing| existing != contents) {
        return false;
    }
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

/// `fs::rename(from, to)`, retried on Windows sharing violations (see the file header);
/// on other targets a failed rename is final. The caller cleans up `from` on failure.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_transient(RENAME_ATTEMPTS, RENAME_RETRY_BACKOFF, || fs::rename(from, to), is_transient_sharing_error)
}

/// Creates `temp`, writes `contents` into it, fsyncs it when `sync` is set and CLOSES it.
/// The handle is
/// dropped before this returns in every path, success or failure, so the caller may delete
/// or rename the file immediately (both fail on an open handle under Windows).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_temp_file(temp: &Path, contents: &[u8], sync: bool) -> Result<(), String> {
    let mut file = fs::File::create(temp).map_err(|err| format!("cannot create temp file {}: {err}", temp.display()))?;
    let result = write_and_sync(&mut file, contents, sync).map_err(|err| err.to_string());
    // Explicit, not scope-implicit: "handle closed, THEN cleanup" is the contract of this
    // function rather than an accident of where a block happens to end.
    drop(file);
    record_step(temp, WriteStep::TempClosed);
    result
}

/// Writes `contents` into the open `file` and fsyncs it when `sync` is set. Split out only
/// so the failure paths stay linear and so a test can inject a failure at either step.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
fn retry_transient(attempts: u32, backoff: std::time::Duration, mut op: impl FnMut() -> std::io::Result<()>, transient: fn(&std::io::Error) -> bool) -> std::io::Result<()> {
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
fn is_transient_sharing_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// Other native targets: `rename(2)` does not fail because a reader holds the file open,
/// so no error is transient and a failed rename is final.
#[cfg(all(not(windows), not(target_arch = "wasm32")))]
fn is_transient_sharing_error(_err: &std::io::Error) -> bool {
    false
}

/// Best-effort removal of a temp file left behind by a failed write. The removal result is
/// deliberately dropped: the write failure is the error worth reporting, and a failed
/// cleanup must not mask it (AGENTS.md §7).
#[cfg(not(target_arch = "wasm32"))]
fn remove_orphan_temp(temp: &Path) {
    let _ = fs::remove_file(temp);
    record_step(temp, WriteStep::TempRemoved);
}

/// Fsyncs the directory `dir` so a rename inside it is on stable storage.
///
/// Unix: open the directory and `sync_all` it. Without this a crash right after a rename
/// can leave neither the renamed-away source nor the new name.
#[cfg(unix)]
pub(crate) fn sync_directory(dir: &Path) -> Result<(), String> {
    let handle = fs::File::open(dir).map_err(|err| format!("cannot open {} for fsync: {err}", dir.display()))?;
    handle.sync_all().map_err(|err| format!("cannot fsync {}: {err}", dir.display()))
}

/// Windows (and any other non-Unix native target): a no-op, because a directory handle
/// cannot be flushed there — `FlushFileBuffers` is unsupported for directories and
/// `File::open` on a directory fails without `FILE_FLAG_BACKUP_SEMANTICS`. The rename's
/// durability rests on the filesystem's own metadata journal. See the file header; the
/// caller's contract ("only delete the old source after `Ok`") is unchanged.
#[cfg(all(not(unix), not(target_arch = "wasm32")))]
// The signature must match the Unix variant so `write_atomic` is target-neutral.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn sync_directory(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// One observable step of the write recipe. The ORDER of these steps is the contract
/// (`TempClosed` before `TempRemoved`, `Renamed` before `DirectoryDurable`) and a unit test
/// cannot observe it any other way; outside tests recording them is compiled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// The temp file's handle has been closed.
    TempClosed,
    /// An orphaned temp file was removed after a failure.
    TempRemoved,
    /// The temp file was renamed over the target.
    Renamed,
    /// The containing directory is known-durable (fsynced on Unix; see `sync_directory`).
    DirectoryDurable,
}

/// Journal of the write steps taken per path, in order. Keyed by path so tests running in
/// parallel over their own temp directories never see each other's entries. Test-only.
#[cfg(any(test, feature = "test-support"))]
fn journal() -> &'static std::sync::Mutex<Vec<(PathBuf, WriteStep)>> {
    static JOURNAL: std::sync::OnceLock<std::sync::Mutex<Vec<(PathBuf, WriteStep)>>> = std::sync::OnceLock::new();
    JOURNAL.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Records one write step in the test journal.
#[cfg(all(any(test, feature = "test-support"), not(target_arch = "wasm32")))]
fn record_step(path: &Path, step: WriteStep) {
    let mut entries = journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.push((path.to_path_buf(), step));
}

/// Production build: the journal does not exist, so recording is nothing at all.
#[cfg(all(not(any(test, feature = "test-support")), not(target_arch = "wasm32")))]
fn record_step(_path: &Path, _step: WriteStep) {}

/// The steps recorded for `path`, in order. Test-only (`test-support` for dependents).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn recorded_steps(path: &std::path::Path) -> Vec<WriteStep> {
    let entries = journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.iter().filter(|(recorded, _)| recorded == path).map(|(_, step)| *step).collect()
}

/// Where an injected I/O failure strikes. The temp-write failure path — and the cleanup
/// ordering it must honor — is otherwise unreachable on a healthy filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// `write_all` into the temp file fails.
    TempWrite,
    /// `sync_all` of the temp file fails.
    TempSync,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Fault armed for THIS thread only, so an injecting test cannot break a parallel one.
    static ARMED_FAULT: std::cell::Cell<Option<FaultPoint>> = const { std::cell::Cell::new(None) };
}

/// Arms (or with `None` disarms) an injected write failure for the current thread.
/// Test-only (`test-support` for dependents).
#[cfg(any(test, feature = "test-support"))]
pub fn arm_fault(point: Option<FaultPoint>) {
    ARMED_FAULT.with(|armed| armed.set(point));
}

/// The injected failure for `point`, if one is armed on this thread. Test-only.
#[cfg(all(any(test, feature = "test-support"), not(target_arch = "wasm32")))]
fn injected_fault(point: FaultPoint) -> Option<std::io::Result<()>> {
    ARMED_FAULT.with(|armed| {
        armed
            .get()
            .filter(|armed| *armed == point)
            .map(|point| Err(std::io::Error::other(format!("injected {point:?} failure"))))
    })
}

/// Production build: nothing can be injected.
#[cfg(all(not(any(test, feature = "test-support")), not(target_arch = "wasm32")))]
fn injected_fault(_point: FaultPoint) -> Option<std::io::Result<()>> {
    None
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// A durable write leaves the directory entry fsynced BEFORE it returns, so a caller
    /// may delete the source it just migrated.
    #[test]
    fn a_durable_write_fsyncs_the_directory_after_the_rename() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("doc.json");
        write_atomic(&path, b"{}\n", Durability::ContentsAndDirectory).expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "{}\n");
        assert_eq!(
            recorded_steps(&path),
            vec![WriteStep::Renamed, WriteStep::DirectoryDurable],
            "the directory must be made durable, and only after the rename"
        );
    }

    /// The cheap mode is unchanged: contents only, no directory fsync.
    #[test]
    fn a_contents_only_write_does_not_touch_the_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("doc.json");
        write_atomic(&path, b"{}\n", Durability::Contents).expect("write");
        assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed]);
    }

    /// A failed temp write closes the handle BEFORE removing the orphan (on Windows
    /// `remove_file` on an open handle fails) and leaves the previous document untouched.
    #[test]
    fn a_failed_temp_write_closes_the_handle_before_removing_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("doc.json");
        fs::write(&path, "previous\n").expect("seed previous document");
        let temp = temp_path_for(&path);

        arm_fault(Some(FaultPoint::TempWrite));
        let err = write_atomic(&path, b"new\n", Durability::ContentsAndDirectory).expect_err("the injected failure must be reported");
        arm_fault(None);

        assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
        assert_eq!(recorded_steps(&temp), vec![WriteStep::TempClosed, WriteStep::TempRemoved], "the handle must be closed before the orphan is removed");
        assert!(!temp.exists(), "no orphaned temp file may survive");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "previous\n", "a failed write must leave the previous document intact");
    }

    /// The same holds when the fsync of the temp file fails.
    #[test]
    fn a_failed_temp_fsync_is_reported_and_cleaned_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("doc.json");
        let temp = temp_path_for(&path);

        arm_fault(Some(FaultPoint::TempSync));
        let err = write_atomic(&path, b"new\n", Durability::Contents).expect_err("the injected failure must be reported");
        arm_fault(None);

        assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
        assert_eq!(recorded_steps(&temp), vec![WriteStep::TempClosed, WriteStep::TempRemoved]);
        assert!(!path.exists(), "nothing may be created by a failed write");
    }

    /// `Durability::None` still replaces atomically and never touches the directory.
    #[test]
    fn an_unsynced_write_is_still_an_atomic_replace() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("doc.json");
        fs::write(&path, "previous\n").expect("seed");
        // An armed fsync fault must not fire: this mode never fsyncs.
        arm_fault(Some(FaultPoint::TempSync));
        let result = write_atomic(&path, b"new\n", Durability::None);
        arm_fault(None);
        result.expect("an unsynced write never reaches the fsync step");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "new\n");
        assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed]);
        assert!(!temp_path_for(&path).exists());
    }

    /// Identical bytes: nothing is rewritten (same inode, no temp, no rename step), even in
    /// the durable modes, whose durability is applied to the existing file instead.
    #[test]
    fn identical_bytes_keep_the_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (index, durability) in [Durability::None, Durability::Contents, Durability::ContentsAndDirectory].into_iter().enumerate() {
            let path = dir.path().join(format!("doc{index}.json"));
            write_atomic(&path, b"{\"a\": 1}\n", durability).expect("first write");
            let before = fs::metadata(&path).expect("stat");
            let steps_before = recorded_steps(&path);
            // An armed fault proves no temp file is even created.
            arm_fault(Some(FaultPoint::TempWrite));
            let result = write_atomic(&path, b"{\"a\": 1}\n", durability);
            arm_fault(None);
            result.expect("an identical write is a no-op");
            let after = fs::metadata(&path).expect("stat");
            assert_eq!(after.modified().expect("mtime"), before.modified().expect("mtime"), "{durability:?}: file untouched");
            #[cfg(unix)]
            assert_eq!(std::os::unix::fs::MetadataExt::ino(&after), std::os::unix::fs::MetadataExt::ino(&before), "{durability:?}: no rename");
            let mut expected = steps_before;
            if durability == Durability::ContentsAndDirectory {
                expected.push(WriteStep::DirectoryDurable);
            }
            assert_eq!(recorded_steps(&path), expected, "{durability:?}: no Renamed step, directory durability still applied");
            assert!(!temp_path_for(&path).exists());
            // Same length, different bytes: the full recipe runs.
            write_atomic(&path, b"{\"a\": 2}\n", durability).expect("changed write");
            assert_eq!(fs::read_to_string(&path).expect("read"), "{\"a\": 2}\n");
        }
    }

    /// Transient failures are retried up to the bound; the last error is returned.
    #[test]
    fn transient_rename_failures_are_retried_within_the_bound() {
        let transient = |err: &std::io::Error| err.kind() == std::io::ErrorKind::WouldBlock;
        let zero = std::time::Duration::ZERO;

        let mut calls = 0;
        let result = retry_transient(5, zero, || {
            calls += 1;
            if calls < 3 { Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)) } else { Ok(()) }
        }, transient);
        assert!(result.is_ok());
        assert_eq!(calls, 3, "success on the third attempt ends the loop");

        let mut calls = 0;
        let result = retry_transient(5, zero, || {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }, transient);
        assert_eq!(result.expect_err("persistent failure").kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(calls, 5, "never more than the bound");

        let mut calls = 0;
        let result = retry_transient(5, zero, || {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }, transient);
        assert!(result.is_err());
        assert_eq!(calls, 1, "a non-transient error is final");
    }

    /// The temp-name predicate recognizes exactly the recipe's temp names.
    #[test]
    fn temp_artifacts_are_recognized_by_the_recipe_pattern() {
        let target = std::path::Path::new("/x/chapter/translation_bubbles.json");
        assert!(is_temp_artifact(&temp_path_for(target)));
        assert!(is_temp_artifact(std::path::Path::new("layers/.layers.json.4242.tmp")));
        assert!(!is_temp_artifact(std::path::Path::new("layers/layers.json")));
        assert!(!is_temp_artifact(std::path::Path::new(".layers.json.tmp")), "pid is required");
        assert!(!is_temp_artifact(std::path::Path::new(".layers.json.12a.tmp")), "pid is digits");
        assert!(!is_temp_artifact(std::path::Path::new("..1.tmp")), "file name is required");
        assert!(!is_temp_artifact(std::path::Path::new("page.tmp")));
        assert!(is_temp_artifact(std::path::Path::new("layers/.layers.db.77.tmp")));
        assert!(is_temp_artifact(std::path::Path::new("layers/.layers.db.77.tmp-journal")), "the journal of a .db temp");
        assert!(!is_temp_artifact(std::path::Path::new("layers/layers.db-journal")), "a document's own journal is needed for recovery");
    }

    /// The baseline vocabulary: only an exact match (or "no expectation") accepts a
    /// document, and `Absent` never accepts an existing one.
    #[test]
    fn baselines_accept_exactly_what_they_promise() {
        let one = fingerprint(b"{}\n");
        let other = fingerprint(b"{ }\n");
        assert!(SaveBaseline::Unchecked.accepts(one));
        assert!(!SaveBaseline::Absent.accepts(one));
        assert!(SaveBaseline::Matching(one).accepts(one));
        assert!(!SaveBaseline::Matching(one).accepts(other));
    }
}
