/*
File: crates/ms-tab-ps-editor/src/marquee.rs

Purpose:
Screen-path decimation and arc-length dashing for the selection marquee.

Key functions:
- `decimate_to_screen()`: drops sub-`MARQUEE_MIN_STEP_PX` steps of a pixel-edge loop.
- `walk_dash_runs()`: emits black/white `DashRun`s by CUMULATIVE arc length, capped by a budget.

Notes:
Used by `draw_selection_marquee` and the brush cursor outline; `DashRun` is defined in `lib.rs`.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// Maps an image-space boundary loop to screen space, dropping points that land closer than
/// `MARQUEE_MIN_STEP_PX` to the previously kept one.
///
/// `loop_pts` traces the mask's pixel edges, so at zoom < 1 a staircase contributes many sub-pixel
/// steps that no dash pattern can resolve; decimating them bounds the emitted run count by the
/// loop's SCREEN length instead of by the mask's complexity. At zoom >= 1 a one-pixel step is at
/// least one screen pixel, so nothing is dropped and the marquee stays pixel-accurate. The first
/// and last points are always kept, so a closed loop stays closed.
///
/// `out` is cleared by the caller and reused across loops.
pub(crate) fn decimate_to_screen(
    loop_pts: &[(f32, f32)],
    view: &viewport::ViewTransform,
    out: &mut Vec<Pos2>,
) {
    let Some((&first, rest)) = loop_pts.split_first() else {
        return;
    };
    let mut last_kept = view.world_to_screen(Pos2::new(first.0, first.1));
    out.push(last_kept);
    let last_idx = rest.len().saturating_sub(1);
    for (i, &(x, y)) in rest.iter().enumerate() {
        let p = view.world_to_screen(Pos2::new(x, y));
        // Keep the closing point unconditionally; otherwise keep only points that advance the
        // path by at least one decimation step.
        if i == last_idx || (p - last_kept).length() >= MARQUEE_MIN_STEP_PX {
            out.push(p);
            last_kept = p;
        }
    }
}

/// Walks a screen-space polyline and emits alternating black/white dash runs by CUMULATIVE arc
/// length, calling `emit` once per run. Returns the number of runs emitted (never above `budget`).
///
/// The dash phase is carried across segment boundaries, which is the whole point: a marquee path
/// traced along mask pixel edges is made of one-pixel segments, far shorter than `dash_len`, so
/// dashing each segment independently restarts the phase every pixel and paints a solid line. Here
/// one dash may span many segments and one segment may hold many dashes.
///
/// `dash_len` is in SCREEN pixels, so the pattern is zoom-invariant. A path shorter than one dash
/// emits a single black run. Returns 0 for a path with fewer than two points, a non-positive
/// `dash_len`, or a zero `budget`. Pure: no painter, no global state.
///
/// Two callers share it: `draw_selection_marquee` here and the brush's pixel-exact cursor outline
/// (`tools/brush.rs`, `draw_pixel_outline`) — both are staircases traced along pixel edges, and both
/// need the same black/white contrast to read over arbitrary artwork. A run keeps its input's exact
/// axis alignment (`a + dir * t` leaves the constant coordinate bit-identical), which is what lets
/// the tessellator snap an emitted segment to the physical pixel grid; do not change that.
pub(crate) fn walk_dash_runs(
    path: &[Pos2],
    dash_len: f32,
    budget: usize,
    mut emit: impl FnMut(DashRun),
) -> usize {
    if path.len() < 2 || budget == 0 || !(dash_len.is_finite() && dash_len > 0.0) {
        return 0;
    }
    let mut emitted = 0usize;
    // Phase carried across segments: distance left in the current dash, and its colour.
    let mut dash_left = dash_len;
    let mut black = true;
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let delta = b - a;
        let seg_len = delta.length();
        // A broken `ViewTransform` can map a point to infinity or NaN. An infinite `seg_len` never
        // shrinks below the loop threshold, so the inner loop would emit `budget` NaN runs from
        // this one segment; NaN would silently poison the phase. Skip such a segment entirely.
        if !seg_len.is_finite() || seg_len <= MARQUEE_MIN_RUN_PX {
            continue;
        }
        let dir = delta / seg_len;
        // Distance from `a` already emitted, and the part of this segment still to emit.
        let mut cursor = 0.0f32;
        let mut rest = seg_len;
        while rest > MARQUEE_MIN_RUN_PX {
            if emitted == budget {
                return emitted;
            }
            let take = dash_left.min(rest);
            let next = cursor + take;
            // Float stall guard: on a very long path `take` can fall below the ulp of `cursor`,
            // which would leave `cursor` unchanged and spin forever. Abandon the segment instead.
            if next <= cursor {
                break;
            }
            emit(DashRun {
                from: a + dir * cursor,
                to: a + dir * next,
                black,
            });
            emitted += 1;
            dash_left -= take;
            if dash_left <= MARQUEE_MIN_RUN_PX {
                dash_left = dash_len;
                black = !black;
            }
            rest -= take;
            cursor = next;
        }
        // Fold the sub-threshold tail into the phase so the pattern does not drift over a
        // staircase of thousands of tiny segments.
        if rest > 0.0 && rest <= MARQUEE_MIN_RUN_PX {
            dash_left -= rest;
            if dash_left <= MARQUEE_MIN_RUN_PX {
                dash_left = dash_len;
                black = !black;
            }
        }
    }
    emitted
}
