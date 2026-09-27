/*
File: codec.rs

Purpose:
Per-format dispatch of every document operation: the public API in lib.rs resolves a
document's format (resolve.rs) and calls in here with it. JSON is the byte-file codec
(atomic temp + rename, json.rs); `Db` is the SQLite fragment codec (sqlite.rs, whole.rs),
compiled out on wasm32 where every `Db` arm is `Unsupported`.

Key functions:
- load_at() / read_value_at()      : one read of a document in a given format
- signature_at() / revision_at()   : change probes
- write_at()                       : whole-document write (JSON bytes / Db row diff)
- update_at()                      : the serialized read-modify-write
- remove_format()                  : delete one format's file (+ a `.db`'s journal)
- value_fingerprint()              : the `Db` baseline fingerprint

Notes:
- FINGERPRINTS ARE FORMAT-SPECIFIC but share one type: JSON fingerprints the exact bytes
  on disk (byte-identical to step 1); `Db` fingerprints the compact JSON of the joined
  document `Value` (canonical: BTreeMap keys, shortest float text). A baseline taken in
  one format never matches the other, so the first save after a conversion reports
  `Conflict` once and the caller's merge path runs — safe, never a silent overwrite.
- `Db` writes ignore the layout options (`pretty`, `trailing_newline`) and `Durability`:
  SQLite always commits with synchronous=FULL (a weaker mode could corrupt the file on
  power loss, which `Durability::None`'s "never torn" promise forbids).
- A `Db` write to an EXISTING file is a row diff under `BEGIN IMMEDIATE`, and the baseline
  check and the RMW read happen inside that transaction (cross-process safe with the
  Python writer). A NEW `.db` is created whole through a temp file + rename (whole.rs), so
  a crash never leaves a half-initialized database under the document's name.
*/

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::json::{AtomicWriteError, Fingerprint, SaveBaseline, fingerprint};
use crate::{DocFormat, DocRef, DocStoreError, Result, Signature, Snapshot, WriteOptions, fsio, resolve};

/// One read of a document, before parsing into the caller's shape.
// wasm32 has no SQLite codec, so `Db` is never constructed there (`load_db` is Unsupported).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) enum Loaded {
    /// The raw bytes of a JSON file (parsed lazily so `read<T>` keeps `from_slice` semantics).
    Json {
        /// The file read.
        path: PathBuf,
        /// Its bytes.
        bytes: Vec<u8>,
    },
    /// The joined document of a `.db` file.
    Db {
        /// The file read.
        path: PathBuf,
        /// The joined document.
        value: Value,
    },
}

impl Loaded {
    /// The document as a `Value`.
    pub(crate) fn into_value(self) -> Result<Value> {
        match self {
            Self::Json { path, bytes } => parse_value(&path, &bytes),
            Self::Db { value, .. } => Ok(value),
        }
    }

    /// The document plus its baseline fingerprint.
    pub(crate) fn into_snapshot(self) -> Result<Snapshot> {
        match self {
            Self::Json { path, bytes } => Ok(Snapshot { value: parse_value(&path, &bytes)?, fingerprint: fingerprint(&bytes) }),
            Self::Db { value, .. } => Ok(Snapshot { fingerprint: value_fingerprint(&value), value }),
        }
    }

    /// The document as `T` plus the fingerprint of the SAME state.
    pub(crate) fn into_typed<T: DeserializeOwned>(self) -> Result<(T, Fingerprint)> {
        match self {
            Self::Json { path, bytes } => {
                let typed = serde_json::from_slice(&bytes).map_err(|err| DocStoreError::Malformed { path, cause: err.to_string() })?;
                Ok((typed, fingerprint(&bytes)))
            }
            Self::Db { path, value } => {
                let typed = T::deserialize(&value).map_err(|err| DocStoreError::Malformed { path, cause: err.to_string() })?;
                Ok((typed, value_fingerprint(&value)))
            }
        }
    }
}

/// The `Db` baseline fingerprint: of the compact JSON text of `value`.
pub(crate) fn value_fingerprint(value: &Value) -> Fingerprint {
    fingerprint(value.to_string().as_bytes())
}

