/*
File: secondary_windows.rs

Purpose:
The secondary-window setting (`General.secondary_windows`): whether extra windows (New
Project, PSD import, the Wiki guide, batch processing) open as separate OS windows or embedded
inside the main window. Dock panels are never embedded: in embedded mode they simply cannot be
detached. One setting for every platform; this file owns
the typed value, its frozen config spelling, the pure per-platform resolution, the
process-global live value and the migration from the Android-only predecessor
`General.android_native_windows`.

Key structures:
- SecondaryWindows : the persisted user choice, Auto (default) / Separate / Embedded
- WindowMode       : the resolved decision, Separate / Embedded
- WindowHost       : the platform class the decision depends on (Desktop / Android / Web)

Key functions:
- resolve_for()          : pure (setting, host) -> WindowMode table
- requested_mode()       : the live setting resolved for the running host
- current() / set() / seed_from_user_settings() : the process-global live value
- secondary_windows_from_user_settings()        : pure reader of a user-config value
- migrate_android_native_windows()              : rewrites the legacy key (load_user_config)

Notes:
- GUI-free and compiled on every target; the only `cfg` site of the decision is
  `WindowHost::current`.
- There is deliberately no `ms_platform::PlatformFeature` for this: the decision is one user
  setting plus eframe's runtime gate (on Android the forked winit may still refuse native
  windows, see `ms_window_geometry::sync_secondary_window_policy`), not a platform capability.
- The live value is a relaxed atomic read on the GUI thread; nothing here does I/O except the
  warning logs of the reader and the migration.
*/

use std::sync::atomic::{AtomicU8, Ordering};

use ms_log::runtime_log;
use serde_json::Value;

/// `General` key holding the secondary-window setting. Frozen spelling (persisted), values
/// [`SecondaryWindows::as_config_str`].
pub const GENERAL_SECONDARY_WINDOWS_KEY: &str = "secondary_windows";

/// `General` key of the Android-only predecessor setting (`"auto"` / `"native"` /
/// `"embedded"`). Read only by the migration and the legacy fallback of the reader; it is not
/// in the defaults tree, so `merge_missing` never re-creates it.
pub const LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY: &str = "android_native_windows";

/// The user's choice of where secondary windows open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SecondaryWindows {
    /// The platform default: separate windows on a desktop, embedded on Android (the user
    /// decided against device auto-detection there), embedded on the web.
    #[default]
    Auto,
    /// Separate OS windows wherever the platform can create them (never on the web).
    Separate,
    /// Always embedded inside the main window.
    Embedded,
}

impl SecondaryWindows {
    /// Every choice, in UI order.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Separate, Self::Embedded];

    /// The persisted spelling (frozen: `"auto"` / `"separate"` / `"embedded"`).
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Separate => "separate",
            Self::Embedded => "embedded",
        }
    }

    /// Parses a persisted spelling (surrounding whitespace and ASCII case ignored); `None`
    /// for anything else.
    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "separate" => Some(Self::Separate),
            "embedded" => Some(Self::Embedded),
            _ => None,
        }
    }

    /// Encoding of the process-global atomic (private, never persisted).
    fn to_raw(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Separate => 1,
            Self::Embedded => 2,
        }
    }

    /// Inverse of [`Self::to_raw`]. Only [`set`] stores into the atomic, so an unknown byte
    /// cannot occur; it would decode as the default.
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Separate,
            2 => Self::Embedded,
            _ => Self::Auto,
        }
    }
}

/// The resolved decision for one secondary window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowMode {
    /// A separate OS window (an egui immediate/deferred viewport of its own).
    Separate,
    /// Drawn inside the main window.
    Embedded,
}

/// The platform class the secondary-window decision depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowHost {
    /// Linux, Windows, macOS.
    Desktop,
    /// Android (forked winit; separate windows are Activities).
    Android,
    /// The wasm build: one browser canvas, separate windows do not exist.
    Web,
}

impl WindowHost {
    /// The host this binary was compiled for. The ONLY `cfg` site of the secondary-window
    /// decision; everything else goes through [`resolve_for`].
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_arch = "wasm32") {
            Self::Web
        } else if cfg!(target_os = "android") {
            Self::Android
        } else {
            Self::Desktop
        }
    }
}

/// Resolves a setting for a host (pure). Web is always embedded; Android is embedded unless
/// the user explicitly chose `Separate`; a desktop is separate unless the user explicitly
/// chose `Embedded`.
#[must_use]
pub fn resolve_for(setting: SecondaryWindows, host: WindowHost) -> WindowMode {
    match host {
        WindowHost::Web => WindowMode::Embedded,
        WindowHost::Android => match setting {
            SecondaryWindows::Separate => WindowMode::Separate,
            SecondaryWindows::Auto | SecondaryWindows::Embedded => WindowMode::Embedded,
        },
        WindowHost::Desktop => match setting {
            SecondaryWindows::Embedded => WindowMode::Embedded,
            SecondaryWindows::Auto | SecondaryWindows::Separate => WindowMode::Separate,
        },
    }
}

