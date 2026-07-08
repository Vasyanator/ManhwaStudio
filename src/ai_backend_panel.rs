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
use crate::tabs::translation::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use crate::widgets::WheelComboBox;
#[cfg(not(target_arch = "wasm32"))]
use crate::onnx_runtime::{OrtDownloadProgress, OrtDownloadStage};
#[cfg(not(target_arch = "wasm32"))]
use crate::tabs::translation::backend_health::AiBackendDeviceOption;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

/// Per-UI scratch state for the shared panel. Each call site owns one instance so
/// the studio and launcher panels can hold independent in-progress selections.
#[derive(Debug, Default)]
pub struct AiBackendPanelState {
    pub selected_backend_device: String,
    pub selected_onnx_provider: String,
    pub selected_onnx_device_id: String,
    pub selected_max_loaded_models: u32,
    pub requested_initial_device_refresh: bool,
    /// Lazily-read AI runtime selection (`General.ai_runtime`). `None` until the
    /// one-shot background read completes, so the GUI thread never reads config
    /// directly. Desktop-only feature; unused on the web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub ai_runtime_selection: std::sync::Arc<std::sync::Mutex<Option<crate::config::AiRuntime>>>,
    /// Whether the one-shot background read of `ai_runtime_selection` has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub ai_runtime_read_started: bool,
    /// Lazily-probed ONNX capabilities (system CUDA + DirectML adapters) used to
    /// build the OFFLINE provider/device list. `None` until the one-shot background
    /// probe completes; the GUI thread never runs the blocking probes directly.
    /// Desktop-only feature; unused on the web build.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_caps: std::sync::Arc<std::sync::Mutex<Option<OnnxCaps>>>,
    /// Whether the one-shot background ONNX capability probe has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_caps_probe_started: bool,
    /// Lazily-read unified ONNX selection + model limit from config, used to seed the
    /// combos offline. `None` until the one-shot background read completes.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_config: std::sync::Arc<std::sync::Mutex<Option<OnnxConfigRead>>>,
    /// Whether the one-shot background ONNX config read has started.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_config_read_started: bool,
    /// Whether the in-memory ONNX provider/device selection has been seeded once from
    /// the persisted config. Seeding happens exactly once so a backend-only provider
    /// (e.g. MIGraphX) chosen by the user survives a transient backend outage — the
    /// selection is not reset just because the union temporarily drops it.
    #[cfg(not(target_arch = "wasm32"))]
    pub onnx_selection_seeded: bool,
    /// Progress/result of the auto-download of the onnxruntime dylib for the selected
    /// provider (Task 4). Written by a worker thread, rendered on the AI pane.
    #[cfg(not(target_arch = "wasm32"))]
    pub ort_download: std::sync::Arc<std::sync::Mutex<OrtDownloadStatus>>,
}

/// Locally-probed ONNX capabilities driving the OFFLINE provider/device list.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default)]
pub struct OnnxCaps {
    /// Whether the system CUDA 12.x/cuDNN 9.x runtime is present (gates CUDA).
    pub cuda_available: bool,
    /// DirectML accelerator NAMES (Windows); the Vec position is the adapter index.
    pub directml_accelerators: Vec<String>,
}

/// The unified ONNX selection + model limit read once from config to seed the UI.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default)]
pub struct OnnxConfigRead {
    /// Persisted ORT provider token (`General.ai_onnx_provider`), if any.
    pub provider_token: Option<String>,
    /// Persisted adapter index string (`General.ai_onnx_device_id`), if any.
    pub device_id: Option<String>,
    /// Persisted model limit (`General.ai_max_loaded_models`), clamped 1..=10.
    pub max_loaded_models: u32,
}

/// Progress/result of the onnxruntime dylib auto-download for one provider.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct OrtDownloadStatus {
    /// The `ExecutionProvider::id` currently being (or last) downloaded.
    pub provider_id: Option<&'static str>,
    /// Latest progress snapshot reported by the worker.
    pub progress: Option<crate::onnx_runtime::OrtDownloadProgress>,
    /// Whether a download worker is currently running.
    pub running: bool,
    /// Whether the last download finished successfully.
    pub done: bool,
    /// Error text when the last download failed.
    pub error: Option<String>,
}

/// One selectable ONNX execution provider in the UNIFIED list (local-native ∪
/// backend-reported), tracking both native and backend facts so the label,
/// usability, device sourcing, and auto-download can be resolved per active runtime.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnnxProviderOption {
    /// ORT provider token (e.g. `"DmlExecutionProvider"`); persisted + sent to backend.
    token: String,
    /// Human-readable base label (availability suffix added at render time).
    label: String,
    /// Whether the token maps to a real native execution provider the in-process ONNX
    /// path can load (CPU/DirectML/CoreML/CUDA). CPU counts as native-capable;
    /// backend-only providers (e.g. MIGraphX, ROCm) do NOT.
    native_capable: bool,
    /// Whether the native provider can actually run locally right now (CPU always;
    /// DirectML iff a DirectML adapter exists; CUDA iff the system CUDA runtime is
    /// present; CoreML on macOS). Only meaningful when `native_capable`.
    native_available: bool,
    /// Whether the connected Python backend reported this provider in
    /// `available_onnx_providers` (always `false` when the backend is offline).
    backend_available: bool,
    /// Selectable adapter devices (id = adapter index string). Native-capable providers
    /// use the local probe devices; backend-only providers use the backend-reported
    /// device list.
    devices: Vec<OnnxDeviceOptionUi>,
}

/// One selectable ONNX device (accelerator adapter) in the offline list.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnnxDeviceOptionUi {
    /// Adapter index as a string (matches the backend `device_id` contract).
    id: String,
    /// Human-readable device label.
    label: String,
}

