//! Declaration validation and binding changes (`registry.rs`).

use serde_json::json;

use super::{TAB, command, ctrl, gesture, key, lmb, mode, registry_with_view, shift, target};
use crate::{
    ActionId, Binding, BindingError, Chord, DeclarationError, Declarations, GestureKind, HotkeyOverrides, InputRegistry, OverrideWarning, Scope, view,
};
use crate::binding::MAX_ALTERNATIVES;

fn single(buttons: Vec<crate::Button>) -> Binding {
    Binding::command(Chord::new(buttons))
}

fn build(actions: Vec<crate::ActionSpec>) -> Result<InputRegistry, DeclarationError> {
    InputRegistry::new(Declarations { actions, ..Declarations::default() })
}

#[test]
fn invalid_declarations_are_rejected() {
    let duplicate = build(vec![command("a", Scope::Session, Vec::new()), command("a", Scope::Session, Vec::new())]);
    assert_eq!(duplicate.err(), Some(DeclarationError::DuplicateAction("a")));

    let unknown_ref = build(vec![command("a", Scope::Session, vec![Binding::command(Chord::new([key(egui::Key::A)]).with_modes([ActionId("ghost")]))])]);
    assert_eq!(
        unknown_ref.err(),
        Some(DeclarationError::InvalidDefault { action: "a", source: BindingError::UnknownModeRef { action: "a", reference: "ghost" } })
    );

    let ref_to_command = build(vec![
        command("b", Scope::Session, Vec::new()),
        command("a", Scope::Session, vec![Binding::command(Chord::new([key(egui::Key::A)]).with_modes([ActionId("b")]))]),
    ]);
    assert_eq!(
        ref_to_command.err(),
        Some(DeclarationError::InvalidDefault { action: "a", source: BindingError::RefNotAMode { action: "a", reference: "b" } })
    );

    let kind_mismatch = build(vec![mode("m", Scope::Session, vec![single(vec![shift()])], &[])]);
    assert!(matches!(kind_mismatch, Err(DeclarationError::InvalidDefault { source: BindingError::KindMismatch { .. }, .. })));

    let unknown_target = build(vec![gesture("g", Scope::Session, "nowhere", vec![Binding::gesture(Chord::default(), lmb(), GestureKind::Click)], Vec::new())]);
    assert_eq!(unknown_target.err(), Some(DeclarationError::UnknownTarget { action: "g", target: "nowhere" }));

    let key_gesture = InputRegistry::new(Declarations {
        actions: vec![gesture("g", Scope::Session, "t", vec![Binding::gesture(Chord::default(), key(egui::Key::A), GestureKind::Click)], Vec::new())],
        targets: vec![target("t", 1)],
        ..Declarations::default()
    });
    assert!(matches!(key_gesture, Err(DeclarationError::InvalidDefault { source: BindingError::InvalidGestureButton { .. }, .. })));

    let beats_command = build(vec![command("c", Scope::Session, Vec::new()), mode("m", Scope::Session, vec![Binding::mode([shift()])], &["c"])]);
    assert_eq!(beats_command.err(), Some(DeclarationError::UnknownBeats { action: "m", mode: "c" }));
}

#[test]
fn set_bindings_validates_and_reports_the_persisted_consequence() {
    let mut registry = registry_with_view(vec![command("a", Scope::Tab(TAB), vec![single(vec![key(egui::Key::A)])])], Vec::new());
    let revision = registry.revision();

    let with_ref = Binding::command(Chord::new([key(egui::Key::B)]).with_modes([view::ZOOM_MODIFIER]));
    let entry = registry.set_bindings(ActionId("a"), vec![with_ref.clone(), single(vec![key(egui::Key::C)])]);
    assert!(matches!(&entry, Ok(entry) if entry.alternatives.as_ref().is_some_and(|alts| alts.len() == 2)), "a user override may reference modes");
    assert_eq!(registry.bindings(ActionId("a")).first(), Some(&with_ref));
    assert_eq!(registry.revision(), revision + 1);

    let unbound = registry.set_bindings(ActionId("a"), Vec::new());
    assert_eq!(unbound.map(|entry| entry.alternatives), Ok(Some(Vec::new())), "[] is an explicit unbind");

    let back_to_default = registry.set_bindings(ActionId("a"), vec![single(vec![key(egui::Key::A)])]);
    assert_eq!(back_to_default.map(|entry| entry.alternatives), Ok(None), "the defaults are never frozen into an override");

    let bad_ref = registry.set_bindings(ActionId("a"), vec![Binding::command(Chord::new([key(egui::Key::B)]).with_modes([view::ZOOM_IN]))]);
    assert_eq!(bad_ref.err(), Some(BindingError::RefNotAMode { action: "a", reference: "view.zoom_in" }));
    assert_eq!(registry.bindings(ActionId("a")), &[single(vec![key(egui::Key::A)])], "a rejected change changes nothing");

    assert_eq!(registry.set_bindings(view::SCROLL, Vec::new()).err(), Some(BindingError::FixedAction("view.scroll")));
    assert_eq!(registry.set_bindings(ActionId("ghost"), Vec::new()).err(), Some(BindingError::UnknownAction("ghost".to_owned())));
    assert_eq!(registry.reset(ActionId("a")).map(|entry| entry.alternatives), Ok(None));
}

