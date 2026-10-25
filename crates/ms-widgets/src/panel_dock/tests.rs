/*
File: crates/ms-widgets/src/panel_dock/tests.rs

Purpose:
Unit tests of the dock driver (`mod.rs`) and its frame-step children: seeding,
frame planning, mutations, gestures, menu moves, cross-window drops,
`PanelDockState` measurement caches and tab extras. GPU-free.
*/

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use super::drop_apply::{PanelMutation, apply_mutations};
use super::frame_plan::{frame_layout, plan_frame};
use super::host_draw::MenuMoveSubject;
use super::menu_move::{
    centered_slot_in_host, free_slot_in_host, gesture_release_point, menu_tab_landing,
    move_into_existing_host,
};
use super::panel_drag::{apply_panel_drop, apply_tab_drop, write_back_positions};

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

/// The slot the default names for a new panel may already be taken in the
/// user's layout — here by a panel the user docked there, holding a tab the
/// default keeps elsewhere. The solver lays two panels sharing target + edge +
/// align on exactly the same rect: the newcomer has the highest id, is drawn
/// last, and buries the live panel completely — which cannot even be dragged
/// out again. So the seeded panel queues behind it.
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

    // The new build's default: the added TAB_C docked under the panel
    // holding TAB_A, and TAB_B on a free panel of its own.
    let mut default = DockLayout::new();
    let mut root = panel_with(0, &[TAB_A]);
    root.anchor = edge_anchor;
    root.size_override = pinned;
    default.insert_panel(root).expect("default 0");
    let mut hanging = panel_with(1, &[TAB_C]);
    hanging.anchor = under_root;
    hanging.size_override = pinned;
    default.insert_panel(hanging).expect("default 1");
    default.insert_panel(panel_with(2, &[TAB_B])).expect("default 2");

    // The user's restored layout, written by the build that had no TAB_C:
    // the user docked TAB_B's panel into the slot TAB_C's default names.
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

const GROUP_D: TabId = TabId::new("test.group.d");
const GROUP_E: TabId = TabId::new("test.group.e");
const GROUP_F: TabId = TabId::new("test.group.f");

/// The default the grouping tests share: TAB_A pinned to the left edge, and
/// one panel holding the group GROUP_D, GROUP_E, GROUP_F (showing GROUP_E) docked
/// under it with a pinned size.
fn group_default() -> DockLayout {
    let mut default = DockLayout::new();
    let mut root = panel_with(0, &[TAB_A]);
    root.anchor = PanelAnchor::ViewportEdge {
        edge: DockEdge::Left,
        along: 0.0,
    };
    default.insert_panel(root).expect("default 0");
    let mut group = panel_with(1, &[GROUP_D, GROUP_E, GROUP_F]);
    group.anchor = PanelAnchor::Panel {
        target: PanelId::new(0),
        edge: DockEdge::Bottom,
        align: 0.0,
    };
    group.size_override = Some(Vec2::new(280.0, 190.0));
    group.active_tab = GROUP_E;
    default.insert_panel(group).expect("default 1");
    default
}

/// A build that adds a whole group of tabs sharing one default panel must
/// give an existing user ONE tabbed panel, in the default's order and with
/// the default's active tab and placement — not a column of single-tab panels.
#[test]
fn missing_tabs_of_one_default_panel_are_seeded_together() {
    let mut layout = DockLayout::new();
    let mut live_root = panel_with(3, &[TAB_A]);
    live_root.anchor = PanelAnchor::ViewportEdge {
        edge: DockEdge::Left,
        along: 0.0,
    };
    layout.insert_panel(live_root).expect("restored root");
    let before = layout.panel(PanelId::new(3)).cloned();

    // Declared in an order that differs from the default's tab order.
    assert!(ensure_declared_tabs(
        &mut layout,
        HostId::MainWindow,
        &[GROUP_F, TAB_A, GROUP_D, GROUP_E],
        Some(&group_default())
    ));
    assert_eq!(layout.validate(), Ok(()));
    assert_eq!(layout.panels().len(), 2, "one new panel for the whole group");
    let group = layout
        .panel_of_tab(GROUP_D)
        .and_then(|id| layout.panel(id))
        .expect("a panel was created for the group");
    assert_eq!(group.tabs, vec![GROUP_D, GROUP_E, GROUP_F]);
    assert_eq!(group.active_tab, GROUP_E);
    assert_eq!(
        group.anchor,
        PanelAnchor::Panel {
            target: PanelId::new(3),
            edge: DockEdge::Bottom,
            align: 0.0,
        }
    );
    assert_eq!(group.size_override, Some(Vec2::new(280.0, 190.0)));
    // The tab that was already there is untouched.
    assert_eq!(layout.panel(PanelId::new(3)).cloned(), before);
}

