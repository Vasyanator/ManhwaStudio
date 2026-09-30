/*
File: crates/ms-tab-page-manager/src/grid.rs

Purpose:
Card grid of the page-manager tab: virtualized rows of page cards (thumbnail,
page number, file name, pixel size, badges), plus selection handling, the
double-click that opens the page viewer (`viewer.rs`), clean content actions,
and the per-card context menu. Rows that carry clean cards and
the «Клин без страницы» section are placed here but DRAWN by `clean_cards.rs`.

Key structures:
- CardThumb: per-frame snapshot of a card's thumbnail visual state (shared with
  the clean cards).
- GridScrollMemo: the content scroll anchor and the layout it was taken in.

Key functions:
- PageManagerTabState::draw_grid(): the ScrollArea + visible rows + cards.
- PageManagerTabState::path_card_thumb(): requests + reads a file thumbnail.
- paint_card_background() / draw_thumb_box(): card chrome shared with clean cards.
- selection_after_click(): pure click/Ctrl/Shift selection logic (unit-tested).

Notes:
Rows are virtualized through `ScrollArea::show_viewport`: the GUI-free row table
of `grid_layout.rs` (variable row heights, prefix-summed offsets) decides which
rows intersect the viewport and where every card goes, so thumbnails are only
requested for visible cards (plus one prefetch row); the LRU thumbnail cache in
`thumbs.rs` bounds decoded memory. Card interactions use explicit ids
(`card_id`), never auto ids, so a card keeps its click / double-click / context
menu state however the rows above it change height. A page row reserves the
link gap + clean-card slot when ANY page of the row has a clean link
(`page_has_clean_link`), so all cards of a row stay aligned.
Because row heights depend on the clean links, the grid (1) shows a loading
placeholder instead of the ScrollArea until the first clean inventory of the
current pages is installed — a ScrollArea that is not shown keeps its stored
offset, one laid out too short would clamp it — and (2) scrolls by content: the
viewport top is remembered as a `ScrollAnchor` (first item of the top row +
intra-row offset), and when a frame's layout differs from the one the anchor was
taken in, the offset is re-derived from it. User scrolling is never overridden
while the layout is unchanged. The thumbnail LRUs are grown to the drawn card
count every frame (`ThumbRuntime::ensure_visible_capacity`).
*/

use std::collections::BTreeSet;

use eframe::egui;

use ms_models::page_view::PageImageInfo;
use ms_project::ProjectData;
use ms_config::app_tab::AppTab;

use super::clean_link::PageCleanLink;
use super::dialogs::PageManagerDialog;
use super::grid_layout::{GridLayout, GridMetrics, GridRowKind, LayoutRect, ScrollAnchor};
use super::thumbs::{THUMB_LONG_SIDE_PX, ThumbVisual};
use super::viewer::ViewerTarget;
use super::{PageManagerAction, PageManagerTabState};

/// Fixed page-card footprint in points.
const CARD_WIDTH: f32 = 212.0;
const CARD_HEIGHT: f32 = 276.0;
/// Padding between the card border and its content (page and clean cards).
pub(super) const CARD_INNER_MARGIN: f32 = 8.0;
/// Clean-card height: the page card's footprint with the caption line in place of
/// the badge line, so both card kinds share one height.
const CLEAN_CARD_HEIGHT: f32 = CARD_HEIGHT;
/// Vertical gap between a page card and its clean card. Holds the unlink control
/// at its top and the link label at its middle without overlap (see
/// `grid_layout::unlink_button_rect` / `label_rect`).
const LINK_GAP: f32 = 72.0;
/// Height of the «Клин без страницы» section header row.
const SECTION_HEADER_HEIGHT: f32 = 36.0;
/// Corner radius of every card (page, linked clean, unassigned clean).
const CARD_CORNER_RADIUS: u8 = 6;

