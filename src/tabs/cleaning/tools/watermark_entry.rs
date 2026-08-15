/*
FILE HEADER (cleaning/tools/watermark_entry.rs)

Purpose:
The bridge between the GUI-free decomposition engine (`../watermark_chapter.rs`) and the
on-disk library (`watermark_library.rs`). It owns three things neither of those may own:

1. the MAPPING between a fitted `WatermarkKind` and the library's plain-data records —
   the literal wire tags of a verdict, a fit method and an alpha source, and their inverse;
2. REFERENCE-CROP INTAKE: turning the same mark supplied on two or more known uniform
   backgrounds, as separate image files, into a library entry with a closed-form model;
3. AUTO-MATCH RANKING: deciding which library entries carry the mark a chapter just
   measured, and in which order.

Main responsibilities:
- keep the engine free of persistence (it never sees a `Stored*` type) and the library free
  of the engine (it never sees a `WatermarkKind`);
- refuse a reference-crop set the maths cannot use, naming the measured reason: a crop whose
  background is not uniform, crops that could not be aligned to one another, and — the
  important one — crops whose backgrounds do not span enough levels to separate `alpha`
  from `W`. Two crops on the SAME background are refused rather than silently accepted,
  because the resulting model would be confidently wrong.

Key structures:
- `ReferenceIntakeRequest` / `ReferenceIntakeOutcome` / `ReferenceCropReport`
- `LibraryCandidate`

Key functions:
- `run_reference_intake()`, `rank_library_candidates()`, `stored_calibration()`,
  `conditioning_from_stored()`, `luma_of_level()`.

Notes:
- The ideal reference pair is white + black: the closed form is then `c = I|B=0` and
  `s = (I|B=255 - I|B=0)/255`, and the slope error is `~sigma*sqrt(2)/spread`. Any uniform
  colour works with proportionally larger error, and a coloured background additionally
  yields per-channel levels. The strongest real-world route to such a pair: some sources
  stamp their mark onto an UPLOADED image, so feeding the source a white rectangle and a
  black one returns the mark with no page content behind it at all.
- Alignment correlates GRADIENT MAGNITUDE, not raw pixels. Raw luma flips sign between a
  white-background crop (`I = 255 - alpha*(255 - W)`) and a black-background one
  (`I = alpha*W`), so a plain NCC of the two can be strongly NEGATIVE for a perfect match;
  gradient magnitude is non-negative under both and peaks at the mark's edges either way.
- Whether the supplied backgrounds separate the model is decided by the ENGINE's own fit,
  never by a threshold copied here: the intake builds the kind, refits, and accepts only
  `ModelConditioning::Separable`. The refusal message is then built from the verdict's own
  levels, spread and `suggested_background()`.
*/
use std::path::{Path, PathBuf};

use image::RgbaImage;
use rayon::prelude::*;

use super::watermark_library::{
    EntrySummary, LibraryPlanes, LibrarySample, LoadedEntry, SaveEntryRequest, StoredAlpha,
    StoredAlphaAssumption, StoredCalibration, StoredSampleBackground, StoredSampleOrigin,
    StoredSignature,
};
use crate::tabs::cleaning::watermark_chapter::{
    AlphaAssumption, AlphaSource, AlphaUncertainty, CalibrationSample, FitMethod, MarkSignature,
    MarkTemplate, ModelConditioning, ModelFitError, PixelRect, SampleBackground, SampleParams,
    SampleRejection, SampleVerdict, SuggestedBackground, WatermarkKind, alpha_blend_operator,
    validate_calibration_sample,
};

/// Rec.601 luma weights, the same plane the engine correlates on.
const LUMA_R: f32 = 0.299;
const LUMA_G: f32 = 0.587;
const LUMA_B: f32 = 0.114;

/// Half-width of the integer alignment search between reference crops, pixels.
///
/// A user cropping the same mark twice by hand lands within a few pixels; 24 covers that
/// with room to spare while keeping the search a fraction of a second on a mark of the
/// measured size. It is NOT a subpixel search: the engine has its own subpixel shift, and
/// the closed form only needs the crops to agree on the integer grid.
const REFERENCE_ALIGN_SEARCH_PX: i32 = 24;
/// Gradient-magnitude NCC below which two crops are not accepted as the same mark.
///
/// The two crops carry different backgrounds, so their gradient maps are similar but never
/// identical (a white-background crop shows the dark outline strongly, a black-background
/// one shows the pale fill). The floor is therefore deliberately loose: it is here to catch
/// "these are two different pictures", not to grade the alignment.
const REFERENCE_MIN_ALIGN_NCC: f32 = 0.30;
/// Extra margin, in ring widths, kept around the mark footprint inside a reference crop.
///
/// The background level is measured from the ring AROUND the footprint, so the crop must
/// carry at least a full ring of flat background outside the mark. One pixel beyond the
/// ring keeps a border artifact of the source's own resampling out of the measurement.
const REFERENCE_RING_MARGIN_SLACK_PX: u32 = 1;

// ---------------------------------------------------------------------------------------
// Engine <-> library mapping
// ---------------------------------------------------------------------------------------

/// Rec.601 luma of a measured background level.
#[must_use]
pub(super) fn luma_of_level(level: [f32; 3]) -> f32 {
    LUMA_R * level[0] + LUMA_G * level[1] + LUMA_B * level[2]
}

/// Literal wire tag of a fit method, for the library metadata.
#[must_use]
pub(super) fn fit_method_wire(method: FitMethod) -> &'static str {
    match method {
        FitMethod::ClosedFormFlat => "closed_form_flat",
        FitMethod::TheilSen => "theil_sen",
        FitMethod::DepositExact => "deposit_exact",
    }
}

