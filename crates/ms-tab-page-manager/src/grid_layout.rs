/*
File: crates/ms-tab-page-manager/src/grid_layout.rs

Purpose:
GUI-free row layout of the page-manager card grid. Turns the grid's metrics, the
available width and the content counts into a table of rows with prefix-summed
offsets, answers which rows intersect a viewport, and places every card / gap /
label rect of a row. `grid.rs` drives `ScrollArea::show_viewport` from it.

Key structures:
- GridMetrics: card footprint, clean-card height, link gap, item spacing, spacing below a
  clean-bearing row, section header height.
- GridRowKind / GridRow: what a row holds and where it sits (top + height).
- GridLayout: the built row table, column count and total content height.
- LayoutRect: an axis-aligned rect in content-origin POINTS (egui-free).
- ScrollAnchor / AnchorItem: a content-based scroll position (first item of the top row +
  intra-row offset) that survives row-height changes above the viewport.

Key functions:
- columns_for_width(): how many card columns fit a width.
- GridLayout::build(): builds the row table.
- GridLayout::visible_rows(): rows intersecting a vertical viewport (binary search).
- GridLayout::card_count(): thumbnail-bearing cards of a row range (sizes the thumbnail LRUs).
- GridLayout::anchor_for_offset() / offset_for_anchor(): scroll offset <-> content anchor.
- GridLayout::page_card_rect() / clean_card_rect() / gap_rect() / header_rect(): per-cell rects.
- unlink_button_rect() / label_rect(): placement of the link widgets inside a gap.

Notes:
Rows have VARIABLE height: a page row is the card height, plus `link_gap +
clean_card_h` when any page of the row has a clean; the optional bottom section is
a header row followed by rows of unassigned clean cards. Consecutive rows are
separated by `spacing[1]` (by `clean_row_spacing` below a row that carries clean
cards, so a clean card is visibly apart from the next page row), and the total
height carries no trailing spacing, which makes the clean-free case identical to
`ScrollArea::show_rows` (row `r` at
`r * (card_h + spacing_y)`, total `rows * (card_h + spacing_y) - spacing_y`).
Contains no egui code and performs no I/O, so every rule here is unit-testable.
*/

use std::ops::Range;

/// Axis-aligned rect in POINTS, relative to the grid's content origin (the
/// top-left of the scrolled content, not of the screen). egui-free so the layout
/// stays unit-testable; `grid.rs` converts it to an `egui::Rect`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct LayoutRect {
    /// Top-left corner `[x, y]`.
    pub(super) min: [f32; 2],
    /// Extent `[width, height]`; never negative for rects produced here.
    pub(super) size: [f32; 2],
}

// Test-only geometry helpers: the tests assert the layout contracts (no overlap, stacking) with
// them; drawing code converts rects to egui and never needs them.
#[cfg(test)]
impl LayoutRect {
    /// Bottom-right corner `[x, y]`.
    #[must_use]
    pub(super) fn max(&self) -> [f32; 2] {
        [self.min[0] + self.size[0], self.min[1] + self.size[1]]
    }

    /// True when the two rects share interior area (touching edges do not count).
    #[must_use]
    pub(super) fn overlaps(&self, other: &LayoutRect) -> bool {
        let (a_max, b_max) = (self.max(), other.max());
        self.min[0] < b_max[0] && other.min[0] < a_max[0] && self.min[1] < b_max[1] && other.min[1] < a_max[1]
    }
}

/// Fixed geometry of the grid in points. `card` is `[width, height]` of a page
/// card; `clean_card_h` is the height of a clean card (same width as a page
/// card); `link_gap` is the vertical gap between a page card and its clean card
/// (where the link connector is drawn); `spacing` is egui's `item_spacing`
/// `[x, y]` between cards and between rows; `clean_row_spacing` replaces
/// `spacing[1]` below a page row that carries clean cards; `section_header_h` is
/// the height of the unassigned-clean section's header row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct GridMetrics {
    pub(super) card: [f32; 2],
    pub(super) clean_card_h: f32,
    pub(super) link_gap: f32,
    pub(super) spacing: [f32; 2],
    pub(super) clean_row_spacing: f32,
    pub(super) section_header_h: f32,
}

