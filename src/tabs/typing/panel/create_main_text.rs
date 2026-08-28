/*
File: panel/create_main_text.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
the main text-parameter UI. The "Параметры" sub-tab is grouped into collapsible
sections (font / glyph metrics / layout & alignment / shape & smoothing /
typeface style / text processing) drawn by `collapsing_param_section`, followed
by the unchanged advanced-params collapsing header.

Main responsibilities:
- draw the main text params container and its six collapsible sections;
- draw inline per-selection offset controls and alignment controls;
- report how many characters the current inline selection covers;
- own the edge detector (`font_combo_user_pick`) that decides when a font combo
  frame counts as a genuine user pick — the create AND edit panels both use it.

Key structures:
- ParamSectionId: identity + durable storage of one section (`in_tab` /
  `not_persisted`).
- FontSectionGates: the two conditions gating the font section.

Key functions:
- collapsing_param_section() (free fn): one collapsible section with a strong
  title, an optional weak right-aligned summary, and a body; the live state is
  egui memory keyed (id_salt, preview_enabled), the DURABLE state is the drawing
  tab's `TabExtras` (a section outside a dock tab is `not_persisted`).
- section_flag_key() (free fn): the `TabExtras` key of one section's flag.
- draw_main_text_params(): builds the section list and wires the closures.
- draw_font_section / draw_metrics_section / draw_layout_alignment_section /
  draw_shape_render_section / draw_weight_section / draw_text_processing_section:
  the six section bodies (control code moved verbatim from the former
  left/right columns).
- draw_param_identity_block() + draw_param_identity_mode_checkboxes() /
  draw_local_preset_combo() / draw_local_preset_row() /
  draw_local_preset_name_row(): the parameter-identity controls drawn at the top
  of the font section, create panel only (`dev-docs/local_presets_plan.md` §7).
- local_preset_wheel_target() (free fn): where a wheel notch over the closed
  local-preset combo moves the selection — the "create" row is excluded there.
- local_preset_popup_height() (free fn): the popup's row cap, derived from the
  live row pitch.
- local_preset_popup_id_bucket() (free fn): the row-count bucket folded into the
  combo's `id_salt`, which is what lets a changed popup height be re-measured.
- draw_local_preset_image_row() (free fn): one preset row painted as its preview
  on a flat grey backdrop, with an OUTLINE (never a fill) when selected.
- local_preset_row_selection_stroke() / local_preset_row_hover_tint() (free fns):
  the selection outline and the hover cue of one backdrop grey.
- fit_preview_size() (free fn): aspect-preserving cap of a preview image.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so `panel.rs`
and sibling submodules can call them. `use super::*;` pulls in the parent
module's types and imports. Both call sites pass `stacked_columns = true`; the
non-stacked ("wide") branch is DEAD CODE kept only so the file compiles.
*/

use super::*;

// The backdrop grey a local-preset row is painted on, and its selection/hover palette, are
// decided ONCE per requested render in `local_preset_preview` (which owns the luminance
// rule); the row only reads the answer.
use super::local_preset_preview::PreviewBackdrop;

/// Uniform vertical gap after each parameter section, so open and collapsed
/// sections keep the same rhythm.
const PARAM_SECTION_GAP_PX: f32 = 3.0;

/// Height of one local-preset combo row, in logical points.
///
/// The row draws a rendered preview of the preset, so the height is the render target
/// and not just a text line: it is a little taller than the default interact size
/// (`egui::style::Spacing::interact_size.y == 18.0`) so that descenders and a stroke or
/// shadow effect still fit, and small enough that a long list stays scannable.
const LOCAL_PRESET_ROW_HEIGHT_PT: f32 = 22.0;

/// How many rows the local-preset popup shows before it starts to scroll.
///
/// A dozen is enough that an ordinary preset set is visible at a glance, and small enough
/// that the popup cannot outgrow a short screen. Without an explicit cap the popup falls
/// back to `Spacing::combo_height == 200.0` (`egui-0.35.0/src/style.rs:1466`), which is
/// barely three preview rows.
const LOCAL_PRESET_POPUP_MAX_ROWS: u8 = 12;

/// The TEXT rows the local-preset popup always draws before the preset rows: the empty
/// selection and «create a preset».
const LOCAL_PRESET_POPUP_TEXT_ROWS: usize = 2;

/// Corner radius of a preset row's backdrop and selection outline, in points.
const LOCAL_PRESET_ROW_ROUNDING_PT: f32 = 2.0;

/// Selection outline of a preset row drawn over the LIGHT backdrop grey.
///
/// This is the dark blue egui itself uses for a selection stroke in its LIGHT theme
/// (`egui-0.35.0/src/style.rs:1619`). The palette is hard-coded rather than taken from
/// `Visuals::selection`, because the backdrop of a preset row does NOT follow the app
/// theme — it follows the preset's own colours — so the theme's single selection tint
/// would vanish on one of the three greys.
const LOCAL_PRESET_ROW_SELECTED_STROKE_ON_LIGHT: Color32 = Color32::from_rgb(0, 83, 125);

/// Selection outline of a preset row drawn over the MEDIUM or DARK backdrop grey: the
/// light blue of egui's DARK-theme selection stroke (`egui-0.35.0/src/style.rs:1612`).
const LOCAL_PRESET_ROW_SELECTED_STROKE_ON_DARK: Color32 = Color32::from_rgb(192, 222, 255);

/// Width of that selection outline, in points. Two points so the outline still reads as a
/// selection next to the 1 px border the backdrop paints around every row.
const LOCAL_PRESET_ROW_SELECTED_STROKE_WIDTH_PT: f32 = 2.0;

/// Alpha of the hover tint laid over a preset row's backdrop.
///
/// The tint is white over the two darker greys and black over the light one, so the hover
/// cue moves the backdrop by roughly 20-36 luminance points on all three and never
/// disappears into the grey it sits on.
const LOCAL_PRESET_ROW_HOVER_TINT_ALPHA: u8 = 40;

/// The selection outline of a preset row on `backdrop`.
///
/// Chosen by which of the two blues has the greater luminance contrast against that grey,
/// so a selected row is always visible: the dark blue reads on the LIGHT grey (a contrast
/// of ~164), the light blue on the MEDIUM (~90) and DARK (~154) ones. The user's earlier
/// decision is preserved unchanged — the dark blue stays the outline of the LIGHT backdrop.
#[must_use]
fn local_preset_row_selection_stroke(backdrop: PreviewBackdrop) -> Color32 {
    match backdrop {
        PreviewBackdrop::Light => LOCAL_PRESET_ROW_SELECTED_STROKE_ON_LIGHT,
        PreviewBackdrop::Medium | PreviewBackdrop::Dark => LOCAL_PRESET_ROW_SELECTED_STROKE_ON_DARK,
    }
}

/// The hover tint laid over a preset row on `backdrop`.
///
/// A tint of the OPPOSITE value to the grey underneath, so the cue reads on every backdrop:
/// black over the light grey, white over the medium and dark ones. On the MEDIUM grey the
/// two directions are worth the same (~20 luminance points either way); white is chosen so
/// that hovering always brightens on the two darker greys.
#[must_use]
fn local_preset_row_hover_tint(backdrop: PreviewBackdrop) -> Color32 {
    match backdrop {
        PreviewBackdrop::Light => Color32::from_black_alpha(LOCAL_PRESET_ROW_HOVER_TINT_ALPHA),
        PreviewBackdrop::Medium | PreviewBackdrop::Dark => {
            Color32::from_white_alpha(LOCAL_PRESET_ROW_HOVER_TINT_ALPHA)
        }
    }
}

/// What the user asked of the local-preset combo this frame.
///
/// Collected inside the popup closure (which holds `&mut self`) and applied after it, so
/// exactly one operation runs per frame and the borrow of the panel state stays local to
/// the closure.
#[derive(Debug, Clone, Copy)]
enum LocalPresetRowAction {
    /// The «none» row: drop the selection, keeping the parameters on screen.
    Deselect,
    /// The «create» row: a new preset carrying nothing but defaults.
    Create,
    /// A preset row: select it and apply its whole snapshot.
    Select(usize),
}

/// Where a wheel notch over the CLOSED local-preset combo moves the selection.
///
/// The wheel cycles a VIRTUAL list — the empty selection (`0`) plus the real presets
/// (`1 + index`). The popup's «create» row is deliberately NOT in that list, so no amount
/// of scrolling can create a preset; creation stays an explicit click.
///
/// `count` is the number of local presets. Returns `None` when the selection does not move
/// (no steps, or a wrap that lands back on the current row); otherwise `Some(selection)`,
/// whose inner `None` is the empty selection.
#[must_use]
fn local_preset_wheel_target(
    selected: Option<usize>,
    count: usize,
    steps: i32,
) -> Option<Option<usize>> {
    let mut index = selected.map_or(0, |idx| idx + 1);
    if !cycle_wrapped_index(&mut index, count + 1, steps) {
        return None;
    }
    Some(index.checked_sub(1))
}

/// Maximum outer height of the local-preset popup, in logical points.
///
/// `row_count` is the TOTAL number of rows the popup draws (the two text rows plus one per
/// preset) and `row_pitch_pt` the vertical step of ONE preset row — its content height plus
/// the button padding above and below plus the spacing to the next row. Deriving the value
/// from the live pitch rather than from a pixel constant keeps it correct when the theme's
/// spacing or the row height changes.
///
/// The result is a CAP only: `ComboBox::height` becomes `ScrollArea::max_height`
/// (`egui-0.35.0/src/containers/combo_box.rs:393-404`), so a short list never reaches it and
/// does not scroll, while a long one starts scrolling after [`LOCAL_PRESET_POPUP_MAX_ROWS`]
/// rows. The PRESET-row pitch is used for every row because it is the taller kind — the cap
/// is then never tighter than the row count it promises.
#[must_use]
fn local_preset_popup_height(row_count: usize, row_pitch_pt: f32) -> f32 {
    let capped = local_preset_popup_id_bucket(row_count);
    // The bucket above is bounded by `LOCAL_PRESET_POPUP_MAX_ROWS`, so the conversion
    // cannot fail; `unwrap_or` states that without a panicking path.
    let rows = u8::try_from(capped).unwrap_or(LOCAL_PRESET_POPUP_MAX_ROWS);
    f32::from(rows) * row_pitch_pt.max(0.0)
}

