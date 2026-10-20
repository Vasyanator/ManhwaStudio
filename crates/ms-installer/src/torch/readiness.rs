/*
File: torch/readiness.rs

Purpose:
The single "does the installed PyTorch match the recorded choice?" decision, shared by the
`--check-venv` readiness check (`venv_check.rs`), the repair worker and the update
continuation (`utils.rs`), and the post-install check of the Torch stage.

Key items:
- `TorchReadiness`: current, or the typed reason it is not.
- `torch_readiness()`, `installed_torch_is_current()`, `required_torch_version()`,
  `installed_torch_version()`.

Notes:
Input is a `pip freeze` snapshot keyed by normalized package name
(`utils::freeze_installed_packages`) plus the recorded choice
(`choice_store::read_torch_choice_record`). `Unrecorded` keeps the legacy version-only rule,
so an install made before the record existed is never re-prompted.
*/

use std::collections::HashMap;

use ms_config::torch_choice::{GfxArch, TorchChoiceRecord, TorchFlavour};
use ms_config::version_format::compare_release_versions;

use super::catalog::{ROCM_TORCH_VERSION, ROCM_TORCHVISION_VERSION, TORCH_VERSION};

/// Package name of PyTorch itself.
///
/// PyTorch is not part of the torch-dependent extras and is deliberately absent from
/// [`crate::utils::required_dependency_specs`]: unlike every other dependency it has a VERSION
/// and FLAVOUR requirement, not a presence requirement, so it is expressed by
/// [`torch_readiness`] instead.
const TORCH_PACKAGE_NAME: &str = "torch";
/// Package name of torchvision (pinned exactly together with a ROCm torch).
const TORCHVISION_PACKAGE_NAME: &str = "torchvision";
/// The ROCm SDK runtime package every AMD whl-next torch needs.
const ROCM_SDK_CORE_PACKAGE_NAME: &str = "rocm-sdk-core";

/// Whether the installed PyTorch satisfies the recorded choice, and if not, why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TorchReadiness {
    /// The installed PyTorch satisfies the record.
    Current,
    /// No PyTorch (or, for ROCm, no torchvision) is installed.
    Missing,
    /// A PyTorch (or ROCm torchvision) of the right flavour but the wrong version.
    Outdated { installed: String, required: String },
    /// The installed build is not of the recorded flavour; `installed` is `None` for an
    /// untagged build.
    FlavourMismatch { installed: Option<TorchFlavour>, expected: TorchFlavour },
    /// A ROCm torch without the device packages of the recorded architecture (no GPU kernels).
    DevicePackagesMissing { gfx: GfxArch },
}

impl TorchReadiness {
    /// Localized, user-facing explanation; `None` for [`TorchReadiness::Current`].
    ///
    /// `required` is the version shown for [`TorchReadiness::Missing`]
    /// ([`required_torch_version`] of the record).
    #[must_use]
    pub fn user_message(&self, required: &str) -> Option<String> {
        match self {
            Self::Current => None,
            Self::Missing => Some(tf!("venv_check.reason_torch_missing", required = required)),
            Self::Outdated { installed, required } => {
                Some(tf!("venv_check.reason_torch_outdated", installed = installed, required = required))
            }
            Self::FlavourMismatch { installed, expected } => Some(tf!(
                "venv_check.reason_torch_flavour_mismatch",
                installed = installed.map_or_else(|| t!("venv_check.torch_flavour_untagged_label").to_string(), |flavour| flavour_display_name(flavour).to_string()),
                expected = flavour_display_name(*expected)
            )),
            Self::DevicePackagesMissing { gfx } => Some(tf!("venv_check.reason_torch_device_missing", gfx = gfx)),
        }
    }
}

/// Technical name of a flavour for messages (`CPU`, `CUDA`, `ROCm`; product names, not
/// translated).
fn flavour_display_name(flavour: TorchFlavour) -> &'static str {
    match flavour {
        TorchFlavour::Cpu => "CPU",
        TorchFlavour::Cuda => "CUDA",
        TorchFlavour::Rocm => "ROCm",
    }
}

/// Returns the installed PyTorch version, if the environment has one.
///
/// `installed` is a `pip freeze` snapshot keyed by normalized package name (see
/// [`crate::utils::freeze_installed_packages`]).
#[must_use]
pub fn installed_torch_version(installed: &HashMap<String, String>) -> Option<&str> {
    installed.get(TORCH_PACKAGE_NAME).map(String::as_str)
}

/// The PyTorch version `record` requires: the exact ROCm pin for `Rocm`, otherwise the
/// minimum [`TORCH_VERSION`].
#[must_use]
pub fn required_torch_version(record: &TorchChoiceRecord) -> &'static str {
    match record {
        TorchChoiceRecord::Rocm { .. } => ROCM_TORCH_VERSION,
        TorchChoiceRecord::Unrecorded | TorchChoiceRecord::Cpu | TorchChoiceRecord::Cuda => TORCH_VERSION,
    }
}

