/*
File: crates/ms-tab-page-manager/src/clean_cards.rs

Purpose:
Draws everything of the page-manager grid that is about clean layers: the clean card under a
page card, the dashed link between them with its status label, the two-step in-gap unlink
control, and the «Клин без страницы» section (header row, unassigned clean cards and their
"bind to …" / delete context menu). Positions come from `grid_layout.rs` through `grid.rs`;
what a page's clean IS comes from `clean_link.rs`; every mutation goes through the
`clean.rs` entry points.

Key items:
- PageManagerTabState::draw_page_clean_slot(): link gap + clean card of one page.
- PageManagerTabState::draw_unassigned_header() / draw_unassigned_card(): the bottom section.
- UnlinkArm / unlink_transition(): armed state and pure state machine of the in-gap unlink
  control, with the arm-time confirm guard (unit-tested).
- bind_menu_groups() / bind_entry_label() / bind_needs_warning() / problem_hover_text(): pure
  helpers of the bind menu and the problem tooltip (unit-tested).

Notes:
- The gap hover is a pure pointer query (`Ui::rect_contains_pointer`) that registers no hitbox,
  so it never steals clicks from cards or buttons. The unlink control sits INSIDE the gap (top
  of the dashed line), so hovering it keeps the gap hovered and nothing flickers.
- Only visible rows reach this file (plus the grid's one prefetch row), so thumbnails are
  requested only for those clean cards. Model-sourced thumbnails clone the page's `Arc` under
  one short model lock, and only when `thumbs.rs` says the cached revision is out of date.
- Every interaction uses an explicit id (`("pm_clean_card", page)`, `("pm_unassigned", name)`),
  never an auto id, because rows are placed by hand. A double-click on either card opens the
  page viewer (`viewer.rs`); single clicks select nothing.
*/

use std::ffi::OsStr;

use eframe::egui;

use ms_models::clean_assign::{CleanPageFit, UnassignedClean};
use ms_project::ProjectData;

use super::clean::CleanDialog;
use super::clean_link::{BindTargets, CleanLinkProblem, CleanThumbSource, PageCleanLink};
use super::grid::{draw_thumb_box, paint_card_background, to_screen, CardThumb, GridFrame, CARD_INNER_MARGIN};
use super::grid_layout::{label_rect, unlink_button_rect, LayoutRect};
use super::thumbs::ModelCleanThumb;
use super::viewer::ViewerTarget;
use super::PageManagerTabState;

/// Width of the dashed link line, points.
const LINK_STROKE_WIDTH: f32 = 2.0;
/// Dash and gap lengths of the link line, points.
const LINK_DASH_LENGTH: f32 = 6.0;
const LINK_DASH_GAP: f32 = 4.0;
/// Side of the painted circled-"!" problem icon, points.
const PROBLEM_ICON_SIDE: f32 = 14.0;
/// Tallest the "bind to …" page list may grow before it scrolls, points.
const BIND_MENU_MAX_HEIGHT: f32 = 320.0;

/// Explicit interaction id of the clean card linked to page `page_idx`.
fn clean_card_id(page_idx: usize) -> egui::Id {
    egui::Id::new(("pm_clean_card", page_idx))
}

/// Explicit interaction id of the unassigned clean card of file `file_name`. Keyed by name, so a
/// card keeps its context-menu state when the list above it changes.
fn unassigned_card_id(file_name: &OsStr) -> egui::Id {
    egui::Id::new(("pm_unassigned", file_name))
}

/// What a clean card shows under its name line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanCardDetail {
    /// The clean's `[width, height]`, or "size unknown".
    Size(Option<[u32; 2]>),
    /// A warning line (circled "!" + `short`) whose tooltip is `hover`.
    Problem { short: String, hover: String },
}

/// What the user did with the in-gap unlink control this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnlinkEvent {
    /// No click.
    None,
    /// The first ("отвязать") button was clicked: arm this page.
    Arm,
    /// "отмена" was clicked while armed.
    Cancel,
    /// The armed "отвязать" was clicked with a plain single click (not the 2nd/3rd click of a
    /// multi-click burst).
    Confirm,
}

