/*
File: watermark_chapter.rs

Purpose:
GUI-free engine for chapter-level watermark decomposition: exact, AI-free removal of a
semi-transparent publisher mark by SOLVING its compositing equation from occurrences observed
over different backgrounds, instead of inferring the hidden background with a network.

Model (per pixel, per channel, in the compositing operator's linear domain):

    I = c + s*B        c = alpha*W,  s = 1 - alpha

`c` and `s` are constant across every occurrence of one mark, so several occurrences over
different backgrounds `B_k` overdetermine them; removal is then `B = (I - c)/s` — division,
not inference. See `dev-docs/watermark_chapter_decomposition_plan.md` for the measurements
this design rests on.

Pipeline, per `WatermarkKind` (a chapter may carry several distinct marks):

    validate_calibration_sample  -> a flat ring means B is known EXACTLY there
      -> estimate_model          -> least squares over separated flat samples, Theil-Sen against
                                    estimated backgrounds, or the deposit-exact graded fit
      -> discover_anchors        -> the anchor SET the source stamps at (measured: 1 column in
                                    one chapter, 3 in another)
      -> find_occurrences        -> anchor-band NCC -> per-pixel-background gain test
      -> remove_occurrence       -> f32 division, subpixel model shift, ONE clamp at the end

Key structures:
- `WatermarkKind`: one distinct mark — id, template, samples, fitted model, conditioning.
- `WatermarkModel`: per-pixel per-channel `c`/`s`. Per channel is mandatory for `c`; alpha
  measured channel-neutral on both chapters, but per-channel alpha is still supported.
- `ModelConditioning`: GRADED typed verdict. Samples over separated backgrounds determine both
  `c` and `s`; samples on one level still measure the deposit exactly and only leave the alpha
  scale uncertain, which yields a model plus a stated uncertainty; only a case where not even
  the deposit is measured is refused.
- `AlphaUncertainty` / `AlphaAssumption`: how well the alpha scale is pinned and what that costs
  in LSB, on the measured calibration.
- `MarkSignature`: SHAPE-INDEPENDENT identity of a mark (deposit chroma + opacity gain). Two
  marks can share their artwork pixel for pixel and still be different assets.
- `MarkTemplate`: correlation reference plus the ANCHOR SET the mark is stamped at.
- `CompositingOperator` / `AlphaBlend`: the compositing law behind a trait.
- `Occurrence` / `AcceptanceEvidence`: a detection plus what actually justified accepting it.
- `RegionPatch` / `RemovalResidual`: recovered pixels plus the honest QA numbers.

Key functions:
- `validate_calibration_sample()`, `calibration_sample_from_page()`
- `estimate_model()`, `refit_with_refined_backgrounds()`
- `discover_anchors()`, `find_occurrences()`, `scan_page()`, `scan_chapter()`
- `find_matching_kind()`
- `remove_occurrence()`, `remove_occurrences_on_page()`

Notes:
Everything here is GUI-free and `Send`: no egui, no backend, no user-visible strings. Error
`Display` text is developer diagnostics; the tool layer maps variants to i18n keys. Job
orchestration, the region editor, `CanvasView` patches and persistence belong to
`tools/watermark_removal.rs` and are deliberately NOT this module's business.

Entry invariant (established once, in `validate_page`): page width, height and `w*h*4` all fit
`usize`, and both dimensions fit `u32` (the `image` crate guarantees the latter). Every rect
handled below is validated to lie inside the page, so all downstream flat indexing and the few
documented numeric casts are provably in range.
*/

use std::sync::Arc;

use image::RgbaImage;
use rayon::prelude::*;

// ---------------------------------------------------------------------------------------
// Named thresholds. Every one of these is a decision; the rationale lives with the value.
// ---------------------------------------------------------------------------------------

/// Rec.601 luma weights. Detection correlates on luma: the mark is achromatic on the measured
/// source, and a single plane keeps the anchor-band scan affordable on 800x15500 strips.
const LUMA_R: f32 = 0.299;
const LUMA_G: f32 = 0.587;
const LUMA_B: f32 = 0.114;

/// Thickness of the ring measured around a calibration rect, pixels. Wide enough to average
/// out sensor/JPEG grain, narrow enough that a nearby object does not automatically enter it.
const RING_WIDTH_PX: u32 = 3;
/// A ring thinner than this many pixels cannot support a flatness claim at all.
const MIN_RING_PIXELS: usize = 32;
/// Per-channel std, LSB, below which a ring counts as flat. The measured per-occurrence noise
/// floor of the reference source is 2.3-2.6 LSB rms, so a genuinely flat ring still carries
/// ~2.5 LSB of grain; 3.0 accepts that and refuses anything with real structure.
const FLAT_RING_STD_LIMIT: f32 = 3.0;
/// Largest per-channel deviation of a single ring pixel from the ring mean, LSB. Std alone
/// cannot see a slow gradient across a thin ring; this can.
const FLAT_RING_MAX_DEV_LIMIT: f32 = 12.0;

/// Two background levels closer than this count as the SAME level when the verdict reports
/// distinct levels. Just above the noise floor.
const SAME_LEVEL_EPS: f32 = 4.0;
/// Minimum separation of background levels for the SLOPE to be worth fitting, LSB. The slope
/// error is about `sigma*sqrt(2)/spread`; with the measured ~2.5 LSB noise floor, 64 LSB bounds a
/// single pair's alpha error at ~0.055, already comparable with the mark's own alpha. Below this
/// the fitted slope would be worse than the deposit-exact graded fit, which is what the engine
/// falls back to — it does NOT refuse: the second measured chapter has every sample on pure white
/// and its pale mark still removes at 1.92 LSB rms there.
const MIN_BACKGROUND_SPREAD: f32 = 64.0;
/// Per-occurrence noise floor of the measured sources, LSB rms (2.3-2.6 in chapter one, 2.66
/// unclamped in chapter two — the same "resampled after stamping" rasterization floor). Used to
/// turn a fitted slope's background spread into an honest alpha uncertainty.
const FIT_NOISE_LSB: f32 = 2.5;
/// Minimum spread between the two members of a Theil-Sen pair, LSB. Pairs closer than this
/// produce slopes dominated by noise and are skipped.
const MIN_PAIR_SPREAD: f32 = 16.0;
/// Physical bounds of `s = 1 - alpha`. The floor caps the noise amplification `1/s` at 20x;
/// a fitted value outside the range is unphysical under alpha compositing, so it is clamped
/// and COUNTED in `ModelProvenance::clamped_pixels` rather than silently accepted.
const S_FLOOR: f32 = 0.05;
const S_CEIL: f32 = 1.0;
/// A pixel counts as part of the mark once `alpha` exceeds this. Below it the mark moves the
/// pixel by under ~5 LSB at full contrast, i.e. under the measured noise floor.
const ALPHA_SIGNIFICANT: f32 = 0.02;
/// The same threshold expressed as a deposit, LSB, for the case where no model exists yet and
/// only an observation over a known background is available: `ALPHA_SIGNIFICANT` at full contrast.
const DEPOSIT_SIGNIFICANT_LSB: f32 = ALPHA_SIGNIFICANT * 255.0;

// --- Cost of an alpha-scale error -------------------------------------------------------
// Once the deposit `D = B - I` is measured exactly, a wrong alpha scale is exactly what is left:
// the recovery error is `delta_alpha * (B - B0) / (1 - alpha)`, i.e. zero at the calibration
// level and linear in the distance from it. Chapter two measured that cost directly.

/// Recovery error caused by one percent of relative alpha error, LSB rms over the mark.
/// Measured (chapter two, pale mark): +-5% -> 1.6 LSB rms, +-10% -> 3.2, +-20% -> 6.4 — linear,
/// as the formula above predicts.
const ALPHA_ERROR_LSB_PER_PERCENT: f32 = 0.32;
/// Background luma below which the "dark background" figures below were measured.
const DARK_BACKGROUND_LUMA: f32 = 80.0;
/// Multiplier from the overall rms error to the rms error on backgrounds darker than
/// `DARK_BACKGROUND_LUMA`. Measured: +-10% costs 3.2 LSB overall but 4.7 LSB on luma < 80.
const DARK_BACKGROUND_RMS_FACTOR: f32 = 4.7 / 3.2;
/// Multiplier from the overall rms error to the WORST single-pixel error on those backgrounds.
/// Measured: +-10% costs 3.2 LSB rms overall and reaches 13 LSB on luma < 80.
const DARK_BACKGROUND_MAX_FACTOR: f32 = 13.0 / 3.2;

// --- Alpha scale when the samples do not pin it ------------------------------------------

/// How far above the deposit's own lower bound the assumed peak opacity is placed when nothing
/// pins the alpha scale.
///
/// `alpha >= D/B` is a HARD bound implied by the exact deposit (it is the `W = 0` solution, the
/// weakest mark that can deposit `D`); the truth sits above it by `1/(1 - W/B)`, which no amount
/// of data at ONE background level can see. Measured: chapter one's mark carries a near-black
/// outline, where the bound is nearly exact (ratio ~1.0); chapter two's pale mark peaks at
/// alpha 0.378 against a deposit bound near 0.24 (ratio ~1.6). 1.3 sits between them, so the
/// assumption is within ~25% of both measured chapters.
const ASSUMED_ALPHA_OVER_DEPOSIT_BOUND: f32 = 1.3;
/// Honest relative uncertainty of that assumption, percent of alpha. It has to cover the whole
/// measured ratio span above (+-23%) with margin, and the resulting 14 LSB rms on dark
/// backgrounds matches the plan's "up to ~14 LSB on near-black backgrounds" for this fallback.
/// Do NOT lower it: the removal is exact at the calibration level whatever alpha is assumed, so
/// nothing in the data would object.
const ASSUMED_ALPHA_UNCERTAINTY_PERCENT: f32 = 30.0;
/// Honest relative uncertainty of an alpha fitted against per-pixel background ESTIMATES,
/// percent. The plan's measurement: the iterated estimator does not converge to the truth, it
/// CROSSES it, so its accuracy is +-10-20% and this takes the pessimistic end.
const ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT: f32 = 20.0;
/// Iterations of the estimated-background refinement loop.
///
/// This is a FREE PARAMETER, not a converged fixed point, and the number is measured rather than
/// derived: the estimate crosses the truth at +0.6% of alpha after 32 iterations, +10.1% after
/// 64 and +18.1% after 96. Nothing in the data selects a stopping point, which is precisely why
/// this path is worth only `ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT` and says so in the
/// verdict. Self-calibrating the scale by regressing the residual against the imprint was tried
/// and refuted (biased 26-28% low at its fixed point); do not re-invent it.
const BACKGROUND_REFINEMENT_ITERATIONS: usize = 32;

// --- Mark identity, which is NOT the template's shape -------------------------------------

/// Deposit chroma at or below which a mark counts as achromatic, LSB. Measured in chapter two,
/// where a colour mark and a greyscale one share the same artwork (band-pass NCC >= 0.97): the
/// colour mark's deposit differs by up to 120 LSB between channels, the pale one by at most 8.
/// The threshold sits well clear of both.
const ACHROMATIC_DEPOSIT_CHROMA: f32 = 24.0;
/// Opacity-gain band inside which two marks may still be the same asset. Measured: the pale mark
/// scores 0.365-0.515 against the colour model while the colour mark scores 0.999-1.000 against
/// its own, so this band alone separates the two.
const SAME_MARK_GAIN_MIN: f32 = 0.80;
const SAME_MARK_GAIN_MAX: f32 = 1.25;

/// Half-width of the anchor band, pixels, and the accept-rule guard. Measured alignment is
/// integer +-1 px on both sources. Chapter two settled the value: 9 of 147 candidates were false,
/// all of them off-anchor and/or below `FALSE_ACCEPT_GAIN_FLOOR`, and `(anchor +-2) AND
/// (gain >= 0.35)` removed all 9 with zero loss of true occurrences. Removal at a false accept
/// INJECTS an inverse mark into content, so this is a precision guard, not a recall preference.
const ANCHOR_TOLERANCE_PX: u32 = 2;
/// Hard upper bound on a hand-supplied anchor tolerance. A persisted or hand-edited detection
/// configuration may widen the band a little for a jitterier source, but not into "anywhere".
const MAX_ANCHOR_TOLERANCE_PX: u32 = 16;
/// Absolute gain floor of the accept rule, below the configurable window. The measured false
/// candidates sat under 0.35 while true occurrences regress to 1.0, so no configuration may
/// accept below this — see `ANCHOR_TOLERANCE_PX` for the same measurement.
const FALSE_ACCEPT_GAIN_FLOOR: f32 = 0.35;

// --- Anchor discovery ---------------------------------------------------------------------

/// Box-average factor of the coarse anchor-discovery scan. Anchor columns are DATA (one column
/// in chapter one, three — x = 48, 278, 523 — in chapter two), so they must be found rather than
/// assumed; a full-resolution full-width correlation over a 690x18000 strip is not affordable,
/// and 4x cuts it by ~256.
const ANCHOR_DISCOVERY_DOWNSCALE: u32 = 4;
/// Smallest downscaled template side the coarse scan can still correlate on. Below it the scan
/// runs at full resolution instead of blurring the mark away.
const MIN_DISCOVERY_TEMPLATE_SIDE: u32 = 4;
/// Correlation floor of the coarse scan. Lower than `COARSE_NCC_MIN` because box-averaging by 4
/// blurs the mark's edges; this is a bootstrap whose output is confirmed by the gain-verified
/// rescan, so recall matters more than precision here.
const ANCHOR_DISCOVERY_NCC_MIN: f32 = 0.45;
/// Columns within this distance of each other belong to the same anchor.
const ANCHOR_CLUSTER_RADIUS_PX: u32 = 3;
/// How many occurrences must share a column before it counts as an anchor. Measured: chapter one
/// stamps 47 occurrences at one column, chapter two 22-24 per page over three, so a real anchor
/// is never supported by a single hit while a content coincidence usually is.
const ANCHOR_MIN_SUPPORT: usize = 2;
/// Cap on coarse hits carried into full-resolution refinement per page, and on the hits collected
/// over all pages handed in. Bounds both the quadratic non-maximum suppression and the refinement
/// cost on a page whose content happens to correlate everywhere.
const MAX_DISCOVERY_HITS: usize = 4096;
/// Correlation floor for a candidate to reach the gain test. This is a RECALL gate whose only
/// job is bounding the cost of the real test; it is deliberately loose because a true
/// occurrence over busy content correlates poorly.
const COARSE_NCC_MIN: f32 = 0.50;
/// Correlation floor when the kind has NO fitted model yet and the gain test cannot run. Such
/// accepts are evidence `Correlation` and are NOT removal-safe; the threshold is strict because
/// correlation alone is the weakest evidence this engine has.
const NCC_ONLY_MIN: f32 = 0.85;
/// Accepted gain window of the per-pixel-background regression. Measured on real data: true
/// occurrences regress to 1.0, and a false accept INJECTS an inverse mark, so the window is
/// tight and biased toward precision over recall.
const GAIN_MIN: f32 = 0.80;
const GAIN_MAX: f32 = 1.15;
/// Scale used to turn |gain - 1| into a comparable fitness when two kinds claim the same spot.
/// Slightly wider than the window's half-width so an accept at the edge still scores above 0.
const GAIN_FITNESS_SCALE: f32 = 0.25;
/// Minimum detection statistic `g*sqrt(sum m^2) / sigma_residual` (a matched-filter t-statistic,
/// which grows with the pixel count). The prototype measured 40-100 for true occurrences over
/// busy content; content coincidences scored COMPARABLY, which is why this only rejects
/// noise-level candidates and the gain window does the discriminating work.
const MIN_DETECTION_T: f32 = 20.0;
/// Fewest mark pixels the gain regression may be fitted from.
const MIN_GAIN_PIXELS: usize = 24;
/// Radius of the background estimator's box blur, pixels. Large enough to average out grain,
/// small enough not to smear real background structure across the mark footprint.
const BACKGROUND_BLUR_RADIUS_PX: u32 = 4;
/// Weight floor of the background estimator. Weights are `s`, so pixels the model calls opaque
/// contribute almost nothing; the floor keeps the weighted mean defined even inside a fully
/// opaque window (where the provisional removal is genuinely all that is known).
const BACKGROUND_WEIGHT_FLOOR: f32 = 0.02;
/// Two detections overlapping by more than this IoU are the same detection.
const OVERLAP_IOU_LIMIT: f32 = 0.30;
/// Cap on candidates carried into the gain test per page. The measured chapter held 47
/// occurrences across 5 strips; 64 per page is generous while bounding the cost.
const MAX_CANDIDATES_PER_PAGE: usize = 64;
/// Largest subpixel model shift the refinement may report, pixels. Measured jitter is <= 0.4 px;
/// anything larger is an integer misalignment, which is detection's problem, not removal's.
const MAX_SUBPIXEL_SHIFT: f32 = 1.0;
/// Half of one quantization step. A recovered background may leave `[0, 255]` by up to
/// `QUANT_SLACK/s` purely because the observation was rounded to a byte; calling that
/// "clipped" would be wrong, so only excursions beyond that count as irrecoverable.
const QUANT_SLACK: f32 = 0.5;

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Typed failures of the watermark engine. Public entry points validate shapes, rects and
/// buffer lengths and return one of these instead of panicking (CLAUDE.md §11).
///
/// `Display` text is developer diagnostics for logs and the console; the tool layer maps
/// variants to localized messages.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(super) enum WatermarkError {
    /// The page has a zero dimension, or its dimensions do not fit this platform's `usize`.
    #[error("unusable page geometry {width}x{height}")]
    PageGeometry { width: u32, height: u32 },
    /// The rect has a zero dimension.
    #[error("empty rect {rect:?}")]
    EmptyRect { rect: PixelRect },
    /// The rect leaves the page.
    #[error("rect {rect:?} does not fit inside the {width}x{height} page")]
    RectOutOfPage { rect: PixelRect, width: u32, height: u32 },
    /// A template patch with no contrast cannot be correlated against anything.
    #[error("template patch {width}x{height} is flat and cannot be matched")]
    FlatTemplate { width: u32, height: u32 },
    /// A template was handed an empty anchor set. Detection is only affordable and only precise
    /// because it is restricted to the columns the source stamps at, so "no anchors" is refused
    /// rather than silently turned into a full-width scan.
    #[error("anchor set is empty")]
    EmptyAnchorSet,
    /// Two things that must share a footprint do not.
    #[error("geometry mismatch: expected {expected_width}x{expected_height}, got {width}x{height}")]
    GeometryMismatch {
        expected_width: u32,
        expected_height: u32,
        width: u32,
        height: u32,
    },
    /// A supplied buffer does not have the length its declared shape requires.
    #[error("buffer length {len} does not match the required {expected} for {what}")]
    BufferLength {
        what: &'static str,
        len: usize,
        expected: usize,
    },
    /// A model parameter is NaN/infinite or physically impossible.
    #[error("parameter {what} = {value} is out of range at index {index}")]
    ParameterOutOfRange {
        what: &'static str,
        index: usize,
        value: f32,
    },
    /// Removal was asked for an occurrence that only correlation ever vouched for. Applying it
    /// would inject an inverse mark wherever detection was wrong, so it is refused.
    #[error("occurrence at {rect:?} was not verified by the gain test and must not be removed")]
    UnverifiedOccurrence { rect: PixelRect },
    /// `estimate_model` was called without any sample, or refinement without a model.
    #[error("no calibration samples")]
    NoSamples,
    /// Background refinement was asked for a kind whose calibration backgrounds are all exact.
    /// Replacing them with estimates would throw away the only hard evidence in the set.
    #[error("no estimated-background samples to refine")]
    NothingToRefine,
}

/// Why `estimate_model` produced no model: either the inputs were invalid, or they were valid
/// but not even the DEPOSIT was measured anywhere, which is the only case the engine refuses.
///
/// The two are kept apart on purpose: the first is a caller bug, the second is a normal state
/// the UI must turn into a concrete instruction ("add a sample over a flat area"). A merely
/// under-determined ALPHA SCALE is not a failure at all — it yields a model plus a stated
/// [`AlphaUncertainty`] (see [`ModelConditioning::DepositExact`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(super) enum ModelFitError {
    #[error("invalid calibration input: {0}")]
    Invalid(#[from] WatermarkError),
    #[error("nothing about the mark was measured: {0:?}")]
    Refused(ModelConditioning),
}

