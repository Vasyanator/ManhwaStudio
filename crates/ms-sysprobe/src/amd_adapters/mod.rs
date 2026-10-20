/*
File: crates/ms-sysprobe/src/amd_adapters/mod.rs

Purpose:
AMD GPU adapter enumeration and PCI device id -> gfx architecture detection: the one owner of
"which AMD GPUs does this machine have, and which `gfxNNNN` is each". The installer picks the
ROCm torch device extra from this answer; `gpu_utils` derives AMD presence from it.

Key structures:
- AmdAdapter: one display-class AMD adapter (ids, name, gfx + where it came from, integrated
  flag, Windows driver version).
- GfxSource: KFD topology (the kernel's own answer) or the pinned device-id table.
- DeviceIdInfo: what the pinned table says about one device id.
- KfdNode (crate-private): the fields of a KFD topology node this module reads.

Key functions:
- detect_amd_adapters(): enumerate (Linux sysfs + KFD; Windows SetupAPI + driver registry key).
- linux_kfd_available(): `/dev/kfd` exists (Linux only).
- device_id_info(): table lookup.
- parse_pci_hardware_id / parse_sysfs_hex_u16 / parse_kfd_node_properties /
  parse_windows_driver_version / order_adapters: pure, cfg-free, unit-tested.

Notes:
- No rocminfo, no lspci, no WMI, no OpenCL: Linux reads sysfs only, Windows calls SetupAPI and
  the registry through `windows-sys` (no COM). Other targets (macOS, Android, wasm) report no
  adapters.
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

/// Enumerates the AMD display adapters present on this machine, discrete first, then
/// integrated, then unknown, each group in enumeration order.
///
/// Linux: `/sys/class/drm/card<N>` with vendor 0x1002, display class `0x03xxxx` and the
/// `amdgpu` driver; gfx from KFD topology, else the table. Windows: SetupAPI display class
/// with `DIGCF_PRESENT`, gfx from the table, driver version from the driver key. Other targets:
/// empty. Never panics; probe failures are logged and yield fewer (or no) adapters. Blocking.
#[must_use]
pub fn detect_amd_adapters() -> Vec<AmdAdapter> {
    #[cfg(target_os = "linux")]
    let adapters = linux::enumerate();
    #[cfg(windows)]
    let adapters = windows::enumerate();
    #[cfg(not(any(target_os = "linux", windows)))]
    let adapters = Vec::new();
    order_adapters(adapters)
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