/// An armed in-gap unlink control: the page and the `InputState::time` (seconds) it was armed at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UnlinkArm {
    pub(super) page_idx: usize,
    pub(super) armed_at: f64,
}

/// The armed-state machine of the in-gap unlink control of page `page_idx` at input time `now`.
///
/// Returns the next arm and whether the unlink must be dispatched now. Only the armed page can
/// confirm or cancel; arming one page disarms any other. A confirm is accepted only when at least
/// `min_confirm_delay` seconds passed since arming — a faster confirm is ignored and the arm stays
/// — so no single physical multi-click gesture can both arm and confirm. Disarming when the
/// pointer leaves the gap is the caller's job (see `draw_grid`).
#[must_use]
fn unlink_transition(armed: Option<UnlinkArm>, page_idx: usize, event: UnlinkEvent, now: f64, min_confirm_delay: f64) -> (Option<UnlinkArm>, bool) {
    match event {
        UnlinkEvent::None => (armed, false),
        UnlinkEvent::Arm => (Some(UnlinkArm { page_idx, armed_at: now }), false),
        UnlinkEvent::Cancel => (armed.filter(|arm| arm.page_idx != page_idx), false),
        UnlinkEvent::Confirm => match armed {
            Some(arm) if arm.page_idx == page_idx && now - arm.armed_at >= min_confirm_delay => (None, true),
            Some(_) | None => (armed, false),
        },
    }
}

/// Minimum seconds between arming and confirming an unlink: egui's triple-click window (twice
/// `InputOptions::max_double_click_delay`, egui input_state/mod.rs:1169-1170). Any click closer
/// than that to the arming click may belong to the same double/triple-click burst.
fn unlink_confirm_delay(ctx: &egui::Context) -> f64 {
    ctx.options(|options| options.input_options.max_double_click_delay) * 2.0
}

/// One headed group of the "bind to …" menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindGroup {
    /// «Без клина»: binding just attaches.
    WithoutClean,
    /// «Есть клин, будет замена»: the page's clean is first kept as an unassigned file.
    Replace,
}

impl BindGroup {
    /// The group's localized heading.
    fn heading(self) -> &'static str {
        match self {
            Self::WithoutClean => t!("page_manager.unassigned.bind_group_without_clean"),
            Self::Replace => t!("page_manager.unassigned.bind_group_replace"),
        }
    }
}

/// The bind menu's groups in display order (pages without a clean first); a group with no pages
/// is omitted, so its heading is never shown alone.
#[must_use]
fn bind_menu_groups(targets: &BindTargets) -> Vec<(BindGroup, &[usize])> {
    [(BindGroup::WithoutClean, targets.without_clean.as_slice()), (BindGroup::Replace, targets.with_clean.as_slice())]
        .into_iter()
        .filter(|(_, pages)| !pages.is_empty())
        .collect()
}

/// A bind-menu entry: `{page_number}. {page_file_name}`, the page card's own caption (data, not a
/// translatable string).
#[must_use]
fn bind_entry_label(page_idx: usize, page_file_name: &str) -> String {
    format!("{}. {page_file_name}", page_idx + 1)
}

/// Whether binding with this fit needs the "bound as-is, will not load" confirmation: anything but
/// a known exact match (a mismatch, an unreadable file or page, or an unknown fit).
#[must_use]
fn bind_needs_warning(fit: Option<&CleanPageFit>) -> bool {
    !matches!(fit, Some(CleanPageFit::Matches { .. }))
}

/// The tooltip describing why a page's clean does not bind.
#[must_use]
fn problem_hover_text(problem: &CleanLinkProblem) -> String {
    match problem {
        CleanLinkProblem::SizeMismatch { clean, page } => tf!(
            "page_manager.clean_link.size_mismatch_tooltip",
            clean_width = clean[0],
            clean_height = clean[1],
            page_width = page[0],
            page_height = page[1]
        ),
        CleanLinkProblem::CleanUnreadable(error) => tf!("page_manager.clean_link.clean_unreadable_tooltip", error = error),
        CleanLinkProblem::PageUnreadable(error) => tf!("page_manager.clean_link.page_unreadable_tooltip", error = error),
    }
}

