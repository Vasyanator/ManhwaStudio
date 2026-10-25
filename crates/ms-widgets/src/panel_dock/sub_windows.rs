/*
File: crates/ms-widgets/src/panel_dock/sub_windows.rs

Purpose:
The egui plumbing of the detached OS windows and of gestures that cross a window
border: live window geometry, the receiving window's feedback, the immediate and
idle (deferred) sub-window passes, and the end-of-frame application of every drop
the windows left unresolved.

Key structures:
- `CarriedGesture`: what a gesture that crossed a border carries, as seen from a
  window that is not the one it started in.

Key functions:
- `window_geometries`: every window of the dock in the shared monitor frame.
- `carried_gesture` / `paint_cross_window_feedback`: the receiving window's marks.
- `show_sub_windows`: one immediate child viewport per sub-window.
- `observe_sub_window_geometry` / `apply_sub_window_geometry` /
  `take_idle_window_reports` / `idle_sub_window_pass`: geometry and close-button
  hand-over, immediate and idle.
- `apply_frame_detaches`: applies the frame's pending drops and menu move in order.

Notes:
The DECISIONS (where a drop lands, when a window opens or closes) are pure functions
in `cross_window.rs` and `window.rs`; this file only feeds them live data and
applies their verdicts.
*/

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use egui::{Pos2, Rect, Vec2};

use super::{PanelDockOutput, PanelDockState, TabEntry, TabMeta, cross_window, drag, window};
use super::cross_window::{PanelDropTarget, TabLanding, WindowGeometry};
use super::drag::DraggedTab;
use super::drop_apply::{apply_addressed_panel_drop, apply_addressed_tab_drop};
use super::host_draw::{FrameContext, HostFrame, HostOutcome, draw_host};
use super::menu_move::apply_menu_move;
use super::model::{HostId, PanelId, TabId};
use super::window::SubWindowNode;

/// Geometry of every window of this dock, in the shared monitor frame.
///
/// `RawInput::viewports` carries the info of ALL viewports in every pass
/// (fork `crates/egui/crates/eframe/src/native/glow_integration.rs:659-663`, `:1916-1921`), so one
/// window can read where the others are without waiting for their passes. The
/// entries for immediate viewports are one frame old, which a drag — repainting
/// continuously — cannot notice.
///
/// A window whose `inner_rect` is `None` contributes nothing: that is Wayland,
/// where no window position exists at any level (`cross_window.rs`), and also a
/// minimised window. A window the platform reports as not visible is left out for
/// the same reason — it cannot receive a drop the user aimed at it.
///
/// **Limitation, stated rather than hidden:** full occlusion is the only stacking
/// information any of this exposes (`ViewportInfo::visible`). Neither egui nor
/// winit can say which window is above which, so a sub-window the user pushed
/// BEHIND the main window still claims the points it covers (`window_at`'s
/// overlap rule). Raising it again — or moving it aside — is the workaround, and
/// there is no API that would let the dock do better.
pub(super) fn window_geometries(ctx: &egui::Context, state: &PanelDockState) -> Vec<WindowGeometry> {
    // Read outside `Context::input`: reading the context again from inside that
    // closure deadlocks (fork `crates/egui/crates/egui/src/context.rs:988-998`).
    let zoom = ctx.zoom_factor();
    let main_viewport = state.main_viewport;
    ctx.input(|input| {
        let mut windows: Vec<WindowGeometry> = Vec::with_capacity(state.sub_windows.len() + 1);
        let mut push = |host: HostId, viewport: egui::ViewportId| {
            let Some(info) = input.raw.viewports.get(&viewport) else {
                return;
            };
            let Some(inner_rect) = info.inner_rect else {
                return;
            };
            if info.visible() == Some(false) {
                return;
            }
            // The factor egui-winit itself uses to turn points into the pixels
            // the window manager speaks (fork `crates/egui/crates/egui-winit/src/lib.rs:54-58`).
            let pixels_per_point = zoom * info.native_pixels_per_point.unwrap_or(1.0);
            if let Some(window) = WindowGeometry::new(host, inner_rect, pixels_per_point) {
                windows.push(window);
            }
        };
        if let Some(viewport) = main_viewport {
            push(HostId::MainWindow, viewport);
        }
        for node in &state.sub_windows {
            push(node.host(), window::sub_window_viewport_id(node.index));
        }
        windows
    })
}

