/*
File: crates/ms-os-integration/src/linux/mod.rs

Purpose:
Linux half of the OS integration: the per-user desktop entry
(`manhwastudio_rs.desktop`) and its 512 px icon that put a program copy into application
menus and "Open with" lists.

Submodules:
- `desktop_entry`: pure text of the entry (`linux_desktop_entry_text`, keyed on a
  `CopyIdentity`), the reading of an existing entry's owner (`entry_owner`) and the startup
  decision (`decide_startup` -> `StartupOutcome`).
- `probe`: the read-only registration probe of the entry in `$XDG_DATA_HOME` and every
  `$XDG_DATA_DIRS` entry (pure judge + reader).
- `xdg`: `DesktopDirs` (`$XDG_DATA_HOME` / `$XDG_DATA_DIRS`), the startup writer
  `ensure_at_startup` and the `update-desktop-database` refresh (filesystem and process I/O).

Notes:
The module is compiled on Linux and into native host test builds; the I/O items of `xdg` are
gated to `target_os = "linux"`, so the pure text and its goldens are testable on every host.
*/

pub mod desktop_entry;
pub mod probe;
#[cfg(target_os = "linux")]
pub mod xdg;
