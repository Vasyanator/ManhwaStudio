/*
File: resolve.rs

Purpose:
Format resolution (plan §B): which on-disk format a logical document has, which format a
write lands in, the both-exist repair, and the process-global default format for NEW
documents.

Key functions:
- set_default_format() / default_format(): the process-global default.
- detect(): unlocked resolution (the body of `actual_format`): B.2 exactly one file -> it;
  B.4 both -> the file matching `default_format()`; none -> `None`.
- resolve_locked(): the resolution a LOCKED operation uses: as `detect`, plus the B.4
  repair (validate the authoritative file, delete a Value-equal leftover, else
  `Ambiguous`) and B.3 (a new document takes its `DocRef` hint, else the default).
- chapter_new_format(): B.3 for chapter documents: the format of the first existing
  sibling, else the default.
- known_formats() / ensure_supported(): what this build probes and can open.

Notes:
- A lone `<stem>.db` must pass the 16-byte header sniff (`SQLite format 3\0`); a file that
  fails it is `Malformed`, never "absent" (a truncated database must not be replaced by a
  new empty document).
- wasm32 has no SQLite codec: `.db` files are not probed there (`known_formats` = JSON) and
  `ensure_supported(Db)` is `Unsupported`.
- The B.4 repair NEVER deletes anything unless both files parse and are Value-equal.
- In this crate's own unit tests `set_default_format` is THREAD-LOCAL (see
  `TEST_DEFAULT`), so tests that need a `Db` default cannot leak it into parallel tests.
*/

use std::sync::atomic::{AtomicU8, Ordering};

use crate::{DocFormat, DocRef, DocStoreError, Result};

/// Process-global default format for new documents, encoded by `format_to_u8`.
/// Single writer by contract (startup seed and the mode switch); plain relaxed atomics are
/// enough because no other memory is published through it.
static DEFAULT_FORMAT: AtomicU8 = AtomicU8::new(FORMAT_JSON);

const FORMAT_JSON: u8 = 0;
const FORMAT_DB: u8 = 1;

#[cfg(test)]
thread_local! {
    /// Unit tests of this crate run in parallel threads of one process; a per-thread
    /// default keeps a test that switches to `Db` from changing its neighbours' behavior.
    static TEST_DEFAULT: std::cell::Cell<Option<DocFormat>> = const { std::cell::Cell::new(None) };
}

/// Sets the format NEW documents are created in, and the authoritative format when both
/// files of one document exist (rule B.4). Existing single-format documents keep theirs.
/// On wasm32 setting `Db` makes creating a new document fail with `Unsupported`.
pub fn set_default_format(format: DocFormat) {
    #[cfg(test)]
    TEST_DEFAULT.with(|cell| cell.set(Some(format)));
    #[cfg(not(test))]
    {
        let encoded = match format {
            DocFormat::Json => FORMAT_JSON,
            DocFormat::Db => FORMAT_DB,
        };
        DEFAULT_FORMAT.store(encoded, Ordering::Relaxed);
    }
}

/// The format NEW documents are created in (initially [`DocFormat::Json`]).
#[must_use]
pub fn default_format() -> DocFormat {
    #[cfg(test)]
    if let Some(format) = TEST_DEFAULT.with(std::cell::Cell::get) {
        return format;
    }
    if DEFAULT_FORMAT.load(Ordering::Relaxed) == FORMAT_DB { DocFormat::Db } else { DocFormat::Json }
}

/// Every format whose file resolution may find for a document, in probe order (also what
/// `remove` deletes). wasm32: JSON only (no codec, a stray `.db` is invisible).
pub(crate) fn known_formats() -> &'static [DocFormat] {
    #[cfg(not(target_arch = "wasm32"))]
    {
        &[DocFormat::Json, DocFormat::Db]
    }
    #[cfg(target_arch = "wasm32")]
    {
        &[DocFormat::Json]
    }
}

/// Rejects a format this build cannot handle. Native builds handle every format.
///
/// # Errors
/// Never on native; see the wasm32 variant.
#[cfg(not(target_arch = "wasm32"))]
// The signature must match the wasm32 variant, which does fail.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn ensure_supported(_doc: &DocRef, _format: DocFormat) -> Result<()> {
    Ok(())
}

/// Rejects a format this build cannot handle.
///
/// # Errors
/// [`DocStoreError::Unsupported`] for [`DocFormat::Db`] (no `SQLite` codec on wasm32).
#[cfg(target_arch = "wasm32")]
pub(crate) fn ensure_supported(doc: &DocRef, format: DocFormat) -> Result<()> {
    match format {
        DocFormat::Json => Ok(()),
        DocFormat::Db => Err(DocStoreError::Unsupported { path: doc.path_for(format), format }),
    }
}

/// Which files of a document exist. A `.db` counts only after passing the header sniff.
#[derive(Debug, Clone, Copy)]
struct Present {
    json: bool,
    db: bool,
}

/// Stats both files; a LONE `.db` failing the header sniff is `Malformed` (clarification
/// 11). Next to a `.json` it still counts as present: rule B.4 then decides, and a bad
/// leftover makes the locked repair report `Ambiguous` instead of deleting anything.
fn probe(doc: &DocRef) -> Result<Present> {
    let json = crate::fsio::exists(&doc.path_for(DocFormat::Json));
    let db = known_formats().contains(&DocFormat::Db) && crate::fsio::exists(&doc.path_for(DocFormat::Db));
    if db && !json {
        sniff_db(doc)?;
    }
    Ok(Present { json, db })
}