/// What one grid row holds. Indices are into the page list (`Pages`) or into the
/// unassigned-clean list (`Unassigned`); `count` is in `1..=columns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GridRowKind {
    /// Page cards `first..first + count`; `with_clean` reserves the gap and a
    /// clean card below every column of the row (true when ANY page of the row
    /// has a clean, so all cards of a row keep a common top and bottom).
    Pages { first: usize, count: usize, with_clean: bool },
    /// Header of the bottom "clean without a page" section.
    UnassignedHeader,
    /// Unassigned clean cards `first..first + count`.
    Unassigned { first: usize, count: usize },
}

/// One row of the grid: its content and its vertical placement in content
/// coordinates. `height` excludes the spacing to the next row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct GridRow {
    pub(super) kind: GridRowKind,
    pub(super) top: f32,
    pub(super) height: f32,
}

impl GridRow {
    /// Bottom edge of the row (excluding the spacing below it).
    #[must_use]
    fn bottom(&self) -> f32 {
        self.top + self.height
    }
}

/// The grid item a scroll anchor is attached to: the first card of the row at the top of the
/// viewport (a page index, the section header, or an unassigned-clean index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnchorItem {
    Page(usize),
    UnassignedHeader,
    Unassigned(usize),
}

/// Content-based scroll position: `item`'s row top plus `offset_in_row` points is the viewport
/// top. Unlike a raw pixel offset it survives rows above the viewport changing height (a clean
/// appearing or disappearing) and column-count changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ScrollAnchor {
    pub(super) item: AnchorItem,
    pub(super) offset_in_row: f32,
}

/// The built row table of the grid. Rows are ordered and monotonic: each row's
/// `top` equals the previous row's bottom plus `metrics.spacing[1]` (plus
/// `metrics.clean_row_spacing` when the previous row carries clean cards), the first
/// row starts at 0, and `total_height` is the last row's bottom (0 when empty).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GridLayout {
    pub(super) metrics: GridMetrics,
    pub(super) columns: usize,
    pub(super) rows: Vec<GridRow>,
    pub(super) total_height: f32,
}

/// Number of card columns that fit `avail_w` points: as many `card_w`-wide cards
/// as fit with `spacing_x` between neighbours, and never fewer than one (a
/// narrow panel still shows one clipped column instead of none).
#[must_use]
pub(super) fn columns_for_width(avail_w: f32, card_w: f32, spacing_x: f32) -> usize {
    let fitting = ((avail_w + spacing_x) / (card_w + spacing_x)).floor();
    // `f32 as usize` saturates: a negative or NaN quotient (degenerate width)
    // becomes 0 and is lifted to the one-column minimum below; a huge width
    // cannot wrap. The value is a small column count, so no precision is lost.
    usize::max(1, fitting as usize)
}

impl GridLayout {
    /// Builds the row table for `page_count` pages followed by `unassigned`
    /// unassigned clean cards, fitting columns into `avail_w` points.
    ///
    /// `has_clean(page_idx)` says whether a page shows a linked clean card below
    /// it; it is queried once per page. With `unassigned == 0` the bottom section
    /// (header included) is omitted entirely.
    #[must_use]
    pub(super) fn build(metrics: &GridMetrics, avail_w: f32, page_count: usize, has_clean: impl Fn(usize) -> bool, unassigned: usize) -> Self {
        let columns = columns_for_width(avail_w, metrics.card[0], metrics.spacing[0]);
        let unassigned_rows = if unassigned == 0 { 0 } else { 1 + unassigned.div_ceil(columns) };
        let mut kinds: Vec<(GridRowKind, f32)> = Vec::with_capacity(page_count.div_ceil(columns) + unassigned_rows);

        for first in (0..page_count).step_by(columns) {
            let count = usize::min(columns, page_count - first);
            let with_clean = (first..first + count).any(&has_clean);
            let height = if with_clean { metrics.card[1] + metrics.link_gap + metrics.clean_card_h } else { metrics.card[1] };
            kinds.push((GridRowKind::Pages { first, count, with_clean }, height));
        }
        if unassigned > 0 {
            kinds.push((GridRowKind::UnassignedHeader, metrics.section_header_h));
            for first in (0..unassigned).step_by(columns) {
                let count = usize::min(columns, unassigned - first);
                kinds.push((GridRowKind::Unassigned { first, count }, metrics.clean_card_h));
            }
        }

        // Prefix sums: every row after the first is preceded by one vertical
        // spacing, matching the layout `show_rows` produced (no trailing spacing).
        // A row carrying clean cards is followed by the wider `clean_row_spacing`.
        let mut rows = Vec::with_capacity(kinds.len());
        let mut top = 0.0_f32;
        for (kind, height) in kinds {
            rows.push(GridRow { kind, top, height });
            let below = if matches!(kind, GridRowKind::Pages { with_clean: true, .. }) { metrics.clean_row_spacing } else { metrics.spacing[1] };
            top += height + below;
        }
        let total_height = rows.last().map_or(0.0, GridRow::bottom);
        Self { metrics: *metrics, columns, rows, total_height }
    }

