/*
File: crates/ms-sysprobe/src/amd_adapters/windows.rs

Purpose:
Windows half of the display-device walk: enumerates present display-class devices through
SetupAPI (any vendor), keeps the PCI ones, and reads each one's description, driver version and
driver-reported dedicated memory. No WMI, no COM, no OpenCL. The AMD view (table gfx) lives in
`mod.rs`.

Key structures:
- DevInfoSet: RAII owner of the SetupAPI device-info set (`SetupDiDestroyDeviceInfoList` on
  every path); the driver key is a `crate::win_registry::RegKey`.

Key functions:
- enumerate(): SetupAPI display class -> `PciDisplayDevice`s in enumeration order.

Notes:
Every `unsafe` block carries a `// SAFETY:` comment. Device-property reads retry with the byte
count the API reports, at most `MAX_READ_ATTEMPTS` times, only while the buffer grows and never
past `MAX_WIN32_STRING_UNITS`; decoding and NUL trimming are the cfg-free
`decode_win32_utf16_strings` in `mod.rs`. Non-PCI display devices (`ROOT\BasicDisplay`, remote
display adapters) have no PCI hardware id and are skipped. Failures are logged and skip the
device or the property; nothing panics.
*/

use ms_log::runtime_log;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DRV, GUID_DEVCLASS_DISPLAY, HDEVINFO, SETUP_DI_REGISTRY_PROPERTY,
    SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_HARDWAREID, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, GetLastError};
use windows_sys::Win32::System::Registry::KEY_READ;

use crate::win_registry::RegKey;

use super::{PciDisplayDevice, choose_registry_vram, decode_win32_utf16_strings, next_win32_buffer_units, parse_pci_hardware_id, parse_registry_memory_size};

/// `SetupDiGetClassDevsW` returns `INVALID_HANDLE_VALUE` (`(HANDLE)-1`) on failure; windows-sys
/// types `HDEVINFO` as `isize`, so the sentinel is `-1`.
const INVALID_DEVINFO: HDEVINFO = -1;

/// First buffer size for a property read, in UTF-16 units (hardware-id lists and device
/// descriptions fit; a larger one triggers the second call with the exact size).
const INITIAL_PROPERTY_UNITS: usize = 512;

/// Attempts per string read: the first call plus resize retries. A value that grows between
/// calls more often than this is given up on (logged), never looped on.
const MAX_READ_ATTEMPTS: usize = 4;

/// Owner of a SetupAPI device-information set; destroys it on drop.
struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid set returned by `SetupDiGetClassDevsW` (the constructor
        // rejects `INVALID_DEVINFO`), and this is the only place it is destroyed.
        let ok = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        if ok == 0 {
            // SAFETY: reads the calling thread's last-error value; no preconditions.
            let error = unsafe { GetLastError() };
            runtime_log::log_warn(format!("[amd-adapters] SetupDiDestroyDeviceInfoList failed (Win32 error {error})"));
        }
    }
}

/// Enumerates present display-class devices and returns the PCI ones. Never panics.
pub(super) fn enumerate() -> Vec<PciDisplayDevice> {
    // SAFETY: the GUID pointer refers to a `static`-lifetime constant; a null enumerator and a
    // null parent window are documented as "no filter" / "no UI".
    let raw = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_DISPLAY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT) };
    if raw == INVALID_DEVINFO {
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        runtime_log::log_warn(format!("[amd-adapters] SetupDiGetClassDevsW(display) failed (Win32 error {error})"));
        return Vec::new();
    }
    let set = DevInfoSet(raw);
    let mut devices = Vec::new();
    for index in 0_u32.. {
        let mut data = SP_DEVINFO_DATA {
            // `cbSize` must be the struct size; a fixed-size Win32 struct always fits in u32.
            cbSize: u32::try_from(std::mem::size_of::<SP_DEVINFO_DATA>()).unwrap_or(u32::MAX),
            ClassGuid: windows_sys::core::GUID::from_u128(0),
            DevInst: 0,
            Reserved: 0,
        };
        // SAFETY: `set.0` is a valid set and `data` is a properly sized, writable
        // `SP_DEVINFO_DATA` with `cbSize` set as the API requires.
        let ok = unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) };
        if ok == 0 {
            // SAFETY: reads the calling thread's last-error value; no preconditions.
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_MORE_ITEMS {
                runtime_log::log_warn(format!("[amd-adapters] SetupDiEnumDeviceInfo({index}) failed (Win32 error {error})"));
            }
            break;
        }
        if let Some(device) = device_from_info(&set, &data) {
            devices.push(device);
        }
    }
    devices
}