/// Paints the circled "!" problem glyph (same construction as the help hint's circled "?").
fn paint_problem_icon(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(PROBLEM_ICON_SIDE), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        painter.circle_stroke(rect.center(), PROBLEM_ICON_SIDE / 2.0 - 1.0, egui::Stroke::new(1.5, color));
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "!", egui::FontId::proportional(PROBLEM_ICON_SIDE - 4.0), color);
    }
}

/// Size of a status row (optional problem icon + `text` in the body font), points.
fn status_row_size(ui: &egui::Ui, text: &str, icon: bool) -> egui::Vec2 {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Body.resolve(ui.style()), egui::Color32::PLACEHOLDER);
    let icon_width = if icon { PROBLEM_ICON_SIDE + ui.spacing().item_spacing.x } else { 0.0 };
    let height = ui.text_style_height(&egui::TextStyle::Body).max(if icon { PROBLEM_ICON_SIDE } else { 0.0 });
    egui::vec2(galley.size().x + icon_width, height)
}

/// Draws a status row (optional circled "!" + `text` in `color`) in exactly its own size, so a
/// centred parent layout centres the whole row. Returns the row's hover-sensing response.
fn status_row(ui: &mut egui::Ui, text: &str, color: egui::Color32, icon: bool) -> egui::Response {
    let size = status_row_size(ui, text, icon);
    ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if icon {
            paint_problem_icon(ui, color);
        }
        ui.add(egui::Label::new(egui::RichText::new(text).color(color)).selectable(false));
    })
    .response
}

impl PageManagerTabState {
    /// Draws page `page_idx`'s link gap (dashed line, status label, unlink control on hover) and
    /// its clean card; `gap` / `clean` are content-relative, `origin` is the content's screen
    /// origin. Draws nothing for a page without a clean (its row reserved the slot for another
    /// page). Returns whether this page's unlink control is armed AND its gap is under the pointer
    /// this frame, which is what keeps an arm alive.
    pub(super) fn draw_page_clean_slot(&mut self, ui: &mut egui::Ui, frame: &GridFrame<'_>, gap: LayoutRect, clean: LayoutRect, origin: egui::Pos2, page_idx: usize) -> bool {
        let Some(link) = self.page_clean_link(page_idx).cloned() else {
            return false;
        };
        let problem_hover = match &link {
            PageCleanLink::None => return false,
            PageCleanLink::Ok { .. } => None,
            PageCleanLink::Problem { problem, .. } => Some(problem_hover_text(problem)),
        };
        let gap_screen = to_screen(gap, origin);
        // Pointer query only (no hitbox): respects the clip rect and layers above the grid, so an
        // open menu or dialog over the gap does not count as hovering it.
        let gap_hovered = ui.rect_contains_pointer(gap_screen);
        if ui.is_rect_visible(gap_screen) {
            let color = if problem_hover.is_some() { ms_theme::status::WARNING } else { ms_theme::status::SUCCESS };
            let x = gap_screen.center().x;
            ui.painter().extend(egui::Shape::dashed_line(
                &[egui::pos2(x, gap_screen.top()), egui::pos2(x, gap_screen.bottom())],
                egui::Stroke::new(LINK_STROKE_WIDTH, color),
                LINK_DASH_LENGTH,
                LINK_DASH_GAP,
            ));
            draw_link_label(ui, gap, origin, page_idx, problem_hover.as_deref());
        }
        // Drawn after the label so it stays on top where the two could touch.
        let armed_and_hovered = gap_hovered && self.draw_unlink_control(ui, frame.project, gap, origin, page_idx);
        self.draw_linked_clean_card(ui, to_screen(clean, origin), page_idx, &link, problem_hover);
        armed_and_hovered
    }

