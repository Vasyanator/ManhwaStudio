/*
File: panel/char_table/favorites.rs

Purpose:
The two favorite-character stores of the character table and their persistence.
They are deliberately asymmetric because their homes already differ:

- GLOBAL  -> `user_config.json`, key `TextTab.char_table_global_favorites`
             (an array of single-character strings). Read through
             `crate::config::JsonConfig`, written through
             `crate::config::update_user_config_file`.
- PROJECT -> `{title_dir}/char_favorites.json`, a versioned document
             `{ "version": 1, "characters": ["★", "→"] }`. TITLE-scoped on
             purpose: every chapter of one manga shares one list.

Main responsibilities:
- decode/encode both documents and normalize a character list (duplicates
  collapse, user insertion order preserved);
- distinguish `Missing` / `Loaded` / `Invalid` / `Unreadable` for the project
  document so neither a corrupt nor an unread file is replaced by the next toggle;
- quarantine a MALFORMED project document to a free `char_favorites.json.bad*`
  name;
- persist every mutation OFF the GUI thread.

Key types:
- `GlobalFavorites` / `ProjectFavorites` (the two stores)
- `LoadOutcome` (Missing / Loaded / Invalid / Unreadable load result)
- `FavoritesError` (typed persistence failure)

Notes:
EVERY filesystem operation on the PROJECT document goes through
`crate::storage::storage()`, never `std::fs`: `src/project.rs` and everything
below it must keep working on the wasm virtual store. The GLOBAL store touches
no filesystem directly at all — `crate::config` owns that file, including its
process-wide write lock.
*/

use crate::config;
use crate::storage::storage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use ms_thread as thread;

/// Current on-disk schema version of `char_favorites.json`.
pub(super) const CHAR_FAVORITES_VERSION: u32 = 1;

/// File name of the title-scoped project favorites document, shared with
/// `ProjectPaths::char_favorites_file`. Not localizable: it is a path that goes
/// to disk.
pub(super) const CHAR_FAVORITES_FILE_NAME: &str = config::CHAR_FAVORITES_FILE;

/// `TextTab` key holding the global favorites array in `user_config.json`.
pub(super) const TEXT_TAB_GLOBAL_FAVORITES_KEY: &str = "char_table_global_favorites";

/// Typed failure of a favorites write. The messages are diagnostic (log/console)
/// text, not UI labels: the window localizes its own user-facing wording.
#[derive(Debug, thiserror::Error)]
pub(super) enum FavoritesError {
    /// The parent directory of the project document could not be created.
    #[error("cannot create directory {dir}: {reason}")]
    CreateDir { dir: String, reason: String },
    /// The document could not be serialized to JSON.
    #[error("cannot serialize character favorites: {reason}")]
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

/// Serde mirror of `char_favorites.json`. Every field has a serde default so a
/// partial or future-version document still deserializes its known keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CharFavoritesFile {
    /// Schema version; see [`CHAR_FAVORITES_VERSION`]. A mismatch is warned
    /// about but the known fields are still parsed best-effort.
    #[serde(default)]
    version: u32,
    /// The favorite characters as single-character strings, in user order.
    ///
    /// Kept as raw `Value`s rather than `String`s on purpose: a single junk
    /// element (a number, a multi-character string) must be SKIPPED by
    /// [`chars_from_json_array`], not condemn the whole document as corrupt and
    /// send it to quarantine.
    #[serde(default)]
    characters: Vec<Value>,
}

/// Typed result of attempting to load the project favorites document.
///
/// The four cases must be handled differently: `Missing` is the normal first-run
/// case, `Loaded` carries the parsed list, `Invalid` means the file exists and is
/// MALFORMED (only this one may be quarantined), and `Unreadable` means its
/// content is simply unknown — a transient I/O failure over a possibly perfect
/// file. Neither failing case may be treated as "the user has no favorites": the
/// next toggle would overwrite a recoverable file.
#[derive(Debug)]
pub(super) enum LoadOutcome {
    /// No document exists yet (normal first-run case).
    Missing,
    /// The document parsed successfully (best-effort for an unknown version).
    Loaded(Vec<char>),
    /// The file exists but its content is not valid JSON for this document.
    Invalid,
    /// The file exists but could not be read at all, so nothing is known about
    /// its content. It must never be quarantined or overwritten.
    Unreadable,
}