/// Literal wire tag of an alpha source, for the library metadata.
#[must_use]
pub(super) fn alpha_source_wire(source: AlphaSource) -> &'static str {
    match source {
        AlphaSource::SeparatedBackgrounds => "separated_backgrounds",
        AlphaSource::EstimatedBackgrounds => "estimated_backgrounds",
        AlphaSource::Assumed => "assumed",
    }
}

/// Inverse of [`alpha_source_wire`]. An unknown tag is read as `Assumed`, the weakest of
/// the three: a claim this build cannot verify must never be upgraded into a stronger one.
#[must_use]
fn alpha_source_from_wire(value: &str) -> AlphaSource {
    match value {
        "separated_backgrounds" => AlphaSource::SeparatedBackgrounds,
        "estimated_backgrounds" => AlphaSource::EstimatedBackgrounds,
        _ => AlphaSource::Assumed,
    }
}

/// Literal wire tag of a conditioning verdict, for the library metadata.
#[must_use]
pub(super) fn conditioning_wire(conditioning: &ModelConditioning) -> &'static str {
    match conditioning {
        ModelConditioning::Separable { .. } => "separable",
        ModelConditioning::DepositExact { .. } => "deposit_exact",
        ModelConditioning::NotEnoughSamples { .. } => "not_enough_samples",
        ModelConditioning::DepositUnavailable { .. } => "deposit_unavailable",
        ModelConditioning::Underdetermined { .. } => "underdetermined",
    }
}

/// Builds the library record of what a kind was calibrated on — the half of an entry that
/// tells a later user whether it is the exact case or the graded one.
#[must_use]
pub(super) fn stored_calibration(kind: &WatermarkKind) -> StoredCalibration {
    let conditioning = kind.conditioning();
    let samples = match conditioning {
        ModelConditioning::DepositExact { samples, .. }
        | ModelConditioning::DepositUnavailable { samples, .. } => *samples,
        ModelConditioning::NotEnoughSamples { have, .. } => *have,
        ModelConditioning::Separable { .. } | ModelConditioning::Underdetermined { .. } => {
            kind.samples().len()
        }
    };
    StoredCalibration {
        verdict: conditioning_wire(conditioning).to_string(),
        levels: conditioning.levels().to_vec(),
        spread: conditioning.spread(),
        samples,
        fit_method: kind
            .model()
            .map(|model| fit_method_wire(model.provenance().method).to_string()),
        clamped_pixels: kind
            .model()
            .map_or(0, |model| model.provenance().clamped_pixels),
        alpha: conditioning.alpha_uncertainty().map(|alpha| StoredAlpha {
            source: alpha_source_wire(alpha.source).to_string(),
            percent: alpha.percent,
            rms_lsb: alpha.rms_lsb,
            dark_rms_lsb: alpha.dark_rms_lsb,
            dark_max_lsb: alpha.dark_max_lsb,
            dark_luma: alpha.dark_luma,
        }),
    }
}

/// Rebuilds the engine's graded verdict from a stored entry's calibration record, so the
/// library window can describe a stored entry with exactly the same words — and the same
/// `suggested_background()` — as a freshly measured one.
///
/// `None` for a verdict tag this build does not know: a stored entry from a newer writer is
/// described by its own literal tag rather than mapped onto the closest known variant, which
/// would misreport its quality.
#[must_use]
pub(super) fn conditioning_from_stored(
    calibration: &StoredCalibration,
) -> Option<ModelConditioning> {
    let alpha = calibration.alpha.as_ref().map(|alpha| {
        // Only `(source, percent)` are load-bearing: every LSB figure is derived from the
        // percentage by the engine's own measured constants, so recomputing them keeps a
        // stored entry's report in step with the engine instead of frozen at write time.
        AlphaUncertainty::from_percent(alpha_source_from_wire(&alpha.source), alpha.percent)
    });
    match calibration.verdict.as_str() {
        "separable" => Some(ModelConditioning::Separable {
            levels: calibration.levels.clone(),
            spread: calibration.spread,
            min_pixel_spread: calibration.spread,
            alpha: alpha?,
        }),
        "deposit_exact" => Some(ModelConditioning::DepositExact {
            levels: calibration.levels.clone(),
            spread: calibration.spread,
            samples: calibration.samples,
            alpha: alpha?,
        }),
        "not_enough_samples" => Some(ModelConditioning::NotEnoughSamples {
            have: calibration.samples,
            need: 2,
        }),
        "deposit_unavailable" => Some(ModelConditioning::DepositUnavailable {
            samples: calibration.samples,
            spread: calibration.spread,
        }),
        _ => None,
    }
}