/// Explicit interaction id of page card `idx`. Independent of the Ui's auto-id
/// counter, so variable-height rows and viewport culling cannot shift which card
/// owns a double-click or an open context menu.
fn card_id(idx: usize) -> egui::Id {
    egui::Id::new(("pm_card", idx))
}

/// Converts a content-relative layout rect to a screen rect at `origin` (the
/// top-left of the scrolled content).
pub(super) fn to_screen(rect: LayoutRect, origin: egui::Pos2) -> egui::Rect {
    egui::Rect::from_min_size(origin + egui::vec2(rect.min[0], rect.min[1]), egui::vec2(rect.size[0], rect.size[1]))
}

/// Per-frame snapshot of a card's thumbnail state, copied out of the cache so
/// the cache borrow does not overlap the child-Ui borrows below.
#[derive(Debug, Clone, Copy)]
pub(super) enum CardThumb {
    /// Texture and its size in points (thumbnail pixels, long side <= 192).
    Ready(egui::TextureId, egui::Vec2),
    Failed,
    Pending,
}

/// Whether page `page_idx` has a clean link (OK or problem) — the per-page input of
/// the row rule: `GridLayout::build` reserves the link gap and clean-card slot for a
/// row iff ANY of its pages answers true; pages of such a row without a clean leave
/// their slot empty. An out-of-range page has none.
#[must_use]
pub(super) fn page_has_clean_link(links: &[PageCleanLink], page_idx: usize) -> bool {
    links.get(page_idx).is_some_and(PageCleanLink::has_clean)
}

/// Paints a card's rounded background and border in `rect`: the selection tint
/// when `selected`, the hovered widget visuals when `hovered`, else the
/// non-interactive ones. Shared by page cards and clean cards.
pub(super) fn paint_card_background(ui: &egui::Ui, rect: egui::Rect, selected: bool, hovered: bool) {
    let visuals = ui.visuals();
    let (fill, stroke) = if selected {
        (visuals.selection.bg_fill.linear_multiply(0.35), visuals.selection.stroke)
    } else if hovered {
        (visuals.widgets.hovered.weak_bg_fill, visuals.widgets.hovered.bg_stroke)
    } else {
        (visuals.widgets.noninteractive.weak_bg_fill, visuals.widgets.noninteractive.bg_stroke)
    };
    ui.painter().rect_filled(rect, egui::CornerRadius::same(CARD_CORNER_RADIUS), fill);
    ui.painter().rect_stroke(rect, egui::CornerRadius::same(CARD_CORNER_RADIUS), stroke, egui::StrokeKind::Inside);
}

