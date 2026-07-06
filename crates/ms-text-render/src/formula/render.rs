/*
File: src/tabs/typing/render_next/formula/render.rs

Purpose:
Formula raster/layout path staged рендера typing.

Main responsibilities:
- рендерить glyph seeds по формульной траектории без зависимости от старого `render.rs`;
- собирать formula-specific glyph metadata, arc-length mapping и rotated bounds/draw;
- отдельно обрабатывать fallback для `TextLayoutMode::Shape`, когда кривая слишком короткая.

Glyph rasterization (formula + custom-line composite pass):
- Each placed glyph is drawn by rasterizing its true font outline
  (`render_next/vector.rs`) directly into the output via
  `glyph_blit::glyph_outline_transform` (the shared single source of truth for the
  outline->world pivot, also used by the horizontal path) + `rasterize_outline_into`.
- COLR/bitmap color glyphs have no monochrome outline (`resolve_glyph_outline`
  returns `None`); those keep the legacy rotated bitmap blit.

Mesh warp (`TextRenderParams.raster_transform`):
Both render functions honor it at the outline seam. Each captures `warp_pre`
(pre-global-rotation content box + centroid = mean of the drawable placement
centers, gated on `raster_transform.is_some()`) BEFORE
`rotate_placements_about_centroid`, then builds `MeshWarpContext` and grows bounds.
Custom VECTOR lines drop their FIXED canvas on a non-identity warp (like a global
rotation) and grow to the warped bounds. `None`/identity is byte-identical; the
color-glyph bitmap fallback is not warped.

Glyph-ink spacing (MinimumPreviousDistance mode):
- `seed_ink_geometry`/`CachedGlyphInk` derive a glyph's ink contour from its outline
  (`glyph_contour_from_outline`, cached by cosmic-text `CacheKey`) plus a horizontal
  ink extent still measured from the rasterized bitmap.
- `drawn_line_transform_at` is the single source of truth for the on-path transform;
  `placed_contour_for_transform` places the outline-frame contour with the same
  `glyph_outline_transform` pivot the rasterizer uses, and
  `find_minimum_ink_distance_center_s` advances the arc-length position until the true
  ink-to-ink gap reaches `target_gap` (kerning-driven).

Source:
- `render_text_with_formula_layout`
- `render_text_with_formula_layout_once`
- `collect_formula_glyph_seeds`
- `assign_formula_seed_advances`
- rotated-rect и arc-length helper'ы
из старого `src/tabs/typing/render.rs`
*/

use ms_log::trace::cat;

use super::{FormulaEvalInput, FormulaProgramBundle};
use crate::drawn_lines::{
    DrawnLinePath, build_vector_line_paths, load_raster_line_paths,
};
use crate::font_registry::InlineFontRegistry;
use crate::glyph_blit::{
    glyph_outline_transform, glyph_subpixel_offset, nominal_glyph_advance_px,
    resolve_outline_for_glyph,
};
use crate::glyph_contour::{
    GlyphContour, PlacedContour, min_placed_distance,
};
use crate::vector::{
    MeshWarpContext, Outline, OutlineCache, RasterScratch, build_aa_lut,
    glyph_contour_from_outline, rasterize_outline_into,
};
use crate::inline_styles::{
    InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
};
use crate::optical::optical_base_advance;
use crate::pipeline::{
    GlyphScaleSettings, KerningSettings, effective_spacing_percent, horizontal_line_offset,
};
use crate::raster::{
    PixelBounds, RigidPlacement, bilinear_sample_rgba, blend_pixel_over, build_glyph_rgba_buffer,
    include_rotated_rect_bounds, rotate_placements_about_centroid, rotated_rect_world_bounds,
    sample_swash_alpha, trim_rendered_image_to_alpha_bounds,
};
use crate::types::{
    HorizontalAlign, KerningMode, RenderedTextImage, TextLayoutMode, TextRenderParams,
    TextVectorLineDistanceMode, TextVectorLineTextDirection,
};
use cosmic_text::{
    Attrs, AttrsOwned, Buffer, CacheKey, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping,
    SwashCache, SwashContent,
};
use std::collections::HashMap;

const SOFT_HYPHEN: char = '\u{00AD}';

/// Douglas-Peucker tolerance (glyph-local pixels) applied when simplifying a
/// glyph's outline-derived contour. Small enough that the polygon still hugs the
/// ink, large enough to keep the edge count (and the O(edges^2) distance test)
/// low.
const CONTOUR_SIMPLIFY_TOLERANCE_PX: f32 = 1.5;

/// Minimum ink-to-ink whitespace (world pixels) enforced between adjacent
/// glyphs in `MinimumPreviousDistance` mode. This is the tunable floor: the
/// natural base gap (derived from advances vs. ink extents) is clamped up to
/// this value so ink shapes never fully touch even where the base gap collapses
/// to zero. Raise it to loosen spacing globally.
const MIN_INK_GAP_FLOOR_PX: f32 = 0.5;

#[derive(Debug)]
pub(crate) enum FormulaRenderOutcome {
    Rendered(RenderedTextImage),
    FallbackToStandard(String),
}

pub(crate) struct FormulaRenderRequest<'a, 'font> {
    pub(crate) params: &'a TextRenderParams,
    pub(crate) font_system: &'font mut FontSystem,
    pub(crate) buffer: &'font mut Buffer,
    pub(crate) attrs: &'a Attrs<'a>,
    pub(crate) inline_style_spans: Option<&'a [InlineStyleSpan]>,
    pub(crate) inline_font_registry: &'a InlineFontRegistry,
    pub(crate) layout_text: &'a str,
    pub(crate) font_size_px: f32,
    pub(crate) base_line_height_px: f32,
    /// Effective perpendicular line-placement fraction in `[-1, 1]`, already
    /// gated by mode in the pipeline router (0.0 for HIDE modes `Shape` /
    /// `CustomRasterLines`, the panel value for `Formula` / `CustomVectorLines`).
    pub(crate) line_placement_frac: f32,
}

#[derive(Debug, Clone)]
struct FormulaGlyphSeed {
    glyph: LayoutGlyph,
    text_color: [u8; 4],
    origin_x: f32,
    origin_y: f32,
    kerning: KerningSettings,
    glyph_scale: GlyphScaleSettings,
    glyph_offset_px: [f32; 2],
    extended_offset: InlineGlyphOffset,
    style_offset: usize,
    offset_span_range: Option<(usize, usize)>,
    line_idx: usize,
    glyph_idx_in_line: usize,
    glyphs_in_line: usize,
    advance_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct FormulaGlyphTransform {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
}

impl RigidPlacement for FormulaGlyphTransform {
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

#[derive(Debug, Clone, Copy)]
struct FormulaArcLengthSample {
    t01: f32,
    arc_len_px: f32,
}

/// On-path placement of a single glyph: the world center of the path point it
/// sits on plus the glyph's total rotation (tangent/static + flip + per-glyph).
/// The blit and the ink-contour placement both derive their world transform
/// from this, guaranteeing they land on the same pixels.
#[derive(Debug, Clone, Copy)]
struct DrawnLineTransform {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
}

impl RigidPlacement for DrawnLineTransform {
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

/// Shift a glyph center perpendicular to its line toward the TOP side.
///
/// `line_frac` in `[-1, 1]`: `0` keeps the center on the line, `+1` rests the
/// glyph ABOVE the line (ink bottom on the line), `-1` BELOW it (ink top on the
/// line). The magnitude is `line_frac * scaled_height / 2`, so each glyph rests
/// naturally on the line using its own scaled ink height.
///
/// Sign convention: the line's DOWN normal (its bottom side — the same direction
/// a positive `normal_offset_px` moves a glyph, `center_y += tangent_x * offset`)
/// is `(-sin, cos)` in screen y-down space. Moving toward the TOP is its
/// negation `(sin, -cos)`, so a positive `line_frac` subtracts along `y` and
/// shifts rendered content UP on screen. Verified by the `line_placement_*` tests.
fn apply_line_placement(
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
    scaled_height: f32,
    line_frac: f32,
) -> (f32, f32) {
    let offset = line_frac * scaled_height * 0.5;
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    (center_x + offset * sin_a, center_y - offset * cos_a)
}

/// World-space center of the glyph bitmap for a given on-path transform.
///
/// This is the single mapping the blit relies on. At `line_frac = 0` the glyph's
/// INK CENTER sits on the path point (`transform.center`); `line_frac` then
/// shifts it perpendicular via [`apply_line_placement`]. Deliberate behavior
/// change from the previous baseline-on-line placement so both line-based modes
/// share one meaning of 0 = center (see `render_next/MODULE_README.md`). Kept
/// free of `FormulaGlyphSeed` so the contour-placement path and unit tests can
/// reuse it.
fn drawn_line_glyph_destination_center_raw(
    transform: &DrawnLineTransform,
    scaled_height: f32,
    line_frac: f32,
) -> (f32, f32) {
    apply_line_placement(
        transform.center_x,
        transform.center_y,
        transform.rotation_rad,
        scaled_height,
        line_frac,
    )
}

/// Resolve a seed glyph's true font outline through the per-render cache.
///
/// Thin wrapper over [`resolve_outline_for_glyph`] keyed on the seed's glyph.
/// Returns `None` when the font is missing or the glyph has no fillable
/// monochrome outline (space or COLR/bitmap color glyph); callers then fall back
/// to the bitmap blit.
fn resolve_glyph_outline(
    seed: &FormulaGlyphSeed,
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
) -> Option<std::sync::Arc<Outline>> {
    resolve_outline_for_glyph(font_system, outline_cache, &seed.glyph)
}

#[derive(Debug, Clone, Copy)]
struct GlyphInkProfile {
    left_px: f32,
    right_px: f32,
}

impl GlyphInkProfile {
    #[must_use]
    fn fallback(width_px: f32, _height_px: f32) -> Self {
        Self {
            left_px: 0.0,
            right_px: width_px.max(1.0),
        }
    }

