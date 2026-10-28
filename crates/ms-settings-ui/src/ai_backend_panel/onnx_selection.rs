/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/onnx_selection.rs
The ONNX part of the AI pane (the body of its «ONNXruntime» tab, below the runtime switch of
`runtime_section.rs`): the runtime-branched provider / build / EP / device pickers.

Key structures:
- `BackendOnnxProviders`: the backend-reported providers folded into the unified list.
- `ProviderRuntimeState`: a provider's usability + label suffix under the active runtime.

Key functions:
- `draw_onnx_section` (desktop and web variants): the section entry, branching on the
  active runtime. The web variant still draws its backend-driven model-limit slider; on
  desktop the limit is the shared one above the tabs (`model_manager.rs`).
- `draw_backend_provider_selection`: the UNIFIED provider/device combos (Backend runtime).
- `draw_native_build_selection`: the build -> EP -> device selection (Native runtime).
- `build_onnx_provider_options`, `provider_runtime_state`, `ep_device_options`: pure option
  logic (unit-tested in `tests.rs`); `reconcile_onnx_selection`, `reconcile_native_ep`: bring
  the panel's selection state back in line with the current options.

Notes:
The probes, config reads, ORT downloads and savers it starts live in `ort_build.rs`; the
build verdicts are `crate::native_builds`, the device-id rule `crate::onnx_caps`.
*/

use super::AiBackendPanelState;
#[cfg(not(target_arch = "wasm32"))]
use super::{OnnxDeviceOptionUi, OnnxProviderOption};
#[cfg(not(target_arch = "wasm32"))]
use super::OnnxConfigRead;
#[cfg(not(target_arch = "wasm32"))]
use super::ort_build::{
    build_dylib_present, draw_build_action_button, draw_ort_download_progress, ensure_build_presence_probe,
    maybe_start_ort_download, spawn_save_onnx_build, spawn_save_onnx_provider_device, start_onnx_caps_probe,
    start_onnx_config_read,
};
use crate::ai_backend_supervisor::AiBackendHandle;
#[cfg(not(target_arch = "wasm32"))]
use crate::settings_shared::with_item_badge;
#[cfg(not(target_arch = "wasm32"))]
use crate::settings_warnings::{SettingKey, item_warning_badge};
use crate::settings_warnings::WarningSet;
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use ms_widgets::WheelComboBox;
#[cfg(target_arch = "wasm32")]
use ms_widgets::WheelSlider;
#[cfg(not(target_arch = "wasm32"))]
use crate::native_builds::NativeBuildAvailability;
#[cfg(not(target_arch = "wasm32"))]
use crate::onnx_caps::{OnnxCaps, device_id_offered, ep_device_ids};
#[cfg(not(target_arch = "wasm32"))]
use ms_native_runtime::ort_provider_token;
#[cfg(not(target_arch = "wasm32"))]
use ms_local_ai::backend_health::AiBackendDeviceOption;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