/// Builds the display device of one enumerated device, or `None` when it has no parseable PCI
/// hardware id.
fn device_from_info(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Option<PciDisplayDevice> {
    let hardware_ids = read_property_strings(set, data, SPDRP_HARDWAREID)?;
    let (vendor_id, device_id, revision) = hardware_ids.iter().find_map(|id| parse_pci_hardware_id(id))?;
    let os_name = read_property_strings(set, data, SPDRP_DEVICEDESC)
        .and_then(|strings| strings.into_iter().map(|text| text.trim().to_string()).find(|text| !text.is_empty()));
    let driver_key = open_driver_key(set, data);
    Some(PciDisplayDevice {
        vendor_id,
        device_id,
        revision,
        os_name,
        kernel_driver: None,
        driver_version: driver_key.as_ref().and_then(read_driver_version),
        reported_memory_bytes: driver_key.as_ref().and_then(|key| read_dedicated_memory(key, vendor_id, device_id)),
        kfd_gfx: None,
    })
}

/// Reads a string (`REG_SZ`) or string-list (`REG_MULTI_SZ`) device property as its
/// non-empty strings. `None` (logged) when the property cannot be read. Retries with the size
/// SetupAPI reports, at most [`MAX_READ_ATTEMPTS`] times and only while the buffer grows.
fn read_property_strings(set: &DevInfoSet, data: &SP_DEVINFO_DATA, property: SETUP_DI_REGISTRY_PROPERTY) -> Option<Vec<String>> {
    let mut buffer = vec![0_u16; INITIAL_PROPERTY_UNITS];
    for _attempt in 0..MAX_READ_ATTEMPTS {
        let byte_len = u32::try_from(buffer.len().checked_mul(2)?).ok()?;
        let mut required_bytes = 0_u32;
        // SAFETY: `set.0` and `data` describe a device enumerated from that set; the buffer
        // pointer is valid for `byte_len` writable bytes (u16 storage satisfies the WCHAR
        // alignment SetupAPI writes with); a null type pointer is allowed.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(set.0, data, property, std::ptr::null_mut(), buffer.as_mut_ptr().cast::<u8>(), byte_len, &mut required_bytes)
        };
        if ok != 0 {
            return Some(decode_win32_utf16_strings(&buffer, required_bytes));
        }
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER {
            runtime_log::log_info(format!("[amd-adapters] device property {property} unreadable (Win32 error {error})"));
            return None;
        }
        let Some(units) = next_win32_buffer_units(buffer.len(), required_bytes) else {
            runtime_log::log_info(format!("[amd-adapters] device property {property}: unusable size request of {required_bytes} bytes"));
            return None;
        };
        buffer = vec![0_u16; units];
    }
    runtime_log::log_info(format!("[amd-adapters] device property {property} kept growing for {MAX_READ_ATTEMPTS} attempts"));
    None
}

/// Opens the device's driver (software) registry key. `None` (logged) when it is unavailable.
fn open_driver_key(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Option<RegKey> {
    // SAFETY: `set.0` and `data` describe a device enumerated from that set; the scope,
    // hardware-profile (0 = current), key-type and access arguments are documented constants.
    let raw = unsafe { SetupDiOpenDevRegKey(set.0, data, DICS_FLAG_GLOBAL, 0, DIREG_DRV, KEY_READ) };
    let key = RegKey::from_raw(raw);
    if key.is_none() {
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        runtime_log::log_info(format!("[amd-adapters] driver key of a display device unavailable (Win32 error {error})"));
    }
    key
}

/// Reads `DriverVersion` (`REG_SZ`) from the driver key. `None` (logged) when missing or not a
/// non-empty string.
fn read_driver_version(key: &RegKey) -> Option<String> {
    let version = key.query_string("DriverVersion");
    if version.is_none() {
        runtime_log::log_info("[amd-adapters] DriverVersion missing or unreadable in a display driver key");
    }
    version
}

/// Reads the driver-reported dedicated memory: `HardwareInformation.qwMemorySize` (64-bit),
/// else `HardwareInformation.MemorySize` (32-bit, `REG_DWORD` or 4-byte `REG_BINARY`). `None`
/// when neither is present or usable; an unexpected read failure is logged.
fn read_dedicated_memory(key: &RegKey, vendor_id: u16, device_id: u16) -> Option<u64> {
    let read = |name: &str| match key.query(name) {
        Ok(value) => {
            let parsed = parse_registry_memory_size(value.value_type, &value.data);
            if parsed.is_none() {
                runtime_log::log_info(format!(
                    "[amd-adapters] {vendor_id:04x}:{device_id:04x}: {name} has unusable type {} / {} bytes",
                    value.value_type,
                    value.data.len()
                ));
            }
            parsed
        }
        // Absent is normal (basic display drivers, some vendors write only one of the two).
        Err(ERROR_FILE_NOT_FOUND) => None,
        Err(status) => {
            runtime_log::log_info(format!("[amd-adapters] {vendor_id:04x}:{device_id:04x}: {name} unreadable (Win32 error {status})"));
            None
        }
    };
    choose_registry_vram(read("HardwareInformation.qwMemorySize"), read("HardwareInformation.MemorySize"))
}