impl ModelFitError {
    /// The conditioning verdict, when the failure was conditioning rather than bad input.
    pub fn conditioning(&self) -> Option<&ModelConditioning> {
        match self {
            Self::Invalid(_) => None,
            Self::Refused(verdict) => Some(verdict),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------------------

/// Axis-aligned rectangle in page pixel coordinates. All engine rects are validated to lie
/// fully inside their page before any indexing happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    #[must_use]
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Exclusive right edge. `u64` because `x + width` may leave `u32`.
    #[must_use]
    pub fn right(self) -> u64 {
        u64::from(self.x) + u64::from(self.width)
    }

    /// Exclusive bottom edge.
    #[must_use]
    pub fn bottom(self) -> u64 {
        u64::from(self.y) + u64::from(self.height)
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    #[must_use]
    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Intersection-over-union with `other`; 0.0 when either rect is empty.
    #[must_use]
    pub fn iou(self, other: Self) -> f32 {
        let inter = self.intersection_area(other);
        if inter == 0 {
            return 0.0;
        }
        let union = self.area() + other.area() - inter;
        if union == 0 {
            return 0.0;
        }
        ratio_u64(inter, union)
    }

    fn intersection_area(self, other: Self) -> u64 {
        let x0 = u64::from(self.x).max(u64::from(other.x));
        let y0 = u64::from(self.y).max(u64::from(other.y));
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            return 0;
        }
        (x1 - x0) * (y1 - y0)
    }
}

/// `numerator / denominator` as f32 for area ratios. Areas here are bounded by the page area,
/// so the conversion is used for a ratio only and a 1-ulp rounding is irrelevant.
fn ratio_u64(numerator: u64, denominator: u64) -> f32 {
    if denominator == 0 {
        return 0.0;
    }
    // Cast justification: both operands are pixel areas of a real image; f64 represents them
    // exactly up to 2^53, far beyond any page this program can open.
    (numerator as f64 / denominator as f64) as f32
}

/// Pixel count -> f32 for means and variances. Exact below 2^24 (a 16.7 Mpx region); beyond
/// that the count rounds by at most one, which cannot matter to a mean.
#[inline]
fn count_f32(n: usize) -> f32 {
    n as f32
}

/// Validate the entry invariant for a page and return its dimensions as `usize`.
fn validate_page(page: &RgbaImage) -> Result<(usize, usize), WatermarkError> {
    let (width, height) = (page.width(), page.height());
    let fits = width > 0
        && height > 0
        && usize::try_from(width).is_ok()
        && usize::try_from(height).is_ok()
        && page.as_raw().len() == (width as usize) * (height as usize) * 4;
    if !fits {
        return Err(WatermarkError::PageGeometry { width, height });
    }
    Ok((width as usize, height as usize))
}

/// Validate that `rect` is non-empty and lies fully inside `page`.
fn validate_rect(page: &RgbaImage, rect: PixelRect) -> Result<(), WatermarkError> {
    if rect.is_empty() {
        return Err(WatermarkError::EmptyRect { rect });
    }
    if rect.right() > u64::from(page.width()) || rect.bottom() > u64::from(page.height()) {
        return Err(WatermarkError::RectOutOfPage {
            rect,
            width: page.width(),
            height: page.height(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Compositing operator
// ---------------------------------------------------------------------------------------

/// The compositing law a mark was stamped with.
///
/// The whole engine fits and inverts `I = c + s*B` in the operator's LINEAR domain; an operator
/// therefore only has to say how to enter and leave that domain. Alpha blending is the identity
/// there, but multiply and gamma marks exist in the wild and would be confidently mis-removed
/// under an alpha-blend assumption — hence a trait rather than a hardcoded formula.
///
/// Implementations must be pure, cheap, and `Send + Sync`: they are called per pixel per channel
/// from worker threads.
pub(super) trait CompositingOperator: std::fmt::Debug + Send + Sync {
    /// Stable literal identity, persisted alongside a fitted model. Never localized.
    fn id(&self) -> &'static str;

    /// Map an observed 0..=255 value into the domain where the mark is linear in `B`.
    fn to_linear(&self, value: f32) -> f32;

    /// Inverse of [`CompositingOperator::to_linear`]: back from the linear domain to an
    /// observable 0..=255 value.
    fn to_observed(&self, linear: f32) -> f32;

    /// Forward composite of background `background` under parameters `c`/`s`.
    fn compose(&self, c: f32, s: f32, background: f32) -> f32 {
        self.to_observed(c + s * self.to_linear(background))
    }

    /// Recover the background from an observation. `s` is guaranteed `>= S_FLOOR` by
    /// [`WatermarkModel`] validation; the guard only protects hand-built callers.
    fn decompose(&self, c: f32, s: f32, observed: f32) -> f32 {
        let linear = self.to_linear(observed) - c;
        if s.abs() < f32::EPSILON {
            return self.to_observed(linear);
        }
        self.to_observed(linear / s)
    }
}

/// Ordinary source-over alpha blending, `I = alpha*W + (1-alpha)*B`. The linear domain is the
/// sample value itself, so both transfers are the identity.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct AlphaBlend;

impl CompositingOperator for AlphaBlend {
    fn id(&self) -> &'static str {
        "alpha_blend"
    }

    fn to_linear(&self, value: f32) -> f32 {
        value
    }

    fn to_observed(&self, linear: f32) -> f32 {
        linear
    }
}

/// The default operator as a shareable handle.
#[must_use]
pub(super) fn alpha_blend_operator() -> Arc<dyn CompositingOperator> {
    Arc::new(AlphaBlend)
}

// ---------------------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------------------

/// The correlation reference of one mark: a zero-mean luma patch plus the SET of page columns the
/// mark is anchored at. The anchor set is what makes detection affordable and precise — a source
/// stamps every occurrence at one of a few fixed columns, so content elsewhere never even reaches
/// the gain test.
///
/// The set is data, not a constant: chapter one uses a single column (x = 662), chapter two three
/// (x = 48, 278, 523). It comes from the picked sample plus [`discover_anchors`], and anything
/// that keys or persists a model must key on the whole set — see [`MarkTemplate::anchor_key`].
#[derive(Debug, Clone)]
pub(super) struct MarkTemplate {
    width: u32,
    height: u32,
    /// Anchor columns, ascending and deduplicated, never empty.
    anchors: Vec<u32>,
    /// Zero-mean luma of the reference patch, row major, `width*height` entries.
    centered: Vec<f32>,
    /// L2 norm of `centered`; validated non-zero at construction.
    norm: f32,
}

impl MarkTemplate {
    /// Cut a template out of `rect` on `page`. The rect's x becomes the initial anchor set — one
    /// column, the one the user pointed at; [`MarkTemplate::set_anchors`] widens it to whatever
    /// the data turns out to hold.
    ///
    /// # Errors
    /// [`WatermarkError::RectOutOfPage`] / [`WatermarkError::EmptyRect`] for a bad rect,
    /// [`WatermarkError::FlatTemplate`] when the patch has no contrast to correlate on.
    pub fn from_page(page: &RgbaImage, rect: PixelRect) -> Result<Self, WatermarkError> {
        let (pw, _ph) = validate_page(page)?;
        validate_rect(page, rect)?;
        let raw = page.as_raw();
        let (tw, th) = (rect.width as usize, rect.height as usize);
        let mut centered = Vec::with_capacity(tw * th);
        for row in 0..th {
            let base = ((rect.y as usize + row) * pw + rect.x as usize) * 4;
            for px in raw[base..base + tw * 4].chunks_exact(4) {
                centered.push(luma_of(px[0], px[1], px[2]));
            }
        }
        let mean = centered.iter().sum::<f32>() / count_f32(centered.len());
        for value in &mut centered {
            *value -= mean;
        }
        let norm = centered.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            return Err(WatermarkError::FlatTemplate {
                width: rect.width,
                height: rect.height,
            });
        }
        Ok(Self {
            width: rect.width,
            height: rect.height,
            anchors: vec![rect.x],
            centered,
            norm,
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

    /// Page columns occurrences of this mark are expected at, ascending and deduplicated.
    #[must_use]
    pub fn anchors(&self) -> &[u32] {
        &self.anchors
    }

    /// Replace the anchor set with one discovered from the data.
    ///
    /// Values are sorted and columns within `ANCHOR_CLUSTER_RADIUS_PX` of one another are merged,
    /// so a caller may hand in raw per-occurrence x positions.
    ///
    /// # Errors
    /// [`WatermarkError::EmptyAnchorSet`] for an empty input — a template with no anchor would
    /// turn detection into a full-width scan, which is neither affordable nor precise.
    pub fn set_anchors(&mut self, anchors: &[u32]) -> Result<(), WatermarkError> {
        let merged = cluster_columns(anchors, ANCHOR_CLUSTER_RADIUS_PX);
        if merged.is_empty() {
            return Err(WatermarkError::EmptyAnchorSet);
        }
        self.anchors = merged;
        Ok(())
    }

    /// Stable literal key of the anchor SET, for persistence and cache lookup.
    ///
    /// A model belongs to a source, and a source is identified by where it stamps: two chapters
    /// of the same publisher with different anchor sets are different layouts and must not share
    /// a cached model. Never localized, never parsed back.
    #[must_use]
    pub fn anchor_key(&self) -> String {
        self.anchors
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// True when `x` sits within `tolerance` of one of the anchor columns. This is half of the
    /// accept rule, not merely a scan restriction: see `ANCHOR_TOLERANCE_PX`.
    fn is_on_anchor(&self, x: u32, tolerance: u32) -> bool {
        self.anchors
            .iter()
            .any(|&anchor| x.abs_diff(anchor) <= tolerance)
    }

    /// This template as a bare correlation reference.
    fn correlation_ref(&self) -> CorrelationRef<'_> {
        CorrelationRef {
            width: self.width as usize,
            height: self.height as usize,
            centered: &self.centered,
            norm: self.norm,
        }
    }
}

/// A zero-mean patch plus its L2 norm: everything [`ncc_patch`] needs to correlate against, so
/// the template and its downscaled copy share one kernel.
#[derive(Debug, Clone, Copy)]
struct CorrelationRef<'a> {
    width: usize,
    height: usize,
    centered: &'a [f32],
    norm: f32,
}

/// Sort `columns`, merge everything within `radius` into one representative (the rounded mean of
/// the cluster) and return the result ascending. Empty input yields an empty result.
fn cluster_columns(columns: &[u32], radius: u32) -> Vec<u32> {
    let mut sorted: Vec<u32> = columns.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<u32> = Vec::new();
    let mut cluster: Vec<u32> = Vec::new();
    for &column in &sorted {
        match cluster.last() {
            Some(&last) if column - last <= radius => cluster.push(column),
            Some(_) => {
                out.push(mean_column(&cluster));
                cluster.clear();
                cluster.push(column);
            }
            None => cluster.push(column),
        }
    }
    if !cluster.is_empty() {
        out.push(mean_column(&cluster));
    }
    out.dedup();
    out
}

/// Rounded mean of a non-empty column cluster. `u64` sums keep the mean exact for any page width.
fn mean_column(cluster: &[u32]) -> u32 {
    let count = cluster.len() as u64;
    let sum: u64 = cluster.iter().map(|&value| u64::from(value)).sum();
    // Cast justification: the mean of values that all fit u32 fits u32.
    ((sum + count / 2) / count) as u32
}

/// Rec.601 luma of one RGB triple.
#[inline]
fn luma_of(r: u8, g: u8, b: u8) -> f32 {
    LUMA_R * f32::from(r) + LUMA_G * f32::from(g) + LUMA_B * f32::from(b)
}

// ---------------------------------------------------------------------------------------
// Calibration samples
// ---------------------------------------------------------------------------------------

/// What the engine knows about the background under one calibration sample.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SampleBackground {
    /// The ring around the mark is uniform, so `B` is known exactly and is the same everywhere
    /// under the footprint. This is the strongest input the estimator can receive.
    Flat { level: [f32; 3], ring_std: [f32; 3] },
    /// `B` was estimated per pixel (e.g. by [`provisional_background`] from an earlier model).
    /// Interleaved RGB, `width*height*3` entries.
    Estimated { values: Vec<f32> },
}

/// One occurrence used to fit the model: the observed pixels under the mark plus what is known
/// about the background there.
#[derive(Debug, Clone)]
pub(super) struct CalibrationSample {
    page_index: usize,
    rect: PixelRect,
    /// Observed RGB under the footprint, interleaved, `width*height*3` entries, 0..=255.
    observed: Vec<f32>,
    background: SampleBackground,
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
            for px in raw[base..base + rect.width as usize * 4].chunks_exact(4) {
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

    /// True when `B` is known exactly (flat ring), which is what the closed form needs.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        matches!(self.background, SampleBackground::Flat { .. })
    }

    #[inline]
    fn observed_at(&self, pixel: usize, channel: usize) -> f32 {
        self.observed[pixel * 3 + channel]
    }

    #[inline]
    fn background_at(&self, pixel: usize, channel: usize) -> f32 {
        match &self.background {
            SampleBackground::Flat { level, .. } => level[channel],
            SampleBackground::Estimated { values } => values[pixel * 3 + channel],
        }
    }

    /// Mean background luma over the footprint — the single number the UI shows as "this
    /// sample sits on that level".
    fn mean_background_luma(&self) -> f32 {
        match &self.background {
            SampleBackground::Flat { level, .. } => {
                LUMA_R * level[0] + LUMA_G * level[1] + LUMA_B * level[2]
            }
            SampleBackground::Estimated { values } => {
                let pixels = values.len() / 3;
                if pixels == 0 {
                    return 0.0;
                }
                let sum: f32 = values
                    .chunks_exact(3)
                    .map(|px| LUMA_R * px[0] + LUMA_G * px[1] + LUMA_B * px[2])
                    .sum();
                sum / count_f32(pixels)
            }
        }
    }
}

/// Why a picked rect cannot serve as anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SampleRejection {
    /// The rect is empty or leaves the page.
    BadRect,
    /// Too few ring pixels survived clipping at the page edge to judge flatness.
    RingTooSmall { pixels: usize, needed: usize },
}

/// Verdict on a user-picked (or auto-collected) sample rect.
///
/// The distinction that matters: a sample whose ring is NOT uniform is refused as a calibration
/// target — its `B` is unknown, so feeding it to the estimator would poison `c` and `s` — but it
/// is still a perfectly good detection template. The variant says which.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SampleVerdict {
    /// Ring uniform: `B` is known exactly. Usable both as calibration and as a template.
    Calibration {
        level: [f32; 3],
        ring_std: [f32; 3],
        ring_pixels: usize,
    },
    /// Ring not uniform: REFUSED as calibration, usable as a detection template.
    TemplateOnly {
        level: [f32; 3],
        ring_std: [f32; 3],
        ring_max_dev: [f32; 3],
        std_limit: f32,
        max_dev_limit: f32,
        ring_pixels: usize,
    },
    /// Unusable for anything.
    Unusable { reason: SampleRejection },
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

/// Tunables of the ring measurement. Defaults are the named constants above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SampleParams {
    pub ring_width: u32,
    pub min_ring_pixels: usize,
    pub std_limit: f32,
    pub max_dev_limit: f32,
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
/// The ring is the annulus of `params.ring_width` pixels around `rect`, clipped to the page. A
/// uniform ring is what licenses reading `B` directly; a structured one means the background
/// under the mark is unknown, and the engine says so instead of guessing.
///
/// Never panics: a bad rect or a starved ring returns [`SampleVerdict::Unusable`].
#[must_use]
pub(super) fn validate_calibration_sample(
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

    let flat = std.iter().all(|&v| v <= params.std_limit)
        && max_dev.iter().all(|&v| v <= params.max_dev_limit);
    if flat {
        SampleVerdict::Calibration {
            level,
            ring_std: std,
            ring_pixels: count,
        }
    } else {
        SampleVerdict::TemplateOnly {
            level,
            ring_std: std,
            ring_max_dev: max_dev,
            std_limit: params.std_limit,
            max_dev_limit: params.max_dev_limit,
            ring_pixels: count,
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
pub(super) fn calibration_sample_from_page(
    page: &RgbaImage,
    page_index: usize,
    rect: PixelRect,
    params: &SampleParams,
) -> Result<(SampleVerdict, Option<CalibrationSample>), WatermarkError> {
    let verdict = validate_calibration_sample(page, rect, params);
    let sample = match &verdict {
        SampleVerdict::Calibration {
            level, ring_std, ..
        } => Some(CalibrationSample::from_page(
            page,
            page_index,
            rect,
            SampleBackground::Flat {
                level: *level,
                ring_std: *ring_std,
            },
        )?),
        SampleVerdict::TemplateOnly { .. } | SampleVerdict::Unusable { .. } => None,
    };
    Ok((verdict, sample))
}

// ---------------------------------------------------------------------------------------
// Conditioning
// ---------------------------------------------------------------------------------------

/// What the UI should ask the user for when a sample would improve the model — the concrete half
/// of every non-`Separable` verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum SuggestedBackground {
    /// Collect an occurrence whose background luma is at or below this level.
    Darker { at_most: f32 },
    /// Collect an occurrence whose background luma is at or above this level.
    Brighter { at_least: f32 },
}

/// Where a model's alpha scale came from — the one quantity that stays uncertain once the
/// deposit is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AlphaSource {
    /// Fitted from samples over well-separated backgrounds, where the slope is data.
    SeparatedBackgrounds,
    /// Fitted against per-pixel background ESTIMATES, which are the model's own output one
    /// iteration earlier. Worth `ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT`, no better.
    EstimatedBackgrounds,
    /// Not determined by the data at all: the deposit is exact and the scale is an assumption,
    /// anchored on the deposit's own lower bound or stated by the caller.
    Assumed,
}

/// How well the alpha scale is pinned down, and what that costs in the output.
///
/// The recovery error of a wrong alpha scale is `delta_alpha * (B - B0) / (1 - alpha)`: zero at
/// the background level the model was calibrated on and linear in the distance from it. The LSB
/// figures below are that relation as MEASURED on chapter two, not as re-derived here — see
/// `ALPHA_ERROR_LSB_PER_PERCENT` and the two dark-background factors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct AlphaUncertainty {
    pub source: AlphaSource,
    /// Relative uncertainty of the alpha scale, percent of alpha.
    pub percent: f32,
    /// Recovery error it causes, LSB rms over the mark, on an arbitrary background.
    pub rms_lsb: f32,
    /// The same on backgrounds darker than `DARK_BACKGROUND_LUMA`, LSB rms.
    pub dark_rms_lsb: f32,
    /// Worst single-pixel recovery error on those backgrounds, LSB.
    pub dark_max_lsb: f32,
    /// Background luma below which the two `dark_*` figures apply.
    pub dark_luma: f32,
}

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

/// What the caller already knows about the mark's peak opacity when the samples cannot pin it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) enum AlphaAssumption {
    /// Nothing. The engine anchors the assumption on the deposit's own lower bound — see
    /// `ASSUMED_ALPHA_OVER_DEPOSIT_BOUND`. This is the default: an unstated assumption is the
    /// weakest claim available, never a borrowed number.
    #[default]
    FromDeposit,
    /// A peak opacity from outside this fit (a persisted model of the same source, a sibling kind
    /// of the catalog, a user-supplied number) together with its honest relative uncertainty in
    /// percent. Both are used verbatim, so an over-confident caller produces an over-confident
    /// verdict; state the uncertainty you can defend.
    Stated {
        peak_alpha: f32,
        uncertainty_percent: f32,
    },
}

/// Typed, GRADED conditioning verdict for a set of calibration samples.
///
/// `c` and `s` are two unknowns per pixel per channel, so two observations over sufficiently
/// different backgrounds determine both. One background level determines LESS but not nothing:
/// the deposit `D = B - I` is still measured exactly there, and removal at that level is exact
/// whatever the alpha scale turns out to be — measured, chapter two removes its pale mark at
/// 1.92 LSB rms with every calibration sample on pure white. The verdict therefore GRADES the
/// state instead of refusing: it carries the observed levels, their spread, and the resulting
/// [`AlphaUncertainty`], and names the sample that would collapse it. Only a case where not even
/// the deposit was measured produces no model.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ModelConditioning {
    /// Both `c` and `s` come from the data: levels far enough apart, every pixel determined.
    Separable {
        levels: Vec<f32>,
        spread: f32,
        min_pixel_spread: f32,
        alpha: AlphaUncertainty,
    },
    /// GRADED: every calibration sample has an exactly known background, so the deposit is exact
    /// and removal is exact at the observed level(s) — but no pair of levels is far enough apart
    /// to pin the slope, so the alpha scale is an assumption worth `alpha`.
    DepositExact {
        levels: Vec<f32>,
        spread: f32,
        samples: usize,
        alpha: AlphaUncertainty,
    },
    /// REFUSED: nothing at all was measured — no samples, or a single sample whose background is
    /// itself an estimate.
    NotEnoughSamples { have: usize, need: usize },
    /// REFUSED: every background is a per-pixel ESTIMATE and they do not span enough levels, so
    /// neither the deposit nor the slope is measured anywhere.
    DepositUnavailable { samples: usize, spread: f32 },
    /// REFUSED: backgrounds are per-pixel estimates and some pixel never saw two of them. Its
    /// `c`/`s` would be a guess dressed as a fit.
    Underdetermined {
        levels: Vec<f32>,
        underdetermined_pixels: usize,
        total_pixels: usize,
        worst_pixel_spread: f32,
        required: f32,
    },
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

/// How a model's parameters were obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FitMethod {
    /// Least squares over flat-background samples, where `B` is known exactly. With exactly two
    /// levels this is the closed form `c = I|B=0`, `s = (I|B=255 - I|B=0)/255`.
    ClosedFormFlat,
    /// Theil-Sen median-of-slopes against per-pixel background estimates. Robust to a minority
    /// of bad samples, used when the flat samples do not span enough levels.
    TheilSen,
    /// Deposit-exact graded fit: `c = mean(I - s*B)` over flat samples with the alpha scale
    /// assumed rather than fitted. Exact at the observed level, uncertain away from it.
    DepositExact,
}

/// Where a fitted model came from, kept with it for reporting and for progressive refinement.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ModelProvenance {
    pub samples: usize,
    pub method: FitMethod,
    /// The verdict that licensed this model, carrying the levels, their spread and the alpha
    /// uncertainty. Always a `produces_model()` variant.
    pub conditioning: ModelConditioning,
    /// Pixels whose fitted `s` or `c` had to be clamped into the physically possible range.
    /// A large count means the samples disagree and the model should not be trusted.
    pub clamped_pixels: usize,
}

// ---------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------

/// The decomposition of one mark: `c` and `s` PER PIXEL PER CHANNEL.
///
/// Channels are never collapsed. Per channel is mandatory for `c`: the measured marks are a white
/// glyph with a dark outline and a colour mark whose deposit differs by up to 120 LSB between
/// channels, so one colour would be a confidently wrong model rather than a simplification.
/// ALPHA, by contrast, measured channel-neutral on both chapters (|alpha_R - alpha_G| <= 0.01,
/// about 4% of alpha) — the earlier prediction that a colour mark forces per-channel alpha is
/// refuted. Per-channel alpha stays supported because nothing guarantees the next source is as
/// well behaved, and the graded fit is the only path that deliberately ties the channels together
/// (see [`FitMethod::DepositExact`]).
#[derive(Debug, Clone)]
pub(super) struct WatermarkModel {
    width: u32,
    height: u32,
    /// Interleaved RGB constant term `c = alpha*W`, `width*height*3` entries.
    c: Vec<f32>,
    /// Interleaved RGB slope `s = 1 - alpha`, `width*height*3` entries, each in `[S_FLOOR, 1]`.
    s: Vec<f32>,
    operator: Arc<dyn CompositingOperator>,
    provenance: ModelProvenance,
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
    fn params_at(&self, pixel: usize, channel: usize) -> (f32, f32) {
        let index = pixel * 3 + channel;
        (self.c[index], self.s[index])
    }

    /// Opacity of a pixel, taken as the strongest channel: a pixel the mark touches on any
    /// channel is a mark pixel.
    #[inline]
    fn mark_alpha(&self, pixel: usize) -> f32 {
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
    fn params_bilinear(&self, x: f32, y: f32, channel: usize) -> (f32, f32) {
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

/// Shape-independent identity of one mark.
///
/// Two marks can share their artwork pixel for pixel and still be different assets: chapter two
/// carries a colour mark and a greyscale one that band-pass correlate at >= 0.97 yet have
/// different `c`/`s`, and applying either model to the other's occurrences leaves visible
/// residue. A catalog keyed on template SHAPE therefore merges them, which is why identity keys
/// on what the deposit measures instead:
///
/// - `deposit_chroma` — the colour mark reaches 120 LSB between channels, the pale one 8;
/// - `mean_deposit` — whose ratio between two marks is the bimodal opacity gain (pale scores
///   0.365-0.515 against the colour model, colour 0.999-1.000 against its own), which separates
///   the two on its own.
///
/// Deposits are only comparable when they were measured against the same background level, hence
/// `reference_level`: a model's signature is normalized to white, a sample's is measured at that
/// sample's own flat level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct MarkSignature {
    /// Background luma the deposit was measured against.
    pub reference_level: f32,
    /// Largest per-pixel difference between the channel deposits, LSB.
    pub deposit_chroma: f32,
    /// Mean deposit magnitude over the mark pixels, LSB.
    pub mean_deposit: f32,
    /// Peak opacity. From a model this is `max(1 - s)`; from a sample it is the deposit's own
    /// hard lower bound ([`alpha_lower_bound`]), which the truth can only exceed.
    pub peak_alpha: f32,
}

impl MarkSignature {
    /// Measure a signature straight from a flat-background sample, before any model exists.
    ///
    /// Returns `None` for a sample whose background is a per-pixel estimate: an estimated
    /// background cannot measure a deposit, it is derived from one.
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

/// Sub-pixel offset of an observed occurrence relative to its integer rect origin.
///
/// Convention: the observed mark equals the model evaluated at `(x - dx, y - dy)`. Removal
/// therefore samples the model shifted by this amount; detection reports the value it measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SubpixelShift {
    pub dx: f32,
    pub dy: f32,
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

// ---------------------------------------------------------------------------------------
// Model estimation
// ---------------------------------------------------------------------------------------

/// Fit `c` and `s` per pixel per channel from calibration samples.
///
/// Three paths, chosen by what the samples actually license:
/// - two or more FLAT samples whose levels are far enough apart -> least squares on exact `B`
///   (the closed form when there are exactly two). Both `c` and `s` are data.
/// - otherwise, samples spanning enough levels with per-pixel background ESTIMATES -> Theil-Sen
///   median-of-slopes, which tolerates a minority of bad samples but is only worth
///   `ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT` on the alpha scale.
/// - otherwise, at least one FLAT sample -> the graded deposit-exact fit: the deposit is exact,
///   the alpha scale comes from `assumption`, and the verdict states what that costs. This is the
///   measured chapter-two case (every sample on pure white) and it removes at 1.92 LSB rms there.
///
/// `assumption` is only consulted on the third path.
///
/// # Errors
/// [`ModelFitError::Invalid`] when the samples disagree on geometry or none were supplied;
/// [`ModelFitError::Refused`] carrying the [`ModelConditioning`] verdict when not even the
/// deposit was measured — the only case that produces no model.
pub(super) fn estimate_model(
    samples: &[CalibrationSample],
    operator: Arc<dyn CompositingOperator>,
    assumption: AlphaAssumption,
) -> Result<WatermarkModel, ModelFitError> {
    let first = samples.first().ok_or(WatermarkError::NoSamples)?;
    let (width, height) = (first.rect.width, first.rect.height);
    for sample in samples {
        if sample.rect.width != width || sample.rect.height != height {
            return Err(ModelFitError::Invalid(WatermarkError::GeometryMismatch {
                expected_width: width,
                expected_height: height,
                width: sample.rect.width,
                height: sample.rect.height,
            }));
        }
    }
    let pixels = (width as usize) * (height as usize);

    let plan = plan_fit(samples, pixels).map_err(ModelFitError::Refused)?;
    let fit_input: Vec<&CalibrationSample> =
        plan.inputs.iter().map(|&index| &samples[index]).collect();

    let fit = match plan.method {
        FitMethod::ClosedFormFlat | FitMethod::TheilSen => {
            regress_planes(&fit_input, pixels, plan.method)
        }
        FitMethod::DepositExact => deposit_exact_planes(&fit_input, pixels, assumption),
    };

    // The alpha uncertainty can only be quoted once the planes exist: the fitted peak opacity is
    // what a slope error is relative TO.
    let peak_alpha = fit
        .s
        .iter()
        .map(|&value| 1.0 - value)
        .fold(0.0f32, f32::max);
    let conditioning = match plan.method {
        FitMethod::ClosedFormFlat => ModelConditioning::Separable {
            levels: plan.levels,
            spread: plan.spread,
            min_pixel_spread: plan.min_pixel_spread,
            alpha: AlphaUncertainty::from_flat_fit(plan.spread, peak_alpha),
        },
        FitMethod::TheilSen => ModelConditioning::Separable {
            levels: plan.levels,
            spread: plan.spread,
            min_pixel_spread: plan.min_pixel_spread,
            alpha: AlphaUncertainty::from_percent(
                AlphaSource::EstimatedBackgrounds,
                ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT,
            ),
        },
        FitMethod::DepositExact => ModelConditioning::DepositExact {
            levels: plan.levels,
            spread: plan.spread,
            samples: fit_input.len(),
            alpha: AlphaUncertainty::from_percent(
                AlphaSource::Assumed,
                fit.alpha_uncertainty_percent,
            ),
        },
    };
    let provenance = ModelProvenance {
        samples: samples.len(),
        method: plan.method,
        conditioning,
        clamped_pixels: fit.clamped_pixels,
    };
    WatermarkModel::from_parts(width, height, fit.c, fit.s, operator, provenance)
        .map_err(ModelFitError::Invalid)
}

/// Which fit the samples license, on which of them, and the levels the verdict will report.
struct FitPlan {
    method: FitMethod,
    /// Indices into the caller's sample slice that the fit runs on.
    inputs: Vec<usize>,
    /// Distinct background levels of those inputs, ascending.
    levels: Vec<f32>,
    /// Widest gap between them, LSB.
    spread: f32,
    /// Narrowest per-pixel background spread over the footprint, LSB.
    min_pixel_spread: f32,
}

/// Fitted parameter planes plus what the fit costs.
struct FittedPlanes {
    c: Vec<f32>,
    s: Vec<f32>,
    clamped_pixels: usize,
    /// Relative uncertainty of the alpha scale, percent — only meaningful for the graded fit,
    /// where the scale is an assumption; the regression paths quote their own.
    alpha_uncertainty_percent: f32,
}

/// Decide what the samples license, and say what is missing when they license nothing.
///
/// The order encodes the evidence hierarchy: exact backgrounds far apart beat estimated ones,
/// which beat a single exact level, which still beats nothing. Only the last step refuses, and
/// only because neither the deposit nor the slope was ever measured.
///
/// # Errors
/// The refusing [`ModelConditioning`] variant, for the caller to wrap.
fn plan_fit(samples: &[CalibrationSample], pixels: usize) -> Result<FitPlan, ModelConditioning> {
    let flat: Vec<usize> = (0..samples.len()).filter(|&i| samples[i].is_flat()).collect();
    let mut flat_levels: Vec<f32> = flat
        .iter()
        .map(|&index| samples[index].mean_background_luma())
        .collect();
    flat_levels.sort_by(f32::total_cmp);
    let flat_spread = luma_spread(&flat_levels);

    // 1. Exact backgrounds, far enough apart: the slope is measured, not assumed.
    if flat.len() >= 2 && flat_spread >= MIN_BACKGROUND_SPREAD {
        return Ok(FitPlan {
            method: FitMethod::ClosedFormFlat,
            inputs: flat,
            levels: distinct_levels(&flat_levels),
            spread: flat_spread,
            // Flat samples give every pixel the same background, so the per-pixel spread is the
            // sample spread by construction.
            min_pixel_spread: flat_spread,
        });
    }

    let mut all_levels: Vec<f32> = samples.iter().map(|s| s.mean_background_luma()).collect();
    all_levels.sort_by(f32::total_cmp);
    let all_spread = luma_spread(&all_levels);

    // 2. Per-pixel background ESTIMATES spanning enough levels. A pixel that never saw two of
    //    them would be a guess dressed as a fit, so that case falls through (or refuses).
    if samples.len() >= 2 && all_spread >= MIN_BACKGROUND_SPREAD {
        let (underdetermined, worst) = narrowest_pixel_spread(samples, pixels);
        if underdetermined == 0 {
            return Ok(FitPlan {
                method: FitMethod::TheilSen,
                inputs: (0..samples.len()).collect(),
                levels: distinct_levels(&all_levels),
                spread: all_spread,
                min_pixel_spread: if worst.is_finite() { worst } else { all_spread },
            });
        }
        if flat.is_empty() {
            return Err(ModelConditioning::Underdetermined {
                levels: distinct_levels(&all_levels),
                underdetermined_pixels: underdetermined,
                total_pixels: pixels,
                worst_pixel_spread: worst,
                required: MIN_BACKGROUND_SPREAD,
            });
        }
    }

    // 3. At least one exactly known background: the deposit is exact everywhere under the mark,
    //    so removal at that level is exact and only the alpha scale is an assumption.
    if !flat.is_empty() {
        return Ok(FitPlan {
            method: FitMethod::DepositExact,
            inputs: flat,
            levels: distinct_levels(&flat_levels),
            spread: flat_spread,
            min_pixel_spread: flat_spread,
        });
    }

    // 4. Nothing was measured: every background is an estimate and they do not even span levels.
    if samples.len() < 2 {
        return Err(ModelConditioning::NotEnoughSamples {
            have: samples.len(),
            need: 2,
        });
    }
    Err(ModelConditioning::DepositUnavailable {
        samples: samples.len(),
        spread: all_spread,
    })
}

/// Count the pixels that never saw two sufficiently different backgrounds, and report the
/// narrowest per-pixel spread. Only violable with per-pixel estimated backgrounds.
fn narrowest_pixel_spread(samples: &[CalibrationSample], pixels: usize) -> (usize, f32) {
    (0..pixels)
        .into_par_iter()
        .map(|pixel| {
            let mut worst = f32::INFINITY;
            for channel in 0..3 {
                let mut lo = f32::INFINITY;
                let mut hi = f32::NEG_INFINITY;
                for sample in samples {
                    let value = sample.background_at(pixel, channel);
                    lo = lo.min(value);
                    hi = hi.max(value);
                }
                worst = worst.min(hi - lo);
            }
            if worst < MIN_BACKGROUND_SPREAD {
                (1usize, worst)
            } else {
                (0usize, worst)
            }
        })
        .reduce(
            || (0usize, f32::INFINITY),
            |a, b| (a.0 + b.0, a.1.min(b.1)),
        )
}

/// Per-pixel per-channel regression of `I = c + s*B` over the fit inputs.
///
/// `method` selects least squares (exact backgrounds) or Theil-Sen (estimated ones); both are
/// clamped into the physically possible range and the clamp count is kept, because a value
/// outside it cannot come from alpha compositing and a large count means the samples disagree.
fn regress_planes(
    fit_input: &[&CalibrationSample],
    pixels: usize,
    method: FitMethod,
) -> FittedPlanes {
    let mut c_plane = vec![0f32; pixels * 3];
    let mut s_plane = vec![1f32; pixels * 3];
    // One counter per pixel, summed afterwards: a shared atomic would serialize the hot loop.
    let mut clamped = vec![0u8; pixels];

    c_plane
        .par_chunks_mut(3)
        .zip(s_plane.par_chunks_mut(3))
        .zip(clamped.par_iter_mut())
        .enumerate()
        .for_each(|(pixel, ((c_px, s_px), clamped_px))| {
            let mut points: Vec<(f32, f32)> = Vec::with_capacity(fit_input.len());
            for channel in 0..3 {
                points.clear();
                points.extend(fit_input.iter().map(|sample| {
                    (
                        sample.background_at(pixel, channel),
                        sample.observed_at(pixel, channel),
                    )
                }));
                let fitted = match method {
                    FitMethod::ClosedFormFlat => least_squares_line(&points),
                    FitMethod::TheilSen => theil_sen_line(&points),
                    // The graded fit has its own path; reaching it here would be a caller bug,
                    // and a neutral line is the only answer that cannot invent a mark.
                    FitMethod::DepositExact => None,
                };
                let (c_raw, s_raw) = fitted.unwrap_or((0.0, 1.0));
                let c_fit = if c_raw.is_finite() { c_raw } else { 0.0 };
                let s_fit = if s_raw.is_finite() { s_raw } else { 1.0 };
                let c_clamped = c_fit.clamp(0.0, 255.0);
                let s_clamped = s_fit.clamp(S_FLOOR, S_CEIL);
                // Values outside the physical range cannot come from alpha compositing; they are
                // noise. Clamping keeps the model usable, and the count keeps it honest.
                if (c_clamped - c_fit).abs() > f32::EPSILON
                    || (s_clamped - s_fit).abs() > f32::EPSILON
                {
                    *clamped_px = 1;
                }
                c_px[channel] = c_clamped;
                s_px[channel] = s_clamped;
            }
        });

    FittedPlanes {
        c: c_plane,
        s: s_plane,
        clamped_pixels: clamped.iter().filter(|&&flag| flag == 1).count(),
        alpha_uncertainty_percent: 0.0,
    }
}

/// The graded fit: exact deposit, assumed alpha scale.
///
/// With every calibration background known exactly, `D = B - I` is measured exactly, and for any
/// assumed `s` the constant follows exactly as `c = mean(I - s*B)`. The recovery is therefore
/// EXACT at the observed level whatever alpha is assumed, and wrong away from it by
/// `delta_alpha * (B - B0) / (1 - alpha)` — which is what the verdict quotes.
///
/// The alpha map is the deposit's own shape scaled so its peak reaches the assumed opacity, and
/// never below the hard lower bound the physical range of the mark's own colour forces (see
/// [`alpha_lower_bound`]). Alpha is channel-neutral here by construction: it measured channel-neutral on both
/// chapters, and a single scalar assumption per pixel is the whole point of this path.
///
/// The limit of the quoted uncertainty, stated plainly: it covers an error in the alpha SCALE,
/// which is what chapter two measured. The alpha MAP's SHAPE is a second assumption — a pixel
/// whose mark colour happens to match the calibration background deposits nothing there and is
/// genuinely unconstrained, so no verdict can bound its error. What keeps that honest is the gain
/// test: on a background far from the calibration level such a model no longer regresses to unity
/// and the occurrence is REFUSED rather than mis-removed.
fn deposit_exact_planes(
    fit_input: &[&CalibrationSample],
    pixels: usize,
    assumption: AlphaAssumption,
) -> FittedPlanes {
    // Pass 1: per-pixel deposit strength and the alpha lower bound the deposit implies.
    let mut strength = vec![0f32; pixels];
    let mut alpha_floor = vec![0f32; pixels];
    strength
        .par_iter_mut()
        .zip(alpha_floor.par_iter_mut())
        .enumerate()
        .for_each(|(pixel, (strength_px, floor_px))| {
            let mut magnitude = 0.0f32;
            let mut bound = 0.0f32;
            for sample in fit_input {
                let SampleBackground::Flat { level, .. } = sample.background() else {
                    continue;
                };
                for (channel, &background) in level.iter().enumerate() {
                    let observed = sample.observed_at(pixel, channel);
                    magnitude = magnitude.max((background - observed).abs());
                    bound = bound.max(alpha_lower_bound(background, observed));
                }
            }
            *strength_px = magnitude;
            *floor_px = bound;
        });
    let peak_strength = strength.iter().copied().fold(0.0f32, f32::max);
    let peak_floor = alpha_floor.iter().copied().fold(0.0f32, f32::max);

    let (peak_alpha, alpha_uncertainty_percent) = match assumption {
        AlphaAssumption::FromDeposit => (
            (peak_floor * ASSUMED_ALPHA_OVER_DEPOSIT_BOUND)
                .max(peak_floor)
                .clamp(0.0, 1.0 - S_FLOOR),
            ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
        ),
        AlphaAssumption::Stated {
            peak_alpha,
            uncertainty_percent,
        } => {
            let stated = if peak_alpha.is_finite() {
                peak_alpha.clamp(0.0, 1.0 - S_FLOOR)
            } else {
                peak_floor
            };
            // A stated alpha below the deposit's hard bound is provably wrong; the data wins, and
            // the size of the correction is a lower bound on how wrong the caller was.
            let used = stated.max(peak_floor);
            let percent = if used > f32::EPSILON {
                uncertainty_percent.max(100.0 * (used - stated) / used)
            } else {
                uncertainty_percent
            };
            (used, percent)
        }
    };
    let scale = if peak_strength > f32::EPSILON {
        peak_alpha / peak_strength
    } else {
        0.0
    };

    // Pass 2: alpha map, then the exact constant that goes with it.
    let mut c_plane = vec![0f32; pixels * 3];
    let mut s_plane = vec![1f32; pixels * 3];
    let mut clamped = vec![0u8; pixels];
    c_plane
        .par_chunks_mut(3)
        .zip(s_plane.par_chunks_mut(3))
        .zip(clamped.par_iter_mut())
        .enumerate()
        .for_each(|(pixel, ((c_px, s_px), clamped_px))| {
            let alpha = (strength[pixel] * scale)
                .max(alpha_floor[pixel])
                .clamp(0.0, 1.0 - S_FLOOR);
            let s = 1.0 - alpha;
            for channel in 0..3 {
                let mut sum = 0.0f64;
                let mut count = 0usize;
                for sample in fit_input {
                    let SampleBackground::Flat { level, .. } = sample.background() else {
                        continue;
                    };
                    sum += f64::from(sample.observed_at(pixel, channel) - s * level[channel]);
                    count += 1;
                }
                let c_fit = if count == 0 {
                    0.0
                } else {
                    (sum / count as f64) as f32
                };
                let c_clamped = c_fit.clamp(0.0, 255.0);
                if (c_clamped - c_fit).abs() > f32::EPSILON {
                    *clamped_px = 1;
                }
                c_px[channel] = c_clamped;
                s_px[channel] = s.clamp(S_FLOOR, S_CEIL);
            }
        });

    FittedPlanes {
        c: c_plane,
        s: s_plane,
        clamped_pixels: clamped.iter().filter(|&&flag| flag == 1).count(),
        alpha_uncertainty_percent,
    }
}

/// Lowest opacity that can deposit `observed` over an exactly known background `background`.
///
/// A HARD bound, not an estimate: it is what `0 <= W <= 255` forces. A darkening deposit needs
/// `alpha >= (B - I)/B`, or `c = alpha*W` would go negative; a brightening one needs
/// `alpha >= (I - B)/(255 - B)`, or the mark's colour would have to exceed white. The truth sits
/// above the bound by a factor no single background level can see — that is the whole content of
/// the graded verdict's uncertainty.
#[inline]
fn alpha_lower_bound(background: f32, observed: f32) -> f32 {
    let deposit = background - observed;
    let mut bound = 0.0f32;
    if background > f32::EPSILON {
        bound = bound.max(deposit / background);
    }
    let headroom = 255.0 - background;
    if headroom > f32::EPSILON {
        bound = bound.max(-deposit / headroom);
    }
    bound.clamp(0.0, 1.0 - S_FLOOR)
}

/// Widest gap between the extremes of a level list.
fn luma_spread(levels: &[f32]) -> f32 {
    if levels.len() < 2 {
        return 0.0;
    }
    let lo = levels.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = levels.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    (hi - lo).max(0.0)
}

/// Collapse levels closer than `SAME_LEVEL_EPS` into one representative, ascending.
fn distinct_levels(sorted_levels: &[f32]) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    for &level in sorted_levels {
        match out.last() {
            Some(&last) if (level - last).abs() <= SAME_LEVEL_EPS => {}
            _ => out.push(level),
        }
    }
    out
}

/// Ordinary least squares fit of `I = c + s*B`. Returns `None` when `B` has no variance.
fn least_squares_line(points: &[(f32, f32)]) -> Option<(f32, f32)> {
    if points.len() < 2 {
        return None;
    }
    let n = count_f32(points.len());
    let mean_b = points.iter().map(|p| p.0).sum::<f32>() / n;
    let mean_i = points.iter().map(|p| p.1).sum::<f32>() / n;
    let mut sxx = 0.0f32;
    let mut sxy = 0.0f32;
    for &(b, i) in points {
        let db = b - mean_b;
        sxx += db * db;
        sxy += db * (i - mean_i);
    }
    if sxx <= f32::EPSILON {
        return None;
    }
    let s = sxy / sxx;
    Some((mean_i - s * mean_b, s))
}

/// Theil-Sen fit of `I = c + s*B`: median of pairwise slopes, then median intercept.
///
/// Pairs whose backgrounds differ by less than `MIN_PAIR_SPREAD` are skipped — their slope is
/// dominated by quantization noise. Returns `None` when no pair qualifies.
fn theil_sen_line(points: &[(f32, f32)]) -> Option<(f32, f32)> {
    if points.len() < 2 {
        return None;
    }
    let mut slopes: Vec<f32> = Vec::with_capacity(points.len() * (points.len() - 1) / 2);
    for (index, &(b0, i0)) in points.iter().enumerate() {
        for &(b1, i1) in &points[index + 1..] {
            let db = b1 - b0;
            if db.abs() < MIN_PAIR_SPREAD {
                continue;
            }
            slopes.push((i1 - i0) / db);
        }
    }
    let s = median(&mut slopes)?;
    let mut intercepts: Vec<f32> = points.iter().map(|&(b, i)| i - s * b).collect();
    let c = median(&mut intercepts)?;
    Some((c, s))
}

/// Median of a scratch slice (sorts it in place). `None` for an empty slice.
fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[middle])
    } else {
        Some(0.5 * (values[middle - 1] + values[middle]))
    }
}

// ---------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------

/// What justified accepting an occurrence.
///
/// The distinction is a safety contract, not bookkeeping: correlation is scale invariant, so it
/// happily matches content that merely has the mark's SHAPE. Only the gain test checks that the
/// content has the mark's AMPLITUDE, and only a gain-verified occurrence may be removed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum AcceptanceEvidence {
    /// Correlation only — the kind had no fitted model yet. NOT sufficient to remove.
    Correlation,
    /// Correlation plus the per-pixel-background gain regression.
    Gain { gain: f32, snr: f32 },
}