    #[must_use]
    fn width_px(self) -> f32 {
        (self.right_px - self.left_px).max(1.0)
    }
}

pub(crate) fn render_text_with_formula_layout(
    request: FormulaRenderRequest<'_, '_>,
) -> Result<FormulaRenderOutcome, String> {
    let _span = ms_log::trace_scope!(cat::RENDER, "render_formula_layout mode={:?}", request.params.text_layout_mode);
    let FormulaRenderRequest {
        params,
        font_system,
        buffer,
        attrs,
        inline_style_spans,
        inline_font_registry,
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
    let line_extra_spacing_table = compute_line_extra_spacing_table(
        params,
        layout_text,
        layout_line_offsets.as_slice(),
        inline_style_spans,
        font_size_px,
        default_extra_line_spacing_px,
    );

    if params.text_layout_mode == TextLayoutMode::Shape
        && let Some(warning) = detect_shape_layout_fallback_reason(
            params,
            font_system,
            buffer,
            attrs,
            inline_style_spans,
            inline_font_registry,
            layout_line_offsets.as_slice(),
            font_size_px,
            base_line_height_px,
            line_extra_spacing_table.as_slice(),
        )?
    {
        return Ok(FormulaRenderOutcome::FallbackToStandard(warning));
    }

    let initial_margin_pad = font_size_px.ceil().max(2.0) as u32;
    let mut render_margin_pad = initial_margin_pad;
    let mut last_image = None;

    for _ in 0..4 {
        let image = render_text_with_formula_layout_once(
            params,
            font_system,
            buffer,
            attrs,
            inline_style_spans,
            inline_font_registry,
            layout_line_offsets.as_slice(),
            font_size_px,
            base_line_height_px,
            line_extra_spacing_table.as_slice(),
            render_margin_pad,
            line_placement_frac,
        )?;
        let touches_edge = image_has_alpha_on_edge(&image, render_margin_pad.saturating_sub(1));
        last_image = Some(image);
        if !touches_edge {
            break;
        }
        render_margin_pad = render_margin_pad
            .saturating_mul(2)
            .max(initial_margin_pad + 2);
    }

    let fallback_image = RenderedTextImage::transparent(
        params.width_px.max(1),
        base_line_height_px.ceil().max(1.0) as u32,
    );
    Ok(FormulaRenderOutcome::Rendered(
        trim_rendered_image_to_alpha_bounds(last_image.unwrap_or(fallback_image), 1),
    ))
}

pub(crate) fn render_text_with_drawn_lines_layout(
    request: FormulaRenderRequest<'_, '_>,
) -> Result<FormulaRenderOutcome, String> {
    let _span = ms_log::trace_scope!(cat::RENDER, "render_drawn_lines_layout");
    let Some(layout_path) = request.params.drawn_lines_layout.image_path.as_deref() else {
        return Ok(FormulaRenderOutcome::FallbackToStandard(
            "Для раскладки по рисованным линиям не задано layout-изображение.".to_string(),
        ));
    };
    if !layout_path.is_file() {
        return Ok(FormulaRenderOutcome::FallbackToStandard(format!(
            "Layout-изображение для рисованных линий не найдено: {}",
            layout_path.display()
        )));
    }
    let paths = load_raster_line_paths(layout_path, &request.params.drawn_lines_layout)?;
    if paths.iter().all(Option::is_none) {
        return Ok(FormulaRenderOutcome::FallbackToStandard(format!(
            "В layout-изображении {} не найдены рисованные линии.",
            layout_path.display()
        )));
    }

    render_text_with_drawn_lines_layout_once(request, paths.as_slice(), None).map(|rendered| {
        FormulaRenderOutcome::Rendered(trim_rendered_image_to_alpha_bounds(rendered, 1))
    })
}

pub(crate) fn render_text_with_vector_lines_layout(
    request: FormulaRenderRequest<'_, '_>,
) -> Result<FormulaRenderOutcome, String> {
    let _span = ms_log::trace_scope!(cat::RENDER, "render_vector_lines_layout");
    let paths = build_vector_line_paths(&request.params.vector_lines_layout);
    if paths.iter().all(Option::is_none) {
        return Ok(FormulaRenderOutcome::FallbackToStandard(
            "Для векторной кастомной раскладки не заданы линии.".to_string(),
        ));
    }

    let fixed_size = Some((
        request.params.vector_lines_layout.width_px.max(1),
        request.params.vector_lines_layout.height_px.max(1),
    ));
    render_text_with_drawn_lines_layout_once(request, paths.as_slice(), fixed_size)
        .map(FormulaRenderOutcome::Rendered)
}

fn render_text_with_drawn_lines_layout_once(
    request: FormulaRenderRequest<'_, '_>,
    paths: &[Option<DrawnLinePath>],
    fixed_output_size: Option<(u32, u32)>,
) -> Result<RenderedTextImage, String> {
    let FormulaRenderRequest {
        params,
        font_system,
        buffer,
        attrs,
        inline_style_spans,
        inline_font_registry,
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
    let line_extra_spacing_table = compute_line_extra_spacing_table(
        params,
        layout_text,
        layout_line_offsets.as_slice(),
        inline_style_spans,
        font_size_px,
        default_extra_line_spacing_px,
    );
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table.as_slice(),
        has_inline_size_overrides,
    );
    let mut seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        buffer,
        attrs,
        inline_style_spans,
        inline_font_registry,
        layout_line_offsets.as_slice(),
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            params.width_px.max(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // The swash cache and the glyph-contour cache both live for the whole
    // render so glyph rasterization and ink-contour tracing happen at most once
    // per distinct glyph, and are reused by the bounds and composite passes.
    let mut cache = SwashCache::new();
    let mut contour_cache: HashMap<CacheKey, CachedGlyphInk> = HashMap::new();
    // Resolution-independent glyph outlines: shared by the ink-distance search
    // (via the transforms below) and the composite pass so each glyph is
    // extracted at most once per render.
    let mut outline_cache = OutlineCache::new();
    // Reused per-glyph rasterizer buffers for the composite pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    let mut transforms = build_drawn_line_transforms(
        params,
        seeds.as_slice(),
        paths,
        font_system,
        &mut cache,
        &mut contour_cache,
        &mut outline_cache,
        line_placement_frac,
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
            let (scaled_left, scaled_top, scaled_width, scaled_height) = seed
                .glyph_scale
                .scaled_rect(src_left as f32, src_top as f32, glyph_w as f32, glyph_h as f32);
            let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
                transform,
                scaled_height,
                line_placement_frac,
            );
            include_rotated_rect_bounds(
                &mut pre_box,
                scaled_left,
                scaled_top,
                scaled_width,
                scaled_height,
                dst_center_x,
                dst_center_y,
                transform.rotation_rad,
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
        let (scaled_left, scaled_top, scaled_width, scaled_height) = seed.glyph_scale.scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
        );
        let (dst_center_x, dst_center_y) =
            drawn_line_glyph_destination_center_raw(transform, scaled_height, line_placement_frac);
        include_rotated_rect_bounds(
            &mut bounds,
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            dst_center_x,
            dst_center_y,
            transform.rotation_rad,
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
        let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (dst_center_x, dst_center_y) =
            drawn_line_glyph_destination_center_raw(&transform, scaled_height, line_placement_frac);

        // Prefer the true font outline: rasterize it directly into the output at
        // the exact world placement the bitmap blit would have used. Color/emoji
        // glyphs have no monochrome outline and keep the bitmap blit below.
        if let Some(outline) = resolve_glyph_outline(&seed, font_system, &mut outline_cache) {
            let glyph_transform = glyph_outline_transform(
                dst_center_x,
                dst_center_y,
                transform.rotation_rad,
                placement_left,
                placement_top,
                glyph_w as f32,
                glyph_h as f32,
                seed.glyph_scale.width_mul,
                seed.glyph_scale.height_mul,
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
                &glyph_transform,
                seed.text_color,
                &aa_lut,
                warp_ctx.as_ref(),
            );
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
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            dst_center_x,
            dst_center_y,
            transform.rotation_rad,
        );
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

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings,
        content_origin_x: 0,
        content_origin_y: 0,
    })
}

/// Compute the per-glyph on-path transforms for every seed of a custom line
/// layout.
///
/// `font_system`/`cache`/`contour_cache`/`outline_cache` are only touched for
/// lines that use `MinimumPreviousDistance` spacing, which needs the glyph's ink
/// contour (derived from its outline); `ByLineLength` lines never extract here.
/// The returned vector is index-aligned with `seeds`; `None` means the glyph
/// could not be placed (past the end of its line path or missing sample) and is
/// dropped by callers.
// The placement context needs all four immutable/mutable dependencies; bundling
// the three caches would not simplify the call site.
#[allow(clippy::too_many_arguments)]
fn build_drawn_line_transforms(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    contour_cache: &mut HashMap<CacheKey, CachedGlyphInk>,
    outline_cache: &mut OutlineCache,
    line_placement_frac: f32,
) -> Vec<Option<DrawnLineTransform>> {
    let mut line_offsets = HashMap::<usize, DrawnLinePlacementState>::new();
    let layout_settings = custom_line_layout_settings(params, line_placement_frac);
    let mut ctx = DrawnLinePlacementCtx {
        params,
        seeds,
        layout: layout_settings,
        letter_spacing_mul: layout_settings.letter_spacing_mul.clamp(0.0, 8.0),
        letter_spacing_px: layout_settings.letter_spacing_px.clamp(-10_000.0, 10_000.0),
        font_system,
        cache,
        contour_cache,
        outline_cache,
    };
    let mut transforms: Vec<Option<DrawnLineTransform>> = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let Some(path) = paths.get(seed.line_idx).and_then(Option::as_ref) else {
            transforms.push(None);
            continue;
        };
        let state = line_offsets.entry(seed.line_idx).or_default();
        transforms.push(drawn_line_seed_transform(&mut ctx, seed, path, state));
    }
    apply_drawn_line_group_rotations(seeds, transforms.as_mut_slice());
    transforms
}

/// Stable placement context shared across all seeds of a custom line layout.
///
/// Bundles the immutable layout parameters with the mutable rasterization
/// caches so per-seed placement helpers take a small argument list instead of a
/// long positional one.
struct DrawnLinePlacementCtx<'a> {
    params: &'a TextRenderParams,
    seeds: &'a [FormulaGlyphSeed],
    layout: CustomLineLayoutSettings,
    /// `letter_spacing_mul` clamped to the ByLineLength range.
    letter_spacing_mul: f32,
    /// `letter_spacing_px` clamped to the ByLineLength range.
    letter_spacing_px: f32,
    font_system: &'a mut FontSystem,
    cache: &'a mut SwashCache,
    contour_cache: &'a mut HashMap<CacheKey, CachedGlyphInk>,
    outline_cache: &'a mut OutlineCache,
}

/// Place one glyph seed along its line path and advance the line's state.
///
/// For `ByLineLength` spacing this reproduces the original arc-length walk
/// unchanged. For `MinimumPreviousDistance` it seeds the same arc-length
/// position, then searches forward so the true minimum ink-to-ink distance to
/// the previous glyph reaches the kerning-driven `target_gap`. Returns the
/// blit-ready transform, or `None` when the glyph runs past the path end.
fn drawn_line_seed_transform(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    state: &mut DrawnLinePlacementState,
) -> Option<DrawnLineTransform> {
    let params = ctx.params;
    let layout = ctx.layout;
    let advance =
        ((seed.advance_px.max(1.0) * ctx.letter_spacing_mul) + ctx.letter_spacing_px).max(1.0);
    let half_advance = advance * 0.5;
    // Arc-length seed: identical to ByLineLength placement.
    let mut center_s = state.offset_s_px + half_advance + seed.extended_offset.line_px;
    if center_s > path.total_len_px {
        return None;
    }

    let use_ink = vector_line_distance_mode(params, seed.line_idx)
        == TextVectorLineDistanceMode::MinimumPreviousDistance;
    // Only the ink-distance mode needs the glyph's rasterized contour.
    let geom = if use_ink {
        seed_ink_geometry(
            seed,
            ctx.font_system,
            ctx.cache,
            ctx.contour_cache,
            ctx.outline_cache,
        )
    } else {
        None
    };

    if use_ink
        && let (Some(prev), Some(g)) = (state.previous_contour.as_ref(), geom.as_ref())
    {
        // Natural base gap: the center-to-center arc advance (which already
        // folds in letter-spacing via `advance` and kerning via `advance_px`)
        // minus the two facing ink half-extents. Clamped up to the floor so the
        // shapes never touch. Because kerning/letter-spacing are baked into
        // `center_distance`, they tighten/loosen this gap without being
        // re-applied (which would double-count them).
        let this_ink_half = g.ink_width_px * seed.glyph_scale.width_mul * 0.5;
        let center_distance = state.previous_half_advance_px + half_advance;
        let target_gap = (center_distance - (state.previous_ink_half_px + this_ink_half))
            .max(MIN_INK_GAP_FLOOR_PX);
        center_s = find_minimum_ink_distance_center_s(path, center_s, target_gap, prev, |s| {
            place_seed_contour_at(params, seed, g, path, s, &layout)
        })?;
    }

    if center_s > path.total_len_px {
        return None;
    }
    state.offset_s_px = center_s + half_advance - seed.extended_offset.line_px;
    let transform = drawn_line_transform_at(params, seed, path, center_s, &layout)?;

    if use_ink {
        state.previous_half_advance_px = half_advance;
        match geom.as_ref() {
            Some(g) => {
                // Store the current glyph placed at its FINAL center so the next
                // glyph measures against the same contour the blit will draw.
                state.previous_contour = Some(placed_contour_for_transform(
                    &g.contour,
                    g.placement_left,
                    g.placement_top,
                    g.glyph_w,
                    g.glyph_h,
                    seed.glyph_scale.width_mul,
                    seed.glyph_scale.height_mul,
                    g.scaled_height,
                    layout.line_placement_frac,
                    g.subpixel,
                    &transform,
                ));
                state.previous_ink_half_px = g.ink_width_px * seed.glyph_scale.width_mul * 0.5;
            }
            None => {
                // Empty/space glyph (no ink): reset so the next glyph falls back
                // to plain arc-length spacing instead of chaining a stale shape.
                state.previous_contour = None;
                state.previous_ink_half_px = 0.0;
            }
        }
    }

    if seed.extended_offset.shift_following && is_last_seed_in_offset_span_on_line(ctx.seeds, seed) {
        state.offset_s_px += seed.extended_offset.line_px;
    }
    Some(transform)
}

/// On-path glyph placement state carried between seeds of one line.
///
/// `previous_contour` is the previous glyph's ink placed at its final center,
/// used by `MinimumPreviousDistance` to measure the true ink-to-ink gap.
/// `previous_half_advance_px` and `previous_ink_half_px` feed the natural base
/// gap; all three are unused (and stay at their defaults) for `ByLineLength`.
#[derive(Debug, Default)]
struct DrawnLinePlacementState {
    offset_s_px: f32,
    previous_contour: Option<PlacedContour>,
    previous_half_advance_px: f32,
    /// World-space half-width of the previous glyph's ink (scaled by width_mul).
    previous_ink_half_px: f32,
}

/// Cached ink data for one glyph key.
///
/// Keyed by the full cosmic-text [`CacheKey`] (subpixel bins included) so the
/// cached data always matches the exact bitmap the blit references; the extra
/// entries per glyph (one per subpixel bin actually used) are few and cheap.
/// The contour lives in the outline's pen-relative y-down pixel frame; the
/// bitmap `placement_left`/`placement_top` are stored so the contour can be
/// placed with the same pivot the outline rasterizer uses.
#[derive(Debug, Clone)]
struct CachedGlyphInk {
    /// Closed outer contour(s) of the glyph ink in the outline (pen-relative,
    /// y-down px) frame, unscaled/unrotated.
    contour: GlyphContour,
    /// Glyph bitmap width in pixels.
    glyph_w: f32,
    /// Glyph bitmap height in pixels.
    glyph_h: f32,
    /// Bitmap x placement (pen-relative left edge).
    placement_left: f32,
    /// Bitmap y placement (pen-relative top above baseline).
    placement_top: f32,
    /// Horizontal ink extent (right - left) in glyph pixels.
    ink_width_px: f32,
}

/// Per-seed ink geometry: the cached glyph contour plus the seed-specific
/// scaled vertical placement needed to map the contour into world space.
#[derive(Debug, Clone)]
struct SeedInkGeometry {
    /// Cached ink contour in the outline (pen-relative, y-down px) frame.
    contour: GlyphContour,
    /// Glyph bitmap width in pixels (pivot input).
    glyph_w: f32,
    /// Glyph bitmap height in pixels (pivot input).
    glyph_h: f32,
    /// Bitmap x placement (pen-relative left edge, pivot input).
    placement_left: f32,
    /// Bitmap y placement (pen-relative top above baseline, pivot input).
    placement_top: f32,
    /// Height of the scaled glyph rect in content coordinates (line-placement basis).
    scaled_height: f32,
    /// Horizontal ink extent (right - left) in glyph pixels, used for the gap target.
    ink_width_px: f32,
    /// Subpixel fraction (`[x_bin, y_bin]`, device px) baked into the bitmap
    /// coverage; folded into the outline pivot so the measured contour lands on
    /// the same pixels as the drawn outline.
    subpixel: [f32; 2],
}

/// Build (or reuse) the ink geometry for a seed's glyph.
///
/// The ink contour is derived from the glyph's true font outline (cached in
/// `contour_cache` on a miss); the horizontal ink extent is still measured from
/// the rasterized bitmap so the min-distance base gap matches the pixels drawn.
/// Returns `None` for glyphs with no bitmap (e.g. spaces), zero-size placement,
/// or no monochrome outline (color glyphs), which callers treat as "no ink" and
/// handle with the arc-length fallback.
fn seed_ink_geometry(
    seed: &FormulaGlyphSeed,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    contour_cache: &mut HashMap<CacheKey, CachedGlyphInk>,
    outline_cache: &mut OutlineCache,
) -> Option<SeedInkGeometry> {
    let physical = seed.glyph.physical(
        (
            seed.origin_x + seed.glyph_offset_px[0],
            seed.origin_y + seed.glyph_offset_px[1],
        ),
        1.0,
    );
    let key = physical.cache_key;

    // Bitmap placement + horizontal ink extent, copied out before the image
    // borrow of `cache` ends.
    let placement_left;
    let placement_top;
    let glyph_w;
    let glyph_h;
    let ink_width_px;
    {
        let Some(image) = cache.get_image(font_system, key) else {
            return None;
        };
        let gw = image.placement.width as usize;
        let gh = image.placement.height as usize;
        if gw == 0 || gh == 0 {
            return None;
        }
        placement_left = image.placement.left as f32;
        placement_top = image.placement.top as f32;
        glyph_w = gw as f32;
        glyph_h = gh as f32;
        let ink = glyph_ink_profile_from_image(
            [
                image.placement.left as f32,
                (physical.y - image.placement.top) as f32,
            ],
            &image.content,
            image.data.as_slice(),
            [gw, gh],
            [seed.glyph.w.max(1.0), 1.0],
        );
        ink_width_px = (ink.right_px - ink.left_px).max(0.0);
    }

    // Trace the ink contour from the true outline once per distinct glyph key.
    // A glyph with no fillable monochrome outline is treated as "no ink".
    if let std::collections::hash_map::Entry::Vacant(entry) = contour_cache.entry(key) {
        let outline = resolve_glyph_outline(seed, font_system, outline_cache)?;
        let contour = glyph_contour_from_outline(&outline, CONTOUR_SIMPLIFY_TOLERANCE_PX);
        entry.insert(CachedGlyphInk {
            contour,
            glyph_w,
            glyph_h,
            placement_left,
            placement_top,
            ink_width_px,
        });
    }

    let cached = contour_cache.get(&key)?;
    let src_left = physical.x as f32 + placement_left;
    let src_top = physical.y as f32 - placement_top;
    let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
        seed.glyph_scale
            .scaled_rect(src_left, src_top, glyph_w, glyph_h);
    Some(SeedInkGeometry {
        contour: cached.contour.clone(),
        glyph_w: cached.glyph_w,
        glyph_h: cached.glyph_h,
        placement_left: cached.placement_left,
        placement_top: cached.placement_top,
        scaled_height,
        ink_width_px: cached.ink_width_px,
        subpixel: glyph_subpixel_offset(key),
    })
}

/// On-path transform (position + rotation) for a candidate arc-length position.
///
/// This is the single source of truth for glyph placement along a custom line:
/// both the final blit transform and every trial contour placement during the
/// ink-distance search go through it. Applies the normal offset, tangent/static
/// rotation, flip, per-glyph rotation, and the rotated glyph offset exactly as
/// the composite pass expects. Returns `None` if the path cannot be sampled.
fn drawn_line_transform_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<DrawnLineTransform> {
    let (center_x, center_y, tangent_x, tangent_y) =
        sample_drawn_line_path_for_direction(path, center_s)?;
    let tangent_len = (tangent_x * tangent_x + tangent_y * tangent_y)
        .sqrt()
        .max(1e-6);
    let tangent_x = tangent_x / tangent_len;
    let tangent_y = tangent_y / tangent_len;
    let normal_offset = layout.normal_offset_px;
    let center_x = center_x - tangent_y * normal_offset;
    let center_y = center_y + tangent_x * normal_offset;
    let rotation_rad = (if layout.use_tangent_rotation {
        tangent_y.atan2(tangent_x)
    } else {
        layout.static_rotation_rad
    }) + vector_line_flip_rotation(params, seed.line_idx)
        + seed.extended_offset.glyph_rotation_rad;
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let center_x = center_x + seed.glyph_offset_px[0] * cos_a - seed.glyph_offset_px[1] * sin_a;
    let center_y = center_y + seed.glyph_offset_px[0] * sin_a + seed.glyph_offset_px[1] * cos_a;
    Some(DrawnLineTransform {
        center_x,
        center_y,
        rotation_rad,
    })
}

/// Place a glyph's cached contour into world space for a candidate position.
///
/// Combines [`drawn_line_transform_at`] with [`placed_contour_for_transform`]
/// so the search closure produces the exact `PlacedContour` the blit would draw
/// at `center_s`. Returns `None` when the path cannot be sampled.
fn place_seed_contour_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<PlacedContour> {
    let transform = drawn_line_transform_at(params, seed, path, center_s, layout)?;
    Some(placed_contour_for_transform(
        &geom.contour,
        geom.placement_left,
        geom.placement_top,
        geom.glyph_w,
        geom.glyph_h,
        seed.glyph_scale.width_mul,
        seed.glyph_scale.height_mul,
        geom.scaled_height,
        layout.line_placement_frac,
        geom.subpixel,
        &transform,
    ))
}

/// Map an outline-frame contour into world space using the blit's exact
/// geometry.
///
/// The contour lives in the outline's pen-relative y-down pixel frame (the same
/// frame the outline rasterizer consumes), so this resolves the glyph's world
/// destination center from the on-path transform and reuses
/// [`glyph_outline_transform`] — the single source of truth for the pivot — so
/// the measured contour lands on the exact pixels the outline is rasterized to.
// The geometry is an irreducible list of independent scalars (bitmap
// placement/size, per-axis scale, scaled ink height, line placement, transform);
// bundling them into a one-off struct would not add clarity.
#[allow(clippy::too_many_arguments)]
fn placed_contour_for_transform(
    contour: &GlyphContour,
    placement_left: f32,
    placement_top: f32,
    glyph_w: f32,
    glyph_h: f32,
    width_mul: f32,
    height_mul: f32,
    scaled_height: f32,
    line_frac: f32,
    subpixel: [f32; 2],
    transform: &DrawnLineTransform,
) -> PlacedContour {
    let (dst_center_x, dst_center_y) =
        drawn_line_glyph_destination_center_raw(transform, scaled_height, line_frac);
    // Same subpixel-corrected pivot the outline rasterizer uses, so the measured
    // contour matches the drawn ink exactly.
    let glyph_transform = glyph_outline_transform(
        dst_center_x,
        dst_center_y,
        transform.rotation_rad,
        placement_left,
        placement_top,
        glyph_w,
        glyph_h,
        width_mul,
        height_mul,
        subpixel,
    );
    glyph_transform.place_contour(contour)
}

#[derive(Debug, Clone, Copy)]
struct CustomLineLayoutSettings {
    use_tangent_rotation: bool,
    static_rotation_rad: f32,
    normal_offset_px: f32,
    letter_spacing_mul: f32,
    letter_spacing_px: f32,
    /// Effective perpendicular line-placement fraction in `[-1, 1]` (already
    /// mode-gated by the router). Shared by the ink-distance search so the
    /// measured contour lands on the same shifted pixels the blit draws.
    line_placement_frac: f32,
}

fn custom_line_layout_settings(
    params: &TextRenderParams,
    line_placement_frac: f32,
) -> CustomLineLayoutSettings {
    match params.text_layout_mode {
        TextLayoutMode::CustomRasterLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.drawn_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.drawn_lines_layout.static_rotation_rad,
            normal_offset_px: params.drawn_lines_layout.normal_offset_px,
            letter_spacing_mul: params.drawn_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.drawn_lines_layout.letter_spacing_px,
            line_placement_frac,
        },
        TextLayoutMode::CustomVectorLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.vector_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.vector_lines_layout.static_rotation_rad,
            normal_offset_px: params.vector_lines_layout.normal_offset_px,
            letter_spacing_mul: params.vector_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.vector_lines_layout.letter_spacing_px,
            line_placement_frac,
        },
        TextLayoutMode::Normal | TextLayoutMode::Formula | TextLayoutMode::Shape => {
            CustomLineLayoutSettings {
                use_tangent_rotation: true,
                static_rotation_rad: 0.0,
                normal_offset_px: 0.0,
                letter_spacing_mul: 1.0,
                letter_spacing_px: 0.0,
                line_placement_frac,
            }
        }
    }
}

