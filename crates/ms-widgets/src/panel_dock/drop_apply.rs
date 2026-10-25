/*
File: crates/ms-widgets/src/panel_dock/drop_apply.rs

Purpose:
Applies a tab or panel drop once the frame knows which window it landed in
(`cross_window::address_drop`): into another of our windows, back into the source
window, or detached into a new sub-window. Also the panel mutations a window's
widgets report.

Key structures:
- `PanelMutation`: an activate / collapse toggle / resize a drawn panel reported.

Key functions:
- `apply_addressed_tab_drop` / `apply_addressed_panel_drop`: the three-way verdict.
- `apply_tab_landing`: lands a tab on what `cross_window::tab_landing` chose.
- `detach_tab_into_sub_window` / `detach_panel_into_sub_window`: open a new window.
- `resolve_window_placement`: where a new sub-window is placed.
- `apply_mutations`: applies the reported panel mutations (pure, unit-tested).
*/

use egui::{Pos2, Vec2};
use ms_log::runtime_log;

use super::{PanelDockState, cross_window};
use super::cross_window::{DropAddress, TabLanding};
use super::host_draw::{PendingPanelDrop, PendingTabDrop};
use super::menu_move::gesture_release_point;
use super::model::{DockLayout, DockModelError, HostId, PanelId, TabId};
use super::panel_drag::apply_tab_drop;
use super::solver::DOCK_GAP;
use super::sub_windows::window_geometries;
use super::window::DetachTrigger;

/// Applies a tab drop once the frame knows which window it landed in.
///
/// Three branches, and they are the whole model: it landed in another of our
/// windows (the tab moves there, into a header strip or onto bare dock area), it
/// landed on the bare desktop (a window of its own opens where it was released),
/// or it landed on something inside a window that takes no tabs — a panel's body,
/// or that window's own toolbar — which cancels the move, exactly as the
/// in-window rules do.
pub(super) fn apply_addressed_tab_drop(
    state: &mut PanelDockState,
    layout_key: &str,
    ctx: &egui::Context,
    request: PendingTabDrop,
) -> bool {
    let windows = window_geometries(ctx, state);
    let global = gesture_release_point(state, request.source, request.global);
    match cross_window::address_drop(&windows, request.source, global) {
        DropAddress::Window { host, local } => {
            let Some(record) = state
                .frame_hosts
                .iter()
                .find(|record| record.host == host)
            else {
                // The window exists but drew no dock this frame (another program
                // tab owns it): there is nothing there to land on.
                runtime_log::log_warn(format!(
                    "[panel_dock] the tab `{}` was released over a window that drew no panels \
                     this frame; the move is cancelled",
                    request.tab
                ));
                return false;
            };
            let landing = cross_window::tab_landing(record.area, &record.panels, local);
            apply_tab_landing(state, layout_key, request.tab, host, landing)
        }
        DropAddress::Desktop { place_at } => {
            let place_at = resolve_window_placement(state, place_at);
            detach_tab_into_sub_window(state, layout_key, request.tab, request.trigger, place_at)
        }
    }
}

/// Moves a tab into the window a cross-window drop addressed, at the place it
/// landed on. Returns `true` when the layout changed.
pub(super) fn apply_tab_landing(
    state: &mut PanelDockState,
    layout_key: &str,
    tab: TabId,
    host: HostId,
    landing: TabLanding,
) -> bool {
    let Some(layout) = state.layouts.get_mut(layout_key) else {
        return false;
    };
    let changed = match landing {
        TabLanding::HeaderStrip { panel, index } => apply_tab_drop(layout, panel, tab, index),
        TabLanding::BareArea { pos } => {
            match layout.detach_tab_to_host(tab, host, pos) {
                Ok(_) => true,
                Err(error) => {
                    runtime_log::log_warn(format!(
                        "[panel_dock] cannot give the tab `{tab}` a panel of its own in \
                         {host:?}: {error}"
                    ));
                    false
                }
            }
        }
        TabLanding::Cancelled => false,
    };
    if changed {
        state.dirty = true;
        runtime_log::log_info(format!(
            "[panel_dock] tab `{tab}` moved across a window border into {host:?} ({landing:?})"
        ));
    }
    changed
}

