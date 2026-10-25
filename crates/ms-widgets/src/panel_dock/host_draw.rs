/*
File: crates/ms-widgets/src/panel_dock/host_draw.rs

Purpose:
One window's pass of the dock: draws every drawable panel of ONE window (the main
one or a sub-window) and applies what its widgets reported, leaving the decisions
only the whole frame can take to `PanelDock::end`.

Key structures:
- `HostFrame`: which window is drawn and where in it.
- `FrameContext`: what the frame as a whole tells every window it draws.
- `HostRecord`: what one window drew while a gesture is in flight, hit-tested at
  the end of the frame on behalf of the window the cursor is over.
- `PendingTabDrop` / `PendingPanelDrop` / `PendingMenuMove` (+ `MenuMoveSubject`):
  drops and menu moves no window consumed, resolved once every window has drawn.
- `HostOutcome`: what one window's pass left for the frame to apply.

Key functions:
- `draw_host`: the body every window shares — plan, solve, draw, gestures,
  write-back for one `HostId`.

Notes:
`draw_host` reads `ctx` while its window's viewport is the current one, which is
what makes the pointer, the gestures and the drawn `Area`s per-window without a
branch on the host. Planning lives in `frame_plan.rs`, gesture decisions in
`panel_drag.rs`, cross-window feedback in `sub_windows.rs`.
*/

use std::collections::{BTreeMap, HashMap};

use egui::{Pos2, Rect, Vec2};
use ms_log::runtime_log;

use super::{MEASUREMENT_EPSILON, PanelDockOutput, PanelDockState, TabEntry, TabMeta, cross_window, drag, header_metrics_changed, panel, pinned_height_changed, window};
use super::cross_window::PanelDropTarget;
use super::drag::DraggedTab;
use super::drop_apply::{PanelMutation, apply_mutations};
use super::frame_plan::{frame_layout, plan_frame};
use super::model::{HostId, PanelId, TabId};
use super::panel::{CollapsiblePanel, MoveTargetEntry, PanelSection, PanelTabHeader};
use super::panel_drag::{DragPhase, advance_panel_drag, apply_panel_drop, apply_tab_drop, begin_panel_drag, snap_candidate, write_back_positions};
use super::solver::{PANEL_MIN_CONTENT_HEIGHT, solve_with_headers};
use super::sub_windows::{carried_gesture, paint_cross_window_feedback, window_geometries};
use super::window::{DetachTrigger, DragEndContext, MoveTarget};

/// Which window `draw_host` is drawing, and where in it.
#[derive(Copy, Clone, Debug)]
pub(super) struct HostFrame<'a> {
    pub(super) host: HostId,
    pub(super) area: Rect,
    pub(super) layout_key: &'a str,
    /// `true` while a tab or panel is being moved somewhere in this dock.
    ///
    /// Decided ONCE per frame, before any window draws, because the gesture ENDS
    /// during the owning window's pass: a flag re-read per window would be false
    /// for every window drawn after the release, and those are exactly the
    /// windows whose geometry the drop has to be addressed against. While it is
    /// false nothing is recorded and the dock behaves exactly as it did before
    /// cross-window addressing existed.
    pub(super) gesture_in_flight: bool,
}

/// What the frame as a whole tells every window it draws.
#[derive(Copy, Clone, Debug)]
pub(super) struct FrameContext<'a> {
    /// Program tab being drawn. (A frame where no tab of this dock draws shows
    /// its sub-windows through [`PanelDockState::show_idle_sub_windows`] instead.)
    pub(super) layout_key: &'a str,
    /// `true` while a tab or panel is being moved somewhere in this dock. See
    /// [`HostFrame::gesture_in_flight`] for why it is decided once per frame.
    pub(super) gesture_in_flight: bool,
}

