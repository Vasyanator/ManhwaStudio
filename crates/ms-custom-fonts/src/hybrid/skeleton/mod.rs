/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/mod.rs

Purpose:
The stroke-skeleton outline blend of the `Skeleton` algorithm: thin each source glyph to its
stroke graph, match strokes between the sources, blend matched strokes (strokes one design
lacks grow or shrink from their attachment instead of fading) and rebuild the outline from the
blended strokes.

Key structures:
- `SkeletonFallback`: why a glyph cannot be blended by skeleton; the dispatch
  (`hybrid::blend_glyph`) then blends that glyph by distance field and counts the reason.

Key functions:
- `blend`: inked source outlines + normalized shape weights + advances -> blended outline.

Notes:
- Pipeline per glyph: alignment exactly as the distance-field path (`sdf::choose_alignment`),
  one analysis grid over the aligned union (`SKELETON_CELL_UNITS`, coarsened by `sdf::grid_for`
  for huge glyphs) -> per source `graph::extract` (mask, thinning, graph, pruning, decorations,
  corner splits, strokes with radii and end caps) -> `harmonize::harmonize` (corner splits
  made to agree) -> `correspond::correspond` against the reference (highest shape weight,
  earliest on ties) -> `place::place` -> `rebuild::rebuild` (capsule-union field, marching
  squares, curve fit). Detail in `MODULE_README.md`.
- Constants below and in the submodules are output semantics of the `Skeleton` algorithm (they never affect `Auto`,
  `Sdf` or `NoBlend` output); changing one changes `Skeleton` output bytes and requires an
  `ENGINE_VERSION` bump.
- Pure per glyph: no cross-glyph state, fixed scan and tie-break orders, no hashing, so the
  output does not depend on thread count. Every loop has a fixed bound; hostile input yields a
  typed fallback, never a panic.
*/

mod correspond;
mod geometry;
mod graph;
mod harmonize;
mod place;
mod rebuild;
#[cfg(test)]
mod tests;
mod thin;

use std::fmt;

use kurbo::Point;

use crate::hybrid::sdf::{self, ExtentTooLarge};
use crate::outline::Outline;

/// Most flattened points of one source outline: 2x the largest glyph of every bundled
/// `fonts/ui` face (8302) and half the distance-field budget, so the skeleton analysis (whose
/// overlap classification is superlinear in points x contours) stays cheap.
const MAX_SKELETON_POINTS: usize = 1 << 14;
/// Most contours of one source outline (the bundled faces peak at 200).
const MAX_SKELETON_CONTOURS: usize = 4 * MAX_STROKES;
/// Analysis and reconstruction grid cell, font units (1000 per em).
const SKELETON_CELL_UNITS: f64 = 4.0;
/// Most strokes (major edges + decorations) one source glyph may have.
const MAX_STROKES: usize = 128;
/// Most capsule segments of one reconstruction.
const MAX_TOTAL_SEGMENTS: usize = 8192;
/// Stroke sample spacing, in cells.
const EDGE_SAMPLE_CELLS: f64 = 2.0;
/// Most samples of one stroke.
const MAX_EDGE_SAMPLES: usize = 256;
/// A spur whose discs reach less than this many cells beyond its junction's disc is pruned.
const PRUNE_GAIN_CELLS: f64 = 1.0;
/// Pruning rounds.
const PRUNE_PASSES: usize = 3;
/// A surviving spur shorter than this multiple of its junction's radius is a decoration.
const DECORATION_LENGTH_RATIO: f64 = 2.0;
/// Pairs costlier than this (in ink-box diagonals) never match.
const MATCH_MAX_COST: f64 = 0.25;
/// No sample pair of a matched pair of strokes may lie farther apart (in ink-box diagonals).
const MATCH_MAX_SAMPLE_DISTANCE: f64 = 0.2;
/// Least matched share of the reference stroke length per source.
const MIN_MATCHED_FRACTION: f64 = 0.5;
/// Stroke radius bounds: at most this many font units, at least this many cells.
const MAX_RADIUS_UNITS: f64 = 200.0;
const MIN_RADIUS_CELLS: f64 = 0.5;
/// Ternary-search steps of the ridge centring.
const RIDGE_SEARCH_STEPS: usize = 16;

