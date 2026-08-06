/*
File: panel/char_table/window.rs

Purpose:
The egui UI of the typing tab's character-table window ("Таблица символов",
`dev-docs/char_table_plan.md` §7): the size control, the tab strip, the wrapping
symbol grid, the expanded per-font variants block, and the favorites star with
its popup. Every piece of STATE it reads or mutates lives in `mod.rs`
(`CharTableState`); this file owns no state of its own.

Main responsibilities:
- draw the window and report the character the user asked to insert;
- keep the extended font tier armed for the visible tab (`ui_fonts::ensure_covers`);
- register per-font preview typefaces under a strict per-frame budget;
- drive the two favorite lists through the star popup.

Key types:
- `CharTableAction` (what the window asks its caller to do)

Key functions:
- `draw_char_table_window` (the whole window; a free fn on purpose, see below)
- `insert_action` (the `<font=…>` tag decision)
- `grid_columns` (pure layout math, unit-tested)

Notes:
`draw_char_table_window` is a FREE FUNCTION taking disjoint borrows rather than a
`&mut TypingCreatePanelState` method: the window has to read the panel font list,
mutate the table state, and cause an edit of the panel's text buffer, which
cannot be one `&mut self` borrow. The text edit is therefore not performed here —
it is returned as a `CharTableAction` the caller applies through
`create_edit::insert_text_at_caret`.

The CHARACTERS painted in the cells are CONTENT, not labels: they are exactly what
an insertion puts into the user's text, so they stay literals
(`dev-docs/i18n_exclusions.md` §A8). Everything around them is localized.
*/

use super::favorites::ProjectDocumentState;
use super::{CharTableState, FAVORITES_TAB_KEY, MAX_CELL_FONT_SIZE, MIN_CELL_FONT_SIZE};
use crate::tabs::typing::panel::FontEntry;
use crate::tabs::typing::panel::fonts::BUNDLED_UI_FONT_IDENTITY;
use crate::widgets::WheelSlider;
use crate::widgets::{
    PreviewFontFamily, combo_font_family_name, is_font_family_bound, request_font_family,
};
use eframe::egui;

/// Stable window identity. A literal persistence key (window position/size), not
/// a caption — the caption is the localized title (`dev-docs/i18n_exclusions.md`).
const WINDOW_ID: &str = "typing.char_table.window";

/// How many not-yet-bound font previews may be advanced in one frame.
///
/// The font FILE is no longer read here — `widgets::font_preview` reads it on its own
/// worker threads — but a request still queues a read and, once the bytes arrive,
/// hands them to egui, which never evicts a registered font and rebuilds its glyph
/// atlas. A symbol covered by forty loaded fonts would otherwise queue forty reads and
/// forty registrations at once. Cells whose typeface is not bound yet draw the glyph in
/// the UI font meanwhile, and the window keeps requesting repaints until the backlog is
/// drained. A font that FAILED to load is remembered by `widgets::font_preview`, so it
/// never costs a slot again.
const MAX_FONT_REGISTRATIONS_PER_FRAME: usize = 2;

/// Extra room around the glyph inside a cell, in points.
const CELL_PADDING_PX: f32 = 12.0;

/// Hard upper bound on the computed column count. It exists so the float→integer
/// conversion in [`grid_columns`] is provably exact, and it is far above any
/// column count a real window can show.
const MAX_GRID_COLUMNS: f32 = 4096.0;

/// Filled-star color of a PROJECT (title-scoped) favorite.
const PROJECT_FAVORITE_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
/// Filled-star color of a GLOBAL (application-wide) favorite.
const GLOBAL_FAVORITE_COLOR: egui::Color32 = egui::Color32::from_rgb(240, 200, 70);

/// Filled star: the character IS in the list this star stands for.
/// CONTENT-class literal (a glyph, not a caption) — see the file header.
const STAR_FILLED: char = '★';
/// Outline star: the character is in NEITHER list; shown only while the cell is
/// hovered. CONTENT-class literal, like [`STAR_FILLED`].
const STAR_OUTLINE: char = '☆';

