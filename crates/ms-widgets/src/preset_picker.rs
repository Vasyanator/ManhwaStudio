/*
FILE HEADER (crates/ms-widgets/src/preset_picker.rs)

Purpose:
A named-preset picker block: row 1 is a `WheelComboBox` listing «None», an optional
«+ Create preset» action row and every preset by name; row 2 is the rename field of the
SELECTED preset with a «Save» button and a two-step red «Delete» button; under them an
unsaved-changes warning line.

Key structures:
- `PresetPickerState`: the caller-held UI state (rename buffer, armed delete).
- `PresetPicker`: the per-frame builder (names, selection, unsaved flag, optional rows,
  whether the name may be edited).
- `PresetPickerResponse` / `PresetPickerAction`: what the user asked for this frame.

Key functions:
- `PresetPicker::show()`: draws the block and reports at most one action.
- `preset_wheel_target()` (public: the typing tab's image-row local-preset combo shares it),
  `popup_id_bucket()`: the pure decisions the drawing uses. The two-click «Delete» is
  `ArmedDeleteButton` (`armed_delete_button.rs`).

Notes:
- DRAW ONLY. The caller owns the preset list, every decision (what create / select / save /
  delete means), name validation and status messages. The widget never mutates the caller's
  selection: it reports an action and the caller applies it.
- The rename buffer is NOT re-synced at draw time. The caller calls
  `PresetPickerState::reset_to` at every selection transition (apply, create, save, deselect,
  delete), because the block may sit in a collapsed section and the caller may ask
  `has_pending_rename` on frames where `show` is not reached.
- `rename(false)` keeps the name field visible but disabled, showing the selection's name
  (not the buffer); the widget then never writes the buffer and the warning line ignores it,
  while «Save» stays available and means "commit the values" to the caller.
- Texts are `widgets.preset_picker.*` except the combo label, which the caller hands in.
*/

use crate::armed_delete_button::{ArmedDeleteButton, ArmedDeleteStep};
use crate::wheel_input_guard::cycle_wrapped_index;
use crate::WheelComboBox;
use eframe::egui;
use egui::{Id, Response, Ui, WidgetText};

/// Width, in points, the name row keeps for the save and delete buttons that follow the rename
/// field. Everything else on the row goes to the field itself.
///
/// A reserve rather than a measurement: egui 0.36 sizes a `Button` through its style-derived
/// frame margins (`egui-0.36.2/src/widgets/button.rs:333`), which a pre-layout measurement would
/// have to replicate. The reserve covers the widest caption pair of the shipped locales, the
/// armed «Delete?» wording included, so the field never jumps when the button arms.
const NAME_ROW_BUTTON_RESERVE_PT: f32 = 190.0;

/// Narrowest the rename field gets, in points, however little room the row has.
const NAME_FIELD_MIN_WIDTH_PT: f32 = 120.0;

/// Caller-held UI state of one [`PresetPicker`]: the rename buffer of the selected preset and
/// whether its delete button is armed.
///
/// The buffer is bound to the SELECTION, not a "name the next preset" box: it is meaningless
/// (and its field disabled) while nothing is selected. Keep one value per picker and call
/// [`Self::reset_to`] whenever the selection changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetPickerState {
    /// Rename buffer, edited by the field; taken verbatim (never trimmed).
    name: String,
    /// The first click on «Delete» happened; the next plain click confirms.
    delete_armed: bool,
}

impl PresetPickerState {
    /// Re-syncs the state with a new selection: the buffer becomes the selected preset's name
    /// (empty for none) and the delete button is disarmed.
    pub fn reset_to(&mut self, selected: Option<&str>) {
        self.name.clear();
        if let Some(name) = selected {
            self.name.push_str(name);
        }
        self.delete_armed = false;
    }

    /// The rename buffer as typed, verbatim.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Replaces the rename buffer exactly as typing it into the field would; the armed delete
    /// is left alone.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Whether the buffer names something other than `selected`, byte for byte. Always `false`
    /// with no selection (there is nothing to rename then).
    #[must_use]
    pub fn has_pending_rename(&self, selected: Option<&str>) -> bool {
        selected.is_some_and(|name| self.name != name)
    }