/// Collapses duplicates while preserving the user's insertion order.
///
/// The FIRST occurrence of a character wins, so re-adding a favorite never
/// reorders the list.
#[must_use]
fn normalize(chars: impl IntoIterator<Item = char>) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for ch in chars {
        if !out.contains(&ch) {
            out.push(ch);
        }
    }
    out
}

/// Decodes an array of JSON strings into characters.
///
/// Elements that are not strings, are empty, or hold more than one character
/// are skipped with a warning: a favorite is exactly one character (a
/// character+font pair is deliberately NOT a favorite — see the module README).
#[must_use]
fn chars_from_json_array(values: &[Value], source: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for value in values {
        let Some(text) = value.as_str() else {
            crate::runtime_log::log_warn(format!(
                "typing: char table favorites: non-string entry ignored. Source: {source}"
            ));
            continue;
        };
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(ch), None) => out.push(ch),
            _ => crate::runtime_log::log_warn(format!(
                "typing: char table favorites: entry {text:?} is not a single character; ignored. \
                 Source: {source}"
            )),
        }
    }
    normalize(out)
}

/// Encodes characters as the JSON array of single-character strings both stores use.
#[must_use]
pub(super) fn global_favorites_json(chars: &[char]) -> Vec<Value> {
    chars
        .iter()
        .map(|ch| Value::String(ch.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Project store (`{title_dir}/char_favorites.json`, via `storage()`)
// ---------------------------------------------------------------------------

/// Loads the project favorites document at `path` into a typed [`LoadOutcome`].
///
/// A missing file is `Missing`; a READ failure is `Unreadable` (the content stays
/// unknown, so the file is never touched again); a PARSE failure is `Invalid`
/// (quarantinable); otherwise `Loaded` (an unexpected version is warned about and
/// still parsed best-effort). Never panics.
#[must_use]
pub(super) fn load_project_document(path: &Path) -> LoadOutcome {
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
                "typing: cannot read {CHAR_FAVORITES_FILE_NAME}; the title's favorites stay \
                 read-only for now and the file is left untouched. Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Unreadable;
        }
    };
    let file: CharFavoritesFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: malformed {CHAR_FAVORITES_FILE_NAME}; treating as corrupt (will \
                 quarantine). Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };
    if file.version != CHAR_FAVORITES_VERSION {
        // Forward/backward compatible: warn but keep the fields we understand.
        crate::runtime_log::log_warn(format!(
            "typing: {CHAR_FAVORITES_FILE_NAME} version {} != expected {CHAR_FAVORITES_VERSION}; \
             parsing known fields only. Path: {}",
            file.version,
            path.display()
        ));
    }
    LoadOutcome::Loaded(chars_from_json_array(
        &file.characters,
        &path.to_string_lossy(),
    ))
}

/// How many `.bad` destinations are probed before quarantine gives up.
///
/// A user who has hit a hundred distinct corruptions of one file has a problem
/// this code cannot fix; refusing is better than looping.
const MAX_QUARANTINE_CANDIDATES: u32 = 100;

/// Picks a quarantine destination that does not exist yet.
///
/// `{file}.bad` first, then `{file}.bad.1`, `{file}.bad.2`, … The plain rename
/// used underneath REPLACES an existing destination (`std::fs::rename` on Unix),
/// so reusing one name would destroy the previously quarantined copy — which is
/// the very content quarantine exists to preserve.
///
/// # Errors
/// [`FavoritesError::Quarantine`] when every probed name is taken.
fn free_quarantine_path(path: &Path) -> Result<PathBuf, FavoritesError> {
    let store = storage();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| CHAR_FAVORITES_FILE_NAME.to_owned());
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
    Err(FavoritesError::Quarantine {
        path: path.display().to_string(),
        destination: parent.join(format!("{file_name}.bad")).display().to_string(),
        reason: format!(
            "no free destination among {MAX_QUARANTINE_CANDIDATES} candidates; earlier \
             quarantined copies must be removed first"
        ),
    })
}

