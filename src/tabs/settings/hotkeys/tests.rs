/*
File: src/tabs/settings/hotkeys/tests.rs

Purpose:
Unit tests of the pure pane rules in `model.rs`: grouping and ordering, the "use as modifier"
offer and its application, templates of added alternatives, conflict / shadow row mapping and
the search match. Registries are built in memory from `ms_input::view::declarations()` plus a
small fictional tab set; nothing touches a config file.
*/

use std::collections::BTreeSet;

use ms_config::app_tab::AppTab;
use ms_input::{
    ActionId, ActionMeta, ActionSpec, AxisSource, Binding, Button, CaptureSlot, Chord, CommandSpec, Conflict, ConflictKind, ConflictReport, Declarations, FixedAction, GestureKind,
    InputRegistry, ModeEffect, ModeSpec, ModifierKey, ReleasePolicy, Scope, ScopeLevel, Shadow, view,
};

use super::model::{self, Group, RowKey};

const TAB_MODE: ActionId = ActionId("test.translation.mode");
const TAB_COMMAND: ActionId = ActionId("test.translation.command");
const SUB_MODE: ActionId = ActionId("test.translation.sub_mode");
const SUB_COMMAND: ActionId = ActionId("test.translation.sub_command");
const OTHER_TAB_MODE: ActionId = ActionId("test.cleaning.mode");
const SESSION_COMMAND: ActionId = ActionId("test.session.command");
const FIXED: ActionId = ActionId("test.session.fixed");

fn meta(id: ActionId, section_key: &'static str, scope: Scope, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key: "test.title", section_key, scope, when: Vec::new(), defaults }
}

fn mode(id: ActionId, scope: Scope, key: egui::Key) -> ActionSpec {
    ActionSpec::Mode(ModeSpec { meta: meta(id, "test.section.modes", scope, vec![Binding::mode([Button::Key(key)])]), effect: ModeEffect::Immediate, on_release_mid_gesture: ReleasePolicy::Commit, beats: Vec::new(), param: None })
}

fn command(id: ActionId, section_key: &'static str, scope: Scope, chord: Chord) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta: meta(id, section_key, scope, vec![Binding::command(chord)]), accepts: Vec::new(), repeat: false })
}

/// The view set plus a fictional Translation / Cleaning / session set.
fn registry() -> InputRegistry {
    let mut decls = view::declarations();
    decls.extend(Declarations {
        actions: vec![
            command(SESSION_COMMAND, "test.section.file", Scope::Session, Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(egui::Key::S)])),
            mode(TAB_MODE, Scope::Tab(AppTab::Translation), egui::Key::Q),
            command(SUB_COMMAND, "test.section.sub", Scope::Sub(Some(AppTab::Translation)), Chord::new([Button::Key(egui::Key::E)])),
            command(TAB_COMMAND, "test.section.tab", Scope::Tab(AppTab::Translation), Chord::new([Button::Key(egui::Key::T)])),
            mode(SUB_MODE, Scope::Sub(Some(AppTab::Translation)), egui::Key::W),
            mode(OTHER_TAB_MODE, Scope::Tab(AppTab::Cleaning), egui::Key::R),
        ],
        targets: Vec::new(),
        fixed: vec![FixedAction { meta: meta(FIXED, "test.section.file", Scope::Session, vec![Binding::command(Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(egui::Key::C)]))]) }],
        exclusive_contexts: Vec::new(),
    });
    match InputRegistry::new(decls) {
        Ok(registry) => registry,
        Err(err) => panic!("test declarations are invalid: {err}"),
    }
}

