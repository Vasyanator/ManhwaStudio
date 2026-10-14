//! The `Hotkeys` codec, the v1 migration and the writers (`persist.rs`). Every document is
//! in-memory JSON or a uniquely named file under `std::env::temp_dir()`, removed by the test.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{TAB, command, ctrl, key, lmb, mode, registry_with_view, shift};
use crate::persist::{decode_binding, encode_binding};
use crate::registry::OverrideEntry;
use crate::{
    ActionId, ActionKind, AxisBinding, AxisSource, Binding, Button, Chord, GestureKind, HOTKEYS_SECTION, HotkeyOverrides, MouseButton, PersistError,
    Scope, TouchSource, load_overrides, view, write_migrated_section, write_override,
};

fn resolve(name: &str) -> Option<ActionId> {
    [view::ZOOM_MODIFIER, view::PAN].into_iter().find(|id| id.0 == name)
}

fn round_trip(binding: &Binding) {
    let encoded = encode_binding(binding);
    let decoded = decode_binding(&encoded, binding.kind(), &resolve);
    assert_eq!(decoded.as_ref(), Ok(binding), "round trip of {encoded}");
}

#[test]
fn every_binding_variant_round_trips() {
    round_trip(&Binding::sequence(vec![
        Chord::new([ctrl(), key(egui::Key::K)]).with_modes([view::ZOOM_MODIFIER, view::PAN]),
        Chord::new([key(egui::Key::Num0), Button::Modifier(crate::ModifierKey::Super), Button::Modifier(crate::ModifierKey::Alt)]),
    ]));
    round_trip(&Binding::mode([shift(), key(egui::Key::Q)]));
    round_trip(&Binding::Axis(AxisBinding { source: AxisSource::DragX, chord: Chord::new([shift()]).with_modes([view::ZOOM_MODIFIER]), sensitivity: 0.1, invert: true }));
    round_trip(&Binding::axis(AxisSource::Pinch, Chord::default()));
    round_trip(&Binding::gesture(Chord::new([Button::Unknown { device: "pen".to_owned(), code: 7 }]), Button::Mouse(MouseButton::Extra(2)), GestureKind::DoubleClick));
    round_trip(&Binding::gesture(Chord::default(), Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag));
    round_trip(&Binding::gesture(Chord::default(), Button::Unknown { device: "tablet:pad".to_owned(), code: 0 }, GestureKind::LongPress));
    round_trip(&Binding::gesture(Chord::default(), lmb(), GestureKind::Click));
    for source in [TouchSource::LongPress, TouchSource::TwoFingerPinch, TouchSource::TwoFingerPan, TouchSource::OneFingerPan] {
        round_trip(&Binding::gesture(Chord::default(), Button::Touch(source), GestureKind::Drag));
    }
    for mouse in [MouseButton::Right, MouseButton::Middle] {
        round_trip(&Binding::gesture(Chord::default(), Button::Mouse(mouse), GestureKind::Drag));
    }
}

#[test]
fn every_egui_key_name_round_trips() {
    for key in egui::Key::ALL {
        round_trip(&Binding::mode([Button::Key(*key)]));
    }
}

#[test]
fn malformed_alternatives_are_decode_errors() {
    let decode = |value: Value, kind: ActionKind| decode_binding(&value, kind, &resolve);
    assert!(decode(json!("x"), ActionKind::Command).is_err());
    assert!(decode(json!({"sequence": [{"buttons": ["key:NoSuchKey"]}]}), ActionKind::Command).is_err());
    assert!(decode(json!({"sequence": [{"modes": ["ghost"], "buttons": []}]}), ActionKind::Command).is_err());
    assert!(decode(json!({"buttons": ["pen:tip"]}), ActionKind::Mode).is_err());
    assert!(decode(json!({"modes": ["view.pan"], "buttons": ["key:A"]}), ActionKind::Mode).is_err());
    assert!(decode(json!({"source": "wheel_z", "chord": {"buttons": []}}), ActionKind::Axis).is_err());
    assert!(decode(json!({"source": "wheel_y", "chord": {"buttons": []}, "sensitivity": "big"}), ActionKind::Axis).is_err());
    assert!(decode(json!({"chord": {"buttons": []}, "button": "mouse:left", "kind": "swipe"}), ActionKind::Gesture).is_err());
}

#[test]
fn format_two_is_read_and_unknown_formats_are_left_alone() {
    let root = json!({ HOTKEYS_SECTION: { "format": 2, "actions": { "a": [], "b": "not a list" } } });
    let loaded = load_overrides(&root);
    assert_eq!(loaded.migrated_section, None);
    assert_eq!(loaded.overrides.actions.keys().collect::<Vec<_>>(), vec!["a"], "a non-list entry is dropped, never the section");

    let newer = load_overrides(&json!({ HOTKEYS_SECTION: { "format": 3, "actions": { "a": [] } } }));
    assert_eq!(newer, crate::LoadedOverrides::default());
    assert_eq!(load_overrides(&json!({ HOTKEYS_SECTION: {} })), crate::LoadedOverrides::default(), "the default empty section needs no migration");
    assert_eq!(load_overrides(&json!({ "General": {} })), crate::LoadedOverrides::default());
    assert_eq!(load_overrides(&Value::Null), crate::LoadedOverrides::default());
}

