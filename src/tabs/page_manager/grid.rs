/*
File: tabs/page_manager/grid.rs

Purpose:
Card grid of the page-manager tab: virtualized rows of page cards (thumbnail,
page number, file name, pixel size, badges), plus selection handling, clean
content actions, and the per-card context menu.

Key structures:
- CardThumb: per-frame snapshot of a card's thumbnail visual state.

Key functions:
- PageManagerTabState::draw_grid(): the ScrollArea + rows + cards.
- selection_after_click(): pure click/Ctrl/Shift selection logic (unit-tested).

Notes:
Rows are virtualized through `ScrollArea::show_rows` with a uniform card height,
so thumbnails are only requested for visible cards; the LRU thumbnail cache in
`thumbs.rs` bounds decoded memory.
*/

use std::collections::BTreeSet;

use eframe::egui;

use crate::app::PageImageInfo;
use crate::project::ProjectData;
use crate::tabs::AppTab;

use super::dialogs::PageManagerDialog;
use super::thumbs::{THUMB_LONG_SIDE_PX, ThumbVisual};
use super::{PageManagerAction, PageManagerTabState};

/// Fixed card footprint; `show_rows` virtualization requires a uniform height.
const CARD_WIDTH: f32 = 212.0;
const CARD_HEIGHT: f32 = 276.0;
/// Padding between the card border and its content.
const CARD_INNER_MARGIN: f32 = 8.0;

/// Per-frame snapshot of a card's thumbnail state, copied out of the cache so
/// the cache borrow does not overlap the child-Ui borrows below.
enum CardThumb {
    Ready(egui::TextureId, egui::Vec2),
    Failed,
    Pending,
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

impl PageManagerTabState {
    /// Draws the scrollable page-card grid and handles selection, double-click
    /// navigation, and the per-card context menu.
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

        let spacing_x = ui.spacing().item_spacing.x;
        let columns = usize::max(
            1,
            ((ui.available_width() + spacing_x) / (CARD_WIDTH + spacing_x)).floor() as usize,
        );
        let rows = page_count.div_ceil(columns);

        egui::ScrollArea::vertical()
            .id_salt("page_manager_grid")
            .auto_shrink([false, false])
            .show_rows(ui, CARD_HEIGHT, rows, |ui, row_range| {
                for row in row_range {
                    ui.horizontal(|ui| {
                        let start = row * columns;
                        let end = usize::min(start + columns, page_count);
                        for idx in start..end {
                            self.draw_card(ui, project, page_infos, op_in_progress, idx, actions);
                        }
                    });
                }
            });
    }

    /// Draws one page card and processes its interactions.
    fn draw_card(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_infos: &std::collections::HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
        idx: usize,
        actions: &mut Vec<PageManagerAction>,
    ) {
        let path = &project.pages[idx].path;
        self.thumbs.request_thumb_if_needed(path, self.generation);

        // Copy the thumbnail state out of the cache before any child Ui borrows.
        let (thumb, cached_full_size) = match self.thumbs.cache.touch_and_get(path) {
            Some(entry) => (
                match &entry.visual {
                    ThumbVisual::Ready(texture) => {
                        CardThumb::Ready(texture.id(), texture.size_vec2())
                    }
                    ThumbVisual::Failed => CardThumb::Failed,
                },
                entry.full_size,
            ),
            None => (CardThumb::Pending, None),
        };
        // Page pixel size: authoritative geometry first, thumbnail probe second.
        let pixel_size = page_infos
            .get(&idx)
            .map(|info| (info.width_px, info.height_px))
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or(cached_full_size);

        let selected = self.selection.contains(&idx);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(CARD_WIDTH, CARD_HEIGHT),
            egui::Sense::click(),
        );
        if ui.is_rect_visible(rect) {
            let visuals = ui.visuals();
            let (fill, stroke) = if selected {
                (
                    visuals.selection.bg_fill.linear_multiply(0.35),
                    visuals.selection.stroke,
                )
            } else if response.hovered() {
                (
                    visuals.widgets.hovered.weak_bg_fill,
                    visuals.widgets.hovered.bg_stroke,
                )
            } else {
                (
                    visuals.widgets.noninteractive.weak_bg_fill,
                    visuals.widgets.noninteractive.bg_stroke,
                )
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(6), fill);
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(6),
                stroke,
                egui::StrokeKind::Inside,
            );

            let inner = rect.shrink(CARD_INNER_MARGIN);
            let mut content = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(inner)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            self.draw_card_content(&mut content, idx, &thumb, pixel_size, project);
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
        if response.double_clicked() {
            actions.push(PageManagerAction::OpenPageIn {
                tab: AppTab::Translation,
                page_idx: idx,
            });
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
    /// pixel size, and the clean/bubbles/layers badges.
    fn draw_card_content(
        &self,
        ui: &mut egui::Ui,
        idx: usize,
        thumb: &CardThumb,
        pixel_size: Option<(u32, u32)>,
        project: &ProjectData,
    ) {
        let thumb_box = egui::vec2(ui.available_width(), THUMB_LONG_SIDE_PX as f32);
        ui.allocate_ui_with_layout(
            thumb_box,
            egui::Layout::top_down_justified(egui::Align::Center),
            |ui| {
                ui.set_min_size(thumb_box);
                ui.centered_and_justified(|ui| match thumb {
                    CardThumb::Ready(texture_id, size) => {
                        ui.add(egui::Image::new((*texture_id, *size)));
                    }
                    CardThumb::Failed => {
                        ui.label(t!("page_manager.card.thumb_error"));
                    }
                    CardThumb::Pending => {
                        ui.label(t!("page_manager.card.thumb_loading"));
                    }
                });
            },
        );

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

        let clean = self.clean_present.get(idx).copied().unwrap_or(false);
        let clean_size_mismatch = self.orphan_cleans.iter().any(|orphan| {
            matches!(orphan.reason, crate::models::clean_assign::OrphanReason::SizeMismatch { page_idx, .. } if page_idx == idx)
        });
        let bubbles = self.bubble_counts.get(&idx).copied().unwrap_or(0);
        let layers = self.effective_layer_count(idx);
        ui.horizontal_wrapped(|ui| {
            if clean {
                ui.colored_label(
                    egui::Color32::from_rgb(110, 200, 110),
                    t!("page_manager.card.clean_present_badge"),
                );
            } else {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(t!("page_manager.card.clean_absent_badge")).weak(),
                    )
                    .selectable(false),
                );
            }
            if clean_size_mismatch {
                ui.colored_label(ui.visuals().warn_fg_color, t!("page_manager.card.clean_size_mismatch_badge"));
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
        let clean_blocked = self.clean_op_in_flight || op_in_progress;
        let clean_present = self.clean_present.get(idx).copied().unwrap_or(false);
        if ui.add_enabled(!clean_blocked, egui::Button::new(t!("page_manager.clean_replace_button"))).clicked() {
            self.selection = [idx].into_iter().collect();
            self.start_replace_clean_picker(op_in_progress);
        }
        if ui.add_enabled(!clean_blocked && clean_present, egui::Button::new(t!("page_manager.clean_detach_button"))).clicked() {
            self.start_detach_clean(idx, op_in_progress);
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
                crate::page_ops::PageOpKind::Move {
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
                crate::page_ops::PageOpKind::Move { from, to: from + 1 },
            ));
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
    fn shift_click_without_anchor_acts_like_plain_click() {
        let (next, anchor) = selection_after_click(&set(&[]), None, 3, false, true);
        assert_eq!(next, set(&[3]));
        assert_eq!(anchor, Some(3));
    }
}
