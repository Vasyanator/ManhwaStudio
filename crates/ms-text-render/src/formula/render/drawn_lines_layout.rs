/*
File: crates/ms-text-render/src/formula/render/drawn_lines_layout.rs

Purpose:
The custom raster / vector line layout (`CustomRasterLines` / `CustomVectorLines`):
one render pass over the drawn line paths, plus the on-path alignment pre-pass that
gives each line its own start cursor.

Key functions:
- render_text_with_drawn_lines_layout_once(): seeds -> per-line transforms -> composite
- on_path_align_fraction(), drawn_line_start_offset(s)(), drawn_line_aligns()
- drawn_line_center_s() / drawn_line_next_cursor(), `LineArcCursor` (impl)

Notes:
The step between glyphs comes only from `OnPathStepSpacing::step_px` (parent); see
the parent header for the alignment and drop-side contract.
*/

use crate::drawn_lines::DrawnLinePath;
use crate::extra_info::{ExtraInfoAccumulator, rotated_box_samples};
use crate::glyph_blit::{
    GlyphBox, glyph_needs_bitmap_fallback, glyph_outline_transform, glyph_subpixel_offset,
};
use crate::vector::{
    MeshWarpContext, OutlinePaint, RasterScratch, build_aa_lut, rasterize_outline_into,
};
use crate::pair_advance::effective_spacing_percent;
use crate::pipeline::{
    InlineHeightRoom, RunPen, RunShaping, compute_layout_line_offsets, line_baseline_advance_table,
    spans_have_inline_size_overrides,
};
use crate::raster::{
    PixelBounds, RgbaCanvasView, RigidPlacement, RotatedRect, bilinear_sample_rgba,
    blend_pixel_over, build_glyph_rgba_buffer, include_rotated_rect_bounds,
    rotate_placements_about_centroid, rotated_rect_world_bounds,
};
use crate::types::{HorizontalAlign, LinePlacementReference, RenderedTextImage, TextLayoutMode};
use std::collections::HashMap;
use super::{
    FormulaGlyphSeed, FormulaLines, FormulaRenderRequest, LineArcCursor, OnPathInkCaches,
    OnPathStepSpacing, drawn_line_glyph_destination_center_raw, resolve_glyph_outline,
};
use super::line_transforms::build_drawn_line_transforms;
use super::path_geometry::{custom_line_layout_settings, is_last_seed_in_offset_span_on_line};
use super::seeds::collect_formula_glyph_seeds;

