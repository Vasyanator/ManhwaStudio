/*
File: src/widgets/panel_dock/model.rs

Purpose:
Pure data model of the dockable-panel system: tab identity, panel identity, the
anchoring graph between panels, and the invariants that keep that graph a forest.

Main responsibilities:
- describe the panel arrangement of every host without touching any egui state;
- enforce the model invariants on construction and on every mutation;
- answer the queries the layout solver and the frame driver need (connected
  chains, tab ownership, free panel ids).

Key structures:
- `TabId`, `PanelId`, `HostId`, `DockEdge`, `PanelAnchor`
- `PanelNode`, `DockLayout`, `MoveTabOutcome`
- `DockModelError`

Key functions:
- `DockLayout::insert_panel`, `remove_panel`, `move_tab`, `set_anchor`
- `DockLayout::detach_tab`, `detach_tab_to_host`, `move_panel_to_host`, `rehost_panels`
- `DockLayout::set_panel_pos`, `set_size_override`, `set_collapsed`, `set_active_tab`, `edit`
- `DockLayout::validate`, `chains`, `panel_of_tab`, `next_panel_id`,
  `has_panels_in_host`, `sub_window_indices`

Notes:
`Pos2` / `Vec2` are used here as plain geometry only. Nothing in this file may
depend on `egui::Context`, `egui::Ui` or `egui::Memory`: the model must stay
unit-testable without a running GUI, and the solver must stay a pure function of
it.

No `&mut PanelNode` ever leaves this file. Consumers mutate a layout through the
checked operations above; `edit` is the general escape hatch and rolls back a
change that would break `validate`. That is what makes an invalid `DockLayout`
unconstructible outside tests, which matters most for phase 5: an invariant
broken by a consumer would otherwise be written to disk.
*/

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use egui::{Pos2, Vec2};
use thiserror::Error;

/// Stable, non-localised identifier of a tab.
///
/// The wrapped string is a program literal (listed in
/// `dev-docs/i18n_exclusions.md`), never a translated title: it is what the
/// persisted layout and the egui `Id` of the tab header derive from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(&'static str);

impl TabId {
    /// Wraps a literal tab key. The key must be unique across the program.
    #[must_use]
    pub const fn new(key: &'static str) -> Self {
        Self(key)
    }

    /// Returns the raw literal key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Identity of a panel inside one `DockLayout`.
///
/// Not stable across sessions by value: on load the persisted panel list is
/// renumbered, and anchors are rewritten to the new ids. Only the relations
/// inside a single layout are meaningful.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PanelId(u32);

impl PanelId {
    /// Wraps a raw panel index.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw panel index (needed for persistence and egui id salts).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for PanelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The window a panel lives in.
///
/// `SubWindow(index)` refers to an entry of the dock's sub-window list, not to
/// an OS handle: sub-windows outlive program-tab switches while the panels they
/// host are per-tab.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HostId {
    /// The studio's main window.
    MainWindow,
    /// A detached OS window created by dragging a tab out of every window.
    SubWindow(u32),
}

/// One of the four sides a panel can be attached to.
///
/// The meaning of the side depends on the anchor kind and is part of the
/// anchor's contract:
/// * `PanelAnchor::Panel` / `PanelAnchor::CanvasControls` — the panel sits
///   *outside* the target, adjacent to this side of it (`Bottom` => below it);
/// * `PanelAnchor::ViewportEdge` — the panel sits *inside* the host area,
///   flush against this side of it (`Bottom` => at the area's bottom).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DockEdge {
    /// Upper side.
    Top,
    /// Lower side.
    Bottom,
    /// Left side.
    Left,
    /// Right side.
    Right,
}

impl DockEdge {
    /// Returns the side facing the opposite direction.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// `true` for `Top`/`Bottom`: attaching to this side displaces the panel
    /// along the Y axis, and the shared edge itself runs horizontally.
    #[must_use]
    pub const fn is_vertical(self) -> bool {
        match self {
            Self::Top | Self::Bottom => true,
            Self::Left | Self::Right => false,
        }
    }

    /// `true` for `Left`/`Right`: attaching to this side displaces the panel
    /// along the X axis.
    #[must_use]
    pub const fn is_horizontal(self) -> bool {
        !self.is_vertical()
    }
}

/// What a panel is attached to.
///
/// The `align` / `along` fractions are always in `0.0..=1.0` and address the
/// position along the *shared* side, as a fraction of the free travel
/// (`shared side length - panel side length`): `0.0` makes the panel flush with
/// the start of that side (left or top), `1.0` flush with its end (right or
/// bottom), `0.5` centres it. Values outside the range and non-finite values are
/// clamped by the solver, never rejected.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PanelAnchor {
    /// Free-floating: the panel's own `pos` is authoritative.
    Free,
    /// Attached to another panel of the same host, outside it.
    Panel {
        /// The panel this one hangs off.
        target: PanelId,
        /// Which side of `target` this panel sits next to.
        edge: DockEdge,
        /// Position along the shared side, `0.0..=1.0`.
        align: f32,
    },
    /// Attached to a side of the host area itself, inside it.
    ViewportEdge {
        /// Which side of the host area the panel is flush with.
        edge: DockEdge,
        /// Position along that side, `0.0..=1.0`.
        along: f32,
    },
    /// Attached to the `CanvasView` controls panel, outside it.
    ///
    /// That panel is not part of this model: the solver receives its rect as an
    /// input and never moves it. When no such rect is supplied, the anchor
    /// degrades to `Free` (see `solver::solve`).
    CanvasControls {
        /// Which side of the controls rect the panel sits next to.
        edge: DockEdge,
        /// Position along that side, `0.0..=1.0`.
        along: f32,
    },
}

impl PanelAnchor {
    /// Returns the anchored-to panel, if this anchor targets one.
    #[must_use]
    pub const fn target_panel(self) -> Option<PanelId> {
        match self {
            Self::Panel { target, .. } => Some(target),
            Self::Free | Self::ViewportEdge { .. } | Self::CanvasControls { .. } => None,
        }
    }

    /// Returns the side this anchor attaches to, if it has one.
    #[must_use]
    pub const fn edge(self) -> Option<DockEdge> {
        match self {
            Self::Panel { edge, .. }
            | Self::ViewportEdge { edge, .. }
            | Self::CanvasControls { edge, .. } => Some(edge),
            Self::Free => None,
        }
    }
}

/// One panel: a movable frame that owns one or more tabs and shows one of them.
///
/// Invariants (enforced by `DockLayout`, checkable via [`PanelNode::validate`]):
/// `tabs` is never empty, holds no duplicates, and always contains `active_tab`.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelNode {
    /// Identity inside the owning layout.
    pub id: PanelId,
    /// Window this panel is drawn in.
    pub host: HostId,
    /// What the panel is attached to.
    pub anchor: PanelAnchor,
    /// Top-left corner in host-area coordinates (offset from the area's
    /// top-left, so a window resize keeps the layout meaningful). Authoritative
    /// only while `anchor` is `Free`; otherwise it is a cache of the last
    /// solved position.
    pub pos: Pos2,
    /// Outer panel size fixed by a manual resize, in points, header included.
    /// `None` means "follow the measured content size".
    pub size_override: Option<Vec2>,
    /// Collapsed panels show only their header strip and never shrink further.
    pub collapsed: bool,
    /// Tab order inside the panel's header strip; never empty.
    pub tabs: Vec<TabId>,
    /// The tab whose body is drawn; always an element of `tabs`.
    pub active_tab: TabId,
}

