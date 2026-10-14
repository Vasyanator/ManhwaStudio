//! The matcher contract (`matching.rs` header).

use std::collections::BTreeSet;

use super::{TAB, axis, command, command_with, ctrl, gesture, key, lmb, mode, registry, registry_with_view, shift, target};
use crate::{
    Accept, ActionId, ActiveSet, AxisSource, Binding, Button, Chord, ContextId, GestureKind, InputRegistry, MatchEvent, MatchOutcome, MatchState,
    Sample, Scope, TargetId, view,
};

fn env() -> ActiveSet {
    ActiveSet { active_tab: Some(TAB), pointer_on_view: true, ..ActiveSet::default() }
}

fn press(state: &mut MatchState, registry: &InputRegistry, button: &Button, env: &ActiveSet) -> Vec<&'static str> {
    fired(&state.step(registry, &MatchEvent::Press { button: button.clone(), repeat: false }, env))
}

fn repeat(state: &mut MatchState, registry: &InputRegistry, button: &Button, env: &ActiveSet) -> Vec<&'static str> {
    fired(&state.step(registry, &MatchEvent::Press { button: button.clone(), repeat: true }, env))
}

fn release(state: &mut MatchState, registry: &InputRegistry, button: &Button, env: &ActiveSet) {
    assert!(state.step(registry, &MatchEvent::Release { button: button.clone() }, env).is_empty());
}

fn fired(outcomes: &[MatchOutcome]) -> Vec<&'static str> {
    outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            MatchOutcome::Fired(fired) => Some(fired.action.0),
            MatchOutcome::SequencePending => None,
        })
        .collect()
}

fn single(buttons: Vec<Button>) -> Binding {
    Binding::command(Chord::new(buttons))
}

#[test]
fn modifiers_are_exact_shift_p_does_not_fire_p() {
    let registry = registry(vec![command("p", Scope::Tab(TAB), vec![single(vec![key(egui::Key::P)])])]);
    let (mut state, env) = (MatchState::default(), env());
    assert!(press(&mut state, &registry, &shift(), &env).is_empty());
    assert!(press(&mut state, &registry, &key(egui::Key::P), &env).is_empty(), "Shift+P must not fire P");
    release(&mut state, &registry, &key(egui::Key::P), &env);
    release(&mut state, &registry, &shift(), &env);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::P), &env), vec!["p"]);
}

#[test]
fn the_completing_press_fires_and_a_modifier_may_complete_the_chord() {
    let registry = registry(vec![command("save", Scope::Session, vec![single(vec![ctrl(), key(egui::Key::S)])])]);
    let (mut state, env) = (MatchState::default(), env());
    assert!(press(&mut state, &registry, &key(egui::Key::S), &env).is_empty());
    assert_eq!(press(&mut state, &registry, &ctrl(), &env), vec!["save"], "Ctrl pressed last completes Ctrl+S");
    assert!(press(&mut state, &registry, &key(egui::Key::A), &env).is_empty(), "a press outside the chord does not re-fire it");
}

#[test]
fn a_command_press_suppresses_the_mode_of_that_button_until_release() {
    let registry = registry(vec![
        mode("zoom_z", Scope::View, vec![Binding::mode([key(egui::Key::Z)])], &[]),
        command("undo", Scope::Tab(TAB), vec![single(vec![ctrl(), key(egui::Key::Z)])]),
    ]);
    let (mut state, env) = (MatchState::default(), env());
    let zoom = ActionId("zoom_z");
    press(&mut state, &registry, &ctrl(), &env);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::Z), &env), vec!["undo"]);
    assert!(!state.mode_active(&registry, zoom, &env), "Z that completed Ctrl+Z must not activate the Z mode");
    release(&mut state, &registry, &ctrl(), &env);
    assert!(!state.mode_active(&registry, zoom, &env), "still suppressed while Z stays held");
    release(&mut state, &registry, &key(egui::Key::Z), &env);
    press(&mut state, &registry, &key(egui::Key::Z), &env);
    assert!(state.mode_active(&registry, zoom, &env), "a fresh Z press activates the mode");
}

