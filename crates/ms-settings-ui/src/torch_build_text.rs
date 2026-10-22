/*
FILE OVERVIEW: crates/ms-settings-ui/src/torch_build_text.rs
GUI-free localized texts for the Python backend's PyTorch build report
(`ms_local_ai::backend_health::TorchBuildInfo`, the health snapshot's `torch_build`).

Purpose:
The one owner of how a reported torch build is put into words, so the AI backend pane's
diagnostics line and the launcher's system-information tab show the same text.

Key functions:
- `torch_build_summary()`: flavour + version · HIP / CUDA runtime · primary GPU.
- `torch_build_line()`: the summary inside the "PyTorch: …" line.
- `torch_build_unreadable_text()`: the line for a report the snapshot could not parse.
- `torch_build_kernel_warning()`: the warning when the main GPU's arch has no installed ROCm
  kernel wheel (`TorchBuildInfo::kernel_mismatch`).

Notes:
Pure functions over in-memory data (no I/O, no egui); cheap enough for every frame. Colours and
layout stay with the caller (`ai_backend_panel::draw_torch_build_lines`).
*/

use ms_config::torch_choice::TorchFlavour;
use ms_local_ai::backend_health::{TorchBuildError, TorchBuildInfo};

/// One-line summary of a torch build: flavour and version, the HIP / CUDA runtime version,
/// and the primary GPU (name and AMD arch), joined by " · " like the other pane summaries.
/// A non-CPU build without a visible GPU says so; a CPU build names no GPU.
#[must_use]
pub fn torch_build_summary(build: &TorchBuildInfo) -> String {
    let flavour = match build.flavour {
        TorchFlavour::Cpu => t!("ai_backend.torch_build_flavour_cpu_label"),
        TorchFlavour::Cuda => t!("ai_backend.torch_build_flavour_cuda_label"),
        TorchFlavour::Rocm => t!("ai_backend.torch_build_flavour_rocm_label"),
    };
    let mut parts = vec![tf!("ai_backend.torch_build_flavour_version_part", flavour = flavour, version = build.version.as_str())];
    // A ROCm wheel may also report a CUDA version; HIP is the runtime it actually uses.
    if let Some(hip) = &build.hip_version {
        parts.push(tf!("ai_backend.torch_build_hip_part", version = hip.as_str()));
    } else if let Some(cuda) = &build.cuda_version {
        parts.push(tf!("ai_backend.torch_build_cuda_part", version = cuda.as_str()));
    }
    match build.primary_device() {
        Some(device) => {
            let name = device.name.trim();
            match (&device.gcn_arch, name.is_empty()) {
                (Some(arch), false) => parts.push(tf!("ai_backend.torch_build_gpu_arch_part", name = name, arch = arch.as_str())),
                (Some(arch), true) => parts.push(tf!("ai_backend.torch_build_gpu_part", name = arch.as_str())),
                (None, false) => parts.push(tf!("ai_backend.torch_build_gpu_part", name = name)),
                (None, true) => {}
            }
        }
        None if build.flavour != TorchFlavour::Cpu => parts.push(t!("ai_backend.torch_build_no_gpu_part").to_string()),
        None => {}
    }
    parts.join(" · ")
}

/// The localized "PyTorch: {summary}" line for a readable build report
/// ([`torch_build_summary`] inside its label).
#[must_use]
pub fn torch_build_line(build: &TorchBuildInfo) -> String {
    tf!("ai_backend.torch_build_line_label", build = torch_build_summary(build))
}

/// The localized line for a build report the health snapshot could not parse; the error's
/// technical `Display` form is embedded verbatim.
#[must_use]
pub fn torch_build_unreadable_text(err: &TorchBuildError) -> String {
    tf!("ai_backend.torch_build_unreadable_status", error = err)
}

