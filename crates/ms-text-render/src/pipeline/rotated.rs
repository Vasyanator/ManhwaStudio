/*
File: crates/ms-text-render/src/pipeline/rotated.rs

Purpose:
The inline-rotated horizontal path: per-glyph `RotatedGlyphPlacement` collection,
group and global rotations about their centroids, the rotated bounds and the
inverse-sampled draw (`render_horizontal_rotated`).

Key functions:
- build_rotated_placement(), apply_rotated_group_rotations(), apply_global_rotation()
- render_horizontal_rotated(): the whole rotated render of one buffer
- rotated_placement_extra_samples(): the extra-info samples of a rotated glyph

Notes:
`RotatedGlyphPlacement` / `InlineGlyphRotation` are declared in `pipeline/mod.rs`;
the `RigidPlacement` impl that the shared centroid rotation needs lives here.
*/

use crate::extra_info::ExtraInfoAccumulator;
use crate::inline_styles::InlineGlyphOffset;
use crate::glyph_blit::{
    GlyphBox, glyph_needs_bitmap_fallback, glyph_outline_transform, glyph_subpixel_offset,
    resolve_outline_for_glyph,
};
use crate::raster::{
    PixelBounds, RgbaCanvasView, RgbaView, RigidPlacement, RotatedGlyphPose, RotatedRect,
    build_glyph_rgba_buffer, draw_rotated_scaled_glyph_rgba, include_rotated_rect_bounds,
    is_cancelled, rotate_placements_about_centroid,
};
use crate::vector::{
    MeshWarpContext, OutlinePaint, RasterScratch, build_aa_lut, rasterize_outline_into,
};
use crate::types::{FontFallbackReport, RenderedTextImage};
use cosmic_text::{Buffer, FontSystem, LayoutGlyph};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use super::{
    FauxGlyphStyle, GlyphCaches, GlyphScaleSettings, GlyphStyle, InlineGlyphRotation,
    RotatedGlyphPlacement, RunPen, RunShaping,
};
use super::run_layout::horizontal_run_layout;
use super::text_prep::{compute_inline_line_aligns, horizontal_line_offset};
use super::inline_lookup::{
    inline_glyph_offset_span_at_offset, inline_glyph_offset_span_for_glyph,
    inline_glyph_offset_style_at_offset, inline_glyph_offset_style_for_glyph,
    inline_glyph_scale_at_offset, inline_glyph_scale_for_glyph, inline_text_color_at_offset,
    inline_text_color_for_glyph,
};
use super::faux::{
    faux_bounds_pads, faux_style_at_offset, faux_style_for_glyph, resolve_faux_counter_flag,
};
use super::hyphen::{
    build_wrapped_hyphen_glyph, run_wraps_at_soft_hyphen, soft_hyphen_style_offset,
    trailing_hyphen_x,
};
use super::hanging::{hanging_edge_run_bounds, is_edge_run_hanging};

impl RotatedGlyphPlacement {
    /// Whether this placement puts any ink on the canvas.
    ///
    /// `false` only for a glyph whose outline was CONSUMED by a faux thinning
    /// offset: it has no outline AND deliberately no fallback bitmap, yet keeps
    /// a non-zero box. Such a glyph must not vote on the extra-info ink centers
    /// (`ExtraInfoAccumulator`), which report where the ink IS.
    #[must_use]
    fn draws_ink(&self) -> bool {
        self.outline.is_some() || !self.glyph_rgba.is_empty()
    }

    /// The scaled glyph rect widened by the faux bounds pads (exactly the
    /// plain scaled rect when faux is off — the pads are hard zeros).
    ///
    /// Box-centre anchored on purpose: every consumer feeds this rect to
    /// `include_rotated_rect_bounds`, which re-pins it to `center_x`/`center_y`
    /// (already baseline-anchored and rotated), so only its SIZE and its
    /// centre-relative extents are read. The pads are symmetric and therefore do
    /// not move the centre either.
    fn padded_scaled_rect(&self) -> (f32, f32, f32, f32) {
        self.scale.scaled_rect(
            self.src_left - self.bounds_pad[0],
            self.src_top - self.bounds_pad[1],
            self.glyph_w as f32 + 2.0 * self.bounds_pad[0],
            self.glyph_h as f32 + 2.0 * self.bounds_pad[1],
        )
    }
}

