/*
File: crates/ms-widgets/src/panel_dock/panel_drag.rs

Purpose:
Applies the in-window reorganisation gestures to the live layout: the panel-move
drag (begin, advance, snap, drop or detach), a tab dropped on a header strip, and
the write-back of solved positions into free panels.

Key structures:
- `DragPhase`: what the panel-move gesture is doing this frame.

Key functions:
- `advance_panel_drag` / `begin_panel_drag` / `snap_candidate`: the gesture.
- `apply_panel_drop` / `apply_tab_drop`: the model mutation of a drop.
- `write_back_positions`: stores the solved positions back into the layout.

Notes:
How a gesture DECIDES (snap search, sibling rule, insertion slot) is pure and
lives in `drag.rs`; this file only applies the decisions.
*/

use std::collections::BTreeMap;

use egui::{Pos2, Rect, Vec2};
use ms_log::runtime_log;

use super::{PanelDockState, drag, window};
use super::drag::{DragSession, SnapCandidate, SnapTargets};
use super::model::{DockLayout, HostId, PanelAnchor, PanelId, TabId};
use super::solver::SolvedLayout;
use super::window::{DetachTrigger, DragEndContext, DragTension};

/// What the panel-move gesture is doing this frame.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum DragPhase {
    /// No panel is being moved in this window.
    Idle,
    /// A panel is following the pointer.
    Moving {
        /// The panel being moved.
        panel: PanelId,
        /// How far the gesture has been pulled past the dock area's border. It
        /// decides which preview is painted: the docking line while the pointer
        /// is inside the area or still being resisted, the tear-out outline once
        /// the panel has broken free.
        tension: DragTension,
        /// Top-left, in this window's screen coordinates, the panel WOULD have
        /// if nothing held it back — the pointer minus the offset it was grabbed
        /// at. While the pointer is outside the area the panel itself is pinned
        /// to the border by the solver's clamp, so this is exactly where the
        /// tear-out outline goes. `None` while the pointer is not reportable at
        /// all, when there is nothing to paint at.
        torn_origin: Option<Pos2>,
    },
    /// The gesture ended this frame.
    Released {
        /// The panel that was being moved.
        panel: PanelId,
        /// Where inside the panel it was grabbed, so a window it lands in can put
        /// that same point back under the cursor.
        grab_offset: Vec2,
        /// Set when the gesture ended past the tension threshold — or over
        /// another of our windows — which takes the drop out of this window's
        /// hands and hands it to the frame's address resolution.
        detach: Option<DetachTrigger>,
    },
}

