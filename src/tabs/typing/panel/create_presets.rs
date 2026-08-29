/*
File: panel/create_presets.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
create-panel preset and formula-preset apply/save UI, the shared typing font
combo, the initial preview request, and the face-index clamp.

Main responsibilities:
- draw and apply/create/rename/save/delete named create presets, and apply/save
  formula-layout presets;
- own the ONE font combo both typing panels draw (`draw_font_combo`): its rows,
  its own-typeface previews, its caption, its display clamp and its pick edge;
- issue the initial preview render request and clamp the selected face index.

It is also the ONE place that maps font diagnostics to colors and wording: the
STATIC per-font coverage classification (`font_coverage.rs`, the combo rows'
`primary_color` + `font_coverage_tooltip`) and the FACTUAL per-render fallback
report the renderer returns (`font_fallback_status_lines`, next to the preview).

Key items of the font combo:
- FontComboSpec / FontComboOutcome: what a panel lends the combo and what it gets
  back (the shown font index, the genuine user pick, the button response).
- FontComboRow (private): one owned row, built before the widget runs so the
  panel borrow ends before `SearchableComboBox` takes `&mut Ui`.
- font_combo_selected_position / font_combo_button_width: the pure display clamp
  and the button width the widget needs (its width is exact, unlike ComboBox's).

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so the `panel`
module root and its sibling submodules can call them. `use super::*;` pulls in
the parent module's types and imports.
*/

use super::*;
use crate::widgets::{RowLayout, SearchableComboBox, SearchableComboItem};

/// Maximum characters listed in one user-facing character list before it is
/// truncated with a "+N more" suffix. Shared by the static coverage tooltip and
/// the per-render fallback status so a long text can never blow up the panel.
const MAX_SHOWN_CHARS: usize = 15;

/// "Works, but not the way you asked": a font that only partially covers the
/// typesetting language, or a character drawn by a fallback font instead of the
/// selected one. Deliberately not red — both cases still render.
pub(super) const FONT_DIAGNOSTIC_WARNING_COLOR: egui::Color32 =
    egui::Color32::from_rgb(240, 200, 60);

/// "This will not be readable": a font that lacks the writing system entirely, or
/// a character no font in the render base could draw (tofu).
pub(super) const FONT_DIAGNOSTIC_ERROR_COLOR: egui::Color32 =
    egui::Color32::from_rgb(230, 96, 92);

/// Fill of the global-preset «Удалить» button AT REST.
///
/// The button is red before it is armed too: deleting a preset is destructive whether or not
/// the confirm step has been reached, and a neutral button that only turns red on the second
/// click hides that from the first one. Muted, so the ARMED state still reads as an escalation.
const PRESET_DELETE_IDLE_COLOR: egui::Color32 = egui::Color32::from_rgb(105, 38, 38);

/// Fill of the global-preset «Удалить» button once ARMED — the same saturated red the font-group
/// delete control confirms in (`settings::typesetting::font_groups::draw_delete_control`).
const PRESET_DELETE_ARMED_COLOR: egui::Color32 = egui::Color32::from_rgb(150, 40, 40);

/// Width, in points, the preset name row keeps for the save and delete buttons that follow the
/// rename field. Everything else on the row goes to the field itself.
const PRESET_NAME_ROW_BUTTON_RESERVE_PT: f32 = 190.0;

/// What a row of the global-preset combo asked for, collected inside the popup closure and
/// applied after it (each arm needs `&mut self`, which the closure still borrows).
#[derive(Debug)]
enum GlobalPresetRowAction {
    /// The «Нет» row: drop the selection and give the live local set back to the parked
    /// document default.
    Deselect,
    /// The «create» row: a new preset inheriting everything on screen.
    Create,
    /// A preset row: apply the preset with that name.
    Select(String),
}

/// Where a wheel notch over the CLOSED global-preset combo moves the selection.
///
/// The wheel cycles a VIRTUAL list — «Нет» (`0`) plus the real presets (`1 + position`). The
/// popup's «create» row is deliberately NOT in that list, so no amount of scrolling can create
/// a preset; creation stays an explicit click.
///
/// `selected` is the selection's position among the sorted preset names (`None` for «Нет») and
/// `count` their number. Returns `None` when the selection does not move (no steps, or a wrap
/// that lands back on the current row); otherwise `Some(position)`, whose inner `None` is «Нет».
#[must_use]
pub(super) fn global_preset_wheel_target(
    selected: Option<usize>,
    count: usize,
    steps: i32,
) -> Option<Option<usize>> {
    let mut index = selected.map_or(0, |pos| pos + 1);
    if !cycle_wrapped_index(&mut index, count + 1, steps) {
        return None;
    }
    Some(index.checked_sub(1))
}

/// Rows the global-preset popup always draws, whatever the preset count: «Нет» and «создать».
const GLOBAL_PRESET_POPUP_TEXT_ROWS: usize = 2;

/// The row-count bucket that MUST be folded into the global-preset combo's `id_salt`.
///
/// THE POPUP AREA REMEMBERS ITS SIZE AND CAN ONLY SHRINK — the same defect
/// `create_main_text::local_preset_popup_id_bucket` guards, and the same reasoning: a combo
/// popup is an `Area`, its stored size is fed back as the body's `max_rect` every frame
/// (`egui-0.35.0/src/area.rs:610-611`) and rewritten from the content's own `min_size`
/// (`area.rs:665`), and a sizing pass runs only for an id with NO stored state
/// (`area.rs:466`); stored areas are never pruned (`memory/mod.rs:1157`). Under a CONSTANT id
/// the popup's height is therefore monotonically non-increasing for the whole session, so a
/// popup first opened with one preset in it would clip every preset created afterwards —
/// and this list grows in place, both through the «create» row and through a rename.
///
/// `row_count` is the TOTAL number of rows drawn («Нет» + the «create» row + the presets).
/// The bucket is capped, because the popup's own height is capped: the body is a `ScrollArea`
/// whose `max_height` is `Spacing::combo_height` when the combo sets no height of its own
/// (`egui-0.35.0/src/containers/combo_box.rs:393`), so every count past the one that fills
/// that cap measures identically and needs no id of its own. The cap is derived from the LIVE
/// style rather than from a pixel or row constant, so a theme change cannot silently make it
/// wrong.
#[must_use]
fn global_preset_popup_id_bucket(ui: &egui::Ui, row_count: usize) -> usize {
    let spacing = ui.spacing();
    // LOWER bound on one row's vertical step: a `selectable_label` is a `Button`, and no
    // button is shorter than `interact_size.y`. A lower bound on the PITCH gives an UPPER
    // bound on the number of rows that can still grow the popup, which is the safe
    // direction — a bucket that changes too often costs one unused stored `Area`, one that
    // changes too rarely brings the pinned-height defect back.
    let row_pitch_pt = (spacing.interact_size.y + spacing.item_spacing.y).max(1.0);
    // Clamped before the conversion, so it cannot truncate or lose a sign (CLAUDE.md §17
    // allows `as` where the conversion is proven safe): 64 rows is far past what any style
    // can fit into `combo_height`, and a degenerate style simply gets the widest bucket.
    let max_rows = (spacing.combo_height / row_pitch_pt).ceil().clamp(1.0, 64.0) as usize;
    row_count.clamp(1, max_rows)
}

/// Point size of the font combo's row previews, and of its own-typeface closed caption.
///
/// Pinned instead of inherited from `SearchableComboBox`'s default so the rows keep exactly
/// the size the hand-drawn options used before the widget swap (a 14 pt
/// `Style::override_font_id` per row).
const FONT_COMBO_PREVIEW_SIZE_PT: f32 = 14.0;

/// Cap on the height of the font combo's drop-down list, in points.
///
/// Larger than the 200 pt `Spacing::combo_height` the old popup was bounded by
/// (`egui-0.35.0/src/style.rs:1466`): one `RowLayout::Wide` row is a single text line, so the
/// taller list is what makes a font catalog browsable without scrolling.
const FONT_COMBO_MAX_POPUP_HEIGHT_PT: f32 = 320.0;

/// Width, in points, the edit panel keeps on the font combo's row for the face combo that
/// follows the combo on the same row.
///
/// [`FONT_COMBO_MIN_WIDTH_PT`] for the face button itself plus room for its label and the
/// spacing between the two — the font combo may take everything else. The create panel needs
/// no such reserve: its face combo is a row of its own.
pub(super) const FONT_COMBO_FACE_ROW_RESERVE_PT: f32 = FONT_COMBO_MIN_WIDTH_PT + 50.0;

/// Smallest width, in points, the font combo's BUTTON is ever given — the square search
/// button that follows it is budgeted on top of this.
///
/// The default `Spacing::combo_width` (`egui-0.35.0/src/style.rs:1457`), which is what the
/// old `egui::ComboBox`-based button used as its MINIMUM width.
const FONT_COMBO_MIN_WIDTH_PT: f32 = 100.0;

/// Everything one frame of the typing font combo needs from its caller.
///
/// The two panels differ only in these five values; everything else about the combo — the
/// rows, the caption, the display clamp, the pick edge — is identical and lives in
/// [`TypingCreatePanelState::draw_font_combo`].
#[derive(Debug, Clone, Copy)]
pub(super) struct FontComboSpec<'a> {
    /// STABLE, language-independent id salt — never a localized caption
    /// (`egui-docs/05-ids-and-i18n.md` §2). It is what the widget's popup state hangs off.
    pub(super) id_salt: &'static str,
    /// The already-localized label drawn AFTER the button, the way `egui::ComboBox` drew it
    /// (`egui-0.35.0/src/containers/combo_box.rs:252-255`).
    pub(super) label: &'a str,
    /// TOTAL width in points of the combo — its button, the gap, and the square search
    /// button — which the popup inherits. See [`font_combo_button_width`]: the widget's width
    /// is EXACT, so a caller that passes nothing would visibly shrink the row.
    pub(super) width: f32,
    /// The inline span's RAW render label while a text selection is being styled, `None`
    /// outside inline-selection mode. Display resolution only: it is never written back here.
    pub(super) inline_font_label: Option<&'a str>,
    /// `true` when the layer's font is not loaded: the button then names the MISSING font
    /// instead of a row (create panel only; the edit panel shows a red banner instead).
    pub(super) font_missing: bool,
}