/// Moves a MALFORMED project document aside before replacement is permitted.
///
/// The destination is the first free `{file}.bad`/`{file}.bad.N` name, so an
/// earlier quarantined copy is never overwritten. Only a document known to be
/// malformed may be passed here — an unread one may be perfectly good.
///
/// # Errors
/// Returns [`FavoritesError::Quarantine`] when the recoverable original could
/// not be moved; callers must leave the list unchanged and unsaved.
pub(super) fn quarantine_bad_project_document(path: &Path) -> Result<(), FavoritesError> {
    let bad = free_quarantine_path(path)?;
    let store = storage();
    store
        .rename(
            path.to_string_lossy().as_ref(),
            bad.to_string_lossy().as_ref(),
        )
        .map_err(|err| FavoritesError::Quarantine {
            path: path.display().to_string(),
            destination: bad.display().to_string(),
            reason: err.to_string(),
        })?;
    crate::runtime_log::log_warn(format!(
        "typing: quarantined corrupt {CHAR_FAVORITES_FILE_NAME} to {}",
        bad.display()
    ));
    Ok(())
}

/// Writes `chars` to the project document at `path`, creating the parent
/// directory if needed.
///
/// The write is atomic: a sibling temp file is written first and then renamed
/// over the target, so a crash mid-write cannot truncate an existing list.
///
/// # Errors
/// [`FavoritesError`] on directory creation, serialization, write, or rename
/// failure. Callers persist off the GUI thread.
pub(super) fn save_project_document(path: &Path, chars: &[char]) -> Result<(), FavoritesError> {
    let store = storage();
    if let Some(parent) = path.parent() {
        let parent_str = parent.to_string_lossy();
        store
            .create_dir_all(parent_str.as_ref())
            .map_err(|err| FavoritesError::CreateDir {
                dir: parent.display().to_string(),
                reason: err.to_string(),
            })?;
    }
    let file = CharFavoritesFile {
        version: CHAR_FAVORITES_VERSION,
        characters: global_favorites_json(&normalize(chars.iter().copied())),
    };
    let mut text =
        serde_json::to_string_pretty(&file).map_err(|err| FavoritesError::Serialize {
            reason: err.to_string(),
        })?;
    text.push('\n');

    // Temp sibling + rename: the target is replaced atomically, so a crash
    // between the two steps leaves the previous list intact. The temp name is
    // per-process so two processes cannot collide on it.
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| CHAR_FAVORITES_FILE_NAME.to_owned());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let temp_str = temp.to_string_lossy().into_owned();
    store
        .write(temp_str.as_str(), text.as_bytes())
        .map_err(|err| FavoritesError::Write {
            path: temp.display().to_string(),
            reason: err.to_string(),
        })?;
    store
        .rename(temp_str.as_str(), path.to_string_lossy().as_ref())
        .map_err(|err| {
            // Best-effort cleanup of the orphaned temp file; the rename failure
            // is the error we report (a failed cleanup must not mask it).
            if let Err(cleanup_err) = store.remove_file(temp_str.as_str()) {
                crate::runtime_log::log_warn(format!(
                    "typing: could not remove orphaned temp file {temp_str}: {cleanup_err}"
                ));
            }
            FavoritesError::Rename {
                path: path.display().to_string(),
                reason: err.to_string(),
            }
        })
}

// ---------------------------------------------------------------------------
// The two stores
// ---------------------------------------------------------------------------

/// Global (application-wide) favorite characters, backed by `user_config.json`.
#[derive(Debug, Default)]
pub(super) struct GlobalFavorites {
    chars: Vec<char>,
}

