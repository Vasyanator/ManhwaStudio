/*
FILE OVERVIEW: src/ai_backend_panel.rs
Shared AI backend settings widget used by BOTH the studio settings tab and the
launcher settings page.

Why this is shared:
The backend is now app-global (see `crate::ai_backend_supervisor`), so its status,
process controls, and device/ONNX selection must be reachable from the launcher as
well as the studio. This module renders that one panel against an
[`AiBackendHandle`] (the shared snapshots + command channels) plus a per-UI
[`AiBackendPanelState`] holding only local selection scratch fields, so the two
call sites stay in sync without duplicating the UI.

Key items:
- `AiBackendPanelState`: per-UI selection scratch (device/provider/onnx/max-models).
- `draw_ai_backend_panel`: renders health, process controls, device selection and
  CUDA/ROCm diagnostics.
*/

use crate::ai_backend_supervisor::{AiBackendHandle, AiBackendProcessCommand};
use crate::backend_ipc;
use crate::tabs::translation::backend_health::AiBackendProbeCommand;
use crate::widgets::WheelComboBox;

/// Per-UI scratch state for the shared panel. Each call site owns one instance so
/// the studio and launcher panels can hold independent in-progress selections.
#[derive(Debug, Default)]
pub struct AiBackendPanelState {
    pub selected_backend_device: String,
    pub selected_onnx_provider: String,
    pub selected_onnx_device_id: String,
    pub selected_max_loaded_models: u32,
    pub requested_initial_device_refresh: bool,
}