/// A v1 section in the exact shape the removed `InputManagerV2` wrote (`binding_to_json`).
fn v1_shortcut(key: &str, set: &[&str]) -> Value {
    let flag = |name: &str| set.contains(&name);
    json!({
        "enabled": true,
        "shortcut": { "key": key, "modifiers": { "alt": flag("alt"), "ctrl": flag("ctrl"), "shift": flag("shift"), "command": flag("command") } },
        "modifier_only": null
    })
}

#[test]
fn v1_sections_migrate_to_format_two() {
    let section = json!({
        // Equal to the v1 default: dropped, the action follows the code defaults.
        "translation.canvas.zoom_in": v1_shortcut("=", &["command"]),
        "translation.panel.bubbles.toggle": v1_shortcut("P", &[]),
        // Collapsing zoom ids: the first non-default one in table order wins.
        "cleaning.canvas.zoom_in": v1_shortcut("Plus", &["ctrl"]),
        "translation.canvas.zoom_out": v1_shortcut("O", &["command"]),
        "cleaning.canvas.zoom_out": v1_shortcut("I", &["command"]),
        // Modes.
        "translation.ocr.quick.selection_mode": { "enabled": true, "shortcut": null, "modifier_only": "alt" },
        "translation.ocr.advanced.selection_mode": v1_shortcut("Q", &["ctrl"]),
        // Disabled, and a modifier-only command (dead in v1, live now).
        "translation.panel.ocr.toggle": { "enabled": false, "shortcut": null, "modifier_only": null },
        "translation.panel.detector.toggle": { "enabled": true, "shortcut": null, "modifier_only": "ctrl" },
        // `mac_cmd` reads as the command role; an id unknown to the v1 table migrates 1:1.
        "plugin.custom": { "shortcut": { "key": "F5", "modifiers": { "mac_cmd": true, "shift": true } } },
        // An entry the old reader rejected (not an object) is dropped; an unknown key name read as
        // "no shortcut" there, so it migrates as explicitly unbound.
        "translation.bubble.create_at_cursor": "not a binding",
        "translation.bubble.create_hint_at_cursor": v1_shortcut("NoSuchKey", &[]),
    });
    let loaded = load_overrides(&json!({ HOTKEYS_SECTION: section }));
    let expected = json!({
        "format": 2,
        "actions": {
            "view.zoom_in": [{"sequence": [{"buttons": ["mod:command", "key:Plus"]}]}],
            "view.zoom_out": [{"sequence": [{"buttons": ["mod:command", "key:O"]}]}],
            "translation.ocr.quick_mode": [{"buttons": ["mod:alt"]}],
            "translation.ocr.advanced_mode": [{"buttons": ["mod:command", "key:Q"]}],
            "translation.panel.ocr.toggle": [],
            "translation.panel.detector.toggle": [{"sequence": [{"buttons": ["mod:command"]}]}],
            "plugin.custom": [{"sequence": [{"buttons": ["mod:command", "mod:shift", "key:F5"]}]}],
            "translation.bubble.create_hint_at_cursor": [],
        }
    });
    assert_eq!(loaded.migrated_section.as_ref(), Some(&expected));
    assert_eq!(loaded.overrides.to_section(), expected);

    // The migrated overrides decode against the real declarations.
    let mut registry = registry_with_view(
        vec![
            mode("translation.ocr.quick_mode", Scope::Tab(TAB), vec![Binding::mode([shift()])], &[]),
            mode("translation.ocr.advanced_mode", Scope::Tab(TAB), vec![Binding::mode([Button::Modifier(crate::ModifierKey::Alt)])], &[]),
            command("translation.panel.ocr.toggle", Scope::Tab(TAB), vec![Binding::command(Chord::new([key(egui::Key::S)]))]),
            command("translation.panel.detector.toggle", Scope::Tab(TAB), vec![Binding::command(Chord::new([key(egui::Key::D)]))]),
        ],
        Vec::new(),
    );
    let warnings = registry.apply_overrides(&loaded.overrides);
    assert_eq!(
        warnings,
        vec![
            crate::OverrideWarning::UnknownAction("plugin.custom".to_owned()),
            crate::OverrideWarning::UnknownAction("translation.bubble.create_hint_at_cursor".to_owned()),
        ]
    );
    assert_eq!(registry.bindings(view::ZOOM_IN), &[Binding::command(Chord::new([ctrl(), key(egui::Key::Plus)]))]);
    assert_eq!(registry.bindings(ActionId("translation.ocr.quick_mode")), &[Binding::mode([Button::Modifier(crate::ModifierKey::Alt)])]);
    assert!(registry.bindings(ActionId("translation.panel.ocr.toggle")).is_empty());
}

