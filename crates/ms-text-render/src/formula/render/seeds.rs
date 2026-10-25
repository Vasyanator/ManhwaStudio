/*
File: crates/ms-text-render/src/formula/render/seeds.rs

Purpose:
Glyph seeds of the on-path layouts: collecting `FormulaGlyphSeed`s from the shaped
buffer and assigning their advances. `assign_formula_seed_advances` is one of the
CUSTOM KERNING sites (`MODULE_README.md` of the crate): the pair decision comes from
`pair_advance`, user-authored pairs replace the font's value under every mode.

Key functions:
- collect_formula_glyph_seeds(), glyph_style_offset()
- assign_formula_seed_advances(), seed_metric_advance_px()
- optical_horizontal_pair_adjustment(), glyph_ink_profile() / _from_image()
*/

use crate::font_registry::CustomKerningMap;
use crate::vector::OutlineCache;
use crate::inline_styles::InlineGlyphOffset;
use crate::pair_advance::{
    KerningSettings, PairAdvanceModel, PairStep, StepDirection, TrackingBasis, single_char_cluster,
};
use crate::pipeline::{
    FauxGlyphStyle, GlyphScaleSettings, RunPen, RunShaping, build_wrapped_hyphen_glyph,
    compute_inline_line_aligns, faux_style_at_offset, faux_style_for_glyph, hanging_edge_run_bounds,
    horizontal_line_offset, horizontal_run_baseline_y, inline_glyph_offset_at_offset,
    inline_glyph_offset_for_glyph, inline_glyph_offset_span_at_offset,
    inline_glyph_offset_span_for_glyph, inline_glyph_offset_style_at_offset,
    inline_glyph_offset_style_for_glyph, inline_glyph_scale_at_offset, inline_glyph_scale_for_glyph,
    inline_kerning_at_offset, inline_kerning_for_glyph, inline_text_color_at_offset,
    inline_text_color_for_glyph, is_edge_run_hanging, resolve_faux_counter_flag,
    run_wraps_at_soft_hyphen, soft_hyphen_style_offset, spans_have_inline_size_overrides,
    trailing_hyphen_x,
};
use crate::raster::sample_swash_alpha;
use crate::types::KerningMode;
use cosmic_text::{Buffer, FontSystem, LayoutGlyph, SwashCache, SwashContent};
use super::{FormulaGlyphSeed, GlyphInkProfile};

