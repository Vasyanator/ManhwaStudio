/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/harmonize.rs

Purpose:
Makes the corner splits of two designs agree before strokes are matched: where one design's
skeleton turns a corner (a corner node from `graph`'s corner split) on top of a stroke that the
other design draws as one smooth edge (its corner is rounded or carries a shoulder), that edge
is split there too, so both designs offer the same stroke pieces to the matcher.

Key functions:
- `harmonize`: splits the reference's majors at every other source's corner nodes, then every
  other source's majors at the reference's corner nodes.

Notes:
- Corner node: a node shared by exactly two edge ends of two different majors (a junction has
  three or more, a free end one).
- A stroke is split at the sample nearest to a foreign corner node when that sample lies within
  `SNAP_DIAGONALS` x the ink-box diagonal and at least `END_MARGIN_CELLS` cells (and
  `END_MARGIN_SHARE` of the stroke length) of arc away from both ends; a corner at a stroke's
  end is already a node in both designs. At most `MAX_SPLITS_PER_SOURCE` splits per source
  (bounded work, the stroke bound of `graph` still applies to matching).
- Decorations anchored on a split stroke move to the piece holding their anchor.
*/

use std::collections::BTreeMap;

use kurbo::Point;

use super::geometry::Stroke;
use super::graph::{Major, Skeleton};

/// A foreign corner farther than this (in ink-box diagonals) from a stroke does not split it.
const SNAP_DIAGONALS: f64 = 0.05;
/// Splits keep at least this many cells of arc to each end of the stroke ...
const END_MARGIN_CELLS: f64 = 4.0;
/// ... and at least this share of its length.
const END_MARGIN_SHARE: f64 = 0.1;
/// Most splits added to one source.
const MAX_SPLITS_PER_SOURCE: usize = 64;

/// Harmonizes the corner splits of every source with the reference's (see the file header).
pub(super) fn harmonize(skeletons: &mut [Skeleton], reference: usize, diagonal: f64, cell: f64) {
    let foreign: Vec<Point> =
        skeletons.iter().enumerate().filter(|&(source, _)| source != reference).flat_map(|(_, skeleton)| corner_nodes(skeleton)).collect();
    split_at(&mut skeletons[reference], &foreign, diagonal, cell);
    let reference_corners = corner_nodes(&skeletons[reference]);
    for (source, skeleton) in skeletons.iter_mut().enumerate() {
        if source != reference {
            split_at(skeleton, &reference_corners, diagonal, cell);
        }
    }
}

/// Positions of a skeleton's corner nodes, in node id order.
fn corner_nodes(skeleton: &Skeleton) -> Vec<Point> {
    // node -> (edge-end count, distinct majors, position)
    let mut nodes: BTreeMap<usize, (usize, Vec<usize>, Point)> = BTreeMap::new();
    for (index, major) in skeleton.majors.iter().enumerate() {
        if major.free_loop || major.stroke.points.len() < 2 {
            continue;
        }
        let ends = [major.stroke.points.first().copied(), major.stroke.points.last().copied()];
        for (node, point) in major.ends.iter().zip(ends) {
            let Some(point) = point else {
                continue;
            };
            let entry = nodes.entry(*node).or_insert((0, Vec::new(), point));
            entry.0 += 1;
            if !entry.1.contains(&index) {
                entry.1.push(index);
            }
        }
    }
    nodes.values().filter(|(count, majors, _)| *count == 2 && majors.len() == 2).map(|(_, _, point)| *point).collect()
}

/// Splits the skeleton's majors at the samples nearest to `corners` (see the file header).
fn split_at(skeleton: &mut Skeleton, corners: &[Point], diagonal: f64, cell: f64) {
    let mut next_node = skeleton.majors.iter().flat_map(|major| major.ends).max().map_or(0, |id| id + 1);
    let mut splits = 0;
    for &corner in corners {
        if splits >= MAX_SPLITS_PER_SOURCE {
            return;
        }
        // Nearest sample over all majors (first on ties).
        let mut best: Option<(f64, usize, usize)> = None;
        for (index, major) in skeleton.majors.iter().enumerate() {
            for (position, point) in major.stroke.points.iter().enumerate() {
                let distance = point.distance(corner);
                if best.is_none_or(|(best_distance, _, _)| distance < best_distance) {
                    best = Some((distance, index, position));
                }
            }
        }
        let Some((distance, index, position)) = best else {
            return;
        };
        if distance > SNAP_DIAGONALS * diagonal {
            continue;
        }
        let major = &skeleton.majors[index];
        let arc: f64 = major.stroke.points[..=position].windows(2).map(|pair| pair[0].distance(pair[1])).sum();
        let margin = (END_MARGIN_CELLS * cell).max(END_MARGIN_SHARE * major.length);
        if !major.free_loop && (arc < margin || major.length - arc < margin) {
            continue;
        }
        let u = if major.length > 0.0 { arc / major.length } else { 0.0 };
        split_major(skeleton, index, position, u, next_node);
        next_node += 1;
        splits += 1;
    }
}

/// Splits major `index` at sample `position` (arc fraction `u`) with a new node `node`; a free
/// loop becomes a loop closed at the new node instead.
fn split_major(skeleton: &mut Skeleton, index: usize, position: usize, u: f64, node: usize) {
    let major = skeleton.majors[index].clone();
    let stroke = &major.stroke;
    if major.free_loop {
        // Points: p0 .. p_last (== p0). Rotate the open cycle to start at `position`.
        let count = stroke.points.len() - 1;
        let start = position % count;
        let mut points: Vec<Point> = stroke.points[start..count].iter().chain(&stroke.points[..start]).copied().collect();
        let mut radii: Vec<f64> = stroke.radii[start..count].iter().chain(&stroke.radii[..start]).copied().collect();
        points.push(points[0]);
        radii.push(radii[0]);
        let length = Stroke::new(points.clone(), radii.clone()).length();
        skeleton.majors[index] = Major { stroke: Stroke::new(points, radii), ends: [node, node], free_loop: false, length };
        for decoration in skeleton.decorations.iter_mut().filter(|decoration| decoration.major == index) {
            decoration.u = (decoration.u - u).rem_euclid(1.0);
        }
        return;
    }
    let head = Stroke { points: stroke.points[..=position].to_vec(), radii: stroke.radii[..=position].to_vec(), caps: [stroke.caps[0], None] };
    let tail = Stroke { points: stroke.points[position..].to_vec(), radii: stroke.radii[position..].to_vec(), caps: [None, stroke.caps[1]] };
    let (head_length, tail_length) = (head.length(), tail.length());
    let new_index = skeleton.majors.len();
    skeleton.majors[index] = Major { stroke: head, ends: [major.ends[0], node], free_loop: false, length: head_length };
    skeleton.majors.push(Major { stroke: tail, ends: [node, major.ends[1]], free_loop: false, length: tail_length });
    for decoration in skeleton.decorations.iter_mut().filter(|decoration| decoration.major == index) {
        if decoration.u <= u && u > 0.0 {
            decoration.u = (decoration.u / u).min(1.0);
        } else {
            decoration.major = new_index;
            decoration.u = if u < 1.0 { ((decoration.u - u) / (1.0 - u)).clamp(0.0, 1.0) } else { 0.0 };
        }
    }
}
