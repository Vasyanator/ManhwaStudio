/*
File: watermark_chapter/samples.rs

Purpose:
Calibration samples: what is known about the background under a sample, ring measurement and
its coverage, the per-sample verdict and the validation that turns a page rect into a sample.

Key items:
- `impl SampleBackground`, `impl CalibrationSample`, `impl RingCoverage`, `impl SampleVerdict`,
  `impl SampleParams`
- `validate_calibration_sample()`, `calibration_sample_from_page()`

Notes:
The sample types are declared in `mod.rs`.
*/
use image::RgbaImage;
use super::{
    CalibrationSample, FLAT_RING_MAX_DEV_LIMIT, FLAT_RING_STD_LIMIT, LUMA_B, LUMA_G, LUMA_R,
    MIN_RING_PIXELS, PixelRect, RING_WIDTH_PX, RingCoverage, SampleBackground, SampleParams,
    SampleRejection, SampleVerdict, WatermarkError, count_f32, validate_page, validate_rect,
};


impl SampleBackground {
    /// The one uniform background level under the whole footprint, when there is one.
    ///
    /// `Some` for a measured flat ring and for a hand-asserted level alike — both state a single
    /// `B` per channel, which is what the closed-form and deposit-exact fits need. `None` for a
    /// per-pixel estimate, which has no single level.
    #[must_use]
    pub fn known_level(&self) -> Option<[f32; 3]> {
        match self {
            Self::Flat { level, .. } | Self::Manual { level } => Some(*level),
            Self::Estimated { .. } => None,
        }
    }
}

impl CalibrationSample {
    /// Build a sample from a page region.
    ///
    /// # Errors
    /// Rect validation errors, or [`WatermarkError::BufferLength`] when an `Estimated`
    /// background does not have `width*height*3` entries.
    pub fn from_page(
        page: &RgbaImage,
        page_index: usize,
        rect: PixelRect,
        background: SampleBackground,
    ) -> Result<Self, WatermarkError> {
        let (pw, _ph) = validate_page(page)?;
        validate_rect(page, rect)?;
        let expected = (rect.width as usize) * (rect.height as usize) * 3;
        if let SampleBackground::Estimated { values } = &background
            && values.len() != expected
        {
            return Err(WatermarkError::BufferLength {
                what: "estimated sample background",
                len: values.len(),
                expected,
            });
        }
        let raw = page.as_raw();
        let mut observed = Vec::with_capacity(expected);
        for row in 0..rect.height as usize {
            let base = ((rect.y as usize + row) * pw + rect.x as usize) * 4;
            for px in raw[base..base + rect.width as usize * 4].as_chunks::<4>().0 {
                observed.push(f32::from(px[0]));
                observed.push(f32::from(px[1]));
                observed.push(f32::from(px[2]));
            }
        }
        Ok(Self {
            page_index,
            rect,
            observed,
            background,
        })
    }

    #[must_use]
    pub fn page_index(&self) -> usize {
        self.page_index
    }

    #[must_use]
    pub fn rect(&self) -> PixelRect {
        self.rect
    }

    #[must_use]
    pub fn background(&self) -> &SampleBackground {
        &self.background
    }

    /// True when `B` was MEASURED exactly from a uniform ring.
    ///
    /// Deliberately false for [`SampleBackground::Manual`]: this predicate answers "is there a
    /// measurement here", and it is what guards the one measurement-only decision the engine
    /// still makes — [`refit_with_refined_backgrounds`] may overwrite anything that is not a
    /// measurement with an estimate, and an asserted level must not be treated that way either.
    /// Use [`CalibrationSample::has_known_level`] for "is `B` a single known value".
    #[must_use]
    pub fn is_flat(&self) -> bool {
        matches!(self.background, SampleBackground::Flat { .. })
    }

    /// True when the user asserted this sample's background level instead of the engine
    /// measuring it.
    #[must_use]
    pub fn is_manual(&self) -> bool {
        matches!(self.background, SampleBackground::Manual { .. })
    }