/// The engine's alpha assumption, as stored.
#[must_use]
pub(super) fn alpha_assumption_from_stored(stored: StoredAlphaAssumption) -> AlphaAssumption {
    match stored {
        StoredAlphaAssumption::FromDeposit => AlphaAssumption::FromDeposit,
        StoredAlphaAssumption::Stated {
            peak_alpha,
            uncertainty_percent,
        } => AlphaAssumption::Stated {
            peak_alpha,
            uncertainty_percent,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Auto-match ranking
// ---------------------------------------------------------------------------------------

/// One library entry offered as the calibration of a mark measured in the open chapter.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LibraryCandidate {
    /// Persisted literal identity of the entry.
    pub entry_id: String,
    /// The entry's display name, verbatim.
    pub name: String,
    /// Literal verdict tag of the entry's calibration.
    pub verdict: String,
    /// Widest gap between the background levels it was calibrated on, LSB.
    pub spread: f32,
    pub samples: usize,
    /// Opacity gain of the entry's deposit measured against the chapter mark's own —
    /// the evidence of the match, and the number that separates a colour mark from its
    /// greyscale twin.
    pub gain: f32,
}

/// Quality rank of a stored verdict: higher is a stronger calibration.
///
/// Only two tags produce a model at all, and only one of them measured the slope, so the
/// ordering has exactly three steps and no ties to break inside them.
fn verdict_rank(verdict: &str) -> u8 {
    match verdict {
        "separable" => 2,
        "deposit_exact" => 1,
        _ => 0,
    }
}

/// Ranks the library entries that carry the same mark as `signature`, best first.
///
/// Matching is deliberately SHAPE-INDEPENDENT (`MarkSignature::is_same_mark_as`): two marks
/// can share their artwork pixel for pixel and still need different `c`/`s`, so identity is
/// the measured deposit chroma plus the opacity gain, never the template picture. On top of
/// that identity test the footprint must agree — a model of a different footprint cannot be
/// substituted for this mark's, whatever its signature says.
///
/// Ambiguity — several entries answering the identity test — is resolved in this order:
/// stronger calibration first (a separable entry beats a graded one), then the entry whose
/// opacity gain sits closest to 1 (the closest deposit match), then more calibration
/// samples, then the most recently updated entry, and finally the literal id, so the result
/// is deterministic rather than dependent on directory order.
#[must_use]
pub(super) fn rank_library_candidates(
    entries: &[EntrySummary],
    signature: &MarkSignature,
    footprint: (u32, u32),
) -> Vec<LibraryCandidate> {
    let mut ranked: Vec<(LibraryCandidate, u64)> = entries
        .iter()
        .filter(|entry| (entry.width, entry.height) == footprint)
        .filter_map(|entry| {
            let stored = entry.signature?;
            let known = MarkSignature {
                reference_level: stored.reference_level,
                deposit_chroma: stored.deposit_chroma,
                mean_deposit: stored.mean_deposit,
                peak_alpha: stored.peak_alpha,
            };
            if !known.is_same_mark_as(signature) {
                return None;
            }
            let gain = known.opacity_gain_against(signature)?;
            Some((
                LibraryCandidate {
                    entry_id: entry.id.clone(),
                    name: entry.name.clone(),
                    verdict: entry.verdict.clone(),
                    spread: entry.spread,
                    samples: entry.samples,
                    gain,
                },
                entry.updated_unix,
            ))
        })
        .collect();
    ranked.sort_by(|(left, left_updated), (right, right_updated)| {
        verdict_rank(&right.verdict)
            .cmp(&verdict_rank(&left.verdict))
            .then_with(|| {
                (left.gain - 1.0)
                    .abs()
                    .total_cmp(&(right.gain - 1.0).abs())
            })
            .then_with(|| right.samples.cmp(&left.samples))
            .then_with(|| right_updated.cmp(left_updated))
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
    ranked.into_iter().map(|(candidate, _)| candidate).collect()
}

/// True when adopting `candidate` would strengthen a mark whose own fit reached
/// `current`.
///
/// Adopting is not free: it replaces the chapter's own measurements with the library's, so
/// it must only happen when the library's calibration is genuinely stronger — a separable
/// entry over a graded chapter fit, or a wider background spread within the same verdict.
/// A chapter that already separated its own model keeps it.
#[must_use]
pub(super) fn candidate_improves(candidate: &LibraryCandidate, current: &ModelConditioning) -> bool {
    let current_rank = verdict_rank(conditioning_wire(current));
    let candidate_rank = verdict_rank(&candidate.verdict);
    if candidate_rank != current_rank {
        return candidate_rank > current_rank;
    }
    candidate_rank > 0 && candidate.spread > current.spread()
}

// ---------------------------------------------------------------------------------------
// Reference-crop intake
// ---------------------------------------------------------------------------------------

/// What the intake was asked to build.
#[derive(Debug)]
pub(super) struct ReferenceIntakeRequest {
    /// Image files, in the order the user picked them. The first one defines the footprint
    /// of a NEW entry; when improving an existing entry the footprint is the entry's.
    pub files: Vec<PathBuf>,
    /// Ring measurement tunables, already the tool's normalized ones.
    pub sample_params: SampleParams,
    /// `Some` improves an existing entry: its template, anchors, name and own calibration
    /// crops are kept, and the new crops are appended.
    pub base: Option<LoadedEntry>,
    /// Display name of a NEW entry, stored VERBATIM. Ignored when `base` is set, because
    /// renaming is its own operation.
    pub name: String,
    /// Largest footprint side the chapter detector will accept, pixels. Passed in so the
    /// intake and the chapter mode cannot disagree about the limit.
    pub max_side: u32,
}

/// What one supplied crop contributed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ReferenceCropReport {
    /// File name as shown to the user (the last path component).
    pub file: String,
    /// Measured background level under that crop, per channel.
    pub level: [f32; 3],
    /// Per-channel std of the ring the level was measured from.
    pub ring_std: [f32; 3],
    /// Integer offset the crop had to be shifted by to line up with the reference.
    pub dx: i32,
    pub dy: i32,
    /// Gradient-magnitude NCC at that offset. 1.0 for the crop that defines the reference.
    pub ncc: f32,
}

/// A successful intake.
#[derive(Debug)]
pub(super) struct ReferenceIntakeOutcome {
    /// Ready to hand to `watermark_library::save_entry`.
    pub request: SaveEntryRequest,
    pub reports: Vec<ReferenceCropReport>,
    /// The verdict the fit reached. Always `Separable` — the intake refuses otherwise.
    pub conditioning: ModelConditioning,
}

/// Builds a library entry from the same mark supplied on two or more known uniform
/// backgrounds.
///
/// This is the exact case the whole feature exists for: with two well-separated levels the
/// compositing equation has a closed form (`c = I|B=0`, `s = (I|B=255 - I|B=0)/255`), so
/// nothing about the mark is assumed. Black + white is the ideal pair; any uniform colour
/// works with proportionally larger error and additionally yields per-channel levels.
///
/// # Errors
/// A user-facing message, naming the file at fault where there is one, for:
/// - no files, or fewer than two when creating a new entry;
/// - a file that cannot be decoded;
/// - a crop too small to carry a full measurement ring around the footprint, or a footprint
///   above `max_side`;
/// - a crop that could not be aligned with the reference (gradient NCC below the floor);
/// - a crop whose background is NOT uniform — its `B` is unknown, so feeding it to the
///   estimator would poison `c` and `s`;
/// - crops whose backgrounds do not SEPARATE the model: two crops on the same background
///   cannot tell `alpha` from `W`, and accepting them would produce a confidently wrong
///   model. The message names the level(s) measured and the background to supply instead.
pub(super) fn run_reference_intake(
    request: ReferenceIntakeRequest,
) -> Result<ReferenceIntakeOutcome, String> {
    let ReferenceIntakeRequest {
        files,
        sample_params,
        base,
        name,
        max_side,
    } = request;
    if files.is_empty() {
        return Err(t!("cleaning.tools.watermark.chapter.reference_no_files_error").to_string());
    }
    if base.is_none() && files.len() < 2 {
        return Err(t!("cleaning.tools.watermark.chapter.reference_needs_two_error").to_string());
    }
    let sample_params = sample_params.normalized();
    let margin = sample_params.ring_width + REFERENCE_RING_MARGIN_SLACK_PX;

    let images = files
        .iter()
        .map(|path| decode_reference(path))
        .collect::<Result<Vec<_>, _>>()?;

    // The footprint size: the entry's own when improving, otherwise the first crop inset by
    // a full measurement ring on every side.
    let footprint = match base.as_ref() {
        Some(entry) => (entry.meta.width, entry.meta.height),
        None => {
            let (width, height) = images[0].dimensions();
            let inset = margin.saturating_mul(2);
            if width <= inset || height <= inset {
                return Err(reference_too_small(
                    &files[0],
                    (width, height),
                    (inset + 1, inset + 1),
                ));
            }
            (width - inset, height - inset)
        }
    };
    if footprint.0 > max_side || footprint.1 > max_side {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.selection_too_large_error",
            width = footprint.0,
            height = footprint.1,
            limit = max_side
        ));
    }

    // The alignment reference: the entry's stored template when improving, otherwise the
    // first crop's own footprint.
    let reference = match base.as_ref() {
        Some(entry) => gradient_magnitude(
            &entry.template,
            PixelRect::new(0, 0, entry.meta.width, entry.meta.height),
        ),
        None => gradient_magnitude(
            &images[0],
            PixelRect::new(margin, margin, footprint.0, footprint.1),
        ),
    };

    let mut reports = Vec::with_capacity(images.len());
    let mut crops: Vec<LibrarySample> = Vec::with_capacity(images.len());
    let mut template_crop: Option<RgbaImage> = None;
    for (index, image) in images.iter().enumerate() {
        let defines_reference = base.is_none() && index == 0;
        let (rect, ncc) = if defines_reference {
            (
                PixelRect::new(margin, margin, footprint.0, footprint.1),
                1.0f32,
            )
        } else {
            align_reference_crop(image, &reference, footprint, margin).ok_or_else(|| {
                reference_too_small(
                    &files[index],
                    image.dimensions(),
                    (footprint.0 + margin * 2, footprint.1 + margin * 2),
                )
            })?
        };
        if ncc < REFERENCE_MIN_ALIGN_NCC {
            return Err(tf!(
                "cleaning.tools.watermark.chapter.reference_align_error",
                file = file_label(&files[index]),
                score = format!("{ncc:.2}"),
                needed = format!("{REFERENCE_MIN_ALIGN_NCC:.2}")
            ));
        }
        let (level, ring_std) = match validate_calibration_sample(image, rect, &sample_params) {
            SampleVerdict::Calibration {
                level, ring_std, ..
            } => (level, ring_std),
            SampleVerdict::TemplateOnly {
                ring_std,
                ring_max_dev,
                std_limit,
                max_dev_limit,
                ..
            } => {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.reference_not_flat_error",
                    file = file_label(&files[index]),
                    std = format!("{:.1}", channel_max(ring_std)),
                    std_limit = format!("{std_limit:.1}"),
                    max_dev = format!("{:.1}", channel_max(ring_max_dev)),
                    max_dev_limit = format!("{max_dev_limit:.1}")
                ));
            }
            SampleVerdict::Unusable {
                reason: SampleRejection::BadRect,
            } => {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.reference_bad_rect_error",
                    file = file_label(&files[index])
                ));
            }
            SampleVerdict::Unusable {
                reason: SampleRejection::RingTooSmall { pixels, needed },
            } => {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.reference_ring_error",
                    file = file_label(&files[index]),
                    pixels = pixels,
                    needed = needed
                ));
            }
        };
        let crop = crop_image(image, rect);
        if defines_reference {
            template_crop = Some(crop.clone());
        }
        reports.push(ReferenceCropReport {
            file: file_label(&files[index]),
            level,
            ring_std,
            // Cast justification: both are pixel coordinates of a crop bounded by
            // `max_side`, far inside `i32`.
            dx: rect.x as i32 - margin as i32,
            dy: rect.y as i32 - margin as i32,
            ncc,
        });
        crops.push(LibrarySample {
            image: crop,
            origin: StoredSampleOrigin::ReferenceCrop,
            background: StoredSampleBackground::Flat { level, ring_std },
        });
    }

    // Build the kind: the entry's own crops first when improving, so a stored measurement is
    // never dropped by an intake that only adds to it.
    let template_source = match (base.as_ref(), template_crop.as_ref()) {
        (Some(entry), _) => entry.template.clone(),
        (None, Some(crop)) => crop.clone(),
        (None, None) => {
            return Err(t!("cleaning.tools.watermark.chapter.reference_no_files_error").to_string());
        }
    };
    let template =
        MarkTemplate::from_page(&template_source, PixelRect::new(0, 0, footprint.0, footprint.1))
            .map_err(engine_error)?;
    let mut kind = WatermarkKind::new(
        base.as_ref()
            .map_or_else(|| "reference".to_string(), |entry| entry.meta.id.clone()),
        template,
        alpha_blend_operator(),
    );
    // A reference crop carries no page layout, so it cannot know where the source stamps
    // its mark. Column 0 is the neutral placeholder; a chapter scan replaces the whole set
    // through `discover_anchors` before detection runs.
    let anchors: Vec<u32> = base
        .as_ref()
        .map(|entry| entry.meta.anchors.clone())
        .filter(|anchors| !anchors.is_empty())
        .unwrap_or_else(|| vec![0]);
    kind.template_mut()
        .set_anchors(&anchors)
        .map_err(engine_error)?;
    kind.set_alpha_assumption(
        base.as_ref()
            .map_or(AlphaAssumption::FromDeposit, |entry| {
                alpha_assumption_from_stored(entry.meta.alpha_assumption)
            }),
    );

    let mut samples: Vec<LibrarySample> = Vec::new();
    if let Some(entry) = base.as_ref() {
        samples.extend(entry.samples.iter().cloned());
    }
    samples.extend(crops);
    for sample in &samples {
        let StoredSampleBackground::Flat { level, ring_std } = sample.background;
        let rect = PixelRect::new(0, 0, sample.image.width(), sample.image.height());
        let calibration = CalibrationSample::from_page(
            &sample.image,
            0,
            rect,
            SampleBackground::Flat { level, ring_std },
        )
        .map_err(engine_error)?;
        kind.add_sample(calibration).map_err(engine_error)?;
    }
    match kind.refit() {
        Ok(()) | Err(ModelFitError::Refused(_)) => {}
        Err(ModelFitError::Invalid(err)) => return Err(engine_error(err)),
    }

    // The engine — not a threshold copied here — decides whether the supplied backgrounds
    // separate `alpha` from `W`. Anything short of `Separable` is refused, with the levels
    // measured and the background that would fix it.
    let conditioning = kind.conditioning().clone();
    if !conditioning.is_separable() {
        return Err(describe_spread_refusal(&conditioning));
    }

    let entry_id = base.as_ref().map(|entry| entry.meta.id.clone());
    let display_name = base
        .as_ref()
        .map_or_else(|| name.clone(), |entry| entry.meta.name.clone());
    let save = SaveEntryRequest {
        entry_id,
        // User data: whatever the user typed, byte for byte.
        name: display_name,
        operator: kind.model().map_or_else(
            || "alpha_blend".to_string(),
            |model| model.operator().id().to_string(),
        ),
        width: footprint.0,
        height: footprint.1,
        anchors: kind.template().anchors().to_vec(),
        anchor_key: kind.template().anchor_key(),
        alpha_assumption: base
            .as_ref()
            .map_or(StoredAlphaAssumption::FromDeposit, |entry| {
                entry.meta.alpha_assumption
            }),
        signature: kind.signature().map(|signature| StoredSignature {
            reference_level: signature.reference_level,
            deposit_chroma: signature.deposit_chroma,
            mean_deposit: signature.mean_deposit,
            peak_alpha: signature.peak_alpha,
        }),
        calibration: stored_calibration(&kind),
        // A reference crop has no chapter behind it, so it contributes no search metadata;
        // the writer keeps whatever the entry already recorded.
        source: None,
        template: template_source,
        samples,
        planes: kind.model().map(|model| LibraryPlanes {
            c: model.c().to_vec(),
            s: model.s().to_vec(),
        }),
    };
    Ok(ReferenceIntakeOutcome {
        request: save,
        reports,
        conditioning,
    })
}