    /// The two-step unlink control at the top of page `page_idx`'s gap: a red "отвязать" that
    /// arms, then "отмена" + an armed "отвязать" that dispatches [`Self::request_unlink`].
    /// Disabled with a reason while [`Self::clean_mutation_blocked`]. Returns whether the page is
    /// armed after this frame's click.
    fn draw_unlink_control(&mut self, ui: &mut egui::Ui, project: &ProjectData, gap: LayoutRect, origin: egui::Pos2, page_idx: usize) -> bool {
        let armed = self.unlink_armed.is_some_and(|arm| arm.page_idx == page_idx);
        let blocked = self.clean_mutation_blocked();
        let unlink_text = t!("page_manager.clean_link.unlink_button");
        let cancel_text = t!("page_manager.clean_link.unlink_cancel_button");
        // Measure the buttons first so the control can be centred on the line.
        let button_width = |text: &str| {
            ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Button.resolve(ui.style()), egui::Color32::PLACEHOLDER).size().x + 2.0 * ui.spacing().button_padding.x
        };
        let width = if armed { button_width(cancel_text) + ui.spacing().item_spacing.x + button_width(unlink_text) } else { button_width(unlink_text) };
        let rect = to_screen(unlink_button_rect(&gap, [width, ui.spacing().interact_size.y]), origin);
        let mut control = ui.new_child(egui::UiBuilder::new().id_salt(("pm_unlink", page_idx)).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let blocked_hint = t!("page_manager.clean_link.blocked_tooltip");
        let mut event = UnlinkEvent::None;
        if armed {
            if control.button(cancel_text).clicked() {
                event = UnlinkEvent::Cancel;
            }
            let confirm = control
                .add_enabled(!blocked, egui::Button::new(unlink_text).fill(ms_theme::status::DESTRUCTIVE_ARMED_FILL))
                .on_hover_text(t!("page_manager.clean_link.unlink_tooltip"))
                .on_disabled_hover_text(blocked_hint);
            // A double- or triple-click delivers several clicks; the confirm must be its own
            // deliberate click. The multi-click filter covers bursts on this button, the arm-time
            // guard in `unlink_transition` covers bursts that started on the arm button.
            if confirm.clicked() && !confirm.double_clicked() && !confirm.triple_clicked() {
                event = UnlinkEvent::Confirm;
            }
        } else {
            let arm = control
                .add_enabled(!blocked, egui::Button::new(egui::RichText::new(unlink_text).color(ms_theme::status::ERROR)))
                .on_hover_text(t!("page_manager.clean_link.unlink_tooltip"))
                .on_disabled_hover_text(blocked_hint);
            if arm.clicked() {
                event = UnlinkEvent::Arm;
            }
        }
        let now = ui.input(|input| input.time);
        let (next, dispatch) = unlink_transition(self.unlink_armed, page_idx, event, now, unlink_confirm_delay(ui.ctx()));
        self.unlink_armed = next;
        if dispatch {
            self.request_unlink(project, page_idx);
        }
        self.unlink_armed.is_some_and(|arm| arm.page_idx == page_idx)
    }

    /// Draws the clean card linked to page `page_idx` in `rect` (screen coordinates): caption,
    /// thumbnail over the checkerboard, `{page_number}. {clean file name}` and the size (or the
    /// problem line when the size is unknown). A double-click opens it in the page viewer; its
    /// context menu offers the unlink (with the confirmation dialog).
    fn draw_linked_clean_card(&mut self, ui: &mut egui::Ui, rect: egui::Rect, page_idx: usize, link: &PageCleanLink, problem_hover: Option<String>) {
        let (thumb, file_name, detail) = match link {
            PageCleanLink::None => return,
            PageCleanLink::Ok { thumb, file_name, size } => {
                let (thumb, probed) = self.link_thumb(page_idx, thumb);
                // The model may not remember a size; the thumbnail knows the pixels it scaled.
                let size = size.or_else(|| probed.map(|(width, height)| [width, height]));
                (thumb, file_name.clone(), CleanCardDetail::Size(size))
            }
            PageCleanLink::Problem { file, size, .. } => {
                let file_name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                let detail = match (size, &problem_hover) {
                    (None, Some(hover)) => CleanCardDetail::Problem { short: t!("page_manager.clean_link.problem_label").to_string(), hover: hover.clone() },
                    (size, _) => CleanCardDetail::Size(*size),
                };
                (self.path_card_thumb(file).0, file_name, detail)
            }
        };
        let mut response = ui.interact(rect, clean_card_id(page_idx), egui::Sense::click());
        paint_clean_card(ui, rect, response.hovered(), ("pm_clean_card_content", page_idx), thumb, &format!("{}. {file_name}", page_idx + 1), &detail);
        if let Some(hover) = problem_hover {
            response = response.on_hover_text(hover);
        }
        if response.double_clicked() {
            self.open_viewer(ViewerTarget::BoundClean(page_idx));
        }
        response.context_menu(|ui| {
            if ui
                .add_enabled(!self.clean_mutation_blocked(), egui::Button::new(t!("page_manager.clean_detach_button")))
                .on_disabled_hover_text(t!("page_manager.clean_link.blocked_tooltip"))
                .clicked()
            {
                self.start_detach_clean(page_idx);
            }
        });
    }