/// Parses `bytes` (read from `path`) as JSON; a failure is `Malformed`.
pub(crate) fn parse_value(path: &Path, bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|err| DocStoreError::Malformed { path: path.to_path_buf(), cause: err.to_string() })
}

/// Reads `doc` in `format`; `None` when that file is absent.
///
/// # Errors
/// `Unsupported` (`Db` on wasm32), `Storage` on I/O failure, and for `Db` the read errors
/// of the `SQLite` codec (`Malformed`, `Unsupported` for a newer schema).
pub(crate) fn load_at(doc: &DocRef, format: DocFormat) -> Result<Option<Loaded>> {
    let path = doc.path_for(format);
    if !fsio::exists(&path) {
        return Ok(None);
    }
    resolve::ensure_supported(doc, format)?;
    match format {
        // Removed between the probe and the read: `fsio::read` reports it as absent.
        DocFormat::Json => Ok(fsio::read(&path)?.map(|bytes| Loaded::Json { path, bytes })),
        DocFormat::Db => load_db(path),
    }
}

/// Reads and parses `doc` in `format`; `None` when that file is absent.
///
/// # Errors
/// As [`load_at`], plus `Malformed` for JSON that does not parse.
pub(crate) fn read_value_at(doc: &DocRef, format: DocFormat) -> Result<Option<Value>> {
    load_at(doc, format)?.map(Loaded::into_value).transpose()
}

#[cfg(not(target_arch = "wasm32"))]
fn load_db(path: PathBuf) -> Result<Option<Loaded>> {
    let (value, _revision) = crate::sqlite::read_document(&path)?;
    Ok(Some(Loaded::Db { path, value }))
}

#[cfg(target_arch = "wasm32")]
fn load_db(path: PathBuf) -> Result<Option<Loaded>> {
    Err(DocStoreError::Unsupported { path, format: DocFormat::Db })
}

/// The signature of `doc` in `format`; `None` when that file is absent. Both formats report
/// `Signature::Bytes` of their file (a `.db` file's length/mtime change on every commit
/// that changes rows; an empty-diff write does not touch it).
///
/// # Errors
/// `Storage` when an existing file cannot be stated; `Unsupported` for `Db` on wasm32.
pub(crate) fn signature_at(doc: &DocRef, format: DocFormat) -> Result<Option<Signature>> {
    let path = doc.path_for(format);
    if !fsio::exists(&path) {
        return Ok(None);
    }
    resolve::ensure_supported(doc, format)?;
    Ok(fsio::metadata(&path)?.map(|meta| {
        let mtime_ns = meta.modified.and_then(|time| time.duration_since(web_time::UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_nanos());
        Signature::Bytes { len: meta.len, mtime_ns }
    }))
}

/// `meta.revision` of `doc` when it is a `.db` document; `None` when it is absent or JSON.
///
/// # Errors
/// The read errors of the `SQLite` codec.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn revision_at(doc: &DocRef, format: DocFormat) -> Result<Option<u64>> {
    let path = doc.path_for(format);
    match format {
        DocFormat::Json => Ok(None),
        DocFormat::Db if !fsio::exists(&path) => Ok(None),
        DocFormat::Db => crate::sqlite::read_revision(&path).map(Some),
    }
}

/// wasm32: there are no `.db` documents.
#[cfg(target_arch = "wasm32")]
// Same signature as the native variant, which does fail.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn revision_at(_doc: &DocRef, _format: DocFormat) -> Result<Option<u64>> {
    Ok(None)
}

/// Enforces `baseline` against a document whose current fingerprint is `found`.
fn check_found(path: &Path, baseline: SaveBaseline, found: Fingerprint) -> Result<()> {
    if baseline.accepts(found) { Ok(()) } else { Err(DocStoreError::Conflict { path: path.to_path_buf(), found }) }
}

/// [`check_found`] for a `Db` document whose joined value is `current`. The fingerprint
/// (a compact serialization + SHA-256 of the whole document) is computed only when the
/// baseline actually needs it: `Unchecked` accepts anything.
#[cfg(not(target_arch = "wasm32"))]
fn check_db_baseline(path: &Path, baseline: SaveBaseline, current: &Value) -> Result<()> {
    if baseline == SaveBaseline::Unchecked { Ok(()) } else { check_found(path, baseline, value_fingerprint(current)) }
}

