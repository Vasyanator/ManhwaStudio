/*
FILE OVERVIEW: crates/ms-settings-ui/src/ai_backend_panel/tests.rs
Unit tests of the AI pane's pure logic: the unified provider list, provider runtime state,
default provider, native EP device options, device-pick saving, build-action decision, the
landed-change channel and the visible sub-tabs per platform. Declared under `cfg(all(test, not(target_arch = "wasm32")))`.
*/

use super::onnx_selection::{
    BackendOnnxProviders, build_onnx_provider_options, default_onnx_provider, device_pick_needs_save,
    ep_device_options, ep_display_label, native_device_ep, provider_runtime_state,
};
use super::ort_build::{OrtBuildAction, ort_build_action};
use super::{AiBackendPanelState, AiBackendTab, OnnxProviderOption, PendingChanges, effective_tab, report_landed, visible_tabs};
use crate::onnx_caps::OnnxCaps;
use crate::settings_warnings::SettingChange;
use ms_native_runtime::ort_provider_token;
use ms_config::AiRuntime;
use ms_local_ai::backend_health::AiBackendDeviceOption;
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

#[test]
fn pending_changes_drain_moves_every_landed_change_in_order() {
    let pending = PendingChanges::default();
    let writer = pending.sender();
    // Changes landing on writer threads arrive in send order.
    let worker = std::thread::spawn(move || {
        report_landed(&writer, SettingChange::AiRuntime);
        report_landed(&writer, SettingChange::OnnxBuild);
        report_landed(&writer, SettingChange::OrtGuardReset);
    });
    worker.join().unwrap_or_else(|_| panic!("writer thread panicked"));
    let mut out = vec![SettingChange::BackendAutostart(true)];
    pending.drain_into(&mut out);
    assert_eq!(
        out,
        vec![
            SettingChange::BackendAutostart(true),
            SettingChange::AiRuntime,
            SettingChange::OnnxBuild,
            SettingChange::OrtGuardReset,
        ]
    );
    let mut again = Vec::new();
    pending.drain_into(&mut again);
    assert!(again.is_empty(), "a drained change is reported exactly once");
}

#[test]
fn landed_changes_append_after_the_frame_changes() {
    let state = AiBackendPanelState::default();
    report_landed(&state.pending_changes.sender(), SettingChange::OnnxProviderDevice);
    let changes = state.append_landed_changes(vec![SettingChange::BackendAutostart(false)]);
    assert_eq!(changes, vec![SettingChange::BackendAutostart(false), SettingChange::OnnxProviderDevice]);
    assert!(state.append_landed_changes(Vec::new()).is_empty());
}

#[test]
fn a_change_landing_after_the_pane_closed_is_dropped_without_panic() {
    let state = AiBackendPanelState::default();
    let writer = state.pending_changes.sender();
    drop(state);
    report_landed(&writer, SettingChange::AiRuntime);
}

#[test]
fn device_pick_saves_only_when_the_persisted_id_differs() {
    // Q4: the persisted id "3" is not offered, the pane displays "0" unsaved; picking
    // the displayed "0" must save.
    assert!(device_pick_needs_save(true, Some("3"), None, "0"));
    // No key persisted yet: an explicit pick saves.
    assert!(device_pick_needs_save(true, None, None, "0"));
    // Re-picking the persisted device is not a change.
    assert!(!device_pick_needs_save(true, Some("0"), None, "0"));
    // No click: the seeding never saves on its own.
    assert!(!device_pick_needs_save(false, Some("3"), None, "0"));
    // After the fixing save of "0" landed, re-picking "0" no longer saves ...
    assert!(!device_pick_needs_save(true, Some("3"), Some("0"), "0"));
    // ... while a genuinely different pick still does.
    assert!(device_pick_needs_save(true, Some("3"), Some("0"), "1"));
    // A later save supersedes the seed even when the seed equals the pick.
    assert!(device_pick_needs_save(true, Some("0"), Some("1"), "0"));
}

/// Shared empty device map so `offline_backend` can hand out a `'static` borrow.
static EMPTY_DEVICES_BY_PROVIDER: std::sync::OnceLock<
    HashMap<String, Vec<AiBackendDeviceOption>>,