/// Only DECLARED members join the new panel; the default's active tab falls
/// back to the first member when it is not among them.
#[test]
fn an_undeclared_member_is_left_out_of_the_seeded_group() {
    let mut layout = DockLayout::new();
    layout.insert_panel(panel_with(0, &[TAB_A])).expect("restored");
    assert!(ensure_declared_tabs(
        &mut layout,
        HostId::MainWindow,
        &[TAB_A, GROUP_F, GROUP_D],
        Some(&group_default())
    ));
    let group = layout
        .panel_of_tab(GROUP_D)
        .and_then(|id| layout.panel(id))
        .expect("a panel was created for the group");
    assert_eq!(group.tabs, vec![GROUP_D, GROUP_F]);
    assert_eq!(group.active_tab, GROUP_D);
    assert_eq!(layout.panel_of_tab(GROUP_E), None);
}

/// A partial group: the missing members join the live panel that already
/// holds a sibling, next to it in the default's order — wherever the user
/// keeps that panel, and without touching its active tab or its other tabs.
#[test]
fn missing_members_join_the_panel_of_a_live_sibling() {
    // The user keeps GROUP_E in a free panel together with an unrelated TAB_B,
    // TAB_B showing; GROUP_D and GROUP_F are new.
    let mut layout = DockLayout::new();
    layout.insert_panel(panel_with(0, &[TAB_A])).expect("restored root");
    let mut kept = panel_with(5, &[TAB_B, GROUP_E]);
    kept.pos = Pos2::new(400.0, 300.0);
    layout.insert_panel(kept).expect("restored group");
    let root_before = layout.panel(PanelId::new(0)).cloned();

    assert!(ensure_declared_tabs(
        &mut layout,
        HostId::MainWindow,
        &[TAB_A, TAB_B, GROUP_F, GROUP_D, GROUP_E],
        Some(&group_default())
    ));
    assert_eq!(layout.validate(), Ok(()));
    assert_eq!(layout.panels().len(), 2, "no new panel for a partial group");
    let joined = layout.panel(PanelId::new(5)).expect("the kept panel");
    // GROUP_D precedes GROUP_E in the default, GROUP_F follows it.
    assert_eq!(joined.tabs, vec![TAB_B, GROUP_D, GROUP_E, GROUP_F]);
    assert_eq!(joined.active_tab, TAB_B);
    assert_eq!(joined.anchor, PanelAnchor::Free);
    assert_eq!(joined.pos, Pos2::new(400.0, 300.0));
    assert_eq!(layout.panel(PanelId::new(0)).cloned(), root_before);
}

/// "Any host" holds literally: a sibling the user keeps in a DETACHED window
/// pulls the missing members into that window's panel — not into a fresh panel
/// of the main window the frame is declaring for.
#[test]
fn missing_members_join_a_live_sibling_in_a_detached_window() {
    let mut layout = DockLayout::new();
    layout.insert_panel(panel_with(0, &[TAB_A])).expect("restored root");
    let detached = PanelNode::new(PanelId::new(7), HostId::SubWindow(0), vec![GROUP_E])
        .expect("detached panel");
    layout.insert_panel(detached).expect("restored detached group");

    assert!(ensure_declared_tabs(
        &mut layout,
        HostId::MainWindow,
        &[TAB_A, GROUP_D, GROUP_E, GROUP_F],
        Some(&group_default())
    ));
    assert_eq!(layout.validate(), Ok(()));
    assert_eq!(layout.panels().len(), 2, "no new panel in the main window");
    let joined = layout.panel(PanelId::new(7)).expect("the detached panel");
    assert_eq!(joined.host, HostId::SubWindow(0));
    assert_eq!(joined.tabs, vec![GROUP_D, GROUP_E, GROUP_F]);
    assert_eq!(joined.active_tab, GROUP_E);
}