    /// Indices of the rows that intersect the vertical band `min_y..=max_y` in
    /// content coordinates (a row touching the band counts). Returns an empty
    /// range for an empty grid or an inverted band. `O(log rows)`.
    #[must_use]
    pub(super) fn visible_rows(&self, min_y: f32, max_y: f32) -> Range<usize> {
        // Rows are sorted by `top` and do not overlap, so both predicates are
        // monotonic over the slice, which is what `partition_point` requires.
        let start = self.rows.partition_point(|row| row.bottom() < min_y);
        let end = self.rows.partition_point(|row| row.top <= max_y);
        start..end.max(start)
    }

    /// Number of thumbnail-bearing cards drawn for `rows` (indices into `self.rows`, clipped to
    /// the table): every page card, one clean slot per page card of a `with_clean` row, and every
    /// unassigned clean card. An upper bound of the thumbnails one frame requests for those rows;
    /// the thumbnail caches are sized from it (`ThumbRuntime::ensure_visible_capacity`).
    #[must_use]
    pub(super) fn card_count(&self, rows: Range<usize>) -> usize {
        let end = usize::min(rows.end, self.rows.len());
        self.rows
            .get(rows.start.min(end)..end)
            .unwrap_or_default()
            .iter()
            .map(|row| match row.kind {
                GridRowKind::Pages { count, with_clean: true, .. } => count.saturating_mul(2),
                GridRowKind::Pages { count, with_clean: false, .. } | GridRowKind::Unassigned { count, .. } => count,
                GridRowKind::UnassignedHeader => 0,
            })
            .fold(0, usize::saturating_add)
    }

    /// The content anchor at vertical scroll offset `y`: the first item of the first row whose
    /// bottom is at or below `y` (the row at the top of the viewport), plus how far `y` sits below
    /// that row's top (negative when `y` falls in the spacing above it). `None` for an empty grid;
    /// an offset past the last row anchors on the last row.
    #[must_use]
    pub(super) fn anchor_for_offset(&self, y: f32) -> Option<ScrollAnchor> {
        let start = self.rows.partition_point(|row| row.bottom() < y);
        let row = self.rows.get(start).or_else(|| self.rows.last())?;
        let item = match row.kind {
            GridRowKind::Pages { first, .. } => AnchorItem::Page(first),
            GridRowKind::UnassignedHeader => AnchorItem::UnassignedHeader,
            GridRowKind::Unassigned { first, .. } => AnchorItem::Unassigned(first),
        };
        Some(ScrollAnchor { item, offset_in_row: y - row.top })
    }

