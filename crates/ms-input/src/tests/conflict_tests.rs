//! Conflict and shadow rules (`conflicts.rs` header).

use super::{TAB, axis, command, command_with, ctrl, key, mode, registry, registry_with_view, shift};
use crate::{ActionId, Binding, Chord, Condition, ConflictKind, ContextId, Declarations, InputRegistry, Scope, Shadow, view};

fn single(buttons: Vec<crate::Button>) -> Binding {
    Binding::command(Chord::new(buttons))
}

fn kinds(registry: &InputRegistry) -> Vec<(ConflictKind, &'static str, Option<&'static str>)> {
    registry.conflicts().conflicts.iter().map(|conflict| (conflict.kind, conflict.a.0, conflict.b.map(|b| b.0))).collect()
}

#[test]
fn the_view_set_is_conflict_free() {
    let registry = registry_with_view(Vec::new(), Vec::new());
    assert!(registry.conflicts().conflicts.is_empty(), "{:?}", registry.conflicts());
    assert!(registry.conflicts().shadows.is_empty(), "{:?}", registry.conflicts());
}

#[test]
fn equal_chords_on_one_level_conflict_and_across_levels_shadow() {
    let registry = registry(vec![
        command("a", Scope::Tab(TAB), vec![single(vec![key(egui::Key::D)])]),
        command("b", Scope::Tab(TAB), vec![single(vec![key(egui::Key::A)]), single(vec![key(egui::Key::D)])]),
        command("other_tab", Scope::Tab(ms_config::app_tab::AppTab::Cleaning), vec![single(vec![key(egui::Key::D)])]),
        command("session", Scope::Session, vec![single(vec![key(egui::Key::D)])]),
    ]);
    assert_eq!(kinds(&registry), vec![(ConflictKind::SameChord, "a", Some("b"))]);
    let conflict = registry.conflicts().conflicts.first().copied();
    assert_eq!(conflict.map(|conflict| (conflict.a_alt, conflict.b_alt)), Some((0, Some(1))), "the alternative indices are reported");
    assert_eq!(registry.conflicts_for(ActionId("b")).len(), 1);
    assert!(registry.conflicts_for(ActionId("other_tab")).is_empty(), "another tab is never active together");
    let shadowed_by: Vec<&str> = registry.shadows_for(ActionId("session")).iter().map(|shadow| shadow.inner.0).collect();
    assert_eq!(shadowed_by, vec!["a", "b", "other_tab"]);
}

#[test]
fn a_sequence_prefix_in_one_scope_is_a_conflict() {
    let registry = registry(vec![
        command("short", Scope::Tab(TAB), vec![single(vec![key(egui::Key::G)])]),
        command("long", Scope::Tab(TAB), vec![Binding::sequence(vec![Chord::new([key(egui::Key::G)]), Chord::new([key(egui::Key::H)])])]),
    ]);
    assert_eq!(kinds(&registry), vec![(ConflictKind::PrefixOf, "short", Some("long"))]);
}

#[test]
fn an_inner_ctrl_wheel_shadows_only_the_command_alternative_of_view_zoom() {
    let registry = registry_with_view(vec![axis("rotate", Scope::Sub(Some(TAB)), vec![Binding::axis(crate::AxisSource::WheelY, Chord::new([ctrl()]))])], Vec::new());
    assert_eq!(registry.conflicts().conflicts, [] as [crate::Conflict; 0]);
    assert_eq!(registry.conflicts().shadows, vec![Shadow { inner: ActionId("rotate"), inner_alt: 0, outer: view::ZOOM, outer_alt: 0 }], "the pinch alternative stays unshadowed");
}

#[test]
fn reserved_clipboard_chords_are_reported() {
    let registry = registry(vec![
        command("copy", Scope::Tab(TAB), vec![single(vec![ctrl(), key(egui::Key::C)])]),
        command("copy_shift", Scope::Tab(TAB), vec![single(vec![ctrl(), shift(), key(egui::Key::C)])]),
        command("shift_delete", Scope::Tab(TAB), vec![single(vec![shift(), key(egui::Key::Delete)])]),
    ]);
    let mut expected = vec![(ConflictKind::ReservedClipboard, "copy", None)];
    if cfg!(windows) {
        expected.push((ConflictKind::ReservedClipboard, "shift_delete", None));
    }
    assert_eq!(kinds(&registry), expected);
}

#[test]
fn modes_conflict_only_on_equal_chords_without_beats() {
    let registry = registry(vec![
        mode("quick", Scope::Tab(TAB), vec![Binding::mode([shift()])], &[]),
        mode("crop", Scope::Tab(TAB), vec![Binding::mode([shift(), key(egui::Key::Q)])], &["quick"]),
        mode("advanced", Scope::Tab(TAB), vec![Binding::mode([shift()])], &["quick"]),
        mode("twin", Scope::Tab(TAB), vec![Binding::mode([ctrl()])], &[]),
        mode("twin2", Scope::Tab(TAB), vec![Binding::mode([ctrl()])], &[]),
        command("mod_command", Scope::Tab(TAB), vec![single(vec![ctrl()])]),
    ]);
    let found = kinds(&registry);
    assert!(found.contains(&(ConflictKind::SameChord, "twin", Some("twin2"))));
    assert!(found.contains(&(ConflictKind::SameChord, "twin", Some("mod_command"))), "a command press would suppress the mode");
    assert!(!found.iter().any(|(_, a, b)| [*a, b.unwrap_or("")].contains(&"crop")), "subset chords are ordered, not conflicting");
    assert!(!found.iter().any(|(_, a, b)| *a == "quick" && *b == Some("advanced")), "beats orders equal chords");
}

