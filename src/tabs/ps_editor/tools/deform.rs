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
  overlay. `drag` is the in-flight marker `PsTool::gesture_in_flight` reports; the cache is not.

Notes:
The tool has NO options and no keys: a handle is grabbed positionally, and that gesture is listed
by `PsTool::hotkey_rows` for the shortcut panel rather than printed as hint labels in `options_ui`.
This is the basic grid-point drag path (Phase 5 scope): each handle moves a single control point.
The richer perspective / bend / sampled-edge handle modes from the typing tab are not wired here;
this delivers a usable, model-consistent raster deform that round-trips through the doc and disk.
*/

use super::{PsHotkeyRow, PsTool, PsToolContext, PsToolId, ToolOutcome};
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
        t!("ps_editor.tools.deform_title")
    }

    /// A gesture is in flight exactly while a control-point drag is held.
    ///
    /// `drag` is set only by a press `interact` accepted (`primary_pressed && pointer_in_viewport`
    /// AND landing on a handle) and is taken on the first frame with the button up, so a press on a
    /// floating panel never reports `true`. `handles` / `grid_dims` are per-frame overlay caches,
    /// not gesture state, and are deliberately not consulted.
    fn gesture_in_flight(&self) -> bool {
        self.drag.is_some()
    }

    /// Drops the in-progress control-point drag and the per-frame handle cache.
    ///
    /// Like the transform tool, `interact` clears the drag once the button is up; this covers the
    /// case where the tool stops receiving frames mid-drag (tool or page switch by hotkey). The
    /// grid edited so far stays on the layer — it is a committed edit.
    fn reset(&mut self) {
        self.drag = None;
        self.handles.clear();
        self.grid_dims = None;
    }

    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        use crate::trace::cat;
        let outcome = ToolOutcome::default();
        if !ctx.primary_down
            && let Some(drag) = self.drag.take()
        {
            crate::trace_log!(cat::INPUT, "deform drag_end point_idx={}", drag.point_idx);
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
            crate::trace_log!(
                cat::PS_EDITOR,
                "deform init_grid cols={} rows={}",
                DEFAULT_COLS,
                DEFAULT_ROWS
            );
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
            if ctx.primary_pressed
                && ctx.pointer_in_viewport
                && self.drag.is_none()
                && let Some(idx) = self.hit_test(&view, &points, pointer)
            {
                crate::trace_log!(
                    cat::INPUT,
                    "deform drag_begin point_idx={} at=({:.1},{:.1})",
                    idx,
                    pointer.x,
                    pointer.y
                );
                self.drag = Some(PointDrag {
                    point_idx: idx,
                    start_point: points[idx],
                    start_pointer: pointer,
                });
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

    /// No parameters: the grid resolution is fixed (`DEFAULT_COLS`×`DEFAULT_ROWS`) and every
    /// gesture is listed by [`PsTool::hotkey_rows`]. So `options_ui` is left at its default no-op
    /// and the panel prints `ps_editor.active_tool.no_options`.
    fn has_options(&self) -> bool {
        false
    }

    /// The one drag gesture plus the fact that entering the tool materializes the grid. No KEYS:
    /// a handle is grabbed positionally, within `HANDLE_HIT_PX` of a control point. "Активный
    /// слой" in the label is the real constraint — base layers are locked.
    fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
        vec![
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.deform.drag_label"),
                t!("ps_editor.tools.hotkey.deform.drag_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.deform.grid_label"),
                t!("ps_editor.tools.hotkey.deform.grid_keys"),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::ps_editor::layers::LayerStack;
    use egui::ColorImage;

    /// Page size of the fixture, in px. The identity view below maps it 1:1 onto screen space.
    const PAGE: [usize; 2] = [64, 64];

    /// A page with the two locked base layers plus one raster, which `add_raster_layer` also makes
    /// active — the only layer this tool accepts (base layers are refused).
    fn stack_with_raster() -> LayerStack {
        let mut stack = LayerStack::new(
            0,
            PAGE,
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
        );
        stack.add_raster_layer();
        stack
    }

    /// Runs ONE frame of `interact` with the pointer state the tab would hand the tool.
    ///
    /// `primary_pressed` is masked with `in_viewport` on purpose: `draw_canvas` passes
    /// `input.primary_pressed && pointer_in_viewport` (`../../mod.rs`), so a press landing on a
    /// floating panel reaches the tool with `primary_pressed == false`, and a test must not be able
    /// to assert a combination the tab cannot produce.
    fn frame(
        tool: &mut DeformTool,
        stack: &mut LayerStack,
        pointer: Pos2,
        in_viewport: bool,
        pressed: bool,
        down: bool,
    ) {
        let mut selection = None;
        let page_size = stack.size();
        let mut ctx = PsToolContext {
            page_size,
            pointer_image: Some(pointer),
            pointer_in_viewport: in_viewport,
            primary_pressed: pressed && in_viewport,
            primary_down: down,
            primary_released: !down,
            modifiers: egui::Modifiers::default(),
            cancel_pressed: false,
            remove_point_pressed: false,
            // Identity view: screen and world coordinates coincide, so handle hit-testing in the
            // fixture works at the control points' page-pixel positions.
            view: ViewTransform {
                viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 64.0)),
                zoom: 1.0,
                center_world: Vec2::new(32.0, 32.0),
            },
            stack,
            selection: &mut selection,
        };
        tool.interact(&mut ctx);
    }

    /// One idle frame materializes the identity grid and caches its control points; returns the
    /// first handle's page-pixel position, which a press must land on to start a drag.
    fn first_handle(tool: &mut DeformTool, stack: &mut LayerStack) -> Pos2 {
        frame(tool, stack, Pos2::new(1.0, 1.0), true, false, false);
        *tool
            .handles
            .first()
            .expect("entering the tool seeds a 3x3 identity grid")
    }

    /// `gesture_in_flight` must track the control-point drag from the accepted press to the
    /// release: it is what lets the tab keep the mesh overlay alive across a floating panel.
    #[test]
    fn gesture_in_flight_spans_a_control_point_drag() {
        let mut tool = DeformTool::default();
        let mut stack = stack_with_raster();

        let handle = first_handle(&mut tool, &mut stack);
        assert!(!tool.gesture_in_flight(), "seeding the grid is not a gesture");
        frame(&mut tool, &mut stack, handle, true, true, true);
        assert!(tool.gesture_in_flight(), "the accepted press over a handle starts a drag");
        frame(&mut tool, &mut stack, handle + Vec2::new(20.0, 0.0), false, false, true);
        assert!(tool.gesture_in_flight(), "the drag survives crossing a panel");
        frame(&mut tool, &mut stack, handle + Vec2::new(20.0, 0.0), true, false, false);
        assert!(!tool.gesture_in_flight(), "the release ends the drag");
    }

    /// A press that lands on a floating panel holds the button down but is refused by the start
    /// gate, so no gesture exists and the overlay must not be kept alive under the panel.
    #[test]
    fn a_press_outside_the_viewport_starts_no_gesture() {
        let mut tool = DeformTool::default();
        let mut stack = stack_with_raster();
        let handle = first_handle(&mut tool, &mut stack);

        frame(&mut tool, &mut stack, handle, false, true, true);
        assert!(!tool.gesture_in_flight(), "a refused press starts no drag");
        for held in 0..3 {
            frame(&mut tool, &mut stack, handle, false, false, true);
            assert!(!tool.gesture_in_flight(), "held frame {held} must stay gesture-free");
        }
    }

    /// `reset` is the tab's abandonment path (tool or page switch taken mid-drag); it must leave no
    /// phantom gesture that would keep the overlay pinned under a panel.
    #[test]
    fn reset_clears_gesture_in_flight() {
        let mut tool = DeformTool::default();
        let mut stack = stack_with_raster();
        let handle = first_handle(&mut tool, &mut stack);

        frame(&mut tool, &mut stack, handle, true, true, true);
        assert!(tool.gesture_in_flight());
        tool.reset();
        assert!(!tool.gesture_in_flight(), "reset must drop the drag");
    }
}
