/*
File: crates/ms-text-render/src/pipeline/faux.rs

Purpose:
Faux bold / italic: the per-glyph `FauxGlyphStyle` resolution, the faux advance growth
and bounds pads, and the UNSERVICEABLE-ATTRS degrade of a real italic / bold request the
registered fonts cannot serve (`MODULE_README.md`).

Key items:
- SYNTHESIZED_ITALIC_SLANT_DEG
- synthesized_italic_slant_deg() / synthesized_bold_params(): whole-overlay degrade
- degrade_unavailable_inline_italic() / degrade_unavailable_inline_bold(): inline spans
- faux_style_at_offset() / faux_style_for_glyph(), resolve_faux_counter_flag()
- faux_advance_extra_px_for_glyph(), faux_bounds_pads(), base_attrs_real_bold_italic()

Notes:
`FauxGlyphStyle` is declared in `pipeline/mod.rs`.
*/

use crate::font_registry::{
    InlineFontRegistry, family_has_face_of_requested_weight, family_has_matching_face,
};
use crate::inline_styles::{FauxFaceBaseline, InlineStyleSpan, apply_inline_style_to_attrs};
use crate::glyph_blit::glyph_outline_has_counter;
use crate::vector::{FauxOutlineParams, OutlineCache, faux_key_bits};
use crate::types::{
    FAUX_THICKEN_PERCENT_MAX, FAUX_THICKEN_PERCENT_MIN, FauxBoldParams, TextRenderParams,
};
use cosmic_text::{Attrs, FontSystem, LayoutGlyph};
use super::FauxGlyphStyle;
use super::inline_lookup::inline_style_at_offset;

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
pub(super) fn synthesized_italic_slant_deg(
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
pub(super) fn degrade_unavailable_inline_italic(
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
pub(super) fn synthesized_bold_params(
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
pub(super) fn degrade_unavailable_inline_bold(
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
pub(super) fn faux_advance_extra_px_for_glyph(
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