/// The row-count bucket that must be folded into the local-preset combo's `id_salt`.
///
/// THE POPUP AREA REMEMBERS ITS SIZE AND CAN ONLY SHRINK. `ComboBox::height` is merely the
/// inner `ScrollArea`'s `max_height` (`egui-0.35.0/src/containers/combo_box.rs:391-404`);
/// the binding cap is the popup `Area`'s STORED size, which is fed back as the body's
/// `max_rect` every frame (`area.rs:610-611`) and written back from the content's own
/// `min_size` (`area.rs:665`), and a sizing pass only ever runs for an id with no stored
/// state (`area.rs:466`). The stored areas are never pruned (`memory/mod.rs:1157`). So one
/// opening at a short row count would pin that height for the whole session — which is
/// exactly what applying a global preset does, because it swaps the live local-preset set
/// wholesale (the EMPTY set of a font-mode preset included).
///
/// Making the id depend on the row count gives every distinct popup HEIGHT its own stored
/// area, so a changed count is re-measured from scratch. The count is bucketed at
/// [`LOCAL_PRESET_POPUP_MAX_ROWS`] because every count above the cap measures identically:
/// that keeps one stored area per distinct height instead of one per preset count.
///
/// `row_count` is the TOTAL number of rows the popup draws, the two text rows included. It
/// is clamped to `1..=LOCAL_PRESET_POPUP_MAX_ROWS`, so the bucket changes EXACTLY when
/// [`local_preset_popup_height`] does — an id that changed less often than the height would
/// leave the defect in place, and one that changed more often would waste a stored area.
/// The same idiom is already used by the font-group combo, which salts with its list.
#[must_use]
fn local_preset_popup_id_bucket(row_count: usize) -> usize {
    row_count.clamp(1, usize::from(LOCAL_PRESET_POPUP_MAX_ROWS))
}

/// Caps a preview image's drawn size to `max_width`, preserving its aspect ratio.
///
/// `size` and `max_width` are both in logical points. The renderer already caps the
/// texture at 320 px wide, which can still overflow a narrow combo popup; scaling by width
/// keeps the whole preset name visible instead of clipping it. A non-finite or
/// non-positive `max_width` (a popup whose width is not known yet) leaves the size alone.
#[must_use]
fn fit_preview_size(size: egui::Vec2, max_width: f32) -> egui::Vec2 {
    if !max_width.is_finite() || max_width <= 0.0 || size.x <= 0.0 || size.x <= max_width {
        return size;
    }
    size * (max_width / size.x)
}

/// Draws one local-preset popup row as its rendered preview and reports whether it was
/// clicked.
///
/// NOT a `selectable_label`: a preview is a TRANSPARENT image, so the row is painted on a
/// flat grey backdrop and a SELECTED row gets an OUTLINE rather than the opaque blue fill a
/// `selectable_label` would lay over the picture.
///
/// `backdrop` decides the row's whole palette — the grey, its border, the hover tint and the
/// outline — so the outline and the hover cue can never sit invisibly on a backdrop of their
/// own value. It is decided ONCE, where the preset's profile is decoded
/// (`local_preset_preview::preview_backdrop`), never here: this runs once per drawn row per
/// frame and must not parse JSON.
///
/// THE ROW HEIGHT IS CONSTANT ([`LOCAL_PRESET_ROW_HEIGHT_PT`]), never the image's own
/// height. A preview is scaled down to fit the popup width, so its height varies from preset
/// to preset; letting the row follow it would make the popup's content height depend on which
/// previews happen to be ready, and a popup `Area` that changes size under a fixed id can
/// only ever SHRINK (see [`local_preset_popup_id_bucket`]). A constant row is also what makes
/// [`local_preset_popup_height`]'s "row pitch" arithmetic true. The image is left-aligned and
/// vertically centred inside the row.
///
/// `label` is the accessibility name only; the drawn content is the image. The row spans the
/// FULL popup width — exactly the footprint the `selectable_label` fill used to have.
fn draw_local_preset_image_row(
    ui: &mut egui::Ui,
    label: &str,
    texture_id: egui::TextureId,
    draw_size: egui::Vec2,
    selected: bool,
    backdrop: PreviewBackdrop,
) -> bool {
    let enabled = ui.is_enabled();
    // Allocated at the popup's full width so that the backdrop, the hover tint, the outline
    // and the click target are ONE rect. `Response::rect` would be the full-width justified
    // rect anyway (`egui-0.35.0/src/ui.rs:1145-1149`); asking for that width explicitly is
    // what makes the painted rect and the hit rect provably the same.
    let row_size = egui::vec2(
        ui.available_width().max(draw_size.x),
        LOCAL_PRESET_ROW_HEIGHT_PT.max(draw_size.y),
    );
    let (rect, response) = ui.allocate_exact_size(row_size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, enabled, selected, label)
    });
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }
    ui.painter()
        .rect_filled(rect, LOCAL_PRESET_ROW_ROUNDING_PT, backdrop.fill());
    // A 1 px border of the backdrop's own family, so the row still reads as its own strip
    // against whatever the popup's background happens to be.
    ui.painter().rect_stroke(
        rect,
        LOCAL_PRESET_ROW_ROUNDING_PT,
        egui::Stroke::new(1.0, backdrop.border()),
        egui::StrokeKind::Inside,
    );
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            LOCAL_PRESET_ROW_ROUNDING_PT,
            local_preset_row_hover_tint(backdrop),
        );
    }
    let image_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - draw_size.y * 0.5),
        draw_size,
    );
    egui::Image::from_texture(egui::load::SizedTexture::new(texture_id, draw_size))
        .paint_at(ui, image_rect);
    if selected {
        // `StrokeKind::Inside` keeps the outline inside the allocated row, so a selected row
        // cannot bleed over its neighbours.
        ui.painter().rect_stroke(
            rect,
            LOCAL_PRESET_ROW_ROUNDING_PT,
            egui::Stroke::new(
                LOCAL_PRESET_ROW_SELECTED_STROKE_WIDTH_PT,
                local_preset_row_selection_stroke(backdrop),
            ),
            egui::StrokeKind::Inside,
        );
    }
    response.clicked()
}

/// The two conditions that gate what the font section offers.
///
/// They travel together because they are read together: one decides whether the
/// per-font parameter memory is consulted, the other disables every control that
/// feeds a render. Grouped rather than passed as two more booleans so the section's
/// parameter list stays readable (and its call sites cannot swap them).
#[derive(Debug, Clone, Copy)]
pub(super) struct FontSectionGates {
    /// Per-font parameter memory is enabled for this panel.
    pub(super) memory_enabled: bool,
    /// The selected/edited layer's font is not loaded: only the font pickers stay
    /// live, everything that would re-render is disabled.
    pub(super) font_missing: bool,
}

/// Decides the font index the user actually PICKED in the font combo this frame,
/// if any — the edge that is allowed to write an inline span's font label.
///
/// - `popup_pick`: the index a popup option click selected (from
///   `SearchableComboResponse::picked`, forwarded by
///   `create_presets::draw_font_combo`). A click always counts as a pick, even on
///   the already-highlighted row (the user explicitly re-affirmed that font).
/// - `wheel`: `(before, after)` font indices around any applied wheel steps. A
///   wheel event counts only when it actually moved the index.
///
/// Returns `None` when nothing changed this frame, so inline-selection writeback
/// stays strictly edge-triggered: merely resolving/clamping `font_idx` per frame
/// never counts as a pick.
pub(super) fn font_combo_user_pick(
    popup_pick: Option<usize>,
    wheel: Option<(usize, usize)>,
) -> Option<usize> {
    if let Some(idx) = popup_pick {
        return Some(idx);
    }
    match wheel {
        Some((before, after)) if before != after => Some(after),
        _ => None,
    }
}

/// Identity and durable storage of one collapsible parameter section.
///
/// The three travel together because they are ONE decision — "which section is
/// this, and where does its expansion state live" — and because a section that
/// took them apart could store its flag under a key that does not match the egui
/// id it draws with. Build it with [`ParamSectionId::in_tab`] (persisted) or
/// [`ParamSectionId::not_persisted`] — a name that has to be typed out, because
/// the two draw identically and only one of them remembers anything.
#[derive(Debug)]
pub(super) struct ParamSectionId<'a> {
    /// Literal persistence key of the section — an i18n exclusion, never a
    /// caption. Feeds both the egui id and [`section_flag_key`].
    id_salt: &'static str,
    /// The panel instance drawing the section: create = `true`, edit = `false`.
    /// Constructor-time, not a runtime toggle, and part of BOTH keys, so the two
    /// panels never share one section state.
    preview_enabled: bool,
    /// The drawing tab's durable bag, or `None` when the section has no tab to
    /// hang state off and is therefore session-only.
    extras: Option<&'a mut TabExtras>,
}

impl<'a> ParamSectionId<'a> {
    /// A section drawn inside a dock tab: its expansion state is stored in that
    /// tab's `extras` and survives a restart.
    #[must_use]
    pub(super) fn in_tab(
        id_salt: &'static str,
        preview_enabled: bool,
        extras: &'a mut TabExtras,
    ) -> Self {
        Self {
            id_salt,
            preview_enabled,
            extras: Some(extras),
        }
    }

    /// A section whose expansion state is DELIBERATELY not persisted: egui
    /// memory only, gone on restart.
    ///
    /// The right choice only where there is nothing to hang the state off — the
    /// section is drawn outside any dock tab, and an `egui::Window` has no
    /// `TabId` whose `TabExtras` could hold the flag (the advanced-form window is
    /// the only such caller today). What is lost is exactly what
    /// [`ParamSectionId::in_tab`] exists for: the section comes back at its
    /// `default_open` in the next session, however the user left it. A section
    /// drawn INSIDE a dock tab must use `in_tab` — this constructor would compile
    /// there, draw identically and silently store nothing.
    #[must_use]
    pub(super) fn not_persisted(id_salt: &'static str, preview_enabled: bool) -> Self {
        Self {
            id_salt,
            preview_enabled,
            extras: None,
        }
    }
}

