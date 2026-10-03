/*
File: crates/ms-text-detect/src/ctd/resize.rs

Purpose:
Port of OpenCV's 8-bit single-channel `cv2.resize(..., INTER_LINEAR)`, the resize CTD applies to
its segmentation map (`ctd/inference.py:263`) before the mask refinement. `image`'s Triangle
filter is not a substitute: it antialiases on downscale and rounds differently.

Key functions:
- `resize_linear_u8()`: the resize.

Notes:
OpenCV 4.x generic fixed-point path (`imgproc/src/resize.cpp`, `resizeGeneric_` with
`HResizeLinear` / `VResizeLinear`):
- source coordinate `f = (float)((d + 0.5) * scale - 0.5)` with `scale = 1 / (dst / src)` in
  double, `s = floor(f)`, fraction `f - s`; `s < 0` and `s >= len - 1` clamp the tap to the edge
  with fraction 0;
- both tap weights are `saturate_cast<short>(w * 2048)` rounded independently (half to even);
- the horizontal pass is exact `int` arithmetic; the vertical pass runs the 128-bit SIMD kernel
  `VResizeLinearVec_32s8u` (`((S0 >> 4) * b0 >> 16) + ((S1 >> 4) * b1 >> 16) + 2) >> 2`) over the
  leading columns and the scalar `FixedPtCast` (`(v + 2^21) >> 22`) over the tail. The two differ
  by one level on some inputs, so both are reproduced with the column split of the SIMD loops.
Not reproduced: the `INTER_AREA` substitution OpenCV makes for an exact 2x downscale.
*/

use crate::num::idx;

/// Weight scale of the fixed-point taps (`INTER_RESIZE_COEF_SCALE`, 11 bits).
const COEF_SCALE: f32 = 2048.0;
/// Lanes of a 128-bit `v_uint8` / `v_int16` register, which bound the SIMD column loops.
const U8_LANES: usize = 16;
/// See [`U8_LANES`].
const I16_LANES: usize = 8;

/// One output position: the left/top source tap and the two fixed-point weights.
#[derive(Debug, Clone, Copy)]
struct Tap {
    /// First source index; the second is `min(first + 1, len - 1)`.
    first: usize,
    /// Weight of the first tap (`round((1 - fraction) * 2048)`).
    w0: i32,
    /// Weight of the second tap (`round(fraction * 2048)`).
    w1: i32,
}

/// The taps of one axis, `src_len -> dst_len` (both non-zero).
fn axis_taps(src_len: usize, dst_len: usize) -> Vec<Tap> {
    // OpenCV computes `inv_scale = dst / src` and `scale = 1 / inv_scale` in double.
    let scale = 1.0 / (len_f64(dst_len) / len_f64(src_len));
    let last = src_len.saturating_sub(1);
    (0..dst_len)
        .map(|d| {
            let f = narrow_f32((len_f64(d) + 0.5) * scale - 0.5);
            let floor = f.floor();
            let mut fraction = f - floor;
            let first = if floor < 0.0 {
                fraction = 0.0;
                0
            } else {
                let s = f32_to_index(floor);
                if s >= last {
                    fraction = 0.0;
                    last
                } else {
                    s
                }
            };
            Tap { first, w0: weight((1.0 - fraction) * COEF_SCALE), w1: weight(fraction * COEF_SCALE) }
        })
        .collect()
}

