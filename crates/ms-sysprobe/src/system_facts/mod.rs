/*
File: crates/ms-sysprobe/src/system_facts/mod.rs

Purpose:
The hardware / OS fact sheet of the machine (OS, device, CPU, RAM, every GPU adapter) as raw,
unformatted, unlocalized data — the one worker-side report the launcher's system-information
view renders. GUI-free and blocking.

Key structures:
- SystemFacts: the whole report.
- OsFacts / DeviceFacts / CpuFacts: OS identity, device identity (Android, macOS), processor.
- GpuFacts / GpuAdapterFacts / GpuVendor / GpuKind / GpuEnumeration: the adapter list.

Key functions:
- probe_system_facts(): blocking probe (worker only); dispatches to the per-platform file.
- order_gpu_adapters(): discrete first, then integrated, then unknown (stable).
- adapter_from_pci() / merge_nvidia_smi() (Linux, Windows): PCI walk rows -> adapters, enriched
  by the one `nvidia-smi` query.

Notes:
- RAM is `ms_memory::current_memory_availability` (the workspace's RAM owner); AMD gfx /
  integrated flag / names come from `amd_adapters` (the one PCI id table + KFD); the PCI walk
  itself is `amd_adapters::enumerate_display_devices` — this module re-derives none of them.
- Every subprocess goes through `gpu_utils::run_command_bounded` with a timeout; no PowerShell,
  no WMI. Unknown is never absent: a fact no source answers is `None`, never a guess.
*/

use ms_config::torch_choice::GfxArch;
use ms_memory::MemoryAvailability;

use crate::gpu_utils::RuntimeVersion;

#[cfg(any(target_os = "linux", windows, test))]
use crate::amd_adapters::{AMD_PCI_VENDOR_ID, PciDisplayDevice, amd_adapter_facts};

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod parse;
#[cfg(any(target_os = "linux", windows))]
mod pci_gpus;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

/// PCI vendor id of NVIDIA.
#[cfg(any(target_os = "linux", windows, test))]
const NVIDIA_PCI_VENDOR_ID: u16 = 0x10de;
/// PCI vendor id of Intel.
#[cfg(any(target_os = "linux", windows, test))]
const INTEL_PCI_VENDOR_ID: u16 = 0x8086;
/// PCI vendor id of Apple.
#[cfg(any(target_os = "linux", windows, test))]
const APPLE_PCI_VENDOR_ID: u16 = 0x106b;

/// Everything [`probe_system_facts`] learned about the machine. Raw facts only: no localized
/// text, no formatting; every `None` means "no source answered", never "absent".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFacts {
    /// Operating system identity.
    pub os: OsFacts,
    /// Device identity where the OS names one (Android, macOS); `None` elsewhere.
    pub device: Option<DeviceFacts>,
    /// Processor.
    pub cpu: CpuFacts,
    /// Physical memory total + available (`ms_memory::current_memory_availability`); `None`
    /// when the OS did not report it.
    pub memory: Option<MemoryAvailability>,
    /// Graphics adapters.
    pub gpus: GpuFacts,
}

/// Operating system identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsFacts {
    /// Product name: Linux `/etc/os-release` `PRETTY_NAME` (`"Ubuntu 26.04 LTS"`, else `NAME`
    /// and `VERSION`, else `"Linux"`); Windows registry `ProductName` corrected to Windows 11 on
    /// build 22000+ (`"Windows 11 Pro"`, else `"Windows"`); `"macOS"`; `"Android"`; on any
    /// other target `std::env::consts::OS`.
    pub name: String,
    /// Marketing version: Windows `DisplayVersion` (`"24H2"`, else `ReleaseId`); macOS
    /// `kern.osproductversion` (`"15.3"`); Android `ro.build.version.release` (`"15"`). `None`
    /// on Linux (the version is part of `PRETTY_NAME`).
    pub version: Option<String>,
    /// Build identifier: Windows `CurrentBuildNumber.UBR` (`"26100.4061"`); macOS
    /// `kern.osversion` (`"24D60"`); `None` elsewhere.
    pub build: Option<String>,
    /// Android API level (`ro.build.version.sdk`, `35`); `None` elsewhere.
    pub sdk_level: Option<u32>,
    /// Kernel release: Linux / Android `/proc/sys/kernel/osrelease`, macOS `kern.osrelease`
    /// (Darwin); `None` on Windows.
    pub kernel: Option<String>,
    /// CPU architecture the program was built for (`std::env::consts::ARCH`: `"x86_64"`,
    /// `"aarch64"`).
    pub arch: &'static str,
}

