/*
File: panel/color_presets_store.rs

Purpose:
Persistence of the typing tab's COLOR PRESETS: the 20 cells the color pickers of
the «Текст» tab offer under the palette. The set is TITLE-scoped and lives in
`{title_dir}/color_presets.json` (`ProjectPaths::color_presets_file`), so every
chapter of one manga is typeset from one set of colors.

Main responsibilities:
- decode/encode the document and keep a cell's POSITION stable across junk;
- distinguish `Missing` / `Loaded` / `NewerVersion` / `Invalid` / `Unreadable` so
  neither a corrupt, nor an unread, nor a future-version file is silently replaced
  by the next update;
- quarantine a MALFORMED document to a free `color_presets.json.bad*` name, always
  OFF the GUI thread;
- load in the background and persist every mutation OFF the GUI thread.

Key types:
- `ColorPresetsStore` (the store owned by `TypingTopPanelState`)
- `ColorPresetsDocumentState` (what is known about the document on disk)
- `LoadOutcome` (typed load result)
- `ColorPresetsError` (typed persistence failure)

Notes:
- The stored bytes are PREMULTIPLIED sRGBA — `Color32`'s own representation, which
  is what `ColorPresets::to_stored`/`from_stored` round-trip losslessly. This is
  deliberately NOT the unmultiplied `[u8; 4]` used by `settings.json`
  (`bubble_status.rs`): the two documents are unrelated and the premultiplied form
  is the only one that survives a round trip bit for bit.
- A document whose `version` is HIGHER than this build's is read best-effort (the
  user still sees the colors this build understands) but is NEVER written back.
  This is the project's standing contract for a self-versioned document — see the
  `"PanelLayout"` section of `README_AGENT.md`: a newer section is never
  overwritten, because rewriting it as the current version would silently drop
  every field the newer format added. An OLDER (or absent) version is read
  best-effort and rewritten normally, which is what upgrades the document.
- NOT ONE filesystem operation may happen inside a frame (CLAUDE.md §5). The load,
  the quarantine and the write all run on workers; `save` only records intent and
  `poll` only starts and collects workers.
- EVERY filesystem operation goes through `crate::storage::storage()`, never
  `std::fs`: everything reading the project tree must keep working on the wasm
  virtual store. This is also why `doc_store::write_atomic` — the panel's durable
  (`sync_all` + directory fsync) write recipe — is deliberately NOT used here: it
  is built on `std::fs` and would take this document off the virtual store. The
  price is stated where it is paid, on `save_document`.
- The load/quarantine/atomic-write discipline mirrors
  `char_table/favorites.rs` — the project's one existing contract for a
  per-title user document — and the writer is literally the same type
  (`char_table::SnapshotWriter`). The two documents deliberately share no code
  beyond it, so `favorites.rs` stays the reference implementation; extracting the
  quarantine + atomic-write recipe into one shared owner is a worthwhile
  follow-up, not part of this change. It is NOT mirrored in one respect:
  `favorites.rs` quarantines synchronously from its GUI-thread `toggle`, which
  this store must not do.
*/

use super::char_table::{SnapshotTarget, SnapshotWriter};
use crate::config;
use crate::storage::storage;
use crate::widgets::{ColorPresets, PRESET_COUNT, PresetDefaults};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use ms_thread as thread;

/// Current on-disk schema version of `color_presets.json`.
const COLOR_PRESETS_VERSION: u32 = 1;

/// File name of the title-scoped presets document, shared with
/// `ProjectPaths::color_presets_file`. Not localizable: it is a path that goes to
/// disk.
const COLOR_PRESETS_FILE_NAME: &str = config::COLOR_PRESETS_FILE;

/// What fills a cell the document does not (validly) provide.
///
/// A missing or unusable cell falls back to the built-in typesetting palette
/// rather than to black: twenty black cells are not a usable starting state, and
/// nothing distinguishes them from a set the user deliberately made black.
const CELL_DEFAULTS: PresetDefaults = PresetDefaults::Palette;

/// Typed failure of a presets write. The messages are diagnostic (log/console)
/// text, not UI labels.
#[derive(Debug, thiserror::Error)]
pub(super) enum ColorPresetsError {
    /// The parent directory of the document could not be created.
    #[error("cannot create directory {dir}: {reason}")]
    CreateDir { dir: String, reason: String },
    /// The document could not be serialized to JSON.
    #[error("cannot serialize color presets: {reason}")]
    Serialize { reason: String },
    /// The (temporary) file could not be written.
    #[error("cannot write {path}: {reason}")]
    Write { path: String, reason: String },
    /// The temporary file could not be renamed over the target.
    #[error("cannot replace {path}: {reason}")]
    Rename { path: String, reason: String },
    /// A corrupt document could not be moved aside safely.
    #[error("cannot quarantine {path} as {destination}: {reason}")]
    Quarantine {
        path: String,
        destination: String,
        reason: String,
    },
}

/// Serde mirror of `color_presets.json`. Every field has a serde default so a
/// partial or future-version document still deserializes its known keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ColorPresetsFile {
    /// Schema version; see [`COLOR_PRESETS_VERSION`]. A mismatch is warned about
    /// but the known fields are still parsed best-effort.
    #[serde(default)]
    version: u32,
    /// The preset cells in cell order, each a `[r, g, b, a]` array of PREMULTIPLIED
    /// sRGBA bytes.
    ///
    /// Kept as raw `Value`s rather than `[u8; 4]`s on purpose: one junk element
    /// (a string, a short array, a value out of byte range) must cost that ONE
    /// cell its stored color, not condemn the whole document as corrupt.
    #[serde(default)]
    colors: Vec<Value>,
}

