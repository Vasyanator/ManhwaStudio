/*
FILE OVERVIEW: src/tabs/settings/general.rs
Thin studio wrapper for the General settings pane.

Main responsibilities:
- Enforce the studio-only vertical typing-panel layout (persisted off the GUI thread).
- Delegate the projects-directory editor and the global memory-profile combo to the
  shared `crate::general_settings_panel` widget (the same widget the launcher renders),
  then apply the studio-only runtime effect (memory profile -> `MemoryManager`) from the
  returned outcome.

Key functions:
- `SettingsTabState::draw_general`

Notes:
- The projects-dir save and memory-profile persistence are handled synchronously inside
  the shared widget; only the typing-panel-layout write is offloaded here.
*/

use super::{SettingsTabState, save_typing_panel_layout};
use crate::runtime_log;
use crate::tabs::typing::TypingPanelLayout;
use ms_thread as thread;

impl SettingsTabState {
    pub(super) fn draw_general(&mut self, ui: &mut egui::Ui) {
        ui.heading("Общие настройки");

        // Studio-only: the "Текст" tab panel is locked to the vertical layout. The
        // launcher has no typing tab, so this enforcement stays in the studio wrapper
        // rather than the shared widget. Persist off the GUI thread when it changes.
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

        // Shared widget: projects directory + memory profile (identical to the launcher).
        let outcome = crate::general_settings_panel::draw_general_settings_panel(
            ui,
            &mut self.general_settings_panel,
        );
        // Studio runtime effect: apply a changed memory profile to the shared
        // `MemoryManager` and cache owners. The saved projects dir needs no studio
        // runtime effect here.
        if let Some(profile) = outcome.memory_profile_changed {
            self.apply_memory_profile_to_runtime(profile);
        }
    }
}
