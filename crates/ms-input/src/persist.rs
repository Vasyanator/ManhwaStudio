/*
File: crates/ms-input/src/persist.rs

Purpose:
The `Hotkeys` section of `user_config`: the format-2 JSON codec of binding alternatives, the pure
v1 -> v2 migration, and the two writers that update the section through the serialized
user-config read-modify-write.

Key structures:
- `HotkeyOverrides`: the overridden actions with their RAW encoded alternative lists (decoded by
  `InputRegistry::apply_overrides`, which knows each action's kind and the declared modes).
- `LoadedOverrides`: the overrides plus the migrated section to write back once (v1 input only).
- `DecodeError`, `PersistError`.

Key functions:
- `load_overrides`: pure; detects the format, migrates v1.
- `encode_binding` / `decode_binding`: one alternative.
- `write_override`, `write_migrated_section`: blocking document writes (worker threads only).

On-disk format 2:
  "Hotkeys": { "format": 2, "actions": { "<action id>": [ <alternative>, ... ] } }
Only overridden actions are stored, each with its FULL alternative list (`[]` = explicitly
unbound); a reset deletes the id. Alternatives per kind:
  command: {"sequence": [chord, ...]}       mode:    {"buttons": [button, ...]}
  axis:    {"source": "wheel_y", "chord": chord, "sensitivity": 1.0, "invert": false}
  gesture: {"chord": chord, "button": button, "kind": "drag"}
  chord:   {"modes": ["<mode id>", ...], "buttons": [button, ...]}  ("modes" omitted when empty)
  button:  "mod:command|shift|alt|super", "key:<egui Key::name>", "mouse:left|right|middle",
           "mouse:extra:<n>", "touch:<source>", or {"device": "<name>", "code": <n>}.
Version 1 (no "format"): `{id: {"enabled", "shortcut": {"key", "modifiers"}, "modifier_only"}}`
written by the removed `InputManagerV2`; migrated by `migrate_v1`.
*/

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::action::{ActionId, ActionKind};
use crate::binding::{AxisBinding, AxisSource, Binding, ButtonChord, Chord, CommandBinding, GestureBinding, GestureKind};
use crate::button::{Button, ModifierKey, MouseButton, TouchSource};
use crate::registry::OverrideEntry;

/// The `user_config` section name.
pub const HOTKEYS_SECTION: &str = "Hotkeys";
/// The section format this build reads and writes.
pub const HOTKEYS_FORMAT: u64 = 2;

const LOG_PREFIX: &str = "[ms-input::persist]";

/// The persisted overrides: action id -> its full encoded alternative list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HotkeyOverrides {
    pub actions: BTreeMap<String, Vec<Value>>,
}

impl HotkeyOverrides {
    /// Applies one change (insert / replace the id, or delete it).
    pub fn apply(&mut self, entry: &OverrideEntry) {
        match &entry.alternatives {
            Some(alternatives) => {
                self.actions.insert(entry.id.0.to_owned(), alternatives.clone());
            }
            None => {
                self.actions.remove(entry.id.0);
            }
        }
    }

    /// The format-2 section value holding these overrides.
    #[must_use]
    pub fn to_section(&self) -> Value {
        let actions: Map<String, Value> = self.actions.iter().map(|(id, alternatives)| (id.clone(), Value::Array(alternatives.clone()))).collect();
        json!({ "format": HOTKEYS_FORMAT, "actions": actions })
    }
}

/// What [`load_overrides`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedOverrides {
    pub overrides: HotkeyOverrides,
    /// The format-2 section to write back once (`write_migrated_section`), present only when the
    /// stored section was version 1.
    pub migrated_section: Option<Value>,
}

/// Why one encoded alternative does not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("expected a JSON object")]
    NotAnObject,
    #[error("missing or malformed field `{0}`")]
    Field(&'static str),
    #[error("unknown button `{0}`")]
    Button(String),
    #[error("unknown axis source `{0}`")]
    AxisSource(String),
    #[error("unknown gesture kind `{0}`")]
    GestureKind(String),
    #[error("a mode alternative cannot reference modes")]
    ModeRefsInMode,
    #[error("unknown mode reference `{0}`")]
    UnknownModeRef(String),
}

