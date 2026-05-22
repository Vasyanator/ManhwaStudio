/*
File: src/tabs/typing/render_next/formula/render.rs

Purpose:
Formula raster/layout path staged рендера typing.

Main responsibilities:
- рендерить glyph seeds по формульной траектории без зависимости от старого `render.rs`;
- собирать formula-specific glyph metadata, arc-length mapping и rotated bounds/draw;
- отдельно обрабатывать fallback для `TextLayoutMode::Shape`, когда кривая слишком короткая.

Source:
- `render_text_with_formula_layout`
- `render_text_with_formula_layout_once`
- `collect_formula_glyph_seeds`
- `assign_formula_seed_advances`
- rotated-rect и arc-length helper'ы
из старого `src/tabs/typing/render.rs`
*/

use super::{FormulaEvalInput, FormulaProgramBundle};
use crate::tabs::typing::render_next::drawn_lines::{
    DrawnLinePath, build_vector_line_paths, load_raster_line_paths,
};
use crate::tabs::typing::render_next::font_registry::InlineFontRegistry;
use crate::tabs::typing::render_next::inline_styles::{
    InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
};
use crate::tabs::typing::render_next::pipeline::{
    GlyphScaleSettings, KerningSettings, effective_spacing_percent, horizontal_line_offset,
};
use crate::tabs::typing::render_next::raster::{
    PixelBounds, bilinear_sample_rgba, blend_pixel_over, build_glyph_rgba_buffer,
    sample_swash_alpha, trim_rendered_image_to_alpha_bounds,
};
use crate::tabs::typing::render_next::types::{
    KerningMode, RenderedTextImage, TextLayoutMode, TextRenderParams, TextVectorLineDistanceMode,
    TextVectorLineTextDirection,
};
use cosmic_text::{
    Attrs, AttrsOwned, Buffer, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping, SwashCache,
    SwashContent,
};
use std::collections::HashMap;

const SOFT_HYPHEN: char = '\u{00AD}';

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

#[derive(Debug, Clone, Copy)]
struct FormulaArcLengthSample {
    t01: f32,
    arc_len_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct DrawnLineTransform {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
}

fn drawn_line_glyph_destination_center(
    seed: &FormulaGlyphSeed,
    transform: &DrawnLineTransform,
    scaled_top: f32,
    scaled_height: f32,
) -> (f32, f32) {
    let scaled_center_y = scaled_top + scaled_height * 0.5;
    let baseline_y = seed.origin_y + seed.glyph_offset_px[1];
    let local_x = 0.0;
    let local_y = scaled_center_y - baseline_y;
    let (sin_a, cos_a) = transform.rotation_rad.sin_cos();
    (
        transform.center_x + local_x * cos_a - local_y * sin_a,
        transform.center_y + local_x * sin_a + local_y * cos_a,
    )
}

#[derive(Debug, Clone, Copy)]
struct GlyphInkProfile {
    left_px: f32,
    right_px: f32,
}

impl GlyphInkProfile {
    #[must_use]
    fn fallback(width_px: f32, height_px: f32) -> Self {
        let _ = height_px;
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

    let transforms = build_drawn_line_transforms(params, seeds.as_slice(), paths);
    let skipped = transforms.iter().filter(|item| item.is_none()).count();
    let mut cache = SwashCache::new();
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
            drawn_line_glyph_destination_center(seed, transform, scaled_top, scaled_height);
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
        });
    }

    let pad = font_size_px.ceil().max(2.0) as u32;
    let (out_width, out_height, x_offset, y_offset) =
        if let Some((width, height)) = fixed_output_size {
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

    for (seed, transform) in seeds.drain(..).zip(transforms.into_iter()) {
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
        let src_left = (physical.x + image.placement.left) as f32;
        let src_top = (physical.y - image.placement.top) as f32;
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
        let (dst_center_x, dst_center_y) =
            drawn_line_glyph_destination_center(&seed, &transform, scaled_top, scaled_height);
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
    })
}