/// One seed per shaped glyph (plus the synthesized wrap hyphen) with every inline
/// override resolved and its line origin (`line_baselines`, falling back to the
/// run's own baseline at `base_line_height_px`); advances are assigned afterwards
/// by `assign_formula_seed_advances`.
pub(super) fn collect_formula_glyph_seeds(
    shaping: RunShaping<'_>,
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
    buffer: &mut Buffer,
    line_baselines: &[f32],
    base_line_height_px: f32,
) -> Vec<FormulaGlyphSeed> {
    let RunPen {
        params,
        inline_style_spans,
        layout_line_offsets,
        custom_kerning,
        font_size_px,
    } = shaping.pen;
    let width_px = params.width_px.max(1);
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let mut line_counts = Vec::<usize>::new();
    let mut line_idx = 0usize;
    for run in buffer.layout_runs() {
        if run.glyphs.is_empty() {
            line_idx += 1;
            continue;
        }
        if line_counts.len() <= line_idx {
            line_counts.push(0);
        }
        line_counts[line_idx] += run.glyphs.len();
        line_idx += 1;
    }

    let mut out = Vec::<FormulaGlyphSeed>::new();
    let mut line_seen = vec![0usize; line_counts.len().max(1)];
    let inline_line_aligns =
        compute_inline_line_aligns(params.align, layout_line_offsets, inline_style_spans);
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    // Leading/trailing hanging-punctuation runs are marked (not dropped): the glyphs still
    // DRAW, they are only kept out of the extra-info center sampling. Computed only when that
    // sampling is active and punctuation hangs at or above the exclusion threshold, so the
    // default path pays nothing.
    let mark_hanging = params.extra_info.is_active() && params.excludes_hanging_from_extra_info();
    while let Some(run) = runs.next() {
        let hanging_bounds = if mark_hanging {
            hanging_edge_run_bounds(&run)
        } else {
            (0, run.glyphs.len())
        };
        let line_align = inline_line_aligns
            .get(line_idx)
            .copied()
            .unwrap_or(params.align);
        let line_offset_x = horizontal_line_offset(width_px, run.line_w, line_align) as f32;
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or_else(|| {
            horizontal_run_baseline_y(
                &run,
                line_idx,
                run.line_y,
                base_line_height_px,
                0.0,
                has_inline_size_overrides,
            )
        });
        for (glyph_idx_in_run, glyph) in run.glyphs.iter().enumerate() {
            let glyph_idx_in_line = line_seen
                .get_mut(line_idx)
                .map(|value| {
                    let idx = *value;
                    *value += 1;
                    idx
                })
                .unwrap_or(0);
            out.push(FormulaGlyphSeed {
                cluster_char: single_char_cluster(run.text, glyph),
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    params.text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                origin_x: line_offset_x,
                origin_y: baseline_y,
                // Keeps the run off the byte-identical fast path when this glyph's
                // face carries overrides; the PAIRS are resolved in
                // `assign_formula_seed_advances`.
                kerning: PairAdvanceModel::new(custom_kerning).kerning_for_glyph(
                    inline_kerning_for_glyph(
                        params,
                        inline_style_spans,
                        layout_line_offsets,
                        run.line_i,
                        glyph,
                    ),
                    glyph,
                ),
                glyph_scale: inline_glyph_scale_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_offset_px: inline_glyph_offset_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                extended_offset: inline_glyph_offset_style_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                style_offset: glyph_style_offset(layout_line_offsets, run.line_i, glyph),
                offset_span_range: inline_glyph_offset_span_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                line_idx,
                glyph_idx_in_line,
                glyphs_in_line: line_counts.get(line_idx).copied().unwrap_or(1),
                line_align,
                advance_px: 0.0,
                faux: resolve_faux_counter_flag(
                    faux_style_for_glyph(
                        params,
                        inline_style_spans,
                        layout_line_offsets,
                        run.line_i,
                        glyph,
                    ),
                    font_system,
                    outline_cache,
                    glyph,
                ),
                hanging_excluded: mark_hanging
                    && is_edge_run_hanging(hanging_bounds, glyph_idx_in_run),
            });
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(mut hyphen_glyph) =
                build_wrapped_hyphen_glyph(font_system, shaping, &run, runs.peek())
        {
            hyphen_glyph.x = trailing_hyphen_x(&run);
            let glyph_idx_in_line = line_seen
                .get_mut(line_idx)
                .map(|value| {
                    let idx = *value;
                    *value += 1;
                    idx
                })
                .unwrap_or(0);
            let style_offset = soft_hyphen_style_offset(&run, runs.peek(), layout_line_offsets);
            // Resolved before the struct literal below moves `hyphen_glyph`.
            let hyphen_faux = resolve_faux_counter_flag(
                style_offset
                    .map(|offset| {
                        faux_style_at_offset(
                            params,
                            inline_style_spans,
                            offset,
                            hyphen_glyph.font_size,
                        )
                    })
                    .unwrap_or(FauxGlyphStyle::NONE),
                font_system,
                outline_cache,
                &hyphen_glyph,
            );
            out.push(FormulaGlyphSeed {
                // Synthesized, not shaped from the layout text: it has no source
                // cluster, so it never participates in a user-authored pair.
                cluster_char: None,
                glyph: hyphen_glyph,
                text_color: style_offset
                    .map(|offset| {
                        inline_text_color_at_offset(params.text_color, inline_style_spans, offset)
                    })
                    .unwrap_or(params.text_color),
                origin_x: line_offset_x,
                origin_y: baseline_y,
                kerning: style_offset
                    .map(|offset| inline_kerning_at_offset(params, inline_style_spans, offset))
                    .unwrap_or_else(|| KerningSettings::from_params(params)),
                glyph_scale: style_offset
                    .map(|offset| inline_glyph_scale_at_offset(params, inline_style_spans, offset))
                    .unwrap_or_else(|| GlyphScaleSettings::from_params(params)),
                glyph_offset_px: style_offset
                    .map(|offset| inline_glyph_offset_at_offset(inline_style_spans, offset))
                    .unwrap_or([0.0, 0.0]),
                extended_offset: style_offset
                    .map(|offset| inline_glyph_offset_style_at_offset(inline_style_spans, offset))
                    .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0])),
                style_offset: style_offset
                    .unwrap_or_else(|| layout_line_offsets.get(run.line_i).copied().unwrap_or(0)),
                offset_span_range: style_offset.and_then(|offset| {
                    inline_glyph_offset_span_at_offset(inline_style_spans, offset)
                }),
                line_idx,
                glyph_idx_in_line,
                glyphs_in_line: line_counts.get(line_idx).copied().unwrap_or(1),
                line_align,
                advance_px: 0.0,
                faux: hyphen_faux,
                // The wrapped soft hyphen is real ink and never hangs, so it always
                // contributes to the extra-info samples (matches the horizontal path).
                hanging_excluded: false,
            });
        }
        line_idx += 1;
    }

    assign_formula_seed_advances(
        out.as_mut_slice(),
        font_system,
        custom_kerning,
        font_size_px,
        (font_size_px * 0.5).max(1.0),
    );
    out
}

