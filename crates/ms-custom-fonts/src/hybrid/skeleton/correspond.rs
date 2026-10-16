/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/correspond.rs

Purpose:
Stroke correspondence between the reference source's skeleton and every other source's:
which major stroke of a source plays the role of which reference stroke (and in which
direction / cyclic phase), and likewise for decorations.

Key structures:
- `Pairing`: the reference stroke a stroke corresponds to and how its arc parameter maps.
- `Correspondence`: pairings of every source's majors and decorations.

Key functions:
- `correspond`: skeletons + reference -> `Correspondence` or `SkeletonFallback::Unmatched`.

Notes:
- Cost of a (reference stroke, other stroke, direction) pair: mean distance of `MATCH_SAMPLES`
  arc-uniform samples / ink-box diagonal + 0.5 x |length difference| / diagonal. Two free loops
  are compared over `LOOP_SAMPLES` samples at every cyclic shift; a free loop never pairs with a
  stroke that has ends.
- A candidate needs cost <= `MATCH_MAX_COST` and every sample pair within
  `MATCH_MAX_SAMPLE_DISTANCE` diagonals (one far end means a different stroke, even when the
  mean is small). Assignment is greedy one-to-one over the candidates, in
  ascending (cost, reference index, other index, reversed, shift) — fully deterministic.
- A source whose matched share of the reference's major stroke length (stroke count when the
  reference has only dots) is below `MIN_MATCHED_FRACTION` fails the glyph (`Unmatched`).
- Decorations pair only when their majors are paired with each other (same cost, fixed
  direction: anchor first).
*/

use std::cmp::Ordering;

use kurbo::Point;

use super::geometry::{Stroke, index_f64};
use super::graph::Skeleton;
use super::{MATCH_MAX_COST, MATCH_MAX_SAMPLE_DISTANCE, MIN_MATCHED_FRACTION, SkeletonFallback};

/// Samples per stroke of the open-stroke cost.
const MATCH_SAMPLES: usize = 8;
/// Samples (and cyclic shifts) of the free-loop cost.
const LOOP_SAMPLES: usize = 16;

/// How a stroke's arc parameter follows its reference stroke's: for reference fraction `u`, the
/// stroke is sampled at `map(u)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ParamMap {
    /// `u`.
    Same,
    /// `1 - u`.
    Reversed,
    /// Free loops: `(shift ± u) mod 1` (`-` when `reversed`).
    Cyclic { reversed: bool, shift: f64 },
}

impl ParamMap {
    /// The stroke's arc fraction for reference fraction `u`.
    pub(super) fn apply(self, u: f64) -> f64 {
        match self {
            Self::Same => u,
            Self::Reversed => 1.0 - u,
            Self::Cyclic { reversed, shift } => {
                let value = if reversed { shift - u } else { shift + u };
                value.rem_euclid(1.0)
            }
        }
    }
}

/// The reference major (or decoration) a stroke corresponds to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Pairing {
    pub(super) reference: usize,
    pub(super) map: ParamMap,
}

/// Pairings of every source's strokes (`None`: unmatched; the reference source's own entries
/// are all `None`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Correspondence {
    /// `[source][major]`.
    pub(super) majors: Vec<Vec<Option<Pairing>>>,
    /// `[source][decoration]` -> reference decoration index.
    pub(super) decorations: Vec<Vec<Option<usize>>>,
}

/// One scored candidate pair.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    cost: f64,
    reference: usize,
    other: usize,
    map: ParamMap,
}

