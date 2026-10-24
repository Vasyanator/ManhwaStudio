/*
File: src/tabs/settings/canvas_layout_presets.rs

Purpose:
The layout preset block of Settings › «Лента и пузыри»: an `ms_widgets::PresetPicker` over the
built-in presets «Pages» / «Webtoon» and the user's own presets, the decisions behind each
picker action, and the ordered off-thread writer that persists every store operation into
`user_config` (`Canvas.layout_presets`).

Key structures:
- LayoutPresetsPane: the in-memory store (`ms_project::CanvasLayoutPresets`), the picker state,
  the status lines, the entries whose last write failed and the lazily started writer.
- PresetRow: what one combo row stands for (also the identity of a store entry).
- WriteOutcome: the writer's answer for one operation (its entries, success or the error).

Key functions:
- LayoutPresetsPane::rebind(): installs the store pre-read at studio open and validates the
  title's selection against it.
- LayoutPresetsPane::draw(): the picker block plus its status line; applies the chosen action to
  the title's selection and canvas settings.
- LayoutPresetsPane::effective_builtin(): what a built-in means now (used by the first-open
  prompt through `SettingsTabState::apply_title_kind_layout`).

Notes:
- The VALUES (13 layout fields) and the SELECTION live per title (`SharedCanvasSettings` and
  `canvas.layout_preset`, persisted by the pane's canvas-settings saver); only the presets
  themselves are app-wide. The store and its rules (override / revert, version guard, the JSON
  codec) are owned by `ms_project::canvas_layout`; this file only decides what each click means.
- Built-ins: editable and savable (their override), never renamable (`rename(false)`); «Delete»
  on a modified built-in drops the override and applies the factory values to THIS title (the
  selection stays); an unmodified built-in has no «Delete». Saving a built-in with its factory
  values removes the override.
- User presets: created from the current values under a free default name; renamed verbatim on
  «Save» (an empty name and a name of any other preset — user or built-in — are refused); a
  confirmed «Delete» removes the preset and leaves the title at «Нет» with its values kept. A
  title elsewhere that selected a renamed or deleted user preset reads as «Нет» when opened.
- Failed writes: the in-memory store is ahead of the disk after a failed write. An entry whose
  LAST write failed stays "write failed" until a later write of that same entry succeeds (each
  write stores the entry's current in-memory state, so a later success makes the disk match): while
  it is selected the picker's unsaved warning stays raised (its «Save» retries the write), and the
  write-error line stays under the picker while ANY entry is failed (a failed delete has no row
  left to mark). Selecting another preset clears only the validation line, never the write error.
- Each operation is its own read-modify-write on ONE writer thread, in arrival order, so another
  app instance's presets survive and a quick save-then-delete never lands reversed. The writer is
  joined when the pane drops (on the studio's teardown worker, never the GUI thread). Without a
  store path (tests, the unbound default) operations stay in memory.
*/

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crate::models::bubbles_model::SharedCanvasSettings;
use crate::project::{BuiltinLayoutPreset, CanvasLayout, CanvasLayoutPresets, LayoutPresetOp, LayoutPresetSelection, apply_layout_preset_op};
use crate::runtime_log;
use crate::widgets::{PresetPicker, PresetPickerAction, PresetPickerState};
use ms_thread::{self as thread, JoinHandle};

/// i18n key of the combo label, also the picker's stable id salt.
const PRESET_LABEL_KEY: &str = "settings.canvas_ribbon.preset_label";

/// What one combo row stands for, in combo order: the built-ins, then the user presets by name.
/// Also the identity of a store entry in the failed-write bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PresetRow {
    Builtin(BuiltinLayoutPreset),
    User(String),
}

impl PresetRow {
    /// The row of `selection`'s entry; `None` for «Нет».
    fn of_selection(selection: &LayoutPresetSelection) -> Option<Self> {
        match selection {
            LayoutPresetSelection::None => None,
            LayoutPresetSelection::Builtin(preset) => Some(Self::Builtin(*preset)),
            LayoutPresetSelection::User(name) => Some(Self::User(name.clone())),
        }
    }

