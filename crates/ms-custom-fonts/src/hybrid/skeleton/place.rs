/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/place.rs

Purpose:
Blends corresponding strokes into the output stroke set: matched groups by weighted average of
position and radius, strokes only one source has by growing / shrinking them from where they
attach, decorations relative to their (blended) anchor.

Key functions:
- `place`: skeletons + weights + correspondence -> output strokes (aligned coordinates).

Notes:
- Group = one reference major and every stroke paired with it. Two or more holders: resampled
  to a common count (one sample per `EDGE_SAMPLE_CELLS`, at most `MAX_EDGE_SAMPLES`), position
  and radius = sum of renormalized weight x holder sample at its mapped arc fraction (holders
  summed in source order).
- Weld: the ends of matched groups meeting at one reference node are moved to their centroid,
  the shift fading linearly along each stroke, so junctions stay closed.
- A stroke only one source holds (weight `s`): with both ends attached (their nodes already
  placed) it is mapped by the similarity taking its ends there; with one end attached it is
  translated there and scaled about it by `s`; unattached (an isolated feature) it is scaled
  about its own centroid by `s`. Its radius is scaled by `min(1, 2 s)`. Strokes attached only to
  other single-holder strokes are placed after them (bounded passes), so chains shrink
  together. Strokes therefore grow or shrink from their attachment and never fade.
- Decorations: anchor = weighted mean of each holder's anchor on its placed major; offsets from
  the anchor are blended like positions (single holder: offsets x `s`, radius x `min(1, 2 s)`).
- Caps (free-end shapes) are averaged by weight over the holders that have one at that end
  (direction-independent sides); transformed single-holder strokes keep theirs.
- Output order: matched groups (reference order), single-holder majors (placement order),
  decorations (reference order, then the other sources' unmatched ones in source order).
*/

use std::collections::BTreeMap;

use kurbo::{Point, Vec2};

use super::correspond::{Correspondence, ParamMap};
use super::geometry::{Cap, Stroke, index_f64, sample_count};
use super::graph::Skeleton;
use super::{EDGE_SAMPLE_CELLS, MAX_EDGE_SAMPLES};

/// One stroke of a group: its source, the stroke and how its arc follows the group's.
#[derive(Debug, Clone, Copy)]
struct Holder<'a> {
    source: usize,
    stroke: &'a Stroke,
    map: ParamMap,
}

/// Where a source major ended up: output stroke index and the arc map from the source stroke's
/// fraction to the output stroke's (identity for transformed single-holder strokes).
#[derive(Debug, Clone, Copy)]
struct Placement {
    output: usize,
    /// Arc map output -> source; inverted on lookup.
    map: ParamMap,
}