/// Draws a collapsible parameter section styled as a "header bar + left guide
/// rule".
///
/// The header row (toggle triangle + strong `title` + optional right-aligned
/// weak `summary`) sits on a faint, full-width background bar; the body is
/// drawn indented (`.body`) with a thin, faint vertical guide line down its
/// left edge to signal "these controls belong to the section above". Both the
/// bar and the guide use theme-derived colors (`Visuals::faint_bg_color` and
/// `Visuals::weak_text_color`), so the look is correct in the standard dark
/// theme and hard-codes no literal colors.
///
/// Composition (verified against `egui-0.35.0/src/containers/collapsing_header.rs`):
/// `HeaderResponse` borrows the same `ui` and its `.body(..)` consumes that
/// borrow, so the bar cannot be a `Frame` wrapped around `show_header`. Instead
/// a background shape slot is reserved BEFORE the header
/// (`painter().add(Shape::Noop)`) and filled AFTER, once the header row rect is
/// known, via `painter().set(..)` — so the bar paints behind the already-drawn
/// header. egui's built-in indent vline is suppressed for the body so it never
/// doubles with the guide we paint.
///
/// The LIVE open/closed state is egui memory, keyed
/// `egui::Id::new((id_salt, preview_enabled))`, so the create and edit panels
/// are independent and the state survives a UI-language switch (the id is
/// language independent — see `egui-docs/05-ids-and-i18n.md`). That memory is
/// session-only: this project builds eframe WITHOUT the `persistence` feature.
///
/// `section` carries the identity AND the durable storage of the state — see
/// [`ParamSectionId`]. The visible `title`/`summary` are already-localized
/// strings supplied by the caller; `add_body` paints the section contents when
/// it is open.
pub(super) fn collapsing_param_section(
    ui: &mut egui::Ui,
    section: ParamSectionId<'_>,
    title: &str,
    default_open: bool,
    summary: Option<&str>,
    add_body: impl FnOnce(&mut egui::Ui),
) {
    let ParamSectionId {
        id_salt,
        preview_enabled,
        extras,
    } = section;
    let id = egui::Id::new((id_salt, preview_enabled));
    // The durable default: what the tab stored last, falling back to the caller's
    // `default_open`. egui memory still wins WITHIN a session — `load_with_default_open`
    // only consults this on the first frame, which is exactly the frame that has to
    // reproduce the previous run.
    let flag_key = section_flag_key(id_salt, preview_enabled);
    let stored_open = extras
        .as_ref()
        .map_or(default_open, |extras| extras.flag(&flag_key, default_open));

    // Full-width horizontal extent for the header bar: the section spans the
    // panel width even though the header ROW only sizes to its own content.
    let bar_x_range = ui.max_rect().x_range();
    // Reserve a slot for the bar BEFORE the header so it can be filled in behind
    // the toggle/title/summary once the header row rect is known.
    let bar_idx = ui.painter().add(egui::Shape::Noop);

    // We paint our own guide line; suppress egui's built-in indent vline for
    // this section (restored right after) so the two never double up.
    let prev_indent_vline = ui.visuals().indent_has_left_vline;
    ui.visuals_mut().indent_has_left_vline = false;

    let (_toggle, header_inner, body_inner) =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, stored_open)
            .show_header(ui, |ui| {
                ui.label(egui::RichText::new(title).strong());
                if let Some(summary) = summary {
                    // Right-aligned, weak (faint) summary of the section's
                    // current state; skipped when there is no compact summary.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(summary);
                    });
                }
            })
            // `body` (indented) so the contents sit visually under the header.
            .body(add_body);

    ui.visuals_mut().indent_has_left_vline = prev_indent_vline;

    // Faint full-width header bar behind the header row, with a little vertical
    // padding so the bar has some height around the text.
    let header_rect = header_inner.response.rect;
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(bar_x_range.min, header_rect.top() - 2.0),
        egui::pos2(bar_x_range.max, header_rect.bottom() + 2.0),
    );
    ui.painter().set(
        bar_idx,
        egui::Shape::rect_filled(bar_rect, 3.0, ui.visuals().faint_bg_color),
    );

    // Thin, faint vertical guide line along the left of the indented body
    // (present only while the section is open / animating).
    if let Some(body) = body_inner {
        let body_rect = body.response.rect;
        let indent = ui.spacing().indent;
        let guide_x = body_rect.left() - 0.5 * indent;
        let guide_stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color());
        ui.painter().vline(guide_x, body_rect.y_range(), guide_stroke);
    }

    // Write back what the header NOW shows: `show_header(..).body(..)` stores the
    // post-toggle state in egui memory on every path
    // (`egui-0.35.0/src/containers/collapsing_header.rs:186,235`), so reloading it
    // here is what the user just saw. Writing every frame is the expected usage —
    // `set_flag` raises `changed` only on a real difference, so an untouched
    // section never marks the dock dirty.
    if let Some(extras) = extras {
        let shown_open = egui::collapsing_header::CollapsingState::load(ui.ctx(), id)
            .map_or(stored_open, |state| state.is_open());
        extras.set_flag(&flag_key, shown_open, default_open);
    }

    ui.add_space(PARAM_SECTION_GAP_PX);
}

/// Durable key of one parameter section's expanded/collapsed flag inside its
/// tab's [`TabExtras`].
///
/// `"{id_salt}#create"` / `"{id_salt}#edit"`: `preview_enabled` is the
/// constructor-time discriminator of the two panel instances (create = `true`,
/// edit = `false`), not a runtime toggle, so it MUST stay part of the key —
/// without it the two panels would share one stored flag. `id_salt` is a literal
/// persistence key, never a caption, so the result is language independent.
#[must_use]
pub(super) fn section_flag_key(id_salt: &str, preview_enabled: bool) -> String {
    let panel = if preview_enabled { "create" } else { "edit" };
    format!("{id_salt}#{panel}")
}

impl TypingCreatePanelState {