#[test]
fn overrides_apply_whole_entries_and_skip_bad_ones() {
    let mut registry = registry_with_view(vec![command("a", Scope::Tab(TAB), vec![single(vec![key(egui::Key::A)])])], Vec::new());
    let mut overrides = HotkeyOverrides::default();
    overrides.actions.insert("a".to_owned(), vec![json!({"sequence": [{"modes": ["view.zoom_modifier"], "buttons": ["key:B"]}]})]);
    overrides.actions.insert("view.zoom_out".to_owned(), vec![json!({"sequence": [{"buttons": ["key:Minus"]}]}), json!({"sequence": "broken"})]);
    overrides.actions.insert("view.zoom_modifier".to_owned(), vec![json!({"modes": ["view.pan"], "buttons": ["key:X"]})]);
    overrides.actions.insert("view.scroll".to_owned(), Vec::new());
    overrides.actions.insert("ghost".to_owned(), Vec::new());
    let warnings = registry.apply_overrides(&overrides);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(warnings.contains(&OverrideWarning::UnknownAction("ghost".to_owned())));
    assert!(warnings.contains(&OverrideWarning::FixedAction("view.scroll".to_owned())));
    assert!(warnings.iter().any(|warning| matches!(warning, OverrideWarning::Undecodable { id, index: 1, .. } if id == "view.zoom_out")));
    assert!(warnings.iter().any(|warning| matches!(warning, OverrideWarning::Undecodable { id, source: crate::DecodeError::ModeRefsInMode, .. } if id == "view.zoom_modifier")));
    assert_eq!(registry.bindings(ActionId("a")), &[Binding::command(Chord::new([key(egui::Key::B)]).with_modes([view::ZOOM_MODIFIER]))]);
    assert_eq!(registry.bindings(view::ZOOM_OUT), registry.defaults(view::ZOOM_OUT), "a partly broken entry keeps the defaults");
    assert!(registry.action("a").is_some_and(|view| view.overridden));
    assert_eq!(registry.mode_button_sets(view::ZOOM_MODIFIER).len(), 2);
    assert!(registry.resolve(&Chord::new([key(egui::Key::A)]).with_modes([view::ZOOM_MODIFIER])).contains(&[ctrl(), key(egui::Key::A)].into_iter().collect()));
}

#[test]
fn beats_self_edges_and_cycles_are_rejected() {
    let self_edge = build(vec![mode("a", Scope::Session, vec![Binding::mode([shift()])], &["a"])]);
    assert_eq!(self_edge.err(), Some(DeclarationError::BeatsCycle("a")));
    let cycle = build(vec![
        mode("a", Scope::Session, vec![Binding::mode([shift()])], &["b"]),
        mode("b", Scope::Session, vec![Binding::mode([ctrl()])], &["c"]),
        mode("c", Scope::Session, vec![Binding::mode([key(egui::Key::Q)])], &["a"]),
    ]);
    assert!(matches!(cycle, Err(DeclarationError::BeatsCycle(_))));
    let order = build(vec![
        mode("a", Scope::Session, vec![Binding::mode([shift()])], &["b", "c"]),
        mode("b", Scope::Session, vec![Binding::mode([ctrl()])], &["c"]),
        mode("c", Scope::Session, vec![Binding::mode([key(egui::Key::Q)])], &[]),
    ]);
    assert!(order.is_ok(), "a diamond-free precedence order is valid");
}

fn modes_with_alternatives(count: u32) -> Vec<Binding> {
    (0..count).map(|code| Binding::mode([crate::Button::Unknown { device: "pad".to_owned(), code }])).collect()
}

