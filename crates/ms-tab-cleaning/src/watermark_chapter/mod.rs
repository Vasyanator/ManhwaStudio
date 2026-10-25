/*
File: watermark_chapter/mod.rs

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
      -> estimate_model          -> least squares over separated known-level samples, Theil-Sen
                                    against estimated backgrounds, or the deposit-exact graded fit
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
- `RingCoverage`: how much of a measurement ring existed. The ring is CLIPPED to the page, so a
  mark stamped against the image border is measured from the sides that exist and admitted on a
  PIXEL COUNT (`SampleParams::min_ring_pixels`), never on a per-side margin. The flatness test
  is unchanged by that — a structured ring is still refused — and the coverage travels with the
  level and is counted in `ModelProvenance::partial_rings`, so no report can call a one-sided
  measurement complete. No numeric penalty rides on it: nothing measured relates a ring's pixel
  count to the alpha uncertainty, and inventing a figure is what the plan forbids.
- `SampleBackground`: what is known about `B` under one sample — MEASURED from a flat ring
  (with its coverage), ASSERTED by the user (`Manual`, carrying no `ring_std` because nothing
  was measured), or estimated per pixel. An asserted level enters the fit exactly like a measured one; what it
  does NOT do is claim to be evidence, so every model it feeds is downgraded to
  `AlphaSource::ManualBackgrounds` and counted in `ModelProvenance::manual_backgrounds`. That
  count is the named exception to "the engine never emits a model it cannot justify": the
  arithmetic is still justified, the background under it is the user's claim, and the
  provenance says so in types rather than in prose.
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
- `trimmed_footprint_from_model()`, `trimmed_footprint_from_template()`,
  `mark_extent_from_template()`

Footprint contract:
A mark's FOOTPRINT is the rectangle the mark actually deposits in, plus `FOOTPRINT_TRIM_SAFETY_PX`
of background — never the loose box a detector happened to hand over. Outside the deposit the model
is `c = 0`, `s = 1`: fully transparent, no information, no removable signal, yet every such pixel is
demanded of every future reference crop and stored twice in the `c`/`s` planes. The two
`trimmed_footprint_*` functions MEASURE that rectangle (from the fitted model where there is one,
otherwise from the template against its own border level); `mark_extent_from_template` is the same
template measurement with NO safety border, which is what says a crop cut the mark and what a
reference intake derives a joint footprint from. All of them err LARGE by construction — a
threshold at the 8-bit quantization floor, so a background that is not flat yields no trim at all.

Files:
This root holds the module header, the shared tuning constants, the error types, `PixelRect` with
the numeric / validation helpers and EVERY type declaration of the engine; behaviour lives in
the children (see `MODULE_README.md`): `template.rs` (operator, template, footprint),
`samples.rs`, `model.rs`, `fit.rs`, `kinds.rs`, `detection.rs`, `removal.rs`, `tests.rs`.

Notes:
Everything here is GUI-free and `Send`: no egui, no backend, no user-visible strings. Error
`Display` text is developer diagnostics; the tool layer maps variants to i18n keys. Job
orchestration, the region editor, `CanvasView` patches and persistence belong to
`tools/watermark_removal/` and are deliberately NOT this module's business.

Entry invariant (established once, in `validate_page`): page width, height and `w*h*4` all fit
`usize`, and both dimensions fit `u32` (the `image` crate guarantees the latter). Every rect
handled below is validated to lie inside the page, so all downstream flat indexing and the few
documented numeric casts are provably in range.
*/

use std::sync::Arc;

use image::RgbaImage;

mod detection;
mod fit;
mod kinds;
mod model;
mod removal;
mod samples;
mod template;

pub(super) use detection::{discover_anchors, find_occurrences, scan_page};
pub(super) use kinds::find_matching_kind;
pub(super) use removal::remove_occurrences_on_page;
pub(super) use samples::{calibration_sample_from_page, validate_calibration_sample};
pub(super) use template::{
    alpha_blend_operator, mark_extent_from_template, operator_from_id, trimmed_footprint_from_model,
    trimmed_footprint_from_template,
};
#[cfg(test)]
pub(super) use fit::estimate_model;
#[cfg(test)]
pub(super) use removal::remove_occurrence;

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
/// Floor on the alpha-scale uncertainty of any fit that rests on a HAND-ASSERTED background
/// level ([`SampleBackground::Manual`]), percent of alpha.
///
/// An asserted level carries no measurement, so the error in `B` is bounded by nothing the
/// engine can see, and the two-point slope error `sigma*sqrt(2)/spread` — which assumes `B` is
/// exact — stops describing it. The engine therefore refuses to quote a figure BETTER than the
/// one it quotes when it has no slope information at all, which is
/// `ASSUMED_ALPHA_UNCERTAINTY_PERCENT`. This is the same rule
/// [`AlphaUncertainty::from_percent`] applies to a non-finite percentage: an uncertainty nothing
/// quantifies is never a small one. It is a FLOOR, not a replacement — a fit that is already
/// worse than this keeps its own worse figure.
const MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT: f32 = ASSUMED_ALPHA_UNCERTAINTY_PERCENT;
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

