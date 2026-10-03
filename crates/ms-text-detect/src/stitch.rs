/*
File: crates/ms-text-detect/src/stitch.rs

Purpose:
Stitches per-tile probability maps back into one scaled-space map with feathered weights that
sum to exactly one at every pixel.

Key functions:
- stitch_tiles : tile-space maps of one channel (plan order) -> one map of `scaled_size`.

Notes:
Weights are separable. Along an axis, tile `i` has the ramp `w(d) = min(1, (d + 0.5) / O)` on
each INTERIOR edge (`d` = distance in pixels from that edge, `O` = the plan's minimum overlap;
page-border edges keep weight 1), the product of its two edge ramps. At every coordinate the
weights of the covering tiles are normalized and quantized to integers that sum to exactly 256
(largest remainder to the heaviest tile), so the 2-D weight `qx * qy` sums to exactly 65536 and
a constant input reproduces exactly; otherwise the rounding error is below one level.
The output is GATHERED row by row with a `u32` row accumulator, so no page-sized accumulator
exists besides the output itself.
*/

use crate::num::idx;
use crate::pipeline::DetectError;
use crate::plan::DetectionPlan;
use crate::runner::ProbMap;

/// Sum of the quantized per-axis weights at every coordinate.
const AXIS_ONE: u32 = 256;
/// Fixed-point shift of the 2-D weight (`AXIS_ONE * AXIS_ONE == 1 << 16`).
const WEIGHT_SHIFT: u32 = 16;

/// Stitches the tile maps of ONE channel into a map of `plan.scaled_size()`.
///
/// `maps[t]` belongs to `plan.tiles()[t]` and is in tile space (`plan.tile_input()`), its valid
/// area at the top-left; the padding beyond the tile's `w x h` is ignored.
///
/// # Errors
/// [`DetectError::MapCount`] when `maps.len() != plan.tiles().len()`; [`DetectError::MapShape`]
/// when a map is not `plan.tile_input()` (reported with channel 0). [`DetectError::Map`] is
/// unreachable for a plan from `plan_detection` (non-zero scaled size).
pub fn stitch_tiles(plan: &DetectionPlan, maps: &[&ProbMap]) -> Result<ProbMap, DetectError> {
    let tiles = plan.tiles();
    if maps.len() != tiles.len() {
        return Err(DetectError::MapCount { expected: tiles.len(), got: maps.len() });
    }
    let tile_input = plan.tile_input();
    if let Some((tile, map)) = maps.iter().enumerate().find(|(_, m)| m.size() != tile_input) {
        return Err(DetectError::MapShape { tile, channel: 0, expected: tile_input, got: map.size() });
    }
    let [cols, _] = plan.grid();
    let [width, height] = plan.scaled_size();
    let overlap = plan.overlap_min();
    // Tiles are row-major: the first row carries every column position, every row starts a column.
    let first = tiles[0];
    let xs: Vec<u32> = tiles.iter().take(idx(cols)).map(|t| t.x).collect();
    let ys: Vec<u32> = tiles.iter().step_by(idx(cols).max(1)).map(|t| t.y).collect();
    let wx = axis_weights(width, &xs, first.w, overlap);
    let wy = axis_weights(height, &ys, first.h, overlap);

    let row_stride = idx(tile_input[0]);
    let mut out = vec![0_u8; idx(width) * idx(height)];
    let mut acc = vec![0_u32; idx(width)];
    for (y, out_row) in (0..height).zip(out.chunks_exact_mut(idx(width))) {
        acc.fill(0);
        for (row, (&py, qys)) in ys.iter().zip(&wy).enumerate() {
            if y < py || y >= py + first.h {
                continue;
            }
            let local_y = y - py;
            let qy = qys[idx(local_y)];
            if qy == 0 {
                continue;
            }
            for (col, (&px, qxs)) in xs.iter().zip(&wx).enumerate() {
                let map = maps[row * idx(cols) + col].data();
                let start = idx(local_y) * row_stride;
                let src = &map[start..start + idx(first.w)];
                let dst = &mut acc[idx(px)..idx(px) + idx(first.w)];
                for ((a, &v), &qx) in dst.iter_mut().zip(src).zip(qxs) {
                    // qx, qy <= 256 and v <= 255, and the weights at one pixel sum to 65536, so
                    // the accumulator stays <= 255 * 65536.
                    *a += qx * qy * u32::from(v);
                }
            }
        }
        for (o, &a) in out_row.iter_mut().zip(&acc) {
            *o = u8::try_from((a + (1 << (WEIGHT_SHIFT - 1))) >> WEIGHT_SHIFT).unwrap_or(u8::MAX);
        }
    }
    // Cannot fail: the plan's scaled size is non-zero and `out` has exactly `width * height` bytes.
    Ok(ProbMap::new(width, height, out)?)
}

