/*
FILE OVERVIEW: crates/ms-settings-ui/src/onnx_caps.rs
Native-only (the module is declared `cfg(not(target_arch = "wasm32"))` in `lib.rs`).

Purpose:
The locally-probed ONNX capabilities of this machine and the rules over them that are not
UI: the AI backend panel draws its offline provider/device/build pickers from them, and
inspection callers (settings checks) ask the same rules instead of re-deriving them.

Key items:
- `OnnxCaps`: probe answers (CUDA runtime, WebGPU, DirectML and WebGPU adapter names, and
  the `ms_native_runtime::NativeHardwareFacts` fields); `native_facts()` maps back to the
  facts the native availability rule consults.
- `probe_onnx_caps()`: runs every probe (blocking system commands): worker threads only.
- `ep_device_ids(ep, &caps)`: the device ids the panel's device combo offers for an EP
  (`ai_backend_panel::ep_device_options` zips its labels onto these ids).
- `device_id_offered(ep, id, &caps)`: the one rule "does this persisted id name an offered
  device", judged on the id as the native runtime parses it
  (`ms_native_runtime::device_selection_for`); used by the settings device check and the
  panel's device reconcile.

Notes:
Build availability is NOT decided here: it is `ms_native_runtime::native_fallback_reason`
over `OnnxCaps::native_facts()`. Labels and pickers stay in `ai_backend_panel.rs` (UI).
*/

use ms_onnx::{ExecutionProvider, NativeDeviceSelection};

/// The OpenVINO device-TYPE options, in display order. OpenVINO selects a device by type
/// string (persisted verbatim to `General.ai_onnx_device_id`), not by numeric index.
const OPENVINO_DEVICE_TYPES: [&str; 3] = ["CPU", "GPU", "NPU"];

/// OpenVINO virtual devices that take a `:`-separated member list (`"HETERO:GPU,CPU"`);
/// only `AUTO` is also valid bare.
const OPENVINO_VIRTUAL_DEVICES: [&str; 3] = ["AUTO", "MULTI", "HETERO"];

/// Locally-probed ONNX capabilities driving the OFFLINE provider/device list. The
/// `webgpu_available`, `cuda13_available`, `cuda12_available` and `openvino_available`
/// fields are [`ms_native_runtime::NativeHardwareFacts`] (probed by its `probe_all`),
/// and build availability is the native runtime's rule over them (see
/// [`OnnxCaps::native_facts`]).
#[derive(Debug, Clone, Default)]
pub struct OnnxCaps {
    /// Whether the system CUDA 12.x/cuDNN 9.x runtime is present (gates CUDA).
    pub cuda_available: bool,
    /// Whether a WebGPU-capable GPU (Dawn D3D12/Vulkan/Metal) is present (gates WebGPU).
    pub webgpu_available: bool,
    /// DirectML accelerator NAMES (Windows); the Vec position is the adapter index.
    pub directml_accelerators: Vec<String>,
    /// WebGPU adapter NAMES, enumerated per-OS with Dawn's backend; the Vec position is
    /// the WebGPU `device_id`. Empty when enumeration is unavailable (macOS / single GPU
    /// / tool missing), in which case the panel offers a single default adapter.
    pub webgpu_adapters: Vec<String>,
    /// Whether the `cuda13` build's CUDA 13.x + cuDNN 9.x runtime is present (gates the
    /// `cuda13` build in the "Билд" selector).
    pub cuda13_available: bool,
    /// Whether the `cuda12` build's CUDA 12.x + cuDNN 9.x runtime is present (gates the
    /// `cuda12` build in the "Билд" selector).
    pub cuda12_available: bool,
    /// Whether the native OpenVINO runtime can plausibly load (Intel device + runtime;
    /// gates the `openvino` build in the "Билд" selector).
    pub openvino_available: bool,
}

impl OnnxCaps {
    /// The probed facts the native availability rule
    /// ([`ms_native_runtime::native_fallback_reason`]) consults.
    pub(crate) fn native_facts(&self) -> ms_native_runtime::NativeHardwareFacts {
        ms_native_runtime::NativeHardwareFacts {
            cuda12_available: self.cuda12_available,
            cuda13_available: self.cuda13_available,
            openvino_available: self.openvino_available,
            webgpu_available: self.webgpu_available,
        }
    }
}

