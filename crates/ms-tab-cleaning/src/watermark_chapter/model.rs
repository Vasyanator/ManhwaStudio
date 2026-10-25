/*
File: watermark_chapter/model.rs

Purpose:
Behaviour of the fitted model and its typed verdicts: alpha uncertainty and its LSB cost,
the conditioning verdict's queries, `WatermarkModel` accessors and per-pixel parameters, the
shape-independent `MarkSignature` and the subpixel shift.

Key items:
- `impl AlphaUncertainty`, `impl ModelConditioning`
- `impl WatermarkModel`, `impl MarkSignature`, `impl SubpixelShift`

Notes:
The types are declared in `mod.rs`; the fit that builds a model lives in `fit.rs`.
*/
use std::sync::Arc;
use super::{
    ACHROMATIC_DEPOSIT_CHROMA, ALPHA_ERROR_LSB_PER_PERCENT, ALPHA_SIGNIFICANT, AlphaSource,
    AlphaUncertainty, CalibrationSample, CompositingOperator, DARK_BACKGROUND_LUMA,
    DARK_BACKGROUND_MAX_FACTOR, DARK_BACKGROUND_RMS_FACTOR, DEPOSIT_SIGNIFICANT_LSB, FIT_NOISE_LSB,
    LUMA_B, LUMA_G, LUMA_R, MAX_SUBPIXEL_SHIFT, MIN_BACKGROUND_SPREAD, MarkSignature,
    ModelConditioning, ModelProvenance, SAME_LEVEL_EPS, SAME_MARK_GAIN_MAX, SAME_MARK_GAIN_MIN,
    S_CEIL, S_FLOOR, SampleBackground, SubpixelShift, SuggestedBackground, WatermarkError,
    WatermarkModel, count_f32,
};
use super::fit::alpha_lower_bound;


impl AlphaUncertainty {
    /// Apply the measured calibration to a relative alpha uncertainty.
    ///
    /// A negative `percent` is taken as zero and a non-finite one as fully uncertain (100%) —
    /// never as certain, because an unquantifiable uncertainty is not a small one. Everything else
    /// follows from the measured constants, so the reported numbers cannot drift apart from them.
    #[must_use]
    pub fn from_percent(source: AlphaSource, percent: f32) -> Self {
        let percent = if percent.is_finite() {
            percent.max(0.0)
        } else {
            100.0
        };
        let rms_lsb = percent * ALPHA_ERROR_LSB_PER_PERCENT;
        Self {
            source,
            percent,
            rms_lsb,
            dark_rms_lsb: rms_lsb * DARK_BACKGROUND_RMS_FACTOR,
            dark_max_lsb: rms_lsb * DARK_BACKGROUND_MAX_FACTOR,
            dark_luma: DARK_BACKGROUND_LUMA,
        }
    }

    /// Uncertainty of a slope fitted from exact backgrounds `spread` LSB apart on a mark whose
    /// peak opacity is `peak_alpha`.
    ///
    /// The two-point slope error is `sigma*sqrt(2)/spread` with `sigma = FIT_NOISE_LSB`; over the
    /// measured chapter (spread 255, peak alpha 0.183) that is 7.6%, i.e. 2.4 LSB — which is
    /// exactly the 2.3-2.6 LSB leave-one-out residual the prototype measured. No averaging over
    /// samples is credited: the residuals of different occurrences are mutually uncorrelated only
    /// because each is a different rasterization, which is a bias per occurrence, not noise that
    /// `sqrt(n)` would cancel.
    #[must_use]
    pub fn from_flat_fit(spread: f32, peak_alpha: f32) -> Self {
        let percent = if spread > f32::EPSILON && peak_alpha > f32::EPSILON {
            100.0 * (FIT_NOISE_LSB * std::f32::consts::SQRT_2 / spread) / peak_alpha
        } else {
            100.0
        };
        Self::from_percent(AlphaSource::SeparatedBackgrounds, percent)
    }
}

impl ModelConditioning {
    /// True when both `c` and `s` were determined by the data.
    #[must_use]
    pub fn is_separable(&self) -> bool {
        matches!(self, Self::Separable { .. })
    }

    /// True when this verdict comes with a model. False only for the three refusals.
    #[must_use]
    pub fn produces_model(&self) -> bool {
        matches!(self, Self::Separable { .. } | Self::DepositExact { .. })
    }

    /// Distinct background levels observed so far, ascending. Empty when nothing was measured.
    #[must_use]
    pub fn levels(&self) -> &[f32] {
        match self {
            Self::Separable { levels, .. }
            | Self::DepositExact { levels, .. }
            | Self::Underdetermined { levels, .. } => levels,
            Self::NotEnoughSamples { .. } | Self::DepositUnavailable { .. } => &[],
        }
    }