/// Places every stroke of every source. `weights` are the normalized shape weights, one per
/// skeleton; `cell` the analysis grid cell.
pub(super) fn place(skeletons: &[Skeleton], weights: &[f64], reference: usize, correspondence: &Correspondence, cell: f64) -> Vec<Stroke> {
    let spacing = EDGE_SAMPLE_CELLS * cell;
    let base = &skeletons[reference];
    let mut output: Vec<Stroke> = Vec::new();
    let mut placements: Vec<Vec<Option<Placement>>> = skeletons.iter().map(|skeleton| vec![None; skeleton.majors.len()]).collect();
    // Matched groups, keyed by reference major.
    let mut matched_groups: Vec<(usize, usize)> = Vec::new();
    for (index, reference_major) in base.majors.iter().enumerate() {
        let mut holders = Vec::new();
        for (source, skeleton) in skeletons.iter().enumerate() {
            if source == reference {
                holders.push(Holder { source, stroke: &reference_major.stroke, map: ParamMap::Same });
                continue;
            }
            for (other, pairing) in correspondence.majors[source].iter().enumerate() {
                if let Some(pairing) = pairing
                    && pairing.reference == index
                {
                    holders.push(Holder { source, stroke: &skeleton.majors[other].stroke, map: pairing.map });
                    placements[source][other] = Some(Placement { output: output.len(), map: pairing.map });
                }
            }
        }
        if holders.len() < 2 {
            continue;
        }
        placements[reference][index] = Some(Placement { output: output.len(), map: ParamMap::Same });
        matched_groups.push((index, output.len()));
        output.push(blend_holders(&holders, weights, reference_major.free_loop, spacing));
    }
    weld(&mut output, &matched_groups, base);
    // Node positions per source, from the matched groups' ends.
    let mut nodes: Vec<BTreeMap<usize, Point>> = vec![BTreeMap::new(); skeletons.len()];
    {
        let mut sums: Vec<BTreeMap<usize, (Vec2, f64)>> = vec![BTreeMap::new(); skeletons.len()];
        for (source, skeleton) in skeletons.iter().enumerate() {
            for (major, placement) in skeleton.majors.iter().zip(&placements[source]) {
                let Some(placement) = placement else {
                    continue;
                };
                if major.free_loop {
                    continue;
                }
                let stroke = &output[placement.output];
                // The source stroke's start / end sit at output fraction map^-1(0) / map^-1(1).
                let (at_start, at_end) = match placement.map {
                    ParamMap::Reversed => (last_point(stroke), first_point(stroke)),
                    ParamMap::Same | ParamMap::Cyclic { .. } => (first_point(stroke), last_point(stroke)),
                };
                for (node, point) in [(major.ends[0], at_start), (major.ends[1], at_end)] {
                    let entry = sums[source].entry(node).or_insert((Vec2::ZERO, 0.0));
                    entry.0 += point.to_vec2();
                    entry.1 += 1.0;
                }
            }
        }
        for (source_nodes, source_sums) in nodes.iter_mut().zip(sums) {
            for (node, (sum, count)) in source_sums {
                source_nodes.insert(node, (sum / count).to_point());
            }
        }
    }
    // Single-holder majors: the reference's unmatched ones first, then the other sources'.
    let mut pending: Vec<(usize, usize)> = Vec::new();
    for (source, skeleton) in skeletons.iter().enumerate().filter(|&(source, _)| source == reference) {
        pending.extend((0..skeleton.majors.len()).filter(|&major| placements[source][major].is_none()).map(|major| (source, major)));
    }
    for (source, skeleton) in skeletons.iter().enumerate().filter(|&(source, _)| source != reference) {
        pending.extend((0..skeleton.majors.len()).filter(|&major| placements[source][major].is_none()).map(|major| (source, major)));
    }
    let mut attached_only = true;
    for _ in 0..=pending.len() {
        if pending.is_empty() {
            break;
        }
        let mut progress = false;
        let mut remaining = Vec::with_capacity(pending.len());
        for &(source, major_index) in &pending {
            let major = &skeletons[source].majors[major_index];
            let known: Vec<Option<Point>> =
                major.ends.iter().map(|node| if major.free_loop { None } else { nodes[source].get(node).copied() }).collect();
            if attached_only && known.iter().all(Option::is_none) {
                remaining.push((source, major_index));
                continue;
            }
            progress = true;
            let stroke = transform_single(&major.stroke, known[0], known[1], major.ends[0] == major.ends[1], weights[source]);
            if !major.free_loop {
                nodes[source].entry(major.ends[0]).or_insert_with(|| first_point(&stroke));
                nodes[source].entry(major.ends[1]).or_insert_with(|| last_point(&stroke));
            }
            placements[source][major_index] = Some(Placement { output: output.len(), map: ParamMap::Same });
            output.push(stroke);
        }
        pending = remaining;
        if !progress {
            // Nothing attached is left: the rest are isolated features, scaled in place.
            attached_only = false;
        }
    }
    place_decorations(skeletons, weights, reference, correspondence, &placements, &mut output, spacing);
    output
}

/// First point of a non-empty stroke (origin for an empty one, never produced).
fn first_point(stroke: &Stroke) -> Point {
    stroke.points.first().copied().unwrap_or(Point::ZERO)
}