    /// The vertical scroll offset that puts `anchor` back where it was: the top of the row now
    /// holding the anchored item plus the remembered intra-row offset, clamped to that row's
    /// height (a row that lost its clean slot is shorter) and to `>= 0`. An item that no longer
    /// exists (pages or unassigned cleans were removed) resolves to the nearest surviving row of
    /// the same kind, then to the last row. `None` for an empty grid.
    #[must_use]
    pub(super) fn offset_for_anchor(&self, anchor: &ScrollAnchor) -> Option<f32> {
        let holds = |kind: GridRowKind| match (anchor.item, kind) {
            (AnchorItem::Page(page), GridRowKind::Pages { first, count, .. }) | (AnchorItem::Unassigned(page), GridRowKind::Unassigned { first, count }) => page < first + count,
            (AnchorItem::UnassignedHeader, GridRowKind::UnassignedHeader) => true,
            (AnchorItem::Page(_) | AnchorItem::UnassignedHeader | AnchorItem::Unassigned(_), _) => false,
        };
        let same_kind = |kind: GridRowKind| match (anchor.item, kind) {
            (AnchorItem::Page(_), GridRowKind::Pages { .. }) | (AnchorItem::UnassignedHeader | AnchorItem::Unassigned(_), GridRowKind::UnassignedHeader | GridRowKind::Unassigned { .. }) => true,
            (AnchorItem::Page(_) | AnchorItem::UnassignedHeader | AnchorItem::Unassigned(_), _) => false,
        };
        // Rows of one kind are contiguous and ordered by first index, so the first row whose range
        // reaches the item holds it; past the end, the kind's last row is the nearest survivor.
        let row = self
            .rows
            .iter()
            .find(|row| holds(row.kind))
            .or_else(|| self.rows.iter().rev().find(|row| same_kind(row.kind)))
            .or_else(|| self.rows.last())?;
        Some((row.top + anchor.offset_in_row.min(row.height)).max(0.0))
    }

    /// Content width the grid occupies: from the first column's left edge to the
    /// right edge of the widest row's last card (cards with spacing between them,
    /// none trailing). Fewer pages than columns therefore yield a narrower
    /// block, as the old per-row layout did. 0 for an empty grid.
    #[must_use]
    pub(super) fn content_width(&self) -> f32 {
        let widest = self
            .rows
            .iter()
            .map(|row| match row.kind {
                GridRowKind::Pages { count, .. } | GridRowKind::Unassigned { count, .. } => count,
                GridRowKind::UnassignedHeader => 0,
            })
            .max()
            .unwrap_or(0);
        match widest {
            0 => 0.0,
            cards => self.column_x(cards - 1) + self.metrics.card[0],
        }
    }

    /// Left x of column `col` in content coordinates.
    fn column_x(&self, col: usize) -> f32 {
        // `col < columns`, a small count far below 2^24: `as f32` is exact here.
        col as f32 * (self.metrics.card[0] + self.metrics.spacing[0])
    }

    /// Rect of the page card in column `col` of row `row`, or `None` when the row
    /// is not a `Pages` row or the column is past the row's last card.
    #[must_use]
    pub(super) fn page_card_rect(&self, row: usize, col: usize) -> Option<LayoutRect> {
        match self.rows.get(row)? {
            GridRow { kind: GridRowKind::Pages { count, .. }, top, .. } if col < *count => Some(LayoutRect { min: [self.column_x(col), *top], size: self.metrics.card }),
            GridRow { kind: GridRowKind::Pages { .. } | GridRowKind::UnassignedHeader | GridRowKind::Unassigned { .. }, .. } => None,
        }
    }

    /// Rect of the link gap between the page card and its clean card in column
    /// `col` of a `Pages { with_clean: true }` row: card-wide, `link_gap` tall,
    /// directly under the page card. `None` for any other row or column.
    #[must_use]
    pub(super) fn gap_rect(&self, row: usize, col: usize) -> Option<LayoutRect> {
        match self.rows.get(row)? {
            GridRow { kind: GridRowKind::Pages { count, with_clean: true, .. }, top, .. } if col < *count => {
                Some(LayoutRect { min: [self.column_x(col), top + self.metrics.card[1]], size: [self.metrics.card[0], self.metrics.link_gap] })
            }
            GridRow { kind: GridRowKind::Pages { .. } | GridRowKind::UnassignedHeader | GridRowKind::Unassigned { .. }, .. } => None,
        }
    }

