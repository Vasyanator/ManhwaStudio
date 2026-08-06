/*
File: panel/doc_store.rs

Purpose:
The ONE write recipe and the ONE optimistic-concurrency vocabulary shared by the panel's
two JSON documents, `fonts/fonts_data.json` (`fonts_data.rs`) and `fonts/presets.json`
(`presets_store.rs`). Both used to carry their own copy of the same code; the copies had
already drifted (neither fsynced the containing directory, and both deleted the temp file
while its handle was still open), which is exactly what a shared owner prevents.

Main responsibilities:
- `write_atomic`: replace a file crash-safely — sibling temp, `write_all`, `sync_all`,
  CLOSE the handle, `rename`, and — when the caller asks for it — fsync the containing
  DIRECTORY so the rename itself is on stable storage before the call returns;
- `DocumentFingerprint` / `SaveBaseline`: the "is the file still what I last read?" check
  two running instances of the app need in order not to overwrite each other silently.

Key types:
- `DocumentFingerprint` (byte length + 64-bit digest of one exact document state)
- `SaveBaseline` (what the caller expects to find on disk: Unchecked / Absent / Matching)
- `Durability` (whether the containing directory is fsynced after the rename)
- `AtomicWriteError` (typed failure of the write recipe)

Notes:
DIRECTORY DURABILITY IS PLATFORM-ASYMMETRIC, deliberately. On Unix the parent directory is
opened and `sync_all`ed, because a `rename` may be durable-in-page-cache only: after a power
loss the new name can be missing while the old content is already gone. On Windows a
directory cannot be flushed at all (`FlushFileBuffers` is not supported for directories, and
`File::open` on a directory fails without `FILE_FLAG_BACKUP_SEMANTICS`), so the step is a
documented no-op there and the rename's durability rests on the filesystem's metadata
journal. Either way the CONTRACT the callers rely on is the same: a caller may delete the
data source it just migrated only after this function returned `Ok`.
*/

use super::*;
use std::io::Write;

/// How durable the replacement must be before [`write_atomic`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tabs::typing) enum Durability {
    /// Only the new CONTENTS are fsynced. Enough for a document that is rewritten by the
    /// next mutation anyway and whose loss costs at most one cached value.
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
/// or a restore can move backwards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tabs::typing) struct DocumentFingerprint {
    /// Length of the document in bytes.
    len: u64,
    /// First 8 bytes of the SHA-256 digest of the document bytes, read big-endian.
    digest: u64,
}

/// What a caller expects to find on disk when it saves — the optimistic-concurrency check
/// that keeps two running app instances from silently overwriting each other's data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tabs::typing) enum SaveBaseline {
    /// No expectation at all: overwrite whatever is there. Used before this process has
    /// read the document even once.
    #[default]
    Unchecked,
    /// The document is expected to be ABSENT (this process saw no file).
    Absent,
    /// The document is expected to hash exactly to this fingerprint.
    Matching(DocumentFingerprint),
}

impl SaveBaseline {
    /// Whether a document currently hashing to `found` satisfies this expectation.
    /// `Absent` never does — an existing file is by definition not the absence we expected.
    #[must_use]
    pub(in crate::tabs::typing) fn accepts(self, found: DocumentFingerprint) -> bool {
        match self {
            Self::Unchecked => true,
            Self::Absent => false,
            Self::Matching(expected) => expected == found,
        }
    }
}

/// 64-bit fingerprint of `contents`: the first 8 bytes of its SHA-256, read big-endian.
///
/// Independent by design from `fonts::font_content_hash` (that one identifies a font file's
/// bytes, this one a DOCUMENT state); neither is persisted.
#[must_use]
pub(in crate::tabs::typing) fn fingerprint(contents: &str) -> DocumentFingerprint {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(contents.as_bytes());
    // SHA-256 is 32 bytes, so the first 8 always exist; the fixed-size copy cannot panic.
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    DocumentFingerprint {
        // The length is a cheap first discriminator beside the digest; a hypothetical
        // >2^64-byte document would saturate rather than wrap, which only ever makes the
        // comparison stricter (never falsely "unchanged").
        len: u64::try_from(contents.len()).unwrap_or(u64::MAX),
        digest: u64::from_be_bytes(head),
    }
}

