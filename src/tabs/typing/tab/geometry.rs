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