/// A zero-mean patch plus its L2 norm: everything [`ncc_patch`] needs to correlate against, so
/// the template and its downscaled copy share one kernel.
#[derive(Debug, Clone, Copy)]
struct CorrelationRef<'a> {
    width: usize,
    height: usize,
    centered: &'a [f32],
    norm: f32,
}

/// Rec.601 luma of one RGB triple.
#[inline]
fn luma_of(r: u8, g: u8, b: u8) -> f32 {
    LUMA_R * f32::from(r) + LUMA_G * f32::from(g) + LUMA_B * f32::from(b)
}

// ---------------------------------------------------------------------------------------
// Footprint trim
// ---------------------------------------------------------------------------------------

/// Smallest per-pixel opacity a footprint pixel must reach before it counts as PART OF THE MARK,
/// when the mark is known through a fitted model.
///
/// One LSB of an 8-bit output byte against the widest possible background swing: a pixel whose
/// alpha is below `1/255` cannot move a single rendered byte at ANY background level, so it
/// carries no removable deposit whatever the page under it. Deliberately far below
/// [`ALPHA_SIGNIFICANT`], which answers a different question ("is this deposit worth
/// reporting"): a trim must keep every pixel that could still matter, because an over-tight
/// footprint corrupts removal while a loose one only costs crop size.
const FOOTPRINT_TRIM_MIN_ALPHA: f32 = 1.0 / 255.0;

/// Smallest deviation from the surrounding background level a TEMPLATE pixel must show before it
/// counts as part of the mark, LSB.
///
/// One LSB is the smallest difference an 8-bit crop can record at all, so nothing below it is
/// evidence of a deposit. Holding the threshold at the quantization floor — rather than at the
/// template's own noise level — is what makes the rule SELF-LIMITING: on a template whose
/// background is not in fact flat, every pixel deviates, the extent is the whole footprint and no
/// trim happens. A noisy template therefore costs a missed trim, never a cut mark.
const FOOTPRINT_TRIM_MIN_DEVIATION_LSB: f32 = 1.0;

/// Background kept around the mark's measured extent when a footprint is trimmed, pixels.
///
/// Three things have to fit inside it. It EXCEEDS [`RING_WIDTH_PX`], so a trimmed footprint still
/// presents a full ring of background to the level estimator — which is what makes a second trim
/// of the same entry a no-op instead of a slow erosion. It covers [`MAX_SUBPIXEL_SHIFT`], so an
/// occurrence that landed off the pixel grid still has its whole deposit inside the footprint.
/// And it absorbs the one-pixel uncertainty of the extent itself, where an antialiased edge fades
/// into the quantization floor.
/// Also the slack the REFERENCE INTAKE carries around a jointly measured mark extent, and the
/// largest disagreement it tolerates between where the aligner puts a crop's window and where
/// that crop's own extent says the mark is — see `tools/watermark_entry.rs`.
pub(super) const FOOTPRINT_TRIM_SAFETY_PX: u32 = 4;

// ---------------------------------------------------------------------------------------
// Calibration samples
// ---------------------------------------------------------------------------------------

/// What the engine knows about the background under one calibration sample.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SampleBackground {
    /// The ring around the mark is uniform, so `B` is known exactly and is the same everywhere
    /// under the footprint. This is the strongest input the estimator can receive.
    ///
    /// `ring` says how much of that ring existed. A mark against a page border is measured from
    /// the sides that survive clipping, which is still a measurement — the level and `ring_std`
    /// mean exactly what they always meant — but it rests on fewer, one-sided pixels, so the
    /// coverage is carried with the level and counted out of a fit in
    /// [`ModelProvenance::partial_rings`] rather than dropped at the boundary.
    Flat {
        level: [f32; 3],
        ring_std: [f32; 3],
        ring: RingCoverage,
    },
    /// `B` was ASSERTED by the user for a sample whose ring the engine refused to call flat.
    ///
    /// It enters the fit exactly like [`SampleBackground::Flat`] — the user is telling the
    /// engine what the background under this occurrence was — but it is NOT a measurement, so
    /// it carries no `ring_std` (nothing was measured, and a fabricated `0.0` would claim a
    /// measurement that never happened) and every model it contributes to is downgraded:
    /// [`AlphaSource::ManualBackgrounds`] with its uncertainty floored at
    /// `MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT`. Nothing in the automatic path ever
    /// produces this variant; it exists only for a level a human states.
    Manual { level: [f32; 3] },
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

/// Why a picked rect cannot serve as anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SampleRejection {
    /// The rect is empty or leaves the page.
    BadRect,
    /// Too few ring pixels survived clipping at the page edge to judge flatness.
    RingTooSmall { pixels: usize, needed: usize },
}