/// When the only live sibling FOLLOWS the missing tab in the default's order,
/// the missing tab goes right before it.
#[test]
fn a_missing_member_goes_before_a_following_live_sibling() {
    let mut layout = DockLayout::new();
    layout.insert_panel(panel_with(0, &[TAB_A])).expect("restored root");
    layout
        .insert_panel(panel_with(1, &[GROUP_F, TAB_B]))
        .expect("restored group");
    assert!(ensure_declared_tabs(
        &mut layout,
        HostId::MainWindow,
        &[TAB_A, TAB_B, GROUP_D, GROUP_F],
        Some(&group_default())
    ));
    let joined = layout.panel(PanelId::new(1)).expect("the kept panel");
    assert_eq!(joined.tabs, vec![GROUP_D, GROUP_F, TAB_B]);
    assert_eq!(joined.active_tab, GROUP_F);
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &fallback, &HashMap::new(), &HashMap::new());
    assert_eq!(plan.panels[0].active_tab, Some(TAB_B));
    assert!(!plan.panels[0].transparent);

    // And a panel with nothing showable at all is not transparent either:
    // it is not drawn, so there is nothing to fade.
    let nothing_shown = decls(&[
        (TAB_A, transparent_meta(false)),
        (TAB_B, meta(false, None, None)),
    ]);
    let plan = plan_frame(&layout, HostId::MainWindow, &nothing_shown, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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

    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &HashMap::new());
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

    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &HashMap::new());
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

    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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

    let first = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &HashMap::new());
    // The driver stores the PANEL's width against the drawn tab, and the
    // content height it measured; both tabs eventually hold that width.
    let assumed = first.panels[0].assumed_size;
    measured.insert(TAB_A, Vec2::new(assumed.x, 150.0));
    measured.insert(TAB_B, Vec2::new(assumed.x, 460.0));
    let second = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &shown, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &hidden, &HashMap::new(), &HashMap::new());
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
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
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
        &HashMap::new(),
    );
    assert_eq!(typing.panels[0].assumed_size, Vec2::new(560.0, 480.0));

    // «Клининг» has never drawn it: it starts from the DECLARED initial size.
    let cleaning = plan_frame(
        state.layout("cleaning").expect("cleaning layout"),
        HostId::MainWindow,
        &declared,
        state.measurements("cleaning").unwrap_or(&no_measurements),
        &HashMap::new(),
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

/// A host-side flag write follows the body-write rules: it stores the value,
/// dirties the dock only when the content moved, and leaves no trace once the
/// flag is back at its default.
#[test]
fn a_host_side_extras_flag_write_dirties_only_on_a_real_change() {
    let mut state = PanelDockState::new();
    state.ensure_default_layout("typing", single_panel_default_layout);
    state.clear_dirty();

    state.set_tab_extras_flag("typing", TAB_A, "tool.ocr", true, false);
    assert!(state.is_dirty());
    assert_eq!(
        state.tab_extras("typing", TAB_A).map(|bag| bag.flag("tool.ocr", false)),
        Some(true)
    );
    state.clear_dirty();

    state.set_tab_extras_flag("typing", TAB_A, "tool.ocr", true, false);
    assert!(!state.is_dirty(), "re-writing the stored value is not a change");

    state.set_tab_extras_flag("typing", TAB_A, "tool.ocr", false, false);
    assert!(state.is_dirty());
    assert!(
        state.tab_extras("typing", TAB_A).is_none(),
        "a flag back at its default stores nothing"
    );
}

/// Focusing a tab makes it the drawn tab of its panel and expands the panel;
/// a tab already in front dirties nothing, and an unknown layout or a tab no
/// panel owns yet is refused without touching the state.
#[test]
fn focusing_a_tab_brings_it_to_the_front_of_an_expanded_panel() {
    let mut state = PanelDockState::new();
    assert!(!state.focus_tab("typing", TAB_B), "no layout yet");
    state.ensure_default_layout("typing", two_tab_default_layout);
    let panel = PanelId::new(0);
    state
        .layout_mut("typing")
        .expect("layout")
        .set_collapsed(panel, true)
        .expect("collapse");
    state.clear_dirty();

    assert!(state.focus_tab("typing", TAB_B));
    let node = state.layout("typing").and_then(|layout| layout.panel(panel)).expect("panel");
    assert_eq!(node.active_tab, TAB_B);
    assert!(!node.collapsed);
    assert!(state.is_dirty());
    state.clear_dirty();

    assert!(state.focus_tab("typing", TAB_B));
    assert!(!state.is_dirty(), "a tab already in front is not a change");

    assert!(!state.focus_tab("typing", TAB_C), "no panel owns the tab");
    assert!(!state.is_dirty());
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

    let main = plan_frame(layout, HostId::MainWindow, &decls, &HashMap::new(), &HashMap::new());
    let sub = plan_frame(layout, HostId::SubWindow(0), &decls, &HashMap::new(), &HashMap::new());
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

/// A state restored from a config that stored panels in sub-window 2.
fn state_with_a_persisted_sub_window() -> PanelDockState {
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
        None,
        Vec2::new(400.0, 500.0),
    )]);
    state
}

