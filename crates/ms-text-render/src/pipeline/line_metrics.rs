/*
File: crates/ms-text-render/src/pipeline/line_metrics.rs

Purpose:
Vertical line stacking of the horizontal path: per-line extra spacing, the upward-only
`InlineHeightRoom` an inline `<stretching>` span may add, and the baseline tables
(GLYPH HEIGHT SCALE contract in `MODULE_README.md`).

Key functions:
- compute_line_extra_spacing_table(), line_baseline_advance_table()
- compute_horizontal_line_baselines(), horizontal_run_baseline_y()

Notes:
`InlineHeightRoom` is declared in `pipeline/mod.rs`; its `impl` lives here.
*/

use crate::inline_styles::InlineStyleSpan;
use crate::glyph_blit::{hash_font_id, resolve_outline_for_glyph};
use crate::pair_advance::effective_spacing_percent;
use crate::vector::OutlineCache;
use crate::types::TextRenderParams;
use cosmic_text::{Buffer, FontSystem, LayoutRun};
use super::InlineHeightRoom;
use super::inline_lookup::inline_glyph_scale_for_glyph;
use super::faux::faux_style_for_glyph;

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