/// What one frame of the typing font combo decided.
#[derive(Debug)]
pub(super) struct FontComboOutcome {
    /// Index into `TypingCreatePanelState::fonts` the combo SHOWS as selected, after the
    /// display clamp. Reproduces the value the old call sites wrote into
    /// `selected_font_idx`, including the empty-list case (the resolved index survives).
    pub(super) font_idx: usize,
    /// The font the user genuinely PICKED this frame — a popup commit (even on the already
    /// selected row) or a wheel step that moved — else `None`. The ONLY value allowed to
    /// write an inline span's font label; the per-frame `font_idx` must never do that.
    pub(super) user_pick: Option<usize>,
    /// The closed button's response, for hover-driven caller logic.
    pub(super) response: egui::Response,
}

/// One row of the typing font combo, materialized for one frame.
///
/// Owned on purpose. [`crate::widgets::SearchableComboBox`] borrows both the row texts and
/// the per-row font resolver while it holds `&mut Ui`; building the rows FIRST is what lets
/// the `&self` borrow of the panel end before the widget runs, which is the only way the
/// resolver can be a closure that touches neither `self` nor `ui`.
#[derive(Debug, Clone)]
struct FontComboRow {
    /// Index into `TypingCreatePanelState::fonts`.
    font_idx: usize,
    /// DISPLAY ONLY (`font_display_label`): the row's main line and the closed caption.
    label: String,
    /// The render identity (`FontEntry::render_identity_name`): the row's second line AND
    /// the key its own-typeface preview registration is derived from.
    identity: String,
    /// `FontEntry::content_hash` — the byte discriminant of that registration, so a replaced
    /// file is never previewed from stale bytes. `0` = content unknown.
    content_hash: u64,
    /// Where the preview's bytes are read from; never part of the registration key.
    path: PathBuf,
    /// The face of `path` the preview is rendered from.
    face_index: usize,
    /// Coverage colour of the main line; `None` for a font that fully covers the language.
    color: Option<egui::Color32>,
    /// Already-localized coverage tooltip; `None` for full coverage.
    tooltip: Option<String>,
}

/// Position the font combo SHOWS as selected: where `font_idx` sits among `font_indices`, or
/// `0` when it is not among them.
///
/// That fallback is the historical DISPLAY clamp: a font outside the active group marks the
/// first visible row instead of no row at all. Pure and total — an empty list yields `0`,
/// which the caller maps back to "no row exists", so the clamp can never write a font the
/// user did not pick.
#[must_use]
pub(super) fn font_combo_selected_position(
    font_indices: impl IntoIterator<Item = usize>,
    font_idx: usize,
) -> usize {
    font_indices
        .into_iter()
        .position(|idx| idx == font_idx)
        .unwrap_or(0)
}

/// TOTAL width, in points, to give the font combo on the current row — its button, the gap
/// and the square search button together, which is what `SearchableComboBox::width` means.
///
/// That width is EXACT and the caption is elided, whereas the old `egui::ComboBox` treated
/// `Spacing::combo_width` as a MINIMUM and grew to fit its caption
/// (`egui-0.35.0/src/containers/combo_box.rs:345-361`) — so passing nothing would visibly
/// shrink both panels. `reserved` is the width the caller still needs on the same row AFTER
/// the combo and its label (the edit panel shares its row with the face combo). The floor is
/// [`FONT_COMBO_MIN_WIDTH_PT`] for the button PLUS whatever the widget says its search button
/// costs, so a cramped row shrinks the drop-down and never the magnifier out of existence.
#[must_use]
pub(super) fn font_combo_button_width(ui: &egui::Ui, label: &str, reserved: f32) -> f32 {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    // The colour is irrelevant to a measurement: this galley is never painted.
    let label_width = ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.to_string(), font_id, egui::Color32::WHITE)
            .size()
            .x
    });
    // Asked of the widget rather than re-derived here: the square's side follows the row
    // height, which follows `FONT_COMBO_PREVIEW_SIZE_PT` and the active style.
    let search_button = SearchableComboBox::search_button_overhang(ui, FONT_COMBO_PREVIEW_SIZE_PT);
    (ui.available_width() - label_width - ui.spacing().item_spacing.x - reserved)
        .max(FONT_COMBO_MIN_WIDTH_PT + search_button)
}

impl TypingCreatePanelState {
    /// Draws the create-only presets section of the «Параметры» dock tab.
    ///
    /// `extras` is that tab's persisted state; the section's expanded/collapsed
    /// flag lives in it (see [`collapsing_param_section`]). Draws nothing on the
    /// edit panel.
    pub(super) fn draw_create_presets_section(
        &mut self,
        ui: &mut egui::Ui,
        extras: &mut TabExtras,
    ) {
        if !self.preview_enabled {
            return;
        }
        // The section title comes from the collapsing header (below); the summary
        // is the currently selected preset display name (or the "none" label).
        let preview_enabled = self.preview_enabled;
        let preset_summary = self
            .selected_preset_name
            .clone()
            .unwrap_or_else(|| text_preset_none_label().to_string());
        collapsing_param_section(
            ui,
            ParamSectionId::in_tab("typing.section.presets", preview_enabled, extras),
            t!("typing.presets.section_heading"),
            false,
            Some(preset_summary.as_str()),
            |ui| {
                self.draw_create_presets_body(ui);
            },
        );
    }

    /// Body of the create-presets section: the preset selector combo (which also CREATES a
    /// preset), the rename field of the selected preset with its save and delete buttons,
    /// and the unsaved-changes warning. The strong section title is shown in the collapsing
    /// header, so it is not drawn inline here.
    ///
    /// NO `ui.group` FRAME. The collapsing header already separates this section from its
    /// neighbours, and none of the sibling parameter sections draws one; a border here read
    /// as a second, redundant boundary around the one section that had it.
    fn draw_create_presets_body(&mut self, ui: &mut egui::Ui) {
        self.draw_global_preset_combo(ui);
        self.draw_global_preset_name_row(ui);
        self.draw_global_preset_unsaved_warning(ui);
    }