pub fn draw_ai_backend_panel(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
) {
    let snapshot = handle.health_snapshot();
    let process = handle.process_snapshot();
    let ai_enabled = handle.ai_enabled;

    // Native ONNX runtime selector (desktop-only; the native path depends on
    // `ms-onnx`/`ort`, compiled out on the web build).
    #[cfg(not(target_arch = "wasm32"))]
    draw_ai_runtime_section(ui, state);

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

    // PyTorch device selection: Torch models ALWAYS run on the Python backend (some
    // models are not ONNX-exportable), so this stays backend-gated.
    draw_torch_device_controls(ui, handle, state, &snapshot, ai_enabled);

    // ONNX provider/device + model limit. On desktop this is populated from local
    // OS/GPU capabilities and stays usable even without a running backend; the same
    // selection drives the native path (config on load) and the backend (device.set
    // when connected).
    draw_onnx_and_models_section(ui, handle, state, &snapshot, ai_enabled);

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
        ui.small("Управление устройством PyTorch отключено флагом --no-ai.");
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
            && snapshot.selected_device.as_deref() != Some(state.selected_backend_device.as_str());
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
        ui.colored_label(egui::Color32::from_rgb(240, 102, 102), "PyTorch не установлен");
    }
}

/// Desktop ONNX provider/device + model-limit section: OFFLINE-capable.
///
/// The provider/device lists come from local OS + `gpu_utils` capability probes
/// (NOT the backend health snapshot), so they are usable even without a running
/// backend. A selection is persisted to the unified config keys off-thread AND — when
/// the backend is connected — pushed live via `device.set`. When the native runtime
/// is active, the selected provider's onnxruntime dylib is auto-downloaded in the
/// background.
#[cfg(not(target_arch = "wasm32"))]
fn draw_onnx_and_models_section(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    _ai_enabled: bool,
) {
    start_onnx_caps_probe(state);
    start_onnx_config_read(state);

    let caps = state.onnx_caps.lock().ok().and_then(|guard| guard.clone());
    let config_read = state.onnx_config.lock().ok().and_then(|guard| guard.clone());

    ui.separator();
    ui.label("ONNX Runtime (нативный и бэкенд)");

    let (Some(caps), Some(config_read)) = (caps, config_read) else {
        ui.small("Загрузка возможностей ONNX Runtime…");
        return;
    };

    // Unify the local-native capability set with the backend-reported providers so a
    // backend-only accelerator (e.g. MIGraphX/ROCm on AMD) stays selectable for the
    // Python backend even though there is no native EP for it.
    let backend = BackendOnnxProviders {
        connected: snapshot.connected,
        providers: &snapshot.available_onnx_providers,
        devices_by_provider: &snapshot.onnx_devices_by_provider,
        generic_devices: &snapshot.onnx_device_options,
    };
    let options = build_onnx_provider_options(
        cfg!(target_os = "windows"),
        cfg!(target_os = "macos"),
        &caps.directml_accelerators,
        caps.cuda_available,
        &backend,
    );

    // The active runtime decides which providers are usable and how each is labelled;
    // read once (may still be `None` while the background read is in flight).
    let runtime = state
        .ai_runtime_selection
        .lock()
        .ok()
        .and_then(|guard| *guard);

    // Seed the model limit once from config (u32 default is 0, out of the 1..=10 range).
    if !(1..=10).contains(&state.selected_max_loaded_models) {
        state.selected_max_loaded_models = config_read.max_loaded_models;
    }
    reconcile_onnx_selection(state, &options, &config_read);

    let prev_provider = state.selected_onnx_provider.clone();
    let prev_device = state.selected_onnx_device_id.clone();

    ui.horizontal_wrapped(|ui| {
        let selected_label = options
            .iter()
            .find(|option| option.token == state.selected_onnx_provider)
            .map(|option| provider_display_label(option, runtime, snapshot.connected))
            .unwrap_or_else(|| state.selected_onnx_provider.clone());
        WheelComboBox::from_label("Провайдер ONNX")
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                for option in &options {
                    let label = provider_display_label(option, runtime, snapshot.connected);
                    ui.selectable_value(&mut state.selected_onnx_provider, option.token.clone(), label);
                }
            });
    });

    // A provider change resets the device to the new provider's first adapter so the
    // device combo below never shows a stale id.
    if state.selected_onnx_provider != prev_provider
        && let Some(first) = options
            .iter()
            .find(|option| option.token == state.selected_onnx_provider)
            .and_then(|option| option.devices.first())
    {
        state.selected_onnx_device_id = first.id.clone();
    }

    let devices = options
        .iter()
        .find(|option| option.token == state.selected_onnx_provider)
        .map(|option| option.devices.clone())
        .unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        let selected_label = devices
            .iter()
            .find(|device| device.id == state.selected_onnx_device_id)
            .map(|device| device.label.clone())
            .unwrap_or_else(|| state.selected_onnx_device_id.clone());
        ui.add_enabled_ui(devices.len() > 1, |ui| {
            WheelComboBox::from_label("Устройство ONNX")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for device in &devices {
                        ui.selectable_value(
                            &mut state.selected_onnx_device_id,
                            device.id.clone(),
                            device.label.as_str(),
                        );
                    }
                });
        });
    });

    let selection_changed = state.selected_onnx_provider != prev_provider
        || state.selected_onnx_device_id != prev_device;
    if selection_changed {
        // Persist to the unified config keys (native path reads them on load).
        spawn_save_onnx_provider_device(
            state.selected_onnx_provider.clone(),
            state.selected_onnx_device_id.clone(),
        );
        // Apply live to the running backend, if any.
        if snapshot.connected {
            handle.send_probe(AiBackendProbeCommand::SetOnnxDevice {
                provider: state.selected_onnx_provider.clone(),
                device_id: state.selected_onnx_device_id.clone(),
            });
        }
    }

    let selected_option = options
        .iter()
        .find(|option| option.token == state.selected_onnx_provider);

    // Explain, per active runtime, why the selected provider may not run as-is.
    if let (Some(option), Some(runtime)) = (selected_option, runtime) {
        match runtime {
            crate::config::AiRuntime::Native => {
                if !option.native_capable {
                    // Backend-only provider (e.g. MIGraphX): the native path has no EP
                    // for it and the token maps to CPU, so native ONNX falls back to CPU.
                    ui.small(
                        "Этот провайдер поддерживается только ИИ-бэкендом. Нативный ONNX Runtime \
                         его не загружает и при запуске откатится на CPU; библиотека для него не \
                         скачивается. Выберите его при рантайме «Через ИИ-бэкенд».",
                    );
                } else if !option.native_available {
                    ui.small(
                        "Выбранный ONNX-провайдер недоступен на этой системе. Нативный ONNX при \
                         запуске выберет запасной провайдер (DirectML/CPU); библиотека не \
                         скачивается.",
                    );
                }
            }
            crate::config::AiRuntime::Backend => {
                if !snapshot.connected {
                    ui.small(
                        "ИИ-бэкенд офлайн: выбор вступит в силу после запуска бэкенда.",
                    );
                } else if !option.backend_available {
                    ui.small(
                        "Выбранный провайдер сейчас не сообщён ИИ-бэкендом и недоступен.",
                    );
                }
            }
        }
    }

    if let Some(current_provider) = snapshot.selected_onnx_provider.as_ref() {
        let current_device = snapshot
            .selected_onnx_device_id
            .clone()
            .unwrap_or_else(|| "0".to_string());
        ui.small(format!("Бэкенд ONNX: {current_provider} / {current_device}"));
    }
    ui.small(format!("ONNX: {}", snapshot.onnx_details));

    // Auto-download the selected provider's onnxruntime dylib while the native runtime
    // is active — ONLY for a native-capable, locally-available provider. A backend-only
    // provider (e.g. MIGraphX) has no native onnxruntime binary to fetch, so skip it
    // (the note above already tells the user the native path falls back to CPU).
    if runtime == Some(crate::config::AiRuntime::Native)
        && let Some(option) = selected_option
        && option.native_capable
        && option.native_available
    {
        let provider = crate::native_runtime::execution_provider_from_ort_token(&option.token);
        maybe_start_ort_download(state, provider);
    }

    ui.separator();
    ui.label("Менеджер моделей");
    // The model limit is editable offline (persisted to config; native LRU reads it,
    // and the backend picks it up via device.set/start).
    let slider_response = ui.add(
        egui::Slider::new(&mut state.selected_max_loaded_models, 1..=10)
            .text("Максимум одновременно загруженных моделей"),
    );
    if slider_response.changed() {
        spawn_save_max_loaded_models(state.selected_max_loaded_models);
        if snapshot.connected {
            handle.send_probe(AiBackendProbeCommand::SetMaxLoadedModels(
                state.selected_max_loaded_models,
            ));
        }
    }
    if snapshot.connected {
        ui.small(format!("Текущий лимит в backend: {}", snapshot.max_loaded_models));
    }
}