#[test]
fn group_of_nests_sub_contexts_under_their_tab() {
    assert_eq!(model::group_of(Scope::Session), Group::Session);
    assert_eq!(model::group_of(Scope::View), Group::View);
    assert_eq!(model::group_of(Scope::Canvas), Group::Canvas);
    assert_eq!(model::group_of(Scope::Tab(AppTab::Typing)), Group::Tab(AppTab::Typing));
    assert_eq!(model::group_of(Scope::Sub(Some(AppTab::Cleaning))), Group::Tab(AppTab::Cleaning));
    assert_eq!(model::group_of(Scope::Window(Some(AppTab::Translation))), Group::Tab(AppTab::Translation));
    assert_eq!(model::group_of(Scope::Sub(None)), Group::SharedContexts);
    assert_eq!(model::group_of(Scope::Window(None)), Group::SharedContexts);
}

#[test]
fn layout_orders_groups_levels_sections_and_declarations() {
    let row = |id: &'static str, scope: Scope, section_key: &'static str, declaration_index: usize| RowKey { id: ActionId(id), scope, section_key, declaration_index };
    let rows = vec![
        row("cleaning.a", Scope::Tab(AppTab::Cleaning), "s.clean", 0),
        row("translation.sub", Scope::Sub(Some(AppTab::Translation)), "s.sub", 1),
        row("translation.b", Scope::Tab(AppTab::Translation), "s.second", 2),
        row("translation.a", Scope::Tab(AppTab::Translation), "s.first", 3),
        row("translation.c", Scope::Tab(AppTab::Translation), "s.second", 4),
        row("shared", Scope::Sub(None), "s.shared", 5),
        row("view", Scope::View, "s.view", 6),
        row("session", Scope::Session, "s.session", 7),
        row("canvas", Scope::Canvas, "s.canvas", 8),
    ];
    let blocks = model::layout_rows(&rows);
    let groups: Vec<Group> = blocks.iter().map(|block| block.group).collect();
    assert_eq!(groups, vec![Group::Session, Group::View, Group::Canvas, Group::SharedContexts, Group::Tab(AppTab::Translation), Group::Tab(AppTab::Cleaning)]);
    let Some(translation) = blocks.iter().find(|block| block.group == Group::Tab(AppTab::Translation)) else { panic!("no translation block") };
    let sections: Vec<(&str, ScopeLevel, Vec<&str>)> = translation.sections.iter().map(|section| (section.section_key, section.level, section.actions.iter().map(|id| id.0).collect())).collect();
    // Tab level first (section order = first declaration: "s.second" at 2 before "s.first" at 3),
    // then the sub-context level; declaration order inside a section.
    assert_eq!(
        sections,
        vec![
            ("s.second", ScopeLevel::Tab, vec!["translation.b", "translation.c"]),
            ("s.first", ScopeLevel::Tab, vec!["translation.a"]),
            ("s.sub", ScopeLevel::Sub, vec!["translation.sub"]),
        ]
    );
}

#[test]
fn row_keys_follow_registry_declaration_order_with_fixed_last() {
    let registry = registry();
    let keys = model::row_keys(&registry);
    let ids: Vec<&str> = keys.iter().map(|key| key.id.0).collect();
    let position = |id: ActionId| ids.iter().position(|candidate| *candidate == id.0);
    assert!(position(SESSION_COMMAND) < position(TAB_MODE));
    assert_eq!(ids.last().copied(), Some(FIXED.0));
    assert!(keys.iter().enumerate().all(|(index, key)| key.declaration_index == index));
}

#[test]
fn offered_modes_are_same_scope_or_same_tab_modes() {
    let registry = registry();
    assert_eq!(model::offered_modes(&registry, view::ZOOM_IN), vec![view::ZOOM_MODIFIER, view::PAN]);
    // Tab command: the tab's own modes and its sub-context modes, never another tab's or the view set.
    assert_eq!(model::offered_modes(&registry, TAB_COMMAND), vec![TAB_MODE, SUB_MODE]);
    assert_eq!(model::offered_modes(&registry, SUB_COMMAND), vec![TAB_MODE, SUB_MODE]);
    // A session command has no session modes here.
    assert!(model::offered_modes(&registry, SESSION_COMMAND).is_empty());
    // Modes never take references; fixed and unknown actions offer nothing.
    assert!(model::offered_modes(&registry, TAB_MODE).is_empty());
    assert!(model::offered_modes(&registry, FIXED).is_empty());
    assert!(model::offered_modes(&registry, ActionId("test.unknown")).is_empty());
}

