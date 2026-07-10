/*
File: src/canvas/bubble_on_top_ui.rs

Purpose:
On-top bubble UI subsystem for `CanvasView`: scene-space widgets, move/resize handles,
hit-rect tracking, and drag interactions for bubbles rendered over the page image.

Main responsibilities:
- render on-top bubble widgets in scene-space with clip-aware child UIs;
- manage move-handle drag, rect-handle resize, and hit-test geometry;
- keep on-top hit rectangles updated for page interaction routing;
- isolate on-top-specific UI code from the main canvas facade.

Key functions:
- draw_on_top_for_page()
- on_top_hit_test()
- draw_rect_handles()
- drag_on_top_by_pointer()

Notes:
- Business logic and persistence remain in `bubble_runtime.rs`.
- This module only drives existing `CanvasView` geometry/runtime helpers.
*/

use super::helpers::{
    bubble_text_font_id, measure_text_widget_content_height, with_bubble_text_font,
};
use super::types::{BubbleTextField, BubbleType, RectCoords};
use super::{
    CanvasHooks, CanvasView, ON_TOP_FOCUS_GAP_PX, ON_TOP_FOOTER_RESERVED_HEIGHT_PX, OnTopFocusMode,
};
use crate::bubble_status::paint_bubble_status_border;
use crate::project::{ProjectData, Side};
use crate::runtime_log;
use crate::widgets::{SpellcheckedTextEdit, misspelled_word_at_pointer};
use eframe::egui;
use egui::{Align, Align2, Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke};

const READONLY_ON_TOP_HEADER_HEIGHT_PX: f32 = 24.0;
const READONLY_ON_TOP_HEADER_GAP_PX: f32 = 4.0;
fn paint_on_top_original_fallback(ui: &egui::Ui, rect: Rect, text: &str) {
    let inner_rect = rect.shrink2(egui::vec2(6.0, 4.0));
    if !inner_rect.is_positive() {
        return;
    }

    let font_id = bubble_text_font_id(ui);
    let text_color = ui.visuals().weak_text_color();
    let layout_text = if text.is_empty() { " " } else { text };
    let galley = ui.fonts_mut(|fonts| {
        fonts.layout_job(egui::text::LayoutJob::simple(
            layout_text.to_owned(),
            font_id.clone(),
            text_color,
            inner_rect.width().max(1.0),
        ))
    });
    let text_pos = egui::pos2(
        inner_rect.center().x - galley.size().x * 0.5,
        inner_rect.top(),
    );
    ui.painter()
        .with_clip_rect(inner_rect.intersect(ui.clip_rect()))
        .galley(text_pos, galley, text_color);
}