    /// Whether the delete button is armed (the next plain click confirms).
    #[must_use]
    pub fn is_delete_armed(&self) -> bool {
        self.delete_armed
    }
}

/// What the user asked for in one frame. Applying it is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetPickerAction {
    /// The «None» row (or the wheel landing on it) while a preset is selected.
    Deselect,
    /// The «+ Create preset» row. Never produced by the wheel.
    Create,
    /// A preset row (or the wheel landing on it), as an index into the names the picker was
    /// given; never the index already selected.
    Select(usize),
    /// «Save» with a preset selected. The new name is [`PresetPickerState::name`], verbatim;
    /// validating it (empty, taken) is the caller's job. A picker built with `rename(false)`
    /// offers no rename, so the caller then commits the values under the selection's own name.
    Save,
    /// The CONFIRMING second click on the armed «Delete», carrying the selected index. The
    /// widget leaves the arm as it is; the caller's deselect resets it via `reset_to`.
    Delete(usize),
}

/// Result of [`PresetPicker::show`].
#[derive(Debug)]
pub struct PresetPickerResponse {
    /// At most one action per frame, already filtered: a row that IS the current state
    /// (re-selecting the selection, «None» with none selected) is dropped.
    pub action: Option<PresetPickerAction>,
    /// The combo button's response (hover text, focus, rect).
    pub combo: Response,
    /// The rename field's response (disabled while nothing is selected or with `rename(false)`).
    pub name_field: Response,
    /// The «Save» button's response.
    pub save: Response,
    /// The «Delete» button's response; `None` when the picker was built with `delete(false)`.
    pub delete: Option<Response>,
}

/// The preset-picker block, built per frame. See the file header for the layout and the
/// ownership split.
#[must_use = "a PresetPicker draws nothing until `show` is called"]
pub struct PresetPicker<'a> {
    id: Id,
    combo_label: WidgetText,
    names: &'a [String],
    selected: Option<usize>,
    unsaved: bool,
    create_row: bool,
    delete: bool,
    rename: bool,
}

impl<'a> PresetPicker<'a> {
    /// A picker over `names` (drawn in the given order) with `selected` an index into them
    /// (`None` = «None»; an out-of-range index reads as `None`). `id_salt` must be stable and
    /// language-independent (the i18n key of the label is the usual choice); `combo_label` is
    /// the visible text beside the combo, already localized.
    pub fn new(
        id_salt: impl std::hash::Hash + std::fmt::Debug,
        combo_label: impl Into<WidgetText>,
        names: &'a [String],
        selected: Option<usize>,
    ) -> Self {
        Self {
            id: Id::new(id_salt),
            combo_label: combo_label.into(),
            names,
            selected: selected.filter(|&idx| idx < names.len()),
            unsaved: false,
            create_row: true,
            delete: true,
            rename: true,
        }
    }

    /// Whether the selected preset carries changes the caller has not persisted. The warning
    /// line shows when this OR a pending rename holds, and only with a selection.
    #[inline]
    pub fn unsaved(mut self, unsaved: bool) -> Self {
        self.unsaved = unsaved;
        self
    }

    /// Whether the popup offers the «+ Create preset» row (default `true`).
    #[inline]
    pub fn create_row(mut self, create_row: bool) -> Self {
        self.create_row = create_row;
        self
    }

    /// Whether the name row carries the two-step «Delete» button (default `true`).
    #[inline]
    pub fn delete(mut self, delete: bool) -> Self {
        self.delete = delete;
        self
    }

    /// Whether the selected preset's name is editable (default `true`). With `false` the name
    /// field stays visible, disabled, showing the selection's name; the widget never writes
    /// [`PresetPickerState::name`] and a pending rename does not raise the warning line. «Save»
    /// stays enabled with a selection: it then commits values only.
    #[inline]
    pub fn rename(mut self, rename: bool) -> Self {
        self.rename = rename;
        self
    }

