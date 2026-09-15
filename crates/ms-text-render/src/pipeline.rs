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
- horizontal glyph pen positions are computed in `horizontal_run_layout`: `Auto`
  is byte-identical to the shaped `cosmic-text` positions plus optional manual
  tracking (font pair kerning applied); `KerningMode::Fixed` steps by each glyph's
  OWN nominal (un-kerned) advance (`nominal_glyph_advance_px`, no font pair
  kerning) plus manual tracking; `KerningMode::Optical` re-spaces
  adjacent inked glyphs per run via `optical_horizontal_run_layout` by measuring
  true ink-to-ink gaps from glyph outlines (through the same
  `glyph_blit::glyph_outline_transform` pivot the draw pass uses) and normalizing
  them toward the run's median gap; the pure numeric core
  (`median_of_gaps`/`optical_delta`/`optical_base_advance`) lives in the shared
  `optical` module and is reused by the vertical path;
- USER-AUTHORED kerning pairs (`font_provider::CustomKerningTable`, indexed per
  render by `font_registry::CustomKerningMap`) REPLACE the font's own value for a
  matching pair under EVERY mode: the pen steps by `nominal_glyph_advance_px(left)`
  plus the authored delta, so an authored pair is indistinguishable from a built-in
  one and a `0.0` entry cancels one. Guards, fast-path interaction and the
  measurement paths that must stay in sync are the CUSTOM KERNING contract in
  `MODULE_README.md`;
- the normal horizontal path runs a SINGLE placement pass: `horizontal_run_layout`
  and `get_image` execute once per run/glyph, collecting each glyph into a
  `HorizontalGlyphPlacement` (`build_horizontal_placement`) that both the bounds
  box and the draw pass (`draw_horizontal_placement`) reuse; the inline-rotated
  path collects `RotatedGlyphPlacement` the same way;
- horizontal monochrome glyphs are rasterized from their true font outlines via the
  shared `glyph_blit` helpers; the layout math, bounds/canvas assembly and
  inline-color contract are unchanged, and color glyphs keep the `raster.rs` bitmap
  blit;
- the vertical glyph scale (global `glyph_height_percent` and the inline
  `<stretching=W%,H%>` tag) is anchored at the run BASELINE through
  `GlyphScaleSettings::scaled_center_about_baseline` / `scaled_rect_about_baseline`;
  `scaled_rect` keeps the box-centre anchor for the cell-placed vertical path and
  for callers that need only the scaled size. Line stacking follows
  `line_baseline_advance_table`: an inline `<stretching>` span never drives
  `compute_line_extra_spacing_table` (which stays a line-level, whole-text rule) and
  can only GROW the gap ABOVE its own line, by the real ink rise of the stretched
  glyphs on their OWN faces, maxed per line and never summed (`InlineHeightRoom`).
  See the GLYPH HEIGHT SCALE contract in `MODULE_README.md`;
- an attrs modification the registered fonts cannot serve is degraded BEFORE the
  shaper sees it: `synthesized_italic_slant_deg` / `synthesized_bold_params` rewrite
  a whole-overlay real italic/bold into the faux form on ONE params copy, and
  `degrade_unavailable_inline_italic` / `degrade_unavailable_inline_bold` do the same
  per inline `<i>`/`<b>` span in place. Italic is gated by
  `font_registry::family_has_matching_face` (style is a hard match filter), bold by
  `font_registry::family_has_face_of_requested_weight` (an exact weight is what the
  primary pick and every weight-filtered fallback pass require). All four warn (log +
  returned `warnings`); see the UNSERVICEABLE-ATTRS GUARD contract in
  `MODULE_README.md`;
- `smoke_render_text_to_image` оставлен как бездисковая заглушка для runtime smoke-anchor;
- основной источник поведения: `render_text_to_image`, `reshape_text_for_shape`,
  `build_vertical_layout_text`, `render_vertical_text`, `render_text_with_formula_layout`,
  `soft_hyphenate_overlong`
  и базовый raster path из старого
  `src/tabs/typing/render.rs`.
*/

use ms_log::runtime_log;
use ms_log::trace::cat;

use super::effects::{apply_effects_pipeline, apply_text_preprocess_effects};
use super::extra_info::ExtraInfoAccumulator;
use super::fallback_diag::collect_font_fallback_report;
use super::font_provider::{CustomKerningTable, FontProvider};
use super::font_registry::{
    CustomKerningMap, InlineFontRegistry, build_inline_font_registry,
    family_has_face_of_requested_weight, family_has_matching_face, load_font_content,
};
use super::font_ligature_patch::EllipsisLigatureMode;
use super::font_system_pool::with_leased_font_system;
use super::formula::{
    FormulaRenderOutcome, FormulaRenderRequest, render_text_with_drawn_lines_layout,
    render_text_with_formula_layout, render_text_with_vector_lines_layout,
};
use super::inline_styles::{
    FauxFaceBaseline, InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
    collect_requested_inline_font_labels, parse_inline_style_tags, remap_inline_style_spans,
    spans_have_attrs_overrides,
};
use super::glyph_blit::{
    glyph_needs_bitmap_fallback, glyph_outline_has_counter, glyph_outline_transform,
    glyph_subpixel_offset, hash_font_id, nominal_glyph_advance_px, resolve_outline_for_glyph,
};
use super::glyph_contour::PlacedContour;
use super::layout::{VerticalRasterRequest, render_vertical_text};
use super::optical::{
    OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX, OpticalAxis, OpticalContourCache, median_of_gaps,
    optical_base_advance, optical_delta, optical_pair_gap,
};
use super::raster::{
    GlyphRgbaView, PixelBounds, RigidPlacement, RgbaCanvasView, build_glyph_rgba_buffer,
    draw_rotated_scaled_glyph_rgba, draw_scaled_glyph_rgba, include_rotated_rect_bounds,
    include_scaled_rect_bounds, is_cancelled, rasterize_unscaled_glyph,
    rotate_placements_about_centroid, trim_rendered_image_to_alpha_bounds,
};
use super::vector::{
    FauxOutlineParams, MeshWarpContext, Outline, OutlineCache, RasterScratch, build_aa_lut,
    faux_key_bits, glyph_contour_from_outline, rasterize_outline_into,
};
use super::types::{
    FAUX_THICKEN_PERCENT_MAX, FAUX_THICKEN_PERCENT_MIN, FauxBoldParams, FontFallbackReport,
    HorizontalAlign, KerningMode, RenderedTextExtraInfo,
    RenderedTextImage, TextLayoutMode, TextLineMode, TextRenderParams,
    TextRenderShapeCompareParams, TextWrapMode,
};
use super::wrap::{
    HyphenationDictionaries, LayoutTextResult, ShapeWrapRequest, VerticalWrapRequest,
    build_vertical_layout_text, needs_hyphenation_dicts, reshape_text_for_shape,
    should_prehyphenate_overlong, word_break_policy,
};
use ms_text_util::language::text_language;
use ms_text_util::segmentation::with_default_segmenter;
use cosmic_text::{
    Align, Attrs, AttrsOwned, Buffer, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping,
    SwashCache, SwashContent, Wrap,
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

    /// Scaled glyph rect anchored at the box CENTRE on both axes.
    ///
    /// The centre is scale-invariant, so this is the placement for a glyph that
    /// sits in a CELL rather than on a shared baseline (the vertical columns,
    /// whose cell step already follows the scaled ink height) and for callers
    /// that only need the scaled SIZE or a rect they re-centre themselves
    /// (`include_rotated_rect_bounds` pins the rect to its own `dst_center`).
    /// Baseline-laid text must use [`Self::scaled_rect_about_baseline`] instead —
    /// a centre anchor lifts a height-scaled glyph off the baseline by
    /// `(glyph_h / 2 - placement_top) * (1 - height_mul)`, which differs per glyph.
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
        let (scaled_width, scaled_height) = self.scaled_size(width_px, height_px);
        (
            center_x - scaled_width * 0.5,
            center_y - scaled_height * 0.5,
            scaled_width,
            scaled_height,
        )
    }

    /// Centre the SCALED glyph box must be pinned to when the vertical scale is
    /// anchored at the text BASELINE.
    ///
    /// `left_px`/`top_px`/`width_px`/`height_px` are the UNSCALED bitmap placement
    /// box in content space (y down); `baseline_y` is the pen baseline of the run
    /// the glyph belongs to, in that same space (`src_top + placement_top`). The
    /// horizontal scale keeps the box centre (the pen x is compensated by the
    /// advance, not by the pivot), the vertical one maps the baseline to itself:
    /// a height-scaled glyph keeps its baseline exactly where an unscaled glyph of
    /// the same run would have it and only its ink extent changes.
    ///
    /// This is the value every baseline-laid draw site feeds to
    /// [`glyph_outline_transform`] as `dst_center`, which pins the scaled box
    /// centre there — so the returned point and
    /// [`Self::scaled_rect_about_baseline`] describe the same box.
    #[must_use]
    pub(crate) fn scaled_center_about_baseline(
        self,
        left_px: f32,
        top_px: f32,
        width_px: f32,
        height_px: f32,
        baseline_y: f32,
    ) -> (f32, f32) {
        (
            left_px + width_px * 0.5,
            baseline_y + (top_px + height_px * 0.5 - baseline_y) * self.height_mul,
        )
    }

    /// Scaled glyph rect with the VERTICAL scale anchored at `baseline_y`.
    ///
    /// Same box as [`Self::scaled_center_about_baseline`], expressed as
    /// `(left, top, width, height)`. The size is the clamped [`Self::scaled_size`]
    /// the bitmap blit and the bounds pass already used, so only the vertical
    /// POSITION changes versus [`Self::scaled_rect`]. Bounds, the bitmap blit and
    /// the extra-info samples must all use this on the baseline-laid paths, or the
    /// canvas box drifts away from the drawn ink.
    #[must_use]
    pub(crate) fn scaled_rect_about_baseline(
        self,
        left_px: f32,
        top_px: f32,
        width_px: f32,
        height_px: f32,
        baseline_y: f32,
    ) -> (f32, f32, f32, f32) {
        let (center_x, center_y) =
            self.scaled_center_about_baseline(left_px, top_px, width_px, height_px, baseline_y);
        let (scaled_width, scaled_height) = self.scaled_size(width_px, height_px);
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
    /// Whether the FONT this glyph was drawn from carries user-authored kerning
    /// pair overrides (`font_provider::CustomKerningTable`). Set per glyph by
    /// [`Self::with_custom_pairs`] from the render's `CustomKerningMap`; `false`
    /// on every path that does not apply pair overrides at all (the vertical
    /// layout, see the MODULE_README).
    ///
    /// It exists only to take the run OFF the byte-identical shaped-position fast
    /// path — see [`Self::uses_default_metric_layout`]. Whether an individual PAIR
    /// is actually overridden is decided in the pen loop, not here.
    pub(crate) custom_pairs: bool,
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
            custom_pairs: false,
        }
    }

    /// Same settings with the custom-override flag set to `active`.
    ///
    /// Applied by the horizontal and formula paths right after
    /// [`inline_kerning_for_glyph`], from `CustomKerningMap::has_table` for the
    /// glyph's own face. Kept a separate step rather than a parameter so the
    /// vertical path — which applies no pair kerning of any kind — keeps calling
    /// the resolver unchanged.
    #[must_use]
    pub(crate) fn with_custom_pairs(mut self, active: bool) -> Self {
        self.custom_pairs = active;
        self
    }

    #[must_use]
    fn has_zero_adjustment(self) -> bool {
        self.spacing_px.abs() <= f32::EPSILON && self.spacing_percent.abs() <= f32::EPSILON
    }

    #[must_use]
    pub(crate) fn extra_spacing_px(self, basis_px: f32) -> f32 {
        self.spacing_px + basis_px.max(0.0) * (self.spacing_percent / 100.0)
    }

    /// Whether this glyph may take the fast byte-identical shaped-position path
    /// (cosmic-text `Shaping::Advanced` positions with no manual tracking). Only
    /// `Auto` qualifies: `Fixed` needs own-advance repositioning and `Optical`
    /// needs ink-gap normalization, so both must go through the custom path.
    ///
    /// A font carrying user-authored kerning overrides (`custom_pairs`) is
    /// disqualified for the same reason: the shaped positions already contain the
    /// font's OWN pair kerning, and an override REPLACES it — taking the shortcut
    /// would silently render the overrides as if they did not exist.
    #[must_use]
    pub(crate) fn uses_default_metric_layout(self) -> bool {
        self.mode == KerningMode::Auto && self.has_zero_adjustment() && !self.custom_pairs
    }
}

/// One laid-out horizontal run: pen positions plus the metrics alignment needs.
///
/// `leading_hang_px`/`trailing_hang_px` are the widths of the run's LEADING and
/// TRAILING hanging-punctuation runs (see [`hanging_metrics_for_layout`]). They
/// are reported raw, unweighted: the caller scales them by the hanging strength
/// (`TextRenderParams::hanging_weight`). `glyph_xs` never depends on them — the
/// hang is a whole-line translation, not a glyph move.
#[derive(Debug, Clone)]
struct HorizontalRunLayout {
    glyph_xs: Vec<f32>,
    line_width_px: f32,
    leading_hang_px: f32,
    trailing_hang_px: f32,
}

impl HorizontalRunLayout {
    /// Width this line is ALIGNED by at hanging strength `weight`: the logical
    /// width minus the weighted part of both hanging edge runs.
    ///
    /// Exact at the ends of the range: `weight == 0.0` returns `line_width_px`
    /// bit for bit, `weight == 1.0` returns the fully hung visual width.
    #[must_use]
    fn align_width_px(&self, weight: f32) -> f32 {
        self.line_width_px - weight * self.leading_hang_px - weight * self.trailing_hang_px
    }

    /// How far left the line origin moves at hanging strength `weight`, i.e. the
    /// weighted part of the LEADING hang that is pushed outside the block.
    #[must_use]
    fn origin_shift_px(&self, weight: f32) -> f32 {
        weight * self.leading_hang_px
    }
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
    custom_kerning: Option<&CustomKerningTable>,
    font_size_px: f32,
    base_line_height_px: f32,
    extra_line_spacing_px: f32,
    preserve_edge_spaces: bool,
) -> LayoutTextResult {
    // Dictionary bundle for the process-global typesetting language; thread-local
    // cached so this hot prepass path does not reload TeX patterns per render.
    let hyphen_dicts = needs_hyphenation_dicts(shape_params.text_wrap_mode)
        .then(|| HyphenationDictionaries::for_language(text_language()));
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
            custom_kerning,
            font_size_px,
            line_height_px: base_line_height_px,
            base_width_px: shape_params.width_px.max(1) as f32,
            wrap_mode: shape_params.text_wrap_mode,
            hyphen_dicts: hyphen_dicts.as_deref(),
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
                hyphen_dicts: hyphen_dicts.as_deref(),
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
    let _effects_span = ms_log::trace_scope!(
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
        // Arbitrary-image effects reuse has no glyph layout to sample from.
        extra: RenderedTextExtraInfo::default(),
        // No text was shaped here, so there is nothing to diagnose.
        font_fallbacks: FontFallbackReport::default(),
    };

    if !effects_json.trim().is_empty() {
        apply_effects_pipeline(&mut image, effects_json, cancel)?;
    }

    Ok(image)
}

/// Effective perpendicular line-placement fraction in `[-1, 1]` for the current
/// layout mode.
///
/// Perpendicular line placement is a SHOW-only feature of the two line-based
/// modes (`Formula`, `CustomVectorLines`). The render code for those modes is
/// shared with a HIDE sibling (`Shape` reuses the formula path,
/// `CustomRasterLines` reuses the drawn-lines path), so this is the single
/// gating source: it returns `0.0` for every HIDE / non-line mode so a stale
/// panel value can never leak into them.
fn effective_line_placement_frac(params: &TextRenderParams) -> f32 {
    match params.text_layout_mode {
        TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines => {
            (params.line_placement_percent / 100.0).clamp(-1.0, 1.0)
        }
        TextLayoutMode::Normal
        | TextLayoutMode::Shape
        | TextLayoutMode::CustomRasterLines => 0.0,
    }
}

/// The font-patch mode this render's params ask for.
///
/// `force_remove_ellipsis_glyph` is a SUB-PARAMETER of
/// `replace_ellipsis_with_dots` (see `types.rs`): removing the font's ellipsis
/// ligatures only makes sense for text whose `…` was just expanded into `...`,
/// so both flags must be set. This is the single place that `&&` is evaluated —
/// it decides the pool partition, and the loader then reads the mode off the
/// leased cache.
fn ellipsis_ligature_mode(params: &TextRenderParams) -> EllipsisLigatureMode {
    if params.replace_ellipsis_with_dots && params.force_remove_ellipsis_glyph {
        EllipsisLigatureMode::Remove
    } else {
        EllipsisLigatureMode::Keep
    }
}

pub fn render_text_to_image(
    params: &TextRenderParams,
    fonts: &dyn FontProvider,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let _render_span = ms_log::trace_scope!(
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

    // Lease a reusable FontSystem (and its per-system font-load cache) from the
    // process-global pool instead of building a fresh one per render. Building a
    // fresh `FontSystem` runs a full system-font scan on every call (~2.2s first
    // time, ~32ms after), which dominated render time. The leased system returns
    // to the pool when this closure finishes; `?` and `return` inside the closure
    // return the render `Result` to this call, which is this function's result.
    // The lease is taken from the pool partition of this render's ellipsis mode:
    // a patched and an unpatched face of the same font must never meet in one
    // `FontSystem` (see `font_system_pool.rs`, ELLIPSIS-PATCH PARTITION).
    with_leased_font_system(ellipsis_ligature_mode(params), |font_system, font_cache| {
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

    let content = fonts.resolve(&params.font_name).ok_or_else(|| {
        format!(
            "шрифт '{}' не найден среди переданных шрифтов",
            params.font_name
        )
    })?;
    let selected_face = load_font_content(
        font_system,
        font_cache,
        &content,
        params.selected_face_index,
    )
    .map_err(|error| format!("не удалось загрузить шрифт в fontdb: {error}"))?;

    // Index of the user-authored kerning overrides BY REGISTERED FACE, built once
    // per render (here for the selected font, extended below with the inline
    // `<font=…>` fonts). The layout paths only know a glyph's `fontdb::ID`, so this
    // is the only bridge from a `FontContent` to a pen step. Empty — and therefore
    // free — for every font without overrides.
    let mut custom_kerning = CustomKerningMap::default();
    custom_kerning.record(font_cache, &content);
    // The SELECTED font's table alone, for the WIDTH METRICS: wrapping measures with
    // the selected `attrs` and has no per-glyph font ids to key the face map by.
    let selected_custom_kerning = content.custom_kerning_table().map(Arc::clone);

    let mut attrs = Attrs::new().metrics(Metrics::new(font_size_px, font_size_px));
    attrs = selected_face.apply_to_attrs(attrs);
    // Faux inline spans must never change font matching: they fall back to the
    // SELECTED face's own weight/style, not to a hardcoded 400/upright which
    // could match a different file of the same family (or nothing at all).
    let faux_face_baseline = FauxFaceBaseline::from_registered_face(&selected_face);

    // A REAL italic request the selected family cannot serve must NOT reach the
    // shaper: `Style::Italic` is a hard `Attrs::matches` filter, so it would drop
    // the caller's font out of the run entirely (tofu from the bundled emoji
    // face, or an empty match set and cosmic-text's `expect` panic). Degrade it
    // to the faux italic the renderer already implements, ONCE, by rewriting the
    // params — every downstream consumer (attrs, advances, bounds pads, all
    // layout modes) then follows the faux path on its own.
    //
    // A REAL bold request is degraded on the same principle, for a different reason:
    // `Weight` is a ranking key, so it cannot empty the match set, but cosmic-text's
    // primary pick requires an EXACT weight match and does not rank down inside the
    // family — so a bold request on a family without a Bold file silently jumps to
    // another typeface AND puts the whole run out of reach of every weight-filtered
    // fallback pass (see `font_registry::family_has_face_of_requested_weight`).
    let synthesized_italic = synthesized_italic_slant_deg(font_system, &attrs, params);
    let synthesized_bold = synthesized_bold_params(font_system, &attrs, params);
    let degraded_params;
    let params = if synthesized_italic.is_some() || synthesized_bold.is_some() {
        let mut degraded = params.clone();
        if let Some(slant) = synthesized_italic {
            runtime_log::log_warn(format!(
                "[ms_text_render] real italic requested for font '{}' (family '{}'), but no italic \
                 face of that family is registered and the bundled base ships none; synthesizing \
                 faux italic at {slant:.1} deg instead. Load the family's Italic file, or set an \
                 explicit faux slant, to control this.",
                params.font_name,
                selected_face.family_name.as_deref().unwrap_or("<none>")
            ));
            warnings.push(format!(
                "у шрифта «{}» нет курсивного начертания — курсив нарисован синтетическим наклоном \
                 {slant:.0}°; подключите файл Italic этого шрифта, чтобы использовать настоящий курсив",
                params.font_name
            ));
            degraded.faux_italic_slant_deg = Some(slant);
        }
        if let Some(faux) = synthesized_bold {
            runtime_log::log_warn(format!(
                "[ms_text_render] real bold requested for font '{}' (family '{}'), but that family \
                 has no face at weight {}; synthesizing faux bold ({}% of the em) instead. Keeping \
                 the real request would draw the text in another typeface and would cut the run \
                 off from the script fallback chains. Load the family's Bold file, or set explicit \
                 faux bold parameters, to control this.",
                params.font_name,
                selected_face.family_name.as_deref().unwrap_or("<none>"),
                cosmic_text::Weight::BOLD.0,
                faux.thicken_percent,
            ));
            warnings.push(format!(
                "у шрифта «{}» нет жирного начертания — жирность нарисована синтетическим \
                 утолщением; подключите файл Bold этого шрифта, чтобы использовать настоящее \
                 жирное начертание",
                params.font_name
            ));
            degraded.faux_bold = Some(faux);
        }
        degraded_params = degraded;
        &degraded_params
    } else {
        params
    };

    // Faux bold/italic bypass: with faux params present the renderer must KEEP
    // the SELECTED face (no Bold/Italic font matching) and synthesize the style
    // geometrically at the glyph seam. Without faux params the legacy real-face
    // behavior is unchanged.
    let (want_real_bold, want_real_italic) = base_attrs_real_bold_italic(params);
    if want_real_bold {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if want_real_italic {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }

    let mut buffer = Buffer::new(
        font_system,
        Metrics::new(font_size_px, base_line_height_px),
    );
    buffer.set_size(font_system, Some(width_px as f32), None);
    buffer.set_wrap(font_system, Wrap::None);

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
        font_system,
        &attrs,
        selected_custom_kerning.as_deref(),
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
            font_system,
            &attrs,
            selected_custom_kerning.as_deref(),
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
                    extra: RenderedTextExtraInfo::default(),
                    // Cancelled before shaping: no glyphs, nothing to diagnose.
                    font_fallbacks: FontFallbackReport::default(),
                });
            }
        }
    }
    let justify_alignment = justify_alignment_option(params.align);

    let mut mapped_inline_style_spans = parsed_inline_styles.as_ref().and_then(|parsed| {
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
    let mut inline_font_registry_build = build_inline_font_registry(
        font_system,
        font_cache,
        fonts,
        requested_inline_fonts.as_slice(),
    );
    // Inline `<font=…>` fonts carry their own overrides; fold them into the render's
    // map so a pair inside an inline span is kerned by ITS font's table. Taken
    // BEFORE the warnings, which move out of the same struct.
    custom_kerning.merge(std::mem::take(&mut inline_font_registry_build.custom_kerning));
    warnings.extend(inline_font_registry_build.warnings);

    // Same guard as the whole-overlay one above, but per span: an inline `<i>`
    // resolves against its own `<font=...>` family, so it can be unserviceable
    // even when the overlay's own font ships an italic face (and vice versa).
    // Runs on the MAPPED spans, after the inline fonts are registered and before
    // anything reads them, so attrs and per-glyph faux resolution stay in sync.
    if let Some(spans) = mapped_inline_style_spans.as_mut() {
        let degraded_spans = degrade_unavailable_inline_italic(
            font_system,
            &attrs,
            &inline_font_registry_build.registry,
            faux_face_baseline,
            spans.as_mut_slice(),
        );
        if degraded_spans > 0 {
            runtime_log::log_warn(format!(
                "[ms_text_render] {degraded_spans} inline <i> span(s) requested a real italic no \
                 registered font family can serve; synthesizing faux italic at \
                 {SYNTHESIZED_ITALIC_SLANT_DEG:.1} deg for them"
            ));
            warnings.push(format!(
                "курсивные вставки <i> нарисованы синтетическим наклоном \
                 {SYNTHESIZED_ITALIC_SLANT_DEG:.0}°: у выбранного шрифта нет курсивного начертания"
            ));
        }

        // Same per span for a real `<b>`: a family with no Bold face would take the
        // whole span into the bundled bold tier (a different typeface) and out of the
        // weight-filtered fallback chains.
        let degraded_bold_spans = degrade_unavailable_inline_bold(
            font_system,
            &attrs,
            &inline_font_registry_build.registry,
            faux_face_baseline,
            spans.as_mut_slice(),
        );
        if degraded_bold_spans > 0 {
            runtime_log::log_warn(format!(
                "[ms_text_render] {degraded_bold_spans} inline <b> span(s) requested a real bold \
                 no registered font family can serve; synthesizing faux bold for them"
            ));
            warnings.push(
                "жирные вставки <b> нарисованы синтетическим утолщением: у выбранного шрифта нет \
                 жирного начертания"
                    .to_string(),
            );
        }
    }

    if let Some(mapped_spans) = mapped_inline_style_spans
        .as_deref()
        .filter(|spans| spans_have_attrs_overrides(spans))
    {
        let styled_spans = mapped_spans
            .iter()
            .map(|span| {
                (
                    span.clone(),
                    apply_inline_style_to_attrs(
                        &attrs,
                        span,
                        &inline_font_registry_build.registry,
                        faux_face_baseline,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let spans_iter = styled_spans.iter().filter_map(|(span, span_attrs)| {
            let text_slice = layout_text.get(span.start..span.end)?;
            Some((text_slice, span_attrs.as_attrs()))
        });
        buffer.set_rich_text(
            font_system,
            spans_iter,
            &attrs,
            Shaping::Advanced,
            justify_alignment,
        );
    } else {
        buffer.set_text(
            font_system,
            layout_text.as_str(),
            &attrs,
            Shaping::Advanced,
        );
    }
    apply_line_aligns_to_buffer(&mut buffer, inline_line_aligns.as_slice());
    buffer.shape_until_scroll(font_system, false);

    // Post-shaping font diagnostic, collected ONCE here because every layout mode
    // below (custom lines, formula/shape, vertical, horizontal rotated, horizontal
    // normal) draws from THIS shaped buffer. The reference point is the set of
    // families the CALLER supplied: the selected font plus every inline
    // `<font=...>` font — a glyph drawn by any of them is "your font"; anything
    // else came out of the deterministic fallback chain (`font_base.rs`).
    let mut expected_families: Vec<&str> = Vec::new();
    if let Some(family) = selected_face.family_name.as_deref() {
        expected_families.push(family);
    }
    let inline_families = inline_font_registry_build
        .registry
        .values()
        .filter_map(|face| face.family_name.as_deref());
    for family in inline_families {
        if !expected_families.contains(&family) {
            expected_families.push(family);
        }
    }
    let font_fallbacks =
        collect_font_fallback_report(font_system, &buffer, expected_families.as_slice());

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
            font_system: &mut *font_system,
            buffer: &mut buffer,
            attrs: &attrs,
            faux_face_baseline,
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            inline_font_registry: &inline_font_registry_build.registry,
            custom_kerning: &custom_kerning,
            layout_text: layout_text.as_str(),
            font_size_px,
            base_line_height_px: font_size_px,
            // Gated: only CustomVectorLines carries a non-zero value here;
            // CustomRasterLines (same render path) resolves to 0.0.
            line_placement_frac: effective_line_placement_frac(params),
        };
        ms_log::trace_log!(cat::RENDER, "render_text path=custom_lines mode={:?}", params.text_layout_mode);
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
                rendered.font_fallbacks = font_fallbacks;
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
        ms_log::trace_log!(cat::RENDER, "render_text path=formula_shape mode={:?}", params.text_layout_mode);
        match render_text_with_formula_layout(FormulaRenderRequest {
            params,
            font_system: &mut *font_system,
            buffer: &mut buffer,
            attrs: &attrs,
            faux_face_baseline,
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            inline_font_registry: &inline_font_registry_build.registry,
            custom_kerning: &custom_kerning,
            layout_text: layout_text.as_str(),
            font_size_px,
            base_line_height_px: font_size_px,
            // Gated: only Formula carries a non-zero value here; Shape (same
            // render path) resolves to 0.0.
            line_placement_frac: effective_line_placement_frac(params),
        })? {
            FormulaRenderOutcome::Rendered(mut rendered) => {
                rendered.warnings.extend(warnings);
                rendered.font_fallbacks = font_fallbacks;
                apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
                return Ok(rendered);
            }
            FormulaRenderOutcome::FallbackToStandard(warning) => warnings.push(warning),
        }
    }

    let glyph_scale = GlyphScaleSettings::from_params(params);
    let layout_line_offsets = compute_layout_line_offsets(layout_text.as_str());
    let has_inline_size_overrides = mapped_inline_style_spans
        .as_deref()
        .is_some_and(spans_have_inline_size_overrides);
    if params.text_line_mode == TextLineMode::Vertical {
        ms_log::trace_log!(cat::RENDER, "render_text path=vertical lines={}", layout_line_offsets.len());
        // The vertical path reads these entries as COLUMN GAPS, so it takes the
        // plain line-level table; the horizontal path below stacks BASELINES and
        // takes the grow-only `line_baseline_advance_table` instead.
        let line_extra_spacing_table = compute_line_extra_spacing_table(
            params,
            layout_text.as_str(),
            layout_line_offsets.as_slice(),
            mapped_inline_style_spans.as_deref(),
            font_size_px,
            extra_line_spacing_px,
        );
        let mut rendered = render_vertical_text(VerticalRasterRequest {
            params,
            font_system: &mut *font_system,
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
        rendered.font_fallbacks = font_fallbacks;
        apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
        return Ok(rendered);
    }

    // Baselines stack on the grow-only advance table: an inline `<stretching>`
    // height span may push the next line further away, never pull it closer.
    let line_advance_table = line_baseline_advance_table(
        params,
        layout_text.as_str(),
        layout_line_offsets.as_slice(),
        mapped_inline_style_spans.as_deref(),
        font_size_px,
        extra_line_spacing_px,
        &InlineHeightRoom::measure(
            params,
            &buffer,
            font_system,
            layout_line_offsets.as_slice(),
            mapped_inline_style_spans.as_deref(),
        ),
    );
    let line_baselines = compute_horizontal_line_baselines(
        &buffer,
        base_line_height_px,
        extra_line_spacing_px,
        line_advance_table.as_slice(),
        has_inline_size_overrides,
    );

    // Inline-смещения с поворотом (группы/символа) обычный «прямой» blit не умеет —
    // для таких overlay используем отдельный путь с обратной выборкой и поворотом.
    // Глобальный поворот всего блока использует тот же векторный путь: он поворачивает
    // контуры глифов до растеризации и растит холст под повёрнутый bbox.
    let has_global_rotation = params.global_rotation_deg.abs() > f32::EPSILON;
    if has_global_rotation
        || mapped_inline_style_spans
            .as_deref()
            .is_some_and(spans_have_inline_rotation)
    {
        ms_log::trace_log!(cat::RENDER, "render_text path=horizontal_rotated lines={} global_rotation_deg={}", layout_line_offsets.len(), params.global_rotation_deg);
        let mut rendered = render_horizontal_rotated(
            params,
            font_system,
            &buffer,
            &attrs,
            faux_face_baseline,
            &inline_font_registry_build.registry,
            mapped_inline_style_spans.as_deref(),
            layout_text.as_str(),
            layout_line_offsets.as_slice(),
            line_baselines.as_slice(),
            &custom_kerning,
            width_px,
            font_size_px,
            line_height_px,
            params.global_rotation_deg,
            cancel,
        )?;
        rendered.warnings.extend(warnings);
        rendered.font_fallbacks = font_fallbacks;
        apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
        rendered = trim_rendered_image_to_alpha_bounds(rendered, 1);
        return Ok(rendered);
    }

    ms_log::trace_log!(cat::RENDER, "render_text path=horizontal lines={} align={:?}", layout_line_offsets.len(), params.align);
    let mut cache = SwashCache::new();
    // Single placement pass: layout and per-glyph placement are computed ONCE per
    // run and reused for both the bounds box and the draw pass. One outline cache,
    // one optical ink-contour cache (both no-ops for every non-Optical kerning
    // mode) and one AA LUT serve the whole path — `horizontal_run_layout` and
    // `get_image` run exactly once per glyph.
    let mut outline_cache = OutlineCache::new();
    let mut contour_cache = OpticalContourCache::new();
    // Reused per-glyph rasterizer buffers for the draw pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info (mean/median centers). Inactive by default -> every
    // `add_glyph`/`map_points`/`finish` call is a no-op with no per-glyph cost.
    // Samples are collected in raw content space here; the optional mesh warp is
    // applied once below (after the warp context is built), and the content->canvas
    // offset is applied in `finish`.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    let mut placements: Vec<HorizontalGlyphPlacement> = Vec::new();
    let mut line_idx = 0usize;
    // Normalized once per render: `0.0` disables the hang, `1.0` is the full hang.
    let hanging_weight = params.hanging_weight();
    let mut runs = buffer.layout_runs().peekable();
    while let Some(run) = runs.next() {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        // Leading/trailing hanging-punctuation runs are excluded from the extra-info
        // sampling only above the strength threshold (see the contract on
        // `TextRenderParams::excludes_hanging_from_extra_info`).
        let hanging_bounds = if extra_active && params.excludes_hanging_from_extra_info() {
            hanging_edge_run_bounds(&run)
        } else {
            (0, run.glyphs.len())
        };
        let run_layout = horizontal_run_layout(
            params,
            &run,
            font_system,
            &mut cache,
            &mut outline_cache,
            &mut contour_cache,
            layout_line_offsets.as_slice(),
            mapped_inline_style_spans.as_deref(),
            &custom_kerning,
            font_size_px,
        );
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
            let glyph_faux = resolve_faux_counter_flag(
                faux_style_for_glyph(
                    params,
                    mapped_inline_style_spans.as_deref(),
                    layout_line_offsets.as_slice(),
                    run.line_i,
                    glyph,
                ),
                font_system,
                &mut outline_cache,
                glyph,
            );
            if let Some(placement) = build_horizontal_placement(
                font_system,
                &mut cache,
                &mut outline_cache,
                glyph,
                line_offset_x + (glyph_x - glyph.x) + glyph_offset[0],
                baseline_y + glyph_offset[1],
                glyph_scale,
                glyph_text_color,
                glyph_faux,
            ) {
                // Feed the extra-info sampler from the SAME placement box the draw
                // pass uses; skip glyphs in the line's hanging-punctuation edge runs
                // and glyphs a faux thinning offset consumed entirely (`draws_ink`) —
                // the reported centers describe where ink IS, so a glyph that draws
                // nothing must not move them.
                // The sample is warpable only when the glyph draws from its outline;
                // a color-glyph bitmap fallback (no outline) draws unwarped pixels,
                // so its sample must stay unwarped too.
                if extra_active
                    && placement.draws_ink()
                    && !is_edge_run_hanging(hanging_bounds, glyph_idx)
                {
                    let (corners, center) = horizontal_placement_extra_samples(&placement);
                    extra_acc.add_glyph(corners, center, placement.outline.is_some(), line_idx);
                }
                placements.push(placement);
            }
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = build_wrapped_hyphen_glyph(
                font_system,
                &attrs,
                faux_face_baseline,
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
            let hyphen_faux = resolve_faux_counter_flag(
                style_offset
                    .map(|offset| {
                        faux_style_at_offset(
                            params,
                            mapped_inline_style_spans.as_deref(),
                            offset,
                            hyphen_glyph.font_size,
                        )
                    })
                    .unwrap_or(FauxGlyphStyle::NONE),
                font_system,
                &mut outline_cache,
                &hyphen_glyph,
            );
            let hyphen_offset_x = line_offset_x
                + trailing_hyphen_x(&run)
                + run_layout
                    .glyph_xs
                    .last()
                    .zip(run.glyphs.last())
                    .map(|(glyph_x, last_glyph)| glyph_x - last_glyph.x)
                    .unwrap_or(0.0);
            if let Some(placement) = build_horizontal_placement(
                font_system,
                &mut cache,
                &mut outline_cache,
                &hyphen_glyph,
                hyphen_offset_x + hyphen_offset[0],
                baseline_y + hyphen_offset[1],
                hyphen_scale,
                hyphen_text_color,
                hyphen_faux,
            ) {
                // The wrapped soft hyphen never hangs, so it contributes to the
                // extra-info samples whenever it actually draws ink (a faux thinning
                // offset can consume even a hyphen's single bar). Warpable only
                // when it draws from its outline (bitmap fallback stays unwarped).
                if extra_active && placement.draws_ink() {
                    let (corners, center) = horizontal_placement_extra_samples(&placement);
                    extra_acc.add_glyph(corners, center, placement.outline.is_some(), line_idx);
                }
                placements.push(placement);
            }
        }
        line_idx += 1;
    }

    // Bounds are derived from the SAME swash bitmap placement boxes collected above
    // (`src_left`/`src_top` + `glyph_w`/`glyph_h`), so the canvas size is
    // byte-identical to the historical two-pass path. Zero-size glyphs never
    // reached the collection (`build_horizontal_placement` skips them) and were
    // already no-ops for `include_scaled_rect_bounds`.
    let mut bounds = PixelBounds::empty();
    for placement in &placements {
        // Faux ink pads (`bounds_pad`) widen the box for the offset/sheared
        // outline; they are exact zeros without faux, keeping the historical
        // canvas byte-identical. The rect is height-anchored at the pen baseline,
        // exactly like the draw pivot below.
        include_scaled_rect_bounds(&mut bounds, placement.padded_scaled_rect());
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            line_height_px.ceil() as u32,
        ));
    }

    // Optional vector mesh warp: normalize over the UNWARPED content box just
    // computed (the pre-warp, pre-global-rotation layout AABB). The Normal path
    // never carries a global rotation (that routes to `render_horizontal_rotated`),
    // so the peel/reapply angle is 0 and the centroid is unused. The warp context
    // is `None` (byte-identical fast path) for `None`/identity/invalid meshes.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let box_min = [bounds.min_x as f32, bounds.min_y as f32];
        let box_size = [
            (bounds.max_x - bounds.min_x) as f32,
            (bounds.max_y - bounds.min_y) as f32,
        ];
        MeshWarpContext::new(warp, box_min, box_size, 0.0, [0.0, 0.0])
    });
    // Grow the canvas bounds to the warped extent so a strong outward warp never
    // clips (identity/None leaves `bounds` untouched).
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
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
    // Draw pass reuses the placements collected above (same layout, same
    // `get_image` placement/outline). Monochrome glyphs rasterize from their
    // outline; color/emoji glyphs blit the captured bitmap fallback.
    for placement in &placements {
        if is_cancelled(cancel) {
            return Err("render_next render cancelled".to_string());
        }
        draw_horizontal_placement(
            &mut raster_scratch,
            rgba.as_mut_slice(),
            out_width,
            out_height,
            placement,
            x_offset,
            y_offset,
            &aa_lut,
            warp_ctx.as_ref(),
        );
    }

    // Extra-info centers: warp the raw content-space samples through the same mesh
    // context the draw pass used, then map to canvas pixels with the same offset.
    // This runs BEFORE effects/trim; those stages self-correct the centers.
    if let Some(ctx) = warp_ctx.as_ref() {
        extra_acc.map_points(|point| ctx.warp_world(point));
    }
    let extra = extra_acc.finish(x_offset as f32, y_offset as f32);

    let mut rendered = RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings,
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        font_fallbacks,
    };
    apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
    rendered = trim_rendered_image_to_alpha_bounds(rendered, 1);
    Ok(rendered)
    })
}

