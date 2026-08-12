/*
File: src/widgets/panel_dock/persist.rs

Purpose:
Persistence of the dockable-panel layouts: the serde mirror of the self-versioned
`PanelLayout` section of `user_config.json`, the two conversions between that
mirror and `DockLayout`, and the coalescing writer thread that owns every write
of the section.

Main responsibilities:
- decode the stored section into one `DockLayout` per program tab, dropping what
  this build can no longer understand instead of wedging the layout;
- encode the live layouts back, touching no key the section does not own;
- keep every write off the GUI thread and off the disk until a gesture settles.

Key structures:
- `StoredSection` / `StoredTabLayout` / `StoredPanel` / `StoredAnchor` /
  `StoredHost` / `StoredSubWindow`: the serde mirror of the section.
- `PanelLayoutSnapshot`: what is restored and what is written, in one value, plus
  the rule two snapshots fold under (`config_saver::SaverPayload`).
- `PanelLayoutWriter`: the handle of this section's `config_saver::ConfigSaver`.
- `PanelLayoutError`: typed failures of the writer.

Key functions:
- `layouts_from_user_settings`, `decode_layout`, `decode_sub_windows`,
  `encode_layout`, `encode_sub_window`
- `persist_layouts`, `PanelLayoutWriter::store`, `PanelLayoutWriter::flush_and_join`

Notes:
This is the ONLY file of `panel_dock/` that touches the disk; `model.rs`,
`solver.rs` and `drag.rs` stay pure and `mod.rs` only draws. Writes go through
`config::update_user_config_file`, the single locked read-modify-write border of
`user_config.json` — never through the `save_*` helpers of the settings tab,
which rewrite the whole root and clobber it on a read failure
(`README_AGENT.md`, "user_config").

DURABILITY. The writer is the LAST owner of a snapshot: `take_dirty_layouts`
clears the dock's `dirty` flag when it hands one over, so a snapshot the writer
drops is gone from the whole process. The policy that keeps it — debounce, hold
and retry with a capped backoff, final attempt on shutdown, "lost" logged as an
error — lives in `config_saver.rs` and is shared with the `Window` section; this
file only supplies the payload's fold rule, the typed error's retryability and
the write step itself.

IDENTITY. A tab is stored under its `TabId` literal and a program tab under its
`AppTab::key()`; neither is ever a localized caption. `PanelId`s are NOT stored:
panels are written in list order and `StoredAnchor::Panel::target` is the INDEX
of the target inside that list, so a load renumbers the panels 0..n and rewrites
the anchors onto the new ids. That keeps the file independent of whatever id
holes a session's inserts and removals left behind.

FORWARD/BACKWARD COMPATIBILITY. A tab the file names but this build does not
declare is dropped (a removed tab must not wedge the layout); a tab this build
declares but the file does not name is re-created by the dock's own
`ensure_declared_tabs`. A section whose `version` is newer than
`PANEL_LAYOUT_SECTION_VERSION` is neither read nor overwritten.

A tag of `StoredAnchor` that the runtime model no longer has must stay
DECODABLE for as long as the section version does not change. The enum is
internally tagged and has no `#[serde(other)]`, and the whole section is decoded
by ONE `from_value`, so an unknown tag is not a per-panel repair but a failure of
the entire section: every program tab would silently fall back to its default
arrangement and the first dirty write would make that permanent
(`StoredAnchor::CanvasControls` is the standing example). Retiring such a tag for
real means bumping the version and writing a migration, never a plain deletion.
*/

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use egui::{Pos2, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::config_saver::{self, ConfigSaver, SaverLabels};
use crate::runtime_log;

use super::model::{DockEdge, DockLayout, HostId, PanelAnchor, PanelId, PanelNode, TabId};
use super::window::SubWindowNode;

/// Top-level `user_config.json` key owned by this module.
pub const PANEL_LAYOUT_SECTION_KEY: &str = "PanelLayout";

/// Schema version of the `PanelLayout` section. The config file has no global
/// version, so the section carries its own (precedent: `fonts_data.rs`).
pub const PANEL_LAYOUT_SECTION_VERSION: u32 = 1;

/// Coordinate/size magnitude beyond which a stored value is considered corrupt.
/// Mirrors `window_geometry::MAX_PERSISTED_COORD`.
const MAX_STORED_COORD: f32 = 65_536.0;

/// One program tab's restore descriptor: its `AppTab::key()` paired with the
/// default-layout builder that names every `TabId` the tab can declare.
///
/// The builder is what [`layouts_from_user_settings`] resolves stored tab keys
/// against; it is never used to replace a stored layout.
pub type LayoutDefault<'a> = (&'a str, fn() -> DockLayout);

/// Typed failures of the `PanelLayout` section writer.
#[derive(Debug, thiserror::Error)]
pub enum PanelLayoutError {
    /// The section on disk declares a schema version this build does not
    /// understand. Rewriting it would silently drop every field that version
    /// added, so the write is refused and the user keeps the newer section.
    #[error(
        "the '{PANEL_LAYOUT_SECTION_KEY}' section of user_config.json declares schema version \
         {found}, newer than the supported {PANEL_LAYOUT_SECTION_VERSION}; refusing to overwrite it"
    )]
    NewerVersion {
        /// The version the on-disk section declares.
        found: u32,
    },
    /// The read-modify-write transaction on `user_config.json` failed. The
    /// payload is the full `anyhow` chain, already suitable for a log line.
    #[error("failed to update the '{PANEL_LAYOUT_SECTION_KEY}' section of user_config.json: {0}")]
    Persist(String),
}

impl config_saver::SaverError for PanelLayoutError {
    /// Whether repeating the same write later could succeed.
    ///
    /// [`PanelLayoutError::Persist`] is transient by assumption — a locked, busy,
    /// momentarily unwritable or unreadable file is the common case, and a
    /// permanent one only costs the capped retries of one session.
    /// [`PanelLayoutError::NewerVersion`] is not: it is a deliberate refusal to
    /// overwrite a section written by a newer build, and it stays true for as
    /// long as that section is on disk, so retrying it would only burn attempts
    /// and log lines.
    fn is_retryable(&self) -> bool {
        match self {
            Self::NewerVersion { .. } => false,
            Self::Persist(_) => true,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Serde mirror of the section
// ---------------------------------------------------------------------------------------

/// One of the four sides, as stored. A separate type from [`DockEdge`] so the
/// wire names are fixed here and cannot drift with a refactor of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl From<DockEdge> for StoredEdge {
    fn from(edge: DockEdge) -> Self {
        match edge {
            DockEdge::Top => Self::Top,
            DockEdge::Bottom => Self::Bottom,
            DockEdge::Left => Self::Left,
            DockEdge::Right => Self::Right,
        }
    }
}

impl From<StoredEdge> for DockEdge {
    fn from(edge: StoredEdge) -> Self {
        match edge {
            StoredEdge::Top => Self::Top,
            StoredEdge::Bottom => Self::Bottom,
            StoredEdge::Left => Self::Left,
            StoredEdge::Right => Self::Right,
        }
    }
}

/// What a stored panel is attached to.
///
/// `target` is the INDEX of the target panel inside the same `panels` array, not
/// a `PanelId`: ids are re-assigned on load (see the file header's IDENTITY
/// note).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAnchor {
    /// Free-floating; `pos` is authoritative.
    #[default]
    Free,
    /// Attached outside another panel of the same list.
    Panel {
        target: u32,
        edge: StoredEdge,
        align: f32,
    },
    /// Attached inside the host area, flush with one of its sides.
    ViewportEdge { edge: StoredEdge, along: f32 },
    /// LEGACY INPUT ONLY — never produced by [`encode_anchor`].
    ///
    /// Written by the builds in which the canvas' controls panel was not a dock
    /// panel but a rect the solver received as an input
    /// (`PanelAnchor::CanvasControls`, removed with the «Лента» tab). The variant
    /// survives here because this enum is internally tagged and carries no
    /// `#[serde(other)]`: the WHOLE section is decoded by one `from_value`, so a
    /// tag serde does not know fails the entire section, and every layout of
    /// every program tab would silently reset to its default.
    ///
    /// Scope: decoding only, mapped by [`decode_anchor`]. **Removable** once no
    /// supported upgrade path can still carry a section written by such a build —
    /// which needs a section version bump plus a migration, not a plain deletion.
    CanvasControls { edge: StoredEdge, along: f32 },
}

/// The window a stored panel is drawn in.
///
/// A `SubWindow` index addresses an entry of the section's own `sub_windows`
/// list; a panel naming an index that list does not carry is drawn in the main
/// window instead of being lost (see [`decode_layout`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredHost {
    /// The studio's main window.
    #[default]
    Main,
    /// A detached OS window, addressed by its index in `sub_windows`.
    SubWindow(u32),
}

