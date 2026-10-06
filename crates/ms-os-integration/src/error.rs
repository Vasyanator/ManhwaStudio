/*
File: crates/ms-os-integration/src/error.rs

Purpose:
`IntegrationError`, the typed failure of every OS-integration operation of this crate.

Key items:
- `IntegrationError`: one variant per failure site; `Display` is a short English text for
  logs, `user_message()` the localized text shown to the user.

Notes:
`user_message()` renders `installer.utils.*` catalog entries. Variants that existed before
the extraction render exactly the installer's old texts; the native registry variants
(`RegistryOpenKey`, `RegistrySetValue`, `RegistryDeleteKey`) render their own keys with the
system's text for the Win32 error code, and the COM shortcut variants (`ShortcutWrite`,
`ShortcutRead`) theirs with the system text and the `HRESULT`. A test pins every variant against the English catalog.
*/

use std::io;
use std::path::PathBuf;

/// Failure of an OS-integration operation (registry, shortcut, elevation, file removal).
///
/// `Display` is English and meant for logs; use [`IntegrationError::user_message`] for any
/// text a user sees.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    /// `std::env::current_exe()` failed.
    #[error("could not determine the running executable path: {source}")]
    DetermineExe { source: io::Error },
    /// `ShellExecuteW("runas")` returned an error code (UAC declined or the launch failed).
    #[error("the elevated relaunch was refused (UAC)")]
    UacDenied,
    /// No Desktop directory could be resolved for a shortcut.
    #[error("the Desktop directory could not be determined")]
    DesktopNotFound,
    /// No Start-menu Programs directory could be resolved for a shortcut.
    #[error("the Start menu Programs directory could not be determined")]
    StartMenuFolderNotFound,
    /// The directory that should hold a shortcut could not be created.
    #[error("could not create the shortcut directory '{}': {source}", parent.display())]
    CreateShortcutFolder { parent: PathBuf, source: io::Error },
    /// A COM step of writing the `.lnk` at `path` failed. `step` names the call (e.g.
    /// `IPersistFile::Save`) for the log, `code` is its `HRESULT` and `message` the system text
    /// of that `HRESULT`.
    #[error("could not write the shortcut '{}' at {step}: {message} (HRESULT 0x{code:08X})", path.display())]
    ShortcutWrite { path: PathBuf, step: &'static str, code: i32, message: String },
    /// A COM step of reading the `.lnk` at `path` failed; fields as in `ShortcutWrite`.
    #[error("could not read the shortcut '{}' at {step}: {message} (HRESULT 0x{code:08X})", path.display())]
    ShortcutRead { path: PathBuf, step: &'static str, code: i32, message: String },
    /// Neither the preferred nor the fallback launcher executable exists in the directory.
    #[error("no launcher executable in '{}'", install_dir.display())]
    LauncherExeNotFound { install_dir: PathBuf },
    /// `RegCreateKeyExW` / `RegOpenKeyExW` failed for `key` (`HKLM\…` / `HKCU\…`) with the Win32
    /// error `code` (e.g. 5 = access denied without elevation).
    #[error("could not open registry key '{key}' (Windows error {code})")]
    RegistryOpenKey { key: String, code: u32 },
    /// `RegSetValueExW` failed for a value of `key`; `value_name` `None` = the default value.
    #[error("could not write registry value {} of '{key}' (Windows error {code})", value_label(.value_name.as_deref()))]
    RegistrySetValue { key: String, value_name: Option<String>, code: u32 },
    /// `RegDeleteTreeW` / `RegDeleteKeyExW` failed for an existing `key`.
    #[error("could not delete registry key '{key}' (Windows error {code})")]
    RegistryDeleteKey { key: String, code: u32 },
    /// A directory could not be removed.
    #[error("could not remove the directory '{}': {source}", path.display())]
    RemoveFolder { path: PathBuf, source: io::Error },
    /// A file could not be removed.
    #[error("could not remove the file '{}': {source}", path.display())]
    RemoveFile { path: PathBuf, source: io::Error },
    /// Several independent operations failed; every failure is kept, in attempt order.
    #[error("{}", join_display(.0))]
    Multiple(Vec<IntegrationError>),
}

impl IntegrationError {
    /// Localized, user-facing text of this error in the active UI locale.
    ///
    /// Byte-identical to the message the installer built at the same failure site before the
    /// extraction (same `installer.utils.*` key and placeholder values). `Multiple` joins its
    /// children's messages with `\n`, in order.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::DetermineExe { source } => tf!("installer.utils.determine_exe_error", e = source),
            Self::UacDenied => t!("installer.utils.uac_denied_error").to_string(),
            Self::DesktopNotFound => t!("installer.utils.desktop_not_found_error").to_string(),
            Self::StartMenuFolderNotFound => t!("installer.utils.start_menu_folder_not_found_error").to_string(),
            Self::CreateShortcutFolder { parent, source } => {
                tf!("installer.utils.create_shortcut_folder_error", parent = parent.display(), e = source)
            }
            Self::ShortcutWrite { path, code, message, .. } => {
                tf!("installer.utils.shortcut_write_error", path = path.display(), e = hresult_text(*code, message))
            }
            Self::ShortcutRead { path, code, message, .. } => {
                tf!("installer.utils.shortcut_read_error", path = path.display(), e = hresult_text(*code, message))
            }
            Self::LauncherExeNotFound { install_dir } => {
                tf!("installer.utils.launcher_exe_not_found_error", install_dir = install_dir.display())
            }
            Self::RegistryOpenKey { key, code } => tf!("installer.utils.registry_open_key_error", key = key, e = win32_error_text(*code)),
            Self::RegistrySetValue { key, value_name: Some(value_name), code } => {
                tf!("installer.utils.registry_set_value_error", key = key, value_name = value_name, e = win32_error_text(*code))
            }
            Self::RegistrySetValue { key, value_name: None, code } => {
                tf!("installer.utils.registry_set_default_value_error", key = key, e = win32_error_text(*code))
            }
            Self::RegistryDeleteKey { key, code } => tf!("installer.utils.registry_delete_key_error", key = key, e = win32_error_text(*code)),
            Self::RemoveFolder { path, source } => tf!("installer.utils.remove_folder_error", path = path.display(), e = source),
            Self::RemoveFile { path, source } => tf!("installer.utils.remove_file_error", path = path.display(), e = source),
            Self::Multiple(errors) => errors.iter().map(Self::user_message).collect::<Vec<_>>().join("\n"),
        }
    }
}