/// Resizes a row-major 8-bit map of `src_w x src_h` to `dst_w x dst_h` like `cv2.resize` with
/// `INTER_LINEAR` (see the file header for the arithmetic). Returns an empty buffer when any size
/// is zero or `src.len()` does not match `src_w * src_h`.
pub(crate) fn resize_linear_u8(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (idx(src_w), idx(src_h), idx(dst_w), idx(dst_h));
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || sw.checked_mul(sh) != Some(src.len()) {
        return Vec::new();
    }
    let x_taps = axis_taps(sw, dw);
    let y_taps = axis_taps(sh, dh);
    let horizontal = |row: usize| -> Vec<i32> {
        let line = &src[row * sw..(row + 1) * sw];
        x_taps.iter().map(|t| i32::from(line[t.first]) * t.w0 + i32::from(line[(t.first + 1).min(sw - 1)]) * t.w1).collect()
    };
    let simd_end = simd_columns(dw);
    let mut out = vec![0_u8; dw * dh];
    for (dst_row, tap) in out.chunks_exact_mut(dw).zip(&y_taps) {
        let s0 = horizontal(tap.first);
        let s1 = horizontal((tap.first + 1).min(sh - 1));
        for (x, value) in dst_row.iter_mut().enumerate() {
            *value = if x < simd_end { vertical_simd(s0[x], s1[x], tap) } else { vertical_scalar(s0[x], s1[x], tap) };
        }
    }
    out
}

/// Columns covered by the SIMD loops of `VResizeLinearVec_32s8u`: steps of 16 while
/// `x <= width - 16`, then steps of 8 while `x < width - 8`.
fn simd_columns(width: usize) -> usize {
    let mut x = 0;
    while x + U8_LANES <= width {
        x += U8_LANES;
    }
    while x + I16_LANES < width {
        x += I16_LANES;
    }
    x
}

/// The SIMD vertical kernel: `>> 4`, 16-bit high multiply, `(sum + 2) >> 2`, saturate.
fn vertical_simd(s0: i32, s1: i32, tap: &Tap) -> u8 {
    let hi = |s: i32, w: i32| ((s >> 4) * w) >> 16;
    saturate_u8((hi(s0, tap.w0) + hi(s1, tap.w1) + 2) >> 2)
}

/// The scalar vertical kernel `FixedPtCast<int, uchar, 22>`.
fn vertical_scalar(s0: i32, s1: i32, tap: &Tap) -> u8 {
    saturate_u8((s0 * tap.w0 + s1 * tap.w1 + (1 << 21)) >> 22)
}

/// Clamps to `0..=255`.
fn saturate_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MAX)
}

/// `saturate_cast<short>(float)`: round half to even, saturate. Weights stay in `0..=2048`.
fn weight(value: f32) -> i32 {
    let rounded = value.round_ties_even().clamp(f32::from(i16::MIN), f32::from(i16::MAX));
    // Exact: an integral value inside the i16 range.
    #[expect(clippy::cast_possible_truncation, reason = "integral value clamped to the i16 range")]
    let out = rounded as i32;
    out
}

/// A map length as `f64` (lengths stay below 2^32, exact in `f64`).
fn len_f64(len: usize) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "map lengths stay far below 2^53")]
    let out = len as f64;
    out
}

/// The `(float)` cast `OpenCV` applies to the source coordinate.
fn narrow_f32(value: f64) -> f32 {
    #[expect(clippy::cast_possible_truncation, reason = "OpenCV narrows the coordinate to float on purpose")]
    let out = value as f32;
    out
}

/// A non-negative integral `f32` coordinate as an index (saturating).
fn f32_to_index(value: f32) -> usize {
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "non-negative integral coordinate; the caller clamps it to the map")]
    let out = value as usize;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_size_is_a_copy() {
        let src: Vec<u8> = (0..=255).cycle().take(37 * 11).collect();
        assert_eq!(resize_linear_u8(&src, 37, 11, 37, 11), src);
    }

    #[test]
    fn constant_maps_stay_constant_and_bad_input_is_empty() {
        let src = vec![201_u8; 30 * 20];
        assert!(resize_linear_u8(&src, 30, 20, 47, 13).iter().all(|&v| v == 201));
        assert!(resize_linear_u8(&src, 30, 21, 47, 13).is_empty());
        assert!(resize_linear_u8(&src, 30, 20, 0, 13).is_empty());
    }

    #[test]
    fn simd_column_split_matches_the_opencv_loops() {
        assert_eq!(simd_columns(7), 0);
        assert_eq!(simd_columns(9), 8);
        assert_eq!(simd_columns(16), 16);
        assert_eq!(simd_columns(24), 16);
        assert_eq!(simd_columns(25), 24);
        assert_eq!(simd_columns(160), 160);
    }
}