/// Geometry of one symbol cell, in points.
///
/// Derived once per frame from the persisted cell size so the grid and the
/// variants block cannot drift apart.
#[derive(Debug, Clone, Copy)]
struct CellMetrics {
    /// Side of the square cell.
    side_px: f32,
    /// Font size the glyph is painted at.
    glyph_px: f32,
}

impl CellMetrics {
    /// Builds the metrics for a glyph size taken from `CharTableState`.
    fn from_glyph_size(glyph_px: f32) -> Self {
        Self {
            side_px: glyph_px + CELL_PADDING_PX,
            glyph_px,
        }
    }

    /// Desired size of one cell.
    fn size(self) -> egui::Vec2 {
        egui::vec2(self.side_px, self.side_px)
    }
}

/// What the window asks its caller to do after this frame.
///
/// The window cannot perform the edit itself: the text buffer lives on
/// `TypingCreatePanelState`, which is not borrowed here (see the file header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tabs::typing::panel) enum CharTableAction {
    /// Insert this exact string at the caret of the active text buffer. It is
    /// either the bare character or the character wrapped in a `<font=…>` inline
    /// tag — see [`insert_action`].
    Insert(String),
}

/// Draws the character-table window and returns the insertion it asks for.
///
/// `state` owns everything the window shows and is the only thing it mutates;
/// `fonts` is the panel font list the coverage indices point into;
/// `base_font_identity` is the render identity of the panel's currently selected
/// font, against which the `<font=…>` tag decision is made (see [`insert_action`]).
///
/// Returns `None` when the window is closed (an immediate no-op) or when the user
/// did not click a variant this frame. The window deliberately stays OPEN after an
/// insertion: inserting several symbols in a row is the normal case.
///
/// Must run on the GUI thread inside a frame: it calls `ui_fonts::ensure_covers`
/// and may register font faces.
#[must_use]
pub(in crate::tabs::typing::panel) fn draw_char_table_window(
    state: &mut CharTableState,
    ctx: &egui::Context,
    fonts: &[FontEntry],
    base_font_identity: Option<&str>,
) -> Option<CharTableAction> {
    if !state.is_open() {
        return None;
    }

    let mut action = None;
    // egui writes its close button into this flag; it is copied back into the
    // state below so the close edge can also collapse the expanded row.
    let mut open = true;

    // Self-centering, mirroring `create_advanced::draw_advanced_form_window`: the
    // size is stated up front, so a `CENTER_CENTER` pivot at the viewport center
    // lands correctly on the FIRST frame and needs no hide-then-measure pass.
    let viewport = ctx.content_rect();
    let default_size = egui::vec2(
        (viewport.width() * 0.55).max(420.0),
        (viewport.height() * 0.65).max(320.0),
    );

    egui::Window::new(t!("typing.char_table.window_title"))
        // The title is localized, so the id must be pinned explicitly
        // (`egui-docs/05-ids-and-i18n.md` §2) or the window would lose its
        // position and size on a UI-language switch.
        .id(egui::Id::new(WINDOW_ID))
        .open(&mut open)
        .resizable(true)
        // Above the typing parameter/action panels, which float on
        // `Order::Foreground` — the same reason the advanced-form window does it.
        .order(egui::Order::Tooltip)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(viewport.center())
        .default_size(default_size)
        .show(ctx, |ui| {
            action = draw_window_body(state, ui, fonts, base_font_identity);
        });

    *state.open_mut() = open;
    if !open {
        // A row expanded in a closed window would reappear on the next open with
        // a variants block the user did not ask for.
        state.collapse();
        state.set_star_popup_char(None);
    }
    action
}

