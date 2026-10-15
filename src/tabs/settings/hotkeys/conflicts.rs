/*
File: src/tabs/settings/hotkeys/conflicts.rs

Purpose:
The "All conflicts (N)" summary at the top of the hotkeys pane: every conflict of the registry's
report, worded from its first action's side, each with a Show button that scrolls to (and
frames) that action, plus the actions whose conflict analysis was cut short.

Key functions:
- `draw_summary`.

Notes:
- Shadows (an inner context taking over an outer binding) are intended behaviour and are shown
  only next to their actions, never in this summary.
*/

use ms_input::InputRegistry;

use super::Intent;

/// Draws the collapsible summary; a Show click becomes an `Intent::Jump`.
pub(super) fn draw_summary(ui: &mut egui::Ui, registry: &InputRegistry, intents: &mut Vec<Intent>) {
    let report = registry.conflicts();
    let count = report.conflicts.len();
    let mut title = egui::RichText::new(tf!("settings.hotkeys.conflicts_summary_title", count = count));
    if count > 0 {
        title = title.color(ms_theme::status::ERROR);
    }
    egui::CollapsingHeader::new(title).id_salt("settings.hotkeys.conflicts_summary_title").default_open(false).show(ui, |ui| {
        if count == 0 {
            ui.weak(t!("settings.hotkeys.no_conflicts_label"));
        }
        for (index, conflict) in report.conflicts.iter().enumerate() {
            ui.push_id(("conflict", index), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let row = tf!("settings.hotkeys.conflict_summary_row", action = registry.title(conflict.a), text = registry.conflict_text(conflict, conflict.a));
                    ui.colored_label(ms_theme::status::ERROR, row);
                    if ui.small_button(t!("settings.hotkeys.show_button")).clicked() {
                        intents.push(Intent::Jump { action: conflict.a });
                    }
                });
            });
        }
        for (index, action) in report.truncated.iter().enumerate() {
            ui.push_id(("truncated", index), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ms_theme::status::WARNING, tf!("settings.hotkeys.truncated_summary_row", action = registry.title(*action)));
                    if ui.small_button(t!("settings.hotkeys.show_button")).clicked() {
                        intents.push(Intent::Jump { action: *action });
                    }
                });
            });
        }
    });
}
