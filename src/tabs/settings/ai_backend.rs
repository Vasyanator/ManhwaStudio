/*
FILE OVERVIEW: src/tabs/settings/ai_backend.rs
Thin studio-side wrapper around the shared AI backend settings widget.

The panel itself lives in `crate::ai_backend_panel` so the launcher settings page
can render the exact same controls against the same app-global supervisor. This
file just forwards the studio's `AiBackendHandle` + panel scratch state.
*/

use super::SettingsTabState;
use crate::ai_backend_panel::draw_ai_backend_panel;

impl SettingsTabState {
    pub(super) fn draw_ai_backend(&mut self, ui: &mut egui::Ui) {
        draw_ai_backend_panel(ui, &self.ai_backend_handle, &mut self.ai_backend_panel);
    }
}
