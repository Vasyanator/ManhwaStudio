/*
File: src/tabs/settings/tutorials.rs

Purpose:
Studio Settings "Обучение" pane — a thin wrapper that delegates to the shared,
surface-agnostic `crate::tutorial::draw_tutorials_pane`. The same pane renders in
the launcher settings page, so the replay UI stays identical across surfaces (the
"double interface" pattern, mirroring `ai_backend.rs`).
*/

use super::SettingsTabState;

impl SettingsTabState {
    /// Render the tutorials replay pane against this tab's progress handle.
    pub(super) fn draw_tutorials(&mut self, ui: &mut egui::Ui) {
        crate::tutorial::draw_tutorials_pane(ui, &self.tutorial_progress);
    }
}
