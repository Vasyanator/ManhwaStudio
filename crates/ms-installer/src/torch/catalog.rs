/*
File: torch/catalog.rs

Purpose:
The single PyTorch pin set of the installer and the wheel options built from it.

Key items:
- `TORCH_VERSION`, `TORCHVISION_VERSION`, `PYTORCH_WHEEL_INDEX_BASE`: the CPU / CUDA pins and
  the official wheel index base.
- `AMD_WHL_NEXT_INDEX`, `ROCM_TORCH_VERSION`, `ROCM_TORCHVISION_VERSION`: the AMD ROCm pins
  (one index for Windows and Linux; the GPU kernels come from the `[device-gfxN]` extra).
- `rocm_device_extras()`: the architectures AMD publishes device wheels for, per OS
  (`TorchOs::current()` selects this machine's list).
- `rocm_download_estimate()`: measured compressed download size of the ROCm set per arch.
- `MIN_WINDOWS_DRIVER` / `MIN_WINDOWS_DRIVER_ADRENALIN`: the AMD Windows driver ROCm needs.
- `cpu_torch_specs()`, `rocm_torch_specs()`: the exact requirement specs.
- `build_cuda_torch_options()`, `build_rocm_torch_option()`: the GPU wheel options, each
  carrying its exact index URL and specs.
- `rocm_option_label()`: the ROCm option label for an architecture (also used by the choice
  UI when the user picks another architecture).
- `choose_recommended_torch_option()`: the default selection of the choice prompt.

Notes:
Every option's index URL and specs are filled here; nothing downstream derives a URL or a spec
from a label. CUDA / CPU download sizes are not measured, so those options carry no size.
*/

use ms_config::torch_choice::{GfxArch, TorchFlavour};
use ms_sysprobe::amd_adapters::AmdAdapter;
use ms_sysprobe::gpu_utils::RuntimeVersion;

use super::{DriverNote, RocmOptionInfo, TorchOs, TorchWheelOption};

/// The PyTorch version the CPU and CUDA flavours install; also the readiness minimum for
/// them and for an unrecorded install (`readiness::required_torch_version`).
pub(crate) const TORCH_VERSION: &str = "2.9.1";
/// The torchvision version installed together with [`TORCH_VERSION`].
pub(crate) const TORCHVISION_VERSION: &str = "0.24.1";
/// Base of the official PyTorch wheel indexes; a CUDA option's index is this plus its wheel tag.
pub(crate) const PYTORCH_WHEEL_INDEX_BASE: &str = "https://download.pytorch.org/whl/";

/// AMD's ROCm wheel index (`whl-next`), used ALONE for the ROCm torch install: PyPI holds
/// squatted placeholders of the ROCm package names, so the two must never share a resolve.
pub(crate) const AMD_WHL_NEXT_INDEX: &str = "https://stable.repo.amd.com/rocm/whl-next/";
/// Exact ROCm torch version (resolve verified for Windows and Linux, 2026-10-20).
pub(crate) const ROCM_TORCH_VERSION: &str = "2.13.0+rocm10.1.0";
/// Exact ROCm torchvision version paired with [`ROCM_TORCH_VERSION`].
pub(crate) const ROCM_TORCHVISION_VERSION: &str = "0.28.0+rocm10.1.0";
/// Wheel tag shown for the ROCm option (the local version tag of the ROCm pins).
const ROCM_WHEEL_TAG: &str = "rocm10.1.0";
/// ROCm stack version of the pins; orders the option inside its flavour.
const ROCM_RUNTIME_VERSION: RuntimeVersion = RuntimeVersion::new(10, 1);

/// Minimum AMD Windows driver version for ROCm 10.1 (the driver registry key's
/// `DriverVersion` of AMD Software: Adrenalin Edition 26.10.41.05).
pub(crate) const MIN_WINDOWS_DRIVER: [u32; 4] = [32, 0, 31041, 5005];
/// The marketing name of [`MIN_WINDOWS_DRIVER`], shown to the user.
pub(crate) const MIN_WINDOWS_DRIVER_ADRENALIN: &str = "26.10.41.05";