fn vector_line_distance_mode(
    params: &TextRenderParams,
    line_idx: usize,
) -> TextVectorLineDistanceMode {
    if params.text_layout_mode != TextLayoutMode::CustomVectorLines {
        return TextVectorLineDistanceMode::ByLineLength;
    }
    params
        .vector_lines_layout
        .lines
        .get(line_idx)
        .map(|line| line.distance_mode)
        .unwrap_or(TextVectorLineDistanceMode::ByLineLength)
}

fn vector_line_flip_rotation(params: &TextRenderParams, line_idx: usize) -> f32 {
    if params.text_layout_mode != TextLayoutMode::CustomVectorLines {
        return 0.0;
    }
    if params
        .vector_lines_layout
        .lines
        .get(line_idx)
        .is_some_and(|line| line.flip_text)
    {
        std::f32::consts::PI
    } else {
        0.0
    }
}

fn is_last_seed_in_offset_span_on_line(
    seeds: &[FormulaGlyphSeed],
    seed: &FormulaGlyphSeed,
) -> bool {
    let Some(span_range) = seed.offset_span_range else {
        return true;
    };
    !seeds.iter().any(|other| {
        other.line_idx == seed.line_idx
            && other.style_offset > seed.style_offset
            && other.offset_span_range == Some(span_range)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OffsetGroupKey {
    line_idx: usize,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct OffsetGroupRotation {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
    count: usize,
}

fn offset_group_key(seed: &FormulaGlyphSeed) -> Option<OffsetGroupKey> {
    let (start, end) = seed.offset_span_range?;
    Some(OffsetGroupKey {
        line_idx: seed.line_idx,
        start,
        end,
    })
}

fn apply_formula_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &mut [FormulaGlyphTransform],
) {
    let groups = formula_group_rotations(seeds, transforms);
    for (seed, transform) in seeds.iter().zip(transforms.iter_mut()) {
        let Some(group) = offset_group_key(seed).and_then(|key| groups.get(&key).copied()) else {
            continue;
        };
        if group.count <= 1 || group.rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let (x, y) = rotate_point_around(
            transform.center_x,
            transform.center_y,
            group.center_x,
            group.center_y,
            group.rotation_rad,
        );
        transform.center_x = x;
        transform.center_y = y;
        transform.rotation_rad += group.rotation_rad;
    }
}

fn formula_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &[FormulaGlyphTransform],
) -> HashMap<OffsetGroupKey, OffsetGroupRotation> {
    let mut groups = HashMap::<OffsetGroupKey, OffsetGroupRotation>::new();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        if seed.extended_offset.group_rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let Some(key) = offset_group_key(seed) else {
            continue;
        };
        let entry = groups.entry(key).or_insert(OffsetGroupRotation {
            center_x: 0.0,
            center_y: 0.0,
            rotation_rad: seed.extended_offset.group_rotation_rad,
            count: 0,
        });
        entry.center_x += transform.center_x;
        entry.center_y += transform.center_y;
        entry.count += 1;
    }
    for group in groups.values_mut() {
        let count = group.count.max(1) as f32;
        group.center_x /= count;
        group.center_y /= count;
    }
    groups
}

fn apply_drawn_line_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &mut [Option<DrawnLineTransform>],
) {
    let groups = drawn_line_group_rotations(seeds, transforms);
    for (seed, transform) in seeds.iter().zip(transforms.iter_mut()) {
        let Some(transform) = transform else {
            continue;
        };
        let Some(group) = offset_group_key(seed).and_then(|key| groups.get(&key).copied()) else {
            continue;
        };
        if group.count <= 1 || group.rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let (x, y) = rotate_point_around(
            transform.center_x,
            transform.center_y,
            group.center_x,
            group.center_y,
            group.rotation_rad,
        );
        transform.center_x = x;
        transform.center_y = y;
        transform.rotation_rad += group.rotation_rad;
    }
}

