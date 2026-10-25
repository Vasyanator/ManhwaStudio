/*
File: crates/ms-tab-ps-editor/src/tests.rs

Purpose:
Unit tests of the crate root and its split-out children (`#[cfg(test)] mod tests;` in `lib.rs`):
overlay pointer gate, marquee dashing, compositing, dock defaults, layers-panel selection, band moves,
clean write-back, undo and the effects recenter math.

Notes:
Pure: no files, no GPU. Reaches crate items through `use super::*`.
*/

use super::*;
use crate::layers_panel::{group_row_is_selected, row_is_selected, selectable_row_order};
use crate::raster_effects::effects_recenter_offset;
use layers::LayerStack;
use std::collections::BTreeSet;

/// The canvas rect the overlay-gate tests position their pointer inside.
const OVERLAY_RECT: Rect = Rect {
    min: Pos2::new(0.0, 0.0),
    max: Pos2::new(100.0, 100.0),
};

/// The plain case: bare canvas, no gesture — the preview follows the cursor.
#[test]
fn the_overlay_follows_the_pointer_on_bare_canvas() {
    let p = Pos2::new(10.0, 10.0);
    assert_eq!(
        overlay_pointer(Some(p), OVERLAY_RECT, false, false),
        Some(p)
    );
}

/// The reported bug: hovering a floating dock panel with no gesture running must HIDE the tool
/// preview, not merely stop it from acting. `rect.contains` alone cannot do this — the canvas
/// is the full-area background, so a panel's pixels are inside the canvas rect.
#[test]
fn the_overlay_is_dropped_over_a_panel_when_no_gesture_runs() {
    let p = Pos2::new(10.0, 10.0);
    assert_eq!(
        overlay_pointer(Some(p), OVERLAY_RECT, true, false),
        None,
        "a preview must not be painted under a floating panel"
    );
}

/// The other half of the rule, and the reason the in-flight term is asked of the TOOL: a
/// gesture that legitimately started on bare canvas keeps its preview while it is dragged over
/// a panel, exactly as a pan started on bare canvas survives the crossing.
#[test]
fn a_gesture_in_flight_keeps_its_overlay_over_a_panel() {
    let p = Pos2::new(10.0, 10.0);
    assert_eq!(
        overlay_pointer(Some(p), OVERLAY_RECT, true, true),
        Some(p),
        "a preview dragged over a panel by a running gesture must stay visible"
    );
}

/// The occlusion gate is added to the rect filter, never substituted for it: a pointer outside
/// the program-tab area has nothing to preview, in flight or not.
#[test]
fn the_overlay_still_requires_the_pointer_inside_the_canvas_rect() {
    let outside = Pos2::new(200.0, 10.0);
    assert_eq!(
        overlay_pointer(Some(outside), OVERLAY_RECT, false, false),
        None
    );
    assert_eq!(
        overlay_pointer(Some(outside), OVERLAY_RECT, false, true),
        None
    );
    assert_eq!(overlay_pointer(None, OVERLAY_RECT, false, true), None);
}

/// The default arrangement is what the dock is handed on a first run AND the dictionary
/// `panel_dock::persist` resolves stored tab keys against, so it must be well-formed and must
/// name every tab this program tab can declare — a tab missing here would be dropped from the
/// user's stored arrangement on every load.
#[test]
fn the_default_dock_layout_places_the_seven_ps_editor_panels() {
    let layout = ps_editor_default_dock_layout();
    assert_eq!(layout.validate(), Ok(()));
    assert_eq!(layout.panels().len(), 7);

    let declared: BTreeSet<TabId> = layout
        .panels()
        .iter()
        .flat_map(|panel| panel.tabs.iter().copied())
        .collect();
    let expected: BTreeSet<TabId> = [
        PS_EDITOR_MAIN_TAB,
        PS_EDITOR_TOOLS_TAB,
        PS_EDITOR_ACTIVE_TOOL_TAB,
        PS_EDITOR_HOTKEYS_TAB,
        PS_EDITOR_LAYERS_TAB,
        PS_EDITOR_CORRECTION_TAB,
        // The active tool's own panel is normally hidden, but it is still DECLARED every frame,
        // so it must be named here: `panel_dock::persist` resolves stored tab keys against this
        // layout and would drop it from the user's arrangement on every load.
        PS_EDITOR_TOOL_PANEL_TAB,
    ]
    .into_iter()
    .collect();
    assert_eq!(
        declared, expected,
        "the default layout must name exactly the seven tabs the program tab declares"
    );
}

/// The two columns the doc comment promises: «PS редактор» roots the left chain at the viewport's
/// left edge, «Слои» roots the right one at its right edge, and each dependant hangs below its
/// own target. Anchors, not rects: the solver is what turns these into geometry.
#[test]
fn the_default_dock_layout_builds_two_columns() {
    let layout = ps_editor_default_dock_layout();
    let panel_of = |tab: TabId| {
        let id = layout
            .panel_of_tab(tab)
            .unwrap_or_else(|| panic!("{tab} has a panel"));
        layout
            .panel(id)
            .unwrap_or_else(|| panic!("{tab}'s panel exists"))
    };
    let main = panel_of(PS_EDITOR_MAIN_TAB);
    let layers = panel_of(PS_EDITOR_LAYERS_TAB);
    assert_eq!(
        main.anchor,
        PanelAnchor::ViewportEdge {
            edge: DockEdge::Left,
            along: 0.0
        }
    );
    assert_eq!(
        layers.anchor,
        PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0
        }
    );
    assert_eq!(
        panel_of(PS_EDITOR_TOOLS_TAB).anchor,
        PanelAnchor::Panel {
            target: main.id,
            edge: DockEdge::Bottom,
            align: 0.0
        }
    );
    assert_eq!(
        panel_of(PS_EDITOR_ACTIVE_TOOL_TAB).anchor,
        PanelAnchor::Panel {
            target: panel_of(PS_EDITOR_TOOLS_TAB).id,
            edge: DockEdge::Bottom,
            align: 0.0
        }
    );
    assert_eq!(
        panel_of(PS_EDITOR_HOTKEYS_TAB).anchor,
        PanelAnchor::Panel {
            target: layers.id,
            edge: DockEdge::Bottom,
            align: 0.0
        }
    );
    // «Коррекция» hangs under «Горячие клавиши», not under «Слои»: sharing «Слои» + Bottom
    // with the hotkeys panel would solve both to one rect and bury this one.
    // The tool panel continues the LEFT chain under «Выбранный инструмент». Same sibling rule
    // as «Коррекция»: it must not share «Выбранный инструмент»'s own target and edge.
    assert_eq!(
        panel_of(PS_EDITOR_TOOL_PANEL_TAB).anchor,
        PanelAnchor::Panel {
            target: panel_of(PS_EDITOR_ACTIVE_TOOL_TAB).id,
            edge: DockEdge::Bottom,
            align: 0.0
        }
    );
    assert_eq!(
        panel_of(PS_EDITOR_CORRECTION_TAB).anchor,
        PanelAnchor::Panel {
            target: panel_of(PS_EDITOR_HOTKEYS_TAB).id,
            edge: DockEdge::Bottom,
            align: 0.0
        }
    );
}

/// The tab-level shortcut list is rendered as a two-column grid, so a blank half is an
/// authoring bug (a forgotten key) or a translation bug (an empty catalog value) and must not
/// reach the UI.
///
/// The reference catalog is installed first on purpose: `t!` falls back to the KEY text on a
/// miss, which is never empty, so without a real catalog an empty translation would slip past.
#[test]
fn the_common_hotkey_rows_are_well_formed() {
    let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
    ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

    let rows = ps_editor_common_hotkey_rows();
    assert!(
        !rows.is_empty(),
        "the tab has shortcuts, so the list is never empty"
    );
    for row in &rows {
        assert!(
            !row.action.trim().is_empty(),
            "a common hotkey row has an empty action (keys {:?})",
            row.keys
        );
        assert!(
            !row.keys.trim().is_empty(),
            "common hotkey row {:?} has empty keys",
            row.action
        );
    }
}

/// One visibility flag key together with the accessor of the field it must be read into.
type VisibilityKeyProbe = (&'static str, fn(&PsEditorPanelVisibility) -> bool);

/// One `entries()` row under test: its flag key, its menu caption, and the mutation that hides
/// that panel and no other.
type VisibilityRowProbe = (&'static str, &'static str, fn(&mut PsEditorPanelVisibility));

/// The five visibility flags default to "shown" when the dock has stored nothing, and each one
/// is read from its OWN key — a copy-paste slip between the five keys would otherwise show up
/// only as two panels that hide together.
///
/// The stored case has to go through a real `TabExtras` bag seeded with
/// [`PanelDockState::put_tab_extras`]: on a bare `PanelDockState` the MAIN tab has no bag at
/// all, so `read` leaves through its `let Some(..) else` branch and the five `extras.flag(..)`
/// lines — the thing being tested — never run. One key is turned off at a time, because with
/// all five flags equal any permutation of the keys looks identical.
#[test]
fn panel_visibility_defaults_to_all_shown_and_reads_each_key() {
    let layout_key = AppTab::PsEditor.key();

    // Nothing stored: no bag for the MAIN tab, so every panel falls back to shown.
    let state = PanelDockState::new();
    let visibility = PsEditorPanelVisibility::read(&state, layout_key);
    assert!(visibility.tools);
    assert!(visibility.active_tool);
    assert!(visibility.hotkeys);
    assert!(visibility.layers);
    assert!(visibility.correction);

    // Each flag key, with the field it must land in.
    let cases: [VisibilityKeyProbe; 5] = [
        (PS_EDITOR_FLAG_TOOLS_PANEL, |v| v.tools),
        (PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL, |v| v.active_tool),
        (PS_EDITOR_FLAG_HOTKEYS_PANEL, |v| v.hotkeys),
        (PS_EDITOR_FLAG_LAYERS_PANEL, |v| v.layers),
        (PS_EDITOR_FLAG_CORRECTION_PANEL, |v| v.correction),
    ];
    for (key, field) in cases {
        let mut state = PanelDockState::new();
        let mut extras = TabExtras::default();
        extras.set_flag(
            key,
            !PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            PS_EDITOR_PANEL_VISIBLE_DEFAULT,
        );
        state.put_tab_extras(layout_key, PS_EDITOR_MAIN_TAB, extras);

        let visibility = PsEditorPanelVisibility::read(&state, layout_key);
        assert!(
            !field(&visibility),
            "`{key}` must be read by its own field, not by another panel's"
        );
        let hidden = cases
            .iter()
            .filter(|(_, other)| !other(&visibility))
            .count();
        assert_eq!(hidden, 1, "`{key}` must hide exactly one panel — its own");
    }
}

/// `entries()` is the single owner of the (flag key, menu caption, field) triple: the «Панели…»
/// checkbox, the persisted flag and the panel that actually hides all come from ONE row. A
/// copy-paste swap between two rows would toggle the wrong panel and persist it under the wrong
/// key, so each field is probed on its own — with all four `true` any permutation looks alike.
///
/// The `TabId` leg of the pairing is deliberately absent here: `entries()` carries no `TabId`.
/// It lives on the `dock.tab(..).title(..).visible(..)` chains in `draw`, where the caption and
/// the visibility field sit on the same builder — which is exactly why the caption is asserted
/// here: a row whose caption belongs to another panel is the same defect seen from the menu.
///
/// The reference catalog is installed first on purpose: `t!` falls back to the KEY text on a
/// miss, so without a real catalog every caption would compare equal to its own key text.
#[test]
fn each_entries_row_pairs_its_own_key_caption_and_field() {
    let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
    ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

    let all_shown = PsEditorPanelVisibility {
        tools: true,
        active_tool: true,
        hotkeys: true,
        layers: true,
        correction: true,
    };
    let cases: [VisibilityRowProbe; 5] = [
        (PS_EDITOR_FLAG_TOOLS_PANEL, t!("ps_editor.tab.tools"), |v| {
            v.tools = false;
        }),
        (
            PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
            t!("ps_editor.tab.active_tool"),
            |v| v.active_tool = false,
        ),
        (
            PS_EDITOR_FLAG_HOTKEYS_PANEL,
            t!("ps_editor.tab.hotkeys"),
            |v| v.hotkeys = false,
        ),
        (
            PS_EDITOR_FLAG_LAYERS_PANEL,
            t!("ps_editor.tab.layers"),
            |v| {
                v.layers = false;
            },
        ),
        (
            PS_EDITOR_FLAG_CORRECTION_PANEL,
            t!("ps_editor.tab.correction"),
            |v| {
                v.correction = false;
            },
        ),
    ];
    for (key, caption, hide) in cases {
        let mut visibility = all_shown;
        hide(&mut visibility);
        let hidden: Vec<(&'static str, &'static str)> = visibility
            .entries()
            .into_iter()
            .filter(|(_, _, value)| !**value)
            .map(|(row_key, row_caption, _)| (row_key, row_caption))
            .collect();
        assert_eq!(
            hidden,
            vec![(key, caption)],
            "hiding the panel of `{key}` must report exactly that key and its own caption"
        );
    }

    let mut visibility = all_shown;
    let keys: Vec<&'static str> = visibility
        .entries()
        .into_iter()
        .map(|(key, _, _)| key)
        .collect();
    assert_eq!(
        keys,
        vec![
            PS_EDITOR_FLAG_TOOLS_PANEL,
            PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
            PS_EDITOR_FLAG_HOTKEYS_PANEL,
            PS_EDITOR_FLAG_LAYERS_PANEL,
            PS_EDITOR_FLAG_CORRECTION_PANEL,
        ],
        "the menu rows and the persisted keys are one list, in one order"
    );
}

fn filled(size: [usize; 2], color: Color32) -> ColorImage {
    ColorImage::filled(size, color)
}

/// Selection covering the inner pixels (1,1)..=(2,2) of a 4x4 page.
fn inner_selection() -> Selection {
    let mut sel = Selection::empty(4, 4);
    sel.set_rect(1, 1, 3, 3);
    sel
}

/// Collects every run `walk_dash_runs` emits for `path` at `dash`, with an unrestrictive budget.
fn collect_runs(path: &[Pos2], dash: f32) -> Vec<DashRun> {
    let mut runs = Vec::new();
    let emitted = walk_dash_runs(path, dash, 10_000, |r| runs.push(r));
    assert_eq!(
        emitted,
        runs.len(),
        "returned count must match the emitted runs"
    );
    runs
}

/// Asserts the runs tile `path` end to end without gaps or overlaps.
fn assert_contiguous(runs: &[DashRun], start: Pos2, end: Pos2) {
    let (Some(first), Some(last)) = (runs.first(), runs.last()) else {
        panic!("expected at least one run");
    };
    assert!(
        (first.from - start).length() < 1e-3,
        "first run must start at the path start"
    );
    for pair in runs.windows(2) {
        assert!(
            (pair[0].to - pair[1].from).length() < 1e-3,
            "runs must be contiguous: {:?} -> {:?}",
            pair[0],
            pair[1]
        );
    }
    assert!(
        (last.to - end).length() < 1e-3,
        "last run must end at the path end"
    );
}

#[test]
fn dash_phase_carries_across_short_staircase_segments() {
    // The case the arc-length rewrite exists for: a pixel-traced lasso outline is a staircase of
    // one-pixel steps, each far shorter than the dash period. Dashing every segment on its own
    // would restart the phase ten times and paint a solid line; here the phase accumulates, so
    // the colour flips only where 4px of ARC LENGTH have been walked.
    let path: Vec<Pos2> = (0u8..=10).map(|i| Pos2::new(f32::from(i), 0.0)).collect();
    let runs = collect_runs(&path, 4.0);
    // One run per segment (each segment is shorter than a dash, so none is subdivided).
    assert_eq!(
        runs.len(),
        10,
        "expected one run per unit segment, got {runs:?}"
    );
    assert_contiguous(&runs, Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0));
    // 4 black, 4 white, then the 2px tail of the next black dash.
    let expected = [
        true, true, true, true, false, false, false, false, true, true,
    ];
    let actual: Vec<bool> = runs.iter().map(|r| r.black).collect();
    assert_eq!(
        actual, expected,
        "dash colours must follow cumulative arc length"
    );
}