#[test]
fn sequences_fire_on_the_last_chord_and_reset_on_a_non_continuing_press() {
    let registry = registry(vec![command("seq", Scope::Tab(TAB), vec![Binding::sequence(vec![Chord::new([key(egui::Key::G)]), Chord::new([key(egui::Key::H)])])])]);
    let (mut state, env) = (MatchState::default(), env());
    let pending = state.step(&registry, &MatchEvent::Press { button: key(egui::Key::G), repeat: false }, &env);
    assert_eq!(pending, vec![MatchOutcome::SequencePending]);
    release(&mut state, &registry, &key(egui::Key::G), &env);
    press(&mut state, &registry, &shift(), &env);
    release(&mut state, &registry, &shift(), &env);
    assert!(state.sequence_pending(), "a modifier press does not reset a pending sequence");
    assert_eq!(press(&mut state, &registry, &key(egui::Key::H), &env), vec!["seq"]);
    release(&mut state, &registry, &key(egui::Key::H), &env);

    press(&mut state, &registry, &key(egui::Key::G), &env);
    release(&mut state, &registry, &key(egui::Key::G), &env);
    assert!(press(&mut state, &registry, &key(egui::Key::X), &env).is_empty());
    release(&mut state, &registry, &key(egui::Key::X), &env);
    assert!(!state.sequence_pending(), "X continued nothing and reset the sequence");
    assert!(press(&mut state, &registry, &key(egui::Key::H), &env).is_empty(), "no timeout-free resurrection");
}

#[test]
fn more_buttons_win_before_the_inner_scope_and_the_inner_scope_breaks_ties() {
    let registry = registry(vec![
        command("outer_two_keys", Scope::Session, vec![single(vec![key(egui::Key::Num2), key(egui::Key::S)])]),
        command("inner_one_key", Scope::Tab(TAB), vec![single(vec![key(egui::Key::S)])]),
        command("outer_same", Scope::Session, vec![single(vec![key(egui::Key::K)])]),
        command("inner_same", Scope::Sub(None), vec![single(vec![key(egui::Key::K)])]),
    ]);
    let (mut state, env) = (MatchState::default(), env());
    press(&mut state, &registry, &key(egui::Key::Num2), &env);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::S), &env), vec!["outer_two_keys"], "the more specific chord wins");
    release(&mut state, &registry, &key(egui::Key::S), &env);
    release(&mut state, &registry, &key(egui::Key::Num2), &env);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::S), &env), vec!["inner_one_key"]);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::K), &env), vec!["inner_same"], "equal chords: the innermost scope wins");
}

#[test]
fn accepted_parameter_modes_are_reported_and_extra_keys_tolerated() {
    let registry = registry(vec![
        mode("recent.2", Scope::Tab(TAB), vec![Binding::mode([key(egui::Key::Num2)])], &[]),
        command_with(
            "create",
            Scope::Tab(TAB),
            Vec::new(),
            vec![single(vec![key(egui::Key::T)])],
            vec![Accept { mode: ActionId("recent.2"), sample: Sample::AtPress }],
            false,
        ),
    ]);
    let (mut state, env) = (MatchState::default(), env());
    press(&mut state, &registry, &key(egui::Key::Num2), &env);
    let outcomes = state.step(&registry, &MatchEvent::Press { button: key(egui::Key::T), repeat: false }, &env);
    match outcomes.as_slice() {
        [MatchOutcome::Fired(fired)] => {
            assert_eq!(fired.action, ActionId("create"));
            assert_eq!(fired.params, vec![ActionId("recent.2")]);
        }
        other => panic!("expected one fired command, got {other:?}"),
    }
}

