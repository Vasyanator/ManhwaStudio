/*
File: src/tabs/typing/render_next/formula/render.rs

Purpose:
Formula raster/layout path staged рендера typing.

Main responsibilities:
- рендерить glyph seeds по формульной траектории без зависимости от старого `render.rs`;
- собирать formula-specific glyph metadata, arc-length mapping и rotated bounds/draw;
- отдельно обрабатывать fallback для `TextLayoutMode::Shape`, когда кривая слишком короткая.

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
  excluded from the median (same contract as `pipeline.rs`, where an authored pair
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

use ms_log::trace::cat;

use super::{FormulaEvalInput, FormulaProgramBundle};
use crate::drawn_lines::{
    DrawnLinePath, build_vector_line_paths, load_raster_line_paths,
};
use crate::extra_info::{ExtraInfoAccumulator, rotated_box_samples};
use crate::font_registry::{CustomKerningMap, InlineFontRegistry};
use crate::glyph_blit::{
    glyph_needs_bitmap_fallback, glyph_outline_transform, glyph_subpixel_offset,
    nominal_glyph_advance_px, resolve_outline_for_glyph,
};
use crate::glyph_contour::{
    GlyphContour, PlacedContour, min_placed_distance, placed_aabb_gap,
};
use crate::vector::{
    MeshWarpContext, Outline, OutlineCache, RasterScratch, build_aa_lut,
    glyph_contour_from_outline, rasterize_outline_into,
};
use crate::inline_styles::{
    FauxFaceBaseline, InlineGlyphOffset, InlineStyleSpan, apply_inline_style_to_attrs,
};
use crate::optical::{median_of_gaps, optical_base_advance, optical_delta};
use crate::pair_gap::{GapAxis, directional_pair_gap};
use crate::pipeline::{
    FauxGlyphStyle, GlyphScaleSettings, InlineHeightRoom, KerningSettings,
    compute_horizontal_line_baselines, effective_spacing_percent,
    hanging_edge_run_bounds, horizontal_run_baseline_y, is_edge_run_hanging,
    line_baseline_advance_table, single_char_cluster,
    faux_bounds_pads, faux_style_at_offset, faux_style_for_glyph, horizontal_line_offset,
    resolve_faux_counter_flag,
};
use crate::raster::{
    PixelBounds, RigidPlacement, bilinear_sample_rgba, blend_pixel_over, build_glyph_rgba_buffer,
    include_rotated_rect_bounds, rotate_placements_about_centroid, rotated_rect_world_bounds,
    sample_swash_alpha, trim_rendered_image_to_alpha_bounds,
};
use crate::types::{
    HorizontalAlign, KerningMode, LinePlacementReference, RenderedTextImage, TextLayoutMode,
    TextRenderParams, TextVectorLineDistanceMode, TextVectorLineTextDirection,
};
use cosmic_text::{
    Attrs, AttrsOwned, Buffer, CacheKey, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping,
    SwashCache, SwashContent,
};
use std::collections::HashMap;

const SOFT_HYPHEN: char = '\u{00AD}';

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
    /// (`pipeline::custom_pair_delta_px` states why a multi-char cluster is skipped).
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
    // next line further away, never pull it closer (see `pipeline.rs`).
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

    if params.text_layout_mode == TextLayoutMode::Shape
        && let Some(warning) = detect_shape_layout_fallback_reason(
            params,
            font_system,
            buffer,
            attrs,
            faux_face_baseline,
            inline_style_spans,
            inline_font_registry,
            custom_kerning,
            layout_line_offsets.as_slice(),
            font_size_px,
            base_line_height_px,
            line_extra_spacing_table.as_slice(),
            default_extra_line_spacing_px,
        )?
    {
        return Ok(FormulaRenderOutcome::FallbackToStandard(warning));
    }

    let initial_margin_pad = font_size_px.ceil().max(2.0) as u32;
    let mut render_margin_pad = initial_margin_pad;
    let mut last_image = None;

    for _ in 0..4 {
        let image = render_text_with_formula_layout_once(
            params,
            font_system,
            buffer,
            attrs,
            faux_face_baseline,
            inline_style_spans,
            inline_font_registry,
            custom_kerning,
            layout_line_offsets.as_slice(),
            font_size_px,
            base_line_height_px,
            line_extra_spacing_table.as_slice(),
            default_extra_line_spacing_px,
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

fn render_text_with_drawn_lines_layout_once(
    request: FormulaRenderRequest<'_, '_>,
    paths: &[Option<DrawnLinePath>],
    fixed_output_size: Option<(u32, u32)>,
) -> Result<RenderedTextImage, String> {
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
    // next line further away, never pull it closer (see `pipeline.rs`).
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
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table.as_slice(),
        has_inline_size_overrides,
    );
    // Resolution-independent glyph outlines: shared by the faux counter-flag
    // lookup during seed collection, the ink-distance search (via the transforms
    // below) and the composite pass, so each glyph is extracted at most once per
    // render.
    let mut outline_cache = OutlineCache::new();
    let mut seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        &mut outline_cache,
        buffer,
        attrs,
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        custom_kerning,
        layout_line_offsets.as_slice(),
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            params.width_px.max(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // The swash cache and the glyph-contour cache both live for the whole
    // render so glyph rasterization and ink-contour tracing happen at most once
    // per distinct glyph, and are reused by the bounds and composite passes.
    let mut cache = SwashCache::new();
    let mut contour_cache: HashMap<(CacheKey, u32), CachedGlyphInk> = HashMap::new();
    // Reused per-glyph rasterizer buffers for the composite pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info (mean/median centers). A FRESH accumulator per `_once`
    // call (this function is re-run from scratch by the retry loop), so the stored
    // extras always match the accepted image. Inactive by default -> a true no-op.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    // Shared band for the LineBox line-placement reference: the primary font's ascent
    // (baseline..ascender top) at the line size, in the SAME scaled px space as each
    // glyph's `scaled_height` (so it also carries the base vertical stretch). Computed
    // once per render from the first seed's font; ignored under GlyphHeight. Gate the
    // reference to CustomVectorLines so raster/drawn lines keep the legacy anchoring.
    let base_height_mul = (params.glyph_height_percent / 100.0).clamp(0.01, 3.0);
    let ascent_scaled = seeds
        .first()
        .and_then(|seed| font_system.get_font(seed.glyph.font_id))
        .map(|font| font.as_swash().metrics(&[]).scale(font_size_px).ascent)
        .unwrap_or(font_size_px)
        * base_height_mul;
    let line_placement_reference = match params.text_layout_mode {
        TextLayoutMode::CustomVectorLines => params.line_placement_reference,
        TextLayoutMode::Normal
        | TextLayoutMode::Formula
        | TextLayoutMode::Shape
        | TextLayoutMode::CustomRasterLines => LinePlacementReference::GlyphHeight,
    };
    let mut transforms = build_drawn_line_transforms(
        params,
        seeds.as_slice(),
        paths,
        custom_kerning,
        font_system,
        &mut cache,
        &mut contour_cache,
        &mut outline_cache,
        line_placement_frac,
        ascent_scaled,
    );
    let skipped = transforms.iter().filter(|item| item.is_none()).count();
    // Global block rotation (vector level): rotate every placed line-glyph rigidly
    // about the layout centroid before bounds/draw, so the whole custom-line block
    // turns as one — matching the Ctrl+wheel overlay post-rotation, only crisper.
    let global_rotation_rad = params.global_rotation_deg.to_radians();
    // Capture the PRE-global-rotation content box + rotation centroid for an optional
    // mesh warp, BEFORE `rotate_placements_about_centroid` mutates the transforms in
    // place. The centroid is the mean of the drawable (Some) placement centers —
    // exactly the pivot the rotation pass computes — so the warp peels/reapplies the
    // same global rotation. Gated on `raster_transform` so the fast path is untouched.
    let warp_pre = if params.raster_transform.is_some() {
        let mut pre_box = PixelBounds::empty();
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        let mut count = 0u32;
        for (seed, transform) in seeds.iter().zip(transforms.iter()) {
            let Some(transform) = transform else {
                continue;
            };
            let (cx, cy) = transform.placement_center();
            sum_x += cx;
            sum_y += cy;
            count += 1;
            let physical = seed.glyph.physical(
                (
                    seed.origin_x + seed.glyph_offset_px[0],
                    seed.origin_y + seed.glyph_offset_px[1],
                ),
                1.0,
            );
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
            let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
            if glyph_w <= 0 || glyph_h <= 0 {
                continue;
            }
            let src_left = physical.x + image.placement.left;
            let src_top = physical.y - image.placement.top;
            // Placement (line centering) keys off the UNPADDED scaled height —
            // exactly what the draw pass uses; only the included rect is
            // widened by the faux pads (hard zeros without faux).
            let (_, _, _, scaled_height) = seed
                .glyph_scale
                .scaled_rect(src_left as f32, src_top as f32, glyph_w as f32, glyph_h as f32);
            let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
                image.placement.top as f32,
            );
            let placement_top_scaled = image.placement.top as f32 * seed.glyph_scale.height_mul;
            let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
                transform,
                scaled_height,
                placement_top_scaled,
                line_placement_frac,
                line_placement_reference,
                ascent_scaled,
            );
            include_rotated_rect_bounds(
                &mut pre_box,
                padded_left,
                padded_top,
                padded_width,
                padded_height,
                dst_center_x,
                dst_center_y,
                transform.rotation_rad,
            );
        }
        let inv = 1.0 / count.max(1) as f32;
        Some((pre_box, [sum_x * inv, sum_y * inv]))
    } else {
        None
    };
    if params.global_rotation_deg.abs() > f32::EPSILON {
        rotate_placements_about_centroid(
            transforms
                .iter_mut()
                .filter_map(Option::as_mut)
                .map(|transform| transform as &mut dyn RigidPlacement)
                .collect(),
            global_rotation_rad,
        );
    }
    let mut bounds = PixelBounds::empty();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        let Some(transform) = transform else {
            continue;
        };
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
        let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
        if glyph_w <= 0 || glyph_h <= 0 {
            continue;
        }
        let src_left = physical.x + image.placement.left;
        let src_top = physical.y - image.placement.top;
        // Same split as the warp pre-box: unpadded height for the placement,
        // faux-padded rect for the bounds so offset/sheared ink never clips.
        let (_, _, _, scaled_height) = seed.glyph_scale.scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
        );
        let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
            image.placement.top as f32,
        );
        let placement_top_scaled = image.placement.top as f32 * seed.glyph_scale.height_mul;
        let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
            transform,
            scaled_height,
            placement_top_scaled,
            line_placement_frac,
            line_placement_reference,
            ascent_scaled,
        );
        include_rotated_rect_bounds(
            &mut bounds,
            padded_left,
            padded_top,
            padded_width,
            padded_height,
            dst_center_x,
            dst_center_y,
            transform.rotation_rad,
        );
    }

    let mut warnings = Vec::new();
    if skipped > 0 {
        warnings.push(format!(
            "Рисованные линии: не отрисовано символов без подходящей точки линии: {skipped}."
        ));
    }
    if !bounds.initialized {
        if let Some((width, height)) = fixed_output_size {
            return Ok(RenderedTextImage {
                width,
                height,
                rgba: RenderedTextImage::transparent(width, height).rgba,
                warnings,
                content_origin_x: 0,
                content_origin_y: 0,
                extra: crate::types::RenderedTextExtraInfo::default(),
                font_fallbacks: crate::types::FontFallbackReport::default(),
            });
        }
        return Ok(RenderedTextImage {
            width: params.width_px.max(1),
            height: base_line_height_px.ceil().max(1.0) as u32,
            rgba: RenderedTextImage::transparent(
                params.width_px.max(1),
                base_line_height_px.ceil().max(1.0) as u32,
            )
            .rgba,
            warnings,
            content_origin_x: 0,
            content_origin_y: 0,
            extra: crate::types::RenderedTextExtraInfo::default(),
            font_fallbacks: crate::types::FontFallbackReport::default(),
        });
    }

    let pad = font_size_px.ceil().max(2.0) as u32;
    // Optional vector mesh warp. The normalization frame is the fixed vector-lines
    // canvas when it is in play (no global rotation) — matching the authoring UI,
    // which normalizes handles against that canvas — otherwise the pre-rotation
    // content bounds. Design B may still override the box SIZE with the mesh's
    // stored source dims inside `MeshWarpContext::new`. Peel/reapply use the SAME
    // centroid + angle the rotation pass used. `None`/identity/invalid => fast path.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let (pre_box, centroid) = warp_pre.as_ref()?;
        let (box_min, box_size) = if let Some((width, height)) =
            fixed_output_size.filter(|_| params.global_rotation_deg.abs() <= f32::EPSILON)
        {
            ([0.0, 0.0], [width as f32, height as f32])
        } else if pre_box.initialized {
            (
                [pre_box.min_x as f32, pre_box.min_y as f32],
                [
                    (pre_box.max_x - pre_box.min_x) as f32,
                    (pre_box.max_y - pre_box.min_y) as f32,
                ],
            )
        } else {
            return None;
        };
        MeshWarpContext::new(warp, box_min, box_size, global_rotation_rad, *centroid)
    });
    // A fixed canvas (vector-lines) is honored only when there is no global
    // rotation AND no active warp; once the block is rotated OR warped the canvas
    // must grow to the transformed bounds (like the Ctrl+wheel overlay) so no corner
    // is clipped.
    let honor_fixed_size = fixed_output_size
        .filter(|_| params.global_rotation_deg.abs() <= f32::EPSILON && warp_ctx.is_none());
    // Grow the content bounds to the warped+rotated lattice extent so a strong
    // outward warp never clips (no-op for `None`/identity; only reached when the
    // fixed canvas is not honored, i.e. exactly when the warp is active).
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }
    let (out_width, out_height, x_offset, y_offset) =
        if let Some((width, height)) = honor_fixed_size {
            (width.max(1), height.max(1), 0, 0)
        } else {
            (
                u32::try_from((bounds.max_x - bounds.min_x).max(1))
                    .unwrap_or(1)
                    .saturating_add(pad * 2),
                u32::try_from((bounds.max_y - bounds.min_y).max(1))
                    .unwrap_or(1)
                    .saturating_add(pad * 2),
                -bounds.min_x + i32::try_from(pad).unwrap_or(0),
                -bounds.min_y + i32::try_from(pad).unwrap_or(0),
            )
        };
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    for (seed, transform) in seeds.drain(..).zip(transforms) {
        let Some(transform) = transform else {
            continue;
        };
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = image.placement.width as usize;
        let glyph_h = image.placement.height as usize;
        if glyph_w == 0 || glyph_h == 0 {
            continue;
        }
        let placement_left = image.placement.left as f32;
        let placement_top = image.placement.top as f32;
        let src_left = (physical.x + image.placement.left) as f32;
        let src_top = (physical.y - image.placement.top) as f32;
        let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let placement_top_scaled = placement_top * seed.glyph_scale.height_mul;
        let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
            &transform,
            scaled_height,
            placement_top_scaled,
            line_placement_frac,
            line_placement_reference,
            ascent_scaled,
        );

        // Resolve the outline up front so the extra-info sample knows whether this
        // glyph draws warped (outline) or as an UNWARPED color-glyph bitmap fallback.
        let glyph_outline = resolve_glyph_outline(&seed, font_system, &mut outline_cache);
        // Whether this glyph puts ink on the canvas at all: it either has an
        // outline, or it is an outline-less glyph whose bitmap still gets blitted.
        // `false` only for a glyph a faux THINNING offset consumed entirely, which
        // draws nothing (the `continue` below) and therefore must not vote on the
        // extra-info ink centers either. Short-circuits, so an outline glyph pays
        // no extra lookup.
        let draws_ink = glyph_outline.is_some()
            || glyph_needs_bitmap_fallback(
                font_system,
                &mut outline_cache,
                &seed.glyph,
                seed.faux.bold,
            );

        // Extra-info sample: the final line-placed, rotated glyph box the composite
        // pass draws. Both kinds contribute, but only the outline sample is warpable;
        // the bitmap fallback's sample stays unwarped to match its unwarped pixels.
        // Leading/trailing hanging punctuation is skipped so it cannot drag the center.
        if extra_active && draws_ink && !seed.hanging_excluded {
            let (scaled_w, scaled_h) =
                seed.glyph_scale.scaled_size(glyph_w as f32, glyph_h as f32);
            let (corners, center) = rotated_box_samples(
                dst_center_x,
                dst_center_y,
                scaled_w,
                scaled_h,
                transform.rotation_rad,
            );
            extra_acc.add_glyph(corners, center, glyph_outline.is_some(), seed.line_idx);
        }

        // Prefer the true font outline: rasterize it directly into the output at
        // the exact world placement the bitmap blit would have used. Color/emoji
        // glyphs have no monochrome outline and keep the bitmap blit below.
        if let Some(outline) = glyph_outline {
            let glyph_transform = glyph_outline_transform(
                dst_center_x,
                dst_center_y,
                transform.rotation_rad,
                placement_left,
                placement_top,
                glyph_w as f32,
                glyph_h as f32,
                seed.glyph_scale.width_mul,
                seed.glyph_scale.height_mul,
                glyph_subpixel_offset(physical.cache_key),
                seed.faux.shear_x,
            );
            rasterize_outline_into(
                &mut raster_scratch,
                rgba.as_mut_slice(),
                out_width as usize,
                out_height as usize,
                -(x_offset as f32),
                -(y_offset as f32),
                &outline,
                &glyph_transform,
                seed.text_color,
                &aa_lut,
                warp_ctx.as_ref(),
            );
            continue;
        }

        // A glyph whose outline was CONSUMED by a faux thinning offset draws
        // nothing: blitting its bitmap would restore it at FULL weight.
        if !draws_ink {
            continue;
        }

        // Fallback: the original rotated bitmap blit for any outline-less glyph
        // (real color glyph or a monochrome embedded-bitmap glyph). This path draws
        // it regardless of color; the subpixel fraction is already in the bitmap.
        let src_center_x = src_left + glyph_w as f32 * 0.5;
        let src_center_y = src_top + glyph_h as f32 * 0.5;
        let cos_a = transform.rotation_rad.cos();
        let sin_a = transform.rotation_rad.sin();
        let glyph_rgba = build_glyph_rgba_buffer(
            &image.content,
            image.data.as_slice(),
            glyph_w,
            glyph_h,
            seed.text_color,
        );
        let (scaled_left, scaled_top, scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            dst_center_x,
            dst_center_y,
            transform.rotation_rad,
        );
        let dst_min_x = ((min_x + x_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_x = ((max_x + x_offset as f32).ceil() as i32 + 1).min(out_width as i32);
        let dst_min_y = ((min_y + y_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_y = ((max_y + y_offset as f32).ceil() as i32 + 1).min(out_height as i32);
        for dst_y in dst_min_y..dst_max_y {
            for dst_x in dst_min_x..dst_max_x {
                let world_x = dst_x as f32 + 0.5 - x_offset as f32;
                let world_y = dst_y as f32 + 0.5 - y_offset as f32;
                let rel_x = world_x - dst_center_x;
                let rel_y = world_y - dst_center_y;
                let rotated_x = rel_x * cos_a + rel_y * sin_a;
                let rotated_y = -rel_x * sin_a + rel_y * cos_a;
                let src_x = src_center_x + rotated_x / seed.glyph_scale.width_mul;
                let src_y = src_center_y + rotated_y / seed.glyph_scale.height_mul;
                let local_x = src_x - src_left - 0.5;
                let local_y = src_y - src_top - 0.5;
                let (src_r, src_g, src_b, src_a) =
                    bilinear_sample_rgba(glyph_rgba.as_slice(), glyph_w, glyph_h, local_x, local_y);
                if src_a == 0 {
                    continue;
                }
                let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
                blend_pixel_over(&mut rgba[dst_idx..dst_idx + 4], src_r, src_g, src_b, src_a);
            }
        }
    }

    // Extra-info centers: warp the raw content-space samples through the same mesh
    // context the composite pass used, then map to canvas pixels via the pass
    // offset. Runs BEFORE the caller's trim/effects; both seams self-correct.
    if let Some(ctx) = warp_ctx.as_ref() {
        extra_acc.map_points(|point| ctx.warp_world(point));
    }
    let extra = extra_acc.finish(x_offset as f32, y_offset as f32);

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings,
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        // Filled in by `pipeline::render_text_to_image`, which owns the shaped
        // buffer this layout drew from.
        font_fallbacks: crate::types::FontFallbackReport::default(),
    })
}

/// Fraction of a path's free arc length that is placed BEFORE the run.
///
/// Mirrors [`HorizontalAlign::offset_fraction`]: `0.0` pins the run to the start
/// of the path, `0.5` centers it, `1.0` pins it to the end. Justified alignment
/// stretches lines to the block width and hides the alignment slider in the UI,
/// so `bias` must never be read there: justify returns `justify_fraction`, which
/// each on-path layout supplies as its own historical default (custom lines
/// `0.0` = start of line, formula `0.5` = centered) so justified overlays keep
/// rendering exactly where they did before alignment was honored on path.
fn on_path_align_fraction(align: HorizontalAlign, justify_fraction: f32) -> f32 {
    if align.justify {
        return justify_fraction;
    }
    align.offset_fraction()
}

/// Arc-length center of the glyph sitting at line cursor `cursor_px`.
///
/// `line_offset_px` is the glyph's inline `<offset>` shift along the line
/// (`InlineGlyphOffset::line_px`); it moves the glyph only, and
/// [`drawn_line_next_cursor`] takes it back out. Single source of truth for both
/// the real placement walk and the alignment pre-pass, so the two can never
/// drift apart.
fn drawn_line_center_s(cursor_px: f32, advance_px: f32, line_offset_px: f32) -> f32 {
    cursor_px + advance_px * 0.5 + line_offset_px
}

/// The line cursor left behind by a glyph centered at `center_s_px`.
///
/// Inverse companion of [`drawn_line_center_s`]: it removes `line_offset_px`
/// again so an inline offset shifts its own glyph without dragging the rest of
/// the line (that is what `InlineGlyphOffset::shift_following` is for, applied
/// separately by the callers).
fn drawn_line_next_cursor(center_s_px: f32, advance_px: f32, line_offset_px: f32) -> f32 {
    center_s_px + advance_px * 0.5 - line_offset_px
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

impl LineArcCursor {
    /// A cursor starting at a line's alignment offset (see
    /// [`drawn_line_start_offset`]).
    #[must_use]
    fn starting_at(start_offset_s_px: f32) -> Self {
        Self {
            s_px: start_offset_s_px,
        }
    }

    /// Arc-length SEED center of the glyph sitting at the cursor, i.e. where the
    /// plain `ByLineLength` walk puts it.
    ///
    /// Pure: the cursor only moves on [`Self::commit`], so a caller may correct
    /// the returned center before committing it.
    #[must_use]
    fn seed_center_s(self, seed: &FormulaGlyphSeed, advance_px: f32) -> f32 {
        drawn_line_center_s(self.s_px, advance_px, seed.extended_offset.line_px)
    }

    /// Walk the cursor past a glyph whose FINAL arc-length center is
    /// `final_center_s`.
    ///
    /// `all_seeds` is the whole seed list, needed only to decide whether this
    /// seed closes an inline `<offset>` span that shifts everything after it
    /// ([`is_last_seed_in_offset_span_on_line`]).
    fn commit(
        &mut self,
        all_seeds: &[FormulaGlyphSeed],
        seed: &FormulaGlyphSeed,
        advance_px: f32,
        final_center_s: f32,
    ) {
        let line_offset_px = seed.extended_offset.line_px;
        self.s_px = drawn_line_next_cursor(final_center_s, advance_px, line_offset_px);
        if seed.extended_offset.shift_following
            && is_last_seed_in_offset_span_on_line(all_seeds, seed)
        {
            self.s_px += line_offset_px;
        }
    }
}

/// The alignment that governs a whole line: the [`FormulaGlyphSeed::line_align`]
/// of its FIRST seed, so an inline `<align=...>` opened later in the line cannot
/// re-align the part already laid out.
///
/// Single owner of that rule, shared by the alignment pre-pass
/// ([`drawn_line_start_offsets`]) and by the `MinimumPreviousDistance` start
/// refinement ([`refine_min_distance_start_offsets`]), which must agree on which
/// alignment a line has.
#[must_use]
fn drawn_line_aligns(seeds: &[FormulaGlyphSeed]) -> HashMap<usize, HorizontalAlign> {
    let mut aligns = HashMap::<usize, HorizontalAlign>::new();
    for seed in seeds {
        aligns.entry(seed.line_idx).or_insert(seed.line_align);
    }
    aligns
}

/// Arc-length position where a line's run must start to satisfy `align`.
///
/// Returns `(total_len_px - run_len_px) * fraction`, with `fraction` from
/// [`on_path_align_fraction`] using `0.0` (start of line) as the justify
/// default. The free space is deliberately NOT clamped to `>= 0`: a run longer
/// than its line yields a NEGATIVE start, which is exactly what moves the
/// clipping from the end of the line to its start.
fn drawn_line_start_offset(total_len_px: f32, run_len_px: f32, align: HorizontalAlign) -> f32 {
    (total_len_px - run_len_px) * on_path_align_fraction(align, 0.0)
}

/// Per-line arc-length start offsets that realise the alignment bias on a
/// custom-line layout.
///
/// Each line uses its own [`FormulaGlyphSeed::line_align`] (so inline
/// `<align=...>` overrides keep working) fed to [`drawn_line_start_offset`].
/// Lines with no path are absent from the map and start at `0.0`.
///
/// `spacing` MUST be the very same [`OnPathStepSpacing`] the real walk in
/// [`drawn_line_seed_transform`] uses: this pre-pass replays that walk, so a
/// different step here would misplace every line's start.
///
/// A line's `run_len_px` is its FINAL CURSOR, i.e. the `ByLineLength` walk of
/// [`drawn_line_seed_transform`] replayed through the shared
/// [`drawn_line_center_s`] / [`drawn_line_next_cursor`] pair: the sum of the
/// effective advances plus the `shift_following` bumps, which genuinely do move
/// every following glyph. A plain (non-`shift_following`) inline
/// `<offset>`/`line_px` deliberately does NOT count, matching the contract
/// [`drawn_line_next_cursor`] documents: such an offset moves its own glyph
/// only. Consequence, by design: a glyph nudged forward past the line end by its
/// own inline offset is invisible to the alignment and can still be clipped, and
/// one nudged glyph never drags the whole line's alignment. The result is
/// floored at `0.0` so a line whose `shift_following` bumps run backwards past
/// the origin cannot produce a negative length.
///
/// Accuracy per spacing mode:
/// - `ByLineLength`: EXACT — that is the recurrence being replayed, and it is
///   the final answer.
/// - `MinimumPreviousDistance`: a STARTING ESTIMATE only. Each glyph's ink
///   correction shifts every following one, so the real run length drifts from
///   the replayed one; on a path that bends the same way throughout the
///   corrections do not cancel and the run ends up genuinely longer. The result
///   here is therefore just the first iterate of
///   [`refine_min_distance_start_offsets`], which re-walks the line and replaces
///   it with an offset derived from the MEASURED length. Do not read an
///   end-aligned `MinimumPreviousDistance` line's start offset off this function.
fn drawn_line_start_offsets(
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
    spacing: OnPathStepSpacing,
) -> HashMap<usize, f32> {
    let aligns = drawn_line_aligns(seeds);
    // Per line: the running cursor of the uncorrected replay.
    let mut lines = HashMap::<usize, LineArcCursor>::new();
    for seed in seeds {
        let cursor = lines.entry(seed.line_idx).or_default();
        let advance = spacing.step_px(seed.advance_px);
        // Nothing to correct in this replay, so the seed center IS the final one.
        let center_s = cursor.seed_center_s(seed, advance);
        cursor.commit(seeds, seed, advance, center_s);
    }

    lines
        .into_iter()
        .filter_map(|(line_idx, cursor)| {
            let path = paths.get(line_idx).and_then(Option::as_ref)?;
            let align = aligns.get(&line_idx).copied()?;
            Some((
                line_idx,
                drawn_line_start_offset(path.total_len_px, cursor.s_px.max(0.0), align),
            ))
        })
        .collect()
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

impl StraightReferenceWalk {
    /// Reference coordinate of a glyph whose arc-length SEED is `center_s_seed`.
    ///
    /// Pure: the walk only moves on [`Self::commit`], so a caller may ask for a
    /// candidate's reference coordinate before deciding to place it.
    #[must_use]
    fn reference_x(&self, center_s_seed: f32) -> f32 {
        self.x_px + self.previous_center_s.map_or(0.0, |prev| center_s_seed - prev)
    }

    /// Record a placed glyph: its reference coordinate `x_px` (from
    /// [`Self::reference_x`]) and its FINAL arc-length center.
    fn commit(&mut self, x_px: f32, final_center_s: f32) {
        self.x_px = x_px;
        self.previous_center_s = Some(final_center_s);
    }
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

/// Target ink gap of every line that uses `MinimumPreviousDistance`: the MEDIAN
/// straight-reference gap of that line's kernable pairs, keyed by `line_idx`.
///
/// This is the contract of the mode (see the file header): every adjacent pair
/// of a line is re-spaced to ONE ink-to-ink distance, so the run reads evenly
/// along the curve. The median is taken in the STRAIGHT REFERENCE, never on the
/// curve, so dragging a path node cannot re-space the whole line — and it is the
/// same statistic `KerningMode::Optical` normalizes on, via the shared
/// [`median_of_gaps`].
///
/// Which pairs contribute:
/// - both glyphs must have ink — an inkless glyph (a space) ends the pair chain,
///   so inter-word distance is never normalized and words cannot merge;
/// - the pair must not carry a USER-AUTHORED kerning override
///   ([`custom_seed_pair_delta_px`]): an authored pair keeps the spacing its
///   author gave it and must not drag the target either;
/// - non-finite gaps (inks that do not face each other at all) are dropped by
///   [`median_of_gaps`] itself.
///
/// A line with no contributing pair is absent from the map and is left on its
/// plain arc-length walk. Lines on any other distance mode are never even
/// measured, so `ByLineLength` (and every non-vector layout mode) pays nothing
/// here — not even glyph-ink extraction.
///
/// The replay must use the SAME `spacing` as the real walk, for the same reason
/// [`drawn_line_start_offsets`] must.
// The pre-pass needs the layout inputs plus all four rasterization caches;
// bundling them would only move the same list one call deeper.
#[allow(clippy::too_many_arguments)]
fn straight_reference_target_gaps(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    layout: &CustomLineLayoutSettings,
    spacing: OnPathStepSpacing,
    custom_kerning: &CustomKerningMap,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    contour_cache: &mut HashMap<(CacheKey, u32), CachedGlyphInk>,
    outline_cache: &mut OutlineCache,
) -> HashMap<usize, f32> {
    let mut lines = HashMap::<usize, StraightReferenceGaps>::new();
    for (seed_idx, seed) in seeds.iter().enumerate() {
        if vector_line_distance_mode(params, seed.line_idx)
            != TextVectorLineDistanceMode::MinimumPreviousDistance
        {
            continue;
        }
        let advance = spacing.step_px(seed.advance_px);
        let line = lines.entry(seed.line_idx).or_default();
        // Uncorrected replay: in the reference there is nothing to correct, so
        // the seed center IS the final center.
        let center_s = line.cursor.seed_center_s(seed, advance);
        let x = line.walk.reference_x(center_s);
        line.walk.commit(x, center_s);
        line.cursor.commit(seeds, seed, advance, center_s);

        let Some(geom) = seed_ink_geometry(seed, font_system, cache, contour_cache, outline_cache)
        else {
            // Inkless glyph (a space): it ends the pair chain, so no inter-word
            // distance ever enters the target.
            line.previous = None;
            continue;
        };
        let ink = place_seed_ink_straight(params, seed, &geom, layout, x);
        if let Some((prev_idx, prev_ink)) = line.previous.as_ref()
            // An authored pair is exempt from the normalization, so it must not
            // influence the target the rest of the line is normalized to either.
            && custom_seed_pair_delta_px(custom_kerning, &seeds[*prev_idx], seed).is_none()
            && let Some(gap) = on_path_pair_gap(prev_ink, &ink)
        {
            line.gaps.push(gap);
        }
        line.previous = Some((seed_idx, ink));
    }

    lines
        .into_iter()
        .filter_map(|(line_idx, line)| Some((line_idx, median_of_gaps(line.gaps.as_slice())?)))
        .collect()
}

/// The end of a line path a glyph center fell off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawnLineDropSide {
    /// Before the start of the path (`center_s < 0`).
    BeforeStart,
    /// Past the end of the path (`center_s > total_len_px`).
    PastEnd,
}

/// Decide whether a glyph centered at `center_s_px` must be dropped, and on
/// which side of its line path.
///
/// `sample_drawn_line_path` CLAMPS outside `[0, total_len_px]`, so an
/// out-of-range glyph would be piled onto the first or last path point instead
/// of being placed. Past the end that is always wrong, so it is always a drop.
///
/// Before the start it is a drop ONLY when the line's own `start_offset_s_px` is
/// negative — which happens exactly when the run overflows its line and an
/// end-biased ALIGNMENT pushed the leading glyphs off, where the clipping is
/// what the user asked for. With a non-negative start offset a negative center
/// can only come from a manual inline `<offset>` nudge on a leading glyph, whose
/// historical (and still expected) behaviour is to clamp onto the first path
/// point and DRAW; dropping it there would silently delete a hand-placed glyph.
fn drawn_line_drop_side(
    center_s_px: f32,
    start_offset_s_px: f32,
    total_len_px: f32,
) -> Option<DrawnLineDropSide> {
    if center_s_px > total_len_px {
        return Some(DrawnLineDropSide::PastEnd);
    }
    if center_s_px < 0.0 && start_offset_s_px < 0.0 {
        return Some(DrawnLineDropSide::BeforeStart);
    }
    None
}

/// Compute the per-glyph on-path transforms for every seed of a custom line
/// layout.
///
/// `font_system`/`cache`/`contour_cache`/`outline_cache` are only touched for
/// lines that use `MinimumPreviousDistance` spacing, which needs the glyph's ink
/// contour (derived from its outline); `ByLineLength` lines never extract here.
/// The returned vector is index-aligned with `seeds`; `None` means the glyph
/// could not be placed (outside its line path or missing sample) and is dropped
/// by callers.
///
/// Each line's cursor starts at [`drawn_line_start_offsets`], so the alignment
/// bias positions the run ALONG its line: bias `< 0` starts at the line start
/// (overflow cut at the end), bias `> 0` ends at the line end (cut at the
/// start), bias `== 0` cuts evenly at both ends. On a `MinimumPreviousDistance`
/// line that offset is then REFINED against the run's real length
/// ([`refine_min_distance_start_offsets`]), because the pre-pass can only
/// estimate it there.
// The placement context needs all four immutable/mutable dependencies; bundling
// the three caches would not simplify the call site.
#[allow(clippy::too_many_arguments)]
fn build_drawn_line_transforms(
    params: &TextRenderParams,
    seeds: &[FormulaGlyphSeed],
    paths: &[Option<DrawnLinePath>],
    custom_kerning: &CustomKerningMap,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    contour_cache: &mut HashMap<(CacheKey, u32), CachedGlyphInk>,
    outline_cache: &mut OutlineCache,
    line_placement_frac: f32,
    ascent_scaled: f32,
) -> Vec<Option<DrawnLineTransform>> {
    let mut line_offsets = HashMap::<usize, DrawnLinePlacementState>::new();
    let layout_settings = custom_line_layout_settings(params, line_placement_frac, ascent_scaled);
    let spacing = OnPathStepSpacing::for_custom_lines(
        layout_settings.letter_spacing_mul,
        layout_settings.letter_spacing_px,
    );
    // Pre-pass: every line's run length must be known before the first glyph is
    // placed, because the alignment bias decides where that line's cursor starts.
    // It replays the walk below, so it must be given the SAME spacing.
    let mut start_offsets = drawn_line_start_offsets(seeds, paths, spacing);
    // Pre-pass: the ONE ink gap every pair of a `MinimumPreviousDistance` line is
    // normalized to. Must also precede the walk — the first pair already needs
    // the whole line's median. A no-op for every other distance mode.
    let line_target_gaps = straight_reference_target_gaps(
        params,
        seeds,
        &layout_settings,
        spacing,
        custom_kerning,
        font_system,
        cache,
        contour_cache,
        outline_cache,
    );
    let mut ctx = DrawnLinePlacementCtx {
        params,
        seeds,
        layout: layout_settings,
        spacing,
        custom_kerning,
        line_target_gaps,
        font_system,
        cache,
        contour_cache,
        outline_cache,
    };
    // The estimate above is EXACT for `ByLineLength` and only an estimate for
    // `MinimumPreviousDistance`; replace it there by the measured run length.
    refine_min_distance_start_offsets(&mut ctx, paths, &mut start_offsets);
    let mut transforms: Vec<Option<DrawnLineTransform>> = Vec::with_capacity(seeds.len());
    for (seed_idx, seed) in seeds.iter().enumerate() {
        let Some(path) = paths.get(seed.line_idx).and_then(Option::as_ref) else {
            transforms.push(None);
            continue;
        };
        let state = line_offsets
            .entry(seed.line_idx)
            .or_insert_with(|| {
                DrawnLinePlacementState::starting_at(
                    start_offsets.get(&seed.line_idx).copied().unwrap_or(0.0),
                )
            });
        transforms.push(drawn_line_seed_transform(
            &mut ctx, seed_idx, seed, path, state,
        ));
    }
    apply_drawn_line_group_rotations(seeds, transforms.as_mut_slice());
    transforms
}

/// Replace the ESTIMATED alignment start offset of every alignment-sensitive
/// `MinimumPreviousDistance` line by one derived from its run's REAL length.
///
/// Why it exists: [`drawn_line_start_offsets`] replays the plain arc-length
/// walk, which is exact for `ByLineLength` but only an estimate here — each
/// glyph's ink correction shifts every following one, and on a path that bends
/// the same way throughout those corrections do NOT cancel. The run then ends up
/// longer than predicted, and an end-biased alignment pushed its tail past the
/// path end, where [`drawn_line_drop_side`] deleted the last glyph. Measuring
/// beats padding: no reserve is right for every geometry, and a reserve that is
/// too large mis-aligns every line that does not need it.
///
/// Which lines pay: only those whose distance mode is `MinimumPreviousDistance`
/// AND whose alignment actually depends on the run length. A start-aligned line
/// ([`on_path_align_fraction`] `== 0.0`, the default for custom lines and for
/// justify) starts at `0.0` whatever the run measures, so it is skipped
/// entirely and costs nothing — which is also why the cost of this pass is
/// invisible in the common case.
///
/// How: [`measure_min_distance_run_len_px`] re-walks the line from the current
/// offset and reports where the cursor really ends up; the offset is recomputed
/// from that through the same [`drawn_line_start_offset`] the pre-pass uses, and
/// the two steps repeat until the offset moves by less than
/// [`ON_PATH_ALIGN_REFINE_TOLERANCE_PX`] or
/// [`ON_PATH_ALIGN_REFINE_PASSES`] passes are spent. A run that overflows its
/// line still yields a NEGATIVE offset, i.e. the clipping keeps moving to the
/// side the bias points away from.
///
/// Lines are refined independently and share only content-keyed caches, so the
/// arbitrary iteration order of the align map cannot make the output differ
/// between runs.
fn refine_min_distance_start_offsets(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    paths: &[Option<DrawnLinePath>],
    start_offsets: &mut HashMap<usize, f32>,
) {
    let params = ctx.params;
    let aligns = drawn_line_aligns(ctx.seeds);
    let mut refined = Vec::<(usize, f32)>::new();
    for (&line_idx, &align) in &aligns {
        if vector_line_distance_mode(params, line_idx)
            != TextVectorLineDistanceMode::MinimumPreviousDistance
        {
            continue;
        }
        // A start-aligned run's offset does not depend on its length at all.
        if on_path_align_fraction(align, 0.0) == 0.0 {
            continue;
        }
        let Some(path) = paths.get(line_idx).and_then(Option::as_ref) else {
            continue;
        };
        let mut start_offset = start_offsets.get(&line_idx).copied().unwrap_or(0.0);
        for _ in 0..ON_PATH_ALIGN_REFINE_PASSES {
            let run_len_px = measure_min_distance_run_len_px(ctx, line_idx, path, start_offset);
            let next = drawn_line_start_offset(path.total_len_px, run_len_px, align);
            let settled = (next - start_offset).abs() <= ON_PATH_ALIGN_REFINE_TOLERANCE_PX;
            start_offset = next;
            if settled {
                break;
            }
        }
        refined.push((line_idx, start_offset));
    }
    start_offsets.extend(refined);
}

/// Arc length (px) one `MinimumPreviousDistance` line's run really occupies when
/// walked from `start_offset_s_px`.
///
/// Replays the REAL walk ([`drawn_line_seed_transform`]) for that line's seeds —
/// ink extraction, directional search and clearance net included, so the
/// measured length carries the very corrections the estimate cannot predict —
/// and returns how far the line cursor moved. The replay runs with
/// [`DrawnLinePlacementState::measure_only`] set, so a glyph past the path end
/// still advances the cursor by its nominal step instead of freezing it: an
/// overflowing run must measure LONGER than its line, or the offset it feeds
/// could never become negative and move the clipping to the other end.
///
/// The result is floored at `0.0` for the same reason
/// [`drawn_line_start_offsets`] floors its own: a line whose `shift_following`
/// bumps run backwards past the origin has no negative length.
///
/// Costs one full placement pass over that line. The caches in `ctx` are shared
/// with the real walk, so glyph outlines and contours are extracted once for
/// both.
fn measure_min_distance_run_len_px(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    line_idx: usize,
    path: &DrawnLinePath,
    start_offset_s_px: f32,
) -> f32 {
    // Copied out so the loop's shared borrow of the seeds does not collide with
    // the mutable borrow of `ctx` the placement needs.
    let seeds = ctx.seeds;
    let mut state = DrawnLinePlacementState::starting_at(start_offset_s_px);
    state.measure_only = true;
    for (seed_idx, seed) in seeds.iter().enumerate() {
        if seed.line_idx != line_idx {
            continue;
        }
        // The placement itself is thrown away on purpose: this pass only exists
        // to learn where the line's cursor ENDS UP, and the real walk recomputes
        // every transform from the refined offset anyway.
        drawn_line_seed_transform(ctx, seed_idx, seed, path, &mut state);
    }
    (state.cursor.s_px - start_offset_s_px).max(0.0)
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
    cache: &'a mut SwashCache,
    contour_cache: &'a mut HashMap<(CacheKey, u32), CachedGlyphInk>,
    outline_cache: &'a mut OutlineCache,
}

/// Place one glyph seed along its line path and advance the line's state.
///
/// For `ByLineLength` spacing this reproduces the original arc-length walk
/// unchanged. For `MinimumPreviousDistance` it seeds the same arc-length
/// position, then (1) moves the glyph either way until the pair's directional
/// chord gap matches the line's ONE target gap
/// ([`straight_reference_target_gaps`]), and (2) pushes it forward if that
/// leaves it inside any windowed neighbour's clearance floor. The target is a
/// property of the LINE, not of the pair, so the mode re-spaces the run even on
/// a straight path — see the file header. Returns the
/// blit-ready transform, or `None` when [`drawn_line_drop_side`] rejects the
/// glyph: past the path end, or before its start when an end-biased alignment
/// gave the line a negative start offset. A negative position caused by a plain
/// inline `<offset>` nudge is NOT a drop — it keeps clamping onto the first path
/// point, as it always did. The before-start drop still advances the line cursor
/// (the following glyphs must keep walking); the past-end drop does not need to,
/// since every later glyph on the line is past the end as well — EXCEPT under
/// [`DrawnLinePlacementState::measure_only`], where the cursor must keep walking
/// through every drop so the run's real length can be measured past the path end.
fn drawn_line_seed_transform(
    ctx: &mut DrawnLinePlacementCtx<'_>,
    seed_idx: usize,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    state: &mut DrawnLinePlacementState,
) -> Option<DrawnLineTransform> {
    let params = ctx.params;
    let layout = ctx.layout;
    let all_seeds = ctx.seeds;
    let custom_kerning = ctx.custom_kerning;
    let line_target_gap = ctx.line_target_gaps.get(&seed.line_idx).copied();
    let advance = ctx.spacing.step_px(seed.advance_px);
    // Arc-length seed: identical to ByLineLength placement.
    let mut center_s = state.cursor.seed_center_s(seed, advance);
    match drawn_line_drop_side(center_s, state.start_offset_s_px, path.total_len_px) {
        Some(DrawnLineDropSide::BeforeStart) => {
            // An end-biased alignment pushed this glyph before the line start;
            // it goes into the same skipped-glyph warning as the end drop.
            // The cursor MUST still walk: unlike the end drop (where every later
            // glyph is past the end anyway) a frozen cursor here would collapse
            // the whole line onto one position and drop all of it.
            state.cursor.commit(all_seeds, seed, advance, center_s);
            return None;
        }
        Some(DrawnLineDropSide::PastEnd) => {
            // The cursor is deliberately left frozen: every later glyph on this
            // line is past the end as well, so they all drop anyway. A
            // MEASUREMENT pass is the exception — it exists to learn the run's
            // real length, and a frozen cursor would report the run as ending
            // at the path end however far past it the text actually reaches.
            if state.measure_only {
                state.cursor.commit(all_seeds, seed, advance, center_s);
            }
            return None;
        }
        None => {}
    }

    let use_ink = vector_line_distance_mode(params, seed.line_idx)
        == TextVectorLineDistanceMode::MinimumPreviousDistance;
    // Only the ink-distance mode needs the glyph's rasterized contour.
    let geom = if use_ink {
        seed_ink_geometry(
            seed,
            ctx.font_system,
            ctx.cache,
            ctx.contour_cache,
            ctx.outline_cache,
        )
    } else {
        None
    };

    // Straight reference coordinate of this glyph, from the single owner of that
    // recurrence. `center_s` is still the arc-length SEED here, so this glyph's
    // own correction never feeds back into the reference.
    let straight_x = state.straight.reference_x(center_s);
    let straight_ink = geom
        .as_ref()
        .map(|g| place_seed_ink_straight(params, seed, g, &layout, straight_x));

    if let (Some(g), Some(cur_straight)) = (geom.as_ref(), straight_ink.as_ref())
        && !state.neighbors.is_empty()
    {
        // 1. Give the pair the line's ONE ink gap. The target is the median of
        //    the line's straight-reference gaps, so the run reads evenly along
        //    the curve and the geometry of the curve does not decide the
        //    spacing; measured with the SAME metric on the SAME outlines.
        if state.previous_has_ink
            && let Some(target) = line_target_gap
            && let Some(prev) = state.neighbors.last()
            // A user-authored pair keeps the spacing its author gave it: the
            // override already sits in the arc-length seed, and normalizing on
            // top of it would silently overrule the user.
            && state.previous_seed_idx.is_none_or(|prev_idx| {
                custom_seed_pair_delta_px(custom_kerning, &all_seeds[prev_idx], seed).is_none()
            })
            && let Some(straight_gap) = on_path_pair_gap(&prev.straight, cur_straight)
            // A pair that does not face itself even when straight (`.` under an
            // apostrophe) has no gap to normalize; the arc-length seed stands.
            && straight_gap.is_finite()
        {
            // Reuse of the optical kerning clamp, so this path cannot drift from
            // the horizontal/vertical ones: the +/- font-size sanity bound on a
            // single pair's correction and the hard anti-collision floor on the
            // resulting gap are `optical_delta`'s, applied to the pair's own
            // straight gap.
            let target_gap =
                straight_gap + optical_delta(straight_gap, target, seed.glyph.font_size);
            let range =
                advance.clamp(ON_PATH_INK_SEARCH_MIN_RANGE_PX, ON_PATH_INK_SEARCH_MAX_RANGE_PX);
            // A backward move may not reach past the line's run start: that
            // would either drop a glyph the alignment meant to keep or pile one
            // onto the first path point. `.min(center_s)` keeps the interval
            // valid when the seed itself already sits before that bound.
            let min_s = (center_s - range)
                .max(state.start_offset_s_px.max(0.0))
                .min(center_s);
            let max_s = (center_s + range).min(path.total_len_px);
            let solved = solve_directional_gap_center_s(
                center_s,
                target_gap,
                min_s,
                max_s,
                &prev.world,
                |s| place_seed_ink_at(params, seed, g, path, s, &layout),
            );
            let Some(solved) = solved else {
                // The path could not be sampled at a trial position, so the
                // glyph is dropped. A measurement pass must still walk its
                // cursor past it; see [`DrawnLinePlacementState::measure_only`].
                if state.measure_only {
                    state.cursor.commit(all_seeds, seed, advance, center_s);
                }
                return None;
            };
            center_s = solved;
        }

        // 2. Safety net: the chord metric cannot see a diagonal approach, and a
        //    sharp turn also brings glyphs older than the predecessor within
        //    reach. Each floor is capped by the straight reference's own
        //    clearance, so an authored overlap is never undone.
        let mut floors = [0.0f32; ON_PATH_INK_NEIGHBOR_WINDOW];
        for (floor, neighbor) in floors.iter_mut().zip(state.neighbors.iter()) {
            *floor = pair_clearance_floor(&neighbor.straight.contour, &cur_straight.contour);
        }
        let cleared = find_clearance_center_s(
            path,
            center_s,
            &state.neighbors,
            &floors[..state.neighbors.len()],
            |s| place_seed_ink_at(params, seed, g, path, s, &layout).map(|ink| ink.contour),
        );
        let Some(cleared) = cleared else {
            // No position up to the path end clears every neighbour, so the
            // glyph is dropped exactly as the arc-length walk would drop it. A
            // measurement pass must still walk its cursor past it.
            if state.measure_only {
                state.cursor.commit(all_seeds, seed, advance, center_s);
            }
            return None;
        };
        center_s = cleared;
    }

    // A backward move is bounded by the line's run start above, so only
    // `PastEnd` is reachable here; the shared helper keeps the two guards from
    // ever disagreeing.
    if drawn_line_drop_side(center_s, state.start_offset_s_px, path.total_len_px).is_some() {
        if state.measure_only {
            state.cursor.commit(all_seeds, seed, advance, center_s);
        }
        return None;
    }
    // The cursor walks past the glyph's FINAL (corrected) center, which is what
    // makes the run's real length differ from the alignment pre-pass estimate.
    state.cursor.commit(all_seeds, seed, advance, center_s);
    let transform = drawn_line_transform_at(params, seed, path, center_s, &layout)?;

    if use_ink {
        // The reference walk follows EVERY placed glyph, inkless ones included,
        // so a space keeps the straight reference aligned with the real walk.
        state.straight.commit(straight_x, center_s);
        state.previous_seed_idx = Some(seed_idx);
        match (geom.as_ref(), straight_ink) {
            (Some(g), Some(straight)) => {
                // Store the glyph in both frames at its FINAL position, so the
                // next glyph measures against the very ink the blit will draw
                // and against the reference that ink is judged by.
                let world = place_seed_ink_for_transform(seed, g, &layout, &transform);
                state.neighbors.push(PlacedNeighbor { world, straight });
                if state.neighbors.len() > ON_PATH_INK_NEIGHBOR_WINDOW {
                    state.neighbors.remove(0);
                }
                state.previous_has_ink = true;
            }
            _ => {
                // Empty/space glyph: it carries no ink, so the next glyph gets
                // no directional target from it. The window is deliberately
                // KEPT — the next word still must not collide with this one.
                state.previous_has_ink = false;
            }
        }
    }

    Some(transform)
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

impl DrawnLinePlacementState {
    /// Fresh state for a line whose run starts at `start_offset_s_px` (its
    /// alignment offset): the cursor and the recorded start offset are the same
    /// value, and no glyph has been placed yet.
    #[must_use]
    fn starting_at(start_offset_s_px: f32) -> Self {
        Self {
            cursor: LineArcCursor::starting_at(start_offset_s_px),
            start_offset_s_px,
            ..Self::default()
        }
    }
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

/// Build (or reuse) the ink geometry for a seed's glyph.
///
/// The ink contour is derived from the glyph's true font outline (cached in
/// `contour_cache` on a miss). Nothing about the ink is measured from the
/// rasterized bitmap: the bitmap is consulted ONLY for its placement box
/// (`placement_left`/`placement_top`, size), which pins the contour to the exact
/// pixels the outline rasterizer draws on. Returns `None` for glyphs with no
/// bitmap (e.g. spaces), zero-size placement, or no monochrome outline (color
/// glyphs), which callers treat as "no ink" and handle with the arc-length
/// fallback.
fn seed_ink_geometry(
    seed: &FormulaGlyphSeed,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    contour_cache: &mut HashMap<(CacheKey, u32), CachedGlyphInk>,
    outline_cache: &mut OutlineCache,
) -> Option<SeedInkGeometry> {
    let physical = seed.glyph.physical(
        (
            seed.origin_x + seed.glyph_offset_px[0],
            seed.origin_y + seed.glyph_offset_px[1],
        ),
        1.0,
    );
    let key = physical.cache_key;
    // The ink cache is per faux-bold variant (same bits as `OutlineKey`), so a
    // faux span's offset contour never aliases the plain one within a render.
    let ink_key = (key, seed.faux.bold_key_bits());

    // Bitmap placement, copied out before the image borrow of `cache` ends.
    // Only the PLACEMENT is read here: the ink itself comes from the outline
    // below, and the spacing target is measured on that contour, so no
    // bitmap-derived ink extent is needed.
    let placement_left;
    let placement_top;
    let glyph_w;
    let glyph_h;
    {
        let Some(image) = cache.get_image(font_system, key) else {
            return None;
        };
        let gw = image.placement.width as usize;
        let gh = image.placement.height as usize;
        if gw == 0 || gh == 0 {
            return None;
        }
        placement_left = image.placement.left as f32;
        placement_top = image.placement.top as f32;
        glyph_w = gw as f32;
        glyph_h = gh as f32;
    }

    // Trace the ink contour from the true outline once per distinct glyph key
    // (and faux-bold variant). A glyph with no fillable monochrome outline is
    // treated as "no ink".
    if let std::collections::hash_map::Entry::Vacant(entry) = contour_cache.entry(ink_key) {
        let outline = resolve_glyph_outline(seed, font_system, outline_cache)?;
        let contour = glyph_contour_from_outline(&outline, CONTOUR_SIMPLIFY_TOLERANCE_PX);
        entry.insert(CachedGlyphInk {
            contour,
            glyph_w,
            glyph_h,
            placement_left,
            placement_top,
        });
    }

    let cached = contour_cache.get(&ink_key)?;
    let src_left = physical.x as f32 + placement_left;
    let src_top = physical.y as f32 - placement_top;
    let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
        seed.glyph_scale
            .scaled_rect(src_left, src_top, glyph_w, glyph_h);
    Some(SeedInkGeometry {
        contour: cached.contour.clone(),
        glyph_w: cached.glyph_w,
        glyph_h: cached.glyph_h,
        placement_left: cached.placement_left,
        placement_top: cached.placement_top,
        scaled_height,
        subpixel: glyph_subpixel_offset(key),
    })
}

/// On-path transform (position + rotation) for a candidate arc-length position.
///
/// Samples the path, then hands the point and tangent to
/// [`on_path_transform_from_sample`], which owns the placement expression itself
/// (normal offset, tangent/static rotation, flip, per-glyph rotation and the
/// rotated glyph offset) and is shared with the straight reference layout. Every
/// blit transform and every trial placement of the ink search comes from here,
/// so a measured contour can never sit anywhere else than the drawn one.
/// Returns `None` if the path cannot be sampled.
fn drawn_line_transform_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<DrawnLineTransform> {
    let (center_x, center_y, tangent_x, tangent_y) =
        sample_drawn_line_path_for_direction(path, center_s)?;
    Some(on_path_transform_from_sample(
        params, seed, layout, center_x, center_y, tangent_x, tangent_y,
    ))
}

/// The on-path transform for an ALREADY SAMPLED path point and tangent.
///
/// Split out of [`drawn_line_transform_at`] so the straight reference layout
/// ([`straight_reference_transform`]) composes the normal offset, rotation,
/// flip and glyph offset through the very same expression instead of a second
/// copy of it. The tangent need not be unit length; it is normalized here.
// The sample is an irreducible list of independent scalars (point + tangent);
// bundling them into a one-off struct would not add clarity.
#[allow(clippy::too_many_arguments)]
fn on_path_transform_from_sample(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    layout: &CustomLineLayoutSettings,
    center_x: f32,
    center_y: f32,
    tangent_x: f32,
    tangent_y: f32,
) -> DrawnLineTransform {
    let tangent_len = (tangent_x * tangent_x + tangent_y * tangent_y)
        .sqrt()
        .max(1e-6);
    let tangent_x = tangent_x / tangent_len;
    let tangent_y = tangent_y / tangent_len;
    let normal_offset = layout.normal_offset_px;
    let center_x = center_x - tangent_y * normal_offset;
    let center_y = center_y + tangent_x * normal_offset;
    let rotation_rad = (if layout.use_tangent_rotation {
        tangent_y.atan2(tangent_x)
    } else {
        layout.static_rotation_rad
    }) + vector_line_flip_rotation(params, seed.line_idx)
        + seed.extended_offset.glyph_rotation_rad;
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let center_x = center_x + seed.glyph_offset_px[0] * cos_a - seed.glyph_offset_px[1] * sin_a;
    let center_y = center_y + seed.glyph_offset_px[0] * sin_a + seed.glyph_offset_px[1] * cos_a;
    DrawnLineTransform {
        center_x,
        center_y,
        rotation_rad,
    }
}

/// The transform this glyph would get on a STRAIGHT line along +x, at distance
/// `x` from that line's origin.
///
/// This is the reference layout `MinimumPreviousDistance` measures its target
/// against (see [`drawn_line_seed_transform`]). It runs through the same
/// [`on_path_transform_from_sample`] as the real placement, with the path point
/// `(x, 0)` and the constant tangent `(1, 0)`, so every other setting — normal
/// offset, flip, static vs. tangent rotation, per-glyph rotation and offset —
/// enters the reference exactly as it enters the real placement. On a straight
/// path the reference therefore differs from the real placement by a rigid
/// transform only, so every gap measured in the reference is exactly the gap the
/// arc-length seed would draw there — which is what makes the reference the
/// honest place to read the line's typography off, NOT a claim that the mode is
/// a no-op on a straight line. It is not: the pairs are still normalized onto
/// the line's ONE median gap (see the file header).
fn straight_reference_transform(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    layout: &CustomLineLayoutSettings,
    x: f32,
) -> DrawnLineTransform {
    on_path_transform_from_sample(params, seed, layout, x, 0.0, 1.0, 0.0)
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

/// Place a seed's cached ink for an already-resolved on-path transform.
///
/// The one place that turns (`SeedInkGeometry`, transform) into a
/// [`PlacedGlyphInk`]: the candidate placements of the search, the straight
/// reference placements and the final stored placement all go through it, so
/// they cannot drift apart.
fn place_seed_ink_for_transform(
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    layout: &CustomLineLayoutSettings,
    transform: &DrawnLineTransform,
) -> PlacedGlyphInk {
    let (center_x, center_y) = drawn_line_glyph_destination_center_raw(
        transform,
        geom.scaled_height,
        geom.placement_top * seed.glyph_scale.height_mul,
        layout.line_placement_frac,
        layout.line_placement_reference,
        layout.ascent_scaled,
    );
    let contour = placed_contour_for_transform(
        &geom.contour,
        geom.placement_left,
        geom.placement_top,
        geom.glyph_w,
        geom.glyph_h,
        seed.glyph_scale.width_mul,
        seed.glyph_scale.height_mul,
        seed.faux.shear_x,
        geom.scaled_height,
        layout.line_placement_frac,
        layout.line_placement_reference,
        layout.ascent_scaled,
        geom.subpixel,
        transform,
    );
    PlacedGlyphInk {
        contour,
        center: [center_x, center_y],
    }
}

/// Place a glyph's cached ink into world space for a candidate arc-length
/// position.
///
/// Combines [`drawn_line_transform_at`] with [`place_seed_ink_for_transform`]
/// so a search closure produces the exact ink the blit would draw at
/// `center_s`. Returns `None` when the path cannot be sampled.
fn place_seed_ink_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<PlacedGlyphInk> {
    let transform = drawn_line_transform_at(params, seed, path, center_s, layout)?;
    Some(place_seed_ink_for_transform(seed, geom, layout, &transform))
}

/// Place a glyph's cached ink in the STRAIGHT REFERENCE layout at reference
/// coordinate `x` (see [`straight_reference_transform`]).
///
/// Infallible: the reference has no path to sample.
fn place_seed_ink_straight(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    layout: &CustomLineLayoutSettings,
    x: f32,
) -> PlacedGlyphInk {
    let transform = straight_reference_transform(params, seed, layout, x);
    place_seed_ink_for_transform(seed, geom, layout, &transform)
}

/// Measuring frame of a pair of placed glyphs: the CHORD between their ink
/// placement centers, with `forward` pointing from `prev` to `cur`.
///
/// The chord — not either glyph's path tangent — because the measure must be
/// SYMMETRIC under swapping the pair: measuring "prev against cur" and "cur
/// against prev" has to give one number, or the walk order would change the
/// spacing. On a circular arc the chord is exactly the bisector of the two
/// tangents, and unlike the bisector it does not degenerate as the turn angle
/// grows.
///
/// Returns `None` when the two centers coincide (a path that doubles back
/// exactly onto itself): the pair then has no advance direction at all, and the
/// caller treats that as "unmeasurably close" rather than dividing by zero.
fn pair_chord_axis(prev_center: [f32; 2], cur_center: [f32; 2]) -> Option<GapAxis> {
    let dx = cur_center[0] - prev_center[0];
    let dy = cur_center[1] - prev_center[1];
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < PAIR_CHORD_MIN_LEN_PX {
        return None;
    }
    let forward = [dx / len, dy / len];
    // Perpendicular of `forward`, unit length by construction.
    Some(GapAxis::new(forward, [-forward[1], forward[0]]))
}

/// Directional ink whitespace of a placed pair, measured on its own chord.
///
/// Delegates to `pair_gap::directional_pair_gap`, the crate's single owner of
/// the pair-gap measurement (the third consumer, next to horizontal and
/// vertical optical kerning). Returns `None` only for the degenerate chord of
/// [`pair_chord_axis`]; the metric's own `f32::INFINITY` ("the pair does not
/// face itself at all") is passed through as `Some(INFINITY)`.
fn on_path_pair_gap(prev: &PlacedGlyphInk, cur: &PlacedGlyphInk) -> Option<f32> {
    let axis = pair_chord_axis(prev.center, cur.center)?;
    Some(directional_pair_gap(&prev.contour, &cur.contour, axis))
}

/// Whether `cur` keeps at least `floor` omnidirectional clearance from `prev`.
///
/// `floor <= 0.0` is always satisfied (an authored overlap is respected, not
/// undone). The AABB lower bound short-circuits the common case.
fn clears_floor(prev: &PlacedContour, cur: &PlacedContour, floor: f32) -> bool {
    if floor <= 0.0 {
        return true;
    }
    if placed_aabb_gap(prev, cur) >= floor {
        return true;
    }
    min_placed_distance(prev, cur) >= floor
}

/// Omnidirectional clearance a pair must keep on the curve, derived from the
/// straight reference layout.
///
/// [`ON_PATH_INK_CLEARANCE_FLOOR_PX`], except that a pair the straight
/// reference already draws TIGHTER than the floor keeps its own straight-line
/// clearance instead. Without that cap the floor would push apart pairs the
/// font (or a user-authored negative kerning pair) deliberately draws touching,
/// which is exactly what the mode must not do.
fn pair_clearance_floor(straight_prev: &PlacedContour, straight_cur: &PlacedContour) -> f32 {
    if placed_aabb_gap(straight_prev, straight_cur) >= ON_PATH_INK_CLEARANCE_FLOOR_PX {
        return ON_PATH_INK_CLEARANCE_FLOOR_PX;
    }
    ON_PATH_INK_CLEARANCE_FLOOR_PX.min(min_placed_distance(straight_prev, straight_cur))
}

/// Map an outline-frame contour into world space using the blit's exact
/// geometry.
///
/// The contour lives in the outline's pen-relative y-down pixel frame (the same
/// frame the outline rasterizer consumes), so this resolves the glyph's world
/// destination center from the on-path transform and reuses
/// [`glyph_outline_transform`] — the single source of truth for the pivot — so
/// the measured contour lands on the exact pixels the outline is rasterized to.
// The geometry is an irreducible list of independent scalars (bitmap
// placement/size, per-axis scale, scaled ink height, line placement, transform);
// bundling them into a one-off struct would not add clarity.
#[allow(clippy::too_many_arguments)]
fn placed_contour_for_transform(
    contour: &GlyphContour,
    placement_left: f32,
    placement_top: f32,
    glyph_w: f32,
    glyph_h: f32,
    width_mul: f32,
    height_mul: f32,
    shear_x: f32,
    scaled_height: f32,
    line_frac: f32,
    reference: LinePlacementReference,
    ascent_scaled: f32,
    subpixel: [f32; 2],
    transform: &DrawnLineTransform,
) -> PlacedContour {
    // Same per-glyph scaled top bearing the placement uses to convert baseline->center.
    let placement_top_scaled = placement_top * height_mul;
    let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
        transform,
        scaled_height,
        placement_top_scaled,
        line_frac,
        reference,
        ascent_scaled,
    );
    // Same subpixel-corrected pivot (and faux-italic shear) the outline
    // rasterizer uses, so the measured contour matches the drawn ink exactly.
    let glyph_transform = glyph_outline_transform(
        dst_center_x,
        dst_center_y,
        transform.rotation_rad,
        placement_left,
        placement_top,
        glyph_w,
        glyph_h,
        width_mul,
        height_mul,
        subpixel,
        shear_x,
    );
    glyph_transform.place_contour(contour)
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

fn custom_line_layout_settings(
    params: &TextRenderParams,
    line_placement_frac: f32,
    ascent_scaled: f32,
) -> CustomLineLayoutSettings {
    match params.text_layout_mode {
        TextLayoutMode::CustomRasterLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.drawn_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.drawn_lines_layout.static_rotation_rad,
            normal_offset_px: params.drawn_lines_layout.normal_offset_px,
            letter_spacing_mul: params.drawn_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.drawn_lines_layout.letter_spacing_px,
            line_placement_frac,
            // Raster lines keep the legacy per-glyph anchoring.
            line_placement_reference: LinePlacementReference::GlyphHeight,
            ascent_scaled,
        },
        TextLayoutMode::CustomVectorLines => CustomLineLayoutSettings {
            use_tangent_rotation: params.vector_lines_layout.use_tangent_rotation,
            static_rotation_rad: params.vector_lines_layout.static_rotation_rad,
            normal_offset_px: params.vector_lines_layout.normal_offset_px,
            letter_spacing_mul: params.vector_lines_layout.letter_spacing_mul,
            letter_spacing_px: params.vector_lines_layout.letter_spacing_px,
            line_placement_frac,
            line_placement_reference: params.line_placement_reference,
            ascent_scaled,
        },
        TextLayoutMode::Normal | TextLayoutMode::Formula | TextLayoutMode::Shape => {
            CustomLineLayoutSettings {
                use_tangent_rotation: true,
                static_rotation_rad: 0.0,
                normal_offset_px: 0.0,
                letter_spacing_mul: 1.0,
                letter_spacing_px: 0.0,
                line_placement_frac,
                line_placement_reference: LinePlacementReference::GlyphHeight,
                ascent_scaled,
            }
        }
    }
}

fn vector_line_distance_mode(
    params: &TextRenderParams,
    line_idx: usize,
) -> TextVectorLineDistanceMode {
    if params.text_layout_mode != TextLayoutMode::CustomVectorLines {
        return TextVectorLineDistanceMode::ByLineLength;
    }
    params
        .vector_lines_layout
        .lines
        .get(line_idx)
        .map(|line| line.distance_mode)
        .unwrap_or(TextVectorLineDistanceMode::ByLineLength)
}

fn vector_line_flip_rotation(params: &TextRenderParams, line_idx: usize) -> f32 {
    if params.text_layout_mode != TextLayoutMode::CustomVectorLines {
        return 0.0;
    }
    if params
        .vector_lines_layout
        .lines
        .get(line_idx)
        .is_some_and(|line| line.flip_text)
    {
        std::f32::consts::PI
    } else {
        0.0
    }
}

fn is_last_seed_in_offset_span_on_line(
    seeds: &[FormulaGlyphSeed],
    seed: &FormulaGlyphSeed,
) -> bool {
    let Some(span_range) = seed.offset_span_range else {
        return true;
    };
    !seeds.iter().any(|other| {
        other.line_idx == seed.line_idx
            && other.style_offset > seed.style_offset
            && other.offset_span_range == Some(span_range)
    })
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

fn offset_group_key(seed: &FormulaGlyphSeed) -> Option<OffsetGroupKey> {
    let (start, end) = seed.offset_span_range?;
    Some(OffsetGroupKey {
        line_idx: seed.line_idx,
        start,
        end,
    })
}

fn apply_formula_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &mut [FormulaGlyphTransform],
) {
    let groups = formula_group_rotations(seeds, transforms);
    for (seed, transform) in seeds.iter().zip(transforms.iter_mut()) {
        let Some(group) = offset_group_key(seed).and_then(|key| groups.get(&key).copied()) else {
            continue;
        };
        if group.count <= 1 || group.rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let (x, y) = rotate_point_around(
            transform.center_x,
            transform.center_y,
            group.center_x,
            group.center_y,
            group.rotation_rad,
        );
        transform.center_x = x;
        transform.center_y = y;
        transform.rotation_rad += group.rotation_rad;
    }
}

fn formula_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &[FormulaGlyphTransform],
) -> HashMap<OffsetGroupKey, OffsetGroupRotation> {
    let mut groups = HashMap::<OffsetGroupKey, OffsetGroupRotation>::new();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        if seed.extended_offset.group_rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let Some(key) = offset_group_key(seed) else {
            continue;
        };
        let entry = groups.entry(key).or_insert(OffsetGroupRotation {
            center_x: 0.0,
            center_y: 0.0,
            rotation_rad: seed.extended_offset.group_rotation_rad,
            count: 0,
        });
        entry.center_x += transform.center_x;
        entry.center_y += transform.center_y;
        entry.count += 1;
    }
    for group in groups.values_mut() {
        let count = group.count.max(1) as f32;
        group.center_x /= count;
        group.center_y /= count;
    }
    groups
}

fn apply_drawn_line_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &mut [Option<DrawnLineTransform>],
) {
    let groups = drawn_line_group_rotations(seeds, transforms);
    for (seed, transform) in seeds.iter().zip(transforms.iter_mut()) {
        let Some(transform) = transform else {
            continue;
        };
        let Some(group) = offset_group_key(seed).and_then(|key| groups.get(&key).copied()) else {
            continue;
        };
        if group.count <= 1 || group.rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let (x, y) = rotate_point_around(
            transform.center_x,
            transform.center_y,
            group.center_x,
            group.center_y,
            group.rotation_rad,
        );
        transform.center_x = x;
        transform.center_y = y;
        transform.rotation_rad += group.rotation_rad;
    }
}

fn drawn_line_group_rotations(
    seeds: &[FormulaGlyphSeed],
    transforms: &[Option<DrawnLineTransform>],
) -> HashMap<OffsetGroupKey, OffsetGroupRotation> {
    let mut groups = HashMap::<OffsetGroupKey, OffsetGroupRotation>::new();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        if seed.extended_offset.group_rotation_rad.abs() <= f32::EPSILON {
            continue;
        }
        let Some(transform) = transform else {
            continue;
        };
        let Some(key) = offset_group_key(seed) else {
            continue;
        };
        let entry = groups.entry(key).or_insert(OffsetGroupRotation {
            center_x: 0.0,
            center_y: 0.0,
            rotation_rad: seed.extended_offset.group_rotation_rad,
            count: 0,
        });
        entry.center_x += transform.center_x;
        entry.center_y += transform.center_y;
        entry.count += 1;
    }
    for group in groups.values_mut() {
        let count = group.count.max(1) as f32;
        group.center_x /= count;
        group.center_y /= count;
    }
    groups
}

fn rotate_point_around(
    x: f32,
    y: f32,
    center_x: f32,
    center_y: f32,
    rotation_rad: f32,
) -> (f32, f32) {
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let rel_x = x - center_x;
    let rel_y = y - center_y;
    (
        center_x + rel_x * cos_a - rel_y * sin_a,
        center_y + rel_x * sin_a + rel_y * cos_a,
    )
}

fn sample_drawn_line_path_for_direction(
    path: &DrawnLinePath,
    target_s: f32,
) -> Option<(f32, f32, f32, f32)> {
    let forward = line_path_forward_for_direction(path);
    let sample_s = if forward {
        target_s
    } else {
        path.total_len_px - target_s
    };
    let (x, y, tangent_x, tangent_y) = sample_drawn_line_path(path, sample_s)?;
    if forward {
        Some((x, y, tangent_x, tangent_y))
    } else {
        Some((x, y, -tangent_x, -tangent_y))
    }
}

fn line_path_forward_for_direction(path: &DrawnLinePath) -> bool {
    if !path.honor_text_direction {
        return true;
    }
    let Some(first) = path.points.first() else {
        return true;
    };
    let Some(last) = path.points.last() else {
        return true;
    };
    let dx = last.x - first.x;
    match path.direction {
        TextVectorLineTextDirection::LeftToRight => dx >= 0.0,
        TextVectorLineTextDirection::RightToLeft => dx < 0.0,
    }
}

/// Move a glyph along the path, in EITHER direction, to the nearest position
/// whose directional chord gap to `prev` equals `target_gap`.
///
/// `place_at(s)` must return the current glyph's ink at candidate arc-length `s`
/// (or `None` when `s` cannot be sampled — the glyph is then dropped). The
/// search is confined to `min_s..=max_s`, which the caller derives from the
/// glyph's own advance and from the line's start offset, so a backward move can
/// never push a glyph before its line's run start and change what
/// [`drawn_line_drop_side`] does.
///
/// # Why two directions
/// A bend shortens the chord between two glyphs on its CONCAVE side (they must
/// move apart) and lengthens it on the CONVEX side (they must move together).
/// A forward-only search fixes the first case and leaves the second one
/// visibly loose, which is what made the gaps on a wave alternate between
/// touching and gaping.
///
/// # Non-monotonicity — deliberately not assumed
/// The gap is NOT a monotone function of `s`: a line path is a POLYLINE, so a
/// glyph crossing a node changes its rotation in a step, and a non-convex glyph
/// changes which feature faces the neighbour. A plain bisection over the whole
/// range would therefore be unsound. Instead the sign of the seed's error picks
/// the direction, a coarse scan walks OUTWARD from the seed and stops at the
/// FIRST sign change it meets, and only that one bracket is bisected. The
/// result is the crossing NEAREST to the arc-length seed, so a distant
/// non-monotone region can never pull the glyph across it; whether further
/// crossings exist is irrelevant, because the minimal correction is the wanted
/// one. Within one bracket (at most one coarse step) the sign change is real by
/// construction, so bisection converges on a crossing — or, where the metric
/// jumps (the pair's overlap band vanishing), on the jump, which is the closest
/// position that still faces the neighbour at all.
///
/// Returns the seed unchanged when the error is already inside
/// [`ON_PATH_INK_GAP_EPSILON_PX`] — the pair already carries the line's target
/// gap, which a straight line hits only for the pairs that happen to sit on the
/// line's median, not for every pair — and also when no sign change exists
/// within the range (the geometry cannot deliver the target; the arc-length walk
/// stands).
fn solve_directional_gap_center_s<F>(
    seed_s: f32,
    target_gap: f32,
    min_s: f32,
    max_s: f32,
    prev: &PlacedGlyphInk,
    mut place_at: F,
) -> Option<f32>
where
    F: FnMut(f32) -> Option<PlacedGlyphInk>,
{
    // Signed error at `s`. A degenerate chord (`on_path_pair_gap` -> None)
    // counts as "unmeasurably close" so the search pushes the glyph forward
    // until the pair has a direction again.
    let mut error_at = |s: f32| -> Option<f32> {
        let cur = place_at(s)?;
        Some(match on_path_pair_gap(prev, &cur) {
            Some(gap) => gap - target_gap,
            None => f32::NEG_INFINITY,
        })
    };

    let seed_error = error_at(seed_s)?;
    if seed_error.abs() <= ON_PATH_INK_GAP_EPSILON_PX {
        return Some(seed_s);
    }
    // Too close -> the gap grows by moving forward; too far -> backward.
    let forward = seed_error < 0.0;
    let limit = if forward { max_s } else { min_s };
    let span = limit - seed_s;
    if span.abs() <= f32::EPSILON {
        return Some(seed_s);
    }
    // `span` carries the direction, so the walk below needs no sign handling
    // and lands exactly on `limit` at the last sample.
    let step = span / ON_PATH_INK_SCAN_SAMPLES as f32;

    let mut bracket_near = seed_s;
    for i in 1..=ON_PATH_INK_SCAN_SAMPLES {
        let s = seed_s + step * i as f32;
        let error = error_at(s)?;
        let crossed = if forward { error >= 0.0 } else { error <= 0.0 };
        if crossed {
            // Order the bracket so `lo` is the too-close side and `hi` the
            // clearing side, whichever direction the scan walked.
            let (mut lo, mut hi) = if forward {
                (bracket_near, s)
            } else {
                (s, bracket_near)
            };
            for _ in 0..ON_PATH_INK_BISECTION_STEPS {
                let mid = lo + (hi - lo) * 0.5;
                if error_at(mid)? >= 0.0 {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            // The clearing side: erring towards slightly too much space rather
            // than towards ink that touches.
            return Some(hi);
        }
        bracket_near = s;
    }
    Some(seed_s)
}

/// Find the smallest `center_s >= start_s` whose placed contour keeps every
/// windowed neighbour's clearance floor.
///
/// `neighbors` and `floors` are index-aligned; the predicate is
/// `clears_floor(neighbor, current, floor)` for all of them. This is the SAFETY
/// NET pass of `MinimumPreviousDistance`, run after
/// [`solve_directional_gap_center_s`]: the directional metric is blind to a
/// diagonal approach outside the pair's overlap band, and on a sharp turn a
/// glyph can also reach the one BEFORE its predecessor. Forward-only on
/// purpose — a backward move could only tighten a clearance that is already too
/// small.
///
/// Every floor is capped by what the straight reference itself achieves
/// ([`pair_clearance_floor`]), so this pass can never push a pair further apart
/// than straight-line typography already keeps it. It is NOT guaranteed to be a
/// no-op on a straight line: `start_s` here is the position the directional
/// search already moved the glyph to, and a pair pulled closed toward the line's
/// median can end up below the clearance the reference had.
///
/// Structure: early-out if the seed already clears; otherwise a coarse forward
/// scan brackets the first passing position, refined by fixed bisection.
/// Returns `None` if no position up to the path end clears, so the glyph is
/// dropped like the arc-length walk would drop it.
fn find_clearance_center_s<F>(
    path: &DrawnLinePath,
    start_s: f32,
    neighbors: &[PlacedNeighbor],
    floors: &[f32],
    mut current_at: F,
) -> Option<f32>
where
    F: FnMut(f32) -> Option<PlacedContour>,
{
    let clears = |placed: &PlacedContour| {
        neighbors
            .iter()
            .zip(floors)
            .all(|(neighbor, floor)| clears_floor(&neighbor.world.contour, placed, *floor))
    };

    if clears(&current_at(start_s)?) {
        return Some(start_s);
    }

    // Step size scales with the largest floor so a loose floor scans coarsely
    // and a tight one finely; clamped to keep the scan bounded on any path.
    let max_floor = floors.iter().copied().fold(0.0f32, f32::max);
    let scan_step = (max_floor.max(1.0) / 8.0).clamp(0.5, 4.0);
    let mut low = start_s;
    let mut high = (start_s + scan_step).min(path.total_len_px);
    loop {
        if clears(&current_at(high)?) {
            break;
        }
        if high >= path.total_len_px {
            // Ran off the end without ever clearing: drop the glyph.
            return None;
        }
        low = high;
        high = (high + scan_step).min(path.total_len_px);
    }

    for _ in 0..18 {
        let mid = low + (high - low) * 0.5;
        if clears(&current_at(mid)?) {
            high = mid;
        } else {
            low = mid;
        }
    }
    Some(high)
}

fn sample_drawn_line_path(path: &DrawnLinePath, target_s: f32) -> Option<(f32, f32, f32, f32)> {
    let points = path.points.as_slice();
    let first = points.first().copied()?;
    if target_s <= 0.0 || points.len() == 1 {
        let next = points.get(1).copied().unwrap_or(first);
        return Some((first.x, first.y, next.x - first.x, next.y - first.y));
    }
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if target_s > b.arc_len_px {
            continue;
        }
        let segment_len = (b.arc_len_px - a.arc_len_px).max(1e-6);
        let t = ((target_s - a.arc_len_px) / segment_len).clamp(0.0, 1.0);
        return Some((
            a.x + (b.x - a.x) * t,
            a.y + (b.y - a.y) * t,
            b.x - a.x,
            b.y - a.y,
        ));
    }
    let last = points.last().copied()?;
    let prev = points
        .get(points.len().saturating_sub(2))
        .copied()
        .unwrap_or(last);
    Some((last.x, last.y, last.x - prev.x, last.y - prev.y))
}

// Formula rendering needs explicit access to the shaped buffer, inline spans and raster bounds.
#[allow(clippy::too_many_arguments)]
fn render_text_with_formula_layout_once(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    faux_face_baseline: FauxFaceBaseline,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    custom_kerning: &CustomKerningMap,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_extra_spacing_table: &[f32],
    // The GLOBAL default extra line spacing the caller built the table around.
    // Re-deriving it as `line_extra_spacing_table.first()` is wrong: with the
    // grow-only inline height room, entry 0 can carry extra px that would then be
    // SUBTRACTED from every later gap in the `has_inline_size_overrides` branch
    // of `compute_horizontal_line_baselines`, pulling lines closer.
    default_extra_line_spacing_px: f32,
    render_margin_pad: u32,
    line_placement_frac: f32,
) -> Result<RenderedTextImage, String> {
    let width_px = params.width_px.max(1);
    let formula_program = FormulaProgramBundle::compile(&params.formula_layout)?;
    let mut cache = SwashCache::new();
    // Per-render outline cache for the vector composite pass (color glyphs fall
    // back to the bitmap blit, so they are never extracted here).
    let mut outline_cache = OutlineCache::new();
    // Reused per-glyph rasterizer buffers for the composite pass (see `RasterScratch`).
    let mut raster_scratch = RasterScratch::new();
    // Coverage->alpha transfer table for the selected AA mode, built once per render.
    let aa_lut = build_aa_lut(params.anti_aliasing);
    // Optional extra-info (mean/median centers). A FRESH accumulator per `_once`
    // call (the retry loop re-runs this function from scratch), so the stored
    // extras always match the accepted image. Inactive by default -> a true no-op.
    let mut extra_acc = ExtraInfoAccumulator::new(params.extra_info);
    let extra_active = extra_acc.is_active();
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table,
        has_inline_size_overrides,
    );
    let mut seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        &mut outline_cache,
        buffer,
        attrs,
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        custom_kerning,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
    );
    if seeds.is_empty() {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let default_advance = (font_size_px * 0.5).max(1.0);
    let spacing = OnPathStepSpacing::for_formula(
        params.formula_layout.letter_spacing_mul,
        params.formula_layout.letter_spacing_px,
        default_advance,
    );
    let mut centers = Vec::<f32>::with_capacity(seeds.len());
    let mut total_advance = 0.0f32;
    for seed in &seeds {
        let advance = spacing.step_px(seed.advance_px);
        centers.push(total_advance + advance * 0.5);
        total_advance += advance;
    }
    let total_advance = total_advance.max(1.0);
    let glyph_count = seeds.len();
    // One continuous arc-length accumulator spans the WHOLE run here (it is not
    // reset per line), so the alignment bias is taken from the block-level
    // `params.align`; per-line inline overrides cannot be honored on this path.
    let formula_align_fraction = on_path_align_fraction(params.align, 0.5);

    let mut transforms = Vec::<FormulaGlyphTransform>::with_capacity(glyph_count);
    let mut formula_line_shifts = HashMap::<usize, f32>::new();
    for (idx, seed) in seeds.iter().enumerate() {
        let line_shift = formula_line_shifts
            .get(&seed.line_idx)
            .copied()
            .unwrap_or(0.0);
        let center_s = centers[idx] + line_shift + seed.extended_offset.line_px;
        let line_t = if seed.glyphs_in_line <= 1 {
            0.0
        } else {
            seed.glyph_idx_in_line as f32 / seed.glyphs_in_line.saturating_sub(1) as f32
        };
        let eval = FormulaEvalInput {
            t01: 0.0,
            i: idx as f32,
            n: glyph_count as f32,
            s: center_s,
            line: seed.line_idx as f32,
            line_t,
            line_n: seed.glyphs_in_line.max(1) as f32,
            width_px: width_px as f32,
            font_size_px,
            user_vars: &params.formula_layout.vars,
        };
        let arc_samples =
            build_formula_arc_length_table(&formula_program, &params.formula_layout, &eval)?;
        let curve_len_px = arc_samples
            .last()
            .map(|sample| sample.arc_len_px)
            .unwrap_or(0.0)
            .max(0.0);
        let target_arc_len_px = map_formula_target_arc_length(
            center_s,
            total_advance,
            curve_len_px,
            formula_align_fraction,
        );
        let mapped_t01 = formula_t01_for_arc_length(arc_samples.as_slice(), target_arc_len_px);
        let transform =
            formula_program.evaluate_transform_at_t01(&params.formula_layout, &eval, mapped_t01)?;
        transforms.push(FormulaGlyphTransform {
            center_x: transform.center_x,
            center_y: transform.center_y,
            rotation_rad: transform.rotation_rad + seed.extended_offset.glyph_rotation_rad,
        });
        if seed.extended_offset.shift_following && is_last_seed_in_offset_span_on_line(&seeds, seed)
        {
            *formula_line_shifts.entry(seed.line_idx).or_insert(0.0) +=
                seed.extended_offset.line_px;
        }
    }
    apply_formula_group_rotations(seeds.as_slice(), transforms.as_mut_slice());
    // Global block rotation (vector level): rotate every on-path glyph rigidly
    // about the layout centroid on top of the per-glyph tangent/static rotation,
    // so the whole formula block turns as one — matching the Ctrl+wheel overlay
    // post-rotation, only crisper. The rotated-rect bounds below grow the canvas.
    let global_rotation_rad = params.global_rotation_deg.to_radians();
    // Capture the PRE-global-rotation content box + rotation centroid for an optional
    // mesh warp, before `rotate_placements_about_centroid` mutates the transforms.
    // The centroid is the mean of ALL placement centers (this path passes every
    // transform to the rotation pass); the pre-box uses the SAME line-placed,
    // pre-rotation glyph rects the bounds loop uses, so peel/reapply is exact.
    let warp_pre = if params.raster_transform.is_some() {
        let mut pre_box = PixelBounds::empty();
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        let mut count = 0u32;
        for (seed, transform) in seeds.iter().zip(transforms.iter()) {
            let (cx, cy) = transform.placement_center();
            sum_x += cx;
            sum_y += cy;
            count += 1;
            let physical = seed.glyph.physical(
                (
                    seed.origin_x + seed.glyph_offset_px[0],
                    seed.origin_y + seed.glyph_offset_px[1],
                ),
                1.0,
            );
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
            let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
            if glyph_w <= 0 || glyph_h <= 0 {
                continue;
            }
            let src_left = physical.x + image.placement.left;
            let src_top = physical.y - image.placement.top;
            // Placement keys off the UNPADDED scaled height (matching the draw
            // pass); the included rect is widened by the faux pads.
            let (_, _, _, scaled_height) = seed.glyph_scale.scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
            );
            let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
                src_left as f32,
                src_top as f32,
                glyph_w as f32,
                glyph_h as f32,
                image.placement.top as f32,
            );
            let (placed_center_x, placed_center_y) = apply_line_placement(
                transform.center_x,
                transform.center_y,
                transform.rotation_rad,
                scaled_height,
                line_placement_frac,
            );
            include_rotated_rect_bounds(
                &mut pre_box,
                padded_left,
                padded_top,
                padded_width,
                padded_height,
                placed_center_x,
                placed_center_y,
                transform.rotation_rad,
            );
        }
        let inv = 1.0 / count.max(1) as f32;
        Some((pre_box, [sum_x * inv, sum_y * inv]))
    } else {
        None
    };
    if params.global_rotation_deg.abs() > f32::EPSILON {
        rotate_placements_about_centroid(
            transforms
                .iter_mut()
                .map(|transform| transform as &mut dyn RigidPlacement)
                .collect(),
            global_rotation_rad,
        );
    }

    let mut bounds = PixelBounds::empty();
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = i32::try_from(image.placement.width).unwrap_or(i32::MAX);
        let glyph_h = i32::try_from(image.placement.height).unwrap_or(i32::MAX);
        if glyph_w <= 0 || glyph_h <= 0 {
            continue;
        }
        let src_left = physical.x + image.placement.left;
        let src_top = physical.y - image.placement.top;
        // Placement keys off the UNPADDED scaled height (matching the draw
        // pass); the included rect is widened by the faux pads (hard zeros
        // without faux).
        let (_, _, _, scaled_height) = seed.glyph_scale.scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
        );
        let (padded_left, padded_top, padded_width, padded_height) = seed.padded_scaled_rect(
            src_left as f32,
            src_top as f32,
            glyph_w as f32,
            glyph_h as f32,
            image.placement.top as f32,
        );
        // Perpendicular line placement: shift the glyph off the curve point by
        // `line_placement_frac * ink_height / 2` toward the top/bottom side. The
        // formula curve point IS the glyph ink center (0% = centered), so this is
        // the only adjustment needed on this path.
        let (placed_center_x, placed_center_y) = apply_line_placement(
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
            scaled_height,
            line_placement_frac,
        );
        include_rotated_rect_bounds(
            &mut bounds,
            padded_left,
            padded_top,
            padded_width,
            padded_height,
            placed_center_x,
            placed_center_y,
            transform.rotation_rad,
        );
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            width_px,
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    // Optional vector mesh warp over the PRE-global-rotation content box, peeled/
    // reapplied around the SAME centroid + angle the rotation pass used. Grow the
    // canvas so a strong outward warp never clips. `None`/identity/invalid meshes or
    // a degenerate box take the byte-identical fast path.
    let warp_ctx = params.raster_transform.as_ref().and_then(|warp| {
        let (pre_box, centroid) = warp_pre.as_ref()?;
        if !pre_box.initialized {
            return None;
        }
        let box_min = [pre_box.min_x as f32, pre_box.min_y as f32];
        let box_size = [
            (pre_box.max_x - pre_box.min_x) as f32,
            (pre_box.max_y - pre_box.min_y) as f32,
        ];
        MeshWarpContext::new(warp, box_min, box_size, global_rotation_rad, *centroid)
    });
    if let Some(ctx) = warp_ctx.as_ref() {
        ctx.for_each_warped_bound_point(|x, y| bounds.include_point(x, y));
    }

    let left_overhang = u32::try_from((-bounds.min_x).max(0)).unwrap_or(0);
    let right_overhang = u32::try_from((bounds.max_x - width_px as i32).max(0)).unwrap_or(0);
    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let side_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let top_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let bottom_safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;

    let out_width = width_px
        .saturating_add(left_overhang)
        .saturating_add(right_overhang)
        .saturating_add(horizontal_pad * 2)
        .saturating_add(side_safety_pad * 2)
        .saturating_add(render_margin_pad * 2);
    let content_height = u32::try_from((bounds.max_y - bounds.min_y).max(1)).unwrap_or(1);
    let min_height = base_line_height_px.ceil().max(1.0) as u32;
    let out_height = content_height
        .max(min_height)
        .saturating_add(vertical_pad * 2)
        .saturating_add(top_safety_pad)
        .saturating_add(bottom_safety_pad)
        .saturating_add(render_margin_pad * 2);
    let x_offset =
        i32::try_from(left_overhang + horizontal_pad + side_safety_pad + render_margin_pad)
            .unwrap_or(i32::MAX);
    let y_offset = (-bounds.min_y).saturating_add(
        i32::try_from(vertical_pad + top_safety_pad + render_margin_pad).unwrap_or(0),
    );
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    for (seed, transform) in seeds.drain(..).zip(transforms.drain(..)) {
        let physical = seed.glyph.physical(
            (
                seed.origin_x + seed.glyph_offset_px[0],
                seed.origin_y + seed.glyph_offset_px[1],
            ),
            1.0,
        );
        let Some(image) = cache.get_image(font_system, physical.cache_key) else {
            continue;
        };
        let glyph_w = image.placement.width as usize;
        let glyph_h = image.placement.height as usize;
        if glyph_w == 0 || glyph_h == 0 {
            continue;
        }
        let placement_left = image.placement.left as f32;
        let placement_top = image.placement.top as f32;
        let src_left = (physical.x + image.placement.left) as f32;
        let src_top = (physical.y - image.placement.top) as f32;

        // Perpendicular line placement: shift the curve point (which is the glyph
        // ink center, 0% = centered) toward the top/bottom side of the line by
        // `line_placement_frac * ink_height / 2`. Shared with the bounds pass.
        let (_placed_scaled_left, _placed_scaled_top, _placed_scaled_width, placed_scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (placed_center_x, placed_center_y) = apply_line_placement(
            transform.center_x,
            transform.center_y,
            transform.rotation_rad,
            placed_scaled_height,
            line_placement_frac,
        );

        // Resolve the outline up front so the extra-info sample knows whether this
        // glyph draws warped (outline) or as an UNWARPED color-glyph bitmap fallback.
        let glyph_outline = resolve_glyph_outline(&seed, font_system, &mut outline_cache);
        // Whether this glyph puts ink on the canvas at all: it either has an
        // outline, or it is an outline-less glyph whose bitmap still gets blitted.
        // `false` only for a glyph a faux THINNING offset consumed entirely, which
        // draws nothing (the `continue` below) and therefore must not vote on the
        // extra-info ink centers either. Short-circuits, so an outline glyph pays
        // no extra lookup.
        let draws_ink = glyph_outline.is_some()
            || glyph_needs_bitmap_fallback(
                font_system,
                &mut outline_cache,
                &seed.glyph,
                seed.faux.bold,
            );

        // Extra-info sample: the final line-placed, rotated glyph box the composite
        // pass draws. Both kinds contribute, but only the outline sample is warpable;
        // the bitmap fallback's sample stays unwarped to match its unwarped pixels.
        // Leading/trailing hanging punctuation is skipped so it cannot drag the center.
        if extra_active && draws_ink && !seed.hanging_excluded {
            let (scaled_w, scaled_h) =
                seed.glyph_scale.scaled_size(glyph_w as f32, glyph_h as f32);
            let (corners, center) = rotated_box_samples(
                placed_center_x,
                placed_center_y,
                scaled_w,
                scaled_h,
                transform.rotation_rad,
            );
            extra_acc.add_glyph(corners, center, glyph_outline.is_some(), seed.line_idx);
        }

        // Prefer the true font outline; keep the bitmap blit for outline-less
        // glyphs. The (line-placed) transform center is the glyph bitmap center in
        // world space, so it is the outline destination center directly.
        if let Some(outline) = glyph_outline {
            let glyph_transform = glyph_outline_transform(
                placed_center_x,
                placed_center_y,
                transform.rotation_rad,
                placement_left,
                placement_top,
                glyph_w as f32,
                glyph_h as f32,
                seed.glyph_scale.width_mul,
                seed.glyph_scale.height_mul,
                glyph_subpixel_offset(physical.cache_key),
                seed.faux.shear_x,
            );
            rasterize_outline_into(
                &mut raster_scratch,
                rgba.as_mut_slice(),
                out_width as usize,
                out_height as usize,
                -(x_offset as f32),
                -(y_offset as f32),
                &outline,
                &glyph_transform,
                seed.text_color,
                &aa_lut,
                warp_ctx.as_ref(),
            );
            continue;
        }

        // A glyph whose outline was CONSUMED by a faux thinning offset draws
        // nothing: blitting its bitmap would restore it at FULL weight.
        if !draws_ink {
            continue;
        }

        // Fallback: the original rotated bitmap blit for any outline-less glyph
        // (real color glyph or a monochrome embedded-bitmap glyph). The subpixel
        // fraction is already baked into the bitmap coverage.
        let src_center_x = src_left + glyph_w as f32 * 0.5;
        let src_center_y = src_top + glyph_h as f32 * 0.5;
        let cos_a = transform.rotation_rad.cos();
        let sin_a = transform.rotation_rad.sin();
        let glyph_rgba = build_glyph_rgba_buffer(
            &image.content,
            image.data.as_slice(),
            glyph_w,
            glyph_h,
            seed.text_color,
        );
        let (scaled_left, scaled_top, scaled_width, scaled_height) =
            seed.glyph_scale
                .scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            placed_center_x,
            placed_center_y,
            transform.rotation_rad,
        );
        let dst_min_x = ((min_x + x_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_x = ((max_x + x_offset as f32).ceil() as i32 + 1).min(out_width as i32);
        let dst_min_y = ((min_y + y_offset as f32).floor() as i32 - 1).max(0);
        let dst_max_y = ((max_y + y_offset as f32).ceil() as i32 + 1).min(out_height as i32);
        for dst_y in dst_min_y..dst_max_y {
            for dst_x in dst_min_x..dst_max_x {
                let world_x = dst_x as f32 + 0.5 - x_offset as f32;
                let world_y = dst_y as f32 + 0.5 - y_offset as f32;
                let rel_x = world_x - placed_center_x;
                let rel_y = world_y - placed_center_y;
                let rotated_x = rel_x * cos_a + rel_y * sin_a;
                let rotated_y = -rel_x * sin_a + rel_y * cos_a;
                let src_x = src_center_x + rotated_x / seed.glyph_scale.width_mul;
                let src_y = src_center_y + rotated_y / seed.glyph_scale.height_mul;
                let local_x = src_x - src_left - 0.5;
                let local_y = src_y - src_top - 0.5;
                let (src_r, src_g, src_b, src_a) =
                    bilinear_sample_rgba(glyph_rgba.as_slice(), glyph_w, glyph_h, local_x, local_y);
                if src_a == 0 {
                    continue;
                }
                let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
                blend_pixel_over(&mut rgba[dst_idx..dst_idx + 4], src_r, src_g, src_b, src_a);
            }
        }
    }

    // Extra-info centers: warp the raw content-space samples through the same mesh
    // context the composite pass used, then map to canvas pixels via the pass
    // offset. Runs BEFORE the caller's trim/effects; both seams self-correct.
    if let Some(ctx) = warp_ctx.as_ref() {
        extra_acc.map_points(|point| ctx.warp_world(point));
    }
    let extra = extra_acc.finish(x_offset as f32, y_offset as f32);

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
        extra,
        // Filled in by `pipeline::render_text_to_image`, which owns the shaped
        // buffer this layout drew from.
        font_fallbacks: crate::types::FontFallbackReport::default(),
    })
}

// Shape fallback depends on shaped glyph metrics, formula arc length and inline-size-aware baselines.
#[allow(clippy::too_many_arguments)]
fn detect_shape_layout_fallback_reason(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    faux_face_baseline: FauxFaceBaseline,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    custom_kerning: &CustomKerningMap,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_extra_spacing_table: &[f32],
    // The GLOBAL default the table was built around; see
    // `render_text_with_formula_layout_once`.
    default_extra_line_spacing_px: f32,
) -> Result<Option<String>, String> {
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let line_baselines = compute_horizontal_line_baselines(
        buffer,
        base_line_height_px,
        default_extra_line_spacing_px,
        line_extra_spacing_table,
        has_inline_size_overrides,
    );
    // This probe only measures shaped advances, but the seeds it builds carry a
    // resolved faux style, so it needs an outline cache of its own. It stays EMPTY
    // unless faux bold in the counter-preserving mode is active (the only case
    // that looks a glyph up), so the no-faux probe costs exactly nothing.
    let mut outline_cache = OutlineCache::new();
    let seeds = collect_formula_glyph_seeds(
        params,
        font_system,
        &mut outline_cache,
        buffer,
        attrs,
        faux_face_baseline,
        inline_style_spans,
        inline_font_registry,
        custom_kerning,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_baselines.as_slice(),
    );
    if seeds.len() <= 1 {
        return Ok(None);
    }

    let formula_program = FormulaProgramBundle::compile(&params.formula_layout)?;
    let width_px = params.width_px.max(1) as f32;
    let mut lines = HashMap::<usize, (f32, usize)>::new();
    let default_advance = (font_size_px * 0.5).max(1.0);
    for seed in &seeds {
        let entry = lines.entry(seed.line_idx).or_insert((0.0, 0));
        entry.0 += seed.glyph.w.max(default_advance).max(1.0);
        entry.1 += 1;
    }

    let mut reasons = Vec::<String>::new();
    for (line_idx, (text_len_px, glyph_count)) in lines {
        if glyph_count <= 1 {
            continue;
        }
        let eval = FormulaEvalInput {
            t01: 0.0,
            i: 0.0,
            n: glyph_count as f32,
            s: 0.0,
            line: line_idx as f32,
            line_t: 0.0,
            line_n: glyph_count as f32,
            width_px,
            font_size_px,
            user_vars: &params.formula_layout.vars,
        };
        let curve_len_px =
            build_formula_arc_length_table(&formula_program, &params.formula_layout, &eval)?
                .last()
                .map(|sample| sample.arc_len_px)
                .unwrap_or(0.0)
                .max(0.0);
        let compression_ratio = curve_len_px / text_len_px.max(1.0);
        if curve_len_px < font_size_px * 1.5 || compression_ratio < 0.38 {
            reasons.push(format!(
                "строка {}: длина формы {:.0}px для текста {:.0}px",
                line_idx + 1,
                curve_len_px,
                text_len_px
            ));
        }
    }

    if reasons.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Форма слишком узкая для текущего текста, выполнен обычный рендер ({})",
            reasons.join(", ")
        )))
    }
}

// Formula seed collection needs the shaped buffer, inline spans and soft-hyphen reconstruction.
#[allow(clippy::too_many_arguments)]
fn collect_formula_glyph_seeds(
    params: &TextRenderParams,
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
    buffer: &mut Buffer,
    attrs: &Attrs<'_>,
    faux_face_baseline: FauxFaceBaseline,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
    custom_kerning: &CustomKerningMap,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    base_line_height_px: f32,
    line_baselines: &[f32],
) -> Vec<FormulaGlyphSeed> {
    let width_px = params.width_px.max(1);
    let has_inline_size_overrides =
        inline_style_spans.is_some_and(spans_have_inline_size_overrides);
    let mut line_counts = Vec::<usize>::new();
    let mut line_idx = 0usize;
    for run in buffer.layout_runs() {
        if run.glyphs.is_empty() {
            line_idx += 1;
            continue;
        }
        if line_counts.len() <= line_idx {
            line_counts.push(0);
        }
        line_counts[line_idx] += run.glyphs.len();
        line_idx += 1;
    }

    let mut out = Vec::<FormulaGlyphSeed>::new();
    let mut line_seen = vec![0usize; line_counts.len().max(1)];
    let inline_line_aligns =
        compute_inline_line_aligns(params.align, layout_line_offsets, inline_style_spans);
    let mut line_idx = 0usize;
    let mut runs = buffer.layout_runs().peekable();
    // Leading/trailing hanging-punctuation runs are marked (not dropped): the glyphs still
    // DRAW, they are only kept out of the extra-info center sampling. Computed only when that
    // sampling is active and punctuation hangs at or above the exclusion threshold, so the
    // default path pays nothing.
    let mark_hanging = params.extra_info.is_active() && params.excludes_hanging_from_extra_info();
    while let Some(run) = runs.next() {
        let hanging_bounds = if mark_hanging {
            hanging_edge_run_bounds(&run)
        } else {
            (0, run.glyphs.len())
        };
        let line_align = inline_line_aligns
            .get(line_idx)
            .copied()
            .unwrap_or(params.align);
        let line_offset_x = horizontal_line_offset(width_px, run.line_w, line_align) as f32;
        let baseline_y = line_baselines.get(line_idx).copied().unwrap_or_else(|| {
            horizontal_run_baseline_y(
                &run,
                line_idx,
                run.line_y,
                base_line_height_px,
                0.0,
                has_inline_size_overrides,
            )
        });
        for (glyph_idx_in_run, glyph) in run.glyphs.iter().enumerate() {
            let glyph_idx_in_line = line_seen
                .get_mut(line_idx)
                .map(|value| {
                    let idx = *value;
                    *value += 1;
                    idx
                })
                .unwrap_or(0);
            out.push(FormulaGlyphSeed {
                cluster_char: single_char_cluster(run.text, glyph),
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    params.text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                origin_x: line_offset_x,
                origin_y: baseline_y,
                kerning: inline_kerning_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                )
                // Keeps the run off the byte-identical fast path when this glyph's
                // face carries overrides; the PAIRS are resolved in
                // `assign_formula_seed_advances`.
                .with_custom_pairs(custom_kerning.has_table(glyph.font_id)),
                glyph_scale: inline_glyph_scale_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_offset_px: inline_glyph_offset_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                extended_offset: inline_glyph_offset_style_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                style_offset: glyph_style_offset(layout_line_offsets, run.line_i, glyph),
                offset_span_range: inline_glyph_offset_span_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                line_idx,
                glyph_idx_in_line,
                glyphs_in_line: line_counts.get(line_idx).copied().unwrap_or(1),
                line_align,
                advance_px: 0.0,
                faux: resolve_faux_counter_flag(
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
                ),
                hanging_excluded: mark_hanging
                    && is_edge_run_hanging(hanging_bounds, glyph_idx_in_run),
            });
        }

        if run_wraps_at_soft_hyphen(&run, runs.peek())
            && let Some(mut hyphen_glyph) = build_wrapped_hyphen_glyph(
                font_system,
                attrs,
                faux_face_baseline,
                inline_style_spans,
                inline_font_registry,
                layout_line_offsets,
                &run,
                runs.peek(),
                font_size_px,
                base_line_height_px,
            )
        {
            hyphen_glyph.x = trailing_hyphen_x(&run);
            let glyph_idx_in_line = line_seen
                .get_mut(line_idx)
                .map(|value| {
                    let idx = *value;
                    *value += 1;
                    idx
                })
                .unwrap_or(0);
            let style_offset = soft_hyphen_style_offset(&run, runs.peek(), layout_line_offsets);
            // Resolved before the struct literal below moves `hyphen_glyph`.
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
                outline_cache,
                &hyphen_glyph,
            );
            out.push(FormulaGlyphSeed {
                // Synthesized, not shaped from the layout text: it has no source
                // cluster, so it never participates in a user-authored pair.
                cluster_char: None,
                glyph: hyphen_glyph,
                text_color: style_offset
                    .map(|offset| {
                        inline_text_color_at_offset(params.text_color, inline_style_spans, offset)
                    })
                    .unwrap_or(params.text_color),
                origin_x: line_offset_x,
                origin_y: baseline_y,
                kerning: style_offset
                    .map(|offset| inline_kerning_at_offset(params, inline_style_spans, offset))
                    .unwrap_or_else(|| KerningSettings::from_params(params)),
                glyph_scale: style_offset
                    .map(|offset| inline_glyph_scale_at_offset(params, inline_style_spans, offset))
                    .unwrap_or_else(|| GlyphScaleSettings::from_params(params)),
                glyph_offset_px: style_offset
                    .map(|offset| inline_glyph_offset_at_offset(inline_style_spans, offset))
                    .unwrap_or([0.0, 0.0]),
                extended_offset: style_offset
                    .map(|offset| inline_glyph_offset_style_at_offset(inline_style_spans, offset))
                    .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0])),
                style_offset: style_offset
                    .unwrap_or_else(|| layout_line_offsets.get(run.line_i).copied().unwrap_or(0)),
                offset_span_range: style_offset.and_then(|offset| {
                    inline_glyph_offset_span_at_offset(inline_style_spans, offset)
                }),
                line_idx,
                glyph_idx_in_line,
                glyphs_in_line: line_counts.get(line_idx).copied().unwrap_or(1),
                line_align,
                advance_px: 0.0,
                faux: hyphen_faux,
                // The wrapped soft hyphen is real ink and never hangs, so it always
                // contributes to the extra-info samples (matches the horizontal path).
                hanging_excluded: false,
            });
        }
        line_idx += 1;
    }

    assign_formula_seed_advances(
        out.as_mut_slice(),
        font_system,
        custom_kerning,
        font_size_px,
        (font_size_px * 0.5).max(1.0),
    );
    out
}

/// The shaped pen delta between two adjacent glyphs, turned into a POSITIVE
/// magnitude fit for an on-path walk.
///
/// `raw_delta_px` is `next.glyph.x - this.glyph.x` straight out of cosmic-text
/// and `glyph_w_px` is the left glyph's shaped advance width. Returns
/// `max(raw_delta_px, glyph_w_px * 0.25, 1.0)`.
///
/// Why the floor exists at all: on-path layouts walk an arc length, so a seed
/// advance is a DISTANCE, not a signed x step (see `assign_formula_seed_advances`).
/// Two inputs break that. A right-to-left or otherwise non-monotonic run has
/// DECREASING `glyph.x`, so `raw_delta_px` is negative and would walk the cursor
/// backwards. And a font pair-kerning value below `-75%` of the left glyph's own
/// advance would collapse the step to (near) nothing, stacking two glyphs on one
/// curve point. The quarter-advance term is the tunable part: it keeps a badly
/// kerned pair legible by refusing to overlap the glyphs by more than three
/// quarters, and the `1.0` term is the absolute guard for a zero-width glyph.
///
/// What this floor is NOT: it does not clip user-authored kerning. An authored
/// pair takes the `custom_delta_px` branch of `assign_formula_seed_advances`,
/// which steps by the left glyph's own NOMINAL advance plus the authored delta
/// and uses this value only as the fallback when that nominal metric is missing.
/// A strongly negative authored pair therefore passes through here untouched and
/// is clamped later, by [`OnPathStepSpacing::step_px`].
#[must_use]
fn seed_metric_advance_px(raw_delta_px: f32, glyph_w_px: f32) -> f32 {
    let glyph_width_floor = (glyph_w_px * 0.25).max(1.0);
    raw_delta_px.max(glyph_width_floor).max(1.0)
}

/// Fills every seed's `advance_px` — the pen step from that glyph to the NEXT one
/// on the same line — from the shaped positions, the kerning mode and the
/// user-authored overrides.
///
/// Custom kerning mirrors `pipeline::horizontal_run_layout` exactly: a pair listed
/// in the left glyph's font table REPLACES the font's own value for that pair under
/// every mode, stepping by `nominal_glyph_advance_px(left) + delta`. Uniform
/// tracking is still added on top.
///
/// DIRECTION: unlike the horizontal pen, `advance_px` is a MAGNITUDE along the
/// drawn line, never a signed x step — the shaped delta is floored positive by
/// [`seed_metric_advance_px`], and every consumer floors the result AGAIN through
/// the single [`OnPathStepSpacing::step_px`] owner before walking the path. So the
/// RTL sign mirroring `pipeline::custom_pair_step_px` performs has no counterpart
/// on this path: seeds are advanced along the curve in logical order regardless of
/// script direction. Formula/drawn lines are LTR-only by construction, which is a
/// limitation of that whole path, not of the overrides.
///
/// The value written into a seed is NOT guaranteed positive: the custom-pair and
/// tracking branches below add signed terms (`custom_delta_px`,
/// `KerningSettings::extra_spacing_px`) on top of an already-floored metric
/// advance and are deliberately not re-floored, so a strongly negative authored
/// pair leaves a negative `advance_px` here. Making it positive is the consumers'
/// job, and theirs alone — [`OnPathStepSpacing::step_px`].
fn assign_formula_seed_advances(
    seeds: &mut [FormulaGlyphSeed],
    font_system: &mut FontSystem,
    custom_kerning: &CustomKerningMap,
    font_size_px: f32,
    default_advance: f32,
) {
    if seeds
        .iter()
        .all(|seed| seed.kerning.uses_default_metric_layout())
    {
        let mut idx = 0usize;
        while idx < seeds.len() {
            let line_idx = seeds[idx].line_idx;
            let line_start = idx;
            idx += 1;
            while idx < seeds.len() && seeds[idx].line_idx == line_idx {
                idx += 1;
            }
            let line_end = idx;
            let mut prev_advance = default_advance;
            for glyph_idx in line_start..line_end {
                let advance_px = if glyph_idx + 1 < line_end {
                    seed_metric_advance_px(
                        seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x,
                        seeds[glyph_idx].glyph.w,
                    )
                } else if glyph_idx > line_start {
                    prev_advance
                } else {
                    seeds[glyph_idx].glyph.w.max(default_advance).max(1.0)
                };
                seeds[glyph_idx].advance_px = advance_px;
                prev_advance = advance_px;
            }
        }
        return;
    }

    let mut cache = SwashCache::new();
    let profiles = seeds
        .iter()
        .map(|seed| glyph_ink_profile(font_system, &mut cache, &seed.glyph, font_size_px))
        .collect::<Vec<_>>();
    let mut idx = 0usize;
    while idx < seeds.len() {
        let line_idx = seeds[idx].line_idx;
        let line_start = idx;
        idx += 1;
        while idx < seeds.len() && seeds[idx].line_idx == line_idx {
            idx += 1;
        }
        let line_end = idx;
        let mut prev_advance = default_advance;
        for glyph_idx in line_start..line_end {
            let advance_px = if glyph_idx + 1 < line_end {
                let metric_advance = seed_metric_advance_px(
                    seeds[glyph_idx + 1].glyph.x - seeds[glyph_idx].glyph.x,
                    seeds[glyph_idx].glyph.w,
                );
                let kerning = seeds[glyph_idx + 1].kerning;
                // A user-authored pair REPLACES the font's own value for that pair
                // under every mode, exactly as on the horizontal path.
                let custom_delta_px =
                    custom_seed_pair_delta_px(custom_kerning, &seeds[glyph_idx], &seeds[glyph_idx + 1]);
                let base_advance = match (custom_delta_px, kerning.mode) {
                    (Some(delta_px), _) => {
                        let own = nominal_glyph_advance_px(font_system, &seeds[glyph_idx].glyph)
                            .unwrap_or(metric_advance);
                        optical_base_advance(own, metric_advance) + delta_px
                    }
                    // `Auto` keeps the shaped (font-pair-kerned) advance.
                    (None, KerningMode::Auto) => metric_advance,
                    // `Fixed` steps by the glyph's OWN nominal (un-kerned) advance,
                    // dropping font pair kerning and the optical adjustment. The
                    // shaped advance bakes in pair kerning, so the raw metrics
                    // table is consulted; falls back to the shaped advance when the
                    // metric is unavailable.
                    (None, KerningMode::Fixed) => {
                        let own = nominal_glyph_advance_px(font_system, &seeds[glyph_idx].glyph)
                            .unwrap_or(metric_advance);
                        optical_base_advance(own, metric_advance)
                    }
                    (None, KerningMode::Optical) => {
                        metric_advance
                            + optical_horizontal_pair_adjustment(
                                profiles[glyph_idx],
                                profiles[glyph_idx + 1],
                                metric_advance,
                                font_size_px,
                            )
                    }
                };
                // The tracking basis follows the branch the STEP actually took: an
                // overridden pair is a metric step, not an optical one.
                let spacing_basis = match (custom_delta_px, kerning.mode) {
                    (Some(_), _) | (None, KerningMode::Auto | KerningMode::Fixed) => metric_advance,
                    (None, KerningMode::Optical) => ((profiles[glyph_idx].width_px()
                        + profiles[glyph_idx + 1].width_px())
                        * 0.5)
                        .max(default_advance),
                };
                base_advance + kerning.extra_spacing_px(spacing_basis)
            } else if glyph_idx > line_start {
                prev_advance
            } else {
                seeds[glyph_idx].glyph.w.max(default_advance).max(1.0)
            };
            seeds[glyph_idx].advance_px = advance_px;
            prev_advance = advance_px;
        }
    }
}

/// The user-authored advance delta in px between two adjacent SEEDS, or `None`
/// when no override applies.
///
/// The seed-side mirror of `pipeline::custom_pair_delta_px` and it enforces the
/// same guards: the same face on both sides (a pair across a font-fallback
/// boundary belongs to no font's design), a face that actually carries a table,
/// and a single-`char` source cluster on each side (captured as
/// `FormulaGlyphSeed::cluster_char`, since the run text is gone by now).
#[must_use]
fn custom_seed_pair_delta_px(
    custom_kerning: &CustomKerningMap,
    prev: &FormulaGlyphSeed,
    cur: &FormulaGlyphSeed,
) -> Option<f32> {
    if prev.glyph.font_id != cur.glyph.font_id {
        return None;
    }
    let table = custom_kerning.table_for(prev.glyph.font_id)?;
    table.delta_px(prev.cluster_char?, cur.cluster_char?, prev.glyph.font_size)
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

/// Resolve per-line alignment from inline style spans, falling back to the block alignment.
fn compute_inline_line_aligns(
    base_align: HorizontalAlign,
    layout_line_offsets: &[usize],
    inline_style_spans: Option<&[InlineStyleSpan]>,
) -> Vec<HorizontalAlign> {
    let Some(spans) = inline_style_spans else {
        return vec![base_align; layout_line_offsets.len().max(1)];
    };
    layout_line_offsets
        .iter()
        .map(|offset| {
            inline_style_at_offset(spans, *offset)
                .and_then(|span| span.align)
                .unwrap_or(base_align)
        })
        .collect()
}

fn spans_have_inline_size_overrides(spans: &[InlineStyleSpan]) -> bool {
    spans.iter().any(|span| span.font_size_px.is_some())
}

fn inline_text_color_at_offset(
    default_text_color: [u8; 4],
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> [u8; 4] {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.text_color)
        .unwrap_or(default_text_color)
}

fn inline_text_color_for_glyph(
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

fn glyph_style_offset(
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> usize {
    layout_line_offsets.get(line_idx).copied().unwrap_or(0) + glyph.start.min(glyph.end)
}

fn inline_glyph_offset_style_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> InlineGlyphOffset {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.glyph_offset)
        .unwrap_or_else(|| InlineGlyphOffset::global_only([0.0, 0.0]))
}

fn inline_glyph_offset_span_at_offset(
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> Option<(usize, usize)> {
    spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .filter(|style| style.glyph_offset.is_some())
        .map(|style| (style.start, style.end))
}

fn inline_glyph_offset_style_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> InlineGlyphOffset {
    inline_glyph_offset_style_at_offset(
        spans,
        glyph_style_offset(layout_line_offsets, line_idx, glyph),
    )
}

fn inline_glyph_offset_span_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> Option<(usize, usize)> {
    inline_glyph_offset_span_at_offset(
        spans,
        glyph_style_offset(layout_line_offsets, line_idx, glyph),
    )
}

fn inline_glyph_offset_for_glyph(
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> [f32; 2] {
    inline_glyph_offset_style_for_glyph(spans, layout_line_offsets, line_idx, glyph).global_px
}

fn inline_glyph_scale_at_offset(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    offset: usize,
) -> GlyphScaleSettings {
    let stretch = spans
        .and_then(|value| inline_style_at_offset(value, offset))
        .and_then(|style| style.glyph_stretch_percent)
        .unwrap_or([params.glyph_width_percent, params.glyph_height_percent]);
    GlyphScaleSettings {
        width_mul: (stretch[0] / 100.0).clamp(0.01, 3.0),
        height_mul: (stretch[1] / 100.0).clamp(0.01, 3.0),
    }
}

fn inline_glyph_scale_for_glyph(
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
    let style = spans.and_then(|value| inline_style_at_offset(value, offset));
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

fn inline_kerning_for_glyph(
    params: &TextRenderParams,
    spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    line_idx: usize,
    glyph: &LayoutGlyph,
) -> KerningSettings {
    let line_offset = layout_line_offsets.get(line_idx).copied().unwrap_or(0);
    inline_kerning_at_offset(params, spans, line_offset + glyph.start.min(glyph.end))
}

fn inline_style_at_offset(spans: &[InlineStyleSpan], offset: usize) -> Option<&InlineStyleSpan> {
    spans
        .iter()
        .find(|span| span.start <= offset && offset < span.end)
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

// Wrapped hyphen synthesis depends on shaped line boundaries, inline attrs and fallback font selection.
#[allow(clippy::too_many_arguments)]
fn build_wrapped_hyphen_glyph(
    font_system: &mut FontSystem,
    base_attrs: &Attrs<'_>,
    faux_face_baseline: FauxFaceBaseline,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    inline_font_registry: &InlineFontRegistry,
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
    inline_font_registry: &InlineFontRegistry,
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

/// Map a glyph's run-local arc length onto its formula curve.
///
/// When the run FITS the curve, the leftover curve length is split by
/// `align_fraction` (`0.0` = run pinned to the curve start, `0.5` = centered,
/// `1.0` = pinned to the curve end); see [`on_path_align_fraction`], which the
/// caller uses with `0.5` as the justify default so justified formula overlays
/// keep their historical centering.
///
/// When the run OVERFLOWS the curve it is compressed onto the curve
/// (`center_s * curve_len / text_len`) and `align_fraction` is deliberately
/// ignored: there is no free space left to distribute, and `Shape`'s fallback
/// threshold (`detect_shape_layout_fallback_reason`) is defined against exactly
/// this compression ratio.
fn map_formula_target_arc_length(
    center_s_px: f32,
    text_len_px: f32,
    curve_len_px: f32,
    align_fraction: f32,
) -> f32 {
    if curve_len_px <= 0.0 {
        return 0.0;
    }
    let text_len_px = text_len_px.max(1.0);
    if text_len_px <= curve_len_px {
        let leading_gap = (curve_len_px - text_len_px) * align_fraction;
        (leading_gap + center_s_px).clamp(0.0, curve_len_px)
    } else {
        (center_s_px * (curve_len_px / text_len_px)).clamp(0.0, curve_len_px)
    }
}

fn formula_t01_for_arc_length(samples: &[FormulaArcLengthSample], target_arc_len_px: f32) -> f32 {
    let Some(last) = samples.last().copied() else {
        return 0.0;
    };
    if target_arc_len_px <= 0.0 {
        return samples.first().map(|sample| sample.t01).unwrap_or(0.0);
    }
    if target_arc_len_px >= last.arc_len_px {
        return last.t01;
    }

    let idx = samples.partition_point(|sample| sample.arc_len_px < target_arc_len_px);
    if idx == 0 {
        return samples[0].t01;
    }
    let prev = samples[idx - 1];
    let next = samples[idx];
    let span = (next.arc_len_px - prev.arc_len_px).abs();
    if span <= 1e-6 {
        return next.t01;
    }
    let local_t = ((target_arc_len_px - prev.arc_len_px) / span).clamp(0.0, 1.0);
    prev.t01 + (next.t01 - prev.t01) * local_t
}

fn build_formula_arc_length_table(
    program: &FormulaProgramBundle,
    layout: &crate::types::TextFormulaLayoutParams,
    input: &FormulaEvalInput<'_>,
) -> Result<Vec<FormulaArcLengthSample>, String> {
    let samples = program.build_arc_length_table(layout, input)?;
    Ok(samples
        .into_iter()
        .map(|sample| FormulaArcLengthSample {
            t01: sample.t01,
            arc_len_px: sample.arc_len_px,
        })
        .collect())
}

// Formula rotated bounds are clearer with explicit source/destination coordinates than with
// a one-off wrapper struct used only by this helper.
#[allow(clippy::too_many_arguments)]
fn optical_horizontal_pair_adjustment(
    prev: GlyphInkProfile,
    next: GlyphInkProfile,
    metric_advance: f32,
    font_size_px: f32,
) -> f32 {
    if !metric_advance.is_finite() || metric_advance <= 0.0 {
        return 0.0;
    }

    let avg_width = ((prev.width_px() + next.width_px()) * 0.5).max(font_size_px * 0.3);
    let actual_gap = metric_advance + next.left_px - prev.right_px;
    let target_gap = (avg_width * 0.08).clamp(font_size_px * 0.02, font_size_px * 0.14);
    let tighten_limit = metric_advance.min(font_size_px * 0.14) * 0.55;
    let loosen_limit = font_size_px * 0.18;
    let delta = ((target_gap - actual_gap) * 0.55).clamp(-tighten_limit, loosen_limit);
    if delta.abs() < 0.25 { 0.0 } else { delta }
}

fn glyph_ink_profile(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
) -> GlyphInkProfile {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
        return GlyphInkProfile::fallback(glyph.w.max(font_size_px * 0.5), font_size_px);
    };
    glyph_ink_profile_from_image(
        [
            image.placement.left as f32,
            (physical.y - image.placement.top) as f32,
        ],
        &image.content,
        image.data.as_slice(),
        [
            image.placement.width as usize,
            image.placement.height as usize,
        ],
        [glyph.w.max(font_size_px * 0.5), font_size_px],
    )
}

fn glyph_ink_profile_from_image(
    draw_origin_px: [f32; 2],
    content: &SwashContent,
    data: &[u8],
    glyph_size: [usize; 2],
    fallback_size_px: [f32; 2],
) -> GlyphInkProfile {
    let [draw_left_px, _draw_top_px] = draw_origin_px;
    let [glyph_w, glyph_h] = glyph_size;
    let [fallback_width_px, fallback_height_px] = fallback_size_px;
    if glyph_w == 0 || glyph_h == 0 {
        return GlyphInkProfile::fallback(fallback_width_px, fallback_height_px);
    }

    let mut min_x = glyph_w;
    let mut min_y = glyph_h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut has_alpha = false;

    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            if sample_swash_alpha(content, data, glyph_w, gx, gy) < 12 {
                continue;
            }
            min_x = min_x.min(gx);
            min_y = min_y.min(gy);
            max_x = max_x.max(gx);
            max_y = max_y.max(gy);
            has_alpha = true;
        }
    }

    if !has_alpha {
        return GlyphInkProfile::fallback(fallback_width_px, fallback_height_px);
    }

    GlyphInkProfile {
        left_px: draw_left_px + min_x as f32,
        right_px: draw_left_px + max_x as f32 + 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DrawnLineDropSide, DrawnLineTransform, FormulaGlyphSeed, GlyphScaleSettings,
        HorizontalAlign, KerningSettings, LinePlacementReference, OnPathStepSpacing,
        apply_line_placement,
        drawn_line_center_s, drawn_line_drop_side, drawn_line_glyph_destination_center_raw,
        drawn_line_next_cursor, drawn_line_start_offset, drawn_line_start_offsets,
        ON_PATH_INK_CLEARANCE_FLOOR_PX, ON_PATH_INK_SEARCH_MAX_RANGE_PX,
        ON_PATH_INK_SEARCH_MIN_RANGE_PX, PlacedGlyphInk, PlacedNeighbor, clears_floor,
        find_clearance_center_s, map_formula_target_arc_length, on_path_align_fraction,
        on_path_pair_gap, pair_chord_axis, pair_clearance_floor, placed_contour_for_transform,
        sample_drawn_line_path_for_direction, solve_directional_gap_center_s,
    };
    use crate::drawn_lines::{DrawnLinePath, DrawnLinePoint};
    use crate::inline_styles::InlineGlyphOffset;
    use crate::pipeline::FauxGlyphStyle;
    use crate::types::KerningMode;
    use cosmic_text::{CacheKeyFlags, LayoutGlyph, fontdb};
    use crate::glyph_contour::{
        GlyphContour, PlacedContour, min_placed_distance,
    };
    use crate::raster::rotated_rect_world_bounds;
    use crate::types::TextVectorLineTextDirection;

    /// Axis-aligned square contour of side `2 * half` centered on the origin.
    fn square_contour(half: f32) -> GlyphContour {
        GlyphContour {
            components: vec![vec![
                [-half, -half],
                [half, -half],
                [half, half],
                [-half, half],
            ]],
        }
    }

    /// A straight horizontal path along +x of the given length.
    fn straight_path(len: f32) -> DrawnLinePath {
        DrawnLinePath {
            points: vec![
                DrawnLinePoint {
                    x: 0.0,
                    y: 0.0,
                    arc_len_px: 0.0,
                },
                DrawnLinePoint {
                    x: len,
                    y: 0.0,
                    arc_len_px: len,
                },
            ],
            total_len_px: len,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// A semicircle path of radius `r`, sampled into `segments` chords, so the
    /// chord distance between two arc-length-equidistant points is always less
    /// than their arc-length separation (any curve pulls shapes together).
    fn semicircle_path(r: f32, segments: usize) -> DrawnLinePath {
        let mut points = Vec::with_capacity(segments + 1);
        let mut arc = 0.0f32;
        let mut prev: Option<(f32, f32)> = None;
        for i in 0..=segments {
            let angle = std::f32::consts::PI * (i as f32) / (segments as f32);
            let x = r * angle.cos();
            let y = r * angle.sin();
            if let Some((px, py)) = prev {
                arc += ((x - px).powi(2) + (y - py).powi(2)).sqrt();
            }
            points.push(DrawnLinePoint {
                x,
                y,
                arc_len_px: arc,
            });
            prev = Some((x, y));
        }
        DrawnLinePath {
            points,
            total_len_px: arc,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// A wedge whose RIGHT side is a sloped facing edge (`half` tall, tip on the
    /// band center) and whose left side is flat — the shape class (`V`, `А`,
    /// `Т`) where a bbox half-width is furthest from the real facing edge and
    /// where the scanline discretization of the gap metric bites hardest.
    fn wedge_contour(half: f32) -> GlyphContour {
        GlyphContour {
            components: vec![vec![[-half, -half], [half, 0.0], [-half, half]]],
        }
    }

    /// Ink of `contour` centered on the path sample for `s`, with NO rotation:
    /// the pair's chord is then the only thing that changes along the path.
    fn ink_at(path: &DrawnLinePath, contour: &GlyphContour, s: f32) -> Option<PlacedGlyphInk> {
        let (x, y, _tx, _ty) = sample_drawn_line_path_for_direction(path, s)?;
        Some(PlacedGlyphInk {
            contour: contour.placed(1.0, 0.0, 1.0, 1.0, x, y),
            center: [x, y],
        })
    }

    /// Ink of `contour` centered on the path sample for `s` and rotated with the
    /// tangent, i.e. exactly how `place_seed_ink_at` places a real glyph under
    /// `use_tangent_rotation`.
    fn tangent_ink_at(
        path: &DrawnLinePath,
        contour: &GlyphContour,
        s: f32,
    ) -> Option<PlacedGlyphInk> {
        let (x, y, tx, ty) = sample_drawn_line_path_for_direction(path, s)?;
        let len = (tx * tx + ty * ty).sqrt().max(1e-6);
        Some(PlacedGlyphInk {
            contour: contour.placed(tx / len, ty / len, 1.0, 1.0, x, y),
            center: [x, y],
        })
    }

    /// The straight REFERENCE pair the production walk measures its target on:
    /// `prev` at the reference origin, `cur` one nominal step further along +x.
    fn straight_reference_pair(
        prev: &GlyphContour,
        cur: &GlyphContour,
        step: f32,
    ) -> (PlacedGlyphInk, PlacedGlyphInk) {
        (
            PlacedGlyphInk {
                contour: prev.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0),
                center: [0.0, 0.0],
            },
            PlacedGlyphInk {
                contour: cur.placed(1.0, 0.0, 1.0, 1.0, step, 0.0),
                center: [step, 0.0],
            },
        )
    }

    /// A straight path of `len` running at `angle_rad` from the origin.
    fn straight_path_at(len: f32, angle_rad: f32) -> DrawnLinePath {
        let (sin, cos) = angle_rad.sin_cos();
        DrawnLinePath {
            points: vec![
                DrawnLinePoint {
                    x: 0.0,
                    y: 0.0,
                    arc_len_px: 0.0,
                },
                DrawnLinePoint {
                    x: len * cos,
                    y: len * sin,
                    arc_len_px: len,
                },
            ],
            total_len_px: len,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// A sharp V: 30px along +x, then a 131-degree turn back up-left to
    /// `(0, 34)`. Sharp enough that a glyph on the return leg reaches the one
    /// two places before it, which is what the clearance window exists for.
    fn sharp_v_path() -> DrawnLinePath {
        let tip_x = 30.0f32;
        let end = (0.0f32, 34.0f32);
        let return_len = ((tip_x - end.0).powi(2) + (end.1).powi(2)).sqrt();
        DrawnLinePath {
            points: vec![
                DrawnLinePoint {
                    x: 0.0,
                    y: 0.0,
                    arc_len_px: 0.0,
                },
                DrawnLinePoint {
                    x: tip_x,
                    y: 0.0,
                    arc_len_px: tip_x,
                },
                DrawnLinePoint {
                    x: end.0,
                    y: end.1,
                    arc_len_px: tip_x + return_len,
                },
            ],
            total_len_px: tip_x + return_len,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// The search range the production walk derives from a glyph's advance.
    fn search_range(advance: f32) -> f32 {
        advance.clamp(ON_PATH_INK_SEARCH_MIN_RANGE_PX, ON_PATH_INK_SEARCH_MAX_RANGE_PX)
    }

    #[test]
    fn a_uniform_run_on_a_straight_line_keeps_its_arc_length_positions() {
        // CONTRACT CHANGE. This test used to be called
        // `straight_line_reproduces_the_by_line_length_position` and asserted the
        // mode was a no-op on ANY straight line. That invariant is GONE: the mode
        // now normalizes every pair of a line onto ONE ink gap (the line's median
        // in the straight reference), so a straight run of MIXED side bearings is
        // deliberately re-spaced — that is the whole point, and it is why the
        // straight reference is the place the median is taken.
        // What survives, and is what this test pins, is the degenerate case a
        // reader still expects to hold: when every pair of the line already has
        // the SAME gap, the median IS that gap, the seed already satisfies it and
        // the search returns the seed untouched. Checked with the wedge, where
        // the facing edge is nowhere near the bbox half-width.
        let path = straight_path(200.0);
        let contour = wedge_contour(5.0);
        let step = 17.0f32;
        let (prev_ref, cur_ref) = straight_reference_pair(&contour, &contour, step);
        let target = on_path_pair_gap(&prev_ref, &cur_ref).expect("reference pair is measurable");

        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = step;
        let result = solve_directional_gap_center_s(
            seed,
            target,
            seed - search_range(step),
            seed + search_range(step),
            &prev,
            |s| ink_at(&path, &contour, s),
        )
        .expect("straight path should place the glyph");
        assert!((result - seed).abs() < 1e-6, "result={result}, seed={seed}");
    }

    #[test]
    fn a_uniform_run_on_an_angled_straight_line_also_keeps_its_seed() {
        // The same degenerate case for a straight line that is NOT axis aligned:
        // the real placement is then a rotated copy of the straight reference,
        // and `directional_pair_gap` derives its band, its sample count and its
        // sample offsets from dot products with the frame, so the rotation
        // cancels exactly and a target equal to the pair's own straight gap still
        // holds at the seed. (As above: this is the uniform-run case, not the
        // withdrawn "straight line == ByLineLength" invariant.)
        let angle = 0.7f32;
        let path = straight_path_at(200.0, angle);
        let contour = wedge_contour(5.0);
        let step = 17.0f32;
        let (prev_ref, cur_ref) = straight_reference_pair(&contour, &contour, step);
        let target = on_path_pair_gap(&prev_ref, &cur_ref).expect("reference pair is measurable");

        let prev = tangent_ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = step;
        let result = solve_directional_gap_center_s(
            seed,
            target,
            seed - search_range(step),
            seed + search_range(step),
            &prev,
            |s| tangent_ink_at(&path, &contour, s),
        )
        .expect("straight path should place the glyph");
        assert!((result - seed).abs() < 1e-4, "result={result}, seed={seed}");
    }

    #[test]
    fn straight_line_keeps_the_seed_when_the_gap_already_matches() {
        // The former `straight_line_keeps_arc_length_seed_when_gap_already_clears`,
        // kept: two width-10 squares at center distance 20 have a gap of 10,
        // which is what the straight reference asks for, so the seed stands.
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 20.0f32;
        let target = 10.0f32;

        let result = solve_directional_gap_center_s(seed, target, seed - 20.0, seed + 20.0, &prev, |s| {
            ink_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");

        assert!((result - seed).abs() < 1e-4, "result={result}");
    }

    #[test]
    fn a_too_close_seed_is_pushed_forward_to_the_target() {
        // The mechanics of the search, independent of where the target came
        // from: a seed that is too close for the target it was given is pushed
        // forward exactly far enough, and no further. Squares of half-extent 5:
        // gap = center distance - 10, so a target of 6 is reached at 16.
        // A target below the pair's own straight-line gap is a NORMAL input now —
        // the line median is below the loose pairs of the line by construction.
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 12.0f32;
        let target = 6.0f32;

        let result = solve_directional_gap_center_s(seed, target, seed - 8.0, seed + 8.0, &prev, |s| {
            ink_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");

        assert!((result - 16.0).abs() <= 0.05, "result={result}");
    }

    #[test]
    fn an_overshot_seed_is_pulled_back_to_the_target() {
        // The half of the search the old forward-only code could not do. It is
        // not a corner case: the walk chains each glyph off the CORRECTED
        // position of the previous one, so wherever the curvature eases the
        // inherited push makes the next pair too wide, and only a backward pull
        // keeps the run from drifting apart.
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 24.0f32;
        let target = 6.0f32;

        let result = solve_directional_gap_center_s(seed, target, seed - 12.0, seed + 12.0, &prev, |s| {
            ink_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");

        assert!(result < seed, "result={result} should be pulled back below {seed}");
        assert!((result - 16.0).abs() <= 0.05, "result={result}");
    }

    #[test]
    fn a_backward_pull_never_reaches_past_the_run_start() {
        // The backward half may not change what `drawn_line_drop_side` decides,
        // so the caller's `min_s` is a hard stop even when the target is not
        // reached there.
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 40.0f32;
        let min_s = 35.0f32;

        let result = solve_directional_gap_center_s(seed, 6.0, min_s, seed + 12.0, &prev, |s| {
            ink_at(&path, &contour, s)
        })
        .expect("straight path should place the glyph");
        assert!(result >= min_s, "result={result} ran past min_s={min_s}");
    }

    #[test]
    fn curved_path_keeps_the_gap_uniform_along_a_whole_run() {
        // The point of the whole mode, and what the previous implementation did
        // not deliver: a run of glyphs walking an arc keeps ONE gap, the one the
        // straight reference layout has. Six consecutive glyphs, not a single
        // pair, because the corrections chain.
        let path = semicircle_path(60.0, 256);
        let contour = square_contour(5.0);
        let step = 14.0f32;
        let (prev_ref, cur_ref) = straight_reference_pair(&contour, &contour, step);
        let target = on_path_pair_gap(&prev_ref, &cur_ref).expect("reference pair is measurable");

        // Plain arc-length walk first: it must genuinely be wrong, or the test
        // below would pass for the wrong reason.
        let mut worst_uncorrected = 0.0f32;
        for i in 0..6 {
            let a = tangent_ink_at(&path, &contour, 7.0 + step * i as f32).expect("sample");
            let b = tangent_ink_at(&path, &contour, 7.0 + step * (i + 1) as f32).expect("sample");
            let gap = on_path_pair_gap(&a, &b).expect("measurable");
            worst_uncorrected = worst_uncorrected.max((gap - target).abs());
        }
        assert!(
            worst_uncorrected > 0.3,
            "arc-length walk should deviate, worst={worst_uncorrected}"
        );

        let mut center = 7.0f32;
        let mut prev = tangent_ink_at(&path, &contour, center).expect("first sample");
        for i in 0..6 {
            let seed = center + step;
            let range = search_range(step);
            let placed_s = solve_directional_gap_center_s(
                seed,
                target,
                seed - range,
                seed + range,
                &prev,
                |s| tangent_ink_at(&path, &contour, s),
            )
            .expect("arc should have room");
            let cur = tangent_ink_at(&path, &contour, placed_s).expect("sample");
            let gap = on_path_pair_gap(&prev, &cur).expect("measurable");
            assert!(
                (gap - target).abs() <= 0.1,
                "pair {i}: gap={gap} target={target} at s={placed_s}"
            );
            prev = cur;
            center = placed_s;
        }
    }

    #[test]
    fn a_curved_run_of_mixed_glyphs_lands_on_the_one_line_target() {
        // The contract of the mode as the user states it: every adjacent pair of
        // the line ends up at the SAME ink-to-ink distance, whatever the glyphs
        // are. The run above uses one repeated square, where "uniform" would also
        // follow from every pair simply keeping its own straight gap; this one
        // ALTERNATES a wedge and a square, so the pairs have genuinely different
        // natural gaps and only a single shared target can make them equal.
        let path = semicircle_path(60.0, 256);
        let wedge = wedge_contour(5.0);
        let square = square_contour(5.0);
        let step = 15.0f32;
        let target = 4.0f32;

        let contour_at = |i: usize| if i.is_multiple_of(2) { &wedge } else { &square };

        let mut center = 8.0f32;
        let mut prev = tangent_ink_at(&path, contour_at(0), center).expect("first sample");
        let mut gaps = Vec::new();
        for i in 1..7 {
            let cur_contour = contour_at(i);
            let seed = center + step;
            let range = search_range(step);
            let placed_s = solve_directional_gap_center_s(
                seed,
                target,
                seed - range,
                seed + range,
                &prev,
                |s| tangent_ink_at(&path, cur_contour, s),
            )
            .expect("arc should have room");
            let cur = tangent_ink_at(&path, cur_contour, placed_s).expect("sample");
            let gap = on_path_pair_gap(&prev, &cur).expect("measurable");
            gaps.push(gap);
            prev = cur;
            center = placed_s;
        }

        for (i, gap) in gaps.iter().enumerate() {
            assert!(
                (gap - target).abs() <= 0.1,
                "pair {i}: gap={gap} target={target}, all gaps {gaps:?}"
            );
        }
    }

    #[test]
    fn the_straight_reference_walk_carries_the_nominal_step() {
        // The reference coordinate of a glyph is the previous glyph's reference
        // coordinate plus the difference between this glyph's arc-length SEED and
        // the previous glyph's FINAL center. That difference is the nominal step,
        // so a glyph's own correction (final != seed) must NOT leak into the
        // reference — otherwise the target would drift with the curve, which is
        // exactly what the reference exists to prevent.
        let mut walk = super::StraightReferenceWalk::default();
        // First glyph: no predecessor, so it anchors the reference origin.
        let first_x = walk.reference_x(7.0);
        assert!((first_x - 0.0).abs() < 1e-6, "first_x={first_x}");
        // It is then CORRECTED forward to 9.0 before being committed.
        walk.commit(first_x, 9.0);
        // Second glyph seeds at 9.0 + 20.0: the reference must advance by the
        // nominal 20, not by the 22 that the seed minus the first glyph's own
        // seed would give.
        let second_x = walk.reference_x(29.0);
        assert!((second_x - 20.0).abs() < 1e-6, "second_x={second_x}");
        walk.commit(second_x, 25.0);
        // Third glyph, nominal step 20 again off the CORRECTED 25.0.
        let third_x = walk.reference_x(45.0);
        assert!((third_x - 40.0).abs() < 1e-6, "third_x={third_x}");
    }

    #[test]
    fn curved_path_pushes_center_forward_past_overlap() {
        // Kept from the original suite: on a semicircle the arc-length seed
        // makes the pair overlap (chord < arc), and the search must push the
        // center forward until the target gap is reached.
        let path = semicircle_path(50.0, 96);
        let contour = square_contour(5.0);
        let prev = tangent_ink_at(&path, &contour, 0.0).expect("prev sample");
        let seed = 6.0f32;
        let target = 4.0f32;

        // Confirm the seed really overlaps, so the test is meaningful.
        let seed_ink = tangent_ink_at(&path, &contour, seed).expect("seed sample");
        assert!(
            on_path_pair_gap(&prev, &seed_ink).expect("measurable") < 0.0,
            "seed should overlap"
        );

        let result =
            solve_directional_gap_center_s(seed, target, seed - 6.0, seed + 12.0, &prev, |s| {
                tangent_ink_at(&path, &contour, s)
            })
            .expect("semicircle should have room");

        assert!(result > seed, "result={result} should exceed seed={seed}");
        let placed = tangent_ink_at(&path, &contour, result).expect("sample");
        let gap = on_path_pair_gap(&prev, &placed).expect("measurable");
        assert!((gap - target).abs() <= 0.1, "gap={gap} at result={result}");
    }

    #[test]
    fn pair_gap_does_not_depend_on_which_end_it_is_measured_from() {
        // The reason the measuring axis is the pair's CHORD and not either
        // glyph's tangent: the walk must not be able to change spacing by the
        // order it visits a pair in.
        let path = semicircle_path(45.0, 128);
        let left = wedge_contour(6.0);
        let right = square_contour(4.0);
        let a = tangent_ink_at(&path, &left, 10.0).expect("sample a");
        let b = tangent_ink_at(&path, &right, 24.0).expect("sample b");

        let forward = on_path_pair_gap(&a, &b).expect("measurable");
        let backward = on_path_pair_gap(&b, &a).expect("measurable");
        assert!(
            (forward - backward).abs() < 1e-3,
            "forward={forward} backward={backward}"
        );
    }

    #[test]
    fn degenerate_chord_has_no_measuring_frame() {
        // Two glyphs placed on the same point (a path that doubles back exactly)
        // have no advance direction at all; the metric must say so instead of
        // dividing by zero.
        let contour = square_contour(5.0);
        let ink = PlacedGlyphInk {
            contour: contour.placed(1.0, 0.0, 1.0, 1.0, 10.0, 10.0),
            center: [10.0, 10.0],
        };
        assert!(pair_chord_axis(ink.center, ink.center).is_none());
        assert!(on_path_pair_gap(&ink, &ink).is_none());
    }

    #[test]
    fn a_negative_authored_gap_is_not_pulled_apart() {
        // A user-authored kerning pair that overlaps the two inks is an
        // INTENTION, and the mode must not undo it. Two independent guards keep
        // it: the walk EXEMPTS an authored pair from the normalization entirely
        // (`custom_seed_pair_delta_px`, checked by
        // `an_authored_pair_is_excluded_from_the_line_target`), and the safety
        // net's floor collapses to the overlap the straight reference itself has
        // — which is what this test pins, together with the search's own
        // behaviour when it IS handed the pair's overlapping gap as a target.
        let path = straight_path(200.0);
        let contour = square_contour(5.0);
        let step = 8.0f32; // center distance 8 < 10: the inks overlap by 2.
        let (prev_ref, cur_ref) = straight_reference_pair(&contour, &contour, step);
        let target = on_path_pair_gap(&prev_ref, &cur_ref).expect("measurable");
        assert!(target < 0.0, "the reference pair must overlap, target={target}");

        let prev = ink_at(&path, &contour, 0.0).expect("prev sample");
        let result = solve_directional_gap_center_s(
            step,
            target,
            step - search_range(step),
            step + search_range(step),
            &prev,
            |s| ink_at(&path, &contour, s),
        )
        .expect("straight path should place the glyph");
        assert!((result - step).abs() < 1e-6, "result={result}");

        // And the safety net must not undo it either.
        let floor = pair_clearance_floor(&prev_ref.contour, &cur_ref.contour);
        assert_eq!(floor, 0.0, "an overlapping reference pair has no floor");
        assert!(clears_floor(&prev.contour, &cur_ref.contour, floor));
    }

    #[test]
    fn a_diagonal_approach_is_caught_by_the_clearance_floor() {
        // The blind spot the omnidirectional floor covers. The directional
        // metric only ever compares the two glyphs WITHIN one scanline, so an
        // overhang that reaches towards the neighbour on scanlines the
        // neighbour has no ink on is invisible to it — the classic `Г` arm over
        // the next letter's shoulder. Hand-placed so the two measures disagree
        // by construction: on the shared band `Г`'s facing edge is its stem
        // (gap 10), while the tip of its arm sits 0.32px from the neighbour's
        // corner.
        let arm = GlyphContour {
            components: vec![vec![
                [-6.0, -6.0],
                [9.7, -6.0],
                [9.7, -4.0],
                [0.0, -4.0],
                [0.0, 6.0],
                [-6.0, 6.0],
            ]],
        };
        let block = GlyphContour {
            components: vec![vec![
                [-4.0, -3.9],
                [4.0, -3.9],
                [4.0, 3.9],
                [-4.0, 3.9],
            ]],
        };
        // Ink-box centers: `arm` spans x in [-6, 9.7] so its box center is at
        // 1.85; `block` is centered on its own placement point.
        let prev = PlacedGlyphInk {
            contour: arm.placed(1.0, 0.0, 1.0, 1.0, 0.0, 0.0),
            center: [1.85, 0.0],
        };
        let seed = 14.0f32;
        let place = |s: f32| PlacedGlyphInk {
            contour: block.placed(1.0, 0.0, 1.0, 1.0, s, 0.0),
            center: [s, 0.0],
        };

        let seed_ink = place(seed);
        let directional = on_path_pair_gap(&prev, &seed_ink).expect("measurable chord");
        let euclidean = min_placed_distance(&prev.contour, &seed_ink.contour);
        assert!(
            directional > 5.0,
            "the directional measure must be comfortable here, got {directional}"
        );
        assert!(
            euclidean < ON_PATH_INK_CLEARANCE_FLOOR_PX,
            "the diagonal approach must violate the floor, got {euclidean}"
        );
        assert!(!clears_floor(
            &prev.contour,
            &seed_ink.contour,
            ON_PATH_INK_CLEARANCE_FLOOR_PX
        ));

        // The safety net pushes forward until the omnidirectional clearance is
        // met, and no further.
        let path = straight_path(200.0);
        let neighbors = [PlacedNeighbor {
            world: prev.clone(),
            straight: prev.clone(),
        }];
        let result = find_clearance_center_s(
            &path,
            seed,
            &neighbors,
            &[ON_PATH_INK_CLEARANCE_FLOOR_PX],
            |s| Some(place(s).contour),
        )
        .expect("a straight path has room");
        assert!(result > seed, "result={result} should exceed seed={seed}");
        assert!(
            result - seed < 1.0,
            "the push must stay minimal, result={result}"
        );
        assert!(
            min_placed_distance(&prev.contour, &place(result).contour)
                >= ON_PATH_INK_CLEARANCE_FLOOR_PX,
            "floor not met at result={result}"
        );
    }

    #[test]
    fn clearance_window_catches_the_glyph_before_the_predecessor() {
        // On a sharp turn a glyph can walk into the one BEFORE its predecessor
        // while staying clear of the predecessor itself — here the predecessor
        // is a narrow glyph (a period, a comma) sitting on the tip of the V, so
        // it occupies almost none of the space the returning glyph sweeps
        // through. With only the immediate neighbour in the window (what the
        // code used to keep) the seed passes; with the window it is pushed.
        let path = sharp_v_path();
        let wide = square_contour(7.0);
        let narrow = square_contour(0.8);
        let older = tangent_ink_at(&path, &wide, 16.0).expect("i-2 sample");
        let previous = tangent_ink_at(&path, &narrow, 28.0).expect("i-1 sample");
        let seed = 44.0f32;
        let seed_ink = tangent_ink_at(&path, &wide, seed).expect("seed sample");

        assert!(
            min_placed_distance(&previous.contour, &seed_ink.contour)
                >= ON_PATH_INK_CLEARANCE_FLOOR_PX,
            "the predecessor must NOT be the one blocking, or the test proves nothing"
        );
        assert!(
            min_placed_distance(&older.contour, &seed_ink.contour) < ON_PATH_INK_CLEARANCE_FLOOR_PX,
            "the i-2 glyph must be the one collided with"
        );

        let only_previous = [PlacedNeighbor {
            world: previous.clone(),
            straight: previous.clone(),
        }];
        let kept = find_clearance_center_s(
            &path,
            seed,
            &only_previous,
            &[ON_PATH_INK_CLEARANCE_FLOOR_PX],
            |s| tangent_ink_at(&path, &wide, s).map(|ink| ink.contour),
        )
        .expect("sampling");
        assert!((kept - seed).abs() < 1e-6, "a one-glyph window misses it: {kept}");

        let window = [
            PlacedNeighbor {
                world: older.clone(),
                straight: older.clone(),
            },
            PlacedNeighbor {
                world: previous.clone(),
                straight: previous.clone(),
            },
        ];
        let floors = [
            ON_PATH_INK_CLEARANCE_FLOOR_PX,
            ON_PATH_INK_CLEARANCE_FLOOR_PX,
        ];
        let result = find_clearance_center_s(&path, seed, &window, &floors, |s| {
            tangent_ink_at(&path, &wide, s).map(|ink| ink.contour)
        })
        .expect("the return leg should have room");
        assert!(result > seed, "result={result} should exceed seed={seed}");
        let placed = tangent_ink_at(&path, &wide, result).expect("sample");
        assert!(
            min_placed_distance(&older.contour, &placed.contour) >= ON_PATH_INK_CLEARANCE_FLOOR_PX,
            "the i-2 clearance is still violated at result={result}"
        );
    }

    #[test]
    fn empty_current_contour_falls_back_to_arc_length_seed() {
        // A glyph with no ink (a space) is unmeasurable in both passes: the
        // directional metric reports INFINITY at every candidate, so no sign
        // change exists and the seed stands; the clearance predicate is
        // satisfied outright. The production walk never even gets here (an
        // inkless seed has no `SeedInkGeometry`), but the searches must not
        // misbehave if it ever does.
        let path = straight_path(200.0);
        let prev = ink_at(&path, &square_contour(5.0), 0.0).expect("prev sample");
        let seed = 25.0f32;
        // No components at all: the same shape a space glyph would have.
        let empty = PlacedGlyphInk {
            contour: PlacedContour::default(),
            center: [seed, 0.0],
        };

        let result = solve_directional_gap_center_s(seed, 6.0, seed - 8.0, seed + 8.0, &prev, |_s| {
            Some(empty.clone())
        })
        .expect("empty ink should place at the seed");
        assert!((result - seed).abs() < 1e-6, "result={result}");

        let neighbors = [PlacedNeighbor {
            world: prev.clone(),
            straight: prev,
        }];
        let cleared =
            find_clearance_center_s(&path, seed, &neighbors, &[ON_PATH_INK_CLEARANCE_FLOOR_PX], |_s| {
                Some(empty.contour.clone())
            })
            .expect("empty ink clears every floor");
        assert!((cleared - seed).abs() < 1e-6, "cleared={cleared}");
    }

    #[test]
    fn line_placement_helper_sign_matches_top_bottom_intent() {
        // Horizontal line (rotation 0): the line's DOWN normal is +y in screen
        // y-down space, so a positive `line_frac` (сверху/top) must move the
        // glyph UP (smaller y), a negative one DOWN, and 0 must stay centered.
        let ink_height = 20.0f32;
        let (cx0, cy0) = apply_line_placement(100.0, 50.0, 0.0, ink_height, 0.0);
        assert!(
            (cx0 - 100.0).abs() < 1e-6 && (cy0 - 50.0).abs() < 1e-6,
            "0% must keep the ink center on the line: ({cx0}, {cy0})"
        );

        let (_, cy_top) = apply_line_placement(100.0, 50.0, 0.0, ink_height, 1.0);
        let (_, cy_bottom) = apply_line_placement(100.0, 50.0, 0.0, ink_height, -1.0);
        assert!(cy_top < cy0, "+100% (сверху) must move UP: {cy_top} !< {cy0}");
        assert!(
            cy_bottom > cy0,
            "-100% (снизу) must move DOWN: {cy_bottom} !> {cy0}"
        );
        // Magnitude at the extremes is half the ink height.
        assert!((cy0 - cy_top - ink_height * 0.5).abs() < 1e-4);
        assert!((cy_bottom - cy0 - ink_height * 0.5).abs() < 1e-4);
    }

    #[test]
    fn placed_contour_stays_within_composited_world_rect() {
        // A contour spanning the glyph outline bbox must land inside the same
        // rotated, scaled world rect the blit draws into. This guards the
        // pivot/translation math in placed_contour_for_transform, which now
        // works in the outline (pen-relative, y-down px) frame.
        let glyph_w = 10.0f32;
        let glyph_h = 8.0f32;
        let width_mul = 1.5f32;
        let height_mul = 1.5f32;
        // With placement (0, 0), the outline bbox spans [0, glyph_w] x
        // [0, glyph_h] and its center is the bitmap center (glyph_w/2, glyph_h/2).
        let placement_left = 0.0f32;
        let placement_top = 0.0f32;
        let contour = GlyphContour {
            components: vec![vec![
                [0.0, 0.0],
                [glyph_w, 0.0],
                [glyph_w, glyph_h],
                [0.0, glyph_h],
            ]],
        };

        // Scaled rect for src at origin, mirroring GlyphScaleSettings::scaled_rect.
        let scaled_width = glyph_w * width_mul;
        let scaled_height = glyph_h * height_mul;
        let scaled_left = glyph_w * 0.5 - scaled_width * 0.5;
        let scaled_top = glyph_h * 0.5 - scaled_height * 0.5;
        // 0% line placement: the ink center sits on the path point.
        let line_frac = 0.0f32;

        let transform = DrawnLineTransform {
            center_x: 40.0,
            center_y: 25.0,
            rotation_rad: 0.7,
        };
        let (dst_cx, dst_cy) = drawn_line_glyph_destination_center_raw(
            &transform,
            scaled_height,
            placement_top * height_mul,
            line_frac,
            LinePlacementReference::GlyphHeight,
            0.0,
        );
        let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
            scaled_left,
            scaled_top,
            scaled_width,
            scaled_height,
            dst_cx,
            dst_cy,
            transform.rotation_rad,
        );

        let placed = placed_contour_for_transform(
            &contour,
            placement_left,
            placement_top,
            glyph_w,
            glyph_h,
            width_mul,
            height_mul,
            0.0,
            scaled_height,
            line_frac,
            LinePlacementReference::GlyphHeight,
            0.0,
            [0.0, 0.0],
            &transform,
        );

        let eps = 0.01f32;
        assert!(placed.aabb_min[0] >= min_x - eps, "min_x");
        assert!(placed.aabb_min[1] >= min_y - eps, "min_y");
        assert!(placed.aabb_max[0] <= max_x + eps, "max_x");
        assert!(placed.aabb_max[1] <= max_y + eps, "max_y");
    }

    #[test]
    fn line_box_reference_shares_one_baseline_across_glyph_heights() {
        // LineBox must anchor EVERY glyph to one shared baseline regardless of its
        // own ink height / top bearing (the fix for glyphs "jumping" on the line),
        // while GlyphHeight (legacy) centers each glyph by its own height.
        let transform = DrawnLineTransform {
            center_x: 40.0,
            center_y: 25.0,
            rotation_rad: 0.0, // horizontal line: DOWN normal is +y, so baseline math is trivial.
        };
        let ascent_scaled = 30.0f32;
        let line_frac = 0.0f32;
        // (scaled_height, placement_top_scaled): a tall cap vs a short x-height glyph.
        let cap = (20.0f32, 20.0f32);
        let low = (14.0f32, 9.0f32);
        // At rotation 0 the bitmap center is (scaled_height/2 - placement_top_scaled)
        // BELOW the baseline, so baseline = dst_cy - (h/2 - top).
        let baseline = |reference, (h, top): (f32, f32)| {
            let (_dx, dy) = drawn_line_glyph_destination_center_raw(
                &transform,
                h,
                top,
                line_frac,
                reference,
                ascent_scaled,
            );
            dy - (h * 0.5 - top)
        };

        let cap_base = baseline(LinePlacementReference::LineBox, cap);
        let low_base = baseline(LinePlacementReference::LineBox, low);
        assert!(
            (cap_base - low_base).abs() < 1e-4,
            "LineBox baselines must match: {cap_base} vs {low_base}"
        );
        // The shared baseline sits `ascent/2 * (1 - line_frac)` below the path point.
        let expected = transform.center_y + ascent_scaled * 0.5 * (1.0 - line_frac);
        assert!(
            (cap_base - expected).abs() < 1e-4,
            "LineBox baseline={cap_base} expected={expected}"
        );

        // Legacy per-glyph centering does NOT share a baseline.
        let cap_gh = baseline(LinePlacementReference::GlyphHeight, cap);
        let low_gh = baseline(LinePlacementReference::GlyphHeight, low);
        assert!(
            (cap_gh - low_gh).abs() > 1.0,
            "GlyphHeight must not share a baseline: {cap_gh} vs {low_gh}"
        );
    }
    /// Non-justified alignment with an explicit bias, for readable test intent.
    fn biased(bias: f32) -> HorizontalAlign {
        HorizontalAlign {
            bias,
            justify: false,
        }
    }

    /// A minimal seed for the alignment pre-pass, which only reads `line_idx`,
    /// `line_align`, `advance_px` and `extended_offset`. Everything else is
    /// inert filler so no font machinery is needed.
    fn align_seed(
        line_idx: usize,
        advance_px: f32,
        line_align: HorizontalAlign,
        line_px: f32,
    ) -> FormulaGlyphSeed {
        FormulaGlyphSeed {
            glyph: LayoutGlyph {
                start: 0,
                end: 1,
                font_size: 10.0,
                line_height_opt: None,
                font_id: fontdb::ID::dummy(),
                glyph_id: 0,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                level: 0u8.into(),
                x_offset: 0.0,
                y_offset: 0.0,
                color_opt: None,
                metadata: 0,
                cache_key_flags: CacheKeyFlags::empty(),
            },
            // This fixture exercises ALIGNMENT arithmetic only, with no font behind
            // it, so there is no source cluster and no override to resolve.
            cluster_char: None,
            text_color: [0, 0, 0, 255],
            origin_x: 0.0,
            origin_y: 0.0,
            kerning: KerningSettings {
                mode: KerningMode::Fixed,
                spacing_px: 0.0,
                spacing_percent: 0.0,
                custom_pairs: false,
            },
            glyph_scale: GlyphScaleSettings {
                width_mul: 1.0,
                height_mul: 1.0,
            },
            glyph_offset_px: [0.0, 0.0],
            extended_offset: InlineGlyphOffset {
                line_px,
                ..InlineGlyphOffset::global_only([0.0, 0.0])
            },
            style_offset: 0,
            offset_span_range: None,
            line_idx,
            glyph_idx_in_line: 0,
            glyphs_in_line: 1,
            line_align,
            advance_px,
            faux: FauxGlyphStyle::NONE,
            hanging_excluded: false,
        }
    }

    /// `count` identical seeds on `line_idx`.
    fn align_seeds(
        line_idx: usize,
        count: usize,
        advance_px: f32,
        line_align: HorizontalAlign,
        line_px: f32,
    ) -> Vec<FormulaGlyphSeed> {
        (0..count)
            .map(|_| align_seed(line_idx, advance_px, line_align, line_px))
            .collect()
    }

    /// Walk `count` glyphs of equal `advance` from `start_s` through the shared
    /// production cursor helpers and return their arc-length centers.
    fn run_centers(start_s: f32, advance: f32, line_offset_px: f32, count: usize) -> Vec<f32> {
        let mut cursor = start_s;
        let mut centers = Vec::with_capacity(count);
        for _ in 0..count {
            let center_s = drawn_line_center_s(cursor, advance, line_offset_px);
            centers.push(center_s);
            cursor = drawn_line_next_cursor(center_s, advance, line_offset_px);
        }
        centers
    }

    /// `(dropped before the line start, dropped past its end)`, decided by the
    /// production guard `drawn_line_drop_side` — never by a test-local copy.
    fn drop_counts(centers: &[f32], start_s: f32, total_len_px: f32) -> (usize, usize) {
        let mut before = 0usize;
        let mut past = 0usize;
        for center_s in centers {
            match drawn_line_drop_side(*center_s, start_s, total_len_px) {
                Some(DrawnLineDropSide::BeforeStart) => before += 1,
                Some(DrawnLineDropSide::PastEnd) => past += 1,
                None => {}
            }
        }
        (before, past)
    }

    #[test]
    fn on_path_align_fraction_matches_the_bias_and_keeps_justify_defaults() {
        assert!((on_path_align_fraction(HorizontalAlign::LEFT, 0.0) - 0.0).abs() < 1e-6);
        assert!((on_path_align_fraction(HorizontalAlign::CENTER, 0.0) - 0.5).abs() < 1e-6);
        assert!((on_path_align_fraction(HorizontalAlign::RIGHT, 0.0) - 1.0).abs() < 1e-6);
        // Justify hides the slider in the UI, so `bias` must never be read: each
        // path keeps its own historical default no matter what bias is stored.
        let justified = HorizontalAlign {
            bias: 1.0,
            justify: true,
        };
        assert!((on_path_align_fraction(justified, 0.0) - 0.0).abs() < 1e-6);
        assert!((on_path_align_fraction(justified, 0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn formula_arc_length_splits_the_free_curve_by_the_align_fraction() {
        // Run of 100px on a 300px curve: 200px of slack to distribute.
        let center_s = 50.0f32;
        let start = map_formula_target_arc_length(center_s, 100.0, 300.0, 0.0);
        let center = map_formula_target_arc_length(center_s, 100.0, 300.0, 0.5);
        let end = map_formula_target_arc_length(center_s, 100.0, 300.0, 1.0);
        assert!((start - 50.0).abs() < 1e-3, "start={start}");
        assert!((center - 150.0).abs() < 1e-3, "center={center}");
        assert!((end - 250.0).abs() < 1e-3, "end={end}");
    }

    #[test]
    fn formula_arc_length_keeps_compressing_on_overflow_regardless_of_alignment() {
        // 400px of text on a 200px curve: compressed 2x, no slack left to bias.
        // `Shape`'s fallback threshold is defined against exactly this ratio.
        let expected = 50.0f32;
        for fraction in [0.0f32, 0.5, 1.0] {
            let mapped = map_formula_target_arc_length(100.0, 400.0, 200.0, fraction);
            assert!(
                (mapped - expected).abs() < 1e-3,
                "fraction={fraction} mapped={mapped}"
            );
        }
    }

    #[test]
    fn drawn_line_start_offset_pins_a_fitting_run_to_the_biased_side() {
        // 100px run on a 300px line.
        let left = drawn_line_start_offset(300.0, 100.0, HorizontalAlign::LEFT);
        let center = drawn_line_start_offset(300.0, 100.0, HorizontalAlign::CENTER);
        let right = drawn_line_start_offset(300.0, 100.0, HorizontalAlign::RIGHT);
        assert!((left - 0.0).abs() < 1e-3, "left={left}");
        assert!((center - 100.0).abs() < 1e-3, "center={center}");
        assert!((right - 200.0).abs() < 1e-3, "right={right}");
        // Justified lines keep the historical start-of-line placement.
        assert!(
            drawn_line_start_offset(300.0, 100.0, HorizontalAlign::JUSTIFY).abs() < 1e-3,
            "justify must not move the run"
        );
    }

    #[test]
    fn drawn_line_start_offset_goes_negative_when_the_run_overflows() {
        // 300px run on a 100px line: the free space is -200px and must NOT be
        // clamped, otherwise an end-biased run could never clip at its start.
        let center = drawn_line_start_offset(100.0, 300.0, HorizontalAlign::CENTER);
        let right = drawn_line_start_offset(100.0, 300.0, HorizontalAlign::RIGHT);
        assert!((center + 100.0).abs() < 1e-3, "center={center}");
        assert!((right + 200.0).abs() < 1e-3, "right={right}");
    }

    #[test]
    fn start_offsets_place_each_line_run_by_its_own_align() {
        // Line 0: 3x20px = a 60px run on a 300px line (fits, 240px of slack).
        // Line 1: 5x40px = a 200px run on a 100px line (overflows by 100px).
        let paths = vec![Some(straight_path(300.0)), Some(straight_path(100.0))];
        for (align, fitting, overflowing) in [
            (HorizontalAlign::LEFT, 0.0f32, 0.0f32),
            (HorizontalAlign::CENTER, 120.0, -50.0),
            (HorizontalAlign::RIGHT, 240.0, -100.0),
        ] {
            let mut seeds = align_seeds(0, 3, 20.0, align, 0.0);
            seeds.extend(align_seeds(1, 5, 40.0, align, 0.0));
            let offsets = drawn_line_start_offsets(seeds.as_slice(), paths.as_slice(), OnPathStepSpacing::for_custom_lines(1.0, 0.0));
            let line0 = offsets.get(&0).copied().unwrap_or(f32::NAN);
            let line1 = offsets.get(&1).copied().unwrap_or(f32::NAN);
            assert!((line0 - fitting).abs() < 1e-3, "{align:?} line0={line0}");
            assert!(
                (line1 - overflowing).abs() < 1e-3,
                "{align:?} line1={line1}"
            );
        }
    }

    #[test]
    fn start_offsets_skip_lines_without_a_path() {
        // Line 1 has no path, so it must be absent from the map (and the caller
        // then starts it at 0.0) instead of dividing by a phantom line length.
        let paths = vec![Some(straight_path(300.0)), None];
        let mut seeds = align_seeds(0, 3, 20.0, HorizontalAlign::RIGHT, 0.0);
        seeds.extend(align_seeds(1, 3, 20.0, HorizontalAlign::RIGHT, 0.0));
        // Line 2 is past the end of `paths` entirely.
        seeds.extend(align_seeds(2, 3, 20.0, HorizontalAlign::RIGHT, 0.0));
        let offsets = drawn_line_start_offsets(seeds.as_slice(), paths.as_slice(), OnPathStepSpacing::for_custom_lines(1.0, 0.0));
        assert!(offsets.contains_key(&0), "line 0 has a path");
        assert!(!offsets.contains_key(&1), "line 1 has no path");
        assert!(!offsets.contains_key(&2), "line 2 is out of range");
    }

    #[test]
    fn a_manually_nudged_glyph_does_not_move_its_line_alignment() {
        // `run_len_px` is the FINAL CURSOR, and a plain inline `line_px` offset
        // moves only its own glyph. So a nudged run must align identically to an
        // unnudged one of the same advances, in either direction.
        let paths = vec![Some(straight_path(300.0))];
        let plain = drawn_line_start_offsets(
            align_seeds(0, 3, 20.0, HorizontalAlign::CENTER, 0.0).as_slice(),
            paths.as_slice(),
            OnPathStepSpacing::for_custom_lines(1.0, 0.0),
        );
        for line_px in [-25.0f32, 25.0] {
            let nudged = drawn_line_start_offsets(
                align_seeds(0, 3, 20.0, HorizontalAlign::CENTER, line_px).as_slice(),
                paths.as_slice(),
                OnPathStepSpacing::for_custom_lines(1.0, 0.0),
            );
            assert_eq!(
                plain.get(&0).copied().map(f32::to_bits),
                nudged.get(&0).copied().map(f32::to_bits),
                "line_px={line_px} must not move the alignment"
            );
        }
    }

    #[test]
    fn start_offsets_apply_letter_spacing_to_the_measured_run() {
        // 3 glyphs, advance 20 -> run 60; with a 2x multiplier -> 120; the
        // centered start on a 300px line moves from 120 to 90 accordingly.
        let paths = vec![Some(straight_path(300.0))];
        let seeds = align_seeds(0, 3, 20.0, HorizontalAlign::CENTER, 0.0);
        let doubled = drawn_line_start_offsets(seeds.as_slice(), paths.as_slice(), OnPathStepSpacing::for_custom_lines(2.0, 0.0))
            .get(&0)
            .copied()
            .unwrap_or(f32::NAN);
        assert!((doubled - 90.0).abs() < 1e-3, "doubled={doubled}");
    }

    #[test]
    fn overlong_run_is_clipped_on_the_side_the_bias_points_away_from() {
        // Five 40px glyphs = a 200px run on a 100px line: 100px must be cut.
        let paths = vec![Some(straight_path(100.0))];
        let advance = 40.0f32;
        for (align, expected, intent) in [
            (
                HorizontalAlign::LEFT,
                (0usize, 2usize),
                "bias<0 must start at the line start and cut at the END",
            ),
            (
                HorizontalAlign::CENTER,
                (1, 1),
                "bias==0 must cut evenly at both ends",
            ),
            (
                HorizontalAlign::RIGHT,
                (2, 0),
                "bias>0 must end at the line end and cut at the START",
            ),
        ] {
            let seeds = align_seeds(0, 5, advance, align, 0.0);
            let start = drawn_line_start_offsets(seeds.as_slice(), paths.as_slice(), OnPathStepSpacing::for_custom_lines(1.0, 0.0))
                .get(&0)
                .copied()
                .unwrap_or(f32::NAN);
            let centers = run_centers(start, advance, 0.0, 5);
            assert_eq!(
                drop_counts(&centers, start, 100.0),
                expected,
                "{intent}: start={start} centers={centers:?}"
            );
        }
    }

    #[test]
    fn fitting_run_keeps_every_glyph_on_the_line_for_any_bias() {
        // Three 20px glyphs = a 60px run on a 100px line: nothing may drop, and
        // an end-biased run must finish exactly at the line end.
        let total_len = 100.0f32;
        let run_len = 60.0f32;
        for bias in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            let start = drawn_line_start_offset(total_len, run_len, biased(bias));
            let centers = run_centers(start, 20.0, 0.0, 3);
            assert_eq!(
                drop_counts(&centers, start, total_len),
                (0, 0),
                "bias={bias} must keep every glyph: {centers:?}"
            );
        }
        let end_start = drawn_line_start_offset(total_len, run_len, HorizontalAlign::RIGHT);
        let end_centers = run_centers(end_start, 20.0, 0.0, 3);
        let last_end = end_centers.last().copied().unwrap_or(0.0) + 10.0;
        assert!(
            (last_end - total_len).abs() < 1e-3,
            "end-aligned run must finish at the line end: {last_end}"
        );
    }

    #[test]
    fn a_negative_inline_nudge_is_clamped_and_drawn_not_dropped() {
        // Regression guard: on a line the run FITS (start offset >= 0) a glyph
        // pushed before the path start by a manual `<offset>` must keep its
        // historical clamp-and-draw behaviour, NOT vanish into the
        // "не отрисовано символов" warning.
        assert_eq!(drawn_line_drop_side(-30.0, 0.0, 100.0), None);
        assert_eq!(drawn_line_drop_side(-30.0, 40.0, 100.0), None);
        // The same position IS dropped once the alignment itself pushed the run
        // off the line start.
        assert_eq!(
            drawn_line_drop_side(-30.0, -50.0, 100.0),
            Some(DrawnLineDropSide::BeforeStart)
        );
        // Past the end is always a drop, whatever the start offset was, and the
        // boundaries themselves are inclusive.
        assert_eq!(
            drawn_line_drop_side(130.0, 0.0, 100.0),
            Some(DrawnLineDropSide::PastEnd)
        );
        assert_eq!(drawn_line_drop_side(0.0, -50.0, 100.0), None);
        assert_eq!(drawn_line_drop_side(100.0, 0.0, 100.0), None);
    }

    #[test]
    fn inline_line_offset_shifts_only_its_own_glyph() {
        // The cursor recurrence must cancel `extended_offset.line_px`; that is
        // what makes the final cursor a pure `Sum(advance)` run length.
        let shifted = run_centers(0.0, 20.0, 7.0, 4);
        let plain = run_centers(0.0, 20.0, 0.0, 4);
        for (idx, (a, b)) in shifted.iter().zip(plain.iter()).enumerate() {
            assert!(
                (a - b - 7.0).abs() < 1e-3,
                "glyph {idx}: shifted={a} plain={b}"
            );
        }
        let last_end = plain.last().copied().unwrap_or(0.0) + 10.0;
        assert!((last_end - 80.0).abs() < 1e-3, "run length={last_end}");
    }

    /// Fixture font for the seed-advance contract: the same Liberation Sans the
    /// pipeline tests use. A test binary cannot reach the shipped `fonts/ui` bundle,
    /// and this contract is about the advance arithmetic, not about the bundle.
    fn seed_fixture_font_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// Shapes `text` with the fixture font and turns its first layout run into the
    /// seeds `assign_formula_seed_advances` consumes, with the custom-pair flag SET
    /// so the run cannot take the default-metric fast path.
    ///
    /// Returns the font system, the registered face id (what an override binds to)
    /// and the seeds, one per shaped glyph, all on line 0.
    fn formula_seeds_for(
        text: &str,
        font_size_px: f32,
        mode: KerningMode,
    ) -> (cosmic_text::FontSystem, fontdb::ID, Vec<FormulaGlyphSeed>) {
        use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};

        let bytes = std::fs::read(seed_fixture_font_path()).expect("fixture font bytes");
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        let mut buffer = Buffer::new(
            &mut font_system,
            Metrics::new(font_size_px, font_size_px * 1.2),
        );
        buffer.set_size(&mut font_system, None, None);
        let attrs = Attrs::new()
            .family(Family::Name(family.as_str()))
            .metrics(Metrics::new(font_size_px, font_size_px));
        buffer.set_text(&mut font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);

        let run = buffer.layout_runs().next().expect("one layout run");
        let glyph_count = run.glyphs.len();
        let seeds = run
            .glyphs
            .iter()
            .enumerate()
            .map(|(idx, glyph)| FormulaGlyphSeed {
                glyph: glyph.clone(),
                cluster_char: run.text.get(glyph.start..glyph.end).and_then(|cluster| {
                    let mut chars = cluster.chars();
                    let first = chars.next()?;
                    chars.next().is_none().then_some(first)
                }),
                text_color: [0, 0, 0, 255],
                origin_x: 0.0,
                origin_y: 0.0,
                kerning: KerningSettings {
                    mode,
                    spacing_px: 0.0,
                    spacing_percent: 0.0,
                    // Set unconditionally: the production caller sets it from the
                    // render's `CustomKerningMap`, and leaving it false would take the
                    // whole run down the fast path that ignores overrides.
                    custom_pairs: true,
                },
                glyph_scale: GlyphScaleSettings {
                    width_mul: 1.0,
                    height_mul: 1.0,
                },
                glyph_offset_px: [0.0, 0.0],
                extended_offset: InlineGlyphOffset::global_only([0.0, 0.0]),
                style_offset: 0,
                offset_span_range: None,
                line_idx: 0,
                glyph_idx_in_line: idx,
                glyphs_in_line: glyph_count,
                line_align: biased(0.0),
                advance_px: 0.0,
                faux: FauxGlyphStyle::NONE,
                hanging_excluded: false,
            })
            .collect::<Vec<_>>();
        // `run` borrows `buffer`, but every seed OWNS its data (the glyph is
        // cloned, the cluster char is copied), so NLL ends that borrow right
        // here. `buffer` is local and dies with the fixture.
        (font_system, face_id, seeds)
    }

    /// A `TextRenderParams` whose single vector line runs in
    /// `MinimumPreviousDistance`, which is what
    /// [`super::straight_reference_target_gaps`] needs to measure a line at all.
    ///
    /// The path itself is irrelevant to the pre-pass (the reference is a virtual
    /// straight line), so it is a token two-point segment.
    fn min_distance_line_params() -> crate::types::TextRenderParams {
        use crate::types::{
            AntiAliasingMode, TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode,
            TextLineMode, TextRenderParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
            TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint,
            TextWrapMode, VerticalLineDirection,
        };
        TextRenderParams {
            text: String::new(),
            text_color: [255, 255, 255, 255],
            font_name: "test-font".to_string(),
            font_size_px: 100.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 0.0,
            kerning_mode: KerningMode::Auto,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 400,
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
            text_layout_mode: TextLayoutMode::CustomVectorLines,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams {
                width_px: 900,
                height_px: 420,
                use_tangent_rotation: true,
                static_rotation_rad: 0.0,
                normal_offset_px: 0.0,
                letter_spacing_mul: 1.0,
                letter_spacing_px: 0.0,
                lines: vec![TextVectorLine {
                    points: vec![
                        TextVectorPoint { x: 0.0, y: 0.0 },
                        TextVectorPoint { x: 800.0, y: 0.0 },
                    ],
                    corner_smoothing_px: 0.0,
                    text_direction: TextVectorLineTextDirection::LeftToRight,
                    distance_mode: TextVectorLineDistanceMode::MinimumPreviousDistance,
                    flip_text: false,
                }],
            },
            effects_json: String::new(),
            anti_aliasing: AntiAliasingMode::Smooth,
            global_rotation_deg: 0.0,
            line_placement_percent: 0.0,
            line_placement_reference: LinePlacementReference::GlyphHeight,
            raster_transform: None,
            extra_info: crate::types::RenderExtraInfoRequest::default(),
        }
    }

    /// Em size of the `MinimumPreviousDistance` fixtures below.
    const TARGET_FIXTURE_EM: f32 = 100.0;

    /// Shaped seeds with their advances assigned, plus the kerning map holding
    /// `pairs`, exactly as a render would hand them to the placement walk.
    fn min_distance_fixture(
        text: &str,
        pairs: &[(char, char, f32)],
    ) -> (
        cosmic_text::FontSystem,
        crate::font_registry::CustomKerningMap,
        Vec<FormulaGlyphSeed>,
    ) {
        min_distance_fixture_at(text, pairs, TARGET_FIXTURE_EM, biased(0.0))
    }

    /// [`min_distance_fixture`] at an explicit em size and line alignment, for
    /// the cases where those two decide the outcome (the alignment start offset
    /// of a run that nearly fills its path).
    fn min_distance_fixture_at(
        text: &str,
        pairs: &[(char, char, f32)],
        em: f32,
        line_align: HorizontalAlign,
    ) -> (
        cosmic_text::FontSystem,
        crate::font_registry::CustomKerningMap,
        Vec<FormulaGlyphSeed>,
    ) {
        use crate::font_provider::CustomKerningTable;
        use crate::font_registry::CustomKerningMap;
        use std::sync::Arc;

        let (mut font_system, face_id, mut seeds) =
            formula_seeds_for(text, em, KerningMode::Auto);
        for seed in &mut seeds {
            seed.line_align = line_align;
        }
        let mut custom_kerning = CustomKerningMap::default();
        if !pairs.is_empty() {
            custom_kerning.insert(
                face_id,
                Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                "fixture",
            );
        }
        super::assign_formula_seed_advances(
            seeds.as_mut_slice(),
            &mut font_system,
            &custom_kerning,
            em,
            (em * 0.5).max(1.0),
        );
        (font_system, custom_kerning, seeds)
    }

    /// The line target gap [`super::straight_reference_target_gaps`] computes for
    /// `text`, with an optional user-authored kerning table on the fixture face.
    ///
    /// Runs the real production chain: shape -> `assign_formula_seed_advances`
    /// (so the authored override lands in the advances exactly as in a render) ->
    /// the pre-pass.
    fn line_target_gap_for(text: &str, pairs: &[(char, char, f32)]) -> Option<f32> {
        let (mut font_system, custom_kerning, seeds) = min_distance_fixture(text, pairs);
        let params = min_distance_line_params();
        let layout = super::custom_line_layout_settings(&params, 0.0, 0.0);
        let spacing = OnPathStepSpacing::for_custom_lines(1.0, 0.0);
        let mut cache = cosmic_text::SwashCache::new();
        let mut contour_cache = std::collections::HashMap::new();
        let mut outline_cache = crate::vector::OutlineCache::new();
        super::straight_reference_target_gaps(
            &params,
            seeds.as_slice(),
            &layout,
            spacing,
            &custom_kerning,
            &mut font_system,
            &mut cache,
            &mut contour_cache,
            &mut outline_cache,
        )
        .get(&0)
        .copied()
    }

    /// A circular arc of `radius` px sweeping `sweep_rad`, sampled into
    /// `segments` chords. Unlike [`semicircle_path`] the sweep is free, so a
    /// path can be built that bends the SAME WAY throughout without folding back
    /// on itself — the geometry where every ink correction points forward and the
    /// run therefore ends up longer than the arc-length estimate.
    fn arc_path(radius: f32, sweep_rad: f32, segments: usize) -> DrawnLinePath {
        let mut points = Vec::with_capacity(segments + 1);
        let mut arc = 0.0f32;
        let mut prev: Option<(f32, f32)> = None;
        for i in 0..=segments {
            let angle = sweep_rad * (i as f32) / (segments as f32);
            let x = radius * angle.sin();
            let y = radius * (1.0 - angle.cos());
            if let Some((px, py)) = prev {
                arc += ((x - px).powi(2) + (y - py).powi(2)).sqrt();
            }
            points.push(DrawnLinePoint {
                x,
                y,
                arc_len_px: arc,
            });
            prev = Some((x, y));
        }
        DrawnLinePath {
            points,
            total_len_px: arc,
            direction: TextVectorLineTextDirection::LeftToRight,
            honor_text_direction: false,
        }
    }

    /// Placement transforms of `text` walked along `path` at em size `em`, with
    /// the line aligned by `line_align` and spaced by `distance_mode`.
    ///
    /// Runs the whole production placement (`build_drawn_line_transforms`), so a
    /// `None` entry is a genuinely DROPPED glyph, exactly the one the render
    /// reports as "not drawn". Index-aligned with the shaped seeds.
    fn placed_transforms_on(
        text: &str,
        em: f32,
        line_align: HorizontalAlign,
        distance_mode: crate::types::TextVectorLineDistanceMode,
        path: &DrawnLinePath,
    ) -> Vec<Option<DrawnLineTransform>> {
        let (mut font_system, custom_kerning, seeds) =
            min_distance_fixture_at(text, &[], em, line_align);
        let mut params = min_distance_line_params();
        params.font_size_px = em;
        params.align = line_align;
        if let Some(line) = params.vector_lines_layout.lines.first_mut() {
            line.distance_mode = distance_mode;
        }
        let mut cache = cosmic_text::SwashCache::new();
        let mut contour_cache = std::collections::HashMap::new();
        let mut outline_cache = crate::vector::OutlineCache::new();
        let paths = vec![Some(path.clone())];
        super::build_drawn_line_transforms(
            &params,
            seeds.as_slice(),
            paths.as_slice(),
            &custom_kerning,
            &mut font_system,
            &mut cache,
            &mut contour_cache,
            &mut outline_cache,
            0.0,
            0.0,
        )
    }

    /// Distances between the placed centers of consecutive glyphs of `text`
    /// after the FULL `MinimumPreviousDistance` walk along `path`.
    ///
    /// The whole production placement runs here (both pre-passes, the directional
    /// search and the clearance net), so this is what the blit would draw. Panics
    /// if any glyph is dropped, which a fixture must never do.
    fn placed_pair_distances(
        text: &str,
        pairs: &[(char, char, f32)],
        path: &DrawnLinePath,
    ) -> Vec<f32> {
        let (mut font_system, custom_kerning, seeds) = min_distance_fixture(text, pairs);
        let params = min_distance_line_params();
        let mut cache = cosmic_text::SwashCache::new();
        let mut contour_cache = std::collections::HashMap::new();
        let mut outline_cache = crate::vector::OutlineCache::new();
        let paths = vec![Some(path.clone())];
        let transforms = super::build_drawn_line_transforms(
            &params,
            seeds.as_slice(),
            paths.as_slice(),
            &custom_kerning,
            &mut font_system,
            &mut cache,
            &mut contour_cache,
            &mut outline_cache,
            0.0,
            0.0,
        );
        let centers = transforms
            .iter()
            .map(|item| {
                let placed = item.as_ref().expect("every fixture glyph must be placed");
                [placed.center_x, placed.center_y]
            })
            .collect::<Vec<_>>();
        centers
            .windows(2)
            .map(|pair| {
                let dx = pair[1][0] - pair[0][0];
                let dy = pair[1][1] - pair[0][1];
                (dx * dx + dy * dy).sqrt()
            })
            .collect()
    }

    /// Arc-length position (px) of the path sample nearest to `point`: the
    /// inverse of `sample_drawn_line_path`, accurate to the path's own sampling
    /// step. A glyph center placed by this module sits ON the path (line
    /// placement fraction `0.0`), so this recovers where along the line it was
    /// put.
    fn nearest_arc_len(path: &DrawnLinePath, point: [f32; 2]) -> f32 {
        path.points
            .iter()
            .map(|p| {
                let d = (p.x - point[0]).powi(2) + (p.y - point[1]).powi(2);
                (d, p.arc_len_px)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(0.0, |(_, arc)| arc)
    }

    /// Arc-length position of the LAST placed glyph of `transforms`, panicking
    /// if nothing was placed at all.
    fn last_placed_arc_len(path: &DrawnLinePath, transforms: &[Option<DrawnLineTransform>]) -> f32 {
        let last = transforms
            .iter()
            .filter_map(Option::as_ref)
            .next_back()
            .expect("at least one glyph must be placed");
        nearest_arc_len(path, [last.center_x, last.center_y])
    }

    /// Final ink gaps between ADJACENT INKED glyphs of `text` after the full
    /// production walk along `path`, measured with the very metric the mode
    /// normalizes on (`on_path_pair_gap` on the pair's chord) over the very
    /// contours the blit rasterizes.
    ///
    /// Pairs whose chain is broken by an inkless glyph (a space) are skipped:
    /// the mode deliberately never normalizes an inter-word distance, so
    /// including one would measure a contract it does not claim.
    fn final_ink_gaps(
        text: &str,
        em: f32,
        distance_mode: crate::types::TextVectorLineDistanceMode,
        path: &DrawnLinePath,
    ) -> Vec<f32> {
        let (mut font_system, custom_kerning, seeds) =
            min_distance_fixture_at(text, &[], em, biased(0.0));
        let mut params = min_distance_line_params();
        params.font_size_px = em;
        if let Some(line) = params.vector_lines_layout.lines.first_mut() {
            line.distance_mode = distance_mode;
        }
        let layout = super::custom_line_layout_settings(&params, 0.0, 0.0);
        let mut cache = cosmic_text::SwashCache::new();
        let mut contour_cache = std::collections::HashMap::new();
        let mut outline_cache = crate::vector::OutlineCache::new();
        let paths = vec![Some(path.clone())];
        let transforms = super::build_drawn_line_transforms(
            &params,
            seeds.as_slice(),
            paths.as_slice(),
            &custom_kerning,
            &mut font_system,
            &mut cache,
            &mut contour_cache,
            &mut outline_cache,
            0.0,
            0.0,
        );

        let mut gaps = Vec::new();
        let mut previous: Option<PlacedGlyphInk> = None;
        for (seed, transform) in seeds.iter().zip(transforms.iter()) {
            let placed = transform
                .as_ref()
                .expect("every glyph of this fixture must be placed");
            let ink = super::seed_ink_geometry(
                seed,
                &mut font_system,
                &mut cache,
                &mut contour_cache,
                &mut outline_cache,
            )
            .map(|geom| super::place_seed_ink_for_transform(seed, &geom, &layout, placed));
            if let (Some(prev), Some(cur)) = (previous.as_ref(), ink.as_ref())
                && let Some(gap) = on_path_pair_gap(prev, cur)
                && gap.is_finite()
            {
                gaps.push(gap);
            }
            previous = ink;
        }
        gaps
    }

    #[test]
    fn a_real_run_on_a_real_curve_ends_up_with_one_uniform_ink_gap() {
        use crate::types::TextVectorLineDistanceMode::{ByLineLength, MinimumPreviousDistance};

        // The end-to-end contract of the mode, with NOTHING hand-set: a real
        // font, a real curve, the full placement chain (median pre-pass ->
        // directional search -> clearance net), and the gaps read back off the
        // contours that get rasterized. The unit tests above pin the SEARCH
        // against a target handed to them; this one pins that the MEDIAN the
        // pre-pass computes is what actually makes a run uniform.
        let path = arc_path(420.0, 1.6, 256);
        let em = 48.0;
        let text = "Изогнутая строка ГЛАВА Ту";

        let ink = final_ink_gaps(text, em, MinimumPreviousDistance, &path);
        assert!(ink.len() >= 15, "too few measured pairs: {}", ink.len());
        let mean = ink.iter().sum::<f32>() / ink.len() as f32;
        let spread = ink
            .iter()
            .fold(0.0f32, |worst, gap| worst.max((gap - mean).abs()));
        // Twice the search's own dead zone (`ON_PATH_INK_GAP_EPSILON_PX`, 0.05),
        // which is the tightest the walk can be asked to agree: a pair already
        // within the epsilon of the target keeps its seed. Measured here: 0.001
        // px, so the bound has two orders of magnitude of headroom and still
        // catches any real regression.
        assert!(
            spread <= 0.1,
            "gaps are not uniform: mean {mean:.2}, worst deviation {spread:.2}, gaps {ink:?}"
        );

        // Non-vacuity: the same text on the same curve under the plain
        // arc-length walk is NOT uniform, so the assertion above measures the
        // mode and not the font.
        let flat = final_ink_gaps(text, em, ByLineLength, &path);
        let flat_mean = flat.iter().sum::<f32>() / flat.len() as f32;
        let flat_spread = flat
            .iter()
            .fold(0.0f32, |worst, gap| worst.max((gap - flat_mean).abs()));
        assert!(
            flat_spread > 3.0 * spread.max(0.1),
            "fixture broken: ByLineLength is already uniform (spread {flat_spread:.2} vs {spread:.2})"
        );
    }

    #[test]
    fn an_end_aligned_run_on_a_one_sided_bend_keeps_every_glyph() {
        use crate::types::TextVectorLineDistanceMode::{ByLineLength, MinimumPreviousDistance};

        // R: `drawn_line_start_offsets` predicts a line's run length by replaying
        // the plain arc-length walk, which cannot see the ink corrections. On a
        // path that bends the same way throughout they do not cancel and the run
        // really ends up LONGER than predicted, so an end-biased alignment
        // started it too late and `drawn_line_drop_side` deleted the tail — one
        // glyph silently missing, with only a warning, while `ByLineLength` drew
        // the same text whole on the same geometry. Since
        // `refine_min_distance_start_offsets` the offset comes from the MEASURED
        // run instead. Measured on this fixture: the estimate is ~26 px short of
        // the real run, i.e. about one and a half glyphs.
        let path = arc_path(420.0, 1100.0 / 420.0, 512);
        let em = 36.0;
        // Wide, uniform glyphs: the line's straight-reference gaps are skewed
        // enough that normalizing every pair onto their MEDIAN lengthens the run.
        let text = "mmmmmm mmmmmm mmmmmm mmmmmm mmmmmm mmm";

        // Non-vacuity 1: the plain arc-length walk places every glyph on this
        // geometry, so any drop below belongs to the ink mode's own arithmetic.
        let by_length = placed_transforms_on(text, em, HorizontalAlign::RIGHT, ByLineLength, &path);
        assert!(
            by_length.iter().all(Option::is_some),
            "fixture broken: ByLineLength already drops {} of {} glyphs",
            by_length.iter().filter(|t| t.is_none()).count(),
            by_length.len()
        );
        // Non-vacuity 2: the estimate must really be wrong here, or the test
        // would pass with no refinement at all. Both runs start at the same
        // place (start bias), so the difference of their ends IS the error the
        // pre-pass cannot predict.
        let flat_end = last_placed_arc_len(
            &path,
            &placed_transforms_on(text, em, HorizontalAlign::LEFT, ByLineLength, &path),
        );
        let ink_end = last_placed_arc_len(
            &path,
            &placed_transforms_on(text, em, HorizontalAlign::LEFT, MinimumPreviousDistance, &path),
        );
        assert!(
            ink_end - flat_end > 8.0,
            "fixture broken: the ink run is only {:.1} px longer than the estimate",
            ink_end - flat_end
        );

        for align in [HorizontalAlign::RIGHT, HorizontalAlign::CENTER] {
            let placed = placed_transforms_on(text, em, align, MinimumPreviousDistance, &path);
            let dropped = placed.iter().filter(|t| t.is_none()).count();
            assert_eq!(
                dropped,
                0,
                "bias {}: {dropped} of {} glyphs dropped off the arc",
                align.bias,
                placed.len()
            );
        }

        // Justify keeps the custom-line default fraction (start of line), so it
        // is skipped by the refinement entirely and must still place everything.
        let justified =
            placed_transforms_on(text, em, HorizontalAlign::JUSTIFY, MinimumPreviousDistance, &path);
        assert!(
            justified.iter().all(Option::is_some),
            "justify dropped {} glyphs",
            justified.iter().filter(|t| t.is_none()).count()
        );
    }

    #[test]
    fn the_measured_start_offset_still_pushes_an_end_aligned_run_to_the_path_end() {
        use crate::types::TextVectorLineDistanceMode::MinimumPreviousDistance;

        // The counterpart of the test above: measuring the run must not collapse
        // into "always start at zero". With real slack on the line, the
        // end-biased run has to finish AT the path end and the start-biased one
        // well before it.
        let path = arc_path(420.0, 1.6, 64);
        let em = 36.0;
        let text = "Короткая строка";

        let left = placed_transforms_on(text, em, HorizontalAlign::LEFT, MinimumPreviousDistance, &path);
        let right =
            placed_transforms_on(text, em, HorizontalAlign::RIGHT, MinimumPreviousDistance, &path);
        assert!(left.iter().all(Option::is_some) && right.iter().all(Option::is_some));

        let left_end = last_placed_arc_len(&path, &left);
        let right_end = last_placed_arc_len(&path, &right);
        // The end-aligned run finishes within one em of the path end...
        assert!(
            right_end >= path.total_len_px - em,
            "end-aligned run stops at {right_end} of {}",
            path.total_len_px
        );
        // ...and there was real free space for it to consume, so the two runs
        // cannot have landed in the same place.
        assert!(
            right_end - left_end > 4.0 * em,
            "alignment barely moved the run: left ends at {left_end}, right at {right_end}"
        );
    }

    #[test]
    fn the_line_target_gap_never_counts_an_inter_word_distance() {
        // R: a space carries no ink, so it is not a kernable pair and its width
        // must not be normalized — otherwise the whole line would collapse to one
        // uniform letter distance and the words would merge. The pre-pass
        // enforces that by ENDING the pair chain at an inkless glyph, so a word
        // gap never reaches the median.
        //
        // Proof without hard-coding a font metric: the same letters with and
        // without a word break must yield the SAME target. If the space
        // contributed, "II II" would median over two letter gaps and two
        // enormous word gaps and land far above "IIII".
        let solid = line_target_gap_for("IIII", &[]).expect("a run of I has kernable pairs");
        let spaced = line_target_gap_for("II II", &[]).expect("both words have kernable pairs");
        // Tolerance, not equality: the two runs place their glyphs at different
        // fractional x, and the measured contour carries the glyph's SUBPIXEL
        // offset (`SeedInkGeometry::subpixel`), so the same pair measures a
        // fraction of a pixel differently. The failure this guards against is an
        // order of magnitude larger — see below.
        assert!(
            (solid - spaced).abs() < 0.5,
            "a word break changed the target: solid={solid} spaced={spaced}"
        );
        // Scale of the failure being excluded: an `I`/`I` gap is ~19 px at em
        // 100 and a space advance ~28 px, so a median that counted the two word
        // gaps of "II II" would sit above 30, not below 25.
        assert!(
            spaced < 25.0,
            "target {spaced} counts an inter-word distance at em 100"
        );
    }

    #[test]
    fn an_authored_pair_is_excluded_from_the_line_target() {
        // A user-authored pair overrules the mode for its own pair (the walk
        // skips it) AND must not drag the target the REST of the line is
        // normalized to — otherwise authoring one pair would silently re-space
        // every other pair of the line.
        //
        // `AVI`: without a table both pairs feed the median; with an authored
        // `AV` only `VI` does. Moving the authored delta from -200 to +200 per
        // mille moves the `A`/`V` inks by 40 px in the reference, so a target
        // that still counted that pair could not possibly stay put.
        let plain = line_target_gap_for("AVI", &[]).expect("plain run has pairs");
        let tight = line_target_gap_for("AVI", &[('A', 'V', -200.0)]).expect("authored run");
        let wide = line_target_gap_for("AVI", &[('A', 'V', 200.0)]).expect("authored run");
        assert!(
            (tight - wide).abs() < 1e-3,
            "the authored pair still influences the target: tight={tight} wide={wide}"
        );
        assert!(
            (plain - tight).abs() > 1e-3,
            "the fixture is vacuous: the authored pair must matter when NOT authored \
             (plain={plain} authored={tight})"
        );
    }

    #[test]
    fn an_authored_pair_survives_the_normalization_on_a_curve() {
        // The other half of the authored-pair contract, at the level of the real
        // placement walk: the pair the user kerned by hand must KEEP the distance
        // the user gave it, even though every other pair of the line is being
        // pulled onto the line target. Its glyphs stay at their arc-length step,
        // which is where `assign_formula_seed_advances` put the override.
        //
        // Moving the authored delta between -200 and +200 per mille is a 40 px
        // move of `A`'s advance at em 100. The arc step between two glyph CENTERS
        // is half of each one's advance, so the `A`/`V` centers must end up 20 px
        // further apart (minus the arc-to-chord shortening). A normalized pair
        // would land on the same ink gap in both runs and the two distances would
        // come out nearly equal instead.
        let path = semicircle_path(220.0, 256);
        let tight = placed_pair_distances("AVI", &[('A', 'V', -200.0)], &path);
        let wide = placed_pair_distances("AVI", &[('A', 'V', 200.0)], &path);
        assert!(
            (wide[0] - tight[0] - 20.0).abs() < 1.0,
            "the authored step was normalized away: tight={tight:?} wide={wide:?}"
        );
        // Non-vacuity: the OTHER pair of the same line IS normalized, onto a
        // target that does not depend on the authored delta. The curvature is
        // constant along a circle, so the same ink gap gives the same chord
        // wherever on the arc the pair ends up.
        assert!(
            (wide[1] - tight[1]).abs() < 1.0,
            "the unauthored pair should be normalized to one target: tight={tight:?} wide={wide:?}"
        );
    }

    /// A custom pair must move a FORMULA seed's advance exactly as it moves the
    /// horizontal pen: the step becomes the left glyph's own (un-kerned) advance plus
    /// the authored delta, so -100 / 0 / +100 per mille sit 10 px apart at em 100.
    /// Formula/drawn lines are a separate advance accumulator, so the horizontal
    /// tests say nothing about them.
    #[test]
    fn a_custom_pair_moves_a_formula_seed_advance() {
        use crate::font_provider::CustomKerningTable;
        use crate::font_registry::CustomKerningMap;
        use std::sync::Arc;

        const EM: f32 = 100.0;
        let advance_for = |pairs: &[(char, char, f32)]| -> f32 {
            let (mut font_system, face_id, mut seeds) =
                formula_seeds_for("AV", EM, KerningMode::Auto);
            assert_eq!(seeds.len(), 2, "the fixture must shape 'AV' as two glyphs");
            let mut custom_kerning = CustomKerningMap::default();
            if !pairs.is_empty() {
                custom_kerning.insert(
                    face_id,
                    Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                    "fixture",
                );
            }
            super::assign_formula_seed_advances(
                seeds.as_mut_slice(),
                &mut font_system,
                &custom_kerning,
                EM,
                (EM * 0.5).max(1.0),
            );
            seeds[0].advance_px
        };

        let shaped = advance_for(&[]);
        let tightened = advance_for(&[('A', 'V', -100.0)]);
        let neutral = advance_for(&[('A', 'V', 0.0)]);
        let widened = advance_for(&[('A', 'V', 100.0)]);

        assert!(
            (neutral - tightened - 10.0).abs() < 1e-2
                && (widened - neutral - 10.0).abs() < 1e-2,
            "-100 / 0 / +100 per mille must sit exactly 10 px apart at em 100; got \
             {tightened}, {neutral}, {widened}"
        );
        assert!(
            (neutral - shaped).abs() > 1e-2,
            "the fixture must kern 'AV' itself, otherwise a 0.0 entry proves nothing \
             about REPLACING that value: un-kerned {neutral} vs shaped {shaped}"
        );
    }

    /// The same guard as the horizontal pen: an unlisted pair must leave the seed
    /// advances exactly where the shaped positions put them, even though the table's
    /// mere presence takes the run off the fast path.
    #[test]
    fn an_unlisted_pair_leaves_formula_seed_advances_untouched() {
        use crate::font_provider::CustomKerningTable;
        use crate::font_registry::CustomKerningMap;
        use std::sync::Arc;

        const EM: f32 = 100.0;
        let advance_for = |pairs: &[(char, char, f32)]| -> f32 {
            let (mut font_system, face_id, mut seeds) =
                formula_seeds_for("AV", EM, KerningMode::Auto);
            let mut custom_kerning = CustomKerningMap::default();
            if !pairs.is_empty() {
                custom_kerning.insert(
                    face_id,
                    Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                    "fixture",
                );
            }
            super::assign_formula_seed_advances(
                seeds.as_mut_slice(),
                &mut font_system,
                &custom_kerning,
                EM,
                (EM * 0.5).max(1.0),
            );
            seeds[0].advance_px
        };

        let shaped = advance_for(&[]);
        let other_pair = advance_for(&[('Q', 'z', -200.0)]);
        assert!(
            (shaped - other_pair).abs() < 1e-3,
            "an unlisted pair must not move a seed advance: {other_pair} vs {shaped}"
        );
    }

    /// Pins the quarter-advance floor of [`super::seed_metric_advance_px`], which
    /// had no test at all. It is the guard that keeps a non-monotonic (RTL) run or
    /// a font pair kerned below -75% from stalling the arc-length cursor; the
    /// `1.0` term takes over only for a glyph too narrow for the quarter to reach
    /// it. Lowering either value is a rendering change, not a cleanup.
    #[test]
    fn the_metric_advance_floor_turns_a_backwards_pen_delta_into_a_quarter_advance() {
        // Ordinary forward delta: the floor must not touch it.
        assert!(
            (super::seed_metric_advance_px(57.0, 60.0) - 57.0).abs() < 1e-3,
            "a forward pen delta must pass through untouched"
        );
        // Font pair kerning below -75% of the left advance, and the RTL case where
        // `glyph.x` decreases outright: both land on the quarter-advance floor.
        assert!(
            (super::seed_metric_advance_px(5.0, 80.0) - 20.0).abs() < 1e-3,
            "a delta under a quarter of the glyph advance must clamp to that quarter"
        );
        assert!(
            (super::seed_metric_advance_px(-140.0, 80.0) - 20.0).abs() < 1e-3,
            "a NEGATIVE (right-to-left) pen delta must become a positive magnitude"
        );
        // Sub-pixel glyph: the quarter is below 1.0, so the absolute guard wins.
        assert!(
            (super::seed_metric_advance_px(-3.0, 2.0) - 1.0).abs() < 1e-3,
            "a glyph too narrow for the quarter floor must still step a whole pixel"
        );
    }

    /// The seed advance written by `assign_formula_seed_advances` is NOT clamped
    /// positive: an authored pair negative enough to invert the step leaves a
    /// negative `advance_px`, because the quarter-advance floor guards only the
    /// METRIC advance and the custom-pair branch steps by the glyph's own nominal
    /// advance instead. Turning that into a walkable step is
    /// [`super::OnPathStepSpacing::step_px`]'s job — and the two on-path layouts
    /// currently floor it at DIFFERENT values, which this test pins so the
    /// divergence cannot be "fixed" silently.
    #[test]
    fn an_extreme_negative_authored_pair_is_floored_by_the_step_owner_not_by_the_seed() {
        use crate::font_provider::CustomKerningTable;
        use crate::font_registry::CustomKerningMap;
        use std::sync::Arc;

        const EM: f32 = 100.0;
        let (mut font_system, face_id, mut seeds) =
            formula_seeds_for("AV", EM, KerningMode::Auto);
        assert_eq!(seeds.len(), 2, "the fixture must shape 'AV' as two glyphs");
        let left_w = seeds[0].glyph.w;
        let mut custom_kerning = CustomKerningMap::default();
        custom_kerning.insert(
            face_id,
            // -2000 per mille at em 100 = -200 px, far past the left glyph's own
            // advance, so the resulting step is genuinely negative.
            Arc::new(CustomKerningTable::from_pairs([('A', 'V', -2000.0)])),
            "fixture",
        );
        super::assign_formula_seed_advances(
            seeds.as_mut_slice(),
            &mut font_system,
            &custom_kerning,
            EM,
            (EM * 0.5).max(1.0),
        );
        let seed_advance = seeds[0].advance_px;
        assert!(
            seed_advance < 0.0,
            "the authored delta must survive into the seed unclamped (the quarter-advance \
             floor of {} px guards only the metric advance); got {seed_advance}",
            (left_w * 0.25).max(1.0)
        );

        // Custom raster/vector lines floor the step at one pixel...
        let custom_lines = super::OnPathStepSpacing::for_custom_lines(1.0, 0.0);
        assert!(
            (custom_lines.step_px(seed_advance) - super::MIN_ON_PATH_STEP_PX).abs() < 1e-3,
            "custom lines must floor the inverted step at MIN_ON_PATH_STEP_PX"
        );
        // ...while the formula path floors it at HALF THE FONT SIZE, so the very
        // same authored pair renders 50 px apart there instead of 1 px. That gap is
        // a known defect kept for byte-compatibility, documented on
        // `OnPathStepSpacing::for_formula`; it is not a contract worth preserving.
        let formula = super::OnPathStepSpacing::for_formula(1.0, 0.0, (EM * 0.5).max(1.0));
        assert!(
            (formula.step_px(seed_advance) - 50.0).abs() < 1e-3,
            "the formula path must still floor the step at font_size/2 = 50 px"
        );
    }

    /// The step owner must apply its two floors in the documented ORDER: the seed
    /// floor first (before the multiplier scales it), the absolute step floor last
    /// (after negative additive tracking). Collapsing them into one would change
    /// rendering for every non-unit letter-spacing multiplier.
    #[test]
    fn the_step_owner_applies_the_seed_floor_before_the_multiplier() {
        // Seed advance below the formula floor: 50 is used, then doubled -> 100.
        let formula = super::OnPathStepSpacing::for_formula(2.0, 0.0, 50.0);
        assert!(
            (formula.step_px(10.0) - 100.0).abs() < 1e-3,
            "the seed floor must be scaled by the multiplier, not applied after it"
        );
        // Negative tracking that overshoots the whole step is caught by the final
        // floor, never by the seed floor.
        let custom_lines = super::OnPathStepSpacing::for_custom_lines(1.0, -500.0);
        assert!(
            (custom_lines.step_px(40.0) - super::MIN_ON_PATH_STEP_PX).abs() < 1e-3,
            "negative tracking must be caught by the final step floor"
        );
        // The letter-spacing inputs are clamped by the constructor, so an absurd
        // multiplier cannot reach the arithmetic.
        let clamped = super::OnPathStepSpacing::for_custom_lines(1_000.0, 0.0);
        assert!(
            (clamped.step_px(10.0) - 80.0).abs() < 1e-3,
            "the multiplier must be clamped to 8.0 by the constructor"
        );
    }
}