impl GlobalFavorites {
    /// Replaces the list from an already-parsed user-config value.
    pub(super) fn load_from_values(&mut self, values: Option<&Vec<Value>>, path: &Path) {
        self.chars = values.map_or_else(Vec::new, |values| {
            chars_from_json_array(values, &path.to_string_lossy())
        });
    }

    /// The favorites in user order.
    #[must_use]
    pub(super) fn chars(&self) -> &[char] {
        &self.chars
    }

    /// Whether `ch` is a global favorite.
    #[must_use]
    pub(super) fn contains(&self, ch: char) -> bool {
        self.chars.contains(&ch)
    }

    /// Adds or removes `ch`; the owner publishes one complete config snapshot.
    ///
    /// Always changes the list (hence always returns `true`, mirroring
    /// [`ProjectFavorites::toggle`]'s "the store changed" contract). Adding
    /// appends to the end (user order); removing preserves the order of the rest.
    pub(super) fn toggle(&mut self, ch: char) -> bool {
        if let Some(pos) = self.chars.iter().position(|&c| c == ch) {
            self.chars.remove(pos);
        } else {
            self.chars.push(ch);
        }
        true
    }
}

/// State of the project favorites document as last observed on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::tabs::typing::panel) enum ProjectDocumentState {
    /// No path bound yet (no project open) — nothing can be read or written.
    #[default]
    Unbound,
    /// The document is absent or was read successfully; saving is allowed.
    Ready,
    /// The currently bound document is being read by a worker.
    Loading,
    /// The document exists but is MALFORMED. The in-memory list is empty and
    /// saving is REFUSED until the user's next explicit toggle, which
    /// quarantines the file first.
    Invalid,
    /// The document exists but could not be read or its load result never
    /// arrived, so its content is unknown. Saving is refused and the file is
    /// left exactly as it is — it is NOT quarantined and the empty in-memory
    /// list does not mean the user has no favorites.
    Unreadable,
    /// Quarantine failed; saving remains blocked to protect the original file.
    QuarantineFailed,
}

/// Complete project save request with its target captured at mutation time.
#[derive(Debug)]
struct ProjectSaveSnapshot {
    path: PathBuf,
    chars: Vec<char>,
}

impl super::SnapshotTarget for ProjectSaveSnapshot {
    fn target(&self) -> &Path {
        &self.path
    }
}

/// Writes one captured project snapshot.
fn save_project_snapshot(snapshot: ProjectSaveSnapshot) -> Result<(), String> {
    save_project_document(&snapshot.path, &snapshot.chars).map_err(|err| err.to_string())
}

/// Title-scoped favorite characters, backed by `{title_dir}/char_favorites.json`.
///
/// TITLE-scoped, not chapter-scoped: every chapter of one manga shares one list
/// (`dev-docs/char_table_plan.md` §2). All filesystem access goes through
/// `crate::storage::storage()`.
#[derive(Debug)]
pub(super) struct ProjectFavorites {
    path: Option<PathBuf>,
    chars: Vec<char>,
    state: ProjectDocumentState,
    load_rx: Option<Receiver<(PathBuf, LoadOutcome)>>,
    writer: super::SnapshotWriter<ProjectSaveSnapshot>,
}

impl Default for ProjectFavorites {
    fn default() -> Self {
        Self {
            path: None,
            chars: Vec::new(),
            state: ProjectDocumentState::Unbound,
            load_rx: None,
            writer: super::SnapshotWriter::new(
                "typing-save-char-favorites-project",
                save_project_snapshot,
            ),
        }
    }
}

impl ProjectFavorites {
    /// Binds the store to `path` (`ProjectPaths::char_favorites_file`) and reloads.
    ///
    /// Passing the SAME path again is a no-op, so a per-frame setter call from
    /// the UI does not re-read the file every frame. Passing `None` unbinds and
    /// clears the list (no project open).
    pub(super) fn set_path(&mut self, path: Option<PathBuf>) {
        if self.path == path {
            return;
        }
        self.path = path;
        self.start_reload();
    }

