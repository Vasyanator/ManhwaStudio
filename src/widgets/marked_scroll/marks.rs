/*
File: src/widgets/marked_scroll/marks.rs

Purpose:
Typed model and painting of "marks" placed onto a vertical scrollbar track.

A mark anchors to a position along the scrollable content (by fraction or by
content pixels) and to a cross-axis sector (whole bar or a third), and is drawn
either as a typed fill (solid / hatched) or by a free user callback. All marks
are painted under the scroll handle by `bar.rs`.

Key structures:
- ScrollSpan: where along the scroll axis a mark sits.
- ScrollSector: where across the bar width a mark sits.
- MarkFill: solid or hatched fill style.
- ScrollMark / MarkKind: a single mark (typed fill or free custom drawing) + layer.
- BarGeometry: projection helpers from content space to the track rect.

Notes:
Pure geometry/painting; no scroll engine state lives here. `bar.rs` builds the
`BarGeometry` from the egui `ScrollArea` output and calls `paint_mark`.
*/

use egui::{Color32, Painter, Rangef, Rect, Stroke, lerp, pos2, remap_clamp};

/// Position of a mark along the scroll axis (vertical).
///
/// Both variants are ordered automatically (start/end may be swapped by the
/// caller); painting clamps them to the visible track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollSpan {
    /// Fraction of the total scrollable content, each component in `[0.0, 1.0]`.
    Fraction { start: f32, end: f32 },
    /// Absolute content pixels along the scroll axis, in the same space as the
    /// `ScrollArea` content size and offset (`0.0` is the top of the content).
    ContentPixels { start: f32, end: f32 },
}

impl ScrollSpan {
    /// A zero-length span at a single fraction, useful for point markers.
    #[must_use]
    pub fn fraction_at(fraction: f32) -> Self {
        Self::Fraction {
            start: fraction,
            end: fraction,
        }
    }

    /// A zero-length span at a single content pixel, useful for point markers.
    #[must_use]
    pub fn pixel_at(content_pixel: f32) -> Self {
        Self::ContentPixels {
            start: content_pixel,
            end: content_pixel,
        }
    }
}

/// Cross-axis (horizontal) extent of a mark within the bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollSector {
    /// The full bar width.
    Full,
    /// Left third of the bar width.
    LeftThird,
    /// Middle third of the bar width.
    MiddleThird,
    /// Right third of the bar width.
    RightThird,
    /// Arbitrary fraction of the bar width, each component in `[0.0, 1.0]`.
    Fraction { start: f32, end: f32 },
}

/// Fill style of a typed mark.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkFill {
    /// Solid color fill.
    Solid(Color32),
    /// Diagonal hatching. `spacing` and `thickness` are in points; `angle_deg`
    /// is the line direction in degrees. `background`, if set, fills the rect
    /// before the hatch lines are drawn.
    Hatched {
        color: Color32,
        background: Option<Color32>,
        spacing: f32,
        thickness: f32,
        angle_deg: f32,
    },
}

impl MarkFill {
    /// Default 45-degree hatching with the given line color.
    #[must_use]
    pub fn hatched(color: Color32) -> Self {
        Self::Hatched {
            color,
            background: None,
            spacing: 6.0,
            thickness: 1.5,
            angle_deg: 45.0,
        }
    }
}

/// Boxed free-drawing callback for a custom mark: painter, bar geometry, and the
/// span×sector cell rect.
pub type CustomMarkFn = Box<dyn FnOnce(&Painter, &BarGeometry, Rect)>;

/// What a mark draws: a typed fill or a free callback over the bar geometry.
///
/// The callback receives the shared `BarGeometry` and the `Rect` computed from
/// the mark's span and sector, so it can either fill that cell or position
/// custom shapes precisely along the bar.
pub enum MarkKind {
    /// Typed solid/hatched fill of the span×sector cell.
    Fill(MarkFill),
    /// Free user drawing. Receives the bar geometry and the span×sector cell.
    Custom(CustomMarkFn),
}

impl std::fmt::Debug for MarkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fill(fill) => f.debug_tuple("Fill").field(fill).finish(),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

/// A single mark on the scrollbar: where it sits, how it looks, and its layer.
///
/// Marks are painted in ascending `layer` order (stable for equal layers) and
/// always below the scroll handle.
#[derive(Debug)]
pub struct ScrollMark {
    pub span: ScrollSpan,
    pub sector: ScrollSector,
    pub layer: i32,
    pub kind: MarkKind,
}

impl ScrollMark {
    /// A typed fill mark spanning `span`, defaulting to the full bar width and layer 0.
    #[must_use]
    pub fn fill(span: ScrollSpan, fill: MarkFill) -> Self {
        Self {
            span,
            sector: ScrollSector::Full,
            layer: 0,
            kind: MarkKind::Fill(fill),
        }
    }

    /// A free-drawing mark spanning `span`, defaulting to the full bar width and layer 0.
    #[must_use]
    pub fn custom(
        span: ScrollSpan,
        draw: impl FnOnce(&Painter, &BarGeometry, Rect) + 'static,
    ) -> Self {
        Self {
            span,
            sector: ScrollSector::Full,
            layer: 0,
            kind: MarkKind::Custom(Box::new(draw)),
        }
    }