/// The live setting ([`current`]) resolved for the running host. On Android a `Separate`
/// result is a request only: the forked winit may still refuse native windows at runtime.
#[must_use]
pub fn requested_mode() -> WindowMode {
    resolve_for(current(), WindowHost::current())
}

/// Backing store of the live setting ([`SecondaryWindows::to_raw`]). Relaxed suffices: it is
/// one independent value read when a window opens.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The live setting (the default `Auto` until [`set`] or [`seed_from_user_settings`] ran).
#[must_use]
pub fn current() -> SecondaryWindows {
    SecondaryWindows::from_raw(CURRENT.load(Ordering::Relaxed))
}

/// Replaces the live setting. Takes effect the next time a secondary window opens; nothing
/// is persisted here.
pub fn set(setting: SecondaryWindows) {
    CURRENT.store(setting.to_raw(), Ordering::Relaxed);
}

/// Seeds the live setting from a loaded `user_config` root (see
/// [`secondary_windows_from_user_settings`] for missing and invalid values).
pub fn seed_from_user_settings(user_settings: &Value) {
    set(secondary_windows_from_user_settings(user_settings));
}

/// Reads the setting out of a user-config value.
///
/// `General.secondary_windows` wins. When it is absent, a legacy
/// `General.android_native_windows` is mapped exactly as [`migrate_android_native_windows`]
/// maps it, so a raw read taken before `load_user_config` migrated the document agrees with
/// the migrated one. Neither present resolves silently to `Auto`; a present but non-string or
/// unknown value resolves to `Auto` with a warning (never an error: the setting must not stop
/// a window from opening).
#[must_use]
pub fn secondary_windows_from_user_settings(user_settings: &Value) -> SecondaryWindows {
    let Some(general) = user_settings.get("General").and_then(Value::as_object) else {
        return SecondaryWindows::default();
    };
    if let Some(raw) = general.get(GENERAL_SECONDARY_WINDOWS_KEY) {
        if let Some(setting) = raw.as_str().and_then(SecondaryWindows::from_config_str) {
            return setting;
        }
        let fallback = SecondaryWindows::default();
        runtime_log::log_warn(format!("[secondary-windows] unknown General.{GENERAL_SECONDARY_WINDOWS_KEY} value {raw}; using \"{}\"", fallback.as_config_str()));
        return fallback;
    }
    general.get(LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY).map_or_else(SecondaryWindows::default, map_legacy_android_native_windows)
}

/// Maps one legacy `General.android_native_windows` value onto the new setting:
/// `"native"` -> `Separate`, `"embedded"` -> `Embedded`, `"auto"` -> `Auto`; anything else
/// -> `Auto` with a warning. Legacy `"auto"` meant "detect free-form windowing", which the
/// new `Auto` no longer does on Android (it embeds); that change is the user's decision.
fn map_legacy_android_native_windows(raw: &Value) -> SecondaryWindows {
    match raw.as_str().map(|text| text.trim().to_ascii_lowercase()).as_deref() {
        Some("native") => SecondaryWindows::Separate,
        Some("embedded") => SecondaryWindows::Embedded,
        Some("auto") => SecondaryWindows::Auto,
        _ => {
            runtime_log::log_warn(format!("[secondary-windows] unknown legacy General.{LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY} value {raw}; migrating as \"auto\""));
            SecondaryWindows::Auto
        }
    }
}