/// Device identity (Android system properties, macOS `hw.model`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFacts {
    /// Maker: Android `ro.product.manufacturer` (`"samsung"`); `"Apple"` on macOS.
    pub manufacturer: Option<String>,
    /// Model: Android `ro.product.model` (`"SM-S921B"`); macOS `hw.model` (`"Mac14,2"`).
    pub model: Option<String>,
    /// SoC maker: Android `ro.soc.manufacturer` (Android 12+, `"Qualcomm"`); `None` elsewhere.
    pub soc_manufacturer: Option<String>,
    /// SoC model: Android `ro.soc.model` (Android 12+, `"SM8650"`), else the board platform
    /// codename `ro.board.platform` (`"kona"`); `None` on macOS.
    pub soc_model: Option<String>,
}

/// Processor facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuFacts {
    /// Brand string: Linux / Android `/proc/cpuinfo` `model name`, else its `Hardware` line
    /// (Android: else the SoC model); Windows registry `ProcessorNameString`; macOS
    /// `machdep.cpu.brand_string`. `None` when no source names it.
    pub name: Option<String>,
    /// Physical cores: Linux / Android distinct (`physical id`, `core id`) pairs of
    /// `/proc/cpuinfo` (`None` where the kernel omits them, typical on ARM and in VMs); Windows
    /// `GetLogicalProcessorInformationEx(RelationProcessorCore)`; macOS `hw.physicalcpu`.
    pub physical_cores: Option<usize>,
    /// Logical processors available to this process (`std::thread::available_parallelism`;
    /// affinity-limited); `None` when the OS did not answer.
    pub logical_cores: Option<usize>,
}

/// Whether this platform can enumerate graphics adapters at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuEnumeration {
    /// The adapter list is the platform's real answer (it may still be empty).
    Supported,
    /// The platform hides the adapters from apps (Android's sandbox hides `/sys/class/drm`)
    /// or no probe exists for it: an empty list means "unknown", not "no GPU".
    Unsupported,
}

/// GPU vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    /// AMD / ATI (PCI `0x1002`).
    Amd,
    /// NVIDIA (PCI `0x10de`).
    Nvidia,
    /// Intel (PCI `0x8086`).
    Intel,
    /// Apple (Apple Silicon GPUs; PCI `0x106b`).
    Apple,
    /// Any other vendor; the PCI vendor id when a source reported one.
    Other {
        /// PCI vendor id; `None` when the source (macOS `system_profiler`) gave none.
        vendor_id: Option<u16>,
    },
}

/// Discrete vs integrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    /// A separate GPU with its own memory (every NVIDIA PCI GPU; AMD per the pinned table;
    /// macOS PCIe GPUs).
    Discrete,
    /// Shares system memory (AMD APUs per the table; Apple Silicon; macOS built-in GPUs).
    Integrated,
    /// No source says (Intel, unknown AMD ids, other vendors): never guessed from a name.
    Unknown,
}

/// One graphics adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuAdapterFacts {
    /// Vendor.
    pub vendor: GpuVendor,
    /// Product name: `nvidia-smi` (NVIDIA), the OS device description (Windows), amdgpu
    /// `product_name` or the pinned AMD table (AMD), `lspci` (other Linux devices),
    /// `system_profiler` (macOS). `None` when no source names it; the view then shows vendor +
    /// PCI id.
    pub name: Option<String>,
    /// PCI `(vendor id, device id)`; `None` when the source has no PCI identity (Apple
    /// Silicon, an `nvidia-smi` row without `pci.device_id`).
    pub pci_id: Option<(u16, u16)>,
    /// Discrete / integrated / unknown.
    pub kind: GpuKind,
    /// Dedicated video memory in bytes, reported ONLY for [`GpuKind::Discrete`] adapters (an
    /// integrated GPU's figure is a firmware carve-out, not VRAM): `nvidia-smi`
    /// `memory.total`, Linux amdgpu `mem_info_vram_total`, Windows driver key
    /// `HardwareInformation.qwMemorySize` / `MemorySize`, macOS `spdisplays_vram`.
    pub vram_bytes: Option<u64>,
    /// AMD processor (`gfx1201`) from KFD topology or the pinned table; `None` for other
    /// vendors and unknown AMD ids.
    pub amd_gfx: Option<GfxArch>,
    /// NVIDIA compute capability (`8.9`) from `nvidia-smi`; `None` otherwise (also on drivers
    /// too old to report it).
    pub nvidia_compute_capability: Option<RuntimeVersion>,
    /// Driver version: `nvidia-smi` `driver_version` (`"560.94"`) for NVIDIA, else the Windows
    /// driver key `DriverVersion` (`"32.0.31041.5005"`); `None` on Linux for other vendors.
    pub driver_version: Option<String>,
    /// Linux kernel driver bound to the PCI function (`"amdgpu"`, `"nvidia"`, `"i915"`,
    /// `"xe"`, `"nouveau"`); `None` elsewhere or when unbound.
    pub kernel_driver: Option<String>,
    /// GPU core count (Apple Silicon, `system_profiler` `sppci_cores`); `None` elsewhere.
    pub gpu_cores: Option<u32>,
}