/// One stored sub-window.
///
/// `pos` is the OUTER position in monitor space and is absent on a platform that
/// does not report window positions (Wayland); `size` is the INNER size. Both are
/// in points, which is what `ViewportBuilder::with_position` / `with_inner_size`
/// consume.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct StoredSubWindow {
    /// Identity; matches the `host` of every panel drawn in this window.
    index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pos: Option<[f32; 2]>,
    #[serde(default)]
    size: [f32; 2],
}

/// One stored panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredPanel {
    /// Tab keys in header order; a panel whose every key is unknown is dropped.
    tabs: Vec<String>,
    /// Key of the drawn tab. An absent or unresolvable value falls back to the
    /// first surviving tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<String>,
    #[serde(default)]
    anchor: StoredAnchor,
    /// Top-left corner in host-area coordinates.
    #[serde(default)]
    pos: [f32; 2],
    /// Outer size pinned by a manual resize. Absent means "follow the tabs'
    /// measured content size" — a behavioural difference (it also decides which
    /// panels the solver shrinks first), not a default, so it is stored as an
    /// option rather than as an unconditional pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<[f32; 2]>,
    #[serde(default)]
    collapsed: bool,
    #[serde(default)]
    host: StoredHost,
}

/// The panel arrangement of one program tab.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct StoredTabLayout {
    #[serde(default)]
    panels: Vec<StoredPanel>,
}

/// The whole `PanelLayout` section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct StoredSection {
    /// Schema version; see [`PANEL_LAYOUT_SECTION_VERSION`]. `None` means
    /// "written before the field existed" and is treated as version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    /// One entry per program tab, keyed by `AppTab::key()`.
    #[serde(default)]
    tabs: BTreeMap<String, StoredTabLayout>,
    /// The detached windows, shared by every program tab: a sub-window outlives a
    /// tab switch while the panels inside it are per-tab.
    #[serde(default)]
    sub_windows: Vec<StoredSubWindow>,
}

// ---------------------------------------------------------------------------------------
// Decoding (pure)
// ---------------------------------------------------------------------------------------

/// Clamps one stored coordinate; a non-finite value degrades to `0.0` rather
/// than reaching the solver.
fn sane_coord(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-MAX_STORED_COORD, MAX_STORED_COORD)
    } else {
        0.0
    }
}

/// Decodes a stored size override, rejecting anything that cannot describe a
/// drawable panel.
fn sane_size(raw: [f32; 2]) -> Option<Vec2> {
    let (w, h) = (raw[0], raw[1]);
    let usable = |v: f32| v.is_finite() && v > 0.0 && v <= MAX_STORED_COORD;
    (usable(w) && usable(h)).then(|| Vec2::new(w, h))
}

/// Tab keys the caller's default layout for a program tab declares.
///
/// **Contract:** the default layout of a program tab must name every `TabId`
/// that tab can declare — it is the set a stored key is resolved against, and a
/// key missing from it would be dropped from the user's arrangement on every
/// load.
fn known_tabs(default_layout: &DockLayout) -> BTreeMap<&'static str, TabId> {
    default_layout
        .panels()
        .iter()
        .flat_map(|panel| panel.tabs.iter())
        .map(|tab| (tab.as_str(), *tab))
        .collect()
}

/// One stored panel that survived tab resolution, before ids are assigned.
#[derive(Debug)]
struct PendingPanel {
    tabs: Vec<TabId>,
    active: TabId,
    anchor: StoredAnchor,
    pos: Pos2,
    size: Option<Vec2>,
    collapsed: bool,
    host: HostId,
}

/// Rebuilds one program tab's [`DockLayout`] from its stored form.
///
/// Returns `None` — always with a logged reason — when the stored arrangement
/// cannot be trusted at all: an empty panel list, a panel whose own invariants
/// are broken (a tab listed twice), or a layout `DockLayout::validate` rejects
/// (a tab owned by two panels, an anchor cycle). The caller then keeps the
/// default layout, which is the only other arrangement known to be usable.
///
/// Repairs applied instead of rejecting, each logged:
/// * a tab key absent from `known` is dropped, and a panel left without tabs
///   with it — a tab removed from the program must not wedge the layout;
/// * an `active` key that no surviving tab matches falls back to the first tab;
/// * an anchor whose target index does not address a surviving panel degrades
///   to `Free`, so the panel stays reachable instead of disappearing;
/// * a `host` naming a sub-window the section does not carry is remapped to the
///   main window — a panel in a window that is never opened would make its tabs
///   unreachable AND unrecoverable, because the dock would not re-create them
///   either.
fn decode_layout(
    stored: &StoredTabLayout,
    key: &str,
    known: &BTreeMap<&'static str, TabId>,
    sub_windows: &BTreeSet<u32>,
) -> Option<DockLayout> {
    // Pass 1: resolve tab keys. A panel is kept only if at least one of its tabs
    // still exists in this build.
    let mut pending: Vec<Option<PendingPanel>> = Vec::with_capacity(stored.panels.len());
    for (index, panel) in stored.panels.iter().enumerate() {
        let mut tabs: Vec<TabId> = Vec::with_capacity(panel.tabs.len());
        for name in &panel.tabs {
            match known.get(name.as_str()) {
                Some(tab) => tabs.push(*tab),
                None => runtime_log::log_warn(format!(
                    "[panel_dock::persist] `{key}`: dropping the unknown tab `{name}` stored in \
                     panel #{index}; this build does not declare it"
                )),
            }
        }
        let Some(first) = tabs.first().copied() else {
            if !panel.tabs.is_empty() {
                runtime_log::log_warn(format!(
                    "[panel_dock::persist] `{key}`: dropping stored panel #{index}; none of its \
                     tabs exists in this build"
                ));
            }
            pending.push(None);
            continue;
        };
        let active = match panel.active.as_deref() {
            Some(name) => known
                .get(name)
                .copied()
                .filter(|tab| tabs.contains(tab))
                .unwrap_or_else(|| {
                    runtime_log::log_warn(format!(
                        "[panel_dock::persist] `{key}`: stored panel #{index} names `{name}` as \
                         its active tab, which it does not own any more; falling back to \
                         `{first}`"
                    ));
                    first
                }),
            None => first,
        };
        let host = match panel.host {
            StoredHost::Main => HostId::MainWindow,
            StoredHost::SubWindow(sub) if sub_windows.contains(&sub) => HostId::SubWindow(sub),
            StoredHost::SubWindow(sub) => {
                runtime_log::log_warn(format!(
                    "[panel_dock::persist] `{key}`: stored panel #{index} belongs to sub-window \
                     {sub}, which the stored window list does not describe; drawing it in the \
                     main window"
                ));
                HostId::MainWindow
            }
        };
        pending.push(Some(PendingPanel {
            tabs,
            active,
            anchor: panel.anchor,
            pos: Pos2::new(sane_coord(panel.pos[0]), sane_coord(panel.pos[1])),
            size: panel.size.and_then(sane_size),
            collapsed: panel.collapsed,
            host,
        }));
    }

    // Pass 2: assign ids in list order and rewrite every anchor target from the
    // stored index onto the new id. Done after the drop pass, because dropping a
    // panel shifts every index behind it.
    let mut new_index: Vec<Option<u32>> = Vec::with_capacity(pending.len());
    let mut next = 0u32;
    for slot in &pending {
        if slot.is_some() {
            new_index.push(Some(next));
            next = next.saturating_add(1);
        } else {
            new_index.push(None);
        }
    }

    let mut nodes: Vec<PanelNode> = Vec::with_capacity(pending.len());
    for (index, slot) in pending.into_iter().enumerate() {
        let Some(panel) = slot else {
            continue;
        };
        let Some(id) = new_index.get(index).copied().flatten() else {
            continue;
        };
        let mut node = match PanelNode::new(PanelId::new(id), panel.host, panel.tabs) {
            Ok(node) => node,
            Err(error) => {
                runtime_log::log_warn(format!(
                    "[panel_dock::persist] `{key}`: stored panel #{index} is invalid ({error}); \
                     falling back to the default layout"
                ));
                return None;
            }
        };
        node.anchor = decode_anchor(panel.anchor, &new_index, key, index);
        node.pos = panel.pos;
        node.size_override = panel.size;
        node.collapsed = panel.collapsed;
        node.active_tab = panel.active;
        nodes.push(node);
    }

    if nodes.is_empty() {
        runtime_log::log_warn(format!(
            "[panel_dock::persist] `{key}`: the stored layout holds no usable panel; falling back \
             to the default layout"
        ));
        return None;
    }

    match DockLayout::from_panels(nodes) {
        Ok(layout) => Some(layout),
        Err(error) => {
            runtime_log::log_warn(format!(
                "[panel_dock::persist] `{key}`: the stored layout breaks a model invariant \
                 ({error}); falling back to the default layout"
            ));
            None
        }
    }
}

