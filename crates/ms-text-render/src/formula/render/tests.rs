//! Unit tests of the formula / custom-line render path (`formula/render/mod.rs` and its
//! children), including the seed characterization matrix (`seed_characterization_matrix`,
//! fixture `tests/fixtures/pair_advance_seed_matrix.json`).

use crate::glyph_blit::GlyphBox;
use super::{
    DrawnLineDropSide, DrawnLineTransform, FormulaGlyphSeed, GlyphScaleSettings, HorizontalAlign,
    KerningSettings, LinePlacementBand, LinePlacementReference, ON_PATH_INK_CLEARANCE_FLOOR_PX,
    ON_PATH_INK_SEARCH_MAX_RANGE_PX, ON_PATH_INK_SEARCH_MIN_RANGE_PX, OnPathStepSpacing,
    PlacedGlyphInk, PlacedNeighbor, apply_line_placement, drawn_line_glyph_destination_center_raw,
};
use super::drawn_lines_layout::{
    drawn_line_center_s, drawn_line_next_cursor, drawn_line_start_offset, drawn_line_start_offsets,
    on_path_align_fraction,
};
use super::line_transforms::drawn_line_drop_side;
use super::seed_ink::{
    clears_floor, on_path_pair_gap, pair_chord_axis, pair_clearance_floor,
    placed_contour_for_transform,
};
use super::path_geometry::{
    find_clearance_center_s, sample_drawn_line_path_for_direction, solve_directional_gap_center_s,
};
use super::formula_layout::map_formula_target_arc_length;
use crate::drawn_lines::{DrawnLinePath, DrawnLinePoint};
use crate::inline_styles::InlineGlyphOffset;
use crate::pipeline::FauxGlyphStyle;
use crate::types::KerningMode;
use cosmic_text::{CacheKeyFlags, LayoutGlyph, fontdb};
use crate::glyph_contour::{
    GlyphContour, PlacedContour, min_placed_distance,
};
use crate::raster::{RotatedRect, rotated_rect_world_bounds};
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
    // (`PairAdvanceModel::is_overridden`, checked by
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
    let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(RotatedRect {
        rect: (scaled_left, scaled_top, scaled_width, scaled_height),
        dst_center: [dst_cx, dst_cy],
        rotation_rad: transform.rotation_rad,
    });

    let placed = placed_contour_for_transform(
        &contour,
        GlyphBox {
            placement_left,
            placement_top,
            glyph_w,
            glyph_h,
            scale: GlyphScaleSettings {
                width_mul,
                height_mul,
            },
            subpixel: [0.0, 0.0],
            shear_x: 0.0,
        },
        scaled_height,
        LinePlacementBand {
            frac: line_frac,
            reference: LinePlacementReference::GlyphHeight,
            ascent_scaled: 0.0,
        },
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

/// Fixture font for the seed-advance contract: the same tracked Arimo caller font
/// the pipeline tests use (`crate::test_fixtures`). A test binary cannot reach the shipped `fonts/ui` bundle,
/// and this contract is about the advance arithmetic, not about the bundle.
fn seed_fixture_font_path() -> std::path::PathBuf {
    crate::test_fixtures::caller_font_path()
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
    let seeds = seeds_from_run(&run, |_glyph| KerningSettings {
        mode,
        spacing_px: 0.0,
        spacing_percent: 0.0,
        // Set unconditionally: the production caller sets it from the
        // render's `CustomKerningMap`, and leaving it false would take the
        // whole run down the fast path that ignores overrides.
        custom_pairs: true,
    });
    // `run` borrows `buffer`, but every seed OWNS its data (the glyph is
    // cloned, the cluster char is copied), so NLL ends that borrow right
    // here. `buffer` is local and dies with the fixture.
    (font_system, face_id, seeds)
}

/// Turns one shaped layout run into the seeds `assign_formula_seed_advances`
/// consumes, one per glyph, all on line 0; `kerning_for` supplies each seed's
/// resolved kerning settings (the production caller stamps them per glyph).
fn seeds_from_run(
    run: &cosmic_text::LayoutRun<'_>,
    kerning_for: impl Fn(&LayoutGlyph) -> KerningSettings,
) -> Vec<FormulaGlyphSeed> {
    let glyph_count = run.glyphs.len();
    run.glyphs
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
            kerning: kerning_for(glyph),
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
        .collect::<Vec<_>>()
}

/// A `TextRenderParams` whose single vector line runs in
/// `MinimumPreviousDistance`, which is what
/// [`super::line_transforms::straight_reference_target_gaps`] needs to measure a line at all.
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
    super::seeds::assign_formula_seed_advances(
        seeds.as_mut_slice(),
        &mut font_system,
        &custom_kerning,
        em,
        (em * 0.5).max(1.0),
    );
    (font_system, custom_kerning, seeds)
}

