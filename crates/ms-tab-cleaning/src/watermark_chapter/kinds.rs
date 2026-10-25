/*
File: watermark_chapter/kinds.rs

Purpose:
Mark kinds and detections: `Occurrence` scoring, `DetectionParams`, `WatermarkKind` (samples,
refit, identity) and the kind matching / background-refined refit across a chapter.

Key items:
- `impl Occurrence`, `impl DetectionParams`, `impl WatermarkKind`
- `find_matching_kind()`, `refit_with_refined_backgrounds()`

Notes:
The types are declared in `mod.rs`.
*/
use std::sync::Arc;
use image::RgbaImage;
use super::{
    ANCHOR_TOLERANCE_PX, AcceptanceEvidence, AlphaAssumption, BACKGROUND_BLUR_RADIUS_PX,
    BACKGROUND_REFINEMENT_ITERATIONS, COARSE_NCC_MIN, CalibrationSample, CompositingOperator,
    DetectionParams, FALSE_ACCEPT_GAIN_FLOOR, GAIN_FITNESS_SCALE, GAIN_MAX, GAIN_MIN,
    MAX_ANCHOR_TOLERANCE_PX, MAX_CANDIDATES_PER_PAGE, MIN_DETECTION_T, MarkSignature, MarkTemplate,
    ModelConditioning, ModelFitError, NCC_ONLY_MIN, OVERLAP_IOU_LIMIT, Occurrence, SampleBackground,
    WatermarkError, WatermarkKind, WatermarkModel,
};
use super::detection::provisional_background;
use super::fit::estimate_model;


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

    pub(super) fn evidence_rank(&self) -> u8 {
        match self.evidence {
            AcceptanceEvidence::Correlation => 0,
            AcceptanceEvidence::Gain { .. } => 1,
        }
    }
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
pub(crate) fn find_matching_kind(
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
/// every sample that HAS an estimated background and refits. Samples carrying a single known `B`
/// — measured ([`SampleBackground::Flat`]) or asserted ([`SampleBackground::Manual`]) — are left
/// alone: downgrading a measurement to an estimate would throw away the only hard evidence in
/// the set, and overwriting an asserted level would discard the user's own instruction.
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
pub(crate) fn refit_with_refined_backgrounds(
    pages: &[&RgbaImage],
    kind: &mut WatermarkKind,
    params: &DetectionParams,
) -> Result<(), ModelFitError> {
    if kind.model.is_none() {
        return Err(ModelFitError::Invalid(WatermarkError::NoSamples));
    }
    // Anything with a single known `B` is left alone: downgrading a measurement to an estimate
    // would throw away the only hard evidence in the set, and downgrading a level the USER
    // asserted would silently overwrite an instruction with a guess.
    let refinable: Vec<usize> = (0..kind.samples.len())
        .filter(|&index| !kind.samples[index].has_known_level())
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