/// Quantized per-axis weights: `result[i][d]` is tile `i`'s weight at local coordinate `d`
/// (`0..extent`). At every global coordinate in `0..len` the covering tiles' weights sum to
/// [`AXIS_ONE`].
fn axis_weights(len: u32, positions: &[u32], extent: u32, overlap: u32) -> Vec<Vec<u32>> {
    let n = positions.len();
    let ramp = f64::from(overlap.max(1));
    let raw = |i: usize, d: u32| -> f64 {
        let lead = if i > 0 { ((f64::from(d) + 0.5) / ramp).min(1.0) } else { 1.0 };
        let trail = if i + 1 < n { ((f64::from(extent - 1 - d) + 0.5) / ramp).min(1.0) } else { 1.0 };
        lead * trail
    };
    let mut out: Vec<Vec<u32>> = positions.iter().map(|_| vec![0; idx(extent)]).collect();
    let mut first_cover = 0_usize;
    let mut covering: Vec<(usize, f64)> = Vec::new();
    for g in 0..len {
        while first_cover < n && positions[first_cover] + extent <= g {
            first_cover += 1;
        }
        covering.clear();
        for (i, &p) in positions.iter().enumerate().skip(first_cover) {
            if p > g {
                break;
            }
            covering.push((i, raw(i, g - p)));
        }
        let total: f64 = covering.iter().map(|&(_, w)| w).sum();
        if covering.is_empty() || total <= 0.0 {
            // Unreachable for plans from `plan_detection` (full coverage, positive ramps); the
            // pixel then simply stays 0.
            continue;
        }
        let mut assigned = 0_u32;
        let mut heaviest = covering[0];
        for &(i, w) in &covering {
            // floor(256 * w / total) is in 0..=256.
            let q = crate::num::f64_round_to_u32((f64::from(AXIS_ONE) * w / total).floor()).unwrap_or(0);
            out[i][idx(g - positions[i])] = q;
            assigned += q;
            if w > heaviest.1 {
                heaviest = (i, w);
            }
        }
        let (i, _) = heaviest;
        out[i][idx(g - positions[i])] += AXIS_ONE.saturating_sub(assigned);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{DetectParams, EngineKind, plan_detection};

    fn plan(engine: EngineKind, w: u32, h: u32) -> DetectionPlan {
        plan_detection(engine, [w, h], &DetectParams::default()).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Cuts the tile-space maps of a scaled-space function `f(x, y)`, padding with `pad`.
    fn tile_maps(plan: &DetectionPlan, f: impl Fn(u32, u32) -> u8, pad: u8) -> Vec<ProbMap> {
        let [tw, th] = plan.tile_input();
        plan.tiles()
            .iter()
            .map(|t| {
                let data = (0..th).flat_map(|y| (0..tw).map(move |x| (x, y))).map(|(x, y)| if x < t.w && y < t.h { f(t.x + x, t.y + y) } else { pad }).collect();
                ProbMap::new(tw, th, data).unwrap_or_else(|err| panic!("{err}"))
            })
            .collect()
    }

    fn stitch(plan: &DetectionPlan, maps: &[ProbMap]) -> ProbMap {
        let refs: Vec<&ProbMap> = maps.iter().collect();
        stitch_tiles(plan, &refs).unwrap_or_else(|err| panic!("{err}"))
    }

    #[test]
    fn constant_maps_reproduce_exactly() {
        for (engine, w, h) in [(EngineKind::Ctd, 800, 12_000), (EngineKind::Paddle, 2500, 7000), (EngineKind::Paddle, 500, 400), (EngineKind::Surya, 900, 4321)] {
            let p = plan(engine, w, h);
            for value in [0_u8, 1, 77, 200, 255] {
                let out = stitch(&p, &tile_maps(&p, |_, _| value, 13));
                assert_eq!(out.size(), p.scaled_size());
                assert!(out.data().iter().all(|&v| v == value), "{engine:?} {w}x{h} value {value}");
            }
        }
    }

    #[test]
    fn axis_weights_sum_to_one_everywhere_on_random_grids() {
        let mut state = 0x9e37_79b9_u32;
        let mut next = |m: u32| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state % m
        };
        for _ in 0..200 {
            let tile = 64 + next(400);
            let overlap = 1 + next(tile / 2);
            let len = 1 + next(5000);
            let positions = crate::plan::axis_positions(len, tile, overlap);
            let extent = len.min(tile);
            let weights = axis_weights(len, &positions, extent, overlap);
            for g in 0..len {
                let sum: u32 = positions.iter().zip(&weights).filter(|(p, _)| **p <= g && g < **p + extent).map(|(p, q)| q[idx(g - p)]).sum();
                assert_eq!(sum, AXIS_ONE, "len {len} tile {tile} overlap {overlap} g {g}");
            }
        }
    }

    #[test]
    fn identical_tiles_of_a_ramp_stitch_continuously() {
        // Every tile sees the same global ramp, so the stitched map must equal the ramp itself
        // (within one level of rounding) with no seam.
        let p = plan(EngineKind::Ctd, 800, 6000);
        assert!(p.grid()[1] > 1);
        let ramp = |x: u32, y: u32| u8::try_from((x / 4 + y / 25) % 256).unwrap_or(0);
        let out = stitch(&p, &tile_maps(&p, ramp, 0));
        let w = p.scaled_size()[0];
        for (i, &v) in out.data().iter().enumerate() {
            let (x, y) = (u32::try_from(i).unwrap_or(0) % w, u32::try_from(i).unwrap_or(0) / w);
            assert!(v.abs_diff(ramp(x, y)) <= 1, "({x}, {y}) {v} vs {}", ramp(x, y));
        }
    }

    #[test]
    fn disagreeing_tiles_blend_without_jumps() {
        // Tile t reports the constant 40 * t: inside the overlap the output moves monotonically
        // from one value to the next, with steps no larger than the ramp slope allows.
        let p = plan(EngineKind::Paddle, 960, 3000);
        let [tw, th] = p.tile_input();
        let maps: Vec<ProbMap> = (0..p.tiles().len()).map(|t| ProbMap::new(tw, th, vec![u8::try_from(40 * t).unwrap_or(255); crate::num::idx(tw * th)]).unwrap_or_else(|err| panic!("{err}"))).collect();
        let out = stitch(&p, &maps);
        let w = crate::num::idx(p.scaled_size()[0]);
        let column: Vec<u8> = out.data().iter().step_by(w).copied().collect();
        for pair in column.windows(2) {
            assert!(pair[1] >= pair[0], "monotone: {pair:?}");
            assert!(pair[1] - pair[0] <= 1, "smooth: {pair:?}");
        }
    }

    #[test]
    fn wrong_map_count_and_shape_are_errors() {
        let p = plan(EngineKind::Paddle, 960, 3000);
        let maps = tile_maps(&p, |_, _| 1, 0);
        let refs: Vec<&ProbMap> = maps.iter().skip(1).collect();
        assert!(matches!(stitch_tiles(&p, &refs), Err(DetectError::MapCount { .. })));
        let small = ProbMap::new(4, 4, vec![0; 16]).unwrap_or_else(|err| panic!("{err}"));
        let mut refs: Vec<&ProbMap> = maps.iter().collect();
        refs[1] = &small;
        assert!(matches!(stitch_tiles(&p, &refs), Err(DetectError::MapShape { tile: 1, got: [4, 4], .. })));
    }
}
