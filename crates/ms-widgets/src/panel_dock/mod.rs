/*
File: crates/ms-widgets/src/panel_dock/mod.rs

Purpose:
Public surface of the dockable-panel system, plus the per-frame driver that ties
the pure layer (model + solver) to the two widgets: `PanelDock` and its caller-
owned state `PanelDockState`.

Main responsibilities:
- own the submodule tree of the dock and re-export what other modules may use;
- collect the tabs a caller declares for the current frame;
- resolve the layout with sizes measured LAST frame, draw every drawable panel in
  panel order, and write back what the user changed.

Key structures:
- `PanelDockState`: per-program-tab layouts, last-frame measurements (content
  and pinned-bottom heights per tab, header rows and caption width per panel),
  per-tab extra state (`extras.rs`), dirty flag.
- `DockArea`: where the dock may place panels this frame.
- `PanelDock`: the frame driver (`begin` → `tab(..).show(..)` → `end`).
- `PanelDockOutput`: rects of the panels drawn this frame, each recorded with
  the window it was drawn in and handed out for the MAIN window only.

Key functions:
- `PanelDock::begin` / `PanelDock::tab` / `PanelDock::end`.
- `PanelDockState::ensure_default_layout`.
- `PanelDockState::set_tab_extras_flag` / `PanelDockState::focus_tab`: host-side
  (outside any body) extras write and "bring this tab to the front".

Frame-step children (private, `pub(super)` items only):
- `host_draw.rs`: `draw_host`, one window's pass, and the per-window records.
- `frame_plan.rs`: `plan_frame` / `tab_request` / `frame_layout` (pure).
- `seeding.rs`: `ensure_declared_tabs` and the default-placement rules (pure).
- `panel_drag.rs`: panel-move drag, tab drop, `write_back_positions`.
- `drop_apply.rs`: addressed cross-window drops, detaches, `apply_mutations`.
- `menu_move.rs`: the «Переместить в окно →» move and its landing rules.
- `sub_windows.rs`: live window geometry, sub-window passes, cross-window
  feedback and `apply_frame_detaches`.
- `tests.rs`: the GPU-free unit tests of all of the above.

Notes:
The driver is deliberately deferred: `tab(..).show(closure)` queues the closure
and `end(cx)` runs it. Bodies do not capture the caller's state — they receive
`&mut C`, a per-frame context the caller assembles once and `end` hands to one
body at a time. That is what lets several tabs of one frame touch the SAME heavy
state (the typing tab's params, effects, actions and preview bodies all need
`TypingTopPanelState`) while others touch a different one, without cloning or
interior mutability. The corollary is a hard constraint: `C` may NOT contain the
same `PanelDockState` that `begin` borrows — keep the dock state in its own
field, disjoint from whatever the bodies touch.

The frame is also where a gesture that crossed a WINDOW border is resolved. Every
window records what it drew (`HostRecord`) while something is being dragged, and
`apply_frame_detaches` — running after the last window's pass — asks
`cross_window.rs` which window the release landed in. That order is the whole
point: the window a drop belongs to may have drawn before the window the gesture
started in, and it can never hit-test the drop itself, because a held button keeps
the pointer in the window the press started in.

Sub-windows are immediate viewports while a program tab of the dock draws (they
run its tab bodies) and DEFERRED viewports while none does
(`PanelDockState::show_idle_sub_windows`): an idle window then repaints only on
its own events instead of with every main-window frame, and hands its close
button and geometry over through `window::IdleWindowReport`.
*/

pub mod cross_window;
pub mod drag;
pub mod elastic;
pub mod extras;
pub mod model;
pub mod panel;
pub mod persist;
pub mod solver;
pub mod tab;
pub mod window;

mod drop_apply;
mod frame_plan;
mod host_draw;
mod menu_move;
mod panel_drag;
mod seeding;
mod sub_windows;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use egui::{Pos2, Rect, Vec2};

use ms_config::secondary_windows::WindowMode;
use ms_log::runtime_log;

use host_draw::{FrameContext, HostFrame, HostRecord, draw_host};
use seeding::ensure_declared_tabs;
use sub_windows::{
    apply_frame_detaches, idle_sub_window_pass, observe_sub_window_geometry, show_sub_windows,
    take_idle_window_reports,
};

pub use cross_window::{DropAddress, PanelDropTarget, TabLanding, WindowGeometry};
pub use drag::{DragSession, DraggedTab, SNAP_DISTANCE, SnapCandidate, SnapTargets};
pub use extras::TabExtras;
pub use model::{
    DetachTabOutcome, DockEdge, DockLayout, DockModelError, HostId, MoveTabOutcome, PanelAnchor,
    PanelId, PanelNode, TabId,
};
pub use panel::{
    CollapsiblePanel, CollapsiblePanelOutput, MoveTargetEntry, PanelSection, PanelTabHeader,
    TabDrop,
};
pub use persist::{
    PANEL_LAYOUT_SECTION_KEY, PANEL_LAYOUT_SECTION_VERSION, PanelLayoutError, PanelLayoutSnapshot,
    PanelLayoutWriter, layouts_from_user_settings,
};
pub use solver::{
    COLLAPSED_PANEL_HEIGHT, DEFAULT_PANEL_SIZE, DOCK_GAP, PANEL_MIN_BODY_HEIGHT,
    PANEL_MIN_CONTENT_HEIGHT, PANEL_MIN_WIDTH, PanelChrome, PanelHeaderMetrics, PanelHeaders,
    PanelSizes, SolvedLayout, SolvedPanel, solve, solve_with_headers,
};
pub use tab::PanelTab;
pub use window::{
    DETACH_TENSION_DISTANCE, DetachTrigger, DragEndContext, DragTension, MoveTarget, SubWindowNode,
    detach_trigger, drag_tension, move_targets,
};

/// Offset, in points, between two panels the dock creates on its own for tabs
/// that no layout knew about. Cascading keeps a freshly declared tab from
/// landing exactly on top of the previous one.
const AUTO_PANEL_CASCADE_STEP: f32 = 24.0;

/// Difference between two anchor fractions (`PanelAnchor::Panel::align`,
/// `PanelAnchor::ViewportEdge::along`) below which the two anchors name the SAME
/// slot. A fraction spans the whole free travel along a side, so a thousandth of
/// one is far below a point on any dock area a window can have.
const SLOT_FRACTION_EPSILON: f32 = 1e-3;

/// Difference, in points, between the size the drawn tab CONTRIBUTED to its
/// panel's request and the size its content turned out to need, above which the
/// dock asks for another frame. One point: below that the layout is visually
/// settled and repainting forever would burn CPU on rounding noise.
const MEASUREMENT_EPSILON: f32 = 1.0;