    /// Widest gap between observed levels, LSB.
    #[must_use]
    pub fn spread(&self) -> f32 {
        match self {
            Self::Separable { spread, .. }
            | Self::DepositExact { spread, .. }
            | Self::DepositUnavailable { spread, .. } => *spread,
            Self::NotEnoughSamples { .. } => 0.0,
            Self::Underdetermined {
                worst_pixel_spread, ..
            } => *worst_pixel_spread,
        }
    }

    /// How well the alpha scale is pinned, when a model was produced at all.
    #[must_use]
    pub fn alpha_uncertainty(&self) -> Option<AlphaUncertainty> {
        match self {
            Self::Separable { alpha, .. } | Self::DepositExact { alpha, .. } => Some(*alpha),
            Self::NotEnoughSamples { .. }
            | Self::DepositUnavailable { .. }
            | Self::Underdetermined { .. } => None,
        }
    }

    /// Which background the next sample should sit on, when one would improve the model.
    ///
    /// This is what turns "the slope is an assumption" into a concrete instruction — "add a
    /// manual sample over a flat DARK area" — and it is returned for the graded state too, where
    /// one such sample collapses the alpha uncertainty to the fitted one. A slope fitted against
    /// per-pixel ESTIMATES also gets one: it is separable but worth only +-10-20%, and one
    /// exactly measured sample beats it.
    #[must_use]
    pub fn suggested_background(&self) -> Option<SuggestedBackground> {
        let (reference, required) = match self {
            Self::Separable { alpha, levels, .. } => {
                if alpha.source == AlphaSource::SeparatedBackgrounds {
                    return None;
                }
                (mean_level(levels), MIN_BACKGROUND_SPREAD)
            }
            Self::NotEnoughSamples { .. } | Self::DepositUnavailable { .. } => {
                (127.5, MIN_BACKGROUND_SPREAD)
            }
            Self::DepositExact { levels, .. } => (mean_level(levels), MIN_BACKGROUND_SPREAD),
            Self::Underdetermined {
                levels, required, ..
            } => (mean_level(levels), *required),
        };
        // Ask for whichever side of the observed level has room left in 0..=255.
        if reference > 127.5 {
            Some(SuggestedBackground::Darker {
                at_most: (reference - required).max(0.0),
            })
        } else {
            Some(SuggestedBackground::Brighter {
                at_least: (reference + required).min(255.0),
            })
        }
    }
}

/// Mean of a level list; mid-grey for an empty one, which is the neutral request.
fn mean_level(levels: &[f32]) -> f32 {
    if levels.is_empty() {
        return 127.5;
    }
    levels.iter().sum::<f32>() / count_f32(levels.len())
}