impl PanelNode {
    /// Creates a free-floating panel at the host area's origin holding `tabs`,
    /// with the first tab active.
    ///
    /// # Errors
    /// [`DockModelError::EmptyPanel`] if `tabs` is empty, and
    /// [`DockModelError::DuplicateTab`] if it repeats a tab.
    pub fn new(id: PanelId, host: HostId, tabs: Vec<TabId>) -> Result<Self, DockModelError> {
        let active_tab = tabs
            .first()
            .copied()
            .ok_or(DockModelError::EmptyPanel(id))?;
        let node = Self {
            id,
            host,
            anchor: PanelAnchor::Free,
            pos: Pos2::ZERO,
            size_override: None,
            collapsed: false,
            tabs,
            active_tab,
        };
        node.validate()?;
        Ok(node)
    }

    /// `true` if this panel owns `tab`.
    #[must_use]
    pub fn contains_tab(&self, tab: TabId) -> bool {
        self.tabs.contains(&tab)
    }

    /// Checks this node's own invariants: non-empty, duplicate-free `tabs`
    /// containing `active_tab`.
    ///
    /// # Errors
    /// [`DockModelError::EmptyPanel`], [`DockModelError::DuplicateTab`] or
    /// [`DockModelError::ActiveTabNotInPanel`].
    pub fn validate(&self) -> Result<(), DockModelError> {
        if self.tabs.is_empty() {
            return Err(DockModelError::EmptyPanel(self.id));
        }
        let mut seen = BTreeSet::new();
        for tab in &self.tabs {
            if !seen.insert(*tab) {
                return Err(DockModelError::DuplicateTab {
                    tab: *tab,
                    panel: self.id,
                });
            }
        }
        if !self.contains_tab(self.active_tab) {
            return Err(DockModelError::ActiveTabNotInPanel {
                panel: self.id,
                tab: self.active_tab,
            });
        }
        Ok(())
    }
}

/// What [`DockLayout::move_tab`] did besides moving the tab.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MoveTabOutcome {
    /// The panel the tab was taken from.
    pub source_panel: PanelId,
    /// Set when the source panel lost its last tab and was therefore removed
    /// (its dependants were re-anchored, see [`DockLayout::remove_panel`]).
    pub removed_source_panel: Option<PanelId>,
}

/// What [`DockLayout::detach_tab`] did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DetachTabOutcome {
    /// The panel now holding the tab: a brand-new one, or — when the tab was the
    /// last of its panel — the source panel itself, moved and freed.
    pub panel: PanelId,
    /// `true` when `panel` is a panel that did not exist before.
    pub created: bool,
    /// The panel the tab was taken from.
    pub source_panel: PanelId,
    /// Set when the source panel lost its last tab and was therefore removed;
    /// only ever set together with `created`, because a panel that still owns
    /// the tab is reused instead of removed.
    pub removed_source_panel: Option<PanelId>,
}

/// Errors produced by the dock data model. All of them mean "the requested
/// mutation would break a documented invariant" and leave the layout unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum DockModelError {
    /// The referenced panel does not exist in this layout.
    #[error("panel {0} is not present in this layout")]
    UnknownPanel(PanelId),
    /// The referenced tab is not owned by any panel of this layout.
    #[error("tab `{0}` is not present in this layout")]
    UnknownTab(TabId),
    /// A panel would be left without tabs; a tab-less panel must be removed.
    #[error("panel {0} would have no tabs")]
    EmptyPanel(PanelId),
    /// `active_tab` does not belong to the panel's own tab list.
    #[error("active tab `{tab}` does not belong to panel {panel}")]
    ActiveTabNotInPanel {
        /// The offending panel.
        panel: PanelId,
        /// The tab that is active but not owned.
        tab: TabId,
    },
    /// Two panels of one layout claim the same id.
    #[error("panel id {0} is already used in this layout")]
    DuplicatePanelId(PanelId),
    /// One tab may live in exactly one panel of a layout.
    #[error("tab `{tab}` is already owned by panel {panel}")]
    DuplicateTab {
        /// The tab claimed twice.
        tab: TabId,
        /// The panel that already owns it.
        panel: PanelId,
    },
    /// The anchor graph must be a forest; this anchor would close a loop.
    #[error("anchoring panel {panel} to panel {target} would create a cycle")]
    CycleDetected {
        /// The panel being (re-)anchored, or the panel a cycle was found from.
        panel: PanelId,
        /// The anchor target that closes the loop.
        target: PanelId,
    },
    /// Panels can only be anchored to panels drawn in the same window.
    #[error("panel {panel} is anchored to panel {target} in another host")]
    CrossHostAnchor {
        /// The anchored panel.
        panel: PanelId,
        /// The target living in a different host.
        target: PanelId,
    },
    /// `u32` panel ids are exhausted (unreachable in practice; reported instead
    /// of wrapping around onto a live id).
    #[error("panel id space is exhausted")]
    PanelIdOverflow,
}

/// The panel arrangement of one program tab, across all hosts.
///
/// The panel list is private so that every mutation goes through a method that
/// preserves the invariants:
/// * panel ids are unique;
/// * every panel holds at least one tab, without duplicates, and its
///   `active_tab` is one of them;
/// * a `TabId` is owned by at most one panel;
/// * `PanelAnchor::Panel` targets an existing panel of the same host, and the
///   resulting directed graph is a forest (no cycles).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockLayout {
    panels: Vec<PanelNode>,
}

impl DockLayout {
    /// Creates an empty layout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a layout from a full panel list, rejecting any invariant breach.
    ///
    /// # Errors
    /// Any [`DockModelError`] reported by [`DockLayout::validate`].
    pub fn from_panels(panels: Vec<PanelNode>) -> Result<Self, DockModelError> {
        let layout = Self { panels };
        layout.validate()?;
        Ok(layout)
    }

    /// Builds a layout from a panel list WITHOUT checking a single invariant.
    ///
    /// Test-only, and the only way to obtain an invalid `DockLayout`: every
    /// public mutation rejects one. It exists so the guards that have to survive
    /// an invalid layout — [`DockLayout::validate`] itself, and the driver's
    /// handling of a layout that fails it — can be exercised at all.
    #[cfg(test)]
    pub(crate) fn from_panels_unchecked(panels: Vec<PanelNode>) -> Self {
        Self { panels }
    }

    /// All panels, in insertion order.
    #[must_use]
    pub fn panels(&self) -> &[PanelNode] {
        &self.panels
    }

    /// Panels drawn in `host`, in insertion order.
    pub fn panels_in_host(&self, host: HostId) -> impl Iterator<Item = &PanelNode> {
        self.panels.iter().filter(move |panel| panel.host == host)
    }

    /// Looks up a panel by id.
    #[must_use]
    pub fn panel(&self, id: PanelId) -> Option<&PanelNode> {
        self.panels.iter().find(|panel| panel.id == id)
    }