/// Per-frame declaration data of one tab, without its closures.
#[derive(Copy, Clone, Debug, PartialEq)]
struct TabMeta {
    /// Whether the tab is drawn this frame. A hidden tab keeps its layout slot.
    visible: bool,
    /// Whether the panel showing this tab hides its chrome until the pointer is
    /// over it. A per-frame declaration like [`TabMeta::visible`] — nothing about
    /// it is stored, because it describes what the CALLER wants this frame, not
    /// how the user arranged the dock.
    transparent_until_hover: bool,
    /// Lower bound on the owning panel's outer size while this tab is active.
    min_size: Option<Vec2>,
    /// Outer size used until the tab has been measured at least once.
    initial_size: Option<Vec2>,
}

impl Default for TabMeta {
    fn default() -> Self {
        Self {
            visible: true,
            transparent_until_hover: false,
            min_size: None,
            initial_size: None,
        }
    }
}

/// Whether a pinned bottom section's drawn `height` differs from the height
/// the scroll viewport was sized for (`previous`, `None` on its first frame) by
/// enough to need another frame. A first measurement is always news.
fn pinned_height_changed(previous: Option<f32>, height: f32) -> bool {
    previous.is_none_or(|previous| (previous - height).abs() >= MEASUREMENT_EPSILON)
}

/// Whether a panel's drawn header `measured` differs from the `previous` one the
/// frame was solved with by enough to need another frame. A panel never drawn
/// was solved as a one-row header with no width floor (`None` reads as the
/// default). Any panel showing a caption reports a `min_width` above zero, so
/// its FIRST draw per layout key is always news and costs one extra frame, as
/// does a caption whose text changed (a language switch) — even when the floor
/// lies far below the panel's width and the re-solve moves nothing. That is the
/// price of comparing the measurement rather than its effect on the solve.
fn header_metrics_changed(previous: Option<PanelHeaderMetrics>, measured: PanelHeaderMetrics) -> bool {
    previous.unwrap_or_default().max_difference(measured) >= MEASUREMENT_EPSILON
}

/// One panel's header measurement as the driver caches it: the metrics plus the
/// captions they were measured WITH.
///
/// The tabs are part of the entry because a `PanelId` is not a stable identity:
/// `DockLayout` mints `max + 1`, so a panel created after the highest one
/// vanished reuses its id, and a panel's shown tabs change when one is hidden
/// or moved. Metrics are only ever applied to a panel showing exactly the tabs
/// they were measured for ([`plan_frame`]); anything else is solved as a fresh
/// one-row header until it is drawn.
#[derive(Clone, Debug, PartialEq)]
struct MeasuredHeader {
    /// The panel's visible tabs, in strip order, when the header was drawn.
    tabs: Vec<TabId>,
    /// What the header measured beyond the one-row chrome.
    metrics: PanelHeaderMetrics,
}

/// Localised caption producer of one declared tab, evaluated once per frame and
/// only for a tab that is actually drawn.
pub(super) type TabTitle<'frame> = Box<dyn Fn() -> String + 'frame>;

/// Body of one declared tab, queued by `PanelTab::show` and run by
/// `PanelDock::end`.
///
/// `C` is the caller's per-frame context: the body borrows nothing of its own
/// and reaches the caller's state exclusively through the `&mut C` that `end`
/// hands to one body at a time.
///
/// The third parameter is the tab's own [`TabExtras`] — the persisted per-tab
/// state the dock keeps for it. Every body takes it, including the ones declared
/// through [`PanelTab::show`](tab::PanelTab::show), which simply ignore it: the
/// dock has to take, hand out and store the bag around EVERY body, and a second
/// body kind would duplicate that path for no gain.
pub(super) type TabBody<'frame, C> =
    Box<dyn FnOnce(&mut egui::Ui, &mut C, &mut TabExtras) + 'frame>;

/// A pinned section of one declared tab, queued by `PanelTab::pinned_top` /
/// `PanelTab::pinned_bottom` and run by `PanelDock::end` around the body.
pub(super) type PinnedBody<'frame, C> = Box<dyn FnOnce(&mut egui::Ui, &mut C) + 'frame>;

/// Every closure one tab declaration queues: the scroll body and the optional
/// pinned sections drawn outside the scroll area.
pub(super) struct TabBodies<'frame, C> {
    /// The body drawn inside the panel's scroll area.
    pub(super) body: TabBody<'frame, C>,
    /// Drawn under the header, above the scroll area.
    pub(super) pinned_top: Option<PinnedBody<'frame, C>>,
    /// Drawn under the scroll area, at the bottom of the panel.
    pub(super) pinned_bottom: Option<PinnedBody<'frame, C>>,
}

/// One declared tab: its metadata, its title producer and its queued closures.
struct TabEntry<'frame, C> {
    meta: TabMeta,
    title: TabTitle<'frame>,
    /// Taken by `end` when the tab turns out to be the drawn one. `None`
    /// afterwards, so a body can never run twice.
    bodies: Option<TabBodies<'frame, C>>,
}

/// Where the dock may place panels this frame.
#[derive(Copy, Clone, Debug)]
pub struct DockArea<'a> {
    /// Region panels are laid out in and clamped to, in screen coordinates.
    pub rect: Rect,
    /// Stable key of the program tab whose layout is being drawn — use
    /// `AppTab::key()`, never a localised title.
    pub layout_key: &'a str,
}

