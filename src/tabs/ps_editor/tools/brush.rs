/*
File: tabs/ps_editor/tools/brush.rs

Purpose:
Round color brush for the PS-like editor. Paints (or erases) onto the active editable raster
layer, clipped by the current selection. Reuses `crate::tools::MaskBrush` for the shared radius
gesture/size-shortcut handling, but stamps RGBA color instead of a binary mask.

Key structures:
- `BrushTool`: brush radius (via `MaskBrush`), color, erase flag, and in-stroke state.

Notes:
The radius shortcuts (Shift+wheel, `-`, `=`/`+`) live in `crate::tools::MaskBrush`; `hotkey_rows`
publishes them to the tab's shortcut panel and must stay in sync with that implementation.
Stamping replaces pixels with the brush color (hard round brush). Erasing writes transparent
pixels. When a selection is active, pixels outside it are left untouched.

A stroke may only START while the pointer is on bare canvas (`PsToolContext::pointer_in_viewport`);
once in flight it keeps painting even when the pointer crosses a floating dock panel. See the guard
in `interact`. `PsTool::gesture_in_flight` publishes that same `last_world` marker so the tab can
apply the identical rule to the brush-size CURSOR CIRCLE.
*/

use super::{DirtyRect, PsHotkeyRow, PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::tabs::ps_editor::layers::Layer;
use crate::tabs::ps_editor::selection::Selection;
use crate::tabs::ps_editor::viewport::ViewTransform;
use crate::tools::MaskBrush;
use eframe::egui;
use egui::{Color32, ColorImage, Pos2, Stroke, Vec2};

/// Round color brush operating on the active raster layer.
#[derive(Debug, Clone)]
pub struct BrushTool {
    brush: MaskBrush,
    color: Color32,
    erase: bool,
    /// Last pointer position in world (page) pixels during an active stroke.
    last_world: Option<Pos2>,
}

impl Default for BrushTool {
    fn default() -> Self {
        Self {
            brush: MaskBrush::default(),
            color: Color32::BLACK,
            erase: false,
            last_world: None,
        }
    }
}

impl BrushTool {
    /// Forwards Shift+wheel radius changes to the shared brush; returns true when consumed.
    pub fn handle_wheel(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.brush.handle_wheel(delta_y, modifiers)
    }

    /// Forwards `-`/`=`/`+` size shortcuts to the shared brush; returns true when the size changed.
    pub fn handle_size_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        self.brush.handle_size_shortcuts(ctx)
    }

    fn radius(&self) -> i32 {
        self.brush.radius_px().max(1) as i32
    }
}

impl PsTool for BrushTool {
    fn id(&self) -> PsToolId {
        PsToolId::Brush
    }

