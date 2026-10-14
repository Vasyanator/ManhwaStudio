/*
FILE OVERVIEW: src/tabs/settings/hotkeys.rs
Hotkeys pane UI for the settings tab.

Main responsibilities:
- Render configurable hotkeys from `InputManagerV2`.
- Capture a replacement shortcut from live keyboard input.
- Persist user overrides in `user_config.json`.

Key types:
- `SettingsTabState`
- `HotkeyCommandV2`
- `InputManagerV2`

Key functions:
- `SettingsTabState::draw_hotkeys`

Notes:
- `Esc` cancels capture mode instead of being assigned as a binding.
- Whether a command is modifier-only (Ctrl/Alt/Shift buttons instead of key capture) comes
  from its spec (`default_modifier_only`); this file names no tab's command ids.
- Persistence is offloaded to a small background thread to avoid sync file IO on the GUI thread.
*/

use super::SettingsTabState;
use crate::input_manager_v2::{
    HotkeyCommandV2, HotkeyScopeV2, InputManagerV2, ModifierOnlyV2, clear_hotkey_override,
    save_hotkey_override,
};
use crate::runtime_log;
use crate::tabs::AppTab;
use ms_thread as thread;

impl SettingsTabState {
    pub(super) fn draw_hotkeys(&mut self, ui: &mut egui::Ui, hotkeys_v2: &mut InputManagerV2) {
        self.handle_hotkey_capture(ui, hotkeys_v2);

        ui.heading(t!("settings.hotkeys.heading"));
        let mut configurable_commands = hotkeys_v2.commands().to_vec();
        configurable_commands.sort_by(|a, b| {
            hotkey_scope_sort_key(a.scope)
                .cmp(&hotkey_scope_sort_key(b.scope))
                .then_with(|| a.section.cmp(&b.section))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.cmp(&b.id))
        });
        if configurable_commands.is_empty() {
            ui.label(t!("settings.hotkeys.none_configurable"));
        } else {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut current_scope: Option<HotkeyScopeV2> = None;
                    let mut scope_commands: Vec<HotkeyCommandV2> = Vec::new();

                    for command in configurable_commands {
                        if current_scope != Some(command.scope) {
                            if let Some(scope) = current_scope {
                                self.draw_configurable_scope_block(
                                    ui,
                                    hotkeys_v2,
                                    scope,
                                    &scope_commands,
                                );
                                scope_commands.clear();
                            }
                            current_scope = Some(command.scope);
                        }
                        scope_commands.push(command);
                    }
                    if let Some(scope) = current_scope {
                        self.draw_configurable_scope_block(ui, hotkeys_v2, scope, &scope_commands);
                    }
                });
        }
    }

    fn draw_configurable_hotkey_row(
        &mut self,
        ui: &mut egui::Ui,
        hotkeys_v2: &mut InputManagerV2,
        command: &HotkeyCommandV2,
    ) {
        let is_capturing = self.hotkey_capture_command_id.as_deref() == Some(command.id.as_str());
        let shortcut_text = hotkeys_v2
            .shortcut_text(ui.ctx(), &command.id)
            .unwrap_or_else(|| t!("settings.hotkeys.unassigned").to_string());

        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&command.title);
                ui.separator();
                ui.monospace(shortcut_text);
                ui.separator();
                ui.small(format_scope_v2(command.scope));
                if command.active_when_input {
                    ui.separator();
                    ui.small(t!("settings.hotkeys.works_while_typing"));
                }
            });
            ui.small(format!("id: {}", command.id));
            ui.add_space(4.0);

            ui.horizontal_wrapped(|ui| {
                if is_capturing {
                    if command_requires_modifier_only(command) {
                        ui.label(t!("settings.hotkeys.capture_modifier_hint"));
                    } else {
                        ui.label(
                            t!("settings.hotkeys.capture_shortcut_hint"),
                        );
                        if ui.button(t!("settings.hotkeys.leave_empty_button")).clicked() {
                            if let Some(binding) = hotkeys_v2.clear_binding(&command.id) {
                                self.persist_hotkey_override(command.id.clone(), binding);
                            }
                            self.hotkey_capture_command_id = None;
                        }
                    }
                    if ui.button("Ctrl").clicked() {
                        if let Some(binding) =
                            hotkeys_v2.set_modifier_only(&command.id, ModifierOnlyV2::Ctrl)
                        {
                            self.persist_hotkey_override(command.id.clone(), binding);
                        }
                        self.hotkey_capture_command_id = None;
                    }
                    if ui.button("Alt").clicked() {
                        if let Some(binding) =
                            hotkeys_v2.set_modifier_only(&command.id, ModifierOnlyV2::Alt)
                        {
                            self.persist_hotkey_override(command.id.clone(), binding);
                        }
                        self.hotkey_capture_command_id = None;
                    }
                    if ui.button("Shift").clicked() {
                        if let Some(binding) =
                            hotkeys_v2.set_modifier_only(&command.id, ModifierOnlyV2::Shift)
                        {
                            self.persist_hotkey_override(command.id.clone(), binding);
                        }
                        self.hotkey_capture_command_id = None;
                    }
                    if ui.button(t!("settings.hotkeys.cancel_button")).clicked() {
                        self.hotkey_capture_command_id = None;
                    }
                } else if ui.button(t!("settings.hotkeys.edit_button")).clicked() {
                    self.hotkey_capture_command_id = Some(command.id.clone());
                    ui.ctx().request_repaint();
                }

                if ui.button(t!("settings.hotkeys.reset_button")).clicked()
                    && let Some(binding) = hotkeys_v2.reset_to_default(&command.id) {
                        self.persist_hotkey_reset(command.id.clone(), binding);
                    }
            });
        });
        ui.add_space(6.0);
    }

    fn draw_configurable_scope_block(
        &mut self,
        ui: &mut egui::Ui,
        hotkeys_v2: &mut InputManagerV2,
        scope: HotkeyScopeV2,
        commands: &[HotkeyCommandV2],
    ) {
        if commands.is_empty() {
            return;
        }

        match scope {
            HotkeyScopeV2::Tab(AppTab::Translation) => {
                egui::CollapsingHeader::new(t!(
                    "settings.hotkeys.translation_scope_title"
                ))
                    // Keep the pre-existing stable id_salt: the header id must not
                    // depend on the (now localized) label text (exclusions §C).
                    .id_salt("settings_hotkeys_translation_scope")
                    .default_open(false)
                    .show(ui, |ui| {
                        self.draw_configurable_sectioned_rows(ui, hotkeys_v2, commands)
                    });
            }
            _ => {
                ui.heading(scope_group_title(scope));
                self.draw_configurable_sectioned_rows(ui, hotkeys_v2, commands);
            }
        }
    }

    fn draw_configurable_sectioned_rows(
        &mut self,
        ui: &mut egui::Ui,
        hotkeys_v2: &mut InputManagerV2,
        commands: &[HotkeyCommandV2],
    ) {
        let mut active_section: Option<&str> = None;
        for command in commands {
            let section = command.section.as_str();
            if active_section != Some(section) {
                if active_section.is_some() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);
                }
                ui.heading(section);
                if section == "OCR" {
                    ui.small(t!("settings.hotkeys.modifier_modes_hint"));
                    ui.small(t!("settings.hotkeys.recognition_modes_hint"));
                    ui.add_space(4.0);
                }
                active_section = Some(section);
            }
            self.draw_configurable_hotkey_row(ui, hotkeys_v2, command);
        }
    }

    fn handle_hotkey_capture(&mut self, ui: &mut egui::Ui, hotkeys_v2: &mut InputManagerV2) {
        let Some(command_id) = self.hotkey_capture_command_id.clone() else {
            return;
        };

        let captured = ui.ctx().input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    repeat: false,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
        });

        let Some((key, modifiers)) = captured else {
            return;
        };

        if key == egui::Key::Escape {
            self.hotkey_capture_command_id = None;
            ui.ctx().request_repaint();
            return;
        }

        // A modifier-only command is rebound through its Ctrl/Alt/Shift buttons, never by a
        // captured key: ignore key presses until the user cancels with Esc.
        let modifier_only = hotkeys_v2
            .commands()
            .iter()
            .find(|command| command.id == command_id)
            .is_some_and(command_requires_modifier_only);
        if modifier_only {
            return;
        }

        let shortcut = egui::KeyboardShortcut::new(
            egui::Modifiers {
                alt: modifiers.alt,
                ctrl: modifiers.ctrl,
                shift: modifiers.shift,
                command: modifiers.command,
                mac_cmd: modifiers.mac_cmd,
            },
            key,
        );
        if let Some(binding) = hotkeys_v2.set_shortcut(&command_id, Some(shortcut)) {
            self.persist_hotkey_override(command_id, binding);
        }
        self.hotkey_capture_command_id = None;
        ui.ctx().request_repaint();
    }

    fn persist_hotkey_override(
        &self,
        command_id: String,
        binding: crate::input_manager_v2::HotkeyBindingV2,
    ) {
        let user_settings_file = self.user_settings_file.clone();
        let _ = thread::Builder::new()
            .name("settings-hotkey-save".to_string())
            .spawn(move || {
                if let Err(err) = save_hotkey_override(&user_settings_file, &command_id, &binding) {
                    runtime_log::log_warn(format!(
                        "[settings::hotkeys] failed to save hotkey override; command_id={command_id}; cause={err}"
                    ));
                }
            });
    }

    fn persist_hotkey_reset(
        &self,
        command_id: String,
        _binding: crate::input_manager_v2::HotkeyBindingV2,
    ) {
        let user_settings_file = self.user_settings_file.clone();
        let _ = thread::Builder::new()
            .name("settings-hotkey-reset".to_string())
            .spawn(move || {
                if let Err(err) = clear_hotkey_override(&user_settings_file, &command_id) {
                    runtime_log::log_warn(format!(
                        "[settings::hotkeys] failed to clear hotkey override; command_id={command_id}; cause={err}"
                    ));
                }
            });
    }
}