impl WatermarkModel {
    /// Build a model from raw parameter planes, validating shape and physical range.
    ///
    /// # Errors
    /// [`WatermarkError::GeometryMismatch`] for a zero dimension, [`WatermarkError::BufferLength`]
    /// when a plane is not `width*height*3` long, and [`WatermarkError::ParameterOutOfRange`] for
    /// a non-finite value, a `c` outside `0..=255` or an `s` outside `[S_FLOOR, S_CEIL]`.
    pub fn from_parts(
        width: u32,
        height: u32,
        c: Vec<f32>,
        s: Vec<f32>,
        operator: Arc<dyn CompositingOperator>,
        provenance: ModelProvenance,
    ) -> Result<Self, WatermarkError> {
        if width == 0 || height == 0 {
            return Err(WatermarkError::GeometryMismatch {
                expected_width: width.max(1),
                expected_height: height.max(1),
                width,
                height,
            });
        }
        let expected = (width as usize) * (height as usize) * 3;
        if c.len() != expected {
            return Err(WatermarkError::BufferLength {
                what: "model c plane",
                len: c.len(),
                expected,
            });
        }
        if s.len() != expected {
            return Err(WatermarkError::BufferLength {
                what: "model s plane",
                len: s.len(),
                expected,
            });
        }
        for (index, &value) in c.iter().enumerate() {
            if !value.is_finite() || !(0.0..=255.0).contains(&value) {
                return Err(WatermarkError::ParameterOutOfRange {
                    what: "c",
                    index,
                    value,
                });
            }
        }
        for (index, &value) in s.iter().enumerate() {
            if !value.is_finite() || !(S_FLOOR..=S_CEIL).contains(&value) {
                return Err(WatermarkError::ParameterOutOfRange {
                    what: "s",
                    index,
                    value,
                });
            }
        }
        Ok(Self {
            width,
            height,
            c,
            s,
            operator,
            provenance,
        })
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn c(&self) -> &[f32] {
        &self.c
    }

    #[must_use]
    pub fn s(&self) -> &[f32] {
        &self.s
    }

    #[must_use]
    pub fn operator(&self) -> &dyn CompositingOperator {
        self.operator.as_ref()
    }

    #[must_use]
    pub fn provenance(&self) -> &ModelProvenance {
        &self.provenance
    }

    /// `(c, s)` of one pixel/channel. Indices are assumed validated by the caller.
    #[inline]
    pub(super) fn params_at(&self, pixel: usize, channel: usize) -> (f32, f32) {
        let index = pixel * 3 + channel;
        (self.c[index], self.s[index])
    }

    /// Opacity of a pixel, taken as the strongest channel: a pixel the mark touches on any
    /// channel is a mark pixel.
    #[inline]
    pub(super) fn mark_alpha(&self, pixel: usize) -> f32 {
        let base = pixel * 3;
        (0..3)
            .map(|channel| 1.0 - self.s[base + channel])
            .fold(0.0f32, f32::max)
    }

    /// Worst noise amplification `1/s` over the whole mark. Measured: 1.226 in chapter one, 1.61
    /// in chapter two — a large value here means removal will visibly amplify grain.
    #[must_use]
    pub fn max_noise_gain(&self) -> f32 {
        self.s
            .iter()
            .map(|&value| 1.0 / value.max(S_FLOOR))
            .fold(1.0f32, f32::max)
    }

    /// Peak opacity of the mark, `max(1 - s)` over pixels and channels.
    #[must_use]
    pub fn peak_alpha(&self) -> f32 {
        self.s
            .iter()
            .map(|&value| 1.0 - value)
            .fold(0.0f32, f32::max)
    }

    /// Deposit this model leaves on a reference white background, `255 - compose(255)`, for one
    /// pixel and channel. This is what a measurement of the mark on white sees, so it is the
    /// quantity [`MarkSignature`] compares between marks.
    #[inline]
    fn deposit_at_white(&self, pixel: usize, channel: usize) -> f32 {
        let (c, s) = self.params_at(pixel, channel);
        255.0 - self.operator.compose(c, s, 255.0)
    }

    /// Shape-independent identity of this mark. See [`MarkSignature`] for why identity must not
    /// key on the template.
    #[must_use]
    pub fn signature(&self) -> MarkSignature {
        let pixels = (self.width as usize) * (self.height as usize);
        let mut deposit_chroma = 0.0f32;
        let mut deposit_sum = 0.0f64;
        let mut mark_pixels = 0usize;
        for pixel in 0..pixels {
            if self.mark_alpha(pixel) < ALPHA_SIGNIFICANT {
                continue;
            }
            let deposit: [f32; 3] = std::array::from_fn(|channel| {
                self.deposit_at_white(pixel, channel)
            });
            let hi = deposit.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let lo = deposit.iter().copied().fold(f32::INFINITY, f32::min);
            deposit_chroma = deposit_chroma.max(hi - lo);
            deposit_sum += f64::from(hi.abs().max(lo.abs()));
            mark_pixels += 1;
        }
        let mean_deposit = if mark_pixels == 0 {
            0.0
        } else {
            (deposit_sum / mark_pixels as f64) as f32
        };
        MarkSignature {
            reference_level: 255.0,
            deposit_chroma,
            mean_deposit,
            peak_alpha: self.peak_alpha(),
        }
    }

    /// Bilinear `(c, s)` at a fractional position, clamped to the model's edge. Used for the
    /// subpixel model shift; sampling both planes together shares the interpolation weights.
    pub(super) fn params_bilinear(&self, x: f32, y: f32, channel: usize) -> (f32, f32) {
        let (w, h) = (self.width as usize, self.height as usize);
        let clamped_x = x.clamp(0.0, (w - 1) as f32);
        let clamped_y = y.clamp(0.0, (h - 1) as f32);
        let x0 = clamped_x.floor();
        let y0 = clamped_y.floor();
        let fx = clamped_x - x0;
        let fy = clamped_y - y0;
        // Cast justification: both are clamped into `0..=dim-1` and floored, so they are exact
        // small non-negative integers in f32 (dimensions are far below 2^24).
        let ix0 = x0 as usize;
        let iy0 = y0 as usize;
        let ix1 = (ix0 + 1).min(w - 1);
        let iy1 = (iy0 + 1).min(h - 1);
        let idx = |px: usize, py: usize| (py * w + px) * 3 + channel;
        let (i00, i10, i01, i11) = (
            idx(ix0, iy0),
            idx(ix1, iy0),
            idx(ix0, iy1),
            idx(ix1, iy1),
        );
        let (w00, w10, w01, w11) = (
            (1.0 - fx) * (1.0 - fy),
            fx * (1.0 - fy),
            (1.0 - fx) * fy,
            fx * fy,
        );
        let c = self.c[i00] * w00 + self.c[i10] * w10 + self.c[i01] * w01 + self.c[i11] * w11;
        let s = self.s[i00] * w00 + self.s[i10] * w10 + self.s[i01] * w01 + self.s[i11] * w11;
        (c, s.clamp(S_FLOOR, S_CEIL))
    }
}

impl MarkSignature {
    /// Measure a signature straight from a flat-background sample, before any model exists.
    ///
    /// Returns `None` for a sample whose background is a per-pixel estimate: an estimated
    /// background cannot measure a deposit, it is derived from one. Also `None` for an ASSERTED
    /// level ([`SampleBackground::Manual`]) — deliberately, and unlike every other consumer of a
    /// known level: a signature is the mark's IDENTITY, the key auto-match resolves a whole
    /// chapter against, and a claimed background would let one wrong claim redirect calibration
    /// onto the wrong entry. Identity stays measurement-only; the fit does not have to.
    #[must_use]
    pub fn from_flat_sample(sample: &CalibrationSample) -> Option<Self> {
        let SampleBackground::Flat { level, .. } = sample.background() else {
            return None;
        };
        let pixels = sample.observed.len() / 3;
        let mut deposit_chroma = 0.0f32;
        let mut deposit_sum = 0.0f64;
        let mut mark_pixels = 0usize;
        let mut peak_alpha = 0.0f32;
        for pixel in 0..pixels {
            let deposit: [f32; 3] = std::array::from_fn(|channel| {
                level[channel] - sample.observed_at(pixel, channel)
            });
            let hi = deposit.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let lo = deposit.iter().copied().fold(f32::INFINITY, f32::min);
            let magnitude = hi.abs().max(lo.abs());
            if magnitude < DEPOSIT_SIGNIFICANT_LSB {
                continue;
            }
            for (channel, &background) in level.iter().enumerate() {
                peak_alpha = peak_alpha
                    .max(alpha_lower_bound(background, sample.observed_at(pixel, channel)));
            }
            deposit_chroma = deposit_chroma.max(hi - lo);
            deposit_sum += f64::from(magnitude);
            mark_pixels += 1;
        }
        if mark_pixels == 0 {
            return None;
        }
        Some(Self {
            reference_level: LUMA_R * level[0] + LUMA_G * level[1] + LUMA_B * level[2],
            deposit_chroma,
            mean_deposit: (deposit_sum / mark_pixels as f64) as f32,
            peak_alpha: peak_alpha.clamp(0.0, 1.0 - S_FLOOR),
        })
    }

