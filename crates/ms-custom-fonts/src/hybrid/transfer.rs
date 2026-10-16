/*
File: crates/ms-custom-fonts/src/hybrid/transfer.rs

Purpose:
The `Transfer` algorithm: measure font-wide design parameters of every source once (stem
thickness, width, x-height, cap height, slant), average them into target parameters, and
transform each glyph of the base font so its parameters move from the base's to the target's.
The base font's glyph shapes are kept; only these parameters change.

Key structures:
- `FontParams`: one face's measured parameters (`None` = not measurable, left unchanged) and
  the reference-chain level each stem / width value was measured on.
- `Targets`: the recipe's base index, every source's parameters and the target parameters.
- `Transform` (private): one glyph transform resolved from base and target parameters.

Key functions:
- `measure`: parameters of one parsed source face (pure, independent of the compile scope).
- `targets`: the target parameters from the measurements and the shape weights.
- `apply`: one base glyph transformed from base to target parameters.

Notes:
- Measurement: fixed reference chains, the first chain glyph the face maps with a valid
  measurement wins per parameter (a degenerate one — run < 1 unit, > 400 units, not finite —
  passes to the next). Stems are first-ink-run widths of nonzero-winding scanlines on the
  flattened outline: vertical stem `l I і І` (Latin level), `ㅣ 十 丨` (CJK level), median
  of rows at 30/50/70 % of the ink height (`十`: one row at 25 %); horizontal stem: the run
  nearest the ink middle of a vertical scanline through `H Н` (Latin), `ㅡ 十 一` (CJK) at 50 %
  of the ink width (`十`: 25 %); width: mean ink width of `n o` / `п о` (Latin), `口 ㅁ` (CJK),
  every member of a group required; x-height: ink top of `x` / `х`, else OS/2 `sxHeight`; cap
  height: top of `H` / `Н`, else `sCapHeight`; slant: x-centre shift of the vertical-stem
  glyph's first runs at 20 % vs 80 % of its height, else `tan(-italicAngle)` when the angle is
  non-zero.
- Levels: a stem or width value is compared only with values of the same chain level (a Latin
  `n` is never averaged with a CJK `口`); heights and slant are level-free.
- Target = per parameter, the shape-weighted mean over the sources measured at the base's
  level (weights renormalized over them); a parameter the base lacks is neutral (skipped).
- Transform order, on control points: (1) zone scale — piecewise-linear y map with anchors
  0 -> 0, x_b -> x_t - 2 d_h, cap_b -> cap_t - 2 d_h, slope 1 below 0 and above the highest
  anchor, each zone factor clamped to [0.7, 1.4], then a shift by d_h (the offset below
  pre-compensated, so baseline, x-height and cap height land on target); (2) shear
  `x += (tan_t - tan_b) * y` about the baseline, |Δ| <= tan 25°; (3) horizontal scale about
  x = 0 by `s_w = (W_t - 2 d_v) / W_b`, clamped to [0.6, 1.6] — the advance stays the spacing
  blend; (4) thickness / contrast by `sdf::offset_outline(d_v, d_h)`. The offsets are solved
  first: `d = (T_t - s T_b) / 2` with `s` the counter ratio `(E_t - T_t) / (E_b - T_b)` of the
  extent the stems sit in (ink width for vertical stems, the lowest zone for horizontal ones);
  one unknown stem direction takes the other's offset; each clamped to
  ±min(60, 0.45 · the smaller base stem). A clamped offset costs stem accuracy, never width or
  height accuracy.
- Exactness: each step is skipped when its effect stays below half a unit (zone, width,
  thickness) or the slant difference below `SLANT_MIN_TAN`; with every step skipped the base
  outline is returned unchanged (bit for bit). Thickness changes re-trace the glyph through a
  distance field, so corners round slightly (as in the SDF blend).
- Measurement is the one font-wide input of the hybrid: it reads a fixed reference glyph set
  straight from each face, once per source before the parallel glyph loop, so it is
  deterministic and independent of the compile scope (a preview subset equals the full face).
- Bounded work: the transforms are bounded scales / shears; the offset grids through the SDF
  path's limits and rejects oversized or non-finite outlines (`ExtentTooLarge`) and outlines
  beyond the flattening budget (`TooManyPoints`), both as `sdf::BlendRejection`.
*/

use kurbo::Point;

use crate::hybrid::sdf::{self, BlendRejection, Bounds, ExtentTooLarge};
use crate::outline::{Contour, Outline, Seg};
use crate::source::{SourceFace, SourceMetrics};

/// Shortest valid stem measurement, font units.
const MIN_STEM: f64 = 1.0;
/// Longest valid stem measurement, font units.
const MAX_STEM: f64 = 400.0;
/// Shortest valid ink extent of a reference glyph (and of a height / width value).
const MIN_EXTENT: f64 = 1.0;
/// Longest valid height / width value, font units (2 em).
const MAX_EXTENT: f64 = 2000.0;
/// Largest valid slant tangent (45°).
const MAX_SLANT_TAN: f64 = 1.0;
/// A slant-probe run wider than this many stems is not on the stem (a serif, a bowl).
const SLANT_RUN_MAX_STEMS: f64 = 2.5;
/// Zone-scale factor range.
const ZONE_FACTOR_RANGE: (f64, f64) = (0.7, 1.4);
/// Width-scale factor range.
const WIDTH_FACTOR_RANGE: (f64, f64) = (0.6, 1.6);
/// Largest transferred slant difference, `tan 25°`.
const MAX_SHEAR_TAN: f64 = 0.466_307_658_154_998_6;
/// Slant differences below this tangent (about 0.1°) are measurement noise and not applied.
const SLANT_MIN_TAN: f64 = 0.002;
/// Smallest counter (ink width minus a stem) the counter-based width ratio uses.
const MIN_COUNTER: f64 = 20.0;
/// Absolute bound of one thickness offset, font units.
const MAX_OFFSET: f64 = 60.0;
/// Bound of one thickness offset relative to the base's thinner stem (never thins to zero).
const MAX_OFFSET_STEM_RATIO: f64 = 0.45;
/// Effects below this many font units are skipped (the base stays exact).
const MIN_EFFECT: f64 = 0.5;