    /// The selection this row makes.
    fn selection(&self) -> LayoutPresetSelection {
        match self {
            Self::Builtin(preset) => LayoutPresetSelection::Builtin(*preset),
            Self::User(name) => LayoutPresetSelection::User(name.clone()),
        }
    }

    /// The row's visible name.
    fn label(&self) -> String {
        match self {
            Self::Builtin(preset) => preset.display_name().to_owned(),
            Self::User(name) => name.clone(),
        }
    }
}

/// The store entries `op` writes (both names of a rename).
fn op_entries(op: &LayoutPresetOp) -> Vec<PresetRow> {
    match op {
        LayoutPresetOp::UpsertUser { name, .. } | LayoutPresetOp::RemoveUser { name } => vec![PresetRow::User(name.clone())],
        LayoutPresetOp::RenameUser { from, to, .. } => vec![PresetRow::User(from.clone()), PresetRow::User(to.clone())],
        LayoutPresetOp::SaveBuiltin { preset, .. } | LayoutPresetOp::ResetBuiltin { preset } => vec![PresetRow::Builtin(*preset)],
    }
}

/// The writer's answer for one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteOutcome {
    /// The entries the operation wrote ([`op_entries`]).
    entries: Vec<PresetRow>,
    /// `None` on success, else the technical error.
    error: Option<String>,
}

/// The ordered store writer: one thread applying [`LayoutPresetOp`]s to one `user_config` file
/// in arrival order, answering every operation with a [`WriteOutcome`].
#[derive(Debug)]
struct PresetWriter {
    /// `None` only while dropping (closing it ends the thread's loop).
    tx: Option<Sender<LayoutPresetOp>>,
    handle: Option<JoinHandle<()>>,
}

/// The layout preset block and its store. See the file header for the rules.
#[derive(Debug)]
pub(super) struct LayoutPresetsPane {
    presets: CanvasLayoutPresets,
    /// Rename buffer + armed delete; reset at every selection change.
    picker: PresetPickerState,
    /// The last refused action (validation, read-only store), shown until the next selection change.
    status: Option<String>,
    /// The last write failure, shown while `failed` is not empty.
    write_error: Option<String>,
    /// Entries whose last write failed (see the file header).
    failed: BTreeSet<PresetRow>,
    /// Where writes go; `None` keeps every operation in memory.
    store_path: Option<PathBuf>,
    /// Started on the first write.
    writer: Option<PresetWriter>,
    outcomes_tx: Sender<WriteOutcome>,
    outcomes_rx: Receiver<WriteOutcome>,
}

impl Default for LayoutPresetsPane {
    /// An empty store without persistence (the state before the studio binds a title).
    fn default() -> Self {
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        Self {
            presets: CanvasLayoutPresets::default(),
            picker: PresetPickerState::default(),
            status: None,
            write_error: None,
            failed: BTreeSet::new(),
            store_path: None,
            writer: None,
            outcomes_tx,
            outcomes_rx,
        }
    }
}

impl Drop for LayoutPresetsPane {
    /// Closes the writer's queue and waits for the queued writes, so an operation made just
    /// before the studio closes still reaches the disk. The settings tab is dropped by the studio's
    /// teardown worker, never on the GUI thread.
    fn drop(&mut self) {
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        drop(writer.tx.take());
        if let Some(handle) = writer.handle.take()
            && handle.join().is_err()
        {
            runtime_log::log_error("[settings::layout_presets] the layout preset writer thread panicked; the last preset operations may not be saved");
        }
    }
}

impl LayoutPresetsPane {
    /// Installs `presets` (pre-read from `user_config` at studio open; no I/O here) and the
    /// `store_path` later operations persist to, and validates the title's `selection` against
    /// it: a user preset that no longer exists becomes «Нет» (saved with the next canvas-settings
    /// save). A writer that is already running is kept (same document).
    pub(super) fn rebind(&mut self, presets: CanvasLayoutPresets, store_path: Option<PathBuf>, selection: &mut LayoutPresetSelection) {
        self.presets = presets;
        self.store_path = store_path;
        if let LayoutPresetSelection::User(name) = selection
            && !self.presets.contains_user(name)
        {
            runtime_log::log_info(format!("[settings::layout_presets] the title's layout preset {name:?} no longer exists; the title now has no preset (its values are kept)"));
            *selection = LayoutPresetSelection::None;
        }
        self.sync_picker(selection);
    }

