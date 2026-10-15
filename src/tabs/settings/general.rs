/*
FILE OVERVIEW: src/tabs/settings/general.rs
Thin studio wrapper for the General settings pane.

Main responsibilities:
- Delegate the projects-directory editor and the global memory-profile combo to the
  shared `crate::general_settings_panel` widget (the same widget the launcher renders),
  then apply the studio-only runtime effect (memory profile -> `MemoryManager`) from the
  returned outcome.

Key functions:
- `SettingsTabState::draw_general`

Notes:
- The projects-dir save and memory-profile persistence are handled inside the shared widget.
- The shared General section is rendered through `SharedSettingsPanels::draw` (owned by
  `SettingsTabState`), not by calling the widget directly, so the studio embeds one
  shared-panel container.
*/

use super::{SettingsSectionId, SettingsSurface, SettingsTabState};

impl SettingsTabState {
    pub(super) fn draw_general(&mut self, ui: &mut egui::Ui) {
        ui.heading(t!("settings.general.heading"));

        ui.add_space(8.0);

        // Shared section: projects directory + memory profile + languages (identical to
        // the launcher), rendered through the shared-panel container. The studio shows no
        // warning badges (`None`) and ignores `outcome.changed_settings` (launcher rechecks).
        let outcome = self.shared.draw(
            SettingsSectionId::General,
            ui,
            SettingsSurface::Studio,
            &self.ai_backend_handle,
            None,
        );
        // Studio runtime effect: apply a changed memory profile to the shared
        // `MemoryManager` and cache owners. The saved projects dir needs no studio
        // runtime effect here.
        if let Some(profile) = outcome.memory_profile_changed {
            self.apply_memory_profile_to_runtime(profile);
        }
    }
}