    /// Draws the block and reports what the user asked for.
    ///
    /// The rename field, «Save» and «Delete» are disabled while nothing is selected; the rename
    /// field also with `rename(false)`.
    pub fn show(self, ui: &mut Ui, state: &mut PresetPickerState) -> PresetPickerResponse {
        let selected_name = self.selected.and_then(|idx| self.names.get(idx)).map(String::as_str);
        let (combo, combo_action) = self.draw_combo(ui, selected_name);
        let row = self.draw_name_row(ui, state, selected_name);
        if shows_unsaved_warning(selected_name, self.unsaved, self.rename, state) {
            ui.label(
                egui::RichText::new(t!("widgets.preset_picker.unsaved_warning"))
                    .small()
                    .color(ms_theme::status::WARNING),
            );
        }
        PresetPickerResponse {
            action: combo_action.or(row.action),
            combo,
            name_field: row.name_field,
            save: row.save,
            delete: row.delete,
        }
    }

    /// Row 1: the combo. THE WHEEL NEVER CREATES: wheel steps go through [`preset_wheel_target`],
    /// which cycles «None» plus the presets only, so a stray scroll over the closed combo can
    /// never mint a preset.
    fn draw_combo(&self, ui: &mut Ui, selected_name: Option<&str>) -> (Response, Option<PresetPickerAction>) {
        let none_label = t!("widgets.preset_picker.none_option");
        let mut action: Option<PresetPickerAction> = None;
        ui.horizontal(|ui| {
            // The row-count bucket is part of the id: a popup `Area` remembers its size under a
            // fixed id and can only ever shrink, and this list grows in place. See
            // `popup_id_bucket`.
            let row_count = self.names.len() + 1 + usize::from(self.create_row);
            let bucket = popup_id_bucket(ui, row_count);
            let combo = WheelComboBox::from_label(self.combo_label.clone())
                .id_salt((self.id, "combo", bucket))
                .selected_text(selected_name.unwrap_or(none_label))
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(self.selected.is_none(), none_label).clicked() {
                        action = Some(PresetPickerAction::Deselect);
                    }
                    // Never drawn as "selected": creating leaves the NEW preset selected, and
                    // this row is an action, not a state.
                    if self.create_row
                        && ui.selectable_label(false, t!("widgets.preset_picker.create_option")).clicked()
                    {
                        action = Some(PresetPickerAction::Create);
                    }
                    for (idx, name) in self.names.iter().enumerate() {
                        if ui.selectable_label(self.selected == Some(idx), name).clicked() {
                            action = Some(PresetPickerAction::Select(idx));
                        }
                    }
                });
            // Wheel steps are only reported while the popup is CLOSED, so they can never race
            // with a click collected above.
            if let Some(steps) = combo.wheel_steps
                && let Some(target) = preset_wheel_target(self.selected, self.names.len(), steps)
            {
                action = Some(target.map_or(PresetPickerAction::Deselect, PresetPickerAction::Select));
            }
            if action.is_some_and(|requested| action_changes_nothing(self.selected, requested)) {
                action = None;
            }
            (combo.inner.response, action)
        })
        .inner
    }

    /// Row 2: the rename field of the selection plus «Save» and the optional «Delete». All
    /// disabled without a selection: there is nothing to rename, commit or delete then. With
    /// `rename(false)` the field is disabled too and shows `selected_name` instead of the buffer.
    fn draw_name_row(&self, ui: &mut Ui, state: &mut PresetPickerState, selected_name: Option<&str>) -> NameRow {
        ui.horizontal(|ui| {
            let has_selection = self.selected.is_some();
            let field_width = (ui.available_width() - NAME_ROW_BUTTON_RESERVE_PT).max(NAME_FIELD_MIN_WIDTH_PT);
            let name_field = if self.rename {
                ui.add_enabled(has_selection, self.name_edit(&mut state.name, field_width))
            } else {
                // An immutable `&str` buffer (egui's `TextBuffer for &str` is read-only): the
                // field looks like the editable one, disabled, and can never touch the state's
                // buffer, so `has_pending_rename` keeps whatever `reset_to` set.
                let mut shown: &str = selected_name.unwrap_or_default();
                ui.add_enabled(false, self.name_edit(&mut shown, field_width))
            };
            let save = ui.add_enabled(has_selection, egui::Button::new(t!("widgets.preset_picker.save_button")));
            let mut action = save.clicked().then_some(PresetPickerAction::Save);
            let delete = self.delete.then(|| {
                let (response, confirmed) = draw_delete_control(ui, state, has_selection);
                if confirmed && let Some(idx) = self.selected {
                    action = Some(PresetPickerAction::Delete(idx));
                }
                response
            });
            NameRow { action, name_field, save, delete }
        })
        .inner
    }

    /// The name field widget over `text`, shared by the editable and the read-only form so
    /// both keep one id, hint and width.
    fn name_edit<'t>(&self, text: &'t mut dyn egui::TextBuffer, field_width: f32) -> egui::TextEdit<'t> {
        egui::TextEdit::singleline(text)
            .id_salt((self.id, "name"))
            .hint_text(t!("widgets.preset_picker.name_hint"))
            .desired_width(field_width)
    }
}