/// How much of the ring around a calibration rect actually existed.
///
/// The ring is CLIPPED to the page, so a mark stamped against an image border is measured from
/// the sides that exist rather than refused: fewer samples of a flat background are less data,
/// not invalid data, and the admission floor is a PIXEL COUNT (`SampleParams::min_ring_pixels`),
/// never a per-side margin. What changes is the precision behind the two numbers the ring
/// produces — the level is the mean of `pixels` samples instead of `full_pixels`, and the
/// flatness statistic is computed over that smaller, one-sided set — so the coverage travels
/// with every measurement that rests on it and is counted out of a fit in
/// [`ModelProvenance::partial_rings`].
///
/// One thing it deliberately does NOT do is carry a numeric penalty. The engine's alpha
/// uncertainty is calibrated on `FIT_NOISE_LSB`, which `AlphaUncertainty::from_flat_fit`
/// documents as a per-occurrence rasterization BIAS that does not average down with sample
/// count; nothing in `dev-docs/watermark_chapter_decomposition_plan.md` measures how a ring's
/// pixel count moves it. Attaching an invented figure would be exactly the fabricated number
/// that document's "honest reporting" section forbids, so the coverage is REPORTED and the
/// uncertainty is left as the data licenses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RingCoverage {
    /// Ring pixels the measurement actually averaged.
    pub pixels: usize,
    /// Ring pixels the same ring would have had with nothing clipping it.
    pub full_pixels: usize,
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
        /// How much of the ring existed. Partial is accepted (see [`RingCoverage`]) and must
        /// be carried, not dropped.
        ring: RingCoverage,
    },
    /// Ring not uniform: REFUSED as calibration, usable as a detection template.
    ///
    /// This is a DIFFERENT failure from a ring too small to judge at all
    /// ([`SampleRejection::RingTooSmall`]) and from a ring that is merely partial: partialness
    /// costs pixels, non-flatness costs the measurement itself, and the two never substitute
    /// for each other.
    TemplateOnly {
        level: [f32; 3],
        ring_std: [f32; 3],
        ring_max_dev: [f32; 3],
        std_limit: f32,
        max_dev_limit: f32,
        ring: RingCoverage,
    },
    /// Unusable for anything.
    Unusable { reason: SampleRejection },
}

/// Tunables of the ring measurement. Defaults are the named constants above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SampleParams {
    pub ring_width: u32,
    pub min_ring_pixels: usize,
    pub std_limit: f32,
    pub max_dev_limit: f32,
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
    /// Fitted with at least one background level ASSERTED by the user
    /// ([`SampleBackground::Manual`]) rather than measured. The fit is arithmetically the same
    /// one a measurement would have produced, but its `B` is a claim, so the uncertainty is
    /// floored at `MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT` and nothing downstream
    /// may describe such a model as measured.
    ManualBackgrounds,
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
    /// How many of the samples this fit actually ran on carried a HAND-ASSERTED background
    /// level ([`SampleBackground::Manual`]) instead of a measured one.
    ///
    /// Non-zero means the model rests on a claim the engine could not check: it is still a
    /// model, and removal through it is still the arithmetic the data licenses, but no report
    /// built from this provenance may say the imprint was measured. The cost is already priced
    /// into the verdict's [`AlphaUncertainty`]; this count is what names the assumption.
    pub manual_backgrounds: usize,
    /// How many of the samples this fit ran on had their background MEASURED from a ring the
    /// page edge truncated ([`RingCoverage::is_partial`]).
    ///
    /// Unlike `manual_backgrounds` this is not an assumption: every one of those levels was
    /// measured, and the flatness test they passed was the ordinary one. What it names is
    /// weaker EVIDENCE — a mean over fewer, one-sided background pixels — so a report built
    /// from this provenance may call the model measured and must not call the measurement
    /// complete. No numeric penalty rides on it; see [`RingCoverage`] for why inventing one
    /// would be dishonest.
    pub partial_rings: usize,
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

/// Sub-pixel offset of an observed occurrence relative to its integer rect origin.
///
/// Convention: the observed mark equals the model evaluated at `(x - dx, y - dy)`. Removal
/// therefore samples the model shifted by this amount; detection reports the value it measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SubpixelShift {
    pub dx: f32,
    pub dy: f32,
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

/// One accepted occurrence attributed to a kind of the catalog.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChapterHit {
    pub page_index: usize,
    pub kind_index: usize,
    pub occurrence: Occurrence,
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

/// The recovered pixels of one occurrence, ready to be handed to the overlay layer.
#[derive(Debug, Clone)]
pub(super) struct RegionPatch {
    pub rect: PixelRect,
    /// RGBA8, `width*height*4` entries. Alpha is copied from the source page.
    pub pixels: Vec<u8>,
    pub residual: RemovalResidual,
}

#[cfg(test)]
mod tests;
