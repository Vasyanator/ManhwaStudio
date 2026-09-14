/*
File: color_preset_picker.rs

Purpose:
Color picker that extends the stock egui palette popup with an editable hex
field, two rows of per-title color presets and an explicit "update / cancel"
pair of actions.

Main responsibilities:
- draw the color swatch button and its popup (palette + hex row + preset grid
  + actions);
- own the UI-only selection state (which preset cell is targeted, the color
  that cell was last synchronized with, and what proves the selection still
  describes the world);
- report to the caller when the color changed and when a preset cell was
  overwritten, so the caller can persist the preset set.

Key structures:
- `ColorPresets`: the 20 preset colors. Plain data; the caller owns it and is
  solely responsible for loading and saving it.
- `PresetDefaults`: what fills the cells a caller has no stored value for.
- `ColorPresetPicker`: the widget's UI state (selected cell + anchor color +
  the two witnesses used to detect an outside replacement, plus the hex field's
  text).
- `HexField`: the text of the hex row and the color that text describes, which
  is what keeps the binding two-way without the field and the palette fighting
  over the text.
- `ColorPresetPickerOutput`: per-frame result of `ColorPresetPicker::draw`.

Key functions:
- `ColorPresetPicker::draw`: one frame of the widget.
- `ColorPresetPicker::note_color_picked_by_user`: report a color the user picked
  outside the popup (the eyedropper), which must not be read as an outside
  replacement.
- `ColorPresets::from_stored` / `to_stored`: conversion to and from the caller's
  persisted byte form.

Notes:
- The selection is invalidated, never trusted forever: both the edited color and
  the preset set are owned by the caller and get replaced behind the widget's
  back (another text layer selected, another title opened). Everything the
  widget did itself is recorded, so what is left over is exactly the outside
  replacement.
- The palette itself is egui's own `color_picker::color_picker_color32` with
  `Alpha::BlendOrAdditive`, which is exactly what `Ui::color_edit_button_srgba`
  uses (egui-0.35.0/src/ui.rs:2043); the two must not drift apart, otherwise the
  "Blending" row would appear or disappear depending on which picker is shown.
- The hex row sits UNDER the whole stock palette rather than directly under its
  R/G/B row: everything between them (the color strip, the "Blending" radio, the
  three sliders) is painted by egui's own `color_picker_color32` in one call,
  whose private internals would have to be forked wholesale to insert a row in
  the middle. That fork is exactly what the previous note forbids.
- The widget never touches the disk and never blocks: persistence of
  `ColorPresets` belongs to the caller.
*/
use eframe::egui;
use egui::{
    Color32, CornerRadius, Popup, PopupCloseBehavior, Rect, Response, Sense, Stroke, StrokeKind,
    Vec2, ecolor::HexColor, vec2,
};

/// Number of preset cells in one row of the grid.
pub const PRESET_COLUMNS: usize = 10;
/// Number of preset rows shown under the palette.
pub const PRESET_ROWS: usize = 2;
/// Total number of preset cells.
pub const PRESET_COUNT: usize = PRESET_COLUMNS * PRESET_ROWS;

/// Width of the palette inside the popup, in points, and therefore the width of
/// the preset grid drawn under it.
///
/// egui declares the same value as a function-local `const COLOR_SLIDER_WIDTH`
/// inside `color_edit_button_hsva` (egui-0.35.0/src/widgets/color_picker.rs:526)
/// and never exports it. It is not a width argument either: the palette reads
/// `Spacing::slider_width`, so the value has to be pushed into the style before
/// the palette is drawn.
const COLOR_SLIDER_WIDTH: f32 = 275.0;

/// Gap between two neighbouring preset cells, in points.
const PRESET_CELL_GAP: f32 = 4.0;

/// Width of the hex text field, in points.
///
/// Sized for its longest content, `#rrggbbaa`, plus the field's own padding.
/// The row deliberately does NOT span [`COLOR_SLIDER_WIDTH`]: a text field as
/// wide as the palette would read as the widget's main input, which it is not.
const HEX_FIELD_WIDTH: f32 = 96.0;

/// Character limit of the hex text field.
///
/// The longest accepted form is `#rrggbbaa` (9 characters); the slack on top of
/// it exists so a paste that carries stray whitespace still parses after the
/// trim in [`parse_hex_text`], instead of being truncated into garbage.
const HEX_TEXT_LIMIT: usize = 12;

/// Glyph of the "copy" button, taken verbatim from the stock picker's own copy
/// button (egui-0.35.0/src/widgets/color_picker.rs:429) so the two read as the
/// same control. An icon, not a translatable string; its tooltip is localized.
const COPY_GLYPH: &str = "📋";

/// Inset of the color swatch inside its cell, in points. The highlight of a
/// selected or hovered cell is painted in this margin, so the swatch itself is
/// never covered by it.
const PRESET_SWATCH_INSET: f32 = 3.0;

/// Corner radius of a preset cell highlight.
const PRESET_CELL_CORNER_RADIUS: u8 = 3;

/// Fill of a cell with unsaved changes ("dirty").
///
/// The theme has no yellow: `Visuals::warn_fg_color` is orange in the dark theme
/// (egui-0.35.0/src/style.rs:1508). This follows the precedent of
/// `GLOBAL_FAVORITE_COLOR` in `crates/ms-tab-typing/src/panel/char_table/window.rs`.
const PRESET_DIRTY_FILL: Color32 = Color32::from_rgb(240, 200, 70);

/// Outline of a cell with unsaved changes, a lighter tone of
/// [`PRESET_DIRTY_FILL`] so the highlight reads as a frame the way the selected
/// cell does with `Visuals::selection`.
const PRESET_DIRTY_STROKE: Color32 = Color32::from_rgb(255, 236, 170);