/// Reference chain a stem or width value was measured on. Values are compared only between
/// faces measured on the same level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Level {
    /// Latin / Cyrillic reference glyphs.
    #[default]
    Latin,
    /// Hangul / Han reference glyphs.
    Cjk,
}

/// Chain levels of the level-dependent parameters (meaningful only where the value is `Some`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Levels {
    stem_v: Level,
    stem_h: Level,
    width: Level,
}

/// Measured design parameters of one face, font units (1000 per em); `None` when the face has
/// no reference glyph for the parameter or the measurement is degenerate.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct FontParams {
    /// Vertical stem thickness.
    pub stem_v: Option<f64>,
    /// Horizontal stem (crossbar) thickness.
    pub stem_h: Option<f64>,
    /// Mean ink width of the reference lowercase / CJK box glyphs.
    pub width: Option<f64>,
    /// Lowercase x-height.
    pub x_height: Option<f64>,
    /// Capital height.
    pub cap_height: Option<f64>,
    /// Tangent of the slant angle (positive = leaning right).
    pub slant_tan: Option<f64>,
    /// Chain level of `stem_v`, `stem_h` and `width`.
    levels: Levels,
}

/// Font-wide inputs of the per-glyph transform.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Targets {
    /// Recipe index of the base font.
    pub base: usize,
    /// Parameters of every source, in recipe order (a glyph whose base font has no ink is
    /// transferred from another source with THAT source's parameters).
    pub base_params: Vec<FontParams>,
    /// The averaged target parameters.
    pub target: FontParams,
}

/// One vertical-stem reference glyph and the ink-height fractions of its probe rows.
struct StemProbe {
    ch: char,
    level: Level,
    /// Rows (fractions of the ink height) whose first-run widths are the stem (median).
    rows: &'static [f64],
}

/// One horizontal-stem reference glyph and the ink-width fraction of its probe column.
struct BarProbe {
    ch: char,
    level: Level,
    /// Column (fraction of the ink width) crossing only the horizontal stem.
    column: f64,
}

const STEM_ROWS: &[f64] = &[0.3, 0.5, 0.7];
/// `十`: the low row crosses only the vertical stroke.
const CROSS_STEM_ROWS: &[f64] = &[0.25];
const STEM_V_CHAIN: [StemProbe; 7] = [
    StemProbe { ch: 'l', level: Level::Latin, rows: STEM_ROWS },
    StemProbe { ch: 'I', level: Level::Latin, rows: STEM_ROWS },
    StemProbe { ch: '\u{456}', level: Level::Latin, rows: STEM_ROWS },
    StemProbe { ch: '\u{406}', level: Level::Latin, rows: STEM_ROWS },
    StemProbe { ch: '\u{3163}', level: Level::Cjk, rows: STEM_ROWS },
    StemProbe { ch: '\u{5341}', level: Level::Cjk, rows: CROSS_STEM_ROWS },
    StemProbe { ch: '\u{4E28}', level: Level::Cjk, rows: STEM_ROWS },
];
const STEM_H_CHAIN: [BarProbe; 5] = [
    BarProbe { ch: 'H', level: Level::Latin, column: 0.5 },
    BarProbe { ch: '\u{41D}', level: Level::Latin, column: 0.5 },
    BarProbe { ch: '\u{3161}', level: Level::Cjk, column: 0.5 },
    BarProbe { ch: '\u{5341}', level: Level::Cjk, column: 0.25 },
    BarProbe { ch: '\u{4E00}', level: Level::Cjk, column: 0.5 },
];
const WIDTH_GROUPS: [(&[char], Level); 3] =
    [(&['n', 'o'], Level::Latin), (&['\u{43F}', '\u{43E}'], Level::Latin), (&['\u{53E3}', '\u{3141}'], Level::Cjk)];
const X_HEIGHT_CHAIN: [char; 2] = ['x', '\u{445}'];
const CAP_HEIGHT_CHAIN: [char; 2] = ['H', '\u{41D}'];
/// Ink-height fractions of the slant probe rows.
const SLANT_ROWS: (f64, f64) = (0.2, 0.8);

/// Measures `face`'s parameters from its fixed reference glyphs and metrics (never from the
/// compile scope).
pub(crate) fn measure(face: &SourceFace<'_>) -> FontParams {
    measure_with(&|ch| face.glyph(ch).map(|glyph| face.outline(glyph)), &face.metrics())
}

/// `measure` over a glyph lookup (normalized outline of a character, `None` when unmapped).
fn measure_with(lookup: &dyn Fn(char) -> Option<Outline>, metrics: &SourceMetrics) -> FontParams {
    let reference = |ch: char| Reference::of(&lookup(ch)?);
    let mut params = FontParams::default();
    for probe in &STEM_V_CHAIN {
        let Some(glyph) = reference(probe.ch) else {
            continue;
        };
        if let Some(stem) = glyph.vertical_stem(probe.rows) {
            params.stem_v = Some(stem);
            params.levels.stem_v = probe.level;
            params.slant_tan = glyph.slant(stem);
            break;
        }
    }
    if params.slant_tan.is_none() {
        // PostScript italic angle: degrees counter-clockwise from vertical (negative leans right).
        let tangent = (-metrics.italic_angle).to_radians().tan();
        if metrics.italic_angle.abs() >= 0.01 && tangent.is_finite() && tangent.abs() <= MAX_SLANT_TAN {
            params.slant_tan = Some(tangent);
        }
    }
    for probe in &STEM_H_CHAIN {
        if let Some(stem) = reference(probe.ch).and_then(|glyph| glyph.horizontal_stem(probe.column)) {
            params.stem_h = Some(stem);
            params.levels.stem_h = probe.level;
            break;
        }
    }
    for (group, level) in WIDTH_GROUPS {
        let widths: Option<Vec<f64>> = group.iter().map(|&ch| reference(ch).map(|glyph| glyph.ink.x1 - glyph.ink.x0)).collect();
        if let Some(widths) = widths {
            let mean = widths.iter().sum::<f64>() / f64::from(u32::try_from(widths.len()).unwrap_or(1));
            if valid_extent(mean) {
                params.width = Some(mean);
                params.levels.width = level;
                break;
            }
        }
    }
    let top = |chain: &[char], fallback: Option<f64>| {
        chain.iter().find_map(|&ch| reference(ch).map(|glyph| glyph.ink.y1).filter(|&value| valid_extent(value))).or(fallback.filter(|&value| valid_extent(value)))
    };
    params.x_height = top(&X_HEIGHT_CHAIN, metrics.x_height);
    params.cap_height = top(&CAP_HEIGHT_CHAIN, metrics.cap_height);
    params
}

