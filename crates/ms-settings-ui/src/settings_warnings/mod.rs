/*
FILE OVERVIEW: crates/ms-settings-ui/src/settings_warnings/mod.rs
Module root of the per-setting warnings ("!" badges) of the shared settings panes.

Purpose:
Re-exports the public surface the panes (item badges, change reporting) and the launcher
(worker lifecycle, tab and main-menu badges) build on. See `MODULE_README.md` here.

Submodules:
- `model`: GUI-free, wasm-clean keys, reasons, levels, changes and the `WarningSet`.
- `checks` (native-only): fact gathering + pure decision functions per key.
- `runtime`: `SettingsWarnings` (worker, generations, polling) and `CheckContext`.
- `badge`: the egui "!" painter and the inline item badge.
*/

mod badge;
// The checks read the disk, the native ORT runtime and the Python backend layout: none
// of that exists on the web build, where the runtime stays inert.
#[cfg(not(target_arch = "wasm32"))]
mod checks;
mod model;
mod runtime;

pub use badge::{item_warning_badge, paint_corner_badge, paint_warning_badge};
pub use model::{
    FallbackCause, NATIVE_FAMILY, SettingChange, SettingGroupId, SettingKey, SettingLocation, SettingWarning,
    WarningLevel, WarningReason, WarningSet,
};
pub use runtime::{CheckContext, SettingsWarnings};