/// Moves the dragged panel to where the pointer is now, and decides whether the
/// gesture continues, ends in place, or ends outside the window.
///
/// The panel's stored `pos` is recomputed from the pointer rather than
/// accumulated from per-frame deltas: an accumulated delta drifts away from the
/// cursor whenever a frame is dropped, and the panel would no longer sit where it
/// was grabbed.
///
/// The panel RESISTS at the dock area's border: its stored `pos` is written
/// unclamped, and the solver's chain clamp (`solver::fitting_shift`) holds the
/// drawn panel inside the area, so a cursor that keeps going outside only opens a
/// gap between the border and itself. That gap is the tension `window::drag_tension`
/// measures, and past `window::DETACH_TENSION_DISTANCE` the panel tears off.
/// Nothing is latched: coming back inside restores ordinary docking.
///
/// A pointer that leaves the window while the button is held does NOT end the
/// gesture (`PointerGone` deliberately keeps the drag alive,
/// `egui-0.36.2/src/input_state/mod.rs:1203-1213`): the session is latched as
/// "outside" and the panel simply stops following, so the user can come back —
/// which clears the latch again — or release out there, which detaches. Ending it
/// on the spot instead would make an accidental brush past the window border look
/// exactly like a deliberate drag-out.
///
/// The session is dropped outright when the layout being drawn is not the one it
/// started in (the user switched program tabs) or when its panel is gone (a tab
/// drop emptied it). A session belonging to ANOTHER window of the same layout is
/// left untouched: every window runs this once per frame.
///
/// `owns_pointer` is the frame's shared-frame verdict "the cursor is over THIS
/// window". A window that does not own it never resolves the release itself: the
/// drop belongs to another of our windows, and only the frame's address
/// resolution can say which one.
pub(super) fn advance_panel_drag(
    state: &mut PanelDockState,
    layout_key: &str,
    host: HostId,
    area: Rect,
    ctx: &egui::Context,
    owns_pointer: bool,
) -> DragPhase {
    let Some(session) = state.drag.clone() else {
        return DragPhase::Idle;
    };
    if session.layout_key != layout_key {
        state.drag = None;
        return DragPhase::Idle;
    }
    if session.host != host {
        return DragPhase::Idle;
    }
    let (pointer, down) =
        ctx.input(|input| (input.pointer.interact_pos(), input.pointer.primary_down()));
    let Some(pointer) = pointer else {
        if down {
            if let Some(session) = state.drag.as_mut() {
                session.left_window = true;
            }
            // The cursor is somewhere this window cannot name: further out than
            // any threshold measured against its own dock area.
            return DragPhase::Moving {
                panel: session.panel,
                tension: DragTension::TornOff,
                torn_origin: None,
            };
        }
        // The button came up while the pointer was not reportable at all: the
        // release happened somewhere we cannot name, which is the detach case.
        state.drag = None;
        return DragPhase::Released {
            panel: session.panel,
            grab_offset: session.grab_offset,
            detach: window::detach_verdict(
                state.detach_allowed,
                DragEndContext {
                    pointer_left_window: true,
                    release_pos: None,
                    area,
                },
                owns_pointer,
            ),
        };
    };
    // The pointer is back (or never left): the latch must not outlive it, or an
    // Alt-Tab in the middle of a drag would detach the panel on the next click.
    if let Some(session) = state.drag.as_mut() {
        session.left_window = false;
    }
    // Where the panel wants to be, relative to the area's top-left, with nothing
    // holding it back. Written to the model unclamped; the solver's chain clamp
    // is what pins the DRAWN panel to the border while the cursor pulls further.
    let moved = session.panel_origin + (pointer - session.grab_pointer);
    if let Some(layout) = state.layouts.get_mut(layout_key) {
        if let Err(error) = layout.set_panel_pos(session.panel, moved) {
            runtime_log::log_warn(format!(
                "[panel_dock] the panel being moved is gone ({error}); the gesture is cancelled"
            ));
            state.drag = None;
            return DragPhase::Idle;
        }
        // A move IS a user change, unlike the position write-back at the end of
        // the frame: persistence has to store where the panel ended up.
        state.dirty = true;
    }
    if down {
        DragPhase::Moving {
            panel: session.panel,
            tension: window::drag_tension(area, Some(pointer), false),
            // The unclamped position, in this window's screen coordinates.
            torn_origin: Some(area.min + moved.to_vec2()),
        }
    } else {
        state.drag = None;
        // A release whose cursor is over another of our windows leaves this
        // window entirely, whatever the tension against ITS dock area says.
        let detach = window::detach_verdict(
            state.detach_allowed,
            DragEndContext {
                pointer_left_window: false,
                release_pos: Some(pointer),
                area,
            },
            owns_pointer,
        );
        DragPhase::Released {
            panel: session.panel,
            grab_offset: session.grab_offset,
            detach,
        }
    }
}

/// Starts a panel move from the header handle's `drag_started` report.
///
/// Detaching the panel immediately (anchor `Free`, position taken from the rect
/// it was solved at) is what makes the gesture possible at all: an anchored panel
/// is placed by its target, so moving it would have no visible effect. Its own
/// dependants stay anchored to it and follow along.
pub(super) fn begin_panel_drag(
    state: &mut PanelDockState,
    layout_key: &str,
    host: HostId,
    ctx: &egui::Context,
    panel: PanelId,
    solved: &SolvedLayout,
    area: Rect,
) {
    let Some(geometry) = solved.get(panel) else {
        return;
    };
    // The press origin, not the current position: the drag is reported once the
    // pointer has travelled past egui's drag threshold, and anchoring the gesture
    // to the current position would make the panel jump by that threshold.
    let Some(grab_pointer) = ctx.input(|input| {
        input
            .pointer
            .press_origin()
            .or_else(|| input.pointer.interact_pos())
    }) else {
        return;
    };
    let panel_origin = Pos2::new(
        geometry.rect.left() - area.left(),
        geometry.rect.top() - area.top(),
    );
    let Some(layout) = state.layouts.get_mut(layout_key) else {
        return;
    };
    if let Err(error) = layout.set_anchor(panel, PanelAnchor::Free) {
        runtime_log::log_warn(format!(
            "[panel_dock] cannot detach panel {panel} for a move: {error}"
        ));
        return;
    }
    if let Err(error) = layout.set_panel_pos(panel, panel_origin) {
        runtime_log::log_warn(format!(
            "[panel_dock] cannot place panel {panel} at the start of a move: {error}"
        ));
        return;
    }
    state.drag = Some(DragSession {
        layout_key: layout_key.to_owned(),
        host,
        panel,
        grab_pointer,
        panel_origin,
        // Both are carried for the window the gesture may END in: it never laid
        // this panel out, so it can neither measure it nor know where it was
        // grabbed.
        grab_offset: grab_pointer - geometry.rect.min,
        carried_size: geometry.rect.size(),
        left_window: false,
    });
    state.dirty = true;
}

