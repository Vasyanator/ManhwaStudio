/*
File: region_edit_v2/render.rs

Purpose:
Everything the on-canvas region frame PAINTS: the two-tone border, the eight resize handles,
the drag strip above the frame, the backgrounds of the chrome rows, the status line and the
off-screen arrow. Paint only — nothing here allocates a widget or claims input, so a call
into this file can never change what the pointer hits (`egui-docs/06-overlays.md` §3).

Key structures:
- none; every entry point is a free function taking an `egui::Painter`

Key functions:
- `visual_color()`: the single mapping from `FrameVisual` to the inner stroke colour (D6)
- `paint_frame_border()`, `paint_handles()`: the frame itself; `sector_points()`, the partial
  disc a handle is drawn as
- `paint_strip_background()`, `paint_grip()`, `paint_layer_chip()`: the strip above the frame
- `paint_row_background()`, `paint_status_text()`: the two rows below it
- `paint_offscreen_arrow()`: the "the frame is over there" indicator of a locked frame

Notes:
The colours are named constants with one meaning each, in the style of `FLUX2_STATUS_*` in
`tools/flux2_klein.rs`; the red and the green are deliberately the same two tones that file
uses, so a status colour means the same thing across the cleaning tools.
Design: `dev-docs/region_edit_v2_plan.md` (§1, §2 D6).
*/

use super::frame::FrameVisual;
use super::geometry::OffscreenArrow;
use super::input::{HANDLE_RADIUS, HandleKind, handle_arc, handle_points};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, pos2, vec2};

/// Outer stroke of the frame: a dark grey ring that separates the coloured inner stroke from
/// the artwork beneath, so the state colour stays readable over a white or a black page.
const FRAME_OUTER_COLOR: Color32 = Color32::from_rgb(28, 28, 28);
/// Inner stroke of a frame that may be moved and resized.
const FRAME_FREE_COLOR: Color32 = Color32::from_rgb(215, 215, 215);
/// Inner stroke of a frame whose size violates the active consumer's requirements. Same red
/// as `FLUX2_STATUS_ERROR_COLOR`.
const FRAME_INVALID_COLOR: Color32 = Color32::from_rgb(255, 120, 120);
/// Inner stroke of an occupied frame — a mask is painted, a result waits, or work is running.
/// Same green as `FLUX2_STATUS_OK_COLOR`.
const FRAME_OCCUPIED_COLOR: Color32 = Color32::from_rgb(90, 255, 130);

/// Background of the strip above the frame and of the two rows below it. Opaque enough to
/// read text over any page.
const CHROME_BACKGROUND: Color32 = Color32::from_rgba_premultiplied(20, 20, 22, 225);
/// Colour of the grip dots and of an inactive mask-layer chip's label.
const CHROME_FOREGROUND: Color32 = Color32::from_rgb(200, 200, 205);
/// Fill of the mask-layer chip that is currently selected for painting.
const LAYER_CHIP_ACTIVE_FILL: Color32 = Color32::from_rgb(60, 90, 140);

/// Width of the outer stroke, in screen points.
const OUTER_STROKE_W: f32 = 3.0;
/// Width of the inner, state-coloured stroke, in screen points.
const INNER_STROKE_W: f32 = 1.5;
/// Corner radius of the chrome rows.
const CHROME_CORNER: u8 = 3;
/// Font size of the status line and of the layer chips.
const CHROME_FONT_SIZE: f32 = 12.0;
/// Padding between the status plate's edge and its text, in screen points.
const STATUS_TEXT_INSET: f32 = 6.0;
/// Length of the off-screen arrow's shaft, in screen points.
const ARROW_SHAFT: f32 = 26.0;
/// Segments a handle's arc is approximated with.
///
/// One count for both sweeps: a half disc is then smoother than it needs to be and a
/// three-quarter disc exactly smooth enough at the 8 pt handle radius, which is cheaper than
/// a second constant nobody would keep in step with the first.
const HANDLE_ARC_SEGMENTS: usize = 24;

/// The inner stroke colour for a frame state. The ONE place `FrameVisual` becomes a colour.
#[must_use]
pub(super) fn visual_color(visual: FrameVisual) -> Color32 {
    match visual {
        FrameVisual::Free => FRAME_FREE_COLOR,
        FrameVisual::Invalid => FRAME_INVALID_COLOR,
        FrameVisual::Occupied => FRAME_OCCUPIED_COLOR,
    }
}