/// Draws everything inside the window frame.
fn draw_window_body(
    state: &mut CharTableState,
    ui: &mut egui::Ui,
    fonts: &[FontEntry],
    base_font_identity: Option<&str>,
) -> Option<CharTableAction> {
    draw_size_control(state, ui);
    draw_tab_strip(state, ui);
    ui.separator();

    if let Some(status) = project_favorites_status(state.project_favorites_state()) {
        ui.label(status);
    }

    let visible: Vec<char> = state.visible_chars().to_vec();
    // The UI font chain carries only `fonts/ui/core` until something asks for
    // more, so a tab full of rare symbols would paint tofu without this. The two
    // star glyphs are appended because they are painted on EVERY tab, not only on
    // the one that lists them. Idempotent, off-thread, GUI-thread-only — see
    // `src/ui_fonts.rs`.
    let mut covered: String = visible.iter().collect();
    covered.push(STAR_FILLED);
    covered.push(STAR_OUTLINE);
    crate::ui_fonts::ensure_covers(ui.ctx(), &covered);

    if visible.is_empty() {
        ui.label(t!("typing.char_table.empty_favorites_status"));
        return None;
    }

    let metrics = CellMetrics::from_glyph_size(state.cell_font_size());
    let expanded = state.expanded_char();
    let mut budget = FontRegistrationBudget::new();
    let mut action = None;

    egui::ScrollArea::vertical()
        .id_salt("typing.char_table.grid_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let columns = grid_columns(
                ui.available_width(),
                metrics.side_px,
                ui.spacing().item_spacing.x,
            );
            for row in visible.chunks(columns) {
                ui.horizontal(|ui| {
                    for &ch in row {
                        draw_grid_cell(state, ui, ch, metrics);
                    }
                });
                // The variants block belongs to the row that CONTAINS the
                // expanded symbol, so the rest of the grid is pushed down
                // instead of the block floating over it.
                if let Some(expanded_char) = expanded
                    && row.contains(&expanded_char)
                {
                    let block_action = draw_variants_block(
                        state,
                        ui,
                        expanded_char,
                        fonts,
                        base_font_identity,
                        metrics,
                        &mut budget,
                    );
                    if block_action.is_some() {
                        action = block_action;
                    }
                }
            }
        });

    if budget.outstanding {
        // Keep the frames coming until every visible variant has its own
        // typeface; without this the remaining cells would stay in the UI font
        // until some unrelated event repainted the window.
        ui.ctx().request_repaint();
    }
    action
}

/// Draws the cell-size row.
fn draw_size_control(state: &mut CharTableState, ui: &mut egui::Ui) {
    let mut size = state.cell_font_size();
    ui.add(
        WheelSlider::new(&mut size, MIN_CELL_FONT_SIZE..=MAX_CELL_FONT_SIZE)
            .text(t!("typing.char_table.cell_size_label")),
    );
    // A no-op when the clamped value did not change, so dragging does not queue a
    // config write per frame (see `CharTableState::set_cell_font_size`).
    state.set_cell_font_size(size);
}

/// Draws the tab strip: every `charset` group plus the favorites tab.
fn draw_tab_strip(state: &mut CharTableState, ui: &mut egui::Ui) {
    let selected = state.selected_group().to_string();
    let mut picked: Option<&'static str> = None;
    ui.horizontal_wrapped(|ui| {
        for group in CharTableState::groups() {
            if ui
                .selectable_label(selected == group.key, tab_label(group.key))
                .clicked()
            {
                picked = Some(group.key);
            }
        }
        if ui
            .selectable_label(selected == FAVORITES_TAB_KEY, tab_label(FAVORITES_TAB_KEY))
            .clicked()
        {
            picked = Some(FAVORITES_TAB_KEY);
        }
    });
    if let Some(key) = picked {
        state.set_selected_group(key);
    }
}

/// Localized caption of a tab.
///
/// The group keys are STABLE persisted identities (`charset.rs`), so the mapping
/// to catalog keys is written out explicitly: a `format!`-built key would be
/// invisible to the i18n orphan test and would silently show the raw key text
/// after a rename. An unknown key cannot occur (the groups are compile-time
/// constants); it degrades to the key itself rather than to an empty tab.
fn tab_label(group_key: &str) -> &str {
    match group_key {
        "arrows" => t!("typing.char_table.group.arrows_label"),
        "lines" => t!("typing.char_table.group.lines_label"),
        "shapes" => t!("typing.char_table.group.shapes_label"),
        "math" => t!("typing.char_table.group.math_label"),
        "typography" => t!("typing.char_table.group.typography_label"),
        "currency" => t!("typing.char_table.group.currency_label"),
        "music" => t!("typing.char_table.group.music_label"),
        "technical" => t!("typing.char_table.group.technical_label"),
        "game" => t!("typing.char_table.group.game_label"),
        "stars_weather" => t!("typing.char_table.group.stars_weather_label"),
        "emoji" => t!("typing.char_table.group.emoji_label"),
        FAVORITES_TAB_KEY => t!("typing.char_table.group.favorites_label"),
        other => other,
    }
}

