/*
File: crates/ms-text-render/src/formula/render/mod.rs

Purpose:
Formula raster/layout path staged рендера typing.

Main responsibilities:
- рендерить glyph seeds по формульной траектории без зависимости от старого `render.rs`;
- собирать formula-specific glyph metadata, arc-length mapping и rotated bounds/draw;
- отдельно обрабатывать fallback для `TextLayoutMode::Shape`, когда кривая слишком короткая.

This file keeps the three `pub(crate)` entry points, the shared types (seeds, transforms,
`OnPathStepSpacing`, the MinimumPreviousDistance state and caches, `CustomLineLayoutSettings`)
and the tuning constants. Children (map: `MODULE_README.md` in this directory):
- `drawn_lines_layout.rs` — the custom raster / vector line render pass and its alignment
  pre-pass;
- `line_transforms.rs` — per-glyph transforms of one custom line, straight reference walk;
- `seed_ink.rs` — glyph ink on the path, pair measure and clearance floor;
- `path_geometry.rs` — line settings, group rotations, path sampling, gap solvers;
- `formula_layout.rs` — the formula / shape render pass, shape fallback, curve arc length;
- `seeds.rs` — seed collection and `assign_formula_seed_advances` (a CUSTOM KERNING site).

On-path horizontal alignment (`TextRenderParams.align`):
- `on_path_align_fraction` turns the bias into the share of free arc length placed
  before the run; `justify` keeps each path's historical default (custom lines 0.0,
  formula 0.5) because the slider is hidden in the UI while justify is on.
- Custom lines: `drawn_line_start_offsets` gives each line its own start cursor from
  that line's align (inline `<align=...>` included). Free space is never clamped to
  `>= 0`. `drawn_line_drop_side` then drops a glyph past the path end always, and one
  before the path start only when the line's start offset is negative (alignment-induced
  overflow) — a plain inline `<offset>` nudge still clamps onto the first point and draws.
  That pre-pass replays the plain arc-length walk, so it is EXACT only for
  `ByLineLength`. A `MinimumPreviousDistance` line whose alignment actually depends on
  the run length then goes through `refine_min_distance_start_offsets`, which re-walks
  the line (`measure_min_distance_run_len_px`) and recomputes the offset from the run's
  MEASURED length until it settles. Without it a one-sided bend made the real run longer
  than the estimate and an end-aligned line lost its last glyph.
- Formula/shape: `map_formula_target_arc_length` splits the free curve length; the
  overflow branch keeps its compression and ignores alignment.

On-path step between two adjacent glyphs (`OnPathStepSpacing`):
- ONE owner. `OnPathStepSpacing::step_px` is the only place that answers "how far
  along the path is the next glyph". Its three consumers — the alignment pre-pass
  `drawn_line_start_offsets`, the custom-line walk `drawn_line_seed_transform`, and
  the formula accumulator in `render_text_with_formula_layout_once` — each used to
  carry a verbatim copy of the expression, and the copies had already drifted apart.
  Never re-fork them: the pre-pass replays the walk, so a second copy silently
  misaligns every line.
- Two floors, two jobs. The SEED floor guards the glyph's own advance before the
  letter-spacing multiplier scales it; `MIN_ON_PATH_STEP_PX` guards the FINAL step so
  negative additive tracking cannot stall or reverse the arc-length cursor.
- KNOWN DEFECT, preserved deliberately: the seed floor differs per path — `1.0` for
  custom raster/vector lines, `(font_size_px * 0.5).max(1.0)` for formula/shape. On a
  formula curve that pushes every narrow glyph apart to half an em and prevents an
  authored negative kerning pair from tightening a step at all. See
  `OnPathStepSpacing::for_formula`; fixing it also means changing
  `detect_shape_layout_fallback_reason`, which estimates run length with the same
  floor.
- `seed_metric_advance_px` is the separate, EARLIER floor that turns the shaped pen
  delta into a positive magnitude (`assign_formula_seed_advances`). It guards only the
  metric advance, so it does NOT clip user-authored kerning.

User-authored kerning pairs:
`assign_formula_seed_advances` applies them exactly as the horizontal pen loop does —
a matching pair REPLACES the font's own value under every `KerningMode`, stepping by
`nominal_glyph_advance_px(left)` plus the authored delta. Seeds are DETACHED from the
`LayoutRun` they came from, so the source character is captured at seed time as
`FormulaGlyphSeed::cluster_char`; it is `None` for a multi-character cluster and for
the synthesized wrap hyphen, neither of which can carry an authored pair.

Glyph rasterization (formula + custom-line composite pass):
- Each placed glyph is drawn by rasterizing its true font outline
  (`render_next/vector.rs`) directly into the output via
  `glyph_blit::glyph_outline_transform` (the shared single source of truth for the
  outline->world pivot, also used by the horizontal path) + `rasterize_outline_into`.
- COLR/bitmap color glyphs have no monochrome outline (`resolve_glyph_outline`
  returns `None`); those keep the legacy rotated bitmap blit.

Mesh warp (`TextRenderParams.raster_transform`):
Both render functions honor it at the outline seam. Each captures `warp_pre`
(pre-global-rotation content box + centroid = mean of the drawable placement
centers, gated on `raster_transform.is_some()`) BEFORE
`rotate_placements_about_centroid`, then builds `MeshWarpContext` and grows bounds.
Custom VECTOR lines drop their FIXED canvas on a non-identity warp (like a global
rotation) and grow to the warped bounds. `None`/identity is byte-identical; the
color-glyph bitmap fallback is not warped.

Extra render info (`TextRenderParams.extra_info`):
Both `_once` functions build their OWN `ExtraInfoAccumulator` (so the formula retry
loop rebuilds it fresh each iteration) and feed it the final line-placed, rotated
glyph box (shared `extra_info::rotated_box_samples`) for both outline and bitmap
glyphs, warp once via `map_points`, then store `finish`'s centers into the returned
image before the caller trims/effects. Leading/trailing hanging punctuation is
excluded from that sampling (marked per seed in `collect_formula_glyph_seeds`),
so it cannot drag the centers. Default request = no-op.

Glyph-ink spacing (MinimumPreviousDistance mode):
The mode gives every adjacent pair of a line ONE ink-to-ink distance — optical kerning
on the glyph contours, applied along the curve. The font's side bearings are therefore
partly overruled and the run does NOT match the same text set in a straight line; that
is the deliberate, user-requested trade this mode exists to make. Six parts:
- REFERENCE. `straight_reference_transform` places every glyph a second time on a
  virtual straight line (point `(x, 0)`, tangent `(1, 0)`), walked by the nominal
  along-path step (`StraightReferenceWalk`, the single owner of that recurrence, so the
  median pre-pass and the real walk cannot drift). It shares
  `on_path_transform_from_sample` with the real placement, so every other setting
  (normal offset, flip, rotation mode, per-glyph rotation/offset) enters it identically.
- MEASURE. `on_path_pair_gap` = `pair_gap::directional_pair_gap` on the CHORD between
  the pair's ink placement centers (`pair_chord_axis`) — the chord because the measure
  must not depend on which end of the pair it is taken from.
- TARGET. `straight_reference_target_gaps`: the MEDIAN of the line's reference-layout
  gaps, via the shared `optical::median_of_gaps`. One number per LINE, not per pair —
  that is what makes the run uniform. Taken in the reference and never on the curve, so
  moving a path node cannot re-space the line; the per-pair target then goes through
  `optical::optical_delta`, so the sanity clamp and the anti-collision floor are the
  horizontal/vertical optical ones.
- WHICH PAIRS. Both glyphs must have ink, so a space ends the pair chain and an
  inter-word distance is never normalized. A user-authored kerning pair is EXEMPT and
  excluded from the median (same contract as `pipeline/`, where an authored pair
  cancels optical kerning). A pair that does not face itself even when straight keeps
  its arc-length seed.
- SEARCH. `solve_directional_gap_center_s` moves the glyph either way, bounded by its
  advance and by the line's run start. The gap is not monotone along a polyline path, so
  it brackets the FIRST sign change while walking outward from the seed and bisects only
  that bracket: the nearest crossing, i.e. the minimal correction.
- SAFETY NET. `find_clearance_center_s` keeps an omnidirectional `min_placed_distance`
  floor (`pair_clearance_floor`, capped by the reference's own clearance) against the
  last `ON_PATH_INK_NEIGHBOR_WINDOW` inked glyphs, covering the overhang case the
  per-scanline directional metric cannot see.
NOT AN INVARIANT ANY MORE: "a straight line is a no-op". A uniform target necessarily
re-spaces a straight run of mixed side bearings. Only the degenerate case survives — a
line whose pairs already share one gap has that gap as its median and keeps its
arc-length positions.
`seed_ink_geometry`/`CachedGlyphInk` supply the ink: a contour derived from the glyph's
outline (`glyph_contour_from_outline`, cached by cosmic-text `CacheKey`) plus the bitmap
PLACEMENT; `place_seed_ink_for_transform` maps it into a frame with the same
`glyph_outline_transform` pivot the rasterizer uses.

Inline lookups and the wrap hyphen:
- Not implemented here. Every per-glyph inline override (`inline_*_for_glyph` /
  `_at_offset`), the per-line alignment (`compute_inline_line_aligns`) and the synthesized
  wrap hyphen (`build_wrapped_hyphen_glyph`, `soft_hyphen_style_offset`,
  `trailing_hyphen_x`) are imported from `pipeline`, their one owner, over the same
  `RunShaping` context; do not fork them here.

Line stacking:
- Not implemented here. `pipeline::line_baseline_advance_table`,
  `compute_horizontal_line_baselines` and `horizontal_run_baseline_y` are imported,
  so inline `<line-spacing>` and the grow-only `<stretching>` height rule behave
  exactly as on the horizontal path. Verbatim copies of all three used to live in
  this file; do not re-fork them.
- The on-path glyph pivot itself keeps the box-centre anchor
  (`GlyphScaleSettings::scaled_rect` + `drawn_line_glyph_destination_center_raw`):
  a glyph here is placed against the CURVE, and the shared-baseline variant is the
  explicit `LinePlacementReference::LineBox` option, not the default.

Source:
- `render_text_with_formula_layout`
- `render_text_with_formula_layout_once`
- `collect_formula_glyph_seeds`
- `assign_formula_seed_advances`
- rotated-rect и arc-length helper'ы
из старого `src/tabs/typing/render.rs`
*/

