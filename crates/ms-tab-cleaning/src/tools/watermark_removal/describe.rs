/*
File: tools/watermark_removal/describe.rs

Purpose:
User-facing wording of engine verdicts: sample verdicts, model conditioning, alpha
uncertainty, residual, library quality and the background histogram.

Key items:
- `describe_sample_verdict()`, `describe_conditioning()`, `describe_alpha_uncertainty()`,
  `describe_residual()`, `library_quality_label()`, `describe_histogram()`

Notes:
Wording is load-bearing (see `describe_conditioning`). `describe_conditioning` is also used by
`tools/watermark_library_window.rs` through the parent's re-export.
*/
use crate::tools::watermark_entry::luma_of_level;
use crate::watermark_chapter::{
    AlphaSource, AlphaUncertainty, ModelConditioning, RemovalResidual, SampleRejection,
    SampleVerdict, SuggestedBackground,
};
use super::CHAPTER_HISTOGRAM_BINS;


// ---------------------------------------------------------------------------------------
// Chapter mode: report text
// ---------------------------------------------------------------------------------------

/// Localized report of one sample verdict, including the measured reason a selection was
/// refused as a calibration target.
pub(super) fn describe_sample_verdict(verdict: &SampleVerdict) -> String {
    match verdict {
        // A partial ring gets its OWN wording rather than the same sentence with a footnote:
        // "measured against the page edge" is what the user can act on, and hiding it inside
        // the ordinary line would let a one-sided measurement read as a complete one.
        SampleVerdict::Calibration {
            level,
            ring_std,
            ring,
        } if ring.is_partial() => tf!(
            "cleaning.tools.watermark.chapter.sample_calibration_partial",
            level = format_levels(&[luma_of_level(*level)]),
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max)),
            pixels = ring.pixels,
            full = ring.full_pixels
        ),
        SampleVerdict::Calibration {
            level, ring_std, ..
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_calibration",
            level = format_levels(&[luma_of_level(*level)]),
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max))
        ),
        SampleVerdict::TemplateOnly {
            ring_std,
            ring_max_dev,
            std_limit,
            max_dev_limit,
            ..
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_template_only",
            std = format!("{:.1}", ring_std.iter().copied().fold(0.0f32, f32::max)),
            std_limit = format!("{std_limit:.1}"),
            max_dev = format!("{:.1}", ring_max_dev.iter().copied().fold(0.0f32, f32::max)),
            max_dev_limit = format!("{max_dev_limit:.1}")
        ),
        SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        } => t!("cleaning.tools.watermark.chapter.sample_unusable_rect").to_string(),
        SampleVerdict::Unusable {
            reason: SampleRejection::RingTooSmall { pixels, needed },
        } => tf!(
            "cleaning.tools.watermark.chapter.sample_unusable_ring",
            pixels = pixels,
            needed = needed
        ),
    }
}