/// One collected glyph placement for the normal (unrotated) horizontal path.
///
/// Captures everything both the bounds box and the draw pass need, computed in a
/// SINGLE layout pass so `horizontal_run_layout` and `get_image` run once per
/// glyph. `outline` is the glyph's true font outline; when present the draw pass
/// rasterizes it and `fallback` is `None`. `fallback` holds the swash bitmap
/// `(content, data)` for any outline-less glyph (real color/emoji glyph or a
/// monochrome embedded-bitmap glyph), captured only when the outline is absent.
///
/// `src_left_i`/`src_top_i` are the INTEGER content-space top-left of the unscaled
/// bitmap (`physical.x + placement.left`, `physical.y - placement.top`) — the same
/// value the historical two-pass path fed to both `include_scaled_rect_bounds`
/// (as `f32`) and the draw pivot, so bounds and pixels stay byte-identical.
/// `placement_left`/`placement_top` and `subpixel` feed the outline->world pivot.
struct HorizontalGlyphPlacement {
    outline: Option<Arc<Outline>>,
    fallback: Option<(SwashContent, Vec<u8>)>,
    glyph_w: usize,
    glyph_h: usize,
    src_left_i: i32,
    src_top_i: i32,
    placement_left: f32,
    placement_top: f32,
    scale: GlyphScaleSettings,
    text_color: [u8; 4],
    /// Subpixel fraction baked into the swash bitmap coverage; re-applied to the
    /// outline placement only (the bitmap fallback already carries it).
    subpixel: [f32; 2],
    /// Faux bold/italic style; `outline` already IS the faux-bold variant, the
    /// shear applies at the draw transform. `NONE` = byte-identical plain path.
    faux: FauxGlyphStyle,
    /// Pre-scale bounds padding for the faux ink (`faux_bounds_pads`);
    /// `[0.0, 0.0]` when `faux` is `NONE`.
    bounds_pad: [f32; 2],
}

impl HorizontalGlyphPlacement {
    /// Whether this placement puts any ink on the canvas.
    ///
    /// `false` only for a glyph whose outline was CONSUMED by a faux thinning
    /// offset: it has no outline AND deliberately no bitmap fallback, yet keeps
    /// a non-zero box. Such a glyph must not vote on the extra-info ink centers
    /// (`ExtraInfoAccumulator`), which report where the ink IS.
    #[must_use]
    fn draws_ink(&self) -> bool {
        self.outline.is_some() || self.fallback.is_some()
    }

    /// The run's pen baseline in content space — the anchor of the height scale.
    ///
    /// `src_top_i` is `physical.y - placement.top`, so adding `placement_top`
    /// recovers exactly the integer pen `physical.y` the glyph was shaped at.
    #[must_use]
    fn baseline_y(&self) -> f32 {
        self.src_top_i as f32 + self.placement_top
    }

    /// The scaled glyph rect, height-anchored at the baseline and widened by the
    /// faux bounds pads (exactly the plain baseline-anchored rect when faux is
    /// off — the pads are hard zeros).
    ///
    /// The pads are symmetric, so they do not move the rect's centre: bounds,
    /// extra-info samples and the draw pivot stay on one box.
    #[must_use]
    fn padded_scaled_rect(&self) -> (f32, f32, f32, f32) {
        self.scale.scaled_rect_about_baseline(
            self.src_left_i as f32 - self.bounds_pad[0],
            self.src_top_i as f32 - self.bounds_pad[1],
            self.glyph_w as f32 + 2.0 * self.bounds_pad[0],
            self.glyph_h as f32 + 2.0 * self.bounds_pad[1],
            self.baseline_y(),
        )
    }
}

/// Collect one glyph placement for the normal horizontal path.
///
/// Calls `get_image` ONCE to read the bitmap placement box (bounds/pivot source)
/// and resolve the fill outline; returns `None` for glyphs `get_image` cannot
/// produce or for zero-size (space) glyphs, which never contribute to bounds or
/// pixels. `pos_x`/`pos_y` are the glyph pen position in layout pixels (already
/// carrying the inline glyph offset).
#[allow(clippy::too_many_arguments)]
fn build_horizontal_placement(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    glyph: &LayoutGlyph,
    pos_x: f32,
    pos_y: f32,
    scale: GlyphScaleSettings,
    text_color: [u8; 4],
    faux: FauxGlyphStyle,
) -> Option<HorizontalGlyphPlacement> {
    let physical = glyph.physical((pos_x, pos_y), 1.0);
    let image = cache.get_image(font_system, physical.cache_key).as_ref()?;
    let glyph_w = image.placement.width as usize;
    let glyph_h = image.placement.height as usize;
    if glyph_w == 0 || glyph_h == 0 {
        return None;
    }
    let placement_left = image.placement.left as f32;
    let placement_top = image.placement.top as f32;
    // Integer content-space top-left, identical to the historical bounds/draw math.
    let src_left_i = physical.x + image.placement.left;
    let src_top_i = physical.y - image.placement.top;
    let subpixel = glyph_subpixel_offset(physical.cache_key);
    let outline = resolve_outline_for_glyph(font_system, outline_cache, glyph, faux.bold);
    // Capture the swash bitmap only for genuinely outline-less glyphs (real
    // color/emoji or a monochrome embedded-bitmap glyph); a glyph whose outline
    // was CONSUMED by a faux thinning offset must draw nothing rather than pop
    // back to its full-weight bitmap.
    let fallback = if outline.is_none()
        && glyph_needs_bitmap_fallback(font_system, outline_cache, glyph, faux.bold)
    {
        Some((image.content, image.data.clone()))
    } else {
        None
    };
    let bounds_pad = faux_bounds_pads(
        faux,
        placement_top,
        glyph_h as f32,
        scale.width_mul,
        scale.height_mul,
    );
    Some(HorizontalGlyphPlacement {
        outline,
        fallback,
        glyph_w,
        glyph_h,
        src_left_i,
        src_top_i,
        placement_left,
        placement_top,
        scale,
        text_color,
        subpixel,
        faux,
        bounds_pad,
    })
}

