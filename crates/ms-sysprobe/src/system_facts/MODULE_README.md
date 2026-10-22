# Module: crates/ms-sysprobe/src/system_facts

## Purpose
The machine's hardware / OS fact sheet: OS identity, device identity (Android, macOS), CPU,
physical memory and every graphics adapter, as RAW data (no localized text, no formatting).
It is the one worker-side report the launcher's system-information view renders; the view
formats and localizes, this module only answers. GUI-free, blocking.

## Architecture
```text
probe_system_facts()                                   (mod.rs; worker thread only)
  ├─ memory: ms_memory::current_memory_availability()  (the workspace's RAM owner)
  ├─ logical cores: std::thread::available_parallelism
  └─ per platform:
     ├─ linux.rs    /etc/os-release, /proc/sys/kernel/osrelease, /proc/cpuinfo
     │              + pci_gpus::probe() + at most one `lspci -mm -nn` for unnamed devices
     ├─ windows.rs  registry CurrentVersion / CentralProcessor\0 (win_registry),
     │              GetLogicalProcessorInformationEx + pci_gpus::probe()
     ├─ macos.rs    sysctlbyname (OS, model, CPU, cores) + one `system_profiler SPDisplaysDataType -json`
     └─ android.rs  __system_property_get (device, SoC, OS), procfs; GPUs: Unsupported
pci_gpus.rs (Linux, Windows)
  amd_adapters::enumerate_display_devices()  (the one all-vendor PCI display walk)
  -> adapter_from_pci (AMD facts from amd_adapters::amd_adapter_facts: table + KFD)
  -> merge_nvidia_smi(one `nvidia-smi --query-gpu`, always asked — an NVIDIA GPU can have no
     DRM card —, CSV-decoded, retried once without compute_cap)
  -> plain `nvidia-smi` for the driver's CUDA version (only when NVIDIA rows exist)
GpuFacts::new -> order_gpu_adapters: discrete, integrated, unknown (stable)
```

## Files and submodules
- `mod.rs`: public types (`SystemFacts`, `OsFacts`, `DeviceFacts`, `CpuFacts`, `GpuFacts`,
  `GpuAdapterFacts`, `GpuVendor`, `GpuKind`, `GpuEnumeration`), `probe_system_facts`, the
  shared pure assembly (`adapter_from_pci`, `merge_nvidia_smi`, `order_gpu_adapters`,
  `gpu_vendor_from_pci`) and the procfs helpers shared by Linux and Android.
- `parse.rs`: every pure parser (os-release, cpuinfo, `nvidia-smi` CSV, `lspci -mm -nn`,
  `system_profiler` JSON, Windows product name / build, processor-record count). Each is
  compiled for the platforms that call it plus `test`.
- `pci_gpus.rs`: the Linux/Windows GPU half (walk + NVIDIA enrichment).
- `linux.rs`, `windows.rs`, `macos.rs`, `android.rs`: one per platform; `probe()` only.
- `tests.rs`: parser tables, mapping / merge / order rules and a non-panicking live probe
  (`cargo test -p ms-sysprobe system_facts -- --nocapture` prints this machine's report).

## Contracts and invariants
- Raw facts only: no `t!`, no formatted sizes, no joined labels. Every field documents its
  source; `None` means "no source answered", never "absent" — nothing is guessed from a name
  (an Intel or unknown-id AMD adapter has `GpuKind::Unknown`).
- `GpuEnumeration::Unsupported` (Android, other targets, macOS when `system_profiler` fails)
  marks an empty adapter list as unknown, not as "no GPU".
- VRAM is reported for `GpuKind::Discrete` adapters only (an integrated GPU's driver figure is a
  firmware carve-out).
- One owner each, never re-derived here: RAM (`ms-memory`), the PCI display walk, AMD gfx /
  integrated flag / product names (`amd_adapters` with its pinned table and KFD).
- Every subprocess runs through `gpu_utils::run_command_bounded` with a timeout (5 s for
  `nvidia-smi` / `lspci`, 10 s for `system_profiler`) and is killed past it. No PowerShell,
  WMI or COM. At most three processes per probe.
- Blocking and worker-only; never panics; every failed source is logged through
  `ms_log::runtime_log` with the path, command or Win32 error.
- `nvidia-smi` rows enrich the NVIDIA PCI rows in order when the counts match, and replace
  them otherwise (WSL has no DRM walk; a headless NVIDIA card has no DRM card), so a row with
  no PCI counterpart is appended next to the other vendors' adapters. Mixed multi-NVIDIA systems whose walk order differs
  from `nvidia-smi`'s PCI-bus order can pair names with the wrong PCI id (known limit).

## Editing map
- A new fact: the type in `mod.rs` (documented with its source per platform), then each
  platform file; a new text format gets a parser in `parse.rs` and a test.
- A new platform: a `<os>.rs` with `probe()` and a cfg arm in `probe_system_facts`.
- AMD ids / gfx: `../amd_adapters/` (never here). RAM: `ms-memory`.
- How the facts are shown: the launcher's system-information page, never here.