/// State the dock keeps between frames. Lives as a field of the caller.
///
/// **Hard constraint:** the field holding this state must be disjoint from every
/// field a tab body touches. `PanelDock::begin` borrows it mutably for the whole
/// frame, so a body closure that also captured it would not compile — and the
/// point of the deferred API is precisely that bodies borrow OTHER fields.
#[derive(Debug, Default)]
pub struct PanelDockState {
    /// One layout per program tab, keyed by `AppTab::key()`.
    layouts: BTreeMap<String, DockLayout>,
    /// The default-layout builder of every key seen so far, so «Сбросить
    /// раскладку» can rebuild one without the caller having to route the
    /// request back into its own code.
    defaults: BTreeMap<String, fn() -> DockLayout>,
    /// The panel move in flight, if any. At most one gesture at a time: it is
    /// driven by the single pointer.
    drag: Option<DragSession>,
    /// Outer size each tab's content asked for when it was last drawn — the
    /// "geometry lags content by one frame" cache — scoped to the program tab it
    /// was measured in and keyed by `AppTab::key()`.
    ///
    /// A `TabId` alone is NOT a key here: the canvas' «Лента» is declared by
    /// three program tabs sharing one state, and one global entry made the width
    /// the user dragged it to in «Текст» jump into «Клининг», whose own panel
    /// carries no `size_override` and therefore takes its size straight from this
    /// cache. A measurement belongs to the arrangement it was taken in, exactly
    /// like the panel that holds the tab.
    measured: BTreeMap<String, HashMap<TabId, Vec2>>,
    /// Height each tab's PINNED BOTTOM section drew the last time it was drawn,
    /// keyed exactly like [`PanelDockState::measured`] and for the same reason.
    /// It is next frame's reserve under the scroll area
    /// (`CollapsiblePanel::pinned_bottom_estimate`); tabs without such a
    /// section never get an entry.
    pinned_bottom_heights: BTreeMap<String, HashMap<TabId, f32>>,
    /// Extra per-tab UI state the dock stores and persists, scoped exactly like
    /// [`PanelDockState::measured`]: by `AppTab::key()` first, by `TabId` second.
    ///
    /// The keying is the same because the reason is the same — the canvas'
    /// «Лента» is ONE `TabId` declared by three program tabs sharing one state,
    /// so a `TabId`-only key would leak one arrangement's state into another's.
    /// It belongs to the STATE and not to `DockLayout` on purpose: «Сбросить
    /// раскладку» restores the arrangement and must leave what the user expanded
    /// inside a tab alone, and the per-frame `frame_layout` clone would otherwise
    /// have to copy it.
    tab_extras: BTreeMap<String, BTreeMap<TabId, TabExtras>>,
    /// What each PANEL's header measured beyond [`PanelDockState::chrome`] the
    /// last time it was drawn — the extra caption rows and the caption-driven
    /// minimum width — keyed by `AppTab::key()` and then `PanelId`, because a
    /// `PanelId` is only unique inside one program tab's layout. Next frame's
    /// solve reads it (`solve_with_headers`); a panel without an entry is
    /// solved as a one-row header. A derived cache like `measured`: never
    /// persisted, never `dirty`.
    /// Entries are dropped for panels that left the layout (end of every drawn
    /// frame) and for the whole key on «Сбросить раскладку».
    header_metrics: BTreeMap<String, HashMap<PanelId, MeasuredHeader>>,
    /// Header/frame overhead of a ONE-ROW header, as the widget measured it the
    /// last time any panel was drawn. It is style-dependent, so it is measured
    /// rather than assumed; one value covers every panel because they all draw
    /// the same header widget, and what wrapped caption rows add is per panel
    /// ([`PanelDockState::header_metrics`]).
    chrome: PanelChrome,
    /// The detached OS windows this dock owns. Shared by every program tab: a
    /// sub-window outlives a tab switch and simply shows whatever the newly
    /// active tab's layout puts in it — possibly nothing (requirement 11).
    sub_windows: Vec<SubWindowNode>,
    /// Sub-window indices whose viewport has already been created. A window is
    /// given a start position only once: afterwards the window manager and the
    /// user own its placement, and re-asserting it every frame would fight them.
    opened_sub_windows: BTreeSet<u32>,
    /// The report slot of every sub-window currently shown DEFERRED by
    /// [`PanelDockState::show_idle_sub_windows`], shared with that window's
    /// `'static` callback. An entry exists exactly while its window is idle: the
    /// immediate path (`show_sub_windows`) takes every pending report and clears
    /// the map, so a missing entry on an idle pass means "this window just became
    /// idle", which is shown once immediate and empty before it turns deferred.
    /// Runtime only, never persisted.
    idle_windows: BTreeMap<u32, Arc<Mutex<window::IdleWindowReport>>>,
    /// `true` while a tab drag is in flight and the pointer has left every one of
    /// our windows. The primary, platform-independent detach signal (plan §4.8):
    /// `PointerGone` clears `latest_pos` without ending the drag.
    tab_drag_left_window: bool,
    /// Viewport `PanelDock::end` is called from — the dock's MAIN window. Needed
    /// to look this window's own geometry up in the all-viewports map, which is
    /// keyed by `ViewportId` and knows nothing about `HostId`.
    main_viewport: Option<egui::ViewportId>,
    /// Pointer position in the shared monitor frame (physical pixels) while a
    /// reorganisation gesture is in flight, published by whichever window can
    /// still report the cursor.
    ///
    /// A held button keeps an implicit pointer grab on the window the press
    /// started in, so the window the cursor is actually OVER usually cannot see
    /// it. This is how that window learns where the gesture is; it may be one
    /// frame old when the publisher draws after the reader, which is invisible
    /// because a drag repaints continuously.
    drag_pointer_global: Option<Pos2>,
    /// What every window of this dock drew THIS frame, recorded only while a
    /// gesture is in flight and consumed at the end of the frame to address a
    /// drop that crossed a window border.
    frame_hosts: Vec<HostRecord>,
    /// Dock area every window of this dock drew THIS frame, in that window's own
    /// screen coordinates.
    ///
    /// Written on EVERY frame, unlike [`PanelDockState::frame_hosts`]: the
    /// «Переместить в окно →» submenu is not a gesture, and the destination's
    /// area is what puts a menu-moved panel in the MIDDLE of the main window
    /// instead of in its corner (`menu_panel_slot`). One entry per window per
    /// frame costs nothing an idle frame would notice.
    host_areas: BTreeMap<HostId, Rect>,
    /// Set once the "this platform does not place windows" warning has been
    /// logged, so a Wayland session does not log it per detach.
    warned_placement_unsupported: bool,
    /// Whether this pass may open sub-windows: `window::detach_allowed` of
    /// `embedded_window::effective_mode`, set by
    /// [`PanelDockState::apply_window_mode`] before any window draws. `false`
    /// hides the tear-out outline and the «Переместить в окно →» submenu and turns
    /// every detach verdict into an ordinary drop.
    detach_allowed: bool,
    /// Set whenever a layout changed in a way persistence would have to store.
    dirty: bool,
}

impl PanelDockState {
    /// Creates empty dock state: no layouts, no measurements.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs `build()` as the layout of `key` when that key has none yet, and
    /// remembers the builder as that key's DEFAULT.
    ///
    /// Call it before [`PanelDock::begin`] for the program tab about to draw.
    /// The builder is not run when a layout already exists (loaded from the
    /// user config, or built on an earlier frame), so it can be as expensive as
    /// it likes without costing anything per frame. An invalid layout is
    /// rejected with a warning and replaced by an empty one; the dock then
    /// re-creates a panel per declared tab, which is degraded but usable.
    ///
    /// `build` is a plain `fn` rather than a closure on purpose: the dock keeps
    /// it for [`PanelDockState::reset_layout`], which runs long after this call
    /// and must not capture anything of the caller's frame.
    pub fn ensure_default_layout(&mut self, key: &str, build: fn() -> DockLayout) {
        if !self.defaults.contains_key(key) {
            self.defaults.insert(key.to_owned(), build);
        }
        if self.layouts.contains_key(key) {
            return;
        }
        let layout = build();
        let layout = match layout.validate() {
            Ok(()) => layout,
            Err(error) => {
                runtime_log::log_warn(format!(
                    "[panel_dock] default layout for `{key}` is invalid ({error}); \
                     starting from an empty layout"
                ));
                DockLayout::new()
            }
        };
        self.layouts.insert(key.to_owned(), layout);
        self.dirty = true;
    }