/// Draws the fixed-height thumbnail box of a card (full available width,
/// `THUMB_LONG_SIDE_PX` tall) with `thumb` centred in it. `transparency` paints
/// the studio checkerboard under the image, exactly over the image rect, so a
/// clean layer's transparent pixels read as transparent.
pub(super) fn draw_thumb_box(ui: &mut egui::Ui, thumb: CardThumb, transparency: bool) {
    let thumb_box = egui::vec2(ui.available_width(), THUMB_LONG_SIDE_PX as f32);
    ui.allocate_ui_with_layout(thumb_box, egui::Layout::top_down_justified(egui::Align::Center), |ui| {
        ui.set_min_size(thumb_box);
        ui.centered_and_justified(|ui| match thumb {
            CardThumb::Ready(texture_id, size) => {
                if transparency {
                    // Centre the image in the box by hand so the checkerboard can be
                    // painted under exactly the image rect before the image itself.
                    let rect = egui::Rect::from_center_size(ui.max_rect().center(), size);
                    ms_theme::checkerboard::CANVAS.paint(ui.painter(), rect, egui::CornerRadius::ZERO);
                    ui.painter().image(texture_id, rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                } else {
                    ui.add(egui::Image::new((texture_id, size)));
                }
            }
            CardThumb::Failed => {
                ui.label(t!("page_manager.card.thumb_error"));
            }
            CardThumb::Pending => {
                ui.label(t!("page_manager.card.thumb_loading"));
            }
        });
    });
}

/// Computes the selection resulting from a click on card `idx`.
///
/// Plain click selects only `idx`; Ctrl toggles it; Shift selects the contiguous
/// range between the anchor (last plain/Ctrl click) and `idx`. Returns the new
/// selection and the new anchor.
fn selection_after_click(
    selection: &BTreeSet<usize>,
    anchor: Option<usize>,
    idx: usize,
    ctrl: bool,
    shift: bool,
) -> (BTreeSet<usize>, Option<usize>) {
    if shift {
        if let Some(anchor_idx) = anchor {
            let (lo, hi) = if anchor_idx <= idx {
                (anchor_idx, idx)
            } else {
                (idx, anchor_idx)
            };
            // Range selection replaces the previous selection but keeps the anchor,
            // so successive Shift+clicks re-pivot around the same page.
            return ((lo..=hi).collect(), Some(anchor_idx));
        }
        // No anchor yet: behave like a plain click.
        return ([idx].into_iter().collect(), Some(idx));
    }
    if ctrl {
        let mut next = selection.clone();
        if !next.remove(&idx) {
            next.insert(idx);
        }
        return (next, Some(idx));
    }
    ([idx].into_iter().collect(), Some(idx))
}

/// The grid's content scroll position between frames: `anchor` was taken at the viewport top of
/// a frame laid out as `layout`. A later frame whose layout differs restores the anchor instead
/// of keeping the raw pixel offset.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GridScrollMemo {
    layout: GridLayout,
    anchor: ScrollAnchor,
}

/// Read-only per-frame inputs every card of the grid needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct GridFrame<'a> {
    pub(super) project: &'a ProjectData,
    pub(super) page_infos: &'a std::collections::HashMap<usize, PageImageInfo>,
    /// A structural op or save is running; gates the context-menu operations.
    pub(super) op_in_progress: bool,
}

