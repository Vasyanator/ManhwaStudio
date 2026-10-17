/*
FILE OVERVIEW: crates/ms-settings-ui/src/large_models_panel.rs
The «Большие модели» section of the AI-backend pane (studio Settings and launcher): what the
backend's large-model pool holds right now, its two hard caps, and the two pool actions
(«Выгрузить большие модели», «Сбросить выученные ограничения»).

Main items:
- `LargeModelsPanelState`: the caps buffer, the debounced cap writer and the action worker.
- `draw_large_models_section()`: the section, drawn under the small-model limit.
- `large_model_entry_line()`: the pure text of one pool entry (tested).

Notes:
- The pool state comes from the PUSHED health snapshot (`AiBackendHealthSnapshot::large_pool`,
  `model_manager.large_models` + `limits`); nothing here polls the backend.
- The backend is the caps' only writer (`large_models.limits.set`); this section sends edits
  through `ms_local_ai::large_models::LimitsWriter`, ONE AXIS per edit (the other `Keep`), and
  never writes `user_config`. A pending edit is delivered when the pane state is dropped.
- Every backend call runs on a worker; drawing only drains channels.
*/

use eframe::egui;
use ms_local_ai::backend_health::AiBackendHealthSnapshot;
use ms_local_ai::large_models::{self as large_models, LIMITS_EDIT_DEBOUNCE, CapChange, LargeModelEntry, LargeModelStep, LargeModelsError, Limits, LimitsWriter};
use ms_widgets::ByteCapEdit;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use web_time::Instant;

/// The answer of one pool action.
#[derive(Debug)]
enum PoolActionOutcome {
    Unloaded(Result<Vec<String>, LargeModelsError>),
    Reset(Result<u32, LargeModelsError>),
}

/// Per-pane state of the «Большие модели» section (one per pane instance).
#[derive(Debug, Default)]
pub struct LargeModelsPanelState {
    /// The caps as edited.
    limits_edit: Limits,
    /// The caps the last pushed snapshot reported: the editor is re-seeded only when they
    /// CHANGE (and no edit is pending), so a write that is waiting for the next push does not
    /// flicker back to the old value.
    last_pushed: Option<Limits>,
    writer: LimitsWriter,
    limits_error: Option<String>,
    action_rx: Option<Receiver<PoolActionOutcome>>,
    /// The outcome line of the last pool action.
    action_status: Option<String>,
}

/// The text of one pool entry: model, bytes on the device and on the host, the effective
/// steps of its last run, and whether it is running or ran degraded.
#[must_use]
pub fn large_model_entry_line(entry: &LargeModelEntry) -> String {
    let mut parts = vec![tf!(
        "ai_backend.large_models.entry_status",
        model = entry.model,
        vram = large_models::format_gib(entry.device_bytes),
        ram = large_models::format_gib(entry.host_bytes)
    )];
    if let (Some(vram), Some(ram)) = (entry.vram_step, entry.ram_step) {
        parts.push(tf!("ai_backend.large_models.steps_status", vram_step = vram.label(), ram_step = ram.label()));
    }
    if entry.in_use {
        parts.push(t!("ai_backend.large_models.in_use_status").to_string());
    }
    if entry.degraded {
        let failure = entry
            .degrade_reason
            .as_ref()
            .map_or_else(|| t!("large_models.step.unknown_label").to_string(), large_models::FailureKind::label);
        parts.push(tf!("ai_backend.large_models.degraded_status", failure = failure));
    }
    parts.join(" · ")
}

/// Drains the cap writer and the action worker. GUI-safe.
fn poll(state: &mut LargeModelsPanelState) {
    match state.writer.poll(Instant::now()) {
        Some(Ok(limits)) => {
            state.limits_error = None;
            if !state.writer.is_busy() {
                state.limits_edit = limits;
            }
        }
        Some(Err(err)) => {
            ms_log::runtime_log::log_warn(format!("[ai_backend_panel] large-model limits not saved: {err}"));
            state.limits_error = Some(tf!("large_models.limits.save_error", err = err.user_message()));
        }
        None => {}
    }
    let Some(rx) = state.action_rx.as_ref() else {
        return;
    };
    let outcome = match rx.try_recv() {
        Ok(outcome) => outcome,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => PoolActionOutcome::Reset(Err(LargeModelsError::WorkerLost)),
    };
    state.action_rx = None;
    state.action_status = Some(match outcome {
        PoolActionOutcome::Unloaded(Ok(keys)) => tf!("ai_backend.large_models.unloaded_status", count = keys.len()),
        PoolActionOutcome::Reset(Ok(cleared)) => tf!("ai_backend.large_models.reset_status", count = cleared),
        PoolActionOutcome::Unloaded(Err(err)) | PoolActionOutcome::Reset(Err(err)) => {
            ms_log::runtime_log::log_warn(format!("[ai_backend_panel] large-model pool action failed: {err}"));
            tf!("ai_backend.large_models.action_error", err = err.user_message())
        }
    });
}

