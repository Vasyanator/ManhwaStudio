/*
File: torch/preflight.rs

Purpose:
Probes the machine's GPUs and decides which PyTorch wheels to offer.

Key items:
- `detect_torch_preflight()`: `Skip` (CPU torch, with the reason) or `Choose` (the GPU options
  from `catalog.rs` plus the recommended one and a detection summary).
- `GpuFacts`, `NvidiaFacts`, `TorchOptionPlan`, `build_torch_options()`: the pure decision from
  probed facts, unit-tested without hardware.

Notes:
`detect_torch_preflight` runs probes (`ms_sysprobe::gpu_utils` spawns processes,
`ms_sysprobe::amd_adapters` reads sysfs / SetupAPI): call it on a worker thread, never on the
GUI thread. AMD ROCm is offered on Windows and Linux from the AMD whl-next wheels, which need
no system ROCm: Linux requires the amdgpu driver (the adapter enumeration only reports amdgpu
adapters) and `/dev/kfd`; Windows requires a known device id. An unknown device id gives no
ROCm option and an explicit reason, never a guess.
*/

use ms_sysprobe::amd_adapters::{AmdAdapter, detect_amd_adapters, linux_kfd_available};
use ms_sysprobe::gpu_utils::{RuntimeVersion, detect_cuda_runtime_version, detect_nvidia_compute_capability, detect_nvidia_gpu};

use super::catalog::{build_cuda_torch_options, build_rocm_torch_option, choose_recommended_torch_option, rocm_device_extras};
use super::{TorchChoicePrompt, TorchOs, TorchPreflightResult, TorchWheelOption};

/// What the NVIDIA probes found (only built when an NVIDIA GPU is present).
#[derive(Debug)]
pub(crate) struct NvidiaFacts {
    /// CUDA runtime version the driver supports, when detected.
    pub(crate) cuda_version: Option<RuntimeVersion>,
    /// Compute capability (SM) of the GPU, when detected.
    pub(crate) capability: Option<RuntimeVersion>,
}

/// Every probed fact the option decision depends on.
#[derive(Debug)]
pub(crate) struct GpuFacts {
    pub(crate) os: TorchOs,
    /// `Some` when an NVIDIA GPU is present.
    pub(crate) nvidia: Option<NvidiaFacts>,
    /// AMD adapters, discrete first (`detect_amd_adapters` order).
    pub(crate) amd_adapters: Vec<AmdAdapter>,
    /// Linux: `/dev/kfd` exists. Ignored on Windows.
    pub(crate) linux_kfd: bool,
}

/// The decision of [`build_torch_options`]: offered options, detection summary parts and
/// localized reasons a GPU flavour was not offered.
#[derive(Debug, Default)]
pub(crate) struct TorchOptionPlan {
    pub(crate) options: Vec<TorchWheelOption>,
    pub(crate) detected: Vec<String>,
    pub(crate) failures: Vec<String>,
}