/// The refusal for a crop set whose backgrounds do not separate the model.
///
/// Two shapes, because they are two different user mistakes: every crop on ONE background
/// (nothing to separate at all) and crops on backgrounds too close together (not enough
/// contrast to fit the slope). Both name the background to supply instead, which the
/// engine's own `suggested_background()` provides.
fn describe_spread_refusal(conditioning: &ModelConditioning) -> String {
    let levels = conditioning.levels();
    let target = match conditioning.suggested_background() {
        Some(SuggestedBackground::Darker { at_most }) => tf!(
            "cleaning.tools.watermark.chapter.suggest_darker",
            level = format!("{at_most:.0}")
        ),
        Some(SuggestedBackground::Brighter { at_least }) => tf!(
            "cleaning.tools.watermark.chapter.suggest_brighter",
            level = format!("{at_least:.0}")
        ),
        None => String::new(),
    };
    let measured = levels
        .iter()
        .map(|level| format!("{level:.0}"))
        .collect::<Vec<_>>()
        .join(", ");
    if levels.len() < 2 {
        tf!(
            "cleaning.tools.watermark.chapter.reference_same_background_error",
            level = measured,
            target = target
        )
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.reference_spread_error",
            levels = measured,
            spread = format!("{:.0}", conditioning.spread()),
            target = target
        )
    }
}

