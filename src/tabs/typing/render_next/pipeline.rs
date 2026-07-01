/*
File: src/tabs/typing/render_next/pipeline.rs

Purpose:
Staged pipeline нового рендера после переноса horizontal foundation, vertical path и formula path.

Main responsibilities:
- давать изолированную входную точку нового рендера без переключения текущего продового пути;
- рендерить staged horizontal/vertical text через `cosmic-text`, включая attrs-level rich text;
- маршрутизировать formula/shape layout в отдельный `formula::render` path;
- подключать post-effects через отдельный `effects` пакет после базового растра;
- предформировать horizontal wrap/shape и vertical columns через отдельный `wrap`-слой;
- использовать вынесенные `font_registry` и `raster` как фундамент для следующих этапов.

Notes:
- inline-теги проходят через отдельный `inline_styles` слой как для attrs-level rich text,
  так и для glyph-level color/kerning/stretch/offset/line-spacing в horizontal path;
- horizontal monochrome glyphs are rasterized from their true font outlines
  (`draw_horizontal_glyph` for the normal path, the `RotatedGlyphPlacement` draw
  pass for the inline-rotated path) via the shared `glyph_blit` helpers; the layout
  math, bounds/canvas assembly and inline-color contract are unchanged, and color
  glyphs keep the `raster.rs` bitmap blit;
- `smoke_render_text_to_image` оставлен как бездисковая заглушка для runtime smoke-anchor;
- основной источник поведения: `render_text_to_image`, `reshape_text_for_shape`,
  `build_vertical_layout_text`, `render_vertical_text`, `render_text_with_formula_layout`,
  `soft_hyphenate_overlong`
  и базовый raster path из старого
  `src/tabs/typing/render.rs`.
*/

use crate::trace::cat;