/// Serializes `value` per `opts` and writes it as the whole document in `format`. Caller
/// holds the document lock. Returns the fingerprint of the written state (JSON: its bytes;
/// `Db`: its compact Value text). Failures are logged here (except `DirSync`).
///
/// # Errors
/// `Conflict`, `Serialize`, `Unsupported`, and the I/O errors of the codec.
pub(crate) fn write_at<T: Serialize + ?Sized>(doc: &DocRef, format: DocFormat, value: &T, opts: WriteOptions) -> Result<Fingerprint> {
    resolve::ensure_supported(doc, format)?;
    let path = doc.path_for(format);
    let result = match format {
        DocFormat::Json => write_json(&path, value, opts),
        DocFormat::Db => {
            let value = serde_json::to_value(value).map_err(|err| DocStoreError::Serialize { path: path.clone(), cause: err.to_string() })?;
            write_db(doc, &path, value, opts)
        }
    };
    result.inspect_err(|err| log_write_failure(&path, err))
}

/// JSON arm of [`write_at`]: `T` is serialized directly (struct field order kept).
fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T, opts: WriteOptions) -> Result<Fingerprint> {
    let mut text = if opts.pretty { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) }
        .map_err(|err| DocStoreError::Serialize { path: path.to_path_buf(), cause: err.to_string() })?;
    if opts.trailing_newline {
        text.push('\n');
    }
    if opts.baseline != SaveBaseline::Unchecked
        // Nothing on disk: any baseline may proceed (a `Matching` baseline whose file
        // vanished has nothing left to preserve).
        && let Some(current) = fsio::read(path)?
    {
        check_found(path, opts.baseline, fingerprint(&current))?;
    }
    write_bytes(path, text.as_bytes(), opts)?;
    Ok(fingerprint(text.as_bytes()))
}

/// `Db` arm of [`write_at`] (native).
#[cfg(not(target_arch = "wasm32"))]
fn write_db(doc: &DocRef, path: &Path, value: Value, opts: WriteOptions) -> Result<Fingerprint> {
    let written = value_fingerprint(&value);
    if fsio::exists(path) {
        crate::sqlite::modify(path, |current| {
            check_db_baseline(path, opts.baseline, &current)?;
            Ok((value, ()))
        })?;
    } else {
        crate::whole::write_whole(doc, &value, DocFormat::Db, opts.create_parent_dirs)?;
    }
    Ok(written)
}

/// wasm32: unreachable after `ensure_supported`.
#[cfg(target_arch = "wasm32")]
fn write_db(_doc: &DocRef, path: &Path, _value: Value, _opts: WriteOptions) -> Result<Fingerprint> {
    Err(DocStoreError::Unsupported { path: path.to_path_buf(), format: DocFormat::Db })
}

/// The serialized read-modify-write in `format`. Caller holds the document lock. An absent
/// document starts as `{}`; a malformed one fails with `Malformed` before the mutator runs.
/// JSON: always rewritten after a successful mutator. `Db`: read, mutation and row diff in
/// ONE `BEGIN IMMEDIATE` transaction; an unchanged document writes nothing.
///
/// # Errors
/// `Malformed`, `Mutator` (nothing written), `Conflict` (`opts.baseline`), and the write
/// errors of [`write_at`].
pub(crate) fn update_at<R>(doc: &DocRef, format: DocFormat, opts: WriteOptions, mutator: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R> {
    resolve::ensure_supported(doc, format)?;
    match format {
        DocFormat::Json => {
            let mut root = read_value_at(doc, format)?.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let outcome = mutator(&mut root).map_err(DocStoreError::Mutator)?;
            write_at(doc, format, &root, opts)?;
            Ok(outcome)
        }
        DocFormat::Db => update_db(doc, opts, mutator),
    }
}

/// `Db` arm of [`update_at`] (native).
#[cfg(not(target_arch = "wasm32"))]
fn update_db<R>(doc: &DocRef, opts: WriteOptions, mutator: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R> {
    let path = doc.path_for(DocFormat::Db);
    let result = if fsio::exists(&path) {
        crate::sqlite::modify(&path, |mut current| {
            check_db_baseline(&path, opts.baseline, &current)?;
            let outcome = mutator(&mut current).map_err(DocStoreError::Mutator)?;
            Ok((current, outcome))
        })
        .map(|(outcome, _committed)| outcome)
    } else {
        let mut root = Value::Object(serde_json::Map::new());
        let outcome = mutator(&mut root).map_err(DocStoreError::Mutator)?;
        crate::whole::write_whole(doc, &root, DocFormat::Db, opts.create_parent_dirs).map(|()| outcome)
    };
    result.inspect_err(|err| log_write_failure(&path, err))
}

/// wasm32: unreachable after `ensure_supported`.
#[cfg(target_arch = "wasm32")]
fn update_db<R>(doc: &DocRef, _opts: WriteOptions, _mutator: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R> {
    Err(DocStoreError::Unsupported { path: doc.path_for(DocFormat::Db), format: DocFormat::Db })
}

/// Native: creates the parent if asked, then the atomic temp + rename recipe.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_bytes(path: &Path, bytes: &[u8], opts: WriteOptions) -> Result<()> {
    if opts.create_parent_dirs
        && let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| DocStoreError::Io { path: parent.to_path_buf(), source })?;
    }
    crate::json::write_atomic(path, bytes, opts.durability)?;
    Ok(())
}

