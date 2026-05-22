/*
File: src/canvas/bubble_aside_ui.rs

Purpose:
Aside bubble UI subsystem for `CanvasView`: column layout, repack, link rendering,
hit-testing, and drag interactions for side-mounted bubbles.

Main responsibilities:
- layout aside bubbles into left/right columns around the page;
- render bubble cards and anchor links without pulling persistence into UI code;
- manage aside drag lifecycle for bubble-body and rect-area moves;
- keep runtime bubble geometry in sync with scene interactions.

Key functions:
- draw_aside_for_page()
- draw_aside_column()
- aside_hit_test()
- drag_aside_by_pointer()

Notes:
- Persistence and shared-model sync remain in `bubble_runtime.rs`.
- This module only drives `CanvasView` through existing runtime/edit helpers.
*/

use super::helpers::{
    draw_anchor_link, measure_text_widget_compact_width, measure_text_widget_content_height,
    with_bubble_text_font,
};
use super::types::{
    AsideBubbleCompactMode, AsideBubbleSideMode, AsideDragTarget, BubbleLink, BubbleTextField,
    BubbleType,
};
use super::{CanvasHooks, CanvasView};
use crate::bubble_status::paint_bubble_status_border;
use crate::project::{ProjectData, Side};
use crate::runtime_log;
use crate::widgets::{SpellcheckedTextEdit, misspelled_word_at_pointer};
use eframe::egui;
use egui::{Align, Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke};
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum AsideBubbleBodyMode {
    Full,
    CompactDual,
    CompactSingle(BubbleTextField),
}

#[derive(Clone, Copy)]
struct AsideBubbleVisibleGroups {
    show_header: bool,
    show_original: bool,
    show_translation: bool,
    show_actions: bool,
    show_footer: bool,
    show_readonly_text: bool,
}