fn build_drawn_line_transforms(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
) -> Vec<Option<DrawnLineTransform>> {
    let mut line_offsets = HashMap::<usize, DrawnLinePlacementState>::new();
    let layout_settings = custom_line_layout_settings(params);
    let letter_spacing_mul = layout_settings.letter_spacing_mul.clamp(0.0, 8.0);
    let letter_spacing_px = layout_settings.letter_spacing_px.clamp(-10_000.0, 10_000.0);
    let mut transforms = seeds
        .iter()
        .map(|seed| {
            let path = paths.get(seed.line_idx).and_then(Option::as_ref)?;
            let advance =
                ((seed.advance_px.max(1.0) * letter_spacing_mul) + letter_spacing_px).max(1.0);
            let state = line_offsets.entry(seed.line_idx).or_default();
            let half_advance = advance * 0.5;
            let mut center_s = state.offset_s_px + half_advance + seed.extended_offset.line_px;
            if center_s > path.total_len_px {
                return None;
            }
            if vector_line_distance_mode(params, seed.line_idx)
                == TextVectorLineDistanceMode::MinimumPreviousDistance
                && let Some((prev_x, prev_y)) = state.previous_center
            {
                let minimum_distance = state.previous_half_advance_px + half_advance;
                center_s = find_minimum_distance_center_s(
                    path,
                    center_s,
                    minimum_distance,
                    prev_x,
                    prev_y,
                )?;
            }
            if center_s > path.total_len_px {
                return None;
            }
            state.offset_s_px = center_s + half_advance - seed.extended_offset.line_px;
            let (center_x, center_y, tangent_x, tangent_y) =
                sample_drawn_line_path_for_direction(path, center_s)?;
            let tangent_len = (tangent_x * tangent_x + tangent_y * tangent_y)
                .sqrt()
                .max(1e-6);
            let tangent_x = tangent_x / tangent_len;
            let tangent_y = tangent_y / tangent_len;
            let normal_offset = layout_settings.normal_offset_px;
            let center_x = center_x - tangent_y * normal_offset;
            let center_y = center_y + tangent_x * normal_offset;
            let rotation_rad = (if layout_settings.use_tangent_rotation {
                tangent_y.atan2(tangent_x)
            } else {
                layout_settings.static_rotation_rad
            }) + vector_line_flip_rotation(params, seed.line_idx)
                + seed.extended_offset.glyph_rotation_rad;
            let (sin_a, cos_a) = rotation_rad.sin_cos();
            let center_x =
                center_x + seed.glyph_offset_px[0] * cos_a - seed.glyph_offset_px[1] * sin_a;
            let center_y =
                center_y + seed.glyph_offset_px[0] * sin_a + seed.glyph_offset_px[1] * cos_a;
            state.previous_center = Some((center_x, center_y));
            state.previous_half_advance_px = half_advance;
            if seed.extended_offset.shift_following
                && is_last_seed_in_offset_span_on_line(seeds, seed)
            {
                state.offset_s_px += seed.extended_offset.line_px;
            }
            Some(DrawnLineTransform {
                center_x,
                center_y,
                rotation_rad,
            })
        })
        .collect::<Vec<_>>();
    apply_drawn_line_group_rotations(seeds, transforms.as_mut_slice());
    transforms
}

