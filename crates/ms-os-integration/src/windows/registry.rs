/*
File: crates/ms-os-integration/src/windows/registry.rs

Purpose:
Native registry I/O of the Windows records (Win32 `advapi32`, always the 64-bit view):
key presence, UTF-16 string reads, value writes, tree deletes, the Explorer association
refresh, and the record-level writers / removers built on the tables of `values.rs`.

Key structures:
- `RegistryHive`, `RegistryKeyPresence`
- `RegistryRemovalSummary`, `KeptRegistryEntry`, `KeptReason` (what uninstall left in place)

Key functions:
- `register_windows_install_in_registry()`, `remove_windows_registry_entries_for_install()`
- `register_windows_open_with()`, `write_windows_open_with()` (no notify),
  `notify_shell_association_change()`
- `reg_read_string()`, `reg_write_value()`, `reg_delete_tree_if_exists()`,
  `registry_key_presence()`
- `split_registry_key()`, `classify_registry_open_status()` (pure)

Notes:
Every key is opened with `KEY_WOW64_64KEY` and every read uses `RRF_SUBKEY_WOW6464KEY`, so a
32-bit build would still see the same records a 64-bit one writes. Existence and failures are
decided by numeric Win32 status codes, never by message text. No child process is spawned.
Every function is blocking; callers run it on a worker thread.
*/

#[cfg(target_os = "windows")]
use std::path::Path;

#[cfg(target_os = "windows")]
use super::values::{
    RecordOwnership, RegistryValue, app_paths_entry_ownership, app_paths_key, open_with_command_targets_install, uninstall_and_app_paths_values,
    uninstall_entry_ownership, uninstall_key, windows_open_with_app_key, windows_open_with_registry_values,
};
#[cfg(target_os = "windows")]
use super::{to_wide, windows_uninstall_registry_root};
use crate::IntegrationError;

/// Registry hive of a `HKLM\…` / `HKCU\…` key string, the only two roots this crate writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryHive {
    /// `HKEY_LOCAL_MACHINE` (`HKLM`): all-users records.
    LocalMachine,
    /// `HKEY_CURRENT_USER` (`HKCU`): per-user records.
    CurrentUser,
}

/// Splits `HKLM\sub\key` / `HKCU\sub\key` (root case-insensitive) into hive and subkey path.
/// `None` for any other root or an empty subkey.
#[must_use]
pub fn split_registry_key(key: &str) -> Option<(RegistryHive, &str)> {
    let (root, subkey) = key.split_once('\\')?;
    if subkey.is_empty() {
        return None;
    }
    let hive = if root.eq_ignore_ascii_case("HKLM") {
        RegistryHive::LocalMachine
    } else if root.eq_ignore_ascii_case("HKCU") {
        RegistryHive::CurrentUser
    } else {
        return None;
    };
    Some((hive, subkey))
}

/// `ERROR_SUCCESS` / `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND` from `winerror.h`, mirrored
/// so the status classification below is testable off Windows; a Windows-only test pins them
/// to the `windows-sys` constants.
const WIN32_ERROR_SUCCESS: u32 = 0;
const WIN32_ERROR_FILE_NOT_FOUND: u32 = 2;
const WIN32_ERROR_PATH_NOT_FOUND: u32 = 3;

/// What a registry key open status says about the key's existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryKeyPresence {
    /// The key exists.
    Present,
    /// The key (or a parent) does not exist.
    Absent,
    /// Any other status (e.g. access denied): existence unknown. Carries the Win32 code.
    Unknown(u32),
}

/// Maps a Win32 registry status to key existence by numeric code, never by message text.
#[must_use]
pub fn classify_registry_open_status(status: u32) -> RegistryKeyPresence {
    match status {
        WIN32_ERROR_SUCCESS => RegistryKeyPresence::Present,
        WIN32_ERROR_FILE_NOT_FOUND | WIN32_ERROR_PATH_NOT_FOUND => RegistryKeyPresence::Absent,
        other => RegistryKeyPresence::Unknown(other),
    }
}

/// Predefined root handle of `hive` (never closed).
#[cfg(target_os = "windows")]
fn hive_handle(hive: RegistryHive) -> windows_sys::Win32::System::Registry::HKEY {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    match hive {
        RegistryHive::LocalMachine => HKEY_LOCAL_MACHINE,
        RegistryHive::CurrentUser => HKEY_CURRENT_USER,
    }
}