/// What a gesture that crossed a window border is carrying, as far as the
/// receiving window's feedback is concerned.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum CarriedGesture {
    /// One tab header.
    Tab {
        /// Where inside the caption it was grabbed.
        grab_offset: Vec2,
        /// Outer size of the caption being carried.
        header_size: Vec2,
    },
    /// A whole panel.
    Panel {
        /// Where inside the panel it was grabbed.
        grab_offset: Vec2,
        /// Outer size of the panel being carried.
        size: Vec2,
    },
}

/// What the gesture in flight is carrying, from the point of view of a window
/// that is NOT the one it started in.
///
/// `None` when nothing is being carried or when the gesture STARTED in `host`:
/// that window's own pass already draws the caption under the cursor, the docking
/// line and the tear-out outline, and a second mark from the driver would only
/// repeat them.
pub(super) fn carried_gesture(
    ctx: &egui::Context,
    state: &PanelDockState,
    layout_key: &str,
    host: HostId,
) -> Option<CarriedGesture> {
    if let Some(payload) = egui::DragAndDrop::payload::<DraggedTab>(ctx) {
        // The tab is still owned by its source panel for the whole drag: it only
        // changes hands when the drop is applied.
        let source = state
            .layouts
            .get(layout_key)
            .and_then(|layout| layout.panel_of_tab(payload.tab))
            .and_then(|panel| layout_host(state, layout_key, panel));
        if source == Some(host) {
            return None;
        }
        return Some(CarriedGesture::Tab {
            grab_offset: payload.grab_offset,
            header_size: payload.header_size,
        });
    }
    let session = state.drag.as_ref().filter(|session| session.host != host)?;
    Some(CarriedGesture::Panel {
        grab_offset: session.grab_offset,
        size: session.carried_size,
    })
}

/// Window one panel of `layout_key` lives in.
fn layout_host(state: &PanelDockState, layout_key: &str, panel: PanelId) -> Option<HostId> {
    state
        .layouts
        .get(layout_key)
        .and_then(|layout| layout.panel(panel))
        .map(|node| node.host)
}

/// Paints, in the window the cursor is over, what a drag that crossed a window
/// border would land on.
///
/// The receiving window cannot paint this through its own widgets: a held button
/// keeps the pointer in the window the press started in, so nothing here is
/// hovered and `dnd_hover_payload` reports nothing. The marks are the same ones
/// the in-window gestures use — a line at the insertion point of a header strip,
/// a dashed contour of what would become a panel of its own — so the gesture
/// looks identical whether or not it crossed a border.
pub(super) fn paint_cross_window_feedback(
    ctx: &egui::Context,
    area: Rect,
    targets: &[PanelDropTarget],
    local: Pos2,
    carried: CarriedGesture,
) {
    match carried {
        CarriedGesture::Tab {
            grab_offset,
            header_size,
        } => match cross_window::tab_landing(area, targets, local) {
            TabLanding::HeaderStrip { panel, .. } => {
                if let Some(target) = targets.iter().find(|target| target.panel == panel) {
                    // The marker of the very slot `tab_landing` chose: one rule
                    // (`drag::insertion_slot`) for the index, the caption row and
                    // the marker, shared with the widget's own strip.
                    let slot = drag::insertion_slot(&target.header_rects, target.header_strip, local);
                    drag::paint_insertion_marker(ctx, slot);
                }
            }
            TabLanding::BareArea { pos } => {
                drag::paint_detach_preview(ctx, Rect::from_min_size(pos - grab_offset, header_size));
            }
            TabLanding::Cancelled => {}
        },
        CarriedGesture::Panel { grab_offset, size } => {
            if cross_window::panel_landing(area, local, grab_offset).is_some() {
                drag::paint_detach_preview(ctx, Rect::from_min_size(local - grab_offset, size));
            }
        }
    }
}