/// Why a section write failed. Nothing was written.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("the Hotkeys section in {path} has format {format}, newer than this build understands; it is left untouched")]
    NewerFormat { path: PathBuf, format: String },
    #[error("failed to update the Hotkeys section in {path}: {message}")]
    UserConfig { path: PathBuf, message: String },
}

// ----------------------------------------------------------------------------------------------
// Button tokens

fn encode_button(button: &Button) -> Value {
    let token = match button {
        Button::Modifier(modifier) => match modifier {
            ModifierKey::Command => "mod:command".to_owned(),
            ModifierKey::Shift => "mod:shift".to_owned(),
            ModifierKey::Alt => "mod:alt".to_owned(),
            ModifierKey::Super => "mod:super".to_owned(),
        },
        Button::Key(key) => format!("key:{}", key.name()),
        Button::Mouse(mouse) => match mouse {
            MouseButton::Left => "mouse:left".to_owned(),
            MouseButton::Right => "mouse:right".to_owned(),
            MouseButton::Middle => "mouse:middle".to_owned(),
            MouseButton::Extra(number) => format!("mouse:extra:{number}"),
        },
        Button::Touch(source) => format!("touch:{}", touch_token(*source)),
        Button::Unknown { device, code } => return json!({ "device": device, "code": code }),
    };
    Value::String(token)
}

fn touch_token(source: TouchSource) -> &'static str {
    match source {
        TouchSource::LongPress => "long_press",
        TouchSource::LongPressDrag => "long_press_drag",
        TouchSource::TwoFingerPinch => "two_finger_pinch",
        TouchSource::TwoFingerPan => "two_finger_pan",
        TouchSource::OneFingerPan => "one_finger_pan",
    }
}

const TOUCH_SOURCES: [TouchSource; 5] = [TouchSource::LongPress, TouchSource::LongPressDrag, TouchSource::TwoFingerPinch, TouchSource::TwoFingerPan, TouchSource::OneFingerPan];

fn decode_button(value: &Value) -> Result<Button, DecodeError> {
    if let Some(object) = value.as_object() {
        let device = object.get("device").and_then(Value::as_str).ok_or(DecodeError::Field("device"))?;
        let code = object
            .get("code")
            .and_then(Value::as_u64)
            .and_then(|code| u32::try_from(code).ok())
            .ok_or(DecodeError::Field("code"))?;
        return Ok(Button::Unknown { device: device.to_owned(), code });
    }
    let token = value.as_str().ok_or_else(|| DecodeError::Button(value.to_string()))?;
    let unknown = || DecodeError::Button(token.to_owned());
    let (family, name) = token.split_once(':').ok_or_else(unknown)?;
    match family {
        "mod" => match name {
            "command" => Ok(Button::Modifier(ModifierKey::Command)),
            "shift" => Ok(Button::Modifier(ModifierKey::Shift)),
            "alt" => Ok(Button::Modifier(ModifierKey::Alt)),
            "super" => Ok(Button::Modifier(ModifierKey::Super)),
            _ => Err(unknown()),
        },
        "key" => egui::Key::from_name(name).map(Button::Key).ok_or_else(unknown),
        "mouse" => match name {
            "left" => Ok(Button::Mouse(MouseButton::Left)),
            "right" => Ok(Button::Mouse(MouseButton::Right)),
            "middle" => Ok(Button::Mouse(MouseButton::Middle)),
            other => other
                .strip_prefix("extra:")
                .and_then(|number| number.parse::<u8>().ok())
                .map(|number| Button::Mouse(MouseButton::Extra(number)))
                .ok_or_else(unknown),
        },
        "touch" => TOUCH_SOURCES.iter().find(|source| touch_token(**source) == name).map(|source| Button::Touch(*source)).ok_or_else(unknown),
        _ => Err(unknown()),
    }
}

fn encode_buttons(buttons: &BTreeSet<Button>) -> Value {
    Value::Array(buttons.iter().map(encode_button).collect())
}

fn decode_buttons(value: Option<&Value>) -> Result<BTreeSet<Button>, DecodeError> {
    value.and_then(Value::as_array).ok_or(DecodeError::Field("buttons"))?.iter().map(decode_button).collect()
}

// ----------------------------------------------------------------------------------------------
// Alternatives