use super::effects::{apply_effects_pipeline, apply_text_preprocess_effects};
use super::font_registry::{build_inline_font_registry, load_selected_font_from_path};
use super::formula::{
    FormulaRenderOutcome, FormulaRenderRequest, render_text_with_drawn_lines_layout,
    render_text_with_formula_layout, render_text_with_vector_lines_layout,
};
use super::inline_styles::{
    InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
    collect_requested_inline_font_labels, parse_inline_style_tags, remap_inline_style_spans,
    spans_have_attrs_overrides,
};
use super::glyph_blit::{glyph_outline_transform, glyph_subpixel_offset, resolve_outline_for_glyph};
use super::layout::{VerticalRasterRequest, render_vertical_text};
use super::raster::{
    GlyphRgbaView, PixelBounds, RgbaCanvasView, build_glyph_rgba_buffer,
    draw_rotated_scaled_glyph_rgba, draw_scaled_glyph_rgba, include_rotated_rect_bounds,
    include_scaled_rect_bounds, is_cancelled, rasterize_unscaled_glyph,
    trim_rendered_image_to_alpha_bounds,
};
use super::vector::{Outline, OutlineCache, build_aa_lut, rasterize_outline_into};
use super::types::{
    HorizontalAlign, KerningMode, RenderedTextImage, TextLayoutMode, TextLineMode,
    TextRenderParams, TextRenderShapeCompareParams, TextWrapMode,
};
use super::wrap::{
    HyphenationDictionaries, LayoutTextResult, ShapeWrapRequest, VerticalWrapRequest,
    build_vertical_layout_text, needs_hyphenation_dicts, reshape_text_for_shape,
    should_prehyphenate_overlong, word_break_policy,
};
use crate::tabs::typing::segmentation::with_default_segmenter;
use cosmic_text::{
    Align, Attrs, AttrsOwned, Buffer, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping,
    SwashCache, Wrap,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

const SOFT_HYPHEN: char = '\u{00AD}';
const UNCHANGED_LAYOUT_TEXT_WARNING: &str =
    "Форма текста совпадает с параметрами сравниваемого рендера.";

#[derive(Debug, Clone, Copy)]
pub(crate) struct GlyphScaleSettings {
    pub(crate) width_mul: f32,
    pub(crate) height_mul: f32,
}

impl GlyphScaleSettings {
    #[must_use]
    pub(crate) fn from_params(params: &TextRenderParams) -> Self {
        Self {
            width_mul: (params.glyph_width_percent / 100.0).clamp(0.01, 3.0),
            height_mul: (params.glyph_height_percent / 100.0).clamp(0.01, 3.0),
        }
    }

    #[must_use]
    pub(crate) fn is_identity(self) -> bool {
        (self.width_mul - 1.0).abs() <= f32::EPSILON
            && (self.height_mul - 1.0).abs() <= f32::EPSILON
    }

    #[must_use]
    pub(crate) fn scaled_size(self, width_px: f32, height_px: f32) -> (f32, f32) {
        (
            (width_px.max(1.0) * self.width_mul).max(1.0),
            (height_px.max(1.0) * self.height_mul).max(1.0),
        )
    }

    #[must_use]
    pub(crate) fn scaled_rect(
        self,
        left_px: f32,
        top_px: f32,
        width_px: f32,
        height_px: f32,
    ) -> (f32, f32, f32, f32) {
        let center_x = left_px + width_px * 0.5;
        let center_y = top_px + height_px * 0.5;
        let scaled_width = (width_px.max(1.0) * self.width_mul).max(1.0);
        let scaled_height = (height_px.max(1.0) * self.height_mul).max(1.0);
        (
            center_x - scaled_width * 0.5,
            center_y - scaled_height * 0.5,
            scaled_width,
            scaled_height,
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KerningSettings {
    pub(crate) mode: KerningMode,
    pub(crate) spacing_px: f32,
    pub(crate) spacing_percent: f32,
}

impl KerningSettings {
    #[must_use]
    pub(crate) fn from_params(params: &TextRenderParams) -> Self {
        Self {
            mode: params.kerning_mode,
            spacing_px: params.kerning_px.clamp(-300.0, 300.0),
            spacing_percent: effective_spacing_percent(
                params.kerning_percent,
                params.glyph_width_percent,
            ),
        }
    }

    #[must_use]
    fn has_zero_adjustment(self) -> bool {
        self.spacing_px.abs() <= f32::EPSILON && self.spacing_percent.abs() <= f32::EPSILON
    }

    #[must_use]
    pub(crate) fn extra_spacing_px(self, basis_px: f32) -> f32 {
        self.spacing_px + basis_px.max(0.0) * (self.spacing_percent / 100.0)
    }

    #[must_use]
    pub(crate) fn uses_default_metric_layout(self) -> bool {
        self.mode == KerningMode::Metric && self.has_zero_adjustment()
    }
}

#[derive(Debug, Clone)]
struct HorizontalRunLayout {
    glyph_xs: Vec<f32>,
    line_width_px: f32,
    visual_width_px: f32,
    leading_hang_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct LayoutShapeParams {
    width_px: u32,
    text_wrap_mode: TextWrapMode,
    shape_min_width_percent: f32,
    shape_variant: u8,
}

impl LayoutShapeParams {
    #[must_use]
    fn from_compare(params: &TextRenderShapeCompareParams) -> Self {
        Self {
            width_px: params.width_px.max(1),
            text_wrap_mode: params.text_wrap_mode,
            shape_min_width_percent: params.shape_min_width_percent,
            shape_variant: params.shape_variant,
        }
    }
}

#[must_use]
fn estimate_placeholder_height(params: &TextRenderParams) -> u32 {
    let line_count = params.text.lines().count().max(1);
    u32::try_from(line_count).unwrap_or(u32::MAX)
}

// Layout comparison needs the same resolved render context as the main prepass.
#[allow(clippy::too_many_arguments)]
fn build_layout_text_for_shape_params(
    params: &TextRenderParams,
    source_text: &str,
    shape_params: LayoutShapeParams,
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    font_size_px: f32,
    base_line_height_px: f32,
    extra_line_spacing_px: f32,
    preserve_edge_spaces: bool,
) -> LayoutTextResult {
    let hyphen_dicts =
        needs_hyphenation_dicts(shape_params.text_wrap_mode).then(HyphenationDictionaries::new);
    let shaped_text = if should_prehyphenate_overlong(shape_params.text_wrap_mode) {
        with_default_segmenter(|seg| seg.soft_hyphenate_overlong(source_text))
    } else {
        source_text.to_string()
    };

    match params.text_line_mode {
        TextLineMode::Horizontal => reshape_text_for_shape(ShapeWrapRequest {
            text: shaped_text.as_str(),
            font_system,
            attrs,
            font_size_px,
            line_height_px: base_line_height_px,
            base_width_px: shape_params.width_px.max(1) as f32,
            wrap_mode: shape_params.text_wrap_mode,
            hyphen_dicts: hyphen_dicts.as_ref(),
            word_break_policy: word_break_policy(shape_params.text_wrap_mode),
            shape: params.text_shape,
            min_width_percent: shape_params.shape_min_width_percent,
            shape_variant: shape_params.shape_variant,
            allow_moderate_trees: params.allow_moderate_trees,
            hanging_punctuation: params.hanging_punctuation,
            preserve_edge_spaces,
        }),
        TextLineMode::Vertical => LayoutTextResult {
            text: build_vertical_layout_text(VerticalWrapRequest {
                text: shaped_text.as_str(),
                width_px: shape_params.width_px.max(1) as f32,
                font_size_px,
                extra_line_spacing_px,
                wrap_mode: shape_params.text_wrap_mode,
                hyphen_dicts: hyphen_dicts.as_ref(),
                word_break_policy: word_break_policy(shape_params.text_wrap_mode),
                shape: params.text_shape,
                min_width_percent: shape_params.shape_min_width_percent,
                allow_moderate_trees: params.allow_moderate_trees,
                preserve_edge_spaces,
            }),
            warnings: Vec::new(),
        },
    }
}

/// Применяет post-effects pipeline (обводка, свечение, тени, градиенты и т.д.) к произвольному
/// RGBA-изображению, минуя layout/raster текста.
///
/// Используется вкладкой typing, чтобы переиспользовать те же эффекты, что уже применяются к
/// растрированному тексту, на сторонних (импортированных) картинках-оверлеях. На вход подаётся
/// исходный (неизменённый) RGBA-буфер `width * height * 4`, на выход возвращается новый
/// `RenderedTextImage` с применёнными эффектами (эффекты могут увеличивать холст под запас).
///
/// `effects_json` имеет тот же контракт, что и `TextRenderParams::effects_json`. Пустой/пробельный
/// JSON означает «без эффектов» и возвращает изображение без изменений.
pub fn apply_effects_to_image(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    effects_json: &str,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let _effects_span = crate::trace_scope!(
        cat::RENDER,
        "apply_effects_to_image w={} h={} has_effects={}",
        width,
        height,
        !effects_json.trim().is_empty()
    );
    if is_cancelled(cancel) {
        return Err("render_next render cancelled".to_string());
    }
    let expected_len = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().map(|h| w.saturating_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "apply_effects_to_image: размеры изображения переполняют usize".to_string())?;
    if rgba.len() != expected_len {
        return Err(format!(
            "apply_effects_to_image: длина RGBA-буфера {} не соответствует {width}x{height}x4 = {expected_len}",
            rgba.len()
        ));
    }

    let mut image = RenderedTextImage {
        width,
        height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
    };

    if !effects_json.trim().is_empty() {
        apply_effects_pipeline(&mut image, effects_json, cancel)?;
    }

    Ok(image)
}

pub fn render_text_to_image(
    params: &TextRenderParams,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let _render_span = crate::trace_scope!(
        cat::RENDER,
        "render_text_to_image layout={:?} line_mode={:?} wrap={:?} width_px={} font_size={:.1} effects={}",
        params.text_layout_mode,
        params.text_line_mode,
        params.text_wrap_mode,
        params.width_px,
        params.font_size_px,
        !params.effects_json.trim().is_empty()
    );
    if is_cancelled(cancel) {
        return Err("render_next render cancelled".to_string());
    }

    let width_px = params.width_px.max(1);
    let font_size_px = params.font_size_px.max(1.0);
    let line_spacing_percent =
        effective_spacing_percent(params.line_spacing_percent, params.glyph_height_percent);
    let extra_line_spacing_px =
        params.line_spacing_px + font_size_px * (line_spacing_percent / 100.0);
    let base_line_height_px = font_size_px.max(1.0);
    let line_height_px = (base_line_height_px + extra_line_spacing_px).max(1.0);
    let mut warnings = Vec::new();
    let prepared_text = prepare_source_text(&params.text, params);
    let (prepared_text, preprocess_generated_inline_tags) =
        apply_text_preprocess_effects(prepared_text.as_str(), params.effects_json.as_str())?;
    let parsed_inline_styles =
        if params.enable_inline_style_tags || preprocess_generated_inline_tags {
            Some(parse_inline_style_tags(
                prepared_text.as_str(),
                params.font_size_px,
            ))
        } else {
            None
        };

    let mut font_system = FontSystem::new();
    let selected_face = load_selected_font_from_path(
        &mut font_system,
        &params.font_path,
        params.selected_face_index,
    )
    .map_err(|error| format!("не удалось загрузить шрифт в fontdb: {error}"))?;

    let mut attrs = Attrs::new().metrics(Metrics::new(font_size_px, font_size_px));
    attrs = selected_face.apply_to_attrs(attrs);
    if params.force_bold {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if params.force_italic {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }

    let mut buffer = Buffer::new(
        &mut font_system,
        Metrics::new(font_size_px, base_line_height_px),
    );
    buffer.set_size(&mut font_system, Some(width_px as f32), None);
    buffer.set_wrap(&mut font_system, Wrap::None);

    let source_text = parsed_inline_styles
        .as_ref()
        .map(|parsed| parsed.plain_text.as_str())
        .unwrap_or(prepared_text.as_str());
    let preserve_edge_spaces = !params.trim_extra_spaces;
    let layout_shape_params = if matches!(
        params.text_layout_mode,
        TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines
    ) {
        LayoutShapeParams {
            width_px,
            text_wrap_mode: TextWrapMode::None,
            shape_min_width_percent: 100.0,
            shape_variant: params.shape_variant,
        }
    } else {
        LayoutShapeParams {
            width_px,
            text_wrap_mode: params.text_wrap_mode,
            shape_min_width_percent: params.shape_min_width_percent,
            shape_variant: params.shape_variant,
        }
    };
    let layout_text_result = build_layout_text_for_shape_params(
        params,
        source_text,
        layout_shape_params,
        &mut font_system,
        &attrs,
        font_size_px,
        base_line_height_px,
        extra_line_spacing_px,
        preserve_edge_spaces,
    );
    warnings.extend(layout_text_result.warnings);
    let layout_text = layout_text_result.text;
    if let Some(compare_params) = params.compare_shape_with.as_ref() {
        let compare_layout_text = build_layout_text_for_shape_params(
            params,
            source_text,
            LayoutShapeParams::from_compare(compare_params),
            &mut font_system,
            &attrs,
            font_size_px,
            base_line_height_px,
            extra_line_spacing_px,
            preserve_edge_spaces,
        )
        .text;
        if compare_layout_text == layout_text {
            warnings.push(UNCHANGED_LAYOUT_TEXT_WARNING.to_string());
            if compare_params.cancel_render_if_layout_text_unchanged {
                return Ok(RenderedTextImage {
                    width: 0,
                    height: 0,
                    rgba: Vec::new(),
                    warnings,
                    content_origin_x: 0,
                    content_origin_y: 0,
                });
            }
        }
    }
    let justify_alignment = justify_alignment_option(params.align);

    let mapped_inline_style_spans = parsed_inline_styles.as_ref().and_then(|parsed| {
        remap_inline_style_spans(
            parsed.plain_text.as_str(),
            layout_text.as_str(),
            parsed.spans.as_slice(),
        )
    });
    if parsed_inline_styles.is_some() && mapped_inline_style_spans.is_none() {
        warnings.push(
            "render_next inline style spans could not be remapped after text normalization; falling back to plain text layout"
                .to_string(),
        );
    }
    let inline_line_aligns = compute_inline_line_aligns(
        params.align,
        layout_text.as_str(),
        mapped_inline_style_spans.as_deref(),
    );
    let requested_inline_fonts = mapped_inline_style_spans
        .as_deref()
        .map(collect_requested_inline_font_labels)
        .unwrap_or_default();
    let inline_font_registry_build = build_inline_font_registry(
        &mut font_system,
        params.available_inline_fonts.as_slice(),
        requested_inline_fonts.as_slice(),
    );
    warnings.extend(inline_font_registry_build.warnings);

    if let Some(mapped_spans) = mapped_inline_style_spans
        .as_deref()
        .filter(|spans| spans_have_attrs_overrides(spans))
    {
        let styled_spans = mapped_spans
            .iter()
            .map(|span| {
                (
                    span.clone(),
                    apply_inline_style_to_attrs(&attrs, span, &inline_font_registry_build.registry),
                )
            })
            .collect::<Vec<_>>();
        let spans_iter = styled_spans.iter().filter_map(|(span, span_attrs)| {
            let text_slice = layout_text.get(span.start..span.end)?;
            Some((text_slice, span_attrs.as_attrs()))
        });
        buffer.set_rich_text(
            &mut font_system,
            spans_iter,
            &attrs,
            Shaping::Advanced,
            justify_alignment,
        );
    } else {
        buffer.set_text(
            &mut font_system,
            layout_text.as_str(),
            &attrs,
            Shaping::Advanced,
        );
    }
    apply_line_aligns_to_buffer(&mut buffer, inline_line_aligns.as_slice());
    buffer.shape_until_scroll(&mut font_system, false);

    if matches!(
        params.text_layout_mode,
        TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines
    ) {
        if params.text_line_mode != TextLineMode::Horizontal {
            return Err(
                "render_next custom line layout currently supports only horizontal line mode"
                    .to_string(),
            );
        }
        let request = FormulaRenderRequest {
            params,
            font_system: &mut font_system,
            buffer: &mut buffer,
            attrs: &attrs,
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            inline_font_registry: &inline_font_registry_build.registry,
            layout_text: layout_text.as_str(),
            font_size_px,
            base_line_height_px: font_size_px,
        };
        crate::trace_log!(cat::RENDER, "render_text path=custom_lines mode={:?}", params.text_layout_mode);
        let custom_lines_result = match params.text_layout_mode {
            TextLayoutMode::CustomRasterLines => render_text_with_drawn_lines_layout(request)?,
            TextLayoutMode::CustomVectorLines => render_text_with_vector_lines_layout(request)?,
            TextLayoutMode::Normal | TextLayoutMode::Formula | TextLayoutMode::Shape => {
                unreachable!("custom line layout branch only handles custom line modes")
            }
        };
        match custom_lines_result {
            FormulaRenderOutcome::Rendered(mut rendered) => {
                rendered.warnings.extend(warnings);
                apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
                return Ok(rendered);
            }
            FormulaRenderOutcome::FallbackToStandard(warning) => warnings.push(warning),
        }
    }

    if matches!(
        params.text_layout_mode,
        TextLayoutMode::Formula | TextLayoutMode::Shape
    ) {
        if params.text_line_mode != TextLineMode::Horizontal {
            return Err(
                "render_next formula layout currently supports only horizontal line mode"
                    .to_string(),
            );
        }
        crate::trace_log!(cat::RENDER, "render_text path=formula_shape mode={:?}", params.text_layout_mode);
        match render_text_with_formula_layout(FormulaRenderRequest {
            params,
            font_system: &mut font_system,
            buffer: &mut buffer,
            attrs: &attrs,
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            inline_font_registry: &inline_font_registry_build.registry,
            layout_text: layout_text.as_str(),
            font_size_px,
            base_line_height_px: font_size_px,
        })? {
            FormulaRenderOutcome::Rendered(mut rendered) => {
                rendered.warnings.extend(warnings);
                apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
                return Ok(rendered);
            }
            FormulaRenderOutcome::FallbackToStandard(warning) => warnings.push(warning),
        }
    }

    let glyph_scale = GlyphScaleSettings::from_params(params);
    if params.text_layout_mode == TextLayoutMode::Normal
        && params.text_line_mode == TextLineMode::Horizontal
        && params.kerning_mode == KerningMode::Optical
    {
        warnings.push(
            "render_next base pipeline пока использует metric glyph positions для optical kerning"
                .to_string(),
        );
    }

    let layout_line_offsets = compute_layout_line_offsets(layout_text.as_str());
    let has_inline_size_overrides = mapped_inline_style_spans
        .as_deref()
        .is_some_and(spans_have_inline_size_overrides);
    let line_extra_spacing_table = compute_line_extra_spacing_table(
        params,
        layout_text.as_str(),
        layout_line_offsets.as_slice(),
        mapped_inline_style_spans.as_deref(),
        font_size_px,
        extra_line_spacing_px,
    );
    if params.text_line_mode == TextLineMode::Vertical {
        crate::trace_log!(cat::RENDER, "render_text path=vertical lines={}", layout_line_offsets.len());
        let mut rendered = render_vertical_text(VerticalRasterRequest {
            params,
            font_system: &mut font_system,
            buffer: &mut buffer,
            layout_text: layout_text.as_str(),
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            layout_line_offsets: layout_line_offsets.as_slice(),
            font_size_px,
            base_line_height_px,
            line_extra_spacing_table: line_extra_spacing_table.as_slice(),
            direction: params.vertical_line_direction,
        })?;
        rendered.warnings.extend(warnings);
        apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
        return Ok(rendered);
    }

    let line_baselines = compute_horizontal_line_baselines(
        &buffer,
        base_line_height_px,
        extra_line_spacing_px,
        line_extra_spacing_table.as_slice(),
        has_inline_size_overrides,
    );

    // Inline-смещения с поворотом (группы/символа) обычный «прямой» blit не умеет —
    // для таких overlay используем отдельный путь с обратной выборкой и поворотом.
    if mapped_inline_style_spans
        .as_deref()
        .is_some_and(spans_have_inline_rotation)
    {
        crate::trace_log!(cat::RENDER, "render_text path=horizontal_rotated lines={}", layout_line_offsets.len());
        let mut rendered = render_horizontal_rotated(
            params,
            &mut font_system,
            &buffer,
            &attrs,
            &inline_font_registry_build.registry,
            mapped_inline_style_spans.as_deref(),
            layout_text.as_str(),
            layout_line_offsets.as_slice(),
            line_baselines.as_slice(),
            width_px,
            font_size_px,
            line_height_px,
            cancel,
        )?;
        rendered.warnings.extend(warnings);
        apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
        rendered = trim_rendered_image_to_alpha_bounds(rendered, 1);
        return Ok(rendered);
    }

    crate::trace_log!(cat::RENDER, "render_text path=horizontal lines={} align={:?}", layout_line_offsets.len(), params.align);
    let mut cache = SwashCache::new();
    let mut bounds = PixelBounds::empty();
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        let run_layout = horizontal_run_layout(
            params,
            &run,
            &mut font_system,
            &mut cache,
            layout_line_offsets.as_slice(),
            mapped_inline_style_spans.as_deref(),
            font_size_px,
        );
        let line_offset_x = horizontal_line_offset(
            width_px,
            if params.hanging_punctuation {
                run_layout.visual_width_px
            } else {
                run_layout.line_width_px
            },
            inline_line_aligns
                .get(line_idx)
                .copied()
                .unwrap_or(params.align),
        ) as f32
            - if params.hanging_punctuation {
                run_layout.leading_hang_px
            } else {
                0.0
            };
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or(run.line_y);

        for (glyph, glyph_x) in run.glyphs.iter().zip(run_layout.glyph_xs.iter().copied()) {
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                mapped_inline_style_spans.as_deref(),
                layout_line_offsets.as_slice(),
                run.line_i,
                glyph,
            );
            let glyph_offset = inline_glyph_offset_for_glyph(
                mapped_inline_style_spans.as_deref(),
                layout_line_offsets.as_slice(),
                run.line_i,
                glyph,
            );
            let physical = glyph.physical(
                (
                    line_offset_x + (glyph_x - glyph.x) + glyph_offset[0],
                    baseline_y + glyph_offset[1],
                ),
                1.0,
            );
            let Some(image) = cache.get_image(&mut font_system, physical.cache_key) else {
                continue;
            };
            include_scaled_rect_bounds(
                &mut bounds,
                (physical.x + image.placement.left) as f32,
                (physical.y - image.placement.top) as f32,
                image.placement.width as f32,
                image.placement.height as f32,
                glyph_scale,
            );
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = build_wrapped_hyphen_glyph(
                &mut font_system,
                &attrs,
                mapped_inline_style_spans.as_deref(),
                &inline_font_registry_build.registry,
                layout_line_offsets.as_slice(),
                &run,
                runs.peek(),
                font_size_px,
                font_size_px,
            )
        {
            let style_offset =
                soft_hyphen_style_offset(&run, runs.peek(), layout_line_offsets.as_slice());
            let hyphen_scale = style_offset
                .map(|offset| {
                    inline_glyph_scale_at_offset(
                        params,
                        mapped_inline_style_spans.as_deref(),
                        offset,
                    )
                })
                .unwrap_or(glyph_scale);
            let hyphen_offset = style_offset
                .map(|offset| {
                    inline_glyph_offset_at_offset(mapped_inline_style_spans.as_deref(), offset)
                })
                .unwrap_or([0.0, 0.0]);
            let hyphen_offset_x = line_offset_x
                + trailing_hyphen_x(&run)
                + run_layout
                    .glyph_xs
                    .last()
                    .zip(run.glyphs.last())
                    .map(|(glyph_x, last_glyph)| glyph_x - last_glyph.x)
                    .unwrap_or(0.0);
            let hyphen_physical = hyphen_glyph.physical(
                (
                    hyphen_offset_x + hyphen_offset[0],
                    baseline_y + hyphen_offset[1],
                ),
                1.0,
            );
            if let Some(image) = cache.get_image(&mut font_system, hyphen_physical.cache_key) {
                include_scaled_rect_bounds(
                    &mut bounds,
                    (hyphen_physical.x + image.placement.left) as f32,
                    (hyphen_physical.y - image.placement.top) as f32,
                    image.placement.width as f32,
                    image.placement.height as f32,
                    hyphen_scale,
                );
            }
        }
        line_idx += 1;
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            line_height_px.ceil() as u32,
        ));
    }

    let left_overhang = u32::try_from((-bounds.min_x).max(0)).unwrap_or(0);
    let right_overhang = u32::try_from((bounds.max_x - width_px as i32).max(0)).unwrap_or(0);
    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let out_width = width_px
        .saturating_add(left_overhang)
        .saturating_add(right_overhang)
        .saturating_add(horizontal_pad * 2)
        .saturating_add(safety_pad * 2);
    let content_height = u32::try_from((bounds.max_y - bounds.min_y).max(1)).unwrap_or(1);
    let min_height = line_height_px.ceil().max(1.0) as u32;
    let out_height = content_height
        .max(min_height)
        .saturating_add(vertical_pad * 2)
        .saturating_add(safety_pad * 2);
    let x_offset = i32::try_from(left_overhang + horizontal_pad + safety_pad).unwrap_or(i32::MAX);
    let y_offset =
        (-bounds.min_y).saturating_add(i32::try_from(vertical_pad + safety_pad).unwrap_or(0));

    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];
    // Per-render outline cache shared across the draw pass; extracts each glyph
    // outline at most once and is unused by the bounds pass above.
    let mut outline_cache = OutlineCache::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        let run_layout = horizontal_run_layout(
            params,
            &run,
            &mut font_system,
            &mut cache,
            layout_line_offsets.as_slice(),
            mapped_inline_style_spans.as_deref(),
            font_size_px,
        );
        let line_offset_x = horizontal_line_offset(
            width_px,
            if params.hanging_punctuation {
                run_layout.visual_width_px
            } else {
                run_layout.line_width_px
            },
            inline_line_aligns
                .get(line_idx)
                .copied()
                .unwrap_or(params.align),
        ) as f32
            - if params.hanging_punctuation {
                run_layout.leading_hang_px
            } else {
                0.0
            };
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or(run.line_y);

        for (glyph, glyph_x) in run.glyphs.iter().zip(run_layout.glyph_xs.iter().copied()) {
            let glyph_text_color = inline_text_color_for_glyph(
                params.text_color,
                mapped_inline_style_spans.as_deref(),
                layout_line_offsets.as_slice(),
                run.line_i,
                glyph,
            );
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                mapped_inline_style_spans.as_deref(),
                layout_line_offsets.as_slice(),
                run.line_i,
                glyph,
            );
            let glyph_offset = inline_glyph_offset_for_glyph(
                mapped_inline_style_spans.as_deref(),
                layout_line_offsets.as_slice(),
                run.line_i,
                glyph,
            );
            draw_horizontal_glyph(
                rgba.as_mut_slice(),
                out_width,
                out_height,
                &mut font_system,
                &mut cache,
                &mut outline_cache,
                glyph,
                line_offset_x + (glyph_x - glyph.x) + glyph_offset[0],
                baseline_y + glyph_offset[1],
                glyph_scale,
                glyph_text_color,
                x_offset,
                y_offset,
                &aa_lut,
            );
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = build_wrapped_hyphen_glyph(
                &mut font_system,
                &attrs,
                mapped_inline_style_spans.as_deref(),
                &inline_font_registry_build.registry,
                layout_line_offsets.as_slice(),
                &run,
                runs.peek(),
                font_size_px,
                font_size_px,
            )
        {
            let style_offset =
                soft_hyphen_style_offset(&run, runs.peek(), layout_line_offsets.as_slice());
            let hyphen_text_color = style_offset
                .map(|offset| {
                    inline_text_color_at_offset(
                        params.text_color,
                        mapped_inline_style_spans.as_deref(),
                        offset,
                    )
                })
                .unwrap_or(params.text_color);
            let hyphen_scale = style_offset
                .map(|offset| {
                    inline_glyph_scale_at_offset(
                        params,
                        mapped_inline_style_spans.as_deref(),
                        offset,
                    )
                })
                .unwrap_or(glyph_scale);
            let hyphen_offset = style_offset
                .map(|offset| {
                    inline_glyph_offset_at_offset(mapped_inline_style_spans.as_deref(), offset)
                })
                .unwrap_or([0.0, 0.0]);
            let hyphen_offset_x = line_offset_x
                + trailing_hyphen_x(&run)
                + run_layout
                    .glyph_xs
                    .last()
                    .zip(run.glyphs.last())
                    .map(|(glyph_x, last_glyph)| glyph_x - last_glyph.x)
                    .unwrap_or(0.0);
            draw_horizontal_glyph(
                rgba.as_mut_slice(),
                out_width,
                out_height,
                &mut font_system,
                &mut cache,
                &mut outline_cache,
                &hyphen_glyph,
                hyphen_offset_x + hyphen_offset[0],
                baseline_y + hyphen_offset[1],
                hyphen_scale,
                hyphen_text_color,
                x_offset,
                y_offset,
                &aa_lut,
            );
        }
        line_idx += 1;
    }

    let mut rendered = RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings,
        content_origin_x: 0,
        content_origin_y: 0,
    };
    apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
    rendered = trim_rendered_image_to_alpha_bounds(rendered, 1);
    Ok(rendered)
}