/// Default preset palette: a neutral ramp, the saturated basics, and a few
/// muted/dark tones. Chosen for manhwa typesetting, where most text is drawn in
/// the grayscale part and effects reach for the saturated one.
const PALETTE_PRESETS: [Color32; PRESET_COUNT] = [
    // Row 1: neutral ramp, then the warm half of the saturated basics.
    Color32::from_rgb(0, 0, 0),       // black
    Color32::from_rgb(48, 48, 48),    // near-black gray
    Color32::from_rgb(96, 96, 96),    // dark gray
    Color32::from_rgb(144, 144, 144), // mid gray
    Color32::from_rgb(192, 192, 192), // light gray
    Color32::from_rgb(255, 255, 255), // white
    Color32::from_rgb(230, 30, 30),   // red
    Color32::from_rgb(245, 130, 30),  // orange
    Color32::from_rgb(250, 215, 60),  // yellow
    Color32::from_rgb(60, 175, 75),   // green
    // Row 2: the cool half of the saturated basics, then dark and muted tones.
    Color32::from_rgb(70, 200, 225),  // cyan
    Color32::from_rgb(45, 95, 215),   // blue
    Color32::from_rgb(140, 70, 200),  // violet
    Color32::from_rgb(240, 120, 180), // pink
    Color32::from_rgb(120, 25, 30),   // dark red
    Color32::from_rgb(25, 45, 110),   // dark blue
    Color32::from_rgb(30, 90, 55),    // dark green
    Color32::from_rgb(110, 75, 45),   // brown
    Color32::from_rgb(210, 180, 150), // muted beige
    Color32::from_rgb(90, 100, 115),  // muted slate
];

/// What fills the cells a caller has no stored value for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetDefaults {
    /// Every cell is opaque black.
    Empty,
    /// Every cell comes from the built-in typesetting palette.
    Palette,
}

/// The color of every preset cell.
///
/// Pure data: the widget reads and overwrites cells, but the set is owned and
/// persisted by the caller (per title, in practice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorPresets {
    colors: [Color32; PRESET_COUNT],
}

impl ColorPresets {
    /// Builds a full set from `defaults`.
    #[must_use]
    pub fn from_defaults(defaults: PresetDefaults) -> Self {
        match defaults {
            PresetDefaults::Empty => Self {
                colors: [Color32::BLACK; PRESET_COUNT],
            },
            PresetDefaults::Palette => Self {
                colors: PALETTE_PRESETS,
            },
        }
    }

    /// Builds a set from persisted values.
    ///
    /// `stored` holds PREMULTIPLIED sRGBA bytes in cell order, exactly as
    /// [`Self::to_stored`] produced them. A short `stored` leaves the remaining
    /// cells at `defaults`; a long one has its extra entries ignored. Neither
    /// case is an error: the stored length is data on disk and must never
    /// decide whether the widget works.
    #[must_use]
    pub fn from_stored(stored: &[[u8; 4]], defaults: PresetDefaults) -> Self {
        let mut presets = Self::from_defaults(defaults);
        for (slot, bytes) in presets.colors.iter_mut().zip(stored.iter()) {
            *slot = Color32::from_rgba_premultiplied(bytes[0], bytes[1], bytes[2], bytes[3]);
        }
        presets
    }

    /// Returns the set as PREMULTIPLIED sRGBA bytes, in cell order.
    ///
    /// Premultiplied is `Color32`'s own internal representation, so this
    /// round-trips through [`Self::from_stored`] without losing a bit; the
    /// unmultiplied form would not.
    #[must_use]
    pub fn to_stored(&self) -> [[u8; 4]; PRESET_COUNT] {
        self.colors.map(|color| color.to_array())
    }

    /// Borrows every preset color in cell order.
    #[must_use]
    pub fn colors(&self) -> &[Color32; PRESET_COUNT] {
        &self.colors
    }

    /// Returns the color of cell `index`, or `None` if the index is out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Color32> {
        self.colors.get(index).copied()
    }

    /// Overwrites cell `index`.
    ///
    /// Returns `false` (and changes nothing) when `index` is out of range, so a
    /// caller whose index went stale gets a verdict instead of a panic.
    #[must_use]
    pub fn set(&mut self, index: usize, color: Color32) -> bool {
        if let Some(slot) = self.colors.get_mut(index) {
            *slot = color;
            true
        } else {
            false
        }
    }
}

/// UI state of the picker: which preset cell the user is aiming at, and the
/// color that cell was last synchronized with.
///
/// It is deliberately not persisted: it describes an in-progress edit, and a
/// selection restored across sessions would point at a preset set the user has
/// long since changed.
#[derive(Debug, Default)]
pub struct ColorPresetPicker {
    state: PresetSelection,
    hex: HexField,
}

/// Per-frame result of [`ColorPresetPicker::draw`].
#[derive(Debug)]
pub struct ColorPresetPickerOutput {
    /// Response of the swatch button that opens the popup.
    pub response: Response,
    /// The caller's color was changed by this widget during this frame.
    pub color_changed: bool,
    /// A preset cell was overwritten: the caller must persist the set.
    pub presets_changed: bool,
}