fn encode_chord(chord: &Chord) -> Value {
    let mut object = Map::new();
    if !chord.modes.is_empty() {
        object.insert("modes".to_owned(), Value::Array(chord.modes.iter().map(|mode| Value::String(mode.0.to_owned())).collect()));
    }
    object.insert("buttons".to_owned(), encode_buttons(&chord.buttons));
    Value::Object(object)
}

fn decode_chord(value: Option<&Value>, resolve: &dyn Fn(&str) -> Option<ActionId>) -> Result<Chord, DecodeError> {
    let object = value.and_then(Value::as_object).ok_or(DecodeError::Field("chord"))?;
    let modes = match object.get("modes") {
        None => BTreeSet::new(),
        Some(raw) => raw
            .as_array()
            .ok_or(DecodeError::Field("modes"))?
            .iter()
            .map(|mode| {
                let name = mode.as_str().ok_or(DecodeError::Field("modes"))?;
                resolve(name).ok_or_else(|| DecodeError::UnknownModeRef(name.to_owned()))
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(Chord { modes, buttons: decode_buttons(object.get("buttons"))? })
}

fn axis_source_token(source: AxisSource) -> &'static str {
    match source {
        AxisSource::WheelY => "wheel_y",
        AxisSource::WheelX => "wheel_x",
        AxisSource::Pinch => "pinch",
        AxisSource::DragX => "drag_x",
        AxisSource::DragY => "drag_y",
    }
}

const AXIS_SOURCES: [AxisSource; 5] = [AxisSource::WheelY, AxisSource::WheelX, AxisSource::Pinch, AxisSource::DragX, AxisSource::DragY];

fn gesture_kind_token(kind: GestureKind) -> &'static str {
    match kind {
        GestureKind::Click => "click",
        GestureKind::DoubleClick => "double_click",
        GestureKind::Drag => "drag",
        GestureKind::LongPress => "long_press",
    }
}

const GESTURE_KINDS: [GestureKind; 4] = [GestureKind::Click, GestureKind::DoubleClick, GestureKind::Drag, GestureKind::LongPress];

/// Encodes one alternative in the format-2 shape of its kind.
#[must_use]
pub fn encode_binding(binding: &Binding) -> Value {
    match binding {
        Binding::Command(command) => json!({ "sequence": command.sequence.iter().map(encode_chord).collect::<Vec<_>>() }),
        Binding::Mode(chord) => json!({ "buttons": encode_buttons(&chord.buttons) }),
        Binding::Axis(axis) => json!({
            "source": axis_source_token(axis.source),
            "chord": encode_chord(&axis.chord),
            "sensitivity": f64::from(axis.sensitivity),
            "invert": axis.invert,
        }),
        Binding::Gesture(gesture) => json!({
            "chord": encode_chord(&gesture.chord),
            "button": encode_button(&gesture.button),
            "kind": gesture_kind_token(gesture.kind),
        }),
    }
}

/// Decodes one alternative of an action of `kind`. `resolve` turns a referenced mode id into
/// the registered [`ActionId`]. Only the shape is checked here; kind-of-reference and other
/// semantic checks are the registry's validation.
///
/// # Errors
/// [`DecodeError`] for a wrong shape, an unknown button / source / kind token, an unknown mode
/// reference, or mode references inside a mode alternative.
pub fn decode_binding(value: &Value, kind: ActionKind, resolve: &dyn Fn(&str) -> Option<ActionId>) -> Result<Binding, DecodeError> {
    let object = value.as_object().ok_or(DecodeError::NotAnObject)?;
    match kind {
        ActionKind::Command => {
            let sequence = object
                .get("sequence")
                .and_then(Value::as_array)
                .ok_or(DecodeError::Field("sequence"))?
                .iter()
                .map(|chord| decode_chord(Some(chord), resolve))
                .collect::<Result<_, _>>()?;
            Ok(Binding::Command(CommandBinding { sequence }))
        }
        ActionKind::Mode => {
            if object.get("modes").and_then(Value::as_array).is_some_and(|modes| !modes.is_empty()) {
                return Err(DecodeError::ModeRefsInMode);
            }
            Ok(Binding::Mode(ButtonChord { buttons: decode_buttons(object.get("buttons"))? }))
        }
        ActionKind::Axis => {
            let token = object.get("source").and_then(Value::as_str).ok_or(DecodeError::Field("source"))?;
            let source = AXIS_SOURCES.iter().copied().find(|source| axis_source_token(*source) == token).ok_or_else(|| DecodeError::AxisSource(token.to_owned()))?;
            let sensitivity = match object.get("sensitivity") {
                None => 1.0,
                Some(raw) => raw.as_f64().and_then(f32_from_f64).ok_or(DecodeError::Field("sensitivity"))?,
            };
            let invert = match object.get("invert") {
                None => false,
                Some(raw) => raw.as_bool().ok_or(DecodeError::Field("invert"))?,
            };
            Ok(Binding::Axis(AxisBinding { source, chord: decode_chord(object.get("chord"), resolve)?, sensitivity, invert }))
        }
        ActionKind::Gesture => {
            let token = object.get("kind").and_then(Value::as_str).ok_or(DecodeError::Field("kind"))?;
            let gesture_kind = GESTURE_KINDS.iter().copied().find(|kind| gesture_kind_token(*kind) == token).ok_or_else(|| DecodeError::GestureKind(token.to_owned()))?;
            let button = decode_button(object.get("button").ok_or(DecodeError::Field("button"))?)?;
            Ok(Binding::Gesture(GestureBinding { chord: decode_chord(object.get("chord"), resolve)?, button, kind: gesture_kind }))
        }
    }
}

/// `value` as `f32` when it is finite and inside the `f32` range.
fn f32_from_f64(value: f64) -> Option<f32> {
    if !value.is_finite() || value.abs() > f64::from(f32::MAX) {
        return None;
    }
    // In range and finite (checked above): the cast only rounds to the nearest f32, which is the
    // exact inverse of the `f64::from(f32)` the encoder wrote.
    #[expect(clippy::cast_possible_truncation, reason = "range checked above; rounding to the nearest f32 is the intended inverse of f64::from(f32)")]
    let narrowed = value as f32;
    Some(narrowed)
}

// ----------------------------------------------------------------------------------------------
// Loading and v1 migration

/// Reads the `Hotkeys` section of a loaded `user_config` root. Pure apart from logging.
///
/// Format 2 is read as is (an id whose value is not an array is dropped with a log line). A
/// section without `format` that is a non-empty object is version 1 and is migrated
/// (`migrated_section` is then set). An absent / non-object / empty section yields no
/// overrides. A section of an unknown or newer format yields no overrides and is never
/// rewritten.
#[must_use]
pub fn load_overrides(root: &Value) -> LoadedOverrides {
    let Some(section) = root.get(HOTKEYS_SECTION).and_then(Value::as_object) else {
        return LoadedOverrides::default();
    };
    match section.get("format") {
        Some(format) if format.as_u64() == Some(HOTKEYS_FORMAT) => LoadedOverrides { overrides: read_v2(section), migrated_section: None },
        Some(format) => {
            ms_log::runtime_log::log_warn(format!(
                "{LOG_PREFIX} Hotkeys section has unsupported format {format}; using default bindings and leaving the section untouched"
            ));
            LoadedOverrides::default()
        }
        None if section.is_empty() => LoadedOverrides::default(),
        None => {
            let overrides = migrate_v1(section);
            ms_log::runtime_log::log_info(format!(
                "{LOG_PREFIX} migrated Hotkeys section v1 -> v2: {} v1 entries, {} overrides kept",
                section.len(),
                overrides.actions.len()
            ));
            let migrated_section = Some(overrides.to_section());
            LoadedOverrides { overrides, migrated_section }
        }
    }
}

fn read_v2(section: &Map<String, Value>) -> HotkeyOverrides {
    let Some(actions) = section.get("actions").and_then(Value::as_object) else {
        if section.contains_key("actions") {
            ms_log::runtime_log::log_warn(format!("{LOG_PREFIX} Hotkeys.actions is not an object; using default bindings"));
        }
        return HotkeyOverrides::default();
    };
    let mut overrides = HotkeyOverrides::default();
    for (id, alternatives) in actions {
        match alternatives.as_array() {
            Some(list) => {
                overrides.actions.insert(id.clone(), list.clone());
            }
            None => ms_log::runtime_log::log_warn(format!("{LOG_PREFIX} Hotkeys override `{id}` is not a list; dropped (defaults apply)")),
        }
    }
    overrides
}

/// A version-1 binding, normalized for comparison (`ctrl`, `command` and `mac_cmd` are one
/// `Command` role).
#[derive(Debug, Clone, PartialEq, Eq)]
enum V1Binding {
    Unbound,
    Shortcut { key: egui::Key, modifiers: BTreeSet<ModifierKey> },
    ModifierOnly(ModifierKey),
}

/// How a version-1 id maps onto format 2.
struct V1Mapping {
    old: &'static str,
    new: &'static str,
    kind: ActionKind,
    default: fn() -> V1Binding,
}

fn v1_shortcut(key: egui::Key, modifiers: &[ModifierKey]) -> V1Binding {
    V1Binding::Shortcut { key, modifiers: modifiers.iter().copied().collect() }
}

/// Every id `InputManagerV2` ever declared, with its v1 default (frozen history: these defaults
/// never change). Zoom ids of both tabs collapse into `view.*`, the earlier row winning.
const V1_MAPPINGS: &[V1Mapping] = &[
    V1Mapping { old: "translation.ocr.quick.selection_mode", new: "translation.ocr.quick_mode", kind: ActionKind::Mode, default: || V1Binding::ModifierOnly(ModifierKey::Shift) },
    V1Mapping { old: "translation.ocr.advanced.selection_mode", new: "translation.ocr.advanced_mode", kind: ActionKind::Mode, default: || V1Binding::ModifierOnly(ModifierKey::Alt) },
    V1Mapping { old: "translation.panel.bubbles.toggle", new: "translation.panel.bubbles.toggle", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::P, &[]) },
    V1Mapping { old: "translation.panel.ocr.toggle", new: "translation.panel.ocr.toggle", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::S, &[]) },
    V1Mapping { old: "translation.panel.composition.toggle", new: "translation.panel.composition.toggle", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::K, &[]) },
    V1Mapping { old: "translation.panel.machine_translation.toggle", new: "translation.panel.machine_translation.toggle", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::M, &[]) },
    V1Mapping { old: "translation.panel.detector.toggle", new: "translation.panel.detector.toggle", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::D, &[]) },
    V1Mapping { old: "translation.bubble.copy_original", new: "translation.bubble.copy_original", kind: ActionKind::Command, default: || V1Binding::Unbound },
    V1Mapping { old: "translation.bubble.copy_translation", new: "translation.bubble.copy_translation", kind: ActionKind::Command, default: || V1Binding::Unbound },
    V1Mapping { old: "translation.bubble.paste_original", new: "translation.bubble.paste_original", kind: ActionKind::Command, default: || V1Binding::Unbound },
    V1Mapping { old: "translation.bubble.paste_translation", new: "translation.bubble.paste_translation", kind: ActionKind::Command, default: || V1Binding::Unbound },
    V1Mapping { old: "translation.canvas.zoom_in", new: "view.zoom_in", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Equals, &[ModifierKey::Command]) },
    V1Mapping { old: "cleaning.canvas.zoom_in", new: "view.zoom_in", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Equals, &[ModifierKey::Command]) },
    V1Mapping { old: "translation.canvas.zoom_out", new: "view.zoom_out", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Minus, &[ModifierKey::Command]) },
    V1Mapping { old: "cleaning.canvas.zoom_out", new: "view.zoom_out", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Minus, &[ModifierKey::Command]) },
    V1Mapping { old: "translation.canvas.zoom_reset", new: "view.zoom_reset", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Num0, &[ModifierKey::Command]) },
    V1Mapping { old: "cleaning.canvas.zoom_reset", new: "view.zoom_reset", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Num0, &[ModifierKey::Command]) },
    V1Mapping { old: "translation.bubble.delete_selected", new: "translation.bubble.delete_selected", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::Delete, &[]) },
    V1Mapping { old: "translation.bubble.create_at_cursor", new: "translation.bubble.create_at_cursor", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::T, &[]) },
    V1Mapping { old: "translation.bubble.create_hint_at_cursor", new: "translation.bubble.create_hint_at_cursor", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::H, &[]) },
    V1Mapping { old: "single_image.save", new: "single_image.save", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::S, &[ModifierKey::Command]) },
    V1Mapping { old: "single_image.save_as", new: "single_image.save_as", kind: ActionKind::Command, default: || v1_shortcut(egui::Key::S, &[ModifierKey::Command, ModifierKey::Shift]) },
];