    /// Mutable access to one panel — deliberately NOT public.
    ///
    /// Handing a `&mut PanelNode` to a consumer would put every layout invariant
    /// at its mercy: emptying `tabs` produces a panel that can never be drawn yet
    /// stays in the layout (and in the persisted one), and reusing a `PanelId`
    /// gives two panels the same egui `Id`. Outside this module the layout is
    /// mutated through the checked operations below.
    fn panel_mut(&mut self, id: PanelId) -> Option<&mut PanelNode> {
        self.panels.iter_mut().find(|panel| panel.id == id)
    }

    /// Moves a panel to `pos`, in host-area coordinates.
    ///
    /// Position constrains nothing, so this cannot fail for any reason other
    /// than the panel not existing. The solver sanitizes a non-finite value.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout.
    pub fn set_panel_pos(&mut self, id: PanelId, pos: Pos2) -> Result<(), DockModelError> {
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        panel.pos = pos;
        Ok(())
    }

    /// Pins a panel's outer size to a manually resized value, or (with `None`)
    /// hands it back to the measured content size.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout.
    pub fn set_size_override(
        &mut self,
        id: PanelId,
        size: Option<Vec2>,
    ) -> Result<(), DockModelError> {
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        panel.size_override = size;
        Ok(())
    }

    /// Sets a panel's collapsed state.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout.
    pub fn set_collapsed(&mut self, id: PanelId, collapsed: bool) -> Result<(), DockModelError> {
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        panel.collapsed = collapsed;
        Ok(())
    }

    /// Makes `tab` the panel's drawn tab.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout and
    /// [`DockModelError::ActiveTabNotInPanel`] if the panel does not own `tab`;
    /// the layout is unchanged in both cases.
    pub fn set_active_tab(&mut self, id: PanelId, tab: TabId) -> Result<(), DockModelError> {
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        if !panel.contains_tab(tab) {
            return Err(DockModelError::ActiveTabNotInPanel { panel: id, tab });
        }
        panel.active_tab = tab;
        Ok(())
    }

    /// Applies an arbitrary edit to one panel, rolling it back when the result
    /// would break a layout invariant.
    ///
    /// The escape hatch for what the setters above do not cover (tab order, a
    /// bulk change while loading a layout). `edit` is all-or-nothing: on failure
    /// the panel is restored exactly as it was, so a rejected edit can never
    /// leave the layout in the state that made it invalid.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout, otherwise
    /// whatever [`DockLayout::validate`] reports about the edited layout.
    pub fn edit(
        &mut self,
        id: PanelId,
        change: impl FnOnce(&mut PanelNode),
    ) -> Result<(), DockModelError> {
        let index = self
            .panels
            .iter()
            .position(|panel| panel.id == id)
            .ok_or(DockModelError::UnknownPanel(id))?;
        let restore = self.panels[index].clone();
        if let Some(panel) = self.panels.get_mut(index) {
            change(panel);
        }
        match self.validate() {
            Ok(()) => Ok(()),
            Err(error) => {
                // Only one panel was touched, so putting it back restores the
                // layout the caller started from.
                if let Some(panel) = self.panels.get_mut(index) {
                    *panel = restore;
                }
                Err(error)
            }
        }
    }

    /// Returns an id that no panel of this layout uses.
    ///
    /// # Errors
    /// [`DockModelError::PanelIdOverflow`] if `u32::MAX` is already taken.
    pub fn next_panel_id(&self) -> Result<PanelId, DockModelError> {
        let highest = self.panels.iter().map(|panel| panel.id.get()).max();
        match highest {
            Some(value) => value
                .checked_add(1)
                .map(PanelId::new)
                .ok_or(DockModelError::PanelIdOverflow),
            None => Ok(PanelId::new(0)),
        }
    }

    /// Adds a panel, keeping every layout invariant.
    ///
    /// # Errors
    /// [`DockModelError::DuplicatePanelId`], [`DockModelError::DuplicateTab`],
    /// [`DockModelError::EmptyPanel`], [`DockModelError::ActiveTabNotInPanel`],
    /// [`DockModelError::UnknownPanel`] / [`DockModelError::CrossHostAnchor`] /
    /// [`DockModelError::CycleDetected`] for an unusable anchor. The layout is
    /// left untouched on every error.
    pub fn insert_panel(&mut self, node: PanelNode) -> Result<(), DockModelError> {
        node.validate()?;
        if self.panel(node.id).is_some() {
            return Err(DockModelError::DuplicatePanelId(node.id));
        }
        for tab in &node.tabs {
            if let Some(owner) = self.panel_of_tab(*tab) {
                return Err(DockModelError::DuplicateTab {
                    tab: *tab,
                    panel: owner,
                });
            }
        }
        self.check_anchor(node.id, node.host, node.anchor)?;
        self.panels.push(node);
        Ok(())
    }

    /// Removes a panel and returns it.
    ///
    /// Panels anchored to the removed one inherit *its* anchor, so the rest of
    /// the chain stays attached to whatever the removed panel hung off instead
    /// of silently detaching to a stale position. When the removed panel was
    /// `Free`, its dependants become `Free` too and keep their own `pos`, which
    /// the frame driver refreshes from the last solved rect.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout.
    pub fn remove_panel(&mut self, id: PanelId) -> Result<PanelNode, DockModelError> {
        let index = self
            .panels
            .iter()
            .position(|panel| panel.id == id)
            .ok_or(DockModelError::UnknownPanel(id))?;
        let removed = self.panels.remove(index);
        for panel in &mut self.panels {
            if panel.anchor.target_panel() == Some(id) {
                panel.anchor = removed.anchor;
            }
        }
        Ok(removed)
    }

    /// Returns the panel owning `tab`.
    #[must_use]
    pub fn panel_of_tab(&self, tab: TabId) -> Option<PanelId> {
        self.panels
            .iter()
            .find(|panel| panel.contains_tab(tab))
            .map(|panel| panel.id)
    }

    /// Moves `tab` into `to_panel` at position `index` (clamped to the target's
    /// tab count). Moving inside the owning panel reorders it.
    ///
    /// The tab is always removed from its previous owner first, so a duplicate
    /// cannot arise. If that leaves the source panel empty, the source panel is
    /// removed as well (a tab-less panel is not a legal state) and reported in
    /// the outcome; if the moved tab was the source's active tab, the first
    /// remaining tab becomes active.
    ///
    /// # Errors
    /// [`DockModelError::UnknownTab`] if no panel owns `tab`, and
    /// [`DockModelError::UnknownPanel`] if `to_panel` does not exist.
    pub fn move_tab(
        &mut self,
        tab: TabId,
        to_panel: PanelId,
        index: usize,
    ) -> Result<MoveTabOutcome, DockModelError> {
        let source = self
            .panel_of_tab(tab)
            .ok_or(DockModelError::UnknownTab(tab))?;
        if self.panel(to_panel).is_none() {
            return Err(DockModelError::UnknownPanel(to_panel));
        }

        if source == to_panel {
            let panel = self
                .panel_mut(to_panel)
                .ok_or(DockModelError::UnknownPanel(to_panel))?;
            panel.tabs.retain(|owned| *owned != tab);
            let at = index.min(panel.tabs.len());
            panel.tabs.insert(at, tab);
            return Ok(MoveTabOutcome {
                source_panel: source,
                removed_source_panel: None,
            });
        }

        let source_emptied = {
            let panel = self
                .panel_mut(source)
                .ok_or(DockModelError::UnknownPanel(source))?;
            panel.tabs.retain(|owned| *owned != tab);
            match panel.tabs.first().copied() {
                Some(first) => {
                    if panel.active_tab == tab {
                        panel.active_tab = first;
                    }
                    false
                }
                None => true,
            }
        };

        {
            let panel = self
                .panel_mut(to_panel)
                .ok_or(DockModelError::UnknownPanel(to_panel))?;
            let at = index.min(panel.tabs.len());
            panel.tabs.insert(at, tab);
        }

        let removed_source_panel = if source_emptied {
            self.remove_panel(source)?;
            Some(source)
        } else {
            None
        };

        Ok(MoveTabOutcome {
            source_panel: source,
            removed_source_panel,
        })
    }

