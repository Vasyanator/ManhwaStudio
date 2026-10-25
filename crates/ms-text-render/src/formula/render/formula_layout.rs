/*
File: crates/ms-text-render/src/formula/render/formula_layout.rs

Purpose:
The formula / shape layout: one render pass along the evaluated formula curve, the
shape-mode fallback decision (curve too short) and the curve arc-length table that maps
a target arc length to the curve parameter.

Key functions:
- render_text_with_formula_layout_once()
- detect_shape_layout_fallback_reason()
- build_formula_arc_length_table(), map_formula_target_arc_length(),
  formula_t01_for_arc_length()
*/

use crate::formula::{FormulaEvalInput, FormulaProgramBundle};
use crate::extra_info::{ExtraInfoAccumulator, rotated_box_samples};
use crate::glyph_blit::{
    GlyphBox, glyph_needs_bitmap_fallback, glyph_outline_transform, glyph_subpixel_offset,
};
use crate::vector::{
    MeshWarpContext, OutlineCache, OutlinePaint, RasterScratch, build_aa_lut,
    rasterize_outline_into,
};
use crate::pipeline::{RunPen, RunShaping, spans_have_inline_size_overrides};
use crate::raster::{
    PixelBounds, RgbaCanvasView, RigidPlacement, RotatedRect, bilinear_sample_rgba,
    blend_pixel_over, build_glyph_rgba_buffer, include_rotated_rect_bounds,
    rotate_placements_about_centroid, rotated_rect_world_bounds,
};
use crate::types::RenderedTextImage;
use cosmic_text::{Buffer, FontSystem, SwashCache};
use std::collections::HashMap;
use super::{
    FormulaArcLengthSample, FormulaGlyphTransform, FormulaLines, OnPathStepSpacing,
    apply_line_placement, resolve_glyph_outline,
};
use super::drawn_lines_layout::on_path_align_fraction;
use super::path_geometry::{apply_formula_group_rotations, is_last_seed_in_offset_span_on_line};
use super::seeds::collect_formula_glyph_seeds;