/// The shaped pen delta between two adjacent glyphs, turned into a POSITIVE
/// magnitude fit for an on-path walk.
///
/// `raw_delta_px` is `next.glyph.x - this.glyph.x` straight out of cosmic-text
/// and `glyph_w_px` is the left glyph's shaped advance width. Returns
/// `max(raw_delta_px, glyph_w_px * 0.25, 1.0)`.
///
/// Why the floor exists at all: on-path layouts walk an arc length, so a seed
/// advance is a DISTANCE, not a signed x step (see `assign_formula_seed_advances`).
/// Two inputs break that. A right-to-left or otherwise non-monotonic run has
/// DECREASING `glyph.x`, so `raw_delta_px` is negative and would walk the cursor
/// backwards. And a font pair-kerning value below `-75%` of the left glyph's own
/// advance would collapse the step to (near) nothing, stacking two glyphs on one
/// curve point. The quarter-advance term is the tunable part: it keeps a badly
/// kerned pair legible by refusing to overlap the glyphs by more than three
/// quarters, and the `1.0` term is the absolute guard for a zero-width glyph.
///
/// What this floor is NOT: it does not clip user-authored kerning. An authored
/// pair takes the `custom_delta_px` branch of `assign_formula_seed_advances`,
/// which steps by the left glyph's own NOMINAL advance plus the authored delta
/// and uses this value only as the fallback when that nominal metric is missing.
/// A strongly negative authored pair therefore passes through here untouched and
/// is clamped later, by [`OnPathStepSpacing::step_px`].
#[must_use]
pub(super) fn seed_metric_advance_px(raw_delta_px: f32, glyph_w_px: f32) -> f32 {
    let glyph_width_floor = (glyph_w_px * 0.25).max(1.0);
    raw_delta_px.max(glyph_width_floor).max(1.0)
}

