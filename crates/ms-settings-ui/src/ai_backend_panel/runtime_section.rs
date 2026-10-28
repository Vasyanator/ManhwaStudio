/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/runtime_section.rs
The "ONNX-инференс" section: the Python-backend / native-ONNX runtime selector.

Key functions:
- `draw_ai_runtime_section`: reads the effective runtime once off the GUI thread and
  renders the selector (no "Backend" choice where the platform lacks the backend).
- `ai_runtime_label`: the runtime choice label.
- `spawn_save_ai_runtime`: persists the choice on a worker and reports the landed change.

Notes:
Desktop only (declared under `cfg(not(target_arch = "wasm32"))` in `mod.rs`).
*/

use super::{AiBackendPanelState, report_landed};
use crate::settings_shared::with_item_badge;
use crate::settings_warnings::{SettingChange, SettingKey, WarningSet};
use ms_widgets::WheelComboBox;
use std::sync::mpsc::Sender;

/// Renders the "ONNX-инференс" section: the Python-backend / native-ONNX runtime
/// selector. The onnxruntime download progress and the build-action button live in the
/// native "Билд" section (they depend on the selected build).
///
/// The current runtime is read once off the GUI thread into `state` (the EFFECTIVE runtime,
/// `local_ai::effective_ai_runtime`); changes are persisted off-thread. With
/// `backend_on_platform == false` the "Backend" choice is not offered. Desktop-only (the
/// native runtime is compiled out on wasm).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_ai_runtime_section(
    ui: &mut egui::Ui,
    state: &mut AiBackendPanelState,
    warnings: Option<&WarningSet>,
    backend_on_platform: bool,
) {
    use ms_config::AiRuntime;

    ui.heading(t!("ai_backend.onnx_inference_heading"));
    ui.small(
        t!("ai_backend.onnx_inference_hint"),
    );
    ui.small(
        t!("ai_backend.torch_backend_only_hint"),
    );
    ui.small(
        t!("ai_backend.runtime_change_restart_hint"),
    );

    // Kick off a one-shot background read of the current runtime so the GUI thread
    // never reads config directly.
    if !state.ai_runtime_read_started {
        state.ai_runtime_read_started = true;
        let slot = state.ai_runtime_selection.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("ai-runtime-read".to_string())
            .spawn(move || {
                let cfg = ms_config::load_raw_user_settings_for_startup()
                    .unwrap_or(serde_json::Value::Null);
                // The effective runtime: a stored `Backend` reads as `Native` where the
                // platform has no Python backend (the same helper every router uses).
                let runtime = ms_local_ai::local_ai::effective_ai_runtime(&cfg);
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(runtime);
                }
            })
        {
            ms_log::runtime_log::log_error(format!(
                "[ai-backend-panel] failed to start AI runtime read thread: {err}"
            ));
        }
    }

    let current_runtime = state
        .ai_runtime_selection
        .lock()
        .ok()
        .and_then(|guard| *guard);
    match current_runtime {
        None => {
            ui.small(t!("ai_backend.runtime_loading"));
        }
        Some(runtime) => {
            let mut selected = runtime;
            with_item_badge(ui, warnings, SettingKey::AiRuntime, |ui| {
                WheelComboBox::from_label(t!("ai_backend.onnx_inference_combo_label")).id_salt("ai_backend.onnx_inference_combo_label")
                    .selected_text(ai_runtime_label(selected))
                    .show_ui(ui, |ui| {
                        // No "Backend" choice where the platform cannot run it.
                        if backend_on_platform {
                            ui.selectable_value(
                                &mut selected,
                                AiRuntime::Backend,
                                ai_runtime_label(AiRuntime::Backend),
                            );
                        }
                        ui.selectable_value(
                            &mut selected,
                            AiRuntime::Native,
                            ai_runtime_label(AiRuntime::Native),
                        );
                    })
            });
            if selected != runtime {
                if let Ok(mut guard) = state.ai_runtime_selection.lock() {
                    *guard = Some(selected);
                }
                spawn_save_ai_runtime(selected, state.pending_changes.sender());
            }
        }
    }

    // The onnxruntime auto-download progress bar and the build-action button
    // (retry / load-other-build / restart) live in the native "Билд" section below,
    // where the selected build is known.
    ui.separator();
}

/// Human-readable Russian label for an ONNX-inference runtime choice.
#[cfg(not(target_arch = "wasm32"))]
fn ai_runtime_label(runtime: ms_config::AiRuntime) -> &'static str {
    match runtime {
        ms_config::AiRuntime::Backend => t!("ai_backend.runtime_backend_option"),
        ms_config::AiRuntime::Native => t!("ai_backend.runtime_native_option"),
    }
}

/// Persists the selected AI runtime off the GUI thread, then reports
/// `SettingChange::AiRuntime` on `landed` (whatever the write's result).
#[cfg(not(target_arch = "wasm32"))]
fn spawn_save_ai_runtime(runtime: ms_config::AiRuntime, landed: Sender<SettingChange>) {
    let path = ms_config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("ai-runtime-save".to_string())
        .spawn(move || {
            if let Err(err) = ms_config::save_ai_runtime(&path, runtime) {
                ms_log::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ai_runtime '{}': {err}",
                    runtime.as_key()
                ));
            }
            report_landed(&landed, SettingChange::AiRuntime);
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ai_runtime save thread: {err}"
        ));
    }
}
