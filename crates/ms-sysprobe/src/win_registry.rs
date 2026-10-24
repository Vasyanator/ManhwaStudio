/*
File: crates/ms-sysprobe/src/win_registry.rs

Purpose:
Crate-private Windows registry reads shared by the probes that need them (`amd_adapters`'
driver key, `system_facts`' OS and CPU keys). Windows-only (`#[cfg(windows)]` on the `mod`).

Key structures:
- RegKey: RAII owner of an open `HKEY`; closes it on drop.
- RegValue: one raw value (type + bytes).

Key functions:
- RegKey::open_local_machine(): open a read-only `HKEY_LOCAL_MACHINE` subkey.
- RegKey::query(): raw value read with a bounded grow-and-retry loop.
- RegKey::query_string(): a `REG_SZ` / `REG_EXPAND_SZ` value as trimmed, non-empty text.
- RegKey::query_u32(): a `REG_DWORD` value.

Notes:
Every `unsafe` block carries a `// SAFETY:` comment. Reads retry with the byte count the
registry reports, at most `MAX_READ_ATTEMPTS` times, only while the buffer grows and never past
`MAX_VALUE_BYTES`. Nothing here logs: callers know which value matters and log with context.
*/

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

use crate::amd_adapters::decode_win32_utf16_strings;

/// First buffer size of a value read, in bytes (strings and integers fit; a larger value
/// triggers a second call with the exact size).
const INITIAL_VALUE_BYTES: usize = 256;

/// Upper bound of a value read (128 KiB): every value these probes read is a few hundred bytes,
/// so a larger request is treated as bogus.
const MAX_VALUE_BYTES: usize = 128 * 1024;

/// Attempts per value read: the first call plus resize retries.
const MAX_READ_ATTEMPTS: usize = 4;

/// Owner of an open registry key; closes it on drop.
#[derive(Debug)]
pub(crate) struct RegKey(HKEY);

/// One raw registry value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegValue {
    /// Registry value type (`REG_SZ`, `REG_DWORD`, `REG_QWORD`, `REG_BINARY`, ...).
    pub(crate) value_type: u32,
    /// The value's bytes, exactly as long as the registry reported.
    pub(crate) data: Vec<u8>,
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid open key (both constructors reject null and failure
        // results), and this is the only place it is closed.
        let status = unsafe { RegCloseKey(self.0) };
        if status != ERROR_SUCCESS {
            ms_log::runtime_log::log_warn(format!("[win-registry] RegCloseKey failed (Win32 error {status})"));
        }
    }
}

impl RegKey {
    /// Takes ownership of a key handle another API opened (e.g. `SetupDiOpenDevRegKey`);
    /// `None` for a null or `INVALID_HANDLE_VALUE` handle.
    pub(crate) fn from_raw(raw: HKEY) -> Option<Self> {
        (!raw.is_null() && raw != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE).then_some(Self(raw))
    }

    /// Opens `HKEY_LOCAL_MACHINE\{subkey}` for reading in the 64-bit view (so a 32-bit build
    /// would read the same keys).
    ///
    /// # Errors
    /// The Win32 error code of `RegOpenKeyExW` (`ERROR_FILE_NOT_FOUND` = no such key).
    pub(crate) fn open_local_machine(subkey: &str) -> Result<Self, u32> {
        let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
        let mut raw: HKEY = std::ptr::null_mut();
        // SAFETY: `HKEY_LOCAL_MACHINE` is a predefined key; `wide` is NUL-terminated UTF-16 that
        // outlives the call; `raw` is a valid out-pointer.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut raw) };
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        Self::from_raw(raw).ok_or(status)
    }

    /// Reads value `name` as raw bytes.
    ///
    /// # Errors
    /// The Win32 status of `RegQueryValueExW` (`ERROR_FILE_NOT_FOUND` = no such value), or
    /// `ERROR_MORE_DATA` when the value kept growing or exceeds `MAX_VALUE_BYTES`.
    pub(crate) fn query(&self, name: &str) -> Result<RegValue, u32> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buffer = vec![0_u8; INITIAL_VALUE_BYTES];
        for _attempt in 0..MAX_READ_ATTEMPTS {
            let mut byte_len = u32::try_from(buffer.len()).map_err(|_| ERROR_MORE_DATA)?;
            let mut value_type = 0_u32;
            // SAFETY: `self.0` is an open key; `wide` is NUL-terminated UTF-16; the data pointer
            // is valid for `byte_len` writable bytes and `byte_len` / `value_type` are valid
            // out-params.
            let status = unsafe { RegQueryValueExW(self.0, wide.as_ptr(), std::ptr::null(), &mut value_type, buffer.as_mut_ptr(), &mut byte_len) };
            if status == ERROR_MORE_DATA {
                // `byte_len` now holds the size the value needs.
                let required = usize::try_from(byte_len).map_err(|_| ERROR_MORE_DATA)?;
                if required <= buffer.len() || required > MAX_VALUE_BYTES {
                    return Err(ERROR_MORE_DATA);
                }
                buffer = vec![0_u8; required];
                continue;
            }
            if status != ERROR_SUCCESS {
                return Err(status);
            }
            let used = usize::try_from(byte_len).map_or(buffer.len(), |len| len.min(buffer.len()));
            buffer.truncate(used);
            return Ok(RegValue { value_type, data: buffer });
        }
        Err(ERROR_MORE_DATA)
    }

    /// Reads a `REG_SZ` / `REG_EXPAND_SZ` value as trimmed text (environment references are
    /// NOT expanded). `None` when the value is missing, of another type, or blank.
    pub(crate) fn query_string(&self, name: &str) -> Option<String> {
        let value = self.query(name).ok()?;
        if value.value_type != REG_SZ && value.value_type != REG_EXPAND_SZ {
            return None;
        }
        let units: Vec<u16> = value.data.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        let byte_len = u32::try_from(value.data.len()).ok()?;
        let text = decode_win32_utf16_strings(&units, byte_len).into_iter().next()?;
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Reads a `REG_DWORD` value. `None` when missing or of another type / size.
    pub(crate) fn query_u32(&self, name: &str) -> Option<u32> {
        let value = self.query(name).ok()?;
        if value.value_type != REG_DWORD {
            return None;
        }
        Some(u32::from_le_bytes(value.data.as_slice().try_into().ok()?))
    }
}
