/*
File: src/canvas/pixel_grid.rs

Purpose:
The ONE implementation of the per-source-pixel inspection grid, shared by every surface that
magnifies page pixels: the canvas tabs (cleaning turns it on automatically at high zoom) and the
PS editor (the user turns it on manually with the «Сетка пикселей» checkbox).

Main responsibilities:
- compute which grid lines are actually visible for a given image rect / zoom / clip rect;
- paint them as individual, DPI-snapped line segments.

Key structures:
- `PixelGridSpans`: the pure, allocation-free result of the visibility computation.

Key functions:
- `pixel_grid_spans`: pure geometry — testable without a live `egui::Context`.
- `draw_pixel_grid`: paints the spans through a caller-supplied painter.

Notes:
This module is deliberately GATE-FREE. It never consults
`pixel_inspection_recommended_for`/`PIXEL_INSPECTION_MIN_DEVICE_PX`: whether the grid should be
visible at all is the CALLER's policy (automatic at high zoom in cleaning, a manual checkbox plus a
cost guard in the PS editor), and the old in-painter re-check is exactly what made the canvas
version unreusable.

Two properties must survive any edit here:
- The visible column/row range is derived from the CLIP rect, never from the whole image rect. At
  `MIN_ZOOM` over a tall webtoon page an unbounded version emits ~80 000 segments per frame.
- Lines are emitted with `Painter::line_segment` only. `epaint` snaps a line SEGMENT to the physical
  pixel grid (`tessellate_line_segment`), while a polyline / `Shape::closed_line` goes through
  `tessellate_path` and blurs (`src/tabs/ps_editor/tools/MODULE_README.md`).
*/

use eframe::egui;
use egui::{Color32, Rect};

/// Visible extent of the pixel grid for one image rect, in grid-line indices.
///
/// Produced by [`pixel_grid_spans`] and consumed by [`draw_pixel_grid`]. Split out from the painter
/// so the (non-trivial) clipping arithmetic is unit-testable without a live `egui::Context`.
///
/// `clip_rect` is the already-intersected paint region; the column/row ranges are INCLUSIVE and are
/// bounded by that rect, so their size depends on the visible area, not on the page size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PixelGridSpans {
    /// Region the grid may paint in: the caller's clip rect intersected with the image rect.
    pub clip_rect: Rect,
    /// First and LAST (inclusive) vertical line index, counted from `image_rect.left()`.
    /// A plain pair rather than a `RangeInclusive` so the whole struct stays `Copy`.
    pub cols: (usize, usize),
    /// First and LAST (inclusive) horizontal line index, counted from `image_rect.top()`.
    pub rows: (usize, usize),
    /// Stroke width in points that renders as exactly one device pixel.
    pub stroke_width: f32,
    /// `pixels_per_point` clamped to `>= 1.0`, as used by the pixel-snapping arithmetic.
    pub snap_pixels_per_point: f32,
}

impl PixelGridSpans {
    /// Snaps a screen-space coordinate to the center of a physical device pixel.
    ///
    /// A 1-device-pixel line lands on a whole pixel only when its center sits at `n + 0.5` device
    /// pixels; without this the grid renders as a blurry two-pixel smear at fractional DPI.
    #[must_use]
    fn align(&self, value: f32) -> f32 {
        ((value * self.snap_pixels_per_point).round() + 0.5) / self.snap_pixels_per_point
    }
}

/// Straight (un-premultiplied) RGBA of a grid line: near-black at low alpha, so the grid reads over
/// both ink and paper without hiding the pixel it delimits.
///
/// Kept as raw components because `Color32::from_rgba_unmultiplied` is not a `const fn` in
/// ecolor 0.35 (`ecolor-0.35.0/src/color32.rs:133`), and the premultiplied `_const` variant would
/// change the colour.
const PIXEL_GRID_STROKE_RGBA: [u8; 4] = [16, 16, 16, 52];

/// Computes the visible pixel-grid extent, or `None` when nothing can be drawn.
///
/// `image_rect` is the image's axis-aligned screen rect; `zoom` is screen POINTS per source pixel
/// (must be finite and `> 0`); `clip_rect` is the region the caller may paint in;
/// `pixels_per_point` is the raw `Context::pixels_per_point()` (clamped internally to `>= 1.0` for
/// the snapping math only — a sub-1 value would distort it).
///
/// Returns `None` when the image rect is degenerate, the zoom is unusable, or the clip rect does
/// not overlap the image. It applies NO magnification threshold: see the module note.
#[must_use]
pub(crate) fn pixel_grid_spans(
    image_rect: Rect,
    zoom: f32,
    clip_rect: Rect,
    pixels_per_point: f32,
) -> Option<PixelGridSpans> {
    if !image_rect.is_positive() || !zoom.is_finite() || zoom <= 0.0 {
        return None;
    }
    let clip_rect = clip_rect.intersect(image_rect);
    if !clip_rect.is_positive() {
        return None;
    }
    let snap_pixels_per_point = pixels_per_point.max(1.0);

    // Derive the line indices from the CLIP rect, not from the image rect: this is what bounds the
    // segment count to the visible area regardless of how tall the page is.
    let first_col = ((clip_rect.left() - image_rect.left()) / zoom).floor().max(0.0) as usize;
    let last_col = ((clip_rect.right() - image_rect.left()) / zoom).ceil().min((image_rect.width() / zoom).ceil()) as usize;
    let first_row = ((clip_rect.top() - image_rect.top()) / zoom).floor().max(0.0) as usize;
    let last_row = ((clip_rect.bottom() - image_rect.top()) / zoom).ceil().min((image_rect.height() / zoom).ceil()) as usize;

    Some(PixelGridSpans {
        clip_rect,
        cols: (first_col, last_col),
        rows: (first_row, last_row),
        stroke_width: 1.0 / snap_pixels_per_point,
        snap_pixels_per_point,
    })
}