/// Rewrites one stored anchor onto the ids assigned by [`decode_layout`].
///
/// A `Panel` anchor whose target was dropped, is out of range, or is the panel
/// itself degrades to `Free`: the panel keeps its stored position and stays
/// draggable, which is strictly better than dropping it.
///
/// [`StoredAnchor::CanvasControls`] — a build in which the canvas' controls were
/// an anchor rather than a dock panel — degrades to `Free` for the same reason.
/// The panel then keeps its STORED `pos`, which is the position it was last drawn
/// at under that anchor: the driver refreshes every panel's `pos` from the solved
/// rect on each frame (`mod.rs::write_back_positions`). That is as far as the
/// guarantee goes — where the file carries no `pos` at all, or a non-finite one,
/// `StoredPanel::pos` defaults to `[0.0, 0.0]` and `sane_coord` zeroes it, so the
/// panel arrives at the host area's origin like any other position-less stored
/// panel. Mapping the anchor onto the panel that now holds the «Лента» tab would
/// need the same coordinates anyway and would make this module depend on a
/// specific program tab's tab set, which the layer boundary forbids.
fn decode_anchor(
    anchor: StoredAnchor,
    new_index: &[Option<u32>],
    key: &str,
    index: usize,
) -> PanelAnchor {
    match anchor {
        StoredAnchor::Free => PanelAnchor::Free,
        StoredAnchor::Panel {
            target,
            edge,
            align,
        } => {
            let resolved = usize::try_from(target)
                .ok()
                .filter(|target| *target != index)
                .and_then(|target| new_index.get(target).copied().flatten());
            match resolved {
                Some(id) => PanelAnchor::Panel {
                    target: PanelId::new(id),
                    edge: edge.into(),
                    align,
                },
                None => {
                    runtime_log::log_warn(format!(
                        "[panel_dock::persist] `{key}`: stored panel #{index} is anchored to panel \
                         #{target}, which no longer exists; leaving it free-floating"
                    ));
                    PanelAnchor::Free
                }
            }
        }
        StoredAnchor::ViewportEdge { edge, along } => PanelAnchor::ViewportEdge {
            edge: edge.into(),
            along,
        },
        StoredAnchor::CanvasControls { .. } => {
            runtime_log::log_info(format!(
                "[panel_dock::persist] `{key}`: stored panel #{index} uses the legacy \
                 canvas-controls anchor; it is restored free-floating at its stored position and \
                 will be written back without that anchor"
            ));
            PanelAnchor::Free
        }
    }
}

/// Decodes the whole section, or `None` when it must not be used.
///
/// `None` means "start from the defaults": the section is absent, malformed, or
/// declares a newer schema version (which is also never overwritten, see
/// [`persist_layouts`]).
fn decode_section(section: Option<&Value>) -> Option<StoredSection> {
    let section = section?;
    if section.is_null() {
        return None;
    }
    let stored = match serde_json::from_value::<StoredSection>(section.clone()) {
        Ok(stored) => stored,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[panel_dock::persist] malformed '{PANEL_LAYOUT_SECTION_KEY}' section in \
                 user_config.json, using the default layouts; error={err}"
            ));
            return None;
        }
    };
    if let Some(found) = stored
        .version
        .filter(|version| *version > PANEL_LAYOUT_SECTION_VERSION)
    {
        runtime_log::log_warn(format!(
            "[panel_dock::persist] the '{PANEL_LAYOUT_SECTION_KEY}' section declares schema \
             version {found}, newer than the supported {PANEL_LAYOUT_SECTION_VERSION}; using the \
             default layouts and leaving the section untouched"
        ));
        return None;
    }
    Some(stored)
}

/// Rebuilds the sub-window list, dropping every entry that cannot describe a
/// window.
///
/// A duplicate index is dropped rather than merged: two windows with the same
/// index would share a `ViewportId` and a `HostId`, so only the first of them
/// could ever be addressed.
fn decode_sub_windows(stored: &[StoredSubWindow]) -> Vec<SubWindowNode> {
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut nodes = Vec::with_capacity(stored.len());
    for entry in stored {
        if !seen.insert(entry.index) {
            runtime_log::log_warn(format!(
                "[panel_dock::persist] two stored sub-windows claim index {}; the second one is \
                 dropped",
                entry.index
            ));
            continue;
        }
        let pos = entry
            .pos
            .map(|pos| Pos2::new(sane_coord(pos[0]), sane_coord(pos[1])));
        let size = sane_size(entry.size).unwrap_or(super::window::DEFAULT_SUB_WINDOW_SIZE);
        nodes.push(SubWindowNode::new(entry.index, pos, size));
    }
    nodes
}

/// Restores the stored arrangement of every program tab named in `defaults`,
/// together with the sub-windows it refers to.
///
/// `user_settings` is an already loaded `user_config.json` snapshot (the one
/// `MangaApp` reads at startup) — this function performs no I/O. Each entry of
/// `defaults` is a program tab's `AppTab::key()` and its default-layout builder;
/// the builder is used only to learn which `TabId`s that program tab knows (see
/// [`known_tabs`]), never to replace a stored layout.
///
/// Keys absent from the result keep whatever
/// [`PanelDockState::ensure_default_layout`](super::PanelDockState::ensure_default_layout)
/// builds for them, which is what makes every failure path above a silent
/// fallback to the default arrangement.
#[must_use]
pub fn layouts_from_user_settings(
    user_settings: &Value,
    defaults: &[LayoutDefault<'_>],
) -> PanelLayoutSnapshot {
    let Some(stored) = decode_section(user_settings.get(PANEL_LAYOUT_SECTION_KEY)) else {
        return PanelLayoutSnapshot::default();
    };
    let sub_windows = decode_sub_windows(&stored.sub_windows);
    let indices: BTreeSet<u32> = sub_windows.iter().map(|node| node.index).collect();
    let mut layouts = BTreeMap::new();
    for (key, build) in defaults {
        let Some(entry) = stored.tabs.get(*key) else {
            continue;
        };
        let known = known_tabs(&build());
        if let Some(layout) = decode_layout(entry, key, &known, &indices) {
            layouts.insert((*key).to_owned(), layout);
        }
    }
    PanelLayoutSnapshot {
        layouts,
        sub_windows,
    }
}

// ---------------------------------------------------------------------------------------
// Encoding (pure)
// ---------------------------------------------------------------------------------------

/// Everything the dock stores: one layout per program tab plus the detached
/// windows those layouts address.
///
/// The same value in both directions — it is what
/// [`layouts_from_user_settings`] returns and what
/// [`PanelDockState::take_dirty_layouts`](super::PanelDockState::take_dirty_layouts)
/// hands to the writer — so a round trip is a plain equality.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelLayoutSnapshot {
    /// One arrangement per program tab, keyed by `AppTab::key()`.
    pub layouts: BTreeMap<String, DockLayout>,
    /// The detached windows, shared by every program tab.
    pub sub_windows: Vec<SubWindowNode>,
}

/// Converts one sub-window into its stored form.
#[must_use]
fn encode_sub_window(node: &SubWindowNode) -> StoredSubWindow {
    StoredSubWindow {
        index: node.index,
        pos: node.pos.map(|pos| [pos.x, pos.y]),
        size: [node.size.x, node.size.y],
    }
}

/// Converts one live layout into its stored form.
///
/// Panels are written in list order and every `PanelAnchor::Panel` target is
/// rewritten into that order's index, so the file never carries a `PanelId`. An
/// anchor whose target is not in the layout — unconstructible through the model,
/// handled because this function must be total — is stored as `Free`.
#[must_use]
fn encode_layout(layout: &DockLayout) -> StoredTabLayout {
    let positions: BTreeMap<PanelId, u32> = layout
        .panels()
        .iter()
        .enumerate()
        .filter_map(|(index, panel)| u32::try_from(index).ok().map(|index| (panel.id, index)))
        .collect();
    let panels = layout
        .panels()
        .iter()
        .map(|panel| StoredPanel {
            tabs: panel.tabs.iter().map(|tab| tab.as_str().to_owned()).collect(),
            active: Some(panel.active_tab.as_str().to_owned()),
            anchor: encode_anchor(panel.anchor, &positions),
            pos: [panel.pos.x, panel.pos.y],
            size: panel.size_override.map(|size| [size.x, size.y]),
            collapsed: panel.collapsed,
            host: match panel.host {
                HostId::MainWindow => StoredHost::Main,
                HostId::SubWindow(index) => StoredHost::SubWindow(index),
            },
        })
        .collect();
    StoredTabLayout { panels }
}

