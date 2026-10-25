/*
File: crates/ms-widgets/src/panel_dock/menu_move.rs

Purpose:
The «Переместить в окно →» context-menu move of a tab or a whole panel: the one
cross-window move that needs no pointer, window geometry or monitor coordinates,
and therefore the only one that works on Wayland.

Key functions:
- `apply_menu_move`: entry point, into an existing window or a new one.
- `move_into_existing_host`: lands the subject through the same model calls a
  cross-window drop uses.
- `menu_tab_landing` / `menu_panel_slot` / `centered_slot_in_host` /
  `step_off_occupied` / `menu_subject_size` / `free_slot_in_host`: where the
  subject lands, decided from the model and the kind of receiving window.
- `focus_host`: raises a sub-window that received a move.
- `gesture_release_point`: where a drag gesture ended in the shared monitor frame.

Notes:
The landing helpers are pure and unit-tested in `tests.rs`.
*/

use std::collections::HashMap;

use egui::{Pos2, Rect, Vec2};
use ms_log::runtime_log;

use super::{AUTO_PANEL_CASCADE_STEP, PanelDockState, window};
use super::cross_window::TabLanding;
use super::drop_apply::{apply_tab_landing, detach_panel_into_sub_window, detach_tab_into_sub_window, resolve_window_placement};
use super::host_draw::{MenuMoveSubject, PendingMenuMove};
use super::model::{DockLayout, HostId, TabId};
use super::solver::{DEFAULT_PANEL_SIZE, DOCK_GAP};
use super::window::{DetachTrigger, MoveTarget};

/// Applies an explicit «Переместить в окно →» from a context menu.
///
/// This is the platform-independent path: it reads no pointer, no window
/// geometry and no monitor coordinates, so it behaves identically on X11,
/// Windows, macOS and Wayland — and on Wayland it is the ONLY way to move a tab
/// or a panel between windows at all (plan §4.8).
///
/// A move into a window that ALREADY exists raises that window afterwards, so
/// the user sees where the tab went. A move into a NEW window does not: the
/// window is created this very frame and the window manager gives a freshly
/// mapped window focus itself.
pub(super) fn apply_menu_move(
    state: &mut PanelDockState,
    layout_key: &str,
    ctx: &egui::Context,
    request: PendingMenuMove,
) -> bool {
    match request.target {
        MoveTarget::NewWindow => {
            let place_at = resolve_window_placement(state, request.place_at);
            match request.subject {
                MenuMoveSubject::Tab(tab) => detach_tab_into_sub_window(
                    state,
                    layout_key,
                    tab,
                    DetachTrigger::ContextMenu,
                    place_at,
                ),
                MenuMoveSubject::Panel(panel) => detach_panel_into_sub_window(
                    state,
                    layout_key,
                    panel,
                    DetachTrigger::ContextMenu,
                    place_at,
                ),
            }
        }
        MoveTarget::Existing(host) => {
            let changed = move_into_existing_host(state, layout_key, request.subject, host);
            if changed {
                focus_host(state, ctx, host);
            }
            changed
        }
    }
}

/// Moves a tab or a whole panel into a window that already exists.
///
/// The landing depends on WHICH KIND of window receives it, because the two are
/// used for opposite things (`menu_tab_landing`, [`menu_panel_slot`]): the main
/// window is the large one the user is looking at and already full of panels, so
/// the newcomer gets a panel of its own in the MIDDLE of the dock area; a
/// sub-window is a small tool window opened for panels, so a tab joins the panel
/// already there. Everything is then applied through the very model calls a
/// cross-window DROP uses ([`apply_tab_landing`],
/// [`DockLayout::move_panel_to_host`]), so an emptied source panel and an
/// emptied window are cleaned up exactly as after a drag.
pub(super) fn move_into_existing_host(
    state: &mut PanelDockState,
    layout_key: &str,
    subject: MenuMoveSubject,
    host: HostId,
) -> bool {
    let area_size = state.host_areas.get(&host).map(Rect::size);
    // The subject is a tab of THIS program tab's arrangement, so its size comes
    // from the measurements taken while this arrangement was drawn.
    let no_measurements = HashMap::new();
    match subject {
        MenuMoveSubject::Tab(tab) => {
            let measured = state.measurements(layout_key).unwrap_or(&no_measurements);
            let Some(layout) = state.layouts.get(layout_key) else {
                return false;
            };
            let panel_size = menu_subject_size(measured, layout, subject);
            let landing = menu_tab_landing(layout, tab, host, area_size, panel_size);
            apply_tab_landing(state, layout_key, tab, host, landing)
        }
        MenuMoveSubject::Panel(panel) => {
            let measured = state.measurements(layout_key).unwrap_or(&no_measurements);
            let Some(layout) = state.layouts.get(layout_key) else {
                return false;
            };
            let panel_size = menu_subject_size(measured, layout, subject);
            let pos = menu_panel_slot(layout, host, area_size, panel_size);
            let Some(layout) = state.layouts.get_mut(layout_key) else {
                return false;
            };
            match layout.move_panel_to_host(panel, host, pos) {
                Ok(()) => {
                    state.dirty = true;
                    runtime_log::log_info(format!(
                        "[panel_dock] panel {panel} moved into {host:?} from its context menu"
                    ));
                    true
                }
                Err(error) => {
                    runtime_log::log_warn(format!(
                        "[panel_dock] cannot move panel {panel} into {host:?}: {error}"
                    ));
                    false
                }
            }
        }
    }
}