/// Typed failure of [`write_atomic`]. Every variant names the path it was working on and
/// the OS reason, so the log line and the user-facing message carry the same facts.
///
/// In EVERY variant the previous document is left untouched, except `DirSync`, where the
/// new document IS in place but its directory entry is not known to be durable yet — which
/// is why that case is still an error: the caller must not delete the data's old home.
#[derive(Debug)]
pub(in crate::tabs::typing) enum AtomicWriteError {
    /// The sibling temp file could not be created, written or fsynced.
    TempWrite {
        /// The temp file involved.
        path: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The finished temp file could not be renamed over the target.
    Rename {
        /// The target document.
        path: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The rename succeeded but the containing directory could not be fsynced.
    DirSync {
        /// The directory that could not be fsynced.
        dir: PathBuf,
        /// OS reason.
        reason: String,
    },
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TempWrite { path, reason } => {
                write!(f, "cannot write temp file {}: {reason}", path.display())
            }
            Self::Rename { path, reason } => {
                write!(f, "cannot replace {}: {reason}", path.display())
            }
            Self::DirSync { dir, reason } => write!(
                f,
                "the new file is written but directory {} could not be fsynced: {reason}",
                dir.display()
            ),
        }
    }
}

impl std::error::Error for AtomicWriteError {}

/// Atomically replaces `path` with `contents`.
///
/// The recipe: write a sibling temp file in the SAME directory (so the rename never crosses
/// a filesystem boundary), `write_all` + `sync_all` it, CLOSE the handle, then `rename` it
/// over the target — an atomic replace on both Unix (`rename(2)`) and Windows
/// (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`). With
/// [`Durability::ContentsAndDirectory`] the containing directory is fsynced afterwards, so
/// the new directory entry is on stable storage before this returns.
///
/// The handle is dropped BEFORE any cleanup or rename: deleting a file that is still open
/// fails on Windows, which would have left the orphaned temp behind on every failed write.
///
/// The parent directory must already exist.
///
/// # Errors
/// Returns [`AtomicWriteError`]; see its variants for what is (and is not) already on disk.
pub(in crate::tabs::typing) fn write_atomic(
    path: &Path,
    contents: &str,
    durability: Durability,
) -> Result<(), AtomicWriteError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document.json".to_owned());
    // Per-process temp name keeps two concurrent processes from colliding on the same temp
    // path; a `.` prefix hides it from casual directory listings.
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    if let Err(reason) = write_temp_file(&temp, contents) {
        // The handle is already closed (see `write_temp_file`), so this cleanup can also
        // succeed on Windows. A failed cleanup cannot mask the real error — CLAUDE.md §7.
        remove_orphan_temp(&temp);
        return Err(AtomicWriteError::TempWrite { path: temp, reason });
    }

    if let Err(err) = fs::rename(&temp, path) {
        remove_orphan_temp(&temp);
        return Err(AtomicWriteError::Rename {
            path: path.to_path_buf(),
            reason: err.to_string(),
        });
    }
    record_step(path, WriteStep::Renamed);

    match durability {
        Durability::Contents => Ok(()),
        Durability::ContentsAndDirectory => {
            sync_directory(parent).map_err(|reason| AtomicWriteError::DirSync {
                dir: parent.to_path_buf(),
                reason,
            })?;
            record_step(path, WriteStep::DirectoryDurable);
            Ok(())
        }
    }
}

/// Creates `temp`, writes `contents` into it, fsyncs it and CLOSES it. The handle is
/// dropped before this returns in every path, success or failure, so the caller may delete
/// or rename the file immediately (both fail on an open handle under Windows).
fn write_temp_file(temp: &Path, contents: &str) -> Result<(), String> {
    let mut file = fs::File::create(temp)
        .map_err(|err| format!("cannot create temp file {}: {err}", temp.display()))?;
    let result = write_and_sync(&mut file, contents).map_err(|err| err.to_string());
    // Explicit, not scope-implicit: "handle closed, THEN cleanup" is the contract of this
    // function rather than an accident of where a block happens to end.
    drop(file);
    record_step(temp, WriteStep::TempClosed);
    result
}