    /// Installs the layouts restored from the user config, before the first
    /// frame of any program tab that uses this state.
    ///
    /// A restored layout WINS over the default: [`PanelDockState::ensure_default_layout`]
    /// only builds a layout for a key that has none, so a key installed here
    /// keeps the user's arrangement, while a key absent from the config still
    /// gets its default. Tabs the restored layout does not own are re-created by
    /// the dock itself on the first frame that declares them
    /// (`ensure_declared_tabs`).
    ///
    /// Deliberately does NOT raise `dirty`: what came from the config is not a
    /// user change, and re-writing it on the first frame would be a write per
    /// startup. An entry that fails [`DockLayout::validate`] is refused with a
    /// warning — persistence already validates, so this is the second gate that
    /// keeps an invalid layout out of the live state — and that key falls back
    /// to its default.
    pub fn install_persisted_layouts(&mut self, layouts: BTreeMap<String, DockLayout>) {
        for (key, layout) in layouts {
            if let Err(error) = layout.validate() {
                runtime_log::log_warn(format!(
                    "[panel_dock] the persisted layout of `{key}` is invalid ({error}); the \
                     default layout is used instead"
                ));
                continue;
            }
            self.layouts.insert(key, layout);
        }
    }

    /// Installs the extra per-tab state restored from the user config, before
    /// the first frame of any program tab that uses this state.
    ///
    /// Keyed by `AppTab::key()` and then by `TabId`, exactly like the storage it
    /// fills. Like the layouts, it deliberately does NOT raise `dirty`: what came
    /// from the config is not a user change, and re-writing it on the first frame
    /// would be a write per startup.
    pub fn install_persisted_tab_extras(
        &mut self,
        extras: BTreeMap<String, BTreeMap<TabId, TabExtras>>,
    ) {
        for (key, tabs) in extras {
            self.tab_extras.entry(key).or_default().extend(tabs);
        }
    }

    /// Installs the sub-windows restored from the user config, before the first
    /// frame.
    ///
    /// Call it together with [`PanelDockState::install_persisted_layouts`] and
    /// BEFORE it is used, because a restored layout may put panels into these
    /// windows. Like the layouts, this deliberately does not raise `dirty`.
    ///
    /// A window the restored layouts put no panel in is dropped here rather than
    /// opened empty: an empty window is requirement 11's answer to a program-tab
    /// SWITCH, not to a session that no longer has anything to show in it.
    pub fn install_persisted_sub_windows(&mut self, nodes: Vec<SubWindowNode>) {
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        self.sub_windows = nodes
            .into_iter()
            .filter(|node| seen.insert(node.index))
            .collect();
        let obsolete = window::obsolete_sub_windows(&self.sub_windows, &self.layouts);
        for index in obsolete {
            runtime_log::log_warn(format!(
                "[panel_dock] the persisted sub-window {index} holds no panel in any restored \
                 layout and is not opened"
            ));
            self.sub_windows.retain(|node| node.index != index);
        }
    }

    /// The detached windows this dock currently owns, in creation order.
    #[must_use]
    pub fn sub_windows(&self) -> &[SubWindowNode] {
        &self.sub_windows
    }

