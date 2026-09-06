/*
File: tabs/ps_editor/tools/brush.rs

Purpose:
Photoshop-like round brush for the PS-like editor. Paints (or erases) onto the active editable
raster layer, clipped by the current selection. A plain round tip with hardness, Normal blend mode
only — deliberately no dual brush, scattering, texture, pen pressure, airbrush or smoothing.

Main responsibilities:
- own the brush parameters (diameter, hardness, opacity, flow, colour, eraser) across tool/page switches
- turn pointer samples into evenly spaced stamps and accumulate their coverage in an f32 alpha buffer
- composite that buffer over the pre-stroke pixels into the layer image, in premultiplied space
- interpret the Photoshop key/mouse bindings the tab forwards to it
- draw the cursor: a pixel-exact outline of the pixels the next stamp would affect

Key structures:
- `BrushTool`: the tool itself; parameters, the live stroke / HUD gesture, and the cursor caches.
- `StrokeState`: one in-flight stroke — its SPARSE tiled alpha buffer, the pre-stroke snapshot it
  composites over, the selection clip mask, and the arc-length carry that keeps stamp spacing
  continuous.
- `StrokeTile`: one 128x128 square of that buffer, allocated on first touch and never copied.
- `HudGesture`: the Alt + right-drag size/hardness gesture's start values and accumulated delta.
- `StampParams`: the per-frame stamp geometry handed to the pure stamping helpers.
- `CursorOutline`: the cached pixel-exact cursor loop plus the tip parameters it was built for.
- `LocalMap`: layer-local <-> page-space mapping, captured by value.

Key functions:
- `stamp_coverage`: the analytic antialiased hardness falloff (the ONE coverage formula).
- `accumulate_alpha`: Photoshop's flow build-up, which is what keeps a self-crossing stroke flat.
- `composite_pixel`: premultiplied source-over (paint) / alpha scale-down (erase).
- `walk_segment`: constant arc-length stamp placement with a carry across frames.
- `diameter_step_up` / `diameter_step_down`: Photoshop's non-linear `[` / `]` size table.
- `coverage_radius_50`: the 50 %-coverage contour the smooth fallback circle represents.
- `coverage_rows` / `coverage_outline_corners` / `merge_collinear_closed` / `build_coverage_outline`:
  the pure, GUI-free cursor-outline builder.

Notes:
Sizes are DIAMETERS in page pixels (`MIN_DIAMETER..=MAX_DIAMETER`), matching Photoshop and making
odd diameters reachable; a stamp centre is snapped to a pixel centre for an odd diameter and to a
pixel boundary for an even one, so diameter 1 paints exactly one pixel. The snap applies to every
stamp only up to `SNAP_ALL_MAX_DIAMETER`; above it just the stroke's opening stamp — the one the
cursor previews — is snapped, because rounding interpolated stamps ripples a thin diagonal stroke.

The tip profile has NO flat core below hardness 1 and the stamp spacing is 10 % of the diameter,
not Photoshop's 25 %. Both exist for the same reason: a stroke's centreline alpha is a sum of tip
profiles sampled once per spacing, so a flat core, or an edge falling off over less than one
spacing, makes the number of contributing stamps flip by one with the walk phase and beads the
stroke at a low flow. See `stamp_coverage` and `SPACING_RATIO`.

The stroke buffer is SPARSE: fixed 128x128 tiles keyed by tile coordinate, allocated on first touch,
never grown, moved or copied. Memory therefore tracks the stroke's real footprint rather than its
bounding box — on a ~800x19000 ribbon page a bounding-box buffer for one long diagonal stroke is
~137 MB, and copying it on every growth is blocking work on the GUI thread. See `STROKE_TILE`.

The stroke composites over a SNAPSHOT of `layer.image` taken when each buffer tile is allocated,
not over `layer.base_image`. The two are equal at stroke start (a paintable layer carries no
effects, so `image == base_image`), but the snapshot does not depend on WHEN the tab re-syncs
`base_image` from the shared doc after a commit, so a second stroke can never composite over stale
pixels and undo the first. `base_image` is never written here — the tab's release commit still
reads the undo "before" from it.

A stroke may only START while the pointer is on bare canvas (`PsToolContext::pointer_in_viewport`);
once in flight it keeps painting even when the pointer crosses a floating dock panel. See the guard
in `interact`. `PsTool::gesture_in_flight` publishes the same marker so the tab can apply the
identical rule to the brush CURSOR CIRCLE.

Alt + LEFT is the eyedropper and is executed by the TAB, not here: sampling the visible composite
needs the band Z order, which lives in `../mod.rs` and is not part of `PsToolContext`. This file
only refuses to start a stroke while Alt is held (`suppress_stroke`).

THE CURSOR IS THE PIXELS, NOT A CIRCLE. `draw_overlay` outlines the exact set of pixels the next
stamp would cover at 50 % or more, so per-pixel work at a high zoom shows where the brush actually
lands. That set is asked of `stamp_coverage` — the same kernel the painting path uses — and placed
with `snap_center`; neither may be re-derived, because a cursor built from its own copy of the
formula eventually marks pixels the brush does not paint. The outline is cached by
`(diameter, hardness)` and only remapped per frame. It degrades to the old smooth 50 %-contour ring
in the two cases where a staircase would be a lie or unreadable: a rotated or scaled layer (the
brush paints in LAYER-local pixels, which are not axis-aligned on screen there), and a view showing
fewer than `PIXEL_OUTLINE_MIN_SCALE` screen px per image px. `draw_overlay` cannot see the layer
stack, so `interact` captures the destination grid into `cursor_grid_origin` for it — the same
per-frame overlay cache `TransformTool` keeps for its gizmo.
*/

