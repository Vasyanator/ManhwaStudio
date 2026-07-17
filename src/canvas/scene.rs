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
- CanvasView::draw_canvas_bottom_hint()
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
    PendingPageFocus, PendingZoomAnchor, SourceTextureUploadBudget,
};
use super::view_transform::DVec2;
use super::{
    BubbleCopyPasteTarget, CanvasHintHelp, CanvasHintRow, CanvasHooks, CanvasUiStatus, CanvasView,
    OnTopFocusMode, ViewTransform,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::project::ProjectData;
use crate::runtime_log;
use crate::widgets::{HelpHint, WheelSlider};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Sense, Vec2};
use std::collections::HashMap;

const HORIZONTAL_SCROLL_EARLY_FACTOR: f32 = 1.7;

/// Screen-point margin by which the viewport is expanded before testing page
/// visibility. Matches the pre-existing `ui.clip_rect().expand(256.0)` used to
/// build `viewport_rect`, so transform-derived visibility stays pixel-equivalent
/// to the old allocation-based test while decoupling it from the allocation.
const PAGE_VISIBILITY_MARGIN_PX: f32 = 256.0;

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
    /// Solid (non-floating) scrollbar rects from the last frame, when the
    /// corresponding axis overflows (else `None`). Used by tabs to occlude tool
    /// cursors/input over the bars (mirrors egui's bar placement).
    pub(super) scroll_vertical_bar_rect: Option<Rect>,
    pub(super) scroll_horizontal_bar_rect: Option<Rect>,
    pub(super) initial_horizontal_scroll_centered: bool,
    pub(super) pending_zoom_anchor: Option<PendingZoomAnchor>,
    pub(super) pending_scroll_offset: Option<Vec2>,
    /// Deferred `CanvasView::focus_page` request waiting for the target page's world rect (and
    /// its center, when not explicit) to become resolvable. Applied and cleared by
    /// `CanvasView::try_apply_pending_focus` from the draw pass; a newer `focus_page` or
    /// `apply_viewport_snapshot` call replaces/drops it.
    pub(super) pending_focus: Option<PendingPageFocus>,
    pub(super) on_top_hit_rects: HashMap<i64, Rect>,
    pub(super) canvas_left_top_controls_rect: Option<Rect>,
    /// Natural content width (points) of the controls panel, measured last frame from the rows that
    /// do NOT contain the shortcuts chip (header, checkbox, opacity slider). The chip row is
    /// stretched to this width to right-align the chip, so it must be excluded from the measurement:
    /// including it would feed the stretched width back in and inflate the panel by one item-spacing
    /// every frame. Re-measured each frame, so the panel also shrinks again when a shorter locale
    /// narrows those rows. `None` until the panel has been laid out expanded at least once.
    pub(super) controls_content_width: Option<f32>,
    /// Screen rect of the collapsible bottom-center keyboard-shortcut hint drawn last frame, or
    /// `None` when the owning tab supplied no hint. Used to occlude canvas zoom/drag input under
    /// the hint. Reset each frame with the other per-frame scene rects.
    pub(super) canvas_bottom_hint_rect: Option<Rect>,
    /// Fully-expanded size of the bottom-hint body measured on the last frame it was drawn at full
    /// reveal. Drives the height-clip reveal animation so the panel grows/shrinks from the bottom
    /// with the arrow riding upward. `None` until the body has been measured at least once.
    pub(super) bottom_hint_body_size: Option<Vec2>,
    /// World<->screen transform established once per frame from the first laid-out
    /// page's `ScrollArea` geometry and then used to derive every page's authoritative
    /// screen `image_rect`. The `ScrollArea` still allocates/scrolls; this only maps
    /// `world * scale + translation` so a future camera switch keeps the same pixels.
    pub(super) view: ViewTransform,
    /// True once `self.scene.view` has been established for the current frame (so the
    /// per-frame translation is derived from the FIRST page only and reused for the
    /// rest). Reset to `false` at the start of each scene draw.
    pub(super) view_established_this_frame: bool,
    /// Set once after the first transform-vs-layout drift warning so the runtime
    /// drift signal cannot spam the log. Never reset.
    pub(super) view_drift_warned: bool,
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
            scroll_vertical_bar_rect: None,
            scroll_horizontal_bar_rect: None,
            initial_horizontal_scroll_centered: false,
            pending_zoom_anchor: None,
            pending_scroll_offset: None,
            pending_focus: None,
            on_top_hit_rects: HashMap::new(),
            canvas_left_top_controls_rect: None,
            controls_content_width: None,
            canvas_bottom_hint_rect: None,
            bottom_hint_body_size: None,
            view: ViewTransform::default(),
            view_established_this_frame: false,
            view_drift_warned: false,
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

    /// Derives the single per-frame `ViewTransform` translation from one page's
    /// already-allocated screen `image_rect` and its world rect.
    ///
    /// Solves `image_rect.left_top() = world_min * scale + translation` for
    /// `translation`. Because the horizontal centering offset (`image_offset_x ==
    /// world_min.x * scale`) and the vertical strip origin are page-independent, the
    /// translation derived from any single page (in practice the first laid-out one)
    /// reproduces every other page's old `image_rect`. Returned transform mirrors
    /// `self.state.zoom` as its scale.
    #[must_use]
    fn view_from_layout(scale: f32, world_min: Pos2, image_left_top: Pos2) -> ViewTransform {
        let scale_f64 = f64::from(scale);
        let translation = DVec2::new(
            f64::from(image_left_top.x) - f64::from(world_min.x) * scale_f64,
            f64::from(image_left_top.y) - f64::from(world_min.y) * scale_f64,
        );
        ViewTransform::new(scale, translation)
    }

    /// Pure page-visibility predicate: the page is in view when its transform-derived
    /// screen rect intersects `viewport` expanded by `PAGE_VISIBILITY_MARGIN_PX`.
    ///
    /// Decoupled from the `ScrollArea` allocation: visibility is computed from the
    /// `ViewTransform` instead of the allocated row rect, yet stays pixel-equivalent
    /// because `image_rect` is now itself transform-derived.
    #[must_use]
    fn page_in_view(view: &ViewTransform, world_rect: Rect, viewport: Rect) -> bool {
        view.world_rect_to_screen(world_rect)
            .intersects(viewport.expand(PAGE_VISIBILITY_MARGIN_PX))
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
        // Reserve room for one aside column, or two when the second-column mode is enabled so the
        // far column stays reachable via horizontal scroll (ribbon->near, near->far, far->edge gaps
        // plus two equal columns). The actual per-side split still depends on live viewport span.
        let columns = if self.state.aside_second_column {
            2.0
        } else {
            1.0
        };
        let side_space = side_margin * (columns + 1.0) + expanded_aside_width * columns;

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
                && !ctx.egui_wants_keyboard_input(),
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
                drag: if frame.space_pan_drag_enabled
                    && !self.scene.drag_scroll_blocked
                    && !frame.zoom_drag_active
                {
                    egui::scroll_area::DragScroll::Always
                } else {
                    egui::scroll_area::DragScroll::Never
                },
                mouse_wheel: !frame.suppress_wheel_scroll && !self.scene.wheel_scroll_blocked,
            });
        if let Some(offset) = requested_offset {
            scroll_area = scroll_area.scroll_offset(offset);
        }

        let scene_output = scroll_area.show(ui, |ui| {
            self.scene.visible_scene_rect = Some(ui.clip_rect());
            // The page strip is positioned explicitly (edge_margin / page_spacing in world
            // units scaled by zoom); egui's ambient item_spacing must not add incidental
            // gaps between page rows, otherwise screen tops stop being linear in world space
            // and the camera transform (screen = world*scale + translation) can no longer
            // reproduce them. Zero item_spacing only for the strip layout (the add_space +
            // allocate_exact_size cursor below), then restore it before drawing aside/on-top
            // bubbles: those bubbles are positioned by explicit child rects, but their inner
            // widgets inherit this ui's style spacing when no per-bubble scale override
            // applies, so a globally zeroed spacing would collapse intra-bubble layout.
            let saved_item_spacing = ui.spacing().item_spacing;
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
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
            // Unexpanded viewport; `Self::page_in_view` expands it by
            // `PAGE_VISIBILITY_MARGIN_PX` internally (equivalent to the old
            // `clip_rect().expand(256.0)` test).
            let viewport_rect = ui.clip_rect();
            // The per-frame transform must be re-established from this frame's first
            // laid-out page, so clear the previous frame's "established" marker.
            self.scene.view_established_this_frame = false;
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

            // Strip rows are fully allocated; restore the ambient spacing so aside/on-top
            // bubble child UIs (which inherit this ui's style when unscaled) keep their
            // intended intra-bubble layout.
            ui.spacing_mut().item_spacing = saved_item_spacing;

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

            // The transform was already established during page reservation and now
            // drives every page's `image_rect`. Re-validate it here against the first
            // laid-out page for self-consistency (round-trip, clamp, anchor cross-check);
            // this is diagnostic only and never mutates `self.scene.view`.
            if self.scene.view_established_this_frame
                && let Some(first) = reserved_pages.first()
            {
                let idx = first.page_idx;
                let image_rect = first.page_frame.image_rect;
                if let Some(world_rect) = self.scene.page_world_rects.get(idx).copied() {
                    self.warn_on_view_transform_drift(self.scene.view, world_rect, image_rect);
                }
            }

            // Bottom strip margin: zero item_spacing again so egui does not insert an
            // incidental theme gap before this trailing space, keeping the bottom edge
            // margin exactly `edge_margin` (matching the top edge).
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            ui.add_space(edge_margin);
        });
        // Tabs decorate the vertical scrollbar (e.g. translation status marks) after
        // the scene is laid out, so the bar geometry and bubble positions are final.
        self.render_scrollbar_marks(ui, &scene_output, project, hooks);
        scene_output
    }

    /// Shadow validation: checks that the freshly computed `view` reproduces the
    /// laid-out page geometry and is self-consistent, logging at most once on drift.
    ///
    /// This is the only consumer of `self.scene.view` and is purely diagnostic: it
    /// never mutates layout, never panics, and never alters rendering. The `with_anchor`
    /// based `view` and the `ScrollArea` layout must agree (`screen = world * scale +
    /// translation`); a drift would mean a future camera switch would visibly jump, so a
    /// one-shot warning is emitted. The `screen_to_world(world_to_screen(p)) == p`
    /// round-trip guards the inverse, and `clamp_translation` over the page's own world
    /// rect and screen image rect must be a no-op (the page is already within bounds).
    fn warn_on_view_transform_drift(
        &mut self,
        view: ViewTransform,
        world_rect: Rect,
        image_rect: Rect,
    ) {
        if self.scene.view_drift_warned {
            return;
        }
        // Tolerance in screen points; generous so sub-pixel rounding never warns.
        const DRIFT_TOLERANCE_PX: f32 = 1.0;
        let mapped = view.world_rect_to_screen(world_rect);
        let forward_drift = (mapped.min.x - image_rect.min.x)
            .abs()
            .max((mapped.min.y - image_rect.min.y).abs())
            .max((mapped.max.x - image_rect.max.x).abs())
            .max((mapped.max.y - image_rect.max.y).abs());
        let round_trip = view.screen_to_world(view.world_to_screen(world_rect.min));
        let round_trip_drift = (round_trip.x - world_rect.min.x)
            .abs()
            .max((round_trip.y - world_rect.min.y).abs());
        // The page is already laid out inside the strip, so clamping its own world rect
        // against its own screen rect must leave the translation unchanged.
        let clamped = view.clamp_translation(world_rect, image_rect);
        let clamp_drift = ((clamped.translation.x - view.translation.x).abs())
            .max((clamped.translation.y - view.translation.y).abs());
        // `with_anchor(scale, world_min -> screen_min)` must reconstruct the same transform
        // as the direct translation formula used to build `view`; this cross-checks the two
        // construction paths the future camera will rely on.
        let anchored =
            ViewTransform::default().with_anchor(view.scale, world_rect.min, image_rect.left_top());
        let anchor_drift = ((anchored.translation.x - view.translation.x).abs())
            .max((anchored.translation.y - view.translation.y).abs());

        if forward_drift > DRIFT_TOLERANCE_PX
            || round_trip_drift > DRIFT_TOLERANCE_PX
            || clamp_drift > f64::from(DRIFT_TOLERANCE_PX)
            || anchor_drift > f64::from(DRIFT_TOLERANCE_PX)
        {
            self.scene.view_drift_warned = true;
            runtime_log::log_warn(format!(
                "[canvas::scene] shadow ViewTransform drift; forward={forward_drift:.3}px \
                 round_trip={round_trip_drift:.3}px clamp={clamp_drift:.3} \
                 anchor={anchor_drift:.3} scale={:.3}",
                view.scale
            ));
        }
    }

    /// Maximum per-edge drift, in screen points, allowed between the old ad-hoc
    /// `image_rect` and the transform-derived one before the equivalence guard warns.
    const IMAGE_RECT_DRIFT_TOLERANCE_PX: f32 = 0.5;

    /// Returns the largest absolute per-edge difference between two screen rects.
    #[must_use]
    fn max_edge_drift(a: Rect, b: Rect) -> f32 {
        (a.min.x - b.min.x)
            .abs()
            .max((a.min.y - b.min.y).abs())
            .max((a.max.x - b.max.x).abs())
            .max((a.max.y - b.max.y).abs())
    }

    /// Equivalence guard for increment 3: warns at most once if the transform-derived
    /// page `image_rect` drifts from the old ad-hoc rect by more than
    /// `IMAGE_RECT_DRIFT_TOLERANCE_PX` on any edge. Reuses the `view_drift_warned`
    /// once-guard so it can never spam, never panics, and never alters rendering.
    fn warn_on_image_rect_drift(&mut self, old_image_rect: Rect, derived_image_rect: Rect) {
        if self.scene.view_drift_warned {
            return;
        }
        let drift = Self::max_edge_drift(old_image_rect, derived_image_rect);
        if drift > Self::IMAGE_RECT_DRIFT_TOLERANCE_PX {
            self.scene.view_drift_warned = true;
            runtime_log::log_warn(format!(
                "[canvas::scene] ViewTransform image_rect drift {drift:.3}px exceeds \
                 {tol:.3}px; old={old_image_rect:?} derived={derived_image_rect:?}",
                tol = Self::IMAGE_RECT_DRIFT_TOLERANCE_PX
            ));
        }
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
        // The `ScrollArea` stays authoritative for content size, scrollbars, and scroll
        // offset read-back, so the row is allocated exactly as before.
        let (row_rect, response) = ui.allocate_exact_size(row_size, row_sense);
        // Old ad-hoc screen rect: kept to bootstrap the per-frame transform and as the
        // equivalence-guard reference. The transform-derived rect below is authoritative.
        let old_image_rect = Rect::from_min_size(
            egui::pos2(row_rect.left() + image_left_offset, row_rect.top()),
            image_size,
        );

        if page_idx >= self.scene.page_world_rects.len() {
            self.scene
                .page_world_rects
                .resize(page_idx + 1, Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        }
        // World rect is computed exactly as before (content-centered, unscaled strip).
        let world_rect = Rect::from_min_size(
            egui::pos2(
                ((content_world_width - page_size_px.x) * 0.5).max(0.0),
                page_world_top,
            ),
            page_size_px,
        );
        self.scene.page_world_rects[page_idx] = world_rect;

        // Establish the single per-frame transform from the FIRST laid-out page's old
        // geometry, then map every page (including this first one) through it. Because
        // the horizontal centering offset equals `world_min.x * scale` and the vertical
        // strip origin is page-independent, this one translation reproduces every page's
        // old `image_rect`.
        if !self.scene.view_established_this_frame {
            self.scene.view =
                Self::view_from_layout(self.state.zoom, world_rect.min, old_image_rect.left_top());
            self.scene.view_established_this_frame = true;
        }
        let image_rect = self.scene.view.world_rect_to_screen(world_rect);
        // Equivalence guard: warn at most once if the transform-derived rect drifts from
        // the old ad-hoc rect by more than half a pixel on any edge. Diagnostic only; the
        // transform-derived rect is the one actually used.
        self.warn_on_image_rect_drift(old_image_rect, image_rect);

        if page_idx >= self.scene.page_rects.len() {
            self.scene
                .page_rects
                .resize(page_idx + 1, Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        }
        self.scene.page_rects[page_idx] = image_rect;
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
                page_in_view: Self::page_in_view(&self.scene.view, world_rect, viewport_rect),
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
            // `scale` is points-per-source-pixel; it mirrors `self.state.zoom` but is read
            // from the authoritative transform so tile placement matches `image_rect`.
            let scale = self.scene.view.scale;
            for (tile_idx, tile) in page_texture.tiles.iter_mut().enumerate() {
                let tile_rect = Rect::from_min_size(
                    egui::pos2(
                        page_frame.image_rect.left() + tile.origin_px.x * scale,
                        page_frame.image_rect.top() + tile.origin_px.y * scale,
                    ),
                    tile.size_px * scale,
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

        // The pixel grid is drawn in a single late overlay pass
        // (`draw_visible_pixel_grid_overlay`) so it sits above source, overlay,
        // and mask layers; it is intentionally not drawn here.
    }

    /// Draws the per-pixel inspection grid over `image_rect`. The grid is gated
    /// on the same DPI-correct magnification notion (`zoom * pixels_per_point`)
    /// and threshold as NEAREST sampling, so both switch on together.
    fn draw_pixel_grid_on_page(&self, ui: &mut egui::Ui, image_rect: Rect) {
        // Points-per-source-pixel from the authoritative transform (numerically equal to
        // `self.state.zoom`); keeps grid alignment consistent with the painted tiles.
        let zoom = self.scene.view.scale;
        // Gate on the UNCLAMPED ppp so the grid switches on together with NEAREST sampling
        // (which also uses the raw `ctx.pixels_per_point()`); clamp only the alignment math
        // below, where a sub-1 ppp would distort the pixel-snapping arithmetic.
        let real_pixels_per_point = ui.ctx().pixels_per_point();
        let pixels_per_point = real_pixels_per_point.max(1.0);
        if !super::pixel_inspection_recommended_for(zoom, real_pixels_per_point)
            || !image_rect.is_positive()
        {
            return;
        }
        let clip_rect = ui.clip_rect().intersect(image_rect);
        if !clip_rect.is_positive() {
            return;
        }

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

        // Single-pass bucketing: bucket this page's runtime bubbles once instead of two
        // independent `page_bubbles` scans (aside Left, aside Right). The owned buckets borrow
        // nothing from `self`, so we move their Vecs out before the `&mut self` draw call below.
        let buckets = self.page_bubbles_bucketed(page_frame.page_idx);
        let aside_left_items = buckets.aside_left;
        let aside_right_items = buckets.aside_right;
        bubble_aside_ui::draw_aside_for_page(
            self,
            ui,
            project,
            page_frame.page_idx,
            page_frame.row_rect,
            page_frame.image_rect,
            aside_left_items,
            aside_right_items,
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

        // Single-pass bucketing: bucket this page's runtime bubbles once instead of two
        // independent `page_bubbles` scans (on-top Left, on-top Right). On-top bubbles are always
        // text bubbles, so each item maps to a plain bubble id; preserve Left-then-Right order.
        let buckets = self.page_bubbles_bucketed(page_frame.page_idx);
        let on_top_bubble_ids: Vec<i64> = buckets
            .on_top_left
            .into_iter()
            .chain(buckets.on_top_right)
            .map(|item| item.bid)
            .collect();
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
                    .add_enabled(self.editable, egui::Button::new(t!("canvas.page_menu.create_image_bubble")))
                    .clicked()
                {
                    if !self.create_image_bubble_from_canvas_context_menu(ui.ctx(), project) {
                        runtime_log::log_warn(format!(
                            "[canvas::scene] failed to create image bubble from context menu; page_idx={}",
                            page_frame.page_idx
                        ));
                    }
                    self.bubble_runtime.canvas_context_menu_target = None;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.editable && self.bubble_runtime.copied_bubble_data.is_some(),
                        egui::Button::new(t!("canvas.page_menu.paste_bubble")),
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
                        egui::Button::new(t!("canvas.page_menu.paste_new_bubble_original")),
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
                        egui::Button::new(t!("canvas.page_menu.paste_new_bubble_translation")),
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
        // Draws the controls panel; the canvas-shortcuts hover chip is rendered inline on its zoom
        // row (see `draw_canvas_shortcuts_chip`), not as a separate floating box.
        self.draw_canvas_controls(ctx, frame.canvas_rect, project.pages.len());
        // Drawn right after the controls: the horizontal scrollbar rect is already cached (see
        // `cache_scrollbar_rects`), so the hint can anchor its bottom just above the bar.
        self.draw_canvas_bottom_hint(ctx, &frame);
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

        // Width the shortcuts chip is right-aligned against: the panel's natural content width as
        // measured last frame from the rows WITHOUT the chip. See `controls_content_width`.
        let target_width = self.scene.controls_content_width;
        // Max width of this frame's chip-free rows, accumulated below.
        let mut natural_width: f32 = 0.0;
        let controls_area = egui::Area::new("canvas_left_top_controls".into())
            .movable(true)
            .default_pos(canvas_rect.left_top() + egui::vec2(12.0, 12.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let toggle_hint = if self.state.controls_panel_collapsed {
                        t!("canvas.controls.expand_panel_tooltip")
                    } else {
                        t!("canvas.controls.collapse_panel_tooltip")
                    };
                    let toggle_icon = if self.state.controls_panel_collapsed {
                        "▶"
                    } else {
                        "▼"
                    };
                    let header = ui.horizontal(|ui| {
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
                    natural_width = natural_width.max(header.response.rect.width());
                    if self.state.controls_panel_collapsed {
                        return;
                    }
                    ui.add_space(2.0);
                    Self::draw_zoom_and_shortcuts_row(ui, &zoom_text, target_width);
                    ui.add_space(4.0);
                    let show_bubbles = ui
                        .checkbox(&mut self.state.show_bubbles, t!("canvas.controls.show_bubbles"));
                    natural_width = natural_width.max(show_bubbles.rect.width());
                    let opacity = ui.add(
                        WheelSlider::new(&mut self.state.bubble_opacity, 0.0..=1.0)
                            .text(t!("canvas.controls.bubble_opacity")),
                    );
                    natural_width = natural_width.max(opacity.rect.width());
                });
            });
        self.scene.canvas_left_top_controls_rect = Some(controls_area.response.rect);
        // Collapsed, the width-defining rows are not laid out, so keep the last expanded value for
        // when the panel opens again rather than collapsing the target to the header's width.
        if !self.state.controls_panel_collapsed {
            self.scene.controls_content_width = Some(natural_width);
        }
    }

    /// Draws the zoom label with the canvas-shortcuts hover chip right-aligned on the same row.
    ///
    /// `target_width` is the panel's natural content width (measured from the chip-free rows). The
    /// row is allocated exactly that wide so `Sides` right-aligns the chip to the panel's right
    /// edge: `Sides` (like a bare `right_to_left` layout) otherwise expands to the parent's full
    /// available width, which in the auto-sized `Area` is the whole screen and would stretch the
    /// panel. `None` — the first expanded frame, before any measurement — falls back to the
    /// natural, unstretched layout; the width captured that frame fixes the alignment on the next.
    fn draw_zoom_and_shortcuts_row(ui: &mut egui::Ui, zoom_text: &str, target_width: Option<f32>) {
        let sides = egui::containers::Sides::new();
        let add_zoom = |ui: &mut egui::Ui| {
            ui.label(zoom_text);
        };
        match target_width {
            Some(width) => {
                let height = ui.spacing().interact_size.y;
                ui.allocate_ui_with_layout(
                    egui::vec2(width, height),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| sides.show(ui, add_zoom, Self::draw_canvas_shortcuts_chip),
                );
            }
            None => {
                sides.show(ui, add_zoom, Self::draw_canvas_shortcuts_chip);
            }
        }
    }

    /// Draws the collapsible bottom-center keyboard-shortcut hint for the current frame.
    ///
    /// Does nothing when the owning tab supplied no hint (`bottom_hint == None`). The hint is
    /// bottom-pinned just above the horizontal scrollbar (or the inner viewport bottom when no bar
    /// is drawn) and never overlaps the bar. Collapsed shows only an up-arrow toggle; expanded
    /// slides a popup panel up from the anchor line with the arrow riding above it. Clicking the
    /// arrow toggles `bottom_hint_collapsed`; the reveal is animated via `animate_bool_responsive`.
    /// Stores the drawn rect in `scene.canvas_bottom_hint_rect` for input occlusion.
    pub(super) fn draw_canvas_bottom_hint(&mut self, ctx: &egui::Context, frame: &CanvasFrameParams) {
        // Gap in logical points between the hint's bottom edge and the scrollbar / viewport bottom.
        const GAP: f32 = 6.0;

        let Some(hint) = self.bottom_hint.as_ref() else {
            return;
        };
        let rows = hint.rows.clone();

        // Bottom edge sits just above the horizontal scrollbar when present, else just above the
        // inner viewport bottom; both are last-frame geometry cached before this pass runs.
        let bottom_ref = self
            .scene
            .scroll_horizontal_bar_rect
            .map(|r| r.top())
            .or_else(|| self.scene.scroll_inner_rect.map(|r| r.bottom()))
            .unwrap_or(frame.canvas_rect.bottom());
        let anchor_y = bottom_ref - GAP;
        let center_x = frame.canvas_rect.center().x;

        let mut collapsed = self.bottom_hint_collapsed;
        let expanded = !collapsed;
        let reveal =
            ctx.animate_bool_responsive(egui::Id::new("canvas_bottom_hint_reveal"), expanded);
        let cached_size = self.scene.bottom_hint_body_size;
        // Body size measured this frame at (near-)full reveal, written back after the Area closes.
        let mut measured_body_size: Option<Vec2> = None;

        let area = egui::Area::new(egui::Id::new("canvas_bottom_hint"))
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(egui::pos2(center_x, anchor_y))
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    // Arrow toggle sits above the body (top-down layout), so with the CENTER_BOTTOM
                    // pivot the body's bottom stays at `anchor_y` and the arrow rides upward.
                    let (icon, tooltip) = if collapsed {
                        ("\u{25B2}", t!("canvas.bottom_hint.expand_tooltip"))
                    } else {
                        ("\u{25BC}", t!("canvas.bottom_hint.collapse_tooltip"))
                    };
                    if ui.small_button(icon).on_hover_text(tooltip).clicked() {
                        collapsed = !collapsed;
                    }

                    if reveal <= 0.001 {
                        return;
                    }
                    ui.add_space(2.0);
                    // Only clip to a partial bottom slice while mid-animation AND a full size has
                    // already been measured; otherwise draw the body at its natural size and record
                    // the size for the next animation.
                    let partial = if reveal < 0.999 { cached_size } else { None };
                    if let Some(full) = partial {
                        // Reserve a growing bottom slice and draw the full body clipped to it,
                        // bottom-aligned, so the panel slides up from behind the anchor line.
                        let revealed_h = (full.y * reveal).max(0.0);
                        let (slot, _resp) =
                            ui.allocate_exact_size(egui::vec2(full.x, revealed_h), Sense::hover());
                        let body_rect =
                            Rect::from_min_size(egui::pos2(slot.left(), slot.bottom() - full.y), full);
                        let mut body_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(body_rect)
                                .layout(egui::Layout::top_down(egui::Align::Center)),
                        );
                        body_ui.set_clip_rect(slot);
                        Self::draw_bottom_hint_body(&mut body_ui, &rows);
                    } else {
                        let size = Self::draw_bottom_hint_body(ui, &rows).rect.size();
                        measured_body_size = Some(size);
                    }
                });
            });

        if let Some(size) = measured_body_size {
            self.scene.bottom_hint_body_size = Some(size);
        }
        self.bottom_hint_collapsed = collapsed;
        self.scene.canvas_bottom_hint_rect = Some(area.response.rect);
    }

    /// Renders the bottom-hint popup body (title + shortcut rows) into `ui` and returns its frame
    /// response. Each row shows its label left and monospaced `keys` right, with an optional "?"
    /// help icon between them; a row with empty `keys` renders as a full-width informational line.
    /// Unlike the shortcuts chip, this body is a real panel, so its rows may carry `help`.
    /// Stateless: no `self` access.
    fn draw_bottom_hint_body(ui: &mut egui::Ui, rows: &[CanvasHintRow]) -> egui::Response {
        egui::Frame::popup(ui.style())
            .show(ui, |ui| {
                ui.label(egui::RichText::new(t!("canvas.bottom_hint.title")).strong());
                ui.add_space(2.0);
                Self::draw_hint_rows_grid(ui, "canvas_bottom_hint_grid", rows);
            })
            .response
    }

    /// Number of grid columns `rows` needs: 3 when any row carries help (label / "?" / keys),
    /// otherwise 2 (label / keys). Pure; the extra column exists only when something fills it.
    #[must_use]
    fn hint_grid_columns(rows: &[CanvasHintRow]) -> usize {
        if rows.iter().any(|row| row.help.is_some()) {
            3
        } else {
            2
        }
    }

    /// Builds the `HelpHint` widget for one row's typed help payload.
    fn hint_help_widget(help: &CanvasHintHelp) -> HelpHint {
        match help {
            CanvasHintHelp::Text(text) => HelpHint::text(text.clone()),
            CanvasHintHelp::Animation(hint) => HelpHint::animated(*hint),
            CanvasHintHelp::TextAndAnimation { text, animation } => {
                HelpHint::animated(*animation).with_text(text.clone())
            }
        }
    }

    /// Renders shortcut rows as a grid: `label` left, monospaced `keys` right. A row with empty
    /// `keys` renders as a full-width label. `grid_id` must be unique per live call site.
    ///
    /// When at least one row carries `help`, the grid gains a third column between label and keys
    /// holding that row's "?" icon (`Self::hint_grid_columns`); rows without help allocate an empty
    /// cell there so `keys` cannot slide into the help column. `egui::Grid` hard-codes cell
    /// alignment to `Align2::LEFT_CENTER` (`egui-0.35.0/src/grid.rs:189-195`) and `Ui::add_space` is
    /// debug-asserted against inside a grid (`egui-0.35.0/src/placer.rs:150-156`), so the empty cell
    /// is a zero-size `Ui::allocate_space`, which advances the cell cursor like any widget.
    ///
    /// # Warnings
    /// - **`help` does not work inside a tooltip.** The icon reveals its payload through its own
    ///   hover tooltip, and egui cannot open a tooltip inside a tooltip. Call sites that render this
    ///   grid within `on_hover_ui` (see `Self::canvas_shortcut_rows`) must keep every `help` at
    ///   `None`.
    /// - **`min_col_width(0.0)` assumes wide label/keys.** `Grid::min_col_width` is global to all
    ///   columns (`egui-0.35.0/src/grid.rs:372-378`) and defaults to `Spacing::interact_size.x`
    ///   (~40pt), which would inflate the ~14pt icon column. Dropping it to 0 is therefore required
    ///   in three-column mode, and is only visually neutral while the label and keys columns are
    ///   wider than 40pt by content; shorter content would render narrower than in two-column mode.
    fn draw_hint_rows_grid(ui: &mut egui::Ui, grid_id: &str, rows: &[CanvasHintRow]) {
        let columns = Self::hint_grid_columns(rows);
        let has_help = columns > 2;
        let mut grid = egui::Grid::new(grid_id)
            .num_columns(columns)
            .spacing(egui::vec2(16.0, 2.0));
        if has_help {
            grid = grid.min_col_width(0.0);
        }
        grid.show(ui, |ui| {
            for row in rows {
                ui.label(&row.label);
                if has_help {
                    match row.help.as_ref() {
                        Some(help) => {
                            Self::hint_help_widget(help).show(ui);
                        }
                        // Keep the keys in their own column: an empty cell still has to be
                        // allocated for the grid cursor to advance past the help column.
                        None => {
                            ui.allocate_space(Vec2::ZERO);
                        }
                    }
                }
                if !row.keys.is_empty() {
                    ui.monospace(&row.keys);
                }
                ui.end_row();
            }
        });
    }

    /// Fixed, localized list of the canvas's own navigation shortcuts (zoom, pan, scroll) shown by
    /// the canvas-shortcuts hover chip. These are canvas-intrinsic behaviours, identical across all
    /// tabs; rebuilt each call so a runtime language switch is reflected.
    ///
    /// # Warning
    /// These rows must never carry `help`: the chip renders them inside its hover tooltip
    /// (`Self::draw_canvas_shortcuts_chip`), and a "?" icon there could not open its own tooltip.
    fn canvas_shortcut_rows() -> Vec<CanvasHintRow> {
        vec![
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.zoom_cursor_label"),
                t!("canvas.shortcuts_hint.zoom_cursor_keys"),
            ),
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.zoom_in_out_label"),
                t!("canvas.shortcuts_hint.zoom_in_out_keys"),
            ),
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.zoom_reset_label"),
                t!("canvas.shortcuts_hint.zoom_reset_keys"),
            ),
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.pan_label"),
                t!("canvas.shortcuts_hint.pan_keys"),
            ),
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.scroll_label"),
                t!("canvas.shortcuts_hint.scroll_keys"),
            ),
            CanvasHintRow::new(
                t!("canvas.shortcuts_hint.scroll_horizontal_label"),
                t!("canvas.shortcuts_hint.scroll_horizontal_keys"),
            ),
        ]
    }

    /// Renders the canvas-shortcuts hover chip inline into `ui`: a subtle, non-interactive weak
    /// label (`canvas.shortcuts_hint.title`) that reveals the canvas's own navigation shortcuts
    /// (zoom, pan, scroll) as a tooltip on hover. Drawn on the zoom row of the controls panel
    /// (see `draw_canvas_controls`); hover-only, so it takes no click and has no persisted state.
    fn draw_canvas_shortcuts_chip(ui: &mut egui::Ui) {
        let rows = Self::canvas_shortcut_rows();
        let label = egui::Label::new(egui::RichText::new(t!("canvas.shortcuts_hint.title")).weak())
            // Keep the chip on a single line; the panel is sized by the wider rows below, so it fits.
            .wrap_mode(egui::TextWrapMode::Extend)
            .sense(egui::Sense::hover());
        ui.add(label).on_hover_ui(|ui| {
            Self::draw_hint_rows_grid(ui, "canvas_shortcuts_hint_grid", &rows);
        });
    }

    fn create_bubble_context_menu_label(&self) -> String {
        match self.create_bubble_shortcut_hint.as_deref() {
            Some(shortcut) if !shortcut.is_empty() => tf!("canvas.page_menu.create_bubble_shortcut", shortcut = shortcut),
            _ => t!("canvas.page_menu.create_bubble").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `CanvasView` with the scene state needed by the pure scroll/zoom
    /// helpers, without touching any live egui `Ui`/`Context`.
    fn view_with_layout(zoom: f32, content_world_width: f32, viewport: Rect) -> CanvasView {
        let mut view = CanvasView::default();
        view.state.zoom = zoom;
        view.scene.content_world_width = content_world_width;
        view.scene.scroll_inner_rect = Some(viewport);
        view
    }

    #[test]
    fn zoom_anchor_round_trip_reproduces_offset_across_scales() {
        // Contract: an anchor captured by `capture_pending_zoom_anchor` at the
        // current zoom reproduces the same scroll offset via
        // `scroll_offset_for_zoom_anchor` when zoom is unchanged (the round trip the
        // directed-zoom path relies on). Tested for the clamp endpoints and 1.0.
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0));
        let content_world_width = 1400.0;
        for scale in [0.2_f32, 1.0, 5.0] {
            let mut view = view_with_layout(scale, content_world_width, viewport);
            // A scroll offset inside the valid range for this zoom.
            let max_x = view.max_scroll_offset_x_for_viewport(viewport.width());
            let start_offset = egui::vec2(max_x * 0.5, 120.0);
            view.scene.scroll_offset = start_offset;

            // Capture an anchor at an interior viewport point.
            let anchor_pos = Some(egui::pos2(640.0, 300.0));
            view.capture_pending_zoom_anchor(anchor_pos, viewport);
            let anchor = view
                .scene
                .pending_zoom_anchor
                .expect("anchor must be captured for a positive viewport");

            // With zoom unchanged, the reconstructed offset must match the (clamped)
            // starting offset.
            let reproduced = view.scroll_offset_for_zoom_anchor(anchor);
            let expected_x = start_offset.x.clamp(0.0, max_x);
            assert!(
                (reproduced.x - expected_x).abs() <= 1e-2,
                "scale {scale}: x {reproduced:?} != {expected_x}"
            );
            assert!(
                (reproduced.y - start_offset.y).abs() <= 1e-2,
                "scale {scale}: y {reproduced:?} != {start_offset:?}"
            );
        }
    }

    #[test]
    fn max_scroll_offset_x_is_monotonic_non_decreasing_in_zoom() {
        // Contract: for a fixed viewport and content, the horizontal scroll range
        // never shrinks as zoom increases.
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0));
        let content_world_width = 1400.0;
        let mut prev = f32::NEG_INFINITY;
        for step in 0..=48 {
            let zoom = 0.2 + (step as f32) * (5.0 - 0.2) / 48.0;
            let view = view_with_layout(zoom, content_world_width, viewport);
            let max_x = view.max_scroll_offset_x_for_viewport(viewport.width());
            assert!(
                max_x + 1e-3 >= prev,
                "max scroll x decreased at zoom {zoom}: {max_x} < {prev}"
            );
            prev = max_x;
        }
    }

    #[test]
    fn page_world_rect_is_horizontally_centered_in_content_width() {
        // Contract: `reserve_canvas_page_frame` writes a `page_world_rect` whose
        // left/right margins inside `content_world_width` are equal (content-centered
        // world strip). This complements `centered_scroll_keeps_pages_centered`, which
        // pins the screen-space centering.
        let content_world_width = 1400.0_f32;
        for page_width in [1400.0_f32, 900.0, 320.0] {
            let left = ((content_world_width - page_width) * 0.5).max(0.0);
            let world_rect =
                Rect::from_min_size(egui::pos2(left, 50.0), egui::vec2(page_width, 1000.0));
            let right_margin = content_world_width - world_rect.max.x;
            assert!(
                (world_rect.min.x - right_margin).abs() <= f32::EPSILON,
                "page width {page_width} not centered in content width"
            );
        }
    }

    #[test]
    fn view_from_layout_reproduces_every_page_old_image_rect() {
        // Contract for increment 3: the single per-frame transform, derived from the
        // FIRST page's old ad-hoc geometry, reproduces every other page's old
        // `image_rect` within the 0.5px equivalence tolerance. This mirrors
        // `reserve_canvas_page_frame`'s layout math for a multi-page strip.
        //
        // This test pins the "linear strip, no item_spacing" contract: `row_top` is modeled
        // as a purely linear function of `page_world_top` (strip_top + (world_top -
        // first_world_top) * zoom), with NO incidental inter-row gap. That matches the
        // strip layout only because `draw_canvas_scene` zeroes the ambient
        // `item_spacing` before allocating page rows. If a non-zero inter-row gap were
        // reintroduced, the real `row_top` would drift from this linear model by
        // accumulated spacing per page and the transform could no longer reproduce it,
        // so this assertion would fail.
        let viewport_width = 1000.0_f32;
        let content_world_width = 1400.0_f32;
        for zoom in [0.2_f32, 1.0, 2.5, 5.0] {
            let content_screen_width = content_world_width * zoom;
            // Page sizes in source pixels and their world top offsets down the strip.
            let pages: [(Vec2, f32); 3] = [
                (egui::vec2(1400.0, 1000.0), 17.0),
                (egui::vec2(900.0, 1300.0), 1017.0),
                (egui::vec2(320.0, 480.0), 2317.0),
            ];
            // A fixed scroll-content left/top origin (where `row_rect` is allocated).
            let strip_left = 8.0_f32;
            let strip_top = 17.0_f32 * zoom; // matches edge_margin_world * zoom

            let mut view: Option<ViewTransform> = None;
            for (page_size_px, page_world_top) in pages {
                let image_size = page_size_px * zoom;
                let (_row_w, image_left_offset) = CanvasView::canvas_page_x_layout(
                    viewport_width,
                    content_screen_width,
                    image_size.x,
                );
                // The row top advances with the world top (same accumulation as layout),
                // so `row_rect.top == strip_top - edge_margin_top + page_world_top*zoom`.
                // Using a constant strip origin: row_top = strip_top + (page_world_top -
                // first_world_top) * zoom. Express it directly relative to world.
                let row_top = strip_top + (page_world_top - pages[0].1) * zoom;
                let old_image_rect = Rect::from_min_size(
                    egui::pos2(strip_left + image_left_offset, row_top),
                    image_size,
                );
                let world_rect = Rect::from_min_size(
                    egui::pos2(
                        ((content_world_width - page_size_px.x) * 0.5).max(0.0),
                        page_world_top,
                    ),
                    page_size_px,
                );
                let v = *view.get_or_insert_with(|| {
                    CanvasView::view_from_layout(zoom, world_rect.min, old_image_rect.left_top())
                });
                let derived = v.world_rect_to_screen(world_rect);
                let drift = CanvasView::max_edge_drift(old_image_rect, derived);
                assert!(
                    drift <= CanvasView::IMAGE_RECT_DRIFT_TOLERANCE_PX,
                    "zoom {zoom}: page (size {page_size_px:?}) drift {drift} exceeds tolerance; \
                     old={old_image_rect:?} derived={derived:?}"
                );
            }
        }
    }

    #[test]
    fn reintroduced_inter_row_spacing_breaks_linear_strip_contract() {
        // Negative control for the "linear strip, no item_spacing" contract pinned by
        // `view_from_layout_reproduces_every_page_old_image_rect`. If `draw_canvas_scene`
        // stopped zeroing the ambient `item_spacing`, every page after the first would have
        // its `row_top` shifted down by `item_spacing.y * page_index`, so the single
        // per-frame `ViewTransform` (anchored on the first page) could no longer reproduce
        // the shifted rects. This test simulates that drift and asserts it is detected,
        // proving the contract test is sensitive to a reintroduced inter-row gap rather than
        // passing vacuously.
        let viewport_width = 1000.0_f32;
        let content_world_width = 1400.0_f32;
        // A representative ambient theme spacing (the app theme uses 12.0); any non-zero
        // value reproduces the regression this guards against.
        const SIMULATED_ITEM_SPACING_Y: f32 = 12.0;
        let zoom = 1.0_f32;
        let content_screen_width = content_world_width * zoom;
        let pages: [(Vec2, f32); 3] = [
            (egui::vec2(1400.0, 1000.0), 17.0),
            (egui::vec2(900.0, 1300.0), 1017.0),
            (egui::vec2(320.0, 480.0), 2317.0),
        ];
        let strip_left = 8.0_f32;
        let strip_top = 17.0_f32 * zoom;

        let mut view: Option<ViewTransform> = None;
        let mut max_drift = 0.0_f32;
        for (page_index, (page_size_px, page_world_top)) in pages.into_iter().enumerate() {
            let image_size = page_size_px * zoom;
            let (_row_w, image_left_offset) = CanvasView::canvas_page_x_layout(
                viewport_width,
                content_screen_width,
                image_size.x,
            );
            // Inject the incidental spacing egui would insert between consecutive rows: it
            // accumulates linearly with the page index (none before the first row).
            let injected_gap = SIMULATED_ITEM_SPACING_Y * page_index as f32;
            let row_top = strip_top + (page_world_top - pages[0].1) * zoom + injected_gap;
            let old_image_rect = Rect::from_min_size(
                egui::pos2(strip_left + image_left_offset, row_top),
                image_size,
            );
            let world_rect = Rect::from_min_size(
                egui::pos2(
                    ((content_world_width - page_size_px.x) * 0.5).max(0.0),
                    page_world_top,
                ),
                page_size_px,
            );
            // Anchor the transform on the first page (gap == 0), exactly as the runtime does.
            let v = *view.get_or_insert_with(|| {
                CanvasView::view_from_layout(zoom, world_rect.min, old_image_rect.left_top())
            });
            let derived = v.world_rect_to_screen(world_rect);
            max_drift = max_drift.max(CanvasView::max_edge_drift(old_image_rect, derived));
        }
        assert!(
            max_drift > CanvasView::IMAGE_RECT_DRIFT_TOLERANCE_PX,
            "a reintroduced {SIMULATED_ITEM_SPACING_Y}px inter-row gap must break the linear \
             strip contract, but max drift was only {max_drift}px"
        );
    }

    #[test]
    fn page_in_view_matches_old_intersection_test() {
        // Contract for increment 4: the transform-based predicate equals the old
        // `image_rect.intersects(clip.expand(256))` test, because `image_rect` is now
        // transform-derived. Exercise pages fully inside, just outside the margin, and
        // far away.
        let view = ViewTransform::new(1.5, DVec2::new(20.0, -10.0));
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0));
        let cases = [
            Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(300.0, 400.0)), // inside
            Rect::from_min_size(egui::pos2(-50.0, -50.0), egui::vec2(80.0, 80.0)),   // near edge
            Rect::from_min_size(egui::pos2(5000.0, 5000.0), egui::vec2(200.0, 200.0)), // far
        ];
        for world_rect in cases {
            let image_rect = view.world_rect_to_screen(world_rect);
            let expected = image_rect.intersects(viewport.expand(PAGE_VISIBILITY_MARGIN_PX));
            let got = CanvasView::page_in_view(&view, world_rect, viewport);
            assert_eq!(
                got, expected,
                "visibility mismatch for {world_rect:?}: got {got}, expected {expected}"
            );
        }
    }

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

    #[test]
    fn bottom_hint_state_defaults_and_seeding_contract() {
        // Contract: the bottom hint starts absent and expanded; `set_bottom_hint` toggles content
        // presence without touching collapsed state, and `set_bottom_hint_collapsed` seeds the
        // persisted collapsed value read back by `bottom_hint_collapsed`.
        use super::super::CanvasBottomHint;
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let mut view = view_with_layout(1.0, 1000.0, viewport);

        assert!(view.bottom_hint.is_none(), "hint content defaults to None");
        assert!(!view.bottom_hint_collapsed(), "default state is expanded");

        view.set_bottom_hint(Some(CanvasBottomHint {
            rows: vec![CanvasHintRow::new("Save", "Ctrl+S")],
        }));
        assert!(view.bottom_hint.is_some(), "content is stored");
        assert!(
            !view.bottom_hint_collapsed(),
            "set_bottom_hint must not change collapsed state"
        );

        view.set_bottom_hint_collapsed(true);
        assert!(view.bottom_hint_collapsed(), "collapsed state is seeded");

        view.set_bottom_hint(None);
        assert!(view.bottom_hint.is_none(), "None hides the hint content");
        assert!(
            view.bottom_hint_collapsed(),
            "hiding content leaves collapsed state untouched"
        );
    }

    #[test]
    fn bottom_hint_row_builder_controls_help_presence() {
        // Contract: `CanvasHintRow::new` never attaches help, and `with_help` is the only way to
        // attach it. Labels and keys pass through unchanged.
        let plain = CanvasHintRow::new("Rotate", "Ctrl+Wheel");
        assert_eq!(plain.label, "Rotate");
        assert_eq!(plain.keys, "Ctrl+Wheel");
        assert!(plain.help.is_none(), "a fresh row carries no help");

        let helped = CanvasHintRow::new("Rotate", "Ctrl+Wheel")
            .with_help(CanvasHintHelp::Text("Rotates the selection".to_string()));
        assert_eq!(helped.label, "Rotate", "with_help must not touch the label");
        assert_eq!(helped.keys, "Ctrl+Wheel", "with_help must not touch the keys");
        assert!(
            matches!(helped.help, Some(CanvasHintHelp::Text(ref text)) if text == "Rotates the selection"),
            "with_help stores the payload verbatim"
        );
    }

    #[test]
    fn bottom_hint_grid_columns_follow_help_presence() {
        // Contract: the help column exists only when a row can fill it, so the help-free grid
        // (which the shortcuts chip also renders) keeps its exact two-column layout.
        assert_eq!(
            CanvasView::hint_grid_columns(&[]),
            2,
            "an empty row set needs no help column"
        );
        assert_eq!(
            CanvasView::hint_grid_columns(&[
                CanvasHintRow::new("Zoom", "Ctrl+Wheel"),
                CanvasHintRow::new("Pan", "Space+Drag"),
            ]),
            2,
            "rows without help stay two-column"
        );
        assert_eq!(
            CanvasView::hint_grid_columns(&[
                CanvasHintRow::new("Zoom", "Ctrl+Wheel"),
                CanvasHintRow::new("Pan", "Space+Drag")
                    .with_help(CanvasHintHelp::Text("Hold space".to_string())),
            ]),
            3,
            "a single helped row switches the whole grid to three columns"
        );
    }

    #[test]
    fn canvas_shortcut_rows_carry_no_help() {
        // Contract: the shortcuts chip renders these rows inside its hover tooltip, where a "?"
        // icon could not open its own tooltip. Every row must stay help-free.
        assert!(
            CanvasView::canvas_shortcut_rows()
                .iter()
                .all(|row| row.help.is_none()),
            "shortcut chip rows must not carry help"
        );
    }
}