    /// Hands out a snapshot of every layout, every sub-window and every tab's
    /// extra state when the USER changed something since the last call, clearing
    /// the dirty flag.
    ///
    /// The persistence poll: the caller passes what it gets to
    /// [`PanelLayoutWriter::store`](persist::PanelLayoutWriter::store), which
    /// coalesces and writes it off the GUI thread. `None` — the common case —
    /// means nothing to write, so an idle frame costs one boolean read.
    #[must_use]
    pub fn take_dirty_layouts(&mut self) -> Option<PanelLayoutSnapshot> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(PanelLayoutSnapshot {
            layouts: self.layouts.clone(),
            sub_windows: self.sub_windows.clone(),
            tab_extras: self.tab_extras.clone(),
        })
    }

    /// Resets the per-frame bookkeeping the cross-window addressing rests on.
    ///
    /// Call it ONCE per frame, from the window `PanelDock::end` draws in, before
    /// anything is drawn: it learns which viewport is this dock's main window
    /// (the all-viewports map is keyed by `ViewportId` and knows nothing about
    /// `HostId`) and drops the previous frame's records. The published pointer is
    /// kept while a gesture is in flight and cleared the moment none is, so a
    /// window can never address a drop against where the cursor was during some
    /// earlier drag.
    fn begin_frame(&mut self, ctx: &egui::Context, gesture_in_flight: bool) {
        self.apply_window_mode(crate::embedded_window::effective_mode(ctx));
        self.main_viewport = Some(ctx.viewport_id());
        self.frame_hosts.clear();
        // Areas are per-frame facts too: a window that stopped drawing must not
        // leave a stale rect behind for a menu move to place a panel against.
        self.host_areas.clear();
        if !gesture_in_flight {
            self.drag_pointer_global = None;
        }
    }

    /// Applies this pass's secondary-window `mode` (from
    /// `crate::embedded_window::effective_mode`, the one owner of that decision;
    /// the dock never reads the setting or a `cfg` itself).
    ///
    /// Sets [`PanelDockState::detach_allowed`]. When detaching is not allowed,
    /// every sub-window is closed through `close_sub_window` — its panels return to
    /// the main window (requirement 10) and the dirty flag persists the re-docked
    /// arrangement, so a stored detached layout is re-docked once and for good —
    /// and a panel drag that was running inside one of them is dropped. Nothing is
    /// restored when the mode turns `Separate` again. Runs before any window of
    /// the pass draws, so a mode flip takes effect without flicker.
    fn apply_window_mode(&mut self, mode: WindowMode) {
        let allowed = window::detach_allowed(mode);
        if allowed != self.detach_allowed {
            runtime_log::log_info(format!(
                "[panel_dock] secondary windows mode {mode:?}: detaching panels into windows \
                 is {}",
                if allowed { "allowed" } else { "disabled" }
            ));
            self.detach_allowed = allowed;
        }
        let redock = window::sub_windows_to_redock(allowed, &self.sub_windows);
        if redock.is_empty() {
            return;
        }
        if self
            .drag
            .as_ref()
            .is_some_and(|session| redock.iter().any(|index| session.host == HostId::SubWindow(*index)))
        {
            // Its window stops being shown, so no pass would ever advance or end
            // the gesture, and `gesture_in_flight` would stay set for good.
            self.drag = None;
        }
        for index in &redock {
            self.close_sub_window(*index);
        }
        runtime_log::log_info(format!(
            "[panel_dock] secondary windows are embedded: {} sub-window(s) re-docked into the \
             main window",
            redock.len()
        ));
    }

    /// Opens a new sub-window and returns its index, or `None` when no index is
    /// left (unreachable in practice — it would take `u32::MAX` live windows).
    ///
    /// `position` is the window's prospective outer position in monitor space;
    /// pass `None` where the platform does not place windows (Wayland), and the
    /// compositor decides instead.
    fn allocate_sub_window(&mut self, position: Option<Pos2>) -> Option<u32> {
        let index = window::next_sub_window_index(&self.sub_windows)?;
        self.sub_windows.push(SubWindowNode::new(
            index,
            position,
            window::DEFAULT_SUB_WINDOW_SIZE,
        ));
        self.dirty = true;
        Some(index)
    }

    /// Closes one sub-window, returning every panel it held to the main window of
    /// the layout it belonged to (requirement 10).
    ///
    /// The panels are never dropped with the window: they carry the user's tabs,
    /// and a tab that is not in any panel of any host is a tab the program can
    /// never show again.
    fn close_sub_window(&mut self, index: u32) {
        let host = HostId::SubWindow(index);
        let mut returned = 0usize;
        for layout in self.layouts.values_mut() {
            returned += layout.rehost_panels(host, HostId::MainWindow);
        }
        self.sub_windows.retain(|node| node.index != index);
        self.opened_sub_windows.remove(&index);
        self.idle_windows.remove(&index);
        self.dirty = true;
        runtime_log::log_info(format!(
            "[panel_dock] sub-window {index} closed; {returned} panel(s) returned to the main \
             window"
        ));
    }

    /// Keeps this dock's sub-windows on screen on a frame where none of its
    /// program tabs draws (requirement 11).
    ///
    /// A child viewport exists only while it is shown, every pass (an unused one
    /// is removed in `end_pass`, fork `crates/egui/crates/egui/src/context.rs:2781-2790`),
    /// so a frame that skips [`PanelDock::end`] would close every detached window
    /// and the user would lose them by switching program tabs. The windows are
    /// therefore shown here EMPTY — the newly active program tab has nothing to
    /// put in them, and the user's decision was that they stay open and grey
    /// rather than disappear.
    ///
    /// They are shown DEFERRED (`show_viewport_deferred`, `context.rs:4069`), with
    /// the same `ViewportId` and builder as the immediate form, so eframe re-classes
    /// the live window in place instead of recreating it — except on the FIRST idle
    /// pass of a window (just idled, or new), which shows it immediate and empty
    /// once so it is painted grey for sure (see the comment in the body). An idle
    /// window then
    /// repaints only on its own events, not with every main-window frame; its
    /// `'static` callback draws the neutral background and records the close
    /// button and geometry into its [`window::IdleWindowReport`], waking the main
    /// window when there is something new, and the next call here (or the next
    /// [`PanelDock::end`]) applies it — closing the window exactly like the
    /// immediate path and storing its geometry, so persistence sees both.
    ///
    /// **Call it exactly once per frame, and only on a frame where no
    /// [`PanelDock::end`] ran for this state**: showing one viewport in both forms
    /// in a pass would render and re-class it twice. Closing and pruning still
    /// happen, so a window whose close button is pressed while the tab is away is
    /// handled at once.
    pub fn show_idle_sub_windows(&mut self, ctx: &egui::Context) {
        // Before the emptiness check: in Embedded mode this re-docks every
        // sub-window, and the pass then shows none.
        self.apply_window_mode(crate::embedded_window::effective_mode(ctx));
        if self.sub_windows.is_empty() {
            self.idle_windows.clear();
            return;
        }
        // No program tab of this dock draws, so no window runs `draw_host` and no
        // gesture can be in flight: the frame records nothing.
        self.begin_frame(ctx, false);
        let live: BTreeSet<u32> = self.sub_windows.iter().map(|node| node.index).collect();
        self.idle_windows.retain(|index, _| live.contains(index));
        let mut needs_repaint = false;
        let mut closed = take_idle_window_reports(self);
        let main_viewport = ctx.viewport_id();
        let nodes: Vec<SubWindowNode> = self.sub_windows.clone();
        for node in nodes {
            if closed.contains(&node.index) {
                continue;
            }
            // Same start-position rule as the immediate form: the stored position
            // is applied once per window, whichever form shows it first.
            let first_frame = self.opened_sub_windows.insert(node.index);
            let title = window::sub_window_title(node.index);
            let builder =
                window::sub_window_builder(&node, &title, if first_frame { node.pos } else { None });
            let viewport_id = window::sub_window_viewport_id(node.index);
            match self.idle_windows.get(&node.index).map(Arc::clone) {
                Some(report) => {
                    ctx.show_viewport_deferred(viewport_id, builder, move |ui, _class| {
                        idle_sub_window_pass(ui, &report, main_viewport);
                    });
                }
                None => {
                    // The window just became idle (or is new): it still shows the
                    // panels of its last immediate frame (or nothing yet). Asking a
                    // deferred viewport to repaint is not reliable here: egui forwards
                    // `request_repaint_of` to eframe only when it shortens the
                    // viewport's pending delay (fork `context.rs:159`), and a window
                    // whose last immediate pass ended with a request keeps that delay
                    // at zero until its next pass — which a deferred viewport only gets
                    // when eframe is told; a brand-new window has no OS window yet
                    // when the request is made. So this pass shows it the old way,
                    // immediate and empty, which paints it grey for sure; from the next
                    // pass on it is deferred with identical content.
                    self.idle_windows.insert(node.index, Arc::default());
                    let close_requested = ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
                        let child = ui.ctx().clone();
                        observe_sub_window_geometry(&child, self, node);
                        egui::CentralPanel::default().show(ui, |_ui| {});
                        child.input(|input| input.viewport().close_requested())
                    });
                    if close_requested {
                        closed.push(node.index);
                    }
                }
            }
        }
        for index in &closed {
            self.close_sub_window(*index);
            needs_repaint = true;
        }
        if self.prune_sub_windows() {
            needs_repaint = true;
        }
        if needs_repaint {
            ctx.request_repaint();
        }
    }

    /// Drops every sub-window that holds no panel in ANY layout (requirement 10).
    ///
    /// Returns `true` when something was dropped. A window that is merely empty
    /// in the program tab currently drawn is NOT obsolete — that is requirement
    /// 11 and keeps it open and grey.
    fn prune_sub_windows(&mut self) -> bool {
        let obsolete = window::obsolete_sub_windows(&self.sub_windows, &self.layouts);
        if obsolete.is_empty() {
            return false;
        }
        for index in &obsolete {
            self.sub_windows.retain(|node| node.index != *index);
            self.opened_sub_windows.remove(index);
            self.idle_windows.remove(index);
            runtime_log::log_info(format!(
                "[panel_dock] sub-window {index} lost its last panel and is closed"
            ));
        }
        self.dirty = true;
        true
    }

    /// Layout of one program tab, if it has one.
    #[must_use]
    pub fn layout(&self, key: &str) -> Option<&DockLayout> {
        self.layouts.get(key)
    }

    /// Mutable layout of one program tab.
    ///
    /// Marks the state dirty unconditionally: the dock cannot tell whether the
    /// caller changed anything.
    pub fn layout_mut(&mut self, key: &str) -> Option<&mut DockLayout> {
        let layout = self.layouts.get_mut(key)?;
        self.dirty = true;
        Some(layout)
    }

    /// Outer size `tab`'s content asked for the last time it was drawn in the
    /// program tab `layout_key`, with its panel's header counted as ONE row: the
    /// extra caption rows a wrapped header drew are left out, because they belong
    /// to the panel and its current width, not to the tab ([`plan_frame`] adds
    /// the panel's current rows back).
    ///
    /// The key is part of the question: a tab several program tabs declare is
    /// measured once per arrangement, and the answer for one of them says
    /// nothing about the others.
    #[must_use]
    pub fn measured_size(&self, layout_key: &str, tab: TabId) -> Option<Vec2> {
        self.measured.get(layout_key)?.get(&tab).copied()
    }

    /// Measurements taken in the program tab `layout_key`; `None` while nothing
    /// has ever been drawn for it. Callers substitute an empty map.
    fn measurements(&self, layout_key: &str) -> Option<&HashMap<TabId, Vec2>> {
        self.measured.get(layout_key)
    }

    /// Remembers what one tab's content measured while the program tab
    /// `layout_key` was drawing it.
    fn remember_measurement(&mut self, layout_key: &str, tab: TabId, size: Vec2) {
        // Looked up before `entry`, which would have to own the key: this runs
        // once per DRAWN PANEL per frame, and every call after the program tab's
        // first frame finds the entry already there. Allocating a `String` for a
        // key that is present is the whole cost of the frame path here.
        if let Some(sizes) = self.measured.get_mut(layout_key) {
            sizes.insert(tab, size);
            return;
        }
        self.measured
            .entry(layout_key.to_owned())
            .or_default()
            .insert(tab, size);
    }

    /// Height `tab`'s pinned bottom section drew the last time the program tab
    /// `layout_key` drew it; `None` before the first time.
    fn pinned_bottom_height(&self, layout_key: &str, tab: TabId) -> Option<f32> {
        self.pinned_bottom_heights.get(layout_key)?.get(&tab).copied()
    }

    /// Remembers what `tab`'s pinned bottom section drew while the program tab
    /// `layout_key` was drawing it.
    fn remember_pinned_bottom_height(&mut self, layout_key: &str, tab: TabId, height: f32) {
        // Same lookup-before-entry reasoning as `remember_measurement`.
        if let Some(heights) = self.pinned_bottom_heights.get_mut(layout_key) {
            heights.insert(tab, height);
            return;
        }
        self.pinned_bottom_heights
            .entry(layout_key.to_owned())
            .or_default()
            .insert(tab, height);
    }

    /// Every cached header measurement of the program tab `layout_key`.
    fn header_measurements(&self, layout_key: &str) -> Option<&HashMap<PanelId, MeasuredHeader>> {
        self.header_metrics.get(layout_key)
    }

    /// Remembers what `panel`'s header measured, showing `tabs`, while the
    /// program tab `layout_key` was drawing it.
    fn remember_header_metrics(&mut self, layout_key: &str, panel: PanelId, tabs: &[TabId], metrics: PanelHeaderMetrics) {
        let entry = MeasuredHeader {
            tabs: tabs.to_vec(),
            metrics,
        };
        // Same lookup-before-entry reasoning as `remember_measurement`.
        if let Some(headers) = self.header_metrics.get_mut(layout_key) {
            headers.insert(panel, entry);
            return;
        }
        self.header_metrics
            .entry(layout_key.to_owned())
            .or_default()
            .insert(panel, entry);
    }

    /// Drops the header measurements of panels the layout `layout_key` no
    /// longer holds (removed, merged away), so a later panel that reuses the id
    /// cannot inherit them. A panel moved to another window keeps its entry:
    /// it is still the same panel with the same captions.
    fn prune_header_metrics(&mut self, layout_key: &str) {
        let Some(layout) = self.layouts.get(layout_key) else {
            self.header_metrics.remove(layout_key);
            return;
        };
        if let Some(headers) = self.header_metrics.get_mut(layout_key) {
            headers.retain(|panel, _| layout.panel(*panel).is_some());
        }
    }

    /// Extra state stored for `tab` inside the program tab `layout_key`, if the
    /// tab ever stored anything.
    ///
    /// The key is part of the question for the same reason it is part of
    /// [`PanelDockState::measured_size`]'s: a tab declared by several program
    /// tabs keeps one bag per arrangement.
    #[must_use]
    pub fn tab_extras(&self, layout_key: &str, tab: TabId) -> Option<&TabExtras> {
        self.tab_extras.get(layout_key)?.get(&tab)
    }

    /// Takes the bag of `tab` out of the state for the duration of one body.
    ///
    /// The driver hands the returned value to the tab body as `&mut TabExtras`
    /// and puts it back with [`PanelDockState::put_tab_extras`] afterwards. It is
    /// REMOVED rather than borrowed because the body runs inside
    /// `CollapsiblePanel::show`, where no borrow of the dock state may be alive —
    /// the same closure still has to reach `state` for the measurement it
    /// reports. A tab that never stored anything yields an empty bag.
    #[must_use]
    pub fn take_tab_extras(&mut self, layout_key: &str, tab: TabId) -> TabExtras {
        self.tab_extras
            .get_mut(layout_key)
            .and_then(|tabs| tabs.remove(&tab))
            .unwrap_or_default()
    }

    /// Puts a bag taken by [`PanelDockState::take_tab_extras`] back.
    ///
    /// An EMPTY bag is not stored (and drops whatever was there): every flag is
    /// then at its default, which is precisely what persistence must not write.
    /// Deliberately does not touch `dirty` — whether the body changed anything is
    /// [`TabExtras::changed`]'s answer, and the driver acts on that one alone.
    pub fn put_tab_extras(&mut self, layout_key: &str, tab: TabId, extras: TabExtras) {
        if extras.is_empty() {
            if let Some(tabs) = self.tab_extras.get_mut(layout_key) {
                tabs.remove(&tab);
            }
            return;
        }
        if let Some(tabs) = self.tab_extras.get_mut(layout_key) {
            tabs.insert(tab, extras);
            return;
        }
        self.tab_extras
            .entry(layout_key.to_owned())
            .or_default()
            .insert(tab, extras);
    }

    /// Stores the bag of the tab that was just DRAWN and raises `dirty` when —
    /// and only when — the body really changed one of its flags.
    ///
    /// The driver's post-body step, kept out of the drawing loop so the rule is
    /// unit-testable without a window. `dirty` is persistence's signal that the
    /// USER changed something, so a body that merely read its extras, or wrote
    /// back what was already stored, must leave it alone. The change flag is
    /// bookkeeping for one frame and is cleared before the bag goes back into the
    /// state.
    fn finish_tab_extras(&mut self, layout_key: &str, tab: TabId, mut extras: TabExtras) {
        if extras.changed() {
            self.dirty = true;
        }
        extras.clear_changed();
        self.put_tab_extras(layout_key, tab, extras);
    }

    /// Writes one flag of `tab`'s extras in the program tab `layout_key` from
    /// OUTSIDE a tab body, with the same footprint and dirty rules a body write has
    /// ([`TabExtras::set_flag`]): `dirty` rises only when the stored content really
    /// moved.
    ///
    /// For state a host owns and merely stores in the dock — e.g. which of its
    /// tool tabs are switched on — that must persist even on frames the owning
    /// tab's body does not run (its panel collapsed, hidden or showing a sibling).
    /// Call it before [`PanelDock::begin`] or after [`PanelDock::end`]: `begin`
    /// holds the state borrow for the frame.
    pub fn set_tab_extras_flag(
        &mut self,
        layout_key: &str,
        tab: TabId,
        key: &str,
        value: bool,
        default: bool,
    ) {
        let mut extras = self.take_tab_extras(layout_key, tab);
        extras.set_flag(key, value, default);
        self.finish_tab_extras(layout_key, tab, extras);
    }

    /// Brings `tab` to the front of the panel that owns it in the program tab
    /// `layout_key` and expands that panel if it was collapsed — what a host does
    /// when the user switches a tool on and expects to see it.
    ///
    /// Returns `false`, changing nothing, when the program tab has no layout yet or
    /// no panel owns `tab` (a tab is seeded into a panel at the end of the first
    /// frame that declares it, so the caller may retry on the next frame). Raises
    /// `dirty` only when the active tab or the collapsed state really changed, so a
    /// tab already in front costs no config write. Works for a panel in any window.
    /// Call it before [`PanelDock::begin`], like
    /// [`PanelDockState::set_tab_extras_flag`].
    pub fn focus_tab(&mut self, layout_key: &str, tab: TabId) -> bool {
        let Some(layout) = self.layouts.get_mut(layout_key) else {
            return false;
        };
        let Some(panel_id) = layout.panel_of_tab(tab) else {
            return false;
        };
        let Some(panel) = layout.panel(panel_id) else {
            return false;
        };
        if panel.active_tab == tab && !panel.collapsed {
            return true;
        }
        // Neither setter can fail here — `panel_of_tab` just found `panel_id` and it
        // owns `tab` — but a refusal is still logged rather than unwrapped.
        if let Err(error) = layout
            .set_active_tab(panel_id, tab)
            .and_then(|()| layout.set_collapsed(panel_id, false))
        {
            runtime_log::log_warn(format!(
                "[panel_dock] could not focus tab `{tab}` in panel {panel_id} of `{layout_key}`: \
                 {error}"
            ));
            return false;
        }
        self.dirty = true;
        true
    }

    /// One-row header/frame overhead measured on the last drawn frame, or the
    /// nominal estimate before anything has been drawn. A panel whose captions
    /// wrapped is taller by its own extra rows, which this value leaves out.
    #[must_use]
    pub fn chrome(&self) -> PanelChrome {
        self.chrome
    }

    /// `true` when a layout changed since the last [`PanelDockState::clear_dirty`].
    /// Persistence (phase 5) drives its writer off this flag.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag, e.g. after a snapshot was handed to the writer.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Restores the layout `key` was given by its default builder, discarding
    /// every user reorganisation of that program tab.
    ///
    /// Returns `false` — with a warning — when the key was never passed to
    /// [`PanelDockState::ensure_default_layout`], or when its builder now
    /// produces an invalid layout; the current layout is kept in both cases,
    /// because dropping it would leave the user with no panels at all.
    ///
    /// Any gesture in flight is cancelled: it addresses panel ids that the fresh
    /// layout does not necessarily contain.
    pub fn reset_layout(&mut self, key: &str) -> bool {
        let Some(build) = self.defaults.get(key).copied() else {
            runtime_log::log_warn(format!(
                "[panel_dock] cannot reset the layout of `{key}`: no default layout was ever \
                 declared for it"
            ));
            return false;
        };
        let layout = build();
        if let Err(error) = layout.validate() {
            runtime_log::log_warn(format!(
                "[panel_dock] the default layout of `{key}` is invalid ({error}); the current \
                 layout is kept"
            ));
            return false;
        }
        self.layouts.insert(key.to_owned(), layout);
        // The default rebuilds panels under ids the old layout may have used
        // for other panels; their header measurements describe those.
        self.header_metrics.remove(key);
        self.drag = None;
        self.dirty = true;
        true
    }
}