fn drawn_line_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &[Option<DrawnLineTransform>],
) -> HashMap<OffsetGroupKey, OffsetGroupRotation> {
    let mut groups = HashMap::<OffsetGroupKey, OffsetGroupRotation>::new();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        if seed.extended_offset.group_rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let Some(transform) = transform else {
            continue;
        };
        let Some(key) = offset_group_key(seed) else {
            continue;
        };
        let entry = groups.entry(key).or_insert(OffsetGroupRotation {
            center_x: 0.0,
            center_y: 0.0,
            rotation_rad: seed.extended_offset.group_rotation_rad,
            count: 0,
        });
        entry.center_x += transform.center_x;
        entry.center_y += transform.center_y;
        entry.count += 1;
    }
    for group in groups.values_mut() {
        let count = group.count.max(1) as f32;
        group.center_x /= count;
        group.center_y /= count;
    }
    groups
}

fn rotate_point_around(
    x: f32,
    y: f32,
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
) -> (f32, f32) {
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let rel_x = x - center_x;
    let rel_y = y - center_y;
    (
        center_x + rel_x * cos_a - rel_y * sin_a,
        center_y + rel_x * sin_a + rel_y * cos_a,
    )
}

fn sample_drawn_line_path_for_direction(
    path: &DrawnLinePath,
    target_s: f32,
) -> Option<(f32, f32, f32, f32)> {
    let forward = line_path_forward_for_direction(path);
    let sample_s = if forward {
        target_s
    } else {
        path.total_len_px - target_s
    };
    let (x, y, tangent_x, tangent_y) = sample_drawn_line_path(path, sample_s)?;
    if forward {
        Some((x, y, tangent_x, tangent_y))
    } else {
        Some((x, y, -tangent_x, -tangent_y))
    }
}