/// Desktop ONNX selection section of the «ONNXruntime» tab: OFFLINE-capable, runtime-branched.
///
/// Reads the local OS + `gpu_utils` capability probes and the persisted config once
/// (both off-thread), then branches on the active `General.ai_runtime`:
/// - Native → the BUILD-based selection (Билд → EP → Устройство): the onnxruntime binary
///   is chosen from the `onnx_runtime::builds` catalog, the EP list comes from that
///   build, and the device combo adapts per EP. The selected build's dylib is
///   auto-downloaded in the background; a build change after the dylib is committed needs
///   an app restart (the process-global ort dylib cannot be hot-swapped).
/// - Backend (or runtime not yet known) → the UNIFIED provider/device combos
///   (local-native ∪ backend-reported, incl. MIGraphX), unchanged.
///
/// The backend's live ONNX status is shown under both branches. The model limit is not
/// here: it is shared by both runtimes and drawn above the tabs (`model_manager.rs`).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_onnx_section(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    _ai_enabled: bool,
    warnings: Option<&WarningSet>,
) {
    start_onnx_caps_probe(state);
    start_onnx_config_read(state);

    let caps = state.onnx_caps.lock().ok().and_then(|guard| guard.clone());
    let config_read = state.onnx_config.lock().ok().and_then(|guard| guard.clone());

    ui.separator();
    ui.label(t!("ai_backend.onnx_runtime_heading"));

    let (Some(caps), Some(config_read)) = (caps, config_read) else {
        ui.small(t!("ai_backend.onnx_caps_loading"));
        return;
    };

    // The active runtime decides which selection UI is shown; read once (may still be
    // `None` while the background read is in flight — treated as Backend-style).
    let runtime = state
        .ai_runtime_selection
        .lock()
        .ok()
        .and_then(|guard| *guard);

    match runtime {
        Some(ms_config::AiRuntime::Native) => {
            draw_native_build_selection(ui, handle, state, snapshot, &caps, &config_read, warnings);
        }
        Some(ms_config::AiRuntime::Backend) | None => {
            draw_backend_provider_selection(
                ui,
                handle,
                state,
                snapshot,
                &caps,
                &config_read,
                runtime,
            );
        }
    }

    // Backend's live ONNX selection + details, shown regardless of runtime (informational
    // when a backend is also connected).
    if let Some(current_provider) = snapshot.selected_onnx_provider.as_ref() {
        let current_device = snapshot
            .selected_onnx_device_id
            .clone()
            .unwrap_or_else(|| "0".to_string());
        ui.small(tf!("ai_backend.onnx_backend_current", current_provider = current_provider, current_device = current_device));
    }
    ui.small(format!("ONNX: {}", snapshot.onnx_details));
}

