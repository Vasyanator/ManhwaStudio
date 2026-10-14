/*
File: crates/ms-widgets/src/touch_nav/mod.rs

Purpose:
Public surface of the canvas touch-gesture owner. A finger that starts on a canvas
surface never reaches a tool: the router (`router.rs`) removes egui-winit's emulated
mouse events for such a touch before egui sees them and turns the touch into typed,
per-canvas output — navigation (pan / pinch), a long-press menu request and a
long-press selection drag. Ordinary UI (panels, menus, dialogs) is not touched.

Main responsibilities:
- the per-frame publication channel from canvases to the router (`publish_surface`,
  `publish_capture_layer`, `publish_hole`), stored in `egui::Context` temp data;
- the take-once delivery channel from the router to canvases (`take_touch_output`);
- the public output types (`TouchNavigation`, `TouchSelection`, `TouchOutput`);
- the long-press timing constant and the promotion-rule constants.

Key structures:
- `TouchSurface`: one canvas' claimable rect, its holes and its long-press-drag mode.
- `TouchOutput`: everything one canvas gets from the router for one frame.

Key functions:
- `publish_surface`, `publish_capture_layer`, `publish_hole`, `take_touch_output`;
- `TouchRouter::route` (in `router.rs`), driven by the shell's `raw_input_hook`;
- `context_menu_popup` (in `menu.rs`), the single open path of canvas context menus.

Notes:
Surfaces are published DURING frame N and frozen (taken) by the router at the start
of frame N+1, before egui builds its pointer state, so a claim always tests last
frame's layout — exactly what `Context::layer_id_at` reads too. Outputs live for one
frame: the router replaces them every frame, a consumer takes them once.
Publishing is live only while an enabled router runs on the ROOT viewport (a flag every
routing call writes): without one nothing would ever drain the lists, so `publish_*` are
no-ops on the wasm entry (no raw-input hook) and where gestures are unavailable.
*/

//! Finger navigation and long-press gestures on canvases (see `MODULE_README.md`).

mod menu;
mod router;

#[cfg(test)]
mod tests;

pub use menu::context_menu_popup;
pub use router::TouchRouter;

use egui::{Context, Id, LayerId, Pos2, Rect, Vec2};

/// How long a single finger must stay within egui's click distance
/// (`InputOptions::max_click_dist`) before its long-press arms, in seconds.
///
/// Deliberately shorter than egui's own long-touch (`max_click_duration`, 0.8 s): an armed
/// press opens the context menu on release and turns a following drag into a selection, so
/// it has to feel responsive. Tuned value, unverified on hardware (the Galaxy Tab S9 device
/// test decides it).
pub const LONG_PRESS_ARM_S: f64 = 0.45;

/// Radius, in points, of the proximity rule: a mouse-button press or move that does NOT sit
/// next to a `Touch` event but lands this close to a claimed (or just-ended) touch is treated
/// as a platform-promoted copy of that touch and removed with it.
///
/// Platform-agnostic and inert where the platform never promotes touches to the mouse
/// (Linux, Android). Meant for Windows touch promotion, where gestures are still gated off,
/// so the value is unverified on hardware.
pub const PROMOTION_RADIUS_PT: f32 = 2.0;

/// How long after a claimed touch ended its position still attracts promoted mouse events
/// (see [`PROMOTION_RADIUS_PT`]), in seconds. Unverified on hardware, like the radius.
pub const PROMOTION_GRACE_S: f64 = 0.5;

/// One canvas surface a finger may be claimed on, published every frame by its owner.
///
/// `owner` is a content-named, stable id (it outlives a `MangaApp` rebuild in the same
/// `egui::Context`); it keys the output in [`take_touch_output`]. `rect` and `holes` are in
/// screen points of the ROOT viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchSurface {
    /// Stable id of the consumer; the same id is passed to [`take_touch_output`].
    pub owner: Id,
    /// The canvas viewport a touch must START in to be claimed.
    pub rect: Rect,
    /// Sub-rects of `rect` where a touch is NOT claimed and egui handles it as usual
    /// (aside cards, on-top bubbles, scrollbars, hint strips).
    pub holes: Vec<Rect>,
    /// `true` when the owner's active mode has a selection target, so a drag after the
    /// long-press armed becomes a [`TouchSelection`]; `false` makes that drag pan instead.
    pub long_press_selection: bool,
}

