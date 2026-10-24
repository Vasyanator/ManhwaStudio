/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/graph.rs

Purpose:
One source glyph -> its stroke graph: inside mask, thinning, pixel graph (nodes = junction
clusters and endpoints, edges = pixel chains), spur pruning, decoration split, corner split
and the final smoothed, ridge-centred strokes with their radius profiles and end caps.

Key structures:
- `Skeleton`: the `Major` strokes (graph edges, closed loops, dots) and the `Decoration`s
  (short spurs anchored at an arc fraction of a major stroke).

Key functions:
- `extract`: polygons (aligned) + grid -> `Skeleton` or a `SkeletonFallback`.

Notes:
- Nodes: 8-connected clusters of skeleton pixels whose neighbour count is not 2; the node
  sits at the cluster pixel farthest from the boundary. A junction-free cycle gets a synthetic
  node at its first pixel in scan order (minimum y, then x). A node without edges is a dot.
- Pruning (`PRUNE_PASSES` rounds): a spur (edge from a free end to a junction) is removed when
  its discs reach less than `PRUNE_GAIN_CELLS` beyond the junction's disc, i.e. when it does not
  change the reconstructed shape (boundary noise); corner and flat-terminal spurs reach
  noticeably farther and survive. A junction never loses all its edges in one round.
- Decorations: surviving spurs shorter than `DECORATION_LENGTH_RATIO` x the junction radius
  (corner and terminal forks). Splitting them off keeps a corner present in one design only
  from cutting that design's stroke into edges the other design lacks.
- Degree-2 nodes are merged away after every removal, so major edges run junction to junction;
  then every edge is split at its sharp turns (`CORNER_SPLIT_COS` over a window of
  max(`CORNER_WINDOW_CELLS`, local radius)), so an "L" is a stem and a foot, not one edge.