/// Draw one horizontal (unrotated) glyph into the output canvas.
///
/// Rasterizes the glyph's true font outline at exactly the pixels the bitmap
/// blit used: scaled about the bitmap center (`dst_center`), no rotation, world
/// mapped to the canvas by the `x_offset`/`y_offset` used by the bounds pass.
/// `pos_x`/`pos_y` are the glyph pen position in layout pixels (already carrying
/// the inline glyph offset). Color/emoji glyphs have no monochrome outline and
/// keep the bitmap blit (identity or center-scaled); ordinary empty glyphs
/// (zero-size placement) draw nothing. `aa_lut` is the coverage->alpha transfer
/// table applied only on the outline path; the bitmap fallback is unaffected.
// The blit call site naturally carries the raster target, glyph, pen position,
// scale, color, canvas offsets and the AA table; bundling them would obscure the mapping.
#[allow(clippy::too_many_arguments)]
fn draw_horizontal_glyph(
    rgba: &mut [u8],
    out_width: u32,
    out_height: u32,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    glyph: &LayoutGlyph,
    pos_x: f32,
    pos_y: f32,
    glyph_scale: GlyphScaleSettings,
    text_color: [u8; 4],
    x_offset: i32,
    y_offset: i32,
    aa_lut: &[u8; 256],
) {
    let physical = glyph.physical((pos_x, pos_y), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
        return;
    };
    let glyph_w = image.placement.width as usize;
    let glyph_h = image.placement.height as usize;
    if glyph_w == 0 || glyph_h == 0 {
        return;
    }
    let placement_left = image.placement.left as f32;
    let placement_top = image.placement.top as f32;
    // Content-space top-left of the (unscaled) glyph bitmap, matching the bounds
    // pass and the bitmap blit's `src_left`/`src_top`.
    let src_left = (physical.x + image.placement.left) as f32;
    let src_top = (physical.y - image.placement.top) as f32;

    // Prefer the true font outline: rasterize it at the exact world placement the
    // bitmap blit would have used (scale about the bitmap center, no rotation).
    if let Some(outline) = resolve_outline_for_glyph(font_system, outline_cache, glyph) {
        let dst_center_x = src_left + glyph_w as f32 * 0.5;
        let dst_center_y = src_top + glyph_h as f32 * 0.5;
        // Re-add the subpixel fraction cosmic-text baked into the bitmap coverage
        // (physical.x/y carry only the integer pen), so the outline matches it.
        let transform = glyph_outline_transform(
            dst_center_x,
            dst_center_y,
            0.0,
            placement_left,
            placement_top,
            glyph_w as f32,
            glyph_h as f32,
            glyph_scale.width_mul,
            glyph_scale.height_mul,
            glyph_subpixel_offset(physical.cache_key),
        );
        rasterize_outline_into(
            rgba,
            out_width as usize,
            out_height as usize,
            -(x_offset as f32),
            -(y_offset as f32),
            &outline,
            &transform,
            text_color,
            aa_lut,
        );
        return;
    }

    // No fillable outline (real color glyph, or a monochrome embedded-bitmap /
    // sbix / CBDT-mono glyph): blit whatever non-empty bitmap `get_image` gave us
    // — spaces are already filtered by the zero-size check above. Dropping this on
    // a non-color glyph would silently lose embedded-bitmap-only glyphs.
    let draw_x = physical.x + image.placement.left + x_offset;
    let draw_y = physical.y - image.placement.top + y_offset;
    if glyph_scale.is_identity() {
        rasterize_unscaled_glyph(
            rgba,
            out_width,
            out_height,
            image.content,
            image.data.as_slice(),
            glyph_w,
            glyph_h,
            draw_x,
            draw_y,
            text_color,
        );
    } else {
        let glyph_rgba =
            build_glyph_rgba_buffer(&image.content, image.data.as_slice(), glyph_w, glyph_h, text_color);
        let mut canvas = RgbaCanvasView {
            rgba,
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
            glyph_scale,
        );
    }
}