/// The edge a dragged panel would dock to right now, from this frame's solved
/// rects. `None` when nothing is within [`SNAP_DISTANCE`], which means "the panel
/// stays free-floating".
pub(super) fn snap_candidate(
    layout: &DockLayout,
    panel: PanelId,
    solved: &SolvedLayout,
    area: Rect,
) -> Option<SnapCandidate> {
    let dragged = solved.get(panel)?.rect;
    let candidates = drag::panel_snap_candidates(
        layout,
        panel,
        solved.iter().map(|(id, geometry)| (id, geometry.rect)),
    );
    drag::find_snap(dragged, SnapTargets {
        area,
        panels: &candidates,
    })
}

/// Writes the anchor a released panel earned, after the sibling rule has had its
/// say. Returns `true` when the layout changed.
///
/// A release with no snap target leaves the panel `Free` where it was dropped —
/// which it already is, because the gesture detached it when it started.
pub(super) fn apply_panel_drop(
    layout: &mut DockLayout,
    panel: PanelId,
    solved: &SolvedLayout,
    area: Rect,
) -> bool {
    let Some(geometry) = solved.get(panel) else {
        return false;
    };
    let Some(candidate) = snap_candidate(layout, panel, solved, area) else {
        return true;
    };
    let rects: BTreeMap<PanelId, Rect> = solved
        .iter()
        .map(|(id, geometry)| (id, geometry.rect))
        .collect();
    let anchor = drag::resolve_slot(
        layout,
        panel,
        geometry.rect.size(),
        candidate.anchor,
        &rects,
        area,
    );
    match layout.set_anchor(panel, anchor) {
        Ok(()) => true,
        Err(error) => {
            runtime_log::log_warn(format!(
                "[panel_dock] the layout model refused to dock panel {panel}: {error}"
            ));
            false
        }
    }
}

/// Moves a dropped tab into the panel whose header strip received it, and makes
/// it that panel's active tab. Returns `true` when the layout changed.
///
/// The emptied source panel is removed by [`DockLayout::move_tab`] itself
/// (requirement 10) — the driver must not do it a second time.
pub(super) fn apply_tab_drop(layout: &mut DockLayout, target: PanelId, tab: TabId, index: usize) -> bool {
    if let Err(error) = layout.move_tab(tab, target, index) {
        runtime_log::log_warn(format!(
            "[panel_dock] cannot move the dropped tab `{tab}` into panel {target}: {error}"
        ));
        return false;
    }
    if let Err(error) = layout.set_active_tab(target, tab) {
        // Unreachable: the tab was just moved into this very panel.
        runtime_log::log_warn(format!(
            "[panel_dock] the dropped tab `{tab}` is not showable in panel {target}: {error}"
        ));
    }
    true
}

/// Refreshes every solved panel's stored `pos` from the rect it was laid out at.
///
/// `pos` is authoritative only for a free panel, but an anchored one keeps it as
/// the cache the model falls back to the moment its anchor stops resolving —
/// which happens on ordinary frames, not only in corrupt layouts: `frame_layout`
/// drops a panel with nothing to draw and hands its OWN anchor to its dependants,
/// so a dependant of a hidden FREE root becomes free itself and is placed by its
/// `pos` alone. The same fallback carries a panel whose anchor target lives in
/// another host and one restored from a stored anchor this build no longer has
/// (`persist::decode_anchor`). Without this refresh the fallback position is
/// whatever the panel was created with (usually the area's origin), so a panel
/// loses its place and jumps into the corner the frame its neighbour is hidden.
///
/// Deliberately does NOT dirty the dock state: a position derived from a solve
/// the user did not ask for is not a change persistence has to store, and
/// flagging it every frame would make the dirty flag meaningless.
pub(super) fn write_back_positions(layout: &mut DockLayout, solved: &SolvedLayout, origin: Pos2) {
    for (id, panel) in solved.iter() {
        let pos = Pos2::new(panel.rect.left() - origin.x, panel.rect.top() - origin.y);
        if let Err(error) = layout.set_panel_pos(id, pos) {
            // Unreachable: `solved` was produced from this layout's own panels.
            runtime_log::log_warn(format!(
                "[panel_dock] cannot store the solved position of panel {id}: {error}"
            ));
        }
    }
}
