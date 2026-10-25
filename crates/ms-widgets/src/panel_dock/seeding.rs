/*
File: crates/ms-widgets/src/panel_dock/seeding.rs

Purpose:
Where a NEWLY DECLARED tab's panel lands: asks the program tab's default layout,
answers a slot that is already taken, and creates the new panels in an order that
lets anchors resolve.

Key structures:
- `DefaultPlacement`: the default's wish, translated into live ids.
- `SiblingSlot`: the EXISTING panel (and strip index) a missing tab joins.

Key functions:
- `ensure_declared_tabs`: entry point, called by `PanelDock::end`.
- `default_placement` / `default_panel_of` / `default_anchor_tab` / `sibling_slot` /
  `seeding_group` / `seeding_order`: what the default asks for.
- `same_slot` / `free_anchor_slot`: never seed two panels into one slot.

Notes:
Every function here is pure and unit-tested in `tests.rs`.
*/

use egui::{Pos2, Vec2};
use ms_log::runtime_log;

use super::{AUTO_PANEL_CASCADE_STEP, SLOT_FRACTION_EPSILON, drag};
use super::menu_move::step_off_occupied;
use super::model::{DockLayout, HostId, PanelAnchor, PanelId, PanelNode, TabId};
use super::solver::DOCK_GAP;

/// Where the DEFAULT arrangement wants a panel that the live layout has to
/// create, expressed in the live layout's own ids.
#[derive(Copy, Clone, Debug, PartialEq)]
struct DefaultPlacement {
    anchor: PanelAnchor,
    pos: Pos2,
    size_override: Option<Vec2>,
}

/// The panel of the DEFAULT layout that holds `tab`, or `None` when the default
/// does not know the tab.
fn default_panel_of(default: &DockLayout, tab: TabId) -> Option<&PanelNode> {
    default.panel_of_tab(tab).and_then(|id| default.panel(id))
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
    let source = default_panel_of(default, tab)?;
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
/// seeding path reaches that state whenever the slot the default names for a new
/// panel is already taken in the user's restored layout — by a panel the user
/// docked there, or by one the default never put there. (A tab whose default
/// panel-mates are already live does not get here: it joins their panel, see
/// [`ensure_declared_tabs`].)
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
    let source = default_panel_of(default, tab).filter(|panel| panel.host == host)?;
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

/// Where a missing tab joins an EXISTING panel: the panel and the index in its
/// tab strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SiblingSlot {
    panel: PanelId,
    index: usize,
}

/// The live panel a missing `tab` joins because a SIBLING of it — a tab sharing
/// its panel in the DEFAULT layout — is already in `layout`, or `None` when no
/// sibling is (or the default does not know the tab).
///
/// The nearest sibling in the default's tab order decides: the closest one
/// BEFORE `tab`, and `tab` goes right after it in that sibling's panel; failing
/// that, the closest one AFTER it, and `tab` goes right before it. Joining tab by
/// tab in default order therefore rebuilds the default's relative order inside
/// the joined panel. The sibling's panel is taken wherever the user put it — any
/// host, any position — because that is where the user keeps the group now.
fn sibling_slot(default: &DockLayout, layout: &DockLayout, tab: TabId) -> Option<SiblingSlot> {
    let siblings = &default_panel_of(default, tab)?.tabs;
    let position = siblings.iter().position(|sibling| *sibling == tab)?;
    let slot_of = |sibling: TabId, after: bool| {
        let panel = layout.panel_of_tab(sibling)?;
        let index = layout
            .panel(panel)?
            .tabs
            .iter()
            .position(|held| *held == sibling)?;
        Some(SiblingSlot {
            panel,
            index: if after { index.saturating_add(1) } else { index },
        })
    };
    let (before, after) = siblings.split_at(position);
    before
        .iter()
        .rev()
        .find_map(|sibling| slot_of(*sibling, true))
        .or_else(|| after.iter().skip(1).find_map(|sibling| slot_of(*sibling, false)))
}