/// Одно размещение глифа для пути с поворотами: векторный outline (когда есть),
/// исходный bitmap для fallback цветных глифов, масштаб, placement, центр в
/// координатах контента, итоговый поворот и принадлежность к группе.
///
/// `outline` is the glyph's true font outline; when present the draw pass
/// rasterizes it and `glyph_rgba` stays empty. `glyph_rgba` backs the bitmap
/// fallback for any outline-less glyph (real color glyph or a monochrome
/// embedded-bitmap glyph), built only when the outline is absent.
/// `placement_left`/`placement_top` feed the outline->world pivot.
struct RotatedGlyphPlacement {
    outline: Option<Arc<Outline>>,
    glyph_rgba: Vec<u8>,
    glyph_w: usize,
    glyph_h: usize,
    src_left: f32,
    src_top: f32,
    placement_left: f32,
    placement_top: f32,
    scale: GlyphScaleSettings,
    text_color: [u8; 4],
    /// Subpixel fraction baked into the swash bitmap coverage; re-applied to the
    /// outline placement only (the bitmap fallback already carries it).
    subpixel: [f32; 2],
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
    group_key: Option<(usize, usize)>,
    group_rotation_rad: f32,
}

#[allow(clippy::too_many_arguments)]
fn build_rotated_placement(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    glyph: &LayoutGlyph,
    pos_x: f32,
    pos_y: f32,
    scale: GlyphScaleSettings,
    text_color: [u8; 4],
    glyph_rotation_rad: f32,
    group_key: Option<(usize, usize)>,
    group_rotation_rad: f32,
) -> Option<RotatedGlyphPlacement> {
    let physical = glyph.physical((pos_x, pos_y), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
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
    let outline = resolve_outline_for_glyph(font_system, outline_cache, glyph);
    // Build the bitmap RGBA for any outline-less glyph fallback: real color glyphs
    // and monochrome embedded-bitmap / sbix / CBDT-mono glyphs alike (the zero-size
    // skip above already filtered spaces).
    let glyph_rgba = if outline.is_none() {
        build_glyph_rgba_buffer(&image.content, image.data.as_slice(), glyph_w, glyph_h, text_color)
    } else {
        Vec::new()
    };
    let (scaled_left, scaled_top, scaled_width, scaled_height) =
        scale.scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
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
        center_x: scaled_left + scaled_width * 0.5,
        center_y: scaled_top + scaled_height * 0.5,
        rotation_rad: glyph_rotation_rad,
        group_key,
        group_rotation_rad,
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

/// Горизонтальный рендер обычного текста с inline-поворотами смещений.
/// Собирает размещения всех глифов, применяет повороты групп, считает повёрнутый
/// bbox и выводит каждый глиф обратной выборкой с поворотом.
#[allow(clippy::too_many_arguments)]
fn render_horizontal_rotated(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &Buffer,
    attrs: &Attrs<'_>,
    inline_font_registry: &super::font_registry::InlineFontRegistry,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    layout_text: &str,
    layout_line_offsets: &[usize],
    line_baselines: &[f32],
    width_px: u32,
    font_size_px: f32,
    line_height_px: f32,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let mut cache = SwashCache::new();
    // Per-render outline cache: each glyph outline is extracted at most once.
    let mut outline_cache = OutlineCache::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    let mut placements: Vec<RotatedGlyphPlacement> = Vec::new();
    let inline_line_aligns =
        compute_inline_line_aligns(params.align, layout_text, inline_style_spans);
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();

    while let Some(run) = runs.next() {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        let run_layout = horizontal_run_layout(
            params,
            &run,
            font_system,
            &mut cache,
            layout_line_offsets,
            inline_style_spans,
            font_size_px,
        );
        let line_offset_x = horizontal_line_offset(
            width_px,
            if params.hanging_punctuation {
                run_layout.visual_width_px
            } else {
                run_layout.line_width_px
            },
            inline_line_aligns
                .get(line_idx)
                .copied()
                .unwrap_or(params.align),
        ) as f32
            - if params.hanging_punctuation {
                run_layout.leading_hang_px
            } else {
                0.0
            };
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or(run.line_y);

        for (glyph, glyph_x) in run.glyphs.iter().zip(run_layout.glyph_xs.iter().copied()) {
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
            if let Some(placement) = build_rotated_placement(
                font_system,
                &mut cache,
                &mut outline_cache,
                glyph,
                line_offset_x + (glyph_x - glyph.x) + offset.global_px[0],
                baseline_y + offset.global_px[1],
                glyph_scale,
                glyph_text_color,
                offset.glyph_rotation_rad,
                group_key,
                offset.group_rotation_rad,
            ) {
                placements.push(placement);
            }
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = build_wrapped_hyphen_glyph(
                font_system,
                attrs,
                inline_style_spans,
                inline_font_registry,
                layout_line_offsets,
                &run,
                runs.peek(),
                font_size_px,
                font_size_px,
            )
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
                &mut cache,
                &mut outline_cache,
                &hyphen_glyph,
                hyphen_offset_x + hyphen_offset.global_px[0],
                baseline_y + hyphen_offset.global_px[1],
                hyphen_scale,
                hyphen_text_color,
                hyphen_offset.glyph_rotation_rad,
                group_key,
                hyphen_offset.group_rotation_rad,
            ) {
                placements.push(placement);
            }
        }
        line_idx += 1;
    }

    apply_rotated_group_rotations(&mut placements);

    let mut bounds = PixelBounds::empty();
    for placement in &placements {
        let (scaled_left, scaled_top, scaled_width, scaled_height) = placement.scale.scaled_rect(
            placement.src_left,
            placement.src_top,
            placement.glyph_w as f32,
            placement.glyph_h as f32,
        );
        include_rotated_rect_bounds(
            &mut bounds,
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            placement.center_x,
            placement.center_y,
            placement.rotation_rad,
        );
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
                placement.center_x,
                placement.center_y,
                placement.rotation_rad,
                placement.placement_left,
                placement.placement_top,
                placement.glyph_w as f32,
                placement.glyph_h as f32,
                placement.scale.width_mul,
                placement.scale.height_mul,
                placement.subpixel,
            );
            rasterize_outline_into(
                rgba.as_mut_slice(),
                out_width as usize,
                out_height as usize,
                -(x_offset as f32),
                -(y_offset as f32),
                outline,
                &transform,
                placement.text_color,
                &aa_lut,
            );
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
            GlyphRgbaView {
                rgba: placement.glyph_rgba.as_slice(),
                width: placement.glyph_w,
                height: placement.glyph_h,
            },
            placement.src_left,
            placement.src_top,
            placement.scale,
            placement.center_x,
            placement.center_y,
            placement.rotation_rad,
            x_offset,
            y_offset,
        );
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

pub fn smoke_render_text_to_image(params: &TextRenderParams) -> Result<RenderedTextImage, String> {
    if params.width_px == 0 {
        return Err("render_next smoke pipeline requires width_px > 0".to_string());
    }

    let mut image =
        RenderedTextImage::transparent(params.width_px, estimate_placeholder_height(params));
    image.warnings.push(
        "render_next placeholder pipeline is active; full raster path is not migrated yet"
            .to_string(),
    );
    Ok(image)
}

// The glyph layout routine needs the rendered glyph run, inline overrides and cached raster
// metrics together to preserve metric positioning with optional custom spacing.
#[allow(clippy::too_many_arguments)]
fn horizontal_run_layout(
    params: &TextRenderParams,
    run: &LayoutRun<'_>,
    _font_system: &mut FontSystem,
    _cache: &mut SwashCache,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
    font_size_px: f32,
) -> HorizontalRunLayout {
    if run.glyphs.is_empty() {
        return HorizontalRunLayout {
            glyph_xs: Vec::new(),
            line_width_px: run.line_w,
            visual_width_px: run.line_w,
            leading_hang_px: 0.0,
        };
    }

    let glyph_kernings = run
        .glyphs
        .iter()
        .map(|glyph| {
            inline_kerning_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            )
        })
        .collect::<Vec<_>>();

    if glyph_kernings
        .iter()
        .all(|kerning| kerning.uses_default_metric_layout())
    {
        let glyph_xs = run.glyphs.iter().map(|glyph| glyph.x).collect::<Vec<_>>();
        let (visual_width_px, leading_hang_px) =
            hanging_metrics_for_layout(run, glyph_xs.as_slice(), run.line_w);
        return HorizontalRunLayout {
            glyph_xs,
            line_width_px: run.line_w,
            visual_width_px,
            leading_hang_px,
        };
    }

    let mut glyph_xs = Vec::with_capacity(run.glyphs.len());
    let mut current_x = run.glyphs.first().map(|glyph| glyph.x).unwrap_or(0.0);
    glyph_xs.push(current_x);
    let default_advance = font_size_px.max(1.0) * 0.5;

    for (idx, pair_kerning) in glyph_kernings
        .iter()
        .copied()
        .enumerate()
        .take(run.glyphs.len())
        .skip(1)
    {
        let prev = &run.glyphs[idx - 1];
        let glyph = &run.glyphs[idx];
        let metric_advance = glyph.x - prev.x;
        let spacing_basis = metric_advance.abs().max(prev.w.max(default_advance));
        current_x += metric_advance + pair_kerning.extra_spacing_px(spacing_basis);
        glyph_xs.push(current_x);
    }

    let line_width_px = run
        .glyphs
        .iter()
        .zip(glyph_xs.iter().copied())
        .map(|(glyph, glyph_x)| glyph_x + glyph.w)
        .fold(0.0, f32::max);
    let (visual_width_px, leading_hang_px) =
        hanging_metrics_for_layout(run, glyph_xs.as_slice(), line_width_px);
    HorizontalRunLayout {
        glyph_xs,
        line_width_px,
        visual_width_px,
        leading_hang_px,
    }
}