/// Applies a whole panel's move once the frame knows which window it landed in.
///
/// A panel that crossed a border becomes free-floating where it was dropped; it
/// does not snap to the receiving window's edges, because it never followed the
/// cursor there and the user was shown no docking preview in that window.
pub(super) fn apply_addressed_panel_drop(
    state: &mut PanelDockState,
    layout_key: &str,
    ctx: &egui::Context,
    request: PendingPanelDrop,
) -> bool {
    let windows = window_geometries(ctx, state);
    let global = gesture_release_point(state, request.source, request.global);
    match cross_window::address_drop(&windows, request.source, global) {
        DropAddress::Window { host, local } => {
            let Some(record) = state
                .frame_hosts
                .iter()
                .find(|record| record.host == host)
            else {
                return false;
            };
            let Some(pos) = cross_window::panel_landing(record.area, local, request.grab_offset)
            else {
                // Released inside the window but outside its dock area: cancelled,
                // and the panel stays where the source window last drew it.
                return false;
            };
            let Some(layout) = state.layouts.get_mut(layout_key) else {
                return false;
            };
            match layout.move_panel_to_host(request.panel, host, pos) {
                Ok(()) => {
                    state.dirty = true;
                    runtime_log::log_info(format!(
                        "[panel_dock] panel {} moved across a window border into {host:?}",
                        request.panel
                    ));
                    true
                }
                Err(error) => {
                    runtime_log::log_warn(format!(
                        "[panel_dock] cannot move panel {} into {host:?}: {error}",
                        request.panel
                    ));
                    false
                }
            }
        }
        DropAddress::Desktop { place_at } => {
            let place_at = resolve_window_placement(state, place_at);
            detach_panel_into_sub_window(
                state,
                layout_key,
                request.panel,
                request.trigger,
                place_at,
            )
        }
    }
}

/// Moves one tab into a panel of its own inside a brand-new sub-window.
///
/// Returns `false` — with a logged reason — when no window index is left or the
/// model refuses the move; the half-created window is rolled back in that case,
/// so a failure can never leave an empty window on screen.
pub(super) fn detach_tab_into_sub_window(
    state: &mut PanelDockState,
    layout_key: &str,
    tab: TabId,
    trigger: DetachTrigger,
    position: Option<Pos2>,
) -> bool {
    let Some(index) = state.allocate_sub_window(position) else {
        runtime_log::log_error(format!(
            "[panel_dock] cannot open a window for the tab `{tab}`: the sub-window index space is \
             exhausted"
        ));
        return false;
    };
    let host = HostId::SubWindow(index);
    let outcome = match state.layouts.get_mut(layout_key) {
        Some(layout) => layout.detach_tab_to_host(tab, host, Pos2::new(DOCK_GAP, DOCK_GAP)),
        None => Err(DockModelError::UnknownTab(tab)),
    };
    match outcome {
        Ok(_) => {
            state.dirty = true;
            runtime_log::log_info(format!(
                "[panel_dock] tab `{tab}` moved into sub-window {index} ({trigger:?})"
            ));
            true
        }
        Err(error) => {
            runtime_log::log_warn(format!(
                "[panel_dock] cannot move the tab `{tab}` into a window of its own: {error}"
            ));
            state.sub_windows.retain(|node| node.index != index);
            state.opened_sub_windows.remove(&index);
            false
        }
    }
}