/// An open registry key handle, closed exactly once on drop (a failed close is logged: the
/// operation on the key already completed, so there is no decision left to change).
#[cfg(target_os = "windows")]
#[derive(Debug)]
struct OpenKey {
    handle: windows_sys::Win32::System::Registry::HKEY,
    key: String,
}

#[cfg(target_os = "windows")]
impl Drop for OpenKey {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Registry::RegCloseKey;
        // SAFETY: `handle` was returned by a successful open/create and is closed only here.
        let status = unsafe { RegCloseKey(self.handle) };
        if status != WIN32_ERROR_SUCCESS {
            ms_log::runtime_log::log_warn(format!(
                "[windows-registry] RegCloseKey failed for {} (Windows error {status})",
                self.key
            ));
        }
    }
}

/// Opens the existing key `key` (`HKLM\…` / `HKCU\…`) in the 64-bit view with `access`.
/// `Err` carries the Win32 status (`ERROR_INVALID_PARAMETER` for an unsupported root).
#[cfg(target_os = "windows")]
fn open_key(key: &str, access: u32) -> Result<OpenKey, u32> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Registry::{KEY_WOW64_64KEY, RegOpenKeyExW};

    let (hive, subkey) = split_registry_key(key).ok_or(ERROR_INVALID_PARAMETER)?;
    let subkey_wide = to_wide(subkey);
    let mut handle = std::ptr::null_mut();
    // SAFETY: `subkey_wide` is a NUL-terminated UTF-16 buffer alive for the call and `handle`
    // is a valid out-pointer; the root is a predefined handle.
    let status = unsafe { RegOpenKeyExW(hive_handle(hive), subkey_wide.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut handle) };
    if status == WIN32_ERROR_SUCCESS {
        Ok(OpenKey { handle, key: key.to_owned() })
    } else {
        Err(status)
    }
}

/// Opens `HKLM\…` / `HKCU\…` (64-bit view) for query and closes it again, classifying the
/// status. A key string with another root yields `Unknown(ERROR_INVALID_PARAMETER)`.
#[cfg(target_os = "windows")]
#[must_use]
pub fn registry_key_presence(key: &str) -> RegistryKeyPresence {
    use windows_sys::Win32::System::Registry::KEY_QUERY_VALUE;
    match open_key(key, KEY_QUERY_VALUE) {
        // The handle is only proof of existence; it closes when the binding drops.
        Ok(_closed_on_drop) => RegistryKeyPresence::Present,
        Err(status) => classify_registry_open_status(status),
    }
}

/// Data calls [`reg_read_string`] makes after its size probe before giving up with
/// `ERROR_MORE_DATA` (each retry regrows the buffer to the size the failing call reported).
#[cfg(target_os = "windows")]
const REG_READ_DATA_ATTEMPTS: usize = 4;