#[test]
fn one_long_segment_holds_several_dashes() {
    // The mirror case: a single collinear-merged edge (a rectangular selection is ~5 points)
    // must be subdivided into as many dashes as it is long.
    let path = [Pos2::new(0.0, 0.0), Pos2::new(20.0, 0.0)];
    let runs = collect_runs(&path, 4.0);
    assert_eq!(runs.len(), 5, "20px / 4px dash = 5 runs, got {runs:?}");
    assert_contiguous(&runs, path[0], path[1]);
    for (i, run) in runs.iter().enumerate() {
        let len = (run.to - run.from).length();
        assert!(
            (len - 4.0).abs() < 1e-3,
            "run {i} should be one dash long, got {len}"
        );
        assert_eq!(run.black, i % 2 == 0, "colours must alternate, run {i}");
    }
}

#[test]
fn path_shorter_than_one_dash_emits_a_single_black_run() {
    let path = [Pos2::new(0.0, 0.0), Pos2::new(1.0, 0.0)];
    let runs = collect_runs(&path, 4.0);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].black, "the pattern starts black");
    assert_contiguous(&runs, path[0], path[1]);
}

#[test]
fn degenerate_dash_inputs_emit_nothing() {
    let path = [Pos2::new(0.0, 0.0), Pos2::new(20.0, 0.0)];
    // Fewer than two points, a non-positive/non-finite dash, or a zero budget: no runs, no panic.
    assert_eq!(walk_dash_runs(&path[..1], 4.0, 10, |_| {}), 0);
    assert_eq!(walk_dash_runs(&path, 0.0, 10, |_| {}), 0);
    assert_eq!(walk_dash_runs(&path, f32::NAN, 10, |_| {}), 0);
    assert_eq!(walk_dash_runs(&path, 4.0, 0, |_| {}), 0);
    // A zero-length path (all points coincide) produces nothing either.
    let collapsed = [
        Pos2::new(3.0, 3.0),
        Pos2::new(3.0, 3.0),
        Pos2::new(3.0, 3.0),
    ];
    assert_eq!(walk_dash_runs(&collapsed, 4.0, 10, |_| {}), 0);
}

#[test]
fn dash_run_budget_is_respected() {
    // 100px at a 1px dash would be 100 runs; the budget cuts the walk short instead.
    let path = [Pos2::new(0.0, 0.0), Pos2::new(100.0, 0.0)];
    let mut count = 0usize;
    let emitted = walk_dash_runs(&path, 1.0, 7, |_| count += 1);
    assert_eq!(emitted, 7);
    assert_eq!(count, 7);
}

#[test]
fn recenter_offset_zero_when_growth_is_symmetric() {
    // Base 10x10 grown to 14x14 with the content centered (origin = 2,2): the content center is
    // unchanged, so no recenter is needed regardless of transform.
    let t = LayerTransform {
        center: Vec2::new(5.0, 5.0),
        rotation: 0.0,
        scale: 1.0,
    };
    let off = effects_recenter_offset([14, 14], [2, 2], [10, 10], t);
    assert!(
        off.length() < 1e-4,
        "symmetric growth needs no recenter, got {off:?}"
    );
}

#[test]
fn recenter_offset_anchors_asymmetric_growth() {
    // Base 10x10 grown to 16x10 with the content at origin (0,0): the new center sits 3px right of
    // the base center, so the layer center must shift +3px in x (identity transform).
    let t = LayerTransform {
        center: Vec2::ZERO,
        rotation: 0.0,
        scale: 1.0,
    };
    let off = effects_recenter_offset([16, 10], [0, 0], [10, 10], t);
    assert!((off.x - 3.0).abs() < 1e-4, "x offset wrong: {off:?}");
    assert!(off.y.abs() < 1e-4, "y offset wrong: {off:?}");
}

#[test]
fn recenter_offset_applies_scale_then_rotation() {
    // Same +3px base-local x delta, but the base layer is scaled 2x and rotated 90°. Scale doubles
    // it to 6px, then a 90° rotation maps +x to +y (within float tolerance).
    let t = LayerTransform {
        center: Vec2::ZERO,
        rotation: std::f32::consts::FRAC_PI_2,
        scale: 2.0,
    };
    let off = effects_recenter_offset([16, 10], [0, 0], [10, 10], t);
    assert!(off.x.abs() < 1e-3, "x should be ~0 after 90° rot: {off:?}");
    assert!(
        (off.y - 6.0).abs() < 1e-3,
        "y should be ~6 (3*2 scaled, rotated): {off:?}"
    );
}

#[test]
fn over_composites_premultiplied() {
    assert_eq!(over(Color32::RED, Color32::TRANSPARENT), Color32::RED);
    assert_eq!(over(Color32::TRANSPARENT, Color32::GREEN), Color32::GREEN);
    // Opaque src fully replaces dst.
    assert_eq!(over(Color32::BLUE, Color32::RED), Color32::BLUE);
}

/// A layer opacity scales all four premultiplied channels, which is what makes
/// `scale_premultiplied` + `over` reproduce what the eyedropper's user actually sees.
#[test]
fn scale_premultiplied_dims_every_channel() {
    assert_eq!(scale_premultiplied(Color32::RED, 1.0), Color32::RED);
    assert_eq!(
        scale_premultiplied(Color32::RED, 0.5),
        Color32::from_rgba_premultiplied(128, 0, 0, 128)
    );
    assert_eq!(scale_premultiplied(Color32::RED, 0.0), Color32::TRANSPARENT);
}

/// One key press event, as the backend delivers it.
fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// `[` and `Shift+[` must mean DIFFERENT things — the whole reason the frame's events are
/// scanned instead of `key_pressed`, which ignores modifiers entirely.
#[test]
fn bracket_keys_separate_size_from_hardness_by_shift() {
    let plain = collect_brush_key_actions(&[
        key_event(egui::Key::OpenBracket, egui::Modifiers::NONE),
        key_event(egui::Key::CloseBracket, egui::Modifiers::NONE),
    ]);
    assert_eq!(
        plain,
        vec![
            BrushKeyAction::DiameterStep { up: false },
            BrushKeyAction::DiameterStep { up: true }
        ]
    );
    let shifted = collect_brush_key_actions(&[
        key_event(egui::Key::OpenBracket, egui::Modifiers::SHIFT),
        key_event(egui::Key::CloseBracket, egui::Modifiers::SHIFT),
    ]);
    assert_eq!(
        shifted,
        vec![
            BrushKeyAction::HardnessStep { up: false },
            BrushKeyAction::HardnessStep { up: true }
        ]
    );
}

/// Digits set opacity, Shift+digits set flow, and `0` means 100 %.
#[test]
fn digit_keys_set_opacity_and_shifted_digits_set_flow() {
    let acts = collect_brush_key_actions(&[
        key_event(egui::Key::Num3, egui::Modifiers::NONE),
        key_event(egui::Key::Num0, egui::Modifiers::NONE),
        key_event(egui::Key::Num7, egui::Modifiers::SHIFT),
    ]);
    assert_eq!(
        acts,
        vec![
            BrushKeyAction::Opacity(0.3),
            BrushKeyAction::Opacity(1.0),
            BrushKeyAction::Flow(0.7),
        ]
    );
}

