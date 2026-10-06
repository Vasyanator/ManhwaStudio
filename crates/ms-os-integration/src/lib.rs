/*
File: crates/ms-os-integration/src/lib.rs

Purpose:
Crate root of `ms-os-integration`: the one owner of the OS records that register a
ManhwaStudio program copy with the operating system (names, expected values, writes and
deletes).

Modules:
- `identity`: product, publisher, executable, shortcut and registry-key names (pure consts).
- `copy_identity`: `CopyIdentity`, the program copy a record is written for or judged against.
- `error`: `IntegrationError`, the typed failure of every OS operation, with
  `user_message()` rendering the installer's localized texts.
- `windows`: the Windows records — "Open with", Uninstall and App Paths registry values,
  `ManhwaStudio.lnk` shortcuts, elevation probe and UAC relaunch.
- `linux`: the Linux records — the per-user `manhwastudio_rs.desktop` entry and its icon, and
  the startup policy that keeps them current without taking over another copy's entry.
- `report`: the registration report (`RecordStatus`, `Defect`, the broken/stale split,
  `badge_worthy`) and the read-only `probe()` over both platform readers.
- `actions`: what may be done with a probed record (`allowed_actions`, `requires_elevation`),
  the in-process executor `apply()`, and the elevated helper's protocol (action tokens, result
  file); the elevated launch itself is `windows::elevation::apply_elevated`.

Notes:
GUI-free and blocking: every I/O function runs on the caller's worker thread, and progress or
console reporting stays with the caller. The pure value builders of the `windows` and `linux`
modules are also compiled into native host test builds, so their goldens run on every host.
*/

#![warn(clippy::all)]

// `t!` / `tf!` for `IntegrationError::user_message`, which renders the installer's existing
// `installer.utils.*` texts.
#[cfg(not(target_arch = "wasm32"))]
#[macro_use]
extern crate ms_i18n;

#[cfg(not(target_arch = "wasm32"))]
pub mod copy_identity;
#[cfg(not(target_arch = "wasm32"))]
mod error;
pub mod identity;
// The pure value builders are compiled into native host test builds too (their goldens run
// on Linux); the Win32, environment and process I/O inside is gated to
// `target_os = "windows"` item by item.
#[cfg(any(target_os = "windows", all(test, not(target_arch = "wasm32"))))]
pub mod windows;
// Same split as `windows`: the pure desktop-entry text is also compiled into native host test
// builds; the filesystem and process I/O (`linux::xdg`) is gated to `target_os = "linux"`.
#[cfg(any(target_os = "linux", all(test, not(target_arch = "wasm32"))))]
pub mod linux;

// The registration report model is pure and also compiled into native host test builds (the
// Windows evaluators are tested on Linux); `report::probe` itself exists on Windows and Linux only.
#[cfg(any(target_os = "windows", target_os = "linux", all(test, not(target_arch = "wasm32"))))]
pub mod report;
// The action rules, the helper codec and its result-file protocol are pure and compiled into
// native host test builds too; `actions::apply` exists on Windows and Linux only.
#[cfg(any(target_os = "windows", target_os = "linux", all(test, not(target_arch = "wasm32"))))]
pub mod actions;

#[cfg(not(target_arch = "wasm32"))]
pub use copy_identity::CopyIdentity;
#[cfg(not(target_arch = "wasm32"))]
pub use error::IntegrationError;