/// Graphics adapters of the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuFacts {
    /// Whether [`adapters`](Self::adapters) is a real answer.
    pub enumeration: GpuEnumeration,
    /// Adapters, discrete first, then integrated, then unknown, each group in enumeration
    /// order.
    pub adapters: Vec<GpuAdapterFacts>,
    /// Highest CUDA version the installed NVIDIA driver supports (`CUDA Version:` of plain
    /// `nvidia-smi`); `None` without an NVIDIA driver.
    pub cuda_driver_version: Option<RuntimeVersion>,
}

impl GpuFacts {
    /// Builds the adapter list in the documented order ([`order_gpu_adapters`]).
    pub(crate) fn new(enumeration: GpuEnumeration, adapters: Vec<GpuAdapterFacts>, cuda_driver_version: Option<RuntimeVersion>) -> Self {
        Self { enumeration, adapters: order_gpu_adapters(adapters), cuda_driver_version }
    }
}

/// Probes the OS, device, CPU, memory and GPU facts of this machine.
///
/// BLOCKING (file reads, Win32 / sysctl calls, and at most three short subprocesses —
/// `nvidia-smi` (twice when it reports GPUs; one failed spawn without an NVIDIA driver) and
/// `lspci` once on Linux, or one
/// `system_profiler` on macOS — each killed after a few seconds): call it on a worker thread,
/// never the GUI thread. Never panics; every failed source is logged with context and leaves
/// its field `None`.
#[must_use]
pub fn probe_system_facts() -> SystemFacts {
    ms_log::runtime_log::log_info("[system-facts] probe started");
    let memory = ms_memory::current_memory_availability();
    let logical_cores = logical_cores();
    #[cfg(target_os = "linux")]
    let facts = linux::probe(memory, logical_cores);
    #[cfg(windows)]
    let facts = windows::probe(memory, logical_cores);
    #[cfg(target_os = "macos")]
    let facts = macos::probe(memory, logical_cores);
    #[cfg(target_os = "android")]
    let facts = android::probe(memory, logical_cores);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos", target_os = "android")))]
    let facts = SystemFacts {
        os: OsFacts { name: std::env::consts::OS.to_string(), version: None, build: None, sdk_level: None, kernel: None, arch: std::env::consts::ARCH },
        device: None,
        cpu: CpuFacts { name: None, physical_cores: None, logical_cores },
        memory,
        gpus: GpuFacts::new(GpuEnumeration::Unsupported, Vec::new(), None),
    };
    ms_log::runtime_log::log_info(format!(
        "[system-facts] probe finished: os {:?} {:?}, cpu {:?}, {} GPU adapter(s) ({:?})",
        facts.os.name,
        facts.os.version,
        facts.cpu.name,
        facts.gpus.adapters.len(),
        facts.gpus.enumeration
    ));
    facts
}

/// The kernel's CPU report (Linux, Android).
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROC_CPUINFO: &str = "/proc/cpuinfo";

/// The running kernel's release string (Linux, Android).
#[cfg(any(target_os = "linux", target_os = "android"))]
const KERNEL_OSRELEASE: &str = "/proc/sys/kernel/osrelease";

/// The kernel release (`/proc/sys/kernel/osrelease`, trimmed); `None` (logged) when unreadable.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn kernel_release() -> Option<String> {
    read_text(KERNEL_OSRELEASE).map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// Reads a small text file; `None` (logged) when it cannot be read.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_text(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) => {
            ms_log::runtime_log::log_info(format!("[system-facts] cannot read {path}: {err}"));
            None
        }
    }
}

/// Logical processors available to this process; `None` (logged) when the OS does not say.
fn logical_cores() -> Option<usize> {
    match std::thread::available_parallelism() {
        Ok(count) => Some(count.get()),
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("[system-facts] available_parallelism failed: {err}"));
            None
        }
    }
}

/// Orders adapters discrete first, then integrated, then unknown, keeping the enumeration
/// order inside each group (stable sort). Pure.
pub(crate) fn order_gpu_adapters(mut adapters: Vec<GpuAdapterFacts>) -> Vec<GpuAdapterFacts> {
    adapters.sort_by_key(|adapter| match adapter.kind {
        GpuKind::Discrete => 0_u8,
        GpuKind::Integrated => 1,
        GpuKind::Unknown => 2,
    });
    adapters
}