/// Moves one whole panel — every tab it holds — into a brand-new sub-window.
///
/// Same rollback rule as [`detach_tab_into_sub_window`].
pub(super) fn detach_panel_into_sub_window(
    state: &mut PanelDockState,
    layout_key: &str,
    panel: PanelId,
    trigger: DetachTrigger,
    position: Option<Pos2>,
) -> bool {
    let Some(index) = state.allocate_sub_window(position) else {
        runtime_log::log_error(format!(
            "[panel_dock] cannot open a window for panel {panel}: the sub-window index space is \
             exhausted"
        ));
        return false;
    };
    let host = HostId::SubWindow(index);
    let outcome = match state.layouts.get_mut(layout_key) {
        Some(layout) => layout.move_panel_to_host(panel, host, Pos2::new(DOCK_GAP, DOCK_GAP)),
        None => Err(DockModelError::UnknownPanel(panel)),
    };
    match outcome {
        Ok(()) => {
            state.dirty = true;
            runtime_log::log_info(format!(
                "[panel_dock] panel {panel} moved into sub-window {index} ({trigger:?})"
            ));
            true
        }
        Err(error) => {
            runtime_log::log_warn(format!(
                "[panel_dock] cannot move panel {panel} into a window of its own: {error}"
            ));
            state.sub_windows.retain(|node| node.index != index);
            state.opened_sub_windows.remove(&index);
            false
        }
    }
}

/// Passes the prospective window position through, saying ONCE why there is none.
///
/// There is no global cursor position anywhere in egui, so a window can only be
/// placed under the cursor — and a drop can only be ADDRESSED to another of our
/// windows — where the platform tells a window where it itself is. Wayland never
/// does (`ViewportInfo::inner_rect` is always `None`), `outer_position()` returns
/// an error below egui, and `ViewportBuilder::with_position` is ignored there as
/// well. Both consequences are stated once instead of being papered over: a
/// detached window lands wherever the compositor puts it, and a tab dragged onto
/// an existing detached window opens a new one instead of moving into it.
/// Everything that does not need monitor coordinates keeps working there —
/// moving tabs and panels inside one window, tearing one out past the border, and
/// the «Переместить в окно →» context-menu submenu, which is the supported way to
/// move between windows on such a platform.
pub(super) fn resolve_window_placement(state: &mut PanelDockState, place_at: Option<Pos2>) -> Option<Pos2> {
    if place_at.is_none() && !state.warned_placement_unsupported {
        state.warned_placement_unsupported = true;
        runtime_log::log_warn(
            "[panel_dock] this session does not report window positions (Wayland, or a window \
             that has not been placed yet): a detached panel window opens where the compositor \
             puts it, its position cannot be restored between runs, and a tab DRAGGED onto an \
             existing detached window opens a new one instead of moving into it. Moving between \
             windows is done here through the «Переместить в окно» submenu in the context menu of \
             a tab caption or of a panel header, which needs no window coordinates at all",
        );
    }
    place_at
}

/// A change the widget reported and the driver has to write into the model.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum PanelMutation {
    /// Make this tab the panel's active one.
    Activate(PanelId, TabId),
    /// Flip the panel's collapsed flag.
    ToggleCollapsed(PanelId),
    /// Pin the panel's outer size to a manually dragged value.
    Resize(PanelId, Vec2),
}

/// Applies the frame's mutations, reporting any the model refuses.
///
/// A refusal means the widget reported something about a panel the layout no
/// longer describes the way the frame assumed (a tab that moved away, a panel
/// that is gone). It is dropped rather than forced: the model is the single
/// place where the invariants live.
pub(super) fn apply_mutations(layout: &mut DockLayout, mutations: &[PanelMutation]) {
    for mutation in mutations {
        let outcome = match *mutation {
            PanelMutation::Activate(panel, tab) => layout.set_active_tab(panel, tab),
            PanelMutation::ToggleCollapsed(panel) => {
                let collapsed = layout.panel(panel).is_some_and(|node| node.collapsed);
                layout.set_collapsed(panel, !collapsed)
            }
            PanelMutation::Resize(panel, size) => layout.set_size_override(panel, Some(size)),
        };
        if let Err(error) = outcome {
            runtime_log::log_warn(format!(
                "[panel_dock] the change {mutation:?} reported this frame was refused by \
                 the layout model and dropped: {error}"
            ));
        }
    }
}