impl ColorPresetPicker {
    /// Draws one frame of the widget: the swatch button and, while its popup is
    /// open, the palette, the preset grid and the action row.
    ///
    /// `color` is the edited color, mutated in place by the palette and by a
    /// click on a preset cell. `presets` is overwritten only by the explicit
    /// "update" action.
    ///
    /// Must be called once per frame while the widget is shown: a frame is the
    /// unit in which an outside replacement of `color` or of `presets` is
    /// detected (see `PresetSelection::invalidate_if_replaced_outside`), and a
    /// skipped frame only defers that check, never breaks it.
    #[must_use]
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        color: &mut Color32,
        presets: &mut ColorPresets,
    ) -> ColorPresetPickerOutput {
        // Runs before this frame's own edits: everything the palette, the grid
        // and the action row do below is the widget's doing and must not be
        // mistaken for an outside replacement.
        self.state.invalidate_if_replaced_outside(*color, presets);

        let color_before = *color;
        let mut response = draw_swatch_button(ui, *color);
        let mut presets_changed = false;

        // `CloseOnClickOutside` is mandatory here: the default `CloseOnClick`
        // (egui-0.35.0/src/containers/popup.rs:196) would close the popup on the
        // first click on a preset cell. `Popup::menu` is deliberately NOT used:
        // its `menu_style` strips the frames off our two buttons and its
        // justified layout stretches them across the popup.
        let popup = Popup::from_toggle_button_response(&response)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside);
        popup.show(|ui| {
            // The palette reads its width from the style, not from an argument.
            ui.spacing_mut().slider_width = COLOR_SLIDER_WIDTH;
            // Same `Alpha` as `Ui::color_edit_button_srgba` (egui-0.35.0/src/ui.rs:2043),
            // so switching a call site to this widget cannot change what the
            // palette offers. Its "did it change" return value is deliberately
            // dropped: the caller-visible verdict is the before/after comparison
            // below, which is exact where egui's own `.changed()` reports a
            // change for a round trip that ends on the same `Color32`.
            egui::color_picker::color_picker_color32(
                ui,
                color,
                egui::color_picker::Alpha::BlendOrAdditive,
            );
            ui.add_space(PRESET_CELL_GAP);
            self.draw_hex_row(ui, color);
            ui.add_space(PRESET_CELL_GAP);
            self.draw_grid(ui, color, presets);
            ui.add_space(PRESET_CELL_GAP);
            presets_changed = self.draw_actions(ui, *color, presets);
        });

        // Whatever the frame ended with is the widget's own view of the color;
        // the next frame calls anything else an outside replacement.
        self.state.remember_color(*color);

        let color_changed = *color != color_before;
        if color_changed {
            // Mirrors the stock color button, whose response reports `changed()`
            // when the popup edited the color (color_picker.rs:535).
            response.mark_changed();
        }

        ColorPresetPickerOutput {
            response,
            color_changed,
            presets_changed,
        }
    }

    /// Reports a color the USER picked outside the popup — in practice the
    /// eyedropper, which samples the viewport in frames where [`Self::draw`] is
    /// never reached (the owner freezes the swatch instead).
    ///
    /// Without this call the sampled color would look like an outside
    /// replacement on the next frame and drop the selection. The selection and
    /// its anchor are deliberately left alone: a color the user picked on
    /// purpose SHOULD light the selected cell up as dirty, ready to be written
    /// into it. Call it at every point where the sampling changes the color,
    /// including the rollback that ends a cancelled sampling.
    pub fn note_color_picked_by_user(&mut self, color: Color32) {
        self.state.remember_color(color);
    }

    /// Draws the hex row: the editable `#rrggbb` / `#rrggbbaa` text of the
    /// current color, and the button that copies it to the clipboard.
    ///
    /// The binding is two-way: the text follows every change of `color` made
    /// elsewhere in the popup, and a parsable edit of the text writes `color`
    /// back. Nothing is returned, because `draw` reports a changed color from
    /// its own before/after comparison of the whole frame.
    fn draw_hex_row(&mut self, ui: &mut egui::Ui, color: &mut Color32) {
        // The id is needed BEFORE the field is drawn, to ask whether the user
        // is typing in it; `TextEdit::id` then pins the field to it instead of
        // letting it take an auto id that would not match.
        let field_id = ui.make_persistent_id("color_preset_hex");
        let focused = ui.memory(|memory| memory.has_focus(field_id));
        self.hex.sync(*color, focused);

        ui.horizontal(|ui| {
            let response = ui
                .add(
                    egui::TextEdit::singleline(&mut self.hex.text)
                        .id(field_id)
                        .desired_width(HEX_FIELD_WIDTH)
                        .char_limit(HEX_TEXT_LIMIT),
                )
                .on_hover_text(t!("widgets.color_preset_picker.hex_tooltip"));
            if response.changed()
                && let Some(parsed) = self.hex.apply_text(*color)
            {
                *color = parsed;
            }

            // Mirrors the stock picker's own copy button
            // (egui-0.35.0/src/widgets/color_picker.rs:428-438), which is a
            // bare glyph button that copies through the context. What is copied
            // is the text as shown, so the clipboard never disagrees with the
            // field the user is looking at.
            if ui
                .button(COPY_GLYPH)
                .on_hover_text(t!("widgets.color_preset_picker.copy_hex_tooltip"))
                .clicked()
            {
                ui.ctx().copy_text(self.hex.text.clone());
            }
        });
    }

    /// Draws the preset grid and applies clicks on its cells.
    fn draw_grid(&mut self, ui: &mut egui::Ui, color: &mut Color32, presets: &ColorPresets) {
        let side = preset_cell_side(
            COLOR_SLIDER_WIDTH,
            count_as_f32(PRESET_COLUMNS),
            PRESET_CELL_GAP,
        );
        let stride = side + PRESET_CELL_GAP;
        let rows = count_as_f32(PRESET_ROWS);
        let height = (rows * side + (rows - 1.0).max(0.0) * PRESET_CELL_GAP).max(0.0);

        // One allocation for the whole grid: the cells are then placed by hand
        // and sensed with `Ui::interact`, which claims no layout space. Asking
        // the popup for `available_width()` would be self-referential — an
        // `Area` builds its `Ui` from the size it had on the PREVIOUS frame.
        let (grid_rect, _grid_response) =
            ui.allocate_exact_size(vec2(COLOR_SLIDER_WIDTH, height), Sense::hover());

        for index in 0..PRESET_COUNT {
            let Some(preset) = presets.get(index) else {
                continue;
            };
            let row = count_as_f32(index / PRESET_COLUMNS);
            let column = count_as_f32(index % PRESET_COLUMNS);
            let cell_rect = Rect::from_min_size(
                grid_rect.min + vec2(column * stride, row * stride),
                Vec2::splat(side),
            );
            let cell_id = ui.make_persistent_id(("color_preset_cell", index));
            let response = ui.interact(cell_rect, cell_id, Sense::click());
            if response.clicked() {
                self.state.select(index, color, preset);
            }

            // Hover comes ONLY from `Response::hovered()`: a raw pointer read
            // would react through anything drawn on top of the popup
            // (`egui-docs/06-overlays.md` §5), and `contains_pointer()` stays
            // true while another widget holds a drag (egui-0.35.0/src/response.rs:319),
            // which would light cells up as the pointer crosses the grid with
            // the palette slider held down.
            let hovered = response.hovered();
            let state = preset_cell_state(
                self.state.selected() == Some(index),
                self.state.is_dirty(*color),
                hovered,
            );
            paint_preset_cell(ui, cell_rect, preset, state);
        }
    }

    /// Draws the "update"/"cancel" row. Returns `true` when a preset cell was
    /// overwritten and the caller must persist the set.
    fn draw_actions(
        &mut self,
        ui: &mut egui::Ui,
        color: Color32,
        presets: &mut ColorPresets,
    ) -> bool {
        // Both actions only make sense while the selected cell disagrees with
        // the current color; with nothing to confirm or to drop they are inert.
        let dirty = self.state.is_dirty(color);
        let mut presets_changed = false;

        ui.horizontal(|ui| {
            let update = ui
                .add_enabled(
                    dirty,
                    egui::Button::new(t!("widgets.color_preset_picker.update_button")),
                )
                .on_hover_text(t!("widgets.color_preset_picker.update_tooltip"))
                .on_disabled_hover_text(t!("widgets.color_preset_picker.update_tooltip"));
            if update.clicked()
                && let Some(index) = self.state.apply(color)
            {
                presets_changed = presets.set(index, color);
                if !presets_changed {
                    // Only reachable if the selection and the preset set went
                    // out of sync (a shorter set than the widget's own count);
                    // the color is simply not written, and the state is clean.
                    ms_log::runtime_log::log_warn(format!(
                        "Color preset cell {index} is out of range and was not updated."
                    ));
                }
            }

            let cancel = ui
                .add_enabled(
                    dirty,
                    egui::Button::new(t!("widgets.color_preset_picker.cancel_button")),
                )
                .on_hover_text(t!("widgets.color_preset_picker.cancel_tooltip"))
                .on_disabled_hover_text(t!("widgets.color_preset_picker.cancel_tooltip"));
            if cancel.clicked() {
                self.state.cancel();
            }
        });

        presets_changed
    }
}

