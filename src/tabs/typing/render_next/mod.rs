/*
File: src/tabs/typing/render_next/mod.rs

Purpose:
Каркас нового рендера вкладки typing.

Main responsibilities:
- публиковать совместимый внешний контракт через `types.rs`;
- собирать внутренние подсистемы pipeline/font registry/raster/effects/wrap/layout/formula.

Public surface:
- публичны подмодули `types` и `pipeline`;
- совместимый контракт лежит в `render_next::types::*`;
- re-export точки входа лежит в `render_next::render_text_to_image`;
- временный smoke path лежит в `render_next::pipeline::smoke_render_text_to_image`;
- `inline_styles`, `font_registry`, `raster`, `effects`, `wrap`, `layout`, `formula`
  остаются внутренними деталями нового рендера.
*/

use std::path::PathBuf;

pub(crate) mod drawn_lines;
mod effects;
mod font_registry;
mod formula;
mod inline_styles;
mod layout;
pub mod pipeline;
mod raster;
pub mod types;
mod wrap;

// Общая логика дискретных форм текста живёт в подсистеме wrap и используется
// как панелью typing, так и рендером (см. `wrap::forms`).
pub(crate) use wrap::forms;

// Резолвер PostScript-имени шрифта переиспользуется PSD-экспортом вкладки typing.
pub(crate) use font_registry::{
    load_selected_font_from_path, resolve_font_family_name, resolve_font_postscript_name,
};

type RenderNextCancel<'a> = Option<(&'a std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>;

// Compile-time smoke anchor for the staged API: keeps the extracted contract wired
// together in non-test builds without touching the current production renderer.
const _: usize = std::mem::size_of::<types::TextRenderParams>()
    + std::mem::size_of::<types::InlineFontEntry>()
    + std::mem::size_of::<types::RenderedTextImage>()
    + std::mem::size_of::<types::HorizontalAlign>()
    + std::mem::size_of::<types::KerningMode>()
    + std::mem::size_of::<types::TextShape>()
    + std::mem::size_of::<types::TextWrapMode>()
    + std::mem::size_of::<types::TextLineMode>()
    + std::mem::size_of::<types::VerticalLineDirection>()
    + std::mem::size_of::<types::TextLayoutMode>()
    + std::mem::size_of::<types::TextFormulaLayoutParams>()
    + std::mem::size_of::<types::TextDrawnLinesLayoutParams>()
    + std::mem::size_of::<types::TextVectorLinesLayoutParams>()
    + std::mem::size_of::<types::TextVectorLine>()
    + std::mem::size_of::<types::TextVectorPoint>()
    + types::TEXT_FORMULA_USER_VAR_COUNT;

const _: fn(&types::TextRenderParams) -> Result<types::RenderedTextImage, String> =
    pipeline::smoke_render_text_to_image;

const _: for<'a> fn(
    &types::TextRenderParams,
    RenderNextCancel<'a>,
) -> Result<types::RenderedTextImage, String> = pipeline::render_text_to_image;

const _: fn(u32, u32) -> types::RenderedTextImage = types::RenderedTextImage::transparent;
pub use pipeline::render_text_to_image;

// Anchor: переиспользование post-effects pipeline на сторонних RGBA-картинках (image-оверлеи).
#[allow(clippy::type_complexity)]
const _: for<'a> fn(
    Vec<u8>,
    u32,
    u32,
    &str,
    RenderNextCancel<'a>,
) -> Result<types::RenderedTextImage, String> = pipeline::apply_effects_to_image;
pub use pipeline::apply_effects_to_image;

// Anchor: рендер-подсистема владеет общим выбором формы и предикатами форм
// поверх scored-wrap (см. `wrap::forms`).
const _: fn(&str, forms::TextFormPreset, usize) -> Option<Vec<String>> = forms::choose_form;
const _: fn(&[u32], forms::TextFormPreset, u32) -> bool = forms::sequence_matches;
const _: fn(&[u32], u32) -> bool = forms::is_christmas_tree;