/// Draws one immediate child viewport per sub-window and the panels the program
/// tab being drawn assigns to each of them; a window it assigns nothing stays open
/// and grey (requirement 11). A frame where no program tab of this dock draws
/// shows the windows through `PanelDockState::show_idle_sub_windows` (deferred)
/// instead; the reports those idle windows left behind are applied here first
/// (close requests join `closed_windows`), and the idle slots are dropped because
/// every window is immediate again from this pass on.
///
/// In Embedded mode the list is always empty here: `apply_window_mode` re-docked
/// every sub-window at the top of the pass, so no viewport is shown (egui would
/// draw one as a window INSIDE the main one, which is no window of its own).
///
/// Immediate, never deferred, while a program tab draws: the deferred form needs
/// `Fn + Send + Sync + 'static` (fork `crates/egui/crates/egui/src/context.rs:4069`),
/// which the caller's tab bodies — borrowing the typing tab's hundred-field
/// states — cannot satisfy. The price is that parent and child repaint together;
/// for a window showing a program tab's panels that is the accepted cost of the
/// feature and must not be "optimised" away. Only IDLE windows (nothing of the
/// dock draws) are deferred.
pub(super) fn show_sub_windows<'frame, C>(
    ctx: &egui::Context,
    state: &mut PanelDockState,
    frame: FrameContext<'_>,
    decls: &BTreeMap<TabId, TabMeta>,
    entries: &mut BTreeMap<TabId, TabEntry<'frame, C>>,
    cx: &mut C,
    output: &mut PanelDockOutput,
) -> HostOutcome {
    let mut outcome = HostOutcome {
        closed_windows: take_idle_window_reports(state),
        ..HostOutcome::default()
    };
    state.idle_windows.clear();
    if state.sub_windows.is_empty() {
        return outcome;
    }
    let nodes: Vec<SubWindowNode> = state.sub_windows.clone();
    for node in nodes {
        // The stored position is a START position, applied once. Re-asserting it
        // every frame would fight the window manager and the user over a window
        // they are allowed to move.
        let first_frame = state.opened_sub_windows.insert(node.index);
        // The very name the «Переместить в окно →» submenu offers, so the window
        // the user picks there is the one they can read off the title bar.
        let title = window::sub_window_title(node.index);
        let builder =
            window::sub_window_builder(&node, &title, if first_frame { node.pos } else { None });
        ctx.show_viewport_immediate(
            window::sub_window_viewport_id(node.index),
            builder,
            |ui, _class| {
                // `ui.ctx()` is the same `Context` with THIS viewport current, so
                // every read below — pointer, viewport info, drawn areas — is the
                // child window's own.
                let child = ui.ctx().clone();
                if child.input(|input| input.viewport().close_requested()) {
                    outcome.closed_windows.push(node.index);
                }
                observe_sub_window_geometry(&child, state, node);
                // The neutral background of a canvas-less window (plan §4.9); the
                // panels themselves are `Area`s floating above it, so the panel
                // only supplies the fill and the rect they are laid out in.
                let area = egui::CentralPanel::default()
                    .show(ui, |ui| ui.max_rect())
                    .inner;
                outcome.merge(draw_host(
                    &child,
                    state,
                    HostFrame {
                        host: node.host(),
                        area,
                        layout_key: frame.layout_key,
                        gesture_in_flight: frame.gesture_in_flight,
                    },
                    decls,
                    entries,
                    cx,
                    output,
                ));
            },
        );
    }
    outcome
}

/// Refreshes one sub-window's stored geometry from what the window reported.
///
/// The position comes from the OUTER rect and the size from the INNER one,
/// because that is exactly what `ViewportBuilder::with_position` /
/// `with_inner_size` consume, so a restored window lands where it was left. Both
/// are `None` on Wayland, where the compositor does not tell a client where its
/// window is; the last known position is then kept rather than erased.
pub(super) fn observe_sub_window_geometry(
    ctx: &egui::Context,
    state: &mut PanelDockState,
    node: SubWindowNode,
) {
    let (outer, inner) = ctx.input(|input| {
        let viewport = input.viewport();
        (viewport.outer_rect, viewport.inner_rect)
    });
    apply_sub_window_geometry(state, node, outer, inner);
}