/// A height / width value in the plausible range.
fn valid_extent(value: f64) -> bool {
    value.is_finite() && (MIN_EXTENT..=MAX_EXTENT).contains(&value)
}

/// A stem measurement in the plausible range.
fn valid_stem(value: f64) -> bool {
    value.is_finite() && (MIN_STEM..=MAX_STEM).contains(&value)
}

/// A flattened reference glyph with a finite, non-degenerate ink box.
struct Reference {
    polygons: Vec<Vec<Point>>,
    ink: Bounds,
}

impl Reference {
    /// `None` for an empty, hostile (non-finite / oversized / beyond the flattening budget) or
    /// degenerate outline.
    fn of(outline: &Outline) -> Option<Self> {
        // A glyph beyond the flattening budget is as unusable as a degenerate one: the chain
        // moves on to its next reference glyph.
        let polygons = sdf::flatten_checked(outline, sdf::MAX_FLATTENED_POINTS).ok()?;
        let ink = sdf::checked_bounds(&polygons).ok().flatten()?;
        (ink.x1 - ink.x0 >= MIN_EXTENT && ink.y1 - ink.y0 >= MIN_EXTENT).then_some(Self { polygons, ink })
    }

    /// y at `fraction` of the ink height.
    fn row(&self, fraction: f64) -> f64 {
        self.ink.y0 + fraction * (self.ink.y1 - self.ink.y0)
    }

    /// Median first-run width over the probe rows (rows without a valid run are left out).
    fn vertical_stem(&self, rows: &[f64]) -> Option<f64> {
        let mut widths: Vec<f64> = rows
            .iter()
            .filter_map(|&fraction| ink_runs(&self.polygons, self.row(fraction), false).first().map(|&(start, end)| end - start))
            .filter(|&width| valid_stem(width))
            .collect();
        widths.sort_by(f64::total_cmp);
        let middle = widths.len() / 2;
        match widths.len() {
            0 => None,
            count if !count.is_multiple_of(2) => Some(widths[middle]),
            _ => Some(0.5 * (widths[middle - 1] + widths[middle])),
        }
    }

    /// Thickness of the vertical-scanline run nearest the ink middle at `column` of the width.
    fn horizontal_stem(&self, column: f64) -> Option<f64> {
        let x = self.ink.x0 + column * (self.ink.x1 - self.ink.x0);
        let middle = 0.5 * (self.ink.y0 + self.ink.y1);
        let runs = ink_runs(&self.polygons, x, true);
        let nearest = runs.iter().min_by(|a, b| (0.5 * (a.0 + a.1) - middle).abs().total_cmp(&(0.5 * (b.0 + b.1) - middle).abs()))?;
        Some(nearest.1 - nearest.0).filter(|&thickness| valid_stem(thickness))
    }

    /// Slant tangent from the first-run centres of the slant rows; `None` when a run is not on
    /// the stem (wider than `SLANT_RUN_MAX_STEMS` stems) or the slope is implausible.
    fn slant(&self, stem: f64) -> Option<f64> {
        let centre = |fraction: f64| -> Option<(f64, f64)> {
            let y = self.row(fraction);
            let &(start, end) = ink_runs(&self.polygons, y, false).first()?;
            (end - start <= SLANT_RUN_MAX_STEMS * stem).then_some((0.5 * (start + end), y))
        };
        let ((low_x, low_y), (high_x, high_y)) = (centre(SLANT_ROWS.0)?, centre(SLANT_ROWS.1)?);
        let tangent = (high_x - low_x) / (high_y - low_y);
        (tangent.is_finite() && tangent.abs() <= MAX_SLANT_TAN).then_some(tangent)
    }
}

/// Ink runs `(start, end)` of the nonzero-winding fill along one scanline, ascending:
/// horizontal at height `at`, or (`vertical`) vertical at abscissa `at`.
fn ink_runs(polygons: &[Vec<Point>], at: f64, vertical: bool) -> Vec<(f64, f64)> {
    let axes = |point: Point| if vertical { (point.y, point.x) } else { (point.x, point.y) };
    let mut crossings: Vec<(f64, i32)> = Vec::new();
    for polygon in polygons {
        for index in 0..polygon.len() {
            let (a_along, a_across) = axes(polygon[index]);
            let (b_along, b_across) = axes(polygon[(index + 1) % polygon.len()]);
            // Half-open rule: a vertex exactly on the scanline counts once.
            if (a_across <= at) != (b_across <= at) {
                let position = a_along + (at - a_across) * (b_along - a_along) / (b_across - a_across);
                crossings.push((position, if b_across > a_across { 1 } else { -1 }));
            }
        }
    }
    crossings.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
    let mut runs = Vec::new();
    let mut winding = 0;
    let mut start = 0.0;
    for (position, direction) in crossings {
        let before = winding;
        winding += direction;
        if before == 0 && winding != 0 {
            start = position;
        } else if before != 0 && winding == 0 {
            runs.push((start, position));
        }
    }
    runs
}