    /// How much of this sample's measurement ring existed, for a MEASURED background.
    ///
    /// `None` when there was no ring to measure — an asserted level or a per-pixel estimate —
    /// which is a different statement from "the ring was complete" and must not be collapsed
    /// into one.
    #[must_use]
    pub fn ring_coverage(&self) -> Option<RingCoverage> {
        match &self.background {
            SampleBackground::Flat { ring, .. } => Some(*ring),
            SampleBackground::Manual { .. } | SampleBackground::Estimated { .. } => None,
        }
    }

    /// True when this sample's background was measured from a ring the page edge truncated.
    #[must_use]
    pub fn has_partial_ring(&self) -> bool {
        self.ring_coverage().is_some_and(|ring| ring.is_partial())
    }

    /// True when `B` is one known value over the whole footprint — measured or asserted.
    ///
    /// This is what the closed-form and deposit-exact fits key on: both need a single `B` per
    /// channel and neither can tell where it came from. Where the provenance matters, it is
    /// carried out of the fit in [`ModelProvenance::manual_backgrounds`] and priced into
    /// [`AlphaUncertainty`], never re-derived from this predicate.
    #[must_use]
    pub fn has_known_level(&self) -> bool {
        self.background.known_level().is_some()
    }

    #[inline]
    pub(super) fn observed_at(&self, pixel: usize, channel: usize) -> f32 {
        self.observed[pixel * 3 + channel]
    }

    #[inline]
    pub(super) fn background_at(&self, pixel: usize, channel: usize) -> f32 {
        match &self.background {
            SampleBackground::Flat { level, .. } | SampleBackground::Manual { level } => {
                level[channel]
            }
            SampleBackground::Estimated { values } => values[pixel * 3 + channel],
        }
    }

    /// Mean background luma over the footprint — the single number the UI shows as "this
    /// sample sits on that level".
    pub(super) fn mean_background_luma(&self) -> f32 {
        match &self.background {
            SampleBackground::Flat { level, .. } | SampleBackground::Manual { level } => {
                LUMA_R * level[0] + LUMA_G * level[1] + LUMA_B * level[2]
            }
            SampleBackground::Estimated { values } => {
                let pixels = values.len() / 3;
                if pixels == 0 {
                    return 0.0;
                }
                let sum: f32 = values
                    .as_chunks::<3>().0.iter()
                    .map(|px| LUMA_R * px[0] + LUMA_G * px[1] + LUMA_B * px[2])
                    .sum();
                sum / count_f32(pixels)
            }
        }
    }
}

impl RingCoverage {
    /// A ring nothing clipped: every pixel it could have had, it had.
    #[must_use]
    pub fn full(pixels: usize) -> Self {
        Self {
            pixels,
            full_pixels: pixels,
        }
    }

    /// True when the page edge (or a degenerate rect) cost the ring pixels it would otherwise
    /// have had. A partial ring is a valid measurement with a weaker statistic behind it.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.pixels < self.full_pixels
    }

    /// Share of the complete ring that survived, 0..=1. 0.0 for a degenerate footprint whose
    /// complete ring would have had no pixels either.
    #[must_use]
    pub fn share(&self) -> f32 {
        if self.full_pixels == 0 {
            return 0.0;
        }
        // Cast justification: both are ring pixel counts of a footprint bounded by the
        // detector's own side cap, exactly representable in f32 far beyond that.
        self.pixels as f32 / self.full_pixels as f32
    }
}

impl SampleVerdict {
    /// True when the sample may be fed to [`estimate_model`].
    #[must_use]
    pub fn is_calibration(&self) -> bool {
        matches!(self, Self::Calibration { .. })
    }

    /// True when the sample may at least be used as a correlation template.
    #[must_use]
    pub fn usable_as_template(&self) -> bool {
        matches!(self, Self::Calibration { .. } | Self::TemplateOnly { .. })
    }