    fn title(&self) -> &'static str {
        t!("ps_editor.tools.brush_title")
    }

    /// A stroke is in flight exactly while `last_world` holds a point.
    ///
    /// It is the same marker the start gate in `interact` reads, so the two can never disagree: a
    /// press the gate refused leaves `last_world == None` and is correctly reported as "no gesture",
    /// even though the button is held.
    fn gesture_in_flight(&self) -> bool {
        self.last_world.is_some()
    }

    /// Ends any in-progress stroke by forgetting its last point.
    ///
    /// `last_world` is the start of the next painted segment, so a stroke that survived a tool or
    /// page switch would draw a line from a position that belongs to another page.
    fn reset(&mut self) {
        self.last_world = None;
    }

    /// Paints one frame of the stroke and reports the region it touched.
    ///
    /// Two gates decide whether anything is painted: the primary button must be DOWN, and a stroke
    /// may only be STARTED while the pointer is inside the canvas viewport. A stroke already in
    /// flight keeps painting wherever the pointer goes — including over a floating dock panel —
    /// and ends on the first frame the button is up.
    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        use crate::trace::cat;
        let mut outcome = ToolOutcome::default();

        // End the stroke when the primary button is released or lifted off the canvas.
        if !ctx.primary_down {
            // Log stroke end only when a stroke was actually in progress (avoids per-frame idle spam).
            if self.last_world.is_some() {
                crate::trace_log!(
                    cat::INPUT,
                    "brush stroke_end radius={} erase={}",
                    self.radius(),
                    self.erase
                );
            }
            self.last_world = None;
            return outcome;
        }

        let Some(pointer) = ctx.pointer_image else {
            return outcome;
        };
        // A stroke may only BEGIN on bare canvas, on EVERY frame — never only on the frame of the
        // press. `last_world` is the in-flight marker: while it is `None` no stroke exists, so a
        // pointer outside the viewport must not start one. Gating on `primary_pressed` instead (as
        // this did) cannot work at all: the tab hands the tool
        // `primary_pressed && pointer_in_viewport` (`../mod.rs`, `draw_canvas`), so over a floating
        // panel that flag is already false, the guard never fired, and `last_world == None` made
        // the frame begin a fresh stroke under the panel — which then committed to undo and to the
        // shared `LayerDoc` on release.
        // Once a stroke IS in flight it continues even when the pointer crosses a panel, mirroring
        // the tab's panning decision (a gesture begun on bare canvas survives the crossing).
        if self.last_world.is_none() && !ctx.pointer_in_viewport {
            return outcome;
        }

        // Snapshot read-only state before borrowing the active layer mutably.
        let radius = self.radius();
        let color = self.color;
        let erase = self.erase;
        // Track the pointer in world (page) px; mapping into layer-local space happens below so
        // the brush paints correctly on moved/rotated/scaled layers.
        let to_world = pointer;
        let from_world = if ctx.primary_pressed || self.last_world.is_none() {
            to_world
        } else {
            self.last_world.unwrap_or(to_world)
        };

        // `selection` and `stack` are distinct fields of `PsToolContext`, so the borrow checker
        // allows borrowing the selection immutably while the active layer is held mutably.
        let selection = ctx.selection.as_ref();
        let Some(layer) = ctx.stack.active_editable_mut() else {
            // Active layer is a locked base layer: nothing to paint on.
            return outcome;
        };

        // Log only the first stamp of a stroke (press), not every dragged point.
        if ctx.primary_pressed || self.last_world.is_none() {
            crate::trace_log!(
                cat::INPUT,
                "brush stroke_begin radius={} erase={} at=({:.1},{:.1})",
                radius,
                erase,
                to_world.x,
                to_world.y
            );
        }
        self.last_world = Some(to_world);
        let map = LocalMap::from_layer(layer);
        let radius_local = ((radius as f32) / map.scale).round().max(1.0) as i32;
        let from = map.to_local(from_world);
        let to = map.to_local(to_world);
        let from = (from.x.round() as i32, from.y.round() as i32);
        let to = (to.x.round() as i32, to.y.round() as i32);
        let layer_size = layer.image.size;
        let params = StampParams {
            selection,
            map,
            radius: radius_local,
            color,
            erase,
        };
        paint_line_color(&mut layer.image, &params, from, to);

        outcome.dirty = Some(segment_dirty_rect(from, to, radius_local, layer_size));
        outcome
    }

    // (stroke begin/end logged above; per-stamp painting is intentionally untraced)

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        pointer_image: Option<Pos2>,
    ) {
        let Some(pointer) = pointer_image else {
            return;
        };
        let center = view.world_to_screen(pointer);
        let radius_screen = (self.radius() as f32 * view.zoom).max(0.5);
        // Black-on-white ring so the cursor reads on any background.
        painter.circle_stroke(center, radius_screen, Stroke::new(2.0, Color32::WHITE));
        painter.circle_stroke(
            center,
            (radius_screen - 1.0).max(0.5),
            Stroke::new(1.0, Color32::BLACK),
        );
    }

    fn as_brush_mut(&mut self) -> Option<&mut BrushTool> {
        Some(self)
    }

    /// Colour, radius and the eraser toggle are real parameters.
    fn has_options(&self) -> bool {
        true
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t!("ps_editor.tools.brush_color_label"));
            let mut rgb = [self.color.r(), self.color.g(), self.color.b()];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                self.color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            }
        });
        let mut radius = self.brush.radius_px();
        if ui
            .add(crate::widgets::WheelSlider::new(&mut radius, 1..=200).text(t!("ps_editor.tools.brush_size_label")))
            .changed()
        {
            self.brush.set_radius_px(radius);
        }
        ui.checkbox(&mut self.erase, t!("ps_editor.tools.brush_eraser_label"));
    }

    /// The three radius shortcuts the brush actually owns, mirroring `crate::tools::MaskBrush`:
    /// Shift+wheel steps the radius (`handle_wheel`), `-` scales it by 0.9 and `=`/`+` by 1.1
    /// (`handle_size_shortcuts`). Painting itself is a plain drag and needs no row.
    fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
        vec![
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_wheel_label"),
                t!("ps_editor.tools.hotkey.brush.radius_wheel_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_down_label"),
                t!("ps_editor.tools.hotkey.brush.radius_down_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_up_label"),
                t!("ps_editor.tools.hotkey.brush.radius_up_keys"),
            ),
        ]
    }
}

