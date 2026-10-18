/*
File: crates/ms-region-postprocess/src/global_match.rs

Purpose:
The global colour match: aligns the generated region's per-channel RGB mean and standard
deviation to the reference over the ring OUTSIDE the colour-match mask, or, for a whole-region
run, over the region's edge band.

Key items:
- `match_color_global()`: the ring stage (sample selection with its fallbacks).
- `match_color_band()`: the edge-band stage.
- `fit_and_apply()`: the shared fit (whole region rewritten).
- `GlobalPath`: which sample set the stage used (diagnostics and tests).
- `MARGIN_PX` / `MIN_SAMPLES`: the ring margin and the minimum sample count.

Notes:
The ring (or the edge band) is the only place where the model's output and the original are
supposed to agree, so it measures the VAE round trip's tone shift of the whole window; a gain +
offset per channel is all that shift needs. The 6 px ring margin keeps a text outline or halo at
the contour out of the fit; the band has no contour and no margin.
Statistics mirror the reference's numpy float32 arithmetic exactly (pairwise summation, see
`pairwise_sum_f32`), so the stage is bit-identical to it, and deterministic: the statistics run
concurrently as whole values, a large sum splits only along numpy's own halving tree (each half
summed as numpy sums it, then the one `+` numpy does), and the apply is per pixel.
*/

use crate::edge_band::EdgeBand;
use crate::mask::dilate_ellipse;
use crate::PostprocessError;
use rayon::prelude::*;

/// Margin, px, by which the colour-match mask is grown before the ring is sampled.
pub(crate) const MARGIN_PX: u32 = 6;
/// Fewest ring samples a fit is computed from; fewer make the statistics noise.
pub(crate) const MIN_SAMPLES: usize = 256;
/// A channel whose ring std is below this has no scale to correct and is only shifted.
const FLAT_STD: f64 = 1e-3;

/// Sample set the global match used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalPath {
    /// Outside the mask grown by `MARGIN_PX`.
    Margin,
    /// The margin left too few samples: outside the undilated mask.
    Unmargined,
    /// The region's edge band (a whole-region run, see `edge_band.rs`).
    Band,
    /// Too few samples: the input is returned unchanged.
    Skipped,
}

/// Matches `generated` (RGB8, `width * height * 3`) to `reference` over the pixels outside
/// `sample_mask` (nonzero = masked) grown by `MARGIN_PX`; falls back to the undilated mask, then
/// to no change, when fewer than `MIN_SAMPLES` pixels remain (the RING reference mode).
///
/// The fit itself is [`fit_and_apply`]; this function only selects the ring samples.
///
/// # Errors
/// `PostprocessError::SizeOverflow` / `ShapeMismatch` from the mask dilation (the caller has
/// validated the buffers, so this does not happen in the pipeline).
pub(crate) fn match_color_global(reference: &[u8], generated: &[u8], sample_mask: &[u8], width: usize, height: usize) -> Result<(Vec<u8>, GlobalPath), PostprocessError> {
    let grown = dilate_ellipse(sample_mask, width, height, MARGIN_PX)?;
    let mut path = GlobalPath::Margin;
    let mut selector: &[u8] = &grown;
    if count_unmasked(selector) < MIN_SAMPLES {
        // A thin ring around a large mask: a margin-free fit beats no fit.
        path = GlobalPath::Unmargined;
        selector = sample_mask;
    }
    if count_unmasked(selector) < MIN_SAMPLES {
        return Ok((generated.to_vec(), GlobalPath::Skipped));
    }
    Ok((fit_and_apply(reference, generated, width, |index| selector[index] == 0), path))
}

/// Matches `generated` to `reference` over the pixels of the region's edge `band` (the
/// EDGE-BAND reference mode of a whole-region run): no margin, because nothing was painted, so
/// no outline or halo sits at a contour; fewer than `MIN_SAMPLES` band pixels skip.
pub(crate) fn match_color_band(reference: &[u8], generated: &[u8], band: &EdgeBand) -> (Vec<u8>, GlobalPath) {
    if band.count() < MIN_SAMPLES {
        return (generated.to_vec(), GlobalPath::Skipped);
    }
    (fit_and_apply(reference, generated, band.width(), |index| band.contains_index(index)), GlobalPath::Band)
}