/// Where a tab moved by the «Переместить в окно →» menu lands inside `host`.
///
/// The menu has no cursor, so the landing is decided from the MODEL instead of
/// from geometry — and from the KIND of window that receives it, because the two
/// kinds are used for opposite things:
///
/// * **the main window** always gives the tab a panel of its OWN, in the MIDDLE
///   of its dock area ([`centered_slot_in_host`]). It is the window the user is
///   looking at and it is already full of panels: appending the tab to whichever
///   panel happens to be first there buries it behind an unrelated caption, in a
///   corner of the canvas, and the user has to hunt for what they just moved;
/// * **a sub-window that already holds panels** takes the tab into the END of the
///   FIRST of them — the oldest one, which is also the one the solver lays out
///   first — exactly as a drop onto its header strip would. A sub-window is a
///   small tool window opened FOR panels, so collecting tabs in the one that is
///   there is what the user asked for by picking that window;
/// * **a sub-window with no panels** gives the tab a panel of its own, at the
///   cascade slot [`free_slot_in_host`] picks.
///
/// `area_size` is the destination's dock area as it was drawn this frame
/// (`PanelDockState::host_areas`) and `panel_size` the size the newcomer will be
/// laid out at; both are only consulted by the centring rule.
///
/// [`TabLanding::Cancelled`] when the tab's own panel is the only one in `host`:
/// the tab is already exactly where the menu would put it. The menu never offers
/// the host a tab is currently in, so this only guards against a stale request.
pub(super) fn menu_tab_landing(
    layout: &DockLayout,
    tab: TabId,
    host: HostId,
    area_size: Option<Vec2>,
    panel_size: Vec2,
) -> TabLanding {
    let source = layout.panel_of_tab(tab);
    let source_is_here = source
        .and_then(|panel| layout.panel(panel))
        .is_some_and(|node| node.host == host);
    let receiver = layout
        .panels_in_host(host)
        .map(|node| node.id)
        .find(|id| Some(*id) != source);
    match host {
        HostId::MainWindow => {
            if receiver.is_none() && source_is_here {
                TabLanding::Cancelled
            } else {
                TabLanding::BareArea {
                    pos: centered_slot_in_host(layout, host, area_size, panel_size),
                }
            }
        }
        HostId::SubWindow(_) => match receiver {
            Some(panel) => TabLanding::HeaderStrip {
                panel,
                // The end of that panel's strip. `move_tab` clamps anyway, but a
                // request that names the real count says what it means.
                index: layout.panel(panel).map_or(0, |node| node.tabs.len()),
            },
            None if source_is_here => TabLanding::Cancelled,
            None => TabLanding::BareArea {
                pos: free_slot_in_host(layout, host),
            },
        },
    }
}

/// Where a WHOLE panel moved by the «Переместить в окно →» menu lands inside
/// `host`.
///
/// The same split as `menu_tab_landing`, minus the merging: a panel cannot join
/// another one, so both branches only choose a free-floating position. The main
/// window centres it for the same reason a tab gets a centred panel there — the
/// user must see what they just moved — and a sub-window cascades it clear of
/// whatever is already inside.
#[must_use]
fn menu_panel_slot(
    layout: &DockLayout,
    host: HostId,
    area_size: Option<Vec2>,
    panel_size: Vec2,
) -> Pos2 {
    match host {
        HostId::MainWindow => centered_slot_in_host(layout, host, area_size, panel_size),
        HostId::SubWindow(_) => free_slot_in_host(layout, host),
    }
}

/// Position, in `host`'s area coordinates, that puts a panel of `panel_size` in
/// the MIDDLE of a dock area of `area_size` without burying a panel already
/// standing there.
///
/// `area_size` is `None` for a window that drew no dock this frame — nothing then
/// says how big the middle is, and the answer degrades to the corner cascade
/// [`free_slot_in_host`] rather than to an invented coordinate. A panel larger
/// than the area would centre at a negative offset; it is placed at the cascade
/// origin instead, which is where the solver would clamp it anyway.
#[must_use]
pub(super) fn centered_slot_in_host(
    layout: &DockLayout,
    host: HostId,
    area_size: Option<Vec2>,
    panel_size: Vec2,
) -> Pos2 {
    let Some(area_size) = area_size else {
        return free_slot_in_host(layout, host);
    };
    // `f32::max` returns the other operand for a NaN one, so a garbage size
    // degrades to the cascade origin instead of poisoning the position.
    let centered = Pos2::new(
        ((area_size.x - panel_size.x) * 0.5).max(DOCK_GAP),
        ((area_size.y - panel_size.y) * 0.5).max(DOCK_GAP),
    );
    step_off_occupied(layout, host, centered)
}