mod drawn_lines_layout;
mod line_transforms;
mod seed_ink;
mod path_geometry;
mod formula_layout;
mod seeds;
#[cfg(test)]
mod tests;

use ms_log::trace::cat;
use crate::drawn_lines::{build_vector_line_paths, load_raster_line_paths};
use crate::font_registry::{CustomKerningMap, InlineFontRegistry};
use crate::glyph_blit::resolve_outline_for_glyph;
use crate::glyph_contour::{GlyphContour, PlacedContour};
use crate::vector::{Outline, OutlineCache};
use crate::inline_styles::{FauxFaceBaseline, InlineGlyphOffset, InlineStyleSpan};
use crate::pair_advance::{KerningSettings, PairGlyph, effective_spacing_percent};
use crate::pipeline::{
    FauxGlyphStyle, GlyphScaleSettings, InlineHeightRoom, RunPen, RunShaping,
    compute_horizontal_line_baselines, compute_layout_line_offsets, faux_bounds_pads,
    line_baseline_advance_table,
};
use crate::raster::{RigidPlacement, trim_rendered_image_to_alpha_bounds};
use crate::types::{
    HorizontalAlign, LinePlacementReference, RenderedTextImage, TextLayoutMode, TextRenderParams,
};
use cosmic_text::{Attrs, Buffer, CacheKey, FontSystem, LayoutGlyph, SwashCache};
use std::collections::HashMap;