/// The global fit over the pixels `is_sample(index)` selects (`index` = row-major pixel index,
/// visited in ascending order), applied to EVERY pixel of `generated`.
///
/// Per channel, with population mean/std `rm, rs` (reference) and `gm, gs` (generated) over the
/// samples: `x' = (x - gm) * (rs / gs) + rm`, or `x + (rm - gm)` for a flat generated channel,
/// rounded half to even and clamped. The caller guarantees at least one sample.
fn fit_and_apply(reference: &[u8], generated: &[u8], width: usize, is_sample: impl Fn(usize) -> bool + Sync) -> Vec<u8> {
    // Six independent statistics (reference / generated x three channels), each summed in
    // numpy's own sequential pairwise order; only whole statistics run concurrently, so every
    // value is the one the sequential computation gives.
    let stats: Vec<(f32, f32)> = (0..6_usize)
        .into_par_iter()
        .map(|job| {
            let (image, channel) = if job < 3 { (reference, job) } else { (generated, job - 3) };
            let values: Vec<f32> = image.chunks_exact(3).enumerate().filter(|(index, _)| is_sample(*index)).map(|(_, px)| f32::from(px[channel])).collect();
            mean_std_f32(&values)
        })
        .collect();
    let maps: [ChannelMap; 3] = std::array::from_fn(|channel| {
        let (ref_mean, ref_std) = stats[channel];
        let (gen_mean, gen_std) = stats[channel + 3];
        // The reference converts each f32 statistic to a Python float (f64) and combines them
        // there; numpy then casts the combined scalar to f32 for the array arithmetic.
        if f64::from(gen_std) < FLAT_STD {
            ChannelMap::Shift(f64_to_f32(f64::from(ref_mean) - f64::from(gen_mean)))
        } else {
            ChannelMap::Gain { gen_mean, gain: f64_to_f32(f64::from(ref_std) / f64::from(gen_std)), ref_mean }
        }
    });
    let mut out = generated.to_vec();
    // Per-pixel, per-channel maps with no reduction: rows are independent.
    let row_len = width.max(1) * 3;
    out.par_chunks_mut(row_len).for_each(|row| {
        for px in row.chunks_exact_mut(3) {
            for (v, map) in px.iter_mut().zip(&maps) {
                *v = map.apply(*v);
            }
        }
    });
    out
}

/// Number of unmasked (zero) entries of a mask.
fn count_unmasked(mask: &[u8]) -> usize {
    mask.par_iter().filter(|&&m| m == 0).count()
}

/// The global match of one channel, in f32 as numpy applies it.
#[derive(Debug, Clone, Copy)]
enum ChannelMap {
    /// A flat generated channel: `x + shift`.
    Shift(f32),
    /// `(x - gen_mean) * gain + ref_mean`.
    Gain { gen_mean: f32, gain: f32, ref_mean: f32 },
}

impl ChannelMap {
    /// The mapped, rounded (half to even) and clamped level of `v`.
    fn apply(self, v: u8) -> u8 {
        match self {
            Self::Shift(shift) => round_to_u8(f32::from(v) + shift),
            Self::Gain { gen_mean, gain, ref_mean } => round_to_u8((f32::from(v) - gen_mean) * gain + ref_mean),
        }
    }
}

/// Population mean and std (ddof 0) of `values` exactly as numpy computes them for a float32
/// array: `mean = sum / n`, `var = sum((x - mean)²) / n`, every step in f32, sums pairwise.
fn mean_std_f32(values: &[f32]) -> (f32, f32) {
    let n = count_f32(values.len());
    let mean = pairwise_sum_f32(values) / n;
    let squares: Vec<f32> = values.iter().map(|&v| (v - mean) * (v - mean)).collect();
    let var = pairwise_sum_f32(&squares) / n;
    (mean, var.sqrt())
}

/// Length from which `pairwise_sum_f32` sums its two halves on two rayon tasks.
const PARALLEL_SUM_MIN: usize = 1 << 16;

