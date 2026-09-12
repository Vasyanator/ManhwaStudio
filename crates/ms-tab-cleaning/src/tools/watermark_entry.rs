/*
FILE HEADER (cleaning/tools/watermark_entry.rs)

Purpose:
The bridge between the GUI-free decomposition engine (`../watermark_chapter.rs`) and the
on-disk library (`watermark_library.rs`). It owns three things neither of those may own:

1. the MAPPING between a fitted `WatermarkKind` and the library's plain-data records —
   the literal wire tags of a verdict, a fit method and an alpha source, and their inverse,
   plus the two halves of the round trip themselves: `rebuild_stored_kind` (stored material ->
   fitted kind) and `save_request_from_kind` (fitted kind -> `SaveEntryRequest`). Those two are
   the ONLY place either direction is written; the reference intake, the chapter's library load
   and its library write all go through them, so they cannot disagree;
2. REFERENCE-CROP INTAKE: turning the same mark supplied on two or more known uniform
   backgrounds, as separate image files, into a library entry with a closed-form model;
3. AUTO-MATCH RANKING: deciding which library entries carry the mark a chapter just
   measured, and in which order;
4. the CARD MATERIAL a library screen draws without recomputing anything — the mark
   composited on white (`render_mark_on_white`) and the typed warning set
   (`entry_warnings`).

Main responsibilities:
- keep the engine free of persistence (it never sees a `Stored*` type) and the library free
  of the engine (it never sees a `WatermarkKind`);
- refuse a reference-crop set the maths cannot use, naming the measured reason AND classifying
  it (`ReferenceRefusal`) so the reporting surface can say what to do about it: a crop whose
  background is not uniform, crops that could not be aligned to one another, and — the
  important one — crops whose backgrounds do not span enough levels to separate `alpha`
  from `W`. Two crops on the SAME background are refused rather than silently accepted,
  because the resulting model would be confidently wrong.

Key structures:
- `ReferenceIntakeRequest` / `ReferenceIntakeOutcome` / `ReferenceCropReport`
- `ReferenceIntakeError` / `ReferenceRefusal`: an intake refusal is an already-localized message
  PLUS a tag (`TooSmall` / `Misaligned` / `Background` / `Other`). The message says what went
  wrong in the engine's own terms — a size, a correlation score, a measured level — and the tag
  is what lets a caller add the advice only IT can give: a crop re-dragged on the canvas can be
  dragged again, one picked from disk cannot. `From<String>` keeps every other refusal untagged
  and every `?` in this file working unchanged.
- `LibraryCandidate`
- `EntryWarnings`: the typed material a card warns from — the rebuilt verdict, the clamped
  share against `CLAMPED_SHARE_WARN`, and which samples rest on an asserted level. Typed, never
  pre-formatted: the wording belongs to the UI layer.

Key functions:
- `rebuild_stored_kind()`, `save_request_from_kind()`, `drop_entry_sample()`,
  `trim_entry_footprint()`, `trim_entry_geometry()`, `run_reference_intake()`,
  `rank_library_candidates()`,
  `stored_calibration()`, `conditioning_from_stored()`, `engine_background()`,
  `luma_of_level()`, `render_mark_on_white()`, `entry_warnings()`.

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
  never by a threshold copied here: the intake builds the kind, refits, and — when it is
  CREATING an entry (`base.is_none()`) — accepts only `ModelConditioning::Separable`. The
  refusal message is then built from the verdict's own levels, spread and
  `suggested_background()`. IMPROVING an existing entry is not gated that way: the store holds
  one-sample, non-separable entries on purpose, so an entry's FIRST crop — necessarily one
  level — must be accepted, and the verdict it reaches is reported rather than assumed.
- An EMPTY entry (`base` present, `LoadedEntry::template` absent) is filled exactly as a new
  one is built: the first crop defines the footprint, the alignment reference and the template,
  while the entry keeps its own id, name and creation time. `empty_entry_request` is the write
  that creates one.
- WHAT A CROP MUST PROVIDE is coverage of the MARK plus a measuring ring — never coverage of a
  rectangle some earlier drag or some detector box happened to define. Three rules carry that, and
  which one applies is decided by whether anything on disk already binds the geometry:
  * an entry with NO TEMPLATE (brand new, or empty and waiting for its first crop) derives its
    footprint from ALL of its crops at once (`measure_mark_extents` -> `joint_footprint`), never
    from the first one: each crop's own mark extent is measured against its own frame — which needs
    no registration and is exactly the case the engine's template rule calls sound — and the
    footprint is the UNION of those extents plus one `FOOTPRINT_TRIM_SAFETY_PX` border, capped by
    the first crop's own inset rule so it can only ever be SMALLER. The rule it replaces made the
    footprint the first crop inset by a fixed margin, which cancelled in the size guard and left
    "every later crop must be at least as large as the first in BOTH axes" — a pair where neither
    crop covers the other could not be built in any order, which is the user's second mark;
  * an entry that HAS a template keeps its stored footprint, because its anchors were measured
    against it — UNLESS that rectangle is what blocks a supplied crop, in which case the intake
    re-derives it from the mark inside the entry's own template (`trim_entry_geometry`, the same
    measurement and the same anchor shift the manual «Trim the footprint» button performs) and
    reports the change in `ReferenceIntakeOutcome::trimmed`. It runs only when the stored rectangle
    BLOCKS a crop: a footprint that works is never moved, because moving one silently is exactly
    what the anchors cannot survive. Both the trim and the new samples land in ONE atomic write;
  * a MARK-SHAPED footprint — derived jointly or trimmed down — is then placed anywhere inside a
    crop and its ring is admitted by the engine's own pixel COUNT, while a rectangle the crop's own
    geometry states keeps the background it states on every side. Each crop's alignment starts from
    its own measured extent (`centre_on_extent`) in the first case: with a mark-sized footprint the
    image-centre guess misses by more than the search half-width.
  Stated `CropMargins` do NOT veto any of this. They say what the cutter had to clip, which is
  information about the crop's own frame: they cap the joint footprint, and they decide whether a
  mark reaching the crop's border is a page border the cutter could not get past (stated `0`) or a
  crop that CUT the mark (refused, naming the side). They are not a geometry the intake must adopt,
  and treating them as one is what kept the canvas lane — the one the user works in — on a
  drag-shaped footprint long after the picker lane had stopped using one.
- The extent and the alignment are two independent measurements of where one mark is, and neither
  grades the other. On the measured reference pair they disagree by 3 px, and the GRADIENT is the
  one that is right: the mark is a glyph with a soft glow, so against white the glyph deviates and
  against black the glow does, and the 1-LSB extent measures a different part of the same mark on
  each background. A disagreement beyond `REFERENCE_REGISTRATION_DRIFT_PX` is REFUSED rather than
  fitted, because a two-sample closed form is exactly determined: a model over mis-registered
  planes still reports a confident verdict and nothing downstream can see it.
- A crop the CUTTER had to clip at the image border states its real per-side background in
  `ReferenceIntakeRequest::crop_margins`, and the footprint is derived from that instead of a
  symmetric inset the crop does not have. Absent margins keep the symmetric rule, which is the
  only assumption available about a file the user picked. The ring is then measured on the crop,
  whose own border is where the page's border was, so it is pixel for pixel the ring the page
  would have given; its partialness rides along in `StoredSampleBackground::Flat`.
- EDITING a stored entry's sample list (`drop_entry_sample`) is a REFIT plus a REWRITE, never a
  file deletion: the `c`/`s` planes were fitted from ALL of the entry's crops. It deliberately
  does NOT copy the intake's `is_separable()` refusal — that rule belongs to building an entry
  from nothing. Dropping to ONE crop degrades the verdict to `deposit_exact`, which the stored
  types already express, and dropping the LAST one leaves a legitimate template-only entry with
  no model; confirming that is the UI's job, not this layer's.
- A crop whose ring is NOT uniform is still refused — unless the caller asserted a level for it
  in `ReferenceIntakeRequest::manual_levels`. The automatic flatness test is untouched by that:
  a crop that measures flat keeps its MEASUREMENT even when a level was also stated, because
  evidence outranks a claim. An accepted assertion is persisted as
  `StoredSampleBackground::Manual` and downgrades the model through the engine's own
  `AlphaSource::ManualBackgrounds`, so nothing downstream can describe such an entry as
  measured.
- `engine_background()` is the ONE place the stored and engine background enums are married, so
  the measured/asserted distinction cannot be flattened by a call site that destructures the
  record itself.
- TRIMMING a footprint (`trim_entry_footprint`, and `trim_entry_geometry` which is its measuring
  half, shared with the reference intake above) is the same shape as `drop_entry_sample`: a REFIT
  plus a full rewrite, never an edit of the stored images in place. The engine measures WHERE the
  mark ends (`trimmed_footprint_from_model` when the entry has samples to fit a model from,
  `trimmed_footprint_from_template` otherwise); this layer re-crops the template and every crop,
  shifts every anchor by the trim's horizontal offset — an anchor is the page COLUMN of the
  footprint's left edge, so that offset and nothing else is what moves — and refits, so the
  written planes always match the written crops. Getting the shift wrong would subtract the mark
  in the wrong place across a whole chapter, which is what
  `a_trimmed_template_finds_the_mark_at_the_same_absolute_pixels` pins in the engine.
*/
use std::path::{Path, PathBuf};

use image::RgbaImage;
use rayon::prelude::*;

use super::watermark_library::{
    EntrySummary, LibraryPlanes, LibrarySample, LoadedEntry, LoadedPlanes, ManualBackgroundRef,
    SaveEntryRequest, StoredAlpha, StoredAlphaAssumption, StoredCalibration,
    StoredSampleBackground, StoredSampleOrigin, StoredSignature, StoredSourceRef,
    load_entry_planes,
};
use crate::watermark_chapter::{
    AlphaAssumption, AlphaSource, AlphaUncertainty, CalibrationSample, CompositingOperator,
    FOOTPRINT_TRIM_SAFETY_PX, FitMethod, MarkSignature, MarkTemplate, ModelConditioning,
    ModelFitError, PixelRect,
    RingCoverage, SampleBackground, SampleParams, SampleRejection, SampleVerdict,
    SuggestedBackground,
    WatermarkError, WatermarkKind, alpha_blend_operator, mark_extent_from_template,
    operator_from_id,
    trimmed_footprint_from_model, trimmed_footprint_from_template,
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
/// Largest disagreement tolerated between the alignment's answer and the crop's own mark extent,
/// pixels, when a NEW entry's footprint is derived jointly from every crop.
///
/// The two measurements are independent and neither can grade the other: the aligner correlates
/// GRADIENT MAGNITUDE, which follows the mark's edges, while the extent is every pixel deviating
/// from the crop's border level by one LSB. On the measured reference pair they disagree by 3 px,
/// and the gradient is the one that is right — the mark there is a glyph with a soft glow, the
/// glyph is what shows against white and the glow is what shows against black, so the extent
/// measures a different part of the same mark on each background. That is a property of the mark,
/// not an error, and the footprint's own safety border is exactly the slack that absorbs it.
///
/// A disagreement LARGER than that border is not absorbed: the deposit would then reach outside
/// the footprint on one side, and the two crops are more likely to be different marks, or the same
/// mark at a different scale, than one mark seen twice. It is refused rather than fitted, because a
/// two-sample closed form is exactly determined and has no residual left to expose it.
const REFERENCE_REGISTRATION_DRIFT_PX: u32 = FOOTPRINT_TRIM_SAFETY_PX;

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
        AlphaSource::ManualBackgrounds => "manual_backgrounds",
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
        "manual_backgrounds" => AlphaSource::ManualBackgrounds,
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

/// The engine's view of one persisted crop's background.
///
/// This is the single place the two enums are married, so the difference between a MEASURED and
/// an ASSERTED level survives the round trip instead of being flattened by whichever call site
/// happened to destructure the record. A `Manual` record deliberately produces
/// [`SampleBackground::Manual`] and not a `Flat` with a fabricated `ring_std`: the fit treats
/// the two alike, but only the engine's own provenance may decide what the verdict then claims.
///
/// RING COVERAGE round-trips the same way. A record written before the coverage existed carries
/// neither count, and it is restored as a COMPLETE ring — the only reading that does not invent
/// a fact: those entries were written by a build whose capture refused anything but a full ring,
/// so "unknown" there means "unclipped by construction", while a build that can clip always
/// writes both counts.
#[must_use]
pub(super) fn engine_background(stored: StoredSampleBackground) -> SampleBackground {
    match stored {
        StoredSampleBackground::Flat {
            level,
            ring_std,
            ring_pixels,
            ring_full_pixels,
        } => SampleBackground::Flat {
            level,
            ring_std,
            ring: match (ring_pixels, ring_full_pixels) {
                (Some(pixels), Some(full)) => RingCoverage {
                    pixels,
                    full_pixels: full,
                },
                (Some(pixels), None) => RingCoverage::full(pixels),
                (None, Some(full)) => RingCoverage::full(full),
                (None, None) => RingCoverage::full(0),
            },
        },
        StoredSampleBackground::Manual { level } => SampleBackground::Manual { level },
    }
}

/// The persisted form of one MEASURED background, coverage included.
///
/// The inverse of [`engine_background`]'s `Flat` arm, kept beside it so the two cannot drift:
/// a coverage of zero total pixels is the "never measured a count" case and writes neither
/// field, which is exactly the document an older build produced.
#[must_use]
pub(super) fn stored_flat_background(level: [f32; 3], ring_std: [f32; 3], ring: RingCoverage) -> StoredSampleBackground {
    if ring.full_pixels == 0 {
        return StoredSampleBackground::Flat {
            level,
            ring_std,
            ring_pixels: None,
            ring_full_pixels: None,
        };
    }
    StoredSampleBackground::Flat {
        level,
        ring_std,
        ring_pixels: Some(ring.pixels),
        ring_full_pixels: Some(ring.full_pixels),
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
// Stored entry -> engine kind -> stored entry
// ---------------------------------------------------------------------------------------

/// Page index the engine records on a calibration sample cut from `origin`.
///
/// A reference crop has no page behind it, so it reports `0`. The value is provenance the
/// engine carries along; nothing in the fit reads it.
fn sample_page_index(origin: StoredSampleOrigin) -> usize {
    match origin {
        StoredSampleOrigin::Page { page_index, .. } => page_index,
        StoredSampleOrigin::ReferenceCrop => 0,
    }
}

/// Rebuilds the engine's [`WatermarkKind`] from stored library material and refits it.
///
/// This is the ONE place a stored entry becomes a model: the calibration crops are the
/// reconstruction source, so every caller that needs an entry's model — loading one into a
/// chapter, extending one through the intake, editing its sample list — goes through here and
/// cannot disagree with the others about how the kind is assembled.
///
/// `kind_id` becomes the catalog identity; `footprint` is the entry's `(width, height)`, which
/// the template must match; an EMPTY `anchors` leaves the template's own single anchor alone.
///
/// An empty `samples` is a legitimate state of a stored entry — a template-only entry — and is
/// NOT a failure: the fit is not attempted at all, and the kind is left in exactly the state a
/// refused fit would leave it in (no model, `NotEnoughSamples { have: 0, need: 2 }`). Calling
/// `refit` instead would raise `WatermarkError::NoSamples`, which describes a caller mistake
/// rather than this entry.
///
/// # Errors
/// A user-facing message when the template is unusable, the anchors are invalid, a crop cannot
/// be turned into a calibration sample, or the fit rejects the INPUT (a geometry mismatch
/// between crops). A fit merely REFUSED for lack of background spread is not an error: the
/// verdict it produced is the answer, and the kind carries it.
pub(super) fn rebuild_stored_kind(
    kind_id: String,
    template_image: &RgbaImage,
    footprint: (u32, u32),
    anchors: &[u32],
    alpha_assumption: StoredAlphaAssumption,
    samples: &[LibrarySample],
) -> Result<WatermarkKind, String> {
    let template = MarkTemplate::from_page(
        template_image,
        PixelRect::new(0, 0, footprint.0, footprint.1),
    )
    .map_err(engine_error)?;
    let mut kind = WatermarkKind::new(kind_id, template, alpha_blend_operator());
    if !anchors.is_empty() {
        kind.template_mut().set_anchors(anchors).map_err(engine_error)?;
    }
    kind.set_alpha_assumption(alpha_assumption_from_stored(alpha_assumption));
    for sample in samples {
        let rect = PixelRect::new(0, 0, sample.image.width(), sample.image.height());
        let calibration = CalibrationSample::from_page(
            &sample.image,
            sample_page_index(sample.origin),
            rect,
            engine_background(sample.background),
        )
        .map_err(engine_error)?;
        kind.add_sample(calibration).map_err(engine_error)?;
    }
    if !samples.is_empty() {
        match kind.refit() {
            Ok(()) | Err(ModelFitError::Refused(_)) => {}
            Err(ModelFitError::Invalid(err)) => return Err(engine_error(err)),
        }
    }
    Ok(kind)
}

/// Everything a library write needs that the fitted kind does not itself carry.
///
/// A struct rather than four more positional parameters: three of the four are `String`-shaped
/// and would be trivially swappable at a call site, which is exactly the mistake a positional
/// list invites (the same reason `IntakeJob` is a struct).
#[derive(Debug)]
pub(super) struct StoredEntryIdentity {
    /// `None` creates a new entry; `Some(id)` rewrites that one in place.
    pub entry_id: Option<String>,
    /// Display name, stored VERBATIM.
    pub name: String,
    pub alpha_assumption: StoredAlphaAssumption,
    /// Search metadata of the chapter this measurement came from, when there is one.
    pub source: Option<StoredSourceRef>,
}

/// Builds the library write request for a fitted kind and the crops it was fitted from.
///
/// The geometry, anchors, signature and calibration verdict are read off the KIND, never
/// restated by the caller, so a request can never describe a model the kind does not have.
/// `samples` must be the very crops `kind` was fitted from — they are the reconstruction
/// source a later load refits from.
#[must_use]
pub(super) fn save_request_from_kind(
    identity: StoredEntryIdentity,
    kind: &WatermarkKind,
    template: Option<RgbaImage>,
    samples: Vec<LibrarySample>,
) -> SaveEntryRequest {
    let StoredEntryIdentity {
        entry_id,
        name,
        alpha_assumption,
        source,
    } = identity;
    SaveEntryRequest {
        entry_id,
        name,
        operator: kind.model().map_or_else(
            || "alpha_blend".to_string(),
            |model| model.operator().id().to_string(),
        ),
        width: kind.template().width(),
        height: kind.template().height(),
        anchors: kind.template().anchors().to_vec(),
        anchor_key: kind.template().anchor_key(),
        alpha_assumption,
        signature: kind.signature().map(|signature| StoredSignature {
            reference_level: signature.reference_level,
            deposit_chroma: signature.deposit_chroma,
            mean_deposit: signature.mean_deposit,
            peak_alpha: signature.peak_alpha,
        }),
        calibration: stored_calibration(kind),
        source,
        template,
        samples,
        planes: kind.model().map(|model| LibraryPlanes {
            c: model.c().to_vec(),
            s: model.s().to_vec(),
        }),
    }
}

/// Drops one calibration crop from a stored entry and returns the rewrite that replaces it.
///
/// Deleting a sample is a REFIT plus a REWRITE, never a file deletion: the entry's `c`/`s`
/// planes were fitted from ALL its crops, so an entry whose crop list changed and whose planes
/// did not would describe a model its own measurements do not support.
///
/// Dropping down to ONE crop is allowed and is not a degenerate case: the engine fits a
/// deposit-exact model from a single known background level, and the stored types express that
/// verdict already. Dropping the LAST one is allowed too and leaves a template-only entry with
/// no model — the caller owns the decision to do that, and this layer does not second-guess it.
///
/// # Errors
/// A user-facing message when `index` is past the end of the entry's sample list, or when the
/// remaining crops cannot be rebuilt into a kind (see [`rebuild_stored_kind`]).
pub(super) fn drop_entry_sample(
    entry: LoadedEntry,
    index: usize,
) -> Result<SaveEntryRequest, String> {
    let LoadedEntry {
        meta,
        template,
        mut samples,
    } = entry;
    if index >= samples.len() {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_sample_index_error",
            index = index + 1,
            count = samples.len()
        ));
    }
    // An entry with no template has no samples either (`validate_entry_dir` enforces exactly
    // that), so the index check above has already refused every call that could reach here.
    // Saying so is cheaper than a panic and is the only honest answer if that ever changes.
    let Some(template) = template else {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_sample_index_error",
            index = index + 1,
            count = samples.len()
        ));
    };
    samples.remove(index);
    let kind = rebuild_stored_kind(
        meta.id.clone(),
        &template,
        (meta.width, meta.height),
        &meta.anchors,
        meta.alpha_assumption,
        &samples,
    )?;
    Ok(save_request_from_kind(
        StoredEntryIdentity {
            entry_id: Some(meta.id),
            name: meta.name,
            alpha_assumption: meta.alpha_assumption,
            // The entry keeps the source list it already has: `save_entry` unions what it is
            // given into the stored one, and an edit discovers no new source.
            source: None,
        },
        &kind,
        Some(template),
        samples,
    ))
}