/// Decides whether `installed` satisfies `record`.
///
/// - `Unrecorded`: the legacy rule — any flavour at or above [`TORCH_VERSION`].
/// - `Cpu`: an untagged or `+cpu` build at or above [`TORCH_VERSION`].
/// - `Cuda`: a `+cu*` build at or above [`TORCH_VERSION`].
/// - `Rocm{gfx}`: torch and torchvision EXACTLY at the ROCm pins, plus
///   `amd-torch-device-{gfx}`, `amd-torchvision-device-{gfx}` and `rocm-sdk-core`.
///
/// Checks run in the order Missing, FlavourMismatch, Outdated, DevicePackagesMissing. A
/// version that does not parse compares BELOW the minimum (`compare_release_versions`), so
/// an unreadable version fails closed (an extra repair offer, never a false "ready").
#[must_use]
pub fn torch_readiness(installed: &HashMap<String, String>, record: &TorchChoiceRecord) -> TorchReadiness {
    let Some(torch) = installed_torch_version(installed) else {
        return TorchReadiness::Missing;
    };
    let tagged = TorchFlavour::from_local_version(torch);
    let at_least_minimum = || {
        if compare_release_versions(torch, TORCH_VERSION).is_ge() {
            TorchReadiness::Current
        } else {
            TorchReadiness::Outdated { installed: torch.to_string(), required: TORCH_VERSION.to_string() }
        }
    };
    match record {
        TorchChoiceRecord::Unrecorded => at_least_minimum(),
        TorchChoiceRecord::Cpu => match tagged {
            None | Some(TorchFlavour::Cpu) => at_least_minimum(),
            Some(other @ (TorchFlavour::Cuda | TorchFlavour::Rocm)) => {
                TorchReadiness::FlavourMismatch { installed: Some(other), expected: TorchFlavour::Cpu }
            }
        },
        TorchChoiceRecord::Cuda => match tagged {
            Some(TorchFlavour::Cuda) => at_least_minimum(),
            None | Some(TorchFlavour::Cpu | TorchFlavour::Rocm) => {
                TorchReadiness::FlavourMismatch { installed: tagged, expected: TorchFlavour::Cuda }
            }
        },
        TorchChoiceRecord::Rocm { gfx } => rocm_readiness(installed, torch, tagged, gfx),
    }
}

/// The `Rocm{gfx}` branch of [`torch_readiness`]; `torch` is the installed torch version.
fn rocm_readiness(installed: &HashMap<String, String>, torch: &str, tagged: Option<TorchFlavour>, gfx: &GfxArch) -> TorchReadiness {
    if tagged != Some(TorchFlavour::Rocm) {
        return TorchReadiness::FlavourMismatch { installed: tagged, expected: TorchFlavour::Rocm };
    }
    if torch != ROCM_TORCH_VERSION {
        return TorchReadiness::Outdated { installed: torch.to_string(), required: ROCM_TORCH_VERSION.to_string() };
    }
    let Some(torchvision) = installed.get(TORCHVISION_PACKAGE_NAME) else {
        return TorchReadiness::Missing;
    };
    if torchvision != ROCM_TORCHVISION_VERSION {
        return TorchReadiness::Outdated {
            installed: format!("{TORCHVISION_PACKAGE_NAME} {torchvision}"),
            required: format!("{TORCHVISION_PACKAGE_NAME} {ROCM_TORCHVISION_VERSION}"),
        };
    }
    let device_packages = [
        format!("amd-torch-device-{}", gfx.as_str()),
        format!("amd-torchvision-device-{}", gfx.as_str()),
        ROCM_SDK_CORE_PACKAGE_NAME.to_string(),
    ];
    if device_packages.iter().all(|name| installed.contains_key(name)) {
        TorchReadiness::Current
    } else {
        TorchReadiness::DevicePackagesMissing { gfx: gfx.clone() }
    }
}

/// Whether `installed` satisfies `record` ([`torch_readiness`] is
/// [`TorchReadiness::Current`]).
///
/// This is the SINGLE predicate behind "is this environment ready?" (the `--check-venv`
/// flow), "may the repair worker skip the Torch stage?" and "is the update's PyTorch
/// current?". They must never disagree: a readiness answer of "ready" has to imply that the
/// worker would have nothing left to do.
#[must_use]
pub fn installed_torch_is_current(installed: &HashMap<String, String>, record: &TorchChoiceRecord) -> bool {
    torch_readiness(installed, record) == TorchReadiness::Current
}
