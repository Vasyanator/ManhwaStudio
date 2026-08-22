/*
File: src/widgets/panel_dock/mod.rs

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
- `PanelDockState`: per-program-tab layouts, last-frame measurements, per-tab
  extra state (`extras.rs`), dirty flag.
- `DockArea`: where the dock may place panels this frame.
- `PanelDock`: the frame driver (`begin` → `tab(..).show(..)` → `end`).
- `PanelDockOutput`: rects of the panels drawn this frame, each recorded with
  the window it was drawn in and handed out for the MAIN window only.

Key functions:
- `PanelDock::begin` / `PanelDock::tab` / `PanelDock::end`.
- `PanelDockState::ensure_default_layout`.
- the pure frame helpers `ensure_declared_tabs`, `plan_frame`, `tab_request`,
  `frame_layout`, `apply_mutations` and `write_back_positions`, all unit-tested
  without a GPU.
- the cross-window plumbing `window_geometries`, `apply_frame_detaches`,
  `apply_addressed_tab_drop`, `apply_addressed_panel_drop` and
  `paint_cross_window_feedback`, which feed the pure decisions of
  `cross_window.rs`.

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
*/

pub mod cross_window;
pub mod drag;
pub mod extras;
pub mod model;
pub mod panel;
pub mod persist;
pub mod solver;
pub mod tab;
pub mod window;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use egui::{Pos2, Rect, Vec2};

use crate::runtime_log;