pub fn draw_ai_backend_panel(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
) {
    let snapshot = handle.health_snapshot();
    let process = handle.process_snapshot();
    let ai_enabled = handle.ai_enabled;

    ui.label(format!(
        "Адрес сервиса (сокет): {}",
        backend_ipc::backend_socket_path().display()
    ));

    if !ai_enabled {
        ui.colored_label(
            egui::Color32::from_rgb(225, 180, 60),
            "Статус: отключено (--no-ai)",
        );
    } else if snapshot.connected {
        ui.colored_label(egui::Color32::from_rgb(42, 168, 88), "Статус: подключено");
    } else {
        ui.colored_label(egui::Color32::from_rgb(208, 84, 62), "Статус: недоступно");
    }

    ui.label(format!("Детали: {}", snapshot.details));
    if let Some(checked_at) = snapshot.checked_at {
        ui.small(format!(
            "Последняя проверка: {} сек назад",
            checked_at.elapsed().as_secs()
        ));
    } else {
        ui.small("Последняя проверка: ещё не выполнялась");
    }

    if ui
        .add_enabled(ai_enabled, egui::Button::new("Проверить сейчас"))
        .clicked()
    {
        handle.send_probe(AiBackendProbeCommand::CheckNow);
    }

    ui.separator();
    ui.heading("Запуск backend");
    if !ai_enabled {
        ui.small("Запуск процесса отключен флагом --no-ai.");
    }
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                ai_enabled && !process.running(),
                egui::Button::new("Запустить"),
            )
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Start);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
        if ui
            .add_enabled(
                ai_enabled && process.running(),
                egui::Button::new("Остановить"),
            )
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Stop);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
        if ui
            .add_enabled(ai_enabled, egui::Button::new("Перезапустить"))
            .clicked()
        {
            handle.send_process(AiBackendProcessCommand::Restart);
            handle.send_probe(AiBackendProbeCommand::CheckNow);
        }
    });

    let mut auto_start = process.auto_start();
    if ui
        .add_enabled(
            ai_enabled,
            egui::Checkbox::new(&mut auto_start, "Запускать автоматически"),
        )
        .changed()
    {
        handle.send_process(AiBackendProcessCommand::SetAutoStart(auto_start));
    }

    if process.running() {
        ui.colored_label(egui::Color32::from_rgb(42, 168, 88), "Процесс: запущен");
    } else {
        ui.colored_label(egui::Color32::from_rgb(208, 84, 62), "Процесс: остановлен");
    }
    ui.small(format!("Статус процесса: {}", process.status()));
    if let Some(updated_at) = process.updated_at() {
        ui.small(format!(
            "Последнее событие процесса: {} сек назад",
            updated_at.elapsed().as_secs()
        ));
    } else {
        ui.small("Событий процесса пока не было.");
    }

    ui.label("Вывод backend:");
    egui::ScrollArea::vertical()
        .id_salt("ai_backend_process_logs")
        .max_height(220.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if process.logs().is_empty() {
                ui.small("Лог пуст.");
            } else {
                for line in process.logs() {
                    ui.monospace(line);
                }
            }
        });

    ui.separator();
    ui.heading("Устройство вычислений");

    if !ai_enabled {
        ui.small("Управление устройством отключено флагом --no-ai.");
    } else {
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

        let onnx_provider_needs_reset = state.selected_onnx_provider.trim().is_empty()
            || !snapshot
                .available_onnx_providers
                .iter()
                .any(|item| item == &state.selected_onnx_provider);
        if onnx_provider_needs_reset {
            if let Some(current) = snapshot.selected_onnx_provider.as_ref() {
                state.selected_onnx_provider = current.clone();
            } else if let Some(first) = snapshot.available_onnx_providers.first() {
                state.selected_onnx_provider = first.clone();
            }
        }

        let current_onnx_device_options = snapshot
            .onnx_devices_by_provider
            .get(state.selected_onnx_provider.as_str())
            .cloned()
            .unwrap_or_else(|| snapshot.onnx_device_options.clone());

        let onnx_device_needs_reset = state.selected_onnx_device_id.trim().is_empty()
            || !current_onnx_device_options
                .iter()
                .any(|item| item.id == state.selected_onnx_device_id);
        if onnx_device_needs_reset {
            if state.selected_onnx_provider
                == snapshot.selected_onnx_provider.clone().unwrap_or_default()
            {
                if let Some(current) = snapshot.selected_onnx_device_id.as_ref() {
                    state.selected_onnx_device_id = current.clone();
                } else if let Some(first) = current_onnx_device_options.first() {
                    state.selected_onnx_device_id = first.id.clone();
                }
            } else if let Some(first) = current_onnx_device_options.first() {
                state.selected_onnx_device_id = first.id.clone();
            } else if let Some(current) = snapshot.selected_onnx_device_id.as_ref() {
                state.selected_onnx_device_id = current.clone();
            }
        }

        let max_loaded_models = snapshot.max_loaded_models.clamp(1, 10);
        if !(1..=10).contains(&state.selected_max_loaded_models) {
            state.selected_max_loaded_models = max_loaded_models;
        }

        ui.horizontal_wrapped(|ui| {
            let selected_text = if state.selected_backend_device.trim().is_empty() {
                "нет данных".to_string()
            } else {
                snapshot
                    .device_options
                    .iter()
                    .find(|option| option.id == state.selected_backend_device)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| state.selected_backend_device.clone())
            };

            ui.add_enabled_ui(snapshot.connected && torch_available, |ui| {
                WheelComboBox::from_label("Устройство PyTorch")
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
                    egui::Button::new("Обновить список"),
                )
                .clicked()
            {
                handle.send_probe(AiBackendProbeCommand::RefreshDeviceInfo);
            }

            let can_apply = snapshot.connected
                && torch_available
                && !state.selected_backend_device.trim().is_empty()
                && snapshot.selected_device.as_deref()
                    != Some(state.selected_backend_device.as_str());
            if ui
                .add_enabled(can_apply, egui::Button::new("Установить"))
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
            ui.small(format!("Текущее устройство PyTorch: {current_label}"));
        }
        ui.small(format!("PyTorch: {}", snapshot.device_details));
        if !torch_available {
            ui.colored_label(
                egui::Color32::from_rgb(240, 102, 102),
                "PyTorch не установлен",
            );
        }

        ui.horizontal_wrapped(|ui| {
            let selected_provider = if state.selected_onnx_provider.trim().is_empty() {
                "нет данных".to_string()
            } else {
                state.selected_onnx_provider.clone()
            };

            WheelComboBox::from_label("Провайдер ONNX")
                .selected_text(selected_provider)
                .show_ui(ui, |ui| {
                    for provider in &snapshot.available_onnx_providers {
                        ui.selectable_value(
                            &mut state.selected_onnx_provider,
                            provider.clone(),
                            provider.as_str(),
                        );
                    }
                });

            let can_apply_provider = snapshot.connected
                && !state.selected_onnx_provider.trim().is_empty()
                && (snapshot.selected_onnx_provider.as_deref()
                    != Some(state.selected_onnx_provider.as_str()));
            if ui
                .add_enabled(can_apply_provider, egui::Button::new("Применить провайдер"))
                .clicked()
            {
                handle.send_probe(AiBackendProbeCommand::SetOnnxDevice {
                    provider: state.selected_onnx_provider.clone(),
                    device_id: state.selected_onnx_device_id.clone(),
                });
            }
        });

        ui.horizontal_wrapped(|ui| {
            let selected_text = if state.selected_onnx_device_id.trim().is_empty() {
                "нет данных".to_string()
            } else {
                snapshot
                    .onnx_devices_by_provider
                    .get(state.selected_onnx_provider.as_str())
                    .unwrap_or(&snapshot.onnx_device_options)
                    .iter()
                    .find(|option| option.id == state.selected_onnx_device_id)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| state.selected_onnx_device_id.clone())
            };

            WheelComboBox::from_label("Устройство ONNX")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for option in snapshot
                        .onnx_devices_by_provider
                        .get(state.selected_onnx_provider.as_str())
                        .unwrap_or(&snapshot.onnx_device_options)
                    {
                        ui.selectable_value(
                            &mut state.selected_onnx_device_id,
                            option.id.clone(),
                            option.label.as_str(),
                        );
                    }
                });

            let can_apply_device = snapshot.connected
                && !state.selected_onnx_provider.trim().is_empty()
                && !state.selected_onnx_device_id.trim().is_empty()
                && (snapshot.selected_onnx_provider.as_deref()
                    != Some(state.selected_onnx_provider.as_str())
                    || snapshot.selected_onnx_device_id.as_deref()
                        != Some(state.selected_onnx_device_id.as_str()));
            if ui
                .add_enabled(can_apply_device, egui::Button::new("Установить ONNX"))
                .clicked()
            {
                handle.send_probe(AiBackendProbeCommand::SetOnnxDevice {
                    provider: state.selected_onnx_provider.clone(),
                    device_id: state.selected_onnx_device_id.clone(),
                });
            }
        });

        if let Some(current_provider) = snapshot.selected_onnx_provider.as_ref() {
            let current_device = snapshot
                .selected_onnx_device_id
                .clone()
                .unwrap_or_else(|| "0".to_string());
            let current_label = snapshot
                .onnx_device_options
                .iter()
                .find(|option| option.id == current_device.as_str())
                .map(|option| option.label.clone())
                .unwrap_or(current_device);
            ui.small(format!("Текущий ONNX: {current_provider} / {current_label}"));
        }
        ui.small(format!("ONNX: {}", snapshot.onnx_details));
        ui.separator();
        ui.label("Менеджер моделей");
        let slider_response = ui.add_enabled(
            snapshot.connected,
            egui::Slider::new(&mut state.selected_max_loaded_models, 1..=10)
                .text("Максимум одновременно загруженных моделей"),
        );
        if slider_response.changed() {
            handle.send_probe(AiBackendProbeCommand::SetMaxLoadedModels(
                state.selected_max_loaded_models,
            ));
        }
        ui.small(format!(
            "Текущий лимит в backend: {}",
            snapshot.max_loaded_models
        ));
        if let Some(checked_at) = snapshot.device_checked_at {
            ui.small(format!(
                "Последнее обновление списка устройств: {} сек назад",
                checked_at.elapsed().as_secs()
            ));
        } else {
            ui.small("Список устройств ещё не запрашивался.");
        }
    }

    ui.separator();
    ui.heading("Диагностика CUDA/ROCm");
    if ui
        .add_enabled(
            ai_enabled && snapshot.connected,
            egui::Button::new("Проверить CUDA/ROCm"),
        )
        .clicked()
    {
        handle.send_probe(AiBackendProbeCommand::RefreshCudaDiagnostics);
    }
    if let Some(checked_at) = snapshot.cuda_checked_at {
        ui.small(format!(
            "Последняя диагностика: {} сек назад",
            checked_at.elapsed().as_secs()
        ));
    } else {
        ui.small("Диагностика ещё не запускалась.");
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
            "Проверка соединения, запуск процесса и вывод логов выполняются в отдельных потоках.",
        );
    } else {
        ui.small("Проверка отключена флагом --no-ai.");
    }
}
