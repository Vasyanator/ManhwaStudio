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
- `FauxBoldParams`
- `RenderExtraInfoRequest`
- `RenderedTextExtraInfo`
- `TEXT_FORMULA_USER_VAR_COUNT`
*/

use std::path::PathBuf;

pub const TEXT_FORMULA_USER_VAR_COUNT: usize = 8;

/// Машиночитаемый inline-тег `<m k=v k=v ...>…</m>` — компактная форма, совмещающая
/// все возможности обычных inline-тегов в одном теге. Каждый регулируемый параметр
/// кодируется одним ключом; отсутствующий параметр — отсутствующий ключ.
///
/// Ключи (общий контракт панели и рендера):
/// - `b` — bold: valueless = the real Bold face; with a value
///   `b=thicken[,sharp|round][,out|both][,expand]` (or `b=default`) = faux bold
///   on the SELECTED face (see [`FauxBoldParams`]); an unreadable value falls
///   back to plain (real-face) bold
/// - `i` — italic: valueless = the real Italic face; with a value
///   `i=slant_deg` (degrees, −45..45) = faux italic (baseline shear); an
///   unreadable value falls back to plain italic
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

    /// Token that ALWAYS round-trips through [`Self::parse`], for values that are being
    /// re-serialized rather than authored.
    ///
    /// [`Self::to_token`] is the canonical, human-facing spelling and is fixed at two
    /// decimals, so it silently rounds `1.2345` to `"1.23"`. That is fine while the exact
    /// value still lives elsewhere, but destructive when the token BECOMES the storage —
    /// as in the schema-1 → schema-2 conversion, which folds the legacy `*_px`/`*_percent`
    /// pair into this token and then drops the pair.
    ///
    /// Returns the canonical two-decimal form whenever it parses back to exactly this
    /// value (the overwhelmingly common case, so persisted payloads keep their canonical
    /// spelling and stay comparable to the frozen schema defaults), and the shortest
    /// exact `f32` representation otherwise.
    #[must_use]
    pub fn to_token_lossless(self) -> String {
        let canonical = self.to_token();
        if Self::parse(&canonical).is_some_and(|back| back == self) {
            return canonical;
        }
        // `{}` on `f32` prints the shortest decimal that reads back as the same bits.
        if self.is_percent {
            format!("{}%", self.value)
        } else {
            format!("{}", self.value)
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

/// Faux (synthetic) bold parameters: geometric thickening of the Regular-face
/// glyph outlines instead of switching to the family's real Bold face.
///
/// Takes effect only when the corresponding bold flag is also set
/// (`TextRenderParams.force_bold` for the whole overlay, or an inline
/// `<b=...>` span). The ink boundary of every affected glyph moves outward by
/// `d = thicken_percent / 100 * font_size_px` (the glyph's own effective font
/// size), and the horizontal pen advance automatically grows by `2*d` plus the
/// extra `expand_percent` letter-spacing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FauxBoldParams {
    /// Outline offset distance as % of font size, `0..=25`. `0` = no thickening.
    pub thicken_percent: f32,
    /// EXTRA letter-spacing as % of font size (`0..=50`), added on top of the
    /// automatic `2*d` advance growth.
    pub expand_percent: f32,
    /// `true` = miter joins (limit ~4, sharp corners preserved); `false` =
    /// round (circular-arc) joins at offset vertices.
    pub sharp_corners: bool,
    /// `true` = only outer contours offset outward (counters/holes keep their
    /// size); `false` = holes also shrink by `d` (denser classic embolden).
    pub outward_only: bool,
}

impl Default for FauxBoldParams {
    /// Defaults match the `<b=default>` inline tag: thicken 3 %, no extra
    /// expansion, sharp (miter) corners, counters preserved.
    fn default() -> Self {
        Self {
            thicken_percent: 3.0,
            expand_percent: 0.0,
            sharp_corners: true,
            outward_only: true,
        }
    }
}

/// Vector mesh warp ("raster-transformation") applied to glyph OUTLINES while
/// still vector — a lattice deformation inserted AFTER per-glyph layout but
/// BEFORE the global rotation and rasterization, so warped text stays crisp.
///
/// The lattice is a `cols x rows` grid. `points_norm` is row-major with
/// `len == cols * rows`; `points_norm[i*cols + j]` is the WARPED normalized
/// position of the node whose IDENTITY (undeformed) normalized position is
/// `(j/(cols-1), i/(rows-1))`. An identity mesh (every node equal to its
/// identity position) is a no-op and renders byte-identically to `None`.
///
/// Normalization frame: the ORIGIN is the axis-aligned bounding box top-left of
/// the laid-out glyph placements BEFORE global rotation and BEFORE the warp (the
/// same content bounds the renderer already computes prior to drawing). The box
/// SIZE is `src_width_px`/`src_height_px` when both are valid (`> 0`, Design B),
/// else the live pre-warp bounds size. A point `P` in that pre-global-rotation
/// layout space is normalized `n = ((P.x-box.min.x)/box.w, (P.y-box.min.y)/box.h)`,
/// its lattice coords `n * (cols-1, rows-1)` are clamped into the grid, the four
/// surrounding nodes are bilinearly interpolated to a warped normalized
/// `(wu, wv)`, and it is denormalized back to
/// `(box.min.x + wu*box.w, box.min.y + wv*box.h)`. Points outside `[0, 1]` clamp
/// to the edge cell (no extrapolation).
///
/// `src_width_px`/`src_height_px` are the source-rect the mesh was authored over.
/// Honoring them as the normalization-box SIZE makes the on-canvas authoring UI
/// (which normalizes handle positions against these dims) and the renderer agree.
/// When absent/`0`, the renderer falls back to the live pre-warp box size.
#[derive(Debug, Clone)]
pub struct VectorMeshWarp {
    /// Grid columns (`>= 2`, typically 13).
    pub cols: usize,
    /// Grid rows (`>= 2`, typically 13).
    pub rows: usize,
    /// Authored source-rect width in px. When `> 0` (and finite) it is the warp
    /// normalization-box WIDTH (Design B); `0`/absent -> live pre-warp box width.
    pub src_width_px: f32,
    /// Authored source-rect height in px. When `> 0` (and finite) it is the warp
    /// normalization-box HEIGHT (Design B); `0`/absent -> live pre-warp box height.
    pub src_height_px: f32,
    /// Row-major warped normalized node positions, `len == cols * rows`.
    pub points_norm: Vec<[f32; 2]>,
}

#[derive(Debug, Clone)]
pub struct TextRenderParams {
    pub text: String,
    pub text_color: [u8; 4],
    /// Working name of the main font. Resolved to bytes/face through the
    /// caller-supplied `FontProvider` passed to `render_text_to_image`; the
    /// renderer never reads a font file itself.
    pub font_name: String,
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
    /// Faux (synthetic) bold. Takes effect ONLY when `force_bold` is also
    /// `true`: the renderer then keeps the SELECTED face (no
    /// `Weight::BOLD` font matching) and thickens glyph outlines geometrically
    /// (see [`FauxBoldParams`]). `force_bold && faux_bold.is_none()` = current
    /// real-Bold-face behavior; `None` + `force_bold == false` = no bold.
    pub faux_bold: Option<FauxBoldParams>,
    /// Faux (synthetic) italic slant in degrees, `-45..=45`; positive = top
    /// leans right. Takes effect ONLY when `force_italic` is also `true`: the
    /// renderer then keeps the SELECTED face (no `Style::Italic` matching) and
    /// shears glyph outlines about the baseline. Advances are unchanged.
    pub faux_italic_slant_deg: Option<f32>,
    pub uppercase_text: bool,
    pub trim_extra_spaces: bool,
    /// Rewrites every `…` (U+2026 HORIZONTAL ELLIPSIS) into three ASCII periods
    /// before any other text processing. Purely a source-text normalization: the
    /// substituted `...` then takes part in sentence detection
    /// (`new_line_after_sentence`), hanging punctuation and wrapping exactly as
    /// if the author had typed three periods.
    pub replace_ellipsis_with_dots: bool,
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
    /// Reference band that `line_placement_percent` snaps a glyph to on the
    /// `CustomVectorLines` layout. `GlyphHeight` centers each glyph by its OWN
    /// scaled bitmap height (legacy — glyphs of different ink height float to
    /// different offsets). `LineBox` anchors every glyph to the SHARED font line
    /// box (baseline..ascent) so all glyphs share one baseline, producing a clean
    /// (just curved) line of text. Only `CustomVectorLines` consults it.
    pub line_placement_reference: LinePlacementReference,
    /// Optional vector mesh warp applied to glyph outlines after per-glyph
    /// layout and before global rotation/rasterization (see [`VectorMeshWarp`]).
    /// `None` (and an identity mesh) render byte-identically to no warp. Phase 1
    /// honors it on the horizontal path (Normal, and the rotated variant); other
    /// layout modes currently ignore it.
    pub raster_transform: Option<VectorMeshWarp>,
    /// Which extra "additional info" items the renderer should compute alongside
    /// the pixels (see [`RenderExtraInfoRequest`] and [`RenderedTextExtraInfo`]).
    /// The DEFAULT (nothing requested) is a true no-op: no per-glyph sampling and
    /// the byte-identical fast path. Like `anti_aliasing`/`global_rotation_deg`,
    /// it does NOT affect layout, so it is intentionally excluded from
    /// [`TextRenderShapeCompareParams`]. It is a per-render compute selection and
    /// is NOT persisted in project JSON.
    pub extra_info: RenderExtraInfoRequest,
}

/// Caller selection of which "extra render info" items to compute.
///
/// Each flag enables one optional metric that the renderer measures from glyph
/// placements at the vector stage and returns in [`RenderedTextExtraInfo`]. The
/// default (all `false`) means COMPUTE NOTHING — the renderer takes its
/// byte-identical fast path with zero per-glyph sampling cost. Because these
/// metrics do not affect layout, this field is deliberately kept out of
/// [`TextRenderShapeCompareParams`] (mirroring `anti_aliasing`), and it is a
/// per-render request that is never serialized into project JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderExtraInfoRequest {
    /// Compute the MEAN center: the area centroid of the convex hull of all
    /// included glyphs' placement-box corners (`RenderedTextExtraInfo::mean_center`).
    pub mean_center: bool,
    /// Compute the MEDIAN center: the per-axis median over LINE samples, where
    /// each layout line contributes one sample — the mean of its included glyphs'
    /// placement-box centers (`RenderedTextExtraInfo::median_center`).
    pub median_center: bool,
}