    /// Draws the six main text-parameter sections plus the advanced-params
    /// header, and reports whether anything the render depends on changed.
    ///
    /// `extras` is the «Параметры» dock tab's persisted state: every section
    /// drawn here reads its expanded/collapsed flag out of it and writes back
    /// what it shows (see [`collapsing_param_section`]). `gates` are the font
    /// section's two conditions, passed through untouched; `stacked_columns =
    /// false` is the dead "wide" path kept only so the file compiles.
    pub(super) fn draw_main_text_params(
        &mut self,
        ui: &mut egui::Ui,
        extras: &mut TabExtras,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
        presets: &mut ColorPresetsBinding<'_>,
        gates: FontSectionGates,
    ) -> bool {
        let FontSectionGates {
            memory_enabled: font_memory_enabled,
            font_missing,
        } = gates;
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        let inline_selection = if self.preview_enabled {
            None
        } else {
            self.inline_selection_context()
        };
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            // Precompute the per-section header summaries as a frame-start snapshot
            // of `self`. They are read-only borrows that end before the section
            // bodies mutate `self`; immediate-mode redraw catches up next frame.
            let preview_enabled = self.preview_enabled;
            let font_label = self
                .fonts
                .get(self.selected_font_idx)
                .map(|font| self.font_display_label(font))
                .unwrap_or_default();
            let font_summary = format!("{} · {}px", font_label, self.font_size_px.round() as i32);
            let layout_summary = match self.text_layout_mode {
                TextLayoutMode::Normal => t!("typing.advanced.layout_kind_standard"),
                TextLayoutMode::Formula => t!("typing.advanced.layout_kind_formula"),
                TextLayoutMode::Shape => t!("typing.advanced.layout_kind_shape"),
                TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines => {
                    t!("typing.advanced.layout_kind_vector_lines")
                }
            };
            let shape_label = match self.text_shape {
                TextShape::Free => t!("typing.params.shape_free_option"),
                TextShape::Rectangle => "[  ]",
                TextShape::Oval => "(  )",
                TextShape::Hexagon => "<  >",
                TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
            };
            let shape_summary = format!("{} · {}", shape_label, anti_aliasing_label(self.anti_aliasing));
            let enabled_count = usize::from(self.hanging_punctuation)
                + usize::from(self.trim_extra_spaces)
                + usize::from(self.replace_ellipsis_with_dots)
                + usize::from(self.new_line_after_sentence)
                + usize::from(self.uppercase_text)
                + usize::from(self.enable_inline_style_tags);
            let text_processing_summary = tf!("typing.section.enabled_count", count = enabled_count);

            if stacked_columns {
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.font", preview_enabled, extras),
                    t!("typing.params.font_label"),
                    true,
                    Some(font_summary.as_str()),
                    |ui| {
                        self.draw_font_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            presets,
                            FontSectionGates {
                                memory_enabled: font_memory_enabled,
                                font_missing,
                            },
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.metrics", preview_enabled, extras),
                    t!("typing.section.metrics"),
                    true,
                    None,
                    |ui| {
                        self.draw_metrics_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.layout", preview_enabled, extras),
                    t!("typing.section.layout"),
                    true,
                    Some(layout_summary),
                    |ui| {
                        self.draw_layout_alignment_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.shape", preview_enabled, extras),
                    t!("typing.section.shape"),
                    false,
                    Some(shape_summary.as_str()),
                    |ui| {
                        self.draw_shape_render_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.weight", preview_enabled, extras),
                    t!("typing.section.weight"),
                    false,
                    None,
                    |ui| {
                        self.draw_weight_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    ParamSectionId::in_tab("typing.section.text_processing", preview_enabled, extras),
                    t!("typing.section.text_processing"),
                    false,
                    Some(text_processing_summary.as_str()),
                    |ui| {
                        self.draw_text_processing_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );

                // The advanced-params collapsing header keeps its original gating:
                // disabled while a font is missing (blocks re-render) and while an
                // inline selection is active. Its own contents are unchanged.
                ui.add_enabled_ui(!font_missing, |ui| {
                    ui.add_enabled_ui(!selection_mode, |ui| {
                        self.draw_advanced_text_params_section(
                            ui,
                            extras,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            "typing_advanced_text_params_right_column",
                        );
                    });
                });
            } else {
                // DEAD non-stacked ("wide") path: both call sites pass
                // `stacked_columns = true`, so this branch is never reached at
                // runtime. It is kept behavior-neutral and compiling by drawing the
                // same sections FLAT (no collapsibles) in order.
                self.draw_font_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    presets,
                    FontSectionGates {
                        memory_enabled: font_memory_enabled,
                        font_missing,
                    },
                );
                self.draw_metrics_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_layout_alignment_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_shape_render_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_weight_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_text_processing_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                ui.add_enabled_ui(!font_missing, |ui| {
                    ui.add_enabled_ui(!selection_mode, |ui| {
                        self.draw_advanced_text_params_section(
                            ui,
                            extras,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            "typing_advanced_text_params_right_column",
                        );
                    });
                });
            }

            // Extra bottom padding so the horizontal scrollbar doesn't overlap the last checkbox text.
            ui.add_space(ui.spacing().scroll.allocated_width() + 4.0);
        });

        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        if let (Some(selection), Some(style)) = (inline_selection, inline_style) {
            changed |= self.apply_inline_style_to_selection(selection, style);
        }
        changed
    }

    /// Font section (default open): font-group / font / face selectors, the
    /// missing-font hint, and the color + size controls. The group/font/hint stay
    /// enabled even when a font is missing so the user can pick a replacement; the
    /// face selector is gated on `!selection_mode`; color + size are gated on
    /// `!font_missing`. `presets` decides whether the color swatch opens the
    /// title-scoped preset picker or the stock egui palette. Control code moved
    /// verbatim from the former panel body and left column.
    pub(super) fn draw_font_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
        presets: &mut ColorPresetsBinding<'_>,
        gates: FontSectionGates,
    ) {
        let FontSectionGates {
            memory_enabled: font_memory_enabled,
            font_missing,
        } = gates;
        let selection_mode = inline_style.is_some();
        // The parameter-identity switch and the local-preset controls it reveals
        // (`dev-docs/local_presets_plan.md` §7). Create panel only — the edit panel's
        // parameters belong to the edited layer, never to a font or a local preset. Gated
        // on `!selection_mode` like the face combo below: with an inline selection every
        // parameter widget edits the SPAN, so applying a local preset's whole snapshot to
        // the panel would not be the operation the user asked for.
        if self.preview_enabled {
            ui.add_enabled_ui(!selection_mode, |ui| {
                self.draw_param_identity_block(ui, block_hscroll_by_hovered_param);
            });
        }
        // Комбобокс группы шрифтов показывается на обеих панелях (создание и
        // редактирование); выбор синхронизируется между ними через
        // `pending_font_group_request` (см. обработку во внешнем цикле).
        {
            let mut selected_group_idx = self
                .selected_font_group
                .as_ref()
                .and_then(|selected| {
                    self.font_groups.iter().position(|group| group == selected)
                })
                .map_or(0usize, |idx| idx + 1);
            let group_count = self.font_groups.len() + 1;
            let selected_group_text =
                self.selected_font_group.as_deref().unwrap_or(t!("typing.params.font_group_all"));
            // Same remembered-popup-size trap as the font combo below: salt with the
            // group list so adding/removing groups re-measures the popup height.
            // The combo and its inline "?" deep-link help icon share one row.
            let (group_combo, help_go_clicked) = ui
                .horizontal(|ui| {
                    let group_combo = WheelComboBox::from_label(t!("typing.create.font_group_combo_id")).id_salt(("typing.create.font_group_combo_id", &self.font_groups))
                        .selected_text(selected_group_text)
                        .show_ui_with_wheel(ui, |ui| {
                            ui.selectable_value(&mut selected_group_idx, 0usize, t!("typing.params.font_group_all"));
                            for (idx, group_name) in self.font_groups.iter().enumerate() {
                                ui.selectable_value(&mut selected_group_idx, idx + 1, group_name);
                            }
                        });
                    // "?" icon whose hover tooltip explains font groups and carries a
                    // "Перейти" button; clicking it requests a deep link to the settings
                    // font-groups block (drained by the outer facade loop).
                    let help_go_clicked = crate::widgets::HelpHint::text(t!("typing.params.font_group_help"))
                        .with_action(t!("typing.params.font_group_help_go"))
                        .show_with_action(ui)
                        .action_clicked;
                    (group_combo, help_go_clicked)
                })
                .inner;
            if help_go_clicked {
                self.pending_settings_link_request =
                    Some(crate::settings_shared::SettingsDeepLink::TypesettingFontGroups);
            }
            mark_hscroll_block_on_hover(
                block_hscroll_by_hovered_param,
                &group_combo.inner.response,
            );
            if let Some(steps) = group_combo.wheel_steps {
                cycle_wrapped_index(&mut selected_group_idx, group_count, steps);
            }
            let previous_group = self.selected_font_group.clone();
            self.selected_font_group = if selected_group_idx == 0 {
                None
            } else {
                self.font_groups.get(selected_group_idx - 1).cloned()
            };
            if self.selected_font_group != previous_group {
                self.ensure_selected_font_in_group();
                self.pending_font_group_request = Some(self.selected_font_group.clone());
                *changed = true;
            }
        }

        let prev_font_idx = self.selected_font_idx;
        // The label the combo draws after its button; the SALT is the same key, but as a
        // stable literal, so a language switch cannot drop the popup's state.
        let font_combo_label = t!("typing.create.font_combo_id");
        let outcome = self.draw_font_combo(
            ui,
            &create_presets::FontComboSpec {
                id_salt: "typing.create.font_combo_id",
                label: font_combo_label,
                width: create_presets::font_combo_button_width(ui, font_combo_label, 0.0),
                inline_font_label: inline_style
                    .as_deref()
                    .and_then(|style| style.font_label.as_deref()),
                font_missing,
            },
        );
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &outcome.response);
        if let Some(style) = inline_style.as_mut() {
            // Edge-triggered writeback (mirrors the non-selection branch's
            // `font_idx != prev_font_idx` guard): only a real pick this frame
            // writes the span font label, so selecting text can never insert a
            // `<font>` tag on its own.
            if let Some(picked) = outcome.user_pick
                && let Some(label) = self.font_identity_name_by_idx(picked)
            {
                style.font_label = Some(label);
            }
        } else {
            self.selected_font_idx = outcome.font_idx;
            if self.selected_font_idx != prev_font_idx {
                // Любой выбор из списка — это доступный шрифт, поэтому снимаем
                // блокировку рендера по ненайденному шрифту.
                self.missing_font = None;
                if font_memory_enabled {
                    *changed |= self.handle_create_font_selection_change(prev_font_idx);
                } else {
                    self.selected_face_idx = 0;
                    *changed = true;
                }
            }
        }

        if font_missing {
            ui.colored_label(
                Color32::from_rgb(240, 200, 60),
                t!("typing.params.pick_available_font_hint"),
            );
        }

        // A font with one face (or none) offers no choice, so the combo is not drawn at all:
        // it could only ever re-select the face already selected. Nothing is lost by hiding
        // it — `selectable_value` cannot move a one-item list, `cycle_wrapped_index` returns
        // early for `len <= 1` (`ui_helpers.rs:366-383`), and the write-back would store the
        // value it just read. `clamp_face_index` still runs, so a `selected_face_idx` left
        // over from a font with more faces cannot survive behind the hidden widget.
        let face_count = self
            .fonts
            .get(self.selected_font_idx)
            .map(|font| font.faces.len())
            .unwrap_or(0);
        if face_count > 1 {
            ui.add_enabled_ui(!selection_mode, |ui| {
                let prev_face_idx = self.selected_face_idx;
                let selected_face_text = self
                    .fonts
                    .get(self.selected_font_idx)
                    .and_then(|font| font.faces.get(self.selected_face_idx))
                    .map(|face| face.label.as_str())
                    .unwrap_or("<face>");
                let mut face_idx = self.selected_face_idx;
                let face_combo = WheelComboBox::from_label("Face")
                    .selected_text(selected_face_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if let Some(font) = self.fonts.get(self.selected_font_idx) {
                            for (idx, face) in font.faces.iter().enumerate() {
                                ui.selectable_value(&mut face_idx, idx, &face.label);
                            }
                        }
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &face_combo.inner.response,
                );
                if let Some(steps) = face_combo.wheel_steps {
                    cycle_wrapped_index(&mut face_idx, face_count, steps);
                }
                self.selected_face_idx = face_idx;
                if self.selected_face_idx != prev_face_idx {
                    *changed = true;
                }
            });
        } else {
            let prev_face_idx = self.selected_face_idx;
            self.clamp_face_index();
            if self.selected_face_idx != prev_face_idx {
                *changed = true;
            }
        }

        // Остальные параметры влияют на рендер: при ненайденном шрифте они
        // блокируются, доступным остаётся только выбор шрифта выше.
        ui.add_enabled_ui(!font_missing, |ui| {
            if let Some(style) = inline_style.as_mut() {
                let mut text_color = style.text_color.unwrap_or(self.text_color);
                *changed |=
                    presets.draw_selector(ui, &mut self.text_color_selector, &mut text_color);
                style.text_color = Some(text_color);
                let mut font_size_px = style
                    .font_size_px
                    .unwrap_or(self.font_size_px)
                    .clamp(1.0, 256.0);
                let font_size_resp = ui.add(
                    WheelSlider::new(&mut font_size_px, 1.0..=256.0)
                        .text(t!("typing.params.size_px_label"))
                        .wheel_step(1.0),
                );
                *changed |= font_size_resp.changed();
                style.font_size_px = Some(font_size_px);
            } else {
                *changed |=
                    presets.draw_selector(ui, &mut self.text_color_selector, &mut self.text_color);
                let font_size_resp = ui.add(
                    WheelSlider::new(&mut self.font_size_px, 1.0..=256.0)
                        .text(t!("typing.params.size_px_label"))
                        .wheel_step(1.0),
                );
                *changed |= font_size_resp.changed();
            }
        });
    }

    /// Draws the create panel's parameter-identity controls: the mode checkbox pair and,
    /// in [`ParamIdentityMode::LocalPreset`] mode, the local-preset combo and the rename +
    /// delete row (`dev-docs/local_presets_plan.md` §7).
    ///
    /// Raises no `changed` flag on purpose. Switching the mode must change NOTHING on
    /// screen (plan §2, fixed decision 4), and every local-preset operation stores its own
    /// snapshot and queues its own preview render, so re-running the caller's
    /// store-and-render pass would only duplicate work — and, for the mode switch, would
    /// hand the current parameters to the incoming owner behind the user's back.
    pub(super) fn draw_param_identity_block(
        &mut self,
        ui: &mut egui::Ui,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        self.draw_param_identity_mode_checkboxes(ui, block_hscroll_by_hovered_param);
        if self.identity_mode != ParamIdentityMode::LocalPreset {
            return;
        }
        self.draw_local_preset_combo(ui, block_hscroll_by_hovered_param);
        // Drawn for its own sake; the returned `Response` only exists so a test can pin the
        // enabled state of the row.
        self.draw_local_preset_name_row(ui);
    }