/// Folds one sub-window's reported `outer` / `inner` rects into its stored
/// geometry and raises the dirty flag when it changed — the one rule both the
/// immediate form (`observe_sub_window_geometry`) and an idle window's report
/// (`take_idle_window_reports`) go through.
fn apply_sub_window_geometry(
    state: &mut PanelDockState,
    node: SubWindowNode,
    outer: Option<Rect>,
    inner: Option<Rect>,
) {
    // Sanitize FIRST and compare afterwards. Comparing the raw report against the
    // stored (already clamped) value would report a difference on every single
    // frame as soon as the window manager hands back a size below
    // `MIN_SUB_WINDOW_SIZE` — and the persistence writer, which is driven off the
    // dirty flag, would then write for the rest of the session.
    let candidate = SubWindowNode::new(
        node.index,
        outer.map(|rect| rect.min).or(node.pos),
        inner.map_or(node.size, |rect| rect.size()),
    );
    if !window::geometry_changed(&node, candidate.pos, candidate.size) {
        return;
    }
    let Some(stored) = state
        .sub_windows
        .iter_mut()
        .find(|stored| stored.index == node.index)
    else {
        return;
    };
    *stored = candidate;
    state.dirty = true;
}

/// Locks one idle window's report. A poisoned lock can only come from a panic
/// inside `IdleWindowReport::record` / `take`, which do plain field writes; the
/// value is still a consistent report, so it is used as is.
fn lock_idle_report(report: &Mutex<window::IdleWindowReport>) -> std::sync::MutexGuard<'_, window::IdleWindowReport> {
    report.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Applies what every idle (deferred) sub-window reported since the main
/// window's last pass: a changed geometry is stored exactly as the immediate form
/// stores it (`apply_sub_window_geometry`, dirty flag included), and the indices
/// whose close button was pressed are returned for the caller to close.
pub(super) fn take_idle_window_reports(state: &mut PanelDockState) -> Vec<u32> {
    let taken: Vec<(u32, window::IdleWindowTake)> = state
        .idle_windows
        .iter()
        .map(|(index, report)| (*index, lock_idle_report(report).take()))
        .collect();
    let mut closed = Vec::new();
    for (index, take) in taken {
        if let Some((outer, inner)) = take.geometry
            && let Some(node) = state.sub_windows.iter().find(|node| node.index == index).copied()
        {
            apply_sub_window_geometry(state, node, outer, inner);
        }
        if take.close_requested {
            closed.push(index);
        }
    }
    closed
}

/// One pass of an IDLE sub-window (a deferred viewport, run by eframe outside
/// the main window's pass): records the close button and geometry into `report`,
/// draws the neutral background, and wakes `main_viewport` when the report holds
/// something new for it.
pub(super) fn idle_sub_window_pass(ui: &mut egui::Ui, report: &Mutex<window::IdleWindowReport>, main_viewport: egui::ViewportId) {
    let (close_requested, outer, inner) = ui.ctx().input(|input| {
        let viewport = input.viewport();
        (viewport.close_requested(), viewport.outer_rect, viewport.inner_rect)
    });
    let wake = lock_idle_report(report).record(close_requested, outer, inner);
    egui::CentralPanel::default().show(ui, |_ui| {});
    if wake {
        ui.ctx().request_repaint_of(main_viewport);
    }
}

/// Applies the drops every window of this frame left unresolved, in the order
/// that keeps them from contradicting each other.
///
/// This runs after EVERY window has drawn, which is what makes cross-window
/// addressing possible: the window a drop landed in may have drawn before the
/// window the gesture started in, and only now does the frame know where all of
/// them are and what each of them offered.
///
/// The drag drop is the only one that is conditional: its payload is taken here
/// and nowhere earlier, so a drop another of our windows claimed through its own
/// header strip during the same pass wins over the "it ended outside" verdict of
/// the window it started in. Returns `true` when something changed.
pub(super) fn apply_frame_detaches(
    state: &mut PanelDockState,
    layout_key: &str,
    ctx: &egui::Context,
    outcome: &HostOutcome,
) -> bool {
    let mut changed = false;
    if let Some(request) = outcome.menu_move {
        changed |= apply_menu_move(state, layout_key, ctx, request);
    }
    if let Some(request) = outcome.tab_drop
        && egui::DragAndDrop::take_payload::<DraggedTab>(ctx).is_some()
    {
        changed |= apply_addressed_tab_drop(state, layout_key, ctx, request);
    }
    if let Some(request) = outcome.panel_drop {
        changed |= apply_addressed_panel_drop(state, layout_key, ctx, request);
    }
    changed
}