    /// What `preset` means now (its override, else the factory row).
    #[must_use]
    pub(super) fn effective_builtin(&self, preset: BuiltinLayoutPreset) -> CanvasLayout {
        self.presets.effective_builtin(preset)
    }

    /// Re-syncs the picker (rename buffer, armed delete) and the validation line with
    /// `selection` (a write error stays). Call at every selection change, including those made
    /// outside [`Self::draw`].
    pub(super) fn sync_picker(&mut self, selection: &LayoutPresetSelection) {
        let name = self.row_of(selection).map(|idx| self.rows()[idx].label());
        self.picker.reset_to(name.as_deref());
        self.status = None;
    }

    /// Every combo row in order: the built-ins, then the user presets (byte-sorted).
    #[must_use]
    fn rows(&self) -> Vec<PresetRow> {
        BuiltinLayoutPreset::ALL.into_iter().map(PresetRow::Builtin).chain(self.presets.user_names().map(|name| PresetRow::User(name.to_owned()))).collect()
    }

    /// The row index of `selection`; `None` for «Нет» and for a user preset that does not exist.
    #[must_use]
    fn row_of(&self, selection: &LayoutPresetSelection) -> Option<usize> {
        match selection {
            LayoutPresetSelection::None => None,
            LayoutPresetSelection::Builtin(preset) => BuiltinLayoutPreset::ALL.iter().position(|row| row == preset),
            LayoutPresetSelection::User(name) => {
                self.presets.user_names().position(|row| row == name).map(|idx| idx + BuiltinLayoutPreset::ALL.len())
            }
        }
    }

    /// Whether the picker shows `selection` as unsaved: the title's current values differ from what
    /// it stands for, or its entry's last write failed. `false` with no selection (the picker's
    /// pending rename is its own half of "unsaved").
    #[must_use]
    fn is_unsaved(&self, selection: &LayoutPresetSelection, settings: &SharedCanvasSettings) -> bool {
        let write_failed = PresetRow::of_selection(selection).is_some_and(|row| self.failed.contains(&row));
        write_failed || self.presets.effective(selection).is_some_and(|preset| preset != settings.layout())
    }

    /// Whether «Delete» is offered: with no selection (drawn disabled, so the row does not jump),
    /// for a user preset and for a modified built-in. An unmodified built-in has nothing to delete.
    #[must_use]
    fn shows_delete(&self, selection: &LayoutPresetSelection) -> bool {
        match selection {
            LayoutPresetSelection::None | LayoutPresetSelection::User(_) => true,
            LayoutPresetSelection::Builtin(preset) => self.presets.is_builtin_modified(*preset),
        }
    }

    /// Whether `name` is taken by any preset: a user preset or a built-in's display name.
    #[must_use]
    fn name_taken(&self, name: &str) -> bool {
        self.presets.contains_user(name) || BuiltinLayoutPreset::ALL.iter().any(|preset| preset.display_name() == name)
    }