/// One accepted occurrence of a mark on a page.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Occurrence {
    pub rect: PixelRect,
    pub ncc: f32,
    pub evidence: AcceptanceEvidence,
    pub shift: SubpixelShift,
}

impl Occurrence {
    /// True when removing this occurrence is licensed. A `Correlation`-only accept must be
    /// shown to the user or fed back into calibration, never subtracted.
    #[must_use]
    pub fn is_removal_safe(&self) -> bool {
        matches!(self.evidence, AcceptanceEvidence::Gain { .. })
    }

    /// Comparable confidence in `0..=1`, used to arbitrate between kinds claiming the same spot.
    /// Only meaningful WITHIN one evidence rank; gain-verified always outranks correlation-only.
    #[must_use]
    pub fn score(&self) -> f32 {
        match self.evidence {
            AcceptanceEvidence::Correlation => self.ncc.clamp(0.0, 1.0),
            AcceptanceEvidence::Gain { gain, snr } => {
                let fitness = 1.0 - ((gain - 1.0).abs() / GAIN_FITNESS_SCALE).min(1.0);
                // Saturating confidence: an enormous t-statistic is not proportionally more
                // trustworthy than a merely large one.
                let confidence = snr / (snr + MIN_DETECTION_T);
                fitness * confidence
            }
        }
    }

