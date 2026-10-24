/*
File: crates/ms-region-postprocess/src/composite.rs

Purpose:
The final composite: the corrected region alpha-blended into the base under the inward feather.

Key functions:
- `composite_over_base()` (tests): the RGB8 result.
- `composite_masked_into()`: the same blend written only into the masked pixels of a caller's
  buffer (the host's display image), row-parallel.

Notes:
The mask is a PERMISSION TO CHANGE: every pixel outside it comes back as the base's bytes
verbatim (the core contract, tested on every fixture). The blend ROUNDS (half to even): a
truncating blend darkens every partially blended pixel by up to one level, a mask-shaped dark
patch with a hard contour; rounding makes blending a region with itself an exact identity.
*/

use crate::global_match::round_to_u8;
use rayon::prelude::*;

/// Blends `corrected` (RGB8) over `base` (RGB8) with weights `alpha` (0..=255, from
/// `feather::feather_alpha`) inside `mask` (nonzero = masked): `rint(base·(1-a) + corrected·a)`
/// with `a = alpha / 255` in f32. Outside the mask the result is `base` byte for byte. All
/// buffers are validated by the caller (`mask`/`alpha`: `n`, images: `3n`). The pipeline uses
/// `composite_masked_into` directly; this form is the stage the parity fixtures pin.
#[cfg(test)]
pub(crate) fn composite_over_base(base: &[u8], corrected: &[u8], mask: &[u8], alpha: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    let (pixels, _) = out.as_chunks_mut::<3>();
    composite_masked_into(base, corrected, mask, alpha, mask.len().max(1), pixels, |px, rgb| *px = rgb);
    out
}

/// The composite of [`composite_over_base`] written into a caller's pixel buffer `out`
/// (`n` pixels, row-major, rows of `width`): `write(pixel, rgb)` receives the blended RGB of
/// every pixel INSIDE `mask`; every other pixel of `out` is left untouched, so a caller that
/// pre-filled it with its own outside pixels (alpha included) keeps them verbatim.
///
/// Row-parallel (rayon); the per-pixel arithmetic is the one of `composite_over_base`, so the
/// bytes do not depend on the thread count. `width` must be nonzero and the buffers validated
/// by the caller (`mask`/`alpha`/`out`: `n`, images: `3n`).
pub(crate) fn composite_masked_into<P: Send>(base: &[u8], corrected: &[u8], mask: &[u8], alpha: &[u8], width: usize, out: &mut [P], write: impl Fn(&mut P, [u8; 3]) + Sync) {
    out.par_chunks_mut(width).enumerate().for_each(|(row, out_row)| {
        let start = row * width;
        let end = start + out_row.len();
        let pixels = out_row.iter_mut().zip(base[start * 3..end * 3].as_chunks::<3>().0).zip(corrected[start * 3..end * 3].as_chunks::<3>().0);
        for (((px, before), src), (&m, &a)) in pixels.zip(mask[start..end].iter().zip(&alpha[start..end])) {
            if m == 0 {
                continue;
            }
            let a = f32::from(a) / 255.0;
            let keep = 1.0 - a;
            write(px, [0, 1, 2].map(|c| round_to_u8(f32::from(before[c]) * keep + f32::from(src[c]) * a)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_is_verbatim_and_self_blend_is_identity() {
        let base: Vec<u8> = (0..12u8).map(|v| v * 20).collect();
        let corrected = vec![7u8; 12];
        let mask = [0u8, 255, 255, 0];
        let alpha = [200u8, 0, 255, 255];
        let out = composite_over_base(&base, &corrected, &mask, &alpha);
        assert_eq!(&out[0..3], &base[0..3], "outside the mask the base survives even with alpha > 0");
        assert_eq!(&out[3..6], &base[3..6], "alpha 0 inside keeps the base");
        assert_eq!(&out[6..9], &[7, 7, 7]);
        assert_eq!(&out[9..12], &base[9..12]);
        for a in 0..=255u8 {
            assert_eq!(composite_over_base(&base, &base, &[255; 4], &[a; 4]), base, "alpha {a}");
        }
    }

    /// The masked-into form writes the blend of every masked pixel and nothing else, row by row.
    #[test]
    fn masked_into_writes_only_inside_the_mask() {
        let base: Vec<u8> = (0..18u8).map(|v| v * 13).collect();
        let corrected: Vec<u8> = (0..18u8).map(|v| 250 - v * 7).collect();
        let mask = [0u8, 3, 255, 255, 0, 9];
        let alpha = [255u8, 128, 0, 77, 255, 255];
        let expected = composite_over_base(&base, &corrected, &mask, &alpha);
        let mut out = vec![None; 6];
        composite_masked_into(&base, &corrected, &mask, &alpha, 2, &mut out, |px, rgb| *px = Some(rgb));
        for ((px, rgb), &m) in out.iter().zip(expected.as_chunks::<3>().0).zip(&mask) {
            assert_eq!(*px, (m != 0).then(|| [rgb[0], rgb[1], rgb[2]]));
        }
    }
}