/// Typed result of attempting to load the presets document.
///
/// The five cases must be handled differently: `Missing` is the normal first-run
/// case, `Loaded` carries the parsed set, `NewerVersion` carries a set parsed from
/// a document this build is too old to rewrite, `Invalid` means the file exists and
/// is MALFORMED (only this one may be quarantined), and `Unreadable` means its
/// content is simply unknown — a transient I/O failure over a possibly perfect
/// file. No failing case may be treated as "the user has no presets": a save would
/// overwrite a recoverable file.
#[derive(Debug)]
enum LoadOutcome {
    /// No document exists yet (normal first-run case).
    Missing,
    /// The document parsed successfully. Its version is this build's or older, so
    /// rewriting it is safe (and is what migrates an older document forward).
    Loaded(ColorPresets),
    /// The document declares a version HIGHER than [`COLOR_PRESETS_VERSION`]. The
    /// cells this build understands are parsed so the user still sees them, but
    /// the document must never be written back — see the file header.
    NewerVersion(ColorPresets),
    /// The file exists but its content is not valid JSON for this document.
    Invalid,
    /// The file exists but could not be read at all, so nothing is known about
    /// its content. It must never be quarantined or overwritten.
    Unreadable,
}

/// Decodes one `[r, g, b, a]` element into premultiplied sRGBA bytes.
///
/// Returns `None` for anything that is not an array of exactly four integers in
/// `0..=255`, so the caller can keep that cell's default instead of guessing.
#[must_use]
fn rgba_from_json(value: &Value) -> Option<[u8; 4]> {
    let array = value.as_array()?;
    if array.len() != 4 {
        return None;
    }
    let mut bytes = [0u8; 4];
    for (slot, component) in bytes.iter_mut().zip(array.iter()) {
        *slot = u8::try_from(component.as_u64()?).ok()?;
    }
    Some(bytes)
}

/// Decodes the stored `colors` array into a full preset set.
///
/// A cell's INDEX is its identity, so a junk element keeps its position and falls
/// back to [`CELL_DEFAULTS`] instead of shifting every later cell up. A short
/// array leaves the tail at the defaults and a long one has its extra entries
/// ignored — the stored length is data on disk and must never decide whether the
/// picker works. `source` only labels the warnings.
#[must_use]
fn presets_from_json_array(values: &[Value], source: &str) -> ColorPresets {
    let mut stored = ColorPresets::from_defaults(CELL_DEFAULTS).to_stored();
    for (index, value) in values.iter().enumerate().take(PRESET_COUNT) {
        let Some(slot) = stored.get_mut(index) else {
            // Unreachable: `take(PRESET_COUNT)` bounds the index by the array
            // length. Handled instead of indexed so the decoder cannot panic.
            continue;
        };
        match rgba_from_json(value) {
            Some(bytes) => *slot = bytes,
            None => crate::runtime_log::log_warn(format!(
                "typing: color presets: cell {index} is not an [r,g,b,a] byte array; the default \
                 color is used for it. Source: {source}"
            )),
        }
    }
    if values.len() > PRESET_COUNT {
        crate::runtime_log::log_warn(format!(
            "typing: color presets: {} of {} stored cells ignored ({PRESET_COUNT} cells exist). \
             Source: {source}",
            values.len() - PRESET_COUNT,
            values.len()
        ));
    }
    ColorPresets::from_stored(&stored, CELL_DEFAULTS)
}

/// Encodes preset cells as the JSON array the document stores.
#[must_use]
fn colors_to_json(colors: &[[u8; 4]; PRESET_COUNT]) -> Vec<Value> {
    colors
        .iter()
        .map(|bytes| Value::Array(bytes.iter().copied().map(Value::from).collect()))
        .collect()
}