/// Maps between a layer's local pixel space and page (world) space, captured by value so it can be
/// used while the layer image is borrowed mutably.
#[derive(Clone, Copy)]
struct LocalMap {
    center: Vec2,
    rotation: f32,
    scale: f32,
    half: Vec2,
}

impl LocalMap {
    fn from_layer(layer: &Layer) -> Self {
        let scale = if layer.transform.scale.abs() < f32::EPSILON {
            f32::EPSILON
        } else {
            layer.transform.scale
        };
        Self {
            center: layer.transform.center,
            rotation: layer.transform.rotation,
            scale,
            half: layer.image_size() * 0.5,
        }
    }

    fn to_local(self, world: Pos2) -> Pos2 {
        (self.half + rotate(world - self.center.to_pos2(), -self.rotation) / self.scale).to_pos2()
    }

    fn to_world(self, local_x: f32, local_y: f32) -> Pos2 {
        (self.center + rotate(Vec2::new(local_x, local_y) - self.half, self.rotation) * self.scale)
            .to_pos2()
    }
}

fn rotate(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Shared brush-stamp parameters, bundled so the stamping helpers stay within argument limits.
struct StampParams<'a> {
    /// Optional clip selection in page space; pixels outside it are skipped when present.
    selection: Option<&'a Selection>,
    /// Layer-local ↔ page-space mapping used for selection clipping.
    map: LocalMap,
    /// Disc radius in layer-local pixels.
    radius: i32,
    /// Fill color; ignored when `erase` is set.
    color: Color32,
    /// When true, stamps transparency instead of `color`.
    erase: bool,
}

/// Stamps a round brush along the segment `from`→`to` (layer-local px), clipped to the selection
/// (page space, mapped through the params' `map`) when present.
fn paint_line_color(dst: &mut ColorImage, params: &StampParams, from: (i32, i32), to: (i32, i32)) {
    let dx = (to.0 - from.0) as f32;
    let dy = (to.1 - from.1) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        stamp_circle(dst, params, from.0, from.1);
        return;
    }
    let step = (params.radius as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (i32::MIN, i32::MIN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = (from.0 as f32 + dx * t).round() as i32;
        let sy = (from.1 as f32 + dy * t).round() as i32;
        if (sx, sy) == last {
            continue;
        }
        stamp_circle(dst, params, sx, sy);
        last = (sx, sy);
    }
}