/// Reads the `REG_SZ` value `value_name` (`None` = the default value) of `HKLM\…` / `HKCU\…`
/// as UTF-16 through `RegGetValueW` (64-bit view; a `REG_EXPAND_SZ` value comes back
/// expanded). `Ok(None)` when the key or the value is absent; `Err(status)` with the Win32
/// error code for any other failure (access denied, another value type, an unsupported root;
/// `ERROR_MORE_DATA` when the value kept outgrowing the buffer for every data attempt).
#[cfg(target_os = "windows")]
pub fn reg_read_string(key: &str, value_name: Option<&str>) -> Result<Option<String>, u32> {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_MORE_DATA};
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW};

    let (hive, subkey) = split_registry_key(key).ok_or(ERROR_INVALID_PARAMETER)?;
    let root = hive_handle(hive);
    let subkey_wide = to_wide(subkey);
    let value_wide = value_name.map(to_wide);
    let value_ptr = value_wide.as_ref().map_or(std::ptr::null(), |wide| wide.as_ptr());
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
    let mut size_bytes: u32 = 0;
    // SAFETY: NUL-terminated `subkey_wide` / value name (or null = default value); a null
    // data pointer with a valid size out-pointer is the documented size query.
    let status = unsafe { RegGetValueW(root, subkey_wide.as_ptr(), value_ptr, flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut size_bytes) };
    match classify_registry_open_status(status) {
        RegistryKeyPresence::Present => {}
        RegistryKeyPresence::Absent => return Ok(None),
        RegistryKeyPresence::Unknown(code) => return Err(code),
    }
    // The probe can under-report (the value grew meanwhile, or a `REG_EXPAND_SZ` expands to
    // more than its stored size); `ERROR_MORE_DATA` then reports the size the data call
    // needs, so the buffer is regrown to that size and only the data call is retried.
    for _ in 0..REG_READ_DATA_ATTEMPTS {
        let len_units = usize::try_from(size_bytes).map_err(|_| ERROR_INVALID_PARAMETER)?.div_ceil(2);
        let mut buffer = vec![0u16; len_units.max(1)];
        let byte_len = buffer.len().checked_mul(2).ok_or(ERROR_INVALID_PARAMETER)?;
        let mut buffer_bytes = u32::try_from(byte_len).map_err(|_| ERROR_INVALID_PARAMETER)?;
        // SAFETY: `buffer` holds `buffer_bytes` writable bytes and outlives the call.
        let status = unsafe { RegGetValueW(root, subkey_wide.as_ptr(), value_ptr, flags, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut buffer_bytes) };
        if status == ERROR_MORE_DATA {
            // `buffer_bytes` now holds the required size; insist on growth so a buggy
            // report cannot spin on the same size.
            let grown = byte_len.checked_add(2).and_then(|bytes| u32::try_from(bytes).ok()).ok_or(ERROR_INVALID_PARAMETER)?;
            size_bytes = buffer_bytes.max(grown);
            continue;
        }
        match classify_registry_open_status(status) {
            RegistryKeyPresence::Present => {}
            RegistryKeyPresence::Absent => return Ok(None),
            RegistryKeyPresence::Unknown(code) => return Err(code),
        }
        // RRF_RT_REG_SZ guarantees NUL termination; drop it and anything after it.
        let text_len = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
        return Ok(Some(String::from_utf16_lossy(&buffer[..text_len])));
    }
    Err(ERROR_MORE_DATA)
}

/// Writes one value (`REG_SZ` or `REG_DWORD`, payload from [`RegistryValue::encode`]) in the
/// 64-bit view, creating its key when missing.
///
/// # Errors
/// `RegistryOpenKey` when the key cannot be created or opened for writing (e.g. 5 = access
/// denied), `RegistrySetValue` when the value write fails.
#[cfg(target_os = "windows")]
pub fn reg_write_value(value: &RegistryValue) -> Result<(), IntegrationError> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Registry::{KEY_SET_VALUE, KEY_WOW64_64KEY, REG_OPTION_NON_VOLATILE, RegCreateKeyExW, RegSetValueExW};

    let key = value.key();
    let open_error = |code: u32| IntegrationError::RegistryOpenKey { key: key.to_owned(), code };
    let set_error = |code: u32| IntegrationError::RegistrySetValue { key: key.to_owned(), value_name: value.value_name().map(str::to_owned), code };
    let (hive, subkey) = split_registry_key(key).ok_or_else(|| open_error(ERROR_INVALID_PARAMETER))?;
    let subkey_wide = to_wide(subkey);
    let mut handle = std::ptr::null_mut();
    // SAFETY: NUL-terminated `subkey_wide`, null class and security attributes (defaults),
    // valid handle out-pointer, null disposition (not needed).
    let status = unsafe {
        RegCreateKeyExW(
            hive_handle(hive),
            subkey_wide.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            std::ptr::null(),
            &mut handle,
            std::ptr::null_mut(),
        )
    };
    if status != WIN32_ERROR_SUCCESS {
        return Err(open_error(status));
    }
    let opened = OpenKey { handle, key: key.to_owned() };
    let (value_type, payload) = value.encode();
    let payload_len = u32::try_from(payload.len()).map_err(|_| set_error(ERROR_INVALID_PARAMETER))?;
    let name_wide = value.value_name().map(to_wide);
    let name_ptr = name_wide.as_ref().map_or(std::ptr::null(), |wide| wide.as_ptr());
    // SAFETY: `opened.handle` is an open key with KEY_SET_VALUE; `payload` holds `payload_len`
    // readable bytes in the layout `value_type` requires (NUL-terminated UTF-16LE / 4-byte LE).
    let status = unsafe { RegSetValueExW(opened.handle, name_ptr, 0, value_type.win32_code(), payload.as_ptr(), payload_len) };
    if status != WIN32_ERROR_SUCCESS {
        return Err(set_error(status));
    }
    Ok(())
}