/// Draws one grid cell: the glyph, its hover/expanded background, and its star.
fn draw_grid_cell(
    state: &mut CharTableState,
    ui: &mut egui::Ui,
    ch: char,
    metrics: CellMetrics,
) {
    let (rect, response) = ui.allocate_exact_size(metrics.size(), egui::Sense::click());
    // Hover comes ONLY from the occlusion-aware hit test: a raw pointer read
    // would react through the star popup and through anything else drawn on top
    // (`egui-docs/06-overlays.md` §5).
    let hovered = response.hovered() || response.contains_pointer();
    let expanded = state.expanded_char() == Some(ch);

    let fill = if expanded {
        Some(ui.visuals().selection.bg_fill)
    } else if hovered {
        Some(ui.visuals().widgets.hovered.bg_fill)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(3), fill);
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ch,
        egui::FontId::new(metrics.glyph_px, egui::FontFamily::Proportional),
        ui.visuals().text_color(),
    );
    let response = response.on_hover_text(t!("typing.char_table.cell_tooltip"));
    if response.clicked() {
        state.toggle_expanded(ch);
    }

    draw_cell_star(state, ui, ch, rect, hovered);
}

/// Draws the favorites star in the cell's top-right corner and its popup.
///
/// A filled blue star means the character is a PROJECT (title-scoped) favorite, a
/// filled yellow one a GLOBAL favorite; both are painted when it is in both. When
/// it is in neither, an outline star appears only while the cell is hovered — and
/// only then does the star claim a hitbox, so an unhovered cell has no invisible
/// corner that swallows its own click.
fn draw_cell_star(
    state: &mut CharTableState,
    ui: &mut egui::Ui,
    ch: char,
    cell_rect: egui::Rect,
    cell_hovered: bool,
) {
    let is_project = state.is_project_favorite(ch);
    let is_global = state.is_global_favorite(ch);
    // The open-popup case is load-bearing, not defensive: while the popup is
    // shown the pointer sits ON IT, so the cell is no longer hovered. Dropping
    // the star here would drop its `Response` with it, and the popup — which is
    // anchored to that response — would vanish the frame after it opened.
    let popup_open = state.star_popup_char() == Some(ch);
    if !is_project && !is_global && !cell_hovered && !popup_open {
        return;
    }

    let star_px = (cell_rect.height() * 0.36).clamp(9.0, 18.0);
    let star_count = if is_project && is_global { 2.0 } else { 1.0 };
    let star_rect = egui::Rect::from_min_size(
        egui::pos2(
            cell_rect.right() - star_px * star_count - 1.0,
            cell_rect.top() + 1.0,
        ),
        egui::vec2(star_px * star_count, star_px),
    );
    // `interact` senses without consuming layout space, and it is registered
    // AFTER the cell, so egui awards a click in the corner to the star (a tie
    // goes to the last-registered rect) while the cell keeps `contains_pointer`
    // (same layer, so the star does not hide it).
    let response = ui.interact(
        star_rect,
        egui::Id::new(("typing.char_table.star", ch)),
        egui::Sense::click(),
    );

    let font = egui::FontId::new(star_px, egui::FontFamily::Proportional);
    let painter = ui.painter();
    let mut x = star_rect.left();
    if is_project {
        painter.text(
            egui::pos2(x, star_rect.top()),
            egui::Align2::LEFT_TOP,
            STAR_FILLED,
            font.clone(),
            PROJECT_FAVORITE_COLOR,
        );
        x += star_px;
    }
    if is_global {
        painter.text(
            egui::pos2(x, star_rect.top()),
            egui::Align2::LEFT_TOP,
            STAR_FILLED,
            font.clone(),
            GLOBAL_FAVORITE_COLOR,
        );
    }
    if !is_project && !is_global {
        painter.text(
            star_rect.left_top(),
            egui::Align2::LEFT_TOP,
            STAR_OUTLINE,
            font,
            ui.visuals().weak_text_color(),
        );
    }
    let response = response.on_hover_text(t!("typing.char_table.star_tooltip"));

    draw_star_popup(state, ch, &response, is_project, is_global);
}