/// One frame of finger navigation for one owner.
///
/// Computed over the claimed navigating touches present both at the start and at the end of
/// the frame, so adding or lifting a finger never makes the page jump. Apply it as: zoom by
/// `zoom_factor` about `prev_centroid`, then move so that the world point that was under
/// `prev_centroid` lands under `centroid`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchNavigation {
    /// Centroid of the contributing touches at the start of the frame (screen points).
    pub prev_centroid: Pos2,
    /// Centroid of the same touches at the end of the frame (screen points).
    pub centroid: Pos2,
    /// Ratio of the touches' mean spread now to their mean spread before; exactly `1.0` for a
    /// single touch or a degenerate (near-zero) previous spread.
    pub zoom_factor: f32,
    /// Number of contributing touches (saturates at `u8::MAX`).
    pub touches: u8,
}

impl TouchNavigation {
    /// Screen-space movement of the centroid this frame.
    #[must_use]
    pub fn pan_delta(&self) -> Vec2 {
        self.centroid - self.prev_centroid
    }
}

/// How a [`TouchSelection`] finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchSelectionEnd {
    /// The finger was lifted: commit the selection exactly as a mouse release would.
    Released,
    /// A second finger, a mouse/pen button, a focus loss or a platform cancel interrupted it:
    /// discard the selection.
    Cancelled,
}

/// A long-press selection drag: a finger armed its long-press and then moved.
///
/// It is NOT a synthesized mouse press (in Cleaning the mouse buttons paint and erase); the
/// consumer drives the selection target of its active mode from it directly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchSelection {
    /// The press point the drag starts from (screen points).
    pub origin: Pos2,
    /// The finger's latest position (screen points).
    pub pos: Pos2,
    /// `true` on the first frame of this selection: start the selection at `origin`.
    pub began: bool,
    /// `Some` on the frame the selection finished. `began` and `end` may both be set when
    /// the whole drag happened within one frame.
    pub end: Option<TouchSelectionEnd>,
}

/// Everything one canvas receives from the router for one frame (take-once).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TouchOutput {
    /// Pan / pinch to apply this frame. `None` while a mouse or pen button is held (palm).
    pub navigation: Option<TouchNavigation>,
    /// Open the canvas context menu at this point (a finger armed its long-press and was
    /// lifted without moving). Pass it to [`context_menu_popup`].
    pub long_press_at: Option<Pos2>,
    /// A finger's long-press is armed and still held here: draw press feedback at this point.
    /// Reported on every frame while it lasts.
    pub long_press_armed_at: Option<Pos2>,
    /// The long-press selection drag, reported on every frame while it lasts.
    pub selection: Option<TouchSelection>,
}

/// One owner's output plus the repaint the router needs from this frame.
#[derive(Clone, Copy, Debug)]
struct Delivery {
    owner: Id,
    output: TouchOutput,
    /// Seconds after which a pending long-press of this owner arms. The repaint is requested
    /// from inside the pass (`take_touch_output`): a request made in the raw-input hook is
    /// reset by `begin_pass_repaint_logic` (`egui-0.36.2/src/context.rs:99-115`).
    repaint_after_s: Option<f64>,
}

fn surfaces_id() -> Id {
    Id::new("ms_touch_nav.surfaces")
}

fn capture_layers_id() -> Id {
    Id::new("ms_touch_nav.capture_layers")
}

fn holes_id() -> Id {
    Id::new("ms_touch_nav.holes")
}

fn deliveries_id() -> Id {
    Id::new("ms_touch_nav.deliveries")
}

fn router_live_id() -> Id {
    Id::new("ms_touch_nav.router_live")
}

/// Whether an enabled router drains this context's publications (see [`set_router_live`]).
fn router_live(ctx: &Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(router_live_id()).unwrap_or(false))
}

/// Records whether the router that just ran on the ROOT viewport is enabled.
///
/// The one gate of every `publish_*`: publications are drained only by an enabled router's
/// freeze step, so without one (the wasm entry has no raw-input hook; a disabled router returns
/// before routing) they would pile up in `ctx` data for the whole session. Written by every ROOT
/// routing call, so it always follows the router that actually runs.
fn set_router_live(ctx: &Context, live: bool) {
    ctx.data_mut(|data| {
        if live {
            data.insert_temp(router_live_id(), true);
        } else {
            data.remove_temp::<bool>(router_live_id());
        }
    });
}