fn prepare_source_text(source_text: &str, params: &TextRenderParams) -> String {
    let source_text = if params.uppercase_text {
        source_text.to_uppercase()
    } else {
        source_text.to_string()
    };
    let source_text = if params.trim_extra_spaces {
        trim_extra_spaces(source_text.as_str())
    } else {
        source_text
    };
    let source_text = if params.new_line_after_sentence {
        apply_sentence_newlines(source_text.as_str())
    } else {
        source_text
    };
    if source_text.is_empty() {
        " ".to_string()
    } else {
        source_text
    }
}

pub(crate) fn effective_spacing_percent(base_percent: f32, glyph_percent: f32) -> f32 {
    (base_percent + (glyph_percent - 100.0)).clamp(-300.0, 300.0)
}

fn trim_extra_spaces(text: &str) -> String {
    fn is_trimmable_space(ch: char) -> bool {
        matches!(ch, ' ' | '\t' | '\r')
    }

    text.trim_matches(is_trimmable_space)
        .split('\n')
        .map(|line| line.trim_matches(is_trimmable_space))
        .collect::<Vec<_>>()
        .join("\n")
}

fn justify_alignment_option(align: HorizontalAlign) -> Option<Align> {
    if align.justify {
        Some(Align::Justified)
    } else {
        None
    }
}