fn displayed_aside_side(canvas: &CanvasView, bubble_side: Side) -> Side {
    match canvas.state.aside_side_mode {
        AsideBubbleSideMode::Auto => bubble_side,
        AsideBubbleSideMode::Left => Side::Left,
        AsideBubbleSideMode::Right => Side::Right,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_aside_for_page(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    project: &ProjectData,
    page_idx: usize,
    row_rect: Rect,
    image_rect: Rect,
    left_bubble_ids: Vec<i64>,
    right_bubble_ids: Vec<i64>,
    hooks: &mut dyn CanvasHooks,
) {
    let [left_w, right_w] = canvas
        .scene
        .page_aside_widths
        .get(&page_idx)
        .copied()
        .unwrap_or_else(|| {
            canvas.aside_available_widths_for_page_viewport(
                image_rect,
                canvas.scene.visible_scene_rect.unwrap_or(row_rect),
            )
        });

    let (left_bubble_ids, right_bubble_ids) = match canvas.state.aside_side_mode {
        AsideBubbleSideMode::Auto => (left_bubble_ids, right_bubble_ids),
        AsideBubbleSideMode::Left => {
            let mut merged = left_bubble_ids;
            merged.extend(right_bubble_ids);
            (merged, Vec::new())
        }
        AsideBubbleSideMode::Right => {
            let mut merged = left_bubble_ids;
            merged.extend(right_bubble_ids);
            (Vec::new(), merged)
        }
    };

    let left_rect = Rect::from_min_size(
        egui::pos2(
            image_rect.left() - canvas.state.side_margin - left_w,
            row_rect.top(),
        ),
        egui::vec2(left_w.max(1.0), image_rect.height()),
    );
    let right_rect = Rect::from_min_size(
        egui::pos2(
            image_rect.right() + canvas.state.side_margin,
            row_rect.top(),
        ),
        egui::vec2(right_w.max(1.0), image_rect.height()),
    );

    let mut left_links = Vec::new();
    let mut right_links = Vec::new();
    draw_aside_column(
        canvas,
        ui,
        project,
        Side::Left,
        left_bubble_ids,
        left_rect,
        image_rect,
        &mut left_links,
        hooks,
    );
    draw_aside_column(
        canvas,
        ui,
        project,
        Side::Right,
        right_bubble_ids,
        right_rect,
        image_rect,
        &mut right_links,
        hooks,
    );

    for link in left_links {
        draw_anchor_link(
            ui.painter(),
            image_rect,
            link.img_u,
            link.img_v,
            link.target_x,
            link.target_y,
            Color32::from_rgb(80, 190, 255),
        );
    }
    for link in right_links {
        draw_anchor_link(
            ui.painter(),
            image_rect,
            link.img_u,
            link.img_v,
            link.target_x,
            link.target_y,
            Color32::from_rgb(255, 160, 80),
        );
    }
}

fn aside_body_mode(canvas: &CanvasView, bid: i64, has_translation: bool) -> AsideBubbleBodyMode {
    if !canvas.editable || canvas.bubble_runtime.selected_bubble == Some(bid) {
        return AsideBubbleBodyMode::Full;
    }
    match canvas.state.aside_compact_mode {
        AsideBubbleCompactMode::None => AsideBubbleBodyMode::Full,
        AsideBubbleCompactMode::Moderate => AsideBubbleBodyMode::CompactDual,
        AsideBubbleCompactMode::Strong => AsideBubbleBodyMode::CompactSingle(if has_translation {
            BubbleTextField::Translation
        } else {
            BubbleTextField::Original
        }),
    }
}

fn aside_visible_groups(
    editable: bool,
    body_mode: AsideBubbleBodyMode,
    has_header: bool,
) -> AsideBubbleVisibleGroups {
    if !editable {
        return AsideBubbleVisibleGroups {
            show_header: has_header,
            show_original: false,
            show_translation: false,
            show_actions: false,
            show_footer: false,
            show_readonly_text: true,
        };
    }

    match body_mode {
        AsideBubbleBodyMode::Full => AsideBubbleVisibleGroups {
            show_header: true,
            show_original: true,
            show_translation: true,
            show_actions: true,
            show_footer: true,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactDual => AsideBubbleVisibleGroups {
            show_header: false,
            show_original: true,
            show_translation: true,
            show_actions: false,
            show_footer: false,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Original) => AsideBubbleVisibleGroups {
            show_header: false,
            show_original: true,
            show_translation: false,
            show_actions: false,
            show_footer: false,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Translation) => {
            AsideBubbleVisibleGroups {
                show_header: false,
                show_original: false,
                show_translation: true,
                show_actions: false,
                show_footer: false,
                show_readonly_text: false,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn estimate_aside_body_height(
    ui: &egui::Ui,
    original_text: &str,
    translation_text: &str,
    display_text: &str,
    base_width_px: f32,
    body_mode: AsideBubbleBodyMode,
    editable: bool,
    scale_factor: f32,
    frame_inner_margin_px: f32,
    has_header: bool,
) -> f32 {
    let margin_unscaled = frame_inner_margin_px / scale_factor.max(f32::EPSILON);
    let content_width_px = (base_width_px - margin_unscaled * 2.0).max(40.0);
    let original_height =
        measure_text_widget_content_height(ui, original_text, content_width_px) * scale_factor;
    let translation_height =
        measure_text_widget_content_height(ui, translation_text, content_width_px) * scale_factor;
    let display_height =
        measure_text_widget_content_height(ui, display_text, content_width_px) * scale_factor;
    let vertical_padding = frame_inner_margin_px * 2.0 + 2.0;
    let item_spacing = ui.style().spacing.item_spacing.y * scale_factor;
    let chrome_row_height = ui.style().spacing.interact_size.y * scale_factor;
    if !editable {
        let header_height = if has_header {
            chrome_row_height + 4.0
        } else {
            0.0
        };
        return display_height + vertical_padding + header_height;
    }
    match body_mode {
        AsideBubbleBodyMode::Full => {
            let action_spacing = (6.0 + 4.0) * scale_factor;
            let inter_row_spacing = item_spacing * 2.0;
            let safety_padding = 6.0 * scale_factor;
            let editable_extra_height =
                chrome_row_height * 2.0 + action_spacing + inter_row_spacing + safety_padding;
            original_height
                + translation_height
                + vertical_padding
                + item_spacing
                + editable_extra_height
        }
        AsideBubbleBodyMode::CompactDual => {
            original_height + translation_height + vertical_padding + item_spacing
        }
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Original) => {
            original_height + vertical_padding
        }
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Translation) => {
            translation_height + vertical_padding
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_aside_column(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    project: &ProjectData,
    side: Side,
    bubble_ids: Vec<i64>,
    column_rect: Rect,
    image_rect: Rect,
    out_links: &mut Vec<BubbleLink>,
    hooks: &mut dyn CanvasHooks,
) {
    let scale_factor = canvas.aside_scale_factor();
    let base_column_width = column_rect.width().max(1.0);
    let scaled_column_width = (base_column_width * scale_factor).max(1.0);
    let frame_inner_margin = (8.0 * scale_factor).round().clamp(2.0, 48.0) as i8;
    let scaled_bubble_style = if (scale_factor - 1.0).abs() > f32::EPSILON {
        let mut style = ui.style().as_ref().clone();
        for font in style.text_styles.values_mut() {
            font.size = (font.size * scale_factor).max(1.0);
        }
        style.spacing.item_spacing *= scale_factor;
        style.spacing.button_padding *= scale_factor;
        style.spacing.interact_size *= scale_factor;
        Some(style)
    } else {
        None
    };
    if bubble_ids.is_empty() || !column_rect.is_positive() {
        return;
    }

    #[derive(Clone)]
    struct AsideDesiredSlot {
        bid: i64,
        desired_cy: f32,
        h: f32,
        source_scene_x: f32,
        source_scene_y: f32,
        angle_key: f32,
    }

    let mut desired_slots: Vec<AsideDesiredSlot> = Vec::new();
    let mut bubble_widths: HashMap<i64, f32> = HashMap::new();
    let gap = 0.0;
    let image_center_y = image_rect.center().y;
    let side_edge_x = match side {
        Side::Left => image_rect.left(),
        Side::Right => image_rect.right(),
    };

    for bid in bubble_ids {
        let Some(b) = canvas.bubble_runtime.runtime_bubbles.get(&bid) else {
            continue;
        };
        let body_mode = aside_body_mode(canvas, bid, !b.text.trim().is_empty());
        let hook_bubble_fallback;
        let hook_bubble = match project.bubbles.iter().find(|bubble| bubble.id == bid) {
            Some(project_bubble) => project_bubble,
            None => {
                hook_bubble_fallback = canvas.hook_bubble_for_runtime(project, b);
                &hook_bubble_fallback
            }
        };
        let has_header = hooks.has_bubble_header(hook_bubble, canvas.editable);
        let readonly_bubble_width = if canvas.editable {
            scaled_column_width
        } else {
            let frame_inner_margin_px = f32::from(frame_inner_margin);
            let text_width =
                measure_text_widget_compact_width(ui, b.display_text(), base_column_width);
            let header_width = if has_header {
                hooks
                    .readonly_aside_header_width_hint(ui, hook_bubble, canvas.editable)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            (text_width.max(header_width) + frame_inner_margin_px * 2.0)
                .clamp(1.0, scaled_column_width)
        };
        let estimated_h = estimate_aside_body_height(
            ui,
            &b.original_text,
            &b.text,
            b.display_text(),
            readonly_bubble_width,
            body_mode,
            canvas.editable,
            scale_factor,
            f32::from(frame_inner_margin),
            has_header,
        );
        let measured_h = if b.mounted && b.height_px.is_finite() {
            b.height_px.max(1.0)
        } else {
            0.0
        };
        let h = estimated_h.max(measured_h).max(1.0);
        let source_scene_x = image_rect.left() + b.img_u.clamp(0.0, 1.0) * image_rect.width();
        let source_scene_y = image_rect.top() + b.img_v.clamp(0.0, 1.0) * image_rect.height();
        let desired_cy = source_scene_y;
        let edge_dx = match side {
            Side::Left => source_scene_x - side_edge_x,
            Side::Right => side_edge_x - source_scene_x,
        }
        .max(1.0);
        let angle_key = (source_scene_y - image_center_y).atan2(edge_dx);
        desired_slots.push(AsideDesiredSlot {
            bid,
            desired_cy,
            h,
            source_scene_x,
            source_scene_y,
            angle_key,
        });
        bubble_widths.insert(bid, readonly_bubble_width);
    }
    if desired_slots.is_empty() {
        return;
    }
    desired_slots.sort_by(|a, b| {
        a.desired_cy
            .total_cmp(&b.desired_cy)
            .then_with(|| a.bid.cmp(&b.bid))
    });

    #[derive(Default)]
    struct Cluster {
        items: Vec<AsideDesiredSlot>,
        block_h: f32,
        top: f32,
    }

    let mut clusters: Vec<Cluster> = Vec::new();
    let mut current: Vec<AsideDesiredSlot> = Vec::new();
    let mut current_bottom = 0.0f32;

    let flush_cluster = |clusters: &mut Vec<Cluster>, items: &mut Vec<AsideDesiredSlot>| {
        if items.is_empty() {
            return;
        }
        let count = items.len() as f32;
        let desired_center = items.iter().map(|item| item.desired_cy).sum::<f32>() / count.max(1.0);
        let body_h = items.iter().map(|item| item.h).sum::<f32>();
        let block_h = body_h + gap * (items.len().saturating_sub(1) as f32);
        let top = desired_center - block_h * 0.5;
        clusters.push(Cluster {
            items: std::mem::take(items),
            block_h,
            top,
        });
    };

    for slot in desired_slots {
        let top = slot.desired_cy - slot.h * 0.5;
        let bottom = slot.desired_cy + slot.h * 0.5;

        if current.is_empty() {
            current_bottom = bottom;
            current.push(slot);
            continue;
        }

        if top <= current_bottom + gap {
            current_bottom = current_bottom.max(bottom);
            current.push(slot);
        } else {
            flush_cluster(&mut clusters, &mut current);
            current_bottom = bottom;
            current.push(slot);
        }
    }
    flush_cluster(&mut clusters, &mut current);
    if clusters.is_empty() {
        return;
    }

    let top_bound = column_rect.top();
    let bottom_bound = column_rect.bottom();
    for i in 0..clusters.len() {
        let min_top = if i == 0 {
            top_bound
        } else {
            clusters[i - 1].top + clusters[i - 1].block_h + gap
        };
        clusters[i].top = clusters[i].top.max(min_top);
    }
    for i in (0..clusters.len()).rev() {
        let max_top = if i + 1 >= clusters.len() {
            bottom_bound - clusters[i].block_h
        } else {
            clusters[i + 1].top - gap - clusters[i].block_h
        };
        clusters[i].top = clusters[i].top.min(max_top);
    }
    for i in 0..clusters.len() {
        let min_top = if i == 0 {
            top_bound
        } else {
            clusters[i - 1].top + clusters[i - 1].block_h + gap
        };
        clusters[i].top = clusters[i].top.max(min_top);
    }

    let oriented_target_x = match side {
        Side::Left => -column_rect.right(),
        Side::Right => column_rect.left(),
    };
    let oriented_source_x = |x: f32| -> f32 {
        match side {
            Side::Left => -x,
            Side::Right => x,
        }
    };
    let y_at_x = |sx: f32, sy: f32, ty: f32, x: f32| -> f32 {
        let denom = oriented_target_x - sx;
        if denom.abs() <= 0.0001 {
            sy
        } else {
            sy + (ty - sy) * ((x - sx) / denom)
        }
    };
    let lines_cross = |a: (f32, f32, f32), b: (f32, f32, f32)| -> bool {
        let overlap_start_x = a.0.max(b.0);
        if overlap_start_x >= oriented_target_x - 0.0001 {
            return false;
        }
        let a_overlap_y = y_at_x(a.0, a.1, a.2, overlap_start_x);
        let b_overlap_y = y_at_x(b.0, b.1, b.2, overlap_start_x);
        let diff_at_overlap = a_overlap_y - b_overlap_y;
        let diff_at_target = a.2 - b.2;
        (diff_at_overlap > 0.001 && diff_at_target < -0.001)
            || (diff_at_overlap < -0.001 && diff_at_target > 0.001)
    };
    let count_crossings = |items: &[AsideDesiredSlot], top: f32| -> usize {
        if items.len() < 2 {
            return 0;
        }
        let mut lines: Vec<(f32, f32, f32)> = Vec::with_capacity(items.len());
        let mut cursor = top;
        for item in items {
            let target_y = cursor + item.h * 0.5;
            cursor += item.h + gap;
            lines.push((
                oriented_source_x(item.source_scene_x),
                item.source_scene_y,
                target_y,
            ));
        }
        let mut crossings = 0usize;
        for i in 0..lines.len() {
            for j in (i + 1)..lines.len() {
                if lines_cross(lines[i], lines[j]) {
                    crossings = crossings.saturating_add(1);
                }
            }
        }
        crossings
    };

    let mut slots: Vec<(i64, f32, f32)> = Vec::new();
    for cluster in &mut clusters {
        if cluster.items.len() > 1 {
            cluster.items.sort_by(|a, b| {
                a.angle_key
                    .total_cmp(&b.angle_key)
                    .then_with(|| a.desired_cy.total_cmp(&b.desired_cy))
                    .then_with(|| a.bid.cmp(&b.bid))
            });

            if cluster.items.len() <= 48 {
                let mut best_crossings = count_crossings(&cluster.items, cluster.top);
                if best_crossings > 0 {
                    let mut improved = true;
                    while improved && best_crossings > 0 {
                        improved = false;
                        for idx in 0..(cluster.items.len() - 1) {
                            cluster.items.swap(idx, idx + 1);
                            let candidate_crossings = count_crossings(&cluster.items, cluster.top);
                            if candidate_crossings < best_crossings {
                                best_crossings = candidate_crossings;
                                improved = true;
                            } else {
                                cluster.items.swap(idx, idx + 1);
                            }
                        }
                    }
                }
            }
        }

        let mut top = cluster.top;
        for item in &cluster.items {
            let cy = top + item.h * 0.5;
            slots.push((item.bid, cy, item.h));
            top += item.h + gap;
        }
    }

    for (bid, cy, h) in slots {
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

        let bubble_top = cy - h * 0.5;
        let bubble_width = bubble_widths
            .get(&bid)
            .copied()
            .unwrap_or(scaled_column_width);
        let bubble_left = match side {
            Side::Left => column_rect.right() - bubble_width,
            Side::Right => column_rect.left(),
        };
        let bubble_slot_rect = Rect::from_min_size(
            egui::pos2(bubble_left, bubble_top),
            egui::vec2(bubble_width, h),
        );
        let bubble_available_rect = Rect::from_min_max(
            bubble_slot_rect.min,
            egui::pos2(
                bubble_slot_rect.right(),
                column_rect.bottom().max(bubble_slot_rect.bottom()),
            ),
        );
        let selected = canvas.bubble_runtime.selected_bubble == Some(bid);
        let frame_color = if selected {
            Color32::from_rgb(42, 54, 71)
        } else {
            Color32::from_rgb(35, 35, 42)
        };
        let frame = egui::Frame::new()
            .fill(frame_color.gamma_multiply(canvas.state.bubble_opacity))
            .stroke(Stroke::new(1.0, Color32::from_gray(90)))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::same(frame_inner_margin));
        let status_stroke = if canvas.editable && canvas.state.show_bubble_status {
            hooks.bubble_status_style(hook_bubble, canvas.editable, canvas)
        } else {
            None
        };

        let mut new_original = snapshot.original_text.clone();
        let mut new_text = snapshot.text.clone();
        let txt_owned = snapshot.display_text().to_owned();
        let mut want_paste_original = false;
        let mut want_paste_translation = false;
        let mut want_copy_whole_bubble = false;
        let mut want_duplicate_bubble = false;
        let mut want_paste_whole_bubble = false;
        let mut want_translate = false;
        let mut want_delete = false;
        let mut want_switch_bubble_type = None;
        let mut text_changed = false;
        let mut interacted_with_bubble = false;
        let mut bubble_has_focus = selected;
        let rtl_align_frame = !canvas.editable && side == Side::Left;
        let body_mode = aside_body_mode(canvas, bid, !snapshot.text.trim().is_empty());
        let has_header = hooks.has_bubble_header(hook_bubble, canvas.editable);
        let visible_groups = aside_visible_groups(canvas.editable, body_mode, has_header);

        let scene_clip_rect = ui.clip_rect();
        let zoom_drag_active = canvas.scene.zoom_drag_active;

        let mut bubble_body = |ui: &mut egui::Ui| {
            if !canvas.editable && rtl_align_frame {
                ui.vertical(|ui| {
                    if visible_groups.show_header {
                        ui.horizontal(|ui| {
                            hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
                        });
                        ui.add_space(4.0);
                    }
                    if visible_groups.show_readonly_text {
                        with_bubble_text_font(ui, |ui| {
                            ui.add(egui::Label::new(txt_owned.as_str()).wrap());
                        });
                    }
                });
            } else {
                if visible_groups.show_header {
                    ui.horizontal(|ui| {
                        hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
                    });
                    ui.add_space(4.0);
                }

                if canvas.editable {
                    let text_width = ui.available_width().max(40.0);
                    if visible_groups.show_original {
                        let original_spellcheck_enabled = !canvas.bubble_spellcheck_disabled(
                            project,
                            bid,
                            BubbleTextField::Original,
                        );
                        let orig_resp = with_bubble_text_font(ui, |ui| {
                            SpellcheckedTextEdit::multiline(&mut new_original)
                                .id_salt(("aside_original", bid))
                                .hint_text("Оригинал")
                                .desired_width(text_width)
                                .desired_rows(1)
                                .spellcheck_enabled(
                                    canvas.state.spellcheck_original && original_spellcheck_enabled,
                                )
                                .show(ui)
                        });
                        let orig_misspelled_word =
                            misspelled_word_at_pointer(ui, &orig_resp, &new_original);
                        canvas.note_focused_bubble_text_input(
                            ui.ctx(),
                            bid,
                            BubbleTextField::Original,
                            &orig_resp.response,
                        );
                        if orig_resp.response.clicked() || orig_resp.response.changed() {
                            interacted_with_bubble = true;
                        }
                        if orig_resp.response.has_focus() {
                            canvas.bubble_runtime.selected_bubble = Some(bid);
                            interacted_with_bubble = true;
                        }
                        bubble_has_focus = bubble_has_focus || orig_resp.response.has_focus();
                        if orig_resp.response.changed() {
                            text_changed = true;
                            canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                        }
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
                    }
                    if visible_groups.show_translation {
                        let translation_spellcheck_enabled = !canvas.bubble_spellcheck_disabled(
                            project,
                            bid,
                            BubbleTextField::Translation,
                        );
                        let tr_resp = with_bubble_text_font(ui, |ui| {
                            SpellcheckedTextEdit::multiline(&mut new_text)
                                .id_salt(("aside_translation", bid))
                                .hint_text("Перевод")
                                .desired_width(text_width)
                                .desired_rows(1)
                                .spellcheck_enabled(
                                    canvas.state.spellcheck_translation
                                        && translation_spellcheck_enabled,
                                )
                                .show(ui)
                        });
                        let tr_misspelled_word =
                            misspelled_word_at_pointer(ui, &tr_resp, &new_text);
                        canvas.note_focused_bubble_text_input(
                            ui.ctx(),
                            bid,
                            BubbleTextField::Translation,
                            &tr_resp.response,
                        );
                        if tr_resp.response.clicked() || tr_resp.response.changed() {
                            interacted_with_bubble = true;
                        }
                        if tr_resp.response.has_focus() {
                            canvas.bubble_runtime.selected_bubble = Some(bid);
                            interacted_with_bubble = true;
                        }
                        bubble_has_focus = bubble_has_focus || tr_resp.response.has_focus();
                        if tr_resp.response.changed() {
                            text_changed = true;
                            canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                        }
                        if tr_resp.response.lost_focus() {
                            canvas.commit_text_upsert_now(bid);
                        }
                        if tr_resp.response.secondary_clicked() {
                            canvas.bubble_runtime.bubble_context_menu_misspelled_word =
                                tr_misspelled_word.clone();
                        }
                        tr_resp.response.context_menu(|ui| {
                            canvas.show_bubble_context_menu(
                                ui,
                                project,
                                bid,
                                snapshot.bubble_type,
                                &new_original,
                                &new_text,
                                tr_misspelled_word.as_deref(),
                                &mut want_copy_whole_bubble,
                                &mut want_duplicate_bubble,
                                &mut want_paste_whole_bubble,
                                &mut want_paste_original,
                                &mut want_paste_translation,
                                &mut want_switch_bubble_type,
                                &mut interacted_with_bubble,
                            );
                        });
                    }
                } else if visible_groups.show_readonly_text {
                    with_bubble_text_font(ui, |ui| {
                        ui.add(egui::Label::new(txt_owned.as_str()).wrap());
                    });
                }
            }

            if visible_groups.show_actions {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("Перевести").clicked() {
                        want_translate = true;
                    }
                    if ui.small_button("Удалить").clicked() {
                        want_delete = true;
                    }
                });
            }

            if visible_groups.show_footer {
                hooks.build_bubble_footer(ui, hook_bubble, canvas.editable);
            }
        };
        let bubble_hit_response = ui.interact(
            bubble_slot_rect,
            Id::new(("aside_bubble_hit", bid)),
            if zoom_drag_active {
                Sense::hover()
            } else {
                Sense::click_and_drag()
            },
        );
        let mut bubble_ui = CanvasView::new_scene_overlay_child(
            ui,
            bubble_available_rect,
            scene_clip_rect,
            egui::Layout::top_down(Align::LEFT),
        );
        bubble_ui.set_max_width(bubble_slot_rect.width());
        if let Some(style) = scaled_bubble_style.as_ref() {
            bubble_ui.set_style(style.clone());
        }
        let response = if rtl_align_frame {
            bubble_ui
                .with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                    frame.show(ui, |ui| bubble_body(ui)).response
                })
                .inner
        } else {
            frame.show(&mut bubble_ui, |ui| bubble_body(ui)).response
        };
        if let Some(style) = status_stroke {
            paint_bubble_status_border(ui.painter(), response.rect, CornerRadius::same(6), style);
        }
        bubble_hit_response.context_menu(|ui| {
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
        if bubble_hit_response.secondary_clicked() {
            canvas.bubble_runtime.bubble_context_menu_misspelled_word = None;
        }

        let pressed_primary_on_bubble = bubble_hit_response.is_pointer_button_down_on()
            && ui.ctx().input(|i| i.pointer.primary_down());
        if pressed_primary_on_bubble {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
        }
        if response.clicked() || bubble_hit_response.clicked() {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
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
        let selected_now = canvas.bubble_runtime.selected_bubble == Some(bid);
        if selected_now {
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
                "[canvas::bubble_aside_ui] failed to copy bubble payload; bubble_id={bid}"
            ));
        }
        if want_duplicate_bubble
            && !canvas.duplicate_bubble_below(project, bid, ui.ctx().input(|i| i.time))
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_aside_ui] failed to duplicate bubble; bubble_id={bid}"
            ));
        }
        if want_paste_whole_bubble
            && !canvas.paste_copied_whole_bubble_into_bid(project, bid, ui.ctx().input(|i| i.time))
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_aside_ui] failed to paste copied bubble payload; bubble_id={bid}"
            ));
        }
        if let Some(next_type) = want_switch_bubble_type
            && !canvas.set_bubble_type_for_bid(bid, next_type)
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_aside_ui] failed to switch bubble type; bubble_id={bid}; next_type={}",
                next_type.as_str()
            ));
        }
        if want_translate {
            canvas.bubble_runtime.pending_translate.insert(bid);
        }
        if want_delete {
            canvas.bubble_runtime.pending_delete.insert(bid);
        }

        let can_drag_aside = canvas.editable
            && selected_now
            && !canvas.scene.zoom_drag_active
            && canvas.bubble_runtime.move_active_bid.is_none()
            && !ui.ctx().wants_keyboard_input()
            && canvas
                .bubble_runtime
                .active_rect_handle
                .is_none_or(|(active_bid, _)| active_bid != bid);
        let mut drag_stopped = false;
        if can_drag_aside
            && bubble_hit_response.drag_started()
            && let Some(pos) = bubble_hit_response.interact_pointer_pos()
        {
            start_aside_drag(canvas, bid, AsideDragTarget::BubbleBody, pos);
        }
        if can_drag_aside
            && bubble_hit_response.dragged()
            && let Some(pos) = bubble_hit_response.interact_pointer_pos()
        {
            drag_aside_by_pointer(canvas, bid, image_rect, pos);
        }
        if canvas
            .bubble_runtime
            .aside_drag_state
            .is_some_and(|state| state.bid == bid)
            && bubble_hit_response.drag_stopped()
        {
            drag_stopped = true;
        }

        let show_rect = canvas.editable
            && selected_now
            && (bubble_has_focus
                || canvas
                    .bubble_runtime
                    .active_rect_handle
                    .is_some_and(|(active_bid, _)| active_bid == bid));
        if show_rect {
            let coords = canvas
                .bubble_runtime
                .runtime_bubbles
                .get(&bid)
                .map(|bubble| bubble.rect_coords)
                .unwrap_or(snapshot.rect_coords);
            let rect = CanvasView::rect_from_coords(image_rect, coords).intersect(image_rect);
            if rect.is_positive() {
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    Stroke::new(3.0, Color32::from_rgb(0, 120, 215)),
                    egui::StrokeKind::Inside,
                );
                let rect_drag_response = ui.interact(
                    rect,
                    Id::new(("aside_rect_drag", bid)),
                    if canvas.scene.zoom_drag_active {
                        Sense::hover()
                    } else {
                        Sense::click_and_drag()
                    },
                );
                let pointer_on_handle =
                    rect_drag_response
                        .interact_pointer_pos()
                        .is_some_and(|pos| {
                            super::bubble_on_top_ui::is_scene_pos_on_rect_handle(rect, pos)
                        });
                let can_drag_rect = can_drag_aside && !pointer_on_handle;
                if can_drag_rect
                    && rect_drag_response.drag_started()
                    && let Some(pos) = rect_drag_response.interact_pointer_pos()
                {
                    start_aside_drag(canvas, bid, AsideDragTarget::RectArea, pos);
                }
                if can_drag_rect
                    && rect_drag_response.dragged()
                    && let Some(pos) = rect_drag_response.interact_pointer_pos()
                {
                    drag_aside_by_pointer(canvas, bid, image_rect, pos);
                }
                if canvas
                    .bubble_runtime
                    .aside_drag_state
                    .is_some_and(|state| state.bid == bid)
                    && rect_drag_response.drag_stopped()
                {
                    drag_stopped = true;
                }
            }
            super::bubble_on_top_ui::draw_rect_handles(canvas, ui, bid, image_rect, coords);
        }
        if drag_stopped {
            finish_aside_drag(canvas, bid);
        }
        let measured_height = response.rect.height().max(1.0);
        let measured_width = response.rect.width().max(1.0);
        let measured_anchor_y = response.rect.center().y;
        let layout_changed =
            canvas
                .bubble_runtime
                .runtime_bubbles
                .get(&bid)
                .is_some_and(|bubble| {
                    (bubble.height_px - measured_height).abs() > 0.5
                        || (bubble.max_width_px - measured_width).abs() > 0.5
                        || (bubble.anchor_y - measured_anchor_y).abs() > 0.5
                });
        if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            if text_changed {
                bubble.original_text = new_original;
                bubble.text = new_text;
                bubble.mounted = true;
            }
            bubble.anchor_y = measured_anchor_y;
            bubble.height_px = measured_height;
            bubble.max_width_px = measured_width;
            bubble.line_x = match side {
                Side::Left => response.rect.right(),
                Side::Right => response.rect.left(),
            };
            bubble.mounted = true;
        }
        if layout_changed {
            ui.ctx().request_repaint();
        }

        let bubble_edge_x = match side {
            Side::Left => response.rect.right(),
            Side::Right => response.rect.left(),
        };
        let (link_img_u, link_img_v) = canvas
            .bubble_runtime
            .runtime_bubbles
            .get(&bid)
            .map(|bubble| (bubble.img_u, bubble.img_v))
            .unwrap_or((snapshot.img_u, snapshot.img_v));
        let line_start = egui::pos2(
            image_rect.left() + link_img_u.clamp(0.0, 1.0) * image_rect.width(),
            image_rect.top() + link_img_v.clamp(0.0, 1.0) * image_rect.height(),
        );
        let line_end = egui::pos2(bubble_edge_x, response.rect.center().y);
        if !hooks.should_hide_aside_bubble_line(snapshot.img_idx, hook_bubble, line_start, line_end)
        {
            out_links.push(BubbleLink {
                img_u: link_img_u,
                img_v: link_img_v,
                target_x: bubble_edge_x,
                target_y: response.rect.center().y,
            });
        }
    }
}