/// The write request that creates an EMPTY library entry: a name, an id and nothing else.
///
/// This is the entry «+ новый» makes. It exists so the user has something to open, name and
/// add crops to, which is the order they work in — the engine's "one background level cannot
/// separate alpha from the mark" rule belongs to FITTING a model, not to creating the entry
/// that will one day carry one. Nothing about it is invented: no template, no footprint, no
/// anchors, no planes, and the verdict is the engine's own `NotEnoughSamples` with the count it
/// actually has.
///
/// `name` is user data and is stored verbatim.
#[must_use]
pub(super) fn empty_entry_request(name: String) -> SaveEntryRequest {
    SaveEntryRequest {
        entry_id: None,
        name,
        // The operator an entry is fitted under is fixed for this build; recording it now keeps
        // the document self-describing instead of leaving a field that means "ask later".
        operator: alpha_blend_operator().id().to_string(),
        width: 0,
        height: 0,
        anchors: Vec::new(),
        anchor_key: String::new(),
        alpha_assumption: StoredAlphaAssumption::FromDeposit,
        signature: None,
        calibration: StoredCalibration {
            verdict: conditioning_wire(&ModelConditioning::NotEnoughSamples { have: 0, need: 2 })
                .to_string(),
            levels: Vec::new(),
            spread: 0.0,
            samples: 0,
            fit_method: None,
            clamped_pixels: 0,
            alpha: None,
        },
        source: None,
        template: None,
        samples: Vec::new(),
        planes: None,
    }
}

// ---------------------------------------------------------------------------------------
// Card material: the mark on white, and the typed warnings
// ---------------------------------------------------------------------------------------

/// Share of the footprint whose fitted parameters may be clamped before the clamp count is
/// reported as "these samples disagree pixel for pixel".
///
/// NOT an empirical number — nothing in `dev-docs/` measures this quantity — so it is a
/// STRUCTURAL bound and deliberately conservative. The regression fit clamps `s` at `S_CEIL`,
/// and a pixel the mark does not touch sits exactly ON that ceiling: measurement noise pushes
/// its fitted `s` above 1 about half the time, for no reason but noise. Since at most every
/// pixel of the footprint can be such a background pixel, the ceiling effect alone cannot
/// explain a clamped share above one half. Above it, the count is evidence that the samples
/// genuinely disagree. The graded deposit-exact fit clamps far less often (its alpha floor is
/// built so `c` stays non-negative), so the same threshold is merely stricter there than it
/// needs to be — which is the safe direction for a warning.
pub(super) const CLAMPED_SHARE_WARN: f32 = 0.5;

/// Everything a card needs to warn about one stored entry, derived from `entry.json` alone.
///
/// Typed on purpose: the card must not re-run the fit, and it must not parse prose. Every
/// field here is either read straight out of the entry or derived from it by this function, and
/// the wording is left entirely to the UI layer.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct EntryWarnings {
    /// The engine's own graded verdict, rebuilt from the stored calibration record.
    ///
    /// `None` for a verdict tag this build does not know — the card must then show
    /// `EntrySummary::verdict` literally rather than pick the nearest known wording. Note that
    /// `ModelConditioning::NotEnoughSamples` DOES arrive here as a proper variant: "not enough
    /// samples" is a known verdict, not an unknown one.
    pub conditioning: Option<ModelConditioning>,
    /// The entry carries no model at all: `estimate_model` refused when it was written, so
    /// there are no planes and nothing to render. The card falls back to `template.png`.
    pub no_model: bool,
    /// Pixels whose fitted parameters had to be clamped into the physical range.
    pub clamped_pixels: usize,
    /// Footprint the count is out of, `width * height`. 0 only for a degenerate entry.
    pub footprint_pixels: usize,
    /// `clamped_pixels / footprint_pixels`, 0.0 when the footprint is unknown.
    pub clamped_share: f32,
    /// True when `clamped_share` exceeds [`CLAMPED_SHARE_WARN`] — the recorded evidence that
    /// the samples disagree pixel for pixel.
    pub samples_disagree: bool,
    /// Every sample whose background level the user ASSERTED rather than the engine measuring.
    pub manual_backgrounds: Vec<ManualBackgroundRef>,
    /// True when at least one sample rests on an assertion. The entry may then NOT be described
    /// as measured, whatever its verdict tag says — the verdict grades the arithmetic, this
    /// grades the evidence under it.
    pub rests_on_assertion: bool,
    /// The entry has no template at all: it was created EMPTY and has not been given a crop
    /// yet. Different from `no_model`, which is an entry that HAS a mark the fit refused to
    /// model; an empty entry has no mark to show and nothing to be refused about.
    pub is_empty: bool,
    /// How many samples rest on a background measured from a ring the page edge truncated.
    /// Still measurements — fewer, one-sided pixels behind them — so this grades the evidence
    /// the way `rests_on_assertion` does, without ever calling them claims.
    pub partial_rings: usize,
}

/// Builds the card's typed warning material for one listed entry.
///
/// Reads nothing from disk: everything comes from the summary `list_entries` already produced,
/// so a list of entries costs one pass over metadata that is already in memory.
#[must_use]
pub(super) fn entry_warnings(entry: &EntrySummary) -> EntryWarnings {
    let footprint_pixels = (entry.width as usize).saturating_mul(entry.height as usize);
    // Cast justification: both operands are pixel counts of a footprint the chapter mode caps
    // at 512 per side, exactly representable in f32 far beyond that.
    let clamped_share = if footprint_pixels == 0 {
        0.0
    } else {
        entry.clamped_pixels as f32 / footprint_pixels as f32
    };
    EntryWarnings {
        conditioning: conditioning_from_stored(&stored_calibration_of(entry)),
        // `fit_method` is written from `kind.model()`, so its absence is exactly "no model" —
        // the same condition that leaves `planes` unwritten.
        no_model: entry.fit_method.is_none(),
        clamped_pixels: entry.clamped_pixels,
        footprint_pixels,
        clamped_share,
        samples_disagree: clamped_share > CLAMPED_SHARE_WARN,
        manual_backgrounds: entry.manual_backgrounds.clone(),
        rests_on_assertion: !entry.manual_backgrounds.is_empty(),
        is_empty: !entry.has_template,
        partial_rings: entry.partial_rings,
    }
}

/// The calibration record of a listed entry, reassembled from its summary.
///
/// `list_entries` flattens `entry.json`'s calibration into the summary; this puts it back
/// together so the summary can be fed to [`conditioning_from_stored`] without re-reading the
/// file. `fit_method` and `clamped_pixels` are carried through unchanged.
fn stored_calibration_of(entry: &EntrySummary) -> StoredCalibration {
    StoredCalibration {
        verdict: entry.verdict.clone(),
        levels: entry.levels.clone(),
        spread: entry.spread,
        samples: entry.samples,
        fit_method: entry.fit_method.clone(),
        clamped_pixels: entry.clamped_pixels,
        alpha: entry.alpha.clone(),
    }
}

/// Renders one stored entry's mark composited onto a WHITE background.
///
/// This is the picture of the mark itself — what the source would have stamped onto a blank
/// white page — and it is the only view of an entry that shows the mark rather than the page it
/// was cut from. Per pixel per channel it is the operator's forward composite at `B = 255`,
/// which for alpha blending is `c + s*255`, quantized once at the byte write exactly as removal
/// quantizes once. The result is opaque: a composite over an opaque background is what a viewer
/// would see, so the alpha channel is 255 everywhere.
///
/// `Ok(None)` when the entry carries no model — `estimate_model` refused when it was written —
/// so there is nothing to composite and the caller must fall back to `template.png`. No icon is
/// invented for an entry that has no model.
///
/// GUI-free by contract: it returns an `image::RgbaImage`, and uploading it is the caller's.
/// The source is the stored PLANES, not a refit of the calibration crops: a refit would decode
/// the template plus every sample and run the estimator once per entry, whereas this decodes two
/// small PNGs whatever the sample count, which is what a LIST of entries can afford. The planes
/// equal the model to within their 16-bit encoding step, far below the byte this render writes.
///
/// # Errors
/// A user-facing message for an unreadable entry, a plane that is not 16-bit RGB at the
/// template's size, a plane whose length does not match the footprint, or a persisted
/// compositing operator this build does not implement — the last is refused rather than
/// approximated with alpha blending, which would be confidently wrong for a multiply mark.
pub(super) fn render_mark_on_white(entry_id: &str) -> Result<Option<RgbaImage>, String> {
    render_mark_on_white_from(load_entry_planes(entry_id)?)
}

/// [`render_mark_on_white`] against planes already read from an explicit library root.
///
/// Split out so the composite can be exercised against a temporary library instead of the
/// installation's own, the same way every `*_in` function of `watermark_library.rs` is.
fn render_mark_on_white_from(loaded: Option<LoadedPlanes>) -> Result<Option<RgbaImage>, String> {
    let Some(loaded) = loaded else {
        return Ok(None);
    };
    let operator = operator_from_id(&loaded.operator).ok_or_else(|| {
        tf!(
            "cleaning.tools.watermark.chapter.library_operator_error",
            operator = loaded.operator.clone()
        )
    })?;
    compose_planes_on_background(
        loaded.width,
        loaded.height,
        &loaded.planes.c,
        &loaded.planes.s,
        operator.as_ref(),
        255.0,
    )
    .map(Some)
    .map_err(engine_error)
}

/// Composites parameter planes onto one uniform background level and rasterizes the result.
///
/// Shape is validated before a single index is taken, so a malformed plane returns a typed
/// [`WatermarkError`] instead of panicking: the length must be exactly `width*height*3` for both
/// planes, computed with checked arithmetic, and every value must be finite. Values are clamped
/// and rounded ONCE, at the byte write.
///
/// # Errors
/// [`WatermarkError::GeometryMismatch`] for a zero dimension or a footprint whose byte count
/// overflows, [`WatermarkError::BufferLength`] for a plane of the wrong length, and
/// [`WatermarkError::ParameterOutOfRange`] for a non-finite parameter.
fn compose_planes_on_background(
    width: u32,
    height: u32,
    c: &[f32],
    s: &[f32],
    operator: &dyn CompositingOperator,
    background: f32,
) -> Result<RgbaImage, WatermarkError> {
    let values = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .filter(|_| width > 0 && height > 0)
        .ok_or(WatermarkError::GeometryMismatch {
            expected_width: width.max(1),
            expected_height: height.max(1),
            width,
            height,
        })?;
    for (plane, what) in [(c, "stored c plane"), (s, "stored s plane")] {
        if plane.len() != values {
            return Err(WatermarkError::BufferLength {
                what,
                len: plane.len(),
                expected: values,
            });
        }
        if let Some((index, &value)) = plane
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite())
        {
            return Err(WatermarkError::ParameterOutOfRange {
                what,
                index,
                value,
            });
        }
    }
    let mut pixels = Vec::with_capacity(values / 3 * 4);
    for pixel in c.chunks_exact(3).zip(s.chunks_exact(3)) {
        let (c_px, s_px) = pixel;
        for channel in 0..3 {
            // Cast justification: the expression is clamped into 0..=255 and rounded, so it is
            // exactly representable as u8. This is the single quantization of this render.
            pixels.push(
                operator
                    .compose(c_px[channel], s_px[channel], background)
                    .clamp(0.0, 255.0)
                    .round() as u8,
            );
        }
        pixels.push(u8::MAX);
    }
    RgbaImage::from_raw(width, height, pixels).ok_or(WatermarkError::BufferLength {
        what: "composited mark",
        len: values / 3 * 4,
        expected: values / 3 * 4,
    })
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
    /// True when the entry's model rests on at least one HAND-ASSERTED background level, so it
    /// is not a purely measured calibration. It still competes — the fit is the same arithmetic
    /// — but it loses every tie against an entry of the same strength that measured everything.
    pub rests_on_assertion: bool,
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
/// stronger calibration first (a separable entry beats a graded one), then a fully MEASURED
/// entry over one resting on a hand-asserted background level, then the entry whose
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
                    rests_on_assertion: !entry.manual_backgrounds.is_empty(),
                },
                entry.updated_unix,
            ))
        })
        .collect();
    ranked.sort_by(|(left, left_updated), (right, right_updated)| {
        verdict_rank(&right.verdict)
            .cmp(&verdict_rank(&left.verdict))
            // A verdict grades the arithmetic; this grades the evidence under it. Between two
            // entries the verdict cannot separate, the one that measured every background wins
            // over the one that was told what a background was.
            .then_with(|| left.rests_on_assertion.cmp(&right.rests_on_assertion))
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
    /// Image files, in the order the user picked them.
    ///
    /// The footprint of an entry with no template yet is derived from ALL of them at once and the
    /// order therefore cannot change it; the first file only supplies the correlation template and
    /// the alignment reference. When improving an entry that HAS a template the footprint is the
    /// entry's own — re-derived from its mark first if that rectangle is what blocks these crops.
    pub files: Vec<PathBuf>,
    /// Optional background level ASSERTED by the user, one slot per entry of `files`, per
    /// channel in 0..=255.
    ///
    /// It is consulted ONLY for a crop whose ring the engine refused to call flat: a crop that
    /// measures flat keeps its MEASURED level, because a measurement is evidence and an
    /// assertion is not, and nothing here may weaken the automatic test. A shorter vector (the
    /// empty one included) means "no level asserted" for the files it does not reach, so a
    /// caller that has no such UI passes `Vec::new()`.
    pub manual_levels: Vec<Option<[f32; 3]>>,
    /// Background actually present around the mark in each crop, one slot per entry of `files`.
    ///
    /// `None` (and a vector too short to reach a file, the empty one included) means the crop
    /// states nothing about its own geometry, which is the only thing knowable about a file the
    /// user picked. A cutter that had to clip its crop at the image border states the real
    /// per-side margins here, and the intake then takes the footprint it names instead of
    /// assuming a symmetry the crop does not have.
    ///
    /// They are GEOMETRY, not a footprint: they cap a jointly derived footprint, they say which
    /// side may legitimately carry the mark right up to the crop's border (a stated `0` is a page
    /// border the cutter could not get past, anything larger is background it claims to have
    /// left), and they are the background a non-mark-shaped window must keep on each side. They do
    /// NOT opt out of the joint rule: a cutter states what it had to clip, never which rectangle
    /// the mark deserves.
    pub crop_margins: Vec<Option<CropMargins>>,
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

/// Margin the intake insets a supplied crop by to find the mark's footprint, pixels.
///
/// A caller that CUTS a crop for the intake — rather than letting the user pick a file — has
/// to leave exactly this much flat background around the mark, or the footprint it means and
/// the footprint the intake derives will not be the same rectangle. Exported so that rule
/// lives in one place instead of being restated at the cutting site.
///
/// It is also the CEILING of the jointly derived footprint of a new entry: that rule measures
/// the mark itself and may only ever make the footprint smaller than this inset, so a crop whose
/// background is not flat — where the extent measurement carries no information — behaves exactly
/// as it did before the joint rule existed.
///
/// A cutter that could NOT leave the full margin on every side — a mark stamped against the
/// page border — says so with a [`CropMargins`] instead of shrinking the crop silently; see
/// [`ReferenceIntakeRequest::crop_margins`].
#[must_use]
pub(super) fn reference_crop_margin_px(sample_params: &SampleParams) -> u32 {
    sample_params.normalized().ring_width + REFERENCE_RING_MARGIN_SLACK_PX
}

/// How much background one supplied crop actually carries on each side of the mark, pixels.
///
/// The intake's own rule is a SYMMETRIC margin: the footprint is the crop inset by
/// [`reference_crop_margin_px`] all round. That rule cannot describe a mark stamped against an
/// image border, where the background on one side simply does not exist — and the user's marks
/// do sit there. This is how a cutter states the asymmetry it had to accept, so the intake
/// derives the footprint the cutter MEANT rather than one shifted by the missing pixels.
///
/// It is geometry, not permission: the ring is still measured by the engine, still clipped by
/// the crop's own bounds exactly as it would be by the page's, and still admitted on a pixel
/// count rather than on a per-side margin (`validate_calibration_sample`). A crop with too
/// little background left is refused by that count, not by this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CropMargins {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl CropMargins {
    /// The symmetric case: `margin` pixels of background on every side.
    #[must_use]
    pub(super) fn uniform(margin: u32) -> Self {
        Self {
            left: margin,
            top: margin,
            right: margin,
            bottom: margin,
        }
    }

    /// True when any side carries less than `full` pixels of background.
    #[must_use]
    pub(super) fn is_partial(&self, full: u32) -> bool {
        self.left < full || self.top < full || self.right < full || self.bottom < full
    }

    /// The footprint left inside a `width` x `height` crop once these margins are removed, or
    /// `None` when they do not leave a non-empty rectangle.
    #[must_use]
    pub(super) fn footprint_in(&self, width: u32, height: u32) -> Option<(u32, u32)> {
        let w = width.checked_sub(self.left)?.checked_sub(self.right)?;
        let h = height.checked_sub(self.top)?.checked_sub(self.bottom)?;
        (w > 0 && h > 0).then_some((w, h))
    }
}

/// What one supplied crop contributed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ReferenceCropReport {
    /// File name as shown to the user (the last path component).
    pub file: String,
    /// Measured background level under that crop, per channel.
    pub level: [f32; 3],
    /// Per-channel std of the ring the level was measured from, or `None` when `level` was
    /// asserted by the user and no ring was ever measured.
    pub ring_std: Option<[f32; 3]>,
    /// True when `level` is the user's claim rather than a measurement.
    pub manual_level: bool,
    /// True when `level` was measured from a ring the crop's own border truncated — a mark
    /// stamped against the image edge. Still a measurement, from fewer and one-sided pixels.
    pub ring_partial: bool,
    /// Integer offset the crop had to be shifted by to line up with the reference, measured from
    /// where the crop's OWN geometry said the footprint sits — the rectangle a cutter stated, the
    /// image centre for a picked file, or the crop's measured mark extent when the footprint was
    /// derived jointly. Zero for a crop framed the way it was expected to be, and for the crop
    /// that defines the reference.
    pub dx: i32,
    pub dy: i32,
    /// Gradient-magnitude NCC at that offset. 1.0 for the crop that defines the reference.
    pub ncc: f32,
}

/// Which mistake an intake refusal describes.
///
/// The MESSAGE is already user-facing and says what went wrong; this tag says what the user
/// should DO about it, and exists because the surfaces that report a refusal differ in what
/// the user can act on. A crop picked from disk cannot be re-dragged; one cut out of the
/// canvas can, and the library panel therefore appends a gesture-specific hint to these three.
/// [`ReferenceRefusal::Other`] is everything with no such advice — a decode failure, a missing
/// file, a footprint over the size cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReferenceRefusal {
    /// The crop does not cover the footprint plus a full measurement ring on every side.
    TooSmall,
    /// The mark could not be matched inside the crop at the required correlation.
    Misaligned,
    /// The supplied backgrounds do not separate `alpha` from `W` — one level, or too narrow a
    /// spread between them.
    Background,
    /// Everything else. Carries no actionable advice beyond its own message.
    Other,
}