/// Publishes `surface` for this frame; the router claims touches on it next frame.
///
/// Replaces an earlier publication of the same owner, so repeated passes of one frame are
/// idempotent. Call it every frame the canvas is drawn; a frame without a publication means
/// "no surface" for the next frame. A no-op while no enabled router runs (see the module notes).
pub fn publish_surface(ctx: &Context, surface: TouchSurface) {
    if !router_live(ctx) {
        return;
    }
    ctx.data_mut(|data| {
        let surfaces = data.get_temp_mut_or_default::<Vec<TouchSurface>>(surfaces_id());
        if let Some(slot) = surfaces.iter_mut().find(|slot| slot.owner == surface.owner) {
            *slot = surface;
        } else {
            surfaces.push(surface);
        }
    });
}

/// Publishes `layer` as a capture layer of `owner` for this frame: a touch that starts on
/// `owner`'s surface under this layer is still claimed (tool capture overlays such as a
/// selection or crop mode, where a finger must pan instead of operating the mode). A no-op
/// while no enabled router runs.
pub fn publish_capture_layer(ctx: &Context, owner: Id, layer: LayerId) {
    if !router_live(ctx) {
        return;
    }
    ctx.data_mut(|data| {
        let layers = data.get_temp_mut_or_default::<Vec<(Id, LayerId)>>(capture_layers_id());
        if !layers.contains(&(owner, layer)) {
            layers.push((owner, layer));
        }
    });
}

/// Publishes `rect` as an extra hole of `owner`'s surface for this frame: a touch that starts
/// inside it is NOT claimed and egui handles it as usual.
///
/// For widgets drawn AFTER the owner published its surface (a tool's on-canvas chrome that
/// runs after the canvas pass, such as the buttons and handles of a capture overlay), so they
/// stay operable by a finger. Merged into the owner's surface when the router freezes the
/// publications; a hole of an owner that published no surface this frame is dropped. A no-op
/// while no enabled router runs.
pub fn publish_hole(ctx: &Context, owner: Id, rect: Rect) {
    if !router_live(ctx) {
        return;
    }
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Vec<(Id, Rect)>>(holes_id()).push((owner, rect));
    });
}

/// Takes this frame's output for `owner` (take-once: a second call in the same frame, or a
/// later pass of it, returns the default).
///
/// Also requests the repaint that fires a pending long-press on time, so a consumer must call
/// it on every frame it publishes a surface.
#[must_use]
pub fn take_touch_output(ctx: &Context, owner: Id) -> TouchOutput {
    let delivery = ctx.data_mut(|data| {
        let deliveries = data.get_temp_mut_or_default::<Vec<Delivery>>(deliveries_id());
        let index = deliveries.iter().position(|delivery| delivery.owner == owner)?;
        Some(deliveries.swap_remove(index))
    });
    let Some(delivery) = delivery else {
        return TouchOutput::default();
    };
    if let Some(seconds) = delivery.repaint_after_s {
        // `try_from_secs_f64` rejects negative / NaN / overflowing values; the router only
        // produces finite non-negative ones, and an immediate repaint is the safe reading.
        match std::time::Duration::try_from_secs_f64(seconds) {
            Ok(wait) => ctx.request_repaint_after(wait),
            Err(_) => ctx.request_repaint(),
        }
    }
    delivery.output
}

/// Takes the surfaces, extra holes and capture layers published during the previous frame
/// (the router's freeze step), with every extra hole merged into its owner's surface. Leaves
/// all lists empty for the frame about to run.
fn freeze_publications(ctx: &Context) -> (Vec<TouchSurface>, Vec<(Id, LayerId)>) {
    ctx.data_mut(|data| {
        let mut surfaces = data.remove_temp::<Vec<TouchSurface>>(surfaces_id()).unwrap_or_default();
        let holes = data.remove_temp::<Vec<(Id, Rect)>>(holes_id()).unwrap_or_default();
        merge_holes(&mut surfaces, holes);
        let capture = data.remove_temp::<Vec<(Id, LayerId)>>(capture_layers_id()).unwrap_or_default();
        (surfaces, capture)
    })
}

/// Appends each `(owner, rect)` hole to the surface of that owner; holes of an owner without
/// a surface are dropped (there is nothing to claim on, so nothing to exempt).
fn merge_holes(surfaces: &mut [TouchSurface], holes: Vec<(Id, Rect)>) {
    for (owner, rect) in holes {
        if let Some(surface) = surfaces.iter_mut().find(|surface| surface.owner == owner) {
            surface.holes.push(rect);
        }
    }
}

/// Replaces the stored deliveries with this frame's (outputs not taken last frame are dropped).
fn store_deliveries(ctx: &Context, deliveries: Vec<Delivery>) {
    ctx.data_mut(|data| {
        data.insert_temp(deliveries_id(), deliveries);
    });
}