/// Parses one v1 entry with the old reader's exact tolerance (`InputManagerV2::binding_from_json`):
/// only a non-object entry is rejected; a missing or non-boolean `enabled` reads as true, an
/// unparsable `shortcut` as absent, an unknown `modifier_only` token as absent, and missing or
/// non-boolean modifier flags as false. A disabled entry is unbound whatever its other fields
/// hold; a `modifier_only` wins over a shortcut, as the old dispatcher skipped such commands'
/// shortcut.
fn parse_v1(value: &Value) -> Result<V1Binding, String> {
    let object = value.as_object().ok_or_else(|| "not an object".to_owned())?;
    let enabled = object.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    if !enabled {
        return Ok(V1Binding::Unbound);
    }
    let modifier_only = match object.get("modifier_only").and_then(Value::as_str) {
        Some("ctrl") => Some(ModifierKey::Command),
        Some("alt") => Some(ModifierKey::Alt),
        Some("shift") => Some(ModifierKey::Shift),
        Some(_) | None => None,
    };
    if let Some(modifier) = modifier_only {
        return Ok(V1Binding::ModifierOnly(modifier));
    }
    Ok(object.get("shortcut").and_then(parse_v1_shortcut).unwrap_or(V1Binding::Unbound))
}

/// The old `shortcut_from_json`: `None` for a non-object, a missing / non-string `key` or an
/// unknown key name.
fn parse_v1_shortcut(value: &Value) -> Option<V1Binding> {
    let object = value.as_object()?;
    let key = egui::Key::from_name(object.get("key")?.as_str()?)?;
    let flag = |field: &str| object.get("modifiers").and_then(|modifiers| modifiers.get(field)).and_then(Value::as_bool).unwrap_or(false);
    let mut modifiers = BTreeSet::new();
    if flag("ctrl") || flag("command") || flag("mac_cmd") {
        modifiers.insert(ModifierKey::Command);
    }
    if flag("shift") {
        modifiers.insert(ModifierKey::Shift);
    }
    if flag("alt") {
        modifiers.insert(ModifierKey::Alt);
    }
    Some(V1Binding::Shortcut { key, modifiers })
}