/// Opens/closes the star popup and applies the favorite toggle it produced.
fn draw_star_popup(
    state: &mut CharTableState,
    ch: char,
    star_response: &egui::Response,
    is_project: bool,
    is_global: bool,
) {
    let was_open = state.star_popup_char() == Some(ch);
    let mut popup_open = was_open;
    if star_response.clicked() {
        popup_open = !popup_open;
    }

    let project_disabled_reason = project_favorite_disabled_reason(state.project_favorites_state());
    let mut toggle_project = false;
    let mut toggle_global = false;
    if popup_open {
        egui::Popup::from_response(star_response)
            // `PopupKind::Menu` would put the popup on `Order::Foreground`, i.e.
            // UNDER this window, which sits on `Order::Tooltip`. The menu layout
            // and the close behavior are set explicitly instead.
            .kind(egui::PopupKind::Tooltip)
            .layout(egui::Layout::top_down_justified(egui::Align::Min))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .open_bool(&mut popup_open)
            .show(|ui| {
                let project_label = if is_project {
                    t!("typing.char_table.remove_project_favorite_button")
                } else {
                    t!("typing.char_table.add_project_favorite_button")
                };
                let mut project_response = ui.add_enabled(
                    project_disabled_reason.is_none(),
                    egui::Button::new(project_label),
                );
                // Each blocked state explains ITSELF; a shared "no project is
                // open" text would be a lie whenever a project is in fact open.
                if let Some(reason) = project_disabled_reason {
                    project_response = project_response.on_disabled_hover_text(reason);
                }
                if project_response.clicked() {
                    toggle_project = true;
                }
                let global_label = if is_global {
                    t!("typing.char_table.remove_global_favorite_button")
                } else {
                    t!("typing.char_table.add_global_favorite_button")
                };
                if ui.button(global_label).clicked() {
                    toggle_global = true;
                }
            });
    }

    // Written back only on a real edge, so a cell whose popup is closed cannot
    // clear the ONE open popup of another cell.
    if popup_open != was_open {
        state.set_star_popup_char(popup_open.then_some(ch));
    }
    if toggle_project {
        state.toggle_project_favorite(ch);
    }
    if toggle_global {
        state.toggle_global_favorite(ch);
    }
}

/// Why the project-favorite button is disabled, or `None` when it is usable.
///
/// The wording is per STATE on purpose: the button is dead for four different
/// reasons, and only one of them is "no project is open". `Invalid` is usable —
/// the toggle quarantines the malformed document first and then saves
/// (`favorites.rs::ProjectFavorites::toggle`).
#[must_use]
fn project_favorite_disabled_reason(state: ProjectDocumentState) -> Option<&'static str> {
    match state {
        ProjectDocumentState::Ready | ProjectDocumentState::Invalid => None,
        ProjectDocumentState::Unbound => Some(t!("typing.char_table.no_project_tooltip")),
        ProjectDocumentState::Loading => {
            Some(t!("typing.char_table.project_favorites_loading_tooltip"))
        }
        ProjectDocumentState::Unreadable => {
            Some(t!("typing.char_table.project_favorites_unreadable_tooltip"))
        }
        ProjectDocumentState::QuarantineFailed => Some(t!(
            "typing.char_table.project_favorites_quarantine_failed_tooltip"
        )),
    }
}

/// Status line shown above the grid for a project document that cannot be saved.
///
/// `None` for every state the user needs no explanation for: no project open, a
/// load still running, and a healthy or merely malformed document (the latter
/// repairs itself on the next toggle).
#[must_use]
fn project_favorites_status(state: ProjectDocumentState) -> Option<&'static str> {
    match state {
        ProjectDocumentState::Unbound
        | ProjectDocumentState::Ready
        | ProjectDocumentState::Loading
        | ProjectDocumentState::Invalid => None,
        ProjectDocumentState::Unreadable => {
            Some(t!("typing.char_table.project_favorites_unreadable_status"))
        }
        ProjectDocumentState::QuarantineFailed => Some(t!(
            "typing.char_table.project_favorites_quarantine_failed_status"
        )),
    }
}