/// Collect one glyph placement for the rotated horizontal path: the bitmap
/// placement box (bounds and pivot source), the fill outline or the RGBA
/// fallback, the baseline-anchored rotation centre and the inline rotation.
/// `None` for glyphs `get_image` cannot produce and zero-size (space) glyphs.
fn build_rotated_placement(
    font_system: &mut FontSystem,
    caches: &mut GlyphCaches,
    glyph: &LayoutGlyph,
    pos: [f32; 2],
    style: GlyphStyle,
    text_color: [u8; 4],
    rotation: InlineGlyphRotation,
) -> Option<RotatedGlyphPlacement> {
    let [pos_x, pos_y] = pos;
    let GlyphStyle { scale, faux } = style;
    let physical = glyph.physical((pos_x, pos_y), 1.0);
    let Some(image) = caches.swash.get_image(font_system, physical.cache_key) else {
        return None;
    };
    let glyph_w = image.placement.width as usize;
    let glyph_h = image.placement.height as usize;
    if glyph_w == 0 || glyph_h == 0 {
        return None;
    }
    let placement_left = image.placement.left as f32;
    let placement_top = image.placement.top as f32;
    let src_left = (physical.x + image.placement.left) as f32;
    let src_top = (physical.y - image.placement.top) as f32;
    let subpixel = glyph_subpixel_offset(physical.cache_key);
    let outline = resolve_outline_for_glyph(font_system, &mut caches.outlines, glyph, faux.bold);
    // Build the bitmap RGBA for any outline-less glyph fallback: real color glyphs
    // and monochrome embedded-bitmap / sbix / CBDT-mono glyphs alike (the zero-size
    // skip above already filtered spaces). A glyph whose outline was CONSUMED by a
    // faux thinning offset gets no bitmap — it must draw nothing.
    let glyph_rgba = if outline.is_none()
        && glyph_needs_bitmap_fallback(font_system, &mut caches.outlines, glyph, faux.bold)
    {
        build_glyph_rgba_buffer(&image.content, image.data.as_slice(), glyph_w, glyph_h, text_color)
    } else {
        Vec::new()
    };
    let bounds_pad = faux_bounds_pads(
        faux,
        placement_top,
        glyph_h as f32,
        scale.width_mul,
        scale.height_mul,
    );
    // Height scale anchored at the pen baseline (see
    // `GlyphScaleSettings::scaled_center_about_baseline`): the rotation pivot IS
    // the scaled box centre, so anchoring it here is what keeps a height-scaled
    // glyph on the run's baseline before any rotation is applied.
    let (center_x, center_y) = scale.scaled_center_about_baseline(
        src_left,
        src_top,
        glyph_w as f32,
        glyph_h as f32,
        src_top + placement_top,
    );
    Some(RotatedGlyphPlacement {
        outline,
        glyph_rgba,
        glyph_w,
        glyph_h,
        src_left,
        src_top,
        placement_left,
        placement_top,
        scale,
        text_color,
        subpixel,
        faux,
        bounds_pad,
        center_x,
        center_y,
        rotation_rad: rotation.glyph_rad,
        group_key: rotation.group_key,
        group_rotation_rad: rotation.group_rad,
        // Set by the collection loop when hanging punctuation is enabled.
        hanging_excluded: false,
        // Overwritten by the collection loop, which knows the run's line. A placement
        // built outside a run loop (none today) reads as line 0.
        line_idx: 0,
    })
}