/// What one window drew this frame, as far as a drop that crossed a window
/// border needs to know.
///
/// Recorded by `draw_host` while a gesture is in flight and read at the end of
/// the frame, when every window has had its pass. That order is what lets the
/// window the cursor was over accept a drop it never saw: it cannot hit-test
/// (the pointer is grabbed by the window the drag started in), so the driver
/// hit-tests these rects for it.
#[derive(Clone, Debug)]
pub(super) struct HostRecord {
    /// The window.
    pub(super) host: HostId,
    /// Its dock area, in its own screen coordinates.
    pub(super) area: Rect,
    /// Every panel it drew, in draw order.
    pub(super) panels: Vec<PanelDropTarget>,
    /// Pointer position this window reported THIS frame, in the shared monitor
    /// frame. `None` when it saw no pointer, or has no geometry to lift one with.
    pub(super) pointer_global: Option<Pos2>,
}

/// A tab drag no window consumed, waiting for the frame's address resolution.
///
/// It is NOT yet a detach: where it goes is decided from the shared monitor frame
/// once every window has drawn (`cross_window::address_drop`), and it becomes a
/// new window only if it landed on none of ours.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct PendingTabDrop {
    pub(super) tab: TabId,
    /// Where inside its caption the tab was grabbed; a panel created for it is
    /// placed by this offset so the caption stays under the cursor.
    grab_offset: Vec2,
    /// The window whose pass saw the release.
    pub(super) source: HostId,
    /// Release point in the shared monitor frame, in physical pixels. `None`
    /// when this window could not lift its pointer into that frame.
    pub(super) global: Option<Pos2>,
    /// Why the source window let the drop go; only used when it turns into a
    /// window of its own, and logged there.
    pub(super) trigger: DetachTrigger,
}

/// A whole panel whose move no window consumed. Same resolution as
/// [`PendingTabDrop`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct PendingPanelDrop {
    pub(super) panel: PanelId,
    /// Where inside the panel it was grabbed, so the panel keeps that point under
    /// the cursor when it lands in another window.
    pub(super) grab_offset: Vec2,
    pub(super) source: HostId,
    pub(super) global: Option<Pos2>,
    pub(super) trigger: DetachTrigger,
}

/// What a «Переместить в окно →» menu item asked to move.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum MenuMoveSubject {
    /// One tab, taken out of the panel that holds it.
    Tab(TabId),
    /// A whole panel with every tab it holds.
    Panel(PanelId),
}

/// An explicit «Переместить в окно →» from a tab caption's or a panel header's
/// context menu.
///
/// The one path that needs no pointer information at all — no window geometry,
/// no monitor coordinates, no pointer grab — which is why it is the only
/// cross-window move that works on Wayland (plan §4.8).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) struct PendingMenuMove {
    /// What is being moved.
    pub(super) subject: MenuMoveSubject,
    /// Where it is going.
    pub(super) target: MoveTarget,
    /// Prospective outer position of a NEW window in monitor space, in points.
    /// `None` where the platform does not report window positions (Wayland), and
    /// unused for a move into a window that already exists.
    pub(super) place_at: Option<Pos2>,
}

/// What one window's pass decided and left for the frame to apply.
///
/// The drops are collected rather than applied on the spot because they are
/// decisions ABOUT the whole dock — the window they belong to may not have drawn
/// yet, a new sub-window may have to be allocated, and a drop that another window
/// may still claim must not be consumed by the first window that saw the release.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct HostOutcome {
    pub(super) needs_repaint: bool,
    /// A tab drag no window consumed, still holding its payload.
    pub(super) tab_drop: Option<PendingTabDrop>,
    /// An explicit «Переместить в окно →» from a context menu.
    pub(super) menu_move: Option<PendingMenuMove>,
    /// A panel move that ended outside the dock area it started in.
    pub(super) panel_drop: Option<PendingPanelDrop>,
    /// Sub-windows whose own close button was pressed this frame.
    pub(super) closed_windows: Vec<u32>,
}

impl HostOutcome {
    /// Folds a later window's outcome into this one. The FIRST window that
    /// decided something keeps the decision: every gesture here is driven by the
    /// single pointer, so a second one is a contradiction, not an addition.
    pub(super) fn merge(&mut self, other: Self) {
        self.needs_repaint |= other.needs_repaint;
        self.tab_drop = self.tab_drop.or(other.tab_drop);
        self.menu_move = self.menu_move.or(other.menu_move);
        self.panel_drop = self.panel_drop.or(other.panel_drop);
        self.closed_windows.extend(other.closed_windows);
    }
}