/// Why an intake refused, as the already-localized message plus the tag that classifies it.
///
/// `Display` is the message verbatim, so a caller that only wants to show it need not know
/// this type exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReferenceIntakeError {
    /// Already-localized and user-facing.
    pub message: String,
    pub refusal: ReferenceRefusal,
}

impl ReferenceIntakeError {
    /// A refusal carrying an explicit tag.
    fn tagged(message: String, refusal: ReferenceRefusal) -> Self {
        Self { message, refusal }
    }
}

/// Every refusal raised as a bare message is untagged, which is what lets `?` keep working on
/// the `Result<_, String>` helpers this module is built from.
impl From<String> for ReferenceIntakeError {
    fn from(message: String) -> Self {
        Self {
            message,
            refusal: ReferenceRefusal::Other,
        }
    }
}

impl std::fmt::Display for ReferenceIntakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A successful intake.
#[derive(Debug)]
pub(super) struct ReferenceIntakeOutcome {
    /// Ready to hand to `watermark_library::save_entry`.
    pub request: SaveEntryRequest,
    pub reports: Vec<ReferenceCropReport>,
    /// The verdict the fit reached.
    ///
    /// Always `Separable` when the intake CREATED the entry — it refuses otherwise, because an
    /// entry that cannot separate the model is not worth writing. When it IMPROVED an existing
    /// entry this is whatever the engine reached, `DepositExact` and `NotEnoughSamples`
    /// included: the caller reports the verdict, it does not assume one.
    pub conditioning: ModelConditioning,
    /// `Some((before, after))` when the entry's stored footprint had to be re-derived from the
    /// mark before it could hold the supplied crops — the automatic form of «Trim the footprint».
    ///
    /// The rewrite in `request` already carries the trimmed geometry and the shifted anchors; this
    /// exists so the surface that reports the write can SAY the entry changed shape, instead of
    /// leaving the user to discover it from the card.
    pub trimmed: Option<((u32, u32), (u32, u32))>,
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
/// A [`ReferenceIntakeError`] — an already-localized, user-facing message naming the file at
/// fault where there is one, plus a [`ReferenceRefusal`] tag that classifies it so a caller can
/// add advice specific to where the crop came from. Raised for:
/// - no files, or fewer than two when creating a new entry;
/// - a file that cannot be decoded;
/// - a crop too small to carry a full measurement ring around the footprint, or a footprint
///   above `max_side`;
/// - a crop that could not be aligned with the reference (gradient NCC below the floor);
/// - a crop whose background is NOT uniform AND for which no level was asserted in
///   `manual_levels` — its `B` is then unknown, and feeding it to the estimator would poison
///   `c` and `s`. With an asserted level the crop IS accepted, recorded as
///   [`StoredSampleBackground::Manual`], and every model it contributes to is downgraded by the
///   engine (see [`crate::watermark_chapter::AlphaSource::ManualBackgrounds`]);
/// - crops whose backgrounds do not SEPARATE the model: two crops on the same background
///   cannot tell `alpha` from `W`, and accepting them would produce a confidently wrong
///   model. The message names the level(s) measured and the background to supply instead.
pub(super) fn run_reference_intake(
    request: ReferenceIntakeRequest,
) -> Result<ReferenceIntakeOutcome, ReferenceIntakeError> {
    let ReferenceIntakeRequest {
        files,
        manual_levels,
        crop_margins,
        sample_params,
        base,
        name,
        max_side,
    } = request;
    if files.is_empty() {
        return Err(t!("cleaning.tools.watermark.chapter.reference_no_files_error")
            .to_string()
            .into());
    }
    if base.is_none() && files.len() < 2 {
        return Err(t!("cleaning.tools.watermark.chapter.reference_needs_two_error")
            .to_string()
            .into());
    }
    let sample_params = sample_params.normalized();
    let margin = sample_params.ring_width + REFERENCE_RING_MARGIN_SLACK_PX;
    // What each crop actually carries around the mark. A cutter that clipped at the image
    // border says so; everything else is the symmetric rule.
    let margins_of = |index: usize| -> Option<CropMargins> { crop_margins.get(index).copied().flatten() };

    let images = files
        .iter()
        .map(|path| decode_reference(path))
        .collect::<Result<Vec<_>, _>>()?;

    // An EMPTY entry has no template and no footprint: its geometry is defined by the first
    // crop written into it, exactly as a brand-new entry's is. Everything else about it — its
    // id, its name, its creation time — is kept, which is what makes it an entry being FILLED
    // rather than one being replaced. `base` is rebound below when a stored footprint has to be
    // re-derived before it can hold these crops, so every read of it happens after that point.
    let mut base = base;
    let defines_geometry = base
        .as_ref()
        .and_then(|entry| entry.template.as_ref())
        .is_none();
    // Nothing on disk binds the geometry of an entry that has no template — a brand-new entry, or
    // an empty one waiting for its first crop — so its footprint is derived from all of its crops
    // at once. An entry that HAS a template carries anchors measured against its stored footprint,
    // and moving that footprint without moving them would subtract the mark in the wrong page
    // column; that entry's route to a mark-sized footprint is the trim below, which owns the
    // anchor shift. Stated `crop_margins` do NOT veto the joint rule: they say what the cutter had
    // to clip, which is information about the crop's own frame — it exempts a clipped side from
    // the "this crop cut the mark" test and caps the footprint — never a reason to fall back on a
    // rectangle some drag happened to define.
    //
    // Where the mark sits inside each crop, measured against that crop's own frame. Measured for
    // EVERY intake: the joint rule needs it as its bootstrap, and every other path needs the
    // refusal it raises — a crop that cut the mark is named by SIDE, which no size can say. Empty
    // when the measurement carries no information (see `measure_mark_extents`), in which case the
    // crop-shaped rule stands unchanged.
    let extents = measure_mark_extents(&images, &files, &crop_margins)?.unwrap_or_default();
    let derives_jointly = defines_geometry && !extents.is_empty();

    // Background every crop must leave around the placed window when the footprint is NOT derived
    // from the mark itself. A cutter that clipped at the page border states the real figure; for a
    // picked file the symmetric rule is the only assumption available.
    let stated_bounds =
        |index: usize| -> CropMargins { margins_of(index).unwrap_or_else(|| CropMargins::uniform(margin)) };

    // The footprint size: the entry's own when improving, otherwise derived from the crops.
    let mut footprint = match base.as_ref().filter(|entry| entry.template.is_some()) {
        Some(entry) => (entry.meta.width, entry.meta.height),
        None => {
            let (width, height) = images[0].dimensions();
            let first = margins_of(0).unwrap_or_else(|| CropMargins::uniform(margin));
            // The largest footprint the first crop can carry with the background it states around
            // it. It is the whole rule when no extent could be measured, and the CEILING of the
            // joint rule otherwise: a jointly derived footprint may only ever be smaller, so a
            // crop whose background is not flat — where the extent degenerates to the whole frame
            // — falls back on exactly the behaviour it had before.
            let ceiling = first.footprint_in(width, height).ok_or_else(|| {
                reference_too_small(
                    &files[0],
                    (width, height),
                    (
                        first.left.saturating_add(first.right).saturating_add(1),
                        first.top.saturating_add(first.bottom).saturating_add(1),
                    ),
                )
            })?;
            if derives_jointly {
                joint_footprint(&extents, &files, (width, height), ceiling, margin)?
            } else {
                ceiling
            }
        }
    };
    // True once the footprint states the MARK rather than a rectangle a drag or a detector box
    // defined: it is then placed anywhere inside a crop and its ring is admitted by the engine's
    // own pixel COUNT, exactly as a mark against a page border already is.
    let mut mark_shaped = derives_jointly;

    // A stored footprint no crop can hold is RE-DERIVED from the mark inside the entry's own
    // template before it is reported as a size the user cannot act on. This is the same
    // measurement, the same re-crop and the same anchor shift the manual «Trim the footprint»
    // button performs (`trim_entry_geometry`), and it lands in the same atomic write as the crops
    // being added, so the entry is never observable half-trimmed. It runs only when the stored
    // rectangle is what BLOCKS the crop: a footprint that works is never moved, because moving one
    // silently is exactly what the anchors cannot survive.
    let mut trimmed: Option<((u32, u32), (u32, u32))> = None;
    if !mark_shaped {
        let holds_everywhere = images
            .iter()
            .enumerate()
            .all(|(index, image)| crop_holds(image.dimensions(), footprint, stated_bounds(index)));
        if !holds_everywhere && let Some(entry) = base.as_mut() {
            let trim = match entry.template.as_ref() {
                Some(template) => trim_entry_geometry(
                    &entry.meta.id,
                    template,
                    footprint,
                    &entry.meta.anchors,
                    entry.meta.alpha_assumption,
                    &entry.samples,
                )?,
                // An entry with no template has no stored footprint to re-derive: this is an
                // EMPTY entry whose crops degenerated, so the crop-shaped ceiling stands and the
                // size refusal that follows is the honest one.
                None => EntryTrim::AlreadyTight,
            };
            if let EntryTrim::Trimmed {
                template,
                samples,
                anchors,
                extent,
            } = trim
            {
                ms_log::runtime_log::log_info(format!(
                    "watermark reference intake: entry {} re-derived its footprint from the mark so the supplied crops fit: {}x{} -> {}x{} at offset ({}, {}); every anchor shifted by {}",
                    entry.meta.id,
                    footprint.0,
                    footprint.1,
                    extent.width,
                    extent.height,
                    extent.x,
                    extent.y,
                    extent.x
                ));
                trimmed = Some((footprint, (extent.width, extent.height)));
                footprint = (extent.width, extent.height);
                mark_shaped = true;
                entry.meta.width = extent.width;
                entry.meta.height = extent.height;
                // Literal key format owned by `MarkTemplate::anchor_key`; the written key is
                // rebuilt from the kind, this only keeps the in-memory record self-consistent.
                entry.meta.anchor_key = anchors.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                entry.meta.anchors = anchors;
                entry.template = Some(template);
                entry.samples = samples;
            }
        }
    }
    if footprint.0 > max_side || footprint.1 > max_side {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.selection_too_large_error",
            width = footprint.0,
            height = footprint.1,
            limit = max_side
        )
        .into());
    }

    // The alignment reference: the entry's stored template when improving, otherwise the
    // first crop's own footprint.
    //
    // Both sides of the comparison are the GRADIENT MAGNITUDE, never luma. Between a crop on
    // white and one on black the mark's luminance contrast changes SIGN, so a luminance
    // correlation scores a perfect match as a strong negative and refuses exactly the second
    // background the fit needs. `|grad|` is sign-free and, for a mark of one colour, is the
    // same map scaled: `I = c + s*B` with `c = alpha*W`, `s = 1 - alpha` gives
    // `grad(I) = (W - B)*grad(alpha)`, so only the factor differs and NCC is scale-invariant.
    //
    // Where each crop's search STARTS and how far it may travel is one decision per crop, taken
    // once here: a MARK-SHAPED footprint — derived jointly, or trimmed down to the mark just above
    // — starts on the crop's own measured mark and may go anywhere inside the crop, while a
    // rectangle the crop's geometry states starts at the middle of that rectangle and keeps the
    // background it states on every side.
    let searches: Vec<AlignSearch> = images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let effective = stated_bounds(index);
            match extents.get(index).filter(|_| mark_shaped) {
                Some(extent) => AlignSearch {
                    guess: centre_on_extent(*extent, footprint, image.dimensions()),
                    // The per-side margin is replaced by the engine's own ring PIXEL COUNT: a
                    // window flush against the crop's border is measured from the sides that
                    // exist, exactly as a mark against a page border is.
                    bounds: CropMargins::uniform(0),
                },
                None => AlignSearch {
                    guess: centre_in_stated_rect(image.dimensions(), footprint, effective),
                    bounds: effective,
                },
            }
        })
        .collect();

    let base_template = base.as_ref().and_then(|entry| entry.template.as_ref());
    let reference = match base_template {
        Some(template) => gradient_magnitude(template, PixelRect::new(0, 0, footprint.0, footprint.1)),
        None => {
            // In range: `searches[0]` was built for `images[0]` and a guess never places the
            // footprint outside the crop it was measured in.
            let guess = searches.first().map_or((0, 0), |search| search.guess);
            gradient_magnitude(
                &images[0],
                PixelRect::new(guess.0, guess.1, footprint.0, footprint.1),
            )
        }
    };

    let mut reports = Vec::with_capacity(images.len());
    let mut crops: Vec<LibrarySample> = Vec::with_capacity(images.len());
    let mut template_crop: Option<RgbaImage> = None;
    for (index, image) in images.iter().enumerate() {
        // In range: one search was built per image just above.
        let search = searches.get(index).copied().ok_or_else(|| {
            ReferenceIntakeError::from(
                t!("cleaning.tools.watermark.chapter.reference_no_files_error").to_string(),
            )
        })?;
        let defines_reference = defines_geometry && index == 0;
        let (rect, ncc) = if defines_reference {
            (
                PixelRect::new(search.guess.0, search.guess.1, footprint.0, footprint.1),
                1.0f32,
            )
        } else {
            align_reference_crop(image, &reference, footprint, search).ok_or_else(|| {
                if mark_shaped {
                    reference_joint_too_small(&files[index], image.dimensions(), footprint)
                } else {
                    reference_too_small(
                        &files[index],
                        image.dimensions(),
                        (
                            footprint.0 + search.bounds.left + search.bounds.right,
                            footprint.1 + search.bounds.top + search.bounds.bottom,
                        ),
                    )
                }
            })?
        };
        // Cast justification: both are pixel coordinates of a crop bounded by `max_side` plus a
        // margin, far inside `i32`. The offset is measured from the search's own starting guess —
        // where the crop's geometry said the footprint sits — so it reads as "how far the mark had
        // to be looked for", and is zero for a crop framed the way it was expected to be.
        let (dx, dy) = (
            rect.x as i32 - search.guess.0 as i32,
            rect.y as i32 - search.guess.1 as i32,
        );
        // The two independent measurements of where the mark is must agree within the footprint's
        // own safety border, or the model would be written over planes that are out of register —
        // silently, because a two-sample closed form is exactly determined and the NCC floor
        // grades nothing. Only the joint rule has a second measurement to check against.
        if derives_jointly
            && (dx.unsigned_abs() > REFERENCE_REGISTRATION_DRIFT_PX
                || dy.unsigned_abs() > REFERENCE_REGISTRATION_DRIFT_PX)
        {
            return Err(ReferenceIntakeError::tagged(
                tf!(
                    "cleaning.tools.watermark.chapter.reference_registration_error",
                    file = file_label(&files[index]),
                    dx = dx,
                    dy = dy,
                    limit = REFERENCE_REGISTRATION_DRIFT_PX
                ),
                ReferenceRefusal::Misaligned,
            ));
        }
        if ncc < REFERENCE_MIN_ALIGN_NCC {
            return Err(ReferenceIntakeError::tagged(
                tf!(
                    "cleaning.tools.watermark.chapter.reference_align_error",
                    file = file_label(&files[index]),
                    score = format!("{ncc:.2}"),
                    needed = format!("{REFERENCE_MIN_ALIGN_NCC:.2}")
                ),
                ReferenceRefusal::Misaligned,
            ));
        }
        // Only a crop the automatic test REFUSED may fall back on an asserted level. The test
        // itself is untouched, and a crop that measures flat keeps its measurement even when the
        // user also stated a level: evidence outranks a claim.
        let asserted = manual_levels
            .get(index)
            .copied()
            .flatten()
            .map(normalize_manual_level);
        // Measured on the CROP, whose own border is where the page's border was: the ring the
        // engine clips here is pixel for pixel the ring it would have clipped on the page.
        let background = match validate_calibration_sample(image, rect, &sample_params) {
            SampleVerdict::Calibration {
                level,
                ring_std,
                ring,
            } => stored_flat_background(level, ring_std, ring),
            SampleVerdict::TemplateOnly {
                ring_std,
                ring_max_dev,
                std_limit,
                max_dev_limit,
                ..
            } => match asserted {
                Some(level) => StoredSampleBackground::Manual { level },
                None => {
                    return Err(tf!(
                        "cleaning.tools.watermark.chapter.reference_not_flat_error",
                        file = file_label(&files[index]),
                        std = format!("{:.1}", channel_max(ring_std)),
                        std_limit = format!("{std_limit:.1}"),
                        max_dev = format!("{:.1}", channel_max(ring_max_dev)),
                        max_dev_limit = format!("{max_dev_limit:.1}")
                    )
                    .into());
                }
            },
            SampleVerdict::Unusable {
                reason: SampleRejection::BadRect,
            } => {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.reference_bad_rect_error",
                    file = file_label(&files[index])
                )
                .into());
            }
            SampleVerdict::Unusable {
                reason: SampleRejection::RingTooSmall { pixels, needed },
            } => {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.reference_ring_error",
                    file = file_label(&files[index]),
                    pixels = pixels,
                    needed = needed
                )
                .into());
            }
        };
        let crop = crop_image(image, rect);
        if defines_reference {
            template_crop = Some(crop.clone());
        }
        reports.push(ReferenceCropReport {
            file: file_label(&files[index]),
            level: background.level(),
            ring_std: background.ring_std(),
            manual_level: background.is_manual(),
            ring_partial: background.has_partial_ring(),
            dx,
            dy,
            ncc,
        });
        crops.push(LibrarySample {
            image: crop,
            origin: StoredSampleOrigin::ReferenceCrop,
            background,
        });
    }

    // Build the kind: the entry's own crops first when improving, so a stored measurement is
    // never dropped by an intake that only adds to it.
    let template_source = match (base_template, template_crop.as_ref()) {
        (Some(template), _) => template.clone(),
        (None, Some(crop)) => crop.clone(),
        (None, None) => {
            return Err(t!("cleaning.tools.watermark.chapter.reference_no_files_error")
                .to_string()
                .into());
        }
    };
    // A reference crop carries no page layout, so it cannot know where the source stamps
    // its mark. Column 0 is the neutral placeholder; a chapter scan replaces the whole set
    // through `discover_anchors` before detection runs.
    let anchors: Vec<u32> = base
        .as_ref()
        .map(|entry| entry.meta.anchors.clone())
        .filter(|anchors| !anchors.is_empty())
        .unwrap_or_else(|| vec![0]);
    let alpha_assumption = base
        .as_ref()
        .map_or(StoredAlphaAssumption::FromDeposit, |entry| {
            entry.meta.alpha_assumption
        });

    // The entry's own crops first when improving, so a stored measurement is never dropped by
    // an intake that only adds to it.
    let mut samples: Vec<LibrarySample> = Vec::new();
    if let Some(entry) = base.as_ref() {
        samples.extend(entry.samples.iter().cloned());
    }
    samples.extend(crops);
    let kind = rebuild_stored_kind(
        base.as_ref()
            .map_or_else(|| "reference".to_string(), |entry| entry.meta.id.clone()),
        &template_source,
        footprint,
        &anchors,
        alpha_assumption,
        &samples,
    )?;

    // The engine — not a threshold copied here — decides whether the supplied backgrounds
    // separate `alpha` from `W`.
    //
    // The refusal belongs to BUILDING an entry and not to IMPROVING one. A new entry made of
    // crops that cannot separate the model would be a confidently wrong artefact the user has
    // no reason to keep, so it is refused before it exists. An entry that already exists is a
    // different question: the store accepts a one-sample, non-separable entry deliberately —
    // that is exactly what `drop_entry_sample` writes, and a single known background level
    // still fits a `DepositExact` model whose deposit is measured — so refusing its FIRST crop
    // would leave it permanently unable to take one. The verdict it ends on is whatever the
    // engine reached, and every surface reports that verdict rather than a separability the
    // data does not support.
    let conditioning = kind.conditioning().clone();
    if base.is_none() && !conditioning.is_separable() {
        return Err(describe_spread_refusal(&conditioning));
    }

    let save = save_request_from_kind(
        StoredEntryIdentity {
            entry_id: base.as_ref().map(|entry| entry.meta.id.clone()),
            // User data: whatever the user typed, byte for byte.
            name: base
                .as_ref()
                .map_or_else(|| name.clone(), |entry| entry.meta.name.clone()),
            alpha_assumption,
            // A reference crop has no chapter behind it, so it contributes no search metadata;
            // the writer keeps whatever the entry already recorded.
            source: None,
        },
        &kind,
        Some(template_source),
        samples,
    );
    Ok(ReferenceIntakeOutcome {
        request: save,
        reports,
        conditioning,
        trimmed,
    })
}

