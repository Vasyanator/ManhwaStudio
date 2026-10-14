/*
FILE OVERVIEW: crates/ms-launcher/src/lib.rs
Crate root of `ms-launcher`, re-exported by the binary as `crate::launcher`
(`src/lib.rs`), so every `crate::launcher::…` call site keeps working unchanged.

Purpose:
Runtime for the Rust launcher UI.

Main responsibilities:
- expose the launcher screen API (`app::{LauncherEntry, prepare_entry, LauncherApp}`), which the
  main window (`src/shell/` of the root package) and the web entry host as a screen;
- keep launcher-specific code isolated in this crate.

Key modules:
- `app`: the launcher screen (`LauncherApp`) and its entry preparation
- `background`: background catalog/validation workers
- `confirm_modal`: in-app Yes/No confirmation (`egui::Modal`) for overwrite questions
- `first_run_language`: first-run interface/typesetting language-selection modal
- `main_page`: rendering of the current main page
- `natural_sort`: the shared natural file-name order (PSD import table, image multi-pick)
- `new_project`: detached "New Project" window runtime and UI
- `open_image`: main-menu "Open image" multi-file picker worker, path validation, sort, outcome
  mapping (native only)
- `pages`: animated fullscreen launcher subpages
- `psd_import_window`: detached PSD/PSB import window, parsed in-process by `ag-psd`
- `state`: shared launcher UI state and page enum
- `theme`: dark theme styling helpers
- `tutorial`: launcher main-menu tutorial step script

Notes:
The launcher owns no window: its exit (`LauncherOutcome`: the selected or newly saved
`project_dir`, picked images for image-set mode, or "update") is taken by the host, which
switches the same window to the studio instead of spawning a second app process.
*/

#![warn(clippy::all)]
// `new_without_default` fires on ten launcher controller/state structs
// (`QuickDownloadController`, `RelineController`, `RibbonState`, `Waifu2xController`, …).
// It is a LIBRARY-API lint: it only started applying when these types stopped being
// crate-private and became exported items of this crate. It does not apply here for an
// architectural reason: each of them has exactly ONE construction point, called from one
// place in this same crate, and `new()` is where the owning window documents what the
// controller starts out holding. A `Default` impl would add a second, equivalent
// constructor to the public surface of a crate that no one outside the binary consumes —
// pure API for no caller. The lint stays on everywhere else in the workspace.
#![allow(clippy::new_without_default)]

// `t!` / `tf!` / `tp!` for every user-visible string of the launcher.
#[macro_use]
extern crate ms_i18n;

pub mod app;
pub mod background;
mod confirm_modal;
pub mod first_run_language;
pub mod main_page;
mod natural_sort;
pub mod new_project;
#[cfg(not(target_arch = "wasm32"))]
pub mod open_image;
pub mod pages;
pub mod psd_import_window;
pub mod state;
pub mod theme;
#[cfg(feature = "tutorial")]
pub mod tutorial;

pub use app::{LauncherApp, LauncherEntry, prepare_entry, prepare_entry_with_settings};