/// Fills a clipped disc of `params.radius` at layer-local `(cx, cy)`. When a selection is present
/// each pixel is mapped back to page space through `params.map` and skipped if it falls outside it.
fn stamp_circle(dst: &mut ColorImage, params: &StampParams, cx: i32, cy: i32) {
    let r = params.radius.max(1);
    let r2 = r * r;
    let w = dst.size[0] as i32;
    let h = dst.size[1] as i32;
    let fill = if params.erase {
        Color32::TRANSPARENT
    } else {
        params.color
    };
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h - 1);
    for y in y0..=y1 {
        let dy = y - cy;
        let rem = r2 - dy * dy;
        if rem < 0 {
            continue;
        }
        let span = (rem as f32).sqrt() as i32;
        let sx0 = (cx - span).max(0);
        let sx1 = (cx + span).min(w - 1);
        if sx0 > sx1 {
            continue;
        }
        let row = y as usize * dst.size[0];
        for x in sx0..=sx1 {
            if let Some(sel) = params.selection {
                let world = params.map.to_world(x as f32 + 0.5, y as f32 + 0.5);
                if world.x < 0.0
                    || world.y < 0.0
                    || !sel.contains(world.x as usize, world.y as usize)
                {
                    continue;
                }
            }
            dst.pixels[row + x as usize] = fill;
        }
    }
}

