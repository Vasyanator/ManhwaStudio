/*
File: src/canvas/scene.rs

Purpose:
Scene and viewport pipeline for `CanvasView`: page strip reservation/draw,
per-page interactions, and floating canvas controls.

Main responsibilities:
- keep page-strip rendering orchestration outside the main canvas facade;
- reserve page frames, draw ordered scene layers, and route page interactions;
- lay out pages from source-page metadata so hit testing survives source GPU eviction;
- render viewport-space controls without mixing them back into runtime modules;
- preserve `CanvasHooks` compatibility while isolating scene-specific code.

Key structures:
- CanvasSceneState

Key functions:
- CanvasView::begin_canvas_frame()
- CanvasView::draw_canvas_scene()
- CanvasView::reserve_canvas_page_frame()
- CanvasView::draw_canvas_page_base_layers()
- CanvasView::draw_canvas_page_aside_layer()
- CanvasView::draw_canvas_page_on_top_layer()
- CanvasView::handle_canvas_page_interactions()
- CanvasView::draw_canvas_viewport_ui()
- CanvasView::draw_canvas_controls()
- Canvas viewport controls here stay lightweight; advanced ribbon settings live in the
  Settings tab and are synchronized through shared canvas snapshots.

Notes:
- Bubble runtime/persistence remain in `bubble_runtime.rs`.
- Clean-overlay runtime remains in `overlay_runtime.rs`.
*/

use super::bubble_aside_ui;
use super::bubble_on_top_ui;
use super::helpers::page_info_content_size;
use super::types::{
    CanvasContextMenuTarget, CanvasFrameParams, CanvasScenePageFrame, OverlayUploadBudget,
    PendingZoomAnchor, SourceTextureUploadBudget,
};
use super::{
    BubbleCopyPasteTarget, BubbleType, CanvasHooks, CanvasUiStatus, CanvasView, OnTopFocusMode,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::project::{ProjectData, Side};
use crate::runtime_log;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, Pos2, Rect, Sense, Vec2};
use std::collections::HashMap;

const HORIZONTAL_SCROLL_EARLY_FACTOR: f32 = 1.7;
const PIXEL_GRID_MIN_STEP_PT: f32 = 4.0;

pub(super) struct CanvasSceneState {
    pub(super) page_rects: Vec<Rect>,
    pub(super) page_world_rects: Vec<Rect>,
    pub(super) content_world_width: f32,
    pub(super) page_aside_presence: HashMap<usize, [bool; 2]>,
    pub(super) page_aside_widths: HashMap<usize, [f32; 2]>,
    pub(super) scroll_center_idx: usize,
    pub(super) scroll_offset: Vec2,
    pub(super) drag_scroll_blocked: bool,
    pub(super) wheel_scroll_blocked: bool,
    pub(super) zoom_blocked: bool,
    pub(super) zoom_drag_active: bool,
    pub(super) zoom_drag_last_x: f32,
    pub(super) visible_scene_rect: Option<Rect>,
    pub(super) scroll_inner_rect: Option<Rect>,
    pub(super) scroll_content_size: Vec2,
    pub(super) initial_horizontal_scroll_centered: bool,
    pub(super) pending_zoom_anchor: Option<PendingZoomAnchor>,
    pub(super) pending_scroll_offset: Option<Vec2>,
    pub(super) on_top_hit_rects: HashMap<i64, Rect>,
    pub(super) canvas_left_top_controls_rect: Option<Rect>,
}

struct ReservedCanvasPage {
    page_idx: usize,
    page_frame: CanvasScenePageFrame,
    response: egui::Response,
}

pub(super) struct CanvasSceneDrawParams<'a> {
    pub(super) ctx: &'a egui::Context,
    pub(super) ui: &'a mut egui::Ui,
    pub(super) project: &'a ProjectData,
    pub(super) page_infos: &'a HashMap<usize, PageImageInfo>,
    pub(super) texture_cache: &'a mut HashMap<usize, PageTexture>,
    pub(super) hooks: &'a mut dyn CanvasHooks,
    pub(super) frame: CanvasFrameParams,
    pub(super) overlay_budget: &'a mut OverlayUploadBudget,
    pub(super) source_upload_budget: &'a mut SourceTextureUploadBudget,
}

struct CanvasPageBaseLayerParams<'a> {
    ctx: &'a egui::Context,
    ui: &'a mut egui::Ui,
    page_texture: Option<&'a mut PageTexture>,
    page_frame: CanvasScenePageFrame,
    hooks: &'a mut dyn CanvasHooks,
    frame: CanvasFrameParams,
    overlay_budget: &'a mut OverlayUploadBudget,
    source_upload_budget: &'a mut SourceTextureUploadBudget,
}

