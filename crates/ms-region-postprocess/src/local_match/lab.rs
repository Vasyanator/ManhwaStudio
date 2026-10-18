/*
File: local_match/lab.rs

Purpose:
sRGB (u8) <-> CIELAB (D65, float32) conversion of the local colour match, a literal port of the
reference's `_srgb_u8_to_lab` / `_lab_to_srgb_u8` / `_mix_channels` numpy fallback
(`region_imaging.py` at `c11c4eb`).

Key structures:
- `LabTables`: the f32 constants and the 256-entry sRGB -> linear LUT, built once per match.

Key functions:
- `LabTables::to_lab()`, `LabTables::to_srgb()`.

Notes:
numpy semantics reproduced: the LUT is computed in f64 and cast to f32; every Python float
constant meets a float32 array as a WEAK scalar (numpy 2), i.e. it is first rounded f64 -> f32;
the channel mix is three explicit f32 sums `(c0·m0 + c1·m1) + c2·m2` (no fused multiply-add);
the result is rounded half to even and clipped. numpy's own SIMD `cbrt` / `power` loops may differ
from libm by one ulp, which the parity tolerance absorbs.
*/

use super::grid::to_f32;

/// Linear sRGB -> XYZ (D65), rows X, Y, Z (reference `_SRGB_TO_XYZ`).
const SRGB_TO_XYZ: [[f64; 3]; 3] = [[0.412_456_4, 0.357_576_1, 0.180_437_5], [0.212_672_9, 0.715_152_2, 0.072_175_0], [0.019_333_9, 0.119_192_0, 0.950_304_1]];

/// XYZ (D65) -> linear sRGB, the inverse of `SRGB_TO_XYZ` (reference `_XYZ_TO_SRGB`).
const XYZ_TO_SRGB: [[f64; 3]; 3] = [[3.240_454_2, -1.537_138_5, -0.498_531_4], [-0.969_266_0, 1.876_010_8, 0.041_556_0], [0.055_643_4, -0.204_025_9, 1.057_225_2]];

/// D65 reference white (reference `_D65_WHITE`).
const D65_WHITE: [f64; 3] = [0.950_47, 1.0, 1.088_83];

/// CIELAB's linear-segment threshold `6/29` (reference `_LAB_DELTA`).
const LAB_DELTA: f64 = 6.0 / 29.0;

/// The float32 constants and the sRGB decode LUT of one conversion.
#[derive(Debug)]
pub(super) struct LabTables {
    to_linear: [f32; 256],
    rgb_to_xyz_over_white: [[f32; 3]; 3],
    xyz_to_rgb: [[f32; 3]; 3],
    white: [f32; 3],
    delta: f32,
    delta_cubed: f32,
    linear_slope: f32,
    linear_offset: f32,
}

impl LabTables {
    /// Builds the tables exactly as the reference computes them (see the file notes).
    pub(super) fn new() -> Self {
        let mut to_linear = [0.0_f32; 256];
        for (level, value) in (0_u8..=255).zip(to_linear.iter_mut()) {
            let encoded = f64::from(level) / 255.0;
            let linear = if encoded <= 0.04045 { encoded / 12.92 } else { ((encoded + 0.055) / 1.055).powf(2.4) };
            *value = to_f32(linear);
        }
        let white = D65_WHITE.map(to_f32);
        let mut rgb_to_xyz_over_white = [[0.0_f32; 3]; 3];
        for ((out_row, row), &white_value) in rgb_to_xyz_over_white.iter_mut().zip(&SRGB_TO_XYZ).zip(&white) {
            for (out, &coefficient) in out_row.iter_mut().zip(row) {
                *out = to_f32(coefficient) / white_value;
            }
        }
        Self {
            to_linear,
            rgb_to_xyz_over_white,
            xyz_to_rgb: XYZ_TO_SRGB.map(|row| row.map(to_f32)),
            white,
            delta: to_f32(LAB_DELTA),
            delta_cubed: to_f32(LAB_DELTA.powf(3.0)),
            linear_slope: to_f32(3.0 * LAB_DELTA * LAB_DELTA),
            linear_offset: to_f32(4.0 / 29.0),
        }
    }

    /// One sRGB u8 pixel -> CIELAB `[L, a, b]` (L in 0..100), float32.
    pub(super) fn to_lab(&self, rgb: [u8; 3]) -> [f32; 3] {
        let linear = rgb.map(|level| self.to_linear[usize::from(level)]);
        let xyz = mix(linear, &self.rgb_to_xyz_over_white);
        let f = xyz.map(|value| if value > self.delta_cubed { value.max(0.0).cbrt() } else { value / self.linear_slope + self.linear_offset });
        [116.0 * f[1] - 16.0, 500.0 * (f[0] - f[1]), 200.0 * (f[1] - f[2])]
    }

    /// One CIELAB pixel -> sRGB u8, clipped to the gamut and rounded half to even.
    pub(super) fn to_srgb(&self, lab: [f32; 3]) -> [u8; 3] {
        let fy = (lab[0] + 16.0) / 116.0;
        let f = [fy + lab[1] / 500.0, fy, fy - lab[2] / 200.0];
        let mut xyz = f.map(|value| if value > self.delta { value * value * value } else { self.linear_slope * (value - self.linear_offset) });
        for (value, &white) in xyz.iter_mut().zip(&self.white) {
            *value *= white;
        }
        let linear = mix(xyz, &self.xyz_to_rgb).map(|value| value.clamp(0.0, 1.0));
        linear.map(|value| {
            let encoded = if value <= to_f32(0.003_130_8) { to_f32(12.92) * value } else { to_f32(1.055) * value.powf(to_f32(1.0 / 2.4)) - to_f32(0.055) };
            round_to_u8(encoded * 255.0)
        })
    }
}

/// `out[i] = (m[i][0]·c0 + m[i][1]·c1) + m[i][2]·c2` in f32, the reference's numpy channel mix.
fn mix(channels: [f32; 3], matrix: &[[f32; 3]; 3]) -> [f32; 3] {
    matrix.map(|row| channels[0] * row[0] + channels[1] * row[1] + channels[2] * row[2])
}

/// numpy `clip(rint(value), 0, 255).astype(uint8)`: round half to even, then clamp.
fn round_to_u8(value: f32) -> u8 {
    let rounded = value.round_ties_even().clamp(0.0, 255.0);
    // Clamped to 0..=255 and integral just above (a NaN would saturate to 0, and none occurs:
    // every input is finite and clipped to the gamut first).
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to the u8 range and integral just above")]
    let level = rounded as u8;
    level
}