/// The refusal for a crop set whose backgrounds do not separate the model.
///
/// Two shapes, because they are two different user mistakes: every crop on ONE background
/// (nothing to separate at all) and crops on backgrounds too close together (not enough
/// contrast to fit the slope). Both name the background to supply instead, which the
/// engine's own `suggested_background()` provides.
///
/// Tagged [`ReferenceRefusal::Background`]: the crop itself is fine — what is missing is one on
/// a DIFFERENT background level.
fn describe_spread_refusal(conditioning: &ModelConditioning) -> ReferenceIntakeError {
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
    let message = if levels.len() < 2 {
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
    };
    ReferenceIntakeError::tagged(message, ReferenceRefusal::Background)
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
///
/// Tagged [`ReferenceRefusal::TooSmall`]: the fix depends on where the crop came from, and only
/// the reporting surface knows that.
fn reference_too_small(path: &Path, size: (u32, u32), needed: (u32, u32)) -> ReferenceIntakeError {
    ReferenceIntakeError::tagged(
        tf!(
            "cleaning.tools.watermark.chapter.reference_too_small_error",
            file = file_label(path),
            width = size.0,
            height = size.1,
            needed_width = needed.0,
            needed_height = needed.1
        ),
        ReferenceRefusal::TooSmall,
    )
}

/// The "crop does not hold the jointly measured mark" refusal.
///
/// Distinct from [`reference_too_small`] because the demand is a different one and stating the
/// wrong demand is what sent the user hunting for a bigger crop in the first place: here the
/// footprint was measured from the MARK, so what the crop is short of is the mark itself plus the
/// ring of background the level is read from — not another crop's rectangle.
///
/// Tagged [`ReferenceRefusal::TooSmall`].
fn reference_joint_too_small(path: &Path, size: (u32, u32), needed: (u32, u32)) -> ReferenceIntakeError {
    ReferenceIntakeError::tagged(
        tf!(
            "cleaning.tools.watermark.chapter.reference_joint_too_small_error",
            file = file_label(path),
            width = size.0,
            height = size.1,
            needed_width = needed.0,
            needed_height = needed.1
        ),
        ReferenceRefusal::TooSmall,
    )
}

/// Largest of three per-channel values.
fn channel_max(values: [f32; 3]) -> f32 {
    values.iter().copied().fold(0.0f32, f32::max)
}

/// Brings a hand-asserted background level into the range an observed sample value can occupy.
///
/// A level is a colour the user pointed at, so it belongs in 0..=255 per channel; a non-finite
/// value is read as mid-grey rather than propagated, because a NaN level would silently poison
/// every fitted pixel of the entry. The engine is not given the chance to see either: this is
/// the one place an asserted number enters the maths.
fn normalize_manual_level(level: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|channel| {
        let value = level[channel];
        if value.is_finite() { value.clamp(0.0, 255.0) } else { 127.5 }
    })
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

/// Where one reference crop's alignment search starts and how far its window may travel.
///
/// Both halves are a decision about GEOMETRY the caller has already taken, carried here together
/// so the search cannot be given a guess from one rule and bounds from another.
#[derive(Debug, Clone, Copy)]
struct AlignSearch {
    /// Window origin the search centres on: where the crop's own geometry says the footprint
    /// sits. [`ReferenceCropReport::dx`] and `dy` are measured from here.
    guess: (u32, u32),
    /// Background the window must leave on each side. [`CropMargins::uniform(0)`] means "anywhere
    /// inside the crop", which is what a jointly derived footprint uses: the ring is then admitted
    /// by the engine's own pixel COUNT rather than by a per-side margin.
    bounds: CropMargins,
}

/// The window origin a crop with no usable mark extent starts from: the centre of the rectangle
/// the crop's own geometry says it cut.
///
/// `margins` is what the cutter stated, or the symmetric default for a file the user picked, so
/// the rectangle is the crop inset by them. Centring inside it is the one rule both cases need:
/// with a footprint exactly as large as that rectangle it IS the rectangle's own origin, and with
/// a smaller footprint — an entry whose stored footprint is tighter than the crop that was cut for
/// it — it is the middle of what was cut rather than its top-left corner.
fn centre_in_stated_rect(size: (u32, u32), footprint: (u32, u32), margins: CropMargins) -> (u32, u32) {
    // Saturating throughout: a footprint larger than the rectangle the margins leave gives a
    // negative ideal origin, which is 0, and the `min` then keeps the window inside the crop.
    let inner_w = size.0.saturating_sub(margins.left).saturating_sub(margins.right);
    let inner_h = size.1.saturating_sub(margins.top).saturating_sub(margins.bottom);
    let x = margins
        .left
        .saturating_add(inner_w.saturating_sub(footprint.0) / 2)
        .min(size.0.saturating_sub(footprint.0));
    let y = margins
        .top
        .saturating_add(inner_h.saturating_sub(footprint.1) / 2)
        .min(size.1.saturating_sub(footprint.1));
    (x, y)
}

/// True when a `size` crop can hold a `footprint`-sized window with `bounds` of background left on
/// every side.
///
/// The same inequality [`align_reference_crop`] refuses on, stated separately because the intake
/// has to ask it BEFORE it searches: a stored footprint that no crop can hold is re-derived from
/// the mark rather than reported as a size the user cannot act on.
fn crop_holds(size: (u32, u32), footprint: (u32, u32), bounds: CropMargins) -> bool {
    let needed_w = footprint.0.saturating_add(bounds.left).saturating_add(bounds.right);
    let needed_h = footprint.1.saturating_add(bounds.top).saturating_add(bounds.bottom);
    size.0 >= needed_w && size.1 >= needed_h
}

/// The window origin that centres `footprint` on a crop's measured mark `extent`, clamped so the
/// window stays inside a `size` crop.
///
/// This is the starting guess the JOINT rule needs and the image centre cannot give: a footprint
/// derived from the mark is much smaller than the crop, and on the measured reference pair the
/// centre guess misses by 26 px — outside [`REFERENCE_ALIGN_SEARCH_PX`] — so the mark would never
/// be found at all.
fn centre_on_extent(extent: PixelRect, footprint: (u32, u32), size: (u32, u32)) -> (u32, u32) {
    // Saturating throughout: a footprint wider than the extent it is centred on gives a negative
    // ideal origin, which is 0, and the clamp then keeps the window inside the crop. A footprint
    // wider than the crop cannot be placed at all and is refused by the caller's size guard.
    let x = (extent.x + extent.width / 2)
        .saturating_sub(footprint.0 / 2)
        .min(size.0.saturating_sub(footprint.0));
    let y = (extent.y + extent.height / 2)
        .saturating_sub(footprint.1 / 2)
        .min(size.1.saturating_sub(footprint.1));
    (x, y)
}

/// Which side of a reference crop cut the mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipSide {
    Left,
    Top,
    Right,
    Bottom,
}

/// The localized noun naming a clipped side, for the refusal message.
fn side_label(side: ClipSide) -> String {
    match side {
        ClipSide::Left => t!("cleaning.tools.watermark.chapter.reference_side_left").to_string(),
        ClipSide::Top => t!("cleaning.tools.watermark.chapter.reference_side_top").to_string(),
        ClipSide::Right => t!("cleaning.tools.watermark.chapter.reference_side_right").to_string(),
        ClipSide::Bottom => t!("cleaning.tools.watermark.chapter.reference_side_bottom").to_string(),
    }
}

/// Measures where the mark sits inside every supplied crop, in that crop's own coordinates.
///
/// This is the bootstrap of the JOINT footprint rule and it deliberately needs no registration:
/// each crop is measured against its OWN frame by the engine's template rule, which is sound
/// exactly here — a mark surrounded by the flat background the crop was cut from — and which errs
/// LARGE by construction, so nothing of the mark is lost by measuring the crops separately. It is
/// run for EVERY intake, not only the joint one: "did this crop cut the mark?" is a question about
/// one crop's own frame, it needs no footprint, and the side it names is the one thing a bare size
/// refusal cannot say.
///
/// `None` when the extents cannot be trusted and the caller must fall back on the crop-shaped
/// rule — see the degeneracy note below. That is not a silent fallback for an unsupported case: it
/// is the engine's own SELF-LIMITING property, where a background that is not flat makes every
/// pixel deviate and the extent comes back as the whole frame, carrying no information about where
/// the mark is.
///
/// `margins` is [`ReferenceIntakeRequest::crop_margins`], and it is consulted for ONE thing: how
/// far from its own border each crop claims its background reaches. A cutter that had to stop at
/// the page border states `0` on that side, and a mark flush against the page legitimately runs
/// into the crop's border there; a cutter that states `m` pixels has claimed the mark ends `m`
/// pixels short of the border, and an extent crossing into that band contradicts the claim. With
/// nothing stated the rule is the crop's own border, which is all that is knowable about a picked
/// file.
///
/// # Errors
/// - [`ReferenceRefusal::Other`] naming the file when a crop holds no mark at all (its frame is
///   uniform), which no footprint can be derived from;
/// - [`ReferenceRefusal::TooSmall`] naming the file AND THE SIDE when the mark's raw extent runs
///   past the background that side claims to carry — the crop cut the mark, and a footprint
///   derived from it would be short on that side for every crop. The RAW extent is what carries
///   that signal: the trimmed one is grown by a safety border that reaches the crop edge routinely
///   and says nothing. A touch on BOTH ends of an axis is the degenerate case above, not a cut,
///   and is never reported as one.
fn measure_mark_extents(
    images: &[RgbaImage],
    files: &[PathBuf],
    margins: &[Option<CropMargins>],
) -> Result<Option<Vec<PixelRect>>, ReferenceIntakeError> {
    let mut extents = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        let (width, height) = image.dimensions();
        let whole = PixelRect::new(0, 0, width, height);
        let extent = mark_extent_from_template(image, whole).map_err(|err| match err {
            // A uniform crop holds no mark, which is the same user mistake as a crop of the wrong
            // picture: the rectangle was not put on the mark. Tagged accordingly, so the panel can
            // give the one instruction that fixes it instead of leaving the message advice-less.
            WatermarkError::FlatTemplate { .. } => ReferenceIntakeError::tagged(
                tf!(
                    "cleaning.tools.watermark.chapter.reference_no_mark_error",
                    file = file_label(&files[index])
                ),
                ReferenceRefusal::Misaligned,
            ),
            other => ReferenceIntakeError::from(engine_error(other)),
        })?;
        // Spanning an axis end to end says the measurement found no background on that axis at
        // all, which a busy crop produces as readily as a mark that fills it. Nothing can be
        // derived from it and nothing may be refused on it.
        let spans_x = extent.x == 0 && extent.right() >= u64::from(width);
        let spans_y = extent.y == 0 && extent.bottom() >= u64::from(height);
        if spans_x || spans_y {
            return Ok(None);
        }
        // The band of background each side claims to carry. `uniform(1)` for an unstated crop is
        // literally "the extent may not touch the border"; a stated `0` waives the test on that
        // side, which is the page border the cutter could not get past.
        let claimed = margins
            .get(index)
            .copied()
            .flatten()
            .unwrap_or_else(|| CropMargins::uniform(1));
        let clipped = [
            (extent.x < claimed.left, ClipSide::Left),
            (extent.y < claimed.top, ClipSide::Top),
            (
                extent.right() > u64::from(width.saturating_sub(claimed.right)),
                ClipSide::Right,
            ),
            (
                extent.bottom() > u64::from(height.saturating_sub(claimed.bottom)),
                ClipSide::Bottom,
            ),
        ]
        .into_iter()
        .find_map(|(touches, side)| touches.then_some(side));
        if let Some(side) = clipped {
            return Err(ReferenceIntakeError::tagged(
                tf!(
                    "cleaning.tools.watermark.chapter.reference_mark_clipped_error",
                    file = file_label(&files[index]),
                    side = side_label(side)
                ),
                ReferenceRefusal::TooSmall,
            ));
        }
        extents.push(extent);
    }
    Ok(Some(extents))
}

/// The footprint a NEW entry takes from ALL of its crops at once, with no designated primary.
///
/// The size is the UNION of the per-crop mark extents — the largest width and the largest height
/// any crop measured — grown by [`FOOTPRINT_TRIM_SAFETY_PX`] of background on each side, which is
/// the same border every trimmed footprint in this project carries and which absorbs both the
/// subpixel shift of a real occurrence and the pixel of uncertainty at an antialiased edge. The
/// union, never an intersection: an intersection needs the registration it is supposed to
/// bootstrap, and it shrinks towards the crop that shows LEAST of the mark, which is the crop that
/// must be reported instead.
///
/// `ceiling` caps it, so a jointly derived footprint can only ever be SMALLER than the one the
/// first crop's own margins allow. That is what keeps a crop whose background is not flat — where
/// the extent degenerates to the whole frame — behaving exactly as it did before.
///
/// # Errors
/// [`ReferenceRefusal::TooSmall`] naming the first crop, its size and the size it would have to
/// have, when the measured mark does not fit inside `ceiling` — the crop holds the mark but not
/// the ring of background the level is measured from.
fn joint_footprint(
    extents: &[PixelRect],
    files: &[PathBuf],
    first_size: (u32, u32),
    ceiling: (u32, u32),
    margin: u32,
) -> Result<(u32, u32), ReferenceIntakeError> {
    let mark = extents.iter().fold((0u32, 0u32), |acc, extent| {
        (acc.0.max(extent.width), acc.1.max(extent.height))
    });
    if mark.0 == 0 || mark.1 == 0 {
        // Unreachable: `measure_mark_extents` never returns an empty extent and the caller has
        // already refused an empty file list. A zero footprint must never reach the engine.
        return Err(t!("cleaning.tools.watermark.chapter.reference_no_files_error")
            .to_string()
            .into());
    }
    if mark.0 > ceiling.0 || mark.1 > ceiling.1 {
        return Err(reference_joint_too_small(
            &files[0],
            first_size,
            (
                mark.0.saturating_add(margin.saturating_mul(2)),
                mark.1.saturating_add(margin.saturating_mul(2)),
            ),
        ));
    }
    // Saturating: both terms are pixel counts of a crop the caller has already bounded.
    let grow = FOOTPRINT_TRIM_SAFETY_PX.saturating_mul(2);
    Ok((
        mark.0.saturating_add(grow).min(ceiling.0),
        mark.1.saturating_add(grow).min(ceiling.1),
    ))
}