/// A brush key carrying Ctrl/Cmd or Alt belongs to someone else (Ctrl+D deselects, Alt is the
/// eyedropper) and must be ignored, and a key RELEASE must never act.
#[test]
fn brush_keys_ignore_other_modifiers_and_releases() {
    let ctrl = egui::Modifiers { command: true, ctrl: true, ..Default::default() };
    let alt = egui::Modifiers { alt: true, ..Default::default() };
    assert!(collect_brush_key_actions(&[key_event(egui::Key::Num1, ctrl)]).is_empty());
    assert!(collect_brush_key_actions(&[key_event(egui::Key::OpenBracket, alt)]).is_empty());
    let release = egui::Event::Key {
        key: egui::Key::CloseBracket,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert!(collect_brush_key_actions(&[release]).is_empty());
}

/// The legacy `-` / `=` / `+` size keys stay wired, `+` with or without Shift.
#[test]
fn the_legacy_size_keys_still_scale_the_diameter() {
    let acts = collect_brush_key_actions(&[
        key_event(egui::Key::Minus, egui::Modifiers::NONE),
        key_event(egui::Key::Equals, egui::Modifiers::NONE),
        key_event(egui::Key::Plus, egui::Modifiers::SHIFT),
    ]);
    assert_eq!(
        acts,
        vec![
            BrushKeyAction::DiameterScale { down: true },
            BrushKeyAction::DiameterScale { down: false },
            BrushKeyAction::DiameterScale { down: false },
        ]
    );
}

#[test]
fn composite_to_page_blends_bottom_then_top() {
    let size = [2, 2];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let bottom = stack.add_raster_layer();
    stack.layer_mut(bottom).unwrap().image = filled(size, Color32::BLUE);
    let top = stack.add_raster_layer();
    {
        let img = &mut stack.layer_mut(top).unwrap().image;
        *img = filled(size, Color32::TRANSPARENT);
        img.pixels[0] = Color32::RED; // opaque only at (0,0)
    }
    let out = composite_to_page(
        &[stack.layer(bottom).unwrap(), stack.layer(top).unwrap()],
        size,
    );
    assert_eq!(
        out.pixels[0],
        Color32::RED,
        "top opaque pixel wins over bottom"
    );
    assert_eq!(
        out.pixels[1],
        Color32::BLUE,
        "bottom shows where top is transparent"
    );
}

#[test]
fn copy_builds_layer_from_selection_without_touching_sources() {
    let size = [4, 4];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::RED),
        filled(size, Color32::TRANSPARENT),
    );
    let sel = inner_selection();
    let source_id = stack.layers()[0].id;

    clip_into_new_layer(&mut stack, &sel, ClipMode::Copy, &[source_id]);

    // A new raster layer is on top, cropped to the 2x2 selection bounds and centered on it.
    assert_eq!(stack.layers().len(), 3);
    let top = stack.layers().last().unwrap();
    assert_eq!(top.image.size, [2, 2]);
    assert!(top.image.pixels.iter().all(|&p| p == Color32::RED));
    assert_eq!(top.transform.center, egui::Vec2::new(2.0, 2.0));
    // The source is untouched (still page-sized red).
    assert_eq!(stack.layers()[0].image.size, [4, 4]);
    assert_eq!(stack.layers()[0].image.pixels[4 + 1], Color32::RED);
}

#[test]
fn cut_clears_mutable_layers_but_never_the_source() {
    let size = [4, 4];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::RED),
        filled(size, Color32::GREEN),
    );
    let source_id = stack.layers()[0].id;
    let clean_id = stack.layers()[1].id;
    let sel = inner_selection();

    clip_into_new_layer(&mut stack, &sel, ClipMode::Cut, &[source_id, clean_id]);

    // Clean (mutable) is cleared inside the selection; source (immutable) is not.
    assert_eq!(
        stack.layer(clean_id).unwrap().image.pixels[4 + 1],
        Color32::TRANSPARENT
    );
    assert_eq!(
        stack.layer(source_id).unwrap().image.pixels[4 + 1],
        Color32::RED
    );
    // The new layer (2x2 crop) is the composite (green over red opaque = green).
    let top = stack.layers().last().unwrap();
    assert_eq!(top.image.size, [2, 2]);
    assert!(top.image.pixels.iter().all(|&p| p == Color32::GREEN));
    // Untouched pixels outside the selection stay put.
    assert_eq!(
        stack.layer(clean_id).unwrap().image.pixels[0],
        Color32::GREEN
    );
}

#[test]
fn touched_detection_skips_transparent_layers() {
    let size = [4, 4];
    let stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::RED),
        filled(size, Color32::TRANSPARENT),
    );
    let sel = inner_selection();
    let bounds = sel.bounds().unwrap();
    assert!(layer_touches_selection(&stack.layers()[0], &sel, bounds));
    assert!(!layer_touches_selection(&stack.layers()[1], &sel, bounds));
}

#[test]
fn moved_layer_samples_from_its_transformed_position() {
    // A 2x2 opaque blue layer placed so its center sits at page (1,1) covers pixels (0,0)..(1,1).
    let size = [4, 4];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let transform = layers::LayerTransform {
        center: egui::Vec2::new(1.0, 1.0),
        rotation: 0.0,
        scale: 1.0,
    };
    let id =
        stack.add_raster_layer_image("blue".into(), filled([2, 2], Color32::BLUE), transform);

    // Page pixel (0,0) maps into the layer; (3,3) does not.
    assert_eq!(
        sample_layer_world(stack.layer(id).unwrap(), 0, 0),
        Color32::BLUE
    );
    assert_eq!(
        sample_layer_world(stack.layer(id).unwrap(), 3, 3),
        Color32::TRANSPARENT
    );
}

#[test]
fn raster_below_by_band_z_picks_visually_below_not_stack_neighbor() {
    // Stack (insertion) order is r_a, r_b, r_c, but the user reordered them so the band-Z order is
    // r_b (z=0, bottom), r_c (z=1), r_a (z=2, top). The list is in STACK order with each raster's
    // BAND z — the helper must use band-Z, not list position.
    let rasters = vec![
        ("r_a".to_string(), 2u32),
        ("r_b".to_string(), 0u32),
        ("r_c".to_string(), 1u32),
    ];

    // r_a is the top band (z=2): directly below it is r_c (z=1), NOT its stack neighbor r_b.
    assert_eq!(
        raster_below_by_band_z(&rasters, "r_a").as_deref(),
        Some("r_c")
    );
    // r_c (z=1): below is r_b (z=0).
    assert_eq!(
        raster_below_by_band_z(&rasters, "r_c").as_deref(),
        Some("r_b")
    );
    // r_b is the bottom band (z=0): nothing below.
    assert_eq!(raster_below_by_band_z(&rasters, "r_b"), None);
    // Unknown uid → None.
    assert_eq!(raster_below_by_band_z(&rasters, "nope"), None);
}

#[test]
fn merge_selection_uses_band_z_after_reorder_and_protects_base_layers() {
    // Integration: a stack with two rasters whose BAND-Z order is the REVERSE of their stack
    // insertion order. `raster_below_uid` / `is_mergeable` must follow band-Z (the visually-below
    // raster). `Клин` is the target below the bottom-most raster; `Исходник` never is, and
    // neither base layer is ever the UPPER participant.
    let size = [2, 2];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    // Inserted first (lower stack index) but placed ON TOP by band-Z below.
    let first = stack.add_raster_layer();
    let second = stack.add_raster_layer();
    let first_uid = stack.layer(first).unwrap().uid.to_string();
    let second_uid = stack.layer(second).unwrap().uid.to_string();

    // Band-Z: `second` is the BOTTOM band (z=0), `first` is the TOP band (z=1) — reverse of stack.
    let ps = PsEditorTabState {
        bands: vec![
            Band::Raster {
                uid: second_uid.clone(),
                z: 0,
            },
            Band::Raster {
                uid: first_uid.clone(),
                z: 1,
            },
        ],
        stack: Some(stack),
        ..Default::default()
    };

    // `first` is visually on top (band z=1): directly below it is `second` (band z=0), NOT a base
    // layer and NOT its stack neighbour.
    assert_eq!(
        ps.raster_below_uid(first).as_deref(),
        Some(second_uid.as_str())
    );
    assert!(ps.is_mergeable(first), "top-by-band raster is mergeable");

    // `second` is the bottom-most raster by band-Z: below it is the `Клин` base layer, so it IS
    // mergeable — merging flattens it into the clean overlay.
    let stack_ref = ps.stack.as_ref().expect("stack resident");
    let clean_uid = stack_ref
        .layers()
        .iter()
        .find(|l| l.kind == LayerKind::Clean)
        .map(|l| l.uid.to_string())
        .expect("clean base layer present");
    let source_uid = stack_ref
        .layers()
        .iter()
        .find(|l| l.kind == LayerKind::Source)
        .map(|l| l.uid.to_string())
        .expect("source base layer present");
    assert_eq!(
        ps.raster_below_uid(second).as_deref(),
        Some(clean_uid.as_str()),
        "the bottom-most raster merges into Клин"
    );
    assert!(
        ps.is_mergeable(second),
        "bottom-by-band raster is mergeable into Клин"
    );

    // `Исходник` is never a merge candidate at all.
    let candidates = ps.merge_candidates_by_band_z();
    assert!(
        !candidates.iter().any(|(uid, _)| *uid == source_uid),
        "Исходник must never be a merge target"
    );
    let clean_z = candidates
        .iter()
        .find(|(uid, _)| *uid == clean_uid)
        .map(|(_, z)| *z)
        .expect("Клин is a merge candidate");
    assert!(
        candidates
            .iter()
            .filter(|(uid, _)| *uid != clean_uid)
            .all(|(_, z)| *z > clean_z),
        "Клин sits strictly below every raster on the band-Z axis"
    );

    // Neither base layer is ever the UPPER participant of a merge (they have no control strip,
    // and the query refuses them by construction).
    let base_ids: Vec<LayerId> = ps
        .stack
        .as_ref()
        .unwrap()
        .layers()
        .iter()
        .filter(|l| l.kind.is_base())
        .map(|l| l.id)
        .collect();
    assert_eq!(base_ids.len(), 2, "source + clean base layers present");
    for id in base_ids {
        assert!(!ps.is_mergeable(id), "a base layer is never merged away");
        assert_eq!(
            ps.raster_below_uid(id),
            None,
            "a base layer is never the upper participant"
        );
    }
}

#[test]
fn merge_candidates_keep_raster_order_and_reserve_the_bottom_slot_for_clean() {
    // The +1 shift that makes room for Клин at Z 0 must not disturb the rasters' relative order.
    let size = [2, 2];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let a = stack.add_raster_layer();
    let b = stack.add_raster_layer();
    let a_uid = stack.layer(a).expect("resident").uid.to_string();
    let b_uid = stack.layer(b).expect("resident").uid.to_string();
    let ps = PsEditorTabState {
        bands: vec![
            Band::Raster {
                uid: a_uid.clone(),
                z: 0,
            },
            Band::Raster {
                uid: b_uid.clone(),
                z: 1,
            },
        ],
        stack: Some(stack),
        ..Default::default()
    };
    let candidates = ps.merge_candidates_by_band_z();
    let z_of = |uid: &str| {
        candidates
            .iter()
            .find(|(u, _)| u == uid)
            .map(|(_, z)| *z)
            .expect("candidate present")
    };
    assert_eq!(z_of(&a_uid), 1, "band z 0 shifts to 1");
    assert_eq!(z_of(&b_uid), 2, "band z 1 shifts to 2");
    // `a` (the lower raster) is what `b` merges into; `a` in turn merges into Клин.
    assert_eq!(ps.raster_below_uid(b).as_deref(), Some(a_uid.as_str()));
    assert!(ps.raster_below_uid(a).is_some(), "Клин is below the bottom raster");
}

#[test]
fn raster_below_by_band_z_breaks_z_ties_toward_lower_stack_index() {
    // Two rasters share a band-Z (e.g. both fell back to the past-the-top fallback). The one
    // earlier in the list (lower stack index) is treated as below, matching `draw_composite`'s
    // stable raster tiebreak.
    let rasters = vec![
        ("low".to_string(), 5u32),  // earlier in the list
        ("high".to_string(), 5u32), // later in the list, same z
        ("top".to_string(), 9u32),
    ];
    // `top` (z=9) is above both; the nearest below is the tied pair — the earlier list entry wins.
    assert_eq!(
        raster_below_by_band_z(&rasters, "top").as_deref(),
        Some("low")
    );
    // Equal-Z peers are NOT below each other (strict `z <` only).
    assert_eq!(raster_below_by_band_z(&rasters, "low"), None);
    assert_eq!(raster_below_by_band_z(&rasters, "high"), None);
}

#[test]
fn an_empty_footprint_never_becomes_a_phantom_selection() {
    // "Select layer fully" on a layer whose footprint misses the page entirely: the mask stays
    // all-zero, and storing it as `Some` would block the brush behind an invisible marquee.
    let mut off_page = Selection::empty(4, 4);
    off_page.set_polygon(&[
        (-30.0, -30.0),
        (-10.0, -30.0),
        (-10.0, -10.0),
        (-30.0, -10.0),
    ]);
    assert!(!off_page.any(), "the fixture must select nothing");
    assert!(non_empty_selection(off_page).is_none());
}

#[test]
fn a_real_footprint_is_stored_as_is() {
    let sel = inner_selection();
    let stored = non_empty_selection(sel).expect("a non-empty mask must be kept");
    assert!(stored.contains(1, 1));
    assert!(!stored.contains(0, 0));
}