/// Requires `<stem>.db` to start with the `SQLite` header.
#[cfg(not(target_arch = "wasm32"))]
fn sniff_db(doc: &DocRef) -> Result<()> {
    let path = doc.path_for(DocFormat::Db);
    if crate::sqlite::has_sqlite_header(&path)? {
        Ok(())
    } else {
        Err(DocStoreError::Malformed { path, cause: "not an SQLite database (the 16-byte header sniff failed)".to_owned() })
    }
}

/// wasm32: `.db` files are never probed, so this is unreachable in practice.
#[cfg(target_arch = "wasm32")]
fn sniff_db(doc: &DocRef) -> Result<()> {
    Err(DocStoreError::Unsupported { path: doc.path_for(DocFormat::Db), format: DocFormat::Db })
}

/// The format of `doc` as an UNLOCKED reader sees it (the body of `actual_format`): the
/// only existing file's format (B.2), the `default_format()` file when both exist (B.4,
/// without repair), `None` when absent.
///
/// # Errors
/// `Malformed` for a lone `.db` failing the header sniff; `Storage` when it cannot be read.
pub(crate) fn detect(doc: &DocRef) -> Result<Option<DocFormat>> {
    let present = probe(doc)?;
    Ok(match (present.json, present.db) {
        (false, false) => None,
        (true, false) => Some(DocFormat::Json),
        (false, true) => Some(DocFormat::Db),
        (true, true) => Some(default_format()),
    })
}

/// The format a NEW document (no file yet) is created in: its `DocRef` hint (B.3, the
/// chapter rule), else `default_format()`.
pub(crate) fn new_document_format(doc: &DocRef) -> DocFormat {
    doc.new_format_hint().unwrap_or_else(default_format)
}

/// The format a LOCKED operation addresses. Caller holds the document lock.
///
/// Like [`detect`], but a new document takes [`new_document_format`], and when both files
/// exist the B.4 repair runs: the `default_format()` file must parse; the other one is
/// deleted when it parses to an equal `Value`; otherwise `Ambiguous` and nothing is deleted.
///
/// # Errors
/// `Malformed` (lone bad `.db`, or the authoritative file does not parse), `Ambiguous`
/// (both exist and are not Value-equal), `Storage` on I/O failure.
pub(crate) fn resolve_locked(doc: &DocRef) -> Result<DocFormat> {
    let present = probe(doc)?;
    match (present.json, present.db) {
        (false, false) => Ok(new_document_format(doc)),
        (true, false) => Ok(DocFormat::Json),
        (false, true) => Ok(DocFormat::Db),
        (true, true) => repair_both(doc),
    }
}

/// Rule B.4 under the lock: keep the `default_format()` file, delete a Value-equal leftover.
fn repair_both(doc: &DocRef) -> Result<DocFormat> {
    let authoritative = default_format();
    let leftover = match authoritative {
        DocFormat::Json => DocFormat::Db,
        DocFormat::Db => DocFormat::Json,
    };
    let json = doc.path_for(DocFormat::Json);
    let db = doc.path_for(DocFormat::Db);
    // The authoritative file must parse; its own error is the answer otherwise (never a
    // silent fallback to the leftover).
    let Some(kept) = crate::codec::read_value_at(doc, authoritative)? else {
        // Vanished between the probe and the read (another process): re-resolve plainly.
        return Ok(if crate::fsio::exists(&doc.path_for(leftover)) { leftover } else { authoritative });
    };
    let other = match crate::codec::read_value_at(doc, leftover) {
        Ok(Some(other)) => other,
        Ok(None) => return Ok(authoritative),
        Err(err) => return Err(DocStoreError::Ambiguous { json, db, cause: format!("the leftover {} does not parse: {err}", leftover.extension()) }),
    };
    if kept != other {
        let cause = format!("the {} copy (authoritative, default format) and the {} copy hold different documents", authoritative.extension(), leftover.extension());
        ms_log::runtime_log::log_error(format!(
            "docstore: both formats of one document exist and differ; nothing was deleted.\nJSON: {}\nDB: {}\nPossible cause: an interrupted format conversion followed by a foreign edit of one copy",
            json.display(),
            db.display()
        ));
        return Err(DocStoreError::Ambiguous { json, db, cause });
    }
    crate::codec::remove_format(doc, leftover)?;
    ms_log::runtime_log::log_warn(format!(
        "docstore: both formats of {} existed with equal contents; removed the leftover .{} (an interrupted conversion was finished)",
        doc.stem().display(),
        leftover.extension()
    ));
    Ok(authoritative)
}

/// Rule B.3 for chapter documents (`Layers`, `Bubbles`): the format a NEW document of the
/// chapter should be created in = the format of the first sibling in `siblings` that has a
/// file (callers pass committed bubbles, committed layers, staging bubbles, staging layers,
/// in that order), else `default_format()`. Only stats files (no header sniff, no parse):
/// even a damaged sibling tells which format the chapter uses. Both files of one sibling
/// existing counts as `default_format()` (rule B.4). Apply the result with
/// [`DocRef::with_new_format`].
#[must_use]
pub fn chapter_new_format(siblings: &[DocRef]) -> DocFormat {
    for sibling in siblings {
        let json = crate::fsio::exists(&sibling.path_for(DocFormat::Json));
        let db = known_formats().contains(&DocFormat::Db) && crate::fsio::exists(&sibling.path_for(DocFormat::Db));
        match (json, db) {
            (false, false) => {}
            (true, false) => return DocFormat::Json,
            (false, true) => return DocFormat::Db,
            (true, true) => return default_format(),
        }
    }
    default_format()
}