/// wasm32: a plain seam write (the in-memory store has neither rename atomicity worth
/// relying on nor fsync); `durability` is ignored.
#[cfg(target_arch = "wasm32")]
pub(crate) fn write_bytes(path: &Path, bytes: &[u8], opts: WriteOptions) -> Result<()> {
    let store = ms_storage::global::storage();
    if opts.create_parent_dirs
        && let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
    {
        store.create_dir_all(parent.to_string_lossy().as_ref())?;
    }
    store.write(path.to_string_lossy().as_ref(), bytes)?;
    Ok(())
}

/// Removes `doc`'s file in `format` (absent is fine). For `Db` the rollback journal goes
/// first: a stale `-journal` would be replayed into the next database created under that
/// name. Caller holds the document lock.
///
/// # Errors
/// `Storage` when an existing file cannot be removed (logged).
pub(crate) fn remove_format(doc: &DocRef, format: DocFormat) -> Result<()> {
    let path = doc.path_for(format);
    let mut targets = Vec::with_capacity(2);
    #[cfg(not(target_arch = "wasm32"))]
    if format == DocFormat::Db {
        targets.push(crate::whole::journal_path(&path));
    }
    targets.push(path);
    for target in targets {
        if let Err(err) = fsio::remove(&target) {
            ms_log::runtime_log::log_error(format!("docstore: failed to remove document.\nPath: {}\nError: {err}", target.display()));
            return Err(err.into());
        }
    }
    Ok(())
}

/// Structured log line for a failed document write (AGENTS.md §7/§8); only I/O-class
/// failures are logged, the rest are the caller's to report. `DirSync` is NOT
/// logged here: the new file is in place, and only the caller knows whether an undurable
/// directory entry matters (it logs that case once, at the severity it chooses).
pub(crate) fn log_write_failure(path: &Path, err: &DocStoreError) {
    let cause = match err {
        DocStoreError::Write(AtomicWriteError::TempWrite { .. } | AtomicWriteError::Rename { .. }) | DocStoreError::Io { .. } | DocStoreError::Storage(_) => "file-system failure (permissions, disk full, missing or read-only directory, a file held open by another process, or an SQLite lock held past the 5 s timeout)",
        // `DirSync`: the caller decides its severity. The rest are not I/O failures: the
        // caller receives them and decides (surface, merge, retry).
        DocStoreError::Write(AtomicWriteError::DirSync { .. })
        | DocStoreError::Conflict { .. }
        | DocStoreError::Mutator(_)
        | DocStoreError::Malformed { .. }
        | DocStoreError::Serialize { .. }
        | DocStoreError::Ambiguous { .. }
        | DocStoreError::Unsupported { .. }
        | DocStoreError::Quarantine { .. } => return,
    };
    ms_log::runtime_log::log_error(format!("docstore: failed to write document.\nPath: {}\nError: {err}\nPossible cause: {cause}", path.display()));
}
