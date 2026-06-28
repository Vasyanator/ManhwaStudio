/*
File: src/widgets/marked_scroll/gutter.rs

Purpose:
API for placing elements in the gutter to the left of the scrollbar, anchored to
a position along the scroll axis. Mirrors the existing "cut arrow" markers used
by the New Project window, generalized into a reusable widget API.

Key structures:
- GutterSlot: the gutter column rect plus the projected Y anchor for an item.
- GutterItem: a span-anchored free-drawing item with a layer.
- ArrowStyle / arrow(): built-in right-pointing arrow that reproduces the
  existing scroll cut markers.

Notes:
Items are painted in ascending `layer` order. Projection from span to anchor Y
is done by `bar.rs` via `BarGeometry`; the gutter never overlaps the bar track.
*/

use super::marks::{BarGeometry, ScrollSpan, span_to_y_range};
use egui::{Color32, Painter, Rect, Shape, Stroke, pos2};

/// On-screen slot handed to a gutter item: the gutter column and its anchor Y.
///
/// `cell` is the full gutter column (left of the bar track); `anchor_y` is the
/// projected screen Y of the item's span center within that column.
#[derive(Clone, Copy, Debug)]
pub struct GutterSlot {
    pub cell: Rect,
    pub anchor_y: f32,
}

/// Boxed free-drawing callback for a gutter item: painter and the resolved slot.
pub type GutterDrawFn = Box<dyn FnOnce(&Painter, &GutterSlot)>;

/// A free-drawing element placed in the gutter left of the bar.
///
/// `span` selects where along the scroll axis the item is anchored (its center
/// is used as the anchor Y). `draw` paints the item given its `GutterSlot`.
pub struct GutterItem {
    pub span: ScrollSpan,
    pub layer: i32,
    pub draw: GutterDrawFn,
}

impl std::fmt::Debug for GutterItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GutterItem")
            .field("span", &self.span)
            .field("layer", &self.layer)
            .field("draw", &"<fn>")
            .finish()
    }
}

impl GutterItem {
    /// Creates a gutter item anchored at `span` with layer 0.
    #[must_use]
    pub fn new(span: ScrollSpan, draw: impl FnOnce(&Painter, &GutterSlot) + 'static) -> Self {
        Self {
            span,
            layer: 0,
            draw: Box::new(draw),
        }
    }

    /// Sets the z-order layer of this item (higher draws later).
    #[must_use]
    pub fn layer(mut self, layer: i32) -> Self {
        self.layer = layer;
        self
    }
}

/// Visual style of the built-in gutter arrow.
///
/// The arrow tip touches the right edge of the gutter cell (pointing at the
/// bar); `width`/`height` size the triangle and `tail_*` size the short tail
/// line drawn to its left.
#[derive(Clone, Copy, Debug)]
pub struct ArrowStyle {
    pub width: f32,
    pub height: f32,
    pub fill: Color32,
    pub stroke: Stroke,
    pub tail_length: f32,
    pub tail_width: f32,
}

/// Paints gutter items into `gutter_rect`, projecting each item's span center
/// onto the bar `geometry`. Items are drawn in ascending `layer` order (stable
/// for equal layers). Shared by build-time gutter items and deferred painting.
pub(crate) fn paint_gutter_items(
    painter: &Painter,
    gutter_rect: Rect,
    geometry: &BarGeometry,
    mut items: Vec<GutterItem>,
) {
    items.sort_by_key(|item| item.layer);
    for item in items {
        let y_range = span_to_y_range(&item.span, geometry);
        let anchor_y = (y_range.min + y_range.max) * 0.5;
        let slot = GutterSlot {
            cell: gutter_rect,
            anchor_y,
        };
        (item.draw)(painter, &slot);
    }
}

/// Built-in right-pointing arrow anchored at `span`, reproducing the existing
/// scroll cut markers (triangle at the bar edge plus a short tail line).
#[must_use]
pub fn arrow(span: ScrollSpan, style: ArrowStyle) -> GutterItem {
    GutterItem::new(span, move |painter, slot| {
        let cell = slot.cell;
        let marker_y = slot.anchor_y;
        let half_height = style.height * 0.5;
        let tip = pos2(cell.right(), marker_y);
        let tail_x = cell.right() - style.width;
        painter.add(Shape::convex_polygon(
            vec![
                tip,
                pos2(tail_x, marker_y - half_height),
                pos2(tail_x, marker_y + half_height),
            ],
            style.fill,
            style.stroke,
        ));
        painter.line_segment(
            [
                pos2(tail_x - style.tail_length, marker_y),
                pos2(tail_x + style.tail_length * 0.2, marker_y),
            ],
            Stroke::new(style.tail_width, style.fill),
        );
    })
}