/// The localized warning for a split-kernel ROCm build whose main GPU's arch has no
/// installed kernel wheel ([`TorchBuildInfo::kernel_mismatch`]): the "no kernels at all"
/// text when none is installed, else the text naming the installed archs. `None` when the
/// build covers its main GPU (or the rule does not apply: CPU / CUDA / bundled-kernel ROCm).
#[must_use]
pub fn torch_build_kernel_warning(build: &TorchBuildInfo) -> Option<String> {
    let mismatch = build.kernel_mismatch()?;
    Some(if mismatch.installed.is_empty() {
        tf!("ai_backend.torch_build_no_kernels_warning", device = mismatch.device)
    } else {
        // Arch names are LLVM processor ids, never localized; ", " joins a list of them.
        let installed = mismatch.installed.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        tf!("ai_backend.torch_build_kernel_mismatch_warning", device = mismatch.device, installed = installed)
    })
}

#[cfg(test)]
mod tests {
    use super::{torch_build_kernel_warning, torch_build_summary};
    use ms_config::torch_choice::{GfxArch, TorchFlavour};
    use ms_local_ai::backend_health::{TorchBuildInfo, TorchGpuInfo};

    fn gfx(name: &str) -> GfxArch {
        GfxArch::parse(name).unwrap_or_else(|| panic!("{name} must parse"))
    }

    fn build(flavour: TorchFlavour, devices: Vec<TorchGpuInfo>, rocm_device_archs: Vec<GfxArch>, split: bool) -> TorchBuildInfo {
        TorchBuildInfo {
            flavour,
            version: "2.13.0".to_string(),
            hip_version: (flavour == TorchFlavour::Rocm).then(|| "10.1".to_string()),
            cuda_version: (flavour == TorchFlavour::Cuda).then(|| "12.6".to_string()),
            devices,
            rocm_device_archs,
            rocm_split_kernels: split,
        }
    }

    fn amd_gpu(arch: &str) -> TorchGpuInfo {
        TorchGpuInfo { index: 0, name: "Radeon".to_string(), gcn_arch: Some(gfx(arch)), integrated: false }
    }

    /// Part count per shape: CPU has flavour only; a GPU build adds runtime and GPU (or the
    /// "no GPU visible" part). Locale-independent: counts the " · " separators.
    #[test]
    fn summary_parts_follow_the_build_shape() {
        let parts = |info: &TorchBuildInfo| torch_build_summary(info).split(" · ").count();
        assert_eq!(parts(&build(TorchFlavour::Cpu, Vec::new(), Vec::new(), false)), 1);
        assert_eq!(parts(&build(TorchFlavour::Cuda, Vec::new(), Vec::new(), false)), 3);
        assert_eq!(parts(&build(TorchFlavour::Rocm, vec![amd_gpu("gfx1100")], Vec::new(), true)), 3);
    }

    /// The warning exists only for a split-kernel ROCm build missing its main GPU's arch,
    /// and picks the "no kernels" text when nothing is installed.
    #[test]
    fn kernel_warning_follows_the_mismatch_rule() {
        let covered = build(TorchFlavour::Rocm, vec![amd_gpu("gfx1100")], vec![gfx("gfx1100")], true);
        assert_eq!(torch_build_kernel_warning(&covered), None);
        let bundled = build(TorchFlavour::Rocm, vec![amd_gpu("gfx1100")], Vec::new(), false);
        assert_eq!(torch_build_kernel_warning(&bundled), None);

        let bare = build(TorchFlavour::Rocm, vec![amd_gpu("gfx1100")], Vec::new(), true);
        assert_eq!(torch_build_kernel_warning(&bare), Some(tf!("ai_backend.torch_build_no_kernels_warning", device = gfx("gfx1100"))));
        let other = build(TorchFlavour::Rocm, vec![amd_gpu("gfx1100")], vec![gfx("gfx1030"), gfx("gfx1201")], true);
        assert_eq!(
            torch_build_kernel_warning(&other),
            Some(tf!("ai_backend.torch_build_kernel_mismatch_warning", device = gfx("gfx1100"), installed = "gfx1030, gfx1201"))
        );
    }
}