use super::{DirtyRect, PsHotkeyRow, PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::tabs::ps_editor::layers::{Layer, LayerId};
use crate::tabs::ps_editor::page_pixel_index;
use crate::tabs::ps_editor::selection::Selection;
use crate::tabs::ps_editor::viewport::ViewTransform;
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use std::collections::HashMap;

/// Smallest brush diameter in page px. One pixel wide, which the coverage formula must reproduce
/// exactly (see `stamp_coverage`), because per-pixel work is a stated requirement of this brush.
pub const MIN_DIAMETER: u32 = 1;
/// Largest brush diameter in page px.
pub const MAX_DIAMETER: u32 = 400;

/// Stamp spacing as a fraction of the diameter. Deliberately NOT exposed in the UI: the user's
/// scope is a plain round brush.
///
/// Photoshop's default is 25 %, and this brush cannot afford it. A stamped stroke's centreline
/// alpha is a sum of tip profiles sampled every `SPACING_RATIO * diameter`, so any tip whose edge
/// falls off over LESS than one spacing aliases: the number of stamps whose steep flank covers a
/// given pixel flips by one as the walk phase slides, and at a low flow — where every stamp is a
/// distinct build-up step rather than a saturated one — that flip is a visible periodic scallop.
/// Measured worst-case centreline ripple (max/min alpha over hardness x flow x diameter, see
/// `the_stroke_centreline_has_no_periodic_ripple`): 1.27x at 25 %, 1.18x at 20 %, 1.07x at 12.5 %,
/// 1.04x at 10 %. Only a reciprocal of a whole number is usable — at hardness 1 the tip is a
/// rectangle of width `diameter`, whose stamp count is constant exactly when `1 / SPACING_RATIO` is
/// an integer (1/6.67 measures 1.16x where 1/6 measures 1.00x).
///
/// The cost is linear: stamps per unit stroke length scale as `1 / SPACING_RATIO`, so painting a
/// tip whose spacing is not pinned to its floor (5 local px and up) is 2.5x the per-pixel work of
/// Photoshop's 25 %. Measured, the kernel itself costs 2.3 ns per pixel against the previous
/// formula's 1.6 ns, which puts one `MAX_DIAMETER` stamp at ~0.4 ms.
const SPACING_RATIO: f32 = 0.10;
/// Minimum stamp spacing in layer-local px for a tip whose stamps are snapped to the pixel grid, so
/// a sub-pixel brush still advances.
///
/// One whole pixel, because the grid IS that tip's resolution: a finer step cannot move a snapped
/// stamp, it only hands one pixel a second flow step — and along a diagonal an uneven number of
/// them, which is the ripple the snap policy exists to avoid.
const MIN_SPACING_SNAPPED: f32 = 1.0;
/// Minimum stamp spacing in layer-local px for a tip whose stamps are not snapped.
///
/// Half a pixel, which for an unsnapped stamp is a real displacement. A 3-5 px tip needs it: at a
/// one-pixel floor its spacing would be a third of its own diameter — far coarser than
/// [`SPACING_RATIO`] asks for, and enough to ripple the stroke by 11 % where 0.5 keeps it under
/// 5 %. The extra stamps cost nothing at these diameters; the floor exists at all only so the walk
/// cannot stall on a sub-pixel tip.
const MIN_SPACING: f32 = 0.5;
/// Floor on `1 - hardness` in the falloff exponent, so a fully hard tip yields a finite exponent.
///
/// At `1e-6` the exponent is `1e6`, which drives the core term to a step at the tip's outer extent
/// to well within f32 precision: hardness 1 therefore reduces EXACTLY to the analytic one-pixel rim
/// ramp, which is the antialiasing. See [`stamp_coverage`].
const HARDNESS_EPS: f32 = 1e-6;
/// Largest tip diameter, in layer-local px, whose INTERPOLATED stamps are still snapped to the
/// pixel grid by [`snap_center`].
///
/// Snapping exists so that diameter 1 paints exactly one pixel and diameter 2 a symmetric 2x2; at
/// any wider tip it only quantizes the walk. That quantization is not free: a 3-5 px tip's spacing
/// is set by its floor rather than by [`SPACING_RATIO`], so it is comparable to the rounding
/// itself, and along a diagonal consecutive rounded centres cluster and skip — measured 1.56x
/// centreline ripple against 1.29x unsnapped. 2.5 is the midpoint between the two diameters that
/// need the snap and the first that does not, so a layer scale that lands `d_local` slightly off
/// 2.0 still snaps.
const SNAP_ALL_MAX_DIAMETER: f32 = 2.5;
/// Side of one square stroke-buffer tile, in layer-local px.
///
/// The stroke buffer is SPARSE: tiles are allocated on first touch and never moved or copied, so
/// its memory tracks the stroke's actual footprint instead of its bounding box. That distinction is
/// not academic on a ribbon page (~800x19000 px): a bounding-box buffer for one full-length diagonal
/// stroke is ~137 MB and a grow-and-copy policy would memcpy all of it on the GUI thread, several
/// times, late in the stroke — the exact blocking work the GUI thread may never do.
///
/// 128 px keeps the per-tile snapshot (one `Layer::image` read per pixel plus the selection clip)
/// short enough to be invisible, while a tile is coarse enough that a stamp touches one or two of
/// them and the map is consulted per tile, never per pixel.
const STROKE_TILE: i32 = 128;
/// [`STROKE_TILE`] as a `usize` row stride, for indexing inside a tile. The literal is a small
/// positive constant, so the conversion is exact and checked at compile time by the `const` context.
const STROKE_TILE_SIDE: usize = STROKE_TILE as usize;
/// Pixels in one stroke tile.
const STROKE_TILE_AREA: usize = STROKE_TILE_SIDE * STROKE_TILE_SIDE;
/// HUD gesture (Alt + right drag): horizontal px of drag per px of diameter. Photoshop's exact rate
/// is undocumented; 1:1 makes the diameter follow the cursor, which reads as direct manipulation.
const HUD_PX_PER_DIAMETER: f32 = 1.0;
/// HUD gesture: vertical px of drag spanning the WHOLE hardness range (0 %..100 %).
const HUD_PX_PER_HARDNESS_RANGE: f32 = 200.0;
/// Hardness grid step for `Shift+[` / `Shift+]`, as a fraction.
const HARDNESS_STEP: f32 = 0.25;

/// Smallest on-screen scale, in SCREEN px per image px, at which the cursor draws the pixel-exact
/// staircase instead of the smooth circle.
///
/// Below two screen pixels per image pixel a one-pixel step of the staircase is thinner than the
/// 1 px stroke that draws it, so the steps blur into a ragged ring that reads as noise while still
/// costing one segment each — a 400 px brush contributes ~1600 of them. The smooth circle carries
/// the same 50 %-coverage information more legibly there, and the pixel detail only becomes
/// actionable once a pixel is large enough to aim at, which is exactly this threshold.
const PIXEL_OUTLINE_MIN_SCALE: f32 = 2.0;
/// Length of one black or white run of the pixel outline, in SCREEN px.
///
/// The same 4 px the selection marquee uses (`MARQUEE_DASH_PX`, `../mod.rs`), so the tab's two
/// "outline over arbitrary artwork" overlays read as one visual language. Kept as its own constant
/// because the cursor and the marquee are independent design knobs.
const CURSOR_DASH_PX: f32 = 4.0;
/// Upper bound on the runs one cursor outline may emit, so a large brush at maximum zoom — whose
/// outline is mostly off-screen — cannot flood a frame with shapes.
const CURSOR_MAX_RUNS: usize = 20_000;
/// Alpha of the soft-tip reach hint (the outer smooth circle drawn when the painted footprint
/// extends past the 50 %-coverage contour). Low enough to read as a hint, not as the cursor.
const SOFT_REACH_ALPHA: u8 = 110;
/// Smallest gap, in image px, between the 50 %-coverage contour and the tip's nominal radius that
/// still earns the soft-reach hint. Below it the two contours coincide on screen and a second ring
/// would only thicken the cursor.
const SOFT_REACH_MIN_GAP: f32 = 0.5;
/// Offset of the HUD readout from the cursor, in screen px, on both axes. Far enough that the text
/// clears the cursor circle of a small brush.
const HUD_TEXT_OFFSET_PX: f32 = 14.0;
/// Font size of the HUD readout, in screen points.
const HUD_TEXT_SIZE: f32 = 13.0;

/// One `STROKE_TILE` x `STROKE_TILE` square of the sparse stroke buffer, in row-major order.
///
/// All three vectors are exactly `STROKE_TILE_AREA` long, so a pixel's index inside a tile is the
/// same in each. A tile that overlaps the layer edge keeps its out-of-layer pixels at their default
/// (transparent / unclipped); nothing ever reads them, because every stamp box is clamped to the
/// layer before it reaches here.
#[derive(Debug)]
struct StrokeTile {
    /// The stroke's own accumulated coverage in `0..=1`.
    alpha: Vec<f32>,
    /// The layer's pixels as they were before the stroke first touched this tile.
    orig: Vec<Color32>,
    /// Selection clip rasterized once when the tile was allocated; `None` when the stroke is
    /// unclipped.
    clip: Option<Vec<bool>>,
}

/// One in-flight brush stroke: the accumulated alpha, the pixels to composite it over, and the
/// walk state that keeps stamp spacing continuous across frames.
///
/// The buffer is a SPARSE map of fixed `STROKE_TILE`-sized tiles keyed by tile coordinate in
/// LAYER-LOCAL pixel space (`tile_of`), allocated on first touch and never reallocated or copied.
/// See [`STROKE_TILE`] for why the bounding-box alternative is not usable on a ribbon page.
#[derive(Debug)]
struct StrokeState {
    /// The layer this stroke belongs to. A stroke whose layer changed underneath it is dropped
    /// rather than composited into the wrong pixels.
    layer_id: LayerId,
    /// Layer image size the buffer was built for; a resize invalidates the stroke.
    layer_size: [usize; 2],
    /// The allocated tiles, keyed by `(tile x, tile y)`.
    tiles: HashMap<(i32, i32), StrokeTile>,
    /// Whether the page had a selection when the stroke STARTED. Latched, so a selection dropped
    /// mid-stroke cannot make the second half of one stroke ignore a clip the first half obeyed.
    clipped: bool,
    /// Arc length walked since the last stamp, in layer-local px. Carried ACROSS frames so spacing
    /// does not reset every frame.
    carry: f32,
    /// Previous pointer sample in layer-local px — the start of the next interpolated segment.
    last_local: Pos2,
    /// Where the stroke was anchored, in page px, for the Shift axis constraint.
    anchor_world: Pos2,
}

/// The cached pixel-exact cursor outline and the tip parameters it was built for.
///
/// `points` is one closed loop of pixel-corner offsets from the SNAPPED stamp centre, in image px,
/// with collinear edges merged and the first point repeated as the last — the same shape
/// `Selection::outline_loops` produces for the marquee. Rebuilding costs O(diameter^2) coverage
/// evaluations, so `key` gates it: only a change of diameter or hardness may pay for it.
#[derive(Debug)]
struct CursorOutline {
    /// `(diameter, hardness bits)` — the exact inputs `points` was built from. Hardness is compared
    /// by bit pattern so the check is an integer equality, not a float comparison.
    key: (u32, u32),
    points: Vec<Vec2>,
}

/// The Alt + right-drag HUD gesture's start values and its accumulated pointer delta in screen px.
#[derive(Debug, Clone, Copy)]
struct HudGesture {
    start_diameter: f32,
    start_hardness: f32,
    total: Vec2,
}

/// Photoshop-like round brush operating on the active raster layer.
#[derive(Debug)]
pub struct BrushTool {
    /// Brush DIAMETER in page px, `MIN_DIAMETER..=MAX_DIAMETER`.
    diameter: u32,
    /// Edge hardness in `0.0..=1.0`; 1.0 is a hard tip with a ~1 px antialiased rim.
    hardness: f32,
    /// Maximum alpha one stroke may build up to, `0.0..=1.0`.
    opacity: f32,
    /// Per-stamp build-up rate toward `opacity`, `0.0..=1.0`.
    flow: f32,
    color: Color32,
    erase: bool,
    /// Wheel accumulator for the tab's Shift+wheel size gesture.
    wheel_accum: f32,
    /// The live stroke, or `None` when idle.
    stroke: Option<StrokeState>,
    /// Anchor of the next Shift+click straight segment: the centre of the last stamp placed WHILE
    /// SHIFT WAS HELD, in page px. `None` when there is no live chain.
    ///
    /// It outlives the stroke that produced it, but only for as long as Shift stays down: the
    /// release drops it, so the next Shift+click starts a fresh chain instead of drawing back to a
    /// position from before the release. A stroke made without Shift leaves none at all.
    ///
    /// This is a DELIBERATE divergence from Photoshop, which chains from the last stamp of the
    /// previous stroke whatever the modifiers did in between. It was chosen on purpose; do not
    /// "restore" the Photoshop rule. See `interact` for where the scope is enforced.
    last_stamp_world: Option<Pos2>,
    /// The live HUD gesture, or `None`.
    hud: Option<HudGesture>,
    /// Set while the primary button is held for a gesture that is NOT a paint stroke (Alt =
    /// eyedropper, owned by the tab). Cleared on release, so releasing Alt mid-drag cannot turn an
    /// eyedropper drag into a stroke.
    suppress_stroke: bool,
    /// Latched on the frame a stroke ENDS, consumed once by the tab (`take_stroke_finished`).
    ///
    /// The tab's commit — the undo entry plus the push to the shared doc or `CleanOverlaysModel` —
    /// must not be driven by the raw pointer-release edge: a release delivered on a frame whose
    /// canvas input went to a pan never reaches `interact` at all. This latch is what carries the
    /// stroke's real end across those frames. `reset` clears it, because an ABANDONED stroke
    /// (page switch, tool switch) must not commit.
    stroke_finished: bool,
    /// Layer-local → page offset of the destination pixel grid, captured in `interact` for the
    /// cursor overlay of the same frame; `None` when the pixel-exact cursor cannot be honest.
    ///
    /// `Some(o)` means the active layer is pixel-editable AND axis-aligned and unscaled, so
    /// `LocalMap::to_world(l) == o + l` and a layer pixel is exactly one page pixel. `None` means
    /// there is no such layer — the cursor then falls back to the smooth circle. See
    /// `refresh_cursor_cache`.
    cursor_grid_origin: Option<Vec2>,
    /// Cached pixel-exact outline of the pixels the current tip would cover at ≥ 50 %.
    cursor_outline: Option<CursorOutline>,
}

impl Default for BrushTool {
    fn default() -> Self {
        Self {
            diameter: 20,
            hardness: 1.0,
            opacity: 1.0,
            flow: 1.0,
            color: Color32::BLACK,
            erase: false,
            wheel_accum: 0.0,
            stroke: None,
            last_stamp_world: None,
            hud: None,
            suppress_stroke: false,
            stroke_finished: false,
            cursor_grid_origin: None,
            cursor_outline: None,
        }
    }
}

impl BrushTool {
    /// Sets the diameter, clamped to `MIN_DIAMETER..=MAX_DIAMETER`. Returns true when it changed.
    pub fn set_diameter(&mut self, diameter: u32) -> bool {
        let next = diameter.clamp(MIN_DIAMETER, MAX_DIAMETER);
        if next == self.diameter {
            return false;
        }
        self.diameter = next;
        true
    }

    /// Replaces the paint colour, dropping any alpha: the brush's own alpha is `opacity`/`flow`.
    ///
    /// Used by the tab's Alt+click eyedropper, which samples the visible composite.
    pub fn set_color(&mut self, color: Color32) {
        self.color = Color32::from_rgb(color.r(), color.g(), color.b());
    }

    /// Consumes the "a stroke has ended" latch: `true` exactly once per finished stroke.
    ///
    /// This — not the pointer-release edge — is what tells the tab to commit a stroke (record the
    /// undo diff and push the pixels to the shared doc / `CleanOverlaysModel`). A release delivered
    /// on a frame whose canvas input went to a pan never reaches `interact`, so a commit keyed on
    /// the raw release loses the whole stroke; see `../mod.rs` (`commit_brush_stroke`).
    #[must_use]
    pub fn take_stroke_finished(&mut self) -> bool {
        std::mem::take(&mut self.stroke_finished)
    }

    /// Ends an in-flight stroke so the tab COMMITS it, instead of letting `reset` drop it.
    ///
    /// The tab calls this immediately before abandoning the gesture (Esc under input suppression, a
    /// tool switch, a page switch). `reset` cannot take painted pixels back — it receives no layer
    /// access — so a stroke abandoned without this leaves pixels the user can see with no undo
    /// entry, and on the `Клин` layer with no write-back to the shared overlay model at all.
    ///
    /// It goes through the same `end_stroke` — and therefore the same one-shot `stroke_finished`
    /// latch — as the ordinary ending, so one stroke can never commit twice. An idle brush is left
    /// untouched and latches nothing, which is what keeps the same tab helper from committing
    /// anything when another tool's gesture is the one being abandoned.
    pub fn end_stroke_for_commit(&mut self) {
        if self.stroke.is_some() {
            self.end_stroke("abandoned");
        }
    }

    /// Steps the diameter from the tab's Shift+wheel gesture; returns true when the event belongs
    /// to the brush.
    ///
    /// `delta_y` is the frame's smooth scroll delta. The TAB owns the decision that this wheel is
    /// the brush's — it calls this only with Shift held and the brush active (`../mod.rs`,
    /// `draw_canvas`) — so there is deliberately no modifier test here. Notches are accumulated so
    /// a high-resolution wheel does not jump, and the sub-notch remainder survives between
    /// gestures on purpose: two short flicks then add up instead of each losing a fraction.
    /// Returning true (even with no size change) tells the tab the wheel was the brush's, so it
    /// must not also zoom the canvas.
    pub fn handle_wheel(&mut self, delta_y: f32) -> bool {
        if delta_y.abs() <= f32::EPSILON {
            return true;
        }
        /// One physical wheel notch in egui's smooth scroll units.
        const WHEEL_NOTCH: f32 = 40.0;
        /// Upper bound on the table walks one frame's wheel delta may perform, so a synthetic or
        /// runaway scroll event cannot spin the loop.
        const MAX_NOTCHES: f32 = 64.0;
        self.wheel_accum += delta_y;
        let steps = (self.wheel_accum / WHEEL_NOTCH).trunc();
        if steps == 0.0 {
            return true;
        }
        self.wheel_accum -= steps * WHEEL_NOTCH;
        let mut next = self.diameter;
        // Walk the Photoshop step table once per notch so the wheel and `[`/`]` agree. The count is
        // walked in f32 rather than cast to an integer: `steps` comes from a float division, and a
        // lossy `as u32` on it is exactly what §17 forbids.
        let up = steps > 0.0;
        let mut remaining = steps.abs().min(MAX_NOTCHES);
        while remaining >= 1.0 {
            next = if up { diameter_step_up(next) } else { diameter_step_down(next) };
            remaining -= 1.0;
        }
        self.set_diameter(next);
        true
    }

    /// Applies one `[` / `]` step to the diameter.
    pub fn step_diameter(&mut self, up: bool) -> bool {
        let next = if up { diameter_step_up(self.diameter) } else { diameter_step_down(self.diameter) };
        self.set_diameter(next)
    }

    /// Scales the diameter by 0.9 (`down`) or 1.1, always moving by at least one px.
    ///
    /// This is the legacy `-` / `=` behaviour the tab has always exposed, kept alongside the
    /// Photoshop `[` / `]` table because both are documented in the shortcut panel.
    pub fn scale_diameter(&mut self, down: bool) -> bool {
        let d = self.diameter as f32;
        let next = if down {
            let scaled = (d * 0.9).floor() as u32;
            scaled.min(self.diameter.saturating_sub(1))
        } else {
            let scaled = (d * 1.1).ceil() as u32;
            scaled.max(self.diameter.saturating_add(1))
        };
        self.set_diameter(next)
    }

    /// Applies one `Shift+[` / `Shift+]` step to hardness, snapping to the 0/25/50/75/100 % grid.
    pub fn step_hardness(&mut self, up: bool) {
        self.hardness = step_hardness_value(self.hardness, up);
    }

    /// Sets opacity from a digit shortcut (`fraction` in `0.0..=1.0`).
    pub fn set_opacity(&mut self, fraction: f32) {
        self.opacity = fraction.clamp(0.0, 1.0);
    }

    /// Sets flow from a Shift+digit shortcut (`fraction` in `0.0..=1.0`).
    pub fn set_flow(&mut self, fraction: f32) {
        self.flow = fraction.clamp(0.0, 1.0);
    }

    /// Applies one frame of the Alt + right-drag HUD gesture.
    ///
    /// `delta` is this frame's pointer movement in screen px. Horizontal movement changes the
    /// diameter (right = larger), vertical changes hardness (down = harder), both relative to the
    /// values sampled when the gesture began, so the gesture is not path-dependent.
    fn update_hud(&mut self, delta: Vec2) {
        let Some(hud) = self.hud.as_mut() else {
            return;
        };
        hud.total += delta;
        let diameter = hud.start_diameter + hud.total.x / HUD_PX_PER_DIAMETER;
        let hardness = hud.start_hardness + hud.total.y / HUD_PX_PER_HARDNESS_RANGE;
        self.diameter = round_diameter(diameter);
        self.hardness = hardness.clamp(0.0, 1.0);
    }

    /// Radius of the smooth cursor ring in page px: the 50 %-coverage radius of the current tip.
    fn cursor_radius(&self) -> f32 {
        coverage_radius_50(self.diameter as f32, self.hardness)
    }

    /// Cache key of the pixel outline: the tip parameters its geometry depends on.
    ///
    /// The parity that decides the centre snap is `diameter % 2`, so it needs no key of its own.
    /// Hardness enters as its bit pattern, which makes the comparison an integer equality.
    fn outline_key(&self) -> (u32, u32) {
        (self.diameter, self.hardness.to_bits())
    }

    /// Refreshes the two pieces of cursor geometry `draw_overlay` needs but cannot look up itself.
    ///
    /// `draw_overlay` takes `&self` and never sees the layer stack, so the destination pixel grid
    /// has to be captured here — the same per-frame overlay cache `TransformTool` keeps for its
    /// gizmo. The frames on which the tab skips `interact` (a pan, a text-layer drag) cannot change
    /// a layer transform, so the captured grid can never be observed stale.
    ///
    /// The outline itself is rebuilt only when the tip parameters changed AND the pixel cursor
    /// would actually be drawn, so zooming out or selecting a rotated layer never pays for it.
    fn refresh_cursor_cache(&mut self, ctx: &PsToolContext<'_>) {
        self.cursor_grid_origin = ctx
            .stack
            .layer(ctx.stack.active_id())
            .filter(|layer| layer.can_edit_pixels())
            // The staircase describes LAYER pixels. On a rotated or scaled layer those are neither
            // axis-aligned on screen nor one page pixel wide, so a staircase drawn in page space
            // would mark pixels the stamp does not cover — exactly the lie this cursor exists to
            // prevent. Fall back to the smooth circle there.
            .filter(|layer| {
                layer.transform.rotation.abs() <= f32::EPSILON
                    && (layer.transform.scale - 1.0).abs() <= f32::EPSILON
            })
            .map(|layer| layer.transform.center - layer.image_size() * 0.5);

        if self.cursor_grid_origin.is_none() || ctx.view.zoom < PIXEL_OUTLINE_MIN_SCALE {
            return;
        }
        let key = self.outline_key();
        if self.cursor_outline.as_ref().is_some_and(|cached| cached.key == key) {
            return;
        }
        self.cursor_outline = Some(CursorOutline {
            key,
            points: build_coverage_outline(self.diameter, self.hardness),
        });
    }
}

impl PsTool for BrushTool {
    fn id(&self) -> PsToolId {
        PsToolId::Brush
    }

    fn title(&self) -> &'static str {
        t!("ps_editor.tools.brush_title")
    }

    /// A gesture is in flight while a stroke buffer is live OR the HUD drag is running.
    ///
    /// It is the same marker the start gate in `interact` reads, so the two can never disagree: a
    /// press the gate refused leaves `stroke == None` and is correctly reported as "no gesture",
    /// even though the button is held.
    fn gesture_in_flight(&self) -> bool {
        self.stroke.is_some() || self.hud.is_some()
    }

    /// Abandons the in-progress stroke and HUD gesture.
    ///
    /// The stroke buffer is dropped, so the next stroke starts from a fresh snapshot and cannot
    /// draw a segment from a position that belongs to another page. The pixels already composited
    /// into `layer.image` are LEFT AS THEY ARE — `reset` receives no layer access, and they are
    /// exactly what the user saw painted. `layer.base_image` was never written, so the tab's undo
    /// "before" is unaffected either way. See this module's `MODULE_README.md`.
    ///
    /// Because those pixels survive, an abandonment that just called this would leave them without
    /// an undo entry. The tab therefore runs [`BrushTool::end_stroke_for_commit`] FIRST, on every
    /// abandonment path; by the time `reset` runs there is normally no stroke left, and clearing
    /// the latch here only guards against a commit firing after the caller has already moved on to
    /// another page.
    fn reset(&mut self) {
        self.stroke = None;
        self.hud = None;
        self.suppress_stroke = false;
        // An abandoned stroke must NOT commit: `reset` runs on a page switch, where the tab has
        // already dropped the stroke's dirty union and where the next frame's layer stack belongs
        // to another page. Clearing the latch is what keeps `take_stroke_finished` from firing there.
        self.stroke_finished = false;
        self.last_stamp_world = None;
        // The cursor caches describe the page/layer that is going away; the next routed frame
        // recaptures them before anything is drawn.
        self.cursor_grid_origin = None;
        self.cursor_outline = None;
    }

    /// Runs one frame of the brush: the HUD gesture, or one frame of a paint stroke.
    ///
    /// Three gates decide whether anything is painted: the Alt-driven gestures (eyedropper, HUD)
    /// take priority and never paint; a stroke may only be STARTED while the pointer is inside the
    /// canvas viewport; and a stroke already in flight keeps painting wherever the pointer goes —
    /// including over a floating dock panel — until the first frame with the button up.
    ///
    /// The returned `dirty` is THIS FRAME's affected box in layer-local px (the segment bbox grown
    /// by the tip radius), never the growing stroke union: the tab re-uploads whole 1024 px tiles
    /// on a per-frame budget, and a growing union would exhaust it.
    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        let mut outcome = ToolOutcome::default();

        // Capture the cursor geometry first, before any branch can return: `draw_overlay` runs
        // later in the SAME frame and cannot reach the layer stack itself.
        self.refresh_cursor_cache(ctx);

        // The straight-line anchor is scoped to a SINGLE Shift hold, so releasing Shift drops it.
        // Tested as a LEVEL rather than as a down->up edge, and before every early return below: an
        // edge the tool never observed — a frame that went to a pan, focus returning with the key
        // already up — would otherwise leave a stale anchor and draw a segment across half the page
        // on the next Shift+click. Deliberately NOT Photoshop's rule (it chains from the previous
        // stroke's last stamp regardless of Shift); the tighter scope was chosen on purpose.
        if !ctx.modifiers.shift {
            self.last_stamp_world = None;
        }

        // Alt + right drag: the size/hardness HUD. It must never paint, and it outranks the stroke
        // because Alt also suppresses a stroke start below.
        if ctx.secondary_down && ctx.modifiers.alt {
            if self.hud.is_none() && self.stroke.is_none() {
                self.hud = Some(HudGesture {
                    start_diameter: self.diameter as f32,
                    start_hardness: self.hardness,
                    total: Vec2::ZERO,
                });
            }
            self.update_hud(ctx.pointer_delta);
            return outcome;
        }
        self.hud = None;

        // Alt + LEFT is the tab's eyedropper. Latch the suppression for the whole button hold so
        // releasing Alt mid-drag cannot promote the sample gesture into a stroke.
        if !ctx.primary_down && !ctx.primary_pressed {
            self.suppress_stroke = false;
        } else if self.stroke.is_none() && ctx.modifiers.alt {
            self.suppress_stroke = true;
        }

        // A stroke may BEGIN on the press frame or on any later held frame, but only on bare
        // canvas. Gating on `primary_pressed` alone cannot work: the tab hands the tool
        // `primary_pressed && pointer_in_viewport` (`../mod.rs`, `draw_canvas`), so over a floating
        // panel that flag is already false and the guard would never fire. `stroke` is the
        // in-flight marker; while it is `None` no stroke exists, so a pointer outside the viewport
        // must not start one. Once a stroke IS in flight it continues across a panel, mirroring the
        // tab's panning decision.
        let holding = ctx.primary_down || ctx.primary_pressed;
        let may_paint = !self.suppress_stroke
            && holding
            && (self.stroke.is_some() || ctx.pointer_in_viewport)
            && ctx.pointer_image.is_some();

        if may_paint {
            outcome.dirty = self.paint_frame(ctx);
        }

        // End the stroke on the first frame with the button up. A fast click whose press and
        // release egui delivers in the SAME frame therefore both paints (above) and ends here, and
        // the latch is what makes the tab commit it on that same frame.
        if !ctx.primary_down && self.stroke.is_some() {
            self.end_stroke("release");
        }
        outcome
    }

    /// Ends the stroke on a frame the tab did NOT route to the brush (a canvas pan or a text-layer
    /// drag is in progress).
    ///
    /// The brush is the one tool for which "freeze and resume" is not an option: its pixels are
    /// ALREADY in `layer.image`, and the suppressed frames hide the button events, so a release
    /// performed during a pan would otherwise never be observed — leaving the stroke without an
    /// undo entry and, on the `Клин` layer, never written to the shared overlay model at all.
    /// Ending it here instead makes the suppressed frame the stroke's end: the latch fires, the tab
    /// commits, and a stroke resumed after the pan is a new stroke with its own undo step.
    fn freeze(&mut self) {
        if self.stroke.is_some() {
            self.end_stroke("suppressed");
        }
        // A suppressed frame carries no modifiers, so the brush cannot see a Shift release that
        // happens during a pan. Dropping the line anchor is the conservative answer — it can never
        // keep one alive past a release the tool never saw — and a pan interrupts the chain anyway.
        self.last_stamp_world = None;
    }

    /// Draws the cursor: the pixel-exact outline of the pixels the next stamp would affect, or the
    /// smooth 50 %-coverage circle where that outline would be a lie or unreadable.
    ///
    /// Three overlays, in order:
    /// * the tip contour — the staircase when `draw_pixel_outline` accepts the frame, the smooth
    ///   ring otherwise;
    /// * for a soft tip, a thin low-alpha SMOOTH circle at the tip's nominal radius, marking the
    ///   reach the 50 %-coverage contour hides. Smooth on purpose: the rendering difference is what
    ///   tells the two contours apart at a glance;
    /// * while the Alt + right-drag HUD gesture runs, the diameter/hardness readout.
    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        pointer_image: Option<Pos2>,
    ) {
        let Some(pointer) = pointer_image else {
            return;
        };
        let center = view.world_to_screen(pointer);
        if !self.draw_pixel_outline(painter, view, pointer) {
            // The ring marks the 50 %-coverage contour, which for a soft tip is well inside the
            // tip's outer extent — the same convention Photoshop's cursor uses.
            let radius_screen = (self.cursor_radius() * view.zoom).max(0.5);
            // Black-on-white ring so the cursor reads on any background.
            painter.circle_stroke(center, radius_screen, Stroke::new(2.0, Color32::WHITE));
            painter.circle_stroke(
                center,
                (radius_screen - 1.0).max(0.5),
                Stroke::new(1.0, Color32::BLACK),
            );
        }

        // A soft tip keeps painting past its 50 %-coverage contour. Test the actual gap rather than
        // `hardness < 1.0`: the falloff width has a one-pixel floor, so a small tip's two contours
        // coincide even below full hardness and a second ring there would only thicken the cursor.
        let outer = self.diameter as f32 * 0.5;
        if outer - self.cursor_radius() >= SOFT_REACH_MIN_GAP {
            let outer_screen = (outer * view.zoom).max(0.5);
            painter.circle_stroke(
                center,
                outer_screen,
                Stroke::new(1.0, Color32::from_white_alpha(SOFT_REACH_ALPHA)),
            );
            painter.circle_stroke(
                center,
                (outer_screen - 1.0).max(0.5),
                Stroke::new(1.0, Color32::from_black_alpha(SOFT_REACH_ALPHA)),
            );
        }

        if self.hud.is_some() {
            self.draw_hud_readout(painter, center);
        }
    }

    fn as_brush_mut(&mut self) -> Option<&mut BrushTool> {
        Some(self)
    }

    /// Colour, diameter, hardness, opacity, flow and the eraser toggle are real parameters.
    fn has_options(&self) -> bool {
        true
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t!("ps_editor.tools.brush_color_label"));
            let mut rgb = [self.color.r(), self.color.g(), self.color.b()];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                self.color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            }
        });
        let mut diameter = self.diameter;
        if ui
            .add(
                crate::widgets::WheelSlider::new(&mut diameter, MIN_DIAMETER..=MAX_DIAMETER)
                    .text(t!("ps_editor.tools.brush_size_label")),
            )
            .changed()
        {
            self.set_diameter(diameter);
        }
        percent_slider(ui, &mut self.hardness, t!("ps_editor.tools.brush_hardness_label"));
        percent_slider(ui, &mut self.opacity, t!("ps_editor.tools.brush_opacity_label"));
        percent_slider(ui, &mut self.flow, t!("ps_editor.tools.brush_flow_label"));
        ui.checkbox(&mut self.erase, t!("ps_editor.tools.brush_eraser_label"));
    }

    /// Every key and mouse gesture the brush interprets.
    ///
    /// The keys themselves are read by the TAB (`PsEditorTabState::brush_shortcuts` and the wheel
    /// branch of `draw_canvas`) and forwarded to the methods above, because a tool never sees the
    /// `egui::Context`. This list and that dispatch must be edited together.
    fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
        vec![
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_wheel_label"),
                t!("ps_editor.tools.hotkey.brush.radius_wheel_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.size_step_label"),
                t!("ps_editor.tools.hotkey.brush.size_step_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_down_label"),
                t!("ps_editor.tools.hotkey.brush.radius_down_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.radius_up_label"),
                t!("ps_editor.tools.hotkey.brush.radius_up_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.hardness_label"),
                t!("ps_editor.tools.hotkey.brush.hardness_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.opacity_label"),
                t!("ps_editor.tools.hotkey.brush.opacity_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.flow_label"),
                t!("ps_editor.tools.hotkey.brush.flow_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.straight_label"),
                t!("ps_editor.tools.hotkey.brush.straight_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.axis_label"),
                t!("ps_editor.tools.hotkey.brush.axis_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.eyedropper_label"),
                t!("ps_editor.tools.hotkey.brush.eyedropper_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.brush.hud_label"),
                t!("ps_editor.tools.hotkey.brush.hud_keys"),
            ),
        ]
    }
}