#[test]
fn dash_walker_skips_non_finite_segments() {
    // A broken `ViewTransform` can hand the walker an infinite point. It must not emit a single
    // run for that segment (an infinite length would otherwise exhaust the whole budget).
    let path = [
        Pos2::new(0.0, 0.0),
        Pos2::new(f32::INFINITY, 0.0),
        Pos2::new(10.0, 0.0),
    ];
    let mut runs = Vec::new();
    let emitted = walk_dash_runs(&path, 4.0, 10_000, |r| runs.push(r));
    assert_eq!(
        emitted, 0,
        "no run may be emitted from or into a non-finite point"
    );
    assert!(runs.is_empty());

    let nan_path = [Pos2::new(0.0, 0.0), Pos2::new(f32::NAN, f32::NAN)];
    assert_eq!(walk_dash_runs(&nan_path, 4.0, 10_000, |_| ()), 0);

    // A finite path around the bad segment still dashes normally.
    let good = [Pos2::new(0.0, 0.0), Pos2::new(12.0, 0.0)];
    assert_eq!(walk_dash_runs(&good, 4.0, 10_000, |_| ()), 3);
}

// ---------------------------------------------------------------------------------------
// `write_clean_region_to_model` — the ONE `Клин` write-back path (see its doc comment).
// ---------------------------------------------------------------------------------------

/// A 1-page clean-overlay model whose page 0 overlay is materialized at `size`.
///
/// `replace` adopts the size of the image it is given on a model that has none yet, which is
/// how these tests give the model a size that either matches or deliberately mismatches the
/// editor's page.
fn overlays_model_with_overlay(size: [usize; 2]) -> Arc<Mutex<CleanOverlaysModel>> {
    let mut model = CleanOverlaysModel::new_from_pages(&[std::path::PathBuf::from("p0.png")]);
    model.replace(0, &filled(size, Color32::TRANSPARENT));
    Arc::new(Mutex::new(model))
}

/// A tab state on page 0 with a `size` page and the given model bound, its `Клин` layer painted
/// `paint` over the whole page while `base_image` still holds transparent (the state a live
/// stroke leaves behind: `image` mutated, `base_image` pre-stroke, `pixels_dirty` set).
fn ps_with_painted_clean(
    size: [usize; 2],
    paint: Color32,
    model: Arc<Mutex<CleanOverlaysModel>>,
) -> (PsEditorTabState, LayerId) {
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let clean_id = stack
        .layers()
        .iter()
        .find(|l| l.kind == LayerKind::Clean)
        .map(|l| l.id)
        .expect("clean base layer present");
    let clean = stack.layer_mut(clean_id).expect("clean layer resident");
    clean.image = filled(size, paint);
    clean.pixels_dirty = true;
    let ps = PsEditorTabState {
        stack: Some(stack),
        overlays_model: Some(model),
        active_page_idx: Some(0),
        ..Default::default()
    };
    (ps, clean_id)
}

/// The straight-RGBA pixel the model holds for page 0 at `(x, y)`.
fn model_pixel(model: &Arc<Mutex<CleanOverlaysModel>>, x: u32, y: u32) -> [u8; 4] {
    let locked = model.lock().expect("model lock");
    let rgba = locked.overlay_rgba(0).expect("overlay materialized");
    rgba.get_pixel(x, y).0
}

/// The happy path: the requested rect (and ONLY it) reaches the model, `base_image` is
/// re-established over that rect so the next undo reads the committed pixels, and
/// `pixels_dirty` is cleared because the shared model owns Clean persistence.
#[test]
fn clean_write_back_pushes_only_the_requested_rect_and_commits_local_state() {
    let size = [4, 4];
    let model = overlays_model_with_overlay(size);
    let (mut ps, clean_id) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));

    assert!(ps.write_clean_region_to_model(0, 1, 1, 2, 2));

    // Inside the rect the model took the painted pixel; outside it is untouched.
    assert_eq!(model_pixel(&model, 1, 1), [255, 0, 0, 255]);
    assert_eq!(model_pixel(&model, 2, 2), [255, 0, 0, 255]);
    assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
    assert_eq!(model_pixel(&model, 3, 3), [0, 0, 0, 0]);

    let clean = ps
        .stack
        .as_ref()
        .and_then(|s| s.layer(clean_id))
        .expect("clean layer resident");
    assert_eq!(
        clean.base_image.pixels[5], // (x=1, y=1) on a 4-wide page
        Color32::RED,
        "base_image must mirror image inside the committed rect"
    );
    assert_eq!(
        clean.base_image.pixels[0],
        Color32::TRANSPARENT,
        "base_image outside the rect is not touched"
    );
    assert!(!clean.pixels_dirty);
}

/// The rect is clamped into the page, and a rect entirely outside it is a no-op that commits
/// nothing — neither to the model nor to the layer's bookkeeping.
#[test]
fn clean_write_back_clamps_the_rect_and_refuses_one_outside_the_page() {
    let size = [4, 4];
    let model = overlays_model_with_overlay(size);
    let (mut ps, clean_id) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));

    // Overhanging rect: clamped to (3,3)-(4,4), so exactly the corner pixel is written.
    assert!(ps.write_clean_region_to_model(0, 3, 3, 100, 100));
    assert_eq!(model_pixel(&model, 3, 3), [255, 0, 0, 255]);
    assert_eq!(model_pixel(&model, 2, 3), [0, 0, 0, 0]);

    // Wholly outside: nothing written, nothing committed.
    let (mut ps_out, clean_out) =
        ps_with_painted_clean(size, Color32::GREEN, overlays_model_with_overlay(size));
    assert!(!ps_out.write_clean_region_to_model(0, 4, 4, 2, 2));
    let clean = ps_out
        .stack
        .as_ref()
        .and_then(|s| s.layer(clean_out))
        .expect("clean layer resident");
    assert!(
        clean.pixels_dirty,
        "a refused write must not clear the dirty flag"
    );

    // The first tab's clamped write left its own layer committed.
    assert!(
        !ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean_id))
            .expect("clean layer resident")
            .pixels_dirty
    );
}

/// A write for a page the resident stack is not on is refused outright — the crop would come
/// from the wrong page's pixels.
#[test]
fn clean_write_back_refuses_a_foreign_page_index() {
    let size = [4, 4];
    let model = overlays_model_with_overlay(size);
    let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
    assert!(!ps.write_clean_region_to_model(1, 0, 0, 4, 4));
    assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
}

/// Revision adoption, half one: with no foreign edit in between, the tab adopts the revision its
/// own write produced, so `sync_view_from_canvas` does not reload the whole page after a stroke.
#[test]
fn clean_write_back_adopts_its_own_revision_when_nothing_foreign_happened() {
    let size = [4, 4];
    let model = overlays_model_with_overlay(size);
    let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
    // In sync with the model, as the last page load left us.
    let before = model.lock().expect("model lock").revision();
    ps.last_overlay_revision = before;

    assert!(ps.write_clean_region_to_model(0, 0, 0, 4, 4));

    let after = model.lock().expect("model lock").revision();
    assert!(after > before, "replace_region must bump the revision");
    assert_eq!(
        ps.last_overlay_revision, after,
        "our own bump must be adopted, or every stroke triggers a full page reload"
    );
}

/// Revision adoption, half two — the bug this guards: a background writer (the page manager's
/// clean-attach worker) bumped the revision while the tab was elsewhere. Adopting now would make
/// the tab equal to the model forever and the foreign edit would NEVER be loaded. The tab must
/// keep its stale revision so the next `sync_view_from_canvas` reloads and reconciles; its own
/// write is not lost by that, the model holds it.
#[test]
fn clean_write_back_keeps_a_stale_revision_when_a_foreign_edit_intervened() {
    let size = [4, 4];
    let model = overlays_model_with_overlay(size);
    let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
    // Last sync saw this revision...
    let synced = model.lock().expect("model lock").revision();
    ps.last_overlay_revision = synced;
    // ...then somebody else wrote to the model while the tab was not looking.
    model
        .lock()
        .expect("model lock")
        .replace(0, &filled(size, Color32::BLUE));
    let foreign = model.lock().expect("model lock").revision();
    assert!(foreign > synced);

    assert!(ps.write_clean_region_to_model(0, 0, 0, 4, 4));

    assert_eq!(
        ps.last_overlay_revision, synced,
        "a foreign bump must stay visible as a difference, so the page still reloads"
    );
    assert_ne!(
        ps.last_overlay_revision,
        model.lock().expect("model lock").revision()
    );
    // The write itself still reached the model — the reload restores it from there.
    assert_eq!(model_pixel(&model, 0, 0), [255, 0, 0, 255]);
}

/// A legacy overlay whose size disagrees with the editor's page must be REFUSED, not rescaled:
/// `replace_region` clamps to the model's size and scales the chunk into it, which would land a
/// brush stroke misaligned. `page_loader` answers the same mismatch with a transparent
/// page-sized `Клин`, so this state is reachable.
#[test]
fn clean_write_back_refuses_a_size_mismatched_overlay_instead_of_rescaling() {
    // The model remembers an 8x8 overlay; the editor page is 4x4.
    let model = overlays_model_with_overlay([8, 8]);
    let (mut ps, clean_id) =
        ps_with_painted_clean([4, 4], Color32::RED, Arc::clone(&model));
    ps.last_overlay_revision = model.lock().expect("model lock").revision();

    assert!(!ps.write_clean_region_to_model(0, 0, 0, 4, 4));

    // Nothing scaled into the model, and no local state was committed.
    assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
    assert_eq!(model_pixel(&model, 7, 7), [0, 0, 0, 0]);
    assert!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean_id))
            .expect("clean layer resident")
            .pixels_dirty,
        "a refused write leaves the layer uncommitted"
    );
}

/// Without a bound model there is nowhere to push to: the write fails and, crucially, leaves the
/// local bookkeeping uncommitted so the edit is not silently declared durable.
#[test]
fn clean_write_back_without_a_model_commits_nothing() {
    let size = [4, 4];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let clean_id = stack
        .layers()
        .iter()
        .find(|l| l.kind == LayerKind::Clean)
        .map(|l| l.id)
        .expect("clean base layer present");
    stack
        .layer_mut(clean_id)
        .expect("clean layer resident")
        .pixels_dirty = true;
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        active_page_idx: Some(0),
        ..Default::default()
    };

    assert!(!ps.write_clean_region_to_model(0, 0, 0, 4, 4));
    assert!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean_id))
            .expect("clean layer resident")
            .pixels_dirty
    );
}

// ---------------------------------------------------------------------------------------
// `page_footprint_rect` — the bound a merge-into-`Клин` write-back uses.
// ---------------------------------------------------------------------------------------

/// The contract that makes the bound safe: outside the upper layer's footprint the composite
/// reproduces the lower layer's pixels EXACTLY, so a write-back limited to that rect loses
/// nothing. Asserted against `composite_to_page` itself, not against the rect math alone.
#[test]
fn a_merge_changes_no_pixel_outside_the_upper_layers_footprint() {
    let size = [8, 8];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::from_rgb(10, 20, 30)),
    );
    let upper_id = stack.add_raster_layer();
    {
        let upper = stack.layer_mut(upper_id).expect("raster resident");
        upper.image = filled([2, 2], Color32::RED);
        upper.base_image = upper.image.clone();
        // Placed over page pixels (1,1)..=(2,2).
        upper.transform = LayerTransform {
            center: Vec2::new(2.0, 2.0),
            rotation: 0.0,
            scale: 1.0,
        };
    }
    let layers = stack.layers();
    let below = layers
        .iter()
        .find(|l| l.kind == LayerKind::Clean)
        .expect("clean base layer present");
    let upper = layers
        .iter()
        .find(|l| l.id == upper_id)
        .expect("raster resident");
    let merged = composite_to_page(&[below, upper], size);
    let (fx, fy, fw, fh) = page_footprint_rect(upper, size).expect("footprint on the page");

    for y in 0..size[1] {
        for x in 0..size[0] {
            let inside = x >= fx && x < fx + fw && y >= fy && y < fy + fh;
            if !inside {
                assert_eq!(
                    merged.pixels[y * size[0] + x],
                    below.image.pixels[y * size[0] + x],
                    "pixel ({x},{y}) lies outside the footprint and must be unchanged"
                );
            }
        }
    }
    // And the bound is actually a bound, not the whole page.
    assert!(fw < size[0] && fh < size[1], "the footprint must be tight");
}