pub(super) fn render_text_with_drawn_lines_layout_once(
    request: FormulaRenderRequest<'_, '_>,
    paths: &[Option<DrawnLinePath>],
    fixed_output_size: Option<(u32, u32)>,
) -> Result<RenderedTextImage, String> {
    let FormulaRenderRequest {
        params,
        font_system,
        buffer,
        attrs,
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        custom_kerning,
        layout_text,
        font_size_px,
        base_line_height_px,
        line_placement_frac,
    } = request;
    let layout_line_offsets = compute_layout_line_offsets(layout_text);
    let line_spacing_percent =
        effective_spacing_percent(params.line_spacing_percent, params.glyph_height_percent);
    let default_extra_line_spacing_px =
        params.line_spacing_px + font_size_px * (line_spacing_percent / 100.0);
    // Lines stack on BASELINES here too, so the table is the grow-only
    // baseline-advance one: an inline `<stretching>` height span may push the
    // next line further away, never pull it closer (see `pipeline/`).
    let line_extra_spacing_table = line_baseline_advance_table(
        params,
        layout_text,
        layout_line_offsets.as_slice(),
        inline_style_spans,
        font_size_px,
        default_extra_line_spacing_px,
        &InlineHeightRoom::measure(
            params,
            buffer,
            font_system,
            layout_line_offsets.as_slice(),
            inline_style_spans,
        ),
    );
    let shaping = RunShaping {
        pen: RunPen {
            params,
            inline_style_spans,
            layout_line_offsets: layout_line_offsets.as_slice(),
            custom_kerning,
            font_size_px,
        },
        attrs,
        faux_face_baseline,
        inline_font_registry,
        hyphen_line_height_px: base_line_height_px,
    };
    let lines = FormulaLines {
        base_line_height_px,
        extra_spacing_table: line_extra_spacing_table.as_slice(),
        default_extra_spacing_px: default_extra_line_spacing_px,
    };
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = lines.baselines(buffer, has_inline_size_overrides);
    // Resolution-independent glyph outlines: shared by the faux counter-flag
    // lookup during seed collection, the ink-distance search (via the transforms
    // below) and the composite pass, so each glyph is extracted at most once per
    // render.
    let mut caches = OnPathInkCaches::new();
    let mut seeds = collect_formula_glyph_seeds(
        shaping,
        font_system,
        &mut caches.outlines,
        buffer,
        line_baselines.as_slice(),
        base_line_height_px,
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            params.width_px.max(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // `caches` lives for the whole render, so glyph rasterization and ink-contour
    // tracing happen at most once per distinct glyph and are reused by the bounds
    // and composite passes.
    // Reused per-glyph rasterizer buffers for the composite pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info (mean/median centers). A FRESH accumulator per `_once`
    // call (this function is re-run from scratch by the retry loop), so the stored
    // extras always match the accepted image. Inactive by default -> a true no-op.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    // Shared band for the LineBox line-placement reference: the primary font's ascent
    // (baseline..ascender top) at the line size, in the SAME scaled px space as each
    // glyph's `scaled_height` (so it also carries the base vertical stretch). Computed
    // once per render from the first seed's font; ignored under GlyphHeight. Gate the
    // reference to CustomVectorLines so raster/drawn lines keep the legacy anchoring.
    let base_height_mul = (params.glyph_height_percent / 100.0).clamp(0.01, 3.0);
    let ascent_scaled = seeds
        .first()
        .and_then(|seed| font_system.get_font(seed.glyph.font_id))
        .map(|font| font.as_swash().metrics(&[]).scale(font_size_px).ascent)
        .unwrap_or(font_size_px)
        * base_height_mul;
    let line_placement_reference = match params.text_layout_mode {
        TextLayoutMode::CustomVectorLines => params.line_placement_reference,
        TextLayoutMode::Normal
        | TextLayoutMode::Formula
        | TextLayoutMode::Shape
        | TextLayoutMode::CustomRasterLines => LinePlacementReference::GlyphHeight,
    };
    let mut transforms = build_drawn_line_transforms(
        params,
        seeds.as_slice(),
        paths,
        custom_kerning,
        font_system,
        &mut caches,
        custom_line_layout_settings(params, line_placement_frac, ascent_scaled),
    );
    let skipped = transforms.iter().filter(|item| item.is_none()).count();
    // Global block rotation (vector level): rotate every placed line-glyph rigidly
    // about the layout centroid before bounds/draw, so the whole custom-line block
    // turns as one — matching the Ctrl+wheel overlay post-rotation, only crisper.
    let global_rotation_rad = params.global_rotation_deg.to_radians();
    // Capture the PRE-global-rotation content box + rotation centroid for an optional
    // mesh warp, BEFORE `rotate_placements_about_centroid` mutates the transforms in
    // place. The centroid is the mean of the drawable (Some) placement centers —
    // exactly the pivot the rotation pass computes — so the warp peels/reapplies the
    // same global rotation. Gated on `raster_transform` so the fast path is untouched.
    let warp_pre = if params.raster_transform.is_some() {
        let mut pre_box = PixelBounds::empty();
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        let mut count = 0u32;
        for (seed, transform) in seeds.iter().zip(transforms.iter()) {
            let Some(transform) = transform else {
                continue;
            };
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
            let Some(image) = caches.swash.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
            let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
            if glyph_w <= 0 || glyph_h <= 0 {
                continue;
            }
            let src_left = physical.x + image.placement.left;
            let src_top = physical.y - image.placement.top;
            // Placement (line centering) keys off the UNPADDED scaled height —
            // exactly what the draw pass uses; only the included rect is
            // widened by the faux pads (hard zeros without faux).
            let (_, _, _, scaled_height) = seed
                .glyph_scale
                .scaled_rect(src_left as f32, src_top as f32, glyph_w as f32, glyph_h as f32);
            let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
                image.placement.top as f32,
            );
            let placement_top_scaled = image.placement.top as f32 * seed.glyph_scale.height_mul;
            let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
                transform,
                scaled_height,
                placement_top_scaled,
                line_placement_frac,
                line_placement_reference,
                ascent_scaled,
            );
            include_rotated_rect_bounds(
                &mut pre_box,
                RotatedRect {
                    rect: (padded_left, padded_top, padded_width, padded_height),
                    dst_center: [dst_center_x, dst_center_y],
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
                .filter_map(Option::as_mut)
                .map(|transform| transform as &mut dyn RigidPlacement)
                .collect(),
            global_rotation_rad,
        );
    }
    let mut bounds = PixelBounds::empty();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        let Some(transform) = transform else {
            continue;
        };
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = caches.swash.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
        let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
        if glyph_w <= 0 || glyph_h <= 0 {
            continue;
        }
        let src_left = physical.x + image.placement.left;
        let src_top = physical.y - image.placement.top;
        // Same split as the warp pre-box: unpadded height for the placement,
        // faux-padded rect for the bounds so offset/sheared ink never clips.
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
        let placement_top_scaled = image.placement.top as f32 * seed.glyph_scale.height_mul;
        let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
            transform,
            scaled_height,
            placement_top_scaled,
            line_placement_frac,
            line_placement_reference,
            ascent_scaled,
        );
        include_rotated_rect_bounds(
            &mut bounds,
            RotatedRect {
                rect: (padded_left, padded_top, padded_width, padded_height),
                dst_center: [dst_center_x, dst_center_y],
                rotation_rad: transform.rotation_rad,
            },
        );
    }

    let mut warnings = Vec::new();
    if skipped > 0 {
        warnings.push(format!(
            "Рисованные линии: не отрисовано символов без подходящей точки линии: {skipped}."
        ));
    }
    if !bounds.initialized {
        if let Some((width, height)) = fixed_output_size {
            return Ok(RenderedTextImage {
                width,
                height,
                rgba: RenderedTextImage::transparent(width, height).rgba,
                warnings,
                content_origin_x: 0,
                content_origin_y: 0,
                extra: crate::types::RenderedTextExtraInfo::default(),
                font_fallbacks: crate::types::FontFallbackReport::default(),
            });
        }
        return Ok(RenderedTextImage {
            width: params.width_px.max(1),
            height: base_line_height_px.ceil().max(1.0) as u32,
            rgba: RenderedTextImage::transparent(
                params.width_px.max(1),
                base_line_height_px.ceil().max(1.0) as u32,
            )
            .rgba,
            warnings,
            content_origin_x: 0,
            content_origin_y: 0,
            extra: crate::types::RenderedTextExtraInfo::default(),
            font_fallbacks: crate::types::FontFallbackReport::default(),
        });
    }

    let pad = font_size_px.ceil().max(2.0) as u32;
    // Optional vector mesh warp. The normalization frame is the fixed vector-lines
    // canvas when it is in play (no global rotation) — matching the authoring UI,
    // which normalizes handles against that canvas — otherwise the pre-rotation
    // content bounds. Design B may still override the box SIZE with the mesh's
    // stored source dims inside `MeshWarpContext::new`. Peel/reapply use the SAME
    // centroid + angle the rotation pass used. `None`/identity/invalid => fast path.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let (pre_box, centroid) = warp_pre.as_ref()?;
        let (box_min, box_size) = if let Some((width, height)) =
            fixed_output_size.filter(|_| params.global_rotation_deg.abs() <= f32::EPSILON)
        {
            ([0.0, 0.0], [width as f32, height as f32])
        } else if pre_box.initialized {
            (
                [pre_box.min_x as f32, pre_box.min_y as f32],
                [
                    (pre_box.max_x - pre_box.min_x) as f32,
                    (pre_box.max_y - pre_box.min_y) as f32,
                ],
            )
        } else {
            return None;
        };
        MeshWarpContext::new(warp, box_min, box_size, global_rotation_rad, *centroid)
    });
    // A fixed canvas (vector-lines) is honored only when there is no global
    // rotation AND no active warp; once the block is rotated OR warped the canvas
    // must grow to the transformed bounds (like the Ctrl+wheel overlay) so no corner
    // is clipped.
    let honor_fixed_size = fixed_output_size
        .filter(|_| params.global_rotation_deg.abs() <= f32::EPSILON && warp_ctx.is_none());
    // Grow the content bounds to the warped+rotated lattice extent so a strong
    // outward warp never clips (no-op for `None`/identity; only reached when the
    // fixed canvas is not honored, i.e. exactly when the warp is active).
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }
    let (out_width, out_height, x_offset, y_offset) =
        if let Some((width, height)) = honor_fixed_size {
            (width.max(1), height.max(1), 0, 0)
        } else {
            (
                u32::try_from((bounds.max_x - bounds.min_x).max(1))
                    .unwrap_or(1)
                    .saturating_add(pad * 2),
                u32::try_from((bounds.max_y - bounds.min_y).max(1))
                    .unwrap_or(1)
                    .saturating_add(pad * 2),
                -bounds.min_x + i32::try_from(pad).unwrap_or(0),
                -bounds.min_y + i32::try_from(pad).unwrap_or(0),
            )
        };
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    for (seed, transform) in seeds.drain(..).zip(transforms) {
        let Some(transform) = transform else {
            continue;
        };
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = caches.swash.get_image(font_system, physical.cache_key) else {
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
        let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let placement_top_scaled = placement_top * seed.glyph_scale.height_mul;
        let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
            &transform,
            scaled_height,
            placement_top_scaled,
            line_placement_frac,
            line_placement_reference,
            ascent_scaled,
        );

        // Resolve the outline up front so the extra-info sample knows whether this
        // glyph draws warped (outline) or as an UNWARPED color-glyph bitmap fallback.
        let glyph_outline = resolve_glyph_outline(&seed, font_system, &mut caches.outlines);
        // Whether this glyph puts ink on the canvas at all: it either has an
        // outline, or it is an outline-less glyph whose bitmap still gets blitted.
        // `false` only for a glyph a faux THINNING offset consumed entirely, which
        // draws nothing (the `continue` below) and therefore must not vote on the
        // extra-info ink centers either. Short-circuits, so an outline glyph pays
        // no extra lookup.
        let draws_ink = glyph_outline.is_some()
            || glyph_needs_bitmap_fallback(
                font_system,
                &mut caches.outlines,
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
                dst_center_x,
                dst_center_y,
                scaled_w,
                scaled_h,
                transform.rotation_rad,
            );
            extra_acc.add_glyph(corners, center, glyph_outline.is_some(), seed.line_idx);
        }

        // Prefer the true font outline: rasterize it directly into the output at
        // the exact world placement the bitmap blit would have used. Color/emoji
        // glyphs have no monochrome outline and keep the bitmap blit below.
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
                [dst_center_x, dst_center_y],
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
        // (real color glyph or a monochrome embedded-bitmap glyph). This path draws
        // it regardless of color; the subpixel fraction is already in the bitmap.
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
            dst_center: [dst_center_x, dst_center_y],
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
                let rel_x = world_x - dst_center_x;
                let rel_y = world_y - dst_center_y;
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
        warnings,
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        // Filled in by `pipeline::render_text_to_image`, which owns the shaped
        // buffer this layout drew from.
        font_fallbacks: crate::types::FontFallbackReport::default(),
    })
}