- Distances are exact distances to the boundary of the nonzero union of the contours
  (`SegmentIndex`; overlapping contours' inner edges excluded).
- Strokes: uniform resampling every `EDGE_SAMPLE_CELLS`, two 1-2-1 smoothing passes (ends
  fixed except on junction-free loops), each interior point moved onto the ridge along its
  normal (ternary search for the farthest-from-boundary point within min(1 cell, r / 2)),
  radius = exact distance to the boundary clamped to `[MIN_RADIUS_CELLS x cell,
  MAX_RADIUS_UNITS]`. A free end (node without other edges) is extended to where the ink
  ends along its tangent (sphere tracing) when that is more than half a cell beyond its disc,
  and its `Cap` measures, per side, how far the ink fills the corner between the round cap
  and a square cut (thinning leaves no corner branches at a flat terminal, so the shape of the
  terminal is carried this way).
- Bounds: raw edges beyond `MAX_RAW_EDGES` and final strokes beyond `MAX_STROKES` are
  `TooManyStrokes`; every walk is bounded by the pixel count, every merge loop by the edge count.
*/

use std::collections::BTreeMap;

use kurbo::{Point, Vec2};

use super::geometry::{Cap, SegmentIndex, Stroke, exceeds, index_f64, sample_count, winding_number};
use super::thin::{neighbour, skeleton_pixels};
use super::{
    DECORATION_LENGTH_RATIO, EDGE_SAMPLE_CELLS, MAX_EDGE_SAMPLES, MAX_RADIUS_UNITS, MAX_STROKES, MIN_RADIUS_CELLS, PRUNE_GAIN_CELLS,
    PRUNE_PASSES, RIDGE_SEARCH_STEPS, SkeletonFallback,
};
use crate::hybrid::sdf::{Grid, winding_inside};

/// Raw pixel-graph edges beyond which a glyph is rejected before any graph work (keeps the
/// quadratic merge loops bounded).
const MAX_RAW_EDGES: usize = 8 * MAX_STROKES;
/// Smoothing passes over resampled stroke points.
const SMOOTHING_PASSES: usize = 2;
/// Corner detection window (at least), in cells.
const CORNER_WINDOW_CELLS: f64 = 4.0;
/// A skeleton turn whose chord cosine is below this (about 50 degrees) is a corner.
const CORNER_SPLIT_COS: f64 = 0.64;
/// Sphere-tracing steps and stop distance (cells) of the free-end extension.
const MARCH_STEPS: usize = 64;
const MARCH_EPSILON_CELLS: f64 = 0.01;
/// Sample positions (0 = round cap's 45-degree point, 1 = square corner) of the cap measure.
const CAP_SAMPLES: [f64; 4] = [0.15, 0.35, 0.55, 0.75];

/// The stroke graph of one source glyph in aligned coordinates.
#[derive(Debug, Clone)]
pub(super) struct Skeleton {
    pub(super) majors: Vec<Major>,
    pub(super) decorations: Vec<Decoration>,
}

/// One major stroke: a graph edge between two nodes, a closed loop, or a dot (one point).
#[derive(Debug, Clone)]
pub(super) struct Major {
    pub(super) stroke: Stroke,
    /// Node ids at the stroke's start and end (equal for loops and dots).
    pub(super) ends: [usize; 2],
    /// A loop whose start point is arbitrary (no junction on it): matched with a cyclic shift.
    pub(super) free_loop: bool,
    /// Polyline length, font units.
    pub(super) length: f64,
}

/// A short spur anchored on a major stroke; its stroke starts at the anchor.
#[derive(Debug, Clone)]
pub(super) struct Decoration {
    /// Index into `Skeleton::majors`.
    pub(super) major: usize,
    /// Arc fraction of the anchor along that major stroke.
    pub(super) u: f64,
    pub(super) stroke: Stroke,
    pub(super) length: f64,
}

/// A pixel-graph node.
#[derive(Debug, Clone)]
struct Node {
    position: Point,
    radius: f64,
    free_loop: bool,
    alive: bool,
}

/// A pixel-graph edge: node ids and the polyline from the first node to the second (both node
/// positions included).
#[derive(Debug, Clone)]
struct Edge {
    ends: [usize; 2],
    points: Vec<Point>,
    alive: bool,
}

/// A decoration before its anchor is resolved: junction position and outward polyline.
#[derive(Debug)]
struct RawDecoration {
    anchor: Point,
    points: Vec<Point>,
}

/// The pixel graph under construction.
#[derive(Debug)]
struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

/// A major stroke before its polyline is built.
#[derive(Debug)]
struct RawMajor {
    points: Vec<Point>,
    ends: [usize; 2],
    free_loop: bool,
    /// Whether each end is a free end (its node has no other edge).
    free: [bool; 2],
}

/// Builds the stroke graph of one source (`polygons` already aligned onto `grid`).
///
/// # Errors
/// `NoSkeleton` when the glyph has no ink on the grid; `TooManyStrokes` when the raw graph or
/// the final stroke set exceeds its bound; `TooComplex` when the outline is too dense to index.
pub(super) fn extract(polygons: &[Vec<Point>], grid: &Grid) -> Result<Skeleton, SkeletonFallback> {
    let inside = winding_inside(polygons, grid);
    if !inside.iter().any(|&flag| flag) {
        return Err(SkeletonFallback::NoSkeleton);
    }
    let index = SegmentIndex::new(polygons, grid).map_err(|_| SkeletonFallback::TooComplex)?;
    let pixels = skeleton_pixels(&inside, grid.nx, grid.ny);
    let mut graph = trace(&pixels, grid, &index)?;
    let threshold = PRUNE_GAIN_CELLS * grid.cell;
    for _ in 0..PRUNE_PASSES {
        if !graph.prune(&index, threshold) {
            break;
        }
    }
    let raw_decorations = graph.split_decorations();
    graph.split_corners(&index, grid.cell);
    let majors_raw = graph.majors();
    let count = majors_raw.len() + raw_decorations.len();
    if count > MAX_STROKES {
        return Err(SkeletonFallback::TooManyStrokes { count });
    }
    if majors_raw.is_empty() {
        return Err(SkeletonFallback::NoSkeleton);
    }
    let shape = Shape { polygons, index: &index, cell: grid.cell };
    let decorations = raw_decorations
        .iter()
        .map(|raw| {
            let (major, u) = anchor_of(raw.anchor, &majors_raw);
            let stroke = build_stroke(&raw.points, false, [false, true], &shape);
            let length = stroke.length();
            Decoration { major, u, stroke, length }
        })
        .collect();
    let majors = majors_raw
        .iter()
        .map(|raw| {
            let stroke = build_stroke(&raw.points, raw.free_loop, raw.free, &shape);
            let length = stroke.length();
            Major { stroke, ends: raw.ends, free_loop: raw.free_loop, length }
        })
        .collect();
    Ok(Skeleton { majors, decorations })
}

/// One source glyph's geometry as the stroke builder needs it.
#[derive(Debug, Clone, Copy)]
struct Shape<'a> {
    polygons: &'a [Vec<Point>],
    index: &'a SegmentIndex,
    cell: f64,
}