/// Selection state of the picker, free of any `egui::Ui` access so the whole
/// interaction can be unit-tested.
///
/// "Dirty" is derived, never stored: a selected cell whose color no longer
/// equals `anchor` has unsaved changes. Deriving it is what makes a color the
/// user picked outside the popup (the eyedropper) light the cell up without the
/// widget having to observe that change — but it also means the selection only
/// tells the truth while `last_seen` and `selected_cell_color` confirm that
/// nobody replaced the color or the whole preset set behind the widget's back.
#[derive(Debug, Default)]
struct PresetSelection {
    /// Cell the user is aiming at, if any.
    selected: Option<usize>,
    /// Color the selected cell was last synchronized with.
    anchor: Color32,
    /// Color the widget itself last accounted for, or `None` before the first
    /// frame. A current color that differs from it was put there by somebody
    /// else — that is the whole detection.
    last_seen: Option<Color32>,
    /// Color the selected cell held when it was chosen or last written to.
    /// Meaningless while `selected` is `None`.
    selected_cell_color: Color32,
}

impl PresetSelection {
    /// The cell the user is aiming at.
    fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Whether the selected cell has unsaved changes.
    fn is_dirty(&self, color: Color32) -> bool {
        self.selected.is_some() && self.anchor != color
    }

    /// Drops the selection when the color or the preset set was replaced by
    /// somebody other than this widget.
    ///
    /// Called at the start of a frame, before any of this frame's own edits.
    /// Two replacements make the selection lie:
    /// - `color` differs from the one the widget last accounted for, so an
    ///   outside owner overwrote it (another text layer selected, a named
    ///   preset applied). Nobody picked that color for this cell, so it must
    ///   not mark the cell dirty: a single "update" click would otherwise
    ///   overwrite the preset with a color the user never chose. A color the
    ///   user DID pick outside the popup is announced through
    ///   [`ColorPresetPicker::note_color_picked_by_user`] and therefore never
    ///   looks like a replacement here.
    /// - the selected cell no longer holds the color it held when it was
    ///   chosen, so the whole set was replaced (another title opened) and the
    ///   index now points into a stranger's set. A coincidental match keeps the
    ///   selection, which is harmless: the cell then holds exactly the color
    ///   the selection promises.
    fn invalidate_if_replaced_outside(&mut self, color: Color32, presets: &ColorPresets) {
        if self.last_seen.is_some_and(|seen| seen != color) {
            self.selected = None;
        }
        if let Some(index) = self.selected
            && presets.get(index) != Some(self.selected_cell_color)
        {
            self.selected = None;
        }
    }

    /// Accounts for `color` as the one the widget knows about, so the next
    /// [`Self::invalidate_if_replaced_outside`] does not read it as a
    /// replacement. Never touches the selection or the anchor.
    fn remember_color(&mut self, color: Color32) {
        self.last_seen = Some(color);
    }

    /// Handles a click on cell `index`.
    ///
    /// With no unsaved changes the click MEANS "use this color": `color` is
    /// overwritten from `preset` and the cell becomes the (clean) selection.
    /// With unsaved changes it means "aim at this cell instead": only the
    /// selection moves, and the pending color survives so it can be written
    /// into the newly chosen cell.
    ///
    /// Either way the cell's current color is recorded, so a later replacement
    /// of the whole preset set can be told apart from an untouched one.
    ///
    /// Returns `true` when `color` was overwritten. An out-of-range `index` is
    /// ignored and returns `false`.
    fn select(&mut self, index: usize, color: &mut Color32, preset: Color32) -> bool {
        if index >= PRESET_COUNT {
            return false;
        }
        if self.is_dirty(*color) {
            self.selected = Some(index);
            self.selected_cell_color = preset;
            return false;
        }
        *color = preset;
        self.selected = Some(index);
        self.anchor = preset;
        self.selected_cell_color = preset;
        true
    }

    /// Drops the selection.
    ///
    /// The color is deliberately NOT rolled back: the user keeps the color they
    /// picked and merely stops aiming at a cell.
    fn cancel(&mut self) {
        self.selected = None;
    }