pub(super) fn aside_hit_test(canvas: &CanvasView, page_idx: usize, scene_pos: Pos2) -> bool {
    for bubble in canvas.bubble_runtime.runtime_bubbles.values() {
        if bubble.img_idx != page_idx
            || canvas.displayed_bubble_type_for_runtime(bubble) != BubbleType::Aside
            || !bubble.mounted
        {
            continue;
        }
        let rect = match displayed_aside_side(canvas, bubble.side) {
            Side::Left => Rect::from_center_size(
                egui::pos2(bubble.line_x - bubble.max_width_px * 0.5, bubble.anchor_y),
                egui::vec2(bubble.max_width_px.max(1.0), bubble.height_px.max(1.0)),
            ),
            Side::Right => Rect::from_center_size(
                egui::pos2(bubble.line_x + bubble.max_width_px * 0.5, bubble.anchor_y),
                egui::vec2(bubble.max_width_px.max(1.0), bubble.height_px.max(1.0)),
            ),
        };
        if rect.contains(scene_pos) {
            return true;
        }
    }
    false
}

pub(super) fn start_aside_drag(
    canvas: &mut CanvasView,
    bid: i64,
    target: AsideDragTarget,
    pointer_pos: Pos2,
) {
    canvas.bubble_runtime.aside_drag_state = Some(super::types::AsideDragState {
        bid,
        target,
        last_pointer_pos: pointer_pos,
        moved: false,
    });
}