/// Draws the expanded symbol's per-font variants block.
///
/// The first cell is always the built-in interface font: it stands for the whole
/// bundled `fonts/ui` fallback chain and is therefore excluded from the coverage
/// map (`coverage.rs`), not missing from it. The remaining cells come from the
/// coverage map, each rendered in that font's OWN typeface.
fn draw_variants_block(
    state: &CharTableState,
    ui: &mut egui::Ui,
    ch: char,
    fonts: &[FontEntry],
    base_font_identity: Option<&str>,
    metrics: CellMetrics,
    budget: &mut FontRegistrationBudget,
) -> Option<CharTableAction> {
    let in_flight = state.coverage_in_flight();
    let font_indices: Vec<usize> = state.fonts_for_char(ch).to_vec();
    let mut action = None;

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            if draw_variant_cell(
                ui,
                ch,
                metrics,
                egui::FontFamily::Proportional,
                t!("typing.fonts.bundled_ui_font_label"),
            )
            .clicked()
            {
                action = Some(insert_action(ch, BUNDLED_UI_FONT_IDENTITY, base_font_identity));
            }

            if in_flight {
                // A partial list would read as "only these fonts have the glyph",
                // which is exactly the wrong statement while the job is running.
                ui.label(t!("typing.char_table.coverage_in_progress_status"));
                return;
            }

            for font in font_indices.iter().filter_map(|&idx| fonts.get(idx)) {
                let family = variant_font_family(ui.ctx(), font, budget);
                if draw_variant_cell(ui, ch, metrics, family, font.display_label()).clicked() {
                    action = Some(insert_action(
                        ch,
                        &font.render_identity_name(),
                        base_font_identity,
                    ));
                }
            }
        });
    });

    action
}

/// Draws one variant cell (the glyph in a specific typeface) and returns its response.
fn draw_variant_cell(
    ui: &mut egui::Ui,
    ch: char,
    metrics: CellMetrics,
    family: egui::FontFamily,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(metrics.size(), egui::Sense::click());
    if response.hovered() || response.contains_pointer() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(3),
            ui.visuals().widgets.hovered.bg_fill,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ch,
        egui::FontId::new(metrics.glyph_px, family),
        ui.visuals().text_color(),
    );
    response.on_hover_text(tooltip)
}

/// Per-frame budget for new egui font-family registrations.
///
/// See [`MAX_FONT_REGISTRATIONS_PER_FRAME`] for why the budget exists.
#[derive(Debug)]
struct FontRegistrationBudget {
    /// Registrations still allowed this frame.
    remaining: usize,
    /// Whether at least one cell had to fall back to the UI font because the
    /// budget ran out, i.e. whether another frame is owed.
    outstanding: bool,
}

impl FontRegistrationBudget {
    /// A fresh full budget with nothing outstanding.
    fn new() -> Self {
        Self {
            remaining: MAX_FONT_REGISTRATIONS_PER_FRAME,
            outstanding: false,
        }
    }
}

/// Resolves the egui family that draws `font`'s own typeface, honouring `budget`.
///
/// Returns `FontFamily::Proportional` (the UI font) whenever the family is not usable
/// in THIS frame — the budget is exhausted, the file is still being read off the GUI
/// thread, or the registration has not taken effect yet — and sets
/// `budget.outstanding`, which is what asks for another frame. A font that cannot be
/// previewed at all (unreadable file, data egui refuses) is remembered by
/// `widgets::font_preview` itself and costs nothing on later frames.
fn variant_font_family(
    ctx: &egui::Context,
    font: &FontEntry,
    budget: &mut FontRegistrationBudget,
) -> egui::FontFamily {
    // The identity names the font (and keys the egui family); the content hash pins WHICH
    // bytes that name stood for, so a replaced file is not drawn from the stale
    // registration; the path is only where the bytes are read from on first registration.
    let path = font.path();
    let identity = font.render_identity_name();
    let content_hash = font.content_hash();
    let face_index = font.representative_face_index();
    let family_name = combo_font_family_name(&identity, content_hash, face_index);
    let family = egui::FontFamily::Name(family_name.into());
    // A font already bound costs nothing and never touches the budget.
    if is_font_family_bound(ctx, &family) {
        return family;
    }
    if budget.remaining == 0 {
        budget.outstanding = true;
        return egui::FontFamily::Proportional;
    }
    budget.remaining -= 1;

    match request_font_family(ctx, &identity, content_hash, path, face_index) {
        PreviewFontFamily::Ready(family) => family,
        PreviewFontFamily::Pending => {
            budget.outstanding = true;
            egui::FontFamily::Proportional
        }
        // `font_preview` has already logged the reason once; repeating it here would
        // write a line per frame for as long as the block stays open.
        PreviewFontFamily::Unavailable => egui::FontFamily::Proportional,
    }
}