    /// The thumbnail of an OK clean — the model's pixels (unsaved edits included) or its bound
    /// file — plus the full `(width, height)` the thumbnail was scaled from, when known.
    fn link_thumb(&mut self, page_idx: usize, source: &CleanThumbSource) -> (CardThumb, Option<(u32, u32)>) {
        match source {
            CleanThumbSource::File(path) => self.path_card_thumb(path),
            CleanThumbSource::Model => self.model_card_thumb(page_idx),
        }
    }

    /// The model-sourced clean thumbnail of page `page_idx`, requesting a fresh downscale when the
    /// cached one is older than the model revision. The model lock is held only to read the
    /// revision and clone the page's `Arc`; the downscale runs on the thumbnail worker. Also
    /// returns the full overlay size of a ready thumbnail.
    fn model_card_thumb(&mut self, page_idx: usize) -> (CardThumb, Option<(u32, u32)>) {
        if let Some(seen) = self.overlays_revision_seen
            && self.thumbs.model_clean_thumb_wanted(page_idx, seen)
            && let Some(model) = self.overlays_model.as_ref()
        {
            match model.lock() {
                Ok(guard) => {
                    let (revision, rgba) = (guard.revision(), guard.overlay_rgba(page_idx));
                    drop(guard);
                    match rgba {
                        Some(rgba) => {
                            self.thumbs.request_model_clean_thumb(page_idx, rgba, revision);
                        }
                        // Materialized but holding no pixels: nothing to downscale.
                        None => return (CardThumb::Failed, None),
                    }
                }
                // Same policy as the badge refresh: a poisoned model is skipped this frame.
                Err(_) => ms_log::runtime_log::log_warn("[page-manager::clean-cards] clean overlays model lock poisoned; clean thumbnail skipped"),
            }
        }
        match self.thumbs.model_clean_thumb(page_idx) {
            ModelCleanThumb::Ready { texture, size, full_size } => (CardThumb::Ready(texture, size), Some(full_size)),
            // A failure at an older revision is stale: a downscale of the current pixels is due.
            ModelCleanThumb::Failed { revision } if Some(revision) != self.overlays_revision_seen => (CardThumb::Pending, None),
            ModelCleanThumb::Failed { .. } => (CardThumb::Failed, None),
            ModelCleanThumb::Pending => (CardThumb::Pending, None),
        }
    }