/// Backend/loading branch: the UNIFIED provider/device combos (local-native ∪
/// backend-reported). Unchanged behavior — a backend-only provider (e.g. MIGraphX)
/// stays selectable for the Python backend. `runtime` is `Some(Backend)` or `None`
/// (still loading); it only feeds the per-provider availability labels.
#[cfg(not(target_arch = "wasm32"))]
fn draw_backend_provider_selection(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    caps: &OnnxCaps,
    config_read: &OnnxConfigRead,
    runtime: Option<ms_config::AiRuntime>,
) {
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
        caps.webgpu_available,
        &caps.webgpu_adapters,
        &backend,
    );

    reconcile_onnx_selection(state, &options, config_read);

    let prev_provider = state.selected_onnx_provider.clone();
    let prev_device = state.selected_onnx_device_id.clone();

    ui.horizontal_wrapped(|ui| {
        let selected_label = options
            .iter()
            .find(|option| option.token == state.selected_onnx_provider)
            .map(|option| provider_display_label(option, runtime, snapshot.connected))
            .unwrap_or_else(|| state.selected_onnx_provider.clone());
        WheelComboBox::from_label(t!("ai_backend.onnx_provider_combo_label")).id_salt("ai_backend.onnx_provider_combo_label")
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
            WheelComboBox::from_label(t!("ai_backend.onnx_device_combo_label")).id_salt("ai_backend.onnx_device_combo_label")
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
            state.pending_changes.sender(),
            state.onnx_device_saved.clone(),
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

    // Explain, under the Backend runtime, why the selected provider may not run as-is.
    if let (Some(option), Some(ms_config::AiRuntime::Backend)) = (selected_option, runtime) {
        if !snapshot.connected {
            ui.small(t!("ai_backend.offline_selection_hint"));
        } else if !option.backend_available {
            ui.small(t!("ai_backend.provider_not_reported_hint"));
        }
    }
}

/// Native branch: the BUILD-based selection (Билд → EP → Устройство).
///
/// The onnxruntime binary is chosen from the `onnx_runtime::builds` catalog, grouped by
/// availability (Базовые / Специфичные / Недоступные); the EP list comes from the chosen
/// build; the device combo adapts per EP (including OpenVINO's device-type strings). The
/// selection is persisted to the unified config keys off-thread. An AVAILABLE build's
/// dylib is auto-downloaded (never a bundled one); the build-action button drives a force-download / retry /
/// restart per the committed-dylib state (see [`super::ort_build::ort_build_action`]).
///
/// `warnings` (launcher only) badges the build, EP, device and build-action rows. An
/// explicit device pick saves even when it re-picks the displayed device, as long as that
/// id differs from the persisted one the pane was seeded from: a persisted id that is no
/// longer offered is DISPLAYED as the first device but never saved by the seeding, and the
/// device warning asks the user to pick one. A flagged device combo also stays enabled with
/// a single device, so that pick is always possible.
#[cfg(not(target_arch = "wasm32"))]
fn draw_native_build_selection(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    caps: &OnnxCaps,
    config_read: &OnnxConfigRead,
    warnings: Option<&WarningSet>,
) {
    use ms_onnx_runtime::builds;

    let availability = NativeBuildAvailability::evaluate(&caps.native_facts());

    // Seed the build + EP + device ONCE from config (each validated against the catalog /
    // the build's EP set), independent of any value the Backend branch may have left in
    // the shared provider/device scratch on an earlier frame (before the runtime was
    // known). This keeps the native combos correct from the first native frame.
    if !state.onnx_build_seeded {
        state.onnx_build_seeded = true;
        let build = config_read
            .build
            .as_ref()
            .map(|slug| slug.trim().to_string())
            .filter(|slug| builds::build_by_slug(slug).is_some())
            .unwrap_or_else(|| builds::default_build_for_current_os().to_string());
        let eps = builds::build_execution_providers(&build);
        // EP: the persisted token if it belongs to the build, else the headline EP.
        state.selected_onnx_provider = config_read
            .provider_token
            .as_ref()
            .map(|token| token.trim())
            .filter(|token| eps.iter().any(|ep| ort_provider_token(*ep) == *token))
            .map(str::to_string)
            .or_else(|| eps.first().map(|ep| ort_provider_token(*ep).to_string()))
            .unwrap_or_default();
        // Device: the persisted id if valid for that EP, else the EP's first device.
        let ep = native_device_ep(&state.selected_onnx_provider);
        let devices = ep_device_options(ep, caps);
        state.selected_onnx_device_id = config_read
            .device_id
            .as_ref()
            .filter(|id| devices.iter().any(|device| &device.id == *id))
            .cloned()
            .or_else(|| devices.first().map(|device| device.id.clone()))
            .unwrap_or_default();
        state.selected_onnx_build = build;
    }

    let groups = availability.groups(ms_onnx_runtime::ort_downloads_offered_here());
    let prev_build = state.selected_onnx_build.clone();

    ui.horizontal_wrapped(|ui| {
        let selected_label = availability.display_label(&state.selected_onnx_build);
        WheelComboBox::from_label(t!("ai_backend.build_combo_label")).id_salt("ai_backend.build_combo_label")
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                render_build_group(ui, state, t!("ai_backend.build_group_basic"), &groups.basic, &availability);
                render_build_group(ui, state, t!("ai_backend.build_group_specific"), &groups.specific, &availability);
                // Unavailable builds sit at the bottom: still selectable for a forced
                // download, EXCEPT the informational QNN entry (no binary, no EP).
                render_build_group(ui, state, t!("ai_backend.build_group_unavailable"), &groups.unavailable, &availability);
            });
        // The hover response only carries the tooltip; nothing reacts to it.
        let _badge = item_warning_badge(ui, warnings, SettingKey::OnnxBuild);
    });
    ui.small(
        t!("ai_backend.build_hint"),
    );

    // A build change resets the EP to the build's headline (first) EP so the EP/device
    // combos never show a token that is not in the new build's set.
    if state.selected_onnx_build != prev_build
        && let Some(first) = builds::build_execution_providers(&state.selected_onnx_build).first()
    {
        state.selected_onnx_provider = ort_provider_token(*first).to_string();
        state.selected_onnx_device_id = ep_device_options(*first, caps)
            .first()
            .map(|device| device.id.clone())
            .unwrap_or_default();
    }

    // Coerce the selected EP into the current build's EP set (also seeds it the first
    // time from the persisted token when it belongs to the build).
    let build_slug = state.selected_onnx_build.clone();
    reconcile_native_ep(state, &build_slug, caps, config_read);

    let prev_provider = state.selected_onnx_provider.clone();
    let prev_device = state.selected_onnx_device_id.clone();

    // EP combo — one entry per EP the selected build registers.
    let eps = builds::build_execution_providers(&state.selected_onnx_build);
    ui.horizontal_wrapped(|ui| {
        let selected_label = eps
            .iter()
            .find(|ep| ort_provider_token(**ep) == state.selected_onnx_provider)
            .map(|ep| ep_display_label(*ep).to_string())
            .unwrap_or_else(|| state.selected_onnx_provider.clone());
        ui.add_enabled_ui(eps.len() > 1, |ui| {
            WheelComboBox::from_label("EP")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for ep in eps {
                        ui.selectable_value(
                            &mut state.selected_onnx_provider,
                            ort_provider_token(*ep).to_string(),
                            ep_display_label(*ep),
                        );
                    }
                });
        });
        let _badge = item_warning_badge(ui, warnings, SettingKey::OnnxProvider);
    });

    // An EP change resets the device to that EP's first option.
    if state.selected_onnx_provider != prev_provider {
        let ep = native_device_ep(&state.selected_onnx_provider);
        state.selected_onnx_device_id = ep_device_options(ep, caps)
            .first()
            .map(|device| device.id.clone())
            .unwrap_or_default();
    }

    // Device combo — per selected EP.
    let selected_ep =
        native_device_ep(&state.selected_onnx_provider);
    let devices = ep_device_options(selected_ep, caps);
    let device_flagged = warnings.and_then(|set| set.item_level(SettingKey::OnnxDevice)).is_some();
    let mut device_picked = false;
    ui.horizontal_wrapped(|ui| {
        let selected_label = devices
            .iter()
            .find(|device| device.id == state.selected_onnx_device_id)
            .map(|device| device.label.clone())
            .unwrap_or_else(|| state.selected_onnx_device_id.clone());
        // A flagged device stays pickable even when only one device is offered: the
        // warning asks for a pick, which is the only way to persist the displayed device.
        ui.add_enabled_ui(devices.len() > 1 || (device_flagged && !devices.is_empty()), |ui| {
            let combo = WheelComboBox::from_label(t!("ai_backend.build_device_combo_label")).id_salt("ai_backend.build_device_combo_label")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    // `selectable_value` leaves a click on the already-selected item
                    // unchanged and unmarked, so the click itself is collected here.
                    let mut clicked = false;
                    for device in &devices {
                        clicked |= ui
                            .selectable_value(
                                &mut state.selected_onnx_device_id,
                                device.id.clone(),
                                device.label.as_str(),
                            )
                            .clicked();
                    }
                    clicked
                });
            device_picked = combo.inner == Some(true);
        });
        let _badge = item_warning_badge(ui, warnings, SettingKey::OnnxDevice);
    });

    // Persist the EP/device selection to the unified config keys, and the build slug to
    // its own key, when either changed — or when the user explicitly picked a device that
    // the persisted config does not hold yet (see the function docs).
    let last_saved_device = match state.onnx_device_saved.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let selection_changed = state.selected_onnx_provider != prev_provider
        || state.selected_onnx_device_id != prev_device
        || device_pick_needs_save(
            device_picked,
            config_read.device_id.as_deref(),
            last_saved_device.as_deref(),
            &state.selected_onnx_device_id,
        );
    if selection_changed {
        spawn_save_onnx_provider_device(
            state.selected_onnx_provider.clone(),
            state.selected_onnx_device_id.clone(),
            state.pending_changes.sender(),
            state.onnx_device_saved.clone(),
        );
        if snapshot.connected {
            handle.send_probe(AiBackendProbeCommand::SetOnnxDevice {
                provider: state.selected_onnx_provider.clone(),
                device_id: state.selected_onnx_device_id.clone(),
            });
        }
    }
    if state.selected_onnx_build != prev_build {
        spawn_save_onnx_build(state.selected_onnx_build.clone(), state.pending_changes.sender());
    }

    // Refresh the presence probe for the selected build so the button state + the
    // auto-download gate use a fresh answer.
    ensure_build_presence_probe(state, &build_slug);
    let dylib_present = build_dylib_present(state, &build_slug);

    // Auto-download the selected build's dylib when it is AVAILABLE and not present yet.
    // Unavailable builds are only fetched via the explicit "Загрузить другую сборку"
    // button (forced download). A bundled build (Android) is never downloaded: its
    // library ships in the app package.
    let selected_available = availability.is_available(&build_slug);
    let bundled = ms_onnx_runtime::build_source_kind_here(&build_slug) == Some(ms_onnx_runtime::OrtBuildSourceKind::Bundled);
    if selected_available && !bundled && dylib_present == Some(false) {
        maybe_start_ort_download(state, &build_slug);
    }

    if !selected_available {
        ui.small(t!("ai_backend.build_unavailable_hint"));
    }

    // onnxruntime auto-download progress for the selected build (worker-reported).
    draw_ort_download_progress(ui, state);
    with_item_badge(ui, warnings, SettingKey::OrtCrashGuard, |ui| draw_build_action_button(ui, state, dylib_present, bundled));
}