    /// Draws the picker block and its status line and applies the action asked for to the
    /// title's `selection` and `settings`. Returns `true` when either changed, so the caller
    /// publishes (model, overlays, saver).
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, selection: &mut LayoutPresetSelection, settings: &mut SharedCanvasSettings) -> bool {
        self.poll_write_outcomes();
        let rows = self.rows();
        let names: Vec<String> = rows.iter().map(PresetRow::label).collect();
        let builtin_selected = matches!(selection, LayoutPresetSelection::Builtin(_));
        let response = PresetPicker::new(PRESET_LABEL_KEY, t!("settings.canvas_ribbon.preset_label"), &names, self.row_of(selection))
            .unsaved(self.is_unsaved(selection, settings))
            .delete(self.shows_delete(selection))
            // A built-in's name is fixed (its display name is a translation): the field is disabled.
            .rename(!builtin_selected)
            .show(ui, &mut self.picker);
        response.combo.on_hover_text(t!("settings.canvas_ribbon.preset_scope_hint"));
        if builtin_selected && let Some(delete) = response.delete {
            delete.on_hover_text(t!("settings.canvas_ribbon.preset_builtin_revert_tooltip"));
        }
        let changed = match response.action {
            Some(PresetPickerAction::Deselect) => self.deselect(selection),
            Some(PresetPickerAction::Create) => self.create(selection, settings),
            Some(PresetPickerAction::Select(idx)) => rows.get(idx).is_some_and(|row| self.select(row, selection, settings)),
            Some(PresetPickerAction::Save) => self.save(selection, settings),
            // The widget reports the selected index; deleting it is deleting the selection.
            Some(PresetPickerAction::Delete(_)) => self.delete_selected(selection, settings),
            None => false,
        };
        if let Some(status) = &self.status {
            ui.colored_label(ms_theme::status::ERROR, status);
        }
        if !self.failed.is_empty()
            && let Some(write_error) = &self.write_error
        {
            ui.colored_label(ms_theme::status::ERROR, write_error);
        }
        changed
    }

    /// Selects `row` and applies ALL its values to `settings`.
    fn select(&mut self, row: &PresetRow, selection: &mut LayoutPresetSelection, settings: &mut SharedCanvasSettings) -> bool {
        let chosen = row.selection();
        let Some(layout) = self.presets.effective(&chosen) else {
            return false;
        };
        settings.apply_layout(&layout);
        *selection = chosen;
        self.sync_picker(selection);
        true
    }

    /// «Нет»: drops the selection; the values stay as they are.
    fn deselect(&mut self, selection: &mut LayoutPresetSelection) -> bool {
        *selection = LayoutPresetSelection::None;
        self.sync_picker(selection);
        true
    }

    /// Creates a user preset from the current values under a free default name and selects it.
    fn create(&mut self, selection: &mut LayoutPresetSelection, settings: &SharedCanvasSettings) -> bool {
        let name = self.free_default_name();
        if !self.commit(LayoutPresetOp::UpsertUser { name: name.clone(), layout: settings.layout() }) {
            return false;
        }
        *selection = LayoutPresetSelection::User(name);
        self.sync_picker(selection);
        true
    }

    /// Commits the selected preset: its values from `settings` and, for a user preset, the name
    /// in the rename buffer (verbatim). Refused with a status line: an empty name, a name another
    /// preset holds, a read-only store. Returns `true` when the selection was renamed (the title's
    /// remembered selection changes); the values themselves are unchanged.
    fn save(&mut self, selection: &mut LayoutPresetSelection, settings: &SharedCanvasSettings) -> bool {
        let layout = settings.layout();
        match selection.clone() {
            LayoutPresetSelection::None => false,
            LayoutPresetSelection::Builtin(preset) => {
                if self.commit(LayoutPresetOp::SaveBuiltin { preset, layout }) {
                    self.sync_picker(selection);
                }
                false
            }
            LayoutPresetSelection::User(current) => {
                let new_name = self.picker.name().to_owned();
                if new_name.is_empty() {
                    self.status = Some(t!("settings.canvas_ribbon.preset_empty_name_status").to_owned());
                    return false;
                }
                if new_name != current && self.name_taken(&new_name) {
                    self.status = Some(tf!("settings.canvas_ribbon.preset_name_taken_status", name = new_name));
                    return false;
                }
                let renamed = new_name != current;
                let op = if renamed {
                    LayoutPresetOp::RenameUser { from: current, to: new_name.clone(), layout }
                } else {
                    LayoutPresetOp::UpsertUser { name: current, layout }
                };
                if !self.commit(op) {
                    return false;
                }
                *selection = LayoutPresetSelection::User(new_name);
                self.sync_picker(selection);
                renamed
            }
        }
    }

    /// Deletes the selection: a user preset is removed and the title goes to «Нет» (values kept);
    /// a modified built-in loses its override, stays selected and its factory values are applied
    /// to `settings`. An unmodified built-in is left alone.
    fn delete_selected(&mut self, selection: &mut LayoutPresetSelection, settings: &mut SharedCanvasSettings) -> bool {
        match selection.clone() {
            LayoutPresetSelection::None => false,
            LayoutPresetSelection::Builtin(preset) => {
                if !self.presets.is_builtin_modified(preset) || !self.commit(LayoutPresetOp::ResetBuiltin { preset }) {
                    return false;
                }
                settings.apply_layout(&preset.factory());
                self.sync_picker(selection);
                true
            }
            LayoutPresetSelection::User(name) => {
                if !self.commit(LayoutPresetOp::RemoveUser { name }) {
                    return false;
                }
                *selection = LayoutPresetSelection::None;
                self.sync_picker(selection);
                true
            }
        }
    }

    /// Applies `op` in memory and hands it to the writer. A read-only store (a newer build's
    /// format) refuses it with a status line; returns whether the operation was applied.
    fn commit(&mut self, op: LayoutPresetOp) -> bool {
        if let Err(err) = self.presets.apply(&op) {
            runtime_log::log_warn(format!("[settings::layout_presets] refused a layout preset operation ({}); error={err}", op.describe()));
            self.status = Some(t!("settings.canvas_ribbon.preset_store_read_only_status").to_owned());
            return false;
        }
        self.queue_write(op);
        true
    }

    /// `settings.canvas_ribbon.preset_default_name` with the lowest 1-based index no preset
    /// carries. Bounded: `count + 1` candidates against `count` names, with a suffix fallback for
    /// a catalog entry that lost `{index}`.
    #[must_use]
    fn free_default_name(&self) -> String {
        let taken = self.presets.user_names().count() + BuiltinLayoutPreset::ALL.len();
        for index in 1..=taken + 1 {
            let candidate = tf!("settings.canvas_ribbon.preset_default_name", index = index);
            if !self.name_taken(&candidate) {
                return candidate;
            }
        }
        let base = tf!("settings.canvas_ribbon.preset_default_name", index = taken + 1);
        (2..=taken + 2)
            .map(|suffix| format!("{base} ({suffix})"))
            .find(|candidate| !self.name_taken(candidate))
            // Unreachable by the same bound: `taken + 1` candidates against `taken` names.
            .unwrap_or_else(|| format!("{base} ({})", taken + 2))
    }

    /// Hands `op` to the writer thread (started on first use). Without a store path the operation
    /// stays in memory. A writer that cannot start or has died is reported in the status line and
    /// the log.
    fn queue_write(&mut self, op: LayoutPresetOp) {
        let Some(path) = self.store_path.clone() else {
            return;
        };
        if self.writer.is_none() {
            match spawn_writer(path.clone(), self.outcomes_tx.clone()) {
                Ok(writer) => self.writer = Some(writer),
                Err(err) => {
                    runtime_log::log_error(format!("[settings::layout_presets] could not start the layout preset writer thread; op={} path={} error={err}", op.describe(), path.display()));
                    self.apply_write_outcome(WriteOutcome { entries: op_entries(&op), error: Some(err.to_string()) });
                    return;
                }
            }
        }
        let description = op.describe();
        let entries = op_entries(&op);
        let sent = self.writer.as_ref().and_then(|writer| writer.tx.as_ref()).is_some_and(|tx| tx.send(op).is_ok());
        if !sent {
            runtime_log::log_error(format!("[settings::layout_presets] the layout preset writer thread is gone; an operation was not saved; op={description} path={}", path.display()));
            self.apply_write_outcome(WriteOutcome { entries, error: Some(t!("settings.canvas_ribbon.preset_writer_lost_error").to_owned()) });
            // A fresh writer is started by the next operation.
            self.writer = None;
        }
    }

    /// Applies every answer the writer sent since the last frame.
    fn poll_write_outcomes(&mut self) {
        // The pane holds a sender itself, so the channel cannot disconnect while `self` lives; an
        // `Err` (empty) simply means "nothing more to read".
        while let Ok(outcome) = self.outcomes_rx.try_recv() {
            self.apply_write_outcome(outcome);
        }
    }

    /// The failed-write rule (file header): a failure marks the operation's entries failed and
    /// becomes the write-error line; a success clears its entries, and the line once none is left.
    fn apply_write_outcome(&mut self, outcome: WriteOutcome) {
        let WriteOutcome { entries, error } = outcome;
        match error {
            Some(err) => {
                self.failed.extend(entries);
                self.write_error = Some(tf!("settings.canvas_ribbon.preset_save_error_status", err = err));
            }
            None => {
                for entry in &entries {
                    self.failed.remove(entry);
                }
                if self.failed.is_empty() {
                    self.write_error = None;
                }
            }
        }
    }
}