pub(super) fn drag_aside_by_pointer(
    canvas: &mut CanvasView,
    bid: i64,
    image_rect: Rect,
    pointer_pos: Pos2,
) {
    let Some(mut state) = canvas.bubble_runtime.aside_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    let dx = pointer_pos.x - state.last_pointer_pos.x;
    let dy = pointer_pos.y - state.last_pointer_pos.y;
    state.last_pointer_pos = pointer_pos;
    canvas.bubble_runtime.aside_drag_state = Some(state);

    if dx.abs() <= f32::EPSILON && dy.abs() <= f32::EPSILON {
        return;
    }
    let du = dx / image_rect.width().max(1.0);
    let dv = dy / image_rect.height().max(1.0);
    match state.target {
        AsideDragTarget::BubbleBody => {
            let Some(current) = canvas.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
                return;
            };
            canvas.move_bubble_anchor_impl(
                bid,
                current.img_u + du,
                current.img_v + dv,
                true,
                false,
            );
        }
        AsideDragTarget::RectArea => {
            move_bubble_rect_by_delta(canvas, bid, du, dv);
        }
    }
    if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
        bubble.side = if bubble.img_u < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
    }
    if let Some(state) = canvas.bubble_runtime.aside_drag_state.as_mut()
        && state.bid == bid
    {
        state.moved = true;
        state.last_pointer_pos = pointer_pos;
    }
}

