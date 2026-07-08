/*
File: gpu_utils.rs

Purpose:
Shared GPU and accelerator detection helpers for installer and runtime code.

Main responsibilities:
- detect NVIDIA/AMD adapters through platform tools;
- detect CUDA, ROCm, and NVIDIA Compute Capability versions;
- inspect Linux driver/ROCm installation state;
- resolve AMD GPU architecture / LLVM target and validate ROCm 7.2 support;
- detect likely DirectML-capable accelerators on Windows;
- probe for the system CUDA 12.x / cuDNN 9.x runtime the onnxruntime GPU build needs;
- probe for a WebGPU-capable GPU (Dawn D3D12/Vulkan/Metal) for the WebGPU provider;
- enumerate the WebGPU GPU adapters (per-OS, index = Dawn `device_id`) for adapter picking.

Key structures:
- RuntimeVersion
- GpuArchitecture
- LinuxDriverStatus
- RocmInstallationStatus
- RocmSupportValidation
- DirectMlAccelerator
- WebGpuAdapter
- CudaRuntimeStatus

Key functions:
- detect_nvidia_gpu()
- detect_amd_gpu()
- detect_cuda_runtime_version()
- detect_nvidia_compute_capability()
- detect_rocm_runtime_version()
- validate_rocm_7_2_support_linux()
- detect_directml_accelerators_windows()
- probe_cuda_runtime() / native_cuda_runtime_available()
- native_cuda_build_available() (per-build CUDA-major gate: cuda12 vs cuda13)
- native_openvino_runtime_available() (Intel-device gate; Windows also needs a system
  OpenVINO runtime library — the Linux wheel bundles it, the Windows wheel does not)
- native_webgpu_runtime_available()
- detect_webgpu_adapters()

Notes:
Detection intentionally uses short-lived system commands and filesystem probes.
Callers must run these helpers outside the GUI thread.

Web (wasm) build:
GPU/accelerator detection relies on system commands (`nvidia-smi`, `lspci`, ...) that do
not exist in the browser. The module still COMPILES on `wasm32` (the launcher
system-information tab is cross-platform), but the single command primitive
(`command_output`) is stubbed to return `None`, so every `detect_*` helper reports
"nothing detected" on web. The public types are plain data and compile everywhere.
*/

use std::fmt::Display;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RuntimeVersion {
    pub major: u32,
    pub minor: u32,
}

impl RuntimeVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

impl Display for RuntimeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuVendor {
    Amd,
    Nvidia,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuArchitecture {
    pub vendor: GpuVendor,
    pub name: Option<String>,
    pub architecture: Option<String>,
    pub llvm_target: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxDriverStatus {
    pub amdgpu_loaded: bool,
    pub kfd_available: bool,
    pub dri_available: bool,
    pub nvidia_driver_available: bool,
    pub nvidia_device_nodes_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RocmInstallationStatus {
    pub present: bool,
    pub version: Option<RuntimeVersion>,
    pub details: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RocmSupportValidation {
    pub supported: bool,
    pub reason: String,
    pub driver_status: LinuxDriverStatus,
    pub rocm_status: RocmInstallationStatus,
    pub architectures: Vec<GpuArchitecture>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectMlAccelerator {
    pub name: String,
    pub vendor: Option<GpuVendor>,
}

/// A GPU adapter selectable by the native WebGPU execution provider.
///
/// The Vec position of an adapter in [`detect_webgpu_adapters`]'s result IS the WebGPU
/// `device_id`: WebGPU runs through Dawn, and Dawn enumerates adapters with the same
/// per-OS backend this module uses (DXGI/D3D12 on Windows, Vulkan on Linux), so the
/// index aligns with `ort::ep::WebGPU::with_device_id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebGpuAdapter {
    /// Human-readable adapter name (e.g. `"AMD Radeon RX 7900 XT"`).
    pub name: String,
}

/// Result of the system CUDA/cuDNN probe used to gate the native CUDA execution
/// provider.
///
/// The onnxruntime 1.20.1 GPU build links against CUDA 12.x and cuDNN 9.x, and it
/// does NOT bundle them: the app downloads only the onnxruntime GPU dylibs and
/// relies on a system CUDA/cuDNN install. [`available`](Self::available) is the
/// gate: an NVIDIA GPU plus a CUDA 12.x runtime plus a cuDNN 9.x library must all be
/// present, otherwise the CUDA EP would fail to register.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CudaRuntimeStatus {
    /// An NVIDIA GPU was detected on the system.
    pub nvidia_gpu: bool,
    /// A CUDA 12.x runtime library (`libcudart.so.12*` / `cudart64_12.dll`) was
    /// found on the dynamic-library search path.
    pub cuda12_runtime: bool,
    /// A cuDNN 9.x library (`libcudnn.so.9*` / `cudnn64_9.dll`) was found on the
    /// dynamic-library search path.
    pub cudnn9_runtime: bool,
    /// Whether the native CUDA provider can plausibly load: all three flags hold.
    pub available: bool,
    /// Short human-readable Russian summary for the settings UI.
    pub details: String,
}

pub const ROCM_7_2_SUPPORTED_LLVM_TARGETS: &[&str] = &[
    "gfx950", "gfx942", "gfx90a", "gfx908", "gfx1201", "gfx1200", "gfx1101", "gfx1100", "gfx1030",
];

#[must_use]
pub fn rocm_7_2_supported_llvm_targets() -> &'static [&'static str] {
    ROCM_7_2_SUPPORTED_LLVM_TARGETS
}

#[must_use]
pub fn detect_nvidia_gpu() -> bool {
    if let Some(output) = command_output("nvidia-smi", &["-L"]) {
        let text = output.to_ascii_lowercase();
        if text.contains("gpu") || text.contains("nvidia") {
            return true;
        }
    }

    if cfg!(target_os = "windows")
        && windows_video_controller_output()
            .as_deref()
            .is_some_and(|output| output.to_ascii_lowercase().contains("nvidia"))
    {
        return true;
    }

    if cfg!(target_os = "linux")
        && let Some(output) = command_output("lspci", &[])
        && output.to_ascii_lowercase().contains("nvidia")
    {
        return true;
    }

    false
}

#[must_use]
pub fn detect_amd_gpu() -> bool {
    if cfg!(target_os = "linux") {
        return detect_amd_gpu_linux();
    }

    if cfg!(target_os = "windows") {
        return windows_video_controller_output()
            .as_deref()
            .is_some_and(text_has_amd_gpu);
    }

    false
}

#[must_use]
pub fn detect_amd_gpu_linux() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }

    if let Some(output) = command_output("lspci", &[]) {
        return text_has_amd_gpu(&output);
    }

    false
}

#[must_use]
pub fn detect_cuda_runtime_version() -> Option<RuntimeVersion> {
    if let Some(output) = command_output("nvidia-smi", &[])
        && let Some(v) = parse_version_after_keywords(&output, &["cuda version"])
    {
        return Some(v);
    }

    if let Some(output) = command_output("nvcc", &["--version"])
        && let Some(v) = parse_version_after_keywords(&output, &["release", "cuda"])
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_rocm_runtime_version() -> Option<RuntimeVersion> {
    if let Some(output) =
        command_output_from_candidates(&["rocminfo", "/opt/rocm/bin/rocminfo"], &[])
        && let Some(v) = parse_version_after_keywords(&output, &["rocm version", "runtime version"])
    {
        return Some(v);
    }

    if let Some(output) =
        command_output_from_candidates(&["hipcc", "/opt/rocm/bin/hipcc"], &["--version"])
        && let Some(v) = parse_version_after_keywords(&output, &["rocm version", "hip version"])
    {
        return Some(v);
    }

    if let Some(output) = command_output_from_candidates(
        &["rocm-smi", "/opt/rocm/bin/rocm-smi"],
        &["--showdriverversion"],
    ) && let Some(v) = parse_first_major_minor(&output)
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_nvidia_compute_capability() -> Option<RuntimeVersion> {
    if let Some(output) = command_output(
        "nvidia-smi",
        &["--query-gpu=compute_cap", "--format=csv,noheader"],
    ) && let Some(v) = parse_min_runtime_version_from_lines(&output)
    {
        return Some(v);
    }

    if let Some(output) = command_output("nvidia-smi", &["-q"])
        && let Some(v) = parse_version_after_keywords(&output, &["compute capability"])
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_nvidia_gpu_architecture() -> Option<GpuArchitecture> {
    detect_nvidia_compute_capability().map(|capability| GpuArchitecture {
        vendor: GpuVendor::Nvidia,
        name: Some(format!("NVIDIA SM {capability}")),
        architecture: Some(format!("sm_{}{}", capability.major, capability.minor)),
        llvm_target: None,
    })
}

#[must_use]
pub fn linux_driver_status() -> LinuxDriverStatus {
    LinuxDriverStatus {
        amdgpu_loaded: Path::new("/sys/module/amdgpu").is_dir(),
        kfd_available: Path::new("/dev/kfd").exists(),
        dri_available: Path::new("/dev/dri").is_dir(),
        nvidia_driver_available: Path::new("/proc/driver/nvidia/version").is_file()
            || command_output("nvidia-smi", &["-L"]).is_some(),
        nvidia_device_nodes_available: linux_nvidia_device_nodes_available(),
    }
}

#[must_use]
pub fn detect_rocm_installation_linux() -> RocmInstallationStatus {
    if !cfg!(target_os = "linux") {
        return RocmInstallationStatus {
            present: false,
            version: None,
            details: "ROCm Linux probe skipped on this target.".to_string(),
        };
    }

    let mut details = Vec::new();
    let version = detect_rocm_runtime_version();

    for command in ["rocminfo", "hipcc", "rocm-smi"] {
        if let Some(path) = command_path(command) {
            details.push(format!("{command}: {}", path.display()));
        }
    }
    if Path::new("/opt/rocm").is_dir() {
        details.push("/opt/rocm exists".to_string());
    }

    let present = version.is_some() || !details.is_empty();
    if details.is_empty() {
        details.push("ROCm not detected through rocminfo/hipcc/rocm-smi or /opt/rocm.".to_string());
    }

    RocmInstallationStatus {
        present,
        version,
        details: details.join("\n"),
    }
}

#[must_use]
pub fn detect_amd_gpu_architectures_linux() -> Vec<GpuArchitecture> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }

    let mut architectures = Vec::new();
    for target in detect_amd_gfx_targets_from_rocm_tools() {
        architectures.push(GpuArchitecture {
            vendor: GpuVendor::Amd,
            name: None,
            architecture: rocm_architecture_family(&target).map(str::to_string),
            llvm_target: Some(target),
        });
    }

    if architectures.is_empty() {
        architectures.extend(detect_amd_gfx_targets_from_lspci().into_iter().map(
            |(name, target)| GpuArchitecture {
                vendor: GpuVendor::Amd,
                name: Some(name),
                architecture: rocm_architecture_family(&target).map(str::to_string),
                llvm_target: Some(target),
            },
        ));
    }

    dedupe_architectures(architectures)
}

#[must_use]
pub fn validate_rocm_7_2_support_linux() -> RocmSupportValidation {
    let driver_status = linux_driver_status();
    let rocm_status = detect_rocm_installation_linux();
    let architectures = detect_amd_gpu_architectures_linux();

    if !detect_amd_gpu_linux() {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU was not detected on Linux.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    if !driver_status.amdgpu_loaded || !driver_status.kfd_available {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU detected, but amdgpu driver or /dev/kfd is unavailable.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    if !rocm_status.present {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU detected, but ROCm installation was not found.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    let has_supported_architecture = architectures.iter().any(|arch| {
        arch.llvm_target
            .as_deref()
            .is_some_and(is_rocm_7_2_supported_llvm_target)
    });

    if has_supported_architecture {
        RocmSupportValidation {
            supported: true,
            reason: "Detected AMD GPU architecture is supported by ROCm 7.2.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        }
    } else {
        RocmSupportValidation {
            supported: false,
            reason:
                "AMD GPU architecture was not detected or is not officially supported by ROCm 7.2."
                    .to_string(),
            driver_status,
            rocm_status,
            architectures,
        }
    }
}

#[must_use]
pub fn detect_directml_accelerators_windows() -> Vec<DirectMlAccelerator> {
    if !cfg!(target_os = "windows") {
        return Vec::new();
    }

    windows_video_controller_output()
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter(|line| !line.to_ascii_lowercase().contains("microsoft basic"))
                .filter_map(directml_accelerator_from_name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[must_use]
pub fn has_directml_accelerator_windows() -> bool {
    !detect_directml_accelerators_windows().is_empty()
}

/// Probes the system for the CUDA 12.x + cuDNN 9.x runtime the onnxruntime GPU
/// build needs, returning a [`CudaRuntimeStatus`].
///
/// Combines an NVIDIA GPU check ([`detect_nvidia_gpu`]) with a scan of the
/// dynamic-library search path for a CUDA 12.x `cudart` library and a cuDNN 9.x
/// library. Pure detection: it spawns short-lived system commands and reads a few
/// directories, so callers MUST run it off the GUI thread. On the web (wasm) build,
/// filesystem/command primitives report nothing, so it reports "unavailable".
#[must_use]
pub fn probe_cuda_runtime() -> CudaRuntimeStatus {
    let nvidia_gpu = detect_nvidia_gpu();
    let cuda12_runtime = has_cuda12_runtime_library();
    let cudnn9_runtime = has_cudnn9_library();
    let available = nvidia_gpu && cuda12_runtime && cudnn9_runtime;

    let details = if available {
        "Обнаружены видеокарта NVIDIA, CUDA 12.x и cuDNN 9.x.".to_string()
    } else {
        let mut missing = Vec::new();
        if !nvidia_gpu {
            missing.push("видеокарта NVIDIA");
        }
        if !cuda12_runtime {
            missing.push("библиотека CUDA 12.x (cudart)");
        }
        if !cudnn9_runtime {
            missing.push("библиотека cuDNN 9.x");
        }
        format!("Не найдено: {}.", missing.join(", "))
    };

    CudaRuntimeStatus {
        nvidia_gpu,
        cuda12_runtime,
        cudnn9_runtime,
        available,
        details,
    }
}

/// Whether the native CUDA execution provider can plausibly load on this system.
///
/// Convenience gate over [`probe_cuda_runtime`]; see [`CudaRuntimeStatus::available`].
/// Must be called off the GUI thread.
#[must_use]
pub fn native_cuda_runtime_available() -> bool {
    probe_cuda_runtime().available
}

/// Whether the native WebGPU execution provider can plausibly load on this system.
///
/// WebGPU runs through Dawn, which targets a native GPU API per platform: D3D12 on
/// Windows, Vulkan on Linux, Metal on macOS. This is a lightweight capability
/// HEURISTIC, not a guarantee — the real backstop is `error_on_failure()` at EP
/// registration time in `ms-onnx`, so a machine that passes this probe but still fails
/// to register the EP surfaces a load error and the native runtime falls back to CPU /
/// the backend.
///
/// Heuristic per platform:
/// - Windows: a DX12-capable adapter exists — reuses [`has_directml_accelerator_windows`]
///   as a proxy, since a DirectML/DX12 adapter means Dawn's D3D12 backend can initialize.
/// - Linux: a Vulkan loader (`libvulkan.so.1` / `libvulkan.so`) is on the library
///   search path AND a DRM GPU device node (`/dev/dri`) is present.
/// - macOS: always true (Metal is available on every supported macOS).
///
/// Pure detection: it spawns short-lived system commands and reads a few directories,
/// so callers MUST run it off the GUI thread. On the web (wasm) build the
/// filesystem/command primitives report nothing, so it degrades to `false`.
#[must_use]
pub fn native_webgpu_runtime_available() -> bool {
    if cfg!(target_os = "macos") {
        // Metal is always available on supported macOS; Dawn defaults to it there.
        return true;
    }
    if cfg!(target_os = "windows") {
        // A DX12/DirectML-capable adapter implies Dawn's D3D12 backend can start.
        return has_directml_accelerator_windows();
    }
    // Linux (and any other unix): Dawn uses the Vulkan backend, which needs both a
    // Vulkan loader and an actual GPU device node.
    has_vulkan_loader() && linux_gpu_device_present()
}

/// Enumerates the GPU adapters selectable by the native WebGPU execution provider, in
/// the order that matches Dawn's `device_id` indexing.
///
/// The returned Vec's position IS the WebGPU `device_id`: the enumeration uses the same
/// native backend Dawn uses per-OS, so the indices line up with
/// `ort::ep::WebGPU::with_device_id`:
/// - Windows: DXGI adapters via [`detect_directml_accelerators_windows`] — DXGI adapter
///   order matches Dawn's D3D12 adapter order.
/// - Linux (and other unix): Vulkan physical devices via `vulkaninfo --summary`; the
///   Vulkan enumeration order is the order Dawn's Vulkan backend sees. If `vulkaninfo`
///   is absent or its output is unparseable, returns an EMPTY Vec (the caller then
///   offers a single default adapter) — adapters are never fabricated.
/// - macOS: EMPTY. A single Metal GPU is the common case and the caller offers a default
///   adapter; enumerating multiple Metal GPUs is out of scope.
///
/// This is a best-effort HEURISTIC: the real backstop remains `error_on_failure()` at EP
/// registration time in `ms-onnx`. Pure detection that spawns a short-lived system
/// command, so callers MUST run it off the GUI thread. On the web (wasm) build the
/// command primitive reports nothing, so it degrades to empty.
#[must_use]
pub fn detect_webgpu_adapters() -> Vec<WebGpuAdapter> {
    if cfg!(target_os = "windows") {
        // Dawn's D3D12 backend enumerates DXGI adapters in the same order DXGI reports
        // them, which is exactly what `detect_directml_accelerators_windows` returns.
        return detect_directml_accelerators_windows()
            .into_iter()
            .map(|adapter| WebGpuAdapter { name: adapter.name })
            .collect();
    }
    if cfg!(target_os = "macos") {
        // Single Metal GPU is the common case; the caller supplies a default adapter.
        return Vec::new();
    }
    // Linux (and other unix): Dawn uses the Vulkan backend. Enumerate Vulkan physical
    // devices via vulkaninfo; their enumeration order is Dawn's `device_id` order.
    command_output("vulkaninfo", &["--summary"])
        .map(|output| {
            parse_vulkaninfo_devices(&output)
                .into_iter()
                .map(|name| WebGpuAdapter { name })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Parses `vulkaninfo --summary` output into the ordered list of Vulkan physical-device
/// names.
///
/// Reads each `GPUn:` summary block and extracts its `deviceName = <name>` value,
/// preserving enumeration order (GPU0, GPU1, …) — the same order Dawn's Vulkan backend
/// sees, so the Vec position is the WebGPU `device_id`. Defensive: unrecognized or
/// malformed lines are skipped and contribute no entry rather than a fabricated name.
fn parse_vulkaninfo_devices(text: &str) -> Vec<String> {
    let mut devices = Vec::new();
    let mut in_gpu_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // A summary device-block header: "GPU0:", "GPU1:", ... The digits are the Vulkan
        // physical-device index, so blocks arrive in enumeration order.
        if let Some(rest) = trimmed.strip_prefix("GPU")
            && let Some(colon) = rest.find(':')
            && !rest[..colon].is_empty()
            && rest[..colon].chars().all(|ch| ch.is_ascii_digit())
        {
            in_gpu_block = true;
            continue;
        }
        // The first `deviceName = ...` line inside a block names that GPU.
        if in_gpu_block
            && let Some(value) = trimmed.strip_prefix("deviceName")
            && let Some(eq) = value.find('=')
        {
            let name = value[eq + 1..].trim();
            if !name.is_empty() {
                devices.push(name.to_string());
            }
            in_gpu_block = false;
        }
    }
    devices
}

/// Whether a Vulkan loader (`libvulkan.so.1` / `libvulkan.so`) is present on the
/// dynamic-library search path — the loader Dawn's Vulkan backend needs on Linux.
///
/// Reuses [`library_search_dirs`] (which includes the standard distro lib dirs where
/// the loader lives, e.g. `/usr/lib/x86_64-linux-gnu`). A directory that cannot be
/// read is harmless (the scan finds nothing there).
fn has_vulkan_loader() -> bool {
    library_search_dirs().iter().any(|dir| {
        scan_dir_for_library(dir, |name| {
            let lower = name.to_ascii_lowercase();
            lower == "libvulkan.so.1" || lower == "libvulkan.so"
        })
    })
}

/// Whether a DRM GPU device node directory (`/dev/dri`) exists — a GPU-present proxy
/// on Linux. Always false off Linux (and on wasm, where there is no such path).
fn linux_gpu_device_present() -> bool {
    Path::new("/dev/dri").is_dir()
}

/// Whether a CUDA 12.x `cudart` runtime library is present on the library path.
///
/// Matches `libcudart.so.12*` on Linux and `cudart64_12*.dll` on Windows; the CUDA 12
/// onnxruntime build links the CUDA 12 runtime, so an older/newer major will not load.
fn has_cuda12_runtime_library() -> bool {
    has_cuda_runtime_library_major(12)
}

/// Whether a `cudart` runtime library of CUDA major `major` is present on the library
/// search path.
///
/// Matches `libcudart.so.<major>*` on Linux and `cudart64_<major>*.dll` on Windows.
/// Each onnxruntime CUDA build links exactly one CUDA major (12 or 13); a mismatched
/// major cannot satisfy it, so the two builds are gated independently. Used by
/// [`native_cuda_build_available`].
fn has_cuda_runtime_library_major(major: u32) -> bool {
    let windows = cfg!(target_os = "windows");
    // `libcudart.so.12` vs `libcudart.so.13` are distinct prefixes; the only ambiguity
    // would be a 1.x major against 12/13, which onnxruntime does not ship, so a plain
    // prefix match is unambiguous for the supported CUDA majors.
    let win_prefix = format!("cudart64_{major}");
    let nix_prefix = format!("libcudart.so.{major}");
    library_search_dirs().iter().any(|dir| {
        scan_dir_for_library(dir, |name| {
            let lower = name.to_ascii_lowercase();
            if windows {
                lower.starts_with(&win_prefix) && lower.ends_with(".dll")
            } else {
                lower.starts_with(&nix_prefix)
            }
        })
    })
}

/// The CUDA major version an onnxruntime CUDA build links against.
///
/// `"cuda12"` → `Some(12)`, `"cuda13"` → `Some(13)`; any other slug (a non-CUDA build)
/// → `None`. Pure and testable; the single place that maps a CUDA build slug to its
/// runtime major.
#[must_use]
fn cuda_major_for_build(build: &str) -> Option<u32> {
    match build {
        "cuda12" => Some(12),
        "cuda13" => Some(13),
        _ => None,
    }
}

/// Whether the native CUDA execution provider for build `build` can plausibly load on
/// this system.
///
/// `build` must be a CUDA build slug (`"cuda12"` / `"cuda13"`); availability requires an
/// NVIDIA GPU, a `cudart` runtime of that build's CUDA major (12 vs 13, checked
/// independently so `cuda12` is available iff a CUDA 12.x runtime exists and `cuda13`
/// iff CUDA 13.x exists), and a cuDNN 9.x library (both onnxruntime CUDA 12 and CUDA 13
/// builds link cuDNN 9). A non-CUDA slug returns `false` — this gate is CUDA-specific.
///
/// Pure detection over short-lived system commands and directory scans, so callers MUST
/// run it off the GUI thread. On the web (wasm) build the probes report nothing, so it
/// returns `false`.
#[must_use]
pub fn native_cuda_build_available(build: &str) -> bool {
    let Some(major) = cuda_major_for_build(build) else {
        return false;
    };
    detect_nvidia_gpu() && has_cuda_runtime_library_major(major) && has_cudnn9_library()
}

/// Whether `text` names an Intel GPU/accelerator (case-insensitive).
///
/// Matches the Intel vendor string and Intel GPU product families (Arc, Iris,
/// UHD/HD Graphics all carry "intel" in the controller name, and standalone Arc/Iris
/// brand names). Used only after the caller has narrowed the text to GPU/display
/// controller lines (Linux) or Win32_VideoController names (Windows), so it need not
/// re-check the device class. Pure and testable.
#[must_use]
fn text_has_intel_gpu(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("intel") || lower.contains("iris") || lower.contains(" arc ")
}

/// Whether an Intel GPU/iGPU is present on Linux (via an `lspci` display-controller
/// line matching Intel). Always false off Linux (and on wasm, where `lspci` is stubbed).
///
/// Narrows `lspci` to VGA/display/3D controller lines BEFORE the Intel match so an Intel
/// chipset/host-bridge on an AMD/NVIDIA machine is not mistaken for an Intel GPU.
fn detect_intel_gpu_linux() -> bool {
    command_output("lspci", &[]).is_some_and(|output| {
        output.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            (lower.contains("vga compatible controller")
                || lower.contains("display controller")
                || lower.contains("3d controller"))
                && text_has_intel_gpu(&lower)
        })
    })
}

/// Whether an Intel GPU is present on Windows (via a Win32_VideoController name matching
/// Intel). Always false off Windows (and on wasm).
fn detect_intel_gpu_windows() -> bool {
    windows_video_controller_output()
        .as_deref()
        .is_some_and(|output| output.lines().any(text_has_intel_gpu))
}

/// Whether a system OpenVINO runtime library (`openvino.dll` / `openvino_c*.dll`) is on
/// the Windows library search path.
///
/// The Windows OpenVINO wheel does NOT bundle the runtime, so this system library must
/// be present for the OpenVINO EP to load. Reuses [`library_search_dirs`] /
/// [`scan_dir_for_library`], matching the base and C-API loader DLLs.
fn has_openvino_runtime_library_windows() -> bool {
    library_search_dirs().iter().any(|dir| {
        scan_dir_for_library(dir, |name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("openvino") && lower.ends_with(".dll")
        })
    })
}

/// Whether the native OpenVINO execution provider can plausibly load on this system.
///
/// Availability is asymmetric by platform because the OpenVINO PYTHON WHEEL bundles the
/// runtime on Linux but NOT on Windows:
/// - Linux: available iff an Intel GPU/iGPU is present (a DRM device node `/dev/dri`
///   plus an Intel display controller in `lspci`). The bundled runtime means no system
///   OpenVINO SDK is required.
/// - Windows: available iff an Intel device is present AND a system OpenVINO runtime
///   library (`openvino.dll` / `openvino_c*.dll`) is findable on the library search
///   path (because the Windows wheel bundles no runtime).
/// - macOS / other: `false` — the OpenVINO builds are x86_64 Windows/Linux only.
///
/// Pure detection over short-lived system commands and directory scans, so callers MUST
/// run it off the GUI thread. On the web (wasm) build the probes report nothing, so it
/// returns `false`.
#[must_use]
pub fn native_openvino_runtime_available() -> bool {
    if cfg!(target_os = "linux") {
        // The Linux wheel bundles the OpenVINO runtime, so availability reduces to an
        // Intel device being present.
        return linux_gpu_device_present() && detect_intel_gpu_linux();
    }
    if cfg!(target_os = "windows") {
        // The Windows wheel does NOT bundle the runtime: a system OpenVINO library must
        // ALSO be on the search path.
        return detect_intel_gpu_windows() && has_openvino_runtime_library_windows();
    }
    false
}

/// Whether a cuDNN 9.x library is present on the library path.
///
/// Matches `libcudnn.so.9*` on Linux and `cudnn64_9*.dll` on Windows; onnxruntime
/// 1.20.1 GPU is built against cuDNN 9.
fn has_cudnn9_library() -> bool {
    let windows = cfg!(target_os = "windows");
    library_search_dirs().iter().any(|dir| {
        scan_dir_for_library(dir, |name| {
            let lower = name.to_ascii_lowercase();
            if windows {
                lower.starts_with("cudnn64_9") && lower.ends_with(".dll")
            } else {
                lower.starts_with("libcudnn.so.9")
            }
        })
    })
}

/// The directories to scan for CUDA/cuDNN shared libraries.
///
/// Starts from the platform dynamic-loader search variable (`LD_LIBRARY_PATH` on
/// Linux, `PATH` on Windows), then adds the standard toolkit/library locations
/// (`/usr/local/cuda*/lib64`, distro lib dirs, or `%CUDA_PATH%\bin`). Non-existent
/// directories are harmless — the scan simply finds nothing in them.
fn library_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    let env_key = if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    if let Some(paths) = std::env::var_os(env_key) {
        dirs.extend(std::env::split_paths(&paths));
    }

    if cfg!(target_os = "windows") {
        // The CUDA toolkit `bin` directory holds cudart64_12.dll / cudnn64_9.dll.
        if let Some(cuda_path) = std::env::var_os("CUDA_PATH") {
            dirs.push(Path::new(&cuda_path).join("bin"));
        }
    } else {
        for dir in [
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib64",
            "/usr/lib",
            "/usr/local/cuda/lib64",
            "/usr/local/cuda-12/lib64",
            "/opt/cuda/lib64",
        ] {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(cuda_home) =
            std::env::var_os("CUDA_HOME").or_else(|| std::env::var_os("CUDA_PATH"))
        {
            dirs.push(Path::new(&cuda_home).join("lib64"));
        }
    }

    dirs
}

/// Returns true if `dir` contains at least one entry whose file name satisfies
/// `matches`. A directory that cannot be read (missing/permission) yields false.
fn scan_dir_for_library<F: Fn(&str) -> bool>(dir: &Path, matches: F) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_name().to_str().is_some_and(&matches))
}

/// Detect the Apple Silicon / Mac GPU description (macOS only).
#[must_use]
pub fn detect_apple_gpu() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }

    if let Some(output) = command_output("system_profiler", &["SPDisplaysDataType"]) {
        let mut chipset: Option<String> = None;
        let mut cores: Option<String> = None;
        let mut metal: Option<String> = None;
        for line in output.lines() {
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix("Chipset Model:") {
                chipset = Some(value.trim().to_string());
            } else if let Some(value) = trimmed.strip_prefix("Total Number of Cores:") {
                cores = Some(value.trim().to_string());
            } else if let Some(value) = trimmed.strip_prefix("Metal Support:") {
                metal = Some(value.trim().to_string());
            } else if let Some(value) = trimmed.strip_prefix("Metal Family:") {
                metal = metal.or_else(|| Some(value.trim().to_string()));
            }
        }

        if let Some(name) = chipset.filter(|value| !value.is_empty()) {
            let mut extra = Vec::new();
            if let Some(cores) = cores.filter(|value| !value.is_empty()) {
                extra.push(format!("{cores} ядер GPU"));
            }
            if let Some(metal) = metal.filter(|value| !value.is_empty()) {
                extra.push(metal);
            }
            if extra.is_empty() {
                return Some(name);
            }
            return Some(format!("{name} ({})", extra.join(", ")));
        }
    }

    command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

#[must_use]
pub fn is_rocm_7_2_supported_llvm_target(target: &str) -> bool {
    let normalized = target.trim().to_ascii_lowercase();
    ROCM_7_2_SUPPORTED_LLVM_TARGETS
        .iter()
        .any(|supported| *supported == normalized)
}

fn directml_accelerator_from_name(name: &str) -> Option<DirectMlAccelerator> {
    let lower = name.to_ascii_lowercase();
    let vendor = if lower.contains("nvidia") {
        Some(GpuVendor::Nvidia)
    } else if text_has_amd_gpu(name) {
        Some(GpuVendor::Amd)
    } else if lower.contains("intel") || lower.contains("arc") || lower.contains("iris") {
        None
    } else {
        return None;
    };

    Some(DirectMlAccelerator {
        name: name.to_string(),
        vendor,
    })
}

fn detect_amd_gfx_targets_from_rocm_tools() -> Vec<String> {
    let mut targets = Vec::new();
    for (command, args) in [
        ("rocm_agent_enumerator", &[] as &[&str]),
        ("/opt/rocm/bin/rocm_agent_enumerator", &[] as &[&str]),
        ("rocminfo", &[] as &[&str]),
        ("/opt/rocm/bin/rocminfo", &[] as &[&str]),
    ] {
        if let Some(output) = command_output(command, args) {
            targets.extend(parse_gfx_targets(&output));
        }
    }
    dedupe_strings(targets)
}

fn detect_amd_gfx_targets_from_lspci() -> Vec<(String, String)> {
    command_output("lspci", &[])
        .map(|output| {
            output
                .lines()
                .filter(|line| text_has_amd_gpu(line))
                .filter_map(|line| {
                    amd_lspci_line_to_gfx_target(line).map(|target| (line.to_string(), target))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn amd_lspci_line_to_gfx_target(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let target = if lower.contains("mi355") || lower.contains("mi350") {
        "gfx950"
    } else if lower.contains("mi325") || lower.contains("mi300") {
        "gfx942"
    } else if lower.contains("mi250") || lower.contains("mi210") {
        "gfx90a"
    } else if lower.contains("mi100") {
        "gfx908"
    } else if lower.contains("r9700") || lower.contains("r9600d") || lower.contains("9070") {
        "gfx1201"
    } else if lower.contains("9060") {
        "gfx1200"
    } else if lower.contains("7900") || lower.contains("w7900") || lower.contains("w7800") {
        "gfx1100"
    } else if lower.contains("7800") || lower.contains("7700") || lower.contains("w7700") {
        "gfx1101"
    } else if lower.contains("w6800") || lower.contains("v620") {
        "gfx1030"
    } else {
        return None;
    };
    Some(target.to_string())
}

fn rocm_architecture_family(target: &str) -> Option<&'static str> {
    match target {
        "gfx950" => Some("CDNA4"),
        "gfx942" => Some("CDNA3"),
        "gfx90a" => Some("CDNA2"),
        "gfx908" => Some("CDNA"),
        "gfx1201" | "gfx1200" => Some("RDNA4"),
        "gfx1101" | "gfx1100" => Some("RDNA3"),
        "gfx1030" => Some("RDNA2"),
        _ => None,
    }
}

fn parse_version_after_keywords(text: &str, keywords: &[&str]) -> Option<RuntimeVersion> {
    let lower = text.to_ascii_lowercase();
    for keyword in keywords {
        let needle = keyword.to_ascii_lowercase();
        if let Some(idx) = lower.find(&needle) {
            let start = idx + needle.len();
            if let Some(v) = parse_first_major_minor(&text[start..]) {
                return Some(v);
            }
        }
    }
    None
}

fn parse_first_major_minor(text: &str) -> Option<RuntimeVersion> {
    for token in text.split(|ch: char| !(ch.is_ascii_digit() || ch == '.')) {
        if !token.contains('.') {
            continue;
        }
        let mut parts = token.split('.');
        let major_s = parts.next().unwrap_or_default();
        let minor_s = parts.next().unwrap_or_default();
        if major_s.is_empty() || minor_s.is_empty() {
            continue;
        }
        let major = match major_s.parse::<u32>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let minor = match minor_s.parse::<u32>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        return Some(RuntimeVersion::new(major, minor));
    }
    None
}

fn parse_min_runtime_version_from_lines(text: &str) -> Option<RuntimeVersion> {
    let mut min_value: Option<RuntimeVersion> = None;
    for line in text.lines() {
        let parsed = match parse_first_major_minor(line) {
            Some(v) => v,
            None => continue,
        };
        min_value = Some(match min_value {
            Some(current_min) => current_min.min(parsed),
            None => parsed,
        });
    }
    min_value
}

fn parse_gfx_targets(text: &str) -> Vec<String> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter_map(normalize_gfx_token)
        .collect()
}

fn normalize_gfx_token(token: &str) -> Option<String> {
    let lower = token.trim().to_ascii_lowercase();
    let start = lower.find("gfx")?;
    let tail = &lower[start..];
    let normalized = tail
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    if normalized.len() > 3 {
        Some(normalized)
    } else {
        None
    }
}

fn text_has_amd_gpu(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("amd ")
        || lower.contains(" amd ")
        || lower.contains("amd/ati")
        || lower.contains("advanced micro devices")
        || lower.contains("radeon")
}

fn windows_video_controller_output() -> Option<String> {
    if let Some(output) = command_output("wmic", &["path", "win32_VideoController", "get", "name"])
    {
        return Some(output);
    }

    command_output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name) -join \"`n\"",
        ],
    )
}

fn command_output_from_candidates(commands: &[&str], args: &[&str]) -> Option<String> {
    commands
        .iter()
        .find_map(|command| command_output(command, args))
}

// The single process-spawning primitive. On wasm there is no subprocess, so it
// returns `None` and every `detect_*` helper degrades to "nothing detected".
#[cfg(target_arch = "wasm32")]
fn command_output(_command: &str, _args: &[&str]) -> Option<String> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(command);
    apply_windows_no_window(&mut cmd);
    let output = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.to_string()
    } else if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn command_path(command: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
    })
}

