/*
File: storage_mode_setting.rs

Purpose:
The "data storage" row of the shared General pane (both shells): a two-option radio (Prod =
SQLite `.db`, Dev = `.json`) with a short explanation, the running conversion's progress, and
the list of documents that failed. Choosing a mode starts the process-wide conversion job
(`storage_mode_job`) on a worker — never on the GUI thread — and disables the radio while
any conversion (including the startup reconciliation and a launcher chapter conversion) runs.

Key structures:
- StorageModeSettingState : the id of the job this pane started (to report its end once)

Key functions:
- draw_storage_mode_setting()      : the row; returns the mode once a switch it started finished
- draw_conversion_failures()       : the shared failure list (also used by the launcher main page)

Notes:
The shown mode is the docstore default format (`ms_docstore::default_format`), which the
startup probe seeded and the driver switches first; it is therefore the mode the process
really writes new documents in. Web builds are Dev-only: the row explains that instead.
*/

use std::path::Path;

use ms_project::storage_mode::GlobalConvertOutcome;

/// How many failed documents are listed by path before the rest is summarized.
const FAILED_PATHS_SHOWN: usize = 6;

/// Per-surface state of the storage row.
#[derive(Debug, Default)]
pub struct StorageModeSettingState {
    /// Job id this pane started and whose end it has not reported yet.
    awaiting_job: Option<u64>,
    /// A start failure to show under the radio (localized).
    start_error: Option<String>,
}

/// Renders the storage row. `projects_root` is the root whose titles a switch converts
/// (the pane's saved projects dir). Returns `Some(mode)` exactly once, on the frame a
/// switch started by THIS pane is observed finished (whatever its per-document outcome).
#[cfg(not(target_arch = "wasm32"))]
pub fn draw_storage_mode_setting(ui: &mut egui::Ui, state: &mut StorageModeSettingState, projects_root: &Path) -> Option<ms_config::StorageMode> {
    use crate::storage_mode_job::{self as job, ConversionJobState, ConversionRequest, JobOrigin, StartConversionError};
    use ms_config::StorageMode;

    ui.label(t!("settings.general.storage_mode_label"));
    ui.small(t!("settings.general.storage_mode_hint"));

    let job_state = job::conversion_job_state();
    // A launcher chapter conversion also blocks the switch: the switch would change that
    // conversion's target format under it (`storage_mode_job::begin_chapter_conversion`).
    let chapter_running = job::chapter_conversion_running();
    let running = job_state.is_running() || chapter_running;
    if chapter_running {
        // The chapter worker ends without an input event on this surface: re-check soon so
        // the radio re-enables.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    let current = StorageMode::from_doc_format(ms_docstore::default_format());
    let mut selected = current;
    ui.add_enabled_ui(!running, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut selected, StorageMode::Prod, t!("settings.general.storage_mode_prod"));
            ui.radio_value(&mut selected, StorageMode::Dev, t!("settings.general.storage_mode_dev"));
        });
    });
    if selected != current && !running {
        let request = ConversionRequest {
            target: selected,
            projects_root: projects_root.to_path_buf(),
            fonts_dir: ms_config::storage_mode::app_fonts_dir(),
            user_config_path: ms_config::user_config_path(),
        };
        match job::start_conversion(JobOrigin::UserSwitch, request) {
            Ok(id) => {
                state.awaiting_job = Some(id);
                state.start_error = None;
            }
            Err(StartConversionError::AlreadyRunning | StartConversionError::ChapterConversionRunning) => state.start_error = Some(t!("settings.general.storage_mode_busy").to_string()),
            Err(StartConversionError::Spawn(err)) => state.start_error = Some(tf!("settings.general.storage_mode_spawn_error", err = err)),
        }
        ui.ctx().request_repaint();
    }
    if let Some(error) = &state.start_error {
        ui.colored_label(ui.visuals().error_fg_color, error.as_str());
    }

    let mut finished_mode = None;
    match &job_state {
        ConversionJobState::Running { done, total, .. } => {
            ui.horizontal_wrapped(|ui| {
                ui.spinner();
                ui.small(tf!("settings.general.storage_mode_converting", done = done, total = total));
            });
            // Progress arrives from the worker without any input event.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        ConversionJobState::Finished { id, target, outcome, .. } => {
            if outcome.is_complete() {
                if state.awaiting_job == Some(*id) || outcome.converted > 0 {
                    ui.small(t!("settings.general.storage_mode_done"));
                }
            } else {
                draw_conversion_failures(ui, outcome);
            }
            if state.awaiting_job == Some(*id) {
                state.awaiting_job = None;
                finished_mode = Some(*target);
            }
        }
        ConversionJobState::Idle | ConversionJobState::Pending { .. } => {}
    }
    finished_mode
}

/// Web build: the store is JSON-only there, so the row only explains that.
#[cfg(target_arch = "wasm32")]
pub fn draw_storage_mode_setting(ui: &mut egui::Ui, _state: &mut StorageModeSettingState, _projects_root: &Path) -> Option<ms_config::StorageMode> {
    ui.label(t!("settings.general.storage_mode_label"));
    ui.small(t!("settings.general.storage_mode_web_hint"));
    None
}

/// Renders a failed conversion's summary and the first failed document paths (the full
/// list with reasons is in the log).
pub fn draw_conversion_failures(ui: &mut egui::Ui, outcome: &GlobalConvertOutcome) {
    let error_color = ui.visuals().error_fg_color;
    if outcome.mode_persisted {
        ui.colored_label(error_color, tf!("settings.general.storage_mode_failed", count = outcome.failed.len()));
    } else {
        // Step (a) failed: the mode was not switched and nothing was converted.
        ui.colored_label(error_color, t!("settings.general.storage_mode_not_switched"));
    }
    for (path, _reason) in outcome.failed.iter().take(FAILED_PATHS_SHOWN) {
        ui.small(path.display().to_string());
    }
    let hidden = outcome.failed.len().saturating_sub(FAILED_PATHS_SHOWN);
    if hidden > 0 {
        ui.small(tf!("settings.general.storage_mode_failed_more", count = hidden));
    }
}