    /// The identity-mode switch: a caption plus a MUTUALLY EXCLUSIVE PAIR of checkboxes and
    /// the help hint, all on one row — `Идентичность: [ ] Шрифт  [x] Локальный пресет`.
    ///
    /// EXACTLY ONE of the two is always checked, because the panel must never end up owning
    /// its parameters by nobody: clicking the UNCHECKED box switches the mode, and clicking
    /// the CHECKED one is a no-op. A pair states the two owners by name where a single
    /// checkbox only named one of them and left the other implicit.
    ///
    /// Neither box carries an `id_salt` on purpose — `Checkbox` takes its id from
    /// `Ui::next_auto_id` and not from the label text
    /// (`egui-0.35.0/src/widgets/checkbox.rs:73`), so a live language switch cannot reset it.
    ///
    /// `egui::Checkbox` flips the `bool` it is given BEFORE it paints
    /// (`egui-0.35.0/src/widgets/checkbox.rs:101-103`), so a click on the CHECKED box paints
    /// it unchecked for the remainder of that one frame; the state is re-derived from
    /// `identity_mode` on the next frame, which the click itself has already scheduled. The
    /// MODE never changes, which is the contract that matters.
    fn draw_param_identity_mode_checkboxes(
        &mut self,
        ui: &mut egui::Ui,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let current = self.identity_mode;
        // Collected inside the row and applied after it: only a checkbox that became CHECKED
        // asks for a switch, so unchecking the active one requests nothing at all.
        let mut requested: Option<ParamIdentityMode> = None;
        let responses = ui
            .horizontal(|ui| {
                ui.label(t!("typing.local_presets.mode_label"));
                let mut font_checked = current == ParamIdentityMode::Font;
                let font_resp = ui.checkbox(&mut font_checked, t!("typing.local_presets.mode_font"));
                if font_resp.changed() && font_checked {
                    requested = Some(ParamIdentityMode::Font);
                }
                let mut preset_checked = current == ParamIdentityMode::LocalPreset;
                let preset_resp = ui.checkbox(
                    &mut preset_checked,
                    t!("typing.local_presets.mode_preset"),
                );
                if preset_resp.changed() && preset_checked {
                    requested = Some(ParamIdentityMode::LocalPreset);
                }
                crate::widgets::HelpHint::text(t!("typing.local_presets.mode_help")).show(ui);
                [font_resp, preset_resp]
            })
            .inner;
        for response in &responses {
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, response);
        }
        if let Some(mode) = requested
            && mode != current
        {
            self.set_param_identity_mode(mode);
        }
    }

    /// The local-preset combo: the empty selection, the «create» row, then one row per
    /// local preset drawn as its rendered preview image.
    ///
    /// THE WHEEL NEVER CREATES A PRESET. Wheel steps go through
    /// [`local_preset_wheel_target`], which cycles a virtual list holding only the empty
    /// selection and the real presets; the «create» row exists in the popup alone. Cycling
    /// into a creation would silently grow the user's set on a stray scroll.
    ///
    /// The CLOSED combo shows the selected preset's NAME as text — `WheelComboBox` takes a
    /// string for its button caption, and extending it is out of scope (plan §3, D5).
    fn draw_local_preset_combo(
        &mut self,
        ui: &mut egui::Ui,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let selected_text = self
            .selected_local_preset
            .and_then(|index| self.local_preset_display_name(index))
            .unwrap_or_else(|| t!("typing.local_presets.none_option").to_string());
        // The renderer downscales to PHYSICAL pixels, so the row height is converted here.
        let row_height_px = LOCAL_PRESET_ROW_HEIGHT_PT * ui.ctx().pixels_per_point();
        let preset_count = self.local_presets.len();
        // The pitch of one PRESET row: its content height plus the button padding on both
        // sides plus the gap to the next row. Read from the live style, so the popup keeps
        // showing the promised number of rows when the theme's spacing changes.
        let row_pitch_pt = LOCAL_PRESET_ROW_HEIGHT_PT
            + 2.0 * ui.spacing().button_padding.y
            + ui.spacing().item_spacing.y;
        let row_count = preset_count + LOCAL_PRESET_POPUP_TEXT_ROWS;
        let popup_height = local_preset_popup_height(row_count, row_pitch_pt);
        let mut action: Option<LocalPresetRowAction> = None;
        // The row-count bucket is part of the id: a popup `Area` remembers its size under a
        // fixed id and can only ever shrink, so a constant salt pins the height measured at
        // the SHORTEST row count the popup was ever opened at — see
        // `local_preset_popup_id_bucket`. Applying a global preset swaps the live set
        // wholesale, which is exactly how that short opening happens in practice.
        let preset_combo = WheelComboBox::from_label(t!("typing.local_presets.combo_id"))
            .id_salt((
                "typing.local_presets.combo_id",
                local_preset_popup_id_bucket(row_count),
            ))
            .selected_text(selected_text)
            .height(popup_height)
            .show_ui_with_wheel(ui, |ui| {
                if ui
                    .selectable_label(
                        self.selected_local_preset.is_none(),
                        t!("typing.local_presets.none_option"),
                    )
                    .clicked()
                {
                    action = Some(LocalPresetRowAction::Deselect);
                }
                // Never drawn as "selected": creating leaves the NEW preset selected, and
                // this row is an action, not a state.
                if ui
                    .selectable_label(false, t!("typing.local_presets.create_option"))
                    .clicked()
                {
                    action = Some(LocalPresetRowAction::Create);
                }
                for index in 0..preset_count {
                    if self.draw_local_preset_row(ui, index, row_height_px) {
                        action = Some(LocalPresetRowAction::Select(index));
                    }
                }
            });
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &preset_combo.inner.response);
        // Wheel steps are only reported while the popup is CLOSED, so they can never race
        // with a click collected above.
        if let Some(steps) = preset_combo.wheel_steps
            && let Some(target) =
                local_preset_wheel_target(self.selected_local_preset, preset_count, steps)
        {
            action = Some(match target {
                Some(index) => LocalPresetRowAction::Select(index),
                None => LocalPresetRowAction::Deselect,
            });
        }
        // Applied after the closure: each operation needs `&mut self`, and all three store
        // the outgoing snapshot and queue their own preview render themselves.
        match action {
            Some(LocalPresetRowAction::Deselect) => self.deselect_local_preset(),
            Some(LocalPresetRowAction::Create) => self.create_local_preset(),
            Some(LocalPresetRowAction::Select(index)) => self.select_local_preset(index),
            None => {}
        }
    }

    /// Draws one local-preset row of the popup and reports whether it was clicked.
    ///
    /// The row is the preset's RENDERED PREVIEW when the off-thread renderer has one. A
    /// `Pending` or `Failed` preview falls back to the same capped label the image would
    /// have drawn ([`local_preset_preview::preview_label`]), so a row is never blank and
    /// never unclickable — a preset whose font is not installed stays selectable.
    ///
    /// An IMAGE row is drawn by [`draw_local_preset_image_row`], on a flat grey backdrop and
    /// with an OUTLINE when selected. The `Pending` / `Failed` fallback is a selectable
    /// BUTTON rather than a `selectable_label` for one reason: it must occupy exactly
    /// [`LOCAL_PRESET_ROW_HEIGHT_PT`], the image row's height, and a bare `selectable_label`
    /// is only `interact_size.y` (18 pt) tall. The FIRST opening of the popup always happens
    /// while every preview is still `Pending`, so a shorter fallback row would pin the
    /// popup's remembered `Area` height ~20 % short for that row count — permanently, since
    /// that height can only shrink (see [`local_preset_popup_id_bucket`]).
    ///
    /// `row_height_px` is the row height in PHYSICAL pixels: the size the renderer
    /// downscales to, and part of the preview cache key.
    fn draw_local_preset_row(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        row_height_px: f32,
    ) -> bool {
        let selected = self.selected_local_preset == Some(index);
        let label =
            local_preset_preview::preview_label(&self.local_preset_display_name(index).unwrap_or_default());
        // The preview's `size` is in pixels; the row is laid out in points.
        let points_per_pixel = 1.0 / ui.ctx().pixels_per_point().max(f32::EPSILON);
        let max_width = ui.available_width();
        let Some(preview) = self.local_preset_row_preview(index, row_height_px) else {
            // Out of range. The caller iterates the live list, so this cannot happen from
            // the UI; drawing nothing is still the only honest answer.
            return false;
        };
        match preview {
            local_preset_preview::LocalPresetPreview::Ready {
                texture,
                size,
                backdrop,
            } => {
                let draw_size = fit_preview_size(size * points_per_pixel, max_width);
                let texture_id = texture.id();
                draw_local_preset_image_row(ui, &label, texture_id, draw_size, selected, backdrop)
            }
            local_preset_preview::LocalPresetPreview::Pending
            | local_preset_preview::LocalPresetPreview::Failed => ui
                .add_sized(
                    [max_width, LOCAL_PRESET_ROW_HEIGHT_PT],
                    egui::Button::selectable(selected, label),
                )
                .clicked(),
        }
    }

    /// The rename box and delete button of the selected local preset.
    ///
    /// ALWAYS DRAWN in local-preset mode, and DISABLED while nothing is selected rather than
    /// hidden: the row is part of the mode's layout, and hiding it made the whole font
    /// section jump by a row on every select/deselect. Returns the name box's `Response`,
    /// whose `enabled()` is that state.
    ///
    /// The name is written back VERBATIM on every change — [`Self::rename_local_preset`]
    /// never trims, folds or de-duplicates it, because the name is not a key: a local preset
    /// is SELECTED by its index (plan §3, D3) and IDENTIFIED by its stable id
    /// (`LocalPreset::id`), so two presets may carry the same name and a rename changes
    /// nothing but the caption. The box is registered with the panel's focus tracking, so the
    /// typing tab's hotkeys stay quiet while the user types a name into it.
    fn draw_local_preset_name_row(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let selected = self.selected_local_preset;
        ui.add_enabled_ui(selected.is_some(), |ui| {
            ui.horizontal(|ui| {
                let name_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.local_preset_name_input)
                        .id_salt("typing.local_presets.name_input")
                        .hint_text(t!("typing.local_presets.name_hint"))
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                self.track_text_input(&name_resp);
                let delete_clicked = ui
                    .button(t!("typing.local_presets.delete_button"))
                    .clicked();
                // `selected` gates both actions: a disabled widget reports no change and no
                // click, but the index is what makes that unmistakable at the call site.
                if let Some(selected) = selected {
                    if name_resp.changed() {
                        let name = self.local_preset_name_input.clone();
                        self.rename_local_preset(selected, name);
                    }
                    if delete_clicked {
                        self.delete_local_preset(selected);
                    }
                }
                name_resp
            })
            .inner
        })
        .inner
    }

    /// Glyph-metrics section (default open, gated on `!font_missing`): line
    /// spacing, kerning mode + value, glyph height/width, and (in an inline
    /// selection) the per-selection offset controls. Moved verbatim from the
    /// former left column.
    pub(super) fn draw_metrics_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            let base_font_size_px = self.font_size_px.max(1.0);
            if let Some(style) = inline_style.as_mut() {
                let inline_font_size_px = style.font_size_px.unwrap_or(base_font_size_px).max(1.0);
                let mut line_spacing = style.line_spacing.unwrap_or(self.line_spacing);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.line_spacing_label"),
                    &mut line_spacing,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::LINE_SPACING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.line_spacing = Some(line_spacing);

                ui.horizontal(|ui| {
                    ui.label(t!("typing.params.kerning_label"));
                    // Read-only indicator of the global kerning mode (kerning is not a
                    // per-span inline override). Optical is not offered as a choice.
                    ui.add_enabled(
                        false,
                        egui::Button::new(t!("typing.params.kerning_metric"))
                            .selected(self.kerning_mode == KerningMode::Fixed),
                    );
                    ui.add_enabled(
                        false,
                        egui::Button::new(t!("typing.params.kerning_auto"))
                            .selected(self.kerning_mode == KerningMode::Auto),
                    );
                });
                let mut kerning = style.kerning.unwrap_or(self.kerning);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.kerning_label"),
                    &mut kerning,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::KERNING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.kerning = Some(kerning);

                let mut stretching = style
                    .glyph_stretching
                    .unwrap_or([self.glyph_width, self.glyph_height]);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_height_label"),
                    &mut stretching[1],
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_HEIGHT),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_width_label"),
                    &mut stretching[0],
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_WIDTH),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.glyph_stretching = Some(stretching);
            } else {
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.line_spacing_label"),
                    &mut self.line_spacing,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::LINE_SPACING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                ui.horizontal(|ui| {
                    ui.label(t!("typing.params.kerning_label"));
                    // Optical is implemented but intentionally not offered here; only
                    // Fixed ("Метрический") and Auto ("Авто") are user-selectable.
                    *changed |= ui
                        .selectable_value(&mut self.kerning_mode, KerningMode::Fixed, t!("typing.params.kerning_metric"))
                        .changed();
                    *changed |= ui
                        .selectable_value(&mut self.kerning_mode, KerningMode::Auto, t!("typing.params.kerning_auto"))
                        .changed();
                });

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.kerning_label"),
                    &mut self.kerning,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::KERNING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_height_label"),
                    &mut self.glyph_height,
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_HEIGHT),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_width_label"),
                    &mut self.glyph_width,
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_WIDTH),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
            }

            if selection_mode {
                self.draw_inline_offset_controls(
                    ui,
                    changed,
                    block_hscroll_by_hovered_param,
                    inline_style,
                );
            }
        });
    }

    /// Layout & alignment section (default open, gated on `!font_missing`): the
    /// global alignment controls, global rotation, and — for line-based layouts —
    /// line placement and the placement-reference combo (all gated on
    /// `!selection_mode`), plus the per-selection alignment controls when an
    /// inline selection is active. Moved verbatim from the former right column.
    pub(super) fn draw_layout_alignment_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                Self::draw_alignment_controls(
                    ui,
                    &mut self.align,
                    changed,
                    block_hscroll_by_hovered_param,
                );

                // Глобальный поворот всего блока: применяется к векторным контурам
                // глифов ДО растеризации, поэтому получается чётче, чем поворот уже
                // готового растра оверлея.
                ui.horizontal(|ui| {
                    // Deliberately two tooltips: the slider keeps its existing
                    // text tooltip, the "?" icon plays the animated hint.
                    let rotation_resp = ui
                        .add(
                            WheelSlider::new(&mut self.global_rotation_deg, -180.0..=180.0)
                                .text(t!("typing.params.global_rotation_label"))
                                .wheel_step(1.0),
                        )
                        .on_hover_text(t!("typing.params.global_rotation_tooltip"));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &rotation_resp);
                    *changed |= rotation_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &rotation_resp) {
                        *changed |=
                            apply_wheel_step_f32(&mut self.global_rotation_deg, steps, 1.0, -180.0, 180.0);
                    }
                    crate::widgets::HelpHint::animated(ms_gifs::typing::GLOBAL_ROTATION).show(ui);
                });

                // Размещение по линии: перпендикулярный сдвиг глифов относительно
                // линии/пути. Показывается только для линейных раскладок (формула и
                // векторные линии); для остальных режимов параметр скрыт и игнорируется
                // рендером.
                if matches!(
                    self.text_layout_mode,
                    TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines
                ) {
                    ui.horizontal(|ui| {
                        let placement_resp = ui
                            .add(
                                WheelSlider::new(&mut self.line_placement_percent, -100.0..=100.0)
                                    .text(t!("typing.params.line_placement_label"))
                                    .wheel_step(5.0),
                            )
                            .on_hover_text(
                                t!("typing.params.line_placement_tooltip"),
                            );
                        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &placement_resp);
                        *changed |= placement_resp.changed();
                        if let Some(steps) = wheel_steps_if_hovered(ui, &placement_resp) {
                            *changed |= apply_wheel_step_f32(
                                &mut self.line_placement_percent,
                                steps,
                                5.0,
                                -100.0,
                                100.0,
                            );
                        }

                        if ui.button("⬇").on_hover_text(t!("typing.params.line_placement_bottom")).clicked() {
                            self.line_placement_percent = -100.0;
                            *changed = true;
                        }
                        if ui.button("⬍").on_hover_text(t!("typing.params.line_placement_center")).clicked() {
                            self.line_placement_percent = 0.0;
                            *changed = true;
                        }
                        if ui.button("⬆").on_hover_text(t!("typing.params.line_placement_top")).clicked() {
                            self.line_placement_percent = 100.0;
                            *changed = true;
                        }
                    });
                }

                // Опора размещения: к чему привязывается «Размещение по линии» —
                // к общей строке (все символы на единой базовой линии, ровная изогнутая
                // строка) или к фактической высоте каждого символа (легаси, символы
                // «прыгают»). Только для векторных линий; формула этот режим не использует.
                if self.text_layout_mode == TextLayoutMode::CustomVectorLines {
                    let prev_reference = self.line_placement_reference;
                    let reference_combo = WheelComboBox::from_label(
                        t!("typing.params.line_placement_reference_label"),
                    )
                    .id_salt("typing.params.line_placement_reference")
                    .selected_text(match self.line_placement_reference {
                        LinePlacementReference::LineBox => {
                            t!("typing.params.line_placement_reference_line")
                        }
                        LinePlacementReference::GlyphHeight => {
                            t!("typing.params.line_placement_reference_glyph")
                        }
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.line_placement_reference,
                            LinePlacementReference::LineBox,
                            t!("typing.params.line_placement_reference_line"),
                        );
                        ui.selectable_value(
                            &mut self.line_placement_reference,
                            LinePlacementReference::GlyphHeight,
                            t!("typing.params.line_placement_reference_glyph"),
                        );
                    });
                    let reference_resp = reference_combo
                        .inner
                        .response
                        .on_hover_text(t!("typing.params.line_placement_reference_tooltip"));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &reference_resp);
                    // A wheel notch over the closed combo toggles between the two modes.
                    if let Some(steps) = reference_combo.wheel_steps
                        && steps != 0
                    {
                        self.line_placement_reference = match self.line_placement_reference {
                            LinePlacementReference::LineBox => LinePlacementReference::GlyphHeight,
                            LinePlacementReference::GlyphHeight => LinePlacementReference::LineBox,
                        };
                    }
                    if self.line_placement_reference != prev_reference {
                        *changed = true;
                    }
                }
            });

            // Per-selection alignment (inline style) — enabled while a selection is
            // active; moved here from the former right column's inline block.
            if let Some(style) = inline_style {
                let mut align = style.align.unwrap_or(self.align);
                Self::draw_alignment_controls(ui, &mut align, changed, block_hscroll_by_hovered_param);
                style.align = Some(align);
            }
        });
    }

    /// Shape & smoothing section (default collapsed, gated on `!font_missing` then
    /// `!selection_mode`): the shape / wrap / anti-aliasing combos, the
    /// moderate-herringbone checkbox, and the shape-specific min-width / variant
    /// sliders. Moved verbatim from the former right column.
    pub(super) fn draw_shape_render_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                let prev_shape = self.text_shape;
                let shape_combo = WheelComboBox::from_label(t!("typing.create.shape_combo_id")).id_salt("typing.create.shape_combo_id")
                    .selected_text(match self.text_shape {
                        TextShape::Free => t!("typing.params.shape_free_option"),
                        TextShape::Rectangle => "[  ]",
                        TextShape::Oval => "(  )",
                        TextShape::Hexagon => "<  >",
                        TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(&mut self.text_shape, TextShape::Free, t!("typing.params.shape_free_option"));
                        ui.selectable_value(&mut self.text_shape, TextShape::Rectangle, "[  ]");
                        ui.selectable_value(&mut self.text_shape, TextShape::Oval, "(  )");
                        ui.selectable_value(&mut self.text_shape, TextShape::Hexagon, "<  >");
                        ui.selectable_value(&mut self.text_shape, TextShape::SoftPeak, t!("typing.params.shape_soft_option"));
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &shape_combo.inner.response,
                );
                if let Some(steps) = shape_combo.wheel_steps {
                    *changed |= cycle_text_shape(&mut self.text_shape, steps);
                }
                if self.text_shape != prev_shape {
                    *changed = true;
                }

                let prev_wrap_mode = self.text_wrap_mode;
                let wrap_combo = WheelComboBox::from_label(t!("typing.create.wrap_combo_id")).id_salt("typing.create.wrap_combo_id")
                    .selected_text(text_wrap_mode_label(self.text_wrap_mode))
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::None,
                            text_wrap_mode_label(TextWrapMode::None),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::WholeWords,
                            text_wrap_mode_label(TextWrapMode::WholeWords),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Minimal,
                            text_wrap_mode_label(TextWrapMode::Minimal),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Moderate,
                            text_wrap_mode_label(TextWrapMode::Moderate),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Aggressive,
                            text_wrap_mode_label(TextWrapMode::Aggressive),
                        );
                    });
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &wrap_combo.inner.response);
                if let Some(steps) = wrap_combo.wheel_steps {
                    *changed |= cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
                }
                if self.text_wrap_mode != prev_wrap_mode {
                    self.sync_wrap_mode_constraints();
                    *changed = true;
                }

                let prev_anti_aliasing = self.anti_aliasing;
                // Horizontal row so the animated help icon sits after the
                // combo's right-hand label.
                let aa_combo = ui
                    .horizontal(|ui| {
                        let aa_combo = WheelComboBox::from_label(t!("typing.create.antialias_combo_id")).id_salt("typing.create.antialias_combo_id")
                            .selected_text(anti_aliasing_label(self.anti_aliasing))
                            .show_ui_with_wheel(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::None,
                                    anti_aliasing_label(AntiAliasingMode::None),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Sharp,
                                    anti_aliasing_label(AntiAliasingMode::Sharp),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Crisp,
                                    anti_aliasing_label(AntiAliasingMode::Crisp),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Strong,
                                    anti_aliasing_label(AntiAliasingMode::Strong),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Smooth,
                                    anti_aliasing_label(AntiAliasingMode::Smooth),
                                );
                            });
                        crate::widgets::HelpHint::animated(ms_gifs::typing::ANTI_ALIASING).show(ui);
                        aa_combo
                    })
                    .inner;
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &aa_combo.inner.response);
                if let Some(steps) = aa_combo.wheel_steps {
                    *changed |= cycle_anti_aliasing(&mut self.anti_aliasing, steps);
                }
                if self.anti_aliasing != prev_anti_aliasing {
                    *changed = true;
                }
                let moderate_trees_resp = ui.add_enabled(
                    self.moderate_trees_checkbox_enabled(),
                    egui::Checkbox::new(&mut self.allow_moderate_trees, t!("typing.params.allow_moderate_herringbone")),
                );
                *changed |= moderate_trees_resp.changed();

                if matches!(self.text_shape, TextShape::Oval | TextShape::Hexagon) {
                    let min_width_resp = ui.add(
                        WheelSlider::new(&mut self.shape_min_width_percent, 5.0..=100.0)
                            .text(t!("typing.params.min_width_percent_label")),
                    );
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &min_width_resp);
                    *changed |= min_width_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &min_width_resp) {
                        *changed |= apply_wheel_step_f32(
                            &mut self.shape_min_width_percent,
                            steps,
                            1.0,
                            5.0,
                            100.0,
                        );
                    }
                }
                if self.text_shape == TextShape::SoftPeak {
                    let variant_resp =
                        ui.add(WheelSlider::new(&mut self.shape_variant, 1..=9).text(t!("typing.params.shape_variant_label")));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &variant_resp);
                    *changed |= variant_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &variant_resp) {
                        *changed |= apply_wheel_step_u8(&mut self.shape_variant, steps, 1, 1, 9);
                    }
                }
            });
        });
    }

    /// Typeface-style section (default collapsed, gated on `!font_missing`): the
    /// faux bold/italic controls, plus the inline `no_break` checkbox in an inline
    /// selection. The per-selection alignment controls live in the layout section.
    /// Moved verbatim from the former right column's faux-style block.
    pub(super) fn draw_weight_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        ui.add_enabled_ui(!font_missing, |ui| {
            if let Some(style) = inline_style {
                let mut bold = style.bold;
                let mut italic = style.italic;
                let faux = style.faux_bold.unwrap_or_default();
                let mut thicken = faux.thicken_percent;
                let mut expand = faux.expand_percent;
                let mut sharp = faux.sharp_corners;
                let mut outward = faux.outward_only;
                let mut faux_bold = style.faux_bold.is_some();
                let mut slant = style.faux_italic_slant.unwrap_or(14.0);
                let mut faux_italic = style.faux_italic_slant.is_some();
                draw_faux_style_controls(
                    ui,
                    &mut bold,
                    &mut italic,
                    FauxStyleControlValues {
                        faux_bold: &mut faux_bold,
                        faux_bold_thicken_percent: &mut thicken,
                        faux_bold_expand_percent: &mut expand,
                        faux_bold_sharp_corners: &mut sharp,
                        faux_bold_outward_only: &mut outward,
                        faux_italic: &mut faux_italic,
                        faux_italic_slant_deg: &mut slant,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                    "typing_main_inline_faux",
                );
                style.bold = bold;
                style.italic = italic;
                style.faux_bold = (bold && faux_bold).then_some(FauxBoldParams {
                    thicken_percent: thicken,
                    expand_percent: expand,
                    sharp_corners: sharp,
                    outward_only: outward,
                });
                style.faux_italic_slant = (italic && faux_italic).then_some(slant);

                let mut no_break = style.no_break;
                let no_break_resp = ui.checkbox(&mut no_break, t!("typing.params.no_break"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &no_break_resp);
                *changed |= no_break_resp.changed();
                style.no_break = no_break;
            } else {
                draw_faux_style_controls(
                    ui,
                    &mut self.force_bold,
                    &mut self.force_italic,
                    FauxStyleControlValues {
                        faux_bold: &mut self.faux_bold,
                        faux_bold_thicken_percent: &mut self.faux_bold_thicken_percent,
                        faux_bold_expand_percent: &mut self.faux_bold_expand_percent,
                        faux_bold_sharp_corners: &mut self.faux_bold_sharp_corners,
                        faux_bold_outward_only: &mut self.faux_bold_outward_only,
                        faux_italic: &mut self.faux_italic,
                        faux_italic_slant_deg: &mut self.faux_italic_slant_deg,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                    "typing_main_faux",
                );
            }
        });
    }

    /// Text-processing section (default collapsed, gated on `!font_missing` then
    /// `!selection_mode`): the six processing checkboxes (hanging punctuation,
    /// strip extra spaces, replace ellipsis with three dots, newline after
    /// sentence, all-uppercase, enable inline tags), plus the indented
    /// force-remove-ellipsis-glyph sub-checkbox shown only while the ellipsis
    /// substitution is on. Moved verbatim from the former right column.
    pub(super) fn draw_text_processing_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                // Horizontal row so the animated help icon sits after the checkbox label.
                let hanging_punct_resp = ui
                    .horizontal(|ui| {
                        let resp = ui
                            .checkbox(&mut self.hanging_punctuation, t!("typing.params.hanging_punctuation"));
                        crate::widgets::HelpHint::animated(ms_gifs::typing::HANGING_PUNCTUATION).show(ui);
                        resp
                    })
                    .inner;
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &hanging_punct_resp);
                *changed |= hanging_punct_resp.changed();
                let trim_spaces_resp =
                    ui.checkbox(&mut self.trim_extra_spaces, t!("typing.params.strip_extra_spaces"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &trim_spaces_resp);
                *changed |= trim_spaces_resp.changed();
                let replace_ellipsis_resp = ui.checkbox(
                    &mut self.replace_ellipsis_with_dots,
                    t!("typing.params.replace_ellipsis"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &replace_ellipsis_resp);
                *changed |= replace_ellipsis_resp.changed();
                // Sub-parameter of the substitution above: only reachable while the
                // parent is on, so it is indented under it and hidden otherwise (same
                // grouping as the faux-bold strip). Its stored value survives the parent
                // being toggled off — the renderer, not the panel, gates on both flags.
                if self.replace_ellipsis_with_dots {
                    // Salted by PANEL like the neighbouring faux-style indents
                    // (`ui_helpers::faux_style_controls`, salt `typing_main_faux`):
                    // the create and edit panels draw the same control, so their
                    // indent ids must not collide.
                    ui.indent(
                        Id::new("typing_main_params").with("force_remove_ellipsis_glyph"),
                        |ui| {
                            let force_remove_glyph_resp = ui.checkbox(
                                &mut self.force_remove_ellipsis_glyph,
                                t!("typing.params.force_remove_ellipsis_glyph"),
                            );
                            mark_hscroll_block_on_hover(
                                block_hscroll_by_hovered_param,
                                &force_remove_glyph_resp,
                            );
                            *changed |= force_remove_glyph_resp.changed();
                        },
                    );
                }
                let sentence_nl_resp = ui.checkbox(
                    &mut self.new_line_after_sentence,
                    t!("typing.params.newline_after_sentence"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sentence_nl_resp);
                *changed |= sentence_nl_resp.changed();
                let uppercase_text_resp =
                    ui.checkbox(&mut self.uppercase_text, t!("typing.params.all_uppercase"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &uppercase_text_resp);
                *changed |= uppercase_text_resp.changed();
                let inline_tags_resp = ui.checkbox(
                    &mut self.enable_inline_style_tags,
                    t!("typing.params.parse_bi_tags"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inline_tags_resp);
                *changed |= inline_tags_resp.changed();
            });
        });
    }

    pub(super) fn draw_inline_offset_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let inline_font_size_px = inline_style
            .as_ref()
            .and_then(|style| style.font_size_px)
            .unwrap_or(self.font_size_px)
            .max(1.0);
        ui.add_enabled_ui(inline_style.is_some(), |ui| {
            let mut offset = inline_style
                .as_ref()
                .and_then(|style| style.glyph_offset)
                .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0]));
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_x_label"),
                &mut offset.global_x,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                    help: None,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_y_label"),
                &mut offset.global_y,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                    help: None,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            // Смещение по линии — линейная концепция: строка показывается только
            // для линейных раскладок (формула и кастомные векторные линии), как и
            // «Размещение по линии». В остальных режимах значение сохраняется, но
            // и сама строка, и её подпараметр скрыты.
            let line_based_layout = matches!(
                self.text_layout_mode,
                TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines
            );
            if line_based_layout {
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.inline_offset_along_line_label"),
                    &mut offset.line,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 1.0,
                        font_size_px: inline_font_size_px,
                        help: None,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                // «Сдвигать следующие символы» — подпараметр смещения по линии:
                // группируется под отступ-линией (как параметры faux bold под
                // чекбоксом) и появляется только при ненулевом смещении.
                if offset.line.value != 0.0 {
                    ui.indent(Id::new("typing_inline_shift_following"), |ui| {
                        *changed |= ui
                            .checkbox(
                                &mut offset.shift_following,
                                t!("typing.params.inline_shift_following"),
                            )
                            .changed();
                    });
                }
            }

            let group_enabled = inline_style
                .as_ref()
                .is_some_and(|_| self.selected_inline_char_count() > 1);
            ui.add_enabled_ui(group_enabled, |ui| {
                let group_resp = ui.add(
                    WheelSlider::new(&mut offset.group_rotation_deg, -180.0..=180.0)
                        .text(t!("typing.params.inline_group_rotation_label"))
                        .wheel_step(1.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &group_resp);
                *changed |= group_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &group_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut offset.group_rotation_deg,
                        steps,
                        1.0,
                        -180.0,
                        180.0,
                    );
                }
            });
            if !group_enabled {
                offset.group_rotation_deg = 0.0;
            }

            let glyph_resp = ui.add(
                WheelSlider::new(&mut offset.glyph_rotation_deg, -180.0..=180.0)
                    .text(t!("typing.params.inline_char_rotation_label"))
                    .wheel_step(1.0),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &glyph_resp);
            *changed |= glyph_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &glyph_resp) {
                *changed |=
                    apply_wheel_step_f32(&mut offset.glyph_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
            if let Some(style) = inline_style {
                style.glyph_offset = Some(offset);
            }
        });
    }

    pub(super) fn selected_inline_char_count(&self) -> usize {
        self.text_selection_char_range
            .as_ref()
            .map(|range| range.end.saturating_sub(range.start))
            .unwrap_or(0)
    }

    /// Управление выравниванием на ОДНОЙ строке: слайдер лево↔право (`-100..100`),
    /// быстрые кнопки (⬅ влево / ⬇ по центру / ➡ вправо) и зажимаемая кнопка-тоггл
    /// ⬌ (justify, «Растягивать по ширине блока»). Слайдер и стрелки отключаются при
    /// включённом justify; кнопка ⬌ остаётся активной, чтобы его можно было выключить.
    pub(super) fn draw_alignment_controls(
        ui: &mut egui::Ui,
        align: &mut HorizontalAlign,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let free_align = align.justify;
        ui.horizontal(|ui| {
            // Слайдер + стрелки отключаются при включённом justify.
            ui.add_enabled_ui(!free_align, |ui| {
                let mut bias_percent = (align.bias.clamp(-1.0, 1.0) * 100.0).round() as i32;
                let slider_resp = ui.add(
                    WheelSlider::new(&mut bias_percent, -100..=100)
                        .text(t!("typing.params.alignment_label"))
                        .wheel_step(5),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
                if slider_resp.changed() {
                    align.bias = bias_percent as f32 / 100.0;
                    *changed = true;
                }

                if ui.button("⬅").on_hover_text(t!("typing.params.align_left")).clicked() {
                    align.bias = -1.0;
                    *changed = true;
                }
                if ui.button("⬇").on_hover_text(t!("typing.params.align_center")).clicked() {
                    align.bias = 0.0;
                    *changed = true;
                }
                if ui.button("➡").on_hover_text(t!("typing.params.align_right")).clicked() {
                    align.bias = 1.0;
                    *changed = true;
                }
            });

            // Зажимаемая кнопка-тоггл justify — остаётся активной даже при включённом
            // justify, чтобы его можно было снять.
            if ui
                .add(egui::Button::new("⬌").selected(align.justify))
                .on_hover_text(t!("typing.params.justify_lines"))
                .clicked()
            {
                align.justify = !align.justify;
                *changed = true;
            }

            // Animated help for the whole alignment row. Kept OUTSIDE the
            // `add_enabled_ui(!free_align, ..)` scope above: a disabled icon
            // would never show its tooltip (`on_hover_ui` is enabled-only,
            // egui-0.35.0/src/response.rs:645), and the hint must stay
            // reachable while justify is on.
            crate::widgets::HelpHint::animated(ms_gifs::typing::ALIGNMENT).show(ui);
        });
    }

}