/// Web build: the native ONNX path does not exist, so the ONNX provider/device
/// combos and the model-limit slider stay backend-driven (unchanged behavior).
#[cfg(target_arch = "wasm32")]
fn draw_onnx_and_models_section(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    ai_enabled: bool,
) {
    if !ai_enabled {
        return;
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
        if let Some(first) = current_onnx_device_options.first() {
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
            && !state.selected_onnx_device_id.trim().is_empty();
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
    ui.small(format!("Текущий лимит в backend: {}", snapshot.max_loaded_models));
}

/// Backend-reported ONNX providers + device lists folded into the offline native
/// capability set to form the unified provider list. Borrowed straight from the health
/// snapshot; `connected` gates whether the backend's list is authoritative.
#[cfg(not(target_arch = "wasm32"))]
struct BackendOnnxProviders<'a> {
    /// Whether the Python backend is currently connected (its provider list is only
    /// folded in / authoritative while connected).
    connected: bool,
    /// ORT provider tokens the backend reported (`available_onnx_providers`).
    providers: &'a [String],
    /// Backend per-provider device lists (`onnx_devices_by_provider`).
    devices_by_provider: &'a HashMap<String, Vec<AiBackendDeviceOption>>,
    /// Backend generic device-list fallback (`onnx_device_options`).
    generic_devices: &'a [AiBackendDeviceOption],
}

/// Builds the UNIFIED ONNX provider option list: the local-native capability set
/// (CPU everywhere; DirectML on Windows; Core ML on macOS; CUDA on non-macOS) UNIONED
/// with the backend-reported providers when the backend is connected, deduped by ORT
/// token (CPU appears once). Pure so the union/labelling is unit-testable.
///
/// Native-set members carry `native_capable = true` with `native_available` per the
/// local probe (DirectML iff an adapter exists, CUDA iff the system runtime is present)
/// and local probe devices. Backend-only providers (not in the local native set, e.g.
/// MIGraphX/ROCm) carry `native_capable = false`, `native_available = false`, and the
/// backend-reported device list; they remain SHOWN and selectable so an AMD/ROCm user
/// can pick them for backend ONNX. `backend_available` is set for any token the
/// connected backend reported.
#[cfg(not(target_arch = "wasm32"))]
fn build_onnx_provider_options(
    is_windows: bool,
    is_macos: bool,
    directml_accelerators: &[String],
    cuda_available: bool,
    backend: &BackendOnnxProviders<'_>,
) -> Vec<OnnxProviderOption> {
    let mut options = vec![OnnxProviderOption {
        token: "CPUExecutionProvider".to_string(),
        label: provider_base_label("CPUExecutionProvider"),
        native_capable: true,
        native_available: true,
        backend_available: false,
        devices: vec![OnnxDeviceOptionUi {
            id: "0".to_string(),
            label: "CPU".to_string(),
        }],
    }];

    if is_windows {
        let adapter_devices: Vec<OnnxDeviceOptionUi> = directml_accelerators
            .iter()
            .enumerate()
            .map(|(index, name)| OnnxDeviceOptionUi {
                id: index.to_string(),
                label: format!("{index}: {name}"),
            })
            .collect();
        let (available, devices) = if adapter_devices.is_empty() {
            // No adapter detected: still list DirectML (marked unavailable) with a
            // placeholder device so the combo has a valid id.
            (
                false,
                vec![OnnxDeviceOptionUi {
                    id: "0".to_string(),
                    label: "GPU 0".to_string(),
                }],
            )
        } else {
            (true, adapter_devices)
        };
        options.push(OnnxProviderOption {
            token: "DmlExecutionProvider".to_string(),
            label: provider_base_label("DmlExecutionProvider"),
            native_capable: true,
            native_available: available,
            backend_available: false,
            devices,
        });
    }

    if is_macos {
        options.push(OnnxProviderOption {
            token: "CoreMLExecutionProvider".to_string(),
            label: provider_base_label("CoreMLExecutionProvider"),
            native_capable: true,
            native_available: true,
            backend_available: false,
            devices: vec![OnnxDeviceOptionUi {
                id: "0".to_string(),
                label: "По умолчанию".to_string(),
            }],
        });
    } else {
        options.push(OnnxProviderOption {
            token: "CUDAExecutionProvider".to_string(),
            label: provider_base_label("CUDAExecutionProvider"),
            native_capable: true,
            native_available: cuda_available,
            backend_available: false,
            devices: vec![OnnxDeviceOptionUi {
                id: "0".to_string(),
                label: "GPU 0".to_string(),
            }],
        });
    }

    // Mark which local-native providers the connected backend also reports.
    for option in &mut options {
        option.backend_available =
            backend.connected && backend.providers.iter().any(|p| p.trim() == option.token);
    }

    // Fold in backend-only providers (only while connected — offline they cannot be
    // enumerated, and the union collapses to the local-native set).
    if backend.connected {
        for token in backend.providers {
            let token = token.trim();
            if token.is_empty() || options.iter().any(|option| option.token == token) {
                continue;
            }
            options.push(OnnxProviderOption {
                token: token.to_string(),
                label: provider_base_label(token),
                native_capable: token_is_native_capable(token),
                // A backend-only provider is, by construction, not part of the local
                // native set for this OS, so the native path cannot run it locally.
                native_available: false,
                backend_available: true,
                devices: backend_devices_for(token, backend),
            });
        }
    }

    options
}

/// Whether an ORT provider token maps to a real native execution provider the
/// in-process ONNX path can load. CPU counts; any token that maps to the CPU FALLBACK
/// (unknown/backend-only, e.g. MIGraphX/ROCm) is NOT native-capable.
#[cfg(not(target_arch = "wasm32"))]
fn token_is_native_capable(token: &str) -> bool {
    let token = token.trim();
    token == "CPUExecutionProvider"
        || crate::native_runtime::execution_provider_from_ort_token(token)
            != ms_onnx::ExecutionProvider::Cpu
}

/// The backend-reported device list for `token` (its `onnx_devices_by_provider` entry,
/// else the generic `onnx_device_options`), with a placeholder so the device combo
/// always has a valid id.
#[cfg(not(target_arch = "wasm32"))]
fn backend_devices_for(token: &str, backend: &BackendOnnxProviders<'_>) -> Vec<OnnxDeviceOptionUi> {
    let source = backend
        .devices_by_provider
        .get(token)
        .map(Vec::as_slice)
        .filter(|options| !options.is_empty())
        .unwrap_or(backend.generic_devices);
    let devices: Vec<OnnxDeviceOptionUi> = source
        .iter()
        .map(|device| OnnxDeviceOptionUi {
            id: device.id.clone(),
            label: device.label.clone(),
        })
        .collect();
    if devices.is_empty() {
        vec![OnnxDeviceOptionUi {
            id: "0".to_string(),
            label: "0".to_string(),
        }]
    } else {
        devices
    }
}

/// Human-readable base label for a known ORT provider token, else the token with its
/// `ExecutionProvider` suffix stripped.
#[cfg(not(target_arch = "wasm32"))]
fn provider_base_label(token: &str) -> String {
    match token.trim() {
        "CPUExecutionProvider" => "CPU".to_string(),
        "DmlExecutionProvider" => "DirectML (GPU)".to_string(),
        "CUDAExecutionProvider" => "CUDA (GPU)".to_string(),
        "CoreMLExecutionProvider" => "Core ML".to_string(),
        "MIGraphXExecutionProvider" => "MIGraphX (AMD ROCm)".to_string(),
        "ROCMExecutionProvider" => "ROCm (AMD)".to_string(),
        other => other
            .strip_suffix("ExecutionProvider")
            .filter(|stripped| !stripped.is_empty())
            .unwrap_or(other)
            .to_string(),
    }
}

/// The default ONNX provider token when none is selected/valid: a native-available
/// DirectML adapter (matching the backend's Windows default), else CPU (always present
/// and available).
#[cfg(not(target_arch = "wasm32"))]
fn default_onnx_provider(options: &[OnnxProviderOption]) -> String {
    if let Some(dml) = options
        .iter()
        .find(|option| option.token == "DmlExecutionProvider" && option.native_available)
    {
        return dml.token.clone();
    }
    "CPUExecutionProvider".to_string()
}

/// A provider's usability + label suffix under the ACTIVE runtime.
///
/// Native: usable iff native-capable AND locally available; a native-capable but
/// unavailable provider is marked `(недоступно)`; a backend-only provider is marked
/// `(только ИИ-бэкенд)` (still selectable — native ONNX falls back to CPU for it).
/// Backend: usable iff the connected backend reports it; connected-but-unreported →
/// `(недоступно)`; offline → no suffix (a general note explains the choice applies
/// once the backend starts).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderRuntimeState {
    /// Whether the provider can actually run under the active runtime right now.
    usable: bool,
    /// Label suffix to append (availability marker), if any.
    suffix: Option<&'static str>,
}

