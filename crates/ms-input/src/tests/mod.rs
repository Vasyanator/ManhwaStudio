/*
File: crates/ms-input/src/tests/mod.rs

Purpose:
Unit tests of the `ms-input` model, split per contract, plus the small declaration builders they
share.

Submodules:
- `matching_tests`: the matcher contract (exactness, completing press, suppression, sequences,
  specificity, modes, sampling, gestures, axes, text-edit / popup rules).
- `conflict_tests`: conflicts, shadows, reserved clipboard chords, co-activity.
- `registry_tests`: declaration validation, set / reset / overrides.
- `label_tests`: the label format.
- `persist_tests`: the codec, the v1 migration and the writers (temp-dir documents only).
- `runtime_tests`: the egui runtime (router frames, claims, axes, capture) over headless
  `Context::run_ui` frames.
*/

use ms_config::app_tab::AppTab;

use crate::{
    Accept, ActionId, ActionMeta, ActionSpec, AxisSpec, Binding, Button, CommandSpec, Condition, Declarations, GestureSpec, InputRegistry,
    ModeEffect, ModeSpec, ModifierKey, MouseButton, ReleasePolicy, Scope, TargetId, TargetSpec,
};

mod conflict_tests;
mod label_tests;
mod matching_tests;
mod persist_tests;
mod registry_tests;
mod runtime_tests;

/// Any existing catalog key; test actions need a title.
const TITLE: &str = "input.view.zoom_title";
const SECTION: &str = "input.section.view";

fn key(key: egui::Key) -> Button {
    Button::Key(key)
}

fn shift() -> Button {
    Button::Modifier(ModifierKey::Shift)
}

fn ctrl() -> Button {
    Button::Modifier(ModifierKey::Command)
}

fn lmb() -> Button {
    Button::Mouse(MouseButton::Left)
}

fn meta(id: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id: ActionId(id), title_key: TITLE, section_key: SECTION, scope, when, defaults }
}

fn command(id: &'static str, scope: Scope, defaults: Vec<Binding>) -> ActionSpec {
    command_with(id, scope, Vec::new(), defaults, Vec::new(), false)
}

fn command_with(id: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>, accepts: Vec<Accept>, repeat: bool) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta: meta(id, scope, when, defaults), accepts, repeat })
}

fn mode(id: &'static str, scope: Scope, defaults: Vec<Binding>, beats: &[&'static str]) -> ActionSpec {
    mode_with(id, scope, Vec::new(), defaults, beats)
}

fn mode_with(id: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>, beats: &[&'static str]) -> ActionSpec {
    ActionSpec::Mode(ModeSpec {
        meta: meta(id, scope, when, defaults),
        effect: ModeEffect::Immediate,
        on_release_mid_gesture: ReleasePolicy::Commit,
        beats: beats.iter().map(|id| ActionId(id)).collect(),
        param: None,
    })
}

fn axis(id: &'static str, scope: Scope, defaults: Vec<Binding>) -> ActionSpec {
    ActionSpec::Axis(AxisSpec { meta: meta(id, scope, Vec::new(), defaults) })
}

fn gesture(id: &'static str, scope: Scope, target: &'static str, defaults: Vec<Binding>, accepts: Vec<Accept>) -> ActionSpec {
    ActionSpec::Gesture(GestureSpec { meta: meta(id, scope, Vec::new(), defaults), target: TargetId(target), accepts })
}

fn target(id: &'static str, priority: u16) -> TargetSpec {
    TargetSpec { id: TargetId(id), priority, canvas_wide: false }
}

/// A registry over `actions` and `targets` plus the shared `view.*` set.
fn registry_with_view(actions: Vec<ActionSpec>, targets: Vec<TargetSpec>) -> InputRegistry {
    let mut decls = crate::view::declarations();
    decls.extend(Declarations { actions, targets, fixed: Vec::new(), exclusive_contexts: Vec::new() });
    match InputRegistry::new(decls) {
        Ok(registry) => registry,
        Err(err) => panic!("test declarations must be valid: {err}"),
    }
}

/// A registry over exactly `actions` (no view set).
fn registry(actions: Vec<ActionSpec>) -> InputRegistry {
    match InputRegistry::new(Declarations { actions, ..Declarations::default() }) {
        Ok(registry) => registry,
        Err(err) => panic!("test declarations must be valid: {err}"),
    }
}

/// Installs the embedded English catalog (the default active catalog is empty).
fn english() {
    if let Err(err) = ms_i18n::set_locale(&ms_i18n::LocaleTag::english()) {
        panic!("embedded English catalog must load: {err}");
    }
}

/// The tab used where a test needs one.
const TAB: AppTab = AppTab::Translation;
