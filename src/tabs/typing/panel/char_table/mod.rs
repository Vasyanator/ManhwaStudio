/*
File: panel/char_table/mod.rs

Purpose:
State of the typing tab's character-table window ("Таблица символов"): which tab
is shown, how large the cells are, which symbol is expanded, the two favorite
lists, and the background glyph-coverage job. This file is the DATA layer — it
contains no egui code at all (see "Where the window goes" below).

Main responsibilities:
- own `CharTableState` and its whole data contract (open flag, selected group,
  cell size, expanded character, star-popup target);
- own the two favorite stores and expose membership/toggle operations;
- drive the background coverage job (spawn on a font-list change, poll, query);
- load and persist the window's two `TextTab` settings off the GUI thread.

Key types:
- `CharTableState` (the whole window state)
- `SnapshotWriter` + `SnapshotTarget` (the one coalescing writer per store, whose
  pending slot is keyed by the file a snapshot is destined for)
- `UserConfigSnapshot` (the complete `TextTab` state of one save)

Key functions:
- `CharTableState::new` / `ensure_loaded` / `poll`
- `all_chars` (the flattened character set, built once per process)

Notes:
Persisted keys (`TextTab.char_table_font_size`, `TextTab.char_table_last_group`,
`TextTab.char_table_global_favorites`) and the group keys from `charset.rs` are
STABLE identities that go to disk: they are never localized and never renamed
without a migration (`dev-docs/i18n_exclusions.md`).

Where the window is:
`window.rs` — the `egui::Window` itself (size control, tab strip, wrapping grid,
expanded variants block, star popup, `dev-docs/char_table_plan.md` §7). It is a
FREE FUNCTION taking disjoint borrows, not a method, because the window must read
the panel font list, mutate this state, and cause an edit of the panel's text
buffer; the edit is returned as a `CharTableAction` instead of performed there.
*/

mod charset;
mod coverage;
mod favorites;
mod window;

pub(super) use window::{CharTableAction, draw_char_table_window};

use crate::config;
use crate::tabs::typing::panel::FontEntry;
use coverage::CoverageJob;
use favorites::{GlobalFavorites, ProjectDocumentState, ProjectFavorites};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use ms_thread as thread;

/// Tab key of the favorites tab. It is NOT a `charset.rs` group (favorites have
/// no fixed character list); it shares the group-key namespace only so the
/// persisted "last tab" setting can name it.
pub(super) const FAVORITES_TAB_KEY: &str = "favorites";

/// `TextTab` key holding the character cell size.
const TEXT_TAB_FONT_SIZE_KEY: &str = "char_table_font_size";
/// `TextTab` key holding the last selected tab.
const TEXT_TAB_LAST_GROUP_KEY: &str = "char_table_last_group";

/// Default character cell size in points (`dev-docs/char_table_plan.md` §7).
pub(super) const DEFAULT_CELL_FONT_SIZE: f32 = 30.0;
/// Smallest selectable cell size.
pub(super) const MIN_CELL_FONT_SIZE: f32 = 12.0;
/// Largest selectable cell size.
pub(super) const MAX_CELL_FONT_SIZE: f32 = 96.0;

/// Every character of the table, flattened in tab order.
///
/// Built once per process (the table is a compile-time constant, so the vector
/// is immutable read-only state — the sanctioned `OnceLock` use). It is the
/// input of the coverage job.
#[must_use]
pub(super) fn all_chars() -> &'static [char] {
    static ALL: OnceLock<Vec<char>> = OnceLock::new();
    ALL.get_or_init(|| {
        charset::groups()
            .iter()
            .flat_map(|group| group.chars.iter().copied())
            .collect()
    })
}

/// A snapshot that knows which file it is destined for.
///
/// The coalescing writer keys its pending slot by this path: replacing a pending
/// snapshot is only sound WITHIN one target, because "last write wins" says
/// nothing about two writes aimed at two different files. A store whose target
/// can change (the project document follows the open title) would otherwise drop
/// the older target's newest state entirely.
pub(super) trait SnapshotTarget {
    /// File this snapshot will be written to; the writer's coalescing key.
    fn target(&self) -> &Path;
}

/// Mutable slot shared between an enqueueing GUI owner and its sole writer.
///
/// `pending` holds AT MOST ONE snapshot per target, so a burst on one file
/// coalesces while a different file keeps its own newest snapshot.
#[derive(Debug)]
struct WriterSlot<T> {
    pending: BTreeMap<PathBuf, T>,
    running: bool,
}

