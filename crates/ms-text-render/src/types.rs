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
- `AntiAliasingMode`
- `TEXT_FORMULA_USER_VAR_COUNT`
*/

use std::path::PathBuf;

pub const TEXT_FORMULA_USER_VAR_COUNT: usize = 8;

/// Машиночитаемый inline-тег `<m k=v k=v ...>…</m>` — компактная форма, совмещающая
/// все возможности обычных inline-тегов в одном теге. Каждый регулируемый параметр
/// кодируется одним ключом; отсутствующий параметр — отсутствующий ключ.
///
/// Ключи (общий контракт панели и рендера):
/// - `b` — bold (флаг), `i` — italic (флаг)
/// - `f` — шрифт (строка, при необходимости в кавычках)
/// - `s` — размер шрифта в px
/// - `c` — цвет (hex `RRGGBBAA`)
/// - `l` — межстрочный отступ (px-или-%), `k` — кернинг (px-или-%)
/// - `w` — ширина символа (px-или-%), `h` — высота символа (px-или-%)
/// - `x` — смещение X (px-или-%), `y` — смещение Y (px-или-%), `n` — смещение по линии (px-или-%)
/// - `g` — поворот группы (град.), `r` — поворот символа (град.)
/// - `q` — сдвигать следующие символы (флаг)
/// - `j` — не разрывать содержимое тега при подборе форм текста (флаг)
/// - `a` — line alignment (`left`, `center`, `right`, `justify`, or bias `-1..1`)
///
/// Разбирает содержимое тега (без угловых скобок) в список `(ключ, значение)`.
/// Значения могут быть в двойных кавычках (для строк с пробелами); бесфлаговые
/// ключи дают пустое значение. Возвращает `None`, если это не тег `m`.
#[must_use]
pub fn parse_machine_tag(raw: &str) -> Option<Vec<(char, String)>> {
    let mut chars = raw.trim().chars().peekable();
    match chars.next() {
        Some('m' | 'M') => {}
        _ => return None,
    }
    // После имени тега `m` обязателен пробел или конец (чтобы не путать с `main` и т.п.).
    match chars.peek() {
        None => return Some(Vec::new()),
        Some(next) if next.is_whitespace() => {}
        _ => return None,
    }

    let mut out = Vec::new();
    while let Some(&next) = chars.peek() {
        if next.is_whitespace() {
            chars.next();
            continue;
        }
        let key = chars.next()?;
        if chars.peek() == Some(&'=') {
            chars.next();
            let value = if chars.peek() == Some(&'"') {
                chars.next();
                let mut value = String::new();
                for ch in chars.by_ref() {
                    if ch == '"' {
                        break;
                    }
                    value.push(ch);
                }
                value
            } else {
                let mut value = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_whitespace() {
                        break;
                    }
                    value.push(ch);
                    chars.next();
                }
                value
            };
            out.push((key, value));
        } else {
            out.push((key, String::new()));
        }
    }
    Some(out)
}

/// Значение параметра, заданное либо в пикселях, либо в процентах от размера шрифта.
///
/// Единое представление для параметров, которые раньше хранились двумя отдельными
/// полями (`*_px` + `*_percent`). В сериализации и inline-тегах кодируется строкой:
/// число без суффикса — пиксели, число с суффиксом `%` — проценты от кегля.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PxOrPercent {
    pub value: f32,
    pub is_percent: bool,
}

impl PxOrPercent {
    #[must_use]
    pub fn px(value: f32) -> Self {
        Self {
            value,
            is_percent: false,
        }
    }

    #[must_use]
    pub fn percent(value: f32) -> Self {
        Self {
            value,
            is_percent: true,
        }
    }