pub use cross_window::{DropAddress, PanelDropTarget, TabLanding, WindowGeometry};
pub use drag::{DragSession, DraggedTab, SNAP_DISTANCE, SnapCandidate, SnapTargets};
pub use extras::TabExtras;
pub use model::{
    DetachTabOutcome, DockEdge, DockLayout, DockModelError, HostId, MoveTabOutcome, PanelAnchor,
    PanelId, PanelNode, TabId,
};
pub use panel::{
    CollapsiblePanel, CollapsiblePanelOutput, MoveTargetEntry, PanelTabHeader, TabDrop,
};
pub use persist::{
    PANEL_LAYOUT_SECTION_KEY, PANEL_LAYOUT_SECTION_VERSION, PanelLayoutError, PanelLayoutSnapshot,
    PanelLayoutWriter, layouts_from_user_settings,
};
pub use solver::{
    COLLAPSED_PANEL_HEIGHT, DEFAULT_PANEL_SIZE, DOCK_GAP, PANEL_MIN_BODY_HEIGHT,
    PANEL_MIN_CONTENT_HEIGHT, PANEL_MIN_WIDTH, PanelChrome, PanelSizes, SolvedLayout, SolvedPanel,
    solve,
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

/// One declared tab: its metadata, its title producer and its queued body.
struct TabEntry<'frame, C> {
    meta: TabMeta,
    title: TabTitle<'frame>,
    /// Taken by `end` when the tab turns out to be the drawn one. `None`
    /// afterwards, so a body can never run twice.
    body: Option<TabBody<'frame, C>>,
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
    /// Header/frame overhead the widget measured the last time any panel was
    /// drawn. It is style-dependent, so it is measured rather than assumed; one
    /// value covers every panel because they all draw the same header widget.
    chrome: PanelChrome,
    /// The detached OS windows this dock owns. Shared by every program tab: a
    /// sub-window outlives a tab switch and simply shows whatever the newly
    /// active tab's layout puts in it — possibly nothing (requirement 11).
    sub_windows: Vec<SubWindowNode>,
    /// Sub-window indices whose viewport has already been created. A window is
    /// given a start position only once: afterwards the window manager and the
    /// user own its placement, and re-asserting it every frame would fight them.
    opened_sub_windows: BTreeSet<u32>,
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
    /// Set once the "this backend has no OS windows" warning has been logged
    /// (web builds, where every viewport is embedded).
    warned_embedded_viewports: bool,
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
        self.main_viewport = Some(ctx.viewport_id());
        self.frame_hosts.clear();
        // Areas are per-frame facts too: a window that stopped drawing must not
        // leave a stale rect behind for a menu move to place a panel against.
        self.host_areas.clear();
        if !gesture_in_flight {
            self.drag_pointer_global = None;
        }
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
        self.dirty = true;
        runtime_log::log_info(format!(
            "[panel_dock] sub-window {index} closed; {returned} panel(s) returned to the main \
             window"
        ));
    }

    /// Keeps this dock's sub-windows on screen on a frame where none of its
    /// program tabs draws (requirement 11).
    ///
    /// An immediate viewport exists only while it is shown, every pass
    /// (`egui-0.35.0/src/context.rs:3997`), so a frame that skips
    /// [`PanelDock::end`] would close every detached window and the user would
    /// lose them by switching program tabs. The windows are therefore shown here
    /// EMPTY — the newly active program tab has nothing to put in them, and the
    /// user's decision was that they stay open and grey rather than disappear.
    ///
    /// **Call it exactly once per frame, and only on a frame where no
    /// [`PanelDock::end`] ran for this state**: showing one viewport twice in a
    /// pass renders it twice. Closing and pruning still happen, so a window whose
    /// close button is pressed while the tab is away is handled at once.
    pub fn show_idle_sub_windows(&mut self, ctx: &egui::Context) {
        if self.sub_windows.is_empty() {
            return;
        }
        // No program tab of this dock draws, so no window runs `draw_host` and no
        // gesture can be in flight: the frame records nothing.
        self.begin_frame(ctx, false);
        let mut entries: BTreeMap<TabId, TabEntry<'_, ()>> = BTreeMap::new();
        let mut output = PanelDockOutput::default();
        let mut unit = ();
        let outcome = show_sub_windows(
            ctx,
            self,
            FrameContext {
                layout_key: None,
                gesture_in_flight: false,
            },
            &BTreeMap::new(),
            &mut entries,
            &mut unit,
            &mut output,
        );
        let mut needs_repaint = outcome.needs_repaint;
        for index in &outcome.closed_windows {
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
    /// program tab `layout_key`.
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

    /// Header/frame overhead measured on the last drawn frame, or the nominal
    /// estimate before anything has been drawn.
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
        body: TabBody<'frame, C>,
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
                body: Some(body),
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
                layout_key: Some(layout_key),
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

/// Which window `draw_host` is drawing, and where in it.
#[derive(Copy, Clone, Debug)]
struct HostFrame<'a> {
    host: HostId,
    area: Rect,
    layout_key: &'a str,
    /// `true` while a tab or panel is being moved somewhere in this dock.
    ///
    /// Decided ONCE per frame, before any window draws, because the gesture ENDS
    /// during the owning window's pass: a flag re-read per window would be false
    /// for every window drawn after the release, and those are exactly the
    /// windows whose geometry the drop has to be addressed against. While it is
    /// false nothing is recorded and the dock behaves exactly as it did before
    /// cross-window addressing existed.
    gesture_in_flight: bool,
}

/// What the frame as a whole tells every window it draws.
#[derive(Copy, Clone, Debug)]
struct FrameContext<'a> {
    /// Program tab being drawn, or `None` on a frame where no tab of this dock
    /// draws at all ([`PanelDockState::show_idle_sub_windows`]).
    layout_key: Option<&'a str>,
    /// `true` while a tab or panel is being moved somewhere in this dock. See
    /// [`HostFrame::gesture_in_flight`] for why it is decided once per frame.
    gesture_in_flight: bool,
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
struct HostRecord {
    /// The window.
    host: HostId,
    /// Its dock area, in its own screen coordinates.
    area: Rect,
    /// Every panel it drew, in draw order.
    panels: Vec<PanelDropTarget>,
    /// Pointer position this window reported THIS frame, in the shared monitor
    /// frame. `None` when it saw no pointer, or has no geometry to lift one with.
    pointer_global: Option<Pos2>,
}

/// A tab drag no window consumed, waiting for the frame's address resolution.
///
/// It is NOT yet a detach: where it goes is decided from the shared monitor frame
/// once every window has drawn (`cross_window::address_drop`), and it becomes a
/// new window only if it landed on none of ours.
#[derive(Copy, Clone, Debug, PartialEq)]
struct PendingTabDrop {
    tab: TabId,
    /// Where inside its caption the tab was grabbed; a panel created for it is
    /// placed by this offset so the caption stays under the cursor.
    grab_offset: Vec2,
    /// The window whose pass saw the release.
    source: HostId,
    /// Release point in the shared monitor frame, in physical pixels. `None`
    /// when this window could not lift its pointer into that frame.
    global: Option<Pos2>,
    /// Why the source window let the drop go; only used when it turns into a
    /// window of its own, and logged there.
    trigger: DetachTrigger,
}

/// A whole panel whose move no window consumed. Same resolution as
/// [`PendingTabDrop`].
#[derive(Copy, Clone, Debug, PartialEq)]
struct PendingPanelDrop {
    panel: PanelId,
    /// Where inside the panel it was grabbed, so the panel keeps that point under
    /// the cursor when it lands in another window.
    grab_offset: Vec2,
    source: HostId,
    global: Option<Pos2>,
    trigger: DetachTrigger,
}

/// What a «Переместить в окно →» menu item asked to move.
#[derive(Copy, Clone, Debug, PartialEq)]
enum MenuMoveSubject {
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
struct PendingMenuMove {
    /// What is being moved.
    subject: MenuMoveSubject,
    /// Where it is going.
    target: MoveTarget,
    /// Prospective outer position of a NEW window in monitor space, in points.
    /// `None` where the platform does not report window positions (Wayland), and
    /// unused for a move into a window that already exists.
    place_at: Option<Pos2>,
}

/// What one window's pass decided and left for the frame to apply.
///
/// The drops are collected rather than applied on the spot because they are
/// decisions ABOUT the whole dock — the window they belong to may not have drawn
/// yet, a new sub-window may have to be allocated, and a drop that another window
/// may still claim must not be consumed by the first window that saw the release.
#[derive(Clone, Debug, Default, PartialEq)]
struct HostOutcome {
    needs_repaint: bool,
    /// A tab drag no window consumed, still holding its payload.
    tab_drop: Option<PendingTabDrop>,
    /// An explicit «Переместить в окно →» from a context menu.
    menu_move: Option<PendingMenuMove>,
    /// A panel move that ended outside the dock area it started in.
    panel_drop: Option<PendingPanelDrop>,
    /// Sub-windows whose own close button was pressed this frame.
    closed_windows: Vec<u32>,
}

impl HostOutcome {
    /// Folds a later window's outcome into this one. The FIRST window that
    /// decided something keeps the decision: every gesture here is driven by the
    /// single pointer, so a second one is a contradiction, not an addition.
    fn merge(&mut self, other: Self) {
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
fn draw_host<'frame, C>(
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
    // (`egui-0.35.0/src/input_state/mod.rs:1103`, `:1204`): that frame's value is
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
        let plan = plan_frame(layout, host, decls, measured);
        // Panels with nothing to draw are removed from the layout the SOLVER
        // sees, never from the stored one: a hidden panel must not reserve
        // its slot in the chain, or the panels below it would float over a
        // gap where nothing is drawn.
        let effective = frame_layout(layout, &plan);
        let solved = solve(
            &effective,
            host,
            area,
            &plan.desired,
            &plan.mins,
            state.chrome,
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
    let move_labels: Vec<(MoveTarget, String)> = window::move_targets(&state.sub_windows, host)
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
        // another of its tabs declared.
        let min_size = plan.mins.get(panel_plan.id).unwrap_or(Vec2::new(
            0.0,
            state.chrome.collapsed_height + PANEL_MIN_CONTENT_HEIGHT,
        ));
        // What must keep a transparent panel visible even with the pointer
        // nowhere near it. Both are gestures the panel is the TARGET of, and a
        // panel that faded out in the middle of one would blink at the user:
        // a tab in flight anywhere in the dock has to have every transparent
        // panel to aim at, and a panel being MOVED is pinned to the dock area's
        // border while the cursor is pulled outside it (the tension model), so
        // the cursor is regularly not over the panel it is dragging.
        let force_visible = carries_tab
            || matches!(drag_phase, DragPhase::Moving { panel, .. } if panel == panel_plan.id);
        let body = entries.get_mut(&active).and_then(|entry| entry.body.take());
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
            .show(ctx, |ui| {
                if let Some(body) = body {
                    body(ui, cx, &mut extras);
                }
            });
        // Back into the state, raising `dirty` only for a REAL change — see
        // `finish_tab_extras`. Outside the `show` closure above, like every other
        // touch of `state` in this loop.
        state.finish_tab_extras(layout_key, active, extras);
        chrome = drawn.chrome;

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
            state.remember_measurement(
                layout_key,
                active,
                Vec2::new(panel_plan.assumed_size.x, measured.y),
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
            let verdict = if owns_pointer {
                window::detach_trigger(DragEndContext {
                    pointer_left_window: state.tab_drag_left_window,
                    release_pos: pointer_pos,
                    area,
                })
            } else {
                Some(DetachTrigger::PointerLeftWindow)
            };
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
                } else if tension.is_torn_off() {
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

/// Geometry of every window of this dock, in the shared monitor frame.
///
/// `RawInput::viewports` carries the info of ALL viewports in every pass
/// (`eframe-0.35.0/src/native/glow_integration.rs:585-589`, `:1549-1554`), so one
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
fn window_geometries(ctx: &egui::Context, state: &PanelDockState) -> Vec<WindowGeometry> {
    // Read outside `Context::input`: reading the context again from inside that
    // closure deadlocks (`egui-0.35.0/src/context.rs:915-925`).
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
            // the window manager speaks (`egui-winit-0.35.0/src/lib.rs:51-55`).
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
enum CarriedGesture {
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
fn carried_gesture(
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
fn paint_cross_window_feedback(
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
            TabLanding::HeaderStrip { panel, index } => {
                if let Some(target) = targets.iter().find(|target| target.panel == panel) {
                    // Left edge of the header the tab would push aside; past the
                    // last one, the right edge of that one. Same rule the widget
                    // uses for its own strip (`panel.rs::tab_drop_zone`).
                    let x = target.header_rects.get(index).map_or_else(
                        || {
                            target
                                .header_rects
                                .last()
                                .map_or(target.header_strip.left(), Rect::right)
                        },
                        Rect::left,
                    );
                    drag::paint_insertion_marker(ctx, target.header_strip, x);
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

/// Draws one immediate child viewport per sub-window and, when a program tab of
/// this dock is drawing, the panels that tab assigns to each of them.
///
/// `layout_key` is `None` on a frame where no program tab of this dock draws
/// (`PanelDockState::show_idle_sub_windows`): the windows are then shown EMPTY
/// rather than closed, which is requirement 11 — a sub-window follows the
/// program's tabs and stays open and grey when the active one has nothing for it.
///
/// Immediate, never deferred: the deferred form needs `Fn + Send + Sync +
/// 'static` (`egui-0.35.0/src/context.rs:3960`), which the caller's tab bodies —
/// borrowing the typing tab's hundred-field states — cannot satisfy. The price is
/// that parent and child repaint together; that is the accepted cost of the
/// feature and must not be "optimised" away.
fn show_sub_windows<'frame, C>(
    ctx: &egui::Context,
    state: &mut PanelDockState,
    frame: FrameContext<'_>,
    decls: &BTreeMap<TabId, TabMeta>,
    entries: &mut BTreeMap<TabId, TabEntry<'frame, C>>,
    cx: &mut C,
    output: &mut PanelDockOutput,
) -> HostOutcome {
    let mut outcome = HostOutcome::default();
    if state.sub_windows.is_empty() {
        return outcome;
    }
    // Backends without multi-window support (the web build) embed every extra
    // viewport in the main one and report `ViewportClass::EmbeddedWindow`
    // (`egui-0.35.0/src/context.rs:4008-4011`). The panels are still reachable
    // there, but they are not OS windows, and saying so once beats letting a
    // user wonder why nothing detached.
    if ctx.embed_viewports() && !state.warned_embedded_viewports {
        state.warned_embedded_viewports = true;
        runtime_log::log_warn(
            "[panel_dock] this backend cannot open separate OS windows; the dock's sub-windows \
             are drawn as windows embedded in the main one",
        );
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
                if let Some(layout_key) = frame.layout_key {
                    outcome.merge(draw_host(
                        &child,
                        state,
                        HostFrame {
                            host: node.host(),
                            area,
                            layout_key,
                            gesture_in_flight: frame.gesture_in_flight,
                        },
                        decls,
                        entries,
                        cx,
                        output,
                    ));
                }
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
fn observe_sub_window_geometry(
    ctx: &egui::Context,
    state: &mut PanelDockState,
    node: SubWindowNode,
) {
    let (outer, inner) = ctx.input(|input| {
        let viewport = input.viewport();
        (viewport.outer_rect, viewport.inner_rect)
    });
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
fn apply_frame_detaches(
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
fn apply_menu_move(
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
fn move_into_existing_host(
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
fn menu_tab_landing(
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
fn centered_slot_in_host(
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
fn step_off_occupied(layout: &DockLayout, host: HostId, pos: Pos2) -> Pos2 {
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
fn free_slot_in_host(layout: &DockLayout, host: HostId) -> Pos2 {
    let occupied = layout.panels_in_host(host).count();
    let step = f32::from(u16::try_from(occupied).unwrap_or(u16::MAX));
    let offset = DOCK_GAP + step * AUTO_PANEL_CASCADE_STEP;
    Pos2::new(offset, offset)
}

/// Raises the window a menu move sent something into, so the user sees where it
/// went.
///
/// `ViewportCommand::Focus` is sent TO the destination viewport
/// (`egui-0.35.0/src/context.rs:3921`, `viewport.rs:1170`) and the window manager
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
fn gesture_release_point(
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

/// Applies a tab drop once the frame knows which window it landed in.
///
/// Three branches, and they are the whole model: it landed in another of our
/// windows (the tab moves there, into a header strip or onto bare dock area), it
/// landed on the bare desktop (a window of its own opens where it was released),
/// or it landed on something inside a window that takes no tabs — a panel's body,
/// or that window's own toolbar — which cancels the move, exactly as the
/// in-window rules do.
fn apply_addressed_tab_drop(
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
fn apply_tab_landing(
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
fn apply_addressed_panel_drop(
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
fn detach_tab_into_sub_window(
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
fn detach_panel_into_sub_window(
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
fn resolve_window_placement(state: &mut PanelDockState, place_at: Option<Pos2>) -> Option<Pos2> {
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
enum PanelMutation {
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
fn apply_mutations(layout: &mut DockLayout, mutations: &[PanelMutation]) {
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

/// What the panel-move gesture is doing this frame.
#[derive(Copy, Clone, Debug, PartialEq)]
enum DragPhase {
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
/// `egui-0.35.0/src/input_state/mod.rs:1200-1210`): the session is latched as
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
fn advance_panel_drag(
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
            detach: window::detach_trigger(DragEndContext {
                pointer_left_window: true,
                release_pos: None,
                area,
            }),
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
        let detach = if owns_pointer {
            window::detach_trigger(DragEndContext {
                pointer_left_window: false,
                release_pos: Some(pointer),
                area,
            })
        } else {
            Some(DetachTrigger::PointerLeftWindow)
        };
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
fn begin_panel_drag(
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
fn snap_candidate(
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
fn apply_panel_drop(
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
fn apply_tab_drop(layout: &mut DockLayout, target: PanelId, tab: TabId, index: usize) -> bool {
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
fn write_back_positions(layout: &mut DockLayout, solved: &SolvedLayout, origin: Pos2) {
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

/// Where the DEFAULT arrangement wants a panel that the live layout has to
/// create, expressed in the live layout's own ids.
#[derive(Copy, Clone, Debug, PartialEq)]
struct DefaultPlacement {
    anchor: PanelAnchor,
    pos: Pos2,
    size_override: Option<Vec2>,
}

/// Tab naming the panel the DEFAULT hangs `tab`'s own panel off, or `None` when
/// the default gives it no `Panel` anchor — or does not know the tab at all.
///
/// A `PanelId` of the default layout means nothing outside it, so a default
/// anchor's target is named by the FIRST tab that panel holds. This is the single
/// place that rule lives: [`default_placement`] resolves the anchor with it and
/// [`seeding_order`] decides what has to exist first with it, and the two would
/// disagree the moment they read the default differently.
fn default_anchor_tab(default: &DockLayout, tab: TabId) -> Option<TabId> {
    let source = default.panel_of_tab(tab).and_then(|id| default.panel(id))?;
    match source.anchor {
        PanelAnchor::Free | PanelAnchor::ViewportEdge { .. } => None,
        PanelAnchor::Panel { target, .. } => default
            .panel(target)
            .and_then(|panel| panel.tabs.first().copied()),
    }
}

/// `true` when two anchors name the same SLOT, i.e. would place two panels of one
/// host on exactly the same rect.
///
/// `Free` is never a slot: a free panel is placed by its own `pos`, which two
/// free panels do not share by being free. The fractions are compared with
/// [`SLOT_FRACTION_EPSILON`] because they are derived values, not literals a
/// caller typed.
fn same_slot(left: PanelAnchor, right: PanelAnchor) -> bool {
    match (left, right) {
        (
            PanelAnchor::Panel {
                target: left_target,
                edge: left_edge,
                align: left_align,
            },
            PanelAnchor::Panel {
                target: right_target,
                edge: right_edge,
                align: right_align,
            },
        ) => {
            left_target == right_target
                && left_edge == right_edge
                && (left_align - right_align).abs() < SLOT_FRACTION_EPSILON
        }
        (
            PanelAnchor::ViewportEdge {
                edge: left_edge,
                along: left_along,
            },
            PanelAnchor::ViewportEdge {
                edge: right_edge,
                along: right_along,
            },
        ) => left_edge == right_edge && (left_along - right_along).abs() < SLOT_FRACTION_EPSILON,
        // Listed per LEFT variant rather than as a catch-all, so a new
        // `PanelAnchor` variant fails to compile here instead of silently
        // answering "different slot".
        (PanelAnchor::Free, _)
        | (PanelAnchor::Panel { .. }, _)
        | (PanelAnchor::ViewportEdge { .. }, _) => false,
    }
}

/// Anchor a panel being SEEDED into `layout` may actually take, given that the
/// slot its default names may already be occupied.
///
/// This closes for the seeding path the defect [`drag::resolve_slot`] closes for
/// the gestures: the solver is a total function and lays two panels sharing an
/// anchor on exactly the same rect, the later one burying the earlier one — and a
/// buried panel can no longer be reached, not even to drag it out again. The
/// seeding path reaches that state by the most ordinary route there is: a new
/// build adds its tab to an EXISTING default panel, the tab is new to the user's
/// restored layout while the panel's older tabs are not, and the panel seeded for
/// it inherits that panel's whole anchor and pinned size.
///
/// So the newcomer QUEUES, exactly as a dropped panel does: while a live panel of
/// `host` already holds this anchor, the newcomer is re-anchored to THAT panel,
/// on the side the queue grows along ([`drag::queue_edge`]), flush with it.
///
/// Occupancy is decided by comparing ANCHORS ([`same_slot`]) and not — as
/// `resolve_slot` does — by comparing rects: the geometric test needs the frame's
/// solved rects, and this runs before the solve, on panels that have never been
/// laid out at all. Two anchors that merely happen to overlap are therefore left
/// alone; the exact coincidence the seeding produces is what has to be caught.
///
/// A `Free` anchor is returned unchanged: a free panel is placed by its `pos`,
/// and the positional rule for it is [`step_off_occupied`], applied by the caller.
///
/// The walk never revisits an occupant and is bounded by the panel count, so it
/// terminates on every layout. The result is a proposal — `insert_panel` remains
/// the authority that may refuse it.
fn free_anchor_slot(layout: &DockLayout, host: HostId, anchor: PanelAnchor) -> PanelAnchor {
    let mut anchor = anchor;
    let mut visited: Vec<PanelId> = Vec::new();
    for _ in 0..layout.panels().len() {
        let occupant = layout
            .panels_in_host(host)
            .find(|node| !visited.contains(&node.id) && same_slot(node.anchor, anchor))
            .map(|node| node.id);
        let Some(occupant) = occupant else {
            break;
        };
        visited.push(occupant);
        anchor = PanelAnchor::Panel {
            target: occupant,
            edge: drag::queue_edge(anchor),
            // Flush with the panel it queues behind, as the drag path does: a
            // column of docked panels reads as a column only if its members share
            // a side.
            align: 0.0,
        };
    }
    anchor
}

/// The declared tabs `layout` does not own yet, in the order they have to be
/// seeded in: a tab whose default anchor targets another tab that is ALSO being
/// created comes after that one.
///
/// Declaration order is the caller's business and says nothing about the shape of
/// the default arrangement. Seeding in it directly made the result depend on it:
/// [`default_placement`] resolves a default anchor against the LIVE layout, so a
/// tab whose default hangs it off a panel whose first tab is created LATER finds
/// no target yet and falls back to the cascade — the arrangement an existing user
/// receives after an update would then hinge on the order two `dock.tab(..)` calls
/// happen to be written in.
///
/// A dependency that is not created here — already in the layout, or not declared
/// at all — ends the chain: it is resolvable, or unresolvable, right away. A cycle
/// among the default's anchors cannot arise (`DockLayout` refuses one), and is
/// broken here rather than trusted.
fn seeding_order(layout: &DockLayout, order: &[TabId], default: Option<&DockLayout>) -> Vec<TabId> {
    let mut missing: Vec<TabId> = Vec::new();
    for tab in order {
        if layout.panel_of_tab(*tab).is_none() && !missing.contains(tab) {
            missing.push(*tab);
        }
    }
    let Some(default) = default else {
        return missing;
    };
    let mut seeded: Vec<TabId> = Vec::with_capacity(missing.len());
    for tab in &missing {
        // Collect what this tab has to wait for by walking UP the default's
        // anchor chain, stopping at the first tab that is not created here.
        let mut chain: Vec<TabId> = Vec::new();
        let mut current = *tab;
        while !seeded.contains(&current) && !chain.contains(&current) {
            chain.push(current);
            match default_anchor_tab(default, current) {
                Some(next) if missing.contains(&next) => current = next,
                Some(_) | None => break,
            }
        }
        // The chain was collected leaf-first, and its root is what must exist
        // before anything hanging off it can resolve.
        seeded.extend(chain.iter().rev().copied());
    }
    seeded
}

/// Translates the place `default` gives `tab` into `layout`'s own ids, or `None`
/// when the default cannot answer.
///
/// Two things must hold for the answer to be usable: the default has to hold the
/// tab in `host` at all, and its anchor has to mean something HERE. A panel id of
/// the default layout is meaningless in a restored one — the two were built
/// independently — so a `Panel` anchor is re-addressed by the TAB its target
/// holds: the live panel showing that tab is the live target. A default whose
/// target tab is not in the restored layout (or lives in another window) has no
/// resolvable anchor, and the caller falls back to the cascade rather than
/// inventing one.
fn default_placement(
    default: &DockLayout,
    layout: &DockLayout,
    host: HostId,
    tab: TabId,
) -> Option<DefaultPlacement> {
    let source = default
        .panel_of_tab(tab)
        .and_then(|id| default.panel(id))
        .filter(|panel| panel.host == host)?;
    let anchor = match source.anchor {
        PanelAnchor::Free => PanelAnchor::Free,
        PanelAnchor::ViewportEdge { edge, along } => PanelAnchor::ViewportEdge { edge, along },
        PanelAnchor::Panel { edge, align, .. } => {
            // The default's target panel is identified by the first tab it holds
            // ([`default_anchor_tab`], the single place that rule lives): that is
            // the stable name a layout keeps across rebuilds, while the `PanelId`
            // is an index into the layout that produced it.
            let target_tab = default_anchor_tab(default, tab)?;
            let resolved = layout.panel_of_tab(target_tab)?;
            if layout.panel(resolved).is_none_or(|panel| panel.host != host) {
                return None;
            }
            PanelAnchor::Panel {
                target: resolved,
                edge,
                align,
            }
        }
    };
    Some(DefaultPlacement {
        anchor,
        pos: source.pos,
        size_override: source.size_override,
    })
}

/// Adds a panel for every declared tab the layout does not own yet.
///
/// **Rule:** a tab declared for the first time gets its OWN new free-floating
/// panel. It is deliberately never appended to an existing panel: merging
/// unrelated tabs into one panel behind the caller's back is not recoverable by
/// the user without dragging, while a panel that stands alone can always be
/// docked onto another one.
///
/// **Where that panel goes** is asked of the program tab's DEFAULT arrangement
/// first (`default`, the layout [`PanelDockState::ensure_default_layout`] was
/// given — pass `None` when the key has no builder). A user with a stored layout
/// keeps it, so the default builder never runs for them, and a tab added by a new
/// build reaches this function for every existing user at once: cascading it into
/// the middle of the canvas puts a brand-new panel on top of the user's own
/// arrangement, while the default builder already says where that tab belongs.
/// The default's anchor, position and pinned size are reused when they can be
/// expressed in this layout (see [`default_placement`]); otherwise — the default
/// does not know the tab, or its anchor targets a panel that is not here — the
/// panel falls back to a free position cascaded by [`AUTO_PANEL_CASCADE_STEP`]
/// from the layout's origin, so it does not land exactly on an existing one.
///
/// **A seeded panel never buries a live one.** The default answers for the whole
/// default panel while the seeded panel holds the new tab ALONE, so a tab added
/// to an existing default panel would otherwise arrive on exactly the rect the
/// live panel holding that panel's older tabs occupies. An anchored placement is
/// therefore queued behind whoever already holds that slot
/// ([`free_anchor_slot`]) and a free one steps off whatever stands at its
/// position ([`step_off_occupied`]).
///
/// The tabs are seeded in [`seeding_order`], not in declaration order: a default
/// anchor is resolved against the LIVE layout, so a tab it hangs off another
/// newly declared tab has to be created after that one.
///
/// Returns `true` when the layout changed.
fn ensure_declared_tabs(
    layout: &mut DockLayout,
    host: HostId,
    order: &[TabId],
    default: Option<&DockLayout>,
) -> bool {
    let mut changed = false;
    // Every entry is a tab this layout does not own, listed once.
    for tab in seeding_order(layout, order, default) {
        let id = match layout.next_panel_id() {
            Ok(id) => id,
            Err(error) => {
                runtime_log::log_warn(format!(
                    "[panel_dock] cannot create a panel for the newly declared tab `{}`: {error}",
                    tab.as_str()
                ));
                continue;
            }
        };
        let mut node = match PanelNode::new(id, host, vec![tab]) {
            Ok(node) => node,
            Err(error) => {
                runtime_log::log_warn(format!(
                    "[panel_dock] cannot build a panel for the newly declared tab `{}`: {error}",
                    tab.as_str()
                ));
                continue;
            }
        };
        match default.and_then(|default| default_placement(default, layout, host, tab)) {
            Some(placement) => {
                match placement.anchor {
                    // Placed by its own position, so the burial test is the
                    // positional one a menu move already uses.
                    PanelAnchor::Free => {
                        node.anchor = PanelAnchor::Free;
                        node.pos = step_off_occupied(layout, host, placement.pos);
                    }
                    anchor @ (PanelAnchor::Panel { .. } | PanelAnchor::ViewportEdge { .. }) => {
                        node.anchor = free_anchor_slot(layout, host, anchor);
                        // Kept as the cache the model falls back to whenever the
                        // anchor stops resolving; the solve overwrites it with the
                        // rect this panel is actually laid out at.
                        node.pos = placement.pos;
                    }
                }
                node.size_override = placement.size_override;
            }
            None => {
                let step = f32::from(u16::try_from(layout.panels().len()).unwrap_or(u16::MAX));
                let offset = DOCK_GAP + step * AUTO_PANEL_CASCADE_STEP;
                node.pos = Pos2::new(offset, offset);
            }
        }
        match layout.insert_panel(node) {
            Ok(()) => changed = true,
            Err(error) => runtime_log::log_warn(format!(
                "[panel_dock] cannot insert a panel for the newly declared tab `{}`: {error}",
                tab.as_str()
            )),
        }
    }
    changed
}

/// Builds the layout the SOLVER sees this frame: every panel that has nothing to
/// draw is dropped, and its dependants inherit its own anchor for the duration of
/// the frame.
///
/// The stored layout is never touched — visibility is a per-frame fact, not a
/// user action, and a panel that comes back must find its place unchanged. The
/// re-anchoring falls out of [`DockLayout::remove_panel`], which already hands a
/// removed panel's anchor to whatever hung off it, so a chain closes over the
/// hole instead of leaving a gap where nothing is drawn: hiding the «Превью
/// текста» tab in edit mode moves «Действия/Слои» up into its place.
///
/// Removal happens in ascending panel order; because each removal re-anchors the
/// dependants, dropping several panels of one chain is transitively correct.
fn frame_layout(layout: &DockLayout, plan: &FramePlan) -> DockLayout {
    let mut effective = layout.clone();
    for panel in &plan.panels {
        if panel.active_tab.is_some() {
            continue;
        }
        if let Err(error) = effective.remove_panel(panel.id) {
            // Unreachable: `plan` is built from this very layout. Reported
            // rather than ignored, and the panel simply stays in the solve.
            runtime_log::log_warn(format!(
                "[panel_dock] cannot exclude the empty panel {} from this frame: {error}",
                panel.id
            ));
        }
    }
    effective
}

/// What one panel does this frame, decided before anything is drawn.
#[derive(Clone, Debug, PartialEq)]
struct PanelPlan {
    id: PanelId,
    collapsed: bool,
    /// Outer size the solver was given for this panel, with the same precedence
    /// the solver applies (`size_override`, else the entry in `FramePlan::desired`,
    /// else [`DEFAULT_PANEL_SIZE`]). It is the assumption the frame was laid out
    /// on, and the WIDTH the drawn tab is measured at.
    assumed_size: Vec2,
    /// Size the ACTIVE tab contributed to `FramePlan::desired`, or `None` when it
    /// contributed nothing (never measured and nothing declared).
    ///
    /// This — not [`PanelPlan::assumed_size`] — is what the drawn measurement is
    /// compared against, because the panel's request is the MAXIMUM over its tabs
    /// and a tab smaller than its panel would otherwise report a difference on
    /// every single frame and repaint forever.
    active_request: Option<Vec2>,
    /// Tabs of this panel that were declared this frame AND are visible, in the
    /// panel's own tab order. These are the headers the panel shows.
    visible_tabs: Vec<TabId>,
    /// Tab whose body is drawn, or `None` when the panel has nothing to show.
    active_tab: Option<TabId>,
    /// `true` when the tab this panel actually DRAWS asked for the
    /// transparent-until-hover mode.
    ///
    /// The question is answered here rather than in the drawing loop so it stays
    /// testable without a GPU, and it is asked of the ACTIVE tab alone: the mode
    /// describes what is on screen, so a hidden — or merely inactive —
    /// transparent tab must not make the panel showing another tab disappear.
    transparent: bool,
}

/// The whole frame's decisions plus the size maps handed to the solver.
#[derive(Clone, Debug, Default, PartialEq)]
struct FramePlan {
    panels: Vec<PanelPlan>,
    desired: PanelSizes,
    mins: PanelSizes,
}

/// Decides, without drawing anything, what each panel of `host` shows this frame
/// and which sizes the solver is given for it.
///
/// Rules:
/// * a panel's headers are its own tabs, filtered to those declared this frame
///   and visible — an undeclared tab (another program tab's, or one the caller
///   skipped) silently keeps its slot;
/// * the drawn tab is the panel's `active_tab` when that one is showable, else
///   the first showable tab, else nothing;
/// * **a panel is as big as its LARGEST tab**: its requested size is the
///   component-wise maximum over every tab it SHOWS this frame, not the size of
///   the one that happens to be active, so switching tabs never resizes the
///   panel and a smaller tab is stretched into it. The same maximum is taken over
///   the declared minimums, because the panel has to satisfy all of them at once;
/// * a tab's own size is its last measurement, else the declared `initial_size`,
///   else the declared `min_size`. A tab that has NEVER been drawn and declares
///   neither contributes nothing to the maximum — there is nothing honest to
///   contribute — and joins it with its real size on the first frame it is shown,
///   which costs the one extra frame every first measurement costs
///   ([`PanelPlan::active_request`] drives that repaint). A panel where no tab
///   contributes anything is left out of `desired` entirely and the solver
///   applies [`DEFAULT_PANEL_SIZE`];
/// * a panel with nothing to draw still reports the sizes of its stored
///   `active_tab`, which costs nothing because [`frame_layout`] takes that panel
///   out of the solve altogether;
/// * the panel is drawn transparent-until-hover exactly when the tab it DRAWS
///   declared that mode ([`PanelPlan::transparent`]) — a declaration of the
///   caller's, so it is answered here rather than in the drawing loop.
fn plan_frame(
    layout: &DockLayout,
    host: HostId,
    decls: &BTreeMap<TabId, TabMeta>,
    measured: &HashMap<TabId, Vec2>,
) -> FramePlan {
    let mut nodes: Vec<&PanelNode> = layout.panels_in_host(host).collect();
    nodes.sort_by_key(|node| node.id);

    let mut plan = FramePlan::default();
    for node in nodes {
        let visible_tabs: Vec<TabId> = node
            .tabs
            .iter()
            .copied()
            .filter(|tab| decls.get(tab).is_some_and(|meta| meta.visible))
            .collect();
        let active_tab = if visible_tabs.contains(&node.active_tab) {
            Some(node.active_tab)
        } else {
            visible_tabs.first().copied()
        };

        // A panel with nothing showable is not solved at all, so its stored
        // active tab is the only sensible source left for the bookkeeping below.
        let stored_only = [node.active_tab];
        let sources: &[TabId] = if visible_tabs.is_empty() {
            &stored_only
        } else {
            &visible_tabs
        };
        let declared = sources
            .iter()
            .filter_map(|tab| tab_request(*tab, decls, measured))
            .reduce(Vec2::max);
        let min = sources
            .iter()
            .filter_map(|tab| decls.get(tab).and_then(|meta| meta.min_size))
            .reduce(Vec2::max);
        if let Some(size) = declared {
            plan.desired.insert(node.id, size);
        }
        if let Some(min) = min {
            plan.mins.insert(node.id, min);
        }

        let size_source = active_tab.unwrap_or(node.active_tab);
        // Only the DRAWN tab decides the mode: a transparent tab that is hidden,
        // or merely not the active one, must not make the panel showing another
        // tab disappear.
        let transparent = active_tab.is_some_and(|tab| {
            decls
                .get(&tab)
                .is_some_and(|meta| meta.transparent_until_hover)
        });
        plan.panels.push(PanelPlan {
            id: node.id,
            collapsed: node.collapsed,
            assumed_size: node
                .size_override
                .or(declared)
                .unwrap_or(DEFAULT_PANEL_SIZE),
            active_request: tab_request(size_source, decls, measured),
            visible_tabs,
            active_tab,
            transparent,
        });
    }
    plan
}

/// Outer size ONE tab contributes to its panel's request: its last measurement,
/// else the declared `initial_size`, else the declared `min_size`.
///
/// `None` means the tab has never been drawn and the caller declared no size for
/// it, i.e. the dock knows nothing about how big it wants to be.
fn tab_request(
    tab: TabId,
    decls: &BTreeMap<TabId, TabMeta>,
    measured: &HashMap<TabId, Vec2>,
) -> Option<Vec2> {
    let meta = decls.get(&tab).copied().unwrap_or_default();
    measured
        .get(&tab)
        .copied()
        .or(meta.initial_size)
        .or(meta.min_size)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    const TAB_A: TabId = TabId::new("test.a");
    const TAB_B: TabId = TabId::new("test.b");
    const TAB_C: TabId = TabId::new("test.c");

    const AREA: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 800.0));

    fn meta(visible: bool, min: Option<Vec2>, initial: Option<Vec2>) -> TabMeta {
        TabMeta {
            visible,
            transparent_until_hover: false,
            min_size: min,
            initial_size: initial,
        }
    }

    /// A declaration that asks for the transparent-until-hover mode.
    fn transparent_meta(visible: bool) -> TabMeta {
        TabMeta {
            transparent_until_hover: true,
            ..meta(visible, None, None)
        }
    }

    fn decls(entries: &[(TabId, TabMeta)]) -> BTreeMap<TabId, TabMeta> {
        entries.iter().copied().collect()
    }

    fn panel_with(id: u32, tabs: &[TabId]) -> PanelNode {
        PanelNode::new(PanelId::new(id), HostId::MainWindow, tabs.to_vec())
            .expect("test panel must be constructible")
    }

    #[test]
    fn a_new_tab_gets_its_own_panel() {
        let mut layout = DockLayout::new();
        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_A, TAB_B],
            None
        ));
        assert_eq!(layout.panels().len(), 2);
        assert_eq!(layout.panel_of_tab(TAB_A), Some(PanelId::new(0)));
        assert_eq!(layout.panel_of_tab(TAB_B), Some(PanelId::new(1)));
        assert_eq!(layout.validate(), Ok(()));
        // Cascaded, so the second panel is not exactly on top of the first.
        let first = layout.panel(PanelId::new(0)).expect("panel 0").pos;
        let second = layout.panel(PanelId::new(1)).expect("panel 1").pos;
        assert!(second.x > first.x && second.y > first.y);
    }

    #[test]
    fn an_already_placed_tab_is_left_alone() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        assert!(!ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_B, TAB_A],
            None
        ));
        assert_eq!(layout.panels().len(), 1);
        // A tab declared later still joins as its own panel.
        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_A, TAB_C],
            None
        ));
        assert_eq!(layout.panel_of_tab(TAB_C), Some(PanelId::new(1)));
    }

    /// A user with a stored layout never runs the default builder, so a tab a new
    /// build adds reaches `ensure_declared_tabs` for EVERY existing user at once.
    /// It must land where the default puts it, not in the middle of the canvas on
    /// top of their own arrangement.
    #[test]
    fn a_newly_declared_tab_takes_the_place_its_default_layout_gives_it() {
        // The default: TAB_A pinned to the area's left edge, TAB_B hanging off it.
        let mut default = DockLayout::new();
        let mut anchored = panel_with(0, &[TAB_A]);
        anchored.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.25,
        };
        anchored.pos = Pos2::new(11.0, 12.0);
        anchored.size_override = Some(Vec2::new(340.0, 160.0));
        default.insert_panel(anchored).expect("default 0");
        let mut hanging = panel_with(1, &[TAB_B]);
        hanging.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.5,
        };
        default.insert_panel(hanging).expect("default 1");

        // The restored layout: only TAB_C, under a panel id the default never used.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(7, &[TAB_C])).expect("restored");

        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_A, TAB_B, TAB_C],
            Some(&default)
        ));
        let created = layout
            .panel_of_tab(TAB_A)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_A");
        assert_eq!(
            created.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.25,
            }
        );
        assert_eq!(created.pos, Pos2::new(11.0, 12.0));
        assert_eq!(created.size_override, Some(Vec2::new(340.0, 160.0)));

        // The default's PANEL anchor is re-addressed by the tab its target holds:
        // the default's `PanelId::new(0)` means nothing in this layout.
        let dependant = layout
            .panel_of_tab(TAB_B)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_B");
        assert_eq!(
            dependant.anchor,
            PanelAnchor::Panel {
                target: created.id,
                edge: DockEdge::Bottom,
                align: 0.5,
            }
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    /// The other branch: the default has an opinion the restored layout cannot
    /// express, so the panel falls back to the cascade instead of being anchored
    /// to a panel that is not there (or to a panel of the wrong window).
    #[test]
    fn a_default_anchor_that_does_not_resolve_falls_back_to_the_cascade() {
        // The default anchors TAB_A to the panel holding TAB_C…
        let mut default = DockLayout::new();
        default.insert_panel(panel_with(0, &[TAB_C])).expect("default 0");
        let mut anchored = panel_with(1, &[TAB_A]);
        anchored.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Right,
            align: 0.0,
        };
        anchored.pos = Pos2::new(500.0, 400.0);
        default.insert_panel(anchored).expect("default 1");

        // …and the restored layout does not hold TAB_C at all.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(0, &[TAB_B])).expect("restored");

        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_A],
            Some(&default)
        ));
        let created = layout
            .panel_of_tab(TAB_A)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_A");
        assert_eq!(created.anchor, PanelAnchor::Free);
        // The cascade, not the default's own position.
        let offset = DOCK_GAP + AUTO_PANEL_CASCADE_STEP;
        assert_eq!(created.pos, Pos2::new(offset, offset));
        assert_eq!(created.size_override, None);

        // A tab the default does not know at all takes the cascade too.
        const TAB_D: TabId = TabId::new("test.d");
        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_D],
            Some(&default)
        ));
        let unknown = layout
            .panel_of_tab(TAB_D)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_D");
        assert_eq!(unknown.anchor, PanelAnchor::Free);
        let offset = DOCK_GAP + 2.0 * AUTO_PANEL_CASCADE_STEP;
        assert_eq!(unknown.pos, Pos2::new(offset, offset));
    }

    /// Every solved rect of `layout` is disjoint from every other one — the same
    /// property `the_default_dock_layout_places_every_panel_clear_of_the_others`
    /// pins for the pristine typing default, asserted here for a SEEDED panel.
    fn assert_panels_stand_clear(layout: &DockLayout) -> SolvedLayout {
        let solved = solve(
            layout,
            HostId::MainWindow,
            AREA,
            &PanelSizes::new(),
            &PanelSizes::new(),
            PanelChrome::default(),
        );
        let placed: Vec<(PanelId, Rect)> = solved.iter().map(|(id, panel)| (id, panel.rect)).collect();
        for (index, (left_id, left)) in placed.iter().enumerate() {
            for (right_id, right) in placed.iter().skip(index + 1) {
                assert!(
                    !left.intersect(*right).is_positive(),
                    "panels {left_id} and {right_id} overlap: {left:?} vs {right:?}"
                );
            }
        }
        solved
    }

    /// The natural way a new build adds a tab is to put it into an EXISTING
    /// default panel. The seeded panel then inherits that panel's whole anchor and
    /// pinned size while holding the new tab ALONE, and the solver lays two panels
    /// sharing target + edge + align on exactly the same rect: the newcomer has the
    /// highest id, is drawn last, and buries the live panel completely — which
    /// cannot even be dragged out again. So the seeded panel queues behind it.
    #[test]
    fn a_seeded_panel_queues_behind_the_live_panel_already_in_its_slot() {
        let pinned = Some(Vec2::new(300.0, 200.0));
        let edge_anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.0,
        };
        let under_root = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };

        // The new build's default: the added TAB_C shares a panel with TAB_B,
        // docked under the panel holding TAB_A.
        let mut default = DockLayout::new();
        let mut root = panel_with(0, &[TAB_A]);
        root.anchor = edge_anchor;
        root.size_override = pinned;
        default.insert_panel(root).expect("default 0");
        let mut hanging = panel_with(1, &[TAB_B, TAB_C]);
        hanging.anchor = under_root;
        hanging.size_override = pinned;
        default.insert_panel(hanging).expect("default 1");

        // The user's restored layout, written by the build that had no TAB_C.
        let mut layout = DockLayout::new();
        let mut live_root = panel_with(4, &[TAB_A]);
        live_root.anchor = edge_anchor;
        live_root.size_override = pinned;
        layout.insert_panel(live_root).expect("restored root");
        let mut live_hanging = panel_with(5, &[TAB_B]);
        live_hanging.anchor = PanelAnchor::Panel {
            target: PanelId::new(4),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        live_hanging.size_override = pinned;
        layout.insert_panel(live_hanging).expect("restored hanging");

        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_A, TAB_B, TAB_C],
            Some(&default)
        ));
        assert_eq!(layout.validate(), Ok(()));
        let created = layout
            .panel_of_tab(TAB_C)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_C");
        // Queued behind the live occupant of the slot, flush with it — not on it.
        assert_eq!(
            created.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(5),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );

        // The contract is geometric, so it is checked on the solved rects.
        let solved = assert_panels_stand_clear(&layout);
        assert_eq!(solved.len(), 3, "every panel is placed");
        let occupant = solved.get(PanelId::new(5)).expect("the live panel is placed");
        let newcomer = solved.get(created.id).expect("the seeded panel is placed");
        assert_ne!(newcomer.rect, occupant.rect);
        // Reachable: the whole panel — its header strip included — is inside the
        // dock area, so it can be grabbed and dragged elsewhere.
        assert!(
            AREA.contains_rect(newcomer.rect),
            "the seeded panel {:?} left the dock area {AREA:?}",
            newcomer.rect
        );
    }

    /// The same rule for a FREE default placement: a seeded free panel is placed
    /// by its own position, so it steps off a live panel standing there instead of
    /// covering it.
    #[test]
    fn a_seeded_free_panel_steps_off_the_position_it_would_bury() {
        let mut default = DockLayout::new();
        let mut free = panel_with(0, &[TAB_C]);
        free.pos = Pos2::new(120.0, 90.0);
        default.insert_panel(free).expect("default 0");

        let mut layout = DockLayout::new();
        let mut live = panel_with(0, &[TAB_A]);
        live.pos = Pos2::new(120.0, 90.0);
        layout.insert_panel(live).expect("restored");

        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_C],
            Some(&default)
        ));
        let created = layout
            .panel_of_tab(TAB_C)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_C");
        assert_eq!(created.anchor, PanelAnchor::Free);
        assert_eq!(
            created.pos,
            Pos2::new(120.0, 90.0) + Vec2::splat(AUTO_PANEL_CASCADE_STEP)
        );
    }

    /// Seeding must not depend on the order the caller happens to declare its
    /// tabs in. A default anchor is resolved against the LIVE layout, so a tab
    /// hanging off another NEWLY declared one finds no target until that one has
    /// been created — and declaration order is the caller's business.
    #[test]
    fn seeding_does_not_depend_on_the_declaration_order() {
        // The default: TAB_B hangs off the panel holding TAB_A.
        let mut default = DockLayout::new();
        let mut root = panel_with(0, &[TAB_A]);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.25,
        };
        root.size_override = Some(Vec2::new(340.0, 160.0));
        default.insert_panel(root).expect("default 0");
        let mut hanging = panel_with(1, &[TAB_B]);
        hanging.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.5,
        };
        default.insert_panel(hanging).expect("default 1");

        // Both tabs are new, and the dependant is declared FIRST.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(7, &[TAB_C])).expect("restored");
        assert!(ensure_declared_tabs(
            &mut layout,
            HostId::MainWindow,
            &[TAB_B, TAB_A],
            Some(&default)
        ));
        assert_eq!(layout.validate(), Ok(()));

        let root = layout
            .panel_of_tab(TAB_A)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_A");
        assert_eq!(
            root.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.25,
            }
        );
        // The dependant found its target although it was declared before it: the
        // anchor is the default's, not the cascade the missing target would give.
        let dependant = layout
            .panel_of_tab(TAB_B)
            .and_then(|id| layout.panel(id))
            .expect("a panel was created for TAB_B");
        assert_eq!(
            dependant.anchor,
            PanelAnchor::Panel {
                target: root.id,
                edge: DockEdge::Bottom,
                align: 0.5,
            }
        );
        // Seeded root-first, whatever the declaration order says.
        assert!(root.id < dependant.id);
        assert_panels_stand_clear(&layout);
    }

    #[test]
    fn plan_shows_only_declared_and_visible_tabs() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B, TAB_C]))
            .expect("insert 0");
        // B is hidden, C is not declared at all.
        let decls = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_B, meta(false, None, None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels.len(), 1);
        assert_eq!(plan.panels[0].visible_tabs, vec![TAB_A]);
        assert_eq!(plan.panels[0].active_tab, Some(TAB_A));
    }

    /// The mode belongs to what is ON SCREEN, so the ACTIVE tab answers for the
    /// panel and its inactive siblings do not.
    #[test]
    fn only_the_drawn_tab_decides_whether_its_panel_is_transparent() {
        let mut layout = DockLayout::new();
        let mut node = panel_with(0, &[TAB_A, TAB_B]);
        node.active_tab = TAB_A;
        layout.insert_panel(node).expect("insert 0");

        // The transparent tab is the drawn one.
        let transparent_active = decls(&[
            (TAB_A, transparent_meta(true)),
            (TAB_B, meta(true, None, None)),
        ]);
        let plan = plan_frame(
            &layout,
            HostId::MainWindow,
            &transparent_active,
            &HashMap::new(),
        );
        assert_eq!(plan.panels[0].active_tab, Some(TAB_A));
        assert!(plan.panels[0].transparent);

        // The same panel with an ORDINARY tab active: a transparent sibling
        // sitting in the strip must not make the drawn one disappear.
        let transparent_sibling = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_B, transparent_meta(true)),
        ]);
        let plan = plan_frame(
            &layout,
            HostId::MainWindow,
            &transparent_sibling,
            &HashMap::new(),
        );
        assert_eq!(plan.panels[0].active_tab, Some(TAB_A));
        assert!(!plan.panels[0].transparent);
    }

    /// A hidden tab is not drawn, so it cannot make its panel transparent —
    /// including when the panel falls back to another tab of its own.
    #[test]
    fn a_hidden_transparent_tab_leaves_its_panel_opaque() {
        let mut layout = DockLayout::new();
        let mut node = panel_with(0, &[TAB_A, TAB_B]);
        node.active_tab = TAB_A;
        layout.insert_panel(node).expect("insert 0");
        let fallback = decls(&[
            (TAB_A, transparent_meta(false)),
            (TAB_B, meta(true, None, None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &fallback, &HashMap::new());
        assert_eq!(plan.panels[0].active_tab, Some(TAB_B));
        assert!(!plan.panels[0].transparent);

        // And a panel with nothing showable at all is not transparent either:
        // it is not drawn, so there is nothing to fade.
        let nothing_shown = decls(&[
            (TAB_A, transparent_meta(false)),
            (TAB_B, meta(false, None, None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &nothing_shown, &HashMap::new());
        assert_eq!(plan.panels[0].active_tab, None);
        assert!(!plan.panels[0].transparent);
    }

    /// The hidden active tab's fallback keeps the mode of the tab that REPLACES
    /// it: the panel shows a transparent tab, so the panel is transparent.
    #[test]
    fn a_transparent_fallback_tab_makes_its_panel_transparent() {
        let mut layout = DockLayout::new();
        let mut node = panel_with(0, &[TAB_A, TAB_B]);
        node.active_tab = TAB_A;
        layout.insert_panel(node).expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(false, None, None)),
            (TAB_B, transparent_meta(true)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels[0].active_tab, Some(TAB_B));
        assert!(plan.panels[0].transparent);
    }

    /// An undeclared tab carries no declaration at all, so nothing about it can
    /// turn the mode on. This is the "another program tab's tab" case.
    #[test]
    fn an_undeclared_tab_never_turns_the_mode_on() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_C]))
            .expect("insert 0");
        let decls = decls(&[(TAB_A, meta(true, None, None))]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels[0].visible_tabs, vec![TAB_A]);
        assert!(!plan.panels[0].transparent);
    }

    #[test]
    fn a_hidden_active_tab_falls_back_to_the_first_visible_one() {
        let mut layout = DockLayout::new();
        let mut node = panel_with(0, &[TAB_A, TAB_B]);
        node.active_tab = TAB_A;
        layout.insert_panel(node).expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(false, None, None)),
            (TAB_B, meta(true, None, None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels[0].active_tab, Some(TAB_B));
        // The model's own active tab is untouched: visibility is not a user
        // choice and must not overwrite one.
        assert_eq!(
            layout.panel(PanelId::new(0)).expect("panel").active_tab,
            TAB_A
        );
    }

    #[test]
    fn a_panel_with_nothing_visible_is_planned_but_not_drawable() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(0, &[TAB_A])).expect("insert");
        let decls = decls(&[(TAB_A, meta(false, None, Some(Vec2::new(300.0, 200.0))))]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels.len(), 1);
        assert!(plan.panels[0].visible_tabs.is_empty());
        assert_eq!(plan.panels[0].active_tab, None);
        // The size is still reported (it costs nothing), but `frame_layout`
        // keeps the panel out of the solve entirely.
        assert_eq!(
            plan.desired.get(PanelId::new(0)),
            Some(Vec2::new(300.0, 200.0))
        );
        assert!(frame_layout(&layout, &plan).panels().is_empty());
    }

    #[test]
    fn a_panel_with_nothing_to_draw_leaves_the_chain_for_this_frame() {
        // 0 (viewport-anchored) <- 1 <- 2. Hiding 1's only tab must let 2 hang
        // off 0 directly, so the chain closes over the hole instead of leaving
        // a gap where nothing is drawn.
        let mut layout = DockLayout::new();
        let mut root = panel_with(0, &[TAB_A]);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Top,
            along: 0.0,
        };
        layout.insert_panel(root).expect("insert 0");
        let mut middle = panel_with(1, &[TAB_B]);
        middle.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(middle).expect("insert 1");
        let mut last = panel_with(2, &[TAB_C]);
        last.anchor = PanelAnchor::Panel {
            target: PanelId::new(1),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(last).expect("insert 2");

        let decls = decls(&[
            (TAB_A, meta(true, None, Some(Vec2::new(300.0, 200.0)))),
            (TAB_B, meta(false, None, Some(Vec2::new(300.0, 200.0)))),
            (TAB_C, meta(true, None, Some(Vec2::new(300.0, 200.0)))),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        let effective = frame_layout(&layout, &plan);
        assert_eq!(effective.panels().len(), 2);
        assert_eq!(effective.panel(PanelId::new(1)), None);
        assert_eq!(
            effective
                .panel(PanelId::new(2))
                .expect("the last panel survives")
                .anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        // The stored layout is untouched: visibility is not a user action.
        assert_eq!(layout.panels().len(), 3);
        assert_eq!(layout.validate(), Ok(()));

        let solved = solve(
            &effective,
            HostId::MainWindow,
            AREA,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        let root = solved.get(PanelId::new(0)).expect("root solved").rect;
        let last = solved.get(PanelId::new(2)).expect("last solved").rect;
        assert!((last.top() - root.bottom() - DOCK_GAP).abs() < 0.01);
        assert!(solved.get(PanelId::new(1)).is_none());
    }

    #[test]
    fn a_hidden_chain_root_hands_its_anchor_to_its_dependant() {
        // The «Превью текста» case: the root is hidden in edit mode and the
        // panel below it must take over its anchor, not float where the root
        // used to end.
        let mut layout = DockLayout::new();
        let mut root = panel_with(0, &[TAB_A]);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.0,
        };
        layout.insert_panel(root).expect("insert 0");
        let mut below = panel_with(1, &[TAB_B]);
        below.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(below).expect("insert 1");

        let decls = decls(&[
            (TAB_A, meta(false, None, Some(Vec2::new(300.0, 200.0)))),
            (TAB_B, meta(true, None, Some(Vec2::new(300.0, 180.0)))),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        let effective = frame_layout(&layout, &plan);
        assert_eq!(
            effective
                .panel(PanelId::new(1))
                .expect("survivor")
                .anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            }
        );

        let solved = solve(
            &effective,
            HostId::MainWindow,
            AREA,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        // The survivor is laid out where the hidden ROOT would have been —
        // flush with the area's left side — instead of one root-height lower.
        let rect = solved.get(PanelId::new(1)).expect("solved").rect;
        assert!((rect.left() - AREA.left() - DOCK_GAP).abs() < 0.01);
        assert!((rect.top() - AREA.top()).abs() < 0.01);
    }

    #[test]
    fn sizes_prefer_the_measurement_then_initial_then_min() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(0, &[TAB_A])).expect("insert 0");
        layout.insert_panel(panel_with(1, &[TAB_B])).expect("insert 1");
        layout.insert_panel(panel_with(2, &[TAB_C])).expect("insert 2");
        let decls = decls(&[
            (
                TAB_A,
                meta(true, Some(Vec2::new(100.0, 100.0)), Some(Vec2::new(300.0, 200.0))),
            ),
            (TAB_B, meta(true, None, Some(Vec2::new(280.0, 190.0)))),
            (TAB_C, meta(true, Some(Vec2::new(220.0, 150.0)), None)),
        ]);
        let mut measured = HashMap::new();
        measured.insert(TAB_A, Vec2::new(310.0, 420.0));

        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured);
        assert_eq!(
            plan.desired.get(PanelId::new(0)),
            Some(Vec2::new(310.0, 420.0))
        );
        assert_eq!(
            plan.desired.get(PanelId::new(1)),
            Some(Vec2::new(280.0, 190.0))
        );
        assert_eq!(
            plan.desired.get(PanelId::new(2)),
            Some(Vec2::new(220.0, 150.0))
        );
        assert_eq!(
            plan.mins.get(PanelId::new(0)),
            Some(Vec2::new(100.0, 100.0))
        );
        assert_eq!(plan.mins.get(PanelId::new(1)), None);
    }

    /// THE PANEL-SIZE CONTRACT. A panel is as big as its LARGEST tab, on each
    /// axis independently, so switching tabs never resizes it. Sizing it from the
    /// active tab alone made the panel jump on every tab click.
    #[test]
    fn a_panel_is_as_big_as_its_largest_tab() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(true, Some(Vec2::new(200.0, 120.0)), None)),
            (TAB_B, meta(true, Some(Vec2::new(180.0, 300.0)), None)),
        ]);
        let mut measured = HashMap::new();
        // TAB_A is the active one and the SHORTER one; the panel must still be
        // laid out for TAB_B's height, and take its width from TAB_A.
        measured.insert(TAB_A, Vec2::new(420.0, 150.0));
        measured.insert(TAB_B, Vec2::new(300.0, 460.0));

        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured);
        assert_eq!(
            plan.desired.get(PanelId::new(0)),
            Some(Vec2::new(420.0, 460.0))
        );
        assert_eq!(plan.panels[0].assumed_size, Vec2::new(420.0, 460.0));
        // …while the repaint check still watches the tab that is actually drawn.
        assert_eq!(plan.panels[0].active_request, Some(Vec2::new(420.0, 150.0)));
    }

    /// A tab the user has never opened has no measurement, so it joins the
    /// maximum with what the caller DECLARED for it and nothing else — and a tab
    /// that declared nothing either simply does not participate, instead of
    /// dragging the panel down to a made-up size.
    #[test]
    fn an_unmeasured_tab_contributes_only_what_was_declared() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout
            .insert_panel(panel_with(1, &[TAB_C]))
            .expect("insert 1");
        let decls = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_B, meta(true, Some(Vec2::new(180.0, 300.0)), None)),
            (TAB_C, meta(true, None, None)),
        ]);
        let mut measured = HashMap::new();
        measured.insert(TAB_A, Vec2::new(240.0, 200.0));

        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured);
        // Panel 0: a measured tab and a declared-but-never-drawn one.
        assert_eq!(
            plan.desired.get(PanelId::new(0)),
            Some(Vec2::new(240.0, 300.0))
        );
        // Panel 1: nothing is known about its tab at all, so the solver's default
        // applies rather than an invented size.
        assert_eq!(plan.desired.get(PanelId::new(1)), None);
        assert_eq!(plan.panels[1].assumed_size, DEFAULT_PANEL_SIZE);
        assert_eq!(plan.panels[1].active_request, None);
    }

    /// The panel has to satisfy every minimum it carries at once: the grip and the
    /// shrink floor both take the maximum, not the active tab's own.
    #[test]
    fn the_panel_minimum_is_the_maximum_over_its_tabs() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(true, Some(Vec2::new(340.0, 100.0)), None)),
            (TAB_B, meta(true, Some(Vec2::new(120.0, 260.0)), None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(
            plan.mins.get(PanelId::new(0)),
            Some(Vec2::new(340.0, 260.0))
        );
    }

    /// A hidden tab is not shown and must not size the panel: the maximum runs
    /// over the tabs whose headers the strip actually carries.
    #[test]
    fn a_hidden_tab_does_not_inflate_its_panel() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(true, Some(Vec2::new(200.0, 120.0)), None)),
            (TAB_B, meta(false, Some(Vec2::new(600.0, 700.0)), None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(
            plan.desired.get(PanelId::new(0)),
            Some(Vec2::new(200.0, 120.0))
        );
    }

    /// The plan must be a FIXED POINT of the measurement it produces: feeding the
    /// panel's stored width back (which is what the driver writes for every drawn
    /// tab) may not grow it, or the panel would creep wider every frame.
    #[test]
    fn re_planning_with_the_stored_measurement_reproduces_the_same_request() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_B, meta(true, None, None)),
        ]);
        let mut measured = HashMap::new();
        measured.insert(TAB_A, Vec2::new(420.0, 150.0));
        measured.insert(TAB_B, Vec2::new(300.0, 460.0));

        let first = plan_frame(&layout, HostId::MainWindow, &decls, &measured);
        // The driver stores the PANEL's width against the drawn tab, and the
        // content height it measured; both tabs eventually hold that width.
        let assumed = first.panels[0].assumed_size;
        measured.insert(TAB_A, Vec2::new(assumed.x, 150.0));
        measured.insert(TAB_B, Vec2::new(assumed.x, 460.0));
        let second = plan_frame(&layout, HostId::MainWindow, &decls, &measured);
        assert_eq!(first.desired, second.desired);
        assert_eq!(first.panels, second.panels);
    }

    #[test]
    fn a_manual_resize_pins_the_panel_and_is_reported_in_id_order() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel_with(2, &[TAB_C])).expect("insert 2");
        let mut pinned = panel_with(0, &[TAB_A]);
        pinned.size_override = Some(Vec2::new(420.0, 260.0));
        layout.insert_panel(pinned).expect("insert 0");
        let decls = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_C, meta(true, None, None)),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        // Insertion order was 2 then 0; the plan is always ascending by id, which
        // is what makes drawing order deterministic.
        let ids: Vec<PanelId> = plan.panels.iter().map(|panel| panel.id).collect();
        assert_eq!(ids, vec![PanelId::new(0), PanelId::new(2)]);
        // The pinned panel is laid out at the size the user dragged it to; the
        // other one, which neither measured nor declared anything, falls back to
        // the solver's default.
        assert_eq!(plan.panels[0].assumed_size, Vec2::new(420.0, 260.0));
        assert_eq!(plan.panels[1].assumed_size, DEFAULT_PANEL_SIZE);
    }

    #[test]
    fn the_plan_feeds_the_solver_and_keeps_the_declared_geometry() {
        let mut layout = DockLayout::new();
        let mut node = panel_with(0, &[TAB_A]);
        node.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Top,
            along: 0.0,
        };
        layout.insert_panel(node).expect("insert 0");
        let decls = decls(&[(TAB_A, meta(true, None, Some(Vec2::new(300.0, 240.0))))]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        let panel = solved.get(PanelId::new(0)).expect("solved");
        // The size the plan derived from the declaration reaches the solver, and
        // the anchor the model carries decides where that size is placed.
        assert_eq!(panel.rect.size(), Vec2::new(300.0, 240.0));
        assert!((panel.rect.top() - AREA.top() - DOCK_GAP).abs() < 0.01);
        assert!((panel.rect.left() - AREA.left()).abs() < 0.01);
        assert!((panel.body_max_height - (240.0 - COLLAPSED_PANEL_HEIGHT)).abs() < 0.01);
    }

    #[test]
    fn mutations_are_applied_and_invariants_survive_them() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        apply_mutations(
            &mut layout,
            &[
                PanelMutation::Activate(PanelId::new(0), TAB_B),
                PanelMutation::ToggleCollapsed(PanelId::new(0)),
                PanelMutation::Resize(PanelId::new(0), Vec2::new(420.0, 260.0)),
                // A tab the panel does not own must be ignored, not stored.
                PanelMutation::Activate(PanelId::new(0), TAB_C),
                // An unknown panel must not panic.
                PanelMutation::ToggleCollapsed(PanelId::new(9)),
            ],
        );
        let node = layout.panel(PanelId::new(0)).expect("panel");
        assert_eq!(node.active_tab, TAB_B);
        assert!(node.collapsed);
        assert_eq!(node.size_override, Some(Vec2::new(420.0, 260.0)));
        assert_eq!(layout.validate(), Ok(()));
    }

    /// How often [`counted_default_layout`] ran. A default builder is a plain
    /// `fn` — the dock keeps it for `reset_layout` long after the call — so it
    /// cannot capture a local counter. Used by exactly one test.
    static DEFAULT_BUILDS: AtomicU32 = AtomicU32::new(0);

    /// Default builder of the tests: one panel holding `TAB_A`.
    fn counted_default_layout() -> DockLayout {
        DEFAULT_BUILDS.fetch_add(1, Ordering::Relaxed);
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A]))
            .expect("insert 0");
        layout
    }

    /// The same default as [`counted_default_layout`], without the counter, for
    /// the tests that only need a layout to exist.
    fn single_panel_default_layout() -> DockLayout {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A]))
            .expect("insert 0");
        layout
    }

    /// A builder whose result breaks an invariant, to exercise the guard.
    fn broken_default_layout() -> DockLayout {
        let mut broken = panel_with(0, &[TAB_A]);
        broken.active_tab = TAB_B;
        DockLayout::from_panels_unchecked(vec![broken])
    }

    #[test]
    fn ensure_default_layout_runs_once_and_rejects_a_broken_layout() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", counted_default_layout);
        state.ensure_default_layout("typing", counted_default_layout);
        assert_eq!(DEFAULT_BUILDS.load(Ordering::Relaxed), 1);
        assert_eq!(
            state
                .layout("typing")
                .expect("layout")
                .panel_of_tab(TAB_A),
            Some(PanelId::new(0))
        );
        assert!(state.is_dirty());
        state.clear_dirty();
        assert!(!state.is_dirty());

        // A layout that breaks an invariant is replaced by an empty one instead
        // of poisoning the dock. No public mutation can produce one any more —
        // phase 5 will deserialize layouts, which is the case this guard is for,
        // so the test reaches for the unchecked test-only constructor.
        state.ensure_default_layout("broken", broken_default_layout);
        assert!(
            state
                .layout("broken")
                .expect("layout")
                .panels()
                .is_empty()
        );
    }

    #[test]
    fn a_panel_keeps_the_place_it_was_drawn_at_when_its_anchor_disappears() {
        // Frame 1: panel 0 floats at (400, 300) and panel 1 hangs under it.
        // Frame 2 hides 0's only tab, so `frame_layout` drops 0 and 1 inherits
        // its `Free` anchor — which places 1 at ITS OWN `pos`. Unless that `pos`
        // is refreshed from the last solve, it is still the one the panel was
        // created with, and the panel jumps into the corner of the area, on top
        // of the canvas controls.
        let area = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(1000.0, 800.0));
        let mut layout = DockLayout::new();
        let mut root = panel_with(0, &[TAB_A]);
        root.pos = Pos2::new(400.0, 300.0);
        layout.insert_panel(root).expect("insert 0");
        let mut below = panel_with(1, &[TAB_B]);
        below.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(below).expect("insert 1");
        let sizes = [
            (TAB_A, Vec2::new(300.0, 200.0)),
            (TAB_B, Vec2::new(300.0, 180.0)),
        ];

        let shown = decls(&[
            (TAB_A, meta(true, None, Some(sizes[0].1))),
            (TAB_B, meta(true, None, Some(sizes[1].1))),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &shown, &HashMap::new());
        let solved = solve(
            &frame_layout(&layout, &plan),
            HostId::MainWindow,
            area,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        write_back_positions(&mut layout, &solved, area.min);
        let remembered = layout.panel(PanelId::new(1)).expect("panel 1").pos;
        assert_eq!(remembered, Pos2::new(400.0, 300.0 + 200.0 + DOCK_GAP));

        // Frame 2: the root has nothing to draw.
        let hidden = decls(&[
            (TAB_A, meta(false, None, Some(sizes[0].1))),
            (TAB_B, meta(true, None, Some(sizes[1].1))),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &hidden, &HashMap::new());
        let effective = frame_layout(&layout, &plan);
        assert_eq!(
            effective.panel(PanelId::new(1)).expect("survivor").anchor,
            PanelAnchor::Free
        );
        let solved = solve(
            &effective,
            HostId::MainWindow,
            area,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        let rect = solved.get(PanelId::new(1)).expect("solved").rect;
        assert_eq!(rect.min, area.min + remembered.to_vec2());
    }

    #[test]
    fn the_position_write_back_does_not_dirty_the_layout() {
        // Nothing the user did changed here, so persistence must not be woken up
        // every frame by a position the solver derived on its own.
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", single_panel_default_layout);
        state.clear_dirty();
        {
            let layout = state.layouts.get_mut("typing").expect("layout");
            let plan = plan_frame(
                layout,
                HostId::MainWindow,
                &decls(&[(TAB_A, meta(true, None, Some(Vec2::new(300.0, 200.0))))]),
                &HashMap::new(),
            );
            let solved = solve(
                &frame_layout(layout, &plan),
                HostId::MainWindow,
                AREA,
                &plan.desired,
                &plan.mins,
                PanelChrome::default(),
            );
            write_back_positions(layout, &solved, AREA.min);
        }
        assert!(!state.is_dirty());
    }

    #[test]
    fn the_plan_reports_the_size_the_solver_will_actually_use() {
        // The repaint request compares the drawn measurement against this, so it
        // has to follow the solver's own precedence — including the case where
        // nothing at all was declared and the solver falls back to its default.
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A]))
            .expect("insert 0");
        layout
            .insert_panel(panel_with(1, &[TAB_B]))
            .expect("insert 1");
        let mut pinned = panel_with(2, &[TAB_C]);
        pinned.size_override = Some(Vec2::new(420.0, 260.0));
        layout.insert_panel(pinned).expect("insert 2");

        let decls = decls(&[
            (TAB_A, meta(true, None, None)),
            (TAB_B, meta(true, None, Some(Vec2::new(280.0, 190.0)))),
            (TAB_C, meta(true, None, Some(Vec2::new(300.0, 200.0)))),
        ]);
        let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new());
        assert_eq!(plan.panels[0].assumed_size, DEFAULT_PANEL_SIZE);
        assert_eq!(plan.panels[1].assumed_size, Vec2::new(280.0, 190.0));
        assert_eq!(plan.panels[2].assumed_size, Vec2::new(420.0, 260.0));
        // And it is exactly what the solver lays the panels out at.
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &plan.desired,
            &plan.mins,
            PanelChrome::default(),
        );
        for panel in &plan.panels {
            let rect = solved.get(panel.id).expect("solved").rect;
            assert_eq!(rect.size(), panel.assumed_size, "panel {}", panel.id);
        }
    }

    /// A platform that hands the cursor to whatever window it entered — instead
    /// of keeping an implicit grab on the one the press started in — leaves the
    /// source window with no release coordinate at all. The window that DID see
    /// the cursor this frame is then what addresses the drop, and only a reading
    /// from this very frame counts.
    #[test]
    fn a_release_the_source_window_could_not_see_is_addressed_from_another_window() {
        let mut state = PanelDockState::new();
        state.frame_hosts.push(HostRecord {
            host: HostId::MainWindow,
            area: AREA,
            panels: Vec::new(),
            pointer_global: Some(Pos2::new(120.0, 140.0)),
        });
        state.frame_hosts.push(HostRecord {
            host: HostId::SubWindow(0),
            area: AREA,
            panels: Vec::new(),
            pointer_global: None,
        });
        assert_eq!(
            gesture_release_point(&state, HostId::SubWindow(0), None),
            Some(Pos2::new(120.0, 140.0))
        );
        // The source's own reading always wins when it has one.
        assert_eq!(
            gesture_release_point(&state, HostId::SubWindow(0), Some(Pos2::new(900.0, 700.0))),
            Some(Pos2::new(900.0, 700.0))
        );
        // Nobody saw the cursor: nothing is invented, and the drop degrades to a
        // window the compositor places.
        assert_eq!(
            gesture_release_point(&PanelDockState::new(), HostId::MainWindow, None),
            None
        );
        // The source never falls back to its OWN record: that is the reading it
        // already said it does not have.
        let mut alone = PanelDockState::new();
        alone.frame_hosts.push(HostRecord {
            host: HostId::MainWindow,
            area: AREA,
            panels: Vec::new(),
            pointer_global: Some(Pos2::new(10.0, 10.0)),
        });
        assert_eq!(
            gesture_release_point(&alone, HostId::MainWindow, None),
            None
        );
    }

    #[test]
    fn a_dropped_tab_joins_the_receiving_panel_at_the_hovered_index() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout.insert_panel(panel_with(1, &[TAB_C])).expect("insert 1");

        assert!(apply_tab_drop(&mut layout, PanelId::new(0), TAB_C, 1));
        let receiver = layout.panel(PanelId::new(0)).expect("receiver");
        assert_eq!(receiver.tabs, vec![TAB_A, TAB_C, TAB_B]);
        // The dropped tab is what the user wants to look at.
        assert_eq!(receiver.active_tab, TAB_C);
        // Requirement 10: the source panel lost its last tab and is gone.
        assert_eq!(layout.panel(PanelId::new(1)), None);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_tab_dropped_on_bare_dock_area_becomes_a_panel_of_its_own() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let outcome = layout
            .detach_tab(TAB_B, Pos2::new(320.0, 180.0))
            .expect("the panel has another tab to keep");
        assert!(outcome.created);
        assert_eq!(outcome.source_panel, PanelId::new(0));
        let created = layout.panel(outcome.panel).expect("the new panel");
        assert_eq!(created.tabs, vec![TAB_B]);
        assert_eq!(created.anchor, PanelAnchor::Free);
        assert_eq!(created.pos, Pos2::new(320.0, 180.0));
        assert_eq!(
            layout.panel(PanelId::new(0)).expect("source").tabs,
            vec![TAB_A]
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_released_panel_docks_to_the_edge_it_was_dropped_next_to() {
        // Panel 0 is flush with the area's right edge; panel 1 is dropped just
        // under it (4 pt off the exact docking position) and slightly left of the
        // area's own right inset, so the panel's edge is the nearer candidate.
        let mut layout = DockLayout::new();
        let mut anchored = panel_with(0, &[TAB_A]);
        anchored.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        layout.insert_panel(anchored).expect("insert 0");
        let mut dropped = panel_with(1, &[TAB_B]);
        dropped.pos = Pos2::new(
            AREA.width() - DOCK_GAP - 300.0 - 12.0,
            DOCK_GAP + 200.0 + DOCK_GAP - 4.0,
        );
        layout.insert_panel(dropped).expect("insert 1");

        let mut desired = PanelSizes::new();
        desired.insert(PanelId::new(0), Vec2::new(300.0, 200.0));
        desired.insert(PanelId::new(1), Vec2::new(300.0, 150.0));
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &desired,
            &PanelSizes::new(),
            PanelChrome::default(),
        );
        assert!(apply_panel_drop(&mut layout, PanelId::new(1), &solved, AREA));
        assert_eq!(
            layout.panel(PanelId::new(1)).expect("dropped").anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
    }

    #[test]
    fn a_released_panel_with_no_target_nearby_stays_free() {
        let mut layout = DockLayout::new();
        let mut dropped = panel_with(0, &[TAB_A]);
        dropped.pos = Pos2::new(400.0, 300.0);
        layout.insert_panel(dropped).expect("insert 0");
        let mut desired = PanelSizes::new();
        desired.insert(PanelId::new(0), Vec2::new(300.0, 150.0));
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &desired,
            &PanelSizes::new(),
            PanelChrome::default(),
        );
        apply_panel_drop(&mut layout, PanelId::new(0), &solved, AREA);
        assert_eq!(
            layout.panel(PanelId::new(0)).expect("dropped").anchor,
            PanelAnchor::Free
        );
    }

    #[test]
    fn reset_layout_restores_the_declared_default() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", single_panel_default_layout);
        {
            let layout = state.layout_mut("typing").expect("layout");
            layout
                .insert_panel(panel_with(9, &[TAB_C]))
                .expect("a panel the user created");
        }
        state.clear_dirty();
        assert!(state.reset_layout("typing"));
        let layout = state.layout("typing").expect("layout");
        assert_eq!(layout.panels().len(), 1);
        assert_eq!(layout.panel_of_tab(TAB_A), Some(PanelId::new(0)));
        assert_eq!(layout.panel_of_tab(TAB_C), None);
        assert!(state.is_dirty());

        // A key nobody declared a default for keeps whatever it has.
        assert!(!state.reset_layout("unknown"));
    }

    #[test]
    fn measurements_round_trip_through_the_state() {
        let mut state = PanelDockState::new();
        assert_eq!(state.measured_size("typing", TAB_A), None);
        state.remember_measurement("typing", TAB_A, Vec2::new(300.0, 210.0));
        assert_eq!(
            state.measured_size("typing", TAB_A),
            Some(Vec2::new(300.0, 210.0))
        );
        // The same tab drawn by ANOTHER program tab is a different measurement.
        assert_eq!(state.measured_size("cleaning", TAB_A), None);
    }

    /// One `TabId` — the canvas' «Лента» — is declared by three program tabs that
    /// share one dock state. A measurement taken while one of them was drawn must
    /// not size the panel of another, or resizing the ribbon in «Текст» would make
    /// «Клининг»' ribbon (which carries no `size_override`) jump to that width on
    /// the next tab switch.
    #[test]
    fn a_measurement_does_not_leak_into_another_program_tabs_layout() {
        let mut state = PanelDockState::new();
        state.layouts.insert("typing".to_owned(), single_panel_default_layout());
        state.layouts.insert("cleaning".to_owned(), single_panel_default_layout());
        // The user dragged the shared tab wide while «Текст» was drawn.
        state.remember_measurement("typing", TAB_A, Vec2::new(560.0, 480.0));

        let declared = decls(&[(
            TAB_A,
            meta(true, None, Some(Vec2::new(340.0, 160.0))),
        )]);
        let no_measurements = HashMap::new();
        let typing = plan_frame(
            state.layout("typing").expect("typing layout"),
            HostId::MainWindow,
            &declared,
            state.measurements("typing").unwrap_or(&no_measurements),
        );
        assert_eq!(typing.panels[0].assumed_size, Vec2::new(560.0, 480.0));

        // «Клининг» has never drawn it: it starts from the DECLARED initial size.
        let cleaning = plan_frame(
            state.layout("cleaning").expect("cleaning layout"),
            HostId::MainWindow,
            &declared,
            state.measurements("cleaning").unwrap_or(&no_measurements),
        );
        assert_eq!(cleaning.panels[0].assumed_size, Vec2::new(340.0, 160.0));
    }

    /// Extra per-tab state is keyed exactly like a measurement — by program tab
    /// first — and for the same reason: the canvas' «Лента» is ONE `TabId`
    /// declared by three program tabs sharing one dock state, so a `TabId`-only
    /// key would show «Клининг» whatever the user expanded in «Текст».
    #[test]
    fn extra_tab_state_does_not_leak_into_another_program_tabs_layout() {
        let mut state = PanelDockState::new();
        let mut extras = state.take_tab_extras("typing", TAB_A);
        extras.set_flag("params.font", false, true);
        state.put_tab_extras("typing", TAB_A, extras);

        assert_eq!(
            state.tab_extras("typing", TAB_A).map(|bag| bag.flag("params.font", true)),
            Some(false)
        );
        // The same tab drawn by ANOTHER program tab knows nothing about it and
        // falls back to the caller's default.
        assert!(state.tab_extras("cleaning", TAB_A).is_none());
        assert!(state.take_tab_extras("cleaning", TAB_A).is_empty());
        assert!(state.take_tab_extras("cleaning", TAB_A).flag("params.font", true));
    }

    /// `dirty` is persistence's signal that the USER changed something, and the
    /// writer is driven off it: a body that only reads its extras — which is what
    /// every frame of an untouched tab does — must not cause a config write.
    #[test]
    fn only_a_real_change_of_the_extra_tab_state_dirties_the_dock() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", single_panel_default_layout);
        state.clear_dirty();

        // A body that only READS.
        let extras = state.take_tab_extras("typing", TAB_A);
        assert!(extras.flag("params.font", true));
        state.finish_tab_extras("typing", TAB_A, extras);
        assert!(!state.is_dirty());

        // A body that really CHANGES something.
        let mut extras = state.take_tab_extras("typing", TAB_A);
        extras.set_flag("params.font", false, true);
        state.finish_tab_extras("typing", TAB_A, extras);
        assert!(state.is_dirty());
        // …and the bookkeeping flag never reaches the stored value.
        assert_eq!(
            state.tab_extras("typing", TAB_A).map(TabExtras::changed),
            Some(false)
        );
        state.clear_dirty();

        // A body writing back what is already stored — the normal per-frame case.
        let mut extras = state.take_tab_extras("typing", TAB_A);
        extras.set_flag("params.font", false, true);
        state.finish_tab_extras("typing", TAB_A, extras);
        assert!(!state.is_dirty());
    }

    /// What came from the config is not a user change: re-writing it on the first
    /// frame would be one config write per startup.
    #[test]
    fn installing_persisted_extra_tab_state_does_not_dirty_the_dock() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", single_panel_default_layout);
        state.clear_dirty();

        let mut stored = TabExtras::default();
        stored.set_flag("params.font", false, true);
        state.install_persisted_tab_extras(
            [("typing".to_owned(), [(TAB_A, stored)].into_iter().collect())]
                .into_iter()
                .collect(),
        );

        assert!(!state.is_dirty());
        assert_eq!(
            state.tab_extras("typing", TAB_A).map(|bag| bag.flag("params.font", true)),
            Some(false)
        );
    }

    /// A `PanelDockOutput` is filled by EVERY window of the dock, and a
    /// sub-window's rect is in that window's own frame — where the dock area
    /// starts near the origin. Handing it to a caller that anchors main-window UI
    /// (or occludes main-window pointer input) with it puts a dead zone in this
    /// window's top-left corner, so a detached panel must read as "not on screen".
    #[test]
    fn the_output_answers_for_the_main_window_only() {
        let main = Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(300.0, 200.0));
        // The very rect a detached panel reports: its window's own coordinates.
        let detached = Rect::from_min_size(Pos2::new(8.0, 8.0), Vec2::new(340.0, 160.0));
        let mut output = PanelDockOutput::default();
        output.record(HostId::MainWindow, PanelId::new(0), main, &[TAB_A]);
        output.record(HostId::SubWindow(1), PanelId::new(3), detached, &[TAB_B]);

        assert_eq!(output.panel_rect(PanelId::new(0)), Some(main));
        assert_eq!(output.tab_rect(TAB_A), Some(main));
        assert_eq!(output.panel_rect(PanelId::new(3)), None);
        assert_eq!(output.tab_rect(TAB_B), None);
        assert_eq!(output.drawn_panels().collect::<Vec<_>>(), vec![(
            PanelId::new(0),
            main
        )]);
        assert!(!output.is_empty());

        // A dock whose every panel is detached has nothing to say about this window.
        let mut detached_only = PanelDockOutput::default();
        detached_only.record(HostId::SubWindow(1), PanelId::new(3), detached, &[TAB_B]);
        assert!(detached_only.is_empty());
        assert_eq!(detached_only.tab_rect(TAB_B), None);
        assert_eq!(detached_only.drawn_panels().count(), 0);
    }

    #[test]
    fn a_persisted_layout_wins_over_the_default_without_dirtying_the_state() {
        let mut restored = DockLayout::new();
        restored
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let mut state = PanelDockState::new();
        state.install_persisted_layouts(
            [("typing".to_owned(), restored)].into_iter().collect(),
        );
        // Restoring is not a user change: the writer must not be woken by it.
        assert!(!state.is_dirty());

        state.ensure_default_layout("typing", single_panel_default_layout);
        let layout = state.layout("typing").expect("layout");
        assert_eq!(layout.panel_of_tab(TAB_B), Some(PanelId::new(0)));
        assert!(!state.is_dirty());
    }

    #[test]
    fn a_declared_tab_the_persisted_layout_lacks_is_re_created() {
        let mut restored = DockLayout::new();
        restored.insert_panel(panel_with(0, &[TAB_A])).expect("insert 0");
        let mut state = PanelDockState::new();
        state.install_persisted_layouts(
            [("typing".to_owned(), restored)].into_iter().collect(),
        );
        state.ensure_default_layout("typing", single_panel_default_layout);

        // What `PanelDock::end` does on the first frame that declares the tabs.
        let layout = state.layouts.get_mut("typing").expect("layout");
        assert!(ensure_declared_tabs(
            layout,
            HostId::MainWindow,
            &[TAB_A, TAB_B],
            None
        ));
        assert_eq!(layout.panel_of_tab(TAB_A), Some(PanelId::new(0)));
        assert_eq!(layout.panel_of_tab(TAB_B), Some(PanelId::new(1)));
    }

    #[test]
    fn an_invalid_persisted_layout_is_refused_in_favour_of_the_default() {
        let mut broken = panel_with(0, &[TAB_A]);
        broken.active_tab = TAB_B;
        let mut state = PanelDockState::new();
        state.install_persisted_layouts(
            [(
                "typing".to_owned(),
                DockLayout::from_panels_unchecked(vec![broken]),
            )]
            .into_iter()
            .collect(),
        );
        assert!(state.layout("typing").is_none());
        state.ensure_default_layout("typing", single_panel_default_layout);
        assert_eq!(
            state.layout("typing").expect("layout").panel_of_tab(TAB_A),
            Some(PanelId::new(0))
        );
    }

    #[test]
    fn the_dirty_snapshot_is_handed_out_once_per_change() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", single_panel_default_layout);
        let snapshot = state.take_dirty_layouts().expect("the default install dirties");
        assert_eq!(
            snapshot
                .layouts
                .get("typing")
                .map(DockLayout::panels)
                .map(<[_]>::len),
            Some(1)
        );
        // Nothing changed since: no second write.
        assert!(state.take_dirty_layouts().is_none());

        // «Сбросить раскладку» must reach the config, or a reset would come back
        // on the next start.
        assert!(state.reset_layout("typing"));
        assert!(state.take_dirty_layouts().is_some());
        assert!(state.take_dirty_layouts().is_none());
    }

    /// Default layout of the sub-window fixtures: ONE panel holding both tabs,
    /// so detaching one of them still leaves a panel in the main window.
    fn two_tab_default_layout() -> DockLayout {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel_with(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout
    }

    /// State with one panel left in the main window and one in sub-window `0` —
    /// exactly what detaching a tab out of a two-tab panel leaves behind.
    fn state_with_a_sub_window() -> PanelDockState {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", two_tab_default_layout);
        {
            let layout = state.layout_mut("typing").expect("layout");
            if let Err(error) =
                layout.detach_tab_to_host(TAB_A, HostId::SubWindow(0), Pos2::new(8.0, 8.0))
            {
                unreachable!("the fixture tab can be detached: {error}");
            }
        }
        state.sub_windows.push(SubWindowNode::new(
            0,
            Some(Pos2::new(300.0, 200.0)),
            Vec2::new(420.0, 560.0),
        ));
        state.opened_sub_windows.insert(0);
        state.clear_dirty();
        state
    }

    #[test]
    fn panels_are_routed_to_the_window_that_hosts_them() {
        let state = state_with_a_sub_window();
        let layout = state.layout("typing").expect("layout");
        let decls = decls(&[
            (TAB_A, meta(true, None, Some(Vec2::new(300.0, 200.0)))),
            (TAB_B, meta(true, None, Some(Vec2::new(300.0, 200.0)))),
        ]);

        let main = plan_frame(layout, HostId::MainWindow, &decls, &HashMap::new());
        let sub = plan_frame(layout, HostId::SubWindow(0), &decls, &HashMap::new());
        // Every panel is planned by exactly one window, and the detached tab is
        // planned only by the window that holds it.
        assert_eq!(main.panels.len(), 1);
        assert_eq!(main.panels[0].visible_tabs, vec![TAB_B]);
        assert_eq!(sub.panels.len(), 1);
        assert_eq!(sub.panels[0].visible_tabs, vec![TAB_A]);
    }

    #[test]
    fn closing_a_window_returns_its_panels_to_the_main_one() {
        // Requirement 10: the user's tabs must never leave with the window.
        let mut state = state_with_a_sub_window();
        state.close_sub_window(0);
        assert!(state.sub_windows().is_empty());
        let layout = state.layout("typing").expect("layout");
        assert!(layout.panel_of_tab(TAB_A).is_some());
        assert!(!layout.has_panels_in_host(HostId::SubWindow(0)));
        assert_eq!(layout.validate(), Ok(()));
        // The change has to reach the config, or the window would come back on
        // the next start.
        assert!(state.is_dirty());
    }

    #[test]
    fn a_window_that_lost_its_last_panel_is_closed() {
        let mut state = state_with_a_sub_window();
        {
            // The user drags the detached tab back into the main window.
            let layout = state.layout_mut("typing").expect("layout");
            let target = layout
                .panels()
                .iter()
                .find(|panel| panel.host == HostId::MainWindow)
                .map(|panel| panel.id);
            let target = match target {
                Some(target) => target,
                None => unreachable!("the fixture keeps one panel in the main window"),
            };
            if let Err(error) = layout.move_tab(TAB_A, target, 0) {
                unreachable!("the tab can come back: {error}");
            }
        }
        assert!(state.prune_sub_windows());
        assert!(state.sub_windows().is_empty());
        // …and a second pass has nothing left to do.
        assert!(!state.prune_sub_windows());
    }

    /// State with two panels in sub-window `0` (`TAB_A`, `TAB_C`) and one left in
    /// the main window (`TAB_B`), so a menu move has a first panel to pick and a
    /// second one to leave alone.
    fn state_with_two_panels_in_a_sub_window() -> PanelDockState {
        let mut state = state_with_a_sub_window();
        {
            let layout = state.layout_mut("typing").expect("layout");
            if let Err(error) =
                layout.insert_panel(PanelNode::new(
                    PanelId::new(9),
                    HostId::SubWindow(0),
                    vec![TAB_C],
                ).expect("second sub-window panel"))
            {
                unreachable!("the fixture panel can be inserted: {error}");
            }
        }
        state.clear_dirty();
        state
    }

    #[test]
    fn a_menu_move_sends_a_tab_into_the_first_panel_of_the_target_window() {
        let mut state = state_with_two_panels_in_a_sub_window();
        let first = state
            .layout("typing")
            .expect("layout")
            .panels_in_host(HostId::SubWindow(0))
            .map(|node| node.id)
            .next()
            .expect("the fixture window holds panels");
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Tab(TAB_B),
            HostId::SubWindow(0),
        ));
        let layout = state.layout("typing").expect("layout");
        let receiver = layout.panel(first).expect("receiving panel");
        // Appended at the END of that panel's strip and shown at once, exactly as
        // a drop onto the strip would be.
        assert_eq!(receiver.tabs.last().copied(), Some(TAB_B));
        assert_eq!(receiver.active_tab, TAB_B);
        // The source panel held nothing else, so the model removed it, and the
        // main window is left with no panel at all.
        assert!(!layout.has_panels_in_host(HostId::MainWindow));
        assert_eq!(layout.validate(), Ok(()));
        assert!(state.is_dirty());
    }

    #[test]
    fn a_menu_move_into_a_window_without_panels_gives_the_tab_one_of_its_own() {
        let mut state = PanelDockState::new();
        state.ensure_default_layout("typing", two_tab_default_layout);
        state.sub_windows.push(SubWindowNode::new(
            1,
            None,
            window::DEFAULT_SUB_WINDOW_SIZE,
        ));
        state.clear_dirty();
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Tab(TAB_A),
            HostId::SubWindow(1),
        ));
        let layout = state.layout("typing").expect("layout");
        let panel = layout.panel_of_tab(TAB_A).expect("the tab kept a panel");
        let node = layout.panel(panel).expect("panel");
        assert_eq!(node.host, HostId::SubWindow(1));
        assert_eq!(node.tabs, vec![TAB_A]);
        // The window was empty, so the newcomer takes the first cascade slot.
        assert_eq!(node.pos, Pos2::new(DOCK_GAP, DOCK_GAP));
        // The other tab stayed behind in the main window.
        assert_eq!(
            layout
                .panel_of_tab(TAB_B)
                .and_then(|id| layout.panel(id))
                .map(|node| node.host),
            Some(HostId::MainWindow)
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_menu_move_of_a_whole_panel_lands_clear_of_the_panels_already_there() {
        let mut state = state_with_two_panels_in_a_sub_window();
        let main_panel = state
            .layout("typing")
            .expect("layout")
            .panels_in_host(HostId::MainWindow)
            .map(|node| node.id)
            .next()
            .expect("the fixture keeps one panel in the main window");
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Panel(main_panel),
            HostId::SubWindow(0),
        ));
        let layout = state.layout("typing").expect("layout");
        let node = layout.panel(main_panel).expect("the panel survived");
        assert_eq!(node.host, HostId::SubWindow(0));
        assert_eq!(node.tabs, vec![TAB_B]);
        assert_eq!(node.anchor, PanelAnchor::Free);
        // Two panels were already there, so the cascade steps twice — the panel
        // cannot arrive exactly on top of one of them.
        let expected = DOCK_GAP + 2.0 * AUTO_PANEL_CASCADE_STEP;
        assert_eq!(node.pos, Pos2::new(expected, expected));
        assert!(!layout.has_panels_in_host(HostId::MainWindow));
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_menu_move_that_empties_a_window_closes_it() {
        // The last panel of a sub-window leaves through the menu: the window has
        // nothing left to show and must not stay open and grey (requirement 10).
        let mut state = state_with_a_sub_window();
        let detached = state
            .layout("typing")
            .expect("layout")
            .panel_of_tab(TAB_A)
            .expect("the detached tab has a panel");
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Panel(detached),
            HostId::MainWindow,
        ));
        assert!(state.prune_sub_windows());
        assert!(state.sub_windows().is_empty());
        let layout = state.layout("typing").expect("layout");
        assert!(!layout.has_panels_in_host(HostId::SubWindow(0)));
        assert!(layout.panel_of_tab(TAB_A).is_some());
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_menu_landing_is_refused_when_the_tab_is_already_alone_in_the_target() {
        // The submenu never offers the host a tab is already in; this is the
        // guard for a request that outlived the layout it was built from.
        let state = state_with_a_sub_window();
        let layout = state.layout("typing").expect("layout");
        assert_eq!(
            menu_tab_landing(
                layout,
                TAB_A,
                HostId::SubWindow(0),
                Some(AREA.size()),
                PANEL_SIZE
            ),
            TabLanding::Cancelled
        );
    }

    /// Size the menu-move fixtures measure their tabs at, so the centred slot is
    /// an exact number instead of whatever the default happens to be.
    const PANEL_SIZE: Vec2 = Vec2::new(300.0, 200.0);

    /// Position a panel of [`PANEL_SIZE`] takes in the middle of [`AREA`].
    fn centered_pos() -> Pos2 {
        Pos2::new(
            (AREA.width() - PANEL_SIZE.x) * 0.5,
            (AREA.height() - PANEL_SIZE.y) * 0.5,
        )
    }

    /// The fixture with two panels in sub-window `0`, plus a main-window dock
    /// area of [`AREA`] and a measurement for every tab — i.e. the state a real
    /// frame leaves behind, which is what the menu move reads.
    fn state_with_a_drawn_main_window() -> PanelDockState {
        let mut state = state_with_two_panels_in_a_sub_window();
        state.host_areas.insert(HostId::MainWindow, AREA);
        for tab in [TAB_A, TAB_B, TAB_C] {
            state.remember_measurement("typing", tab, PANEL_SIZE);
        }
        state
    }

    #[test]
    fn a_menu_move_into_the_main_window_gives_the_tab_a_panel_in_the_middle() {
        // The user's rule: a tab sent to the main window must not join whatever
        // panel happens to be first there — it gets a panel of its own, in the
        // middle of the dock area, where it can be seen.
        let mut state = state_with_a_drawn_main_window();
        let occupant = state
            .layout("typing")
            .expect("layout")
            .panels_in_host(HostId::MainWindow)
            .map(|node| node.id)
            .next()
            .expect("the fixture keeps one panel in the main window");
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Tab(TAB_A),
            HostId::MainWindow,
        ));
        let layout = state.layout("typing").expect("layout");
        let panel = layout.panel_of_tab(TAB_A).expect("the tab kept a panel");
        assert_ne!(panel, occupant, "the tab must not join the first panel");
        let node = layout.panel(panel).expect("panel");
        assert_eq!(node.host, HostId::MainWindow);
        assert_eq!(node.tabs, vec![TAB_A]);
        assert_eq!(node.anchor, PanelAnchor::Free);
        assert_eq!(node.pos, centered_pos());
        // The panel that was already there kept its own tab.
        assert_eq!(
            layout.panel(occupant).map(|node| node.tabs.clone()),
            Some(vec![TAB_B])
        );
        assert_eq!(layout.validate(), Ok(()));
        assert!(state.is_dirty());
    }

    #[test]
    fn a_centered_menu_move_steps_off_a_panel_already_standing_there() {
        // Requirement of the same rule: the middle may be taken. The newcomer
        // then cascades off it instead of burying it.
        let mut state = state_with_a_drawn_main_window();
        let center = centered_pos();
        {
            let layout = state.layout_mut("typing").expect("layout");
            let occupant = layout
                .panels_in_host(HostId::MainWindow)
                .map(|node| node.id)
                .next()
                .expect("the fixture keeps one panel in the main window");
            if let Err(error) = layout.set_panel_pos(occupant, center) {
                unreachable!("the fixture panel can be moved to the middle: {error}");
            }
        }
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Tab(TAB_A),
            HostId::MainWindow,
        ));
        let layout = state.layout("typing").expect("layout");
        let panel = layout.panel_of_tab(TAB_A).expect("the tab kept a panel");
        let node = layout.panel(panel).expect("panel");
        assert_eq!(
            node.pos,
            center + Vec2::splat(AUTO_PANEL_CASCADE_STEP),
            "a newcomer must never land exactly on the panel already in the middle"
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn a_menu_move_of_a_whole_panel_into_the_main_window_is_centered_too() {
        let mut state = state_with_a_drawn_main_window();
        let detached = state
            .layout("typing")
            .expect("layout")
            .panel_of_tab(TAB_A)
            .expect("the detached tab has a panel");
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Panel(detached),
            HostId::MainWindow,
        ));
        let layout = state.layout("typing").expect("layout");
        let node = layout.panel(detached).expect("the panel survived");
        assert_eq!(node.host, HostId::MainWindow);
        assert_eq!(node.anchor, PanelAnchor::Free);
        assert_eq!(node.pos, centered_pos());
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn the_centered_rule_falls_back_to_the_cascade_without_a_drawn_area() {
        // A window that drew no dock this frame says nothing about where its
        // middle is; the slot degrades to the corner cascade rather than to an
        // invented coordinate.
        let mut state = state_with_a_drawn_main_window();
        state.host_areas.clear();
        assert!(move_into_existing_host(
            &mut state,
            "typing",
            MenuMoveSubject::Tab(TAB_A),
            HostId::MainWindow,
        ));
        let layout = state.layout("typing").expect("layout");
        let panel = layout.panel_of_tab(TAB_A).expect("the tab kept a panel");
        let node = layout.panel(panel).expect("panel");
        // One panel was already in the main window, so the cascade steps once.
        let expected = DOCK_GAP + AUTO_PANEL_CASCADE_STEP;
        assert_eq!(node.pos, Pos2::new(expected, expected));
    }

    #[test]
    fn a_sub_window_that_already_holds_panels_still_collects_the_tab() {
        // The other half of the rule, restated where it can regress: only the
        // MAIN window creates a panel of its own; a sub-window opened for panels
        // takes the tab into the first panel it holds.
        let state = state_with_a_drawn_main_window();
        let layout = state.layout("typing").expect("layout");
        let first = layout
            .panels_in_host(HostId::SubWindow(0))
            .map(|node| node.id)
            .next()
            .expect("the fixture window holds panels");
        assert_eq!(
            menu_tab_landing(
                layout,
                TAB_B,
                HostId::SubWindow(0),
                Some(AREA.size()),
                PANEL_SIZE
            ),
            TabLanding::HeaderStrip {
                panel: first,
                index: 1
            }
        );
    }

    #[test]
    fn a_panel_bigger_than_the_area_starts_at_the_cascade_origin() {
        // Centring a panel wider than the window would place it at a negative
        // offset; it starts at the cascade origin instead, which is where the
        // solver would clamp it anyway.
        let layout = DockLayout::new();
        assert_eq!(
            centered_slot_in_host(
                &layout,
                HostId::MainWindow,
                Some(Vec2::new(100.0, 100.0)),
                Vec2::new(400.0, 400.0)
            ),
            Pos2::new(DOCK_GAP, DOCK_GAP)
        );
    }

    #[test]
    fn the_cascade_slot_steps_with_the_windows_occupancy() {
        let state = state_with_two_panels_in_a_sub_window();
        let layout = state.layout("typing").expect("layout");
        assert_eq!(
            free_slot_in_host(layout, HostId::MainWindow),
            Pos2::new(DOCK_GAP + AUTO_PANEL_CASCADE_STEP, DOCK_GAP + AUTO_PANEL_CASCADE_STEP)
        );
        assert_eq!(
            free_slot_in_host(layout, HostId::SubWindow(7)),
            Pos2::new(DOCK_GAP, DOCK_GAP)
        );
    }

    #[test]
    fn a_window_empty_only_in_the_active_program_tab_stays_open() {
        // Requirement 11: switching program tabs must not close the window, even
        // though the newly active tab puts nothing in it.
        let mut state = state_with_a_sub_window();
        state.ensure_default_layout("cleaning", empty_default_layout);
        assert!(!state.prune_sub_windows());
        assert_eq!(state.sub_windows().len(), 1);
        // The layout of the program tab now drawing has no panel for the window,
        // so its pass draws nothing inside it — and the window stays.
        let cleaning = state.layout("cleaning").expect("layout");
        let plan = plan_frame(
            cleaning,
            HostId::SubWindow(0),
            &decls(&[]),
            &HashMap::new(),
        );
        assert!(plan.panels.is_empty());
    }

    /// A program tab that declares nothing, standing in for another tab of the
    /// program while the sub-window keeps living.
    fn empty_default_layout() -> DockLayout {
        DockLayout::new()
    }

    #[test]
    fn a_persisted_window_without_panels_is_not_opened() {
        let mut state = PanelDockState::new();
        state.install_persisted_layouts(BTreeMap::new());
        state.install_persisted_sub_windows(vec![SubWindowNode::new(
            0,
            None,
            Vec2::new(420.0, 560.0),
        )]);
        assert!(state.sub_windows().is_empty());
        // Restoring is not a user change, even when it had to drop something.
        assert!(!state.is_dirty());
    }

    #[test]
    fn a_persisted_window_with_panels_is_restored() {
        let mut restored = DockLayout::new();
        restored
            .insert_panel(
                PanelNode::new(PanelId::new(0), HostId::SubWindow(2), vec![TAB_A])
                    .expect("panel in sub-window 2"),
            )
            .expect("insert");
        let mut state = PanelDockState::new();
        state.install_persisted_layouts([("typing".to_owned(), restored)].into_iter().collect());
        state.install_persisted_sub_windows(vec![SubWindowNode::new(
            2,
            Some(Pos2::new(120.0, 80.0)),
            Vec2::new(400.0, 500.0),
        )]);
        assert_eq!(state.sub_windows().len(), 1);
        assert_eq!(state.sub_windows()[0].index, 2);
        assert_eq!(state.sub_windows()[0].pos, Some(Pos2::new(120.0, 80.0)));
        assert!(!state.is_dirty());
    }

    #[test]
    fn the_dirty_snapshot_carries_the_windows_too() {
        let mut state = state_with_a_sub_window();
        state.layout_mut("typing").expect("layout");
        let snapshot = state
            .take_dirty_layouts()
            .expect("touching the layout dirties the state");
        assert_eq!(snapshot.sub_windows.len(), 1);
        assert_eq!(snapshot.sub_windows[0].index, 0);
    }
}