/// Loads the presets document at `path` into a typed [`LoadOutcome`].
///
/// A missing file is `Missing`; a READ failure is `Unreadable` (the content stays
/// unknown, so the file is never touched again); a PARSE failure is `Invalid`
/// (quarantinable); a version HIGHER than [`COLOR_PRESETS_VERSION`] is
/// `NewerVersion` (parsed best-effort, but write-protected); anything else is
/// `Loaded` — including an older or absent version, which is warned about, parsed
/// best-effort and rewritten by the next save. Never panics.
#[must_use]
fn load_document(path: &Path) -> LoadOutcome {
    let store = storage();
    let path_str = path.to_string_lossy();
    if !store.exists(path_str.as_ref()) {
        return LoadOutcome::Missing;
    }
    let raw = match store.read_to_string(path_str.as_ref()) {
        Ok(raw) => raw,
        Err(err) => {
            // NOT `Invalid`: the document may be perfectly valid and merely
            // unreadable right now, and quarantining it would rename a good file
            // out of the user's way.
            crate::runtime_log::log_warn(format!(
                "typing: cannot read {COLOR_PRESETS_FILE_NAME}; the title's color presets stay \
                 read-only for now and the file is left untouched. Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Unreadable;
        }
    };
    let file: ColorPresetsFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: malformed {COLOR_PRESETS_FILE_NAME}; treating as corrupt (will \
                 quarantine). Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };
    let presets = presets_from_json_array(&file.colors, &path.to_string_lossy());
    if file.version > COLOR_PRESETS_VERSION {
        // A document from a NEWER build. Its extra fields survive only as long as
        // nothing rewrites the file, and this build can only rewrite it as version
        // COLOR_PRESETS_VERSION — which would drop them. So it is shown, never
        // written. Same contract as the `"PanelLayout"` section of `user_config.json`.
        crate::runtime_log::log_warn(format!(
            "typing: {COLOR_PRESETS_FILE_NAME} was written by a newer version of ManhwaStudio \
             (document version {}, this build understands {COLOR_PRESETS_VERSION}); the colors it \
             stores are shown but the file is WRITE-PROTECTED, so nothing this build does not \
             understand is lost. Update ManhwaStudio to edit this title's presets. Path: {}",
            file.version,
            path.display()
        ));
        return LoadOutcome::NewerVersion(presets);
    }
    if file.version != COLOR_PRESETS_VERSION {
        // An OLDER (or absent) version: every field this build knows is present in
        // it by definition, so it is parsed best-effort and the next save migrates
        // the file forward.
        crate::runtime_log::log_warn(format!(
            "typing: {COLOR_PRESETS_FILE_NAME} version {} is older than {COLOR_PRESETS_VERSION}; \
             parsing known fields only, the next change rewrites it as \
             {COLOR_PRESETS_VERSION}. Path: {}",
            file.version,
            path.display()
        ));
    }
    LoadOutcome::Loaded(presets)
}

/// How many `.bad` destinations are probed before quarantine gives up.
///
/// A user who has hit a hundred distinct corruptions of one file has a problem
/// this code cannot fix; refusing is better than looping.
const MAX_QUARANTINE_CANDIDATES: u32 = 100;

/// Picks a quarantine destination that does not exist yet.
///
/// `{file}.bad` first, then `{file}.bad.1`, `{file}.bad.2`, … The rename used
/// underneath REPLACES an existing destination, so reusing one name would destroy
/// the previously quarantined copy — the very content quarantine exists to
/// preserve.
///
/// The probe and the rename are separate operations, so two app instances that
/// quarantine the SAME title's document at the same instant can pick one name
/// twice and the second rename replaces the first copy. Accepted, not overlooked:
/// closing it needs a rename-without-replace primitive that `Storage` does not
/// have, and it costs a copy of an already-corrupt file in a race that requires
/// two instances open on one title at the same millisecond.
///
/// # Errors
/// [`ColorPresetsError::Quarantine`] when every probed name is taken.
fn free_quarantine_path(path: &Path) -> Result<PathBuf, ColorPresetsError> {
    let store = storage();
    let file_name = document_file_name(path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for attempt in 0..MAX_QUARANTINE_CANDIDATES {
        let candidate = if attempt == 0 {
            parent.join(format!("{file_name}.bad"))
        } else {
            parent.join(format!("{file_name}.bad.{attempt}"))
        };
        if !store.exists(candidate.to_string_lossy().as_ref()) {
            return Ok(candidate);
        }
    }
    Err(ColorPresetsError::Quarantine {
        path: path.display().to_string(),
        destination: parent.join(format!("{file_name}.bad")).display().to_string(),
        reason: format!(
            "no free destination among {MAX_QUARANTINE_CANDIDATES} candidates; earlier \
             quarantined copies must be removed first"
        ),
    })
}

/// File name of `path`, falling back to the canonical document name for a path
/// that has none (which no real document path has).
#[must_use]
fn document_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| COLOR_PRESETS_FILE_NAME.to_owned())
}

/// Moves a MALFORMED document aside before replacement is permitted.
///
/// The destination is the first free `{file}.bad`/`{file}.bad.N` name, so an
/// earlier quarantined copy is never overwritten (single-instance; see
/// [`free_quarantine_path`]). Only a document known to be malformed may be passed
/// here — an unread one may be perfectly good.
///
/// Blocking, and therefore FORBIDDEN on the GUI thread: it performs up to
/// [`MAX_QUARANTINE_CANDIDATES`] existence probes plus a rename. The only caller is
/// the quarantine worker started by [`ColorPresetsStore::poll`].
///
/// # Errors
/// [`ColorPresetsError::Quarantine`] when the recoverable original could not be
/// moved; callers must then leave the file alone and refuse to save.
fn quarantine_bad_document(path: &Path) -> Result<(), ColorPresetsError> {
    let bad = free_quarantine_path(path)?;
    storage()
        .rename(
            path.to_string_lossy().as_ref(),
            bad.to_string_lossy().as_ref(),
        )
        .map_err(|err| ColorPresetsError::Quarantine {
            path: path.display().to_string(),
            destination: bad.display().to_string(),
            reason: err.to_string(),
        })?;
    crate::runtime_log::log_warn(format!(
        "typing: quarantined corrupt {COLOR_PRESETS_FILE_NAME} to {}",
        bad.display()
    ));
    Ok(())
}

/// Writes `colors` to the document at `path`, creating the parent directory if
/// needed.
///
/// `colors` holds PREMULTIPLIED sRGBA bytes in cell order.
///
/// # Durability — exactly what temp+rename buys
/// A sibling temp file is written first and then renamed over the target, so a
/// reader never observes a half-written document and a process that dies mid-write
/// leaves the previous set intact. That is protection against a PARTIAL WRITE, not
/// against power loss: neither the temp file's contents nor the containing
/// directory is fsynced, so after a host crash the new name may be missing or its
/// contents may not have reached the disk. Buying that would require the durable
/// recipe of `doc_store::write_atomic`, which is built on `std::fs` and therefore
/// unavailable to a document that must also live on the wasm virtual store
/// (`Storage` exposes no fsync). The cost of a lost write here is one preset edit,
/// which the user can simply redo.
///
/// # Errors
/// [`ColorPresetsError`] on directory creation, serialization, write, or rename
/// failure. Callers persist off the GUI thread.
fn save_document(path: &Path, colors: &[[u8; 4]; PRESET_COUNT]) -> Result<(), ColorPresetsError> {
    let store = storage();
    if let Some(parent) = path.parent() {
        let parent_str = parent.to_string_lossy();
        store
            .create_dir_all(parent_str.as_ref())
            .map_err(|err| ColorPresetsError::CreateDir {
                dir: parent.display().to_string(),
                reason: err.to_string(),
            })?;
    }
    let file = ColorPresetsFile {
        version: COLOR_PRESETS_VERSION,
        colors: colors_to_json(colors),
    };
    let mut text =
        serde_json::to_string_pretty(&file).map_err(|err| ColorPresetsError::Serialize {
            reason: err.to_string(),
        })?;
    text.push('\n');

    // Temp sibling + rename: the target is replaced in one step, so a process that
    // dies between the two leaves the previous set intact. No fsync is involved —
    // see the "Durability" section above for what that does and does not cover. The
    // temp name is per-process so two processes cannot collide on it.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        document_file_name(path),
        std::process::id()
    ));
    let temp_str = temp.to_string_lossy().into_owned();
    store
        .write(temp_str.as_str(), text.as_bytes())
        .map_err(|err| ColorPresetsError::Write {
            path: temp.display().to_string(),
            reason: err.to_string(),
        })?;
    store
        .rename(temp_str.as_str(), path.to_string_lossy().as_ref())
        .map_err(|err| {
            // Best-effort cleanup of the orphaned temp file; the rename failure is
            // the error we report (a failed cleanup must not mask it).
            if let Err(cleanup_err) = store.remove_file(temp_str.as_str()) {
                crate::runtime_log::log_warn(format!(
                    "typing: could not remove orphaned temp file {temp_str}: {cleanup_err}"
                ));
            }
            ColorPresetsError::Rename {
                path: path.display().to_string(),
                reason: err.to_string(),
            }
        })
}