/// Steps `pos` along the cascade until no panel of `host` stands on it.
///
/// "Stands on it" is deliberately coarse: two panels less than
/// [`AUTO_PANEL_CASCADE_STEP`] apart on BOTH axes are stacked as far as the user
/// is concerned — the lower one's header strip is covered — and the newcomer must
/// not be what covers it. Panel positions are the ones the last solve wrote back
/// (`write_back_positions`), so this compares against where the panels really
/// were drawn.
///
/// Terminates: every step moves the candidate strictly further along both axes,
/// so a panel it has passed can never block it again and at most one step per
/// panel of `host` is needed.
#[must_use]
pub(super) fn step_off_occupied(layout: &DockLayout, host: HostId, pos: Pos2) -> Pos2 {
    let mut candidate = pos;
    let panels = layout.panels_in_host(host).count();
    for _ in 0..=panels {
        let blocked = layout.panels_in_host(host).any(|node| {
            (node.pos.x - candidate.x).abs() < AUTO_PANEL_CASCADE_STEP
                && (node.pos.y - candidate.y).abs() < AUTO_PANEL_CASCADE_STEP
        });
        if !blocked {
            break;
        }
        candidate += Vec2::splat(AUTO_PANEL_CASCADE_STEP);
    }
    candidate
}

/// Outer size the panel a MENU move creates (or moves) will be laid out at.
///
/// For a TAB it is the size that tab was last measured at — the dock has one for
/// every tab it has drawn, and a tab whose caption the user just right-clicked
/// has been drawn. For a whole PANEL it is the size the user pinned it to, else
/// the largest measurement over the tabs it holds ("a panel is as big as its
/// largest tab"). [`DEFAULT_PANEL_SIZE`] when nothing is known, so a placement is
/// always possible.
#[must_use]
fn menu_subject_size(
    measured: &HashMap<TabId, Vec2>,
    layout: &DockLayout,
    subject: MenuMoveSubject,
) -> Vec2 {
    match subject {
        MenuMoveSubject::Tab(tab) => measured.get(&tab).copied().unwrap_or(DEFAULT_PANEL_SIZE),
        MenuMoveSubject::Panel(panel) => layout.panel(panel).map_or(DEFAULT_PANEL_SIZE, |node| {
            node.size_override
                .or_else(|| {
                    node.tabs
                        .iter()
                        .filter_map(|tab| measured.get(tab).copied())
                        .reduce(Vec2::max)
                })
                .unwrap_or(DEFAULT_PANEL_SIZE)
        }),
    }
}

/// Free-floating position inside `host` for a panel that arrives without a
/// cursor to place it.
///
/// The same cascade [`ensure_declared_tabs`] uses, stepped by how many panels
/// `host` already holds, so the newcomer neither lands exactly on one of them nor
/// hides one behind itself.
///
/// The sibling QUEUE rule (`drag::resolve_slot`) is deliberately NOT used here:
/// it resolves ANCHORED slots — two panels docked to the same edge of the same
/// target — while a panel moved by the menu arrives `Free`, anchored to nothing,
/// and has no docking candidate to queue behind.
#[must_use]
pub(super) fn free_slot_in_host(layout: &DockLayout, host: HostId) -> Pos2 {
    let occupied = layout.panels_in_host(host).count();
    let step = f32::from(u16::try_from(occupied).unwrap_or(u16::MAX));
    let offset = DOCK_GAP + step * AUTO_PANEL_CASCADE_STEP;
    Pos2::new(offset, offset)
}

/// Raises the window a menu move sent something into, so the user sees where it
/// went.
///
/// `ViewportCommand::Focus` is sent TO the destination viewport
/// (fork `crates/egui/crates/egui/src/context.rs:4030`,
/// `egui-0.36.2/src/viewport.rs:1169`) and the window manager
/// does the rest. It is a REQUEST: egui states that it has no effect on Wayland,
/// or on a minimised or hidden window, and nothing here depends on it succeeding
/// — the move itself has already been applied to the model.
fn focus_host(state: &PanelDockState, ctx: &egui::Context, host: HostId) {
    let viewport = match host {
        HostId::MainWindow => state.main_viewport,
        HostId::SubWindow(index) => Some(window::sub_window_viewport_id(index)),
    };
    if let Some(viewport) = viewport {
        ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
    }
}

/// Where the gesture ended in the shared monitor frame.
///
/// The window that saw the release answers first; a window that lost the pointer
/// altogether (`PointerGone`, on a platform that hands the cursor to whatever it
/// entered instead of keeping an implicit grab) falls back to the reading ANOTHER
/// of our windows published THIS frame — never to an older one, which would
/// address the drop against wherever the cursor happened to be during an earlier
/// part of the gesture.
pub(super) fn gesture_release_point(
    state: &PanelDockState,
    source: HostId,
    global: Option<Pos2>,
) -> Option<Pos2> {
    global.or_else(|| {
        state
            .frame_hosts
            .iter()
            .find(|record| record.host != source && record.pointer_global.is_some())
            .and_then(|record| record.pointer_global)
    })
}
