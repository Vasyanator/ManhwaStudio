/*
FILE OVERVIEW: src/tabs/typing/rotation_ctrl_wheel.rs
App-wide runtime selection for how the typing tab's "Ctrl + mouse wheel" rotates a
selected text overlay.

Two modes:
- `Vector`: rotate the text-render parameter `global_rotation_deg` and re-render the
  overlay at the render level (ideal sharpness after rotation, slightly heavier).
- `Raster`: rotate the already-rasterized placement (`angle_deg` / deform mesh) — the
  legacy behavior; faster but the picture becomes less sharp.

This module is config-free (like `ms_text_util::text_punctuation`): it only owns the
thread-safe runtime global. The app seeds it at startup from
`TextTab.rotation_ctrl_wheel_mode` (`main.rs::seed_rotation_ctrl_wheel_from_config`),
the settings "Тайп" pane edits it live, and the typing tab reads it in the Ctrl+wheel
handler. It lives under `tabs::typing` because both the setting and the rotation are
typing-only concerns; the settings pane reaches it via
`crate::tabs::typing::rotation_ctrl_wheel::…` (same-crate path, no dependency cycle).

Key types:
- `RotationCtrlWheelMode`

Key functions:
- `rotation_ctrl_wheel_mode()` / `set_rotation_ctrl_wheel_mode()`
*/

use std::sync::atomic::{AtomicU8, Ordering};

/// How the typing tab's Ctrl+wheel gesture rotates a selected text overlay.
///
/// The stored config form is the lowercase string returned by
/// [`RotationCtrlWheelMode::as_config_str`] (`"vector"` / `"raster"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationCtrlWheelMode {
    /// Rotate the render-level `global_rotation_deg` and re-render (sharp).
    Vector,
    /// Rotate the rasterized placement (`angle_deg` / deform mesh) — legacy behavior.
    Raster,
}

/// Default mode: sharp render-level rotation.
pub const DEFAULT_ROTATION_CTRL_WHEEL_MODE: RotationCtrlWheelMode = RotationCtrlWheelMode::Vector;

impl RotationCtrlWheelMode {
    /// Stable lowercase token persisted in `user_config.json`.
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            RotationCtrlWheelMode::Vector => "vector",
            RotationCtrlWheelMode::Raster => "raster",
        }
    }

    /// Parses a persisted token; returns `None` for anything unrecognized so the
    /// caller can fall back to the default instead of guessing.
    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim() {
            "vector" => Some(RotationCtrlWheelMode::Vector),
            "raster" => Some(RotationCtrlWheelMode::Raster),
            _ => None,
        }
    }

    /// Raw discriminant for the atomic backing store. `const` so the static
    /// initializer can derive its default from `DEFAULT_ROTATION_CTRL_WHEEL_MODE`.
    const fn to_u8(self) -> u8 {
        match self {
            RotationCtrlWheelMode::Vector => 0,
            RotationCtrlWheelMode::Raster => 1,
        }
    }

    /// Inverse of [`RotationCtrlWheelMode::to_u8`]; any unexpected value resolves to
    /// the default (defensive — the store is only ever written from `to_u8`).
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => RotationCtrlWheelMode::Raster,
            0 => RotationCtrlWheelMode::Vector,
            _ => DEFAULT_ROTATION_CTRL_WHEEL_MODE,
        }
    }
}

/// Backing store for the runtime-global mode. Not on any per-character hot path, so a
/// plain relaxed atomic is sufficient (no generation-cache like `text_punctuation`).
static MODE: AtomicU8 = AtomicU8::new(DEFAULT_ROTATION_CTRL_WHEEL_MODE.to_u8());

/// Returns the currently active Ctrl+wheel rotation mode.
#[must_use]
pub fn rotation_ctrl_wheel_mode() -> RotationCtrlWheelMode {
    RotationCtrlWheelMode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Sets the active Ctrl+wheel rotation mode. New typing-tab rotations pick it up on the
/// next wheel event; nothing else is invalidated.
pub fn set_rotation_ctrl_wheel_mode(mode: RotationCtrlWheelMode) {
    MODE.store(mode.to_u8(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminant_roundtrips_both_variants() {
        // The atomic backing store relies on `to_u8`/`from_u8` being exact inverses.
        for mode in [RotationCtrlWheelMode::Vector, RotationCtrlWheelMode::Raster] {
            assert_eq!(RotationCtrlWheelMode::from_u8(mode.to_u8()), mode);
        }
        // Documented default must match the static initializer's discriminant.
        assert_eq!(DEFAULT_ROTATION_CTRL_WHEEL_MODE, RotationCtrlWheelMode::Vector);
    }

    #[test]
    fn config_str_roundtrips_both_variants() {
        for mode in [RotationCtrlWheelMode::Vector, RotationCtrlWheelMode::Raster] {
            assert_eq!(
                RotationCtrlWheelMode::from_config_str(mode.as_config_str()),
                Some(mode)
            );
        }
    }

    #[test]
    fn from_config_str_rejects_unknown() {
        assert_eq!(RotationCtrlWheelMode::from_config_str("nope"), None);
        assert_eq!(RotationCtrlWheelMode::from_config_str(""), None);
    }

    #[test]
    fn set_then_get_returns_value() {
        set_rotation_ctrl_wheel_mode(RotationCtrlWheelMode::Raster);
        assert_eq!(rotation_ctrl_wheel_mode(), RotationCtrlWheelMode::Raster);
        // Restore the default so other tests observing the global are unaffected.
        set_rotation_ctrl_wheel_mode(RotationCtrlWheelMode::Vector);
    }
}
