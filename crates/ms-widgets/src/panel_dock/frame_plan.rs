/*
File: crates/ms-widgets/src/panel_dock/frame_plan.rs

Purpose:
The pure frame-planning step of the driver: which panels have something to draw,
which tab each draws, how big each asks to be, which header metrics apply, and the
layout the SOLVER sees this frame.

Key structures:
- `PanelPlan`: one drawable panel's plan (drawn tab, request, header, transparency).
- `FramePlan`: the whole frame's plan (desired sizes, minimums, headers, panels).

Key functions:
- `plan_frame`: declarations + measurements -> `FramePlan`.
- `tab_request`: one tab's contribution to its panel's request.
- `frame_layout`: drops panels with nothing to draw, re-anchoring their dependants
  for this frame only.

Notes:
No egui runtime state: unit-tested without a GPU in `tests.rs`.
*/

use std::collections::{BTreeMap, HashMap};

use egui::Vec2;
use ms_log::runtime_log;

use super::{MeasuredHeader, TabMeta};
use super::model::{DockLayout, HostId, PanelId, PanelNode, TabId};
use super::solver::{DEFAULT_PANEL_SIZE, PanelHeaderMetrics, PanelHeaders, PanelSizes};

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
pub(super) fn frame_layout(layout: &DockLayout, plan: &FramePlan) -> DockLayout {
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
pub(super) struct PanelPlan {
    pub(super) id: PanelId,
    pub(super) collapsed: bool,
    /// Outer size the solver was given for this panel, with the same precedence
    /// the solver applies (`size_override`, else the entry in `FramePlan::desired`,
    /// else [`DEFAULT_PANEL_SIZE`]). It is the assumption the frame was laid out
    /// on, and the WIDTH the drawn tab is measured at.
    pub(super) assumed_size: Vec2,
    /// Size the ACTIVE tab contributed to `FramePlan::desired`, or `None` when it
    /// contributed nothing (never measured and nothing declared).
    ///
    /// This — not [`PanelPlan::assumed_size`] — is what the drawn measurement is
    /// compared against, because the panel's request is the MAXIMUM over its tabs
    /// and a tab smaller than its panel would otherwise report a difference on
    /// every single frame and repaint forever.
    pub(super) active_request: Option<Vec2>,
    /// Tabs of this panel that were declared this frame AND are visible, in the
    /// panel's own tab order. These are the headers the panel shows.
    pub(super) visible_tabs: Vec<TabId>,
    /// Last frame's header measurement of this panel, when it was taken with
    /// exactly [`PanelPlan::visible_tabs`] shown; `None` otherwise (never drawn,
    /// other captions, a reused id), which the solve treats as a one-row header.
    pub(super) header: Option<PanelHeaderMetrics>,
    /// Tab whose body is drawn, or `None` when the panel has nothing to show.
    pub(super) active_tab: Option<TabId>,
    /// `true` when the tab this panel actually DRAWS asked for the
    /// transparent-until-hover mode.
    ///
    /// The question is answered here rather than in the drawing loop so it stays
    /// testable without a GPU, and it is asked of the ACTIVE tab alone: the mode
    /// describes what is on screen, so a hidden — or merely inactive —
    /// transparent tab must not make the panel showing another tab disappear.
    pub(super) transparent: bool,
}

/// The whole frame's decisions plus the size maps handed to the solver.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct FramePlan {
    pub(super) panels: Vec<PanelPlan>,
    pub(super) desired: PanelSizes,
    pub(super) mins: PanelSizes,
    /// Every [`PanelPlan::header`] that is `Some`, as the solver's input.
    pub(super) headers: PanelHeaders,
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
/// * a tab's own size is its last measurement plus the panel's CURRENT extra
///   caption rows (measurements are stored with a one-row header, see
///   `PanelDockState::measured_size`), else the declared `initial_size`, else the
///   declared `min_size` — both outer sizes the caller chose, so no rows are
///   added to them;
/// * a panel's header metrics are last frame's, and only when they were taken
///   with exactly the tabs it shows now ([`MeasuredHeader`]); A tab that has NEVER been drawn and declares
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
pub(super) fn plan_frame(
    layout: &DockLayout,
    host: HostId,
    decls: &BTreeMap<TabId, TabMeta>,
    measured: &HashMap<TabId, Vec2>,
    headers: &HashMap<PanelId, MeasuredHeader>,
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
        let header = headers
            .get(&node.id)
            .filter(|entry| entry.tabs == visible_tabs)
            .map(|entry| entry.metrics);
        let extra_rows = header.map_or(0.0, |header| header.extra_height);
        if let Some(header) = header {
            plan.headers.insert(node.id, header);
        }
        let declared = sources
            .iter()
            .filter_map(|tab| tab_request(*tab, decls, measured, extra_rows))
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
            active_request: tab_request(size_source, decls, measured, extra_rows),
            visible_tabs,
            header,
            active_tab,
            transparent,
        });
    }
    plan
}

/// Outer size ONE tab contributes to its panel's request: its last measurement
/// with the panel's `extra_rows` (points of caption rows beyond the first) added
/// to its height, else the declared `initial_size`, else the declared `min_size`.
///
/// `None` means the tab has never been drawn and the caller declared no size for
/// it, i.e. the dock knows nothing about how big it wants to be.
pub(super) fn tab_request(
    tab: TabId,
    decls: &BTreeMap<TabId, TabMeta>,
    measured: &HashMap<TabId, Vec2>,
    extra_rows: f32,
) -> Option<Vec2> {
    let meta = decls.get(&tab).copied().unwrap_or_default();
    measured
        .get(&tab)
        .map(|size| Vec2::new(size.x, size.y + extra_rows))
        .or(meta.initial_size)
        .or(meta.min_size)
}
