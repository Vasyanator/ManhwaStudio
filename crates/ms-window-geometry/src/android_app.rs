/*
File: crates/ms-window-geometry/src/android_app.rs

Purpose:
The one process-global owner of the Android `AndroidApp` handle. winit cannot create an event
loop on Android without it, so every `eframe::NativeOptions` the process builds (launcher,
studio, startup prompts) reads it from here into `NativeOptions::android_app`.

Key functions:
- install(): stores the handle of the current Activity (called by `android_main`).
- current(): a clone of the stored handle for a `NativeOptions` builder.
- set_secondary_window_policy(): resolves `General.secondary_windows` for Android and hands it
  to the forked winit's process-global secondary-window policy (called through the crate-root
  `sync_secondary_window_policy` / `apply_android_native_window_policy`).
- native_windows_available(): the fork's runtime gate (policy, bridge, full-screen fallback).

Notes:
Compiled only for `target_os = "android"` (the `mod` declaration in `lib.rs` is gated), so
desktop and wasm builds contain nothing of it. It lives in this crate because the crate is the
project's only direct `winit` user and already sits under both the binary and `ms-launcher`.
android-activity may call `android_main` again for a re-created Activity, so `install`
REPLACES the previous handle instead of keeping the first one.
*/

use ms_config::secondary_windows::{SecondaryWindows, WindowHost, WindowMode, resolve_for};
use ms_log::runtime_log;
use std::sync::{PoisonError, RwLock};
use winit::platform::android::multiwindow::{self, NativeWindowPolicy, set_native_window_policy};

/// The android-activity handle type, re-exported so callers need no direct `winit` dependency.
pub use winit::platform::android::activity::AndroidApp;

/// Handle of the Activity the process currently runs in; `None` until `install`.
static CURRENT: RwLock<Option<AndroidApp>> = RwLock::new(None);

/// Stores `app` as the process's current Activity handle, replacing any earlier one.
///
/// Must run in `android_main` before the first `eframe::run_native`. A poisoned lock is
/// recovered: the slot holds a plain `Option` that no panic can leave half-written.
pub fn install(app: AndroidApp) {
    let mut slot = CURRENT.write().unwrap_or_else(PoisonError::into_inner);
    *slot = Some(app);
}

/// A clone of the handle stored by `install`, for `NativeOptions::android_app`.
///
/// Returns `None` only when `install` has not run, i.e. when a window is opened outside the
/// `android_main` startup path; winit then refuses to build the event loop.
#[must_use]
pub fn current() -> Option<AndroidApp> {
    CURRENT.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Sets the forked winit's secondary-window policy from the user's `setting`.
///
/// The setting is resolved for Android first (`ms_config::secondary_windows::resolve_for`):
/// `Separate` -> `NativeWindowPolicy::Native`, `Auto` / `Embedded` -> `Embedded`. The fork's
/// own `Auto` (device free-form detection) is never used: on Android the user's `Auto` means
/// embedded. Windows already open keep their mode.
pub fn set_secondary_window_policy(setting: SecondaryWindows) {
    let policy = match resolve_for(setting, WindowHost::Android) {
        WindowMode::Separate => NativeWindowPolicy::Native,
        WindowMode::Embedded => NativeWindowPolicy::Embedded,
    };
    set_native_window_policy(policy);
    runtime_log::log_info(format!("[secondary-windows] Android policy set to {policy:?} for setting \"{}\"", setting.as_config_str()));
}

/// Whether a window created now becomes a separate OS window: the policy allows it, the Kotlin
/// bridge is packaged and no secondary window fell back to full-screen in this process.
/// `false` before the first event loop exists.
#[must_use]
pub fn native_windows_available() -> bool {
    multiwindow::native_windows_available()
}
