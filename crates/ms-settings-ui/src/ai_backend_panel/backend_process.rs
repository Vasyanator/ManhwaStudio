/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/backend_process.rs
The «Основной PyTorch» tab of the AI pane: everything about the Python backend process.

Key functions:
- `draw_pytorch_tab`: the backend's address and health, process control (start / stop /
  restart, autostart with its warning badge), the process log, the PyTorch device, the
  large-model section (`model_manager.rs`), the CUDA / ROCm diagnostics and the threads hint.
- `draw_torch_build_lines`: the backend's PyTorch build line and the missing-ROCm-kernel
  warning (texts owned by `crate::torch_build_text`).
- `draw_torch_device_controls`: the backend-gated PyTorch device selector, applied to the
  running backend via `device.set`.

Notes:
The tab is listed by `mod.rs` only where the platform has the Python backend; under
`--no-ai` it is drawn disabled. It reads the pushed health / process snapshots and sends
supervisor commands; no I/O on the GUI thread.
*/

use super::{AiBackendPanelOutcome, AiBackendPanelState};
#[cfg(not(target_arch = "wasm32"))]
use super::model_manager::draw_model_manager;
use crate::ai_backend_supervisor::{AiBackendHandle, AiBackendProcessCommand};
use crate::settings_shared::with_item_badge;
use crate::settings_warnings::{SettingChange, SettingKey, WarningSet};
use crate::torch_build_text;
use ms_backend_ipc as backend_ipc;
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use ms_widgets::WheelComboBox;