fn line_path_forward_for_direction(path: &DrawnLinePath) -> bool {
    if !path.honor_text_direction {
        return true;
    }
    let Some(first) = path.points.first() else {
        return true;
    };
    let Some(last) = path.points.last() else {
        return true;
    };
    let dx = last.x - first.x;
    match path.direction {
        TextVectorLineTextDirection::LeftToRight => dx >= 0.0,
        TextVectorLineTextDirection::RightToLeft => dx < 0.0,
    }
}

/// Find the smallest `center_s >= start_s` whose placed contour clears the
/// previous glyph's ink by at least `target_gap`.
///
/// `current_at(s)` must return the current glyph's `PlacedContour` at candidate
/// arc-length `s` (or `None` when `s` cannot be sampled). The predicate is
/// `min_placed_distance(prev_placed, current) >= target_gap`; note that
/// `min_placed_distance` yields `0.0` on overlap (so a concave inner corner is
/// correctly pushed forward) and `f32::INFINITY` when either contour is empty
/// (so an empty previous or current glyph satisfies the predicate immediately
/// and falls back to the plain arc-length seed).
///
/// Robust structure: early-out if the seed already clears the gap; otherwise a
/// coarse forward scan brackets the first passing position, refined by fixed
/// bisection. Returns `None` if no position up to the path end satisfies the
/// gap, so the glyph is dropped like the arc-length walk would drop it.
fn find_minimum_ink_distance_center_s<F>(
    path: &DrawnLinePath,
    start_s: f32,
    target_gap: f32,
    prev_placed: &PlacedContour,
    mut current_at: F,
) -> Option<f32>
where
    F: FnMut(f32) -> Option<PlacedContour>,
{
    let clears = |placed: &PlacedContour| min_placed_distance(prev_placed, placed) >= target_gap;

    if clears(&current_at(start_s)?) {
        return Some(start_s);
    }

    // Step size scales with the target so wide gaps scan coarsely and tight
    // gaps finely; clamped to keep the scan bounded on any path length.
    let scan_step = (target_gap.max(1.0) / 8.0).clamp(0.5, 4.0);
    let mut low = start_s;
    let mut high = (start_s + scan_step).min(path.total_len_px);
    loop {
        if clears(&current_at(high)?) {
            break;
        }
        if high >= path.total_len_px {
            // Ran off the end without ever clearing the gap: drop the glyph.
            return None;
        }
        low = high;
        high = (high + scan_step).min(path.total_len_px);
    }

    for _ in 0..18 {
        let mid = low + (high - low) * 0.5;
        if clears(&current_at(mid)?) {
            high = mid;
        } else {
            low = mid;
        }
    }
    Some(high)
}

fn sample_drawn_line_path(path: &DrawnLinePath, target_s: f32) -> Option<(f32, f32, f32, f32)> {
    let points = path.points.as_slice();
    let first = points.first().copied()?;
    if target_s <= 0.0 || points.len() == 1 {
        let next = points.get(1).copied().unwrap_or(first);
        return Some((first.x, first.y, next.x - first.x, next.y - first.y));
    }
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if target_s > b.arc_len_px {
            continue;
        }
        let segment_len = (b.arc_len_px - a.arc_len_px).max(1e-6);
        let t = ((target_s - a.arc_len_px) / segment_len).clamp(0.0, 1.0);
        return Some((
            a.x + (b.x - a.x) * t,
            a.y + (b.y - a.y) * t,
            b.x - a.x,
            b.y - a.y,
        ));
    }
    let last = points.last().copied()?;
    let prev = points
        .get(points.len().saturating_sub(2))
        .copied()
        .unwrap_or(last);
    Some((last.x, last.y, last.x - prev.x, last.y - prev.y))
}

