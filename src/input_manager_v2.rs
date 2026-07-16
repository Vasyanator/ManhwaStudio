/*
FILE OVERVIEW: src/input_manager_v2.rs
User-configurable hotkey manager with persistent overrides in `user_config.json`.

Main responsibilities:
- Register hotkey specs close to feature code via lightweight declarations.
- Apply persisted user overrides on top of code-defined defaults.
- Collect triggered commands from egui input by `id`/scope.
- Serialize and persist hotkey overrides for the Settings UI.

Key types:
- `HotkeySpecV2`: code-defined hotkey declaration with default shortcut.
- `HotkeyScopeV2`: scope of availability (`Global` or a concrete `AppTab`).
- `HotkeyBindingV2`: effective runtime binding.
- `HotkeyCommandV2`: registered command with metadata and current binding.
- `InputManagerV2`: runtime registry and dispatcher for configurable hotkeys.

Notes:
- Defaults stay in Rust code; only user overrides are stored on disk.
- Persistence helpers here are intentionally small and JSON-based to keep GUI wiring simple.
*/

use crate::tabs::AppTab;
use eframe::egui;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub const HOTKEYS_CONFIG_SECTION: &str = "Hotkeys";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(dead_code)]
pub enum HotkeyScopeV2 {
    Global,
    Tab(AppTab),
}

#[derive(Debug, Clone, Copy)]
pub struct HotkeySpecV2 {
    pub id: &'static str,
    pub title: &'static str,
    pub section: &'static str,
    pub default_shortcut: Option<egui::KeyboardShortcut>,
    pub default_modifier_only: Option<ModifierOnlyV2>,
    pub scope: HotkeyScopeV2,
    pub active_when_input: bool,
}

#[derive(Debug, Clone)]
pub struct HotkeyBindingV2 {
    pub shortcut: Option<egui::KeyboardShortcut>,
    pub modifier_only: Option<ModifierOnlyV2>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ModifierOnlyV2 {
    Ctrl,
    Alt,
    Shift,
}

impl ModifierOnlyV2 {
    fn label(self) -> &'static str {
        match self {
            ModifierOnlyV2::Ctrl => "Ctrl",
            ModifierOnlyV2::Alt => "Alt",
            ModifierOnlyV2::Shift => "Shift",
        }
    }

    fn matches(self, modifiers: egui::Modifiers) -> bool {
        match self {
            ModifierOnlyV2::Ctrl => modifiers.ctrl || modifiers.command,
            ModifierOnlyV2::Alt => modifiers.alt,
            ModifierOnlyV2::Shift => modifiers.shift,
        }
    }