/// Fraction of a path's free arc length that is placed BEFORE the run.
///
/// Mirrors [`HorizontalAlign::offset_fraction`]: `0.0` pins the run to the start
/// of the path, `0.5` centers it, `1.0` pins it to the end. Justified alignment
/// stretches lines to the block width and hides the alignment slider in the UI,
/// so `bias` must never be read there: justify returns `justify_fraction`, which
/// each on-path layout supplies as its own historical default (custom lines
/// `0.0` = start of line, formula `0.5` = centered) so justified overlays keep
/// rendering exactly where they did before alignment was honored on path.
pub(super) fn on_path_align_fraction(align: HorizontalAlign, justify_fraction: f32) -> f32 {
    if align.justify {
        return justify_fraction;
    }
    align.offset_fraction()
}

/// Arc-length center of the glyph sitting at line cursor `cursor_px`.
///
/// `line_offset_px` is the glyph's inline `<offset>` shift along the line
/// (`InlineGlyphOffset::line_px`); it moves the glyph only, and
/// [`drawn_line_next_cursor`] takes it back out. Single source of truth for both
/// the real placement walk and the alignment pre-pass, so the two can never
/// drift apart.
pub(super) fn drawn_line_center_s(cursor_px: f32, advance_px: f32, line_offset_px: f32) -> f32 {
    cursor_px + advance_px * 0.5 + line_offset_px
}