    /// Moves `tab` out of its panel into a FREE panel of its own at `pos`
    /// (host-area coordinates) — what dropping a tab header on bare dock area
    /// does (plan §4.8, requirement 8).
    ///
    /// When the tab is the last one of its panel, no new panel is created: the
    /// source panel itself is freed and moved to `pos`. Creating one would give
    /// the same single-tab panel a new id for no reason and destroy whatever
    /// hangs off it.
    ///
    /// # Errors
    /// [`DockModelError::UnknownTab`] if no panel owns `tab`, and
    /// [`DockModelError::PanelIdOverflow`] if no panel id is left. The layout is
    /// unchanged on every error.
    pub fn detach_tab(
        &mut self,
        tab: TabId,
        pos: Pos2,
    ) -> Result<DetachTabOutcome, DockModelError> {
        let source = self
            .panel_of_tab(tab)
            .ok_or(DockModelError::UnknownTab(tab))?;
        let host = self
            .panel(source)
            .ok_or(DockModelError::UnknownPanel(source))?
            .host;
        self.detach_tab_to_host(tab, host, pos)
    }

    /// Moves `tab` into a panel of its own inside `host`, at `pos` (that host's
    /// area coordinates).
    ///
    /// The cross-window generalisation of [`DockLayout::detach_tab`]: it is what
    /// a tab dropped into another of our windows, and a tab dragged out of every
    /// window, both do (plan §4.8b, requirement 9).
    ///
    /// When the tab is the last one of its panel AND the host does not change,
    /// no new panel is created: the source panel itself is freed and moved, so a
    /// lone panel keeps its id and whatever hangs off it. A change of host always
    /// produces a NEW panel instead — a panel that carries dependants cannot
    /// cross hosts without leaving them anchored across windows, which
    /// [`DockModelError::CrossHostAnchor`] forbids.
    ///
    /// # Errors
    /// [`DockModelError::UnknownTab`] if no panel owns `tab`, and
    /// [`DockModelError::PanelIdOverflow`] if no panel id is left. The layout is
    /// unchanged on every error.
    pub fn detach_tab_to_host(
        &mut self,
        tab: TabId,
        host: HostId,
        pos: Pos2,
    ) -> Result<DetachTabOutcome, DockModelError> {
        let source = self
            .panel_of_tab(tab)
            .ok_or(DockModelError::UnknownTab(tab))?;
        let source_node = self
            .panel(source)
            .ok_or(DockModelError::UnknownPanel(source))?;
        let source_host = source_node.host;
        let was_last_tab = source_node.tabs.len() <= 1;
        if was_last_tab && source_host == host {
            let panel = self
                .panel_mut(source)
                .ok_or(DockModelError::UnknownPanel(source))?;
            panel.anchor = PanelAnchor::Free;
            panel.pos = pos;
            return Ok(DetachTabOutcome {
                panel: source,
                created: false,
                source_panel: source,
                removed_source_panel: None,
            });
        }

        // Everything that can fail is done before the layout is touched: the id
        // and the new node are obtained first, so the two mutations below cannot
        // leave the tab owned by nobody.
        let id = self.next_panel_id()?;
        let mut node = PanelNode::new(id, host, vec![tab])?;
        node.pos = pos;

        let removed_source_panel = if was_last_tab {
            // `edit` would refuse the intermediate state (a panel with no tabs),
            // so the emptied source is removed outright; its dependants inherit
            // its own anchor, exactly as they do for any other removal.
            self.remove_panel(source)?;
            Some(source)
        } else {
            self.edit(source, |panel| {
                panel.tabs.retain(|owned| *owned != tab);
                if panel.active_tab == tab
                    && let Some(first) = panel.tabs.first().copied()
                {
                    panel.active_tab = first;
                }
            })?;
            None
        };
        self.insert_panel(node)?;
        Ok(DetachTabOutcome {
            panel: id,
            created: true,
            source_panel: source,
            removed_source_panel,
        })
    }

    /// Moves one panel into `host`, placing it free-floating at `pos` (that
    /// host's area coordinates).
    ///
    /// The panel leaves whatever it was attached to, and its dependants inherit
    /// its own anchor — the same rule [`DockLayout::remove_panel`] applies,
    /// because from the old host's point of view the panel is gone. Moving a
    /// panel to the host it already lives in only repositions it.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] if `id` is not in this layout.
    pub fn move_panel_to_host(
        &mut self,
        id: PanelId,
        host: HostId,
        pos: Pos2,
    ) -> Result<(), DockModelError> {
        let node = self.panel(id).ok_or(DockModelError::UnknownPanel(id))?;
        if node.host == host {
            return self.set_panel_pos(id, pos);
        }
        let inherited = node.anchor;
        for panel in &mut self.panels {
            if panel.id != id && panel.anchor.target_panel() == Some(id) {
                panel.anchor = inherited;
            }
        }
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        panel.host = host;
        panel.anchor = PanelAnchor::Free;
        panel.pos = pos;
        Ok(())
    }

    /// Moves EVERY panel of `from` into `to`, keeping their arrangement, and
    /// returns how many panels moved.
    ///
    /// Anchors survive untouched on purpose: a `PanelAnchor::Panel` inside one
    /// host can only target a panel of that same host, and the whole host moves
    /// at once, so no anchor ends up crossing windows. This is what closing a
    /// sub-window does with the panels it held (requirement 10) — the user's
    /// tabs come back to the main window instead of disappearing with it.
    pub fn rehost_panels(&mut self, from: HostId, to: HostId) -> usize {
        if from == to {
            return 0;
        }
        let mut moved = 0usize;
        for panel in &mut self.panels {
            if panel.host == from {
                panel.host = to;
                moved += 1;
            }
        }
        moved
    }

    /// `true` when at least one panel of this layout is drawn in `host`.
    #[must_use]
    pub fn has_panels_in_host(&self, host: HostId) -> bool {
        self.panels.iter().any(|panel| panel.host == host)
    }

    /// Every sub-window index this layout addresses, ascending.
    ///
    /// The question "is this sub-window still needed?" (requirement 10) is asked
    /// against the union of these sets over every program tab's layout.
    #[must_use]
    pub fn sub_window_indices(&self) -> BTreeSet<u32> {
        self.panels
            .iter()
            .filter_map(|panel| match panel.host {
                HostId::MainWindow => None,
                HostId::SubWindow(index) => Some(index),
            })
            .collect()
    }