#[test]
fn embedded_mode_redocks_persisted_sub_windows_and_persists_that() {
    let mut state = state_with_a_persisted_sub_window();
    assert!(!state.is_dirty());
    state.apply_window_mode(WindowMode::Embedded);
    assert!(!state.detach_allowed);
    assert!(state.sub_windows().is_empty());
    let layout = state.layout("typing").expect("layout");
    let panel = layout.panel_of_tab(TAB_A).expect("the tab survives the re-dock");
    assert_eq!(
        layout.panel(panel).map(|node| node.host),
        Some(HostId::MainWindow)
    );
    // The re-docked arrangement is what persistence stores: no windows left.
    let snapshot = state
        .take_dirty_layouts()
        .expect("re-docking dirties the state");
    assert!(snapshot.sub_windows.is_empty());
    // A further Embedded pass has nothing left to do.
    state.apply_window_mode(WindowMode::Embedded);
    assert!(!state.is_dirty());
}

#[test]
fn separate_mode_keeps_the_sub_windows() {
    let mut state = state_with_a_persisted_sub_window();
    state.apply_window_mode(WindowMode::Separate);
    assert!(state.detach_allowed);
    assert_eq!(state.sub_windows().len(), 1);
    assert_eq!(state.sub_windows()[0].index, 2);
    assert!(!state.is_dirty());
}