/// Fills every seed's `advance_px` — the pen step from that glyph to the NEXT one
/// on the same line — from the shaped positions, the kerning mode and the
/// user-authored overrides.
///
/// Custom kerning mirrors `pipeline::horizontal_run_layout` exactly: a pair listed
/// in the left glyph's font table REPLACES the font's own value for that pair under
/// every mode, stepping by `nominal_glyph_advance_px(left) + delta`. Uniform
/// tracking is still added on top.
///
/// DIRECTION: unlike the horizontal pen, `advance_px` is a MAGNITUDE along the
/// drawn line, never a signed x step — the shaped delta is floored positive by
/// [`seed_metric_advance_px`], and every consumer floors the result AGAIN through
/// the single [`OnPathStepSpacing::step_px`] owner before walking the path. So the
/// RTL sign mirroring `StepDirection::FollowRun` performs has no counterpart
/// on this path: seeds are advanced along the curve in logical order regardless of
/// script direction. Formula/drawn lines are LTR-only by construction, which is a
/// limitation of that whole path, not of the overrides.
///
/// The value written into a seed is NOT guaranteed positive: the custom-pair and
/// tracking branches below add signed terms (`custom_delta_px`,
/// `KerningSettings::extra_spacing_px`) on top of an already-floored metric
/// advance and are deliberately not re-floored, so a strongly negative authored
/// pair leaves a negative `advance_px` here. Making it positive is the consumers'
/// job, and theirs alone — [`OnPathStepSpacing::step_px`].
pub(super) fn assign_formula_seed_advances(
    seeds: &mut [FormulaGlyphSeed],
    font_system: &mut FontSystem,
    custom_kerning: &CustomKerningMap,
    font_size_px: f32,
    default_advance: f32,
) {
    if seeds
        .iter()
        .all(|seed| seed.kerning.uses_default_metric_layout())
    {
        let mut idx = 0usize;
        while idx < seeds.len() {
            let line_idx = seeds[idx].line_idx;
            let line_start = idx;
            idx += 1;
            while idx < seeds.len() && seeds[idx].line_idx == line_idx {
                idx += 1;
            }
            let line_end = idx;
            let mut prev_advance = default_advance;
            for glyph_idx in line_start..line_end {
                let advance_px = if glyph_idx + 1 < line_end {
                    seed_metric_advance_px(
                        seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x,
                        seeds[glyph_idx].glyph.w,
                    )
                } else if glyph_idx > line_start {
                    prev_advance
                } else {
                    seeds[glyph_idx].glyph.w.max(default_advance).max(1.0)
                };
                seeds[glyph_idx].advance_px = advance_px;
                prev_advance = advance_px;
            }
        }
        return;
    }

    let model = PairAdvanceModel::new(custom_kerning);
    let mut cache = SwashCache::new();
    let profiles = seeds
        .iter()
        .map(|seed| glyph_ink_profile(font_system, &mut cache, &seed.glyph, font_size_px))
        .collect::<Vec<_>>();
    let mut idx = 0usize;
    while idx < seeds.len() {
        let line_idx = seeds[idx].line_idx;
        let line_start = idx;
        idx += 1;
        while idx < seeds.len() && seeds[idx].line_idx == line_idx {
            idx += 1;
        }
        let line_end = idx;
        let mut prev_advance = default_advance;
        for glyph_idx in line_start..line_end {
            let advance_px = if glyph_idx + 1 < line_end {
                let metric_advance = seed_metric_advance_px(
                    seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x,
                    seeds[glyph_idx].glyph.w,
                );
                let kerning = seeds[glyph_idx + 1].kerning;
                // A user-authored pair REPLACES the font's own value for that pair
                // under every mode, exactly as on the horizontal path; the step is a
                // MAGNITUDE along the path, never mirrored.
                let step = model.resolve(
                    font_system,
                    kerning.mode,
                    StepDirection::Magnitude,
                    seeds[glyph_idx].pair_glyph(),
                    seeds[glyph_idx + 1].pair_glyph(),
                    metric_advance,
                );
                let base_advance = match (step, kerning.mode) {
                    // An override, or `Fixed`'s own (un-kerned) advance: already final.
                    (PairStep::Override { step_px, .. } | PairStep::OwnAdvance { step_px }, _) => {
                        step_px
                    }
                    // `Auto` keeps the shaped (font-pair-kerned) advance. `Fixed` never
                    // gets here (the owner answers it with `OwnAdvance`); it is listed only
                    // to keep the match exhaustive.
                    (PairStep::Shaped { step_px }, KerningMode::Auto | KerningMode::Fixed) => step_px,
                    // `Optical` adds this path's own ink-profile adjustment (NOT
                    // `optical.rs`: a known, kept divergence, see MODULE_README).
                    (PairStep::Shaped { step_px }, KerningMode::Optical) => {
                        step_px
                            + optical_horizontal_pair_adjustment(
                                profiles[glyph_idx],
                                profiles[glyph_idx + 1],
                                metric_advance,
                                font_size_px,
                            )
                    }
                };
                // The tracking basis follows the branch the STEP actually took: an
                // overridden pair is a metric step, not an optical one.
                let tracking_basis = match (step.is_override(), kerning.mode) {
                    (true, _) | (false, KerningMode::Auto | KerningMode::Fixed) => {
                        TrackingBasis::SeedMetric {
                            metric_advance_px: metric_advance,
                        }
                    }
                    (false, KerningMode::Optical) => TrackingBasis::SeedOpticalInk {
                        mean_ink_width_px: (profiles[glyph_idx].width_px()
                            + profiles[glyph_idx + 1].width_px())
                            * 0.5,
                        default_advance_px: default_advance,
                    },
                };
                base_advance + kerning.tracking_px(tracking_basis)
            } else if glyph_idx > line_start {
                prev_advance
            } else {
                seeds[glyph_idx].glyph.w.max(default_advance).max(1.0)
            };
            seeds[glyph_idx].advance_px = advance_px;
            prev_advance = advance_px;
        }
    }
}