/// The line cursor left behind by a glyph centered at `center_s_px`.
///
/// Inverse companion of [`drawn_line_center_s`]: it removes `line_offset_px`
/// again so an inline offset shifts its own glyph without dragging the rest of
/// the line (that is what `InlineGlyphOffset::shift_following` is for, applied
/// separately by the callers).
pub(super) fn drawn_line_next_cursor(center_s_px: f32, advance_px: f32, line_offset_px: f32) -> f32 {
    center_s_px + advance_px * 0.5 - line_offset_px
}

impl LineArcCursor {
    /// A cursor starting at a line's alignment offset (see
    /// [`drawn_line_start_offset`]).
    #[must_use]
    pub(super) fn starting_at(start_offset_s_px: f32) -> Self {
        Self {
            s_px: start_offset_s_px,
        }
    }

    /// Arc-length SEED center of the glyph sitting at the cursor, i.e. where the
    /// plain `ByLineLength` walk puts it.
    ///
    /// Pure: the cursor only moves on [`Self::commit`], so a caller may correct
    /// the returned center before committing it.
    #[must_use]
    pub(super) fn seed_center_s(self, seed: &FormulaGlyphSeed, advance_px: f32) -> f32 {
        drawn_line_center_s(self.s_px, advance_px, seed.extended_offset.line_px)
    }