/// Locks a writer slot, recovering the state of a poisoned mutex.
///
/// The slot is a plain data holder guarded only during O(1) map operations, so a
/// writer that unwound elsewhere cannot have left it half-updated; refusing to
/// write from then on would be strictly worse than continuing.
fn lock_slot<T>(slot: &Mutex<WriterSlot<T>>) -> MutexGuard<'_, WriterSlot<T>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Coalescing per-target last-snapshot-wins writer.
///
/// Enqueue replaces the pending complete snapshot OF THAT TARGET. At most one
/// worker is live; it drains one target's newest snapshot after every write and
/// exits when nothing is pending, clearing `running` under the same lock that
/// found the slot empty (and from an RAII guard if a write unwinds).
///
/// Shared infrastructure of the whole `panel` module, not only of this window:
/// the typing tab's color presets (`panel::color_presets_store`) use it too, which
/// is why nothing here names a specific document.
#[derive(Debug)]
pub(super) struct SnapshotWriter<T: SnapshotTarget + Send + 'static> {
    slot: Arc<Mutex<WriterSlot<T>>>,
    thread_name: &'static str,
    save: fn(T) -> Result<(), String>,
}

impl<T: SnapshotTarget + Send + 'static> SnapshotWriter<T> {
    /// Creates an idle writer using `save` for each selected snapshot.
    #[must_use]
    pub(super) fn new(thread_name: &'static str, save: fn(T) -> Result<(), String>) -> Self {
        Self {
            slot: Arc::new(Mutex::new(WriterSlot {
                pending: BTreeMap::new(),
                running: false,
            })),
            thread_name,
            save,
        }
    }

    /// Publishes the newest snapshot for its target without doing I/O on the caller.
    ///
    /// Tests early-return so ordinary state tests never touch real files.
    pub(super) fn enqueue(&self, snapshot: T) {
        if cfg!(test) {
            return;
        }
        self.enqueue_inner(snapshot);
    }

    /// Publishes a snapshot and starts the single worker when necessary.
    fn enqueue_inner(&self, snapshot: T) {
        let should_spawn = {
            let mut slot = lock_slot(&self.slot);
            // Keyed by target: only a snapshot for the SAME file may supersede
            // this one.
            slot.pending
                .insert(snapshot.target().to_path_buf(), snapshot);
            if slot.running {
                false
            } else {
                slot.running = true;
                true
            }
        };
        if !should_spawn {
            return;
        }
        let slot = Arc::clone(&self.slot);
        let save = self.save;
        let thread_name = self.thread_name;
        let spawn_result = thread::Builder::new()
            .name(self.thread_name.to_string())
            .spawn(move || writer_loop(&slot, save, thread_name));
        if let Err(err) = spawn_result {
            let mut guard = lock_slot(&self.slot);
            guard.running = false;
            crate::runtime_log::log_error(format!(
                "typing: failed to spawn the {} writer; change not persisted: {err}",
                self.thread_name
            ));
        }
    }
}

/// Clears the slot's `running` flag if the writer thread unwinds.
///
/// Without it a panic inside `save` would leave `running == true` forever: every
/// later enqueue would fill the slot and never spawn, silently stopping ALL
/// writes of that store for the rest of the process. The orderly exit path
/// disarms the guard, because it must clear the flag under the SAME lock that
/// observed the slot empty — clearing it a second time later could cancel a
/// writer another thread has meanwhile spawned.
struct RunningFlagGuard<'a, T> {
    slot: &'a Mutex<WriterSlot<T>>,
    armed: bool,
}

impl<T> Drop for RunningFlagGuard<'_, T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Snapshots still pending here are written by the writer that the next
        // enqueue is now free to spawn.
        lock_slot(self.slot).running = false;
    }
}

/// Drains a writer slot one target at a time, holding no lock during a write.
///
/// `thread_name` only labels the failure log: this writer serves several stores
/// (the character-table favorites and the typing tab's color presets), so a
/// message naming one of them would misreport the others.
fn writer_loop<T: SnapshotTarget + Send + 'static>(
    slot: &Arc<Mutex<WriterSlot<T>>>,
    save: fn(T) -> Result<(), String>,
    thread_name: &'static str,
) {
    let mut running = RunningFlagGuard {
        slot: slot.as_ref(),
        armed: true,
    };
    loop {
        let next = {
            let mut guard = lock_slot(slot);
            match guard.pending.pop_first() {
                Some((_target, snapshot)) => Some(snapshot),
                None => {
                    // Clearing the flag while still holding the lock is what
                    // makes "no work left" and "no writer live" one atomic fact,
                    // so a concurrent enqueue either sees the slot busy (and
                    // leaves the work to us) or spawns its own writer.
                    guard.running = false;
                    running.armed = false;
                    None
                }
            }
        };
        let Some(snapshot) = next else {
            return;
        };
        if let Err(err) = save(snapshot) {
            crate::runtime_log::log_error(format!(
                "typing: {thread_name}: failed to persist a snapshot: {err}"
            ));
        }
    }
}

/// Complete character-table portion of `user_config.json`.
#[derive(Debug)]
struct UserConfigSnapshot {
    path: PathBuf,
    cell_font_size: f32,
    selected_group: String,
    /// The global favorites to store, or `None` to LEAVE THE KEY ON DISK ALONE.
    ///
    /// `None` is the state after a failed settings read: the in-memory list is
    /// then not known to be the user's list, so writing it would destroy the real
    /// one. The other two settings still persist.
    global_favorites: Option<Vec<char>>,
}