/// One panel as it was drawn this frame: the window it was drawn in, and its
/// outer rect in THAT window's own screen coordinates.
#[derive(Copy, Clone, Debug, PartialEq)]
struct DrawnPanel {
    host: HostId,
    rect: Rect,
}

/// Rects of everything the dock drew this frame.
///
/// Only panels that were actually drawn appear here: a panel whose tabs are all
/// hidden or undeclared has no rect, exactly as if it were not on screen.
///
/// **Every accessor answers for the MAIN window alone, and that is the type's
/// whole point.** A panel the user detached into a sub-window is drawn in that
/// window's own coordinate frame, whose dock area starts near the origin; the
/// numbers are meaningless to the main window and dangerous there — compared
/// against a main-window pointer position they carve an invisible dead zone out
/// of its top-left corner, and used as an anchor they place UI at the wrong
/// place. The host is therefore recorded with every rect and a sub-window's
/// geometry is never handed out unlabelled: a detached panel reads as "not on
/// screen", which is exactly what a caller anchoring main-window UI to a dock
/// panel has to do with it anyway.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelDockOutput {
    panels: BTreeMap<PanelId, DrawnPanel>,
    tabs: BTreeMap<TabId, PanelId>,
}

impl PanelDockOutput {
    /// Records one panel drawn in `host` and the tabs whose headers it showed.
    ///
    /// `rect` is in `host`'s own screen coordinates. Panel ids are unique across
    /// the hosts of one layout, so two windows can never overwrite each other's
    /// entry.
    fn record(&mut self, host: HostId, panel: PanelId, rect: Rect, tabs: &[TabId]) {
        self.panels.insert(panel, DrawnPanel { host, rect });
        for tab in tabs {
            self.tabs.insert(*tab, panel);
        }
    }