impl Default for CanvasSceneState {
    fn default() -> Self {
        Self {
            page_rects: Vec::new(),
            page_world_rects: Vec::new(),
            content_world_width: 1.0,
            page_aside_presence: HashMap::new(),
            page_aside_widths: HashMap::new(),
            scroll_center_idx: 0,
            scroll_offset: Vec2::ZERO,
            drag_scroll_blocked: false,
            wheel_scroll_blocked: false,
            zoom_blocked: false,
            zoom_drag_active: false,
            zoom_drag_last_x: 0.0,
            visible_scene_rect: None,
            scroll_inner_rect: None,
            scroll_content_size: Vec2::ZERO,
            initial_horizontal_scroll_centered: false,
            pending_zoom_anchor: None,
            pending_scroll_offset: None,
            on_top_hit_rects: HashMap::new(),
            canvas_left_top_controls_rect: None,
        }
    }
}

impl CanvasView {
    fn canvas_horizontal_scroll_threshold(viewport_width: f32) -> f32 {
        viewport_width.max(1.0) / HORIZONTAL_SCROLL_EARLY_FACTOR
    }

    pub(super) fn canvas_row_screen_width_for_content(
        viewport_width: f32,
        content_screen_width: f32,
    ) -> f32 {
        let viewport_width = viewport_width.max(1.0);
        let content_screen_width = content_screen_width.max(1.0);
        let threshold = Self::canvas_horizontal_scroll_threshold(viewport_width);
        if content_screen_width <= threshold {
            viewport_width
        } else {
            viewport_width + content_screen_width - threshold
        }
    }

    fn canvas_page_x_layout(
        viewport_width: f32,
        content_screen_width: f32,
        image_screen_width: f32,
    ) -> (f32, f32) {
        let row_screen_width =
            Self::canvas_row_screen_width_for_content(viewport_width, content_screen_width);
        let centered_strip_inset_x = ((row_screen_width - content_screen_width).max(0.0)) * 0.5;
        let image_offset_x = ((content_screen_width - image_screen_width) * 0.5).max(0.0);
        (row_screen_width, centered_strip_inset_x + image_offset_x)
    }

    fn viewport_content_inset_x(&self, viewport_width: f32) -> f32 {
        let scaled_content_width = self.scene.content_world_width.max(1.0) * self.state.zoom;
        let row_width =
            Self::canvas_row_screen_width_for_content(viewport_width, scaled_content_width);
        ((row_width - scaled_content_width).max(0.0)) * 0.5
    }

    pub(super) fn max_scroll_offset_x_for_viewport(&self, viewport_width: f32) -> f32 {
        let scaled_content_width = self.scene.content_world_width.max(1.0) * self.state.zoom;
        let row_width =
            Self::canvas_row_screen_width_for_content(viewport_width, scaled_content_width);
        (row_width - viewport_width.max(1.0)).max(0.0)
    }

    pub(super) fn aside_available_widths_for_page_viewport(
        &self,
        image_rect: Rect,
        viewport_rect: Rect,
    ) -> [f32; 2] {
        let side_margin = self.state.side_margin.max(0.0);
        let left_span = (image_rect.left() - viewport_rect.left() - side_margin * 2.0).max(0.0);
        let right_span = (viewport_rect.right() - image_rect.right() - side_margin * 2.0).max(0.0);
        [
            self.calc_bubble_width(left_span),
            self.calc_bubble_width(right_span),
        ]
    }

    pub(super) fn initial_horizontal_center_scroll_offset(
        &mut self,
        viewport_width: f32,
    ) -> Option<Vec2> {
        if self.scene.initial_horizontal_scroll_centered {
            return None;
        }
        let max_scroll_x = self.max_scroll_offset_x_for_viewport(viewport_width);
        if max_scroll_x <= f32::EPSILON {
            return None;
        }
        self.scene.initial_horizontal_scroll_centered = true;
        Some(egui::vec2(
            max_scroll_x * 0.5,
            self.scene.scroll_offset.y.max(0.0),
        ))
    }

    fn canvas_row_width_for_page(&self, page_idx: usize, image_width: f32) -> f32 {
        if image_width <= 0.0 {
            return 1.0;
        }
        let side_margin = self.state.side_margin.max(0.0);
        let aside_scale = self.aside_scale_factor();
        let base_bubble_width = if self.state.scale_bubbles {
            self.state.bubble_max_width.max(self.state.bubble_min_width)
        } else {
            self.state.bubble_min_width
        }
        .max(1.0);
        let expanded_aside_width = base_bubble_width * aside_scale;
        let side_space = (side_margin * 2.0) + expanded_aside_width;

        let [has_left_aside, has_right_aside] = self
            .scene
            .page_aside_presence
            .get(&page_idx)
            .copied()
            .unwrap_or([false, false]);
        let left_extra = if self.state.show_bubbles && has_left_aside {
            side_space
        } else {
            0.0
        };
        let right_extra = if self.state.show_bubbles && has_right_aside {
            side_space
        } else {
            0.0
        };
        let symmetric_side_extra = left_extra.max(right_extra);

        (image_width + symmetric_side_extra * 2.0).max(1.0)
    }