    /// Confirms the pending edit.
    ///
    /// Returns the cell the caller must overwrite with `color`, and makes the
    /// state clean while keeping the selection; the caller MUST perform that
    /// write, because the state now describes a cell holding `color`. Returns
    /// `None` when there is nothing to confirm.
    fn apply(&mut self, color: Color32) -> Option<usize> {
        if !self.is_dirty(color) {
            return None;
        }
        let index = self.selected?;
        self.anchor = color;
        // The caller's half of the contract is to write `color` into that cell,
        // so this is what the cell holds from the next frame's point of view.
        self.selected_cell_color = color;
        Some(index)
    }
}

/// Text state of the hex field, free of any `egui::Ui` access so the whole
/// binding can be unit-tested.
///
/// UI-only, like the selection: it describes a value being typed, which means
/// nothing outside the frames the popup is open.
#[derive(Debug, Default)]
struct HexField {
    /// What the field shows. Owned here because `egui::TextEdit` edits a
    /// `String` in place.
    text: String,
    /// The color `text` currently describes, or `None` while it describes no
    /// color at all (half-typed, or simply invalid). This is what tells a color
    /// the FIELD produced apart from one the palette or a preset cell produced,
    /// and therefore what makes the binding two-way without a fight over the
    /// text between the two sides.
    shown: Option<Color32>,
}

impl HexField {
    /// Brings the text in line with `color`.
    ///
    /// With no focus the text is always rewritten into the canonical form, so
    /// leaving the field also tidies up whatever was typed into it. While the
    /// user is typing, only a color that moved SOMEWHERE ELSE (the palette, a
    /// preset cell, the eyedropper) may overwrite the text under the caret; a
    /// value that parses into nothing yet is the user's to finish.
    fn sync(&mut self, color: Color32, focused: bool) {
        if focused && (self.shown.is_none() || self.shown == Some(color)) {
            return;
        }
        self.text = hex_text_for(color);
        self.shown = Some(color);
    }

    /// Applies the text the user just edited.
    ///
    /// `current` is the color before the edit; it supplies the alpha the short
    /// hex forms do not carry. Returns the color the text describes, or `None`
    /// when it describes none — in which case the color is left alone and the
    /// user keeps typing.
    fn apply_text(&mut self, current: Color32) -> Option<Color32> {
        self.shown = parse_hex_text(&self.text, current);
        self.shown
    }
}

/// Formats `color` for the hex field.
///
/// Six digits (`#rrggbb`) whenever the alpha byte carries no editable
/// magnitude — an opaque color (255), or egui's additive marker (0, whose rgb
/// is already premultiplied) — and eight (`#rrggbbaa`) for a partly transparent
/// color, whose alpha the six-digit form would drop.
fn hex_text_for(color: Color32) -> String {
    match color.a() {
        0 | 255 => HexColor::Hex6(color),
        _ => HexColor::Hex8(color),
    }
    .to_string()
}

/// Parses what the user typed in the hex field.
///
/// Accepts exactly the two forms the field itself shows — six digits
/// (`#rrggbb`, which keeps `current`'s alpha, see [`with_alpha_of`]) and eight
/// (`#rrggbbaa`, which sets it) — with the leading `#` optional and surrounding
/// whitespace ignored. Returns `None` for everything else, which is how a
/// half-typed value leaves the color alone instead of clearing it.
///
/// The CSS short forms egui also understands, three and four digits, are
/// deliberately REJECTED. The text is parsed on every keystroke, and `#ff00` is
/// a valid four-digit "transparent yellow" that every user typing `#ff0000`
/// passes through: accepting it would zero the alpha halfway through the word
/// and hand a transparent — or, with a non-zero rgb, an additive — color to the
/// six-digit form that follows it.
fn parse_hex_text(text: &str, current: Color32) -> Option<Color32> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    match HexColor::from_str_without_hash(digits).ok()? {
        HexColor::Hex6(rgb) => Some(with_alpha_of(rgb, current)),
        HexColor::Hex8(rgba) => Some(rgba),
        HexColor::Hex3(_) | HexColor::Hex4(_) => None,
    }
}

/// Re-applies `current`'s alpha to an opaque color parsed from the six-digit
/// hex form, which carries none.
///
/// `Color32` is premultiplied, so the two alpha regimes need different
/// constructors: a normal color is rebuilt from UNMULTIPLIED components, while
/// an alpha of 0 — egui's additive regime, where a non-zero rgb is stored
/// already premultiplied (ecolor-0.35.0/src/hsva.rs:45-50) — must be rebuilt as
/// such. `from_rgba_unmultiplied` maps EVERY alpha-0 color to `TRANSPARENT`
/// (ecolor-0.35.0/src/color32.rs:135-137), so using it here would silently turn
/// an additive color black.
fn with_alpha_of(rgb: Color32, current: Color32) -> Color32 {
    let [r, g, b, _] = rgb.to_array();
    match current.a() {
        0 => Color32::from_rgba_premultiplied(r, g, b, 0),
        alpha => Color32::from_rgba_unmultiplied(r, g, b, alpha),
    }
}

/// Visual state of one preset cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetCellState {
    /// Neither selected nor hovered.
    Idle,
    /// Hovered, and not the selected cell.
    Hovered,
    /// Selected, in sync with the current color.
    Selected,
    /// Selected, with unsaved changes.
    Dirty,
}

/// Picks the visual state of a cell. Selection outranks hover: while the user
/// aims at a cell, moving the pointer must not hide where the color would go.
fn preset_cell_state(selected: bool, dirty: bool, hovered: bool) -> PresetCellState {
    match (selected, dirty, hovered) {
        (true, true, _) => PresetCellState::Dirty,
        (true, false, _) => PresetCellState::Selected,
        (false, _, true) => PresetCellState::Hovered,
        (false, _, false) => PresetCellState::Idle,
    }
}

/// Side of one square preset cell, in points.
///
/// `width` is the width of the whole grid, `columns` the number of cells per
/// row and `gap` the space between two of them; the last cell of a row needs no
/// trailing gap. Returns `0.0` for degenerate input instead of a negative or
/// non-finite size, so a caller can always feed the result to `Rect`.
fn preset_cell_side(width: f32, columns: f32, gap: f32) -> f32 {
    if !width.is_finite() || !columns.is_finite() || !gap.is_finite() || columns < 1.0 {
        return 0.0;
    }
    let gaps = (columns - 1.0) * gap;
    ((width - gaps) / columns).max(0.0)
}