/// A layer dragged entirely off the page contributes nothing, so there is no rect to write.
#[test]
fn a_footprint_entirely_off_the_page_is_none() {
    let size = [8, 8];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let id = stack.add_raster_layer();
    let layer = stack.layer_mut(id).expect("raster resident");
    layer.image = filled([2, 2], Color32::RED);
    layer.transform = LayerTransform {
        center: Vec2::new(-50.0, -50.0),
        rotation: 0.0,
        scale: 1.0,
    };
    assert_eq!(page_footprint_rect(layer, size), None);
}

/// A degenerate transform must widen the bound to the whole page, never guess a smaller one:
/// losing merged pixels is worse than one expensive write.
#[test]
fn a_non_finite_footprint_falls_back_to_the_whole_page() {
    let size = [8, 8];
    let mut stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let id = stack.add_raster_layer();
    let layer = stack.layer_mut(id).expect("raster resident");
    layer.image = filled([2, 2], Color32::RED);
    layer.transform = LayerTransform {
        center: Vec2::new(f32::NAN, 0.0),
        rotation: 0.0,
        scale: 1.0,
    };
    assert_eq!(page_footprint_rect(layer, size), Some((0, 0, 8, 8)));
}

// ---------------------------------------------------------------------------------------
// The panel's ALWAYS-PRESENT active layer (`panel_primary`).
// ---------------------------------------------------------------------------------------

/// A stack with only the two base layers, plus the `Клин` layer's id.
fn base_stack_with_clean() -> (LayerStack, LayerId) {
    let size = [2, 2];
    let stack = LayerStack::new(
        0,
        size,
        filled(size, Color32::TRANSPARENT),
        filled(size, Color32::TRANSPARENT),
    );
    let clean = stack
        .layers()
        .iter()
        .find(|l| l.kind == LayerKind::Clean)
        .map(|l| l.id)
        .expect("the clean base layer is always present");
    (stack, clean)
}

/// (a) A base row must be representable as the primary row AND must read as selected from that
/// alone: a freshly loaded page seeds `panel_primary` while deliberately leaving
/// `panel_selection` empty, so a selection-only highlight rule would show nothing at all — which
/// is the reported bug.
#[test]
fn a_base_row_is_keyed_and_is_highlighted_by_being_the_primary_row() {
    let (stack, clean) = base_stack_with_clean();
    let ps = PsEditorTabState {
        stack: Some(stack),
        panel_primary: Some(RowSel::Base(clean)),
        ..Default::default()
    };

    let rows = ps.build_panel_rows();
    let base_sels: Vec<Option<RowSel>> = rows
        .iter()
        .filter_map(|r| match r {
            PanelRow::Leaf(l) if l.is_base => Some(l.sel.clone()),
            PanelRow::Leaf(_) | PanelRow::Group(_) => None,
        })
        .collect();
    assert!(
        base_sels.iter().all(Option::is_some),
        "a base leaf must carry a RowSel so it can become the primary row: {base_sels:?}"
    );
    assert!(
        base_sels.contains(&Some(RowSel::Base(clean))),
        "the Клин row must be keyed as RowSel::Base"
    );

    let empty = HashSet::new();
    assert!(
        row_is_selected(Some(&RowSel::Base(clean)), &empty, ps.panel_primary.as_ref()),
        "the primary row is highlighted even with an empty multi-selection"
    );
}

/// (b) The user-visible default: a page opens with `Клин` as the active row, not with the
/// «Выберите слой» hint. `poll_loader` clears `panel_primary` and then calls exactly this.
#[test]
fn a_loaded_page_makes_the_clean_base_layer_the_primary_row() {
    let (stack, clean) = base_stack_with_clean();
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        // Exactly the state `poll_loader` leaves behind before it normalizes.
        panel_primary: None,
        ..Default::default()
    };

    ps.normalize_panel_primary();

    assert_eq!(ps.panel_primary, Some(RowSel::Base(clean)));
    assert_eq!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean))
            .map(|l| l.kind),
        Some(LayerKind::Clean),
        "the seeded primary must be the Клин base layer, not Исходник"
    );
}

/// No page loaded ⇒ nothing to point at. The invariant is scoped to a resident page, and
/// asserting `Some` unconditionally would be wrong before the first load finishes.
#[test]
fn the_primary_row_stays_none_while_no_page_is_loaded() {
    let mut ps = PsEditorTabState::default();
    ps.normalize_panel_primary();
    assert_eq!(ps.panel_primary, None);
}

/// (c) Deleting the primary raster must re-point the primary at the stack's own fallback. Before
/// this, `panel_primary` kept naming the dead layer and the controls strip rendered NOTHING —
/// not even its hint — because `draw_active_controls` bails on an unresolvable id.
#[test]
fn deleting_the_primary_raster_repoints_the_primary_at_the_stack_fallback() {
    let (mut stack, clean) = base_stack_with_clean();
    let raster = stack.add_raster_layer();
    stack.set_active(raster);
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        panel_primary: Some(RowSel::Raster(raster)),
        panel_selection: [RowSel::Raster(raster)].into_iter().collect(),
        panel_anchor: Some(RowSel::Raster(raster)),
        ..Default::default()
    };

    // The no-doc removal path (`apply_panel_actions`' fallback), which never reaches a projection.
    assert!(
        ps.stack
            .as_mut()
            .expect("page resident")
            .remove_layer(raster)
    );
    ps.normalize_panel_primary();

    assert_eq!(
        ps.panel_primary,
        Some(RowSel::Base(clean)),
        "the primary must fall back to Клин, never dangle on the deleted raster"
    );
    assert!(
        ps.panel_selection.is_empty(),
        "a row that no longer exists must leave the multi-selection too"
    );
    assert_eq!(ps.panel_anchor, None);
}

/// A still-valid primary is left exactly where it is: normalization repairs, it does not reset
/// the user's choice on every projection.
#[test]
fn a_valid_primary_row_survives_normalization() {
    let (mut stack, _clean) = base_stack_with_clean();
    let raster = stack.add_raster_layer();
    // The active layer is deliberately NOT the primary row, so a blind "follow active" would fail.
    stack.set_active(raster);
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        panel_primary: Some(RowSel::Raster(raster)),
        ..Default::default()
    };
    ps.normalize_panel_primary();
    assert_eq!(ps.panel_primary, Some(RowSel::Raster(raster)));
}

/// A group the stack still holds but whose last member is gone renders NO row
/// (`tree::build_unified_tree` brackets a header around a run of member leaves), so a primary
/// left on it is dangling even though `group_by_uid` still resolves.
#[test]
fn a_group_emptied_of_its_last_member_is_treated_as_dangling() {
    let (mut stack, clean) = base_stack_with_clean();
    let raster = stack.add_raster_layer();
    let group = stack.add_group("g".to_string());
    stack.set_layer_group(raster, Some(group));
    let uid = stack
        .group(group)
        .expect("group just added")
        .uid
        .to_string();
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        panel_primary: Some(RowSel::Group(uid.clone())),
        ..Default::default()
    };

    // While the group still owns the raster it is a real row and must be left alone.
    ps.normalize_panel_primary();
    assert_eq!(ps.panel_primary, Some(RowSel::Group(uid.clone())));

    // Deleting the last member leaves the group in the stack but removes its row.
    assert!(
        ps.stack
            .as_mut()
            .expect("page resident")
            .remove_layer(raster)
    );
    ps.normalize_panel_primary();
    assert_eq!(
        ps.panel_primary,
        Some(RowSel::Base(clean)),
        "an emptied group has no row, so the primary must fall back to the active layer"
    );
}

/// A `RowSel` whose id resolves to a layer of the WRONG kind is dangling too: it would drive
/// `draw_active_controls` into the raster arm — delete / merge / bake — for a base layer.
#[test]
fn a_base_id_wearing_the_raster_variant_is_treated_as_dangling() {
    let (stack, clean) = base_stack_with_clean();
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        panel_primary: Some(RowSel::Raster(clean)),
        ..Default::default()
    };
    ps.normalize_panel_primary();
    assert_eq!(ps.panel_primary, Some(RowSel::Base(clean)));
}

/// (d) The structural lock, re-expressed after base rows became selectable: whatever the
/// modifiers, a base row is a SOLO primary and never enters `panel_selection` — which is the
/// only thing `apply_group_op` reads, so a base layer can never be grouped, ungrouped or
/// deleted with a group.
#[test]
fn a_base_row_can_never_join_a_multi_selection() {
    let (mut stack, clean) = base_stack_with_clean();
    let raster = stack.add_raster_layer();
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        ..Default::default()
    };
    let order = vec![RowSel::Raster(raster)];

    for mods in [
        egui::Modifiers::default(),
        egui::Modifiers::COMMAND,
        egui::Modifiers::SHIFT,
    ] {
        // Seed a real multi-selection first, so "cleared" is an observable effect.
        ps.select_row(RowSel::Raster(raster), egui::Modifiers::default(), &order);
        assert!(!ps.panel_selection.is_empty());

        ps.select_row(RowSel::Base(clean), mods, &order);
        assert_eq!(ps.panel_primary, Some(RowSel::Base(clean)));
        assert!(
            ps.panel_selection.is_empty(),
            "a base row must not join the multi-selection (mods {mods:?})"
        );
        assert_eq!(ps.panel_anchor, None);
    }
}

/// The other half of that lock: base rows are absent from the Shift-range order, so a range
/// dragged over the bottom of the panel cannot sweep `Исходник`/`Клин` into the selection.
#[test]
fn the_shift_range_order_excludes_base_rows() {
    let (mut stack, _clean) = base_stack_with_clean();
    let raster = stack.add_raster_layer();
    let ps = PsEditorTabState {
        stack: Some(stack),
        ..Default::default()
    };
    let rows = ps.build_panel_rows();
    let order = selectable_row_order(&rows);
    assert_eq!(
        order,
        vec![RowSel::Raster(raster)],
        "only the user raster is range-selectable; the two base rows are locked out"
    );
}

/// A collapsed group emits no member rows, so the header borrows the primary highlight —
/// otherwise the active layer would be invisible whenever its group is folded.
#[test]
fn a_collapsed_group_header_carries_the_hidden_primary_highlight() {
    let empty = HashSet::new();
    let primary = RowSel::Raster(7);
    assert!(
        group_row_is_selected("g1", &empty, Some(&primary), true, Some("g1")),
        "a collapsed group holding the primary row is highlighted"
    );
    assert!(
        !group_row_is_selected("g1", &empty, Some(&primary), false, Some("g1")),
        "an EXPANDED group must not steal its member's highlight — the member row shows it"
    );
    assert!(
        !group_row_is_selected("g2", &empty, Some(&primary), true, Some("g1")),
        "an unrelated collapsed group is not highlighted"
    );
    assert!(
        group_row_is_selected("g1", &empty, Some(&RowSel::Group("g1".to_owned())), false, None),
        "a group that IS the primary row is highlighted regardless of collapse"
    );
}

// ---------------------------------------------------------------------------------------
// Region-tool host hooks: the deferred action queue, the write-region apply path, and the
// seventh dock tab's visibility.
// ---------------------------------------------------------------------------------------

/// A project with no pages and no real paths. Nothing this file's tool-action tests reach
/// touches the disk: with no `LayerDoc` bound, `route_to_doc` returns before it ever looks at
/// `project.paths`.
fn empty_project() -> ProjectData {
    use ms_project::{CanvasSettings, ProjectPaths};
    let empty = std::path::PathBuf::new;
    ProjectData {
        project_dir: empty(),
        image_dir: empty(),
        pages: Vec::new(),
        bubbles: Arc::new(Vec::new()),
        paths: ProjectPaths {
            project_dir: empty(),
            title_dir: empty(),
            notes_file: empty(),
            char_favorites_file: empty(),
            color_presets_file: empty(),
            bubbles_file: empty(),
            src_dir: empty(),
            clean_layers_dir: empty(),
            cleaned_dir: empty(),
            alt_vers_dir: empty(),
            saved_dir: empty(),
            image_bubbles_dir: empty(),
            text_images_dir: empty(),
            layers_dir: empty(),
            text_detection_dir: empty(),
            characters_dir: empty(),
            terms_file: empty(),
            settings_file: empty(),
            unsaved_dir: empty(),
            unsaved_bubbles_file: empty(),
            unsaved_clean_layers_dir: empty(),
            unsaved_image_bubbles_dir: empty(),
            unsaved_text_images_dir: empty(),
            unsaved_layers_dir: empty(),
        },
        comic_type: None,
        canvas_settings: CanvasSettings::default(),
        settings_data: serde_json::Value::Null,
        session: Default::default(),
    }
}