/// Writes `contents` into the open `file` and fsyncs it. Split out only so the failure
/// paths stay linear and so a test can inject a failure at either step.
fn write_and_sync(file: &mut fs::File, contents: &str) -> std::io::Result<()> {
    if let Some(injected) = injected_fault(FaultPoint::TempWrite) {
        return injected;
    }
    file.write_all(contents.as_bytes())?;
    if let Some(injected) = injected_fault(FaultPoint::TempSync) {
        return injected;
    }
    file.sync_all()
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
#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<(), String> {
    let handle = fs::File::open(dir)
        .map_err(|err| format!("cannot open {} for fsync: {err}", dir.display()))?;
    handle
        .sync_all()
        .map_err(|err| format!("cannot fsync {}: {err}", dir.display()))
}

/// Windows (and any non-Unix target): a no-op, because a directory handle cannot be flushed
/// there — `FlushFileBuffers` is unsupported for directories and `File::open` on a directory
/// fails without `FILE_FLAG_BACKUP_SEMANTICS`. The rename's durability rests on the
/// filesystem's own metadata journal. See the file header; the caller's contract ("only
/// delete the old source after `Ok`") is unchanged.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// One observable step of the write recipe. The ORDER of these steps is the contract
/// (`TempClosed` before `TempRemoved`, `Renamed` before `DirectoryDurable`) and a unit test
/// cannot observe it any other way; outside tests recording them is compiled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tabs::typing) enum WriteStep {
    /// The temp file's handle has been closed.
    TempClosed,
    /// An orphaned temp file was removed after a failure.
    TempRemoved,
    /// The temp file was renamed over the target.
    Renamed,
    /// The containing directory is known-durable (fsynced on Unix; see [`sync_directory`]).
    DirectoryDurable,
}

/// Journal of the write steps taken per path, in order. Keyed by path so tests running in
/// parallel over their own temp directories never see each other's entries. Test-only.
#[cfg(test)]
fn journal() -> &'static std::sync::Mutex<Vec<(PathBuf, WriteStep)>> {
    static JOURNAL: std::sync::OnceLock<std::sync::Mutex<Vec<(PathBuf, WriteStep)>>> =
        std::sync::OnceLock::new();
    JOURNAL.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Records one write step in the test journal.
#[cfg(test)]
fn record_step(path: &Path, step: WriteStep) {
    let mut entries = journal()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.push((path.to_path_buf(), step));
}

/// Production build: the journal does not exist, so recording is nothing at all.
#[cfg(not(test))]
fn record_step(_path: &Path, _step: WriteStep) {}

/// The steps recorded for `path`, in order. Test-only.
#[cfg(test)]
#[must_use]
pub(in crate::tabs::typing) fn recorded_steps(path: &Path) -> Vec<WriteStep> {
    let entries = journal()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries
        .iter()
        .filter(|(recorded, _)| recorded == path)
        .map(|(_, step)| *step)
        .collect()
}

/// Where an injected I/O failure strikes. The temp-write failure path — and the cleanup
/// ordering it must honor — is otherwise unreachable on a healthy filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tabs::typing) enum FaultPoint {
    /// `write_all` into the temp file fails.
    TempWrite,
    /// `sync_all` of the temp file fails.
    TempSync,
}

#[cfg(test)]
thread_local! {
    /// Fault armed for THIS thread only, so an injecting test cannot break a parallel one.
    static ARMED_FAULT: std::cell::Cell<Option<FaultPoint>> = const { std::cell::Cell::new(None) };
}

/// Arms (or with `None` disarms) an injected write failure for the current thread.
/// Test-only.
#[cfg(test)]
pub(in crate::tabs::typing) fn arm_fault(point: Option<FaultPoint>) {
    ARMED_FAULT.with(|armed| armed.set(point));
}