/// Centre of raster node `slot`.
fn centre(slot: usize, grid: &Grid) -> Point {
    Point::new(grid.x(slot % grid.nx), grid.y(slot / grid.nx))
}

/// Traces the pixel graph of a skeleton raster.
///
/// # Errors
/// `TooManyStrokes` when it has more than `MAX_RAW_EDGES` edges.
fn trace(pixels: &[bool], grid: &Grid, index: &SegmentIndex) -> Result<Graph, SkeletonFallback> {
    let (nx, ny) = (grid.nx, grid.ny);
    let degree: Vec<u8> = (0..pixels.len())
        .map(|slot| {
            if !pixels[slot] {
                return 0;
            }
            let count = (0..8).filter(|&k| neighbour(slot, k, nx, ny).is_some_and(|other| pixels[other])).count();
            u8::try_from(count).unwrap_or(u8::MAX)
        })
        .collect();
    let is_node = |slot: usize| pixels[slot] && degree[slot] != 2;
    // Node clusters: 8-connected components of node pixels, numbered in scan order.
    let mut cluster = vec![usize::MAX; pixels.len()];
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack = Vec::new();
    for seed in 0..pixels.len() {
        if !is_node(seed) || cluster[seed] != usize::MAX {
            continue;
        }
        let id = nodes.len();
        cluster[seed] = id;
        stack.push(seed);
        let mut best: Option<(f64, usize)> = None;
        while let Some(slot) = stack.pop() {
            let radius = index.distance(centre(slot, grid));
            // Farthest from the boundary wins; equal distances go to the earlier slot.
            if best.is_none_or(|(best_radius, best_slot)| radius.total_cmp(&best_radius).then(best_slot.cmp(&slot)) == std::cmp::Ordering::Greater) {
                best = Some((radius, slot));
            }
            for k in 0..8 {
                if let Some(other) = neighbour(slot, k, nx, ny)
                    && is_node(other)
                    && cluster[other] == usize::MAX
                {
                    cluster[other] = id;
                    stack.push(other);
                }
            }
        }
        // The seed itself was visited, so `best` is set.
        let (radius, slot) = best.unwrap_or((0.0, seed));
        nodes.push(Node { position: centre(slot, grid), radius, free_loop: false, alive: true });
    }
    let mut edges: Vec<Edge> = Vec::new();
    let mut visited = vec![false; pixels.len()];
    for start in 0..pixels.len() {
        if !is_node(start) {
            continue;
        }
        for k in 0..8 {
            let Some(first) = neighbour(start, k, nx, ny) else {
                continue;
            };
            if !pixels[first] || is_node(first) || visited[first] {
                continue;
            }
            let (chain, end) = walk(start, first, pixels, &is_node, &mut visited, nx, ny);
            let from = cluster[start];
            let to = end.map_or(from, |slot| cluster[slot]);
            if from == to && chain.len() < 3 {
                continue;
            }
            let mut points = Vec::with_capacity(chain.len() + 2);
            points.push(nodes[from].position);
            points.extend(chain.iter().map(|&slot| centre(slot, grid)));
            points.push(nodes[to].position);
            edges.push(Edge { ends: [from, to], points, alive: true });
            if edges.len() > MAX_RAW_EDGES {
                return Err(SkeletonFallback::TooManyStrokes { count: edges.len() });
            }
        }
    }
    let mut graph = Graph { nodes, edges };
    trace_cycles(pixels, grid, index, &is_node, &mut visited, &mut graph)?;
    Ok(graph)
}

/// Adds every junction-free cycle (all remaining unvisited path pixels lie on one) as a free
/// loop with a node at its first pixel in scan order.
///
/// # Errors
/// `TooManyStrokes` when the graph exceeds `MAX_RAW_EDGES` edges.
fn trace_cycles(
    pixels: &[bool],
    grid: &Grid,
    index: &SegmentIndex,
    is_node: &impl Fn(usize) -> bool,
    visited: &mut [bool],
    graph: &mut Graph,
) -> Result<(), SkeletonFallback> {
    let (nx, ny) = (grid.nx, grid.ny);
    for start in 0..pixels.len() {
        if !pixels[start] || is_node(start) || visited[start] {
            continue;
        }
        let id = graph.nodes.len();
        let position = centre(start, grid);
        graph.nodes.push(Node { position, radius: index.distance(position), free_loop: true, alive: true });
        visited[start] = true;
        let mut points = vec![position];
        let mut previous = start;
        let mut current = (0..8).filter_map(|k| neighbour(start, k, nx, ny)).find(|&other| pixels[other]);
        for _ in 0..pixels.len() {
            let Some(slot) = current else {
                break;
            };
            if slot == start {
                break;
            }
            visited[slot] = true;
            points.push(centre(slot, grid));
            let next = (0..8).filter_map(|k| neighbour(slot, k, nx, ny)).find(|&other| pixels[other] && other != previous);
            previous = slot;
            current = next;
        }
        points.push(position);
        if points.len() >= 4 {
            graph.edges.push(Edge { ends: [id, id], points, alive: true });
            if graph.edges.len() > MAX_RAW_EDGES {
                return Err(SkeletonFallback::TooManyStrokes { count: graph.edges.len() });
            }
        }
    }
    Ok(())
}