/// Runs every ONNX capability probe: the system CUDA runtime, the native availability
/// facts (`NativeHardwareFacts::probe_all`, the same probe set the runtime's selection
/// reads), the DirectML adapters and the WebGPU adapters. Blocking (spawns system
/// commands, scans library directories): worker threads only, never the GUI thread.
pub(crate) fn probe_onnx_caps() -> OnnxCaps {
    let cuda_available = ms_sysprobe::gpu_utils::native_cuda_runtime_available();
    // The facts the native availability rule consults come from the runtime's own probe
    // set, so the build picker and the runtime read the same answers.
    let facts = ms_native_runtime::NativeHardwareFacts::probe_all();
    let directml_accelerators = ms_sysprobe::gpu_utils::detect_directml_accelerators_windows()
        .into_iter()
        .map(|adapter| adapter.name)
        .collect::<Vec<_>>();
    // WebGPU adapters are enumerated with Dawn's per-OS backend so the Vec index is the
    // `device_id` passed to `ort::ep::WebGPU::with_device_id`.
    let webgpu_adapters = ms_sysprobe::gpu_utils::detect_webgpu_adapters()
        .into_iter()
        .map(|adapter| adapter.name)
        .collect::<Vec<_>>();
    OnnxCaps {
        cuda_available,
        webgpu_available: facts.webgpu_available,
        directml_accelerators,
        webgpu_adapters,
        cuda13_available: facts.cuda13_available,
        cuda12_available: facts.cuda12_available,
        openvino_available: facts.openvino_available,
    }
}

/// The valid `General.ai_onnx_device_id` values for `ep` under the native build runtime,
/// in the order the panel's device combo lists them. Pure (no probe).
///
/// - DirectML → one id per detected DX12 adapter (the adapter index), else `"0"`.
/// - WebGPU → one id per enumerated adapter (the Dawn `device_id` index), else `"0"`.
/// - CUDA / TensorRT / CPU / CoreML → the single id `"0"`.
/// - OpenVINO → the device-TYPE strings `"CPU"` / `"GPU"` / `"NPU"`.
pub(crate) fn ep_device_ids(ep: ExecutionProvider, caps: &OnnxCaps) -> Vec<String> {
    // One id per enumerated adapter (the index), or the single placeholder id "0".
    let indexed = |names: &[String]| -> Vec<String> {
        if names.is_empty() {
            vec!["0".to_string()]
        } else {
            (0..names.len()).map(|index| index.to_string()).collect()
        }
    };
    match ep {
        ExecutionProvider::DirectMl => indexed(&caps.directml_accelerators),
        ExecutionProvider::WebGpu => indexed(&caps.webgpu_adapters),
        ExecutionProvider::Cuda
        | ExecutionProvider::TensorRt
        | ExecutionProvider::Cpu
        | ExecutionProvider::CoreMl => vec!["0".to_string()],
        ExecutionProvider::OpenVino => OPENVINO_DEVICE_TYPES.iter().map(|kind| (*kind).to_string()).collect(),
    }
}

/// Whether the persisted `device_id` names a device `ep` offers now. The id is first parsed
/// exactly as the native load parses it (`ms_native_runtime::device_selection_for`), so
/// every spelling the runtime accepts for an offered device passes:
/// - index EPs: the parsed index is one of [`ep_device_ids`] (`" 1 "` passes as `"1"`);
/// - OpenVINO: the type string names only offered types, each optionally with a `.N`
///   instance (`"GPU.0"`), or an `AUTO` / `MULTI` / `HETERO` virtual device over such
///   members (`"HETERO:GPU,CPU"`; bare `"AUTO"`). Hardware presence is not probed for
///   OpenVINO: its offered list is the static type list;
/// - an id the runtime ignores (CPU / CoreML, an unparseable index, an empty string):
///   plain membership in [`ep_device_ids`].
///
/// Pure (no probe).
pub(crate) fn device_id_offered(ep: ExecutionProvider, device_id: &str, caps: &OnnxCaps) -> bool {
    match ms_native_runtime::device_selection_for(ep, Some(device_id)) {
        NativeDeviceSelection::Index(index) => ep_device_ids(ep, caps).contains(&index.to_string()),
        NativeDeviceSelection::OpenVinoDeviceType(device_type) => openvino_device_type_offered(&device_type),
        NativeDeviceSelection::Default => ep_device_ids(ep, caps).iter().any(|id| id == device_id),
    }
}