    /// Walk the cursor past a glyph whose FINAL arc-length center is
    /// `final_center_s`.
    ///
    /// `all_seeds` is the whole seed list, needed only to decide whether this
    /// seed closes an inline `<offset>` span that shifts everything after it
    /// ([`is_last_seed_in_offset_span_on_line`]).
    pub(super) fn commit(
        &mut self,
        all_seeds: &[FormulaGlyphSeed],
        seed: &FormulaGlyphSeed,
        advance_px: f32,
        final_center_s: f32,
    ) {
        let line_offset_px = seed.extended_offset.line_px;
        self.s_px = drawn_line_next_cursor(final_center_s, advance_px, line_offset_px);
        if seed.extended_offset.shift_following
            && is_last_seed_in_offset_span_on_line(all_seeds, seed)
        {
            self.s_px += line_offset_px;
        }
    }
}

/// The alignment that governs a whole line: the [`FormulaGlyphSeed::line_align`]
/// of its FIRST seed, so an inline `<align=...>` opened later in the line cannot
/// re-align the part already laid out.
///
/// Single owner of that rule, shared by the alignment pre-pass
/// ([`drawn_line_start_offsets`]) and by the `MinimumPreviousDistance` start
/// refinement ([`refine_min_distance_start_offsets`]), which must agree on which
/// alignment a line has.
#[must_use]
pub(super) fn drawn_line_aligns(seeds: &[FormulaGlyphSeed]) -> HashMap<usize, HorizontalAlign> {
    let mut aligns = HashMap::<usize, HorizontalAlign>::new();
    for seed in seeds {
        aligns.entry(seed.line_idx).or_insert(seed.line_align);
    }
    aligns
}