impl BrushTool {
    /// Drops the in-flight stroke buffer and latches the "commit me" signal for the tab.
    ///
    /// `reason` names the transition for the trace log only. This is the ONE place a stroke ends
    /// normally — the button-up frame and the first suppressed frame both go through it — so the
    /// latch can never be forgotten on one of the two paths. Abandonment (`reset`) deliberately
    /// does NOT come here: it must leave the latch clear.
    fn end_stroke(&mut self, reason: &str) {
        use crate::trace::cat;
        crate::trace_log!(
            cat::INPUT,
            "brush stroke_end reason={} diameter={} hardness={:.2} erase={}",
            reason,
            self.diameter,
            self.hardness,
            self.erase
        );
        self.stroke = None;
        self.stroke_finished = true;
    }

    /// Paints one frame of the stroke and returns the layer-local box it changed.
    ///
    /// Begins the stroke when none is in flight (honouring a Shift+click chain from the current
    /// line anchor, if one is live), walks the segment from the previous pointer sample at constant
    /// arc length, accumulates every stamp into the alpha buffer, and re-composites only the pixels
    /// whose alpha changed this frame.
    ///
    /// The stamp it ends on becomes the new line anchor only while Shift is held — see
    /// `BrushTool::last_stamp_world` for the scope rule and why it is not Photoshop's.
    ///
    /// Returns `None` when nothing was stamped (the pointer moved less than one spacing) or the
    /// active layer refuses pixel edits.
    fn paint_frame(&mut self, ctx: &mut PsToolContext<'_>) -> Option<DirtyRect> {
        use crate::trace::cat;
        let pointer = ctx.pointer_image?;
        let starting = self.stroke.is_none();
        let shift = ctx.modifiers.shift;
        // Shift constrains the drag to the dominant axis relative to the stroke anchor. It is
        // recomputed every frame (not latched), so the line snaps between horizontal and vertical
        // as the pointer crosses the diagonal — the behaviour Photoshop shows.
        let target_world = match (&self.stroke, shift) {
            (Some(stroke), true) => constrain_to_axis(stroke.anchor_world, pointer),
            _ => pointer,
        };

        // `selection` and `stack` are distinct fields of `PsToolContext`, so the borrow checker
        // allows borrowing the selection immutably while the active layer is held mutably.
        let selection = ctx.selection.as_ref();
        let layer = ctx.stack.active_editable_mut()?;
        let map = LocalMap::from_layer(layer);
        let layer_size = layer.image.size;

        // Diameter in LAYER-local px: the destination grid is the layer's, so a scaled layer needs
        // the tip rescaled with it, and the antialiasing floor must be one LOCAL pixel.
        let d_local = (self.diameter as f32 / map.scale).max(0.01);
        let params = StampParams {
            radius: d_local * 0.5,
            hardness: self.hardness,
            opacity: self.opacity,
            flow: self.flow,
            odd_diameter: (d_local.round().max(1.0) as i64) % 2 == 1,
        };
        let (spacing, snap_interpolated) = stamp_step(d_local);

        let to_local = map.to_local(target_world);
        let mut centers: Vec<Pos2> = Vec::new();
        if starting {
            // A Shift+click chains a straight segment from the anchor at any angle. The anchor
            // exists only while one Shift hold has been running since it was placed, so without one
            // this is an ordinary stroke start that merely becomes the new anchor.
            let chain_from = if shift { self.last_stamp_world } else { None };
            let from_local = chain_from.map(|w| map.to_local(w));
            let anchor_local = from_local.unwrap_or(to_local);
            self.stroke = Some(StrokeState::new(
                layer.id,
                layer_size,
                selection.is_some(),
                anchor_local,
                // The axis constraint anchors at the PRESS, not at the chained segment's start:
                // Shift+click first draws the chain, then a Shift-drag constrains from where the
                // user clicked.
                target_world,
            ));
            // The stroke always opens with a stamp at its anchor, so a click without a drag paints.
            centers.push(anchor_local);
            if let Some(from_local) = from_local {
                let carry = walk_segment(from_local, to_local, spacing, 0.0, &mut centers);
                if let Some(s) = self.stroke.as_mut() {
                    s.carry = carry;
                }
            }
            crate::trace_log!(
                cat::INPUT,
                "brush stroke_begin diameter={} hardness={:.2} erase={} at=({:.1},{:.1})",
                self.diameter,
                self.hardness,
                self.erase,
                target_world.x,
                target_world.y
            );
        }

        // The active layer or its size changed underneath the stroke: the buffer no longer
        // describes these pixels, so drop it rather than composite into the wrong image.
        let stale = self
            .stroke
            .as_ref()
            .is_none_or(|s| s.layer_id != layer.id || s.layer_size != layer_size);
        if stale {
            self.stroke = None;
            return None;
        }
        let stroke = self.stroke.as_mut()?;
        if !starting {
            let carry = walk_segment(stroke.last_local, to_local, spacing, stroke.carry, &mut centers);
            stroke.carry = carry;
        }
        stroke.last_local = to_local;
        if centers.is_empty() {
            return None;
        }

        // Snap a stamp centre so the tip lands on the pixel grid: a pixel CENTRE for an odd diameter
        // (diameter 1 = exactly one pixel), a pixel BOUNDARY for an even one (diameter 2 = a
        // symmetric 2x2). The walk itself always stays unsnapped so spacing is exact.
        //
        // WHICH stamps are snapped is a contract with the cursor. The stroke's OPENING stamp sits
        // on the pointer sample the cursor outline previews, so it is always snapped and a click
        // therefore paints exactly the outlined pixels. The stamps `walk_segment` interpolates sit
        // at arc-length positions the cursor makes no claim about, and are snapped only while the
        // tip is narrow enough for the grid contract above to depend on it
        // ([`SNAP_ALL_MAX_DIAMETER`]) — rounding them at any wider tip only quantizes the walk and
        // ripples thin diagonal strokes.
        let mut touched: Option<PixelBox> = None;
        for (index, center) in centers.iter().enumerate() {
            let opening = starting && index == 0;
            let snapped = if snap_interpolated || opening {
                Pos2::new(
                    snap_center(center.x, params.odd_diameter),
                    snap_center(center.y, params.odd_diameter),
                )
            } else {
                *center
            };
            if let Some(bbox) = stroke.stamp(layer, selection, map, &params, snapped) {
                touched = Some(match touched {
                    Some(t) => t.union(bbox),
                    None => bbox,
                });
            }
        }
        // The last stamp's PAGE position seeds the next Shift+click chain, but ONLY when Shift was
        // held while it was placed: a plain stroke must leave no anchor behind, so this assignment
        // is also what clears one. It is written to `self` after the composite, which still holds
        // the `stroke` borrow.
        let last_stamp_world = centers
            .last()
            .filter(|_| shift)
            .map(|last| map.to_world(last.x, last.y));

        let touched = touched?;
        stroke.composite(layer, touched, self.color, self.erase);
        self.last_stamp_world = last_stamp_world;
        Some(DirtyRect {
            min_x: touched.x0 as usize,
            min_y: touched.y0 as usize,
            max_x: touched.x1 as usize,
            max_y: touched.y1 as usize,
        })
    }
}

impl BrushTool {
    /// Draws the pixel-exact cursor outline, or returns false when this frame does not qualify.
    ///
    /// It qualifies only when all three hold: the view shows at least [`PIXEL_OUTLINE_MIN_SCALE`]
    /// screen px per image px; the destination grid was captured (an axis-aligned, unscaled,
    /// pixel-editable layer — see `refresh_cursor_cache`); and the cached outline matches the
    /// current tip. The caller draws the smooth circle for every other case.
    ///
    /// The loop is emitted as alternating black/white [`egui::Shape::line_segment`]s rather than one
    /// `Shape::closed_line`: only `tessellate_line_segment` snaps a line to the physical pixel grid,
    /// and only for a run whose two endpoints compare EQUAL on one axis
    /// (`epaint-0.35.0/src/tessellator.rs:1654`). Both the cached offsets and
    /// `ViewTransform::world_to_screen` preserve that equality exactly, and `walk_dash_runs` splits
    /// a run without disturbing it, so every axis-aligned step lands on a crisp physical pixel.
    fn draw_pixel_outline(&self, painter: &egui::Painter, view: &ViewTransform, pointer: Pos2) -> bool {
        if view.zoom < PIXEL_OUTLINE_MIN_SCALE {
            return false;
        }
        let Some(origin) = self.cursor_grid_origin else {
            return false;
        };
        let Some(outline) = self.cursor_outline.as_ref() else {
            return false;
        };
        // A cache built for other parameters (the tab skipped `interact` while the size changed)
        // would draw the wrong pixels; the smooth circle is the honest answer until it catches up.
        if outline.key != self.outline_key() || outline.points.len() < 2 {
            return false;
        }

        // Where `paint_frame` would put the OPENING stamp for this pointer: into layer-local px,
        // snapped by diameter parity, back to page px. That stamp is snapped at every diameter,
        // which is what keeps this outline exact — and is why the snap rule of `paint_frame` may
        // not be relaxed for it. `origin` is an offset only, which is exactly why
        // `refresh_cursor_cache` refuses a rotated or scaled layer.
        let odd = self.diameter % 2 == 1;
        let local = pointer - origin;
        let center_world = Pos2::new(snap_center(local.x, odd), snap_center(local.y, odd)) + origin;

        let screen: Vec<Pos2> = outline
            .points
            .iter()
            .map(|offset| view.world_to_screen(center_world + *offset))
            .collect();

        // The painter is already clipped to the viewport; culling here only avoids building shapes
        // for the part of a large brush's outline that is off-screen at a high zoom.
        let clip = view.viewport_rect.expand(CURSOR_DASH_PX);
        let mut shapes: Vec<egui::Shape> = Vec::new();
        // Shared with the selection marquee: the dash phase must accumulate along the WHOLE loop,
        // or a staircase of one-pixel steps restarts the pattern every step and paints solid.
        crate::tabs::ps_editor::walk_dash_runs(&screen, CURSOR_DASH_PX, CURSOR_MAX_RUNS, |run| {
            if !clip.intersects(Rect::from_two_pos(run.from, run.to)) {
                return;
            }
            let color = if run.black { Color32::BLACK } else { Color32::WHITE };
            shapes.push(egui::Shape::line_segment([run.from, run.to], Stroke::new(1.0, color)));
        });
        painter.extend(shapes);
        true
    }

    /// Draws the diameter/hardness readout next to the cursor while the HUD gesture is running.
    ///
    /// `center` is the cursor position in screen px. Photoshop shows the same numbers for the same
    /// gesture; without them the drag is a guess, because the cursor circle alone cannot show
    /// hardness. The text is laid out first so its background can be sized to it.
    fn draw_hud_readout(&self, painter: &egui::Painter, center: Pos2) {
        let text = tf!(
            "ps_editor.tools.brush_hud_readout",
            diameter = self.diameter.to_string(),
            hardness = format!("{:.0}", self.hardness * 100.0)
        );
        let galley = painter.layout_no_wrap(text, egui::FontId::proportional(HUD_TEXT_SIZE), Color32::WHITE);
        let pos = center + Vec2::splat(HUD_TEXT_OFFSET_PX);
        let background = Rect::from_min_size(pos, galley.size()).expand(3.0);
        painter.rect_filled(background, egui::CornerRadius::same(3), Color32::from_black_alpha(210));
        painter.galley(pos, galley, Color32::WHITE);
    }
}

/// Draws one `0..=100 %` control for a fraction stored as `f32` in `0.0..=1.0`.
///
/// The project forbids `egui::Slider`/`DragValue` in product UI, so this wraps `WheelSlider` on an
/// integer percentage — which is also the unit Photoshop shows these three parameters in.
fn percent_slider(ui: &mut egui::Ui, value: &mut f32, label: &str) {
    let mut percent = (*value * 100.0).round() as i32;
    if ui
        .add(crate::widgets::WheelSlider::new(&mut percent, 0..=100).suffix(" %").text(label))
        .changed()
    {
        *value = (percent as f32 / 100.0).clamp(0.0, 1.0);
    }
}

/// An inclusive pixel box in layer-local coordinates, already clamped to the layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelBox {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl PixelBox {
    /// Smallest box containing both.
    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Every stroke-buffer tile coordinate this box overlaps, row by row.
    ///
    /// The box is inclusive, so a box entirely inside one tile yields exactly that tile.
    fn tiles(self) -> impl Iterator<Item = (i32, i32)> {
        (tile_of(self.y0)..=tile_of(self.y1))
            .flat_map(move |ty| (tile_of(self.x0)..=tile_of(self.x1)).map(move |tx| (tx, ty)))
    }

    /// This box intersected with the tile at `(tx, ty)`.
    ///
    /// Only meaningful for a tile the box actually overlaps (one yielded by [`PixelBox::tiles`]),
    /// where the result is non-empty by construction.
    fn clamped_to_tile(self, tx: i32, ty: i32) -> Self {
        Self {
            x0: self.x0.max(tx * STROKE_TILE),
            y0: self.y0.max(ty * STROKE_TILE),
            x1: self.x1.min(tx * STROKE_TILE + STROKE_TILE - 1),
            y1: self.y1.min(ty * STROKE_TILE + STROKE_TILE - 1),
        }
    }
}

/// Per-stamp geometry, bundled so the stamping helpers stay within argument limits.
#[derive(Debug, Clone, Copy)]
struct StampParams {
    /// Tip radius in layer-local px.
    radius: f32,
    /// Edge hardness in `0.0..=1.0`.
    hardness: f32,
    /// Maximum alpha the stroke may build up to.
    opacity: f32,
    /// Build-up rate per stamp.
    flow: f32,
    /// Whether the (local) diameter is odd, which decides the pixel-grid snap.
    odd_diameter: bool,
}