/// The OpenVINO device-string grammar of [`device_id_offered`] over the offered types.
fn openvino_device_type_offered(device_type: &str) -> bool {
    let member_offered = |member: &str| {
        let (kind, instance) = match member.split_once('.') {
            Some((kind, instance)) => (kind, Some(instance)),
            None => (member, None),
        };
        OPENVINO_DEVICE_TYPES.contains(&kind)
            && instance.is_none_or(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
    };
    match device_type.split_once(':') {
        Some((virtual_device, members)) => {
            OPENVINO_VIRTUAL_DEVICES.contains(&virtual_device)
                && !members.is_empty()
                && members.split(',').all(member_offered)
        }
        None => device_type == "AUTO" || member_offered(device_type),
    }
}

#[cfg(test)]
mod tests {
    use super::{OnnxCaps, device_id_offered, ep_device_ids};
    use ms_onnx::ExecutionProvider;

    /// An adapter list yields one index id per adapter; an empty one the placeholder "0".
    #[test]
    fn indexed_eps_follow_the_adapter_lists() {
        let caps = OnnxCaps {
            directml_accelerators: vec!["A".to_string(), "B".to_string(), "C".to_string()],
            ..OnnxCaps::default()
        };
        assert_eq!(ep_device_ids(ExecutionProvider::DirectMl, &caps), vec!["0", "1", "2"]);
        assert_eq!(ep_device_ids(ExecutionProvider::WebGpu, &caps), vec!["0"]);
    }

    /// OpenVINO ids are device-TYPE strings whatever the caps.
    #[test]
    fn openvino_ids_are_device_types() {
        assert_eq!(ep_device_ids(ExecutionProvider::OpenVino, &OnnxCaps::default()), vec!["CPU", "GPU", "NPU"]);
    }

    /// OpenVINO spellings the runtime passes through for an offered type are offered;
    /// unknown types and malformed instances are not.
    #[test]
    fn openvino_offered_follows_the_device_string_grammar() {
        let caps = OnnxCaps::default();
        for id in ["GPU", "GPU.0", " GPU.1 ", "NPU", "AUTO", "AUTO:GPU,CPU", "HETERO:GPU.0,CPU", "MULTI:GPU,NPU"] {
            assert!(device_id_offered(ExecutionProvider::OpenVino, id, &caps), "{id}");
        }
        for id in ["TPU", "GPU.", "GPU.x", "HETERO", "HETERO:", "HETERO:GPU,TPU", "FOO:GPU"] {
            assert!(!device_id_offered(ExecutionProvider::OpenVino, id, &caps), "{id}");
        }
    }

    /// Index EPs compare the PARSED index with the offered ids; an id the runtime ignores
    /// falls back to plain membership.
    #[test]
    fn index_offered_uses_the_runtime_parse() {
        let caps = OnnxCaps {
            directml_accelerators: vec!["A".to_string(), "B".to_string()],
            ..OnnxCaps::default()
        };
        assert!(device_id_offered(ExecutionProvider::DirectMl, " 1 ", &caps));
        assert!(!device_id_offered(ExecutionProvider::DirectMl, "2", &caps));
        assert!(device_id_offered(ExecutionProvider::Cuda, "0", &caps));
        assert!(!device_id_offered(ExecutionProvider::Cuda, "3", &caps));
        assert!(device_id_offered(ExecutionProvider::Cpu, "0", &caps));
        assert!(!device_id_offered(ExecutionProvider::Cpu, "3", &caps));
    }
}