/// Converts one anchor into its stored form, mapping a panel target onto its
/// list index.
///
/// Total over `PanelAnchor`, which is why the legacy-only
/// [`StoredAnchor::CanvasControls`] can never be produced here: the runtime model
/// has no variant that maps onto it.
fn encode_anchor(anchor: PanelAnchor, positions: &BTreeMap<PanelId, u32>) -> StoredAnchor {
    match anchor {
        PanelAnchor::Free => StoredAnchor::Free,
        PanelAnchor::Panel {
            target,
            edge,
            align,
        } => match positions.get(&target) {
            Some(index) => StoredAnchor::Panel {
                target: *index,
                edge: edge.into(),
                align,
            },
            None => StoredAnchor::Free,
        },
        PanelAnchor::ViewportEdge { edge, along } => StoredAnchor::ViewportEdge {
            edge: edge.into(),
            along,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------

/// Writes the layouts of the program tabs in `snapshot` and its sub-window list
/// into the `PanelLayout` section of the config file at `path`, leaving every
/// other program tab's entry and every other config section untouched.
///
/// The sub-window list is REPLACED rather than merged: it is global to the dock,
/// so the snapshot that carries it is the whole truth about which windows exist.
///
/// # Errors
/// [`PanelLayoutError::NewerVersion`] when the section on disk declares a schema
/// version this build does not understand — nothing is written in that case, not
/// even the sections this call does not own. [`PanelLayoutError::Persist`] when
/// the read-modify-write transaction itself failed (unreadable, malformed or
/// unwritable `user_config.json`).
fn persist_layouts(path: &Path, snapshot: &PanelLayoutSnapshot) -> Result<(), PanelLayoutError> {
    // The version verdict is taken INSIDE the locked transaction (the file may
    // have been replaced since startup) but has to leave it as a typed error,
    // and the mutator may only return `anyhow`. It therefore reports the version
    // through this slot and aborts the transaction, so nothing is written.
    let mut refused_version: Option<u32> = None;
    let outcome = config::update_user_config_file(path, |root| {
        let section = root.get(PANEL_LAYOUT_SECTION_KEY).cloned().unwrap_or(Value::Null);
        let found = section
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version > u64::from(PANEL_LAYOUT_SECTION_VERSION));
        if let Some(found) = found {
            let found = u32::try_from(found).unwrap_or(u32::MAX);
            refused_version = Some(found);
            anyhow::bail!(
                "the '{PANEL_LAYOUT_SECTION_KEY}' section declares schema version {found}, newer \
                 than the supported {PANEL_LAYOUT_SECTION_VERSION}"
            );
        }
        // A malformed section is REPLACED rather than merged into: it carries
        // nothing this build could preserve, and refusing to write would leave
        // the user unable to save a layout ever again.
        let mut stored = serde_json::from_value::<StoredSection>(section).unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[panel_dock::persist] the '{PANEL_LAYOUT_SECTION_KEY}' section could not be \
                 parsed before writing and is replaced; error={err}"
            ));
            StoredSection::default()
        });
        stored.version = Some(PANEL_LAYOUT_SECTION_VERSION);
        for (key, layout) in &snapshot.layouts {
            stored.tabs.insert(key.clone(), encode_layout(layout));
        }
        stored.sub_windows = snapshot.sub_windows.iter().map(encode_sub_window).collect();
        let encoded = serde_json::to_value(&stored)
            .map_err(|err| anyhow::anyhow!("failed to serialize the panel layouts: {err}"))?;
        if let Some(object) = root.as_object_mut() {
            object.insert(PANEL_LAYOUT_SECTION_KEY.to_owned(), encoded);
        }
        Ok(())
    });
    if let Some(found) = refused_version {
        return Err(PanelLayoutError::NewerVersion { found });
    }
    outcome.map_err(|err| PanelLayoutError::Persist(format!("{err:#}")))
}

impl config_saver::SaverPayload for PanelLayoutSnapshot {
    /// Folds a newer snapshot over this one PER PROGRAM TAB: it overwrites the
    /// entry of every key it carries and leaves the others in place. Replacing
    /// the whole map would drop the pending layout of a program tab the newer
    /// snapshot says nothing about — and a snapshot legitimately carries only the
    /// keys whose layouts changed.
    ///
    /// The sub-window list, by contrast, is REPLACED: it is one global list, so
    /// the newest snapshot is the whole truth about it. That is sound because the
    /// writer has exactly ONE feeder — the studio window's single
    /// `PanelDockState`, owned by `MangaApp` — so every snapshot describes all
    /// windows. A second feeder would have to teach this fold how they share the
    /// list; the project deliberately does not have one.
    ///
    /// The same rule serves the debounce window and the retry queue, so "the
    /// last writer of a program tab wins" holds on both paths.
    fn coalesce(&mut self, newer: Self) {
        self.layouts.extend(newer.layouts);
        self.sub_windows = newer.sub_windows;
    }
}

/// Log strings of the panel-layout writer thread.
const LAYOUT_SAVER_LABELS: SaverLabels = SaverLabels {
    tag: "[panel_dock::persist]",
    subject: "the panel arrangement",
    thread_name: "panel-layout-saver",
};

/// Handle of the single writer of the `PanelLayout` section.
///
/// Owned by the application (one per studio window), NOT by `PanelDockState`:
/// the dock state is constructed in tests and in every program tab that draws
/// panels, and neither may spawn a thread or reach the disk. The application
/// polls its dock states for a dirty snapshot once per frame and hands it here;
/// all disk work then happens on the writer thread, whose debounce and retry
/// policy lives in [`config_saver`].
///
/// Dropping the writer without [`PanelLayoutWriter::flush_and_join`] disconnects
/// the channel and discards whatever was still inside the debounce window, so
/// the app's `on_exit` must flush it.
#[derive(Debug)]
pub struct PanelLayoutWriter {
    /// The debouncing writer thread that owns every write of this section.
    saver: ConfigSaver<PanelLayoutSnapshot>,
}

impl PanelLayoutWriter {
    /// Spawns the writer thread. A thread that cannot be spawned is logged and
    /// leaves the writer inert: the studio still runs, it just stops remembering
    /// the panel arrangement.
    #[must_use]
    pub fn spawn() -> Self {
        Self {
            saver: ConfigSaver::spawn(LAYOUT_SAVER_LABELS, persist_layouts),
        }
    }

    /// Queues one snapshot of the arrangement the caller owns — its layouts,
    /// keyed by `AppTab::key()`, plus the sub-windows those layouts address.
    /// Never blocks and never touches the disk.
    pub fn store(&mut self, snapshot: PanelLayoutSnapshot) {
        self.saver.store(snapshot);
    }