#[test]
fn offered_modes_keep_already_referenced_modes() {
    let mut registry = registry();
    let chord = Chord::new([Button::Key(egui::Key::T)]).with_modes([view::ZOOM_MODIFIER]);
    if let Err(err) = registry.set_bindings(TAB_COMMAND, vec![Binding::command(chord)]) {
        panic!("override rejected: {err}");
    }
    assert_eq!(model::offered_modes(&registry, TAB_COMMAND), vec![view::ZOOM_MODIFIER, TAB_MODE, SUB_MODE]);
}

#[test]
fn template_modes_follow_the_capture_template_rule() {
    let registry = registry();
    let zoom_modifier: BTreeSet<ActionId> = [view::ZOOM_MODIFIER].into_iter().collect();
    assert_eq!(model::template_modes(&registry, view::ZOOM_IN, CaptureSlot::Append), zoom_modifier);
    assert_eq!(model::template_modes(&registry, view::ZOOM_IN, CaptureSlot::Replace(1)), zoom_modifier);
    // `view.zoom` alternative 1 is the chord-less pinch.
    assert!(model::template_modes(&registry, view::ZOOM, CaptureSlot::Replace(1)).is_empty());
    assert_eq!(model::template_modes(&registry, view::ZOOM_DRAG, CaptureSlot::Append), zoom_modifier);
    assert!(model::template_modes(&registry, view::ZOOM_IN, CaptureSlot::Replace(9)).is_empty());
    assert!(model::template_modes(&registry, TAB_MODE, CaptureSlot::Append).is_empty());
}

#[test]
fn template_modes_fall_back_to_defaults_when_unbound() {
    let mut registry = registry();
    if let Err(err) = registry.set_bindings(view::ZOOM_OUT, Vec::new()) {
        panic!("unbinding rejected: {err}");
    }
    let zoom_modifier: BTreeSet<ActionId> = [view::ZOOM_MODIFIER].into_iter().collect();
    assert_eq!(model::template_modes(&registry, view::ZOOM_OUT, CaptureSlot::Append), zoom_modifier);
}

#[test]
fn modifier_choice_strips_contained_mode_chords() {
    let registry = registry();
    let selected: BTreeSet<ActionId> = [view::ZOOM_MODIFIER].into_iter().collect();
    let sets = |mode: ActionId| registry.mode_button_sets(mode);
    // Ctrl+= captured with the zoom modifier (Ctrl | Z) attached -> [zoom modifier] + =.
    let mut binding = Binding::command(Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(egui::Key::Equals)]));
    model::apply_modifier_choice(&mut binding, &selected, &sets);
    assert_eq!(binding, Binding::command(Chord::new([Button::Key(egui::Key::Equals)]).with_modes([view::ZOOM_MODIFIER])));
    // Only "=" captured: the reference is added, nothing to strip.
    let mut binding = Binding::command(Chord::new([Button::Key(egui::Key::Equals)]));
    model::apply_modifier_choice(&mut binding, &selected, &sets);
    assert_eq!(binding, Binding::command(Chord::new([Button::Key(egui::Key::Equals)]).with_modes([view::ZOOM_MODIFIER])));
}

#[test]
fn modifier_choice_deselect_removes_references_and_keeps_buttons() {
    let registry = registry();
    let sets = |mode: ActionId| registry.mode_button_sets(mode);
    let mut binding = Binding::command(Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(egui::Key::Equals)]).with_modes([view::ZOOM_MODIFIER]));
    model::apply_modifier_choice(&mut binding, &BTreeSet::new(), &sets);
    assert_eq!(binding, Binding::command(Chord::new([Button::Modifier(ModifierKey::Command), Button::Key(egui::Key::Equals)])));
}

