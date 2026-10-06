/*
File: crates/ms-os-integration/src/windows/values.rs

Purpose:
Pure data of the Windows registry records: key names, the exact value tables the installer
writes, their Win32 value encoding, command-line quoting, and the ownership predicates that
decide whether uninstall may delete a record.

Key structures:
- `RegistryStringValue`, `RegistryDwordValue`, `RegistryValue`, `RegistryValueType`
- `RecordOwnership`

Key functions:
- `windows_open_with_app_key()`, `windows_open_with_registry_values()`
- `uninstall_key()`, `app_paths_key()`, `uninstall_and_app_paths_values()`
- `open_with_command_targets_install()`, `uninstall_entry_ownership()`,
  `app_paths_entry_ownership()`, `quote_windows_arg()`
- `RegistryValue::encode()`, `encode_reg_sz()`

Notes:
No I/O and no Win32: everything here takes and returns strings, so the tables are pinned by
golden tests on every host. The writers in `registry.rs` only iterate these tables.
*/

use crate::identity::{APP_PATHS_SUBKEY, PRODUCT_NAME, PUBLISHER, UNINSTALL_SUBKEY, WINDOWS_EXE_NAME};

/// Executable name under `Software\Classes\Applications\` that carries the "Open with" entry.
/// It is the on-disk name of the launcher (the file `resolve_windows_launcher_target`
/// prefers), so Explorer ties the entry to that binary.
/// The key is fixed per registry root, so two installs under the same root share it: the last
/// install wins, and uninstalling one removes the entry only if its command points into that
/// install's directory (`registry::remove_windows_open_with_for_install`). Accepted debt, see
/// this directory's `MODULE_README.md`.
pub const WINDOWS_OPEN_WITH_APP_NAME: &str = WINDOWS_EXE_NAME;

/// One `REG_SZ` value to write: full key path (with its `HKLM` / `HKCU` root), value name
/// (`None` = the key's default value) and data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryStringValue {
    /// Full key path including its `HKLM` / `HKCU` root.
    pub key: String,
    /// Value name; `None` is the key's default (`(Default)`) value.
    pub value_name: Option<String>,
    /// String data, written as `REG_SZ`.
    pub data: String,
}

/// One named `REG_DWORD` value to write: full key path (with its root), value name and data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDwordValue {
    /// Full key path including its `HKLM` / `HKCU` root.
    pub key: String,
    /// Value name (DWORD values are never the default value here).
    pub value_name: String,
    /// Numeric data, written as `REG_DWORD` (little-endian).
    pub data: u32,
}

/// One registry value of a record table, in the type it is written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValue {
    /// A `REG_SZ` value.
    String(RegistryStringValue),
    /// A `REG_DWORD` value.
    Dword(RegistryDwordValue),
}

/// Win32 registry value type of a written value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryValueType {
    /// `REG_SZ`: NUL-terminated UTF-16LE string.
    String,
    /// `REG_DWORD`: 32-bit little-endian number.
    Dword,
}

/// `REG_SZ` / `REG_DWORD` from `winnt.h`, mirrored so the encoding is testable off Windows; a
/// Windows-only test in `registry.rs` pins them to the `windows-sys` constants.
pub(crate) const WIN32_REG_SZ: u32 = 1;
pub(crate) const WIN32_REG_DWORD: u32 = 4;

impl RegistryValueType {
    /// The Win32 `REG_*` type code `RegSetValueExW` takes.
    #[must_use]
    pub fn win32_code(self) -> u32 {
        match self {
            Self::String => WIN32_REG_SZ,
            Self::Dword => WIN32_REG_DWORD,
        }
    }
}

impl RegistryValue {
    /// Full key path (with its root) the value lives in.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::String(value) => &value.key,
            Self::Dword(value) => &value.key,
        }
    }

    /// Value name; `None` = the key's default value.
    #[must_use]
    pub fn value_name(&self) -> Option<&str> {
        match self {
            Self::String(value) => value.value_name.as_deref(),
            Self::Dword(value) => Some(&value.value_name),
        }
    }

    /// Type and exact byte payload `RegSetValueExW` writes: `REG_SZ` as UTF-16LE with its
    /// terminating NUL ([`encode_reg_sz`]), `REG_DWORD` as 4 little-endian bytes.
    #[must_use]
    pub fn encode(&self) -> (RegistryValueType, Vec<u8>) {
        match self {
            Self::String(value) => (RegistryValueType::String, encode_reg_sz(&value.data)),
            Self::Dword(value) => (RegistryValueType::Dword, value.data.to_le_bytes().to_vec()),
        }
    }
}