pub(super) fn draw_on_top_for_page(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    project: &ProjectData,
    image_rect: Rect,
    ids: Vec<i64>,
    hooks: &mut dyn CanvasHooks,
) {
    let scene_clip_rect = ui.clip_rect();
    for bid in ids {
        let Some(snapshot) = canvas.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            continue;
        };
        let hook_bubble_fallback;
        let hook_bubble = match project.bubbles.iter().find(|bubble| bubble.id == bid) {
            Some(project_bubble) => project_bubble,
            None => {
                hook_bubble_fallback = canvas.hook_bubble_for_runtime(project, &snapshot);
                &hook_bubble_fallback
            }
        };

        let rect =
            CanvasView::rect_from_coords(image_rect, snapshot.rect_coords).intersect(image_rect);
        if !rect.is_positive() {
            continue;
        }
        if hooks.should_hide_on_top_bubble(snapshot.img_idx, hook_bubble, rect) {
            continue;
        }

        let id = Id::new(("on_top_bubble", bid));
        let response = ui.interact(
            rect,
            id,
            if canvas.scene.zoom_drag_active {
                Sense::hover()
            } else {
                Sense::click_and_drag()
            },
        );
        let mut interacted_with_bubble = false;
        let pressed_primary_on_bubble = canvas.editable
            && response.is_pointer_button_down_on()
            && ui.ctx().input(|i| i.pointer.primary_down());
        if pressed_primary_on_bubble {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
        }
        if canvas.editable && response.clicked() {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
        }
        let mut selected_now = canvas.bubble_runtime.selected_bubble == Some(bid);
        let suppress_on_top_focus_ui = canvas.editable
            && canvas.state.on_top_focus_mode == OnTopFocusMode::Aside
            && interacted_with_bubble;
        if canvas.editable
            && !canvas.scene.zoom_drag_active
            && selected_now
            && canvas
                .bubble_runtime
                .on_top_drag_state
                .is_none_or(|drag_state| drag_state.bid != bid)
            && response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let uv = CanvasView::uv_from_scene(image_rect, pos);
            canvas.move_bubble_anchor(bid, uv.x, uv.y, true);
            interacted_with_bubble = true;
        }

        let mut new_text = snapshot.text.clone();
        let mut new_original = snapshot.original_text.clone();
        let mut want_paste_original = false;
        let mut want_paste_translation = false;
        let mut want_copy_whole_bubble = false;
        let mut want_duplicate_bubble = false;
        let mut want_paste_whole_bubble = false;
        let mut want_translate = false;
        let mut want_delete = false;
        let mut want_switch_bubble_type = None;
        let mut bubble_has_focus = selected_now && !suppress_on_top_focus_ui;
        let mut text_changed = false;
        let mut hit_rect = rect;
        let has_header = hooks.has_bubble_header(hook_bubble, canvas.editable);
        let status_stroke = if canvas.editable && canvas.state.show_bubble_status {
            hooks.bubble_status_style(hook_bubble, canvas.editable, canvas)
        } else {
            None
        };
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(4),
            Color32::from_rgb(35, 35, 42).gamma_multiply(canvas.state.bubble_opacity),
        );
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(4),
            Stroke::new(1.0, Color32::from_gray(90)),
            egui::StrokeKind::Inside,
        );
        if let Some(style) = status_stroke {
            paint_bubble_status_border(ui.painter(), rect, CornerRadius::same(4), style);
        }
        let content_rect = rect.shrink(3.0);
        if content_rect.is_positive() {
            let mut header_rect = Rect::NOTHING;
            let mut text_rect = content_rect;
            if !canvas.editable && has_header {
                let header_height = READONLY_ON_TOP_HEADER_HEIGHT_PX.min(content_rect.height());
                header_rect = Rect::from_min_size(
                    content_rect.min,
                    egui::vec2(content_rect.width(), header_height),
                );
                let next_top = (header_rect.bottom() + READONLY_ON_TOP_HEADER_GAP_PX)
                    .min(content_rect.bottom());
                text_rect =
                    Rect::from_min_max(egui::pos2(content_rect.left(), next_top), content_rect.max);
                let mut header_ui = CanvasView::new_scene_overlay_child(
                    ui,
                    header_rect,
                    scene_clip_rect,
                    egui::Layout::left_to_right(Align::Center),
                );
                header_ui.set_width(header_rect.width());
                header_ui.set_min_width(header_rect.width());
                header_ui.set_max_width(header_rect.width());
                header_ui.set_min_height(header_rect.height());
                header_ui.horizontal_centered(|ui| {
                    hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
                });
            }
            let field_size = egui::vec2(text_rect.width().max(8.0), text_rect.height().max(8.0));
            let mut content_ui = CanvasView::new_scene_overlay_child(
                ui,
                text_rect,
                scene_clip_rect,
                egui::Layout::top_down(Align::LEFT),
            );
            content_ui.set_width(field_size.x);
            content_ui.set_min_width(field_size.x);
            content_ui.set_max_width(field_size.x);
            content_ui.set_min_height(field_size.y);
            if canvas.editable {
                let text_edit_id = content_ui.make_persistent_id(("on_top_text", bid));
                let show_original_fallback = new_text.trim().is_empty()
                    && !new_original.trim().is_empty()
                    && !content_ui.ctx().memory(|mem| mem.has_focus(text_edit_id));
                egui::ScrollArea::vertical()
                    .id_salt(("on_top_text_scroll", bid))
                    .auto_shrink([false, false])
                    .show(&mut content_ui, |ui| {
                        let translation_spellcheck_enabled = !canvas.bubble_spellcheck_disabled(
                            project,
                            bid,
                            BubbleTextField::Translation,
                        );
                        let text_resp = with_bubble_text_font(ui, |ui| {
                            SpellcheckedTextEdit::multiline(&mut new_text)
                                .id(text_edit_id)
                                .hint_text(if show_original_fallback {
                                    ""
                                } else {
                                    t!("canvas.bubble.translation_label")
                                })
                                .desired_width(field_size.x)
                                .desired_rows(1)
                                .spellcheck_enabled(
                                    canvas.state.spellcheck_translation
                                        && translation_spellcheck_enabled,
                                )
                                .horizontal_align(Align::Center)
                                .vertical_align(Align::TOP)
                                .show(ui)
                        });
                        let text_misspelled_word =
                            misspelled_word_at_pointer(ui, &text_resp, &new_text);
                        canvas.note_focused_bubble_text_input(
                            ui.ctx(),
                            bid,
                            BubbleTextField::Translation,
                            &text_resp.response,
                        );
                        if text_resp.response.clicked() || text_resp.response.changed() {
                            interacted_with_bubble = true;
                        }
                        if text_resp.response.changed() {
                            text_changed = true;
                            canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                        }
                        if text_resp.response.has_focus() {
                            canvas.bubble_runtime.selected_bubble = Some(bid);
                            interacted_with_bubble = true;
                        }
                        bubble_has_focus = bubble_has_focus || text_resp.response.has_focus();
                        if text_resp.response.lost_focus() {
                            canvas.commit_text_upsert_now(bid);
                        }
                        if show_original_fallback
                            && !text_resp.response.has_focus()
                            && !text_resp.response.changed()
                        {
                            paint_on_top_original_fallback(
                                ui,
                                text_resp.response.rect,
                                &new_original,
                            );
                        }
                        if text_resp.response.secondary_clicked() {
                            canvas.bubble_runtime.bubble_context_menu_misspelled_word =
                                text_misspelled_word.clone();
                        }
                        text_resp.response.context_menu(|ui| {
                            canvas.show_bubble_context_menu(
                                ui,
                                project,
                                bid,
                                snapshot.bubble_type,
                                &new_original,
                                &new_text,
                                text_misspelled_word.as_deref(),
                                &mut want_copy_whole_bubble,
                                &mut want_duplicate_bubble,
                                &mut want_paste_whole_bubble,
                                &mut want_paste_original,
                                &mut want_paste_translation,
                                &mut want_switch_bubble_type,
                                &mut interacted_with_bubble,
                            );
                        });
                    });
            } else {
                egui::ScrollArea::vertical()
                    .id_salt(("on_top_text_scroll_ro", bid))
                    .auto_shrink([false, false])
                    .show(&mut content_ui, |ui| {
                        ui.allocate_ui_with_layout(
                            field_size,
                            egui::Layout::top_down(Align::Center),
                            |ui| {
                                ui.centered_and_justified(|ui| {
                                    with_bubble_text_font(ui, |ui| {
                                        ui.label(snapshot.display_text());
                                    });
                                });
                            },
                        );
                    });
            }
            hit_rect = hit_rect.union(text_rect.intersect(scene_clip_rect));
            if header_rect.is_positive() {
                hit_rect = hit_rect.union(header_rect.intersect(scene_clip_rect));
            }
        }
        if interacted_with_bubble {
            canvas.bubble_runtime.selected_bubble = Some(bid);
        }
        selected_now = canvas.bubble_runtime.selected_bubble == Some(bid);
        let selected_for_on_top_ui = selected_now && !suppress_on_top_focus_ui;
        if selected_for_on_top_ui {
            bubble_has_focus = true;
        }

        if canvas.editable && selected_for_on_top_ui {
            let original_field_width = rect.width().max(8.0);
            let original_widget_h =
                measure_text_widget_content_height(ui, &new_original, original_field_width)
                    .max(28.0);
            let original_bottom = rect.top() - 10.0;
            let original_rect = Rect::from_min_max(
                egui::pos2(rect.left(), original_bottom - original_widget_h),
                egui::pos2(rect.left() + original_field_width, original_bottom),
            );

            let header_rect = Rect::from_min_size(
                egui::pos2(original_rect.left(), original_rect.top() - 24.0),
                egui::vec2(original_field_width, 24.0),
            );
            let mut header_ui = CanvasView::new_scene_overlay_child(
                ui,
                header_rect,
                scene_clip_rect,
                egui::Layout::left_to_right(Align::Center),
            );
            header_ui.set_width(header_rect.width());
            header_ui.set_min_width(header_rect.width());
            header_ui.set_max_width(header_rect.width());
            header_ui.set_min_height(header_rect.height());
            header_ui.horizontal_centered(|ui| {
                hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
            });
            if header_rect.intersect(scene_clip_rect).is_positive() {
                hit_rect = hit_rect.union(header_rect.intersect(scene_clip_rect));
            }

            let mut original_ui = CanvasView::new_scene_overlay_child(
                ui,
                original_rect,
                scene_clip_rect,
                egui::Layout::bottom_up(Align::LEFT),
            );
            original_ui.set_width(original_field_width);
            original_ui.set_min_width(original_field_width);
            original_ui.set_max_width(original_field_width);
            original_ui.set_min_height(original_widget_h);
            let original_spellcheck_enabled =
                !canvas.bubble_spellcheck_disabled(project, bid, BubbleTextField::Original);
            let orig_resp = with_bubble_text_font(&mut original_ui, |ui| {
                SpellcheckedTextEdit::multiline(&mut new_original)
                    .id_salt(("on_top_original", bid))
                    .hint_text(t!("canvas.bubble.original_label"))
                    .desired_width(original_field_width)
                    .desired_rows(1)
                    .spellcheck_enabled(
                        canvas.state.spellcheck_original && original_spellcheck_enabled,
                    )
                    .horizontal_align(Align::Center)
                    .vertical_align(Align::TOP)
                    .show(ui)
            });
            let orig_misspelled_word =
                misspelled_word_at_pointer(&original_ui, &orig_resp, &new_original);
            canvas.note_focused_bubble_text_input(
                original_ui.ctx(),
                bid,
                BubbleTextField::Original,
                &orig_resp.response,
            );
            if orig_resp.response.clicked() || orig_resp.response.changed() {
                interacted_with_bubble = true;
            }
            if orig_resp.response.changed() {
                text_changed = true;
                canvas.schedule_text_upsert(bid, original_ui.ctx().input(|i| i.time));
            }
            if orig_resp.response.has_focus() {
                canvas.bubble_runtime.selected_bubble = Some(bid);
                interacted_with_bubble = true;
            }
            bubble_has_focus = bubble_has_focus || orig_resp.response.has_focus();
            if orig_resp.response.lost_focus() {
                canvas.commit_text_upsert_now(bid);
            }
            if orig_resp.response.secondary_clicked() {
                canvas.bubble_runtime.bubble_context_menu_misspelled_word =
                    orig_misspelled_word.clone();
            }
            orig_resp.response.context_menu(|ui| {
                canvas.show_bubble_context_menu(
                    ui,
                    project,
                    bid,
                    snapshot.bubble_type,
                    &new_original,
                    &new_text,
                    orig_misspelled_word.as_deref(),
                    &mut want_copy_whole_bubble,
                    &mut want_duplicate_bubble,
                    &mut want_paste_whole_bubble,
                    &mut want_paste_original,
                    &mut want_paste_translation,
                    &mut want_switch_bubble_type,
                    &mut interacted_with_bubble,
                );
            });
            if original_rect.intersect(scene_clip_rect).is_positive() {
                hit_rect = hit_rect.union(original_rect.intersect(scene_clip_rect));
            }

            let footer_zone = Rect::from_min_size(
                egui::pos2(rect.left(), rect.bottom() + ON_TOP_FOCUS_GAP_PX),
                egui::vec2(original_field_width, ON_TOP_FOOTER_RESERVED_HEIGHT_PX),
            );
            let mut footer_ui = CanvasView::new_scene_overlay_child(
                ui,
                footer_zone,
                scene_clip_rect,
                egui::Layout::top_down(Align::LEFT),
            );
            footer_ui.set_width(footer_zone.width());
            footer_ui.set_min_width(footer_zone.width());
            footer_ui.set_max_width(footer_zone.width());
            footer_ui.set_min_height(footer_zone.height());
            footer_ui.horizontal_wrapped(|ui| {
                if ui.small_button(t!("canvas.bubble.translate_button")).clicked() {
                    want_translate = true;
                    interacted_with_bubble = true;
                }
                if ui.small_button(t!("canvas.bubble.delete_button")).clicked() {
                    want_delete = true;
                    interacted_with_bubble = true;
                }
            });

            hooks.build_bubble_footer(&mut footer_ui, project, hook_bubble, canvas.editable);
            if footer_zone.intersect(scene_clip_rect).is_positive() {
                hit_rect = hit_rect.union(footer_zone.intersect(scene_clip_rect));
            }
        }

        if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            if bubble.text != new_text {
                bubble.text = new_text.clone();
                text_changed = true;
            }
            if bubble.original_text != new_original {
                bubble.original_text = new_original.clone();
                text_changed = true;
            }
            if text_changed {
                bubble.mounted = true;
            }
        }
        response.context_menu(|ui| {
            canvas.show_bubble_context_menu(
                ui,
                project,
                bid,
                snapshot.bubble_type,
                &new_original,
                &new_text,
                None,
                &mut want_copy_whole_bubble,
                &mut want_duplicate_bubble,
                &mut want_paste_whole_bubble,
                &mut want_paste_original,
                &mut want_paste_translation,
                &mut want_switch_bubble_type,
                &mut interacted_with_bubble,
            );
        });
        if response.secondary_clicked() {
            canvas.bubble_runtime.bubble_context_menu_misspelled_word = None;
        }

        let show_rect = canvas.editable
            && selected_for_on_top_ui
            && (bubble_has_focus
                || canvas
                    .bubble_runtime
                    .active_rect_handle
                    .is_some_and(|(active_bid, _)| active_bid == bid));

        if show_rect {
            ui.painter().rect_stroke(
                rect,
                0.0,
                Stroke::new(3.0, Color32::from_rgb(0, 120, 215)),
                egui::StrokeKind::Inside,
            );
        }

        if show_rect && !canvas.scene.zoom_drag_active {
            let handle_rect = on_top_move_handle_rect(rect);
            let handle_id = Id::new(("on_top_move_handle", bid));
            let handle_response = ui.interact(handle_rect, handle_id, Sense::click_and_drag());
            let handle_active = canvas
                .bubble_runtime
                .on_top_drag_state
                .is_some_and(|drag_state| drag_state.bid == bid);
            if handle_response.hovered() || handle_active {
                ui.ctx().set_cursor_icon(if handle_active {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
            let handle_fill = if handle_active {
                Color32::from_rgb(0, 120, 215)
            } else if handle_response.hovered() {
                Color32::from_rgb(38, 153, 251)
            } else {
                Color32::from_rgba_premultiplied(0, 120, 215, 220)
            };
            ui.painter().circle_filled(
                handle_rect.center(),
                super::ON_TOP_MOVE_HANDLE_RADIUS_PX,
                handle_fill,
            );
            ui.painter().circle_stroke(
                handle_rect.center(),
                super::ON_TOP_MOVE_HANDLE_RADIUS_PX,
                Stroke::new(1.5, Color32::WHITE),
            );
            ui.painter().text(
                handle_rect.center(),
                Align2::CENTER_CENTER,
                "✋",
                egui::FontId::proportional(14.0),
                Color32::WHITE,
            );
            if handle_response.is_pointer_button_down_on() || handle_response.clicked() {
                canvas.bubble_runtime.selected_bubble = Some(bid);
                interacted_with_bubble = true;
            }
            if handle_response.drag_started()
                && let Some(pos) = handle_response.interact_pointer_pos()
            {
                start_on_top_drag(canvas, bid, pos);
                canvas.bubble_runtime.selected_bubble = Some(bid);
                interacted_with_bubble = true;
            }
            if handle_response.dragged()
                && let Some(pos) = handle_response.interact_pointer_pos()
            {
                drag_on_top_by_pointer(canvas, bid, image_rect, pos);
                canvas.bubble_runtime.selected_bubble = Some(bid);
                interacted_with_bubble = true;
            }
            if handle_active && handle_response.drag_stopped() {
                finish_on_top_drag(canvas, bid);
                interacted_with_bubble = true;
            }
            if handle_rect.intersect(scene_clip_rect).is_positive() {
                hit_rect = hit_rect.union(handle_rect.intersect(scene_clip_rect));
            }
            let coords = canvas
                .bubble_runtime
                .runtime_bubbles
                .get(&bid)
                .map(|bubble| bubble.rect_coords)
                .unwrap_or(snapshot.rect_coords);
            draw_rect_handles(canvas, ui, bid, image_rect, coords);
        }

        if want_paste_original
            || want_paste_translation
            || want_copy_whole_bubble
            || want_duplicate_bubble
            || want_paste_whole_bubble
            || want_translate
            || want_delete
            || want_switch_bubble_type.is_some()
            || interacted_with_bubble
        {
            canvas.bubble_runtime.selected_bubble = Some(bid);
        }
        selected_now = canvas.bubble_runtime.selected_bubble == Some(bid);
        if selected_for_on_top_ui && selected_now {
            bubble_has_focus = true;
        }
        if bubble_has_focus {
            canvas.bubble_runtime.focused_bubbles.insert(bid);
        }
        if want_paste_original {
            canvas.request_paste_from_clipboard(ui.ctx(), bid, BubbleTextField::Original);
        }
        if want_paste_translation {
            canvas.request_paste_from_clipboard(ui.ctx(), bid, BubbleTextField::Translation);
        }
        if want_copy_whole_bubble && !canvas.copy_whole_bubble_to_internal_buffer(project, bid) {
            runtime_log::log_warn(format!(
                "[canvas::bubble_on_top_ui] failed to copy bubble payload; bubble_id={bid}"
            ));
        }
        if want_duplicate_bubble
            && !canvas.duplicate_bubble_below(project, bid, ui.ctx().input(|i| i.time))
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_on_top_ui] failed to duplicate bubble; bubble_id={bid}"
            ));
        }
        if want_paste_whole_bubble
            && !canvas.paste_copied_whole_bubble_into_bid(project, bid, ui.ctx().input(|i| i.time))
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_on_top_ui] failed to paste copied bubble payload; bubble_id={bid}"
            ));
        }
        if let Some(next_type) = want_switch_bubble_type
            && !canvas.set_bubble_type_for_bid(bid, next_type)
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_on_top_ui] failed to switch bubble type; bubble_id={bid}; next_type={}",
                next_type.as_str()
            ));
        }
        if want_translate {
            canvas.bubble_runtime.pending_translate.insert(bid);
        }
        if want_delete {
            canvas.bubble_runtime.pending_delete.insert(bid);
        }
        canvas.scene.on_top_hit_rects.insert(bid, hit_rect);
    }
}