/// Draw one collected horizontal (unrotated) glyph placement into the canvas.
///
/// Rasterizes the glyph's true font outline at exactly the pixels the bitmap blit
/// used: scaled about the bitmap center (`dst_center`), no rotation, world mapped
/// to the canvas by the `x_offset`/`y_offset` derived from the bounds box.
/// Color/emoji glyphs (captured `fallback`) keep the bitmap blit (identity or
/// center-scaled). `aa_lut` is the coverage->alpha transfer table applied only on
/// the outline path; the bitmap fallback is unaffected. `scratch` supplies the
/// reused per-glyph rasterizer buffers. `warp` is the optional vector mesh warp,
/// applied only on the outline path (the bitmap fallback is not warped in
/// Phase 1); `None` keeps the byte-identical fast path.
// The draw call naturally carries the scratch, canvas target + dimensions, the
// collected placement, the bounds-derived offsets and the AA table; splitting
// them would only obscure the 1:1 mapping to `rasterize_outline_into`.
#[allow(clippy::too_many_arguments)]
fn draw_horizontal_placement(
    scratch: &mut RasterScratch,
    rgba: &mut [u8],
    out_width: u32,
    out_height: u32,
    placement: &HorizontalGlyphPlacement,
    x_offset: i32,
    y_offset: i32,
    aa_lut: &[u8; 256],
    warp: Option<&MeshWarpContext>,
) {
    let glyph_w = placement.glyph_w;
    let glyph_h = placement.glyph_h;
    // Content-space top-left of the (unscaled) glyph bitmap.
    let src_left = placement.src_left_i as f32;
    let src_top = placement.src_top_i as f32;

    // Prefer the true font outline: rasterize it at the exact world placement the
    // bitmap blit would have used (scale about the bitmap center, no rotation).
    if let Some(outline) = placement.outline.as_ref() {
        // Height scale anchored at the pen baseline, not at the glyph's own box
        // centre: a box-centre pivot lifts a shrunk glyph off the baseline by an
        // amount that differs per glyph (its own ink box), which is what made a
        // partially height-scaled word float at mid-height.
        let (dst_center_x, dst_center_y) = placement.scale.scaled_center_about_baseline(
            src_left,
            src_top,
            glyph_w as f32,
            glyph_h as f32,
            placement.baseline_y(),
        );
        // Re-add the subpixel fraction cosmic-text baked into the bitmap coverage
        // (physical.x/y carried only the integer pen), so the outline matches it.
        let transform = glyph_outline_transform(
            dst_center_x,
            dst_center_y,
            0.0,
            placement.placement_left,
            placement.placement_top,
            glyph_w as f32,
            glyph_h as f32,
            placement.scale.width_mul,
            placement.scale.height_mul,
            placement.subpixel,
            placement.faux.shear_x,
        );
        rasterize_outline_into(
            scratch,
            rgba,
            out_width as usize,
            out_height as usize,
            -(x_offset as f32),
            -(y_offset as f32),
            outline,
            &transform,
            placement.text_color,
            aa_lut,
            warp,
        );
        return;
    }

    // No fillable outline (real color glyph, or a monochrome embedded-bitmap /
    // sbix / CBDT-mono glyph): blit the captured non-empty bitmap. Spaces were
    // already filtered by the zero-size check during collection. Dropping this on
    // a non-color glyph would silently lose embedded-bitmap-only glyphs.
    let Some((content, data)) = placement.fallback.as_ref() else {
        return;
    };
    let draw_x = placement.src_left_i + x_offset;
    let draw_y = placement.src_top_i + y_offset;
    if placement.scale.is_identity() {
        rasterize_unscaled_glyph(
            rgba,
            out_width,
            out_height,
            *content,
            data.as_slice(),
            glyph_w,
            glyph_h,
            draw_x,
            draw_y,
            placement.text_color,
        );
    } else {
        let glyph_rgba =
            build_glyph_rgba_buffer(content, data.as_slice(), glyph_w, glyph_h, placement.text_color);
        let mut canvas = RgbaCanvasView {
            rgba,
            width: out_width as usize,
            height: out_height as usize,
        };
        // Same baseline anchor as the outline path above, shifted into canvas px
        // by the bounds offsets — the bitmap fallback must land in the same box
        // the bounds pass reserved for it.
        let dst_rect = placement.scale.scaled_rect_about_baseline(
            draw_x as f32,
            draw_y as f32,
            glyph_w as f32,
            glyph_h as f32,
            placement.baseline_y() + y_offset as f32,
        );
        draw_scaled_glyph_rgba(
            &mut canvas,
            GlyphRgbaView {
                rgba: glyph_rgba.as_slice(),
                width: glyph_w,
                height: glyph_h,
            },
            dst_rect,
            placement.scale,
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
/// embedded-bitmap glyph), built only when the outline is absent. BOTH empty
/// means the glyph draws NOTHING — a faux thinning offset consumed every
/// contour — and the draw pass skips it (see [`glyph_needs_bitmap_fallback`]);
/// the box dimensions stay non-zero, so emptiness is the only signal.
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
    /// Faux bold/italic style; `outline` already IS the faux-bold variant, the
    /// shear applies at the draw transform. `NONE` = byte-identical plain path.
    faux: FauxGlyphStyle,
    /// Pre-scale bounds padding for the faux ink (`faux_bounds_pads`);
    /// `[0.0, 0.0]` when `faux` is `NONE`.
    bounds_pad: [f32; 2],
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
    group_key: Option<(usize, usize)>,
    group_rotation_rad: f32,
    /// `true` when this glyph is in its line's leading/trailing hanging-punctuation
    /// run and must be excluded from the extra-info sampling (set by the collection
    /// loop; defaults to `false`, e.g. for the wrapped hyphen).
    hanging_excluded: bool,
    /// Layout line this glyph belongs to. Carried on the placement because the
    /// extra-info sampling runs AFTER the run loop here (global rotation is baked in
    /// first), so the loop's line counter is no longer in scope. Median-only.
    line_idx: usize,
}

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
    faux: FauxGlyphStyle,
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
    let outline = resolve_outline_for_glyph(font_system, outline_cache, glyph, faux.bold);
    // Build the bitmap RGBA for any outline-less glyph fallback: real color glyphs
    // and monochrome embedded-bitmap / sbix / CBDT-mono glyphs alike (the zero-size
    // skip above already filtered spaces). A glyph whose outline was CONSUMED by a
    // faux thinning offset gets no bitmap — it must draw nothing.
    let glyph_rgba = if outline.is_none()
        && glyph_needs_bitmap_fallback(font_system, outline_cache, glyph, faux.bold)
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
        rotation_rad: glyph_rotation_rad,
        group_key,
        group_rotation_rad,
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
/// `faux_face_baseline` — weight/style выбранного face: fallback для faux-спанов
/// в attrs переносимого мягкого переноса (см. `apply_inline_style_to_attrs`).
#[allow(clippy::too_many_arguments)]
fn render_horizontal_rotated(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &Buffer,
    attrs: &Attrs<'_>,
    faux_face_baseline: FauxFaceBaseline,
    inline_font_registry: &super::font_registry::InlineFontRegistry,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    layout_text: &str,
    layout_line_offsets: &[usize],
    line_baselines: &[f32],
    custom_kerning: &CustomKerningMap,
    width_px: u32,
    font_size_px: f32,
    line_height_px: f32,
    global_rotation_deg: f32,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    let mut cache = SwashCache::new();
    // Per-render outline cache: each glyph outline is extracted at most once.
    let mut outline_cache = OutlineCache::new();
    // Per-render glyph ink-contour cache for optical horizontal kerning.
    let mut contour_cache = OpticalContourCache::new();
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
        compute_inline_line_aligns(params.align, layout_text, inline_style_spans);
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
        let run_layout = horizontal_run_layout(
            params,
            &run,
            font_system,
            &mut cache,
            &mut outline_cache,
            &mut contour_cache,
            layout_line_offsets,
            inline_style_spans,
            custom_kerning,
            font_size_px,
        );
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
                &mut outline_cache,
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
                &mut cache,
                &mut outline_cache,
                glyph,
                line_offset_x + (glyph_x - glyph.x) + offset.global_px[0],
                baseline_y + offset.global_px[1],
                glyph_scale,
                glyph_text_color,
                glyph_faux,
                offset.glyph_rotation_rad,
                group_key,
                offset.group_rotation_rad,
            ) {
                placement.hanging_excluded = is_edge_run_hanging(hanging_bounds, glyph_idx);
                placement.line_idx = line_idx;
                placements.push(placement);
            }
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(hyphen_glyph) = build_wrapped_hyphen_glyph(
                font_system,
                attrs,
                faux_face_baseline,
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
                &mut outline_cache,
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
                &mut cache,
                &mut outline_cache,
                &hyphen_glyph,
                hyphen_offset_x + hyphen_offset.global_px[0],
                baseline_y + hyphen_offset.global_px[1],
                hyphen_scale,
                hyphen_text_color,
                hyphen_faux,
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
                scaled_left,
                scaled_top,
                scaled_width,
                scaled_height,
                placement.center_x,
                placement.center_y,
                placement.rotation_rad,
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
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            placement.center_x,
            placement.center_y,
            placement.rotation_rad,
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
                placement.faux.shear_x,
            );
            rasterize_outline_into(
                &mut raster_scratch,
                rgba.as_mut_slice(),
                out_width as usize,
                out_height as usize,
                -(x_offset as f32),
                -(y_offset as f32),
                outline,
                &transform,
                placement.text_color,
                &aa_lut,
                warp_ctx.as_ref(),
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
        extra,
        // Filled in by `render_text_to_image`, which owns the shaped buffer.
        font_fallbacks: FontFallbackReport::default(),
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

/// Combine a glyph's shaped advance with its faux-bold advance delta without
/// letting the delta REVERSE the pen.
///
/// `faux_extra` is signed (`faux_advance_extra_px_for_glyph`): at the minimum
/// strength it is `-0.10 * em` uniformly and `-0.20 * em` for a compensated
/// counter-bearing glyph in the `outward_only` mode. A glyph whose own advance
/// is smaller than that contraction — a zero-advance combining mark is the clear
/// case, narrow punctuation the marginal one — would otherwise step the pen
/// BACKWARDS and place the following glyph before the current one. The sum is
/// therefore floored at zero in the direction the run actually advances:
/// cosmic-text walks the pen LEFTWARDS in an RTL run (`shape.rs`: `x -= x_advance`
/// before the glyph is pushed), so `base_advance` is negative there and the floor
/// mirrors instead of flipping the run around.
///
/// Explicitly requested tracking/kerning is deliberately NOT part of the floor:
/// the user asking for negative letter-spacing is asking for overlap. Returns
/// `base_advance` unchanged for `faux_extra == 0.0`, so a run without faux bold
/// keeps bit-identical pen positions.
#[must_use]
fn faux_floored_advance(base_advance: f32, faux_extra: f32) -> f32 {
    if faux_extra == 0.0 {
        return base_advance;
    }
    let stepped = base_advance + faux_extra;
    if base_advance < 0.0 {
        stepped.min(0.0)
    } else {
        stepped.max(0.0)
    }
}

/// The single `char` of a glyph's source cluster, or `None` when the cluster is
/// not exactly one character.
///
/// `glyph.start`/`glyph.end` are byte offsets into the run's own text, so the
/// slice is the cluster the shaper folded into this glyph. A multi-character
/// cluster (a ligature, a base + combining mark, a surrogate-free emoji sequence)
/// deliberately yields `None`: user-authored kerning pairs are entered as exactly
/// TWO characters in the settings UI, so a cluster pair has no authored meaning and
/// guessing one would kern text the user never described.
///
/// `get` rather than indexing: the offsets come from the shaper, and an
/// out-of-range or non-boundary pair must degrade to "no override", never panic.
#[must_use]
pub(crate) fn single_char_cluster(run_text: &str, glyph: &LayoutGlyph) -> Option<char> {
    let start = glyph.start.min(glyph.end);
    let end = glyph.start.max(glyph.end);
    let mut chars = run_text.get(start..end)?.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The user-authored advance delta in px for the pair (`prev`, `cur`), or `None`
/// when no override applies.
///
/// Conservative by contract — ALL of the following must hold, and each guard has a
/// reason that is easy to "simplify" away:
/// - both glyphs come from the SAME face (`font_id`). A pair that straddles a
///   font-fallback boundary is not a kerning pair in ANY font: the two characters
///   were never adjacent in one typeface's design, and the override was authored
///   against one specific font entry.
/// - that face carries an override table at all.
/// - each glyph's source cluster is exactly one `char` (see
///   [`single_char_cluster`]).
///
/// The em basis is the LEFT glyph's own `font_size`, not the block font size, so an
/// inline `<size=…>` span kerns proportionally to the text it is part of.
#[must_use]
fn custom_pair_delta_px(
    custom_kerning: &CustomKerningMap,
    run_text: &str,
    prev: &LayoutGlyph,
    cur: &LayoutGlyph,
) -> Option<f32> {
    if prev.font_id != cur.font_id {
        return None;
    }
    let table = custom_kerning.table_for(prev.font_id)?;
    let left = single_char_cluster(run_text, prev)?;
    let right = single_char_cluster(run_text, cur)?;
    table.delta_px(left, right, prev.font_size)
}

/// The pen step an OVERRIDDEN pair must take, in the run's own direction.
///
/// `magnitude_px` is the unsigned distance between the two pens — the left glyph's
/// raw `hmtx` advance (`nominal_glyph_advance_px`) plus the authored delta — and
/// `metric_advance` is the SHAPED step (`cur.x - prev.x`) the override replaces.
///
/// Why a sign is needed at all: cosmic-text lays a right-to-left run out by
/// SUBTRACTING each (always positive) shaped advance before pushing the glyph, so
/// `glyph.x` DECREASES with the glyph index and `metric_advance` is negative there
/// (`cosmic-text/src/shape.rs`, `ShapeLine::layout_to_buffer`). Every input the
/// override path starts from is unsigned instead: `hmtx` advances are positive by
/// construction and [`optical_base_advance`] hands a positive own-advance back
/// verbatim. Adding the authored delta to that magnitude and using it as the step
/// would walk an RTL pen RIGHTWARD and invert "tighten" into "widen"; mirroring it
/// onto the run's axis keeps a negative authored delta tightening the pair in BOTH
/// directions.
///
/// The shaped step is the primary evidence of direction — it IS the direction the
/// shaper walked the pen. A degenerate step (zero or non-finite, e.g. a zero-width
/// glyph) carries no sign, so the left glyph's bidi embedding level decides
/// instead; it is the only other direction fact a `LayoutGlyph` holds.
///
/// Scope note: `KerningMode::Fixed` WITHOUT an override still steps by the unsigned
/// nominal advance, so an un-overridden RTL run keeps its pre-existing (wrong)
/// direction under that mode. Making the override follow the run is a fix for the
/// override path only; the `Fixed` behaviour predates it and is deliberately left
/// untouched here.
#[must_use]
fn custom_pair_step_px(magnitude_px: f32, metric_advance: f32, prev: &LayoutGlyph) -> f32 {
    let rtl = if metric_advance.is_finite() && metric_advance != 0.0 {
        metric_advance < 0.0
    } else {
        prev.level.is_rtl()
    };
    if rtl { -magnitude_px } else { magnitude_px }
}

/// Unsigned distance the pen must cover for an OVERRIDDEN pair: the left glyph's
/// raw (un-kerned) `hmtx` advance plus the authored delta.
///
/// The `hmtx` advance is unsigned by construction, so the shaped step is folded in
/// only as an unsigned FALLBACK magnitude for a glyph whose own advance cannot be
/// read (`optical_base_advance`'s degenerate branch) — feeding it in signed would
/// re-introduce the RTL sign this split exists to keep out.
#[must_use]
fn custom_pair_magnitude_px(
    font_system: &mut FontSystem,
    prev: &LayoutGlyph,
    metric_advance: f32,
    delta_px: f32,
) -> f32 {
    let fallback = metric_advance.abs();
    let own = nominal_glyph_advance_px(font_system, prev).unwrap_or(fallback);
    optical_base_advance(own, fallback) + delta_px
}

/// Compute per-glyph pen positions (`glyph_xs`) and hanging metrics for one
/// horizontal layout run, honoring inline tracking and the selected kerning mode.
///
/// `Auto` mode is byte-identical to cosmic-text's shaped positions plus optional
/// manual tracking (font pair kerning applied). `Fixed` steps by each glyph's OWN
/// nominal (un-kerned) advance (`nominal_glyph_advance_px`) so font pair kerning
/// is dropped, plus manual tracking. When
/// `params.kerning_mode == KerningMode::Optical`, adjacent inked glyphs are
/// re-spaced by measuring true ink-to-ink gaps (`optical_horizontal_run_layout`)
/// and normalizing them toward the run's median gap; a run with fewer than one
/// finite gap (e.g. a single inked glyph) falls back to the metric accumulation.
///
/// `font_system` is also read by `Fixed` (nominal own-advance lookup);
/// `cache`/`outline_cache`/`contour_cache` are used only by the optical path
/// (outline extraction + ink measurement) and are untouched for `Auto`/`Fixed`.
/// Never panics; a glyph without a fillable outline is treated as non-kernable
/// (delta 0) rather than an error.
///
/// Faux bold: on the metric branches (`Auto`/`Fixed`, and the `Optical`
/// fallback) each pen step additionally grows by the previous glyph's
/// `2*d + expand_px` (`faux_advance_extra_px_for_glyph`), and `line_width_px`
/// includes each glyph's own extra so the trailing faux glyph is part of the
/// logical (alignment) width; a run with any faux glyph never takes the
/// byte-identical shaped-position fast path. BOTH places combine the two terms
/// through `faux_floored_advance`, so a THINNING delta larger than the glyph's
/// own advance cannot walk the pen backwards and cannot make the alignment
/// width disagree with the accumulated pen. The optical branch instead
/// measures ink from the offset outlines and adds only the normalization
/// delta. A `LayoutRun` is one whole laid-out line in cosmic-text 0.14, so
/// per-run accumulation already spans every inline size/font/style boundary.
///
/// Custom kerning: a pair listed in the glyph font's
/// `font_provider::CustomKerningTable` REPLACES the font's own value for that pair
/// under EVERY mode — the step becomes `nominal_glyph_advance_px(prev)` (the raw
/// `hmtx` advance `Fixed` already uses) plus the authored delta, so the font's
/// GPOS/`kern` contribution is dropped and the user's number stands in its place.
/// That is what makes an authored pair indistinguishable from a built-in one.
/// Uniform tracking (`extra_spacing_px`) is a separate setting and is still added
/// on top, unchanged.
#[allow(clippy::too_many_arguments)]
fn horizontal_run_layout(
    params: &TextRenderParams,
    run: &LayoutRun<'_>,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
    custom_kerning: &CustomKerningMap,
    font_size_px: f32,
) -> HorizontalRunLayout {
    if run.glyphs.is_empty() {
        return HorizontalRunLayout {
            glyph_xs: Vec::new(),
            line_width_px: run.line_w,
            leading_hang_px: 0.0,
            trailing_hang_px: 0.0,
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
            // Takes the run off the byte-identical fast path when this glyph's own
            // face carries overrides; which PAIRS are overridden is decided below.
            .with_custom_pairs(custom_kerning.has_table(glyph.font_id))
        })
        .collect::<Vec<_>>();

    // Faux-bold advance growth per glyph (`2*d + expand_px`; all zeros when
    // faux bold is off anywhere in the run). The step from glyph `i-1` to `i`
    // grows by the PREVIOUS glyph's extra: thickening a glyph widens its own
    // ink, so it pushes the following glyph away. No cross-run carry is
    // needed: a cosmic-text 0.14 `LayoutRun` is one WHOLE laid-out line
    // (`LayoutRun.glyphs` spans every attrs/size/font boundary of the line,
    // see cosmic-text `buffer.rs`), so this accumulation already covers inline
    // boundaries; the trailing glyph's extra is folded into `line_width_px`
    // below instead.
    let mut faux_extras = Vec::with_capacity(run.glyphs.len());
    for glyph in run.glyphs {
        // The counter flag must be resolved BEFORE the advance is derived from
        // it, otherwise a counter-bearing glyph in the `outward_only` mode would
        // step by `2*d` while its ink grew by `4*d` and the next letter would
        // collide with it. The lookup is skipped entirely outside that mode.
        let style = resolve_faux_counter_flag(
            faux_style_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            ),
            font_system,
            outline_cache,
            glyph,
        );
        faux_extras.push(faux_advance_extra_px_for_glyph(
            params,
            inline_style_spans,
            layout_line_offsets,
            run.line_i,
            glyph,
            style,
        ));
    }

    if glyph_kernings
        .iter()
        .all(|kerning| kerning.uses_default_metric_layout())
        && faux_extras.iter().all(|extra| *extra == 0.0)
    {
        let glyph_xs = run.glyphs.iter().map(|glyph| glyph.x).collect::<Vec<_>>();
        let (leading_hang_px, trailing_hang_px) =
            hanging_metrics_for_layout(run, glyph_xs.as_slice(), run.line_w);
        return HorizontalRunLayout {
            glyph_xs,
            line_width_px: run.line_w,
            leading_hang_px,
            trailing_hang_px,
        };
    }

    // Optical kerning: re-space adjacent inked glyphs by true ink-to-ink gaps.
    // A run that cannot be optically kerned (fewer than one finite gap) returns
    // None and falls through to the metric accumulation below.
    if params.kerning_mode == KerningMode::Optical
        && let Some(layout) = optical_horizontal_run_layout(
            params,
            run,
            glyph_kernings.as_slice(),
            font_system,
            cache,
            outline_cache,
            contour_cache,
            layout_line_offsets,
            inline_style_spans,
            custom_kerning,
            font_size_px,
        )
    {
        return layout;
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
        // `Fixed` steps by each glyph's OWN (nominal, un-kerned) advance so font
        // GPOS/`kern` pair kerning is dropped; `Auto` keeps the shaped (kerned)
        // delta; `Optical` only reaches this branch as a fallback when the run
        // cannot be optically kerned, where it keeps the shaped delta like `Auto`.
        // Note: `prev.w == metric_advance` in cosmic-text (pair kerning is baked
        // into the advance), so the nominal metrics advance is the actual lever.
        //
        // A USER-AUTHORED pair overrides all three: the step becomes the left
        // glyph's raw `hmtx` advance plus the authored delta, so the font's own
        // pair value is dropped and the user's value takes its place verbatim.
        // That is what makes an authored pair behave exactly like a built-in one.
        // Both inputs are UNSIGNED, so the run's own direction is re-applied by
        // `custom_pair_step_px` (an RTL run walks the pen leftwards).
        let custom_delta_px = custom_pair_delta_px(custom_kerning, run.text, prev, glyph);
        let base_advance = match (custom_delta_px, params.kerning_mode) {
            (Some(delta_px), _) => custom_pair_step_px(
                custom_pair_magnitude_px(font_system, prev, metric_advance, delta_px),
                metric_advance,
                prev,
            ),
            (None, KerningMode::Fixed) => {
                let own = nominal_glyph_advance_px(font_system, prev).unwrap_or(metric_advance);
                optical_base_advance(own, metric_advance)
            }
            (None, KerningMode::Auto | KerningMode::Optical) => metric_advance,
        };
        let spacing_basis = metric_advance.abs().max(prev.w.max(default_advance));
        // Faux-bold growth of the PREVIOUS glyph's advance (0.0 when off),
        // floored so a thinning delta wider than that glyph's own advance cannot
        // step the pen backwards. Manual tracking stays OUTSIDE the floor: a
        // negative letter-spacing is an explicit request for overlap.
        let effective_advance = faux_floored_advance(base_advance, faux_extras[idx - 1]);
        current_x += effective_advance + pair_kerning.extra_spacing_px(spacing_basis);
        glyph_xs.push(current_x);
    }

    // Logical line width includes each glyph's own faux advance growth (the
    // trailing glyph's `2*d + expand` would otherwise be dropped, making
    // right/center alignment overshoot the margin by the faux growth). The same
    // `faux_floored_advance` rule as the pen above, so alignment can never be
    // computed from an effective advance the pen did not take.
    // `faux_extras` are exact zeros without faux, keeping this byte-identical.
    let line_width_px = run
        .glyphs
        .iter()
        .zip(glyph_xs.iter().copied())
        .zip(faux_extras.iter().copied())
        .map(|((glyph, glyph_x), faux_extra)| {
            glyph_x + faux_floored_advance(glyph.w, faux_extra)
        })
        .fold(0.0, f32::max);
    let (leading_hang_px, trailing_hang_px) =
        hanging_metrics_for_layout(run, glyph_xs.as_slice(), line_width_px);
    HorizontalRunLayout {
        glyph_xs,
        line_width_px,
        leading_hang_px,
        trailing_hang_px,
    }
}

/// Optical horizontal accumulation for one run.
///
/// MVP scope: optical pairs are considered only WITHIN a single layout run;
/// cosmic-text splits a line into runs at style/font/bidi boundaries, so a pair
/// that straddles a run boundary is spaced by the shaped advance only. This is a
/// known limitation of the horizontal optical path.
///
/// Algorithm (spec Phase 1):
/// 1. Base advance is each glyph's OWN shaped advance (`prev.w`), falling back to
///    the metric advance `cur.x - prev.x` when `prev.w` is not positive/finite.
/// 2. For every adjacent inked pair the MINIMUM DIRECTIONAL horizontal whitespace
///    is measured (`optical_pair_gap`, `OpticalAxis::Horizontal` — an adapter over
///    the owner `pair_gap::directional_pair_gap`): the smallest
///    `cur_left(y) - prev_right(y)` over the pair's overlapping vertical band (the
///    closest facing points), from the glyph outlines placed through the exact
///    draw-pass transform. This projected measure (not a Euclidean min-distance)
///    keeps slanted/overhanging features from inverting the sign. Spaces / empty /
///    outline-less glyphs and pairs with no vertical overlap yield an infinite gap
///    (delta 0).
/// 3. The self-calibrating target is the median of all finite per-pair MIN gaps;
///    fewer than one finite gap returns `None` (caller keeps metric spacing).
/// 4. Each pair is nudged by `optical_delta` (a signed delta normalized on the
///    pair MIN gap so the closest points become uniform, clamped to +/- font_size,
///    then floored on that same MIN gap so the closest points never collide)
///    applied ON TOP of the base advance and any manual tracking
///    (`extra_spacing_px`), with the same `spacing_basis` as the metric branch.
/// 5. A pair listed in the glyph font's `CustomKerningTable` skips steps 1/2/4
///    entirely and steps by `nominal_glyph_advance_px(prev) + authored delta`: an
///    override is an explicit instruction about that one distance, so normalizing
///    it toward the run's median gap would discard the value the user typed. The
///    remaining pairs of the run are still optical.
///
/// Returns `None` to signal "cannot optically kern this run".
// Threads the full per-run layout and per-glyph kernings plus the font system, both
// per-render caches, and layout params; a wrapper struct would just hide the wiring.
#[allow(clippy::too_many_arguments)]
fn optical_horizontal_run_layout(
    params: &TextRenderParams,
    run: &LayoutRun<'_>,
    glyph_kernings: &[KerningSettings],
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
    custom_kerning: &CustomKerningMap,
    font_size_px: f32,
) -> Option<HorizontalRunLayout> {
    let glyph_count = run.glyphs.len();
    if glyph_count < 2 {
        return None;
    }

    // Per-glyph draw scale, mirroring the draw pass (`inline_glyph_scale_for_glyph`)
    // so measured ink uses exactly the scale the glyph is rasterized with.
    let glyph_scales: Vec<GlyphScaleSettings> = run
        .glyphs
        .iter()
        .map(|glyph| {
            inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            )
        })
        .collect();
    // Per-glyph faux style, mirroring the draw pass: optical gaps MUST be
    // measured from the same (possibly offset/sheared) ink that is drawn.
    // The counter flag is deliberately NOT resolved here. Nothing on this path
    // reads it: the contour cache keys off `bold_key_bits()` (which excludes the
    // flag by contract, it being a function of the glyph), the outline variant is
    // selected by `bold` alone, and the placement uses only `shear_x`. The
    // optical branch adds no `outer_delta_px` pen term at all — it normalizes
    // measured ink gaps — so resolving the flag would buy a winding
    // classification per glyph and change nothing.
    let glyph_fauxes: Vec<FauxGlyphStyle> = run
        .glyphs
        .iter()
        .map(|glyph| {
            faux_style_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            )
        })
        .collect();

    // gaps[idx] is the minimum directional projected whitespace of the pair
    // (idx-1, idx) — the closest facing points; index 0 has no pair. The gap is
    // x-translation invariant (rotation 0), so both glyphs are placed relative to
    // a shared baseline: prev at pen 0, cur at pen prev.w. `f32::INFINITY` marks a
    // non-kernable pair (first glyph / space / empty / no overlap).
    let mut gaps: Vec<f32> = Vec::with_capacity(glyph_count);
    gaps.push(f32::INFINITY);
    for idx in 1..glyph_count {
        let prev = &run.glyphs[idx - 1];
        let cur = &run.glyphs[idx];
        let prev_placed = place_optical_horizontal_contour(
            prev,
            0.0,
            glyph_scales[idx - 1],
            glyph_fauxes[idx - 1],
            font_system,
            cache,
            outline_cache,
            contour_cache,
        );
        let cur_placed = place_optical_horizontal_contour(
            cur,
            prev.w,
            glyph_scales[idx],
            glyph_fauxes[idx],
            font_system,
            cache,
            outline_cache,
            contour_cache,
        );
        let gap = match (prev_placed, cur_placed) {
            (Some(p), Some(c)) => optical_pair_gap(&p, &c, OpticalAxis::Horizontal),
            // Space / empty / outline-less glyph on either side: not kernable.
            _ => f32::INFINITY,
        };
        gaps.push(gap);
    }

    // Self-calibrating target: the median of finite per-pair MIN gaps. None when
    // the run has no finite gap to normalize.
    let target = median_of_gaps(&gaps)?;

    let default_advance = font_size_px.max(1.0) * 0.5;
    let mut glyph_xs = Vec::with_capacity(glyph_count);
    let mut current_x = run.glyphs.first().map(|glyph| glyph.x).unwrap_or(0.0);
    glyph_xs.push(current_x);
    for idx in 1..glyph_count {
        let prev = &run.glyphs[idx - 1];
        let cur = &run.glyphs[idx];
        let metric_advance = cur.x - prev.x;
        // Keep the manual-tracking basis identical to the metric branch.
        let spacing_basis = metric_advance.abs().max(prev.w.max(default_advance));
        // A user-authored pair wins over the optical normalization too: the point
        // of an override is that the user dictated this one distance, so measuring
        // ink and moving the pair toward the run's median gap would overwrite the
        // very value they entered. Every OTHER pair of the run stays optical.
        // Direction is re-applied exactly as on the metric branch: the override's
        // inputs are unsigned, the run's step may not be.
        let step = match custom_pair_delta_px(custom_kerning, run.text, prev, cur) {
            Some(custom_delta_px) => custom_pair_step_px(
                custom_pair_magnitude_px(font_system, prev, metric_advance, custom_delta_px),
                metric_advance,
                prev,
            ),
            None => {
                optical_base_advance(prev.w, metric_advance)
                    + optical_delta(gaps[idx], target, font_size_px)
            }
        };
        current_x += step + glyph_kernings[idx].extra_spacing_px(spacing_basis);
        glyph_xs.push(current_x);
    }

    let line_width_px = run
        .glyphs
        .iter()
        .zip(glyph_xs.iter().copied())
        .map(|(glyph, glyph_x)| glyph_x + glyph.w)
        .fold(0.0, f32::max);
    let (leading_hang_px, trailing_hang_px) =
        hanging_metrics_for_layout(run, glyph_xs.as_slice(), line_width_px);
    Some(HorizontalRunLayout {
        glyph_xs,
        line_width_px,
        leading_hang_px,
        trailing_hang_px,
    })
}

/// Place a glyph's ink contour in world space using the exact transform the
/// horizontal draw pass (`draw_horizontal_placement`) uses, so the measured ink
/// matches the drawn ink.
///
/// `pen_x` is the pen x in layout px (the baseline y is irrelevant to the gap and
/// is fixed at 0). `faux` selects the faux-bold outline variant the contour is
/// derived from (the contour cache is keyed per variant, like `OutlineKey`) and
/// the faux-italic shear the placement applies, so the measured ink matches the
/// drawn ink exactly. Returns `None` for a space/empty glyph (zero-size
/// placement), an outline-less color glyph, or an empty contour — all of which
/// are treated as non-kernable by the caller. Never panics.
// Mirrors the draw-pass inputs; a wrapper struct would just hide the wiring.
#[allow(clippy::too_many_arguments)]
fn place_optical_horizontal_contour(
    glyph: &LayoutGlyph,
    pen_x: f32,
    glyph_scale: GlyphScaleSettings,
    faux: FauxGlyphStyle,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    outline_cache: &mut OutlineCache,
    contour_cache: &mut OpticalContourCache,
) -> Option<PlacedContour> {
    let physical = glyph.physical((pen_x, 0.0), 1.0);
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

    // Derive the ink contour once per distinct (font, glyph, em); the outline
    // itself is negatively cached by `OutlineCache`, so an outline-less glyph is
    // cheap to re-probe even without a contour-cache entry.
    let contour_key = (
        hash_font_id(glyph.font_id),
        glyph.glyph_id,
        glyph.font_size.to_bits(),
        faux.bold_key_bits(),
    );
    let contour = match contour_cache.entry(contour_key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let outline = resolve_outline_for_glyph(font_system, outline_cache, glyph, faux.bold)?;
            entry.insert(glyph_contour_from_outline(
                &outline,
                OPTICAL_CONTOUR_SIMPLIFY_TOLERANCE_PX,
            ))
        }
    };
    if contour.is_empty() {
        return None;
    }

    // Same baseline anchor as `draw_horizontal_placement`, so the measured ink is
    // the drawn ink. The baseline here is the probe pen (`pos_y = 0`), which the
    // gap measurement never reads, but the two paths must not diverge.
    let (dst_center_x, dst_center_y) = glyph_scale.scaled_center_about_baseline(
        src_left,
        src_top,
        glyph_w,
        glyph_h,
        src_top + placement_top,
    );
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
        faux.shear_x,
    );
    Some(transform.place_contour(contour))
}

fn prepare_source_text(source_text: &str, params: &TextRenderParams) -> String {
    // Ellipsis expansion runs FIRST, so every later step (sentence detection,
    // wrapping, hanging punctuation) sees the same `...` the reader will see.
    let source_text = if params.replace_ellipsis_with_dots {
        replace_ellipsis_with_dots(source_text)
    } else {
        source_text.to_string()
    };
    let source_text = if params.uppercase_text {
        source_text.to_uppercase()
    } else {
        source_text
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

/// Expands every `…` (U+2026 HORIZONTAL ELLIPSIS) into three ASCII periods.
///
/// Only U+2026 is expanded; other leader/ellipsis characters (`‥`, `⋯`, `︙`) are
/// left alone because they do not stand for exactly three dots. Returns the text
/// unchanged when it contains no `…`.
fn replace_ellipsis_with_dots(text: &str) -> String {
    // Cheap early-out: the common case is text without a single `…`, and
    // `replace` would still allocate a full copy.
    if !text.contains('…') {
        return text.to_string();
    }
    text.replace('…', "...")
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

/// Per-glyph faux (synthetic) bold/italic style resolved from the inline span
/// with fallback to the whole-overlay params.
///
/// `bold` carries the QUANTIZED outline-offset parameters at the glyph's own
/// em; `shear_x` is the baseline shear `tan(slant)` for faux italic (`0.0` =
/// none). The default (`NONE`) keeps every seam byte-identical to the pre-faux
/// renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FauxGlyphStyle {
    /// Faux-bold outline offset variant; `None` = plain outline.
    pub(crate) bold: Option<FauxOutlineParams>,
    /// Whether the glyph's PLAIN outline bounds a counter (hole). Only ever
    /// `true` after [`resolve_faux_counter_flag`] has looked the glyph up, and
    /// only for the `outward_only` faux-bold mode that acts on it; every other
    /// path leaves it `false` and pays nothing.
    pub(crate) bold_has_counter: bool,
    /// Faux-italic baseline shear (`tan(slant)`, positive = top leans right).
    pub(crate) shear_x: f32,
}

impl FauxGlyphStyle {
    /// No faux styling (plain outline, no shear).
    pub(crate) const NONE: Self = Self {
        bold: None,
        bold_has_counter: false,
        shear_x: 0.0,
    };

    /// SIGNED distance the glyph's OUTER ink boundary moves (`0.0` when faux
    /// bold is off, negative under thinning), counter compensation included.
    ///
    /// This — not the raw `d` — is what the pen advance, the ink profile and
    /// the bounds padding must reason about, because the counter-preserving
    /// mode doubles the outer step on a counter-bearing glyph.
    #[must_use]
    pub(crate) fn outer_delta_px(self) -> f32 {
        self.bold
            .map_or(0.0, |bold| bold.outer_delta_px(self.bold_has_counter))
    }

    /// Worst-case distance the faux-bold ink can stray OUTSIDE the plain ink,
    /// never negative and never signed — a thinning offset strays outside too
    /// (`0.0` only when faux bold is off); see
    /// [`FauxOutlineParams::max_overhang_px`].
    #[must_use]
    pub(crate) fn max_overhang_px(self) -> f32 {
        self.bold
            .map_or(0.0, |bold| bold.max_overhang_px(self.bold_has_counter))
    }

    /// Whether both faux effects are off.
    #[must_use]
    pub(crate) fn is_none(self) -> bool {
        self.bold.is_none() && self.shear_x == 0.0
    }

    /// Packed cache-key bits for the faux-bold variant (`0` = plain), shared
    /// with `OutlineKey` so every per-variant cache keys identically.
    #[must_use]
    pub(crate) fn bold_key_bits(self) -> u32 {
        faux_key_bits(self.bold)
    }
}

/// Slant (degrees) of the faux italic the renderer synthesizes when a REAL
/// italic was asked for but no italic face of the selected family is registered.
///
/// 12° is the `post.italicAngle` the Italic companions of the fonts this project
/// ships and tests against declare (Noto Sans Italic, Liberation Sans Italic), so
/// the synthesized shear lands where the real face would have. It is a fallback
/// default only: an explicit `faux_italic_slant_deg` (inline `<i=...>` or the
/// panel value) always wins and never reaches this path.
pub(crate) const SYNTHESIZED_ITALIC_SLANT_DEG: f32 = 12.0;

/// The slant to synthesize when the whole-overlay REAL italic request cannot be
/// served by the selected family, or `None` when nothing must change.
///
/// Returns `Some` only for a REAL italic request (`force_italic` WITHOUT an
/// explicit faux slant) whose `Style::Italic` variant of `base_attrs` no longer
/// selects the caller's own font — see [`family_has_matching_face`] for why that
/// is a total-loss render rather than a graceful degradation. The caller applies
/// the result by setting `faux_italic_slant_deg`, which turns the request into
/// the faux path everywhere at once (attrs, advances, bounds pads, every layout
/// mode) instead of patching one seam.
#[must_use]
fn synthesized_italic_slant_deg(
    font_system: &FontSystem,
    base_attrs: &Attrs<'_>,
    params: &TextRenderParams,
) -> Option<f32> {
    if !params.force_italic || params.faux_italic_slant_deg.is_some() {
        return None;
    }
    let italic_attrs = base_attrs.clone().style(cosmic_text::Style::Italic);
    if family_has_matching_face(font_system, &italic_attrs) {
        return None;
    }
    Some(SYNTHESIZED_ITALIC_SLANT_DEG)
}

/// Rewrites every REAL inline `<i>` span the registered fonts cannot serve into a
/// FAUX italic span, in place, and reports how many spans were degraded.
///
/// The span-level analog of [`synthesized_italic_slant_deg`], and it must run on
/// the MAPPED spans before they are used, because those same spans feed both the
/// attrs (`apply_inline_style_to_attrs`) and the per-glyph faux resolution
/// (`faux_italic_slant_at_offset`) on every layout mode — mutating them once here
/// is what keeps the two consistent. A span carrying an explicit `<i=slant>` is
/// already faux and is skipped; so is a span whose resolved family DOES ship an
/// italic face (a real `<i>` on such a font keeps its real face).
///
/// The returned count is the degradation signal the caller turns into the log and
/// user-visible warning; dropping it would make the fallback silent.
#[must_use]
fn degrade_unavailable_inline_italic(
    font_system: &FontSystem,
    base_attrs: &Attrs<'_>,
    inline_font_registry: &InlineFontRegistry,
    faux_face_baseline: FauxFaceBaseline,
    spans: &mut [InlineStyleSpan],
) -> usize {
    let mut degraded = 0usize;
    for span in spans.iter_mut() {
        if !span.italic || span.faux_italic_slant_deg.is_some() {
            continue;
        }
        // Resolve the span exactly as the shaper will (inline `<font=...>` family
        // and stretch included), then ask whether that resolved face can be
        // italic at all.
        let span_attrs =
            apply_inline_style_to_attrs(base_attrs, span, inline_font_registry, faux_face_baseline);
        if family_has_matching_face(font_system, &span_attrs.as_attrs()) {
            continue;
        }
        span.faux_italic_slant_deg = Some(SYNTHESIZED_ITALIC_SLANT_DEG);
        degraded += 1;
    }
    degraded
}

/// The faux-bold parameters to synthesize when the whole-overlay REAL bold request
/// cannot be served by the selected family, or `None` when nothing must change.
///
/// The weight analog of [`synthesized_italic_slant_deg`]. Returns `Some` only for a
/// REAL bold request (`force_bold` WITHOUT explicit faux params) whose family ships no
/// face at `Weight::BOLD` — see [`family_has_face_of_requested_weight`] for the two
/// silent losses that request would otherwise cause (the run jumps to the bundled
/// `Noto Sans Bold`, and every weight-filtered fallback pass becomes unreachable, so
/// rare-plane CJK in the same run turns into tofu).
///
/// The synthesized strength is [`FauxBoldParams::default`], i.e. exactly the
/// `<b=default>` inline tag, so the fallback has one documented meaning.
#[must_use]
fn synthesized_bold_params(
    font_system: &FontSystem,
    base_attrs: &Attrs<'_>,
    params: &TextRenderParams,
) -> Option<FauxBoldParams> {
    if !params.force_bold || params.faux_bold.is_some() {
        return None;
    }
    let bold_attrs = base_attrs.clone().weight(cosmic_text::Weight::BOLD);
    if family_has_face_of_requested_weight(font_system, &bold_attrs) {
        return None;
    }
    Some(FauxBoldParams::default())
}

/// Rewrites every REAL inline `<b>` span whose resolved family ships no bold face
/// into a FAUX bold span, in place, and reports how many spans were degraded.
///
/// The span-level analog of [`synthesized_bold_params`], and the exact counterpart of
/// [`degrade_unavailable_inline_italic`]: it must run on the MAPPED
/// spans before anything reads them, because those spans feed both the attrs
/// (`apply_inline_style_to_attrs`) and the per-glyph faux resolution
/// (`faux_bold_params_at_offset`). A span carrying explicit `<b=...>` params is
/// already faux and is skipped; so is a span whose resolved family DOES ship a bold
/// face (a real `<b>` on such a font keeps its real face).
#[must_use]
fn degrade_unavailable_inline_bold(
    font_system: &FontSystem,
    base_attrs: &Attrs<'_>,
    inline_font_registry: &InlineFontRegistry,
    faux_face_baseline: FauxFaceBaseline,
    spans: &mut [InlineStyleSpan],
) -> usize {
    let mut degraded = 0usize;
    for span in spans.iter_mut() {
        if !span.bold || span.faux_bold.is_some() {
            continue;
        }
        // Resolve the span exactly as the shaper will (inline `<font=...>` family and
        // its own weight included), then ask whether that family can be bold at all.
        let span_attrs =
            apply_inline_style_to_attrs(base_attrs, span, inline_font_registry, faux_face_baseline);
        if family_has_face_of_requested_weight(font_system, &span_attrs.as_attrs()) {
            continue;
        }
        span.faux_bold = Some(FauxBoldParams::default());
        degraded += 1;
    }
    degraded
}

/// Whether the whole-overlay base attrs request the REAL Bold/Italic faces.
///
/// The pair is `(bold, italic)`. Real face matching happens only when the
/// force flag is set WITHOUT faux params; `force_* && faux present` keeps the
/// SELECTED face (the faux geometry is synthesized at the glyph seam), and faux
/// params without the force flag are ignored entirely.
#[must_use]
pub(crate) fn base_attrs_real_bold_italic(params: &TextRenderParams) -> (bool, bool) {
    (
        params.force_bold && params.faux_bold.is_none(),
        params.force_italic && params.faux_italic_slant_deg.is_none(),
    )
}

/// Effective faux-bold parameters at a plain-text byte offset.
///
/// Resolution contract: a span that sets bold decides for itself — `Some` faux
/// params mean faux bold, `None` means the real Bold face (no faux, even if
/// the whole overlay is faux). A glyph outside any bold span falls back to the
/// whole-overlay pair: `faux_bold` takes effect ONLY when `force_bold` is set.
fn faux_bold_params_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<FauxBoldParams> {
    if let Some(style) = spans.and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        && style.bold
    {
        return style.faux_bold;
    }
    if params.force_bold {
        params.faux_bold
    } else {
        None
    }
}

/// Effective faux-italic slant (degrees) at a plain-text byte offset; the same
/// span-over-global resolution as [`faux_bold_params_at_offset`].
fn faux_italic_slant_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<f32> {
    if let Some(style) = spans.and_then(|style_spans| inline_style_at_offset(style_spans, offset))
        && style.italic
    {
        return style.faux_italic_slant_deg;
    }
    if params.force_italic {
        params.faux_italic_slant_deg
    } else {
        None
    }
}

/// Resolve the faux style at `offset` for a glyph shaped at `em_px`.
///
/// Converts the percent-domain params to the glyph's own em: bold offset
/// `d = thicken/100 * em` (quantized by `FauxOutlineParams::new`; zero-strength
/// resolves to `None`, a NEGATIVE `thicken_percent` resolves to a thinning
/// offset), italic shear `tan(slant)` with the documented `-45..=45` degree
/// clamp.
///
/// The returned style carries `bold_has_counter == false`; a caller that has
/// the font at hand must pass it through [`resolve_faux_counter_flag`] before
/// deriving advances or bounds from it.
pub(crate) fn faux_style_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
    em_px: f32,
) -> FauxGlyphStyle {
    let bold = faux_bold_params_at_offset(params, spans, offset).and_then(|faux| {
        let d_px = faux.thicken_percent.clamp(FAUX_THICKEN_PERCENT_MIN, FAUX_THICKEN_PERCENT_MAX)
            / 100.0
            * em_px.max(0.0);
        FauxOutlineParams::new(d_px, faux.sharp_corners, faux.outward_only)
    });
    let shear_x = faux_italic_slant_at_offset(params, spans, offset)
        .map_or(0.0, |slant| slant.clamp(-45.0, 45.0).to_radians().tan());
    FauxGlyphStyle {
        bold,
        bold_has_counter: false,
        shear_x,
    }
}

/// Fill in `FauxGlyphStyle::bold_has_counter` for a laid-out glyph.
///
/// A no-op — and, crucially, NO outline lookup — unless faux bold is active in
/// the counter-preserving (`outward_only`) mode, the only mode whose geometry
/// depends on whether the glyph bounds a counter. Every other render therefore
/// pays exactly nothing for this step. Never panics.
///
/// Call it ONLY where the flag is actually consumed — i.e. before
/// [`FauxGlyphStyle::outer_delta_px`] / [`FauxGlyphStyle::max_overhang_px`]
/// (pen advance, ink profile, bounds pads). The optical path does not: its
/// caches key off `bold_key_bits()`, which excludes the flag by contract, and
/// it derives no distance from the style, so resolving it there would be a
/// per-glyph winding classification with no observable effect.
#[must_use]
pub(crate) fn resolve_faux_counter_flag(
    style: FauxGlyphStyle,
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
    glyph: &LayoutGlyph,
) -> FauxGlyphStyle {
    if !style.bold.is_some_and(|bold| bold.outward_only()) {
        return style;
    }
    FauxGlyphStyle {
        bold_has_counter: glyph_outline_has_counter(font_system, outline_cache, glyph),
        ..style
    }
}

/// [`faux_style_at_offset`] for a laid-out glyph (em = `glyph.font_size`).
pub(crate) fn faux_style_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> FauxGlyphStyle {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    faux_style_at_offset(
        params,
        spans,
        line_offset + glyph.start.min(glyph.end),
        glyph.font_size,
    )
}

/// Horizontal pen-advance growth for one glyph under faux bold:
/// `2 * outer_delta + expand_px`, where `outer_delta` is the SIGNED distance
/// the glyph's outer ink boundary moves (`FauxGlyphStyle::outer_delta_px`, so
/// counter compensation and thinning are both included) and
/// `expand_px = expand_percent/100 * em`.
///
/// `style` must already carry the resolved counter flag
/// ([`resolve_faux_counter_flag`]); passing an unresolved style would under-step
/// a counter-bearing glyph in the `outward_only` mode and let the next letter
/// collide with its ink. The result is legitimately NEGATIVE under thinning
/// (letters pull together as their ink shrinks); the caller combines it with
/// the glyph's own advance through [`faux_floored_advance`], which is what keeps
/// a contraction larger than that advance from reversing the pen. Exactly `0.0`
/// when faux bold is off, which keeps the non-faux layout byte-identical.
/// Optical kerning deliberately does NOT add this term: it re-normalizes true
/// ink gaps measured from the already-offset outlines, so the thickening is
/// accounted for by measurement.
///
/// PER-GLYPH vs PER-RING (the `outward_only` mode only): `style.bold_has_counter`
/// says "some ring of this glyph bounds a counter", while
/// `vector::offset_outline` decides the `2*d` compensation PER RING — only an
/// outer ring that actually contains a retained counter takes it. For a glyph
/// that mixes a counter-bearing ring with a wider counter-less one (`Ы`, `Ю`
/// drawn as two rings) the flag is `true` while one ring is uncompensated, so
/// the ink grows by `3*d` and this term reserves `4*d`: the step OVERSHOOTS by
/// `d`. That direction is deliberate — an over-step only loosens the spacing,
/// whereas an under-step would let the next letter collide with drawn ink. The
/// default uniform mode (`outward_only == false`) has no compensation and no
/// asymmetry.
fn faux_advance_extra_px_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
    style: FauxGlyphStyle,
) -> f32 {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    let offset = line_offset + glyph.start.min(glyph.end);
    let Some(faux) = faux_bold_params_at_offset(params, spans, offset) else {
        return 0.0;
    };
    let em = glyph.font_size.max(0.0);
    2.0 * style.outer_delta_px() + faux.expand_percent.clamp(0.0, 50.0) / 100.0 * em
}

