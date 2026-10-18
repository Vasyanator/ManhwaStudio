/*
File: crates/ms-region-postprocess/src/mask.rs

Purpose:
The cv2-exact elliptical binary dilation of a region mask: the colour-match margins of this crate
(global 6 px, local 3 px) and the FLUX.2 klein latent mask the engine builds BEFORE a run.

Key items:
- `dilate_ellipse()`: public dilation, 0/255 output, radius capped at `MAX_DILATE_RADIUS_PX`.
- `ellipse_half_widths()`: the structuring element's row half-widths (crate-private, tested
  against OpenCV's kernels).

Notes:
The element is `cv2.getStructuringElement(MORPH_ELLIPSE, (2r+1, 2r+1))`: row `dy` in `-r..=r`
spans `±round_half_even(r * sqrt((r² - dy²) / r²))` (f64, cv2's `saturate_cast`), so the end rows
are a single pixel. Out-of-bounds pixels are absent (cv2's default dilate border). This is NOT
`ms_raster::dilate_square` (square) nor the text renderer's `dilate_alpha_ellipse` (exact lattice
inequality): the colour-match sample sets must equal the reference's.
*/

use crate::PostprocessError;
use rayon::prelude::*;

/// Largest radius `dilate_ellipse` accepts, px. Not in the reference: the element holds
/// `2 * radius + 1` rows and the dilation costs `O(area * (2r + 1))`, so an unbounded radius would
/// exhaust memory or time instead of returning an error. Sixteen times the FLUX.2 klein dilate
/// maximum (64 px) and the same bound as the local match's scale cap.
pub const MAX_DILATE_RADIUS_PX: u32 = 1024;

/// Dilates a row-major `width * height` mask (nonzero = set) by `OpenCV`'s `MORPH_ELLIPSE` element
/// of size `2 * radius + 1`, one iteration, out-of-bounds pixels absent.
///
/// Returns a 0/255 mask; `radius == 0` only binarizes. Cost `O(width * height * (2r + 1))`,
/// row-parallel (rayon); the result does not depend on the thread count.
///
/// # Errors
/// `PostprocessError::RadiusTooLarge` when `radius > MAX_DILATE_RADIUS_PX` (checked first,
/// before any allocation), `PostprocessError::SizeOverflow` when `width * height` overflows, and
/// `PostprocessError::ShapeMismatch` when `mask.len() != width * height`.
pub fn dilate_ellipse(mask: &[u8], width: usize, height: usize, radius: u32) -> Result<Vec<u8>, PostprocessError> {
    if radius > MAX_DILATE_RADIUS_PX {
        return Err(PostprocessError::RadiusTooLarge { radius, max: MAX_DILATE_RADIUS_PX });
    }
    let area = crate::checked_area(width, height)?;
    crate::check_len("mask", area, mask.len())?;
    let r = usize::try_from(radius).map_err(|_| PostprocessError::SizeOverflow { width, height })?;
    let half_widths = ellipse_half_widths(radius);
    if area == 0 {
        return Ok(Vec::new());
    }
    // Per input row: horizontal distance from every column to the nearest set pixel of that row,
    // saturated at `NO_SET_PIXEL` (no set pixel within reach). A row `dy` of the element with
    // half width `hw` then hits the output pixel exactly when that distance is `<= hw`; every
    // `hw` is `<= MAX_DILATE_RADIUS_PX` < `NO_SET_PIXEL`, so the saturation never changes a hit.
    let mut hdist = vec![NO_SET_PIXEL; area];
    hdist.par_chunks_mut(width).zip(mask.par_chunks(width)).for_each(|(dst, src)| row_distances(src, dst));
    let mut out = vec![0u8; area];
    // Every output row reads only `hdist`, so the rows are independent (no shared reduction).
    out.par_chunks_mut(width).enumerate().for_each(|(y, row_out)| {
        for (i, &hw) in half_widths.iter().enumerate() {
            // Source row y + (i - r); rows past either edge are absent.
            let Some(sy) = (y + i).checked_sub(r) else { continue };
            if sy >= height {
                continue;
            }
            // Always fits: `hw <= radius <= MAX_DILATE_RADIUS_PX` (checked above).
            let Ok(hw) = u16::try_from(hw) else { continue };
            let row_dist = &hdist[sy * width..(sy + 1) * width];
            for (o, &d) in row_out.iter_mut().zip(row_dist) {
                // Branch-free `if d <= hw { *o = 255 }`: 0 - 1 wraps to 255.
                *o |= 0u8.wrapping_sub(u8::from(d <= hw));
            }
        }
    });
    Ok(out)
}

/// `hdist` value of a column with no set pixel of its row within `NO_SET_PIXEL - 1` columns.
const NO_SET_PIXEL: u16 = u16::MAX;

/// Horizontal distance, saturated at `NO_SET_PIXEL`, from every column of one mask row `src`
/// to the nearest set (nonzero) pixel of that row, written into `dst` (pre-filled with
/// `NO_SET_PIXEL`, same length).
fn row_distances(src: &[u8], dst: &mut [u16]) {
    let saturate = |d: usize| u16::try_from(d).unwrap_or(NO_SET_PIXEL);
    let mut last: Option<usize> = None;
    for (x, (&m, d)) in src.iter().zip(dst.iter_mut()).enumerate() {
        if m != 0 {
            last = Some(x);
        }
        if let Some(l) = last {
            *d = saturate(x - l);
        }
    }
    last = None;
    for (x, (&m, d)) in src.iter().zip(dst.iter_mut()).enumerate().rev() {
        if m != 0 {
            last = Some(x);
        }
        if let Some(l) = last {
            *d = (*d).min(saturate(l - x));
        }
    }
}

