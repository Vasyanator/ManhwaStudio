/*
File: sqlite.rs

Purpose:
The I/O half of the SQLite fragment codec (native only): opening a document database with
the pinned pragmas, the strict schema check, whole-document reads, the row-level diff
write inside one `BEGIN IMMEDIATE` transaction, and creating a fresh database file.

Key functions:
- has_sqlite_header() : the 16-byte header sniff.
- read_document()     : rows + revision in one read transaction -> joined `Value`.
- read_revision()     : `meta.revision` only.
- modify()            : THE write primitive: lock the file (BEGIN IMMEDIATE), read rows,
                        join, let the caller compute the new value, diff-write (UPDATE
                        in place / INSERT new / DELETE vanished — never REPLACE), bump
                        `revision` + set `writer` only when rows changed, COMMIT.
- create()            : a NEW file (must not exist) holding a whole document.

Schema (plan §C, shared with docstore.py — do not change without a `user_version` bump):
  PRAGMA application_id = 0x4D534453 ('MSDS'); PRAGMA user_version = 1;
  meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)
      schema_version='1', doc_kind=<DocKind::as_str>, revision=<u64>, writer='rust'|'python',
      app_version=<informational, '' today>
  frag(path TEXT PRIMARY KEY, parent TEXT, seg TEXT, kind TEXT NOT NULL, payload TEXT)
  INDEX frag_parent ON frag(parent)

Notes:
- Connections are short-lived: one per operation, closed before returning, so no handle
  outlives a call (Windows renames/deletes and page-op quiesce stay trivial).
- Every open sets busy_timeout=5000 first; write opens then set journal_mode=DELETE
  (explicitly: a foreign tool may have left WAL in the header), synchronous=FULL and
  foreign_keys=OFF. Readers also open read-write (never creating): only a writable
  connection can roll back a crashed writer's hot journal, and a reader of a WAL-mode
  file resets it to DELETE, so no `-wal`/`-shm` sidecars ever appear.
- Error mapping (the public error enum is deliberately unchanged): a foreign
  application_id, missing tables, corrupt rows or a failed join -> `Malformed` (never
  overwritten); a newer `user_version`/`schema_version` -> `Unsupported`; engine failures
  (busy past the timeout, I/O, permissions) -> `Storage(StorageError::Io)` with the SQLite
  message and an `io::ErrorKind` (`ResourceBusy` for busy/locked).
*/

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::Value;

use crate::split::{self, FragKind, Row};
use crate::{DocFormat, DocStoreError, Result, StorageError};

/// `PRAGMA application_id` of every document database ('MSDS').
pub(crate) const APPLICATION_ID: i32 = 0x4D53_4453;
/// `PRAGMA user_version` = the schema version this build reads and writes.
pub(crate) const SCHEMA_VERSION: i32 = 1;
/// `meta.writer` of rows this implementation commits.
const WRITER: &str = "rust";
/// The first 16 bytes of every `SQLite` 3 database file.
const HEADER: &[u8; 16] = b"SQLite format 3\0";
/// How long a connection waits for another process's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_SQL: &str = "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);\
CREATE TABLE frag(path TEXT PRIMARY KEY, parent TEXT, seg TEXT, kind TEXT NOT NULL, payload TEXT);\
CREATE INDEX frag_parent ON frag(parent);";

/// What a successful [`modify`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Committed {
    /// Rows inserted, updated or deleted (0 = nothing was written).
    pub(crate) touched: usize,
    /// `meta.revision` after the call (unchanged when `touched == 0`).
    pub(crate) revision: u64,
}

/// Header facts of an existing file.
struct Header {
    /// Starts with the `SQLite` magic.
    sqlite: bool,
    /// File-format read/write version bytes (offsets 18/19) say WAL.
    wal: bool,
}

/// Reads the first 20 header bytes of `path` (fewer for a short file).
fn read_header(path: &Path) -> Result<Header> {
    let mut buffer = [0u8; 20];
    let mut file = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => return Err(io_error(path, source)),
        }
    }
    let sqlite = filled >= HEADER.len() && &buffer[..HEADER.len()] == HEADER;
    Ok(Header { sqlite, wal: sqlite && filled >= 20 && (buffer[18] == 2 || buffer[19] == 2) })
}

