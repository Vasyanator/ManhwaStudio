/*
File: torch/command.rs

Purpose:
Turns a Torch selection into the exact `pip install` arguments of the Torch stage, and decides
which AMD packages must be uninstalled before it.

Key items:
- `torch_install_pip_args()`: selection -> arguments, validating the selection.
- `TorchCommandError`: a selection that cannot be installed.
- `amd_packages_to_remove()`: the AMD / ROCm packages left by another flavour or architecture.

Notes:
The arguments come only from the selected option's fields (`index_url`, `torch_spec`,
`torchvision_spec`), from `catalog::cpu_torch_specs`, or for ROCm from
`catalog::rocm_torch_specs` of the validated architecture; nothing is derived from a label here.
The runner (`uv pip install --python <py>` or `python -m pip install`) prepends its own words in
`utils.rs`.
*/

use std::collections::{BTreeSet, HashMap};

use ms_config::torch_choice::{GfxArch, TorchChoiceRecord, TorchFlavour};

use super::TorchInstallSelection;
use super::catalog::{cpu_torch_specs, rocm_torch_specs};

/// Name prefix of the AMD torch device packages (`amd-torch-device-gfx1201`, and the family
/// packages `amd-torch-device-gfx12-0` / `-gfx110x`).
const AMD_TORCH_DEVICE_PREFIX: &str = "amd-torch-device-";
/// Name prefix of the AMD torchvision device packages.
const AMD_TORCHVISION_DEVICE_PREFIX: &str = "amd-torchvision-device-";
/// Name prefix of the ROCm SDK device packages (`rocm-sdk-device-gfx1201`).
const ROCM_SDK_DEVICE_PREFIX: &str = "rocm-sdk-device-";
/// Name prefix of every ROCm SDK package (`rocm-sdk-core`, `-libraries`, `-device-*`).
const ROCM_SDK_PREFIX: &str = "rocm-sdk-";
/// Exact names of the remaining AMD ROCm runtime packages.
const ROCM_RUNTIME_PACKAGES: &[&str] = &["rocm", "rocm-bootstrap", "rocm-profiler"];
/// Triton builds of the legacy pytorch.org ROCm wheels; they share the `triton` package
/// directory with every other triton, so they are always removed before an install.
const LEGACY_ROCM_TRITON_PACKAGES: &[&str] = &["pytorch-triton-rocm", "triton-rocm"];

/// Why a [`TorchInstallSelection`] cannot be turned into an install command.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum TorchCommandError {
    /// `InstallGpu` carried a non-CUDA option (a ROCm option needs an architecture).
    NotACudaOption,
    /// `InstallRocm` carried an option that is not a ROCm option.
    NotARocmOption,
    /// `InstallRocm` named an architecture outside the option's `supported` list.
    UnsupportedArch { gfx: GfxArch },
}

impl TorchCommandError {
    /// Localized message for the installer's error screen and console.
    #[must_use]
    pub(crate) fn user_message(&self) -> String {
        match self {
            Self::NotACudaOption | Self::NotARocmOption => t!("installer.torch.selection_invalid_error").to_string(),
            Self::UnsupportedArch { gfx } => tf!("installer.torch.rocm_arch_unsupported_error", gfx = gfx),
        }
    }
}