impl RenderExtraInfoRequest {
    /// `true` when at least one extra metric is requested. When `false` the
    /// renderer skips all per-glyph sampling and returns
    /// `RenderedTextExtraInfo::default()`.
    #[must_use]
    pub fn is_active(self) -> bool {
        self.mean_center || self.median_center
    }
}

/// Extra "additional info" computed alongside the rendered pixels.
///
/// Every populated coordinate is in FINAL-IMAGE pixels: top-left origin, with
/// pixel `(0, 0)` spanning `[0, 1)` on each axis. Values may be fractional and
/// may even lie OUTSIDE the image bounds — extreme trim or canvas-growing effects
/// still keep the centers consistent relative to the glyphs. A metric that was
/// not requested (or had zero contributing glyphs) is `None`. The default (all
/// `None`) is what the renderer returns when nothing was requested.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderedTextExtraInfo {
    /// Area centroid of the convex hull of all included glyphs' placement-box
    /// corners, in final-image pixels. `None` when not requested or when no glyph
    /// contributed (e.g. every glyph excluded as hanging punctuation).
    pub mean_center: Option<[f32; 2]>,
    /// Per-axis median over LINE samples, in final-image pixels: each layout line
    /// (a COLUMN in vertical mode) is first collapsed to the mean of its included
    /// glyphs' placement-box centers, and the median is taken over those samples, so
    /// every line weighs the same regardless of how many glyphs it holds. `None` when
    /// not requested or when no glyph contributed.
    pub median_center: Option<[f32; 2]>,
}