/// State of the presets document as last observed on disk.
///
/// Exactly one variant — `Ready` — permits a write. Every other variant means the
/// bytes on disk are unknown, protected, or not this build's to rewrite, and the
/// file is left byte-for-byte alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ColorPresetsDocumentState {
    /// No path bound yet (no title open) — nothing can be read or written.
    #[default]
    Unbound,
    /// The bound document is being read by a worker.
    Loading,
    /// The document is absent or was read successfully; saving is allowed.
    Ready,
    /// The document exists but is MALFORMED. The in-memory set is the default
    /// palette and saving is REFUSED until the user's next explicit update, which
    /// requests quarantine (see [`Self::Quarantining`]) instead of writing.
    Invalid,
    /// A quarantine of the malformed document has been requested or is running.
    /// Saving stays REFUSED until the worker CONFIRMS the rename: acting on an
    /// unconfirmed quarantine would let the write land on a file that is still the
    /// user's only copy of its content.
    Quarantining,
    /// The document declares a version HIGHER than [`COLOR_PRESETS_VERSION`]. The
    /// cells this build understands are loaded and shown, but saving is REFUSED
    /// FOREVER for this document: this build can only write version
    /// [`COLOR_PRESETS_VERSION`], which would silently drop every field the newer
    /// format added. Recovery is to run a build that understands the document, not
    /// to overwrite it. Same contract as the `"PanelLayout"` section of
    /// `user_config.json` (`README_AGENT.md`).
    NewerVersion,
    /// The document exists but could not be read or its load result never arrived,
    /// so its content is unknown. Saving is refused and the file is left exactly as
    /// it is — it is NOT quarantined, and the in-memory palette does not mean the
    /// user has no presets.
    Unreadable,
    /// Quarantine failed; saving remains blocked to protect the original file.
    QuarantineFailed,
}

/// Complete save request with its target captured at mutation time.
#[derive(Debug)]
struct ColorPresetsSnapshot {
    path: PathBuf,
    colors: [[u8; 4]; PRESET_COUNT],
}

impl SnapshotTarget for ColorPresetsSnapshot {
    fn target(&self) -> &Path {
        &self.path
    }
}

/// Writes one captured snapshot.
fn save_snapshot(snapshot: ColorPresetsSnapshot) -> Result<(), String> {
    save_document(&snapshot.path, &snapshot.colors).map_err(|err| err.to_string())
}

/// Title-scoped color presets backed by `{title_dir}/color_presets.json`.
///
/// Owned by `TypingTopPanelState` — ONE set above both the create and the edit
/// panel, so the two never drift apart. The set is always usable: before a title is
/// bound (and while a corrupt or unreadable document blocks writes) it holds the
/// built-in palette, and only SAVING is refused.
#[derive(Debug)]
pub(super) struct ColorPresetsStore {
    path: Option<PathBuf>,
    presets: ColorPresets,
    state: ColorPresetsDocumentState,
    load_rx: Option<Receiver<(PathBuf, LoadOutcome)>>,
    /// Quarantine target recorded by [`Self::save`] and picked up by the NEXT
    /// [`Self::poll`], which is what keeps every filesystem operation out of the
    /// frame that requested it. The path is captured here rather than re-read from
    /// `self.path` for the same reason `ColorPresetsSnapshot` captures it: the title
    /// may change before the request is served.
    quarantine_request: Option<PathBuf>,
    /// Target and result channel of the running quarantine worker, if any. The
    /// target is kept next to the receiver so a verdict — including a worker that
    /// died without sending one — can be matched against the currently bound
    /// document before it is allowed to change any state.
    quarantine_rx: Option<(PathBuf, Receiver<Result<(), String>>)>,
    writer: SnapshotWriter<ColorPresetsSnapshot>,
}

impl Default for ColorPresetsStore {
    fn default() -> Self {
        Self {
            path: None,
            presets: ColorPresets::from_defaults(CELL_DEFAULTS),
            state: ColorPresetsDocumentState::Unbound,
            load_rx: None,
            quarantine_request: None,
            quarantine_rx: None,
            writer: SnapshotWriter::new("typing-save-color-presets", save_snapshot),
        }
    }
}

impl ColorPresetsStore {
    /// Binds the store to `path` (`ProjectPaths::color_presets_file`) and reloads.
    ///
    /// Passing the SAME path again is a no-op, so a per-frame setter call from the
    /// UI does not re-read the file every frame. Passing `None` unbinds and resets
    /// the set to the built-in palette (no title open).
    pub(super) fn set_path(&mut self, path: Option<PathBuf>) {
        if self.path == path {
            return;
        }
        self.path = path;
        self.start_reload();
    }

    /// The preset set to hand to the color pickers.
    ///
    /// The only accessor the UI needs: the set is always usable, and everything the
    /// store knows about the document on disk is acted on by [`Self::save`] itself
    /// (no caller has to branch on it). Tests read the private fields directly.
    pub(super) fn presets_mut(&mut self) -> &mut ColorPresets {
        &mut self.presets
    }

