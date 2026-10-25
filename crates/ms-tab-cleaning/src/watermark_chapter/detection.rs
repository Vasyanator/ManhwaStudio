/*
File: watermark_chapter/detection.rs

Purpose:
Detection: anchor discovery on a downscaled plane, anchor-band NCC search, the per-pixel
background gain test, chapter scans with overlap resolution, subpixel shift estimation and the
provisional background under an occurrence.

Key items:
- `discover_anchors()`, `find_occurrences()`, `scan_page()`, `scan_chapter()`
- `gain_test()` / `GainVerdict`, `padded_patch()` / `PaddedPatch`, `box_blur_rgb()`,
  `lucas_kanade_shift()`, `provisional_background()`

Notes:
`ChapterHit` and `CorrelationRef` are declared in `mod.rs`.
*/
use image::RgbaImage;
use rayon::prelude::*;
use super::{
    ALPHA_SIGNIFICANT, ANCHOR_CLUSTER_RADIUS_PX, ANCHOR_DISCOVERY_DOWNSCALE,
    ANCHOR_DISCOVERY_NCC_MIN, ANCHOR_MIN_SUPPORT, AcceptanceEvidence, BACKGROUND_WEIGHT_FLOOR,
    ChapterHit, CorrelationRef, DetectionParams, FALSE_ACCEPT_GAIN_FLOOR, MAX_DISCOVERY_HITS,
    MIN_DISCOVERY_TEMPLATE_SIDE, MIN_GAIN_PIXELS, MarkTemplate, Occurrence, PixelRect,
    SubpixelShift, WatermarkError, WatermarkKind, WatermarkModel, count_f32, luma_of, validate_page,
    validate_rect,
};
use super::template::mean_column;


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
pub(crate) fn discover_anchors(
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
pub(crate) fn find_occurrences(
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

/// Run every kind of `catalog` over one page and resolve overlaps between them.
///
/// Overlap resolution: gain-verified accepts always beat correlation-only ones, and within a
/// rank the higher [`Occurrence::score`] wins; the loser is dropped, not merged. A colour mark
/// therefore cannot be "removed" with the greyscale kind's model, which would subtract the
/// wrong `c`/`s`.
///
/// # Errors
/// Page geometry failures.
pub(crate) fn scan_page(
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
pub(crate) fn scan_chapter(
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
            .as_chunks::<4>().0.iter()
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
pub(crate) fn provisional_background(
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
