/*
File: crates/ms-text-render/src/pipeline/mod.rs

Purpose:
Staged pipeline нового рендера после переноса horizontal foundation, vertical path и formula path.

Main responsibilities:
- давать изолированную входную точку нового рендера без переключения текущего продового пути;
- рендерить staged horizontal/vertical text через `cosmic-text`, включая attrs-level rich text;
- маршрутизировать formula/shape layout в отдельный `formula::render` path;
- подключать post-effects через отдельный `effects` пакет после базового растра;
- предформировать horizontal wrap/shape и vertical columns через отдельный `wrap`-слой;
- использовать вынесенные `font_registry` и `raster` как фундамент для следующих этапов.

This file keeps the orchestration (`render_text_to_image`, `apply_effects_to_image`,
`smoke_render_text_to_image`, the wrap prepass) and every type the children share
(`GlyphScaleSettings`, `GlyphCaches`, `GlyphStyle`, `RunPen`, `RunShaping`,
`HorizontalRunLayout`, `PenRunWidth`, the placement structs, `FauxGlyphStyle`,
`InlineHeightRoom`). Children (map: `MODULE_README.md` in this directory):
- `run_layout.rs` — the horizontal pen (`horizontal_run_layout`, optical re-spacing);
- `horizontal_draw.rs` / `rotated.rs` — the unrotated and inline-rotated glyph passes;
- `text_prep.rs` — source-text preparation and line alignment;
- `inline_lookup.rs` — the per-glyph inline lookups (one owner);
- `faux.rs` — faux bold/italic and the unserviceable-attrs degrade;
- `line_metrics.rs` — line spacing, `InlineHeightRoom`, baseline tables;
- `hyphen.rs` — the wrap hyphen (one owner); `hanging.rs` — hanging punctuation.
Items other modules import keep their `pipeline::` path through `pub(crate) use`.

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
  `MODULE_README.md`. The pair DECISION (override guard and magnitude, `Fixed` step,
  run-axis orientation, tracking basis) is owned by `pair_advance`; the pen loops
  here only accumulate its `PairStep`s plus tracking, faux growth and the optical
  normalization;
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

mod horizontal_draw;
mod rotated;
mod run_layout;
mod text_prep;
mod inline_lookup;
mod faux;
mod line_metrics;
mod hyphen;
mod hanging;
#[cfg(test)]
mod tests;

use ms_log::runtime_log;
use ms_log::trace::cat;
use super::effects::{apply_effects_pipeline, apply_text_preprocess_effects};
use super::extra_info::ExtraInfoAccumulator;
use super::fallback_diag::collect_font_fallback_report;
use super::font_provider::FontProvider;
use super::font_registry::{
    CustomKerningMap, InlineFontRegistry, build_inline_font_registry, load_font_content,
};
use super::font_ligature_patch::EllipsisLigatureMode;
use super::font_system_pool::with_leased_font_system;
use super::formula::{
    FormulaRenderOutcome, FormulaRenderRequest, render_text_with_drawn_lines_layout,
    render_text_with_formula_layout, render_text_with_vector_lines_layout,
};
use super::inline_styles::{
    FauxFaceBaseline, InlineStyleSpan, apply_inline_style_to_attrs,
    collect_requested_inline_font_labels, parse_inline_style_tags, remap_inline_style_spans,
    spans_have_attrs_overrides,
};
use super::layout::{VerticalRasterRequest, render_vertical_text};
use super::pair_advance::effective_spacing_percent;
use super::optical::OpticalContourCache;
use super::raster::{
    PixelBounds, RgbaCanvasView, include_scaled_rect_bounds, is_cancelled,
    trim_rendered_image_to_alpha_bounds,
};
use super::vector::{
    FauxOutlineParams, MeshWarpContext, Outline, OutlineCache, RasterScratch, build_aa_lut,
};
use super::types::{
    FontFallbackReport, RenderedTextExtraInfo, RenderedTextImage, TextLayoutMode, TextLineMode,
    TextRenderParams, TextRenderShapeCompareParams, TextWrapMode,
};
use super::wrap::{
    HyphenationDictionaries, LayoutTextResult, ShapeWrapRequest, VerticalWrapRequest,
    build_vertical_layout_text, needs_hyphenation_dicts, reshape_text_for_shape,
    should_prehyphenate_overlong, word_break_policy,
};
use ms_text_util::language::text_language;
use ms_text_util::segmentation::with_default_segmenter;
use cosmic_text::{
    Attrs, Buffer, FontSystem, LayoutRun, Metrics, Shaping, SwashCache, SwashContent, Wrap,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use horizontal_draw::{
    build_horizontal_placement, draw_horizontal_placement, horizontal_placement_extra_samples,
};
use rotated::render_horizontal_rotated;
use run_layout::horizontal_run_layout;
use text_prep::{apply_line_aligns_to_buffer, justify_alignment_option, prepare_source_text};
pub(crate) use text_prep::{
    compute_inline_line_aligns, compute_layout_line_offsets, horizontal_line_offset,
};
use inline_lookup::spans_have_inline_rotation;
pub(crate) use inline_lookup::{
    inline_glyph_offset_at_offset, inline_glyph_offset_for_glyph,
    inline_glyph_offset_span_at_offset, inline_glyph_offset_span_for_glyph,
    inline_glyph_offset_style_at_offset, inline_glyph_offset_style_for_glyph,
    inline_glyph_scale_at_offset, inline_glyph_scale_for_glyph, inline_kerning_at_offset,
    inline_kerning_for_glyph, inline_text_color_at_offset, inline_text_color_for_glyph,
    spans_have_inline_size_overrides,
};
use faux::{
    SYNTHESIZED_ITALIC_SLANT_DEG, base_attrs_real_bold_italic, degrade_unavailable_inline_bold,
    degrade_unavailable_inline_italic, synthesized_bold_params, synthesized_italic_slant_deg,
};
pub(crate) use faux::{
    faux_bounds_pads, faux_style_at_offset, faux_style_for_glyph, resolve_faux_counter_flag,
};
use line_metrics::compute_line_extra_spacing_table;
pub(crate) use line_metrics::{
    compute_horizontal_line_baselines, horizontal_run_baseline_y, line_baseline_advance_table,
};
pub(crate) use hyphen::{
    build_wrapped_hyphen_glyph, run_wraps_at_soft_hyphen, soft_hyphen_style_offset,
    trailing_hyphen_x,
};
pub(crate) use hanging::{hanging_edge_run_bounds, is_edge_run_hanging};

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

/// The per-render glyph caches a draw or measure pass shares: swash bitmaps (the
/// placement box that pins the outline pivot, colour fallbacks), resolution-
/// independent outlines, and the optical ink contours (`KerningMode::Optical`).
///
/// Created together and dropped with the render; each glyph is extracted at most
/// once per cache. Passed as `&mut` beside the `FontSystem`; callers borrow the
/// fields disjointly (a swash image stays borrowed while an outline resolves).
pub(crate) struct GlyphCaches {
    pub(crate) swash: SwashCache,
    pub(crate) outlines: OutlineCache,
    pub(crate) contours: OpticalContourCache,
}

impl GlyphCaches {
    /// Three empty caches.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            swash: SwashCache::new(),
            outlines: OutlineCache::new(),
            contours: OpticalContourCache::new(),
        }
    }
}

/// The geometric per-glyph style an outline is drawn AND measured with: the inline
/// glyph scale and the faux bold/italic style. `Copy`, passed by value on the
/// per-glyph path so the draw and the optical measurement cannot disagree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlyphStyle {
    pub(crate) scale: GlyphScaleSettings,
    pub(crate) faux: FauxGlyphStyle,
}

/// The inline lookups of one shaped buffer as a run walk reads them: the (possibly
/// degraded) params, the remapped inline spans with the byte offset of every layout
/// line, the render's face-keyed user-authored kerning and the block font size.
///
/// Built once per render (the wrap measure builds a span-less one with a single
/// line); `Copy`, it only borrows.
#[derive(Clone, Copy)]
pub(crate) struct RunPen<'a> {
    pub(crate) params: &'a TextRenderParams,
    pub(crate) inline_style_spans: Option<&'a [InlineStyleSpan]>,
    pub(crate) layout_line_offsets: &'a [usize],
    pub(crate) custom_kerning: &'a CustomKerningMap,
    pub(crate) font_size_px: f32,
}

/// [`RunPen`] plus what re-shaping text outside the buffer needs — the synthesized
/// wrap hyphen ([`build_wrapped_hyphen_glyph`]): the base attrs, the selected face's
/// faux fallback weight/style, the inline font registry and the line height of the
/// hyphen's own one-line buffer (`hyphen_line_height_px`; every path passes its
/// shaping metrics' line height). Built once per render; `Copy`, it only borrows.
#[derive(Clone, Copy)]
pub(crate) struct RunShaping<'a> {
    pub(crate) pen: RunPen<'a>,
    pub(crate) attrs: &'a Attrs<'a>,
    pub(crate) faux_face_baseline: FauxFaceBaseline,
    pub(crate) inline_font_registry: &'a InlineFontRegistry,
    pub(crate) hyphen_line_height_px: f32,
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

/// The pen as the wrap's line-width measure: a shaped run is laid out by
/// [`horizontal_run_layout`] with the block's kerning mode, manual tracking and the
/// render's user-authored pairs, and its `line_width_px` is the width the wrap scores —
/// so a line breaks exactly where the drawn line ends.
///
/// Measured with the block-level settings only (faux bold included): the wrap shapes
/// plain candidate lines, so inline `<…>` spans do not reach it. Owns its own
/// per-render outline / contour caches (the `Optical` branch measures ink, faux bold
/// resolves counters), dropped with the wrap.
pub(crate) struct PenRunWidth<'a> {
    params: TextRenderParams,
    custom_kerning: &'a CustomKerningMap,
    font_size_px: f32,
    caches: GlyphCaches,
}

impl<'a> PenRunWidth<'a> {
    /// The pen for `params` over the render's face-keyed overrides `custom_kerning`, at
    /// the block font size `font_size_px`.
    #[must_use]
    pub(crate) fn new(params: &TextRenderParams, custom_kerning: &'a CustomKerningMap, font_size_px: f32) -> Self {
        Self {
            params: params.clone(),
            custom_kerning,
            font_size_px,
            caches: GlyphCaches::new(),
        }
    }
}

impl super::wrap::RunWidthMeasure for PenRunWidth<'_> {
    fn run_width_px(&mut self, font_system: &mut FontSystem, run: &LayoutRun<'_>) -> f32 {
        let pen = RunPen {
            params: &self.params,
            inline_style_spans: None,
            layout_line_offsets: &[0],
            custom_kerning: self.custom_kerning,
            font_size_px: self.font_size_px,
        };
        horizontal_run_layout(pen, run, font_system, &mut self.caches).line_width_px
    }
}

/// The resolved render context the wrap prepass reshapes the source text in: the
/// main layout and the optional compare layout (`compare_shape_with`) differ only
/// in their `LayoutShapeParams`, so both read this one context.
#[derive(Clone, Copy)]
struct ShapeLayoutContext<'a> {
    params: &'a TextRenderParams,
    attrs: &'a Attrs<'a>,
    custom_kerning: &'a CustomKerningMap,
    font_size_px: f32,
    base_line_height_px: f32,
    extra_line_spacing_px: f32,
    preserve_edge_spaces: bool,
}

/// Wrap / shape `source_text` for `shape_params` (horizontal: the scored shape wrap
/// measured by the pen; vertical: the column wrap) and return the layout text plus
/// its warnings.
fn build_layout_text_for_shape_params(
    context: ShapeLayoutContext<'_>,
    font_system: &mut FontSystem,
    source_text: &str,
    shape_params: LayoutShapeParams,
) -> LayoutTextResult {
    let ShapeLayoutContext {
        params,
        attrs,
        custom_kerning,
        font_size_px,
        base_line_height_px,
        extra_line_spacing_px,
        preserve_edge_spaces,
    } = context;
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
            run_width: &mut PenRunWidth::new(params, custom_kerning, font_size_px),
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
    // The wrap measures through this SAME map (selected font only at that point; the
    // wrap shapes with the selected `attrs`), so a line breaks where it is drawn.

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
    let shape_layout_context = ShapeLayoutContext {
        params,
        attrs: &attrs,
        custom_kerning: &custom_kerning,
        font_size_px,
        base_line_height_px,
        extra_line_spacing_px,
        preserve_edge_spaces,
    };
    let layout_text_result = build_layout_text_for_shape_params(
        shape_layout_context,
        font_system,
        source_text,
        layout_shape_params,
    );
    warnings.extend(layout_text_result.warnings);
    let layout_text = layout_text_result.text;
    if let Some(compare_params) = params.compare_shape_with.as_ref() {
        let compare_layout_text = build_layout_text_for_shape_params(
            shape_layout_context,
            font_system,
            source_text,
            LayoutShapeParams::from_compare(compare_params),
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
        compute_layout_line_offsets(layout_text.as_str()).as_slice(),
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
    // The inline-style context every run walk below reads (both horizontal paths).
    let shaping = RunShaping {
        pen: RunPen {
            params,
            inline_style_spans: mapped_inline_style_spans.as_deref(),
            layout_line_offsets: layout_line_offsets.as_slice(),
            custom_kerning: &custom_kerning,
            font_size_px,
        },
        attrs: &attrs,
        faux_face_baseline,
        inline_font_registry: &inline_font_registry_build.registry,
        hyphen_line_height_px: font_size_px,
    };
    let has_global_rotation = params.global_rotation_deg.abs() > f32::EPSILON;
    if has_global_rotation
        || mapped_inline_style_spans
            .as_deref()
            .is_some_and(spans_have_inline_rotation)
    {
        ms_log::trace_log!(cat::RENDER, "render_text path=horizontal_rotated lines={} global_rotation_deg={}", layout_line_offsets.len(), params.global_rotation_deg);
        let mut rendered = render_horizontal_rotated(
            shaping,
            font_system,
            &buffer,
            line_baselines.as_slice(),
            line_height_px,
            cancel,
        )?;
        rendered.warnings.extend(warnings);
        rendered.font_fallbacks = font_fallbacks;
        apply_effects_pipeline(&mut rendered, params.effects_json.as_str(), cancel)?;
        rendered = trim_rendered_image_to_alpha_bounds(rendered, 1);
        return Ok(rendered);
    }

    ms_log::trace_log!(cat::RENDER, "render_text path=horizontal lines={} align={:?}", layout_line_offsets.len(), params.align);
    // Single placement pass: layout and per-glyph placement are computed ONCE per
    // run and reused for both the bounds box and the draw pass. One set of glyph
    // caches (the outline and optical ink-contour caches are no-ops for every
    // non-Optical kerning mode) and one AA LUT serve the whole path —
    // `horizontal_run_layout` and `get_image` run exactly once per glyph.
    let mut caches = GlyphCaches::new();
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
                &mut caches.outlines,
                glyph,
            );
            if let Some(placement) = build_horizontal_placement(
                font_system,
                &mut caches,
                glyph,
                [
                    line_offset_x + (glyph_x - glyph.x) + glyph_offset[0],
                    baseline_y + glyph_offset[1],
                ],
                GlyphStyle {
                    scale: glyph_scale,
                    faux: glyph_faux,
                },
                glyph_text_color,
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
            && let Some(hyphen_glyph) =
                build_wrapped_hyphen_glyph(font_system, shaping, &run, runs.peek())
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
                &mut caches.outlines,
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
                &mut caches,
                &hyphen_glyph,
                [hyphen_offset_x + hyphen_offset[0], baseline_y + hyphen_offset[1]],
                GlyphStyle {
                    scale: hyphen_scale,
                    faux: hyphen_faux,
                },
                hyphen_text_color,
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
            &mut RgbaCanvasView {
                rgba: rgba.as_mut_slice(),
                width: out_width as usize,
                height: out_height as usize,
            },
            placement,
            [x_offset, y_offset],
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

/// The inline rotation of one glyph on the rotated horizontal path: its own turn
/// and, when it belongs to an inline rotation group, the group key and the group's
/// turn (applied later about the group centroid, `apply_rotated_group_rotations`).
#[derive(Debug, Clone, Copy)]
struct InlineGlyphRotation {
    glyph_rad: f32,
    group_key: Option<(usize, usize)>,
    group_rad: f32,
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
