/*
File: crates/ms-sysprobe/src/amd_adapters/mod.rs

Purpose:
AMD GPU adapter enumeration and PCI device id -> gfx architecture detection: the one owner of
"which AMD GPUs does this machine have, and which `gfxNNNN` is each". The installer picks the
ROCm torch device extra from this answer; `gpu_utils` derives AMD presence from it. The
platform walk underneath is vendor-neutral: it returns every display-class PCI function
(`PciDisplayDevice`), which `system_facts` also consumes for its all-vendor adapter list, and
the AMD view is derived from that one walk.

Key structures:
- PciDisplayDevice (crate-private): one display-class PCI function of any vendor as the
  platform walk sees it (ids, OS name, Linux kernel driver, Windows driver version, driver-
  reported dedicated memory, Linux KFD gfx).
- AmdAdapter: one display-class AMD adapter (ids, name, gfx + where it came from, integrated
  flag, Windows driver version).
- GfxSource: KFD topology (the kernel's own answer) or the pinned device-id table.
- DeviceIdInfo: what the pinned table says about one device id.
- KfdNode (crate-private): the fields of a KFD topology node this module reads.

Key functions:
- detect_amd_adapters(): enumerate (Linux sysfs + KFD; Windows SetupAPI + driver registry key).
- enumerate_display_devices() / amd_adapters_from() / amd_adapter_facts() (crate-private): the
  all-vendor walk and the AMD view over it.
- parse_registry_memory_size / choose_registry_vram (crate-private, pure): the Windows driver
  key's dedicated-memory values.
- linux_kfd_available(): `/dev/kfd` exists (Linux only).
- device_id_info(): table lookup.
- parse_pci_hardware_id / parse_sysfs_hex_u16 / parse_kfd_node_properties /
  parse_windows_driver_version / order_adapters: pure, cfg-free, unit-tested.

Notes:
- No rocminfo, no lspci, no WMI, no OpenCL: Linux reads sysfs only, Windows calls SetupAPI and
  the registry through `windows-sys` (no COM). Other targets (macOS, Android, wasm) report no
  adapters and no display devices.
- Blocking (filesystem / Win32 calls): call from a worker thread, never the GUI thread.
- Unknown is not absent: an adapter whose id the table does not know is still returned, with
  `gfx = None`; nothing is guessed from a name.
*/

use ms_config::torch_choice::GfxArch;

mod device_ids;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

/// PCI vendor id of AMD / ATI.
pub const AMD_PCI_VENDOR_ID: u16 = 0x1002;

/// Where an adapter's [`AmdAdapter::gfx`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfxSource {
    /// KFD topology `gfx_target_version` (Linux): the kernel driver's own answer.
    Kfd,
    /// The pinned PCI device-id table (`device_ids.rs`).
    DeviceIdTable,
}

/// One AMD display adapter present on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmdAdapter {
    /// PCI vendor id; always [`AMD_PCI_VENDOR_ID`] in [`detect_amd_adapters`]'s output.
    pub vendor_id: u16,
    /// PCI device id (`DEV_xxxx`).
    pub device_id: u16,
    /// PCI revision id, when the OS reports it.
    pub revision: Option<u8>,
    /// Display name: the OS device description on Windows; on Linux the board's
    /// `product_name`, else the table's product label, else `AMD [1002:xxxx]`.
    pub name: String,
    /// Processor name, `None` when neither KFD nor the table knows the device.
    pub gfx: Option<GfxArch>,
    /// Where [`gfx`](Self::gfx) came from; `None` exactly when `gfx` is `None`.
    pub gfx_source: Option<GfxSource>,
    /// Integrated (APU) vs discrete, from the table; `None` for an unknown id.
    pub integrated: Option<bool>,
    /// Windows driver version string from the driver registry key (`"32.0.31041.5005"`);
    /// always `None` on Linux.
    pub driver_version: Option<String>,
}