pub(super) fn finish_aside_drag(canvas: &mut CanvasView, bid: i64) {
    let Some(state) = canvas.bubble_runtime.aside_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    canvas.bubble_runtime.aside_drag_state = None;
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let next_side = if bubble.img_u < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let mut changed = state.moved;
    if bubble.side != next_side {
        bubble.side = next_side;
        changed = true;
    }
    if changed {
        canvas.bubble_runtime.pending_upsert.insert(bid);
    }
}

fn move_bubble_rect_by_delta(canvas: &mut CanvasView, bid: i64, du: f32, dv: f32) {
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

    let shift_x = CanvasView::clamp_rect_shift_axis(rect.p1.x, rect.p2.x, du);
    let shift_y = CanvasView::clamp_rect_shift_axis(rect.p1.y, rect.p2.y, dv);
    rect.p1.x += shift_x;
    rect.p2.x += shift_x;
    rect.p1.y += shift_y;
    rect.p2.y += shift_y;
    rect = rect.normalized();

    let anchor = CanvasView::clamp_anchor_to_rect(
        bubble.img_u,
        bubble.img_v,
        rect,
        min_margin_u,
        min_margin_v,
    );
    bubble.rect_coords = rect;
    bubble.img_u = anchor.x;
    bubble.img_v = anchor.y;
}