    /// Разобрать строку вида `"12"`, `"12px"` или `"50%"`.
    /// Возвращает `None`, если число нечитаемо/не конечно.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim();
        let (number, is_percent) = match trimmed.strip_suffix('%') {
            Some(rest) => (rest.trim(), true),
            None => (trimmed.strip_suffix("px").unwrap_or(trimmed).trim(), false),
        };
        let value = number.parse::<f32>().ok()?;
        if value.is_finite() {
            Some(Self { value, is_percent })
        } else {
            None
        }
    }

    /// Свернуть устаревшую пару (px, percent) в одно значение.
    /// Берём пиксели, если они ненулевые, иначе проценты.
    #[must_use]
    pub fn from_legacy_pair(px: f32, percent: f32) -> Self {
        if px != 0.0 {
            Self::px(px)
        } else {
            Self::percent(percent)
        }
    }

    /// Строковое представление для сериализации/inline-тегов.
    #[must_use]
    pub fn to_token(self) -> String {
        if self.is_percent {
            format!("{:.2}%", self.value)
        } else {
            format!("{:.2}", self.value)
        }
    }

    /// Разложить на устаревшую пару (px, percent): активна ровно одна компонента.
    #[must_use]
    pub fn as_px_percent(self) -> (f32, f32) {
        if self.is_percent {
            (0.0, self.value)
        } else {
            (self.value, 0.0)
        }
    }

    /// Привести к процентам от кегля (px-режим: `value / font_size * 100`).
    #[must_use]
    pub fn as_percent_of(self, font_size_px: f32) -> f32 {
        if self.is_percent {
            self.value
        } else if font_size_px > 0.0 {
            self.value / font_size_px * 100.0
        } else {
            0.0
        }
    }

    /// Привести к пикселям (percent-режим: `value / 100 * font_size`).
    #[must_use]
    pub fn as_px_of(self, font_size_px: f32) -> f32 {
        if self.is_percent {
            self.value / 100.0 * font_size_px
        } else {
            self.value
        }
    }
}

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
    /// Glyph edge anti-aliasing mode. Does not affect layout, only the
    /// coverage->alpha transfer curve applied by the outline rasterizer.
    pub anti_aliasing: AntiAliasingMode,
    /// Global rotation of the whole laid-out text (degrees), applied to glyph
    /// outlines while still vector, before rasterization. 0.0 = no rotation.
    /// A non-zero value routes the horizontal path through the rotated renderer
    /// so the canvas auto-grows to the rotated bounds (no clipping).
    pub global_rotation_deg: f32,
    /// Perpendicular placement of glyphs relative to the layout line/path,
    /// in percent `[-100, 100]`. Only the line-based SHOW modes honor it
    /// (`Formula`, `CustomVectorLines`); `Shape`/`CustomRasterLines`/`Normal`
    /// ignore it. `0` centers each glyph's ink on the line, `+100` rests it
    /// ABOVE the line (сверху, ink bottom on the line), `-100` BELOW it (снизу,
    /// ink top on the line). Applied along the path normal at the vector level.
    pub line_placement_percent: f32,
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
    /// X-позиция (в пикселях итогового изображения) левого верхнего угла
    /// ИСХОДНОГО контента — накопленный левый паддинг всех увеличивающих холст
    /// post-эффектов (тень/свечение/блюр и т.п.). По умолчанию 0.
    pub content_origin_x: u32,
    /// Y-позиция (в пикселях итогового изображения) левого верхнего угла
    /// ИСХОДНОГО контента — накопленный верхний паддинг всех увеличивающих
    /// холст post-эффектов. По умолчанию 0.
    pub content_origin_y: u32,
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
            content_origin_x: 0,
            content_origin_y: 0,
        }
    }
}

/// Горизонтальное выравнивание строк.
///
/// Раньше было четырьмя дискретными вариантами (`Left`/`Center`/`Right`/`Justify`).
/// Теперь это непрерывное смещение `bias` от -1.0 (по левому краю) до 1.0 (по
/// правому краю) плюс флаг `justify` (свободное выравнивание, растягивающее
/// строки по ширине блока). Старые варианты восстанавливаются через
/// [`HorizontalAlign::from_config`] и сводятся обратно к строке через
/// [`HorizontalAlign::legacy_str`] (PSD-экспорт, легаси-JSON).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizontalAlign {
    /// -1.0 = по левому краю, 0.0 = по центру, 1.0 = по правому краю.
    pub bias: f32,
    /// Свободное (justify) выравнивание — растягивает строки по ширине блока.
    pub justify: bool,
}