pub(super) fn on_top_hit_test(
    canvas: &CanvasView,
    page_idx: usize,
    image_rect: Rect,
    scene_pos: Pos2,
) -> bool {
    focus_candidate_at_scene_pos(canvas, page_idx, image_rect, scene_pos).is_some()
}

pub(super) fn focus_candidate_at_scene_pos(
    canvas: &CanvasView,
    page_idx: usize,
    image_rect: Rect,
    scene_pos: Pos2,
) -> Option<i64> {
    // On-top bubbles are always text bubbles, so each item is a plain bubble id. Bucket the page
    // once and read the two on-top columns from it instead of scanning the runtime bubbles twice.
    let buckets = canvas.page_bubbles_bucketed(page_idx);
    let ids: Vec<i64> = buckets
        .bucket(Side::Left, BubbleType::OnTop)
        .iter()
        .chain(buckets.bucket(Side::Right, BubbleType::OnTop))
        .map(|item| item.bid)
        .collect();
    for bid in ids.into_iter().rev() {
        let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get(&bid) else {
            continue;
        };
        if let Some(hit_rect) = canvas.scene.on_top_hit_rects.get(&bid) {
            if hit_rect.contains(scene_pos) {
                return Some(bid);
            }
        } else {
            let rect =
                CanvasView::rect_from_coords(image_rect, bubble.rect_coords).intersect(image_rect);
            if rect.is_positive() && rect.contains(scene_pos) {
                return Some(bid);
            }
        }
    }
    None
}