/// Row half-widths of `OpenCV`'s `MORPH_ELLIPSE` element of size `2 * radius + 1`, rows
/// `dy = -radius..=radius` in order (`[0]` for radius 0).
pub(crate) fn ellipse_half_widths(radius: u32) -> Vec<usize> {
    if radius == 0 {
        return vec![0];
    }
    let r = i64::from(radius);
    let rf = f64::from(radius);
    let inv_r2 = 1.0 / (rf * rf);
    (-r..=r)
        .map(|dy| {
            // r² - dy² is in 0..=r² < 2^64 and exact in f64 below 2^53 (radius < 2^26 always
            // holds for a real kernel; larger radii only lose sub-pixel precision).
            let span = rf * (precise_f64(r * r - dy * dy) * inv_r2).sqrt();
            half_width_from_f64(span.round_ties_even())
        })
        .collect()
}

/// `i64` -> `f64` for kernel arithmetic (exact below 2^53).
#[expect(clippy::cast_precision_loss, reason = "r² - dy² of a kernel radius (u32) is exact below 2^53; beyond it only sub-pixel precision is lost")]
fn precise_f64(v: i64) -> f64 {
    v as f64
}

/// A rounded, non-negative half width `<= radius` (so it fits `usize`) to `usize`.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "the value is a rounded r*sqrt(t) with t in 0..=1, so 0 <= value <= radius <= u32::MAX")]
fn half_width_from_f64(v: f64) -> usize {
    v as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force dilation straight from the element definition.
    fn brute(mask: &[u8], w: usize, h: usize, radius: u32) -> Vec<u8> {
        let hw = ellipse_half_widths(radius);
        let r = i64::from(radius);
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let hit = hw.iter().enumerate().any(|(i, &half)| {
                    let Ok(sy) = usize::try_from(i64::try_from(y).unwrap_or(0) + i64::try_from(i).unwrap_or(0) - r) else { return false };
                    sy < h && (x.saturating_sub(half)..=(x + half).min(w - 1)).any(|sx| mask[sy * w + sx] != 0)
                });
                out[y * w + x] = if hit { 255 } else { 0 };
            }
        }
        out
    }

    #[test]
    fn matches_brute_force_on_a_pseudo_random_mask() -> Result<(), PostprocessError> {
        let (w, h) = (23, 17);
        let mut state = 0x1234_5678_u32;
        let mask: Vec<u8> = (0..w * h)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if state >> 28 == 0 { 7 } else { 0 }
            })
            .collect();
        for radius in [0, 1, 2, 3, 5, 9] {
            assert_eq!(dilate_ellipse(&mask, w, h, radius)?, brute(&mask, w, h, radius), "radius {radius}");
        }
        Ok(())
    }

    #[test]
    fn radius_zero_binarizes_and_bad_shapes_are_refused() -> Result<(), PostprocessError> {
        assert_eq!(dilate_ellipse(&[0, 3, 0, 255], 2, 2, 0)?, vec![0, 255, 0, 255]);
        assert!(matches!(dilate_ellipse(&[0; 5], 2, 2, 1), Err(PostprocessError::ShapeMismatch { what: "mask", expected: 4, actual: 5 })));
        assert!(matches!(dilate_ellipse(&[], usize::MAX, 2, 1), Err(PostprocessError::SizeOverflow { .. })));
        assert_eq!(dilate_ellipse(&[], 0, 0, 4)?, Vec::<u8>::new());
        Ok(())
    }

    /// The cap itself is accepted; one more is refused before the kernel or any buffer is built,
    /// whatever the radius.
    #[test]
    fn the_radius_is_capped() -> Result<(), PostprocessError> {
        assert_eq!(dilate_ellipse(&[255], 1, 1, MAX_DILATE_RADIUS_PX)?, vec![255]);
        for radius in [MAX_DILATE_RADIUS_PX + 1, u32::MAX] {
            assert_eq!(dilate_ellipse(&[255], 1, 1, radius), Err(PostprocessError::RadiusTooLarge { radius, max: MAX_DILATE_RADIUS_PX }));
        }
        Ok(())
    }

    /// A row longer than the `u16` distance range: distances saturate without moving a hit.
    #[test]
    fn rows_longer_than_the_distance_range_saturate_safely() -> Result<(), PostprocessError> {
        let width = usize::from(u16::MAX) + 1000;
        let mut mask = vec![0u8; width * 2];
        mask[0] = 1;
        mask[2 * width - 1] = 1;
        let out = dilate_ellipse(&mask, width, 2, 3)?;
        let set: Vec<usize> = out.iter().enumerate().filter(|(_, v)| **v == 255).map(|(i, _)| i).collect();
        assert_eq!(set, brute(&mask, width, 2, 3).iter().enumerate().filter(|(_, v)| **v == 255).map(|(i, _)| i).collect::<Vec<_>>());
        assert!(set.contains(&0) && set.contains(&(2 * width - 1)) && set.len() == 16, "4 columns on both rows around each pixel");
        Ok(())
    }

    #[test]
    fn r6_kernel_is_opencvs() {
        assert_eq!(ellipse_half_widths(6), vec![0, 3, 4, 5, 6, 6, 6, 6, 6, 5, 4, 3, 0]);
        assert_eq!(ellipse_half_widths(1), vec![0, 1, 0]);
    }
}