/// Migrates `General.android_native_windows` to `General.secondary_windows` in place.
///
/// When the legacy key is present it is ALWAYS removed; the new key is written from it only
/// when the new key is absent (after a downgrade-then-upgrade both exist, and the new key
/// wins). Returns whether `data` changed, i.e. whether the caller must persist it. Run by
/// `load_user_config` before the defaults backfill, under the document lock.
pub fn migrate_android_native_windows(data: &mut Value) -> bool {
    let Some(general) = data.get_mut("General").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(legacy) = general.remove(LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY) else {
        return false;
    };
    if general.contains_key(GENERAL_SECONDARY_WINDOWS_KEY) {
        runtime_log::log_info(format!("[secondary-windows] dropped legacy General.{LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY}={legacy}; General.{GENERAL_SECONDARY_WINDOWS_KEY} is already set"));
        return true;
    }
    let migrated = map_legacy_android_native_windows(&legacy);
    general.insert(GENERAL_SECONDARY_WINDOWS_KEY.to_string(), Value::String(migrated.as_config_str().to_string()));
    runtime_log::log_info(format!("[secondary-windows] migrated General.{LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY}={legacy} to General.{GENERAL_SECONDARY_WINDOWS_KEY}=\"{}\"", migrated.as_config_str()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_spelling_round_trips_for_every_setting() {
        for setting in SecondaryWindows::ALL {
            assert_eq!(SecondaryWindows::from_config_str(setting.as_config_str()), Some(setting));
            assert_eq!(SecondaryWindows::from_raw(setting.to_raw()), setting);
        }
        assert_eq!(SecondaryWindows::from_config_str("  Separate "), Some(SecondaryWindows::Separate));
        assert_eq!(SecondaryWindows::from_config_str("native"), None);
    }

    #[test]
    fn resolve_for_covers_every_setting_and_host() {
        use SecondaryWindows::{Auto, Embedded, Separate};
        use WindowHost::{Android, Desktop, Web};
        let table = [
            (Auto, Desktop, WindowMode::Separate),
            (Separate, Desktop, WindowMode::Separate),
            (Embedded, Desktop, WindowMode::Embedded),
            (Auto, Android, WindowMode::Embedded),
            (Separate, Android, WindowMode::Separate),
            (Embedded, Android, WindowMode::Embedded),
            (Auto, Web, WindowMode::Embedded),
            (Separate, Web, WindowMode::Embedded),
            (Embedded, Web, WindowMode::Embedded),
        ];
        for (setting, host, expected) in table {
            assert_eq!(resolve_for(setting, host), expected, "{setting:?} on {host:?}");
        }
    }

    #[test]
    fn host_current_matches_the_compile_target() {
        let expected = if cfg!(target_arch = "wasm32") {
            WindowHost::Web
        } else if cfg!(target_os = "android") {
            WindowHost::Android
        } else {
            WindowHost::Desktop
        };
        assert_eq!(WindowHost::current(), expected);
    }

    #[test]
    fn reader_defaults_to_auto_for_missing_or_invalid_values() {
        assert_eq!(secondary_windows_from_user_settings(&json!({})), SecondaryWindows::Auto);
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {}})), SecondaryWindows::Auto);
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"secondary_windows": 1}})), SecondaryWindows::Auto);
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"secondary_windows": "bogus"}})), SecondaryWindows::Auto);
    }

    #[test]
    fn reader_honours_stored_settings_and_prefers_the_new_key() {
        for setting in SecondaryWindows::ALL {
            assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"secondary_windows": setting.as_config_str()}})), setting);
        }
        let both = json!({"General": {"secondary_windows": "embedded", "android_native_windows": "native"}});
        assert_eq!(secondary_windows_from_user_settings(&both), SecondaryWindows::Embedded);
    }

    #[test]
    fn reader_maps_an_unmigrated_legacy_key() {
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"android_native_windows": "native"}})), SecondaryWindows::Separate);
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"android_native_windows": "embedded"}})), SecondaryWindows::Embedded);
        assert_eq!(secondary_windows_from_user_settings(&json!({"General": {"android_native_windows": "auto"}})), SecondaryWindows::Auto);
    }

    #[test]
    fn migration_maps_every_legacy_value_and_removes_the_old_key() {
        let cases = [(json!("native"), "separate"), (json!("embedded"), "embedded"), (json!("auto"), "auto"), (json!("bogus"), "auto"), (json!(7), "auto")];
        for (legacy, expected) in cases {
            let mut data = json!({"General": {"android_native_windows": legacy.clone(), "keep": true}});
            assert!(migrate_android_native_windows(&mut data), "legacy {legacy}");
            assert_eq!(data, json!({"General": {"secondary_windows": expected, "keep": true}}), "legacy {legacy}");
        }
    }

    #[test]
    fn migration_keeps_an_existing_new_key_and_still_drops_the_old_one() {
        let mut data = json!({"General": {"android_native_windows": "native", "secondary_windows": "embedded"}});
        assert!(migrate_android_native_windows(&mut data));
        assert_eq!(data, json!({"General": {"secondary_windows": "embedded"}}));
    }

    #[test]
    fn migration_is_a_no_op_without_the_legacy_key() {
        for original in [json!({}), json!({"General": "not an object"}), json!({"General": {"secondary_windows": "separate"}}), json!([])] {
            let mut data = original.clone();
            assert!(!migrate_android_native_windows(&mut data));
            assert_eq!(data, original);
        }
    }

    #[test]
    fn default_tree_carries_auto_and_no_legacy_key() {
        let defaults = crate::user_config_defaults();
        assert_eq!(defaults["General"][GENERAL_SECONDARY_WINDOWS_KEY], json!(SecondaryWindows::Auto.as_config_str()));
        assert!(defaults["General"].get(LEGACY_GENERAL_ANDROID_NATIVE_WINDOWS_KEY).is_none());
    }
}
