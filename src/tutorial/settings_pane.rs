/*
File: src/tutorial/settings_pane.rs

Purpose:
The surface-agnostic "Обучение" settings pane. Depends only on a
`TutorialProgressHandle`, so the exact same UI is reused by the studio Settings
tab and the launcher settings page (the "double interface" the plan asks for,
mirroring `crate::ai_backend_panel`).

Behavior:
- an autoplay toggle for auto-starting unseen tutorials on first entry;
- one row per available tutorial with its completion state and a
  "Пройти заново обучение" button that clears completion (persisted). The
  tutorial then auto-starts the next time its surface/tab is entered — this pane
  never starts a tutorial itself, it only edits progress.

Notes:
Progress is snapshotted before the UI and re-locked only to apply a change, so
the mutex is never held across egui callbacks (project rule: no lock held during
callbacks).
*/

use std::sync::PoisonError;

use eframe::egui::{self, Align, Layout};

use super::id::TutorialId;
use super::progress::TutorialProgressHandle;

/// Render the shared tutorials replay pane against `progress`.
pub fn draw_tutorials_pane(ui: &mut egui::Ui, progress: &TutorialProgressHandle) {
    // Snapshot outside the UI so the lock is not held across widget callbacks.
    let (autoplay_now, rows) = {
        let guard = progress.lock().unwrap_or_else(PoisonError::into_inner);
        let rows: Vec<(TutorialId, bool)> = TutorialId::ALL
            .into_iter()
            .filter(|id| id.is_available())
            .map(|id| (id, guard.is_completed(id)))
            .collect();
        (guard.autoplay(), rows)
    };

    ui.heading("Обучение");
    ui.label(
        "Здесь можно заново пройти обучающие подсказки. После сброса обучение запустится \
         при следующем входе в соответствующее окно или вкладку.",
    );
    ui.add_space(8.0);

    let mut autoplay = autoplay_now;
    if ui
        .checkbox(
            &mut autoplay,
            "Автоматически запускать обучение при первом входе",
        )
        .changed()
    {
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_autoplay(autoplay);
    }

    ui.separator();

    if rows.is_empty() {
        ui.label("Пока нет доступных обучений.");
        return;
    }

    let mut reset_target = None;
    for (id, completed) in rows {
        ui.horizontal(|ui| {
            ui.label(id.title());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Пройти заново обучение").clicked() {
                    reset_target = Some(id);
                }
                if completed {
                    ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "✔ пройдено");
                } else {
                    ui.colored_label(egui::Color32::from_rgb(180, 180, 180), "○ не пройдено");
                }
            });
        });
    }

    if let Some(id) = reset_target {
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset(id);
    }
}
