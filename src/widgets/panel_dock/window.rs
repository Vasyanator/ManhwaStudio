/*
File: src/widgets/panel_dock/window.rs

Purpose:
Sub-windows of the dockable-panel system (plan §4.8 detach detection, §4.9): the
description of one detached OS window, the pure decisions that govern when one is
born and when it dies, and the egui plumbing (`ViewportId`, `ViewportBuilder`)
that turns such a description into a real window.

Main responsibilities:
- describe a sub-window (`SubWindowNode`) and address it from a `HostId`;
- measure how hard a drag is being pulled past the dock area's border
  (`drag_tension`) and decide, WITHOUT any global cursor coordinate, whether a
  finished drag tore out of the window (`detach_trigger`);
- decide which sub-windows are still needed (`obsolete_sub_windows`) and which
  index a new one takes (`next_sub_window_index`);
- name the windows (`sub_window_name`, `sub_window_title`, `move_target_label`)
  and list where something may be moved (`move_targets`);
- build the child viewport's id and builder, and state honestly what a platform
  without window placement (Wayland) can and cannot do.

Key structures:
- `SubWindowNode`: one detached window's identity and last known geometry.
- `DragTension`: how far a drag has been pulled past the dock area's border.
- `DetachTrigger`: which rule decided that a drag left the window.
- `DragEndContext`: everything a detach verdict is allowed to look at.
- `MoveTarget`: one destination of the «Переместить в окно →» submenu.

Key functions:
- `drag_tension`, `detach_trigger`
- `next_sub_window_index`, `obsolete_sub_windows`
- `move_targets`, `move_target_label`, `sub_window_name`, `sub_window_title`
- `sub_window_viewport_id`, `sub_window_builder`

Notes:
NO GLOBAL CURSOR POSITION EXISTS. egui exposes none at all, and on Wayland
`ViewportInfo::inner_rect` / `outer_rect` are always `None`
(`egui-0.35.0/src/data/input/viewport_info.rs:52-66`), so it cannot be derived
either. Every decision here is therefore taken from window-LOCAL facts: whether
the pointer left the window while a button was held (`PointerGone` clears
`latest_pos` but deliberately does not end the drag,
`egui-0.35.0/src/input_state/mod.rs:1200-1210`), and how far outside the dock
area the pointer is — the only "how far out" a window can measure at all.

IMMEDIATE, NOT DEFERRED. `Context::show_viewport_immediate` takes an `FnMut` with
no `Send`/`Sync`/`'static` bound (`egui-0.35.0/src/context.rs:4014`), which is the
only reason a sub-window can run the caller's tab bodies at all: the deferred form
(`context.rs:3960`) requires `Fn + Send + Sync + 'static` and would force an
`Arc<Mutex<…>>` around the typing tab's hundred-field states. The price is that
parent and child repaint together (`egui-0.35.0/src/viewport.rs:29-32`); that is
accepted and must not be "optimised" into the deferred form.
*/

use std::collections::{BTreeMap, BTreeSet};

use egui::{Pos2, Rect, Vec2};

use super::model::{DockLayout, HostId};

/// Literal salt of a sub-window's [`egui::ViewportId`].
///
/// A program literal, never a localised title: the viewport id must survive a
/// language switch (`egui-docs/05-ids-and-i18n.md` §5). Listed in
/// `dev-docs/i18n_exclusions.md`.
const SUB_WINDOW_VIEWPORT_SALT: &str = "ms_panel_subwindow";

/// Outer size, in points, a freshly detached sub-window opens at.
pub const DEFAULT_SUB_WINDOW_SIZE: Vec2 = Vec2::new(420.0, 560.0);

/// Smallest size, in points, a sub-window may be resized to. Below this the
/// panels inside it cannot draw their header strip.
pub const MIN_SUB_WINDOW_SIZE: Vec2 = Vec2::new(260.0, 200.0);