use drawn_lines_layout::render_text_with_drawn_lines_layout_once;
use formula_layout::{detect_shape_layout_fallback_reason, render_text_with_formula_layout_once};

/// Douglas-Peucker tolerance (glyph-local pixels) applied when simplifying a
/// glyph's outline-derived contour. Small enough that the polygon still hugs the
/// ink, large enough to keep the edge count (and the O(edges^2) distance test)
/// low.
const CONTOUR_SIMPLIFY_TOLERANCE_PX: f32 = 1.5;

/// Omnidirectional clearance (world pixels) `MinimumPreviousDistance` keeps
/// between a glyph and the recently placed ones, as a SAFETY NET only.
///
/// The directional chord gap cannot see a DIAGONAL approach: on a sharp turn
/// two glyphs can close in on each other outside their chord overlap band,
/// where `directional_pair_gap` reports `f32::INFINITY` (correct — they do not
/// face each other — but useless as a collision guard). This floor is the
/// second, unsigned guard for exactly that case.
///
/// It is NEVER applied above what the straight reference layout itself achieves
/// (see [`pair_clearance_floor`]): a pair the font draws tighter than this on a
/// straight line — a tight Cyrillic pair, or a user-authored negative kerning
/// pair — must stay exactly that tight on the curve, otherwise the mode would
/// rewrite the font's typography instead of compensating for the bend.
const ON_PATH_INK_CLEARANCE_FLOOR_PX: f32 = 0.5;

/// How many previously placed INKED glyphs of a line stay in the clearance
/// window of `MinimumPreviousDistance`.
///
/// The immediate predecessor drives the directional target; the older entries
/// only contribute the clearance floor above. Four covers a path that folds
/// back onto itself within four advances (a U-turn of roughly two advances'
/// radius), which is the sharpest turn where re-spacing is still the right
/// answer; tighter curls are a path problem, not a spacing one. Each extra
/// entry costs one AABB test that almost always rejects.
const ON_PATH_INK_NEIGHBOR_WINDOW: usize = 4;

/// Dead zone (world pixels) around the target gap inside which the arc-length
/// seed is kept as is.
///
/// Well below one rasterized pixel, so it never changes the drawn result, but
/// large enough to absorb the float noise between the straight reference
/// measurement and the on-curve one (the two differ only by a rigid transform,
/// so the discretization of `directional_pair_gap` is identical and the residue
/// is pure rounding).
///
/// NOT a dead zone around the LINE TARGET, and therefore NOT what makes a
/// straight line a no-op — it is not one (see the file header). The error this
/// is compared against is `straight_gap - target_gap`, i.e. how far the pair
/// already sits from the gap the line was normalized to; on a straight run of
/// mixed side bearings that difference is a real number of pixels and the pair
/// IS moved. What the epsilon buys is the degenerate case: a pair that already
/// carries the line's own median keeps its exact arc-length seed instead of
/// jittering by a rounding residue.
const ON_PATH_INK_GAP_EPSILON_PX: f32 = 0.05;

/// Lower/upper bound (world pixels) on how far from its arc-length seed the
/// directional search may move a glyph, in EITHER direction.
///
/// The search range is the glyph's own advance clamped into this interval: a
/// correction bigger than one advance means the path geometry, not the spacing,
/// is at fault, and letting the search run further would only smear the error
/// into the rest of the line.
const ON_PATH_INK_SEARCH_MIN_RANGE_PX: f32 = 4.0;

/// Upper bound of the directional search range; see
/// [`ON_PATH_INK_SEARCH_MIN_RANGE_PX`].
const ON_PATH_INK_SEARCH_MAX_RANGE_PX: f32 = 64.0;

/// Number of coarse samples the directional search walks outward from the seed
/// before it brackets a sign change (per direction; it only ever walks one).
const ON_PATH_INK_SCAN_SAMPLES: usize = 12;

/// Bisection steps used to refine a bracketed sign change. A bracket is at most
/// one coarse step wide (<= ~5px), so 12 halvings land well inside
/// [`ON_PATH_INK_GAP_EPSILON_PX`] of the crossing.
const ON_PATH_INK_BISECTION_STEPS: usize = 12;

/// How many times a `MinimumPreviousDistance` line may be re-walked to measure
/// its real run length before its alignment start offset is fixed
/// ([`refine_min_distance_start_offsets`]).
///
/// The iteration is a contraction — the run length changes only as slowly as the
/// path's curvature does along it, and the new offset scales that change by the
/// alignment fraction (`<= 1`) — so it converges in one or two passes on any
/// real path. Three is the hard stop for pathological geometry; the cost is paid
/// ONLY by lines that are both in this mode and not start-aligned.
const ON_PATH_ALIGN_REFINE_PASSES: usize = 3;

/// Convergence tolerance (world pixels) of that refinement: once a pass moves
/// the start offset by less than this, the offset is final. A quarter pixel is
/// well below what the rasterizer can show, so further passes could only burn
/// time.
const ON_PATH_ALIGN_REFINE_TOLERANCE_PX: f32 = 0.25;

/// Shortest chord (world pixels) between two ink centers that still defines a
/// measuring frame. Below it the pair has no meaningful advance direction (a
/// path that doubles back exactly onto itself).
const PAIR_CHORD_MIN_LEN_PX: f32 = 1e-3;

/// Hard floor (world pixels) every on-path step is clamped up to, whatever the
/// letter-spacing settings do. A zero or negative step would leave the
/// arc-length cursor standing still (or walking backwards), piling the rest of
/// the line onto one point, so no consumer may ever see one.
const MIN_ON_PATH_STEP_PX: f32 = 1.0;

/// Seed-advance floor used by the CUSTOM LINE paths (raster and vector drawn
/// lines). It only guards against a non-positive seed advance; a genuinely
/// narrow glyph keeps its narrow step.
const CUSTOM_LINE_SEED_ADVANCE_FLOOR_PX: f32 = 1.0;

