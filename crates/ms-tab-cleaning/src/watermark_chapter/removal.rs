/*
File: watermark_chapter/removal.rs

Purpose:
Removal: `B = (I - c)/s` per occurrence with the subpixel model shift and one clamp at the
end, plus the honest residual numbers of a recovered region.

Key items:
- `impl RemovalResidual`
- `remove_occurrence()`, `remove_occurrences_on_page()`

Notes:
`RegionPatch` and `RemovalResidual` are declared in `mod.rs`.
*/
use image::RgbaImage;
use rayon::prelude::*;
use super::{
    ALPHA_SIGNIFICANT, Occurrence, PixelRect, QUANT_SLACK, RegionPatch, RemovalResidual, S_FLOOR,
    SubpixelShift, WatermarkError, WatermarkModel, ratio_u64, validate_page, validate_rect,
};


impl RemovalResidual {
    /// Per-channel rms recomposition error over the mark pixels.
    #[must_use]
    pub fn rms(&self) -> [f32; 3] {
        if self.mark_pixels == 0 {
            return [0.0; 3];
        }
        let n = self.mark_pixels as f64;
        [
            (self.sum_sq[0] / n).sqrt() as f32,
            (self.sum_sq[1] / n).sqrt() as f32,
            (self.sum_sq[2] / n).sqrt() as f32,
        ]
    }

    /// Share of mark pixels recovered exactly, in `0..=1`.
    #[must_use]
    pub fn exact_share(&self) -> f32 {
        if self.mark_pixels == 0 {
            return 1.0;
        }
        ratio_u64(self.exact_pixels as u64, self.mark_pixels as u64)
    }

    /// Share of mark pixels that were irrecoverable, in `0..=1`.
    #[must_use]
    pub fn clipped_share(&self) -> f32 {
        if self.mark_pixels == 0 {
            return 0.0;
        }
        ratio_u64(self.clipped_pixels as u64, self.mark_pixels as u64)
    }

    /// Fold another patch's residual in, for chapter-wide reporting.
    pub fn merge(&mut self, other: &Self) {
        self.mark_pixels += other.mark_pixels;
        self.exact_pixels += other.exact_pixels;
        self.clipped_pixels += other.clipped_pixels;
        self.max_uncertainty_lsb = self.max_uncertainty_lsb.max(other.max_uncertainty_lsb);
        for (slot, value) in self.sum_sq.iter_mut().zip(other.sum_sq.iter()) {
            *slot += value;
        }
    }
}

/// Recover the background under one occurrence: `B = (I - c)/s`, in f32 from end to end.
///
/// `shift` shifts the MODEL, not the image: the model is sampled bilinearly at `(x - dx, y - dy)`
/// to match an occurrence that landed off the integer grid. The whole computation stays in f32
/// and is clamped and rounded EXACTLY ONCE, at the write to the output byte — the error
/// amplification of this operator is `1/s`, and compounding intermediate rounding into it would
/// show up directly in the result.
///
/// # Errors
/// Page/rect validation failures, or [`WatermarkError::GeometryMismatch`] when the model does not
/// cover the rect.
pub(crate) fn remove_occurrence(
    page: &RgbaImage,
    rect: PixelRect,
    model: &WatermarkModel,
    shift: SubpixelShift,
) -> Result<RegionPatch, WatermarkError> {
    let (pw, _ph) = validate_page(page)?;
    validate_rect(page, rect)?;
    if rect.width != model.width || rect.height != model.height {
        return Err(WatermarkError::GeometryMismatch {
            expected_width: model.width,
            expected_height: model.height,
            width: rect.width,
            height: rect.height,
        });
    }
    let operator = model.operator();
    let (iw, ih) = (rect.width as usize, rect.height as usize);
    let raw = page.as_raw();
    let mut pixels = vec![0u8; iw * ih * 4];
    let mut residual = RemovalResidual::default();

    for iy in 0..ih {
        let source_base = ((rect.y as usize + iy) * pw + rect.x as usize) * 4;
        for ix in 0..iw {
            let inner_pixel = iy * iw + ix;
            let source = source_base + ix * 4;
            let destination = inner_pixel * 4;
            let is_mark = model.mark_alpha(inner_pixel) >= ALPHA_SIGNIFICANT;
            let mut exact = true;
            let mut clipped = false;
            for channel in 0..3 {
                let observed = f32::from(raw[source + channel]);
                let (c, s) = if shift.is_zero() {
                    model.params_at(inner_pixel, channel)
                } else {
                    // Cast justification: `ix < iw` and `iy < ih`, both far below 2^24.
                    model.params_bilinear(
                        ix as f32 - shift.dx,
                        iy as f32 - shift.dy,
                        channel,
                    )
                };
                let background = operator.decompose(c, s, observed);
                let slack = QUANT_SLACK / s.max(S_FLOOR);
                if background < -slack || background > 255.0 + slack {
                    clipped = true;
                }
                // The one and only quantization of this pipeline.
                let quantized = background.clamp(0.0, 255.0).round();
                // Cast justification: `quantized` is clamped to 0..=255 and rounded, so it is
                // exactly representable as u8.
                let byte = quantized as u8;
                pixels[destination + channel] = byte;
                if is_mark {
                    let error = f64::from(operator.compose(c, s, quantized) - observed);
                    residual.sum_sq[channel] += error * error;
                    if error.abs() > f64::from(QUANT_SLACK) {
                        exact = false;
                    }
                    residual.max_uncertainty_lsb =
                        residual.max_uncertainty_lsb.max(QUANT_SLACK / s.max(S_FLOOR));
                }
            }
            pixels[destination + 3] = raw[source + 3];
            if is_mark {
                residual.mark_pixels += 1;
                if exact {
                    residual.exact_pixels += 1;
                }
                if clipped {
                    residual.clipped_pixels += 1;
                }
            }
        }
    }

    Ok(RegionPatch {
        rect,
        pixels,
        residual,
    })
}

/// Remove every listed occurrence from one page, one rayon task per occurrence.
///
/// Every occurrence must be gain-verified: a correlation-only accept is refused with
/// [`WatermarkError::UnverifiedOccurrence`] rather than skipped, because subtracting a mark that
/// is not there INJECTS an inverse mark and a silent skip would hide that risk from the caller.
///
/// # Errors
/// The first validation failure, including an unverified occurrence.
pub(crate) fn remove_occurrences_on_page(
    page: &RgbaImage,
    model: &WatermarkModel,
    occurrences: &[Occurrence],
) -> Result<Vec<RegionPatch>, WatermarkError> {
    occurrences
        .par_iter()
        .map(|occurrence| {
            if !occurrence.is_removal_safe() {
                return Err(WatermarkError::UnverifiedOccurrence {
                    rect: occurrence.rect,
                });
            }
            remove_occurrence(page, occurrence.rect, model, occurrence.shift)
        })
        .collect()
}