    /// Rect of one panel drawn in the MAIN window. `None` when the panel was not
    /// drawn at all, or was drawn in a detached sub-window.
    #[must_use]
    pub fn panel_rect(&self, id: PanelId) -> Option<Rect> {
        self.panels
            .get(&id)
            .filter(|drawn| drawn.host == HostId::MainWindow)
            .map(|drawn| drawn.rect)
    }

    /// Outer rect of the panel that showed `tab` this frame, in MAIN-window
    /// coordinates.
    ///
    /// `None` when the tab was hidden, undeclared, its panel had nothing to
    /// draw, or the user moved it into a detached sub-window — all four are "not
    /// on screen in this window" to a caller anchoring other main-window UI to a
    /// dock panel, and none of them may be approximated.
    #[must_use]
    pub fn tab_rect(&self, tab: TabId) -> Option<Rect> {
        let panel = self.tabs.get(&tab)?;
        self.panel_rect(*panel)
    }

    /// `true` when nothing was drawn in the MAIN window.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.drawn_panels().next().is_none()
    }

    /// Every panel drawn in the MAIN window with its rect, in ascending panel id
    /// order. Panels drawn in a sub-window are deliberately left out — see the
    /// type's own documentation.
    pub fn drawn_panels(&self) -> impl Iterator<Item = (PanelId, Rect)> + '_ {
        self.panels
            .iter()
            .filter(|(_, drawn)| drawn.host == HostId::MainWindow)
            .map(|(id, drawn)| (*id, drawn.rect))
    }
}