/// Computes the [`ProviderRuntimeState`] for `option` under `runtime`. `backend_connected`
/// distinguishes "backend up but does not offer this provider" from "backend offline".
#[cfg(not(target_arch = "wasm32"))]
fn provider_runtime_state(
    option: &OnnxProviderOption,
    runtime: crate::config::AiRuntime,
    backend_connected: bool,
) -> ProviderRuntimeState {
    match runtime {
        crate::config::AiRuntime::Native => {
            if !option.native_capable {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some("(только ИИ-бэкенд)"),
                }
            } else if option.native_available {
                ProviderRuntimeState {
                    usable: true,
                    suffix: None,
                }
            } else {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some("(недоступно)"),
                }
            }
        }
        crate::config::AiRuntime::Backend => {
            if option.backend_available {
                ProviderRuntimeState {
                    usable: true,
                    suffix: None,
                }
            } else if backend_connected {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some("(недоступно)"),
                }
            } else {
                ProviderRuntimeState {
                    usable: false,
                    suffix: None,
                }
            }
        }
    }
}

/// The provider label shown in the combo: the base label plus the active runtime's
/// availability suffix (see [`provider_runtime_state`]). When the runtime is not yet
/// known (`None`) the base label is shown without a suffix.
#[cfg(not(target_arch = "wasm32"))]
fn provider_display_label(
    option: &OnnxProviderOption,
    runtime: Option<crate::config::AiRuntime>,
    backend_connected: bool,
) -> String {
    let suffix =
        runtime.and_then(|runtime| provider_runtime_state(option, runtime, backend_connected).suffix);
    match suffix {
        Some(suffix) => format!("{} {suffix}", option.label),
        None => option.label.clone(),
    }
}