> = std::sync::OnceLock::new();

#[test]
fn windows_lists_cpu_directml_cuda_with_availability() {
    // Two DirectML adapters present; CUDA runtime absent; WebGPU available; backend
    // offline.
    let options = build_onnx_provider_options(
        true,
        false,
        &["NVIDIA RTX".to_string(), "AMD Radeon".to_string()],
        false,
        true,
        &[],
        &offline_backend(),
    );
    let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
    assert_eq!(
        tokens,
        vec![
            "CPUExecutionProvider",
            "DmlExecutionProvider",
            "CUDAExecutionProvider",
            "WebGpuExecutionProvider"
        ]
    );

    // WebGPU is offered on Windows and marked available from the fed-in probe.
    let webgpu = options
        .iter()
        .find(|o| o.token == "WebGpuExecutionProvider")
        .expect("webgpu listed on windows");
    assert!(webgpu.native_capable && webgpu.native_available);
    assert_eq!(webgpu.label, "WebGPU (GPU)");

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
    let options =
        build_onnx_provider_options(true, false, &[], true, false, &[], &offline_backend());
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
fn linux_lists_cpu_cuda_and_webgpu() {
    // WebGPU is offered on Linux too (Dawn Vulkan); mark it available via the probe.
    let options =
        build_onnx_provider_options(false, false, &[], true, true, &[], &offline_backend());
    let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
    assert_eq!(
        tokens,
        vec![
            "CPUExecutionProvider",
            "CUDAExecutionProvider",
            "WebGpuExecutionProvider"
        ]
    );
    assert!(
        options
            .iter()
            .find(|o| o.token == "CUDAExecutionProvider")
            .unwrap()
            .native_available
    );
    let webgpu = options
        .iter()
        .find(|o| o.token == "WebGpuExecutionProvider")
        .expect("webgpu listed on linux");
    assert!(webgpu.native_capable && webgpu.native_available);
}

#[test]
fn macos_lists_cpu_coreml_and_webgpu() {
    // WebGPU is offered on macOS too (Dawn Metal, always available); feed the probe
    // as available.
    let options =
        build_onnx_provider_options(false, true, &[], false, true, &[], &offline_backend());
    let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
    assert_eq!(
        tokens,
        vec![
            "CPUExecutionProvider",
            "CoreMLExecutionProvider",
            "WebGpuExecutionProvider"
        ]
    );
    assert!(
        options
            .iter()
            .find(|o| o.token == "CoreMLExecutionProvider")
            .unwrap()
            .native_available
    );
    let webgpu = options
        .iter()
        .find(|o| o.token == "WebGpuExecutionProvider")
        .expect("webgpu listed on macos");
    assert!(webgpu.native_capable && webgpu.native_available);
}

#[test]
fn default_provider_prefers_available_directml_else_cpu() {
    let win = build_onnx_provider_options(
        true,
        false,
        &["GPU".to_string()],
        false,
        false,
        &[],
        &offline_backend(),
    );
    assert_eq!(default_onnx_provider(&win), "DmlExecutionProvider");

    // DirectML present but unavailable -> CPU.
    let win_no_adapter =
        build_onnx_provider_options(true, false, &[], false, false, &[], &offline_backend());
    assert_eq!(default_onnx_provider(&win_no_adapter), "CPUExecutionProvider");

    let linux =
        build_onnx_provider_options(false, false, &[], true, false, &[], &offline_backend());
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
    // Linux build (non-macOS, non-Windows): native set is CPU + CUDA + WebGPU.
    let options = build_onnx_provider_options(false, false, &[], true, false, &[], &backend);

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
    assert_eq!(native.suffix, Some(t!("ai_backend.provider_backend_only_suffix")));

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
    let options = build_onnx_provider_options(false, false, &[], true, false, &[], &backend);
    let tokens: Vec<&str> = options.iter().map(|o| o.token.as_str()).collect();
    assert_eq!(
        tokens,
        vec![
            "CPUExecutionProvider",
            "CUDAExecutionProvider",
            "WebGpuExecutionProvider"
        ]
    );
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
    let options =
        build_onnx_provider_options(false, false, &[], false, false, &[], &offline_backend());
    let cuda = options
        .iter()
        .find(|o| o.token == "CUDAExecutionProvider")
        .unwrap();
    assert!(cuda.native_capable && !cuda.native_available);
    let state = provider_runtime_state(cuda, AiRuntime::Native, false);
    assert!(!state.usable);
    assert_eq!(state.suffix, Some(t!("ai_backend.provider_unavailable_suffix")));
}

/// WebGPU is offered on every desktop OS, and its `native_available` follows the
/// fed-in probe: available -> usable under Native, unavailable -> `(недоступно)`.
#[test]
fn webgpu_availability_follows_probe_per_os() {
    for (is_windows, is_macos) in [(true, false), (false, true), (false, false)] {
        // Available probe: WebGPU is present and usable under the Native runtime.
        let options = build_onnx_provider_options(
            is_windows,
            is_macos,
            &[],
            false,
            true,
            &[],
            &offline_backend(),
        );
        let webgpu = options
            .iter()
            .find(|o| o.token == "WebGpuExecutionProvider")
            .expect("webgpu is offered on every desktop OS");
        assert!(webgpu.native_capable && webgpu.native_available);
        let state = provider_runtime_state(webgpu, AiRuntime::Native, false);
        assert!(state.usable);
        assert_eq!(state.suffix, None);

        // Unavailable probe: still listed, but marked unavailable under Native.
        let options = build_onnx_provider_options(
            is_windows,
            is_macos,
            &[],
            false,
            false,
            &[],
            &offline_backend(),
        );
        let webgpu = options
            .iter()
            .find(|o| o.token == "WebGpuExecutionProvider")
            .expect("webgpu stays listed when unavailable");
        assert!(webgpu.native_capable && !webgpu.native_available);
        let state = provider_runtime_state(webgpu, AiRuntime::Native, false);
        assert!(!state.usable);
        assert_eq!(state.suffix, Some(t!("ai_backend.provider_unavailable_suffix")));
    }
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
    assert_eq!(state.suffix, Some(t!("ai_backend.provider_unavailable_suffix")));
}

/// WebGPU's device list is built from the enumerated adapters: one device per
/// adapter, id = index ("0"/"1"/…) = the Dawn `device_id`, label = "<index>: <name>".
#[test]
fn webgpu_lists_one_device_per_enumerated_adapter() {
    let adapters = [
        "AMD Radeon RX 7900 XT".to_string(),
        "Intel(R) UHD Graphics".to_string(),
    ];
    // Windows shape so both DirectML and WebGPU coexist; adapters feed WebGPU only.
    let options = build_onnx_provider_options(
        true,
        false,
        &["AMD Radeon RX 7900 XT".to_string()],
        false,
        true,
        &adapters,
        &offline_backend(),
    );
    let webgpu = options
        .iter()
        .find(|o| o.token == "WebGpuExecutionProvider")
        .expect("webgpu listed");
    assert_eq!(webgpu.devices.len(), 2);
    assert_eq!(webgpu.devices[0].id, "0");
    assert_eq!(webgpu.devices[0].label, "0: AMD Radeon RX 7900 XT");
    assert_eq!(webgpu.devices[1].id, "1");
    assert_eq!(webgpu.devices[1].label, "1: Intel(R) UHD Graphics");
}

/// With no enumerated adapters (macOS / single GPU / tool missing) WebGPU still
/// offers a single default device with id "0" so the combo has a valid selection.
#[test]
fn webgpu_empty_adapters_fall_back_to_single_default_device() {
    let options =
        build_onnx_provider_options(false, false, &[], false, true, &[], &offline_backend());
    let webgpu = options
        .iter()
        .find(|o| o.token == "WebGpuExecutionProvider")
        .expect("webgpu listed");
    assert_eq!(webgpu.devices.len(), 1);
    assert_eq!(webgpu.devices[0].id, "0");
    assert_eq!(webgpu.devices[0].label, t!("ai_backend.device_default"));
}

/// Each EP's ORT token round-trips back to the same EP through the native runtime's
/// parser, so a persisted token resolves correctly (the device combo's EP included).
#[test]
fn ep_tokens_round_trip() {
    use ms_onnx::ExecutionProvider;
    for ep in [
        ExecutionProvider::Cpu,
        ExecutionProvider::DirectMl,
        ExecutionProvider::CoreMl,
        ExecutionProvider::Cuda,
        ExecutionProvider::WebGpu,
        ExecutionProvider::OpenVino,
        ExecutionProvider::TensorRt,
        ExecutionProvider::Xnnpack,
    ] {
        let token = ort_provider_token(ep);
        assert_eq!(native_device_ep(token), ep, "token {token} must map back to its EP");
    }
    // No EP named (informational build): the device combo shows CPU's default entry.
    assert_eq!(native_device_ep(""), ExecutionProvider::Cpu);
}

/// The EP labels for a build follow the build's ordered EP set.
#[test]
fn ep_labels_for_build_follow_the_catalog() {
    use ms_onnx_runtime::builds;
    let cuda: Vec<&str> = builds::build_execution_providers("cuda13")
        .iter()
        .map(|ep| ep_display_label(*ep))
        .collect();
    assert_eq!(cuda, vec!["CUDA", "TensorRT", "CPU"]);
    let openvino: Vec<&str> = builds::build_execution_providers("openvino")
        .iter()
        .map(|ep| ep_display_label(*ep))
        .collect();
    assert_eq!(openvino, vec!["OpenVINO", "CPU"]);
}

/// OpenVINO's device options are device-TYPE strings (id == label), not numeric
/// indices, matching the OpenVINO `with_device_type` contract.
#[test]
fn openvino_device_options_are_type_strings() {
    let caps = OnnxCaps::default();
    let devices = ep_device_options(ms_onnx::ExecutionProvider::OpenVino, &caps);
    let ids: Vec<&str> = devices.iter().map(|device| device.id.as_str()).collect();
    assert_eq!(ids, vec!["CPU", "GPU", "NPU"]);
    assert!(devices.iter().all(|device| device.id == device.label));
}

/// CUDA/TensorRT/CPU/CoreML each expose exactly one device at index 0.
#[test]
fn single_device_eps_use_index_zero() {
    use ms_onnx::ExecutionProvider;
    let caps = OnnxCaps::default();
    for ep in [
        ExecutionProvider::Cuda,
        ExecutionProvider::TensorRt,
        ExecutionProvider::Cpu,
        ExecutionProvider::CoreMl,
    ] {
        let devices = ep_device_options(ep, &caps);
        assert_eq!(devices.len(), 1, "{ep:?} should have one device");
        assert_eq!(devices[0].id, "0");
    }
}

/// Characterization of the device-id rule of `ep_device_options` for every EP over two
/// caps shapes (DirectML adapters without WebGPU adapters, and the reverse), with the
/// non-localized labels of the enumerated adapters.
#[test]
fn ep_device_options_ids_per_caps_shape() {
    use ms_onnx::ExecutionProvider;
    let ids_of = |ep: ExecutionProvider, caps: &OnnxCaps| -> Vec<String> {
        ep_device_options(ep, caps).into_iter().map(|device| device.id).collect()
    };
    let directml_only = OnnxCaps {
        directml_accelerators: vec!["A".to_string(), "B".to_string()],
        ..OnnxCaps::default()
    };
    let webgpu_only = OnnxCaps {
        webgpu_adapters: vec!["X".to_string(), "Y".to_string()],
        ..OnnxCaps::default()
    };
    let cases: [(ExecutionProvider, &[&str], &[&str]); 7] = [
        (ExecutionProvider::Cpu, &["0"], &["0"]),
        (ExecutionProvider::DirectMl, &["0", "1"], &["0"]),
        (ExecutionProvider::CoreMl, &["0"], &["0"]),
        (ExecutionProvider::Cuda, &["0"], &["0"]),
        (ExecutionProvider::WebGpu, &["0"], &["0", "1"]),
        (ExecutionProvider::OpenVino, &["CPU", "GPU", "NPU"], &["CPU", "GPU", "NPU"]),
        (ExecutionProvider::TensorRt, &["0"], &["0"]),
    ];
    for (ep, with_directml, with_webgpu) in cases {
        assert_eq!(ids_of(ep, &directml_only), with_directml, "{ep:?} with DirectML adapters");
        assert_eq!(ids_of(ep, &webgpu_only), with_webgpu, "{ep:?} with WebGPU adapters");
    }
    let labels = |ep: ExecutionProvider, caps: &OnnxCaps| -> Vec<String> {
        ep_device_options(ep, caps).into_iter().map(|device| device.label).collect()
    };
    assert_eq!(labels(ExecutionProvider::DirectMl, &directml_only), vec!["0: A", "1: B"]);
    assert_eq!(labels(ExecutionProvider::DirectMl, &webgpu_only), vec!["GPU 0"]);
    assert_eq!(labels(ExecutionProvider::WebGpu, &webgpu_only), vec!["0: X", "1: Y"]);
    assert_eq!(labels(ExecutionProvider::Cuda, &webgpu_only), vec!["GPU 0"]);
}

/// The build-action decision matrix: `(committed, active, selected, present)` →
/// {Retry | LoadOtherBuild | RestartNote}.
#[test]
fn build_action_decision_matrix() {
    use OrtBuildAction::{LoadOtherBuild, RestartNote, Retry};
    // Not committed: present → same-build retry; absent → download it.
    assert_eq!(ort_build_action(false, None, "cuda13", true, false), Retry);
    assert_eq!(ort_build_action(false, None, "cuda13", false, false), LoadOtherBuild);
    // Committed same build → retry regardless of presence.
    assert_eq!(ort_build_action(true, Some("cuda13"), "cuda13", true, false), Retry);
    assert_eq!(ort_build_action(true, Some("cuda13"), "cuda13", false, false), Retry);
    // Committed different build → restart required (dylib is pinned).
    assert_eq!(ort_build_action(true, Some("directml"), "cuda13", true, false), RestartNote);
    assert_eq!(ort_build_action(true, Some("directml"), "cuda13", false, false), RestartNote);
    // Defensive: committed but no active recorded → retry.
    assert_eq!(ort_build_action(true, None, "cuda13", true, false), Retry);
    // Bundled (Android): nothing to download, so never LoadOtherBuild.
    assert_eq!(ort_build_action(false, None, "xnnpack", false, true), Retry);
    assert_eq!(ort_build_action(true, Some("cpu"), "xnnpack", false, true), RestartNote);
}

/// The tabs follow the platform facts: PyTorch only with the Python backend, sd.cpp only
/// with its runtime, ONNX always; one visible tab means no strip (the caller's `len() > 1`).
#[test]
fn visible_tabs_follow_the_platform() {
    use AiBackendTab::{Onnx, PyTorch, SdCpp};
    assert_eq!(visible_tabs(true, true), vec![PyTorch, Onnx, SdCpp]);
    assert_eq!(visible_tabs(true, false), vec![PyTorch, Onnx]);
    assert_eq!(visible_tabs(false, true), vec![Onnx, SdCpp]);
    assert_eq!(visible_tabs(false, false), vec![Onnx]);
}

/// The pane opens on PyTorch where it exists, else on ONNX; a hidden selection falls back
/// to the first visible tab and a visible one is kept.
#[test]
fn effective_tab_defaults_to_pytorch_else_onnx() {
    use AiBackendTab::{Onnx, PyTorch, SdCpp};
    assert_eq!(AiBackendTab::default(), PyTorch);
    assert_eq!(effective_tab(PyTorch, &visible_tabs(true, true)), PyTorch);
    assert_eq!(effective_tab(PyTorch, &visible_tabs(false, true)), Onnx);
    assert_eq!(effective_tab(SdCpp, &visible_tabs(true, false)), PyTorch);
    assert_eq!(effective_tab(SdCpp, &visible_tabs(false, true)), SdCpp);
    assert_eq!(effective_tab(Onnx, &[]), Onnx);
}
