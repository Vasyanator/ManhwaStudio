/*
File: tabs/ps_editor/tools/brush.rs

Purpose:
Round color brush for the PS-like editor. Paints (or erases) onto the active editable raster
layer, clipped by the current selection. Reuses `crate::tools::MaskBrush` for the shared radius
gesture/size-shortcut handling, but stamps RGBA color instead of a binary mask.

Key structures:
- `BrushTool`: brush radius (via `MaskBrush`), color, erase flag, and in-stroke state.

Notes:
Stamping replaces pixels with the brush color (hard round brush). Erasing writes transparent
pixels. When a selection is active, pixels outside it are left untouched.
*/

use super::{DirtyRect, PsTool, PsToolContext, PsToolId, ToolOutcome};
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
        "Кисть"
    }

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
        if !ctx.pointer_in_viewport && ctx.primary_pressed {
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
        paint_line_color(
            &mut layer.image,
            selection,
            &map,
            from,
            to,
            radius_local,
            color,
            erase,
        );

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

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Цвет:");
            let mut rgb = [self.color.r(), self.color.g(), self.color.b()];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                self.color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            }
        });
        let mut radius = self.brush.radius_px();
        if ui
            .add(crate::widgets::WheelSlider::new(&mut radius, 1..=200).text("Размер"))
            .changed()
        {
            self.brush.set_radius_px(radius);
        }
        ui.checkbox(&mut self.erase, "Ластик");
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

    fn to_local(&self, world: Pos2) -> Pos2 {
        (self.half + rotate(world - self.center.to_pos2(), -self.rotation) / self.scale).to_pos2()
    }

    fn to_world(&self, local_x: f32, local_y: f32) -> Pos2 {
        (self.center + rotate(Vec2::new(local_x, local_y) - self.half, self.rotation) * self.scale)
            .to_pos2()
    }
}

fn rotate(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Stamps a round brush along the segment `from`→`to` (layer-local px), clipped to `selection`
/// (page space, mapped through `map`) when present.
fn paint_line_color(
    dst: &mut ColorImage,
    selection: Option<&Selection>,
    map: &LocalMap,
    from: (i32, i32),
    to: (i32, i32),
    radius: i32,
    color: Color32,
    erase: bool,
) {
    let dx = (to.0 - from.0) as f32;
    let dy = (to.1 - from.1) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        stamp_circle(dst, selection, map, from.0, from.1, radius, color, erase);
        return;
    }
    let step = (radius as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (i32::MIN, i32::MIN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = (from.0 as f32 + dx * t).round() as i32;
        let sy = (from.1 as f32 + dy * t).round() as i32;
        if (sx, sy) == last {
            continue;
        }
        stamp_circle(dst, selection, map, sx, sy, radius, color, erase);
        last = (sx, sy);
    }
}

/// Fills a clipped disc of `radius` at layer-local `(cx, cy)`. When a `selection` is present each
/// pixel is mapped back to page space through `map` and skipped if it falls outside the selection.
#[allow(clippy::too_many_arguments)]
fn stamp_circle(
    dst: &mut ColorImage,
    selection: Option<&Selection>,
    map: &LocalMap,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
    erase: bool,
) {
    let r = radius.max(1);
    let r2 = r * r;
    let w = dst.size[0] as i32;
    let h = dst.size[1] as i32;
    let fill = if erase { Color32::TRANSPARENT } else { color };
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
            if let Some(sel) = selection {
                let world = map.to_world(x as f32 + 0.5, y as f32 + 0.5);
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