    /// Measured ring level, when there was a ring to measure.
    #[must_use]
    pub fn level(&self) -> Option<[f32; 3]> {
        match self {
            Self::Calibration { level, .. } | Self::TemplateOnly { level, .. } => Some(*level),
            Self::Unusable { .. } => None,
        }
    }

    /// Measured per-channel ring std, when there was a ring to measure.
    #[must_use]
    pub fn ring_std(&self) -> Option<[f32; 3]> {
        match self {
            Self::Calibration { ring_std, .. } | Self::TemplateOnly { ring_std, .. } => {
                Some(*ring_std)
            }
            Self::Unusable { .. } => None,
        }
    }
}

impl Default for SampleParams {
    fn default() -> Self {
        Self {
            ring_width: RING_WIDTH_PX,
            min_ring_pixels: MIN_RING_PIXELS,
            std_limit: FLAT_RING_STD_LIMIT,
            max_dev_limit: FLAT_RING_MAX_DEV_LIMIT,
        }
    }
}

impl SampleParams {
    /// Clamp hand-supplied values into ranges the measurement can honour.
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            ring_width: self.ring_width.clamp(1, 64),
            min_ring_pixels: self.min_ring_pixels.max(8),
            std_limit: self.std_limit.clamp(0.1, 64.0),
            max_dev_limit: self.max_dev_limit.clamp(0.5, 128.0),
        }
    }
}