/// Повернуть глифы одной группы как жёсткое тело: вокруг центроида группы, добавляя
/// поворот группы к собственному повороту каждого глифа.
fn apply_rotated_group_rotations(placements: &mut [RotatedGlyphPlacement]) {
    let mut i = 0;
    while i < placements.len() {
        let Some(key) = placements[i].group_key else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < placements.len() && placements[j].group_key == Some(key) {
            j += 1;
        }
        let group_rotation = placements[i].group_rotation_rad;
        let count = (j - i) as f32;
        let center_x = placements[i..j].iter().map(|p| p.center_x).sum::<f32>() / count;
        let center_y = placements[i..j].iter().map(|p| p.center_y).sum::<f32>() / count;
        let (sin_a, cos_a) = group_rotation.sin_cos();
        for placement in &mut placements[i..j] {
            let rel_x = placement.center_x - center_x;
            let rel_y = placement.center_y - center_y;
            placement.center_x = center_x + rel_x * cos_a - rel_y * sin_a;
            placement.center_y = center_y + rel_x * sin_a + rel_y * cos_a;
            placement.rotation_rad += group_rotation;
        }
        i = j;
    }
}

impl RigidPlacement for RotatedGlyphPlacement {
    fn placement_center(&self) -> (f32, f32) {
        (self.center_x, self.center_y)
    }
    fn set_placement_center(&mut self, x: f32, y: f32) {
        self.center_x = x;
        self.center_y = y;
    }
    fn add_placement_rotation(&mut self, angle_rad: f32) {
        self.rotation_rad += angle_rad;
    }
}

/// Повернуть ВСЕ размещения как единое жёсткое тело вокруг центроида всей
/// раскладки (глобальный поворот блока), добавляя `angle_rad` к собственному
/// повороту каждого глифа. Делегирует общей `rotate_placements_about_centroid`,
/// чтобы математика поворота совпадала со всеми остальными режимами и с
/// пост-поворотом слоя по Ctrl+колесо.
fn apply_global_rotation(placements: &mut [RotatedGlyphPlacement], angle_rad: f32) {
    rotate_placements_about_centroid(
        placements
            .iter_mut()
            .map(|placement| placement as &mut dyn RigidPlacement)
            .collect(),
        angle_rad,
    );
}