/// numpy's `pairwise_sum` for float32 (`loops_utils.h.src`): below 8 elements a plain loop, up
/// to 128 eight interleaved accumulators combined as `((r0+r1)+(r2+r3))+((r4+r5)+(r6+r7))` plus
/// the sequential tail, above that the two halves split at a multiple of 8.
fn pairwise_sum_f32(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        a.iter().fold(0.0, |acc, &v| acc + v)
    } else if n <= 128 {
        let mut r = [0.0f32; 8];
        r.copy_from_slice(&a[..8]);
        let full = n - n % 8;
        for block in a[8..full].chunks_exact(8) {
            for (acc, &v) in r.iter_mut().zip(block) {
                *acc += v;
            }
        }
        let head = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        a[full..].iter().fold(head, |acc, &v| acc + v)
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        let (low, high) = (&a[..n2], &a[n2..]);
        // The two halves of numpy's tree are independent; summing them on two threads and
        // adding them once is exactly the sequential result. Small halves stay on this thread.
        if n >= PARALLEL_SUM_MIN {
            let (low, high) = rayon::join(|| pairwise_sum_f32(low), || pairwise_sum_f32(high));
            low + high
        } else {
            pairwise_sum_f32(low) + pairwise_sum_f32(high)
        }
    }
}

/// A sample count as the f32 divisor numpy uses.
#[expect(clippy::cast_precision_loss, reason = "a region's pixel count is far below 2^24 in practice; beyond it numpy rounds the divisor the same way")]
fn count_f32(n: usize) -> f32 {
    n as f32
}

/// f64 -> f32 rounding to nearest, as numpy casts a Python float scalar into f32 arithmetic.
#[expect(clippy::cast_possible_truncation, reason = "deliberate f64 -> f32 rounding of a colour statistic, mirroring the reference")]
fn f64_to_f32(v: f64) -> f32 {
    v as f32
}

/// numpy `clip(rint(x), 0, 255).astype(uint8)`: round half to even, then clamp.
pub(crate) fn round_to_u8(v: f32) -> u8 {
    let r = v.round_ties_even();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        // Integral and inside 0..255 here; NaN fails both comparisons, lands here as well and
        // saturates to 0 (`nan_rounds_to_zero_and_ties_go_to_even`).
        integral_to_u8(r)
    }
}

/// An integral f32 in `0.0..=255.0` (NaN saturates to 0) to `u8`.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "the value is integral and inside 0..=255; `as` saturates NaN to 0")]
fn integral_to_u8(v: f32) -> u8 {
    v as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_rounds_to_zero_and_ties_go_to_even() {
        assert_eq!(round_to_u8(f32::NAN), 0);
        assert_eq!(round_to_u8(2.5), 2);
        assert_eq!(round_to_u8(3.5), 4);
        assert_eq!(round_to_u8(-7.0), 0);
        assert_eq!(round_to_u8(300.0), 255);
    }

    #[test]
    fn pairwise_sum_of_integers_is_exact_when_small() {
        let v: Vec<f32> = (0..1000u16).map(f32::from).collect();
        assert!((pairwise_sum_f32(&v) - 499_500.0).abs() < f32::EPSILON);
    }

    /// numpy's pairwise tree, summed sequentially (the form before the halves ran on rayon).
    fn sequential_pairwise(a: &[f32]) -> f32 {
        let n = a.len();
        if n <= 128 {
            return pairwise_sum_f32(a);
        }
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        sequential_pairwise(&a[..n2]) + sequential_pairwise(&a[n2..])
    }

    /// Splitting the halves across threads keeps every bit of numpy's sequential sum.
    #[test]
    fn parallel_pairwise_sum_equals_the_sequential_tree() {
        let mut state = 0x9e37_79b9_u32;
        let values: Vec<f32> = (0..(PARALLEL_SUM_MIN * 5 + 37))
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                f32::from(u16::try_from(state >> 16).unwrap_or(0)) / 7.0
            })
            .collect();
        assert_eq!(pairwise_sum_f32(&values).to_bits(), sequential_pairwise(&values).to_bits());
    }

    #[test]
    fn identical_images_are_unchanged_and_tiny_rings_skip() -> Result<(), PostprocessError> {
        let (w, h) = (40, 40);
        let img: Vec<u8> = (0..w * h * 3).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let mut mask = vec![0u8; w * h];
        mask[20 * w + 20] = 255;
        let (out, path) = match_color_global(&img, &img, &mask, w, h)?;
        assert_eq!(path, GlobalPath::Margin);
        assert_eq!(out, img, "matching an image to itself is the identity");
        let full = vec![255u8; w * h];
        let shifted: Vec<u8> = img.iter().map(|v| v.saturating_add(9)).collect();
        assert_eq!(match_color_global(&img, &shifted, &full, w, h)?, (shifted, GlobalPath::Skipped));
        Ok(())
    }
}