/// One formula / shape render at canvas margin `render_margin_pad` (the caller
/// retries with a larger margin while ink touches the edge). `line_placement_frac`
/// is the mode-gated perpendicular line placement.
pub(super) fn render_text_with_formula_layout_once(
    shaping: RunShaping<'_>,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    lines: FormulaLines<'_>,
    render_margin_pad: u32,
    line_placement_frac: f32,
) -> Result<RenderedTextImage, String> {
    let RunPen {
        params,
        inline_style_spans,
        font_size_px,
        ..
    } = shaping.pen;
    let base_line_height_px = lines.base_line_height_px;
    let width_px = params.width_px.max(1);
    let formula_program = FormulaProgramBundle::compile(&params.formula_layout)?;
    let mut cache = SwashCache::new();
    // Per-render outline cache for the vector composite pass (color glyphs fall
    // back to the bitmap blit, so they are never extracted here).
    let mut outline_cache = OutlineCache::new();
    // Reused per-glyph rasterizer buffers for the composite pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info (mean/median centers). A FRESH accumulator per `_once`
    // call (the retry loop re-runs this function from scratch), so the stored
    // extras always match the accepted image. Inactive by default -> a true no-op.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = lines.baselines(buffer, has_inline_size_overrides);
    let mut seeds = collect_formula_glyph_seeds(
        shaping,
        font_system,
        &mut outline_cache,
        buffer,
        line_baselines.as_slice(),
        base_line_height_px,
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let default_advance = (font_size_px * 0.5).max(1.0);
    let spacing = OnPathStepSpacing::for_formula(
        params.formula_layout.letter_spacing_mul,
        params.formula_layout.letter_spacing_px,
        default_advance,
    );
    let mut centers = Vec::<f32>::with_capacity(seeds.len());
    let mut total_advance = 0.0f32;
    for seed in &seeds {
        let advance = spacing.step_px(seed.advance_px);
        centers.push(total_advance + advance * 0.5);
        total_advance += advance;
    }
    let total_advance = total_advance.max(1.0);
    let glyph_count = seeds.len();
    // One continuous arc-length accumulator spans the WHOLE run here (it is not
    // reset per line), so the alignment bias is taken from the block-level
    // `params.align`; per-line inline overrides cannot be honored on this path.
    let formula_align_fraction = on_path_align_fraction(params.align, 0.5);

    let mut transforms = Vec::<FormulaGlyphTransform>::with_capacity(glyph_count);
    let mut formula_line_shifts = HashMap::<usize, f32>::new();
    for (idx, seed) in seeds.iter().enumerate() {
        let line_shift = formula_line_shifts
            .get(&seed.line_idx)
            .copied()
            .unwrap_or(0.0);
        let center_s = centers[idx] + line_shift + seed.extended_offset.line_px;
        let line_t = if seed.glyphs_in_line <= 1 {
            0.0
        } else {
            seed.glyph_idx_in_line as f32 / seed.glyphs_in_line.saturating_sub(1) as f32
        };
        let eval = FormulaEvalInput {
            t01: 0.0,
            i: idx as f32,
            n: glyph_count as f32,
            s: center_s,
            line: seed.line_idx as f32,
            line_t,
            line_n: seed.glyphs_in_line.max(1) as f32,
            width_px: width_px as f32,
            font_size_px,
            user_vars: &params.formula_layout.vars,
        };
        let arc_samples =
            build_formula_arc_length_table(&formula_program, &params.formula_layout, &eval)?;
        let curve_len_px = arc_samples
            .last()
            .map(|sample| sample.arc_len_px)
            .unwrap_or(0.0)
            .max(0.0);
        let target_arc_len_px = map_formula_target_arc_length(
            center_s,
            total_advance,
            curve_len_px,
            formula_align_fraction,
        );
        let mapped_t01 = formula_t01_for_arc_length(arc_samples.as_slice(), target_arc_len_px);
        let transform =
            formula_program.evaluate_transform_at_t01(&params.formula_layout, &eval, mapped_t01)?;
        transforms.push(FormulaGlyphTransform {
            center_x: transform.center_x,
            center_y: transform.center_y,
            rotation_rad: transform.rotation_rad + seed.extended_offset.glyph_rotation_rad,
        });
        if seed.extended_offset.shift_following && is_last_seed_in_offset_span_on_line(&seeds, seed)
        {
            *formula_line_shifts.entry(seed.line_idx).or_insert(0.0) +=
                seed.extended_offset.line_px;
        }
    }
    apply_formula_group_rotations(seeds.as_slice(), transforms.as_mut_slice());
    // Global block rotation (vector level): rotate every on-path glyph rigidly
    // about the layout centroid on top of the per-glyph tangent/static rotation,
    // so the whole formula block turns as one — matching the Ctrl+wheel overlay
    // post-rotation, only crisper. The rotated-rect bounds below grow the canvas.
    let global_rotation_rad = params.global_rotation_deg.to_radians();
    // Capture the PRE-global-rotation content box + rotation centroid for an optional
    // mesh warp, before `rotate_placements_about_centroid` mutates the transforms.
    // The centroid is the mean of ALL placement centers (this path passes every
    // transform to the rotation pass); the pre-box uses the SAME line-placed,
    // pre-rotation glyph rects the bounds loop uses, so peel/reapply is exact.
    let warp_pre = if params.raster_transform.is_some() {
        let mut pre_box = PixelBounds::empty();
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        let mut count = 0u32;
        for (seed, transform) in seeds.iter().zip(transforms.iter()) {
            let (cx, cy) = transform.placement_center();
            sum_x += cx;
            sum_y += cy;
            count += 1;
            let physical = seed.glyph.physical(
                (
                    seed.origin_x + seed.glyph_offset_px[0],
                    seed.origin_y + seed.glyph_offset_px[1],
                ),
                1.0,
            );
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
            let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
            if glyph_w <= 0 || glyph_h <= 0 {
                continue;
            }
            let src_left = physical.x + image.placement.left;
            let src_top = physical.y - image.placement.top;
            // Placement keys off the UNPADDED scaled height (matching the draw
            // pass); the included rect is widened by the faux pads.
            let (_, _, _, scaled_height) = seed.glyph_scale.scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
            );
            let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
                image.placement.top as f32,
            );
            let (placed_center_x, placed_center_y) = apply_line_placement(
                transform.center_x,
                transform.center_y,
                transform.rotation_rad,
                scaled_height,
                line_placement_frac,
            );
            include_rotated_rect_bounds(
                &mut pre_box,
                RotatedRect {
                    rect: (padded_left, padded_top, padded_width, padded_height),
                    dst_center: [placed_center_x, placed_center_y],
                    rotation_rad: transform.rotation_rad,
                },
            );
        }
        let inv = 1.0 / count.max(1) as f32;
        Some((pre_box, [sum_x * inv, sum_y * inv]))
    } else {
        None
    };
    if params.global_rotation_deg.abs() > f32::EPSILON {
        rotate_placements_about_centroid(
            transforms
                .iter_mut()
                .map(|transform| transform as &mut dyn RigidPlacement)
                .collect(),
            global_rotation_rad,
        );
    }

    let mut bounds = PixelBounds::empty();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
        let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
        if glyph_w <= 0 || glyph_h <= 0 {
            continue;
        }
        let src_left = physical.x + image.placement.left;
        let src_top = physical.y - image.placement.top;
        // Placement keys off the UNPADDED scaled height (matching the draw
        // pass); the included rect is widened by the faux pads (hard zeros
        // without faux).
        let (_, _, _, scaled_height) = seed.glyph_scale.scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
        );
        let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
            image.placement.top as f32,
        );
        // Perpendicular line placement: shift the glyph off the curve point by
        // `line_placement_frac * ink_height / 2` toward the top/bottom side. The
        // formula curve point IS the glyph ink center (0% = centered), so this is
        // the only adjustment needed on this path.
        let (placed_center_x, placed_center_y) = apply_line_placement(
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
            scaled_height,
            line_placement_frac,
        );
        include_rotated_rect_bounds(
            &mut bounds,
            RotatedRect {
                rect: (padded_left, padded_top, padded_width, padded_height),
                dst_center: [placed_center_x, placed_center_y],
                rotation_rad: transform.rotation_rad,
            },
        );
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // Optional vector mesh warp over the PRE-global-rotation content box, peeled/
    // reapplied around the SAME centroid + angle the rotation pass used. Grow the
    // canvas so a strong outward warp never clips. `None`/identity/invalid meshes or
    // a degenerate box take the byte-identical fast path.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let (pre_box, centroid) = warp_pre.as_ref()?;
        if !pre_box.initialized {
            return None;
        }
        let box_min = [pre_box.min_x as f32, pre_box.min_y as f32];
        let box_size = [
            (pre_box.max_x - pre_box.min_x) as f32,
            (pre_box.max_y - pre_box.min_y) as f32,
        ];
        MeshWarpContext::new(warp, box_min, box_size, global_rotation_rad, *centroid)
    });
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }

    let left_overhang = u32::try_from((-bounds.min_x).max(0)).unwrap_or(0);
    let right_overhang = u32::try_from((bounds.max_x - width_px as i32).max(0)).unwrap_or(0);
    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let side_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let top_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let bottom_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;

    let out_width = width_px
        .saturating_add(left_overhang)
        .saturating_add(right_overhang)
        .saturating_add(horizontal_pad * 2)
        .saturating_add(side_safety_pad * 2)
        .saturating_add(render_margin_pad * 2);
    let content_height = u32::try_from((bounds.max_y - bounds.min_y).max(1)).unwrap_or(1);
    let min_height = base_line_height_px.ceil().max(1.0) as u32;
    let out_height = content_height
        .max(min_height)
        .saturating_add(vertical_pad * 2)
        .saturating_add(top_safety_pad)
        .saturating_add(bottom_safety_pad)
        .saturating_add(render_margin_pad * 2);
    let x_offset =
        i32::try_from(left_overhang + horizontal_pad + side_safety_pad + render_margin_pad)
            .unwrap_or(i32::MAX);
    let y_offset = (-bounds.min_y).saturating_add(
        i32::try_from(vertical_pad + top_safety_pad + render_margin_pad).unwrap_or(0),
    );
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    for (seed, transform) in seeds.drain(..).zip(transforms.drain(..)) {
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = image.placement.width as usize;
        let glyph_h = image.placement.height as usize;
        if glyph_w == 0 || glyph_h == 0 {
            continue;
        }
        let placement_left = image.placement.left as f32;
        let placement_top = image.placement.top as f32;
        let src_left = (physical.x + image.placement.left) as f32;
        let src_top = (physical.y - image.placement.top) as f32;

        // Perpendicular line placement: shift the curve point (which is the glyph
        // ink center, 0% = centered) toward the top/bottom side of the line by
        // `line_placement_frac * ink_height / 2`. Shared with the bounds pass.
        let (_placed_scaled_left, _placed_scaled_top, _placed_scaled_width, placed_scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (placed_center_x, placed_center_y) = apply_line_placement(
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
            placed_scaled_height,
            line_placement_frac,
        );

        // Resolve the outline up front so the extra-info sample knows whether this
        // glyph draws warped (outline) or as an UNWARPED color-glyph bitmap fallback.
        let glyph_outline = resolve_glyph_outline(&seed, font_system, &mut outline_cache);
        // Whether this glyph puts ink on the canvas at all: it either has an
        // outline, or it is an outline-less glyph whose bitmap still gets blitted.
        // `false` only for a glyph a faux THINNING offset consumed entirely, which
        // draws nothing (the `continue` below) and therefore must not vote on the
        // extra-info ink centers either. Short-circuits, so an outline glyph pays
        // no extra lookup.
        let draws_ink = glyph_outline.is_some()
            || glyph_needs_bitmap_fallback(
                font_system,
                &mut outline_cache,
                &seed.glyph,
                seed.faux.bold,
            );

        // Extra-info sample: the final line-placed, rotated glyph box the composite
        // pass draws. Both kinds contribute, but only the outline sample is warpable;
        // the bitmap fallback's sample stays unwarped to match its unwarped pixels.
        // Leading/trailing hanging punctuation is skipped so it cannot drag the center.
        if extra_active && draws_ink && !seed.hanging_excluded {
            let (scaled_w, scaled_h) =
                seed.glyph_scale.scaled_size(glyph_w as f32, glyph_h as f32);
            let (corners, center) = rotated_box_samples(
                placed_center_x,
                placed_center_y,
                scaled_w,
                scaled_h,
                transform.rotation_rad,
            );
            extra_acc.add_glyph(corners, center, glyph_outline.is_some(), seed.line_idx);
        }

        // Prefer the true font outline; keep the bitmap blit for outline-less
        // glyphs. The (line-placed) transform center is the glyph bitmap center in
        // world space, so it is the outline destination center directly.
        if let Some(outline) = glyph_outline {
            let glyph_transform = glyph_outline_transform(
                GlyphBox {
                    placement_left,
                    placement_top,
                    glyph_w: glyph_w as f32,
                    glyph_h: glyph_h as f32,
                    scale: seed.glyph_scale,
                    subpixel: glyph_subpixel_offset(physical.cache_key),
                    shear_x: seed.faux.shear_x,
                },
                [placed_center_x, placed_center_y],
                transform.rotation_rad,
            );
            rasterize_outline_into(
                &mut raster_scratch,
                &mut RgbaCanvasView {
                    rgba: rgba.as_mut_slice(),
                    width: out_width as usize,
                    height: out_height as usize,
                },
                [-(x_offset as f32), -(y_offset as f32)],
                &outline,
                &glyph_transform,
                OutlinePaint {
                    color: seed.text_color,
                    aa_lut: &aa_lut,
                    warp: warp_ctx.as_ref(),
                },
            );
            continue;
        }

        // A glyph whose outline was CONSUMED by a faux thinning offset draws
        // nothing: blitting its bitmap would restore it at FULL weight.
        if !draws_ink {
            continue;
        }

        // Fallback: the original rotated bitmap blit for any outline-less glyph
        // (real color glyph or a monochrome embedded-bitmap glyph). The subpixel
        // fraction is already baked into the bitmap coverage.
        let src_center_x = src_left + glyph_w as f32 * 0.5;
        let src_center_y = src_top + glyph_h as f32 * 0.5;
        let cos_a = transform.rotation_rad.cos();
        let sin_a = transform.rotation_rad.sin();
        let glyph_rgba = build_glyph_rgba_buffer(
            &image.content,
            image.data.as_slice(),
            glyph_w,
            glyph_h,
            seed.text_color,
        );
        let (scaled_left, scaled_top, scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(RotatedRect {
            rect: (scaled_left, scaled_top, scaled_width, scaled_height),
            dst_center: [placed_center_x, placed_center_y],
            rotation_rad: transform.rotation_rad,
        });
        let dst_min_x = ((min_x + x_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_x = ((max_x + x_offset as f32).ceil() as i32 + 1).min(out_width as i32);
        let dst_min_y = ((min_y + y_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_y = ((max_y + y_offset as f32).ceil() as i32 + 1).min(out_height as i32);
        for dst_y in dst_min_y..dst_max_y {
            for dst_x in dst_min_x..dst_max_x {
                let world_x = dst_x as f32 + 0.5 - x_offset as f32;
                let world_y = dst_y as f32 + 0.5 - y_offset as f32;
                let rel_x = world_x - placed_center_x;
                let rel_y = world_y - placed_center_y;
                let rotated_x = rel_x * cos_a + rel_y * sin_a;
                let rotated_y = -rel_x * sin_a + rel_y * cos_a;
                let src_x = src_center_x + rotated_x / seed.glyph_scale.width_mul;
                let src_y = src_center_y + rotated_y / seed.glyph_scale.height_mul;
                let local_x = src_x - src_left - 0.5;
                let local_y = src_y - src_top - 0.5;
                let (src_r, src_g, src_b, src_a) =
                    bilinear_sample_rgba(glyph_rgba.as_slice(), glyph_w, glyph_h, local_x, local_y);
                if src_a == 0 {
                    continue;
                }
                let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
                blend_pixel_over(&mut rgba[dst_idx..dst_idx + 4], src_r, src_g, src_b, src_a);
            }
        }
    }

    // Extra-info centers: warp the raw content-space samples through the same mesh
    // context the composite pass used, then map to canvas pixels via the pass
    // offset. Runs BEFORE the caller's trim/effects; both seams self-correct.
    if let Some(ctx) = warp_ctx.as_ref() {
        extra_acc.map_points(|point| ctx.warp_world(point));
    }
    let extra = extra_acc.finish(x_offset as f32, y_offset as f32);

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        // Filled in by `pipeline::render_text_to_image`, which owns the shaped
        // buffer this layout drew from.
        font_fallbacks: crate::types::FontFallbackReport::default(),
    })
}

/// Why the `Shape` layout cannot draw this text on its formula curve (too short a
/// curve, too strong a compression), or `None` when it can.
pub(super) fn detect_shape_layout_fallback_reason(
    shaping: RunShaping<'_>,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    lines: FormulaLines<'_>,
) -> Result<Option<String>, String> {
    let RunPen {
        params,
        inline_style_spans,
        font_size_px,
        ..
    } = shaping.pen;
    let base_line_height_px = lines.base_line_height_px;
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = lines.baselines(buffer, has_inline_size_overrides);
    // This probe only measures shaped advances, but the seeds it builds carry a
    // resolved faux style, so it needs an outline cache of its own. It stays EMPTY
    // unless faux bold in the counter-preserving mode is active (the only case
    // that looks a glyph up), so the no-faux probe costs exactly nothing.
    let mut outline_cache = OutlineCache::new();
    let seeds = collect_formula_glyph_seeds(
        shaping,
        font_system,
        &mut outline_cache,
        buffer,
        line_baselines.as_slice(),
        base_line_height_px,
    );
    if seeds.len() <= 1 {
        return Ok(None);
    }

    let formula_program = FormulaProgramBundle::compile(&params.formula_layout)?;
    let width_px = params.width_px.max(1) as f32;
    let mut lines = HashMap::<usize, (f32, usize)>::new();
    let default_advance = (font_size_px * 0.5).max(1.0);
    for seed in &seeds {
        let entry = lines.entry(seed.line_idx).or_insert((0.0, 0));
        entry.0 += seed.glyph.w.max(default_advance).max(1.0);
        entry.1 += 1;
    }

    let mut reasons = Vec::<String>::new();
    for (line_idx, (text_len_px, glyph_count)) in lines {
        if glyph_count <= 1 {
            continue;
        }
        let eval = FormulaEvalInput {
            t01: 0.0,
            i: 0.0,
            n: glyph_count as f32,
            s: 0.0,
            line: line_idx as f32,
            line_t: 0.0,
            line_n: glyph_count as f32,
            width_px,
            font_size_px,
            user_vars: &params.formula_layout.vars,
        };
        let curve_len_px =
            build_formula_arc_length_table(&formula_program, &params.formula_layout, &eval)?
                .last()
                .map(|sample| sample.arc_len_px)
                .unwrap_or(0.0)
                .max(0.0);
        let compression_ratio = curve_len_px / text_len_px.max(1.0);
        if curve_len_px < font_size_px * 1.5 || compression_ratio < 0.38 {
            reasons.push(format!(
                "строка {}: длина формы {:.0}px для текста {:.0}px",
                line_idx + 1,
                curve_len_px,
                text_len_px
            ));
        }
    }

    if reasons.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Форма слишком узкая для текущего текста, выполнен обычный рендер ({})",
            reasons.join(", ")
        )))
    }
}