/// Whether `path` starts with the 16-byte `SQLite` header.
///
/// # Errors
/// `Storage` when the file cannot be opened or read.
pub(crate) fn has_sqlite_header(path: &Path) -> Result<bool> {
    Ok(read_header(path)?.sqlite)
}

/// `Storage(Io)` for a plain file operation on `path`.
fn io_error(path: &Path, source: std::io::Error) -> DocStoreError {
    DocStoreError::Storage(StorageError::Io { path: path.display().to_string(), source })
}

/// Maps an engine error during `operation` on `path` (see the file header).
fn sql_error(path: &Path, operation: &str, err: &rusqlite::Error) -> DocStoreError {
    use rusqlite::ErrorCode;
    let code = err.sqlite_error_code();
    if let Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) = code {
        return DocStoreError::Malformed { path: path.to_path_buf(), cause: format!("SQLite {operation}: {err}") };
    }
    let kind = match code {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => std::io::ErrorKind::ResourceBusy,
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => std::io::ErrorKind::PermissionDenied,
        Some(ErrorCode::DiskFull) => std::io::ErrorKind::StorageFull,
        Some(ErrorCode::CannotOpen) => std::io::ErrorKind::NotFound,
        _ => std::io::ErrorKind::Other,
    };
    io_error(path, std::io::Error::new(kind, format!("SQLite {operation} failed: {err}")))
}

/// A `Malformed` error for `path`.
fn malformed(path: &Path, cause: impl Into<String>) -> DocStoreError {
    DocStoreError::Malformed { path: path.to_path_buf(), cause: cause.into() }
}

/// The pragmas every WRITE connection sets (busy timeout first, so the journal-mode
/// change waits for foreign locks instead of failing).
fn configure_writer(conn: &Connection, path: &Path) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|err| sql_error(path, "configure", &err))?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0)).map_err(|err| sql_error(path, "set journal_mode", &err))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(io_error(path, std::io::Error::other(format!("SQLite refused journal_mode=DELETE (still {mode})"))));
    }
    conn.pragma_update(None, "synchronous", "FULL").map_err(|err| sql_error(path, "set synchronous", &err))?;
    conn.pragma_update(None, "foreign_keys", "OFF").map_err(|err| sql_error(path, "set foreign_keys", &err))?;
    Ok(())
}

/// Opens an EXISTING document database (never creates one) and checks its identity:
/// `application_id` (foreign -> `Malformed`) and `user_version` (other -> `Unsupported`).
///
/// Readers open read-WRITE too (still never creating the file; `SQLite` falls back to
/// read-only when permissions deny writing): only a writable connection can roll back a
/// hot journal left by a crashed writer, which a read-only one reports as an error. A
/// reader of a WAL-mode file resets it to DELETE so no `-wal`/`-shm` sidecars appear.
fn open_existing(path: &Path, write: bool) -> Result<Connection> {
    let header = read_header(path)?;
    if !header.sqlite {
        return Err(malformed(path, "not an SQLite database (the 16-byte header sniff failed)"));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|err| sql_error(path, "open", &err))?;
    if write || header.wal {
        configure_writer(&conn, path)?;
    } else {
        conn.busy_timeout(BUSY_TIMEOUT).map_err(|err| sql_error(path, "configure", &err))?;
    }
    let application_id: i32 = conn.pragma_query_value(None, "application_id", |row| row.get(0)).map_err(|err| sql_error(path, "read application_id", &err))?;
    let user_version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(|err| sql_error(path, "read user_version", &err))?;
    if application_id != APPLICATION_ID {
        return Err(malformed(path, format!("not a ManhwaStudio document database (application_id={application_id:#x}, expected {APPLICATION_ID:#x})")));
    }
    if user_version != SCHEMA_VERSION {
        ms_log::runtime_log::log_error(format!("docstore: unsupported document database schema.\nPath: {}\nuser_version: {user_version} (this build reads {SCHEMA_VERSION})\nPossible cause: written by a newer ManhwaStudio", path.display()));
        return Err(DocStoreError::Unsupported { path: path.to_path_buf(), format: DocFormat::Db });
    }
    Ok(conn)
}