/// Converts a cell count or index to `f32` for the layout math.
///
/// `u16` is the widest integer with a lossless `f32` conversion, which is what
/// keeps this a checked conversion instead of a numeric `as` cast. Every value
/// passed here is bounded by [`PRESET_COUNT`], so the saturating fallback is
/// unreachable; it exists only so the function is total.
fn count_as_f32(count: usize) -> f32 {
    f32::from(u16::try_from(count).unwrap_or(u16::MAX))
}

/// Draws the color swatch button that opens the popup.
///
/// This reproduces egui's private `color_button`
/// (egui-0.35.0/src/widgets/color_picker.rs:110-136) through the public
/// `show_color_at`, including its "popup is open" visuals, so the button is
/// indistinguishable from the stock one it replaces.
fn draw_swatch_button(ui: &mut egui::Ui, color: Color32) -> Response {
    let desired_size = ui.spacing().interact_size;
    let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ColorButton));
    if !ui.is_rect_visible(rect) {
        return response;
    }

    // The popup id is derived from the response we just allocated, which is the
    // very id `Popup::from_toggle_button_response` will use — reading it here
    // avoids depending on the auto-id of the NEXT widget the way egui's own
    // `color_edit_button_hsva` does.
    let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
    let visuals = if open {
        &ui.visuals().widgets.open
    } else {
        ui.style().interact(&response)
    };
    let rect = rect.expand(visuals.expansion);

    let stroke_width = 1.0;
    egui::color_picker::show_color_at(ui.painter(), color, rect.shrink(stroke_width));
    // No more rounding than this: the transparency checkerboard behind the
    // color is not rounded at all.
    let corner_radius = visuals.corner_radius.at_most(2);
    ui.painter().rect_stroke(
        rect,
        corner_radius,
        // Using the fill as the stroke color is intentional and comes from
        // egui: the default style gives these widgets no border color.
        (stroke_width, visuals.bg_fill),
        StrokeKind::Inside,
    );

    response
}