// Formula rendering needs explicit access to the shaped buffer, inline spans and raster bounds.
#[allow(clippy::too_many_arguments)]
fn render_text_with_formula_layout_once(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_extra_spacing_table: &[f32],
    render_margin_pad: u32,
    line_placement_frac: f32,
) -> Result<RenderedTextImage, String> {
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
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let default_extra_line_spacing_px = line_extra_spacing_table.first().copied().unwrap_or(0.0);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table,
        has_inline_size_overrides,
    );
    let mut seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        buffer,
        attrs,
        inline_style_spans,
        inline_font_registry,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let letter_spacing_mul = params.formula_layout.letter_spacing_mul.clamp(0.0, 8.0);
    let letter_spacing_px = params
        .formula_layout
        .letter_spacing_px
        .clamp(-10_000.0, 10_000.0);
    let default_advance = (font_size_px * 0.5).max(1.0);
    let mut centers = Vec::<f32>::with_capacity(seeds.len());
    let mut total_advance = 0.0f32;
    for seed in &seeds {
        let advance = ((seed.advance_px.max(default_advance) * letter_spacing_mul)
            + letter_spacing_px)
            .max(1.0);
        centers.push(total_advance + advance * 0.5);
        total_advance += advance;
    }
    let total_advance = total_advance.max(1.0);
    let glyph_count = seeds.len();

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
        let target_arc_len_px =
            map_formula_target_arc_length(center_s, total_advance, curve_len_px);
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
            let (scaled_left, scaled_top, scaled_width, scaled_height) =
                seed.glyph_scale.scaled_rect(
                    src_left as f32,
                    src_top as f32,
                    glyph_w as f32,
                    glyph_h as f32,
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
                scaled_left,
                scaled_top,
                scaled_width,
                scaled_height,
                placed_center_x,
                placed_center_y,
                transform.rotation_rad,
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
        let (scaled_left, scaled_top, scaled_width, scaled_height) = seed.glyph_scale.scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
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
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            placed_center_x,
            placed_center_y,
            transform.rotation_rad,
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

        // Prefer the true font outline; keep the bitmap blit for outline-less
        // glyphs. The (line-placed) transform center is the glyph bitmap center in
        // world space, so it is the outline destination center directly.
        if let Some(outline) = resolve_glyph_outline(&seed, font_system, &mut outline_cache) {
            let glyph_transform = glyph_outline_transform(
                placed_center_x,
                placed_center_y,
                transform.rotation_rad,
                placement_left,
                placement_top,
                glyph_w as f32,
                glyph_h as f32,
                seed.glyph_scale.width_mul,
                seed.glyph_scale.height_mul,
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
                &glyph_transform,
                seed.text_color,
                &aa_lut,
                warp_ctx.as_ref(),
            );
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
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            placed_center_x,
            placed_center_y,
            transform.rotation_rad,
        );
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

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
    })
}

// Shape fallback depends on shaped glyph metrics, formula arc length and inline-size-aware baselines.
#[allow(clippy::too_many_arguments)]
fn detect_shape_layout_fallback_reason(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_extra_spacing_table: &[f32],
) -> Result<Option<String>, String> {
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let default_extra_line_spacing_px = line_extra_spacing_table.first().copied().unwrap_or(0.0);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table,
        has_inline_size_overrides,
    );
    let seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        buffer,
        attrs,
        inline_style_spans,
        inline_font_registry,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
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

// Formula seed collection needs the shaped buffer, inline spans and soft-hyphen reconstruction.
#[allow(clippy::too_many_arguments)]
fn collect_formula_glyph_seeds(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_baselines: &[f32],
) -> Vec<FormulaGlyphSeed> {
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
    while let Some(run) = runs.next() {
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
        for glyph in run.glyphs {
            let glyph_idx_in_line = line_seen
                .get_mut(line_idx)
                .map(|value| {
                    let idx = *value;
                    *value += 1;
                    idx
                })
                .unwrap_or(0);
            out.push(FormulaGlyphSeed {
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
                kerning: inline_kerning_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
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
                advance_px: 0.0,
            });
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(mut hyphen_glyph) = build_wrapped_hyphen_glyph(
                font_system,
                attrs,
                inline_style_spans,
                inline_font_registry,
                layout_line_offsets,
                &run,
                runs.peek(),
                font_size_px,
                base_line_height_px,
            )
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
            out.push(FormulaGlyphSeed {
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
                advance_px: 0.0,
            });
        }
        line_idx += 1;
    }

    assign_formula_seed_advances(
        out.as_mut_slice(),
        font_system,
        font_size_px,
        (font_size_px * 0.5).max(1.0),
    );
    out
}

fn assign_formula_seed_advances(
    seeds: &mut [FormulaGlyphSeed],
    font_system: &mut FontSystem,
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
                    let raw = seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x;
                    let glyph_width_floor = (seeds[glyph_idx].glyph.w * 0.25).max(1.0);
                    raw.max(glyph_width_floor).max(1.0)
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
                let raw = seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x;
                let glyph_width_floor = (seeds[glyph_idx].glyph.w * 0.25).max(1.0);
                let metric_advance = raw.max(glyph_width_floor).max(1.0);
                let kerning = seeds[glyph_idx + 1].kerning;
                let base_advance = match kerning.mode {
                    // `Auto` keeps the shaped (font-pair-kerned) advance.
                    KerningMode::Auto => metric_advance,
                    // `Fixed` steps by the glyph's OWN nominal (un-kerned) advance,
                    // dropping font pair kerning and the optical adjustment. The
                    // shaped advance bakes in pair kerning, so the raw metrics
                    // table is consulted; falls back to the shaped advance when the
                    // metric is unavailable.
                    KerningMode::Fixed => {
                        let own = nominal_glyph_advance_px(font_system, &seeds[glyph_idx].glyph)
                            .unwrap_or(metric_advance);
                        optical_base_advance(own, metric_advance)
                    }
                    KerningMode::Optical => {
                        metric_advance
                            + optical_horizontal_pair_adjustment(
                                profiles[glyph_idx],
                                profiles[glyph_idx + 1],
                                metric_advance,
                                font_size_px,
                            )
                    }
                };
                let spacing_basis = match kerning.mode {
                    KerningMode::Auto | KerningMode::Fixed => metric_advance,
                    KerningMode::Optical => ((profiles[glyph_idx].width_px()
                        + profiles[glyph_idx + 1].width_px())
                        * 0.5)
                        .max(default_advance),
                };
                base_advance + kerning.extra_spacing_px(spacing_basis)
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

fn compute_layout_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            offsets.push(idx + ch.len_utf8());
        }
    }
    offsets
}

/// Resolve per-line alignment from inline style spans, falling back to the block alignment.
fn compute_inline_line_aligns(
    base_align: HorizontalAlign,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
) -> Vec<HorizontalAlign> {
    let Some(spans) = inline_style_spans else {
        return vec![base_align; layout_line_offsets.len().max(1)];
    };
    layout_line_offsets
        .iter()
        .map(|offset| {
            inline_style_at_offset(spans, *offset)
                .and_then(|span| span.align)
                .unwrap_or(base_align)
        })
        .collect()
}

fn spans_have_inline_size_overrides(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(|span| span.font_size_px.is_some())
}

fn compute_line_extra_spacing_table(
    params: &TextRenderParams,
    layout_text: &str,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
    font_size_px: f32,
    default_extra_line_spacing_px: f32,
) -> Vec<f32> {
    let Some(spans) = inline_style_spans else {
        return vec![default_extra_line_spacing_px; layout_line_offsets.len().max(1)];
    };
    let mut out = Vec::with_capacity(layout_line_offsets.len().max(1));
    for (line_idx, line_start) in layout_line_offsets.iter().copied().enumerate() {
        let line_end = layout_line_offsets
            .get(line_idx + 1)
            .copied()
            .unwrap_or(layout_text.len());
        let mut spacing_px = params.line_spacing_px;
        let mut spacing_percent = params.line_spacing_percent;
        let mut stretch_y_percent = params.glyph_height_percent;
        for span in spans
            .iter()
            .filter(|span| span.end > line_start && span.start < line_end)
        {
            if let Some(value) = span.line_spacing_px {
                spacing_px = value;
            }
            if let Some(value) = span.line_spacing_percent {
                spacing_percent = value;
            }
            if let Some(value) = span.glyph_stretch_percent {
                stretch_y_percent = value[1];
            }
        }
        let effective_percent = effective_spacing_percent(spacing_percent, stretch_y_percent);
        out.push(spacing_px + font_size_px * (effective_percent / 100.0));
    }
    if out.is_empty() {
        out.push(default_extra_line_spacing_px);
    }
    out
}

fn compute_horizontal_line_baselines(
    buffer: &Buffer,
    base_line_height_px: f32,
    default_extra_line_spacing_px: f32,
    line_extra_spacing_table: &[f32],
    has_inline_size_overrides: bool,
) -> Vec<f32> {
    let anchor_y = buffer
        .layout_runs()
        .next()
        .map(|run| run.line_y)
        .unwrap_or(base_line_height_px);
    let mut baselines = Vec::new();
    let mut cumulative_delta = 0.0f32;
    for (line_idx, run) in buffer.layout_runs().enumerate() {
        let baseline = horizontal_run_baseline_y(
            &run,
            line_idx,
            anchor_y,
            base_line_height_px,
            default_extra_line_spacing_px,
            has_inline_size_overrides,
        ) + cumulative_delta;
        baselines.push(baseline);
        cumulative_delta += line_extra_spacing_table
            .get(line_idx)
            .copied()
            .unwrap_or(default_extra_line_spacing_px)
            - default_extra_line_spacing_px;
    }
    baselines
}