/// Reconciles the in-memory provider/device selection against `options`.
///
/// Seeds the provider/device ONCE from the persisted config, independent of current
/// availability — so a backend-only provider (e.g. MIGraphX) the user chose survives a
/// transient backend outage that temporarily drops it from the union. After seeding, if
/// the selected provider is present in `options`, the device is validated (re-seeded
/// from config or the provider's first device); if the provider is currently absent
/// (e.g. a backend-only provider while the backend is offline) the selection is left
/// intact so it reappears when the provider returns.
#[cfg(not(target_arch = "wasm32"))]
fn reconcile_onnx_selection(
    state: &mut AiBackendPanelState,
    options: &[OnnxProviderOption],
    config_read: &OnnxConfigRead,
) {
    if !state.onnx_selection_seeded {
        state.onnx_selection_seeded = true;
        state.selected_onnx_provider = config_read
            .provider_token
            .as_ref()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| default_onnx_provider(options));
        state.selected_onnx_device_id = config_read.device_id.clone().unwrap_or_default();
    }

    // Only validate the device when the selected provider is currently enumerated;
    // otherwise keep the persisted selection so it survives the provider's absence.
    let Some(option) = options
        .iter()
        .find(|option| option.token == state.selected_onnx_provider)
    else {
        return;
    };
    let device_valid = option
        .devices
        .iter()
        .any(|device| device.id == state.selected_onnx_device_id);
    if !device_valid {
        let chosen = config_read
            .device_id
            .as_ref()
            .filter(|id| option.devices.iter().any(|device| &device.id == *id))
            .cloned()
            .or_else(|| option.devices.first().map(|device| device.id.clone()));
        if let Some(id) = chosen {
            state.selected_onnx_device_id = id;
        }
    }
}

/// Starts the one-shot background ONNX capability probe (system CUDA + DirectML
/// adapters). The blocking probes never run on the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn start_onnx_caps_probe(state: &mut AiBackendPanelState) {
    if state.onnx_caps_probe_started {
        return;
    }
    state.onnx_caps_probe_started = true;
    let slot = state.onnx_caps.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-caps-probe".to_string())
        .spawn(move || {
            let cuda_available = crate::gpu_utils::native_cuda_runtime_available();
            let directml_accelerators = crate::gpu_utils::detect_directml_accelerators_windows()
                .into_iter()
                .map(|adapter| adapter.name)
                .collect::<Vec<_>>();
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(OnnxCaps {
                    cuda_available,
                    directml_accelerators,
                });
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX capability probe thread: {err}"
        ));
    }
}

/// Starts the one-shot background read of the unified ONNX selection + model limit
/// from config so the GUI thread never reads config directly.
#[cfg(not(target_arch = "wasm32"))]
fn start_onnx_config_read(state: &mut AiBackendPanelState) {
    if state.onnx_config_read_started {
        return;
    }
    state.onnx_config_read_started = true;
    let slot = state.onnx_config.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-config-read".to_string())
        .spawn(move || {
            let cfg = crate::config::load_raw_user_settings_for_startup()
                .unwrap_or(serde_json::Value::Null);
            let read = OnnxConfigRead {
                provider_token: crate::config::ai_onnx_provider_token_from_user_settings(&cfg),
                device_id: crate::config::ai_onnx_device_id_from_user_settings(&cfg),
                max_loaded_models: crate::config::ai_max_loaded_models_from_user_settings(&cfg),
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(read);
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX config read thread: {err}"
        ));
    }
}

/// Russian label for an onnxruntime download stage.
#[cfg(not(target_arch = "wasm32"))]
fn ort_stage_label_ru(stage: OrtDownloadStage) -> &'static str {
    match stage {
        OrtDownloadStage::Probing => "Проверка",
        OrtDownloadStage::Downloading => "Скачивание",
        OrtDownloadStage::Verifying => "Проверка целостности",
        OrtDownloadStage::Extracting => "Распаковка",
        OrtDownloadStage::Done => "Готово",
    }
}