/// The format-2 alternatives of a v1 binding for an action of `kind`.
fn v1_to_alternatives(binding: &V1Binding, kind: ActionKind) -> Vec<Value> {
    let buttons: BTreeSet<Button> = match binding {
        V1Binding::Unbound => return Vec::new(),
        V1Binding::Shortcut { key, modifiers } => modifiers.iter().map(|modifier| Button::Modifier(*modifier)).chain(std::iter::once(Button::Key(*key))).collect(),
        V1Binding::ModifierOnly(modifier) => std::iter::once(Button::Modifier(*modifier)).collect(),
    };
    let alternative = if kind == ActionKind::Mode { Binding::Mode(ButtonChord { buttons }) } else { Binding::command(Chord { modes: BTreeSet::new(), buttons }) };
    vec![encode_binding(&alternative)]
}

/// Migrates a version-1 section to format-2 overrides. Pure apart from logging: an entry the old
/// reader could not parse (not an object) is dropped with a log line (never the whole section),
/// damaged fields inside an entry read exactly as the old reader read them, an entry equal to its v1
/// default is dropped (the action follows the code defaults), two v1 ids that map onto one new
/// id keep the first non-default one in `V1_MAPPINGS` order, and an id unknown to the v1 table
/// migrates 1:1 as a command (the registry drops it later if no such action exists).
fn migrate_v1(section: &Map<String, Value>) -> HotkeyOverrides {
    let mut overrides = HotkeyOverrides::default();
    let mut decided: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    for mapping in V1_MAPPINGS {
        let Some(raw) = section.get(mapping.old) else { continue };
        let binding = match parse_v1(raw) {
            Ok(binding) => binding,
            Err(reason) => {
                ms_log::runtime_log::log_warn(format!("{LOG_PREFIX} v1 hotkey `{}` dropped during migration: {reason}", mapping.old));
                continue;
            }
        };
        if binding == (mapping.default)() {
            continue;
        }
        if let Some(winner) = decided.get(mapping.new) {
            ms_log::runtime_log::log_info(format!("{LOG_PREFIX} v1 hotkey `{}` dropped: `{}` already set `{}`", mapping.old, winner, mapping.new));
            continue;
        }
        decided.insert(mapping.new, mapping.old);
        overrides.actions.insert(mapping.new.to_owned(), v1_to_alternatives(&binding, mapping.kind));
    }
    for (id, raw) in section {
        if V1_MAPPINGS.iter().any(|mapping| mapping.old == id) {
            continue;
        }
        match parse_v1(raw) {
            Ok(binding) => {
                overrides.actions.insert(id.clone(), v1_to_alternatives(&binding, ActionKind::Command));
            }
            Err(reason) => ms_log::runtime_log::log_warn(format!("{LOG_PREFIX} v1 hotkey `{id}` dropped during migration: {reason}")),
        }
    }
    overrides
}