fn glyph_style_offset(
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> usize {
    layout_line_offsets.get(line_idx).copied().unwrap_or(0) + glyph.start.min(glyph.end)
}

/// The formula seeds' own `Optical` nudge (px) for one pair, from the two glyphs'
/// bitmap ink profiles: pulls the pair's ink gap toward a size-relative target,
/// clamped; `0.0` for a non-positive or non-finite `metric_advance` and for
/// nudges under a quarter pixel. Not the shared `optical.rs` formula (a kept
/// divergence, see MODULE_README).
fn optical_horizontal_pair_adjustment(
    prev: GlyphInkProfile,
    next: GlyphInkProfile,
    metric_advance: f32,
    font_size_px: f32,
) -> f32 {
    if !metric_advance.is_finite() || metric_advance <= 0.0 {
        return 0.0;
    }

    let avg_width = ((prev.width_px() + next.width_px()) * 0.5).max(font_size_px * 0.3);
    let actual_gap = metric_advance + next.left_px - prev.right_px;
    let target_gap = (avg_width * 0.08).clamp(font_size_px * 0.02, font_size_px * 0.14);
    let tighten_limit = metric_advance.min(font_size_px * 0.14) * 0.55;
    let loosen_limit = font_size_px * 0.18;
    let delta = ((target_gap - actual_gap) * 0.55).clamp(-tighten_limit, loosen_limit);
    if delta.abs() < 0.25 { 0.0 } else { delta }
}

fn glyph_ink_profile(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
) -> GlyphInkProfile {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
        return GlyphInkProfile::fallback(glyph.w.max(font_size_px * 0.5), font_size_px);
    };
    glyph_ink_profile_from_image(
        [
            image.placement.left as f32,
            (physical.y - image.placement.top) as f32,
        ],
        &image.content,
        image.data.as_slice(),
        [
            image.placement.width as usize,
            image.placement.height as usize,
        ],
        [glyph.w.max(font_size_px * 0.5), font_size_px],
    )
}

fn glyph_ink_profile_from_image(
    draw_origin_px: [f32; 2],
    content: &SwashContent,
    data: &[u8],
    glyph_size: [usize; 2],
    fallback_size_px: [f32; 2],
) -> GlyphInkProfile {
    let [draw_left_px, _draw_top_px] = draw_origin_px;
    let [glyph_w, glyph_h] = glyph_size;
    let [fallback_width_px, fallback_height_px] = fallback_size_px;
    if glyph_w == 0 || glyph_h == 0 {
        return GlyphInkProfile::fallback(fallback_width_px, fallback_height_px);
    }

    let mut min_x = glyph_w;
    let mut min_y = glyph_h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut has_alpha = false;

    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            if sample_swash_alpha(content, data, glyph_w, gx, gy) < 12 {
                continue;
            }
            min_x = min_x.min(gx);
            min_y = min_y.min(gy);
            max_x = max_x.max(gx);
            max_y = max_y.max(gy);
            has_alpha = true;
        }
    }

    if !has_alpha {
        return GlyphInkProfile::fallback(fallback_width_px, fallback_height_px);
    }

    GlyphInkProfile {
        left_px: draw_left_px + min_x as f32,
        right_px: draw_left_px + max_x as f32 + 1.0,
    }
}