/// Matches every non-reference skeleton against `skeletons[reference]`. `diagonal` (> 0) is the
/// aligned ink box diagonal, the cost's length unit.
///
/// # Errors
/// `Unmatched` with the first failing source's matched fraction when it is below
/// `MIN_MATCHED_FRACTION`.
pub(super) fn correspond(skeletons: &[Skeleton], reference: usize, diagonal: f64) -> Result<Correspondence, SkeletonFallback> {
    let base = &skeletons[reference];
    let reference_samples: Vec<Vec<Point>> = base.majors.iter().map(|major| samples(&major.stroke, major.free_loop)).collect();
    let total_length: f64 = base.majors.iter().map(|major| major.length).sum();
    let mut majors = Vec::with_capacity(skeletons.len());
    let mut decorations = Vec::with_capacity(skeletons.len());
    for (source, skeleton) in skeletons.iter().enumerate() {
        if source == reference {
            majors.push(vec![None; skeleton.majors.len()]);
            decorations.push(vec![None; skeleton.decorations.len()]);
            continue;
        }
        let mut candidates = Vec::new();
        for (other, major) in skeleton.majors.iter().enumerate() {
            let other_samples = samples(&major.stroke, major.free_loop);
            for (index, reference_major) in base.majors.iter().enumerate() {
                if reference_major.free_loop != major.free_loop {
                    continue;
                }
                let (mean, map) = if major.free_loop {
                    loop_distance(&reference_samples[index], &other_samples)
                } else {
                    open_distance(&reference_samples[index], &other_samples)
                };
                let cost = (mean + 0.5 * (reference_major.length - major.length).abs()) / diagonal;
                if cost <= MATCH_MAX_COST && worst_distance(&reference_samples[index], &other_samples, map) <= MATCH_MAX_SAMPLE_DISTANCE * diagonal {
                    candidates.push(Candidate { cost, reference: index, other, map });
                }
            }
        }
        let pairs = assign(candidates, base.majors.len(), skeleton.majors.len());
        let mut matched_length = 0.0;
        let mut matched_count = 0_usize;
        for pairing in pairs.iter().flatten() {
            matched_length += base.majors[pairing.reference].length;
            matched_count += 1;
        }
        let matched_fraction =
            if total_length > 0.0 { matched_length / total_length } else { index_f64(matched_count) / index_f64(base.majors.len().max(1)) };
        if matched_fraction < MIN_MATCHED_FRACTION {
            return Err(SkeletonFallback::Unmatched { matched_fraction });
        }
        decorations.push(match_decorations(base, skeleton, &pairs, diagonal));
        majors.push(pairs);
    }
    Ok(Correspondence { majors, decorations })
}

/// Decoration pairing of one source against the reference (anchor-first, majors paired).
fn match_decorations(base: &Skeleton, skeleton: &Skeleton, major_pairs: &[Option<Pairing>], diagonal: f64) -> Vec<Option<usize>> {
    let reference_samples: Vec<Vec<Point>> = base.decorations.iter().map(|decoration| samples(&decoration.stroke, false)).collect();
    let mut candidates = Vec::new();
    for (other, decoration) in skeleton.decorations.iter().enumerate() {
        let Some(Some(major_pair)) = major_pairs.get(decoration.major) else {
            continue;
        };
        let other_samples = samples(&decoration.stroke, false);
        for (index, reference_decoration) in base.decorations.iter().enumerate() {
            if reference_decoration.major != major_pair.reference {
                continue;
            }
            let mean = mean_distance(&reference_samples[index], &other_samples, |k| k);
            let cost = (mean + 0.5 * (reference_decoration.length - decoration.length).abs()) / diagonal;
            if cost <= MATCH_MAX_COST && worst_distance(&reference_samples[index], &other_samples, ParamMap::Same) <= MATCH_MAX_SAMPLE_DISTANCE * diagonal {
                candidates.push(Candidate { cost, reference: index, other, map: ParamMap::Same });
            }
        }
    }
    assign(candidates, base.decorations.len(), skeleton.decorations.len()).into_iter().map(|pair| pair.map(|pair| pair.reference)).collect()
}

/// Greedy one-to-one assignment in ascending (cost, reference, other, map order).
fn assign(mut candidates: Vec<Candidate>, reference_count: usize, other_count: usize) -> Vec<Option<Pairing>> {
    candidates.sort_by(|a, b| {
        a.cost
            .total_cmp(&b.cost)
            .then(a.reference.cmp(&b.reference))
            .then(a.other.cmp(&b.other))
            .then_with(|| map_order(a.map).total_cmp(&map_order(b.map)))
    });
    let mut reference_used = vec![false; reference_count];
    let mut pairs = vec![None; other_count];
    for candidate in candidates {
        if reference_used[candidate.reference] || pairs[candidate.other].is_some() {
            continue;
        }
        reference_used[candidate.reference] = true;
        pairs[candidate.other] = Some(Pairing { reference: candidate.reference, map: candidate.map });
    }
    pairs
}

/// Total order key of a parameter map among equal-cost candidates.
fn map_order(map: ParamMap) -> f64 {
    match map {
        ParamMap::Same => 0.0,
        ParamMap::Reversed => 1.0,
        ParamMap::Cyclic { reversed, shift } => 2.0 + if reversed { 1.0 } else { 0.0 } + shift / 2.0,
    }
}