impl SnapshotTarget for UserConfigSnapshot {
    fn target(&self) -> &Path {
        &self.path
    }
}

/// Writes one complete character-table settings snapshot in one serialized
/// user-config transaction.
///
/// A `None` favorites member leaves `TextTab.char_table_global_favorites` exactly
/// as it is on disk; the size and tab settings are always written.
fn save_user_config_snapshot(snapshot: UserConfigSnapshot) -> Result<(), String> {
    let favorites = snapshot
        .global_favorites
        .as_deref()
        .map(favorites::global_favorites_json);
    config::update_user_config_file(&snapshot.path, move |root| {
        let root_obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("user config root must be an object"))?;
        let text_tab = root_obj
            .entry("TextTab".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !text_tab.is_object() {
            *text_tab = Value::Object(Map::new());
        }
        let text_tab_obj = text_tab
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("TextTab must be an object"))?;
        text_tab_obj.insert(
            TEXT_TAB_FONT_SIZE_KEY.to_string(),
            Value::from(f64::from(snapshot.cell_font_size)),
        );
        text_tab_obj.insert(
            TEXT_TAB_LAST_GROUP_KEY.to_string(),
            Value::String(snapshot.selected_group),
        );
        if let Some(favorites) = favorites {
            text_tab_obj.insert(
                favorites::TEXT_TAB_GLOBAL_FAVORITES_KEY.to_string(),
                Value::Array(favorites),
            );
        }
        Ok(())
    })
    .map_err(|err| format!("{err:#}"))
}

/// Whole state of the character-table window.
///
/// Constructed with [`CharTableState::new`] as part of `TypingCreatePanelState`;
/// nothing is read from disk until the window is first opened
/// ([`CharTableState::ensure_loaded`]), so a user who never opens it pays
/// nothing.
#[derive(Debug)]
pub(super) struct CharTableState {
    /// Window visibility, bound directly to `egui::Window::open`.
    open: bool,
    /// Selected tab: a `charset` group key or [`FAVORITES_TAB_KEY`].
    selected_group: String,
    /// Character cell size in points, clamped to
    /// [`MIN_CELL_FONT_SIZE`]..=[`MAX_CELL_FONT_SIZE`].
    cell_font_size: f32,
    /// The symbol whose per-font variants row is currently expanded.
    expanded_char: Option<char>,
    /// The symbol whose star popup is currently open.
    star_popup_char: Option<char>,
    global_favorites: GlobalFavorites,
    project_favorites: ProjectFavorites,
    /// Merged favorites view backing the favorites tab; rebuilt on every change
    /// so the tab can be rendered from a plain slice.
    favorites_view: Vec<char>,
    coverage: CoverageJob,
    user_config_writer: SnapshotWriter<UserConfigSnapshot>,
    /// Whether the persisted settings and favorites have been read yet.
    loaded: bool,
    /// Whether the global favorites key may be WRITTEN this session.
    ///
    /// Cleared for the rest of the process when the one settings read failed: an
    /// empty in-memory list would then be an artifact of that failure, and
    /// serializing it (on the next size drag or tab switch) would destroy the
    /// user's real list. The size and tab settings keep persisting.
    global_favorites_writable: bool,
}

impl Default for CharTableState {
    fn default() -> Self {
        Self::new()
    }
}

impl CharTableState {
    /// Builds the closed, unloaded state with the built-in defaults.
    ///
    /// Performs no I/O: the persisted settings and both favorite lists are read
    /// on the first [`CharTableState::ensure_loaded`], i.e. when the window is
    /// first opened.
    #[must_use]
    pub(super) fn new() -> Self {
        Self {
            open: false,
            selected_group: charset::groups()
                .first()
                .map_or_else(|| FAVORITES_TAB_KEY.to_string(), |group| group.key.to_string()),
            cell_font_size: DEFAULT_CELL_FONT_SIZE,
            expanded_char: None,
            star_popup_char: None,
            global_favorites: GlobalFavorites::default(),
            project_favorites: ProjectFavorites::default(),
            favorites_view: Vec::new(),
            coverage: CoverageJob::default(),
            user_config_writer: SnapshotWriter::new(
                "typing-save-char-table-config",
                save_user_config_snapshot,
            ),
            loaded: false,
            global_favorites_writable: true,
        }
    }

    // -- window visibility ---------------------------------------------------

    /// Whether the window is currently shown.
    #[must_use]
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Mutable open flag for `egui::Window::open`.
    ///
    /// Call [`CharTableState::ensure_loaded`] before showing the window; the
    /// flag itself intentionally has no side effects so egui's close button can
    /// write it directly.
    pub(super) fn open_mut(&mut self) -> &mut bool {
        &mut self.open
    }

    /// Opens the window and loads everything it needs, or closes it.
    pub(super) fn set_open(&mut self, open: bool) {
        self.open = open;
        if open {
            self.ensure_loaded();
        }
    }

    /// Toggles the window, loading on the opening edge.
    pub(super) fn toggle_open(&mut self) {
        self.set_open(!self.open);
    }