/// Everything that decides how far along a path the next glyph sits.
///
/// THE single owner of the on-path step: [`Self::step_px`] is the only place in
/// this crate where "the arc-length distance from this glyph to the next one
/// along the drawn/formula path" is computed. The three consumers — the
/// alignment pre-pass ([`drawn_line_start_offsets`]), the custom-line walk
/// ([`drawn_line_seed_transform`]) and the formula/shape accumulator
/// ([`render_text_with_formula_layout_once`]) — all go through it, and each used
/// to carry its own copy of the expression. Do not re-fork them.
///
/// Both letter-spacing fields are stored ALREADY CLAMPED (the constructors do
/// it), so callers must build this through [`Self::for_custom_lines`] or
/// [`Self::for_formula`] rather than a struct literal.
#[derive(Debug, Clone, Copy)]
struct OnPathStepSpacing {
    /// Multiplier applied to the seed advance before the additive term, clamped
    /// to `0.0..=8.0`.
    letter_spacing_mul: f32,
    /// Additive tracking in world pixels, clamped to `-10_000.0..=10_000.0`.
    /// May be negative; the result is floored regardless.
    letter_spacing_px: f32,
    /// Floor applied to the SEED advance before letter spacing scales it.
    ///
    /// KNOWN DIVERGENCE (deliberately preserved, see the two constructors): the
    /// custom-line paths use [`CUSTOM_LINE_SEED_ADVANCE_FLOOR_PX`] while the
    /// formula/shape path uses half the font size. It is a parameter only
    /// because the two historical values must stay observable, not because the
    /// two paths have different needs.
    seed_advance_floor_px: f32,
}

impl OnPathStepSpacing {
    /// Spacing for the CUSTOM RASTER/VECTOR LINE paths.
    ///
    /// `letter_spacing_mul` / `letter_spacing_px` are the raw
    /// `CustomLineLayoutSettings` values; they are clamped here. The seed floor
    /// is [`CUSTOM_LINE_SEED_ADVANCE_FLOOR_PX`], i.e. it only keeps the step
    /// positive.
    #[must_use]
    fn for_custom_lines(letter_spacing_mul: f32, letter_spacing_px: f32) -> Self {
        Self {
            letter_spacing_mul: letter_spacing_mul.clamp(0.0, 8.0),
            letter_spacing_px: letter_spacing_px.clamp(-10_000.0, 10_000.0),
            seed_advance_floor_px: CUSTOM_LINE_SEED_ADVANCE_FLOOR_PX,
        }
    }

    /// Spacing for the FORMULA/SHAPE path.
    ///
    /// `default_advance_px` is the path's `(font_size_px * 0.5).max(1.0)`
    /// fallback advance, used here as the seed floor.
    ///
    /// KNOWN DEFECT, kept on purpose: that floor is far above the one the custom
    /// line paths use, so on a formula curve every glyph whose shaped advance is
    /// narrower than half the font size (`i`, `l`, `.`, `'`, a strongly kerned
    /// pair) is pushed apart to half an em, and a user-authored negative kerning
    /// pair cannot tighten a step below it at all. The custom-line floor of
    /// `1.0` is the honest one. Lowering it here is a RENDERING change, so it is
    /// out of scope for the refactor that created this type; when it is fixed,
    /// `detect_shape_layout_fallback_reason` must be changed in the same step
    /// (it estimates the run length with the same `default_advance` floor, and
    /// the compression ratio it computes is what decides the shape fallback).
    #[must_use]
    fn for_formula(letter_spacing_mul: f32, letter_spacing_px: f32, default_advance_px: f32) -> Self {
        Self {
            letter_spacing_mul: letter_spacing_mul.clamp(0.0, 8.0),
            letter_spacing_px: letter_spacing_px.clamp(-10_000.0, 10_000.0),
            seed_advance_floor_px: default_advance_px,
        }
    }

    /// The arc-length step in world pixels from the glyph whose seed advance is
    /// `seed_advance_px` to the next glyph on the same line.
    ///
    /// `seed_advance_px` is [`FormulaGlyphSeed::advance_px`]: a MAGNITUDE along
    /// the path, never a signed x step (see `assign_formula_seed_advances`). The
    /// step is `max(seed_advance, seed_advance_floor) * mul + px`, floored at
    /// [`MIN_ON_PATH_STEP_PX`]. The returned value is therefore always
    /// `>= MIN_ON_PATH_STEP_PX` and always positive, so the arc-length cursor
    /// cannot stall or run backwards.
    ///
    /// Two floors, two different jobs: `seed_advance_floor_px` guards the glyph's
    /// own advance BEFORE letter spacing scales it (so a degenerate advance does
    /// not get multiplied), while [`MIN_ON_PATH_STEP_PX`] guards the FINAL step
    /// (so negative additive tracking cannot collapse it). Neither may be
    /// dropped without changing rendering.
    #[must_use]
    fn step_px(self, seed_advance_px: f32) -> f32 {
        ((seed_advance_px.max(self.seed_advance_floor_px) * self.letter_spacing_mul)
            + self.letter_spacing_px)
            .max(MIN_ON_PATH_STEP_PX)
    }
}

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
    /// Weight/style of the SELECTED face — the fallback a FAUX inline span
    /// resolves to, so faux never changes font matching (see
    /// `inline_styles::FauxFaceBaseline`).
    pub(crate) faux_face_baseline: FauxFaceBaseline,
    pub(crate) inline_style_spans: Option<&'a [InlineStyleSpan]>,
    pub(crate) inline_font_registry: &'a InlineFontRegistry,
    /// Per-face user-authored kerning overrides for this render (selected font plus
    /// every inline `<font=…>` font), built once by `pipeline::render_text_to_image`.
    pub(crate) custom_kerning: &'a CustomKerningMap,
    pub(crate) layout_text: &'a str,
    pub(crate) font_size_px: f32,
    pub(crate) base_line_height_px: f32,
    /// Effective perpendicular line-placement fraction in `[-1, 1]`, already
    /// gated by mode in the pipeline router (0.0 for HIDE modes `Shape` /
    /// `CustomRasterLines`, the panel value for `Formula` / `CustomVectorLines`).
    pub(crate) line_placement_frac: f32,
}

