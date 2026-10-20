/*
File: torch/mod.rs

Purpose:
The installer's PyTorch seam: which wheels exist (pins), which are offered on this machine,
how the chosen one is installed and recorded, and whether an installed PyTorch matches the
recorded choice.

Key items:
- `TorchOs`, `TorchWheelOption`, `RocmOptionInfo`, `DriverNote`, `TorchChoicePrompt`,
  `TorchPreflightResult`, `TorchInstallSelection`: the choice data model shared by the
  installer window, the update window and the launcher settings page.
- Re-exported entry points: `TorchChoiceState` + `draw_torch_choice` (the one choice widget
  of the three screens), `detect_torch_preflight`, `run_torch_upgrade_worker`,
  `TorchReadiness`, `torch_readiness`, `required_torch_version`, `installed_torch_version`,
  `installed_torch_is_current`, `read_torch_choice_record`.

Submodules:
- `catalog`: the single pin set and the option builders.
- `choice_ui`: the one egui widget that draws a `TorchChoicePrompt` and returns the selection.
- `preflight`: GPU / adapter probing into a choice (pure builder + probing wrapper).
- `command`: selection -> `pip install` arguments; the AMD uninstall set.
- `stage`: the Torch install stage and the launcher upgrade worker.
- `readiness`: the "installed PyTorch matches the record" predicate.
- `choice_store`: read / persist the recorded choice (`General.torch_flavour` / `torch_rocm_gfx`).
- `constraints`: the `-c` constraints file that keeps the installed torch through later
  PyPI installs.

Notes:
See `MODULE_README.md` in this directory for the contracts.
*/

use ms_config::torch_choice::{GfxArch, TorchChoiceRecord, TorchFlavour};
use ms_sysprobe::amd_adapters::AmdAdapter;
use ms_sysprobe::gpu_utils::RuntimeVersion;

mod catalog;
mod choice_store;
mod choice_ui;
mod command;
mod constraints;
mod preflight;
mod readiness;
mod stage;
#[cfg(test)]
mod tests;

pub use choice_store::read_torch_choice_record;
pub use choice_ui::{TorchChoiceState, draw_torch_choice};
pub(crate) use constraints::write_torch_constraints;
pub use preflight::detect_torch_preflight;
pub use readiness::{TorchReadiness, installed_torch_is_current, installed_torch_version, required_torch_version, torch_readiness};
pub(crate) use stage::install_torch_stage;
pub use stage::run_torch_upgrade_worker;

/// The OS family a ROCm decision is made for (device-wheel list, size table, driver rule).
/// Every non-Windows target is treated as Linux; macOS never reaches a GPU decision.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TorchOs {
    Windows,
    Linux,
}

impl TorchOs {
    /// The OS this binary was built for.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(windows) { Self::Windows } else { Self::Linux }
    }
}

/// Whether the AMD Windows driver is known to be new enough for the ROCm wheels. A warning
/// shown next to the ROCm option; it never blocks the option.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverNote {
    /// No note: Linux, or a Windows driver at or above the minimum.
    NotApplicable,
    /// Windows driver version absent or unparsable: state the minimum requirement.
    Requirement,
    /// Windows driver below the minimum; `installed` is the driver's version string.
    BelowMinimum { installed: String },
}

impl DriverNote {
    /// The localized warning for this note, `None` for [`DriverNote::NotApplicable`].
    #[must_use]
    pub fn message(&self) -> Option<String> {
        match self {
            Self::NotApplicable => None,
            Self::Requirement => Some(tf!("installer.torch.driver_requirement_hint", version = catalog::MIN_WINDOWS_DRIVER_ADRENALIN)),
            Self::BelowMinimum { installed } => Some(tf!(
                "installer.torch.driver_below_minimum_hint",
                installed = installed,
                version = catalog::MIN_WINDOWS_DRIVER_ADRENALIN
            )),
        }
    }
}

/// The ROCm-specific facts of a ROCm [`TorchWheelOption`].
#[derive(Clone, Debug)]
pub struct RocmOptionInfo {
    /// Every AMD adapter found, discrete first (`ms_sysprobe::amd_adapters` order).
    pub detected: Vec<AmdAdapter>,
    /// The architecture preselected for install: the primary adapter's.
    pub default_gfx: GfxArch,
    /// The architectures the user may pick instead (AMD's device extras for this OS).
    pub supported: &'static [&'static str],
    /// Driver warning (Windows); never blocks.
    pub driver_note: DriverNote,
}

