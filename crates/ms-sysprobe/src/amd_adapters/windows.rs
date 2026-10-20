/*
File: crates/ms-sysprobe/src/amd_adapters/windows.rs

Purpose:
Windows half of `amd_adapters`: enumerates present display-class devices through SetupAPI,
keeps vendor 0x1002, takes the gfx from the pinned device-id table and reads the driver
version from the device's driver registry key. No WMI, no COM, no OpenCL.

Key structures:
- DevInfoSet / DriverKey: RAII owners of the SetupAPI device-info set and the registry key, so
  each handle is released on every path (`SetupDiDestroyDeviceInfoList`, `RegCloseKey`).

Key functions:
- enumerate(): SetupAPI display class -> `AmdAdapter`s (unordered; the caller orders them).

Notes:
Every `unsafe` block carries a `// SAFETY:` comment. String reads (device properties and
`DriverVersion`) retry with the byte count the API reports, at most `MAX_READ_ATTEMPTS` times,
only while the buffer grows and never past `MAX_WIN32_STRING_UNITS`; decoding and NUL trimming
are the cfg-free `decode_win32_utf16_strings` in `mod.rs`. Failures are logged and skip
the device or the property; nothing panics.
*/

use ms_log::runtime_log;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DRV, GUID_DEVCLASS_DISPLAY, HDEVINFO, SETUP_DI_REGISTRY_PROPERTY,
    SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_HARDWAREID, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Registry::{HKEY, KEY_READ, REG_SZ, RegCloseKey, RegQueryValueExW};

use super::{AMD_PCI_VENDOR_ID, AmdAdapter, GfxSource, decode_win32_utf16_strings, device_id_info, next_win32_buffer_units, parse_pci_hardware_id};

/// `SetupDiGetClassDevsW` returns `INVALID_HANDLE_VALUE` (`(HANDLE)-1`) on failure; windows-sys
/// types `HDEVINFO` as `isize`, so the sentinel is `-1`.
const INVALID_DEVINFO: HDEVINFO = -1;

/// First buffer size for a property read, in UTF-16 units (hardware-id lists and device
/// descriptions fit; a larger one triggers the second call with the exact size).
const INITIAL_PROPERTY_UNITS: usize = 512;

/// First buffer size for `DriverVersion`, in UTF-16 units (`"32.0.31041.5005"` is 16).
const INITIAL_DRIVER_VERSION_UNITS: usize = 64;

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

/// Owner of an opened driver registry key; closes it on drop.
struct DriverKey(HKEY);

impl Drop for DriverKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid key returned by `SetupDiOpenDevRegKey` (the opener
        // rejects null and `INVALID_HANDLE_VALUE`), and this is the only place it is closed.
        let status = unsafe { RegCloseKey(self.0) };
        if status != ERROR_SUCCESS {
            runtime_log::log_warn(format!("[amd-adapters] RegCloseKey failed (Win32 error {status})"));
        }
    }
}

/// Enumerates present display-class devices and returns the AMD ones. Never panics.
pub(super) fn enumerate() -> Vec<AmdAdapter> {
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
    let mut adapters = Vec::new();
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
        if let Some(adapter) = adapter_from_device(&set, &data) {
            adapters.push(adapter);
        }
    }
    adapters
}

/// Builds the adapter of one enumerated device, or `None` when it is not a parseable AMD
/// PCI device.
fn adapter_from_device(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Option<AmdAdapter> {
    let hardware_ids = read_property_strings(set, data, SPDRP_HARDWAREID)?;
    let (vendor_id, device_id, revision) = hardware_ids.iter().find_map(|id| parse_pci_hardware_id(id))?;
    if vendor_id != AMD_PCI_VENDOR_ID {
        return None;
    }
    let table = device_id_info(device_id);
    let name = read_property_strings(set, data, SPDRP_DEVICEDESC)
        .and_then(|strings| strings.into_iter().find(|text| !text.trim().is_empty()))
        .or_else(|| table.as_ref().map(|info| info.product.to_string()))
        .unwrap_or_else(|| format!("AMD [{AMD_PCI_VENDOR_ID:04x}:{device_id:04x}]"));
    if table.is_none() {
        runtime_log::log_info(format!("[amd-adapters] AMD adapter {name} ({device_id:#06x}) is not in the device-id table; its gfx is unknown"));
    }
    Some(AmdAdapter {
        vendor_id,
        device_id,
        revision,
        name,
        gfx: table.as_ref().map(|info| info.gfx.clone()),
        gfx_source: table.as_ref().map(|_| GfxSource::DeviceIdTable),
        integrated: table.as_ref().map(|info| info.integrated),
        driver_version: read_driver_version(set, data),
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

/// Reads `DriverVersion` from the device's driver (software) registry key. `None` (logged)
/// when the key or value is missing or not a string. Grows the buffer on `ERROR_MORE_DATA`
/// with the size the registry reports, bounded like [`read_property_strings`].
fn read_driver_version(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Option<String> {
    // SAFETY: `set.0` and `data` describe a device enumerated from that set; the scope,
    // hardware-profile (0 = current), key-type and access arguments are documented constants.
    let raw = unsafe { SetupDiOpenDevRegKey(set.0, data, DICS_FLAG_GLOBAL, 0, DIREG_DRV, KEY_READ) };
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        runtime_log::log_info(format!("[amd-adapters] driver key of an AMD adapter unavailable (Win32 error {error})"));
        return None;
    }
    let key = DriverKey(raw);
    let name: Vec<u16> = "DriverVersion".encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = vec![0_u16; INITIAL_DRIVER_VERSION_UNITS];
    for _attempt in 0..MAX_READ_ATTEMPTS {
        let mut byte_len = u32::try_from(buffer.len().checked_mul(2)?).ok()?;
        let mut value_type = 0_u32;
        // SAFETY: `key.0` is an open key; `name` is NUL-terminated UTF-16; the data pointer is
        // valid for `byte_len` writable bytes and `byte_len` / `value_type` are valid out-params.
        let status = unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut value_type, buffer.as_mut_ptr().cast::<u8>(), &mut byte_len) };
        if status == ERROR_MORE_DATA {
            // `byte_len` now holds the size the value needs.
            let Some(units) = next_win32_buffer_units(buffer.len(), byte_len) else {
                runtime_log::log_info(format!("[amd-adapters] DriverVersion: unusable size request of {byte_len} bytes"));
                return None;
            };
            buffer = vec![0_u16; units];
            continue;
        }
        if status != ERROR_SUCCESS || value_type != REG_SZ {
            runtime_log::log_info(format!("[amd-adapters] DriverVersion unreadable (status {status}, type {value_type})"));
            return None;
        }
        // A REG_SZ value may or may not include its terminating NUL; the decoder cuts at it.
        let text = decode_win32_utf16_strings(&buffer, byte_len).into_iter().next()?;
        let text = text.trim().to_string();
        return (!text.is_empty()).then_some(text);
    }
    runtime_log::log_info(format!("[amd-adapters] DriverVersion kept growing for {MAX_READ_ATTEMPTS} attempts"));
    None
}
