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

/// Ctrl/Cmd+wheel rotation step for a RASTER layer, in RADIANS, matching the ±2°/notch of the
/// text-overlay Ctrl+wheel rotate. Returns `None` when the raw wheel delta is effectively zero (no
/// rotation). The sign follows `scroll_delta_y` (wheel up → positive); the result is intentionally
/// NOT normalized because a raster stores its rotation in radians and the drag-rotate path
/// (`apply_raster_drag` `Rotate`) also lets `transform.rotation` grow unbounded, so wheel and drag
/// stay consistent.
pub(super) fn ctrl_wheel_raster_rotation_step_rad(scroll_delta_y: f32) -> Option<f32> {
    if scroll_delta_y.abs() <= f32::EPSILON {
        return None;
    }
    let steps = if scroll_delta_y > 0.0 { 1.0 } else { -1.0 };
    Some((steps * 2.0_f32).to_radians())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_wheel_raster_step_matches_overlay_two_degrees() {
        // Wheel up → +2°, wheel down → -2°, both expressed in radians (raster rotation unit).
        let up = ctrl_wheel_raster_rotation_step_rad(1.0);
        let down = ctrl_wheel_raster_rotation_step_rad(-1.0);
        assert!(matches!(up, Some(v) if (v - 2.0_f32.to_radians()).abs() < 1e-6));
        assert!(matches!(down, Some(v) if (v + 2.0_f32.to_radians()).abs() < 1e-6));
        // Magnitude is independent of the raw delta size (per-notch step, not proportional).
        assert_eq!(ctrl_wheel_raster_rotation_step_rad(37.5), up);
    }

    #[test]
    fn ctrl_wheel_raster_step_zero_delta_is_none() {
        assert_eq!(ctrl_wheel_raster_rotation_step_rad(0.0), None);
        assert_eq!(ctrl_wheel_raster_rotation_step_rad(f32::EPSILON), None);
    }
}