#[test]
fn v1_damaged_fields_read_exactly_as_the_legacy_decoder_read_them() {
    let section = json!({
        // Disabled wins over any damage in the other fields.
        "translation.panel.machine_translation.toggle": { "enabled": false, "shortcut": "damaged", "modifier_only": 42 },
        // A non-boolean `enabled` reads as true.
        "translation.panel.composition.toggle": { "enabled": "yes", "shortcut": { "key": "X" } },
        // An unknown modifier-only token is ignored; a non-boolean modifier flag reads as false.
        "translation.panel.bubbles.toggle": { "modifier_only": "hyper", "shortcut": { "key": "Y", "modifiers": { "shift": "on", "alt": true } } },
        // A damaged shortcut reads as absent: nothing bound.
        "translation.panel.ocr.toggle": { "shortcut": { "modifiers": {} } },
        // Ignored token + the v1 default shortcut = the default: dropped.
        "translation.panel.detector.toggle": { "enabled": true, "shortcut": { "key": "D" }, "modifier_only": "bogus" },
    });
    let loaded = load_overrides(&json!({ HOTKEYS_SECTION: section }));
    let expected = json!({
        "format": 2,
        "actions": {
            "translation.panel.machine_translation.toggle": [],
            "translation.panel.composition.toggle": [{"sequence": [{"buttons": ["key:X"]}]}],
            "translation.panel.bubbles.toggle": [{"sequence": [{"buttons": ["mod:alt", "key:Y"]}]}],
            "translation.panel.ocr.toggle": [],
        }
    });
    assert_eq!(loaded.migrated_section, Some(expected));
}

/// A unique scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir().join(format!("ms-input-test-{tag}-{}-{nanos}", std::process::id()));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            panic!("cannot create scratch dir {}: {err}", dir.display());
        }
        Self(dir)
    }

    fn config(&self) -> PathBuf {
        self.0.join("user_config.json")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove scratch dir {}: {err}", self.0.display());
        }
    }
}

fn read_section(path: &std::path::Path) -> Option<Value> {
    let doc = ms_docstore::DocRef::new(path, ms_docstore::DocKind::UserConfig);
    match ms_docstore::read_value(&doc) {
        Ok(root) => root.and_then(|root| root.get(HOTKEYS_SECTION).cloned()),
        Err(err) => panic!("scratch user config must read back: {err}"),
    }
}

#[test]
fn writers_update_only_the_hotkeys_section_of_a_scratch_config() {
    let scratch = ScratchDir::new("writers");
    let path = scratch.config();
    let entry = OverrideEntry { id: ActionId("a"), alternatives: Some(vec![json!({"sequence": [{"buttons": ["key:A"]}]})]) };
    assert!(write_override(&path, &entry).is_ok());
    assert_eq!(read_section(&path), Some(json!({"format": 2, "actions": {"a": [{"sequence": [{"buttons": ["key:A"]}]}]}})));

    // The migration write is a no-op once the section has format 2.
    assert!(write_migrated_section(&path, json!({"format": 2, "actions": {}})).is_ok());
    assert!(read_section(&path).is_some_and(|section| section.pointer("/actions/a").is_some()));

    assert!(write_override(&path, &OverrideEntry { id: ActionId("a"), alternatives: None }).is_ok());
    assert_eq!(read_section(&path), Some(HotkeyOverrides::default().to_section()));
}

#[test]
fn writers_migrate_a_v1_section_first_and_never_touch_a_newer_format() {
    let scratch = ScratchDir::new("v1");
    let path = scratch.config();
    let seed = |section: Value| {
        let outcome = ms_config::update_user_config_file(&path, |root| {
            if let Some(root) = root.as_object_mut() {
                root.insert(HOTKEYS_SECTION.to_owned(), section);
            }
            Ok(())
        });
        assert!(outcome.is_ok(), "seeding the scratch config failed: {outcome:?}");
    };
    seed(json!({ "translation.panel.ocr.toggle": { "enabled": false } }));
    let entry = OverrideEntry { id: ActionId("view.pan"), alternatives: Some(Vec::new()) };
    assert!(write_override(&path, &entry).is_ok());
    assert_eq!(read_section(&path), Some(json!({"format": 2, "actions": {"translation.panel.ocr.toggle": [], "view.pan": []}})), "the v1 override survives");

    seed(json!({ "format": 3, "actions": { "x": [] } }));
    assert!(matches!(write_override(&path, &entry), Err(PersistError::NewerFormat { .. })));
    assert!(matches!(write_migrated_section(&path, json!({})), Err(PersistError::NewerFormat { .. })));
    assert_eq!(read_section(&path), Some(json!({ "format": 3, "actions": { "x": [] } })));
}