/// Builds the insertion for `ch` rendered in the font named `identity`.
///
/// A `<font=…>` inline tag is emitted ONLY when `identity` differs from the font
/// already in effect at the caret; picking the font that is already in effect
/// inserts the bare character. `identity` must be a render IDENTITY
/// (`FontEntry::render_identity_name` / `fonts::BUNDLED_UI_FONT_IDENTITY`), never
/// a display label, because that is what the renderer's `FontProvider` resolves.
///
/// LIMITATION: `base_font_identity` is the panel's SELECTED font, not the font of
/// the inline span the caret happens to sit in — there is no helper that resolves
/// an effective inline style for a bare caret (`inline_selection_context` requires
/// a non-empty selection). Inserting inside a `<font=Other>` span therefore emits
/// a redundant tag rather than none; it renders correctly either way. Recorded in
/// `char_table/MODULE_README.md`.
fn insert_action(
    ch: char,
    identity: &str,
    base_font_identity: Option<&str>,
) -> CharTableAction {
    let identity = identity.trim();
    // Same comparison rule as `create_edit::normalize_desired_inline_tag_style`,
    // which also strips a span naming the base font case-insensitively.
    let same_as_base = base_font_identity
        .is_some_and(|base| base.trim().eq_ignore_ascii_case(identity));
    if same_as_base || identity.is_empty() {
        CharTableAction::Insert(ch.to_string())
    } else {
        CharTableAction::Insert(format!("<font={identity}>{ch}</font>"))
    }
}