/// Decodes one reference crop into RGBA8.
///
/// # Errors
/// A user-facing message naming the file when it cannot be opened or decoded.
fn decode_reference(path: &Path) -> Result<RgbaImage, String> {
    image::open(path)
        .map(|image| image.to_rgba8())
        .map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.reference_decode_error",
                file = file_label(path),
                err = err
            )
        })
}

/// The last path component, for a report line. Never localized.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| {
            name.to_string_lossy().to_string()
        })
}

/// The "crop too small" refusal, naming what was supplied and the smallest crop that would
/// still carry a full measurement ring around the footprint.
fn reference_too_small(path: &Path, size: (u32, u32), needed: (u32, u32)) -> String {
    tf!(
        "cleaning.tools.watermark.chapter.reference_too_small_error",
        file = file_label(path),
        width = size.0,
        height = size.1,
        needed_width = needed.0,
        needed_height = needed.1
    )
}

/// Largest of three per-channel values.
fn channel_max(values: [f32; 3]) -> f32 {
    values.iter().copied().fold(0.0f32, f32::max)
}

/// Wraps an engine failure into the intake's user-facing message.
fn engine_error(err: impl std::fmt::Display) -> String {
    tf!("cleaning.tools.watermark.chapter.engine_error", err = err)
}

/// Cuts `rect` out of `image`. The rect must already be validated against the image.
fn crop_image(image: &RgbaImage, rect: PixelRect) -> RgbaImage {
    image::imageops::crop_imm(image, rect.x, rect.y, rect.width, rect.height).to_image()
}