/// Горизонтальный рендер обычного текста с inline-поворотами смещений.
/// Собирает размещения всех глифов, применяет повороты групп, считает повёрнутый
/// bbox и выводит каждый глиф обратной выборкой с поворотом.
///
/// `shaping` — inline-контекст рендера (params, спаны, смещения строк, кернинг,
/// attrs и faux-fallback для переносимого мягкого переноса); ширина блока и
/// глобальный поворот берутся из `shaping.pen.params`. `line_height_px` — высота
/// пустого (прозрачного) результата.
pub(super) fn render_horizontal_rotated(
    shaping: RunShaping<'_>,
    font_system: &mut FontSystem,
    buffer: &Buffer,
    line_baselines: &[f32],
    line_height_px: f32,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let RunPen {
        params,
        inline_style_spans,
        layout_line_offsets,
        font_size_px,
        ..
    } = shaping.pen;
    let width_px = params.width_px.max(1);
    let global_rotation_deg = params.global_rotation_deg;
    // Per-render swash / outline / optical ink-contour caches: each glyph is
    // extracted at most once per cache.
    let mut caches = GlyphCaches::new();
    // Reused per-glyph rasterizer buffers for the draw pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info sampler; per-glyph exclusion of hanging punctuation is
    // recorded on each placement and read after the global rotation is baked in.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    let mut placements: Vec<RotatedGlyphPlacement> = Vec::new();
    let inline_line_aligns =
        compute_inline_line_aligns(params.align, layout_line_offsets, inline_style_spans);
    let mut line_idx = 0usize;
    // Normalized once per render: `0.0` disables the hang, `1.0` is the full hang.
    let hanging_weight = params.hanging_weight();
    let mut runs = buffer.layout_runs().peekable();

    while let Some(run) = runs.next() {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        let hanging_bounds = if extra_active && params.excludes_hanging_from_extra_info() {
            hanging_edge_run_bounds(&run)
        } else {
            (0, run.glyphs.len())
        };
        let run_layout = horizontal_run_layout(shaping.pen, &run, font_system, &mut caches);
        // Hanging punctuation is a WEIGHT on the line, not a glyph move: the pen
        // positions stay as shaped and only the width this line is aligned by, plus
        // its origin, lose the weighted part of the hanging edge runs.
        let line_offset_x = horizontal_line_offset(
            width_px,
            run_layout.align_width_px(hanging_weight),
            inline_line_aligns
                .get(line_idx)
                .copied()
                .unwrap_or(params.align),
        ) as f32
            - run_layout.origin_shift_px(hanging_weight);
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or(run.line_y);

        for (glyph_idx, (glyph, glyph_x)) in run
            .glyphs
            .iter()
            .zip(run_layout.glyph_xs.iter().copied())
            .enumerate()
        {
            let glyph_text_color = inline_text_color_for_glyph(
                params.text_color,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            let offset = inline_glyph_offset_style_for_glyph(
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            let glyph_faux = resolve_faux_counter_flag(
                faux_style_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                font_system,
                &mut caches.outlines,
                glyph,
            );
            let group_key = if offset.group_rotation_rad.abs() > f32::EPSILON {
                inline_glyph_offset_span_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                )
            } else {
                None
            };
            if let Some(mut placement) = build_rotated_placement(
                font_system,
                &mut caches,
                glyph,
                [
                    line_offset_x + (glyph_x - glyph.x) + offset.global_px[0],
                    baseline_y + offset.global_px[1],
                ],
                GlyphStyle {
                    scale: glyph_scale,
                    faux: glyph_faux,
                },
                glyph_text_color,
                InlineGlyphRotation {
                    glyph_rad: offset.glyph_rotation_rad,
                    group_key,
                    group_rad: offset.group_rotation_rad,
                },
            ) {
                placement.hanging_excluded = is_edge_run_hanging(hanging_bounds, glyph_idx);
                placement.line_idx = line_idx;
                placements.push(placement);
            }
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) =
                build_wrapped_hyphen_glyph(font_system, shaping, &run, runs.peek())
        {
            let style_offset = soft_hyphen_style_offset(&run, runs.peek(), layout_line_offsets);
            let hyphen_text_color = style_offset
                .map(|offset| {
                    inline_text_color_at_offset(params.text_color, inline_style_spans, offset)
                })
                .unwrap_or(params.text_color);
            let hyphen_scale = style_offset
                .map(|offset| inline_glyph_scale_at_offset(params, inline_style_spans, offset))
                .unwrap_or_else(|| GlyphScaleSettings::from_params(params));
            let hyphen_offset = style_offset
                .map(|offset| inline_glyph_offset_style_at_offset(inline_style_spans, offset))
                .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0]));
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
                &mut caches.outlines,
                &hyphen_glyph,
            );
            let group_key = if hyphen_offset.group_rotation_rad.abs() > f32::EPSILON {
                style_offset
                    .and_then(|offset| inline_glyph_offset_span_at_offset(inline_style_spans, offset))
            } else {
                None
            };
            let hyphen_offset_x = line_offset_x
                + trailing_hyphen_x(&run)
                + run_layout
                    .glyph_xs
                    .last()
                    .zip(run.glyphs.last())
                    .map(|(glyph_x, last_glyph)| glyph_x - last_glyph.x)
                    .unwrap_or(0.0);
            if let Some(placement) = build_rotated_placement(
                font_system,
                &mut caches,
                &hyphen_glyph,
                [
                    hyphen_offset_x + hyphen_offset.global_px[0],
                    baseline_y + hyphen_offset.global_px[1],
                ],
                GlyphStyle {
                    scale: hyphen_scale,
                    faux: hyphen_faux,
                },
                hyphen_text_color,
                InlineGlyphRotation {
                    glyph_rad: hyphen_offset.glyph_rotation_rad,
                    group_key,
                    group_rad: hyphen_offset.group_rotation_rad,
                },
            ) {
                placements.push(placement);
            }
        }
        line_idx += 1;
    }

    apply_rotated_group_rotations(&mut placements);

    // Optional vector mesh warp. It normalizes over the PRE-global-rotation
    // layout box (placements carry their group/inline rotation here but NOT yet
    // the global rotation) and is peeled/reapplied around the same global-rotation
    // centroid that `apply_global_rotation` uses. Both the box AABB and the
    // centroid must be captured BEFORE the global rotation is applied.
    let global_rotation_rad = if global_rotation_deg.abs() > f32::EPSILON {
        global_rotation_deg.to_radians()
    } else {
        0.0
    };
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let mut pre_box = PixelBounds::empty();
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        for placement in &placements {
            // Faux-padded rect (plain rect when faux is off).
            let (scaled_left, scaled_top, scaled_width, scaled_height) =
                placement.padded_scaled_rect();
            include_rotated_rect_bounds(
                &mut pre_box,
                RotatedRect {
                    rect: (scaled_left, scaled_top, scaled_width, scaled_height),
                    dst_center: [placement.center_x, placement.center_y],
                    rotation_rad: placement.rotation_rad,
                },
            );
            sum_x += placement.center_x;
            sum_y += placement.center_y;
        }
        if !pre_box.initialized || placements.is_empty() {
            return None;
        }
        let count = placements.len() as f32;
        let centroid = [sum_x / count, sum_y / count];
        let box_min = [pre_box.min_x as f32, pre_box.min_y as f32];
        let box_size = [
            (pre_box.max_x - pre_box.min_x) as f32,
            (pre_box.max_y - pre_box.min_y) as f32,
        ];
        MeshWarpContext::new(warp, box_min, box_size, global_rotation_rad, centroid)
    });

    // Глобальный поворот: жёстко вращаем ВСЕ размещения вокруг центроида всей
    // раскладки. Делается после групповых поворотов и ДО расчёта bbox/размера
    // холста, поэтому изображение само вырастает под повёрнутые границы.
    if global_rotation_deg.abs() > f32::EPSILON {
        apply_global_rotation(&mut placements, global_rotation_rad);
    }

    let mut bounds = PixelBounds::empty();
    for placement in &placements {
        // Faux-padded rect (plain rect when faux is off), so offset/sheared ink
        // never clips the auto-grown canvas.
        let (scaled_left, scaled_top, scaled_width, scaled_height) =
            placement.padded_scaled_rect();
        include_rotated_rect_bounds(
            &mut bounds,
            RotatedRect {
                rect: (scaled_left, scaled_top, scaled_width, scaled_height),
                dst_center: [placement.center_x, placement.center_y],
                rotation_rad: placement.rotation_rad,
            },
        );
    }
    // Grow the canvas to the warped extent (peel->warp->reapply of the lattice
    // nodes) so a strong outward warp never clips. No-op for `None`/identity.
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }
    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let pad = (font_size_px * 0.5).ceil().max(2.0) as i32;
    let out_width = u32::try_from((bounds.max_x - bounds.min_x).max(1))
        .unwrap_or(1)
        .saturating_add(pad as u32 * 2);
    let out_height = u32::try_from((bounds.max_y - bounds.min_y).max(1))
        .unwrap_or(1)
        .saturating_add(pad as u32 * 2);
    let x_offset = -bounds.min_x + pad;
    let y_offset = -bounds.min_y + pad;

    // Extra-info centers: the placement centers/rotations are final (post global
    // rotation), so the sample boxes match the draw pass. Warp through the same
    // context (peel/reapply is baked in) and map to canvas pixels via the offset.
    if extra_active {
        for placement in &placements {
            // Hanging punctuation and glyphs a faux thinning offset consumed
            // entirely draw no ink here, so neither may move the reported centers.
            if placement.hanging_excluded || !placement.draws_ink() {
                continue;
            }
            let (corners, center) = rotated_placement_extra_samples(placement);
            // Warpable only for outline glyphs; a color-glyph bitmap fallback draws
            // unwarped pixels, so its sample must not be warped either.
            extra_acc.add_glyph(
                corners,
                center,
                placement.outline.is_some(),
                placement.line_idx,
            );
        }
        if let Some(ctx) = warp_ctx.as_ref() {
            extra_acc.map_points(|point| ctx.warp_world(point));
        }
    }
    let extra = extra_acc.finish(x_offset as f32, y_offset as f32);

    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];
    for placement in &placements {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        // Prefer the true font outline, rasterized at the same rotated/scaled
        // world placement the bitmap blit would have used (dst_center =
        // placement.center after group rotation).
        if let Some(outline) = placement.outline.as_ref() {
            let transform = glyph_outline_transform(
                GlyphBox {
                    placement_left: placement.placement_left,
                    placement_top: placement.placement_top,
                    glyph_w: placement.glyph_w as f32,
                    glyph_h: placement.glyph_h as f32,
                    scale: placement.scale,
                    subpixel: placement.subpixel,
                    shear_x: placement.faux.shear_x,
                },
                [placement.center_x, placement.center_y],
                placement.rotation_rad,
            );
            rasterize_outline_into(
                &mut raster_scratch,
                &mut RgbaCanvasView {
                    rgba: rgba.as_mut_slice(),
                    width: out_width as usize,
                    height: out_height as usize,
                },
                [-(x_offset as f32), -(y_offset as f32)],
                outline,
                &transform,
                OutlinePaint {
                    color: placement.text_color,
                    aa_lut: &aa_lut,
                    warp: warp_ctx.as_ref(),
                },
            );
            continue;
        }
        // A glyph whose outline was CONSUMED by a faux thinning offset carries an
        // EMPTY `glyph_rgba` (no fallback bitmap was built for it) while keeping
        // its non-zero box size, and must draw nothing. Skipping it explicitly
        // instead of relying on the blitter's out-of-range pixel fallback also
        // saves a full destination-box scan that writes nothing.
        if !placement.draws_ink() {
            continue;
        }
        // No fillable outline: blit the fallback bitmap for any outline-less glyph
        // (real color glyph or a monochrome embedded-bitmap glyph).
        let mut canvas = RgbaCanvasView {
            rgba: rgba.as_mut_slice(),
            width: out_width as usize,
            height: out_height as usize,
        };
        draw_rotated_scaled_glyph_rgba(
            &mut canvas,
            RgbaView {
                rgba: placement.glyph_rgba.as_slice(),
                width: placement.glyph_w,
                height: placement.glyph_h,
            },
            RotatedGlyphPose {
                src_left: placement.src_left,
                src_top: placement.src_top,
                scale: placement.scale,
                dst_center: [placement.center_x, placement.center_y],
                rotation_rad: placement.rotation_rad,
            },
            [x_offset, y_offset],
        );
    }

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        // Filled in by `render_text_to_image`, which owns the shaped buffer.
        font_fallbacks: FontFallbackReport::default(),
    })
}