/// How many cells of `cell_px` (separated by `spacing_px`) fit into `available_px`.
///
/// Always at least 1, so a window narrower than one cell still renders one column
/// per row instead of an empty grid. Clamped to [`MAX_GRID_COLUMNS`], which is
/// what makes the float→integer conversion below exact rather than truncating.
fn grid_columns(available_px: f32, cell_px: f32, spacing_px: f32) -> usize {
    let stride = cell_px + spacing_px;
    if !stride.is_finite() || stride <= 0.0 || !available_px.is_finite() {
        return 1;
    }
    // `available + spacing` because the LAST cell of a row needs no trailing gap.
    let fits = ((available_px + spacing_px) / stride).floor();
    if !fits.is_finite() || fits < 1.0 {
        return 1;
    }
    // The value is now finite and inside `1.0..=MAX_GRID_COLUMNS`, a range every
    // member of which `f32` represents as an exact small integer — so the
    // narrowing conversion below can neither truncate a fraction nor wrap.
    // (`f32 -> usize` has no `try_from`; this mirrors
    // `mesh_geometry::preview_char_budget`.)
    let fits = fits.min(MAX_GRID_COLUMNS);
    fits as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_columns_is_at_least_one_and_counts_gaps_correctly() {
        // Three 20px cells with 4px gaps need 20*3 + 4*2 = 68px.
        assert_eq!(grid_columns(68.0, 20.0, 4.0), 3);
        assert_eq!(grid_columns(67.0, 20.0, 4.0), 2);
        // Narrower than one cell, zero, negative, NaN: still one column.
        assert_eq!(grid_columns(5.0, 20.0, 4.0), 1);
        assert_eq!(grid_columns(0.0, 20.0, 4.0), 1);
        assert_eq!(grid_columns(-100.0, 20.0, 4.0), 1);
        assert_eq!(grid_columns(f32::NAN, 20.0, 4.0), 1);
        assert_eq!(grid_columns(100.0, 0.0, 0.0), 1);
        // An absurd width is clamped instead of overflowing.
        assert_eq!(grid_columns(f32::MAX, 1.0, 0.0), 4096);
    }

    #[test]
    fn insert_action_emits_a_font_tag_only_for_a_different_font() {
        assert_eq!(
            insert_action('→', "Alpha", Some("Alpha")),
            CharTableAction::Insert("→".to_string()),
            "the font already in effect must not sprout a tag"
        );
        assert_eq!(
            insert_action('→', "alpha", Some("ALPHA")),
            CharTableAction::Insert("→".to_string()),
            "the base comparison is case-insensitive, like the inline-tag normalizer"
        );
        assert_eq!(
            insert_action('→', "Beta", Some("Alpha")),
            CharTableAction::Insert("<font=Beta>→</font>".to_string())
        );
        assert_eq!(
            insert_action('→', BUNDLED_UI_FONT_IDENTITY, Some("Alpha")),
            CharTableAction::Insert(format!("<font={BUNDLED_UI_FONT_IDENTITY}>→</font>"))
        );
        // No base font at all (empty panel list): tag explicitly rather than
        // guessing that the caret is already in that font.
        assert_eq!(
            insert_action('→', "Beta", None),
            CharTableAction::Insert("<font=Beta>→</font>".to_string())
        );
    }

    /// Every state that blocks the project-favorite button must explain ITSELF.
    /// The old shared "no project is open" text was wrong for three of the four
    /// blocked states, all of which are reachable with a project open.
    #[test]
    fn each_blocked_document_state_has_its_own_button_wording() {
        assert!(project_favorite_disabled_reason(ProjectDocumentState::Ready).is_none());
        assert!(
            project_favorite_disabled_reason(ProjectDocumentState::Invalid).is_none(),
            "a malformed document is repaired by the toggle itself"
        );

        let blocked = [
            ProjectDocumentState::Unbound,
            ProjectDocumentState::Loading,
            ProjectDocumentState::Unreadable,
            ProjectDocumentState::QuarantineFailed,
        ];
        // No catalog is installed in a unit test, so `t!` yields the KEY: what is
        // asserted here is that each state maps to its own key. That those keys
        // exist in all five catalogs is enforced by `ms-i18n`'s key-validation
        // test, and the assertion holds unchanged if some other test in this
        // binary has installed a locale.
        let mut seen: Vec<&'static str> = Vec::new();
        for state in blocked {
            let Some(reason) = project_favorite_disabled_reason(state) else {
                panic!("{state:?} must disable the button");
            };
            assert!(!reason.is_empty(), "{state:?} must state a reason");
            assert!(
                !seen.contains(&reason),
                "{state:?} reuses another state's wording: {reason}"
            );
            seen.push(reason);
        }
    }

    /// The two states that leave the list unsavable also say so above the grid;
    /// the others must stay silent.
    #[test]
    fn only_unsavable_document_states_show_a_status_line() {
        assert!(project_favorites_status(ProjectDocumentState::Unbound).is_none());
        assert!(project_favorites_status(ProjectDocumentState::Ready).is_none());
        assert!(project_favorites_status(ProjectDocumentState::Loading).is_none());
        assert!(project_favorites_status(ProjectDocumentState::Invalid).is_none());
        let unreadable = project_favorites_status(ProjectDocumentState::Unreadable);
        let quarantine_failed = project_favorites_status(ProjectDocumentState::QuarantineFailed);
        match (unreadable, quarantine_failed) {
            (Some(unreadable), Some(quarantine_failed)) => {
                assert!(!unreadable.is_empty());
                assert!(!quarantine_failed.is_empty());
                assert_ne!(unreadable, quarantine_failed);
            }
            _ => panic!("both unsavable states must explain themselves"),
        }
    }

    #[test]
    fn every_group_key_has_its_own_localized_tab_label() {
        for group in CharTableState::groups() {
            let label = tab_label(group.key);
            assert_ne!(
                label, group.key,
                "group {} must map to a catalog key, not to its own identity",
                group.key
            );
        }
        assert_ne!(tab_label(FAVORITES_TAB_KEY), FAVORITES_TAB_KEY);
        assert_eq!(tab_label("no_such_group"), "no_such_group");
    }
}