    /// `true` when `panel` hangs, directly or transitively, off `ancestor`.
    ///
    /// The question the docking gesture asks before offering a snap target:
    /// anchoring a panel to one of its own dependants closes a cycle, which
    /// [`DockLayout::set_anchor`] refuses. The walk is bounded by the panel
    /// count, so a layout that already contains a cycle terminates too.
    #[must_use]
    pub fn is_anchored_to(&self, panel: PanelId, ancestor: PanelId) -> bool {
        let mut current = panel;
        for _ in 0..self.panels.len() {
            let Some(parent) = self.parent_of(current) else {
                return false;
            };
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }

    /// Re-anchors a panel, refusing anchors that would break the forest.
    ///
    /// # Errors
    /// [`DockModelError::UnknownPanel`] for an unknown panel or anchor target,
    /// [`DockModelError::CrossHostAnchor`] when the target lives in another
    /// window, and [`DockModelError::CycleDetected`] when the target is the
    /// panel itself or one of its dependants.
    pub fn set_anchor(&mut self, id: PanelId, anchor: PanelAnchor) -> Result<(), DockModelError> {
        let host = self
            .panel(id)
            .ok_or(DockModelError::UnknownPanel(id))?
            .host;
        self.check_anchor(id, host, anchor)?;
        let panel = self.panel_mut(id).ok_or(DockModelError::UnknownPanel(id))?;
        panel.anchor = anchor;
        Ok(())
    }

    /// Splits the layout into connected components of the anchor graph.
    ///
    /// Two panels are in one chain when one is (transitively) anchored to the
    /// other. Free panels form single-element chains. The result is
    /// deterministic: each chain is sorted by panel id, and the chains are
    /// ordered by their smallest id.
    #[must_use]
    pub fn chains(&self) -> Vec<Vec<PanelId>> {
        let mut neighbours: BTreeMap<PanelId, BTreeSet<PanelId>> = self
            .panels
            .iter()
            .map(|panel| (panel.id, BTreeSet::new()))
            .collect();
        for panel in &self.panels {
            if let Some(target) = panel.anchor.target_panel() {
                // An anchor to a panel outside this layout is ignored here; it
                // is reported by `validate` and treated as "free" by the solver.
                if neighbours.contains_key(&target) {
                    if let Some(set) = neighbours.get_mut(&panel.id) {
                        set.insert(target);
                    }
                    if let Some(set) = neighbours.get_mut(&target) {
                        set.insert(panel.id);
                    }
                }
            }
        }

        let mut visited: BTreeSet<PanelId> = BTreeSet::new();
        let mut chains: Vec<Vec<PanelId>> = Vec::new();
        for start in neighbours.keys().copied() {
            if !visited.insert(start) {
                continue;
            }
            let mut chain = vec![start];
            let mut queue = VecDeque::from([start]);
            while let Some(current) = queue.pop_front() {
                let Some(adjacent) = neighbours.get(&current) else {
                    continue;
                };
                for next in adjacent.iter().copied() {
                    if visited.insert(next) {
                        chain.push(next);
                        queue.push_back(next);
                    }
                }
            }
            chain.sort_unstable();
            chains.push(chain);
        }
        chains
    }

    /// Returns the panel `id` is anchored to, if any.
    #[must_use]
    pub fn parent_of(&self, id: PanelId) -> Option<PanelId> {
        self.panel(id).and_then(|panel| panel.anchor.target_panel())
    }

    /// Verifies every layout invariant.
    ///
    /// # Errors
    /// The first violation found, scanning panels in ascending id order so the
    /// reported error is deterministic. See [`DockLayout`] for the list of
    /// invariants and [`DockModelError`] for the variants.
    pub fn validate(&self) -> Result<(), DockModelError> {
        let mut ids: BTreeSet<PanelId> = BTreeSet::new();
        for panel in &self.panels {
            if !ids.insert(panel.id) {
                return Err(DockModelError::DuplicatePanelId(panel.id));
            }
        }

        let mut ordered: Vec<&PanelNode> = self.panels.iter().collect();
        ordered.sort_by_key(|panel| panel.id);

        let mut owners: BTreeMap<TabId, PanelId> = BTreeMap::new();
        for panel in &ordered {
            panel.validate()?;
            for tab in &panel.tabs {
                if let Some(previous) = owners.insert(*tab, panel.id) {
                    return Err(DockModelError::DuplicateTab {
                        tab: *tab,
                        panel: previous,
                    });
                }
            }
        }

        for panel in &ordered {
            if let Some(target) = panel.anchor.target_panel() {
                if target == panel.id {
                    return Err(DockModelError::CycleDetected {
                        panel: panel.id,
                        target,
                    });
                }
                let target_node = self
                    .panel(target)
                    .ok_or(DockModelError::UnknownPanel(target))?;
                if target_node.host != panel.host {
                    return Err(DockModelError::CrossHostAnchor {
                        panel: panel.id,
                        target,
                    });
                }
            }
        }

        for panel in &ordered {
            // Walk up the anchor chain; revisiting a panel means the chain
            // closes on itself. The visited set is per walk, so this is O(n^2)
            // in the worst case — panel counts here are in the tens.
            let mut seen: BTreeSet<PanelId> = BTreeSet::from([panel.id]);
            let mut current = panel.id;
            while let Some(parent) = self.parent_of(current) {
                if !seen.insert(parent) {
                    return Err(DockModelError::CycleDetected {
                        panel: panel.id,
                        target: parent,
                    });
                }
                current = parent;
            }
        }

        Ok(())
    }

    /// Validates a prospective anchor for `panel` (which may not be in the
    /// layout yet) without mutating anything.
    fn check_anchor(
        &self,
        panel: PanelId,
        host: HostId,
        anchor: PanelAnchor,
    ) -> Result<(), DockModelError> {
        let Some(target) = anchor.target_panel() else {
            return Ok(());
        };
        if target == panel {
            return Err(DockModelError::CycleDetected { panel, target });
        }
        let target_node = self
            .panel(target)
            .ok_or(DockModelError::UnknownPanel(target))?;
        if target_node.host != host {
            return Err(DockModelError::CrossHostAnchor { panel, target });
        }
        // Walking up from the target must not lead back to `panel`: that is
        // exactly the condition for the new edge to close a loop.
        let mut seen: BTreeSet<PanelId> = BTreeSet::from([target]);
        let mut current = target;
        while let Some(parent) = self.parent_of(current) {
            if parent == panel {
                return Err(DockModelError::CycleDetected { panel, target });
            }
            if !seen.insert(parent) {
                // The layout already contained a cycle above the target.
                return Err(DockModelError::CycleDetected {
                    panel: current,
                    target: parent,
                });
            }
            current = parent;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB_A: TabId = TabId::new("test.a");
    const TAB_B: TabId = TabId::new("test.b");
    const TAB_C: TabId = TabId::new("test.c");
    const TAB_D: TabId = TabId::new("test.d");

    fn panel(id: u32, tabs: &[TabId]) -> PanelNode {
        PanelNode::new(PanelId::new(id), HostId::MainWindow, tabs.to_vec())
            .expect("test panel must be constructible")
    }

    fn anchored(id: u32, tabs: &[TabId], target: u32, edge: DockEdge) -> PanelNode {
        let mut node = panel(id, tabs);
        node.anchor = PanelAnchor::Panel {
            target: PanelId::new(target),
            edge,
            align: 0.0,
        };
        node
    }

    #[test]
    fn dock_edge_opposite_and_orientation() {
        assert_eq!(DockEdge::Top.opposite(), DockEdge::Bottom);
        assert_eq!(DockEdge::Left.opposite(), DockEdge::Right);
        assert!(DockEdge::Top.is_vertical());
        assert!(!DockEdge::Top.is_horizontal());
        assert!(DockEdge::Right.is_horizontal());
        assert!(!DockEdge::Right.is_vertical());
    }

    #[test]
    fn panel_node_rejects_empty_and_duplicate_tabs() {
        assert_eq!(
            PanelNode::new(PanelId::new(0), HostId::MainWindow, Vec::new()),
            Err(DockModelError::EmptyPanel(PanelId::new(0)))
        );
        assert_eq!(
            PanelNode::new(PanelId::new(1), HostId::MainWindow, vec![TAB_A, TAB_A]),
            Err(DockModelError::DuplicateTab {
                tab: TAB_A,
                panel: PanelId::new(1),
            })
        );
    }

    #[test]
    fn active_tab_must_belong_to_the_panel() {
        let mut node = panel(0, &[TAB_A]);
        node.active_tab = TAB_B;
        assert_eq!(
            node.validate(),
            Err(DockModelError::ActiveTabNotInPanel {
                panel: PanelId::new(0),
                tab: TAB_B,
            })
        );
    }

    #[test]
    fn one_tab_cannot_live_in_two_panels() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("first insert");
        assert_eq!(
            layout.insert_panel(panel(1, &[TAB_A, TAB_B])),
            Err(DockModelError::DuplicateTab {
                tab: TAB_A,
                panel: PanelId::new(0),
            })
        );
        // The rejected insert must not have touched the layout.
        assert_eq!(layout.panels().len(), 1);
        assert_eq!(layout.panel_of_tab(TAB_B), None);
    }

    #[test]
    fn duplicate_panel_id_is_rejected() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(3, &[TAB_A])).expect("first insert");
        assert_eq!(
            layout.insert_panel(panel(3, &[TAB_B])),
            Err(DockModelError::DuplicatePanelId(PanelId::new(3)))
        );
    }