#[test]
fn modifier_choice_touches_only_the_last_chord_and_skips_modes() {
    let registry = registry();
    let sets = |mode: ActionId| registry.mode_button_sets(mode);
    let selected: BTreeSet<ActionId> = [TAB_MODE].into_iter().collect();
    let first = Chord::new([Button::Key(egui::Key::Q), Button::Key(egui::Key::G)]);
    let last = Chord::new([Button::Key(egui::Key::Q), Button::Key(egui::Key::H)]);
    let mut binding = Binding::sequence(vec![first.clone(), last]);
    model::apply_modifier_choice(&mut binding, &selected, &sets);
    assert_eq!(binding, Binding::sequence(vec![first, Chord::new([Button::Key(egui::Key::H)]).with_modes([TAB_MODE])]));
    let mut mode_binding = Binding::mode([Button::Key(egui::Key::Q)]);
    model::apply_modifier_choice(&mut mode_binding, &selected, &sets);
    assert_eq!(mode_binding, Binding::mode([Button::Key(egui::Key::Q)]));
}

#[test]
fn modifier_choice_applies_to_gesture_and_axis_chords() {
    let registry = registry();
    let sets = |mode: ActionId| registry.mode_button_sets(mode);
    let selected: BTreeSet<ActionId> = [view::ZOOM_MODIFIER].into_iter().collect();
    let mut gesture = Binding::gesture(Chord::new([Button::Key(egui::Key::Z)]), Button::Mouse(ms_input::MouseButton::Left), GestureKind::Drag);
    model::apply_modifier_choice(&mut gesture, &selected, &sets);
    assert_eq!(gesture, Binding::gesture(Chord::default().with_modes([view::ZOOM_MODIFIER]), Button::Mouse(ms_input::MouseButton::Left), GestureKind::Drag));
    let mut axis = Binding::axis(AxisSource::WheelY, Chord::new([Button::Modifier(ModifierKey::Command), Button::Modifier(ModifierKey::Shift)]));
    model::apply_modifier_choice(&mut axis, &selected, &sets);
    assert_eq!(axis, Binding::axis(AxisSource::WheelY, Chord::new([Button::Modifier(ModifierKey::Shift)]).with_modes([view::ZOOM_MODIFIER])));
}

#[test]
fn append_templates_come_from_current_then_default_alternatives() {
    let mut registry = registry();
    assert_eq!(model::template_axis_source(&registry, view::ZOOM), Some(AxisSource::WheelY));
    assert_eq!(model::template_gesture_kind(&registry, view::ZOOM_DRAG), Some(GestureKind::Drag));
    assert_eq!(model::template_axis_source(&registry, view::ZOOM_IN), None);
    assert_eq!(model::template_gesture_kind(&registry, view::ZOOM), None);
    if let Err(err) = registry.set_bindings(view::ZOOM, vec![Binding::axis(AxisSource::Pinch, Chord::default())]) {
        panic!("override rejected: {err}");
    }
    assert_eq!(model::template_axis_source(&registry, view::ZOOM), Some(AxisSource::Pinch));
    if let Err(err) = registry.set_bindings(view::ZOOM, Vec::new()) {
        panic!("unbinding rejected: {err}");
    }
    assert_eq!(model::template_axis_source(&registry, view::ZOOM), Some(AxisSource::WheelY));
}