/// `REG_SZ` payload of `text`: UTF-16LE code units followed by one NUL unit (the byte count
/// passed to `RegSetValueExW` includes it, as the API requires).
#[must_use]
pub fn encode_reg_sz(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

/// Whether a registry record found at uninstall time belongs to the uninstalled directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOwnership {
    /// Every identifying value present points into the install directory: delete it.
    Owned,
    /// At least one identifying value points elsewhere (another copy): leave it.
    Foreign,
    /// No identifying value is present (or all are blank): ownership unprovable, leave it.
    NoEvidence,
}

/// The `Applications\manhwastudio_rs.exe` key under `registry_root` (`"HKLM"` or `"HKCU"`):
/// the whole tree the "Open with" registration owns and uninstall deletes.
#[must_use]
pub fn windows_open_with_app_key(registry_root: &str) -> String {
    format!(r"{registry_root}\Software\Classes\Applications\{WINDOWS_OPEN_WITH_APP_NAME}")
}

/// The Uninstall entry key under `registry_root` (`"HKLM"` or `"HKCU"`).
#[must_use]
pub fn uninstall_key(registry_root: &str) -> String {
    format!(r"{registry_root}\{UNINSTALL_SUBKEY}")
}

/// The App Paths entry key under `registry_root` (`"HKLM"` or `"HKCU"`).
#[must_use]
pub fn app_paths_key(registry_root: &str) -> String {
    format!(r"{registry_root}\{APP_PATHS_SUBKEY}")
}

/// Pure list of values that make ManhwaStudio appear in Explorer's "Open with" list for every
/// readable single-image input type (`ms_config::single_image::input_extensions`).
///
/// Writes ONLY under `Applications\manhwastudio_rs.exe`: the open command `"<launcher>" "%1"`
/// (the positional image path is the CLI contract), `FriendlyAppName`, and one empty
/// `SupportedTypes\.<ext>` value per extension. No ProgID, no `OpenWithProgids`, no default
/// handler: no extension is ever taken over.
#[must_use]
pub fn windows_open_with_registry_values(
    registry_root: &str,
    launcher_path: &str,
) -> Vec<RegistryStringValue> {
    let app_key = windows_open_with_app_key(registry_root);
    let mut values = vec![
        RegistryStringValue {
            key: app_key.clone(),
            value_name: Some("FriendlyAppName".to_owned()),
            data: PRODUCT_NAME.to_owned(),
        },
        RegistryStringValue {
            key: format!(r"{app_key}\shell\open\command"),
            value_name: None,
            data: format!("{} \"%1\"", quote_windows_arg(launcher_path)),
        },
    ];
    let supported_types_key = format!(r"{app_key}\SupportedTypes");
    values.extend(ms_config::single_image::input_extensions().map(|extension| RegistryStringValue {
        key: supported_types_key.clone(),
        value_name: Some(format!(".{extension}")),
        data: String::new(),
    }));
    values
}