    #[test]
    fn next_panel_id_is_free() {
        let mut layout = DockLayout::new();
        assert_eq!(layout.next_panel_id(), Ok(PanelId::new(0)));
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout.insert_panel(panel(7, &[TAB_B])).expect("insert 7");
        assert_eq!(layout.next_panel_id(), Ok(PanelId::new(8)));
    }

    #[test]
    fn next_panel_id_reports_overflow_instead_of_wrapping() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(u32::MAX, &[TAB_A]))
            .expect("insert max");
        assert_eq!(layout.next_panel_id(), Err(DockModelError::PanelIdOverflow));
    }

    #[test]
    fn cycle_is_detected_by_validate() {
        // 0 -> 1 -> 0 must be rejected when the whole list is validated.
        let layout = DockLayout::from_panels(vec![
            anchored(0, &[TAB_A], 1, DockEdge::Bottom),
            anchored(1, &[TAB_B], 0, DockEdge::Bottom),
        ]);
        assert_eq!(
            layout,
            Err(DockModelError::CycleDetected {
                panel: PanelId::new(0),
                target: PanelId::new(0),
            })
        );
    }

    #[test]
    fn set_anchor_refuses_to_close_a_loop() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout
            .insert_panel(anchored(2, &[TAB_C], 1, DockEdge::Bottom))
            .expect("insert 2");

        // 0 -> 2 would close 0 -> 2 -> 1 -> 0.
        assert_eq!(
            layout.set_anchor(
                PanelId::new(0),
                PanelAnchor::Panel {
                    target: PanelId::new(2),
                    edge: DockEdge::Top,
                    align: 0.0,
                },
            ),
            Err(DockModelError::CycleDetected {
                panel: PanelId::new(0),
                target: PanelId::new(2),
            })
        );
        assert_eq!(layout.validate(), Ok(()));
        // Self-anchoring is the degenerate case of the same rule.
        assert_eq!(
            layout.set_anchor(
                PanelId::new(0),
                PanelAnchor::Panel {
                    target: PanelId::new(0),
                    edge: DockEdge::Top,
                    align: 0.0,
                },
            ),
            Err(DockModelError::CycleDetected {
                panel: PanelId::new(0),
                target: PanelId::new(0),
            })
        );
    }

    #[test]
    fn cross_host_anchor_is_rejected() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        let mut sub = PanelNode::new(PanelId::new(1), HostId::SubWindow(0), vec![TAB_B])
            .expect("sub panel");
        sub.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        assert_eq!(
            layout.insert_panel(sub),
            Err(DockModelError::CrossHostAnchor {
                panel: PanelId::new(1),
                target: PanelId::new(0),
            })
        );
    }

    #[test]
    fn removing_a_middle_panel_reattaches_its_dependants() {
        // 0 <- 1 <- 2, 1 also carries 3. Removing 1 must leave 2 and 3 anchored
        // to 0 with 1's own anchor.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout
            .insert_panel(anchored(2, &[TAB_C], 1, DockEdge::Bottom))
            .expect("insert 2");
        layout
            .insert_panel(anchored(3, &[TAB_D], 1, DockEdge::Right))
            .expect("insert 3");

        let removed = layout.remove_panel(PanelId::new(1)).expect("remove 1");
        assert_eq!(removed.id, PanelId::new(1));
        let inherited = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        for id in [2_u32, 3] {
            let node = layout.panel(PanelId::new(id)).expect("dependant survives");
            assert_eq!(node.anchor, inherited);
        }
        assert_eq!(layout.validate(), Ok(()));
        assert_eq!(layout.chains(), vec![vec![
            PanelId::new(0),
            PanelId::new(2),
            PanelId::new(3)
        ]]);
    }

    #[test]
    fn removing_a_free_root_frees_its_dependants() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout.remove_panel(PanelId::new(0)).expect("remove root");
        let node = layout.panel(PanelId::new(1)).expect("dependant survives");
        assert_eq!(node.anchor, PanelAnchor::Free);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn remove_unknown_panel_reports_it() {
        let mut layout = DockLayout::new();
        assert_eq!(
            layout.remove_panel(PanelId::new(4)),
            Err(DockModelError::UnknownPanel(PanelId::new(4)))
        );
    }

    #[test]
    fn move_tab_between_panels_transfers_ownership() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout.insert_panel(panel(1, &[TAB_C])).expect("insert 1");

        let outcome = layout
            .move_tab(TAB_A, PanelId::new(1), 0)
            .expect("move A into panel 1");
        assert_eq!(outcome.source_panel, PanelId::new(0));
        assert_eq!(outcome.removed_source_panel, None);

        let source = layout.panel(PanelId::new(0)).expect("source survives");
        assert_eq!(source.tabs, vec![TAB_B]);
        // The moved tab was active, so the survivor took over.
        assert_eq!(source.active_tab, TAB_B);
        let target = layout.panel(PanelId::new(1)).expect("target");
        assert_eq!(target.tabs, vec![TAB_A, TAB_C]);
        assert_eq!(layout.panel_of_tab(TAB_A), Some(PanelId::new(1)));
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn move_tab_never_duplicates_a_tab() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout.insert_panel(panel(1, &[TAB_C])).expect("insert 1");
        layout
            .move_tab(TAB_A, PanelId::new(1), 5)
            .expect("index is clamped, not rejected");
        // Moving it again from its new owner must not leave a copy behind.
        layout
            .move_tab(TAB_A, PanelId::new(0), 0)
            .expect("move back");
        let occurrences: usize = layout
            .panels()
            .iter()
            .map(|node| node.tabs.iter().filter(|tab| **tab == TAB_A).count())
            .sum();
        assert_eq!(occurrences, 1);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn move_tab_inside_one_panel_reorders() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(0, &[TAB_A, TAB_B, TAB_C]))
            .expect("insert 0");
        let outcome = layout
            .move_tab(TAB_A, PanelId::new(0), 2)
            .expect("reorder inside the panel");
        assert_eq!(outcome.removed_source_panel, None);
        let node = layout.panel(PanelId::new(0)).expect("panel");
        assert_eq!(node.tabs, vec![TAB_B, TAB_C, TAB_A]);
        assert_eq!(node.active_tab, TAB_A);
    }

    #[test]
    fn emptying_a_panel_by_moving_removes_it_and_reattaches_children() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout
            .insert_panel(anchored(2, &[TAB_C], 1, DockEdge::Bottom))
            .expect("insert 2");

        let outcome = layout
            .move_tab(TAB_B, PanelId::new(0), 1)
            .expect("move the only tab of panel 1 away");
        assert_eq!(outcome.removed_source_panel, Some(PanelId::new(1)));
        assert_eq!(layout.panel(PanelId::new(1)), None);
        let survivor = layout.panel(PanelId::new(2)).expect("child survives");
        assert_eq!(
            survivor.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn move_tab_reports_unknown_inputs() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        assert_eq!(
            layout.move_tab(TAB_B, PanelId::new(0), 0),
            Err(DockModelError::UnknownTab(TAB_B))
        );
        assert_eq!(
            layout.move_tab(TAB_A, PanelId::new(9), 0),
            Err(DockModelError::UnknownPanel(PanelId::new(9)))
        );
        // Both failures must leave the layout untouched.
        assert_eq!(layout.panel_of_tab(TAB_A), Some(PanelId::new(0)));
    }

    #[test]
    fn detaching_the_last_tab_reuses_its_panel_instead_of_creating_one() {
        // Dropping the only tab of a panel on bare dock area must not churn the
        // panel id: everything anchored to it would be re-anchored for nothing.
        let mut layout = DockLayout::new();
        let mut only = panel(0, &[TAB_A]);
        only.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.5,
        };
        layout.insert_panel(only).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");

        let outcome = layout
            .detach_tab(TAB_A, Pos2::new(120.0, 90.0))
            .expect("the tab exists");
        assert_eq!(outcome, DetachTabOutcome {
            panel: PanelId::new(0),
            created: false,
            source_panel: PanelId::new(0),
            removed_source_panel: None,
        });
        let node = layout.panel(PanelId::new(0)).expect("panel survives");
        assert_eq!(node.anchor, PanelAnchor::Free);
        assert_eq!(node.pos, Pos2::new(120.0, 90.0));
        assert_eq!(node.tabs, vec![TAB_A]);
        // Its dependant is untouched.
        assert_eq!(
            layout.panel(PanelId::new(1)).expect("dependant").anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn detaching_one_of_several_tabs_creates_a_free_panel_for_it() {
        let mut layout = DockLayout::new();
        let mut source = panel(0, &[TAB_A, TAB_B]);
        source.active_tab = TAB_B;
        layout.insert_panel(source).expect("insert 0");

        let outcome = layout
            .detach_tab(TAB_B, Pos2::new(40.0, 60.0))
            .expect("the tab exists");
        assert!(outcome.created);
        assert_eq!(outcome.removed_source_panel, None);
        let created = layout.panel(outcome.panel).expect("created panel");
        assert_eq!(created.tabs, vec![TAB_B]);
        assert_eq!(created.active_tab, TAB_B);
        assert_eq!(created.anchor, PanelAnchor::Free);
        assert_eq!(created.pos, Pos2::new(40.0, 60.0));
        let source = layout.panel(PanelId::new(0)).expect("source");
        assert_eq!(source.tabs, vec![TAB_A]);
        // The detached tab was the active one; the survivor took over.
        assert_eq!(source.active_tab, TAB_A);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn detaching_an_unknown_tab_changes_nothing() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        assert_eq!(
            layout.detach_tab(TAB_B, Pos2::ZERO),
            Err(DockModelError::UnknownTab(TAB_B))
        );
        assert_eq!(layout.panels().len(), 1);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn is_anchored_to_walks_the_whole_chain() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout
            .insert_panel(anchored(2, &[TAB_C], 1, DockEdge::Bottom))
            .expect("insert 2");
        layout.insert_panel(panel(3, &[TAB_D])).expect("insert 3");

        assert!(layout.is_anchored_to(PanelId::new(2), PanelId::new(0)));
        assert!(layout.is_anchored_to(PanelId::new(1), PanelId::new(0)));
        assert!(!layout.is_anchored_to(PanelId::new(0), PanelId::new(2)));
        assert!(!layout.is_anchored_to(PanelId::new(3), PanelId::new(0)));
        // Not a reflexive relation: a panel does not hang off itself.
        assert!(!layout.is_anchored_to(PanelId::new(0), PanelId::new(0)));
    }

    #[test]
    fn chains_split_into_connected_components() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout.insert_panel(panel(2, &[TAB_C])).expect("insert 2");
        let mut viewport = panel(3, &[TAB_D]);
        viewport.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        layout.insert_panel(viewport).expect("insert 3");

        assert_eq!(
            layout.chains(),
            vec![
                vec![PanelId::new(0), PanelId::new(1)],
                vec![PanelId::new(2)],
                vec![PanelId::new(3)],
            ]
        );
    }

    #[test]
    fn the_targeted_setters_change_exactly_what_they_name() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        layout
            .set_panel_pos(PanelId::new(0), Pos2::new(120.0, 40.0))
            .expect("panel exists");
        layout
            .set_size_override(PanelId::new(0), Some(Vec2::new(420.0, 260.0)))
            .expect("panel exists");
        layout
            .set_collapsed(PanelId::new(0), true)
            .expect("panel exists");
        layout
            .set_active_tab(PanelId::new(0), TAB_B)
            .expect("the panel owns the tab");

        let node = layout.panel(PanelId::new(0)).expect("panel");
        assert_eq!(node.pos, Pos2::new(120.0, 40.0));
        assert_eq!(node.size_override, Some(Vec2::new(420.0, 260.0)));
        assert!(node.collapsed);
        assert_eq!(node.active_tab, TAB_B);
        assert_eq!(layout.validate(), Ok(()));

        // A tab of another panel is refused instead of being stored.
        assert_eq!(
            layout.set_active_tab(PanelId::new(0), TAB_C),
            Err(DockModelError::ActiveTabNotInPanel {
                panel: PanelId::new(0),
                tab: TAB_C,
            })
        );
        assert_eq!(
            layout.panel(PanelId::new(0)).expect("panel").active_tab,
            TAB_B
        );
        // An unknown panel is an error, never a silent no-op.
        assert_eq!(
            layout.set_collapsed(PanelId::new(9), true),
            Err(DockModelError::UnknownPanel(PanelId::new(9)))
        );
    }

    #[test]
    fn edit_rolls_back_a_change_that_would_break_an_invariant() {
        // Emptying a panel through a raw `&mut PanelNode` used to be possible and
        // left a panel that can never be drawn — and never leaves the layout,
        // because the driver re-creates a panel for the orphaned tab every frame.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout.insert_panel(panel(1, &[TAB_B])).expect("insert 1");

        assert_eq!(
            layout.edit(PanelId::new(0), |node| node.tabs.clear()),
            Err(DockModelError::EmptyPanel(PanelId::new(0)))
        );
        assert_eq!(
            layout.panel(PanelId::new(0)).expect("panel 0").tabs,
            vec![TAB_A]
        );
        // Reusing another panel's id would give both the same egui id.
        assert_eq!(
            layout.edit(PanelId::new(1), |node| node.id = PanelId::new(0)),
            Err(DockModelError::DuplicatePanelId(PanelId::new(0)))
        );
        // Stealing a tab owned elsewhere is caught by the same check.
        assert_eq!(
            layout.edit(PanelId::new(1), |node| node.tabs.push(TAB_A)),
            Err(DockModelError::DuplicateTab {
                tab: TAB_A,
                panel: PanelId::new(0),
            })
        );
        assert_eq!(layout.validate(), Ok(()));

        // A legal edit goes through, and reordering tabs is what `edit` is for.
        layout
            .edit(PanelId::new(0), |node| {
                node.tabs.push(TAB_C);
                node.active_tab = TAB_C;
            })
            .expect("a valid edit is applied");
        let node = layout.panel(PanelId::new(0)).expect("panel 0");
        assert_eq!(node.tabs, vec![TAB_A, TAB_C]);
        assert_eq!(node.active_tab, TAB_C);
        assert_eq!(
            layout.edit(PanelId::new(7), |node| node.collapsed = true),
            Err(DockModelError::UnknownPanel(PanelId::new(7)))
        );
    }

    #[test]
    fn chains_of_an_empty_layout_are_empty() {
        assert!(DockLayout::new().chains().is_empty());
        assert_eq!(DockLayout::new().validate(), Ok(()));
    }

    #[test]
    fn a_tab_detached_into_another_host_always_gets_a_new_panel() {
        // Requirement 9: even the LAST tab of a panel gets a fresh panel when it
        // crosses windows — moving the panel itself would leave its dependants
        // anchored across hosts, which `validate` refuses.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");

        let outcome = layout
            .detach_tab_to_host(TAB_A, HostId::SubWindow(0), Pos2::new(8.0, 8.0))
            .expect("the tab can leave");
        assert!(outcome.created);
        assert_eq!(outcome.source_panel, PanelId::new(0));
        assert_eq!(outcome.removed_source_panel, Some(PanelId::new(0)));
        let created = layout.panel(outcome.panel).expect("the new panel");
        assert_eq!(created.host, HostId::SubWindow(0));
        assert_eq!(created.tabs, vec![TAB_A]);
        assert_eq!(created.pos, Pos2::new(8.0, 8.0));
        // The dependant survived in the main window with the removed panel's own
        // anchor, exactly as an ordinary removal leaves it.
        let dependant = layout.panel(PanelId::new(1)).expect("the dependant");
        assert_eq!(dependant.host, HostId::MainWindow);
        assert_eq!(dependant.anchor, PanelAnchor::Free);
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn detaching_into_the_same_host_still_reuses_a_lone_panel() {
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        let outcome = layout
            .detach_tab_to_host(TAB_A, HostId::MainWindow, Pos2::new(40.0, 50.0))
            .expect("the tab can move");
        assert!(!outcome.created);
        assert_eq!(outcome.panel, PanelId::new(0));
        assert_eq!(layout.panels().len(), 1);
    }

    #[test]
    fn a_tab_leaving_a_shared_panel_keeps_the_panel_behind() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(panel(0, &[TAB_A, TAB_B]))
            .expect("insert 0");
        let outcome = layout
            .detach_tab_to_host(TAB_A, HostId::SubWindow(3), Pos2::ZERO)
            .expect("the tab can leave");
        assert!(outcome.created);
        assert_eq!(outcome.removed_source_panel, None);
        let source = layout.panel(PanelId::new(0)).expect("source");
        assert_eq!(source.tabs, vec![TAB_B]);
        assert_eq!(source.active_tab, TAB_B);
        assert_eq!(
            layout.panel(outcome.panel).expect("created").host,
            HostId::SubWindow(3)
        );
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn moving_a_panel_to_another_host_hands_its_anchor_down() {
        // 0 <- 1 <- 2. Moving 1 out must leave 2 anchored to 0 — 1 is gone from
        // that window as far as the remaining chain is concerned.
        let mut layout = DockLayout::new();
        let mut root = panel(0, &[TAB_A]);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        layout.insert_panel(root).expect("insert 0");
        layout
            .insert_panel(anchored(1, &[TAB_B], 0, DockEdge::Bottom))
            .expect("insert 1");
        layout
            .insert_panel(anchored(2, &[TAB_C], 1, DockEdge::Bottom))
            .expect("insert 2");

        layout
            .move_panel_to_host(PanelId::new(1), HostId::SubWindow(0), Pos2::new(8.0, 8.0))
            .expect("the panel can leave");
        let moved = layout.panel(PanelId::new(1)).expect("moved");
        assert_eq!(moved.host, HostId::SubWindow(0));
        assert_eq!(moved.anchor, PanelAnchor::Free);
        assert_eq!(moved.pos, Pos2::new(8.0, 8.0));
        assert_eq!(
            layout.panel(PanelId::new(2)).expect("dependant").anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        assert_eq!(layout.validate(), Ok(()));
        assert!(layout.has_panels_in_host(HostId::SubWindow(0)));
        assert_eq!(
            layout.sub_window_indices(),
            BTreeSet::from([0u32])
        );
    }

    #[test]
    fn rehosting_a_whole_window_keeps_the_arrangement() {
        // Requirement 10: closing a sub-window returns its panels to the main
        // window WITH their attachment, because the whole host moves at once.
        let mut layout = DockLayout::new();
        layout.insert_panel(panel(0, &[TAB_A])).expect("insert 0");
        let mut first = PanelNode::new(PanelId::new(1), HostId::SubWindow(0), vec![TAB_B])
            .expect("sub panel 1");
        first.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Top,
            along: 0.0,
        };
        layout.insert_panel(first).expect("insert 1");
        let mut second = PanelNode::new(PanelId::new(2), HostId::SubWindow(0), vec![TAB_C])
            .expect("sub panel 2");
        second.anchor = PanelAnchor::Panel {
            target: PanelId::new(1),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(second).expect("insert 2");

        assert_eq!(
            layout.rehost_panels(HostId::SubWindow(0), HostId::MainWindow),
            2
        );
        assert!(!layout.has_panels_in_host(HostId::SubWindow(0)));
        assert_eq!(
            layout.panel(PanelId::new(2)).expect("second").anchor,
            PanelAnchor::Panel {
                target: PanelId::new(1),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        assert_eq!(layout.validate(), Ok(()));
        // No tab was lost with the window.
        assert_eq!(layout.panel_of_tab(TAB_B), Some(PanelId::new(1)));
        assert_eq!(layout.panel_of_tab(TAB_C), Some(PanelId::new(2)));
        assert!(layout.sub_window_indices().is_empty());
    }
}