    /// The global-preset combo: «Нет», the «create» action row, then every preset by name.
    ///
    /// THE WHEEL NEVER CREATES A PRESET. Wheel steps go through
    /// [`global_preset_wheel_target`], which cycles a virtual list holding only «Нет» and the
    /// real presets; the «create» row exists in the popup alone. Cycling into a creation
    /// would mint presets on a stray scroll over the closed combo.
    fn draw_global_preset_combo(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut names: Vec<String> = self.presets_by_name.keys().cloned().collect();
            names.sort();
            let selected_text = self
                .selected_preset_name
                .as_deref()
                .unwrap_or(text_preset_none_label());
            // Position of the selection IN `names`, `None` for «Нет». The row index in the
            // popup is one higher for «Нет» and one more for the «create» row, but neither of
            // those is addressable by the wheel, so the wheel works on this value.
            let selected_pos = self
                .selected_preset_name
                .as_ref()
                .and_then(|selected| names.iter().position(|name| name == selected));
            // Collected inside the popup closure and applied AFTER it: every arm needs
            // `&mut self`, which the closure would still be borrowing.
            let mut action: Option<GlobalPresetRowAction> = None;
            // The row-count bucket is part of the id: a popup `Area` remembers its size under
            // a fixed id and can only ever shrink, so a constant salt would pin the height
            // measured at the SHORTEST row count the popup was ever opened at — and this list
            // grows in place, one row per created preset. See
            // `global_preset_popup_id_bucket`. «Нет» + the «create» row are the two rows
            // that exist before any preset does.
            let row_count = names.len() + GLOBAL_PRESET_POPUP_TEXT_ROWS;
            let popup_bucket = global_preset_popup_id_bucket(ui, row_count);
            let preset_combo =
                WheelComboBox::from_label(t!("typing.presets.current_preset_combo_id"))
                    .id_salt(("typing.presets.current_preset_combo_id", popup_bucket))
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(selected_pos.is_none(), text_preset_none_label())
                            .clicked()
                        {
                            action = Some(GlobalPresetRowAction::Deselect);
                        }
                        // Never drawn as "selected": creating leaves the NEW preset selected,
                        // and this row is an action, not a state.
                        if ui
                            .selectable_label(false, t!("typing.presets.create_option"))
                            .clicked()
                        {
                            action = Some(GlobalPresetRowAction::Create);
                        }
                        for (idx, name) in names.iter().enumerate() {
                            if ui.selectable_label(selected_pos == Some(idx), name).clicked() {
                                action = Some(GlobalPresetRowAction::Select(name.clone()));
                            }
                        }
                    });
            // Wheel steps are only reported while the popup is CLOSED, so they can never race
            // with a click collected above.
            if let Some(steps) = preset_combo.wheel_steps
                && let Some(target) = global_preset_wheel_target(selected_pos, names.len(), steps)
            {
                action = Some(match target.and_then(|idx| names.get(idx).cloned()) {
                    Some(name) => GlobalPresetRowAction::Select(name),
                    None => GlobalPresetRowAction::Deselect,
                });
            }
            // A row that IS the panel's current state is dropped here rather than inside the
            // arms below, so every arm stays a plain action.
            if action
                .as_ref()
                .is_some_and(|requested| self.global_preset_action_changes_nothing(requested))
            {
                action = None;
            }
            match action {
                // «Нет»: ownership goes back to the panel itself. The session font profiles
                // simply become the fonts' own again, and the LIVE local set — which belonged
                // to the preset that was just dropped, in EITHER identity mode — goes back to
                // the parked document default (`restore_default_local_set_after_deselect`).
                Some(GlobalPresetRowAction::Deselect) => self.deselect_global_preset(),
                Some(GlobalPresetRowAction::Create) => self.create_global_preset(),
                // `apply_preset_by_name` has to see the PREVIOUS selection to know whether
                // this is the transition that parks the default local set, so the selection is
                // never written here.
                Some(GlobalPresetRowAction::Select(name)) => {
                    self.apply_preset_by_name(name);
                    self.queue_preview_render();
                }
                None => {}
            }
        });
    }

    /// Whether a combo row's action would change nothing — the row that already IS the
    /// panel's state.
    ///
    /// Re-applying the selected preset would park and re-install its own local set for no
    /// reason, and «Нет» with no selection would restore a default set that was never parked.
    /// The «create» row is never a no-op: it always mints a preset.
    #[must_use]
    fn global_preset_action_changes_nothing(&self, action: &GlobalPresetRowAction) -> bool {
        match action {
            GlobalPresetRowAction::Deselect => self.selected_preset_name.is_none(),
            GlobalPresetRowAction::Create => false,
            GlobalPresetRowAction::Select(name) => {
                self.selected_preset_name.as_deref() == Some(name.as_str())
            }
        }
    }

    /// The rename field of the SELECTED preset plus the save and delete buttons.
    ///
    /// All three are disabled without a selection: there is nothing to rename, commit or
    /// delete then, and the buffer is bound to the selection rather than being a name for a
    /// preset that does not exist yet.
    fn draw_global_preset_name_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let has_selection = self.selected_preset_name.is_some();
            let field_width =
                (ui.available_width() - PRESET_NAME_ROW_BUTTON_RESERVE_PT).max(120.0);
            let preset_name_resp = ui.add_enabled(
                has_selection,
                egui::TextEdit::singleline(&mut self.preset_name_input)
                    .id_salt("typing_preset_name_input")
                    .hint_text(t!("typing.presets.name_hint"))
                    .desired_width(field_width),
            );
            self.track_text_input(&preset_name_resp);
            if ui
                .add_enabled(
                    has_selection,
                    egui::Button::new(t!("typing.presets.save_button")),
                )
                .clicked()
            {
                self.save_current_preset();
            }
            self.draw_global_preset_delete_control(ui, has_selection);
        });
    }

    /// The two-step delete button of the selected preset: the first click ARMS it (the
    /// caption switches to the confirm wording), a second PLAIN click deletes.
    ///
    /// Ported from `settings::typesetting::font_groups::draw_delete_control`, hardening
    /// included: a physical DOUBLE-click cannot delete (the confirm step requires
    /// `clicked() && !double_clicked()`, and a double-click delivers a press on two
    /// consecutive frames), and the armed state AUTO-DISARMS as soon as the pointer is no
    /// longer over the button, so a stale arm cannot turn a later unrelated click into a
    /// deletion. Tinted red in BOTH states — it is destructive at rest — and more strongly so
    /// once armed.
    fn draw_global_preset_delete_control(&mut self, ui: &mut egui::Ui, enabled: bool) {
        // A disabled button reports neither clicks nor hovers, so an arm left behind by a
        // selection change could otherwise sit there until the next selection.
        let armed = self.preset_delete_armed && enabled;
        let button = if armed {
            egui::Button::new(t!("typing.presets.delete_confirm_button"))
                .fill(PRESET_DELETE_ARMED_COLOR)
        } else {
            egui::Button::new(t!("typing.presets.delete_button")).fill(PRESET_DELETE_IDLE_COLOR)
        };
        let response = ui.add_enabled(enabled, button);
        if response.clicked() && !response.double_clicked() {
            if armed {
                self.delete_selected_preset();
            } else {
                self.preset_delete_armed = true;
            }
        } else if armed && !response.hovered() {
            self.preset_delete_armed = false;
        }
    }

    /// The yellow, small "this preset is not saved" line under the name row. Drawn only while
    /// a preset is selected AND [`Self::selected_preset_has_unsaved_changes`] holds.
    fn draw_global_preset_unsaved_warning(&mut self, ui: &mut egui::Ui) {
        if !self.selected_preset_has_unsaved_changes() {
            return;
        }
        ui.label(
            egui::RichText::new(t!("typing.presets.unsaved_warning"))
                .small()
                .color(FONT_DIAGNOSTIC_WARNING_COLOR),
        );
    }

    /// Whether the selected global preset differs from what the document on disk holds.
    /// Always `false` with no preset selected.
    ///
    /// TWO independent sources, and neither is a per-frame payload comparison:
    /// - `selected_preset_dirty`, the flag raised at the ONE parameter-edit dispatch
    ///   (`local_presets::store_current_params_snapshot`) and by the structural local-preset
    ///   operations. Rebuilding the would-be-saved payload every frame instead would clone
    ///   the whole session profile map or local-preset vector on the GUI thread, against a
    ///   document that reaches ~127 KB;
    /// - a PENDING RENAME, which is a plain string compare and therefore computed inline: the
    ///   name is the preset's identity, so an edited name buffer is an unsaved change exactly
    ///   like an edited parameter.
    ///
    /// The rename compare is VERBATIM, byte for byte, exactly like the save
    /// ([`Self::save_current_preset`]): preset names are user data stored verbatim, so
    /// `" Рао-кун "` and `"Рао-кун"` are two different names and turning the first into the
    /// second is a rename the user must see reported as unsaved.
    #[must_use]
    pub(super) fn selected_preset_has_unsaved_changes(&self) -> bool {
        let Some(name) = self.selected_preset_name.as_deref() else {
            return false;
        };
        self.selected_preset_dirty || self.preset_name_input != name
    }

    /// Drops the global-preset selection («Нет» in the combo) and gives the live local set
    /// back to the document default.
    ///
    /// The inverse of the `no preset` → `preset` transition
    /// (`local_presets::park_default_local_set_for_global_preset`), and like it, it does NOT
    /// look at the identity mode: the live-set invariant does not mention the mode, and
    /// gating the restore on it left the panel owning the DEFAULT set while holding a global
    /// preset's — the first edit then persisted the preset's set over the user's own.
    ///
    /// The rename buffer, the dirty flag and the armed delete button all belong to the
    /// selection, so all three are cleared with it.
    pub(super) fn deselect_global_preset(&mut self) {
        self.selected_preset_name = None;
        self.preset_name_input.clear();
        self.selected_preset_dirty = false;
        self.preset_delete_armed = false;
        self.restore_default_local_set_after_deselect();
    }

    /// Applies a saved create preset — its per-font profile memory and its primary font —
    /// and re-syncs everything bound to the selection: the rename buffer, the dirty flag and
    /// the armed delete button.
    ///
    /// The preset names its font ONCE, by identity (`TypingCreatePreset.font`). A value the
    /// migration could not resolve survives there in its legacy spelling, so this stays a
    /// READ path: the profile map is re-keyed to IDENTITIES in memory and the primary font
    /// is resolved through the one legacy door
    /// (`dev-docs/font_identity_postscript_plan.md`, fixed decision 5). A key that resolves
    /// to no loaded font is kept VERBATIM rather than dropped — it is the only remaining
    /// clue about which font it meant, and the user may install that font later.
    ///
    /// MISSING PRIMARY FONT. When the preset NAMES a primary font that no loaded font
    /// matches BY NAME, the panel enters the same `missing_font` state an overlay load
    /// produces (`create_apply::select_font_by_identity`): the selection is left where it
    /// was and no profile is applied to it, so the preset is never silently applied to a
    /// DIFFERENT font than the one it was saved for. A legacy value that only matches a
    /// loaded font by PATH counts as missing too — the file at a remembered path is not
    /// proof of identity. A preset that names no font at all (an empty `font`, only
    /// reachable for a preset saved with an empty font list) keeps the current selection
    /// and is not a missing font.
    ///
    /// The flag is cleared AFTER the apply, not where the selection is set: applying stores
    /// the panel's parameters through the ordinary dispatch (which raises the flag), and what
    /// is on screen at the end of an apply is the preset's own content — not a user edit. A
    /// name that resolves to no preset changes nothing at all, the flag of the current
    /// selection included.
    pub(super) fn apply_preset_by_name(&mut self, name: String) {
        if !self.apply_preset_by_name_inner(name) {
            return;
        }
        self.selected_preset_dirty = false;
        self.preset_delete_armed = false;
    }

    /// The apply itself. Returns whether a preset was really installed — `false` only when
    /// `name` names no stored preset, in which case nothing was touched.
    fn apply_preset_by_name_inner(&mut self, name: String) -> bool {
        let Some(preset) = self.presets_by_name.get(&name).cloned() else {
            return false;
        };
        // THE LIVE-SET INVARIANT (`local_presets::default_local_set_snapshot`): from the next
        // line on the live local set belongs to this global preset, so the DEFAULT set has to
        // be parked FIRST — while `selected_preset_name` still says the panel owns it. A
        // no-op when a preset was already applied.
        self.park_default_local_set_for_global_preset();
        // Marked applied BEFORE any profile is stored: from here on every parameter write
        // belongs to THIS preset's working set, not to the font's persisted default
        // (`create_render_data::store_current_font_profile_by_idx`, variant A).
        self.preset_name_input.clone_from(&name);
        self.selected_preset_name = Some(name);
        // The MODE travels with the preset and is installed FIRST, because it decides which
        // of the two disjoint payloads below is the preset's real content. Persisting it
        // here keeps a restart in the mode the user last worked in (plan §3, D7).
        if self.identity_mode != preset.identity_mode {
            self.identity_mode = preset.identity_mode;
            self.persist_param_identity_mode();
        }
        if preset.identity_mode == ParamIdentityMode::LocalPreset {
            // A local-preset preset carries no font and no per-font profiles at all: its
            // whole payload is the set and the selection inside it.
            self.apply_local_preset_payload(preset.local_presets, preset.selected_local_preset);
            self.clamp_face_index();
            return true;
        }
        // A FONT-mode preset owns an EMPTY local set, and installing it is not cosmetic: the
        // live set must be the applied preset's in both identity modes, or switching to
        // «Пресет» under this preset would hand the panel the DEFAULT set to edit while the
        // preset owns it (`local_presets::apply_local_preset_payload`).
        self.apply_local_preset_payload(preset.local_presets, preset.selected_local_preset);
        // Applying a preset replaces the SESSION memory only; each font's persisted default
        // profile is left alone (a preset is an independent overlay, not a rewrite of what
        // every font remembers on disk).
        self.font_profiles_by_identity =
            FontProfileMemory::from_map(self.font_profiles_keyed_by_identity(preset.font_profiles));

        let primary = preset.font.trim();
        let names_a_font = !primary.is_empty();
        // The stored value is an identity; a leftover legacy value may be a name form or a
        // path, so it goes through the legacy door — where only NAME evidence may select.
        let target_idx = self.find_font_idx_by_identity(primary).or_else(|| {
            match self.match_font_by_legacy_reference(Some(primary), &[primary]) {
                Some(LegacyFontMatch::ByName(idx)) => Some(idx),
                Some(LegacyFontMatch::PathOnly(_)) | None => None,
            }
        });
        match target_idx {
            Some(idx) => {
                self.selected_font_idx = idx;
                self.missing_font = None;
            }
            None if names_a_font => {
                // The preset's own font is not loaded. Record it and stop: applying its
                // parameters to whatever font happens to be selected would show the user a
                // preset "applied" to a font it was never saved for.
                self.missing_font = Some(
                    Path::new(primary)
                        .file_name()
                        .and_then(|file| file.to_str())
                        .filter(|_| primary.contains(std::path::is_separator))
                        .unwrap_or(primary)
                        .to_string(),
                );
                return true;
            }
            None => {}
        }
        self.active_font_identity = self.current_font_identity();
        if let Some(identity) = self.current_font_identity() {
            if let Some(profile) = self.font_profiles_by_identity.get(&identity).cloned() {
                self.apply_render_data_json_with_options(&profile, false);
            } else {
                self.selected_face_idx = 0;
                self.sync_current_font_profile_memory();
            }
        }
        self.clamp_face_index();
        true
    }

    /// Re-keys a stored profile map to font IDENTITIES.
    ///
    /// Every key that resolves to a loaded font is replaced by that font's identity
    /// STRING (so a key differing only in case stops shadowing the profile it means);
    /// a key that resolves to nothing survives unchanged, so no user data is lost — it is
    /// the only remaining clue about which font it meant, and the font may be installed
    /// later. Such a key can never collide with a converted one, since a key that matches
    /// a loaded identity in any casing resolves by definition.
    ///
    /// COLLISION PRIORITY (deterministic, and NOT the `HashMap` iteration order — that is
    /// randomized per process, so the surviving profile used to be a coin toss). Several
    /// legacy keys can name one font (`/old/fonts/Regular.ttf` and `Regular`); the winner
    /// is the key with the strongest claim, and ties are broken lexicographically:
    ///
    /// 1. the key IS the identity, byte for byte — the current form;
    /// 2. the key is the identity up to case;
    /// 3. a legacy NAME (family / label / stem) — still a name for the font;
    /// 4. a legacy PATH — the weakest form, and the one the plan is removing.
    ///
    /// A PATH is deliberately still accepted HERE, unlike in font SELECTION: the stored key
    /// is the only reference a legacy preset ever had for a profile (it was literally
    /// `path.to_string_lossy()`), so refusing it would strand every profile the user has,
    /// while the worst case is remembered PARAMETERS attached to a font whose file was
    /// replaced — not a layer re-rendered in the wrong typeface. Ranking it last is what
    /// keeps a name from ever losing to a path.
    ///
    /// Every displaced profile is logged with the key that won, so a merge is visible
    /// rather than silent.
    fn font_profiles_keyed_by_identity(
        &self,
        profiles: HashMap<String, Value>,
    ) -> HashMap<String, Value> {
        // (winning rank, winning key, profile) per target key.
        let mut out: HashMap<String, (u8, String, Value)> = HashMap::with_capacity(profiles.len());
        // Sorting makes both the rank comparison and its lexicographic tie-break
        // independent of the map's randomized iteration order.
        let mut incoming: Vec<(String, Value)> = profiles.into_iter().collect();
        incoming.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (key, profile) in incoming {
            // One lookup decides both the target and the rank: the legacy door reports
            // WHICH kind of evidence matched, so a name can no longer lose to a path (it
            // used to, because the key was handed in as a path AND as a name, and the path
            // was tried first).
            let matched = self
                .find_font_idx_by_identity(&key)
                .map(LegacyFontMatch::ByName)
                .or_else(|| self.match_font_by_legacy_reference(Some(&key), &[&key]));
            let resolved = matched
                .map(LegacyFontMatch::font_idx)
                .and_then(|idx| self.font_identity_name_by_idx(idx));
            let (target, rank) = match resolved {
                Some(identity) => {
                    let rank = if identity == key {
                        0
                    } else if identity.eq_ignore_ascii_case(&key) {
                        1
                    } else if matches!(matched, Some(LegacyFontMatch::ByName(_))) {
                        2
                    } else {
                        3
                    };
                    (identity, rank)
                }
                // Unresolvable: kept verbatim, and alone under that key.
                None => (key.clone(), 0),
            };
            match out.entry(target) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert((rank, key, profile));
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let (loser_key, winner_key) = if rank < slot.get().0 {
                        let previous = slot.insert((rank, key.clone(), profile));
                        (previous.1, key)
                    } else {
                        (key, slot.get().1.clone())
                    };
                    crate::runtime_log::log_info(format!(
                        "typing presets: profile keys '{loser_key}' and '{winner_key}' both name \
                         the font '{}'; keeping the profile stored under '{winner_key}' (the \
                         stronger key form) and dropping the other.",
                        slot.key(),
                    ));
                }
            }
        }
        out.into_iter()
            .map(|(identity, (_, _, profile))| (identity, profile))
            .collect()
    }

    /// The preset payload for the panel's CURRENT state, in the current [`ParamIdentityMode`].
    ///
    /// The two identity modes own DISJOINT payloads and every preset carries exactly one of
    /// them. In local-preset mode `font` must stay EMPTY: a non-empty font would send
    /// [`Self::apply_preset_by_name`] down its MISSING PRIMARY FONT rule for a preset whose
    /// font is not its own identity at all, and the per-font profiles are meaningless there.
    ///
    /// The font-mode payload is the SESSION profile memory ONLY — the fonts the user actually
    /// touched here. It used to additionally copy the CURRENT font's profile into every other
    /// loaded font's key, which is what turned 67 real profiles into 162 stored ones (87 % of
    /// `user_config.json`) and, worse, made a preset claim parameters for fonts it was never
    /// configured for. Each font's own remembered parameters live in
    /// `fonts_data.fonts.<identity>.profile` and need no copy.
    ///
    /// A pure READ of `self`: the caller stores the result and owns the live-set transition.
    #[must_use]
    fn capture_current_preset(&self) -> TypingCreatePreset {
        match self.identity_mode {
            ParamIdentityMode::Font => TypingCreatePreset {
                font: self.current_font_identity().unwrap_or_default(),
                font_profiles: self.font_profiles_by_identity.to_map(),
                identity_mode: ParamIdentityMode::Font,
                local_presets: Vec::new(),
                selected_local_preset: None,
            },
            ParamIdentityMode::LocalPreset => TypingCreatePreset {
                font: String::new(),
                font_profiles: HashMap::new(),
                identity_mode: ParamIdentityMode::LocalPreset,
                local_presets: self.local_presets.clone(),
                selected_local_preset: self.selected_local_preset,
            },
        }
    }

    /// Name for a preset created from the combo's «create» row: `typing.presets.default_name`
    /// with the lowest 1-based index no existing preset already carries.
    ///
    /// The name IS the identity of a global preset (it is the `presets_by_name` key), so
    /// unlike a local preset's name this uniqueness is an INVARIANT and not a courtesy: a
    /// colliding name would silently OVERWRITE the preset holding it.
    ///
    /// Both loops are BOUNDED by the pigeonhole argument (one more candidate than there are
    /// stored presets), never open-ended: a catalog entry that lost its `{index}` placeholder
    /// makes every indexed candidate the same string, and an unbounded search would then hang
    /// the GUI thread instead of degrading to the suffixed fallback.
    #[must_use]
    fn free_default_preset_name(&self) -> String {
        let taken = self.presets_by_name.len();
        // From 1 UP, so a hole left by a rename or a deletion is reused: after deleting
        // «Пресет 2» the next created preset is «Пресет 2» again, not «Пресет 4».
        for index in 1..=taken + 1 {
            let candidate = tf!("typing.presets.default_name", index = index);
            if !self.presets_by_name.contains_key(&candidate) {
                return candidate;
            }
        }
        // Degenerate catalog (see above): disambiguate by suffix instead.
        let base = tf!("typing.presets.default_name", index = taken + 1);
        (2..=taken + 2)
            .map(|suffix| format!("{base} ({suffix})"))
            .find(|candidate| !self.presets_by_name.contains_key(candidate))
            // Unreachable by the same bound: `taken + 1` candidates against `taken` names.
            .unwrap_or_else(|| format!("{base} ({})", taken + 2))
    }

    /// Creates a new global preset that INHERITS everything on screen, selects it and
    /// persists the document off the GUI thread.
    ///
    /// The payload is [`Self::capture_current_preset`] — exactly what saving would have
    /// written — and the name is [`Self::free_default_preset_name`]. Deliberately independent
    /// of `preset_name_input`: that buffer is the RENAME field of the SELECTED preset, so
    /// gating creation on it would make the combo's «create» row silently do nothing.
    ///
    /// This is a `no preset` → `preset` transition even when one was already applied (the new
    /// preset replaces it), so the DEFAULT local set is parked exactly as
    /// [`Self::apply_preset_by_name`] parks it — before `selected_preset_name` changes, and a
    /// no-op when a preset was already applied
    /// (`local_presets::park_default_local_set_for_global_preset`).
    ///
    /// REFUSED WHILE `missing_font` IS SET, with the reason in the status line — the same
    /// rule and the same shape as `local_presets::create_local_preset`. The panel then sits
    /// on a NEIGHBOUR of a font it could not resolve, so the capture would record that
    /// SUBSTITUTED font as the new preset's own (in `Font` mode as `TypingCreatePreset.font`,
    /// in `LocalPreset` mode inside the snapshot the store refuses to build), and the preset
    /// would silently claim a typeface the user never chose.
    pub(super) fn create_global_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        if self.missing_font.is_some() {
            self.status_line = t!("typing.presets.create_blocked_missing_font").to_string();
            return;
        }
        // The outgoing owner keeps everything edited so far; the capture below is then the
        // state the user is looking at.
        self.store_current_params_snapshot();
        let name = self.free_default_preset_name();
        let preset = self.capture_current_preset();
        self.presets_by_name.insert(name.clone(), preset);
        self.park_default_local_set_for_global_preset();
        self.preset_name_input.clone_from(&name);
        self.selected_preset_name = Some(name);
        // The preset was just written FROM what is on screen, so nothing is unsaved.
        self.selected_preset_dirty = false;
        self.preset_delete_armed = false;
        self.spawn_presets_save(false);
    }

    /// Commits the SELECTED preset — both its name and its parameters — and persists the
    /// whole preset document off the GUI thread. A no-op with no preset selected.
    ///
    /// THE NAME IS THE IDENTITY of a global preset (it is the `presets_by_name` key), so a
    /// rename is remove-then-insert. Two names are refused rather than applied, each with its
    /// own status line: an EMPTY one (it can address no combo row, and `presets_store` drops
    /// it on write) and one another preset already holds — silently clobbering a second
    /// preset is never what a rename meant. Renaming to the name it already has is not a
    /// collision; it is the ordinary save.
    ///
    /// THE BUFFER IS TAKEN VERBATIM, never trimmed: preset names are USER DATA stored as
    /// typed (`presets_store`, and the module README's verbatim-name contract), so `" Рао-кун "`
    /// and `"Рао-кун"` are two legitimate, distinct presets. Trimming here read a buffer
    /// PREFILLED with a padded name as a rename to its trimmed form — refused as taken when
    /// that name existed (the parameter edit could then never be saved at all) or silently
    /// re-keying the preset when it did not. Only a COMPLETELY empty name is refused, because
    /// that is the one the store drops on write.
    ///
    /// Nothing is parked here: a preset is already applied, so the DEFAULT local set was
    /// parked at the transition that applied it.
    pub(super) fn save_current_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let Some(current_name) = self.selected_preset_name.clone() else {
            return;
        };
        // VERBATIM (see the doc comment): the buffer IS the name, whitespace included.
        let new_name = self.preset_name_input.clone();
        if new_name.is_empty() {
            self.status_line = t!("typing.presets.empty_name_status").to_string();
            return;
        }
        if new_name != current_name && self.presets_by_name.contains_key(&new_name) {
            self.status_line = tf!("typing.presets.name_taken_status", name = new_name);
            return;
        }

        self.store_current_params_snapshot();
        let preset = self.capture_current_preset();
        if new_name != current_name {
            // The old key must go, or the rename would leave a duplicate of the preset behind
            // under its previous name.
            self.presets_by_name.remove(&current_name);
        }
        self.presets_by_name.insert(new_name.clone(), preset);
        self.preset_name_input.clone_from(&new_name);
        self.selected_preset_name = Some(new_name);
        self.selected_preset_dirty = false;
        self.preset_delete_armed = false;
        self.spawn_presets_save(false);
    }

    /// Deletes the selected preset and returns the panel to «Нет». A no-op with no selection.
    ///
    /// The selection is dropped through [`Self::deselect_global_preset`] — the SAME path the
    /// «Нет» row takes — because that is what runs `restore_default_local_set_after_deselect`.
    /// Clearing `selected_preset_name` directly would leave the LIVE local set as the deleted
    /// preset's, breaking THE LIVE-SET INVARIANT
    /// (`local_presets::default_local_set_snapshot`) and letting the next edit persist a dead
    /// preset's set over the user's own default one.
    ///
    /// KNOWN CONSEQUENCE — no tombstone. The cross-instance merge is additive
    /// (`presets_store::save` + `PresetStoreEvent::MergedFromDisk`), so a preset deleted here
    /// can be re-added by a SECOND running app instance's next save. The same accepted
    /// asymmetry `fonts_data.json` carries: a preset that comes back can be deleted again,
    /// one that was destroyed could not be recovered.
    pub(super) fn delete_selected_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let Some(name) = self.selected_preset_name.clone() else {
            return;
        };
        self.presets_by_name.remove(&name);
        self.deselect_global_preset();
        self.spawn_presets_save(false);
    }

    /// Persists the whole preset document to `fonts/presets.json` off the GUI thread.
    ///
    /// `then_clean_user_config` additionally deletes the migrated legacy `TextTab` keys —
    /// only ever passed by the migration, and only AFTER the new document is safely on
    /// disk, so a failed write can never lose the presets it was supposed to replace.
    ///
    /// A failed save (or a failed thread spawn) is logged AND pushed to `preset_store_tx`,
    /// which the GUI thread turns into a visible status line: a preset the user just saved
    /// must never disappear silently, which is exactly what the two `let _ =` this replaced
    /// allowed.
    ///
    /// Under `#[cfg(test)]` the body early-returns before spawning, so unit tests never
    /// touch the real fonts directory; the write itself is covered by `presets_store`'s own
    /// tests and by `run_presets_save`, which a test drives synchronously (same precedent as
    /// `font_settings_store::persist_off_thread`).
    pub(super) fn spawn_presets_save(&self, then_clean_user_config: bool) {
        if cfg!(test) {
            return;
        }
        // The document is BOTH halves: the named presets and the panel's copy of the
        // document-level default local set. The mirror is used, never the live set — while a
        // global preset is applied the live set is THAT preset's, and writing it as the
        // default one would overwrite the user's own (`local_presets::default_local_set_snapshot`).
        let document = presets_store::StoredDocument {
            presets: self.presets_by_name.clone(),
            default_local: self.default_local_set_snapshot(),
        };
        let fonts_dir = self.fonts_dir.clone();
        let events = self.preset_store_tx.clone();
        // Ticket taken HERE, where the snapshot is: it is what keeps a slow writer from
        // putting an older state of the document back over a newer one.
        let ticket = presets_store::next_save_ticket();
        // Taken here for the same reason: it is the generation of THIS snapshot, and it is
        // what the outcome event marks clean (or re-arms) on the GUI thread.
        let generation = self.local_presets_generation;
        // And so is the SELECTION, for exactly the same reason: the document being written is
        // the one this preset was part of, and the user may have selected another preset (or
        // none) by the time a failure comes back. Without it a failed write re-raised the
        // unsaved-changes warning on whatever happened to be selected then.
        let selected_preset = self.selected_preset_name.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-save-create-presets".to_string())
            .spawn(move || {
                // The config path is resolved HERE, off the GUI thread, and handed down
                // explicitly so the whole chain can be tested against a temp file.
                let clean_config = then_clean_user_config.then(config::user_config_path);
                run_presets_save(
                    &fonts_dir,
                    &document,
                    ticket,
                    selected_preset.as_deref(),
                    generation,
                    clean_config.as_deref(),
                    &events,
                );
            });
        if let Err(err) = spawn_result {
            // A failed spawn is environmental and retryable: the next attempt may well get
            // its thread.
            report_preset_save_failure(
                &self.preset_store_tx,
                &format!("cannot spawn the presets.json writer thread: {err}"),
                self.selected_preset_name.as_deref(),
                generation,
                true,
            );
        }
    }

    /// Drains everything the background `fonts/presets.json` workers have to say, once per
    /// frame: the off-thread seed, the one-shot `user_config` migration, presets another app
    /// instance wrote, and save failures. A no-op when the channel is empty.
    ///
    /// The migration is finished HERE, on the GUI thread, because re-keying the legacy font
    /// references needs this panel's font list — the same reason `fonts_data`'s v1 migration
    /// is deferred to the end of a font-list build.
    ///
    /// And it waits for the AUTHORITATIVE (combined) list. The preset read and the font load
    /// are two independent background jobs, so "the fonts are usually there by now" is a
    /// race, not an ordering: when the reader wins, a migration run here would resolve no
    /// IMPORTED system font, keep those references verbatim, delete the legacy
    /// `user_config` key and never retry — `presets.json` and `fonts_data.json` would
    /// disagree about the same font forever. The payload is therefore PARKED until
    /// `poll_font_reload_results` reports the combined list.
    pub(super) fn poll_preset_store_events(&mut self) {
        loop {
            match self.preset_store_rx.try_recv() {
                Ok(PresetStoreEvent::Seeded {
                    presets,
                    default_local,
                    legacy,
                }) => {
                    self.install_seeded_presets(presets);
                    self.install_seeded_default_local_set(default_local);
                    if let Some(legacy) = legacy {
                        if self.font_list_is_authoritative {
                            self.finish_legacy_presets_migration(legacy);
                        } else {
                            self.pending_legacy_presets_migration = Some(legacy);
                        }
                    }
                }
                Ok(PresetStoreEvent::MergedFromDisk {
                    presets,
                    default_local,
                }) => {
                    // Written by another app instance and already part of the document on
                    // disk; adopting them keeps the next snapshot from dropping them again.
                    // Ours wins a name clash — it is what is on screen.
                    for (name, preset) in presets {
                        self.presets_by_name.entry(name).or_insert(preset);
                    }
                    // The DEFAULT local presets the save APPENDED are additive by the same
                    // rule (`presets_store::merge_default_local_set`): ours are kept, theirs
                    // are added after them.
                    self.adopt_appended_default_local_presets(default_local);
                }
                Ok(PresetStoreEvent::Saved {
                    default_local_generation,
                }) => self.note_default_local_set_saved(default_local_generation),
                Ok(PresetStoreEvent::SaveFailed {
                    reason,
                    selected_preset_name,
                    default_local_generation,
                    retryable,
                }) => {
                    self.status_line = tf!("typing.presets.save_error_status", err = reason);
                    // Same rule for the SELECTED GLOBAL preset: the document did not reach
                    // disk, so what is on screen is genuinely unsaved and the warning has to
                    // come back. Clean only once a write actually SUCCEEDED — but only for
                    // the preset the LOST SNAPSHOT belonged to. The failure arrives whole
                    // frames later, and a debounced DEFAULT-local-set write carries no
                    // global preset at all, so re-arming the flag on "something is selected"
                    // marked a preset the user had just selected, and never edited, unsaved
                    // for the rest of its selection.
                    if selected_preset_name.is_some()
                        && selected_preset_name == self.selected_preset_name
                    {
                        self.selected_preset_dirty = true;
                    }
                    // A failed write must not leave the DEFAULT local set marked clean: the
                    // edits it was carrying are still only in memory.
                    self.rearm_default_local_set_after_failed_save(
                        default_local_generation,
                        retryable,
                    );
                }
                // The senders live in the panel itself, so the channel cannot be
                // disconnected while the panel exists; both idle cases end the drain.
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Installs the presets the startup read found. A preset the user saved BEFORE the read
    /// landed wins over its stored namesake: it is the fresher one, and the save that wrote
    /// it already merged the document it did not know about (`presets_store::save`).
    fn install_seeded_presets(&mut self, presets: HashMap<String, TypingCreatePreset>) {
        for (name, preset) in presets {
            self.presets_by_name.entry(name).or_insert(preset);
        }
    }

    /// Completes the one-shot migration out of `user_config.TextTab.create_presets`
    /// (`dev-docs/font_identity_postscript_plan.md` phase 5) with the payload the seed read
    /// off the GUI thread, then persists the result and cleans the legacy keys.
    ///
    /// A migrated preset whose name is ALREADY taken is kept under a suffixed name instead
    /// of being dropped: two presets are the user's data twice over, and "the newer one
    /// wins" would silently delete years-old parameters the user never asked to lose.
    pub(super) fn finish_legacy_presets_migration(
        &mut self,
        legacy: Vec<presets_store::LegacyPresetEntry>,
    ) {
        if legacy.is_empty() {
            // Nothing to migrate. The dead keys may still be lying around, so the cleanup
            // pass runs anyway (it rewrites nothing when they are absent).
            spawn_user_config_cleanup(&self.fonts_dir);
            return;
        }
        let migrated = self.migrate_legacy_presets(legacy);
        let migrated_count = migrated.len();
        for (name, preset) in migrated {
            let free_name = self.free_preset_name(name);
            self.presets_by_name.insert(free_name, preset);
        }
        crate::runtime_log::log_info(format!(
            "typing: migrated {migrated_count} create preset(s) from user_config.json into \
             fonts/presets.json"
        ));
        self.spawn_presets_save(true);
    }

    /// `name` itself when no preset holds it, otherwise the first free `"{name} (N)"`
    /// (N from 2). A rename is logged, since the user will see a name they did not type.
    fn free_preset_name(&self, name: String) -> String {
        if !self.presets_by_name.contains_key(&name) {
            return name;
        }
        // Bounded by the number of presets plus one, so a free slot always exists.
        let taken = self.presets_by_name.len() + 2;
        for suffix in 2..=taken {
            let candidate = format!("{name} ({suffix})");
            if !self.presets_by_name.contains_key(&candidate) {
                crate::runtime_log::log_warn(format!(
                    "typing presets: a preset named '{name}' already exists, so the migrated \
                     one was kept as '{candidate}' rather than dropped."
                ));
                return candidate;
            }
        }
        // Unreachable by the bound above; keeping the original name would overwrite, so the
        // fallback appends the count instead of losing the preset.
        format!("{name} ({taken})")
    }

    /// Converts legacy presets into the current form, resolving every stored font
    /// reference against THIS panel's font list. Pure with respect to `self` (nothing is
    /// stored here), so the whole migration rule is unit-testable.
    ///
    /// Per preset:
    /// - the three competing primary references collapse into one `font`. Resolution is by
    ///   NAME (`primary_font_key` as an identity first, then the label, then the key, then
    ///   the path's own name forms); a match that exists ONLY as a file path is refused,
    ///   because a file sitting at a remembered location is not proof of identity.
    /// - the profile map is re-keyed by [`Self::font_profiles_keyed_by_identity`], where a
    ///   path key IS accepted (it is the only reference a legacy profile ever had) but
    ///   ranks below every name.
    /// - every profile body is upgraded to the current `text_params` schema.
    ///
    /// Anything that resolves to nothing is KEPT VERBATIM under its legacy string and
    /// logged — never dropped: it is the only surviving clue about the font it meant, and
    /// it resolves again once the user reinstalls that font.
    pub(super) fn migrate_legacy_presets(
        &self,
        legacy: Vec<presets_store::LegacyPresetEntry>,
    ) -> Vec<(String, TypingCreatePreset)> {
        legacy
            .into_iter()
            .map(|(name, preset)| {
                let font = self.migrate_legacy_primary_font(&name, &preset);
                let profiles = self.font_profiles_keyed_by_identity(preset.font_profiles);
                let font_profiles = profiles
                    .into_iter()
                    .map(|(key, profile)| (key, self.upgrade_profile_to_current_schema(profile)))
                    .collect();
                (
                    name,
                    TypingCreatePreset {
                        font,
                        font_profiles,
                        ..TypingCreatePreset::default()
                    },
                )
            })
            .collect()
    }

    /// Resolves the three legacy primary-font references of one preset into a single
    /// identity, or returns the strongest legacy string VERBATIM when nothing resolves by
    /// name. See [`Self::migrate_legacy_presets`] for the rule; this logs what it kept.
    fn migrate_legacy_primary_font(
        &self,
        preset_name: &str,
        preset: &presets_store::LegacyCreatePreset,
    ) -> String {
        let key = preset.primary_font_key.trim();
        let label = preset.primary_font_label.as_deref().unwrap_or_default().trim();
        let path = preset.primary_font_path.as_deref().unwrap_or_default().trim();
        if key.is_empty() && label.is_empty() && path.is_empty() {
            // The preset names no font at all; it selects nothing and reports nothing.
            return String::new();
        }
        // Identity first (what a late build wrote), then every remaining NAME form: the
        // stored key and label as written, then the FILE STEM of each stored path — the
        // last name candidate of the historical chain
        // (`text_params_schema::legacy_font_name_candidates`), so a preset and a layer
        // written by the same build resolve the same way. The paths themselves are only
        // offered to the path pass, whose match is refused below.
        let stem_of = |value: &str| {
            Path::new(value)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty() && *stem != value)
                .map(ToOwned::to_owned)
        };
        let stems: Vec<String> = [key, path].into_iter().filter_map(stem_of).collect();
        let names: Vec<&str> = [key, label]
            .into_iter()
            .chain(stems.iter().map(String::as_str))
            .filter(|value| !value.is_empty())
            .collect();
        let matched = self
            .find_font_idx_by_identity(key)
            .map(LegacyFontMatch::ByName)
            .or_else(|| self.match_font_by_legacy_reference(Some(path), &names));
        if let Some(LegacyFontMatch::ByName(idx)) = matched
            && let Some(identity) = self.font_identity_name_by_idx(idx)
        {
            return identity;
        }
        // Keep the strongest legacy spelling so the user can still see (and repair) what
        // the preset meant; `apply_preset_by_name` reports it as a missing font.
        let kept = names.first().copied().unwrap_or_default().to_string();
        crate::runtime_log::log_warn(format!(
            "typing presets: the primary font of preset '{preset_name}' ('{kept}') matches no \
             loaded font by name; it is KEPT VERBATIM and will resolve again if that font is \
             reinstalled."
        ));
        kept
    }

    /// Upgrades one stored profile body (`{ "effects": [...], "text_params": {...} }`) to
    /// the current `text_params` schema, reusing the ONE conversion the tab-side codec owns
    /// so a preset and a layer can never disagree about what a legacy key meant.
    ///
    /// A body whose font does not resolve is returned UNCHANGED (schema 1) — the conversion
    /// refuses to drop legacy keys it cannot replace, and so does this.
    fn upgrade_profile_to_current_schema(&self, profile: Value) -> Value {
        let Some(text_params) = profile
            .get("text_params")
            .and_then(Value::as_object)
            .cloned()
        else {
            return profile;
        };
        let upgraded = crate::tabs::typing::tab::codec::upgrade_text_params_to_v2(
            &text_params,
            &|path, name| self.resolve_legacy_font_identity(path, name),
        );
        match upgraded {
            crate::tabs::typing::tab::codec::TextParamsUpgrade::Converted(value) => {
                let mut profile = profile;
                if let Some(obj) = profile.as_object_mut() {
                    obj.insert("text_params".to_string(), value);
                }
                profile
            }
            // Already current, or a font this build cannot resolve: leave the body alone.
            crate::tabs::typing::tab::codec::TextParamsUpgrade::AlreadyCurrent
            | crate::tabs::typing::tab::codec::TextParamsUpgrade::UnresolvedFont { .. }
            | crate::tabs::typing::tab::codec::TextParamsUpgrade::PathOnlyFont { .. } => profile,
        }
    }


    pub(super) fn apply_formula_preset_by_name(&mut self, name: String) -> bool {
        let Some(preset) = self.formula_presets_by_name.get(&name).cloned() else {
            return false;
        };
        self.formula_layout = preset.layout;
        self.selected_formula_preset_name = Some(name);
        true
    }

    pub(super) fn save_current_formula_preset(&mut self) {
        let preset_name = self.formula_preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }
        self.formula_presets_by_name.insert(
            preset_name.clone(),
            TypingFormulaPreset {
                layout: self.formula_layout.clone(),
            },
        );
        self.selected_formula_preset_name = Some(preset_name);
        let presets = self.formula_presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-formula-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_formula_presets(&presets);
            });
    }

    pub(super) fn swap_formula_xy_expressions(&mut self) {
        std::mem::swap(
            &mut self.formula_layout.x_expr,
            &mut self.formula_layout.y_expr,
        );
        self.selected_formula_preset_name = None;
    }

    pub(super) fn sync_selected_formula_preset_by_layout(&mut self) {
        self.selected_formula_preset_name =
            self.formula_presets_by_name
                .iter()
                .find_map(|(name, preset)| {
                    if formula_layout_approx_eq(&self.formula_layout, &preset.layout) {
                        Some(name.clone())
                    } else {
                        None
                    }
                });
    }

    /// Materializes one owned row per font of the ACTIVE group, in catalog order.
    ///
    /// The rows are the combo's index space: `FontComboRow::font_idx` maps a row back to
    /// `self.fonts`, so a row list built this way can never drift from the positions the
    /// widget reports (which a separately-kept `filtered_font_indices()` could).
    ///
    /// There is deliberately NO cap on how many rows may register an own-typeface preview.
    /// This list is the project's own `fonts/` plus the fonts the user imported — not the OS
    /// catalog — and the previous combo registered EVERY filtered font on every frame its
    /// popup was open. `SearchableComboBox` resolves only the rows it actually draws, so the
    /// registrations are bounded by the visible rows rather than by the list length.
    fn build_font_combo_rows(&self) -> Vec<FontComboRow> {
        self.filtered_font_indices()
            .into_iter()
            .filter_map(|font_idx| {
                let font = self.fonts.get(font_idx)?;
                // Highlight fonts that do not fully support the typesetting language. The
                // wording lives in `font_coverage_tooltip`, which returns `None` for `Full`.
                let color = match font.coverage.support {
                    FontLanguageSupport::Full => None,
                    FontLanguageSupport::Partial => Some(FONT_DIAGNOSTIC_WARNING_COLOR),
                    FontLanguageSupport::Unsupported => Some(FONT_DIAGNOSTIC_ERROR_COLOR),
                };
                Some(FontComboRow {
                    font_idx,
                    label: self.font_display_label(font),
                    identity: font.render_identity_name(),
                    content_hash: font.content_hash(),
                    path: font.path.clone(),
                    face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
                    color,
                    tooltip: font_coverage_tooltip(&font.coverage),
                })
            })
            .collect()
    }

    /// The text the CLOSED font combo has to show, in every case the old combo covered.
    ///
    /// `font_indices` is the combo's row index space (see [`Self::build_font_combo_rows`]);
    /// it makes the inline label resolve group-preferringly, exactly as the row marking does.
    fn font_combo_caption(&self, spec: &FontComboSpec<'_>, font_indices: &[usize]) -> String {
        if spec.font_missing {
            // Шрифт оверлея не найден: показываем его имя, чтобы было понятно,
            // какой именно шрифт отсутствует и какой надо заменить.
            return self
                .missing_font
                .as_ref()
                .map(|name| tf!("typing.params.font_not_found_option", name = name))
                .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string());
        }
        spec.inline_font_label
            .map(|label| {
                // DISPLAY ONLY: resolve the raw render label to its display label (a user
                // rename override) when a font matches, so the CLOSED combo shows the same
                // name as the popup rows. The span style's render key is never touched.
                self.find_font_idx_by_label_preferring_indices(Some(label), font_indices)
                    .and_then(|idx| self.fonts.get(idx))
                    .map(|font| self.font_display_label(font))
                    .unwrap_or_else(|| label.to_string())
            })
            .or_else(|| {
                self.fonts
                    .get(self.selected_font_idx)
                    .map(|font| self.font_display_label(font))
            })
            .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
    }

    /// Draws ONE frame of the typing font combo — button, label and searchable drop-down —
    /// and reports what it decided.
    ///
    /// Shared verbatim by the create panel and the edit panel: both resolve the same fonts,
    /// clamp the same way and detect the same pick edge. What they do with
    /// [`FontComboOutcome::user_pick`] / [`FontComboOutcome::font_idx`] afterwards is NOT
    /// shared — the two writeback branches genuinely differ and stay at their call sites.
    ///
    /// Rows are `RowLayout::Wide`: the display label in the font's OWN face, followed by its
    /// render identity in the interface font, coloured and explained by the language-coverage
    /// diagnostics. The closed caption keeps the row's own face too, unless the text it must
    /// show is not a row at all (a missing font, an unresolvable inline label, or an inline
    /// font outside the active group) — then it is drawn in the interface font.
    ///
    /// Registering the own-typeface preview goes through the shared
    /// `widgets::font_preview`, keyed by `(identity, content hash, face index)`, so the two
    /// panels sharing one egui `Context` share one registration. The first frames of a row
    /// are drawn in the interface font: the file is read in the BACKGROUND and only
    /// `Context::add_font` happens on the GUI thread.
    pub(super) fn draw_font_combo(
        &self,
        ui: &mut egui::Ui,
        spec: &FontComboSpec<'_>,
    ) -> FontComboOutcome {
        let rows = self.build_font_combo_rows();
        let font_indices: Vec<usize> = rows.iter().map(|row| row.font_idx).collect();
        // Resolve the selection's/overlay's current font from its label. When a group is
        // active this PREFERS the in-group copy over a same-named font outside the group, so
        // an ambiguous label (e.g. an imported system font colliding with a group member)
        // does not silently resolve to the wrong entry.
        let resolved_font_idx = self
            .find_font_idx_by_label_preferring_indices(spec.inline_font_label, &font_indices)
            .unwrap_or(self.selected_font_idx);
        let caption = self.font_combo_caption(spec, &font_indices);
        // DISPLAY-ONLY clamp: a font outside the active group marks the first visible row, so
        // a valid row is always shown as selected. In inline-selection mode this clamped
        // value is NEVER written back into the span style (the caller's writeback is gated on
        // `user_pick`) — otherwise merely selecting text would bounce the label to a
        // different font and re-insert a `<font>` tag every frame.
        let mut position =
            font_combo_selected_position(font_indices.iter().copied(), resolved_font_idx);
        // The widget's own caption is the marked row's main line in that row's face, which
        // equals `caption` in the common case. Override it only when the two DISAGREE — the
        // missing-font text, an inline label that resolved to nothing, an inline font outside
        // the active group, or an empty list — so the common case keeps its own typeface.
        let caption_is_marked_row = rows.get(position).is_some_and(|row| row.label == caption);
        let items: Vec<SearchableComboItem<'_>> = rows
            .iter()
            .map(|row| {
                // The identity is shown on EVERY row, including the ones where it repeats the
                // display label. That duplicate is deliberate and was chosen over suppressing
                // it: a row that sometimes carries a second line and sometimes does not makes
                // the list ragged, and the reader loses the fixed place to look for the
                // PostScript name. The two lines still differ visibly — the label is drawn in
                // the font's own typeface, the identity in the interface font.
                let mut item = SearchableComboItem::with_secondary(&row.label, &row.identity);
                if let Some(color) = row.color {
                    item = item.primary_color(color);
                }
                if let Some(tooltip) = row.tooltip.as_deref() {
                    item = item.tooltip(tooltip);
                }
                item
            })
            .collect();
        // The resolver runs while the widget holds `&mut Ui`, so it may touch neither `ui`
        // nor `self`: it gets an owned `Context` handle and the owned rows, and nothing else.
        let ctx = ui.ctx().clone();
        let mut resolve_family = |index: usize| -> Option<egui::FontFamily> {
            let row = rows.get(index)?;
            match crate::widgets::request_font_family(
                &ctx,
                &row.identity,
                row.content_hash,
                &row.path,
                row.face_index,
            ) {
                crate::widgets::PreviewFontFamily::Ready(family) => Some(family),
                // Both non-ready states draw the row in the interface font, which is what the
                // widget does for a `None`. `Pending` retries by itself on a later frame.
                crate::widgets::PreviewFontFamily::Pending
                | crate::widgets::PreviewFontFamily::Unavailable => None,
            }
        };
        let mut combo = SearchableComboBox::new(spec.id_salt)
            .row_layout(RowLayout::Wide)
            .primary_size(FONT_COMBO_PREVIEW_SIZE_PT)
            .max_popup_height(FONT_COMBO_MAX_POPUP_HEIGHT_PT)
            .width(spec.width)
            .item_font(&mut resolve_family);
        if !caption_is_marked_row {
            combo = combo.selected_text(caption);
        }
        // The widget draws no label of its own; `egui::ComboBox` used to draw one after the
        // button inside its own horizontal row, and dropping it would silently remove the
        // word «Шрифт» from both panels.
        let before = position;
        let (response, picked, changed) = ui
            .horizontal(|ui| {
                let outcome = combo.show(ui, &mut position, &items);
                let label = ui.label(spec.label);
                (
                    outcome.response.labelled_by(label.id),
                    outcome.picked,
                    outcome.changed,
                )
            })
            .inner;
        let font_idx = font_indices
            .get(position)
            .copied()
            .unwrap_or(resolved_font_idx);
        // `changed` is exactly "the widget WROTE the selection this frame" — a click on
        // another row, `Enter`, or a wheel step — so pairing it with the pre-show position
        // gives the edge detector the same `(before, after)` the old wheel handling produced.
        let user_pick =
            create_main_text::font_combo_user_pick(picked, changed.then_some((before, position)))
                .and_then(|pos| font_indices.get(pos).copied());
        FontComboOutcome {
            font_idx,
            user_pick,
            response,
        }
    }

    pub(super) fn ensure_initial_preview_request(&mut self) {
        if !self.preview_enabled {
            return;
        }
        if !self.needs_initial_preview {
            return;
        }
        self.needs_initial_preview = false;
        self.queue_preview_render();
    }

    pub(super) fn clamp_face_index(&mut self) {
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let max_idx = font.faces.len().saturating_sub(1);
            self.selected_face_idx = self.selected_face_idx.min(max_idx);
        } else {
            self.selected_face_idx = 0;
        }
    }
}