    /// Sets the cross-axis sector of this mark.
    #[must_use]
    pub fn sector(mut self, sector: ScrollSector) -> Self {
        self.sector = sector;
        self
    }

    /// Sets the z-order layer of this mark (higher draws later, still under the handle).
    #[must_use]
    pub fn layer(mut self, layer: i32) -> Self {
        self.layer = layer;
        self
    }
}

/// Projection of content space onto the on-screen bar track.
///
/// `track_rect` is the on-screen column the handle travels in (its currently
/// visible width for floating bars). `content_size` and `viewport` are the
/// scroll-axis lengths of the content and the visible viewport; `offset` is the
/// current scroll offset. Marks map content positions to `track_rect` linearly
/// and independently of `offset` (a mark sits at a fixed content position).
#[derive(Clone, Copy, Debug)]
pub struct BarGeometry {
    pub track_rect: Rect,
    pub content_size: f32,
    pub viewport: f32,
    pub offset: f32,
}

impl BarGeometry {
    /// Maps a content fraction in `[0.0, 1.0]` to a screen Y on the track.
    #[must_use]
    pub fn fraction_to_y(&self, fraction: f32) -> f32 {
        lerp(
            self.track_rect.top()..=self.track_rect.bottom(),
            fraction.clamp(0.0, 1.0),
        )
    }

    /// Maps an absolute content pixel to a screen Y on the track, clamped to the track.
    #[must_use]
    pub fn content_to_y(&self, content_pixel: f32) -> f32 {
        // `max(EPSILON)` keeps the remap well-defined for empty/degenerate content.
        let total = self.content_size.max(f32::EPSILON);
        remap_clamp(
            content_pixel,
            0.0..=total,
            self.track_rect.top()..=self.track_rect.bottom(),
        )
    }

    /// Computes the handle rectangle for this geometry, matching the bar port in
    /// `bar.rs`. `handle_min_length` is the minimum handle length in points.
    ///
    /// Useful when decorating a foreign scrollbar (e.g. an `egui::ScrollArea::both`)
    /// where the handle must be re-drawn on top of marks.
    #[must_use]
    pub fn handle_rect(&self, handle_min_length: f32) -> Rect {
        let track_len = self.track_rect.height();
        let handle_size = if self.content_size > 0.0 {
            (self.viewport / self.content_size * track_len).clamp(handle_min_length, track_len)
        } else {
            track_len
        };
        let max_offset = (self.content_size - self.viewport).max(0.0);
        let travel_top = self.track_rect.top();
        let travel_bottom = self.track_rect.bottom() - handle_size;
        let top = if max_offset > 0.0 {
            remap_clamp(self.offset, 0.0..=max_offset, travel_top..=travel_bottom)
        } else {
            travel_top
        };
        Rect::from_min_max(
            pos2(self.track_rect.left(), top),
            pos2(self.track_rect.right(), top + handle_size),
        )
    }
}

/// Screen-space Y range of `span` on the track, with ordered, clamped bounds.
pub(crate) fn span_to_y_range(span: &ScrollSpan, geom: &BarGeometry) -> Rangef {
    let (a, b) = match span {
        ScrollSpan::Fraction { start, end } => (
            geom.fraction_to_y(start.min(*end)),
            geom.fraction_to_y(start.max(*end)),
        ),
        ScrollSpan::ContentPixels { start, end } => (
            geom.content_to_y(start.min(*end)),
            geom.content_to_y(start.max(*end)),
        ),
    };
    Rangef::new(a, b)
}

/// Screen-space X range of `sector` across the bar `track`.
pub(crate) fn sector_to_x_range(sector: &ScrollSector, track: &Rect) -> Rangef {
    let left = track.left();
    let width = track.width();
    let right = track.right();
    match sector {
        ScrollSector::Full => Rangef::new(left, right),
        ScrollSector::LeftThird => Rangef::new(left, left + width / 3.0),
        ScrollSector::MiddleThird => Rangef::new(left + width / 3.0, left + 2.0 * width / 3.0),
        ScrollSector::RightThird => Rangef::new(left + 2.0 * width / 3.0, right),
        ScrollSector::Fraction { start, end } => {
            let lo = start.min(*end).clamp(0.0, 1.0);
            let hi = start.max(*end).clamp(0.0, 1.0);
            Rangef::new(left + width * lo, left + width * hi)
        }
    }
}

/// Paints a single mark onto the track. `opacity` multiplies typed-fill alpha
/// (custom marks manage their own alpha).
pub(crate) fn paint_mark(painter: &Painter, mark: ScrollMark, geom: &BarGeometry, opacity: f32) {
    let y_range = span_to_y_range(&mark.span, geom);
    let x_range = sector_to_x_range(&mark.sector, &geom.track_rect);
    let rect = Rect::from_min_max(
        pos2(x_range.min, y_range.min),
        pos2(x_range.max, y_range.max),
    );
    match mark.kind {
        MarkKind::Fill(fill) => paint_fill(painter, rect, &fill, opacity),
        MarkKind::Custom(draw) => draw(painter, geom, rect),
    }
}