#[derive(Debug, Default)]
struct DrawnLinePlacementState {
    offset_s_px: f32,
    previous_center: Option<(f32, f32)>,
    previous_half_advance_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct CustomLineLayoutSettings {
    use_tangent_rotation: bool,
    static_rotation_rad: f32,
    normal_offset_px: f32,
    letter_spacing_mul: f32,
    letter_spacing_px: f32,
}

fn custom_line_layout_settings(params: &TextRenderParams) -> CustomLineLayoutSettings {
    match params.text_layout_mode {
        TextLayoutMode::CustomRasterLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.drawn_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.drawn_lines_layout.static_rotation_rad,
            normal_offset_px: params.drawn_lines_layout.normal_offset_px,
            letter_spacing_mul: params.drawn_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.drawn_lines_layout.letter_spacing_px,
        },
        TextLayoutMode::CustomVectorLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.vector_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.vector_lines_layout.static_rotation_rad,
            normal_offset_px: params.vector_lines_layout.normal_offset_px,
            letter_spacing_mul: params.vector_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.vector_lines_layout.letter_spacing_px,
        },
        TextLayoutMode::Normal | TextLayoutMode::Formula | TextLayoutMode::Shape => {
            CustomLineLayoutSettings {
                use_tangent_rotation: true,
                static_rotation_rad: 0.0,
                normal_offset_px: 0.0,
                letter_spacing_mul: 1.0,
                letter_spacing_px: 0.0,
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

fn distance_between_points(a_x: f32, a_y: f32, b_x: f32, b_y: f32) -> f32 {
    let dx = b_x - a_x;
    let dy = b_y - a_y;
    (dx * dx + dy * dy).sqrt()
}

fn find_minimum_distance_center_s(
    path: &DrawnLinePath,
    start_s: f32,
    minimum_distance: f32,
    prev_x: f32,
    prev_y: f32,
) -> Option<f32> {
    let start_sample = sample_drawn_line_path_for_direction(path, start_s)?;
    if distance_between_points(prev_x, prev_y, start_sample.0, start_sample.1) >= minimum_distance {
        return Some(start_s);
    }

    let scan_step = (minimum_distance / 8.0).clamp(0.5, 4.0);
    let mut low = start_s;
    let mut high = (start_s + scan_step).min(path.total_len_px);
    while high <= path.total_len_px {
        let sample = sample_drawn_line_path_for_direction(path, high)?;
        if distance_between_points(prev_x, prev_y, sample.0, sample.1) >= minimum_distance {
            break;
        }
        if high >= path.total_len_px {
            return Some(path.total_len_px + minimum_distance);
        }
        low = high;
        high = (high + scan_step).min(path.total_len_px);
    }

    for _ in 0..18 {
        let mid = low + (high - low) * 0.5;
        let sample = sample_drawn_line_path_for_direction(path, mid)?;
        if distance_between_points(prev_x, prev_y, sample.0, sample.1) >= minimum_distance {
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
) -> Result<RenderedTextImage, String> {
    let width_px = params.width_px.max(1);
    let formula_program = FormulaProgramBundle::compile(&params.formula_layout)?;
    let mut cache = SwashCache::new();
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
        include_rotated_rect_bounds(
            &mut bounds,
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
        );
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
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
        let src_left = (physical.x + image.placement.left) as f32;
        let src_top = (physical.y - image.placement.top) as f32;
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
            transform.center_x,
            transform.center_y,
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
                let rel_x = world_x - transform.center_x;
                let rel_y = world_y - transform.center_y;
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
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        let line_offset_x = horizontal_line_offset(width_px, run.line_w, params.align) as f32;
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
                    KerningMode::Metric => metric_advance,
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
                    KerningMode::Metric => metric_advance,
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
    layout: &crate::tabs::typing::render_next::types::TextFormulaLayoutParams,
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
fn include_rotated_rect_bounds(
    bounds: &mut PixelBounds,
    src_left: f32,
    src_top: f32,
    src_width: f32,
    src_height: f32,
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
) {
    let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
        src_left,
        src_top,
        src_width,
        src_height,
        dst_center_x,
        dst_center_y,
        rotation_rad,
    );
    let min_x_i = min_x.floor() as i32;
    let min_y_i = min_y.floor() as i32;
    let max_x_i = max_x.ceil() as i32;
    let max_y_i = max_y.ceil() as i32;
    bounds.include_rect(
        min_x_i,
        min_y_i,
        (max_x_i - min_x_i).max(1),
        (max_y_i - min_y_i).max(1),
    );
}

fn rotated_rect_world_bounds(
    src_left: f32,
    src_top: f32,
    src_width: f32,
    src_height: f32,
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
) -> (f32, f32, f32, f32) {
    let src_center_x = src_left + src_width * 0.5;
    let src_center_y = src_top + src_height * 0.5;
    let corners = [
        (src_left, src_top),
        (src_left + src_width, src_top),
        (src_left + src_width, src_top + src_height),
        (src_left, src_top + src_height),
    ];
    let cos_a = rotation_rad.cos();
    let sin_a = rotation_rad.sin();
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
        let rel_x = x - src_center_x;
        let rel_y = y - src_center_y;
        let tx = dst_center_x + rel_x * cos_a - rel_y * sin_a;
        let ty = dst_center_y + rel_x * sin_a + rel_y * cos_a;
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }
    (min_x, min_y, max_x, max_y)
}

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
    use super::{distance_between_points, find_minimum_distance_center_s};
    use crate::tabs::typing::render_next::drawn_lines::{DrawnLinePath, DrawnLinePoint};
    use crate::tabs::typing::render_next::types::TextVectorLineTextDirection;

    #[test]
    fn minimum_distance_search_places_next_center_at_threshold() {
        let path = DrawnLinePath {
            points: vec![
                DrawnLinePoint {
                    x: 0.0,
                    y: 0.0,
                    arc_len_px: 0.0,
                },
                DrawnLinePoint {
                    x: 200.0,
                    y: 0.0,
                    arc_len_px: 200.0,
                },
            ],
            total_len_px: 200.0,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: true,
        };

        let next_s = match find_minimum_distance_center_s(&path, 30.0, 42.0, 0.0, 0.0) {
            Some(value) => value,
            None => panic!("straight path should have enough room"),
        };
        let actual = distance_between_points(0.0, 0.0, next_s, 0.0);

        assert!((next_s - 42.0).abs() <= 0.02, "next_s={next_s}");
        assert!(actual >= 41.98, "actual={actual}");
    }
}