#[test]
fn modes_match_by_subset_and_beats_orders_them() {
    let registry = registry(vec![
        mode("quick", Scope::Tab(TAB), vec![Binding::mode([shift()])], &[]),
        mode("crop", Scope::Tab(TAB), vec![Binding::mode([shift(), key(egui::Key::Q)])], &["quick"]),
        mode("rect", Scope::Tab(TAB), vec![Binding::mode([ctrl()])], &[]),
    ]);
    let (mut state, env) = (MatchState::default(), env());
    press(&mut state, &registry, &shift(), &env);
    assert_eq!(state.active_modes(&registry, &env), BTreeSet::from([ActionId("quick")]));
    press(&mut state, &registry, &key(egui::Key::Q), &env);
    assert_eq!(state.active_modes(&registry, &env), BTreeSet::from([ActionId("crop")]), "crop beats quick");
    release(&mut state, &registry, &key(egui::Key::Q), &env);
    press(&mut state, &registry, &ctrl(), &env);
    assert_eq!(state.active_modes(&registry, &env), BTreeSet::from([ActionId("quick"), ActionId("rect")]), "independent modes are active together");
}

#[test]
fn gesture_params_sample_at_press_or_latest() {
    let registry = registry_with_view(
        vec![
            mode("erase", Scope::Sub(None), vec![Binding::mode([shift()])], &[]),
            mode("rect", Scope::Sub(None), vec![Binding::mode([ctrl()])], &[]),
            gesture(
                "stroke",
                Scope::Sub(None),
                "page",
                vec![Binding::gesture(Chord::default(), lmb(), GestureKind::Drag)],
                vec![Accept { mode: ActionId("erase"), sample: Sample::LatestDuringGesture }, Accept { mode: ActionId("rect"), sample: Sample::AtPress }],
            ),
        ],
        vec![target("page", 10)],
    );
    let (mut state, env) = (MatchState::default(), env());
    let targets = [TargetId("page"), view::SURFACE];
    press(&mut state, &registry, &ctrl(), &env);
    press(&mut state, &registry, &lmb(), &env);
    let Some(matched) = state.match_gesture(&registry, &lmb(), GestureKind::Drag, &targets, &env) else { panic!("Ctrl+LMB drag must match a gesture") };
    assert_eq!(matched.action, ActionId("stroke"), "accepted Ctrl ties with the zoom drag; the higher target priority wins");
    assert_eq!(matched.at_press, vec![ActionId("rect")]);
    release(&mut state, &registry, &ctrl(), &env);
    press(&mut state, &registry, &shift(), &env);
    assert_eq!(state.gesture_params(&registry, &matched, &env), vec![ActionId("erase"), ActionId("rect")], "rect sampled at press, erase latest");
    release(&mut state, &registry, &shift(), &env);
    assert_eq!(state.gesture_params(&registry, &matched, &env), vec![ActionId("rect")]);
}

#[test]
fn a_more_specific_gesture_chord_beats_target_priority() {
    let registry = registry_with_view(
        vec![gesture("stroke", Scope::Sub(None), "page", vec![Binding::gesture(Chord::default(), lmb(), GestureKind::Drag)], Vec::new())],
        vec![target("page", 10)],
    );
    let (mut state, env) = (MatchState::default(), env());
    let targets = [TargetId("page"), view::SURFACE];
    press(&mut state, &registry, &key(egui::Key::Z), &env);
    press(&mut state, &registry, &lmb(), &env);
    let matched = state.match_gesture(&registry, &lmb(), GestureKind::Drag, &targets, &env).map(|matched| matched.action);
    assert_eq!(matched, Some(view::ZOOM_DRAG), "Z+LMB drag zooms");
    release(&mut state, &registry, &key(egui::Key::Z), &env);
    let matched = state.match_gesture(&registry, &lmb(), GestureKind::Drag, &targets, &env).map(|matched| matched.action);
    assert_eq!(matched, Some(ActionId("stroke")));
}

