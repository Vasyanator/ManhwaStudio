/*
File: crates/ms-os-integration/src/identity.rs

Purpose:
The names every OS record of a ManhwaStudio copy is keyed on: product and publisher names,
the Windows executable name, the shortcut file name and the registry subkeys. One owner, so
the writer, the uninstaller and any later probe can never disagree on a spelling.

Key items:
- `PRODUCT_NAME`, `PUBLISHER`, `WINDOWS_EXE_NAME`, `LINUX_EXE_NAME`, `SHORTCUT_FILE_NAME`
- `UNINSTALL_SUBKEY`, `APP_PATHS_SUBKEY`
- `LINUX_OWNER_EXE_KEY` (the desktop entry's ownership key)
- `APP_ICON_PNG` (the 512 px program icon the Linux desktop entry installs)

Notes:
Pure consts, compiled on every target. Changing a value changes which records an existing
install is recognised by: an old install keeps the old names on disk and in the registry.
*/

/// Display name of the program: the Uninstall entry's `DisplayName` and the "Open with"
/// `FriendlyAppName`.
pub const PRODUCT_NAME: &str = "ManhwaStudio";

/// Publisher shown in the Windows installed-programs list (`Publisher` of the Uninstall entry).
pub const PUBLISHER: &str = "Vasyanator";

/// On-disk name of the Windows launcher executable: the file shortcuts and registry entries
/// target first, and the name of the App Paths and `Applications\…` keys.
pub const WINDOWS_EXE_NAME: &str = "manhwastudio_rs.exe";

/// File name of the Linux executable: a desktop entry whose program has another file name
/// launches something that is not ManhwaStudio (`Defect::ForeignProgram`).
pub const LINUX_EXE_NAME: &str = "manhwastudio_rs";

/// File name of every Windows shortcut the installer creates (desktop and Start menu).
pub const SHORTCUT_FILE_NAME: &str = "ManhwaStudio.lnk";

/// Uninstall entry subkey, relative to an `HKLM` / `HKCU` root.
pub const UNINSTALL_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio";

/// App Paths entry subkey, relative to an `HKLM` / `HKCU` root.
pub const APP_PATHS_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\manhwastudio_rs.exe";

/// Vendor key of the Linux desktop entry naming the executable the entry was written for: the
/// entry's ownership key (`linux::desktop_entry::OWNER_EXE_KEY`). Here, not in `linux`, because
/// the registration report names it on every target.
pub const LINUX_OWNER_EXE_KEY: &str = "X-ManhwaStudio-Exe";

/// The 512 px program icon (`app_icon_512.png` at the repository root), installed as
/// `icons/hicolor/512x512/apps/manhwastudio_rs.png` next to the Linux desktop entry. The same
/// file the binary embeds for its window icon.
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../../app_icon_512.png");

#[cfg(test)]
mod tests {
    use super::{APP_PATHS_SUBKEY, UNINSTALL_SUBKEY, WINDOWS_EXE_NAME};

    /// The App Paths key is named after the launcher executable, exactly as Windows resolves it.
    #[test]
    fn app_paths_subkey_ends_with_the_executable_name() {
        assert!(APP_PATHS_SUBKEY.ends_with(&format!(r"\App Paths\{WINDOWS_EXE_NAME}")));
        assert!(UNINSTALL_SUBKEY.ends_with(r"\Uninstall\ManhwaStudio"));
    }
}