/// The «Основной PyTorch» tab: the Python backend's address and health, process control
/// (start / stop / restart, autostart), its log, the PyTorch device, the large-model pool,
/// the CUDA / ROCm diagnostics and the threads hint. Drawn only where the platform has the
/// Python backend; under `--no-ai` everything stays visible but disabled. The autostart
/// toggle is reported in `outcome` (the supervisor persists it on its own worker).
pub(super) fn draw_pytorch_tab(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    warnings: Option<&WarningSet>,
    outcome: &mut AiBackendPanelOutcome,
) {
    let process = handle.process_snapshot();
    let ai_enabled = handle.local_ai.backend.is_ok();
    // The tab exists only where the platform has the backend, so `--no-ai` is the only
    // thing left that forbids a spawn.
    let can_spawn = ai_enabled;

    ui.label(tf!("ai_backend.service_address_label", backend_ipc = backend_ipc::backend_socket_path().display()));

    if !ai_enabled {
        ui.colored_label(
            ms_theme::status::WARNING,
            t!("ai_backend.status_disabled_no_ai"),
        );
    } else if snapshot.connected {
        ui.colored_label(ms_theme::status::SUCCESS, t!("ai_backend.status_connected"));
    } else {
        ui.colored_label(ms_theme::status::ERROR, t!("ai_backend.status_unavailable"));
    }

    ui.label(tf!("ai_backend.status_details", snapshot = snapshot.details));
    if let Some(checked_at) = snapshot.checked_at {
        ui.small(tf!("ai_backend.last_check_seconds_ago", checked_at = checked_at.elapsed().as_secs()));
    } else {
        ui.small(t!("ai_backend.last_check_never"));
    }

    if ui
        .add_enabled(ai_enabled, egui::Button::new(t!("ai_backend.check_now_button")))
        .clicked()
    {
        handle.send_probe(AiBackendProbeCommand::CheckNow);
    }

    ui.separator();
    ui.heading(t!("ai_backend.process_launch_heading"));
    if !ai_enabled {
        ui.small(t!("ai_backend.process_launch_disabled_no_ai"));
    }
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                can_spawn && !process.running(),
                egui::Button::new(t!("ai_backend.start_button")),
            )
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Start);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
        if ui
            .add_enabled(
                ai_enabled && process.running(),
                egui::Button::new(t!("ai_backend.stop_button")),
            )
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Stop);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
        if ui
            .add_enabled(can_spawn, egui::Button::new(t!("ai_backend.restart_button")))
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Restart);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
    });

    let mut auto_start = process.auto_start();
    let autostart_response = with_item_badge(ui, warnings, SettingKey::BackendAutostart, |ui| {
        ui.add_enabled(
            ai_enabled,
            egui::Checkbox::new(&mut auto_start, t!("ai_backend.autostart_checkbox")),
        )
    });
    if autostart_response.changed() {
        handle.send_process(AiBackendProcessCommand::SetAutoStart(auto_start));
        // The supervisor persists the flag on its own worker; the change carries the new
        // value so a recheck does not depend on that write having landed yet.
        outcome.changed_settings.push(SettingChange::BackendAutostart(auto_start));
    }

    if process.running() {
        ui.colored_label(ms_theme::status::SUCCESS, t!("ai_backend.process_running"));
    } else {
        ui.colored_label(ms_theme::status::ERROR, t!("ai_backend.process_stopped"));
    }
    ui.small(tf!("ai_backend.process_status_label", process = process.status()));
    if let Some(updated_at) = process.updated_at() {
        ui.small(tf!("ai_backend.process_last_event_seconds_ago", updated_at = updated_at.elapsed().as_secs()));
    } else {
        ui.small(t!("ai_backend.process_no_events"));
    }

    ui.label(t!("ai_backend.output_heading"));
    egui::ScrollArea::vertical()
        .id_salt("ai_backend_process_logs")
        .max_height(220.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if process.logs().is_empty() {
                ui.small(t!("ai_backend.log_empty"));
            } else {
                for line in process.logs() {
                    ui.monospace(line);
                }
            }
        });

    ui.separator();
    ui.heading(t!("ai_backend.compute_device_heading"));

    // PyTorch device selection: Torch models ALWAYS run on the Python backend (some
    // models are not ONNX-exportable), so this stays backend-gated.
    draw_torch_device_controls(ui, handle, state, snapshot, ai_enabled);

    // The backend's large-model pool (caps, actions); the small-model limit is shared and
    // drawn above the tabs.
    #[cfg(not(target_arch = "wasm32"))]
    draw_model_manager(ui, state, snapshot);

    ui.separator();
    ui.heading(t!("ai_backend.cuda_rocm_diagnostics_heading"));
    draw_torch_build_lines(ui, snapshot);
    if ui
        .add_enabled(
            ai_enabled && snapshot.connected,
            egui::Button::new(t!("ai_backend.cuda_rocm_check_button")),
        )
        .clicked()
    {
        handle.send_probe(AiBackendProbeCommand::RefreshCudaDiagnostics);
    }
    if let Some(checked_at) = snapshot.cuda_checked_at {
        ui.small(tf!("ai_backend.last_diagnostics_seconds_ago", checked_at = checked_at.elapsed().as_secs()));
    } else {
        ui.small(t!("ai_backend.diagnostics_never"));
    }
    egui::ScrollArea::vertical()
        .id_salt("ai_backend_cuda_diagnostics")
        .max_height(220.0)
        .show(ui, |ui| {
            ui.monospace(snapshot.cuda_diagnostics.as_str());
        });

    ui.separator();
    if ai_enabled {
        ui.small(
            t!("ai_backend.threads_hint"),
        );
    } else {
        ui.small(t!("ai_backend.check_disabled_no_ai"));
    }

}

