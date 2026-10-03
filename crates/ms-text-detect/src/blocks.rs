/*
File: crates/ms-text-detect/src/blocks.rs

Purpose:
Detected-block rules with one owner: the axis-aligned block rectangle, the reading-order sort
and the cap on the number of detected blocks (sort, then truncate).

Key items:
- `DetectRect`: detected text block in source-image pixels (xyxy, finite, non-empty).
- `sort_reading_order()`: the one block order, `(y1, x1, y2, x2)` by `f32::total_cmp`.
- `MAX_DETECTED_BLOCKS` / `finalize_blocks()`: sort, then keep at most 2500 blocks.

Notes:
Every detector path (classic, native Paddle, backend response) and the translation tab's
merge/storage reload use these, so a page's block order never depends on the engine.
*/

use std::cmp::Ordering;

/// Upper bound on the blocks a detector result keeps; the rest are dropped after sorting.
pub const MAX_DETECTED_BLOCKS: usize = 2500;

/// A detected text block in source-image pixels: `(x1, y1)` top-left, `(x2, y2)` bottom-right.
///
/// Built through [`DetectRect::from_xyxy`], which guarantees finite coordinates and
/// `x2 > x1`, `y2 > y1`. The fields stay public for read access by the tab's block tools.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectRect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl DetectRect {
    /// Returns the rectangle when all four coordinates are finite and it has a positive
    /// width and height; `None` otherwise (NaN/inf, empty or inverted boxes).
    #[must_use]
    pub fn from_xyxy(x1: f32, y1: f32, x2: f32, y2: f32) -> Option<Self> {
        if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
            return None;
        }
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Self { x1, y1, x2, y2 })
    }
}

/// The reading-order comparator: top edge, then left edge, then bottom, then right.
///
/// `f32::total_cmp` makes it a total order (it orders `-0.0` before `0.0`), so the sort is
/// deterministic for every input the constructor admits.
fn reading_order(a: &DetectRect, b: &DetectRect) -> Ordering {
    a.y1.total_cmp(&b.y1)
        .then_with(|| a.x1.total_cmp(&b.x1))
        .then_with(|| a.y2.total_cmp(&b.y2))
        .then_with(|| a.x2.total_cmp(&b.x2))
}

/// Sorts `blocks` into reading order `(y1, x1, y2, x2)`. Stable: fully equal blocks keep
/// their relative input order.
pub fn sort_reading_order(blocks: &mut [DetectRect]) {
    blocks.sort_by(reading_order);
}

/// Finalizes a detector block list: reading-order sort, then truncation to
/// [`MAX_DETECTED_BLOCKS`] (the blocks dropped are the last in reading order).
pub fn finalize_blocks(blocks: &mut Vec<DetectRect>) {
    sort_reading_order(blocks);
    blocks.truncate(MAX_DETECTED_BLOCKS);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> DetectRect {
        DetectRect::from_xyxy(x1, y1, x2, y2).unwrap_or_else(|| panic!("valid test rect {x1} {y1} {x2} {y2}"))
    }

    fn arrays(blocks: &[DetectRect]) -> Vec<[f32; 4]> {
        blocks.iter().map(|r| [r.x1, r.y1, r.x2, r.y2]).collect()
    }

    #[test]
    fn from_xyxy_rejects_non_finite_and_empty_boxes() {
        assert!(DetectRect::from_xyxy(f32::NAN, 0.0, 1.0, 1.0).is_none());
        assert!(DetectRect::from_xyxy(0.0, 0.0, f32::INFINITY, 1.0).is_none());
        assert!(DetectRect::from_xyxy(0.0, 0.0, 0.0, 1.0).is_none(), "zero width");
        assert!(DetectRect::from_xyxy(0.0, 1.0, 1.0, 1.0).is_none(), "zero height");
        assert!(DetectRect::from_xyxy(2.0, 0.0, 1.0, 1.0).is_none(), "inverted");
        assert!(DetectRect::from_xyxy(-3.0, -2.0, -1.0, 0.5).is_some());
    }

    #[test]
    fn sort_orders_by_y1_x1_y2_x2_with_total_cmp() {
        let mut blocks = vec![
            rect(5.0, 10.0, 9.0, 20.0),
            rect(5.0, 10.0, 9.0, 18.0),
            rect(3.0, 10.0, 8.0, 20.0),
            rect(5.0, 10.0, 7.0, 18.0),
            rect(0.0, 2.0, 4.0, 6.0),
            rect(-0.0, 2.0, 4.0, 6.0),
            rect(5.0, 0.0, 6.0, 1.0),
            rect(5.0, -0.0, 6.0, 1.0),
        ];
        sort_reading_order(&mut blocks);
        assert_eq!(
            arrays(&blocks),
            vec![
                [5.0, -0.0, 6.0, 1.0],
                [5.0, 0.0, 6.0, 1.0],
                [-0.0, 2.0, 4.0, 6.0],
                [0.0, 2.0, 4.0, 6.0],
                [3.0, 10.0, 8.0, 20.0],
                [5.0, 10.0, 7.0, 18.0],
                [5.0, 10.0, 9.0, 18.0],
                [5.0, 10.0, 9.0, 20.0],
            ]
        );
        assert!(blocks[0].y1.is_sign_negative(), "-0.0 sorts before 0.0");
        assert!(blocks[2].x1.is_sign_negative(), "-0.0 sorts before 0.0");
    }

    #[test]
    fn finalize_keeps_short_lists_whole_and_sorted() {
        let mut blocks = vec![rect(0.0, 9.0, 1.0, 10.0), rect(0.0, 1.0, 1.0, 2.0)];
        finalize_blocks(&mut blocks);
        assert_eq!(arrays(&blocks), vec![[0.0, 1.0, 1.0, 2.0], [0.0, 9.0, 1.0, 10.0]]);
    }

    #[test]
    fn finalize_truncates_to_the_first_blocks_in_reading_order() {
        // Reverse order, one block more than the cap: the block with the largest y1 is dropped.
        let mut blocks = (0..=MAX_DETECTED_BLOCKS)
            .rev()
            .map(|i| {
                let y = f32::from(u16::try_from(i).unwrap_or(u16::MAX));
                rect(0.0, y, 1.0, y + 1.0)
            })
            .collect::<Vec<_>>();
        finalize_blocks(&mut blocks);
        assert_eq!(blocks.len(), MAX_DETECTED_BLOCKS);
        assert_eq!(arrays(&blocks[..1]), vec![[0.0, 0.0, 1.0, 1.0]]);
        let last = f32::from(u16::try_from(MAX_DETECTED_BLOCKS - 1).unwrap_or(u16::MAX));
        assert_eq!(arrays(&blocks[MAX_DETECTED_BLOCKS - 1..]), vec![[0.0, last, 1.0, last + 1.0]]);
    }
}