pub(super) fn draw_rect_handles(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    bid: i64,
    image_rect: Rect,
    coords: RectCoords,
) {
    let rect = CanvasView::rect_from_coords(image_rect, coords).intersect(image_rect);
    if !rect.is_positive() {
        return;
    }

    let points = rect_handle_points(rect);
    for (idx, point) in points.iter().enumerate() {
        let handle_rect = Rect::from_center_size(*point, egui::vec2(10.0, 10.0));
        let id = Id::new(("rect_handle", bid, idx));
        let response = ui.interact(handle_rect, id, Sense::click_and_drag());

        ui.painter().circle_filled(*point, 4.0, Color32::WHITE);
        ui.painter().circle_stroke(
            *point,
            4.0,
            Stroke::new(1.0, Color32::from_rgb(0, 120, 215)),
        );

        if response.dragged() {
            canvas.bubble_runtime.active_rect_handle = Some((bid, idx));
            if let Some(pos) = response.interact_pointer_pos() {
                // `resize_rect_by_handle` re-queues `bid` in `pending_upsert` every dragged frame
                // (see its final line), so the last frame before release leaves it queued.
                resize_rect_by_handle(canvas, bid, idx, image_rect, pos);
            }
        }
        if response.drag_stopped() {
            // Clearing the active handle re-enables the debounced flush; the id is already in
            // `pending_upsert` from the final `resize_rect_by_handle` call above, so the next flush
            // commits the final rect. Unlike the area-handle `drag_stopped` path (which inserts
            // `pending_upsert` here because its resize fn does not), no insert is needed here.
            canvas.bubble_runtime.active_rect_handle = None;
        }
    }
}