/// Finds the integer placement of a `footprint`-sized window inside `image` whose gradient
/// map correlates best with `reference`.
///
/// BOTH sides of the comparison are gradient magnitude — `reference` is built that way by the
/// caller and each candidate window is converted here — which is what lets a crop on a dark
/// background align against a template cut from a light one. A luminance comparison would flip
/// sign between the two and refuse exactly the pairing the fit exists for.
///
/// The window is searched within [`REFERENCE_ALIGN_SEARCH_PX`] of `search.guess` and never leaves
/// less than `search.bounds` of background on any side. `None` when the image is too small to hold
/// the footprint plus those bounds at all.
fn align_reference_crop(
    image: &RgbaImage,
    reference: &[f32],
    footprint: (u32, u32),
    search: AlignSearch,
) -> Option<(PixelRect, f32)> {
    let (width, height) = image.dimensions();
    let margins = search.bounds;
    let needed_w = footprint.0.checked_add(margins.left)?.checked_add(margins.right)?;
    let needed_h = footprint.1.checked_add(margins.top)?.checked_add(margins.bottom)?;
    if width < needed_w || height < needed_h {
        return None;
    }
    // Cast justification: every value here is a pixel coordinate of an image the caller
    // already bounded by `max_side` plus a margin, far inside `i32`.
    let (centre_x, centre_y) = (search.guess.0 as i32, search.guess.1 as i32);
    let min_x = margins.left as i32;
    let min_y = margins.top as i32;
    let max_x = (width - footprint.0 - margins.right) as i32;
    let max_y = (height - footprint.1 - margins.bottom) as i32;

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

// ---------------------------------------------------------------------------------------
// Footprint trim
// ---------------------------------------------------------------------------------------

/// What trimming one stored entry's footprint would change.
#[derive(Debug)]
pub(super) enum FootprintTrimOutcome {
    /// The entry has no mark yet — an EMPTY entry. There is no footprint to measure.
    NoMark,
    /// The stored footprint already ends where the mark does, safety border included.
    AlreadyTight {
        /// The footprint that was measured and kept, pixels.
        footprint: (u32, u32),
    },
    /// The rewrite that replaces the entry with its trimmed self.
    Trimmed {
        /// Ready to hand to [`super::watermark_library::save_entry`], which swaps the whole
        /// directory in atomically. Boxed because it dwarfs every other variant of this enum.
        request: Box<SaveEntryRequest>,
        /// The footprint as stored before the trim.
        before: (u32, u32),
        /// The footprint after it.
        after: (u32, u32),
        /// Where the trimmed footprint sits inside the old one. `offset.0` is exactly what every
        /// anchor column moved by — see [`trim_entry_footprint`].
        offset: (u32, u32),
    },
}

/// Crops `image` to `rect`, refusing rather than panicking on a rect the image does not contain.
///
/// # Errors
/// A user-facing message when `rect` leaves `image`, which would be a geometry bug in the caller
/// rather than a property of the entry.
fn crop_to_rect(image: &RgbaImage, rect: PixelRect) -> Result<RgbaImage, String> {
    let (width, height) = image.dimensions();
    let fits = rect.width > 0
        && rect.height > 0
        && rect.right() <= u64::from(width)
        && rect.bottom() <= u64::from(height);
    if !fits {
        return Err(engine_error(WatermarkError::RectOutOfPage {
            rect,
            width,
            height,
        }));
    }
    Ok(RgbaImage::from_fn(rect.width, rect.height, |x, y| {
        // In range: the rect was just proven to lie inside the image.
        *image.get_pixel(rect.x + x, rect.y + y)
    }))
}

/// Re-derives one stored entry's footprint from the mark it actually holds and returns the rewrite
/// that replaces it, or says the entry is already tight.
///
/// WHY this exists: a footprint that came from the chapter detector is the detector's box, not the
/// mark. The measured case is an entry whose stored footprint is 319x236 around a mark occupying
/// 127x34 — 98% of its `c`/`s` planes are "fully transparent", every future reference crop is
/// asked to cover 319x236 plus a measurement ring, and the mark reads as flush against the right
/// page edge (anchor 480 + width 319 = 799 of 800) when it really ends 12 px short of it. Trimming
/// fixes all three from one cause.
///
/// ANCHOR CORRECTNESS is the invariant this function exists to keep. A stored anchor is the PAGE
/// COLUMN of the footprint's left edge, so moving that edge right by `offset.0` moves every anchor
/// by exactly the same amount; the vertical offset moves nothing persisted, because a mark's
/// anchor set is columns only (`MarkTemplate::anchors`). With both applied, an occurrence found
/// with the trimmed template lands at the same ABSOLUTE page pixels as one found with the old one,
/// which is what stops a trimmed entry from subtracting the mark in the wrong place.
///
/// The extent is measured from the fitted MODEL when the entry has samples to fit one from, and
/// from the TEMPLATE against its own border level otherwise; the engine owns both rules and both
/// err large (`trimmed_footprint_from_model`, `trimmed_footprint_from_template`). The model is
/// preferred where it exists because a crop cut from a busy page tells nothing about faint deposit,
/// while `c`/`s` do.
///
/// The rewrite is a full REFIT: the template and every calibration crop are re-cropped to the new
/// footprint, and the `c`/`s` planes are re-fitted from the re-cropped crops rather than being
/// cropped themselves, so the planes can never describe a geometry the samples do not have. The
/// entry keeps its id, its name, its creation time and its sources.
///
/// # Errors
/// A user-facing message when the stored template does not match the stored footprint, when a
/// stored crop does not, when the mark's extent cannot be measured (a template holding no mark at
/// all), or when the re-cropped material cannot be rebuilt into a kind.
pub(super) fn trim_entry_footprint(entry: LoadedEntry) -> Result<FootprintTrimOutcome, String> {
    let LoadedEntry {
        meta,
        template,
        samples,
    } = entry;
    let Some(template) = template else {
        // An EMPTY entry: a name and an id, nothing to measure. Not a failure.
        return Ok(FootprintTrimOutcome::NoMark);
    };
    let footprint = (meta.width, meta.height);
    match trim_entry_geometry(&meta.id, &template, footprint, &meta.anchors, meta.alpha_assumption, &samples)? {
        EntryTrim::AlreadyTight => Ok(FootprintTrimOutcome::AlreadyTight { footprint }),
        EntryTrim::Trimmed {
            template: trimmed_template,
            samples: trimmed_samples,
            anchors,
            extent,
        } => {
            let kind = rebuild_stored_kind(
                meta.id.clone(),
                &trimmed_template,
                (extent.width, extent.height),
                &anchors,
                meta.alpha_assumption,
                &trimmed_samples,
            )?;
            let request = save_request_from_kind(
                StoredEntryIdentity {
                    entry_id: Some(meta.id),
                    name: meta.name,
                    alpha_assumption: meta.alpha_assumption,
                    // A trim discovers no new source: `save_entry` keeps the stored list as it is.
                    source: None,
                },
                &kind,
                Some(trimmed_template),
                trimmed_samples,
            );
            Ok(FootprintTrimOutcome::Trimmed {
                request: Box::new(request),
                before: footprint,
                after: (extent.width, extent.height),
                offset: (extent.x, extent.y),
            })
        }
    }
}

/// The GEOMETRY half of a footprint trim: one entry's material re-cropped to the mark it holds.
///
/// Separate from [`trim_entry_footprint`] because two callers need it and only one of them wants a
/// `SaveEntryRequest`. The manual button rebuilds the kind and writes the entry; the reference
/// intake ADOPTS this geometry as the base it is about to append crops to, so a trim and the new
/// samples land in ONE atomic write rather than two, and the entry is never observable in a state
/// where its planes and its crops disagree.
#[derive(Debug)]
enum EntryTrim {
    /// The stored footprint already ends where the mark does, safety border included.
    AlreadyTight,
    /// The entry re-cropped to the mark's own extent.
    Trimmed {
        template: RgbaImage,
        samples: Vec<LibrarySample>,
        /// Every stored anchor shifted by `extent.x` — see [`trim_entry_footprint`] for why that
        /// offset, and nothing else, is what moves.
        anchors: Vec<u32>,
        /// Where the trimmed footprint sits inside the stored one, in footprint-local pixels.
        extent: PixelRect,
    },
}

/// Measures where the mark really ends inside a stored entry and re-crops the entry to it.
///
/// `footprint` is the entry's stored `(width, height)`, which the template and every sample must
/// match exactly. The extent is measured from the fitted MODEL when the samples can fit one and
/// from the TEMPLATE against its own border level otherwise; the engine owns both rules and both
/// err LARGE by construction, so this can never cut signal, and on a background too busy to
/// measure against it yields no trim at all rather than a wrong one. It is idempotent: the safety
/// border it leaves is wider than the ring the measurement reads, so a second pass measures the
/// same rectangle and stands down as [`EntryTrim::AlreadyTight`].
///
/// # Errors
/// A user-facing message when the stored template or a stored crop does not match `footprint`,
/// when the mark's extent cannot be measured (a template holding no mark at all), when the refit
/// needed to measure it fails, or when shifting an anchor by the trim offset would leave the page.
fn trim_entry_geometry(
    id: &str,
    template: &RgbaImage,
    footprint: (u32, u32),
    anchors: &[u32],
    alpha_assumption: StoredAlphaAssumption,
    samples: &[LibrarySample],
) -> Result<EntryTrim, String> {
    if template.dimensions() != footprint {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_trim_geometry_error",
            width = template.width(),
            height = template.height(),
            expected_width = footprint.0,
            expected_height = footprint.1
        ));
    }
    for sample in samples {
        if sample.image.dimensions() != footprint {
            return Err(tf!(
                "cleaning.tools.watermark.chapter.library_trim_geometry_error",
                width = sample.image.width(),
                height = sample.image.height(),
                expected_width = footprint.0,
                expected_height = footprint.1
            ));
        }
    }

    // Measured on the entry AS STORED. A model needs a refit, which is a decode-free fit over the
    // already-decoded crops; it is the better measurement because `c`/`s` state the deposit
    // itself, while the template only states how one crop differs from its own border.
    let fitted = rebuild_stored_kind(id.to_string(), template, footprint, anchors, alpha_assumption, samples)?;
    let extent = match fitted.model() {
        Some(model) => trimmed_footprint_from_model(model).map_err(engine_error)?,
        None => trimmed_footprint_from_template(template, PixelRect::new(0, 0, footprint.0, footprint.1)).map_err(engine_error)?,
    };
    if extent.width == footprint.0 && extent.height == footprint.1 {
        return Ok(EntryTrim::AlreadyTight);
    }

    let trimmed_template = crop_to_rect(template, extent)?;
    let trimmed_samples = samples
        .iter()
        .map(|sample| {
            crop_to_rect(&sample.image, extent).map(|image| LibrarySample {
                image,
                origin: sample.origin,
                background: sample.background,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The anchor is the page column of the footprint's LEFT EDGE, so the whole set shifts with it.
    // Checked: a page column plus a trim offset cannot overflow a real page, and an overflow here
    // would silently point removal at the wrong column.
    let shifted = anchors
        .iter()
        .map(|&anchor| {
            anchor.checked_add(extent.x).ok_or_else(|| {
                tf!(
                    "cleaning.tools.watermark.chapter.library_trim_anchor_error",
                    anchor = anchor,
                    offset = extent.x
                )
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(EntryTrim::Trimmed {
        template: trimmed_template,
        samples: trimmed_samples,
        anchors: shifted,
        extent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::watermark_library::{EntryFile, StoredSourceRef};
    use crate::watermark_chapter::AlphaBlend;
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

    /// Installs the embedded English catalog for the tests that compare localized text, and
    /// holds the process-global locale lock for the test's lifetime — the active locale is
    /// process-global and other tests in this binary switch it.
    fn locale_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
        guard
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("manhwastudio-wm-reference-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn intake(files: Vec<PathBuf>) -> ReferenceIntakeRequest {
        ReferenceIntakeRequest {
            crop_margins: Vec::new(),
            files,
            manual_levels: Vec::new(),
            sample_params: SampleParams::default(),
            base: None,
            name: "  Знак  ".to_string(),
            max_side: 512,
        }
    }

    // -----------------------------------------------------------------------------------
    // Footprint trim
    // -----------------------------------------------------------------------------------

    /// A crop of `size` on a flat `level` background carrying an opaque `block`, which is the
    /// shape of the user's own template: a mark on empty page inside a loose detector box.
    fn template_with_block(size: (u32, u32), block: PixelRect, level: u8, ink: u8) -> RgbaImage {
        RgbaImage::from_fn(size.0, size.1, |x, y| {
            let inside = x >= block.x
                && y >= block.y
                && u64::from(x) < block.right()
                && u64::from(y) < block.bottom();
            let value = if inside { ink } else { level };
            Rgba([value, value, value, 255])
        })
    }

    /// An in-memory `LoadedEntry` with no calibration crops — a TEMPLATE-ONLY entry, which is
    /// exactly what the chapter scanner writes and what the user has on disk.
    fn loaded_entry(id: &str, template: Option<RgbaImage>, anchors: Vec<u32>) -> LoadedEntry {
        let (width, height) = template
            .as_ref()
            .map_or((0, 0), image::GenericImageView::dimensions);
        LoadedEntry {
            meta: EntryFile {
                format: 1,
                id: id.to_string(),
                name: "Знак 1".to_string(),
                created_unix: 1_786_790_637,
                updated_unix: 1_789_212_512,
                operator: "alpha_blend".to_string(),
                width,
                height,
                anchor_key: anchors
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                anchors,
                alpha_assumption: StoredAlphaAssumption::FromDeposit,
                signature: None,
                calibration: StoredCalibration {
                    verdict: "not_enough_samples".to_string(),
                    levels: Vec::new(),
                    spread: 0.0,
                    samples: 0,
                    fit_method: None,
                    clamped_pixels: 0,
                    alpha: None,
                },
                sources: Vec::new(),
                samples: Vec::new(),
                template: template.as_ref().map(|_| "template.png".to_string()),
                planes: None,
            },
            template,
            samples: Vec::new(),
        }
    }

    /// The user's own entry, to the pixel, and the refusal it caused.
    ///
    /// A 319x236 footprint around a mark occupying 127x34 demanded a 327x244 reference crop; the
    /// user's 224x170 drag was refused as too small, and at anchor 480 on an 800 px page the
    /// footprint read as flush against the right edge. After the trim the entry demands 143x50,
    /// the same drag is accepted, and the mark ends 796 px in.
    #[test]
    fn the_users_entry_trims_and_then_accepts_the_crop_it_refused() {
        let dir = fixture_dir("trim-users-entry");
        let mark = PixelRect::new(182, 110, 127, 34);
        let template = template_with_block((319, 236), mark, 255, 40);
        let entry = loaded_entry("wm-users", Some(template.clone()), vec![480]);

        // The crop the user actually dragged: 224x170 with the mark near its centre.
        let crop = {
            let mut image = RgbaImage::from_pixel(224, 170, Rgba([255, 255, 255, 255]));
            for y in 0..mark.height {
                for x in 0..mark.width {
                    image.put_pixel(48 + x, 68 + y, Rgba([40, 40, 40, 255]));
                }
            }
            image
        };
        let file = write_reference(&dir, "drag.png", &crop);

        // THE CROP IS ACCEPTED, and the entry trims itself to make it fit. The stored 319x236
        // rectangle is 98% empty border; demanding that a drag cover it was a number the user
        // could not act on, so the intake re-derives the footprint from the mark inside the
        // entry's own template — the same measurement, the same anchor shift, one atomic write.
        let accepted = run_reference_intake(ReferenceIntakeRequest {
            files: vec![file.clone()],
            manual_levels: Vec::new(),
            crop_margins: Vec::new(),
            sample_params: SampleParams::default(),
            base: Some(loaded_entry("wm-users", Some(template.clone()), vec![480])),
            name: String::new(),
            max_side: 512,
        })
        .expect("the drag covers the mark and a ring, which is all the entry may demand");
        assert_eq!(
            accepted.trimmed,
            Some(((319, 236), (135, 42))),
            "and it says so: the entry changed shape in the same write"
        );
        assert_eq!((accepted.request.width, accepted.request.height), (135, 42));
        // ANCHORS: the stored column is the page column of the footprint's LEFT edge, so it moved
        // by exactly the trim's horizontal offset and by nothing else. An occurrence found with
        // the rewritten entry therefore lands on the same absolute page pixels as before.
        assert_eq!(accepted.request.anchors, vec![480 + 178]);
        assert_eq!(accepted.request.anchor_key, "658");
        assert_eq!(accepted.request.samples.len(), 1, "and the crop became a sample");
        assert_eq!(
            accepted.request.samples[0].image.dimensions(),
            (135, 42),
            "cut to the trimmed footprint, like the template"
        );

        // The SAME drag arriving from the CANVAS, which states the background it carries. Stated
        // margins say what the cutter had to clip; they neither block the trim nor change it.
        let canvas = run_reference_intake(ReferenceIntakeRequest {
            files: vec![file.clone()],
            manual_levels: Vec::new(),
            crop_margins: vec![Some(CropMargins::uniform(4))],
            sample_params: SampleParams::default(),
            base: Some(loaded_entry("wm-users", Some(template.clone()), vec![480])),
            name: String::new(),
            max_side: 512,
        })
        .expect("the canvas lane must reach the same answer as the picker lane");
        assert_eq!(canvas.trimmed, Some(((319, 236), (135, 42))));
        assert_eq!(canvas.request.anchors, vec![658]);

        // A stored footprint that the crop DOES hold is never moved: the trim runs only when the
        // rectangle is what blocks the crop.
        let roomy = {
            // The mark placed where a 319x236 window CAN reach it: the window's own origin
            // (40, 32) plus the mark's offset inside the stored template.
            let mut image = RgbaImage::from_pixel(400, 300, Rgba([255, 255, 255, 255]));
            for y in 0..mark.height {
                for x in 0..mark.width {
                    image.put_pixel(40 + mark.x + x, 32 + mark.y + y, Rgba([40, 40, 40, 255]));
                }
            }
            image
        };
        let roomy_file = write_reference(&dir, "roomy.png", &roomy);
        let untouched = run_reference_intake(ReferenceIntakeRequest {
            files: vec![roomy_file],
            manual_levels: Vec::new(),
            crop_margins: Vec::new(),
            sample_params: SampleParams::default(),
            base: Some(loaded_entry("wm-users", Some(template), vec![480])),
            name: String::new(),
            max_side: 512,
        })
        .expect("a crop large enough for the stored footprint needs no trim");
        assert_eq!(untouched.trimmed, None);
        assert_eq!((untouched.request.width, untouched.request.height), (319, 236));
        assert_eq!(untouched.request.anchors, vec![480]);

        // The manual button performs exactly the same trim, and still must.
        let outcome = trim_entry_footprint(entry).expect("the template holds a mark");
        let FootprintTrimOutcome::Trimmed {
            request,
            before,
            after,
            offset,
        } = outcome
        else {
            panic!("a 319x236 footprint around a 127x34 mark is not tight: {outcome:?}");
        };
        assert_eq!(before, (319, 236));
        assert_eq!(after, (135, 42));
        assert_eq!(offset, (178, 106));
        // The anchor moved with the footprint's left edge, and nowhere else.
        assert_eq!(request.anchors, vec![480 + 178]);
        assert_eq!(request.anchor_key, "658");
        assert_eq!((request.width, request.height), (135, 42));
        let trimmed_template = request.template.clone().expect("the trim keeps a template");
        assert_eq!(trimmed_template.dimensions(), (135, 42));
        // The mark's real extent survived, with the safety border around it.
        assert!(offset.0 <= mark.x && offset.1 <= mark.y);
        assert!(u64::from(offset.0 + after.0) >= mark.right());
        assert!(u64::from(offset.1 + after.1) >= mark.bottom());
        // And the mark no longer reads as flush against the right edge of an 800 px page.
        assert_eq!(480 + 319, 799);
        assert_eq!(658 + 135, 793);

        // After the manual trim the same drag is accepted with nothing left to move.
        let outcome = run_reference_intake(ReferenceIntakeRequest {
            files: vec![file],
            manual_levels: Vec::new(),
            crop_margins: Vec::new(),
            sample_params: SampleParams::default(),
            base: Some(loaded_entry("wm-users", Some(trimmed_template), vec![658])),
            name: String::new(),
            max_side: 512,
        })
        .expect("a 224x170 crop carries a 135x42 footprint plus its ring with room to spare");
        assert_eq!(
            outcome.trimmed, None,
            "a footprint that already ends where the mark does is not trimmed twice"
        );
        assert_eq!(outcome.request.samples.len(), 1);
        assert_eq!(outcome.reports.len(), 1);
        assert!(
            !outcome.reports[0].manual_level && outcome.reports[0].ring_std.is_some(),
            "the crop's own ring is flat white and must be MEASURED: {:?}",
            outcome.reports[0]
        );
    }

    /// MIGRATION: an entry that is already on disk, written before the footprint was measured,
    /// must become trimmed, stay readable, and keep removing exactly what it removed before.
    ///
    /// "Keeps removing the same thing" is checked where it matters — the fitted `c`/`s` at every
    /// pixel of the trimmed footprint, against the same pixel of the untrimmed one. Those two
    /// planes ARE the removal: `B = (I - c)/s`.
    #[test]
    fn an_entry_on_disk_migrates_to_a_trimmed_footprint_and_removes_the_same_pixels() {
        let root = fixture_dir("trim-migration");
        std::fs::create_dir_all(&root).expect("create fixture library");
        let footprint = (200u32, 120u32);
        let mark = PixelRect::new(80, 50, 24, 16);
        // The mark as the detector's loose box saw it: dark ink on flat white page.
        let observed = template_with_block(footprint, mark, 255, 40);
        let untrimmed = SaveEntryRequest {
            entry_id: Some("wm-migrate".to_string()),
            name: "Знак 1".to_string(),
            operator: "alpha_blend".to_string(),
            width: footprint.0,
            height: footprint.1,
            anchors: vec![480],
            anchor_key: "480".to_string(),
            alpha_assumption: StoredAlphaAssumption::FromDeposit,
            signature: None,
            calibration: StoredCalibration {
                verdict: "deposit_exact".to_string(),
                levels: vec![255.0],
                spread: 0.0,
                samples: 1,
                fit_method: Some("deposit_exact".to_string()),
                clamped_pixels: 0,
                alpha: None,
            },
            source: None,
            template: Some(observed.clone()),
            samples: vec![LibrarySample {
                image: observed,
                origin: StoredSampleOrigin::ReferenceCrop,
                background: StoredSampleBackground::Flat {
                    level: [255.0; 3],
                    ring_std: [0.0; 3],
                    ring_pixels: None,
                    ring_full_pixels: None,
                },
            }],
            planes: None,
        };
        let id = super::super::watermark_library::save_entry_in(&root, &untrimmed)
            .expect("the fixture entry is written");

        let stored = super::super::watermark_library::load_entry_in(&root, &id).expect("load");
        let before_kind = rebuild_stored_kind(
            id.clone(),
            stored.template.as_ref().expect("template"),
            (stored.meta.width, stored.meta.height),
            &stored.meta.anchors,
            stored.meta.alpha_assumption,
            &stored.samples,
        )
        .expect("the stored entry rebuilds");
        let before_model = before_kind.model().expect("one flat level still fits a model");
        let (before_c, before_s) = (before_model.c().to_vec(), before_model.s().to_vec());

        // The migration itself: measure, rewrite, reload.
        let FootprintTrimOutcome::Trimmed {
            request,
            before,
            after,
            offset,
        } = trim_entry_footprint(stored).expect("the entry carries a mark")
        else {
            panic!("a 200x120 footprint around a 24x16 mark is not tight");
        };
        assert_eq!(before, footprint);
        assert_eq!(after, (32, 24));
        assert_eq!(offset, (76, 46));
        super::super::watermark_library::save_entry_in(&root, &request).expect("rewrite");

        // Still a valid entry directory, still readable, and its metadata now states the
        // trimmed geometry and the shifted anchor.
        let dir = root.join(&id);
        super::super::watermark_library::validate_entry_dir(&dir)
            .expect("a trimmed entry is still a valid entry directory");
        let migrated = super::super::watermark_library::load_entry_in(&root, &id).expect("reload");
        assert_eq!((migrated.meta.width, migrated.meta.height), after);
        assert_eq!(migrated.meta.anchors, vec![480 + offset.0]);
        assert_eq!(migrated.meta.anchor_key, "556");
        assert_eq!(migrated.samples.len(), 1);
        assert_eq!(migrated.samples[0].image.dimensions(), after);

        // And the removal it licenses is unchanged where the mark actually is.
        let after_kind = rebuild_stored_kind(
            id.clone(),
            migrated.template.as_ref().expect("template"),
            (migrated.meta.width, migrated.meta.height),
            &migrated.meta.anchors,
            migrated.meta.alpha_assumption,
            &migrated.samples,
        )
        .expect("the migrated entry rebuilds");
        let after_model = after_kind.model().expect("model");
        assert_eq!((after_model.width(), after_model.height()), after);
        for y in 0..after.1 as usize {
            for x in 0..after.0 as usize {
                let from = (((offset.1 as usize + y) * footprint.0 as usize)
                    + offset.0 as usize
                    + x)
                    * 3;
                let to = (y * after.0 as usize + x) * 3;
                for channel in 0..3 {
                    assert!(
                        (before_c[from + channel] - after_model.c()[to + channel]).abs() <= 0.01,
                        "c moved at ({x}, {y}) channel {channel}: {} -> {}",
                        before_c[from + channel],
                        after_model.c()[to + channel]
                    );
                    assert!(
                        (before_s[from + channel] - after_model.s()[to + channel]).abs() <= 0.0001,
                        "s moved at ({x}, {y}) channel {channel}: {} -> {}",
                        before_s[from + channel],
                        after_model.s()[to + channel]
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A second trim of an entry that has already been trimmed must change nothing at all.
    #[test]
    fn a_trimmed_entry_is_idempotent_under_a_second_trim() {
        let template = template_with_block((319, 236), PixelRect::new(182, 110, 127, 34), 255, 40);
        let first = trim_entry_footprint(loaded_entry("wm-a", Some(template), vec![480]))
            .expect("first trim");
        let FootprintTrimOutcome::Trimmed { request, .. } = first else {
            panic!("expected a trim");
        };
        let again = trim_entry_footprint(loaded_entry(
            "wm-a",
            request.template.clone(),
            request.anchors.clone(),
        ))
        .expect("second trim");
        assert!(
            matches!(
                again,
                FootprintTrimOutcome::AlreadyTight {
                    footprint: (135, 42)
                }
            ),
            "a trimmed entry must already be tight: {again:?}"
        );
    }

    /// An EMPTY entry has no mark and therefore no footprint to measure. That is a state, not a
    /// failure, and it must never become a zero-sized footprint.
    #[test]
    fn an_empty_entry_has_nothing_to_trim() {
        let outcome = trim_entry_footprint(loaded_entry("wm-empty", None, Vec::new()))
            .expect("an empty entry is a legitimate state");
        assert!(matches!(outcome, FootprintTrimOutcome::NoMark), "{outcome:?}");
    }

    /// A template holding no mark at all is REFUSED, with the engine's own reason, rather than
    /// trimmed to nothing.
    #[test]
    fn a_blank_template_is_refused_rather_than_trimmed_to_nothing() {
        let blank = RgbaImage::from_pixel(60, 40, Rgba([255, 255, 255, 255]));
        let err = trim_entry_footprint(loaded_entry("wm-blank", Some(blank), vec![10]))
            .expect_err("a template with no mark cannot be measured");
        assert!(!err.is_empty(), "the refusal must carry a reason");
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
        assert_eq!(
            outcome
                .conditioning
                .alpha_uncertainty()
                .expect("a separable verdict quotes its alpha")
                .source,
            AlphaSource::SeparatedBackgrounds,
            "an entry built only from measurements keeps today's verdict"
        );
        assert!(outcome.reports.iter().all(|report| !report.manual_level));
        // The display name is user data and is never trimmed on the way in.
        assert_eq!(outcome.request.name, "  Знак  ");
        // The entry's footprint is the MARK's own measured extent plus one safety border on each
        // side, derived from every crop at once — NOT the first crop's rectangle, which is only
        // ever a statement about how the user happened to frame it. The synthetic mark occupies
        // the middle half of the drawn footprint in both axes.
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            (
                footprint.0 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2,
                footprint.1 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2
            ),
            "the footprint is the jointly measured mark extent plus its safety border"
        );
        assert!(
            outcome.reports.iter().all(|report| report.dx == 0 && report.dy == 0),
            "identically framed crops need no shift"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------------------
    // Joint footprint derivation
    // -----------------------------------------------------------------------------------

    /// The user's own reference crops, when they are on this machine.
    ///
    /// `test/` is a working directory, not a published fixture path, so every test below is a
    /// no-op when the files are absent — it must never fail for a checkout that does not carry
    /// them. What they pin cannot be synthesised: a real mark with a soft glow, cut twice by
    /// hand at two different sizes on two different backgrounds.
    fn users_reference_crops(names: [&str; 2]) -> Option<[PathBuf; 2]> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/водяные метки");
        let files = names.map(|name| dir.join(name));
        files.iter().all(|path| path.is_file()).then_some(files)
    }

    /// THE ACCEPTANCE TEST: both of the user's marks build, from either crop first.
    ///
    /// Seven of the eight combinations of these four files were refused `TooSmall` before the
    /// footprint was derived jointly, and mark 2 was refused in BOTH orders — its crops are
    /// 259x258 and 226x266, so neither covers the other and no ordering could ever satisfy a
    /// rule shaped like "every later crop must be at least as large as the first".
    #[test]
    fn the_users_real_crops_build_both_marks_in_every_order() {
        for (names, expected) in [
            (["1_w.png", "1_b.png"], (176u32, 58u32)),
            (["2_w.png", "2_b.png"], (180, 217)),
        ] {
            let Some([first, second]) = users_reference_crops(names) else {
                continue;
            };
            for files in [
                vec![first.clone(), second.clone()],
                vec![second.clone(), first.clone()],
            ] {
                let order = format!("{:?}", files.iter().map(|f| file_label(f)).collect::<Vec<_>>());
                let outcome = run_reference_intake(intake(files))
                    .unwrap_or_else(|err| panic!("{order} must build: {}", err.message));
                assert_eq!(
                    (outcome.request.width, outcome.request.height),
                    expected,
                    "{order}: the footprint is the union of the crops' own mark extents plus one \
                     safety border, whichever crop came first"
                );
                assert!(
                    outcome.conditioning.is_separable(),
                    "{order}: white and black separate the model: {:?}",
                    outcome.conditioning
                );
                for report in &outcome.reports {
                    assert!(
                        report.ncc >= REFERENCE_MIN_ALIGN_NCC,
                        "{order}: {} scored {}",
                        report.file,
                        report.ncc
                    );
                    // The alignment reached an optimum the crop's own measurement agrees with,
                    // rather than settling wherever a collapsed search window left it.
                    assert!(
                        report.dx.unsigned_abs() <= REFERENCE_REGISTRATION_DRIFT_PX
                            && report.dy.unsigned_abs() <= REFERENCE_REGISTRATION_DRIFT_PX,
                        "{order}: {} landed {},{} from its own extent",
                        report.file,
                        report.dx,
                        report.dy
                    );
                }
            }
        }
    }

    /// Neither crop covers the other, which is the shape of the user's second mark and was
    /// unbuildable in either order. The mark is what the footprint comes from, so both fit.
    #[test]
    fn a_pair_where_neither_crop_covers_the_other_is_accepted() {
        let dir = fixture_dir("neither-covers");
        let footprint = (40u32, 32u32);
        let colour = [20.0, 30.0, 25.0];
        // Wide and short against narrow and tall: each crop dominates one axis only.
        let wide = write_reference(
            &dir,
            "wide.png",
            &asymmetric_reference(
                footprint,
                CropMargins { left: 30, top: 4, right: 30, bottom: 4 },
                [255, 255, 255],
                colour,
            ),
        );
        let tall = write_reference(
            &dir,
            "tall.png",
            &asymmetric_reference(
                footprint,
                CropMargins { left: 4, top: 30, right: 4, bottom: 30 },
                [0, 0, 0],
                colour,
            ),
        );
        for files in [vec![wide.clone(), tall.clone()], vec![tall, wide]] {
            let outcome = run_reference_intake(intake(files))
                .unwrap_or_else(|err| panic!("both orders must build: {}", err.message));
            assert_eq!(
                (outcome.request.width, outcome.request.height),
                (
                    footprint.0 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2,
                    footprint.1 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2
                ),
                "the footprint is the mark's, so the order the crops were picked in cannot change it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crop that CUT the mark is refused by name, with the side to re-cut.
    #[test]
    fn a_crop_that_clipped_the_mark_is_refused_naming_the_side() {
        let _guard = locale_guard();
        let dir = fixture_dir("clipped-mark");
        let footprint = (40u32, 32u32);
        let colour = [20.0, 30.0, 25.0];
        let good = write_reference(
            &dir,
            "good.png",
            &render_reference(footprint, 12, [255, 255, 255], colour),
        );
        // The same crop with everything left of the mark's own left edge cut away, so the mark
        // runs into the crop's border there.
        let full = render_reference(footprint, 12, [0, 0, 0], colour);
        let cut = crop_image(
            &full,
            PixelRect::new(
                12 + footprint.0 / 4 + 2,
                0,
                full.width() - 12 - footprint.0 / 4 - 2,
                full.height(),
            ),
        );
        let clipped = write_reference(&dir, "clipped.png", &cut);
        let err = run_reference_intake(intake(vec![good, clipped]))
            .expect_err("a crop that cut the mark cannot define a footprint");
        assert_eq!(err.refusal, ReferenceRefusal::TooSmall, "{}", err.message);
        assert!(
            err.message.contains("clipped.png"),
            "the refusal must name the file: {}",
            err.message
        );
        assert!(
            err.message.contains(&side_label(ClipSide::Left)),
            "and the side to re-cut: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crop that holds the mark but not the background around it is refused with the numbers
    /// the user can act on: what it is, and what the mark plus its border needs.
    #[test]
    fn a_crop_too_tight_for_the_joint_footprint_is_refused_with_the_numbers() {
        let _guard = locale_guard();
        let dir = fixture_dir("tight-crop");
        let footprint = (40u32, 32u32);
        let colour = [20.0, 30.0, 25.0];
        let roomy = write_reference(
            &dir,
            "roomy.png",
            &render_reference(footprint, 12, [255, 255, 255], colour),
        );
        // The mark with exactly one pixel of background all round: not clipped, but far too
        // tight to carry the footprint the mark itself measures.
        let full = render_reference(footprint, 12, [0, 0, 0], colour);
        // Three pixels: enough background for the extent rule's own border frame to be
        // background rather than mark, and far short of the footprint the mark measures.
        let spare = 3u32;
        let tight = crop_image(
            &full,
            PixelRect::new(
                12 + footprint.0 / 4 - spare,
                12 + footprint.1 / 4 - spare,
                footprint.0 / 2 + spare * 2,
                footprint.1 / 2 + spare * 2,
            ),
        );
        let tight = write_reference(&dir, "tight.png", &tight);
        let err = run_reference_intake(intake(vec![roomy, tight]))
            .expect_err("a crop with no background round the mark cannot be measured");
        assert_eq!(err.refusal, ReferenceRefusal::TooSmall, "{}", err.message);
        for number in [
            // what the crop is,
            (footprint.0 / 2 + spare * 2).to_string(),
            // and what the mark plus its border needs.
            (footprint.0 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2).to_string(),
        ] {
            assert!(
                err.message.contains(&number),
                "the refusal must state {number}: {}",
                err.message
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crop whose own mark extent disagrees with where the mark actually correlates, by more
    /// than the footprint's safety border, is REFUSED rather than fitted.
    ///
    /// This is the silent failure the joint rule would otherwise hide: a two-sample closed form
    /// is exactly determined, so a model fitted over mis-registered planes still reports a
    /// confident verdict and nothing downstream can tell. The fixture models the real cause —
    /// deposit faint enough to move the measured extent without moving the mark's edges.
    #[test]
    fn a_crop_whose_extent_disagrees_with_the_alignment_is_refused() {
        let _guard = locale_guard();
        let dir = fixture_dir("registration-drift");
        let footprint = (40u32, 32u32);
        let margin = 12u32;
        let colour = [20.0, 30.0, 25.0];
        let plain = write_reference(
            &dir,
            "plain.png",
            &render_reference(footprint, margin, [255, 255, 255], colour),
        );
        // The same mark on black, plus a barely-visible smear to its right. Two LSB is over the
        // extent rule's floor and under anything the gradient correlates on, so the crop's
        // measured extent grows to the right while its mark stays where it was.
        let mut smeared = render_reference(footprint, margin, [0, 0, 0], colour);
        let smear_from = margin + footprint.0 * 3 / 4;
        for y in margin + footprint.1 / 4..margin + footprint.1 * 3 / 4 {
            for x in smear_from..smear_from + margin - 1 {
                smeared.put_pixel(x, y, Rgba([2, 2, 2, 255]));
            }
        }
        let smeared = write_reference(&dir, "smeared.png", &smeared);
        let err = run_reference_intake(intake(vec![plain, smeared]))
            .expect_err("a crop whose two measurements disagree must not be fitted");
        assert_eq!(err.refusal, ReferenceRefusal::Misaligned, "{}", err.message);
        assert!(
            err.message.contains("smeared.png"),
            "the refusal names the crop at fault: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// IMPROVING an entry keeps the entry's own geometry: the joint rule may not move a stored
    /// footprint, because the anchors that say WHERE the mark sits on a page were measured
    /// against it.
    #[test]
    fn improving_an_entry_keeps_its_stored_footprint_and_anchors() {
        let (built, dir) = separable_entry("improve-geometry");
        let stored = (built.width, built.height);
        let mut base = loaded_from("wm-improve-geometry", &built);
        base.meta.anchors = vec![480, 1120];
        base.meta.anchor_key = "480,1120".to_string();

        let crop_dir = fixture_dir("improve-geometry-crop");
        // Deliberately framed with far more background than the entry's footprint: under the
        // joint rule this crop's own mark extent would give a different, smaller footprint.
        let crop = write_reference(
            &crop_dir,
            "wide.png",
            &render_reference((40, 32), 24, [0, 0, 0], [20.0, 30.0, 25.0]),
        );
        let outcome = run_reference_intake(improving(vec![crop], base))
            .expect("a well-formed crop improves the entry");
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            stored,
            "an entry's stored footprint is bound to its anchors and must not move here"
        );
        assert_eq!(
            outcome.request.anchors,
            vec![480, 1120],
            "and the anchors themselves are carried through untouched"
        );
        for dir in [dir, crop_dir] {
            let _ = std::fs::remove_dir_all(&dir);
        }
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
            !err.message.is_empty(),
            "the refusal must carry the measured reason, not an empty string"
        );
        assert_eq!(
            err.refusal,
            ReferenceRefusal::Background,
            "a one-level crop set is a BACKGROUND refusal, so the panel can say what to drag next"
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

    /// Builds the two-sample set the closed form is defined on, over a 2x1 footprint, and
    /// fits it. `manual` decides whether the DARK sample's level is measured or asserted.
    fn fit_two_levels(manual: bool) -> crate::watermark_chapter::WatermarkModel {
        // Pixel 0: I|B=0 = 20, I|B=255 = 140. Pixel 1: 0 and 255.
        let bright = RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 { Rgba([140, 140, 140, 255]) } else { Rgba([255, 255, 255, 255]) }
        });
        let dark = RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 { Rgba([20, 20, 20, 255]) } else { Rgba([0, 0, 0, 255]) }
        });
        let rect = PixelRect::new(0, 0, 2, 1);
        let dark_background = if manual {
            SampleBackground::Manual { level: [0.0, 0.0, 0.0] }
        } else {
            SampleBackground::Flat {
                level: [0.0, 0.0, 0.0],
                ring_std: [0.2, 0.2, 0.2],
                ring: RingCoverage::full(200)
            }
        };
        let samples = vec![
            CalibrationSample::from_page(
                &bright,
                0,
                rect,
                SampleBackground::Flat {
                    level: [255.0, 255.0, 255.0],
                    ring_std: [0.3, 0.3, 0.3],
                    ring: RingCoverage::full(200),
                },
            )
            .expect("bright sample"),
            CalibrationSample::from_page(&dark, 0, rect, dark_background).expect("dark sample"),
        ];
        crate::watermark_chapter::estimate_model(
            &samples,
            alpha_blend_operator(),
            AlphaAssumption::FromDeposit,
        )
        .expect("two well-separated levels fit")
    }

    /// The whole contract of a manually asserted level in one test: it FEEDS the fit exactly
    /// like a measured one — the planes are identical, pin for pin — and the model pays for it
    /// with a downgraded, typed provenance instead of a softer word in a report.
    #[test]
    fn a_manual_level_feeds_the_fit_and_is_paid_for_in_the_verdict() {
        let measured = fit_two_levels(false);
        let asserted = fit_two_levels(true);

        // Hand-computed closed form over the two levels: s = (I|255 - I|0)/255, c = I|0.
        let expected_c = [20.0f32, 0.0];
        let expected_s = [120.0f32 / 255.0, 1.0];
        for pixel in 0..2 {
            for channel in 0..3 {
                let index = pixel * 3 + channel;
                assert!(
                    (measured.c()[index] - expected_c[pixel]).abs() <= 1e-3,
                    "measured c[{index}] = {} , expected {}",
                    measured.c()[index],
                    expected_c[pixel]
                );
                assert!(
                    (measured.s()[index] - expected_s[pixel]).abs() <= 1e-4,
                    "measured s[{index}] = {} , expected {}",
                    measured.s()[index],
                    expected_s[pixel]
                );
                assert!(
                    (asserted.c()[index] - measured.c()[index]).abs() <= f32::EPSILON,
                    "an asserted level must produce the SAME c as the measurement it replaces"
                );
                assert!(
                    (asserted.s()[index] - measured.s()[index]).abs() <= f32::EPSILON,
                    "an asserted level must produce the SAME s as the measurement it replaces"
                );
            }
        }

        // The measured fit keeps today's verdict, untouched.
        assert_eq!(measured.provenance().manual_backgrounds, 0);
        let measured_alpha = measured
            .provenance()
            .conditioning
            .alpha_uncertainty()
            .expect("a separable fit quotes its alpha uncertainty");
        assert_eq!(measured_alpha.source, AlphaSource::SeparatedBackgrounds);
        assert!(measured.provenance().conditioning.is_separable());

        // The asserted one reaches the same arithmetic through a weaker claim, and says so.
        assert_eq!(asserted.provenance().manual_backgrounds, 1);
        assert!(asserted.provenance().conditioning.is_separable());
        let asserted_alpha = asserted
            .provenance()
            .conditioning
            .alpha_uncertainty()
            .expect("the downgraded fit still quotes an alpha uncertainty");
        assert_eq!(asserted_alpha.source, AlphaSource::ManualBackgrounds);
        assert!(
            asserted_alpha.percent > measured_alpha.percent,
            "the downgrade must cost something: {} vs {}",
            asserted_alpha.percent,
            measured_alpha.percent
        );
        assert!(asserted_alpha.rms_lsb > measured_alpha.rms_lsb);
        // And the stored record carries the weaker source, so a reload cannot silently upgrade.
        assert_eq!(
            alpha_source_wire(asserted_alpha.source),
            "manual_backgrounds"
        );
        assert_eq!(
            alpha_source_from_wire("manual_backgrounds"),
            AlphaSource::ManualBackgrounds
        );
        // An unknown tag still falls back to the WEAKEST source, never to this one.
        assert_eq!(alpha_source_from_wire("from_a_newer_build"), AlphaSource::Assumed);
    }

    /// The reference intake accepts a crop it refuses today — and only when a level comes with
    /// it. The automatic flatness test itself is untouched: the same crop with no asserted
    /// level is still refused.
    #[test]
    fn a_structured_crop_is_accepted_only_with_an_asserted_level() {
        let dir = fixture_dir("asserted");
        let footprint = (40, 32);
        let margin = 6;
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        // The same mark over BLACK, but the MEASUREMENT RING around the footprint is textured
        // instead of uniform. Only the ring is touched, so the crop still aligns — the footprint
        // the correlation looks at is untouched — while `validate_calibration_sample` refuses to
        // call the ring flat. That is exactly the crop the engine drops today and only the user
        // can vouch for.
        let ring = SampleParams::default().normalized().ring_width + REFERENCE_RING_MARGIN_SLACK_PX;
        let mut textured = render_reference(footprint, margin, [0, 0, 0], [20.0, 30.0, 25.0]);
        let (width, height) = textured.dimensions();
        for y in 0..height {
            for x in 0..width {
                // The annulus the level is read from: everything outside the footprint window,
                // which `run_reference_intake` insets by `ring` on every side.
                let in_ring = x < ring || y < ring || x + ring >= width || y + ring >= height;
                if in_ring {
                    // Cast justification: a two-valued synthetic texture, 0 or 24 by
                    // construction. Its std clears `FLAT_RING_STD_LIMIT` on every channel.
                    let value = u8::from((x + y) % 2 == 0) * 24;
                    textured.put_pixel(x, y, Rgba([value, value, value, 255]));
                }
            }
        }
        let structured = write_reference(&dir, "structured.png", &textured);

        let mut without = intake(vec![white.clone(), structured.clone()]);
        without.manual_levels = vec![None, None];
        assert!(
            run_reference_intake(without).is_err(),
            "with no asserted level the automatic test must still refuse the crop"
        );

        let mut with = intake(vec![white, structured]);
        with.manual_levels = vec![None, Some([0.0, 0.0, 0.0])];
        let outcome = run_reference_intake(with).expect("an asserted level licenses the crop");

        assert!(outcome.conditioning.is_separable());
        let alpha = outcome
            .conditioning
            .alpha_uncertainty()
            .expect("a separable verdict quotes its alpha");
        assert_eq!(
            alpha.source,
            AlphaSource::ManualBackgrounds,
            "the entry may no longer claim its backgrounds were measured"
        );
        assert_eq!(
            outcome.request.calibration.alpha.as_ref().map(|alpha| alpha.source.as_str()),
            Some("manual_backgrounds"),
            "and the claim's weakness is what gets persisted"
        );

        // The reports name WHICH crop rests on a claim, and refuse to invent a ring for it.
        assert!(!outcome.reports[0].manual_level);
        assert!(outcome.reports[0].ring_std.is_some());
        assert!(outcome.reports[1].manual_level);
        assert_eq!(outcome.reports[1].ring_std, None);
        assert_eq!(outcome.reports[1].level, [0.0, 0.0, 0.0]);
        assert_eq!(
            outcome.request.samples[1].background,
            StoredSampleBackground::Manual { level: [0.0, 0.0, 0.0] }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An asserted level is a colour, not a free number: what reaches the fit is bounded and
    /// finite whatever the caller passed.
    #[test]
    fn an_asserted_level_is_bounded_before_it_reaches_the_maths() {
        assert_eq!(normalize_manual_level([-5.0, 300.0, 128.0]), [0.0, 255.0, 128.0]);
        assert_eq!(normalize_manual_level([f32::NAN, f32::INFINITY, 10.0]), [127.5, 127.5, 10.0]);
    }

    /// `c + s*255` per channel, quantized once, with the clamp at the top of the range
    /// exercised deliberately.
    #[test]
    fn the_mark_on_white_is_the_forward_composite() {
        // Three pixels: a transparent one, a half-opaque dark deposit, and one whose composite
        // lands above 255 and must clamp rather than wrap.
        let c = vec![
            0.0, 0.0, 0.0, // s = 1  -> 0 + 255 = 255
            20.0, 30.0, 40.0, // s = 0.5 -> 147.5 / 157.5 / 167.5
            250.0, 250.0, 250.0, // s = 0.9 -> 479.5, clamps to 255
        ];
        let s = vec![1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.9, 0.9, 0.9];
        let image = compose_planes_on_background(3, 1, &c, &s, &AlphaBlend, 255.0)
            .expect("a well-formed plane pair composites");
        assert_eq!(image.dimensions(), (3, 1));
        assert_eq!(image.as_raw(), &[255, 255, 255, 255, 148, 158, 168, 255, 255, 255, 255, 255]);
    }

    /// A plane that does not describe the footprint is refused with a typed engine error, not
    /// an index panic, and neither is a plane carrying a non-finite parameter.
    #[test]
    fn a_malformed_plane_is_refused_without_panicking() {
        let good = vec![0.0f32; 6];
        assert!(matches!(
            compose_planes_on_background(2, 1, &good[..3], &good, &AlphaBlend, 255.0),
            Err(WatermarkError::BufferLength { .. })
        ));
        assert!(matches!(
            compose_planes_on_background(2, 1, &good, &good[..5], &AlphaBlend, 255.0),
            Err(WatermarkError::BufferLength { .. })
        ));
        let mut broken = good.clone();
        broken[4] = f32::NAN;
        assert!(matches!(
            compose_planes_on_background(2, 1, &broken, &good, &AlphaBlend, 255.0),
            Err(WatermarkError::ParameterOutOfRange { .. })
        ));
        assert!(matches!(
            compose_planes_on_background(0, 1, &[], &[], &AlphaBlend, 255.0),
            Err(WatermarkError::GeometryMismatch { .. })
        ));
    }

    /// An entry whose fit was refused has no planes, so it gets no icon — never a fabricated
    /// one. The caller falls back to the stored template.
    #[test]
    fn an_entry_without_a_model_has_no_icon() {
        assert_eq!(
            render_mark_on_white_from(None).expect("no model is not an error"),
            None
        );
    }

    /// A compositing law this build does not implement is refused rather than approximated
    /// with alpha blending, which would be confidently wrong rather than slightly off.
    #[test]
    fn an_unknown_compositing_operator_is_refused() {
        let planes = LoadedPlanes {
            width: 1,
            height: 1,
            operator: "multiply".to_string(),
            planes: LibraryPlanes { c: vec![0.0; 3], s: vec![1.0; 3] },
        };
        assert!(render_mark_on_white_from(Some(planes)).is_err());
    }

    /// The card's warning material, from metadata alone.
    #[test]
    fn entry_warnings_carry_every_condition_the_card_draws() {
        let signature = StoredSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };

        // "Not enough samples" arrives as its OWN variant, not as an unknown verdict.
        let mut starved = summary("wm-starved", signature, "not_enough_samples", 0.0);
        starved.samples = 1;
        let warnings = entry_warnings(&starved);
        assert!(matches!(
            warnings.conditioning,
            Some(ModelConditioning::NotEnoughSamples { have: 1, need: 2 })
        ));
        assert!(warnings.no_model, "no fit method recorded means no model");
        assert!(!warnings.samples_disagree);
        assert!(!warnings.rests_on_assertion);

        // A verdict tag from a newer writer stays unknown rather than being rounded down to
        // the nearest one this build does know.
        let unknown = summary("wm-unknown", signature, "something_newer", 0.0);
        assert!(entry_warnings(&unknown).conditioning.is_none());

        // Clamped pixels are the recorded evidence that the samples disagree pixel for pixel.
        let mut disagreeing = summary("wm-clamped", signature, "separable", 255.0);
        disagreeing.fit_method = Some("closed_form_flat".to_string());
        let footprint = (disagreeing.width as usize) * (disagreeing.height as usize);
        disagreeing.clamped_pixels = footprint / 4;
        let quiet = entry_warnings(&disagreeing);
        assert!(!quiet.no_model);
        assert!((quiet.clamped_share - 0.25).abs() <= 1e-6);
        assert!(
            !quiet.samples_disagree,
            "a quarter of the footprint is within what the parameter ceiling alone explains"
        );
        disagreeing.clamped_pixels = footprint * 3 / 4;
        let loud = entry_warnings(&disagreeing);
        assert!(loud.clamped_share > CLAMPED_SHARE_WARN);
        assert!(loud.samples_disagree);

        // And an entry resting on an assertion names the samples that do.
        let mut asserted = summary("wm-asserted", signature, "separable", 255.0);
        asserted.manual_backgrounds = vec![ManualBackgroundRef {
            file: "samples/001.png".to_string(),
            level: [0.0, 0.0, 0.0],
        }];
        let warnings = entry_warnings(&asserted);
        assert!(warnings.rests_on_assertion);
        assert_eq!(warnings.manual_backgrounds.len(), 1);
        assert_eq!(warnings.manual_backgrounds[0].file, "samples/001.png");
    }

    /// Between two entries a verdict cannot separate, the measured one wins.
    #[test]
    fn a_measured_entry_outranks_one_resting_on_an_assertion() {
        let signature = StoredSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };
        let mut asserted = summary("wm-asserted", signature, "separable", 255.0);
        asserted.manual_backgrounds = vec![ManualBackgroundRef {
            file: "samples/000.png".to_string(),
            level: [0.0, 0.0, 0.0],
        }];
        let entries = vec![asserted, summary("wm-measured", signature, "separable", 255.0)];
        let measured_mark = MarkSignature {
            reference_level: 255.0,
            deposit_chroma: 4.0,
            mean_deposit: 43.0,
            peak_alpha: 0.19,
        };
        let ranked = rank_library_candidates(&entries, &measured_mark, (40, 32));
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].entry_id, "wm-measured");
        assert!(!ranked[0].rests_on_assertion);
        assert!(ranked[1].rests_on_assertion);
    }

    fn summary(id: &str, signature: StoredSignature, verdict: &str, spread: f32) -> EntrySummary {
        EntrySummary {
            has_template: true,
            partial_rings: 0,
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
            format: 1,
            clamped_pixels: 0,
            manual_backgrounds: Vec::new(),
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

    /// A `LoadedEntry` standing for what `load_entry` would hand back for `request`.
    ///
    /// Built from the request rather than from disk so the delete path can be exercised
    /// without a library root: `save_entry`/`load_entry` already have their own round-trip
    /// tests, and what matters here is the refit, not the I/O.
    fn loaded_from(id: &str, request: &SaveEntryRequest) -> LoadedEntry {
        LoadedEntry {
            meta: super::super::watermark_library::EntryFile {
                format: super::super::watermark_library::WATERMARK_LIBRARY_FORMAT,
                id: id.to_string(),
                name: request.name.clone(),
                created_unix: 1,
                updated_unix: 2,
                operator: request.operator.clone(),
                width: request.width,
                height: request.height,
                anchors: request.anchors.clone(),
                anchor_key: request.anchor_key.clone(),
                alpha_assumption: request.alpha_assumption,
                signature: request.signature,
                calibration: request.calibration.clone(),
                sources: Vec::new(),
                samples: Vec::new(),
                template: Some("template.png".to_string()),
                planes: None,
            },
            template: request.template.clone(),
            samples: request.samples.clone(),
        }
    }

    /// The white+black entry every delete test starts from: separable, two crops, a model.
    fn separable_entry(name: &str) -> (SaveEntryRequest, PathBuf) {
        let dir = fixture_dir(name);
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
        (outcome.request, dir)
    }

    /// A crop of the mark with a DIFFERENT amount of background on each side, as
    /// `run_library_capture` writes one that the page border clipped.
    fn asymmetric_reference(
        footprint: (u32, u32),
        margins: CropMargins,
        background: [u8; 3],
        colour: [f32; 3],
    ) -> RgbaImage {
        let width = footprint.0 + margins.left + margins.right;
        let height = footprint.1 + margins.top + margins.bottom;
        RgbaImage::from_fn(width, height, |x, y| {
            let inside = x >= margins.left
                && y >= margins.top
                && x < margins.left + footprint.0
                && y < margins.top + footprint.1;
            if !inside {
                return Rgba([background[0], background[1], background[2], 255]);
            }
            let alpha = mark_alpha(x - margins.left, y - margins.top, footprint.0, footprint.1);
            let channels: [u8; 3] = std::array::from_fn(|channel| {
                let base = f32::from(background[channel]);
                // Cast justification: an alpha composite of two 0..=255 values, rounded.
                (alpha * colour[channel] + (1.0 - alpha) * base).clamp(0.0, 255.0).round() as u8
            });
            Rgba([channels[0], channels[1], channels[2], 255])
        })
    }

    /// The EMPTY entry `empty_entry_request` describes, as `load_entry` would hand it back.
    fn loaded_from_empty(id: &str, request: &SaveEntryRequest) -> LoadedEntry {
        LoadedEntry {
            meta: super::super::watermark_library::EntryFile {
                format: super::super::watermark_library::WATERMARK_LIBRARY_FORMAT,
                id: id.to_string(),
                name: request.name.clone(),
                created_unix: 1,
                updated_unix: 2,
                operator: request.operator.clone(),
                width: 0,
                height: 0,
                anchors: Vec::new(),
                anchor_key: String::new(),
                alpha_assumption: request.alpha_assumption,
                signature: None,
                calibration: request.calibration.clone(),
                sources: Vec::new(),
                samples: Vec::new(),
                template: None,
                planes: None,
            },
            template: None,
            samples: Vec::new(),
        }
    }

    /// An intake against an EXISTING entry, i.e. exactly what «+ Выделить новый» starts.
    fn improving(files: Vec<PathBuf>, base: LoadedEntry) -> ReferenceIntakeRequest {
        ReferenceIntakeRequest {
            crop_margins: Vec::new(),
            files,
            manual_levels: vec![None; 1],
            sample_params: SampleParams::default(),
            base: Some(base),
            name: String::new(),
            max_side: 512,
        }
    }

    /// The two refusals a normal user hits when re-dragging a sample for an entry that
    /// already exists, each carrying the TAG that decides what the panel tells them to do,
    /// plus the case that is deliberately NO LONGER a refusal.
    ///
    /// Both used to be silent on the canvas path: a crop that measures flat is committed
    /// without a row, so the refusal had no row to land on. The tags are what turn the
    /// engine's own numeric reason into an instruction.
    #[test]
    fn the_re_drag_refusals_carry_their_tags() {
        // The same geometry `separable_entry` drew the entry from, so a "good" re-drag here is
        // pixel-identical to what the entry was fitted on.
        let drawn = (40, 32);
        let drawn_margin = 6;
        let colour = [20.0, 30.0, 25.0];
        let (built, dir) = separable_entry("re-drag");

        // (a) TOO SMALL — the crop is short of the entry's STORED footprint, which the intake
        // measured from the mark itself when the entry was created. Two pixels are enough.
        let small_dir = fixture_dir("re-drag-small-crop");
        let stored = (built.width, built.height);
        let small = write_reference(
            &small_dir,
            "small.png",
            &render_reference(
                (stored.0 - 2 - drawn_margin * 2, stored.1 - 2 - drawn_margin * 2),
                drawn_margin,
                [0, 0, 0],
                colour,
            ),
        );
        let err =
            run_reference_intake(improving(vec![small], loaded_from("wm-re-drag", &built)))
                .expect_err("a crop short of the stored footprint cannot carry a full ring");
        assert_eq!(err.refusal, ReferenceRefusal::TooSmall, "{}", err.message);

        // (b) MISALIGNED — a crop of ample size that does not carry this mark at all.
        //
        // A re-drag a few pixels off is no longer a refusal, and must not be: the stored
        // footprint is the MARK's own extent, so a crop framed loosely around it leaves the
        // aligner room to find it, which is exactly what the search exists for
        // (`a_shifted_crop_is_aligned_before_it_is_measured` pins that). What cannot be accepted
        // is a crop whose content does not correlate with the entry's template at all. A blank
        // crop is caught one step earlier than [`REFERENCE_MIN_ALIGN_NCC`] — it holds no mark to
        // measure an extent from — and carries the same tag, because it is the same mistake:
        // the rectangle was not put on the mark.
        let shifted_dir = fixture_dir("re-drag-shift-crop");
        let shifted = write_reference(
            &shifted_dir,
            "blank.png",
            &RgbaImage::from_pixel(
                drawn.0 + drawn_margin * 2,
                drawn.1 + drawn_margin * 2,
                Rgba([0, 0, 0, 255]),
            ),
        );
        let err =
            run_reference_intake(improving(vec![shifted], loaded_from("wm-re-drag", &built)))
                .expect_err("a crop that does not carry this mark cannot be aligned to it");
        assert_eq!(err.refusal, ReferenceRefusal::Misaligned, "{}", err.message);

        // (c) SAME BACKGROUND — a well-formed white crop for an entry that already holds only
        // white. This is ACCEPTED, and the verdict says exactly what it is worth.
        //
        // Separability is a rule about BUILDING an entry, not about improving one. The store
        // already holds one-sample, non-separable entries on purpose (`drop_entry_sample`
        // writes exactly that), so refusing here was inconsistent — and for an entry with no
        // samples at all it was fatal: its FIRST crop is necessarily one level, so the entry
        // could never take one. The crop is written and the verdict degrades to `DepositExact`,
        // which claims the deposit is measured and the alpha scale is not — the honest report.
        let white_only = drop_entry_sample(loaded_from("wm-re-drag", &built), 1)
            .expect("dropping the black crop refits the entry from the white one");
        let white_dir = fixture_dir("re-drag-white-crop");
        let white = write_reference(
            &white_dir,
            "white.png",
            &render_reference(drawn, drawn_margin, [255, 255, 255], colour),
        );
        let outcome = run_reference_intake(improving(
            vec![white],
            loaded_from("wm-re-drag", &white_only),
        ))
        .expect("a second sample on a level the entry already has is still worth keeping");
        assert!(
            !outcome.conditioning.is_separable(),
            "one level cannot separate alpha from the mark, and the verdict must not claim it does"
        );
        assert!(
            matches!(outcome.conditioning, ModelConditioning::DepositExact { .. }),
            "the honest degraded verdict, not a separable one: {:?}",
            outcome.conditioning
        );
        assert_eq!(
            outcome.request.samples.len(),
            2,
            "the entry keeps its own crop and gains the new one"
        );

        for dir in [dir, small_dir, shifted_dir, white_dir] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// An EMPTY entry — no template, no footprint, no samples — accepts its FIRST crop, and
    /// that crop is what gives it its geometry.
    ///
    /// This is the «+ новый» flow end to end: the entry is created before anything is measured,
    /// and the engine's "one background cannot separate alpha from the mark" rule applies to the
    /// FIT, not to the entry's existence. The verdict it lands on is the honest degraded one.
    #[test]
    fn an_empty_entry_takes_its_first_crop_and_ends_on_the_degraded_verdict() {
        let dir = fixture_dir("empty-first-crop");
        let footprint = (40, 32);
        // The intake insets a crop by exactly this much to find the mark, so a fixture cut with
        // the same margin makes the derived footprint the one that was drawn.
        let margin = reference_crop_margin_px(&SampleParams::default());
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], [20.0, 30.0, 25.0]),
        );
        let empty = empty_entry_request("Новый знак".to_string());
        let outcome = run_reference_intake(improving(
            vec![white],
            loaded_from_empty("wm-empty", &empty),
        ))
        .expect("an entry with no samples must be able to take its first one");
        assert_eq!(outcome.request.entry_id.as_deref(), Some("wm-empty"));
        assert_eq!(
            outcome.request.name, "Новый знак",
            "improving an entry keeps its own name verbatim"
        );
        assert_eq!(outcome.request.samples.len(), 1);
        // The synthetic mark deposits on the middle half of the drawn rectangle, so its own
        // extent is 20x16 and the footprint is that plus one safety border on each side. An
        // entry with no template has nothing on disk binding its geometry, so its first crop
        // defines it from the MARK — never from the rectangle the crop happens to be.
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            (footprint.0 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2, footprint.1 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2),
            "an entry that had no footprint takes the mark's own, not the crop's rectangle"
        );
        assert!(
            outcome.trimmed.is_none(),
            "nothing was trimmed: there was no stored footprint to move"
        );
        assert!(
            outcome.request.template.is_some(),
            "and it is what gives the entry its correlation template"
        );
        assert!(
            matches!(outcome.conditioning, ModelConditioning::DepositExact { .. }),
            "one measured level fits the deposit exactly and assumes only the alpha scale: {:?}",
            outcome.conditioning
        );
        assert!(
            !outcome.conditioning.is_separable(),
            "and the verdict must not claim a separability one level cannot give"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crop cut against the page border carries LESS background on one side, and the intake
    /// reads the asymmetry as GEOMETRY — what the cutter had to clip — rather than as the
    /// footprint the entry should get.
    ///
    /// The numbers are the user's own: a 319x236 selection whose right edge lands on an 800-px
    /// page boundary, leaving one pixel of background there against the four the symmetric rule
    /// wants. Under that rule the crop was refused before it was ever written. It is now accepted,
    /// its level is still MEASURED from the ring that survived, and the entry it defines takes the
    /// MARK's own footprint — the selection is a ceiling, never a shape.
    #[test]
    fn a_crop_clipped_at_the_page_border_is_measured_and_gives_the_mark_its_own_footprint() {
        let dir = fixture_dir("edge-crop");
        let footprint = (319u32, 236u32);
        let margins = CropMargins {
            left: 4,
            top: 4,
            right: 1,
            bottom: 4,
        };
        // The crop exactly as `run_library_capture` would have written it: the mark plus the
        // background the page could give on each side.
        let image = asymmetric_reference(footprint, margins, [240, 240, 240], [20.0, 30.0, 25.0]);
        let file = write_reference(&dir, "edge.png", &image);
        let empty = empty_entry_request("Край".to_string());
        let mut request = improving(vec![file], loaded_from_empty("wm-edge", &empty));
        request.crop_margins = vec![Some(margins)];
        let outcome = run_reference_intake(request)
            .expect("a mark against the page border is measurable from the sides that exist");
        // The synthetic mark deposits on the middle half of the selection, so its own extent is
        // 160x118 and the footprint is that plus a safety border on each side — far smaller than
        // the rectangle that was cut, and the whole point: every pixel of the difference is empty
        // background that would otherwise be stored twice and demanded of every future crop.
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            (
                footprint.0 / 2 + 1 + FOOTPRINT_TRIM_SAFETY_PX * 2,
                footprint.1 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2
            ),
            "the footprint is the mark's, and the cut rectangle is only its ceiling"
        );
        assert!(
            outcome.request.width < footprint.0 && outcome.request.height < footprint.1,
            "and it is strictly smaller than the rectangle the drag defined"
        );
        let report = outcome.reports.first().expect("one crop, one report");
        assert!(
            !report.manual_level,
            "a truncated ring is MEASURED, never asserted"
        );
        assert!(
            (report.level[0] - 240.0).abs() < 0.5,
            "the level is the background it was cut from: {:?}",
            report.level
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ONE thing a stated [`CropMargins`] decides: a side the cutter had to clip at the page
    /// border may carry the mark right up to the crop's own edge.
    ///
    /// Everywhere else that touch means the crop CUT the mark, and the refusal names the side —
    /// the one thing a size cannot say. The two cases are indistinguishable from the pixels alone;
    /// only the cutter knows whether the background ran out or the drag did, which is exactly what
    /// the margins state.
    ///
    /// Measured on [`measure_mark_extents`] itself rather than through a whole intake, because the
    /// rule is a property of ONE crop against its own frame and needs no footprint to exist.
    #[test]
    fn a_stated_clipped_side_lets_the_mark_reach_the_crops_own_border() {
        let _guard = locale_guard();
        let dir = fixture_dir("edge-flush-mark");
        // Flat background with the mark's body well inside, plus a faint spur running into the
        // RIGHT border. The spur is deliberately faint and small: the extent's own level is the
        // mean of the crop's inner frame, so a heavy mark ON that frame drags the level towards
        // itself and the measurement degenerates instead of reporting anything (the engine's
        // documented self-limiting property, which is NOT what this test is about).
        let mut image = RgbaImage::from_pixel(200, 160, Rgba([240, 240, 240, 255]));
        for y in 50..110u32 {
            for x in 60..140u32 {
                image.put_pixel(x, y, Rgba([40, 40, 40, 255]));
            }
        }
        for y in 78..81u32 {
            for x in 196..200u32 {
                image.put_pixel(x, y, Rgba([150, 150, 150, 255]));
            }
        }
        let file = write_reference(&dir, "spur.png", &image);
        let images = vec![image];
        let files = vec![file];

        // Nothing stated: the mark runs into the crop's own border, so the crop cut it.
        let refusal = measure_mark_extents(&images, &files, &[])
            .expect_err("a mark reaching the crop border is a cut mark when nothing says otherwise");
        assert_eq!(refusal.refusal, ReferenceRefusal::TooSmall);
        assert!(
            refusal.message.contains(
                &t!("cleaning.tools.watermark.chapter.reference_side_right").to_string()
            ),
            "and the refusal names the side: {}",
            refusal.message
        );

        // The cutter states that the page border took that side: the same pixels are accepted,
        // and the measured extent really does reach the border.
        let clipped = CropMargins {
            left: 10,
            top: 10,
            right: 0,
            bottom: 10,
        };
        let extents = measure_mark_extents(&images, &files, &[Some(clipped)])
            .expect("a stated clipped side is geometry, not a cut mark")
            .expect("the background is flat, so the extent carries information");
        let extent = extents.first().expect("one crop, one extent");
        assert_eq!(extent.right(), 200, "the extent reaches the border it was allowed to");
        assert_eq!(extent.x, 60, "and stops where the mark's body does on the other side");

        // A side the cutter says it DID leave background on is still checked against that claim.
        let full = CropMargins::uniform(10);
        let refusal = measure_mark_extents(&images, &files, &[Some(full)])
            .expect_err("a mark crossing into background the cutter claims to have left is cut");
        assert_eq!(refusal.refusal, ReferenceRefusal::TooSmall);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GAP 1 + GAP 2, through the CANVAS: an entry with no template and no samples takes its
    /// first canvas selection, and the footprint it gets is the MARK's — not the rectangle the
    /// user dragged.
    ///
    /// The canvas capture lane STATES its `CropMargins`, which used to veto the joint rule
    /// outright and send this entry back to a drag-shaped footprint. Stated margins say what the
    /// cutter had to clip; they are not a geometry the intake must adopt. Pinned by comparing the
    /// two lanes directly: stating them changes nothing about the footprint.
    #[test]
    fn an_empty_entry_takes_a_canvas_selection_and_derives_the_footprint_from_the_mark() {
        let dir = fixture_dir("empty-canvas-crop");
        let drawn = (40u32, 32u32);
        // `run_library_capture` grows the user's selection by exactly this much and states it.
        let margin = reference_crop_margin_px(&SampleParams::default());
        let image = render_reference(drawn, margin, [255, 255, 255], [20.0, 30.0, 25.0]);
        let file = write_reference(&dir, "canvas.png", &image);
        let empty = empty_entry_request("Новый знак".to_string());

        let mut request = improving(vec![file.clone()], loaded_from_empty("wm-canvas", &empty));
        request.crop_margins = vec![Some(CropMargins::uniform(margin))];
        let canvas = run_reference_intake(request)
            .expect("a canvas selection must be able to fill an empty entry");

        let mark = (
            drawn.0 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2,
            drawn.1 / 2 + FOOTPRINT_TRIM_SAFETY_PX * 2,
        );
        assert_eq!(
            (canvas.request.width, canvas.request.height),
            mark,
            "the footprint is the mark plus its safety border, not the {drawn:?} rectangle dragged"
        );
        assert_eq!(canvas.request.samples.len(), 1);
        assert!(
            canvas.request.template.is_some(),
            "and the crop is what gives the entry its correlation template"
        );
        assert_eq!(
            canvas.trimmed, None,
            "nothing was trimmed: the entry had no stored footprint to move"
        );

        // The file-picker lane states nothing, and reaches exactly the same footprint. That
        // equality IS the contract: the margins are information about the crop, never a shape.
        let picked = run_reference_intake(improving(
            vec![file],
            loaded_from_empty("wm-canvas", &empty),
        ))
        .expect("the same crop through the picker lane");
        assert_eq!(
            (picked.request.width, picked.request.height),
            (canvas.request.width, canvas.request.height),
            "stating the margins may not change the footprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A TIGHT entry still refuses a crop that genuinely missed part of the mark, and names the
    /// SIDE rather than a size.
    ///
    /// This is the other half of the trim: the footprint stops being a rectangle the user must
    /// cover, but "your selection cut the mark" remains a real refusal and must stay one. The
    /// entry here is already tight — `trim_entry_footprint` says so — so nothing can be trimmed to
    /// make the crop fit, and the refusal is about the crop.
    #[test]
    fn a_tight_entry_still_refuses_a_crop_that_cut_the_mark_and_names_the_side() {
        let _guard = locale_guard();
        let (built, dir) = separable_entry("tight-refuses-cut");
        let base = loaded_from("wm-tight-cut", &built);
        assert!(
            matches!(
                trim_entry_footprint(loaded_from("wm-tight-cut", &built)),
                Ok(FootprintTrimOutcome::AlreadyTight { .. })
            ),
            "this fixture is the TIGHT case: there is nothing left to trim"
        );

        let crop_dir = fixture_dir("tight-refuses-cut-crop");
        let drawn = (40u32, 32u32);
        let drawn_margin = 6u32;
        let colour = [20.0, 30.0, 25.0];
        let good = render_reference(drawn, drawn_margin, [255, 255, 255], colour);
        // The same crop, with a faint part of the mark running into the background the cutter
        // states it left on the right: the selection stopped short of the mark. Faint and small on
        // purpose — a heavy mark ON the crop's own ring frame poisons the level the extent is
        // measured against, and the measurement then reports nothing at all rather than a side.
        let cut = {
            let mut image = good.clone();
            for y in 20..23u32 {
                for x in 46..49u32 {
                    image.put_pixel(x, y, Rgba([245, 245, 245, 255]));
                }
            }
            image
        };
        let stated = CropMargins::uniform(drawn_margin);

        let mut request = improving(
            vec![write_reference(&crop_dir, "good.png", &good)],
            loaded_from("wm-tight-cut", &built),
        );
        request.crop_margins = vec![Some(stated)];
        run_reference_intake(request).expect("the unclipped crop is a perfectly good sample");

        let mut request = improving(vec![write_reference(&crop_dir, "cut.png", &cut)], base);
        request.crop_margins = vec![Some(stated)];
        let refusal = run_reference_intake(request)
            .expect_err("a crop whose mark runs past the background it claims is a cut crop");
        assert_eq!(refusal.refusal, ReferenceRefusal::TooSmall);
        assert!(
            refusal.message.contains(
                &t!("cleaning.tools.watermark.chapter.reference_side_right").to_string()
            ),
            "and it names the side the user has to give room on: {}",
            refusal.message
        );
        for dir in [dir, crop_dir] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// THE USER'S OWN ENTRY: a template-only entry with ZERO samples whose mark is flush
    /// against the page edge accepts the crop cut for it.
    ///
    /// `wm-1786790637-268d778f`: footprint 319x236, anchor x=480, page width 800 — so the mark
    /// ends on 799 and one pixel of background is left to its right. Two separate refusals kept
    /// this entry from ever taking a sample: the capture demanded four pixels of ring on every
    /// side, and the intake demanded a separable crop set that one crop cannot be. Neither said
    /// anything the user could see. Both are answered here, on the entry's real numbers.
    #[test]
    fn the_users_zero_sample_edge_entry_accepts_its_first_crop() {
        let dir = fixture_dir("users-entry");
        let footprint = (319u32, 236u32);
        let margins = CropMargins {
            left: 4,
            top: 4,
            right: 1,
            bottom: 4,
        };
        let colour = [20.0, 30.0, 25.0];
        // The stored template: the mark's own pixels, as `template.png` holds them.
        let template = crop_image(
            &render_reference(footprint, 4, [240, 240, 240], colour),
            PixelRect::new(4, 4, footprint.0, footprint.1),
        );
        let base = LoadedEntry {
            meta: super::super::watermark_library::EntryFile {
                format: 1,
                id: "wm-1786790637-268d778f".to_string(),
                name: "Знак".to_string(),
                created_unix: 1,
                updated_unix: 2,
                operator: "alpha_blend".to_string(),
                width: footprint.0,
                height: footprint.1,
                anchors: vec![480],
                anchor_key: "480".to_string(),
                alpha_assumption: StoredAlphaAssumption::FromDeposit,
                signature: None,
                calibration: StoredCalibration {
                    verdict: "not_enough_samples".to_string(),
                    levels: Vec::new(),
                    spread: 0.0,
                    samples: 0,
                    fit_method: None,
                    clamped_pixels: 0,
                    alpha: None,
                },
                sources: Vec::new(),
                samples: Vec::new(),
                template: Some("template.png".to_string()),
                planes: None,
            },
            // A real template, and NO samples: the exact state the entry was found in.
            template: Some(template),
            samples: Vec::new(),
        };

        // The crop the capture lane now writes: the mark plus the one pixel of background the
        // page could give on its right.
        let file = write_reference(
            &dir,
            "edge.png",
            &asymmetric_reference(footprint, margins, [240, 240, 240], colour),
        );
        let mut request = improving(vec![file], base);
        request.crop_margins = vec![Some(margins)];
        let outcome = run_reference_intake(request)
            .expect("the user's entry must be able to receive a sample");

        assert_eq!(
            outcome.request.entry_id.as_deref(),
            Some("wm-1786790637-268d778f"),
            "the crop improves the entry rather than creating another one"
        );
        assert_eq!(
            (outcome.request.width, outcome.request.height),
            footprint,
            "and the entry keeps its own footprint: its template is its identity"
        );
        assert_eq!(outcome.request.anchors, vec![480], "and its anchors");
        assert_eq!(outcome.request.samples.len(), 1, "it now has the sample it lacked");
        let report = outcome.reports.first().expect("one crop, one report");
        assert!(!report.manual_level, "the level was measured, not claimed");
        assert!(report.ring_partial, "from a ring the page edge truncated, and it says so");
        assert!(
            matches!(outcome.conditioning, ModelConditioning::DepositExact { .. }),
            "one level measures the deposit exactly and assumes only the alpha scale: {:?}",
            outcome.conditioning
        );
        assert!(
            outcome.request.planes.is_some(),
            "and a model IS produced: the deposit at that level is exact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Alignment compares GRADIENT MAGNITUDE on both sides, so a crop on a dark background
    /// lines up against a template cut from a light one.
    ///
    /// This is the pairing the whole feature exists for — the second background is what
    /// separates alpha from the mark — and it is exactly the case a raw luminance correlation
    /// destroys: between white and black the mark's contrast changes SIGN, so a luminance NCC
    /// scores a perfect match near -1 and the crop is refused as misaligned. `|grad|` is
    /// sign-free, and for a mark of one colour the two maps differ only by a positive factor
    /// that a zero-mean NCC divides out.
    #[test]
    fn a_crop_on_the_opposite_background_still_aligns_against_the_stored_template() {
        let dir = fixture_dir("align-across-backgrounds");
        let footprint = (40, 32);
        let margin = 6;
        let colour = [20.0, 30.0, 25.0];
        // The entry is built from WHITE alone, so its stored template is the mark over white.
        let white = write_reference(
            &dir,
            "white.png",
            &render_reference(footprint, margin, [255, 255, 255], colour),
        );
        let empty = empty_entry_request("Знак".to_string());
        let built = run_reference_intake(improving(
            vec![white],
            loaded_from_empty("wm-align", &empty),
        ))
        .expect("the first crop builds the template");

        // The second crop is the same mark over BLACK: the case luminance correlation inverts.
        let black = write_reference(
            &dir,
            "black.png",
            &render_reference(footprint, margin, [0, 0, 0], colour),
        );
        let outcome =
            run_reference_intake(improving(vec![black], loaded_from("wm-align", &built.request)))
                .expect("a crop on the opposite background must align, not be refused");
        let report = outcome.reports.first().expect("one crop, one report");
        assert_eq!((report.dx, report.dy), (0, 0), "it is where it was cut");
        assert!(
            report.ncc >= REFERENCE_MIN_ALIGN_NCC,
            "the gradient correlation must clear the floor, not sit below it at a negative \
             score: {}",
            report.ncc
        );
        assert!(
            report.ncc > 0.9,
            "and it is a near-perfect match, because the two gradient maps differ only by a \
             positive factor: {}",
            report.ncc
        );
        assert!(
            outcome.conditioning.is_separable(),
            "white plus black is the exact case: {:?}",
            outcome.conditioning
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting a crop REFITS the entry from the ones that remain. Down to one crop the model
    /// survives — the engine fits a deposit-exact one from a single known level — and the
    /// verdict degrades to say so, which is the honest report, not a refusal.
    #[test]
    fn deleting_a_crop_refits_the_entry_and_degrades_the_verdict() {
        let (built, dir) = separable_entry("drop-one");
        assert_eq!(built.calibration.verdict, "separable");
        let kept_pixels = built.samples[1].image.as_raw().clone();

        let rewrite = drop_entry_sample(loaded_from("wm-drop-one", &built), 0)
            .expect("dropping one of two crops must succeed");

        assert_eq!(rewrite.entry_id.as_deref(), Some("wm-drop-one"));
        assert_eq!(rewrite.samples.len(), 1);
        assert_eq!(
            rewrite.samples[0].image.as_raw(),
            &kept_pixels,
            "the surviving crop is handed on byte for byte"
        );
        assert_eq!(
            rewrite.calibration.verdict, "deposit_exact",
            "one known level still fits, but no longer separates alpha from W"
        );
        assert!(
            rewrite.planes.is_some(),
            "a deposit-exact fit still carries a model"
        );
        // `c` is deliberately NOT compared: the kept crop sits on black, where `c = I` under
        // both fits, so the two agree exactly — which is the physics, not a carried-over
        // plane. `s` is what the lost level was paying for, and it must have moved.
        assert_ne!(
            rewrite.planes.as_ref().map(|planes| planes.s.clone()),
            built.planes.as_ref().map(|planes| planes.s.clone()),
            "the planes must be REFITTED, not carried over from the two-crop model"
        );
        assert_eq!(
            conditioning_from_stored(&rewrite.calibration)
                .and_then(|verdict| verdict.alpha_uncertainty().map(|alpha| alpha.source)),
            Some(AlphaSource::Assumed),
            "with one level left the alpha scale rests on an assumption, and says so"
        );
        // Geometry, name and anchors are the entry's own and survive the edit.
        assert_eq!((rewrite.width, rewrite.height), (built.width, built.height));
        assert_eq!(rewrite.name, built.name);
        assert_eq!(rewrite.anchors, built.anchors);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Deleting the LAST crop is allowed and leaves a template-only entry: no model, no
    /// planes, and a verdict that says exactly that. The decision to do it belongs to the UI
    /// (which confirms it); this layer must not pretend the state is impossible, because
    /// `save_entry` writes it and `load_entry` reads it back.
    #[test]
    fn deleting_the_last_crop_leaves_a_template_only_entry() {
        let (built, dir) = separable_entry("drop-last");
        let one_left = drop_entry_sample(loaded_from("wm-drop-last", &built), 0)
            .expect("dropping one of two crops must succeed");
        let empty = drop_entry_sample(loaded_from("wm-drop-last", &one_left), 0)
            .expect("dropping the last crop must succeed, not error");

        assert!(empty.samples.is_empty());
        assert!(empty.planes.is_none(), "no crops, no model");
        assert_eq!(empty.calibration.verdict, "not_enough_samples");
        assert_eq!(empty.calibration.samples, 0);
        assert_eq!(
            (empty.width, empty.height),
            (built.width, built.height),
            "the template — and therefore the footprint — outlives the crops"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An index past the end is refused with a reason instead of panicking: the row list the
    /// user clicked may have been rewritten by another instance since it was read.
    #[test]
    fn a_crop_index_past_the_end_is_refused() {
        let (built, dir) = separable_entry("drop-oob");
        let err = drop_entry_sample(loaded_from("wm-drop-oob", &built), 9)
            .expect_err("an index past the end must be refused");
        assert!(!err.is_empty());
        let _ = std::fs::remove_dir_all(dir);
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
