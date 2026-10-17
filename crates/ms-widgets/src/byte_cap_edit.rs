/*
FILE HEADER (crates/ms-widgets/src/byte_cap_edit.rs)

Purpose:
An editor for an OPTIONAL byte cap ("at most N GiB" or "no limit") as one row: a caption, a
"no limit" checkbox and, while a cap is set, a `WheelSpinBox` in GiB on a 0.5 GiB grid.

Key structures:
- `ByteCapEdit`: the widget over `&mut Option<u64>` (`None` = no cap).

Key functions:
- `gib_to_bytes()`, `bytes_to_gib()`, `snap_to_half_gib()`: the checked / saturating unit
  conversions the widget is built on.

Notes:
- No literals: every visible text (caption, "no limit", unit suffix) is handed in by the
  caller, already localized.
- `Response::changed()` is set on every edit, so the caller can debounce and send it.
- Consumers: the large-model limits of the cleaning AI editor and of the AI-backend settings
  pane; the widget knows nothing about either.
*/

use crate::WheelSpinBox;
use eframe::egui;
use egui::{Response, Ui, Widget};

/// Bytes in one GiB (2^30).
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// The editing grid, GiB.
const STEP_GIB: f64 = 0.5;
/// The cap a freshly enabled editor starts from when the caller gives no maximum, GiB.
const DEFAULT_CAP_GIB: f64 = 8.0;
/// The largest cap offered when the caller gives no maximum, GiB.
const FALLBACK_MAX_GIB: f64 = 1024.0;

/// Converts GiB to bytes. Non-finite and non-positive inputs give `0`; inputs beyond
/// `u64::MAX` bytes saturate.
#[must_use]
pub fn gib_to_bytes(gib: f64) -> u64 {
    if !gib.is_finite() || gib <= 0.0 {
        return 0;
    }
    let bytes = (gib * GIB).round();
    // Cast justification: `u64::MAX as f64` rounds up to 2^64, so anything at or above it
    // saturates explicitly; below it the value is a finite non-negative integer that fits.
    if bytes >= u64::MAX as f64 { u64::MAX } else { bytes as u64 }
}

/// Converts bytes to GiB (display precision: exact up to 2^53 bytes).
#[must_use]
pub fn bytes_to_gib(bytes: u64) -> f64 {
    // Cast justification: display-only; above 2^53 bytes the rounding is far below the grid.
    bytes as f64 / GIB
}

/// Rounds a GiB value onto the 0.5 GiB grid, never below one step.
#[must_use]
pub fn snap_to_half_gib(gib: f64) -> f64 {
    if !gib.is_finite() {
        return STEP_GIB;
    }
    ((gib / STEP_GIB).round() * STEP_GIB).max(STEP_GIB)
}

/// One row editing an optional byte cap: caption, "no limit" checkbox, GiB spin box.
///
/// `max_bytes` bounds the spin box (the device or host total when the caller knows it) and
/// is also where a freshly enabled cap starts; without it the cap starts at 8 GiB and the
/// range ends at 1024 GiB. `id_salt` namespaces the row, so two editors in one panel keep
/// separate state whatever their captions are.
pub struct ByteCapEdit<'a> {
    value: &'a mut Option<u64>,
    id_salt: &'a str,
    label: &'a str,
    unlimited_label: &'a str,
    unit_suffix: &'a str,
    max_bytes: Option<u64>,
}

impl<'a> ByteCapEdit<'a> {
    /// An editor over `value` (`None` = no cap). `unit_suffix` follows the number (e.g.
    /// " GiB"), localized by the caller.
    #[must_use]
    pub fn new(value: &'a mut Option<u64>, id_salt: &'a str, label: &'a str, unlimited_label: &'a str, unit_suffix: &'a str) -> Self {
        Self { value, id_salt, label, unlimited_label, unit_suffix, max_bytes: None }
    }

    /// Bounds the spin box at `max_bytes` (ignored when `None` or zero).
    #[must_use]
    pub fn max_bytes(mut self, max_bytes: Option<u64>) -> Self {
        self.max_bytes = max_bytes.filter(|bytes| *bytes > 0);
        self
    }
}

impl Widget for ByteCapEdit<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let Self { value, id_salt, label, unlimited_label, unit_suffix, max_bytes } = self;
        let max_gib = max_bytes.map_or(FALLBACK_MAX_GIB, |bytes| snap_to_half_gib(bytes_to_gib(bytes)));
        ui.push_id(id_salt, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut response = ui.label(label);
                let mut changed = false;
                let mut unlimited = value.is_none();
                let checkbox = ui.checkbox(&mut unlimited, unlimited_label);
                if checkbox.changed() {
                    *value = if unlimited {
                        None
                    } else {
                        Some(gib_to_bytes(DEFAULT_CAP_GIB.min(max_gib)))
                    };
                    changed = true;
                }
                response = response.union(checkbox);
                if let Some(bytes) = value.as_mut() {
                    let mut gib = snap_to_half_gib(bytes_to_gib(*bytes));
                    let spin = ui.add(
                        WheelSpinBox::new(&mut gib)
                            .range(STEP_GIB..=max_gib)
                            .speed(0.05)
                            .wheel_step(STEP_GIB)
                            .min_decimals(1)
                            .max_decimals(1)
                            .suffix(unit_suffix),
                    );
                    if spin.changed() {
                        let snapped = gib_to_bytes(snap_to_half_gib(gib));
                        if snapped != *bytes {
                            *bytes = snapped;
                            changed = true;
                        }
                    }
                    response = response.union(spin);
                }
                if changed {
                    response.mark_changed();
                }
                response
            })
            .inner
        })
        .inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_round_trip_on_the_grid() {
        for gib in [0.5, 1.0, 7.5, 24.0, 512.0] {
            assert!((bytes_to_gib(gib_to_bytes(gib)) - gib).abs() < 1e-9, "{gib}");
        }
        assert_eq!(gib_to_bytes(1.0), 1 << 30);
    }

    #[test]
    fn conversions_saturate_and_reject_nonsense() {
        assert_eq!(gib_to_bytes(f64::NAN), 0);
        assert_eq!(gib_to_bytes(-3.0), 0);
        assert_eq!(gib_to_bytes(0.0), 0);
        assert_eq!(gib_to_bytes(f64::INFINITY), 0);
        assert_eq!(gib_to_bytes(1.0e30), u64::MAX);
        assert!(bytes_to_gib(u64::MAX).is_finite());
    }

    #[test]
    fn values_snap_to_half_a_gib() {
        assert!((snap_to_half_gib(7.3) - 7.5).abs() < f64::EPSILON);
        assert!((snap_to_half_gib(7.2) - 7.0).abs() < f64::EPSILON);
        assert!((snap_to_half_gib(0.1) - 0.5).abs() < f64::EPSILON, "never below one step");
        assert!((snap_to_half_gib(f64::NAN) - 0.5).abs() < f64::EPSILON);
    }
}