/// Walks a chain of path pixels from node pixel `start` through `first` until a node pixel.
/// Returns the chain and the node pixel reached (`None` for a chain that stopped early, which
/// only a raster violating the path-pixel invariant can produce).
fn walk(
    start: usize,
    first: usize,
    pixels: &[bool],
    is_node: &impl Fn(usize) -> bool,
    visited: &mut [bool],
    nx: usize,
    ny: usize,
) -> (Vec<usize>, Option<usize>) {
    let mut chain = Vec::new();
    let (mut previous, mut current) = (start, first);
    for _ in 0..pixels.len() {
        visited[current] = true;
        chain.push(current);
        // A path pixel has exactly two neighbours: `previous` and the next one.
        let Some(next) = (0..8).filter_map(|k| neighbour(current, k, nx, ny)).find(|&other| pixels[other] && other != previous) else {
            return (chain, None);
        };
        if is_node(next) {
            return (chain, Some(next));
        }
        if visited[next] {
            return (chain, None);
        }
        previous = current;
        current = next;
    }
    (chain, None)
}

impl Graph {
    /// Edge-end count per node over the alive edges (a self-loop counts twice).
    fn degrees(&self) -> Vec<usize> {
        let mut degrees = vec![0_usize; self.nodes.len()];
        for edge in self.edges.iter().filter(|edge| edge.alive) {
            degrees[edge.ends[0]] += 1;
            degrees[edge.ends[1]] += 1;
        }
        degrees
    }

    /// Alive spurs as `(edge, junction node)`: edges from a free end (degree 1) to a junction
    /// (degree >= 3).
    fn spurs(&self, degrees: &[usize]) -> Vec<(usize, usize)> {
        let mut spurs = Vec::new();
        for (id, edge) in self.edges.iter().enumerate().filter(|(_, edge)| edge.alive) {
            let [a, b] = edge.ends;
            if a == b {
                continue;
            }
            if degrees[a] == 1 && degrees[b] >= 3 {
                spurs.push((id, b));
            } else if degrees[b] == 1 && degrees[a] >= 3 {
                spurs.push((id, a));
            }
        }
        spurs
    }

    /// Picks, per junction, the candidate spurs to remove, keeping the best one when all of a
    /// junction's edges are candidates. `candidates` are `(edge, junction, score)`; the kept
    /// one has the highest score (earliest on ties).
    fn removable(candidates: &[(usize, usize, f64)], degrees: &[usize]) -> Vec<usize> {
        let mut by_junction: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
        for &(edge, junction, score) in candidates {
            by_junction.entry(junction).or_default().push((edge, score));
        }
        let mut removals = Vec::new();
        for (junction, spurs) in by_junction {
            let keep = if spurs.len() >= degrees[junction] {
                let mut best = 0;
                for (position, &(_, score)) in spurs.iter().enumerate().skip(1) {
                    if score > spurs[best].1 {
                        best = position;
                    }
                }
                Some(best)
            } else {
                None
            };
            removals.extend(spurs.iter().enumerate().filter(|&(position, _)| Some(position) != keep).map(|(_, &(edge, _))| edge));
        }
        removals.sort_unstable();
        removals
    }

    /// Removes edge `id` and the free-end node it leaves behind.
    fn remove_spur(&mut self, id: usize, junction: usize) {
        let edge = &mut self.edges[id];
        edge.alive = false;
        let free = if edge.ends[0] == junction { edge.ends[1] } else { edge.ends[0] };
        self.nodes[free].alive = false;
    }

