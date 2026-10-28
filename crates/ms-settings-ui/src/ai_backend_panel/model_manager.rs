/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/model_manager.rs
The model manager of the AI pane: the shared «Модели в памяти» limit drawn above the tabs
(persisted off-thread; it caps both small-model LRUs — the native ONNX one reads it, the
backend gets it via `device.set` / start) and the «Большие модели» section of the
«Основной PyTorch» tab (`crate::large_models_panel`).

Key functions:
- `draw_shared_model_limit`: seeds the limit once from the off-thread config read and renders
  the slider plus the backend's live limit.
- `draw_model_manager`: renders the large-model section.
- `spawn_save_max_loaded_models`: persists the limit on a worker.

Notes:
Desktop only (declared under `cfg(not(target_arch = "wasm32"))` in `mod.rs`); the web
build draws its backend-driven slider in `onnx_selection.rs`. sd.cpp holds at most one model
and ignores the limit.
*/

use super::AiBackendPanelState;
use super::ort_build::start_onnx_config_read;
use crate::ai_backend_supervisor::AiBackendHandle;
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use ms_widgets::WheelSlider;

/// The shared «Модели в памяти» slider (`General.ai_max_loaded_models`, 1..=10), drawn above
/// the pane's tabs because ONE value caps both small-model LRUs (backend and native ONNX).
/// The value is seeded once from the off-thread config read (a spinner until it lands), a
/// change is persisted on a worker and sent to a connected backend, and the backend's live
/// limit is shown while connected.
pub(super) fn draw_shared_model_limit(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
) {
    ui.heading(t!("ai_backend.shared_models_heading"));
    start_onnx_config_read(state);
    // Seed the model limit once from config (u32 default is 0, out of the 1..=10 range).
    if !(1..=10).contains(&state.selected_max_loaded_models) {
        let seed = state.onnx_config.lock().ok().and_then(|guard| guard.as_ref().map(|read| read.max_loaded_models));
        match seed {
            Some(value) => state.selected_max_loaded_models = value,
            None => {
                ui.spinner();
                return;
            }
        }
    }
    let slider_response = ui
        .add(
            WheelSlider::new(&mut state.selected_max_loaded_models, 1..=10)
                .text(t!("ai_backend.max_loaded_models_label")),
        )
        // The count applies to the small pools only; the large models are budgeted by bytes
        // in the «Основной PyTorch» tab, whose label the hint names.
        .on_hover_text(tf!("ai_backend.max_loaded_models_shared_hint", tab = t!("ai_backend.tab_pytorch")));
    if slider_response.changed() {
        spawn_save_max_loaded_models(state.selected_max_loaded_models);
        if snapshot.connected {
            handle.send_probe(AiBackendProbeCommand::SetMaxLoadedModels(
                state.selected_max_loaded_models,
            ));
        }
    }
    if snapshot.connected {
        ui.small(tf!("ai_backend.backend_current_limit", snapshot = snapshot.max_loaded_models));
    }
}

/// The «Большие модели» section of the «Основной PyTorch» tab: the backend's large-model
/// pool, its byte caps and actions (`crate::large_models_panel`).
pub(super) fn draw_model_manager(ui: &mut egui::Ui, state: &mut AiBackendPanelState, snapshot: &AiBackendHealthSnapshot) {
    ui.separator();
    ui.label(t!("ai_backend.model_manager_heading"));
    crate::large_models_panel::draw_large_models_section(ui, &mut state.large_models, snapshot);
}

/// Persists the model limit off the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_save_max_loaded_models(value: u32) {
    let path = ms_config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-max-models-save".to_string())
        .spawn(move || {
            if let Err(err) = ms_config::save_max_loaded_models(&path, value) {
                ms_log::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ai_max_loaded_models={value}: {err}"
                ));
            }
        })
    {
        ms_log::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start max-models save thread: {err}"
        ));
    }
}