/// Runs one operation against the document at `path` (blocking; the writer thread only),
/// logging the outcome.
///
/// # Errors
/// The technical message of [`apply_layout_preset_op`].
fn persist(path: &Path, op: &LayoutPresetOp) -> Result<(), String> {
    match apply_layout_preset_op(path, op) {
        Ok(()) => {
            runtime_log::log_info(format!("[settings::layout_presets] saved layout preset operation ({}) to {}", op.describe(), path.display()));
            Ok(())
        }
        Err(err) => {
            runtime_log::log_error(format!(
                "[settings::layout_presets] failed to save a layout preset operation; op={} path={} error={err} possible_cause=malformed or unwritable user_config, or presets of a newer build (the file is left untouched)",
                op.describe(),
                path.display()
            ));
            Err(err)
        }
    }
}

/// Starts the ordered writer over `path`; every operation is answered on `outcomes` (failures
/// are logged too).
///
/// # Errors
/// The OS error when the thread cannot be spawned.
fn spawn_writer(path: PathBuf, outcomes: Sender<WriteOutcome>) -> std::io::Result<PresetWriter> {
    let (tx, rx) = mpsc::channel::<LayoutPresetOp>();
    let handle = thread::Builder::new().name("layout-presets-writer".to_owned()).spawn(move || {
        while let Ok(op) = rx.recv() {
            let outcome = WriteOutcome { entries: op_entries(&op), error: persist(&path, &op).err() };
            // The pane outlives the writer (it joins it on drop), so a send can only fail during
            // that drop, when nobody would read the answer anyway.
            if outcomes.send(outcome).is_err() {
                runtime_log::log_warn("[settings::layout_presets] a layout preset write outcome had no reader (the pane is closing)");
            }
        }
    })?;
    Ok(PresetWriter { tx: Some(tx), handle: Some(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A pane over `store` (a `Canvas.layout_presets` JSON value), without persistence.
    fn pane(store: serde_json::Value, selection: &mut LayoutPresetSelection) -> LayoutPresetsPane {
        let mut pane = LayoutPresetsPane::default();
        pane.rebind(CanvasLayoutPresets::from_user_config(&json!({"Canvas": {"layout_presets": store}})), None, selection);
        pane
    }

    /// Settings whose layout is `layout`.
    fn settings_with(layout: &CanvasLayout) -> SharedCanvasSettings {
        let mut settings = SharedCanvasSettings::default();
        settings.apply_layout(layout);
        settings
    }

    fn user(name: &str) -> LayoutPresetSelection {
        LayoutPresetSelection::User(name.to_owned())
    }

    #[test]
    fn rows_list_builtins_then_sorted_user_presets_and_map_back() {
        let mut selection = user("b");
        let pane = pane(json!({"user": {"b": {}, "a": {}}}), &mut selection);
        let rows = pane.rows();
        assert_eq!(rows, vec![PresetRow::Builtin(BuiltinLayoutPreset::Pages), PresetRow::Builtin(BuiltinLayoutPreset::Webtoon), PresetRow::User("a".into()), PresetRow::User("b".into())]);
        assert_eq!(pane.row_of(&selection), Some(3));
        assert_eq!(pane.row_of(&LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon)), Some(1));
        assert_eq!(pane.row_of(&LayoutPresetSelection::None), None);
        assert_eq!(pane.picker.name(), "b", "the rename buffer follows the bound selection");
    }

    #[test]
    fn a_dangling_user_selection_reads_as_none() {
        let mut selection = user("gone");
        let _pane = pane(json!({"user": {"kept": {}}}), &mut selection);
        assert_eq!(selection, LayoutPresetSelection::None);
    }

    #[test]
    fn selecting_a_builtin_applies_its_override_and_editing_marks_unsaved() {
        let mut selection = LayoutPresetSelection::None;
        let mut pane = pane(json!({"builtin": {"webtoon": {"page_spacing_px": 9}}}), &mut selection);
        let mut settings = SharedCanvasSettings::default();
        assert!(pane.select(&PresetRow::Builtin(BuiltinLayoutPreset::Webtoon), &mut selection, &mut settings));
        assert_eq!(settings.layout(), CanvasLayout { page_spacing_px: 9, ..BuiltinLayoutPreset::Webtoon.factory() });
        assert!(!pane.is_unsaved(&selection, &settings));
        settings.aside_scale_pct = 150;
        assert!(pane.is_unsaved(&selection, &settings));
        assert!(!pane.is_unsaved(&LayoutPresetSelection::None, &settings), "«None» is never unsaved");
    }

    #[test]
    fn builtin_save_override_revert_and_delete_visibility() {
        let mut selection = LayoutPresetSelection::None;
        let mut pane = pane(json!({}), &mut selection);
        let preset = BuiltinLayoutPreset::Pages;
        let mut settings = SharedCanvasSettings::default();
        pane.select(&PresetRow::Builtin(preset), &mut selection, &mut settings);
        assert!(!pane.shows_delete(&selection), "an unmodified built-in cannot be deleted");
        assert!(!pane.delete_selected(&mut selection, &mut settings));

        settings.page_spacing = 33.0;
        pane.picker.set_name("ignored");
        assert!(!pane.save(&mut selection, &settings), "saving a built-in keeps its name");
        assert_eq!(pane.status, None);
        assert!(pane.presets.is_builtin_modified(preset));
        assert!(pane.shows_delete(&selection));
        assert_eq!(pane.picker.name(), preset.display_name(), "the buffer is re-synced");

        assert!(pane.delete_selected(&mut selection, &mut settings), "delete reverts and re-applies");
        assert_eq!(selection, LayoutPresetSelection::Builtin(preset));
        assert_eq!(settings.layout(), preset.factory());
        assert!(!pane.presets.is_builtin_modified(preset));

        // Saving the factory values on a modified built-in removes the override.
        settings.page_spacing = 44.0;
        pane.save(&mut selection, &settings);
        settings.apply_layout(&preset.factory());
        pane.save(&mut selection, &settings);
        assert!(!pane.presets.is_builtin_modified(preset));
    }

    #[test]
    fn user_presets_create_rename_and_delete() {
        let mut selection = LayoutPresetSelection::None;
        let mut pane = pane(json!({"user": {"other": {}}}), &mut selection);
        let mut settings = settings_with(&CanvasLayout { aside_scale_pct: 120, ..BuiltinLayoutPreset::Webtoon.factory() });

        assert!(pane.create(&mut selection, &settings));
        let LayoutPresetSelection::User(created) = selection.clone() else {
            panic!("the created preset is selected: {selection:?}");
        };
        assert!(created != "other" && !BuiltinLayoutPreset::ALL.iter().any(|preset| preset.display_name() == created));
        assert!(!pane.is_unsaved(&selection, &settings));
        pane.create(&mut selection, &settings);
        assert_ne!(selection, user(&created), "a second create gets a new name");

        for (taken, key) in [("other", "taken"), (BuiltinLayoutPreset::Pages.display_name(), "taken"), ("", "empty")] {
            pane.picker.set_name(taken);
            assert!(!pane.save(&mut selection, &settings));
            let expected = if key == "empty" { t!("settings.canvas_ribbon.preset_empty_name_status").to_owned() } else { tf!("settings.canvas_ribbon.preset_name_taken_status", name = taken) };
            assert_eq!(pane.status.as_deref(), Some(expected.as_str()));
        }

        pane.picker.set_name(" mine ");
        assert!(pane.save(&mut selection, &settings), "a rename changes the remembered selection");
        assert_eq!(selection, user(" mine "));
        assert_eq!(pane.status, None);

        settings.aside_scale_pct = 130;
        assert!(!pane.save(&mut selection, &settings), "saving values under the same name");
        assert!(!pane.is_unsaved(&selection, &settings));

        assert!(pane.delete_selected(&mut selection, &mut settings));
        assert_eq!(selection, LayoutPresetSelection::None);
        assert_eq!(settings.aside_scale_pct, 130, "deleting keeps the values");
        assert!(!pane.presets.contains_user(" mine "));
    }

    #[test]
    fn a_read_only_store_refuses_every_operation() {
        let mut selection = LayoutPresetSelection::None;
        let mut pane = pane(json!({"version": 99}), &mut selection);
        let settings = SharedCanvasSettings::default();
        assert!(!pane.create(&mut selection, &settings));
        assert_eq!(selection, LayoutPresetSelection::None);
        assert_eq!(pane.status.as_deref(), Some(t!("settings.canvas_ribbon.preset_store_read_only_status")));
    }

    /// A failed write keeps the entry unsaved and the error line up across selection changes, until
    /// a later write of the same entry succeeds.
    #[test]
    fn a_failed_write_stays_unsaved_until_a_later_success() {
        let mut selection = LayoutPresetSelection::None;
        let mut pane = pane(json!({"user": {"a": {}, "b": {}}}), &mut selection);
        let mut settings = SharedCanvasSettings::default();
        pane.select(&PresetRow::User("a".into()), &mut selection, &mut settings);
        assert!(!pane.is_unsaved(&selection, &settings));

        pane.apply_write_outcome(WriteOutcome { entries: vec![PresetRow::User("a".into())], error: Some("disk full".into()) });
        assert!(pane.is_unsaved(&selection, &settings), "the failed entry reads as unsaved");
        pane.select(&PresetRow::User("b".into()), &mut selection, &mut settings);
        assert!(!pane.is_unsaved(&selection, &settings), "another entry is not affected");
        assert!(pane.write_error.is_some() && !pane.failed.is_empty(), "the error survives a selection change");

        pane.apply_write_outcome(WriteOutcome { entries: vec![PresetRow::User("b".into())], error: None });
        assert!(pane.write_error.is_some(), "a success of ANOTHER entry clears nothing");
        pane.apply_write_outcome(WriteOutcome { entries: vec![PresetRow::User("a".into()), PresetRow::User("c".into())], error: None });
        assert!(pane.failed.is_empty() && pane.write_error.is_none());
    }

    /// End to end: a write into a malformed `user_config` fails and marks the created preset.
    #[test]
    fn a_real_failed_write_is_reported_by_the_writer() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, "{ not json").map_err(|err| err.to_string())?;
        let mut selection = LayoutPresetSelection::None;
        let mut pane = LayoutPresetsPane::default();
        pane.rebind(CanvasLayoutPresets::default(), Some(path.clone()), &mut selection);
        let settings = SharedCanvasSettings::default();
        assert!(pane.create(&mut selection, &settings));
        let outcome = pane.outcomes_rx.recv_timeout(std::time::Duration::from_secs(30)).map_err(|err| err.to_string())?;
        assert!(outcome.error.is_some(), "{outcome:?}");
        pane.apply_write_outcome(outcome);
        assert!(pane.is_unsaved(&selection, &settings));
        assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, "{ not json", "the malformed file is untouched");
        Ok(())
    }

    /// The writer persists every operation in order to its own (temporary) file.
    #[test]
    fn the_writer_persists_operations_in_order() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, r#"{"General":{"ui_language":"ru"}}"#).map_err(|err| err.to_string())?;
        let mut selection = LayoutPresetSelection::None;
        let first;
        {
            let mut pane = LayoutPresetsPane::default();
            pane.rebind(CanvasLayoutPresets::default(), Some(path.clone()), &mut selection);
            let mut settings = SharedCanvasSettings::default();
            pane.create(&mut selection, &settings);
            first = selection.clone();
            settings.aside_scale_pct = 200;
            pane.save(&mut selection, &settings);
            pane.create(&mut selection, &settings);
            pane.delete_selected(&mut selection, &mut settings);
            // Dropping joins the writer, so every queued operation is on disk afterwards.
        }
        let raw = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let root: serde_json::Value = serde_json::from_str(&raw).map_err(|err| err.to_string())?;
        assert_eq!(root["General"]["ui_language"], "ru");
        let stored = CanvasLayoutPresets::from_user_config(&root);
        assert_eq!(stored.user_names().count(), 1, "the second preset was deleted after it was created");
        assert_eq!(stored.effective(&first).map(|layout| layout.aside_scale_pct), Some(200));
        Ok(())
    }
}