impl PageManagerTabState {
    /// Draws the scrollable page-card grid and handles selection, the double-click
    /// that opens the page viewer, and the per-card context menu. Until the first clean inventory
    /// of the current pages is installed it draws a loading placeholder instead
    /// (no ScrollArea, so its stored offset is not clamped); afterwards the scroll
    /// position follows the content across layout changes (see the file header).
    pub(super) fn draw_grid(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_infos: &std::collections::HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) {
        let page_count = project.pages.len();
        if page_count == 0 {
            ui.centered_and_justified(|ui| {
                ui.label(t!("page_manager.grid.empty"));
            });
            return;
        }
        if self.grid_awaits_clean_inventory() {
            // No gap is drawn, so no arm may survive.
            self.unlink_armed = None;
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.4);
                ui.spinner();
                ui.label(t!("page_manager.grid.loading"));
            });
            return;
        }

        let spacing = ui.spacing().item_spacing;
        let metrics = GridMetrics {
            card: [CARD_WIDTH, CARD_HEIGHT],
            clean_card_h: CLEAN_CARD_HEIGHT,
            link_gap: LINK_GAP,
            spacing: [spacing.x, spacing.y],
            section_header_h: SECTION_HEADER_HEIGHT,
        };
        // Columns are fitted to the width OUTSIDE the scroll area (scroll bar and
        // content margin not subtracted), as the grid always has been.
        let layout = GridLayout::build(&metrics, ui.available_width(), page_count, |idx| page_has_clean_link(&self.clean_links, idx), self.unassigned_cleans().len());
        let frame = GridFrame { project, page_infos, op_in_progress };
        // Set when the armed unlink control's gap is drawn under the pointer this
        // frame; an arm that was not re-confirmed that way is dropped below.
        let mut armed_gap_hovered = false;
        // Rows above the viewport changed height (a clean appeared or went away, pages moved,
        // the column count changed): put the remembered content back at the viewport top. Only
        // on a layout change, so ordinary scrolling is never fought.
        let restore_offset = match &self.grid_scroll {
            Some(memo) if memo.layout != layout => layout.offset_for_anchor(&memo.anchor),
            Some(_) | None => None,
        };
        let mut scroll_area = egui::ScrollArea::vertical().id_salt("page_manager_grid").auto_shrink([false, false]);
        if let Some(offset) = restore_offset {
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }

        let anchor = scroll_area
            .show_viewport(ui, |ui, viewport| {
                // The content Ui spans the whole scroll extent (the column block
                // wide, all rows tall: the size the old per-row layout reached);
                // `viewport` is the visible band in content coordinates (min.y == 0
                // when unscrolled).
                ui.set_height(layout.total_height);
                ui.set_min_width(layout.content_width());
                let origin = ui.max_rect().min;
                let mut rows = layout.visible_rows(viewport.min.y, viewport.max.y);
                // One prefetch row below the viewport, so the next row's
                // thumbnails are requested before it scrolls in.
                rows.end = usize::min(rows.end + 1, layout.rows.len());
                // Before any request of this frame: the LRUs must hold every card drawn now.
                self.thumbs.ensure_visible_capacity(layout.card_count(rows.clone()));
                for row in rows {
                    match layout.rows[row].kind {
                        GridRowKind::Pages { first, count, with_clean } => {
                            for col in 0..count {
                                if let Some(rect) = layout.page_card_rect(row, col) {
                                    self.draw_card(ui, &frame, to_screen(rect, origin), first + col, actions);
                                }
                                if with_clean
                                    && let (Some(gap), Some(clean)) = (layout.gap_rect(row, col), layout.clean_card_rect(row, col))
                                {
                                    armed_gap_hovered |= self.draw_page_clean_slot(ui, &frame, gap, clean, origin, first + col);
                                }
                            }
                        }
                        GridRowKind::UnassignedHeader => {
                            if let Some(rect) = layout.header_rect(row) {
                                self.draw_unassigned_header(ui, to_screen(rect, origin));
                            }
                        }
                        GridRowKind::Unassigned { first, count } => {
                            for col in 0..count {
                                if let Some(rect) = layout.clean_card_rect(row, col) {
                                    self.draw_unassigned_card(ui, &frame, to_screen(rect, origin), first + col);
                                }
                            }
                        }
                    }
                }
                layout.anchor_for_offset(viewport.min.y)
            })
            .inner;
        match (&mut self.grid_scroll, anchor) {
            (Some(memo), Some(anchor)) if memo.layout == layout => memo.anchor = anchor,
            (_, Some(anchor)) => self.grid_scroll = Some(GridScrollMemo { layout, anchor }),
            (_, None) => self.grid_scroll = None,
        }
        if !armed_gap_hovered {
            // The armed gap scrolled away, lost its clean, or the pointer left it:
            // a stale arm must never turn a later click into an unlink.
            self.unlink_armed = None;
        }
    }

    /// Requests (if needed) and snapshots the file thumbnail of `path`, plus the
    /// full image size the decoder probed. Used for page images and clean files.
    pub(super) fn path_card_thumb(&mut self, path: &std::path::Path) -> (CardThumb, Option<(u32, u32)>) {
        self.thumbs.request_thumb_if_needed(path, self.generation);
        match self.thumbs.cache.touch_and_get(path) {
            Some(entry) => (
                match &entry.visual {
                    ThumbVisual::Ready(texture) => CardThumb::Ready(texture.id(), texture.size_vec2()),
                    ThumbVisual::Failed => CardThumb::Failed,
                },
                entry.full_size,
            ),
            None => (CardThumb::Pending, None),
        }
    }

    /// Draws page card `idx` into `rect` (screen coordinates, placed by the grid
    /// layout; nothing is allocated in `ui`) and processes its interactions under
    /// the explicit id `card_id(idx)`.
    fn draw_card(
        &mut self,
        ui: &mut egui::Ui,
        frame: &GridFrame<'_>,
        rect: egui::Rect,
        idx: usize,
        actions: &mut Vec<PageManagerAction>,
    ) {
        let GridFrame { project, page_infos, op_in_progress } = *frame;
        // Copy the thumbnail state out of the cache before any child Ui borrows.
        let (thumb, cached_full_size) = self.path_card_thumb(&project.pages[idx].path);
        // Page pixel size: authoritative geometry first, thumbnail probe second.
        let pixel_size = page_infos
            .get(&idx)
            .map(|info| (info.width_px, info.height_px))
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or(cached_full_size);

        let selected = self.selection.contains(&idx);
        let response = ui.interact(rect, card_id(idx), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            paint_card_background(ui, rect, selected, response.hovered());

            let inner = rect.shrink(CARD_INNER_MARGIN);
            let mut content = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("pm_card_content", idx))
                    .max_rect(inner)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            self.draw_card_content(&mut content, idx, thumb, pixel_size, project);
        }

        // Selection: plain / Ctrl (toggle) / Shift (range) click.
        if response.clicked() {
            let modifiers = ui.ctx().input(|i| i.modifiers);
            let (next, anchor) = selection_after_click(
                &self.selection,
                self.selection_anchor,
                idx,
                modifiers.command,
                modifiers.shift,
            );
            self.selection = next;
            self.selection_anchor = anchor;
        }
        // Double-click opens the full-resolution viewer; switching to another tab
        // stays reachable through the context menu's "Open in: …" entries.
        if response.double_clicked() {
            self.open_viewer(ViewerTarget::Page(idx));
        }
        // A right click on an unselected card re-targets the selection to it, so
        // the context-menu operations act on the card under the cursor.
        if response.secondary_clicked() && !self.selection.contains(&idx) {
            self.selection = [idx].into_iter().collect();
            self.selection_anchor = Some(idx);
        }
        response.context_menu(|ui| {
            self.card_context_menu(ui, idx, project.pages.len(), op_in_progress, actions);
        });
    }

    /// Draws the inside of a card: thumbnail box, page number + file name,
    /// pixel size, and the badges line (a weak "no clean" marker only for pages
    /// without any clean — an existing clean's state is shown on its link — then
    /// bubbles and layers).
    fn draw_card_content(
        &self,
        ui: &mut egui::Ui,
        idx: usize,
        thumb: CardThumb,
        pixel_size: Option<(u32, u32)>,
        project: &ProjectData,
    ) {
        draw_thumb_box(ui, thumb, false);

        // Page number + file name (data, not a translatable caption).
        let file_name = project.pages[idx]
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        ui.add(
            egui::Label::new(egui::RichText::new(format!("{}. {file_name}", idx + 1)).strong())
                .truncate()
                .selectable(false),
        );
        match pixel_size {
            Some((width, height)) => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(tf!(
                            "page_manager.card.size_label",
                            width = width,
                            height = height
                        ))
                        .weak(),
                    )
                    .selectable(false),
                );
            }
            None => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(t!("page_manager.card.size_unknown")).weak(),
                    )
                    .selectable(false),
                );
            }
        }

        let has_clean = page_has_clean_link(&self.clean_links, idx);
        let bubbles = self.bubble_counts.get(&idx).copied().unwrap_or(0);
        let layers = self.effective_layer_count(idx);
        ui.horizontal_wrapped(|ui| {
            if !has_clean {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(t!("page_manager.card.clean_absent_badge")).weak(),
                    )
                    .selectable(false),
                );
            }
            ui.add(
                egui::Label::new(tf!("page_manager.card.bubbles_badge", count = bubbles))
                    .selectable(false),
            );
            ui.add(
                egui::Label::new(tf!("page_manager.card.layers_badge", count = layers))
                    .selectable(false),
            );
        });
    }

    /// Context menu of a card: open-in navigation plus the toolbar's structural
    /// operations scoped to the current selection.
    fn card_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        page_count: usize,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) {
        for tab in [AppTab::Translation, AppTab::Cleaning, AppTab::Typing] {
            if ui
                .button(tf!("page_manager.context.open_in", tab = tab.title()))
                .clicked()
            {
                actions.push(PageManagerAction::OpenPageIn { tab, page_idx: idx });
            }
        }
        ui.separator();
        // Clean mutations are blocked both by their own worker flag and by any
        // structural op / save in flight (`op_in_progress`), mirroring the reverse
        // gate in the app root (`start_page_op` / `request_save_to_project`).
        let clean_blocked = self.clean_mutation_blocked();
        if ui.add_enabled(!clean_blocked, egui::Button::new(t!("page_manager.clean_replace_button"))).clicked() {
            self.selection = [idx].into_iter().collect();
            self.start_replace_clean_picker();
        }
        // A problem clean can be unlinked too (it is kept as an unassigned file).
        // The in-gap two-step control is the main path; this entry keeps its
        // confirmation dialog because a menu click is a single gesture.
        let has_clean = page_has_clean_link(&self.clean_links, idx);
        if ui
            .add_enabled(!clean_blocked && has_clean, egui::Button::new(t!("page_manager.clean_detach_button")))
            .on_disabled_hover_text(if clean_blocked { t!("page_manager.clean_link.blocked_tooltip") } else { t!("page_manager.context.detach_no_clean_tooltip") })
            .clicked()
        {
            self.start_detach_clean(idx);
        }
        ui.separator();
        if ui
            .add_enabled(
                !op_in_progress,
                egui::Button::new(t!("page_manager.toolbar.insert_pages_button")),
            )
            .clicked()
        {
            self.dialog = Some(PageManagerDialog::insert(!self.selection.is_empty()));
        }
        if ui
            .add_enabled(
                !op_in_progress,
                egui::Button::new(t!("page_manager.toolbar.create_page_button")),
            )
            .clicked()
        {
            self.dialog = Some(PageManagerDialog::create(!self.selection.is_empty()));
        }
        ui.separator();
        let single = self.single_selection();
        if ui
            .add_enabled(
                !op_in_progress && single.is_some_and(|i| i > 0),
                egui::Button::new(t!("page_manager.toolbar.move_up_button")),
            )
            .clicked()
            && let Some(from) = single
        {
            actions.push(PageManagerAction::RequestOp(
                ms_page_ops::PageOpKind::Move {
                    from,
                    to: from.saturating_sub(1),
                },
            ));
        }
        if ui
            .add_enabled(
                !op_in_progress && single.is_some_and(|i| i + 1 < page_count),
                egui::Button::new(t!("page_manager.toolbar.move_down_button")),
            )
            .clicked()
            && let Some(from) = single
        {
            actions.push(PageManagerAction::RequestOp(
                ms_page_ops::PageOpKind::Move { from, to: from + 1 },
            ));
        }
        // Stitching merges the whole selection into one page, so it needs at
        // least two selected pages; the selection is re-validated inside the
        // dialog because `clamp_selection` may shrink it after a reload.
        if ui
            .add_enabled(
                !op_in_progress && self.selection.len() >= 2,
                egui::Button::new(t!("page_manager.context.stitch_pages_button")),
            )
            .on_disabled_hover_text(t!("page_manager.context.stitch_pages_disabled_tooltip"))
            .clicked()
        {
            self.dialog = Some(PageManagerDialog::stitch(
                self.selection.iter().copied().collect(),
            ));
        }
        // Splitting cuts ONE page into several, so it is defined for exactly one
        // selected page; the index is re-validated inside the dialog because
        // `clamp_selection` may drop it after a reload.
        if ui
            .add_enabled(
                !op_in_progress && single.is_some(),
                egui::Button::new(t!("page_manager.context.split_page_button")),
            )
            .on_disabled_hover_text(t!("page_manager.context.split_page_disabled_tooltip"))
            .clicked()
            && let Some(page_idx) = single
        {
            self.dialog = Some(PageManagerDialog::split(page_idx));
        }
        // Cropping rotates and trims ONE page, so it is defined for exactly one
        // selected page; the index is re-validated inside the dialog because
        // `clamp_selection` may drop it after a reload.
        if ui
            .add_enabled(
                !op_in_progress && single.is_some(),
                egui::Button::new(t!("page_manager.context.crop_page_button")),
            )
            .on_disabled_hover_text(t!("page_manager.context.crop_page_disabled_tooltip"))
            .clicked()
            && let Some(page_idx) = single
        {
            self.dialog = Some(PageManagerDialog::crop(page_idx));
        }
        if ui
            .add_enabled(
                !op_in_progress && !self.selection.is_empty(),
                egui::Button::new(t!("page_manager.toolbar.delete_button")),
            )
            .clicked()
        {
            self.dialog = Some(PageManagerDialog::delete(
                self.selection.iter().copied().collect(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[usize]) -> BTreeSet<usize> {
        values.iter().copied().collect()
    }

    #[test]
    fn plain_click_selects_single() {
        let (next, anchor) = selection_after_click(&set(&[1, 2]), Some(1), 4, false, false);
        assert_eq!(next, set(&[4]));
        assert_eq!(anchor, Some(4));
    }

    #[test]
    fn ctrl_click_toggles_membership() {
        let (next, anchor) = selection_after_click(&set(&[1]), Some(1), 3, true, false);
        assert_eq!(next, set(&[1, 3]));
        assert_eq!(anchor, Some(3));
        let (next2, _) = selection_after_click(&next, anchor, 3, true, false);
        assert_eq!(next2, set(&[1]));
    }

    #[test]
    fn shift_click_selects_range_and_keeps_anchor() {
        let (next, anchor) = selection_after_click(&set(&[2]), Some(2), 5, false, true);
        assert_eq!(next, set(&[2, 3, 4, 5]));
        assert_eq!(anchor, Some(2));
        // Reverse direction from the same anchor.
        let (next2, anchor2) = selection_after_click(&next, anchor, 0, false, true);
        assert_eq!(next2, set(&[0, 1, 2]));
        assert_eq!(anchor2, Some(2));
    }

    #[test]
    fn row_reserves_the_clean_slot_iff_any_of_its_pages_has_a_link() {
        use super::super::clean_link::CleanLinkProblem;
        let problem = PageCleanLink::Problem { problem: CleanLinkProblem::CleanUnreadable("e".to_string()), file: std::path::PathBuf::from("002.png"), size: None };
        let links = [PageCleanLink::None, problem, PageCleanLink::None, PageCleanLink::None];
        assert!(!page_has_clean_link(&links, 0));
        assert!(page_has_clean_link(&links, 1));
        assert!(!page_has_clean_link(&links, 99));
        let metrics = GridMetrics { card: [CARD_WIDTH, CARD_HEIGHT], clean_card_h: CLEAN_CARD_HEIGHT, link_gap: LINK_GAP, spacing: [8.0, 3.0], section_header_h: SECTION_HEADER_HEIGHT };
        // Two columns: pages 0-1 share a row with a link, pages 2-3 do not.
        let layout = GridLayout::build(&metrics, 2.0 * CARD_WIDTH + 8.0, links.len(), |idx| page_has_clean_link(&links, idx), 0);
        assert_eq!(layout.rows[0].kind, GridRowKind::Pages { first: 0, count: 2, with_clean: true });
        assert_eq!(layout.rows[1].kind, GridRowKind::Pages { first: 2, count: 2, with_clean: false });
        // The page without a clean in the linked row still gets a (drawn-empty) slot.
        assert!(layout.clean_card_rect(0, 0).is_some());
    }

    #[test]
    fn shift_click_without_anchor_acts_like_plain_click() {
        let (next, anchor) = selection_after_click(&set(&[]), None, 3, false, true);
        assert_eq!(next, set(&[3]));
        assert_eq!(anchor, Some(3));
    }
}