// ----------------------------------------------------------------------------------------------
// Writers

/// The format of the stored section: `None` = absent / empty / version 1 (rewritable),
/// `Some(Ok(()))` = format 2, `Some(Err(text))` = a format this build must not overwrite.
fn stored_format(section: Option<&Value>) -> Option<Result<(), String>> {
    let format = section?.as_object()?.get("format")?;
    Some(if format.as_u64() == Some(HOTKEYS_FORMAT) { Ok(()) } else { Err(format.to_string()) })
}

/// The marker error a mutator returns to abort on a newer section format.
#[derive(Debug, thiserror::Error)]
#[error("Hotkeys section format {0} is newer than this build")]
struct NewerFormatAbort(String);

fn update_section(path: &Path, edit: impl FnOnce(&mut Map<String, Value>) -> Result<(), NewerFormatAbort>) -> Result<(), PersistError> {
    let mut newer: Option<String> = None;
    let outcome = ms_config::update_user_config_file(path, |root| {
        let Some(root) = root.as_object_mut() else {
            return Err(anyhow::anyhow!("user config root is not an object"));
        };
        edit(root).map_err(|abort| {
            newer = Some(abort.0.clone());
            anyhow::Error::new(abort)
        })
    });
    match (outcome, newer) {
        (Ok(()), _) => Ok(()),
        (Err(_), Some(format)) => Err(PersistError::NewerFormat { path: path.to_path_buf(), format }),
        (Err(err), None) => Err(PersistError::UserConfig { path: path.to_path_buf(), message: format!("{err:#}") }),
    }
}