    /// True when the mark deposits the same amount on every channel — a greyscale mark.
    #[must_use]
    pub fn is_achromatic(&self) -> bool {
        self.deposit_chroma <= ACHROMATIC_DEPOSIT_CHROMA
    }

    /// Opacity gain of this mark measured against `reference`: the ratio of their deposits.
    ///
    /// `None` when the two were measured against different background levels, where the ratio has
    /// no meaning.
    #[must_use]
    pub fn opacity_gain_against(&self, reference: &Self) -> Option<f32> {
        if (self.reference_level - reference.reference_level).abs() > SAME_LEVEL_EPS
            || reference.mean_deposit <= f32::EPSILON
        {
            return None;
        }
        Some(self.mean_deposit / reference.mean_deposit)
    }

    /// True when both signatures describe the SAME mark.
    ///
    /// Fails safe: incomparable signatures (different reference levels, no measurable deposit)
    /// are reported as different marks, because a false "same" is the expensive mistake — it
    /// merges two assets and then removes one with the other's model.
    #[must_use]
    pub fn is_same_mark_as(&self, other: &Self) -> bool {
        let Some(gain) = self.opacity_gain_against(other) else {
            return false;
        };
        self.is_achromatic() == other.is_achromatic()
            && (SAME_MARK_GAIN_MIN..=SAME_MARK_GAIN_MAX).contains(&gain)
    }
}

impl SubpixelShift {
    /// No shift — the occurrence is exactly on the integer grid.
    pub const NONE: Self = Self { dx: 0.0, dy: 0.0 };

    /// Clamp a measured shift into the plausible range; non-finite input becomes `NONE`.
    #[must_use]
    pub fn new(dx: f32, dy: f32) -> Self {
        if !dx.is_finite() || !dy.is_finite() {
            return Self::NONE;
        }
        Self {
            dx: dx.clamp(-MAX_SUBPIXEL_SHIFT, MAX_SUBPIXEL_SHIFT),
            dy: dy.clamp(-MAX_SUBPIXEL_SHIFT, MAX_SUBPIXEL_SHIFT),
        }
    }

    #[must_use]
    pub fn is_zero(self) -> bool {
        self.dx == 0.0 && self.dy == 0.0
    }
}