/// The injected failure for `point`, if one is armed on this thread. Test-only; in a
/// production build no fault can be armed, so this is always `None`.
#[cfg(test)]
fn injected_fault(point: FaultPoint) -> Option<std::io::Result<()>> {
    ARMED_FAULT.with(|armed| {
        armed.get().filter(|armed| *armed == point).map(|point| {
            Err(std::io::Error::other(format!(
                "injected {point:?} failure"
            )))
        })
    })
}

/// Production build: nothing can be injected.
#[cfg(not(test))]
fn injected_fault(_point: FaultPoint) -> Option<std::io::Result<()>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique temp directory so parallel tests never share a file.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_doc_store_{tag}_{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// A durable write leaves the directory entry fsynced BEFORE it returns, so a caller
    /// may delete the source it just migrated (defect 1 of the phase-5 review: the presets
    /// were removed from `user_config.json` while `presets.json` was only in the page
    /// cache, and a power loss in that window lost both).
    #[test]
    fn a_durable_write_fsyncs_the_directory_after_the_rename() {
        let dir = unique_temp_dir("dir_sync");
        let path = dir.join("doc.json");
        write_atomic(&path, "{}\n", Durability::ContentsAndDirectory).expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "{}\n");
        assert_eq!(
            recorded_steps(&path),
            vec![WriteStep::Renamed, WriteStep::DirectoryDurable],
            "the directory must be made durable, and only after the rename"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The cheap mode is unchanged: contents only, no directory fsync.
    #[test]
    fn a_contents_only_write_does_not_touch_the_directory() {
        let dir = unique_temp_dir("no_dir_sync");
        let path = dir.join("doc.json");
        write_atomic(&path, "{}\n", Durability::Contents).expect("write");
        assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A failed temp write closes the handle BEFORE removing the orphan (defect 7: on
    /// Windows `remove_file` on an open handle fails, so the temp file survived every
    /// failed save) and leaves the previous document untouched.
    #[test]
    fn a_failed_temp_write_closes_the_handle_before_removing_it() {
        let dir = unique_temp_dir("fault");
        let path = dir.join("doc.json");
        fs::write(&path, "previous\n").expect("seed previous document");
        let temp = dir.join(format!(".doc.json.{}.tmp", std::process::id()));

        arm_fault(Some(FaultPoint::TempWrite));
        let err = write_atomic(&path, "new\n", Durability::ContentsAndDirectory)
            .expect_err("the injected failure must be reported");
        arm_fault(None);

        assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
        assert_eq!(
            recorded_steps(&temp),
            vec![WriteStep::TempClosed, WriteStep::TempRemoved],
            "the handle must be closed before the orphan is removed"
        );
        assert!(!temp.exists(), "no orphaned temp file may survive");
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            "previous\n",
            "a failed write must leave the previous document intact"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The same holds when the fsync of the temp file fails.
    #[test]
    fn a_failed_temp_fsync_is_reported_and_cleaned_up() {
        let dir = unique_temp_dir("fault_sync");
        let path = dir.join("doc.json");
        let temp = dir.join(format!(".doc.json.{}.tmp", std::process::id()));

        arm_fault(Some(FaultPoint::TempSync));
        let err = write_atomic(&path, "new\n", Durability::Contents)
            .expect_err("the injected failure must be reported");
        arm_fault(None);

        assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
        assert_eq!(
            recorded_steps(&temp),
            vec![WriteStep::TempClosed, WriteStep::TempRemoved]
        );
        assert!(!path.exists(), "nothing may be created by a failed write");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The baseline vocabulary: only an exact match (or "no expectation") accepts a
    /// document, and `Absent` never accepts an existing one.
    #[test]
    fn baselines_accept_exactly_what_they_promise() {
        let one = fingerprint("{}\n");
        let other = fingerprint("{ }\n");
        assert!(SaveBaseline::Unchecked.accepts(one));
        assert!(!SaveBaseline::Absent.accepts(one));
        assert!(SaveBaseline::Matching(one).accepts(one));
        assert!(!SaveBaseline::Matching(one).accepts(other));
    }
}