/// Measure the ring around `rect` and decide what the sample is good for.
///
/// The ring is the annulus of `params.ring_width` pixels around `rect`, CLIPPED TO THE PAGE. A
/// uniform ring is what licenses reading `B` directly; a structured one means the background
/// under the mark is unknown, and the engine says so instead of guessing.
///
/// A mark stamped against a page border therefore still measures: the sides that exist are
/// averaged and the admission floor is `params.min_ring_pixels`, a pixel COUNT, never a
/// per-side margin. The cost of that asymmetry is reported rather than hidden — the verdict
/// carries a [`RingCoverage`], and a partial ring's level is the mean of a one-sided sample of
/// the background. The flatness test is NOT relaxed to pay for it: a ring with real structure
/// is still refused as [`SampleVerdict::TemplateOnly`] whatever its coverage, which is what
/// keeps a truncated ring from silently averaging one end of a gradient into a "measured" level.
///
/// Never panics: a bad rect or a starved ring returns [`SampleVerdict::Unusable`].
#[must_use]
pub(crate) fn validate_calibration_sample(
    page: &RgbaImage,
    rect: PixelRect,
    params: &SampleParams,
) -> SampleVerdict {
    let params = params.normalized();
    let Ok((pw, ph)) = validate_page(page) else {
        return SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        };
    };
    if validate_rect(page, rect).is_err() {
        return SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        };
    }

    // Ring bounds: the rect grown by `ring_width`, clipped to the page. The inner hole is the
    // rect itself, so the ring never reads a pixel the mark could have touched.
    let ring = params.ring_width as usize;
    // What an UNCLIPPED ring would have held, computed before the clip so the coverage can say
    // how much the page edge cost. Saturating throughout: the product of two bounded pixel
    // counts cannot overflow on a validated rect, and a degenerate one reports zero rather
    // than wrapping.
    let full_ring_pixels = (rect.width as usize)
        .saturating_add(ring.saturating_mul(2))
        .saturating_mul((rect.height as usize).saturating_add(ring.saturating_mul(2)))
        .saturating_sub((rect.width as usize).saturating_mul(rect.height as usize));
    let x0 = (rect.x as usize).saturating_sub(ring);
    let y0 = (rect.y as usize).saturating_sub(ring);
    let x1 = ((rect.right() as usize) + ring).min(pw);
    let y1 = ((rect.bottom() as usize) + ring).min(ph);

    let raw = page.as_raw();
    let mut count = 0usize;
    let mut sum = [0f64; 3];
    let mut sum_sq = [0f64; 3];
    for y in y0..y1 {
        let inside_rows = y >= rect.y as usize && (y as u64) < rect.bottom();
        for x in x0..x1 {
            if inside_rows && x >= rect.x as usize && (x as u64) < rect.right() {
                continue;
            }
            let base = (y * pw + x) * 4;
            for channel in 0..3 {
                let value = f64::from(raw[base + channel]);
                sum[channel] += value;
                sum_sq[channel] += value * value;
            }
            count += 1;
        }
    }

    if count < params.min_ring_pixels {
        return SampleVerdict::Unusable {
            reason: SampleRejection::RingTooSmall {
                pixels: count,
                needed: params.min_ring_pixels,
            },
        };
    }

    let n = count as f64;
    let mut level = [0f32; 3];
    let mut std = [0f32; 3];
    for channel in 0..3 {
        let mean = sum[channel] / n;
        let variance = (sum_sq[channel] / n - mean * mean).max(0.0);
        level[channel] = mean as f32;
        std[channel] = variance.sqrt() as f32;
    }

    // Second pass for the max deviation: std alone cannot see a slow gradient across a thin ring.
    let mut max_dev = [0f32; 3];
    for y in y0..y1 {
        let inside_rows = y >= rect.y as usize && (y as u64) < rect.bottom();
        for x in x0..x1 {
            if inside_rows && x >= rect.x as usize && (x as u64) < rect.right() {
                continue;
            }
            let base = (y * pw + x) * 4;
            for channel in 0..3 {
                let deviation = (f32::from(raw[base + channel]) - level[channel]).abs();
                max_dev[channel] = max_dev[channel].max(deviation);
            }
        }
    }

    let coverage = RingCoverage {
        pixels: count,
        // A clip can only ever remove pixels, so a count above the unclipped figure would mean
        // the two were computed from different rects; clamping keeps `is_partial` honest
        // instead of letting an impossible pair read as "complete".
        full_pixels: full_ring_pixels.max(count),
    };
    // The flatness thresholds are the SAME whatever the coverage: partialness costs pixels, not
    // the right to be called structured. Weakening them for a truncated ring would turn "less
    // evidence" into "a lower bar", which is the one substitution this measurement forbids.
    let flat = std.iter().all(|&v| v <= params.std_limit)
        && max_dev.iter().all(|&v| v <= params.max_dev_limit);
    if flat {
        SampleVerdict::Calibration {
            level,
            ring_std: std,
            ring: coverage,
        }
    } else {
        SampleVerdict::TemplateOnly {
            level,
            ring_std: std,
            ring_max_dev: max_dev,
            std_limit: params.std_limit,
            max_dev_limit: params.max_dev_limit,
            ring: coverage,
        }
    }
}

/// Validate a rect and, when its ring is flat, build the flat-background calibration sample in
/// one step. Used by both the manual picker and the automatic collection pass over detected
/// occurrences.
///
/// Returns the verdict always, and the sample only when the verdict licensed one.
///
/// # Errors
/// Propagates rect/page validation failures from [`CalibrationSample::from_page`].
pub(crate) fn calibration_sample_from_page(
    page: &RgbaImage,
    page_index: usize,
    rect: PixelRect,
    params: &SampleParams,
) -> Result<(SampleVerdict, Option<CalibrationSample>), WatermarkError> {
    let verdict = validate_calibration_sample(page, rect, params);
    let sample = match &verdict {
        SampleVerdict::Calibration {
            level,
            ring_std,
            ring,
        } => Some(CalibrationSample::from_page(
            page,
            page_index,
            rect,
            SampleBackground::Flat {
                level: *level,
                ring_std: *ring_std,
                ring: *ring,
            },
        )?),
        SampleVerdict::TemplateOnly { .. } | SampleVerdict::Unusable { .. } => None,
    };
    Ok((verdict, sample))
}
