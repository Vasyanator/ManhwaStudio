/*
FILE OVERVIEW: src/tabs/settings/general.rs
General settings pane UI for the settings tab.

Main responsibilities:
- Render general settings that still affect runtime state.
- Render and persist projects directory.

Key types:
- `SettingsTabState`

Key functions:
- `SettingsTabState::draw_general`

Notes:
- Persistence is delegated to background threads to avoid blocking the GUI thread.
*/

use super::{
    SettingsTabState, normalize_projects_dir_value, save_memory_profile, save_projects_dir,
    save_typing_panel_layout,
};
use crate::config;
use crate::memory_manager::MemoryProfile;
use crate::runtime_log;
use crate::tabs::typing::TypingPanelLayout;
use std::thread;

impl SettingsTabState {
    pub(super) fn draw_general(&mut self, ui: &mut egui::Ui) {
        ui.heading("Общие настройки");
        if self.typing_panel_layout != TypingPanelLayout::Vertical {
            self.typing_panel_layout = TypingPanelLayout::Vertical;
            self.pending_typing_panel_layout = Some(TypingPanelLayout::Vertical);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-typing-layout-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_typing_panel_layout(&path, TypingPanelLayout::Vertical) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist typing panel layout to {}; error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start typing panel layout save thread; error={err}"
                ));
            }
        }
        ui.label("Панель вкладки «Текст» использует только вертикальный формат.");

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label("Использование памяти:");
        ui.small("Применяется сразу к общей политике кэшей изображений.");

        let mut selected_profile = self.memory_profile;
        egui::ComboBox::from_id_salt("settings_memory_profile")
            .selected_text(selected_profile.display_name_ru())
            .show_ui(ui, |ui| {
                for profile in MemoryProfile::ALL {
                    ui.selectable_value(&mut selected_profile, profile, profile.display_name_ru());
                }
            });

        if selected_profile != self.memory_profile {
            self.memory_profile = selected_profile;
            self.apply_memory_profile_to_runtime(selected_profile);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-memory-profile-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_memory_profile(&path, selected_profile) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist memory profile '{}' to {}; error={err}",
                            selected_profile.as_config_str(),
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start memory profile save thread; error={err}"
                ));
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label("Папка с проектами:");
        ui.small("Используется базовым Rust-лаунчером при выборе тайтла и главы.");

        let mut should_save_projects_dir = false;
        ui.horizontal_wrapped(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.projects_dir_input)
                    .font(egui::FontId::new(14.0, egui::FontFamily::Monospace))
                    .desired_width(520.0)
                    .hint_text(config::default_projects_root().to_string_lossy()),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                should_save_projects_dir = true;
            }
            let current_projects_dir = normalize_projects_dir_value(&self.projects_dir_input);
            if ui
                .add_enabled(
                    current_projects_dir != self.saved_projects_dir,
                    egui::Button::new("Сохранить"),
                )
                .clicked()
            {
                should_save_projects_dir = true;
            }
        });

        if should_save_projects_dir {
            let normalized = normalize_projects_dir_value(&self.projects_dir_input);
            self.projects_dir_input = normalized.clone();
            self.saved_projects_dir = normalized.clone();
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-projects-dir-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_projects_dir(&path, &normalized) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist projects directory to {}; error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start projects directory save thread; error={err}"
                ));
            }
        }
        ui.small("Если поле пустое, автоматически используется путь по умолчанию.");
    }
}