/// The name row's responses and the action it produced.
struct NameRow {
    action: Option<PresetPickerAction>,
    name_field: Response,
    save: Response,
    delete: Option<Response>,
}

/// Whether the unsaved-changes line shows: only with a selection, when the caller flags unsaved
/// values or the buffer holds a pending rename. With renaming off (`rename == false`) the buffer
/// is not the user's input — the field never edits it — so only the caller's flag counts.
#[must_use]
fn shows_unsaved_warning(selected_name: Option<&str>, unsaved: bool, rename: bool, state: &PresetPickerState) -> bool {
    selected_name.is_some() && (unsaved || (rename && state.has_pending_rename(selected_name)))
}

/// The two-step delete button ([`ArmedDeleteButton`], red at rest and more strongly so once
/// armed) over the state's single arm. Returns the button response and whether this frame
/// confirmed; the confirm leaves the arm as it is (the caller's deselect resets it).
fn draw_delete_control(ui: &mut Ui, state: &mut PresetPickerState, enabled: bool) -> (Response, bool) {
    let (response, step) = ArmedDeleteButton::new(t!("widgets.preset_picker.delete_button"), t!("widgets.preset_picker.delete_confirm_button"))
        .idle_fill(ms_theme::status::DESTRUCTIVE_IDLE_FILL)
        .enabled(enabled)
        .show(ui, state.delete_armed);
    match step {
        ArmedDeleteStep::Arm => state.delete_armed = true,
        ArmedDeleteStep::Disarm => state.delete_armed = false,
        ArmedDeleteStep::Confirm | ArmedDeleteStep::Keep => {}
    }
    (response, step == ArmedDeleteStep::Confirm)
}

/// Where a wheel notch over the CLOSED combo moves the selection.
///
/// The wheel cycles a VIRTUAL list — «None» (`0`) plus the presets (`1 + position`). The
/// popup's create row is deliberately NOT in it, so no amount of scrolling can create a preset.
///
/// `selected` is the selection's position among the names (`None` for «None») and `count`
/// their number. Returns `None` when the selection does not move (no steps, or a wrap that
/// lands back on the current row); otherwise `Some(position)`, whose inner `None` is «None».
///
/// Public because a preset combo that cannot be a `PresetPicker` (the typing tab's local
/// presets draw image rows) must still cycle exactly like one.
#[must_use]
pub fn preset_wheel_target(selected: Option<usize>, count: usize, steps: i32) -> Option<Option<usize>> {
    let index = selected.map_or(0, |pos| pos + 1);
    let next = cycle_wrapped_index(index, count + 1, steps);
    (next != index).then(|| next.checked_sub(1))
}

/// Whether `action` would change nothing — the row that already IS the selection. «None» with
/// nothing selected and re-selecting the selected preset are no-ops; create, save and delete
/// never are.
#[must_use]
fn action_changes_nothing(selected: Option<usize>, action: PresetPickerAction) -> bool {
    match action {
        PresetPickerAction::Deselect => selected.is_none(),
        PresetPickerAction::Select(idx) => selected == Some(idx),
        PresetPickerAction::Create | PresetPickerAction::Save | PresetPickerAction::Delete(_) => false,
    }
}