/// A tool that queues exactly the writes it is handed, so the TAB half of the out-channel can
/// be exercised without inventing a real region tool.
#[derive(Default)]
struct QueueingTool {
    queued: Vec<tools::PsToolAction>,
    wants_panel: bool,
}

impl PsTool for QueueingTool {
    fn id(&self) -> PsToolId {
        PsToolId::Brush
    }
    fn title(&self) -> &'static str {
        "queueing"
    }
    fn interact(&mut self, _ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        ToolOutcome::default()
    }
    fn draw_overlay(
        &self,
        _painter: &egui::Painter,
        _view: &viewport::ViewTransform,
        _pointer_image: Option<Pos2>,
    ) {
    }
    fn has_options(&self) -> bool {
        false
    }
    fn hotkey_rows(&self) -> Vec<tools::PsHotkeyRow> {
        Vec::new()
    }
    fn gesture_in_flight(&self) -> bool {
        false
    }
    fn wants_main_panel(&self) -> bool {
        self.wants_panel
    }
    fn take_actions(&mut self) -> Vec<tools::PsToolAction> {
        std::mem::take(&mut self.queued)
    }
}

/// A solid `w`x`h` premultiplied source rect.
fn solid(w: usize, h: usize, color: Color32) -> ColorImage {
    ColorImage::new([w, h], vec![color; w * h])
}

/// A write request for page 0 with the given label.
fn write_action(origin: [usize; 2], pixels: ColorImage, label: &str) -> tools::PsToolAction {
    tools::PsToolAction::WriteRegion(tools::ToolRegionWrite {
        page_idx: 0,
        origin,
        pixels,
        coverage: None,
        label: label.to_string(),
    })
}

/// A tab state whose only tool queues `actions`, on a two-base-layer page 0.
fn tab_with_queued(actions: Vec<tools::PsToolAction>) -> (PsEditorTabState, LayerId) {
    let (stack, clean) = base_stack_with_clean();
    let tool = QueueingTool {
        queued: actions,
        wants_panel: false,
    };
    let ps = PsEditorTabState {
        stack: Some(stack),
        active_page_idx: Some(0),
        tools: vec![Box::new(tool)],
        active_tool_idx: 0,
        ..Default::default()
    };
    (ps, clean)
}

/// A `Клин`-targeted write must produce EXACTLY ONE undo entry, and it must be the
/// `CleanPixels` variant — the base layer is not a doc node, so a `RasterPixels` entry would
/// try to resolve a uid that is regenerated on every page load.
#[test]
fn a_queued_write_onto_klin_records_one_clean_undo_entry() {
    let project = empty_project();
    let (mut ps, clean) = tab_with_queued(vec![write_action(
        [0, 0],
        solid(2, 2, Color32::RED),
        "region write",
    )]);
    // `Клин` is the active layer of a freshly built stack.
    assert_eq!(
        ps.stack.as_ref().and_then(|s| s.layer(s.active_id())).map(|l| l.kind),
        Some(LayerKind::Clean)
    );

    assert!(ps.apply_tool_actions(&project), "the write changed pixels");

    assert_eq!(ps.history.undo_len(), 1, "exactly one undo step per queued write");
    assert_eq!(ps.history.peek_undo_label(), Some("region write"));
    assert_eq!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean))
            .map(|l| l.image.pixels[0]),
        Some(Color32::RED),
        "the source pixels must actually land on the layer"
    );
    // The variant, asserted through the pure decision the recording site uses.
    assert!(matches!(
        pixel_region_edit_op(
            LayerKind::Clean,
            0,
            "uid".to_string(),
            Arc::new(
                RasterDiff::from_region_pixels(
                    &[0u8; 4],
                    &[255u8; 4],
                    [0, 0],
                    [1, 1],
                    [1, 1],
                    PS_UNDO_TILE_SIDE,
                )
                .expect("a 1x1 diff builds")
            ),
            "l".to_string(),
        ),
        Some(PsEditOp::CleanPixels { .. })
    ));
}

/// The `Клин` half of the route, asserted against the REAL shared model rather than inferred:
/// a write onto the base layer must reach `CleanOverlaysModel`, because that layer is not
/// persisted by the layer document at all and the pixels would otherwise die at the next page
/// switch.
#[test]
fn a_queued_write_onto_klin_reaches_the_clean_overlay_model() {
    let project = empty_project();
    let model = Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[
        std::path::PathBuf::from("0.png"),
    ])));
    assert!(
        model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ensure_overlay(0, [2, 2]),
        "the overlay is materialized at the page size"
    );
    let (mut ps, _clean) = tab_with_queued(vec![write_action(
        [0, 0],
        solid(2, 2, Color32::RED),
        "region write",
    )]);
    ps.set_overlays_model(Arc::clone(&model));

    assert!(ps.apply_tool_actions(&project));

    let locked = model
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rgba = locked.overlay_rgba(0).expect("the overlay is materialized");
    assert_eq!(
        rgba.get_pixel(0, 0).0,
        [255, 0, 0, 255],
        "the Клин write must be pushed into the shared model, straight-alpha"
    );
}

/// A RASTER-targeted write takes the other arm: it records `RasterPixels` (keyed on the
/// layer's doc uid) and must NOT touch the clean-overlay model, whose revision therefore stays
/// where it was.
#[test]
fn a_queued_write_onto_a_raster_records_raster_pixels_and_spares_the_clean_model() {
    let project = empty_project();
    let model = Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[
        std::path::PathBuf::from("0.png"),
    ])));
    assert!(
        model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ensure_overlay(0, [2, 2]),
        "the overlay is materialized at the page size"
    );
    let revision_before = model
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .revision();

    let (mut ps, _clean) = tab_with_queued(vec![write_action(
        [0, 0],
        solid(2, 2, Color32::RED),
        "region write",
    )]);
    ps.set_overlays_model(Arc::clone(&model));
    let raster = {
        let stack = ps.stack.as_mut().expect("page resident");
        let raster = stack.add_raster_layer();
        let layer = stack.layer_mut(raster).expect("raster resident");
        layer.image = solid(2, 2, Color32::TRANSPARENT);
        layer.base_image = solid(2, 2, Color32::TRANSPARENT);
        stack.set_active(raster);
        raster
    };

    assert!(ps.apply_tool_actions(&project));

    assert_eq!(ps.history.undo_len(), 1);
    assert_eq!(ps.history.peek_undo_label(), Some("region write"));
    assert_eq!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(raster))
            .map(|l| l.image.pixels[0]),
        Some(Color32::RED)
    );
    assert_eq!(
        model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision(),
        revision_before,
        "a raster write must never reach the clean-overlay model"
    );
    assert!(matches!(
        pixel_region_edit_op(
            LayerKind::Raster,
            0,
            "uid".to_string(),
            Arc::new(
                RasterDiff::from_region_pixels(
                    &[0u8; 4],
                    &[255u8; 4],
                    [0, 0],
                    [1, 1],
                    [1, 1],
                    PS_UNDO_TILE_SIDE,
                )
                .expect("a 1x1 diff builds")
            ),
            "l".to_string(),
        ),
        Some(PsEditOp::RasterPixels { .. })
    ));
}

/// `Исходник` is immutable, so no undo variant exists for it. The `match` in
/// `pixel_region_edit_op` is exhaustive on purpose: a new paintable kind must be given a route
/// here rather than falling into a catch-all.
#[test]
fn the_source_layer_has_no_pixel_undo_variant() {
    let diff = Arc::new(
        RasterDiff::from_region_pixels(
            &[0u8; 4],
            &[255u8; 4],
            [0, 0],
            [1, 1],
            [1, 1],
            PS_UNDO_TILE_SIDE,
        )
        .expect("a 1x1 diff builds"),
    );
    assert!(
        pixel_region_edit_op(LayerKind::Source, 0, "uid".to_string(), diff, "l".to_string())
            .is_none()
    );
}

/// A request naming a page that is no longer resident is DROPPED, not applied to whatever page
/// happens to be open: a worker result can easily outlive a page switch, and landing it would
/// paint one page's patch onto another.
#[test]
fn a_write_for_another_page_is_dropped() {
    let project = empty_project();
    let (mut ps, clean) = tab_with_queued(vec![tools::PsToolAction::WriteRegion(
        tools::ToolRegionWrite {
            page_idx: 4,
            origin: [0, 0],
            pixels: solid(2, 2, Color32::RED),
            coverage: None,
            label: "stale".to_string(),
        },
    )]);

    assert!(!ps.apply_tool_actions(&project));

    assert_eq!(ps.history.undo_len(), 0);
    assert_eq!(
        ps.stack
            .as_ref()
            .and_then(|s| s.layer(clean))
            .map(|l| l.image.pixels[0]),
        Some(Color32::TRANSPARENT),
        "a stale request must leave the resident page untouched"
    );
}

/// The queue is DRAINED once per frame: a second pass must find nothing, or a commit would be
/// recorded twice and undo would need two steps to take back one edit.
#[test]
fn the_action_queue_is_drained_by_one_pass() {
    let project = empty_project();
    let (mut ps, _clean) = tab_with_queued(vec![write_action(
        [0, 0],
        solid(2, 2, Color32::RED),
        "region write",
    )]);
    assert!(ps.apply_tool_actions(&project));
    assert!(!ps.apply_tool_actions(&project), "the queue is empty now");
    assert_eq!(ps.history.undo_len(), 1);
}

/// Full coverage is a plain replace, and the returned rect is the written region in the
/// destination's own pixels.
#[test]
fn a_full_coverage_write_replaces_the_destination() {
    let mut dst = solid(4, 4, Color32::TRANSPARENT);
    let src = solid(2, 2, Color32::RED);
    let dirty = blend_premul_region(&mut dst, [1, 1], &src, None).expect("the rect is inside");
    assert_eq!((dirty.min_x, dirty.min_y, dirty.max_x, dirty.max_y), (1, 1, 2, 2));
    assert_eq!(dst.pixels[5], Color32::RED, "(1,1) of a 4-wide destination");
    assert_eq!(dst.pixels[0], Color32::TRANSPARENT, "outside the rect is untouched");
}

/// Coverage 255 must be bit-identical to no coverage at all — that is what lets a feathered
/// patch's opaque interior stay exact — and coverage 0 must leave the destination alone.
#[test]
fn coverage_extremes_match_replace_and_no_op() {
    let src = solid(2, 2, Color32::from_rgba_premultiplied(10, 20, 30, 200));
    let mut opaque = solid(2, 2, Color32::TRANSPARENT);
    blend_premul_region(&mut opaque, [0, 0], &src, Some(&[255u8; 4][..])).expect("inside");
    assert_eq!(opaque.pixels, src.pixels);

    let mut untouched = solid(2, 2, Color32::BLUE);
    blend_premul_region(&mut untouched, [0, 0], &src, Some(&[0u8; 4][..])).expect("inside");
    assert_eq!(untouched.pixels, vec![Color32::BLUE; 4]);
}

/// A mid-range coverage interpolates every premultiplied channel toward the source.
#[test]
fn a_partial_coverage_lerps_toward_the_source() {
    let mut dst = solid(1, 1, Color32::from_rgba_premultiplied(0, 0, 0, 0));
    let src = solid(1, 1, Color32::from_rgba_premultiplied(200, 100, 50, 255));
    blend_premul_region(&mut dst, [0, 0], &src, Some(&[128u8][..])).expect("inside");
    let px = dst.pixels[0];
    // round(200 * 128 / 255) = 100, round(100 * 128/255) = 50, round(50*128/255) = 25.
    assert_eq!((px.r(), px.g(), px.b(), px.a()), (100, 50, 25, 128));
}

/// A rect hanging off the destination is CLIPPED, never refused and never a panic: a region
/// tool works in page pixels and its ROI legitimately reaches the page edge.
#[test]
fn a_write_overhanging_the_layer_is_clipped() {
    let mut dst = solid(2, 2, Color32::TRANSPARENT);
    let src = solid(4, 4, Color32::RED);
    let dirty = blend_premul_region(&mut dst, [1, 1], &src, None).expect("part of it is inside");
    assert_eq!((dirty.min_x, dirty.min_y, dirty.max_x, dirty.max_y), (1, 1, 1, 1));
    assert_eq!(dst.pixels[3], Color32::RED);
    assert_eq!(dst.pixels[0], Color32::TRANSPARENT);

    // Entirely outside is a no-op, reported as such.
    assert!(blend_premul_region(&mut dst, [9, 9], &src, None).is_none());
}

