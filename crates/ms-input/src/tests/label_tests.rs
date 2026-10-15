//! The label format (`label.rs` header).

use super::{TAB, axis, command, ctrl, english, gesture, key, lmb, mode, registry_with_view, shift, target};
use crate::{ActionId, AxisSource, Binding, Button, Chord, GestureKind, MouseButton, Scope, TouchSource, button_label, view};

fn ctrl_name() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
}

fn recent_char(id: &'static str, digit: egui::Key) -> crate::ActionSpec {
    mode(id, Scope::Tab(TAB), vec![Binding::mode([key(digit)])], &[])
}

#[test]
fn alternatives_join_with_slash_and_mode_refs_print_the_current_chord() {
    english();
    let mut registry = registry_with_view(Vec::new(), Vec::new());
    let ctrl = ctrl_name();
    assert_eq!(registry.label(view::ZOOM_IN), format!("({ctrl} / Z)+= / ({ctrl} / Z)+Shift+= / ({ctrl} / Z)++"));
    assert_eq!(registry.label(view::ZOOM), format!("({ctrl} / Z)+Wheel / Pinch"));
    assert_eq!(registry.label(view::ZOOM_DRAG), format!("({ctrl} / Z)+LMB drag"));
    assert_eq!(registry.label(view::PAN), "Space");
    assert_eq!(registry.label(view::SCROLL_HORIZONTAL), "Shift+Wheel / Horizontal wheel");

    assert!(registry.set_bindings(view::ZOOM_MODIFIER, vec![Binding::mode([super::ctrl()])]).is_ok());
    assert_eq!(registry.label(view::ZOOM_IN), format!("{ctrl}+= / {ctrl}+Shift+= / {ctrl}++"), "one alternative prints inline");
    assert!(registry.set_bindings(view::ZOOM_MODIFIER, Vec::new()).is_ok());
    assert_eq!(registry.label(view::ZOOM_OUT), format!("(no binding)+{}", egui::Key::Minus.symbol_or_name()));
    assert!(registry.set_bindings(view::ZOOM_OUT, Vec::new()).is_ok());
    assert_eq!(registry.label(view::ZOOM_OUT), "no binding");
}

#[test]
fn sequences_gestures_touch_and_unknown_buttons() {
    english();
    let registry = registry_with_view(
        vec![
            mode("quick", Scope::Tab(TAB), vec![Binding::mode([shift()])], &[]),
            command("seq", Scope::Tab(TAB), vec![Binding::sequence(vec![Chord::new([ctrl(), key(egui::Key::K)]), Chord::new([key(egui::Key::Delete)])])]),
            gesture(
                "ocr",
                Scope::Tab(TAB),
                "page",
                vec![
                    Binding::gesture(Chord::default().with_modes([ActionId("quick")]), lmb(), GestureKind::Drag),
                    Binding::gesture(Chord::default(), Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag),
                    Binding::gesture(Chord::new([key(egui::Key::Space)]), Button::Mouse(MouseButton::Right), GestureKind::Click),
                ],
                Vec::new(),
            ),
            axis("rotate", Scope::Tab(TAB), vec![Binding::axis(AxisSource::WheelY, Chord::new([super::ctrl()]))]),
        ],
        vec![target("page", 10)],
    );
    let ctrl = ctrl_name();
    assert_eq!(registry.label(ActionId("seq")), format!("{ctrl}+K, Del"));
    assert_eq!(registry.label(ActionId("ocr")), "Shift+LMB drag / Long press and drag / Space+RMB click");
    assert_eq!(registry.label(ActionId("rotate")), format!("{ctrl}+Wheel"));
    assert_eq!(button_label(&Button::Unknown { device: "pen".to_owned(), code: 3 }), "pen button 3");
    assert_eq!(button_label(&Button::Mouse(MouseButton::Extra(2))), "Mouse button 2");
    assert_eq!(registry.label(ActionId("ghost")), "no binding");
}

#[test]
fn parameter_groups_print_a_range_or_the_member_labels() {
    english();
    let digits = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6];
    let ids = ["rc.1", "rc.2", "rc.3", "rc.4", "rc.5", "rc.6"];
    let actions = ids.iter().zip(digits).map(|(id, digit)| recent_char(id, digit)).collect();
    let mut registry = registry_with_view(actions, Vec::new());
    let group: Vec<ActionId> = ids.iter().map(|id| ActionId(id)).collect();
    assert_eq!(registry.param_group_label(&group), "1-6");
    assert!(registry.set_bindings(ActionId("rc.3"), vec![Binding::mode([key(egui::Key::X)])]).is_ok());
    assert_eq!(registry.param_group_label(&group), "1 / 2 / X / 4 / 5 / 6");
    assert_eq!(registry.param_group_label(&[]), "no binding");
}

#[test]
fn conflict_and_shadow_texts_name_the_other_action() {
    english();
    let registry = registry_with_view(
        vec![
            command("a", Scope::View, vec![Binding::command(Chord::new([key(egui::Key::Num0)]).with_modes([view::ZOOM_MODIFIER]))]),
            axis("rotate", Scope::Sub(Some(TAB)), vec![Binding::axis(AxisSource::WheelY, Chord::new([super::ctrl()]))]),
        ],
        Vec::new(),
    );
    let conflicts = registry.conflicts_for(ActionId("a"));
    let [conflict] = conflicts.as_slice() else { panic!("expected one conflict, got {conflicts:?}") };
    assert_eq!(registry.conflict_text(conflict, ActionId("a")), "Same binding as \"Reset zoom\"");
    let shadows = registry.shadows_for(view::ZOOM);
    let [shadow] = shadows.as_slice() else { panic!("expected one shadow, got {shadows:?}") };
    assert_eq!(registry.shadow_text(shadow, view::ZOOM), "While \"Zoom\" is active, it takes over this binding");
    assert_eq!(registry.shadow_text(shadow, ActionId("rotate")), "Takes over a binding of \"Zoom\" while active here");
}