/// Whether `command` is bound by a held modifier alone (Ctrl / Alt / Shift) rather than by a key
/// shortcut. Decided by the command's own spec (`default_modifier_only`), so the settings pane
/// needs no knowledge of which tab declares such commands.
fn command_requires_modifier_only(command: &HotkeyCommandV2) -> bool {
    command.default_modifier_only.is_some()
}

fn format_scope_v2(scope: HotkeyScopeV2) -> String {
    match scope {
        HotkeyScopeV2::Global => t!("settings.hotkeys.scope_global").to_string(),
        HotkeyScopeV2::Tab(tab) => tf!("settings.hotkeys.scope_tab", tab = tab.title()),
    }
}

fn hotkey_scope_sort_key(scope: HotkeyScopeV2) -> (u8, String) {
    match scope {
        HotkeyScopeV2::Global => (0, String::new()),
        HotkeyScopeV2::Tab(tab) => (1, tab.title().to_string()),
    }
}

fn scope_group_title(scope: HotkeyScopeV2) -> String {
    match scope {
        HotkeyScopeV2::Global => t!("settings.hotkeys.global_scope_title").to_string(),
        HotkeyScopeV2::Tab(AppTab::Translation) => t!("settings.hotkeys.translation_scope_title").to_string(),
        HotkeyScopeV2::Tab(tab) => tf!("settings.hotkeys.tab_scope_title", tab = tab.title()),
    }
}

#[cfg(test)]
mod tests {
    use super::command_requires_modifier_only;
    use crate::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2, InputManagerV2};
    use crate::tabs::AppTab;
    use crate::tabs::translation::{
        HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
        TranslationTabState,
    };

    /// The spec-derived decision matches the former hard-coded id list: both translation OCR
    /// selection modes are modifier-only, an ordinary shortcut command is not.
    #[test]
    fn modifier_only_follows_the_command_spec() {
        const NORMAL_ID: &str = "test.settings.normal_shortcut";
        let mut manager = InputManagerV2::default();
        for spec in TranslationTabState::hotkey_specs() {
            manager.register(spec);
        }
        manager.register(HotkeySpecV2 {
            id: NORMAL_ID,
            title: "normal",
            section: "Test",
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        });
        let requires = |id: &str| {
            manager
                .commands()
                .iter()
                .find(|command| command.id == id)
                .map(command_requires_modifier_only)
        };
        assert_eq!(requires(HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE), Some(true));
        assert_eq!(requires(HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE), Some(true));
        assert_eq!(requires(NORMAL_ID), Some(false));
    }
}