pub(super) fn is_scene_pos_on_rect_handle(rect: Rect, scene_pos: Pos2) -> bool {
    rect_handle_points(rect)
        .iter()
        .any(|point| Rect::from_center_size(*point, egui::vec2(10.0, 10.0)).contains(scene_pos))
}

pub(super) fn start_on_top_drag(canvas: &mut CanvasView, bid: i64, pointer_pos: Pos2) {
    canvas.bubble_runtime.on_top_drag_state = Some(super::types::OnTopDragState {
        bid,
        last_pointer_pos: pointer_pos,
        moved: false,
    });
}

pub(super) fn drag_on_top_by_pointer(
    canvas: &mut CanvasView,
    bid: i64,
    image_rect: Rect,
    pointer_pos: Pos2,
) {
    let Some(mut state) = canvas.bubble_runtime.on_top_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    let dx = pointer_pos.x - state.last_pointer_pos.x;
    let dy = pointer_pos.y - state.last_pointer_pos.y;
    state.last_pointer_pos = pointer_pos;
    canvas.bubble_runtime.on_top_drag_state = Some(state);

    if dx.abs() <= f32::EPSILON && dy.abs() <= f32::EPSILON {
        return;
    }
    let du = dx / image_rect.width().max(1.0);
    let dv = dy / image_rect.height().max(1.0);
    move_bubble_by_delta(canvas, bid, du, dv);
    if let Some(state) = canvas.bubble_runtime.on_top_drag_state.as_mut()
        && state.bid == bid
    {
        state.moved = true;
        state.last_pointer_pos = pointer_pos;
    }
}