pub(crate) fn touch_runtime_smoke_contract() {
    formula::touch_formula_smoke_contract();

    let formula_layout = types::TextFormulaLayoutParams::default();
    let inline_font = types::InlineFontEntry {
        label: "render-next-smoke".to_string(),
        font_path: PathBuf::from("fonts/render-next-smoke.ttf"),
        face_index: 0,
    };
    let params = types::TextRenderParams {
        text: "render-next smoke".to_string(),
        text_color: [255, 255, 255, 255],
        font_path: PathBuf::from("fonts/render-next-smoke.ttf"),
        available_inline_fonts: vec![inline_font.clone()],
        font_size_px: 24.0,
        line_spacing_px: 28.0,
        line_spacing_percent: 100.0,
        kerning_mode: types::KerningMode::Metric,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: 128,
        align: types::HorizontalAlign::CENTER,
        selected_face_index: 0,
        force_bold: false,
        force_italic: false,
        uppercase_text: false,
        trim_extra_spaces: true,
        hanging_punctuation: false,
        new_line_after_sentence: false,
        enable_inline_style_tags: true,
        text_wrap_mode: types::TextWrapMode::WholeWords,
        text_shape: types::TextShape::Free,
        shape_min_width_percent: 100.0,
        shape_variant: 5,
        compare_shape_with: None,
        allow_moderate_trees: false,
        text_line_mode: types::TextLineMode::Horizontal,
        vertical_line_direction: types::VerticalLineDirection::RightToLeft,
        text_layout_mode: types::TextLayoutMode::Normal,
        formula_layout: formula_layout.clone(),
        drawn_lines_layout: types::TextDrawnLinesLayoutParams::default(),
        vector_lines_layout: types::TextVectorLinesLayoutParams::default(),
        effects_json: String::new(),
    };

    let image = match pipeline::smoke_render_text_to_image(&params) {
        Ok(image) => image,
        Err(error) => {
            panic!("render_next smoke contract failed to build placeholder image: {error}")
        }
    };

    std::hint::black_box((
        &params.text,
        params.text_color,
        &params.font_path,
        &params.available_inline_fonts,
        params.font_size_px,
        params.line_spacing_px,
        params.line_spacing_percent,
        params.kerning_mode,
        params.kerning_px,
        params.kerning_percent,
        params.glyph_height_percent,
        params.glyph_width_percent,
        params.width_px,
        params.align,
        params.selected_face_index,
        params.force_bold,
        params.force_italic,
        params.uppercase_text,
        params.trim_extra_spaces,
        params.hanging_punctuation,
        params.new_line_after_sentence,
        params.enable_inline_style_tags,
        params.text_wrap_mode,
        params.text_shape,
        params.shape_min_width_percent,
        &params.compare_shape_with,
        params.allow_moderate_trees,
        params.text_line_mode,
        params.vertical_line_direction,
        params.text_layout_mode,
        &params.formula_layout,
        &params.drawn_lines_layout,
        &params.vector_lines_layout,
        &params.effects_json,
    ));
    std::hint::black_box((
        &inline_font.label,
        &inline_font.font_path,
        inline_font.face_index,
    ));
    std::hint::black_box((image.width, image.height, &image.rgba, &image.warnings));
    std::hint::black_box((
        &formula_layout.x_expr,
        &formula_layout.y_expr,
        &formula_layout.rotation_expr,
        formula_layout.use_tangent_rotation,
        formula_layout.t_start,
        formula_layout.t_end,
        formula_layout.offset_x_px,
        formula_layout.offset_y_px,
        formula_layout.scale_x,
        formula_layout.scale_y,
        formula_layout.normal_offset_px,
        formula_layout.letter_spacing_mul,
        formula_layout.letter_spacing_px,
        formula_layout.vars,
        types::TEXT_FORMULA_USER_VAR_COUNT,
    ));
    std::hint::black_box([
        types::HorizontalAlign::LEFT,
        types::HorizontalAlign::CENTER,
        types::HorizontalAlign::RIGHT,
        types::HorizontalAlign::JUSTIFY,
    ]);
    std::hint::black_box([types::KerningMode::Metric, types::KerningMode::Optical]);
    std::hint::black_box([
        types::TextShape::Free,
        types::TextShape::Rectangle,
        types::TextShape::Oval,
        types::TextShape::Hexagon,
        types::TextShape::SoftPeak,
    ]);
    std::hint::black_box([
        types::TextWrapMode::None,
        types::TextWrapMode::WholeWords,
        types::TextWrapMode::Minimal,
        types::TextWrapMode::Moderate,
        types::TextWrapMode::Aggressive,
    ]);
    std::hint::black_box([
        types::TextLineMode::Horizontal,
        types::TextLineMode::Vertical,
    ]);
    std::hint::black_box([
        types::VerticalLineDirection::LeftToRight,
        types::VerticalLineDirection::RightToLeft,
    ]);
    std::hint::black_box([
        types::TextLayoutMode::Normal,
        types::TextLayoutMode::Formula,
        types::TextLayoutMode::Shape,
        types::TextLayoutMode::CustomRasterLines,
        types::TextLayoutMode::CustomVectorLines,
    ]);
    std::hint::black_box((
        &params.drawn_lines_layout.image_path,
        params.drawn_lines_layout.use_tangent_rotation,
        params.drawn_lines_layout.static_rotation_rad,
        params.drawn_lines_layout.normal_offset_px,
        params.drawn_lines_layout.letter_spacing_mul,
        params.drawn_lines_layout.letter_spacing_px,
        params.drawn_lines_layout.color_tolerance,
        params.drawn_lines_layout.continuation_alpha,
        params.drawn_lines_layout.start_alpha,
    ));
    std::hint::black_box((
        params.vector_lines_layout.width_px,
        params.vector_lines_layout.height_px,
        params.vector_lines_layout.use_tangent_rotation,
        params.vector_lines_layout.static_rotation_rad,
        params.vector_lines_layout.normal_offset_px,
        params.vector_lines_layout.letter_spacing_mul,
        params.vector_lines_layout.letter_spacing_px,
        &params.vector_lines_layout.lines,
    ));
}