/// Starts the OFF-GUI-THREAD seeding of a create panel's preset state.
///
/// Reading `fonts/presets.json` (and, when a migration is owed, the up-to-half-a-megabyte
/// `user_config.json`) is file I/O and must not happen while a panel is being constructed on
/// the GUI thread (CLAUDE.md §5). The worker sends exactly one
/// [`PresetStoreEvent::Seeded`], which `poll_preset_store_events` installs.
///
/// The panel starts with NO presets and the writer's baseline set to "the document is
/// absent", so a save issued before the seed lands cannot blindly overwrite the document it
/// has not read yet: `presets_store::save` sees the mismatch, merges the file in and
/// retries.
///
/// Under `#[cfg(test)]` nothing is spawned and no disk is touched: the store is covered by
/// `presets_store`'s tests and the migration RULE by `migrate_legacy_presets`' own tests,
/// while a unit test must never read the developer's real `fonts/` or `user_config.json`.
pub(super) fn spawn_presets_seed(fonts_dir: &Path, events: &Sender<PresetStoreEvent>) {
    if cfg!(test) {
        return;
    }
    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Absent);
    let fonts_dir = fonts_dir.to_path_buf();
    let events = events.clone();
    let spawn_result = thread::Builder::new()
        .name("typing-read-create-presets".to_string())
        .spawn(move || {
            let (event, clean_config_now) = read_presets_seed(&fonts_dir);
            // A closed channel means the panel is already gone; there is nobody left to
            // hand the payload to and nothing has been modified, so the send result is
            // deliberately ignored.
            let _ = events.send(event);
            if clean_config_now {
                clean_migrated_user_config_keys(&fonts_dir, &config::user_config_path());
            }
        });
    if let Err(err) = spawn_result {
        crate::runtime_log::log_warn(format!(
            "typing: could not spawn the create-preset reader; presets stay unloaded for this \
             session and the read is retried on the next launch: {err}"
        ));
    }
}

