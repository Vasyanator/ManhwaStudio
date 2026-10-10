/*
File: crates/ms-window-geometry/src/android_app.rs

Purpose:
The one process-global owner of the Android `AndroidApp` handle. winit cannot create an event
loop on Android without it, so every `eframe::NativeOptions` the process builds (launcher,
studio, startup prompts) reads it from here into `NativeOptions::android_app`.

Key functions:
- install(): stores the handle of the current Activity (called by `android_main`).
- current(): a clone of the stored handle for a `NativeOptions` builder.
- apply_native_window_policy(): hands `General.android_native_windows` to the forked winit's
  process-global secondary-window policy (called through the crate-root
  `apply_android_native_window_policy` before each product `run_native`).

Notes:
Compiled only for `target_os = "android"` (the `mod` declaration in `lib.rs` is gated), so
desktop and wasm builds contain nothing of it. It lives in this crate because the crate is the
project's only direct `winit` user and already sits under both the binary and `ms-launcher`.
android-activity may call `android_main` again for a re-created Activity, so `install`
REPLACES the previous handle instead of keeping the first one.
*/

use ms_config::AndroidNativeWindows;
use ms_log::runtime_log;
use serde_json::Value;
use std::sync::{PoisonError, RwLock};
use winit::platform::android::multiwindow::{NativeWindowPolicy, set_native_window_policy};

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

/// Sets the forked winit's secondary-window policy from `General.android_native_windows` in
/// `settings` (unknown value -> `Auto`, see `ms_config::android_native_windows_from_user_settings`).
///
/// eframe reads `multiwindow::native_windows_available()` once per `run_native` to choose
/// between native and embedded viewports, so this must run BEFORE each product `run_native`
/// for a changed setting to take effect. Windows already open keep their mode.
pub fn apply_native_window_policy(settings: &Value) {
    let setting = ms_config::android_native_windows_from_user_settings(settings);
    let policy = match setting {
        AndroidNativeWindows::Auto => NativeWindowPolicy::Auto,
        AndroidNativeWindows::Native => NativeWindowPolicy::Native,
        AndroidNativeWindows::Embedded => NativeWindowPolicy::Embedded,
    };
    set_native_window_policy(policy);
    runtime_log::log_info(format!("[android-windows] secondary-window policy set to \"{}\"", setting.as_config_str()));
}