#[test]
fn limits_reject_oversized_bindings_without_applying_them() {
    let refs = ["m0", "m1", "m2", "m3", "m4"];
    let mut actions: Vec<crate::ActionSpec> = refs.iter().map(|id| mode(id, Scope::Tab(TAB), vec![Binding::mode([shift()])], &[])).collect();
    actions.push(command("a", Scope::Tab(TAB), vec![single(vec![key(egui::Key::A)])]));
    let mut registry = registry_with_view(actions, Vec::new());
    let ids: Vec<ActionId> = refs.iter().map(|id| ActionId(id)).collect();

    let too_many_alternatives = registry.set_bindings(ActionId("a"), (0..=MAX_ALTERNATIVES).map(|_| single(vec![key(egui::Key::A)])).collect());
    assert_eq!(too_many_alternatives.err(), Some(BindingError::TooManyAlternatives { action: "a", count: MAX_ALTERNATIVES + 1 }));
    let too_many_refs = registry.set_bindings(ActionId("a"), vec![Binding::command(Chord::new([key(egui::Key::A)]).with_modes(ids.iter().copied()))]);
    assert_eq!(too_many_refs.err(), Some(BindingError::TooManyModeRefs { action: "a" }));

    // Two refs to 8-alternative modes = 64 combinations: allowed; a ninth alternative is refused
    // because it would push the dependent binding to 72.
    assert!(registry.set_bindings(ActionId("m0"), modes_with_alternatives(8)).is_ok());
    assert!(registry.set_bindings(ActionId("m1"), modes_with_alternatives(8)).is_ok());
    let two_refs = Binding::command(Chord::new([key(egui::Key::A)]).with_modes([ActionId("m0"), ActionId("m1")]));
    assert!(registry.set_bindings(ActionId("a"), vec![two_refs.clone()]).is_ok());
    assert_eq!(registry.set_bindings(ActionId("m0"), modes_with_alternatives(9)).err(), Some(BindingError::TooManyCombinations { action: "a" }));
    let three_refs = Binding::command(Chord::new([key(egui::Key::B)]).with_modes([ActionId("m0"), ActionId("m1"), ActionId("m2")]));
    assert!(registry.set_bindings(ActionId("m2"), modes_with_alternatives(2)).is_ok());
    assert_eq!(registry.set_bindings(ActionId("a"), vec![three_refs]).err(), Some(BindingError::TooManyCombinations { action: "a" }));
    assert_eq!(registry.bindings(ActionId("a")), &[two_refs], "refused changes change nothing");
}

#[test]
fn oversized_override_entries_are_skipped_with_a_warning() {
    let refs = ["m0", "m1", "m2", "m3"];
    let mut actions: Vec<crate::ActionSpec> = refs.iter().map(|id| mode(id, Scope::Tab(TAB), vec![Binding::mode([shift()])], &[])).collect();
    actions.push(command("a", Scope::Tab(TAB), vec![single(vec![key(egui::Key::A)])]));
    actions.push(command("b", Scope::Tab(TAB), vec![Binding::command(Chord::new([key(egui::Key::B)]).with_modes([ActionId("m0"), ActionId("m1")]))]));
    let mut registry = registry_with_view(actions, Vec::new());
    let mode_alternatives = |count: u32| -> Vec<serde_json::Value> { (0..count).map(|code| json!({"buttons": [{"device": "pad", "code": code}]})).collect() };
    let mut overrides = HotkeyOverrides::default();
    // Four refs to 16-alternative modes would be 65536 combinations: never materialized.
    overrides.actions.insert("a".to_owned(), vec![json!({"sequence": [{"modes": refs, "buttons": ["key:A"]}]})]);
    overrides.actions.insert("m2".to_owned(), mode_alternatives(16));
    overrides.actions.insert("m3".to_owned(), mode_alternatives(16));
    // Mode entries apply in id order: m0 (16 alternatives) keeps `b` at 16 combinations and is
    // accepted; m1 (8) would then push `b` to 128 and is refused.
    overrides.actions.insert("m0".to_owned(), mode_alternatives(16));
    overrides.actions.insert("m1".to_owned(), mode_alternatives(8));
    overrides.actions.insert("view.zoom_in".to_owned(), (0..=MAX_ALTERNATIVES).map(|_| json!({"sequence": [{"buttons": ["key:A"]}]})).collect());
    let warnings = registry.apply_overrides(&overrides);
    let skipped: Vec<&str> = warnings
        .iter()
        .filter_map(|warning| match warning {
            OverrideWarning::Invalid { id, .. } => Some(id.as_str()),
            OverrideWarning::UnknownAction(_) | OverrideWarning::FixedAction(_) | OverrideWarning::Undecodable { .. } => None,
        })
        .collect();
    assert_eq!(skipped, vec!["m1", "a", "view.zoom_in"], "{warnings:?}");
    assert_eq!(registry.bindings(ActionId("a")), registry.defaults(ActionId("a")));
    assert_eq!(registry.bindings(ActionId("m0")).len(), 16);
    assert_eq!(registry.bindings(ActionId("m1")).len(), 1);
    assert_eq!(registry.bindings(ActionId("m2")).len(), 16);
}