/// Arc-uniform samples: `MATCH_SAMPLES` from start to end, or `LOOP_SAMPLES` around a free
/// loop (the closing point excluded).
fn samples(stroke: &Stroke, free_loop: bool) -> Vec<Point> {
    let sampler = stroke.sampler();
    if free_loop {
        (0..LOOP_SAMPLES).map(|k| sampler.at(index_f64(k) / index_f64(LOOP_SAMPLES)).0).collect()
    } else {
        (0..MATCH_SAMPLES).map(|k| sampler.at(index_f64(k) / index_f64(MATCH_SAMPLES - 1)).0).collect()
    }
}

/// Mean distance between `a[k]` and `b[pick(k)]`.
fn mean_distance(a: &[Point], b: &[Point], pick: impl Fn(usize) -> usize) -> f64 {
    let sum: f64 = a.iter().enumerate().map(|(k, point)| point.distance(b[pick(k) % b.len()])).sum();
    sum / index_f64(a.len().max(1))
}

/// Largest distance between `a[k]` and `b[map(k)]` for a chosen parameter map (sample
/// positions of `b` taken at the mapped fractions; free loops use their sample count).
fn worst_distance(a: &[Point], b: &[Point], map: ParamMap) -> f64 {
    let count = b.len();
    let pick = |k: usize| -> usize {
        match map {
            ParamMap::Same => k,
            ParamMap::Reversed => count - 1 - k.min(count - 1),
            ParamMap::Cyclic { reversed, shift } => {
                // `shift` is a multiple of 1 / count (see `loop_distance`).
                let steps = (0..count).find(|&step| (index_f64(step) / index_f64(count) - shift).abs() < 1e-9).unwrap_or(0);
                if reversed { (steps + count - k % count) % count } else { (steps + k) % count }
            }
        }
    };
    a.iter().enumerate().map(|(k, point)| point.distance(b[pick(k) % count])).fold(0.0, f64::max)
}

/// Best mean sample distance of two open strokes over both directions (forward first).
fn open_distance(reference: &[Point], other: &[Point]) -> (f64, ParamMap) {
    let last = other.len() - 1;
    let forward = mean_distance(reference, other, |k| k);
    let backward = mean_distance(reference, other, |k| last - k.min(last));
    if backward < forward { (backward, ParamMap::Reversed) } else { (forward, ParamMap::Same) }
}

/// Best mean sample distance of two free loops over both directions and every cyclic shift
/// (forward and smaller shifts first on ties).
fn loop_distance(reference: &[Point], other: &[Point]) -> (f64, ParamMap) {
    let count = other.len();
    let mut best = (f64::INFINITY, ParamMap::Same);
    for reversed in [false, true] {
        for shift in 0..count {
            let mean = mean_distance(reference, other, |k| if reversed { (shift + count - k % count) % count } else { (shift + k) % count });
            if mean.partial_cmp(&best.0) == Some(Ordering::Less) {
                best = (mean, ParamMap::Cyclic { reversed, shift: index_f64(shift) / index_f64(count) });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyclic_maps_wrap_around() {
        let map = ParamMap::Cyclic { reversed: true, shift: 0.25 };
        assert!((map.apply(0.5) - 0.75).abs() < 1e-12);
        let map = ParamMap::Cyclic { reversed: false, shift: 0.75 };
        assert!((map.apply(0.5) - 0.25).abs() < 1e-12);
        assert!((ParamMap::Reversed.apply(0.2) - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_rotated_loop_is_found_with_its_shift() {
        let circle = |phase: f64, reversed: bool| -> Vec<Point> {
            (0..LOOP_SAMPLES)
                .map(|k| {
                    let step = index_f64(k) / index_f64(LOOP_SAMPLES) * std::f64::consts::TAU;
                    let angle = phase + if reversed { -step } else { step };
                    Point::new(100.0 * angle.cos(), 100.0 * angle.sin())
                })
                .collect()
        };
        let reference = circle(0.0, false);
        let other = circle(std::f64::consts::TAU * 0.25, true);
        let (mean, map) = loop_distance(&reference, &other);
        assert!(mean < 1e-9, "{mean}");
        // Reference sample k (angle k) is other sample (shift - k): other k' has angle 0.25 turn - k'.
        assert_eq!(map, ParamMap::Cyclic { reversed: true, shift: 0.25 });
    }
}