/// The `pip install` arguments of the Torch stage for `selection`.
///
/// CPU and CUDA force a clean reinstall without the cache; CUDA appends its wheel index.
/// ROCm uses the AMD index ALONE (`--index-url` first) with the `[device-gfxN]` specs of the
/// selected architecture and no reinstall flags: the exact local-version pins replace any
/// other torch anyway, and keeping uv's cache lets a retry reuse the completed wheels.
///
/// # Errors
/// [`TorchCommandError`] when the selection breaks the invariants of
/// [`TorchInstallSelection`].
pub(crate) fn torch_install_pip_args(selection: &TorchInstallSelection) -> Result<Vec<String>, TorchCommandError> {
    match selection {
        TorchInstallSelection::SkipCpu => {
            let mut args = vec!["--force-reinstall".to_string(), "--no-cache-dir".to_string()];
            args.extend(cpu_torch_specs());
            Ok(args)
        }
        TorchInstallSelection::InstallGpu(option) => {
            if option.flavour != TorchFlavour::Cuda {
                return Err(TorchCommandError::NotACudaOption);
            }
            Ok(vec![
                "--force-reinstall".to_string(),
                "--no-cache-dir".to_string(),
                option.torch_spec.clone(),
                option.torchvision_spec.clone(),
                "--index-url".to_string(),
                option.index_url.clone(),
            ])
        }
        TorchInstallSelection::InstallRocm { option, gfx } => {
            let rocm = option.rocm.as_ref().filter(|_| option.flavour == TorchFlavour::Rocm).ok_or(TorchCommandError::NotARocmOption)?;
            if !rocm.supported.contains(&gfx.as_str()) {
                return Err(TorchCommandError::UnsupportedArch { gfx: gfx.clone() });
            }
            let [torch_spec, torchvision_spec] = rocm_torch_specs(gfx);
            Ok(vec!["--index-url".to_string(), option.index_url.clone(), torch_spec, torchvision_spec])
        }
    }
}

/// The installed AMD / ROCm packages to uninstall before installing `target`, sorted.
///
/// `installed` is a `pip freeze` snapshot keyed by normalized name. Target not ROCm: every
/// `amd-torch-device-*`, `amd-torchvision-device-*`, `rocm-sdk-*`, `rocm`, `rocm-bootstrap`,
/// `rocm-profiler`, the legacy ROCm triton builds, and a `triton` whose version names ROCm.
/// Target ROCm{gfx}: the legacy ROCm triton builds always, and every device package
/// (`amd-torch-device-*`, `amd-torchvision-device-*`, `rocm-sdk-device-*`) whose suffix is not
/// exactly `{gfx}`. Family packages (`amd-torch-device-gfx12-0`, `-gfx110x`) are therefore
/// always removed; the resolve re-pulls the right one, from uv's cache when unchanged (the
/// ROCm command keeps the cache), so a stale family or sibling-arch wheel never survives.
#[must_use]
pub(crate) fn amd_packages_to_remove(installed: &HashMap<String, String>, target: &TorchChoiceRecord) -> Vec<String> {
    let is_legacy_triton = |name: &str| LEGACY_ROCM_TRITON_PACKAGES.contains(&name);
    let remove: BTreeSet<&String> = match target {
        TorchChoiceRecord::Rocm { gfx } => {
            let stale_device_package = |name: &str| {
                [AMD_TORCH_DEVICE_PREFIX, AMD_TORCHVISION_DEVICE_PREFIX, ROCM_SDK_DEVICE_PREFIX]
                    .iter()
                    .find_map(|prefix| name.strip_prefix(prefix))
                    .is_some_and(|suffix| suffix != gfx.as_str())
            };
            installed
                .keys()
                .filter(|name| is_legacy_triton(name) || stale_device_package(name))
                .collect()
        }
        TorchChoiceRecord::Unrecorded | TorchChoiceRecord::Cpu | TorchChoiceRecord::Cuda => installed
            .iter()
            .filter(|(name, version)| {
                name.starts_with(AMD_TORCH_DEVICE_PREFIX)
                    || name.starts_with(AMD_TORCHVISION_DEVICE_PREFIX)
                    || name.starts_with(ROCM_SDK_PREFIX)
                    || ROCM_RUNTIME_PACKAGES.contains(&name.as_str())
                    || is_legacy_triton(name)
                    || (name.as_str() == "triton" && version.to_ascii_lowercase().contains("rocm"))
            })
            .map(|(name, _)| name)
            .collect(),
    };
    remove.into_iter().cloned().collect()
}
