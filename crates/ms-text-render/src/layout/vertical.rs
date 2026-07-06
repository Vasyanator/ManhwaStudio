/*
File: src/tabs/typing/render_next/layout/vertical.rs

Purpose:
Vertical raster/layout path staged рендера typing.

Main responsibilities:
- превратить vertical `layout_text` и shaped glyph runs в колонки/cells;
- посчитать column positions и vertical optical spacing без участия старого `render.rs`;
- собрать итоговый `RenderedTextImage`.

Optical kerning (vertical axis):
`KerningMode::Optical` re-spaces adjacent inked glyphs of a column. For each pair
the MINIMUM DIRECTIONAL vertical whitespace is measured from the glyph outlines
placed through the same draw-pass pivot (`place_optical_vertical_contour` +
`optical_pair_gap` with `OpticalAxis::Vertical`): the smallest
`cur_top(x) - prev_bottom(x)` over the pair's overlapping horizontal band (the
closest facing points; a scanline projection, not a Euclidean min-distance). That
min gap is normalized toward the column median so the closest points become
uniform, and the same min gap feeds the collision floor. The pure numeric core
(`median_of_gaps`/`optical_delta`/`optical_base_advance`/`optical_pair_gap`) is
shared with the horizontal path in `render_next::optical`.
Every non-Optical kerning mode keeps `delta == 0` and stays byte-identical to
the pre-optical stacking. The vertical stacking is ink-height based and never
applies font pair kerning, so `Fixed` and `Auto` coincide here (only `Optical`
re-spaces); spaces and line breaks reset the pair chain, so optical kerning never
crosses them.

Draw path:
Each drawn glyph rasterizes its true font outline via the shared `glyph_blit`
helpers (`resolve_outline_for_glyph` + `glyph_outline_transform`) and
`vector::rasterize_outline_into`, placed on exactly the pixels the old swash
bitmap blit produced (scale about the bitmap center, no rotation). Layout,
bounds, optical spacing and visual-width measurement stay swash-bitmap based, so
switching the draw is AA-only. Only COLR/bitmap color glyphs (no monochrome
outline) fall back to the `raster.rs` bitmap blit.

Mesh warp (`TextRenderParams.raster_transform`):
Honored on the outline draw seam. The bounds pass captures the un-rotated scaled
`pre_box` (gated on `raster_transform.is_some()`); `MeshWarpContext::new`
normalizes over it and peels/reapplies the SAME `global_rotation_rad` +
`vertical_layout_centroid` the draw uses, so the warp is exact. `None`/identity is
byte-identical; the color-glyph bitmap fallback is not warped.

Source:
- `render_vertical_text`
- `collect_vertical_render_columns`
- `compute_vertical_column_positions`
- `compute_vertical_cell_baselines`
- `optical_vertical_gap_deltas` / `place_optical_vertical_contour`
частично из старого `src/tabs/typing/render.rs`
*/

use crate::glyph_blit::{
    glyph_outline_transform, glyph_subpixel_offset, hash_font_id, resolve_outline_for_glyph,
};
use crate::glyph_contour::PlacedContour;
use crate::inline_styles::InlineStyleSpan;
use crate::optical::{
    OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX, OpticalAxis, OpticalContourCache, median_of_gaps,
    optical_base_advance, optical_delta, optical_pair_gap,
};
use crate::pipeline::{
    GlyphScaleSettings, KerningSettings, inline_glyph_offset_for_glyph,
    inline_glyph_scale_for_glyph, inline_kerning_for_glyph, inline_text_color_for_glyph,
};
use crate::raster::{
    GlyphRgbaView, PixelBounds, RgbaCanvasView, build_glyph_rgba_buffer,
    draw_rotated_scaled_glyph_rgba, draw_scaled_glyph_rgba, include_rotated_rect_bounds,
    include_scaled_rect_bounds, rasterize_unscaled_glyph, sample_swash_alpha,
};
use crate::types::{
    KerningMode, RenderedTextImage, TextRenderParams, VerticalLineDirection,
};
use crate::vector::{
    MeshWarpContext, OutlineCache, RasterScratch, build_aa_lut, glyph_contour_from_outline,
    rasterize_outline_into,
};
use cosmic_text::{Buffer, FontSystem, LayoutGlyph, SwashCache};

const OPTICAL_ALPHA_THRESHOLD: u8 = 24;
const VERTICAL_HALF_SPACE: char = '\u{200A}';

pub(crate) struct VerticalRasterRequest<'a> {
    pub(crate) params: &'a TextRenderParams,
    pub(crate) font_system: &'a mut FontSystem,
    pub(crate) buffer: &'a mut Buffer,
    pub(crate) layout_text: &'a str,
    pub(crate) inline_style_spans: Option<&'a [InlineStyleSpan]>,
    pub(crate) layout_line_offsets: &'a [usize],
    pub(crate) font_size_px: f32,
    pub(crate) base_line_height_px: f32,
    pub(crate) line_extra_spacing_table: &'a [f32],
    pub(crate) direction: VerticalLineDirection,
}

#[derive(Clone)]
enum VerticalRenderCell {
    Glyph {
        glyph: LayoutGlyph,
        text_color: [u8; 4],
        glyph_scale: GlyphScaleSettings,
        kerning: KerningSettings,
        glyph_offset_px: [f32; 2],
    },
    Blank(f32),
}

#[derive(Clone)]
struct VerticalRenderColumn {
    cells: Vec<VerticalRenderCell>,
    visual_width_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct GlyphInkProfile {
    top_px: f32,
    bottom_px: f32,
}

impl GlyphInkProfile {
    #[must_use]
    fn fallback(height_px: f32) -> Self {
        Self {
            top_px: 0.0,
            bottom_px: height_px.max(1.0),
        }
    }

