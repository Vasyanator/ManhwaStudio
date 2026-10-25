/*
File: crates/ms-region-postprocess/src/lab.rs

Purpose:
The project's one sRGB (u8) <-> CIELAB (D65, float32) conversion: a literal port of the region
post-processing reference's `_srgb_u8_to_lab` / `_lab_to_srgb_u8` / `_mix_channels` numpy
fallback (`region_imaging.py` at `c11c4eb`), used by the local colour match of this crate and by
every other CIELAB consumer (the cleaning tab's gradient fill).

Key structures:
- `LabTables`: the f32 constants and the 256-entry sRGB -> linear LUT, built once per run.

Key functions:
- `LabTables::to_lab()`: sRGB u8 -> Lab.
- `LabTables::to_srgb()`: Lab -> sRGB u8 (gamut-clipped, rounded half to even).
- `LabTables::to_linear_rgb()` + `encode_srgb()`: the two halves of `to_srgb` before its clip
  and quantization, for a caller that quantizes itself (dither, its own rounding).

Notes:
numpy semantics reproduced: the LUT is computed in f64 and cast to f32; every Python float
constant meets a float32 array as a WEAK scalar (numpy 2), i.e. it is first rounded f64 -> f32;
the channel mix is three explicit f32 sums `(c0·m0 + c1·m1) + c2·m2` (no fused multiply-add);
the result is rounded half to even and clipped. numpy's own SIMD `cbrt` / `power` loops may differ
from libm by one ulp, which the parity tolerance absorbs. The bit pattern of every output is
pinned by the colour-match parity fixtures, so a change here is a fixture change.
*/

/// Linear sRGB -> XYZ (D65), rows X, Y, Z (reference `_SRGB_TO_XYZ`).
const SRGB_TO_XYZ: [[f64; 3]; 3] = [[0.412_456_4, 0.357_576_1, 0.180_437_5], [0.212_672_9, 0.715_152_2, 0.072_175_0], [0.019_333_9, 0.119_192_0, 0.950_304_1]];

/// XYZ (D65) -> linear sRGB, the inverse of `SRGB_TO_XYZ` (reference `_XYZ_TO_SRGB`).
const XYZ_TO_SRGB: [[f64; 3]; 3] = [[3.240_454_2, -1.537_138_5, -0.498_531_4], [-0.969_266_0, 1.876_010_8, 0.041_556_0], [0.055_643_4, -0.204_025_9, 1.057_225_2]];

/// D65 reference white (reference `_D65_WHITE`).
const D65_WHITE: [f64; 3] = [0.950_47, 1.0, 1.088_83];

/// CIELAB's linear-segment threshold `6/29` (reference `_LAB_DELTA`).
const LAB_DELTA: f64 = 6.0 / 29.0;

/// The float32 constants and the sRGB decode LUT of one conversion. Cheap to build (256 `powf`
/// in f64); immutable afterwards, so one instance may serve many threads.
#[derive(Debug)]
pub struct LabTables {
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
    #[must_use]
    pub fn new() -> Self {
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
    #[must_use]
    pub fn to_lab(&self, rgb: [u8; 3]) -> [f32; 3] {
        let linear = rgb.map(|level| self.to_linear[usize::from(level)]);
        let xyz = mix(linear, &self.rgb_to_xyz_over_white);
        let f = xyz.map(|value| if value > self.delta_cubed { value.max(0.0).cbrt() } else { value / self.linear_slope + self.linear_offset });
        [116.0 * f[1] - 16.0, 500.0 * (f[0] - f[1]), 200.0 * (f[1] - f[2])]
    }

    /// One CIELAB pixel -> sRGB u8, clipped to the gamut and rounded half to even.
    #[must_use]
    pub fn to_srgb(&self, lab: [f32; 3]) -> [u8; 3] {
        let linear = self.to_linear_rgb(lab).map(|value| value.clamp(0.0, 1.0));
        linear.map(|value| round_to_u8(encode_srgb(value) * 255.0))
    }

    /// One CIELAB pixel -> LINEAR sRGB `[r, g, b]`, NOT clipped: an out-of-gamut colour yields
    /// values below 0 or above 1. `to_srgb` is `clamp(0, 1)` of this, then `encode_srgb`, then
    /// half-to-even rounding of `x * 255`.
    #[must_use]
    pub fn to_linear_rgb(&self, lab: [f32; 3]) -> [f32; 3] {
        let fy = (lab[0] + 16.0) / 116.0;
        let f = [fy + lab[1] / 500.0, fy, fy - lab[2] / 200.0];
        let mut xyz = f.map(|value| if value > self.delta { value * value * value } else { self.linear_slope * (value - self.linear_offset) });
        for (value, &white) in xyz.iter_mut().zip(&self.white) {
            *value *= white;
        }
        mix(xyz, &self.xyz_to_rgb)
    }
}

impl Default for LabTables {
    fn default() -> Self {
        Self::new()
    }
}

/// sRGB transfer function (the reference's encode step, f32 weak-scalar constants): a linear
/// value `>= 0` -> its encoded value, `1.0 -> 1.0`. Defined for any finite input; a caller
/// clamps first (`to_srgb` clips to `0..=1`, a caller that dithers may clip only below 0).
#[must_use]
pub fn encode_srgb(linear: f32) -> f32 {
    if linear <= to_f32(0.003_130_8) { to_f32(12.92) * linear } else { to_f32(1.055) * linear.powf(to_f32(1.0 / 2.4)) - to_f32(0.055) }
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

/// numpy `astype(float32)` of a float64: round to nearest.
#[expect(clippy::cast_possible_truncation, reason = "f64 -> f32 rounding to nearest IS the numpy float32 cast this port reproduces")]
fn to_f32(value: f64) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `to_srgb` is exactly the documented composition of its public halves, so a caller that
    /// quantizes itself starts from the same linear values.
    #[test]
    fn to_srgb_is_clip_encode_round_of_to_linear_rgb() {
        let tables = LabTables::new();
        for lab in [[0.0, 0.0, 0.0], [53.2, 80.1, 67.2], [100.0, 0.0, 0.0], [62.5, -40.0, 90.0], [30.0, 70.0, -110.0]] {
            let composed = tables.to_linear_rgb(lab).map(|v| round_to_u8(encode_srgb(v.clamp(0.0, 1.0)) * 255.0));
            assert_eq!(tables.to_srgb(lab), composed, "lab {lab:?}");
        }
    }

    /// Out-of-gamut colours stay out of gamut in `to_linear_rgb` (no hidden clip), while
    /// `to_srgb` clips them; the primaries and grey levels round-trip exactly.
    #[test]
    fn linear_is_unclipped_and_primaries_round_trip() {
        let tables = LabTables::new();
        let linear = tables.to_linear_rgb([62.5, -40.0, 90.0]);
        assert!(linear.iter().any(|&v| !(0.0..=1.0).contains(&v)), "{linear:?}");
        for rgb in [[0, 0, 0], [255, 255, 255], [255, 0, 0], [0, 255, 0], [0, 0, 255], [128, 128, 128]] {
            assert_eq!(tables.to_srgb(tables.to_lab(rgb)), rgb, "rgb {rgb:?}");
        }
        assert!((encode_srgb(1.0) - 1.0).abs() < 1e-6);
        assert_eq!(encode_srgb(0.0).to_bits(), 0);
    }
}