    /// Writes the pending snapshot and joins the writer thread. Called from the
    /// app's `on_exit`, so a layout changed in the last moments before closing
    /// is not lost inside the debounce window. Idempotent.
    ///
    /// The shutdown also makes the final attempt at a snapshot an earlier write
    /// failed on (see [`config_saver::run_saver_loop`]), which is the last moment
    /// the process still holds it; a failure there is logged as a lost
    /// arrangement.
    pub fn flush_and_join(&mut self) {
        self.saver.flush_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_saver::test_harness::{FAST_TIMING, LoopHarness, NO_TIMER_TIMING};
    use crate::config_saver::{SaverError, SaverPayload};

    const PARAMS: TabId = TabId::new("test.params");
    const EFFECTS: TabId = TabId::new("test.effects");
    const LAYERS: TabId = TabId::new("test.layers");

    /// Default layout of the fictional program tab the tests use: two panels,
    /// the second anchored under the first.
    fn test_default_layout() -> DockLayout {
        let mut first = match PanelNode::new(
            PanelId::new(0),
            HostId::MainWindow,
            vec![PARAMS, EFFECTS],
        ) {
            Ok(node) => node,
            Err(error) => unreachable!("the fixture panel is valid: {error}"),
        };
        first.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        let mut second =
            match PanelNode::new(PanelId::new(1), HostId::MainWindow, vec![LAYERS]) {
                Ok(node) => node,
                Err(error) => unreachable!("the fixture panel is valid: {error}"),
            };
        second.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        match DockLayout::from_panels(vec![first, second]) {
            Ok(layout) => layout,
            Err(error) => unreachable!("the fixture layout is valid: {error}"),
        }
    }

    fn defaults() -> [LayoutDefault<'static>; 1] {
        [("test", test_default_layout as fn() -> DockLayout)]
    }

    /// Builds a snapshot carrying the given program tabs and no sub-window.
    fn snapshot_of<const N: usize>(layouts: [(&str, DockLayout); N]) -> PanelLayoutSnapshot {
        PanelLayoutSnapshot {
            layouts: layouts
                .into_iter()
                .map(|(key, layout)| (key.to_owned(), layout))
                .collect(),
            sub_windows: Vec::new(),
        }
    }

    /// Wraps one program tab's stored form into a whole user-settings snapshot.
    fn user_settings_with(section: Value) -> Value {
        serde_json::json!({ PANEL_LAYOUT_SECTION_KEY: section })
    }

    /// Encodes a layout into the section shape the writer produces.
    fn section_from(layout: &DockLayout) -> Value {
        let stored = StoredSection {
            version: Some(PANEL_LAYOUT_SECTION_VERSION),
            tabs: [("test".to_owned(), encode_layout(layout))]
                .into_iter()
                .collect(),
            sub_windows: Vec::new(),
        };
        match serde_json::to_value(&stored) {
            Ok(value) => value,
            Err(error) => unreachable!("the fixture section serializes: {error}"),
        }
    }

    #[test]
    fn a_layout_survives_a_full_encode_decode_round_trip() {
        let mut layout = test_default_layout();
        if let Err(error) = layout.set_collapsed(PanelId::new(1), true) {
            unreachable!("the fixture panel exists: {error}");
        }
        if let Err(error) =
            layout.set_size_override(PanelId::new(0), Some(Vec2::new(320.0, 240.0)))
        {
            unreachable!("the fixture panel exists: {error}");
        }
        if let Err(error) = layout.set_panel_pos(PanelId::new(0), Pos2::new(12.0, 34.0)) {
            unreachable!("the fixture panel exists: {error}");
        }
        if let Err(error) = layout.set_active_tab(PanelId::new(0), EFFECTS) {
            unreachable!("the fixture panel owns the tab: {error}");
        }
        let restored = layouts_from_user_settings(
            &user_settings_with(section_from(&layout)),
            &defaults(),
        );
        assert_eq!(restored.layouts.get("test"), Some(&layout));
    }

    #[test]
    fn the_stored_layout_wins_over_the_default() {
        // A layout that no default builder would produce: one panel, free.
        let mut node = match PanelNode::new(PanelId::new(0), HostId::MainWindow, vec![LAYERS]) {
            Ok(node) => node,
            Err(error) => unreachable!("the fixture panel is valid: {error}"),
        };
        node.pos = Pos2::new(7.0, 9.0);
        let layout = match DockLayout::from_panels(vec![node]) {
            Ok(layout) => layout,
            Err(error) => unreachable!("the fixture layout is valid: {error}"),
        };
        let restored =
            layouts_from_user_settings(&user_settings_with(section_from(&layout)), &defaults());
        assert_eq!(restored.layouts.get("test"), Some(&layout));
        assert_ne!(restored.layouts.get("test"), Some(&test_default_layout()));
    }

    #[test]
    fn an_unknown_tab_is_dropped_and_the_rest_of_the_layout_survives() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params", "test.removed"], "active": "test.params",
                  "anchor": {"kind": "free"}, "pos": [1.0, 2.0] },
                { "tabs": ["test.gone"], "active": "test.gone", "anchor": {"kind": "free"} }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the surviving panel keeps the layout usable"),
        };
        assert_eq!(layout.panels().len(), 1);
        assert_eq!(layout.panels()[0].tabs, vec![PARAMS]);
        // `test.effects` and `test.layers` are known but absent from the file:
        // the dock re-creates them, this layer does not.
        assert_eq!(layout.panel_of_tab(EFFECTS), None);
    }

    #[test]
    fn a_dropped_panel_does_not_shift_the_anchor_of_the_panels_behind_it() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.gone"], "anchor": {"kind": "free"} },
                { "tabs": ["test.params"], "anchor": {"kind": "free"} },
                { "tabs": ["test.layers"],
                  "anchor": {"kind": "panel", "target": 1, "edge": "bottom", "align": 0.0} }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the two surviving panels keep the layout usable"),
        };
        assert_eq!(layout.panels().len(), 2);
        // The «params» panel was #1 in the file and is #0 after the drop; the
        // anchor must follow it rather than address the free slot.
        assert_eq!(layout.panels()[1].anchor.target_panel(), Some(PanelId::new(0)));
    }

    #[test]
    fn an_anchor_onto_a_dropped_panel_degrades_to_free() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.gone"], "anchor": {"kind": "free"} },
                { "tabs": ["test.params"],
                  "anchor": {"kind": "panel", "target": 0, "edge": "bottom", "align": 0.0} }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the surviving panel keeps the layout usable"),
        };
        assert_eq!(layout.panels()[0].anchor, PanelAnchor::Free);
    }

    #[test]
    fn a_malformed_section_falls_back_to_the_defaults() {
        let section = serde_json::json!({ "tabs": "not an object" });
        assert!(
            layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty()
        );
        // Not JSON at all in the slot: same outcome, no panic.
        assert!(
            layouts_from_user_settings(&user_settings_with(Value::Bool(true)), &defaults())
                .layouts
                .is_empty()
        );
        // Absent section.
        assert!(layouts_from_user_settings(&Value::Null, &defaults()).layouts.is_empty());
    }

    #[test]
    fn a_newer_section_version_is_ignored_on_read() {
        let mut section = section_from(&test_default_layout());
        if let Some(object) = section.as_object_mut() {
            object.insert(
                "version".to_owned(),
                Value::from(PANEL_LAYOUT_SECTION_VERSION + 1),
            );
        }
        assert!(layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty());
    }

    #[test]
    fn a_tab_owned_by_two_panels_rejects_the_whole_stored_layout() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params"], "anchor": {"kind": "free"} },
                { "tabs": ["test.params"], "anchor": {"kind": "free"} }
            ] } },
            "sub_windows": []
        });
        assert!(layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty());
    }

    #[test]
    fn a_tab_listed_twice_in_one_panel_rejects_the_whole_stored_layout() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params", "test.params"], "anchor": {"kind": "free"} }
            ] } },
            "sub_windows": []
        });
        assert!(layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty());
    }

    #[test]
    fn an_anchor_cycle_rejects_the_whole_stored_layout() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params"],
                  "anchor": {"kind": "panel", "target": 1, "edge": "bottom", "align": 0.0} },
                { "tabs": ["test.layers"],
                  "anchor": {"kind": "panel", "target": 0, "edge": "bottom", "align": 0.0} }
            ] } },
            "sub_windows": []
        });
        assert!(layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty());
    }

    #[test]
    fn an_empty_stored_layout_falls_back_to_the_defaults() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [] } },
            "sub_windows": []
        });
        assert!(layouts_from_user_settings(&user_settings_with(section), &defaults()).layouts.is_empty());
    }

    #[test]
    fn a_corrupt_position_or_size_is_sanitized_instead_of_rejecting_the_layout() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params"], "anchor": {"kind": "free"},
                  "pos": [1.0e12, -1.0e12], "size": [0.0, 240.0] }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("a sanitizable panel keeps the layout usable"),
        };
        assert_eq!(
            layout.panels()[0].pos,
            Pos2::new(MAX_STORED_COORD, -MAX_STORED_COORD)
        );
        assert_eq!(layout.panels()[0].size_override, None);
    }

    #[test]
    fn a_sub_window_panel_is_drawn_in_the_main_window_until_phase_6() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params"], "anchor": {"kind": "free"},
                  "host": {"sub_window": 2} }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the remapped panel keeps the layout usable"),
        };
        assert_eq!(layout.panels()[0].host, HostId::MainWindow);
    }

    #[test]
    fn an_unresolvable_active_tab_falls_back_to_the_first_one() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": { "test": { "panels": [
                { "tabs": ["test.params", "test.effects"], "active": "test.gone",
                  "anchor": {"kind": "free"} }
            ] } },
            "sub_windows": []
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the repaired panel keeps the layout usable"),
        };
        assert_eq!(layout.panels()[0].active_tab, PARAMS);
    }

    #[test]
    fn a_layout_with_id_holes_is_renumbered_and_keeps_its_relations() {
        // Ids 3 and 7, as a session full of inserts and removals leaves them.
        let mut first = match PanelNode::new(PanelId::new(3), HostId::MainWindow, vec![PARAMS]) {
            Ok(node) => node,
            Err(error) => unreachable!("the fixture panel is valid: {error}"),
        };
        first.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.5,
        };
        let mut second = match PanelNode::new(PanelId::new(7), HostId::MainWindow, vec![LAYERS]) {
            Ok(node) => node,
            Err(error) => unreachable!("the fixture panel is valid: {error}"),
        };
        second.anchor = PanelAnchor::Panel {
            target: PanelId::new(3),
            edge: DockEdge::Bottom,
            align: 0.25,
        };
        let layout = match DockLayout::from_panels(vec![first, second]) {
            Ok(layout) => layout,
            Err(error) => unreachable!("the fixture layout is valid: {error}"),
        };
        let restored =
            layouts_from_user_settings(&user_settings_with(section_from(&layout)), &defaults());
        let restored = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the fixture layout is restorable"),
        };
        assert_eq!(restored.panels()[0].id, PanelId::new(0));
        assert_eq!(restored.panels()[1].id, PanelId::new(1));
        assert_eq!(
            restored.panels()[1].anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.25,
            }
        );
    }

    #[test]
    fn coalescing_keeps_the_last_snapshot_of_every_program_tab() {
        let mut pending = snapshot_of([("typing", test_default_layout())]);
        let mut newer_typing = test_default_layout();
        if let Err(error) = newer_typing.set_collapsed(PanelId::new(0), true) {
            unreachable!("the fixture panel exists: {error}");
        }

        pending.coalesce(snapshot_of([("cleaning", DockLayout::new())]));
        pending.coalesce(snapshot_of([("typing", newer_typing.clone())]));

        // The newest «typing» snapshot wins…
        assert_eq!(pending.layouts.get("typing"), Some(&newer_typing));
        // …and the program tab the newest snapshot said nothing about survives.
        assert!(pending.layouts.contains_key("cleaning"));
    }

    #[test]
    fn the_shipped_defaults_carry_the_current_section_version() {
        let defaults = config::user_config_defaults();
        let section = defaults.get(PANEL_LAYOUT_SECTION_KEY);
        let stored = match section.map(|value| serde_json::from_value::<StoredSection>(value.clone()))
        {
            Some(Ok(stored)) => stored,
            Some(Err(error)) => unreachable!("the shipped default section decodes: {error}"),
            None => unreachable!("the shipped defaults carry a '{PANEL_LAYOUT_SECTION_KEY}' section"),
        };
        assert_eq!(stored.version, Some(PANEL_LAYOUT_SECTION_VERSION));
        assert!(stored.tabs.is_empty());
        assert!(stored.sub_windows.is_empty());
    }

    /// A layout whose `LAYERS` panel lives in sub-window `1`.
    fn layout_with_a_sub_window() -> DockLayout {
        let mut layout = test_default_layout();
        if let Err(error) = layout.detach_tab_to_host(
            LAYERS,
            HostId::SubWindow(1),
            Pos2::new(8.0, 8.0),
        ) {
            unreachable!("the fixture tab can be detached: {error}");
        }
        layout
    }

    /// Encodes a whole snapshot the way the writer does, without touching a disk.
    fn section_from_snapshot(snapshot: &PanelLayoutSnapshot) -> Value {
        let stored = StoredSection {
            version: Some(PANEL_LAYOUT_SECTION_VERSION),
            tabs: snapshot
                .layouts
                .iter()
                .map(|(key, layout)| (key.clone(), encode_layout(layout)))
                .collect(),
            sub_windows: snapshot.sub_windows.iter().map(encode_sub_window).collect(),
        };
        match serde_json::to_value(&stored) {
            Ok(value) => value,
            Err(error) => unreachable!("the fixture section serializes: {error}"),
        }
    }

    #[test]
    fn sub_windows_and_their_panels_survive_a_round_trip() {
        let snapshot = PanelLayoutSnapshot {
            layouts: [("test".to_owned(), layout_with_a_sub_window())]
                .into_iter()
                .collect(),
            sub_windows: vec![SubWindowNode::new(
                1,
                Some(Pos2::new(240.0, 120.0)),
                Vec2::new(420.0, 560.0),
            )],
        };
        let restored = layouts_from_user_settings(
            &user_settings_with(section_from_snapshot(&snapshot)),
            &defaults(),
        );
        assert_eq!(restored.sub_windows, snapshot.sub_windows);
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the stored layout is usable"),
        };
        let detached = match layout.panel_of_tab(LAYERS) {
            Some(panel) => panel,
            None => unreachable!("the detached tab survived"),
        };
        assert_eq!(
            layout.panel(detached).map(|panel| panel.host),
            Some(HostId::SubWindow(1))
        );
        assert_eq!(layout.panel(detached).map(|panel| panel.pos), Some(Pos2::new(8.0, 8.0)));
        assert_eq!(layout.validate(), Ok(()));
        // Re-encoding the restored arrangement reproduces the file: panel IDS are
        // renumbered on load by design (they are not stored at all), so the round
        // trip is an equality of the SECTION, not of the live ids.
        assert_eq!(
            section_from_snapshot(&restored),
            section_from_snapshot(&snapshot)
        );
    }

    #[test]
    fn a_window_without_a_position_round_trips_as_such() {
        // Wayland reports no window position, and "unknown" must stay unknown
        // rather than becoming a coordinate the compositor never gave us.
        let snapshot = PanelLayoutSnapshot {
            layouts: [("test".to_owned(), layout_with_a_sub_window())]
                .into_iter()
                .collect(),
            sub_windows: vec![SubWindowNode::new(1, None, Vec2::new(420.0, 560.0))],
        };
        let restored = layouts_from_user_settings(
            &user_settings_with(section_from_snapshot(&snapshot)),
            &defaults(),
        );
        assert_eq!(restored.sub_windows.first().map(|node| node.pos), Some(None));
    }

    #[test]
    fn a_panel_naming_an_unknown_window_comes_back_to_the_main_one() {
        // The panel must never be dropped: the dock does not re-create a tab it
        // already owns, so its tabs would be unreachable forever.
        let mut snapshot = PanelLayoutSnapshot {
            layouts: [("test".to_owned(), layout_with_a_sub_window())]
                .into_iter()
                .collect(),
            sub_windows: Vec::new(),
        };
        let section = section_from_snapshot(&snapshot);
        snapshot.sub_windows.clear();
        let restored = layouts_from_user_settings(&user_settings_with(section), &defaults());
        let layout = match restored.layouts.get("test") {
            Some(layout) => layout,
            None => unreachable!("the stored layout is usable"),
        };
        assert!(restored.sub_windows.is_empty());
        assert!(!layout.has_panels_in_host(HostId::SubWindow(1)));
        assert!(layout.panel_of_tab(LAYERS).is_some());
        assert_eq!(layout.validate(), Ok(()));
    }

    #[test]
    fn two_windows_claiming_one_index_keep_the_first() {
        let stored = [
            StoredSubWindow {
                index: 4,
                pos: Some([10.0, 20.0]),
                size: [400.0, 500.0],
            },
            StoredSubWindow {
                index: 4,
                pos: Some([90.0, 90.0]),
                size: [300.0, 300.0],
            },
        ];
        let decoded = decode_sub_windows(&stored);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].pos, Some(Pos2::new(10.0, 20.0)));
    }

    #[test]
    fn a_nonsense_window_size_falls_back_to_the_default() {
        let stored = [StoredSubWindow {
            index: 0,
            pos: None,
            size: [0.0, f32::NAN],
        }];
        let decoded = decode_sub_windows(&stored);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].size, super::super::window::DEFAULT_SUB_WINDOW_SIZE);
    }

    // -----------------------------------------------------------------------
    // Writer loop: this section's part of the shared debounce/retry policy
    // -----------------------------------------------------------------------

    /// A `Persist` failure, i.e. the transient kind the retry queue exists for.
    fn transient_failure() -> PanelLayoutError {
        PanelLayoutError::Persist("permission denied".to_owned())
    }

    /// Fails the first `n` attempts and lets every later one succeed.
    fn fail_first(n: u32) -> impl FnMut(u32) -> Result<(), PanelLayoutError> + Send + 'static {
        move |attempt| {
            if attempt <= n {
                Err(transient_failure())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_failed_write_is_retried_until_it_succeeds() {
        // The regression: the dock clears `dirty` when it hands the snapshot
        // over, so a snapshot dropped on a transient failure is gone from the
        // whole process. It must be held and retried instead.
        let harness = LoopHarness::start(FAST_TIMING, fail_first(2));
        let snapshot = snapshot_of([("typing", test_default_layout())]);
        harness.store(snapshot.clone());

        // Two failures and the success that follows them, with no further input
        // from the GUI thread — the user stopped rearranging panels.
        harness.await_attempt();
        harness.await_attempt();
        harness.await_attempt();

        harness.shutdown();
        let attempts = harness.join_and_take_attempts();
        assert_eq!(attempts.len(), 3);
        for attempt in &attempts {
            assert_eq!(attempt, &snapshot);
        }
    }

    #[test]
    fn a_newer_snapshot_replaces_the_held_one_per_program_tab() {
        // Same rule as the debounce coalescing: the newest layout of a program
        // tab wins, and a program tab the newer snapshot says nothing about is
        // still owed to the disk.
        let harness = LoopHarness::start(NO_TIMER_TIMING, fail_first(1));
        harness.store(snapshot_of([
            ("typing", test_default_layout()),
            ("cleaning", DockLayout::new()),
        ]));
        harness.await_attempt();

        let mut newer_typing = test_default_layout();
        if let Err(error) = newer_typing.set_collapsed(PanelId::new(0), true) {
            unreachable!("the fixture panel exists: {error}");
        }
        harness.store(snapshot_of([("typing", newer_typing.clone())]));
        harness.await_attempt();
        harness.shutdown();

        let attempts = harness.join_and_take_attempts();
        assert_eq!(attempts.len(), 2);
        let second = match attempts.get(1) {
            Some(second) => second,
            None => unreachable!("two attempts were recorded"),
        };
        assert_eq!(second.layouts.get("typing"), Some(&newer_typing));
        assert!(second.layouts.contains_key("cleaning"));
    }

    #[test]
    fn a_version_refusal_is_not_retried() {
        // Refusing to overwrite a section from a newer build stays true for as
        // long as that section is on disk: holding it would only burn attempts.
        // Nothing is held, so the shutdown writes nothing at all.
        let harness = LoopHarness::start(FAST_TIMING, |_| {
            Err(PanelLayoutError::NewerVersion {
                found: PANEL_LAYOUT_SECTION_VERSION + 1,
            })
        });
        harness.store(snapshot_of([("typing", test_default_layout())]));
        harness.await_attempt();

        harness.shutdown();
        assert_eq!(harness.join_and_take_attempts().len(), 1);
    }

    #[test]
    fn only_a_transient_failure_is_worth_retrying() {
        assert!(!PanelLayoutError::NewerVersion { found: 99 }.is_retryable());
        assert!(transient_failure().is_retryable());
    }

    // -----------------------------------------------------------------------
    // Legacy `canvas_controls` anchor: the section must still decode WHOLE
    // -----------------------------------------------------------------------

    /// The eight tab keys a pre-«Лента» build stored for the typing program tab.
    const LEGACY_TYPING_TABS: [TabId; 8] = [
        TabId::new("typing.preview"),
        TabId::new("typing.params"),
        TabId::new("typing.effects"),
        TabId::new("typing.mask"),
        TabId::new("typing.deform"),
        TabId::new("typing.layout_editor"),
        TabId::new("typing.actions"),
        TabId::new("typing.layers"),
    ];

    /// The «Лента» tab, which the second program tab of the fixture declares.
    const LEGACY_RIBBON_TAB: TabId = TabId::new("canvas.ribbon");

    /// Builds a one-panel default layout naming `tabs`.
    ///
    /// A default layout is used by [`layouts_from_user_settings`] only as the
    /// dictionary of tab keys a stored layout is resolved against, so its own
    /// arrangement is irrelevant here.
    fn dictionary_layout(tabs: &[TabId]) -> DockLayout {
        let node = match PanelNode::new(PanelId::new(0), HostId::MainWindow, tabs.to_vec()) {
            Ok(node) => node,
            Err(error) => unreachable!("the fixture panel is valid: {error}"),
        };
        match DockLayout::from_panels(vec![node]) {
            Ok(layout) => layout,
            Err(error) => unreachable!("the fixture layout is valid: {error}"),
        }
    }

    fn legacy_typing_dictionary() -> DockLayout {
        dictionary_layout(&LEGACY_TYPING_TABS)
    }

    fn legacy_cleaning_dictionary() -> DockLayout {
        dictionary_layout(&[LEGACY_RIBBON_TAB])
    }

    fn legacy_defaults() -> [LayoutDefault<'static>; 2] {
        [
            ("typing", legacy_typing_dictionary as fn() -> DockLayout),
            ("cleaning", legacy_cleaning_dictionary as fn() -> DockLayout),
        ]
    }

    /// A version-1 section exactly as a pre-«Лента» build wrote it: six typing
    /// panels whose first one carries the retired `canvas_controls` anchor and is
    /// the anchor TARGET of two others, plus a second program tab whose panel
    /// lives in a detached window.
    fn legacy_section() -> Value {
        serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "sub_windows": [
                { "index": 3, "pos": [240.0, 120.0], "size": [420.0, 560.0] }
            ],
            "tabs": {
                "typing": { "panels": [
                    {
                        "tabs": ["typing.preview"],
                        "active": "typing.preview",
                        "anchor": {
                            "kind": "canvas_controls",
                            "edge": "bottom",
                            "along": 0.017_609_848
                        },
                        "collapsed": false,
                        "host": "main",
                        "pos": [12.727_575, 122.187_5],
                        "size": [276.652_34, 292.027_34]
                    },
                    {
                        "tabs": ["typing.params", "typing.effects"],
                        "active": "typing.params",
                        "anchor": { "kind": "viewport_edge", "edge": "right", "along": 0.038_616_493 },
                        "collapsed": false,
                        "host": "main",
                        "pos": [1_573.218_75, 19.582_031],
                        "size": [434.781_25, 533.316_4]
                    },
                    {
                        "tabs": ["typing.mask"],
                        "active": "typing.mask",
                        "anchor": { "kind": "panel", "target": 1, "edge": "bottom", "align": 0.0 },
                        "collapsed": false,
                        "host": "main",
                        "pos": [0.0, 0.0]
                    },
                    {
                        "tabs": ["typing.deform"],
                        "active": "typing.deform",
                        "anchor": { "kind": "panel", "target": 0, "edge": "bottom", "align": 0.008_073_221 },
                        "collapsed": false,
                        "host": "main",
                        "pos": [0.0, 0.0]
                    },
                    {
                        "tabs": ["typing.layout_editor"],
                        "active": "typing.layout_editor",
                        "anchor": { "kind": "panel", "target": 3, "edge": "bottom", "align": 0.0 },
                        "collapsed": false,
                        "host": "main",
                        "pos": [0.0, 0.0]
                    },
                    {
                        "tabs": ["typing.actions", "typing.layers"],
                        "active": "typing.actions",
                        "anchor": { "kind": "panel", "target": 0, "edge": "bottom", "align": 0.0 },
                        "collapsed": true,
                        "host": "main",
                        "pos": [12.727_575, 422.214_84],
                        "size": [277.511_72, 284.738_28]
                    }
                ] },
                "cleaning": { "panels": [
                    {
                        "tabs": ["canvas.ribbon"],
                        "active": "canvas.ribbon",
                        "anchor": { "kind": "free" },
                        "collapsed": false,
                        "host": { "sub_window": 3 },
                        "pos": [8.0, 8.0],
                        "size": [320.0, 180.0]
                    }
                ] }
            }
        })
    }

    #[test]
    fn a_legacy_canvas_controls_anchor_keeps_the_whole_stored_arrangement() {
        // The regression this pins is total, not local: `StoredAnchor` is
        // internally tagged with no `#[serde(other)]` and the whole section is
        // decoded by ONE `from_value`, so dropping the retired tag would fail the
        // section and reset EVERY program tab's layout — permanently, on the next
        // dirty write. Every panel, tab, size, host and window is asserted.
        let restored =
            layouts_from_user_settings(&user_settings_with(legacy_section()), &legacy_defaults());

        let typing = match restored.layouts.get("typing") {
            Some(layout) => layout,
            None => unreachable!("the legacy section must decode"),
        };
        assert_eq!(typing.panels().len(), 6);
        assert_eq!(typing.validate(), Ok(()));

        let tabs: Vec<Vec<TabId>> = typing
            .panels()
            .iter()
            .map(|panel| panel.tabs.clone())
            .collect();
        assert_eq!(tabs, vec![
            vec![TabId::new("typing.preview")],
            vec![TabId::new("typing.params"), TabId::new("typing.effects")],
            vec![TabId::new("typing.mask")],
            vec![TabId::new("typing.deform")],
            vec![TabId::new("typing.layout_editor")],
            vec![TabId::new("typing.actions"), TabId::new("typing.layers")],
        ]);
        let active: Vec<TabId> = typing
            .panels()
            .iter()
            .map(|panel| panel.active_tab)
            .collect();
        assert_eq!(active, vec![
            TabId::new("typing.preview"),
            TabId::new("typing.params"),
            TabId::new("typing.mask"),
            TabId::new("typing.deform"),
            TabId::new("typing.layout_editor"),
            TabId::new("typing.actions"),
        ]);

        // The retired anchor becomes `Free` at the position the panel was last
        // drawn at, which is the geometry the old anchor produced.
        assert_eq!(typing.panels()[0].anchor, PanelAnchor::Free);
        assert_eq!(typing.panels()[0].pos, Pos2::new(12.727_575, 122.187_5));
        assert_eq!(
            typing.panels()[0].size_override,
            Some(Vec2::new(276.652_34, 292.027_34))
        );

        // Every other anchor survives untouched — including the two panels that
        // address the legacy-anchored one by index.
        let anchors: Vec<PanelAnchor> = typing
            .panels()
            .iter()
            .map(|panel| panel.anchor)
            .collect();
        assert_eq!(anchors, vec![
            PanelAnchor::Free,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.038_616_493,
            },
            PanelAnchor::Panel {
                target: PanelId::new(1),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.008_073_221,
            },
            PanelAnchor::Panel {
                target: PanelId::new(3),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ]);

        let sizes: Vec<Option<Vec2>> = typing
            .panels()
            .iter()
            .map(|panel| panel.size_override)
            .collect();
        assert_eq!(sizes, vec![
            Some(Vec2::new(276.652_34, 292.027_34)),
            Some(Vec2::new(434.781_25, 533.316_4)),
            None,
            None,
            None,
            Some(Vec2::new(277.511_72, 284.738_28)),
        ]);
        let collapsed: Vec<bool> = typing
            .panels()
            .iter()
            .map(|panel| panel.collapsed)
            .collect();
        assert_eq!(collapsed, vec![false, false, false, false, false, true]);
        assert!(
            typing
                .panels()
                .iter()
                .all(|panel| panel.host == HostId::MainWindow)
        );

        // The second program tab and the window it lives in are untouched too.
        let cleaning = match restored.layouts.get("cleaning") {
            Some(layout) => layout,
            None => unreachable!("the second program tab must decode as well"),
        };
        assert_eq!(cleaning.panels().len(), 1);
        assert_eq!(cleaning.panels()[0].tabs, vec![LEGACY_RIBBON_TAB]);
        assert_eq!(cleaning.panels()[0].host, HostId::SubWindow(3));
        assert_eq!(cleaning.panels()[0].pos, Pos2::new(8.0, 8.0));
        assert_eq!(
            restored.sub_windows,
            vec![SubWindowNode::new(
                3,
                Some(Pos2::new(240.0, 120.0)),
                Vec2::new(420.0, 560.0)
            )]
        );
    }

    /// The legacy anchor keeps the panel's stored position — and NOTHING more.
    /// A file that carries no `pos` for such a panel leaves it at the host area's
    /// ORIGIN, exactly like any other position-less stored panel: `StoredPanel::pos`
    /// is `#[serde(default)]`. The compatibility path reproduces the old geometry
    /// only as far as the file describes it, which is what `decode_anchor`'s
    /// contract says.
    #[test]
    fn a_legacy_anchor_without_a_stored_position_lands_at_the_origin() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "tabs": {
                "cleaning": { "panels": [
                    {
                        "tabs": ["canvas.ribbon"],
                        "active": "canvas.ribbon",
                        "anchor": { "kind": "canvas_controls", "edge": "bottom", "along": 0.5 },
                        "host": "main"
                    }
                ] }
            }
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &[(
            "cleaning",
            legacy_cleaning_dictionary as fn() -> DockLayout,
        )]);
        let cleaning = match restored.layouts.get("cleaning") {
            Some(layout) => layout,
            None => unreachable!("the section must decode"),
        };
        assert_eq!(cleaning.panels().len(), 1);
        // No `pos` field at all: serde's default, not the old anchor's geometry.
        assert_eq!(cleaning.panels()[0].anchor, PanelAnchor::Free);
        assert_eq!(cleaning.panels()[0].pos, Pos2::new(0.0, 0.0));
    }

    /// The legacy anchor is orthogonal to the HOST: a panel a pre-«Лента» build
    /// left in a detached window must come back into that window, free at its
    /// stored position, with the window itself restored. The main-window fixture
    /// above cannot show this — `decode_anchor` and `decode_host` are separate
    /// steps and only a panel carrying both proves they compose.
    #[test]
    fn a_legacy_anchor_inside_a_sub_window_keeps_its_window() {
        let section = serde_json::json!({
            "version": PANEL_LAYOUT_SECTION_VERSION,
            "sub_windows": [
                { "index": 2, "pos": [640.0, 200.0], "size": [380.0, 500.0] }
            ],
            "tabs": {
                "cleaning": { "panels": [
                    {
                        "tabs": ["canvas.ribbon"],
                        "active": "canvas.ribbon",
                        "anchor": { "kind": "canvas_controls", "edge": "bottom", "along": 0.75 },
                        "collapsed": false,
                        "host": { "sub_window": 2 },
                        "pos": [16.0, 24.0],
                        "size": [300.0, 170.0]
                    }
                ] }
            }
        });
        let restored = layouts_from_user_settings(&user_settings_with(section), &[(
            "cleaning",
            legacy_cleaning_dictionary as fn() -> DockLayout,
        )]);
        let cleaning = match restored.layouts.get("cleaning") {
            Some(layout) => layout,
            None => unreachable!("the section must decode"),
        };
        assert_eq!(cleaning.panels().len(), 1);
        let panel = &cleaning.panels()[0];
        assert_eq!(panel.tabs, vec![LEGACY_RIBBON_TAB]);
        assert_eq!(panel.anchor, PanelAnchor::Free);
        assert_eq!(panel.host, HostId::SubWindow(2));
        assert_eq!(panel.pos, Pos2::new(16.0, 24.0));
        assert_eq!(panel.size_override, Some(Vec2::new(300.0, 170.0)));
        assert_eq!(
            restored.sub_windows,
            vec![SubWindowNode::new(
                2,
                Some(Pos2::new(640.0, 200.0)),
                Vec2::new(380.0, 500.0)
            )]
        );
    }

    #[test]
    fn the_legacy_anchor_tag_is_never_written_back() {
        // Decoding keeps the section readable; encoding must retire the tag, or
        // the compatibility path would be self-perpetuating.
        let restored =
            layouts_from_user_settings(&user_settings_with(legacy_section()), &legacy_defaults());
        let encoded = match serde_json::to_value(&StoredSection {
            version: Some(PANEL_LAYOUT_SECTION_VERSION),
            tabs: restored
                .layouts
                .iter()
                .map(|(key, layout)| (key.clone(), encode_layout(layout)))
                .collect(),
            sub_windows: restored.sub_windows.iter().map(encode_sub_window).collect(),
        }) {
            Ok(value) => value,
            Err(error) => unreachable!("the restored section serializes: {error}"),
        };
        assert!(!encoded.to_string().contains("canvas_controls"));

        // …and what replaced it is the free anchor at the stored position.
        let typing = match restored.layouts.get("typing") {
            Some(layout) => layout,
            None => unreachable!("the legacy section must decode"),
        };
        assert_eq!(
            encode_anchor(typing.panels()[0].anchor, &BTreeMap::new()),
            StoredAnchor::Free
        );
    }

    #[test]
    fn the_newest_snapshot_owns_the_whole_window_list() {
        // The windows are ONE global list, unlike the per-program-tab layouts:
        // the newest snapshot is the whole truth about which ones exist.
        let mut pending = snapshot_of([("typing", test_default_layout())]);
        pending.sub_windows = vec![SubWindowNode::new(0, None, Vec2::new(420.0, 560.0))];

        pending.coalesce(snapshot_of([("cleaning", DockLayout::new())]));

        assert!(pending.sub_windows.is_empty());
        assert_eq!(pending.layouts.len(), 2);
    }
}