/// Pure list of the App Paths and Uninstall values of an all-users install, in write order.
///
/// App Paths: default = `launcher_path`, `Path` = `install_dir`. Uninstall: `DisplayName`,
/// `Publisher`, `DisplayVersion` = `display_version` (the installed program's
/// `version_core`, e.g. `3.2.1`), `InstallLocation` = `install_dir`, `DisplayIcon` =
/// `launcher_path`, `UninstallString` and `QuietUninstallString` = `"<launcher>" --uninstall`
/// (the same command: the uninstaller has no quiet mode), then the DWORDs `NoModify` = 1 and
/// `NoRepair` = 1. `registry_root` is `"HKLM"` or `"HKCU"`.
#[must_use]
pub fn uninstall_and_app_paths_values(registry_root: &str, install_dir: &str, launcher_path: &str, display_version: &str) -> Vec<RegistryValue> {
    let app_path_key = app_paths_key(registry_root);
    let uninstall_key = uninstall_key(registry_root);
    let string_value = |key: &str, value_name: Option<&str>, data: &str| {
        RegistryValue::String(RegistryStringValue { key: key.to_owned(), value_name: value_name.map(str::to_owned), data: data.to_owned() })
    };
    let dword_value = |key: &str, value_name: &str, data: u32| {
        RegistryValue::Dword(RegistryDwordValue { key: key.to_owned(), value_name: value_name.to_owned(), data })
    };
    let uninstall_command = format!("{} --uninstall", quote_windows_arg(launcher_path));
    vec![
        string_value(&app_path_key, None, launcher_path),
        string_value(&app_path_key, Some("Path"), install_dir),
        string_value(&uninstall_key, Some("DisplayName"), PRODUCT_NAME),
        string_value(&uninstall_key, Some("Publisher"), PUBLISHER),
        string_value(&uninstall_key, Some("DisplayVersion"), display_version),
        string_value(&uninstall_key, Some("InstallLocation"), install_dir),
        string_value(&uninstall_key, Some("DisplayIcon"), launcher_path),
        string_value(&uninstall_key, Some("UninstallString"), &uninstall_command),
        string_value(&uninstall_key, Some("QuietUninstallString"), &uninstall_command),
        dword_value(&uninstall_key, "NoModify", 1),
        dword_value(&uninstall_key, "NoRepair", 1),
    ]
}

/// True when the open `command` (`"<exe>" "%1"` as written by
/// [`windows_open_with_registry_values`], or an unquoted `<exe> …`) launches an executable
/// located directly in `install_dir`. Windows paths compare case-insensitively, with `/` and
/// `\` equivalent and trailing separators ignored. Pure string logic, so it is testable on
/// every target (a Linux `Path` would not split on `\`).
#[must_use]
pub fn open_with_command_targets_install(command: &str, install_dir: &str) -> bool {
    exe_is_directly_in_dir(command_executable(command), install_dir)
}

/// The executable of a Windows command line: the text inside a leading `"…"` (up to the
/// closing quote or the end), else the first whitespace-separated token. Leading whitespace
/// is skipped.
pub(crate) fn command_executable(command: &str) -> &str {
    let command = command.trim_start();
    match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default(),
        None => command.split_whitespace().next().unwrap_or_default(),
    }
}