/// Line stacking of one formula / shape / custom-line render: the base line
/// height, the grow-only baseline-advance table and the GLOBAL default extra line
/// spacing the table was built around.
///
/// The default is carried separately on purpose: re-deriving it as
/// `extra_spacing_table.first()` is wrong — with the grow-only inline height room,
/// entry 0 can carry extra px that would then be SUBTRACTED from every later gap in
/// the `has_inline_size_overrides` branch of `compute_horizontal_line_baselines`,
/// pulling lines closer.
#[derive(Clone, Copy)]
struct FormulaLines<'a> {
    base_line_height_px: f32,
    extra_spacing_table: &'a [f32],
    default_extra_spacing_px: f32,
}

impl FormulaLines<'_> {
    /// The baseline of every layout line of `buffer` under this stacking.
    fn baselines(self, buffer: &Buffer, has_inline_size_overrides: bool) -> Vec<f32> {
        compute_horizontal_line_baselines(
            buffer,
            self.base_line_height_px,
            self.default_extra_spacing_px,
            self.extra_spacing_table,
            has_inline_size_overrides,
        )
    }
}

#[derive(Debug, Clone)]
struct FormulaGlyphSeed {
    glyph: LayoutGlyph,
    /// The single `char` of this glyph's SOURCE cluster, or `None` when the cluster
    /// is not exactly one character (a ligature, a base + combining mark) or the
    /// glyph is synthesized rather than shaped from the text (the wrapped hyphen).
    ///
    /// Seeds are detached from the `LayoutRun` they came from, so the run text is no
    /// longer reachable when advances are assigned; the character is captured here
    /// instead. Only the user-authored kerning lookup reads it
    /// (`pair_advance::PairAdvanceModel::override_delta_px` states why a multi-char
    /// cluster is skipped).
    cluster_char: Option<char>,
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
    /// Horizontal alignment resolved for this glyph's layout line, i.e.
    /// `params.align` already overridden by an inline `<align=...>` span
    /// (`compute_inline_line_aligns`). The custom-line path turns it into the
    /// line's arc-length start offset; the formula path ignores it (its
    /// arc-length accumulator is one continuous run, not per line).
    line_align: HorizontalAlign,
    advance_px: f32,
    /// Faux bold/italic style resolved once per seed; the outline seam
    /// (`resolve_glyph_outline`) and the draw transforms consume it.
    /// `FauxGlyphStyle::NONE` keeps every seam byte-identical to no faux.
    faux: FauxGlyphStyle,
    /// `true` when this glyph sits in its line's leading/trailing hanging-punctuation
    /// run, so the draw pass must keep it OUT of the extra-info (mean/median center)
    /// sampling — it hangs past the text block and would drag the center with it.
    /// Always `false` unless extra info was requested AND the hanging strength is at
    /// or above the exclusion threshold
    /// (`TextRenderParams::excludes_hanging_from_extra_info`);
    /// it never affects the drawn pixels.
    hanging_excluded: bool,
}

impl FormulaGlyphSeed {
    /// This seed as the pair owner sees it: the glyph plus the source char captured at
    /// seed time (the run text is gone by the time pairs are decided).
    fn pair_glyph(&self) -> PairGlyph<'_> {
        PairGlyph::detached(&self.glyph, self.cluster_char)
    }
}

