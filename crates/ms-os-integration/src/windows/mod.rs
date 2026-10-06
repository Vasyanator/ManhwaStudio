/*
File: crates/ms-os-integration/src/windows/mod.rs

Purpose:
Windows half of the OS integration: which records a program copy owns and where they live.
This file holds the install-kind rule (all-users = under Program Files) and the path helpers
the records are keyed on; the records themselves live in the submodules.

Submodules:
- `values`: pure value tables ("Open with", Uninstall + App Paths), key builders, argument
  quoting and the "Open with" ownership predicate.
- `registry`: native registry presence / reads / writes / deletes (Win32, 64-bit view) and
  the Explorer association refresh.
- `shortcut`: `ManhwaStudio.lnk` write / read (native `IShellLinkW` over COM) and removal,
  Desktop / Start-menu known folders, launcher target resolution.
- `elevation`: token elevation probe and the UAC relaunch of the running executable.
- `probe`: the read-only registration probe of every record under both roots (pure judges +
  readers).

Key functions:
- `is_windows_all_users_install_dir()`, `windows_uninstall_registry_root()`
- `normalize_windows_path()`

Notes:
The module is compiled on Windows and into host test builds; every Win32, environment or
process item is gated to `target_os = "windows"` one by one, so the pure parts are testable
on Linux. The `windows` extern crate (COM bindings) is shadowed by this module's name and is
referenced as `::windows` inside it. No child process is spawned anywhere in this module.
*/

#[cfg(target_os = "windows")]
use std::env;
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};

pub mod elevation;
pub mod probe;
pub mod registry;
pub mod shortcut;
pub mod values;

/// True when `path` is an all-users install: the same as or inside `%ProgramFiles%` or
/// `%ProgramFiles(x86)%`. Decides the registry root (HKLM vs HKCU), the Start-menu folder
/// (%ProgramData% vs %APPDATA%) and which records an install gets.
#[cfg(target_os = "windows")]
pub fn is_windows_all_users_install_dir(path: &Path) -> bool {
    is_windows_program_files_dir(path)
}

#[cfg(target_os = "windows")]
fn is_windows_program_files_dir(path: &Path) -> bool {
    const PROGRAM_FILES_ENV_VARS: &[&str] = &["ProgramFiles", "ProgramFiles(x86)"];
    let normalized_path = normalize_windows_path(path);
    PROGRAM_FILES_ENV_VARS.iter().any(|env_name| {
        env::var_os(env_name)
            .map(PathBuf::from)
            .map(|root| windows_path_is_same_or_child_of(&normalized_path, &root))
            .unwrap_or(false)
    })
}

/// True when `normalized_path` (already [`normalize_windows_path`]ed) equals `candidate_root`
/// or lies below it on a `\` boundary (`C:\Program Files2` is not inside `C:\Program Files`).
#[cfg(target_os = "windows")]
fn windows_path_is_same_or_child_of(normalized_path: &str, candidate_root: &Path) -> bool {
    let normalized_root = normalize_windows_path(candidate_root);
    normalized_path == normalized_root
        || normalized_path
            .strip_prefix(&normalized_root)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

/// Comparison form of a Windows path: `/` turned into `\`, trailing `\` removed, ASCII
/// lowercase. Lexical only (no canonicalization, no drive or UNC resolution).
#[cfg(target_os = "windows")]
pub fn normalize_windows_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// Registry root of an install's records: `"HKLM"` for an all-users install, `"HKCU"` for a
/// per-user one ([`is_windows_all_users_install_dir`]).
#[cfg(target_os = "windows")]
pub fn windows_uninstall_registry_root(install_dir: &Path) -> &'static str {
    if is_windows_all_users_install_dir(install_dir) {
        "HKLM"
    } else {
        "HKCU"
    }
}

/// NUL-terminated UTF-16 copy of `text` for Win32 `*W` calls. Pure (also built into host
/// test builds, where its test runs).
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::to_wide;

    /// Win32 wide strings are UTF-16 code units with exactly one trailing NUL; non-BMP text
    /// becomes a surrogate pair.
    #[test]
    fn wide_strings_are_nul_terminated_utf16() {
        assert_eq!(to_wide(""), vec![0]);
        assert_eq!(to_wide(r"HKLM\Вя"), vec![0x48, 0x4B, 0x4C, 0x4D, 0x5C, 0x0412, 0x044F, 0]);
        assert_eq!(to_wide("😀"), vec![0xD83D, 0xDE00, 0]);
    }
}