/// Whether an explicit device pick must be saved although the selection did not change:
/// the user clicked an item (`picked`) and the picked id `selected` is not the persisted
/// one. The persisted id is `last_saved` (the last successful save from this pane) when
/// there is one, else `seed` (the one-shot config read; `None` when the key is absent).
/// This covers the "persisted id no longer offered, first device displayed but unsaved"
/// case once, without re-saving on every later click of the same item.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn device_pick_needs_save(picked: bool, seed: Option<&str>, last_saved: Option<&str>, selected: &str) -> bool {
    picked && last_saved.or(seed) != Some(selected)
}

/// Web build: the native ONNX path does not exist, so the ONNX provider/device
/// combos and the model-limit slider stay backend-driven (unchanged behavior).
#[cfg(target_arch = "wasm32")]
pub(super) fn draw_onnx_section(
    ui: &mut egui::Ui,
    handle: &AiBackendHandle,
    state: &mut AiBackendPanelState,
    snapshot: &AiBackendHealthSnapshot,
    ai_enabled: bool,
    _warnings: Option<&WarningSet>,
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
            t!("ai_backend.no_data").to_string()
        } else {
            state.selected_onnx_provider.clone()
        };
        WheelComboBox::from_label(t!("ai_backend.onnx_provider_combo_label_backend")).id_salt("ai_backend.onnx_provider_combo_label_backend")
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
            .add_enabled(can_apply_provider, egui::Button::new(t!("ai_backend.apply_provider_button")))
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
            t!("ai_backend.no_data").to_string()
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
        WheelComboBox::from_label(t!("ai_backend.onnx_device_combo_label_backend")).id_salt("ai_backend.onnx_device_combo_label_backend")
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
            .add_enabled(can_apply_device, egui::Button::new(t!("ai_backend.apply_onnx_button")))
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
    ui.label(t!("ai_backend.model_manager_heading"));
    let slider_response = ui.add_enabled(
        snapshot.connected,
        WheelSlider::new(&mut state.selected_max_loaded_models, 1..=10)
            .text(t!("ai_backend.max_loaded_models_label")),
    );
    if slider_response.changed() {
        handle.send_probe(AiBackendProbeCommand::SetMaxLoadedModels(
            state.selected_max_loaded_models,
        ));
    }
    ui.small(tf!("ai_backend.backend_current_limit", snapshot = snapshot.max_loaded_models));
}