/// Arc-length position where a line's run must start to satisfy `align`.
///
/// Returns `(total_len_px - run_len_px) * fraction`, with `fraction` from
/// [`on_path_align_fraction`] using `0.0` (start of line) as the justify
/// default. The free space is deliberately NOT clamped to `>= 0`: a run longer
/// than its line yields a NEGATIVE start, which is exactly what moves the
/// clipping from the end of the line to its start.
pub(super) fn drawn_line_start_offset(total_len_px: f32, run_len_px: f32, align: HorizontalAlign) -> f32 {
    (total_len_px - run_len_px) * on_path_align_fraction(align, 0.0)
}

/// Per-line arc-length start offsets that realise the alignment bias on a
/// custom-line layout.
///
/// Each line uses its own [`FormulaGlyphSeed::line_align`] (so inline
/// `<align=...>` overrides keep working) fed to [`drawn_line_start_offset`].
/// Lines with no path are absent from the map and start at `0.0`.
///
/// `spacing` MUST be the very same [`OnPathStepSpacing`] the real walk in
/// [`drawn_line_seed_transform`] uses: this pre-pass replays that walk, so a
/// different step here would misplace every line's start.
///
/// A line's `run_len_px` is its FINAL CURSOR, i.e. the `ByLineLength` walk of
/// [`drawn_line_seed_transform`] replayed through the shared
/// [`drawn_line_center_s`] / [`drawn_line_next_cursor`] pair: the sum of the
/// effective advances plus the `shift_following` bumps, which genuinely do move
/// every following glyph. A plain (non-`shift_following`) inline
/// `<offset>`/`line_px` deliberately does NOT count, matching the contract
/// [`drawn_line_next_cursor`] documents: such an offset moves its own glyph
/// only. Consequence, by design: a glyph nudged forward past the line end by its
/// own inline offset is invisible to the alignment and can still be clipped, and
/// one nudged glyph never drags the whole line's alignment. The result is
/// floored at `0.0` so a line whose `shift_following` bumps run backwards past
/// the origin cannot produce a negative length.
///
/// Accuracy per spacing mode:
/// - `ByLineLength`: EXACT — that is the recurrence being replayed, and it is
///   the final answer.
/// - `MinimumPreviousDistance`: a STARTING ESTIMATE only. Each glyph's ink
///   correction shifts every following one, so the real run length drifts from
///   the replayed one; on a path that bends the same way throughout the
///   corrections do not cancel and the run ends up genuinely longer. The result
///   here is therefore just the first iterate of
///   [`refine_min_distance_start_offsets`], which re-walks the line and replaces
///   it with an offset derived from the MEASURED length. Do not read an
///   end-aligned `MinimumPreviousDistance` line's start offset off this function.
pub(super) fn drawn_line_start_offsets(
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
    spacing: OnPathStepSpacing,
) -> HashMap<usize, f32> {
    let aligns = drawn_line_aligns(seeds);
    // Per line: the running cursor of the uncorrected replay.
    let mut lines = HashMap::<usize, LineArcCursor>::new();
    for seed in seeds {
        let cursor = lines.entry(seed.line_idx).or_default();
        let advance = spacing.step_px(seed.advance_px);
        // Nothing to correct in this replay, so the seed center IS the final one.
        let center_s = cursor.seed_center_s(seed, advance);
        cursor.commit(seeds, seed, advance, center_s);
    }

    lines
        .into_iter()
        .filter_map(|(line_idx, cursor)| {
            let path = paths.get(line_idx).and_then(Option::as_ref)?;
            let align = aligns.get(&line_idx).copied()?;
            Some((
                line_idx,
                drawn_line_start_offset(path.total_len_px, cursor.s_px.max(0.0), align),
            ))
        })
        .collect()
}