impl StrokeTile {
    /// Allocates the tile at `(tx, ty)` and snapshots the layer pixels (and the selection clip) it
    /// covers.
    ///
    /// The snapshot is taken NOW rather than at stroke start, which is correct because the stroke
    /// has never painted outside the tiles it already owns: every pixel entering the buffer is
    /// still pre-stroke. Pixels of the tile that fall outside the layer are left at their default
    /// and are never read — every stamp box is clamped to the layer first.
    fn snapshot(
        layer: &Layer,
        selection: Option<&Selection>,
        clipped: bool,
        map: LocalMap,
        layer_size: [usize; 2],
        (tx, ty): (i32, i32),
    ) -> Self {
        let side = STROKE_TILE;
        let mut orig = vec![Color32::TRANSPARENT; STROKE_TILE_AREA];
        // `clipped` — not `selection.is_some()` — decides whether this tile has a clip mask at all:
        // the answer was latched when the stroke began, so a selection cleared mid-stroke leaves the
        // remaining tiles clipped to an empty mask instead of silently un-clipping them.
        let mut clip = clipped.then(|| vec![false; STROKE_TILE_AREA]);
        let [lw, lh] = layer_size;
        for row in 0..side {
            let y = ty * side + row;
            let Some(y_idx) = layer_axis_index(y, lh) else {
                continue;
            };
            let tile_row = in_tile(row) * STROKE_TILE_SIDE;
            for col in 0..side {
                let x = tx * side + col;
                let Some(x_idx) = layer_axis_index(x, lw) else {
                    continue;
                };
                let idx = tile_row + in_tile(col);
                orig[idx] = layer.image.pixels[y_idx * lw + x_idx];
                if let (Some(dst), Some(sel)) = (clip.as_mut(), selection) {
                    // The clip is rasterized ONCE per tile instead of re-tested per overlapping
                    // stamp; the page-space round-trip is what makes a rotated layer clip correctly.
                    let world = map.to_world(x as f32 + 0.5, y as f32 + 0.5);
                    dst[idx] = match (page_pixel_index(world.x), page_pixel_index(world.y)) {
                        (Some(wx), Some(wy)) => sel.contains(wx, wy),
                        _ => false,
                    };
                }
            }
        }
        Self {
            alpha: vec![0.0f32; STROKE_TILE_AREA],
            orig,
            clip,
        }
    }
}

impl StrokeState {
    /// Allocates a stroke buffer that owns no tiles yet, anchored at `anchor_world`.
    ///
    /// Tiles appear lazily in `ensure_covers`, so starting a stroke on a large layer costs no
    /// allocation proportional to the layer.
    fn new(
        layer_id: LayerId,
        layer_size: [usize; 2],
        clipped: bool,
        anchor_local: Pos2,
        anchor_world: Pos2,
    ) -> Self {
        Self {
            layer_id,
            layer_size,
            tiles: HashMap::new(),
            clipped,
            carry: 0.0,
            last_local: anchor_local,
            anchor_world,
        }
    }

    /// Allocates every tile the box `want` touches that the stroke does not own yet.
    ///
    /// Nothing already accumulated is moved or copied — that is the whole point of the tiling — so
    /// the cost is proportional to the NEWLY covered area, never to the stroke's bounding box.
    fn ensure_covers(
        &mut self,
        layer: &Layer,
        selection: Option<&Selection>,
        map: LocalMap,
        want: PixelBox,
    ) {
        let (clipped, layer_size) = (self.clipped, self.layer_size);
        for ty in tile_of(want.y0)..=tile_of(want.y1) {
            for tx in tile_of(want.x0)..=tile_of(want.x1) {
                self.tiles.entry((tx, ty)).or_insert_with(|| {
                    StrokeTile::snapshot(layer, selection, clipped, map, layer_size, (tx, ty))
                });
            }
        }
    }

    /// Accumulates one stamp centred at layer-local `center` into the alpha buffer.
    ///
    /// `selection` and `map` are only needed when a new tile has to be allocated: the clip mask is
    /// rasterized once per tile rather than re-tested on every overlapping stamp.
    /// Returns the clamped box the stamp actually touched, or `None` when it misses the layer.
    fn stamp(
        &mut self,
        layer: &Layer,
        selection: Option<&Selection>,
        map: LocalMap,
        params: &StampParams,
        center: Pos2,
    ) -> Option<PixelBox> {
        let bbox = self.stamp_box(params, center)?;
        self.ensure_covers(layer, selection, map, bbox);
        // Walk tile by tile so the sparse map is consulted once per tile instead of once per pixel.
        for (tx, ty) in bbox.tiles() {
            let Some(tile) = self.tiles.get_mut(&(tx, ty)) else {
                // `ensure_covers` above allocated every tile of `bbox`; this arm is unreachable.
                continue;
            };
            let sub = bbox.clamped_to_tile(tx, ty);
            for y in sub.y0..=sub.y1 {
                let dy = (y as f32 + 0.5) - center.y;
                let tile_row = in_tile(y) * STROKE_TILE_SIDE;
                for x in sub.x0..=sub.x1 {
                    let dx = (x as f32 + 0.5) - center.x;
                    let r = (dx * dx + dy * dy).sqrt();
                    let coverage = stamp_coverage(r, params.radius, params.hardness);
                    if coverage <= 0.0 {
                        continue;
                    }
                    let idx = tile_row + in_tile(x);
                    if tile.clip.as_ref().is_some_and(|c| !c[idx]) {
                        continue;
                    }
                    tile.alpha[idx] =
                        accumulate_alpha(tile.alpha[idx], coverage, params.flow, params.opacity);
                }
            }
        }
        Some(bbox)
    }

    /// The layer-clamped pixel box one stamp at `center` can touch, or `None` when it misses the
    /// layer entirely.
    fn stamp_box(&self, params: &StampParams, center: Pos2) -> Option<PixelBox> {
        // Coverage is zero beyond `radius + 0.5`; one extra pixel absorbs the f32 rounding.
        let ext = params.radius + 1.5;
        // A non-finite centre or radius has no box at all, and must be rejected HERE: `as i32` maps
        // a NaN to 0, which would stamp the layer's top-left pixel instead of nothing.
        if !center.x.is_finite() || !center.y.is_finite() || !ext.is_finite() {
            return None;
        }
        // Layer dimensions come from a `ColorImage`, so they fit an `i32` with room to spare; a
        // huge stamp centre saturates the conversion and is then rejected by the emptiness test
        // below rather than wrapping into the layer.
        let lw = i32::try_from(self.layer_size[0]).unwrap_or(i32::MAX);
        let lh = i32::try_from(self.layer_size[1]).unwrap_or(i32::MAX);
        let bbox = PixelBox {
            x0: (center.x - ext).floor().max(0.0) as i32,
            y0: (center.y - ext).floor().max(0.0) as i32,
            x1: ((center.x + ext).ceil().max(0.0) as i32).min(lw - 1),
            y1: ((center.y + ext).ceil().max(0.0) as i32).min(lh - 1),
        };
        (bbox.x1 >= bbox.x0 && bbox.y1 >= bbox.y0).then_some(bbox)
    }

    /// Re-composites `region` from the pre-stroke snapshot and the accumulated alpha into
    /// `layer.image`, in premultiplied space.
    ///
    /// Only the pixels whose alpha changed this frame are rewritten, which is exactly the rect the
    /// tool reports as dirty — so the tab's stroke union (its undo bound) stays complete.
    fn composite(&self, layer: &mut Layer, region: PixelBox, color: Color32, erase: bool) {
        let lw = self.layer_size[0];
        for (tx, ty) in region.tiles() {
            let Some(tile) = self.tiles.get(&(tx, ty)) else {
                continue;
            };
            let sub = region.clamped_to_tile(tx, ty);
            for y in sub.y0..=sub.y1 {
                let tile_row = in_tile(y) * STROKE_TILE_SIDE;
                // `region` was clamped to the layer when the stamp box was built, so both indices
                // are non-negative and inside the image.
                let image_row = y as usize * lw;
                for x in sub.x0..=sub.x1 {
                    let idx = tile_row + in_tile(x);
                    let alpha = tile.alpha[idx];
                    if alpha <= 0.0 {
                        continue;
                    }
                    layer.image.pixels[image_row + x as usize] =
                        composite_pixel(tile.orig[idx], alpha, color, erase);
                }
            }
        }
    }
}

/// Tile coordinate owning the layer-local pixel coordinate `v`, on either axis.
fn tile_of(v: i32) -> i32 {
    v.div_euclid(STROKE_TILE)
}

/// Offset of a layer-local pixel coordinate inside its own tile, on either axis.
///
/// `rem_euclid` is non-negative and smaller than [`STROKE_TILE`], so the conversion is exact.
fn in_tile(v: i32) -> usize {
    v.rem_euclid(STROKE_TILE) as usize
}

/// Index of a layer coordinate `v` on an axis of length `len`, or `None` when it is outside.
///
/// Shared by the tile snapshot's two axes so an out-of-layer pixel is skipped once, by name. The
/// conversion is checked rather than cast: a tile overlapping the layer's top or left edge holds
/// NEGATIVE layer coordinates, which an `as usize` would turn into a huge in-range-looking index.
fn layer_axis_index(v: i32, len: usize) -> Option<usize> {
    usize::try_from(v).ok().filter(|idx| *idx < len)
}

/// Stamp spacing in layer-local px for a tip of `d_local` layer-local px, and whether the walk's
/// INTERPOLATED stamps are snapped to the pixel grid.
///
/// The two answers belong together: the spacing floor depends on whether a sub-pixel step can move
/// a stamp at all (see [`MIN_SPACING_SNAPPED`]). Both the painting path and the ripple tests ask
/// this function rather than re-deriving the rule, for the same reason the cursor outline asks
/// [`stamp_coverage`] instead of copying the formula.
fn stamp_step(d_local: f32) -> (f32, bool) {
    let snap_interpolated = d_local <= SNAP_ALL_MAX_DIAMETER;
    let floor = if snap_interpolated { MIN_SPACING_SNAPPED } else { MIN_SPACING };
    ((d_local * SPACING_RATIO).max(floor), snap_interpolated)
}

/// Snaps a stamp centre coordinate to the pixel grid: a pixel CENTRE for an odd diameter, a pixel
/// BOUNDARY for an even one.
///
/// This is what makes diameter 1 paint exactly one pixel and diameter 2 a symmetric 2x2: without
/// it a diameter-1 tip landing on a boundary would split its coverage over two pixels.
///
/// Which stamps it is applied to is decided in one place, `paint_frame`, and mirrored by the cursor
/// outline: every stamp of a tip up to [`SNAP_ALL_MAX_DIAMETER`], and otherwise only the stroke's
/// opening stamp — the one the cursor previews.
fn snap_center(v: f32, odd_diameter: bool) -> f32 {
    if odd_diameter { v.floor() + 0.5 } else { v.round() }
}

/// Coverage of one pixel by one stamp: the brush's ONE hardness/antialiasing formula.
///
/// `r` is the distance from the stamp centre to the PIXEL CENTRE, `radius` the tip radius and
/// `hardness` its edge falloff, all in the destination pixel grid's units. The tip's outer extent
/// is `radius + 0.5` — half a pixel outside the nominal radius, so a diameter-1 tip covers its own
/// pixel completely and nothing else. Returns `0.0..=1.0`; no supersampling is needed or done.
///
/// The profile is the smaller of two terms, and both are load-bearing:
///
/// * `core = 1 - (r / extent)^k` with `k = 1 / (1 - hardness)`. It is PLATEAU-FREE: below
///   hardness 1 it descends from the very centre, so consecutive stamps always overlap in a region
///   where coverage still varies. A profile with a flat full-coverage core instead hands each
///   centreline pixel an INTEGER number of saturated stamps, and that integer flips by one as the
///   walk phase slides — the periodic scallop this kernel exists to avoid (see [`SPACING_RATIO`]).
/// * `edge = clamp(extent - r, 0, 1)` is the antialiasing: a one-pixel rim that no hardness can
///   remove. It is also what makes hardness 1 well defined — the exponent is floored by
///   [`HARDNESS_EPS`] rather than infinite, and the core term then degenerates to a step at
///   `extent`, leaving `edge` alone to shape the rim.
///
/// Monotonically non-increasing in `r`, zero at and beyond `radius + 0.5`, and finite for every
/// `hardness` in `0.0..=1.0` and every radius the tool can produce.
fn stamp_coverage(r: f32, radius: f32, hardness: f32) -> f32 {
    let extent = radius + 0.5;
    // Outside the tip there is nothing to compute. The early exit also keeps the ratio below 1, so
    // the power cannot overflow at the large exponents a near-hard tip produces, and it is the
    // only place a non-finite input can enter — a NaN would otherwise survive `powf` and both
    // `clamp` and `min` would propagate it into the alpha buffer.
    if !r.is_finite() || !extent.is_finite() || r >= extent {
        return 0.0;
    }
    let k = 1.0 / (1.0 - hardness.clamp(0.0, 1.0)).max(HARDNESS_EPS);
    let core = 1.0 - (r.max(0.0) / extent).powf(k);
    let edge = (extent - r).clamp(0.0, 1.0);
    core.clamp(0.0, 1.0).min(edge)
}

/// Photoshop's flow build-up: one stamp moves the accumulated alpha a `flow * coverage` fraction of
/// the way toward `opacity`, and never past it.
///
/// This is what makes a self-crossing stroke NOT darken where it overlaps: once the buffer reaches
/// `opacity`, further stamps add nothing. Returns `0.0..=1.0`.
fn accumulate_alpha(current: f32, coverage: f32, flow: f32, opacity: f32) -> f32 {
    if opacity <= current {
        return current;
    }
    (current + flow * coverage * (opacity - current)).min(opacity)
}

/// Composites one stroke pixel over its pre-stroke value, in PREMULTIPLIED space.
///
/// `orig` is the pre-stroke pixel, `alpha` the stroke's accumulated coverage in `0.0..=1.0`, and
/// `color` the brush colour (its own alpha is ignored — the stroke's alpha is `alpha`). Painting is
/// source-over; erasing scales all four premultiplied channels down, which is the same operation
/// with a transparent source.
fn composite_pixel(orig: Color32, alpha: f32, color: Color32, erase: bool) -> Color32 {
    let a = alpha.clamp(0.0, 1.0);
    let inv = 1.0 - a;
    // `orig` and the result are both premultiplied, so each channel scales independently.
    let keep = |c: u8| (c as f32) * inv;
    if erase {
        return Color32::from_rgba_premultiplied(
            keep(orig.r()).round() as u8,
            keep(orig.g()).round() as u8,
            keep(orig.b()).round() as u8,
            keep(orig.a()).round() as u8,
        );
    }
    // Premultiplying an opaque brush colour by `a` is simply `channel * a`, alpha included.
    let add = |c: u8| (c as f32) * a;
    Color32::from_rgba_premultiplied(
        (keep(orig.r()) + add(color.r())).round().clamp(0.0, 255.0) as u8,
        (keep(orig.g()) + add(color.g())).round().clamp(0.0, 255.0) as u8,
        (keep(orig.b()) + add(color.b())).round().clamp(0.0, 255.0) as u8,
        (keep(orig.a()) + 255.0 * a).round().clamp(0.0, 255.0) as u8,
    )
}

/// Places stamp centres along `from`→`to` at constant arc length `spacing`, appending them to
/// `out`, and returns the leftover distance to carry into the next segment.
///
/// `carry` is the distance already walked since the last stamp; carrying it ACROSS frames is what
/// keeps spacing uniform when the pointer is sampled irregularly. The returned carry is always in
/// `0.0..spacing`, and `from` itself is never stamped — the position it holds was already accounted
/// for by whoever produced the incoming carry (the stroke's opening stamp, or the previous
/// segment's walk).
///
/// An incoming carry may nonetheless be `>= spacing`, because `spacing` follows the diameter and
/// `[` / the wheel can shrink it MID-STROKE. Stamping the overdue remainder at distance zero would
/// place a stamp on `from` and give that one position two flow steps, so the phase is restarted
/// there instead: the next stamp lands one full (new) spacing along.
fn walk_segment(from: Pos2, to: Pos2, spacing: f32, carry: f32, out: &mut Vec<Pos2>) -> f32 {
    let seg = to - from;
    let len = seg.length();
    if !len.is_finite() || len <= f32::EPSILON || spacing <= 0.0 {
        return carry;
    }
    let dir = seg / len;
    let mut next = spacing - carry;
    if next <= 0.0 {
        next = spacing;
    }
    while next <= len {
        out.push(from + dir * next);
        next += spacing;
    }
    len - (next - spacing)
}

/// Projects `pointer` onto the horizontal or vertical line through `anchor`, whichever axis the
/// movement is dominant on.
///
/// Photoshop's Shift-constrained brush drag offers exactly these two axes — no 45° diagonal.
fn constrain_to_axis(anchor: Pos2, pointer: Pos2) -> Pos2 {
    let d = pointer - anchor;
    if d.x.abs() >= d.y.abs() {
        Pos2::new(pointer.x, anchor.y)
    } else {
        Pos2::new(anchor.x, pointer.y)
    }
}

/// Photoshop's non-linear diameter step for a value inside a band.
///
/// The bands are the ones Photoshop uses for `[` / `]`: 1 px below 10, then 5, 10, 25, 50 and 100.
fn diameter_step(d: u32) -> u32 {
    match d {
        0..=9 => 1,
        10..=49 => 5,
        50..=99 => 10,
        100..=199 => 25,
        200..=299 => 50,
        _ => 100,
    }
}

/// Next larger diameter on the `[` / `]` grid, clamped to [`MAX_DIAMETER`].
///
/// The result is always strictly greater than `d` (until the cap) and snapped to a multiple of the
/// current band's step, so repeated presses walk 10, 15, … 50, 60, … and never land off-grid.
fn diameter_step_up(d: u32) -> u32 {
    let d = d.clamp(MIN_DIAMETER, MAX_DIAMETER);
    let step = diameter_step(d);
    ((d / step + 1) * step).min(MAX_DIAMETER)
}

/// Next smaller diameter on the `[` / `]` grid, clamped to [`MIN_DIAMETER`].
///
/// The band is chosen from `d - 1`, not from `d`: at a band boundary the step that BROUGHT us here
/// is the one that must take us back, so stepping down from 50 lands on 45 (the 5-band) and not on
/// 40. That is what makes `diameter_step_down(diameter_step_up(d)) == d` for every on-grid `d`.
fn diameter_step_down(d: u32) -> u32 {
    let d = d.clamp(MIN_DIAMETER, MAX_DIAMETER);
    if d <= MIN_DIAMETER {
        return MIN_DIAMETER;
    }
    let step = diameter_step(d - 1);
    (((d - 1) / step) * step).max(MIN_DIAMETER)
}

