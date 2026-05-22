/*
File: src/tabs/typing/render_next/types.rs

Purpose:
Публичный контракт нового рендера вкладки typing, вынесенный отдельно от алгоритмов.

Main responsibilities:
- хранить совместимые имена и поля публичных типов из старого `render.rs`;
- изолировать внешний API от будущих внутренних подсистем pipeline/layout/raster;
- дать стабильную точку импорта для последующего переключения call-site.

Source compatibility:
- `TextRenderParams`
- `TextRenderShapeCompareParams`
- `InlineFontEntry`
- `RenderedTextImage`
- `HorizontalAlign`
- `KerningMode`
- `TextShape`
- `TextWrapMode`
- `TextLineMode`
- `VerticalLineDirection`
- `TextLayoutMode`
- `TextFormulaLayoutParams`
- `TextDrawnLinesLayoutParams`
- `TextVectorLinesLayoutParams`
- `TextVectorLineTextDirection`
- `TextVectorLineDistanceMode`
- `TEXT_FORMULA_USER_VAR_COUNT`
*/

use std::path::PathBuf;

pub const TEXT_FORMULA_USER_VAR_COUNT: usize = 8;

#[derive(Debug, Clone)]
pub struct TextRenderParams {
    pub text: String,
    pub text_color: [u8; 4],
    pub font_path: PathBuf,
    pub available_inline_fonts: Vec<InlineFontEntry>,
    pub font_size_px: f32,
    pub line_spacing_px: f32,
    pub line_spacing_percent: f32,
    pub kerning_mode: KerningMode,
    pub kerning_px: f32,
    pub kerning_percent: f32,
    pub glyph_height_percent: f32,
    pub glyph_width_percent: f32,
    pub width_px: u32,
    pub align: HorizontalAlign,
    pub selected_face_index: usize,
    pub force_bold: bool,
    pub force_italic: bool,
    pub uppercase_text: bool,
    pub trim_extra_spaces: bool,
    pub hanging_punctuation: bool,
    pub new_line_after_sentence: bool,
    pub enable_inline_style_tags: bool,
    pub text_wrap_mode: TextWrapMode,
    pub text_shape: TextShape,
    pub shape_min_width_percent: f32,
    pub shape_variant: u8,
    pub compare_shape_with: Option<TextRenderShapeCompareParams>,
    pub allow_moderate_trees: bool,
    pub text_line_mode: TextLineMode,
    pub vertical_line_direction: VerticalLineDirection,
    pub text_layout_mode: TextLayoutMode,
    pub formula_layout: TextFormulaLayoutParams,
    pub drawn_lines_layout: TextDrawnLinesLayoutParams,
    pub vector_lines_layout: TextVectorLinesLayoutParams,
    pub effects_json: String,
}

#[derive(Debug, Clone)]
pub struct TextRenderShapeCompareParams {
    pub width_px: u32,
    pub text_wrap_mode: TextWrapMode,
    pub shape_min_width_percent: f32,
    pub shape_variant: u8,
    pub cancel_render_if_layout_text_unchanged: bool,
}

#[derive(Debug, Clone)]
pub struct InlineFontEntry {
    pub label: String,
    pub font_path: PathBuf,
    pub face_index: usize,
}

#[derive(Debug, Clone)]
pub struct RenderedTextImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub warnings: Vec<String>,
}

impl RenderedTextImage {
    #[must_use]
    pub fn transparent(width: u32, height: u32) -> Self {
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|width_usize| {
                usize::try_from(height)
                    .ok()
                    .map(|height_usize| width_usize.saturating_mul(height_usize))
            })
            .unwrap_or(0);
        Self {
            width,
            height,
            rgba: vec![0; pixel_count.saturating_mul(4)],
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KerningMode {
    Metric,
    Optical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextShape {
    Free,
    Rectangle,
    Oval,
    Hexagon,
    SoftPeak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextWrapMode {
    None,
    WholeWords,
    Minimal,
    Moderate,
    Aggressive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextLineMode {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalLineDirection {
    LeftToRight,
    RightToLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextLayoutMode {
    Normal,
    Formula,
    Shape,
    CustomRasterLines,
    CustomVectorLines,
}

#[derive(Debug, Clone)]
pub struct TextFormulaLayoutParams {
    pub x_expr: String,
    pub y_expr: String,
    pub rotation_expr: String,
    pub use_tangent_rotation: bool,
    pub t_start: f32,
    pub t_end: f32,
    pub offset_x_px: f32,
    pub offset_y_px: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub normal_offset_px: f32,
    pub letter_spacing_mul: f32,
    pub letter_spacing_px: f32,
    pub vars: [f32; TEXT_FORMULA_USER_VAR_COUNT],
}

#[derive(Debug, Clone)]
pub struct TextDrawnLinesLayoutParams {
    pub image_path: Option<PathBuf>,
    pub use_tangent_rotation: bool,
    pub static_rotation_rad: f32,
    pub normal_offset_px: f32,
    pub letter_spacing_mul: f32,
    pub letter_spacing_px: f32,
    pub color_tolerance: u8,
    pub continuation_alpha: u8,
    pub start_alpha: u8,
}

#[derive(Debug, Clone)]
pub struct TextVectorLinesLayoutParams {
    pub width_px: u32,
    pub height_px: u32,
    pub use_tangent_rotation: bool,
    pub static_rotation_rad: f32,
    pub normal_offset_px: f32,
    pub letter_spacing_mul: f32,
    pub letter_spacing_px: f32,
    pub lines: Vec<TextVectorLine>,
}

#[derive(Debug, Clone)]
pub struct TextVectorLine {
    pub points: Vec<TextVectorPoint>,
    pub corner_smoothing_px: f32,
    pub text_direction: TextVectorLineTextDirection,
    pub distance_mode: TextVectorLineDistanceMode,
    pub flip_text: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct TextVectorPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextVectorLineTextDirection {
    LeftToRight,
    RightToLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextVectorLineDistanceMode {
    ByLineLength,
    MinimumPreviousDistance,
}

impl Default for TextFormulaLayoutParams {
    fn default() -> Self {
        Self {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            t_start: 0.0,
            t_end: 1.0,
            offset_x_px: 0.0,
            offset_y_px: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            normal_offset_px: 0.0,
            letter_spacing_mul: 1.0,
            letter_spacing_px: 0.0,
            vars: [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        }
    }
}

impl Default for TextDrawnLinesLayoutParams {
    fn default() -> Self {
        Self {
            image_path: None,
            use_tangent_rotation: true,
            static_rotation_rad: 0.0,
            normal_offset_px: 0.0,
            letter_spacing_mul: 1.0,
            letter_spacing_px: 0.0,
            color_tolerance: 16,
            continuation_alpha: 64,
            start_alpha: 192,
        }
    }
}

impl Default for TextVectorLinesLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 1,
            height_px: 1,
            use_tangent_rotation: true,
            static_rotation_rad: 0.0,
            normal_offset_px: 0.0,
            letter_spacing_mul: 1.0,
            letter_spacing_px: 0.0,
            lines: Vec::new(),
        }
    }
}