#[test]
fn a_focused_text_edit_blocks_key_chords_only_and_a_popup_blocks_commands() {
    let registry = registry(vec![
        command("p", Scope::Tab(TAB), vec![single(vec![key(egui::Key::P)])]),
        command("ctrl_only", Scope::Tab(TAB), vec![single(vec![ctrl()])]),
        mode("shift_mode", Scope::Tab(TAB), vec![Binding::mode([shift()])], &[]),
        mode("space_mode", Scope::Tab(TAB), vec![Binding::mode([key(egui::Key::Space)])], &[]),
    ]);
    let typing = ActiveSet { text_edit_focused: true, ..env() };
    let mut state = MatchState::default();
    assert!(press(&mut state, &registry, &key(egui::Key::P), &typing).is_empty());
    release(&mut state, &registry, &key(egui::Key::P), &typing);
    assert_eq!(press(&mut state, &registry, &ctrl(), &typing), vec!["ctrl_only"], "modifier-only chords ignore text focus");
    release(&mut state, &registry, &ctrl(), &typing);
    press(&mut state, &registry, &shift(), &typing);
    press(&mut state, &registry, &key(egui::Key::Space), &typing);
    assert_eq!(state.active_modes(&registry, &typing), BTreeSet::from([ActionId("shift_mode")]), "a key mode stays off while typing");

    let popup = ActiveSet { popup_open: true, ..env() };
    let mut state = MatchState::default();
    assert!(press(&mut state, &registry, &key(egui::Key::P), &popup).is_empty());
}

#[test]
fn only_repeat_commands_refire_on_auto_repeat() {
    let registry = registry(vec![
        command_with("nudge", Scope::Tab(TAB), Vec::new(), vec![single(vec![key(egui::Key::ArrowLeft)])], Vec::new(), true),
        command("toggle", Scope::Tab(TAB), vec![single(vec![key(egui::Key::C)])]),
    ]);
    let (mut state, env) = (MatchState::default(), env());
    assert_eq!(press(&mut state, &registry, &key(egui::Key::ArrowLeft), &env), vec!["nudge"]);
    assert_eq!(repeat(&mut state, &registry, &key(egui::Key::ArrowLeft), &env), vec!["nudge"]);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::C), &env), vec!["toggle"]);
    assert!(repeat(&mut state, &registry, &key(egui::Key::C), &env).is_empty());
}

#[test]
fn axes_match_by_source_chord_and_scope() {
    let registry = registry_with_view(vec![axis("rotate", Scope::Sub(None), vec![Binding::axis(AxisSource::WheelY, Chord::new([ctrl()]))])], Vec::new());
    let (mut state, env) = (MatchState::default(), env());
    assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &env), None, "plain wheel is egui's scrolling");
    assert_eq!(state.match_axis(&registry, AxisSource::Pinch, &env).map(|axis| axis.action), Some(view::ZOOM));
    press(&mut state, &registry, &ctrl(), &env);
    assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &env).map(|axis| axis.action), Some(ActionId("rotate")), "the inner scope shadows Ctrl+wheel");
    release(&mut state, &registry, &ctrl(), &env);
    press(&mut state, &registry, &key(egui::Key::Z), &env);
    assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &env).map(|axis| axis.action), Some(view::ZOOM), "Z+wheel still zooms");
    let off_view = ActiveSet { pointer_on_view: false, ..env };
    assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &off_view), None, "no zoom over a panel");
}

#[test]
fn tab_and_context_conditions_gate_activity_and_reset_clears_everything() {
    let registry = registry(vec![
        command("translation_only", Scope::Tab(TAB), vec![single(vec![key(egui::Key::D)])]),
        command_with("in_edit", Scope::Sub(None), vec![crate::Condition::Context(ContextId("edit"))], vec![single(vec![key(egui::Key::N)])], Vec::new(), false),
    ]);
    let other_tab = ActiveSet { active_tab: Some(ms_config::app_tab::AppTab::Cleaning), ..env() };
    let mut state = MatchState::default();
    assert!(press(&mut state, &registry, &key(egui::Key::D), &other_tab).is_empty());
    assert!(press(&mut state, &registry, &key(egui::Key::N), &env()).is_empty());
    let mut editing = env();
    editing.contexts.insert(ContextId("edit"));
    release(&mut state, &registry, &key(egui::Key::N), &editing);
    assert_eq!(press(&mut state, &registry, &key(egui::Key::N), &editing), vec!["in_edit"]);
    assert!(state.step(&registry, &MatchEvent::Reset, &editing).is_empty());
    assert!(state.held().is_empty());
}