/// Reads `meta` (schema check + revision) and every `frag` row. Runs inside the caller's
/// transaction so rows and revision are one consistent state.
fn fetch(conn: &Connection, path: &Path) -> Result<(Vec<Row>, u64)> {
    // A missing table or a non-text column is a malformed document, not an engine failure.
    let as_malformed = |err: rusqlite::Error| match err.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked | rusqlite::ErrorCode::SystemIoFailure | rusqlite::ErrorCode::PermissionDenied) => sql_error(path, "read", &err),
        _ => malformed(path, format!("unreadable document rows: {err}")),
    };
    let mut meta: HashMap<String, String> = HashMap::new();
    {
        let mut statement = conn.prepare("SELECT key, value FROM meta").map_err(as_malformed)?;
        let entries = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(as_malformed)?;
        for entry in entries {
            let (key, value) = entry.map_err(as_malformed)?;
            meta.insert(key, value);
        }
    }
    match meta.get("schema_version").map(String::as_str) {
        Some("1") => {}
        found => {
            ms_log::runtime_log::log_error(format!("docstore: unsupported document database schema.\nPath: {}\nmeta.schema_version: {found:?} (this build reads '1')", path.display()));
            return Err(DocStoreError::Unsupported { path: path.to_path_buf(), format: DocFormat::Db });
        }
    }
    let revision = meta
        .get("revision")
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| malformed(path, format!("meta.revision={:?} is not an unsigned integer", meta.get("revision"))))?;
    let mut statement = conn.prepare("SELECT path, parent, seg, kind, payload FROM frag").map_err(as_malformed)?;
    let records = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?)))
        .map_err(as_malformed)?;
    let mut rows = Vec::new();
    for record in records {
        let (row_path, parent, seg, kind, payload) = record.map_err(as_malformed)?;
        let kind = FragKind::parse(&kind).ok_or_else(|| malformed(path, format!("row {row_path:?} has unknown kind {kind:?}")))?;
        rows.push(Row { path: row_path, parent, seg, kind, payload });
    }
    Ok((rows, revision))
}

/// Joins `rows` read from `path`; a contract violation is `Malformed`.
fn join(path: &Path, rows: &[Row]) -> Result<Value> {
    split::join(rows).map_err(|cause| malformed(path, cause))
}

/// Reads the whole document at `path` and its revision (one read transaction).
///
/// # Errors
/// `Malformed` / `Unsupported` / `Storage` per the file header's mapping.
pub(crate) fn read_document(path: &Path) -> Result<(Value, u64)> {
    let mut conn = open_existing(path, false)?;
    let transaction = conn.transaction().map_err(|err| sql_error(path, "begin read", &err))?;
    let (rows, revision) = fetch(&transaction, path)?;
    // Read-only: ending the transaction by drop (rollback) releases the shared lock.
    drop(transaction);
    close(conn, path)?;
    Ok((join(path, &rows)?, revision))
}

/// `meta.revision` of the database at `path`.
///
/// # Errors
/// As [`read_document`].
pub(crate) fn read_revision(path: &Path) -> Result<u64> {
    let mut conn = open_existing(path, false)?;
    let transaction = conn.transaction().map_err(|err| sql_error(path, "begin read", &err))?;
    let revision: Option<String> = transaction.query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| row.get(0)).map_err(|err| malformed(path, format!("no meta.revision: {err}")))?;
    drop(transaction);
    close(conn, path)?;
    revision
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| malformed(path, "meta.revision is not an unsigned integer"))
}

/// Closes `conn`, surfacing a close failure (a leaked handle would block Windows renames).
fn close(conn: Connection, path: &Path) -> Result<()> {
    conn.close().map_err(|(_conn, err)| sql_error(path, "close", &err))
}