/// Bounding box (clamped to the page) of a stamped segment, used for tile invalidation.
fn segment_dirty_rect(
    from: (i32, i32),
    to: (i32, i32),
    radius: i32,
    page_size: [usize; 2],
) -> DirtyRect {
    let r = radius.max(1);
    let min_x = (from.0.min(to.0) - r).max(0) as usize;
    let min_y = (from.1.min(to.1) - r).max(0) as usize;
    let max_x = ((from.0.max(to.0) + r).max(0) as usize).min(page_size[0].saturating_sub(1));
    let max_y = ((from.1.max(to.1) + r).max(0) as usize).min(page_size[1].saturating_sub(1));
    DirtyRect {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::ps_editor::layers::LayerStack;
    use egui::Rect;

    /// Page size of the fixture, in px. Large enough that a small brush stamp lands well inside it.
    const PAGE: [usize; 2] = [64, 64];

    /// A page with the two locked base layers plus one blank, directly editable raster on top,
    /// which `add_raster_layer` also makes active — the layer the brush is allowed to paint.
    fn stack_with_blank_raster() -> LayerStack {
        let mut stack = LayerStack::new(
            0,
            PAGE,
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
        );
        stack.add_raster_layer();
        stack
    }

    /// A brush with a small radius, so one stamp stays inside the fixture page and two stamps at
    /// different positions cover measurably different pixel counts.
    fn small_brush() -> BrushTool {
        let mut tool = BrushTool::default();
        tool.brush.set_radius_px(2);
        tool
    }

    /// Number of non-transparent pixels of the stack's active raster.
    fn painted_pixels(stack: &LayerStack) -> usize {
        stack.layer(stack.active_id()).map_or(0, |layer| {
            layer.image.pixels.iter().filter(|px| px.a() != 0).count()
        })
    }

    /// Runs ONE frame of `interact` with the pointer state the tab would hand the tool.
    ///
    /// `primary_pressed` is deliberately masked with `in_viewport`: `draw_canvas` passes
    /// `input.primary_pressed && pointer_in_viewport` (`../mod.rs`), so a press landing on a
    /// floating panel reaches the tool with `primary_pressed == false`. Letting a test set that
    /// combination freely would let it assert a state the tab cannot produce.
    fn frame(
        tool: &mut BrushTool,
        stack: &mut LayerStack,
        pointer: Pos2,
        in_viewport: bool,
        pressed: bool,
        down: bool,
    ) -> ToolOutcome {
        let mut selection = None;
        let page_size = stack.size();
        let mut ctx = PsToolContext {
            page_size,
            pointer_image: Some(pointer),
            pointer_in_viewport: in_viewport,
            primary_pressed: pressed && in_viewport,
            primary_down: down,
            primary_released: false,
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
        tool.interact(&mut ctx)
    }

    /// A press that lands on a floating dock panel must not paint — not on the press frame, and
    /// not on any held-button frame after it.
    ///
    /// This is what the old `!pointer_in_viewport && primary_pressed` guard could not do: the tab
    /// masks `primary_pressed` with `pointer_in_viewport`, so that condition was never true and
    /// every one of these frames found `last_world == None` and began a stroke under the panel.
    #[test]
    fn a_press_outside_the_viewport_never_starts_a_stroke() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let under_panel = Pos2::new(32.0, 32.0);

        let press = frame(&mut tool, &mut stack, under_panel, false, true, true);
        assert!(press.dirty.is_none(), "the press frame must not paint under a panel");
        for held in 0..3 {
            let outcome = frame(&mut tool, &mut stack, under_panel, false, false, true);
            assert!(
                outcome.dirty.is_none(),
                "held frame {held} must not paint under a panel"
            );
        }
        assert_eq!(
            painted_pixels(&stack),
            0,
            "no pixel of the active layer may change while the stroke was never allowed to start"
        );
    }

    /// The other half of the contract: only the START is gated. A stroke begun on bare canvas keeps
    /// painting when the pointer crosses a floating panel, mirroring the tab's panning decision.
    #[test]
    fn a_stroke_begun_inside_the_viewport_survives_the_pointer_crossing_a_panel() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        let press = frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(press.dirty.is_some(), "a press on bare canvas paints");
        let after_press = painted_pixels(&stack);
        assert!(after_press > 0, "the press stamp is on the layer");

        let crossing = frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), false, false, true);
        assert!(
            crossing.dirty.is_some(),
            "a stroke in flight continues while the pointer is over a panel"
        );
        assert!(
            painted_pixels(&stack) > after_press,
            "the segment dragged across the panel must be painted"
        );
    }

    /// `gesture_in_flight` must track the STROKE, not the button: it is what the tab asks before it
    /// lets the brush circle keep following a pointer that moved over a floating panel.
    #[test]
    fn gesture_in_flight_follows_the_stroke_from_press_to_release() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        assert!(!tool.gesture_in_flight(), "a fresh brush holds no stroke");
        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(tool.gesture_in_flight(), "the accepted press starts a stroke");
        frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), false, false, true);
        assert!(tool.gesture_in_flight(), "the stroke survives crossing a panel");
        frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), true, false, false);
        assert!(!tool.gesture_in_flight(), "the release ends the stroke");
    }

    /// The case a `primary_down` test at the call site would get wrong: a press that LANDS on a
    /// floating panel holds the button down too, but the start gate refused it, so no gesture
    /// exists and the preview must not be kept alive under the panel.
    #[test]
    fn a_refused_press_never_reports_a_gesture_in_flight() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let under_panel = Pos2::new(32.0, 32.0);

        frame(&mut tool, &mut stack, under_panel, false, true, true);
        assert!(!tool.gesture_in_flight(), "a refused press starts no gesture");
        for held in 0..3 {
            frame(&mut tool, &mut stack, under_panel, false, false, true);
            assert!(!tool.gesture_in_flight(), "held frame {held} must stay gesture-free");
        }
    }

    /// Releasing ends the stroke, so the start gate is armed again: a finished stroke must not let
    /// a later press over a panel paint.
    #[test]
    fn a_release_re_arms_the_start_gate() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        // Button up: `interact` clears `last_world`, ending the stroke.
        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, false, false);
        let after_stroke = painted_pixels(&stack);

        let press = frame(&mut tool, &mut stack, Pos2::new(48.0, 48.0), false, true, true);
        assert!(press.dirty.is_none(), "the next press under a panel must not paint");
        let held = frame(&mut tool, &mut stack, Pos2::new(48.0, 48.0), false, false, true);
        assert!(held.dirty.is_none(), "nor may the frame after it");
        assert_eq!(
            painted_pixels(&stack),
            after_stroke,
            "the layer must still hold exactly the pixels of the finished stroke"
        );
    }
}
