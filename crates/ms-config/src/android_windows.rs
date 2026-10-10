/*
File: android_windows.rs

Purpose:
The Android secondary-window policy setting (`General.android_native_windows`): whether extra
viewports (New Project, PSD import, Wiki guide, floating panels) open as separate OS windows
on Android or are embedded in the main window. This file owns the typed value, its frozen
config spelling and the pure reader.

Key structures:
- AndroidNativeWindows : Auto (default) / Native / Embedded, with `as_config_str` /
  `from_config_str`

Key functions:
- android_native_windows_from_user_settings() : read the policy out of a user-config value

Notes:
- GUI-free and compiled on every target: the settings pane names the enum on desktop too
  (it only SHOWS the row on Android), and the reader is unit-tested on the host.
- The value is APPLIED by `ms_window_geometry::apply_android_native_window_policy` before each
  product `eframe::run_native` (the forked winit holds the process-global policy that eframe
  consults once per `run_native`), so a change takes effect at the next window start.
*/

use ms_log::runtime_log;
use serde_json::Value;

/// `General` key holding the Android secondary-window policy. Frozen spelling (persisted),
/// values [`AndroidNativeWindows::as_config_str`].
pub const GENERAL_ANDROID_NATIVE_WINDOWS_KEY: &str = "android_native_windows";

/// Whether secondary viewports may be separate OS windows on Android.
///
/// Mirrors the forked winit's `multiwindow::NativeWindowPolicy` one to one; it is declared
/// here because this crate cannot depend on winit and every other target still has to read,
/// show and persist it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AndroidNativeWindows {
    /// Separate windows when the device can place free-form windows (DeX, free-form
    /// feature or developer setting), embedded otherwise.
    #[default]
    Auto,
    /// Separate windows whenever the window bridge is packaged, whatever the device reports.
    Native,
    /// Always embedded in the main window.
    Embedded,
}

impl AndroidNativeWindows {
    /// Every policy, in UI order.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Native, Self::Embedded];

    /// The persisted spelling (frozen: `"auto"` / `"native"` / `"embedded"`).
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
            Self::Embedded => "embedded",
        }
    }

    /// Parses a persisted spelling (surrounding whitespace and ASCII case ignored); `None`
    /// for anything else.
    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "native" => Some(Self::Native),
            "embedded" => Some(Self::Embedded),
            _ => None,
        }
    }
}

/// Reads `General.android_native_windows` out of a user-config value. A missing value
/// resolves silently to [`AndroidNativeWindows::Auto`]; a present but non-string or unknown
/// value also resolves to `Auto` and logs a warning (never an error: the policy must not
/// stop a window from opening).
#[must_use]
pub fn android_native_windows_from_user_settings(user_settings: &Value) -> AndroidNativeWindows {
    let Some(raw) = user_settings.get("General").and_then(Value::as_object).and_then(|general| general.get(GENERAL_ANDROID_NATIVE_WINDOWS_KEY)) else {
        return AndroidNativeWindows::default();
    };
    if let Some(policy) = raw.as_str().and_then(AndroidNativeWindows::from_config_str) {
        return policy;
    }
    let fallback = AndroidNativeWindows::default();
    runtime_log::log_warn(format!("[android-windows] unknown General.{GENERAL_ANDROID_NATIVE_WINDOWS_KEY} value {raw}; using \"{}\"", fallback.as_config_str()));
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_spelling_round_trips_for_every_policy() {
        for policy in AndroidNativeWindows::ALL {
            assert_eq!(AndroidNativeWindows::from_config_str(policy.as_config_str()), Some(policy));
        }
        assert_eq!(AndroidNativeWindows::from_config_str("  Native "), Some(AndroidNativeWindows::Native));
        assert_eq!(AndroidNativeWindows::from_config_str("separate"), None);
    }

    #[test]
    fn reader_defaults_to_auto_for_missing_or_invalid_values() {
        assert_eq!(android_native_windows_from_user_settings(&json!({})), AndroidNativeWindows::Auto);
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {}})), AndroidNativeWindows::Auto);
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {"android_native_windows": 1}})), AndroidNativeWindows::Auto);
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {"android_native_windows": "bogus"}})), AndroidNativeWindows::Auto);
    }

    #[test]
    fn reader_honours_stored_policies() {
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {"android_native_windows": "native"}})), AndroidNativeWindows::Native);
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {"android_native_windows": "embedded"}})), AndroidNativeWindows::Embedded);
        assert_eq!(android_native_windows_from_user_settings(&json!({"General": {"android_native_windows": "auto"}})), AndroidNativeWindows::Auto);
    }

    #[test]
    fn default_tree_carries_the_auto_policy() {
        let defaults = crate::user_config_defaults();
        assert_eq!(defaults["General"][GENERAL_ANDROID_NATIVE_WINDOWS_KEY], json!(AndroidNativeWindows::Auto.as_config_str()));
    }
}