/// The serialized read-modify-write of an EXISTING database: under `BEGIN IMMEDIATE`
/// (the file's write lock, shared with the Python backend) read every row, join them and
/// hand the current value to `change`, which returns the new value (or an error: rollback,
/// nothing written). Then write the row diff: changed rows `UPDATE`d in place, new rows
/// `INSERT`ed, vanished rows `DELETE`d, and — only if any row changed — `revision + 1` and
/// `writer = 'rust'`, all in the same transaction.
///
/// # Errors
/// `change`'s error (rolled back), plus the read errors of [`read_document`] and engine
/// failures as `Storage`.
pub(crate) fn modify<R>(path: &Path, change: impl FnOnce(Value) -> Result<(Value, R)>) -> Result<(R, Committed)> {
    let mut conn = open_existing(path, true)?;
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|err| sql_error(path, "begin write", &err))?;
    let (old_rows, revision) = fetch(&transaction, path)?;
    let current = join(path, &old_rows)?;
    // An error returns here: dropping the transaction rolls it back.
    let (new_value, outcome) = change(current)?;
    let new_rows = split::split(&new_value);
    let touched = apply_diff(&transaction, path, &old_rows, &new_rows)?;
    let revision = if touched > 0 {
        let next = revision.checked_add(1).ok_or_else(|| malformed(path, "meta.revision overflowed"))?;
        transaction.execute("UPDATE meta SET value = ?1 WHERE key = 'revision'", [next.to_string()]).map_err(|err| sql_error(path, "bump revision", &err))?;
        transaction.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('writer', ?1)", [WRITER]).map_err(|err| sql_error(path, "set writer", &err))?;
        next
    } else {
        revision
    };
    transaction.commit().map_err(|err| sql_error(path, "commit", &err))?;
    close(conn, path)?;
    Ok((outcome, Committed { touched, revision }))
}

/// Writes the row diff between `old` and `new` (clarification 9 + the payload rule of
/// `split::payload_unchanged`). Returns the number of rows touched.
///
/// A changed row whose path already exists is rewritten with `UPDATE ... WHERE path = ?`,
/// never `INSERT OR REPLACE`: REPLACE deletes the row and re-inserts it under a NEW max
/// rowid, which migrates rewritten rows to the right-hand leaves and leaves freed pages
/// behind (measured: a long-lived `layers.db` at 68% free pages, and a one-row commit
/// dirtying ~7 pages spread over the file). An UPDATE keeps the rowid and rewrites the row
/// in its own leaf. Only paths absent from `old` are `INSERT`ed; vanished paths `DELETE`d.
fn apply_diff(conn: &Connection, path: &Path, old: &[Row], new: &[Row]) -> Result<usize> {
    let old_by_path: HashMap<&str, &Row> = old.iter().map(|row| (row.path.as_str(), row)).collect();
    let mut update = conn.prepare("UPDATE frag SET parent = ?1, seg = ?2, kind = ?3, payload = ?4 WHERE path = ?5").map_err(|err| sql_error(path, "prepare update", &err))?;
    let mut insert = conn.prepare("INSERT INTO frag(path, parent, seg, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)").map_err(|err| sql_error(path, "prepare insert", &err))?;
    let mut touched = 0usize;
    let mut new_paths = std::collections::HashSet::with_capacity(new.len());
    for row in new {
        new_paths.insert(row.path.as_str());
        match old_by_path.get(row.path.as_str()) {
            Some(previous)
                if previous.parent == row.parent
                    && previous.seg == row.seg
                    && previous.kind == row.kind
                    && split::payload_unchanged(row.kind, previous.payload.as_deref(), row.payload.as_deref()) =>
            {
                continue;
            }
            Some(_) => {
                let changed = update.execute(rusqlite::params![row.parent, row.seg, row.kind.as_str(), row.payload, row.path]).map_err(|err| sql_error(path, "update row", &err))?;
                // The row was read in this same IMMEDIATE transaction, so it must still exist;
                // anything else means the table changed under the write lock. Reported as an
                // engine-class failure (rolled back, retryable), never as `Malformed`: the
                // stored document itself was valid when read.
                if changed != 1 {
                    return Err(io_error(path, std::io::Error::other(format!("SQLite update of row {:?} changed {changed} rows inside the write transaction (expected 1)", row.path))));
                }
            }
            None => {
                insert.execute(rusqlite::params![row.path, row.parent, row.seg, row.kind.as_str(), row.payload]).map_err(|err| sql_error(path, "insert row", &err))?;
            }
        }
        touched += 1;
    }
    let mut delete = conn.prepare("DELETE FROM frag WHERE path = ?1").map_err(|err| sql_error(path, "prepare delete", &err))?;
    for row in old {
        if !new_paths.contains(row.path.as_str()) {
            delete.execute([row.path.as_str()]).map_err(|err| sql_error(path, "delete row", &err))?;
            touched += 1;
        }
    }
    Ok(touched)
}