/// What the pinned table says about one PCI device id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdInfo {
    /// The processor of this id.
    pub gfx: GfxArch,
    /// Whether the id is an APU / integrated GPU.
    pub integrated: bool,
    /// Short product label (hardware name, never translated).
    pub product: &'static str,
}

/// The fields of one KFD topology node (`/sys/class/kfd/kfd/topology/nodes/<n>/properties`)
/// this module reads. A missing or unparseable line leaves its field at `0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KfdNode {
    /// Encoded processor (`major*10000 + minor*100 + stepping`); `0` on CPU nodes.
    pub(crate) gfx_target_version: u32,
    /// PCI vendor id in decimal in the file (`4098` = 0x1002).
    pub(crate) vendor_id: u32,
    /// PCI device id in decimal in the file.
    pub(crate) device_id: u32,
    /// SIMD count; `0` marks a CPU-only node.
    pub(crate) simd_count: u32,
}

/// One display-class PCI function of ANY vendor, as the platform walk reports it. Raw facts
/// only; vendor-specific meaning (gfx, integrated, names) is derived by the consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PciDisplayDevice {
    /// PCI vendor id (`0x1002` AMD, `0x10de` NVIDIA, `0x8086` Intel, ...).
    pub(crate) vendor_id: u16,
    /// PCI device id.
    pub(crate) device_id: u16,
    /// PCI revision id, when the OS reports it.
    pub(crate) revision: Option<u8>,
    /// The OS's own non-empty name for the device: Windows `SPDRP_DEVICEDESC`; Linux sysfs
    /// `product_name` (amdgpu only, frequently empty). `None` when the OS names nothing.
    pub(crate) os_name: Option<String>,
    /// Linux kernel driver bound to the function (`amdgpu`, `nvidia`, `i915`, ...); `None`
    /// when unbound and always `None` on Windows.
    pub(crate) kernel_driver: Option<String>,
    /// Windows driver version from the driver registry key; always `None` on Linux.
    pub(crate) driver_version: Option<String>,
    /// Dedicated memory the driver reports, in bytes: Linux amdgpu `mem_info_vram_total`;
    /// Windows driver key `HardwareInformation.qwMemorySize`, else `.MemorySize`. On an
    /// integrated GPU this is a firmware carve-out, not VRAM; consumers decide whether to show it.
    pub(crate) reported_memory_bytes: Option<u64>,
    /// Linux KFD topology processor for this device id (AMD only); `None` elsewhere.
    pub(crate) kfd_gfx: Option<GfxArch>,
}

/// Enumerates every present display-class PCI function, in the platform's enumeration order
/// (Linux: `cardN` order, deduplicated; Windows: SetupAPI order). Empty on other targets.
/// Never panics; failures are logged and yield fewer (or no) devices. Blocking.
pub(crate) fn enumerate_display_devices() -> Vec<PciDisplayDevice> {
    #[cfg(target_os = "linux")]
    let devices = linux::enumerate();
    #[cfg(windows)]
    let devices = windows::enumerate();
    #[cfg(not(any(target_os = "linux", windows)))]
    let devices = Vec::new();
    devices
}

/// Enumerates the AMD display adapters present on this machine, discrete first, then
/// integrated, then unknown, each group in enumeration order.
///
/// Linux: `/sys/class/drm/card<N>` with vendor 0x1002, display class `0x03xxxx` and the
/// `amdgpu` driver; gfx from KFD topology, else the table. Windows: SetupAPI display class
/// with `DIGCF_PRESENT`, gfx from the table, driver version from the driver key. Other targets:
/// empty. Never panics; probe failures are logged and yield fewer (or no) adapters. Blocking.
#[must_use]
pub fn detect_amd_adapters() -> Vec<AmdAdapter> {
    amd_adapters_from(&enumerate_display_devices())
}