/// Rounds a floating diameter to the nearest whole px inside `MIN_DIAMETER..=MAX_DIAMETER`.
///
/// The clamp happens BEFORE the integer conversion, which is what makes that conversion exact (§17
/// allows an `as` only when truncation is impossible): an unclamped `as u32` on a HUD drag's
/// accumulated float would saturate silently. A non-finite value — a NaN pointer delta — cannot be
/// ordered by `clamp`, so it is rejected up front and falls back to the smallest diameter.
fn round_diameter(value: f32) -> u32 {
    if !value.is_finite() {
        return MIN_DIAMETER;
    }
    // Both bounds are small integers, so widening them to f32 is exact.
    let bounded = value.round().clamp(MIN_DIAMETER as f32, MAX_DIAMETER as f32);
    bounded as u32
}

/// Next hardness on the 0/25/50/75/100 % grid, in `0.0..=1.0`.
fn step_hardness_value(hardness: f32, up: bool) -> f32 {
    let steps = hardness.clamp(0.0, 1.0) / HARDNESS_STEP;
    let next = if up { steps.floor() + 1.0 } else { steps.ceil() - 1.0 };
    (next * HARDNESS_STEP).clamp(0.0, 1.0)
}

/// Radius at which [`stamp_coverage`] equals 0.5, for a tip of `diameter` px and `hardness`.
///
/// Both terms of the kernel are inverted in closed form and the smaller contour wins, because
/// coverage is their minimum: the core term passes 0.5 at `extent * 0.5^(1 - hardness)`, the rim
/// ramp at `extent - 0.5`. This is what the cursor ring draws — for a hard tip the nominal radius,
/// for a soft one well inside the tip's outer extent. The `0.5` floor keeps a sub-pixel tip's ring
/// visible; it is inert for every diameter the tool can hold ([`MIN_DIAMETER`] and up).
fn coverage_radius_50(diameter: f32, hardness: f32) -> f32 {
    let extent = diameter.max(0.0) * 0.5 + 0.5;
    let core = extent * 0.5f32.powf(1.0 - hardness.clamp(0.0, 1.0));
    core.min(extent - 0.5).max(0.5)
}

/// Inclusive x-runs of the pixels one stamp covers at 50 % or more, one entry per row.
///
/// Coverage is asked of [`stamp_coverage`] — the kernel the painting path itself uses — and the
/// stamp centre is snapped by [`snap_center`] exactly as `paint_frame` snaps it. Neither may be
/// re-derived here: a cursor drawn from its own copy of the formula eventually marks pixels the
/// brush does not paint, which is the failure this outline exists to prevent.
///
/// Pixel indices are absolute in a frame whose stamp centre sits at `snap_center(0.0, odd)`: pixel
/// `i` spans `i..i + 1` and has its centre at `i + 0.5`. Returns the top row's index together with
/// one run per row from the top down.
///
/// Returns `None` when the covered set has no single-loop boundary — a row with a horizontal gap,
/// or a covered row below an empty one. That cannot happen for a kernel decreasing with the
/// distance to the centre (`stamp_coverage` is the minimum of two functions that both decrease in
/// `r`), but it is checked rather than assumed, because the caller's single loop would otherwise
/// enclose pixels the brush never touches.
fn coverage_rows(diameter: u32, hardness: f32) -> Option<(i32, Vec<(i32, i32)>)> {
    let radius = diameter.max(MIN_DIAMETER) as f32 * 0.5;
    let center = snap_center(0.0, diameter % 2 == 1);
    // Coverage is zero beyond `radius + 0.5`; the extra pixel absorbs the f32 rounding, the same
    // padding `StrokeState::stamp` puts on its own bounding box.
    let ext = radius + 1.5;
    let lo = (center - ext).floor() as i32;
    let hi = (center + ext).ceil() as i32;

    let mut rows: Vec<(i32, i32)> = Vec::new();
    let mut top: Option<i32> = None;
    let mut ended = false;
    for y in lo..=hi {
        let dy = (y as f32 + 0.5) - center;
        let mut run: Option<(i32, i32)> = None;
        for x in lo..=hi {
            let dx = (x as f32 + 0.5) - center;
            if stamp_coverage((dx * dx + dy * dy).sqrt(), radius, hardness) < 0.5 {
                continue;
            }
            run = Some(match run {
                Some((first, last)) => {
                    // A covered pixel that does not continue the run means a hole in the row.
                    if x != last + 1 {
                        return None;
                    }
                    (first, x)
                }
                None => (x, x),
            });
        }
        match run {
            Some(r) => {
                if ended {
                    return None;
                }
                if top.is_none() {
                    top = Some(y);
                }
                rows.push(r);
            }
            None => ended = top.is_some(),
        }
    }
    Some((top?, rows))
}

/// Closed loop of pixel corners enclosing the pixels one stamp covers at 50 % or more.
///
/// Vertices are integer pixel corners in the frame [`coverage_rows`] defines, wound clockwise for a
/// downward `y` axis, collinear edges merged, the first vertex repeated as the last — the same
/// shape `Selection::outline_loops` produces for the marquee, so the same dash walker can draw it.
/// Empty when the covered set has no single-loop boundary.
fn coverage_outline_corners(diameter: u32, hardness: f32) -> Vec<(i32, i32)> {
    let Some((top, rows)) = coverage_rows(diameter, hardness) else {
        return Vec::new();
    };
    let Some(&(first_x0, first_x1)) = rows.first() else {
        return Vec::new();
    };
    let Ok(span) = i32::try_from(rows.len() - 1) else {
        return Vec::new();
    };
    let bottom = top + span;
    let run = |y: i32| rows[(y - top) as usize];

    let mut pts: Vec<(i32, i32)> = Vec::new();
    // Top edge of the first row, left to right.
    pts.push((first_x0, top));
    pts.push((first_x1 + 1, top));
    // Down the right side: each row's right edge, then a horizontal jog to where the next row's run
    // ends. A jog of zero length degenerates into a repeated vertex, which the merge removes.
    for y in top..=bottom {
        pts.push((run(y).1 + 1, y + 1));
        if y < bottom {
            pts.push((run(y + 1).1 + 1, y + 1));
        }
    }
    // Bottom edge of the last row, right to left.
    pts.push((run(bottom).0, bottom + 1));
    // Up the left side, mirroring the right-hand walk. Its final vertex is the starting one again,
    // so the loop closes itself and no explicit closing point is pushed here.
    for y in (top..=bottom).rev() {
        pts.push((run(y).0, y));
        if y > top {
            pts.push((run(y - 1).0, y));
        }
    }
    merge_collinear_closed(&pts)
}

/// Drops repeated vertices and vertices where a closed axis-aligned path keeps its direction.
///
/// `path` must be closed (first vertex repeated as last) and made of axis-aligned steps; the result
/// is closed too, or empty for a degenerate path. Merging is what keeps the outline cheap and clean:
/// a 10x10 block becomes five vertices instead of forty-one, and a 400 px brush a few hundred
/// segments instead of ~1600.
fn merge_collinear_closed(path: &[(i32, i32)]) -> Vec<(i32, i32)> {
    // Drop consecutive duplicates (the zero-length jogs above) and the closing repeat, so the walk
    // below sees only real steps.
    let mut open: Vec<(i32, i32)> = Vec::with_capacity(path.len());
    for &p in path {
        if open.last() != Some(&p) {
            open.push(p);
        }
    }
    if open.first() == open.last() {
        open.pop();
    }
    if open.len() < 3 {
        return Vec::new();
    }
    // Unit step of an edge. Exact for integer, axis-aligned pixel-boundary edges, so "same
    // direction" is an integer comparison rather than a float one.
    let step = |from: (i32, i32), to: (i32, i32)| ((to.0 - from.0).signum(), (to.1 - from.1).signum());
    let n = open.len();
    let mut merged: Vec<(i32, i32)> = Vec::new();
    for i in 0..n {
        let cur = open[i];
        // The wrap-around matters: the walk may have started in the middle of a straight run, and
        // that vertex is not a corner either.
        if step(open[(i + n - 1) % n], cur) != step(cur, open[(i + 1) % n]) {
            merged.push(cur);
        }
    }
    let Some(&first) = merged.first() else {
        return Vec::new();
    };
    merged.push(first);
    merged
}

/// Pixel-exact outline of the pixels one stamp of `diameter`/`hardness` would cover at 50 % or more.
///
/// Returns one closed loop of pixel-corner OFFSETS from the snapped stamp centre, in image px, the
/// first point repeated as the last; empty when no single-loop outline exists (see
/// [`coverage_rows`]). The contour is the same one the smooth fallback ring draws
/// ([`coverage_radius_50`]) and the one Photoshop's brush circle marks — only resolved per pixel.
///
/// Pure and GUI-free. Costs O(diameter²) [`stamp_coverage`] evaluations (~160 k at
/// [`MAX_DIAMETER`]), paid only when the diameter or the hardness CHANGES — so an idle frame, a pan
/// or a plain drag never pays for it; see `BrushTool::refresh_cursor_cache`. The one gesture that
/// does change a parameter every frame is the Alt + right-drag HUD, which therefore rebuilds once
/// per frame while it runs (measured at ~0.3 ms release / ~1.9 ms debug at [`MAX_DIAMETER`]).
#[must_use]
fn build_coverage_outline(diameter: u32, hardness: f32) -> Vec<Vec2> {
    let center = snap_center(0.0, diameter % 2 == 1);
    coverage_outline_corners(diameter, hardness)
        .into_iter()
        .map(|(x, y)| Vec2::new(x as f32 - center, y as f32 - center))
        .collect()
}

/// Maps between a layer's local pixel space and page (world) space, captured by value so it can be
/// used while the layer image is borrowed mutably.
#[derive(Clone, Copy, Debug)]
struct LocalMap {
    center: Vec2,
    rotation: f32,
    scale: f32,
    half: Vec2,
}

impl LocalMap {
    fn from_layer(layer: &Layer) -> Self {
        let scale = if layer.transform.scale.abs() < f32::EPSILON {
            f32::EPSILON
        } else {
            layer.transform.scale
        };
        Self {
            center: layer.transform.center,
            rotation: layer.transform.rotation,
            scale,
            half: layer.image_size() * 0.5,
        }
    }

    fn to_local(self, world: Pos2) -> Pos2 {
        (self.half + rotate(world - self.center.to_pos2(), -self.rotation) / self.scale).to_pos2()
    }

    fn to_world(self, local_x: f32, local_y: f32) -> Pos2 {
        (self.center + rotate(Vec2::new(local_x, local_y) - self.half, self.rotation) * self.scale)
            .to_pos2()
    }
}