/// Distance, in points, the pointer must be pulled BEYOND the dock area's border
/// before the drag tears out into a window of its own.
///
/// This is the resistance the gesture offers at the edge: while the pointer is
/// inside the area everything behaves as usual (the panel follows it and snaps to
/// the nearest edge), and once it steps outside, the panel is held at the border
/// by the solver's clamp while the tension builds up. Only past this distance
/// does a release open a window.
///
/// Chosen as twice [`crate::widgets::panel_dock::drag::SNAP_DISTANCE`]: the pull
/// that abandons the area has to be unmistakably larger than the magnet that
/// docks to its edges. The two can never fight each other anyway — snapping is
/// measured INSIDE the area and the tension strictly OUTSIDE it — but a threshold
/// smaller than the magnet would still feel like the panel escaped by accident.
///
/// It is a distance the user can see: the panel stops at the border, so the
/// tension is exactly the gap that opens between the border and the cursor.
pub const DETACH_TENSION_DISTANCE: f32 = 48.0;

/// Coordinate/size magnitude beyond which a sub-window geometry is nonsense and
/// is discarded rather than handed to the window manager.
const MAX_WINDOW_COORD: f32 = 65_536.0;

/// Geometry change, in points, below which a sub-window's stored rect is left
/// alone. Without it every frame of a window drag would raise the dirty flag.
const GEOMETRY_EPSILON: f32 = 1.0;

/// One detached OS window hosting panels.
///
/// `index` is the identity: it is what [`HostId::SubWindow`] carries and what the
/// viewport id is derived from, and it is stable for the life of the window
/// (including across restarts, because it is persisted). `pos` and `size` are the
/// last geometry the window reported; `pos` stays `None` wherever the platform
/// refuses to report window positions (Wayland), and the window then opens
/// wherever the compositor puts it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SubWindowNode {
    /// Identity, matching `HostId::SubWindow(index)`.
    pub index: u32,
    /// Last known outer position in monitor space, in points. `None` when the
    /// platform does not expose it.
    pub pos: Option<Pos2>,
    /// Last known inner size, in points.
    pub size: Vec2,
}

impl SubWindowNode {
    /// Creates a node with sanitized geometry.
    ///
    /// A non-finite or absurd position is dropped (the window opens wherever the
    /// window manager decides), and a size that cannot describe a drawable window
    /// is raised to [`MIN_SUB_WINDOW_SIZE`].
    #[must_use]
    pub fn new(index: u32, pos: Option<Pos2>, size: Vec2) -> Self {
        Self {
            index,
            pos: pos.filter(|pos| sane_coord(pos.x) && sane_coord(pos.y)),
            size: Vec2::new(
                sane_extent(size.x, MIN_SUB_WINDOW_SIZE.x, DEFAULT_SUB_WINDOW_SIZE.x),
                sane_extent(size.y, MIN_SUB_WINDOW_SIZE.y, DEFAULT_SUB_WINDOW_SIZE.y),
            ),
        }
    }

    /// The host every panel drawn in this window carries.
    #[must_use]
    pub const fn host(&self) -> HostId {
        HostId::SubWindow(self.index)
    }
}

/// `true` when a coordinate is usable as a window position.
fn sane_coord(value: f32) -> bool {
    value.is_finite() && value.abs() <= MAX_WINDOW_COORD
}

/// Clamps one window extent, falling back to `fallback` for a value that cannot
/// describe a window at all.
fn sane_extent(value: f32, min: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 && value <= MAX_WINDOW_COORD {
        value.max(min)
    } else {
        fallback
    }
}

/// How hard a drag in flight is being pulled past the dock area's border.
///
/// The gesture RESISTS at the border: a panel is clamped into the dock area by
/// the solver, so once the pointer steps outside, the panel stays at the edge and
/// what grows is the gap between the two. That gap is the tension, and it is what
/// decides whether the release docks the panel or opens a window for it.
///
/// It is a pure function of where the pointer is RIGHT NOW ([`drag_tension`]) and
/// carries no latch: bringing the cursor back inside the area returns the gesture
/// to ordinary docking, however far it was pulled before. The one exception is
/// the `PointerGone` case, which the caller latches for its own reasons (a brush
/// past the border must not end the gesture) and hands in as
/// `pointer_left_window`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum DragTension {
    /// The pointer is inside the dock area. Everything behaves as usual: the
    /// panel follows the cursor and snaps to the nearest edge.
    Inside,
    /// The pointer is outside the area but the pull is still resisted. Carries
    /// how far outside it is, in points — always `> 0.0` and
    /// `<= DETACH_TENSION_DISTANCE`.
    Resisting { pull: f32 },
    /// The pull passed [`DETACH_TENSION_DISTANCE`] (or the pointer left the
    /// window altogether): a release now moves what is being dragged into a
    /// window of its own.
    TornOff,
}