/// The AMD view of one display-device walk: [`detect_amd_adapters`]'s answer for `devices`
/// (on Linux only `amdgpu`-driven functions count), ordered by [`order_adapters`].
pub(crate) fn amd_adapters_from(devices: &[PciDisplayDevice]) -> Vec<AmdAdapter> {
    let require_amdgpu = cfg!(target_os = "linux");
    order_adapters(devices.iter().filter_map(|device| amd_adapter_from_device(device, require_amdgpu)).collect())
}

/// Builds the [`AmdAdapter`] of one AMD display device; `None` for another vendor and, when
/// `require_amdgpu` holds (Linux), for a function not driven by `amdgpu` (logged: a `radeon`
/// or driverless card cannot run ROCm).
fn amd_adapter_from_device(device: &PciDisplayDevice, require_amdgpu: bool) -> Option<AmdAdapter> {
    if device.vendor_id != AMD_PCI_VENDOR_ID {
        return None;
    }
    if require_amdgpu && device.kernel_driver.as_deref() != Some("amdgpu") {
        ms_log::runtime_log::log_info(format!(
            "[amd-adapters] skipping AMD display device {:#06x}: driver {} is not amdgpu",
            device.device_id,
            device.kernel_driver.as_deref().unwrap_or("<none>")
        ));
        return None;
    }
    let facts = amd_adapter_facts(device);
    let name = facts.name.unwrap_or_else(|| format!("AMD [{AMD_PCI_VENDOR_ID:04x}:{:04x}]", device.device_id));
    Some(AmdAdapter {
        vendor_id: device.vendor_id,
        device_id: device.device_id,
        revision: device.revision,
        name,
        gfx: facts.gfx,
        gfx_source: facts.gfx_source,
        integrated: facts.integrated,
        driver_version: device.driver_version.clone(),
    })
}

/// What this module knows about one AMD device, without the driver filter or a made-up name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AmdDeviceFacts {
    /// The OS name, else the pinned table's product label; `None` when neither knows it.
    pub(crate) name: Option<String>,
    /// KFD processor (Linux), else the table's; `None` for an unknown id.
    pub(crate) gfx: Option<GfxArch>,
    /// Where `gfx` came from; `None` exactly when `gfx` is `None`.
    pub(crate) gfx_source: Option<GfxSource>,
    /// Integrated flag from the table; `None` for an unknown id.
    pub(crate) integrated: Option<bool>,
}

/// Resolves gfx (KFD wins over the table; a disagreement is logged), integrated flag and name
/// of one AMD device. The one place both the AMD adapter list and `system_facts` take these
/// facts from. An id the table lacks is logged.
pub(crate) fn amd_adapter_facts(device: &PciDisplayDevice) -> AmdDeviceFacts {
    let device_id = device.device_id;
    let table = device_id_info(device_id);
    if let (Some(kfd_gfx), Some(table)) = (&device.kfd_gfx, &table)
        && *kfd_gfx != table.gfx
    {
        ms_log::runtime_log::log_warn(format!(
            "[amd-adapters] device {device_id:#06x}: KFD reports {kfd_gfx}, the device-id table says {}; using KFD",
            table.gfx
        ));
    }
    if table.is_none() {
        ms_log::runtime_log::log_info(format!("[amd-adapters] AMD device {device_id:#06x} is not in the device-id table; its gfx is unknown unless KFD names it"));
    }
    let (gfx, gfx_source) = match (device.kfd_gfx.clone(), &table) {
        (Some(gfx), _) => (Some(gfx), Some(GfxSource::Kfd)),
        (None, Some(info)) => (Some(info.gfx.clone()), Some(GfxSource::DeviceIdTable)),
        (None, None) => (None, None),
    };
    AmdDeviceFacts {
        name: device.os_name.clone().or_else(|| table.as_ref().map(|info| info.product.to_string())),
        gfx,
        gfx_source,
        integrated: table.as_ref().map(|info| info.integrated),
    }
}