/// Deletes the registry key `key` (`HKLM\…` / `HKCU\…`, 64-bit view) with all its subkeys and
/// values. Returns `Ok(false)` when the key does not exist, `Ok(true)` when it was deleted.
///
/// # Errors
/// `RegistryDeleteKey` with the Win32 code when the existing key cannot be opened for deletion
/// or deleted (e.g. 5 = access denied without elevation).
#[cfg(target_os = "windows")]
pub fn reg_delete_tree_if_exists(key: &str) -> Result<bool, IntegrationError> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    // The standard `DELETE` right is declared only in `Storage::FileSystem`, a feature this
    // crate enables on `windows` (for the shortcut code) but not on `windows-sys`.
    use ::windows::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::System::Registry::{KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, RegDeleteKeyExW, RegDeleteTreeW};

    let delete_error = |code: u32| IntegrationError::RegistryDeleteKey { key: key.to_owned(), code };
    let (hive, subkey) = split_registry_key(key).ok_or_else(|| delete_error(ERROR_INVALID_PARAMETER))?;
    // `RegDeleteTreeW` has no view flag, so the key itself is opened in the 64-bit view and its
    // CONTENT deleted through that handle; the then-empty key is removed with
    // `RegDeleteKeyExW(KEY_WOW64_64KEY)`.
    // Exactly the rights `RegDeleteTreeW` documents (no WRITE_DAC / WRITE_OWNER, which a key's
    // DACL may withhold while still allowing the delete); `open_key` adds the 64-bit view.
    let opened = match open_key(key, DELETE.0 | KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_SET_VALUE) {
        Ok(opened) => opened,
        Err(status) => {
            return match classify_registry_open_status(status) {
                RegistryKeyPresence::Absent => Ok(false),
                RegistryKeyPresence::Present | RegistryKeyPresence::Unknown(_) => Err(delete_error(status)),
            };
        }
    };
    // SAFETY: `opened.handle` is an open key with the rights RegDeleteTreeW requires; a null
    // subkey deletes its content.
    let status = unsafe { RegDeleteTreeW(opened.handle, std::ptr::null()) };
    drop(opened);
    if status != WIN32_ERROR_SUCCESS {
        return Err(delete_error(status));
    }
    let subkey_wide = to_wide(subkey);
    // SAFETY: NUL-terminated `subkey_wide` alive for the call; the root is a predefined handle.
    let status = unsafe { RegDeleteKeyExW(hive_handle(hive), subkey_wide.as_ptr(), KEY_WOW64_64KEY, 0) };
    match classify_registry_open_status(status) {
        // Absent here means someone else removed the emptied key meanwhile: still gone.
        RegistryKeyPresence::Present | RegistryKeyPresence::Absent => Ok(true),
        RegistryKeyPresence::Unknown(code) => Err(delete_error(code)),
    }
}