fn horizontal_run_baseline_y(
    run: &LayoutRun<'_>,
    line_idx: usize,
    anchor_y: f32,
    base_line_height_px: f32,
    extra_line_spacing_px: f32,
    has_inline_size_overrides: bool,
) -> f32 {
    if has_inline_size_overrides {
        run.line_y
    } else {
        anchor_y + line_idx as f32 * base_line_height_px + line_idx as f32 * extra_line_spacing_px
    }
}

fn inline_text_color_at_offset(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> [u8; 4] {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.text_color)
        .unwrap_or(default_text_color)
}

fn inline_text_color_for_glyph(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [u8; 4] {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_text_color_at_offset(
        default_text_color,
        spans,
        line_offset + glyph.start.min(glyph.end),
    )
}

fn inline_glyph_offset_at_offset(spans: Option<&[InlineStyleSpan]>, offset: usize) -> [f32; 2] {
    inline_glyph_offset_style_at_offset(spans, offset).global_px
}

fn glyph_style_offset(
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> usize {
    layout_line_offsets.get(line_idx).copied().unwrap_or(0) + glyph.start.min(glyph.end)
}

fn inline_glyph_offset_style_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> InlineGlyphOffset {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.glyph_offset)
        .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0]))
}

fn inline_glyph_offset_span_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<(usize, usize)> {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .filter(|style| style.glyph_offset.is_some())
        .map(|style| (style.start, style.end))
}

fn inline_glyph_offset_style_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> InlineGlyphOffset {
    inline_glyph_offset_style_at_offset(
        spans,
        glyph_style_offset(layout_line_offsets, line_idx, glyph),
    )
}

fn inline_glyph_offset_span_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> Option<(usize, usize)> {
    inline_glyph_offset_span_at_offset(
        spans,
        glyph_style_offset(layout_line_offsets, line_idx, glyph),
    )
}

fn inline_glyph_offset_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [f32; 2] {
    inline_glyph_offset_style_for_glyph(spans, layout_line_offsets, line_idx, glyph).global_px
}

fn inline_glyph_scale_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> GlyphScaleSettings {
    let stretch = spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.glyph_stretch_percent)
        .unwrap_or([params.glyph_width_percent, params.glyph_height_percent]);
    GlyphScaleSettings {
        width_mul: (stretch[0] / 100.0).clamp(0.01, 3.0),
        height_mul: (stretch[1] / 100.0).clamp(0.01, 3.0),
    }
}

fn inline_glyph_scale_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> GlyphScaleSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_scale_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}

fn inline_kerning_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> KerningSettings {
    let style = spans.and_then(|value| inline_style_at_offset(value, offset));
    let stretch_x_percent = style
        .and_then(|value| value.glyph_stretch_percent)
        .map(|value| value[0])
        .unwrap_or(params.glyph_width_percent);
    let kerning_percent = style
        .and_then(|value| value.kerning_percent)
        .unwrap_or(params.kerning_percent);
    KerningSettings {
        mode: params.kerning_mode,
        spacing_px: style
            .and_then(|value| value.kerning_px)
            .unwrap_or(params.kerning_px)
            .clamp(-300.0, 300.0),
        spacing_percent: effective_spacing_percent(kerning_percent, stretch_x_percent),
    }
}

fn inline_kerning_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> KerningSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_kerning_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}

fn inline_style_at_offset(spans: &[InlineStyleSpan], offset: usize) -> Option<&InlineStyleSpan> {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end)
}

fn build_hard_hyphen_glyph(
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    font_size_px: f32,
    line_height_px: f32,
) -> Option<LayoutGlyph> {
    let mut buffer = Buffer::new(
        font_system,
        Metrics::new(font_size_px.max(1.0), line_height_px.max(1.0)),
    );
    buffer.set_size(font_system, None, None);
    buffer.set_text(font_system, "-", attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().cloned())
}

// Wrapped hyphen synthesis depends on shaped line boundaries, inline attrs and fallback font selection.
#[allow(clippy::too_many_arguments)]
fn build_wrapped_hyphen_glyph(
    font_system: &mut FontSystem,
    base_attrs: &Attrs<'_>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    layout_line_offsets: &[usize],
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
    font_size_px: f32,
    line_height_px: f32,
) -> Option<LayoutGlyph> {
    let hyphen_attrs = wrapped_hyphen_attrs(
        base_attrs,
        inline_style_spans,
        inline_font_registry,
        layout_line_offsets,
        run,
        next,
    );
    let hyphen_attrs = hyphen_attrs.as_attrs();
    build_hard_hyphen_glyph(font_system, &hyphen_attrs, font_size_px, line_height_px)
}

fn wrapped_hyphen_attrs<'a>(
    base_attrs: &Attrs<'a>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    layout_line_offsets: &[usize],
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
) -> AttrsOwned {
    let Some(spans) = inline_style_spans else {
        return AttrsOwned::new(base_attrs);
    };
    let Some(style_offset) = soft_hyphen_style_offset(run, next, layout_line_offsets) else {
        return AttrsOwned::new(base_attrs);
    };
    let Some(style) = inline_style_at_offset(spans, style_offset) else {
        return AttrsOwned::new(base_attrs);
    };
    apply_inline_style_to_attrs(base_attrs, style, inline_font_registry)
}

fn soft_hyphen_style_offset(
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
    layout_line_offsets: &[usize],
) -> Option<usize> {
    let next_run = next?;
    if next_run.line_i != run.line_i {
        return None;
    }

    let line_offset = layout_line_offsets.get(run.line_i).copied().unwrap_or(0);
    let last_glyph = run.glyphs.last()?;
    let next_first_glyph = next_run.glyphs.first()?;
    let end = last_glyph.end.min(run.text.len());
    let next_start = next_first_glyph.start.min(run.text.len());

    if next_start >= end
        && let Some(slice) = run.text.get(end..next_start)
        && let Some(rel_idx) = slice.find(SOFT_HYPHEN)
    {
        return Some(line_offset + end + rel_idx);
    }

    run.text[..end]
        .rfind(SOFT_HYPHEN)
        .filter(|idx| *idx < end)
        .map(|idx| line_offset + idx)
}

fn run_wraps_at_soft_hyphen(run: &LayoutRun<'_>, next: Option<&LayoutRun<'_>>) -> bool {
    let Some(next_run) = next else {
        return false;
    };
    if next_run.line_i != run.line_i {
        return false;
    }

    let Some(last_glyph) = run.glyphs.last() else {
        return false;
    };
    let Some(next_first_glyph) = next_run.glyphs.first() else {
        return false;
    };

    let end = last_glyph.end.min(run.text.len());
    let next_start = next_first_glyph.start.min(run.text.len());
    if next_start >= end {
        if let Some(slice) = run.text.get(end..next_start)
            && slice.contains(SOFT_HYPHEN)
        {
            return true;
        }
        if run.text[..end].ends_with(SOFT_HYPHEN) {
            return true;
        }
    }
    false
}

fn trailing_hyphen_x(run: &LayoutRun<'_>) -> f32 {
    let mut right = run.line_w;
    for glyph in run.glyphs {
        right = right.max(glyph.x + glyph.w);
    }
    right
}