/// Content-space placement-box `(corners, center)` of a rotated horizontal glyph
/// AFTER all rotations are baked into `center_x/center_y`/`rotation_rad`. The
/// scaled box half-extents are rotated about the final center with the shared
/// screen (y-down) `[cos -sin; sin cos]` convention (same as
/// `rotated_rect_world_bounds`).
///
/// Only the scaled SIZE is read from the rect, so the box-centre-anchored
/// `scaled_rect` is the right call here: the position comes from `center_x`/
/// `center_y`, which `build_rotated_placement` already anchored at the baseline.
#[must_use]
fn rotated_placement_extra_samples(
    placement: &RotatedGlyphPlacement,
) -> ([[f32; 2]; 4], [f32; 2]) {
    let (_, _, width, height) = placement.scale.scaled_rect(
        placement.src_left,
        placement.src_top,
        placement.glyph_w as f32,
        placement.glyph_h as f32,
    );
    let center = [placement.center_x, placement.center_y];
    let (sin_a, cos_a) = placement.rotation_rad.sin_cos();
    let half_w = width * 0.5;
    let half_h = height * 0.5;
    let local = [
        [-half_w, -half_h],
        [half_w, -half_h],
        [half_w, half_h],
        [-half_w, half_h],
    ];
    let corners = local.map(|[lx, ly]| {
        [
            center[0] + lx * cos_a - ly * sin_a,
            center[1] + lx * sin_a + ly * cos_a,
        ]
    });
    (corners, center)
}