    /// One pruning round; returns whether anything was removed.
    fn prune(&mut self, index: &SegmentIndex, threshold: f64) -> bool {
        let degrees = self.degrees();
        let mut candidates = Vec::new();
        for (edge, junction) in self.spurs(&degrees) {
            let node = &self.nodes[junction];
            let points = &self.edges[edge].points;
            // Cheap bound first: a spur reaching beyond the junction disc by the threshold even
            // without its own radius is kept without distance queries.
            if points.iter().any(|point| point.distance(node.position) >= node.radius + threshold) {
                continue;
            }
            let reach = points.iter().map(|&point| point.distance(node.position) + index.distance(point)).fold(0.0, f64::max);
            let gain = reach - node.radius;
            if gain < threshold {
                candidates.push((edge, junction, gain));
            }
        }
        let removals = Self::removable(&candidates, &degrees);
        if removals.is_empty() {
            return false;
        }
        for edge in removals {
            let junction = candidates.iter().find(|candidate| candidate.0 == edge).map_or(self.edges[edge].ends[0], |candidate| candidate.1);
            self.remove_spur(edge, junction);
        }
        self.merge_degree_two();
        true
    }

    /// Splits off the short spurs (decorations), oriented from their junction outwards.
    fn split_decorations(&mut self) -> Vec<RawDecoration> {
        let degrees = self.degrees();
        let candidates: Vec<(usize, usize, f64)> = self
            .spurs(&degrees)
            .into_iter()
            .filter_map(|(edge, junction)| {
                let length = polyline_length(&self.edges[edge].points);
                (length < DECORATION_LENGTH_RATIO * self.nodes[junction].radius).then_some((edge, junction, length))
            })
            .collect();
        let removals = Self::removable(&candidates, &degrees);
        let mut decorations = Vec::with_capacity(removals.len());
        for edge in removals {
            let Some(&(_, junction, _)) = candidates.iter().find(|candidate| candidate.0 == edge) else {
                continue;
            };
            let mut points = self.edges[edge].points.clone();
            if self.edges[edge].ends[0] != junction {
                points.reverse();
            }
            decorations.push(RawDecoration { anchor: self.nodes[junction].position, points });
            self.remove_spur(edge, junction);
        }
        self.merge_degree_two();
        decorations
    }