fn compute_inline_line_aligns(
    base_align: HorizontalAlign,
    layout_text: &str,
    spans: Option<&[InlineStyleSpan]>,
) -> Vec<HorizontalAlign> {
    let line_offsets = compute_layout_line_offsets(layout_text);
    let Some(spans) = spans else {
        return vec![base_align; line_offsets.len()];
    };
    line_offsets
        .iter()
        .map(|offset| {
            inline_style_at_offset(spans, *offset)
                .and_then(|span| span.align)
                .unwrap_or(base_align)
        })
        .collect()
}

fn apply_line_aligns_to_buffer(
    buffer: &mut Buffer,
    line_aligns: &[HorizontalAlign],
) {
    for (idx, line) in buffer.lines.iter_mut().enumerate() {
        let cosmic_align = line_aligns
            .get(idx)
            .and_then(|align| justify_alignment_option(*align));
        line.set_align(cosmic_align);
    }
}

pub(crate) fn horizontal_line_offset(
    width_px: u32,
    line_width: f32,
    align: HorizontalAlign,
) -> i32 {
    // Свободное выравнивание растягивает строки до полной ширины, поэтому начинаем
    // от левого края (смещение 0); прочие случаи позиционируются по `bias`.
    if align.justify {
        return 0;
    }
    let free = width_px as f32 - line_width;
    (free * align.offset_fraction()).round() as i32
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

fn inline_style_at_offset(spans: &[InlineStyleSpan], offset: usize) -> Option<&InlineStyleSpan> {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end)
}

fn inline_text_color_at_offset(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> [u8; 4] {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.text_color)
        .unwrap_or(default_text_color)
}

pub(crate) fn inline_text_color_for_glyph(
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

pub(crate) fn inline_glyph_offset_style_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> InlineGlyphOffset {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.glyph_offset)
        .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0]))
}