    /// Starts a document read without blocking the GUI thread.
    ///
    /// Tests execute the transition inline so fixtures stay deterministic;
    /// production delivers through [`ColorPresetsStore::poll`].
    fn start_reload(&mut self) {
        // A quarantine that was requested for the PREVIOUS target is dropped rather
        // than carried over: it only ever existed to make room for a write this
        // store is no longer going to perform, and renaming a file of a title the
        // user has left would move it out of their way for nothing. A quarantine
        // already IN FLIGHT is not cancellable; `apply_quarantine` discards its
        // result instead (same stale-target guard as `apply_load`).
        if let Some(stale) = self.quarantine_request.take() {
            crate::runtime_log::log_warn(format!(
                "typing: the color-presets target changed before the corrupt \
                 {COLOR_PRESETS_FILE_NAME} was moved aside; it is left untouched and the pending \
                 change is dropped. Path: {}",
                stale.display()
            ));
        }
        let Some(path) = self.path.clone() else {
            self.presets = ColorPresets::from_defaults(CELL_DEFAULTS);
            self.state = ColorPresetsDocumentState::Unbound;
            self.load_rx = None;
            return;
        };
        self.presets = ColorPresets::from_defaults(CELL_DEFAULTS);
        self.state = ColorPresetsDocumentState::Loading;
        if cfg!(test) {
            let outcome = load_document(&path);
            self.apply_load(path, outcome);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-load-color-presets".to_string())
            .spawn(move || {
                let outcome = load_document(&worker_path);
                if tx.send((worker_path, outcome)).is_err() {
                    crate::runtime_log::log_warn(
                        "typing: color presets load result was superseded",
                    );
                }
            });
        match spawn_result {
            Ok(_handle) => self.load_rx = Some(rx),
            Err(err) => {
                // The document was never looked at, so its content is unknown:
                // `Unreadable`, never `Invalid` — a spawn failure must not put a
                // healthy file on the quarantine path.
                self.load_rx = None;
                self.state = ColorPresetsDocumentState::Unreadable;
                crate::runtime_log::log_error(format!(
                    "typing: failed to spawn color-presets loader; the title's presets stay \
                     read-only. Path: {} Error: {err}",
                    path.display()
                ));
            }
        }
    }

    /// Applies a worker result only when its captured target remains bound.
    fn apply_load(&mut self, path: PathBuf, outcome: LoadOutcome) {
        if self.path.as_ref() != Some(&path) {
            return;
        }
        match outcome {
            LoadOutcome::Missing => {
                // A first-run title has no document, and none is written until the
                // user actually changes a cell: the built-in palette is a default,
                // not a user decision worth persisting.
                self.presets = ColorPresets::from_defaults(CELL_DEFAULTS);
                self.state = ColorPresetsDocumentState::Ready;
            }
            LoadOutcome::Loaded(presets) => {
                self.presets = presets;
                self.state = ColorPresetsDocumentState::Ready;
            }
            LoadOutcome::NewerVersion(presets) => {
                // Shown, never written: the set the user sees is the part of a newer
                // document this build understands.
                self.presets = presets;
                self.state = ColorPresetsDocumentState::NewerVersion;
            }
            LoadOutcome::Invalid => {
                self.presets = ColorPresets::from_defaults(CELL_DEFAULTS);
                self.state = ColorPresetsDocumentState::Invalid;
            }
            LoadOutcome::Unreadable => {
                self.presets = ColorPresets::from_defaults(CELL_DEFAULTS);
                self.state = ColorPresetsDocumentState::Unreadable;
            }
        }
    }

    /// Drives every background step of the store without blocking: it collects a
    /// finished load, starts a quarantine [`Self::save`] asked for, and collects a
    /// finished quarantine. Cheap no-op when nothing is outstanding.
    ///
    /// This is the ONLY place the store may start or finish disk work, and it must
    /// be called once per frame (`facade::set_color_presets_path` does). Starting a
    /// worker is not itself I/O, so the frame stays free of filesystem calls.
    ///
    /// Deliberately reports nothing: the only consumer of the set is a popup the
    /// user opens by hand, long after a title change settles, so no caller has to
    /// react to the arrival and no repaint has to be requested for it.
    pub(super) fn poll(&mut self) {
        self.poll_load();
        // Start before collect, so a quarantine that completes synchronously (the
        // `cfg!(test)` path) is already accounted for when this call returns.
        self.start_pending_quarantine();
        self.poll_quarantine();
    }

    /// Collects a finished background load, if one is outstanding.
    fn poll_load(&mut self) {
        let Some(rx) = self.load_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((path, outcome)) => {
                self.load_rx = None;
                self.apply_load(path, outcome);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker died without sending: the document was never
                // classified, so it is unknown, not corrupt.
                self.load_rx = None;
                self.state = ColorPresetsDocumentState::Unreadable;
                crate::runtime_log::log_error(
                    "typing: the color-presets load result never arrived; the title's presets \
                     stay read-only",
                );
            }
        }
    }

