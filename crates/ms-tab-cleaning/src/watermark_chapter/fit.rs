/*
File: watermark_chapter/fit.rs

Purpose:
Model estimation: chooses the fit plan from the samples (least squares over separated known
levels, Theil-Sen against estimated backgrounds, or the deposit-exact graded fit) and solves
the per-pixel per-channel `c`/`s` planes.

Key items:
- `estimate_model()` (entry)
- `FitPlan`, `FittedPlanes` (private plan / result)
- `plan_fit()`, `regress_planes()`, `deposit_exact_planes()`, robust line fits, `median()`

Notes:
`alpha_lower_bound()` is shared with `model.rs`.
*/
use std::sync::Arc;
use rayon::prelude::*;
use super::{
    ASSUMED_ALPHA_OVER_DEPOSIT_BOUND, ASSUMED_ALPHA_UNCERTAINTY_PERCENT, AlphaAssumption,
    AlphaSource, AlphaUncertainty, CalibrationSample, CompositingOperator,
    ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT, FitMethod,
    MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT, MIN_BACKGROUND_SPREAD, MIN_PAIR_SPREAD,
    ModelConditioning, ModelFitError, ModelProvenance, SAME_LEVEL_EPS, S_CEIL, S_FLOOR,
    WatermarkError, WatermarkModel, count_f32,
};


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
pub(crate) fn estimate_model(
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
    let alpha = match plan.method {
        FitMethod::ClosedFormFlat => AlphaUncertainty::from_flat_fit(plan.spread, peak_alpha),
        FitMethod::TheilSen => AlphaUncertainty::from_percent(
            AlphaSource::EstimatedBackgrounds,
            ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT,
        ),
        FitMethod::DepositExact => {
            AlphaUncertainty::from_percent(AlphaSource::Assumed, fit.alpha_uncertainty_percent)
        }
    };
    // A fit that ran on even one ASSERTED background level is downgraded here rather than at
    // any reporting site: the verdict is the one thing every caller already reads, so the
    // assumption cannot be lost on the way out. The figure is never improved, only floored.
    let alpha = downgrade_for_manual_backgrounds(alpha, plan.manual_inputs);
    let conditioning = match plan.method {
        FitMethod::ClosedFormFlat | FitMethod::TheilSen => ModelConditioning::Separable {
            levels: plan.levels,
            spread: plan.spread,
            min_pixel_spread: plan.min_pixel_spread,
            alpha,
        },
        FitMethod::DepositExact => ModelConditioning::DepositExact {
            levels: plan.levels,
            spread: plan.spread,
            samples: fit_input.len(),
            alpha,
        },
    };
    let provenance = ModelProvenance {
        samples: samples.len(),
        method: plan.method,
        conditioning,
        clamped_pixels: fit.clamped_pixels,
        manual_backgrounds: plan.manual_inputs,
        partial_rings: plan.partial_inputs,
    };
    WatermarkModel::from_parts(width, height, fit.c, fit.s, operator, provenance)
        .map_err(ModelFitError::Invalid)
}

/// Re-price an alpha uncertainty when the fit rested on hand-asserted background levels.
///
/// Returns `alpha` untouched when `manual_inputs` is zero. Otherwise the source becomes
/// [`AlphaSource::ManualBackgrounds`] and the percentage is raised to at least
/// `MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT` — never lowered, so a fit that was
/// already worse keeps its own figure. The LSB costs follow from the percentage through the
/// engine's measured constants, exactly as for every other source.
fn downgrade_for_manual_backgrounds(alpha: AlphaUncertainty, manual_inputs: usize) -> AlphaUncertainty {
    if manual_inputs == 0 {
        return alpha;
    }
    AlphaUncertainty::from_percent(
        AlphaSource::ManualBackgrounds,
        alpha
            .percent
            .max(MANUAL_BACKGROUND_ALPHA_UNCERTAINTY_FLOOR_PERCENT),
    )
}

/// Which fit the samples license, on which of them, and the levels the verdict will report.
struct FitPlan {
    method: FitMethod,
    /// Indices into the caller's sample slice that the fit runs on.
    inputs: Vec<usize>,
    /// How many of `inputs` carry a hand-asserted background level rather than a measured one.
    manual_inputs: usize,
    /// How many of `inputs` had their background measured from a ring the page edge truncated.
    partial_inputs: usize,
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
    // "Known level" rather than "measured level": a hand-asserted level states the same single
    // `B` the closed form needs, so it feeds the fit identically. What it does NOT do is claim
    // to be evidence, which is what `manual_count` carries out of here.
    let flat: Vec<usize> = (0..samples.len())
        .filter(|&i| samples[i].has_known_level())
        .collect();
    let manual_count = |indices: &[usize]| indices.iter().filter(|&&i| samples[i].is_manual()).count();
    // Counted the same way and for the same reason: the fit cannot tell a truncated ring from a
    // complete one, so the distinction has to be carried out of here rather than re-derived.
    let partial_count =
        |indices: &[usize]| indices.iter().filter(|&&i| samples[i].has_partial_ring()).count();
    let mut flat_levels: Vec<f32> = flat
        .iter()
        .map(|&index| samples[index].mean_background_luma())
        .collect();
    flat_levels.sort_by(f32::total_cmp);
    let flat_spread = luma_spread(&flat_levels);

    // 1. Known backgrounds, far enough apart: the slope comes out of the levels, not an
    //    assumption about alpha.
    if flat.len() >= 2 && flat_spread >= MIN_BACKGROUND_SPREAD {
        let manual_inputs = manual_count(&flat);
        let partial_inputs = partial_count(&flat);
        return Ok(FitPlan {
            method: FitMethod::ClosedFormFlat,
            inputs: flat,
            manual_inputs,
            partial_inputs,
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
            let inputs: Vec<usize> = (0..samples.len()).collect();
            let manual_inputs = manual_count(&inputs);
            let partial_inputs = partial_count(&inputs);
            return Ok(FitPlan {
                method: FitMethod::TheilSen,
                inputs,
                manual_inputs,
                partial_inputs,
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

    // 3. At least one known background: the deposit follows exactly from it everywhere under the
    //    mark, so removal at that level is exact and only the alpha scale is an assumption.
    if !flat.is_empty() {
        let manual_inputs = manual_count(&flat);
        let partial_inputs = partial_count(&flat);
        return Ok(FitPlan {
            method: FitMethod::DepositExact,
            inputs: flat,
            manual_inputs,
            partial_inputs,
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
/// With every calibration background known (measured, or asserted by the user), `D = B - I`
/// follows exactly from it, and for any
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
                // Measured or asserted alike: this pass only needs the single `B` under the
                // footprint. A per-pixel estimate has none, and never reaches this fit.
                let Some(level) = sample.background().known_level() else {
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
                    let Some(level) = sample.background().known_level() else {
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
pub(super) fn alpha_lower_bound(background: f32, observed: f32) -> f32 {
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
