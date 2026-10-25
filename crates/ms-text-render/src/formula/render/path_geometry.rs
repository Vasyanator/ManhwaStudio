/*
File: crates/ms-text-render/src/formula/render/path_geometry.rs

Purpose:
Geometry along a custom line path: the per-line layout settings (distance mode, flip,
placement band), offset-group rotations (formula and drawn lines), path sampling in the
text direction, and the two arc-length solvers of the MinimumPreviousDistance search.

Key functions:
- custom_line_layout_settings(), `CustomLineLayoutSettings` (impl), vector_line_distance_mode(),
  vector_line_flip_rotation()
- apply_formula_group_rotations() / apply_drawn_line_group_rotations(), rotate_point_around()
- sample_drawn_line_path(), sample_drawn_line_path_for_direction()
- solve_directional_gap_center_s(), find_clearance_center_s()
*/

use crate::drawn_lines::DrawnLinePath;
use crate::glyph_contour::PlacedContour;
use crate::types::{
    LinePlacementReference, TextLayoutMode, TextRenderParams, TextVectorLineDistanceMode,
    TextVectorLineTextDirection,
};
use std::collections::HashMap;
use super::{
    CustomLineLayoutSettings, DrawnLineTransform, FormulaGlyphSeed, FormulaGlyphTransform,
    LinePlacementBand, ON_PATH_INK_BISECTION_STEPS, ON_PATH_INK_GAP_EPSILON_PX,
    ON_PATH_INK_SCAN_SAMPLES, OffsetGroupKey, OffsetGroupRotation, PlacedGlyphInk, PlacedNeighbor,
};
use super::seed_ink::{clears_floor, on_path_pair_gap};

impl CustomLineLayoutSettings {
    /// This layout's cross-line placement band.
    pub(super) fn placement_band(&self) -> LinePlacementBand {
        LinePlacementBand {
            frac: self.line_placement_frac,
            reference: self.line_placement_reference,
            ascent_scaled: self.ascent_scaled,
        }
    }
}

pub(super) fn custom_line_layout_settings(
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

pub(super) fn vector_line_distance_mode(
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

pub(super) fn vector_line_flip_rotation(params: &TextRenderParams, line_idx: usize) -> f32 {
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

pub(super) fn is_last_seed_in_offset_span_on_line(
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

fn offset_group_key(seed: &FormulaGlyphSeed) -> Option<OffsetGroupKey> {
    let (start, end) = seed.offset_span_range?;
    Some(OffsetGroupKey {
        line_idx: seed.line_idx,
        start,
        end,
    })
}

pub(super) fn apply_formula_group_rotations(
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

pub(super) fn apply_drawn_line_group_rotations(
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

pub(super) fn sample_drawn_line_path_for_direction(
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
pub(super) fn solve_directional_gap_center_s<F>(
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
pub(super) fn find_clearance_center_s<F>(
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