    /// Starts the quarantine [`Self::save`] asked for, if any.
    ///
    /// Called from [`Self::poll`] and NEVER from `save`: the request deliberately
    /// crosses a frame boundary so that the frame in which the user edited a cell
    /// performs no filesystem work at all (CLAUDE.md §5). Only one quarantine can be
    /// outstanding — `save` refuses while the store is `Quarantining`.
    fn start_pending_quarantine(&mut self) {
        let Some(path) = self.quarantine_request.take() else {
            return;
        };
        if cfg!(test) {
            // Tests execute the transition inline so fixtures stay deterministic;
            // production delivers through the worker below. Mirrors `start_reload`.
            let result = quarantine_bad_document(&path).map_err(|err| err.to_string());
            self.apply_quarantine(path, result);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-quarantine-color-presets".to_string())
            .spawn(move || {
                let result = quarantine_bad_document(&worker_path).map_err(|err| err.to_string());
                if tx.send(result).is_err() {
                    crate::runtime_log::log_warn(
                        "typing: the color-presets quarantine result was superseded",
                    );
                }
            });
        match spawn_result {
            Ok(_handle) => self.quarantine_rx = Some((path, rx)),
            Err(err) => {
                // Nothing was renamed, so the corrupt file is still the user's only
                // copy of its content: stay write-protected rather than fall back to
                // overwriting it.
                self.quarantine_rx = None;
                self.state = ColorPresetsDocumentState::QuarantineFailed;
                crate::runtime_log::log_error(format!(
                    "typing: failed to spawn the color-presets quarantine worker; the corrupt \
                     {COLOR_PRESETS_FILE_NAME} is left untouched and the title's presets stay \
                     read-only. Path: {} Error: {err}",
                    path.display()
                ));
            }
        }
    }

    /// Collects the quarantine worker's verdict, if one is outstanding.
    fn poll_quarantine(&mut self) {
        let Some((path, rx)) = self.quarantine_rx.as_ref() else {
            return;
        };
        let verdict = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            // The worker died without sending: nothing proves the rename happened,
            // so it is treated exactly like a failure.
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the quarantine worker ended without a result".to_owned())
            }
        };
        let path = path.clone();
        self.quarantine_rx = None;
        self.apply_quarantine(path, verdict);
    }

    /// Applies a quarantine verdict captured for `path`.
    ///
    /// A verdict for a target that is no longer bound is DISCARDED: the state now
    /// describes another title's document. On success the store becomes `Ready` and
    /// the current set is handed to the writer immediately, so the cell edit that
    /// asked for the quarantine survives the frame it had to wait. On failure the
    /// store stays write-protected and the corrupt file is left untouched.
    fn apply_quarantine(&mut self, path: PathBuf, result: Result<(), String>) {
        if self.path.as_ref() != Some(&path) {
            crate::runtime_log::log_warn(format!(
                "typing: the color-presets quarantine of {} finished after the bound title \
                 changed; its result is ignored",
                path.display()
            ));
            return;
        }
        match result {
            Ok(()) => {
                self.state = ColorPresetsDocumentState::Ready;
                // The corrupt content is preserved under its `.bad` name and the
                // target is free, so the edit that triggered the quarantine is
                // persisted now — it was refused, not forgotten.
                self.writer.enqueue(ColorPresetsSnapshot {
                    path,
                    colors: self.presets.to_stored(),
                });
            }
            Err(reason) => {
                self.state = ColorPresetsDocumentState::QuarantineFailed;
                crate::runtime_log::log_error(format!(
                    "typing: could not quarantine corrupt {COLOR_PRESETS_FILE_NAME}; the title's \
                     color presets remain read-only and the file is left untouched. Path: {} \
                     Error: {reason}",
                    path.display()
                ));
            }
        }
    }

    /// Hands the current set to the background writer after the user overwrote a
    /// cell. Performs NO filesystem operation itself — it is called from a frame.
    ///
    /// Returns whether the change was handed to the writer. `false` means the
    /// document may not be written right now: no title bound, still `Loading` (the
    /// stored cells this process has not read yet must not be replaced by in-memory
    /// defaults), written by a NEWER build (never overwritten, see
    /// [`ColorPresetsDocumentState::NewerVersion`]), or a previous failure left the
    /// file protected. On `Invalid` — and ONLY there, where the file is known to be
    /// malformed — this explicit user action REQUESTS a quarantine; the change is
    /// then persisted by [`Self::poll`] once the rename is confirmed, so `false`
    /// there means "not yet", not "dropped".
    pub(super) fn save(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            crate::runtime_log::log_warn(
                "typing: a color preset was updated with no title bound; it is not persisted",
            );
            return false;
        };
        match self.state {
            ColorPresetsDocumentState::Ready => {}
            ColorPresetsDocumentState::Invalid => {
                // The user explicitly asked to change the set, so the MALFORMED file
                // must be moved aside instead of overwritten in place — but NOT from
                // here: the free-name probe and the rename are blocking filesystem
                // calls and this runs inside a frame (CLAUDE.md §5). Record the
                // request, stay write-protected, and let `poll` do the disk work.
                self.state = ColorPresetsDocumentState::Quarantining;
                self.quarantine_request = Some(path.clone());
                crate::runtime_log::log_warn(format!(
                    "typing: {COLOR_PRESETS_FILE_NAME} is corrupt; it is being moved aside in the \
                     background and the change is persisted as soon as that is confirmed. Path: {}",
                    path.display()
                ));
                return false;
            }
            ColorPresetsDocumentState::NewerVersion => {
                // Not a failure and not transient: this build cannot express the
                // document, so it never writes it. The in-memory edit stays usable
                // for the session and is simply not stored.
                crate::runtime_log::log_warn(format!(
                    "typing: a color preset was updated, but {COLOR_PRESETS_FILE_NAME} was written \
                     by a newer version of ManhwaStudio; the file is left untouched so nothing \
                     this build does not understand is lost, and the change applies to this \
                     session only. Path: {}",
                    path.display()
                ));
                return false;
            }
            // `Unbound` cannot reach here (the path check above returned). The rest
            // all mean "the stored set is unknown or protected": writing would
            // destroy content this process never saw, or content whose quarantine is
            // not confirmed yet.
            ColorPresetsDocumentState::Unbound
            | ColorPresetsDocumentState::Loading
            | ColorPresetsDocumentState::Quarantining
            | ColorPresetsDocumentState::Unreadable
            | ColorPresetsDocumentState::QuarantineFailed => {
                crate::runtime_log::log_warn(format!(
                    "typing: a color preset was updated while {COLOR_PRESETS_FILE_NAME} is \
                     {:?}; the change is not persisted and the file is left untouched. Path: {}",
                    self.state,
                    path.display()
                ));
                return false;
            }
        }
        self.writer.enqueue(ColorPresetsSnapshot {
            path,
            colors: self.presets.to_stored(),
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;
    use std::time::{SystemTime, UNIX_EPOCH};

    const RED: Color32 = Color32::from_rgb(255, 0, 0);

    /// Unique temp path so parallel tests never share a file.
    fn unique_temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_color_presets_{tag}_{nanos}.json"))
    }

    /// Removes a temp file after a test.
    ///
    /// The result is deliberately discarded: cleanup runs after every assertion has
    /// already been made, and a missing file is the normal case for the tests that
    /// never create one.
    fn cleanup(path: &Path) {
        let _ = storage().remove_file(path.to_string_lossy().as_ref());
    }

    /// Writes `contents` verbatim, so a test can plant a malformed document.
    fn write_raw(path: &Path, contents: &str) {
        storage()
            .write(path.to_string_lossy().as_ref(), contents.as_bytes())
            .expect("test fixture must be writable");
    }

    /// Unwraps a `Loaded` outcome or panics naming the actual variant.
    fn expect_loaded(outcome: LoadOutcome) -> ColorPresets {
        match outcome {
            LoadOutcome::Loaded(presets) => presets,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::NewerVersion(_) => panic!("expected Loaded, got NewerVersion"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
            LoadOutcome::Unreadable => panic!("expected Loaded, got Unreadable"),
        }
    }

    /// Unwraps a `NewerVersion` outcome or panics naming the actual variant.
    fn expect_newer_version(outcome: LoadOutcome) -> ColorPresets {
        match outcome {
            LoadOutcome::NewerVersion(presets) => presets,
            LoadOutcome::Loaded(_) => panic!("expected NewerVersion, got Loaded"),
            LoadOutcome::Missing => panic!("expected NewerVersion, got Missing"),
            LoadOutcome::Invalid => panic!("expected NewerVersion, got Invalid"),
            LoadOutcome::Unreadable => panic!("expected NewerVersion, got Unreadable"),
        }
    }

    #[test]
    fn document_round_trip_preserves_every_cell() {
        let path = unique_temp_path("roundtrip");
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert!(presets.set(0, RED));
        assert!(presets.set(PRESET_COUNT - 1, Color32::from_rgba_premultiplied(1, 2, 3, 4)));

        save_document(&path, &presets.to_stored()).expect("save must succeed");
        let loaded = expect_loaded(load_document(&path));
        assert_eq!(loaded, presets);
        cleanup(&path);
    }

    #[test]
    fn missing_document_is_missing_not_invalid() {
        let path = unique_temp_path("missing");
        assert!(matches!(load_document(&path), LoadOutcome::Missing));
    }

    #[test]
    fn short_and_long_arrays_are_padded_and_truncated() {
        let path = unique_temp_path("length");
        // Two stored cells: the rest of the set must come from the palette default.
        write_raw(&path, r#"{"version":1,"colors":[[10,20,30,40],[50,60,70,80]]}"#);
        let loaded = expect_loaded(load_document(&path));
        let defaults = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert_eq!(loaded.get(0), Some(Color32::from_rgba_premultiplied(10, 20, 30, 40)));
        assert_eq!(loaded.get(1), Some(Color32::from_rgba_premultiplied(50, 60, 70, 80)));
        assert_eq!(loaded.get(2), defaults.get(2));
        assert_eq!(loaded.get(PRESET_COUNT - 1), defaults.get(PRESET_COUNT - 1));

        // More cells than exist: the extra ones are ignored, not an error.
        let mut colors: Vec<String> = (0..PRESET_COUNT + 3)
            .map(|_| "[1,2,3,4]".to_owned())
            .collect();
        colors.push("[9,9,9,9]".to_owned());
        write_raw(
            &path,
            &format!(r#"{{"version":1,"colors":[{}]}}"#, colors.join(",")),
        );
        let loaded = expect_loaded(load_document(&path));
        assert!(
            loaded
                .colors()
                .iter()
                .all(|color| *color == Color32::from_rgba_premultiplied(1, 2, 3, 4))
        );
        cleanup(&path);
    }

    #[test]
    fn one_junk_cell_keeps_its_position_and_its_default() {
        let path = unique_temp_path("junk_cell");
        write_raw(
            &path,
            r#"{"version":1,"colors":["nope",[50,60,70,80],[1,2,3],[300,0,0,0]]}"#,
        );
        let loaded = expect_loaded(load_document(&path));
        let defaults = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert_eq!(loaded.get(0), defaults.get(0), "junk must not shift cells");
        assert_eq!(loaded.get(1), Some(Color32::from_rgba_premultiplied(50, 60, 70, 80)));
        assert_eq!(loaded.get(2), defaults.get(2), "a 3-component cell is junk");
        assert_eq!(loaded.get(3), defaults.get(3), "an out-of-byte-range cell is junk");
        cleanup(&path);
    }

    #[test]
    fn older_version_is_parsed_best_effort_and_stays_writable() {
        let path = unique_temp_path("older_version");
        // Version 0 is what a document written before the field existed decodes to;
        // it is an OLD version, so it is read AND rewritten normally.
        write_raw(&path, r#"{"version":0,"colors":[[10,20,30,40]]}"#);
        let loaded = expect_loaded(load_document(&path));
        assert_eq!(loaded.get(0), Some(Color32::from_rgba_premultiplied(10, 20, 30, 40)));

        let mut store = ColorPresetsStore::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        assert!(store.presets.set(3, RED));
        assert!(store.save(), "an older document must be rewritten normally");
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        cleanup(&path);
    }

    #[test]
    fn newer_version_is_shown_but_never_overwritten() {
        let path = unique_temp_path("newer_version");
        let raw = format!(
            r#"{{"version":{},"colors":[[10,20,30,40]],"unknown_future_field":true}}"#,
            COLOR_PRESETS_VERSION + 1
        );
        write_raw(&path, &raw);
        // The cells this build understands are still decoded: the user must see the
        // colors, only the WRITE is forbidden.
        let loaded = expect_newer_version(load_document(&path));
        assert_eq!(loaded.get(0), Some(Color32::from_rgba_premultiplied(10, 20, 30, 40)));

        let mut store = ColorPresetsStore::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::NewerVersion);
        assert_eq!(store.presets.get(0), Some(Color32::from_rgba_premultiplied(10, 20, 30, 40)));

        assert!(store.presets.set(1, RED));
        assert!(!store.save(), "a newer document must never be written");
        assert_eq!(
            store.state,
            ColorPresetsDocumentState::NewerVersion,
            "the refusal is permanent for this document, not a transient failure"
        );
        assert!(store.quarantine_request.is_none(), "a newer file is not corrupt");
        store.poll();
        assert_eq!(store.state, ColorPresetsDocumentState::NewerVersion);
        assert_eq!(
            storage()
                .read_to_string(path.to_string_lossy().as_ref())
                .expect("the newer document must still be readable"),
            raw,
            "the bytes of a newer document must be untouched"
        );
        cleanup(&path);
    }

    #[test]
    fn malformed_document_is_quarantined_off_the_frame_and_then_saved() {
        let path = unique_temp_path("malformed");
        write_raw(&path, "{not json");
        assert!(matches!(load_document(&path), LoadOutcome::Invalid));

        let mut store = ColorPresetsStore::default();
        // `cfg!(test)` makes the reload inline, so the state is settled here.
        store.set_path(Some(path.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::Invalid);
        assert!(store.presets.set(2, RED));

        // The frame-side call only RECORDS the quarantine: no rename, no existence
        // probe, and the store stays write-protected (CLAUDE.md §5).
        assert!(!store.save(), "the change cannot be persisted before the quarantine");
        assert_eq!(store.state, ColorPresetsDocumentState::Quarantining);
        assert_eq!(store.quarantine_request.as_deref(), Some(path.as_path()));
        assert!(
            storage().exists(path.to_string_lossy().as_ref()),
            "save() must not have touched the file"
        );
        let quarantined = path.with_file_name(format!(
            "{}.bad",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        assert!(
            !storage().exists(quarantined.to_string_lossy().as_ref()),
            "save() must not have renamed anything"
        );

        // A further edit while the quarantine is unconfirmed is refused too.
        assert!(store.presets.set(4, RED));
        assert!(!store.save());
        assert_eq!(store.state, ColorPresetsDocumentState::Quarantining);

        // `poll` is where the disk work happens (inline under `cfg!(test)`).
        store.poll();
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        assert!(store.quarantine_request.is_none());
        assert!(
            storage().exists(quarantined.to_string_lossy().as_ref()),
            "the corrupt document must be preserved at {}",
            quarantined.display()
        );
        assert!(!storage().exists(path.to_string_lossy().as_ref()));
        cleanup(&quarantined);
        cleanup(&path);
    }

    #[test]
    fn a_pending_quarantine_is_dropped_when_the_title_changes() {
        let path = unique_temp_path("quarantine_rebind");
        write_raw(&path, "{not json");
        let mut store = ColorPresetsStore::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::Invalid);
        assert!(store.presets.set(2, RED));
        assert!(!store.save());
        assert_eq!(store.state, ColorPresetsDocumentState::Quarantining);

        // Binding another title abandons the request: the corrupt file belongs to a
        // title the store no longer edits, so it must not be renamed for nothing.
        let other = unique_temp_path("quarantine_rebind_other");
        store.set_path(Some(other.clone()));
        assert!(store.quarantine_request.is_none());
        store.poll();
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        assert!(
            storage().exists(path.to_string_lossy().as_ref()),
            "the abandoned title's file must be left exactly as it was"
        );
        cleanup(&path);
        cleanup(&other);
    }

    #[test]
    fn rebinding_the_same_path_does_not_reload() {
        let path = unique_temp_path("rebind");
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert!(presets.set(5, RED));
        save_document(&path, &presets.to_stored()).expect("save must succeed");

        let mut store = ColorPresetsStore::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        assert_eq!(store.presets, presets);

        // A local edit that was not saved survives a repeated bind to the same path:
        // the no-op is what keeps the per-frame setter from re-reading the file.
        assert!(store.presets.set(6, RED));
        let edited = store.presets.clone();
        store.set_path(Some(path.clone()));
        assert_eq!(store.presets, edited, "the same path must not reload");

        // A different path DOES reload.
        let other = unique_temp_path("rebind_other");
        store.set_path(Some(other.clone()));
        assert_eq!(store.state, ColorPresetsDocumentState::Ready);
        assert_eq!(
            store.presets,
            ColorPresets::from_defaults(PresetDefaults::Palette)
        );
        cleanup(&path);
        cleanup(&other);
    }

    #[test]
    fn an_unbound_store_refuses_to_save() {
        let mut store = ColorPresetsStore::default();
        assert_eq!(store.state, ColorPresetsDocumentState::Unbound);
        assert_eq!(store.path, None);
        assert!(store.presets.set(0, RED));
        assert!(!store.save());
    }

    #[test]
    fn unbinding_restores_the_default_palette() {
        let path = unique_temp_path("unbind");
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert!(presets.set(1, RED));
        save_document(&path, &presets.to_stored()).expect("save must succeed");

        let mut store = ColorPresetsStore::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.presets, presets);
        store.set_path(None);
        assert_eq!(store.state, ColorPresetsDocumentState::Unbound);
        assert_eq!(
            store.presets,
            ColorPresets::from_defaults(PresetDefaults::Palette)
        );
        cleanup(&path);
    }
}