impl HorizontalAlign {
    pub const LEFT: Self = Self {
        bias: -1.0,
        justify: false,
    };
    pub const CENTER: Self = Self {
        bias: 0.0,
        justify: false,
    };
    pub const RIGHT: Self = Self {
        bias: 1.0,
        justify: false,
    };
    pub const JUSTIFY: Self = Self {
        bias: 0.0,
        justify: true,
    };

    /// Доля свободного пространства слева от строки: 0.0 — влево, 0.5 — центр,
    /// 1.0 — вправо.
    #[must_use]
    pub fn offset_fraction(self) -> f32 {
        (self.bias.clamp(-1.0, 1.0) + 1.0) * 0.5
    }

    /// Ближайший дискретный вариант для совместимости (PSD-экспорт, легаси-JSON).
    #[must_use]
    pub fn legacy_str(self) -> &'static str {
        if self.justify {
            "justify"
        } else if self.bias <= -0.5 {
            "left"
        } else if self.bias >= 0.5 {
            "right"
        } else {
            "center"
        }
    }

    fn legacy_bias_from_str(raw: &str) -> Option<f32> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "left" => Some(-1.0),
            "right" => Some(1.0),
            "center" | "justify" => Some(0.0),
            _ => None,
        }
    }

    /// Восстановление из конфигурации с обратной совместимостью: точный `bias`
    /// (новое поле `align_bias`), если задан, иначе он выводится из легаси-строки
    /// `align` (`left`/`center`/`right`/`justify`). Флаг `justify` берётся из
    /// строки `align == "justify"`.
    #[must_use]
    pub fn from_config(align_str: Option<&str>, bias: Option<f32>) -> Self {
        let justify = align_str
            .map(str::trim)
            .is_some_and(|s| s.eq_ignore_ascii_case("justify"));
        let bias = bias
            .or_else(|| align_str.and_then(Self::legacy_bias_from_str))
            .unwrap_or(0.0)
            .clamp(-1.0, 1.0);
        Self { bias, justify }
    }
}

/// Kerning mode for horizontal and formula/on-path glyph spacing (the vertical
/// path stacks by ink height, where `Fixed` and `Auto` coincide).
///
/// - `Fixed` (user label "Метрический"): fixed per-glyph advance built from each
///   glyph's OWN advance width; font GPOS/`kern` pair kerning is NOT applied.
///   Manual tracking (`kerning_px`/`kerning_percent`) is added on top.
/// - `Auto` (user label "Авто"): font glyph-pair kerning (GPOS/`kern`) applied —
///   cosmic-text `Shaping::Advanced` shaped positions plus manual tracking. This is
///   the byte-identical successor of the historical `Metric` mode; the legacy
///   serialized value `"metric"` deserializes to `Auto` so old overlays keep their
///   font-pair kerning and render identically.
/// - `Optical`: shape-based optical spacing that normalizes true ink-to-ink gaps
///   toward the run/column median. Implemented, but hidden from the panel UI (only
///   ever set through a loaded/legacy project value, never offered as a choice).
///
/// Serialization: `Fixed` -> `"fixed"`, `Auto` -> `"auto"`, `Optical` ->
/// `"optical"`, with legacy `"metric"` mapping to `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KerningMode {
    Fixed,
    Auto,
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

/// Glyph edge anti-aliasing style applied as a coverage->alpha transfer curve
/// in the outline rasterizer. Does not affect layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntiAliasingMode {
    None,
    Sharp,
    Crisp,
    Strong,
    Smooth,
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