/// A coverage buffer whose length does not match the source rect is REFUSED, not padded:
/// silently filling the difference would land a half-written patch (`CLAUDE.md` §11).
#[test]
fn a_wrongly_sized_coverage_buffer_is_refused() {
    let mut dst = solid(4, 4, Color32::TRANSPARENT);
    let src = solid(2, 2, Color32::RED);
    assert!(blend_premul_region(&mut dst, [0, 0], &src, Some(&[255u8; 3][..])).is_none());
    assert!(blend_premul_region(&mut dst, [0, 0], &src, Some(&[255u8; 5][..])).is_none());
    assert_eq!(dst.pixels, vec![Color32::TRANSPARENT; 16], "nothing was written");
}

/// The seventh dock tab follows the ACTIVE tool, not a stored user flag: switching to a tool
/// that owns a panel shows it, switching away hides it again.
#[test]
fn the_tool_panel_tab_follows_the_active_tool() {
    let tools: Vec<Box<dyn PsTool>> = vec![
        Box::new(QueueingTool {
            queued: Vec::new(),
            wants_panel: false,
        }),
        Box::new(QueueingTool {
            queued: Vec::new(),
            wants_panel: true,
        }),
    ];
    assert!(!tool_panel_visible(&tools, 0));
    assert!(tool_panel_visible(&tools, 1));
    // An out-of-range index answers `false` rather than panicking: the dock declaration runs
    // every frame, including before the tool list is ever indexed.
    assert!(!tool_panel_visible(&tools, 9));
    assert!(!tool_panel_visible(&[], 0));
}

/// None of the SHIPPED tools asks for the panel, so the default arrangement is unchanged by
/// the tab's existence — the seventh panel is declared but never drawn today.
#[test]
fn no_shipped_tool_opens_the_tool_panel() {
    let ps = PsEditorTabState::default();
    for idx in 0..ps.tools.len() {
        assert!(
            !tool_panel_visible(&ps.tools, idx),
            "tool {idx} unexpectedly asks for a dock panel"
        );
    }
}


// ---------------------------------------------------------------------------------------
// Structural edits (band order / pin) through the shared doc: doc-owned text pin meta.
// ---------------------------------------------------------------------------------------

use ms_models::layer_model::layer_doc::{
    DecodedPagePayload, LayerDoc, LayerNode, NodeBody, NodeKind,
};

// Node uids must be real UUIDs: the stack projection parses them (`sync_view_from_doc`).
const R1: &str = "00000000-0000-4000-8000-000000000001";
const R2: &str = "00000000-0000-4000-8000-000000000002";
const T: &str = "00000000-0000-4000-8000-0000000000aa";

/// A unique, fresh scratch directory under the system temp dir (the crate has no `tempfile`
/// dev-dependency). Removed by [`ScratchDir`]'s drop.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ms_ps_editor_{tag}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort cleanup of a test scratch dir; a leftover temp dir fails nothing.
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            eprintln!("scratch cleanup {}: {err}", self.0.display());
        }
    }
}

/// `empty_project` with real staging (`unsaved_layers_dir`) and committed (`layers_dir`) dirs.
fn project_in(dir: &ScratchDir) -> ProjectData {
    let mut project = empty_project();
    project.paths.unsaved_layers_dir = dir.0.join("unsaved_layers");
    project.paths.layers_dir = dir.0.join("layers");
    project
}

/// A 2x2 doc node of `kind` at unified `z`; a text node carries the given doc-owned pin meta.
fn doc_node(uid: &str, kind: NodeKind, z: u32, pinned: bool) -> LayerNode {
    let image = filled([2, 2], Color32::RED);
    let body = match kind {
        NodeKind::Raster => NodeBody::Raster {
            base_image: image.clone(),
            display_image: image,
            effects: Vec::new(),
            base_file: format!("{uid}.png"),
            mask_clip: None,
        },
        NodeKind::Text => NodeBody::Text {
            render_data: serde_json::json!({"text_params": {"text": uid}}),
            image,
            is_image: false,
            payload_uid: uid.to_string(),
            mask_clip: None,
            extra_centers: Default::default(),
            centering_frame: None,
        },
    };
    LayerNode {
        uid: uid.to_string(),
        name: uid.to_string(),
        kind,
        z,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: (kind == NodeKind::Text).then_some(0),
        text_pinned: kind == NodeKind::Text && pinned,
        text_pinned_by_group: false,
        transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
        deform: None,
        generation: 0,
        pixels_dirty: kind == NodeKind::Raster,
        body,
    }
}

/// A PS tab on page 0 wired to a doc holding raster `R1` (z0), raster `R2` (z1) and pinned text
/// `T` (z2), projected exactly like a page load (`materialize_text_runtime_from_doc` then
/// `sync_view_from_doc`). The page is flushed synchronously to staging first, so the manifest the
/// structural edits address exists. `with_saver` enables the background saver AFTER that flush.
fn ps_on_doc_page(project: &ProjectData, with_saver: bool) -> (PsEditorTabState, Arc<Mutex<LayerDoc>>) {
    let mut doc = LayerDoc::new();
    doc.insert_decoded_page(
        0,
        DecodedPagePayload {
            nodes: vec![
                doc_node(R1, NodeKind::Raster, 0, false),
                doc_node(R2, NodeKind::Raster, 1, false),
                doc_node(T, NodeKind::Text, 2, true),
            ],
            groups: Vec::new(),
        },
    );
    doc.flush_page(0, &project.paths.unsaved_layers_dir, None).expect("seed staging manifest");
    if with_saver {
        doc.enable_background_saver(None);
    }
    let doc = Arc::new(Mutex::new(doc));
    let (stack, _clean) = base_stack_with_clean();
    let mut ps = PsEditorTabState {
        stack: Some(stack),
        active_page_idx: Some(0),
        ..Default::default()
    };
    ps.set_layer_doc(Arc::clone(&doc));
    ps.materialize_text_runtime_from_doc(0);
    ps.sync_view_from_doc(0);
    (ps, doc)
}

/// Doc node `uid` of page 0.
fn find_node<'a>(doc: &'a LayerDoc, uid: &str) -> Option<&'a LayerNode> {
    doc.page(0).and_then(|page| page.nodes.iter().find(|n| n.uid == uid))
}

/// The staging manifest's pinned flag of text `uid` on page 0.
fn disk_text_pinned(project: &ProjectData, uid: &str) -> Option<bool> {
    persist::load_page_text_nodes(&project.paths.unsaved_layers_dir, None, 0)
        .expect("read staging text nodes")
        .into_iter()
        .find(|n| n.uid == uid)
        .map(|n| n.pinned)
}

/// The staging manifest's unified band order of page 0, as node uids (bottom-to-top).
fn disk_band_uids(project: &ProjectData) -> Vec<String> {
    persist::load_page_bands(&project.paths.unsaved_layers_dir, None, 0)
        .iter()
        .filter_map(|b| match b {
            Band::Raster { uid, .. } | Band::PinnedText { uid, .. } => Some(uid.clone()),
            Band::TextGroup { .. } => None,
        })
        .collect()
}

/// Text runtimes are materialized from the doc's pin meta with NO disk read, and a runtime whose
/// uid left the doc is dropped; with no doc the runtimes clear instead of leaking a prior page.
#[test]
fn text_runtimes_are_materialized_from_the_doc_pin_meta() {
    let dir = ScratchDir::new("materialize");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, false);
    assert_eq!(ps.text_layers.len(), 1);
    assert!(ps.text_layers[0].pinned, "pin meta comes from the doc node");

    // A doc-side pin edit (another path) is picked up by the next materialization.
    assert!(doc.lock().expect("doc lock").set_text_pin_meta(0, T, false, true));
    ps.materialize_text_runtime_from_doc(0);
    assert!(!ps.text_layers[0].pinned);
    assert!(ps.text_layers[0].pinned_by_group);

    // The node leaves the doc: its runtime is dropped.
    assert!(doc.lock().expect("doc lock").remove_node(0, T));
    ps.materialize_text_runtime_from_doc(0);
    assert!(ps.text_layers.is_empty());

    // No doc page at all (not resident): nothing to materialize.
    ps.text_layers.push(PsTextLayer::meta_from_node("stale".into(), "stale".into(), 0, None, true, false));
    ps.materialize_text_runtime_from_doc(7);
    assert!(ps.text_layers.is_empty(), "a non-resident page leaves no stale runtimes");
}

/// Saver-less doc: a band move takes the synchronous fallback — the manifest is written
/// immediately, the doc z / pin meta and the local runtime agree with it.
#[test]
fn a_band_move_without_a_saver_writes_synchronously() {
    let dir = ScratchDir::new("sync_band");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, false);

    // Text `T` (top) one step down: it hops below `R2`.
    ps.move_band_one(RowSel::Text(T.into()), false, &project);
    assert_eq!(disk_band_uids(&project), [R1, T, R2]);
    {
        let guard = doc.lock().expect("doc lock");
        let z = |uid: &str| find_node(&guard, uid).map(|n| n.z);
        assert!(z(R1) < z(T) && z(T) < z(R2), "doc mirrors the move");
        assert!(find_node(&guard, T).is_some_and(|n| n.text_pinned));
    }
    assert!(ps.text_layers[0].pinned);
}

/// With a saver: a band move persists the rasters and the order as saver jobs applied in FIFO
/// order (no synchronous `flush_page`), and the doc + local state update immediately.
#[test]
fn a_band_move_with_a_saver_lands_after_the_barrier() {
    let dir = ScratchDir::new("saver_band");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, true);

    ps.move_band_one(RowSel::Text(T.into()), false, &project);
    {
        let guard = doc.lock().expect("doc lock");
        let z = |uid: &str| find_node(&guard, uid).map(|n| n.z);
        assert!(z(R1) < z(T) && z(T) < z(R2), "doc mirrors the move at once");
        assert!(guard.page_has_pending_save(0));
    }
    assert!(ps.text_layers[0].pinned, "a pinned band move keeps the text pinned");

    let handle = doc.lock().expect("doc lock").saver_handle().expect("saver enabled");
    assert!(handle.barrier_blocking().is_empty(), "no failed page");
    assert_eq!(disk_band_uids(&project), [R1, T, R2]);
    assert_eq!(disk_text_pinned(&project, T), Some(true));
}
// ---------------------------------------------------------------------------------------
// Doc-level composite meta: group eye / opacity and the text eye reach the shared doc.
// ---------------------------------------------------------------------------------------

const G: &str = "00000000-0000-4000-8000-0000000000b1";

/// `ps_on_doc_page` plus a visible, opaque group `G` holding raster `R1` and text `T`, added to
/// the doc and projected (the PS view of a page whose group came from disk).
fn ps_on_doc_page_with_group(project: &ProjectData) -> (PsEditorTabState, Arc<Mutex<LayerDoc>>) {
    let (mut ps, doc) = ps_on_doc_page(project, false);
    {
        let mut guard = doc.lock().expect("doc lock");
        guard.add_group(
            0,
            persist::GroupMeta {
                uid: G.to_string(),
                name: "G".to_string(),
                visible: true,
                opacity: 1.0,
                collapsed: false,
            },
        );
        guard.set_group(0, R1, Some(G.to_string()));
        guard.set_group(0, T, Some(G.to_string()));
    }
    ps.sync_view_from_doc(0);
    (ps, doc)
}

/// The doc's `(visible, opacity)` of group `G` on page 0.
fn doc_group_meta(doc: &Arc<Mutex<LayerDoc>>) -> Option<(bool, f32)> {
    let guard = doc.lock().expect("doc lock");
    guard
        .page(0)
        .and_then(|page| page.groups.iter().find(|g| g.uid == G))
        .map(|g| (g.visible, g.opacity))
}

/// The stack's `(visible, opacity)` of group `G`.
fn stack_group_meta(ps: &PsEditorTabState) -> Option<(bool, f32)> {
    ps.group_meta_value(G).map(|v| (v.visible, v.opacity))
}