/// The row-count bucket that MUST be folded into the combo's `id_salt`.
///
/// THE POPUP AREA REMEMBERS ITS SIZE AND CAN ONLY SHRINK: a combo popup is an `Area`, its
/// stored size is fed back as the body's `max_rect` every frame
/// (`egui-0.36.2/src/containers/area.rs:612` + `:668`), and a sizing pass runs only for an id
/// with NO stored state. Under a constant id the popup's height is therefore non-increasing for
/// the whole session, so a popup first opened with one preset in it would clip every preset
/// created afterwards — and this list grows in place (create row, rename).
///
/// `row_count` is the TOTAL number of rows drawn. The bucket is capped because the popup's
/// height is capped: its body is a `ScrollArea` limited to `Spacing::combo_height` when the
/// combo sets no height of its own, so every count past the one that fills that cap measures
/// identically. The cap is derived from the LIVE style, so a theme change cannot make it wrong.
#[must_use]
fn popup_id_bucket(ui: &Ui, row_count: usize) -> usize {
    let spacing = ui.spacing();
    // LOWER bound on one row's vertical step: a `selectable_label` is a `Button`, and no button
    // is shorter than `interact_size.y`. A lower bound on the PITCH gives an UPPER bound on the
    // number of rows that can still grow the popup, which is the safe direction — a bucket that
    // changes too often costs one unused stored `Area`, one that changes too rarely brings the
    // pinned-height defect back.
    let row_pitch_pt = (spacing.interact_size.y + spacing.item_spacing.y).max(1.0);
    // Clamped before the conversion, so it cannot truncate or lose a sign (CLAUDE.md §17
    // allows `as` where the conversion is proven safe): 64 rows is far past
    // what any style can fit into `combo_height`, and a degenerate style simply gets the widest
    // bucket.
    let max_rows = (spacing.combo_height / row_pitch_pt).ceil().clamp(1.0, 64.0) as usize;
    row_count.clamp(1, max_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE contract of the wheel: no step ever reaches a "create" outcome, names a preset that
    /// does not exist, or reports the row that is already selected.
    #[test]
    fn wheel_never_leaves_the_virtual_list() {
        for count in 0..5usize {
            for steps in -9..=9i32 {
                for selected in std::iter::once(None).chain((0..count).map(Some)) {
                    let Some(target) = preset_wheel_target(selected, count, steps) else {
                        continue;
                    };
                    if let Some(index) = target {
                        assert!(index < count, "count={count} steps={steps} index={index}");
                    }
                    assert_ne!(target, selected, "count={count} steps={steps}");
                }
            }
        }
    }

    #[test]
    fn wheel_cycles_none_and_presets_only() {
        assert_eq!(preset_wheel_target(None, 2, 1), Some(Some(0)));
        assert_eq!(preset_wheel_target(Some(0), 2, 1), Some(Some(1)));
        // Past the last preset the wheel wraps to «None», never onto a create row.
        assert_eq!(preset_wheel_target(Some(1), 2, 1), Some(None));
        assert_eq!(preset_wheel_target(None, 2, -1), Some(Some(1)));
        // Only «None» exists, or no step: nothing moves.
        assert_eq!(preset_wheel_target(None, 0, 1), None);
        assert_eq!(preset_wheel_target(Some(0), 2, 0), None);
        // A full lap lands back on the current row.
        assert_eq!(preset_wheel_target(Some(0), 2, 3), None);
    }

    #[test]
    fn rows_that_are_the_current_state_change_nothing() {
        assert!(action_changes_nothing(None, PresetPickerAction::Deselect));
        assert!(!action_changes_nothing(Some(0), PresetPickerAction::Deselect));
        assert!(action_changes_nothing(Some(1), PresetPickerAction::Select(1)));
        assert!(!action_changes_nothing(Some(1), PresetPickerAction::Select(0)));
        assert!(!action_changes_nothing(None, PresetPickerAction::Select(0)));
        assert!(!action_changes_nothing(None, PresetPickerAction::Create));
        assert!(!action_changes_nothing(Some(0), PresetPickerAction::Create));
        assert!(!action_changes_nothing(Some(0), PresetPickerAction::Save));
        assert!(!action_changes_nothing(Some(0), PresetPickerAction::Delete(0)));
    }

    #[test]
    fn state_reset_and_pending_rename_are_verbatim() {
        let mut state = PresetPickerState::default();
        assert!(!state.has_pending_rename(None));
        state.reset_to(Some(" Рао-кун "));
        assert_eq!(state.name(), " Рао-кун ");
        assert!(!state.has_pending_rename(Some(" Рао-кун ")));
        // Whitespace is part of the name: trimming it is a rename.
        state.set_name("Рао-кун");
        assert!(state.has_pending_rename(Some(" Рао-кун ")));
        // No selection: nothing can be pending, whatever the buffer holds.
        assert!(!state.has_pending_rename(None));
        state.delete_armed = true;
        state.reset_to(None);
        assert_eq!(state.name(), "");
        assert!(!state.is_delete_armed());
    }

    #[test]
    fn popup_bucket_grows_with_rows_up_to_the_style_cap() {
        let ctx = egui::Context::default();
        let mut buckets: Vec<usize> = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            buckets = [0, 1, 2, 3, 10_000].iter().map(|&rows| popup_id_bucket(ui, rows)).collect();
        })
        .drop_without_applying_deltas();
        assert_eq!(buckets.len(), 5, "every count must be measured");
        assert_eq!(buckets[0], 1, "an empty list still has one bucket");
        assert!(buckets[1] < buckets[2] && buckets[2] < buckets[3], "small lists get their own buckets: {buckets:?}");
        assert!(buckets[4] <= 64, "the bucket is capped: {buckets:?}");
    }

    #[test]
    fn nothing_selected_disables_the_name_row() {
        let ctx = egui::Context::default();
        let names = vec!["A".to_string(), "B".to_string()];
        let mut state = PresetPickerState::default();
        let mut enabled: Vec<(bool, bool, Option<bool>)> = Vec::new();
        for selected in [None, Some(1)] {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let parts = PresetPicker::new("test_picker", "Current", &names, selected).show(ui, &mut state);
                assert_eq!(parts.action, None, "no input, no action");
                enabled.push((parts.name_field.enabled(), parts.save.enabled(), parts.delete.map(|d| d.enabled())));
            })
            .drop_without_applying_deltas();
        }
        assert_eq!(enabled, vec![(false, false, Some(false)), (true, true, Some(true))]);
    }

    #[test]
    fn rename_false_disables_only_the_name_field() {
        let ctx = egui::Context::default();
        let names = vec!["A".to_string(), "B".to_string()];
        let mut state = PresetPickerState::default();
        state.reset_to(Some("B"));
        // A stale buffer the caller left behind must not leak into a rename-less picker.
        state.set_name("stale");
        let mut enabled: Vec<(bool, bool, Option<bool>)> = Vec::new();
        for selected in [None, Some(1)] {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let parts =
                    PresetPicker::new("test_picker", "Current", &names, selected).rename(false).show(ui, &mut state);
                assert_eq!(parts.action, None, "no input, no action");
                enabled.push((parts.name_field.enabled(), parts.save.enabled(), parts.delete.map(|d| d.enabled())));
            })
            .drop_without_applying_deltas();
        }
        assert_eq!(enabled, vec![(false, false, Some(false)), (false, true, Some(true))]);
        assert_eq!(state.name(), "stale", "the widget never writes the buffer with renaming off");
    }

    #[test]
    fn unsaved_warning_ignores_the_buffer_without_rename() {
        let mut state = PresetPickerState::default();
        state.reset_to(Some("A"));
        assert!(!shows_unsaved_warning(Some("A"), false, true, &state));
        assert!(shows_unsaved_warning(Some("A"), true, false, &state));
        state.set_name("B");
        assert!(shows_unsaved_warning(Some("A"), false, true, &state), "a pending rename warns");
        assert!(!shows_unsaved_warning(Some("A"), false, false, &state), "no rename, no pending rename");
        // Nothing selected: never a warning.
        assert!(!shows_unsaved_warning(None, true, true, &state));
    }

    #[test]
    fn the_delete_button_can_be_left_out() {
        let ctx = egui::Context::default();
        let names = vec!["A".to_string()];
        let mut state = PresetPickerState::default();
        let mut has_delete = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let parts = PresetPicker::new("test_picker", "Current", &names, Some(0))
                .create_row(false)
                .delete(false)
                .show(ui, &mut state);
            has_delete = Some(parts.delete.is_some());
        })
        .drop_without_applying_deltas();
        assert_eq!(has_delete, Some(false));
    }
}