/// Starts one pool action on a worker (at most one at a time).
fn start_action(state: &mut LargeModelsPanelState, unload: bool) {
    if state.action_rx.is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    state.action_rx = Some(rx);
    state.action_status = None;
    ms_thread::spawn(move || {
        let outcome = if unload {
            PoolActionOutcome::Unloaded(large_models::unload(None))
        } else {
            PoolActionOutcome::Reset(large_models::reset_learned(None))
        };
        // A failed send means the pane was dropped meanwhile: nobody waits for the answer.
        let _ = tx.send(outcome);
    });
}

/// Draws «Большие модели»: the pool entries, the two caps and the two pool actions.
///
/// Everything editable needs a running backend (it is the caps' only writer); offline the
/// section says so and draws no editor. Re-paints itself while an edit waits for its quiet
/// time or an action is in flight.
pub fn draw_large_models_section(ui: &mut egui::Ui, state: &mut LargeModelsPanelState, snapshot: &AiBackendHealthSnapshot) {
    poll(state);
    ui.separator();
    ui.label(t!("ai_backend.large_models.heading_label"));
    if !snapshot.connected {
        ui.small(t!("large_models.limits.offline_hint"));
        return;
    }
    match snapshot.large_pool.as_ref() {
        None => {}
        Some(Err(err)) => {
            ui.colored_label(ms_theme::status::ERROR, tf!("ai_backend.large_models.error_status", err = err.user_message()));
        }
        Some(Ok(pool)) => {
            if pool.entries.is_empty() {
                ui.small(t!("ai_backend.large_models.none_status"));
            }
            for entry in &pool.entries {
                ui.small(large_model_entry_line(entry));
            }
            if state.last_pushed != Some(pool.limits) {
                state.last_pushed = Some(pool.limits);
                if !state.writer.is_busy() {
                    state.limits_edit = pool.limits;
                }
            }
        }
    }

    let vram_edited = ui
        .add(ByteCapEdit::new(
            &mut state.limits_edit.vram_cap,
            "ai_backend.large_models.vram_cap",
            t!("large_models.limits.vram_cap_label"),
            t!("large_models.limits.unlimited_label"),
            t!("large_models.limits.gib_suffix"),
        ))
        .changed();
    let ram_edited = ui
        .add(ByteCapEdit::new(
            &mut state.limits_edit.ram_cap,
            "ai_backend.large_models.ram_cap",
            t!("large_models.limits.ram_cap_label"),
            t!("large_models.limits.unlimited_label"),
            t!("large_models.limits.gib_suffix"),
        ))
        .changed();
    // Only the axis the user touched is sent; the other travels as `Keep`, so an edit made
    // meanwhile in the cleaning panel is never undone by this pane's stale value.
    if vram_edited {
        state.writer.edit(CapChange::from_cap(state.limits_edit.vram_cap), CapChange::Keep, Instant::now());
    }
    if ram_edited {
        state.writer.edit(CapChange::Keep, CapChange::from_cap(state.limits_edit.ram_cap), Instant::now());
    }
    ui.small(t!("large_models.limits.scope_hint"));
    if let Some(error) = state.limits_error.as_deref() {
        ui.colored_label(ms_theme::status::ERROR, error);
    }

    let idle = state.action_rx.is_none();
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(idle, egui::Button::new(t!("ai_backend.large_models.unload_button"))).clicked() {
            start_action(state, true);
        }
        if ui.add_enabled(idle, egui::Button::new(t!("ai_backend.large_models.reset_learned_button"))).clicked() {
            start_action(state, false);
        }
        if !idle {
            ui.spinner();
        }
    });
    if let Some(status) = state.action_status.as_deref() {
        ui.small(status);
    }
    if state.writer.is_busy() || state.action_rx.is_some() {
        ui.ctx().request_repaint_after(LIMITS_EDIT_DEBOUNCE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_local_ai::large_models::{FailureKind, RamStep, VramStep};

    fn entry() -> LargeModelEntry {
        LargeModelEntry {
            model: "flux2_klein".to_string(),
            key: "flux2_klein@cuda:0".to_string(),
            format: Some("bf16".to_string()),
            device: Some("cuda:0".to_string()),
            vram_step: None,
            ram_step: None,
            device_bytes: 1 << 30,
            host_bytes: 0,
            in_use: false,
            degraded: false,
            degrade_reason: None,
        }
    }

    /// Without a catalog `t!` returns the key, so each part of the line is its key.
    #[test]
    fn an_entry_line_names_only_what_the_entry_has() {
        assert_eq!(large_model_entry_line(&entry()), "ai_backend.large_models.entry_status");
        let full = LargeModelEntry {
            vram_step: Some(VramStep::Phase),
            ram_step: Some(RamStep::Keep),
            in_use: true,
            degraded: true,
            degrade_reason: Some(FailureKind::DeviceOom),
            ..entry()
        };
        assert_eq!(
            large_model_entry_line(&full),
            "ai_backend.large_models.entry_status · ai_backend.large_models.steps_status · ai_backend.large_models.in_use_status · ai_backend.large_models.degraded_status"
        );
        let half = LargeModelEntry { vram_step: Some(VramStep::Min), ..entry() };
        assert_eq!(large_model_entry_line(&half), "ai_backend.large_models.entry_status", "one step alone is not a pair");
    }
}
