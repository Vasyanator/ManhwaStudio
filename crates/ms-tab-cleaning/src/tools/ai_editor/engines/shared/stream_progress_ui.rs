/*
File: cleaning/tools/ai_editor/engines/shared/stream_progress_ui.rs

Purpose:
Draws the progress bar every long operation of a large-model engine shares: one bar for the
load and generate phases, a SECOND bar under it while a model download reports the file it
is transferring or any other phase the file it is reading (named by its component), and the
speed / remaining-time line of that transfer or read.

Key functions:
- `draw_stream_progress()`: the bar with the backend's own `label` text (FLUX.2).
- `draw_stream_progress_staged()`: the same bar, naming a load or generate frame by the
  engine's localization of its stable `stage` id (FLUX.3 klein), the `label` as fallback.

Notes:
Only the DRAWING lives here; the state, its generation guard and the rate window are
`stream_progress.rs`. Every text is a `cleaning.tools.engine_progress.*` key (or the shared
`cleaning.common.step_progress_status`). An inactive bar draws nothing at all.
*/

use super::stream_progress::{EngineProgress, file_component_label, lock_progress, progress_fractions, transfer_status};
use eframe::egui;
use ms_local_ai::large_models::format_gib;
use std::sync::Mutex;

/// Draws the progress shared by every long operation of an engine: one bar for the
/// load and generate phases, and a SECOND bar under it while a model download reports the
/// file it is transferring or a load the file it is reading; the speed line under them.
///
/// The state is copied out under the lock and drawn after it is released, so the lock is
/// never held across layout. Requests a repaint while active: nothing else wakes the GUI
/// while the worker runs. A frame's `stage` is not consulted: the text is the backend's
/// `label` ([`draw_stream_progress_staged`] is the variant that localizes stages).
pub(in crate::tools::ai_editor::engines) fn draw_stream_progress(ui: &mut egui::Ui, progress: &Mutex<EngineProgress>) {
    draw_stream_progress_staged(ui, progress, |_| None);
}

/// [`draw_stream_progress`], naming a load or generate frame by `stage_label` of its stable
/// `stage` id instead of the backend's `label` (which is log text in the backend's language).
///
/// `stage_label` is the engine's own localization: it answers `None` for an id it does not
/// know, and the frame then falls back to the `label` exactly as without a stage, so a newer
/// backend's new stage still reads as something. Download frames are unaffected (their
/// counters are bytes and their text is the file names).
pub(in crate::tools::ai_editor::engines) fn draw_stream_progress_staged(
    ui: &mut egui::Ui,
    progress: &Mutex<EngineProgress>,
    stage_label: impl Fn(&str) -> Option<&'static str>,
) {
    let state = {
        let guard = lock_progress(progress);
        if !guard.active {
            return;
        }
        EngineProgress {
            generation: guard.generation,
            active: guard.active,
            phase: guard.phase.clone(),
            step: guard.step,
            total: guard.total,
            label: guard.label.clone(),
            stage: guard.stage.clone(),
            file: guard.file.clone(),
            rate: guard.rate.clone(),
            rate_source: guard.rate_source.clone(),
            // The control and the canceller are not display state.
            control: None,
            canceller: None,
        }
    };
    let (overall, file_fraction) = progress_fractions(&state);
    let staged = state.stage.as_deref().and_then(&stage_label);
    let text = if state.phase == "download" {
        // Bytes, not steps: `step`/`total` are the whole plan's byte counts here.
        tf!(
            "cleaning.tools.engine_progress.overall_progress_status",
            done = format_gib(state.step),
            total = format_gib(state.total)
        )
    } else if state.phase == "load" {
        tf!(
            "cleaning.tools.engine_progress.load_progress_status",
            label = staged.map_or_else(|| state.label.clone(), str::to_string),
            step = state.step,
            total = state.total
        )
    } else if let Some(stage) = staged {
        // A named generate stage reads like a load one: its name and its counter.
        tf!("cleaning.tools.engine_progress.load_progress_status", label = stage, step = state.step, total = state.total)
    } else if state.total > 0 {
        tf!("cleaning.common.step_progress_status", step = state.step, total = state.total)
    } else {
        state.label.clone()
    };
    ui.add(egui::ProgressBar::new(overall).text(text));
    match (file_fraction, state.file.as_ref()) {
        (Some(fraction), Some(file)) => {
            // A read of any phase (load, encode, a denoise re-read) names WHICH component the
            // file is read for; a download's file names are already the repository paths.
            let component = if state.phase == "download" { None } else { file_component_label(file.component.as_deref()) };
            let text = match component {
                Some(component) => tf!(
                    "cleaning.tools.engine_progress.file_component_progress_status",
                    component = component,
                    label = file.label,
                    done = format_gib(file.step),
                    total = format_gib(file.total)
                ),
                None => tf!(
                    "cleaning.tools.engine_progress.file_progress_status",
                    label = file.label,
                    done = format_gib(file.step),
                    total = format_gib(file.total)
                ),
            };
            ui.add(egui::ProgressBar::new(fraction).text(text));
        }
        // No second level: the frame is a preparation phase, and its `label` is the only
        // thing that says what the backend is doing. It goes under the bar as plain text
        // rather than as a second bar frozen at zero.
        _ if state.phase == "download" && !state.label.trim().is_empty() => {
            ui.small(state.label.clone());
        }
        _ => {}
    }
    // Speed and remaining time, under the bars and only while a transfer or a file read is
    // actually running: this whole function returned early when the bar is inactive, so the
    // numbers cannot be left frozen beside an idle bar. `transfer_status` answers only while
    // the current frame still feeds the measured byte counter — a generation's counters are
    // steps, and a load step between two files has no file to time.
    if let Some(line) = transfer_status(&state) {
        ui.small(line);
    }
    // Nothing else drives repaints while the worker runs, so the bar would freeze.
    ui.ctx().request_repaint();
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Every `cleaning.tools.engine_progress.*` key the bar and its formatters spell is in
    /// every shipped catalog with its placeholders — a `t!` call cannot check that, because
    /// a unit test runs with no catalog loaded.
    #[test]
    fn every_catalog_carries_the_progress_strings() {
        // The load-read keys are newer than the es / fr / pt translations, which fall back to
        // `en` (PROJECT_RULES "Localization"): they are required in the two reference catalogs.
        let reference_only = ["file_component_progress_status", "component_transformer_label", "component_text_encoder_label", "component_vae_label"];
        let keys: [(&str, &[&str]); 13] = [
            ("preparing_status", &[]),
            ("load_progress_status", &["{label}", "{step}", "{total}"]),
            ("overall_progress_status", &["{done}", "{total}"]),
            ("file_progress_status", &["{label}", "{done}", "{total}"]),
            ("file_component_progress_status", &["{component}", "{label}", "{done}", "{total}"]),
            ("component_transformer_label", &[]),
            ("component_text_encoder_label", &[]),
            ("component_vae_label", &[]),
            ("speed_status", &["{speed}"]),
            ("speed_and_eta_status", &["{speed}", "{eta}"]),
            ("eta_hours", &["{hours}", "{minutes}"]),
            ("eta_minutes", &["{minutes}"]),
            ("eta_seconds", &["{seconds}"]),
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for (key, placeholders) in keys {
                if reference_only.contains(&key) && !matches!(*tag, "en" | "ru") {
                    continue;
                }
                let key = format!("cleaning.tools.engine_progress.{key}");
                let text = catalog.get(&key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                assert!(!text.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                for placeholder in placeholders {
                    assert!(text.contains(placeholder), "locale `{tag}`: `{key}` must carry `{placeholder}`");
                }
            }
        }
    }
}