#[test]
fn conflict_and_shadow_rows_map_to_the_viewers_alternative() {
    let a = ActionId("a");
    let b = ActionId("b");
    let c = ActionId("c");
    let report = ConflictReport {
        conflicts: vec![
            Conflict { kind: ConflictKind::SameChord, a, a_alt: 1, b: Some(b), b_alt: Some(0) },
            Conflict { kind: ConflictKind::ReservedClipboard, a, a_alt: 0, b: None, b_alt: None },
            Conflict { kind: ConflictKind::PrefixOf, a: c, a_alt: 2, b: Some(a), b_alt: Some(1) },
        ],
        shadows: vec![Shadow { inner: a, inner_alt: 3, outer: b, outer_alt: 4 }],
        truncated: Vec::new(),
    };
    let for_a = model::conflicts_by_alternative(&report, a);
    assert_eq!(for_a.keys().copied().collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(for_a.get(&1).map(|rows| rows.iter().map(|conflict| conflict.kind).collect::<Vec<_>>()), Some(vec![ConflictKind::SameChord, ConflictKind::PrefixOf]));
    assert_eq!(for_a.get(&0).map(Vec::len), Some(1));
    let for_b = model::conflicts_by_alternative(&report, b);
    assert_eq!(for_b.keys().copied().collect::<Vec<_>>(), vec![0]);
    assert!(model::conflicts_by_alternative(&report, ActionId("none")).is_empty());
    assert_eq!(model::shadows_by_alternative(&report, a).keys().copied().collect::<Vec<_>>(), vec![3]);
    assert_eq!(model::shadows_by_alternative(&report, b).keys().copied().collect::<Vec<_>>(), vec![4]);
    assert!(model::shadows_by_alternative(&report, c).is_empty());
}

#[test]
fn search_matches_case_insensitively_and_empty_matches_all() {
    assert!(model::matches_query("", &["anything"]));
    assert!(model::matches_query("zoom", &["Reset Zoom", "Ctrl+0"]));
    assert!(model::matches_query("ctrl+0", &["Reset", "Ctrl+0"]));
    assert!(!model::matches_query("pan", &["Zoom in", "Ctrl+="]));
    assert!(model::matches_query("масштаб", &["Сбросить МАСШТАБ"]));
}

/// The teardown barrier writes every queued change, in order, before it returns: the last of two
/// edits of one action wins, an independent action is kept, and nothing is left for later.
#[test]
fn writer_shutdown_barrier_persists_every_queued_change_in_order() {
    let dir = std::env::temp_dir().join(format!("ms-settings-hotkeys-writer-{}-{:?}", std::process::id(), std::thread::current().id()));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        panic!("temp dir {}: {err}", dir.display());
    }
    let path = dir.join("user_config.json");
    let mut registry = registry();
    let mut entry = |id: ActionId, key: egui::Key| match registry.set_bindings(id, vec![Binding::mode([Button::Key(key)])]) {
        Ok(entry) => entry,
        Err(err) => panic!("override rejected: {err}"),
    };
    let first_pan = entry(view::PAN, egui::Key::B);
    let tab_mode = entry(TAB_MODE, egui::Key::Y);
    let last_pan = entry(view::PAN, egui::Key::N);
    let ctx = egui::Context::default();
    let mut writer = super::writer::OverrideWriter::new(path.clone());
    writer.submit(&ctx, first_pan);
    writer.submit(&ctx, tab_mode);
    writer.submit(&ctx, last_pan);
    let outcome = writer.shutdown_blocking();
    let text = std::fs::read_to_string(&path);
    if let Err(err) = std::fs::remove_dir_all(&dir) {
        panic!("cleanup of {}: {err}", dir.display());
    }
    assert_eq!(outcome, Ok(Vec::new()));
    let text = match text {
        Ok(text) => text,
        Err(err) => panic!("user config not written: {err}"),
    };
    let root: serde_json::Value = match serde_json::from_str(&text) {
        Ok(root) => root,
        Err(err) => panic!("user config is not JSON: {err}"),
    };
    let actions = &root["Hotkeys"]["actions"];
    let expected_pan = serde_json::Value::Array(vec![ms_input::persist::encode_binding(&Binding::mode([Button::Key(egui::Key::N)]))]);
    assert_eq!(actions[view::PAN.0], expected_pan, "{text}");
    assert!(actions[TAB_MODE.0].is_array(), "{text}");
    // Nothing queued: a second barrier is a no-op.
    assert_eq!(writer.shutdown_blocking(), Ok(Vec::new()));
}