    #[must_use]
    fn height_px(self) -> f32 {
        (self.bottom_px - self.top_px).max(1.0)
    }
}

pub(crate) fn render_vertical_text(
    request: VerticalRasterRequest<'_>,
) -> Result<RenderedTextImage, String> {
    let VerticalRasterRequest {
        params,
        font_system,
        buffer,
        layout_text,
        inline_style_spans,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_extra_spacing_table,
        direction,
    } = request;
    let width_px = layout_text.lines().count().max(1);
    let mut cache = SwashCache::new();
    let columns = collect_vertical_render_columns(
        params,
        buffer,
        font_system,
        &mut cache,
        layout_text,
        inline_style_spans,
        layout_line_offsets,
        font_size_px,
        params.text_color,
    );
    if columns.is_empty() {
        return Ok(RenderedTextImage::transparent(
            u32::try_from(width_px).unwrap_or(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let column_positions = compute_vertical_column_positions(
        columns.as_slice(),
        line_extra_spacing_table,
        line_extra_spacing_table.first().copied().unwrap_or(0.0),
        direction,
    );
    // Per-render outline + ink-contour caches. The outline cache feeds both the
    // draw-pass vector rasterizer and the optical ink measurement; the contour
    // cache derives each glyph's ink contour once. Created before the bounds pass
    // so the baselines (which the optical path may re-space) are computed with the
    // same caches on both passes. Unused for non-Optical kerning modes.
    let mut outline_cache = OutlineCache::new();
    let mut contour_cache = OpticalContourCache::new();
    // Reused per-glyph rasterizer buffers for the draw pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();

    // Global block rotation (vector level): turn the whole column block rigidly
    // about its centroid, matching the Ctrl+wheel overlay post-rotation but crisp.
    // The centroid is computed once up front so the bounds and draw passes rotate
    // every glyph about the same pivot; `rotate_block` gates all of it so the
    // non-rotated path stays byte-identical.
    let global_rotation_rad = params.global_rotation_deg.to_radians();
    let rotate_block = params.global_rotation_deg.abs() > f32::EPSILON;
    let (rot_sin, rot_cos) = global_rotation_rad.sin_cos();
    let (centroid_x, centroid_y) = if rotate_block {
        vertical_layout_centroid(
            columns.as_slice(),
            column_positions.as_slice(),
            params,
            font_system,
            &mut cache,
            &mut outline_cache,
            &mut contour_cache,
            font_size_px,
        )
    } else {
        (0.0, 0.0)
    };

    // A mesh warp normalizes over the PRE-global-rotation content box in layout
    // space, so capture the un-rotated scaled-glyph AABB in `pre_box` alongside the
    // (possibly rotated) `bounds`. Gated on `raster_transform` so the no-warp fast
    // path does no extra work and stays byte-identical.
    let want_warp = params.raster_transform.is_some();
    let mut pre_box = PixelBounds::empty();
    let mut bounds = PixelBounds::empty();
    for (column_idx, column) in columns.iter().enumerate() {
        let Some(column_x) = column_positions.get(column_idx).copied() else {
            continue;
        };
        let cell_baselines = compute_vertical_cell_baselines(
            column,
            params,
            font_system,
            &mut cache,
            &mut outline_cache,
            &mut contour_cache,
            font_size_px,
        );
        for (cell_idx, cell) in column.cells.iter().enumerate() {
            let VerticalRenderCell::Glyph {
                glyph,
                glyph_scale,
                glyph_offset_px,
                ..
            } = cell
            else {
                continue;
            };
            let baseline_y = cell_baselines.get(cell_idx).copied().unwrap_or(font_size_px)
                + glyph_offset_px[1];
            let origin_x = column_x + ((column.visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x
                + glyph_offset_px[0];
            let physical = glyph.physical((origin_x, baseline_y), 1.0);
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let x = physical.x + image.placement.left;
            let y = physical.y - image.placement.top;
            if want_warp {
                // Un-rotated scaled glyph box (the same math the non-rotated bounds
                // branch uses) — this is the warp's normalization frame.
                include_scaled_rect_bounds(
                    &mut pre_box,
                    x as f32,
                    y as f32,
                    image.placement.width as f32,
                    image.placement.height as f32,
                    *glyph_scale,
                );
            }
            if rotate_block {
                // Rotate the glyph's (unscaled==scaled) center about the centroid,
                // then account for the rotated scaled rect so the canvas grows.
                let glyph_w = image.placement.width as f32;
                let glyph_h = image.placement.height as f32;
                let (scaled_left, scaled_top, scaled_width, scaled_height) =
                    glyph_scale.scaled_rect(x as f32, y as f32, glyph_w, glyph_h);
                let (dst_center_x, dst_center_y) = rotate_point_about(
                    x as f32 + glyph_w * 0.5,
                    y as f32 + glyph_h * 0.5,
                    centroid_x,
                    centroid_y,
                    rot_sin,
                    rot_cos,
                );
                include_rotated_rect_bounds(
                    &mut bounds,
                    scaled_left,
                    scaled_top,
                    scaled_width,
                    scaled_height,
                    dst_center_x,
                    dst_center_y,
                    global_rotation_rad,
                );
            } else {
                include_scaled_rect_bounds(
                    &mut bounds,
                    x as f32,
                    y as f32,
                    image.placement.width as f32,
                    image.placement.height as f32,
                    *glyph_scale,
                );
            }
        }
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            u32::try_from(width_px).unwrap_or(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // Optional vector mesh warp. Normalize over the PRE-global-rotation content box
    // (`pre_box`) and peel/reapply the SAME global rotation the bounds/draw passes
    // use (`global_rotation_rad` about `vertical_layout_centroid`). `None`, an
    // identity/invalid mesh, or a degenerate box take the byte-identical fast path.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        if !pre_box.initialized {
            return None;
        }
        let box_min = [pre_box.min_x as f32, pre_box.min_y as f32];
        let box_size = [
            (pre_box.max_x - pre_box.min_x) as f32,
            (pre_box.max_y - pre_box.min_y) as f32,
        ];
        MeshWarpContext::new(
            warp,
            box_min,
            box_size,
            global_rotation_rad,
            [centroid_x, centroid_y],
        )
    });
    // Grow the canvas to the warped+rotated lattice extent so a strong outward warp
    // never clips (no-op for `None`/identity).
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }

    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let content_width = (bounds.max_x - bounds.min_x).max(1) as u32;
    let content_height = (bounds.max_y - bounds.min_y).max(1) as u32;
    let out_width = content_width
        .saturating_add(horizontal_pad * 2)
        .saturating_add(safety_pad * 2);
    let out_height = content_height
        .max(base_line_height_px.ceil().max(1.0) as u32)
        .saturating_add(vertical_pad * 2)
        .saturating_add(safety_pad * 2);
    let x_offset = -bounds.min_x + horizontal_pad as i32 + safety_pad as i32;
    let y_offset = -bounds.min_y + vertical_pad as i32 + safety_pad as i32;
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);

    for (column_idx, column) in columns.iter().enumerate() {
        let Some(column_x) = column_positions.get(column_idx).copied() else {
            continue;
        };
        let cell_baselines = compute_vertical_cell_baselines(
            column,
            params,
            font_system,
            &mut cache,
            &mut outline_cache,
            &mut contour_cache,
            font_size_px,
        );
        for (cell_idx, cell) in column.cells.iter().enumerate() {
            let VerticalRenderCell::Glyph {
                glyph,
                text_color,
                glyph_scale,
                glyph_offset_px,
                ..
            } = cell
            else {
                continue;
            };
            let baseline_y = cell_baselines.get(cell_idx).copied().unwrap_or(font_size_px)
                + glyph_offset_px[1];
            let origin_x = column_x + ((column.visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x
                + glyph_offset_px[0];
            let physical = glyph.physical((origin_x, baseline_y), 1.0);
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
            // World-space (pre-canvas-offset) top-left of the glyph bitmap, matching
            // the bounds pass and the old bitmap blit's `src_left`/`src_top`.
            let src_left = (physical.x + image.placement.left) as f32;
            let src_top = (physical.y - image.placement.top) as f32;

            // Rotate the glyph's (unscaled==scaled) center about the block centroid;
            // with no global rotation this is the identity (centroid at origin, angle
            // 0), so the non-rotated placement stays byte-identical.
            let (dst_center_x, dst_center_y) = rotate_point_about(
                src_left + glyph_w as f32 * 0.5,
                src_top + glyph_h as f32 * 0.5,
                centroid_x,
                centroid_y,
                rot_sin,
                rot_cos,
            );

            // Prefer the true font outline: rasterize it at the exact world placement
            // the bitmap blit used (scale about the bitmap center). Vertical cells are
            // upright; the only rotation is the global block rotation. The canvas
            // offset is folded into the rasterizer origin as the horizontal path does.
            if let Some(outline) =
                resolve_outline_for_glyph(font_system, &mut outline_cache, glyph)
            {
                // Re-add the subpixel fraction baked into the bitmap coverage so the
                // outline lands on the same pixels (physical.x/y are integer-only).
                let transform = glyph_outline_transform(
                    dst_center_x,
                    dst_center_y,
                    global_rotation_rad,
                    placement_left,
                    placement_top,
                    glyph_w as f32,
                    glyph_h as f32,
                    glyph_scale.width_mul,
                    glyph_scale.height_mul,
                    glyph_subpixel_offset(physical.cache_key),
                );
                rasterize_outline_into(
                    &mut raster_scratch,
                    rgba.as_mut_slice(),
                    out_width as usize,
                    out_height as usize,
                    -(x_offset as f32),
                    -(y_offset as f32),
                    &outline,
                    &transform,
                    *text_color,
                    &aa_lut,
                    warp_ctx.as_ref(),
                );
                continue;
            }

            // No fillable outline: blit whatever non-empty bitmap `get_image` gave
            // us — real color (COLR/bitmap) glyphs and monochrome embedded-bitmap /
            // sbix / CBDT-mono glyphs alike (spaces are already filtered by size).
            // Under a global block rotation the bitmap fallback must also rotate, via
            // the shared reverse-sampling rotator (same path the horizontal mode uses).
            if rotate_block {
                let glyph_rgba = build_glyph_rgba_buffer(
                    &image.content,
                    image.data.as_slice(),
                    glyph_w,
                    glyph_h,
                    *text_color,
                );
                let mut canvas = RgbaCanvasView {
                    rgba: rgba.as_mut_slice(),
                    width: out_width as usize,
                    height: out_height as usize,
                };
                draw_rotated_scaled_glyph_rgba(
                    &mut canvas,
                    GlyphRgbaView {
                        rgba: glyph_rgba.as_slice(),
                        width: glyph_w,
                        height: glyph_h,
                    },
                    src_left,
                    src_top,
                    *glyph_scale,
                    dst_center_x,
                    dst_center_y,
                    global_rotation_rad,
                    x_offset,
                    y_offset,
                );
                continue;
            }
            let draw_x = physical.x + image.placement.left + x_offset;
            let draw_y = physical.y - image.placement.top + y_offset;
            if glyph_scale.is_identity() {
                rasterize_unscaled_glyph(
                    rgba.as_mut_slice(),
                    out_width,
                    out_height,
                    image.content,
                    image.data.as_slice(),
                    glyph_w,
                    glyph_h,
                    draw_x,
                    draw_y,
                    *text_color,
                );
                continue;
            }

            let glyph_rgba = build_glyph_rgba_buffer(
                &image.content,
                image.data.as_slice(),
                glyph_w,
                glyph_h,
                *text_color,
            );
            let mut canvas = RgbaCanvasView {
                rgba: rgba.as_mut_slice(),
                width: out_width as usize,
                height: out_height as usize,
            };
            draw_scaled_glyph_rgba(
                &mut canvas,
                GlyphRgbaView {
                    rgba: glyph_rgba.as_slice(),
                    width: glyph_w,
                    height: glyph_h,
                },
                draw_x as f32,
                draw_y as f32,
                *glyph_scale,
            );
        }
    }

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
    })
}

/// Rotate point `(px, py)` about pivot `(cx, cy)` given precomputed `sin`/`cos`.
///
/// Uses the standard screen (y-down) rotation matrix `[cos -sin; sin cos]`, the
/// same convention as the horizontal path and the Ctrl+wheel overlay
/// post-rotation, so a positive angle turns text the same visual direction.
fn rotate_point_about(px: f32, py: f32, cx: f32, cy: f32, sin_a: f32, cos_a: f32) -> (f32, f32) {
    let rel_x = px - cx;
    let rel_y = py - cy;
    (cx + rel_x * cos_a - rel_y * sin_a, cy + rel_x * sin_a + rel_y * cos_a)
}

/// Centroid (mean of glyph bitmap-box centers) of the whole vertical layout, used
/// as the pivot for the global block rotation. Mirrors the bounds/draw passes'
/// per-glyph position math so all three agree on the same centers. Returns
/// `(0.0, 0.0)` when the layout has no drawable glyph (the caller only uses this
/// when a rotation is requested).
#[allow(clippy::too_many_arguments)]
fn vertical_layout_centroid(
    columns: &[VerticalRenderColumn],
    column_positions: &[f32],
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
    font_size_px: f32,
) -> (f32, f32) {
    let mut sum_x = 0.0f32;
    let mut sum_y = 0.0f32;
    let mut count = 0u32;
    for (column_idx, column) in columns.iter().enumerate() {
        let Some(column_x) = column_positions.get(column_idx).copied() else {
            continue;
        };
        let cell_baselines = compute_vertical_cell_baselines(
            column,
            params,
            font_system,
            cache,
            outline_cache,
            contour_cache,
            font_size_px,
        );
        for (cell_idx, cell) in column.cells.iter().enumerate() {
            let VerticalRenderCell::Glyph {
                glyph,
                glyph_offset_px,
                ..
            } = cell
            else {
                continue;
            };
            let baseline_y = cell_baselines.get(cell_idx).copied().unwrap_or(font_size_px)
                + glyph_offset_px[1];
            let origin_x = column_x + ((column.visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x
                + glyph_offset_px[0];
            let physical = glyph.physical((origin_x, baseline_y), 1.0);
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let glyph_w = image.placement.width as f32;
            let glyph_h = image.placement.height as f32;
            if glyph_w <= 0.0 || glyph_h <= 0.0 {
                continue;
            }
            let x = (physical.x + image.placement.left) as f32;
            let y = (physical.y - image.placement.top) as f32;
            sum_x += x + glyph_w * 0.5;
            sum_y += y + glyph_h * 0.5;
            count += 1;
        }
    }
    if count == 0 {
        return (0.0, 0.0);
    }
    let count = count as f32;
    (sum_x / count, sum_y / count)
}

// The collector must see layout, spans and per-glyph defaults together to preserve vertical
// glyph order and inline styling in one pass without allocating an intermediate model.
#[allow(clippy::too_many_arguments)]
fn collect_vertical_render_columns(
    params: &TextRenderParams,
    buffer: &mut Buffer,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    layout_text: &str,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    default_text_color: [u8; 4],
) -> Vec<VerticalRenderColumn> {
    let mut columns = Vec::<VerticalRenderColumn>::new();
    let source_columns = layout_text.split('\n').collect::<Vec<_>>();

    for (run_idx, run) in buffer.layout_runs().enumerate() {
        let mut cells = Vec::<VerticalRenderCell>::new();
        let mut visual_width_px = 0.0f32;
        let mut glyph_iter = run.glyphs.iter();
        for ch in source_columns
            .get(run_idx)
            .copied()
            .unwrap_or_default()
            .chars()
        {
            if ch == VERTICAL_HALF_SPACE {
                cells.push(VerticalRenderCell::Blank(0.5));
                continue;
            }
            if ch.is_whitespace() {
                cells.push(VerticalRenderCell::Blank(1.0));
                continue;
            }
            let Some(glyph) = glyph_iter.next() else {
                continue;
            };
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            visual_width_px = visual_width_px.max(measure_vertical_glyph_visual_width(
                font_system,
                cache,
                glyph,
                font_size_px,
                glyph_scale,
            ));
            cells.push(VerticalRenderCell::Glyph {
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    default_text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_scale,
                kerning: inline_kerning_for_glyph(
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
            });
        }
        for glyph in glyph_iter {
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            visual_width_px = visual_width_px.max(measure_vertical_glyph_visual_width(
                font_system,
                cache,
                glyph,
                font_size_px,
                glyph_scale,
            ));
            cells.push(VerticalRenderCell::Glyph {
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    default_text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_scale,
                kerning: inline_kerning_for_glyph(
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
            });
        }
        if !cells.is_empty() {
            columns.push(VerticalRenderColumn {
                cells,
                visual_width_px: visual_width_px.max(font_size_px * 0.5).ceil(),
            });
        }
    }

    columns
}

fn measure_vertical_glyph_visual_width(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
    glyph_scale: GlyphScaleSettings,
) -> f32 {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    if let Some(image) = cache.get_image(font_system, physical.cache_key) {
        let (scaled_width, scaled_height) =
            glyph_scale.scaled_size(image.placement.width as f32, image.placement.height as f32);
        scaled_width
            .max(scaled_height)
            .max(glyph.w.max(1.0) * glyph_scale.width_mul)
    } else {
        glyph.w.max(font_size_px * 0.5) * glyph_scale.width_mul
    }
}

fn compute_vertical_column_positions(
    columns: &[VerticalRenderColumn],
    line_extra_spacing_table: &[f32],
    default_extra_line_spacing_px: f32,
    direction: VerticalLineDirection,
) -> Vec<f32> {
    if columns.is_empty() {
        return Vec::new();
    }

    let mut positions = vec![0.0f32; columns.len()];
    match direction {
        VerticalLineDirection::LeftToRight => {
            let mut x = 0.0f32;
            for (idx, column) in columns.iter().enumerate() {
                positions[idx] = x;
                x += column.visual_width_px
                    + line_extra_spacing_table
                        .get(idx)
                        .copied()
                        .unwrap_or(default_extra_line_spacing_px);
            }
        }
        VerticalLineDirection::RightToLeft => {
            let total_width = columns
                .iter()
                .enumerate()
                .fold(0.0f32, |acc, (idx, column)| {
                    acc + column.visual_width_px
                        + if idx + 1 < columns.len() {
                            line_extra_spacing_table
                                .get(idx)
                                .copied()
                                .unwrap_or(default_extra_line_spacing_px)
                        } else {
                            0.0
                        }
                });
            let mut x = total_width;
            for (idx, column) in columns.iter().enumerate() {
                x -= column.visual_width_px;
                positions[idx] = x;
                if idx + 1 < columns.len() {
                    x -= line_extra_spacing_table
                        .get(idx)
                        .copied()
                        .unwrap_or(default_extra_line_spacing_px);
                }
            }
        }
    }
    positions
}

/// Доля кегля для базового зазора между «чернильными» (ink) боксами глифов столбца.
const VERTICAL_INK_GAP_FRACTION: f32 = 0.1;

/// Базовые линии глифов в столбце.
///
/// Каждый глиф ставится так, чтобы верх его ink-бокса встал на текущую позицию, а
/// шаг до следующего глифа равнялся ink-высоте текущего глифа плюс равномерный зазор
/// (базовый + кернинг). То есть вертикальный шаг определяется реальной высотой глифа,
/// а не полным em, поэтому при нулевом кернинге символы стоят плотно. Кернинг (в % от
/// кегля) и пробелы (em-доли) по-прежнему регулируют расстояние.
///
/// When `params.kerning_mode == KerningMode::Optical`, the uniform base gap between
/// two adjacent inked glyphs is additionally nudged by a per-pair optical delta so
/// the true top-to-bottom ink whitespace of the column converges toward the
/// column's median gap (`optical_vertical_gap_deltas`). Every non-Optical kerning
/// mode (`Fixed`/`Auto`) keeps `delta == 0` and stays byte-identical to the pre-optical
/// stacking. Spaces / line breaks reset the pair chain (a gap is only ever added
/// between two consecutive `Glyph` cells), so optical kerning never crosses them.
// Threads the whole per-column layout plus the font system, both per-render caches,
// and font size; grouping them into a throwaway struct would only hide the plumbing.
#[allow(clippy::too_many_arguments)]
fn compute_vertical_cell_baselines(
    column: &VerticalRenderColumn,
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
    font_size_px: f32,
) -> Vec<f32> {
    let base_gap = font_size_px * VERTICAL_INK_GAP_FRACTION;
    let profiles = column
        .cells
        .iter()
        .map(|cell| match cell {
            VerticalRenderCell::Glyph { glyph, .. } => {
                Some(glyph_ink_profile(font_system, cache, glyph, font_size_px))
            }
            VerticalRenderCell::Blank(_) => None,
        })
        .collect::<Vec<_>>();

    // Optical vertical kerning: per-pair signed gap deltas, gated strictly on
    // Optical. `None` means "keep metric spacing" (delta 0 everywhere) — either a
    // non-Optical mode or a column with fewer than one measurable finite gap.
    let optical_deltas = if params.kerning_mode == KerningMode::Optical {
        optical_vertical_gap_deltas(
            column,
            profiles.as_slice(),
            font_system,
            cache,
            outline_cache,
            contour_cache,
            font_size_px,
            base_gap,
        )
    } else {
        None
    };

    let mut baselines = vec![font_size_px; column.cells.len()];
    let mut current_top = 0.0f32;
    for (idx, cell) in column.cells.iter().enumerate() {
        match cell {
            VerticalRenderCell::Glyph { kerning, .. } => {
                let profile =
                    profiles[idx].unwrap_or_else(|| GlyphInkProfile::fallback(font_size_px));
                // Базовая линия так, чтобы верх ink-бокса оказался на `current_top`.
                baselines[idx] = current_top + font_size_px - profile.top_px;
                current_top += profile.height_px();
                // Зазор добавляется только между двумя глифами одной строки.
                if matches!(
                    column.cells.get(idx + 1),
                    Some(VerticalRenderCell::Glyph { .. })
                ) {
                    // `delta` is 0 for non-Optical modes (byte-identical: adding a
                    // hard 0.0 before the tracking term does not change the sum), and
                    // the optical nudge for the (idx, idx+1) pair when Optical.
                    let delta = optical_deltas
                        .as_ref()
                        .and_then(|deltas| deltas.get(idx).copied())
                        .unwrap_or(0.0);
                    current_top += base_gap + delta + kerning.extra_spacing_px(font_size_px);
                }
            }
            VerticalRenderCell::Blank(height_mul) => {
                current_top += font_size_px * height_mul;
            }
        }
    }
    baselines
}

/// Own vertical advance for one optical vertical step: the metric per-glyph
/// vertical step from `prev`'s ink-top to `cur`'s ink-top (prev ink height plus the
/// uniform base gap). Falls back to the bare `base_gap` when the ink height is not
/// positive/finite (degenerate glyph) via the shared `optical_base_advance`.
///
/// `prev_ink_height_px` is the prev glyph's ink height (px); `base_gap_px` is the
/// uniform metric gap between two glyphs (px).
#[must_use]
fn vertical_base_advance(prev_ink_height_px: f32, base_gap_px: f32) -> f32 {
    // own step = ink height + base gap; metric fallback = the base gap alone.
    optical_base_advance(prev_ink_height_px + base_gap_px, base_gap_px)
}

/// Per-pair optical vertical gap deltas for one column.
///
/// For every adjacent INKED glyph pair `(idx, idx+1)` the two ink contours are
/// placed in the metric stacking configuration (prev ink-top at local 0, cur
/// ink-top at prev's base advance) and the MINIMUM DIRECTIONAL top-to-bottom ink
/// whitespace is measured with `optical_pair_gap` (`OpticalAxis::Vertical`): the
/// smallest `cur_top(x) - prev_bottom(x)` over the pair's overlapping horizontal
/// band (the closest facing points; a scanline projection, not a Euclidean
/// min-distance). A pair broken by a blank/space, a missing ink profile, an
/// outline-less (color) glyph, or no horizontal overlap yields an infinite gap and
/// is not kerned across. The column target is the median of finite per-pair MIN
/// gaps; a column with fewer than one finite gap returns `None` (caller keeps
/// metric spacing).
///
/// Returns a vector indexed by the pair's first cell (`deltas[idx]` = the nudge for
/// the gap after cell `idx`); non-pair indices hold `0.0`.
// Threads the per-column layout and ink profiles plus the font system, both
// per-render caches, and the base gap; a wrapper struct would just hide the wiring.
#[allow(clippy::too_many_arguments)]
fn optical_vertical_gap_deltas(
    column: &VerticalRenderColumn,
    profiles: &[Option<GlyphInkProfile>],
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
    font_size_px: f32,
    base_gap: f32,
) -> Option<Vec<f32>> {
    let cell_count = column.cells.len();
    // gaps[idx] is the minimum directional projected whitespace of the pair
    // (idx, idx+1) — the closest facing points; non-pairs stay `f32::INFINITY`.
    let mut gaps = vec![f32::INFINITY; cell_count];
    for idx in 0..cell_count.saturating_sub(1) {
        let (
            VerticalRenderCell::Glyph {
                glyph: prev_glyph,
                glyph_scale: prev_scale,
                glyph_offset_px: prev_offset,
                ..
            },
            VerticalRenderCell::Glyph {
                glyph: cur_glyph,
                glyph_scale: cur_scale,
                glyph_offset_px: cur_offset,
                ..
            },
        ) = (&column.cells[idx], &column.cells[idx + 1])
        else {
            // A blank/space between glyphs resets the pair chain.
            continue;
        };
        let (Some(prev_profile), Some(cur_profile)) = (profiles[idx], profiles[idx + 1]) else {
            continue;
        };
        let base_advance = vertical_base_advance(prev_profile.height_px(), base_gap);
        // Baselines mirror the draw pass (`baseline = ink_top + font_size - top_px`),
        // so the measured ink matches the drawn ink: prev ink-top at local 0, cur
        // ink-top at `base_advance` below it. The metric bounding-box gap is exactly
        // `base_gap`; the outline whitespace measured here may differ per shape.
        let prev_baseline = font_size_px - prev_profile.top_px;
        let cur_baseline = base_advance + font_size_px - cur_profile.top_px;
        let prev_placed = place_optical_vertical_contour(
            prev_glyph,
            prev_baseline,
            column.visual_width_px,
            prev_offset[0],
            *prev_scale,
            font_system,
            cache,
            outline_cache,
            contour_cache,
        );
        let cur_placed = place_optical_vertical_contour(
            cur_glyph,
            cur_baseline,
            column.visual_width_px,
            cur_offset[0],
            *cur_scale,
            font_system,
            cache,
            outline_cache,
            contour_cache,
        );
        gaps[idx] = match (prev_placed, cur_placed) {
            (Some(prev), Some(cur)) => optical_pair_gap(&prev, &cur, OpticalAxis::Vertical),
            // Outline-less color glyph on either side: not kernable.
            _ => f32::INFINITY,
        };
    }

    // Self-calibrating target: median of finite per-pair MIN gaps. None when the
    // column has no finite gap to normalize.
    let target = median_of_gaps(&gaps)?;
    Some(
        gaps.iter()
            .map(|&gap| optical_delta(gap, target, font_size_px))
            .collect(),
    )
}

/// Place a glyph's ink contour in world space using the exact transform the
/// vertical draw pass uses (upright, `rot = 0`, scale about the bitmap center), so
/// the measured ink matches the drawn ink.
///
/// `baseline_y` is the glyph baseline in layout px. The pen x mirrors the draw
/// pass's `origin_x` (`../vertical.rs` ~line 265) EXCEPT the per-column constant
/// `column_x`, which is identical for every glyph of a column and therefore cancels
/// out of a relative top-to-bottom gap measurement. `visual_width_px` is the column
/// visual width (px) used for the same in-column horizontal centering as the draw,
/// and `glyph_offset_x` is the cell's inline `glyph_offset_px[0]` (px). Keeping the
/// centering and offset here means the measured ink overlap matches the drawn
/// overlap for glyphs of differing advance width. Returns `None` for a space/empty
/// glyph (zero-size placement), an outline-less color glyph, or an empty contour —
/// all treated as non-kernable by the caller. Never panics.
// Threads the glyph plus its baseline, column width, inline x offset, and draw
// scale alongside the font system and both per-render caches; a wrapper struct
// would only hide the draw-pass mirroring these arguments encode.
#[allow(clippy::too_many_arguments)]
fn place_optical_vertical_contour(
    glyph: &LayoutGlyph,
    baseline_y: f32,
    visual_width_px: f32,
    glyph_offset_x: f32,
    glyph_scale: GlyphScaleSettings,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
) -> Option<PlacedContour> {
    // Mirror the draw pass's `origin_x` minus the shared per-column `column_x`:
    // center the glyph in the column and honor the cell's inline x offset.
    let pen_x = ((visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x + glyph_offset_x;
    let physical = glyph.physical((pen_x, baseline_y), 1.0);
    let cache_key = physical.cache_key;
    // Copy the bitmap placement box out before the `cache` image borrow ends.
    let (glyph_w, glyph_h, placement_left, placement_top, src_left, src_top) = {
        let Some(image) = cache.get_image(font_system, cache_key) else {
            return None;
        };
        let gw = image.placement.width;
        let gh = image.placement.height;
        if gw == 0 || gh == 0 {
            return None;
        }
        (
            gw as f32,
            gh as f32,
            image.placement.left as f32,
            image.placement.top as f32,
            (physical.x + image.placement.left) as f32,
            (physical.y - image.placement.top) as f32,
        )
    };

    // Derive the ink contour once per distinct (font, glyph, em); the outline itself
    // is negatively cached by `OutlineCache`, so an outline-less glyph is cheap to
    // re-probe even without a contour-cache entry.
    let contour_key = (
        hash_font_id(glyph.font_id),
        glyph.glyph_id,
        glyph.font_size.to_bits(),
    );
    let contour = match contour_cache.entry(contour_key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let outline = resolve_outline_for_glyph(font_system, outline_cache, glyph)?;
            entry.insert(glyph_contour_from_outline(
                &outline,
                OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX,
            ))
        }
    };
    if contour.is_empty() {
        return None;
    }

    let dst_center_x = src_left + glyph_w * 0.5;
    let dst_center_y = src_top + glyph_h * 0.5;
    let transform = glyph_outline_transform(
        dst_center_x,
        dst_center_y,
        0.0,
        placement_left,
        placement_top,
        glyph_w,
        glyph_h,
        glyph_scale.width_mul,
        glyph_scale.height_mul,
        glyph_subpixel_offset(cache_key),
    );
    Some(transform.place_contour(contour))
}

fn glyph_ink_profile(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
) -> GlyphInkProfile {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
        return GlyphInkProfile::fallback(font_size_px);
    };
    glyph_ink_profile_from_image(
        (physical.y - image.placement.top) as f32,
        &image.content,
        image.data.as_slice(),
        [
            image.placement.width as usize,
            image.placement.height as usize,
        ],
        font_size_px,
    )
}

fn glyph_ink_profile_from_image(
    draw_top_px: f32,
    content: &cosmic_text::SwashContent,
    data: &[u8],
    glyph_size: [usize; 2],
    fallback_height_px: f32,
) -> GlyphInkProfile {
    let [glyph_w, glyph_h] = glyph_size;
    if glyph_w == 0 || glyph_h == 0 {
        return GlyphInkProfile::fallback(fallback_height_px);
    }

    let mut min_y = glyph_h;
    let mut max_y = 0usize;
    let mut has_alpha = false;

    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            if sample_swash_alpha(content, data, glyph_w, gx, gy) < OPTICAL_ALPHA_THRESHOLD {
                continue;
            }
            min_y = min_y.min(gy);
            max_y = max_y.max(gy);
            has_alpha = true;
        }
    }

    if !has_alpha {
        return GlyphInkProfile::fallback(fallback_height_px);
    }

    GlyphInkProfile {
        top_px: draw_top_px + min_y as f32,
        bottom_px: draw_top_px + max_y as f32 + 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_vertical_column_positions, vertical_base_advance};
    use crate::pipeline::render_text_to_image;
    use crate::types::{
        AntiAliasingMode, HorizontalAlign, KerningMode, RenderedTextImage,
        TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode, TextLineMode,
        TextRenderParams, TextShape, TextVectorLinesLayoutParams, TextWrapMode,
        VerticalLineDirection,
    };
    use std::path::PathBuf;

    #[test]
    fn right_to_left_columns_shift_from_total_width() {
        let columns = vec![
            super::VerticalRenderColumn {
                cells: Vec::new(),
                visual_width_px: 10.0,
            },
            super::VerticalRenderColumn {
                cells: Vec::new(),
                visual_width_px: 12.0,
            },
        ];

        let positions = compute_vertical_column_positions(
            columns.as_slice(),
            &[4.0, 4.0],
            4.0,
            VerticalLineDirection::RightToLeft,
        );

        assert_eq!(positions, vec![16.0, 0.0]);
    }

    #[test]
    fn vertical_base_advance_uses_step_or_metric_fallback() {
        // Normal case: the own vertical step is ink height plus the base gap.
        assert!((vertical_base_advance(30.0, 6.0) - 36.0).abs() < 1e-4);
        // Degenerate ink height driving the own step non-positive falls back to
        // the bare base gap so the pair still advances by a sane metric amount.
        assert!((vertical_base_advance(-10.0, 6.0) - 6.0).abs() < 1e-4);
        assert!((vertical_base_advance(f32::NAN, 6.0) - 6.0).abs() < 1e-4);
    }

    fn test_font_path() -> PathBuf {
        // Fixture lives at the workspace root; this crate sits two levels down
        // (crates/ms-text-render), so anchor CARGO_MANIFEST_DIR up two dirs.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    fn vertical_params(text: &str) -> TextRenderParams {
        TextRenderParams {
            text: text.to_string(),
            text_color: [255, 255, 255, 255],
            font_path: test_font_path(),
            available_inline_fonts: Vec::new(),
            font_size_px: 64.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 100.0,
            kerning_mode: KerningMode::Auto,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 400,
            align: HorizontalAlign::LEFT,
            selected_face_index: 0,
            force_bold: false,
            force_italic: false,
            uppercase_text: false,
            trim_extra_spaces: true,
            hanging_punctuation: false,
            new_line_after_sentence: false,
            enable_inline_style_tags: false,
            text_wrap_mode: TextWrapMode::WholeWords,
            text_shape: TextShape::Free,
            shape_min_width_percent: 100.0,
            shape_variant: 5,
            compare_shape_with: None,
            allow_moderate_trees: false,
            text_line_mode: TextLineMode::Vertical,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            effects_json: String::new(),
            // Identity transfer keeps the AA independent of these geometry checks.
            anti_aliasing: AntiAliasingMode::Smooth,
            global_rotation_deg: 0.0,
            line_placement_percent: 0.0,
            raster_transform: None,
        }
    }

    fn render(params: &TextRenderParams) -> RenderedTextImage {
        render_text_to_image(params, None).expect("vertical render succeeds")
    }

    /// Width/height of the non-transparent content, or `None` if fully empty.
    fn content_bounds(image: &RenderedTextImage) -> Option<(u32, u32)> {
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut found = false;
        for y in 0..image.height {
            for x in 0..image.width {
                let idx = ((y * image.width + x) * 4 + 3) as usize;
                if image.rgba.get(idx).copied().unwrap_or(0) > 0 {
                    found = true;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        found.then(|| (max_x - min_x + 1, max_y - min_y + 1))
    }

    #[test]
    fn global_rotation_rotates_vertical_block() {
        // Vertical text is tall and narrow; a 90° global rotation turns the whole
        // column block wide and short, at the vector level.
        let mut params = vertical_params("ГРОМ");

        params.global_rotation_deg = 0.0;
        let plain = render(&params);
        let (plain_w, plain_h) = content_bounds(&plain).expect("vertical plain bounds");
        assert!(plain_h > plain_w, "vertical text should be tall: {plain_w}x{plain_h}");

        // 0.0 must be a byte-identical no-op versus the default path.
        assert_eq!(render(&params).rgba, plain.rgba, "0.0 rotation must be a no-op");

        params.global_rotation_deg = 90.0;
        let rotated = render(&params);
        let (rotated_w, rotated_h) = content_bounds(&rotated).expect("vertical rotated bounds");
        assert!(rotated_w > plain_w, "vertical 90°: {rotated_w} !> {plain_w}");
        assert!(rotated_h < plain_h, "vertical 90°: {rotated_h} !< {plain_h}");
    }

    #[test]
    fn vertical_optical_changes_multi_glyph_column_spacing() {
        // A column with several inked glyphs has measurable ink gaps, so optical
        // kerning re-spaces it away from the metric stacking.
        let metric = render(&vertical_params("ГРОМ"));
        let mut optical_params = vertical_params("ГРОМ");
        optical_params.kerning_mode = KerningMode::Optical;
        let optical = render(&optical_params);
        assert_ne!(
            (metric.width, metric.height, metric.rgba),
            (optical.width, optical.height, optical.rgba),
            "optical vertical kerning must change multi-glyph column spacing"
        );
    }

    #[test]
    fn vertical_optical_single_glyph_columns_match_metric() {
        // Each column holds one inked glyph, so there is no adjacent pair to
        // measure: the median has fewer than one finite gap and the column falls
        // back to metric spacing byte-for-byte.
        let metric = render(&vertical_params("Г\nО"));
        let mut optical_params = vertical_params("Г\nО");
        optical_params.kerning_mode = KerningMode::Optical;
        let optical = render(&optical_params);
        assert_eq!(metric.width, optical.width);
        assert_eq!(metric.height, optical.height);
        assert_eq!(metric.rgba, optical.rgba);
    }

    #[test]
    fn vertical_optical_space_reset_renders_valid_image() {
        // A space inside a column resets the optical pair chain (no kern across
        // it). The render must stay valid (unmultiplied `width * height * 4`).
        let mut params = vertical_params("ГГ ГГ");
        params.kerning_mode = KerningMode::Optical;
        let image = render(&params);
        assert_eq!(
            image.rgba.len(),
            image.width as usize * image.height as usize * 4,
            "vertical optical output must be width * height * 4 bytes"
        );
    }
}