    fn canvas_content_world_width(
        &self,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
    ) -> f32 {
        let mut content_width = 1.0f32;
        for page in &project.pages {
            let Some(page_info) = page_infos.get(&page.idx) else {
                continue;
            };
            let Some(page_size_px) = page_info_content_size(page_info) else {
                continue;
            };
            if page_size_px.x <= 0.0 || page_size_px.y <= 0.0 {
                continue;
            }
            content_width =
                content_width.max(self.canvas_row_width_for_page(page.idx, page_size_px.x));
        }
        content_width
    }

    pub(super) fn capture_pending_zoom_anchor(
        &mut self,
        anchor_pos: Option<Pos2>,
        fallback_rect: Rect,
    ) {
        let viewport_rect = self.scene.scroll_inner_rect.unwrap_or(fallback_rect);
        if !viewport_rect.is_positive() {
            self.scene.pending_zoom_anchor = None;
            return;
        }
        let anchor_screen = anchor_pos.unwrap_or_else(|| viewport_rect.center());
        let viewport_local = egui::vec2(
            (anchor_screen.x - viewport_rect.left()).clamp(0.0, viewport_rect.width()),
            (anchor_screen.y - viewport_rect.top()).clamp(0.0, viewport_rect.height()),
        );
        let old_zoom = self.state.zoom.max(f32::EPSILON);
        let inset_x = self.viewport_content_inset_x(viewport_rect.width());
        let clamped_scroll_x = self.scene.scroll_offset.x.clamp(
            0.0,
            self.max_scroll_offset_x_for_viewport(viewport_rect.width()),
        );
        let content_focus = egui::vec2(
            (clamped_scroll_x + viewport_local.x - inset_x).max(0.0),
            (self.scene.scroll_offset.y + viewport_local.y).max(0.0),
        );
        self.scene.pending_zoom_anchor = Some(PendingZoomAnchor {
            viewport_local,
            world_focus: content_focus / old_zoom,
        });
    }

    pub(super) fn scroll_offset_for_zoom_anchor(&self, anchor: PendingZoomAnchor) -> Vec2 {
        let target_content_pos = anchor.world_focus * self.state.zoom;
        let viewport_width = self
            .scene
            .scroll_inner_rect
            .map_or(0.0, |rect| rect.width())
            .max(0.0);
        let inset_x = self.viewport_content_inset_x(viewport_width);
        let max_scroll_x = self.max_scroll_offset_x_for_viewport(viewport_width);
        egui::vec2(
            (target_content_pos.x + inset_x - anchor.viewport_local.x).clamp(0.0, max_scroll_x),
            (target_content_pos.y - anchor.viewport_local.y).max(0.0),
        )
    }

    fn prime_on_top_aside_focus_selection(
        &mut self,
        ctx: &egui::Context,
        reserved_pages: &[ReservedCanvasPage],
        frame: CanvasFrameParams,
    ) {
        if !self.editable
            || self.state.on_top_focus_mode != OnTopFocusMode::Aside
            || frame.zoom_drag_active
        {
            return;
        }

        let pointer_pos = ctx.input(|i| {
            if i.pointer.primary_down() || i.pointer.primary_clicked() {
                i.pointer.interact_pos()
            } else {
                None
            }
        });
        let Some(pointer_pos) = pointer_pos else {
            return;
        };

        for reserved_page in reserved_pages {
            let page_frame = reserved_page.page_frame;
            if !page_frame.page_in_view || !page_frame.image_rect.contains(pointer_pos) {
                continue;
            }

            if let Some(bid) = bubble_on_top_ui::focus_candidate_at_scene_pos(
                self,
                page_frame.page_idx,
                page_frame.image_rect,
                pointer_pos,
            ) {
                self.bubble_runtime.selected_bubble = Some(bid);
                return;
            }
        }
    }