/// Draws every drawable panel of ONE window and applies what its widgets
/// reported, returning the decisions only the whole frame can take.
///
/// This is the body every window shares: the main one and each sub-window run it
/// with their own area and their own `HostId`, reading `ctx` while that window's
/// viewport is the current one — which is what makes the pointer, the drag
/// gestures and the drawn `Area`s per-window without a single branch on the host.
pub(super) fn draw_host<'frame, C>(
    ctx: &egui::Context,
    state: &mut PanelDockState,
    frame: HostFrame<'_>,
    decls: &BTreeMap<TabId, TabMeta>,
    entries: &mut BTreeMap<TabId, TabEntry<'frame, C>>,
    cx: &mut C,
    output: &mut PanelDockOutput,
) -> HostOutcome {
    let HostFrame {
        host,
        area,
        layout_key,
        gesture_in_flight,
    } = frame;
    let mut outcome = HostOutcome::default();
    // Recorded before anything can return early: a menu move applied at the end
    // of the frame asks how big this window's dock area is, and a window whose
    // layout is missing still has one.
    state.host_areas.insert(host, area);

    let carries_tab = egui::DragAndDrop::has_payload_of_type::<DraggedTab>(ctx);
    // Everything this window's pass knows about the pointer, read once.
    // `inner_rect` is where the window's content starts in monitor space; it is
    // `None` on Wayland, which is precisely why every position derived from it is
    // an option and not a coordinate (`cross_window.rs`, plan §4.8).
    let (released, pointer_pos, latest_pos, any_down, window_origin) = ctx.input(|input| {
        (
            input.pointer.any_released(),
            input.pointer.interact_pos(),
            input.pointer.latest_pos(),
            input.pointer.any_down(),
            input.viewport().inner_rect.map(|rect| rect.min),
        )
    });

    // Where the pointer is in THIS window decides whether a tab drag has left
    // every window we own. A window that can still see the pointer clears the
    // flag, so the sub-window passes that follow the main one can undo the
    // main window's "the cursor is gone" reading — which is exactly what a drag
    // that crossed into a sub-window looks like from the main window.
    if carries_tab {
        if latest_pos.is_some() {
            state.tab_drag_left_window = false;
        } else if any_down {
            state.tab_drag_left_window = true;
        }
    } else {
        state.tab_drag_left_window = false;
    }

    // THE SHARED MONITOR FRAME. Where every window of this dock is, and where the
    // gesture is inside it. Built only while something is being dragged: it costs
    // one read of the all-viewports map plus a small vector, and an idle frame
    // must stay free.
    let windows = if gesture_in_flight {
        window_geometries(ctx, state)
    } else {
        Vec::new()
    };
    let this_window = windows.iter().copied().find(|window| window.host == host);
    let local_global = this_window
        .zip(pointer_pos)
        .and_then(|(window, pointer)| window.to_global(pointer));
    // Published only while the cursor is LIVE here (`latest_pos`), never from the
    // one extra frame `interact_pos` survives a `PointerGone` for
    // (`egui-0.36.2/src/input_state/mod.rs:1106`, `:1207`): that frame's value is
    // where the cursor WAS, and a window drawn after the one that really has it
    // would otherwise overwrite the live reading with it.
    if gesture_in_flight && latest_pos.is_some() && local_global.is_some() {
        state.drag_pointer_global = local_global;
    }
    // This window's own reading when it has one, else whatever another of our
    // windows published: while a button is held the pointer belongs to the window
    // the press started in, so the window the cursor is OVER is usually not the
    // one that can see it.
    let gesture_global = local_global.or(state.drag_pointer_global);
    // Which of our windows the cursor is over, if any. `None` covers three cases
    // that all mean the same thing here: nothing is being dragged, the platform
    // reports no window geometry at all (Wayland), or the cursor is on the bare
    // desktop — and in every one of them each window keeps answering for itself.
    let pointer_host = gesture_global
        .filter(|_| gesture_in_flight)
        .and_then(|global| cross_window::window_at(&windows, global))
        .map(|window| window.host);
    // A window stops owning the drop only when ANOTHER of ours owns it. A cursor
    // out on the desktop must NOT take the gesture away from the window it
    // started in: that is the tear-out the tension model exists for, and the
    // dashed outline that announces it is painted from this flag.
    let owns_pointer = pointer_host.is_none_or(|owner| owner == host);

    // A panel move in flight is advanced BEFORE the solve, from the pointer
    // position rather than from the widget's frame-delayed drag delta, so the
    // panel is laid out under the cursor in the same frame the cursor moved.
    let drag_phase = advance_panel_drag(state, layout_key, host, area, ctx, owns_pointer);

    // The layout borrow ends here: everything the drawing loop needs is
    // copied into `plan` / `solved`, so `state.measured` stays writable
    // while panels draw.
    let (plan, solved) = {
        let Some(layout) = state.layouts.get(layout_key) else {
            return outcome;
        };
        // Only THIS program tab's measurements: the same `TabId` measured while
        // another program tab was drawn describes another arrangement.
        let no_measurements = HashMap::new();
        let measured = state.measurements(layout_key).unwrap_or(&no_measurements);
        let no_headers = HashMap::new();
        let headers = state.header_measurements(layout_key).unwrap_or(&no_headers);
        let plan = plan_frame(layout, host, decls, measured, headers);
        // Panels with nothing to draw are removed from the layout the SOLVER
        // sees, never from the stored one: a hidden panel must not reserve
        // its slot in the chain, or the panels below it would float over a
        // gap where nothing is drawn.
        let effective = frame_layout(layout, &plan);
        // Last frame's header rows and caption widths, per panel: width ->
        // rows -> height is a one-way chain, so solving with them converges
        // within the same one-frame lag as every other measurement.
        let solved = solve_with_headers(
            &effective,
            host,
            area,
            &plan.desired,
            &plan.mins,
            state.chrome,
            &plan.headers,
        );
        (plan, solved)
    };

    let mut mutations: Vec<PanelMutation> = Vec::new();
    let mut chrome = state.chrome;
    // Gestures reported by the widgets this frame. At most one of each can
    // happen: they are all driven by the single pointer.
    let mut started_drag: Option<PanelId> = None;
    let mut tab_drop: Option<(PanelId, panel::TabDrop)> = None;
    let mut reset_requested = false;
    let mut menu_move: Option<(MenuMoveSubject, MoveTarget)> = None;
    let mut drawn_rects: Vec<Rect> = Vec::new();
    // What a drop that crossed a window border would land on here. Collected only
    // while a gesture is in flight; see `HostRecord`.
    let mut drop_targets: Vec<PanelDropTarget> = Vec::new();

    // Destinations of the «Переместить в окно →» submenu. Built ONCE per window:
    // every panel drawn here lives in the same host and therefore offers the same
    // destinations, and the labels must outlive the borrowed slice below.
    let move_labels: Vec<(MoveTarget, String)> = window::move_targets(&state.sub_windows, host, state.detach_allowed)
        .into_iter()
        .map(|target| (target, window::move_target_label(target)))
        .collect();
    let move_entries: Vec<MoveTargetEntry<'_>> = move_labels
        .iter()
        .map(|(target, label)| MoveTargetEntry {
            target: *target,
            label: label.as_str(),
        })
        .collect();

    for panel_plan in &plan.panels {
        let Some(active) = panel_plan.active_tab else {
            // Nothing declared and visible in this panel: it keeps its slot
            // in the stored layout, but it is neither solved nor drawn.
            continue;
        };
        let Some(geometry) = solved.get(panel_plan.id) else {
            continue;
        };

        // Titles are produced once per frame and must outlive the header
        // slice that borrows them.
        let titles: Vec<(TabId, String)> = panel_plan
            .visible_tabs
            .iter()
            .map(|tab| {
                let title = entries
                    .get(tab)
                    .map_or_else(|| tab.as_str().to_owned(), |entry| (entry.title)());
                (*tab, title)
            })
            .collect();
        let headers: Vec<PanelTabHeader<'_>> = titles
            .iter()
            .map(|(id, title)| PanelTabHeader {
                id: *id,
                title: title.as_str(),
            })
            .collect();

        // The panel's minimum, not the active tab's: the panel is sized by its
        // largest tab, so the grip must not let the user drag it below what
        // another of its tabs declared. The widest caption is a floor too —
        // the panel cannot be drawn narrower — so a resize may not pin a width
        // the solver would only have to raise again.
        let previous_header = panel_plan.header;
        let header_floor = previous_header.unwrap_or_default();
        let declared_min = plan.mins.get(panel_plan.id).unwrap_or(Vec2::new(
            0.0,
            state.chrome.with_header(header_floor).collapsed_height + PANEL_MIN_CONTENT_HEIGHT,
        ));
        let min_size = Vec2::new(declared_min.x.max(header_floor.min_width), declared_min.y);
        // What must keep a transparent panel visible even with the pointer
        // nowhere near it. Both are gestures the panel is the TARGET of, and a
        // panel that faded out in the middle of one would blink at the user:
        // a tab in flight anywhere in the dock has to have every transparent
        // panel to aim at, and a panel being MOVED is pinned to the dock area's
        // border while the cursor is pulled outside it (the tension model), so
        // the cursor is regularly not over the panel it is dragging.
        let force_visible = carries_tab
            || matches!(drag_phase, DragPhase::Moving { panel, .. } if panel == panel_plan.id);
        let bodies = entries.get_mut(&active).and_then(|entry| entry.bodies.take());
        let (has_pinned_top, has_pinned_bottom) = bodies.as_ref().map_or((false, false), |b| {
            (b.pinned_top.is_some(), b.pinned_bottom.is_some())
        });
        let (mut body, mut pinned_top, mut pinned_bottom) = match bodies {
            Some(b) => (Some(b.body), b.pinned_top, b.pinned_bottom),
            None => (None, None, None),
        };
        // Last frame's height of the pinned bottom section, or nothing reserved
        // on its first frame: the section is then clipped for one frame and the
        // repaint below lays it out properly in the next.
        let previous_pinned_bottom = if has_pinned_bottom {
            state.pinned_bottom_height(layout_key, active)
        } else {
            None
        };
        // The tab's persisted extra state is taken OUT of the dock state for the
        // duration of the body: the closure below borrows this local, so nothing
        // of `state` is borrowed across `CollapsiblePanel::show` — which still
        // has to reach `state` for `remember_measurement` further down. It is
        // handed to the DRAWN tab only, which is what makes it per-tab state
        // rather than per-panel state.
        let mut extras = state.take_tab_extras(layout_key, active);
        let drawn = CollapsiblePanel::new(panel_plan.id, layout_key)
            .geometry(geometry)
            .collapsed(panel_plan.collapsed)
            .tabs(&headers, Some(active))
            .min_size(min_size)
            .move_targets(&move_entries)
            .transparent_until_hover(panel_plan.transparent)
            .force_visible(force_visible)
            // A window that merely still receives pointer events must not claim a
            // drop the user made over a window floating above it.
            .accepts_drop(owns_pointer)
            .pinned_sections(has_pinned_top, has_pinned_bottom)
            .pinned_bottom_estimate(previous_pinned_bottom.unwrap_or(0.0))
            // ONE closure for the three sections, so `cx` is borrowed once and
            // lent to each section in turn; every closure is taken, so none can
            // run twice.
            .show_sections(ctx, |ui, section| match section {
                PanelSection::PinnedTop => {
                    if let Some(top) = pinned_top.take() {
                        top(ui, cx);
                    }
                }
                PanelSection::Scroll => {
                    if let Some(body) = body.take() {
                        body(ui, cx, &mut extras);
                    }
                }
                PanelSection::PinnedBottom => {
                    if let Some(bottom) = pinned_bottom.take() {
                        bottom(ui, cx);
                    }
                }
            });
        // Back into the state, raising `dirty` only for a REAL change — see
        // `finish_tab_extras`. Outside the `show` closure above, like every other
        // touch of `state` in this loop.
        state.finish_tab_extras(layout_key, active, extras);
        chrome = drawn.chrome;
        // The solve above used last frame's header rows; a header that now
        // wraps differently (the panel's width changed, a caption appeared)
        // needs one more frame to get the height it takes.
        if header_metrics_changed(previous_header, drawn.header) {
            outcome.needs_repaint = true;
        }
        state.remember_header_metrics(layout_key, panel_plan.id, &panel_plan.visible_tabs, drawn.header);
        if let Some(height) = drawn.pinned_bottom_height {
            // The scroll viewport was sized from the PREVIOUS height; a section
            // that changed needs one more frame to get the room it now takes.
            if pinned_height_changed(previous_pinned_bottom, height) {
                outcome.needs_repaint = true;
            }
            state.remember_pinned_bottom_height(layout_key, active, height);
        }

        drawn_rects.push(drawn.rect);
        if gesture_in_flight {
            drop_targets.push(PanelDropTarget {
                panel: panel_plan.id,
                rect: drawn.rect,
                header_strip: drawn.header_strip,
                header_rects: drawn.header_rects.clone(),
            });
        }
        // Recorded WITH the host: the same output is filled by every window of the
        // dock, and a sub-window's rect is in that window's own frame.
        output.record(host, panel_plan.id, drawn.rect, &panel_plan.visible_tabs);

        if let Some(activated) = drawn.activated_tab
            && activated != active
        {
            mutations.push(PanelMutation::Activate(panel_plan.id, activated));
        }
        if drawn.toggle_collapsed {
            mutations.push(PanelMutation::ToggleCollapsed(panel_plan.id));
        }
        if drawn.drag_started {
            started_drag = Some(panel_plan.id);
        }
        if let Some(drop) = drawn.tab_drop {
            tab_drop = Some((panel_plan.id, drop));
        }
        if drawn.reset_layout {
            reset_requested = true;
        }
        if let Some((tab, target)) = drawn.move_tab {
            menu_move = Some((MenuMoveSubject::Tab(tab), target));
        }
        if let Some(target) = drawn.move_panel {
            menu_move = Some((MenuMoveSubject::Panel(panel_plan.id), target));
        }
        if let Some(size) = drawn.size_override {
            mutations.push(PanelMutation::Resize(panel_plan.id, size));
        }
        if let Some(measured) = drawn.measured_size {
            // The HEIGHT this tab CONTRIBUTED to the panel's request is compared
            // with the height its content turned out to need, so the frame that
            // learns something new about a tab is followed by one that acts on
            // it. Three things this must not be:
            // * the panel's `assumed_size` — the request is the maximum over the
            //   panel's tabs, so a tab smaller than its panel would differ from
            //   it on every frame and repaint forever;
            // * `plan.desired` alone — a tab declaring neither a measurement nor
            //   a size has no entry there, so comparing it with itself never
            //   asked for the second frame, while the solver had laid the panel
            //   out at `DEFAULT_PANEL_SIZE` and everything docked under it
            //   overlapped it until an unrelated event woke egui up. That case is
            //   the `None` arm below: nothing was contributed, so the first
            //   measurement is always news;
            // * a width comparison — the width is never measured from the
            //   content (see `measured_size`), so a difference there only ever
            //   means the solver shrank the panel to fit, and re-solving would
            //   reproduce it frame after frame.
            let changed = panel_plan
                .active_request
                .is_none_or(|request| (request.y - measured.y).abs() >= MEASUREMENT_EPSILON);
            if changed {
                outcome.needs_repaint = true;
            }
            // Only the HEIGHT is remembered from what was drawn. The drawn
            // width is by construction the width the panel was given, so
            // storing it would turn a width the solver shrank to fit a
            // narrow area into the panel's own request — and the panel would
            // never widen again when the area does. What the PANEL asked for is
            // the stable quantity, and a horizontal overflow now scrolls inside
            // the body instead of being remembered here.
            //
            // Storing the panel's width against a tab makes every tab of one
            // panel converge on that panel's width, which is a fixed point (the
            // request is the maximum over the tabs, and each of them now holds
            // it), not a ratchet. It does mean a tab dragged out of a wide panel
            // carries that width with it, which is correct: it is the last width
            // that tab was actually drawn at.
            //
            // The panel's extra caption rows are taken OUT of the stored height:
            // they depend on the panel's width and on which tabs it shows, not on
            // this tab, and only the active tab is ever re-measured. Kept in, an
            // inactive tab would hold the panel one row taller after it widened,
            // and a tab moved elsewhere would carry its old panel's rows along.
            // `plan_frame` adds the rows of whatever panel shows the tab.
            state.remember_measurement(
                layout_key,
                active,
                Vec2::new(
                    panel_plan.assumed_size.x,
                    (measured.y - drawn.header.extra_height).max(0.0),
                ),
            );
        }
    }

    // The prospective outer position of a window detached from a context menu:
    // the click is window-LOCAL, so it only becomes a monitor coordinate where
    // the window knows where it itself is.
    let menu_place_at = match (window_origin, pointer_pos) {
        (Some(origin), Some(pos)) => Some(origin + pos.to_vec2()),
        _ => None,
    };
    // A tab drag that has been pulled past the dock area's border tears out on
    // release, and nothing on screen would say so: the caption preview looks the
    // same wherever it is. The DRIVER announces it — the widget cannot, because
    // the area's border and this window's latch are the driver's knowledge — by
    // outlining the very rect `carry_dragged_tab` is painting under the cursor
    // (`grab_offset` gives its origin, `header_size` its extent). Suppressed
    // while the cursor is over another of OUR windows: that is a transfer, not a
    // tear-out, and the receiving window paints the insertion feedback instead.
    if carries_tab
        && owns_pointer
        && state.detach_allowed
        && window::drag_tension(area, pointer_pos, state.tab_drag_left_window).is_torn_off()
        && let Some(pointer) = pointer_pos
        && let Some(payload) = egui::DragAndDrop::payload::<DraggedTab>(ctx)
    {
        drag::paint_detach_preview(
            ctx,
            Rect::from_min_size(pointer - payload.grab_offset, payload.header_size),
        );
    }
    // The gesture came from ANOTHER window and the cursor is now over this one.
    // Its own widgets cannot show where the drop would land — a held button keeps
    // the pointer in the window the press started in, so nothing here is hovered
    // — so the driver paints it from the shared frame instead.
    if pointer_host == Some(host)
        && let Some(global) = gesture_global
        && let Some(window) = this_window
        && let Some(local) = window.to_local(global)
        && let Some(carried) = carried_gesture(ctx, state, layout_key, host)
    {
        paint_cross_window_feedback(ctx, area, &drop_targets, local, carried);
    }
    outcome.menu_move = menu_move.map(|(subject, target)| PendingMenuMove {
        subject,
        target,
        place_at: menu_place_at,
    });

    if let Some(layout) = state.layouts.get_mut(layout_key) {
        // Positions first: a mutation may pin a size, but neither changes
        // where the panels were drawn THIS frame.
        write_back_positions(layout, &solved, area.min);
        if !mutations.is_empty() {
            apply_mutations(layout, &mutations);
            state.dirty = true;
        }
        // The tab gestures are applied before the panel one: a tab released
        // on a header strip can EMPTY the panel a move gesture addresses,
        // and the model removes an emptied panel itself.
        let taken_by_a_strip = match tab_drop {
            Some((target, drop)) => apply_tab_drop(layout, target, drop.tab, drop.index),
            None => false,
        };
        if taken_by_a_strip {
            state.dirty = true;
            outcome.needs_repaint = true;
        } else if released
            && let Some(payload) = egui::DragAndDrop::payload::<DraggedTab>(ctx)
        {
            // The drop zones have already run: a release the header strips did
            // not take is either outside the dock area — where the tension rule
            // decides whether the tab tore out — or inside it, where landing on
            // bare area gives the tab a panel of its own (requirement 8).
            let bare_point =
                pointer_pos.and_then(|point| drag::empty_space_drop(area, &drawn_rects, point));
            // A window that does not own the pointer answers nothing at all: the
            // drop belongs to another of our windows (or to the bare desktop),
            // and only the frame's address resolution can say which.
            // Not allowed to detach (Embedded mode), a release past the border
            // is an ordinary drop: no bare area under it, so the move is
            // cancelled.
            let verdict = window::detach_verdict(
                state.detach_allowed,
                DragEndContext {
                    pointer_left_window: state.tab_drag_left_window,
                    release_pos: pointer_pos,
                    area,
                },
                owns_pointer,
            );
            match verdict {
                Some(trigger) => {
                    // The payload is deliberately NOT taken: another of our
                    // windows may still be about to claim this drop in the same
                    // pass. `end` takes it only if nobody did.
                    outcome.tab_drop = Some(PendingTabDrop {
                        tab: payload.tab,
                        grab_offset: payload.grab_offset,
                        source: host,
                        global: local_global,
                        trigger,
                    });
                }
                None => {
                    // Consumed either way: a release this window resolved must
                    // not travel on to the next one. Landing on bare dock area
                    // gives the tab a panel of its own, landing on anything else
                    // cancels the move.
                    let taken = egui::DragAndDrop::take_payload::<DraggedTab>(ctx);
                    if let Some(payload) = taken
                        && let Some(point) = bare_point
                    {
                        let pos = point - area.min.to_vec2() - payload.grab_offset;
                        match layout.detach_tab_to_host(payload.tab, host, pos) {
                            Ok(_) => {
                                state.dirty = true;
                                outcome.needs_repaint = true;
                            }
                            Err(error) => runtime_log::log_warn(format!(
                                "[panel_dock] cannot give the dropped tab `{}` a panel of its \
                                 own: {error}",
                                payload.tab
                            )),
                        }
                    }
                }
            }
        }
        match drag_phase {
            DragPhase::Idle => {}
            DragPhase::Moving {
                panel,
                tension,
                torn_origin,
            } => {
                if !owns_pointer {
                    // The cursor is over another of our windows; that window
                    // paints the feedback, and painting a tear-out outline here
                    // as well would promise a window that will not open.
                } else if tension.is_torn_off() && state.detach_allowed {
                    // The panel itself is pinned to the area's border, so the
                    // only thing that can say "let go now and this becomes a
                    // window" is an outline where the cursor actually is. No
                    // docking line is painted next to it: the two verdicts are
                    // mutually exclusive and showing both would be a lie.
                    if let Some(origin) = torn_origin
                        && let Some(geometry) = solved.get(panel)
                    {
                        drag::paint_detach_preview(
                            ctx,
                            Rect::from_min_size(origin, geometry.rect.size()),
                        );
                    }
                } else if let Some(candidate) =
                    snap_candidate(layout, panel, &solved, area)
                {
                    drag::paint_snap_preview(ctx, &candidate);
                }
            }
            DragPhase::Released {
                panel,
                grab_offset,
                detach,
            } => {
                match detach {
                    Some(trigger) => {
                        outcome.panel_drop = Some(PendingPanelDrop {
                            panel,
                            grab_offset,
                            source: host,
                            global: local_global,
                            trigger,
                        });
                    }
                    None => {
                        if apply_panel_drop(layout, panel, &solved, area) {
                            state.dirty = true;
                        }
                    }
                }
                outcome.needs_repaint = true;
            }
        }
    }
    if let Some(panel) = started_drag {
        begin_panel_drag(state, layout_key, host, ctx, panel, &solved, area);
    }
    // Last, so a reset cannot be undone by this frame's own gestures.
    if reset_requested && state.reset_layout(layout_key) {
        outcome.needs_repaint = true;
    }
    // A style change (or the very first drawn frame) invalidates the header
    // estimate the solve above used, so the layout has to be re-solved.
    if chrome.max_difference(state.chrome) >= MEASUREMENT_EPSILON {
        outcome.needs_repaint = true;
    }
    state.chrome = chrome;
    // After this frame's own mutations: a panel merged away or removed by them
    // must not leave measurements a reused id could pick up.
    state.prune_header_metrics(layout_key);
    // What this window offered a drop, for the frame's address resolution. The
    // record is written even when the gesture ended in this very pass: the window
    // the drop belongs to may not have drawn yet, and it is the whole frame — not
    // this pass — that decides.
    if gesture_in_flight {
        state.frame_hosts.push(HostRecord {
            host,
            area,
            panels: drop_targets,
            pointer_global: local_global,
        });
    }
    outcome
}