/// Architectures with AMD device wheels on Windows (the `[device-gfxN]` extras of the pins).
const ROCM_DEVICE_EXTRAS_WINDOWS: &[&str] = &[
    "gfx1010", "gfx1011", "gfx1012", "gfx1030", "gfx1031", "gfx1032", "gfx1033", "gfx1034", "gfx1035", "gfx1036",
    "gfx1100", "gfx1101", "gfx1102", "gfx1103", "gfx1150", "gfx1151", "gfx1152", "gfx1153", "gfx1200", "gfx1201",
    "gfx908", "gfx90a",
];
/// Architectures with AMD device wheels on Linux: the Windows set plus three datacenter archs.
const ROCM_DEVICE_EXTRAS_LINUX: &[&str] = &[
    "gfx1010", "gfx1011", "gfx1012", "gfx1030", "gfx1031", "gfx1032", "gfx1033", "gfx1034", "gfx1035", "gfx1036",
    "gfx1100", "gfx1101", "gfx1102", "gfx1103", "gfx1150", "gfx1151", "gfx1152", "gfx1153", "gfx1200", "gfx1201",
    "gfx908", "gfx90a", "gfx1250", "gfx942", "gfx950",
];

/// The `torch` and `torchvision` specs of the CPU install (default PyPI index).
#[must_use]
pub(crate) fn cpu_torch_specs() -> [String; 2] {
    [format!("torch=={TORCH_VERSION}"), format!("torchvision=={TORCHVISION_VERSION}")]
}

/// The `torch` and `torchvision` specs of the ROCm install for `gfx`:
/// `torch[device-gfxN]==<pin>` and `torchvision[device-gfxN]==<pin>`. The extra pulls the
/// architecture's device packages; a bare `torch` from this index has no GPU kernels.
#[must_use]
pub(crate) fn rocm_torch_specs(gfx: &GfxArch) -> [String; 2] {
    let extra = gfx.extra_name();
    [format!("torch[{extra}]=={ROCM_TORCH_VERSION}"), format!("torchvision[{extra}]=={ROCM_TORCHVISION_VERSION}")]
}

/// The architectures AMD publishes ROCm device wheels for on `os`.
#[must_use]
pub(crate) fn rocm_device_extras(os: TorchOs) -> &'static [&'static str] {
    match os {
        TorchOs::Windows => ROCM_DEVICE_EXTRAS_WINDOWS,
        TorchOs::Linux => ROCM_DEVICE_EXTRAS_LINUX,
    }
}

/// Approximate compressed download of the ROCm set for `gfx` on `os`, in bytes.
///
/// Measured with HTTP HEAD over the resolved wheel set (research `amd_windows_torch_plan.md`
/// §9.1; torch 2.13 within 2% of it): gfx120x 1.55 GB Windows / 1.89 GB Linux, gfx110x
/// 1.57 / 1.91, gfx101x and gfx103x 1.11 / 1.84 (decimal GB; Linux includes triton). Families
/// not measured (gfx115x, gfx90x, gfx94x / 95x, gfx1250) return `None` ("size unknown").
#[must_use]
pub(crate) fn rocm_download_estimate(os: TorchOs, gfx: &GfxArch) -> Option<u64> {
    let name = gfx.as_str();
    let (windows, linux) = if name.starts_with("gfx120") {
        (1_550_000_000, 1_890_000_000)
    } else if name.starts_with("gfx110") {
        (1_570_000_000, 1_910_000_000)
    } else if name.starts_with("gfx101") || name.starts_with("gfx103") {
        (1_110_000_000, 1_840_000_000)
    } else {
        return None;
    };
    Some(match os {
        TorchOs::Windows => windows,
        TorchOs::Linux => linux,
    })
}

/// The driver note of a ROCm option on `os` for the adapter's driver version string.
///
/// Linux needs no driver version (amdgpu + `/dev/kfd` are checked by the preflight):
/// `NotApplicable`. Windows: an absent or unparsable version gives `Requirement` (state the
/// minimum), a version below [`MIN_WINDOWS_DRIVER`] gives `BelowMinimum`, otherwise
/// `NotApplicable`. A warning only; it never removes the option.
#[must_use]
pub(crate) fn driver_note(os: TorchOs, driver_version: Option<&str>) -> DriverNote {
    if os == TorchOs::Linux {
        return DriverNote::NotApplicable;
    }
    match driver_version.and_then(|text| ms_sysprobe::amd_adapters::parse_windows_driver_version(text).map(|parsed| (text, parsed))) {
        None => DriverNote::Requirement,
        Some((text, parsed)) if parsed < MIN_WINDOWS_DRIVER => DriverNote::BelowMinimum { installed: text.trim().to_string() },
        Some(_) => DriverNote::NotApplicable,
    }
}