/// Paints the per-source-pixel grid over `image_rect`, clipped to `clip_rect`.
///
/// `painter` supplies the layer to draw into; the function narrows it to the effective clip region
/// itself, so the caller may pass an unclipped painter. `zoom` is screen points per source pixel,
/// `pixels_per_point` the raw `Context::pixels_per_point()`.
///
/// Does nothing when [`pixel_grid_spans`] finds nothing visible. It is GATE-FREE: the decision
/// whether a grid is wanted at this magnification belongs to the caller.
pub(crate) fn draw_pixel_grid(
    painter: &egui::Painter,
    image_rect: Rect,
    zoom: f32,
    clip_rect: Rect,
    pixels_per_point: f32,
) {
    let Some(spans) = pixel_grid_spans(image_rect, zoom, clip_rect, pixels_per_point) else {
        return;
    };
    let [r, g, b, a] = PIXEL_GRID_STROKE_RGBA;
    let stroke = egui::Stroke::new(
        spans.stroke_width,
        Color32::from_rgba_unmultiplied(r, g, b, a),
    );
    let painter = painter.with_clip_rect(spans.clip_rect);

    for col in spans.cols.0..=spans.cols.1 {
        let x = spans.align(image_rect.left() + col as f32 * zoom);
        painter.line_segment(
            [
                egui::pos2(x, spans.clip_rect.top()),
                egui::pos2(x, spans.clip_rect.bottom()),
            ],
            stroke,
        );
    }
    for row in spans.rows.0..=spans.rows.1 {
        let y = spans.align(image_rect.top() + row as f32 * zoom);
        painter.line_segment(
            [
                egui::pos2(spans.clip_rect.left(), y),
                egui::pos2(spans.clip_rect.right(), y),
            ],
            stroke,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    /// A tall page seen through a small viewport must produce a segment count proportional to the
    /// VISIBLE area, not to the page — the clip-first bounding is a hard requirement.
    #[test]
    fn spans_are_bounded_by_the_clip_rect_not_by_the_page() {
        let image_rect = Rect::from_min_size(pos2(0.0, -50_000.0), egui::vec2(800.0, 76_000.0));
        let clip_rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let spans = pixel_grid_spans(image_rect, 4.0, clip_rect, 1.0).expect("visible");
        // 400 pt / 4 pt-per-px of visible width + 1, and 300 pt / 4 of visible height + 1 — the
        // 19 000-row page contributes nothing beyond the clip.
        assert_eq!(spans.cols, (0, 100));
        assert_eq!(spans.rows, (12_500, 12_575));
    }

    /// The painter must NOT re-apply the pixel-inspection threshold: below
    /// `PIXEL_INSPECTION_MIN_DEVICE_PX` device pixels per source pixel it still reports spans, so a
    /// caller with its own policy (the PS editor's manual checkbox) can use it.
    #[test]
    fn spans_are_gate_free_below_the_inspection_threshold() {
        let zoom = 1.0;
        let pixels_per_point = 1.0;
        assert!(
            !super::super::pixel_inspection_recommended_for(zoom, pixels_per_point),
            "test premise: this magnification is below the inspection threshold"
        );
        let image_rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(64.0, 64.0));
        let spans = pixel_grid_spans(image_rect, zoom, image_rect, pixels_per_point);
        assert!(spans.is_some(), "the extracted painter must not gate on zoom");
    }

    /// Degenerate inputs are rejected rather than turned into an unbounded or NaN loop.
    #[test]
    fn degenerate_inputs_produce_no_spans() {
        let image_rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(64.0, 64.0));
        assert!(pixel_grid_spans(image_rect, 0.0, image_rect, 1.0).is_none());
        assert!(pixel_grid_spans(image_rect, f32::NAN, image_rect, 1.0).is_none());
        assert!(pixel_grid_spans(Rect::NOTHING, 8.0, image_rect, 1.0).is_none());
        let elsewhere = Rect::from_min_size(pos2(500.0, 500.0), egui::vec2(10.0, 10.0));
        assert!(pixel_grid_spans(image_rect, 8.0, elsewhere, 1.0).is_none());
    }

    /// Grid lines must land on physical device pixel centers at fractional DPI, and a sub-1
    /// `pixels_per_point` must not be allowed to distort that arithmetic.
    #[test]
    fn lines_snap_to_device_pixel_centers() {
        let image_rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(64.0, 64.0));
        let spans = pixel_grid_spans(image_rect, 8.0, image_rect, 2.0).expect("visible");
        assert!((spans.stroke_width - 0.5).abs() < f32::EPSILON);
        let aligned = spans.align(3.3);
        assert!(((aligned * 2.0) - (aligned * 2.0).floor() - 0.5).abs() < 1e-4);

        let clamped = pixel_grid_spans(image_rect, 8.0, image_rect, 0.5).expect("visible");
        assert!((clamped.snap_pixels_per_point - 1.0).abs() < f32::EPSILON);
    }
}
