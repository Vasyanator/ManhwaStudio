/*
File: tab/geometry.rs

Purpose:
Pure coordinate and scalar math helpers shared across the typing tab submodules
(page/scene/UV conversions, angle normalization, clamping). No `self`, no state.

Notes:
Extracted verbatim from `tab.rs`. Items are `pub(super)` so sibling submodules of
`tab` can use them.
*/

/// Normalizes a radian angle into the `(-PI, PI]` range.
pub(super) fn normalize_angle_rad(angle: f32) -> f32 {
    let two_pi = std::f32::consts::TAU;
    ((angle + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI
}

/// Normalizes a degree angle into the `(-180, 180]` range.
pub(super) fn normalize_angle_deg(angle: f32) -> f32 {
    ((angle + 180.0).rem_euclid(360.0)) - 180.0
}

/// Linear interpolation between `a` and `b` with `t` clamped to `[0, 1]`.
pub(super) fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

/// Degrees one wheel notch of `typing.layer.rotate` turns the selected layer (overlay or raster).
pub(super) const WHEEL_ROTATION_STEP_DEG: f32 = 2.0;

/// Wheel rotation for `notches` (signed, continuous — the axis sample's `delta`), in DEGREES:
/// ±2° per notch for overlays and rasters alike. `None` for a zero or non-finite delta (nothing to
/// rotate). A raster adds it, in radians, un-normalized, to `transform.rotation` exactly as its
/// drag-rotate does (`apply_raster_drag` `Rotate`), so wheel and drag stay consistent.
pub(super) fn wheel_rotation_step_deg(notches: f32) -> Option<f32> {
    (notches.is_finite() && notches.abs() > f32::EPSILON).then_some(notches * WHEEL_ROTATION_STEP_DEG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_rotation_is_two_degrees_per_notch_and_continuous() {
        assert!(matches!(wheel_rotation_step_deg(1.0), Some(v) if (v - 2.0).abs() < 1e-6));
        assert!(matches!(wheel_rotation_step_deg(-3.0), Some(v) if (v + 6.0).abs() < 1e-6));
        // A trackpad's 10-point event is a quarter notch: half a degree.
        assert!(matches!(wheel_rotation_step_deg(0.25), Some(v) if (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn wheel_rotation_zero_or_non_finite_is_none() {
        assert_eq!(wheel_rotation_step_deg(0.0), None);
        assert_eq!(wheel_rotation_step_deg(f32::NAN), None);
    }
}
