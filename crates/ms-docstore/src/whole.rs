/*
File: whole.rs

Purpose:
Whole-document materialization in either format (native only): write a complete document
into a fresh sibling temp file, validate it by reading it back, and atomically rename it
over the target. Used by `write_whole_atomic` (page-ops phase B), by the creation of a NEW
`.db` document, and step by step by the format conversion (`convert.rs`).

Key functions:
- write_temp()    : fresh temp `.{file}.{pid}.tmp` in the target's directory, full content.
- validate_temp() : reopen the temp and require a Value-equal document.
- commit_temp()   : fsync the temp, drop a stale `-journal` of the replaced `.db`, rename
                    over the target (Windows sharing-violation retry), fsync the directory.
- write_whole()   : the three above in order; the temp never survives a failure.
- remove_temp()   : best-effort cleanup of a temp (and its SQLite `-journal`).

Notes:
- Windows: the temp is fsynced through a WRITE handle (`FlushFileBuffers` requires
  `GENERIC_WRITE`; a read-only `File::open` handle fails with ACCESS_DENIED).
- The temp name is the atomic-write recipe's (`json::temp_path_for`), so
  `is_temp_artifact` recognizes it (and its `-journal`) as a crash leftover.
- HOT-JOURNAL HAZARD: SQLite replays a `<db>-journal` into whatever file carries the `<db>`
  name. Replacing (or deleting) a `.db` therefore removes its `-journal` first; the caller
  holds the document lock and, for chapter documents, the page-op quiesce, so no live
  transaction owns that journal.
- A validation mismatch is reported as `AtomicWriteError::TempWrite` (the previous document
  is untouched, which is exactly that variant's contract).
*/

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::json::{self, AtomicWriteError};
use crate::{DocFormat, DocRef, DocStoreError, Result};

/// `<path>-journal`, `SQLite`'s rollback journal of the database at `path`.
pub(crate) fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("-journal");
    PathBuf::from(name)
}

/// Best-effort removal of `temp` and its `SQLite` journal. The result is dropped on purpose:
/// the failure being reported is the one worth surfacing (AGENTS.md §7).
pub(crate) fn remove_temp(temp: &Path) {
    let _ = std::fs::remove_file(temp);
    let _ = std::fs::remove_file(journal_path(temp));
}

/// Writes the whole `value` in `format` into a FRESH sibling temp of `doc`'s `format` file
/// and returns the temp path. A stale temp of this process is removed first. JSON is
/// 2-space pretty without a trailing newline (the `WriteOptions::default` layout); a `.db`
/// gets `revision` = the replaced database's revision + 1 (0 when there is none).
///
/// # Errors
/// `Io` when the parent cannot be created, `Write(TempWrite)` / `Storage` on write failure;
/// the temp is removed on every failure.
pub(crate) fn write_temp(doc: &DocRef, value: &Value, format: DocFormat, create_parent_dirs: bool) -> Result<PathBuf> {
    let target = doc.path_for(format);
    if create_parent_dirs
        && let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| DocStoreError::Io { path: parent.to_path_buf(), source })?;
    }
    let temp = json::temp_path_for(&target);
    remove_temp(&temp);
    let written = match format {
        DocFormat::Json => {
            let text = serde_json::to_string_pretty(value).map_err(|err| DocStoreError::Serialize { path: target.clone(), cause: err.to_string() })?;
            json::write_temp_file(&temp, text.as_bytes(), true).map_err(|reason| DocStoreError::Write(AtomicWriteError::TempWrite { path: temp.clone(), reason }))
        }
        DocFormat::Db => {
            // Monotonic revision across a whole rewrite, so a revision probe sees a change.
            // A failed probe (corrupt target) restarts at 0 — logged, not propagated, because
            // the whole rewrite replaces that file anyway and nothing orders on the revision.
            let revision = if target.exists() {
                match crate::sqlite::read_revision(&target) {
                    Ok(revision) => revision.checked_add(1).unwrap_or(0),
                    Err(err) => {
                        ms_log::runtime_log::log_warn(format!("docstore: cannot read the revision of the database being replaced; the rewrite restarts at revision 0.\nPath: {}\nError: {err}", target.display()));
                        0
                    }
                }
            } else {
                0
            };
            crate::sqlite::create(&temp, value, doc.kind().as_str(), revision)
        }
    };
    match written {
        Ok(()) => Ok(temp),
        Err(err) => {
            remove_temp(&temp);
            Err(err)
        }
    }
}