/// Maps a PCI vendor id to a [`GpuVendor`]. Pure.
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) fn gpu_vendor_from_pci(vendor_id: u16) -> GpuVendor {
    match vendor_id {
        AMD_PCI_VENDOR_ID => GpuVendor::Amd,
        NVIDIA_PCI_VENDOR_ID => GpuVendor::Nvidia,
        INTEL_PCI_VENDOR_ID => GpuVendor::Intel,
        APPLE_PCI_VENDOR_ID => GpuVendor::Apple,
        other => GpuVendor::Other { vendor_id: Some(other) },
    }
}

/// Turns one PCI walk row into an adapter: vendor from the PCI id; for AMD the gfx, kind and
/// name from `amd_adapters` (table + KFD); NVIDIA is always discrete; Apple integrated;
/// everything else of unknown kind. The driver-reported memory is kept only for a discrete
/// adapter. `name` is the OS name; the caller may fill a missing one from another source.
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) fn adapter_from_pci(device: &PciDisplayDevice) -> GpuAdapterFacts {
    let vendor = gpu_vendor_from_pci(device.vendor_id);
    let (name, kind, amd_gfx) = match vendor {
        GpuVendor::Amd => {
            let facts = amd_adapter_facts(device);
            let kind = match facts.integrated {
                Some(true) => GpuKind::Integrated,
                Some(false) => GpuKind::Discrete,
                None => GpuKind::Unknown,
            };
            (facts.name, kind, facts.gfx)
        }
        GpuVendor::Nvidia => (device.os_name.clone(), GpuKind::Discrete, None),
        GpuVendor::Apple => (device.os_name.clone(), GpuKind::Integrated, None),
        GpuVendor::Intel | GpuVendor::Other { .. } => (device.os_name.clone(), GpuKind::Unknown, None),
    };
    GpuAdapterFacts {
        vendor,
        name,
        pci_id: Some((device.vendor_id, device.device_id)),
        kind,
        vram_bytes: device.reported_memory_bytes.filter(|_| kind == GpuKind::Discrete),
        amd_gfx,
        nvidia_compute_capability: None,
        driver_version: device.driver_version.clone(),
        kernel_driver: device.kernel_driver.clone(),
        gpu_cores: None,
    }
}

/// Merges the `nvidia-smi` rows into the PCI adapters. When the number of NVIDIA PCI adapters
/// equals the number of rows, each row (in order) enriches its adapter: name, VRAM, compute
/// capability and driver version from `nvidia-smi`, PCI id / kernel driver kept. Otherwise
/// (the walk missed devices — WSL without `/sys/class/drm`, a headless or modesetting-off
/// NVIDIA GPU without a DRM card — or saw extra ones) the NVIDIA PCI adapters are replaced by
/// the rows, so rows with no PCI counterpart are appended next to the other vendors' adapters.
/// No rows: unchanged. Pure; the result is unordered.
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) fn merge_nvidia_smi(adapters: Vec<GpuAdapterFacts>, rows: Vec<parse::NvidiaSmiGpu>) -> Vec<GpuAdapterFacts> {
    if rows.is_empty() {
        return adapters;
    }
    let nvidia_count = adapters.iter().filter(|adapter| adapter.vendor == GpuVendor::Nvidia).count();
    if nvidia_count == rows.len() {
        let mut rows = rows.into_iter();
        return adapters
            .into_iter()
            .map(|adapter| {
                if adapter.vendor != GpuVendor::Nvidia {
                    return adapter;
                }
                match rows.next() {
                    Some(row) => GpuAdapterFacts {
                        name: row.name.or(adapter.name),
                        vram_bytes: row.memory_bytes.or(adapter.vram_bytes),
                        nvidia_compute_capability: row.compute_capability,
                        driver_version: row.driver_version.or(adapter.driver_version),
                        ..adapter
                    },
                    None => adapter,
                }
            })
            .collect();
    }
    let mut merged: Vec<GpuAdapterFacts> = adapters.into_iter().filter(|adapter| adapter.vendor != GpuVendor::Nvidia).collect();
    merged.extend(rows.into_iter().map(|row| GpuAdapterFacts {
        vendor: GpuVendor::Nvidia,
        name: row.name,
        pci_id: row.device_id.map(|device_id| (NVIDIA_PCI_VENDOR_ID, device_id)),
        kind: GpuKind::Discrete,
        vram_bytes: row.memory_bytes,
        amd_gfx: None,
        nvidia_compute_capability: row.compute_capability,
        driver_version: row.driver_version,
        kernel_driver: None,
        gpu_cores: None,
    }));
    merged
}