#[cfg(test)]
mod tests {
    use super::{
        Color32, LOCAL_PRESET_ROW_HOVER_TINT_ALPHA, LOCAL_PRESET_ROW_SELECTED_STROKE_ON_DARK,
        LOCAL_PRESET_ROW_SELECTED_STROKE_ON_LIGHT, LOCAL_PRESET_POPUP_MAX_ROWS,
        LOCAL_PRESET_POPUP_TEXT_ROWS, LocalPreset, ParamIdentityMode, PreviewBackdrop,
        TypingCreatePanelState, fit_preview_size, local_preset_popup_height,
        local_preset_popup_id_bucket, local_preset_row_hover_tint,
        local_preset_row_selection_stroke, local_preset_wheel_target,
    };
    use crate::tabs::typing::tab::render_store::shape_variant_luminance;
    use eframe::egui;
    use serde_json::json;

    /// The name row is part of the local-preset mode's LAYOUT: it is drawn whether or not a
    /// preset is selected, and only its enabled state follows the selection. Hiding it made
    /// the font section jump by a row on every select/deselect.
    #[test]
    fn the_name_row_is_always_drawn_and_only_disabled_without_a_selection() {
        let mut state = TypingCreatePanelState::new(false);
        state.preview_enabled = true;
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.local_presets = vec![LocalPreset::new(
            "П".to_string(),
            json!({"text_params": {"schema": 2}}),
        )];

        let mut without_selection = None;
        let mut without_height = 0.0_f32;
        egui::__run_test_ui(|ui| {
            let before = ui.min_rect().height();
            without_selection = Some(state.draw_local_preset_name_row(ui).enabled());
            without_height = ui.min_rect().height() - before;
        });

        state.selected_local_preset = Some(0);
        state.local_preset_name_input = "П".to_string();
        let mut with_selection = None;
        let mut with_height = 0.0_f32;
        egui::__run_test_ui(|ui| {
            let before = ui.min_rect().height();
            with_selection = Some(state.draw_local_preset_name_row(ui).enabled());
            with_height = ui.min_rect().height() - before;
        });

        assert_eq!(
            without_selection,
            Some(false),
            "with nothing selected the row must be disabled, not hidden"
        );
        assert_eq!(with_selection, Some(true));
        assert!(without_height > 0.0, "the row must occupy its space either way");
        assert!(
            (with_height - without_height).abs() < 0.5,
            "the layout must not jump: {without_height} vs {with_height}"
        );
    }

