/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/progress.rs

Purpose:
The two things the panel draws above its controls: the progress shared by every long
operation of this engine, and the hint that explains what the mask MEANS here.

Main responsibilities:
- draw one bar for the load and generate phases, and a SECOND bar under it while a model
  download reports the file it is transferring (`draw_flux2_progress_ui`);
- state the mask rule and what an empty mask does (`draw_flux2_mask_hint`).

Key functions:
- `draw_flux2_progress_ui()`, `draw_flux2_mask_hint()`

Notes:
Only the DRAWING lives here; the state, its generation guard and the rate window are
`../progress.rs`. The mask hint reads no state at all — the working mode is derived from
the mask itself, and the brush belongs to the host's «Выбранный инструмент» panel.
*/

use super::*;

/// Explains what the mask MEANS for this engine, and what an empty one means.
///
/// A free function and not a [`Flux2PanelCtx`] method because it reads no state at all:
/// there is no control here and nothing to change. The working mode is derived from the
/// mask itself ([`mask_for_run`]), and the brush — radius, paint/erase, clear and fill —
/// belongs to the host's «Выбранный инструмент» panel, which drives the frame's one
/// `MaskBrush` for every engine. Both halves of the rule are stated at once because the
/// user picks between them by painting or not painting rather than by reading a state
/// back; the host adds its own green line under «Обработать» while the empty half is what
/// a click would actually do.
pub(super) fn draw_flux2_mask_hint(ui: &mut egui::Ui) {
    ui.separator();
    ui.label(t!("cleaning.tools.flux2_klein.mask_heading"));
    ui.small(t!("cleaning.tools.flux2_klein.mask_hint"));
}

/// Draws the progress shared by every long operation of this engine: one bar for the
/// load and generate phases, and a SECOND bar under it while a model download reports the
/// file it is transferring.
pub(super) fn draw_flux2_progress_ui(ui: &mut egui::Ui, progress: &Mutex<Flux2Progress>) {
    let state = {
        let guard = lock_progress(progress);
        if !guard.active {
            return;
        }
        Flux2Progress {
            generation: guard.generation,
            active: guard.active,
            phase: guard.phase.clone(),
            step: guard.step,
            total: guard.total,
            label: guard.label.clone(),
            file: guard.file.clone(),
            rate: guard.rate.clone(),
            cancel_id: guard.cancel_id,
        }
    };
    let (overall, file_fraction) = flux2_progress_fractions(&state);
    let text = if state.phase == "download" {
        // Bytes, not steps: `step`/`total` are the whole plan's byte counts here.
        tf!(
            "cleaning.tools.flux2_klein.download.overall_progress_status",
            done = format_gib(state.step),
            total = format_gib(state.total)
        )
    } else if state.phase == "load" {
        tf!(
            "cleaning.tools.flux2_klein.load_progress_status",
            label = state.label,
            step = state.step,
            total = state.total
        )
    } else if state.total > 0 {
        tf!("cleaning.common.step_progress_status", step = state.step, total = state.total)
    } else {
        state.label.clone()
    };
    ui.add(egui::ProgressBar::new(overall).text(text));
    match (file_fraction, state.file.as_ref()) {
        (Some(fraction), Some(file)) => {
            ui.add(
                egui::ProgressBar::new(fraction).text(tf!(
                    "cleaning.tools.flux2_klein.download.file_progress_status",
                    label = file.label,
                    done = format_gib(file.step),
                    total = format_gib(file.total)
                )),
            );
        }
        // No second level: the frame is a preparation phase, and its `label` is the only
        // thing that says what the backend is doing. It goes under the bar as plain text
        // rather than as a second bar frozen at zero.
        _ if state.phase == "download" && !state.label.trim().is_empty() => {
            ui.small(state.label.clone());
        }
        _ => {}
    }
    // Speed and remaining time, under the bars and only while a transfer is actually
    // running: this whole function returned early when the bar is inactive, so the numbers
    // cannot be left frozen beside an idle bar. Download frames only — a generation's
    // counters are steps, not bytes.
    if state.phase == "download"
        && let Some(line) = flux2_transfer_status(&state)
    {
        ui.small(line);
    }
    // Nothing else drives repaints while the worker runs, so the bar would freeze.
    ui.ctx().request_repaint();
}