/// Last point of a non-empty stroke (origin for an empty one, never produced).
fn last_point(stroke: &Stroke) -> Point {
    stroke.points.last().copied().unwrap_or(Point::ZERO)
}

/// Weighted average of the holders sampled at a common count (`free_loop`: closed, sampled
/// around and closed again).
fn blend_holders(holders: &[Holder<'_>], weights: &[f64], free_loop: bool, spacing: f64) -> Stroke {
    let total_weight: f64 = holders.iter().map(|holder| weights[holder.source]).sum();
    let samplers: Vec<_> = holders.iter().map(|holder| holder.stroke.sampler()).collect();
    let longest = samplers.iter().map(super::geometry::Sampler::total).fold(0.0, f64::max);
    let fractions: Vec<f64> = if longest <= 0.0 {
        vec![0.0]
    } else if free_loop {
        let count = sample_count(longest, spacing, 3, MAX_EDGE_SAMPLES - 1);
        (0..count).map(|k| index_f64(k) / index_f64(count)).collect()
    } else {
        let segments = sample_count(longest, spacing, 1, MAX_EDGE_SAMPLES - 1);
        (0..=segments).map(|k| index_f64(k) / index_f64(segments)).collect()
    };
    let mut points = Vec::with_capacity(fractions.len() + 1);
    let mut radii = Vec::with_capacity(fractions.len() + 1);
    for &u in &fractions {
        let mut position = Vec2::ZERO;
        let mut radius = 0.0;
        for (holder, sampler) in holders.iter().zip(&samplers) {
            let share = weights[holder.source] / total_weight;
            let (point, holder_radius) = sampler.at(holder.map.apply(u));
            position += point.to_vec2() * share;
            radius += holder_radius * share;
        }
        points.push(position.to_point());
        radii.push(radius);
    }
    if free_loop && points.len() > 1 {
        points.push(points[0]);
        radii.push(radii[0]);
    }
    let caps = [0_usize, 1].map(|end| {
        blend_caps(holders.iter().map(|holder| {
            let holder_end = match holder.map {
                ParamMap::Same => Some(end),
                ParamMap::Reversed => Some(1 - end),
                ParamMap::Cyclic { .. } => None,
            };
            (holder_end.and_then(|holder_end| holder.stroke.caps[holder_end]), weights[holder.source])
        }))
    });
    Stroke { points, radii, caps }
}

/// Weighted mean of the present caps (`None` when no holder has one at this end).
fn blend_caps(caps: impl Iterator<Item = (Option<Cap>, f64)>) -> Option<Cap> {
    let (mut left, mut right, mut total) = (0.0, 0.0, 0.0);
    for (cap, weight) in caps {
        if let Some(cap) = cap {
            left += cap.left * weight;
            right += cap.right * weight;
            total += weight;
        }
    }
    (total > 0.0).then(|| Cap { left: left / total, right: right / total })
}

/// Moves the ends of matched groups meeting at one reference node to their centroid, the shift
/// fading linearly towards each stroke's other end.
fn weld(output: &mut [Stroke], matched_groups: &[(usize, usize)], base: &Skeleton) {
    let mut sums: BTreeMap<usize, (Vec2, f64)> = BTreeMap::new();
    let ends_of = |major: usize| -> Option<[usize; 2]> {
        let major = &base.majors[major];
        (!major.free_loop && major.stroke.points.len() >= 2).then_some(major.ends)
    };
    for &(major, stroke) in matched_groups {
        let Some(ends) = ends_of(major) else {
            continue;
        };
        for (node, point) in [(ends[0], first_point(&output[stroke])), (ends[1], last_point(&output[stroke]))] {
            let entry = sums.entry(node).or_insert((Vec2::ZERO, 0.0));
            entry.0 += point.to_vec2();
            entry.1 += 1.0;
        }
    }
    for &(major, stroke) in matched_groups {
        let Some(ends) = ends_of(major) else {
            continue;
        };
        let points = &mut output[stroke].points;
        let (Some(&first), Some(&last)) = (points.first(), points.last()) else {
            continue;
        };
        let centroid = |node: usize, fallback: Point| sums.get(&node).map_or(fallback, |(sum, count)| (*sum / *count).to_point());
        let start_shift = centroid(ends[0], first) - first;
        let end_shift = centroid(ends[1], last) - last;
        let span = index_f64(points.len() - 1);
        for (position, point) in points.iter_mut().enumerate() {
            let t = index_f64(position) / span;
            *point += start_shift * (1.0 - t) + end_shift * t;
        }
    }
}

/// A stroke only one source holds, with weight `share`: attached by both ends (`start` / `end`
/// known and distinct, not a loop) -> similarity onto them; by one -> translated there and
/// scaled about it; unattached -> scaled about its centroid. Radius x `min(1, 2 share)`.
fn transform_single(stroke: &Stroke, start: Option<Point>, end: Option<Point>, closed: bool, share: f64) -> Stroke {
    let radius_scale = (2.0 * share).min(1.0);
    let radii = stroke.radii.iter().map(|radius| radius * radius_scale).collect();
    let (Some(&first), Some(&last)) = (stroke.points.first(), stroke.points.last()) else {
        return Stroke { points: Vec::new(), radii, caps: stroke.caps };
    };
    let points = match (start, end) {
        (Some(target_start), Some(target_end)) if !closed && first.distance(last) > 1e-9 => {
            // Complex-number similarity z -> target_start + (z - first) * (target_end - target_start) / (last - first).
            let from = last - first;
            let to = target_end - target_start;
            let denominator = from.hypot2();
            let (cos, sin) = ((to.x * from.x + to.y * from.y) / denominator, (to.y * from.x - to.x * from.y) / denominator);
            stroke
                .points
                .iter()
                .map(|&point| {
                    let offset = point - first;
                    target_start + Vec2::new(offset.x * cos - offset.y * sin, offset.x * sin + offset.y * cos)
                })
                .collect()
        }
        (Some(target), _) => scale_about(&stroke.points, first, target, share),
        (None, Some(target)) => scale_about(&stroke.points, last, target, share),
        (None, None) => {
            let count = index_f64(stroke.points.len());
            let centroid = (stroke.points.iter().fold(Vec2::ZERO, |sum, point| sum + point.to_vec2()) / count).to_point();
            scale_about(&stroke.points, centroid, centroid, share)
        }
    };
    Stroke { points, radii, caps: stroke.caps }
}

/// `points` translated so that `pivot` lands on `target`, then scaled about it by `share`.
fn scale_about(points: &[Point], pivot: Point, target: Point, share: f64) -> Vec<Point> {
    points.iter().map(|&point| target + (point - pivot) * share).collect()
}

/// Places the decorations of every source after the majors.
fn place_decorations(
    skeletons: &[Skeleton],
    weights: &[f64],
    reference: usize,
    correspondence: &Correspondence,
    placements: &[Vec<Option<Placement>>],
    output: &mut Vec<Stroke>,
    spacing: f64,
) {
    // The anchor of decoration `index` of `source` on its placed major.
    let anchor = |output: &[Stroke], source: usize, index: usize| -> Point {
        let decoration = &skeletons[source].decorations[index];
        match placements[source].get(decoration.major).copied().flatten() {
            Some(placement) => {
                let sampler = output[placement.output].sampler();
                sampler.at(invert(placement.map, decoration.u)).0
            }
            None => first_point(&decoration.stroke),
        }
    };
    let base = &skeletons[reference];
    let mut matched = vec![Vec::new(); skeletons.len()];
    for (index, reference_decoration) in base.decorations.iter().enumerate() {
        let mut holders: Vec<(usize, usize)> = Vec::new();
        for (source, pairs) in correspondence.decorations.iter().enumerate() {
            if source == reference {
                holders.push((source, index));
                continue;
            }
            if let Some(other) = pairs.iter().position(|pair| *pair == Some(index)) {
                holders.push((source, other));
            }
        }
        if holders.len() < 2 {
            continue;
        }
        let total_weight: f64 = holders.iter().map(|&(source, _)| weights[source]).sum();
        let mut centre = Vec2::ZERO;
        for &(source, other) in &holders {
            centre += anchor(output, source, other).to_vec2() * (weights[source] / total_weight);
            matched[source].push(other);
        }
        let strokes: Vec<(&Stroke, f64)> =
            holders.iter().map(|&(source, other)| (&skeletons[source].decorations[other].stroke, weights[source] / total_weight)).collect();
        let longest = strokes.iter().map(|(stroke, _)| stroke.length()).fold(reference_decoration.length, f64::max);
        let segments = sample_count(longest, spacing, 1, MAX_EDGE_SAMPLES - 1);
        let samplers: Vec<_> = strokes.iter().map(|(stroke, share)| (stroke.sampler(), first_point(stroke), *share)).collect();
        let mut points = Vec::with_capacity(segments + 1);
        let mut radii = Vec::with_capacity(segments + 1);
        for k in 0..=segments {
            let u = index_f64(k) / index_f64(segments);
            let mut offset = Vec2::ZERO;
            let mut radius = 0.0;
            for (sampler, start, share) in &samplers {
                let (point, holder_radius) = sampler.at(u);
                offset += (point - *start) * *share;
                radius += holder_radius * share;
            }
            points.push(centre.to_point() + offset);
            radii.push(radius);
        }
        let tip = blend_caps(holders.iter().map(|&(source, other)| (skeletons[source].decorations[other].stroke.caps[1], weights[source])));
        output.push(Stroke { points, radii, caps: [None, tip] });
    }
    for (source, skeleton) in skeletons.iter().enumerate() {
        for (index, decoration) in skeleton.decorations.iter().enumerate() {
            if matched[source].contains(&index) {
                continue;
            }
            let target = anchor(output, source, index);
            let share = weights[source];
            let start = first_point(&decoration.stroke);
            let radius_scale = (2.0 * share).min(1.0);
            output.push(Stroke {
                points: scale_about(&decoration.stroke.points, start, target, share),
                radii: decoration.stroke.radii.iter().map(|radius| radius * radius_scale).collect(),
                caps: decoration.stroke.caps,
            });
        }
    }
}

/// The output fraction at which a source stroke's fraction `u` lies, for an output -> source
/// map.
fn invert(map: ParamMap, u: f64) -> f64 {
    match map {
        ParamMap::Same => u,
        ParamMap::Reversed => 1.0 - u,
        ParamMap::Cyclic { reversed, shift } => {
            let value = if reversed { shift - u } else { u - shift };
            value.rem_euclid(1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_undoes_every_map() {
        for map in [
            ParamMap::Same,
            ParamMap::Reversed,
            ParamMap::Cyclic { reversed: false, shift: 0.3 },
            ParamMap::Cyclic { reversed: true, shift: 0.7 },
        ] {
            for step in 0..10 {
                let u = f64::from(step) / 10.0;
                let back = map.apply(invert(map, u));
                assert!((back - u).abs() < 1e-12 || (back - u).abs() > 1.0 - 1e-12, "{map:?} {u} {back}");
            }
        }
    }

    #[test]
    fn single_strokes_grow_from_their_attachment() {
        let stroke = Stroke::new(vec![Point::new(0.0, 0.0), Point::new(0.0, 100.0)], vec![10.0, 10.0]);
        let grown = transform_single(&stroke, Some(Point::new(5.0, 0.0)), None, false, 0.25);
        assert_eq!(grown.points, vec![Point::new(5.0, 0.0), Point::new(5.0, 25.0)]);
        assert_eq!(grown.radii, vec![5.0, 5.0]);
        let bridged = transform_single(&stroke, Some(Point::new(0.0, 0.0)), Some(Point::new(100.0, 0.0)), false, 0.5);
        assert!(bridged.points[1].distance(Point::new(100.0, 0.0)) < 1e-9);
        let isolated = transform_single(&stroke, None, None, false, 0.5);
        assert_eq!(isolated.points, vec![Point::new(0.0, 25.0), Point::new(0.0, 75.0)]);
    }
}