/// Writes one change into the `Hotkeys` section of the `user_config` at `path` (blocking; call
/// from a worker). A still-version-1 section is migrated in the same write first, so no other
/// override is lost.
///
/// # Errors
/// [`PersistError::NewerFormat`] when the stored section has a format this build does not
/// know (left untouched); [`PersistError::UserConfig`] when the document cannot be read, is
/// malformed (never overwritten) or cannot be written.
pub fn write_override(path: &Path, entry: &OverrideEntry) -> Result<(), PersistError> {
    update_section(path, |root| {
        let mut overrides = match stored_format(root.get(HOTKEYS_SECTION)) {
            Some(Err(format)) => return Err(NewerFormatAbort(format)),
            Some(Ok(())) | None => load_overrides(&Value::Object(root.clone())).overrides,
        };
        overrides.apply(entry);
        root.insert(HOTKEYS_SECTION.to_owned(), overrides.to_section());
        Ok(())
    })
}

/// Writes the section `load_overrides` migrated (blocking; call from a worker). A no-op when the
/// stored section already has format 2 (another writer got there first).
///
/// # Errors
/// As [`write_override`].
pub fn write_migrated_section(path: &Path, section: Value) -> Result<(), PersistError> {
    update_section(path, |root| {
        match stored_format(root.get(HOTKEYS_SECTION)) {
            Some(Err(format)) => return Err(NewerFormatAbort(format)),
            Some(Ok(())) => return Ok(()),
            None => {}
        }
        root.insert(HOTKEYS_SECTION.to_owned(), section);
        Ok(())
    })
}