    /// Rect of the clean card in column `col` of row `row`: below the link gap of
    /// a `Pages { with_clean: true }` row, or at the top of an `Unassigned` row.
    /// `None` for any other row or column. Whether a page of a `with_clean` row
    /// actually HAS a clean is the caller's question — the slot exists for every
    /// column so all cards of the row stay aligned.
    #[must_use]
    pub(super) fn clean_card_rect(&self, row: usize, col: usize) -> Option<LayoutRect> {
        let size = [self.metrics.card[0], self.metrics.clean_card_h];
        match self.rows.get(row)? {
            GridRow { kind: GridRowKind::Pages { count, with_clean: true, .. }, top, .. } if col < *count => {
                Some(LayoutRect { min: [self.column_x(col), top + self.metrics.card[1] + self.metrics.link_gap], size })
            }
            GridRow { kind: GridRowKind::Unassigned { count, .. }, top, .. } if col < *count => Some(LayoutRect { min: [self.column_x(col), *top], size }),
            GridRow { kind: GridRowKind::Pages { .. } | GridRowKind::UnassignedHeader | GridRowKind::Unassigned { .. }, .. } => None,
        }
    }

    /// Rect of an `UnassignedHeader` row, spanning `content_width()`; `None` for
    /// any other row.
    #[must_use]
    pub(super) fn header_rect(&self, row: usize) -> Option<LayoutRect> {
        match self.rows.get(row)? {
            GridRow { kind: GridRowKind::UnassignedHeader, top, height } => Some(LayoutRect { min: [0.0, *top], size: [self.content_width(), *height] }),
            GridRow { kind: GridRowKind::Pages { .. } | GridRowKind::Unassigned { .. }, .. } => None,
        }
    }
}

/// Places the unlink button of size `button_size` inside `gap`: horizontally
/// centred on the connector line, flush with the gap's top edge. Keeping the
/// button INSIDE the gap means hovering it keeps the gap hovered, so the
/// hover-revealed button cannot flicker. A button larger than the gap overhangs
/// it symmetrically sideways and downward.
#[must_use]
pub(super) fn unlink_button_rect(gap: &LayoutRect, button_size: [f32; 2]) -> LayoutRect {
    let x = gap.min[0] + (gap.size[0] - button_size[0]) * 0.5;
    LayoutRect { min: [x, gap.min[1]], size: button_size }
}