    fn evidence_rank(&self) -> u8 {
        match self.evidence {
            AcceptanceEvidence::Correlation => 0,
            AcceptanceEvidence::Gain { .. } => 1,
        }
    }
}

/// Tunables of the detection pass. Defaults are the named constants above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DetectionParams {
    pub anchor_tolerance: u32,
    pub coarse_ncc_min: f32,
    pub ncc_only_min: f32,
    pub gain_min: f32,
    pub gain_max: f32,
    pub min_snr: f32,
    pub max_candidates_per_page: usize,
    pub background_blur_radius: u32,
    pub overlap_iou_limit: f32,
}

impl Default for DetectionParams {
    fn default() -> Self {
        Self {
            anchor_tolerance: ANCHOR_TOLERANCE_PX,
            coarse_ncc_min: COARSE_NCC_MIN,
            ncc_only_min: NCC_ONLY_MIN,
            gain_min: GAIN_MIN,
            gain_max: GAIN_MAX,
            min_snr: MIN_DETECTION_T,
            max_candidates_per_page: MAX_CANDIDATES_PER_PAGE,
            background_blur_radius: BACKGROUND_BLUR_RADIUS_PX,
            overlap_iou_limit: OVERLAP_IOU_LIMIT,
        }
    }
}

impl DetectionParams {
    /// Clamp hand-supplied values into ranges the detector can honour. Applied internally by
    /// every entry point, so a persisted or hand-edited configuration cannot widen the gain
    /// window into "accept anything" nor the anchor band into "anywhere on the page" — the two
    /// guards chapter two measured to remove every false accept at zero cost.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let gain_min = self.gain_min.clamp(FALSE_ACCEPT_GAIN_FLOOR, 1.0);
        let gain_max = self.gain_max.clamp(1.0, 1.5).max(gain_min);
        Self {
            anchor_tolerance: self.anchor_tolerance.min(MAX_ANCHOR_TOLERANCE_PX),
            coarse_ncc_min: self.coarse_ncc_min.clamp(0.0, 1.0),
            ncc_only_min: self.ncc_only_min.clamp(0.5, 1.0),
            gain_min,
            gain_max,
            min_snr: self.min_snr.max(0.0),
            max_candidates_per_page: self.max_candidates_per_page.clamp(1, 4096),
            background_blur_radius: self.background_blur_radius.clamp(1, 64),
            overlap_iou_limit: self.overlap_iou_limit.clamp(0.0, 1.0),
        }
    }
}

/// One distinct watermark of a chapter: its identity, correlation reference, calibration
/// samples, fitted model and conditioning verdict.
///
/// A chapter can carry several — measured: a colour mark and a greyscale one that share their
/// artwork pixel for pixel yet have different `c`/`s` — so the unit of work is a CATALOG of kinds
/// and every stage runs per kind. Kind identity is [`MarkSignature`], never the template's shape.
#[derive(Debug, Clone)]
pub(super) struct WatermarkKind {
    id: String,
    template: MarkTemplate,
    samples: Vec<CalibrationSample>,
    model: Option<WatermarkModel>,
    conditioning: ModelConditioning,
    operator: Arc<dyn CompositingOperator>,
    assumption: AlphaAssumption,
}

impl WatermarkKind {
    /// Create a kind with no samples yet. `id` is persisted identity and stays literal — the
    /// user-facing label belongs to the UI layer.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        template: MarkTemplate,
        operator: Arc<dyn CompositingOperator>,
    ) -> Self {
        Self {
            id: id.into(),
            template,
            samples: Vec::new(),
            model: None,
            conditioning: ModelConditioning::NotEnoughSamples { have: 0, need: 2 },
            operator,
            assumption: AlphaAssumption::default(),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn template(&self) -> &MarkTemplate {
        &self.template
    }

    /// Mutable access to the template, for installing a discovered anchor set.
    pub fn template_mut(&mut self) -> &mut MarkTemplate {
        &mut self.template
    }

    #[must_use]
    pub fn samples(&self) -> &[CalibrationSample] {
        &self.samples
    }

    #[must_use]
    pub fn model(&self) -> Option<&WatermarkModel> {
        self.model.as_ref()
    }

    #[must_use]
    pub fn conditioning(&self) -> &ModelConditioning {
        &self.conditioning
    }

    /// State what is known about this mark's peak opacity from outside its own samples — a
    /// persisted model of the same source, a sibling kind, or the user. Only consulted by the
    /// graded fit, which is where the alpha scale is otherwise an assumption.
    pub fn set_alpha_assumption(&mut self, assumption: AlphaAssumption) {
        self.assumption = assumption;
    }

    /// Shape-independent identity of this mark: from the fitted model when there is one, else
    /// from the first flat calibration sample. `None` when neither exists yet — an unfitted kind
    /// with no exactly-backed sample has no measured identity, only a shape.
    #[must_use]
    pub fn signature(&self) -> Option<MarkSignature> {
        if let Some(model) = self.model.as_ref() {
            return Some(model.signature());
        }
        self.samples.iter().find_map(MarkSignature::from_flat_sample)
    }

    /// True when `other` carries the SAME mark as this kind. Two kinds with identical templates
    /// are still distinct when their deposits differ — that is the whole point.
    #[must_use]
    pub fn is_same_mark_as(&self, other: &Self) -> bool {
        match (self.signature(), other.signature()) {
            (Some(mine), Some(theirs)) => mine.is_same_mark_as(&theirs),
            _ => false,
        }
    }

    /// Add a calibration sample. Its footprint must match the template exactly.
    ///
    /// # Errors
    /// [`WatermarkError::GeometryMismatch`] when it does not.
    pub fn add_sample(&mut self, sample: CalibrationSample) -> Result<(), WatermarkError> {
        if sample.rect.width != self.template.width || sample.rect.height != self.template.height {
            return Err(WatermarkError::GeometryMismatch {
                expected_width: self.template.width,
                expected_height: self.template.height,
                width: sample.rect.width,
                height: sample.rect.height,
            });
        }
        self.samples.push(sample);
        Ok(())
    }

    /// Refit the model from the current samples and store the resulting verdict.
    ///
    /// The verdict is the model's own: a graded [`ModelConditioning::DepositExact`] fit succeeds
    /// and carries its alpha uncertainty with it. On failure the previous model is DROPPED:
    /// keeping a stale model next to a fresh refusal is exactly the confusion that leads to
    /// removing with the wrong parameters.
    ///
    /// # Errors
    /// Propagates [`estimate_model`]'s failure.
    pub fn refit(&mut self) -> Result<(), ModelFitError> {
        match estimate_model(&self.samples, Arc::clone(&self.operator), self.assumption) {
            Ok(model) => {
                self.conditioning = model.provenance.conditioning.clone();
                self.model = Some(model);
                Ok(())
            }
            Err(error) => {
                self.model = None;
                self.conditioning = match error.conditioning() {
                    Some(verdict) => verdict.clone(),
                    None => ModelConditioning::NotEnoughSamples {
                        have: self.samples.len(),
                        need: 2,
                    },
                };
                Err(error)
            }
        }
    }
}

/// Index of the catalog entry carrying the same mark as `signature`, or `None` for a new mark.
///
/// This is the primitive a catalog must use instead of comparing templates: the measured colour
/// mark and its greyscale twin are pixel-identical in shape, and merging them means removing one
/// with the other's `c`/`s`, which leaves visible residue. A kind without a measured signature
/// (no model and no flat sample yet) never matches — it has a shape but no identity.
#[must_use]
pub(super) fn find_matching_kind(
    catalog: &[WatermarkKind],
    signature: &MarkSignature,
) -> Option<usize> {
    catalog.iter().position(|kind| {
        kind.signature()
            .is_some_and(|known| known.is_same_mark_as(signature))
    })
}

/// Refit `kind` against per-pixel background estimates produced by its own current model,
/// iterating `BACKGROUND_REFINEMENT_ITERATIONS` times.
///
/// This is the REFINEMENT path, not a bootstrap one: a per-pixel background beneath an opaque
/// mark pixel cannot exist before a model does, so the kind must already carry one (typically the
/// graded deposit-exact fit). Each iteration recomputes the `s`-weighted background estimate of
/// every sample that HAS an estimated background and refits. Samples with an exactly measured
/// flat background are left alone — downgrading a measurement to an estimate would throw away the
/// only hard evidence in the set.
///
/// The loop count is a FREE PARAMETER — see `BACKGROUND_REFINEMENT_ITERATIONS`: the estimate
/// crosses the truth rather than converging on it, so the result is worth
/// `ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT` and the verdict says so. Do not raise the
/// count expecting convergence.
///
/// `pages` is indexed by [`CalibrationSample::page_index`].
///
/// # Errors
/// [`WatermarkError::NoSamples`] when the kind has no model, [`WatermarkError::NothingToRefine`]
/// when every background is already exact, page/rect validation failures from the background
/// estimator, and any refit failure.
pub(super) fn refit_with_refined_backgrounds(
    pages: &[&RgbaImage],
    kind: &mut WatermarkKind,
    params: &DetectionParams,
) -> Result<(), ModelFitError> {
    if kind.model.is_none() {
        return Err(ModelFitError::Invalid(WatermarkError::NoSamples));
    }
    let refinable: Vec<usize> = (0..kind.samples.len())
        .filter(|&index| !kind.samples[index].is_flat())
        .collect();
    if refinable.is_empty() {
        return Err(ModelFitError::Invalid(WatermarkError::NothingToRefine));
    }
    let params = params.normalized();
    for _ in 0..BACKGROUND_REFINEMENT_ITERATIONS {
        let Some(model) = kind.model.as_ref() else {
            return Err(ModelFitError::Invalid(WatermarkError::NoSamples));
        };
        let mut refined: Vec<Vec<f32>> = Vec::with_capacity(refinable.len());
        for &index in &refinable {
            let sample = &kind.samples[index];
            let page = pages.get(sample.page_index).ok_or(ModelFitError::Invalid(
                WatermarkError::RectOutOfPage {
                    rect: sample.rect,
                    width: 0,
                    height: 0,
                },
            ))?;
            refined.push(
                provisional_background(page, sample.rect, model, &params)
                    .map_err(ModelFitError::Invalid)?,
            );
        }
        for (&index, values) in refinable.iter().zip(refined) {
            kind.samples[index].background = SampleBackground::Estimated { values };
        }
        kind.refit()?;
    }
    Ok(())
}

/// Discover the anchor columns a mark is stamped at, from the pages themselves.
///
/// The anchor set is DATA — one column in chapter one, three (x = 48, 278, 523) in chapter two —
/// so it cannot be assumed from the one occurrence the user pointed at. A full-resolution
/// full-width correlation over a 690x18000 strip is not affordable, so the scan runs on a
/// `ANCHOR_DISCOVERY_DOWNSCALE`-times box-averaged copy of both page and template, then refines
/// every surviving hit at full resolution and clusters the results. Columns supported by fewer
/// than `ANCHOR_MIN_SUPPORT` occurrences are dropped.
///
/// This is a BOOTSTRAP: correlation alone cannot tell a mark from content that merely has its
/// shape, so the returned set must be installed with [`MarkTemplate::set_anchors`] and confirmed
/// by the gain-verified rescan. The result never includes a column the data did not support, so
/// an empty result means "nothing found", not "scan everywhere".
///
/// # Errors
/// Page geometry failures. A template larger than a page contributes nothing rather than failing.
pub(super) fn discover_anchors(
    pages: &[&RgbaImage],
    template: &MarkTemplate,
    params: &DetectionParams,
) -> Result<Vec<u32>, WatermarkError> {
    let params = params.normalized();
    let mut hits: Vec<u32> = Vec::new();
    for page in pages {
        let (pw, ph) = validate_page(page)?;
        hits.extend(discover_anchors_on_page(page, pw, ph, template, &params));
        if hits.len() >= MAX_DISCOVERY_HITS {
            break;
        }
    }
    Ok(anchors_from_hits(&hits))
}

/// Coarse-then-refine anchor hits of one page. Returns the full-resolution x of every occurrence
/// candidate it could confirm at `params.coarse_ncc_min`.
fn discover_anchors_on_page(
    page: &RgbaImage,
    pw: usize,
    ph: usize,
    template: &MarkTemplate,
    params: &DetectionParams,
) -> Vec<u32> {
    let (tw, th) = (template.width as usize, template.height as usize);
    if tw > pw || th > ph {
        return Vec::new();
    }
    let luma = luma_plane(page, pw, ph);
    let factor = discovery_factor(template.width, template.height);
    let (coarse_page, cpw, cph) = downscale_plane(&luma, pw, ph, factor);
    let Some((coarse_template, ctw, cth)) =
        downscale_reference(&template.centered, tw, th, factor)
    else {
        return Vec::new();
    };
    if ctw > cpw || cth > cph {
        return Vec::new();
    }
    let coarse_norm = coarse_template
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if !coarse_norm.is_finite() || coarse_norm <= f32::EPSILON {
        return Vec::new();
    }
    let coarse_ref = CorrelationRef {
        width: ctw,
        height: cth,
        centered: &coarse_template,
        norm: coarse_norm,
    };

    // Coarse full-width scan.
    let (cmax_x, cmax_y) = (cpw - ctw, cph - cth);
    let rows: Vec<Vec<(f32, usize, usize)>> = (0..=cmax_y)
        .into_par_iter()
        .map(|y| {
            let mut found = Vec::new();
            for x in 0..=cmax_x {
                let score = ncc_patch(&coarse_page, cpw, x, y, coarse_ref);
                if score >= ANCHOR_DISCOVERY_NCC_MIN {
                    found.push((score, x, y));
                }
            }
            found
        })
        .collect();
    let mut candidates: Vec<(f32, usize, usize)> = rows.into_iter().flatten().collect();
    candidates.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });

    // Non-maximum suppression on the coarse grid, so one occurrence costs one refinement.
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for (_, x, y) in candidates {
        // Cast justification: coarse coordinates are bounded by the downscaled page, which is
        // smaller than the page, whose dimensions are validated to fit u32.
        let rect = PixelRect::new(x as u32, y as u32, ctw as u32, cth as u32);
        if kept.iter().any(|&(other_x, other_y)| {
            rect.iou(PixelRect::new(
                other_x as u32,
                other_y as u32,
                ctw as u32,
                cth as u32,
            )) > params.overlap_iou_limit
        }) {
            continue;
        }
        kept.push((x, y));
        if kept.len() >= MAX_DISCOVERY_HITS {
            break;
        }
    }

    // Refine every survivor at full resolution: the coarse grid only locates it to `factor` px.
    // One rayon task per survivor — the search window is small but there can be hundreds of them.
    let (max_x, max_y) = (pw - tw, ph - th);
    kept.into_par_iter()
        .filter_map(|(cx, cy)| {
            let (x0, y0) = (cx * factor, cy * factor);
            let lo_x = x0.saturating_sub(factor);
            let hi_x = (x0 + factor).min(max_x);
            let lo_y = y0.saturating_sub(factor);
            let hi_y = (y0 + factor).min(max_y);
            if lo_x > hi_x || lo_y > hi_y {
                return None;
            }
            let mut best = (f32::NEG_INFINITY, lo_x);
            for y in lo_y..=hi_y {
                for x in lo_x..=hi_x {
                    let score = ncc_at(&luma, pw, x, y, template);
                    if score > best.0 {
                        best = (score, x);
                    }
                }
            }
            // Cast justification: `best.1 <= max_x < pw`, validated to fit u32.
            (best.0 >= params.coarse_ncc_min).then_some(best.1 as u32)
        })
        .collect()
}