/// Decides the GPU options for `facts`. Pure (no probes).
///
/// NVIDIA: the CUDA options of the detected runtime and SM (`catalog::build_cuda_torch_options`).
/// AMD: one ROCm option for the primary adapter (the first in discrete-first order) when its
/// gfx is known, is one of the OS's device extras and — on Linux — `/dev/kfd` exists;
/// otherwise a reason (unknown device id with its hex id, unsupported architecture, KFD
/// missing) is pushed to `failures`.
#[must_use]
pub(crate) fn build_torch_options(facts: &GpuFacts) -> TorchOptionPlan {
    let mut plan = TorchOptionPlan::default();

    if let Some(nvidia) = &facts.nvidia {
        if let Some(capability) = nvidia.capability {
            plan.detected.push(format!("NVIDIA SM {capability}"));
        } else {
            plan.failures.push(t!("installer.utils.nvidia_no_sm_status").to_string());
        }
        if let Some(cuda_version) = nvidia.cuda_version {
            plan.detected.push(format!("CUDA {cuda_version}"));
            let cuda_options = build_cuda_torch_options(cuda_version, nvidia.capability);
            if cuda_options.is_empty()
                && let Some(capability) = nvidia.capability
            {
                if capability < RuntimeVersion::new(6, 1) {
                    plan.failures.push(tf!("installer.utils.nvidia_sm_too_low_error", capability = capability));
                } else if capability < RuntimeVersion::new(7, 5) {
                    plan.failures.push(tf!("installer.utils.nvidia_sm_cuda_mismatch_error", capability = capability));
                }
            }
            plan.options.extend(cuda_options);
        } else {
            plan.failures.push(t!("installer.utils.nvidia_no_cuda_version_status").to_string());
        }
    }

    if let Some(primary) = facts.amd_adapters.first() {
        match &primary.gfx {
            None => plan.failures.push(tf!(
                "installer.torch.amd_unknown_device_status",
                name = primary.name,
                device_id = format!("{:04x}", primary.device_id)
            )),
            Some(gfx) => {
                plan.detected.push(format!("AMD {} ({gfx})", primary.name));
                if !rocm_device_extras(facts.os).contains(&gfx.as_str()) {
                    plan.failures.push(tf!("installer.torch.amd_unsupported_arch_status", name = primary.name, gfx = gfx));
                } else if facts.os == TorchOs::Linux && !facts.linux_kfd {
                    plan.failures.push(t!("installer.torch.amd_kfd_missing_status").to_string());
                } else {
                    plan.options.push(build_rocm_torch_option(
                        facts.os,
                        gfx.clone(),
                        facts.amd_adapters.clone(),
                        primary.driver_version.as_deref(),
                    ));
                }
            }
        }
    }

    plan
}

/// Turns a plan into the preflight answer: `Skip` with the reasons when nothing is offered,
/// otherwise `Choose` with the recommended option and the detection summary.
fn preflight_from_plan(plan: TorchOptionPlan) -> TorchPreflightResult {
    let TorchOptionPlan { options, detected, failures } = plan;
    if options.is_empty() {
        let reason = if failures.is_empty() {
            t!("installer.utils.cuda_rocm_below_min_status").to_string()
        } else {
            tf!("installer.utils.skip_pytorch_cpu_status", failures = failures.join("; "))
        };
        return TorchPreflightResult::Skip { reason };
    }

    let recommended_index = choose_recommended_torch_option(&options);
    let summary = if detected.is_empty() {
        t!("installer.utils.gpu_variants_available_status").to_string()
    } else {
        tf!("installer.utils.detected_status", detected = detected.join(", "))
    };
    TorchPreflightResult::Choose(TorchChoicePrompt { options, recommended_index, summary })
}

/// Detects the GPUs and returns the PyTorch choice to present.
///
/// macOS and machines without an NVIDIA or AMD GPU skip to CPU torch. Otherwise the CUDA /
/// ROCm options of [`build_torch_options`] are offered; when none is usable the result is
/// `Skip` with the collected reasons. Blocking: spawns probe processes and reads the system.
pub fn detect_torch_preflight() -> TorchPreflightResult {
    if cfg!(target_os = "macos") {
        return TorchPreflightResult::Skip {
            reason: t!("installer.utils.macos_skip_gpu_wheels_status").to_string(),
        };
    }

    let has_nvidia = detect_nvidia_gpu();
    let amd_adapters = detect_amd_adapters();
    if !has_nvidia && amd_adapters.is_empty() {
        return TorchPreflightResult::Skip {
            reason: t!("installer.utils.no_gpu_keep_cpu_status").to_string(),
        };
    }

    let nvidia = has_nvidia.then(|| NvidiaFacts {
        cuda_version: detect_cuda_runtime_version(),
        capability: detect_nvidia_compute_capability(),
    });
    let os = TorchOs::current();
    let linux_kfd = os == TorchOs::Linux && linux_kfd_available();
    for adapter in &amd_adapters {
        ms_log::runtime_log::log_info(format!(
            "[torch] AMD adapter {:04x}:{:04x} '{}' gfx={:?} source={:?} integrated={:?} driver={:?}",
            adapter.vendor_id, adapter.device_id, adapter.name, adapter.gfx, adapter.gfx_source, adapter.integrated, adapter.driver_version
        ));
    }
    preflight_from_plan(build_torch_options(&GpuFacts { os, nvidia, amd_adapters, linux_kfd }))
}