impl FormulaGlyphSeed {
    /// The scaled glyph rect widened by the faux bounds pads
    /// (`faux_bounds_pads`); exactly the plain `scaled_rect` when faux is off
    /// (the pads are hard zeros). `placement_top` is the bitmap y placement
    /// (pen-relative top above the baseline) the shear pad keys off.
    fn padded_scaled_rect(
        &self,
        src_left: f32,
        src_top: f32,
        glyph_w: f32,
        glyph_h: f32,
        placement_top: f32,
    ) -> (f32, f32, f32, f32) {
        let pads = faux_bounds_pads(
            self.faux,
            placement_top,
            glyph_h,
            self.glyph_scale.width_mul,
            self.glyph_scale.height_mul,
        );
        self.glyph_scale.scaled_rect(
            src_left - pads[0],
            src_top - pads[1],
            glyph_w + 2.0 * pads[0],
            glyph_h + 2.0 * pads[1],
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct FormulaGlyphTransform {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
}

impl RigidPlacement for FormulaGlyphTransform {
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

#[derive(Debug, Clone, Copy)]
struct FormulaArcLengthSample {
    t01: f32,
    arc_len_px: f32,
}

/// On-path placement of a single glyph: the world center of the path point it
/// sits on plus the glyph's total rotation (tangent/static + flip + per-glyph).
/// The blit and the ink-contour placement both derive their world transform
/// from this, guaranteeing they land on the same pixels.
#[derive(Debug, Clone, Copy)]
struct DrawnLineTransform {
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
}

impl RigidPlacement for DrawnLineTransform {
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

/// Shift a glyph center perpendicular to its line toward the TOP side.
///
/// `line_frac` in `[-1, 1]`: `0` keeps the center on the line, `+1` rests the
/// glyph ABOVE the line (ink bottom on the line), `-1` BELOW it (ink top on the
/// line). The magnitude is `line_frac * scaled_height / 2`, so each glyph rests
/// naturally on the line using its own scaled ink height.
///
/// Sign convention: the line's DOWN normal (its bottom side — the same direction
/// a positive `normal_offset_px` moves a glyph, `center_y += tangent_x * offset`)
/// is `(-sin, cos)` in screen y-down space. Moving toward the TOP is its
/// negation `(sin, -cos)`, so a positive `line_frac` subtracts along `y` and
/// shifts rendered content UP on screen. Verified by the `line_placement_*` tests.
fn apply_line_placement(
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
    scaled_height: f32,
    line_frac: f32,
) -> (f32, f32) {
    let offset = line_frac * scaled_height * 0.5;
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    (center_x + offset * sin_a, center_y - offset * cos_a)
}

/// World-space center of the glyph bitmap for a given on-path transform.
///
/// This is the single mapping the blit relies on. The `reference` selects HOW the
/// perpendicular placement is anchored:
/// - [`GlyphHeight`](LinePlacementReference::GlyphHeight): legacy — the glyph's INK
///   CENTER sits on the path point at `line_frac = 0`, shifted by `line_frac` via
///   [`apply_line_placement`] using the glyph's own `scaled_height`. Glyphs of
///   different ink height float to different offsets.
/// - [`LineBox`](LinePlacementReference::LineBox): the glyph's BASELINE is anchored
///   to a shared band (`ascent_scaled` above the baseline is the top, the baseline
///   is the bottom), constant across glyphs, so every glyph shares one baseline and
///   `line_frac` snaps the band baseline(+1, ink rests above the line)/center(0)/top
///   (-1, ink hangs below) — a clean, just-curved line. `placement_top_scaled` is the glyph's scaled
///   top bearing (`placement.top * height_mul`) and `scaled_height` its scaled bitmap
///   height, both used to convert the shared baseline to the bitmap center that
///   [`glyph_outline_transform`] positions.
///
/// Kept free of `FormulaGlyphSeed` so the contour-placement path and unit tests can
/// reuse it.
fn drawn_line_glyph_destination_center_raw(
    transform: &DrawnLineTransform,
    scaled_height: f32,
    placement_top_scaled: f32,
    line_frac: f32,
    reference: LinePlacementReference,
    ascent_scaled: f32,
) -> (f32, f32) {
    match reference {
        LinePlacementReference::GlyphHeight => apply_line_placement(
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
            scaled_height,
            line_frac,
        ),
        LinePlacementReference::LineBox => {
            // Height above the baseline of the band point the line snaps to. `line_frac`
            // in [-1, 1]: +1 -> baseline (bottom) on line, 0 -> band center, -1 -> band
            // top on line. Shared `ascent_scaled` => identical for every glyph, so the
            // baseline is consistent (no per-glyph float).
            let h_sel = ascent_scaled * 0.5 * (1.0 - line_frac);
            // `glyph_outline_transform` places the bitmap CENTER at the returned point;
            // the bitmap center sits `scaled_height/2 - placement_top_scaled` below the
            // baseline, so offset the shared band selection by that per-glyph term. The
            // sign follows [`apply_line_placement`]'s TOP normal `(sin, -cos)`.
            let offset = -(h_sel + scaled_height * 0.5 - placement_top_scaled);
            let (sin_a, cos_a) = transform.rotation_rad.sin_cos();
            (
                transform.center_x + offset * sin_a,
                transform.center_y - offset * cos_a,
            )
        }
    }
}

/// Resolve a seed glyph's true font outline through the per-render cache.
///
/// Thin wrapper over [`resolve_outline_for_glyph`] keyed on the seed's glyph
/// and its faux-bold variant (`seed.faux.bold`), so the on-path modes draw and
/// measure the same offset outline the other paths use. Returns `None` when
/// the font is missing or the glyph has no fillable monochrome outline (space
/// or COLR/bitmap color glyph); callers then fall back to the bitmap blit.
fn resolve_glyph_outline(
    seed: &FormulaGlyphSeed,
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
) -> Option<std::sync::Arc<Outline>> {
    resolve_outline_for_glyph(font_system, outline_cache, &seed.glyph, seed.faux.bold)
}

#[derive(Debug, Clone, Copy)]
struct GlyphInkProfile {
    left_px: f32,
    right_px: f32,
}

impl GlyphInkProfile {
    #[must_use]
    fn fallback(width_px: f32, _height_px: f32) -> Self {
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
    let _span = ms_log::trace_scope!(cat::RENDER, "render_formula_layout mode={:?}", request.params.text_layout_mode);
    let FormulaRenderRequest {
        params,
        font_system,
        buffer,
        attrs,
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        custom_kerning,
        layout_text,
        font_size_px,
        base_line_height_px,
        line_placement_frac,
    } = request;
    let layout_line_offsets = compute_layout_line_offsets(layout_text);
    let line_spacing_percent =
        effective_spacing_percent(params.line_spacing_percent, params.glyph_height_percent);
    let default_extra_line_spacing_px =
        params.line_spacing_px + font_size_px * (line_spacing_percent / 100.0);
    // Lines stack on BASELINES here too, so the table is the grow-only
    // baseline-advance one: an inline `<stretching>` height span may push the
    // next line further away, never pull it closer (see `pipeline/`).
    let line_extra_spacing_table = line_baseline_advance_table(
        params,
        layout_text,
        layout_line_offsets.as_slice(),
        inline_style_spans,
        font_size_px,
        default_extra_line_spacing_px,
        &InlineHeightRoom::measure(
            params,
            buffer,
            font_system,
            layout_line_offsets.as_slice(),
            inline_style_spans,
        ),
    );

    let shaping = RunShaping {
        pen: RunPen {
            params,
            inline_style_spans,
            layout_line_offsets: layout_line_offsets.as_slice(),
            custom_kerning,
            font_size_px,
        },
        attrs,
        faux_face_baseline,
        inline_font_registry,
        hyphen_line_height_px: base_line_height_px,
    };
    let lines = FormulaLines {
        base_line_height_px,
        extra_spacing_table: line_extra_spacing_table.as_slice(),
        default_extra_spacing_px: default_extra_line_spacing_px,
    };

    if params.text_layout_mode == TextLayoutMode::Shape
        && let Some(warning) = detect_shape_layout_fallback_reason(shaping, font_system, buffer, lines)?
    {
        return Ok(FormulaRenderOutcome::FallbackToStandard(warning));
    }

    let initial_margin_pad = font_size_px.ceil().max(2.0) as u32;
    let mut render_margin_pad = initial_margin_pad;
    let mut last_image = None;

    for _ in 0..4 {
        let image = render_text_with_formula_layout_once(
            shaping,
            font_system,
            buffer,
            lines,
            render_margin_pad,
            line_placement_frac,
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
    let _span = ms_log::trace_scope!(cat::RENDER, "render_drawn_lines_layout");
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
    let _span = ms_log::trace_scope!(cat::RENDER, "render_vector_lines_layout");
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

/// The arc-length cursor of ONE custom line, and the single owner of the
/// recurrence that walks it.
///
/// [`drawn_line_center_s`] / [`drawn_line_next_cursor`] own the two halves of
/// the step; this type owns the LOOP around them, including the
/// `shift_following` bump that a whole inline `<offset>` span leaves behind.
/// Three consumers replay the very same recurrence and must not re-fork it:
/// the alignment pre-pass ([`drawn_line_start_offsets`]), the median pre-pass
/// ([`straight_reference_target_gaps`]) and the real walk
/// ([`drawn_line_seed_transform`]). The three differ ONLY in what they do
/// between [`Self::seed_center_s`] and [`Self::commit`] — the pre-passes commit
/// the seed center unchanged, the real walk commits the corrected one.
///
/// The FORMULA accumulator in [`render_text_with_formula_layout_once`] looks
/// similar but is deliberately NOT a consumer: its cursor spans the whole run
/// instead of one line, and an inline `<offset>` enters it as a later per-line
/// shift rather than through [`drawn_line_center_s`], so the two recurrences
/// have different inputs and only the STEP ([`OnPathStepSpacing::step_px`]) is
/// genuinely shared.
#[derive(Debug, Default, Clone, Copy)]
struct LineArcCursor {
    /// Arc length (px along the line path) the next glyph's box starts at.
    s_px: f32,
}

/// Cursor of the STRAIGHT REFERENCE layout: the same glyphs walked along a
/// virtual straight line, which is what `MinimumPreviousDistance` reads its
/// target spacing off (see [`straight_reference_transform`]).
///
/// Single owner of that recurrence. The reference coordinate of a glyph is the
/// previous glyph's reference coordinate plus the NOMINAL along-path step, and
/// that step is read off the difference between this glyph's arc-length SEED and
/// the previous glyph's FINAL arc-length center — which folds in advances,
/// letter spacing, authored kerning and inline offsets without a second copy of
/// the cursor arithmetic. Two consumers: the median pre-pass
/// ([`straight_reference_target_gaps`]), which walks uncorrected seeds, and the
/// real walk ([`drawn_line_seed_transform`]), where a glyph's own correction
/// must NOT feed back into the reference.
///
/// The reference origin is arbitrary (the first glyph lands on `0.0`): only
/// DIFFERENCES between reference coordinates are ever measured.
#[derive(Debug, Default, Clone, Copy)]
struct StraightReferenceWalk {
    /// Reference coordinate of the previously committed glyph.
    x_px: f32,
    /// Final arc-length center of the previously committed glyph, ink or not.
    /// `None` before the line's first placed glyph.
    previous_center_s: Option<f32>,
}

/// Per-line accumulator of the straight reference gaps, used only by
/// [`straight_reference_target_gaps`].
#[derive(Debug, Default)]
struct StraightReferenceGaps {
    /// Line cursor of the uncorrected (`ByLineLength`) replay.
    cursor: LineArcCursor,
    walk: StraightReferenceWalk,
    /// The previously placed glyph of this line when it HAS ink: its seed index
    /// and its ink in the straight reference. `None` after an inkless glyph, so
    /// a pair across a space is never measured.
    previous: Option<(usize, PlacedGlyphInk)>,
    /// One entry per adjacent inked, non-authored pair of the line.
    gaps: Vec<f32>,
}

/// The end of a line path a glyph center fell off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawnLineDropSide {
    /// Before the start of the path (`center_s < 0`).
    BeforeStart,
    /// Past the end of the path (`center_s > total_len_px`).
    PastEnd,
}

/// Stable placement context shared across all seeds of a custom line layout.
///
/// Bundles the immutable layout parameters with the mutable rasterization
/// caches so per-seed placement helpers take a small argument list instead of a
/// long positional one.
struct DrawnLinePlacementCtx<'a> {
    params: &'a TextRenderParams,
    seeds: &'a [FormulaGlyphSeed],
    layout: CustomLineLayoutSettings,
    /// The single owner of the arc-length step between adjacent glyphs; shared
    /// verbatim with the [`drawn_line_start_offsets`] pre-pass.
    spacing: OnPathStepSpacing,
    /// User-authored kerning pairs, consulted only to EXEMPT a pair from the
    /// `MinimumPreviousDistance` normalization (the advances themselves already
    /// carry the override, applied by `assign_formula_seed_advances`).
    custom_kerning: &'a CustomKerningMap,
    /// Per-line target ink gap of `MinimumPreviousDistance`
    /// ([`straight_reference_target_gaps`]); a line absent from the map keeps
    /// its plain arc-length walk.
    line_target_gaps: HashMap<usize, f32>,
    font_system: &'a mut FontSystem,
    caches: &'a mut OnPathInkCaches,
}

/// One already-placed inked glyph of a line, kept in the clearance window of
/// `MinimumPreviousDistance`.
///
/// Both frames are stored because the mode compares them: `world` is where the
/// ink really is on the path, `straight` is where the same ink sits in the
/// STRAIGHT REFERENCE layout, which is what the pair's target gap and its
/// clearance floor are read off.
#[derive(Debug, Clone)]
struct PlacedNeighbor {
    world: PlacedGlyphInk,
    straight: PlacedGlyphInk,
}

/// On-path glyph placement state carried between seeds of one line.
///
/// Everything below `start_offset_s_px` serves `MinimumPreviousDistance` only
/// and stays at its default for `ByLineLength`.
#[derive(Debug, Default)]
struct DrawnLinePlacementState {
    /// The line's arc-length cursor (see [`LineArcCursor`]).
    cursor: LineArcCursor,
    /// The line's alignment start offset, i.e. the value `cursor` was seeded
    /// with. Negative exactly when the run overflows its line, which is
    /// the only case where a glyph before the line start may be dropped
    /// (see [`drawn_line_drop_side`]).
    start_offset_s_px: f32,
    /// MEASUREMENT pass: the walk is only being replayed to learn how long the
    /// run really turns out, so a glyph past the path end must keep the cursor
    /// walking instead of freezing it (see
    /// [`measure_min_distance_run_len_px`]). `false` for the real placement
    /// pass, where a frozen cursor is the whole point of the end drop.
    measure_only: bool,
    /// The last [`ON_PATH_INK_NEIGHBOR_WINDOW`] placed glyphs that HAVE ink,
    /// oldest first; `last()` is the newest. An inkless glyph (a space) does
    /// not enter the window and does not clear it: the glyph after a space must
    /// still not collide with the word before it on a sharp turn.
    neighbors: Vec<PlacedNeighbor>,
    /// Cursor of the STRAIGHT REFERENCE layout this line's ink spacing is judged
    /// against; advanced for every placed glyph, inkless ones included.
    straight: StraightReferenceWalk,
    /// Seed index of the previously placed glyph, ink or not. `None` before the
    /// line's first placed glyph. Used only to look the pair up in the
    /// user-authored kerning table, which exempts it from the normalization.
    previous_seed_idx: Option<usize>,
    /// Whether the previously placed glyph had ink, i.e. whether
    /// `neighbors.last()` is the IMMEDIATE predecessor (the only neighbour a
    /// directional target is derived from) or an older glyph across a space.
    previous_has_ink: bool,
}

/// Cached ink data for one glyph key.
///
/// Keyed by the full cosmic-text [`CacheKey`] (subpixel bins included) so the
/// cached data always matches the exact bitmap the blit references; the extra
/// entries per glyph (one per subpixel bin actually used) are few and cheap.
/// The contour lives in the outline's pen-relative y-down pixel frame; the
/// bitmap `placement_left`/`placement_top` are stored so the contour can be
/// placed with the same pivot the outline rasterizer uses.
#[derive(Debug, Clone)]
struct CachedGlyphInk {
    /// Closed outer contour(s) of the glyph ink in the outline (pen-relative,
    /// y-down px) frame, unscaled/unrotated.
    contour: GlyphContour,
    /// Glyph bitmap width in pixels.
    glyph_w: f32,
    /// Glyph bitmap height in pixels.
    glyph_h: f32,
    /// Bitmap x placement (pen-relative left edge).
    placement_left: f32,
    /// Bitmap y placement (pen-relative top above baseline).
    placement_top: f32,
}

/// The glyph caches one custom-line render shares between seed collection, the
/// `MinimumPreviousDistance` ink search and the composite pass: swash bitmaps
/// (placement boxes, colour fallbacks), the traced ink per [`CacheKey`] and faux
/// variant, and the resolution-independent outlines. Each glyph is extracted at
/// most once per cache per render.
struct OnPathInkCaches {
    swash: SwashCache,
    ink: HashMap<(CacheKey, u32), CachedGlyphInk>,
    outlines: OutlineCache,
}

/// Per-seed ink geometry: the cached glyph contour plus the seed-specific
/// scaled vertical placement needed to map the contour into world space.
#[derive(Debug, Clone)]
struct SeedInkGeometry {
    /// Cached ink contour in the outline (pen-relative, y-down px) frame.
    contour: GlyphContour,
    /// Glyph bitmap width in pixels (pivot input).
    glyph_w: f32,
    /// Glyph bitmap height in pixels (pivot input).
    glyph_h: f32,
    /// Bitmap x placement (pen-relative left edge, pivot input).
    placement_left: f32,
    /// Bitmap y placement (pen-relative top above baseline, pivot input).
    placement_top: f32,
    /// Height of the scaled glyph rect in content coordinates (line-placement basis).
    scaled_height: f32,
    /// Subpixel fraction (`[x_bin, y_bin]`, device px) baked into the bitmap
    /// coverage; folded into the outline pivot so the measured contour lands on
    /// the same pixels as the drawn outline.
    subpixel: [f32; 2],
}

/// A glyph's ink placed in some frame: the world-space contour plus the point
/// its ink box is centered on.
///
/// The center is NOT the contour's AABB center — it is the placement anchor
/// ([`drawn_line_glyph_destination_center_raw`]), i.e. the point the glyph's
/// bitmap box is pinned to. Two of them define a pair's chord, which is the
/// measuring frame of [`on_path_pair_gap`].
#[derive(Debug, Clone)]
struct PlacedGlyphInk {
    contour: PlacedContour,
    center: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct CustomLineLayoutSettings {
    use_tangent_rotation: bool,
    static_rotation_rad: f32,
    normal_offset_px: f32,
    letter_spacing_mul: f32,
    letter_spacing_px: f32,
    /// Effective perpendicular line-placement fraction in `[-1, 1]` (already
    /// mode-gated by the router). Shared by the ink-distance search so the
    /// measured contour lands on the same shifted pixels the blit draws.
    line_placement_frac: f32,
    /// Reference band `line_placement_frac` snaps to. `LineBox` only for
    /// `CustomVectorLines`; every other mode stays `GlyphHeight` (legacy).
    line_placement_reference: LinePlacementReference,
    /// Shared scaled font ascent (baseline..ascender top, in the same px units as
    /// the glyph `scaled_height`) used by the `LineBox` reference; `0` under
    /// `GlyphHeight` where it is ignored.
    ascent_scaled: f32,
}

/// Where a glyph sits ACROSS its line: the perpendicular placement fraction in
/// `[-1, 1]`, the reference band it snaps to, and the shared scaled ascent the
/// `LineBox` reference reads (ignored under `GlyphHeight`).
#[derive(Debug, Clone, Copy)]
struct LinePlacementBand {
    frac: f32,
    reference: LinePlacementReference,
    ascent_scaled: f32,
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
