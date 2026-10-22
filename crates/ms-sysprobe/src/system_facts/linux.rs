/*
File: crates/ms-sysprobe/src/system_facts/linux.rs

Purpose:
Linux half of `system_facts`: OS name from `/etc/os-release`, kernel from
`/proc/sys/kernel/osrelease`, CPU from `/proc/cpuinfo`, GPUs from the shared PCI walk
(`pci_gpus`) with names for the devices nobody else names from at most one `lspci -mm -nn`.

Key functions:
- probe(): the whole Linux report.

Notes:
`lspci` runs only when an adapter is still unnamed after the walk and `nvidia-smi` (an AMD
card is named by the pinned table, an NVIDIA card by `nvidia-smi`), under `LSPCI_TIMEOUT`.
*/

use std::time::Duration;

use ms_log::runtime_log;
use ms_memory::MemoryAvailability;

use crate::gpu_utils::{CommandRunError, run_command_bounded};

use super::parse::{parse_cpuinfo, parse_lspci_display_names, parse_os_release};
use super::{CpuFacts, GpuAdapterFacts, GpuEnumeration, GpuFacts, OsFacts, PROC_CPUINFO, SystemFacts, kernel_release, pci_gpus, read_text};

/// The distribution identity file (systemd; present on every current distribution).
const OS_RELEASE: &str = "/etc/os-release";
/// The fallback location of the same file (`os-release(5)`).
const OS_RELEASE_FALLBACK: &str = "/usr/lib/os-release";
/// Upper bound of the `lspci` run (it answers in ~0.1 s).
const LSPCI_TIMEOUT: Duration = Duration::from_secs(5);

/// Collects the Linux report. Blocking; never panics.
pub(super) fn probe(memory: Option<MemoryAvailability>, logical_cores: Option<usize>) -> SystemFacts {
    let cpuinfo = read_text(PROC_CPUINFO).map(|text| parse_cpuinfo(&text));
    let (mut adapters, cuda_driver_version) = pci_gpus::probe();
    fill_names_from_lspci(&mut adapters);
    SystemFacts {
        os: OsFacts {
            name: os_name(),
            version: None,
            build: None,
            sdk_level: None,
            kernel: kernel_release(),
            arch: std::env::consts::ARCH,
        },
        device: None,
        cpu: CpuFacts {
            // `Hardware` names the SoC on ARM boards whose kernel prints no `model name`.
            name: cpuinfo.as_ref().and_then(|info| info.model_name.clone().or_else(|| info.hardware.clone())),
            physical_cores: cpuinfo.as_ref().and_then(|info| info.physical_cores),
            logical_cores,
        },
        memory,
        gpus: GpuFacts::new(GpuEnumeration::Supported, adapters, cuda_driver_version),
    }
}

/// `PRETTY_NAME` of os-release (either location), else `"Linux"`.
fn os_name() -> String {
    let parsed = read_text(OS_RELEASE).or_else(|| read_text(OS_RELEASE_FALLBACK)).and_then(|text| parse_os_release(&text));
    parsed.unwrap_or_else(|| "Linux".to_string())
}

/// Names still-unnamed adapters from one `lspci -mm -nn` run, matched by PCI id. Skipped when
/// every adapter already has a name; a missing `lspci` leaves the names `None`.
fn fill_names_from_lspci(adapters: &mut [GpuAdapterFacts]) {
    if adapters.iter().all(|adapter| adapter.name.is_some() || adapter.pci_id.is_none()) {
        return;
    }
    let text = match run_command_bounded("lspci", &["-mm", "-nn"], LSPCI_TIMEOUT) {
        Ok(output) if output.status.success() => output.stdout,
        Ok(output) => {
            runtime_log::log_info(format!("[system-facts] `lspci -mm -nn` exited with {}", output.status));
            return;
        }
        Err(CommandRunError::Spawn(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            runtime_log::log_info("[system-facts] lspci is not installed; unnamed GPUs stay unnamed");
            return;
        }
        Err(err) => {
            runtime_log::log_warn(format!("[system-facts] `lspci -mm -nn` failed: {err}"));
            return;
        }
    };
    let names = parse_lspci_display_names(&text);
    for adapter in adapters.iter_mut().filter(|adapter| adapter.name.is_none()) {
        adapter.name = adapter.pci_id.and_then(|id| names.iter().find(|(key, _)| *key == id).map(|(_, name)| name.clone()));
    }
}