/// Tells Explorer that file associations changed (`SHChangeNotify(SHCNE_ASSOCCHANGED,
/// SHCNF_IDLIST)`), so "Open with" lists and App Paths lookups refresh without a sign-out.
/// Called once at the end of every public operation that wrote or deleted an "Open with" or
/// App Paths key. Has no failure result. Public so a caller that composes several
/// non-notifying writes (e.g. [`write_windows_open_with`]) can notify once at the end.
#[cfg(target_os = "windows")]
pub fn notify_shell_association_change() {
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    // The event id parameter is `i32` while the constant is `u32`; `SHCNE_ASSOCCHANGED`
    // (0x0800_0000) fits, the bit-exact reinterpretation keeps it unchanged.
    let event_id = i32::from_ne_bytes(SHCNE_ASSOCCHANGED.to_ne_bytes());
    // SAFETY: SHCNE_ASSOCCHANGED takes no items; both item pointers must be null.
    unsafe { SHChangeNotify(event_id, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    ms_log::runtime_log::log_info("[windows-registry] association change notified (SHCNE_ASSOCCHANGED)");
}

/// Writes the App Paths and Uninstall entries of the install at `install_dir` under its
/// registry root ([`windows_uninstall_registry_root`]), value by value in the order of
/// [`uninstall_and_app_paths_values`]; `display_version` is the installed program's version
/// core (`DisplayVersion`). Stops at the first failed write (earlier values stay), then
/// notifies Explorer of the App Paths change.
///
/// # Errors
/// The first failing write (`RegistryOpenKey` / `RegistrySetValue`).
#[cfg(target_os = "windows")]
pub fn register_windows_install_in_registry(install_dir: &Path, launcher_path: &Path, display_version: &str) -> Result<(), IntegrationError> {
    let registry_root = windows_uninstall_registry_root(install_dir);
    let values = uninstall_and_app_paths_values(registry_root, &install_dir.to_string_lossy(), &launcher_path.to_string_lossy(), display_version);
    let result = values.iter().try_for_each(reg_write_value);
    // The App Paths key is the table's first entry: even a failed run may have changed it.
    notify_shell_association_change();
    result
}

/// Writes the "Open with" entry under `registry_root` (`"HKLM"` all-users, `"HKCU"` per-user)
/// WITHOUT notifying Explorer: the caller must call [`notify_shell_association_change`]
/// afterwards, also after a failure. The owned tree is deleted first so a re-install never
/// keeps extensions dropped from the input table; an entry of another install under the same
/// root is replaced (last install wins). Stops at the first failure.
///
/// # Errors
/// The failing delete (`RegistryDeleteKey`) or write (`RegistryOpenKey` / `RegistrySetValue`).
#[cfg(target_os = "windows")]
pub fn write_windows_open_with(registry_root: &str, launcher_path: &Path) -> Result<(), IntegrationError> {
    reg_delete_tree_if_exists(&windows_open_with_app_key(registry_root)).and_then(|_deleted| {
        windows_open_with_registry_values(registry_root, &launcher_path.to_string_lossy())
            .into_iter()
            .try_for_each(|value| reg_write_value(&RegistryValue::String(value)))
    })
}

/// [`write_windows_open_with`] followed by one Explorer notification (also after a partial
/// failure). The installer calls it through its best-effort wrapper, which never fails an
/// install.
///
/// # Errors
/// As [`write_windows_open_with`].
#[cfg(target_os = "windows")]
pub fn register_windows_open_with(registry_root: &str, launcher_path: &Path) -> Result<(), IntegrationError> {
    let result = write_windows_open_with(registry_root, launcher_path);
    notify_shell_association_change();
    result
}

/// Removes the "Open with" tree under `registry_root` only when its open command launches an
/// executable inside `install_dir` (the key is shared by every install under one root, see
/// [`super::values::WINDOWS_OPEN_WITH_APP_NAME`]). A tree owned by another install, a missing
/// command, or an unreadable command is left in place and logged. Returns what happened to the
/// tree. Errors: only a failed deletion of a tree this install owns. Does not notify.
#[cfg(target_os = "windows")]
fn remove_windows_open_with_for_install(registry_root: &str, install_dir: &Path) -> Result<RecordRemoval, IntegrationError> {
    let app_key = windows_open_with_app_key(registry_root);
    let command_key = format!(r"{app_key}\shell\open\command");
    match reg_read_string(&command_key, None) {
        Ok(Some(command)) if open_with_command_targets_install(&command, &install_dir.to_string_lossy()) => {
            reg_delete_tree_if_exists(&app_key).map(RecordRemoval::from_deleted)
        }
        Ok(Some(command)) => {
            ms_log::runtime_log::log_info(format!(
                "[windows-uninstall] \"Open with\" entry {app_key} belongs to another install ({command}); left in place"
            ));
            Ok(RecordRemoval::Kept(KeptRegistryEntry { key: app_key, reason: KeptReason::OtherInstall }))
        }
        // No command: either the tree is absent, or it holds nothing that names an install.
        Ok(None) => match registry_key_presence(&app_key) {
            RegistryKeyPresence::Absent => Ok(RecordRemoval::Absent),
            RegistryKeyPresence::Present => {
                ms_log::runtime_log::log_warn(format!(
                    "[windows-uninstall] \"Open with\" entry {app_key} has no open command to check against {}; left in place",
                    install_dir.display()
                ));
                Ok(RecordRemoval::Kept(KeptRegistryEntry { key: app_key, reason: KeptReason::NoEvidence }))
            }
            RegistryKeyPresence::Unknown(status) => {
                ms_log::runtime_log::log_warn(format!(
                    "[windows-uninstall] existence of {app_key} undetermined (Windows error {status}); left in place"
                ));
                Ok(RecordRemoval::Kept(KeptRegistryEntry { key: app_key, reason: KeptReason::Unreadable(status) }))
            }
        },
        Err(status) => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-uninstall] could not read {command_key} (Windows error {status}); \"Open with\" entry left in place"
            ));
            Ok(RecordRemoval::Kept(KeptRegistryEntry { key: app_key, reason: KeptReason::Unreadable(status) }))
        }
    }
}