impl DragTension {
    /// `true` once the gesture has torn out of the window and a release would
    /// open a window of its own.
    #[must_use]
    pub const fn is_torn_off(self) -> bool {
        matches!(self, Self::TornOff)
    }
}

/// How far a drag has been pulled past `area`'s border, from window-LOCAL facts
/// only.
///
/// `pointer` is the pointer position in this window's screen coordinates;
/// `pointer_left_window` is the caller's latch for "the pointer is outside this
/// window with the button still held" (`PointerGone`). Both a missing position
/// and that latch mean the cursor is somewhere this window cannot name, which is
/// by definition further out than any threshold measured inside it — the answer
/// is [`DragTension::TornOff`].
///
/// A non-finite coordinate (or a degenerate area, for which
/// `Rect::distance_to_pos` reports infinity) is reported as
/// [`DragTension::Inside`]: garbage geometry must not open windows behind the
/// user's back.
#[must_use]
pub fn drag_tension(area: Rect, pointer: Option<Pos2>, pointer_left_window: bool) -> DragTension {
    if pointer_left_window {
        return DragTension::TornOff;
    }
    let Some(pos) = pointer else {
        return DragTension::TornOff;
    };
    // Zero inside the rect, and the euclidean distance to the nearest point of
    // it outside — including diagonally past a corner
    // (`emath-0.35.0/src/rect.rs:391-425`).
    let pull = area.distance_to_pos(pos);
    if !pull.is_finite() {
        return DragTension::Inside;
    }
    if pull <= 0.0 {
        DragTension::Inside
    } else if pull <= DETACH_TENSION_DISTANCE {
        DragTension::Resisting { pull }
    } else {
        DragTension::TornOff
    }
}

/// Which rule decided that a finished drag left the window.
///
/// Reported rather than collapsed into a boolean because the rules have very
/// different reliability, and a log line that names the one that fired is what
/// makes a platform-specific report from a user actionable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DetachTrigger {
    /// The pointer left the window while the button was still held, or was not
    /// reportable at all when the gesture ended — the primary,
    /// platform-independent signal (`PointerGone`).
    PointerLeftWindow,
    /// The gesture was pulled further than [`DETACH_TENSION_DISTANCE`] past the
    /// dock area's border and released there. This is the rule that works on
    /// every platform, including one where the pointer never stops being
    /// reported (an implicit pointer grab keeps delivering coordinates outside
    /// the window, and those are simply very far outside the area).
    PulledPastArea,
    /// The user asked for it explicitly in the tab's context menu. Always
    /// available, and the only path that needs no pointer information at all.
    ContextMenu,
}

/// Everything a detach verdict may look at.
///
/// Deliberately window-LOCAL: no global cursor position exists on any platform,
/// and none is needed.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DragEndContext {
    /// `true` when this window observed the pointer leave while a button was
    /// down, and it never came back before the gesture ended.
    pub pointer_left_window: bool,
    /// Where the release landed, in this window's screen coordinates. `None` when
    /// the pointer is gone, which is itself a detach signal.
    pub release_pos: Option<Pos2>,
    /// The dock area inside this window: the border the tension is measured
    /// against.
    pub area: Rect,
}

/// Whether a finished drag — of a tab OR of a whole panel — detaches into a
/// sub-window, and why.
///
/// ONE rule for both gestures, which is the point: the user pulls the thing being
/// dragged past the dock area's border, feels it resist (the panel is held at the
/// edge), and past [`DETACH_TENSION_DISTANCE`] it tears off. A release short of
/// that distance is an ordinary drop, whatever it landed on.
///
/// The verdict is deliberately taken at the END of the gesture and never while it
/// is in flight: a release inside another of OUR windows must stay an ordinary
/// cross-window transfer, and only the frame as a whole knows whether any window
/// claimed the drop (`mod.rs::apply_frame_detaches`).
///
/// A detach rule may never be measured INSIDE the dock area, however tempting a
/// "released close to the border" fallback is: edge docking lives there, a panel
/// snaps to the area's sides from up to `drag::SNAP_DISTANCE` away, and such a
/// rule would make docking to that border impossible — so it could only ever be
/// given to the tab gesture, and the two gestures would then answer differently
/// at the same place on screen. Measuring strictly OUTSIDE the area is what lets
/// both share one rule.
#[must_use]
pub fn detach_trigger(context: DragEndContext) -> Option<DetachTrigger> {
    if context.pointer_left_window || context.release_pos.is_none() {
        return Some(DetachTrigger::PointerLeftWindow);
    }
    match drag_tension(context.area, context.release_pos, context.pointer_left_window) {
        DragTension::TornOff => Some(DetachTrigger::PulledPastArea),
        DragTension::Inside | DragTension::Resisting { .. } => None,
    }
}