/// Whether the ROCm compute device node `/dev/kfd` exists (Linux). Always `false` elsewhere.
#[must_use]
pub fn linux_kfd_available() -> bool {
    cfg!(target_os = "linux") && std::path::Path::new("/dev/kfd").exists()
}

/// Looks `device_id` (vendor 0x1002) up in the pinned table. Pure.
#[must_use]
pub fn device_id_info(device_id: u16) -> Option<DeviceIdInfo> {
    let row = device_ids::lookup_row(device_id)?;
    // Every row's gfx parses (unit-tested), so `None` here is unreachable in practice; it
    // degrades to "unknown id" rather than panicking.
    let gfx = GfxArch::parse(row.gfx)?;
    Some(DeviceIdInfo { gfx, integrated: row.integrated, product: row.product })
}

/// Parses a Windows PCI hardware id such as
/// `PCI\VEN_1002&DEV_7550&SUBSYS_04161002&REV_C0` into `(vendor, device, revision)`.
///
/// Case-insensitive; the `REV_` part is optional (`None` when absent or malformed). Returns
/// `None` for a non-PCI id or a missing / malformed `VEN_` or `DEV_` field.
#[cfg_attr(all(not(windows), not(test)), expect(dead_code, reason = "called only by the Windows enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn parse_pci_hardware_id(hardware_id: &str) -> Option<(u16, u16, Option<u8>)> {
    let upper = hardware_id.trim().to_ascii_uppercase();
    let fields = upper.strip_prefix("PCI\\")?;
    let mut vendor = None;
    let mut device = None;
    let mut revision = None;
    for field in fields.split('&') {
        if let Some(hex) = field.strip_prefix("VEN_") {
            vendor = u16::from_str_radix(hex, 16).ok();
        } else if let Some(hex) = field.strip_prefix("DEV_") {
            device = u16::from_str_radix(hex, 16).ok();
        } else if let Some(hex) = field.strip_prefix("REV_") {
            revision = u8::from_str_radix(hex, 16).ok();
        }
    }
    Some((vendor?, device?, revision))
}

/// Upper bound for a Win32 string-property buffer, in UTF-16 units (128 KiB): a hardware-id
/// list or a driver version is a few hundred units, so a larger request is treated as bogus.
pub(crate) const MAX_WIN32_STRING_UNITS: usize = 64 * 1024;

/// Buffer size (UTF-16 units) for the next attempt of a Win32 "buffer too small" retry.
///
/// `required_bytes` is the byte count the API reported. Returns `None` when retrying cannot
/// help: the request does not grow the buffer (no progress) or exceeds
/// [`MAX_WIN32_STRING_UNITS`].
#[cfg_attr(all(not(windows), not(test)), expect(dead_code, reason = "called only by the Windows enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn next_win32_buffer_units(current_units: usize, required_bytes: u32) -> Option<usize> {
    let required_units = usize::try_from(required_bytes).ok()?.div_ceil(2);
    (required_units > current_units && required_units <= MAX_WIN32_STRING_UNITS).then_some(required_units)
}

/// Decodes the first `byte_len` bytes of a UTF-16 buffer filled by a Win32 string API
/// (`REG_SZ` or `REG_MULTI_SZ`) into its non-empty strings, splitting at NULs.
///
/// An odd `byte_len` rounds up; a `byte_len` past the buffer is clamped to it. A value with
/// or without its terminating NUL decodes the same.
#[cfg_attr(all(not(windows), not(test)), expect(dead_code, reason = "called only by the Windows enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn decode_win32_utf16_strings(buffer: &[u16], byte_len: u32) -> Vec<String> {
    let units = usize::try_from(byte_len).map_or(buffer.len(), |bytes| bytes.div_ceil(2)).min(buffer.len());
    buffer[..units].split(|unit| *unit == 0).filter(|part| !part.is_empty()).map(String::from_utf16_lossy).collect()
}