fn image_has_alpha_on_edge(image: &RenderedTextImage, inset_px: u32) -> bool {
    if image.width == 0 || image.height == 0 {
        return false;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    let inset = inset_px.min(
        image
            .width
            .saturating_sub(1)
            .min(image.height.saturating_sub(1)),
    ) as usize;
    let left = inset;
    let right = width.saturating_sub(1 + inset);
    let top = inset;
    let bottom = height.saturating_sub(1 + inset);

    for x in left..=right {
        if image.rgba[(top * width + x) * 4 + 3] != 0 {
            return true;
        }
        if image.rgba[(bottom * width + x) * 4 + 3] != 0 {
            return true;
        }
    }
    for y in top..=bottom {
        if image.rgba[(y * width + left) * 4 + 3] != 0 {
            return true;
        }
        if image.rgba[(y * width + right) * 4 + 3] != 0 {
            return true;
        }
    }
    false
}

fn map_formula_target_arc_length(center_s_px: f32, text_len_px: f32, curve_len_px: f32) -> f32 {
    if curve_len_px <= 0.0 {
        return 0.0;
    }
    let text_len_px = text_len_px.max(1.0);
    if text_len_px <= curve_len_px {
        let leading_gap = (curve_len_px - text_len_px) * 0.5;
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

// Formula rotated bounds are clearer with explicit source/destination coordinates than with
// a one-off wrapper struct used only by this helper.
#[allow(clippy::too_many_arguments)]
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

#[cfg(test)]
mod tests {
    use super::{
        DrawnLineTransform, apply_line_placement, drawn_line_glyph_destination_center_raw,
        find_minimum_ink_distance_center_s, placed_contour_for_transform,
        sample_drawn_line_path_for_direction,
    };
    use crate::drawn_lines::{DrawnLinePath, DrawnLinePoint};
    use crate::glyph_contour::{
        GlyphContour, PlacedContour, min_placed_distance,
    };
    use crate::raster::rotated_rect_world_bounds;
    use crate::types::TextVectorLineTextDirection;

    /// Axis-aligned square contour of side `2 * half` centered on the origin.
    fn square_contour(half: f32) -> GlyphContour {
        GlyphContour {
            components: vec![vec![
                [-half, -half],
                [half, -half],
                [half, half],
                [-half, half],
            ]],
        }
    }

    /// A straight horizontal path along +x of the given length.
    fn straight_path(len: f32) -> DrawnLinePath {
        DrawnLinePath {
            points: vec![
                DrawnLinePoint {
                    x: 0.0,
                    y: 0.0,
                    arc_len_px: 0.0,
                },
                DrawnLinePoint {
                    x: len,
                    y: 0.0,
                    arc_len_px: len,
                },
            ],
            total_len_px: len,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// A semicircle path of radius `r`, sampled into `segments` chords, so the
    /// chord distance between two arc-length-equidistant points is always less
    /// than their arc-length separation (any curve pulls shapes together).
    fn semicircle_path(r: f32, segments: usize) -> DrawnLinePath {
        let mut points = Vec::with_capacity(segments + 1);
        let mut arc = 0.0f32;
        let mut prev: Option<(f32, f32)> = None;
        for i in 0..=segments {
            let angle = std::f32::consts::PI * (i as f32) / (segments as f32);
            let x = r * angle.cos();
            let y = r * angle.sin();
            if let Some((px, py)) = prev {
                arc += ((x - px).powi(2) + (y - py).powi(2)).sqrt();
            }
            points.push(DrawnLinePoint {
                x,
                y,
                arc_len_px: arc,
            });
            prev = Some((x, y));
        }
        DrawnLinePath {
            points,
            total_len_px: arc,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// Place a square contour (identity rotation/scale) at the path sample for
    /// arc-length `s`.
    fn square_at(path: &DrawnLinePath, contour: &GlyphContour, s: f32) -> Option<PlacedContour> {
        let (x, y, _tx, _ty) = sample_drawn_line_path_for_direction(path, s)?;
        Some(contour.placed(1.0, 0.0, 1.0, 1.0, x, y))
    }

    #[test]
    fn straight_line_keeps_arc_length_seed_when_gap_already_clears() {
        // Two width-10 squares (half extent 5). On a straight line the ink gap
        // at center distance d is d - 10. Seed at 20 already gives gap 10 >= 6,
        // so the search must return the seed unchanged (straight text intact).
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = contour.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let seed = 20.0f32;
        let target_gap = 6.0f32;

        let result = find_minimum_ink_distance_center_s(&path, seed, target_gap, &prev, |s| {
            square_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");

        assert!((result - seed).abs() < 1e-4, "result={result}");
        let placed = square_at(&path, &contour, result).expect("sample");
        assert!(
            min_placed_distance(&prev, &placed) >= target_gap,
            "gap not met"
        );
    }

    #[test]
    fn straight_line_pushes_forward_minimally_to_reach_gap() {
        // Seed at 12 gives gap 2 < 6; the search should push to ~16 (gap 6).
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = contour.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let target_gap = 6.0f32;

        let result = find_minimum_ink_distance_center_s(&path, 12.0, target_gap, &prev, |s| {
            square_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");

        assert!((result - 16.0).abs() <= 0.2, "result={result}");
        let placed = square_at(&path, &contour, result).expect("sample");
        assert!(min_placed_distance(&prev, &placed) >= target_gap);
    }

    #[test]
    fn curved_path_pushes_center_forward_past_overlap() {
        // On a semicircle, placing the current square at the arc-length seed
        // overlaps the previous one (chord < arc). The search must push the
        // center forward until the ink gap is cleared.
        let path = semicircle_path(50.0, 96);
        let contour = square_contour(5.0);
        let prev = square_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 6.0f32;
        let target_gap = 4.0f32;

        // Confirm the seed really overlaps (distance 0) so the test is meaningful.
        let seed_placed = square_at(&path, &contour, seed).expect("seed sample");
        assert_eq!(min_placed_distance(&prev, &seed_placed), 0.0);

        let result = find_minimum_ink_distance_center_s(&path, seed, target_gap, &prev, |s| {
            square_at(&path, &contour, s)
        })
        .expect("semicircle should have room");

        assert!(result > seed, "result={result} should exceed seed={seed}");
        let placed = square_at(&path, &contour, result).expect("sample");
        assert!(
            min_placed_distance(&prev, &placed) >= target_gap,
            "gap not met at result={result}"
        );
    }

    #[test]
    fn empty_current_contour_falls_back_to_arc_length_seed() {
        // A space glyph has no ink: min distance is INFINITY, so the predicate
        // holds at the seed and the search returns it without looping.
        let path = straight_path(200.0);
        let prev = square_contour(5.0).placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0);
        let empty = PlacedContour::default();
        let seed = 25.0f32;

        let result = find_minimum_ink_distance_center_s(&path, seed, 6.0, &prev, |_s| {
            Some(empty.clone())
        })
        .expect("empty contour should place at the seed");
        assert!((result - seed).abs() < 1e-6, "result={result}");
    }

    #[test]
    fn line_placement_helper_sign_matches_top_bottom_intent() {
        // Horizontal line (rotation 0): the line's DOWN normal is +y in screen
        // y-down space, so a positive `line_frac` (сверху/top) must move the
        // glyph UP (smaller y), a negative one DOWN, and 0 must stay centered.
        let ink_height = 20.0f32;
        let (cx0, cy0) = apply_line_placement(100.0, 50.0, 0.0, ink_height, 0.0);
        assert!(
            (cx0 - 100.0).abs() < 1e-6 && (cy0 - 50.0).abs() < 1e-6,
            "0% must keep the ink center on the line: ({cx0}, {cy0})"
        );

        let (_, cy_top) = apply_line_placement(100.0, 50.0, 0.0, ink_height, 1.0);
        let (_, cy_bottom) = apply_line_placement(100.0, 50.0, 0.0, ink_height, -1.0);
        assert!(cy_top < cy0, "+100% (сверху) must move UP: {cy_top} !< {cy0}");
        assert!(
            cy_bottom > cy0,
            "-100% (снизу) must move DOWN: {cy_bottom} !> {cy0}"
        );
        // Magnitude at the extremes is half the ink height.
        assert!((cy0 - cy_top - ink_height * 0.5).abs() < 1e-4);
        assert!((cy_bottom - cy0 - ink_height * 0.5).abs() < 1e-4);
    }

    #[test]
    fn placed_contour_stays_within_composited_world_rect() {
        // A contour spanning the glyph outline bbox must land inside the same
        // rotated, scaled world rect the blit draws into. This guards the
        // pivot/translation math in placed_contour_for_transform, which now
        // works in the outline (pen-relative, y-down px) frame.
        let glyph_w = 10.0f32;
        let glyph_h = 8.0f32;
        let width_mul = 1.5f32;
        let height_mul = 1.5f32;
        // With placement (0, 0), the outline bbox spans [0, glyph_w] x
        // [0, glyph_h] and its center is the bitmap center (glyph_w/2, glyph_h/2).
        let placement_left = 0.0f32;
        let placement_top = 0.0f32;
        let contour = GlyphContour {
            components: vec![vec![
                [0.0, 0.0],
                [glyph_w, 0.0],
                [glyph_w, glyph_h],
                [0.0, glyph_h],
            ]],
        };

        // Scaled rect for src at origin, mirroring GlyphScaleSettings::scaled_rect.
        let scaled_width = glyph_w * width_mul;
        let scaled_height = glyph_h * height_mul;
        let scaled_left = glyph_w * 0.5 - scaled_width * 0.5;
        let scaled_top = glyph_h * 0.5 - scaled_height * 0.5;
        // 0% line placement: the ink center sits on the path point.
        let line_frac = 0.0f32;

        let transform = DrawnLineTransform {
            center_x: 40.0,
            center_y: 25.0,
            rotation_rad: 0.7,
        };
        let (dst_cx, dst_cy) =
            drawn_line_glyph_destination_center_raw(&transform, scaled_height, line_frac);
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            dst_cx,
            dst_cy,
            transform.rotation_rad,
        );

        let placed = placed_contour_for_transform(
            &contour,
            placement_left,
            placement_top,
            glyph_w,
            glyph_h,
            width_mul,
            height_mul,
            scaled_height,
            line_frac,
            [0.0, 0.0],
            &transform,
        );

        let eps = 0.01f32;
        assert!(placed.aabb_min[0] >= min_x - eps, "min_x");
        assert!(placed.aabb_min[1] >= min_y - eps, "min_y");
        assert!(placed.aabb_max[0] <= max_x + eps, "max_x");
        assert!(placed.aabb_max[1] <= max_y + eps, "max_y");
    }
}