/// One installable GPU PyTorch wheel set, built only by `catalog.rs`.
#[derive(Clone, Debug)]
pub struct TorchWheelOption {
    pub(crate) flavour: TorchFlavour,
    /// Wheel tag (`cu126`, `rocm10.1.0`), shown as a tooltip.
    pub(crate) wheel_tag: String,
    /// Human-readable name of the wheel, shown verbatim in the Torch choice UI (the
    /// launcher's settings page renders the same prompt as the installer window).
    pub label: String,
    /// Runtime version the wheel needs; orders the recommendation inside a flavour.
    pub(crate) version: RuntimeVersion,
    /// Exact `--index-url` the wheels are installed from.
    pub(crate) index_url: String,
    /// Exact `torch` requirement spec. For ROCm, the spec of the DEFAULT architecture; the
    /// command rebuilds it for the selected one.
    pub(crate) torch_spec: String,
    /// Exact `torchvision` requirement spec (same ROCm rule as `torch_spec`).
    pub(crate) torchvision_spec: String,
    /// ROCm facts; `Some` exactly for a ROCm option.
    pub rocm: Option<RocmOptionInfo>,
    /// Approximate compressed download in bytes, `None` when not measured.
    pub approx_download_bytes: Option<u64>,
}

impl TorchWheelOption {
    /// The localized download-size text of this option: the estimate, "size unknown" for a
    /// ROCm option without one, and `None` for an option whose size was never measured
    /// (CUDA), which shows no size at all.
    #[must_use]
    pub fn download_size_text(&self) -> Option<String> {
        download_size_text(self.approx_download_bytes, self.flavour)
    }
}

/// The localized download-size text for an estimate of `bytes` of a `flavour` wheel set: the
/// estimate, "size unknown" for ROCm without one, `None` for an unmeasured CUDA / CPU set.
/// The one owner of this rule ([`TorchWheelOption::download_size_text`] and the choice UI's
/// per-architecture ROCm size both use it).
#[must_use]
pub(crate) fn download_size_text(bytes: Option<u64>, flavour: TorchFlavour) -> Option<String> {
    match (bytes, flavour) {
        (Some(bytes), _) => Some(tf!("installer.torch.download_size_label", size = crate::utils::format_bytes(bytes))),
        (None, TorchFlavour::Rocm) => Some(t!("installer.torch.download_size_unknown_label").to_string()),
        (None, TorchFlavour::Cuda | TorchFlavour::Cpu) => None,
    }
}

/// The GPU choice offered to the user: the options, the preselected one and a detection summary.
#[derive(Clone, Debug)]
pub struct TorchChoicePrompt {
    pub options: Vec<TorchWheelOption>,
    pub recommended_index: usize,
    pub summary: String,
}

/// Outcome of [`detect_torch_preflight`]: install CPU torch without asking (`Skip`, with the
/// localized reason) or ask the user (`Choose`).
#[derive(Debug)]
pub enum TorchPreflightResult {
    Skip { reason: String },
    Choose(TorchChoicePrompt),
}

/// What the Torch stage installs: CPU torch from the default index, a CUDA option, or the
/// ROCm option for one architecture.
///
/// Invariants checked by the command builder (`command::torch_install_pip_args`, which
/// returns an error otherwise): `InstallGpu` carries a CUDA option; `InstallRocm` carries a
/// ROCm option and a `gfx` from its `supported` list.
#[derive(Clone, Debug)]
pub enum TorchInstallSelection {
    SkipCpu,
    InstallGpu(TorchWheelOption),
    InstallRocm { option: TorchWheelOption, gfx: GfxArch },
}

impl TorchInstallSelection {
    /// The selection for picking `option` as offered: a ROCm option installs its default
    /// architecture, any other option goes through `InstallGpu`.
    #[must_use]
    pub fn for_option(option: TorchWheelOption) -> Self {
        match option.rocm.as_ref().map(|rocm| rocm.default_gfx.clone()) {
            Some(gfx) => Self::InstallRocm { option, gfx },
            None => Self::InstallGpu(option),
        }
    }

    /// The record persisted after this selection installed successfully.
    ///
    /// An `InstallGpu` with a non-CUDA option never installs (the command builder rejects
    /// it); it maps to its flavour without an architecture (`Unrecorded` for ROCm).
    #[must_use]
    pub fn record(&self) -> TorchChoiceRecord {
        match self {
            Self::SkipCpu => TorchChoiceRecord::Cpu,
            Self::InstallGpu(option) => match option.flavour {
                TorchFlavour::Cuda => TorchChoiceRecord::Cuda,
                TorchFlavour::Cpu => TorchChoiceRecord::Cpu,
                TorchFlavour::Rocm => TorchChoiceRecord::Unrecorded,
            },
            Self::InstallRocm { gfx, .. } => TorchChoiceRecord::Rocm { gfx: gfx.clone() },
        }
    }

    /// Whether this selection installs a CUDA build.
    #[must_use]
    pub(crate) fn is_cuda(&self) -> bool {
        matches!(self, Self::InstallGpu(option) if option.flavour == TorchFlavour::Cuda)
    }
}