/// The line target gap [`super::line_transforms::straight_reference_target_gaps`] computes for
/// `text`, with an optional user-authored kerning table on the fixture face.
///
/// Runs the real production chain: shape -> `assign_formula_seed_advances`
/// (so the authored override lands in the advances exactly as in a render) ->
/// the pre-pass.
fn line_target_gap_for(text: &str, pairs: &[(char, char, f32)]) -> Option<f32> {
    let (mut font_system, custom_kerning, seeds) = min_distance_fixture(text, pairs);
    let params = min_distance_line_params();
    let layout = super::path_geometry::custom_line_layout_settings(&params, 0.0, 0.0);
    let spacing = OnPathStepSpacing::for_custom_lines(1.0, 0.0);
    let mut caches = super::OnPathInkCaches::new();
    super::line_transforms::straight_reference_target_gaps(
        &params,
        seeds.as_slice(),
        &layout,
        spacing,
        &custom_kerning,
        &mut font_system,
        &mut caches,
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
    let mut caches = super::OnPathInkCaches::new();
    let paths = vec![Some(path.clone())];
    super::line_transforms::build_drawn_line_transforms(
        &params,
        seeds.as_slice(),
        paths.as_slice(),
        &custom_kerning,
        &mut font_system,
        &mut caches,
        super::path_geometry::custom_line_layout_settings(&params, 0.0, 0.0),
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
    let mut caches = super::OnPathInkCaches::new();
    let paths = vec![Some(path.clone())];
    let transforms = super::line_transforms::build_drawn_line_transforms(
        &params,
        seeds.as_slice(),
        paths.as_slice(),
        &custom_kerning,
        &mut font_system,
        &mut caches,
        super::path_geometry::custom_line_layout_settings(&params, 0.0, 0.0),
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
    let layout = super::path_geometry::custom_line_layout_settings(&params, 0.0, 0.0);
    let mut caches = super::OnPathInkCaches::new();
    let paths = vec![Some(path.clone())];
    let transforms = super::line_transforms::build_drawn_line_transforms(
        &params,
        seeds.as_slice(),
        paths.as_slice(),
        &custom_kerning,
        &mut font_system,
        &mut caches,
        super::path_geometry::custom_line_layout_settings(&params, 0.0, 0.0),
    );

    let mut gaps = Vec::new();
    let mut previous: Option<PlacedGlyphInk> = None;
    for (seed, transform) in seeds.iter().zip(transforms.iter()) {
        let placed = transform
            .as_ref()
            .expect("every glyph of this fixture must be placed");
        let ink = super::seed_ink::seed_ink_geometry(
            seed,
            &mut font_system,
            &mut caches,
        )
        .map(|geom| super::seed_ink::place_seed_ink_for_transform(seed, &geom, &layout, placed));
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
        super::seeds::assign_formula_seed_advances(
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
        super::seeds::assign_formula_seed_advances(
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

/// Pins the quarter-advance floor of [`super::seeds::seed_metric_advance_px`], which
/// had no test at all. It is the guard that keeps a non-monotonic (RTL) run or
/// a font pair kerned below -75% from stalling the arc-length cursor; the
/// `1.0` term takes over only for a glyph too narrow for the quarter to reach
/// it. Lowering either value is a rendering change, not a cleanup.
#[test]
fn the_metric_advance_floor_turns_a_backwards_pen_delta_into_a_quarter_advance() {
    // Ordinary forward delta: the floor must not touch it.
    assert!(
        (super::seeds::seed_metric_advance_px(57.0, 60.0) - 57.0).abs() < 1e-3,
        "a forward pen delta must pass through untouched"
    );
    // Font pair kerning below -75% of the left advance, and the RTL case where
    // `glyph.x` decreases outright: both land on the quarter-advance floor.
    assert!(
        (super::seeds::seed_metric_advance_px(5.0, 80.0) - 20.0).abs() < 1e-3,
        "a delta under a quarter of the glyph advance must clamp to that quarter"
    );
    assert!(
        (super::seeds::seed_metric_advance_px(-140.0, 80.0) - 20.0).abs() < 1e-3,
        "a NEGATIVE (right-to-left) pen delta must become a positive magnitude"
    );
    // Sub-pixel glyph: the quarter is below 1.0, so the absolute guard wins.
    assert!(
        (super::seeds::seed_metric_advance_px(-3.0, 2.0) - 1.0).abs() < 1e-3,
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
    super::seeds::assign_formula_seed_advances(
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

// ---- Characterization of the advance between two adjacent glyphs (seed side) ----

/// The seed matrix: every text x mode x tracking x table x em of
/// `crate::characterization`, as each seed's `advance_px` after
/// `assign_formula_seed_advances`. Kerning settings are stamped the way the
/// production seed capture stamps them: tracking resolved as `KerningSettings::from_params`
/// resolves it, and `custom_pairs` from the map's `has_table` for the glyph's own face, so
/// the `none` table rows exercise the default-metric fast path.
fn seed_characterization_matrix() -> Vec<crate::characterization::MatrixRow> {
    use crate::characterization::{
        MATRIX_EMS, MATRIX_MODES, MATRIX_TABLES, MATRIX_TEXTS, MATRIX_TRACKING, MatrixRow,
        TrackedFont, case_name,
    };
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use crate::pair_advance::effective_spacing_percent;
    use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};
    use std::sync::Arc;

    let mut latin = TrackedFont::Latin.load();
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut rows = Vec::new();
    for text in &MATRIX_TEXTS {
        let font = match text.font {
            TrackedFont::Latin => &mut latin,
            TrackedFont::Hebrew => &mut hebrew,
        };
        for (mode_label, mode) in MATRIX_MODES {
            for (tracking_label, kerning_px, kerning_percent) in MATRIX_TRACKING {
                for (table_label, pairs) in MATRIX_TABLES {
                    for em in MATRIX_EMS {
                        let mut custom_kerning = CustomKerningMap::default();
                        if !pairs.is_empty() {
                            custom_kerning.insert(
                                font.face_id,
                                Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                                "characterization",
                            );
                        }
                        let font_system = &mut font.font_system;
                        let mut buffer = Buffer::new(font_system, Metrics::new(em, em * 1.2));
                        buffer.set_size(font_system, None, None);
                        let attrs = Attrs::new()
                            .family(Family::Name(font.family.as_str()))
                            .metrics(Metrics::new(em, em));
                        buffer.set_text(font_system, text.text, &attrs, Shaping::Advanced);
                        buffer.shape_until_scroll(font_system, false);
                        let run = buffer.layout_runs().next().expect("one layout run");
                        let mut seeds = seeds_from_run(&run, |glyph| {
                            KerningSettings {
                                mode,
                                spacing_px: kerning_px.clamp(-300.0, 300.0),
                                spacing_percent: effective_spacing_percent(kerning_percent, 100.0),
                                custom_pairs: false,
                            }
                            .with_custom_pairs(custom_kerning.has_table(glyph.font_id))
                        });
                        super::seeds::assign_formula_seed_advances(
                            seeds.as_mut_slice(),
                            font_system,
                            &custom_kerning,
                            em,
                            (em * 0.5).max(1.0),
                        );
                        rows.push(MatrixRow {
                            case: case_name(text, mode_label, tracking_label, table_label, em),
                            fields: vec![(
                                "advances",
                                seeds.iter().map(|seed| seed.advance_px).collect(),
                            )],
                        });
                    }
                }
            }
        }
    }
    rows
}

/// The formula seed advances (`assign_formula_seed_advances`, all three modes, tracking,
/// overrides, the fast path) must reproduce the recorded matrix BIT FOR BIT. A deliberate
/// behaviour change regenerates the fixture with
/// `MS_TEXT_RENDER_REGEN_CHARACTERIZATION=1` and names the rows it moved.
#[test]
fn seed_advances_match_the_recorded_characterization() {
    crate::characterization::check_against_fixture(
        "pair_advance_seed_matrix.json",
        "ms-text-render seed characterization: formula::render::assign_formula_seed_advances \
         advance_px per seed as f32 bit patterns (see src/characterization.rs)",
        &seed_characterization_matrix(),
    );
}