#[cfg(test)]
mod tests {
    use super::{
        pipeline::smoke_render_text_to_image,
        touch_runtime_smoke_contract,
        types::{
            HorizontalAlign, InlineFontEntry, KerningMode, TextDrawnLinesLayoutParams,
            TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams, TextShape,
            TextVectorLinesLayoutParams, TextWrapMode, VerticalLineDirection,
        },
    };
    use std::path::PathBuf;

    #[test]
    fn smoke_pipeline_returns_transparent_placeholder_image() {
        touch_runtime_smoke_contract();

        let params = TextRenderParams {
            text: "Smoke".to_string(),
            text_color: [255, 255, 255, 255],
            font_path: PathBuf::from("fonts/test.ttf"),
            available_inline_fonts: vec![InlineFontEntry {
                label: "Test".to_string(),
                font_path: PathBuf::from("fonts/test-inline.ttf"),
                face_index: 0,
            }],
            font_size_px: 42.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 100.0,
            kerning_mode: KerningMode::Metric,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 320,
            align: HorizontalAlign::CENTER,
            selected_face_index: 0,
            force_bold: false,
            force_italic: false,
            uppercase_text: false,
            trim_extra_spaces: true,
            hanging_punctuation: false,
            new_line_after_sentence: false,
            enable_inline_style_tags: true,
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
        };

        let image = match smoke_render_text_to_image(&params) {
            Ok(image) => image,
            Err(error) => {
                panic!("smoke placeholder pipeline should produce an image, got error: {error}")
            }
        };

        assert_eq!(image.width, 320);
        assert!(image.height >= 1);
        let expected_rgba_len = usize::try_from(image.width).ok().and_then(|width_usize| {
            usize::try_from(image.height)
                .ok()
                .map(|height_usize| width_usize.saturating_mul(height_usize).saturating_mul(4))
        });
        assert_eq!(image.rgba.len(), expected_rgba_len.unwrap_or(0));
        assert!(
            image
                .warnings
                .iter()
                .any(|warning| warning.contains("placeholder")),
            "smoke pipeline should mark the output as placeholder"
        );
    }
}