    /// The bound document path, or `None` when no project is open.
    #[must_use]
    pub(super) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Starts a project-document read without blocking the GUI thread.
    ///
    /// Tests execute the transition inline so private fixtures stay
    /// deterministic; production delivers through [`ProjectFavorites::poll`].
    fn start_reload(&mut self) {
        let Some(path) = self.path.clone() else {
            self.chars.clear();
            self.state = ProjectDocumentState::Unbound;
            self.load_rx = None;
            return;
        };
        self.chars.clear();
        self.state = ProjectDocumentState::Loading;
        if cfg!(test) {
            let outcome = load_project_document(&path);
            self.apply_load(path, outcome);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-load-char-favorites-project".to_string())
            .spawn(move || {
                let outcome = load_project_document(&worker_path);
                if tx.send((worker_path, outcome)).is_err() {
                    crate::runtime_log::log_warn(
                        "typing: project character-favorites load result was superseded",
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
                self.state = ProjectDocumentState::Unreadable;
                crate::runtime_log::log_error(format!(
                    "typing: failed to spawn project character-favorites loader; the title's \
                     favorites stay read-only. Path: {} Error: {err}",
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
                self.chars.clear();
                self.state = ProjectDocumentState::Ready;
            }
            LoadOutcome::Loaded(chars) => {
                self.chars = chars;
                self.state = ProjectDocumentState::Ready;
            }
            LoadOutcome::Invalid => {
                self.chars.clear();
                self.state = ProjectDocumentState::Invalid;
            }
            LoadOutcome::Unreadable => {
                self.chars.clear();
                self.state = ProjectDocumentState::Unreadable;
            }
        }
    }

    /// Polls the project loader without blocking.
    ///
    /// Returns `true` when the visible list or document state changed.
    pub(super) fn poll(&mut self) -> bool {
        let Some(rx) = self.load_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok((path, outcome)) => {
                self.load_rx = None;
                self.apply_load(path, outcome);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker died without sending: the document was never
                // classified, so it is unknown, not corrupt.
                self.load_rx = None;
                self.state = ProjectDocumentState::Unreadable;
                crate::runtime_log::log_error(
                    "typing: the project character-favorites load result never arrived; the \
                     title's favorites stay read-only",
                );
                true
            }
        }
    }

    /// The favorites in user order (empty when unbound or corrupt).
    #[must_use]
    pub(super) fn chars(&self) -> &[char] {
        &self.chars
    }

    /// Whether `ch` is a project favorite.
    #[must_use]
    pub(super) fn contains(&self, ch: char) -> bool {
        self.chars.contains(&ch)
    }

    /// Current document state; the window uses it to explain an unavailable or
    /// corrupt list.
    #[must_use]
    pub(super) fn state(&self) -> ProjectDocumentState {
        self.state
    }

    /// Adds or removes `ch` and persists the new list off-thread.
    ///
    /// Returns `false` without touching anything unless the document is `Ready`
    /// or `Invalid`. On `Invalid` — and ONLY there, where the file is known to be
    /// malformed — this explicit user action first quarantines it to a free
    /// `char_favorites.json.bad*` name, so the corrupt content is preserved and
    /// the new list starts from the (empty) in-memory state. A document that was
    /// merely not readable is left completely alone.
    pub(super) fn toggle(&mut self, ch: char) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        match self.state {
            ProjectDocumentState::Ready => {}
            ProjectDocumentState::Invalid => {
                // The user explicitly asked to change the list, so the MALFORMED
                // file is moved aside instead of being overwritten in place.
                if let Err(err) = quarantine_bad_project_document(&path) {
                    crate::runtime_log::log_error(format!(
                        "typing: could not quarantine corrupt {CHAR_FAVORITES_FILE_NAME}; project favorites remain read-only. Error: {err}"
                    ));
                    self.state = ProjectDocumentState::QuarantineFailed;
                    return false;
                }
                self.state = ProjectDocumentState::Ready;
            }
            // Unbound cannot reach here (the path check above returned), and the
            // remaining states all mean "the stored list is unknown or protected":
            // writing would destroy content this process never saw.
            ProjectDocumentState::Unbound
            | ProjectDocumentState::Loading
            | ProjectDocumentState::Unreadable
            | ProjectDocumentState::QuarantineFailed => return false,
        }
        if let Some(pos) = self.chars.iter().position(|&c| c == ch) {
            self.chars.remove(pos);
        } else {
            self.chars.push(ch);
        }
        self.writer.enqueue(ProjectSaveSnapshot {
            path,
            chars: self.chars.clone(),
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique temp path so parallel tests never share a file.
    fn unique_temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_char_favorites_{tag}_{nanos}.json"))
    }

    /// Removes a temp file after a test.
    ///
    /// The result is deliberately discarded: cleanup runs after every assertion
    /// has already been made, and a missing file is the normal case for the
    /// tests that never create one.
    fn cleanup(path: &Path) {
        let _ = storage().remove_file(path.to_string_lossy().as_ref());
    }

    /// Unwraps a `Loaded` outcome or panics naming the actual variant.
    fn expect_loaded(outcome: LoadOutcome) -> Vec<char> {
        match outcome {
            LoadOutcome::Loaded(chars) => chars,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
            LoadOutcome::Unreadable => panic!("expected Loaded, got Unreadable"),
        }
    }

    #[test]
    fn project_document_round_trip_preserves_order() {
        let path = unique_temp_path("roundtrip");
        let chars = vec!['★', '→', '±', '♪'];
        save_project_document(&path, &chars).expect("save must succeed");
        assert_eq!(expect_loaded(load_project_document(&path)), chars);
        cleanup(&path);
    }

    #[test]
    fn project_document_collapses_duplicates_keeping_first_position() {
        let path = unique_temp_path("duplicates");
        // '→' repeats: only the FIRST occurrence survives, so the order of the
        // remaining entries is unchanged.
        let chars = vec!['→', '★', '→', '±', '★'];
        save_project_document(&path, &chars).expect("save must succeed");
        assert_eq!(
            expect_loaded(load_project_document(&path)),
            vec!['→', '★', '±']
        );
        cleanup(&path);
    }

    #[test]
    fn missing_project_document_is_missing_outcome() {
        let path = unique_temp_path("missing");
        assert!(matches!(load_project_document(&path), LoadOutcome::Missing));
    }

    #[test]
    fn malformed_project_document_is_invalid_outcome() {
        let path = unique_temp_path("malformed");
        storage()
            .write(path.to_string_lossy().as_ref(), b"{ not json")
            .expect("write malformed doc");
        // A corrupt file must be Invalid (so the caller quarantines it), NOT
        // silently empty — the next toggle would otherwise destroy it.
        assert!(matches!(load_project_document(&path), LoadOutcome::Invalid));
        cleanup(&path);
    }

    #[test]
    fn unknown_version_still_parses_known_fields() {
        let path = unique_temp_path("future_version");
        storage()
            .write(
                path.to_string_lossy().as_ref(),
                r#"{ "version": 99, "characters": ["★"], "future": 1 }"#.as_bytes(),
            )
            .expect("write future-version doc");
        assert_eq!(expect_loaded(load_project_document(&path)), vec!['★']);
        cleanup(&path);
    }

    #[test]
    fn multi_character_entries_are_ignored() {
        let path = unique_temp_path("multi_char");
        storage()
            .write(
                path.to_string_lossy().as_ref(),
                r#"{ "version": 1, "characters": ["ab", "", "★", 5] }"#.as_bytes(),
            )
            .expect("write doc with bad entries");
        // A favorite is exactly ONE character; everything else is skipped.
        assert_eq!(expect_loaded(load_project_document(&path)), vec!['★']);
        cleanup(&path);
    }

    #[test]
    fn invalid_document_is_quarantined_on_the_next_toggle() {
        let path = unique_temp_path("quarantine");
        storage()
            .write(path.to_string_lossy().as_ref(), b"{ not json")
            .expect("write corrupt doc");

        let mut store = ProjectFavorites::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state(), ProjectDocumentState::Invalid);
        assert!(store.chars().is_empty(), "a corrupt file degrades to empty");

        // The corrupt file must still be on disk: loading alone must NOT touch it.
        assert!(storage().exists(path.to_string_lossy().as_ref()));

        assert!(store.toggle('★'), "an explicit toggle must be accepted");
        let bad = path.with_extension("json.bad");
        // What the toggle owns is the QUARANTINE: the corrupt bytes must be at
        // the `.bad` name, unchanged. It says nothing about the original path —
        // the enqueued save (a no-op under `cfg!(test)`, real in production)
        // recreates it right away, which is why this test must not assert the
        // path is gone.
        assert_eq!(
            storage()
                .read_to_string(bad.to_string_lossy().as_ref())
                .expect("the corrupt document must land at char_favorites.json.bad"),
            "{ not json",
            "quarantine must preserve the corrupt bytes verbatim"
        );
        assert_eq!(store.state(), ProjectDocumentState::Ready);
        assert_eq!(store.chars(), &['★']);
        cleanup(&path);
        cleanup(&bad);
    }

    #[test]
    fn quarantine_never_overwrites_an_earlier_bad_copy() {
        let path = unique_temp_path("quarantine_twice");
        let first_bad = path.with_extension("json.bad");
        storage()
            .write(first_bad.to_string_lossy().as_ref(), b"older corruption")
            .expect("write an earlier quarantined copy");
        storage()
            .write(path.to_string_lossy().as_ref(), b"newer corruption")
            .expect("write corrupt doc");

        quarantine_bad_project_document(&path).expect("quarantine must find a free name");

        // The first quarantined copy is the only record of an earlier corruption;
        // replacing it would destroy the content quarantine exists to keep.
        assert_eq!(
            storage()
                .read_to_string(first_bad.to_string_lossy().as_ref())
                .expect("the earlier copy must still exist"),
            "older corruption"
        );
        let second_bad = PathBuf::from(format!("{}.1", first_bad.display()));
        assert_eq!(
            storage()
                .read_to_string(second_bad.to_string_lossy().as_ref())
                .expect("the new copy must take the next free name"),
            "newer corruption"
        );
        cleanup(&first_bad);
        cleanup(&second_bad);
    }

    #[test]
    fn an_unreadable_document_is_neither_quarantined_nor_saved_over() {
        // A DIRECTORY at the document path is the portable way to make the read
        // fail while `exists` is true — the transient EACCES/EMFILE case cannot be
        // provoked from a test, but it reaches the same branch.
        let path = unique_temp_path("unreadable");
        storage()
            .create_dir_all(path.to_string_lossy().as_ref())
            .expect("create the directory standing in for an unreadable file");

        assert!(matches!(
            load_project_document(&path),
            LoadOutcome::Unreadable
        ));

        let mut store = ProjectFavorites::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state(), ProjectDocumentState::Unreadable);
        assert!(
            !store.toggle('★'),
            "an unread document must block writes instead of starting from empty"
        );
        assert_eq!(
            store.state(),
            ProjectDocumentState::Unreadable,
            "a rejected toggle must not move the state on"
        );
        assert!(
            !storage().exists(path.with_extension("json.bad").to_string_lossy().as_ref()),
            "an unread document must NEVER be quarantined"
        );
        assert!(
            storage().exists(path.to_string_lossy().as_ref()),
            "the original must be left exactly where it is"
        );
        if let Err(err) = storage().remove_dir_all(path.to_string_lossy().as_ref()) {
            panic!("cannot remove fixture {}: {err}", path.display());
        }
    }

    #[test]
    fn project_snapshot_save_writes_the_document_and_reports_failure() {
        // `ProjectFavorites::toggle` enqueues this exact function, and the
        // enqueue itself is a no-op under `cfg!(test)`, so the write path is only
        // genuinely covered by calling it directly.
        let path = unique_temp_path("snapshot_save");
        let outcome = save_project_snapshot(ProjectSaveSnapshot {
            path: path.clone(),
            chars: vec!['★', '→', '★'],
        });
        if let Err(err) = outcome {
            panic!("a project snapshot must be written: {err}");
        }
        assert_eq!(expect_loaded(load_project_document(&path)), vec!['★', '→']);

        // A parent that is a FILE cannot hold the document: the failure must be
        // reported, not swallowed.
        let blocked = path.join("nested").join(CHAR_FAVORITES_FILE_NAME);
        let failure = save_project_snapshot(ProjectSaveSnapshot {
            path: blocked,
            chars: vec!['★'],
        });
        assert!(
            failure.is_err(),
            "a snapshot that cannot be written must return the error"
        );
        cleanup(&path);
    }

    #[test]
    fn toggle_without_a_path_is_rejected() {
        let mut store = ProjectFavorites::default();
        assert_eq!(store.state(), ProjectDocumentState::Unbound);
        assert!(!store.toggle('★'), "no project bound: nothing to toggle");
        assert!(store.chars().is_empty());
    }

    #[test]
    fn project_store_loads_an_existing_document_on_bind() {
        let path = unique_temp_path("bind");
        save_project_document(&path, &['→', '★']).expect("save must succeed");
        let mut store = ProjectFavorites::default();
        store.set_path(Some(path.clone()));
        assert_eq!(store.state(), ProjectDocumentState::Ready);
        assert_eq!(store.chars(), &['→', '★']);
        // Toggling an existing entry removes it and keeps the rest in order.
        assert!(store.toggle('→'));
        assert_eq!(store.chars(), &['★']);
        cleanup(&path);
    }

    #[test]
    fn global_store_toggles_in_memory_and_never_writes() {
        // This store has no persistence of its own at all: its owner publishes a
        // complete `UserConfigSnapshot` through `SnapshotWriter`. What is covered
        // here is therefore its whole contract — order + duplicate collapse.
        let mut store = GlobalFavorites::default();
        assert!(store.toggle('★'));
        assert!(store.toggle('→'));
        assert!(store.contains('★'));
        assert_eq!(store.chars(), &['★', '→']);
        assert!(store.toggle('★'));
        assert!(!store.contains('★'));
        assert_eq!(store.chars(), &['→']);
    }

    // The WRITE side of the global store lives in `super::save_user_config_snapshot`
    // (one transaction for all three settings) and is covered there. What belongs
    // here is the READ side this module owns: turning the raw config array into a
    // character list.

    #[test]
    fn global_favorites_load_collapses_duplicates_and_keeps_order() {
        let values = vec![
            Value::String("★".to_owned()),
            Value::String("→".to_owned()),
            Value::String("★".to_owned()),
        ];
        let mut store = GlobalFavorites::default();
        store.load_from_values(Some(&values), Path::new("test-config.json"));
        assert_eq!(store.chars(), &['★', '→']);
    }

    #[test]
    fn global_favorites_load_without_a_value_is_empty() {
        // A missing key and a non-array value both arrive here as `None`, which
        // means "no favorites yet" rather than a corrupt document.
        let mut store = GlobalFavorites::default();
        store.load_from_values(None, Path::new("test-config.json"));
        assert!(store.chars().is_empty());
    }

    #[test]
    fn global_favorites_load_skips_junk_elements() {
        // One malformed element must not discard the user's whole list.
        let values = vec![
            Value::String("★".to_owned()),
            Value::from(7),
            Value::String("many chars".to_owned()),
            Value::String("→".to_owned()),
        ];
        let mut store = GlobalFavorites::default();
        store.load_from_values(Some(&values), Path::new("test-config.json"));
        assert_eq!(store.chars(), &['★', '→']);
    }
}