    fn as_config_str(self) -> &'static str {
        match self {
            ModifierOnlyV2::Ctrl => "ctrl",
            ModifierOnlyV2::Alt => "alt",
            ModifierOnlyV2::Shift => "shift",
        }
    }

    fn from_config_str(value: &str) -> Option<Self> {
        match value {
            "ctrl" => Some(ModifierOnlyV2::Ctrl),
            "alt" => Some(ModifierOnlyV2::Alt),
            "shift" => Some(ModifierOnlyV2::Shift),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HotkeyCommandV2 {
    pub id: String,
    pub title: String,
    pub section: String,
    pub default_shortcut: Option<egui::KeyboardShortcut>,
    pub default_modifier_only: Option<ModifierOnlyV2>,
    pub scope: HotkeyScopeV2,
    pub active_when_input: bool,
    pub binding: HotkeyBindingV2,
    /// Whether this command's keyboard shortcut was held on the previous `collect_triggered` call.
    /// Used to fire only on the rising edge of a press, so holding a key does not repeat the command
    /// (a new activation requires releasing and pressing the key again).
    last_shortcut_held: bool,
}

#[derive(Debug, Default)]
pub struct InputManagerV2 {
    commands: Vec<HotkeyCommandV2>,
    bindings_revision: u64,
}

impl InputManagerV2 {
    pub fn register(&mut self, spec: HotkeySpecV2) {
        let binding = if let Some(modifier_only) = spec.default_modifier_only {
            HotkeyBindingV2 {
                shortcut: None,
                modifier_only: Some(modifier_only),
                enabled: true,
            }
        } else {
            HotkeyBindingV2 {
                shortcut: spec.default_shortcut,
                modifier_only: None,
                enabled: spec.default_shortcut.is_some(),
            }
        };
        self.commands.push(HotkeyCommandV2 {
            id: spec.id.to_string(),
            title: spec.title.to_string(),
            section: spec.section.to_string(),
            default_shortcut: spec.default_shortcut,
            default_modifier_only: spec.default_modifier_only,
            scope: spec.scope,
            active_when_input: spec.active_when_input,
            binding,
            last_shortcut_held: false,
        });
        self.bindings_revision = self.bindings_revision.saturating_add(1);
    }

    pub fn load_overrides(&mut self, user_settings_file: &Path) {
        let overrides = load_hotkey_overrides(user_settings_file);
        for command in &mut self.commands {
            if let Some(binding) = overrides.get(&command.id) {
                command.binding = binding.clone();
            }
        }
        self.bindings_revision = self.bindings_revision.saturating_add(1);
    }

    pub fn commands(&self) -> &[HotkeyCommandV2] {
        &self.commands
    }

    pub fn bindings_revision(&self) -> u64 {
        self.bindings_revision
    }

    pub fn set_shortcut(
        &mut self,
        command_id: &str,
        shortcut: Option<egui::KeyboardShortcut>,
    ) -> Option<HotkeyBindingV2> {
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.id == command_id)?;
        command.binding.shortcut = shortcut;
        command.binding.modifier_only = None;
        command.binding.enabled = shortcut.is_some();
        self.bindings_revision = self.bindings_revision.saturating_add(1);
        Some(command.binding.clone())
    }

    pub fn clear_binding(&mut self, command_id: &str) -> Option<HotkeyBindingV2> {
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.id == command_id)?;
        command.binding = HotkeyBindingV2 {
            shortcut: None,
            modifier_only: None,
            enabled: false,
        };
        self.bindings_revision = self.bindings_revision.saturating_add(1);
        Some(command.binding.clone())
    }

    pub fn set_modifier_only(
        &mut self,
        command_id: &str,
        modifier_only: ModifierOnlyV2,
    ) -> Option<HotkeyBindingV2> {
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.id == command_id)?;
        command.binding.shortcut = None;
        command.binding.modifier_only = Some(modifier_only);
        command.binding.enabled = true;
        self.bindings_revision = self.bindings_revision.saturating_add(1);
        Some(command.binding.clone())
    }

    pub fn reset_to_default(&mut self, command_id: &str) -> Option<HotkeyBindingV2> {
        let command = self
            .commands
            .iter_mut()
            .find(|command| command.id == command_id)?;
        command.binding = if let Some(modifier_only) = command.default_modifier_only {
            HotkeyBindingV2 {
                shortcut: None,
                modifier_only: Some(modifier_only),
                enabled: true,
            }
        } else {
            HotkeyBindingV2 {
                shortcut: command.default_shortcut,
                modifier_only: None,
                enabled: command.default_shortcut.is_some(),
            }
        };
        self.bindings_revision = self.bindings_revision.saturating_add(1);
        Some(command.binding.clone())
    }

    pub fn binding(&self, command_id: &str) -> Option<&HotkeyBindingV2> {
        self.commands
            .iter()
            .find(|command| command.id == command_id)
            .map(|command| &command.binding)
    }

    pub fn shortcut_text(&self, ctx: &egui::Context, command_id: &str) -> Option<String> {
        let binding = self.binding(command_id)?;
        if !binding.enabled {
            return None;
        }
        if let Some(modifier_only) = binding.modifier_only {
            return Some(modifier_only.label().to_string());
        }
        binding
            .shortcut
            .as_ref()
            .map(|shortcut| ctx.format_shortcut(shortcut))
    }

    pub fn modifier_only_active(&self, ctx: &egui::Context, command_id: &str) -> bool {
        let Some(binding) = self.binding(command_id) else {
            return false;
        };
        if !binding.enabled {
            return false;
        }
        let Some(modifier_only) = binding.modifier_only else {
            return false;
        };
        ctx.input(|input| modifier_only.matches(input.modifiers))
    }

    pub fn collect_triggered(&mut self, ctx: &egui::Context, active_tab: AppTab) -> Vec<String> {
        let wants_keyboard_input = ctx.egui_wants_keyboard_input();
        let mut triggered = Vec::new();

        for command in &mut self.commands {
            if !command.binding.enabled
                || !matches_scope(command.scope, active_tab)
                || (wants_keyboard_input && !command.active_when_input)
            {
                command.last_shortcut_held = false;
                continue;
            }

            if command.binding.modifier_only.is_some() {
                continue;
            }

            let Some(shortcut) = command.binding.shortcut else {
                command.last_shortcut_held = false;
                continue;
            };

            // Consume the press event (including auto-repeats) so it does not propagate, but fire
            // only on the rising edge: when the shortcut is freshly pressed and was not already held
            // on the previous frame. Holding the key therefore activates the command exactly once;
            // a new activation requires releasing and pressing the key again.
            let pressed_event = ctx.input_mut(|i| i.consume_shortcut(&shortcut));
            let held = ctx.input(|i| {
                i.key_down(shortcut.logical_key)
                    && i.modifiers.matches_logically(shortcut.modifiers)
            });
            if pressed_event && !command.last_shortcut_held {
                triggered.push(command.id.clone());
            }
            command.last_shortcut_held = held;
        }

        triggered
    }
}

