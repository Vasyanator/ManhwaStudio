/*
File: lib.rs

Purpose:
Public entry point of `ms-docstore`, the single owner of "read / update / write a named
logical document" (user_config, fonts_data, presets, project settings, characters, terms,
favorites, color presets, chapter layers and bubbles). Callers name a document by a
`DocRef` (extension-less path + kind) and never touch its file directly.

Key structures:
- DocRef / DocKind / DocFormat : naming a logical document and its on-disk format
- WriteOptions                 : pretty/newline layout, durability, optimistic baseline
- Signature / Snapshot         : cheap change probe; value + fingerprint of one read
- LockedDoc                    : operations inside `with_lock` (format resolved once)
- QuarantineNaming / Quarantined : preserving a malformed document under a sidecar name
- DocStoreError                : typed failure surface (technical Display; callers localize)

Key functions:
- read_value / read / read_snapshot / signature / revision / exists / actual_format
  (unlocked reads)
- write_value / write / update / remove / quarantine / copy_document / write_whole_atomic
  (locked mutations)
- with_lock (multi-step critical sections), set_default_format / default_format,
  chapter_new_format (new chapter documents follow their siblings' format)
- convert_document / convert_many / chapter_format_report (convert.rs, re-exported)
- is_temp_artifact (skip crash-leftover temp files when copying document trees)
- write_bytes_atomic (native: the atomic write recipe for non-document files, e.g. a
  user image saved in place; json.rs, re-exported)

Notes:
- Two formats: `<stem>.json` (JSON codec, json.rs) and `<stem>.db` (SQLite fragment codec,
  split.rs + sqlite.rs + whole.rs; native only — `Db` is `Unsupported` on wasm32). Every
  operation resolves the document's format first (resolve.rs) and dispatches (codec.rs).
- Every write is serialized per document by a process-local, NON-reentrant lock
  (lock.rs). Cross-process: `.db` writes are `BEGIN IMMEDIATE` transactions (safe with the
  Python writer); `.json` writers are last-writer-wins; `SaveBaseline` is the opt-in
  optimistic check for documents that need it.
- A MALFORMED document is never overwritten by `update`: it fails with `Malformed` and the
  file is left untouched. Plain `write_value` replaces whatever is there by design.
- Native: reads, stats and writes all address the real `Path` through `std::fs` (fsio.rs,
  json.rs: atomic temp + rename). wasm32: everything goes through
  `ms_storage::global::storage()`; a write there is a plain seam `write`.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate is named after the domain concept its types export (`DocStoreError`, ...).
#![allow(clippy::module_name_repetitions)]

mod codec;
mod convert;
mod fsio;
mod json;
mod lock;
mod resolve;
#[cfg(not(target_arch = "wasm32"))]
mod split;
#[cfg(not(target_arch = "wasm32"))]
mod sqlite;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_db;
#[cfg(not(target_arch = "wasm32"))]
mod whole;

use std::cell::Cell;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use json::{AtomicWriteError, Durability, FaultPoint, Fingerprint, SaveBaseline, WriteStep, fingerprint, is_temp_artifact, temp_path_for};
#[cfg(not(target_arch = "wasm32"))]
pub use json::write_bytes_atomic;
#[cfg(any(test, feature = "test-support"))]
pub use json::{arm_fault, recorded_steps};
pub use convert::{BatchOutcome, ChapterFormatReport, ConvertHook, ConvertOutcome, ConvertStep, NoHook, chapter_format_report, convert_document, convert_many};
pub use resolve::{chapter_new_format, default_format, set_default_format};
/// The storage seam's error, carried by [`DocStoreError::Storage`]; re-exported so callers
/// can inspect it (e.g. recover the OS `io::ErrorKind`) without depending on `ms-storage`.
pub use ms_storage::StorageError;

/// Result alias of every store operation.
pub type Result<T, E = DocStoreError> = std::result::Result<T, E>;

/// On-disk format of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocFormat {
    /// `<stem>.json`, pretty-printed JSON.
    Json,
    /// `<stem>.db`, `SQLite` fragment store (native only; [`DocStoreError::Unsupported`] on
    /// wasm32).
    Db,
}

impl DocFormat {
    /// File extension of this format, without the dot.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Db => "db",
        }
    }
}

/// Which logical document a [`DocRef`] names. The `SQLite` codec records it in the file's
/// `meta.doc_kind` ([`DocKind::as_str`]); it carries no other behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    /// `user_config.json` of a data root.
    UserConfig,
    /// `fonts/fonts_data.json`.
    FontsData,
    /// `fonts/presets.json`.
    Presets,
    /// A title's `settings.json`.
    ProjectSettings,
    /// A title's `characters.json`.
    Characters,
    /// A title's `terms.json`.
    Terms,
    /// A title's character-table favorites.
    CharFavorites,
    /// A title's color presets.
    ColorPresets,
    /// A chapter's `layers.json` manifest.
    Layers,
    /// A chapter's `bubbles.json`.
    Bubbles,
}

impl DocKind {
    /// The `snake_case` name stored in a `.db` file's `meta.doc_kind` (frozen spelling).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserConfig => "user_config",
            Self::FontsData => "fonts_data",
            Self::Presets => "presets",
            Self::ProjectSettings => "project_settings",
            Self::Characters => "characters",
            Self::Terms => "terms",
            Self::CharFavorites => "char_favorites",
            Self::ColorPresets => "color_presets",
            Self::Layers => "layers",
            Self::Bubbles => "bubbles",
        }
    }
}

/// Name of one logical document: its path WITHOUT a format extension plus its kind, and an
/// optional format for creating it when it does not exist yet (see [`DocRef::with_new_format`]).
///
/// Identity (`==`, `Hash`) is the stem and kind only; two `DocRef`s with equal stems name
/// the same document and share one write lock whatever their new-document hints.
#[derive(Debug, Clone)]
pub struct DocRef {
    stem: PathBuf,
    kind: DocKind,
    new_format: Option<DocFormat>,
}

impl PartialEq for DocRef {
    fn eq(&self, other: &Self) -> bool {
        self.stem == other.stem && self.kind == other.kind
    }
}

impl Eq for DocRef {}

impl std::hash::Hash for DocRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.stem.hash(state);
        self.kind.hash(state);
    }
}

impl DocRef {
    /// Names the document at `path`. A trailing `.json` or `.db` extension is stripped, so
    /// `DocRef::new("x/user_config.json", ..)` and `DocRef::new("x/user_config", ..)` are
    /// equal. Any other extension is kept as part of the stem.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, kind: DocKind) -> Self {
        let mut stem: PathBuf = path.into();
        let known = stem.extension().is_some_and(|ext| ext == DocFormat::Json.extension() || ext == DocFormat::Db.extension());
        if known {
            stem.set_extension("");
        }
        Self { stem, kind, new_format: None }
    }

    /// The format this document is created in when NO file of it exists yet (rule B.3),
    /// overriding `default_format()`; ignored once the document exists. Chapter callers
    /// pass [`chapter_new_format`] of the chapter's sibling documents so a new `layers` or
    /// `bubbles` joins the format the chapter already uses.
    #[must_use]
    pub fn with_new_format(mut self, format: DocFormat) -> Self {
        self.new_format = Some(format);
        self
    }

    /// The new-document format set by [`DocRef::with_new_format`], if any.
    #[must_use]
    pub fn new_format_hint(&self) -> Option<DocFormat> {
        self.new_format
    }

    /// The extension-less path of the document.
    #[must_use]
    pub fn stem(&self) -> &Path {
        &self.stem
    }

    /// The kind of the document.
    #[must_use]
    pub fn kind(&self) -> DocKind {
        self.kind
    }

    /// The file path the document has in `format` (`<stem>.<ext>`). Callers use it for
    /// diagnostics and sidecar names (e.g. a quarantine `.bad` copy), never to write it.
    #[must_use]
    pub fn path_for(&self, format: DocFormat) -> PathBuf {
        let mut name = self.stem.clone().into_os_string();
        name.push(".");
        name.push(format.extension());
        PathBuf::from(name)
    }
}

/// Cheap change probe of a document: equal signatures mean "very probably unchanged".
///
/// `Bytes` compares length and modification time, so two same-length writes inside the
/// filesystem's timestamp resolution are indistinguishable (and backends without mtimes,
/// like the web store, report `None`). Use [`read_snapshot`]'s fingerprint when an exact
/// answer is required. Both formats report `Bytes` of their file (a `.db` changes on
/// every commit that changes rows); [`revision`] is the exact counter of a `.db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signature {
    /// The document's file (either format).
    Bytes {
        /// File length in bytes.
        len: u64,
        /// Modification time in nanoseconds since the Unix epoch, when the backend has one.
        mtime_ns: Option<u128>,
    },
}

/// One read of a document: its parsed value and the fingerprint of the state read (JSON:
/// the exact bytes; `Db`: the compact text of the value), which is the baseline for a
/// later [`SaveBaseline::Matching`] write.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The parsed document.
    pub value: Value,
    /// Fingerprint of the state the value was read from (see the type docs).
    pub fingerprint: Fingerprint,
}

/// How a document is written.
///
/// The default reproduces the historical `user_config.json` layout: 2-space pretty JSON
/// (`serde_json::to_string_pretty`), no trailing newline, contents-only durability, no
/// baseline check, missing parent directories created. The `fonts/` documents and the
/// title stores historically end with a newline and set `trailing_newline`.
///
/// For a `.db` document only `baseline` and `create_parent_dirs` apply: the layout fields
/// have no meaning there and `SQLite` always commits fully synced (`durability` ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent layout/behavior switches, not a state machine.
pub struct WriteOptions {
    /// Fsync policy of the replacement (ignored on wasm32).
    pub durability: Durability,
    /// Optimistic-concurrency expectation checked under the lock before writing. A file
    /// that is absent at write time never blocks a write.
    pub baseline: SaveBaseline,
    /// 2-space pretty output (`true`) or compact output.
    pub pretty: bool,
    /// Append one `\n` after the JSON text.
    pub trailing_newline: bool,
    /// Create missing parent directories before writing (`false`: a missing parent is an
    /// error, for documents whose directory must already exist).
    pub create_parent_dirs: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self { durability: Durability::Contents, baseline: SaveBaseline::Unchecked, pretty: true, trailing_newline: false, create_parent_dirs: true }
    }
}

/// Typed failure of a store operation. `Display` is technical (paths and OS reasons) and
/// meant for logs; user-facing text is produced by the caller, which knows the document's
/// meaning and owns the localization keys.
#[derive(Debug, thiserror::Error)]
pub enum DocStoreError {
    /// A native file-system operation outside the atomic-write recipe failed (e.g.
    /// creating the parent directory).
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        /// Path the operation was performed on.
        path: PathBuf,
        /// Original OS error.
        #[source]
        source: std::io::Error,
    },
    /// The storage seam failed while reading, stating or removing a document, or the
    /// `SQLite` engine failed (busy past its 5 s timeout — `io::ErrorKind::ResourceBusy` —,
    /// I/O, permissions).
    #[error("storage error: {0}")]
    Storage(#[from] ms_storage::StorageError),
    /// The atomic write recipe failed; the variant says what is already on disk.
    #[error("atomic write failed: {0}")]
    Write(#[from] AtomicWriteError),
    /// The document exists but does not parse (or does not match the requested type); for
    /// a `.db`: header sniff failed, foreign `application_id`, corrupt file, or rows that
    /// violate the fragment contract. It is NEVER overwritten by `update`; the caller
    /// decides (surface, quarantine).
    #[error("malformed document {}: {cause}", path.display())]
    Malformed {
        /// The document file.
        path: PathBuf,
        /// Parser message.
        cause: String,
    },
    /// The value could not be serialized; nothing was written.
    #[error("cannot serialize document {}: {cause}", path.display())]
    Serialize {
        /// The document file.
        path: PathBuf,
        /// Serializer message.
        cause: String,
    },
    /// The document no longer matches the caller's [`SaveBaseline`]; nothing was written.
    #[error("document {} was changed by someone else", path.display())]
    Conflict {
        /// The document file.
        path: PathBuf,
        /// Fingerprint of what is on disk now (the caller's next baseline after a merge).
        found: Fingerprint,
    },
    /// Both formats of one document exist and their contents differ (rule B.4); nothing
    /// was deleted.
    #[error("both {} and {} exist and differ: {cause}", json.display(), db.display())]
    Ambiguous {
        /// The JSON file.
        json: PathBuf,
        /// The `SQLite` file.
        db: PathBuf,
        /// What differs.
        cause: String,
    },
    /// The document's format cannot be handled by this build: `Db` on wasm32, or a `.db`
    /// written with a newer schema (`user_version` / `meta.schema_version`).
    #[error("unsupported document format {format:?} for {}", path.display())]
    Unsupported {
        /// The document file in that format.
        path: PathBuf,
        /// The format.
        format: DocFormat,
    },
    /// The caller's mutator failed; nothing was written.
    #[error("document update rejected: {0}")]
    Mutator(String),
    /// A quarantine could not preserve the document under a sidecar name; the document
    /// itself is left where it was. See [`quarantine`].
    #[error("cannot quarantine {} as {}: {}", path.display(), destination.display(), quarantine_reason(rename_error, copy_error.as_deref()))]
    Quarantine {
        /// The document file.
        path: PathBuf,
        /// The (first) destination tried.
        destination: PathBuf,
        /// Why the move aside failed (OS reason, or "no free destination" for
        /// [`QuarantineNaming::FirstFree`]).
        rename_error: String,
        /// Why the fallback copy failed; `None` when no copy was attempted.
        copy_error: Option<String>,
    },
}

/// Display text of [`DocStoreError::Quarantine`]'s failed steps.
fn quarantine_reason(rename_error: &str, copy_error: Option<&str>) -> String {
    match copy_error {
        Some(copy_error) => format!("rename failed: {rename_error}; copy failed: {copy_error}"),
        None => format!("rename failed: {rename_error}"),
    }
}

/// How [`quarantine`] names the sidecar that preserves a malformed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineNaming {
    /// `<file>.<suffix>`; an older quarantined copy under that name is replaced.
    Replace,
    /// The first free name among `<file>.<suffix>`, `<file>.<suffix>.1`, `<file>.<suffix>.2`,
    /// … (`max_candidates` probes), so an earlier quarantined copy is never overwritten.
    FirstFree {
        /// How many names are probed before giving up.
        max_candidates: u32,
    },
}

/// What [`quarantine`] may do when the rename aside fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineFallback {
    /// Only a rename counts; its failure is the quarantine's failure.
    RenameOnly,
    /// Copy the bytes to the destination instead: the original stays in place, but a
    /// recoverable second copy exists, so replacing the original is safe.
    CopyIfRenameFails,
}

/// Successful outcome of [`quarantine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quarantined {
    /// There was no document file to preserve.
    Absent,
    /// The document was renamed to this path; its own path is free now.
    Moved(PathBuf),
    /// The rename failed, but a copy reached `destination`; the original is still in place.
    Copied {
        /// Where the copy lives.
        destination: PathBuf,
        /// Why the rename failed.
        rename_error: String,
    },
}

/// Operations on one document while its write lock is held (see [`with_lock`]). None of
/// them re-acquires the lock, so they may be combined freely inside the closure.
///
/// The document's format is resolved ONCE per critical section, by the first method that
/// needs it (its existing format — running the both-exist repair of rule B.4 when needed —
/// else the new-document format); every method addresses that one format, so a critical
/// section never sees two formats of one document. A resolution failure (e.g. `Ambiguous`)
/// is returned by every method until it is fixed.
#[derive(Debug)]
pub struct LockedDoc<'a> {
    doc: &'a DocRef,
    format: Cell<Option<DocFormat>>,
}

impl LockedDoc<'_> {
    /// The locked document.
    #[must_use]
    pub fn doc(&self) -> &DocRef {
        self.doc
    }

    /// The format every method of this section addresses (resolved on first use).
    fn format(&self) -> Result<DocFormat> {
        if let Some(format) = self.format.get() {
            return Ok(format);
        }
        let format = resolve::resolve_locked(self.doc)?;
        self.format.set(Some(format));
        Ok(format)
    }

    /// The document's format when present, `None` when absent.
    ///
    /// # Errors
    /// The resolution errors: `Malformed` (a lone `.db` failing the header sniff, or the
    /// authoritative file of a both-exist pair not parsing), `Ambiguous`, `Storage`.
    pub fn actual_format(&self) -> Result<Option<DocFormat>> {
        let format = self.format()?;
        Ok(fsio::exists(&self.doc.path_for(format)).then_some(format))
    }

    /// See [`signature`].
    ///
    /// # Errors
    /// As [`signature`].
    pub fn signature(&self) -> Result<Option<Signature>> {
        codec::signature_at(self.doc, self.format()?)
    }

    /// See [`read_value`].
    ///
    /// # Errors
    /// As [`read_value`].
    pub fn read_value(&self) -> Result<Option<Value>> {
        codec::read_value_at(self.doc, self.format()?)
    }

    /// See [`read_snapshot`].
    ///
    /// # Errors
    /// As [`read_snapshot`].
    pub fn read_snapshot(&self) -> Result<Option<Snapshot>> {
        codec::load_at(self.doc, self.format()?)?.map(codec::Loaded::into_snapshot).transpose()
    }

    /// See [`read`].
    ///
    /// # Errors
    /// As [`read`].
    pub fn read<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        Ok(self.read_typed_snapshot()?.map(|(value, _)| value))
    }

    /// Reads the document as `T` together with the fingerprint of the SAME state it was
    /// parsed from; `None` when absent. Pass the fingerprint as a
    /// [`SaveBaseline::Matching`] baseline so a later write is checked against exactly the
    /// state the typed value describes (two separate reads could straddle a foreign write).
    ///
    /// # Errors
    /// As [`read`].
    pub fn read_typed_snapshot<T: DeserializeOwned>(&self) -> Result<Option<(T, Fingerprint)>> {
        codec::load_at(self.doc, self.format()?)?.map(codec::Loaded::into_typed).transpose()
    }

    /// Writes `value` as the whole document; see [`write`]. Returns the fingerprint of the
    /// written state.
    ///
    /// # Errors
    /// As [`write`].
    pub fn write<T: Serialize + ?Sized>(&self, value: &T, opts: WriteOptions) -> Result<Fingerprint> {
        codec::write_at(self.doc, self.format()?, value, opts)
    }

    /// Writes `value` as the whole document; see [`write_value`].
    ///
    /// # Errors
    /// As [`write_value`].
    pub fn write_value(&self, value: &Value, opts: WriteOptions) -> Result<Fingerprint> {
        codec::write_at(self.doc, self.format()?, value, opts)
    }

    /// The serialized read-modify-write; see [`update`].
    ///
    /// # Errors
    /// As [`update`].
    pub fn update<R>(&self, opts: WriteOptions, mutator: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R> {
        codec::update_at(self.doc, self.format()?, opts, mutator)
    }

    /// Removes the document; see [`remove`].
    ///
    /// # Errors
    /// As [`remove`].
    pub fn remove(&self) -> Result<()> {
        remove_all_formats(self.doc)
    }

    /// Preserves the document's file under a sidecar name; see [`quarantine`].
    ///
    /// # Errors
    /// As [`quarantine`].
    pub fn quarantine(&self, suffix: &str, naming: QuarantineNaming, fallback: QuarantineFallback) -> Result<Quarantined> {
        // A malformed document may be exactly what defeats resolution (a lone `.db` failing
        // the sniff): quarantine then addresses the one file that exists.
        let format = match self.format() {
            Ok(format) => format,
            Err(_) => resolve::known_formats().iter().copied().find(|format| fsio::exists(&self.doc.path_for(*format))).unwrap_or_else(default_format),
        };
        quarantine_at(self.doc, format, suffix, naming, fallback)
    }
}

/// Whether the document exists in any supported format. A probe failure reads as absent.
#[must_use]
pub fn exists(doc: &DocRef) -> bool {
    matches!(actual_format(doc), Ok(Some(_)))
}

/// The format the document currently has on disk, or `None` when it is absent. Stats the
/// files and sniffs a `.db` header; when both files exist, reports `default_format()` (the
/// authoritative one, rule B.4) without repairing anything.
///
/// # Errors
/// `Malformed` for a lone `.db` failing the header sniff, `Storage` when it cannot be read.
pub fn actual_format(doc: &DocRef) -> Result<Option<DocFormat>> {
    resolve::detect(doc)
}

/// Cheap change probe of the document, `None` when it is absent. Unlocked.
///
/// # Errors
/// [`DocStoreError::Storage`] when an existing file cannot be stated; the errors of
/// [`actual_format`].
pub fn signature(doc: &DocRef) -> Result<Option<Signature>> {
    let Some(format) = resolve::detect(doc)? else { return Ok(None) };
    codec::signature_at(doc, format)
}

/// The exact change counter of a `.db` document (`meta.revision`: +1 on every commit that
/// changes rows, by either implementation); `None` when the document is absent or JSON.
/// Unlocked.
///
/// # Errors
/// The errors of [`actual_format`] and the `.db` read errors of [`read_value`].
pub fn revision(doc: &DocRef) -> Result<Option<u64>> {
    let Some(format) = resolve::detect(doc)? else { return Ok(None) };
    codec::revision_at(doc, format)
}

/// Reads and parses the whole document; `None` when it is absent. Unlocked: a JSON file is
/// never torn (atomic rename) and a `.db` is read in one transaction, but a value read here
/// and written back later is a stale snapshot — use [`update`] for read-modify-write.
///
/// # Errors
/// [`DocStoreError::Malformed`] when the file does not parse (or a `.db` violates the
/// fragment contract), [`DocStoreError::Unsupported`] for a newer `.db` schema,
/// [`DocStoreError::Storage`] when it cannot be read.
pub fn read_value(doc: &DocRef) -> Result<Option<Value>> {
    let Some(format) = resolve::detect(doc)? else { return Ok(None) };
    codec::read_value_at(doc, format)
}

/// Like [`read_value`], plus the fingerprint of the state read (the baseline for a later
/// [`SaveBaseline::Matching`] write).
///
/// # Errors
/// As [`read_value`].
pub fn read_snapshot(doc: &DocRef) -> Result<Option<Snapshot>> {
    let Some(format) = resolve::detect(doc)? else { return Ok(None) };
    codec::load_at(doc, format)?.map(codec::Loaded::into_snapshot).transpose()
}

/// Reads the document and deserializes it into `T`; `None` when it is absent.
///
/// # Errors
/// [`DocStoreError::Malformed`] when the file does not parse as `T`, and the other errors
/// of [`read_value`].
pub fn read<T: DeserializeOwned>(doc: &DocRef) -> Result<Option<T>> {
    let Some(format) = resolve::detect(doc)? else { return Ok(None) };
    Ok(codec::load_at(doc, format)?.map(codec::Loaded::into_typed::<T>).transpose()?.map(|(value, _)| value))
}

/// Replaces the whole document with `value`, under the document lock. Returns the
/// fingerprint of the written state. Whatever is on disk (even a malformed JSON file) is
/// replaced unless `opts.baseline` rejects it; a `.db` is updated by a row diff (a
/// malformed `.db` fails with `Malformed` and is left alone).
///
/// # Errors
/// [`DocStoreError::Conflict`] when the baseline does not accept the current document,
/// [`DocStoreError::Unsupported`] for `Db` on wasm32, [`DocStoreError::Write`] /
/// [`DocStoreError::Io`] / [`DocStoreError::Storage`] on I/O failure (logged, except
/// `AtomicWriteError::DirSync`, which the caller logs).
pub fn write_value(doc: &DocRef, value: &Value, opts: WriteOptions) -> Result<Fingerprint> {
    with_lock(doc, |locked| locked.write_value(value, opts))
}

/// Serializes `value` directly (for JSON, struct fields keep their declaration order,
/// unlike a round trip through `Value`; for `Db` it goes through `Value`) and replaces the
/// whole document with it; see [`write_value`].
///
/// # Errors
/// As [`write_value`], plus [`DocStoreError::Serialize`].
pub fn write<T: Serialize + ?Sized>(doc: &DocRef, value: &T, opts: WriteOptions) -> Result<Fingerprint> {
    with_lock(doc, |locked| locked.write(value, opts))
}

/// THE serialized read-modify-write: lock → read (absent ⇒ empty object) → `mutator` →
/// write → unlock. Returns the mutator's result.
///
/// The root is passed to the mutator as found (it is NOT coerced to an object); callers
/// that require an object root check it in the mutator. JSON: the document is always
/// written after a successful mutator, even when unchanged. `Db`: read, mutator and row
/// diff run inside one `BEGIN IMMEDIATE` transaction (serialized with the Python writer
/// too); an unchanged document writes nothing and keeps its revision.
///
/// # Errors
/// [`DocStoreError::Malformed`] when the existing document does not parse (the file is
/// left untouched), [`DocStoreError::Mutator`] when the mutator fails (nothing written),
/// and every error of [`write_value`].
pub fn update<R>(doc: &DocRef, opts: WriteOptions, mutator: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R> {
    with_lock(doc, |locked| locked.update(opts, mutator))
}

/// Runs `f` while holding the document's write lock, for multi-step critical sections
/// (baseline guard + write, per-page manifest merges). The document's format is resolved
/// once per section. The lock is NON-reentrant: `f` must use the [`LockedDoc`] methods,
/// never the free functions that lock (`write_value`, `write`, `update`, `remove`,
/// `quarantine`, `copy_document`, `write_whole_atomic`, `convert_document` onto the same
/// document) — that deadlocks.
pub fn with_lock<R>(doc: &DocRef, f: impl FnOnce(&LockedDoc<'_>) -> R) -> R {
    lock::with_document_lock(doc.stem(), || f(&LockedDoc { doc, format: Cell::new(None) }))
}

/// Removes the document, under the document lock: the stem's file in every format this
/// build knows (and a `.db`'s rollback journal). Absent is not an error.
///
/// # Errors
/// [`DocStoreError::Storage`] when a file exists but cannot be removed (logged).
pub fn remove(doc: &DocRef) -> Result<()> {
    lock::with_document_lock(doc.stem(), || remove_all_formats(doc))
}

/// Preserves a document the caller found MALFORMED under a sidecar name, under the
/// document lock, so a later replacement cannot destroy its only copy. The sidecar is
/// `<file>.<suffix>` (per `naming`) next to the document's file; the original is renamed
/// there, or — with [`QuarantineFallback::CopyIfRenameFails`] — copied when the rename
/// fails. A `.db`'s rollback journal moves along (`<sidecar>-journal`), so the quarantined
/// copy stays recoverable and a new database under the old name cannot replay it. Logged
/// at WARN on success.
///
/// # Errors
/// [`DocStoreError::Quarantine`] when no free name exists (`FirstFree`) or when neither
/// allowed step worked; the document is then untouched and must not be replaced.
pub fn quarantine(doc: &DocRef, suffix: &str, naming: QuarantineNaming, fallback: QuarantineFallback) -> Result<Quarantined> {
    with_lock(doc, |locked| locked.quarantine(suffix, naming, fallback))
}

/// Copies document `from` over document `to` (e.g. a chapter's staging bubbles onto the
/// committed ones). Returns `Ok(false)` and writes nothing when `from` is absent.
///
/// `from` is read unlocked in its own format; `to` is written under its lock in ITS
/// resolved format (existing format, else its new-document format) with `durability`.
/// Only when both sides are JSON are the bytes copied verbatim (after validating that they
/// parse), so the destination is byte-identical; any other combination goes through the
/// document `Value` (a `.db` destination is updated by a row diff; a JSON destination from
/// a `.db` source is written in the default layout).
///
/// # Errors
/// [`DocStoreError::Malformed`] when `from` does not parse (nothing written), and every
/// read error of [`read_value`] and write error of [`write_value`].
pub fn copy_document(from: &DocRef, to: &DocRef, durability: Durability) -> Result<bool> {
    let Some(source_format) = resolve::detect(from)? else { return Ok(false) };
    let Some(loaded) = codec::load_at(from, source_format)? else { return Ok(false) };
    let opts = WriteOptions { durability, ..WriteOptions::default() };
    match loaded {
        codec::Loaded::Json { path, bytes } => {
            let value = codec::parse_value(&path, &bytes)?;
            with_lock(to, |locked| match locked.format()? {
                DocFormat::Json => {
                    let target = to.path_for(DocFormat::Json);
                    codec::write_bytes(&target, &bytes, opts).inspect_err(|err| codec::log_write_failure(&target, err))
                }
                DocFormat::Db => locked.write_value(&value, opts).map(|_| ()),
            })?;
        }
        codec::Loaded::Db { value, .. } => {
            with_lock(to, |locked| locked.write_value(&value, opts))?;
        }
    }
    Ok(true)
}

/// Writes `value` as the whole `format` file of `doc` under the document lock, for callers
/// that materialize a complete document (page-ops phase B): a fresh sibling temp
/// (`.{file}.{pid}.tmp`, recognized by [`is_temp_artifact`]) gets the full content, is
/// reopened and validated Value-equal, fsynced, renamed over the target (retried on
/// Windows sharing violations; a replaced `.db`'s stale `-journal` is removed first) and
/// the directory is fsynced. Missing parent directories are created. The OTHER format's
/// file of the stem is not touched — removing it is the caller's decision.
///
/// JSON is written 2-space pretty without a trailing newline. On wasm32 a JSON write is a
/// plain seam write and `Db` is `Unsupported`.
///
/// # Errors
/// [`DocStoreError::Write`] (`TempWrite` includes a failed validation; `DirSync` means the
/// new file IS in place), [`DocStoreError::Io`], [`DocStoreError::Serialize`],
/// [`DocStoreError::Storage`], [`DocStoreError::Unsupported`]. No temp survives a failure.
pub fn write_whole_atomic(doc: &DocRef, value: &Value, format: DocFormat) -> Result<()> {
    resolve::ensure_supported(doc, format)?;
    lock::with_document_lock(doc.stem(), || write_whole_locked(doc, value, format)).inspect_err(|err| codec::log_write_failure(&doc.path_for(format), err))
}

/// Native body of [`write_whole_atomic`]; caller holds the lock.
#[cfg(not(target_arch = "wasm32"))]
fn write_whole_locked(doc: &DocRef, value: &Value, format: DocFormat) -> Result<()> {
    whole::write_whole(doc, value, format, true)
}

/// wasm32 body of [`write_whole_atomic`]: JSON through the seam (`Db` was rejected).
#[cfg(target_arch = "wasm32")]
fn write_whole_locked(doc: &DocRef, value: &Value, format: DocFormat) -> Result<()> {
    let path = doc.path_for(format);
    let text = serde_json::to_string_pretty(value).map_err(|err| DocStoreError::Serialize { path: path.clone(), cause: err.to_string() })?;
    codec::write_bytes(&path, text.as_bytes(), WriteOptions::default())
}

/// Removes every known format's file of `doc`. Caller holds the document lock.
fn remove_all_formats(doc: &DocRef) -> Result<()> {
    for format in resolve::known_formats() {
        codec::remove_format(doc, *format)?;
    }
    Ok(())
}

/// `<path>.<suffix>` (with `.N` appended for `index > 0`).
fn sidecar_path(path: &Path, suffix: &str, index: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    if index > 0 {
        name.push(format!(".{index}"));
    }
    PathBuf::from(name)
}

/// Body of [`quarantine`] for the file of `doc` in `format`. Caller holds the lock.
fn quarantine_at(doc: &DocRef, format: DocFormat, suffix: &str, naming: QuarantineNaming, fallback: QuarantineFallback) -> Result<Quarantined> {
    let path = doc.path_for(format);
    if !fsio::exists(&path) {
        return Ok(Quarantined::Absent);
    }
    let destination = match naming {
        QuarantineNaming::Replace => sidecar_path(&path, suffix, 0),
        QuarantineNaming::FirstFree { max_candidates } => (0..max_candidates)
            .map(|index| sidecar_path(&path, suffix, index))
            .find(|candidate| !fsio::exists(candidate))
            .ok_or_else(|| DocStoreError::Quarantine {
                path: path.clone(),
                destination: sidecar_path(&path, suffix, 0),
                rename_error: format!("no free destination among {max_candidates} candidates; earlier quarantined copies must be removed first"),
                copy_error: None,
            })?,
    };
    let rename_error = match fsio::rename(&path, &destination) {
        Ok(()) => {
            move_journal_along(format, &path, &destination);
            ms_log::runtime_log::log_warn(format!("docstore: quarantined malformed document {} to {}", path.display(), destination.display()));
            return Ok(Quarantined::Moved(destination));
        }
        Err(err) => err.to_string(),
    };
    match fallback {
        QuarantineFallback::RenameOnly => Err(DocStoreError::Quarantine { path, destination, rename_error, copy_error: None }),
        QuarantineFallback::CopyIfRenameFails => match fsio::copy(&path, &destination) {
            Ok(()) => {
                ms_log::runtime_log::log_warn(format!(
                    "docstore: could not rename the malformed document {} ({rename_error}); copied it to {} instead, so the original may be replaced safely",
                    path.display(),
                    destination.display()
                ));
                Ok(Quarantined::Copied { destination, rename_error })
            }
            Err(copy_error) => Err(DocStoreError::Quarantine { path, destination, rename_error, copy_error: Some(copy_error.to_string()) }),
        },
    }
}


/// After a `.db` was renamed from `path` to `destination`, moves its rollback journal to
/// `<destination>-journal`: the journal belongs to the quarantined copy (needed to recover
/// it) and must not be replayed into a new database created under `path`. When the move
/// fails the journal is deleted instead — a new database must never inherit it — and the
/// failure is logged.
#[cfg(not(target_arch = "wasm32"))]
fn move_journal_along(format: DocFormat, path: &Path, destination: &Path) {
    if format != DocFormat::Db {
        return;
    }
    let journal = whole::journal_path(path);
    if !fsio::exists(&journal) {
        return;
    }
    if let Err(err) = fsio::rename(&journal, &whole::journal_path(destination)) {
        let removed = fsio::remove(&journal);
        ms_log::runtime_log::log_error(format!("docstore: could not move the rollback journal of a quarantined database; it was {}.\nJournal: {}\nError: {err}", if removed.is_ok() { "deleted" } else { "left in place (deletion failed too)" }, journal.display()));
    }
}

/// wasm32: there are no `.db` documents.
#[cfg(target_arch = "wasm32")]
fn move_journal_along(_format: DocFormat, _path: &Path, _destination: &Path) {}