/// The tabs that get ONE new panel together with `tab`, and the one of them that
/// panel shows: every DECLARED tab of `tab`'s default panel that `layout` does
/// not own yet, in the default's tab order, active as in the default when the
/// default's active tab is among them (else the first). Without a default that
/// knows `tab`, the group is `tab` alone.
///
/// Only called once [`sibling_slot`] found no sibling in `layout`, so no member
/// of the default panel is live and the group is that panel's declared part.
fn seeding_group(
    default: Option<&DockLayout>,
    layout: &DockLayout,
    order: &[TabId],
    tab: TabId,
) -> (Vec<TabId>, TabId) {
    let Some(source) = default.and_then(|default| default_panel_of(default, tab)) else {
        return (vec![tab], tab);
    };
    let members: Vec<TabId> = source
        .tabs
        .iter()
        .copied()
        .filter(|member| {
            *member == tab || (order.contains(member) && layout.panel_of_tab(*member).is_none())
        })
        .collect();
    let active = if members.contains(&source.active_tab) {
        source.active_tab
    } else {
        members.first().copied().unwrap_or(tab)
    };
    (members, active)
}

/// Gives every declared tab the layout does not own yet a panel.
///
/// **Rule:** a missing tab is placed by its DEFAULT panel (`default`, the layout
/// [`PanelDockState::ensure_default_layout`] was given — pass `None` when the key
/// has no builder). A user with a stored layout keeps it, so the default builder
/// never runs for them, and a tab added by a new build reaches this function for
/// every existing user at once; the default already says which tabs belong
/// together and where.
/// - **A sibling is live** — some tab sharing the missing tab's default panel is
///   already in `layout`: the missing tab JOINS that sibling's panel, next to the
///   nearest sibling in default order ([`sibling_slot`]). That panel's active tab,
///   size, anchor and host stay as the user left them.
/// - **No sibling is live:** the missing DECLARED members of the default panel
///   get ONE new panel together, in the default's tab order, showing the
///   default's active tab when it is among them ([`seeding_group`]) — a group a
///   new build adds arrives as one tabbed panel, not a column of single-tab ones.
/// - **No default, or the default does not know the tab:** the tab gets its OWN
///   new panel. Unrelated tabs are never merged behind the caller's back: that is
///   not recoverable without dragging, while a panel that stands alone can always
///   be docked onto another one.
///
/// Tabs already in `layout` are never moved; only their panel's tab strip may
/// grow.
///
/// **Where a new panel goes:** the default panel's anchor, position and pinned
/// size when they can be expressed in this layout (see [`default_placement`]);
/// otherwise — the default does not know the tab, or its anchor targets a panel
/// that is not here — a free position cascaded by [`AUTO_PANEL_CASCADE_STEP`]
/// from the layout's origin, so it does not land exactly on an existing one.
///
/// **A seeded panel never buries a live one.** A slot the user filled with
/// another panel may be the one the default names, so an anchored placement is
/// queued behind whoever already holds that slot ([`free_anchor_slot`]) and a
/// free one steps off whatever stands at its position ([`step_off_occupied`]).
///
/// The tabs are seeded in [`seeding_order`], not in declaration order: a default
/// anchor is resolved against the LIVE layout, so a tab it hangs off another
/// newly declared tab has to be created after that one.
///
/// Returns `true` when the layout changed.
pub(super) fn ensure_declared_tabs(
    layout: &mut DockLayout,
    host: HostId,
    order: &[TabId],
    default: Option<&DockLayout>,
) -> bool {
    let mut changed = false;
    // Every entry is a tab this layout did not own when the pass began, listed
    // once; a group seeded earlier in the pass may have taken it since.
    for tab in seeding_order(layout, order, default) {
        if layout.panel_of_tab(tab).is_some() {
            continue;
        }
        if let Some(slot) = default.and_then(|default| sibling_slot(default, layout, tab)) {
            let joined = layout.edit(slot.panel, |node| {
                // `slot.index` is at most the strip's length (one past a held
                // tab's index); the clamp keeps `insert` total regardless.
                let index = slot.index.min(node.tabs.len());
                node.tabs.insert(index, tab);
            });
            match joined {
                Ok(()) => changed = true,
                Err(error) => runtime_log::log_warn(format!(
                    "[panel_dock] cannot add the newly declared tab `{}` to panel {} of its default siblings: {error}",
                    tab.as_str(),
                    slot.panel
                )),
            }
            continue;
        }
        let (members, active) = seeding_group(default, layout, order, tab);
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
        let mut node = match PanelNode::new(id, host, members) {
            Ok(node) => node,
            Err(error) => {
                runtime_log::log_warn(format!(
                    "[panel_dock] cannot build a panel for the newly declared tab `{}`: {error}",
                    tab.as_str()
                ));
                continue;
            }
        };
        // `seeding_group` picks the active tab among the members it returns.
        node.active_tab = active;
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