/// Map a glyph's run-local arc length onto its formula curve.
///
/// When the run FITS the curve, the leftover curve length is split by
/// `align_fraction` (`0.0` = run pinned to the curve start, `0.5` = centered,
/// `1.0` = pinned to the curve end); see [`on_path_align_fraction`], which the
/// caller uses with `0.5` as the justify default so justified formula overlays
/// keep their historical centering.
///
/// When the run OVERFLOWS the curve it is compressed onto the curve
/// (`center_s * curve_len / text_len`) and `align_fraction` is deliberately
/// ignored: there is no free space left to distribute, and `Shape`'s fallback
/// threshold (`detect_shape_layout_fallback_reason`) is defined against exactly
/// this compression ratio.
pub(super) fn map_formula_target_arc_length(
    center_s_px: f32,
    text_len_px: f32,
    curve_len_px: f32,
    align_fraction: f32,
) -> f32 {
    if curve_len_px <= 0.0 {
        return 0.0;
    }
    let text_len_px = text_len_px.max(1.0);
    if text_len_px <= curve_len_px {
        let leading_gap = (curve_len_px - text_len_px) * align_fraction;
        (leading_gap + center_s_px).clamp(0.0, curve_len_px)
    } else {
        (center_s_px * (curve_len_px / text_len_px)).clamp(0.0, curve_len_px)
    }
}