/// Pre-scale bounds padding `[pad_x, pad_y]` (bitmap-local px) for a glyph's
/// faux style: the swash bitmap placement box under-reports faux ink.
///
/// Bold pads both axes by the worst-case offset overhang (the outer delta, or
/// the miter limit `4x` that for sharp corners — counter compensation
/// included, so `faux` must already carry the resolved counter flag from
/// [`resolve_faux_counter_flag`]). A THINNING (negative) offset pads by the same
/// MAGNITUDE, not by zero: an inward offset also strays outside the source ink,
/// because the miter apex at a REFLEX vertex of an outer ring is pushed up to
/// `4*|d|` along the inward bisector and pierces any material thinner than that
/// (see [`FauxOutlineParams::max_overhang_px`]). Italic pads x by the worst-case baseline-shear
/// overhang `|shear_x| * max_dy * height_mul / width_mul`, where `max_dy` is
/// the farthest bitmap-box edge from the baseline (plus the bold overhang); the
/// `height/width` ratio converts the post-scale overhang into the pre-scale
/// units `include_scaled_rect_bounds`/`scaled_rect` expect. Returns exact
/// `[0.0, 0.0]` when faux is off, so subtracting/adding the pads keeps the
/// non-faux bounds bit-identical, and never returns a negative pad.
pub(crate) fn faux_bounds_pads(
    faux: FauxGlyphStyle,
    placement_top: f32,
    glyph_h: f32,
    width_mul: f32,
    height_mul: f32,
) -> [f32; 2] {
    if faux.is_none() {
        return [0.0, 0.0];
    }
    let overhang = faux.max_overhang_px();
    let mut pad_x = overhang;
    if faux.shear_x != 0.0 {
        // The bitmap box spans y in [-placement_top, glyph_h - placement_top]
        // around the baseline (y-down), so the extreme |y| is the larger edge.
        let max_dy = placement_top.abs().max((glyph_h - placement_top).abs()) + overhang;
        pad_x += faux.shear_x.abs() * max_dy * height_mul / width_mul.max(0.01);
    }
    [pad_x, overhang]
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
        // Purely style-driven; the per-FONT override flag is stamped on afterwards
        // by `with_custom_pairs`, which is the only thing that knows the face.
        custom_pairs: false,
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

/// Per-line extra spacing in px, driven by the LINE-level inline tags only.
///
/// Entry `i` is the extra spacing of layout line `i`: `<line-spacing=px,%>` spans
/// touching that line override `params.line_spacing_px`/`line_spacing_percent`
/// (last overlapping span wins — a `<line-spacing>` tag is a request ABOUT the
/// line, so touching the line is enough), and the result is coupled to the GLOBAL
/// `glyph_height_percent` through [`effective_spacing_percent`], the documented
/// whole-text rule (`ms-tab-typing/src/panel/MODULE_README.md`).
///
/// A per-character `<stretching=W%,H%>` span deliberately does NOT feed this
/// table: it is not a statement about the line, and letting it in re-spaced the
/// whole line (and, on the vertical path, the whole COLUMN GAP) because one word
/// was scaled. The room a TALL inline span needs is added instead, grow-only and
/// on the correct side, by [`line_baseline_advance_table`].
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
        }
        let effective_percent =
            effective_spacing_percent(spacing_percent, params.glyph_height_percent);
        out.push(spacing_px + font_size_px * (effective_percent / 100.0));
    }
    if out.is_empty() {
        out.push(default_extra_line_spacing_px);
    }
    out
}

/// Grow-only vertical room, in px, that inline `<stretching>` height spans ask
/// for ABOVE each layout line.
///
/// `above(i)` is how much higher a height-scaled run on layout line `i` reaches
/// than the same run at the global `glyph_height_percent` would. It is measured
/// as REAL INK: the glyph outline's own extent above the baseline
/// (`Outline::local_bbox`, whose frame has `y = 0` on the baseline), taken at the
/// glyph's own em from the glyph's OWN face and faux-bold variant — the same
/// resolver the draw pass rasterizes — times that glyph's excess multiplier.
///
/// MAXIMUM, NEVER A SUM. A line carrying several height tags asks for the LARGEST
/// rise any one of them needs, not their total: two 150 % spans on one line space
/// exactly like one, and a 150 % span beside a 200 % one spaces exactly like the
/// 200 % span alone, wherever the larger one sits in reading order. Every glyph
/// folds into its line's entry with `max`; an accumulating `+=` here would be a
/// defect, not an optimisation.
///
/// The face `ascent` metric is deliberately NOT
/// used: it carries headroom well above the actual ink of most lines (a line of
/// x-height glyphs tops out around 0.53 em against a ~0.9 em ascent), so scaling
/// it makes the gap grow visibly faster than the letters do.
///
/// GROW UPWARD ONLY. There is no "below" side by design: a tall span enlarges
/// only the gap ABOVE its own line, never the one below it. A large multiplier
/// therefore pushes a descender (`р`, `у`, `д`) further down and it may crowd the
/// following line — that is the ACCEPTED behaviour, chosen over spacing that
/// grows on both sides; raising the line spacing is the author's lever. Do not
/// "fix" this by re-adding a descent term.
///
/// The value is `0.0` on every line no `<stretching>` span makes taller than the
/// global `glyph_height_percent`, so the whole mechanism is inert without the tag.
#[derive(Debug, Clone, Default)]
pub(crate) struct InlineHeightRoom {
    above_px: Vec<f32>,
}

impl InlineHeightRoom {
    /// Extra ink above line `line_idx`'s baseline; `0.0` past the last line.
    #[must_use]
    pub(crate) fn above(&self, line_idx: usize) -> f32 {
        self.above_px.get(line_idx).copied().unwrap_or(0.0)
    }

    /// Test-only constructor taking the exact per-line room in px, so the advance
    /// arithmetic can be pinned without shaping a buffer.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_above(above_px: Vec<f32>) -> Self {
        Self { above_px }
    }

    /// Measure the room from the SHAPED buffer, per layout run.
    ///
    /// A glyph contributes only the part of its height that exceeds the global
    /// `glyph_height_percent` (`height_mul - global_mul`, clamped at zero), which
    /// is what makes the rule grow-only; the multiplier comes from
    /// [`inline_glyph_scale_for_glyph`] and the faux variant from
    /// [`faux_style_for_glyph`], the same resolutions the draw pass uses, so the
    /// measured ink is the drawn ink. Run index and `layout_line_offsets` index
    /// are the same line here (the layout text is pre-wrapped), exactly as in
    /// [`compute_horizontal_line_baselines`].
    ///
    /// Only glyphs that actually carry an excess are looked up, through a local
    /// [`OutlineCache`] — a handful of extractions per render, none at all
    /// without a `<stretching>` tag. A glyph with no fillable outline (a color /
    /// emoji or embedded-bitmap glyph) has no outline box to read and falls back
    /// to its face `ascent`: an over-allocation, which is the safe side of a
    /// grow-only rule, and far cheaper than rasterizing it here just to measure.
    #[must_use]
    pub(crate) fn measure(
        params: &TextRenderParams,
        buffer: &Buffer,
        font_system: &mut FontSystem,
        layout_line_offsets: &[usize],
        inline_style_spans: Option<&[InlineStyleSpan]>,
    ) -> Self {
        let line_count = layout_line_offsets.len().max(1);
        let mut room = Self { above_px: vec![0.0; line_count] };
        // No stretch tag anywhere -> no room to add, and no per-glyph lookup.
        if !inline_style_spans.is_some_and(|spans| {
            spans.iter().any(|span| span.glyph_stretch_percent.is_some())
        }) {
            return room;
        }
        let global_mul = (params.glyph_height_percent / 100.0).clamp(0.01, 3.0);
        let mut outline_cache = OutlineCache::new();
        // (face, em) -> ascent px, for the outline-less fallback only.
        let mut face_ascents: std::collections::HashMap<(u64, u32), f32> =
            std::collections::HashMap::new();
        for (line_idx, run) in buffer.layout_runs().enumerate() {
            while room.above_px.len() <= line_idx {
                room.above_px.push(0.0);
            }
            for glyph in run.glyphs {
                let excess = inline_glyph_scale_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    line_idx,
                    glyph,
                )
                .height_mul
                    - global_mul;
                if excess <= 0.0 {
                    continue;
                }
                let faux = faux_style_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    line_idx,
                    glyph,
                );
                let ink_rise_px = match resolve_outline_for_glyph(
                    font_system,
                    &mut outline_cache,
                    glyph,
                    faux.bold,
                ) {
                    // The outline frame is y-down with `y = 0` on the baseline,
                    // so the topmost ink is the MOST NEGATIVE y. A glyph whose
                    // ink lies entirely below the baseline rises by nothing.
                    Some(outline) => (-outline.local_bbox().0[1]).max(0.0),
                    None => {
                        let key = (hash_font_id(glyph.font_id), glyph.font_size.to_bits());
                        *face_ascents.entry(key).or_insert_with(|| {
                            font_system
                                .get_font(glyph.font_id)
                                .map(|font| {
                                    font.as_swash()
                                        .metrics(&[])
                                        .scale(glyph.font_size)
                                        .ascent
                                        .max(0.0)
                                })
                                // Unreachable for a glyph this very `FontSystem`
                                // shaped; the nominal em over-allocates, which is
                                // the safe direction for a grow-only rule.
                                .unwrap_or(glyph.font_size)
                        })
                    }
                };
                // MAX, never `+=`: several height tags on one line ask for the
                // largest rise among them, not for their total.
                room.above_px[line_idx] = room.above_px[line_idx].max(ink_rise_px * excess);
            }
        }
        room
    }
}

/// Baseline-advance table for the paths that stack lines on BASELINES: the
/// line-level [`compute_line_extra_spacing_table`] plus the grow-only room an
/// inline `<stretching>` height span needs.
///
/// Entry `i` is consumed by [`compute_horizontal_line_baselines`] as the extra
/// advance of the gap BELOW line `i`, i.e. the gap ABOVE line `i+1`. Since the
/// height scale is anchored at the baseline, a span taller than the global height
/// reaches higher above its own baseline, so gap `i` makes room for line `i+1`'s
/// extra ink rise — [`InlineHeightRoom::above`] and nothing else. The term is
/// non-negative: the line box may grow, never shrink.
///
/// UPWARD ONLY, and a MAXIMUM rather than a sum. A tall span never enlarges the
/// gap BELOW its own line (its descenders may crowd the next line; see
/// [`InlineHeightRoom`] for why that is the accepted trade), and several height
/// tags on one line ask for the largest of their individual rises, never their
/// total — that maximum is taken per line inside [`InlineHeightRoom::measure`].
///
/// The vertical path must NOT use this table — there the same entries are COLUMN
/// GAPS, where an ink-rise term is meaningless; it keeps the plain
/// [`compute_line_extra_spacing_table`].
#[must_use]
pub(crate) fn line_baseline_advance_table(
    params: &TextRenderParams,
    layout_text: &str,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
    font_size_px: f32,
    default_extra_line_spacing_px: f32,
    inline_height_room: &InlineHeightRoom,
) -> Vec<f32> {
    let mut table = compute_line_extra_spacing_table(
        params,
        layout_text,
        layout_line_offsets,
        inline_style_spans,
        font_size_px,
        default_extra_line_spacing_px,
    );
    for (line_idx, entry) in table.iter_mut().enumerate() {
        *entry += inline_height_room.above(line_idx + 1);
    }
    table
}

/// Pen baseline (content px, y down) of every layout run in `buffer`.
///
/// `line_advance_table` must come from [`line_baseline_advance_table`] — entry
/// `i` is the extra advance of the gap BELOW line `i`, so it shifts lines `i+1..`
/// and never line `i` itself. `default_extra_line_spacing_px` is the value the
/// uniform ladder in [`horizontal_run_baseline_y`] already contains; it cancels
/// out of the telescoping sum there, and only matters in the
/// `has_inline_size_overrides` branch, which replaces the ladder with
/// cosmic-text's own `run.line_y`.
pub(crate) fn compute_horizontal_line_baselines(
    buffer: &Buffer,
    base_line_height_px: f32,
    default_extra_line_spacing_px: f32,
    line_advance_table: &[f32],
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
        cumulative_delta += line_advance_table
            .get(line_idx)
            .copied()
            .unwrap_or(default_extra_line_spacing_px)
            - default_extra_line_spacing_px;
    }
    baselines
}

/// Baseline of one layout run BEFORE the per-gap advance corrections.
///
/// With no inline `<size>` tag this is the uniform ladder
/// `anchor + i * (base_line_height + extra)`; when a `<size>` tag exists the
/// shaper owns the line box and cosmic-text's own `run.line_y` is used instead.
pub(crate) fn horizontal_run_baseline_y(
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
    faux_face_baseline: FauxFaceBaseline,
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
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        layout_line_offsets,
        run,
        next,
    );
    let hyphen_attrs = hyphen_attrs.as_attrs();
    build_hard_hyphen_glyph(font_system, &hyphen_attrs, font_size_px, line_height_px)
}

/// Attrs of the synthesized wrap hyphen: `base_attrs` plus the inline style
/// active at the consumed soft hyphen. `faux_face_baseline` is the selected
/// face's weight/style a faux span falls back to, so the hyphen matches exactly
/// the same face as the text around it.
fn wrapped_hyphen_attrs<'a>(
    base_attrs: &Attrs<'a>,
    faux_face_baseline: FauxFaceBaseline,
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
    apply_inline_style_to_attrs(base_attrs, style, inline_font_registry, faux_face_baseline)
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