/// Paints a typed fill into `rect` with `opacity` applied to its colors.
fn paint_fill(painter: &Painter, rect: Rect, fill: &MarkFill, opacity: f32) {
    match fill {
        MarkFill::Solid(color) => {
            painter.rect_filled(rect, 0.0, color.gamma_multiply(opacity));
        }
        MarkFill::Hatched {
            color,
            background,
            spacing,
            thickness,
            angle_deg,
        } => {
            if let Some(bg) = background {
                painter.rect_filled(rect, 0.0, bg.gamma_multiply(opacity));
            }
            paint_hatch(
                painter,
                rect,
                color.gamma_multiply(opacity),
                *spacing,
                *thickness,
                *angle_deg,
            );
        }
    }
}

/// Draws parallel hatch lines covering `rect`, clipped to it.
fn paint_hatch(
    painter: &Painter,
    rect: Rect,
    color: Color32,
    spacing: f32,
    thickness: f32,
    angle_deg: f32,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    // Keep the step positive so the loop always terminates.
    let spacing = spacing.max(1.0);
    let clipped = painter.with_clip_rect(rect);
    let angle = angle_deg.to_radians();
    let (dir_x, dir_y) = (angle.cos(), angle.sin());
    // Perpendicular direction along which we step between successive lines.
    let (perp_x, perp_y) = (-dir_y, dir_x);
    // A diagonal length guarantees each line spans the whole rect after clipping.
    let diag = rect.width() + rect.height();
    let center = rect.center();
    let stroke = Stroke::new(thickness, color);
    let mut t = -diag;
    while t <= diag {
        let base = pos2(center.x + perp_x * t, center.y + perp_y * t);
        let p0 = pos2(base.x - dir_x * diag, base.y - dir_y * diag);
        let p1 = pos2(base.x + dir_x * diag, base.y + dir_y * diag);
        clipped.line_segment([p0, p1], stroke);
        t += spacing;
    }
}

#[cfg(test)]
mod tests {
    use super::{BarGeometry, ScrollSector, ScrollSpan, sector_to_x_range, span_to_y_range};
    use egui::{Rect, pos2};

    fn geom(content: f32, viewport: f32) -> BarGeometry {
        BarGeometry {
            track_rect: Rect::from_min_max(pos2(100.0, 0.0), pos2(110.0, 200.0)),
            content_size: content,
            viewport,
            offset: 0.0,
        }
    }

    #[test]
    fn content_pixels_map_to_track_endpoints_and_midpoint() {
        let g = geom(1000.0, 200.0);
        assert!((g.content_to_y(0.0) - 0.0).abs() < 1e-3);
        assert!((g.content_to_y(1000.0) - 200.0).abs() < 1e-3);
        assert!((g.content_to_y(500.0) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn content_pixels_clamp_outside_range() {
        let g = geom(1000.0, 200.0);
        assert!((g.content_to_y(-50.0) - 0.0).abs() < 1e-3);
        assert!((g.content_to_y(5000.0) - 200.0).abs() < 1e-3);
    }

    #[test]
    fn span_orders_swapped_bounds() {
        let g = geom(1000.0, 200.0);
        let r = span_to_y_range(
            &ScrollSpan::ContentPixels {
                start: 1725.0,
                end: 1567.0,
            },
            &g,
        );
        assert!(r.min <= r.max);
    }

    #[test]
    fn fraction_span_maps_to_quarter_marks() {
        let g = geom(1000.0, 200.0);
        let r = span_to_y_range(
            &ScrollSpan::Fraction {
                start: 0.20,
                end: 0.25,
            },
            &g,
        );
        assert!((r.min - 40.0).abs() < 1e-3);
        assert!((r.max - 50.0).abs() < 1e-3);
    }

    #[test]
    fn sector_thirds_partition_bar_width() {
        let track = Rect::from_min_max(pos2(100.0, 0.0), pos2(130.0, 200.0));
        let left = sector_to_x_range(&ScrollSector::LeftThird, &track);
        let mid = sector_to_x_range(&ScrollSector::MiddleThird, &track);
        let right = sector_to_x_range(&ScrollSector::RightThird, &track);
        assert!((left.min - 100.0).abs() < 1e-3);
        assert!((left.max - 110.0).abs() < 1e-3);
        assert!((mid.min - 110.0).abs() < 1e-3);
        assert!((mid.max - 120.0).abs() < 1e-3);
        assert!((right.min - 120.0).abs() < 1e-3);
        assert!((right.max - 130.0).abs() < 1e-3);
    }

    #[test]
    fn empty_content_does_not_panic() {
        let g = geom(0.0, 0.0);
        let _ = g.content_to_y(123.0);
        let r = span_to_y_range(&ScrollSpan::pixel_at(50.0), &g);
        assert!(r.min.is_finite() && r.max.is_finite());
    }
}