/// System text of a Win32 error code (`FormatMessageW` through `std::io::Error`, localized on
/// Windows), ending in `(os error N)`. Win32 codes are small positive numbers; the bit-exact
/// reinterpretation keeps any value intact.
fn win32_error_text(code: u32) -> String {
    io::Error::from_raw_os_error(i32::from_ne_bytes(code.to_ne_bytes())).to_string()
}

/// User-facing text of a COM failure: the system `message` followed by the `HRESULT` in hex,
/// so a support report can be matched even when the system text is empty or localized.
fn hresult_text(code: i32, message: &str) -> String {
    format!("{message} (0x{code:08X})")
}

/// Log label of a registry value name: `'Name'`, or `(Default)` for the default value.
fn value_label(value_name: Option<&str>) -> String {
    value_name.map_or_else(|| "(Default)".to_owned(), |name| format!("'{name}'"))
}

/// `Display` of [`IntegrationError::Multiple`]: the children's log texts joined by `; `.
fn join_display(errors: &[IntegrationError]) -> String {
    errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::IntegrationError;
    use std::io;
    use std::path::PathBuf;

    fn io_error() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "denied")
    }

    /// Every variant renders the English catalog text of the installer key it replaced, with
    /// every placeholder filled: a renamed key or placeholder would leave `{…}` or the key text.
    #[test]
    fn user_messages_render_the_installer_catalog_texts() {
        let tag = ms_i18n::LocaleTag::parse("en").expect("valid embedded tag");
        ms_i18n::set_locale(&tag).expect("embedded catalog installs");
        let path = PathBuf::from("/opt/ms");
        // The platform's own text for error 5 (access denied on Windows, EIO on Linux).
        let os5 = io::Error::from_raw_os_error(5).to_string();
        let open_text = format!("could not open the registry key 'HKLM\\K': {os5}");
        let set_text = format!("could not write the registry value 'NoModify' of 'HKLM\\K': {os5}");
        let default_text = format!("could not write the default value of the registry key 'HKLM\\K': {os5}");
        let delete_text = format!("could not delete the registry key 'HKCU\\K': {os5}");
        let cases: Vec<(IntegrationError, &str)> = vec![
            (IntegrationError::DetermineExe { source: io_error() }, "could not determine the exe path: denied"),
            (IntegrationError::UacDenied, "the system denied the elevated launch (UAC)"),
            (IntegrationError::DesktopNotFound, "could not determine Desktop for shortcut creation"),
            (IntegrationError::StartMenuFolderNotFound, "could not determine the Start Menu folder for shortcut creation"),
            (IntegrationError::CreateShortcutFolder { parent: path.clone(), source: io_error() }, "could not create the shortcut folder '/opt/ms': denied"),
            (
                IntegrationError::ShortcutWrite { path: path.clone(), step: "IPersistFile::Save", code: -2147024891, message: "Access is denied.".to_owned() },
                "could not create the shortcut '/opt/ms': Access is denied. (0x80070005)",
            ),
            (
                IntegrationError::ShortcutRead { path: path.clone(), step: "IPersistFile::Load", code: -2147024894, message: String::new() },
                "could not read the shortcut '/opt/ms':  (0x80070002)",
            ),
            (IntegrationError::LauncherExeNotFound { install_dir: path.clone() }, "launcher executable not found in '/opt/ms'"),
            (IntegrationError::RegistryOpenKey { key: "HKLM\\K".to_owned(), code: 5 }, &open_text),
            (
                IntegrationError::RegistrySetValue { key: "HKLM\\K".to_owned(), value_name: Some("NoModify".to_owned()), code: 5 },
                &set_text,
            ),
            (
                IntegrationError::RegistrySetValue { key: "HKLM\\K".to_owned(), value_name: None, code: 5 },
                &default_text,
            ),
            (IntegrationError::RegistryDeleteKey { key: "HKCU\\K".to_owned(), code: 5 }, &delete_text),
            (IntegrationError::RemoveFolder { path: path.clone(), source: io_error() }, "could not remove the folder '/opt/ms': denied"),
            (IntegrationError::RemoveFile { path, source: io_error() }, "could not remove the file '/opt/ms': denied"),
            (
                IntegrationError::Multiple(vec![IntegrationError::UacDenied, IntegrationError::DesktopNotFound]),
                "the system denied the elevated launch (UAC)\ncould not determine Desktop for shortcut creation",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.user_message(), expected, "{error:?}");
        }
    }
}