/// Places a link label of size `label_size` on the connector line: centred
/// horizontally, and vertically centred in the part of `gap` below the top
/// `top_reserved` points (the unlink button's height, see [`unlink_button_rect`]),
/// so the hover-revealed button never covers the label. An oversized label
/// overhangs symmetrically.
#[must_use]
pub(super) fn label_rect(gap: &LayoutRect, label_size: [f32; 2], top_reserved: f32) -> LayoutRect {
    let x = gap.min[0] + (gap.size[0] - label_size[0]) * 0.5;
    let y = gap.min[1] + top_reserved + (gap.size[1] - top_reserved - label_size[1]) * 0.5;
    LayoutRect { min: [x, y], size: label_size }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Today's page-card metrics (grid.rs constants) with egui's default spacing.
    fn metrics() -> GridMetrics {
        GridMetrics { card: [212.0, 276.0], clean_card_h: 200.0, link_gap: 40.0, spacing: [8.0, 3.0], clean_row_spacing: 60.0, section_header_h: 30.0 }
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-3
    }

    #[test]
    fn column_formula_matches_legacy() {
        // (avail + sx) / (w + sx): exactly 3 cards fit 3*212 + 2*8.
        assert_eq!(columns_for_width(3.0 * 212.0 + 2.0 * 8.0, 212.0, 8.0), 3);
        assert_eq!(columns_for_width(3.0 * 212.0 + 2.0 * 8.0 - 0.5, 212.0, 8.0), 2);
        assert_eq!(columns_for_width(10.0, 212.0, 8.0), 1);
        assert_eq!(columns_for_width(0.0, 212.0, 8.0), 1);
        assert_eq!(columns_for_width(-50.0, 212.0, 8.0), 1);
        assert_eq!(columns_for_width(f32::NAN, 212.0, 8.0), 1);
    }

    #[test]
    fn uniform_rows_match_show_rows() {
        let m = metrics();
        let avail = 2.0 * 212.0 + 8.0 + 5.0; // two columns
        let layout = GridLayout::build(&m, avail, 7, |_| false, 0);
        assert_eq!(layout.columns, 2);
        assert_eq!(layout.rows.len(), 4);
        let pitch = m.card[1] + m.spacing[1];
        for (r, row) in layout.rows.iter().enumerate() {
            let expected = r as f32 * pitch;
            assert!(approx(row.top, expected), "row {r}: {} != {expected}", row.top);
            assert!(approx(row.height, m.card[1]));
            assert_eq!(row.kind, GridRowKind::Pages { first: r * 2, count: usize::min(2, 7 - r * 2), with_clean: false });
        }
        // show_rows: rows * (h + sy) - sy.
        assert!(approx(layout.total_height, 4.0 * pitch - m.spacing[1]));
        assert!(approx(layout.content_width(), 2.0 * 212.0 + 8.0));
        // Last row holds one card; the second column is absent.
        assert!(layout.page_card_rect(3, 0).is_some());
        assert!(layout.page_card_rect(3, 1).is_none());
        let rect = layout.page_card_rect(1, 1).expect("row 1 has two cards");
        assert_eq!(rect.min, [212.0 + 8.0, pitch]);
        assert_eq!(rect.size, m.card);
    }

    #[test]
    fn empty_grid_has_no_rows_and_zero_height() {
        let layout = GridLayout::build(&metrics(), 1000.0, 0, |_| true, 0);
        assert!(layout.rows.is_empty());
        assert!(approx(layout.total_height, 0.0));
        assert_eq!(layout.visible_rows(0.0, 500.0), 0..0);
    }

    #[test]
    fn visible_rows_at_edges() {
        let m = metrics();
        let layout = GridLayout::build(&m, 212.0, 10, |_| false, 0); // one column, 10 rows
        let pitch = m.card[1] + m.spacing[1];
        assert_eq!(layout.visible_rows(0.0, 100.0), 0..1);
        // A band that only touches row 1's top includes it.
        assert_eq!(layout.visible_rows(0.0, pitch), 0..2);
        // A band inside the spacing between rows 0 and 1 selects nothing.
        assert_eq!(layout.visible_rows(m.card[1] + 1.0, m.card[1] + 2.0), 1..1);
        // Past the end.
        assert_eq!(layout.visible_rows(layout.total_height + 10.0, layout.total_height + 500.0), 10..10);
        // Whole range.
        assert_eq!(layout.visible_rows(-10.0, layout.total_height + 10.0), 0..10);
        // Inverted band is empty.
        let inverted = layout.visible_rows(500.0, 100.0);
        assert!(inverted.is_empty());
    }

    #[test]
    fn single_row_visibility() {
        let layout = GridLayout::build(&metrics(), 5000.0, 3, |_| false, 0);
        assert_eq!(layout.rows.len(), 1);
        assert_eq!(layout.visible_rows(0.0, 10.0), 0..1);
        assert_eq!(layout.visible_rows(300.0, 400.0), 1..1);
    }

    #[test]
    fn with_clean_and_unassigned_shapes() {
        let m = metrics();
        let avail = 3.0 * 212.0 + 2.0 * 8.0; // three columns
        // Page 4 has a clean -> row 1 (pages 3..6) carries clean slots; row 0 does not.
        let layout = GridLayout::build(&m, avail, 7, |idx| idx == 4, 4);
        assert_eq!(layout.columns, 3);
        let kinds: Vec<GridRowKind> = layout.rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GridRowKind::Pages { first: 0, count: 3, with_clean: false },
                GridRowKind::Pages { first: 3, count: 3, with_clean: true },
                GridRowKind::Pages { first: 6, count: 1, with_clean: false },
                GridRowKind::UnassignedHeader,
                GridRowKind::Unassigned { first: 0, count: 3 },
                GridRowKind::Unassigned { first: 3, count: 1 },
            ]
        );
        assert!(approx(layout.rows[1].height, m.card[1] + m.link_gap + m.clean_card_h));
        assert!(approx(layout.rows[3].height, m.section_header_h));
        assert!(approx(layout.rows[4].height, m.clean_card_h));

        // Rows are monotonic with exactly one spacing between them; the wider one only below
        // the clean-bearing row.
        for (idx, pair) in layout.rows.windows(2).enumerate() {
            let below = if idx == 1 { m.clean_row_spacing } else { m.spacing[1] };
            assert!(approx(pair[1].top, pair[0].bottom() + below));
        }
        assert!(approx(layout.total_height, layout.rows[5].bottom()));

        // No two rects of the grid overlap.
        let mut rects = Vec::new();
        for row in 0..layout.rows.len() {
            for col in 0..layout.columns {
                rects.extend(layout.page_card_rect(row, col));
                rects.extend(layout.gap_rect(row, col));
                rects.extend(layout.clean_card_rect(row, col));
            }
            rects.extend(layout.header_rect(row));
        }
        assert_eq!(rects.len(), 7 + 3 + 3 + 1 + 4);
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
            }
        }

        // Gap sits between card and clean card of the same column.
        let card = layout.page_card_rect(1, 2).expect("card");
        let gap = layout.gap_rect(1, 2).expect("gap");
        let clean = layout.clean_card_rect(1, 2).expect("clean");
        assert!(approx(gap.min[1], card.max()[1]));
        assert!(approx(clean.min[1], gap.max()[1]));
        assert!(approx(clean.max()[1], layout.rows[1].bottom()));
        // Rows without clean slots have no gap / clean rects.
        assert!(layout.gap_rect(0, 0).is_none());
        assert!(layout.clean_card_rect(0, 0).is_none());
        assert!(layout.page_card_rect(4, 0).is_none());
        assert!(layout.header_rect(0).is_none());
        let header = layout.header_rect(3).expect("header");
        assert!(approx(header.size[0], avail));
        // Fewer pages than columns: the content is only as wide as the cards.
        let narrow = GridLayout::build(&m, 2000.0, 2, |_| false, 0);
        assert!(approx(narrow.content_width(), 2.0 * 212.0 + 8.0));

        // Link widgets stay inside the gap for sizes that fit.
        let button = unlink_button_rect(&gap, [60.0, 20.0]);
        assert!(approx(button.min[1], gap.min[1]));
        assert!(approx(button.min[0] + 30.0, gap.min[0] + gap.size[0] * 0.5));
        let label = label_rect(&gap, [100.0, 18.0], 20.0);
        assert!(approx(label.min[1] + 9.0, gap.min[1] + 20.0 + (gap.size[1] - 20.0) * 0.5));
        assert!(!label.overlaps(&button));
        assert!(label.min[0] >= gap.min[0] && label.max()[0] <= gap.max()[0]);

        // Visible range over variable rows.
        let clean_row_mid = layout.rows[1].top + m.card[1] + m.link_gap + 1.0;
        assert_eq!(layout.visible_rows(clean_row_mid, clean_row_mid + 1.0), 1..2);
    }

    #[test]
    fn card_count_counts_pages_clean_slots_and_unassigned() {
        let m = metrics();
        let avail = 3.0 * 212.0 + 2.0 * 8.0; // three columns
        let layout = GridLayout::build(&m, avail, 7, |idx| idx == 4, 4);
        // Rows: 3 pages | 3 pages + 3 clean slots | 1 page | header (none) | 3 unassigned | 1 unassigned.
        assert_eq!(layout.card_count(0..layout.rows.len()), 3 + 6 + 1 + 3 + 1);
        assert_eq!(layout.card_count(1..2), 6);
        assert_eq!(layout.card_count(3..4), 0);
        // Ranges past the table are clipped, inverted ranges are empty.
        assert_eq!(layout.card_count(5..100), 1);
        assert_eq!(layout.card_count(100..200), 0);
        let (hi, lo) = (4, 2);
        assert_eq!(layout.card_count(hi..lo), 0);
    }

    #[test]
    fn anchor_round_trips_on_an_unchanged_layout() {
        let m = metrics();
        let layout = GridLayout::build(&m, 2.0 * 212.0 + 8.0, 9, |idx| idx % 3 == 0, 3);
        for y in [0.0, 10.0, 276.0, 277.5, 700.0, 1500.0, layout.total_height - 1.0] {
            let anchor = layout.anchor_for_offset(y).expect("non-empty grid");
            let back = layout.offset_for_anchor(&anchor).expect("non-empty grid");
            assert!(approx(back, y), "{y} -> {anchor:?} -> {back}");
        }
        assert_eq!(GridLayout::build(&m, 500.0, 0, |_| false, 0).anchor_for_offset(100.0), None);
    }

    #[test]
    fn anchor_keeps_the_top_page_in_place_when_rows_above_grow() {
        let m = metrics();
        // One column; the user looks 50 pt into page 5's row.
        let before = GridLayout::build(&m, 212.0, 10, |_| false, 0);
        let y = before.rows[5].top + 50.0;
        let anchor = before.anchor_for_offset(y).expect("anchor");
        assert_eq!(anchor.item, AnchorItem::Page(5));
        // Pages 1 and 3 gain a clean (the first inventory arrived): rows above page 5 grow.
        let after = GridLayout::build(&m, 212.0, 10, |idx| idx == 1 || idx == 3, 0);
        let restored = after.offset_for_anchor(&anchor).expect("offset");
        assert!(approx(restored, after.rows[5].top + 50.0));
        assert!(restored > y);
        // The reverse: those cleans go away again.
        let back = before.offset_for_anchor(&after.anchor_for_offset(restored).expect("anchor")).expect("offset");
        assert!(approx(back, y));
    }

    #[test]
    fn anchor_follows_a_page_across_a_column_change_and_clamps_to_shorter_rows() {
        let m = metrics();
        let one_col = GridLayout::build(&m, 212.0, 10, |idx| idx == 6, 0);
        // 400 pt into page 6's row: inside its clean-card slot.
        let anchor = one_col.anchor_for_offset(one_col.rows[6].top + 400.0).expect("anchor");
        assert_eq!(anchor.item, AnchorItem::Page(6));
        // Two columns: page 6 sits in row 3 (pages 6..8); the clean is gone, the row is shorter.
        let two_col = GridLayout::build(&m, 2.0 * 212.0 + 8.0, 10, |_| false, 0);
        let restored = two_col.offset_for_anchor(&anchor).expect("offset");
        assert!(approx(restored, two_col.rows[3].top + m.card[1]));
    }

    #[test]
    fn anchor_on_a_removed_item_falls_back_to_the_nearest_survivor() {
        let m = metrics();
        let before = GridLayout::build(&m, 212.0, 3, |_| false, 5);
        // Anchored on unassigned clean 4 (last row).
        let anchor = before.anchor_for_offset(before.total_height - 10.0).expect("anchor");
        assert_eq!(anchor.item, AnchorItem::Unassigned(4));
        // Two unassigned cleans got bound: their rows are gone; the last unassigned row remains.
        let fewer = GridLayout::build(&m, 212.0, 3, |_| false, 3);
        let restored = fewer.offset_for_anchor(&anchor).expect("offset");
        let last = fewer.rows.last().expect("rows");
        assert!(approx(restored, last.top + anchor.offset_in_row.min(last.height)));
        // The whole section is gone: the last page row takes over.
        let none = GridLayout::build(&m, 212.0, 3, |_| false, 0);
        let restored = none.offset_for_anchor(&anchor).expect("offset");
        assert!(approx(restored, none.rows[2].top + anchor.offset_in_row.min(none.rows[2].height)));
        // Pages deleted past the anchor: the last page row.
        let page_anchor = ScrollAnchor { item: AnchorItem::Page(8), offset_in_row: 20.0 };
        assert!(approx(none.offset_for_anchor(&page_anchor).expect("offset"), none.rows[2].top + 20.0));
        // A negative offset (the anchor sat in the spacing above row 0) never scrolls above 0.
        let above = ScrollAnchor { item: AnchorItem::Page(0), offset_in_row: -2.0 };
        assert!(approx(none.offset_for_anchor(&above).expect("offset"), 0.0));
    }
}
