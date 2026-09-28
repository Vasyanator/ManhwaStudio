/*
File: crates/ms-theme/src/status.rs

Purpose:
Status (severity) colours of studio TEXT and labels on dark panels: error, warning, success and
in-progress messages, toasts and status lines; plus the fill of an armed destructive button.

Key structures:
- Severity

Key constants:
- ERROR, WARNING, SUCCESS, INFO, DESTRUCTIVE_ARMED_FILL

Notes:
These are panel-text shades, deliberately softer than the canvas chrome in `canvas.rs`, which must
stay readable over arbitrary page images. `ERROR` and `WARNING` are also installed into
`Visuals::error_fg_color` / `warn_fg_color` by `crate::apply`, so `ui.visuals()` agrees with them.
*/

use egui::Color32;

/// Error text: failed operations, rejected input, error toasts.
pub const ERROR: Color32 = Color32::from_rgb(240, 102, 102);

/// Warning text: degraded or partial results, recoverable problems, "check this" notices.
pub const WARNING: Color32 = Color32::from_rgb(225, 180, 60);

/// Success text: completed operations, "saved"/"deleted" confirmations, healthy status.
pub const SUCCESS: Color32 = Color32::from_rgb(42, 168, 88);

/// In-progress / neutral notice text: "loading…", "downloading model…" toasts.
///
/// Taken from the only existing use (`Color32::GOLD`); it sits close to [`WARNING`] and is the
/// first candidate for retuning.
pub const INFO: Color32 = Color32::from_rgb(255, 215, 0);

/// Fill of a destructive button once it is ARMED (second click confirms the deletion).
pub const DESTRUCTIVE_ARMED_FILL: Color32 = Color32::from_rgb(150, 40, 40);

/// Severity of a status message; picks its text colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Neutral or in-progress notice ([`INFO`]).
    Info,
    /// Completed successfully ([`SUCCESS`]).
    Success,
    /// Recoverable problem or partial result ([`WARNING`]).
    Warning,
    /// Failure ([`ERROR`]).
    Error,
}

impl Severity {
    /// Text colour of a message of this severity, on a dark panel.
    #[must_use]
    pub const fn color(self) -> Color32 {
        match self {
            Self::Info => INFO,
            Self::Success => SUCCESS,
            Self::Warning => WARNING,
            Self::Error => ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ERROR, INFO, SUCCESS, Severity, WARNING};

    #[test]
    fn severity_maps_to_its_constant() {
        assert_eq!(Severity::Info.color(), INFO);
        assert_eq!(Severity::Success.color(), SUCCESS);
        assert_eq!(Severity::Warning.color(), WARNING);
        assert_eq!(Severity::Error.color(), ERROR);
    }

    #[test]
    fn severities_are_distinct() {
        let colors = [INFO, SUCCESS, WARNING, ERROR];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