/// Why uninstall left an existing registry entry in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptReason {
    /// Its identifying values point at another install (`RecordOwnership::Foreign`).
    OtherInstall,
    /// It holds no identifying value to check against the install (`RecordOwnership::NoEvidence`).
    NoEvidence,
    /// Its existence or an identifying value could not be read; carries the Win32 code.
    Unreadable(u32),
}

/// One existing registry entry uninstall did not delete, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRegistryEntry {
    /// Full key (`HKLM\…` / `HKCU\…`).
    pub key: String,
    /// Why it was left in place.
    pub reason: KeptReason,
}

/// What a successful [`remove_windows_registry_entries_for_install`] did. Every entry it did
/// not delete although the key exists is listed in `kept` (attempt order); entries that were
/// absent or deleted are not listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryRemovalSummary {
    /// Existing entries left in place (another install's, unverifiable or unreadable ones).
    pub kept: Vec<KeptRegistryEntry>,
}

/// Outcome of one record's ownership-checked removal.
#[derive(Debug)]
enum RecordRemoval {
    /// The key did not exist (or vanished before the delete).
    Absent,
    /// The key belonged to the install and was deleted.
    Deleted,
    /// The key exists and was left in place.
    Kept(KeptRegistryEntry),
}

#[cfg(target_os = "windows")]
impl RecordRemoval {
    /// Maps [`reg_delete_tree_if_exists`]'s `true` (deleted) / `false` (absent).
    fn from_deleted(deleted: bool) -> Self {
        if deleted { Self::Deleted } else { Self::Absent }
    }
}

/// A record uninstall deletes only when it belongs to the uninstalled directory.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy)]
enum OwnedRecord {
    /// `Uninstall\ManhwaStudio`, judged by `InstallLocation` + `UninstallString`.
    Uninstall,
    /// `App Paths\manhwastudio_rs.exe`, judged by its default value + `Path`.
    AppPaths,
}

#[cfg(target_os = "windows")]
impl OwnedRecord {
    /// The two identifying value names (`None` = the default value) the ownership rule reads.
    fn evidence_value_names(self) -> [Option<&'static str>; 2] {
        match self {
            Self::Uninstall => [Some("InstallLocation"), Some("UninstallString")],
            Self::AppPaths => [None, Some("Path")],
        }
    }

    /// The pure ownership rule of `values.rs` for this record.
    fn ownership(self, values: &[Option<String>; 2], install_dir: &str) -> RecordOwnership {
        let [first, second] = values;
        match self {
            Self::Uninstall => uninstall_entry_ownership(first.as_deref(), second.as_deref(), install_dir),
            Self::AppPaths => app_paths_entry_ownership(first.as_deref(), second.as_deref(), install_dir),
        }
    }
}

/// Deletes `key` only when its identifying values prove it belongs to `install_dir`
/// ([`RecordOwnership::Owned`]). A missing key is a no-op; a key of another install, one without
/// evidence, or one whose existence or values cannot be read is left in place and logged.
/// Returns what happened to the key.
///
/// # Errors
/// Only a failed deletion of a key this install owns (`RegistryDeleteKey`).
#[cfg(target_os = "windows")]
fn remove_record_if_owned(key: &str, record: OwnedRecord, install_dir: &Path) -> Result<RecordRemoval, IntegrationError> {
    let kept = |reason: KeptReason| RecordRemoval::Kept(KeptRegistryEntry { key: key.to_owned(), reason });
    match registry_key_presence(key) {
        RegistryKeyPresence::Absent => return Ok(RecordRemoval::Absent),
        RegistryKeyPresence::Present => {}
        RegistryKeyPresence::Unknown(status) => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-uninstall] existence of {key} undetermined (Windows error {status}); left in place"
            ));
            return Ok(kept(KeptReason::Unreadable(status)));
        }
    }
    let mut values: [Option<String>; 2] = [None, None];
    for (slot, value_name) in values.iter_mut().zip(record.evidence_value_names()) {
        match reg_read_string(key, value_name) {
            Ok(value) => *slot = value,
            Err(status) => {
                ms_log::runtime_log::log_warn(format!(
                    "[windows-uninstall] could not read {key} value {} (Windows error {status}); {record:?} entry left in place",
                    value_name.unwrap_or("(Default)")
                ));
                return Ok(kept(KeptReason::Unreadable(status)));
            }
        }
    }
    let install_dir_text = install_dir.to_string_lossy();
    match record.ownership(&values, &install_dir_text) {
        RecordOwnership::Owned => reg_delete_tree_if_exists(key).map(RecordRemoval::from_deleted),
        RecordOwnership::Foreign => {
            ms_log::runtime_log::log_info(format!(
                "[windows-uninstall] {record:?} entry {key} belongs to another install ({values:?}); left in place"
            ));
            Ok(kept(KeptReason::OtherInstall))
        }
        RecordOwnership::NoEvidence => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-uninstall] {record:?} entry {key} has no install location to check against {install_dir_text}; left in place"
            ));
            Ok(kept(KeptReason::NoEvidence))
        }
    }
}