/// Why the skeleton blend gave up on a glyph. `Display` is the reason key counted in
/// `BlendStats::skeleton_fallback_reasons`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SkeletonFallback {
    /// A source glyph has more strokes than the bounded matcher takes (also: a raw graph or a
    /// reconstruction over its bound).
    TooManyStrokes {
        /// Strokes (or entries) found.
        count: usize,
    },
    /// A source outline is too dense for the bounded analysis (flattened points beyond
    /// `MAX_SKELETON_POINTS`, contours beyond `MAX_SKELETON_CONTOURS`, or a boundary index /
    /// overlap classification over its work bound); rejected before any raster work.
    TooComplex,
    /// Thinning or reconstruction left nothing.
    NoSkeleton,
    /// Too little of the reference glyph's stroke length found a partner.
    Unmatched {
        /// Matched share of the reference stroke length, `0..=1`.
        matched_fraction: f64,
    },
    /// The aligned glyph is beyond the raster limits.
    Extent(ExtentTooLarge),
}

impl SkeletonFallback {
    /// Stable reason key (stats map key, log text).
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::TooManyStrokes { .. } => "too_many_strokes",
            Self::TooComplex => "too_complex",
            Self::NoSkeleton => "no_skeleton",
            Self::Unmatched { .. } => "unmatched",
            Self::Extent(_) => "extent",
        }
    }
}

impl fmt::Display for SkeletonFallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason())
    }
}

/// Blends two or more non-empty outlines (`weights` normalized, `advances` the sources'
/// advances in normalized units) by stroke skeleton. Output contours follow the crate's
/// orientation (outer clockwise).
///
/// # Errors
/// A `SkeletonFallback` naming why this glyph must be blended by distance field instead.
pub(crate) fn blend(outlines: &[&Outline], weights: &[f64], advances: &[f64]) -> Result<Outline, SkeletonFallback> {
    // Budgeted while flattening: a hostile outline is refused before any raster work (the
    // distance-field fallback then applies its own, larger budget).
    let raw = outlines
        .iter()
        .map(|outline| if outline.len() > MAX_SKELETON_CONTOURS { Err(SkeletonFallback::TooComplex) } else { sdf::flatten_checked(outline, MAX_SKELETON_POINTS).map_err(|_| SkeletonFallback::TooComplex) })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(alignment) = sdf::choose_alignment(&raw, weights, advances).map_err(SkeletonFallback::Extent)? else {
        return Err(SkeletonFallback::NoSkeleton);
    };
    let polygons: Vec<Vec<Vec<Point>>> = raw.iter().zip(&alignment).map(|(polygons, map)| map.apply(polygons)).collect();
    let all: Vec<Vec<Point>> = polygons.iter().flatten().cloned().collect();
    let Some(union) = sdf::checked_bounds(&all).map_err(SkeletonFallback::Extent)? else {
        return Err(SkeletonFallback::NoSkeleton);
    };
    let grid = sdf::grid_for(union, SKELETON_CELL_UNITS).ok_or(SkeletonFallback::Extent(ExtentTooLarge::of(union)))?;
    let mut skeletons = polygons.iter().map(|source| graph::extract(source, &grid)).collect::<Result<Vec<_>, _>>()?;
    let diagonal = (union.x1 - union.x0).hypot(union.y1 - union.y0).max(grid.cell);
    let reference = heaviest(weights);
    harmonize::harmonize(&mut skeletons, reference, diagonal, grid.cell);
    let correspondence = correspond::correspond(&skeletons, reference, diagonal)?;
    let strokes = place::place(&skeletons, weights, reference, &correspondence, grid.cell);
    rebuild::rebuild(&strokes, grid.cell)
}

/// Position of the largest weight; ties go to the earliest (recipe order).
fn heaviest(weights: &[f64]) -> usize {
    let mut best = 0;
    for (position, weight) in weights.iter().enumerate().skip(1) {
        if *weight > weights[best] {
            best = position;
        }
    }
    best
}