/// Creates a NEW database at `path` (which must not exist) holding `value`: schema,
/// identity pragmas, `meta` (`revision` as given, `writer='rust'`) and every row, in one
/// transaction; the connection is closed before returning. The caller fsyncs and renames.
///
/// # Errors
/// `Storage` when the file exists or on any engine failure.
pub(crate) fn create(path: &Path, value: &Value, doc_kind: &str, revision: u64) -> Result<()> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if path.exists() {
        return Err(io_error(path, std::io::Error::new(std::io::ErrorKind::AlreadyExists, "refusing to create a database over an existing file")));
    }
    let mut conn = Connection::open_with_flags(path, flags).map_err(|err| sql_error(path, "create", &err))?;
    configure_writer(&conn, path)?;
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|err| sql_error(path, "begin create", &err))?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID).map_err(|err| sql_error(path, "set application_id", &err))?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(|err| sql_error(path, "set user_version", &err))?;
    transaction.execute_batch(SCHEMA_SQL).map_err(|err| sql_error(path, "create schema", &err))?;
    {
        let mut meta = transaction.prepare("INSERT INTO meta(key, value) VALUES (?1, ?2)").map_err(|err| sql_error(path, "prepare meta", &err))?;
        let revision = revision.to_string();
        for (key, text) in [("schema_version", "1"), ("doc_kind", doc_kind), ("revision", revision.as_str()), ("writer", WRITER), ("app_version", "")] {
            meta.execute([key, text]).map_err(|err| sql_error(path, "write meta", &err))?;
        }
        let mut insert = transaction.prepare("INSERT INTO frag(path, parent, seg, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)").map_err(|err| sql_error(path, "prepare rows", &err))?;
        for row in split::split(value) {
            insert.execute(rusqlite::params![row.path, row.parent, row.seg, row.kind.as_str(), row.payload]).map_err(|err| sql_error(path, "write row", &err))?;
        }
    }
    transaction.commit().map_err(|err| sql_error(path, "commit create", &err))?;
    close(conn, path)
}

/// Test helpers shared by the crate's tests (row dumps, raw pragma access).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// One `frag` row as `(path, parent, seg, kind, payload)`.
    pub(crate) type RowTuple = (String, Option<String>, Option<String>, String, Option<String>);

    /// Every `frag` row of `path`, sorted by path.
    pub(crate) fn dump_rows(path: &Path) -> Vec<RowTuple> {
        let conn = Connection::open(path).expect("open");
        let mut statement = conn.prepare("SELECT path, parent, seg, kind, payload FROM frag ORDER BY path").expect("prepare");
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).expect("query").map(|row| row.expect("row")).collect()
    }

    /// `meta` as a map.
    pub(crate) fn dump_meta(path: &Path) -> HashMap<String, String> {
        let conn = Connection::open(path).expect("open");
        let mut statement = conn.prepare("SELECT key, value FROM meta").expect("prepare");
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).expect("query").map(|row| row.expect("row")).collect()
    }

    /// Runs raw SQL on `path` (test setup: corrupting rows, switching to WAL, ...).
    pub(crate) fn exec(path: &Path, sql: &str) {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(sql).expect("exec");
    }

    /// Adds triggers logging every INSERT/UPDATE/DELETE on `frag` into `write_log`
    /// (`ins`/`upd`/`del`).
    pub(crate) fn install_write_log(path: &Path) {
        exec(
            path,
            "CREATE TABLE write_log(op TEXT, path TEXT);\
             CREATE TRIGGER log_ins AFTER INSERT ON frag BEGIN INSERT INTO write_log VALUES ('ins', NEW.path); END;\
             CREATE TRIGGER log_upd AFTER UPDATE ON frag BEGIN INSERT INTO write_log VALUES ('upd', NEW.path); END;\
             CREATE TRIGGER log_del AFTER DELETE ON frag BEGIN INSERT INTO write_log VALUES ('del', OLD.path); END;",
        );
    }

    /// Drains `write_log`, sorted.
    pub(crate) fn take_write_log(path: &Path) -> Vec<(String, String)> {
        let conn = Connection::open(path).expect("open");
        let mut entries: Vec<(String, String)> = {
            let mut statement = conn.prepare("SELECT op, path FROM write_log").expect("prepare");
            statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).expect("query").map(|row| row.expect("row")).collect()
        };
        conn.execute("DELETE FROM write_log", []).expect("clear");
        entries.sort();
        entries
    }
}