    /// A short list gets a cap tall enough for ALL of its rows, so it never scrolls.
    #[test]
    fn the_popup_fits_a_short_list_without_scrolling() {
        let pitch = 26.0_f32;
        // Two presets plus the two text rows.
        let height = local_preset_popup_height(2 + LOCAL_PRESET_POPUP_TEXT_ROWS, pitch);
        assert!(
            height >= 4.0 * pitch,
            "four rows must fit in {height} at a pitch of {pitch}"
        );
    }

    /// A long list is capped at [`LOCAL_PRESET_POPUP_MAX_ROWS`] rows, which is what makes it
    /// scroll instead of growing past the screen.
    #[test]
    fn the_popup_stops_growing_at_the_row_cap() {
        let pitch = 26.0_f32;
        let cap = f32::from(LOCAL_PRESET_POPUP_MAX_ROWS) * pitch;
        assert!((local_preset_popup_height(40, pitch) - cap).abs() < f32::EPSILON);
        assert!(
            (local_preset_popup_height(usize::from(LOCAL_PRESET_POPUP_MAX_ROWS), pitch) - cap)
                .abs()
                < f32::EPSILON,
            "exactly the cap must not already be clipped"
        );
    }

    /// Degenerate inputs stay usable: never a zero-height popup, never a negative one.
    #[test]
    fn the_popup_height_is_never_zero_or_negative() {
        assert!(local_preset_popup_height(0, 26.0) > 0.0);
        assert!(local_preset_popup_height(3, -5.0) >= 0.0);
    }