pub(super) fn finish_on_top_drag(canvas: &mut CanvasView, bid: i64) {
    let Some(state) = canvas.bubble_runtime.on_top_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    canvas.bubble_runtime.on_top_drag_state = None;
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    if state.moved {
        bubble.side = if bubble.img_u < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        canvas.bubble_runtime.pending_upsert.insert(bid);
    }
}

fn rect_handle_points(rect: Rect) -> [Pos2; 8] {
    let center = rect.center();
    [
        egui::pos2(rect.left(), rect.top()),
        egui::pos2(center.x, rect.top()),
        egui::pos2(rect.right(), rect.top()),
        egui::pos2(rect.right(), center.y),
        egui::pos2(rect.right(), rect.bottom()),
        egui::pos2(center.x, rect.bottom()),
        egui::pos2(rect.left(), rect.bottom()),
        egui::pos2(rect.left(), center.y),
    ]
}

fn on_top_move_handle_rect(rect: Rect) -> Rect {
    Rect::from_center_size(
        egui::pos2(
            rect.right() + super::ON_TOP_MOVE_HANDLE_OFFSET_PX.x,
            rect.top() + super::ON_TOP_MOVE_HANDLE_OFFSET_PX.y,
        ),
        egui::vec2(
            super::ON_TOP_MOVE_HANDLE_RADIUS_PX * 2.0,
            super::ON_TOP_MOVE_HANDLE_RADIUS_PX * 2.0,
        ),
    )
}