/// Widths of the run's LEADING and TRAILING hanging-punctuation runs, in px.
///
/// Returns `(leading_hang_px, trailing_hang_px)`, both `>= 0`, measured against
/// the pen positions `glyph_xs` (which must be one per glyph of `run`) and the
/// run's logical `line_width_px`. Only the EDGE runs hang: punctuation between
/// two ordinary glyphs is never counted.
///
/// Returns `(0.0, 0.0)` — "nothing hangs" — for an empty/mismatched run and for a
/// DEGENERATE line whose glyphs are all hanging punctuation: hanging such a line
/// would leave no visual width to align by, so it is aligned as if the feature
/// were off. That guard is independent of the hanging strength, so the layout
/// does not jump as the strength is raised.
///
/// The caller applies the strength (`TextRenderParams::hanging_weight`); this
/// function knows nothing about it and never touches `glyph_xs`.
fn hanging_metrics_for_layout(
    run: &LayoutRun<'_>,
    glyph_xs: &[f32],
    line_width_px: f32,
) -> (f32, f32) {
    if run.glyphs.is_empty() || glyph_xs.len() != run.glyphs.len() {
        return (0.0, 0.0);
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
    // The width the line would be aligned by at FULL strength. Computed here (and
    // with the same left-to-right subtraction the caller uses) only to detect the
    // degenerate all-hanging line.
    let visual_width_px = (line_width_px - leading_hang_px - trailing_hang_px).max(0.0);

    if visual_width_px <= f32::EPSILON {
        (0.0, 0.0)
    } else {
        (leading_hang_px, trailing_hang_px)
    }
}

/// `true` when the glyph's source text is made up entirely of hanging
/// punctuation. `text` must be the glyph's OWN run text, since `LayoutGlyph`
/// byte offsets are run-relative.
///
/// Uses this module's [`is_hanging_punctuation`] set, the SAME predicate the
/// layout-level hang metrics use, so the exclusion and the visual hang can never
/// disagree about what hangs.
#[must_use]
pub(crate) fn glyph_is_hanging_punctuation(text: &str, glyph: &LayoutGlyph) -> bool {
    let end = glyph.end.min(text.len());
    let start = glyph.start.min(end);
    let Some(slice) = text.get(start..end) else {
        return false;
    };
    !slice.is_empty() && slice.chars().all(is_hanging_punctuation)
}

/// Index bounds of a layout run's non-hanging glyphs: returns `(leading_end,
/// trailing_start)` such that glyphs at index `< leading_end` or `>=
/// trailing_start` belong to the line's LEADING/TRAILING hanging-punctuation
/// runs and must be excluded from the extra-info sampling (same edge-run
/// semantics as [`hanging_metrics_for_layout`]). A run that is entirely hanging
/// punctuation (or empty) excludes nothing and returns `(0, len)`.
///
/// Shared with the formula/custom-line draw paths (`formula::render`), which hang
/// punctuation through the same horizontal wrap. `TextLineMode::Vertical` is NOT
/// a caller: its wrap never hangs punctuation, so nothing hangs there to exclude.
pub(crate) fn hanging_edge_run_bounds(run: &LayoutRun<'_>) -> (usize, usize) {
    let len = run.glyphs.len();
    let Some(first_non) = run
        .glyphs
        .iter()
        .position(|glyph| !glyph_is_hanging_punctuation(run.text, glyph))
    else {
        return (0, len);
    };
    let last_non = run
        .glyphs
        .iter()
        .rposition(|glyph| !glyph_is_hanging_punctuation(run.text, glyph))
        .unwrap_or(first_non);
    (first_non, last_non + 1)
}

/// `true` when the glyph at `index` is in the leading/trailing hanging run given
/// `(leading_end, trailing_start)` from [`hanging_edge_run_bounds`].
#[must_use]
pub(crate) fn is_edge_run_hanging(bounds: (usize, usize), index: usize) -> bool {
    index < bounds.0 || index >= bounds.1
}

/// Content-space placement-box `(corners, center)` of a normal (unrotated)
/// horizontal glyph, matching the scaled draw box (per-glyph width/height scale,
/// no rotation). Corners are counter-clockwise starting at the top-left.
#[must_use]
fn horizontal_placement_extra_samples(
    placement: &HorizontalGlyphPlacement,
) -> ([[f32; 2]; 4], [f32; 2]) {
    let (left, top, width, height) = placement.scale.scaled_rect_about_baseline(
        placement.src_left_i as f32,
        placement.src_top_i as f32,
        placement.glyph_w as f32,
        placement.glyph_h as f32,
        placement.baseline_y(),
    );
    let corners = [
        [left, top],
        [left + width, top],
        [left + width, top + height],
        [left, top + height],
    ];
    let center = [left + width * 0.5, top + height * 0.5];
    (corners, center)
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

#[cfg(test)]
mod tests {
    use super::{
        FauxGlyphStyle, KerningSettings, SYNTHESIZED_ITALIC_SLANT_DEG, apply_effects_to_image,
        base_attrs_real_bold_italic, faux_bounds_pads, prepare_source_text,
        replace_ellipsis_with_dots, single_char_cluster,
    };
    use crate::vector::FauxOutlineParams;
    use crate::font_provider::{FontContent, FontContentSet, font_content_id};
    use crate::types::{
        AntiAliasingMode, FauxBoldParams, HorizontalAlign, KerningMode, RenderExtraInfoRequest,
        RenderedTextImage, TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode,
        TextLineMode, TextRenderParams, TextRenderShapeCompareParams, TextShape, TextVectorLine,
        TextVectorLineDistanceMode, TextVectorLineTextDirection, TextVectorLinesLayoutParams,
        TextVectorPoint, TextWrapMode, VectorMeshWarp, VerticalLineDirection,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_font_path() -> PathBuf {
        // Fixture lives at the workspace root; this crate sits two levels down
        // (crates/ms-text-render), so anchor CARGO_MANIFEST_DIR up two dirs.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// Builds a single-font provider that resolves `params.font_name` to the test
    /// fixture bytes, so the render path never touches the filesystem itself.
    fn font_provider(params: &TextRenderParams) -> FontContentSet {
        let bytes = std::fs::read(test_font_path()).unwrap_or_default();
        let content_id = font_content_id(&bytes);
        FontContentSet::new(vec![FontContent {
            name: params.font_name.clone(),
            original_name: params.font_name.clone(),
            data: Arc::new(bytes),
            face_index: params.selected_face_index,
            content_id,
            custom_kerning: None,
        }])
    }

    /// Local test wrapper matching the pre-refactor `render_text_to_image(&params,
    /// cancel)` call shape: builds the fixture font provider from
    /// `params.font_name` and delegates to the real entry point.
    fn render_text_to_image(
        params: &TextRenderParams,
        cancel: Option<(&Arc<AtomicU64>, u64)>,
    ) -> Result<RenderedTextImage, String> {
        crate::pipeline::render_text_to_image(params, &font_provider(params), cancel)
    }

    fn base_params() -> TextRenderParams {
        TextRenderParams {
            text: "Hello world".to_string(),
            text_color: [255, 255, 255, 255],
            font_name: "test-font".to_string(),
            font_size_px: 36.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 100.0,
            kerning_mode: KerningMode::Auto,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 256,
            align: HorizontalAlign::LEFT,
            selected_face_index: 0,
            force_bold: false,
            force_italic: false,
            faux_bold: None,
            faux_italic_slant_deg: None,
            uppercase_text: false,
            trim_extra_spaces: true,
            replace_ellipsis_with_dots: true,
            force_remove_ellipsis_glyph: false,
            hanging_punctuation: 0.0,
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
            global_rotation_deg: 0.0,
            line_placement_percent: 0.0,
            line_placement_reference: crate::types::LinePlacementReference::GlyphHeight,
            raster_transform: None,
            extra_info: crate::types::RenderExtraInfoRequest::default(),
        }
    }

    // The optical pure-numeric core (`median_of_gaps`, `optical_delta`,
    // `optical_base_advance`) is unit-tested in `super::super::optical` since it
    // is shared by the horizontal and vertical paths.

    #[test]
    fn fixed_kerning_drops_font_pair_kerning_versus_auto() {
        // Text loaded with negative-kern pairs (AV/VA/To/Yo/Wa in LiberationSans).
        // `Auto` applies the font's GPOS/`kern` pair kerning (shaped positions);
        // `Fixed` steps by each glyph's OWN advance, so the pairs are NOT pulled
        // together. With kern pairs present, the two renders must differ.
        let kern_text = "AVA To Yo Wa VA";
        let mut auto_params = base_params();
        auto_params.text = kern_text.to_string();
        auto_params.width_px = 640;
        auto_params.kerning_mode = KerningMode::Auto;

        let mut fixed_params = auto_params.clone();
        fixed_params.kerning_mode = KerningMode::Fixed;

        let auto = render_text_to_image(&auto_params, None)
            .expect("Auto kerning render should succeed");
        let fixed = render_text_to_image(&fixed_params, None)
            .expect("Fixed kerning render should succeed");

        // Fixed spacing is looser (own advance, no negative kern), so the inked
        // content is at least as wide as Auto and the buffers differ.
        assert!(
            fixed.width >= auto.width,
            "Fixed own-advance spacing should be no narrower than Auto (fixed {} vs auto {})",
            fixed.width,
            auto.width
        );
        assert_ne!(
            (auto.width, auto.height, &auto.rgba),
            (fixed.width, fixed.height, &fixed.rgba),
            "Fixed must drop font pair kerning and differ from Auto for kern-pair text"
        );
    }

    #[test]
    fn auto_kerning_matches_default_shaped_positions() {
        // `Auto` with zero manual tracking is the fast byte-identical shaped-position
        // path (the historical `Metric` behavior). Rendering the same text twice is
        // deterministic and stable across the enum rename.
        let mut params = base_params();
        params.text = "AVA To Yo".to_string();
        params.kerning_mode = KerningMode::Auto;
        let a = render_text_to_image(&params, None).expect("Auto render should succeed");
        let b = render_text_to_image(&params, None).expect("Auto render should succeed");
        assert_eq!((a.width, a.height, a.rgba), (b.width, b.height, b.rgba));
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

    /// Alpha that counts as ink for the glyph-geometry probes below. Above the
    /// anti-aliasing fringe of an outline edge, so a row/column is reported as
    /// ink only where the glyph really covers it.
    const INK_ALPHA: u8 = 32;

    /// Ink bounding box `(min_x, max_x, min_y, max_y)` of every CONNECTED run of
    /// inked columns, split on fully empty columns and returned left to right.
    ///
    /// With space-separated glyphs this is one entry per glyph, so `max_y` is
    /// that glyph's ink BOTTOM — the row a baseline-anchored glyph shares with
    /// its unscaled neighbours (`H`, `.` and friends have no descender).
    fn ink_column_clusters(image: &RenderedTextImage) -> Vec<(usize, usize, usize, usize)> {
        let width = image.width as usize;
        let height = image.height as usize;
        let mut clusters: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut open: Option<(usize, usize, usize, usize)> = None;
        for x in 0..width {
            let mut column_min_y = None;
            let mut column_max_y = 0usize;
            for y in 0..height {
                if image.rgba[(y * width + x) * 4 + 3] < INK_ALPHA {
                    continue;
                }
                column_min_y.get_or_insert(y);
                column_max_y = y;
            }
            match (column_min_y, open.as_mut()) {
                (Some(min_y), Some(current)) => {
                    current.1 = x;
                    current.2 = current.2.min(min_y);
                    current.3 = current.3.max(column_max_y);
                }
                (Some(min_y), None) => open = Some((x, x, min_y, column_max_y)),
                (None, _) => {
                    if let Some(finished) = open.take() {
                        clusters.push(finished);
                    }
                }
            }
        }
        clusters.extend(open);
        clusters
    }

    /// Ink row bands `(min_y, max_y)`, split on fully empty rows, top to bottom.
    /// One entry per text line as long as neighbouring lines do not overlap.
    fn ink_row_bands(image: &RenderedTextImage) -> Vec<(usize, usize)> {
        let width = image.width as usize;
        let height = image.height as usize;
        let mut bands: Vec<(usize, usize)> = Vec::new();
        let mut open: Option<(usize, usize)> = None;
        for y in 0..height {
            let inked = (0..width).any(|x| image.rgba[(y * width + x) * 4 + 3] >= INK_ALPHA);
            match (inked, open.as_mut()) {
                (true, Some(current)) => current.1 = y,
                (true, None) => open = Some((y, y)),
                (false, _) => {
                    if let Some(finished) = open.take() {
                        bands.push(finished);
                    }
                }
            }
        }
        bands.extend(open);
        bands
    }

    /// Faux params for the render tests: strong enough that the geometric
    /// growth is unambiguous against AA noise.
    fn test_faux_bold(thicken_percent: f32, expand_percent: f32) -> FauxBoldParams {
        FauxBoldParams {
            thicken_percent,
            expand_percent,
            sharp_corners: true,
            outward_only: true,
        }
    }

    #[test]
    fn faux_zero_strength_and_ungated_params_are_byte_identical_to_plain() {
        let base = base_params();
        let plain = render_text_to_image(&base, None).expect("plain render");

        // Faux params WITHOUT the force flag are ignored entirely.
        let mut ungated = base.clone();
        ungated.faux_bold = Some(test_faux_bold(10.0, 5.0));
        ungated.faux_italic_slant_deg = Some(20.0);
        let ungated_render = render_text_to_image(&ungated, None).expect("ungated render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (ungated_render.width, ungated_render.height, &ungated_render.rgba),
            "faux params without force flags must be byte-identical to plain"
        );

        // Zero-strength faux bold keeps the Regular face and offsets nothing.
        let mut zero_bold = base.clone();
        zero_bold.force_bold = true;
        zero_bold.faux_bold = Some(test_faux_bold(0.0, 0.0));
        let zero_bold_render = render_text_to_image(&zero_bold, None).expect("zero-bold render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (zero_bold_render.width, zero_bold_render.height, &zero_bold_render.rgba),
            "zero-strength faux bold must be byte-identical to plain"
        );

        // Zero slant shears nothing.
        let mut zero_italic = base.clone();
        zero_italic.force_italic = true;
        zero_italic.faux_italic_slant_deg = Some(0.0);
        let zero_italic_render =
            render_text_to_image(&zero_italic, None).expect("zero-italic render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (zero_italic_render.width, zero_italic_render.height, &zero_italic_render.rgba),
            "zero faux slant must be byte-identical to plain"
        );
    }

    #[test]
    fn faux_bold_attrs_keep_regular_face() {
        // Faux path must NOT request the real Bold/Italic faces from font
        // matching; the legacy force flags without faux still do.
        let mut params = base_params();
        params.force_bold = true;
        params.force_italic = true;
        assert_eq!(base_attrs_real_bold_italic(&params), (true, true));
        params.faux_bold = Some(test_faux_bold(3.0, 0.0));
        assert_eq!(base_attrs_real_bold_italic(&params), (false, true));
        params.faux_italic_slant_deg = Some(14.0);
        assert_eq!(base_attrs_real_bold_italic(&params), (false, false));
        params.force_bold = false;
        params.force_italic = false;
        assert_eq!(base_attrs_real_bold_italic(&params), (false, false));
    }

    #[test]
    fn faux_bold_grows_ink_extent_by_offset_and_advance() {
        // Two glyphs: the total ink width grows by the pen-step growth
        // (2*d + expand) plus one `d` of outline growth on each outer edge,
        // i.e. 4*d + expand; the ink height grows by 2*d.
        let mut plain = base_params();
        plain.text = "HH".to_string();
        plain.font_size_px = 48.0;
        let plain_render = render_text_to_image(&plain, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let thicken = 5.0f32;
        let expand = 10.0f32;
        let d = thicken / 100.0 * plain.font_size_px;
        let expand_px = expand / 100.0 * plain.font_size_px;
        let mut faux = plain.clone();
        faux.force_bold = true;
        faux.faux_bold = Some(test_faux_bold(thicken, expand));
        let faux_render = render_text_to_image(&faux, None).expect("faux render");
        let (faux_w, faux_h) =
            alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
                .expect("faux ink");

        let expected_w_growth = 4.0 * d + expand_px;
        let w_growth = faux_w as f32 - plain_w as f32;
        assert!(
            (w_growth - expected_w_growth).abs() <= 2.0,
            "ink width growth {w_growth} should be ~{expected_w_growth} (d={d}, expand={expand_px})"
        );
        let h_growth = faux_h as f32 - plain_h as f32;
        assert!(
            (h_growth - 2.0 * d).abs() <= 2.0,
            "ink height growth {h_growth} should be ~{}",
            2.0 * d
        );
    }

    #[test]
    fn faux_bold_compensates_counter_bearing_glyphs_in_ink_and_advance() {
        // `outward_only` keeps the counter, so the outer contour of a
        // counter-bearing glyph moves by 2*d to give its strokes the same
        // weight the two moving edges of `H` produce. Both the ink and the pen
        // must follow: per glyph the ink grows by 2*(2*d) on each axis and the
        // pen step by 2*(2*d), so two glyphs grow by 8*d in width and 4*d in
        // height. An UNCOMPENSATED advance would leave only 6*d of width growth
        // and overlap the two letters by 2*d.
        let mut plain = base_params();
        plain.text = "OO".to_string();
        plain.font_size_px = 48.0;
        let plain_render = render_text_to_image(&plain, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let thicken = 5.0f32;
        let d = thicken / 100.0 * plain.font_size_px;
        let mut faux = plain.clone();
        faux.force_bold = true;
        faux.faux_bold = Some(test_faux_bold(thicken, 0.0));
        let faux_render = render_text_to_image(&faux, None).expect("faux render");
        let (faux_w, faux_h) =
            alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
                .expect("faux ink");

        let w_growth = faux_w as f32 - plain_w as f32;
        assert!(
            (w_growth - 8.0 * d).abs() <= 3.0,
            "counter-bearing ink width growth {w_growth} should be ~{} (d={d})",
            8.0 * d
        );
        let h_growth = faux_h as f32 - plain_h as f32;
        assert!(
            (h_growth - 4.0 * d).abs() <= 3.0,
            "counter-bearing ink height growth {h_growth} should be ~{}",
            4.0 * d
        );
    }

    #[test]
    fn faux_thinning_shrinks_ink_extent_and_pulls_the_advance_in() {
        // Mirror of `faux_bold_grows_ink_extent_by_offset_and_advance`: a
        // NEGATIVE thicken pulls each outer edge into the ink by |d| and each
        // pen step in by 2*|d|, so two glyphs lose ~4*|d| of total width. A
        // shrink of only ~2*|d| would mean the advance ignored the sign.
        let mut plain = base_params();
        plain.text = "HH".to_string();
        plain.font_size_px = 64.0;
        let plain_render = render_text_to_image(&plain, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let d = 3.0 / 100.0 * plain.font_size_px;
        let mut thinned = plain.clone();
        thinned.force_bold = true;
        thinned.faux_bold = Some(test_faux_bold(-3.0, 0.0));
        let thinned_render = render_text_to_image(&thinned, None).expect("thinned render");
        let (thin_w, thin_h) = alpha_bounds_from_rgba(
            thinned_render.width,
            thinned_render.height,
            &thinned_render.rgba,
        )
        .expect("thinned ink");

        let w_shrink = plain_w as f32 - thin_w as f32;
        assert!(
            (w_shrink - 4.0 * d).abs() <= 2.5,
            "ink width shrink {w_shrink} should be ~{} (d=-{d})",
            4.0 * d
        );
        let h_shrink = plain_h as f32 - thin_h as f32;
        assert!(
            (h_shrink - 2.0 * d).abs() <= 2.5,
            "ink height shrink {h_shrink} should be ~{}",
            2.0 * d
        );
    }

    /// Shape `text` with the fixture font and run the REAL horizontal metric
    /// accumulation (`horizontal_run_layout`) over its first layout run.
    ///
    /// Returns the accumulated pen positions and the logical (alignment) width.
    /// The `FontSystem` holds ONLY the fixture face — a test binary cannot
    /// resolve the shipped `fonts/ui` bundle (see the module Testing Guidance),
    /// and this contract is about arithmetic, not about the bundle.
    fn fixture_run_layout(text: &str, params: &TextRenderParams) -> (Vec<f32>, f32) {
        fixture_run_layout_with_custom_kerning(text, params, &[])
    }

    /// [`fixture_run_layout`] with user-authored kerning overrides bound to the
    /// fixture face, as `(left, right, offset_per_mille)` triples.
    ///
    /// The face id comes straight out of the database this test built, which is
    /// exactly what `CustomKerningMap::record` derives from the load cache in
    /// production — so the pen loop sees an override the same way either way.
    fn fixture_run_layout_with_custom_kerning(
        text: &str,
        params: &TextRenderParams,
        pairs: &[(char, char, f32)],
    ) -> (Vec<f32>, f32) {
        use crate::font_provider::CustomKerningTable;
        use crate::font_registry::CustomKerningMap;
        use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

        let bytes = std::fs::read(test_font_path()).expect("fixture font bytes");
        let mut db = fontdb::Database::new();
        db.load_font_data(bytes);
        let face = db.faces().next().expect("fixture face");
        let face_id = face.id;
        let family = face
            .families
            .first()
            .cloned()
            .map(|(name, _language)| name)
            .expect("fixture family name");
        let mut custom_kerning = CustomKerningMap::default();
        if !pairs.is_empty() {
            custom_kerning.insert(
                face_id,
                Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                "fixture",
            );
        }
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        let em = params.font_size_px;
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em * 1.2));
        buffer.set_size(&mut font_system, None, None);
        let attrs = Attrs::new()
            .family(Family::Name(family.as_str()))
            .metrics(Metrics::new(em, em));
        buffer.set_text(&mut font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);

        let mut swash_cache = cosmic_text::SwashCache::new();
        let mut outline_cache = crate::vector::OutlineCache::new();
        let mut contour_cache = crate::optical::OpticalContourCache::new();
        let run = buffer.layout_runs().next().expect("one layout run");
        let layout = super::horizontal_run_layout(
            params,
            &run,
            &mut font_system,
            &mut swash_cache,
            &mut outline_cache,
            &mut contour_cache,
            &[0],
            None,
            &custom_kerning,
            em,
        );
        (layout.glyph_xs, layout.line_width_px)
    }

    /// Params for a thinned run of `text` at the minimum (most negative)
    /// strength — the case where the faux delta can exceed a glyph's advance.
    fn thinned_run_params(text: &str) -> TextRenderParams {
        let mut params = base_params();
        params.text = text.to_string();
        params.font_size_px = 48.0;
        params.force_bold = true;
        params.faux_bold = Some(test_faux_bold(crate::types::FAUX_THICKEN_PERCENT_MIN, 0.0));
        params
    }

    #[test]
    fn a_fully_consumed_glyph_does_not_vote_on_the_extra_info_centers() {
        // At the minimum strength and em 48 the contraction (2*|d| = 4.8 px)
        // exceeds a Liberation Sans stem, so `l` loses every contour and draws
        // NOTHING while `o` survives. The reported ink centers must therefore
        // describe the `o` alone. Before the exclusion the phantom `l` box
        // dragged them to mean [5.25, 8.21] / median [1.25, 8.75] — the median
        // sitting 1.25 px from the left edge of an image whose only ink is the
        // `o` spanning its full width.
        let mut solo = thinned_run_params("o");
        solo.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let solo_render = render_text_to_image(&solo, None).expect("solo render");
        let solo_mean = solo_render.extra.mean_center.expect("solo mean center");
        let solo_median = solo_render.extra.median_center.expect("solo median center");

        let mut pair = solo.clone();
        pair.text = "lo".to_string();
        let pair_render = render_text_to_image(&pair, None).expect("pair render");
        let mean = pair_render.extra.mean_center.expect("pair mean center");
        let median = pair_render.extra.median_center.expect("pair median center");
        for (label, got, want) in [("mean", mean, solo_mean), ("median", median, solo_median)] {
            assert!(
                (got[0] - want[0]).abs() <= 1.5 && (got[1] - want[1]).abs() <= 1.5,
                "{label} center {got:?} of \"lo\" must match the surviving `o` alone {want:?}"
            );
            assert!(
                got[0] >= 0.0
                    && got[1] >= 0.0
                    && got[0] <= pair_render.width as f32
                    && got[1] <= pair_render.height as f32,
                "{label} center {got:?} fell outside the {}x{} image",
                pair_render.width,
                pair_render.height
            );
        }

        // A run in which EVERY glyph is consumed reports no center at all
        // rather than the centroid of boxes that drew nothing.
        let mut all_gone = solo.clone();
        all_gone.text = "Il".to_string();
        let gone_render = render_text_to_image(&all_gone, None).expect("consumed render");
        assert!(
            gone_render.rgba.chunks_exact(4).all(|pixel| pixel[3] == 0),
            "the run should have drawn no ink at all"
        );
        assert_eq!(gone_render.extra.mean_center, None);
        assert_eq!(gone_render.extra.median_center, None);
    }

    #[test]
    fn faux_floored_advance_never_reverses_the_pen() {
        use super::faux_floored_advance;

        // No faux delta: the shaped advance is returned bit-for-bit, in both
        // directions, so a run without faux bold cannot move by a single ulp.
        for base in [0.0f32, 13.336, -26.695, f32::MIN_POSITIVE] {
            assert_eq!(faux_floored_advance(base, 0.0).to_bits(), base.to_bits());
        }
        // A contraction smaller than the advance simply narrows the step.
        assert!((faux_floored_advance(13.336, -4.8) - 8.536).abs() < 1e-4);
        // A contraction at least as large as the advance stops the pen instead
        // of walking it backwards — the zero-advance combining-mark case.
        assert_eq!(faux_floored_advance(0.0, -4.8), 0.0);
        assert_eq!(faux_floored_advance(2.0, -4.8), 0.0);
        // RTL: cosmic-text walks the pen leftwards, so the floor mirrors and a
        // THICKENING delta may not flip the step to the right either.
        assert!((faux_floored_advance(-26.695, -4.8) + 31.495).abs() < 1e-4);
        assert_eq!(faux_floored_advance(-2.0, 4.8), 0.0);
    }

    #[test]
    fn faux_thinning_never_steps_the_pen_backwards_over_a_combining_mark() {
        // A base letter followed by a zero-advance combining mark: at the
        // minimum strength the mark's own faux delta (-2 * 0.05 * em = -4.81 px
        // at em 48) exceeds its zero advance. Before the floor the run measured
        // xs = [0.0, 17.070313, 12.257813] — the `b` placed 4.81 px LEFT of the
        // mark that precedes it — and reported line_width_px = 29.328125.
        let text = "a\u{0301}\u{0301}b";
        let params = thinned_run_params(text);
        let (xs, line_width_px) = fixture_run_layout(text, &params);
        assert_eq!(xs.len(), 3, "expected [base, mark, b], got {xs:?}");
        for pair in xs.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "pen stepped backwards: {xs:?} (line width {line_width_px})"
            );
        }
        // The mark contributes EXACTLY nothing: its floored effective advance is
        // a hard zero, so the following glyph sits on the mark's own pen.
        assert_eq!(
            xs[2].to_bits(),
            xs[1].to_bits(),
            "a fully contracted zero-advance mark must leave the pen where it was: {xs:?}"
        );
        // Alignment width follows the SAME floored rule, so it still reaches the
        // last glyph's right edge instead of the pre-floor 29.328125.
        assert!(
            line_width_px >= xs[2],
            "alignment width {line_width_px} is behind the pen {xs:?}"
        );
        let (_solo_xs, solo_width) = fixture_run_layout("b", &thinned_run_params("b"));
        assert!(
            (line_width_px - (xs[2] + solo_width)).abs() < 1e-3,
            "alignment width {line_width_px} must equal the last pen {} plus that \
             glyph's own floored advance {solo_width}",
            xs[2]
        );
    }

    #[test]
    fn faux_thinning_keeps_narrow_punctuation_monotone() {
        // The marginal case: a period/comma advance (0.278 em = 13.34 px at
        // em 48) is wider than the -4.81 px contraction, so the pen must keep
        // moving FORWARD, just by less.
        let text = ".,.,.";
        let params = thinned_run_params(text);
        let (xs, line_width_px) = fixture_run_layout(text, &params);
        assert_eq!(xs.len(), 5, "expected five glyphs, got {xs:?}");
        for pair in xs.windows(2) {
            assert!(
                pair[1] > pair[0],
                "narrow-glyph pen must stay strictly increasing: {xs:?}"
            );
        }
        let mut plain = base_params();
        plain.text = text.to_string();
        plain.font_size_px = 48.0;
        let (plain_xs, _plain_width) = fixture_run_layout(text, &plain);
        assert!(
            xs[4] < plain_xs[4],
            "thinning must still pull the run in: thinned {xs:?} vs plain {plain_xs:?}"
        );
        assert!(
            line_width_px >= xs[4],
            "alignment width {line_width_px} is behind the pen {xs:?}"
        );
    }

    #[test]
    fn faux_bounds_pads_are_never_negative() {
        // A THINNING offset pads by the same MAGNITUDE a thickening one does: an
        // inward offset strays outside the source ink at a reflex vertex (the
        // miter apex is pushed up to 4*|d| along the inward bisector, through
        // any thinner material), and a zero pad would crop that spike. What must
        // never happen is a NEGATIVE pad, which would shrink the canvas below the
        // plain ink.
        let em = 48.0f32;
        for sharp_corners in [true, false] {
            for outward_only in [true, false] {
                for has_counter in [true, false] {
                    let thinning = FauxGlyphStyle {
                        bold: FauxOutlineParams::new(-0.05 * em, sharp_corners, outward_only),
                        bold_has_counter: has_counter,
                        shear_x: 0.0,
                    };
                    assert!(thinning.bold.is_some(), "the thinning params must be valid");
                    let pads = faux_bounds_pads(thinning, 30.0, 40.0, 1.0, 1.0);
                    let thin_quantized = thinning.bold.map_or(0.0, FauxOutlineParams::d_px).abs();
                    let thin_expected = thin_quantized
                        * if outward_only && has_counter { 2.0 } else { 1.0 }
                        * if sharp_corners { 4.0 } else { 1.0 };
                    assert!(
                        pads[0] >= 0.0 && pads[1] >= 0.0,
                        "a pad must never be negative (sharp={sharp_corners}, \
                         out={outward_only}, counter={has_counter})"
                    );
                    assert!(
                        (pads[1] - thin_expected).abs() < 1e-3,
                        "thinning pad {} should be the magnitude {thin_expected} \
                         (sharp={sharp_corners}, out={outward_only}, counter={has_counter})",
                        pads[1]
                    );
                    assert!(thinning.outer_delta_px() < 0.0, "thinning delta stays negative");

                    // The thickening counterpart DOES pad, and the compensated
                    // (counter-bearing, outward_only) case pads twice as much.
                    let thickening = FauxGlyphStyle {
                        bold: FauxOutlineParams::new(0.05 * em, sharp_corners, outward_only),
                        bold_has_counter: has_counter,
                        shear_x: 0.0,
                    };
                    let pads = faux_bounds_pads(thickening, 30.0, 40.0, 1.0, 1.0);
                    assert!(pads[0] > 0.0 && pads[1] > 0.0, "thickening must pad");
                    let compensated = outward_only && has_counter;
                    // Compare against the QUANTIZED distance (1/64 px grid),
                    // not the requested one, so the check pins the formula
                    // rather than the rounding.
                    let quantized = thickening.bold.map_or(0.0, FauxOutlineParams::d_px);
                    let expected = quantized
                        * if compensated { 2.0 } else { 1.0 }
                        * if sharp_corners { 4.0 } else { 1.0 };
                    assert!(
                        (pads[1] - expected).abs() < 1e-3,
                        "pad {} should be {expected}",
                        pads[1]
                    );
                }
            }
        }
    }

    /// Alpha-weighted x centroid over the row band `[y0, y1)`.
    fn alpha_centroid_x_in_rows(
        width: u32,
        height: u32,
        rgba: &[u8],
        y0: usize,
        y1: usize,
    ) -> Option<f32> {
        let width = width as usize;
        let mut weighted_x = 0.0f32;
        let mut alpha_sum = 0.0f32;
        for y in y0..y1.min(height as usize) {
            for x in 0..width {
                let alpha = f32::from(rgba[(y * width + x) * 4 + 3]);
                weighted_x += x as f32 * alpha;
                alpha_sum += alpha;
            }
        }
        (alpha_sum > 0.0).then_some(weighted_x / alpha_sum)
    }

    #[test]
    fn faux_italic_leans_tops_in_slant_direction() {
        // Compare the x centroid of the ink's top half against its bottom
        // half: a positive slant leans tops right, a negative one left.
        let mut base = base_params();
        base.text = "H".to_string();
        base.font_size_px = 64.0;
        base.force_italic = true;

        let lean_of = |slant: f32| -> f32 {
            let mut params = base.clone();
            params.faux_italic_slant_deg = Some(slant);
            let render = render_text_to_image(&params, None).expect("italic render");
            let h = render.height as usize;
            let top = alpha_centroid_x_in_rows(render.width, render.height, &render.rgba, 0, h / 2)
                .expect("top ink");
            let bottom =
                alpha_centroid_x_in_rows(render.width, render.height, &render.rgba, h / 2, h)
                    .expect("bottom ink");
            top - bottom
        };

        let right = lean_of(30.0);
        let left = lean_of(-30.0);
        assert!(right > 3.0, "positive slant must lean tops right, got {right}");
        assert!(left < -3.0, "negative slant must lean tops left, got {left}");
    }

    #[test]
    fn optical_kerning_measures_faux_offset_ink() {
        // Under Optical the pen positions come from ink-gap normalization of
        // the OFFSET outlines (no `2*d` pen growth); under Auto the faux pen
        // growth applies. So the optical faux render must be measurably
        // narrower than the Auto faux render of the same text, proving the
        // optical path measures the emboldened contours instead of inheriting
        // the metric growth.
        let mut auto_faux = base_params();
        auto_faux.text = "HHH".to_string();
        auto_faux.font_size_px = 48.0;
        auto_faux.force_bold = true;
        auto_faux.faux_bold = Some(test_faux_bold(6.0, 0.0));
        auto_faux.kerning_mode = KerningMode::Auto;
        let auto_render = render_text_to_image(&auto_faux, None).expect("auto faux render");
        let (auto_w, _) =
            alpha_bounds_from_rgba(auto_render.width, auto_render.height, &auto_render.rgba)
                .expect("auto ink");

        let mut optical_faux = auto_faux.clone();
        optical_faux.kerning_mode = KerningMode::Optical;
        let optical_render =
            render_text_to_image(&optical_faux, None).expect("optical faux render");
        let (optical_w, _) = alpha_bounds_from_rgba(
            optical_render.width,
            optical_render.height,
            &optical_render.rgba,
        )
        .expect("optical ink");

        let d = 6.0 / 100.0 * 48.0;
        assert!(
            (auto_w as f32) - (optical_w as f32) >= d,
            "optical faux ({optical_w}) must be narrower than auto faux ({auto_w}) \
             because optical normalizes the measured (offset) ink gaps"
        );
    }

    #[test]
    fn faux_bold_grows_vertical_stacking() {
        let mut plain = base_params();
        plain.text = "HH".to_string();
        plain.font_size_px = 48.0;
        plain.text_line_mode = TextLineMode::Vertical;
        let plain_render = render_text_to_image(&plain, None).expect("plain vertical render");
        let (_, plain_h) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let mut faux = plain.clone();
        faux.force_bold = true;
        faux.faux_bold = Some(test_faux_bold(6.0, 0.0));
        let faux_render = render_text_to_image(&faux, None).expect("faux vertical render");
        let (_, faux_h) =
            alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
                .expect("faux ink");

        // Two glyphs, each thickened by d top+bottom, and the ink-height
        // stacking pads the step accordingly: expect ~4*d total growth.
        let d = 6.0 / 100.0 * 48.0;
        let growth = faux_h as f32 - plain_h as f32;
        assert!(
            (growth - 4.0 * d).abs() <= 3.0,
            "vertical ink height growth {growth} should be ~{}",
            4.0 * d
        );
    }

    #[test]
    fn inline_faux_bold_tag_differs_from_real_bold_tag() {
        // `<b=...>` must produce different pixels than the legacy `<b>` (which
        // resolves the family's Bold face — here the same Regular fixture, so
        // it renders like plain) on the same text.
        let mut real = base_params();
        real.text = "a<b>bb</b>a".to_string();
        real.enable_inline_style_tags = true;
        let real_render = render_text_to_image(&real, None).expect("real-bold render");

        let mut faux = real.clone();
        faux.text = "a<b=8>bb</b>a".to_string();
        let faux_render = render_text_to_image(&faux, None).expect("faux-bold render");
        assert_ne!(
            (real_render.width, real_render.height, &real_render.rgba),
            (faux_render.width, faux_render.height, &faux_render.rgba),
            "a parameterized faux bold span must change the rendered pixels"
        );
    }

    /// Maximum inked x over the row band `[y0, y1)`, or `None` if the band is
    /// fully transparent.
    fn max_ink_x_in_rows(
        width: u32,
        height: u32,
        rgba: &[u8],
        y0: usize,
        y1: usize,
    ) -> Option<usize> {
        let width = width as usize;
        let mut max_x = None;
        for y in y0..y1.min(height as usize) {
            for x in 0..width {
                if rgba[(y * width + x) * 4 + 3] > 0 {
                    max_x = Some(max_x.map_or(x, |current: usize| current.max(x)));
                }
            }
        }
        max_x
    }

    #[test]
    fn faux_advance_carries_across_inline_size_boundary() {
        // A faux span ending immediately before an inline size change: the
        // following (differently-sized, non-faux) glyph must be pushed by the
        // full 2*d instead of staying at its shaped position over the
        // thickened ink. Expected total ink width growth vs the same text
        // without faux: d (left edge of the thickened glyph) + 2*d (pen shift
        // of everything after the span) = 3*d. A dropped boundary extra would
        // leave only ~d of growth (and a d-deep overlap).
        let mut plain = base_params();
        plain.text = "H<size=24>H".to_string();
        plain.font_size_px = 48.0;
        plain.enable_inline_style_tags = true;
        let plain_render = render_text_to_image(&plain, None).expect("plain render");
        let (plain_w, _) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let mut faux = plain.clone();
        faux.text = "<b=10>H</b><size=24>H".to_string();
        let faux_render = render_text_to_image(&faux, None).expect("faux render");
        let (faux_w, _) =
            alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
                .expect("faux ink");

        let d = 10.0 / 100.0 * 48.0;
        let growth = faux_w as f32 - plain_w as f32;
        assert!(
            (growth - 3.0 * d).abs() <= 2.0,
            "ink width growth {growth} should be ~{} (no overlap at the size boundary)",
            3.0 * d
        );
    }

    #[test]
    fn faux_trailing_extra_included_in_alignment_width() {
        // Right-aligned two-line text: the line ending in a faux-bold glyph
        // must not overshoot the flush margin relative to the plain line
        // below it — the trailing glyph's faux advance growth is part of the
        // logical line width.
        let mut params = base_params();
        params.text = "<b=12>HH</b>\nHH".to_string();
        params.enable_inline_style_tags = true;
        params.font_size_px = 48.0;
        params.align = HorizontalAlign::RIGHT;
        let render = render_text_to_image(&params, None).expect("aligned render");
        let h = render.height as usize;
        let top_right = max_ink_x_in_rows(render.width, render.height, &render.rgba, 0, h / 2)
            .expect("top line ink");
        let bottom_right = max_ink_x_in_rows(render.width, render.height, &render.rgba, h / 2, h)
            .expect("bottom line ink");
        assert!(
            top_right <= bottom_right + 1,
            "faux line right edge ({top_right}) must not overshoot the plain flush margin \
             ({bottom_right})"
        );
    }

    #[test]
    fn vertical_columns_do_not_overlap_under_faux_italic_slant() {
        // Two single-glyph columns at a +/-45 degree faux slant: the column
        // width must include the shear overhang, so a fully transparent pixel
        // column still separates the two glyphs' ink.
        for slant in [45.0f32, -45.0] {
            let mut params = base_params();
            params.text = "H\nH".to_string();
            params.font_size_px = 48.0;
            params.text_line_mode = TextLineMode::Vertical;
            // No extra column spacing: adjacent columns are separated only by
            // the measured visual width, so a missing shear overhang overlaps.
            params.line_spacing_percent = 0.0;
            params.force_italic = true;
            params.faux_italic_slant_deg = Some(slant);
            let render = render_text_to_image(&params, None).expect("vertical italic render");
            let (min_x, _, max_x, _) =
                alpha_box(render.width, render.height, &render.rgba).expect("ink");
            let width = render.width as usize;
            let height = render.height as usize;
            let has_gap = ((min_x + 1)..max_x).any(|x| {
                (0..height).all(|y| render.rgba[(y * width + x) * 4 + 3] == 0)
            });
            assert!(
                has_gap,
                "columns must stay separated at slant {slant} (no shear overlap)"
            );
        }
    }

    #[test]
    fn rotated_faux_bold_italic_stays_within_canvas() {
        // Global rotation + faux bold + faux italic combined must route
        // through the rotated placement path without clipping: the trim pass
        // pads 1px, so ink touching the canvas edge would indicate clipped
        // bounds.
        let mut plain = base_params();
        plain.text = "HH".to_string();
        plain.font_size_px = 48.0;
        plain.global_rotation_deg = 30.0;
        let plain_render = render_text_to_image(&plain, None).expect("plain rotated render");

        let mut faux = plain.clone();
        faux.force_bold = true;
        faux.faux_bold = Some(test_faux_bold(8.0, 0.0));
        faux.force_italic = true;
        faux.faux_italic_slant_deg = Some(25.0);
        let render = render_text_to_image(&faux, None).expect("rotated faux render");
        let (min_x, min_y, max_x, max_y) =
            alpha_box(render.width, render.height, &render.rgba).expect("ink");
        assert!(min_x >= 1 && min_y >= 1, "ink must not touch the left/top edge");
        assert!(
            (max_x as u32) < render.width - 1 && (max_y as u32) < render.height - 1,
            "ink must not touch the right/bottom edge (would indicate clipping)"
        );
        assert_ne!(
            (plain_render.width, plain_render.height, &plain_render.rgba),
            (render.width, render.height, &render.rgba),
            "rotated faux render must differ from the plain rotated render"
        );
    }

    #[test]
    fn formula_mode_applies_faux_bold_across_effective_sizes() {
        // Formula/on-path mode with two effective sizes of the same glyph:
        // each glyph resolves its own quantized d, so the ink cache holds a
        // separate (CacheKey, faux_bits) entry per size and both glyphs
        // thicken (the faux render grows in both axes).
        let mut plain = base_params();
        plain.text = "H<size=24>H".to_string();
        plain.enable_inline_style_tags = true;
        plain.font_size_px = 48.0;
        plain.text_layout_mode = TextLayoutMode::Formula;
        let plain_render = render_text_to_image(&plain, None).expect("plain formula render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
                .expect("plain ink");

        let mut faux = plain.clone();
        faux.force_bold = true;
        faux.faux_bold = Some(test_faux_bold(8.0, 0.0));
        let faux_render = render_text_to_image(&faux, None).expect("faux formula render");
        let (faux_w, faux_h) =
            alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
                .expect("faux ink");
        assert!(
            faux_w > plain_w && faux_h > plain_h,
            "faux bold must thicken the on-path ink ({plain_w}x{plain_h} -> {faux_w}x{faux_h})"
        );
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

    /// Build a `cols x rows` identity mesh (every node at its identity position).
    fn identity_mesh(cols: usize, rows: usize) -> VectorMeshWarp {
        let mut points_norm = Vec::with_capacity(cols * rows);
        for i in 0..rows {
            for j in 0..cols {
                points_norm.push([j as f32 / (cols - 1) as f32, i as f32 / (rows - 1) as f32]);
            }
        }
        VectorMeshWarp {
            cols,
            rows,
            // 0 => exercise the LIVE pre-warp-bounds normalization (Phase 1 path); the explicit
            // src-dims (Design B) path is unit-tested in `vector.rs`.
            src_width_px: 0.0,
            src_height_px: 0.0,
            points_norm,
        }
    }

    /// Full inked-alpha bounding box `(min_x, min_y, max_x, max_y)` in pixels.
    fn alpha_box(width: u32, height: u32, rgba: &[u8]) -> Option<(usize, usize, usize, usize)> {
        let (width, height) = (width as usize, height as usize);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (width, height, 0usize, 0usize);
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
        found.then_some((min_x, min_y, max_x, max_y))
    }

    /// Count of inked (alpha > 0) pixels.
    fn inked_pixels(rgba: &[u8]) -> usize {
        rgba.chunks_exact(4).filter(|px| px[3] > 0).count()
    }

    #[test]
    fn mesh_warp_none_and_identity_are_byte_identical() {
        // Contract: `None` and an identity mesh both take the byte-identical fast
        // path, so the output RGBA equals a plain render exactly.
        let base = base_params();
        let plain = render_text_to_image(&base, None).expect("plain render");

        let mut identity = base.clone();
        identity.raster_transform = Some(identity_mesh(13, 13));
        let warped = render_text_to_image(&identity, None).expect("identity-warp render");

        assert_eq!(
            (plain.width, plain.height),
            (warped.width, warped.height),
            "identity warp must not change canvas size"
        );
        assert_eq!(
            plain.rgba, warped.rgba,
            "identity warp must be byte-identical to no warp"
        );
    }

    #[test]
    fn mesh_warp_single_corner_grows_canvas_and_shifts_coverage() {
        // Push the bottom-right lattice node outward (beyond the box). The warped
        // content must extend farther right/down than the un-warped render, and the
        // canvas must grow to hold it (no clipping).
        let base = base_params();
        let plain = render_text_to_image(&base, None).expect("plain render");
        let (_, _, plain_max_x, plain_max_y) =
            alpha_box(plain.width, plain.height, &plain.rgba).expect("plain has ink");

        let mut mesh = identity_mesh(13, 13);
        let last = mesh.points_norm.len() - 1; // bottom-right node (i=12, j=12).
        mesh.points_norm[last] = [1.35, 1.35];
        let mut params = base.clone();
        params.raster_transform = Some(mesh);
        let warped = render_text_to_image(&params, None).expect("corner-warp render");
        let (_, _, warped_max_x, warped_max_y) =
            alpha_box(warped.width, warped.height, &warped.rgba).expect("warped has ink");

        // Canvas grew in at least one dimension to accommodate the outward warp.
        assert!(
            warped.width > plain.width || warped.height > plain.height,
            "canvas must grow: plain {}x{}, warped {}x{}",
            plain.width,
            plain.height,
            warped.width,
            warped.height
        );
        // Coverage shifted outward (down-right): warped ink reaches farther.
        assert!(
            warped_max_x >= plain_max_x && warped_max_y > plain_max_y,
            "warped ink must reach farther down-right: plain ({plain_max_x},{plain_max_y}) \
             warped ({warped_max_x},{warped_max_y})"
        );
        // No clipping: the trimmed content keeps a transparent border on all sides
        // (trim pads by 1px), so the farthest ink is strictly inside the canvas.
        assert!(
            (warped_max_x as u32) < warped.width - 1 && (warped_max_y as u32) < warped.height - 1,
            "warped ink must not touch the canvas edge (would indicate clipping)"
        );
    }

    #[test]
    fn mesh_warp_pure_translation_shifts_without_shape_change() {
        // A translation-equivalent mesh (all nodes shifted by a constant) moves the
        // content without deforming it: the trimmed content size and inked pixel
        // count match the un-warped render, but the pre-trim placement shifts.
        let base = base_params();
        let plain = render_text_to_image(&base, None).expect("plain render");
        let (px0, py0, px1, py1) =
            alpha_box(plain.width, plain.height, &plain.rgba).expect("plain ink");
        let plain_w = px1 - px0;
        let plain_h = py1 - py0;

        // Shift every node right+down by a constant normalized offset.
        let mut mesh = identity_mesh(13, 13);
        for node in &mut mesh.points_norm {
            node[0] += 0.25;
            node[1] += 0.15;
        }
        let mut params = base.clone();
        params.raster_transform = Some(mesh);
        let warped = render_text_to_image(&params, None).expect("translation-warp render");
        let (wx0, wy0, wx1, wy1) =
            alpha_box(warped.width, warped.height, &warped.rgba).expect("warped ink");
        let warped_w = wx1 - wx0;
        let warped_h = wy1 - wy0;

        // Shape (trimmed extent) is preserved within a small AA/rounding tolerance.
        assert!(
            (warped_w as i32 - plain_w as i32).abs() <= 2,
            "translated content width must be unchanged: plain {plain_w}, warped {warped_w}"
        );
        assert!(
            (warped_h as i32 - plain_h as i32).abs() <= 2,
            "translated content height must be unchanged: plain {plain_h}, warped {warped_h}"
        );
        // Ink mass is preserved (a shift that clipped would drop inked pixels).
        let (plain_ink, warped_ink) = (inked_pixels(&plain.rgba), inked_pixels(&warped.rgba));
        let tolerance = plain_ink / 20 + 4; // 5% + slack for AA at the new subpixel phase.
        assert!(
            warped_ink.abs_diff(plain_ink) <= tolerance,
            "translation must preserve ink mass (no clipping): plain {plain_ink}, warped {warped_ink}"
        );
    }

    /// Push the bottom-right lattice node of a fresh 13x13 identity mesh outward by
    /// `amount` (both axes) so the warp bulges the content down-right beyond the box.
    fn corner_pushed_mesh(amount: f32) -> VectorMeshWarp {
        let mut mesh = identity_mesh(13, 13);
        let last = mesh.points_norm.len() - 1;
        mesh.points_norm[last] = [1.0 + amount, 1.0 + amount];
        mesh
    }

    #[test]
    fn mesh_warp_applies_on_vertical_path() {
        // Regression: the warp used to be ignored on the vertical path. Identity must
        // stay byte-identical; a non-identity corner push must change the pixels and
        // grow the trimmed canvas outward (down-right) without clipping.
        let mut base = base_params();
        base.text = "ТЕКСТ".to_string();
        base.text_line_mode = TextLineMode::Vertical;
        base.width_px = 120;
        let plain = render_text_to_image(&base, None).expect("vertical plain render");
        let (_, _, plain_max_x, plain_max_y) =
            alpha_box(plain.width, plain.height, &plain.rgba).expect("vertical plain ink");

        let mut identity = base.clone();
        identity.raster_transform = Some(identity_mesh(13, 13));
        let ident = render_text_to_image(&identity, None).expect("vertical identity render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (ident.width, ident.height, &ident.rgba),
            "vertical identity warp must be byte-identical to no warp"
        );

        let mut warped_params = base.clone();
        warped_params.raster_transform = Some(corner_pushed_mesh(0.35));
        let warped = render_text_to_image(&warped_params, None).expect("vertical warp render");
        assert_ne!(
            (plain.width, plain.height, &plain.rgba),
            (warped.width, warped.height, &warped.rgba),
            "vertical non-identity warp must change the render"
        );
        let (_, _, warped_max_x, warped_max_y) =
            alpha_box(warped.width, warped.height, &warped.rgba).expect("vertical warp ink");
        assert!(
            warped.width > plain.width || warped.height > plain.height,
            "vertical warp must grow the canvas: plain {}x{} warped {}x{}",
            plain.width,
            plain.height,
            warped.width,
            warped.height
        );
        assert!(
            warped_max_x >= plain_max_x && warped_max_y >= plain_max_y,
            "vertical warp ink must reach at least as far down-right: plain \
             ({plain_max_x},{plain_max_y}) warped ({warped_max_x},{warped_max_y})"
        );
        assert!(
            (warped_max_x as u32) < warped.width - 1 && (warped_max_y as u32) < warped.height - 1,
            "vertical warp ink must not touch the canvas edge (clipping)"
        );
    }

    #[test]
    fn mesh_warp_applies_on_formula_path() {
        // Regression: the warp used to be ignored on the formula/on-path path.
        let mut base = base_params();
        base.text = "FORMULA".to_string();
        base.width_px = 320;
        base.text_layout_mode = TextLayoutMode::Formula;
        base.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            ..TextFormulaLayoutParams::default()
        };
        let plain = render_text_to_image(&base, None).expect("formula plain render");

        let mut identity = base.clone();
        identity.raster_transform = Some(identity_mesh(13, 13));
        let ident = render_text_to_image(&identity, None).expect("formula identity render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (ident.width, ident.height, &ident.rgba),
            "formula identity warp must be byte-identical to no warp"
        );

        let mut warped_params = base.clone();
        warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
        let warped = render_text_to_image(&warped_params, None).expect("formula warp render");
        assert_ne!(
            (plain.width, plain.height, &plain.rgba),
            (warped.width, warped.height, &warped.rgba),
            "formula non-identity warp must change the render"
        );
        assert!(
            warped.width > plain.width || warped.height > plain.height,
            "formula warp must grow the canvas: plain {}x{} warped {}x{}",
            plain.width,
            plain.height,
            warped.width,
            warped.height
        );
        let (_, _, wmax_x, wmax_y) =
            alpha_box(warped.width, warped.height, &warped.rgba).expect("formula warp ink");
        assert!(
            (wmax_x as u32) < warped.width - 1 && (wmax_y as u32) < warped.height - 1,
            "formula warp ink must not touch the canvas edge (clipping)"
        );
    }

    #[test]
    fn mesh_warp_applies_on_shape_path() {
        // `Shape` reuses the formula render path; the warp must apply there too.
        let mut base = base_params();
        base.text = "SHAPE PATH".to_string();
        base.width_px = 240;
        base.text_layout_mode = TextLayoutMode::Shape;
        base.text_shape = TextShape::Free;
        let plain = render_text_to_image(&base, None).expect("shape plain render");

        let mut identity = base.clone();
        identity.raster_transform = Some(identity_mesh(13, 13));
        let ident = render_text_to_image(&identity, None).expect("shape identity render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (ident.width, ident.height, &ident.rgba),
            "shape identity warp must be byte-identical to no warp"
        );

        let mut warped_params = base.clone();
        warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
        let warped = render_text_to_image(&warped_params, None).expect("shape warp render");
        assert_ne!(
            (plain.width, plain.height, &plain.rgba),
            (warped.width, warped.height, &warped.rgba),
            "shape non-identity warp must change the render"
        );
    }

    #[test]
    fn mesh_warp_applies_on_vector_lines_path() {
        // Regression: the warp used to be ignored on the custom-vector-lines path,
        // which also uses a FIXED output canvas. Identity keeps that fixed canvas and
        // stays byte-identical; a non-identity warp drops the fixed canvas (like a
        // global rotation) and grows to the warped content bounds without clipping.
        let mut base = base_params();
        base.text = "VECTOR".to_string();
        base.width_px = 260;
        base.text_layout_mode = TextLayoutMode::CustomVectorLines;
        base.vector_lines_layout = TextVectorLinesLayoutParams {
            width_px: 260,
            height_px: 100,
            lines: vec![TextVectorLine {
                points: vec![
                    TextVectorPoint { x: 8.0, y: 50.0 },
                    TextVectorPoint { x: 130.0, y: 50.0 },
                    TextVectorPoint { x: 252.0, y: 50.0 },
                ],
                corner_smoothing_px: 16.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };
        let plain = render_text_to_image(&base, None).expect("vector-lines plain render");

        let mut identity = base.clone();
        identity.raster_transform = Some(identity_mesh(13, 13));
        let ident = render_text_to_image(&identity, None).expect("vector-lines identity render");
        assert_eq!(
            (plain.width, plain.height, &plain.rgba),
            (ident.width, ident.height, &ident.rgba),
            "vector-lines identity warp must keep the fixed canvas and stay byte-identical"
        );

        let mut warped_params = base.clone();
        warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
        let warped = render_text_to_image(&warped_params, None).expect("vector-lines warp render");
        assert_ne!(
            (plain.width, plain.height, &plain.rgba),
            (warped.width, warped.height, &warped.rgba),
            "vector-lines non-identity warp must change the render"
        );
        assert!(
            warped.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0),
            "vector-lines warp must still ink pixels"
        );
        let (_, _, wmax_x, wmax_y) =
            alpha_box(warped.width, warped.height, &warped.rgba).expect("vector-lines warp ink");
        assert!(
            (wmax_x as u32) < warped.width - 1 && (wmax_y as u32) < warped.height - 1,
            "vector-lines warp ink must not touch the canvas edge (clipping)"
        );
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
    fn global_rotation_rotates_whole_block_while_vector() {
        let mut params = base_params();
        params.text = "Hello world".to_string();

        // Baseline: no global rotation -> wide, short horizontal block.
        params.global_rotation_deg = 0.0;
        let plain = render_text_to_image(&params, None).expect("plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");
        assert!(
            plain_w > plain_h,
            "horizontal 'Hello world' should be wide and short: {plain_w}x{plain_h}"
        );

        // A 0.0 value must be a true no-op: the routing gate uses abs > EPSILON,
        // so it stays on the normal path and is byte-identical across renders.
        let plain_again = render_text_to_image(&params, None).expect("plain render again");
        assert_eq!(
            plain.rgba, plain_again.rgba,
            "global_rotation_deg = 0.0 must be deterministic and unchanged"
        );

        // 90° rotates the whole laid-out block (vector) -> tall and narrow.
        params.global_rotation_deg = 90.0;
        let rotated = render_text_to_image(&params, None).expect("rotated render");
        assert!(
            rotated.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0),
            "rotated block must render visible pixels"
        );
        let (rotated_w, rotated_h) =
            alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
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
        params.hanging_punctuation = 1.0;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render hanging punctuation case: {error}")
        });
        assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
    }

    #[test]
    fn the_hanging_weight_moves_the_line_origin_between_the_two_ends() {
        // Synthetic run layout: 8px of leading hang, 2px of trailing hang inside a
        // 30px line. The weight scales BOTH the alignment width and the origin shift,
        // and must reproduce the two historical behaviours exactly at the ends.
        let layout = super::HorizontalRunLayout {
            glyph_xs: vec![0.0, 10.0, 20.0],
            line_width_px: 30.0,
            leading_hang_px: 8.0,
            trailing_hang_px: 2.0,
        };

        // Weight 0 == the old "off": the logical width, bit for bit, and no shift.
        assert_eq!(layout.align_width_px(0.0), 30.0);
        assert_eq!(layout.origin_shift_px(0.0), 0.0);
        // Weight 1 == the old "on": the fully hung visual width and the whole lead.
        assert_eq!(layout.align_width_px(1.0), 20.0);
        assert_eq!(layout.origin_shift_px(1.0), 8.0);
        // A mid weight lands strictly between, on both numbers.
        let mid_width = layout.align_width_px(0.5);
        let mid_shift = layout.origin_shift_px(0.5);
        assert!(
            (20.0..30.0).contains(&mid_width) && mid_width > 20.0,
            "mid alignment width {mid_width} must be strictly between 20 and 30"
        );
        assert!(
            (0.0..8.0).contains(&mid_shift) && mid_shift > 0.0,
            "mid origin shift {mid_shift} must be strictly between 0 and 8"
        );

        // ...and so does the resulting centered line origin, which is what the eye sees.
        let origin = |weight: f32| {
            super::horizontal_line_offset(
                100,
                layout.align_width_px(weight),
                HorizontalAlign::CENTER,
            ) as f32
                - layout.origin_shift_px(weight)
        };
        let (off, mid, full) = (origin(0.0), origin(0.5), origin(1.0));
        assert!(
            full < mid && mid < off,
            "the mid-weight origin must sit strictly between the two ends: \
             off={off} mid={mid} full={full}"
        );
    }

    #[test]
    fn a_degenerate_all_hanging_line_never_hangs_at_any_weight() {
        // A line made only of hanging punctuation has no visual width to align by, so
        // the guard reports "nothing hangs" — for EVERY weight, or the layout would
        // jump as the slider moves.
        let layout = super::HorizontalRunLayout {
            glyph_xs: vec![0.0, 10.0],
            line_width_px: 20.0,
            leading_hang_px: 0.0,
            trailing_hang_px: 0.0,
        };
        for weight in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(layout.align_width_px(weight), 20.0, "weight {weight}");
            assert_eq!(layout.origin_shift_px(weight), 0.0, "weight {weight}");
        }
    }

    #[test]
    fn the_hanging_weight_reaches_the_pixels_and_broken_values_are_clamped() {
        // The weight comes from a UI slider and a project file, so a broken value must
        // degrade to a legal one instead of poisoning the layout with NaN.
        // TWO lines, only the first with a leading hanging quote: the hang slides that
        // line left RELATIVE to the other one, which a one-line fixture could not show
        // (a rigid translation of the only line is invisible once the canvas is
        // cropped to the ink).
        let mut params = base_params();
        params.text = "«Hi\nAbc".to_string();
        params.align = HorizontalAlign::LEFT;

        let render = |weight: f32| {
            let mut params = params.clone();
            params.hanging_punctuation = weight;
            let image = render_text_to_image(&params, None).expect("render should succeed");
            (image.width, image.height, image.rgba)
        };

        let off = render(0.0);
        let full = render(1.0);
        assert_ne!(
            off, full,
            "the fixture must actually react to hanging punctuation, \
             otherwise the assertions below are vacuous"
        );
        // NaN is "off", never "half" and never a panic.
        assert_eq!(render(f32::NAN), off);
        assert_eq!(render(-2.0), off);
        assert_eq!(render(f32::NEG_INFINITY), off);
        // Above the range the hang saturates instead of overshooting.
        assert_eq!(render(4.0), full);
        assert_eq!(render(f32::INFINITY), full);
        // A legal mid weight is a real third layout, not a rounded end: the slider is
        // continuous all the way down to the pixels.
        let mid = render(0.5);
        assert_ne!(mid, off, "a half hang must not collapse onto the off layout");
        assert_ne!(mid, full, "a half hang must not collapse onto the full layout");
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

    /// The glyph height scale is anchored at the run BASELINE: an inline
    /// `<stretching=100%,H%>` span changes a glyph's ink EXTENT and nothing else,
    /// so its bottom edge stays on the same row as the unscaled glyphs beside it.
    ///
    /// Before the baseline anchor the scale pivoted the glyph's own ink-box
    /// centre, which left a 50 % glyph floating ~6 px above the line (the reported
    /// "текст" defect) and pushed a 200 % one below it. Tolerance is 1 px: the
    /// outline is rasterized at the scaled size, so the bottom edge can land one
    /// anti-aliased row either way.
    #[test]
    fn inline_height_scale_keeps_glyphs_on_the_line_baseline() {
        for height_percent in [50u32, 200] {
            let mut params = base_params();
            params.enable_inline_style_tags = true;
            params.width_px = 512;
            params.text = format!("H <stretching=100%,{height_percent}%>H</stretching> H");

            let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
                panic!("render_next should render the inline height case: {error}")
            });
            let clusters = ink_column_clusters(&rendered);
            assert_eq!(
                clusters.len(),
                3,
                "the three space-separated H glyphs must stay separate at {height_percent}%: {clusters:?}"
            );

            let baseline_row = clusters[0].3;
            for (idx, cluster) in clusters.iter().enumerate() {
                let delta = cluster.3 as i64 - baseline_row as i64;
                assert!(
                    delta.abs() <= 1,
                    "glyph {idx} ink bottom {} must sit on the baseline row {baseline_row} at {height_percent}% (delta {delta}): {clusters:?}",
                    cluster.3
                );
            }

            // The scale must still HAPPEN — a baseline anchor is not a no-op.
            let plain_height = clusters[0].3 - clusters[0].2;
            let scaled_height = clusters[1].3 - clusters[1].2;
            if height_percent < 100 {
                assert!(
                    scaled_height * 2 < plain_height * 3,
                    "the 50% glyph must be visibly shorter ({scaled_height} vs {plain_height})"
                );
            } else {
                assert!(
                    scaled_height > plain_height,
                    "the 200% glyph must be visibly taller ({scaled_height} vs {plain_height})"
                );
            }
        }
    }

    /// A partial-line inline `<stretching>` height span must not move the COLUMN
    /// GAPS of vertical text.
    ///
    /// The vertical path reads `line_extra_spacing_table` as a horizontal gap
    /// between columns, so the old "any overlapping stretch span rewrites the
    /// line's height percent" rule leaked a per-character height change straight
    /// into the spacing between whole columns (at 50 % it pulled them 18 px
    /// closer here). The grow-only ink-rise room is meaningless for a
    /// horizontal gap and is deliberately NOT applied there either, so the table
    /// the vertical path consumes must be invariant to the tag in BOTH
    /// directions.
    ///
    /// The end-to-end half of the check uses the shrinking direction only: a
    /// TALLER glyph legitimately widens its own cell, because a vertical cell is
    /// sized by its scaled ink extent (`measure_vertical_glyph_visual_width`,
    /// `vertical_step_follows_glyph_ink_height`) — that is the cell width, not
    /// the gap, and it is not what this contract is about.
    #[test]
    fn vertical_column_gaps_ignore_an_inline_height_span() {
        let mut params = base_params();
        params.enable_inline_style_tags = true;
        params.text_line_mode = TextLineMode::Vertical;
        let font_size_px = params.font_size_px;

        // The exact table the vertical path is handed for its column gaps.
        let column_gap_table = |text: &str| -> Vec<f32> {
            let parsed = crate::inline_styles::parse_inline_style_tags(text, font_size_px);
            let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
            super::compute_line_extra_spacing_table(
                &params,
                parsed.plain_text.as_str(),
                offsets.as_slice(),
                Some(parsed.spans.as_slice()),
                font_size_px,
                0.0,
            )
        };
        let plain_table = column_gap_table("HH\nHH");
        for percent in ["50%", "200%"] {
            let tagged_table =
                column_gap_table(&format!("H<stretching=100%,{percent}>H</stretching>\nHH"));
            assert_eq!(
                tagged_table, plain_table,
                "an inline height span must not change the column-gap table at {percent}"
            );
        }

        // End-to-end: the shrinking direction cannot touch the cell width, so any
        // movement of the ink columns would come from the gap.
        let column_edges = |text: &str| -> Vec<(usize, usize)> {
            let mut render_params = params.clone();
            render_params.text = text.to_string();
            render_params.width_px = 512;
            let rendered = render_text_to_image(&render_params, None)
                .unwrap_or_else(|error| panic!("vertical stretch render: {error}"));
            ink_column_clusters(&rendered)
                .into_iter()
                .map(|(min_x, max_x, _, _)| (min_x, max_x))
                .collect()
        };
        let plain = column_edges("HH\nHH");
        assert_eq!(plain.len(), 2, "two ink columns expected: {plain:?}");
        let shrunk = column_edges("H<stretching=100%,50%>H</stretching>\nHH");
        assert_eq!(
            shrunk.len(),
            plain.len(),
            "the column count must not change: {shrunk:?} vs {plain:?}"
        );
        assert_eq!(
            shrunk[1].0 as i64 - shrunk[0].1 as i64,
            plain[1].0 as i64 - plain[0].1 as i64,
            "a partial 50% height span must not narrow the column gap ({shrunk:?} vs {plain:?})"
        );
    }

    /// PURE geometry of the baseline anchor, at float precision.
    ///
    /// The pixel-level baseline tests reduce ink to integer rows with a 1 px
    /// tolerance, so a sub-pixel per-glyph drift — exactly the shape of the old
    /// box-centre bug, which displaced each glyph by
    /// `(glyph_h / 2 − placement_top) · (1 − height_mul)` — could slip through.
    /// This pins the invariant directly on glyphs with UNLIKE `placement_top`:
    /// the scaled box's BOTTOM edge relative to the baseline must scale by
    /// exactly `height_mul`, whatever the glyph's own ink box, and the whole box
    /// must be untouched at `height_mul == 1`.
    #[test]
    fn baseline_anchored_scale_is_exact_for_unlike_ink_boxes() {
        // (glyph_w, glyph_h, placement_top): a capital, a descender-only glyph
        // (box entirely BELOW the baseline, negative top), a period sitting on
        // the baseline, and a tall accented glyph.
        let boxes = [
            (20.0f32, 26.0f32, 26.0f32),
            (14.0, 9.0, -1.0),
            (5.0, 5.0, 5.0),
            (21.0, 37.0, 37.0),
        ];
        let baseline_y = 100.0f32;

        for height_mul in [0.5f32, 1.0, 2.0, 0.37, 2.91] {
            let scale = super::GlyphScaleSettings { width_mul: 1.0, height_mul };
            for (glyph_w, glyph_h, placement_top) in boxes {
                // Content-space box top, as `build_horizontal_placement` derives it.
                let src_top = baseline_y - placement_top;
                let (_, center_y) = scale.scaled_center_about_baseline(
                    0.0,
                    src_top,
                    glyph_w,
                    glyph_h,
                    baseline_y,
                );
                // The drawn box is `center +- scaled_height / 2` (the outline is
                // scaled by `height_mul` about `center`, see `glyph_outline_transform`).
                let scaled_bottom = center_y + glyph_h * height_mul * 0.5;
                let scaled_top = center_y - glyph_h * height_mul * 0.5;
                // Both edges are the unscaled ones scaled about the baseline.
                let expect_bottom = baseline_y + (src_top + glyph_h - baseline_y) * height_mul;
                let expect_top = baseline_y + (src_top - baseline_y) * height_mul;
                assert!(
                    (scaled_bottom - expect_bottom).abs() <= 1e-4,
                    "bottom edge must scale about the baseline (h={glyph_h}, top={placement_top}, \
                     mul={height_mul}): {scaled_bottom} vs {expect_bottom}"
                );
                assert!(
                    (scaled_top - expect_top).abs() <= 1e-4,
                    "top edge must scale about the baseline (h={glyph_h}, top={placement_top}, \
                     mul={height_mul}): {scaled_top} vs {expect_top}"
                );

                // The rect form must describe the SAME box as the centre form.
                let (_, rect_top, _, rect_height) = scale.scaled_rect_about_baseline(
                    0.0,
                    src_top,
                    glyph_w,
                    glyph_h,
                    baseline_y,
                );
                assert!(
                    (rect_top + rect_height * 0.5 - center_y).abs() <= 1e-4,
                    "rect and centre helpers must agree: {rect_top}+{rect_height} vs {center_y}"
                );

                if (height_mul - 1.0).abs() <= f32::EPSILON {
                    // An unscaled glyph must land exactly where the plain
                    // box-centre placement puts it, bit for bit.
                    let plain_center_y = src_top + glyph_h * 0.5;
                    assert_eq!(
                        center_y, plain_center_y,
                        "height_mul == 1 must be an exact no-op (h={glyph_h}, top={placement_top})"
                    );
                }
            }
        }

        // The defect this replaces: at a box-centre anchor two glyphs with
        // unlike ink boxes end up on DIFFERENT baselines. Here they must not.
        let scale = super::GlyphScaleSettings { width_mul: 1.0, height_mul: 0.5 };
        let bottoms: Vec<f32> = boxes
            .iter()
            .map(|(glyph_w, glyph_h, placement_top)| {
                let src_top = baseline_y - placement_top;
                let (_, center_y) = scale.scaled_center_about_baseline(
                    0.0,
                    src_top,
                    *glyph_w,
                    *glyph_h,
                    baseline_y,
                );
                // Baseline-relative bottom, normalized by the glyph's own
                // unscaled baseline-relative bottom: identical for every glyph.
                let unscaled_bottom = src_top + glyph_h - baseline_y;
                (center_y + glyph_h * 0.5 * 0.5 - baseline_y) - unscaled_bottom * 0.5
            })
            .collect();
        for offset in &bottoms {
            assert!(
                offset.abs() <= 1e-4,
                "every glyph must scale about ONE baseline, not its own centre: {bottoms:?}"
            );
        }
    }

    /// Path of a secondary fixture face, used to build a MIXED-face line. Its
    /// ink extents differ from `LiberationSans-Regular`, which is the whole
    /// point: the grow-only room must follow the face that carries the stretch.
    fn second_test_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/NotoMono-Regular.ttf")
    }

    /// The grow-only inline height room follows the glyphs that ACTUALLY carry
    /// the stretch, on their OWN face, not the face of the line's first glyph.
    ///
    /// A tall `<stretching>` span can sit on an inline `<font=…>` or on a
    /// fallback face whose letters reach higher than the face the line opens
    /// with. Sizing the room from one buffer-wide face then mis-allocates the gap
    /// above the line.
    #[test]
    fn inline_height_room_follows_the_stretched_glyphs_own_face() {
        use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

        let mut db = fontdb::Database::new();
        for path in [test_font_path(), second_test_font_path()] {
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("fixture font {}: {error}", path.display()));
            db.load_font_data(bytes);
        }
        let families: Vec<String> = db
            .faces()
            .filter_map(|face| face.families.first().cloned().map(|(name, _)| name))
            .collect();
        assert_eq!(families.len(), 2, "two fixture faces expected: {families:?}");
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        let em = 36.0f32;
        // Ink rise above the baseline of the glyphs each face draws for `text`,
        // measured the way `InlineHeightRoom::measure` measures it.
        let ink_rise = |font_system: &mut FontSystem, family: &str, text: &str| -> f32 {
            let mut probe = Buffer::new(font_system, Metrics::new(em, em));
            probe.set_size(font_system, None, None);
            let attrs = Attrs::new()
                .family(Family::Name(family))
                .metrics(Metrics::new(em, em));
            probe.set_text(font_system, text, &attrs, Shaping::Advanced);
            probe.shape_until_scroll(font_system, false);
            let glyphs: Vec<_> = probe
                .layout_runs()
                .flat_map(|run| run.glyphs.to_vec())
                .collect();
            let mut cache = crate::vector::OutlineCache::new();
            glyphs
                .iter()
                .filter_map(|glyph| {
                    crate::glyph_blit::resolve_outline_for_glyph(
                        font_system,
                        &mut cache,
                        glyph,
                        None,
                    )
                })
                .map(|outline| (-outline.local_bbox().0[1]).max(0.0))
                .fold(0.0f32, f32::max)
        };
        let opening_rise = ink_rise(&mut font_system, &families[0], "Ab");
        let stretched_rise = ink_rise(&mut font_system, &families[1], "Cd");
        assert!(
            (stretched_rise - opening_rise).abs() > 0.5,
            "the two fixture faces must draw visibly different ink heights, or this \
             test cannot tell the two implementations apart \
             ({opening_rise} vs {stretched_rise})"
        );

        // `Cd` is stretched to 300% AND shaped in the second face; `Ab` opens the
        // line in the first face, exactly the mixed-face case.
        let parsed = crate::inline_styles::parse_inline_style_tags(
            "Ab<stretching=100%,300%>Cd</stretching>",
            em,
        );
        assert_eq!(parsed.plain_text, "AbCd");

        let opening_attrs = Attrs::new()
            .family(Family::Name(families[0].as_str()))
            .metrics(Metrics::new(em, em));
        let stretched_attrs = Attrs::new()
            .family(Family::Name(families[1].as_str()))
            .metrics(Metrics::new(em, em));
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em));
        buffer.set_size(&mut font_system, None, None);
        buffer.set_rich_text(
            &mut font_system,
            [("Ab", opening_attrs.clone()), ("Cd", stretched_attrs)],
            &opening_attrs,
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut font_system, false);

        let mut params = base_params();
        params.font_size_px = em;
        params.enable_inline_style_tags = true;
        let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
        let room = super::InlineHeightRoom::measure(
            &params,
            &buffer,
            &mut font_system,
            offsets.as_slice(),
            Some(parsed.spans.as_slice()),
        );

        // 300% against a 100% global height leaves an excess multiplier of 2.0.
        let excess = 2.0f32;
        assert!(
            (room.above(0) - stretched_rise * excess).abs() <= 1e-3,
            "the room must come from the STRETCHED glyphs' own face: {} vs {}",
            room.above(0),
            stretched_rise * excess
        );
        // The exact regression: sizing the room from the line's opening face.
        assert!(
            (room.above(0) - opening_rise * excess).abs() > 1e-3,
            "sizing the room from the line's opening face is wrong ({} vs the \
             opening face's {})",
            room.above(0),
            opening_rise * excess
        );
    }

    /// A PARTIAL inline `<stretching>` height span never re-spaces its line, and
    /// the grow-only room lands on the gap ABOVE the tagged line and nowhere else.
    ///
    /// Entry `i` is the extra advance of the gap below line `i` — the gap ABOVE
    /// line `i+1` — so it must grow by line `i+1`'s ink rise and by nothing else.
    /// Feeding an exact `InlineHeightRoom` pins that pairing without a shaped
    /// buffer; the measurement itself is covered by
    /// `inline_height_room_*` and the end-to-end tests.
    #[test]
    fn inline_height_span_grows_the_line_advance_and_never_shrinks_it() {
        let mut params = base_params();
        params.enable_inline_style_tags = true;
        let font_size_px = params.font_size_px;
        let no_room = super::InlineHeightRoom::default();

        let advance_table = |text: &str, room: &super::InlineHeightRoom| -> Vec<f32> {
            let parsed = crate::inline_styles::parse_inline_style_tags(text, font_size_px);
            let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
            super::line_baseline_advance_table(
                &params,
                parsed.plain_text.as_str(),
                offsets.as_slice(),
                Some(parsed.spans.as_slice()),
                font_size_px,
                0.0,
                room,
            )
        };

        let plain = advance_table("AB\nCD\nEF", &no_room);
        assert_eq!(plain.len(), 3, "three layout lines: {plain:?}");

        // Half the height on half of line 1 asks for no room, and the spacing
        // table itself must not react to the stretch tag either.
        let shrunk = advance_table("A<stretching=100%,50%>B</stretching>\nCD\nEF", &no_room);
        assert_eq!(shrunk, plain, "a partial 50% span must not re-space the line");

        // A tall span on line 1 (index 0) asks for room ABOVE line 1 — a gap that
        // does not exist — so NO gap moves. This is the user-reported defect: the
        // distance to the line BELOW a stretched line must not grow.
        let first_line = super::InlineHeightRoom::from_above(vec![26.0, 0.0, 0.0]);
        let tall_first = advance_table("A<stretching=100%,200%>B</stretching>\nCD\nEF", &first_line);
        assert_eq!(
            tall_first, plain,
            "a tall span must not widen the gap BELOW its own line: {tall_first:?} vs {plain:?}"
        );

        // The same span on line 2 (index 1) grows the gap ABOVE it, entry 0, by
        // that line's ink rise — and only that entry.
        let second_line = super::InlineHeightRoom::from_above(vec![0.0, 26.0, 0.0]);
        let tall_second =
            advance_table("AB\nC<stretching=100%,200%>D</stretching>\nEF", &second_line);
        assert!(
            (tall_second[0] - plain[0] - 26.0).abs() <= 1e-3,
            "gap above the tagged line must grow by its ink rise: {tall_second:?}"
        );
        assert!(
            (tall_second[1] - plain[1]).abs() <= 1e-3,
            "the gap below the tagged line must not move: {tall_second:?} vs {plain:?}"
        );
    }

    /// Baseline-to-baseline advance between the two ink bands of a two-line
    /// render. Every text here uses glyphs without descenders, so each band's
    /// bottom row IS that line's baseline.
    fn two_line_baseline_advance(text: &str) -> usize {
        let mut params = base_params();
        params.enable_inline_style_tags = true;
        params.width_px = 512;
        params.text = text.to_string();
        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render the two-line height case: {error}")
        });
        let bands = ink_row_bands(&rendered);
        assert_eq!(bands.len(), 2, "two separated text lines expected for {text:?}: {bands:?}");
        bands[1].1 - bands[0].1
    }

    /// End-to-end: an inline `<stretching>` height span grows the gap ABOVE its
    /// own line and NOTHING else.
    ///
    /// Two separate defects are pinned here, both user-reported.
    /// - A 50 % span used to drive the whole line's spacing through
    ///   `effective_spacing_percent`, pulling line 2 up by `font_size * 0.5`
    ///   (72 -> 54 px here) until the ink bands merged.
    /// - A 200 % span used to widen the gap BELOW its line as well, by the face
    ///   descent (72 -> 80 px). The rule is upward-only now: the gap below a
    ///   stretched line must be byte-identical to the untagged render, and a
    ///   stretched descender is allowed to reach into it (see `InlineHeightRoom`).
    #[test]
    fn inline_height_span_grows_only_the_gap_above_its_own_line() {
        let plain = two_line_baseline_advance("H H\nH H");

        // Line 1 tagged: the gap BELOW it must not move, in either direction.
        let shrunk_below = two_line_baseline_advance("H <stretching=100%,50%>H</stretching>\nH H");
        let tall_below = two_line_baseline_advance("H <stretching=100%,200%>H</stretching>\nH H");
        assert_eq!(
            shrunk_below, plain,
            "a partial 50% span must leave the gap below its line alone ({shrunk_below} vs {plain})"
        );
        assert_eq!(
            tall_below, plain,
            "a partial 200% span must NOT widen the gap below its line ({tall_below} vs {plain})"
        );

        // Line 2 tagged: the gap ABOVE it grows, by the glyph's ink rise. `H` is a
        // cap-height glyph (~0.717 em = 25.8 px at 36 px), so the advance lands
        // near 72 + 26; the face ascent (~32.6 px) would overshoot to ~105.
        let tall_above = two_line_baseline_advance("H H\nH <stretching=100%,200%>H</stretching>");
        assert!(
            tall_above > plain,
            "a 200% span must grow the gap above its line ({tall_above} vs {plain})"
        );
        assert!(
            tall_above.abs_diff(plain + 26) <= 2,
            "the growth must be the cap-height ink rise, not the face ascent \
             ({tall_above} vs ~{} expected; the face-ascent rule overshot to ~{})",
            plain + 26,
            plain + 33
        );
    }

    /// The room grows by the glyphs' REAL INK rise, not by the face `ascent`.
    ///
    /// A face's `ascent` includes headroom far above the ink of most lines, so
    /// scaling it made the gap grow visibly faster than the letters did — the
    /// user-reported "the distance to the line above grows a bit faster than the
    /// character height". A line of x-height-only glyphs is where the two numbers
    /// diverge most: `x` tops out around 0.53 em against a ~0.9 em ascent.
    #[test]
    fn inline_height_room_grows_by_real_ink_not_face_ascent() {
        use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

        let mut db = fontdb::Database::new();
        let bytes = std::fs::read(test_font_path()).expect("fixture font bytes");
        db.load_font_data(bytes);
        let family = db
            .faces()
            .next()
            .and_then(|face| face.families.first().cloned())
            .map(|(name, _language)| name)
            .expect("fixture family name");
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        let em = 36.0f32;
        let attrs = Attrs::new()
            .family(Family::Name(family.as_str()))
            .metrics(Metrics::new(em, em));

        // `xx` with the SECOND `x` stretched to 200%.
        let parsed = crate::inline_styles::parse_inline_style_tags(
            "x<stretching=100%,200%>x</stretching>",
            em,
        );
        assert_eq!(parsed.plain_text, "xx");
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em));
        buffer.set_size(&mut font_system, None, None);
        buffer.set_text(&mut font_system, parsed.plain_text.as_str(), &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);

        // The two candidate measurements for the same glyph, side by side.
        let glyphs: Vec<_> = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.to_vec())
            .collect();
        let glyph = glyphs.first().expect("a shaped `x`");
        let mut outline_cache = crate::vector::OutlineCache::new();
        let ink_rise = crate::glyph_blit::resolve_outline_for_glyph(
            &mut font_system,
            &mut outline_cache,
            glyph,
            None,
        )
        .map(|outline| (-outline.local_bbox().0[1]).max(0.0))
        .expect("`x` has an outline");
        let face_ascent = font_system
            .get_font(glyph.font_id)
            .map(|font| font.as_swash().metrics(&[]).scale(em).ascent)
            .expect("fixture face metrics");
        assert!(
            face_ascent - ink_rise > 10.0,
            "the fixture must keep ink rise and face ascent far apart, or this test \
             cannot tell the two rules apart ({ink_rise} vs {face_ascent})"
        );

        let mut params = base_params();
        params.font_size_px = em;
        params.enable_inline_style_tags = true;
        let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
        let room = super::InlineHeightRoom::measure(
            &params,
            &buffer,
            &mut font_system,
            offsets.as_slice(),
            Some(parsed.spans.as_slice()),
        );

        // 200% against a 100% global height leaves an excess multiplier of 1.0.
        assert!(
            (room.above(0) - ink_rise).abs() <= 1e-3,
            "the room must be the glyph's ink rise: {} vs {ink_rise}",
            room.above(0)
        );
        assert!(
            room.above(0) < face_ascent - 10.0,
            "the room must NOT be the face ascent: {} vs {face_ascent}",
            room.above(0)
        );
    }

    /// Several height tags on ONE line ask for the LARGEST rise among them, never
    /// for their sum.
    ///
    /// Two 150 % spans must space exactly like one, and any mix must space
    /// exactly like its tallest member alone — wherever that member sits in
    /// reading order, so a "last span wins" rule (the shape of the original
    /// whole-line spacing defect) is caught as well as a summing one. All texts
    /// use `x`, whose ink rise is ~19 px at 36 px: a sum of three spans would
    /// overshoot the single-span advance by tens of px, far outside the 1 px
    /// rasterization tolerance.
    #[test]
    fn several_height_spans_on_one_line_take_the_maximum_not_the_sum() {
        let plain = two_line_baseline_advance("x x\nx x");
        let single_200 =
            two_line_baseline_advance("x x\nx <stretching=100%,200%>x</stretching> x x");
        assert!(
            single_200 > plain,
            "a 200% span must grow the gap above its line ({single_200} vs {plain})"
        );

        // Two spans, the larger first and then last.
        for text in [
            "x x\nx <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching> x",
            "x x\nx <stretching=100%,150%>x</stretching> <stretching=100%,200%>x</stretching> x",
        ] {
            assert_eq!(
                two_line_baseline_advance(text),
                single_200,
                "two spans must space like their tallest alone, not like their sum: {text:?}"
            );
        }

        // Three spans, the largest first, in the middle and last: a sum and a max
        // can coincide on two equal values but never on three distinct ones.
        for text in [
            "x x\nx <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching> <stretching=100%,120%>x</stretching>",
            "x x\nx <stretching=100%,120%>x</stretching> <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching>",
            "x x\nx <stretching=100%,120%>x</stretching> <stretching=100%,150%>x</stretching> <stretching=100%,200%>x</stretching>",
        ] {
            assert_eq!(
                two_line_baseline_advance(text),
                single_200,
                "three spans must space like their tallest alone, not like their sum: {text:?}"
            );
        }

        // Two EQUAL spans must also space like one of them.
        let one_150 = two_line_baseline_advance("x x\nx <stretching=100%,150%>x</stretching> x x");
        let two_150 = two_line_baseline_advance(
            "x x\nx <stretching=100%,150%>x</stretching> <stretching=100%,150%>x</stretching> x",
        );
        assert_eq!(
            two_150, one_150,
            "two identical spans must space exactly like one ({two_150} vs {one_150})"
        );
        assert!(
            one_150 < single_200,
            "a 150% span must ask for less room than a 200% one ({one_150} vs {single_200})"
        );
    }

    /// The GLOBAL `glyph_height_percent` is anchored at the baseline too.
    ///
    /// It is the same defect, only harder to see: with a box-centre pivot every
    /// glyph drifted by its OWN centre-to-baseline distance, so `H` and `.` ended
    /// up on different baselines (measured 13 vs 18 at 50 %). Glyphs with very
    /// different ink boxes must keep ONE shared bottom row at any height.
    #[test]
    fn global_glyph_height_keeps_one_shared_baseline() {
        for height_percent in [50.0f32, 200.0] {
            let mut params = base_params();
            params.width_px = 512;
            params.glyph_height_percent = height_percent;
            params.text = "H .".to_string();

            let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
                panic!("render_next should render the global height case: {error}")
            });
            let clusters = ink_column_clusters(&rendered);
            assert_eq!(
                clusters.len(),
                2,
                "`H` and `.` must stay separate at {height_percent}%: {clusters:?}"
            );
            let delta = clusters[1].3 as i64 - clusters[0].3 as i64;
            assert!(
                delta.abs() <= 1,
                "`H` and `.` must share one ink bottom row at {height_percent}% (delta {delta}): {clusters:?}"
            );
        }
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
    fn ellipsis_expansion_replaces_only_u2026() {
        assert_eq!(replace_ellipsis_with_dots("Что…"), "Что...");
        assert_eq!(replace_ellipsis_with_dots("а… б…в"), "а... б...в");
        // Text without `…` must come back byte-identical.
        assert_eq!(replace_ellipsis_with_dots("Что..."), "Что...");
        // Sibling leader characters do not stand for three dots and stay put.
        assert_eq!(replace_ellipsis_with_dots("а‥б⋯в"), "а‥б⋯в");
    }

    #[test]
    fn ellipsis_expansion_is_off_when_disabled() {
        let mut params = base_params();
        params.text = "Что…".to_string();
        params.trim_extra_spaces = false;
        params.replace_ellipsis_with_dots = false;
        assert_eq!(prepare_source_text(&params.text, &params), "Что…");
    }

    #[test]
    fn ellipsis_expansion_feeds_sentence_newlines() {
        // The expansion runs FIRST, so the produced `...` is a sentence end for
        // `new_line_after_sentence` — the same as if the author typed three dots.
        let mut params = base_params();
        params.text = "Что… Дальше".to_string();
        params.trim_extra_spaces = false;
        params.replace_ellipsis_with_dots = true;
        params.new_line_after_sentence = true;
        assert_eq!(prepare_source_text(&params.text, &params), "Что...\nДальше");

        params.replace_ellipsis_with_dots = false;
        assert_eq!(prepare_source_text(&params.text, &params), "Что… Дальше");
    }

    #[test]
    fn base_pipeline_renders_expanded_ellipsis() {
        let mut params = base_params();
        params.text = "Что… дальше…".to_string();
        params.replace_ellipsis_with_dots = true;
        params.width_px = 320;

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render expanded-ellipsis case: {error}")
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
    fn global_rotation_rotates_formula_block() {
        // A flat horizontal on-path line (y = 0) is wide and short; a 90° global
        // rotation must turn the whole block tall and narrow, at the vector level.
        let mut params = base_params();
        params.text = "FORMULA".to_string();
        params.width_px = 320;
        params.text_layout_mode = TextLayoutMode::Formula;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            ..TextFormulaLayoutParams::default()
        };

        params.global_rotation_deg = 0.0;
        let plain = render_text_to_image(&params, None).expect("formula plain render");
        let (plain_w, plain_h) =
            alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("formula bounds");

        params.global_rotation_deg = 90.0;
        let rotated = render_text_to_image(&params, None).expect("formula rotated render");
        assert!(rotated.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        let (rotated_w, rotated_h) =
            alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
                .expect("formula rotated bounds");
        assert!(rotated_h > plain_h, "formula 90°: {rotated_h} !> {plain_h}");
        assert!(rotated_w < plain_w, "formula 90°: {rotated_w} !< {plain_w}");
    }

    #[test]
    fn global_rotation_rotates_vector_lines_block() {
        // Custom vector lines use a fixed canvas; a non-zero global rotation must
        // grow it to the rotated bounds (no clipping) and turn a wide line tall.
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
                    TextVectorPoint { x: 120.0, y: 40.0 },
                    TextVectorPoint { x: 240.0, y: 40.0 },
                ],
                corner_smoothing_px: 16.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };

        params.global_rotation_deg = 0.0;
        let plain = render_text_to_image(&params, None).expect("vector-lines plain render");
        let (plain_w, plain_h) = alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba)
            .expect("vector-lines bounds");

        params.global_rotation_deg = 90.0;
        let rotated = render_text_to_image(&params, None).expect("vector-lines rotated render");
        assert!(rotated.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        let (rotated_w, rotated_h) =
            alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
                .expect("vector-lines rotated bounds");
        assert!(rotated_h > plain_h, "vector-lines 90°: {rotated_h} !> {plain_h}");
        assert!(rotated_w < plain_w, "vector-lines 90°: {rotated_w} !< {plain_w}");
    }

    #[test]
    fn line_placement_shifts_vector_lines_perpendicular() {
        // A flat horizontal vector line on a fixed (untrimmed) canvas: the
        // content's top alpha row directly reflects the perpendicular shift.
        // +100% (сверху) must raise the content, -100% (снизу) must lower it,
        // and 0% sits between them.
        fn alpha_min_y(width: u32, height: u32, rgba: &[u8]) -> Option<usize> {
            let width = width as usize;
            (0..height as usize)
                .find(|&y| (0..width).any(|x| rgba[(y * width + x) * 4 + 3] != 0))
        }

        let mut params = base_params();
        params.text = "VECTOR".to_string();
        params.width_px = 260;
        params.text_layout_mode = TextLayoutMode::CustomVectorLines;
        params.vector_lines_layout = TextVectorLinesLayoutParams {
            width_px: 260,
            height_px: 140,
            lines: vec![TextVectorLine {
                points: vec![
                    TextVectorPoint { x: 8.0, y: 70.0 },
                    TextVectorPoint { x: 130.0, y: 70.0 },
                    TextVectorPoint { x: 252.0, y: 70.0 },
                ],
                corner_smoothing_px: 16.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };

        params.line_placement_percent = 0.0;
        let centered = render_text_to_image(&params, None).expect("vector-lines centered render");
        let centered_min_y = alpha_min_y(centered.width, centered.height, &centered.rgba)
            .expect("centered content");

        params.line_placement_percent = 100.0;
        let top = render_text_to_image(&params, None).expect("vector-lines top render");
        let top_min_y = alpha_min_y(top.width, top.height, &top.rgba).expect("top content");

        params.line_placement_percent = -100.0;
        let bottom = render_text_to_image(&params, None).expect("vector-lines bottom render");
        let bottom_min_y =
            alpha_min_y(bottom.width, bottom.height, &bottom.rgba).expect("bottom content");

        assert!(
            top_min_y < centered_min_y,
            "+100% (сверху) must raise content: {top_min_y} !< {centered_min_y}"
        );
        assert!(
            bottom_min_y > centered_min_y,
            "-100% (снизу) must lower content: {bottom_min_y} !> {centered_min_y}"
        );
    }

    #[test]
    fn line_placement_applied_on_formula_path() {
        // Mixed-height text (ascenders/descenders) so the per-glyph perpendicular
        // shift changes the trimmed raster; the direction itself is proven by the
        // vector-lines test and the `apply_line_placement` unit test.
        let mut params = base_params();
        params.text = "Apjqy bd".to_string();
        params.width_px = 320;
        params.text_layout_mode = TextLayoutMode::Formula;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            ..TextFormulaLayoutParams::default()
        };

        params.line_placement_percent = 0.0;
        let centered = render_text_to_image(&params, None).expect("formula centered render");
        assert!(centered.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));

        params.line_placement_percent = 100.0;
        let top = render_text_to_image(&params, None).expect("formula top render");
        assert!(top.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));

        params.line_placement_percent = -100.0;
        let bottom = render_text_to_image(&params, None).expect("formula bottom render");
        assert!(bottom.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));

        let centered_key = (centered.width, centered.height, centered.rgba);
        let top_key = (top.width, top.height, top.rgba);
        let bottom_key = (bottom.width, bottom.height, bottom.rgba);
        assert!(
            top_key != centered_key,
            "formula +100% must change the render vs 0%"
        );
        assert!(
            bottom_key != centered_key,
            "formula -100% must change the render vs 0%"
        );
        assert!(top_key != bottom_key, "formula +100% must differ from -100%");
    }

    #[test]
    fn line_placement_ignored_by_shape() {
        // Shape reuses the formula render path but must HIDE/ignore line
        // placement: a non-zero percent must produce a byte-identical image.
        let mut params = base_params();
        params.text = "shape gating stays byte identical".to_string();
        params.width_px = 280;
        params.text_layout_mode = TextLayoutMode::Shape;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * 24".to_string(),
            y_expr: "0".to_string(),
            ..TextFormulaLayoutParams::default()
        };

        params.line_placement_percent = 0.0;
        let zero = render_text_to_image(&params, None).expect("shape zero render");
        params.line_placement_percent = 80.0;
        let nonzero = render_text_to_image(&params, None).expect("shape nonzero render");
        assert_eq!(zero.width, nonzero.width);
        assert_eq!(zero.height, nonzero.height);
        assert_eq!(
            zero.rgba, nonzero.rgba,
            "Shape must ignore line_placement_percent"
        );
    }

    #[test]
    fn line_placement_ignored_by_raster_lines() {
        // CustomRasterLines reuses the drawn-lines render path but must
        // HIDE/ignore line placement: a non-zero percent must be byte-identical.
        let mut layout_image = image::RgbaImage::new(260, 80);
        layout_image.put_pixel(8, 40, image::Rgba([255, 0, 0, 255]));
        for x in 9..240 {
            layout_image.put_pixel(x, 40, image::Rgba([255, 0, 0, 128]));
        }
        let layout_path = std::env::temp_dir().join(format!(
            "manhwastudio_line_placement_raster_{}.png",
            std::process::id()
        ));
        layout_image
            .save(&layout_path)
            .unwrap_or_else(|error| panic!("should write raster layout image: {error}"));

        let mut params = base_params();
        params.text = "DRAWN".to_string();
        params.width_px = 260;
        params.text_layout_mode = TextLayoutMode::CustomRasterLines;
        params.drawn_lines_layout = TextDrawnLinesLayoutParams {
            image_path: Some(layout_path.clone()),
            ..TextDrawnLinesLayoutParams::default()
        };

        params.line_placement_percent = 0.0;
        let zero = render_text_to_image(&params, None).expect("raster zero render");
        params.line_placement_percent = 80.0;
        let nonzero = render_text_to_image(&params, None).expect("raster nonzero render");
        let _ = std::fs::remove_file(layout_path);
        assert_eq!(
            zero.rgba, nonzero.rgba,
            "CustomRasterLines must ignore line_placement_percent"
        );
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

    /// Alpha bounding box `(min_x, min_y, max_x, max_y)` in pixels, or `None` when
    /// the image is fully transparent.
    fn alpha_bbox(image: &RenderedTextImage) -> Option<(f32, f32, f32, f32)> {
        let width = image.width as usize;
        let height = image.height as usize;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (width, height, 0usize, 0usize);
        let mut found = false;
        for y in 0..height {
            for x in 0..width {
                if image.rgba[(y * width + x) * 4 + 3] == 0 {
                    continue;
                }
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                found = true;
            }
        }
        found.then_some((min_x as f32, min_y as f32, max_x as f32, max_y as f32))
    }

    #[test]
    fn extra_info_mean_and_median_land_near_symmetric_line_center() {
        // A short symmetric single line: both centers must be Some, sit inside the
        // image, and land near the inked alpha box center (loose tolerance).
        let mut params = base_params();
        params.text = "HH".to_string();
        params.align = HorizontalAlign::LEFT;
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let image = render_text_to_image(&params, None).expect("render should succeed");
        let mean = image.extra.mean_center.expect("mean center requested");
        let median = image.extra.median_center.expect("median center requested");

        for center in [mean, median] {
            assert!(
                center[0] >= 0.0 && center[0] <= image.width as f32,
                "center x {center:?} must lie within the image width {}",
                image.width
            );
            assert!(
                center[1] >= 0.0 && center[1] <= image.height as f32,
                "center y {center:?} must lie within the image height {}",
                image.height
            );
        }

        let (min_x, min_y, max_x, max_y) = alpha_bbox(&image).expect("inked content");
        let box_cx = (min_x + max_x) * 0.5;
        let box_cy = (min_y + max_y) * 0.5;
        let tol = params.font_size_px; // loose: within one em of the ink center
        for center in [mean, median] {
            assert!(
                (center[0] - box_cx).abs() <= tol,
                "center x {center:?} should be near the ink center x {box_cx} (tol {tol})"
            );
            assert!(
                (center[1] - box_cy).abs() <= tol,
                "center y {center:?} should be near the ink center y {box_cy} (tol {tol})"
            );
        }
    }

    /// Renders `text` twice — hanging punctuation off, then on — and returns both
    /// mean centers. Shared by the horizontal and formula exclusion tests.
    fn mean_centers_without_and_with_hanging(
        text: &str,
        layout_mode: TextLayoutMode,
    ) -> ([f32; 2], [f32; 2], usize) {
        let mut params = base_params();
        params.text = text.to_string();
        params.text_layout_mode = layout_mode;
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: false,
        };

        params.hanging_punctuation = 0.0;
        let off = render_text_to_image(&params, None).expect("render should succeed");
        params.hanging_punctuation = 1.0;
        let on = render_text_to_image(&params, None).expect("render should succeed");

        assert_eq!(
            [off.width, off.height],
            [on.width, on.height],
            "the fixture must not change the image size between the two renders, \
             so any center delta is the exclusion and not a different layout"
        );
        (
            off.extra.mean_center.expect("mean requested"),
            on.extra.mean_center.expect("mean requested"),
            off.width as usize,
        )
    }

    #[test]
    fn formula_layout_excludes_hanging_punctuation_from_the_centers() {
        // The formula path hangs punctuation through the same horizontal wrap the
        // Normal path uses, so it must exclude it from the center sampling too —
        // it used to sample every glyph, letting a trailing "?!" drag the center
        // right while the pixels of that punctuation hung outside the block.
        let (off, on, _) = mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Formula);
        assert!(
            on[0] < off[0] - 1.0,
            "excluding the trailing punctuation must pull the mean center LEFT: \
             off={off:?} on={on:?}"
        );
    }

    #[test]
    fn formula_and_normal_agree_on_what_hangs() {
        // Same text, same wrap: both layout modes must move their center in the same
        // direction and by a comparable amount when the exclusion kicks in. This is
        // the regression guard against one path being fixed and the other forgotten.
        let (normal_off, normal_on, width) =
            mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Normal);
        let (formula_off, formula_on, _) =
            mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Formula);
        let normal_shift = normal_off[0] - normal_on[0];
        let formula_shift = formula_off[0] - formula_on[0];
        assert!(normal_shift > 1.0 && formula_shift > 1.0);
        // Loose: the two paths place glyphs differently, so only the magnitude class
        // is comparable, not the exact value.
        let tol = width as f32 * 0.25;
        assert!(
            (normal_shift - formula_shift).abs() <= tol,
            "the two paths must exclude the same glyphs: \
             normal_shift={normal_shift} formula_shift={formula_shift} tol={tol}"
        );
    }

    #[test]
    fn vertical_layout_keeps_punctuation_in_the_centers() {
        // Vertical text never hangs punctuation (its wrap has no such flag), so there
        // is nothing hanging to exclude and the centers must be identical either way.
        // Guards against "fix it everywhere" symmetry that would silently move the
        // vertical center for a setting that mode does not implement.
        let mut params = base_params();
        params.text = "Aa?!".to_string();
        params.text_line_mode = TextLineMode::Vertical;
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        params.hanging_punctuation = 0.0;
        let off = render_text_to_image(&params, None).expect("render should succeed");
        params.hanging_punctuation = 1.0;
        let on = render_text_to_image(&params, None).expect("render should succeed");
        assert_eq!(off.extra, on.extra, "vertical centers must not react");
    }

    #[test]
    fn extra_info_default_request_yields_default_extra() {
        // No request -> byte-identical fast path and a default (all-None) payload.
        let params = base_params();
        assert!(!params.extra_info.is_active());
        let image = render_text_to_image(&params, None).expect("render should succeed");
        assert_eq!(image.extra, crate::types::RenderedTextExtraInfo::default());
    }

    #[test]
    fn extra_info_tracks_glyphs_through_canvas_growing_effect() {
        // A stroke grows the canvas symmetrically around the glyph, then trim crops
        // back. The effects+trim seams must shift the extra centers so they keep
        // pointing at the glyph: the mean center stays near the (symmetric) alpha
        // box center. Without the shift it would be stale by ~the stroke pad.
        let mut plain = base_params();
        plain.text = "O".to_string();
        plain.align = HorizontalAlign::LEFT;
        plain.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: false,
        };
        let plain_image = render_text_to_image(&plain, None).expect("plain render");
        let plain_mean = plain_image.extra.mean_center.expect("mean requested");
        let (pmin_x, pmin_y, pmax_x, pmax_y) = alpha_bbox(&plain_image).expect("plain ink");
        let plain_off = [
            plain_mean[0] - (pmin_x + pmax_x) * 0.5,
            plain_mean[1] - (pmin_y + pmax_y) * 0.5,
        ];

        let mut stroked = plain.clone();
        stroked.effects_json = r#"[{"effect":"stroke","width_px":8,"color":[0,0,0,255]}]"#.to_string();
        let stroked_image = render_text_to_image(&stroked, None).expect("stroked render");
        let stroked_mean = stroked_image.extra.mean_center.expect("mean requested");
        let (smin_x, smin_y, smax_x, smax_y) = alpha_bbox(&stroked_image).expect("stroked ink");
        let stroked_off = [
            stroked_mean[0] - (smin_x + smax_x) * 0.5,
            stroked_mean[1] - (smin_y + smax_y) * 0.5,
        ];

        // The stroke is symmetric, so the glyph center relative to the alpha box
        // center is stable across both renders (a stale, unshifted center would be
        // off by roughly the 8px stroke pad).
        assert!(
            (plain_off[0] - stroked_off[0]).abs() <= 2.0,
            "mean-center offset drifted in x: plain {plain_off:?} vs stroked {stroked_off:?}"
        );
        assert!(
            (plain_off[1] - stroked_off[1]).abs() <= 2.0,
            "mean-center offset drifted in y: plain {plain_off:?} vs stroked {stroked_off:?}"
        );
    }

    #[test]
    fn extra_info_excludes_trailing_hanging_punctuation_when_enabled() {
        // Left-aligned text with a trailing '.' draws identically whether hanging
        // punctuation is on or off (only leading hang shifts layout), so the only
        // difference is the extra-info sampling: with hanging on, the trailing '.'
        // contributes nothing and both centers move LEFT.
        let mut off = base_params();
        off.text = "Hi.".to_string();
        off.align = HorizontalAlign::LEFT;
        off.hanging_punctuation = 0.0;
        off.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let mut on = off.clone();
        on.hanging_punctuation = 1.0;

        let off_image = render_text_to_image(&off, None).expect("hanging-off render");
        let on_image = render_text_to_image(&on, None).expect("hanging-on render");

        let off_mean = off_image.extra.mean_center.expect("mean off");
        let on_mean = on_image.extra.mean_center.expect("mean on");
        assert!(
            on_mean[0] < off_mean[0] - 0.5,
            "excluding the trailing '.' must move the mean center left: on {on_mean:?} vs off {off_mean:?}"
        );

        let off_median = off_image.extra.median_center.expect("median off");
        let on_median = on_image.extra.median_center.expect("median on");
        assert_ne!(
            on_median, off_median,
            "excluding the trailing '.' must change the median center"
        );
    }

    /// Assert both requested centers are present and land inside the image plane.
    fn assert_centers_in_bounds(image: &RenderedTextImage) {
        let mean = image.extra.mean_center.expect("mean center requested");
        let median = image.extra.median_center.expect("median center requested");
        for center in [mean, median] {
            assert!(
                center[0] >= 0.0 && center[0] <= image.width as f32,
                "center x {center:?} must lie within width {}",
                image.width
            );
            assert!(
                center[1] >= 0.0 && center[1] <= image.height as f32,
                "center y {center:?} must lie within height {}",
                image.height
            );
        }
    }

    #[test]
    fn extra_info_populated_on_formula_path() {
        // Formula mode routes through `formula::render` (retry loop). The extras
        // must be populated (both centers Some) and land inside the rendered image.
        let mut params = base_params();
        params.text = "FORMULA".to_string();
        params.width_px = 320;
        params.text_layout_mode = TextLayoutMode::Formula;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "sin(t * tau) * 20".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            offset_y_px: 40.0,
            ..TextFormulaLayoutParams::default()
        };
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let image = render_text_to_image(&params, None).expect("formula render");
        assert_centers_in_bounds(&image);

        // No request -> default (all-None) payload on the same path.
        let mut plain = params.clone();
        plain.extra_info = RenderExtraInfoRequest::default();
        let plain_image = render_text_to_image(&plain, None).expect("formula plain render");
        assert_eq!(
            plain_image.extra,
            crate::types::RenderedTextExtraInfo::default()
        );
    }

    #[test]
    fn extra_info_populated_on_vector_lines_path() {
        // Custom vector lines route through `formula::render`'s drawn-lines path
        // (fixed canvas). Extras must be populated and inside the fixed canvas.
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
                    TextVectorPoint { x: 120.0, y: 40.0 },
                    TextVectorPoint { x: 240.0, y: 40.0 },
                ],
                corner_smoothing_px: 16.0,
                text_direction: TextVectorLineTextDirection::LeftToRight,
                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                flip_text: false,
            }],
            ..TextVectorLinesLayoutParams::default()
        };
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let image = render_text_to_image(&params, None).expect("vector-lines render");
        assert_centers_in_bounds(&image);
    }

    #[test]
    fn extra_info_centers_stay_in_bounds_under_formula_rotation() {
        // A 90° global rotation grows the formula canvas to the rotated bounds; the
        // extras, sampled post-rotation, must still lie inside the rotated image.
        let mut params = base_params();
        params.text = "FORMULA".to_string();
        params.width_px = 320;
        params.text_layout_mode = TextLayoutMode::Formula;
        params.formula_layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            rotation_expr: "0".to_string(),
            use_tangent_rotation: false,
            ..TextFormulaLayoutParams::default()
        };
        params.global_rotation_deg = 90.0;
        params.extra_info = RenderExtraInfoRequest {
            mean_center: true,
            median_center: true,
        };
        let image = render_text_to_image(&params, None).expect("formula rotated render");
        assert_centers_in_bounds(&image);
    }

    /// REGRESSION (blocker): a REAL italic request — `force_italic` WITHOUT an
    /// explicit faux slant — on a font that ships no italic face must render,
    /// not panic.
    ///
    /// Before the guard this reached cosmic-text with `Style::Italic` in the
    /// attrs. `Style` is a HARD `Attrs::matches` filter
    /// (`cosmic-text-0.14.2/src/attrs.rs:322-327`) and the whole fallback
    /// iteration is built from that filtered set, so the upright-only fixture and
    /// the upright-only bundled base left it EMPTY and
    /// `shape.rs:274 .expect("no default font found")` panicked. (With the
    /// shipped `fonts/ui/ext/12-NotoEmoji-Regular.ttf` present the set is not
    /// empty — the emoji exemption keeps that one face in — and the failure mode
    /// is a full-run `.notdef` tofu render instead; see
    /// `font_registry::the_emoji_exemption_makes_a_database_wide_match_check_useless`.)
    /// A panic here is doubly bad: `with_leased_font_system` has no Drop guard,
    /// so it also leaks the leased `FontSystem` out of the pool.
    #[test]
    fn real_italic_without_an_italic_face_degrades_to_faux_instead_of_panicking() {
        let mut params = base_params();
        params.text = "Hi".to_string();
        params.font_size_px = 64.0;
        params.force_italic = true;
        assert!(
            params.faux_italic_slant_deg.is_none(),
            "this test only means something for a REAL italic request"
        );

        let degraded = render_text_to_image(&params, None)
            .expect("a real italic request must never panic the renderer");
        assert!(
            degraded.width > 0 && degraded.height > 0,
            "the degraded render must still produce pixels"
        );

        // The degradation is the documented faux italic at the default slant,
        // not "silently upright" and not some other angle.
        let mut explicit_faux = params.clone();
        explicit_faux.faux_italic_slant_deg = Some(SYNTHESIZED_ITALIC_SLANT_DEG);
        let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
        assert_eq!(
            (degraded.width, degraded.height, &degraded.rgba),
            (explicit.width, explicit.height, &explicit.rgba),
            "the degraded render must equal an explicit faux italic at the default slant"
        );

        let mut upright = params.clone();
        upright.force_italic = false;
        let upright = render_text_to_image(&upright, None).expect("upright render");
        assert_ne!(
            (degraded.width, degraded.height, &degraded.rgba),
            (upright.width, upright.height, &upright.rgba),
            "the degradation must actually slant the text, not silently drop the italic"
        );

        // Degradation must not be silent (CLAUDE.md: no silent fallback for an
        // unsupported request).
        assert!(
            degraded
                .warnings
                .iter()
                .any(|warning| warning.contains("курсив")),
            "the degraded render must warn the user, got {:?}",
            degraded.warnings
        );
    }

    /// The same guard per inline span: a bare `<i>` is a REAL italic request and
    /// must not panic (or silently leave the selected font) when the resolved
    /// family ships no italic face.
    #[test]
    fn real_inline_italic_without_an_italic_face_degrades_to_faux() {
        let mut params = base_params();
        params.text = "a<i>b</i>c".to_string();
        params.font_size_px = 64.0;
        params.enable_inline_style_tags = true;

        let degraded = render_text_to_image(&params, None)
            .expect("a real inline italic must never panic the renderer");
        assert!(degraded.width > 0 && degraded.height > 0);
        assert!(
            degraded
                .warnings
                .iter()
                .any(|warning| warning.contains("<i>")),
            "the degraded inline render must warn the user, got {:?}",
            degraded.warnings
        );

        // An explicit `<i=12>` span is the faux path the degradation lands on.
        let mut explicit_faux = params.clone();
        explicit_faux.text = format!("a<i={SYNTHESIZED_ITALIC_SLANT_DEG}>b</i>c");
        let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
        assert_eq!(
            (degraded.width, degraded.height, &degraded.rgba),
            (explicit.width, explicit.height, &explicit.rgba),
            "a degraded <i> span must equal an explicit faux <i=slant> span"
        );
    }

    /// A REAL bold request — `force_bold` WITHOUT explicit faux params — on a family
    /// that ships no Bold file must be degraded to faux bold, not sent to the shaper.
    ///
    /// Sending it costs twice, both silently: cosmic-text's primary face pick needs an
    /// EXACT weight match and does not rank down inside the family, so the whole run
    /// jumps to whatever family DOES have a 700 face (the bundled `Noto Sans Bold`);
    /// and the script/common fallback passes admit only `weight_diff == 0` candidates,
    /// so every bundled fallback font — all of them 400 — becomes unreachable and rare
    /// glyphs in the same run render as tofu.
    #[test]
    fn real_bold_without_a_bold_face_degrades_to_faux() {
        let mut params = base_params();
        params.text = "Hi".to_string();
        params.font_size_px = 64.0;
        params.force_bold = true;
        assert!(
            params.faux_bold.is_none(),
            "this test only means something for a REAL bold request"
        );

        let degraded =
            render_text_to_image(&params, None).expect("a real bold request must still render");
        assert!(degraded.width > 0 && degraded.height > 0);

        // The degradation is the documented `<b=default>` strength, not "silently
        // regular" and not some other thickness.
        let mut explicit_faux = params.clone();
        explicit_faux.faux_bold = Some(FauxBoldParams::default());
        let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
        assert_eq!(
            (degraded.width, degraded.height, &degraded.rgba),
            (explicit.width, explicit.height, &explicit.rgba),
            "the degraded render must equal an explicit faux bold at the default strength"
        );

        let mut plain = params.clone();
        plain.force_bold = false;
        let plain = render_text_to_image(&plain, None).expect("plain render");
        assert_ne!(
            (degraded.width, degraded.height, &degraded.rgba),
            (plain.width, plain.height, &plain.rgba),
            "the degradation must actually thicken the text, not silently drop the bold"
        );

        assert!(
            degraded
                .warnings
                .iter()
                .any(|warning| warning.contains("жирного начертания")),
            "the degraded render must warn the user, got {:?}",
            degraded.warnings
        );
    }

    /// The same guard per inline span: a bare `<b>` is a REAL bold request and must
    /// not leave the selected font when its family ships no bold face.
    #[test]
    fn real_inline_bold_without_a_bold_face_degrades_to_faux() {
        let mut params = base_params();
        params.text = "a<b>b</b>c".to_string();
        params.font_size_px = 64.0;
        params.enable_inline_style_tags = true;

        let degraded =
            render_text_to_image(&params, None).expect("a real inline bold must still render");
        assert!(degraded.width > 0 && degraded.height > 0);
        assert!(
            degraded
                .warnings
                .iter()
                .any(|warning| warning.contains("<b>")),
            "the degraded inline render must warn the user, got {:?}",
            degraded.warnings
        );

        // An explicit `<b=3>` span is the faux path the degradation lands on (3 % is
        // `FauxBoldParams::default().thicken_percent`).
        let mut explicit_faux = params.clone();
        explicit_faux.text = format!(
            "a<b={}>b</b>c",
            FauxBoldParams::default().thicken_percent as u32
        );
        let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
        assert_eq!(
            (degraded.width, degraded.height, &degraded.rgba),
            (explicit.width, explicit.height, &explicit.rgba),
            "a degraded <b> span must equal an explicit faux <b=thicken> span"
        );
    }





    /// A custom pair must MOVE the pen, and must do so under `KerningMode::Auto`
    /// — the mode whose whole point is "use the font's own pair kerning". If the
    /// override did not win there, an authored pair would be invisible in the
    /// default mode.
    #[test]
    fn a_custom_pair_overrides_the_font_under_auto_kerning() {
        let mut params = base_params();
        params.text = "AV".to_string();
        params.font_size_px = 100.0;
        params.kerning_mode = KerningMode::Auto;

        let (plain_xs, plain_width) = fixture_run_layout("AV", &params);
        // -100 per mille of a 100 px em is a flat -10 px on the A->V step.
        let (kerned_xs, kerned_width) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -100.0)]);
        assert_eq!(plain_xs.len(), 2, "the fixture must shape 'AV' as two glyphs");
        assert_eq!(kerned_xs.len(), 2);
        assert!(
            (kerned_xs[0] - plain_xs[0]).abs() < 1e-3,
            "the first glyph of the run never moves: {kerned_xs:?} vs {plain_xs:?}"
        );
        assert!(
            kerned_xs[1] < plain_xs[1],
            "a -100 per mille pair must pull 'V' left; got {kerned_xs:?} vs {plain_xs:?}"
        );
        assert!(
            kerned_width < plain_width,
            "the logical width must shrink with the pen: {kerned_width} vs {plain_width}"
        );

        // The step is the LEFT glyph's own (un-kerned) advance PLUS the authored
        // delta — the pair REPLACES the font's value rather than adding to it. So
        // the authored offsets must map onto the pen linearly, in exact px: at em
        // 100 the three entries -100 / 0 / +100 per mille sit 10 px apart, anchored
        // on the un-kerned advance. `plain_xs[1]` is NOT that anchor — Liberation
        // Sans kerns `AV` itself, which is precisely the value being replaced.
        let (neutral_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 0.0)]);
        let (widened_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 100.0)]);
        let tightened_step = kerned_xs[1] - kerned_xs[0];
        let neutral_step = neutral_xs[1] - neutral_xs[0];
        let widened_step = widened_xs[1] - widened_xs[0];
        assert!(
            (neutral_step - tightened_step - 10.0).abs() < 1e-2
                && (widened_step - neutral_step - 10.0).abs() < 1e-2,
            "-100 / 0 / +100 per mille must sit exactly 10 px apart at em 100; got \
             {tightened_step}, {neutral_step}, {widened_step}"
        );
        assert!(
            neutral_step > plain_xs[1] - plain_xs[0],
            "the fixture must actually kern 'AV' itself, otherwise this test proves \
             nothing about REPLACING that value: un-kerned {neutral_step} vs shaped {}",
            plain_xs[1] - plain_xs[0]
        );
    }

    /// The override replaces the font's pair value under EVERY mode, so `Fixed`
    /// (which already drops font pair kerning) and `Auto` must land on the SAME
    /// pen for an overridden pair. That equality is the whole "indistinguishable
    /// from a built-in pair" contract.
    #[test]
    fn an_overridden_pair_lands_identically_under_auto_and_fixed() {
        let mut params = base_params();
        params.text = "AV".to_string();
        params.font_size_px = 100.0;

        params.kerning_mode = KerningMode::Auto;
        let (auto_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
        params.kerning_mode = KerningMode::Fixed;
        let (fixed_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
        assert_eq!(auto_xs.len(), fixed_xs.len());
        for (auto_x, fixed_x) in auto_xs.iter().zip(fixed_xs.iter()) {
            assert!(
                (auto_x - fixed_x).abs() < 1e-3,
                "an overridden pair must be mode-independent: {auto_xs:?} vs {fixed_xs:?}"
            );
        }
    }

    /// A pair the table does not list must leave layout BYTE-identical: the
    /// presence of a table may not perturb text it says nothing about.
    #[test]
    fn an_unlisted_pair_leaves_the_pen_untouched() {
        let mut params = base_params();
        params.text = "AV".to_string();
        params.font_size_px = 100.0;
        params.kerning_mode = KerningMode::Auto;

        let (plain_xs, plain_width) = fixture_run_layout("AV", &params);
        // The table is non-empty (so the fast path is off) but mentions another pair.
        let (other_xs, other_width) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('Q', 'z', -200.0)]);
        assert_eq!(plain_xs, other_xs, "an unlisted pair must not move the pen");
        assert!((plain_width - other_width).abs() < 1e-3);
    }

    /// A `0.0` entry is an instruction, not a no-op: it CANCELS the font's own
    /// kerning for that pair, so the pen must land on the un-kerned advance —
    /// i.e. exactly where `KerningMode::Fixed` puts it without any override.
    #[test]
    fn a_zero_entry_cancels_the_fonts_own_pair_kerning() {
        let mut params = base_params();
        params.text = "AV".to_string();
        params.font_size_px = 100.0;

        params.kerning_mode = KerningMode::Fixed;
        let (unkerned_xs, _) = fixture_run_layout("AV", &params);
        params.kerning_mode = KerningMode::Auto;
        let (cancelled_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 0.0)]);
        assert_eq!(unkerned_xs.len(), cancelled_xs.len());
        for (unkerned_x, cancelled_x) in unkerned_xs.iter().zip(cancelled_xs.iter()) {
            assert!(
                (unkerned_x - cancelled_x).abs() < 1e-3,
                "a 0.0 entry must reproduce the un-kerned advance: {cancelled_xs:?} vs \
                 {unkerned_xs:?}"
            );
        }
    }

    /// An override must follow the RUN's direction. cosmic-text lays a right-to-left
    /// run out by subtracting each advance, so its pen steps LEFT while every input
    /// the override path starts from (`hmtx` advances) is positive. Before
    /// `custom_pair_step_px` the overridden pair stepped the pen RIGHT — the glyphs
    /// of an RTL word crossed over each other — and a negative authored delta
    /// WIDENED the pair instead of tightening it.
    #[test]
    fn an_overridden_pair_follows_an_rtl_run_leftwards() {
        // Hebrew alef + bet: the fixture (Liberation Sans) covers both, so the pair
        // is same-face and single-cluster on each side, i.e. genuinely overridable.
        const RTL_PAIR: &str = "\u{05D0}\u{05D1}";
        let mut params = base_params();
        params.text = RTL_PAIR.to_string();
        params.font_size_px = 100.0;
        params.kerning_mode = KerningMode::Auto;

        let (plain_xs, _) = fixture_run_layout(RTL_PAIR, &params);
        assert_eq!(plain_xs.len(), 2, "the fixture must shape the pair as two glyphs");
        assert!(
            plain_xs[1] < plain_xs[0],
            "precondition: cosmic-text must lay this run out right-to-left; got {plain_xs:?}"
        );

        let pairs = [('\u{05D0}', '\u{05D1}', -100.0)];
        let (kerned_xs, _) = fixture_run_layout_with_custom_kerning(RTL_PAIR, &params, &pairs);
        assert_eq!(kerned_xs.len(), 2);
        assert!(
            kerned_xs[1] < kerned_xs[0],
            "an overridden RTL pair must keep walking the pen LEFT, not reverse it; \
             got {kerned_xs:?}"
        );

        // Direction aside, the magnitudes must behave exactly as in an LTR run: the
        // three authored offsets -100 / 0 / +100 per mille sit 10 px apart at em 100,
        // and a NEGATIVE offset must TIGHTEN the pair (shrink the distance) in this
        // direction too.
        let (neutral_xs, _) = fixture_run_layout_with_custom_kerning(
            RTL_PAIR,
            &params,
            &[('\u{05D0}', '\u{05D1}', 0.0)],
        );
        let (widened_xs, _) = fixture_run_layout_with_custom_kerning(
            RTL_PAIR,
            &params,
            &[('\u{05D0}', '\u{05D1}', 100.0)],
        );
        let tightened_gap = (kerned_xs[1] - kerned_xs[0]).abs();
        let neutral_gap = (neutral_xs[1] - neutral_xs[0]).abs();
        let widened_gap = (widened_xs[1] - widened_xs[0]).abs();
        assert!(
            tightened_gap < neutral_gap && neutral_gap < widened_gap,
            "a negative authored delta must TIGHTEN an RTL pair: {tightened_gap} / \
             {neutral_gap} / {widened_gap}"
        );
        assert!(
            (neutral_gap - tightened_gap - 10.0).abs() < 1e-2
                && (widened_gap - neutral_gap - 10.0).abs() < 1e-2,
            "-100 / 0 / +100 per mille must sit exactly 10 px apart at em 100; got \
             {tightened_gap}, {neutral_gap}, {widened_gap}"
        );
    }

    /// `Optical` re-spaces pairs by measured ink, which would otherwise overwrite the
    /// one distance the user dictated. An overridden pair must therefore land on the
    /// SAME pen under `Optical` as under `Auto`/`Fixed` — the third leg of the "every
    /// mode" contract, which the other tests do not exercise.
    #[test]
    fn an_overridden_pair_lands_identically_under_optical() {
        let mut params = base_params();
        params.text = "AV".to_string();
        params.font_size_px = 100.0;

        params.kerning_mode = KerningMode::Auto;
        let (auto_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
        params.kerning_mode = KerningMode::Optical;
        let (optical_xs, _) =
            fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
        assert_eq!(auto_xs.len(), optical_xs.len());
        for (auto_x, optical_x) in auto_xs.iter().zip(optical_xs.iter()) {
            assert!(
                (auto_x - optical_x).abs() < 1e-3,
                "an overridden pair must be mode-independent: {auto_xs:?} vs {optical_xs:?}"
            );
        }

        // Control: without the override `Optical` really does move this pair, so the
        // equality above proves the override wins rather than that the mode is inert.
        let (optical_plain_xs, _) = fixture_run_layout("AV", &params);
        assert!(
            (optical_plain_xs[1] - optical_xs[1]).abs() > 1e-2,
            "the optical pass must actually move 'AV' when it is not overridden: \
             {optical_plain_xs:?} vs {optical_xs:?}"
        );
    }

    /// The fast path must be unreachable for a font with overrides, otherwise the
    /// run would keep the shaped positions and the overrides would silently do
    /// nothing. `Auto` with zero tracking is the ONLY configuration that qualifies
    /// without them, so it is the only one that can regress.
    #[test]
    fn a_custom_table_takes_the_run_off_the_default_metric_fast_path() {
        let params = base_params();
        let settings = KerningSettings::from_params(&params);
        assert_eq!(params.kerning_mode, KerningMode::Auto);
        assert!(
            settings.uses_default_metric_layout(),
            "Auto with zero tracking must still qualify without overrides"
        );
        assert!(
            !settings.with_custom_pairs(true).uses_default_metric_layout(),
            "a font with overrides must never take the shaped-position shortcut"
        );
        assert!(
            settings.with_custom_pairs(false).uses_default_metric_layout(),
            "clearing the flag must restore the fast path exactly"
        );
    }

    /// The cluster guard: a pair is only authored for two single characters, so a
    /// glyph whose source cluster is not exactly one `char` can never match one.
    #[test]
    fn only_single_char_clusters_can_carry_an_authored_pair() {
        use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

        let bytes = std::fs::read(test_font_path()).expect("fixture font bytes");
        let mut db = fontdb::Database::new();
        db.load_font_data(bytes);
        let family = db
            .faces()
            .next()
            .and_then(|face| face.families.first().cloned())
            .map(|(name, _language)| name)
            .expect("fixture family name");
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(40.0, 48.0));
        buffer.set_size(&mut font_system, None, None);
        let attrs = Attrs::new()
            .family(Family::Name(family.as_str()))
            .metrics(Metrics::new(40.0, 40.0));
        buffer.set_text(&mut font_system, "AV", &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);
        let run = buffer.layout_runs().next().expect("one layout run");
        let first = run.glyphs.first().expect("a shaped glyph");
        assert_eq!(
            single_char_cluster(run.text, first),
            Some('A'),
            "a one-character cluster must resolve to that character"
        );
        // Out-of-range byte offsets (which a caller must never produce, but which
        // must not panic either) degrade to "no override".
        let mut broken = first.clone();
        broken.start = 0;
        broken.end = run.text.len() + 8;
        assert_eq!(single_char_cluster(run.text, &broken), None);
        // A two-character span is a multi-char cluster and is skipped.
        let mut wide = first.clone();
        wide.start = 0;
        wide.end = 2;
        assert_eq!(single_char_cluster(run.text, &wide), None);
    }

}