/// Gradient magnitude of the luma plane over `rect`, row major, `width*height` entries.
///
/// Forward differences, with the last row and column left at zero: the mark's edges are
/// what the alignment correlates on, and they are interior to any usable crop. Gradient
/// magnitude rather than luma because the two crops sit on DIFFERENT backgrounds, which
/// flips the sign of the mark's contrast between them (see the file header).
fn gradient_magnitude(image: &RgbaImage, rect: PixelRect) -> Vec<f32> {
    let (width, height) = (rect.width as usize, rect.height as usize);
    let stride = image.width() as usize;
    let raw = image.as_raw();
    let mut luma = vec![0.0f32; width * height];
    for row in 0..height {
        let base = ((rect.y as usize + row) * stride + rect.x as usize) * 4;
        for (column, px) in raw[base..base + width * 4].chunks_exact(4).enumerate() {
            luma[row * width + column] =
                LUMA_R * f32::from(px[0]) + LUMA_G * f32::from(px[1]) + LUMA_B * f32::from(px[2]);
        }
    }
    let mut gradient = vec![0.0f32; width * height];
    for row in 0..height.saturating_sub(1) {
        for column in 0..width.saturating_sub(1) {
            let index = row * width + column;
            let dx = luma[index + 1] - luma[index];
            let dy = luma[index + width] - luma[index];
            gradient[index] = dx.hypot(dy);
        }
    }
    gradient
}

/// Zero-mean normalized cross-correlation of two equally long planes.
///
/// Returns 0 for a degenerate input (empty, or either plane constant), which the caller
/// reads as "did not align" rather than as a perfect or an impossible match.
fn ncc(left: &[f32], right: &[f32]) -> f32 {
    if left.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    // Cast justification: the length is a crop area bounded by `max_side` squared.
    let count = left.len() as f32;
    let left_mean = left.iter().sum::<f32>() / count;
    let right_mean = right.iter().sum::<f32>() / count;
    let mut dot = 0.0f64;
    let mut left_sq = 0.0f64;
    let mut right_sq = 0.0f64;
    for (&a, &b) in left.iter().zip(right.iter()) {
        let (a, b) = (a - left_mean, b - right_mean);
        dot += f64::from(a) * f64::from(b);
        left_sq += f64::from(a) * f64::from(a);
        right_sq += f64::from(b) * f64::from(b);
    }
    let norm = (left_sq * right_sq).sqrt();
    if norm <= f64::EPSILON {
        return 0.0;
    }
    // Cast justification: a correlation coefficient, bounded by +-1 by construction.
    (dot / norm) as f32
}