    /// Draws the «Клин без страницы» header in `rect` (screen coordinates): a separator line,
    /// the title, the file count, a refresh button (forces an inventory rescan) and a spinner
    /// while a scan runs.
    pub(super) fn draw_unassigned_header(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        if !ui.is_rect_visible(rect) {
            return;
        }
        ui.painter().hline(rect.x_range(), rect.top(), ui.visuals().widgets.noninteractive.bg_stroke);
        let mut header = ui.new_child(egui::UiBuilder::new().id_salt("pm_unassigned_header").max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
        header.add(egui::Label::new(egui::RichText::new(t!("page_manager.unassigned.header_title")).strong()).selectable(false));
        header.add(egui::Label::new(egui::RichText::new(tf!("page_manager.unassigned.header_count", count = self.unassigned_cleans().len())).weak()).selectable(false));
        if header.button(t!("page_manager.clean_refresh_button")).clicked() {
            self.request_clean_rescan();
        }
        if self.clean_scan_in_flight() {
            header.spinner();
        }
    }

    /// Draws unassigned clean `item_idx` (index into [`Self::unassigned_cleans`]) as a clean card
    /// in `rect` (screen coordinates): caption, thumbnail, file name, size — or, for an unreadable
    /// file, the problem line with the read error. A double-click opens it in the page viewer; its
    /// context menu binds or deletes it.
    pub(super) fn draw_unassigned_card(&mut self, ui: &mut egui::Ui, frame: &GridFrame<'_>, rect: egui::Rect, item_idx: usize) {
        let Some(item) = self.unassigned_cleans().get(item_idx).cloned() else {
            return;
        };
        // `scan_clean_inventory` never lists a file without a copy.
        let Some(probe) = item.effective().cloned() else {
            return;
        };
        let (thumb, detail, unreadable) = match &probe.size {
            Ok(size) => (self.path_card_thumb(&probe.path).0, CleanCardDetail::Size(Some(*size)), None),
            Err(error) => {
                // Its header did not parse, so a decode cannot succeed: request none.
                let hover = tf!("page_manager.clean_link.clean_unreadable_tooltip", error = error);
                (CardThumb::Failed, CleanCardDetail::Problem { short: t!("page_manager.clean_link.problem_label").to_string(), hover: hover.clone() }, Some(hover))
            }
        };
        let response = ui.interact(rect, unassigned_card_id(&item.file_name), egui::Sense::click());
        paint_clean_card(ui, rect, response.hovered(), ("pm_unassigned_content", &item.file_name), thumb, &item.file_name.to_string_lossy(), &detail);
        if response.double_clicked() {
            self.open_viewer(ViewerTarget::Unassigned(item.file_name.clone()));
        }
        response.context_menu(|ui| {
            self.unassigned_context_menu(ui, frame.project, &item, unreadable.as_deref());
        });
    }

    /// Context menu of an unassigned clean: "Привязать к …" (a scrollable submenu of pages in the
    /// «Без клина» / «Есть клин, будет замена» groups; disabled for an unreadable file) and
    /// "Удалить" (with its confirmation). Both are disabled while clean mutations are blocked.
    fn unassigned_context_menu(&mut self, ui: &mut egui::Ui, project: &ProjectData, item: &UnassignedClean, unreadable: Option<&str>) {
        let blocked = self.clean_mutation_blocked();
        let targets = self.clean_bind_targets();
        let mut choice: Option<usize> = None;
        let bind = ui
            .add_enabled_ui(!blocked && unreadable.is_none(), |ui| {
                // Inside a context menu `menu_button` becomes a hover-opened submenu.
                ui.menu_button(t!("page_manager.unassigned.bind_menu"), |ui| {
                    egui::ScrollArea::vertical().id_salt(("pm_bind_menu", &item.file_name)).max_height(BIND_MENU_MAX_HEIGHT).show(ui, |ui| {
                        for (group_idx, (group, pages)) in bind_menu_groups(&targets).into_iter().enumerate() {
                            if group_idx > 0 {
                                ui.separator();
                            }
                            ui.add(egui::Label::new(egui::RichText::new(group.heading()).weak()).selectable(false));
                            for &page_idx in pages {
                                let page_file_name = project.pages.get(page_idx).and_then(|page| page.path.file_name()).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                                if ui.button(bind_entry_label(page_idx, &page_file_name)).clicked() {
                                    choice = Some(page_idx);
                                    ui.close();
                                }
                            }
                        }
                    });
                })
                .response
            })
            .inner;
        bind.on_disabled_hover_text(unreadable.unwrap_or(t!("page_manager.clean_link.blocked_tooltip")));
        if let Some(page_idx) = choice {
            self.choose_bind_target(project, &item.file_name, page_idx);
        }
        if ui
            .add_enabled(!blocked, egui::Button::new(t!("page_manager.clean_delete_button")))
            .on_disabled_hover_text(t!("page_manager.clean_link.blocked_tooltip"))
            .clicked()
        {
            self.clean_dialog = Some(CleanDialog::DeleteUnassigned { file_name: item.file_name.clone() });
        }
    }

    /// Binds unassigned clean `file_name` to page `page_idx` right away when it is a known exact
    /// size match; otherwise opens the "bound as-is, will not load" warning first. A page that
    /// already has a clean needs no extra confirmation: its clean is kept as an unassigned file.
    fn choose_bind_target(&mut self, project: &ProjectData, file_name: &OsStr, page_idx: usize) {
        let fit = self.clean_bind_fit(file_name, page_idx);
        if bind_needs_warning(fit.as_ref()) {
            self.clean_dialog = Some(CleanDialog::BindMismatch { file_name: file_name.to_os_string(), page_idx, fit });
        } else {
            self.request_bind(project, file_name, page_idx);
        }
    }
}

/// Draws the link label in the middle of the gap: "клин ✓" in green, or a circled "!" +
/// "Проблема" in yellow whose tooltip is `problem_hover`, on the default dark canvas frame
/// (`Frame::canvas`: extreme-background fill, theme corner radius and stroke, no shadow).
fn draw_link_label(ui: &mut egui::Ui, gap: LayoutRect, origin: egui::Pos2, page_idx: usize, problem_hover: Option<&str>) {
    let (text, color) = match problem_hover {
        None => (t!("page_manager.card.clean_present_badge"), ms_theme::status::SUCCESS),
        Some(_) => (t!("page_manager.clean_link.problem_label"), ms_theme::status::WARNING),
    };
    let frame = egui::Frame::canvas(ui.style());
    // The frame's outer size = content + inner margin + stroke on both sides.
    let chrome = frame.inner_margin.sum() + egui::Vec2::splat(2.0 * frame.stroke.width);
    let size = status_row_size(ui, text, problem_hover.is_some()) + chrome;
    let rect = to_screen(label_rect(&gap, [size.x, size.y], ui.spacing().interact_size.y), origin);
    let mut label = ui.new_child(egui::UiBuilder::new().id_salt(("pm_link_label", page_idx)).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let response = frame.show(&mut label, |ui| status_row(ui, text, color, problem_hover.is_some())).response;
    if let Some(hover) = problem_hover {
        response.on_hover_text(hover);
    }
}

/// Draws a clean card in `rect` (screen coordinates): the page-card background, the «Клин»
/// caption, the thumbnail over the checkerboard, `name_line` and `detail`. `id_salt` must be
/// unique per card (its content Ui is placed by hand).
fn paint_clean_card(ui: &mut egui::Ui, rect: egui::Rect, hovered: bool, id_salt: impl egui::AsIdSalt, thumb: CardThumb, name_line: &str, detail: &CleanCardDetail) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    paint_card_background(ui, rect, false, hovered);
    let mut content = ui.new_child(egui::UiBuilder::new().id_salt(id_salt).max_rect(rect.shrink(CARD_INNER_MARGIN)).layout(egui::Layout::top_down(egui::Align::Center)));
    content.add(egui::Label::new(egui::RichText::new(t!("page_manager.clean_card.caption")).strong()).selectable(false));
    draw_thumb_box(&mut content, thumb, true);
    content.add(egui::Label::new(egui::RichText::new(name_line).strong()).truncate().selectable(false));
    match detail {
        CleanCardDetail::Size(Some([width, height])) => {
            content.add(egui::Label::new(egui::RichText::new(tf!("page_manager.card.size_label", width = width, height = height)).weak()).selectable(false));
        }
        CleanCardDetail::Size(None) => {
            content.add(egui::Label::new(egui::RichText::new(t!("page_manager.card.size_unknown")).weak()).selectable(false));
        }
        CleanCardDetail::Problem { short, hover } => {
            status_row(&mut content, short, ms_theme::status::WARNING, true).on_hover_text(hover);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(page_idx: usize, armed_at: f64) -> Option<UnlinkArm> {
        Some(UnlinkArm { page_idx, armed_at })
    }

    #[test]
    fn unlink_arms_confirms_and_cancels_only_its_own_page() {
        // `now` = 10 s, well past the 0.6 s guard of an arm made at 1 s.
        let t = |armed, page, event| unlink_transition(armed, page, event, 10.0, 0.6);
        assert_eq!(t(None, 3, UnlinkEvent::None), (None, false));
        assert_eq!(t(None, 3, UnlinkEvent::Arm), (arm(3, 10.0), false));
        // Arming another page moves the arm (and restarts its clock).
        assert_eq!(t(arm(1, 1.0), 3, UnlinkEvent::Arm), (arm(3, 10.0), false));
        assert_eq!(t(arm(3, 1.0), 3, UnlinkEvent::Confirm), (None, true));
        assert_eq!(t(arm(3, 1.0), 3, UnlinkEvent::Cancel), (None, false));
        // A confirm or cancel of a page that is not armed changes nothing.
        assert_eq!(t(arm(1, 1.0), 3, UnlinkEvent::Confirm), (arm(1, 1.0), false));
        assert_eq!(t(None, 3, UnlinkEvent::Confirm), (None, false));
        assert_eq!(t(arm(1, 1.0), 3, UnlinkEvent::Cancel), (arm(1, 1.0), false));
    }

    #[test]
    fn unlink_confirm_too_soon_after_arming_is_ignored() {
        // Click 1 arms at t = 5.0; click 3 of a fast triple-click lands 0.25 s later.
        let (armed, dispatch) = unlink_transition(None, 2, UnlinkEvent::Arm, 5.0, 0.6);
        assert!(!dispatch);
        let (armed, dispatch) = unlink_transition(armed, 2, UnlinkEvent::Confirm, 5.25, 0.6);
        assert!(!dispatch, "a confirm inside the multi-click window must not unlink");
        assert_eq!(armed, arm(2, 5.0), "the ignored confirm keeps the arm");
        // Exactly at the guard boundary and later, a deliberate click confirms.
        assert_eq!(unlink_transition(armed, 2, UnlinkEvent::Confirm, 5.5, 0.5), (None, true));
        assert_eq!(unlink_transition(armed, 2, UnlinkEvent::Confirm, 9.0, 0.6), (None, true));
    }

    #[test]
    fn bind_menu_omits_empty_groups_and_keeps_order() {
        let both = BindTargets { without_clean: vec![0, 2], with_clean: vec![1] };
        assert_eq!(bind_menu_groups(&both), vec![(BindGroup::WithoutClean, [0, 2].as_slice()), (BindGroup::Replace, [1].as_slice())]);
        let replace_only = BindTargets { without_clean: Vec::new(), with_clean: vec![1] };
        assert_eq!(bind_menu_groups(&replace_only), vec![(BindGroup::Replace, [1].as_slice())]);
        assert!(bind_menu_groups(&BindTargets::default()).is_empty());
        assert_ne!(BindGroup::WithoutClean.heading(), BindGroup::Replace.heading());
    }

    #[test]
    fn bind_entry_uses_one_based_page_number() {
        assert_eq!(bind_entry_label(0, "001.png"), "1. 001.png");
        assert_eq!(bind_entry_label(11, "x.webp"), "12. x.webp");
    }

    #[test]
    fn only_a_known_exact_fit_binds_without_warning() {
        assert!(!bind_needs_warning(Some(&CleanPageFit::Matches { size: [1, 2] })));
        assert!(bind_needs_warning(Some(&CleanPageFit::SizeMismatch { clean: [1, 2], page: [3, 4] })));
        assert!(bind_needs_warning(Some(&CleanPageFit::CleanUnreadable("e".to_string()))));
        assert!(bind_needs_warning(Some(&CleanPageFit::PageUnreadable { clean: [1, 2], error: "e".to_string() })));
        assert!(bind_needs_warning(None));
    }

    #[test]
    fn problem_tooltip_names_both_sizes_or_the_error() {
        // `tf!` answers against the process-global catalog; without one installed it degrades to
        // the bare key. Install the reference catalog (idempotent; no other test of this crate
        // switches the locale, so no cross-test lock is needed in this test binary).
        ms_i18n::set_locale(&ms_i18n::LocaleTag::parse("en").expect("en tag is valid")).expect("en catalog installs");
        let mismatch = problem_hover_text(&CleanLinkProblem::SizeMismatch { clean: [1201, 1802], page: [803, 904] });
        for number in ["1201", "1802", "803", "904"] {
            assert!(mismatch.contains(number), "{mismatch}");
        }
        assert!(problem_hover_text(&CleanLinkProblem::CleanUnreadable("bad header".to_string())).contains("bad header"));
        assert!(problem_hover_text(&CleanLinkProblem::PageUnreadable("no page".to_string())).contains("no page"));
    }
}