/// Reads the preset document and, when one is owed, the legacy payload the migration needs.
/// Returns the event to hand to the GUI thread plus whether the legacy `user_config` keys
/// may be cleaned right away.
///
/// - `Loaded`: use the document and remember its bytes as the writer's baseline. The legacy
///   keys are obsolete, but an earlier run may have died between writing the document and
///   rewriting the config, so that half is retried NOW (nothing is rewritten when the keys
///   are already gone).
/// - `Missing` / `Invalid` (the corrupt file is quarantined first, so the next save cannot
///   destroy a recoverable document): read the legacy `user_config.TextTab.create_presets`
///   payload for the one-shot migration, which is finished on the GUI thread, where the font
///   list exists. The config keys may only be dropped once the new document is written.
pub(super) fn read_presets_seed(fonts_dir: &Path) -> (PresetStoreEvent, bool) {
    match presets_store::load_outcome(fonts_dir) {
        presets_store::LoadOutcome::Loaded {
            document,
            fingerprint,
        } => {
            presets_store::set_baseline(
                fonts_dir,
                doc_store::SaveBaseline::Matching(fingerprint),
            );
            (
                PresetStoreEvent::Seeded {
                    presets: document.presets,
                    default_local: document.default_local,
                    legacy: None,
                },
                true,
            )
        }
        presets_store::LoadOutcome::Missing => (
            PresetStoreEvent::Seeded {
                presets: HashMap::new(),
                default_local: presets_store::DefaultLocalSet::default(),
                legacy: Some(presets_store::load_legacy_presets()),
            },
            false,
        ),
        presets_store::LoadOutcome::Invalid => {
            // The baseline follows what the quarantine achieved. `Failed` needs no baseline
            // at all: `quarantine_bad_file` has already disabled persistence for this file,
            // because the corrupt document is then the only copy of the user's presets.
            match presets_store::quarantine_bad_file(fonts_dir) {
                // The corrupt file is gone; the next save creates a fresh document.
                presets_store::QuarantineOutcome::Moved => {
                    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Absent);
                }
                // The corrupt file is still in place but its content is preserved in the
                // `.bad` copy, so replacing it is safe — and its bytes are not our baseline.
                presets_store::QuarantineOutcome::Copied
                | presets_store::QuarantineOutcome::Failed => {
                    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Unchecked);
                }
            }
            (
                PresetStoreEvent::Seeded {
                    presets: HashMap::new(),
                    default_local: presets_store::DefaultLocalSet::default(),
                    legacy: Some(presets_store::load_legacy_presets()),
                },
                false,
            )
        }
    }
}