impl RenderedTextExtraInfo {
    /// Translate every populated center by `(dx, dy)` final-image pixels.
    ///
    /// Used by the trim and effects stages to keep the centers fixed relative to
    /// the glyphs when the canvas is cropped or grown. A translation commutes with
    /// both the hull-centroid and the per-axis-median computation, so shifting the
    /// finished centers is exact. `None` metrics stay `None`.
    pub fn shift(&mut self, dx: f32, dy: f32) {
        if let Some(center) = self.mean_center.as_mut() {
            center[0] += dx;
            center[1] += dy;
        }
        if let Some(center) = self.median_center.as_mut() {
            center[0] += dx;
            center[1] += dy;
        }
    }
}

/// What the perpendicular line placement (`TextRenderParams::line_placement_percent`)
/// snaps a glyph to on the line-based layouts.
///
/// `GlyphHeight` is the legacy on-path behavior (each glyph centered by its own
/// scaled bitmap height); `LineBox` anchors every glyph to the shared font line box
/// so they share one baseline. See `line_placement_reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinePlacementReference {
    /// Legacy: the glyph's own scaled ink height defines the perpendicular offset,
    /// so at 0% each glyph's ink center sits on the line. Kept as the default so
    /// projects saved before this option render unchanged.
    #[default]
    GlyphHeight,
    /// Shared font line box (baseline..ascent): every glyph is anchored to one
    /// common baseline and only the shared band shifts with the percent.
    LineBox,
}