    pub(super) fn begin_canvas_frame(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        suppress_wheel_scroll: bool,
        zoom_drag_active: bool,
        hooks: &mut dyn CanvasHooks,
    ) -> CanvasFrameParams {
        CanvasFrameParams {
            canvas_rect,
            suppress_wheel_scroll,
            zoom_drag_active,
            hook_claims_shift_drag: hooks.wants_canvas_shift_drag_selection(ctx),
            overlays_enabled: self.overlay_runtime.overlays_model.is_some()
                && self.overlay_runtime.overlays_visible
                && !self.overlay_runtime.overlay_render_suppressed,
            space_pan_drag_enabled: ctx.input(|i| i.key_down(egui::Key::Space))
                && !ctx.wants_keyboard_input(),
        }
    }

    pub(super) fn draw_canvas_scene(
        &mut self,
        params: CanvasSceneDrawParams<'_>,
    ) -> egui::scroll_area::ScrollAreaOutput<()> {
        let CanvasSceneDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            hooks,
            frame,
            overlay_budget,
            source_upload_budget,
        } = params;
        let requested_offset = self
            .scene
            .pending_zoom_anchor
            .map(|anchor| self.scroll_offset_for_zoom_anchor(anchor))
            .or_else(|| self.scene.pending_scroll_offset.take());
        let content_world_width = self.canvas_content_world_width(project, page_infos);
        self.scene.content_world_width = content_world_width;
        let requested_offset = requested_offset
            .or_else(|| self.initial_horizontal_center_scroll_offset(frame.canvas_rect.width()));
        let mut scroll_area = egui::ScrollArea::both()
            .id_salt(self.scroll_area_id_salt)
            .auto_shrink([false, false])
            .scroll_source(egui::scroll_area::ScrollSource {
                scroll_bar: true,
                drag: frame.space_pan_drag_enabled
                    && !self.scene.drag_scroll_blocked
                    && !frame.zoom_drag_active,
                mouse_wheel: !frame.suppress_wheel_scroll && !self.scene.wheel_scroll_blocked,
            });
        if let Some(offset) = requested_offset {
            scroll_area = scroll_area.scroll_offset(offset);
        }