/// One destination offered by the «Переместить в окно →» submenu of a tab
/// caption or a panel header.
///
/// The menu is the platform-independent half of the cross-window move: unlike
/// the drag gestures it needs no pointer, no window position and no monitor
/// coordinates at all. On Wayland — where `ViewportInfo::inner_rect` is always
/// `None` and a held button keeps the pointer in the window the press started in
/// — it is therefore the ONLY way to move a tab or a panel between windows
/// (plan §4.8).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MoveTarget {
    /// A window that already exists.
    Existing(HostId),
    /// A brand-new sub-window, opened for whatever is being moved.
    NewWindow,
}

/// Destinations to offer for something that currently lives in `current`, in
/// menu order: the main window, then every sub-window by ascending index, then
/// [`MoveTarget::NewWindow`].
///
/// `current` is excluded — moving something into the window it is already in
/// would do nothing — while a new window is always offered, because opening one
/// is always possible. `sub_windows` may be in any order; the result is sorted
/// so the menu does not reshuffle when the dock's own list does.
#[must_use]
pub fn move_targets(sub_windows: &[SubWindowNode], current: HostId) -> Vec<MoveTarget> {
    let mut targets: Vec<MoveTarget> = Vec::with_capacity(sub_windows.len() + 2);
    if current != HostId::MainWindow {
        targets.push(MoveTarget::Existing(HostId::MainWindow));
    }
    let indices: BTreeSet<u32> = sub_windows.iter().map(|node| node.index).collect();
    targets.extend(
        indices
            .into_iter()
            .map(HostId::SubWindow)
            .filter(|host| *host != current)
            .map(MoveTarget::Existing),
    );
    targets.push(MoveTarget::NewWindow);
    targets
}

/// Human-readable name of one sub-window, shown BOTH in the OS title bar
/// ([`sub_window_title`]) and in the «Переместить в окно →» submenu, so the
/// window the user picks in the menu is the one they can see.
///
/// Numbered from `index + 1`, so the first window opened is «Окно 1». The number
/// is the window's persisted IDENTITY and not its position in a list: closing
/// «Окно 1» leaves «Окно 2» named «Окно 2», and the next window opened reuses the
/// freed number ([`next_sub_window_index`]).
#[must_use]
pub fn sub_window_name(index: u32) -> String {
    tf!(
        "widgets.panel_dock.sub_window_name",
        number = index.saturating_add(1)
    )
}

/// Title of one sub-window's OS title bar: its [`sub_window_name`] plus the
/// program's name.
///
/// The template MUST keep its `{name}` placeholder — that is the whole of the
/// window's number, and a catalog value without it drops the number silently,
/// leaving every detached window with the same title. The shipped catalogs are
/// pinned by a test here; an on-disk `locale/<tag>.json` that predates a change
/// to this key keeps its own value forever, because the disk layer only
/// backfills keys it LACKS (`src/locale_store.rs`, reconcile contract).
///
/// Rebuilt from `index` on every frame the window is shown
/// (`mod.rs::show_sub_windows`), so it follows a language switch and any change
/// of the window's identity without a separate invalidation path: eframe diffs
/// the `ViewportBuilder` and emits `ViewportCommand::Title` for a title that
/// differs from the one the window already carries
/// (`eframe-0.35.0/src/native/glow_integration.rs:1454`,
/// `egui-0.35.0/src/viewport.rs:752`).
#[must_use]
pub fn sub_window_title(index: u32) -> String {
    tf!(
        "widgets.panel_dock.sub_window_title",
        name = sub_window_name(index)
    )
}

/// Localised label of one move destination, as the submenu shows it.
#[must_use]
pub fn move_target_label(target: MoveTarget) -> String {
    match target {
        MoveTarget::Existing(HostId::MainWindow) => {
            t!("widgets.panel_dock.main_window_name").to_owned()
        }
        MoveTarget::Existing(HostId::SubWindow(index)) => sub_window_name(index),
        MoveTarget::NewWindow => t!("widgets.panel_dock.new_window").to_owned(),
    }
}

