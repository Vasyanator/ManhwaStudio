/*
File: tabs/ps_editor/tools/transform.rs

Purpose:
Move / rotate / scale tool for the PS-like editor. It manipulates the active raster layer's
`LayerTransform` (placement in page space) without touching pixels, so a layer can be freely
moved, rotated, and uniformly scaled. Base layers (source/clean) are locked and ignored.

Key structures:
- `TransformTool`: the active drag gesture plus a per-frame gizmo cache for overlay drawing. `drag`
  is the in-flight marker `PsTool::gesture_in_flight` reports; the gizmo cache is not.

Notes:
The tool has NO options and no keys: every interaction is a positional gizmo drag, listed by
`PsTool::hotkey_rows` for the shortcut panel rather than printed as hint labels in `options_ui`.
Pixels never change here, so no tile re-upload is needed; rendering re-evaluates the transform
each frame. Handles are hit-tested in screen space (via the frame `ViewTransform`), while the drag
math runs in page space.
*/

use super::{PsHotkeyRow, PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::tabs::ps_editor::layers::LayerTransform;
use crate::tabs::ps_editor::viewport::ViewTransform;
use eframe::egui;
use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};

/// Screen-space pull distance of the rotate handle above the layer's top edge.
const ROTATE_HANDLE_OFFSET_PX: f32 = 26.0;
/// Screen-space half-size of a drawn handle square.
const HANDLE_HALF_PX: f32 = 4.0;
/// Screen-space radius within which a handle is grabbed.
const HANDLE_HIT_PX: f32 = 11.0;
const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Move,
    Rotate,
    Scale,
}

/// In-progress gesture: the mode plus the layer state captured when it began.
#[derive(Debug, Clone, Copy)]
struct TransformDrag {
    mode: DragMode,
    start_pointer: Pos2,
    start_transform: LayerTransform,
    /// Angle (rad) from the layer center to the press point, for `Rotate`.
    start_angle: f32,
    /// Distance from the layer center to the press point, for `Scale`.
    start_dist: f32,
}

/// Cached per-frame gizmo geometry (page-space), drawn in `draw_overlay`.
#[derive(Debug, Clone, Copy)]
struct Gizmo {
    corners: [Pos2; 4],
    rotate_handle: Pos2,
}

/// Move / rotate / scale tool operating on the active raster layer's transform.
#[derive(Debug, Clone, Default)]
pub struct TransformTool {
    drag: Option<TransformDrag>,
    gizmo: Option<Gizmo>,
}

fn midpoint(a: Pos2, b: Pos2) -> Pos2 {
    Pos2::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5)
}

impl TransformTool {
    /// Picks the interaction mode from where the press landed, in screen space. Handles take
    /// priority; anything else (inside or just outside the footprint) moves the layer.
    fn hit_test(&self, view: &ViewTransform, gizmo: &Gizmo, pointer: Pos2) -> DragMode {
        let screen = view.world_to_screen(pointer);
        if screen.distance(view.world_to_screen(gizmo.rotate_handle)) <= HANDLE_HIT_PX {
            return DragMode::Rotate;
        }
        for corner in gizmo.corners {
            if screen.distance(view.world_to_screen(corner)) <= HANDLE_HIT_PX {
                return DragMode::Scale;
            }
        }
        DragMode::Move
    }
}

impl PsTool for TransformTool {
    fn id(&self) -> PsToolId {
        PsToolId::Transform
    }