/// Removes this install's registry entries under its root (HKLM all-users, HKCU per-user):
/// the "Open with" tree, the Uninstall entry and the App Paths entry, each only when it points
/// into `install_dir` (another copy's entries stay). Every key is attempted independently: one
/// failure never skips the others, and all failures are returned together as
/// [`IntegrationError::Multiple`] (in attempt order). Keys that do not exist are not failures.
/// On success the summary lists every existing entry left in place, so a caller can tell
/// "everything of this install is gone" from "some entries were kept" (the reasons are also
/// logged). Explorer is notified once when an "Open with" or App Paths deletion was attempted.
#[cfg(target_os = "windows")]
pub fn remove_windows_registry_entries_for_install(install_dir: &Path) -> Result<RegistryRemovalSummary, IntegrationError> {
    let registry_root = windows_uninstall_registry_root(install_dir);
    // The "Open with" tree is the only key a per-user install owns; the other two are absent
    // there and therefore no-ops.
    let open_with = remove_windows_open_with_for_install(registry_root, install_dir);
    let uninstall = remove_record_if_owned(&uninstall_key(registry_root), OwnedRecord::Uninstall, install_dir);
    let app_paths = remove_record_if_owned(&app_paths_key(registry_root), OwnedRecord::AppPaths, install_dir);
    if removal_attempted_delete(&open_with) || removal_attempted_delete(&app_paths) {
        notify_shell_association_change();
    }
    summarize_removals([open_with, uninstall, app_paths])
}

/// Whether a record removal tried to delete its key. A failed deletion may still have removed
/// part of a tree, so it counts as a change (Explorer is notified).
fn removal_attempted_delete(removal: &Result<RecordRemoval, IntegrationError>) -> bool {
    !matches!(removal, Ok(RecordRemoval::Absent | RecordRemoval::Kept(_)))
}