    /// Joins the two edges of every degree-2 node until none is left; a node whose only edge
    /// is a self-loop becomes that loop's free start.
    fn merge_degree_two(&mut self) {
        for _ in 0..=self.edges.len() {
            let degrees = self.degrees();
            let mut merged = false;
            for (node, &degree) in degrees.iter().enumerate() {
                if !self.nodes[node].alive || degree != 2 {
                    continue;
                }
                let incident: Vec<usize> =
                    (0..self.edges.len()).filter(|&id| self.edges[id].alive && self.edges[id].ends.contains(&node)).collect();
                match incident.as_slice() {
                    [single] => {
                        // A self-loop alone at its node: the loop has no junction any more.
                        if self.edges[*single].ends == [node, node] {
                            self.nodes[node].free_loop = true;
                        }
                    }
                    [first, second] => {
                        let (first, second) = (*first, *second);
                        let mut head = self.edges[first].points.clone();
                        let mut head_other = self.edges[first].ends[0];
                        if self.edges[first].ends[0] == node {
                            head.reverse();
                            head_other = self.edges[first].ends[1];
                        }
                        let mut tail = self.edges[second].points.clone();
                        let mut tail_other = self.edges[second].ends[1];
                        if self.edges[second].ends[1] == node {
                            tail.reverse();
                            tail_other = self.edges[second].ends[0];
                        }
                        head.extend(tail.into_iter().skip(1));
                        self.edges[first] = Edge { ends: [head_other, tail_other], points: head, alive: true };
                        self.edges[second].alive = false;
                        self.nodes[node].alive = false;
                        merged = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !merged {
                return;
            }
        }
    }

    /// Splits every edge at its sharp skeleton turns, adding a node at each corner: a stroke
    /// that turns a corner in one design is two strokes there, so that each part can pair
    /// with a stroke of a design that lacks the other part.
    fn split_corners(&mut self, index: &SegmentIndex, cell: f64) {
        for id in 0..self.edges.len() {
            if !self.edges[id].alive {
                continue;
            }
            let edge = self.edges[id].clone();
            let free_loop = edge.ends[0] == edge.ends[1] && self.nodes[edge.ends[0]].free_loop;
            let mut corners = corner_positions(&edge.points, free_loop, index, cell);
            let Some(&first) = corners.first() else {
                continue;
            };
            let mut points = edge.points;
            let (start, end) = if free_loop {
                // Rotate the loop to start at its first corner; the arbitrary start disappears.
                let cycle = &points[..points.len() - 1];
                let mut rotated: Vec<Point> = cycle[first..].iter().chain(&cycle[..first]).copied().collect();
                rotated.push(rotated[0]);
                points = rotated;
                corners = corners[1..].iter().map(|&corner| corner - first).collect();
                self.nodes[edge.ends[0]].alive = false;
                let node = self.add_node(points[0], index);
                (node, node)
            } else {
                (edge.ends[0], edge.ends[1])
            };
            let mut pieces = Vec::with_capacity(corners.len() + 1);
            let (mut previous_node, mut previous_position) = (start, 0);
            for &corner in &corners {
                let node = self.add_node(points[corner], index);
                pieces.push(Edge { ends: [previous_node, node], points: points[previous_position..=corner].to_vec(), alive: true });
                previous_node = node;
                previous_position = corner;
            }
            pieces.push(Edge { ends: [previous_node, end], points: points[previous_position..].to_vec(), alive: true });
            let mut pieces = pieces.into_iter();
            if let Some(piece) = pieces.next() {
                self.edges[id] = piece;
            }
            self.edges.extend(pieces);
        }
    }

    /// Adds a (non-loop) node at `position`.
    fn add_node(&mut self, position: Point, index: &SegmentIndex) -> usize {
        self.nodes.push(Node { position, radius: index.distance(position), free_loop: false, alive: true });
        self.nodes.len() - 1
    }

    /// The major strokes: alive edges in order, then dots (alive nodes without edges).
    fn majors(&self) -> Vec<RawMajor> {
        let degrees = self.degrees();
        let mut majors: Vec<RawMajor> = self
            .edges
            .iter()
            .filter(|edge| edge.alive)
            .map(|edge| {
                let free_loop = edge.ends[0] == edge.ends[1] && self.nodes[edge.ends[0]].free_loop;
                let free = [degrees[edge.ends[0]] == 1, degrees[edge.ends[1]] == 1];
                RawMajor { points: edge.points.clone(), ends: edge.ends, free_loop, free }
            })
            .collect();
        for (id, node) in self.nodes.iter().enumerate() {
            if node.alive && degrees[id] == 0 {
                majors.push(RawMajor { points: vec![node.position], ends: [id, id], free_loop: false, free: [false, false] });
            }
        }
        majors
    }
}

/// Positions (indices into `points`) of the sharp turns of a raw pixel polyline: the turn
/// between the chords to the points `max(CORNER_WINDOW_CELLS cells, local radius)` of arc length
/// behind and ahead has a cosine below `CORNER_SPLIT_COS`; one position (the sharpest, first on
/// ties) per run of consecutive candidates. Positions closer than the window to an open
/// polyline's ends are ignored (junction regions turn by construction). `cyclic`: a free loop
/// (first point == last point), whose window wraps around.
fn corner_positions(points: &[Point], cyclic: bool, index: &SegmentIndex, cell: f64) -> Vec<usize> {
    if points.len() < 5 {
        return Vec::new();
    }
    let stroke = Stroke::new(points.to_vec(), vec![0.0; points.len()]);
    let sampler = stroke.sampler();
    let total = sampler.total();
    if !exceeds(total, 0.0) {
        return Vec::new();
    }
    let last = points.len() - 1;
    // Runs of consecutive candidates: (first position, last position, best cosine, its position).
    let mut runs: Vec<(usize, usize, f64, usize)> = Vec::new();
    let mut arc = 0.0;
    for position in 0..last {
        if position > 0 {
            arc += points[position - 1].distance(points[position]);
        }
        let window = (CORNER_WINDOW_CELLS * cell).max(index.distance(points[position]));
        let (back, ahead) = if cyclic {
            if 2.0 * window >= total {
                continue;
            }
            ((arc - window).rem_euclid(total), (arc + window).rem_euclid(total))
        } else {
            if arc < window || total - arc < window {
                continue;
            }
            (arc - window, arc + window)
        };
        let before = points[position] - sampler.at(back / total).0;
        let after = sampler.at(ahead / total).0 - points[position];
        let lengths = before.hypot() * after.hypot();
        if !exceeds(lengths, 0.0) {
            continue;
        }
        let cos = before.dot(after) / lengths;
        if cos >= CORNER_SPLIT_COS {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.1 + 1 == position => {
                run.1 = position;
                if cos < run.2 {
                    run.2 = cos;
                    run.3 = position;
                }
            }
            _ => runs.push((position, position, cos, position)),
        }
    }
    if cyclic
        && runs.len() >= 2
        && let (Some(&first), Some(&final_run)) = (runs.first(), runs.last())
        && first.0 == 0
        && final_run.1 + 1 == last
    {
        // A run crossing the loop's start is one corner.
        runs.pop();
        if final_run.2 < first.2 {
            runs[0].2 = final_run.2;
            runs[0].3 = final_run.3;
        }
    }
    let mut corners: Vec<usize> = runs.iter().map(|run| run.3).filter(|&position| cyclic || (position > 0 && position < last)).collect();
    corners.sort_unstable();
    corners
}

/// Length of a polyline.
fn polyline_length(points: &[Point]) -> f64 {
    points.windows(2).map(|pair| pair[0].distance(pair[1])).sum()
}

/// The major stroke and arc fraction nearest to `anchor` (first major / first point on ties).
fn anchor_of(anchor: Point, majors: &[RawMajor]) -> (usize, f64) {
    let mut best = (f64::INFINITY, 0_usize, 0_usize);
    for (major, raw) in majors.iter().enumerate() {
        for (position, point) in raw.points.iter().enumerate() {
            let distance = point.distance(anchor);
            if distance < best.0 {
                best = (distance, major, position);
            }
        }
    }
    let (_, major, position) = best;
    let points = &majors[major].points;
    let total = polyline_length(points);
    let before = polyline_length(&points[..=position.min(points.len().saturating_sub(1))]);
    (major, if total > 0.0 { before / total } else { 0.0 })
}

/// Resampled, smoothed, ridge-centred stroke with radii and end caps from a raw polyline.
/// `cyclic` strokes (free loops, first point == last point) are smoothed and centred all
/// around; `free` ends are extended to the stroke's real end and get a measured `Cap`.
fn build_stroke(raw: &[Point], cyclic: bool, free: [bool; 2], shape: &Shape<'_>) -> Stroke {
    let cell = shape.cell;
    let clamp_radius = |radius: f64| radius.clamp(MIN_RADIUS_CELLS * cell, MAX_RADIUS_UNITS.max(MIN_RADIUS_CELLS * cell));
    let raw_stroke = Stroke::new(raw.to_vec(), vec![0.0; raw.len()]);
    let sampler = raw_stroke.sampler();
    let total = sampler.total();
    if raw.len() < 2 || total <= 0.0 {
        let point = raw.first().copied().unwrap_or(Point::ZERO);
        return Stroke::new(vec![point], vec![clamp_radius(shape.index.distance(point))]);
    }
    let segments = sample_count(total, EDGE_SAMPLE_CELLS * cell, if cyclic { 3 } else { 1 }, MAX_EDGE_SAMPLES - 1);
    let mut points: Vec<Point> = (0..=segments).map(|k| sampler.at(index_f64(k) / index_f64(segments)).0).collect();
    for _ in 0..SMOOTHING_PASSES {
        points = smooth(&points, cyclic);
    }
    let last = points.len() - 1;
    // Open strokes keep their end points (nodes); a free loop's closing point follows its first.
    let first_movable = usize::from(!cyclic);
    for position in first_movable..last {
        let before = if position == 0 { points[last - 1] } else { points[position - 1] };
        let after = points[position + 1];
        points[position] = centre_on_ridge(points[position], after - before, shape.index, cell);
    }
    if cyclic {
        points[last] = points[0];
    }
    let mut radii: Vec<f64> = points.iter().map(|&point| clamp_radius(shape.index.distance(point))).collect();
    let mut caps = [None, None];
    if !cyclic {
        for end in [1_usize, 0] {
            if !free[end] {
                continue;
            }
            let tip_slot = if end == 0 { 0 } else { points.len() - 1 };
            let tip = points[tip_slot];
            let radius = radii[tip_slot];
            let Some(outward) = outward_tangent(&points, end, cell) else {
                continue;
            };
            // Extend the stroke to the cap centre when the ink reaches farther than the disc.
            let extension = march_to_boundary(tip, outward, radius, shape.index, cell);
            let centre = if extension - radius > 0.5 * cell {
                let centre = tip + outward * (extension - radius);
                if end == 0 {
                    points.insert(0, centre);
                    radii.insert(0, radius);
                } else {
                    points.push(centre);
                    radii.push(radius);
                }
                centre
            } else {
                tip
            };
            caps[end] = Some(measure_cap(centre, outward, radius, shape.polygons));
        }
    }
    Stroke { points, radii, caps }
}

/// Unit tangent pointing out of the stroke through end `end` (0 = first point), measured over
/// at least two cells of arc; `None` for a degenerate stroke.
fn outward_tangent(points: &[Point], end: usize, cell: f64) -> Option<Vec2> {
    let ordered: Vec<Point> = if end == 0 { points.to_vec() } else { points.iter().rev().copied().collect() };
    let tip = *ordered.first()?;
    let inner = ordered.iter().skip(1).find(|point| point.distance(tip) >= 2.0 * cell).or_else(|| ordered.last())?;
    let direction = tip - *inner;
    let length = direction.hypot();
    (length > 0.0).then(|| direction / length)
}

/// Distance from `start` (inside the glyph) along `direction` to the boundary, by sphere
/// tracing on exact distances; at most `4 radius + 4 cells`.
fn march_to_boundary(start: Point, direction: Vec2, radius: f64, index: &SegmentIndex, cell: f64) -> f64 {
    let limit = 4.0 * radius + 4.0 * cell;
    let mut travelled = 0.0;
    for _ in 0..MARCH_STEPS {
        let step = index.distance(start + direction * travelled);
        if !exceeds(step, MARCH_EPSILON_CELLS * cell) {
            break;
        }
        travelled += step;
        if travelled >= limit {
            return limit;
        }
    }
    travelled
}

/// How square each side of a free end at `centre` (outward tangent `outward`, radius `radius`)
/// is: the share of `CAP_SAMPLES` points between the round cap's 45-degree point and the square
/// corner that lie inside the glyph.
fn measure_cap(centre: Point, outward: Vec2, radius: f64, polygons: &[Vec<Point>]) -> Cap {
    let left_normal = Vec2::new(-outward.y, outward.x);
    let side = |normal: Vec2| -> f64 {
        let round = centre + (outward + normal) * (radius * std::f64::consts::FRAC_1_SQRT_2);
        let square = centre + (outward + normal) * radius;
        let inside = CAP_SAMPLES.iter().filter(|&&lambda| winding_contains(polygons, round.lerp(square, lambda))).count();
        index_f64(inside) / index_f64(CAP_SAMPLES.len())
    };
    Cap { left: side(left_normal), right: side(-left_normal) }
}

/// Nonzero-winding inside test of a point against closed polygons.
fn winding_contains(polygons: &[Vec<Point>], point: Point) -> bool {
    polygons.iter().map(|polygon| winding_number(polygon, point)).sum::<i32>() != 0
}

/// One 1-2-1 smoothing pass (ends fixed unless `cyclic`, where the closing point follows the
/// first).
fn smooth(points: &[Point], cyclic: bool) -> Vec<Point> {
    let last = points.len() - 1;
    let mut smoothed = points.to_vec();
    for position in 0..points.len() {
        let (before, after) = match (position, cyclic) {
            (0, false) => continue,
            (0, true) => (points[last - 1], points[1]),
            (p, _) if p == last => continue,
            (p, _) => (points[p - 1], points[p + 1]),
        };
        smoothed[position] = Point::new(0.25 * before.x + 0.5 * points[position].x + 0.25 * after.x, 0.25 * before.y + 0.5 * points[position].y + 0.25 * after.y);
    }
    if cyclic {
        smoothed[last] = smoothed[0];
    }
    smoothed
}

/// `point` moved along the normal of `tangent` to the farthest-from-boundary position within
/// `min(cell, r / 2)` (ternary search; kept only when it is farther than the start).
fn centre_on_ridge(point: Point, tangent: Vec2, index: &SegmentIndex, cell: f64) -> Point {
    let length = tangent.hypot();
    if !exceeds(length, 0.0) {
        return point;
    }
    let normal = Vec2::new(-tangent.y / length, tangent.x / length);
    let start = index.distance(point);
    let reach = cell.min(0.5 * start);
    if !exceeds(reach, 0.0) {
        return point;
    }
    let (mut low, mut high) = (-reach, reach);
    for _ in 0..RIDGE_SEARCH_STEPS {
        let first = low + (high - low) / 3.0;
        let second = high - (high - low) / 3.0;
        if index.distance(point + normal * first) < index.distance(point + normal * second) {
            low = first;
        } else {
            high = second;
        }
    }
    let candidate = point + normal * f64::midpoint(low, high);
    if index.distance(candidate) > start { candidate } else { point }
}