/// Starts a background onnxruntime dylib download for `provider`, unless one is
/// already running or this exact provider has already finished/errored.
///
/// Never dlopens/loads here — it only fetches the library so the native OCR load (via
/// `native_runtime`, still under the SIGILL guard) finds it ready. Worker-thread only.
#[cfg(not(target_arch = "wasm32"))]
fn maybe_start_ort_download(state: &AiBackendPanelState, provider: ms_onnx::ExecutionProvider) {
    let provider_id = provider.id();
    {
        let mut status = match state.ort_download.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // One download at a time; and do not re-run a completed/errored provider (a
        // provider re-select changes provider_id and resets this below).
        if status.running {
            return;
        }
        if status.provider_id == Some(provider_id) && (status.done || status.error.is_some()) {
            return;
        }
        status.provider_id = Some(provider_id);
        status.progress = None;
        status.done = false;
        status.error = None;
        status.running = true;
    }

    let slot = state.ort_download.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("ort-download".to_string())
        .spawn(move || {
            let mut report = |progress: OrtDownloadProgress| {
                if let Ok(mut status) = slot.lock() {
                    status.progress = Some(progress);
                }
            };
            let result = crate::onnx_runtime::resolve_or_download_ort_dylib(provider, &mut report);
            let mut status = match slot.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            status.running = false;
            match result {
                Ok(_path) => status.done = true,
                Err(err) => {
                    status.error = Some(err.to_string());
                    crate::runtime_log::log_warn(format!(
                        "[ai-backend-panel] onnxruntime download failed for '{provider_id}': {err}"
                    ));
                }
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ORT download thread: {err}"
        ));
        // Clear the running flag so a later attempt can retry.
        if let Ok(mut status) = state.ort_download.lock() {
            status.running = false;
        }
    }
}

/// Renders the onnxruntime auto-download progress/result (a determinate
/// `ProgressBar` when the total size is known, otherwise a spinner + stage text).
#[cfg(not(target_arch = "wasm32"))]
fn draw_ort_download_progress(ui: &mut egui::Ui, state: &AiBackendPanelState) {
    let (progress, running, done, error) = match state.ort_download.lock() {
        Ok(status) => (status.progress, status.running, status.done, status.error.clone()),
        Err(poisoned) => {
            let status = poisoned.into_inner();
            (status.progress, status.running, status.done, status.error.clone())
        }
    };

    if let Some(error) = error {
        ui.colored_label(
            egui::Color32::from_rgb(208, 84, 62),
            format!("Не удалось подготовить ONNX Runtime: {error}"),
        );
        return;
    }

    if running || progress.is_some() {
        match progress {
            Some(progress) => {
                let stage = ort_stage_label_ru(progress.stage);
                match progress.total {
                    Some(total) if total > 0 => {
                        // Display-only ratio; the f64/f32 casts cannot meaningfully
                        // lose precision for a 0..=1 progress fraction.
                        let fraction =
                            (progress.downloaded.min(total) as f64 / total as f64) as f32;
                        let percent = (fraction * 100.0) as u32;
                        ui.add(egui::ProgressBar::new(fraction).text(format!("{stage} {percent}%")));
                    }
                    Some(_) | None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{stage}…"));
                        });
                    }
                }
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Подготовка ONNX Runtime…");
                });
            }
        }
    } else if done {
        ui.colored_label(
            egui::Color32::from_rgb(42, 168, 88),
            "Библиотека ONNX Runtime готова.",
        );
    }
}

/// Persists the unified ONNX selection off the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_save_onnx_provider_device(provider_token: String, device_id: String) {
    let path = crate::config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-selection-save".to_string())
        .spawn(move || {
            if let Err(err) =
                crate::tabs::settings::save_onnx_provider_device(&path, &provider_token, &device_id)
            {
                crate::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ONNX selection '{provider_token}'/'{device_id}': {err}"
                ));
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ONNX selection save thread: {err}"
        ));
    }
}

/// Persists the model limit off the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_save_max_loaded_models(value: u32) {
    let path = crate::config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("onnx-max-models-save".to_string())
        .spawn(move || {
            if let Err(err) = crate::tabs::settings::save_max_loaded_models(&path, value) {
                crate::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ai_max_loaded_models={value}: {err}"
                ));
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start max-models save thread: {err}"
        ));
    }
}

/// Renders the "Рантайм ИИ" section: the Python-backend / native-ONNX runtime
/// selector and the "Повторить попытку ORT" control.
///
/// The current runtime is read once off the GUI thread into `state`; changes are
/// persisted off-thread. Desktop-only (the native runtime is compiled out on wasm).
#[cfg(not(target_arch = "wasm32"))]
fn draw_ai_runtime_section(ui: &mut egui::Ui, state: &mut AiBackendPanelState) {
    use crate::config::AiRuntime;

    ui.heading("ONNX-инференс");
    ui.small(
        "Выберите, где выполнять ONNX-модели: через ИИ-бэкенд (Python) или нативно \
         (Rust ONNX Runtime, прямо в приложении). Нативный путь сейчас покрывает OCR MangaOCR \
         и PaddleOCR, а также детекцию текста PaddleOCR; остальные ONNX-операции продолжают идти \
         через бэкенд.",
    );
    ui.small(
        "Torch-модели всегда выполняются на ИИ-бэкенде (часть моделей не экспортируется в ONNX).",
    );
    ui.small(
        "Смена рантайма или провайдера вступает в силу только после перезапуска приложения: \
         окружение и библиотека onnxruntime фиксируются в процессе один раз.",
    );

    // Kick off a one-shot background read of the current runtime so the GUI thread
    // never reads config directly.
    if !state.ai_runtime_read_started {
        state.ai_runtime_read_started = true;
        let slot = state.ai_runtime_selection.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("ai-runtime-read".to_string())
            .spawn(move || {
                let cfg = crate::config::load_raw_user_settings_for_startup()
                    .unwrap_or(serde_json::Value::Null);
                let runtime = AiRuntime::from_user_settings(&cfg);
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(runtime);
                }
            })
        {
            crate::runtime_log::log_error(format!(
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
            ui.small("Загрузка текущего рантайма…");
        }
        Some(runtime) => {
            let mut selected = runtime;
            WheelComboBox::from_label("ONNX-инференс")
                .selected_text(ai_runtime_label(selected))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut selected,
                        AiRuntime::Backend,
                        ai_runtime_label(AiRuntime::Backend),
                    );
                    ui.selectable_value(
                        &mut selected,
                        AiRuntime::Native,
                        ai_runtime_label(AiRuntime::Native),
                    );
                });
            if selected != runtime {
                if let Ok(mut guard) = state.ai_runtime_selection.lock() {
                    *guard = Some(selected);
                }
                spawn_save_ai_runtime(selected);
            }
        }
    }

    // onnxruntime auto-download progress (shown while/after the native runtime fetches
    // the library for the selected provider).
    draw_ort_download_progress(ui, state);

    if ui
        .button("Повторить попытку ORT")
        .on_hover_text(
            "Сбрасывает защиту после аварийной загрузки ONNX Runtime, чтобы снова попробовать \
             нативный рантайм без перезапуска приложения.",
        )
        .clicked()
    {
        spawn_reset_ort_guard();
    }

    ui.separator();
}

