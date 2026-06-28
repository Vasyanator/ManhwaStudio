/*
File: tabs/ps_editor/tools/deform.rs

Purpose:
Mesh-deform tool for the PS-like editor. It gives a raster layer the same warp mechanism text
layers already have: a `cols`×`rows` grid of control points (absolute page px) that the renderer
maps the image through (see `layer_render::TiledTexture::draw_deform`). Entering the tool on a
raster with no deform initializes an identity grid spanning its current affine footprint, so the
placement is unchanged until the user drags a handle. Base layers are locked and ignored.

Key structures:
- `DeformTool`: the active grid-point drag plus a per-frame cache of handle positions for the
  overlay.

Notes:
This is the basic grid-point drag path (Phase 5 scope): each handle moves a single control point.
The richer perspective / bend / sampled-edge handle modes from the typing tab are not wired here;
this delivers a usable, model-consistent raster deform that round-trips through the doc and disk.
*/

use super::{PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::models::layer_model::manifest::DeformRec;
use crate::tabs::ps_editor::viewport::ViewTransform;
use eframe::egui;
use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};

/// Default control-grid resolution when a raster first enters deform mode. A 3×3 grid gives corner
/// + edge-midpoint + center handles — enough for perspective and a gentle bend without a dense mesh.
const DEFAULT_COLS: usize = 3;
const DEFAULT_ROWS: usize = 3;
/// Screen-space half-size of a drawn handle square.
const HANDLE_HALF_PX: f32 = 4.0;
/// Screen-space radius within which a handle is grabbed.
const HANDLE_HIT_PX: f32 = 11.0;

/// In-progress grid-point drag: the index of the control point and where it started (page px).
#[derive(Debug, Clone, Copy)]
struct PointDrag {
    point_idx: usize,
    start_point: Pos2,
    start_pointer: Pos2,
}

/// Mesh-deform tool operating on the active raster layer's `deform` grid.
#[derive(Debug, Clone, Default)]
pub struct DeformTool {
    drag: Option<PointDrag>,
    /// Control points (page px) cached this frame for overlay drawing.
    handles: Vec<Pos2>,
    /// (cols, rows) of `handles` this frame, so the overlay can draw grid lines.
    grid_dims: Option<(usize, usize)>,
}

impl DeformTool {
    /// Index of the control point whose screen position is within grab range of `pointer`, if any.
    fn hit_test(&self, view: &ViewTransform, points: &[Pos2], pointer: Pos2) -> Option<usize> {
        let screen = view.world_to_screen(pointer);
        let mut best: Option<(usize, f32)> = None;
        for (i, &p) in points.iter().enumerate() {
            let d = screen.distance(view.world_to_screen(p));
            if d <= HANDLE_HIT_PX && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }
}

/// Reads a deform grid's control points as `Pos2` (page px).
fn grid_points(grid: &DeformRec) -> Vec<Pos2> {
    grid.points_px.iter().map(|p| Pos2::new(p[0], p[1])).collect()
}

impl PsTool for DeformTool {
    fn id(&self) -> PsToolId {
        PsToolId::Deform
    }

    fn title(&self) -> &'static str {
        "Деформация (сетка)"
    }

    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        let outcome = ToolOutcome::default();
        if !ctx.primary_down {
            self.drag = None;
        }

        let view = ctx.view;
        let Some(layer) = ctx.stack.active_transformable_mut() else {
            self.drag = None;
            self.handles.clear();
            self.grid_dims = None;
            return outcome;
        };
        if layer.kind.is_base() {
            self.drag = None;
            self.handles.clear();
            self.grid_dims = None;
            return outcome;
        }

        // Initialize an identity grid from the affine footprint the first time we touch this layer.
        if layer.deform.is_none() {
            layer.deform = Some(layer.identity_deform_grid(DEFAULT_COLS, DEFAULT_ROWS));
        }
        let (mut points, dims) = match &layer.deform {
            Some(grid) => (grid_points(grid), (grid.cols, grid.rows)),
            None => {
                self.handles.clear();
                self.grid_dims = None;
                return outcome;
            }
        };

        if let Some(pointer) = ctx.pointer_image {
            // Begin a grid-point drag on a fresh press over a handle inside the viewport.
            if ctx.primary_pressed && ctx.pointer_in_viewport && self.drag.is_none() {
                if let Some(idx) = self.hit_test(&view, &points, pointer) {
                    self.drag = Some(PointDrag {
                        point_idx: idx,
                        start_point: points[idx],
                        start_pointer: pointer,
                    });
                }
            }

            // Apply the active drag: move the one control point by the pointer delta (page px).
            if let Some(drag) = self.drag
                && ctx.primary_down
                && drag.point_idx < points.len()
            {
                let new = drag.start_point + (pointer - drag.start_pointer);
                points[drag.point_idx] = new;
                if let Some(grid) = layer.deform.as_mut() {
                    grid.points_px[drag.point_idx] = [new.x, new.y];
                }
            }
        }

        self.handles = points;
        self.grid_dims = Some(dims);
        outcome
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        _pointer_image: Option<Pos2>,
    ) {
        if self.handles.is_empty() {
            return;
        }
        let outline = Stroke::new(1.0, Color32::from_rgb(120, 200, 255));
        let shadow = Stroke::new(1.0, Color32::from_black_alpha(140));
        // Mesh lines: connect each control point to its right and below neighbor (row-major grid).
        if let Some((cols, rows)) = self.grid_dims
            && cols >= 2
            && rows >= 2
            && self.handles.len() == cols * rows
        {
            let line = Stroke::new(1.0, Color32::from_rgb(120, 200, 255).gamma_multiply(0.6));
            let scr = |i: usize| view.world_to_screen(self.handles[i]);
            for r in 0..rows {
                for c in 0..cols {
                    let i = r * cols + c;
                    if c + 1 < cols {
                        painter.line_segment([scr(i), scr(i + 1)], line);
                    }
                    if r + 1 < rows {
                        painter.line_segment([scr(i), scr(i + cols)], line);
                    }
                }
            }
        }
        for &p in &self.handles {
            let s = view.world_to_screen(p);
            let rect = Rect::from_center_size(s, Vec2::splat(HANDLE_HALF_PX * 2.0));
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::WHITE);
            painter.rect_stroke(
                rect,
                CornerRadius::ZERO,
                shadow,
                egui::StrokeKind::Outside,
            );
            painter.rect_stroke(rect, CornerRadius::ZERO, outline, egui::StrokeKind::Middle);
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Тяните узлы сетки,");
        ui.label("чтобы деформировать слой.");
        ui.label("Сетка появляется при входе в режим.");
        ui.label("Действует на активный слой (не базовый).");
    }
}