        scroll_area.show(ui, |ui| {
            self.scene.visible_scene_rect = Some(ui.clip_rect());
            let edge_margin_world = self.state.edge_margin.max(0.0);
            let edge_margin = edge_margin_world * self.state.zoom;
            let page_gap_world = if self.state.separate_pages {
                self.state.page_spacing.max(0.0)
            } else {
                0.0
            };
            let page_gap = page_gap_world * self.state.zoom;
            ui.add_space(edge_margin);

            let viewport_center_y = ui.clip_rect().center().y;
            let viewport_rect = ui.clip_rect().expand(256.0);
            let mut nearest_page = 0usize;
            let mut nearest_dist = f32::MAX;
            let mut has_drawn_any_page = false;
            let mut reserved_pages = Vec::new();
            let mut page_world_top = edge_margin_world;

            for page in &project.pages {
                let Some(page_info) = page_infos.get(&page.idx) else {
                    continue;
                };
                let Some(page_size_px) = page_info_content_size(page_info) else {
                    continue;
                };
                if has_drawn_any_page && page_gap > 0.0 {
                    ui.add_space(page_gap);
                    page_world_top += page_gap_world;
                }

                let Some((page_frame, response)) = self.reserve_canvas_page_frame(
                    ui,
                    page.idx,
                    page_size_px,
                    content_world_width,
                    page_world_top,
                    viewport_rect,
                    viewport_center_y,
                    frame.hook_claims_shift_drag,
                    &mut nearest_page,
                    &mut nearest_dist,
                ) else {
                    continue;
                };

                if page_frame.page_in_view && frame.overlays_enabled {
                    let ov_w = page_size_px.x.round().max(1.0) as usize;
                    let ov_h = page_size_px.y.round().max(1.0) as usize;
                    self.ensure_overlay_for_page_size(page.idx, [ov_w, ov_h]);
                }

                reserved_pages.push(ReservedCanvasPage {
                    page_idx: page.idx,
                    page_frame,
                    response,
                });
                has_drawn_any_page = true;
                page_world_top += page_size_px.y;
            }

            for reserved_page in &reserved_pages {
                let page_texture = texture_cache.get_mut(&reserved_page.page_idx);
                self.draw_canvas_page_base_layers(CanvasPageBaseLayerParams {
                    ctx,
                    ui,
                    page_texture,
                    page_frame: reserved_page.page_frame,
                    hooks,
                    frame,
                    overlay_budget,
                    source_upload_budget,
                });
            }

            self.prime_on_top_aside_focus_selection(ctx, &reserved_pages, frame);

            if self.state.show_bubbles {
                for reserved_page in &reserved_pages {
                    self.draw_canvas_page_aside_layer(ui, project, reserved_page.page_frame, hooks);
                }
                for reserved_page in &reserved_pages {
                    self.draw_canvas_page_on_top_layer(
                        ui,
                        project,
                        reserved_page.page_frame,
                        hooks,
                    );
                }
            }

            for reserved_page in &reserved_pages {
                self.handle_canvas_page_interactions(
                    &reserved_page.response,
                    project,
                    reserved_page.page_frame,
                    hooks,
                    frame,
                );
            }

            self.scene.scroll_center_idx = nearest_page;
            ui.add_space(edge_margin);
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn reserve_canvas_page_frame(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        page_size_px: Vec2,
        content_world_width: f32,
        page_world_top: f32,
        viewport_rect: Rect,
        viewport_center_y: f32,
        hook_claims_shift_drag: bool,
        nearest_page: &mut usize,
        nearest_dist: &mut f32,
    ) -> Option<(CanvasScenePageFrame, egui::Response)> {
        if page_size_px.x <= 0.0 || page_size_px.y <= 0.0 {
            return None;
        }
        let image_size = page_size_px * self.state.zoom;
        let viewport_width = ui.clip_rect().width().max(1.0);
        let content_screen_width = content_world_width.max(1.0) * self.state.zoom;
        let (row_screen_width, image_left_offset) =
            Self::canvas_page_x_layout(viewport_width, content_screen_width, image_size.x);
        let row_size = egui::vec2(row_screen_width, image_size.y);
        let row_sense = if hook_claims_shift_drag {
            Sense::hover()
        } else {
            Sense::click()
        };
        let (row_rect, response) = ui.allocate_exact_size(row_size, row_sense);
        let image_rect = Rect::from_min_size(
            egui::pos2(row_rect.left() + image_left_offset, row_rect.top()),
            image_size,
        );

        if page_idx >= self.scene.page_rects.len() {
            self.scene
                .page_rects
                .resize(page_idx + 1, Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        }
        self.scene.page_rects[page_idx] = image_rect;
        if page_idx >= self.scene.page_world_rects.len() {
            self.scene
                .page_world_rects
                .resize(page_idx + 1, Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        }
        self.scene.page_world_rects[page_idx] = Rect::from_min_size(
            egui::pos2(
                ((content_world_width - page_size_px.x) * 0.5).max(0.0),
                page_world_top,
            ),
            page_size_px,
        );
        self.scene.page_aside_widths.insert(
            page_idx,
            self.aside_available_widths_for_page_viewport(image_rect, ui.clip_rect()),
        );

        let page_dist = (image_rect.center().y - viewport_center_y).abs();
        if page_dist < *nearest_dist {
            *nearest_dist = page_dist;
            *nearest_page = page_idx;
        }

        Some((
            CanvasScenePageFrame {
                page_idx,
                row_rect,
                image_rect,
                page_in_view: image_rect.intersects(viewport_rect),
            },
            response,
        ))
    }

    fn draw_canvas_page_base_layers(&mut self, params: CanvasPageBaseLayerParams<'_>) {
        let CanvasPageBaseLayerParams {
            ctx,
            ui,
            page_texture,
            page_frame,
            hooks,
            frame,
            overlay_budget,
            source_upload_budget,
        } = params;
        if !page_frame.page_in_view {
            return;
        }

        let viewport_rect = self.scene.visible_scene_rect.unwrap_or(page_frame.row_rect);

        if let Some(page_texture) = page_texture {
            let mut linear_used_this_frame = false;
            let mut nearest_used_this_frame = false;
            let mut source_upload_work_remaining = false;
            let current_frame = ui.ctx().cumulative_frame_nr();
            for (tile_idx, tile) in page_texture.tiles.iter_mut().enumerate() {
                let tile_rect = Rect::from_min_size(
                    egui::pos2(
                        page_frame.image_rect.left() + tile.origin_px.x * self.state.zoom,
                        page_frame.image_rect.top() + tile.origin_px.y * self.state.zoom,
                    ),
                    tile.size_px * self.state.zoom,
                );
                if !tile_rect.intersects(viewport_rect) {
                    continue;
                }
                if tile.linear_texture.is_none() {
                    if source_upload_budget.try_consume(tile.rgba.len()) {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [tile.size_px.x as usize, tile.size_px.y as usize],
                            &tile.rgba,
                        );
                        tile.linear_texture = Some(ui.ctx().load_texture(
                            format!("page-{}-tile-{}-linear", page_frame.page_idx, tile_idx),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        ));
                    } else {
                        source_upload_work_remaining = true;
                    }
                }
                if self.pixel_sampling_nearest && tile.nearest_texture.is_none() {
                    if source_upload_budget.try_consume(tile.rgba.len()) {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [tile.size_px.x as usize, tile.size_px.y as usize],
                            &tile.rgba,
                        );
                        tile.nearest_texture = Some(ui.ctx().load_texture(
                            format!("page-{}-tile-{}-nearest", page_frame.page_idx, tile_idx),
                            color_image,
                            egui::TextureOptions::NEAREST,
                        ));
                    } else {
                        source_upload_work_remaining = true;
                    }
                }
                let (texture_id, used_nearest) = if self.pixel_sampling_nearest {
                    if let Some(texture) = tile.nearest_texture.as_ref() {
                        (Some(texture.id()), true)
                    } else {
                        (
                            tile.linear_texture.as_ref().map(egui::TextureHandle::id),
                            false,
                        )
                    }
                } else {
                    (
                        tile.linear_texture.as_ref().map(egui::TextureHandle::id),
                        false,
                    )
                };
                if let Some(texture_id) = texture_id {
                    if used_nearest {
                        nearest_used_this_frame = true;
                    } else {
                        linear_used_this_frame = true;
                    }
                    ui.painter().image(
                        texture_id,
                        tile_rect,
                        Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            }
            if linear_used_this_frame {
                page_texture.linear_last_used_frame = current_frame;
            }
            if nearest_used_this_frame {
                page_texture.nearest_last_used_frame = current_frame;
            }
            if source_upload_work_remaining {
                ui.ctx().request_repaint();
            }
        }

        if frame.overlays_enabled {
            self.draw_overlay_on_page(
                ui,
                page_frame.page_idx,
                page_frame.image_rect,
                &mut overlay_budget.tile_budget,
                &mut overlay_budget.bytes_budget,
            );
        }

        hooks.draw_canvas_mask_overlay_on_page(
            ui,
            ctx,
            page_frame.page_idx,
            page_frame.image_rect,
            self.state.zoom,
        );
        hooks.draw_canvas_overlay_on_page(
            ui,
            ctx,
            page_frame.page_idx,
            page_frame.image_rect,
            self.state.zoom,
        );

        if self.pixel_grid_visible {
            self.draw_pixel_grid_on_page(ui, page_frame.image_rect);
        }
    }

    fn draw_pixel_grid_on_page(&self, ui: &mut egui::Ui, image_rect: Rect) {
        let zoom = self.state.zoom;
        if zoom < PIXEL_GRID_MIN_STEP_PT || !image_rect.is_positive() {
            return;
        }
        let clip_rect = ui.clip_rect().intersect(image_rect);
        if !clip_rect.is_positive() {
            return;
        }

        let pixels_per_point = ui.ctx().pixels_per_point().max(1.0);
        let stroke_width = 1.0 / pixels_per_point;
        let align = |value: f32| ((value * pixels_per_point).round() + 0.5) / pixels_per_point;
        let stroke = egui::Stroke::new(
            stroke_width,
            Color32::from_rgba_unmultiplied(16, 16, 16, 52),
        );
        let painter = ui.painter().with_clip_rect(clip_rect);

        let first_col = ((clip_rect.left() - image_rect.left()) / zoom)
            .floor()
            .max(0.0) as usize;
        let last_col = ((clip_rect.right() - image_rect.left()) / zoom)
            .ceil()
            .min((image_rect.width() / zoom).ceil()) as usize;
        for col in first_col..=last_col {
            let x = image_rect.left() + col as f32 * zoom;
            let x = align(x);
            painter.line_segment(
                [
                    egui::pos2(x, clip_rect.top()),
                    egui::pos2(x, clip_rect.bottom()),
                ],
                stroke,
            );
        }

        let first_row = ((clip_rect.top() - image_rect.top()) / zoom)
            .floor()
            .max(0.0) as usize;
        let last_row = ((clip_rect.bottom() - image_rect.top()) / zoom)
            .ceil()
            .min((image_rect.height() / zoom).ceil()) as usize;
        for row in first_row..=last_row {
            let y = image_rect.top() + row as f32 * zoom;
            let y = align(y);
            painter.line_segment(
                [
                    egui::pos2(clip_rect.left(), y),
                    egui::pos2(clip_rect.right(), y),
                ],
                stroke,
            );
        }
    }

    pub(super) fn draw_visible_pixel_grid_overlay(&self, ui: &mut egui::Ui) {
        if !self.pixel_grid_visible {
            return;
        }
        for page_rect in &self.scene.page_rects {
            if page_rect.intersects(ui.clip_rect()) {
                self.draw_pixel_grid_on_page(ui, *page_rect);
            }
        }
    }

    pub(super) fn draw_canvas_page_aside_layer(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_frame: CanvasScenePageFrame,
        hooks: &mut dyn CanvasHooks,
    ) {
        if !page_frame.page_in_view {
            return;
        }

        let aside_left_bubble_ids =
            self.page_bubbles(page_frame.page_idx, Side::Left, BubbleType::Aside);
        let aside_right_bubble_ids =
            self.page_bubbles(page_frame.page_idx, Side::Right, BubbleType::Aside);
        bubble_aside_ui::draw_aside_for_page(
            self,
            ui,
            project,
            page_frame.page_idx,
            page_frame.row_rect,
            page_frame.image_rect,
            aside_left_bubble_ids,
            aside_right_bubble_ids,
            hooks,
        );
    }

    pub(super) fn draw_canvas_page_on_top_layer(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_frame: CanvasScenePageFrame,
        hooks: &mut dyn CanvasHooks,
    ) {
        if !page_frame.page_in_view {
            return;
        }

        let mut on_top_bubble_ids =
            self.page_bubbles(page_frame.page_idx, Side::Left, BubbleType::OnTop);
        on_top_bubble_ids.extend(self.page_bubbles(
            page_frame.page_idx,
            Side::Right,
            BubbleType::OnTop,
        ));
        bubble_on_top_ui::draw_on_top_for_page(
            self,
            ui,
            project,
            page_frame.image_rect,
            on_top_bubble_ids,
            hooks,
        );
    }

    pub(super) fn handle_canvas_page_interactions(
        &mut self,
        response: &egui::Response,
        project: &ProjectData,
        page_frame: CanvasScenePageFrame,
        hooks: &mut dyn CanvasHooks,
        frame: CanvasFrameParams,
    ) {
        if self.editable
            && !frame.hook_claims_shift_drag
            && !frame.zoom_drag_active
            && !hooks.suppress_canvas_page_context_menu(page_frame.page_idx)
            && response.secondary_clicked()
            && let Some(mouse_pos) = response.interact_pointer_pos()
        {
            if page_frame.image_rect.contains(mouse_pos) {
                let clicked_on_bubble =
                    bubble_on_top_ui::on_top_hit_test(
                        self,
                        page_frame.page_idx,
                        page_frame.image_rect,
                        mouse_pos,
                    ) || bubble_aside_ui::aside_hit_test(self, page_frame.page_idx, mouse_pos);
                if !clicked_on_bubble {
                    self.bubble_runtime.canvas_context_menu_target =
                        Some(CanvasContextMenuTarget {
                            page_idx: page_frame.page_idx,
                            page_uv: Self::uv_from_scene(page_frame.image_rect, mouse_pos),
                        });
                } else {
                    self.bubble_runtime.canvas_context_menu_target = None;
                }
            } else {
                self.bubble_runtime.canvas_context_menu_target = None;
            }
        }
        if self.editable
            && !hooks.suppress_canvas_page_context_menu(page_frame.page_idx)
            && self
                .bubble_runtime
                .canvas_context_menu_target
                .is_some_and(|target| target.page_idx == page_frame.page_idx)
        {
            response.context_menu(|ui| {
                let handled_by_hook = self
                    .bubble_runtime
                    .canvas_context_menu_target
                    .filter(|target| target.page_idx == page_frame.page_idx)
                    .is_some_and(|target| {
                        hooks.draw_canvas_page_context_menu(
                            ui,
                            project,
                            target.page_idx,
                            target.page_uv,
                        )
                    });
                if handled_by_hook {
                    return;
                }
                if ui
                    .add_enabled(
                        self.editable,
                        egui::Button::new(self.create_bubble_context_menu_label()),
                    )
                    .clicked()
                {
                    if !self.create_bubble_from_canvas_context_menu(ui.ctx(), project, None) {
                        runtime_log::log_warn(format!(
                            "[canvas::scene] failed to create bubble from context menu; page_idx={}",
                            page_frame.page_idx
                        ));
                    }
                    self.bubble_runtime.canvas_context_menu_target = None;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.editable && self.bubble_runtime.copied_bubble_data.is_some(),
                        egui::Button::new("Вставить пузырь"),
                    )
                    .clicked()
                {
                    if !self.create_bubble_from_canvas_context_menu(
                        ui.ctx(),
                        project,
                        Some(BubbleCopyPasteTarget::WholeBubble),
                    ) {
                        runtime_log::log_warn(format!(
                            "[canvas::scene] failed to paste whole bubble from context menu; page_idx={}",
                            page_frame.page_idx
                        ));
                    }
                    self.bubble_runtime.canvas_context_menu_target = None;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.editable,
                        egui::Button::new("Вставить в новый пузырь (оригинал)"),
                    )
                    .clicked()
                {
                    if !self.create_bubble_from_canvas_context_menu(
                        ui.ctx(),
                        project,
                        Some(BubbleCopyPasteTarget::Original),
                    ) {
                        runtime_log::log_warn(format!(
                            "[canvas::scene] failed to paste original text into new bubble from context menu; page_idx={}",
                            page_frame.page_idx
                        ));
                    }
                    self.bubble_runtime.canvas_context_menu_target = None;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.editable,
                        egui::Button::new("Вставить в новый пузырь (перевод)"),
                    )
                    .clicked()
                {
                    if !self.create_bubble_from_canvas_context_menu(
                        ui.ctx(),
                        project,
                        Some(BubbleCopyPasteTarget::Translation),
                    ) {
                        runtime_log::log_warn(format!(
                            "[canvas::scene] failed to paste translation text into new bubble from context menu; page_idx={}",
                            page_frame.page_idx
                        ));
                    }
                    self.bubble_runtime.canvas_context_menu_target = None;
                    ui.close();
                }
            });
        }
        if !frame.hook_claims_shift_drag && !frame.zoom_drag_active && response.clicked() {
            self.bubble_runtime.canvas_context_menu_target = None;
            if let Some(mouse_pos) = response.interact_pointer_pos() {
                if let Some(bid) = self.bubble_runtime.move_active_bid {
                    self.place_or_move_bubble(
                        bid,
                        page_frame.page_idx,
                        page_frame.image_rect,
                        mouse_pos,
                    );
                    self.bubble_runtime.move_active_bid = None;
                } else {
                    let clicked_on_bubble =
                        bubble_on_top_ui::on_top_hit_test(
                            self,
                            page_frame.page_idx,
                            page_frame.image_rect,
                            mouse_pos,
                        ) || bubble_aside_ui::aside_hit_test(self, page_frame.page_idx, mouse_pos);
                    if !clicked_on_bubble {
                        self.bubble_runtime.selected_bubble = None;
                    }
                }
            }
        }
    }

    pub(super) fn draw_canvas_viewport_ui(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        status: CanvasUiStatus,
        frame: CanvasFrameParams,
        hooks: &mut dyn CanvasHooks,
    ) {
        self.draw_canvas_controls(ctx, frame.canvas_rect, project.pages.len());
        hooks.draw_canvas_overlay_top_left(ctx, frame.canvas_rect, self, project, status);
    }

    pub(super) fn draw_canvas_controls(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        total_pages: usize,
    ) {
        let cur_page = if total_pages == 0 {
            0
        } else {
            self.scene.scroll_center_idx.min(total_pages - 1) + 1
        };
        let page_text = format!("{} / {}", cur_page, total_pages.max(1));
        let zoom_text = format!("{:.1}×", self.state.zoom);

        let controls_area = egui::Area::new("canvas_left_top_controls".into())
            .movable(true)
            .default_pos(canvas_rect.left_top() + egui::vec2(12.0, 12.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let toggle_hint = if self.state.controls_panel_collapsed {
                        "Нажмите, чтобы развернуть панель"
                    } else {
                        "Нажмите, чтобы свернуть панель"
                    };
                    let toggle_icon = if self.state.controls_panel_collapsed {
                        "▶"
                    } else {
                        "▼"
                    };
                    ui.horizontal(|ui| {
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.state.controls_panel_collapsed =
                                !self.state.controls_panel_collapsed;
                        }
                        ui.label(&page_text);
                    });
                    if self.state.controls_panel_collapsed {
                        return;
                    }
                    ui.add_space(2.0);
                    ui.label(zoom_text);
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.state.show_bubbles, "Показывать пузыри");
                    ui.add(
                        WheelSlider::new(&mut self.state.bubble_opacity, 0.0..=1.0)
                            .text("Прозрачность пузырей"),
                    );
                });
            });
        self.scene.canvas_left_top_controls_rect = Some(controls_area.response.rect);
    }

    fn create_bubble_context_menu_label(&self) -> String {
        match self.create_bubble_shortcut_hint.as_deref() {
            Some(shortcut) if !shortcut.is_empty() => format!("Создать пузырь ({shortcut})"),
            _ => "Создать пузырь".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_scroll_keeps_pages_centered_across_widths() {
        let viewport_width = 1000.0;
        let content_screen_width = 1400.0;

        for image_screen_width in [1400.0, 1000.0, 640.0] {
            let (row_width, image_left_offset) = CanvasView::canvas_page_x_layout(
                viewport_width,
                content_screen_width,
                image_screen_width,
            );
            let centered_scroll_offset = (row_width - viewport_width).max(0.0) * 0.5;
            let visible_image_left = image_left_offset - centered_scroll_offset;
            let expected_image_left = (viewport_width - image_screen_width) * 0.5;

            assert!(
                (visible_image_left - expected_image_left).abs() <= f32::EPSILON,
                "image width {image_screen_width} should stay centered in the viewport"
            );
        }
    }
}