/// Folds the per-record removals (attempt order) into the uninstall result: every failure
/// into one [`IntegrationError::Multiple`], else the summary of the entries kept in place.
fn summarize_removals<const N: usize>(removals: [Result<RecordRemoval, IntegrationError>; N]) -> Result<RegistryRemovalSummary, IntegrationError> {
    let mut summary = RegistryRemovalSummary::default();
    let mut failures = Vec::new();
    for removal in removals {
        match removal {
            Ok(RecordRemoval::Absent | RecordRemoval::Deleted) => {}
            Ok(RecordRemoval::Kept(entry)) => summary.kept.push(entry),
            Err(err) => failures.push(err),
        }
    }
    if failures.is_empty() {
        Ok(summary)
    } else {
        Err(IntegrationError::Multiple(failures))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeptReason, KeptRegistryEntry, RecordRemoval, RegistryHive, RegistryKeyPresence, classify_registry_open_status, removal_attempted_delete,
        split_registry_key, summarize_removals,
    };
    use crate::IntegrationError;

    fn kept(key: &str, reason: KeptReason) -> RecordRemoval {
        RecordRemoval::Kept(KeptRegistryEntry { key: key.to_owned(), reason })
    }

    fn delete_failure(key: &str) -> IntegrationError {
        IntegrationError::RegistryDeleteKey { key: key.to_owned(), code: 5 }
    }

    /// Kept entries are listed in attempt order with their reason; absent and deleted keys are
    /// not, so an empty `kept` means nothing of the install is left behind.
    #[test]
    fn removal_summary_lists_only_kept_entries() {
        let summary = summarize_removals([Ok(RecordRemoval::Deleted), Ok(kept(r"HKLM\U", KeptReason::OtherInstall)), Ok(kept(r"HKLM\A", KeptReason::Unreadable(5)))])
            .expect("no failure");
        assert_eq!(
            summary.kept,
            vec![
                KeptRegistryEntry { key: r"HKLM\U".to_owned(), reason: KeptReason::OtherInstall },
                KeptRegistryEntry { key: r"HKLM\A".to_owned(), reason: KeptReason::Unreadable(5) },
            ]
        );
        let clean = summarize_removals([Ok(RecordRemoval::Absent), Ok(RecordRemoval::Deleted), Ok(RecordRemoval::Absent)]).expect("no failure");
        assert!(clean.kept.is_empty());
    }

    /// Any failed deletion turns the whole result into `Multiple`, all failures in attempt
    /// order, even when other entries were kept.
    #[test]
    fn removal_failures_are_returned_together() {
        let result = summarize_removals([Err(delete_failure("KEY_FIRST")), Ok(kept("KEY_KEPT", KeptReason::NoEvidence)), Err(delete_failure("KEY_SECOND"))]);
        match result {
            Err(IntegrationError::Multiple(failures)) => {
                let keys: Vec<String> = failures.iter().map(ToString::to_string).collect();
                assert_eq!(keys.len(), 2);
                assert!(keys[0].contains("KEY_FIRST") && keys[1].contains("KEY_SECOND"), "{keys:?}");
            }
            other => panic!("expected Multiple, got {other:?}"),
        }
    }

    /// Explorer is notified after a deletion or a failed deletion, never for an absent or
    /// kept key.
    #[test]
    fn only_delete_attempts_count_as_association_changes() {
        assert!(removal_attempted_delete(&Ok(RecordRemoval::Deleted)));
        assert!(removal_attempted_delete(&Err(delete_failure("k"))));
        assert!(!removal_attempted_delete(&Ok(RecordRemoval::Absent)));
        assert!(!removal_attempted_delete(&Ok(kept("k", KeptReason::OtherInstall))));
    }

    /// Key existence is decided by the numeric status: success = present, file/path not found
    /// = absent (never an error), anything else (access denied, …) = unknown, so the deletion
    /// is still attempted and reports its own failure.
    #[test]
    fn registry_open_status_decides_presence_by_code() {
        assert_eq!(classify_registry_open_status(0), RegistryKeyPresence::Present);
        assert_eq!(classify_registry_open_status(2), RegistryKeyPresence::Absent);
        assert_eq!(classify_registry_open_status(3), RegistryKeyPresence::Absent);
        assert_eq!(classify_registry_open_status(5), RegistryKeyPresence::Unknown(5));
        assert_eq!(classify_registry_open_status(1), RegistryKeyPresence::Unknown(1));
    }

    /// The mirrored Win32 codes must equal the `windows-sys` constants they stand for.
    #[cfg(target_os = "windows")]
    #[test]
    fn mirrored_win32_codes_match_windows_sys() {
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
        use windows_sys::Win32::System::Registry::{REG_DWORD, REG_SZ};
        assert_eq!(super::WIN32_ERROR_SUCCESS, ERROR_SUCCESS);
        assert_eq!(super::WIN32_ERROR_FILE_NOT_FOUND, ERROR_FILE_NOT_FOUND);
        assert_eq!(super::WIN32_ERROR_PATH_NOT_FOUND, ERROR_PATH_NOT_FOUND);
        assert_eq!(crate::windows::values::WIN32_REG_SZ, REG_SZ);
        assert_eq!(crate::windows::values::WIN32_REG_DWORD, REG_DWORD);
    }

    /// Only the two roots this crate writes are split; anything else is rejected.
    #[test]
    fn registry_keys_split_into_hive_and_subkey() {
        assert_eq!(split_registry_key(r"HKLM\Software\X"), Some((RegistryHive::LocalMachine, r"Software\X")));
        assert_eq!(split_registry_key(r"hkcu\Software\Classes"), Some((RegistryHive::CurrentUser, r"Software\Classes")));
        assert_eq!(split_registry_key(r"HKCR\.png"), None);
        assert_eq!(split_registry_key("HKLM"), None);
        assert_eq!(split_registry_key(r"HKLM\"), None);
    }
}