/// Paints the frame's two-tone border: a dark grey ring OUTSIDE the rect and the
/// state-coloured stroke INSIDE it, so the two never overdraw each other.
pub(super) fn paint_frame_border(painter: &Painter, rect: Rect, visual: FrameVisual) {
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(OUTER_STROKE_W, FRAME_OUTER_COLOR), StrokeKind::Outside);
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(INNER_STROKE_W, visual_color(visual)), StrokeKind::Inside);
}

/// Paints the eight resize handles of `rect`, each as the PARTIAL disc that lies outside the
/// frame: a half disc on a side midpoint, a three-quarter disc on a corner.
///
/// Nothing is drawn inside the frame, because the interior is where the pointer paints the
/// mask; the omitted part is exactly the part the handle's hit rectangles omit too
/// (`input::handle_hit_rects`), so what the user sees is what the user can grab.
///
/// `enabled` is false for a locked frame (D4): the handles stay visible so the user can see
/// where they would be, but they are drawn in the state colour rather than white and no
/// hitbox is registered for them — that decision belongs to `frame.rs`, not here.
pub(super) fn paint_handles(painter: &Painter, rect: Rect, visual: FrameVisual, enabled: bool) {
    let fill = if enabled { Color32::WHITE } else { visual_color(visual) };
    let stroke = Stroke::new(1.0, FRAME_OUTER_COLOR);
    for (handle, point) in HandleKind::ALL.into_iter().zip(handle_points(rect)) {
        let (start, sweep) = handle_arc(handle);
        painter.add(Shape::convex_polygon(sector_points(point, HANDLE_RADIUS, start, sweep), fill, stroke));
    }
}

/// The outline of a circular sector, as the polygon `Shape::convex_polygon` is handed.
///
/// The disc CENTRE is deliberately the first point: `Shape::convex_polygon` tessellates a
/// filled path as a triangle fan from vertex 0 (`epaint-0.35.0/src/tessellator.rs:790`), and
/// a fan from the centre is exact for a sector of ANY sweep — every point of a sector is
/// visible from its centre. That is what lets a three-quarter disc, which is NOT convex, be
/// filled correctly through this call; the winding order is normalized by the tessellator
/// itself, so the caller may sweep either way.
///
/// Angles are radians in egui's y-down screen space (`x = cos`, `y = sin`).
#[must_use]
fn sector_points(center: Pos2, radius: f32, start: f32, sweep: f32) -> Vec<Pos2> {
    let mut points = Vec::with_capacity(HANDLE_ARC_SEGMENTS + 2);
    points.push(center);
    let steps = f32::from(u16::try_from(HANDLE_ARC_SEGMENTS).unwrap_or(u16::MAX)).max(1.0);
    for step in 0..=HANDLE_ARC_SEGMENTS {
        // Widened through `u16` rather than cast: the loop bound is a small constant, so the
        // conversion is exact, and no `as` on a numeric type is needed to get there.
        let t = f32::from(u16::try_from(step).unwrap_or(u16::MAX)) / steps;
        let angle = sweep.mul_add(t, start);
        points.push(center + vec2(angle.cos(), angle.sin()) * radius);
    }
    points
}

/// Paints the background plate shared by the top strip and the two rows below the frame.
pub(super) fn paint_row_background(painter: &Painter, rect: Rect) {
    if !rect.is_positive() {
        return;
    }
    painter.rect_filled(rect, CornerRadius::same(CHROME_CORNER), CHROME_BACKGROUND);
}

/// Paints the drag grip: three short horizontal bars centred in `rect`.
///
/// `active` brightens the grip while the strip is being dragged, which is the only feedback
/// the strip gives — the frame itself already follows the pointer.
pub(super) fn paint_grip(painter: &Painter, rect: Rect, active: bool) {
    if !rect.is_positive() {
        return;
    }
    let color = if active { Color32::WHITE } else { CHROME_FOREGROUND };
    let center = rect.center();
    let half_w = (rect.width() * 0.25).min(18.0);
    // Three bars, 3 pt apart, centred on the strip: enough of a texture to read as a grip at
    // any strip height without needing an icon font.
    for offset in [-3.0_f32, 0.0, 3.0] {
        let y = center.y + offset;
        if y > rect.top() && y < rect.bottom() {
            painter.line_segment([pos2(center.x - half_w, y), pos2(center.x + half_w, y)], Stroke::new(1.0, color));
        }
    }
}