fn move_bubble_by_delta(canvas: &mut CanvasView, bid: i64, du: f32, dv: f32) {
    let Some(page_idx) = canvas
        .bubble_runtime
        .runtime_bubbles
        .get(&bid)
        .map(|bubble| bubble.img_idx)
    else {
        return;
    };
    let (min_margin_u, min_margin_v) = canvas.bubble_min_uv_margin_for_page(page_idx);
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };

    let mut rect = bubble.rect_coords.normalized();
    rect.p1.x = rect.p1.x.clamp(0.0, 1.0);
    rect.p1.y = rect.p1.y.clamp(0.0, 1.0);
    rect.p2.x = rect.p2.x.clamp(0.0, 1.0);
    rect.p2.y = rect.p2.y.clamp(0.0, 1.0);
    rect = rect.normalized();

    let shift_x =
        CanvasView::clamp_bubble_shift_axis(rect.p1.x, rect.p2.x, bubble.img_u, min_margin_u, du);
    let shift_y =
        CanvasView::clamp_bubble_shift_axis(rect.p1.y, rect.p2.y, bubble.img_v, min_margin_v, dv);
    rect.p1.x += shift_x;
    rect.p2.x += shift_x;
    rect.p1.y += shift_y;
    rect.p2.y += shift_y;
    rect = rect.normalized();

    bubble.rect_coords = rect;
    bubble.img_u = (bubble.img_u + shift_x).clamp(min_margin_u, 1.0 - min_margin_u);
    bubble.img_v = (bubble.img_v + shift_y).clamp(min_margin_v, 1.0 - min_margin_v);
}