/// The index a new sub-window takes: the lowest one no live window uses.
///
/// Reusing a hole keeps the indices — and with them the viewport ids and the
/// persisted host keys — small and stable. `None` only when `u32::MAX` windows
/// exist, which is reported instead of wrapping onto a live index.
#[must_use]
pub fn next_sub_window_index(existing: &[SubWindowNode]) -> Option<u32> {
    let taken: BTreeSet<u32> = existing.iter().map(|node| node.index).collect();
    (0..=u32::MAX).find(|candidate| !taken.contains(candidate))
}

/// Sub-windows that hold no panel in ANY program tab's layout, ascending.
///
/// Requirement 10, scoped exactly as the plan scopes it (§4.4): a sub-window is
/// closed when it is EMPTY EVERYWHERE, not when the program tab currently drawn
/// happens to put nothing in it — that case is requirement 11 and keeps the
/// window open and grey.
#[must_use]
pub fn obsolete_sub_windows(
    nodes: &[SubWindowNode],
    layouts: &BTreeMap<String, DockLayout>,
) -> Vec<u32> {
    let used: BTreeSet<u32> = layouts
        .values()
        .flat_map(DockLayout::sub_window_indices)
        .collect();
    nodes
        .iter()
        .map(|node| node.index)
        .filter(|index| !used.contains(index))
        .collect()
}

/// `true` when a geometry report differs enough from the stored one to be worth
/// persisting. Keeps a window drag from raising the dirty flag every frame.
#[must_use]
pub fn geometry_changed(stored: &SubWindowNode, pos: Option<Pos2>, size: Vec2) -> bool {
    let moved = match (stored.pos, pos) {
        (Some(old), Some(new)) => {
            (old.x - new.x).abs() >= GEOMETRY_EPSILON || (old.y - new.y).abs() >= GEOMETRY_EPSILON
        }
        (None, Some(_)) => true,
        // A report that lost the position (minimised, or a platform that stopped
        // answering) must not erase the one we already know.
        (_, None) => false,
    };
    let resized = (stored.size.x - size.x).abs() >= GEOMETRY_EPSILON
        || (stored.size.y - size.y).abs() >= GEOMETRY_EPSILON;
    moved || resized
}

/// Stable viewport id of one sub-window.
///
/// Derived from a literal salt plus the window's index — never from the localised
/// window title, which would give the window a new identity on every language
/// switch and orphan the OS window that already exists.
#[must_use]
pub fn sub_window_viewport_id(index: u32) -> egui::ViewportId {
    egui::ViewportId::from_hash_of((SUB_WINDOW_VIEWPORT_SALT, index))
}