/// Reopens `temp` (a `format` file) and requires it to hold a document equal to `expected`.
/// On mismatch or read failure the temp is removed.
///
/// # Errors
/// `Write(TempWrite)` naming the mismatch or the read failure.
pub(crate) fn validate_temp(temp: &Path, format: DocFormat, expected: &Value) -> Result<()> {
    let read_back = match format {
        DocFormat::Json => std::fs::read(temp).map_err(|err| err.to_string()).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|err| err.to_string())),
        DocFormat::Db => crate::sqlite::read_document(temp).map(|(value, _)| value).map_err(|err| err.to_string()),
    };
    let reason = match read_back {
        Ok(value) if value == *expected => return Ok(()),
        Ok(_) => "validation failed: the document read back differs from the one written".to_owned(),
        Err(err) => format!("validation failed: cannot read the written document back: {err}"),
    };
    remove_temp(temp);
    ms_log::runtime_log::log_error(format!("docstore: whole-document write failed validation.\nTemp: {}\nError: {reason}", temp.display()));
    Err(DocStoreError::Write(AtomicWriteError::TempWrite { path: temp.to_path_buf(), reason }))
}

/// Makes `temp` the document's `format` file: fsync the temp, remove the replaced `.db`'s
/// stale journal, rename (Windows retry), fsync the directory. On a failed rename the temp
/// is removed and the previous document is untouched.
///
/// # Errors
/// `Write(TempWrite)` (fsync), `Write(Rename)`, `Write(DirSync)` (the new file IS in place).
pub(crate) fn commit_temp(doc: &DocRef, temp: &Path, format: DocFormat) -> Result<()> {
    let target = doc.path_for(format);
    // SQLite already synced its pages at COMMIT (synchronous=FULL); this also covers the
    // JSON temp written without sync and any file-system metadata of the new file.
    // The handle MUST be opened with write access: on Windows `sync_all` is
    // `FlushFileBuffers`, which fails with ERROR_ACCESS_DENIED on a read-only handle
    // (`File::open`), and that would refuse every `.db` creation and conversion there.
    if let Err(err) = std::fs::OpenOptions::new().write(true).open(temp).and_then(|file| file.sync_all()) {
        remove_temp(temp);
        return Err(DocStoreError::Write(AtomicWriteError::TempWrite { path: temp.to_path_buf(), reason: format!("cannot fsync: {err}") }));
    }
    if format == DocFormat::Db {
        let journal = journal_path(&target);
        if journal.exists()
            && let Err(source) = std::fs::remove_file(&journal)
        {
            remove_temp(temp);
            return Err(DocStoreError::Io { path: journal, source });
        }
    }
    json::rename_with_retry(temp, &target).map_err(|err| {
        remove_temp(temp);
        DocStoreError::Write(AtomicWriteError::Rename { path: target.clone(), reason: err.to_string() })
    })?;
    let parent = target.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    json::sync_directory(parent).map_err(|reason| DocStoreError::Write(AtomicWriteError::DirSync { dir: parent.to_path_buf(), reason }))
}

/// Writes `value` as the whole `format` file of `doc`: temp, validation, atomic commit.
/// Caller holds the document lock. Does not touch the other format's file.
///
/// # Errors
/// As [`write_temp`], [`validate_temp`] and [`commit_temp`].
pub(crate) fn write_whole(doc: &DocRef, value: &Value, format: DocFormat, create_parent_dirs: bool) -> Result<()> {
    let temp = write_temp(doc, value, format, create_parent_dirs)?;
    validate_temp(&temp, format, value)?;
    commit_temp(doc, &temp, format)
}