/// Resizes the red image-area rect of bubble `bid` by dragging handle `idx` toward `scene_pos`,
/// re-normalizes the rect, and keeps the anchor inside it.
///
/// Called every dragged frame from [`draw_rect_handles`]. It re-queues `bid` in `pending_upsert`
/// on each call (final line), so the last frame before release leaves `bid` queued; the
/// `drag_stopped` handler then clears `active_rect_handle`, and the next debounced flush commits the
/// final rect. This is the commit guarantee for a normal on-screen release (no data loss).
fn resize_rect_by_handle(
    canvas: &mut CanvasView,
    bid: i64,
    idx: usize,
    image_rect: Rect,
    scene_pos: Pos2,
) {
    let Some(page_idx) = canvas
        .bubble_runtime
        .runtime_bubbles
        .get(&bid)
        .map(|bubble| bubble.img_idx)
    else {
        return;
    };
    let (min_margin_u, min_margin_v) = canvas.bubble_min_uv_margin_for_page(page_idx);
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };

    let mut rect =
        CanvasView::rect_from_coords(image_rect, bubble.rect_coords).intersect(image_rect);
    if !rect.is_positive() {
        return;
    }

    let min_sc = (8.0 / canvas.state.zoom.max(0.2)).max(4.0);
    if matches!(idx, 0 | 6 | 7) {
        rect.set_left(scene_pos.x.min(rect.right() - min_sc));
    }
    if matches!(idx, 2..=4) {
        rect.set_right(scene_pos.x.max(rect.left() + min_sc));
    }
    if matches!(idx, 0..=2) {
        rect.set_top(scene_pos.y.min(rect.bottom() - min_sc));
    }
    if matches!(idx, 4..=6) {
        rect.set_bottom(scene_pos.y.max(rect.top() + min_sc));
    }

    let uv1 = CanvasView::uv_from_scene(image_rect, rect.left_top());
    let uv2 = CanvasView::uv_from_scene(image_rect, rect.right_bottom());
    bubble.rect_coords = RectCoords {
        p1: egui::pos2(uv1.x.min(uv2.x), uv1.y.min(uv2.y)),
        p2: egui::pos2(uv1.x.max(uv2.x), uv1.y.max(uv2.y)),
    }
    .normalized();

    let anchor = CanvasView::clamp_anchor_to_rect(
        bubble.img_u,
        bubble.img_v,
        bubble.rect_coords,
        min_margin_u,
        min_margin_v,
    );
    bubble.img_u = anchor.x;
    bubble.img_v = anchor.y;
    // Re-queue every dragged frame: the flush is debounced while `active_rect_handle` is set, so the
    // queued id is only consumed once `draw_rect_handles`'s `drag_stopped` clears the handle. Keeping
    // it queued each frame guarantees the final rect commits on a normal on-screen release.
    canvas.bubble_runtime.pending_upsert.insert(bid);
}