fn linux_nvidia_device_nodes_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    if Path::new("/dev/nvidiactl").exists() {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/dev")
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .any(|name| {
                name.strip_prefix("nvidia")
                    .is_some_and(|suffix| suffix.chars().all(|ch| ch.is_ascii_digit()))
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn dedupe_architectures(items: Vec<GpuArchitecture>) -> Vec<GpuArchitecture> {
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if deduped.iter().any(|existing: &GpuArchitecture| {
            existing.vendor == item.vendor
                && existing.llvm_target == item.llvm_target
                && existing.name == item.name
        }) {
            continue;
        }
        deduped.push(item);
    }
    deduped
}

fn dedupe_strings(items: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if deduped.iter().any(|existing| existing == &item) {
            continue;
        }
        deduped.push(item);
    }
    deduped
}

#[cfg(target_os = "windows")]
fn apply_windows_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

// Only referenced by the native `command_output`; the `&mut Command` signature
// means it must be excluded from wasm too (there is no `Command` type there).
#[cfg(all(not(target_os = "windows"), not(target_arch = "wasm32")))]
fn apply_windows_no_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_cuda_runtime_is_internally_consistent() {
        // The probe must never panic and its `available` gate must be exactly the
        // conjunction of the three detected flags, regardless of the host.
        let status = probe_cuda_runtime();
        assert_eq!(
            status.available,
            status.nvidia_gpu && status.cuda12_runtime && status.cudnn9_runtime
        );
        assert!(!status.details.is_empty());
        // The convenience gate mirrors the struct field.
        assert_eq!(native_cuda_runtime_available(), status.available);
    }

    #[test]
    fn native_webgpu_runtime_available_is_non_panicking_and_matches_platform() {
        // The probe must never panic. On macOS it is unconditionally true; elsewhere it
        // is a heuristic over the environment, so we only assert it agrees with its own
        // building blocks rather than a fixed value (which is host-dependent).
        let available = native_webgpu_runtime_available();
        if cfg!(target_os = "macos") {
            assert!(available, "macOS always has Metal");
        } else if cfg!(target_os = "windows") {
            assert_eq!(available, has_directml_accelerator_windows());
        } else {
            assert_eq!(available, has_vulkan_loader() && linux_gpu_device_present());
        }
    }

    #[test]
    fn parse_vulkaninfo_devices_extracts_ordered_names() {
        // A captured `vulkaninfo --summary` shape with two physical devices; the parser
        // must return their deviceName values in enumeration order (GPU0 then GPU1).
        let sample = "\
==========
VULKANINFO
==========

Vulkan Instance Version: 1.3.280


Instance Extensions: count = 24
-------------------------------
\tVK_KHR_device_group_creation           : extension revision 1

Devices:
========
GPU0:
\tapiVersion         = 1.3.280
\tdriverVersion      = 23.2.1
\tvendorID           = 0x1002
\tdeviceID           = 0x73df
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
\tdeviceName         = AMD Radeon RX 6700 XT (RADV NAVI22)
GPU1:
\tapiVersion         = 1.3.280
\tdriverVersion      = 0.0.1
\tvendorID           = 0x10005
\tdeviceID           = 0x0000
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
\tdeviceName         = llvmpipe (LLVM 15.0.7, 256 bits)
";
        let devices = parse_vulkaninfo_devices(sample);
        assert_eq!(
            devices,
            vec![
                "AMD Radeon RX 6700 XT (RADV NAVI22)".to_string(),
                "llvmpipe (LLVM 15.0.7, 256 bits)".to_string(),
            ]
        );
    }

    #[test]
    fn parse_vulkaninfo_devices_returns_empty_on_unrelated_output() {
        // Output without any `GPUn:`/`deviceName` blocks must yield no fabricated names.
        assert!(parse_vulkaninfo_devices("command not found").is_empty());
        assert!(parse_vulkaninfo_devices("").is_empty());
    }

    #[test]
    fn detect_webgpu_adapters_is_non_panicking() {
        // Enumeration must never panic on any host; on wasm/macOS it is simply empty.
        let _ = detect_webgpu_adapters();
    }

    #[test]
    fn cuda_major_for_build_maps_only_cuda_builds() {
        assert_eq!(cuda_major_for_build("cuda12"), Some(12));
        assert_eq!(cuda_major_for_build("cuda13"), Some(13));
        // Non-CUDA and unknown slugs have no CUDA major.
        assert_eq!(cuda_major_for_build("cpu"), None);
        assert_eq!(cuda_major_for_build("openvino"), None);
        assert_eq!(cuda_major_for_build("webgpu"), None);
        assert_eq!(cuda_major_for_build("nope"), None);
    }

    #[test]
    fn native_cuda_build_available_is_non_panicking_and_slug_scoped() {
        // A non-CUDA slug is never CUDA-available regardless of the host hardware.
        assert!(!native_cuda_build_available("cpu"));
        assert!(!native_cuda_build_available("openvino"));
        assert!(!native_cuda_build_available("unknown"));
        // The CUDA slugs must not panic; their result is host-dependent, so we only
        // assert it agrees with the underlying detected facts.
        for slug in ["cuda12", "cuda13"] {
            let major = cuda_major_for_build(slug).expect("cuda slug has a major");
            let expected = detect_nvidia_gpu()
                && has_cuda_runtime_library_major(major)
                && has_cudnn9_library();
            assert_eq!(native_cuda_build_available(slug), expected, "{slug}");
        }
    }

    #[test]
    fn text_has_intel_gpu_matches_intel_families_only() {
        assert!(text_has_intel_gpu(
            "VGA compatible controller: Intel Corporation UHD Graphics 630"
        ));
        assert!(text_has_intel_gpu("Intel(R) Iris(R) Xe Graphics"));
        assert!(text_has_intel_gpu("Intel Arc A770"));
        assert!(text_has_intel_gpu("my arc gpu")); // standalone Arc brand
        // Non-Intel controllers must not match.
        assert!(!text_has_intel_gpu(
            "VGA compatible controller: NVIDIA Corporation GA104"
        ));
        assert!(!text_has_intel_gpu("AMD Radeon RX 7900 XT"));
        // "architecture" must not trip the bare-Arc heuristic (no surrounding spaces).
        assert!(!text_has_intel_gpu("modern gpu architecture"));
    }

    #[test]
    fn native_openvino_runtime_available_is_non_panicking_and_matches_platform() {
        // Must never panic on any host. The result is host-dependent; assert it agrees
        // with its own building blocks per platform (and is false off Windows/Linux).
        let available = native_openvino_runtime_available();
        if cfg!(target_os = "linux") {
            assert_eq!(available, linux_gpu_device_present() && detect_intel_gpu_linux());
        } else if cfg!(target_os = "windows") {
            assert_eq!(
                available,
                detect_intel_gpu_windows() && has_openvino_runtime_library_windows()
            );
        } else {
            assert!(!available, "OpenVINO builds are x86_64 Windows/Linux only");
        }
    }

    #[test]
    fn library_search_dirs_is_non_panicking() {
        // Reading missing directories must be harmless; the scan just finds nothing.
        for dir in library_search_dirs() {
            let _ = scan_dir_for_library(&dir, |_name| false);
        }
    }
}