#[test]
fn exclusive_and_contradictory_contexts_never_conflict() {
    let in_context = |id: &'static str, context: &'static str| {
        command_with(id, Scope::Sub(Some(TAB)), vec![Condition::Context(ContextId(context))], vec![single(vec![key(egui::Key::Escape)])], Vec::new(), false)
    };
    let mut decls = Declarations {
        actions: vec![
            in_context("region_cancel", "tool.region"),
            in_context("patch_cancel", "tool.patch"),
            command_with("outside", Scope::Sub(Some(TAB)), vec![Condition::NotContext(ContextId("tool.region"))], vec![single(vec![key(egui::Key::Escape)])], Vec::new(), false),
        ],
        ..Declarations::default()
    };
    let without_group = InputRegistry::new(decls.clone()).map(|registry| kinds(&registry));
    assert_eq!(
        without_group,
        Ok(vec![(ConflictKind::SameChord, "region_cancel", Some("patch_cancel")), (ConflictKind::SameChord, "patch_cancel", Some("outside"))]),
        "Context(X) and NotContext(X) are never active together"
    );
    decls.exclusive_contexts.push(vec![ContextId("tool.region"), ContextId("tool.patch")]);
    let with_group = InputRegistry::new(decls).map(|registry| kinds(&registry));
    assert_eq!(with_group, Ok(vec![(ConflictKind::SameChord, "patch_cancel", Some("outside"))]));
}

#[test]
fn accepted_modes_count_as_chord_buttons_for_commands() {
    let registry = registry(vec![
        mode("recent.2", Scope::Tab(TAB), vec![Binding::mode([key(egui::Key::Num2)])], &[]),
        command_with(
            "create",
            Scope::Tab(TAB),
            Vec::new(),
            vec![single(vec![key(egui::Key::T)])],
            vec![crate::Accept { mode: ActionId("recent.2"), sample: crate::Sample::AtPress }],
            false,
        ),
        command("two_t", Scope::Tab(TAB), vec![single(vec![key(egui::Key::Num2), key(egui::Key::T)])]),
    ]);
    assert_eq!(kinds(&registry), vec![(ConflictKind::SameChord, "create", Some("two_t"))], "with Num2 held both match T with equal specificity");
    assert_eq!(registry.conflicts().truncated, [] as [ActionId; 0]);
}

#[test]
fn accepted_modes_count_as_chord_buttons_for_gestures() {
    let registry = super::registry_with_view(
        vec![
            mode("erase", Scope::Sub(None), vec![Binding::mode([shift()])], &[]),
            super::gesture(
                "stroke",
                Scope::Sub(None),
                "page",
                vec![Binding::gesture(Chord::default(), super::lmb(), crate::GestureKind::Drag)],
                vec![crate::Accept { mode: ActionId("erase"), sample: crate::Sample::LatestDuringGesture }],
            ),
            super::gesture("shift_drag", Scope::Sub(None), "page", vec![Binding::gesture(Chord::new([shift()]), super::lmb(), crate::GestureKind::Drag)], Vec::new()),
            super::gesture("other_target", Scope::Sub(None), "handle", vec![Binding::gesture(Chord::new([shift()]), super::lmb(), crate::GestureKind::Drag)], Vec::new()),
        ],
        vec![super::target("page", 10), super::target("handle", 20)],
    );
    assert_eq!(kinds(&registry), vec![(ConflictKind::SameGesture, "stroke", Some("shift_drag"))]);
}

#[test]
fn accepted_mode_expansion_is_bounded_and_reported() {
    // 17 alternative choices per accepted mode (none + 16) across four modes: far past the limit.
    let alternatives = |base: u8| -> Vec<Binding> {
        (0..16_u8).map(|code| Binding::mode([crate::Button::Unknown { device: "pad".to_owned(), code: u32::from(base) * 100 + u32::from(code) }])).collect()
    };
    let accepts = ["m0", "m1", "m2", "m3"].iter().map(|id| crate::Accept { mode: ActionId(id), sample: crate::Sample::AtPress }).collect();
    let registry = registry(vec![
        mode("m0", Scope::Tab(TAB), alternatives(0), &[]),
        mode("m1", Scope::Tab(TAB), alternatives(1), &[]),
        mode("m2", Scope::Tab(TAB), alternatives(2), &[]),
        mode("m3", Scope::Tab(TAB), alternatives(3), &[]),
        command_with("wide", Scope::Tab(TAB), Vec::new(), vec![single(vec![key(egui::Key::T)])], accepts, false),
    ]);
    assert_eq!(registry.conflicts().truncated, vec![ActionId("wide")]);
}