    /// The wheel walks the empty selection and the real presets, and wraps between them.
    #[test]
    fn wheel_walks_none_and_the_presets() {
        assert_eq!(local_preset_wheel_target(None, 2, 1), Some(Some(0)));
        assert_eq!(local_preset_wheel_target(Some(0), 2, 1), Some(Some(1)));
        // Past the last preset the list wraps onto the empty selection, not onto «create».
        assert_eq!(local_preset_wheel_target(Some(1), 2, 1), Some(None));
        assert_eq!(local_preset_wheel_target(None, 2, -1), Some(Some(1)));
    }

    /// A notch that does not move the selection reports nothing, so no operation runs.
    #[test]
    fn wheel_reports_nothing_when_the_selection_does_not_move() {
        assert_eq!(local_preset_wheel_target(None, 2, 0), None);
        // An empty set is a one-row virtual list: every step lands back on «none».
        assert_eq!(local_preset_wheel_target(None, 0, 1), None);
        assert_eq!(local_preset_wheel_target(None, 0, -7), None);
        // A full wrap (3 steps over 1 + 2 rows) is also a no-op.
        assert_eq!(local_preset_wheel_target(Some(0), 2, 3), None);
    }

    /// THE contract of this helper: no wheel step ever reaches a "create" outcome, and no
    /// step ever names a preset that does not exist.
    #[test]
    fn wheel_never_leaves_the_virtual_list() {
        for count in 0..5usize {
            for steps in -9..=9i32 {
                for selected in std::iter::once(None).chain((0..count).map(Some)) {
                    let Some(target) = local_preset_wheel_target(selected, count, steps) else {
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

    /// A preview narrower than the popup is drawn at its own size; a wider one is scaled
    /// by width with its aspect ratio preserved.
    #[test]
    fn preview_size_is_capped_by_width_only() {
        let small = egui::vec2(80.0, 22.0);
        assert_eq!(fit_preview_size(small, 200.0), small);

        let wide = fit_preview_size(egui::vec2(320.0, 22.0), 160.0);
        assert!((wide.x - 160.0).abs() < 1e-3, "{wide:?}");
        assert!((wide.y - 11.0).abs() < 1e-3, "{wide:?}");
    }

    /// An unknown popup width (not yet laid out) must not collapse the image.
    #[test]
    fn preview_size_survives_a_degenerate_width() {
        let size = egui::vec2(120.0, 22.0);
        assert_eq!(fit_preview_size(size, 0.0), size);
        assert_eq!(fit_preview_size(size, -4.0), size);
        assert_eq!(fit_preview_size(size, f32::NAN), size);
        assert_eq!(fit_preview_size(egui::vec2(0.0, 22.0), 10.0), egui::vec2(0.0, 22.0));
    }

    /// THE POPUP-SALT INVARIANT: distinct row counts must produce distinct buckets, because
    /// a popup `Area` remembers its size under a fixed id and can only shrink. Sharing a
    /// bucket between two different heights is exactly how the popup got stuck at three rows.
    #[test]
    fn every_row_count_below_the_cap_gets_its_own_popup_id_bucket() {
        // From 1: a zero-row popup cannot occur (the two text rows are always drawn) and is
        // deliberately folded onto the one-row bucket, whose height it shares.
        let buckets: Vec<usize> = (1..usize::from(LOCAL_PRESET_POPUP_MAX_ROWS))
            .map(local_preset_popup_id_bucket)
            .collect();
        let unique: std::collections::BTreeSet<usize> = buckets.iter().copied().collect();
        assert_eq!(
            unique.len(),
            buckets.len(),
            "two row counts below the cap must never share a popup id: {buckets:?}"
        );
    }

    /// Above the cap every row count measures identically, so they SHARE one bucket — one
    /// stored `Area` per distinct height, not one per preset count.
    #[test]
    fn row_counts_above_the_cap_share_one_popup_id_bucket() {
        let cap = usize::from(LOCAL_PRESET_POPUP_MAX_ROWS);
        assert_eq!(local_preset_popup_id_bucket(cap), cap);
        assert_eq!(local_preset_popup_id_bucket(cap + 1), cap);
        assert_eq!(local_preset_popup_id_bucket(10_000), cap);
    }

    /// The bucket and the height agree: two row counts that share a bucket must produce the
    /// same popup height, and two that do not must not. This is the property that makes the
    /// bucket a legitimate id — an id that changed less often than the height would leave
    /// the old defect in place.
    #[test]
    fn the_popup_id_bucket_decides_the_popup_height() {
        let pitch = 26.0_f32;
        for left in 0..20usize {
            for right in 0..20usize {
                let same_bucket =
                    local_preset_popup_id_bucket(left) == local_preset_popup_id_bucket(right);
                let same_height = (local_preset_popup_height(left, pitch)
                    - local_preset_popup_height(right, pitch))
                .abs()
                    < f32::EPSILON;
                assert_eq!(same_bucket, same_height, "left={left} right={right}");
            }
        }
    }

    /// Every backdrop's selection outline must actually be visible on it: the outline is the
    /// ONLY selection cue a preset row has, since a fill would cover the preview.
    #[test]
    fn the_selection_outline_contrasts_with_every_backdrop() {
        for backdrop in [
            PreviewBackdrop::Light,
            PreviewBackdrop::Medium,
            PreviewBackdrop::Dark,
        ] {
            let stroke = local_preset_row_selection_stroke(backdrop).to_srgba_unmultiplied();
            let contrast = (shape_variant_luminance(stroke)
                - shape_variant_luminance(backdrop.fill().to_srgba_unmultiplied()))
            .abs();
            assert!(
                contrast >= 60.0,
                "{backdrop:?}: the selection outline is only {contrast} luminance points away"
            );
        }
    }

    /// The user's earlier decision, pinned so a later edit cannot quietly swap the pair: the
    /// dark blue is the outline of the LIGHT backdrop.
    #[test]
    fn the_dark_blue_outline_stays_on_the_light_backdrop() {
        assert_eq!(
            local_preset_row_selection_stroke(PreviewBackdrop::Light),
            LOCAL_PRESET_ROW_SELECTED_STROKE_ON_LIGHT
        );
        assert_eq!(
            local_preset_row_selection_stroke(PreviewBackdrop::Dark),
            LOCAL_PRESET_ROW_SELECTED_STROKE_ON_DARK
        );
    }

    /// The hover cue must move the backdrop far enough to be seen on all three greys —
    /// including the middle one, where neither direction is obviously right.
    #[test]
    fn the_hover_tint_is_visible_on_every_backdrop() {
        let alpha = f32::from(LOCAL_PRESET_ROW_HOVER_TINT_ALPHA) / 255.0;
        for backdrop in [
            PreviewBackdrop::Light,
            PreviewBackdrop::Medium,
            PreviewBackdrop::Dark,
        ] {
            let grey = shape_variant_luminance(backdrop.fill().to_srgba_unmultiplied());
            // The tint is either pure white or pure black at `alpha`, so the composite is a
            // straight interpolation towards 255 or towards 0.
            let tint = local_preset_row_hover_tint(backdrop);
            let towards_white = tint == Color32::from_white_alpha(LOCAL_PRESET_ROW_HOVER_TINT_ALPHA);
            assert!(
                towards_white
                    || tint == Color32::from_black_alpha(LOCAL_PRESET_ROW_HOVER_TINT_ALPHA),
                "{backdrop:?}: the hover tint must be pure white or pure black"
            );
            let hovered = if towards_white {
                grey + alpha * (255.0 - grey)
            } else {
                grey * (1.0 - alpha)
            };
            assert!(
                (hovered - grey).abs() >= 15.0,
                "{backdrop:?}: hovering moves the row by only {} luminance points",
                (hovered - grey).abs()
            );
        }
    }
}