/// Writes one preset snapshot and reports the outcome to the GUI thread. The body of the
/// background writer, split out so a test can drive a REAL save (and a real failure) without
/// a thread.
///
/// ORDERING CONTRACT: the legacy `user_config` keys are deleted only after `save` returned
/// `Ok`, and `save` returns only once the document AND its directory entry are durable
/// (`doc_store::Durability::ContentsAndDirectory`). Without that a power loss between the
/// two could leave the presets in neither file.
///
/// `default_local_generation` travels with the outcome in BOTH directions: a success marks
/// the panel's DEFAULT local set clean up to it, a failure re-arms the debounce for it. It is
/// the panel's `local_presets_generation` at the moment `document` was snapshotted.
///
/// `selected_preset_name` travels the same way and for the same reason, but in one direction
/// only: it names the GLOBAL preset that was applied when `document` was snapshotted, so a
/// FAILURE can re-raise the unsaved-changes warning for THAT preset instead of for whichever
/// one the user has selected by the time the failure arrives.
pub(super) fn run_presets_save(
    fonts_dir: &Path,
    document: &presets_store::StoredDocument,
    ticket: u64,
    selected_preset_name: Option<&str>,
    default_local_generation: u64,
    clean_user_config: Option<&Path>,
    events: &Sender<PresetStoreEvent>,
) {
    match presets_store::save(fonts_dir, document, ticket) {
        Ok(report) => {
            if !report.merged_from_disk.is_empty() || !report.appended_default_local.is_empty() {
                // Whatever the save merged in is already part of the document on disk, so
                // the panel must take it over or its next snapshot would drop it again.
                // Same reasoning as elsewhere: a closed channel means the panel is gone.
                let _ = events.send(PresetStoreEvent::MergedFromDisk {
                    presets: report.merged_from_disk,
                    default_local: report.appended_default_local,
                });
            }
            // The DEFAULT local set becomes clean HERE and only here — a spawned writer is
            // not a written document.
            let _ = events.send(PresetStoreEvent::Saved {
                default_local_generation,
            });
            if let Some(user_settings_file) = clean_user_config {
                clean_migrated_user_config_keys(fonts_dir, user_settings_file);
            }
        }
        Err(err) => report_preset_save_failure(
            events,
            &err.to_string(),
            selected_preset_name,
            default_local_generation,
            err.is_retryable(),
        ),
    }
}