pub(crate) fn inline_glyph_offset_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [f32; 2] {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

fn inline_glyph_offset_style_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> InlineGlyphOffset {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_style_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

/// Диапазон inline-спана, задающего смещение для глифа, — ключ для группировки
/// глифов, поворачиваемых как единая группа.
fn inline_glyph_offset_span_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<(usize, usize)> {
    spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .filter(|style| style.glyph_offset.is_some())
        .map(|style| (style.start, style.end))
}

fn inline_glyph_offset_span_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> Option<(usize, usize)> {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_glyph_offset_span_at_offset(spans, line_offset + glyph.start.min(glyph.end))
}

/// Есть ли среди inline-спанов смещения с ненулевым поворотом (группы или символа).
fn spans_have_inline_rotation(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(|span| {
        span.glyph_offset.is_some_and(|offset| {
            offset.group_rotation_rad.abs() > f32::EPSILON
                || offset.glyph_rotation_rad.abs() > f32::EPSILON
        })
    })
}

fn inline_glyph_scale_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> GlyphScaleSettings {
    let stretch = spans
        .and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        .and_then(|style| style.glyph_stretch_percent)
        .unwrap_or([params.glyph_width_percent, params.glyph_height_percent]);
    GlyphScaleSettings {
        width_mul: (stretch[0] / 100.0).clamp(0.01, 3.0),
        height_mul: (stretch[1] / 100.0).clamp(0.01, 3.0),
    }
}

pub(crate) fn inline_glyph_scale_for_glyph(
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
    let style = spans.and_then(|style_spans| inline_style_at_offset(style_spans, offset));
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

pub(crate) fn inline_kerning_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> KerningSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_kerning_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}

pub(crate) fn compute_line_extra_spacing_table(
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

pub(crate) fn compute_horizontal_line_baselines(
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

#[allow(clippy::too_many_arguments)]
fn build_wrapped_hyphen_glyph(
    font_system: &mut FontSystem,
    base_attrs: &Attrs<'_>,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &super::font_registry::InlineFontRegistry,
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
    inline_font_registry: &super::font_registry::InlineFontRegistry,
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

fn apply_sentence_newlines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut after_sentence_end = false;
    let mut pending_spaces = String::new();

    for ch in text.chars() {
        if matches!(ch, '.' | '?' | '!') {
            result.push_str(&pending_spaces);
            pending_spaces.clear();
            result.push(ch);
            after_sentence_end = true;
        } else if after_sentence_end {
            if ch.is_alphabetic() {
                pending_spaces.clear();
                result.push('\n');
                result.push(ch);
                after_sentence_end = false;
            } else if ch == '\n' {
                pending_spaces.clear();
                result.push(ch);
                after_sentence_end = false;
            } else if ch == ' ' || ch == '\t' {
                pending_spaces.push(ch);
            } else {
                result.push_str(&pending_spaces);
                pending_spaces.clear();
                result.push(ch);
                after_sentence_end = false;
            }
        } else {
            result.push(ch);
        }
    }
    result.push_str(&pending_spaces);
    result
}

fn is_hanging_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ','
            | '!'
            | '?'
            | ':'
            | ';'
            | '-'
            | '–'
            | '—'
            | '~'
            | '…'
            | '·'
            | '•'
            | '。'
            | '、'
            | '，'
            | '．'
            | '！'
            | '？'
            | '：'
            | '；'
            | '・'
            | '･'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '\''
            | '«'
            | '»'
            | '\u{201C}'
            | '\u{201D}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{201E}'
            | '\u{201F}'
            | '\u{201A}'
    )
}

fn hanging_metrics_for_layout(
    run: &LayoutRun<'_>,
    glyph_xs: &[f32],
    line_width_px: f32,
) -> (f32, f32) {
    if run.glyphs.is_empty() || glyph_xs.len() != run.glyphs.len() {
        return (line_width_px.max(0.0), 0.0);
    }

    let mut left_boundary = glyph_xs.first().copied().unwrap_or(0.0);
    let mut right_boundary = line_width_px;
    let mut saw_non_hanging = false;

    for (glyph, glyph_x) in run.glyphs.iter().zip(glyph_xs.iter().copied()) {
        if glyph_is_hanging_punctuation(run.text, glyph) {
            if !saw_non_hanging {
                left_boundary = glyph_x + glyph.w;
            }
            continue;
        }
        left_boundary = glyph_x;
        saw_non_hanging = true;
        break;
    }

    if saw_non_hanging {
        for (glyph, glyph_x) in run.glyphs.iter().zip(glyph_xs.iter().copied()).rev() {
            if glyph_is_hanging_punctuation(run.text, glyph) {
                continue;
            }
            right_boundary = glyph_x + glyph.w;
            break;
        }
    } else {
        left_boundary = glyph_xs.first().copied().unwrap_or(0.0);
        right_boundary = line_width_px;
    }

    let left_edge = glyph_xs.first().copied().unwrap_or(0.0);
    let leading_hang_px = (left_boundary - left_edge).max(0.0);
    let trailing_hang_px = (line_width_px - right_boundary).max(0.0);
    let visual_width_px = (line_width_px - leading_hang_px - trailing_hang_px).max(0.0);

    if visual_width_px <= f32::EPSILON {
        (line_width_px.max(0.0), 0.0)
    } else {
        (visual_width_px, leading_hang_px)
    }
}

fn glyph_is_hanging_punctuation(text: &str, glyph: &LayoutGlyph) -> bool {
    let end = glyph.end.min(text.len());
    let start = glyph.start.min(end);
    let Some(slice) = text.get(start..end) else {
        return false;
    };
    !slice.is_empty() && slice.chars().all(is_hanging_punctuation)
}

#[cfg(test)]
mod tests {
    use super::{apply_effects_to_image, render_text_to_image};
    use crate::tabs::typing::render_next::types::{
        AntiAliasingMode, HorizontalAlign, KerningMode, TextDrawnLinesLayoutParams,
        TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams,
        TextRenderShapeCompareParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
        TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
        VerticalLineDirection,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    fn base_params() -> TextRenderParams {
        TextRenderParams {
            text: "Hello world".to_string(),
            text_color: [255, 255, 255, 255],
            font_path: test_font_path(),
            available_inline_fonts: Vec::new(),
            font_size_px: 36.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 100.0,
            kerning_mode: KerningMode::Metric,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 256,
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
            text_line_mode: TextLineMode::Horizontal,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            effects_json: String::new(),
            // Identity transfer so existing raster/geometry assertions in these
            // tests keep matching the pre-AA coverage exactly.
            anti_aliasing: AntiAliasingMode::Smooth,
        }
    }

    fn alpha_bounds_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<(usize, usize)> {
        let width = width as usize;
        let height = height as usize;
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut found = false;
        for y in 0..height {
            for x in 0..width {
                if rgba[(y * width + x) * 4 + 3] == 0 {
                    continue;
                }
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                found = true;
            }
        }
        found.then_some((
            max_x.saturating_sub(min_x).saturating_add(1),
            max_y.saturating_sub(min_y).saturating_add(1),
        ))
    }

    fn alpha_centroid_y(width: u32, height: u32, rgba: &[u8]) -> Option<f32> {
        let width = width as usize;
        let height = height as usize;
        let mut weighted_y = 0.0f32;
        let mut alpha_sum = 0.0f32;
        for y in 0..height {
            for x in 0..width {
                let alpha = f32::from(rgba[(y * width + x) * 4 + 3]);
                if alpha <= 0.0 {
                    continue;
                }
                weighted_y += y as f32 * alpha;
                alpha_sum += alpha;
            }
        }
        (alpha_sum > 0.0).then_some(weighted_y / alpha_sum)
    }

    #[test]
    fn base_pipeline_renders_non_empty_plain_text() {
        let rendered = render_text_to_image(&base_params(), None).unwrap_or_else(|error| {
            panic!("base render_next pipeline should render text: {error}")
        });
        assert!(rendered.width > 0);
        assert!(rendered.height > 0);
        assert!(rendered.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn vertical_step_follows_glyph_ink_height() {
        let mut params = base_params();
        params.text_line_mode = TextLineMode::Vertical;

        // Низкие глифы (точки у базовой линии) укладываются плотно по своей ink-высоте.
        params.text = "...".to_string();
        let dots = render_text_to_image(&params, None).expect("vertical dots render");
        let (_, dots_h) =
            alpha_bounds_from_rgba(dots.width, dots.height, &dots.rgba).expect("dots bounds");

        // Высокие глифы занимают по вертикали заметно больше.
        params.text = "III".to_string();
        let bars = render_text_to_image(&params, None).expect("vertical bars render");
        let (_, bars_h) =
            alpha_bounds_from_rgba(bars.width, bars.height, &bars.rgba).expect("bars bounds");

        // При старом «em на символ» обе высоты были бы почти равны; при шаге по
        // ink-высоте высокие глифы тянутся значительно дальше низких.
        assert!(
            bars_h > dots_h * 2,
            "vertical step should track ink height: III={bars_h} vs ...={dots_h}"
        );
    }

    #[test]
    fn inline_group_rotation_rotates_text_block() {
        let mut params = base_params();
        params.enable_inline_style_tags = true;

        params.text = "Hello world".to_string();
        let plain = render_text_to_image(&params, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");

        // Поворот всей строки на 90° через машиночитаемый тег делает блок высоким и узким.
        params.text = "<m g=90>Hello world</m>".to_string();
        let rotated = render_text_to_image(&params, None).expect("rotated render");
        assert!(
            rotated.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0),
            "rotated block must render visible pixels"
        );
        let (rotated_w, rotated_h) = alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
            .expect("rotated bounds");
        assert!(
            rotated_h > plain_h,
            "90°-rotated block should be taller: {rotated_h} vs {plain_h}"
        );
        assert!(
            rotated_w < plain_w,
            "90°-rotated block should be narrower: {rotated_w} vs {plain_w}"
        );
    }

    #[test]
    fn inline_glyph_rotation_flips_tall_glyph_bounds() {
        let mut params = base_params();
        params.enable_inline_style_tags = true;

        // Заглавная «I» — высокая и узкая.
        params.text = "I".to_string();
        let plain = render_text_to_image(&params, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");
        assert!(plain_h > plain_w, "capital I should be tall and narrow");

        // Повёрнутая на 90° «I» становится низкой и широкой.
        params.text = "<m r=90>I</m>".to_string();
        let rotated = render_text_to_image(&params, None).expect("glyph-rotated render");
        let (rotated_w, rotated_h) = alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
            .expect("rotated bounds");
        assert!(
            rotated_w > rotated_h,
            "90°-rotated I should be wide and short: {rotated_w}x{rotated_h}"
        );
    }

    #[test]
    fn shape_compare_warns_when_layout_text_is_unchanged() {
        let mut params = base_params();
        params.compare_shape_with = Some(TextRenderShapeCompareParams {
            width_px: params.width_px,
            text_wrap_mode: params.text_wrap_mode,
            shape_min_width_percent: params.shape_min_width_percent,
            shape_variant: params.shape_variant,
            cancel_render_if_layout_text_unchanged: false,
        });

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render unchanged shape compare case: {error}")
        });

        assert!(rendered.width > 0);
        assert!(
            rendered
                .warnings
                .iter()
                .any(|warning| warning == super::UNCHANGED_LAYOUT_TEXT_WARNING),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn shape_compare_can_skip_render_when_layout_text_is_unchanged() {
        let mut params = base_params();
        params.compare_shape_with = Some(TextRenderShapeCompareParams {
            width_px: params.width_px,
            text_wrap_mode: params.text_wrap_mode,
            shape_min_width_percent: params.shape_min_width_percent,
            shape_variant: params.shape_variant,
            cancel_render_if_layout_text_unchanged: true,
        });

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should skip unchanged shape compare case cleanly: {error}")
        });

        assert_eq!(rendered.width, 0);
        assert_eq!(rendered.height, 0);
        assert!(rendered.rgba.is_empty());
        assert!(
            rendered
                .warnings
                .iter()
                .any(|warning| warning == super::UNCHANGED_LAYOUT_TEXT_WARNING),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn base_pipeline_cancel_token_stops_render() {
        let token = Arc::new(AtomicU64::new(7));
        token.store(8, Ordering::Release);
        let error = render_text_to_image(&base_params(), Some((&token, 7)))
            .err()
            .unwrap_or_else(|| "missing cancel error".to_string());
        assert!(error.contains("cancelled"));
    }

    #[test]
    fn base_pipeline_renders_inline_font_size_text() {
        let params = base_params();
        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render baseline test case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn base_pipeline_renders_vertical_text() {
        let mut params = base_params();
        params.text = "вертикальный текст".to_string();
        params.text_line_mode = TextLineMode::Vertical;
        params.vertical_line_direction = VerticalLineDirection::RightToLeft;
        params.text_wrap_mode = TextWrapMode::WholeWords;
        params.width_px = 140;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render vertical baseline test case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn base_pipeline_renders_hanging_punctuation() {
        let mut params = base_params();
        params.text = "«Hello!»".to_string();
        params.align = HorizontalAlign::CENTER;
        params.hanging_punctuation = true;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render hanging punctuation case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn horizontal_center_alignment_keeps_overlong_line_centered() {
        assert_eq!(
            super::horizontal_line_offset(100, 140.0, HorizontalAlign::CENTER),
            -20
        );
    }

    #[test]
    fn base_pipeline_renders_soft_hyphen_wrap() {
        let mut params = base_params();
        params.text = "super\u{00AD}califragilistic".to_string();
        params.width_px = 110;
        params.text_wrap_mode = TextWrapMode::Moderate;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render soft-hyphen wrap case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn base_pipeline_keeps_inline_style_across_soft_hyphen_wrap() {
        let mut params = base_params();
        params.text = "<b>super\u{00AD}califragilistic</b>".to_string();
        params.enable_inline_style_tags = true;
        params.width_px = 110;
        params.text_wrap_mode = TextWrapMode::Moderate;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render inline soft-hyphen wrap case: {error}")
        });

        assert!(
            !rendered
                .warnings
                .iter()
                .any(|warning| warning.contains("inline style spans could not be remapped")),
            "{:?}",
            rendered.warnings
        );
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn base_pipeline_renders_inline_non_attrs_overrides() {
        let mut params = base_params();
        params.text =
            "<color=#ff0000><stretching=160,120><offset=4,-3>A</offset></stretching></color>\n<line-spacing=18,120><kerning=8,0>BC</kerning></line-spacing>"
                .to_string();
        params.enable_inline_style_tags = true;
        params.width_px = 180;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render inline non-attrs case: {error}")
        });

        assert!(
            !rendered
                .warnings
                .iter()
                .any(|warning| { warning.contains("currently apply only attrs-level overrides") }),
            "glyph-level inline override warning should disappear after implementation"
        );
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn base_pipeline_renders_sentence_newlines() {
        let mut params = base_params();
        params.text = "First sentence. Second sentence!".to_string();
        params.new_line_after_sentence = true;
        params.width_px = 320;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render sentence-newline case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn formula_pipeline_renders_curved_text() {
        let mut params = base_params();
        params.text = "FORMULA PATH".to_string();
        params.width_px = 320;
        params.text_layout_mode = TextLayoutMode::Formula;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "sin(t * tau) * 28".to_string(),
            rotation_expr: "rad(12) * sin(t * tau)".to_string(),
            use_tangent_rotation: true,
            offset_x_px: 0.0,
            offset_y_px: 42.0,
            ..TextFormulaLayoutParams::default()
        };

        let rendered = render_text_to_image(&params, None)
            .unwrap_or_else(|error| panic!("render_next should render formula test case: {error}"));
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn shape_formula_pipeline_keeps_fallback_warning_and_visible_alpha() {
        let mut params = base_params();
        params.text = "shape fallback path should stay readable".to_string();
        params.width_px = 280;
        params.text_layout_mode = TextLayoutMode::Shape;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * 24".to_string(),
            y_expr: "0".to_string(),
            ..TextFormulaLayoutParams::default()
        };

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render shape fallback test case: {error}")
        });

        assert!(
            rendered
                .warnings
                .iter()
                .any(|warning| warning.contains("Форма слишком узкая"))
        );
        assert!(
            alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
            "render_next should still produce visible alpha after shape fallback"
        );
    }

    #[test]
    fn drawn_lines_pipeline_renders_text_from_layout_image() {
        let mut layout_image = image::RgbaImage::new(260, 80);
        layout_image.put_pixel(8, 40, image::Rgba([255, 0, 0, 255]));
        for x in 9..240 {
            layout_image.put_pixel(x, 40, image::Rgba([255, 0, 0, 128]));
        }
        let layout_path = std::env::temp_dir().join(format!(
            "manhwastudio_drawn_lines_test_{}.png",
            std::process::id()
        ));
        layout_image
            .save(&layout_path)
            .unwrap_or_else(|error| panic!("should write drawn-lines layout image: {error}"));

        let mut params = base_params();
        params.text = "DRAWN".to_string();
        params.width_px = 260;
        params.text_layout_mode = TextLayoutMode::CustomRasterLines;
        params.drawn_lines_layout = TextDrawnLinesLayoutParams {
            image_path: Some(layout_path.clone()),
            ..TextDrawnLinesLayoutParams::default()
        };

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render drawn-lines test case: {error}")
        });
        let _ = std::fs::remove_file(layout_path);
        assert!(
            alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
            "drawn-lines render should produce visible alpha: warnings={:?}",
            rendered.warnings
        );
    }

    #[test]
    fn vector_lines_pipeline_renders_text_from_points() {
        let mut params = base_params();
        params.text = "VECTOR".to_string();
        params.width_px = 260;
        params.text_layout_mode = TextLayoutMode::CustomVectorLines;
        params.vector_lines_layout = TextVectorLinesLayoutParams {
            width_px: 260,
            height_px: 80,
            lines: vec![TextVectorLine {
                points: vec![
                    TextVectorPoint { x: 8.0, y: 40.0 },
                    TextVectorPoint { x: 120.0, y: 20.0 },
                    TextVectorPoint { x: 240.0, y: 40.0 },
                ],
                corner_smoothing_px: 16.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render vector-lines test case: {error}")
        });
        assert!(
            alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
            "vector-lines render should produce visible alpha: warnings={:?}",
            rendered.warnings
        );
    }

    #[test]
    fn vector_lines_pipeline_applies_inline_glyph_offset() {
        let mut base = base_params();
        base.text = "A".to_string();
        base.width_px = 120;
        base.text_layout_mode = TextLayoutMode::CustomVectorLines;
        base.vector_lines_layout = TextVectorLinesLayoutParams {
            width_px: 120,
            height_px: 90,
            use_tangent_rotation: false,
            lines: vec![TextVectorLine {
                points: vec![
                    TextVectorPoint { x: 20.0, y: 30.0 },
                    TextVectorPoint { x: 100.0, y: 30.0 },
                ],
                corner_smoothing_px: 0.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };

        let without_offset = render_text_to_image(&base, None).unwrap_or_else(|error| {
            panic!("render_next should render vector-lines offset baseline: {error}")
        });
        let mut with_offset = base;
        with_offset.text = "<offset=0,24>A</offset>".to_string();
        with_offset.enable_inline_style_tags = true;
        let with_offset = render_text_to_image(&with_offset, None).unwrap_or_else(|error| {
            panic!("render_next should render vector-lines inline offset: {error}")
        });

        let baseline_y = alpha_centroid_y(
            without_offset.width,
            without_offset.height,
            &without_offset.rgba,
        )
        .unwrap_or_else(|| panic!("baseline vector-lines render should have alpha"));
        let offset_y = alpha_centroid_y(with_offset.width, with_offset.height, &with_offset.rgba)
            .unwrap_or_else(|| panic!("offset vector-lines render should have alpha"));
        assert!(
            offset_y > baseline_y + 12.0,
            "inline Y offset should move vector-line glyph down: baseline={baseline_y}, offset={offset_y}"
        );
    }

    #[test]
    fn apply_effects_to_image_without_effects_returns_unchanged() {
        let rgba = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let result = apply_effects_to_image(rgba.clone(), 2, 1, "", None)
            .unwrap_or_else(|error| panic!("empty effects should pass image through: {error}"));
        assert_eq!(result.width, 2);
        assert_eq!(result.height, 1);
        assert_eq!(result.rgba, rgba);

        // Пустой JSON-массив эффектов тоже означает «без эффектов».
        let result_empty_array = apply_effects_to_image(rgba.clone(), 2, 1, "[]", None)
            .unwrap_or_else(|error| panic!("empty effects array should pass through: {error}"));
        assert_eq!(result_empty_array.rgba, rgba);
    }

    #[test]
    fn apply_effects_to_image_rejects_mismatched_buffer() {
        let result = apply_effects_to_image(vec![0u8; 7], 2, 1, "", None);
        assert!(
            result.is_err(),
            "buffer length not matching width*height*4 must be an error"
        );
    }

    #[test]
    fn apply_effects_to_image_stroke_grows_canvas() {
        // Сплошной непрозрачный квадрат + обводка должны увеличить холст под запас контура.
        let width = 8u32;
        let height = 8u32;
        let rgba = vec![255u8; (width * height * 4) as usize];
        let effects_json = r#"[{"effect":"stroke","width_px":4,"color":[0,0,0,255]}]"#;
        let result = apply_effects_to_image(rgba, width, height, effects_json, None)
            .unwrap_or_else(|error| panic!("stroke effect should apply to image: {error}"));
        assert!(
            result.width >= width && result.height >= height,
            "stroke should not shrink the canvas: got {}x{}",
            result.width,
            result.height
        );
        assert_eq!(
            result.rgba.len(),
            (result.width * result.height * 4) as usize,
            "RGBA buffer must stay width*height*4 after effects"
        );
    }
}