/// Finds the integer placement of a `footprint`-sized window inside `image` whose gradient
/// map correlates best with `reference`.
///
/// The window is searched around the image centre within [`REFERENCE_ALIGN_SEARCH_PX`],
/// always leaving `margin` pixels of background around it so the ring measurement still has
/// something to measure. `None` when the image is too small to hold the footprint plus its
/// margins at all.
fn align_reference_crop(
    image: &RgbaImage,
    reference: &[f32],
    footprint: (u32, u32),
    margin: u32,
) -> Option<(PixelRect, f32)> {
    let (width, height) = image.dimensions();
    let needed_w = footprint.0.checked_add(margin.checked_mul(2)?)?;
    let needed_h = footprint.1.checked_add(margin.checked_mul(2)?)?;
    if width < needed_w || height < needed_h {
        return None;
    }
    // Cast justification: every value here is a pixel coordinate of an image the caller
    // already bounded by `max_side` plus a margin, far inside `i32`.
    let centre_x = ((width - footprint.0) / 2) as i32;
    let centre_y = ((height - footprint.1) / 2) as i32;
    let min_x = margin as i32;
    let min_y = margin as i32;
    let max_x = (width - footprint.0 - margin) as i32;
    let max_y = (height - footprint.1 - margin) as i32;

    let candidates: Vec<(i32, i32)> = (-REFERENCE_ALIGN_SEARCH_PX..=REFERENCE_ALIGN_SEARCH_PX)
        .flat_map(|dy| {
            (-REFERENCE_ALIGN_SEARCH_PX..=REFERENCE_ALIGN_SEARCH_PX).map(move |dx| (dx, dy))
        })
        .filter(|(dx, dy)| {
            let x = centre_x + dx;
            let y = centre_y + dy;
            (min_x..=max_x).contains(&x) && (min_y..=max_y).contains(&y)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // The search is embarrassingly parallel and runs on a worker thread; `rayon` keeps a
    // full-page reference crop from turning the intake into a visible wait.
    let best = candidates
        .into_par_iter()
        .map(|(dx, dy)| {
            // Cast justification: clamped into the valid ranges computed above.
            let rect = PixelRect::new(
                (centre_x + dx) as u32,
                (centre_y + dy) as u32,
                footprint.0,
                footprint.1,
            );
            let score = ncc(reference, &gradient_magnitude(image, rect));
            (score, rect.x, rect.y)
        })
        .reduce(
            || (f32::NEG_INFINITY, 0, 0),
            |left, right| {
                // Ties break on the lower origin so the result does not depend on the order
                // rayon happened to reduce in.
                if right.0 > left.0 || (right.0 == left.0 && (right.1, right.2) < (left.1, left.2))
                {
                    right
                } else {
                    left
                }
            },
        );
    if !best.0.is_finite() {
        return None;
    }
    Some((
        PixelRect::new(best.1, best.2, footprint.0, footprint.1),
        best.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::watermark_library::StoredSourceRef;
    use image::Rgba;

    /// Synthetic mark: a solid opaque-ish glyph in the middle of the footprint, with a
    /// per-pixel alpha ramp so `c` and `s` vary the way a real mark's do.
    fn mark_alpha(x: u32, y: u32, width: u32, height: u32) -> f32 {
        let inside = x >= width / 4 && x < width * 3 / 4 && y >= height / 4 && y < height * 3 / 4;
        if !inside {
            return 0.0;
        }
        // Cast justification: small synthetic dimensions, exactly representable in f32.
        0.15 + 0.2 * (x as f32 / width as f32)
    }

    /// Renders the synthetic mark composited over a flat background, with `margin` pixels
    /// of that background around it.
    fn render_reference(
        footprint: (u32, u32),
        margin: u32,
        background: [u8; 3],
        colour: [f32; 3],
    ) -> RgbaImage {
        let width = footprint.0 + margin * 2;
        let height = footprint.1 + margin * 2;
        RgbaImage::from_fn(width, height, |x, y| {
            let inside = x >= margin && y >= margin && x < margin + footprint.0 && y < margin + footprint.1;
            if !inside {
                return Rgba([background[0], background[1], background[2], 255]);
            }
            let alpha = mark_alpha(x - margin, y - margin, footprint.0, footprint.1);
            let channels: [u8; 3] = std::array::from_fn(|channel| {
                let base = f32::from(background[channel]);
                // Cast justification: an alpha composite of two 0..=255 values, rounded.
                (alpha * colour[channel] + (1.0 - alpha) * base).clamp(0.0, 255.0).round() as u8
            });
            Rgba([channels[0], channels[1], channels[2], 255])
        })
    }

    fn write_reference(dir: &Path, name: &str, image: &RgbaImage) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create fixture dir");
        let path = dir.join(name);
        image.save(&path).expect("write fixture");
        path
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("manhwastudio-wm-reference-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn intake(files: Vec<PathBuf>) -> ReferenceIntakeRequest {
        ReferenceIntakeRequest {
            files,
            sample_params: SampleParams::default(),
            base: None,
            name: "  Знак  ".to_string(),
            max_side: 512,
        }
    }

    #[test]
    fn a_white_and_black_pair_produces_the_exact_case() {
        let dir = fixture_dir("pair");
        let footprint = (40, 32);
        let margin = 6;
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        let black = write_reference(
            &dir,
            "black.png",
            &render_reference(footprint, margin, [0, 0, 0], [20.0, 30.0, 25.0]),
        );
        let outcome = run_reference_intake(intake(vec![white, black])).expect("intake succeeds");
        assert!(
            outcome.conditioning.is_separable(),
            "two well-separated levels must give the closed form, got {:?}",
            outcome.conditioning
        );
        assert_eq!(outcome.reports.len(), 2);
        assert_eq!(outcome.request.samples.len(), 2);
        assert!(outcome.request.planes.is_some(), "the exact case carries planes");
        // The display name is user data and is never trimmed on the way in.
        assert_eq!(outcome.request.name, "  Знак  ");
        // The entry's footprint is the WHOLE first crop inset by one measurement ring
        // (`SampleParams::default().ring_width` + the slack), not the mark's own bbox: the
        // ring is what the background level is read from, and everything inside it is model.
        let ring = SampleParams::default().normalized().ring_width + REFERENCE_RING_MARGIN_SLACK_PX;
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            (
                footprint.0 + margin * 2 - ring * 2,
                footprint.1 + margin * 2 - ring * 2
            ),
            "the footprint is the first crop inset by a full measurement ring"
        );
        assert!(
            outcome.reports.iter().all(|report| report.dx == 0 && report.dy == 0),
            "identically framed crops need no shift"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_crops_on_the_same_background_are_refused() {
        let dir = fixture_dir("same-background");
        let footprint = (40, 32);
        let margin = 6;
        let first = write_reference(
            &dir,
            "white-a.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        let second = write_reference(
            &dir,
            "white-b.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        let err = run_reference_intake(intake(vec![first, second]))
            .expect_err("one background cannot separate alpha from W");
        assert!(
            !err.is_empty(),
            "the refusal must carry the measured reason, not an empty string"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backgrounds_too_close_together_are_refused() {
        let dir = fixture_dir("narrow-spread");
        let footprint = (40, 32);
        let margin = 6;
        // 255 and 200 are two DISTINCT levels, but only 55 LSB apart — below what the
        // engine needs to fit the slope, so the intake must refuse instead of producing a
        // model whose alpha scale is noise.
        let bright = write_reference(
            &dir,
            "bright.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        let almost = write_reference(
            &dir,
            "almost.png",
            &render_reference(footprint, margin, [200, 200, 200], [20.0, 30.0, 25.0]),
        );
        assert!(
            run_reference_intake(intake(vec![bright, almost])).is_err(),
            "a spread below the engine's own floor must be refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_file_is_refused_when_creating() {
        let dir = fixture_dir("single");
        let only = write_reference(
            &dir,
            "white.png",
            &render_reference((40, 32), 6, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        assert!(run_reference_intake(intake(vec![only])).is_err());
        assert!(run_reference_intake(intake(Vec::new())).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crop_on_a_structured_background_is_refused() {
        let dir = fixture_dir("structured");
        let footprint = (40, 32);
        let margin = 6;
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        // Same mark, but the surrounding background is a hard gradient: `B` is unknown
        // there, so the crop must be refused as a calibration target.
        let mut noisy = render_reference(footprint, margin, [0, 0, 0], [20.0, 30.0, 25.0]);
        let (width, height) = noisy.dimensions();
        for y in 0..height {
            for x in 0..width {
                let outside = x < margin || y < margin || x >= margin + footprint.0 || y >= margin + footprint.1;
                if outside {
                    // Cast justification: a synthetic ramp over a small fixture image.
                    let value = ((x * 7 + y * 11) % 200) as u8;
                    noisy.put_pixel(x, y, Rgba([value, value, value, 255]));
                }
            }
        }
        let structured = write_reference(&dir, "structured.png", &noisy);
        assert!(run_reference_intake(intake(vec![white, structured])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shifted_crop_is_aligned_before_it_is_measured() {
        let dir = fixture_dir("shifted");
        let footprint = (40, 32);
        let margin = 6;
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        // The same mark on black, but framed with a wider margin on the left/top: the
        // footprint sits off-centre and has to be found.
        let wide = render_reference(footprint, margin + 4, [0, 0, 0], [20.0, 30.0, 25.0]);
        let mut shifted = RgbaImage::from_pixel(
            wide.width(),
            wide.height(),
            Rgba([0, 0, 0, 255]),
        );
        for y in 0..footprint.1 + margin * 2 {
            for x in 0..footprint.0 + margin * 2 {
                shifted.put_pixel(x, y, *wide.get_pixel(x + 4, y + 4));
            }
        }
        let black = write_reference(&dir, "black.png", &shifted);
        let outcome =
            run_reference_intake(intake(vec![white, black])).expect("the shifted crop aligns");
        assert!(outcome.conditioning.is_separable());
        assert!(
            outcome.reports[1].ncc >= REFERENCE_MIN_ALIGN_NCC,
            "alignment score {} must clear the floor",
            outcome.reports[1].ncc
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn summary(id: &str, signature: StoredSignature, verdict: &str, spread: f32) -> EntrySummary {
        EntrySummary {
            id: id.to_string(),
            name: id.to_string(),
            width: 40,
            height: 32,
            anchor_key: "0".to_string(),
            verdict: verdict.to_string(),
            levels: vec![0.0, 255.0],
            spread,
            samples: 2,
            alpha: None,
            fit_method: None,
            signature: Some(signature),
            sources: Vec::<StoredSourceRef>::new(),
            updated_unix: 1,
        }
    }

    /// Two entries whose ARTWORK is identical and whose deposits are not: the colour mark
    /// and its greyscale twin of the measured second chapter. Matching must pick the one
    /// whose deposit matches, never the one whose picture does.
    #[test]
    fn auto_match_separates_artwork_identical_marks() {
        let colour = StoredSignature {
            reference_level: 255.0,
            deposit_chroma: 120.0,
            mean_deposit: 90.0,
            peak_alpha: 0.38,
        };
        let pale = StoredSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };
        let entries = vec![
            summary("wm-colour", colour, "separable", 255.0),
            summary("wm-pale", pale, "separable", 255.0),
        ];
        let measured_pale = MarkSignature {
            reference_level: 255.0,
            deposit_chroma: 5.0,
            mean_deposit: 44.0,
            peak_alpha: 0.19,
        };
        let ranked = rank_library_candidates(&entries, &measured_pale, (40, 32));
        assert_eq!(ranked.len(), 1, "only the pale entry carries this mark");
        assert_eq!(ranked[0].entry_id, "wm-pale");

        let measured_colour = MarkSignature {
            reference_level: 255.0,
            deposit_chroma: 118.0,
            mean_deposit: 91.0,
            peak_alpha: 0.38,
        };
        let ranked = rank_library_candidates(&entries, &measured_colour, (40, 32));
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].entry_id, "wm-colour");

        // A different footprint is never substituted, however well the deposit matches.
        assert!(rank_library_candidates(&entries, &measured_colour, (41, 32)).is_empty());
    }

    #[test]
    fn a_stronger_calibration_outranks_a_graded_one() {
        let signature = StoredSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };
        let entries = vec![
            summary("wm-graded", signature, "deposit_exact", 0.0),
            summary("wm-exact", signature, "separable", 255.0),
        ];
        let measured = MarkSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };
        let ranked = rank_library_candidates(&entries, &measured, (40, 32));
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].entry_id, "wm-exact");

        // Adopting is only offered when it actually strengthens the chapter's own fit.
        let graded = ModelConditioning::DepositExact {
            levels: vec![255.0],
            spread: 0.0,
            samples: 3,
            alpha: AlphaUncertainty::from_percent(AlphaSource::Assumed, 30.0),
        };
        assert!(candidate_improves(&ranked[0], &graded));
        let separable = ModelConditioning::Separable {
            levels: vec![0.0, 255.0],
            spread: 255.0,
            min_pixel_spread: 255.0,
            alpha: AlphaUncertainty::from_flat_fit(255.0, 0.19),
        };
        assert!(!candidate_improves(&ranked[0], &separable));
    }

    #[test]
    fn stored_calibration_roundtrips_through_the_verdict() {
        let stored = StoredCalibration {
            verdict: "deposit_exact".to_string(),
            levels: vec![255.0],
            spread: 0.0,
            samples: 4,
            fit_method: Some("deposit_exact".to_string()),
            clamped_pixels: 0,
            alpha: Some(StoredAlpha {
                source: "assumed".to_string(),
                percent: 30.0,
                rms_lsb: 9.6,
                dark_rms_lsb: 14.1,
                dark_max_lsb: 39.0,
                dark_luma: 80.0,
            }),
        };
        let conditioning = conditioning_from_stored(&stored).expect("a known verdict maps back");
        assert_eq!(conditioning_wire(&conditioning), "deposit_exact");
        assert!(
            conditioning.suggested_background().is_some(),
            "a graded entry must still name the background that would fix it"
        );
        let mut unknown = stored;
        unknown.verdict = "something_newer".to_string();
        assert!(conditioning_from_stored(&unknown).is_none());
    }
}
