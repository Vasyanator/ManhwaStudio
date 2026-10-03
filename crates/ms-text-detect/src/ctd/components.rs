/*
File: crates/ms-text-detect/src/ctd/components.rs

Purpose:
8-connected component labelling of a binary window, the `cv2.connectedComponentsWithStats` the CTD
mask refinement calls (`ctd/textmask.py:91,111`; connectivity really is 8 there, see the fixture
README quirk 6).

Key structures:
- `Components`: label image plus per-component bounding box and area.

Key functions:
- `label_8()`: the labelling.

Notes:
Labels are numbered in raster order of each component's first pixel, which differs from
OpenCV's 8-connected numbering; the refinement is independent of label order (each component's
keep test only touches its own pixels), so that is not a parity concern.
*/

/// Bounding box and area of one component; the box is `[x0, x1) x [y0, y1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ComponentStats {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub area: usize,
}

impl ComponentStats {
    /// Bounding-box area `w * h` (the `w * h < 3` skip of `merge_mask_list`).
    pub fn box_area(&self) -> usize {
        (self.x1 - self.x0) * (self.y1 - self.y0)
    }
}

/// The labelling of one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Components {
    /// Row-major labels; 0 = background (zero pixel), `k >= 1` = component `stats[k - 1]`.
    pub labels: Vec<u32>,
    /// Per-component statistics, indexed by `label - 1`.
    pub stats: Vec<ComponentStats>,
    /// Pixel count of label 0 (the zero pixels), which `OpenCV` reports as `stats[0]`.
    pub background_area: usize,
}

/// Labels the nonzero pixels of a row-major `width x height` buffer with 8-connectivity.
///
/// `mask.len()` must be `width * height`; extra bytes are ignored and missing ones read as zero.
pub(super) fn label_8(mask: &[u8], width: usize, height: usize) -> Components {
    let len = width * height;
    let set = |i: usize| mask.get(i).is_some_and(|&v| v != 0);
    let mut labels = vec![0_u32; len];
    let mut stats: Vec<ComponentStats> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut background_area = 0_usize;
    for start in 0..len {
        if !set(start) {
            background_area += 1;
            continue;
        }
        if labels[start] != 0 {
            continue;
        }
        let label = u32::try_from(stats.len() + 1).unwrap_or(u32::MAX);
        let (sx, sy) = (start % width, start / width);
        let mut comp = ComponentStats { x0: sx, y0: sy, x1: sx + 1, y1: sy + 1, area: 0 };
        labels[start] = label;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % width, i / width);
            comp.area += 1;
            comp.x0 = comp.x0.min(x);
            comp.x1 = comp.x1.max(x + 1);
            comp.y0 = comp.y0.min(y);
            comp.y1 = comp.y1.max(y + 1);
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let j = ny * width + nx;
                    if labels[j] == 0 && set(j) {
                        labels[j] = label;
                        stack.push(j);
                    }
                }
            }
        }
        stats.push(comp);
    }
    Components { labels, stats, background_area }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_pixels_join_and_stats_are_exact() {
        #[rustfmt::skip]
        let mask = [
            255, 0, 0, 0, 9,
            0, 255, 0, 0, 0,
            0, 0, 0, 7, 7,
        ];
        let c = label_8(&mask, 5, 3);
        // (0,0)-(1,1) join diagonally; (4,0) and the pair (3,2)-(4,2) are two rows apart.
        assert_eq!(c.stats.len(), 3);
        assert_eq!(c.stats[0], ComponentStats { x0: 0, y0: 0, x1: 2, y1: 2, area: 2 });
        assert_eq!(c.labels[4], 2);
        assert_eq!(c.stats[2], ComponentStats { x0: 3, y0: 2, x1: 5, y1: 3, area: 2 });
        assert_eq!(c.labels[2 * 5 + 3], 3);
        assert_eq!(c.background_area, 15 - 5);
    }

    #[test]
    fn empty_and_full_windows() {
        let none = label_8(&[0; 6], 3, 2);
        assert!(none.stats.is_empty());
        assert_eq!(none.background_area, 6);
        let full = label_8(&[1; 6], 3, 2);
        assert_eq!(full.stats, vec![ComponentStats { x0: 0, y0: 0, x1: 3, y1: 2, area: 6 }]);
        assert_eq!(full.stats[0].box_area(), 6);
    }
}
