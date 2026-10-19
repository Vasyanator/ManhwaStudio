/*
File: crates/ms-raster/src/fill.rs

Purpose:
The one "fill what the mask encloses" rule of the project (hole filling of a binary mask).

Key functions:
- `fill_enclosed()`: sets every unset pixel that no 4-connected path of unset pixels links to the
  buffer's border.

Notes:
The background is flooded from the border with a scanline (span) fill: each popped seed is
extended left and right along its row, and only one seed per run of unvisited background is
pushed for the rows above and below. That keeps the explicit stack proportional to the number of
runs rather than to the area (a pixel-per-entry stack reaches megabytes on an empty megapixel
mask) and never recurses. Background connectivity is 4, so a stroke that is closed only
diagonally (8-connected) still counts as a wall. Pixels outside the buffer are background, which
is why anything touching the border is never enclosed.
*/

use crate::RasterError;

/// Fills every enclosed hole of `mask` (`width * height`, row-major, nonzero = set) in place
/// and returns how many pixels it newly set.
///
/// An unset (zero) pixel is ENCLOSED when no 4-connected path of unset pixels leads from it to
/// the buffer's border; every enclosed pixel becomes `255`. Pixels that were already set keep
/// their value, and unset pixels reachable from the border stay `0`. The area outside the
/// buffer counts as background, so a loop closed only by the image edge encloses nothing. A
/// zero-area buffer returns `Ok(0)`. Allocates one `bool` per pixel plus a seed stack that grows
/// with the number of background runs, not with the area. O(w*h).
///
/// # Errors
/// `RasterError::LengthMismatch` when `mask.len() != width * height` (including when the product
/// overflows `usize`); the mask is then left untouched.
pub fn fill_enclosed(mask: &mut [u8], width: usize, height: usize) -> Result<usize, RasterError> {
    let mismatch = RasterError::LengthMismatch { width, height, len: mask.len() };
    let area = width.checked_mul(height).ok_or_else(|| mismatch.clone())?;
    if area != mask.len() {
        return Err(mismatch);
    }
    if area == 0 {
        return Ok(0);
    }

    // `outside[i]`: pixel `i` is background reachable from the border. Every index below is
    // `y * width + x` with `x < width`, `y < height`, so it is `< area` and cannot overflow.
    let mut outside = vec![false; area];
    let open = |mask: &[u8], outside: &[bool], idx: usize| mask[idx] == 0 && !outside[idx];

    let mut seeds: Vec<(usize, usize)> = Vec::with_capacity(width.saturating_add(height).saturating_mul(2));
    for x in 0..width {
        seeds.push((x, 0));
        seeds.push((x, height - 1));
    }
    for y in 0..height {
        seeds.push((0, y));
        seeds.push((width - 1, y));
    }

    while let Some((x, y)) = seeds.pop() {
        let row = y * width;
        if !open(mask, &outside, row + x) {
            continue;
        }
        // Extend the seed to the whole run of open pixels on its row.
        let mut left = x;
        while left > 0 && open(mask, &outside, row + left - 1) {
            left -= 1;
        }
        let mut right = x;
        while right + 1 < width && open(mask, &outside, row + right + 1) {
            right += 1;
        }
        outside[row + left..=row + right].fill(true);

        // 4-connectivity: the run's neighbours are exactly columns `left..=right` of the rows
        // directly above and below. One seed per open run there; the pop re-extends it.
        let above = y.checked_sub(1);
        let below = (y + 1 < height).then_some(y + 1);
        for ny in [above, below].into_iter().flatten() {
            let nrow = ny * width;
            let mut in_run = false;
            for nx in left..=right {
                let is_open = open(mask, &outside, nrow + nx);
                if is_open && !in_run {
                    seeds.push((nx, ny));
                }
                in_run = is_open;
            }
        }
    }

    let mut filled = 0usize;
    for (px, &reached) in mask.iter_mut().zip(&outside) {
        if *px == 0 && !reached {
            *px = 255;
            filled += 1;
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w x h` mask with `value` at each `(x, y)` of `set`.
    fn mask_with(w: usize, h: usize, set: &[(usize, usize)], value: u8) -> Vec<u8> {
        let mut mask = vec![0u8; w * h];
        for &(x, y) in set {
            mask[y * w + x] = value;
        }
        mask
    }

    /// The 3x3 ring around `(cx, cy)`: eight wall pixels enclosing the centre.
    fn ring(cx: usize, cy: usize) -> Vec<(usize, usize)> {
        let mut walls = Vec::new();
        for y in cy - 1..=cy + 1 {
            for x in cx - 1..=cx + 1 {
                if (x, y) != (cx, cy) {
                    walls.push((x, y));
                }
            }
        }
        walls
    }

    /// The pre-move per-pixel flood of `ms-tab-cleaning`'s `autoclean::fill_holes`, kept as the
    /// reference the span fill must agree with pixel for pixel.
    fn reference_fill(mask: &mut [u8], w: usize, h: usize) -> usize {
        let mut outside = vec![false; w * h];
        let mut stack = Vec::new();
        let push = |x: usize, y: usize, outside: &mut [bool], stack: &mut Vec<(usize, usize)>, mask: &[u8]| {
            let idx = y * w + x;
            if !outside[idx] && mask[idx] == 0 {
                outside[idx] = true;
                stack.push((x, y));
            }
        };
        for x in 0..w {
            push(x, 0, &mut outside, &mut stack, mask);
            push(x, h - 1, &mut outside, &mut stack, mask);
        }
        for y in 0..h {
            push(0, y, &mut outside, &mut stack, mask);
            push(w - 1, y, &mut outside, &mut stack, mask);
        }
        while let Some((x, y)) = stack.pop() {
            if x > 0 {
                push(x - 1, y, &mut outside, &mut stack, mask);
            }
            if x + 1 < w {
                push(x + 1, y, &mut outside, &mut stack, mask);
            }
            if y > 0 {
                push(x, y - 1, &mut outside, &mut stack, mask);
            }
            if y + 1 < h {
                push(x, y + 1, &mut outside, &mut stack, mask);
            }
        }
        let mut filled = 0;
        for (px, reached) in mask.iter_mut().zip(outside) {
            if *px == 0 && !reached {
                *px = 255;
                filled += 1;
            }
        }
        filled
    }

    #[test]
    fn an_enclosed_hole_is_filled_and_counted() {
        let mut mask = mask_with(5, 5, &ring(2, 2), 255);
        assert_eq!(fill_enclosed(&mut mask, 5, 5), Ok(1));
        assert_eq!(mask[2 * 5 + 2], 255);
        assert_eq!(mask.iter().filter(|&&px| px != 0).count(), 9);
    }

    #[test]
    fn an_open_bay_is_not_filled() {
        // The ring with its right-middle wall pixel removed: the centre leaks out.
        let walls: Vec<_> = ring(2, 2).into_iter().filter(|&p| p != (3, 2)).collect();
        let mut mask = mask_with(5, 5, &walls, 255);
        let before = mask.clone();
        assert_eq!(fill_enclosed(&mut mask, 5, 5), Ok(0));
        assert_eq!(mask, before);
    }

    #[test]
    fn a_loop_closed_only_by_the_border_encloses_nothing() {
        // A "U" whose open side is the top edge of the buffer.
        let walls = [(1, 0), (1, 1), (1, 2), (2, 2), (3, 2), (3, 1), (3, 0)];
        let mut mask = mask_with(5, 4, &walls, 255);
        assert_eq!(fill_enclosed(&mut mask, 5, 4), Ok(0));
        assert_eq!(mask[5 + 2], 0, "the pocket touches the border, so it is background");
    }

    #[test]
    fn a_diagonal_wall_closes_the_hole() {
        // A diamond: its walls touch only at corners (8-connected), which still blocks the
        // 4-connected background flood.
        let walls = [(2, 0), (1, 1), (3, 1), (0, 2), (4, 2), (1, 3), (3, 3), (2, 4)];
        let mut mask = mask_with(5, 5, &walls, 255);
        // Inside the diamond: (2,1), (1,2), (2,2), (3,2), (2,3).
        assert_eq!(fill_enclosed(&mut mask, 5, 5), Ok(5));
        assert_eq!(mask[2 * 5 + 2], 255);
    }

    #[test]
    fn already_set_values_are_preserved() {
        let mut mask = mask_with(5, 5, &ring(2, 2), 7);
        assert_eq!(fill_enclosed(&mut mask, 5, 5), Ok(1));
        assert_eq!(mask[2 * 5 + 2], 255, "a filled pixel becomes 255");
        for (x, y) in ring(2, 2) {
            assert_eq!(mask[y * 5 + x], 7, "a wall keeps its own value");
        }
    }

    #[test]
    fn a_large_hole_and_two_separate_holes_are_filled() {
        // A 10x8 buffer with a box from (1,1) to (6,6) (hole 4x4) and a ring at (8,3).
        let (w, h) = (10, 8);
        let mut walls = Vec::new();
        for i in 1..=6 {
            walls.extend([(i, 1), (i, 6), (1, i), (6, i)]);
        }
        walls.extend(ring(8, 3));
        let mut mask = mask_with(w, h, &walls, 255);
        assert_eq!(fill_enclosed(&mut mask, w, h), Ok(16 + 1));
    }

    #[test]
    fn a_wrong_length_is_refused_and_leaves_the_mask_untouched() {
        let mut mask = vec![0u8, 255, 0];
        assert_eq!(fill_enclosed(&mut mask, 2, 2), Err(RasterError::LengthMismatch { width: 2, height: 2, len: 3 }));
        assert_eq!(mask, [0, 255, 0]);
        let mut empty: Vec<u8> = Vec::new();
        assert!(matches!(fill_enclosed(&mut empty, usize::MAX, 2), Err(RasterError::LengthMismatch { .. })));
    }

    #[test]
    fn empty_and_single_pixel_buffers_fill_nothing() {
        let mut empty: Vec<u8> = Vec::new();
        assert_eq!(fill_enclosed(&mut empty, 0, 0), Ok(0));
        assert_eq!(fill_enclosed(&mut empty, 0, 7), Ok(0));
        let mut one = vec![0u8];
        assert_eq!(fill_enclosed(&mut one, 1, 1), Ok(0));
        assert_eq!(one, [0]);
        let mut one_set = vec![9u8];
        assert_eq!(fill_enclosed(&mut one_set, 1, 1), Ok(0));
        assert_eq!(one_set, [9]);
    }

    /// The span fill agrees pixel for pixel with the pre-move per-pixel flood on random masks
    /// of several densities and shapes, including 1-wide and 1-tall buffers.
    #[test]
    fn matches_the_pre_move_per_pixel_flood() {
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        for &(w, h) in &[(1, 1), (1, 9), (9, 1), (7, 5), (16, 16), (33, 21), (64, 48)] {
            for percent in [10u64, 35, 50, 65, 90] {
                let source: Vec<u8> = (0..w * h).map(|_| if next() % 100 < percent { 255 } else { 0 }).collect();
                let mut expected = source.clone();
                let expected_count = reference_fill(&mut expected, w, h);
                let mut actual = source;
                assert_eq!(fill_enclosed(&mut actual, w, h), Ok(expected_count), "{w}x{h} at {percent}%");
                assert_eq!(actual, expected, "{w}x{h} at {percent}%");
            }
        }
    }
}