    /// Reads the persisted settings and both favorite lists once.
    ///
    /// Later calls are no-ops — the flag is never reset, so this is once per
    /// PROCESS, not once per window open. The small local user config is read and
    /// parsed synchronously because opening the window is user-initiated and the
    /// file is bounded; the project-tree document is loaded by a worker.
    ///
    /// A FAILED read is not retried and does not fall back to "the user has no
    /// favorites": it permanently forbids writing the global favorites key
    /// ([`CharTableState::global_favorites_writable`]).
    pub(super) fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let config_path = config::user_config_path();
        // One bounded local read supplies all three values; project storage may
        // be network-backed and is therefore handled by `ProjectFavorites`.
        let cfg = match config::JsonConfig::new(&config_path, Value::Object(Map::new())) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                // The file may be perfectly valid and merely unreadable right now
                // (permissions, descriptor exhaustion, a network-backed home).
                // Whatever it holds must stay there: this session writes the size
                // and tab settings but never the favorites key.
                self.global_favorites_writable = false;
                crate::runtime_log::log_warn(format!(
                    "typing: char table: cannot read settings; using defaults and keeping the \
                     stored global favorites untouched for this session. Path: {} Error: {err:#}",
                    config_path.display()
                ));
                None
            }
        };
        if let Some(size) = cfg
            .as_ref()
            .and_then(|cfg| cfg.get_path(&["TextTab", TEXT_TAB_FONT_SIZE_KEY]))
            .and_then(Value::as_f64)
        {
            // A stored value outside the selectable range (hand-edited config or
            // an older build) is clamped rather than rejected. Clamping happens
            // in `f64` FIRST, so the narrowing cast below is always inside
            // 12.0..=96.0 — a range every value of which `f32` represents
            // exactly enough for a font size; `f64 -> f32` has no `try_from`.
            let clamped = size.clamp(
                f64::from(MIN_CELL_FONT_SIZE),
                f64::from(MAX_CELL_FONT_SIZE),
            );
            self.cell_font_size = clamped as f32;
        }
        if let Some(group) = cfg
            .as_ref()
            .and_then(|cfg| cfg.get_path(&["TextTab", TEXT_TAB_LAST_GROUP_KEY]))
            .and_then(Value::as_str)
            .map(str::to_owned)
            && is_known_tab(&group)
        {
            // An unknown key (a group removed since it was persisted) falls back
            // to the default tab chosen in `new`.
            self.selected_group = group;
        }
        let global_values = cfg.as_ref().and_then(|cfg| {
            cfg.get_path(&["TextTab", favorites::TEXT_TAB_GLOBAL_FAVORITES_KEY])
                .and_then(Value::as_array)
        });
        self.global_favorites
            .load_from_values(global_values, &config_path);
        self.rebuild_favorites_view();
    }

    // -- tabs ----------------------------------------------------------------

    /// The selected tab key (a `charset` group key or [`FAVORITES_TAB_KEY`]).
    #[must_use]
    pub(super) fn selected_group(&self) -> &str {
        &self.selected_group
    }

    /// Selects a tab and persists the choice off-thread.
    ///
    /// Unknown keys are ignored (the selection stays where it was), and the
    /// expanded symbol is collapsed because its row belongs to the old tab.
    pub(super) fn set_selected_group(&mut self, key: &str) {
        if self.selected_group == key || !is_known_tab(key) {
            return;
        }
        self.selected_group = key.to_string();
        self.expanded_char = None;
        self.star_popup_char = None;
        self.persist_user_config();
    }

    /// All character groups, in tab order. The favorites tab is not among them.
    #[must_use]
    pub(super) fn groups() -> &'static [charset::CharGroup] {
        charset::groups()
    }

    /// Characters shown by the currently selected tab.
    ///
    /// For the favorites tab this is the merged view (project list first, then
    /// the global entries it does not already contain).
    #[must_use]
    pub(super) fn visible_chars(&self) -> &[char] {
        if self.selected_group == FAVORITES_TAB_KEY {
            return &self.favorites_view;
        }
        charset::group_by_key(&self.selected_group).map_or(&[], |group| group.chars)
    }

    // -- cell size -----------------------------------------------------------

    /// Character cell size in points.
    #[must_use]
    pub(super) fn cell_font_size(&self) -> f32 {
        self.cell_font_size
    }

    /// Sets the cell size and publishes a complete config snapshot off-thread.
    ///
    /// During a drag, pending intermediate values are replaced so the single
    /// writer eventually commits the newest size.
    pub(super) fn set_cell_font_size(&mut self, size: f32) {
        let size = clamp_cell_font_size(size);
        if (self.cell_font_size - size).abs() < f32::EPSILON {
            return;
        }
        self.cell_font_size = size;
        self.persist_user_config();
    }

    // -- expansion and popups ------------------------------------------------

    /// The symbol whose per-font variants row is expanded, if any.
    #[must_use]
    pub(super) fn expanded_char(&self) -> Option<char> {
        self.expanded_char
    }

    /// Expands `ch`, or collapses it when it is already the expanded symbol.
    pub(super) fn toggle_expanded(&mut self, ch: char) {
        self.expanded_char = if self.expanded_char == Some(ch) {
            None
        } else {
            Some(ch)
        };
    }

    /// Collapses the expanded symbol (e.g. after an insertion).
    pub(super) fn collapse(&mut self) {
        self.expanded_char = None;
    }

    /// The symbol whose star popup is open, if any.
    #[must_use]
    pub(super) fn star_popup_char(&self) -> Option<char> {
        self.star_popup_char
    }

    /// Opens the star popup for `ch` (or closes it with `None`).
    pub(super) fn set_star_popup_char(&mut self, ch: Option<char>) {
        self.star_popup_char = ch;
    }

    // -- favorites -----------------------------------------------------------

    /// Binds the project favorites store to the title-scoped document
    /// (`ProjectPaths::char_favorites_file`), reloading it on a real change.
    ///
    /// Passing `None` unbinds it (no project open). The window's owner calls
    /// this; the store ignores a repeated identical path, so a per-frame call is
    /// cheap.
    pub(super) fn set_project_favorites_path(&mut self, path: Option<PathBuf>) {
        if self.project_favorites.path() == path.as_deref() {
            return;
        }
        self.project_favorites.set_path(path);
        self.rebuild_favorites_view();
    }

    /// Whether `ch` is in the title-scoped project list.
    #[must_use]
    pub(super) fn is_project_favorite(&self, ch: char) -> bool {
        self.project_favorites.contains(ch)
    }

    /// Whether `ch` is in the global (application-wide) list.
    #[must_use]
    pub(super) fn is_global_favorite(&self, ch: char) -> bool {
        self.global_favorites.contains(ch)
    }

    /// Adds/removes `ch` in the project list and persists off-thread.
    ///
    /// Returns `false` when no project is bound (nothing changed).
    pub(super) fn toggle_project_favorite(&mut self, ch: char) -> bool {
        let changed = self.project_favorites.toggle(ch);
        if changed {
            self.rebuild_favorites_view();
        }
        changed
    }

    /// Adds/removes `ch` in the global list and persists off-thread.
    ///
    /// After a failed settings read the change stays in memory only: the stored
    /// list is not known here and must not be overwritten
    /// ([`CharTableState::global_favorites_writable`]).
    pub(super) fn toggle_global_favorite(&mut self, ch: char) -> bool {
        let changed = self.global_favorites.toggle(ch);
        if changed {
            if !self.global_favorites_writable {
                crate::runtime_log::log_warn(
                    "typing: char table: the settings read failed earlier, so the global \
                     favorites change applies to this session only and is not written to disk",
                );
            }
            self.rebuild_favorites_view();
            self.persist_user_config();
        }
        changed
    }

    /// State of the project document: unbound (no project), loading, ready,
    /// malformed, unreadable, or blocked by a failed quarantine. The window
    /// turns it into the wording that explains an unavailable list.
    #[must_use]
    pub(super) fn project_favorites_state(&self) -> ProjectDocumentState {
        self.project_favorites.state()
    }

    /// Rebuilds the merged favorites view: the project list in its own order,
    /// then the global entries it does not already contain.
    fn rebuild_favorites_view(&mut self) {
        self.favorites_view.clear();
        self.favorites_view
            .extend_from_slice(self.project_favorites.chars());
        for &ch in self.global_favorites.chars() {
            if !self.favorites_view.contains(&ch) {
                self.favorites_view.push(ch);
            }
        }
        // A favorite that was just removed must not stay expanded in a tab that
        // no longer shows it.
        if self.selected_group == FAVORITES_TAB_KEY
            && let Some(expanded) = self.expanded_char
            && !self.favorites_view.contains(&expanded)
        {
            self.expanded_char = None;
        }
    }

    /// Publishes the complete config-side state to its coalescing writer.
    fn persist_user_config(&self) {
        self.user_config_writer.enqueue(self.user_config_snapshot());
    }

    /// Builds the config snapshot this state would persist right now.
    ///
    /// Its favorites member is `None` — "leave the stored list alone" — whenever
    /// the settings read failed, see
    /// [`CharTableState::global_favorites_writable`].
    #[must_use]
    fn user_config_snapshot(&self) -> UserConfigSnapshot {
        UserConfigSnapshot {
            path: config::user_config_path(),
            cell_font_size: self.cell_font_size,
            selected_group: self.selected_group.clone(),
            global_favorites: self
                .global_favorites_writable
                .then(|| self.global_favorites.chars().to_vec()),
        }
    }

    // -- glyph coverage ------------------------------------------------------

    /// Starts a background coverage computation when `fonts` changed since the
    /// last one. Cheap enough to call every frame (it compares a fingerprint).
    pub(super) fn ensure_coverage(&mut self, fonts: &[FontEntry]) {
        self.coverage.ensure(fonts, all_chars());
    }

    /// Picks up a finished coverage result. Call once per frame.
    pub(super) fn poll(&mut self) {
        if self.project_favorites.poll() {
            self.rebuild_favorites_view();
        }
        self.coverage.poll();
    }

    /// Whether a coverage computation is still running.
    #[must_use]
    pub(super) fn coverage_in_flight(&self) -> bool {
        self.coverage.in_flight()
    }

    /// Indices into the panel font list of the fonts that can draw `ch`.
    ///
    /// The bundled-stack entry is never listed (see `coverage.rs`): the window
    /// offers it unconditionally as the first variant.
    #[must_use]
    pub(super) fn fonts_for_char(&self, ch: char) -> &[usize] {
        self.coverage.fonts_for(ch)
    }
}