/// Draws the backend's PyTorch build (`AiBackendHealthSnapshot::torch_build`) as one line and,
/// below it, a warning when the main GPU's arch has no installed ROCm kernel wheel
/// (`TorchBuildInfo::kernel_mismatch`). Draws nothing while the build is not reported
/// (offline, warming up, no torch); an unreadable report is shown as such. The texts are
/// owned by `crate::torch_build_text` (shared with the launcher's system-information tab).
fn draw_torch_build_lines(ui: &mut egui::Ui, snapshot: &AiBackendHealthSnapshot) {
    match &snapshot.torch_build {
        None => {}
        Some(Err(err)) => {
            ui.colored_label(ms_theme::status::WARNING, torch_build_text::torch_build_unreadable_text(err));
        }
        Some(Ok(build)) => {
            ui.label(torch_build_text::torch_build_line(build));
            if let Some(warning) = torch_build_text::torch_build_kernel_warning(build) {
                ui.colored_label(ms_theme::status::WARNING, warning);
            }
        }
    }
}

/// Renders the PyTorch device selector (backend-gated).
///
/// Torch models always run on the Python backend, so this is shown only when the
/// backend is enabled and its controls are enabled only when connected + Torch is
/// available. The selection is applied to the running backend via `device.set`.
fn draw_torch_device_controls(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    ai_enabled: bool,
) {
    if !ai_enabled {
        ui.small(t!("ai_backend.pytorch_device_disabled_no_ai"));
        return;
    }

    let torch_available = snapshot.is_torch_available.unwrap_or(true);
    if snapshot.connected
        && snapshot.device_options.is_empty()
        && !state.requested_initial_device_refresh
    {
        state.requested_initial_device_refresh = true;
        handle.send_probe(AiBackendProbeCommand::RefreshDeviceInfo);
    }
    if !snapshot.connected {
        state.requested_initial_device_refresh = false;
    }

    let backend_needs_reset = state.selected_backend_device.trim().is_empty()
        || !snapshot
            .device_options
            .iter()
            .any(|item| item.id == state.selected_backend_device);
    if backend_needs_reset {
        if let Some(current) = snapshot.selected_device.as_ref() {
            state.selected_backend_device = current.clone();
        } else if let Some(first) = snapshot.device_options.first() {
            state.selected_backend_device = first.id.clone();
        }
    }

    ui.horizontal_wrapped(|ui| {
        let selected_text = if state.selected_backend_device.trim().is_empty() {
            t!("ai_backend.no_data").to_string()
        } else {
            snapshot
                .device_options
                .iter()
                .find(|option| option.id == state.selected_backend_device)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| state.selected_backend_device.clone())
        };

        ui.add_enabled_ui(snapshot.connected && torch_available, |ui| {
            WheelComboBox::from_label(t!("ai_backend.pytorch_device_combo_label")).id_salt("ai_backend.pytorch_device_combo_label")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for option in &snapshot.device_options {
                        ui.selectable_value(
                            &mut state.selected_backend_device,
                            option.id.clone(),
                            option.label.as_str(),
                        );
                    }
                });
        });

        if ui
            .add_enabled(
                snapshot.connected && torch_available,
                egui::Button::new(t!("ai_backend.refresh_list_button")),
            )
            .clicked()
        {
            handle.send_probe(AiBackendProbeCommand::RefreshDeviceInfo);
        }

        let can_apply = snapshot.connected
            && torch_available
            && !state.selected_backend_device.trim().is_empty()
            && snapshot.selected_device.as_deref() != Some(state.selected_backend_device.as_str());
        if ui
            .add_enabled(can_apply, egui::Button::new(t!("ai_backend.pytorch_set_device_button")))
            .clicked()
        {
            handle.send_probe(AiBackendProbeCommand::SetDevice(
                state.selected_backend_device.clone(),
            ));
        }
    });

    if let Some(current) = snapshot.selected_device.as_ref() {
        let current_label = snapshot
            .device_options
            .iter()
            .find(|option| &option.id == current)
            .map(|option| option.label.clone())
            .unwrap_or_else(|| current.clone());
        ui.small(tf!("ai_backend.pytorch_current_device", current_label = current_label));
    }
    ui.small(format!("PyTorch: {}", snapshot.device_details));
    if !torch_available {
        ui.colored_label(ms_theme::status::ERROR, t!("ai_backend.pytorch_not_installed"));
    }
}