/// Deletes the migrated (and dead) legacy `TextTab` keys off the GUI thread, without writing
/// `presets.json` first. Used when the document is already there (an earlier run wrote it but
/// could not rewrite the config) or when there was nothing to migrate at all; the pass
/// rewrites nothing when no legacy key is present, so it is a cheap no-op from the second
/// launch on. Test-gated like `spawn_presets_save`: a unit test must not touch the real
/// `user_config.json`.
fn spawn_user_config_cleanup(fonts_dir: &Path) {
    if cfg!(test) {
        return;
    }
    let fonts_dir = fonts_dir.to_path_buf();
    let spawn_result = thread::Builder::new()
        .name("typing-clean-legacy-presets-config".to_string())
        .spawn(move || clean_migrated_user_config_keys(&fonts_dir, &config::user_config_path()));
    if let Err(err) = spawn_result {
        crate::runtime_log::log_warn(format!(
            "typing: could not spawn the user_config cleanup thread; the legacy preset keys \
             stay in place and the cleanup retries next launch: {err}"
        ));
    }
}

/// Deletes the legacy `TextTab` keys the preset migration made obsolete and logs the
/// outcome. Which keys those are — in particular whether the imported-system-fonts list may
/// go — is decided by `presets_store::drop_migrated_user_config_keys` from the CONTENT of
/// `fonts_data.json`, never from its mere existence (see that function).
fn clean_migrated_user_config_keys(fonts_dir: &Path, user_settings_file: &Path) {
    match presets_store::drop_migrated_user_config_keys(fonts_dir, user_settings_file) {
        Ok(removed) if removed.is_empty() => {}
        Ok(removed) => crate::runtime_log::log_info(format!(
            "typing: removed migrated legacy keys from user_config.json TextTab: {removed:?}"
        )),
        Err(err) => crate::runtime_log::log_warn(format!(
            "typing: could not remove the migrated legacy preset keys from user_config.json; \
             they stay in place and the cleanup retries next launch: {err}"
        )),
    }
}