/// Persists one command's hotkey override into `user_config.json`.
///
/// Routed through [`crate::config::update_user_config_file`], which serializes the whole
/// read-modify-write on the process-wide user-config lock. That serialization is load-bearing:
/// an unlocked writer here can drop the fsync'd ORT SIGILL guard marker written concurrently by
/// `write_ort_load_state`, re-arming an uncatchable crash on the next launch.
///
/// # Errors
/// Returns the boundary's error text when the file cannot be read, is malformed JSON, or cannot
/// be written. A malformed file is reported, never silently replaced.
pub fn save_hotkey_override(
    user_settings_file: &Path,
    command_id: &str,
    binding: &HotkeyBindingV2,
) -> Result<(), String> {
    crate::config::update_user_config_file(user_settings_file, |root| {
        let root_obj = ensure_object(root);
        let hotkeys_value = root_obj
            .entry(HOTKEYS_CONFIG_SECTION.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let hotkeys_obj = ensure_object(hotkeys_value);
        hotkeys_obj.insert(command_id.to_string(), binding_to_json(binding));
        Ok(())
    })
    .map_err(|err| err.to_string())
}

/// Removes one command's hotkey override from `user_config.json`, restoring its default bind.
///
/// Shares [`save_hotkey_override`]'s serialized read-modify-write boundary and its guarantee that a
/// malformed config is reported rather than overwritten.
///
/// # Errors
/// Returns the boundary's error text when the file cannot be read, is malformed JSON, or cannot
/// be written.
pub fn clear_hotkey_override(user_settings_file: &Path, command_id: &str) -> Result<(), String> {
    crate::config::update_user_config_file(user_settings_file, |root| {
        let root_obj = ensure_object(root);
        if let Some(hotkeys) = root_obj.get_mut(HOTKEYS_CONFIG_SECTION)
            && let Some(hotkeys_obj) = hotkeys.as_object_mut()
        {
            hotkeys_obj.remove(command_id);
        }
        Ok(())
    })
    .map_err(|err| err.to_string())
}

fn load_hotkey_overrides(user_settings_file: &Path) -> HashMap<String, HotkeyBindingV2> {
    let root = load_root_json(user_settings_file);
    root.get(HOTKEYS_CONFIG_SECTION)
        .and_then(Value::as_object)
        .map(|hotkeys| {
            hotkeys
                .iter()
                .filter_map(|(command_id, raw_binding)| {
                    binding_from_json(raw_binding).map(|binding| (command_id.clone(), binding))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_root_json(user_settings_file: &Path) -> Value {
    match fs::read_to_string(user_settings_file) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        Err(_) => Value::Object(Map::new()),
    }
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("object ensured")
}

fn binding_to_json(binding: &HotkeyBindingV2) -> Value {
    let shortcut_value = binding
        .shortcut
        .as_ref()
        .map(shortcut_to_json)
        .unwrap_or(Value::Null);
    let mut object = Map::new();
    object.insert("enabled".to_string(), Value::Bool(binding.enabled));
    object.insert("shortcut".to_string(), shortcut_value);
    object.insert(
        "modifier_only".to_string(),
        binding
            .modifier_only
            .map(|modifier| Value::String(modifier.as_config_str().to_string()))
            .unwrap_or(Value::Null),
    );
    Value::Object(object)
}

fn binding_from_json(value: &Value) -> Option<HotkeyBindingV2> {
    let object = value.as_object()?;
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let shortcut = match object.get("shortcut") {
        Some(Value::Null) | None => None,
        Some(raw) => shortcut_from_json(raw),
    };
    let modifier_only = object
        .get("modifier_only")
        .and_then(Value::as_str)
        .and_then(ModifierOnlyV2::from_config_str);
    Some(HotkeyBindingV2 {
        shortcut,
        modifier_only,
        enabled,
    })
}

fn shortcut_to_json(shortcut: &egui::KeyboardShortcut) -> Value {
    let mut modifiers = Map::new();
    modifiers.insert("alt".to_string(), Value::Bool(shortcut.modifiers.alt));
    modifiers.insert("ctrl".to_string(), Value::Bool(shortcut.modifiers.ctrl));
    modifiers.insert("shift".to_string(), Value::Bool(shortcut.modifiers.shift));
    modifiers.insert(
        "command".to_string(),
        Value::Bool(shortcut.modifiers.command),
    );

    let mut object = Map::new();
    object.insert(
        "key".to_string(),
        Value::String(shortcut.logical_key.name().to_string()),
    );
    object.insert("modifiers".to_string(), Value::Object(modifiers));
    Value::Object(object)
}

fn shortcut_from_json(value: &Value) -> Option<egui::KeyboardShortcut> {
    let object = value.as_object()?;
    let key_name = object.get("key")?.as_str()?;
    let key = egui::Key::from_name(key_name)?;
    let modifiers = object
        .get("modifiers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Some(egui::KeyboardShortcut::new(
        egui::Modifiers {
            alt: modifiers
                .get("alt")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ctrl: modifiers
                .get("ctrl")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            shift: modifiers
                .get("shift")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            command: modifiers
                .get("command")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..Default::default()
        },
        key,
    ))
}

fn matches_scope(scope: HotkeyScopeV2, active_tab: AppTab) -> bool {
    match scope {
        HotkeyScopeV2::Global => true,
        HotkeyScopeV2::Tab(tab) => tab == active_tab,
    }
}