/// Background levels as a compact list.
fn format_levels(levels: &[f32]) -> String {
    levels
        .iter()
        .map(|level| format!("{level:.0}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The GRADED conditioning verdict in the user's words.
///
/// Wording is load-bearing (`dev-docs/watermark_chapter_decomposition_plan.md`,
/// "Corrections from the second implementation round"): what is measured exactly is the
/// IMPRINT, never «c»; the stated percentage bounds the alpha SCALE only.
///
/// `rests_on_assertion` is `true` when the model behind `conditioning` was fitted on at
/// least one HAND-STATED background level (`ModelProvenance::manual_backgrounds` is
/// non-zero). It is a CORRECTNESS input, not a decoration: such a model may never be
/// described as measured, whatever the verdict tag says, so the two verdicts that claim a
/// measurement switch to their `_asserted` wording. The library card enforces the same rule
/// through `entry_verdict_line`, and the two must not be able to disagree about one entry.
/// `pub(in crate::tools)` (re-exported by `watermark_removal`) for one reason only: the library
/// panel's own wording test asserts that this function and `entry_verdict_line` cannot
/// disagree about the SAME entry, and the obligation is only worth testing when both halves
/// are checked together.
pub(in crate::tools) fn describe_conditioning(
    conditioning: &ModelConditioning,
    rests_on_assertion: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match conditioning {
        ModelConditioning::Separable { .. } if rests_on_assertion => {
            t!("cleaning.tools.watermark.chapter.verdict_separable_asserted").to_string()
        }
        ModelConditioning::Separable { .. } => {
            t!("cleaning.tools.watermark.chapter.verdict_separable").to_string()
        }
        ModelConditioning::DepositExact { .. } if rests_on_assertion => {
            t!("cleaning.tools.watermark.chapter.verdict_deposit_exact_asserted").to_string()
        }
        ModelConditioning::DepositExact { .. } => {
            t!("cleaning.tools.watermark.chapter.verdict_deposit_exact").to_string()
        }
        ModelConditioning::NotEnoughSamples { have, need } => tf!(
            "cleaning.tools.watermark.chapter.verdict_not_enough",
            have = have,
            need = need
        ),
        ModelConditioning::DepositUnavailable { samples, spread } => tf!(
            "cleaning.tools.watermark.chapter.verdict_deposit_unavailable",
            samples = samples,
            spread = format!("{spread:.0}")
        ),
        ModelConditioning::Underdetermined {
            underdetermined_pixels,
            total_pixels,
            worst_pixel_spread,
            required,
            ..
        } => tf!(
            "cleaning.tools.watermark.chapter.verdict_underdetermined",
            pixels = underdetermined_pixels,
            total = total_pixels,
            spread = format!("{worst_pixel_spread:.0}"),
            required = format!("{required:.0}")
        ),
    });
    let levels = conditioning.levels();
    lines.push(if levels.is_empty() {
        t!("cleaning.tools.watermark.chapter.levels_none").to_string()
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.levels_line",
            levels = format_levels(levels),
            spread = format!("{:.0}", conditioning.spread())
        )
    });
    if let Some(alpha) = conditioning.alpha_uncertainty() {
        lines.push(describe_alpha_uncertainty(&alpha));
    }
    if let Some(suggestion) = conditioning.suggested_background() {
        lines.push(match suggestion {
            SuggestedBackground::Darker { at_most } => tf!(
                "cleaning.tools.watermark.chapter.suggest_darker",
                level = format!("{at_most:.0}")
            ),
            SuggestedBackground::Brighter { at_least } => tf!(
                "cleaning.tools.watermark.chapter.suggest_brighter",
                level = format!("{at_least:.0}")
            ),
        });
    }
    lines
}

/// How well the alpha SCALE is pinned, and what that costs in LSB.
fn describe_alpha_uncertainty(alpha: &AlphaUncertainty) -> String {
    let source = match alpha.source {
        AlphaSource::SeparatedBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_separated")
        }
        AlphaSource::EstimatedBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_estimated")
        }
        AlphaSource::ManualBackgrounds => {
            t!("cleaning.tools.watermark.chapter.alpha_source_manual")
        }
        AlphaSource::Assumed => t!("cleaning.tools.watermark.chapter.alpha_source_assumed"),
    };
    tf!(
        "cleaning.tools.watermark.chapter.alpha_line",
        percent = format!("{:.0}", alpha.percent),
        source = source,
        rms = format!("{:.1}", alpha.rms_lsb),
        dark_luma = format!("{:.0}", alpha.dark_luma),
        dark_rms = format!("{:.1}", alpha.dark_rms_lsb),
        dark_max = format!("{:.0}", alpha.dark_max_lsb)
    )
}

/// The quantization-and-clipping report of an apply.
///
/// Deliberately NOT a quality score: by construction the recomposition error is bounded by
/// `s*0.5` unless the recovery was clipped, so a badly fitted model produces a small
/// residual just as happily as a good one. Model quality is the detection gain and the
/// t-statistic, reported per mark.
pub(super) fn describe_residual(residual: &RemovalResidual) -> String {
    tf!(
        "cleaning.tools.watermark.chapter.quantization_line",
        exact = format!("{:.1}", residual.exact_share() * 100.0),
        clipped = format!("{:.1}", residual.clipped_share() * 100.0),
        uncertainty = format!("{:.2}", residual.max_uncertainty_lsb)
    )
}

/// One word for a stored entry's calibration quality, for the match picker.
///
/// Only two stored verdicts carry a model at all; anything else is reported as "no model"
/// rather than dressed up as a grade.
pub(super) fn library_quality_label(verdict: &str) -> &'static str {
    match verdict {
        "separable" => t!("cleaning.tools.watermark.chapter.library_quality_exact"),
        "deposit_exact" => t!("cleaning.tools.watermark.chapter.library_quality_graded"),
        _ => t!("cleaning.tools.watermark.chapter.library_quality_none"),
    }
}

/// The background histogram as a compact `bin: count` list, empty bins dropped.
pub(super) fn describe_histogram(histogram: &[u32; CHAPTER_HISTOGRAM_BINS]) -> String {
    let step = 256 / CHAPTER_HISTOGRAM_BINS;
    histogram
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(bin, count)| format!("{}-{}: {count}", bin * step, (bin + 1) * step - 1))
        .collect::<Vec<_>>()
        .join("; ")
}