/// Paints one mask-layer chip: a plate carrying the layer's 1-based number, filled when the
/// layer is the one strokes go into.
pub(super) fn paint_layer_chip(painter: &Painter, rect: Rect, number: usize, active: bool, tint: Color32) {
    if !rect.is_positive() {
        return;
    }
    let fill = if active { LAYER_CHIP_ACTIVE_FILL } else { CHROME_BACKGROUND };
    painter.rect_filled(rect, CornerRadius::same(CHROME_CORNER), fill);
    // The layer's own preview tint as a border, so the chip and the mask it paints are
    // recognisably the same layer even when the numbers are too small to read.
    painter.rect_stroke(rect, CornerRadius::same(CHROME_CORNER), Stroke::new(1.0, tint), StrokeKind::Inside);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        number.to_string(),
        FontId::proportional(CHROME_FONT_SIZE),
        if active { Color32::WHITE } else { CHROME_FOREGROUND },
    );
}

/// Paints the status line, left-aligned inside `rect` and coloured by the frame's state.
///
/// The text is ELIDED at the plate's width rather than laid out freely: `Painter::text` lays
/// out through `layout_no_wrap` (`egui-0.35.0/src/painter.rs:477`), which has no width at all,
/// so a status sentence longer than the plate would run out over the artwork — unreadable, and
/// outside the hitbox the user is told belongs to the frame.
pub(super) fn paint_status_text(painter: &Painter, rect: Rect, text: &str, visual: FrameVisual) {
    if !rect.is_positive() {
        return;
    }
    let color = visual_color(visual);
    let max_width = (rect.width() - STATUS_TEXT_INSET * 2.0).max(1.0);
    let mut job = LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(CHROME_FONT_SIZE), color);
    job.wrap = TextWrapping::truncate_at_width(max_width);
    let galley = painter.layout_job(job);
    // Vertically centred by hand: `Painter::galley` positions a galley by its TOP-LEFT.
    let top = rect.center().y - galley.size().y * 0.5;
    painter.galley(pos2(rect.left() + STATUS_TEXT_INSET, top), galley, color);
}

/// Paints the "the frame is over there" arrow at the viewport border.
///
/// The tip comes from `geometry::offscreen_arrow`; the shaft is drawn back from it towards
/// the viewport centre, and a disc under the head keeps the arrow visible over busy artwork.
pub(super) fn paint_offscreen_arrow(painter: &Painter, arrow: &OffscreenArrow, visual: FrameVisual) {
    let color = visual_color(visual);
    let tail: Pos2 = arrow.tip - arrow.dir * ARROW_SHAFT;
    painter.circle_filled(arrow.tip, 5.0, FRAME_OUTER_COLOR);
    painter.arrow(tail, arrow.dir * ARROW_SHAFT, Stroke::new(2.5, color));
}

/// Paints a translucent scrim over the frame body while work is running, so the running
/// state is visible without hiding the mask or the page under it.
pub(super) fn paint_processing_scrim(painter: &Painter, rect: Rect) {
    if !rect.is_positive() {
        return;
    }
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgba_premultiplied(0, 0, 0, 60));
}

/// Rect of the chip of mask layer `index` inside a strip whose right end is `right`.
///
/// Chips are laid out right to left so the drag grip keeps the whole left part of the strip
/// whatever the layer count is. Pure geometry, kept here beside the painting that uses it.
#[must_use]
pub(super) fn layer_chip_rect(strip: Rect, index: usize, count: usize) -> Rect {
    let side = (strip.height() - 4.0).max(1.0);
    let gap = 2.0;
    // `count - 1 - index`: index 0 is the LEFTMOST chip of the right-aligned group.
    let from_right = count.saturating_sub(1).saturating_sub(index);
    // Widened losslessly through `u16`: the layer count comes from a tint slice built in
    // code, so it is a handful, and saturating there beats an `as` cast that could round.
    let steps = f32::from(u16::try_from(from_right).unwrap_or(u16::MAX));
    let right = strip.right() - 2.0 - steps * (side + gap);
    Rect::from_min_size(pos2(right - side, strip.top() + 2.0), vec2(side, side))
}