/// The transfer inputs: `base` (recipe index), every source's measured parameters in recipe
/// order, and the raw per-source shape weights the target average is taken with (one per
/// source, positive). A parameter the base lacks stays `None` (neutral).
pub(crate) fn targets(base: usize, base_params: Vec<FontParams>, shape_weights: &[f64]) -> Targets {
    let target = base_params.get(base).map_or_else(FontParams::default, |reference| {
        let average = |get: &dyn Fn(&FontParams) -> Option<(f64, Level)>| -> Option<f64> {
            let (_, level) = get(reference)?;
            let (mut sum, mut total) = (0.0, 0.0);
            for (params, &weight) in base_params.iter().zip(shape_weights) {
                if let Some((value, value_level)) = get(params)
                    && value_level == level
                {
                    sum += weight * value;
                    total += weight;
                }
            }
            (total > 0.0).then(|| sum / total)
        };
        FontParams {
            stem_v: average(&|params| params.stem_v.map(|value| (value, params.levels.stem_v))),
            stem_h: average(&|params| params.stem_h.map(|value| (value, params.levels.stem_h))),
            width: average(&|params| params.width.map(|value| (value, params.levels.width))),
            x_height: average(&|params| params.x_height.map(|value| (value, Level::Latin))),
            cap_height: average(&|params| params.cap_height.map(|value| (value, Level::Latin))),
            slant_tan: average(&|params| params.slant_tan.map(|value| (value, Level::Latin))),
            levels: reference.levels,
        }
    });
    Targets { base, base_params, target }
}

/// Transforms one non-empty base glyph `outline` from the `base` parameters to the `target`
/// ones (order and clamps in the file header). The output advance is not changed here (it
/// stays the spacing blend). With every step below its threshold the outline is returned
/// unchanged.
///
/// # Errors
/// `BlendRejection::Extent` when the glyph has non-finite or oversized coordinates and a step
/// applies, or a thickness change would need a raster beyond the limits of the distance-field
/// path; `BlendRejection::TooManyPoints` when it flattens beyond `sdf::MAX_FLATTENED_POINTS`.
pub(crate) fn apply(outline: &Outline, base: &FontParams, target: &FontParams) -> Result<Outline, BlendRejection> {
    let transform = Transform::new(base, target);
    if transform.is_identity() {
        return Ok(outline.clone());
    }
    // Hostile coordinates are rejected before any point moves or a grid is sized. Control
    // points are checked too: flattening drops an off-curve point whose curve it cannot
    // subdivide (a NaN control), so `checked_bounds` alone would not see it.
    if !outline_points(outline).all(|point| point.x.is_finite() && point.y.is_finite()) {
        return Err(ExtentTooLarge { width: f64::NAN, height: f64::NAN }.into());
    }
    if sdf::checked_bounds(&sdf::flatten_checked(outline, sdf::MAX_FLATTENED_POINTS)?)?.is_none() {
        return Ok(outline.clone());
    }
    let mapped = if transform.moves_points() { map_outline(outline, |point| transform.map(point)) } else { outline.clone() };
    match transform.offset {
        Some((dx, dy)) => sdf::offset_outline(&mapped, dx, dy),
        None => Ok(mapped),
    }
}

/// One glyph transform resolved from base and target parameters (identity parts skipped).
#[derive(Debug)]
struct Transform {
    /// Zone anchors `(base y, mapped y)` above 0, ascending; empty = no zone scale.
    zones: Vec<(f64, f64)>,
    /// Vertical shift after the zone map: the vertical offset, so the offset's growth below
    /// the baseline is pre-compensated (baseline and heights stay on target).
    shift_y: f64,
    /// Shear tangent added (0 = none).
    shear: f64,
    /// Horizontal scale about x = 0 (1 = none).
    scale_x: f64,
    /// Thickness offsets `(horizontal, vertical)` for `sdf::offset_outline`.
    offset: Option<(f64, f64)>,
}

impl Transform {
    /// Resolves the steps (header "Transform order"). The thickness offsets are solved first,
    /// from the counter ratios, and the scales then keep the ink extents on target AFTER the
    /// offset: ink width `s_w * W_b + 2 d_v = W_t`, heights `zone(h_b) + 2 d_h = h_t` with the
    /// baseline kept by `shift_y = d_h`. A clamped offset therefore costs stem accuracy only,
    /// never width or height accuracy.
    fn new(base: &FontParams, target: &FontParams) -> Self {
        let both = |base: Option<f64>, target: Option<f64>| base.zip(target);
        let shear = both(base.slant_tan, target.slant_tan).map_or(0.0, |(from, to)| (to - from).clamp(-MAX_SHEAR_TAN, MAX_SHEAR_TAN));
        let shear = if shear.abs() < SLANT_MIN_TAN { 0.0 } else { shear };

        let stem_v = both(base.stem_v, target.stem_v);
        let stem_h = both(base.stem_h, target.stem_h);
        let width = both(base.width, target.width);
        let x_height = both(base.x_height, target.x_height);
        let cap_height = both(base.cap_height, target.cap_height);
        // Crossbars and lowercase horizontals sit in the lowest zone.
        let lowest_zone = x_height.filter(|&(x_from, _)| cap_height.is_none_or(|(cap_from, _)| x_from < cap_from)).or(cap_height);
        let offset_v = stem_v.map(|stem| axis_offset(stem, width, WIDTH_FACTOR_RANGE));
        let offset_h = stem_h.map(|stem| axis_offset(stem, lowest_zone, ZONE_FACTOR_RANGE));
        let thinner = stem_v.map(|(from, _)| from).into_iter().chain(stem_h.map(|(from, _)| from)).fold(f64::INFINITY, f64::min);
        let limit = MAX_OFFSET.min(MAX_OFFSET_STEM_RATIO * thinner);
        let offset = match (offset_v, offset_h) {
            (Some(dx), Some(dy)) => Some((dx, dy)),
            // One unknown stem direction takes the known one's offset (plain weight change).
            (Some(single), None) | (None, Some(single)) => Some((single, single)),
            (None, None) => None,
        }
        .map(|(dx, dy)| (dx.clamp(-limit, limit), dy.clamp(-limit, limit)))
        .filter(|&(dx, dy)| limit >= MIN_EFFECT && (dx.abs() >= MIN_EFFECT || dy.abs() >= MIN_EFFECT));
        let (dx, dy) = offset.unwrap_or((0.0, 0.0));

        let scale_x = width.map_or(1.0, |(from, to)| {
            let ratio = ((to - 2.0 * dx) / from).clamp(WIDTH_FACTOR_RANGE.0, WIDTH_FACTOR_RANGE.1);
            if (ratio - 1.0).abs() * from < MIN_EFFECT { 1.0 } else { ratio }
        });
        let compensated = |pair: Option<(f64, f64)>| pair.map(|(from, to)| (from, to - 2.0 * dy));
        let zones = zone_anchors(compensated(x_height), compensated(cap_height));
        Self { zones, shift_y: dy, shear, scale_x, offset }
    }