/// Human-readable Russian label for an ONNX-inference runtime choice.
#[cfg(not(target_arch = "wasm32"))]
fn ai_runtime_label(runtime: crate::config::AiRuntime) -> &'static str {
    match runtime {
        crate::config::AiRuntime::Backend => "Через ИИ-бэкенд (Python)",
        crate::config::AiRuntime::Native => "Нативно (Rust ONNX Runtime)",
    }
}

/// Persists the selected AI runtime off the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_save_ai_runtime(runtime: crate::config::AiRuntime) {
    let path = crate::config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("ai-runtime-save".to_string())
        .spawn(move || {
            if let Err(err) = crate::tabs::settings::save_ai_runtime(&path, runtime) {
                crate::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to persist ai_runtime '{}': {err}",
                    runtime.as_key()
                ));
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ai_runtime save thread: {err}"
        ));
    }
}

/// Resets the ONNX Runtime SIGILL load-guard for the EFFECTIVE native scope
/// (provider + adapter) — on disk and the in-process latch — so a retry can
/// re-attempt the native runtime without an app restart.
///
/// The guard scope is computed inside the worker thread via
/// `native_runtime::native_load_scope_key` (it does disk I/O + a CUDA probe on first
/// call), never on the GUI thread.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_reset_ort_guard() {
    // Reset the in-process latch first so a retry re-attempts loading.
    crate::native_runtime::reset_load_latch();

    let path = crate::config::user_config_path();
    if let Err(err) = std::thread::Builder::new()
        .name("ort-guard-reset".to_string())
        .spawn(move || {
            let scope = crate::native_runtime::native_load_scope_key();
            if let Err(err) = crate::tabs::settings::reset_ort_load_guard(&path, &scope) {
                crate::runtime_log::log_error(format!(
                    "[ai-backend-panel] failed to reset ORT load guard for '{scope}': {err}"
                ));
            }
        })
    {
        crate::runtime_log::log_error(format!(
            "[ai-backend-panel] failed to start ORT guard reset thread: {err}"
        ));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{
        BackendOnnxProviders, OnnxProviderOption, build_onnx_provider_options,
        default_onnx_provider, provider_runtime_state,
    };
    use crate::config::AiRuntime;
    use crate::tabs::translation::backend_health::AiBackendDeviceOption;
    use std::collections::HashMap;

    /// An offline backend (no reported providers): the union collapses to the
    /// local-native set. Kept as a helper so the OS-shape tests read clearly.
    fn offline_backend() -> BackendOnnxProviders<'static> {
        BackendOnnxProviders {
            connected: false,
            providers: &[],
            devices_by_provider: EMPTY_DEVICES_BY_PROVIDER.get_or_init(HashMap::new),
            generic_devices: &[],
        }
    }

    /// Shared empty device map so `offline_backend` can hand out a `'static` borrow.
    static EMPTY_DEVICES_BY_PROVIDER: std::sync::OnceLock<
        HashMap<String, Vec<AiBackendDeviceOption>>,
    > = std::sync::OnceLock::new();

    #[test]
    fn windows_lists_cpu_directml_cuda_with_availability() {
        // Two DirectML adapters present; CUDA runtime absent; backend offline.
        let options = build_onnx_provider_options(
            true,
            false,
            &["NVIDIA RTX".to_string(), "AMD Radeon".to_string()],
            false,
            &offline_backend(),
        );
        let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
        assert_eq!(
            tokens,
            vec![
                "CPUExecutionProvider",
                "DmlExecutionProvider",
                "CUDAExecutionProvider"
            ]
        );

        let dml = options
            .iter()
            .find(|o| o.token == "DmlExecutionProvider")
            .expect("directml listed on windows");
        assert!(dml.native_capable && dml.native_available);
        assert_eq!(dml.devices.len(), 2);
        assert_eq!(dml.devices[0].id, "0");
        assert_eq!(dml.devices[1].id, "1");

        // CUDA is listed but marked unavailable natively (no system runtime).
        let cuda = options
            .iter()
            .find(|o| o.token == "CUDAExecutionProvider")
            .expect("cuda listed on windows");
        assert!(cuda.native_capable && !cuda.native_available);

        // CPU is always native-available; nothing is backend-available while offline.
        let cpu = options
            .iter()
            .find(|o| o.token == "CPUExecutionProvider")
            .unwrap();
        assert!(cpu.native_capable && cpu.native_available);
        assert!(options.iter().all(|o| !o.backend_available));
    }

    #[test]
    fn windows_directml_without_adapters_stays_listed_but_unavailable() {
        let options = build_onnx_provider_options(true, false, &[], true, &offline_backend());
        let dml = options
            .iter()
            .find(|o| o.token == "DmlExecutionProvider")
            .expect("directml still listed");
        assert!(!dml.native_available);
        // A placeholder device keeps the combo id valid.
        assert!(!dml.devices.is_empty());
        // CUDA now available natively (system runtime present).
        assert!(
            options
                .iter()
                .find(|o| o.token == "CUDAExecutionProvider")
                .unwrap()
                .native_available
        );
    }

    #[test]
    fn linux_lists_cpu_and_cuda_only() {
        let options = build_onnx_provider_options(false, false, &[], true, &offline_backend());
        let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
        assert_eq!(tokens, vec!["CPUExecutionProvider", "CUDAExecutionProvider"]);
        assert!(
            options
                .iter()
                .find(|o| o.token == "CUDAExecutionProvider")
                .unwrap()
                .native_available
        );
    }

    #[test]
    fn macos_lists_cpu_and_coreml_only() {
        let options = build_onnx_provider_options(false, true, &[], false, &offline_backend());
        let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
        assert_eq!(
            tokens,
            vec!["CPUExecutionProvider", "CoreMLExecutionProvider"]
        );
        assert!(
            options
                .iter()
                .find(|o| o.token == "CoreMLExecutionProvider")
                .unwrap()
                .native_available
        );
    }

    #[test]
    fn default_provider_prefers_available_directml_else_cpu() {
        let win =
            build_onnx_provider_options(true, false, &["GPU".to_string()], false, &offline_backend());
        assert_eq!(default_onnx_provider(&win), "DmlExecutionProvider");

        // DirectML present but unavailable -> CPU.
        let win_no_adapter =
            build_onnx_provider_options(true, false, &[], false, &offline_backend());
        assert_eq!(default_onnx_provider(&win_no_adapter), "CPUExecutionProvider");

        let linux = build_onnx_provider_options(false, false, &[], true, &offline_backend());
        assert_eq!(default_onnx_provider(&linux), "CPUExecutionProvider");
    }

    /// Regression: a connected AMD/ROCm backend reporting MIGraphX must surface it in
    /// the union — backend-only, selectable under Backend, `(только ИИ-бэкенд)` under
    /// Native — with its device list sourced from the backend. CPU must not duplicate.
    #[test]
    fn migraphx_backend_provider_unions_in_as_backend_only() {
        let mut devices_by_provider: HashMap<String, Vec<AiBackendDeviceOption>> = HashMap::new();
        devices_by_provider.insert(
            "MIGraphXExecutionProvider".to_string(),
            vec![AiBackendDeviceOption {
                id: "0".to_string(),
                label: "0: AMD Radeon RX 7900".to_string(),
            }],
        );
        let providers = [
            "CPUExecutionProvider".to_string(),
            "CUDAExecutionProvider".to_string(),
            "MIGraphXExecutionProvider".to_string(),
        ];
        let backend = BackendOnnxProviders {
            connected: true,
            providers: &providers,
            devices_by_provider: &devices_by_provider,
            generic_devices: &[],
        };
        // Linux build (non-macOS, non-Windows): native set is CPU + CUDA.
        let options = build_onnx_provider_options(false, false, &[], true, &backend);

        // CPU appears exactly once (deduped across native + backend).
        assert_eq!(
            options
                .iter()
                .filter(|o| o.token == "CPUExecutionProvider")
                .count(),
            1
        );
        let cpu = options
            .iter()
            .find(|o| o.token == "CPUExecutionProvider")
            .unwrap();
        assert!(cpu.backend_available, "connected backend reports CPU");

        let migraphx = options
            .iter()
            .find(|o| o.token == "MIGraphXExecutionProvider")
            .expect("MIGraphX must appear in the union when the backend reports it");
        assert!(!migraphx.native_capable, "MIGraphX has no native EP");
        assert!(!migraphx.native_available);
        assert!(migraphx.backend_available);
        // Device list comes from the backend snapshot, not a native probe.
        assert_eq!(migraphx.devices.len(), 1);
        assert_eq!(migraphx.devices[0].label, "0: AMD Radeon RX 7900");

        // Under Native: shown, NOT usable, marked backend-only.
        let native = provider_runtime_state(migraphx, AiRuntime::Native, true);
        assert!(!native.usable);
        assert_eq!(native.suffix, Some("(только ИИ-бэкенд)"));

        // Under Backend (connected): usable, no suffix — the core regression fix.
        let backend_state = provider_runtime_state(migraphx, AiRuntime::Backend, true);
        assert!(backend_state.usable);
        assert_eq!(backend_state.suffix, None);
    }

    /// Offline: the union is exactly the local-native set (no backend-only providers
    /// can be enumerated), and under Backend every provider is non-usable with no hard
    /// `(недоступно)` suffix (a general note explains the choice applies once up).
    #[test]
    fn offline_backend_shows_only_local_native_set() {
        let migraphx = "MIGraphXExecutionProvider".to_string();
        let backend = BackendOnnxProviders {
            connected: false,
            providers: std::slice::from_ref(&migraphx),
            devices_by_provider: EMPTY_DEVICES_BY_PROVIDER.get_or_init(HashMap::new),
            generic_devices: &[],
        };
        let options = build_onnx_provider_options(false, false, &[], true, &backend);
        let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
        assert_eq!(tokens, vec!["CPUExecutionProvider", "CUDAExecutionProvider"]);
        assert!(options.iter().all(|o| !o.backend_available));

        let cpu = options
            .iter()
            .find(|o| o.token == "CPUExecutionProvider")
            .unwrap();
        let state = provider_runtime_state(cpu, AiRuntime::Backend, false);
        assert!(!state.usable);
        assert_eq!(state.suffix, None);
    }

    /// A native-capable but locally-unavailable provider (CUDA without the system
    /// runtime) is marked `(недоступно)` under the Native runtime.
    #[test]
    fn native_capable_unavailable_marked_unavailable_under_native() {
        let options = build_onnx_provider_options(false, false, &[], false, &offline_backend());
        let cuda = options
            .iter()
            .find(|o| o.token == "CUDAExecutionProvider")
            .unwrap();
        assert!(cuda.native_capable && !cuda.native_available);
        let state = provider_runtime_state(cuda, AiRuntime::Native, false);
        assert!(!state.usable);
        assert_eq!(state.suffix, Some("(недоступно)"));
    }

    /// A connected backend that does NOT report a native provider marks it
    /// `(недоступно)` under the Backend runtime (distinct from the offline case).
    #[test]
    fn connected_backend_unreported_provider_marked_unavailable() {
        let option = OnnxProviderOption {
            token: "CUDAExecutionProvider".to_string(),
            label: "CUDA (GPU)".to_string(),
            native_capable: true,
            native_available: true,
            backend_available: false,
            devices: Vec::new(),
        };
        let state = provider_runtime_state(&option, AiRuntime::Backend, true);
        assert!(!state.usable);
        assert_eq!(state.suffix, Some("(недоступно)"));
    }
}