/// Backend-reported ONNX providers + device lists folded into the offline native
/// capability set to form the unified provider list. Borrowed straight from the health
/// snapshot; `connected` gates whether the backend's list is authoritative.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct BackendOnnxProviders<'a> {
    /// Whether the Python backend is currently connected (its provider list is only
    /// folded in / authoritative while connected).
    pub(super) connected: bool,
    /// ORT provider tokens the backend reported (`available_onnx_providers`).
    pub(super) providers: &'a [String],
    /// Backend per-provider device lists (`onnx_devices_by_provider`).
    pub(super) devices_by_provider: &'a HashMap<String, Vec<AiBackendDeviceOption>>,
    /// Backend generic device-list fallback (`onnx_device_options`).
    pub(super) generic_devices: &'a [AiBackendDeviceOption],
}

/// Builds the UNIFIED ONNX provider option list: the local-native capability set
/// (CPU everywhere; DirectML on Windows; Core ML on macOS; CUDA on non-macOS) UNIONED
/// with the backend-reported providers when the backend is connected, deduped by ORT
/// token (CPU appears once). Pure so the union/labelling is unit-testable.
///
/// Native-set members carry `native_capable = true` with `native_available` per the
/// local probe (DirectML iff an adapter exists, CUDA iff the system runtime is present,
/// WebGPU iff a WebGPU-capable GPU is present) and local probe devices. WebGPU is
/// offered on all three desktop targets (Dawn D3D12/Vulkan/Metal), and its device list
/// is built from `webgpu_adapters` (enumerated with Dawn's per-OS backend, so the device
/// id = adapter index = the `device_id` Dawn indexes); an empty adapter list falls back
/// to a single default device so the combo still works. Backend-only
/// providers (not in the local native set, e.g. MIGraphX/ROCm) carry
/// `native_capable = false`, `native_available = false`, and the backend-reported
/// device list; they remain SHOWN and selectable so an AMD/ROCm user can pick them for
/// backend ONNX. `backend_available` is set for any token the connected backend
/// reported (including `WebGpuExecutionProvider` if a WebGPU-enabled backend reports it).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn build_onnx_provider_options(
    is_windows: bool,
    is_macos: bool,
    directml_accelerators: &[String],
    cuda_available: bool,
    webgpu_available: bool,
    webgpu_adapters: &[String],
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
                label: t!("ai_backend.device_default").to_string(),
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

    // WebGPU is a native provider on all three desktop targets (Dawn D3D12 on Windows,
    // Vulkan on Linux, Metal on macOS), so it is offered everywhere, marked available
    // per the local WebGPU probe. It participates in the local-native set exactly like
    // DirectML/CUDA. Its device list is the enumerated adapters: the id is the adapter
    // INDEX, which is the `device_id` Dawn indexes (enumeration uses Dawn's per-OS
    // backend). When enumeration is unavailable (macOS / single GPU / tool missing) the
    // list is empty, so fall back to a single default device that maps to `device_id` 0.
    let webgpu_devices: Vec<OnnxDeviceOptionUi> = webgpu_adapters
        .iter()
        .enumerate()
        .map(|(index, name)| OnnxDeviceOptionUi {
            id: index.to_string(),
            label: format!("{index}: {name}"),
        })
        .collect();
    let webgpu_devices = if webgpu_devices.is_empty() {
        vec![OnnxDeviceOptionUi {
            id: "0".to_string(),
            label: t!("ai_backend.device_default").to_string(),
        }]
    } else {
        webgpu_devices
    };
    options.push(OnnxProviderOption {
        token: "WebGpuExecutionProvider".to_string(),
        label: provider_base_label("WebGpuExecutionProvider"),
        native_capable: true,
        native_available: webgpu_available,
        backend_available: false,
        devices: webgpu_devices,
    });

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
/// in-process ONNX path can load. CPU counts; an unknown / backend-only token (e.g.
/// MIGraphX/ROCm) or the "not selected" sentinel is NOT native-capable.
#[cfg(not(target_arch = "wasm32"))]
fn token_is_native_capable(token: &str) -> bool {
    ms_native_runtime::parse_ort_provider_token(token).provider().is_some()
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
        "WebGpuExecutionProvider" => "WebGPU (GPU)".to_string(),
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
pub(super) fn default_onnx_provider(options: &[OnnxProviderOption]) -> String {
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
pub(super) struct ProviderRuntimeState {
    /// Whether the provider can actually run under the active runtime right now.
    pub(super) usable: bool,
    /// Label suffix to append (availability marker), if any.
    pub(super) suffix: Option<&'static str>,
}

/// Computes the [`ProviderRuntimeState`] for `option` under `runtime`. `backend_connected`
/// distinguishes "backend up but does not offer this provider" from "backend offline".
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn provider_runtime_state(
    option: &OnnxProviderOption,
    runtime: ms_config::AiRuntime,
    backend_connected: bool,
) -> ProviderRuntimeState {
    match runtime {
        ms_config::AiRuntime::Native => {
            if !option.native_capable {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some(t!("ai_backend.provider_backend_only_suffix")),
                }
            } else if option.native_available {
                ProviderRuntimeState {
                    usable: true,
                    suffix: None,
                }
            } else {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some(t!("ai_backend.provider_unavailable_suffix")),
                }
            }
        }
        ms_config::AiRuntime::Backend => {
            if option.backend_available {
                ProviderRuntimeState {
                    usable: true,
                    suffix: None,
                }
            } else if backend_connected {
                ProviderRuntimeState {
                    usable: false,
                    suffix: Some(t!("ai_backend.provider_unavailable_suffix")),
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
    runtime: Option<ms_config::AiRuntime>,
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

/// Renders one "Билд" combo group (separator + header + entries), if non-empty.
///
/// Real builds use `selectable_value` (an unavailable one stays selectable for a forced
/// download); the informational QNN entry is a non-interactive weak label (no binary, no
/// EP).
#[cfg(not(target_arch = "wasm32"))]
fn render_build_group(
    ui: &mut egui::Ui,
    state: &mut AiBackendPanelState,
    header: &str,
    slugs: &[&'static str],
    availability: &NativeBuildAvailability,
) {
    use ms_onnx_runtime::builds::{self, BuildCategory};
    if slugs.is_empty() {
        return;
    }
    ui.separator();
    ui.label(egui::RichText::new(header).small().strong());
    for slug in slugs {
        let label = availability.display_label(slug);
        match builds::build_by_slug(slug).map(|build| build.category) {
            Some(BuildCategory::Informational) => {
                // Display-only: no binary to download, not selectable.
                ui.weak(label);
            }
            Some(BuildCategory::Basic) | Some(BuildCategory::Specific) | None => {
                ui.selectable_value(&mut state.selected_onnx_build, (*slug).to_string(), label);
            }
        }
    }
}

/// The EP whose device list the native device combo shows for the selected ORT `token`:
/// the parsed provider, else the CPU EP's single "default" entry. The native branch keeps
/// the token inside the selected build's EP set (`reconcile_native_ep`), so the fallback
/// is reached only for a build with no EP (the informational QNN row), which has no device
/// to choose either way.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn native_device_ep(token: &str) -> ms_onnx::ExecutionProvider {
    ms_native_runtime::parse_ort_provider_token(token).provider().unwrap_or(ms_onnx::ExecutionProvider::Cpu)
}

/// A friendly EP name for the "EP" combo.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn ep_display_label(ep: ms_onnx::ExecutionProvider) -> &'static str {
    use ms_onnx::ExecutionProvider;
    match ep {
        ExecutionProvider::Cpu => "CPU",
        ExecutionProvider::DirectMl => "DirectML",
        ExecutionProvider::CoreMl => "Core ML",
        ExecutionProvider::Cuda => "CUDA",
        ExecutionProvider::WebGpu => "WebGPU",
        ExecutionProvider::OpenVino => "OpenVINO",
        ExecutionProvider::TensorRt => "TensorRT",
        ExecutionProvider::Xnnpack => "XNNPACK",
    }
}

/// The device options for an EP under the native build runtime: the ids of
/// [`ep_device_ids`] (the device-id rule, shared with inspection callers) with their
/// labels.
///
/// - DirectML → `"{index}: {name}"` per detected DX12 adapter, else `GPU 0`.
/// - WebGPU → `"{index}: {name}"` per enumerated adapter, else «По умолчанию».
/// - CUDA / TensorRT → `GPU 0`.
/// - CPU / CoreML / XNNPACK → «По умолчанию».
/// - OpenVINO → the device-TYPE string itself (`CPU` / `GPU` / `NPU`).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn ep_device_options(ep: ms_onnx::ExecutionProvider, caps: &OnnxCaps) -> Vec<OnnxDeviceOptionUi> {
    use ms_onnx::ExecutionProvider;
    let ids = ep_device_ids(ep, caps);
    // One label per enumerated adapter, or the single placeholder label; this mirrors the
    // id rule's "adapter list or one placeholder" shape, so ids and labels pair 1:1.
    let indexed = |names: &[String], fallback: &str| -> Vec<String> {
        if names.is_empty() {
            vec![fallback.to_string()]
        } else {
            names.iter().enumerate().map(|(index, name)| format!("{index}: {name}")).collect()
        }
    };
    let labels = match ep {
        ExecutionProvider::DirectMl => indexed(&caps.directml_accelerators, "GPU 0"),
        ExecutionProvider::WebGpu => indexed(&caps.webgpu_adapters, t!("ai_backend.device_default")),
        ExecutionProvider::Cuda | ExecutionProvider::TensorRt => vec!["GPU 0".to_string()],
        ExecutionProvider::Cpu | ExecutionProvider::CoreMl | ExecutionProvider::Xnnpack => vec![t!("ai_backend.device_default").to_string()],
        // OpenVINO selects a device by TYPE string, persisted verbatim to
        // `ai_onnx_device_id` (the native path routes it via `with_device_type`); the
        // type string is also its label.
        ExecutionProvider::OpenVino => ids.clone(),
    };
    ids.into_iter().zip(labels).map(|(id, label)| OnnxDeviceOptionUi { id, label }).collect()
}

/// Coerces the selected EP into `build`'s EP set (seeding it from the persisted token on
/// first run when that token belongs to the build, else the headline EP), then validates
/// the device against the resulting EP. Leaves an already-valid selection intact.
#[cfg(not(target_arch = "wasm32"))]
fn reconcile_native_ep(
    state: &mut AiBackendPanelState,
    build: &str,
    caps: &OnnxCaps,
    config_read: &OnnxConfigRead,
) {
    let eps = ms_onnx_runtime::builds::build_execution_providers(build);
    let Some(headline) = eps.first() else {
        return; // informational build (no EP) — nothing to reconcile.
    };

    let current_in_set = eps
        .iter()
        .any(|ep| ort_provider_token(*ep) == state.selected_onnx_provider);
    if !current_in_set {
        let from_config = config_read
            .provider_token
            .as_ref()
            .map(|token| token.trim())
            .filter(|token| eps.iter().any(|ep| ort_provider_token(*ep) == *token))
            .map(str::to_string);
        state.selected_onnx_provider =
            from_config.unwrap_or_else(|| ort_provider_token(*headline).to_string());
    }

    let ep =
        native_device_ep(&state.selected_onnx_provider);
    let devices = ep_device_options(ep, caps);
    // Validity is the runtime-consistent rule (`device_id_offered`), not combo membership:
    // a hand-written id the native load accepts for an offered device (OpenVINO "GPU.0")
    // is kept and shown verbatim instead of being silently replaced by the first item.
    let device_valid = device_id_offered(ep, &state.selected_onnx_device_id, caps);
    if !device_valid {
        let chosen = config_read
            .device_id
            .as_ref()
            .filter(|id| device_id_offered(ep, id, caps))
            .cloned()
            .or_else(|| devices.first().map(|device| device.id.clone()));
        if let Some(id) = chosen {
            state.selected_onnx_device_id = id;
        }
    }
}