    /// True when no step applies (the outline is returned as is).
    fn is_identity(&self) -> bool {
        !self.moves_points() && self.offset.is_none()
    }

    /// True when the zone (or shift), shear or width step applies.
    fn moves_points(&self) -> bool {
        !self.zones.is_empty() || self.shift_y.abs() > 0.0 || self.shear.abs() > 0.0 || (self.scale_x - 1.0).abs() > 0.0
    }

    /// One control point through zone scale (+ shift), shear (about the baseline) and width
    /// scale.
    fn map(&self, point: Point) -> Point {
        let y = self.map_y(point.y);
        Point::new((point.x + self.shear * y) * self.scale_x, y + self.shift_y)
    }

    /// The piecewise-linear zone map (slope 1 below 0 and above the highest anchor).
    fn map_y(&self, y: f64) -> f64 {
        if y <= 0.0 {
            return y;
        }
        let mut previous = (0.0, 0.0);
        for &(from, to) in &self.zones {
            if y <= from {
                return previous.1 + (y - previous.0) * (to - previous.1) / (from - previous.0);
            }
            previous = (from, to);
        }
        previous.1 + (y - previous.0)
    }
}

/// Zone anchors from the `(base, target)` x-height and cap-height pairs: ascending, each zone
/// factor clamped to `ZONE_FACTOR_RANGE`; empty when no anchor moves by `MIN_EFFECT`. An
/// x-height not below the cap height is dropped (the cap anchor alone defines the zone).
fn zone_anchors(x_height: Option<(f64, f64)>, cap_height: Option<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut wanted: Vec<(f64, f64)> = x_height.into_iter().chain(cap_height).collect();
    if let [(x_from, _), (cap_from, _)] = wanted.as_slice()
        && x_from >= cap_from
    {
        wanted.remove(0);
    }
    let mut anchors = Vec::with_capacity(wanted.len());
    let mut previous = (0.0, 0.0);
    for (from, to) in wanted {
        let factor = ((to - previous.1) / (from - previous.0)).clamp(ZONE_FACTOR_RANGE.0, ZONE_FACTOR_RANGE.1);
        let anchor = (from, previous.1 + (from - previous.0) * factor);
        anchors.push(anchor);
        previous = anchor;
    }
    if anchors.iter().all(|&(from, to)| (to - from).abs() < MIN_EFFECT) {
        anchors.clear();
    }
    anchors
}

/// Half the thickness change of one stem direction, `(T_t - s T_b) / 2`, where `s` is the
/// counter ratio `(E_t - T_t) / (E_b - T_b)` of the extent the stems sit in (ink width for
/// vertical stems, the lowest zone height for horizontal ones), the plain extent ratio when a
/// counter is below `MIN_COUNTER`, 1 without an extent; `s` clamped to `range`.
fn axis_offset((stem_from, stem_to): (f64, f64), extent: Option<(f64, f64)>, range: (f64, f64)) -> f64 {
    let scale = extent.map_or(1.0, |(from, to)| {
        let ratio = if from - stem_from >= MIN_COUNTER && to - stem_to >= MIN_COUNTER { (to - stem_to) / (from - stem_from) } else { to / from };
        ratio.clamp(range.0, range.1)
    });
    (stem_to - scale * stem_from) / 2.0
}

/// Every point (on- and off-curve) of `outline`.
fn outline_points(outline: &Outline) -> impl Iterator<Item = Point> + '_ {
    outline.iter().flat_map(|contour| {
        std::iter::once(contour.start).chain(contour.segs.iter().flat_map(|seg| match *seg {
            Seg::Line(end) => vec![end],
            Seg::Quad(control, end) => vec![control, end],
            Seg::Cubic(control1, control2, end) => vec![control1, control2, end],
        }))
    })
}