/// The per-frame driver of the dock.
///
/// Usage — the inversion the whole design rests on (see
/// `dev-docs/dockable_panels_plan.md` §4.2): the caller declares TABS, the dock
/// decides which panel each one lives in.
///
/// ```ignore
/// state.ensure_default_layout(AppTab::Typing.key(), default_layout);
/// let mut cx = TypingDockCx { top_panel, text_overlays, page_idx };
/// let mut dock = PanelDock::begin(ctx, &mut self.panel_dock, DockArea {
///     rect: canvas_rect,
///     layout_key: AppTab::Typing.key(),
/// });
/// dock.tab(PREVIEW_TAB)
///     .title(|| t!("typing.preview.panel_heading"))
///     .visible(create_mode)
///     .show(|ui, cx| cx.top_panel.draw_preview_tab_body(ui));
/// dock.tab(LAYERS_TAB)
///     .title(|| t!("typing.panel.layers_tab"))
///     .show(|ui, cx| cx.text_overlays.draw_layers_tab_body(ui, cx.page_idx));
/// let out = dock.end(&mut cx);
/// ```
///
/// Lifetimes: `'ctx` covers the context and the borrowed state, `'frame` covers
/// the queued closures. They are independent on purpose. `C` is the caller's
/// per-frame context; bodies capture nothing and receive `&mut C` one at a time,
/// so any number of tabs may touch the same caller state in one frame.
pub struct PanelDock<'ctx, 'frame, C> {
    ctx: &'ctx egui::Context,
    state: &'ctx mut PanelDockState,
    rect: Rect,
    layout_key: &'ctx str,
    /// Declaration order, which decides where auto-created panels cascade.
    order: Vec<TabId>,
    entries: BTreeMap<TabId, TabEntry<'frame, C>>,
}

impl<'ctx, 'frame, C> PanelDock<'ctx, 'frame, C> {
    /// Opens a dock frame for one program tab.
    ///
    /// Nothing is drawn until [`PanelDock::end`]. Panels live in
    /// [`HostId::MainWindow`]; sub-windows are a later phase.
    #[must_use]
    pub fn begin(
        ctx: &'ctx egui::Context,
        state: &'ctx mut PanelDockState,
        area: DockArea<'ctx>,
    ) -> Self {
        Self {
            ctx,
            state,
            rect: area.rect,
            layout_key: area.layout_key,
            order: Vec::new(),
            entries: BTreeMap::new(),
        }
    }

    /// Starts declaring the tab `id` for this frame.
    ///
    /// The returned builder does nothing until `show` is called on it.
    pub fn tab(&mut self, id: TabId) -> PanelTab<'_, 'ctx, 'frame, C> {
        PanelTab::new(self, id)
    }

    /// Records one declared tab. Called by [`PanelTab::show`].
    fn declare(
        &mut self,
        id: TabId,
        meta: TabMeta,
        title: TabTitle<'frame>,
        bodies: TabBodies<'frame, C>,
    ) {
        if self.entries.contains_key(&id) {
            runtime_log::log_warn(format!(
                "[panel_dock] tab `{}` declared twice in one frame for layout `{}`; \
                 the second declaration is ignored",
                id.as_str(),
                self.layout_key
            ));
            return;
        }
        self.order.push(id);
        self.entries.insert(
            id,
            TabEntry {
                meta,
                title,
                bodies: Some(bodies),
            },
        );
    }

    /// Resolves the layout and draws every drawable panel, in ascending panel id
    /// order.
    ///
    /// The frame model (plan §4.3): panels are laid out with the sizes measured
    /// LAST frame (or the tab's `initial_size` / `min_size` the first time), each
    /// body is measured while it is drawn, and a measurement that differs from
    /// the assumption by at least one point triggers a repaint so the layout
    /// converges within a couple of frames.
    ///
    /// Tabs declared this frame that no panel owns are added to the layout first
    /// — see [`ensure_declared_tabs`] for the rule. A panel with nothing to draw
    /// is dropped from the SOLVED layout for this frame only — see
    /// [`frame_layout`].
    ///
    /// `cx` is handed to one body at a time and must be disjoint from the
    /// `PanelDockState` borrowed by [`PanelDock::begin`].
    ///
    /// Every window this dock owns is drawn here: the main one first, then one
    /// immediate child viewport per sub-window (plan §4.9). The order matters —
    /// a tab released over a sub-window must be able to reach that window's
    /// header strip before the main window's release verdict turns the drop into
    /// yet another window.
    #[must_use]
    pub fn end(self, cx: &mut C) -> PanelDockOutput {
        let Self {
            ctx,
            state,
            rect,
            layout_key,
            order,
            mut entries,
        } = self;
        let decls: BTreeMap<TabId, TabMeta> = entries
            .iter()
            .map(|(id, entry)| (*id, entry.meta))
            .collect();

        if !state.layouts.contains_key(layout_key) {
            state.layouts.insert(layout_key.to_owned(), DockLayout::new());
        }
        {
            // The default arrangement is rebuilt only on a frame where some
            // declared tab still has no panel — the first frame of a key with no
            // stored layout, or the first frame after a build added a tab. The
            // insertion below then gives every declared tab a panel, so the
            // following frames find nothing missing and never run the builder.
            // The one case that keeps rebuilding it every frame is an insertion
            // that cannot succeed (`PanelIdOverflow`, `DuplicatePanelId`): the
            // tab stays unowned and `ensure_declared_tabs` logs the refusal each
            // time. Nothing is built for that here — the model refuses both only
            // for a layout that cannot arise from these helpers.
            let missing = state.layouts.get(layout_key).is_some_and(|layout| {
                order.iter().any(|tab| layout.panel_of_tab(*tab).is_none())
            });
            let default = missing
                .then(|| state.defaults.get(layout_key).copied())
                .flatten()
                .map(|build| build());
            let Some(layout) = state.layouts.get_mut(layout_key) else {
                return PanelDockOutput::default();
            };
            if ensure_declared_tabs(layout, HostId::MainWindow, &order, default.as_ref()) {
                state.dirty = true;
            }
        }

        // Decided once, before any window draws: the gesture ends during the
        // owning window's pass, and every window drawn after that still has to
        // record the geometry the drop is addressed against.
        let gesture_in_flight =
            egui::DragAndDrop::has_payload_of_type::<DraggedTab>(ctx) || state.drag.is_some();
        state.begin_frame(ctx, gesture_in_flight);

        let mut output = PanelDockOutput::default();
        let mut outcome = draw_host(
            ctx,
            state,
            HostFrame {
                host: HostId::MainWindow,
                area: rect,
                layout_key,
                gesture_in_flight,
            },
            &decls,
            &mut entries,
            cx,
            &mut output,
        );
        outcome.merge(show_sub_windows(
            ctx,
            state,
            FrameContext {
                layout_key,
                gesture_in_flight,
            },
            &decls,
            &mut entries,
            cx,
            &mut output,
        ));

        if apply_frame_detaches(state, layout_key, ctx, &outcome) {
            outcome.needs_repaint = true;
        }
        for index in &outcome.closed_windows {
            state.close_sub_window(*index);
            outcome.needs_repaint = true;
        }
        if state.prune_sub_windows() {
            outcome.needs_repaint = true;
        }
        if outcome.needs_repaint {
            ctx.request_repaint();
        }
        output
    }
}

#[cfg(test)]
mod tests;