/// Builds one CUDA option from the official index `wheel_tag` with the shared pins.
fn pytorch_index_option(wheel_tag: &str, label: String, version: RuntimeVersion) -> TorchWheelOption {
    let [torch_spec, torchvision_spec] = cpu_torch_specs();
    TorchWheelOption {
        flavour: TorchFlavour::Cuda,
        wheel_tag: wheel_tag.to_string(),
        label,
        version,
        index_url: format!("{PYTORCH_WHEEL_INDEX_BASE}{wheel_tag}"),
        torch_spec,
        torchvision_spec,
        rocm: None,
        approx_download_bytes: None,
    }
}

/// The CUDA wheel options usable with CUDA runtime `cuda_version` and, when known, the
/// NVIDIA compute capability `cuda_capability`.
///
/// Options whose CUDA version exceeds the runtime are dropped; SM below 6.1 gets none and
/// SM below 7.5 only `cu126`. Ordered by CUDA version, ascending.
#[must_use]
pub(crate) fn build_cuda_torch_options(
    cuda_version: RuntimeVersion,
    cuda_capability: Option<RuntimeVersion>,
) -> Vec<TorchWheelOption> {
    let variants = [
        (RuntimeVersion::new(12, 6), "CUDA 12.6", "cu126"),
        (RuntimeVersion::new(12, 8), "CUDA 12.8", "cu128"),
        (RuntimeVersion::new(13, 0), "CUDA 13.0", "cu130"),
    ];

    let mut options = variants
        .into_iter()
        .filter(|(required, _, _)| *required <= cuda_version)
        .map(|(required, label, wheel_tag)| pytorch_index_option(wheel_tag, format!("{label} ({wheel_tag})"), required))
        .collect::<Vec<_>>();

    if let Some(capability) = cuda_capability {
        if capability < RuntimeVersion::new(6, 1) {
            return Vec::new();
        }
        if capability < RuntimeVersion::new(7, 5) {
            options.retain(|option| option.version == RuntimeVersion::new(12, 6));
        }
    }

    options
}

/// The localized label of the ROCm option installing `gfx` ("AMD ROCm gfx1201 (PyTorch 2.13.0)").
/// The option's own `label` names its default architecture; the choice UI relabels the row
/// with this for the architecture the user picked.
#[must_use]
pub(crate) fn rocm_option_label(gfx: &GfxArch) -> String {
    let release = ROCM_TORCH_VERSION.split_once('+').map_or(ROCM_TORCH_VERSION, |(release, _)| release);
    tf!("installer.torch.rocm_option_label", gfx = gfx, version = release)
}

/// The ROCm option for `default_gfx` on `os`: the AMD index, the specs of `default_gfx`, the
/// detected adapters, the architectures the user may pick instead, the download estimate and
/// the driver note of `driver_version` (Windows).
///
/// The caller guarantees `default_gfx` is one of [`rocm_device_extras`]`(os)`.
#[must_use]
pub(crate) fn build_rocm_torch_option(
    os: TorchOs,
    default_gfx: GfxArch,
    detected: Vec<AmdAdapter>,
    driver_version: Option<&str>,
) -> TorchWheelOption {
    let [torch_spec, torchvision_spec] = rocm_torch_specs(&default_gfx);
    TorchWheelOption {
        flavour: TorchFlavour::Rocm,
        wheel_tag: ROCM_WHEEL_TAG.to_string(),
        label: rocm_option_label(&default_gfx),
        version: ROCM_RUNTIME_VERSION,
        index_url: AMD_WHL_NEXT_INDEX.to_string(),
        torch_spec,
        torchvision_spec,
        approx_download_bytes: rocm_download_estimate(os, &default_gfx),
        rocm: Some(RocmOptionInfo {
            detected,
            default_gfx,
            supported: rocm_device_extras(os),
            driver_note: driver_note(os, driver_version),
        }),
    }
}

/// Index of the option the choice prompt preselects: CUDA before ROCm, then the highest
/// runtime version inside the flavour. `0` for an empty list.
#[must_use]
pub(crate) fn choose_recommended_torch_option(options: &[TorchWheelOption]) -> usize {
    options
        .iter()
        .enumerate()
        .max_by_key(|(_, option)| {
            let flavour_rank = match option.flavour {
                TorchFlavour::Cuda => 2_u8,
                TorchFlavour::Rocm => 1_u8,
                TorchFlavour::Cpu => 0_u8,
            };
            (flavour_rank, option.version)
        })
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}