/// Every point (on- and off-curve) of `outline` mapped through `map`.
fn map_outline(outline: &Outline, map: impl Fn(Point) -> Point) -> Outline {
    outline
        .iter()
        .map(|contour| Contour {
            start: map(contour.start),
            segs: contour
                .segs
                .iter()
                .map(|seg| match *seg {
                    Seg::Line(end) => Seg::Line(map(end)),
                    Seg::Quad(control, end) => Seg::Quad(map(control), map(end)),
                    Seg::Cubic(control1, control2, end) => Seg::Cubic(map(control1), map(control2), map(end)),
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::source::SourceFont;
    use crate::{BlendAlgorithm, CompileScope, CompiledFont, CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MissingGlyphPolicy, compile};

    const NOTO_REGULAR: &str = "core/00-NotoSans-Regular.ttf";
    const NOTO_BOLD: &str = "bold/00-NotoSans-Bold.ttf";
    const SOURCE_HAN: &str = "core/01-SourceHanSansK-Regular.otf";

    /// Reads a bundled `fonts/ui` file, or logs and returns `None` when it is absent (same
    /// rule as `hybrid::tests`, whose helpers are private to that module).
    fn bundled(relative: &str) -> Option<Vec<u8>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui").join(relative);
        match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                eprintln!("skipping: bundled font {} is unavailable ({error})", path.display());
                None
            }
        }
    }

    fn source<'a>(identity: &'a str, data: &'a [u8]) -> SourceFont<'a> {
        SourceFont { identity, data, face_index: 0, user_kerning: &[] }
    }

    fn face(data: &[u8]) -> SourceFace<'_> {
        SourceFace::parse(&source("face", data)).expect("parses")
    }

    fn transfer_recipe(sources: &[(&str, f32)], base_source: usize) -> CustomFontRecipe {
        CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: sources.iter().map(|&(identity, weight)| HybridSource { identity: identity.to_owned(), weight, missing_override: None }).collect(),
                missing_default: MissingGlyphPolicy::NoInfluence,
                algorithm: BlendAlgorithm::Transfer { base_source },
                property_weights: None,
            }),
        }
    }

    fn run(recipe: &CustomFontRecipe, sources: &[SourceFont<'_>], set: &str) -> CompiledFont {
        let chars: BTreeSet<char> = set.chars().collect();
        compile(recipe, sources, CompileScope::Subset(&chars), &|_, _| {}, &AtomicBool::new(false)).expect("compiles")
    }

    /// Outline and advance of `ch` in a (compiled or source) face.
    fn glyph_of(data: &[u8], ch: char) -> (Outline, f64) {
        let face = face(data);
        let glyph = face.glyph(ch).expect("mapped");
        (face.outline(glyph), face.advance(glyph))
    }

    /// Largest distance from a point of either outline's flattening to the other's boundary.
    fn boundary_distance(a: &Outline, b: &Outline) -> f64 {
        let (pa, pb) = (sdf::flatten(a), sdf::flatten(b));
        let to = |point: Point, polygons: &[Vec<Point>]| {
            let mut best = f64::INFINITY;
            for polygon in polygons {
                for index in 0..polygon.len() {
                    let (start, end) = (polygon[index], polygon[(index + 1) % polygon.len()]);
                    let edge = end - start;
                    let t = if edge.hypot2() > 0.0 { ((point - start).dot(edge) / edge.hypot2()).clamp(0.0, 1.0) } else { 0.0 };
                    best = best.min(point.distance(start + edge * t));
                }
            }
            best
        };
        let one_way = |from: &[Vec<Point>], other: &[Vec<Point>]| from.iter().flatten().map(|&point| to(point, other)).fold(0.0, f64::max);
        one_way(&pa, &pb).max(one_way(&pb, &pa))
    }

    fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Contour {
        // Clockwise (TrueType outer orientation).
        Contour {
            start: Point::new(x0, y0),
            segs: vec![Seg::Line(Point::new(x0, y1)), Seg::Line(Point::new(x1, y1)), Seg::Line(Point::new(x1, y0))],
        }
    }

    /// An "H": two 80-unit stems and a 60-unit crossbar (overlapping contours, nonzero union).
    fn synthetic_h() -> Outline {
        vec![rectangle(100.0, 0.0, 180.0, 700.0), rectangle(420.0, 0.0, 500.0, 700.0), rectangle(100.0, 320.0, 500.0, 380.0)]
    }

    fn params(stem_v: f64, stem_h: f64) -> FontParams {
        FontParams { stem_v: Some(stem_v), stem_h: Some(stem_h), ..FontParams::default() }
    }

    fn no_metrics() -> SourceMetrics {
        SourceMetrics {
            ascender: 800.0,
            descender: -200.0,
            line_gap: 0.0,
            x_height: None,
            cap_height: None,
            underline_position: None,
            underline_thickness: None,
            weight_class: 400.0,
            italic: false,
            italic_angle: 0.0,
        }
    }

    #[test]
    fn noto_regular_and_bold_measure_through_the_latin_chain() {
        let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
            return;
        };
        let (regular_face, bold_face) = (face(&regular), face(&bold));
        let (light, heavy) = (measure(&regular_face), measure(&bold_face));
        for params in [&light, &heavy] {
            assert!(
                [params.stem_v, params.stem_h, params.width, params.x_height, params.cap_height, params.slant_tan].iter().all(Option::is_some),
                "{params:?}"
            );
            assert_eq!(params.levels, Levels::default(), "Latin chain");
            assert!(params.slant_tan.is_some_and(|slant| slant.abs() < SLANT_MIN_TAN), "upright: {params:?}");
        }
        assert!(heavy.stem_v > light.stem_v && heavy.stem_h > light.stem_h, "{light:?} vs {heavy:?}");
        for (params, face) in [(&light, &regular_face), (&heavy, &bold_face)] {
            let os2 = face.metrics().x_height.expect("Noto has sxHeight");
            assert!(params.x_height.is_some_and(|height| (height - os2).abs() <= 10.0), "{params:?} vs sxHeight {os2}");
        }
    }

    #[test]
    fn source_han_measures_through_the_cjk_chain_without_latin() {
        let Some(han) = bundled(SOURCE_HAN) else {
            return;
        };
        let face = face(&han);
        // Latin, Cyrillic and every other script below Hangul Jamo hidden.
        let lookup = |ch: char| (u32::from(ch) >= 0x1100).then(|| face.glyph(ch).map(|glyph| face.outline(glyph))).flatten();
        let params = measure_with(&lookup, &face.metrics());
        assert!(
            [params.stem_v, params.stem_h, params.width, params.x_height, params.cap_height, params.slant_tan].iter().all(Option::is_some),
            "{params:?}"
        );
        assert_eq!(params.levels, Levels { stem_v: Level::Cjk, stem_h: Level::Cjk, width: Level::Cjk });
        // With Latin present the Latin chain wins.
        assert_eq!(measure(&face).levels, Levels::default());
    }

    #[test]
    fn a_face_without_any_reference_glyph_measures_nothing() {
        let params = measure_with(&|_| None, &no_metrics());
        assert_eq!(params, FontParams::default());
        // Zero OS/2 heights are "not set", not a measurement.
        let zero = SourceMetrics { x_height: Some(0.0), cap_height: Some(0.0), ..no_metrics() };
        assert_eq!(measure_with(&|_| None, &zero), FontParams::default());
        // A non-zero italic angle is the slant fallback (PostScript: negative leans right).
        let italic = SourceMetrics { italic_angle: -12.0, ..no_metrics() };
        let slant = measure_with(&|_| None, &italic).slant_tan.expect("from the angle");
        assert!((slant - 12.0_f64.to_radians().tan()).abs() < 1e-12);
    }

    #[test]
    fn targets_average_by_shape_weight_within_one_level() {
        let latin = FontParams { stem_v: Some(80.0), width: Some(500.0), x_height: Some(500.0), ..FontParams::default() };
        let heavy = FontParams { stem_v: Some(140.0), width: Some(560.0), x_height: Some(540.0), ..FontParams::default() };
        let cjk = FontParams {
            stem_v: Some(200.0),
            width: Some(900.0),
            levels: Levels { stem_v: Level::Cjk, stem_h: Level::Latin, width: Level::Cjk },
            ..FontParams::default()
        };
        let targets = targets(0, vec![latin, heavy, cjk], &[1.0, 3.0, 4.0]);
        // The CJK-level values are left out; weights renormalize over the Latin ones.
        assert_eq!(targets.target.stem_v, Some(125.0));
        assert_eq!(targets.target.width, Some(545.0));
        assert_eq!(targets.target.x_height, Some(530.0));
        // The base lacks stem_h / slant: neutral, even though no other source has them either.
        assert_eq!((targets.target.stem_h, targets.target.slant_tan), (None, None));
    }

    #[test]
    fn equal_parameters_return_the_base_outline_exactly() {
        let outline = synthetic_h();
        let same = FontParams { width: Some(400.0), x_height: Some(500.0), cap_height: Some(700.0), slant_tan: Some(0.0), ..params(80.0, 60.0) };
        assert_eq!(apply(&outline, &same, &same), Ok(outline.clone()));
        // Sub-threshold differences are skipped as well.
        let nudged = FontParams { stem_v: Some(80.4), x_height: Some(500.3), slant_tan: Some(0.001), ..same };
        assert_eq!(apply(&outline, &same, &nudged), Ok(outline));
    }

    #[test]
    fn identity_transfer_reproduces_the_base_font() {
        let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
            return;
        };
        // Bold at a negligible weight: every target parameter stays within the thresholds.
        let recipe = transfer_recipe(&[("Regular", 1.0), ("Bold", 0.001)], 0);
        let compiled = run(&recipe, &[source("Regular", &regular), source("Bold", &bold)], "lHxnoagQ&");
        assert_eq!(compiled.stats.transfer, 9, "{:?}", compiled.stats);
        for ch in "lHxnoagQ&".chars() {
            let (output, _) = glyph_of(&compiled.bytes, ch);
            let (expected, _) = glyph_of(&regular, ch);
            let distance = boundary_distance(&output, &expected);
            assert!(distance <= 0.75, "{ch}: {distance}");
        }
    }

    #[test]
    fn a_bolder_target_moves_the_measured_parameters_onto_the_target() {
        let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
            return;
        };
        let recipe = transfer_recipe(&[("Regular", 1.0), ("Bold", 1.0)], 0);
        let compiled = run(&recipe, &[source("Regular", &regular), source("Bold", &bold)], "lHxno");
        let target = targets(0, vec![measure(&face(&regular)), measure(&face(&bold))], &[1.0, 1.0]).target;
        let output = measure(&face(&compiled.bytes));
        let close = |found: Option<f64>, wanted: Option<f64>, tolerance: f64| {
            found.zip(wanted).is_some_and(|(found, wanted)| (found - wanted).abs() <= tolerance)
        };
        assert!(close(output.stem_v, target.stem_v, 2.0), "stem_v {output:?} vs {target:?}");
        // Noto's crossbar / stem ratio differs between the weights: the anisotropic case.
        assert!(close(output.stem_h, target.stem_h, 2.0), "stem_h {output:?} vs {target:?}");
        assert!(close(output.x_height, target.x_height, 2.0), "x_height {output:?} vs {target:?}");
        assert!(close(output.cap_height, target.cap_height, 2.0), "cap_height {output:?} vs {target:?}");
        assert!(close(output.width, target.width, 3.0), "width {output:?} vs {target:?}");
    }

    #[test]
    fn anisotropic_offsets_land_on_both_stem_targets() {
        let base = params(80.0, 60.0);
        // Same sign (one stretched pass) and opposite signs (two passes).
        for (stem_v, stem_h) in [(100.0, 90.0), (110.0, 50.0), (70.0, 76.0)] {
            let output = apply(&synthetic_h(), &base, &params(stem_v, stem_h)).expect("in range");
            let glyph = Reference::of(&output).expect("ink");
            let measured_v = glyph.vertical_stem(STEM_ROWS).expect("stem");
            let measured_h = glyph.horizontal_stem(0.5).expect("bar");
            assert!((measured_v - stem_v).abs() <= 2.0, "target {stem_v}/{stem_h}: vertical {measured_v}");
            assert!((measured_h - stem_h).abs() <= 2.0, "target {stem_v}/{stem_h}: horizontal {measured_h}");
        }
    }

    #[test]
    fn slant_target_shears_and_is_measured_back() {
        let stem = vec![rectangle(100.0, 0.0, 180.0, 700.0)];
        let upright = FontParams { slant_tan: Some(0.0), ..FontParams::default() };
        let tangent = 12.0_f64.to_radians().tan();
        let italic = FontParams { slant_tan: Some(tangent), ..FontParams::default() };
        let output = apply(&stem, &upright, &italic).expect("in range");
        // Corners move by tan * y; the baseline stays.
        assert_eq!(output[0].start, Point::new(100.0, 0.0));
        let top_left = output[0].segs[0].end();
        assert!((top_left.x - (100.0 + tangent * 700.0)).abs() < 1e-9 && (top_left.y - 700.0).abs() < 1e-9, "{top_left:?}");
        let lookup = |ch: char| (ch == 'l').then(|| output.clone());
        let measured = measure_with(&lookup, &no_metrics()).slant_tan.expect("slant of the sheared stem");
        assert!((measured - tangent).abs() < 1e-3, "{measured} vs {tangent}");
    }

    #[test]
    fn zone_scale_moves_the_x_height_and_keeps_the_cap_height() {
        let x_box = vec![rectangle(50.0, 0.0, 450.0, 500.0)];
        let cap_box = vec![rectangle(50.0, -100.0, 450.0, 700.0)];
        let base = FontParams { x_height: Some(500.0), cap_height: Some(700.0), ..FontParams::default() };
        let target = FontParams { x_height: Some(550.0), cap_height: Some(700.0), ..FontParams::default() };
        let tops = |outline: &Outline| sdf::bounds(&sdf::flatten(outline)).expect("ink");
        let raised = tops(&apply(&x_box, &base, &target).expect("in range"));
        assert!((raised.y1 - 550.0).abs() < 1e-9 && raised.y0.abs() < 1e-9, "{raised:?}");
        let capital = tops(&apply(&cap_box, &base, &target).expect("in range"));
        assert!((capital.y1 - 700.0).abs() < 1e-9 && (capital.y0 + 100.0).abs() < 1e-9, "descender unchanged: {capital:?}");
        // Factors are clamped: a doubled x-height becomes 1.4x.
        let doubled = FontParams { x_height: Some(1000.0), cap_height: Some(1400.0), ..FontParams::default() };
        let clamped = tops(&apply(&x_box, &base, &doubled).expect("in range"));
        assert!((clamped.y1 - 700.0).abs() < 1e-9, "{clamped:?}");
    }

    #[test]
    fn preview_subset_glyphs_equal_the_larger_scope() {
        let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
            return;
        };
        let recipe = transfer_recipe(&[("Regular", 1.0), ("Bold", 1.0)], 0);
        let sources = [source("Regular", &regular), source("Bold", &bold)];
        // The wide scope holds every reference glyph; the narrow one none but `l`.
        let narrow = run(&recipe, &sources, "l");
        let wide = run(&recipe, &sources, "lIHxnoпоН十口abc");
        assert_eq!(glyph_of(&narrow.bytes, 'l'), glyph_of(&wide.bytes, 'l'));
    }

    #[test]
    fn transfer_is_deterministic_across_thread_counts() {
        let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
            return;
        };
        let recipe = transfer_recipe(&[("Regular", 0.3), ("Bold", 0.7)], 0);
        let sources = [source("Regular", &regular), source("Bold", &bold)];
        let one = rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("pool");
        let many = rayon::ThreadPoolBuilder::new().num_threads(4).build().expect("pool");
        let first = one.install(|| run(&recipe, &sources, "lHxnoag&"));
        let second = many.install(|| run(&recipe, &sources, "lHxnoag&"));
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.stats, second.stats);
    }

    #[test]
    fn hostile_outlines_are_rejected_not_gridded() {
        let base = params(80.0, 60.0);
        let target = params(100.0, 80.0);
        let huge = vec![rectangle(0.0, 0.0, 1.0e12, 1.0e12)];
        assert!(apply(&huge, &base, &target).is_err());
        let broken = vec![rectangle(0.0, 0.0, f64::NAN, 100.0)];
        assert!(apply(&broken, &base, &target).is_err());
        // An identity transform never looks at the coordinates (the copy path does not either).
        assert_eq!(apply(&huge, &base, &base), Ok(huge.clone()));
    }

    /// A stem-shaped contour of 800 cubics that each flatten 64-fold: beyond the flattening
    /// budget of the raster paths.
    fn dense_stem() -> Outline {
        let mut segs: Vec<Seg> = (0..800)
            .map(|k| {
                let y = f64::from(k + 1) * 0.875;
                Seg::Cubic(Point::new(-5000.0, y - 1.2), Point::new(5000.0, y - 0.6), Point::new(100.0, y))
            })
            .collect();
        segs.extend([Seg::Line(Point::new(180.0, 700.0)), Seg::Line(Point::new(180.0, 0.0))]);
        vec![Contour { start: Point::new(100.0, 0.0), segs }]
    }

    #[test]
    fn an_over_budget_reference_glyph_falls_through_to_the_next_in_its_chain() {
        let dense = dense_stem();
        assert!(sdf::flatten_checked(&dense, sdf::MAX_FLATTENED_POINTS).is_err(), "the fixture must exceed the budget");
        // 'l' (first in the stem chain) is over budget; 'I' is an 80-unit stem.
        let lookup = |ch: char| match ch {
            'l' => Some(dense.clone()),
            'I' => Some(vec![rectangle(100.0, 0.0, 180.0, 700.0)]),
            _ => None,
        };
        let measured = measure_with(&lookup, &no_metrics());
        assert!(measured.stem_v.is_some_and(|stem| (stem - 80.0).abs() < 0.5), "{measured:?}");
    }

    #[test]
    fn an_over_budget_glyph_is_rejected_as_too_dense() {
        let rejection = apply(&dense_stem(), &params(80.0, 60.0), &params(100.0, 70.0));
        assert!(matches!(rejection, Err(BlendRejection::TooManyPoints(_))), "{rejection:?}");
    }
}