/// Comparison form of a Windows path string: `/` turned into `\`, trailing `\` removed,
/// Unicode lowercase. Lexical only.
pub(crate) fn normalize_windows_path_text(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// True when `exe` (a path, not a command line) names a file located directly in
/// `install_dir` (not in a subdirectory), compared by [`normalize_windows_path_text`].
fn exe_is_directly_in_dir(exe: &str, install_dir: &str) -> bool {
    let exe = normalize_windows_path_text(exe);
    let Some((exe_dir, exe_name)) = exe.rsplit_once('\\') else {
        return false;
    };
    let install_dir = normalize_windows_path_text(install_dir);
    !exe_name.is_empty() && !install_dir.is_empty() && exe_dir == install_dir
}

/// True when the directory `dir` is `install_dir` itself, compared by
/// [`normalize_windows_path_text`]; an empty side never matches.
fn dir_is_install_dir(dir: &str, install_dir: &str) -> bool {
    let dir = normalize_windows_path_text(dir);
    let install_dir = normalize_windows_path_text(install_dir);
    !dir.is_empty() && dir == install_dir
}

/// Trims whitespace and one pair of surrounding `"` (a path value some writers quote); `None`
/// for an absent or blank value.
pub(crate) fn present_path_value(value: Option<&str>) -> Option<&str> {
    let value = value?.trim();
    let value = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(value).trim();
    (!value.is_empty()).then_some(value)
}

/// Combines per-value verdicts (`None` = value absent): no verdict -> `NoEvidence`, every
/// verdict true -> `Owned`, any false -> `Foreign`.
fn combine_ownership_evidence(verdicts: &[Option<bool>]) -> RecordOwnership {
    let mut present = verdicts.iter().flatten().peekable();
    if present.peek().is_none() {
        return RecordOwnership::NoEvidence;
    }
    if present.all(|points_into_install| *points_into_install) {
        RecordOwnership::Owned
    } else {
        RecordOwnership::Foreign
    }
}

/// Ownership of an `Uninstall\ManhwaStudio` entry by the install at `install_dir`, from its
/// `InstallLocation` (must equal the directory) and `UninstallString` (its executable must lie
/// directly in the directory). Every present, non-blank value must agree (see
/// [`RecordOwnership`]); the values written by [`uninstall_and_app_paths_values`] are `Owned`.
#[must_use]
pub fn uninstall_entry_ownership(install_location: Option<&str>, uninstall_string: Option<&str>, install_dir: &str) -> RecordOwnership {
    combine_ownership_evidence(&[
        present_path_value(install_location).map(|dir| dir_is_install_dir(dir, install_dir)),
        uninstall_string
            .filter(|command| !command.trim().is_empty())
            .map(|command| exe_is_directly_in_dir(command_executable(command), install_dir)),
    ])
}

/// Ownership of an `App Paths\manhwastudio_rs.exe` entry by the install at `install_dir`, from
/// its default value (the launcher path, which must lie directly in the directory) and its
/// `Path` value (must equal the directory). Same combination rule as
/// [`uninstall_entry_ownership`].
#[must_use]
pub fn app_paths_entry_ownership(launcher_path: Option<&str>, path_value: Option<&str>, install_dir: &str) -> RecordOwnership {
    combine_ownership_evidence(&[
        present_path_value(launcher_path).map(|exe| exe_is_directly_in_dir(exe, install_dir)),
        present_path_value(path_value).map(|dir| dir_is_install_dir(dir, install_dir)),
    ])
}

/// Wraps `text` in double quotes for a Windows command line, backslash-escaping embedded `"`.
/// Backslashes (including a trailing one) are left as they are.
#[must_use]
pub fn quote_windows_arg(text: &str) -> String {
    let escaped = text.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::{
        RecordOwnership, RegistryDwordValue, RegistryStringValue, RegistryValue, RegistryValueType, app_paths_entry_ownership, encode_reg_sz,
        open_with_command_targets_install, quote_windows_arg, uninstall_and_app_paths_values, uninstall_entry_ownership,
        windows_open_with_app_key, windows_open_with_registry_values,
    };

    /// Every readable input extension must be offered in the "Open with" `SupportedTypes`, each
    /// as an empty `REG_SZ` named `.<ext>`, and nothing outside the owned app key is written.
    #[test]
    fn windows_open_with_registers_every_input_extension() {
        let values = windows_open_with_registry_values("HKLM", r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe");
        let supported_key = r"HKLM\Software\Classes\Applications\manhwastudio_rs.exe\SupportedTypes";
        let supported: Vec<&str> = values
            .iter()
            .filter(|value| value.key == supported_key)
            .map(|value| {
                assert_eq!(value.data, "", "SupportedTypes values carry no data");
                value.value_name.as_deref().expect("SupportedTypes values are named")
            })
            .collect();
        let expected: Vec<String> = ms_config::single_image::input_extensions().map(|extension| format!(".{extension}")).collect();
        assert_eq!(supported, expected);
        for required in [".png", ".jpg", ".jpeg", ".webp", ".tif", ".tiff", ".qoi"] {
            assert!(supported.contains(&required), "missing {required}");
        }
        let app_key = windows_open_with_app_key("HKLM");
        assert!(values.iter().all(|value| value.key.starts_with(&app_key)));
        assert_eq!(values.len(), expected.len() + 2);
    }

    /// The open command quotes the launcher path (spaces in `Program Files`) and passes the
    /// file as one quoted positional argument; `FriendlyAppName` names the product.
    #[test]
    fn windows_open_with_command_quotes_paths_with_spaces() {
        let launcher = r"C:\Program Files\Manhwa Studio\manhwastudio_rs.exe";
        let values = windows_open_with_registry_values("HKCU", launcher);
        let command = values
            .iter()
            .find(|value| value.key == r"HKCU\Software\Classes\Applications\manhwastudio_rs.exe\shell\open\command")
            .expect("open command is registered");
        assert_eq!(command.value_name, None, "the command is the key's default value");
        assert_eq!(command.data, r#""C:\Program Files\Manhwa Studio\manhwastudio_rs.exe" "%1""#);
        let friendly = values
            .iter()
            .find(|value| value.value_name.as_deref() == Some("FriendlyAppName"))
            .expect("friendly name is registered");
        assert_eq!(friendly.key, r"HKCU\Software\Classes\Applications\manhwastudio_rs.exe");
        assert_eq!(friendly.data, "ManhwaStudio");
    }

    /// All-users installs write under HKLM, per-user installs under HKCU, and the removal key
    /// is exactly the tree every registered value lives in.
    #[test]
    fn windows_open_with_uses_the_given_registry_root() {
        for root in ["HKLM", "HKCU"] {
            let app_key = windows_open_with_app_key(root);
            assert_eq!(app_key, format!(r"{root}\Software\Classes\Applications\manhwastudio_rs.exe"));
            let values = windows_open_with_registry_values(root, r"D:\Apps\manhwastudio_rs.exe");
            assert!(values.iter().all(|value| value.key.starts_with(&format!("{root}\\"))));
            assert!(values.iter().all(|value| value.key == app_key || value.key.starts_with(&format!("{app_key}\\"))));
            assert!(values.iter().all(|value| !value.key.contains("OpenWithProgids") && !value.key.contains("FileExts")));
        }
    }

    /// Uninstall removes the shared "Open with" tree only when its command launches an exe
    /// directly inside THIS install directory (case-insensitive, separator-agnostic), and the
    /// command written at install time always matches its own install.
    #[test]
    fn open_with_command_ownership_matches_only_this_install() {
        let install = r"C:\Program Files\ManhwaStudio";
        assert!(open_with_command_targets_install(r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe" "%1""#, install));
        assert!(open_with_command_targets_install(r#""c:/program files/manhwastudio/MANHWASTUDIO_RS.EXE" "%1""#, r"C:\Program Files\ManhwaStudio\"));
        assert!(open_with_command_targets_install(r"C:\Apps\MS\manhwastudio_rs.exe %1", r"C:\Apps\MS"));
        assert!(open_with_command_targets_install(r#""C:\Users\Вася\ManhwaStudio\manhwastudio_rs.exe" "%1""#, r"C:\Users\вася\ManhwaStudio"));
        assert!(!open_with_command_targets_install(r#""D:\Other\ManhwaStudio\manhwastudio_rs.exe" "%1""#, install));
        assert!(!open_with_command_targets_install(r#""C:\Program Files\ManhwaStudio\sub\manhwastudio_rs.exe" "%1""#, install));
        assert!(!open_with_command_targets_install(r#""C:\Program Files\ManhwaStudio2\manhwastudio_rs.exe" "%1""#, install));
        assert!(!open_with_command_targets_install("", install));
        assert!(!open_with_command_targets_install(r#""manhwastudio_rs.exe" "%1""#, install));
        assert!(!open_with_command_targets_install(r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe" "%1""#, ""));

        let launcher = r"C:\Program Files\Manhwa Studio\manhwastudio_rs.exe";
        let command = windows_open_with_registry_values("HKLM", launcher)
            .into_iter()
            .find(|value| value.key.ends_with(r"\shell\open\command"))
            .expect("open command is registered")
            .data;
        assert!(open_with_command_targets_install(&command, r"C:\Program Files\Manhwa Studio"));
    }

    /// Full golden of the "Open with" value table (keys, names, data and ORDER), pinned before
    /// the OS-integration extraction so the moved builder provably writes the same values.
    #[test]
    fn windows_open_with_registry_values_golden() {
        let app_key = r"HKLM\Software\Classes\Applications\manhwastudio_rs.exe";
        let supported_key = r"HKLM\Software\Classes\Applications\manhwastudio_rs.exe\SupportedTypes";
        let mut expected = vec![
            RegistryStringValue { key: app_key.to_owned(), value_name: Some("FriendlyAppName".to_owned()), data: "ManhwaStudio".to_owned() },
            RegistryStringValue {
                key: r"HKLM\Software\Classes\Applications\manhwastudio_rs.exe\shell\open\command".to_owned(),
                value_name: None,
                data: r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe" "%1""#.to_owned(),
            },
        ];
        for extension in [".png", ".jpg", ".jpeg", ".jpe", ".webp", ".bmp", ".tif", ".tiff", ".gif", ".tga", ".qoi"] {
            expected.push(RegistryStringValue { key: supported_key.to_owned(), value_name: Some(extension.to_owned()), data: String::new() });
        }
        let values = windows_open_with_registry_values("HKLM", r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe");
        assert_eq!(values, expected);
    }

    /// Golden of the Windows argument quoting: wraps in `"`, backslash-escapes embedded `"`,
    /// and leaves backslashes (including a trailing one) untouched.
    #[test]
    fn quote_windows_arg_golden() {
        assert_eq!(quote_windows_arg(r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe"), r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe""#);
        assert_eq!(quote_windows_arg(""), r#""""#);
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_windows_arg(r"C:\dir\"), r#""C:\dir\""#);
        assert_eq!(quote_windows_arg(r"C:\Users\Вася\ms.exe"), r#""C:\Users\Вася\ms.exe""#);
    }

    /// Edge cases of the "Open with" ownership predicate: unquoted commands, trailing separators
    /// on either side, case folding, leading whitespace, an unterminated quote.
    #[test]
    fn open_with_command_ownership_edge_cases() {
        // Unquoted command: the exe is the first whitespace-separated token.
        assert!(open_with_command_targets_install(r"C:\Apps\MS\manhwastudio_rs.exe", r"C:\Apps\MS"));
        assert!(open_with_command_targets_install(r#"C:\Apps\MS\manhwastudio_rs.exe "%1""#, r"C:\Apps\MS"));
        // An unquoted path with a space is cut at the space, so it no longer matches its dir.
        assert!(!open_with_command_targets_install(r"C:\Program Files\MS\manhwastudio_rs.exe %1", r"C:\Program Files\MS"));
        // Trailing separators on the install dir (one or several, either slash) are ignored.
        assert!(open_with_command_targets_install(r#""C:\Apps\MS\ms.exe" "%1""#, r"C:\Apps\MS\"));
        assert!(open_with_command_targets_install(r#""C:\Apps\MS\ms.exe" "%1""#, r"C:\Apps\MS\\\"));
        assert!(open_with_command_targets_install(r#""C:\Apps\MS\ms.exe" "%1""#, "C:/Apps/MS/"));
        // A trailing separator on the command path is trimmed, so the last dir becomes the "exe".
        assert!(!open_with_command_targets_install(r#""C:\Apps\MS\" "%1""#, r"C:\Apps\MS"));
        assert!(open_with_command_targets_install(r#""C:\Apps\MS\" "%1""#, r"C:\Apps"));
        // Case folding on both sides, including non-ASCII.
        assert!(open_with_command_targets_install(r#""C:\APPS\ÄÖ\MS.EXE" "%1""#, r"c:\apps\äö"));
        // Leading whitespace before the command is skipped.
        assert!(open_with_command_targets_install(r#"   "C:\Apps\MS\ms.exe" "%1""#, r"C:\Apps\MS"));
        // An unterminated quote still yields the path up to the end of the string.
        assert!(open_with_command_targets_install(r#""C:\Apps\MS\ms.exe"#, r"C:\Apps\MS"));
        // A bare drive root as install dir normalizes to `c:` and matches an exe in the root.
        assert!(open_with_command_targets_install(r#""C:\ms.exe" "%1""#, r"C:\"));
        // Whitespace-only command has no exe.
        assert!(!open_with_command_targets_install("   ", r"C:\Apps\MS"));
    }

    /// Golden of the App Paths + Uninstall table: the values of `ms-installer`'s former inline
    /// `reg add` sequence (same keys, names, data, types and ORDER) plus `DisplayVersion` =
    /// the version core, written right after `Publisher`.
    #[test]
    fn uninstall_and_app_paths_values_golden() {
        let install = r"C:\Program Files\ManhwaStudio";
        let launcher = r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe";
        let app_paths = r"HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\manhwastudio_rs.exe";
        let uninstall = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio";
        let sz = |key: &str, name: Option<&str>, data: &str| {
            RegistryValue::String(RegistryStringValue { key: key.to_owned(), value_name: name.map(str::to_owned), data: data.to_owned() })
        };
        let dword = |name: &str| RegistryValue::Dword(RegistryDwordValue { key: uninstall.to_owned(), value_name: name.to_owned(), data: 1 });
        let command = r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe" --uninstall"#;
        let expected = vec![
            sz(app_paths, None, launcher),
            sz(app_paths, Some("Path"), install),
            sz(uninstall, Some("DisplayName"), "ManhwaStudio"),
            sz(uninstall, Some("Publisher"), "Vasyanator"),
            sz(uninstall, Some("DisplayVersion"), "3.2.1"),
            sz(uninstall, Some("InstallLocation"), install),
            sz(uninstall, Some("DisplayIcon"), launcher),
            sz(uninstall, Some("UninstallString"), command),
            sz(uninstall, Some("QuietUninstallString"), command),
            dword("NoModify"),
            dword("NoRepair"),
        ];
        assert_eq!(uninstall_and_app_paths_values("HKLM", install, launcher, "3.2.1"), expected);
        let per_user = uninstall_and_app_paths_values("HKCU", install, launcher, "3.2.1");
        assert!(per_user.iter().all(|value| match value {
            RegistryValue::String(value) => value.key.starts_with(r"HKCU\"),
            RegistryValue::Dword(value) => value.key.starts_with(r"HKCU\"),
        }));
    }

    /// `REG_SZ` payloads are UTF-16LE with exactly one terminating NUL unit (non-BMP text as a
    /// surrogate pair); `REG_DWORD` payloads are 4 little-endian bytes; type codes match
    /// `winnt.h`.
    #[test]
    fn registry_values_encode_to_win32_payloads() {
        assert_eq!(encode_reg_sz(""), vec![0, 0]);
        assert_eq!(encode_reg_sz("A"), vec![0x41, 0, 0, 0]);
        assert_eq!(encode_reg_sz("Вя"), vec![0x12, 0x04, 0x4F, 0x04, 0, 0]);
        assert_eq!(encode_reg_sz("😀"), vec![0x3D, 0xD8, 0x00, 0xDE, 0, 0]);
        let string = RegistryValue::String(RegistryStringValue { key: r"HKLM\K".to_owned(), value_name: None, data: "1.2".to_owned() });
        assert_eq!(string.encode(), (RegistryValueType::String, vec![0x31, 0, 0x2E, 0, 0x32, 0, 0, 0]));
        assert_eq!(string.key(), r"HKLM\K");
        assert_eq!(string.value_name(), None);
        let dword = RegistryValue::Dword(RegistryDwordValue { key: r"HKLM\K".to_owned(), value_name: "NoModify".to_owned(), data: 0x0102_0304 });
        assert_eq!(dword.encode(), (RegistryValueType::Dword, vec![0x04, 0x03, 0x02, 0x01]));
        assert_eq!(dword.value_name(), Some("NoModify"));
        assert_eq!(RegistryValueType::String.win32_code(), 1);
        assert_eq!(RegistryValueType::Dword.win32_code(), 4);
    }

    /// The Uninstall entry is deleted only when every identifying value present points into the
    /// uninstalled directory; the values the installer writes are always `Owned`.
    #[test]
    fn uninstall_entry_ownership_requires_every_value_to_point_into_the_install() {
        let install = r"C:\Program Files\ManhwaStudio";
        let command = r#""C:\Program Files\ManhwaStudio\manhwastudio_rs.exe" --uninstall"#;
        assert_eq!(uninstall_entry_ownership(Some(install), Some(command), install), RecordOwnership::Owned);
        // Case, separators and a trailing separator are ignored; a quoted location is accepted.
        assert_eq!(uninstall_entry_ownership(Some(r#""c:/program files/manhwastudio/""#), None, install), RecordOwnership::Owned);
        assert_eq!(uninstall_entry_ownership(None, Some(command), install), RecordOwnership::Owned);
        // Another copy, a subdirectory, a sibling with a common prefix.
        assert_eq!(uninstall_entry_ownership(Some(r"D:\Other\ManhwaStudio"), Some(r#""D:\Other\ManhwaStudio\manhwastudio_rs.exe" --uninstall"#), install), RecordOwnership::Foreign);
        assert_eq!(uninstall_entry_ownership(Some(r"C:\Program Files\ManhwaStudio2"), None, install), RecordOwnership::Foreign);
        assert_eq!(uninstall_entry_ownership(None, Some(r#""C:\Program Files\ManhwaStudio\sub\ms.exe" --uninstall"#), install), RecordOwnership::Foreign);
        // Contradicting values are never owned.
        assert_eq!(uninstall_entry_ownership(Some(install), Some(r#""D:\Other\ms.exe" --uninstall"#), install), RecordOwnership::Foreign);
        // Nothing to judge by.
        assert_eq!(uninstall_entry_ownership(None, None, install), RecordOwnership::NoEvidence);
        assert_eq!(uninstall_entry_ownership(Some("  "), Some(""), install), RecordOwnership::NoEvidence);
        // An empty install dir owns nothing.
        assert_eq!(uninstall_entry_ownership(Some(install), None, ""), RecordOwnership::Foreign);
    }

    /// App Paths is deleted only when its launcher lies directly in the uninstalled directory and
    /// its `Path` (when present) is that directory; unquoted paths with spaces are paths, not
    /// command lines.
    #[test]
    fn app_paths_entry_ownership_requires_every_value_to_point_into_the_install() {
        let install = r"C:\Program Files\ManhwaStudio";
        let launcher = r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe";
        assert_eq!(app_paths_entry_ownership(Some(launcher), Some(install), install), RecordOwnership::Owned);
        assert_eq!(app_paths_entry_ownership(Some(&format!("\"{launcher}\"")), None, install), RecordOwnership::Owned);
        assert_eq!(app_paths_entry_ownership(None, Some(r"C:\PROGRAM FILES\ManhwaStudio\"), install), RecordOwnership::Owned);
        assert_eq!(app_paths_entry_ownership(Some(r"D:\Apps\manhwastudio_rs.exe"), Some(r"D:\Apps"), install), RecordOwnership::Foreign);
        assert_eq!(app_paths_entry_ownership(Some(launcher), Some(r"D:\Apps"), install), RecordOwnership::Foreign);
        assert_eq!(app_paths_entry_ownership(None, None, install), RecordOwnership::NoEvidence);
    }

    /// Round trip: every App Paths / Uninstall value the installer writes identifies the same
    /// install as owner, for both registry roots.
    #[test]
    fn written_records_are_owned_by_their_install() {
        let install = r"C:\Program Files\Manhwa Studio";
        let launcher = r"C:\Program Files\Manhwa Studio\manhwastudio_rs.exe";
        let values = uninstall_and_app_paths_values("HKLM", install, launcher, "1.0.0");
        let find = |name: Option<&str>, key_suffix: &str| {
            values
                .iter()
                .find(|value| value.key().ends_with(key_suffix) && value.value_name() == name)
                .and_then(|value| match value {
                    RegistryValue::String(value) => Some(value.data.clone()),
                    RegistryValue::Dword(_) => None,
                })
        };
        let location = find(Some("InstallLocation"), r"\Uninstall\ManhwaStudio");
        let command = find(Some("UninstallString"), r"\Uninstall\ManhwaStudio");
        assert_eq!(uninstall_entry_ownership(location.as_deref(), command.as_deref(), install), RecordOwnership::Owned);
        let default = find(None, r"\App Paths\manhwastudio_rs.exe");
        let path = find(Some("Path"), r"\App Paths\manhwastudio_rs.exe");
        assert_eq!(app_paths_entry_ownership(default.as_deref(), path.as_deref(), install), RecordOwnership::Owned);
        assert_eq!(app_paths_entry_ownership(default.as_deref(), path.as_deref(), r"C:\Program Files\Other"), RecordOwnership::Foreign);
    }
}