/// Logs a preset-save failure and hands the technical reason to the GUI thread, which turns
/// it into a visible status line. The localization happens THERE, not here: `tf!` is a
/// catalog lookup and the message belongs to the frame that shows it.
///
/// `selected_preset_name` is the GLOBAL preset the lost snapshot belonged to (`None` when
/// none was applied when it was taken), `default_local_generation` is the generation of that
/// snapshot and `retryable` whether attempting it again could ever succeed
/// (`presets_store::PresetsStoreError::is_retryable`); together they let the panel re-arm the
/// debounced DEFAULT local-set save instead of losing its edits
/// (`local_presets::rearm_default_local_set_after_failed_save`).
pub(super) fn report_preset_save_failure(
    events: &Sender<PresetStoreEvent>,
    reason: &str,
    selected_preset_name: Option<&str>,
    default_local_generation: u64,
    retryable: bool,
) {
    crate::runtime_log::log_error(format!("typing: failed to save fonts/presets.json: {reason}"));
    // A closed channel means the panel that would show the message is gone; the log line
    // above is then the whole record, so the send result is deliberately ignored.
    let _ = events.send(PresetStoreEvent::SaveFailed {
        reason: reason.to_string(),
        selected_preset_name: selected_preset_name.map(str::to_string),
        default_local_generation,
        retryable,
    });
}

/// Build the hover tooltip for a font dropdown item, or `None` when the font
/// fully supports the selected typesetting language (no highlight, no tooltip).
///
/// The writing-system name and language name are derived from the currently
/// selected `TextLanguage` (`ms_text_util::language::text_language()`), so the
/// wording is factually correct for any typesetting language, not just Russian.
/// This matches the language `coverage` was classified against: `facade.rs`
/// reloads coverage whenever the typesetting language changes.
fn font_coverage_tooltip(coverage: &FontLanguageCoverage) -> Option<String> {
    let language = ms_text_util::language::text_language();
    // The crate hands us catalog keys (it is GUI-free); resolve them here.
    let language_name = crate::i18n_resolve::resolve_key(language.name_key());
    let script_name = crate::i18n_resolve::resolve_key(language.group().script_name_key());
    match coverage.support {
        FontLanguageSupport::Full => None,
        FontLanguageSupport::Unsupported => Some(tf!("typing.font_coverage.unsupported_tooltip", script_name = script_name, language_name = language_name)),
        FontLanguageSupport::Partial => {
            let list = truncated_char_list(coverage.missing.as_slice());
            Some(tf!("typing.font_coverage.partial_tooltip", language_name = language_name, list = list))
        }
    }
}

/// Renders `chars` as a space-separated list, truncated to [`MAX_SHOWN_CHARS`]
/// with a "+N more" suffix.
///
/// The suffix reuses `typing.font_coverage.more_chars_tooltip`: it is a pure
/// "{shown} … (and N more)" fragment with exactly this meaning, and duplicating
/// the literal into a second key would only let the two drift per locale.
fn truncated_char_list(chars: &[char]) -> String {
    let shown: String = chars
        .iter()
        .take(MAX_SHOWN_CHARS)
        .map(|ch| ch.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let extra = chars.len().saturating_sub(MAX_SHOWN_CHARS);
    if extra > 0 {
        tf!("typing.font_coverage.more_chars_tooltip", shown = shown, extra = extra)
    } else {
        shown
    }
}

/// One user-facing status row of the per-render font diagnostic: the text, the
/// color it is painted in, and the tooltip explaining what it means.
#[derive(Debug)]
pub(super) struct FontFallbackStatusLine {
    pub(super) text: String,
    pub(super) color: egui::Color32,
    pub(super) tooltip: &'static str,
}

/// Turns the renderer's factual fallback report into at most two status rows.
///
/// Row 1 (warning color) lists the characters the deterministic fallback chain
/// drew and the font that drew each group — INFORMATION, not an error: the result
/// is correct and identical on every machine, it just is not the selected
/// typeface. Row 2 (error color) lists characters nothing could draw, which the
/// reader really does lose (a tofu box).
///
/// Returns an empty vector when the selected font served the whole text, so the
/// caller draws nothing at all. Both character lists are truncated by
/// [`truncated_char_list`].
///
/// This is the FACTUAL counterpart of [`font_coverage_tooltip`]: that one judges a
/// FONT against the typesetting LANGUAGE before anything is typed, this one reports
/// what happened to THIS text. Both are kept; they answer different questions.
pub(super) fn font_fallback_status_lines(
    report: &FontFallbackReport,
) -> Vec<FontFallbackStatusLine> {
    let mut lines = Vec::new();
    if !report.fallbacks.is_empty() {
        let list = report
            .fallbacks
            .iter()
            .map(|used| {
                tf!(
                    "typing.font_fallback.entry_label",
                    chars = truncated_char_list(used.chars.as_slice()),
                    font = used.family
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(FontFallbackStatusLine {
            text: tf!("typing.font_fallback.used_status", list = list),
            color: FONT_DIAGNOSTIC_WARNING_COLOR,
            tooltip: t!("typing.font_fallback.used_tooltip"),
        });
    }
    if !report.missing.is_empty() {
        lines.push(FontFallbackStatusLine {
            text: tf!(
                "typing.font_fallback.missing_status",
                chars = truncated_char_list(report.missing.as_slice())
            ),
            color: FONT_DIAGNOSTIC_ERROR_COLOR,
            tooltip: t!("typing.font_fallback.missing_tooltip"),
        });
    }
    lines
}