impl LinePlacementReference {
    /// Stable JSON token used by the typing tab's hand-rolled render-data codec.
    #[must_use]
    pub fn as_json_str(self) -> &'static str {
        match self {
            LinePlacementReference::GlyphHeight => "glyph_height",
            LinePlacementReference::LineBox => "line_box",
        }
    }

    /// Parse the JSON token produced by [`as_json_str`]; unknown/absent input maps
    /// to the legacy [`GlyphHeight`](Self::GlyphHeight) so old projects are stable.
    #[must_use]
    pub fn from_json_str(value: &str) -> Self {
        match value {
            "line_box" => LinePlacementReference::LineBox,
            _ => LinePlacementReference::GlyphHeight,
        }
    }
}

/// One aggregated fallback fact: characters that were drawn by a font OTHER than
/// the one the caller selected, together with the font that actually drew them.
///
/// `family` is the human-readable FAMILY name of that font — never a file path and
/// never a `fontdb` id — because it is what the UI shows and what the user can act
/// on. `chars` holds the DISTINCT characters that font drew, sorted by codepoint.
/// Empty `chars` never occurs: an entry exists only because at least one character
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFallbackUse {
    /// Family name of the font that drew `chars`.
    pub family: String,
    /// Distinct characters that font drew, sorted by codepoint.
    pub chars: Vec<char>,
}

/// Post-shaping font diagnostic of one render ([`RenderedTextImage::font_fallbacks`]).
///
/// It answers "what did the reader ACTUALLY see" for THIS text, which is a different
/// question from the typing panel's static per-font coverage check ("could this font
/// serve the selected typesetting language at all"). Both remain useful: the static
/// check ranks fonts before anything is typed, this one reports the finished render.
///
/// `fallbacks` is INFORMATION, not an error — the renderer's fallback chain is
/// deterministic and identical on every machine, so a character served by it is
/// rendered correctly, just not in the selected typeface. `missing` is the real
/// problem: nothing in the render base could draw those characters and the reader
/// sees a tofu box.
///
/// The default (every glyph drawn by the selected font) is empty and costs no
/// allocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontFallbackReport {
    /// Characters served by the fallback chain instead of the selected font,
    /// grouped by the font that drew them, in first-seen order.
    pub fallbacks: Vec<FontFallbackUse>,
    /// Distinct characters no font in the render base could draw (`glyph_id == 0`,
    /// i.e. `.notdef`/tofu), sorted by codepoint.
    pub missing: Vec<char>,
}

impl FontFallbackReport {
    /// `true` when every glyph was drawn by the selected font and nothing was lost.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fallbacks.is_empty() && self.missing.is_empty()
    }
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
    /// Optional extra "additional info" (mean/median centers) requested via
    /// [`TextRenderParams::extra_info`]. Coordinates are in final-image pixels and
    /// are kept consistent through trim and canvas-growing effects. The default
    /// (`RenderedTextExtraInfo::default()`, all `None`) is what a render that did
    /// not request any extra info returns.
    pub extra: RenderedTextExtraInfo,
    /// Which characters of THIS text the fallback chain drew instead of the
    /// selected font, and which nothing could draw at all. Always computed (it is
    /// two integer comparisons per glyph while everything is fine) and empty when
    /// the selected font served the whole text. See [`FontFallbackReport`].
    pub font_fallbacks: FontFallbackReport,
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
            extra: RenderedTextExtraInfo::default(),
            font_fallbacks: FontFallbackReport::default(),
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

#[cfg(test)]
mod px_or_percent_tests {
    use super::PxOrPercent;

    /// `to_token` is fixed at two decimals and therefore LOSES a value that needs more —
    /// which is destructive when the token replaces the value's only other storage. The
    /// lossless form always parses back to the same value.
    #[test]
    fn lossless_token_round_trips_where_the_canonical_one_rounds() {
        for value in [
            PxOrPercent::px(1.2345),
            PxOrPercent::percent(0.006),
            PxOrPercent::percent(100.125),
            PxOrPercent::px(-0.001),
        ] {
            assert_ne!(
                PxOrPercent::parse(&value.to_token()),
                Some(value),
                "the canonical token is expected to round {value:?}"
            );
            assert_eq!(
                PxOrPercent::parse(&value.to_token_lossless()),
                Some(value),
                "the lossless token must round-trip {value:?}"
            );
        }
    }

    /// It keeps the CANONICAL spelling whenever that already round-trips, so persisted
    /// payloads stay byte-comparable with the frozen schema defaults (`"0.00%"`).
    #[test]
    fn lossless_token_keeps_the_canonical_spelling_when_it_is_exact() {
        assert_eq!(PxOrPercent::percent(0.0).to_token_lossless(), "0.00%");
        assert_eq!(PxOrPercent::percent(50.0).to_token_lossless(), "50.00%");
        assert_eq!(PxOrPercent::percent(100.0).to_token_lossless(), "100.00%");
        assert_eq!(PxOrPercent::px(7.5).to_token_lossless(), "7.50");
    }
}
