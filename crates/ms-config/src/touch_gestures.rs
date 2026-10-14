/*
File: touch_gestures.rs

Purpose:
The touchscreen-gestures setting (`General.touch_gestures`): whether a finger on a canvas
pans, pinch-zooms and long-presses (the `ms_widgets::touch_nav` router) or is left to egui
as ordinary pointer input. One boolean for every platform; this file owns its frozen config
spelling, the pure per-platform default, the pure reader, the process-global live value and
the pure combination with the platform capability.

Key items:
- GENERAL_TOUCH_GESTURES_KEY : the frozen `General` key (a JSON boolean)
- default_for() / DEFAULT    : the per-host default (Android on; desktop and web off)
- touch_gestures_from_user_settings() : pure reader of a user-config value
- current() / set() / seed_from_user_settings() : the process-global live value
- effective() / enabled_with()        : capability AND setting

Notes:
- GUI-free and compiled on every target. The platform CAPABILITY (Windows cannot tell a pen
  from a finger, the web has no raw-input hook) is `ms_platform::PlatformFeature::
  CanvasTouchGestures`; this crate does not depend on `ms-platform`, so the caller passes the
  capability in (`enabled_with`). The default follows `WindowHost`, the one host class
  `secondary_windows` already owns.
- The live value is a relaxed atomic read by the main window's raw-input hook every frame;
  nothing here does I/O except the warning log of the reader.
*/

use std::sync::atomic::{AtomicBool, Ordering};

use ms_log::runtime_log;
use serde_json::Value;

use crate::secondary_windows::WindowHost;

/// `General` key holding the touchscreen-gestures setting. Frozen spelling (persisted); the
/// value is a JSON boolean.
pub const GENERAL_TOUCH_GESTURES_KEY: &str = "touch_gestures";

/// The default for a host (pure): on for Android, whose primary input is a finger; off for a
/// desktop, where a touchscreen is the exception and finger gestures would surprise a mouse
/// user; off for the web, which cannot route touches at all.
#[must_use]
pub const fn default_for(host: WindowHost) -> bool {
    match host {
        WindowHost::Android => true,
        WindowHost::Desktop | WindowHost::Web => false,
    }
}

/// The default for the host this binary was compiled for; the `General` default tree
/// carries it.
pub const DEFAULT: bool = default_for(WindowHost::current());

/// Backing store of the live setting. Relaxed suffices: it is one independent flag read once
/// per frame. Starts at the host default so a reader before the startup seed agrees with a
/// fresh install.
static CURRENT: AtomicBool = AtomicBool::new(DEFAULT);

/// The live setting (the host default until [`set`] or [`seed_from_user_settings`] ran).
#[must_use]
pub fn current() -> bool {
    CURRENT.load(Ordering::Relaxed)
}

/// Replaces the live setting. The main window applies it on its next frame; nothing is
/// persisted here.
pub fn set(enabled: bool) {
    CURRENT.store(enabled, Ordering::Relaxed);
}

/// Seeds the live setting from a loaded `user_config` root (see
/// [`touch_gestures_from_user_settings`] for missing and invalid values).
pub fn seed_from_user_settings(user_settings: &Value) {
    set(touch_gestures_from_user_settings(user_settings));
}

/// Reads the setting out of a user-config value: `General.touch_gestures` when it is a JSON
/// boolean. Absent resolves silently to [`DEFAULT`]; a present non-boolean value resolves to
/// [`DEFAULT`] with a warning (never an error: the setting must not stop a window from
/// opening).
#[must_use]
pub fn touch_gestures_from_user_settings(user_settings: &Value) -> bool {
    let Some(raw) = user_settings.get("General").and_then(|general| general.get(GENERAL_TOUCH_GESTURES_KEY)) else {
        return DEFAULT;
    };
    raw.as_bool().unwrap_or_else(|| {
        runtime_log::log_warn(format!("[touch-gestures] invalid General.{GENERAL_TOUCH_GESTURES_KEY} value {raw} (expected true/false); using {DEFAULT}"));
        DEFAULT
    })
}

/// Whether canvas finger gestures run (pure): the platform supports them (`available`, from
/// `PlatformFeature::CanvasTouchGestures`) AND the user setting is on.
#[must_use]
pub const fn effective(available: bool, setting: bool) -> bool {
    available && setting
}

/// [`effective`] over the live setting.
#[must_use]
pub fn enabled_with(available: bool) -> bool {
    effective(available, current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_is_on_for_android_only() {
        assert!(default_for(WindowHost::Android));
        assert!(!default_for(WindowHost::Desktop));
        assert!(!default_for(WindowHost::Web));
        assert_eq!(DEFAULT, default_for(WindowHost::current()));
    }

    #[test]
    fn reader_falls_back_to_the_host_default() {
        assert_eq!(touch_gestures_from_user_settings(&json!({})), DEFAULT);
        assert_eq!(touch_gestures_from_user_settings(&json!({"General": {}})), DEFAULT);
        assert_eq!(touch_gestures_from_user_settings(&json!({"General": "not an object"})), DEFAULT);
        assert_eq!(touch_gestures_from_user_settings(&json!({"General": {"touch_gestures": "true"}})), DEFAULT);
        assert_eq!(touch_gestures_from_user_settings(&json!({"General": {"touch_gestures": 1}})), DEFAULT);
    }

    #[test]
    fn reader_honours_a_stored_boolean() {
        assert!(touch_gestures_from_user_settings(&json!({"General": {"touch_gestures": true}})));
        assert!(!touch_gestures_from_user_settings(&json!({"General": {"touch_gestures": false}})));
    }

    /// The only test that writes the process-global value: it restores it before returning,
    /// so no other test of this crate observes a change.
    #[test]
    fn seed_and_set_round_trip_through_the_live_value() {
        let before = current();
        seed_from_user_settings(&json!({"General": {"touch_gestures": !DEFAULT}}));
        assert_eq!(current(), !DEFAULT);
        assert_eq!(enabled_with(true), !DEFAULT);
        assert!(!enabled_with(false));
        set(DEFAULT);
        assert_eq!(current(), DEFAULT);
        seed_from_user_settings(&json!({}));
        assert_eq!(current(), DEFAULT);
        set(before);
    }

    #[test]
    fn effective_requires_capability_and_setting() {
        assert!(effective(true, true));
        assert!(!effective(true, false));
        assert!(!effective(false, true));
        assert!(!effective(false, false));
    }

    #[test]
    fn default_tree_carries_the_host_default() {
        let defaults = crate::user_config_defaults();
        assert_eq!(defaults["General"][GENERAL_TOUCH_GESTURES_KEY], json!(DEFAULT));
    }
}