/// The group eye writes the DOC (so typing and export see it), bumps its version, survives the
/// projection that any later doc edit triggers, persists, and undoes / redoes through the doc.
#[test]
fn a_group_eye_toggle_reaches_the_doc_and_survives_a_doc_sync() {
    let dir = ScratchDir::new("group_eye");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page_with_group(&project);
    let v0 = doc.lock().expect("doc lock").version();

    ps.apply_group_op(GroupOp::ToggleGroupVisible(G.to_string()), &project);
    assert_eq!(doc_group_meta(&doc), Some((false, 1.0)), "the doc group is hidden");
    assert!(doc.lock().expect("doc lock").version() > v0, "the doc version bumped");
    assert_eq!(stack_group_meta(&ps), Some((false, 1.0)));

    // An unrelated doc edit re-projects the stack from the doc: the group stays hidden.
    ps.edit_doc_node(0, |doc| doc.set_opacity(0, R2, 0.5));
    assert_eq!(stack_group_meta(&ps), Some((false, 1.0)), "the toggle survives sync_view_from_doc");

    // Persisted through the doc page save, in the manifest's group record.
    let disk = persist::load_page_rasters(&project.paths.unsaved_layers_dir, None, 0).expect("read staging");
    assert!(disk.groups.iter().any(|g| g.uid == G && !g.visible), "the hidden group reached disk");

    assert_eq!(ps.history.undo_len(), 1, "one undo step per eye toggle");
    assert!(ps.undo(&project));
    assert_eq!(doc_group_meta(&doc), Some((true, 1.0)), "undo shows the group again, in the doc");
    assert!(ps.redo(&project));
    assert_eq!(doc_group_meta(&doc), Some((false, 1.0)), "redo hides it again");
}

/// A group opacity drag writes the doc on every value change but records ONE undo step for the
/// whole gesture, closed by the first panel frame without a change.
#[test]
fn a_group_opacity_drag_writes_the_doc_and_records_one_undo_step() {
    let dir = ScratchDir::new("group_opacity");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page_with_group(&project);

    for value in [0.8_f32, 0.6, 0.4] {
        ps.apply_panel_actions(
            PanelActions {
                group_op: Some(GroupOp::GroupOpacity(G.to_string(), value)),
                ..Default::default()
            },
            &project,
        );
        assert_eq!(doc_group_meta(&doc), Some((true, value)), "the doc follows each drag step");
    }
    assert_eq!(ps.history.undo_len(), 0, "no undo step while the drag is in flight");
    // The first idle panel frame ends the gesture.
    ps.apply_panel_actions(PanelActions::default(), &project);
    assert_eq!(ps.history.undo_len(), 1, "one undo step for the whole drag");
    ps.apply_panel_actions(PanelActions::default(), &project);
    assert_eq!(ps.history.undo_len(), 1, "idle frames record nothing more");

    assert!(ps.undo(&project));
    assert_eq!(doc_group_meta(&doc), Some((true, 1.0)), "undo restores the pre-drag opacity");
}

/// The PS text eye writes the doc text node's `visible` (typing / export omit it), survives a
/// later projection, is read back by the PS panel, and undoes through the doc.
#[test]
fn the_text_eye_writes_the_doc_node_and_survives_a_doc_sync() {
    let dir = ScratchDir::new("text_eye");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, false);
    let text_visible_in_doc = |doc: &Arc<Mutex<LayerDoc>>| {
        find_node(&doc.lock().expect("doc lock"), T).map(|n| n.visible)
    };

    ps.apply_panel_actions(
        PanelActions {
            toggle_visible_text: Some(0),
            ..Default::default()
        },
        &project,
    );
    assert_eq!(text_visible_in_doc(&doc), Some(false), "the doc text node is hidden");
    assert!(!ps.text_layers[0].visible, "the panel reads it back from the doc");

    ps.edit_doc_node(0, |doc| doc.set_opacity(0, R1, 0.5));
    assert!(!ps.text_layers[0].visible, "the eye survives sync_view_from_doc");
    let disk_visible = persist::load_page_text_nodes(&project.paths.unsaved_layers_dir, None, 0)
        .expect("read staging text nodes")
        .into_iter()
        .find(|n| n.uid == T)
        .map(|n| n.visible);
    assert_eq!(disk_visible, Some(false), "the text save persisted the hidden node");

    assert_eq!(ps.history.undo_len(), 1);
    assert!(ps.undo(&project));
    assert_eq!(text_visible_in_doc(&doc), Some(true), "undo shows the text again, in the doc");
    assert!(ps.text_layers[0].visible);
}

/// One group-opacity drag tick is ONE doc version step (the mutator's own bump; `edit_doc_node`
/// adds none on top of it).
#[test]
fn one_group_opacity_tick_bumps_the_doc_version_once() {
    let dir = ScratchDir::new("opacity_bump");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page_with_group(&project);
    let (v0, r0) = {
        let guard = doc.lock().expect("doc lock");
        (guard.version(), guard.page_revision(0))
    };
    ps.apply_group_op(GroupOp::GroupOpacity(G.to_string(), 0.5), &project);
    let guard = doc.lock().expect("doc lock");
    assert_eq!(guard.version(), v0 + 1);
    assert!(guard.page_revision(0) > r0, "the edited page's revision advanced");
    assert_eq!(guard.page_revision(0), guard.version(), "one step: the page stamp is the new version");
}

/// S-050: the per-frame sync keys on the ACTIVE page's revision. An edit of another doc page (a
/// typing edit or preload elsewhere) does not re-project the active page; an edit of the active
/// page does, and records its new revision.
#[test]
fn only_an_edit_of_the_active_page_reprojects_it() {
    let dir = ScratchDir::new("page_revision_sync");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, false);
    let projected = ps.last_projected_revision;
    assert_eq!(projected, Some((0, doc.lock().expect("doc lock").page_revision(0))), "the load projection recorded page 0");
    // A marker only a re-projection would undo: `sync_view_from_doc` rebuilds the bands.
    ps.bands.clear();

    {
        let mut guard = doc.lock().expect("doc lock");
        guard.insert_decoded_page(1, DecodedPagePayload { nodes: vec![doc_node(R1, NodeKind::Raster, 0, false)], groups: Vec::new() });
        guard.set_opacity(1, R1, 0.5);
    }
    ps.refresh_view_if_doc_version_changed();
    assert!(ps.bands.is_empty(), "an edit of page 1 did not re-project page 0");
    assert_eq!(ps.last_projected_revision, projected);

    doc.lock().expect("doc lock").set_opacity(0, R1, 0.5);
    ps.refresh_view_if_doc_version_changed();
    assert!(!ps.bands.is_empty(), "an edit of the active page re-projected it");
    assert_eq!(ps.last_projected_revision, Some((0, doc.lock().expect("doc lock").page_revision(0))));
}

/// S-050: a PS funnel edit made only through `node_mut` (no bumping mutator) still marks the edited
/// page changed, so the typing tab re-projects it, and leaves other pages' revisions alone.
#[test]
fn a_node_mut_only_funnel_edit_marks_its_page_changed() {
    let dir = ScratchDir::new("node_mut_funnel");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page(&project, false);
    let r1 = {
        let mut guard = doc.lock().expect("doc lock");
        guard.insert_decoded_page(1, DecodedPagePayload { nodes: Vec::new(), groups: Vec::new() });
        guard.page_revision(1)
    };
    let r0 = doc.lock().expect("doc lock").page_revision(0);
    assert!(ps.edit_doc_node(0, |doc| {
        if let Some(node) = doc.node_mut(0, R1) {
            node.name = "renamed".to_string();
        }
    }));
    let guard = doc.lock().expect("doc lock");
    assert!(guard.page_revision(0) > r0, "the funnel marked page 0 changed");
    assert_eq!(guard.page_revision(1), r1, "page 1 is untouched");
    drop(guard);
    assert_eq!(ps.last_projected_revision, Some((0, doc.lock().expect("doc lock").page_revision(0))), "the funnel re-projected page 0");
}

/// Review High-2: a deferred (`edit_doc_node`) edit has not reached the autosave gate yet, so it
/// must be visible as `has_deferred_layer_edits` until the flush that persists it — the
/// single-image session reads it as unsaved work. An immediately persisted edit leaves it clear.
#[test]
fn a_deferred_doc_edit_is_reported_until_flushed() {
    let dir = ScratchDir::new("deferred_edits");
    let project = project_in(&dir);
    let (mut ps, _doc) = ps_on_doc_page_with_group(&project);
    assert!(!ps.has_deferred_layer_edits(), "a freshly loaded page owes nothing");
    ps.apply_group_op(GroupOp::ToggleCollapse(G.to_string()), &project);
    assert!(!ps.has_deferred_layer_edits(), "a route_to_doc edit is persisted at once");
    ps.apply_group_op(GroupOp::GroupOpacity(G.to_string(), 0.5), &project);
    assert!(ps.has_deferred_layer_edits(), "an opacity tick defers its persistence");
    ps.flush_layers(&project);
    assert!(!ps.has_deferred_layer_edits(), "the flush persisted it");
}

/// Collapse has ONE owner, the doc: the toggle writes it there and persists at once, a later group
/// eye (whose doc page save also writes the groups) keeps it, and a fresh PS view of the same doc
/// page — leaving the page and coming back — shows it collapsed. No undo step is recorded.
#[test]
fn a_group_collapse_is_doc_owned_persisted_and_survives_a_page_return() {
    let dir = ScratchDir::new("group_collapse");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page_with_group(&project);
    let doc_collapsed = |doc: &Arc<Mutex<LayerDoc>>| {
        let guard = doc.lock().expect("doc lock");
        guard.page(0).and_then(|page| page.groups.iter().find(|g| g.uid == G)).map(|g| g.collapsed)
    };
    let disk_collapsed = |project: &ProjectData| {
        persist::load_page_rasters(&project.paths.unsaved_layers_dir, None, 0)
            .expect("read staging")
            .groups
            .iter()
            .find(|g| g.uid == G)
            .map(|g| g.collapsed)
    };

    ps.apply_group_op(GroupOp::ToggleCollapse(G.to_string()), &project);
    assert_eq!(doc_collapsed(&doc), Some(true), "the doc owns the flag");
    assert!(ps.stack.as_ref().and_then(|s| s.group_by_uid(G)).is_some_and(|g| g.collapsed));
    assert_eq!(disk_collapsed(&project), Some(true), "persisted at once");
    assert!(!ps.layers_dirty, "nothing left for a tab-switch flush to lose");
    assert_eq!(ps.history.undo_len(), 0, "collapse is view state, never an undo step");

    // The group eye's doc page save writes the doc groups, collapse included.
    ps.apply_group_op(GroupOp::ToggleGroupVisible(G.to_string()), &project);
    assert_eq!(disk_collapsed(&project), Some(true), "the eye's save keeps the collapse");

    // Leaving and returning: a fresh stack projected from the resident doc page.
    let (stack, _clean) = base_stack_with_clean();
    ps.stack = Some(stack);
    ps.materialize_text_runtime_from_doc(0);
    ps.sync_view_from_doc(0);
    assert!(ps.stack.as_ref().and_then(|s| s.group_by_uid(G)).is_some_and(|g| g.collapsed));
}

/// An undo entry whose target was deleted is skipped: the keypress undoes the next applicable edit
/// instead of silently consuming the orphan.
#[test]
fn undo_skips_a_meta_entry_whose_group_was_deleted() {
    let dir = ScratchDir::new("orphan_undo");
    let project = project_in(&dir);
    let (mut ps, doc) = ps_on_doc_page_with_group(&project);
    // Text eye first (older entry), then the group eye (newest entry).
    ps.apply_panel_actions(
        PanelActions {
            toggle_visible_text: Some(0),
            ..Default::default()
        },
        &project,
    );
    ps.apply_group_op(GroupOp::ToggleGroupVisible(G.to_string()), &project);
    assert_eq!(ps.history.undo_len(), 2);
    // The group is deleted elsewhere (structural, no undo entry).
    ps.edit_doc_node(0, |doc| doc.remove_group(0, G));
    assert!(ps.group_meta_value(G).is_none());

    assert!(ps.undo(&project), "one keypress reaches the text eye entry");
    assert_eq!(ps.history.undo_len(), 0, "the orphan was skipped, the text entry undone");
    let visible = find_node(&doc.lock().expect("doc lock"), T).map(|n| n.visible);
    assert_eq!(visible, Some(true), "the text eye was undone");
}