/// Paints one preset cell: its state highlight and, inset into it, the color.
fn paint_preset_cell(ui: &egui::Ui, rect: Rect, color: Color32, state: PresetCellState) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    let corner_radius = CornerRadius::same(PRESET_CELL_CORNER_RADIUS);

    // The highlight is painted UNDER the swatch and shows through the inset, so
    // it reads as a frame around the color rather than covering it.
    let highlight = match state {
        PresetCellState::Idle => None,
        PresetCellState::Hovered => Some((ui.visuals().widgets.hovered.bg_fill, None)),
        PresetCellState::Selected => Some((
            ui.visuals().selection.bg_fill,
            Some(ui.visuals().selection.stroke),
        )),
        PresetCellState::Dirty => Some((
            PRESET_DIRTY_FILL,
            Some(Stroke::new(1.0, PRESET_DIRTY_STROKE)),
        )),
    };
    if let Some((fill, stroke)) = highlight {
        painter.rect_filled(rect, corner_radius, fill);
        if let Some(stroke) = stroke {
            painter.rect_stroke(rect, corner_radius, stroke, StrokeKind::Inside);
        }
    }

    let swatch = rect.shrink(PRESET_SWATCH_INSET);
    egui::color_picker::show_color_at(painter, color, swatch);
    // A thin border keeps a black preset distinguishable from the popup
    // background; same intentional "fill as stroke" as the swatch button.
    painter.rect_stroke(
        swatch,
        CornerRadius::ZERO,
        (1.0, ui.visuals().widgets.inactive.bg_fill),
        StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color32 = Color32::from_rgb(255, 0, 0);
    const BLUE: Color32 = Color32::from_rgb(0, 0, 255);

    #[test]
    fn palette_and_empty_defaults_fill_every_cell() {
        let empty = ColorPresets::from_defaults(PresetDefaults::Empty);
        assert!(empty.colors().iter().all(|c| *c == Color32::BLACK));

        let palette = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert_eq!(palette.colors()[0], Color32::BLACK);
        assert_eq!(palette.colors()[5], Color32::WHITE);
        // A palette of 20 identical colors would be a silent authoring mistake.
        assert!(palette.colors().iter().any(|c| *c != Color32::BLACK));
    }

    #[test]
    fn stored_values_round_trip_and_missing_cells_fall_back_to_defaults() {
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert!(presets.set(3, RED));
        let stored = presets.to_stored();
        assert_eq!(
            ColorPresets::from_stored(&stored, PresetDefaults::Empty),
            presets
        );

        // Short input: the tail comes from the defaults, not from garbage.
        let short = ColorPresets::from_stored(&stored[..2], PresetDefaults::Empty);
        assert_eq!(short.get(0), presets.get(0));
        assert_eq!(short.get(1), presets.get(1));
        assert_eq!(short.get(2), Some(Color32::BLACK));

        // Long input: the extra entries are ignored instead of panicking.
        let mut long = stored.to_vec();
        long.push([1, 2, 3, 4]);
        assert_eq!(ColorPresets::from_stored(&long, PresetDefaults::Empty), presets);
    }

    #[test]
    fn out_of_range_access_is_reported_not_panicked() {
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        assert_eq!(presets.get(PRESET_COUNT), None);
        assert!(!presets.set(PRESET_COUNT, RED));
        assert!(!presets.set(usize::MAX, RED));
        assert_eq!(presets.get(PRESET_COUNT - 1), Some(Color32::BLACK));
    }

    #[test]
    fn clicking_a_cell_while_clean_applies_its_color() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        assert!(state.select(4, &mut color, RED));
        assert_eq!(color, RED);
        assert_eq!(state.selected(), Some(4));
        assert!(!state.is_dirty(color));
        // Nothing to confirm while the cell and the color agree.
        assert_eq!(state.apply(color), None);
    }

    #[test]
    fn editing_the_color_makes_the_selected_cell_dirty() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;
        assert!(state.select(4, &mut color, RED));

        // Stands for a palette edit inside the popup, i.e. an edit the widget
        // performs itself between the frame-start check and the frame end.
        color = BLUE;
        assert!(state.is_dirty(color));
        assert_eq!(state.selected(), Some(4));
    }

    #[test]
    fn a_color_replaced_from_outside_drops_the_selection_instead_of_dirtying_it() {
        let presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        // Frame 1: a clean selection of a black cell.
        state.invalidate_if_replaced_outside(color, &presets);
        assert!(state.select(4, &mut color, Color32::BLACK));
        state.remember_color(color);
        assert_eq!(state.selected(), Some(4));

        // Between frames the owner replaces the color (another text layer was
        // selected on the canvas). The user picked nothing.
        color = RED;

        // Frame 2: the selection is gone, so "update" cannot overwrite the cell
        // with a color nobody chose.
        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), None);
        assert!(!state.is_dirty(color));
        assert_eq!(state.apply(color), None);
    }

    #[test]
    fn a_color_the_user_picked_outside_the_popup_keeps_the_selection_and_dirties_it() {
        let presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        state.invalidate_if_replaced_outside(color, &presets);
        assert!(state.select(4, &mut color, Color32::BLACK));
        state.remember_color(color);

        // The eyedropper samples the viewport in frames where the picker is not
        // drawn, and announces every write it makes.
        color = RED;
        state.remember_color(color);

        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), Some(4), "a deliberate pick keeps the cell");
        assert!(state.is_dirty(color));
        assert_eq!(state.apply(color), Some(4), "and can be written into it");
    }

    #[test]
    fn a_cancelled_eyedropper_leaves_the_selection_exactly_as_it_was() {
        let presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        state.invalidate_if_replaced_outside(color, &presets);
        assert!(state.select(4, &mut color, Color32::BLACK));
        state.remember_color(color);

        // Preview, then a rollback to the color the sampling started from; both
        // writes are announced.
        let start_color = color;
        color = RED;
        state.remember_color(color);
        color = start_color;
        state.remember_color(color);

        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), Some(4));
        assert!(!state.is_dirty(color), "a rolled-back sampling changed nothing");
    }

    #[test]
    fn replacing_the_whole_preset_set_drops_the_selection() {
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        state.invalidate_if_replaced_outside(color, &presets);
        assert!(state.select(12, &mut color, Color32::BLACK));
        state.remember_color(color);

        // Another title is opened: the owner replaces the whole set, and cell 12
        // now holds a color that has nothing to do with the selection.
        presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        assert_ne!(presets.get(12), Some(Color32::BLACK));

        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), None);
        assert_eq!(state.apply(color), None, "nothing can be written any more");
    }

    #[test]
    fn a_confirmed_cell_survives_the_next_frames_check() {
        let mut presets = ColorPresets::from_defaults(PresetDefaults::Empty);
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        state.invalidate_if_replaced_outside(color, &presets);
        assert!(state.select(4, &mut color, Color32::BLACK));
        // Palette edit inside the same frame, then "update".
        color = RED;
        assert_eq!(state.apply(color), Some(4));
        assert!(presets.set(4, color));
        state.remember_color(color);

        // The cell the state describes is the one the caller has just written.
        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), Some(4));
        assert!(!state.is_dirty(color));
    }

    #[test]
    fn retargeting_while_dirty_records_the_new_cells_color() {
        let presets = ColorPresets::from_defaults(PresetDefaults::Palette);
        let mut state = PresetSelection::default();
        let mut color = BLUE;
        // Taken from the same array the set was built from, so the recorded
        // cell color really is the one the set holds.
        assert!(state.select(4, &mut color, PALETTE_PRESETS[4]));
        color = RED;
        assert!(!state.select(7, &mut color, PALETTE_PRESETS[7]));
        state.remember_color(color);

        // The retargeted cell is untouched, so the pending edit survives the
        // next frame's check instead of being read as a replaced set.
        state.invalidate_if_replaced_outside(color, &presets);
        assert_eq!(state.selected(), Some(7));
        assert!(state.is_dirty(color));
    }

    #[test]
    fn clicking_another_cell_while_dirty_only_moves_the_target() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;
        assert!(state.select(4, &mut color, RED));
        color = BLUE;

        assert!(!state.select(7, &mut color, Color32::WHITE));
        assert_eq!(color, BLUE, "the pending color must survive retargeting");
        assert_eq!(state.selected(), Some(7));
        assert!(state.is_dirty(color));
    }

    #[test]
    fn apply_confirms_the_selected_cell_and_clears_dirty() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;
        assert!(state.select(4, &mut color, RED));
        color = BLUE;

        assert_eq!(state.apply(color), Some(4));
        assert!(!state.is_dirty(color));
        assert_eq!(state.selected(), Some(4), "the cell stays selected");
        assert_eq!(state.apply(color), None, "a second apply has nothing to do");
    }

    #[test]
    fn cancel_drops_the_selection_and_keeps_the_color() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;
        assert!(state.select(4, &mut color, RED));
        color = BLUE;

        state.cancel();
        assert_eq!(state.selected(), None);
        assert!(!state.is_dirty(color));
        assert_eq!(color, BLUE, "cancel must not roll the color back");
    }

    #[test]
    fn an_out_of_range_cell_is_never_selected() {
        let mut state = PresetSelection::default();
        let mut color = BLUE;

        assert!(!state.select(PRESET_COUNT, &mut color, RED));
        assert_eq!(state.selected(), None);
        assert_eq!(color, BLUE);
    }

    #[test]
    fn cell_state_gives_the_selection_priority_over_hover() {
        assert_eq!(preset_cell_state(true, true, true), PresetCellState::Dirty);
        assert_eq!(preset_cell_state(true, true, false), PresetCellState::Dirty);
        assert_eq!(
            preset_cell_state(true, false, true),
            PresetCellState::Selected
        );
        assert_eq!(
            preset_cell_state(false, true, true),
            PresetCellState::Hovered
        );
        assert_eq!(preset_cell_state(false, false, false), PresetCellState::Idle);
    }

    #[test]
    fn cell_side_fills_the_width_and_survives_degenerate_input() {
        // 10 cells and 9 gaps of 4pt must add up to the grid width exactly.
        let side = preset_cell_side(275.0, 10.0, 4.0);
        assert!((side * 10.0 + 4.0 * 9.0 - 275.0).abs() < 1e-3);
        assert!((side - 23.9).abs() < 0.05);

        assert_eq!(preset_cell_side(275.0, 1.0, 4.0), 275.0);
        assert_eq!(preset_cell_side(10.0, 0.0, 4.0), 0.0);
        assert_eq!(preset_cell_side(f32::NAN, 10.0, 4.0), 0.0);
        assert_eq!(preset_cell_side(10.0, 10.0, f32::INFINITY), 0.0);
        // Narrower than the gaps alone: clamped, never negative.
        assert_eq!(preset_cell_side(4.0, 10.0, 4.0), 0.0);
    }

    #[test]
    fn count_conversion_is_exact_for_every_cell_index() {
        assert_eq!(count_as_f32(0), 0.0);
        assert_eq!(count_as_f32(PRESET_COLUMNS), 10.0);
        assert_eq!(count_as_f32(PRESET_COUNT), 20.0);
    }

    #[test]
    fn hex_text_keeps_the_alpha_byte_only_when_it_carries_a_magnitude() {
        assert_eq!(hex_text_for(RED), "#ff0000");
        assert_eq!(hex_text_for(Color32::WHITE), "#ffffff");
        // Half-transparent: the alpha has to survive the round trip through the
        // text, so the eight-digit form is the only honest one.
        let half = Color32::from_rgba_unmultiplied(255, 0, 0, 128);
        assert_eq!(hex_text_for(half), "#ff000080");
        // Additive (egui's alpha-0 marker): the rgb is already premultiplied
        // and the alpha byte is a flag, not a magnitude.
        assert_eq!(hex_text_for(Color32::from_rgba_premultiplied(10, 20, 30, 0)), "#0a141e");
    }

    #[test]
    fn hex_text_round_trips_through_the_parser() {
        for color in [
            RED,
            BLUE,
            Color32::BLACK,
            Color32::WHITE,
            Color32::from_rgba_unmultiplied(12, 34, 56, 200),
        ] {
            let text = hex_text_for(color);
            assert_eq!(parse_hex_text(&text, Color32::BLACK), Some(color), "{text}");
        }
    }

    #[test]
    fn parsing_accepts_the_two_shown_forms_and_rejects_the_rest() {
        // The hash and the surrounding whitespace are optional.
        assert_eq!(parse_hex_text("#ff0000", Color32::WHITE), Some(RED));
        assert_eq!(parse_hex_text("ff0000", Color32::WHITE), Some(RED));
        assert_eq!(parse_hex_text("  #ff0000\t", Color32::WHITE), Some(RED));

        // Six digits carry no alpha, so the current one is kept...
        let half_blue = Color32::from_rgba_unmultiplied(0, 0, 255, 128);
        assert_eq!(parse_hex_text("#ff0000", half_blue).map(|color| color.a()), Some(128));
        // ...while eight digits set it.
        assert_eq!(
            parse_hex_text("#ff000040", half_blue),
            Some(Color32::from_rgba_unmultiplied(255, 0, 0, 0x40))
        );
        assert_eq!(parse_hex_text("#ff0000ff", half_blue), Some(RED));

        // The CSS short forms are valid hex yet deliberately refused: they are
        // the states a six-digit value passes through while being typed.
        assert_eq!(parse_hex_text("#f00", Color32::WHITE), None);
        assert_eq!(parse_hex_text("#ff00", Color32::WHITE), None);

        // Half-typed or plainly invalid: the color is left alone.
        assert_eq!(parse_hex_text("#ff000", Color32::WHITE), None);
        assert_eq!(parse_hex_text("#gg0000", Color32::WHITE), None);
        assert_eq!(parse_hex_text("", Color32::WHITE), None);
        assert_eq!(parse_hex_text("#", Color32::WHITE), None);
    }

    #[test]
    fn short_form_preserves_an_additive_color_instead_of_blanking_it() {
        // `from_rgba_unmultiplied` maps every alpha-0 color to TRANSPARENT, so
        // the additive regime needs the premultiplied constructor; getting this
        // wrong turns an additive color black without a word.
        let additive = Color32::from_rgba_premultiplied(1, 2, 3, 0);
        let parsed = parse_hex_text("#0a141e", additive);
        assert_eq!(parsed, Some(Color32::from_rgba_premultiplied(10, 20, 30, 0)));
    }

    #[test]
    fn unfocused_field_always_shows_the_current_color() {
        let mut field = HexField::default();
        field.sync(RED, false);
        assert_eq!(field.text, "#ff0000");

        // A color the palette or a preset cell moved to is picked up.
        field.sync(BLUE, false);
        assert_eq!(field.text, "#0000ff");

        // Leaving the field also tidies up a non-canonical spelling of the very
        // same color.
        field.text = "00f".to_owned();
        field.shown = Some(BLUE);
        field.sync(BLUE, false);
        assert_eq!(field.text, "#0000ff");
    }

    #[test]
    fn a_half_typed_value_survives_until_the_color_moves_elsewhere() {
        let mut field = HexField::default();
        field.sync(RED, false);

        // "#00" parses into nothing: the color stays, and the text is the
        // user's to finish.
        field.text = "#00".to_owned();
        assert_eq!(field.apply_text(RED), None);
        field.sync(RED, true);
        assert_eq!(field.text, "#00");

        // Finishing it applies the color...
        field.text = "#0000ff".to_owned();
        assert_eq!(field.apply_text(RED), Some(BLUE));
        // ...and the now-valid text is not reformatted under the caret.
        field.sync(BLUE, true);
        assert_eq!(field.text, "#0000ff");

        // But a color moved by the palette while the field is focused wins.
        field.sync(RED, true);
        assert_eq!(field.text, "#ff0000");
    }
}