/// Downscale factor the coarse scan may use for a template of this size: the blur must not eat
/// the mark, so a template that would shrink below `MIN_DISCOVERY_TEMPLATE_SIDE` is scanned at
/// full resolution instead.
fn discovery_factor(width: u32, height: u32) -> usize {
    let shrunk_w = width / ANCHOR_DISCOVERY_DOWNSCALE;
    let shrunk_h = height / ANCHOR_DISCOVERY_DOWNSCALE;
    if shrunk_w >= MIN_DISCOVERY_TEMPLATE_SIDE && shrunk_h >= MIN_DISCOVERY_TEMPLATE_SIDE {
        ANCHOR_DISCOVERY_DOWNSCALE as usize
    } else {
        1
    }
}

/// Box-average `src` by `factor` in both axes, dropping the trailing partial blocks.
fn downscale_plane(
    src: &[f32],
    width: usize,
    height: usize,
    factor: usize,
) -> (Vec<f32>, usize, usize) {
    if factor <= 1 {
        return (src.to_vec(), width, height);
    }
    let (out_w, out_h) = (width / factor, height / factor);
    let mut out = vec![0f32; out_w * out_h];
    let block = count_f32(factor * factor);
    for y in 0..out_h {
        for x in 0..out_w {
            let mut sum = 0.0f32;
            for row in 0..factor {
                let base = (y * factor + row) * width + x * factor;
                sum += src[base..base + factor].iter().sum::<f32>();
            }
            out[y * out_w + x] = sum / block;
        }
    }
    (out, out_w, out_h)
}

/// Box-average a zero-mean reference and re-center it. `None` when the result would be empty.
fn downscale_reference(
    centered: &[f32],
    width: usize,
    height: usize,
    factor: usize,
) -> Option<(Vec<f32>, usize, usize)> {
    let (mut small, out_w, out_h) = downscale_plane(centered, width, height, factor);
    if small.is_empty() {
        return None;
    }
    // Dropping partial blocks can shift the mean off zero; the correlation needs it exact.
    let mean = small.iter().sum::<f32>() / count_f32(small.len());
    for value in &mut small {
        *value -= mean;
    }
    Some((small, out_w, out_h))
}

/// Cluster raw per-occurrence x positions into anchor columns, keeping only the ones at least
/// `ANCHOR_MIN_SUPPORT` occurrences agree on.
fn anchors_from_hits(hits: &[u32]) -> Vec<u32> {
    let mut sorted: Vec<u32> = hits.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<u32> = Vec::new();
    let mut cluster: Vec<u32> = Vec::new();
    let flush = |cluster: &mut Vec<u32>, out: &mut Vec<u32>| {
        if cluster.len() >= ANCHOR_MIN_SUPPORT {
            out.push(mean_column(cluster));
        }
        cluster.clear();
    };
    for &column in &sorted {
        match cluster.last() {
            Some(&last) if column - last <= ANCHOR_CLUSTER_RADIUS_PX => cluster.push(column),
            Some(_) => {
                flush(&mut cluster, &mut out);
                cluster.push(column);
            }
            None => cluster.push(column),
        }
    }
    flush(&mut cluster, &mut out);
    out.dedup();
    out
}

/// Merge the anchor bands into an ascending list of distinct columns to scan, clipped to
/// `0..=max_x`. An anchor whose whole band falls outside the page contributes nothing.
fn anchor_columns(anchors: &[u32], tolerance: u32, max_x: usize) -> Vec<usize> {
    let mut columns: Vec<usize> = Vec::new();
    for &anchor in anchors {
        let lo = u64::from(anchor).saturating_sub(u64::from(tolerance));
        let hi = (u64::from(anchor) + u64::from(tolerance)).min(max_x as u64);
        if lo > hi {
            continue;
        }
        // Cast justification: the range is clipped to `max_x`, which is a page dimension.
        columns.extend((lo..=hi).map(|value| value as usize));
    }
    columns.sort_unstable();
    columns.dedup();
    columns
}

/// Find every occurrence of `kind` on `page`.
///
/// Two stages. First a normalized cross-correlation scan restricted to the template's ANCHOR
/// COLUMN BANDS: a source stamps every occurrence at one of a few fixed columns (measured: one in
/// chapter one, three in chapter two), so content elsewhere is rejected outright instead of being
/// argued with. Then, for each surviving candidate, the per-pixel-background gain test: estimate
/// the background by blurring the provisional removal, regress the observed mark signal against
/// the model's, and accept only on `gain in [gain_min, gain_max]`, an absolute gain of at least
/// `FALSE_ACCEPT_GAIN_FLOOR`, a position within `anchor_tolerance` of an anchor, AND a sufficient
/// detection statistic.
///
/// When the kind has no model yet the gain test cannot run; candidates then need a much higher
/// correlation and are returned with [`AcceptanceEvidence::Correlation`], which
/// [`remove_occurrences_on_page`] refuses to act on.
///
/// # Errors
/// Page/template geometry failures. A template larger than the page yields an empty result
/// rather than an error.
pub(super) fn find_occurrences(
    page: &RgbaImage,
    kind: &WatermarkKind,
    params: &DetectionParams,
) -> Result<Vec<Occurrence>, WatermarkError> {
    let (pw, ph) = validate_page(page)?;
    let luma = luma_plane(page, pw, ph);
    find_occurrences_with_luma(page, &luma, pw, ph, kind, &params.normalized())
}

/// Detection core sharing one prepared luma plane across the kinds of a catalog.
fn find_occurrences_with_luma(
    page: &RgbaImage,
    luma: &[f32],
    pw: usize,
    ph: usize,
    kind: &WatermarkKind,
    params: &DetectionParams,
) -> Result<Vec<Occurrence>, WatermarkError> {
    let template = &kind.template;
    let (tw, th) = (template.width as usize, template.height as usize);
    if tw > pw || th > ph {
        return Ok(Vec::new());
    }

    // Anchor bands: only x within `anchor_tolerance` of one of the template's columns is
    // considered. The bands are merged into one column list so overlapping anchors are not
    // scanned twice.
    let max_x = pw - tw;
    let max_y = ph - th;
    let columns = anchor_columns(template.anchors(), params.anchor_tolerance, max_x);
    if columns.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<Vec<(f32, usize, usize)>> = (0..=max_y)
        .into_par_iter()
        .map(|y| {
            let mut hits = Vec::new();
            for &x in &columns {
                let score = ncc_at(luma, pw, x, y, template);
                if score >= params.coarse_ncc_min {
                    hits.push((score, x, y));
                }
            }
            hits
        })
        .collect();

    let mut candidates: Vec<(f32, usize, usize)> = rows.into_iter().flatten().collect();
    // Deterministic order: best correlation first, ties broken by position so two runs on the
    // same page always produce the same list.
    candidates.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });

    // Non-maximum suppression before the expensive stage: neighbouring positions of one true
    // occurrence all correlate well and must not each pay for a gain test.
    let mut kept: Vec<(f32, PixelRect)> = Vec::new();
    for (score, x, y) in candidates {
        // Cast justification: `x <= pw` and `y <= ph`, both validated to fit u32 in `validate_page`.
        let rect = PixelRect::new(x as u32, y as u32, template.width, template.height);
        if kept
            .iter()
            .any(|(_, other)| rect.iou(*other) > params.overlap_iou_limit)
        {
            continue;
        }
        kept.push((score, rect));
        if kept.len() >= params.max_candidates_per_page {
            break;
        }
    }

    // The accept rule. Both guards below are measured, not defensive: 9 of 147 candidates on the
    // second chapter were false, ALL of them off-anchor and/or under `FALSE_ACCEPT_GAIN_FLOOR`,
    // and removal at a false accept injects an inverse mark into content. Re-checking the anchor
    // here rather than relying on the scan band keeps the rule in one place and survives a
    // widened band.
    let mut accepted: Vec<Occurrence> = match kind.model.as_ref() {
        Some(model) => kept
            .par_iter()
            .filter(|(_, rect)| template.is_on_anchor(rect.x, params.anchor_tolerance))
            .filter_map(|&(score, rect)| {
                let verdict = gain_test(page, rect, model, params)?;
                if verdict.gain < FALSE_ACCEPT_GAIN_FLOOR {
                    return None;
                }
                Some(Occurrence {
                    rect,
                    ncc: score,
                    evidence: AcceptanceEvidence::Gain {
                        gain: verdict.gain,
                        snr: verdict.snr,
                    },
                    shift: verdict.shift,
                })
            })
            .collect(),
        None => kept
            .iter()
            .filter(|(score, rect)| {
                *score >= params.ncc_only_min
                    && template.is_on_anchor(rect.x, params.anchor_tolerance)
            })
            .map(|&(score, rect)| Occurrence {
                rect,
                ncc: score,
                evidence: AcceptanceEvidence::Correlation,
                shift: SubpixelShift::NONE,
            })
            .collect(),
    };
    accepted.sort_by(|a, b| {
        a.rect
            .y
            .cmp(&b.rect.y)
            .then_with(|| a.rect.x.cmp(&b.rect.x))
    });
    Ok(accepted)
}

/// One accepted occurrence attributed to a kind of the catalog.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChapterHit {
    pub page_index: usize,
    pub kind_index: usize,
    pub occurrence: Occurrence,
}

/// Run every kind of `catalog` over one page and resolve overlaps between them.
///
/// Overlap resolution: gain-verified accepts always beat correlation-only ones, and within a
/// rank the higher [`Occurrence::score`] wins; the loser is dropped, not merged. A colour mark
/// therefore cannot be "removed" with the greyscale kind's model, which would subtract the
/// wrong `c`/`s`.
///
/// # Errors
/// Page geometry failures.
pub(super) fn scan_page(
    page: &RgbaImage,
    page_index: usize,
    catalog: &[WatermarkKind],
    params: &DetectionParams,
) -> Result<Vec<ChapterHit>, WatermarkError> {
    let params = params.normalized();
    let (pw, ph) = validate_page(page)?;
    let luma = luma_plane(page, pw, ph);
    let mut hits: Vec<ChapterHit> = Vec::new();
    for (kind_index, kind) in catalog.iter().enumerate() {
        let found = find_occurrences_with_luma(page, &luma, pw, ph, kind, &params)?;
        hits.extend(found.into_iter().map(|occurrence| ChapterHit {
            page_index,
            kind_index,
            occurrence,
        }));
    }
    Ok(resolve_overlaps(hits, params.overlap_iou_limit))
}

/// Run the catalog over a whole chapter, one page per rayon task.
///
/// # Errors
/// The first page-geometry failure encountered.
pub(super) fn scan_chapter(
    pages: &[&RgbaImage],
    catalog: &[WatermarkKind],
    params: &DetectionParams,
) -> Result<Vec<ChapterHit>, WatermarkError> {
    let per_page = pages
        .par_iter()
        .enumerate()
        .map(|(page_index, page)| scan_page(page, page_index, catalog, params))
        .collect::<Result<Vec<Vec<ChapterHit>>, WatermarkError>>()?;
    Ok(per_page.into_iter().flatten().collect())
}

/// Greedy overlap resolution within one page: strongest evidence first, then best score.
fn resolve_overlaps(mut hits: Vec<ChapterHit>, iou_limit: f32) -> Vec<ChapterHit> {
    hits.sort_by(|a, b| {
        b.occurrence
            .evidence_rank()
            .cmp(&a.occurrence.evidence_rank())
            .then_with(|| b.occurrence.score().total_cmp(&a.occurrence.score()))
            .then_with(|| a.occurrence.rect.y.cmp(&b.occurrence.rect.y))
            .then_with(|| a.occurrence.rect.x.cmp(&b.occurrence.rect.x))
            .then_with(|| a.kind_index.cmp(&b.kind_index))
    });
    let mut accepted: Vec<ChapterHit> = Vec::with_capacity(hits.len());
    for hit in hits {
        if accepted
            .iter()
            .any(|other| hit.occurrence.rect.iou(other.occurrence.rect) > iou_limit)
        {
            continue;
        }
        accepted.push(hit);
    }
    accepted.sort_by(|a, b| {
        a.page_index
            .cmp(&b.page_index)
            .then_with(|| a.occurrence.rect.y.cmp(&b.occurrence.rect.y))
            .then_with(|| a.occurrence.rect.x.cmp(&b.occurrence.rect.x))
    });
    accepted
}

/// Rec.601 luma plane of a whole page.
fn luma_plane(page: &RgbaImage, pw: usize, ph: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(pw * ph);
    out.extend(
        page.as_raw()
            .chunks_exact(4)
            .map(|px| luma_of(px[0], px[1], px[2])),
    );
    out
}

/// Normalized cross-correlation of the template against the page patch at `(x, y)`.
///
/// Returns 0 for a patch with no contrast (a flat area cannot match a structured template).
/// NCC is deliberately contrast INVARIANT: it finds the mark's shape, and the gain test is what
/// checks the amplitude.
fn ncc_at(luma: &[f32], pw: usize, x: usize, y: usize, template: &MarkTemplate) -> f32 {
    ncc_patch(luma, pw, x, y, template.correlation_ref())
}