#[test]
fn a_mode_flip_redocks_and_is_not_undone_by_flipping_back() {
    let mut state = state_with_a_sub_window();
    state.apply_window_mode(WindowMode::Separate);
    assert_eq!(state.sub_windows().len(), 1);
    state.apply_window_mode(WindowMode::Embedded);
    assert!(state.sub_windows().is_empty());
    assert!(!state.opened_sub_windows.contains(&0));
    state.apply_window_mode(WindowMode::Separate);
    assert!(state.detach_allowed);
    assert!(state.sub_windows().is_empty());
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

#[test]
fn a_pinned_bottom_height_repaints_only_when_it_is_news() {
    // The first measurement is always news: nothing was reserved for it.
    assert!(pinned_height_changed(None, 40.0));
    assert!(pinned_height_changed(Some(20.0), 40.0));
    assert!(!pinned_height_changed(Some(40.0), 40.4));
}

#[test]
fn a_header_measurement_repaints_only_when_it_is_news() {
    // A panel never drawn was solved as a one-row header: measuring exactly
    // that costs no extra frame.
    assert!(!header_metrics_changed(None, PanelHeaderMetrics::default()));
    assert!(header_metrics_changed(None, PanelHeaderMetrics::new(22.0, 0.0)));
    assert!(header_metrics_changed(None, PanelHeaderMetrics::new(0.0, 160.0)));
    let settled = PanelHeaderMetrics::new(22.0, 160.0);
    assert!(!header_metrics_changed(Some(settled), PanelHeaderMetrics::new(22.4, 160.4)));
    assert!(header_metrics_changed(Some(settled), PanelHeaderMetrics::new(0.0, 160.0)));
}

const WRAP_LAYOUT_KEY: &str = "test.wrap";
const WRAP_TABS: [(TabId, &str); 3] = [
    (TabId::new("test.wrap.a"), "Слои"),
    (TabId::new("test.wrap.b"), "Действия"),
    (TabId::new("test.wrap.c"), "Превью текста"),
];

/// One free panel holding the three wrap tabs, pinned by hand to `WIDTH`
/// points wide and 220 tall.
fn wrap_layout<const WIDTH: u16>() -> DockLayout {
    let mut layout = DockLayout::new();
    let mut panel = PanelNode::new(
        PanelId::new(0),
        HostId::MainWindow,
        WRAP_TABS.iter().map(|(id, _)| *id).collect(),
    )
    .expect("three distinct tabs make a valid panel");
    panel.pos = Pos2::new(20.0, 20.0);
    panel.size_override = Some(Vec2::new(f32::from(WIDTH), 220.0));
    layout.insert_panel(panel).expect("the only panel");
    layout
}

/// Drives `frames` headless frames of a dock showing the three wrap tabs in
/// the layout `build` makes, returning the state and the panel's rect per
/// frame.
fn drive_wrap_dock(build: fn() -> DockLayout, frames: usize) -> (PanelDockState, Vec<Option<Rect>>) {
    let ctx = egui::Context::default();
    let mut state = PanelDockState::default();
    state.ensure_default_layout(WRAP_LAYOUT_KEY, build);
    let mut rects = Vec::new();
    for _ in 0..frames {
        // `FullOutput` carries this test's paint output only; nothing is
        // rendered, so dropping it is the whole intent.
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut dock = PanelDock::begin(
                ui.ctx(),
                &mut state,
                DockArea {
                    rect: AREA,
                    layout_key: WRAP_LAYOUT_KEY,
                },
            );
            for (id, title) in WRAP_TABS {
                dock.tab(id).title(move || title).show(|ui, _cx: &mut ()| {
                    ui.label("body");
                });
            }
            let output = dock.end(&mut ());
            rects.push(output.panel_rect(PanelId::new(0)));
        })
        .drop_without_applying_deltas();
    }
    (state, rects)
}

fn wrap_header(state: &PanelDockState) -> PanelHeaderMetrics {
    state
        .header_measurements(WRAP_LAYOUT_KEY)
        .and_then(|headers| headers.get(&PanelId::new(0)))
        .expect("the panel was drawn")
        .metrics
}

/// End to end through the driver: a panel pinned narrower than its longest
/// caption is laid out at the caption-driven minimum width on the next
/// frame, its captions wrap, and the layout then SETTLES — the same rect on
/// every later frame, so width -> rows -> height never feeds back.
#[test]
fn a_panel_narrower_than_its_longest_caption_widens_once_and_settles() {
    let (state, rects) = drive_wrap_dock(wrap_layout::<60>, 6);
    let header = wrap_header(&state);
    let settled = rects
        .last()
        .copied()
        .flatten()
        .expect("the panel is on screen");
    assert!(header.min_width > 60.0, "{header:?}");
    assert!(header.extra_height > 0.0, "the captions wrapped: {header:?}");
    assert!((settled.width() - header.min_width).abs() < 0.01, "{settled:?} vs {header:?}");
    // Settled: nothing moves any more.
    assert_eq!(rects[3], rects[5]);
    assert_eq!(rects[4], rects[5]);
    // The user's manual size is a request, not something the floor rewrites.
    let stored = state
        .layout(WRAP_LAYOUT_KEY)
        .and_then(|layout| layout.panel(PanelId::new(0)))
        .and_then(|panel| panel.size_override);
    assert_eq!(stored, Some(Vec2::new(60.0, 220.0)));
}

/// A tab's stored measurement counts its panel's header as ONE row: the same
/// tab drawn in a wrapped and in a one-row panel stores the same height, so
/// an inactive tab cannot keep a widened panel a row too tall and a moved tab
/// carries no rows of its old panel.
#[test]
fn a_stored_tab_height_leaves_the_wrapped_rows_out() {
    let (narrow, _) = drive_wrap_dock(wrap_layout::<60>, 6);
    let (wide, _) = drive_wrap_dock(wrap_layout::<600>, 6);
    assert!(wrap_header(&narrow).extra_height > 0.0);
    assert_eq!(wrap_header(&wide).extra_height, 0.0);
    let tab = WRAP_TABS[0].0;
    let narrow_height = narrow.measured_size(WRAP_LAYOUT_KEY, tab).expect("measured").y;
    let wide_height = wide.measured_size(WRAP_LAYOUT_KEY, tab).expect("measured").y;
    assert!((narrow_height - wide_height).abs() < 0.01, "{narrow_height} vs {wide_height}");
}