fn formula_t01_for_arc_length(samples: &[FormulaArcLengthSample], target_arc_len_px: f32) -> f32 {
    let Some(last) = samples.last().copied() else {
        return 0.0;
    };
    if target_arc_len_px <= 0.0 {
        return samples.first().map(|sample| sample.t01).unwrap_or(0.0);
    }
    if target_arc_len_px >= last.arc_len_px {
        return last.t01;
    }

    let idx = samples.partition_point(|sample| sample.arc_len_px < target_arc_len_px);
    if idx == 0 {
        return samples[0].t01;
    }
    let prev = samples[idx - 1];
    let next = samples[idx];
    let span = (next.arc_len_px - prev.arc_len_px).abs();
    if span <= 1e-6 {
        return next.t01;
    }
    let local_t = ((target_arc_len_px - prev.arc_len_px) / span).clamp(0.0, 1.0);
    prev.t01 + (next.t01 - prev.t01) * local_t
}

fn build_formula_arc_length_table(
    program: &FormulaProgramBundle,
    layout: &crate::types::TextFormulaLayoutParams,
    input: &FormulaEvalInput<'_>,
) -> Result<Vec<FormulaArcLengthSample>, String> {
    let samples = program.build_arc_length_table(layout, input)?;
    Ok(samples
        .into_iter()
        .map(|sample| FormulaArcLengthSample {
            t01: sample.t01,
            arc_len_px: sample.arc_len_px,
        })
        .collect())
}