/// Builds the child viewport of one sub-window.
///
/// `title` is already localised; only the id above is a literal. `position` is
/// applied when the platform reports window positions at all — on Wayland
/// `ViewportBuilder::with_position` is ignored and `ViewportInfo::outer_rect` is
/// always `None` (`egui-0.35.0/src/viewport.rs:610`,
/// `data/input/viewport_info.rs:52-66`), so the caller passes `None` there and
/// logs the reason instead of pretending the window landed where it asked.
#[must_use]
pub fn sub_window_builder(
    node: &SubWindowNode,
    title: &str,
    position: Option<Pos2>,
) -> egui::ViewportBuilder {
    let mut builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(node.size)
        .with_min_inner_size(MIN_SUB_WINDOW_SIZE)
        .with_resizable(true)
        .with_close_button(true)
        .with_minimize_button(true)
        .with_maximize_button(true);
    if let Some(position) = position.filter(|pos| sane_coord(pos.x) && sane_coord(pos.y)) {
        builder = builder.with_position(position);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::widgets::panel_dock::model::{PanelId, PanelNode, TabId};

    const TAB_A: TabId = TabId::new("test.a");
    const TAB_B: TabId = TabId::new("test.b");

    const AREA: Rect = Rect::from_min_max(Pos2::new(0.0, 60.0), Pos2::new(1000.0, 800.0));

    fn context(left: bool, release: Option<Pos2>) -> DragEndContext {
        DragEndContext {
            pointer_left_window: left,
            release_pos: release,
            area: AREA,
        }
    }

    /// Both gestures ask the SAME question, so every case below is stated once
    /// and asserted for the tab call site and the panel call site together.
    /// The two used to differ (the tab had a border rule the panel could not
    /// have), which is exactly the inconsistency the tension model removes.
    fn verdict(context: DragEndContext) -> Option<DetachTrigger> {
        detach_trigger(context)
    }

    #[test]
    fn a_pointer_inside_the_area_is_under_no_tension_at_all() {
        assert_eq!(
            drag_tension(AREA, Some(Pos2::new(500.0, 400.0)), false),
            DragTension::Inside
        );
        // One point inside the border — where edge docking happens — is still
        // fully inside: the tension must never compete with the snap.
        assert_eq!(
            drag_tension(AREA, Some(Pos2::new(1.0, 400.0)), false),
            DragTension::Inside
        );
        assert_eq!(verdict(context(false, Some(Pos2::new(500.0, 400.0)))), None);
    }

    #[test]
    fn a_pull_short_of_the_threshold_is_resisted_and_does_not_detach() {
        // Ten points to the left of the area: outside, but nowhere near enough.
        let pulled = Pos2::new(AREA.left() - 10.0, 400.0);
        match drag_tension(AREA, Some(pulled), false) {
            DragTension::Resisting { pull } => assert!((pull - 10.0).abs() < 0.01),
            other => panic!("expected a resisted pull, got {other:?}"),
        }
        assert_eq!(verdict(context(false, Some(pulled))), None);

        // Exactly at the threshold still holds: the panel escapes only PAST it.
        let at_threshold = Pos2::new(AREA.left() - DETACH_TENSION_DISTANCE, 400.0);
        match drag_tension(AREA, Some(at_threshold), false) {
            DragTension::Resisting { pull } => {
                assert!((pull - DETACH_TENSION_DISTANCE).abs() < 0.01);
            }
            other => panic!("expected the threshold itself to still resist, got {other:?}"),
        }
        assert_eq!(verdict(context(false, Some(at_threshold))), None);
    }

    #[test]
    fn a_pull_past_the_threshold_tears_off() {
        let torn = Pos2::new(AREA.left() - DETACH_TENSION_DISTANCE - 1.0, 400.0);
        assert_eq!(drag_tension(AREA, Some(torn), false), DragTension::TornOff);
        assert_eq!(
            verdict(context(false, Some(torn))),
            Some(DetachTrigger::PulledPastArea)
        );

        // Above the area — the program's own toolbar — is the same rule; the
        // area's border is what counts, not the window's.
        let above = Pos2::new(500.0, AREA.top() - DETACH_TENSION_DISTANCE - 1.0);
        assert_eq!(drag_tension(AREA, Some(above), false), DragTension::TornOff);
    }

    #[test]
    fn the_pull_is_measured_diagonally_past_a_corner() {
        // 40 pt left and 40 pt above the top-left corner: 56.6 pt away, which is
        // past the threshold even though neither axis is on its own.
        let corner = Pos2::new(AREA.left() - 40.0, AREA.top() - 40.0);
        assert_eq!(drag_tension(AREA, Some(corner), false), DragTension::TornOff);
        // 30 pt on each axis is 42.4 pt: still resisted.
        let closer = Pos2::new(AREA.left() - 30.0, AREA.top() - 30.0);
        match drag_tension(AREA, Some(closer), false) {
            DragTension::Resisting { pull } => assert!(pull > 42.0 && pull < 43.0),
            other => panic!("expected a resisted diagonal pull, got {other:?}"),
        }
    }

    #[test]
    fn coming_back_inside_releases_a_partial_tension() {
        // Pulled out, then brought back: the tension carries no latch, so the
        // gesture is an ordinary docking one again.
        let pulled = Pos2::new(AREA.left() - 30.0, 400.0);
        assert!(matches!(
            drag_tension(AREA, Some(pulled), false),
            DragTension::Resisting { .. }
        ));
        let back = Pos2::new(AREA.left() + 5.0, 400.0);
        assert_eq!(drag_tension(AREA, Some(back), false), DragTension::Inside);
        assert_eq!(verdict(context(false, Some(back))), None);
    }

    #[test]
    fn a_pointer_that_left_the_window_is_torn_off_whatever_the_distance_says() {
        // The latch wins over the coordinate: `PointerGone` means the cursor is
        // somewhere this window cannot measure.
        assert_eq!(
            drag_tension(AREA, Some(Pos2::new(500.0, 400.0)), true),
            DragTension::TornOff
        );
        assert_eq!(drag_tension(AREA, None, false), DragTension::TornOff);
        assert_eq!(
            verdict(context(true, Some(Pos2::new(500.0, 400.0)))),
            Some(DetachTrigger::PointerLeftWindow)
        );
        assert_eq!(
            verdict(context(false, None)),
            Some(DetachTrigger::PointerLeftWindow)
        );
    }

    #[test]
    fn a_release_just_outside_the_window_docks_instead_of_detaching() {
        // The old rule detached on ANY release outside the viewport rect. Under
        // the tension model a cursor that merely slipped past the border is
        // still resisted — which is the whole point of the resistance.
        let just_outside = Pos2::new(AREA.left() - 4.0, 400.0);
        assert_eq!(verdict(context(false, Some(just_outside))), None);
    }

    #[test]
    fn garbage_geometry_never_opens_a_window() {
        assert_eq!(
            drag_tension(AREA, Some(Pos2::new(f32::NAN, 400.0)), false),
            DragTension::Inside
        );
        let degenerate = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(0.0, 0.0));
        assert_eq!(
            drag_tension(degenerate, Some(Pos2::new(500.0, 400.0)), false),
            DragTension::Inside
        );
    }

    #[test]
    fn the_menu_offers_every_window_except_the_current_one() {
        let nodes = [
            SubWindowNode::new(0, None, DEFAULT_SUB_WINDOW_SIZE),
            SubWindowNode::new(2, None, DEFAULT_SUB_WINDOW_SIZE),
        ];
        // From the main window: no «main window» entry, both sub-windows, and a
        // new one.
        assert_eq!(
            move_targets(&nodes, HostId::MainWindow),
            vec![
                MoveTarget::Existing(HostId::SubWindow(0)),
                MoveTarget::Existing(HostId::SubWindow(2)),
                MoveTarget::NewWindow,
            ]
        );
        // From sub-window 0: the main window comes first and 0 itself is gone.
        assert_eq!(
            move_targets(&nodes, HostId::SubWindow(0)),
            vec![
                MoveTarget::Existing(HostId::MainWindow),
                MoveTarget::Existing(HostId::SubWindow(2)),
                MoveTarget::NewWindow,
            ]
        );
    }

    #[test]
    fn the_menu_offers_a_new_window_even_when_none_exists() {
        assert_eq!(
            move_targets(&[], HostId::MainWindow),
            vec![MoveTarget::NewWindow]
        );
    }

    #[test]
    fn the_menu_order_does_not_follow_the_dock_s_own_list_order() {
        // The dock keeps its windows in creation order and reuses freed indices,
        // so the list can be unsorted; the menu must not reshuffle with it.
        let nodes = [
            SubWindowNode::new(3, None, DEFAULT_SUB_WINDOW_SIZE),
            SubWindowNode::new(1, None, DEFAULT_SUB_WINDOW_SIZE),
        ];
        assert_eq!(
            move_targets(&nodes, HostId::MainWindow),
            vec![
                MoveTarget::Existing(HostId::SubWindow(1)),
                MoveTarget::Existing(HostId::SubWindow(3)),
                MoveTarget::NewWindow,
            ]
        );
    }

    #[test]
    fn indices_fill_the_lowest_hole() {
        let nodes = [
            SubWindowNode::new(0, None, DEFAULT_SUB_WINDOW_SIZE),
            SubWindowNode::new(2, None, DEFAULT_SUB_WINDOW_SIZE),
        ];
        assert_eq!(next_sub_window_index(&nodes), Some(1));
        assert_eq!(next_sub_window_index(&[]), Some(0));
    }

    #[test]
    fn geometry_is_sanitized_on_construction() {
        let node = SubWindowNode::new(0, Some(Pos2::new(f32::NAN, 10.0)), Vec2::new(-5.0, 10.0));
        assert_eq!(node.pos, None);
        assert_eq!(node.size.x, DEFAULT_SUB_WINDOW_SIZE.x);
        assert_eq!(node.size.y, MIN_SUB_WINDOW_SIZE.y);
    }

    #[test]
    fn only_a_window_empty_in_every_layout_is_obsolete() {
        let mut typing = DockLayout::new();
        typing
            .insert_panel(
                PanelNode::new(PanelId::new(0), HostId::SubWindow(0), vec![TAB_A])
                    .expect("panel in sub-window 0"),
            )
            .expect("insert");
        // The cleaning layout puts nothing in either window: requirement 11 says
        // that keeps them open, and that is exactly what this must not report.
        let cleaning = DockLayout::new();
        let mut layouts = BTreeMap::new();
        layouts.insert("typing".to_owned(), typing);
        layouts.insert("cleaning".to_owned(), cleaning);

        let nodes = [
            SubWindowNode::new(0, None, DEFAULT_SUB_WINDOW_SIZE),
            SubWindowNode::new(1, None, DEFAULT_SUB_WINDOW_SIZE),
        ];
        assert_eq!(obsolete_sub_windows(&nodes, &layouts), vec![1]);
    }

    #[test]
    fn a_window_used_by_another_program_tab_survives() {
        let mut cleaning = DockLayout::new();
        cleaning
            .insert_panel(
                PanelNode::new(PanelId::new(0), HostId::SubWindow(3), vec![TAB_B])
                    .expect("panel in sub-window 3"),
            )
            .expect("insert");
        let mut layouts = BTreeMap::new();
        layouts.insert("typing".to_owned(), DockLayout::new());
        layouts.insert("cleaning".to_owned(), cleaning);
        let nodes = [SubWindowNode::new(3, None, DEFAULT_SUB_WINDOW_SIZE)];
        assert!(obsolete_sub_windows(&nodes, &layouts).is_empty());
    }

    #[test]
    fn geometry_changes_are_reported_only_past_the_epsilon() {
        let node = SubWindowNode::new(0, Some(Pos2::new(100.0, 100.0)), Vec2::new(400.0, 500.0));
        assert!(!geometry_changed(
            &node,
            Some(Pos2::new(100.4, 100.4)),
            Vec2::new(400.2, 500.2)
        ));
        assert!(geometry_changed(
            &node,
            Some(Pos2::new(140.0, 100.0)),
            Vec2::new(400.0, 500.0)
        ));
        assert!(geometry_changed(
            &node,
            Some(Pos2::new(100.0, 100.0)),
            Vec2::new(400.0, 620.0)
        ));
        // A lost position must not erase the one we have.
        assert!(!geometry_changed(&node, None, Vec2::new(400.0, 500.0)));
    }

    #[test]
    fn the_menu_entry_and_the_os_title_bar_carry_the_same_number() {
        // The user picks «Окно 2» in the submenu and must find «Окно 2» on the
        // title bar: both are built from `sub_window_name`, and the title only
        // carries the number as long as its template keeps `{name}`.
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        for index in [0_u32, 1, 7] {
            let name = sub_window_name(index);
            let number = index.saturating_add(1).to_string();
            assert!(
                name.contains(&number),
                "the window name must show its number: {name}"
            );
            assert_eq!(
                move_target_label(MoveTarget::Existing(HostId::SubWindow(index))),
                name,
                "the submenu entry is the window's name and nothing else"
            );
            let title = sub_window_title(index);
            assert!(
                title.contains(&name),
                "the OS title bar must contain the very name the menu offers: {title}"
            );
        }
        // A different index is a different window, and says so on its title bar.
        assert_ne!(sub_window_title(0), sub_window_title(1));
    }

    #[test]
    fn every_shipped_catalog_keeps_the_placeholders_the_window_name_is_built_from() {
        // A translation that drops `{number}` or `{name}` compiles, passes the
        // key-existence test, and silently leaves every detached window with the
        // same title — which is exactly how the number went missing once.
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: serde_json::Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let entry = |key: &str| -> String {
                catalog
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"))
                    .to_owned()
            };
            let name = entry("widgets.panel_dock.sub_window_name");
            assert!(
                name.contains("{number}"),
                "locale `{tag}`: the window name carries the number, `{name}` does not"
            );
            let title = entry("widgets.panel_dock.sub_window_title");
            assert!(
                title.contains("{name}"),
                "locale `{tag}`: the title bar is built from the window name, `{title}` is not"
            );
        }
    }

    #[test]
    fn the_viewport_id_depends_only_on_the_index() {
        assert_eq!(sub_window_viewport_id(2), sub_window_viewport_id(2));
        assert_ne!(sub_window_viewport_id(2), sub_window_viewport_id(3));
    }
}