/// The plan adds the panel's CURRENT rows to a measured height — and only
/// when the cached header was measured with exactly the tabs shown now; a
/// declared size is an outer size and gets nothing added.
#[test]
fn the_plan_adds_the_panels_current_rows_to_measured_tabs_only() {
    let mut layout = DockLayout::new();
    layout.insert_panel(panel_with(0, &[TAB_A, TAB_B])).expect("insert 0");
    layout.insert_panel(panel_with(1, &[TAB_C])).expect("insert 1");
    let decls: BTreeMap<TabId, TabMeta> = [
        (TAB_A, meta(true, None, None)),
        (TAB_B, meta(true, None, None)),
        (TAB_C, meta(true, None, Some(Vec2::new(200.0, 150.0)))),
    ]
    .into_iter()
    .collect();
    let measured: HashMap<TabId, Vec2> = [(TAB_A, Vec2::new(300.0, 200.0))].into_iter().collect();
    let rows = PanelHeaderMetrics::new(22.0, 180.0);
    let matching: HashMap<PanelId, MeasuredHeader> = [
        (PanelId::new(0), MeasuredHeader { tabs: vec![TAB_A, TAB_B], metrics: rows }),
        (PanelId::new(1), MeasuredHeader { tabs: vec![TAB_C], metrics: rows }),
    ]
    .into_iter()
    .collect();
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &matching);
    assert_eq!(plan.desired.get(PanelId::new(0)), Some(Vec2::new(300.0, 222.0)));
    assert_eq!(plan.desired.get(PanelId::new(1)), Some(Vec2::new(200.0, 150.0)));
    assert_eq!(plan.headers.get(PanelId::new(0)), rows);
    assert_eq!(plan.panels[0].active_request, Some(Vec2::new(300.0, 222.0)));

    // Measured with other captions (a reused id, a tab hidden since): not
    // this panel's header, so it is solved as a fresh one-row header.
    let stale: HashMap<PanelId, MeasuredHeader> =
        [(PanelId::new(0), MeasuredHeader { tabs: vec![TAB_C], metrics: rows })].into_iter().collect();
    let plan = plan_frame(&layout, HostId::MainWindow, &decls, &measured, &stale);
    assert_eq!(plan.desired.get(PanelId::new(0)), Some(Vec2::new(300.0, 200.0)));
    assert!(plan.headers.is_empty());
    assert_eq!(plan.panels[0].header, None);
}

/// Header measurements never outlive their panel: a panel gone from the
/// layout loses its entry, and «Сбросить раскладку» drops the whole key,
/// because the default rebuilds panels under ids the old layout used.
#[test]
fn header_measurements_are_dropped_with_their_panel_and_on_reset() {
    let mut state = PanelDockState::new();
    state.ensure_default_layout("typing", single_panel_default_layout);
    {
        let layout = state.layout_mut("typing").expect("layout");
        layout
            .insert_panel(panel_with(9, &[TAB_C]))
            .expect("a panel the user created");
    }
    let rows = PanelHeaderMetrics::new(22.0, 180.0);
    state.remember_header_metrics("typing", PanelId::new(0), &[TAB_A], rows);
    state.remember_header_metrics("typing", PanelId::new(9), &[TAB_C], rows);
    state.remember_header_metrics("typing", PanelId::new(4), &[TAB_B], rows);
    state.prune_header_metrics("typing");
    let kept: BTreeSet<PanelId> = state
        .header_measurements("typing")
        .expect("entries remain")
        .keys()
        .copied()
        .collect();
    assert_eq!(kept, [PanelId::new(0), PanelId::new(9)].into_iter().collect());

    assert!(state.reset_layout("typing"));
    assert!(state.header_measurements("typing").is_none());
}