fn rotate(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::ps_editor::layers::LayerStack;
    use egui::ColorImage;
    use std::collections::BTreeSet;

    /// Page size of the fixture, in px. Large enough that a small brush stamp lands well inside it.
    const PAGE: [usize; 2] = [64, 64];

    /// A page with the two locked base layers plus one blank, directly editable raster on top,
    /// which `add_raster_layer` also makes active — the layer the brush is allowed to paint.
    fn stack_with_blank_raster() -> LayerStack {
        let mut stack = LayerStack::new(
            0,
            PAGE,
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
            ColorImage::filled(PAGE, Color32::TRANSPARENT),
        );
        stack.add_raster_layer();
        stack
    }

    /// A brush with a small diameter, so one stamp stays inside the fixture page and two stamps at
    /// different positions cover measurably different pixel counts.
    fn small_brush() -> BrushTool {
        let mut tool = BrushTool::default();
        tool.set_diameter(5);
        tool
    }

    /// Number of non-transparent pixels of the stack's active raster.
    fn painted_pixels(stack: &LayerStack) -> usize {
        stack.layer(stack.active_id()).map_or(0, |layer| {
            layer.image.pixels.iter().filter(|px| px.a() != 0).count()
        })
    }

    /// Runs ONE frame of `interact` with the pointer state the tab would hand the tool.
    ///
    /// `primary_pressed` is deliberately masked with `in_viewport`: `draw_canvas` passes
    /// `input.primary_pressed && pointer_in_viewport` (`../mod.rs`), so a press landing on a
    /// floating panel reaches the tool with `primary_pressed == false`. Letting a test set that
    /// combination freely would let it assert a state the tab cannot produce.
    fn frame(
        tool: &mut BrushTool,
        stack: &mut LayerStack,
        pointer: Pos2,
        in_viewport: bool,
        pressed: bool,
        down: bool,
    ) -> ToolOutcome {
        frame_with(tool, stack, pointer, in_viewport, pressed, down, egui::Modifiers::default())
    }

    /// `frame` with explicit keyboard modifiers.
    fn frame_with(
        tool: &mut BrushTool,
        stack: &mut LayerStack,
        pointer: Pos2,
        in_viewport: bool,
        pressed: bool,
        down: bool,
        modifiers: egui::Modifiers,
    ) -> ToolOutcome {
        let mut selection = None;
        let page_size = stack.size();
        let mut ctx = PsToolContext {
            page_size,
            pointer_image: Some(pointer),
            pointer_in_viewport: in_viewport,
            primary_pressed: pressed && in_viewport,
            primary_down: down,
            primary_released: false,
            secondary_down: false,
            pointer_delta: Vec2::ZERO,
            modifiers,
            cancel_pressed: false,
            remove_point_pressed: false,
            // Identity view: screen and world coordinates coincide, so the fixture can name pixels.
            view: ViewTransform {
                viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 64.0)),
                zoom: 1.0,
                center_world: Vec2::new(32.0, 32.0),
            },
            stack,
            selection: &mut selection,
        };
        tool.interact(&mut ctx)
    }

    /// One `interact` frame with an explicit page selection, so the per-tile clip mask is
    /// exercised. `frame` passes no selection, which would leave the clip path untested.
    fn frame_with_selection(
        tool: &mut BrushTool,
        stack: &mut LayerStack,
        selection: &mut Option<Selection>,
        pointer: Pos2,
        pressed: bool,
        down: bool,
    ) -> ToolOutcome {
        let page_size = stack.size();
        let mut ctx = PsToolContext {
            page_size,
            pointer_image: Some(pointer),
            pointer_in_viewport: true,
            primary_pressed: pressed,
            primary_down: down,
            primary_released: false,
            secondary_down: false,
            pointer_delta: Vec2::ZERO,
            modifiers: egui::Modifiers::default(),
            cancel_pressed: false,
            remove_point_pressed: false,
            view: ViewTransform {
                viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 64.0)),
                zoom: 1.0,
                center_world: Vec2::new(32.0, 32.0),
            },
            stack,
            selection,
        };
        tool.interact(&mut ctx)
    }

    /// Alpha of the active raster at a page pixel (the fixture uses an identity layer transform).
    fn alpha_at(stack: &LayerStack, x: usize, y: usize) -> u8 {
        stack
            .layer(stack.active_id())
            .map_or(0, |l| l.image.pixels[y * l.image.size[0] + x].a())
    }

    // ---- the coverage formula (D2) ----

    /// A one-pixel brush must paint exactly one pixel: full coverage at its own pixel centre and
    /// none at any 4-neighbour. This is the per-pixel-work requirement the whole geometry exists
    /// for, so it is asserted on the formula, not only through the paint path.
    #[test]
    fn a_one_pixel_hard_tip_covers_its_own_pixel_and_nothing_else() {
        let radius = 0.5;
        assert!((stamp_coverage(0.0, radius, 1.0) - 1.0).abs() < 1e-6, "the centre pixel is full");
        assert_eq!(stamp_coverage(1.0, radius, 1.0), 0.0, "a 4-neighbour is untouched");
        assert_eq!(stamp_coverage(2.0_f32.sqrt(), radius, 1.0), 0.0, "a diagonal neighbour too");
    }

    /// A hard tip is not aliased: its rim spans about one pixel, so coverage falls from full to
    /// zero across the radius rather than switching at it.
    #[test]
    fn a_large_hard_tip_has_a_one_pixel_soft_rim() {
        let radius = 20.0;
        assert!((stamp_coverage(19.0, radius, 1.0) - 1.0).abs() < 1e-6, "well inside is full");
        assert_eq!(stamp_coverage(21.0, radius, 1.0), 0.0, "well outside is empty");
        let rim = stamp_coverage(20.0, radius, 1.0);
        assert!(rim > 0.0 && rim < 1.0, "the rim pixel is partially covered, got {rim}");
    }

    /// Hardness widens the falloff: at half the radius a soft tip is already fading while a hard
    /// one is still solid.
    #[test]
    fn hardness_controls_the_falloff_width() {
        let radius = 20.0;
        assert!((stamp_coverage(10.0, radius, 1.0) - 1.0).abs() < 1e-6);
        let soft = stamp_coverage(10.0, radius, 0.0);
        assert!(soft > 0.0 && soft < 1.0, "a soft tip fades well inside its radius, got {soft}");
        assert!(stamp_coverage(10.0, radius, 0.5) > soft, "more hardness means more coverage");
    }

    /// Below full hardness the tip has NO flat core: coverage falls strictly from a quarter of the
    /// tip's extent all the way out. A plateau is what beads a low-flow stroke — the previous
    /// formula held coverage at exactly 1.0 out to `hardness` times the radius, so consecutive
    /// stamps handed a centreline pixel an INTEGER number of saturated steps that flipped with the
    /// walk phase — so its absence is a contract, not a side effect of the current formula.
    ///
    /// The assertion starts at a quarter of the extent because f32 cannot resolve the descent
    /// nearer the centre at a high hardness: `(r / extent)^k` underflows the mantissa there, which
    /// is a representation limit and not a plateau. What the stroke actually needs is bounded by
    /// `the_stroke_centreline_has_no_periodic_ripple`, which measures the accumulation itself.
    #[test]
    fn the_falloff_has_no_flat_core_below_full_hardness() {
        let radius = 50.0;
        let extent = radius + 0.5;
        for hardness in [0.0, 0.25, 0.5, 0.75, 0.9] {
            assert!(
                stamp_coverage(extent * 0.5, radius, hardness) < 1.0,
                "hardness {hardness} is still saturated halfway out — that is the old plateau"
            );
            let mut previous = f32::INFINITY;
            for step in 5..=20 {
                let r = extent * step as f32 / 20.0;
                let current = stamp_coverage(r, radius, hardness);
                assert!(
                    current < previous,
                    "hardness {hardness} is flat approaching r={r}: {previous} -> {current}"
                );
                previous = current;
            }
        }
    }

    /// The kernel is total: for every tip the tool can produce and every hardness, coverage stays a
    /// finite `0.0..=1.0`, never rises with `r`, and is exactly zero at and beyond `radius + 0.5` —
    /// including hardness 1, where the falloff exponent is at its [`HARDNESS_EPS`] ceiling.
    #[test]
    fn the_falloff_is_total_over_every_tip_and_hardness() {
        for diameter in [MIN_DIAMETER, 2, 3, 5, 17, 100, MAX_DIAMETER] {
            let radius = diameter as f32 * 0.5;
            let extent = radius + 0.5;
            for hardness in [0.0, 0.1, 0.5, 0.75, 0.9, 0.999, 1.0] {
                let mut previous = f32::INFINITY;
                // 400 steps across the tip, then well past its extent.
                for step in 0..=440 {
                    let r = extent * step as f32 / 400.0;
                    let coverage = stamp_coverage(r, radius, hardness);
                    assert!(
                        coverage.is_finite() && (0.0..=1.0).contains(&coverage),
                        "d={diameter} h={hardness} r={r} gave {coverage}"
                    );
                    assert!(coverage <= previous, "d={diameter} h={hardness} rose at r={r}");
                    if r >= extent {
                        assert_eq!(coverage, 0.0, "d={diameter} h={hardness} paints beyond {extent}");
                    }
                    previous = coverage;
                }
                assert!(
                    (stamp_coverage(0.0, radius, hardness) - 1.0).abs() < 1e-6,
                    "d={diameter} h={hardness} does not reach full coverage at its centre"
                );
            }
        }
    }

    /// [`coverage_radius_50`] is the kernel's own 50 % contour, not a second formula that may drift
    /// from it: the cursor's smooth fallback ring and the pixel-exact outline would otherwise
    /// disagree about the same tip.
    #[test]
    fn the_fifty_percent_radius_matches_the_kernel() {
        for diameter in [1.0, 2.0, 5.0, 17.0, 100.0, 400.0] {
            for hardness in [0.0, 0.2, 0.5, 0.75, 0.9, 1.0] {
                let r50 = coverage_radius_50(diameter, hardness);
                let radius = diameter * 0.5;
                let delta = 1e-3 * diameter;
                let inside = stamp_coverage(r50 - delta, radius, hardness);
                let outside = stamp_coverage(r50 + delta, radius, hardness);
                assert!(inside >= 0.5, "d={diameter} h={hardness}: inside r50 gave {inside}");
                assert!(outside < 0.5, "d={diameter} h={hardness}: outside r50 gave {outside}");
            }
        }
    }

    // ---- the stroke's centreline profile ----

    /// Stamp centres of a straight stroke, produced by the REAL walk and the REAL snap policy.
    ///
    /// `dir` must be a unit vector. The stroke is split into IRREGULAR frames, as a dragged pointer
    /// is, so the carry that keeps spacing uniform across frames is exercised too. The opening stamp
    /// is snapped at every diameter and the interpolated ones only up to
    /// [`SNAP_ALL_MAX_DIAMETER`] — the same rule `paint_frame` applies.
    fn stroke_centers(diameter: f32, start: Pos2, dir: Vec2, length: f32) -> Vec<Pos2> {
        let (spacing, snap_interpolated) = stamp_step(diameter);
        let odd = (diameter.round().max(1.0) as i64) % 2 == 1;
        // Frame lengths a real drag produces: uneven, and none a multiple of the spacing.
        const STEPS: [f32; 6] = [3.1, 7.4, 2.2, 11.9, 5.0, 8.3];

        let mut raw = vec![start];
        let mut carry = 0.0;
        let mut cursor = start;
        let mut walked = 0.0;
        let mut frame = 0;
        while walked < length {
            let step = STEPS[frame % STEPS.len()];
            frame += 1;
            let next = cursor + dir * step;
            carry = walk_segment(cursor, next, spacing, carry, &mut raw);
            cursor = next;
            walked += step;
        }
        raw.iter()
            .enumerate()
            .map(|(index, center)| {
                if snap_interpolated || index == 0 {
                    Pos2::new(snap_center(center.x, odd), snap_center(center.y, odd))
                } else {
                    *center
                }
            })
            .collect()
    }

    /// Ratio of the largest to the smallest accumulated alpha along the stroke's centreline.
    ///
    /// The stroke runs long enough that a middle section sees a fully populated window of stamps,
    /// and only that section is sampled: the ends ramp in over a whole tip radius and their
    /// gradient is not the periodic modulation this measures. Sampling follows the stroke's own
    /// line rather than a pixel row, so the number isolates the along-track ripple from the
    /// perpendicular offset a pixel grid adds; opacity is 1, which cancels out of the ratio.
    ///
    /// 1.0 means a perfectly even stroke. Anything visibly above it is the beading defect.
    fn centreline_ripple(diameter: f32, hardness: f32, flow: f32, dir: Vec2) -> f32 {
        let radius = diameter * 0.5;
        let extent = radius + 0.5;
        let (spacing, _) = stamp_step(diameter);
        // Enough length for a stationary middle: two full tips of ramp plus a dozen spacings.
        let length = 4.0 * extent + 12.0 * spacing + 20.0;
        let start = Pos2::new(40.137, 40.311);
        let centers = stroke_centers(diameter, start, dir, length);
        let margin = extent + spacing;
        let span = length - 2.0 * margin;
        // The modulation's period is one spacing, so a twelfth of it resolves the extremes.
        let step = spacing / 12.0;
        let samples = (span / step).floor().max(1.0) as i32;

        let mut max = f32::MIN;
        let mut min = f32::MAX;
        for index in 0..=samples {
            let point = start + dir * (margin + step * index as f32);
            let mut alpha = 0.0;
            for center in &centers {
                let coverage = stamp_coverage(point.distance(*center), radius, hardness);
                alpha = accumulate_alpha(alpha, coverage, flow, 1.0);
            }
            max = max.max(alpha);
            min = min.min(alpha);
        }
        assert!(min > 0.0, "d={diameter} h={hardness} flow={flow} left a gap in the stroke");
        max / min
    }

    /// A stroke must be EVEN: no periodic scallop along its centreline, at any combination of
    /// hardness, flow and diameter.
    ///
    /// This is the regression guard for the beading defect. Its two causes are both geometric — a
    /// flat core in [`stamp_coverage`] and too coarse a [`SPACING_RATIO`] — so the bound is asserted
    /// on the accumulated f32 alpha rather than on painted pixels: at flow 0.02 a painted pixel
    /// holds an alpha near 10/255 and u8 quantization alone would swamp a 5 % measurement.
    #[test]
    fn the_stroke_centreline_has_no_periodic_ripple() {
        let mut worst = (1.0_f32, (0.0_f32, 0.0_f32, 0.0_f32));
        for diameter in [3.0, 10.0, 20.0, 50.0, 100.0, 400.0] {
            for hardness in [0.0, 0.2, 0.4, 0.5, 0.6, 0.75, 0.9, 1.0] {
                for flow in [0.02, 0.05, 0.1, 0.2, 0.5, 1.0] {
                    let ripple = centreline_ripple(diameter, hardness, flow, Vec2::new(1.0, 0.0));
                    if ripple > worst.0 {
                        worst = (ripple, (diameter, hardness, flow));
                    }
                }
            }
        }
        let (ratio, (diameter, hardness, flow)) = worst;
        assert!(
            ratio <= 1.05,
            "worst centreline ripple {ratio:.4} at diameter {diameter} hardness {hardness} flow {flow}"
        );
    }

    /// A thin diagonal stroke is even too, which it is not while every interpolated stamp is rounded
    /// to the pixel grid: at these diameters the spacing is floored ([`MIN_SPACING`]) rather than set
    /// by [`SPACING_RATIO`], so the rounding clusters and skips stamps and the measured ripple rises
    /// to about 1.56. With the snap confined to [`SNAP_ALL_MAX_DIAMETER`] it measures 1.0003.
    #[test]
    fn a_thin_diagonal_stroke_is_not_rippled_by_the_pixel_snap() {
        let dir = Vec2::new(0.8, 0.6);
        for diameter in [3.0, 4.0, 5.0] {
            for hardness in [0.0, 0.5] {
                let ripple = centreline_ripple(diameter, hardness, 0.02, dir);
                assert!(
                    ripple <= 1.05,
                    "diagonal ripple {ripple:.4} at diameter {diameter} hardness {hardness}"
                );
            }
        }
    }

    // ---- flow accumulation (D3) ----

    /// Flow lerps toward opacity and never past it, which is what keeps a self-crossing stroke from
    /// darkening where it overlaps itself.
    #[test]
    fn flow_builds_up_toward_opacity_and_stops_there() {
        let mut a = 0.0;
        for _ in 0..200 {
            a = accumulate_alpha(a, 1.0, 0.2, 0.6);
        }
        assert!((a - 0.6).abs() < 1e-4, "flow converges on opacity, got {a}");
        assert_eq!(accumulate_alpha(0.6, 1.0, 1.0, 0.6), 0.6, "at opacity nothing more is added");
        assert_eq!(accumulate_alpha(0.9, 1.0, 1.0, 0.6), 0.9, "a lower opacity never erases");
    }

    /// Full flow reaches full opacity in one stamp wherever coverage is full — the default brush.
    #[test]
    fn full_flow_paints_solid_in_one_stamp() {
        assert_eq!(accumulate_alpha(0.0, 1.0, 1.0, 1.0), 1.0);
        assert!((accumulate_alpha(0.0, 0.5, 1.0, 1.0) - 0.5).abs() < 1e-6);
    }

    // ---- the premultiplied composite (D3) ----

    /// Painting is premultiplied source-over: full alpha replaces, half alpha blends both the
    /// colour and the alpha channel.
    #[test]
    fn compositing_paint_is_premultiplied_source_over() {
        let orig = Color32::from_rgba_premultiplied(0, 0, 0, 0);
        let solid = composite_pixel(orig, 1.0, Color32::RED, false);
        assert_eq!(solid, Color32::from_rgba_premultiplied(255, 0, 0, 255));
        let half = composite_pixel(orig, 0.5, Color32::RED, false);
        assert_eq!(half, Color32::from_rgba_premultiplied(128, 0, 0, 128));
        // Over an opaque blue background, half red gives half of each in premultiplied space.
        let over_blue = composite_pixel(Color32::BLUE, 0.5, Color32::RED, false);
        assert_eq!(over_blue, Color32::from_rgba_premultiplied(128, 0, 128, 255));
    }

    /// Erasing scales every premultiplied channel down, so a fully erased pixel is transparent and
    /// a half-erased opaque one keeps its hue.
    #[test]
    fn compositing_erase_scales_all_premultiplied_channels() {
        assert_eq!(composite_pixel(Color32::BLUE, 1.0, Color32::RED, true), Color32::TRANSPARENT);
        let half = composite_pixel(Color32::BLUE, 0.5, Color32::RED, true);
        assert_eq!(half, Color32::from_rgba_premultiplied(0, 0, 128, 128));
    }

    // ---- the stamp-spacing walk (D3) ----

    /// Stamps land at exactly `spacing` apart and `from` is never re-stamped, because the stroke's
    /// opening stamp is placed by the caller.
    #[test]
    fn the_walk_places_stamps_at_constant_arc_length() {
        let mut out = Vec::new();
        let carry = walk_segment(Pos2::ZERO, Pos2::new(10.0, 0.0), 4.0, 0.0, &mut out);
        assert_eq!(out, vec![Pos2::new(4.0, 0.0), Pos2::new(8.0, 0.0)]);
        assert!((carry - 2.0).abs() < 1e-5, "2 px remain toward the next stamp, got {carry}");
    }

    /// The carry is what makes spacing independent of how the pointer was sampled: two short
    /// segments must produce the same stamps as one long one.
    #[test]
    fn the_carry_makes_spacing_independent_of_the_frame_split() {
        let mut whole = Vec::new();
        walk_segment(Pos2::ZERO, Pos2::new(12.0, 0.0), 5.0, 0.0, &mut whole);

        let mut split = Vec::new();
        let carry = walk_segment(Pos2::ZERO, Pos2::new(7.0, 0.0), 5.0, 0.0, &mut split);
        walk_segment(Pos2::new(7.0, 0.0), Pos2::new(12.0, 0.0), 5.0, carry, &mut split);

        assert_eq!(whole.len(), split.len(), "the same number of stamps either way");
        for (a, b) in whole.iter().zip(split.iter()) {
            assert!((a.x - b.x).abs() < 1e-4, "{a:?} vs {b:?}");
        }
    }

    /// `[` or the wheel can shrink the diameter — and with it the spacing — in the middle of a
    /// stroke, leaving a carry that is already larger than the new spacing. The walk must not
    /// answer that by stamping at distance zero: `from` would then take an extra flow step, which
    /// shows as a dark spot at low flow.
    #[test]
    fn a_mid_stroke_spacing_shrink_never_stamps_the_segment_start() {
        let mut out = Vec::new();
        let carry = walk_segment(Pos2::ZERO, Pos2::new(6.0, 0.0), 2.0, 4.0, &mut out);
        assert!(
            out.iter().all(|p| p.x > 0.0),
            "`from` must never be stamped, got {out:?}"
        );
        assert_eq!(
            out,
            vec![Pos2::new(2.0, 0.0), Pos2::new(4.0, 0.0), Pos2::new(6.0, 0.0)],
            "the phase restarts at `from` and then walks the new spacing"
        );
        assert!(carry.abs() < 1e-5, "the last stamp landed on `to`, got {carry}");
    }

    /// A carry exactly equal to the spacing is the boundary of the same case and must behave the
    /// same way — the old `(spacing - carry).max(0.0)` produced a zero step there too.
    #[test]
    fn a_carry_equal_to_the_spacing_also_restarts_the_phase() {
        let mut out = Vec::new();
        walk_segment(Pos2::ZERO, Pos2::new(10.0, 0.0), 5.0, 5.0, &mut out);
        assert_eq!(out, vec![Pos2::new(5.0, 0.0), Pos2::new(10.0, 0.0)]);
    }

    /// A segment shorter than the spacing places no stamp but still accumulates its length.
    #[test]
    fn a_short_segment_only_accumulates_carry() {
        let mut out = Vec::new();
        let carry = walk_segment(Pos2::ZERO, Pos2::new(1.0, 0.0), 5.0, 0.0, &mut out);
        assert!(out.is_empty());
        assert!((carry - 1.0).abs() < 1e-5, "got {carry}");
    }

    // ---- the diameter step table (D5) ----

    /// The band boundaries, in both directions. Stepping DOWN from a boundary must use the band
    /// below it (50 -> 45, not 40), which is the whole reason `diameter_step_down` looks at `d - 1`.
    #[test]
    fn the_diameter_table_steps_on_the_photoshop_grid() {
        assert_eq!(diameter_step_up(9), 10);
        assert_eq!(diameter_step_up(10), 15);
        assert_eq!(diameter_step_up(49), 50);
        assert_eq!(diameter_step_up(50), 60);
        assert_eq!(diameter_step_up(99), 100);
        assert_eq!(diameter_step_up(100), 125);
        assert_eq!(diameter_step_up(199), 200);
        assert_eq!(diameter_step_up(200), 250);
        assert_eq!(diameter_step_up(299), 300);
        assert_eq!(diameter_step_up(300), 400);

        assert_eq!(diameter_step_down(10), 9);
        assert_eq!(diameter_step_down(50), 45);
        assert_eq!(diameter_step_down(100), 90);
        assert_eq!(diameter_step_down(200), 175);
        assert_eq!(diameter_step_down(300), 250);
        assert_eq!(diameter_step_down(400), 300);
    }

    /// The table is clamped at both ends.
    #[test]
    fn the_diameter_table_saturates_at_the_limits() {
        assert_eq!(diameter_step_down(MIN_DIAMETER), MIN_DIAMETER);
        assert_eq!(diameter_step_up(MAX_DIAMETER), MAX_DIAMETER);
    }

    /// Up-then-down must return to the same value for every value the table can reach; otherwise
    /// tapping `]` and `[` would drift the brush size.
    #[test]
    fn stepping_up_then_down_returns_to_the_same_diameter() {
        let mut d = MIN_DIAMETER;
        while d < MAX_DIAMETER {
            let up = diameter_step_up(d);
            assert!(up > d, "the step must advance, {d} -> {up}");
            assert_eq!(diameter_step_down(up), d, "round trip broke at {d} (up = {up})");
            d = up;
        }
    }

    // ---- hardness steps (D5) ----

    #[test]
    fn hardness_snaps_to_the_quarter_grid() {
        assert!((step_hardness_value(1.0, false) - 0.75).abs() < 1e-6);
        assert!((step_hardness_value(0.75, false) - 0.5).abs() < 1e-6);
        assert!((step_hardness_value(0.6, true) - 0.75).abs() < 1e-6);
        assert!((step_hardness_value(0.6, false) - 0.5).abs() < 1e-6);
        assert_eq!(step_hardness_value(0.0, false), 0.0, "clamped at the bottom");
        assert_eq!(step_hardness_value(1.0, true), 1.0, "clamped at the top");
    }

    // ---- geometry helpers ----

    /// An odd diameter centres on a pixel, an even one on a boundary.
    #[test]
    fn the_stamp_centre_snaps_by_diameter_parity() {
        assert!((snap_center(3.2, true) - 3.5).abs() < 1e-6);
        assert!((snap_center(3.9, true) - 3.5).abs() < 1e-6);
        assert!((snap_center(3.2, false) - 3.0).abs() < 1e-6);
        assert!((snap_center(3.6, false) - 4.0).abs() < 1e-6);
    }

    /// The cursor ring marks the 50 %-coverage contour: the nominal radius for a hard tip, well
    /// inside it for a soft one.
    #[test]
    fn the_cursor_ring_tracks_the_fifty_percent_contour() {
        assert!((coverage_radius_50(40.0, 1.0) - 20.0).abs() < 1e-6);
        let soft = coverage_radius_50(40.0, 0.0);
        assert!(soft < 20.0 && soft > 5.0, "a soft tip's 50 % contour is inside its extent, got {soft}");
        assert!((coverage_radius_50(1.0, 1.0) - 0.5).abs() < 1e-6);
    }

    // ---- the pixel-exact cursor outline ----

    /// The pixels the stamp covers at 50 % or more, computed straight from [`stamp_coverage`] over
    /// a box WIDER than the outline builder scans — so a too-small scan box fails the tests too.
    fn covered_pixels(diameter: u32, hardness: f32) -> BTreeSet<(i32, i32)> {
        let radius = diameter as f32 * 0.5;
        let center = snap_center(0.0, diameter % 2 == 1);
        let ext = radius + 3.0;
        let lo = (center - ext).floor() as i32;
        let hi = (center + ext).ceil() as i32;
        let mut set = BTreeSet::new();
        for y in lo..=hi {
            for x in lo..=hi {
                let dx = (x as f32 + 0.5) - center;
                let dy = (y as f32 + 0.5) - center;
                if stamp_coverage((dx * dx + dy * dy).sqrt(), radius, hardness) >= 0.5 {
                    set.insert((x, y));
                }
            }
        }
        set
    }

    /// Undirected unit edges of a pixel set's boundary, as sorted corner pairs.
    fn boundary_edges(pixels: &BTreeSet<(i32, i32)>) -> BTreeSet<((i32, i32), (i32, i32))> {
        let mut edges = BTreeSet::new();
        for &(x, y) in pixels {
            let sides = [
                ((x, y), (x + 1, y), (x, y - 1)),
                ((x + 1, y), (x + 1, y + 1), (x + 1, y)),
                ((x, y + 1), (x + 1, y + 1), (x, y + 1)),
                ((x, y), (x, y + 1), (x - 1, y)),
            ];
            for (a, b, neighbour) in sides {
                if !pixels.contains(&neighbour) {
                    edges.insert(if a <= b { (a, b) } else { (b, a) });
                }
            }
        }
        edges
    }

    /// Expands a merged loop back into its unit edges, in the same sorted-pair form.
    fn loop_unit_edges(loop_pts: &[(i32, i32)]) -> Vec<((i32, i32), (i32, i32))> {
        let mut out = Vec::new();
        for pair in loop_pts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let dx = (b.0 - a.0).signum();
            let dy = (b.1 - a.1).signum();
            for k in 0..(b.0 - a.0).abs().max((b.1 - a.1).abs()) {
                let p = (a.0 + dx * k, a.1 + dy * k);
                let q = (a.0 + dx * (k + 1), a.1 + dy * (k + 1));
                out.push(if p <= q { (p, q) } else { (q, p) });
            }
        }
        out
    }

    /// Even-odd ray cast. Loop vertices are integers and the tested points are pixel centres, so no
    /// ray ever passes through a vertex.
    fn loop_contains(loop_pts: &[(i32, i32)], px: f32, py: f32) -> bool {
        let mut inside = false;
        for pair in loop_pts.windows(2) {
            let (ax, ay) = (pair[0].0 as f32, pair[0].1 as f32);
            let (bx, by) = (pair[1].0 as f32, pair[1].1 as f32);
            if (ay > py) != (by > py) {
                let t = (py - ay) / (by - ay);
                if px < ax + t * (bx - ax) {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// Unit step of one loop segment.
    fn seg_dir(from: (i32, i32), to: (i32, i32)) -> (i32, i32) {
        ((to.0 - from.0).signum(), (to.1 - from.1).signum())
    }

    /// One idle `interact` frame at the given zoom: no buttons, so only the cursor cache is touched.
    fn cursor_frame(tool: &mut BrushTool, stack: &mut LayerStack, zoom: f32) -> ToolOutcome {
        let page_size = stack.size();
        let mut selection = None;
        let mut ctx = PsToolContext {
            page_size,
            pointer_image: Some(Pos2::new(20.0, 20.0)),
            pointer_in_viewport: true,
            primary_pressed: false,
            primary_down: false,
            primary_released: false,
            secondary_down: false,
            pointer_delta: Vec2::ZERO,
            modifiers: egui::Modifiers::default(),
            cancel_pressed: false,
            remove_point_pressed: false,
            view: ViewTransform {
                viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 64.0)),
                zoom,
                center_world: Vec2::new(32.0, 32.0),
            },
            stack,
            selection: &mut selection,
        };
        tool.interact(&mut ctx)
    }

    /// A one-pixel brush outlines exactly the four edges of its own pixel — the per-pixel-work
    /// requirement stated on the CURSOR instead of on the paint path.
    #[test]
    fn a_one_pixel_brush_outlines_exactly_its_own_pixel() {
        assert_eq!(coverage_outline_corners(1, 1.0), vec![(0, 0), (1, 0), (1, 1), (0, 1), (0, 0)]);
        // And the offsets the overlay uses centre that square on the pixel centre.
        assert_eq!(
            build_coverage_outline(1, 1.0),
            vec![
                Vec2::new(-0.5, -0.5),
                Vec2::new(0.5, -0.5),
                Vec2::new(0.5, 0.5),
                Vec2::new(-0.5, 0.5),
                Vec2::new(-0.5, -0.5),
            ]
        );
    }

    /// A two-pixel brush outlines the boundary of a symmetric 2x2 block, centred on a pixel
    /// boundary — the even half of the parity contract.
    #[test]
    fn a_two_pixel_brush_outlines_the_two_by_two_block() {
        assert_eq!(
            coverage_outline_corners(2, 1.0),
            vec![(-1, -1), (1, -1), (1, 1), (-1, 1), (-1, -1)]
        );
    }

    /// The outline sits on the grid the stamp will actually land on: an odd diameter centres on a
    /// pixel centre, so its corner offsets are half-integers; an even one on a pixel boundary, so
    /// they are whole. Both come from `snap_center`, never from a second copy of the rule.
    #[test]
    fn the_outline_is_placed_by_diameter_parity() {
        for offset in build_coverage_outline(5, 1.0) {
            assert!((offset.x.fract().abs() - 0.5).abs() < 1e-6, "odd diameter: {offset:?}");
            assert!((offset.y.fract().abs() - 0.5).abs() < 1e-6, "odd diameter: {offset:?}");
        }
        for offset in build_coverage_outline(6, 1.0) {
            assert!(offset.x.fract().abs() < 1e-6, "even diameter: {offset:?}");
            assert!(offset.y.fract().abs() < 1e-6, "even diameter: {offset:?}");
        }
    }

    /// Collinear boundary edges are merged: no two consecutive segments of the loop share a
    /// direction, and a straight run is one segment instead of one per pixel edge.
    #[test]
    fn collinear_boundary_edges_are_merged() {
        let corners = coverage_outline_corners(20, 1.0);
        for w in corners.windows(3) {
            assert_ne!(
                seg_dir(w[0], w[1]),
                seg_dir(w[1], w[2]),
                "a straight run survived the merge at {:?}",
                w[1]
            );
        }
        // The closing vertex, which `windows(3)` cannot see: the wrap-around case the merge has to
        // handle because the walk may start in the middle of a straight run.
        let n = corners.len();
        assert!(n >= 4);
        assert_ne!(
            seg_dir(corners[n - 2], corners[0]),
            seg_dir(corners[0], corners[1]),
            "the closing vertex is not a corner"
        );
        let longest = corners
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).abs().max((w[1].1 - w[0].1).abs()))
            .max()
            .unwrap_or(0);
        assert!(longest > 1, "nothing was merged: the longest segment is {longest} px");
    }

    /// The merge collapses a run even when the walk entered it in the middle, which is why it
    /// compares the wrap-around triple and not just the interior ones.
    #[test]
    fn merging_collapses_a_run_that_wraps_the_start() {
        let path = vec![(1, 0), (2, 0), (3, 0), (3, 1), (2, 1), (1, 1), (0, 1), (0, 0), (1, 0)];
        assert_eq!(merge_collinear_closed(&path), vec![(3, 0), (3, 1), (0, 1), (0, 0), (3, 0)]);
    }

    /// The loop is closed and traces every boundary edge of the covered set exactly once — no gap
    /// to leak through and no edge drawn twice.
    #[test]
    fn the_outline_traces_every_boundary_edge_exactly_once() {
        for (d, h) in [(1u32, 1.0f32), (2, 1.0), (5, 1.0), (9, 0.5), (20, 0.0), (33, 0.75), (64, 1.0)] {
            let corners = coverage_outline_corners(d, h);
            assert_eq!(corners.first(), corners.last(), "d={d} h={h}: the loop must be closed");
            let mut walked = loop_unit_edges(&corners);
            let traced = walked.len();
            walked.sort_unstable();
            walked.dedup();
            assert_eq!(walked.len(), traced, "d={d} h={h}: an edge was traced twice");
            let expected: Vec<_> = boundary_edges(&covered_pixels(d, h)).into_iter().collect();
            assert_eq!(walked, expected, "d={d} h={h}: the loop is not the coverage boundary");
        }
    }

    /// The decisive one: the pixels the outline encloses are exactly the pixels the PAINTING kernel
    /// covers at 50 % or more. If the two ever drift apart the cursor lies about where the brush
    /// paints, which is the whole reason this outline calls `stamp_coverage` instead of a formula
    /// of its own.
    #[test]
    fn the_outline_encloses_exactly_the_fifty_percent_coverage_pixels() {
        for (d, h) in [(1u32, 1.0f32), (2, 1.0), (3, 0.0), (4, 0.25), (7, 1.0), (16, 0.5), (25, 0.0), (40, 1.0)] {
            let corners = coverage_outline_corners(d, h);
            let expected = covered_pixels(d, h);
            let center = snap_center(0.0, d % 2 == 1);
            let ext = d as f32 * 0.5 + 3.0;
            let lo = (center - ext).floor() as i32;
            let hi = (center + ext).ceil() as i32;
            for y in lo..=hi {
                for x in lo..=hi {
                    assert_eq!(
                        loop_contains(&corners, x as f32 + 0.5, y as f32 + 0.5),
                        expected.contains(&(x, y)),
                        "d={d} h={h}: pixel ({x},{y}) disagrees"
                    );
                }
            }
        }
    }

    /// Every reachable tip yields a single closed loop. `coverage_rows` refuses a broken row or a
    /// vertical gap by returning `None`, which would silently degrade the cursor to the smooth
    /// circle; the kernel's radial monotonicity means that must never happen.
    #[test]
    fn every_tip_yields_a_single_closed_loop() {
        for d in [MIN_DIAMETER, 2, 3, 4, 5, 8, 13, 21, 34, 55, 89, 144, 233, MAX_DIAMETER] {
            for h in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let outline = build_coverage_outline(d, h);
                assert!(outline.len() >= 5, "d={d} h={h}: got {} points", outline.len());
                assert_eq!(outline.first(), outline.last(), "d={d} h={h}: not closed");
            }
        }
    }

    /// The cursor cache answers the two questions `draw_overlay` cannot ask itself: whether the
    /// destination grid is one a staircase may describe, and whether the outline is worth building.
    #[test]
    fn the_cursor_cache_captures_the_grid_and_defers_the_outline() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        cursor_frame(&mut tool, &mut stack, 1.0);
        assert!(tool.cursor_grid_origin.is_some(), "an identity raster transform is a usable grid");
        assert!(tool.cursor_outline.is_none(), "below the zoom threshold nothing would draw it");

        cursor_frame(&mut tool, &mut stack, PIXEL_OUTLINE_MIN_SCALE);
        let key = tool.outline_key();
        assert!(
            tool.cursor_outline.as_ref().is_some_and(|cached| cached.key == key),
            "zoomed in, the outline is built and keyed to the current tip"
        );

        // A rotated layer paints on a grid that is not axis-aligned on screen, so a staircase drawn
        // in page space would mark the wrong pixels.
        if let Some(layer) = stack.active_editable_mut() {
            layer.transform.rotation = 0.3;
        }
        cursor_frame(&mut tool, &mut stack, PIXEL_OUTLINE_MIN_SCALE);
        assert!(tool.cursor_grid_origin.is_none(), "a rotated layer falls back to the smooth circle");

        // ... and so does a scaled one, for the same reason.
        if let Some(layer) = stack.active_editable_mut() {
            layer.transform.rotation = 0.0;
            layer.transform.scale = 2.0;
        }
        cursor_frame(&mut tool, &mut stack, PIXEL_OUTLINE_MIN_SCALE);
        assert!(tool.cursor_grid_origin.is_none(), "a scaled layer falls back to the smooth circle");
    }

    /// Shift constrains to the dominant axis only — never to a diagonal.
    #[test]
    fn the_shift_constraint_picks_one_axis() {
        let anchor = Pos2::new(10.0, 10.0);
        assert_eq!(constrain_to_axis(anchor, Pos2::new(30.0, 15.0)), Pos2::new(30.0, 10.0));
        assert_eq!(constrain_to_axis(anchor, Pos2::new(15.0, 30.0)), Pos2::new(10.0, 30.0));
    }

    // ---- the paint path ----

    /// A press with no drag paints, and a diameter-1 hard tip paints exactly the pixel under the
    /// pointer — the end-to-end form of the per-pixel requirement.
    #[test]
    fn a_single_click_with_a_one_pixel_tip_paints_exactly_one_pixel() {
        let mut tool = BrushTool::default();
        tool.set_diameter(1);
        let mut stack = stack_with_blank_raster();

        let out = frame(&mut tool, &mut stack, Pos2::new(20.4, 30.7), true, true, true);
        assert!(out.dirty.is_some(), "the press paints");
        assert_eq!(painted_pixels(&stack), 1, "exactly one pixel is opaque");
        assert_eq!(alpha_at(&stack, 20, 30), 255, "and it is the one under the pointer");
    }

    /// The stroke's OPENING stamp is snapped to the pixel grid at EVERY diameter, not only at the
    /// ones whose interpolated stamps are: it is the stamp the pixel-exact cursor outline previews,
    /// and that outline is built in a frame centred on `snap_center(0.0, odd)`. Two clicks anywhere
    /// inside the same pixel must therefore paint exactly the same pixels, or the cursor is lying
    /// about where the brush lands.
    #[test]
    fn a_click_lands_on_the_pixel_grid_at_a_diameter_that_does_not_snap_its_walk() {
        let diameter = 5;
        assert!(diameter as f32 > SNAP_ALL_MAX_DIAMETER, "the interpolated stamps of this tip do not snap");
        let paint = |pointer: Pos2| {
            let mut tool = BrushTool::default();
            tool.set_diameter(diameter);
            let mut stack = stack_with_blank_raster();
            frame(&mut tool, &mut stack, pointer, true, true, true);
            let layer = stack.layer(stack.active_id()).map(|l| l.image.pixels.clone());
            layer.unwrap_or_default()
        };
        let low = paint(Pos2::new(20.05, 30.95));
        let high = paint(Pos2::new(20.95, 30.05));
        assert!(!low.is_empty() && low.iter().any(|px| px.a() != 0), "the click paints");
        assert_eq!(low, high, "two clicks inside one pixel painted different pixels");
    }

    /// A press and a release delivered in the SAME frame (a fast click) must still paint: the tool
    /// stamps the press before it observes the button being up.
    #[test]
    fn a_click_resolved_in_one_frame_still_paints() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        let out = frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, false);
        assert!(out.dirty.is_some(), "a same-frame click must report a dirty rect");
        assert!(painted_pixels(&stack) > 0);
        assert!(!tool.gesture_in_flight(), "and must not leave a stroke in flight");
    }

    /// A stroke that crosses itself must not darken at the crossing: the flow buffer, not the
    /// layer, accumulates.
    #[test]
    fn a_self_crossing_stroke_does_not_darken_at_the_crossing() {
        let mut tool = BrushTool::default();
        tool.set_diameter(9);
        tool.opacity = 0.5;
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(10.0, 32.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(50.0, 32.0), true, false, true);
        let single_pass = alpha_at(&stack, 32, 32);
        // Come straight back over the same line.
        frame(&mut tool, &mut stack, Pos2::new(10.0, 32.0), true, false, true);
        assert_eq!(alpha_at(&stack, 32, 32), single_pass, "the second pass must add nothing");
        assert!(single_pass > 100 && single_pass < 160, "half opacity, got {single_pass}");
    }

    /// The reported dirty rect is THIS frame's segment box, never the growing stroke union — the
    /// tab's tile upload budget depends on it.
    #[test]
    fn the_dirty_rect_covers_only_this_frames_segment() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(5.0, 32.0), true, true, true);
        let far = frame(&mut tool, &mut stack, Pos2::new(58.0, 32.0), true, false, true)
            .dirty
            .expect("the drag paints");
        let back = frame(&mut tool, &mut stack, Pos2::new(56.0, 32.0), true, false, true)
            .dirty
            .expect("the small move back still stamps");
        assert!(back.min_x > far.min_x, "the second rect must not include the whole stroke");
    }

    /// Erasing removes pixels a previous stroke painted.
    #[test]
    fn the_eraser_clears_previously_painted_pixels() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, false, false);
        assert!(painted_pixels(&stack) > 0);

        tool.erase = true;
        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, false, false);
        assert_eq!(alpha_at(&stack, 20, 20), 0, "the erased centre is fully transparent");
    }

    /// Two consecutive strokes must both survive: the second composites over the pixels the first
    /// left, not over the layer as it was before the first.
    #[test]
    fn a_second_stroke_does_not_undo_the_first() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        frame(&mut tool, &mut stack, Pos2::new(15.0, 15.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(15.0, 15.0), true, false, false);
        let first = painted_pixels(&stack);
        assert!(first > 0);

        frame(&mut tool, &mut stack, Pos2::new(45.0, 45.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(45.0, 45.0), true, false, false);
        assert!(painted_pixels(&stack) > first, "both stamps must be on the layer");
        assert_eq!(alpha_at(&stack, 15, 15), 255, "the first stroke's centre is still painted");
    }

    /// The line anchor: Shift held for the whole time chains segment after segment, at any angle,
    /// painting the pixels in between.
    ///
    /// The anchor is scoped to ONE Shift hold — a deliberate divergence from Photoshop, which
    /// chains from the previous stroke's last stamp whatever the modifiers did. The three tests
    /// after this one pin the three ways the anchor must NOT survive.
    #[test]
    fn shift_click_chains_a_straight_segment_while_shift_stays_held() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let shift = egui::Modifiers { shift: true, ..Default::default() };

        // First Shift+click: it only places a stamp and becomes the anchor.
        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, false, false, shift);
        assert_eq!(alpha_at(&stack, 40, 40), 0, "nothing painted at the far end yet");

        // Second Shift+click, with Shift never released in between: the segment is drawn.
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, false, false, shift);
        assert!(alpha_at(&stack, 40, 40) > 0, "the click end is painted");
        assert!(alpha_at(&stack, 25, 25) > 0, "and so is the middle of the chained segment");
    }

    /// Releasing Shift drops the anchor: the next Shift+click starts a fresh chain and must NOT
    /// draw back to a stamp placed before the release.
    #[test]
    fn releasing_shift_drops_the_line_anchor() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let shift = egui::Modifiers { shift: true, ..Default::default() };

        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, false, false, shift);
        // One idle frame with Shift up — the release the rule hangs on.
        frame(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, false, false);

        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, false, false, shift);
        assert!(alpha_at(&stack, 40, 40) > 0, "the new Shift+click still paints its own stamp");
        assert_eq!(
            alpha_at(&stack, 25, 25),
            0,
            "but no segment may connect it to a stamp from before the Shift release"
        );
    }

    /// A stroke made WITHOUT Shift leaves no anchor at all, so a later Shift+click connects to
    /// nothing. This is exactly where the rule departs from Photoshop.
    #[test]
    fn a_stroke_without_shift_leaves_no_line_anchor() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, false, false);

        let shift = egui::Modifiers { shift: true, ..Default::default() };
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, false, false, shift);
        assert!(alpha_at(&stack, 40, 40) > 0, "the Shift+click paints its own stamp");
        assert_eq!(alpha_at(&stack, 25, 25), 0, "a plain stroke chains to nothing");
    }

    /// A Shift release that happens on a SUPPRESSED frame (the tab routed input to a pan and called
    /// `freeze` instead of `interact`) is one the tool can never observe: `freeze` carries no
    /// modifiers. The anchor must be dropped there rather than survive a release nobody saw.
    #[test]
    fn a_suppressed_frame_drops_the_line_anchor() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let shift = egui::Modifiers { shift: true, ..Default::default() };

        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(10.0, 10.0), true, false, false, shift);
        tool.freeze();

        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, true, true, shift);
        frame_with(&mut tool, &mut stack, Pos2::new(40.0, 40.0), true, false, false, shift);
        assert!(alpha_at(&stack, 40, 40) > 0, "the Shift+click paints its own stamp");
        assert_eq!(alpha_at(&stack, 25, 25), 0, "the pan ended the chain");
    }

    /// Shift held during a drag constrains the stroke to one axis.
    #[test]
    fn shift_during_a_drag_constrains_the_stroke_to_an_axis() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let shift = egui::Modifiers { shift: true, ..Default::default() };

        frame(&mut tool, &mut stack, Pos2::new(10.0, 32.0), true, true, true);
        frame_with(&mut tool, &mut stack, Pos2::new(50.0, 45.0), true, false, true, shift);
        assert!(alpha_at(&stack, 45, 32) > 0, "the horizontal run is painted");
        assert_eq!(alpha_at(&stack, 45, 45), 0, "the vertical drift is not");
    }

    /// Alt is the eyedropper/HUD modifier: it must never start a stroke, and releasing it mid-drag
    /// must not promote the held button into one.
    #[test]
    fn alt_suppresses_painting_for_the_whole_button_hold() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let alt = egui::Modifiers { alt: true, ..Default::default() };

        frame_with(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, true, alt);
        assert_eq!(painted_pixels(&stack), 0, "Alt+click samples, it does not paint");
        assert!(!tool.gesture_in_flight());
        // Alt released while the button is still held.
        frame(&mut tool, &mut stack, Pos2::new(25.0, 20.0), true, false, true);
        assert_eq!(painted_pixels(&stack), 0, "and the drag must not become a stroke");
        // The release re-arms the brush.
        frame(&mut tool, &mut stack, Pos2::new(25.0, 20.0), true, false, false);
        frame(&mut tool, &mut stack, Pos2::new(30.0, 20.0), true, true, true);
        assert!(painted_pixels(&stack) > 0, "the next plain press paints again");
    }

    /// The Alt + right drag HUD changes diameter horizontally and hardness vertically, relative to
    /// the values sampled at gesture start, and paints nothing.
    #[test]
    fn the_hud_gesture_changes_size_and_hardness_without_painting() {
        let mut tool = BrushTool::default();
        tool.set_diameter(20);
        tool.hardness = 0.5;
        let mut stack = stack_with_blank_raster();
        let page_size = stack.size();
        let alt = egui::Modifiers { alt: true, ..Default::default() };

        let hud_frame = |delta: Vec2, tool: &mut BrushTool, stack: &mut LayerStack| {
            let mut selection = None;
            let mut ctx = PsToolContext {
                page_size,
                pointer_image: Some(Pos2::new(20.0, 20.0)),
                pointer_in_viewport: true,
                primary_pressed: false,
                primary_down: false,
                primary_released: false,
                secondary_down: true,
                    pointer_delta: delta,
                modifiers: alt,
                cancel_pressed: false,
                remove_point_pressed: false,
                view: ViewTransform {
                    viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 64.0)),
                    zoom: 1.0,
                    center_world: Vec2::new(32.0, 32.0),
                },
                stack,
                selection: &mut selection,
            };
            tool.interact(&mut ctx)
        };

        hud_frame(Vec2::new(30.0, 0.0), &mut tool, &mut stack);
        assert_eq!(tool.diameter, 50, "dragging right grows the diameter 1:1");
        assert!(tool.gesture_in_flight(), "the HUD drag is a gesture");
        hud_frame(Vec2::new(0.0, 100.0), &mut tool, &mut stack);
        assert!((tool.hardness - 1.0).abs() < 1e-6, "dragging down hardens, got {}", tool.hardness);
        assert_eq!(painted_pixels(&stack), 0, "the HUD must not paint");
    }

    // ---- the start gate (unchanged contract) ----

    // ---- the stroke-end latch, which is what triggers the tab's commit ----

    /// The ordinary case: one finished stroke asks for exactly one commit, and the latch is
    /// consumed by the tab that reads it.
    #[test]
    fn a_released_stroke_latches_exactly_one_commit() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(
            !tool.take_stroke_finished(),
            "a stroke still in flight must not ask for a commit"
        );
        frame(&mut tool, &mut stack, Pos2::new(20.0, 16.0), true, false, false);
        assert!(tool.take_stroke_finished(), "the release asks for the commit");
        assert!(!tool.take_stroke_finished(), "and the latch is consumed exactly once");
    }

    /// A press and release egui delivers in ONE frame must still ask for a commit. This is why the
    /// tab reads a latch instead of watching `gesture_in_flight` fall: such a click never shows an
    /// in-flight frame at all.
    #[test]
    fn a_click_resolved_in_one_frame_latches_a_commit() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(20.0, 20.0), true, true, false);
        assert!(tool.take_stroke_finished(), "a same-frame click is a finished stroke");
    }

    /// THE regression this latch exists for: the tab routes a frame to a canvas pan (middle button,
    /// or Space + drag) and calls `freeze` instead of `interact`, so a pointer release performed
    /// during the pan is never delivered to the tool. The stroke must therefore END on the
    /// suppressed frame — otherwise it gets no undo entry at all, and a `Клин` stroke is never
    /// written to the shared overlay model and vanishes on the next page switch.
    #[test]
    fn a_stroke_suppressed_by_a_pan_ends_and_latches_a_commit() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(tool.gesture_in_flight(), "the press started a stroke");
        assert!(painted_pixels(&stack) > 0, "and painted pixels the commit must not lose");

        tool.freeze();
        assert!(!tool.gesture_in_flight(), "the suppressed frame ends the stroke");
        assert!(tool.take_stroke_finished(), "and asks the tab to commit it");

        // Further suppressed frames hold no stroke, so they must ask for nothing.
        tool.freeze();
        assert!(!tool.take_stroke_finished(), "an idle suppressed frame asks for no commit");
    }

    /// `reset` on its own never latches a commit: it is the last step of an abandonment, and by
    /// then the tab has already run `end_stroke_for_commit`. Latching here as well would let a
    /// commit fire on a later frame, after a page switch has replaced the layer stack — an undo
    /// step recorded against the wrong page's pixels.
    #[test]
    fn an_abandoned_stroke_never_latches_a_commit() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        tool.reset();
        assert!(!tool.gesture_in_flight());
        assert!(!tool.take_stroke_finished(), "an abandoned stroke asks for no commit");
    }

    /// Esc pressed on a frame the tab routed to a pan abandons the gesture — but the brush's pixels
    /// are already on the layer and `reset` cannot take them back, so the tab commits the stroke
    /// first (`commit_brush_stroke_before_abandon` -> `reset_active_tool`). This exercises that
    /// exact two-step sequence: the commit must be asked for BEFORE the abandon, and once.
    #[test]
    fn esc_mid_stroke_commits_the_painted_stroke_before_abandoning_it() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(painted_pixels(&stack) > 0, "the stroke painted pixels that must stay undoable");

        tool.end_stroke_for_commit();
        assert!(!tool.gesture_in_flight(), "the stroke is over either way");
        assert!(tool.take_stroke_finished(), "and the tab is asked to commit it");

        tool.reset();
        assert!(
            !tool.take_stroke_finished(),
            "the abandon that follows must not ask for a second commit of the same stroke"
        );
    }

    /// The same sequence on a TOOL SWITCH, which reaches it through `set_active_tool` on the
    /// outgoing tool. Same latch, so the stroke commits exactly once.
    #[test]
    fn a_tool_switch_mid_stroke_commits_the_painted_stroke_before_abandoning_it() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        frame(&mut tool, &mut stack, Pos2::new(28.0, 16.0), true, false, true);
        let painted = painted_pixels(&stack);
        assert!(painted > 0);

        tool.end_stroke_for_commit();
        assert!(tool.take_stroke_finished(), "the outgoing tool's stroke is committed");
        tool.reset();
        assert!(!tool.gesture_in_flight(), "and only then is the gesture abandoned");
        assert!(!tool.take_stroke_finished(), "one stroke, one commit");
        assert_eq!(
            painted_pixels(&stack),
            painted,
            "abandoning changes no pixel: the commit is about the undo entry, not the image"
        );
    }

    /// The scope guard: the tab runs the same helper for EVERY tool's abandonment, so an idle brush
    /// must latch nothing. Otherwise the lasso's Esc — which must only drop its pending polygon —
    /// would drag a phantom brush commit along with it.
    #[test]
    fn abandoning_while_the_brush_is_idle_asks_for_no_commit() {
        let mut tool = small_brush();

        tool.end_stroke_for_commit();
        assert!(
            !tool.take_stroke_finished(),
            "an idle brush must not ask for a commit when another tool's gesture is abandoned"
        );
    }

    // ---- the sparse stroke buffer ----

    /// The stroke buffer must be sparse: a long diagonal stroke allocates tiles along its PATH, not
    /// over its bounding box. On a ribbon page (~800x19000 px) a bounding-box buffer for one such
    /// stroke is ~137 MB and every growth memcpys all of it on the GUI thread, which §5 forbids.
    #[test]
    fn a_long_diagonal_stroke_allocates_tiles_along_its_path_not_its_bounding_box() {
        /// Page side in px: eight stroke tiles across, so the bounding box of a diagonal stroke
        /// spans 64 tiles while its path spans 8.
        const SIDE: usize = 8 * STROKE_TILE_SIDE;
        let big: [usize; 2] = [SIDE, SIDE];
        let mut stack = LayerStack::new(
            0,
            big,
            ColorImage::filled(big, Color32::TRANSPARENT),
            ColorImage::filled(big, Color32::TRANSPARENT),
        );
        stack.add_raster_layer();
        let mut tool = small_brush();

        let last = SIDE as f32 - 4.0;
        frame(&mut tool, &mut stack, Pos2::new(4.0, 4.0), true, true, true);
        for step in 1..=16 {
            let t = step as f32 / 16.0;
            let p = 4.0 + (last - 4.0) * t;
            frame(&mut tool, &mut stack, Pos2::new(p, p), true, false, true);
        }

        let tiles = tool
            .stroke
            .as_ref()
            .expect("the stroke is still in flight")
            .tiles
            .len();
        let bbox_tiles = 8 * 8;
        assert!(
            tiles >= 8,
            "the diagonal crosses eight tiles, so at least that many exist, got {tiles}"
        );
        assert!(
            tiles * 2 < bbox_tiles,
            "the buffer must follow the path, not the bounding box: {tiles} tiles vs {bbox_tiles}"
        );
        // The pixels are still correct — a sparse buffer must not change what is painted.
        assert!(alpha_at(&stack, SIDE / 2, SIDE / 2) > 0, "the diagonal is painted");
        assert_eq!(alpha_at(&stack, 4, SIDE - 8), 0, "and the bounding box's corner is not");
    }

    /// The selection clip is rasterized once per TILE, so a stroke that crosses a tile boundary
    /// must be clipped in the newly allocated tile exactly as in the first one.
    #[test]
    fn a_selection_clips_the_stroke_in_every_tile_it_crosses() {
        /// Two stroke tiles across, so the stroke leaves the first tile column mid-run.
        const SIDE: usize = 2 * STROKE_TILE_SIDE;
        let page: [usize; 2] = [SIDE, SIDE];
        let mut stack = LayerStack::new(
            0,
            page,
            ColorImage::filled(page, Color32::TRANSPARENT),
            ColorImage::filled(page, Color32::TRANSPARENT),
        );
        stack.add_raster_layer();
        let mut tool = small_brush();

        let mut mask = Selection::empty(SIDE, SIDE);
        // The left tile column only.
        mask.set_rect(0, 0, STROKE_TILE - 1, SIDE as i32 - 1);
        assert!(mask.contains(100, 60), "the fixture selects the left half");
        assert!(!mask.contains(200, 60), "and not the right one");
        let mut selection = Some(mask);

        frame_with_selection(&mut tool, &mut stack, &mut selection, Pos2::new(10.0, 60.0), true, true);
        for x in [60.0, 120.0, 180.0, 240.0] {
            frame_with_selection(&mut tool, &mut stack, &mut selection, Pos2::new(x, 60.0), false, true);
        }

        assert!(alpha_at(&stack, 100, 60) > 0, "inside the selection the stroke paints");
        assert_eq!(
            alpha_at(&stack, 200, 60),
            0,
            "and outside it nothing is written, in a freshly allocated tile as much as in the first"
        );
    }

    /// A press that lands on a floating dock panel must not paint — not on the press frame, and
    /// not on any held-button frame after it.
    #[test]
    fn a_press_outside_the_viewport_never_starts_a_stroke() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let under_panel = Pos2::new(32.0, 32.0);

        let press = frame(&mut tool, &mut stack, under_panel, false, true, true);
        assert!(press.dirty.is_none(), "the press frame must not paint under a panel");
        for held in 0..3 {
            let outcome = frame(&mut tool, &mut stack, under_panel, false, false, true);
            assert!(
                outcome.dirty.is_none(),
                "held frame {held} must not paint under a panel"
            );
        }
        assert_eq!(
            painted_pixels(&stack),
            0,
            "no pixel of the active layer may change while the stroke was never allowed to start"
        );
    }

    /// The other half of the contract: only the START is gated. A stroke begun on bare canvas keeps
    /// painting when the pointer crosses a floating panel, mirroring the tab's panning decision.
    #[test]
    fn a_stroke_begun_inside_the_viewport_survives_the_pointer_crossing_a_panel() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        let press = frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(press.dirty.is_some(), "a press on bare canvas paints");
        let after_press = painted_pixels(&stack);
        assert!(after_press > 0, "the press stamp is on the layer");

        let crossing = frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), false, false, true);
        assert!(
            crossing.dirty.is_some(),
            "a stroke in flight continues while the pointer is over a panel"
        );
        assert!(
            painted_pixels(&stack) > after_press,
            "the segment dragged across the panel must be painted"
        );
    }

    /// `gesture_in_flight` must track the STROKE, not the button: it is what the tab asks before it
    /// lets the brush circle keep following a pointer that moved over a floating panel.
    #[test]
    fn gesture_in_flight_follows_the_stroke_from_press_to_release() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        assert!(!tool.gesture_in_flight(), "a fresh brush holds no stroke");
        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        assert!(tool.gesture_in_flight(), "the accepted press starts a stroke");
        frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), false, false, true);
        assert!(tool.gesture_in_flight(), "the stroke survives crossing a panel");
        frame(&mut tool, &mut stack, Pos2::new(48.0, 16.0), true, false, false);
        assert!(!tool.gesture_in_flight(), "the release ends the stroke");
    }

    /// The case a `primary_down` test at the call site would get wrong: a press that LANDS on a
    /// floating panel holds the button down too, but the start gate refused it, so no gesture
    /// exists and the preview must not be kept alive under the panel.
    #[test]
    fn a_refused_press_never_reports_a_gesture_in_flight() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();
        let under_panel = Pos2::new(32.0, 32.0);

        frame(&mut tool, &mut stack, under_panel, false, true, true);
        assert!(!tool.gesture_in_flight(), "a refused press starts no gesture");
        for held in 0..3 {
            frame(&mut tool, &mut stack, under_panel, false, false, true);
            assert!(!tool.gesture_in_flight(), "held frame {held} must stay gesture-free");
        }
    }

    /// Releasing ends the stroke, so the start gate is armed again: a finished stroke must not let
    /// a later press over a panel paint.
    #[test]
    fn a_release_re_arms_the_start_gate() {
        let mut tool = small_brush();
        let mut stack = stack_with_blank_raster();

        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, true, true);
        // Button up: `interact` drops the stroke buffer, ending the stroke.
        frame(&mut tool, &mut stack, Pos2::new(16.0, 16.0), true, false, false);
        let after_stroke = painted_pixels(&stack);

        let press = frame(&mut tool, &mut stack, Pos2::new(48.0, 48.0), false, true, true);
        assert!(press.dirty.is_none(), "the next press under a panel must not paint");
        let held = frame(&mut tool, &mut stack, Pos2::new(48.0, 48.0), false, false, true);
        assert!(held.dirty.is_none(), "nor may the frame after it");
        assert_eq!(
            painted_pixels(&stack),
            after_stroke,
            "the layer must still hold exactly the pixels of the finished stroke"
        );
    }
}