/// [`ncc_at`] against a bare zero-mean reference, so the coarse anchor-discovery scan can reuse
/// it with a downscaled template.
fn ncc_patch(plane: &[f32], stride: usize, x: usize, y: usize, reference: CorrelationRef<'_>) -> f32 {
    let (tw, th) = (reference.width, reference.height);
    let (centered, norm) = (reference.centered, reference.norm);
    let mut sum = 0.0f32;
    let mut sum_sq = 0.0f32;
    for row in 0..th {
        let base = (y + row) * stride + x;
        for &value in &plane[base..base + tw] {
            sum += value;
            sum_sq += value * value;
        }
    }
    let n = count_f32(tw * th);
    let mean = sum / n;
    let variance = sum_sq - sum * mean;
    // A flat (or numerically degenerate) patch has nothing to correlate: report no match rather
    // than dividing by ~0. NaN is checked explicitly — `<=` alone would let it through.
    if !variance.is_finite() || variance <= f32::EPSILON {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for row in 0..th {
        let base = (y + row) * stride + x;
        let trow = row * tw;
        for (&value, &reference) in plane[base..base + tw].iter().zip(&centered[trow..trow + tw]) {
            dot += (value - mean) * reference;
        }
    }
    (dot / (variance.sqrt() * norm)).clamp(-1.0, 1.0)
}

/// Outcome of the per-pixel-background gain test.
#[derive(Debug, Clone, Copy)]
struct GainVerdict {
    gain: f32,
    snr: f32,
    shift: SubpixelShift,
}

/// The acceptance test: does the content at `rect` carry this mark's AMPLITUDE, not merely its
/// shape?
///
/// Steps, following the refuted-alternatives note in the plan (Laplacian energy and a scalar
/// background estimate were both tried and fail):
/// 1. provisional removal `B_hat = (I - c)/s` over the rect plus a blur-radius margin;
/// 2. background estimate `B_bar` = box blur of `B_hat` WEIGHTED by `s`. The weighting is the
///    load-bearing part: an unweighted blur folds the injected inverse mark back into the
///    estimate, and a false candidate then scores near unity. Weighted by `s`, the estimate is
///    built from the pixels the model says the mark does not cover, where `B_hat` equals the
///    observation;
/// 3. regress the observed deviation `d = I - B_bar` against the model's predicted mark signal
///    `m = compose(c, s, B_bar) - B_bar` through the origin: `g = sum(d*m)/sum(m*m)`;
/// 4. accept on the gain window AND the matched-filter t-statistic.
///
/// Known limit of step 2: the estimate is only as good as the unmarked pixels within
/// `BACKGROUND_BLUR_RADIUS_PX`. A mark with a SOLID interior wider than the blur diameter has
/// pixels whose whole window is mark, and there the estimate falls back on the provisional
/// removal — which is where a wrong-amplitude candidate can drift toward `g = 1`. Real marks are
/// thin glyph strokes, where every mark pixel has unmarked neighbours; a source that violates
/// that needs a larger radius, not a looser window.
///
/// Returns `None` for any rejection, including a rect the model does not fit.
fn gain_test(
    page: &RgbaImage,
    rect: PixelRect,
    model: &WatermarkModel,
    params: &DetectionParams,
) -> Option<GainVerdict> {
    if rect.width != model.width || rect.height != model.height {
        return None;
    }
    let margin = params.background_blur_radius as usize;
    let patch = padded_patch(page, rect, margin)?;
    let operator = model.operator();
    let (iw, ih) = (rect.width as usize, rect.height as usize);

    // Provisional removal and its weights over the padded patch. Outside the mark footprint the
    // model is "no mark" (c = 0, s = 1), which makes the margin real background context.
    let mut provisional = vec![0f32; patch.rgb.len()];
    let mut weights = vec![0f32; patch.rgb.len()];
    for py in 0..patch.height {
        for px in 0..patch.width {
            let patch_pixel = py * patch.width + px;
            let inside = px >= margin && py >= margin && px < margin + iw && py < margin + ih;
            let model_pixel = if inside {
                Some((py - margin) * iw + (px - margin))
            } else {
                None
            };
            for channel in 0..3 {
                let index = patch_pixel * 3 + channel;
                let observed = patch.rgb[index];
                let (c, s) = match model_pixel {
                    Some(pixel) => model.params_at(pixel, channel),
                    None => (0.0, 1.0),
                };
                provisional[index] = operator.decompose(c, s, observed);
                weights[index] = s.clamp(BACKGROUND_WEIGHT_FLOOR, 1.0);
            }
        }
    }
    let mut numerator = vec![0f32; provisional.len()];
    for ((slot, &value), &weight) in numerator
        .iter_mut()
        .zip(provisional.iter())
        .zip(weights.iter())
    {
        *slot = value * weight;
    }
    let blurred_num = box_blur_rgb(&numerator, patch.width, patch.height, margin);
    let blurred_den = box_blur_rgb(&weights, patch.width, patch.height, margin);

    // Regression over the mark pixels of the inner rect.
    let mut sum_dm = 0.0f64;
    let mut sum_mm = 0.0f64;
    let mut used = 0usize;
    let mut signal = vec![0f32; iw * ih * 3];
    let mut deviation = vec![0f32; iw * ih * 3];
    for iy in 0..ih {
        for ix in 0..iw {
            let inner_pixel = iy * iw + ix;
            let patch_pixel = (iy + margin) * patch.width + (ix + margin);
            let significant = model.mark_alpha(inner_pixel) >= ALPHA_SIGNIFICANT;
            for channel in 0..3 {
                let inner_index = inner_pixel * 3 + channel;
                let patch_index = patch_pixel * 3 + channel;
                let denominator = blurred_den[patch_index];
                let background = if denominator.abs() > f32::EPSILON {
                    blurred_num[patch_index] / denominator
                } else {
                    patch.rgb[patch_index]
                };
                let (c, s) = model.params_at(inner_pixel, channel);
                let m = operator.compose(c, s, background) - background;
                let d = patch.rgb[patch_index] - background;
                signal[inner_index] = m;
                deviation[inner_index] = d;
                if significant {
                    sum_dm += f64::from(d) * f64::from(m);
                    sum_mm += f64::from(m) * f64::from(m);
                }
            }
            if significant {
                used += 1;
            }
        }
    }
    if used < MIN_GAIN_PIXELS || sum_mm <= f64::EPSILON {
        return None;
    }
    let gain = (sum_dm / sum_mm) as f32;
    if !gain.is_finite() || gain < params.gain_min || gain > params.gain_max {
        return None;
    }

    // Detection statistic: explained amplitude over the residual's standard deviation. It grows
    // with the pixel count, matching the matched-filter SNR the prototype measured (40-100 for
    // true occurrences over busy content).
    let mut sum_rr = 0.0f64;
    let mut terms = 0usize;
    for iy in 0..ih {
        for ix in 0..iw {
            let inner_pixel = iy * iw + ix;
            if model.mark_alpha(inner_pixel) < ALPHA_SIGNIFICANT {
                continue;
            }
            for channel in 0..3 {
                let index = inner_pixel * 3 + channel;
                let residual = f64::from(deviation[index]) - f64::from(gain) * f64::from(signal[index]);
                sum_rr += residual * residual;
                terms += 1;
            }
        }
    }
    let degrees = terms.saturating_sub(1).max(1);
    let sigma = (sum_rr / degrees as f64).sqrt();
    let snr = if sigma <= f64::EPSILON {
        f32::MAX
    } else {
        ((f64::from(gain) * sum_mm.sqrt()) / sigma) as f32
    };
    if !snr.is_finite() || snr < params.min_snr {
        return None;
    }

    Some(GainVerdict {
        gain,
        snr,
        shift: lucas_kanade_shift(&deviation, &signal, iw, ih),
    })
}

/// Padded RGB f32 copy of `rect` with `margin` pixels of surrounding context.
///
/// Reads are clamped to the page edge, so a rect at the border gets replicated context rather
/// than zeros (zeros would look like a black background to the estimator).
struct PaddedPatch {
    width: usize,
    height: usize,
    rgb: Vec<f32>,
}

fn padded_patch(page: &RgbaImage, rect: PixelRect, margin: usize) -> Option<PaddedPatch> {
    let (pw, ph) = validate_page(page).ok()?;
    validate_rect(page, rect).ok()?;
    let width = rect.width as usize + margin * 2;
    let height = rect.height as usize + margin * 2;
    let raw = page.as_raw();
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        // Cast justification: rect coordinates are validated inside the page, and `margin` is a
        // clamped detection parameter, so both sides of the subtraction fit i64 comfortably.
        let source_y = (rect.y as i64 + row as i64 - margin as i64).clamp(0, ph as i64 - 1) as usize;
        for column in 0..width {
            let source_x =
                (rect.x as i64 + column as i64 - margin as i64).clamp(0, pw as i64 - 1) as usize;
            let base = (source_y * pw + source_x) * 4;
            rgb.push(f32::from(raw[base]));
            rgb.push(f32::from(raw[base + 1]));
            rgb.push(f32::from(raw[base + 2]));
        }
    }
    Some(PaddedPatch { width, height, rgb })
}

/// Separable box blur of an interleaved RGB plane, normalized over the VALID window only (the
/// window shrinks at the borders instead of replicating), so a weighted mean built from a
/// numerator and a denominator blurred the same way stays a proper weighted mean.
fn box_blur_rgb(src: &[f32], width: usize, height: usize, radius: usize) -> Vec<f32> {
    if radius == 0 || width == 0 || height == 0 {
        return src.to_vec();
    }
    let mut horizontal = vec![0f32; src.len()];
    let mut prefix = vec![0f32; (width + 1) * 3];
    for y in 0..height {
        let row = y * width * 3;
        prefix[..3].fill(0.0);
        for x in 0..width {
            for channel in 0..3 {
                prefix[(x + 1) * 3 + channel] = prefix[x * 3 + channel] + src[row + x * 3 + channel];
            }
        }
        for x in 0..width {
            let lo = x.saturating_sub(radius);
            let hi = (x + radius + 1).min(width);
            let count = count_f32(hi - lo);
            for channel in 0..3 {
                let sum = prefix[hi * 3 + channel] - prefix[lo * 3 + channel];
                horizontal[row + x * 3 + channel] = sum / count;
            }
        }
    }

    let mut out = vec![0f32; src.len()];
    let mut column_prefix = vec![0f32; (height + 1) * 3];
    for x in 0..width {
        column_prefix[..3].fill(0.0);
        for y in 0..height {
            for channel in 0..3 {
                column_prefix[(y + 1) * 3 + channel] =
                    column_prefix[y * 3 + channel] + horizontal[(y * width + x) * 3 + channel];
            }
        }
        for y in 0..height {
            let lo = y.saturating_sub(radius);
            let hi = (y + radius + 1).min(height);
            let count = count_f32(hi - lo);
            for channel in 0..3 {
                let sum = column_prefix[hi * 3 + channel] - column_prefix[lo * 3 + channel];
                out[(y * width + x) * 3 + channel] = sum / count;
            }
        }
    }
    out
}

/// First-order (Lucas-Kanade) estimate of the sub-pixel offset between the observed deviation
/// and the model's predicted mark signal.
///
/// Solves `min_delta sum (r + delta . grad m)^2` with `r = d - m`, which is exact to first order
/// for a shift well under one pixel — the measured jitter is <= 0.4 px. A singular structure
/// tensor (no gradient to lock onto) yields no shift rather than a wild guess.
fn lucas_kanade_shift(deviation: &[f32], signal: &[f32], width: usize, height: usize) -> SubpixelShift {
    if width < 3 || height < 3 {
        return SubpixelShift::NONE;
    }
    let (mut a11, mut a12, mut a22) = (0.0f64, 0.0f64, 0.0f64);
    let (mut b1, mut b2) = (0.0f64, 0.0f64);
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let pixel = y * width + x;
            for channel in 0..3 {
                let index = pixel * 3 + channel;
                let gx = f64::from(signal[index + 3] - signal[index - 3]) * 0.5;
                let gy = f64::from(
                    signal[index + width * 3] - signal[index - width * 3],
                ) * 0.5;
                let residual = f64::from(deviation[index] - signal[index]);
                a11 += gx * gx;
                a12 += gx * gy;
                a22 += gy * gy;
                b1 += residual * gx;
                b2 += residual * gy;
            }
        }
    }
    let determinant = a11 * a22 - a12 * a12;
    if determinant.abs() <= f64::EPSILON {
        return SubpixelShift::NONE;
    }
    let dx = -(a22 * b1 - a12 * b2) / determinant;
    let dy = -(a11 * b2 - a12 * b1) / determinant;
    SubpixelShift::new(dx as f32, dy as f32)
}

