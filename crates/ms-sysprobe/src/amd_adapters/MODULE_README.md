# Module: crates/ms-sysprobe/src/amd_adapters

## Purpose
The one owner of "which AMD GPUs does this machine have, and which `gfxNNNN` processor is
each" — the fact the installer needs to pick a ROCm torch device extra
(`torch[device-gfxNNNN]`) and that `gpu_utils` derives AMD presence from. GUI-free, blocking,
no config access. Underneath it owns the crate's one PCI display-device walk, which is
vendor-neutral: `enumerate_display_devices` (crate-private) returns every display-class PCI
function, `system_facts` builds its all-vendor adapter list from it, and the AMD view is a
filter over the same walk.

## Architecture
```text
enumerate_display_devices() -> Vec<PciDisplayDevice>   (every vendor; crate-private)
  ├─ linux.rs   /sys/class/drm/card<N>/device/{vendor,class,device,revision,driver,product_name,
  │             mem_info_vram_total} + KFD topology nodes (gfx_target_version per AMD device id)
  ├─ windows.rs SetupAPI GUID_DEVCLASS_DISPLAY (DIGCF_PRESENT): SPDRP_HARDWAREID, SPDRP_DEVICEDESC
  │             + driver key (DIREG_DRV): DriverVersion, HardwareInformation.qwMemorySize / MemorySize
  └─ (other targets: empty)
detect_amd_adapters() = amd_adapters_from(walk): vendor 0x1002 (Linux: amdgpu-driven only)
  -> amd_adapter_facts(): gfx (KFD, else table), integrated, name (OS, else table)
  -> order_adapters(): discrete, then integrated, then unknown (stable)
device_ids.rs  the pinned PCI device id -> (gfx, integrated, product) table
```
gfx precedence: on Linux KFD topology wins (it is the kernel driver's own answer, the same
source AMD's `rocm_bootstrap` uses), the table is the fallback and a disagreement is logged; on
Windows the table is the only source. The processor type is `ms_config::torch_choice::GfxArch`,
so every crate shares one validated spelling.

## Files and submodules
- `mod.rs`: public types (`AmdAdapter`, `GfxSource`, `DeviceIdInfo`), `detect_amd_adapters`,
  `linux_kfd_available`, `device_id_info`; crate-private `PciDisplayDevice`,
  `enumerate_display_devices`, `amd_adapters_from`, `amd_adapter_facts`; and the pure cfg-free
  parsers (`parse_pci_hardware_id`, `parse_sysfs_hex_u16`, `parse_kfd_node_properties`,
  `parse_windows_driver_version`, `parse_registry_memory_size`, `choose_registry_vram`) plus
  `order_adapters`. A parser used by only one platform
  carries a `cfg_attr(..., expect(dead_code))` so its tests still run on every host.
- `device_ids.rs`: the table and its source pins (header). Edit only by re-reading the pinned
  sources at newer commits; record the new commits in the header.
- `linux.rs`: sysfs walk of every display-class PCI function (deduplicated by the canonical
  `device` path; non-PCI DRM devices skipped) + KFD (read only when an AMD function exists; CPU
  nodes skipped by `simd_count 0`).
- `windows.rs`: SetupAPI + driver-key enumeration of every PCI display device; RAII owners
  free the device-info set and the registry key (`crate::win_registry::RegKey`) on every
  path; every `unsafe` has a `// SAFETY:` comment.
- `tests.rs`: parser tables, table sanity (unique ids, every gfx canonical, coverage of the
  AMD wheel extras), ordering, a captured KFD fixture (gfx1201 + gfx1036) and a live,
  non-panicking probe (`cargo test -p ms-sysprobe amd_ -- --nocapture` prints this machine's
  adapters).

## Contracts and invariants
- No rocminfo, lspci, WMI, COM or OpenCL. A system ROCm install is never required. (OpenCL
  `CL_DEVICE_NAME` confirmation on Windows is deliberately deferred: loading the vendor ICD into
  a GUI process risks a crash.)
- Unknown is not absent: an AMD display adapter with an id the table lacks is returned with
  `gfx = None`, `gfx_source = None`, `integrated = None`; nothing is guessed from a name.
- `gfx.is_some() == gfx_source.is_some()`; `vendor_id` is always 0x1002; `driver_version` is
  Windows-only.
- On Linux only `amdgpu`-driven cards are reported (a `radeon`-driven card cannot run ROCm);
  the all-vendor walk itself keeps every display function, with its kernel driver named.
- The walk is the crate's only PCI display enumeration: a consumer that needs another vendor's
  adapters reads `enumerate_display_devices`, never a second sysfs / SetupAPI loop.
- Every function blocks (filesystem / Win32): workers only, never the GUI thread. Failures are
  logged through `ms_log::runtime_log` and never panic.
- One table: no other crate maps PCI ids or product names to gfx.

## Editing map
- A new card / APU id: add a row in `device_ids.rs` from a pinned source and cite it.
- A new probe field of the walk (PCI location, ...): `PciDisplayDevice` in `mod.rs`, then both
  platform files; expose it on `AmdAdapter` and / or `system_facts::GpuAdapterFacts` as needed.
- A gfx spelling rule: `ms_config::torch_choice::GfxArch`, not here.
- Which gfx gets a ROCm option, and driver minimums: `ms_installer::torch` (catalog), not here.
