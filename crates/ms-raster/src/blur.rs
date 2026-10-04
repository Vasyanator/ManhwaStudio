/*
File: crates/ms-raster/src/blur.rs

Purpose:
The project's box blur of a single-channel `u8` plane (feathering a 0/255 edit mask into a
blend alpha).

Key functions:
- `box_blur_u8()`: separable (2r+1) x (2r+1) mean with clamp-to-edge borders.

Notes:
Clamp-to-edge means an out-of-bounds sample reads the nearest edge pixel, so the divisor is
always `(2r+1)^2` and a constant plane stays exactly constant, edges included. Both passes keep
exact integer sums (`u64`) and round once at the end (half up), so the result equals the 2-D
box mean of the clamped plane bit for bit. Each pass slides a running sum, so the cost is
O(width * height) for any radius; a pixel farther than `r` (Chebyshev) from every nonzero pixel
stays 0, which the feathered composite relies on to keep unpainted pixels untouched.
*/

use crate::RasterError;

/// Box-blurs `src` (`width * height`, row-major, one byte per pixel) with a square window of
/// half-size `radius`: output pixel `(x, y)` is the mean of the `(2r+1)^2` source samples at
/// `(clamp(x+dx), clamp(y+dy))`, `|dx|, |dy| <= r`, rounded half up. The output has the same
/// length and layout as `src`. `radius == 0` returns an identical copy; radii larger than the
/// plane are allowed; an empty plane with a zero-area size returns an empty `Vec`.
///
/// # Errors
/// - `RasterError::LengthMismatch` when `src.len() != width * height` (including when the
///   product overflows `usize`).
/// - `RasterError::RadiusTooLarge` when `255 * (2r+1)^2` does not fit in `u64` (about
///   `r >= 2^27`), so the exact window sum cannot be represented.
pub fn box_blur_u8(src: &[u8], width: usize, height: usize, radius: usize) -> Result<Vec<u8>, RasterError> {
    let mismatch = RasterError::LengthMismatch { width, height, len: src.len() };
    let area = width.checked_mul(height).ok_or_else(|| mismatch.clone())?;
    if area != src.len() {
        return Err(mismatch);
    }
    if area == 0 || radius == 0 {
        return Ok(src.to_vec());
    }
    let too_large = || RasterError::RadiusTooLarge { radius };
    let radius_u64 = u64::try_from(radius).map_err(|_| too_large())?;
    let window = radius_u64.checked_mul(2).and_then(|v| v.checked_add(1)).ok_or_else(too_large)?;
    let divisor = window.checked_mul(window).ok_or_else(too_large)?;
    // Bound of every running sum below; checking it once makes the unchecked additions safe.
    divisor.checked_mul(255).ok_or_else(too_large)?;

    // Pass 1: horizontal window sums (exact, not divided) of every row.
    let mut row_sums = vec![0u64; area];
    for (src_row, dst_row) in src.chunks_exact(width).zip(row_sums.chunks_exact_mut(width)) {
        let last = width - 1;
        // Window at x = 0 covers indices -r..=r: r clamped copies of the first pixel, the
        // in-bounds prefix 0..=min(r, last), and the indices past `last` clamped to the last pixel.
        let reach = radius.min(last);
        let beyond = u64::try_from(radius - reach).map_err(|_| too_large())?;
        let mut sum = radius_u64 * u64::from(src_row[0])
            + src_row[..=reach].iter().map(|&px| u64::from(px)).sum::<u64>()
            + beyond * u64::from(src_row[last]);
        for (x, slot) in dst_row.iter_mut().enumerate() {
            *slot = sum;
            // Slide to x + 1: index x + 1 + r enters, index x - r leaves (both clamped).
            let entering = x.saturating_add(1).saturating_add(radius).min(last);
            let leaving = x.saturating_sub(radius);
            sum = sum + u64::from(src_row[entering]) - u64::from(src_row[leaving]);
        }
    }

    // Pass 2: vertical window sums of the row sums, one whole row at a time (cache friendly).
    let row = |y: usize| &row_sums[y * width..(y + 1) * width];
    let last = height - 1;
    let reach = radius.min(last);
    let beyond = u64::try_from(radius - reach).map_err(|_| too_large())?;
    let mut column_sums: Vec<u64> = row(0).iter().zip(row(last)).map(|(&first, &end)| radius_u64 * first + beyond * end).collect();
    for y in 0..=reach {
        for (sum, &value) in column_sums.iter_mut().zip(row(y)) {
            *sum += value;
        }
    }
    let half = divisor / 2;
    let mut out = Vec::with_capacity(area);
    for y in 0..height {
        // A rounded mean of `u8` samples is at most 255, so the saturating fallback never fires.
        out.extend(column_sums.iter().map(|&sum| u8::try_from((sum + half) / divisor).unwrap_or(u8::MAX)));
        let entering = row(y.saturating_add(1).saturating_add(radius).min(last));
        let leaving = row(y.saturating_sub(radius));
        for ((sum, &enter), &leave) in column_sums.iter_mut().zip(entering).zip(leaving) {
            *sum = *sum + enter - leave;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force reference: the 2-D clamped-window mean, rounded half up.
    fn reference(src: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
        let clamp = |v: isize, len: usize| usize::try_from(v.max(0)).unwrap_or(0).min(len - 1);
        let r = isize::try_from(radius).unwrap_or(0);
        let window = 2 * radius + 1;
        let divisor = u64::try_from(window * window).unwrap_or(1);
        let mut out = Vec::with_capacity(src.len());
        for y in 0..height {
            for x in 0..width {
                let (xi, yi) = (isize::try_from(x).unwrap_or(0), isize::try_from(y).unwrap_or(0));
                let mut sum = 0u64;
                for dy in -r..=r {
                    for dx in -r..=r {
                        sum += u64::from(src[clamp(yi + dy, height) * width + clamp(xi + dx, width)]);
                    }
                }
                out.push(u8::try_from((sum + divisor / 2) / divisor).unwrap_or(u8::MAX));
            }
        }
        out
    }

    fn noise(len: usize, mut state: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                u8::try_from(state >> 56).unwrap_or(0)
            })
            .collect()
    }

    #[test]
    fn matches_brute_force_reference() {
        let sizes = [(1usize, 1usize), (1, 7), (7, 1), (2, 3), (9, 5), (13, 11)];
        let mut seed = 0x0b1_u64;
        for &(width, height) in &sizes {
            for radius in [1usize, 2, 3, 6, 15] {
                seed += 1;
                let src = noise(width * height, seed);
                let got = box_blur_u8(&src, width, height, radius).expect("length matches");
                assert_eq!(got, reference(&src, width, height, radius), "{width}x{height} r={radius}");
            }
        }
    }

    #[test]
    fn radius_zero_is_identity() {
        let src = noise(6 * 4, 3);
        assert_eq!(box_blur_u8(&src, 6, 4, 0).expect("length matches"), src);
    }

    #[test]
    fn constant_plane_stays_constant() {
        for value in [0u8, 1, 128, 255] {
            let src = vec![value; 7 * 5];
            for radius in [1usize, 2, 10, 1000] {
                assert_eq!(box_blur_u8(&src, 7, 5, radius).expect("length matches"), src, "value {value} r={radius}");
            }
        }
    }

    #[test]
    fn pixels_farther_than_radius_from_paint_stay_zero() {
        let (width, height, radius) = (20usize, 20usize, 3usize);
        let mut src = vec![0u8; width * height];
        src[10 * width + 10] = 255;
        let got = box_blur_u8(&src, width, height, radius).expect("length matches");
        for y in 0..height {
            for x in 0..width {
                if x.abs_diff(10) > radius || y.abs_diff(10) > radius {
                    assert_eq!(got[y * width + x], 0, "({x}, {y})");
                }
            }
        }
        assert!(got[10 * width + 10] > 0);
    }

    #[test]
    fn length_contract() {
        let src = noise(5 * 3, 11);
        assert_eq!(box_blur_u8(&src, 5, 3, 2).expect("length matches").len(), src.len());
        assert_eq!(box_blur_u8(&[], 0, 4, 2).expect("zero area"), Vec::<u8>::new());
        assert_eq!(box_blur_u8(&src, 4, 3, 2), Err(RasterError::LengthMismatch { width: 4, height: 3, len: 15 }));
        assert_eq!(box_blur_u8(&src, usize::MAX, 2, 2), Err(RasterError::LengthMismatch { width: usize::MAX, height: 2, len: 15 }));
    }

    #[test]
    fn unrepresentable_radius_is_an_error() {
        let src = [7u8; 4];
        assert_eq!(box_blur_u8(&src, 2, 2, usize::MAX), Err(RasterError::RadiusTooLarge { radius: usize::MAX }));
    }
}