/// Estimate the background under an occurrence from an existing model, for building
/// [`SampleBackground::Estimated`] samples (progressive refinement across chapters).
///
/// This is the same weighted estimator the gain test uses. It REQUIRES a model: without one,
/// nothing is known about the background under an opaque mark pixel, and inventing a value
/// there would poison the next fit. [`refit_with_refined_backgrounds`] is the loop that feeds its
/// output back into the fit, and the fixed iteration count there is why that path's alpha is
/// quoted at `ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT`.
///
/// # Errors
/// Page/rect validation failures, or [`WatermarkError::GeometryMismatch`] when the rect does not
/// match the model.
pub(super) fn provisional_background(
    page: &RgbaImage,
    rect: PixelRect,
    model: &WatermarkModel,
    params: &DetectionParams,
) -> Result<Vec<f32>, WatermarkError> {
    validate_page(page)?;
    validate_rect(page, rect)?;
    if rect.width != model.width || rect.height != model.height {
        return Err(WatermarkError::GeometryMismatch {
            expected_width: model.width,
            expected_height: model.height,
            width: rect.width,
            height: rect.height,
        });
    }
    let params = params.normalized();
    let margin = params.background_blur_radius as usize;
    let patch = padded_patch(page, rect, margin).ok_or(WatermarkError::RectOutOfPage {
        rect,
        width: page.width(),
        height: page.height(),
    })?;
    let operator = model.operator();
    let (iw, ih) = (rect.width as usize, rect.height as usize);
    let mut numerator = vec![0f32; patch.rgb.len()];
    let mut weights = vec![0f32; patch.rgb.len()];
    for py in 0..patch.height {
        for px in 0..patch.width {
            let patch_pixel = py * patch.width + px;
            let inside = px >= margin && py >= margin && px < margin + iw && py < margin + ih;
            let model_pixel = if inside {
                Some((py - margin) * iw + (px - margin))
            } else {
                None
            };
            for channel in 0..3 {
                let index = patch_pixel * 3 + channel;
                let (c, s) = match model_pixel {
                    Some(pixel) => model.params_at(pixel, channel),
                    None => (0.0, 1.0),
                };
                let weight = s.clamp(BACKGROUND_WEIGHT_FLOOR, 1.0);
                numerator[index] = operator.decompose(c, s, patch.rgb[index]) * weight;
                weights[index] = weight;
            }
        }
    }
    let blurred_num = box_blur_rgb(&numerator, patch.width, patch.height, margin);
    let blurred_den = box_blur_rgb(&weights, patch.width, patch.height, margin);
    let mut out = vec![0f32; iw * ih * 3];
    for iy in 0..ih {
        for ix in 0..iw {
            let patch_pixel = (iy + margin) * patch.width + (ix + margin);
            let inner_pixel = iy * iw + ix;
            for channel in 0..3 {
                let patch_index = patch_pixel * 3 + channel;
                let denominator = blurred_den[patch_index];
                out[inner_pixel * 3 + channel] = if denominator.abs() > f32::EPSILON {
                    blurred_num[patch_index] / denominator
                } else {
                    patch.rgb[patch_index]
                };
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------------------

/// Honest QA numbers of one removal.
///
/// What this measures, precisely: whether the recovered background, quantized back to a byte,
/// RECOMPOSES to the observation. It therefore reports quantization loss and clipping — the
/// pixels that are irrecoverable no matter how good the model is. It does NOT validate the
/// model; model quality is what the detection gain and t-statistic report.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct RemovalResidual {
    /// Pixels the mark actually covers (`alpha >= ALPHA_SIGNIFICANT`).
    pub mark_pixels: usize,
    /// Mark pixels that recompose to the exact observed bytes.
    pub exact_pixels: usize,
    /// Mark pixels whose recovered background left `0..=255` by more than quantization can
    /// explain: information the source no longer contains.
    pub clipped_pixels: usize,
    /// Worst `0.5/s` over the mark, LSB: the recovery's own uncertainty, i.e. how much the
    /// division amplifies one quantization step.
    pub max_uncertainty_lsb: f32,
    /// Sum of squared recomposition errors per channel over mark pixels (f64: a chapter merges
    /// thousands of patches).
    sum_sq: [f64; 3],
}

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

/// The recovered pixels of one occurrence, ready to be handed to the overlay layer.
#[derive(Debug, Clone)]
pub(super) struct RegionPatch {
    pub rect: PixelRect,
    /// RGBA8, `width*height*4` entries. Alpha is copied from the source page.
    pub pixels: Vec<u8>,
    pub residual: RemovalResidual,
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
pub(super) fn remove_occurrence(
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
pub(super) fn remove_occurrences_on_page(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic mark: a rectangular "glyph" of a bright fill inside a dark outline, i.e. the
    /// structure the measured 腾讯动漫 mark has (white glyph, dark outline). Per-channel alpha
    /// makes it a colour mark when the three values differ; a coloured FILL makes it one the way
    /// the measured chapter-two mark is (its deposit differs by up to 120 LSB between channels).
    struct SyntheticMark {
        width: u32,
        height: u32,
        c: Vec<f32>,
        s: Vec<f32>,
    }

    impl SyntheticMark {
        /// `alpha` is per channel; the fill is bright, the outline dark, both at that alpha.
        fn new(width: u32, height: u32, alpha: [f32; 3]) -> Self {
            Self::with_colours(width, height, alpha, [235.0, 243.0, 242.0], [12.0, 14.0, 13.0])
        }

        /// The same glyph geometry with explicit fill and outline colours: two marks built this
        /// way have the SAME shape and different deposits, which is the measured case a
        /// template-keyed catalog would merge.
        fn with_colours(
            width: u32,
            height: u32,
            alpha: [f32; 3],
            fill: [f32; 3],
            outline: [f32; 3],
        ) -> Self {
            let (w, h) = (width as usize, height as usize);
            let mut c = vec![0f32; w * h * 3];
            let mut s = vec![1f32; w * h * 3];
            for y in 0..h {
                for x in 0..w {
                    // Glyph body: a centred rectangle with a 2 px outline ring around it.
                    let in_glyph = x >= 2 && y >= 2 && x + 2 < w && y + 2 < h;
                    let in_outline = (x >= 1 && y >= 1 && x + 1 < w && y + 1 < h) && !in_glyph;
                    if !in_glyph && !in_outline {
                        continue;
                    }
                    let colour = if in_glyph { fill } else { outline };
                    for channel in 0..3 {
                        let index = (y * w + x) * 3 + channel;
                        c[index] = alpha[channel] * colour[channel];
                        s[index] = 1.0 - alpha[channel];
                    }
                }
            }
            Self {
                width,
                height,
                c,
                s,
            }
        }

        /// Composite the mark onto `page` at `rect`, optionally scaling the alpha (`k = 1` is the
        /// mark itself; `k != 1` makes a look-alike with the same SHAPE but a different
        /// amplitude, which correlation cannot tell apart).
        fn composite(&self, page: &mut RgbaImage, rect: PixelRect, alpha_scale: f32) {
            let (w, _h) = (self.width as usize, self.height as usize);
            for y in 0..self.height as usize {
                for x in 0..w {
                    let index = (y * w + x) * 3;
                    let pixel = page.get_pixel_mut(rect.x + x as u32, rect.y + y as u32);
                    for channel in 0..3 {
                        let c = self.c[index + channel] * alpha_scale;
                        let s = 1.0 - (1.0 - self.s[index + channel]) * alpha_scale;
                        let background = f32::from(pixel[channel]);
                        pixel[channel] = (c + s * background).clamp(0.0, 255.0).round() as u8;
                    }
                }
            }
        }

        /// A hollow variant: the 2 px outline ring only, no fill. The measured marks are thin
        /// glyph strokes, and a hollow mark is what lets the `s`-weighted background estimator
        /// work the way it does on real data — it needs unmarked pixels within
        /// `BACKGROUND_BLUR_RADIUS_PX` of every mark pixel, which a solid block this size does
        /// not have.
        fn ring(width: u32, height: u32, alpha: [f32; 3]) -> Self {
            let mut mark = Self::new(width, height, alpha);
            let (w, h) = (width as usize, height as usize);
            for y in 0..h {
                for x in 0..w {
                    if x >= 2 && y >= 2 && x + 2 < w && y + 2 < h {
                        for channel in 0..3 {
                            let index = (y * w + x) * 3 + channel;
                            mark.c[index] = 0.0;
                            mark.s[index] = 1.0;
                        }
                    }
                }
            }
            mark
        }

        fn rect_at(&self, x: u32, y: u32) -> PixelRect {
            PixelRect::new(x, y, self.width, self.height)
        }

        fn model(&self) -> WatermarkModel {
            WatermarkModel::from_parts(
                self.width,
                self.height,
                self.c.clone(),
                self.s.clone(),
                alpha_blend_operator(),
                test_provenance(),
            )
            .expect("synthetic model parameters are in range")
        }
    }

    fn test_provenance() -> ModelProvenance {
        ModelProvenance {
            samples: 2,
            method: FitMethod::ClosedFormFlat,
            conditioning: ModelConditioning::Separable {
                levels: vec![0.0, 255.0],
                spread: 255.0,
                min_pixel_spread: 255.0,
                alpha: AlphaUncertainty::from_flat_fit(255.0, 0.18),
            },
            clamped_pixels: 0,
        }
    }

    fn solid_page(width: u32, height: u32, colour: [u8; 3]) -> RgbaImage {
        RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([colour[0], colour[1], colour[2], 255]),
        )
    }

    /// Deterministic pseudo-texture so a "busy content" page is reproducible without fixtures.
    fn textured_page(width: u32, height: u32, base: u8) -> RgbaImage {
        let mut page = solid_page(width, height, [base, base, base]);
        for (x, y, pixel) in page.enumerate_pixels_mut() {
            let wave = ((x * 7 + y * 13) % 61) as i32 - 30;
            for channel in 0..3 {
                let value = i32::from(pixel[channel]) + wave / 3;
                pixel[channel] = value.clamp(0, 255) as u8;
            }
        }
        page
    }

    /// Build the two flat samples (over white and over black) that condition a mark.
    fn flat_samples(mark: &SyntheticMark, rect: PixelRect) -> Vec<CalibrationSample> {
        let mut white = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [255; 3]);
        let mut black = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [0; 3]);
        mark.composite(&mut white, rect, 1.0);
        mark.composite(&mut black, rect, 1.0);
        let params = SampleParams::default();
        let (white_verdict, white_sample) =
            calibration_sample_from_page(&white, 0, rect, &params).expect("white sample builds");
        let (black_verdict, black_sample) =
            calibration_sample_from_page(&black, 1, rect, &params).expect("black sample builds");
        assert!(white_verdict.is_calibration(), "white ring must be flat");
        assert!(black_verdict.is_calibration(), "black ring must be flat");
        vec![
            white_sample.expect("flat white sample"),
            black_sample.expect("flat black sample"),
        ]
    }

    #[test]
    fn closed_form_recovers_the_mark_and_removal_is_exact() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let rect = mark.rect_at(10, 12);
        let samples = flat_samples(&mark, rect);

        let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
            .expect("well conditioned");
        assert_eq!(model.provenance().method, FitMethod::ClosedFormFlat);
        assert_eq!(model.provenance().clamped_pixels, 0);
        for (index, (&fitted_c, &true_c)) in model.c().iter().zip(mark.c.iter()).enumerate() {
            assert!(
                (fitted_c - true_c).abs() <= 1.0,
                "c[{index}] = {fitted_c}, expected {true_c}"
            );
        }
        for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
            assert!(
                (fitted_s - true_s).abs() <= 0.01,
                "s[{index}] = {fitted_s}, expected {true_s}"
            );
        }

        // Removal over a background level that was never calibrated on.
        let mut page = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [128; 3]);
        mark.composite(&mut page, rect, 1.0);
        let patch = remove_occurrence(&page, rect, &model, SubpixelShift::NONE)
            .expect("removal of a valid rect");
        for (index, chunk) in patch.pixels.chunks_exact(4).enumerate() {
            for (channel, &byte) in chunk.iter().take(3).enumerate() {
                let value = i32::from(byte);
                assert!(
                    (value - 128).abs() <= 1,
                    "pixel {index} channel {channel} recovered as {value}, expected 128"
                );
            }
        }
        assert_eq!(patch.residual.clipped_pixels, 0);
        assert!(patch.residual.exact_share() > 0.99);
    }

    /// The measured chapter-two case: every calibration sample sits on pure white. The deposit is
    /// exact there, so a model IS produced and removal at that level is exact; only the alpha
    /// scale is an assumption, and the verdict has to say so and name the fix.
    #[test]
    fn one_background_level_yields_a_graded_model_with_a_stated_alpha_uncertainty() {
        let mark = SyntheticMark::new(20, 16, [0.18; 3]);
        let rect = mark.rect_at(8, 8);
        let params = SampleParams::default();
        let page_size = (rect.right() as u32 + 8, rect.bottom() as u32 + 8);
        let mut samples = Vec::new();
        for page_index in 0..2 {
            let mut page = solid_page(page_size.0, page_size.1, [255; 3]);
            mark.composite(&mut page, rect, 1.0);
            let (_, sample) = calibration_sample_from_page(&page, page_index, rect, &params)
                .expect("sample builds");
            samples.push(sample.expect("flat sample"));
        }

        let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
            .expect("an exact deposit is enough for a graded model");
        assert_eq!(model.provenance().method, FitMethod::DepositExact);
        let verdict = &model.provenance().conditioning;
        let uncertainty = match verdict {
            ModelConditioning::DepositExact {
                samples, alpha, ..
            } => {
                assert_eq!(*samples, 2);
                assert_eq!(alpha.source, AlphaSource::Assumed);
                *alpha
            }
            other => panic!("expected DepositExact, got {other:?}"),
        };
        assert!(!verdict.is_separable(), "the slope is not data here");
        assert!(verdict.produces_model());
        assert!(
            uncertainty.percent >= ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
            "the assumption must not be quoted as better than it is: {uncertainty:?}"
        );
        assert!(uncertainty.dark_max_lsb > uncertainty.rms_lsb);
        // The observed level is white, so the fix names a DARK background.
        match verdict.suggested_background() {
            Some(SuggestedBackground::Darker { at_most }) => {
                assert!(at_most <= 255.0 - MIN_BACKGROUND_SPREAD);
            }
            other => panic!("expected a request for a darker sample, got {other:?}"),
        }

        // Removal at the calibrated level is EXACT, whatever the alpha scale turns out to be.
        let mut white = solid_page(page_size.0, page_size.1, [255; 3]);
        mark.composite(&mut white, rect, 1.0);
        let patch = remove_occurrence(&white, rect, &model, SubpixelShift::NONE).expect("removal");
        for (index, chunk) in patch.pixels.chunks_exact(4).enumerate() {
            for (channel, &byte) in chunk.iter().take(3).enumerate() {
                assert!(
                    i32::from(byte) >= 254,
                    "pixel {index} channel {channel} recovered as {byte}, expected white"
                );
            }
        }

        // Far from the calibrated level the assumed alpha shows, and the gain test is what keeps
        // that honest: the occurrence no longer regresses to unity, so detection refuses it
        // instead of removing it with a model that does not describe it.
        let mut dark = solid_page(page_size.0, page_size.1, [10; 3]);
        mark.composite(&mut dark, rect, 1.0);
        let mut dark_kind = WatermarkKind::new(
            "test.single_level.dark",
            MarkTemplate::from_page(&white, rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in &samples {
            dark_kind.add_sample(sample.clone()).expect("geometry");
        }
        dark_kind.refit().expect("graded fit succeeds");
        let dark_hits = find_occurrences(&dark, &dark_kind, &DetectionParams::default())
            .expect("scan");
        assert!(
            dark_hits.is_empty(),
            "a graded model must not vouch for an occurrence far from its calibration level: \
             {dark_hits:?}"
        );

        // The kind stores the graded verdict together with the model it licensed.
        let template = MarkTemplate::from_page(&white, rect).expect("template");
        let mut kind = WatermarkKind::new(
            "test.single_level",
            template.clone(),
            alpha_blend_operator(),
        );
        for sample in &samples {
            kind.add_sample(sample.clone()).expect("matching geometry");
        }
        kind.refit().expect("graded fit succeeds");
        assert!(kind.model().is_some());
        assert!(!kind.conditioning().is_separable());
        assert!(kind.conditioning().produces_model());
        assert!(kind.conditioning().alpha_uncertainty().is_some());

        // A caller who knows the scale from elsewhere — a persisted model of the same source, a
        // sibling kind — states it, and the engine uses that number and that uncertainty.
        let mut stated = WatermarkKind::new("test.stated", template, alpha_blend_operator());
        stated.set_alpha_assumption(AlphaAssumption::Stated {
            peak_alpha: 0.18,
            uncertainty_percent: 8.0,
        });
        for sample in samples {
            stated.add_sample(sample).expect("matching geometry");
        }
        stated.refit().expect("graded fit succeeds");
        let stated_model = stated.model().expect("model");
        assert!(
            (stated_model.peak_alpha() - 0.18).abs() < 0.01,
            "the stated peak opacity must be the one used: {}",
            stated_model.peak_alpha()
        );
        let stated_uncertainty = stated
            .conditioning()
            .alpha_uncertainty()
            .expect("graded verdict");
        assert!((stated_uncertainty.percent - 8.0).abs() < 0.01);
        assert!(stated_uncertainty.rms_lsb < uncertainty.rms_lsb);
    }

    /// Alpha measured channel-neutral on both chapters, so per-channel alpha is NOT required by a
    /// colour mark — but it stays supported, and a source that does vary per channel must not be
    /// collapsed onto one value.
    #[test]
    fn per_channel_alpha_is_supported_although_measured_marks_are_channel_neutral() {
        let alpha = [0.30f32, 0.10, 0.20];
        let mark = SyntheticMark::new(22, 16, alpha);
        let rect = mark.rect_at(9, 7);
        let samples = flat_samples(&mark, rect);
        let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
            .expect("well conditioned");

        // Pick a pixel inside the glyph body and check the three channels stayed distinct.
        let pixel = (8 * model.width() as usize) + 8;
        let fitted: Vec<f32> = (0..3).map(|ch| model.params_at(pixel, ch).1).collect();
        for channel in 0..3 {
            assert!(
                (fitted[channel] - (1.0 - alpha[channel])).abs() <= 0.01,
                "channel {channel} fitted s = {}, expected {}",
                fitted[channel],
                1.0 - alpha[channel]
            );
        }
        assert!(
            (fitted[0] - fitted[1]).abs() > 0.1 && (fitted[1] - fitted[2]).abs() > 0.05,
            "channels were collapsed: {fitted:?}"
        );
    }

    #[test]
    fn non_uniform_ring_is_refused_for_calibration_but_kept_as_template() {
        let mark = SyntheticMark::new(20, 16, [0.18; 3]);
        let rect = mark.rect_at(12, 12);
        let mut page = solid_page(rect.right() as u32 + 12, rect.bottom() as u32 + 12, [255; 3]);
        // A hard edge crossing the ring: half the surroundings are dark.
        for y in 0..page.height() {
            for x in 0..page.width() {
                if x > rect.x + rect.width / 2 {
                    page.put_pixel(x, y, image::Rgba([20, 20, 20, 255]));
                }
            }
        }
        mark.composite(&mut page, rect, 1.0);

        let verdict = validate_calibration_sample(&page, rect, &SampleParams::default());
        assert!(!verdict.is_calibration(), "structured ring must be refused");
        assert!(
            verdict.usable_as_template(),
            "a refused calibration target is still a valid detection template"
        );
        match verdict {
            SampleVerdict::TemplateOnly {
                ring_std,
                std_limit,
                ..
            } => assert!(
                ring_std.iter().any(|&value| value > std_limit),
                "the refusal must report the measured std that caused it: {ring_std:?}"
            ),
            other => panic!("expected TemplateOnly, got {other:?}"),
        }

        // And the calibration helper must not hand back a sample for it.
        let (_, sample) =
            calibration_sample_from_page(&page, 0, rect, &SampleParams::default()).expect("valid rect");
        assert!(sample.is_none());
    }

    #[test]
    fn subpixel_shift_more_than_halves_the_residual() {
        let mark = SyntheticMark::new(24, 20, [0.18; 3]);
        let rect = mark.rect_at(10, 10);
        let model = mark.model();
        let shift = SubpixelShift::new(0.5, 0.5);

        // Compose an occurrence that landed half a pixel off the grid: the model itself is
        // resampled, exactly as a resampled strip would carry it.
        let background = 128u8;
        let mut page = solid_page(
            rect.right() as u32 + 8,
            rect.bottom() as u32 + 8,
            [background; 3],
        );
        for y in 0..rect.height as usize {
            for x in 0..rect.width as usize {
                let pixel = page.get_pixel_mut(rect.x + x as u32, rect.y + y as u32);
                for channel in 0..3 {
                    let (c, s) = model.params_bilinear(
                        x as f32 - shift.dx,
                        y as f32 - shift.dy,
                        channel,
                    );
                    pixel[channel] = (c + s * f32::from(background)).clamp(0.0, 255.0).round() as u8;
                }
            }
        }

        let deviation = |patch: &RegionPatch| -> f32 {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for chunk in patch.pixels.chunks_exact(4) {
                for &byte in chunk.iter().take(3) {
                    let error = f64::from(i32::from(byte) - i32::from(background));
                    sum += error * error;
                    count += 1;
                }
            }
            (sum / count.max(1) as f64).sqrt() as f32
        };

        let unshifted = remove_occurrence(&page, rect, &model, SubpixelShift::NONE).expect("removal");
        let shifted = remove_occurrence(&page, rect, &model, shift).expect("removal");
        let (before, after) = (deviation(&unshifted), deviation(&shifted));
        assert!(before > 1.0, "the misaligned removal should leave a residual");
        assert!(
            after < before * 0.5,
            "subpixel model shift must cut the residual: {before} -> {after}"
        );
    }

    #[test]
    fn quantization_is_clamped_once_and_only_real_clipping_is_reported() {
        // Hand-built model whose recovery legitimately overshoots the byte range purely because
        // the observation was rounded: c has a fractional part and s halves the range.
        let (width, height) = (8u32, 6u32);
        let pixels = (width as usize) * (height as usize);
        let model = WatermarkModel::from_parts(
            width,
            height,
            vec![10.3; pixels * 3],
            vec![0.5; pixels * 3],
            alpha_blend_operator(),
            test_provenance(),
        )
        .expect("in-range parameters");

        // Two true backgrounds at the extremes of the range.
        let mut page = solid_page(width + 4, height + 4, [0; 3]);
        let rect = PixelRect::new(2, 2, width, height);
        for y in 0..height {
            for x in 0..width {
                let truth = if x.is_multiple_of(2) { 0.0f32 } else { 255.0f32 };
                let observed = (10.3 + 0.5 * truth).round().clamp(0.0, 255.0) as u8;
                page.put_pixel(rect.x + x, rect.y + y, image::Rgba([observed; 4]));
            }
        }

        let patch = remove_occurrence(&page, rect, &model, SubpixelShift::NONE).expect("removal");
        for (index, chunk) in patch.pixels.chunks_exact(4).enumerate() {
            let x = index % width as usize;
            let expected = if x.is_multiple_of(2) { 0u8 } else { 255u8 };
            for (channel, &byte) in chunk.iter().take(3).enumerate() {
                assert_eq!(
                    byte, expected,
                    "pixel {index} channel {channel} lost its value to intermediate clamping"
                );
            }
        }
        assert_eq!(
            patch.residual.clipped_pixels, 0,
            "an excursion explainable by one quantization step is not clipping"
        );
        assert!(patch.residual.rms().iter().all(|&value| value <= 0.5));

        // A genuinely irrecoverable pixel: the observation cannot come from any in-range
        // background under this model.
        let mut saturated = solid_page(width + 4, height + 4, [250; 3]);
        for y in 0..height {
            for x in 0..width {
                saturated.put_pixel(rect.x + x, rect.y + y, image::Rgba([250, 250, 250, 255]));
            }
        }
        let clipped =
            remove_occurrence(&saturated, rect, &model, SubpixelShift::NONE).expect("removal");
        assert_eq!(clipped.residual.clipped_pixels, clipped.residual.mark_pixels);
        assert!(clipped.residual.clipped_share() > 0.99);
    }

    #[test]
    fn detection_rejects_a_shape_alike_with_the_wrong_amplitude() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let model = mark.model();
        let anchor = 60u32;
        let true_rect = mark.rect_at(anchor, 30);

        let mut page = textured_page(140, 260, 150);
        mark.composite(&mut page, true_rect, 1.0);
        // Same glyph, twice the alpha: correlation is scale invariant and scores it 1.0.
        mark.composite(&mut page, mark.rect_at(anchor, 120), 2.0);
        // An exact copy far off the anchor column.
        mark.composite(&mut page, mark.rect_at(10, 200), 1.0);

        let template_page = {
            let mut clean = solid_page(140, 260, [150; 3]);
            mark.composite(&mut clean, true_rect, 1.0);
            clean
        };
        let template = MarkTemplate::from_page(&template_page, true_rect).expect("template");
        let mut kind = WatermarkKind::new("test.mark", template, alpha_blend_operator());
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("two separated levels");

        let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
        assert_eq!(
            found.len(),
            1,
            "exactly the true occurrence must survive, got {found:?}"
        );
        let hit = &found[0];
        assert_eq!(hit.rect, true_rect);
        assert!(hit.is_removal_safe());
        match hit.evidence {
            AcceptanceEvidence::Gain { gain, snr } => {
                assert!((gain - 1.0).abs() < 0.1, "gain {gain} should be near unity");
                assert!(snr >= MIN_DETECTION_T);
            }
            AcceptanceEvidence::Correlation => panic!("a kind with a model must use the gain test"),
        }

        // Without a model the same page can only produce correlation-only accepts, and those are
        // explicitly not removal-safe.
        let mut blind = WatermarkKind::new(
            "test.blind",
            MarkTemplate::from_page(&template_page, true_rect).expect("template"),
            alpha_blend_operator(),
        );
        assert!(blind.refit().is_err());
        let blind_hits = find_occurrences(&page, &blind, &DetectionParams::default()).expect("scan");
        assert!(blind_hits.iter().all(|hit| !hit.is_removal_safe()));
        assert!(
            blind_hits.len() >= 2,
            "correlation alone cannot tell the look-alike apart: {blind_hits:?}"
        );
        let error = remove_occurrences_on_page(&page, &model, &blind_hits)
            .expect_err("unverified occurrences must be refused");
        assert!(matches!(error, WatermarkError::UnverifiedOccurrence { .. }));
    }

    #[test]
    fn overlapping_kinds_are_resolved_by_score() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let anchor = 50u32;
        let true_rect = mark.rect_at(anchor, 40);
        let mut page = textured_page(120, 140, 150);
        mark.composite(&mut page, true_rect, 1.0);

        let template_page = {
            let mut clean = solid_page(120, 140, [150; 3]);
            mark.composite(&mut clean, true_rect, 1.0);
            clean
        };
        let template = MarkTemplate::from_page(&template_page, true_rect).expect("template");

        let mut exact = WatermarkKind::new("test.exact", template.clone(), alpha_blend_operator());
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            exact.add_sample(sample).expect("matching geometry");
        }
        exact.refit().expect("separable");

        // A second kind claiming the same spot with a slightly wrong alpha: still inside the gain
        // window, but a worse fit, so it must lose the arbitration instead of both being applied.
        let mut approximate =
            WatermarkKind::new("test.approximate", template, alpha_blend_operator());
        let weak = SyntheticMark::new(24, 18, [0.18 * 0.92; 3]);
        for sample in flat_samples(&weak, weak.rect_at(4, 4)) {
            approximate.add_sample(sample).expect("matching geometry");
        }
        approximate.refit().expect("separable");

        let catalog = [exact, approximate];
        let hits = scan_page(&page, 3, &catalog, &DetectionParams::default()).expect("scan");
        assert_eq!(hits.len(), 1, "overlapping kinds must not both apply: {hits:?}");
        assert_eq!(hits[0].kind_index, 0, "the better-fitting kind must win");
        assert_eq!(hits[0].page_index, 3);
    }

    #[test]
    fn scan_chapter_is_deterministic_and_page_ordered() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let anchor = 40u32;
        let rect = mark.rect_at(anchor, 30);
        let mut first = textured_page(100, 120, 150);
        mark.composite(&mut first, rect, 1.0);
        let mut second = textured_page(100, 120, 90);
        mark.composite(&mut second, mark.rect_at(anchor, 60), 1.0);

        let template_page = {
            let mut clean = solid_page(100, 120, [150; 3]);
            mark.composite(&mut clean, rect, 1.0);
            clean
        };
        let mut kind = WatermarkKind::new(
            "test.chapter",
            MarkTemplate::from_page(&template_page, rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");

        let pages = [&first, &second];
        let catalog = [kind];
        let params = DetectionParams::default();
        let first_run = scan_chapter(&pages, &catalog, &params).expect("scan");
        let second_run = scan_chapter(&pages, &catalog, &params).expect("scan");
        assert_eq!(first_run, second_run, "the scan must be deterministic");
        assert_eq!(first_run.len(), 2);
        assert_eq!(first_run[0].page_index, 0);
        assert_eq!(first_run[1].page_index, 1);
    }

    #[test]
    fn public_entry_points_reject_bad_geometry_without_panicking() {
        let mark = SyntheticMark::new(16, 12, [0.18; 3]);
        let model = mark.model();
        let page = solid_page(40, 30, [128; 3]);

        let outside = PixelRect::new(30, 25, 16, 12);
        assert!(matches!(
            remove_occurrence(&page, outside, &model, SubpixelShift::NONE),
            Err(WatermarkError::RectOutOfPage { .. })
        ));
        assert!(matches!(
            remove_occurrence(&page, PixelRect::new(0, 0, 0, 12), &model, SubpixelShift::NONE),
            Err(WatermarkError::EmptyRect { .. })
        ));
        assert!(matches!(
            remove_occurrence(&page, PixelRect::new(0, 0, 8, 8), &model, SubpixelShift::NONE),
            Err(WatermarkError::GeometryMismatch { .. })
        ));
        assert!(matches!(
            MarkTemplate::from_page(&page, PixelRect::new(0, 0, 8, 8)),
            Err(WatermarkError::FlatTemplate { .. })
        ));
        assert!(matches!(
            estimate_model(&[], alpha_blend_operator(), AlphaAssumption::default()),
            Err(ModelFitError::Invalid(WatermarkError::NoSamples))
        ));
        assert!(matches!(
            WatermarkModel::from_parts(
                4,
                4,
                vec![0.0; 3],
                vec![1.0; 48],
                alpha_blend_operator(),
                test_provenance()
            ),
            Err(WatermarkError::BufferLength { .. })
        ));
        assert!(matches!(
            WatermarkModel::from_parts(
                2,
                2,
                vec![0.0; 12],
                vec![0.0; 12],
                alpha_blend_operator(),
                test_provenance()
            ),
            Err(WatermarkError::ParameterOutOfRange { what: "s", .. })
        ));
        assert!(matches!(
            validate_calibration_sample(&page, outside, &SampleParams::default()),
            SampleVerdict::Unusable {
                reason: SampleRejection::BadRect
            }
        ));
    }

    #[test]
    fn theil_sen_path_fits_from_estimated_backgrounds() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let rect = mark.rect_at(10, 10);
        let bootstrap = mark.model();
        let params = DetectionParams::default();

        // Three occurrences over well-separated levels, with the background supplied per pixel by
        // the engine's own estimator instead of a flat-ring reading. No sample is `Flat`, so the
        // robust path has to carry the fit.
        let mut samples = Vec::new();
        for (page_index, level) in [20u8, 128, 240].into_iter().enumerate() {
            let mut page = solid_page(
                rect.right() as u32 + 12,
                rect.bottom() as u32 + 12,
                [level; 3],
            );
            mark.composite(&mut page, rect, 1.0);
            let estimated = provisional_background(&page, rect, &bootstrap, &params)
                .expect("background estimate");
            for (index, &value) in estimated.iter().enumerate() {
                assert!(
                    (value - f32::from(level)).abs() <= 3.0,
                    "estimated background[{index}] = {value}, expected ~{level}"
                );
            }
            samples.push(
                CalibrationSample::from_page(
                    &page,
                    page_index,
                    rect,
                    SampleBackground::Estimated { values: estimated },
                )
                .expect("sample builds"),
            );
        }

        let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
            .expect("well conditioned");
        assert_eq!(model.provenance().method, FitMethod::TheilSen);
        // A slope fitted against ESTIMATES is never quoted as well as one fitted against exact
        // backgrounds: the plan measured that path at +-10-20% and the verdict must say so.
        let uncertainty = model
            .provenance()
            .conditioning
            .alpha_uncertainty()
            .expect("a model always carries one");
        assert_eq!(uncertainty.source, AlphaSource::EstimatedBackgrounds);
        assert!(
            (uncertainty.percent - ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT).abs() < 0.01
        );
        for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
            assert!(
                (fitted_s - true_s).abs() <= 0.05,
                "s[{index}] = {fitted_s}, expected {true_s}"
            );
        }
        for (index, (&fitted_c, &true_c)) in model.c().iter().zip(mark.c.iter()).enumerate() {
            assert!(
                (fitted_c - true_c).abs() <= 8.0,
                "c[{index}] = {fitted_c}, expected {true_c}"
            );
        }
    }

    /// The refinement loop runs a fixed, named number of iterations because nothing in the data
    /// selects a stopping point, and it never downgrades an exactly measured background.
    #[test]
    fn background_refinement_iterates_a_fixed_number_of_times_and_keeps_exact_samples() {
        let mark = SyntheticMark::ring(24, 18, [0.18; 3]);
        let rect = mark.rect_at(10, 10);
        let (page_w, page_h) = (rect.right() as u32 + 12, rect.bottom() as u32 + 12);
        let template_page = {
            let mut clean = solid_page(page_w, page_h, [150; 3]);
            mark.composite(&mut clean, rect, 1.0);
            clean
        };
        let mut kind = WatermarkKind::new(
            "test.refine",
            MarkTemplate::from_page(&template_page, rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");

        // With only exactly measured backgrounds there is nothing to refine, and replacing them
        // with estimates would throw away the only hard evidence in the set.
        let pages: Vec<RgbaImage> = Vec::new();
        let page_refs: Vec<&RgbaImage> = pages.iter().collect();
        assert!(matches!(
            refit_with_refined_backgrounds(&page_refs, &mut kind, &DetectionParams::default()),
            Err(ModelFitError::Invalid(WatermarkError::NothingToRefine))
        ));

        // Add occurrences over busy content, with backgrounds the model itself estimated.
        let bootstrap = kind.model().expect("fitted").clone();
        let params = DetectionParams::default();
        let mut busy_pages = Vec::new();
        for base in [40u8, 200] {
            let mut page = textured_page(page_w, page_h, base);
            mark.composite(&mut page, rect, 1.0);
            busy_pages.push(page);
        }
        for (page_index, page) in busy_pages.iter().enumerate() {
            let values =
                provisional_background(page, rect, &bootstrap, &params).expect("estimate");
            kind.add_sample(
                CalibrationSample::from_page(
                    page,
                    page_index,
                    rect,
                    SampleBackground::Estimated { values },
                )
                .expect("sample builds"),
            )
            .expect("matching geometry");
        }

        let refs: Vec<&RgbaImage> = busy_pages.iter().collect();
        refit_with_refined_backgrounds(&refs, &mut kind, &params).expect("refinement");
        let model = kind.model().expect("refinement keeps a model");
        for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
            assert!(
                (fitted_s - true_s).abs() <= 0.1,
                "s[{index}] = {fitted_s}, expected {true_s}"
            );
        }
        assert!(
            kind.samples().iter().filter(|s| s.is_flat()).count() == 2,
            "the exactly measured samples must survive refinement"
        );
    }

    #[test]
    fn a_pixel_that_never_saw_two_backgrounds_is_reported_as_underdetermined() {
        let mark = SyntheticMark::new(16, 12, [0.18; 3]);
        let rect = mark.rect_at(6, 6);
        let pixels = (rect.width as usize) * (rect.height as usize);
        let mut samples = Vec::new();
        for (page_index, level) in [10u8, 250].into_iter().enumerate() {
            let mut page = solid_page(
                rect.right() as u32 + 8,
                rect.bottom() as u32 + 8,
                [level; 3],
            );
            mark.composite(&mut page, rect, 1.0);
            let mut values = vec![f32::from(level); pixels * 3];
            // One pixel is pinned to the same background in every sample: its `c`/`s` would be a
            // guess, and the engine must say so instead of fitting it.
            values[..3].fill(64.0);
            samples.push(
                CalibrationSample::from_page(
                    &page,
                    page_index,
                    rect,
                    SampleBackground::Estimated { values },
                )
                .expect("sample builds"),
            );
        }

        let error = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
            .expect_err("one underdetermined pixel blocks the fit");
        match error.conditioning() {
            Some(ModelConditioning::Underdetermined {
                underdetermined_pixels,
                total_pixels,
                ..
            }) => {
                assert_eq!(*underdetermined_pixels, 1);
                assert_eq!(*total_pixels, pixels);
            }
            other => panic!("expected Underdetermined, got {other:?}"),
        }
    }

    #[test]
    fn conditioning_names_the_missing_background() {
        let assumed = AlphaUncertainty::from_percent(
            AlphaSource::Assumed,
            ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
        );
        let dark = ModelConditioning::DepositExact {
            levels: vec![12.0],
            spread: 1.0,
            samples: 3,
            alpha: assumed,
        };
        assert!(matches!(
            dark.suggested_background(),
            Some(SuggestedBackground::Brighter { .. })
        ));
        let bright = ModelConditioning::DepositExact {
            levels: vec![240.0],
            spread: 1.0,
            samples: 3,
            alpha: assumed,
        };
        match bright.suggested_background() {
            Some(SuggestedBackground::Darker { at_most }) => {
                assert!((at_most - (240.0 - MIN_BACKGROUND_SPREAD)).abs() < 0.01);
            }
            other => panic!("expected a request for a darker sample, got {other:?}"),
        }
        assert!(
            ModelConditioning::Separable {
                levels: vec![0.0, 255.0],
                spread: 255.0,
                min_pixel_spread: 255.0,
                alpha: AlphaUncertainty::from_flat_fit(255.0, 0.18),
            }
            .suggested_background()
            .is_none()
        );
    }

    /// The measured chapter-two case a template-keyed catalog gets wrong: a colour mark and a
    /// greyscale one sharing the same artwork. Here they literally share the template, so only the
    /// deposit can tell them apart — and it must, in both directions.
    #[test]
    fn identical_templates_with_different_deposits_stay_two_marks() {
        // Deposit chroma ~127 LSB on white; the measured colour mark reaches 120.
        let colour = SyntheticMark::with_colours(
            24,
            18,
            [0.50; 3],
            [255.0, 30.0, 0.0],
            [12.0, 14.0, 13.0],
        );
        // Achromatic twin depositing ~0.42 of the colour one — the measured bimodal opacity gain
        // (pale 0.365-0.515 against the colour model).
        let pale = SyntheticMark::with_colours(
            24,
            18,
            [0.34; 3],
            [128.0, 128.0, 128.0],
            [13.0, 13.0, 13.0],
        );

        let anchor = 40u32;
        let colour_rect = colour.rect_at(anchor, 20);
        let pale_rect = pale.rect_at(anchor, 80);
        let mut page = textured_page(120, 140, 150);
        colour.composite(&mut page, colour_rect, 1.0);
        pale.composite(&mut page, pale_rect, 1.0);

        // ONE template, cloned into both kinds: shape cannot possibly tell them apart.
        let template_page = {
            let mut clean = solid_page(120, 140, [150; 3]);
            colour.composite(&mut clean, colour_rect, 1.0);
            clean
        };
        let template = MarkTemplate::from_page(&template_page, colour_rect).expect("template");
        let mut colour_kind =
            WatermarkKind::new("test.colour", template.clone(), alpha_blend_operator());
        for sample in flat_samples(&colour, colour.rect_at(4, 4)) {
            colour_kind.add_sample(sample).expect("matching geometry");
        }
        colour_kind.refit().expect("separable");

        // Identity has to work at PICK time as well, before any fit: a flat sample measures the
        // deposit, and that is all the discriminator needs.
        let mut unfitted =
            WatermarkKind::new("test.unfitted", template.clone(), alpha_blend_operator());
        for sample in flat_samples(&pale, pale.rect_at(4, 4)) {
            unfitted.add_sample(sample).expect("matching geometry");
        }
        assert!(unfitted.model().is_none());
        let unfitted_signature = unfitted
            .signature()
            .expect("a flat sample is enough for an identity");
        assert!(unfitted_signature.is_achromatic());

        let mut pale_kind = WatermarkKind::new("test.pale", template, alpha_blend_operator());
        for sample in flat_samples(&pale, pale.rect_at(4, 4)) {
            pale_kind.add_sample(sample).expect("matching geometry");
        }
        pale_kind.refit().expect("separable");

        let colour_signature = colour_kind.signature().expect("a fitted kind has an identity");
        let pale_signature = pale_kind.signature().expect("a fitted kind has an identity");
        assert!(
            !colour_signature.is_achromatic(),
            "colour deposit chroma {} should exceed the achromatic limit",
            colour_signature.deposit_chroma
        );
        assert!(
            pale_signature.is_achromatic(),
            "greyscale deposit chroma {} should be under the achromatic limit",
            pale_signature.deposit_chroma
        );
        let gain = pale_signature
            .opacity_gain_against(&colour_signature)
            .expect("both deposits are measured against white");
        assert!(
            (0.30..0.60).contains(&gain),
            "expected the measured bimodal gain around 0.42, got {gain}"
        );
        assert!(!colour_kind.is_same_mark_as(&pale_kind));
        assert!(!pale_kind.is_same_mark_as(&colour_kind));
        assert!(colour_signature.is_same_mark_as(&colour_signature));
        // The sample-measured identity agrees with the fitted one, and still refuses the twin.
        assert!(unfitted_signature.is_same_mark_as(&pale_signature));
        assert!(!unfitted_signature.is_same_mark_as(&colour_signature));

        // A catalog lookup keyed on the signature keeps them apart; a shape-keyed one could not.
        let catalog = [colour_kind, pale_kind];
        assert_eq!(find_matching_kind(&catalog, &colour_signature), Some(0));
        assert_eq!(find_matching_kind(&catalog, &pale_signature), Some(1));

        // And each occurrence is attributed to its own kind, never removed with the other model.
        let hits = scan_page(&page, 0, &catalog, &DetectionParams::default()).expect("scan");
        assert_eq!(hits.len(), 2, "both marks must be found: {hits:?}");
        for hit in &hits {
            let expected_kind = if hit.occurrence.rect == colour_rect {
                0
            } else {
                assert_eq!(hit.occurrence.rect, pale_rect, "unexpected hit {hit:?}");
                1
            };
            assert_eq!(
                hit.kind_index, expected_kind,
                "occurrence matched to the wrong kind: {hit:?}"
            );
            assert!(hit.occurrence.is_removal_safe());
        }
    }

    /// The anchor set is DATA: chapter two stamps at three columns, chapter one at one. Detection
    /// must accept at every column of the set and refuse the same mark anywhere else.
    #[test]
    fn an_anchor_set_accepts_at_every_column_and_rejects_off_anchor() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let anchors = [20u32, 60, 100];
        let mut page = textured_page(140, 200, 150);
        for (index, &anchor) in anchors.iter().enumerate() {
            let y = 20 + 40 * u32::try_from(index).expect("three anchors");
            mark.composite(&mut page, mark.rect_at(anchor, y), 1.0);
        }
        // The same mark at full amplitude, far from every anchor column.
        let off_anchor = mark.rect_at(45, 160);
        mark.composite(&mut page, off_anchor, 1.0);

        let template_rect = mark.rect_at(anchors[0], 20);
        let template_page = {
            let mut clean = solid_page(140, 200, [150; 3]);
            mark.composite(&mut clean, template_rect, 1.0);
            clean
        };
        let mut kind = WatermarkKind::new(
            "test.anchor_set",
            MarkTemplate::from_page(&template_page, template_rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");
        assert_eq!(
            kind.template().anchors(),
            &[anchors[0]],
            "a picked sample knows only its own column"
        );
        kind.template_mut()
            .set_anchors(&anchors)
            .expect("non-empty set");
        assert_eq!(kind.template().anchor_key(), "20,60,100");

        let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
        assert_eq!(found.len(), 3, "every anchor column must be scanned: {found:?}");
        for (occurrence, &anchor) in found.iter().zip(anchors.iter()) {
            assert_eq!(occurrence.rect.x, anchor);
            assert!(occurrence.is_removal_safe());
        }
        assert!(
            found.iter().all(|hit| hit.rect != off_anchor),
            "an off-anchor occurrence must not be accepted: {found:?}"
        );
        assert!(matches!(
            kind.template_mut().set_anchors(&[]),
            Err(WatermarkError::EmptyAnchorSet)
        ));
    }

    /// The two guards chapter two measured: 9 of 147 candidates were false, all off-anchor and/or
    /// under a gain of 0.35, and removing at one injects an inverse mark into content.
    #[test]
    fn the_anchor_and_gain_guards_reject_what_the_looser_rule_accepted() {
        let mark = SyntheticMark::ring(24, 18, [0.18; 3]);
        let anchor = 50u32;
        let mut page = textured_page(140, 220, 150);
        let true_rect = mark.rect_at(anchor, 20);
        mark.composite(&mut page, true_rect, 1.0);
        // 3 px off the anchor column: inside the old +-3 band, outside the measured +-2 one.
        let off_anchor = mark.rect_at(anchor + 3, 80);
        mark.composite(&mut page, off_anchor, 1.0);
        // On the anchor at a fifth of the amplitude: a gain of ~0.2, under the measured floor.
        let weak = mark.rect_at(anchor, 140);
        mark.composite(&mut page, weak, 0.2);

        let template_page = {
            let mut clean = solid_page(140, 220, [150; 3]);
            mark.composite(&mut clean, true_rect, 1.0);
            clean
        };
        let mut kind = WatermarkKind::new(
            "test.guards",
            MarkTemplate::from_page(&template_page, true_rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");

        let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
        assert_eq!(found.len(), 1, "only the true occurrence survives: {found:?}");
        assert_eq!(found[0].rect, true_rect);

        // The looser band the engine used before this measurement accepted the off-anchor one.
        let loose = DetectionParams {
            anchor_tolerance: 3,
            ..DetectionParams::default()
        };
        let loose_found = find_occurrences(&page, &kind, &loose).expect("scan");
        assert!(
            loose_found.iter().any(|hit| hit.rect == off_anchor),
            "the +-3 px band is what let the measured false accepts through: {loose_found:?}"
        );

        // No configuration may lower the gain floor under the measured 0.35, so the fifth-
        // amplitude look-alike stays rejected even with a hand-widened window.
        let wide = DetectionParams {
            gain_min: 0.05,
            gain_max: 1.5,
            anchor_tolerance: 3,
            ..DetectionParams::default()
        };
        assert!((wide.normalized().gain_min - FALSE_ACCEPT_GAIN_FLOOR).abs() < f32::EPSILON);
        let wide_found = find_occurrences(&page, &kind, &wide).expect("scan");
        assert!(
            wide_found.iter().all(|hit| hit.rect != weak),
            "a gain of ~0.2 must never be accepted: {wide_found:?}"
        );
    }

    /// The anchor set has to come from the data — nobody knows the other columns from the one
    /// occurrence the user pointed at.
    #[test]
    fn anchor_discovery_finds_every_stamped_column() {
        let mark = SyntheticMark::new(24, 18, [0.18; 3]);
        let columns = [12u32, 60, 104];
        let mut page = textured_page(140, 260, 150);
        let mut stamped = Vec::new();
        for (index, &column) in columns.iter().enumerate() {
            let offset = 32 * u32::try_from(index).expect("three columns");
            for base in [20u32, 160] {
                let rect = mark.rect_at(column, base + offset);
                mark.composite(&mut page, rect, 1.0);
                stamped.push(rect);
            }
        }

        let template_rect = mark.rect_at(columns[0], 20);
        let template_page = {
            let mut clean = solid_page(140, 260, [150; 3]);
            mark.composite(&mut clean, template_rect, 1.0);
            clean
        };
        let mut kind = WatermarkKind::new(
            "test.discovery",
            MarkTemplate::from_page(&template_page, template_rect).expect("template"),
            alpha_blend_operator(),
        );
        for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");

        let params = DetectionParams::default();
        let discovered = discover_anchors(&[&page], kind.template(), &params).expect("discovery");
        for &column in &columns {
            assert!(
                discovered
                    .iter()
                    .any(|&found| found.abs_diff(column) <= ANCHOR_CLUSTER_RADIUS_PX),
                "column {column} was stamped twice and must be discovered: {discovered:?}"
            );
        }

        // Installing the discovered set finds every occurrence, and the gain test is what keeps a
        // spurious column from turning into a spurious removal.
        kind.template_mut()
            .set_anchors(&discovered)
            .expect("non-empty set");
        let found = find_occurrences(&page, &kind, &params).expect("scan");
        assert_eq!(found.len(), stamped.len(), "{found:?}");
        for rect in stamped {
            assert!(found.iter().any(|hit| hit.rect == rect), "missing {rect:?}");
        }
    }

    /// The alpha-uncertainty figures are the MEASURED calibration, not a re-derivation: +-10% on
    /// the alpha scale costs 3.2 LSB rms overall and 4.7 LSB (worst 13) on backgrounds under
    /// luma 80. A change to the constants must show up here.
    #[test]
    fn reported_uncertainty_matches_the_measured_alpha_calibration() {
        let ten = AlphaUncertainty::from_percent(AlphaSource::Assumed, 10.0);
        assert!((ten.rms_lsb - 3.2).abs() < 0.05, "{ten:?}");
        assert!((ten.dark_rms_lsb - 4.7).abs() < 0.05, "{ten:?}");
        assert!((ten.dark_max_lsb - 13.0).abs() < 0.05, "{ten:?}");
        assert!((ten.dark_luma - DARK_BACKGROUND_LUMA).abs() < f32::EPSILON);

        // Linear in the alpha error, as `delta_alpha * (B - B0) / (1 - alpha)` requires.
        let five = AlphaUncertainty::from_percent(AlphaSource::Assumed, 5.0);
        let twenty = AlphaUncertainty::from_percent(AlphaSource::Assumed, 20.0);
        assert!((five.rms_lsb - 1.6).abs() < 0.05, "{five:?}");
        assert!((twenty.rms_lsb - 6.4).abs() < 0.05, "{twenty:?}");

        // A fitted slope over well-separated exact backgrounds is worth far more than an
        // assumption, and the measured 2.3-2.6 LSB leave-one-out residual is what it predicts.
        let fitted = AlphaUncertainty::from_flat_fit(255.0, 0.183);
        assert_eq!(fitted.source, AlphaSource::SeparatedBackgrounds);
        assert!(
            fitted.percent < ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
            "a measured slope must beat an assumed one: {fitted:?}"
        );
        assert!(
            (2.0..3.0).contains(&fitted.rms_lsb),
            "expected the measured ~2.5 LSB floor, got {fitted:?}"
        );
        // A degenerate spread cannot be quoted as certain.
        assert!((AlphaUncertainty::from_flat_fit(0.0, 0.18).percent - 100.0).abs() < 0.01);
    }
}