    fn title(&self) -> &'static str {
        t!("ps_editor.tools.transform_title")
    }

    /// A gesture is in flight exactly while a handle drag is held.
    ///
    /// `drag` is set only by a press that `interact` accepted (`primary_pressed &&
    /// pointer_in_viewport`) and is taken on the first frame with the button up, so a press that
    /// landed on a floating panel never reports `true`. The cached `gizmo` is NOT consulted: it is
    /// rebuilt every frame for the overlay and says nothing about a gesture.
    fn gesture_in_flight(&self) -> bool {
        self.drag.is_some()
    }

    /// Drops the in-progress drag (and its captured start transform) plus the cached gizmo.
    ///
    /// `interact` already clears the drag on the first frame with the button up, so this only
    /// matters when the tool stops receiving frames at all — a tool or page switch taken mid-drag
    /// via a hotkey. The layer transform mutated so far is kept: it is a committed edit.
    fn reset(&mut self) {
        self.drag = None;
        self.gizmo = None;
    }

    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        use crate::trace::cat;
        let outcome = ToolOutcome::default();
        if !ctx.primary_down {
            // Log gesture end once, only when a gesture was actually active.
            if let Some(drag) = self.drag.take() {
                crate::trace_log!(cat::INPUT, "transform drag_end mode={:?}", drag.mode);
            }
        }

        let view = ctx.view;
        let Some(layer) = ctx.stack.active_transformable_mut() else {
            self.drag = None;
            self.gizmo = None;
            return outcome;
        };
        // A deformed layer is positioned by its mesh (absolute page px), not its affine transform —
        // mirror the deformed-text rule and refuse affine move/rotate/scale while a mesh is present.
        if layer.deform.is_some() {
            self.drag = None;
            self.gizmo = None;
            return outcome;
        }

        let corners = layer.world_corners();
        let top_mid = midpoint(corners[0], corners[1]);
        let bottom_mid = midpoint(corners[2], corners[3]);
        let up = (top_mid - bottom_mid).normalized();
        let offset_world = if view.zoom > f32::EPSILON {
            ROTATE_HANDLE_OFFSET_PX / view.zoom
        } else {
            ROTATE_HANDLE_OFFSET_PX
        };
        let rotate_handle = top_mid + up * offset_world;
        let gizmo = Gizmo {
            corners,
            rotate_handle,
        };

        if let Some(pointer) = ctx.pointer_image {
            // Begin a gesture on a fresh press inside the viewport.
            if ctx.primary_pressed && ctx.pointer_in_viewport && self.drag.is_none() {
                let mode = self.hit_test(&view, &gizmo, pointer);
                let center = layer.transform.center.to_pos2();
                crate::trace_log!(
                    cat::INPUT,
                    "transform drag_begin mode={:?} at=({:.1},{:.1})",
                    mode,
                    pointer.x,
                    pointer.y
                );
                self.drag = Some(TransformDrag {
                    mode,
                    start_pointer: pointer,
                    start_transform: layer.transform,
                    start_angle: (pointer - center).angle(),
                    start_dist: (pointer - center).length(),
                });
            }

            // Apply the active gesture.
            if let Some(drag) = self.drag
                && ctx.primary_down
            {
                let center = drag.start_transform.center.to_pos2();
                match drag.mode {
                    DragMode::Move => {
                        layer.transform.center =
                            drag.start_transform.center + (pointer - drag.start_pointer);
                    }
                    DragMode::Rotate => {
                        let delta = (pointer - center).angle() - drag.start_angle;
                        layer.transform.rotation = drag.start_transform.rotation + delta;
                    }
                    DragMode::Scale => {
                        let dist = (pointer - center).length();
                        if drag.start_dist > f32::EPSILON {
                            let factor = dist / drag.start_dist;
                            layer.transform.scale =
                                (drag.start_transform.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
                        }
                    }
                }
            }
        }

        // Recompute the gizmo from the (possibly updated) transform for this frame's overlay.
        let corners = layer.world_corners();
        let top_mid = midpoint(corners[0], corners[1]);
        let bottom_mid = midpoint(corners[2], corners[3]);
        let up = (top_mid - bottom_mid).normalized();
        self.gizmo = Some(Gizmo {
            corners,
            rotate_handle: top_mid + up * offset_world,
        });
        outcome
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        _pointer_image: Option<Pos2>,
    ) {
        let Some(gizmo) = self.gizmo else {
            return;
        };
        let screen: Vec<Pos2> = gizmo
            .corners
            .iter()
            .map(|&c| view.world_to_screen(c))
            .collect();
        let outline = Stroke::new(1.0, Color32::from_rgb(230, 230, 230));
        let shadow = Stroke::new(1.0, Color32::from_black_alpha(140));
        // Bounding box (shadow underlay then light stroke for contrast on any background).
        let mut box_pts = screen.clone();
        box_pts.push(screen[0]);
        painter.add(egui::Shape::line(box_pts.clone(), shadow));
        painter.add(egui::Shape::line(box_pts, outline));

        // Rotate handle: stalk from the top-edge midpoint to a small circle.
        let top_mid = midpoint(screen[0], screen[1]);
        let handle = view.world_to_screen(gizmo.rotate_handle);
        painter.add(egui::Shape::line_segment([top_mid, handle], outline));
        painter.circle(
            handle,
            HANDLE_HALF_PX + 1.0,
            Color32::WHITE,
            Stroke::new(1.0, Color32::BLACK),
        );

        // Corner scale handles.
        for &corner in &screen {
            let rect = Rect::from_center_size(corner, Vec2::splat(HANDLE_HALF_PX * 2.0));
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::WHITE);
            painter.rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::BLACK),
                egui::StrokeKind::Middle,
            );
        }
    }

    /// No parameters: the whole tool is the positional gizmo, and every gesture it understands is
    /// listed by [`PsTool::hotkey_rows`] instead. So `options_ui` is left at its default no-op and
    /// the panel prints `ps_editor.active_tool.no_options`.
    fn has_options(&self) -> bool {
        false
    }

    /// The gizmo's three drag zones. There are no KEYS here — the mode is chosen positionally by
    /// `hit_test` (rotate handle, corner square, anything else), so each row names the target it
    /// is dragged by. "Активный слой" in the labels is the real constraint: base layers are locked
    /// and `active_transformable_mut` refuses them.
    fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
        vec![
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.transform.move_label"),
                t!("ps_editor.tools.hotkey.transform.move_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.transform.rotate_label"),
                t!("ps_editor.tools.hotkey.transform.rotate_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.transform.scale_label"),
                t!("ps_editor.tools.hotkey.transform.scale_keys"),
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

    /// A page with the two locked base layers plus one transformable raster, which
    /// `add_raster_layer` also makes active — the only layer this tool accepts.
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
        tool: &mut TransformTool,
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
            secondary_down: false,
            pointer_delta: Vec2::ZERO,
            modifiers: egui::Modifiers::default(),
            cancel_pressed: false,
            remove_point_pressed: false,
            // Identity view: screen and world coordinates coincide, so the fixture can name pixels.
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

    /// `gesture_in_flight` must track the handle drag from the accepted press to the release: it is
    /// what lets the tab keep the gizmo overlay alive while the pointer is dragged over a panel.
    #[test]
    fn gesture_in_flight_spans_a_handle_drag() {
        let mut tool = TransformTool::default();
        let mut stack = stack_with_raster();

        assert!(!tool.gesture_in_flight(), "a fresh tool holds no drag");
        // Anywhere inside the viewport begins a Move drag (`hit_test`'s fallback).
        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, true);
        assert!(tool.gesture_in_flight(), "the accepted press starts a drag");
        frame(&mut tool, &mut stack, Pos2::new(50.0, 20.0), false, false, true);
        assert!(tool.gesture_in_flight(), "the drag survives crossing a panel");
        frame(&mut tool, &mut stack, Pos2::new(50.0, 20.0), true, false, false);
        assert!(!tool.gesture_in_flight(), "the release ends the drag");
    }

    /// A press that lands on a floating panel holds the button down but is refused by the start
    /// gate, so no gesture exists and the overlay must not be kept alive under the panel.
    #[test]
    fn a_press_outside_the_viewport_starts_no_gesture() {
        let mut tool = TransformTool::default();
        let mut stack = stack_with_raster();
        let under_panel = Pos2::new(20.0, 20.0);

        frame(&mut tool, &mut stack, under_panel, false, true, true);
        assert!(!tool.gesture_in_flight(), "a refused press starts no drag");
        for held in 0..3 {
            frame(&mut tool, &mut stack, under_panel, false, false, true);
            assert!(!tool.gesture_in_flight(), "held frame {held} must stay gesture-free");
        }
    }

    /// `reset` is the tab's abandonment path (tool or page switch taken mid-drag); it must leave no
    /// phantom gesture that would keep the overlay pinned under a panel.
    #[test]
    fn reset_clears_gesture_in_flight() {
        let mut tool = TransformTool::default();
        let mut stack = stack_with_raster();

        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, true);
        assert!(tool.gesture_in_flight());
        tool.reset();
        assert!(!tool.gesture_in_flight(), "reset must drop the drag");
    }
}
