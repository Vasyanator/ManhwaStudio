/*
File: crates/ms-region-postprocess/src/feather.rs

Purpose:
The inward feather: blend weights that rise from 0 on the mask contour to 255 `feather_px` px
inside the mask, and the distance field they are built on.

Key functions:
- `inside_distance()`: Euclidean distance of every masked pixel to the nearest unmasked one,
  the region border counting as unmasked.
- `feather_alpha()`: the smoothstep ramp over that distance, 0 outside the mask.

Notes:
Deliberate deviation from the Python reference: the distance is the EXACT Euclidean transform
(`ms_raster::squared_distance_to_mask`), not cv2's 5x5 chamfer (`DIST_L2, 5`), which
underestimates diagonal distances by up to ~2 %. The ramp is a few levels different at most.
The transform itself is sequential (`ms-raster` is a std-only foundation crate); the pipeline
runs it concurrently with the colour match instead. The per-pixel passes here are rayon-parallel.
*/

use ms_raster::squared_distance_to_mask;
use rayon::prelude::*;

use crate::PostprocessError;

/// Distance, px, from every pixel of a row-major `width * height` mask (nonzero = masked) to
/// the nearest unmasked pixel; 0 outside the mask. The caller has validated
/// `mask.len() == width * height`.
///
/// Everything beyond the region's border counts as unmasked (a one-pixel zero ring is added
/// before measuring): the region is a window onto a larger page whose pixels past the edge this
/// edit may not change, so the region border is a contour the feather ramps in from too —
/// otherwise a mask painted up to the edge meets the page with a hard step and a whole-region
/// mask gets no feather at all.
///
/// # Errors
/// `PostprocessError::SizeOverflow` when the padded size overflows `usize`.
pub(crate) fn inside_distance(mask: &[u8], width: usize, height: usize) -> Result<Vec<f32>, PostprocessError> {
    let overflow = PostprocessError::SizeOverflow { width, height };
    let pw = width.checked_add(2).ok_or_else(|| overflow.clone())?;
    let ph = height.checked_add(2).ok_or_else(|| overflow.clone())?;
    let padded_area = pw.checked_mul(ph).ok_or_else(|| overflow.clone())?;
    // Seeds (nonzero) are the UNMASKED pixels and the ring; the transform measures to them.
    let mut seeds = vec![1u8; padded_area];
    for (y, row) in mask.chunks_exact(width.max(1)).take(height).enumerate() {
        let start = (y + 1) * pw + 1;
        for (s, &m) in seeds[start..start + width].iter_mut().zip(row) {
            *s = u8::from(m == 0);
        }
    }
    // The ring guarantees a seed, so no pixel stays at the unreachable sentinel.
    let squared = squared_distance_to_mask(&seeds, pw, ph).map_err(|_| overflow)?;
    let mut out = vec![0.0f32; width * height];
    if width > 0 {
        out.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            let start = (y + 1) * pw + 1;
            for (o, d) in row.iter_mut().zip(&squared[start..start + width]) {
                *o = d.sqrt();
            }
        });
    }
    Ok(out)
}

/// Inward feather weights (0..=255) over `distance` (from [`inside_distance`]) for a ramp of
/// `feather_px` px; `mask` (nonzero = masked) decides inside.
///
/// `feather_px == 0` is a hard edge (255 inside, 0 outside). Otherwise the ramp width is
/// `min(feather_px, max distance)` — a mask thinner than the ramp compresses it to its own
/// half-width instead of never reaching full strength — `t = clamp(d / width, 0, 1)`,
/// `s = t²(3 - 2t)` and the weight is `floor(s * 255 + 0.5)` in f32, as the reference computes
/// it; pixels outside the mask are always 0.
pub(crate) fn feather_alpha(mask: &[u8], distance: &[f32], feather_px: u32) -> Vec<u8> {
    if feather_px == 0 {
        return mask.par_iter().map(|&m| if m != 0 { 255 } else { 0 }).collect();
    }
    // `max` over finite non-negative values is exact and order-free, so a parallel reduction
    // gives the sequential value.
    let reach = distance.par_iter().copied().reduce(|| 0.0f32, f32::max);
    if reach <= 0.0 {
        // An empty mask: nothing is blended.
        return vec![0; mask.len()];
    }
    // The reference takes `min(float(feather_px), reach)` in f64 and numpy casts it to f32 for
    // the division; `reach` is an f32 and `feather_px <= 32`, so the min is exact in f32.
    let width = feather_to_f32(feather_px).min(reach);
    mask.par_iter()
        .zip(distance)
        .map(|(&m, &d)| {
            if m == 0 {
                return 0;
            }
            let t = (d / width).clamp(0.0, 1.0);
            let smooth = t * t * (3.0 - 2.0 * t);
            truncate_to_u8(smooth * 255.0 + 0.5)
        })
        .collect()
}

/// A feather width (`<= FEATHER_MAX_PX` after normalization) to f32.
#[expect(clippy::cast_precision_loss, reason = "a feather width is a small integer (<= 32 after normalization); larger values only matter as `>= reach`")]
fn feather_to_f32(feather_px: u32) -> f32 {
    feather_px as f32
}

/// numpy `astype(uint8)` of a value in `0.5..=255.5`: truncation toward zero.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "the value lies in 0.5..=255.5; `as` truncates (the reference's astype) and saturates at 255")]
fn truncate_to_u8(v: f32) -> u8 {
    v as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_is_a_contour_and_outside_is_zero() -> Result<(), PostprocessError> {
        let mask = vec![255u8; 7];
        let d = inside_distance(&mask, 7, 1)?;
        assert_eq!(d, vec![1.0; 7], "a one-row strip is one pixel from the ring above and below");
        let (w, h) = (9, 9);
        let full = vec![255u8; w * h];
        let d = inside_distance(&full, w, h)?;
        assert!((d[4 * w + 4] - 5.0).abs() < f32::EPSILON, "the centre is 5 px from the ring");
        let alpha = feather_alpha(&full, &d, 12);
        assert_eq!(alpha[4 * w + 4], 255, "the ramp compresses to the reach");
        assert!(alpha[0] < 40, "the corner sits on the ramp's start");
        let mut part = full.clone();
        part[0] = 0;
        let d = inside_distance(&part, w, h)?;
        assert_eq!(d[0].to_bits(), 0);
        assert_eq!(feather_alpha(&part, &d, 3)[0], 0);
        Ok(())
    }

    #[test]
    fn zero_feather_is_a_hard_edge_and_empty_masks_blend_nothing() -> Result<(), PostprocessError> {
        let mask = [0u8, 9, 255, 0];
        let d = inside_distance(&mask, 4, 1)?;
        assert_eq!(feather_alpha(&mask, &d, 0), vec![0, 255, 255, 0]);
        let empty = [0u8; 4];
        let d = inside_distance(&empty, 2, 2)?;
        assert_eq!(feather_alpha(&empty, &d, 8), vec![0; 4]);
        Ok(())
    }
}