/// Parses a sysfs hex attribute such as `"0x1002\n"` (the `0x` prefix is optional).
#[cfg_attr(all(not(target_os = "linux"), not(test)), expect(dead_code, reason = "called only by the Linux enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn parse_sysfs_hex_u16(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    let hex = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")).unwrap_or(trimmed);
    u16::from_str_radix(hex, 16).ok()
}

/// Parses a KFD topology node `properties` file (`"<key> <decimal>"` per line).
///
/// Only the fields of [`KfdNode`] are kept; unknown keys, non-numeric values and malformed
/// lines are skipped, so a field that is missing stays `0`.
#[cfg_attr(all(not(target_os = "linux"), not(test)), expect(dead_code, reason = "called only by the Linux enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn parse_kfd_node_properties(text: &str) -> KfdNode {
    let mut node = KfdNode::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u32>() else {
            continue;
        };
        match key {
            "gfx_target_version" => node.gfx_target_version = value,
            "vendor_id" => node.vendor_id = value,
            "device_id" => node.device_id = value,
            "simd_count" => node.simd_count = value,
            _ => {}
        }
    }
    node
}

/// Orders adapters discrete first, then integrated, then unknown (`integrated == None`),
/// keeping the enumeration order inside each group (stable sort).
pub(crate) fn order_adapters(mut adapters: Vec<AmdAdapter>) -> Vec<AmdAdapter> {
    adapters.sort_by_key(|adapter| match adapter.integrated {
        Some(false) => 0_u8,
        Some(true) => 1,
        None => 2,
    });
    adapters
}

/// Parses a Windows driver version string (`"32.0.31041.5005"`) into its four numeric
/// parts, comparable as an array. Exactly four dot-separated decimal `u32` parts are
/// required (surrounding whitespace ignored); anything else is `None`.
#[must_use]
pub fn parse_windows_driver_version(text: &str) -> Option<[u32; 4]> {
    let mut parts = text.trim().split('.');
    let mut out = [0_u32; 4];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Windows registry value type `REG_BINARY` (winnt.h), spelled here so the pure decoder below
/// builds and is tested on every host.
const REG_BINARY_TYPE: u32 = 3;
/// Windows registry value type `REG_DWORD` (little-endian 32-bit).
const REG_DWORD_TYPE: u32 = 4;
/// Windows registry value type `REG_QWORD` (little-endian 64-bit).
const REG_QWORD_TYPE: u32 = 11;

/// Decodes a display driver key's `HardwareInformation.qwMemorySize` / `.MemorySize` value
/// into bytes. Accepts `REG_QWORD` (8 bytes), `REG_DWORD` (4 bytes) and `REG_BINARY` of 4 or 8
/// bytes (older drivers write the 32-bit size as binary), all little-endian. Any other type or
/// length, and `0`, is `None`. Pure.
#[cfg_attr(all(not(windows), not(test)), expect(dead_code, reason = "called only by the Windows enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn parse_registry_memory_size(value_type: u32, data: &[u8]) -> Option<u64> {
    let value = match (value_type, data.len()) {
        (REG_QWORD_TYPE | REG_BINARY_TYPE, 8) => u64::from_le_bytes(data.try_into().ok()?),
        (REG_DWORD_TYPE | REG_BINARY_TYPE, 4) => u64::from(u32::from_le_bytes(data.try_into().ok()?)),
        _ => return None,
    };
    (value > 0).then_some(value)
}

/// The dedicated memory of a Windows display device: the 64-bit `qwMemorySize` when present
/// (the 32-bit `MemorySize` saturates at 4 GiB), else `MemorySize`. Pure.
#[cfg_attr(all(not(windows), not(test)), expect(dead_code, reason = "called only by the Windows enumerator; kept cfg-free so its tests run on every host"))]
pub(crate) fn choose_registry_vram(qw_memory_size: Option<u64>, memory_size: Option<u64>) -> Option<u64> {
    qw_memory_size.or(memory_size)
}