/// Clamps a cell size into the selectable range.
#[must_use]
fn clamp_cell_font_size(size: f32) -> f32 {
    size.clamp(MIN_CELL_FONT_SIZE, MAX_CELL_FONT_SIZE)
}

/// Whether `key` names a tab that exists: a `charset` group or the favorites tab.
#[must_use]
fn is_known_tab(key: &str) -> bool {
    key == FAVORITES_TAB_KEY || charset::group_by_key(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    /// Test snapshot used to exercise the real writer loop without user files.
    struct TestSnapshot {
        path: PathBuf,
        text: &'static str,
        delay: Duration,
        /// Whether `save` must panic instead of writing, to prove the writer
        /// survives an unwinding save (see the panic-safety test).
        panics: bool,
    }

    impl SnapshotTarget for TestSnapshot {
        fn target(&self) -> &Path {
            &self.path
        }
    }

    /// Builds a plain writable snapshot with no delay.
    fn test_snapshot(path: &Path, text: &'static str) -> TestSnapshot {
        TestSnapshot {
            path: path.to_path_buf(),
            text,
            delay: Duration::ZERO,
            panics: false,
        }
    }

    /// Writes a test snapshot after its optional delay.
    fn save_test_snapshot(snapshot: TestSnapshot) -> Result<(), String> {
        if !snapshot.delay.is_zero() {
            std::thread::sleep(snapshot.delay);
        }
        assert!(
            !snapshot.panics,
            "deliberate test panic inside a writer save"
        );
        std::fs::write(&snapshot.path, snapshot.text).map_err(|err| err.to_string())
    }

    /// Covers the WRITE side of the config-backed settings: all three members
    /// land in ONE `TextTab` transaction, and the favorites list is normalized
    /// (duplicates collapsed, user order kept) on the way out.
    #[test]
    fn user_config_snapshot_writes_all_three_settings_in_one_transaction() {
        let path = writer_test_path();
        let outcome = save_user_config_snapshot(UserConfigSnapshot {
            path: path.clone(),
            cell_font_size: 42.0,
            selected_group: FAVORITES_TAB_KEY.to_owned(),
            global_favorites: Some(vec!['★', '→', '★']),
        });
        if let Err(err) = outcome {
            panic!("user config transaction must succeed: {err}");
        }
        let cfg = match config::JsonConfig::new(&path, Value::Object(Map::new())) {
            Ok(cfg) => cfg,
            Err(err) => panic!("cannot re-read the written config: {err:#}"),
        };
        assert_eq!(
            cfg.get_path(&["TextTab", TEXT_TAB_FONT_SIZE_KEY])
                .and_then(Value::as_f64),
            Some(42.0)
        );
        assert_eq!(
            cfg.get_path(&["TextTab", TEXT_TAB_LAST_GROUP_KEY])
                .and_then(Value::as_str),
            Some(FAVORITES_TAB_KEY)
        );
        let mut store = favorites::GlobalFavorites::default();
        store.load_from_values(
            cfg.get_path(&["TextTab", favorites::TEXT_TAB_GLOBAL_FAVORITES_KEY])
                .and_then(Value::as_array),
            &path,
        );
        assert_eq!(store.chars(), &['★', '→']);
        if let Err(err) = std::fs::remove_file(&path) {
            panic!("cannot remove config fixture {}: {err}", path.display());
        }
    }

    /// Unique path for a parallel-safe writer fixture.
    ///
    /// The counter is what makes two fixtures of ONE test distinct: a clock read
    /// alone can repeat when two paths are taken in the same tick.
    fn writer_test_path() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ms_char_table_writer_{nanos}_{seq}.json"))
    }

    /// Waits until the writer is idle or the test deadline expires.
    fn wait_for_writer(writer: &SnapshotWriter<TestSnapshot>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let idle = {
                let slot = lock_slot(&writer.slot);
                !slot.running && slot.pending.is_empty()
            };
            if idle {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    #[test]
    fn all_chars_flattens_every_group_without_duplicates() {
        let expected: usize = charset::groups()
            .iter()
            .map(|group| group.chars.len())
            .sum();
        let all = all_chars();
        assert_eq!(all.len(), expected);
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "the flattened set must be unique");
    }

    #[test]
    fn new_state_is_closed_on_the_first_group() {
        let state = CharTableState::new();
        assert!(!state.is_open());
        assert_eq!(
            state.selected_group(),
            charset::groups().first().map_or("", |group| group.key)
        );
        assert!(state.expanded_char().is_none());
        assert!(state.fonts_for_char('A').is_empty());
    }

    #[test]
    fn selecting_an_unknown_tab_is_ignored() {
        let mut state = CharTableState::new();
        let before = state.selected_group().to_string();
        state.set_selected_group("no_such_group");
        assert_eq!(state.selected_group(), before);
        state.set_selected_group(FAVORITES_TAB_KEY);
        assert_eq!(state.selected_group(), FAVORITES_TAB_KEY);
        assert!(state.visible_chars().is_empty(), "no favorites yet");
    }

    #[test]
    fn cell_size_is_clamped_to_the_selectable_range() {
        let mut state = CharTableState::new();
        state.set_cell_font_size(1000.0);
        assert!((state.cell_font_size() - MAX_CELL_FONT_SIZE).abs() < f32::EPSILON);
        state.set_cell_font_size(0.0);
        assert!((state.cell_font_size() - MIN_CELL_FONT_SIZE).abs() < f32::EPSILON);
    }

    #[test]
    fn expansion_toggles_and_collapses() {
        let mut state = CharTableState::new();
        state.toggle_expanded('→');
        assert_eq!(state.expanded_char(), Some('→'));
        state.toggle_expanded('→');
        assert!(state.expanded_char().is_none());
        state.toggle_expanded('★');
        state.collapse();
        assert!(state.expanded_char().is_none());
    }

    #[test]
    fn switching_tabs_collapses_the_expanded_symbol() {
        let mut state = CharTableState::new();
        state.toggle_expanded('→');
        state.set_star_popup_char(Some('→'));
        state.set_selected_group(FAVORITES_TAB_KEY);
        assert!(state.expanded_char().is_none());
        assert!(state.star_popup_char().is_none());
    }

    #[test]
    fn global_favorites_feed_the_favorites_tab() {
        // Persistence is a no-op under `cfg!(test)`, so this exercises only the
        // in-memory merge (project list first, then the global remainder).
        let mut state = CharTableState::new();
        assert!(state.toggle_global_favorite('★'));
        assert!(state.toggle_global_favorite('→'));
        state.set_selected_group(FAVORITES_TAB_KEY);
        assert_eq!(state.visible_chars(), &['★', '→']);
        assert!(state.is_global_favorite('★'));
        assert!(!state.is_project_favorite('★'));

        // Removing the expanded favorite must collapse it.
        state.toggle_expanded('★');
        assert!(state.toggle_global_favorite('★'));
        assert_eq!(state.visible_chars(), &['→']);
        assert!(state.expanded_char().is_none());
    }

    #[test]
    fn project_favorites_are_rejected_without_a_bound_document() {
        let mut state = CharTableState::new();
        assert_eq!(
            state.project_favorites_state(),
            ProjectDocumentState::Unbound
        );
        assert!(!state.toggle_project_favorite('★'));
        assert!(!state.is_project_favorite('★'));
    }

    #[test]
    fn every_group_key_is_a_known_tab() {
        for group in CharTableState::groups() {
            assert!(is_known_tab(group.key), "group {} must be a tab", group.key);
        }
        assert!(is_known_tab(FAVORITES_TAB_KEY));
        assert!(!is_known_tab(""));
    }

    #[test]
    fn coalescing_writer_commits_the_last_snapshot_from_a_burst() {
        let path = writer_test_path();
        let writer = SnapshotWriter::new("char-table-writer-test", save_test_snapshot);
        writer.enqueue_inner(TestSnapshot {
            path: path.clone(),
            text: "first",
            delay: Duration::from_millis(50),
            panics: false,
        });
        for text in ["second", "third", "last"] {
            writer.enqueue_inner(test_snapshot(&path, text));
        }
        assert!(wait_for_writer(&writer), "writer did not become idle");
        let actual = match std::fs::read_to_string(&path) {
            Ok(actual) => actual,
            Err(err) => panic!("cannot read writer fixture {}: {err}", path.display()),
        };
        assert_eq!(actual, "last");
        if let Err(err) = std::fs::remove_file(&path) {
            panic!("cannot remove writer fixture {}: {err}", path.display());
        }
    }

    /// Coalescing is per TARGET: a snapshot aimed at another file must never
    /// replace one that has not been written yet (the project document follows
    /// the open title, so both targets are live within one session).
    #[test]
    fn coalescing_writer_keeps_one_pending_snapshot_per_target() {
        let path_a = writer_test_path();
        let path_b = writer_test_path();
        assert_ne!(path_a, path_b, "the fixtures must be two distinct files");
        let writer = SnapshotWriter::new("char-table-writer-targets-test", save_test_snapshot);

        // The first write is slow, so everything below lands in the pending slot
        // while it runs — exactly the situation that used to drop a target.
        writer.enqueue_inner(TestSnapshot {
            path: path_a.clone(),
            text: "a-first",
            delay: Duration::from_millis(80),
            panics: false,
        });
        writer.enqueue_inner(test_snapshot(&path_a, "a-newest"));
        writer.enqueue_inner(test_snapshot(&path_b, "b-newest"));

        assert!(wait_for_writer(&writer), "writer did not become idle");
        for (path, expected) in [(&path_a, "a-newest"), (&path_b, "b-newest")] {
            let actual = match std::fs::read_to_string(path) {
                Ok(actual) => actual,
                Err(err) => panic!("cannot read writer fixture {}: {err}", path.display()),
            };
            assert_eq!(actual, expected, "target {} lost its newest snapshot", path.display());
            if let Err(err) = std::fs::remove_file(path) {
                panic!("cannot remove writer fixture {}: {err}", path.display());
            }
        }
    }

    /// An unwinding `save` must not strand `running == true`, which would stop
    /// every later write of that store for the process lifetime. The panic is
    /// deliberate, so a panic report on stderr is expected output of this test.
    #[test]
    fn writer_survives_a_panicking_save() {
        let path = writer_test_path();
        let writer = SnapshotWriter::new("char-table-writer-panic-test", save_test_snapshot);
        writer.enqueue_inner(TestSnapshot {
            path: path.clone(),
            text: "never written",
            delay: Duration::ZERO,
            panics: true,
        });
        assert!(
            wait_for_writer(&writer),
            "the running flag was never cleared after an unwinding save"
        );

        // The store must still accept work afterwards.
        writer.enqueue_inner(test_snapshot(&path, "after the panic"));
        assert!(wait_for_writer(&writer), "writer did not become idle");
        let actual = match std::fs::read_to_string(&path) {
            Ok(actual) => actual,
            Err(err) => panic!("cannot read writer fixture {}: {err}", path.display()),
        };
        assert_eq!(actual, "after the panic");
        if let Err(err) = std::fs::remove_file(&path) {
            panic!("cannot remove writer fixture {}: {err}", path.display());
        }
    }

    /// A snapshot with `global_favorites: None` persists the size and the tab but
    /// leaves the stored favorites array exactly as it was — the behavior that
    /// keeps a transient settings-read failure from wiping the user's list.
    #[test]
    fn a_none_favorites_member_leaves_the_stored_list_untouched() {
        let path = writer_test_path();
        let stored = save_user_config_snapshot(UserConfigSnapshot {
            path: path.clone(),
            cell_font_size: 20.0,
            selected_group: FAVORITES_TAB_KEY.to_owned(),
            global_favorites: Some(vec!['★', '→']),
        });
        if let Err(err) = stored {
            panic!("the initial transaction must succeed: {err}");
        }
        let updated = save_user_config_snapshot(UserConfigSnapshot {
            path: path.clone(),
            cell_font_size: 30.0,
            selected_group: "arrows".to_owned(),
            global_favorites: None,
        });
        if let Err(err) = updated {
            panic!("the favorites-preserving transaction must succeed: {err}");
        }

        let cfg = match config::JsonConfig::new(&path, Value::Object(Map::new())) {
            Ok(cfg) => cfg,
            Err(err) => panic!("cannot re-read the written config: {err:#}"),
        };
        assert_eq!(
            cfg.get_path(&["TextTab", TEXT_TAB_FONT_SIZE_KEY])
                .and_then(Value::as_f64),
            Some(30.0),
            "the size must still persist after a failed settings read"
        );
        assert_eq!(
            cfg.get_path(&["TextTab", TEXT_TAB_LAST_GROUP_KEY])
                .and_then(Value::as_str),
            Some("arrows"),
            "the tab must still persist after a failed settings read"
        );
        let mut store = favorites::GlobalFavorites::default();
        store.load_from_values(
            cfg.get_path(&["TextTab", favorites::TEXT_TAB_GLOBAL_FAVORITES_KEY])
                .and_then(Value::as_array),
            &path,
        );
        assert_eq!(
            store.chars(),
            &['★', '→'],
            "the stored favorites must survive a snapshot that does not own them"
        );
        if let Err(err) = std::fs::remove_file(&path) {
            panic!("cannot remove config fixture {}: {err}", path.display());
        }
    }

    /// The state stops offering its in-memory favorites once the settings read
    /// has failed, so no later size drag or tab switch can serialize them.
    #[test]
    fn a_failed_settings_read_stops_writing_the_favorites_key() {
        let mut state = CharTableState::new();
        assert!(state.toggle_global_favorite('★'));
        assert!(
            state.user_config_snapshot().global_favorites.is_some(),
            "a healthy session writes its favorites"
        );

        // What `ensure_loaded` does when `JsonConfig::new` fails.
        state.global_favorites_writable = false;
        state.set_cell_font_size(44.0);
        let snapshot = state.user_config_snapshot();
        assert!(
            snapshot.global_favorites.is_none(),
            "an unread list must never be written back"
        );
        assert!((snapshot.cell_font_size - 44.0).abs() < f32::EPSILON);
        // The in-memory list still answers membership questions this session.
        assert!(state.is_global_favorite('★'));
    }
}
