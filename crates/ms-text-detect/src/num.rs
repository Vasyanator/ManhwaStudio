/*
File: crates/ms-text-detect/src/num.rs

Purpose:
The few unavoidable float <-> integer conversions of the DB postprocess, the glyph mask, the
detection plan and the stitcher, centralized so every cast carries its range argument once.

Key functions:
- u32_to_f32       : image dimensions / pixel counts to `f32`.
- f32_from_i32     : small pixel coordinates to `f32`.
- f32_to_i32_trunc : truncation toward zero with saturation (Python `int(...)`).
- f64_round_to_u32 : checked rounding of plan sizes and percentages.
- idx              : `u32` -> `usize` for buffer indexing (lossless, see the const assertion).

Notes:
Crate-private. `ms-onnx` keeps its own `u32_to_f32` for its preprocessing; the two are tiny,
identical and live on opposite sides of a crate boundary that must not grow a numeric API.
*/

/// Lossless-in-practice `u32` -> `f32` for image dimensions and pixel counts.
///
/// All call sites pass image sizes / counts far below f32's 2^24 exact-integer
/// limit, so no precision is lost in practice.
#[must_use]
pub(crate) fn u32_to_f32(value: u32) -> f32 {
    // f32 cannot represent every u32 exactly, but our values stay < 2^24.
    #[allow(clippy::cast_precision_loss)]
    let out = value as f32;
    out
}

/// Lossless `i32` -> `f32` for small pixel coordinates (well within 2^24).
#[must_use]
pub(crate) fn f32_from_i32(v: i32) -> f32 {
    // Pixel coordinates are far below f32's 2^24 exact-integer limit.
    #[allow(clippy::cast_precision_loss)]
    let out = v as f32;
    out
}

/// Truncates a finite `f32` to `i32`, saturating out-of-range input.
///
/// Matches `NumPy`'s `astype(np.int32)` / Python `int(...)` truncation toward zero.
/// NaN maps to 0.
#[must_use]
pub(crate) fn f32_to_i32_trunc(value: f32) -> i32 {
    if value.is_nan() {
        return 0;
    }
    let capped = value.clamp(i32_min_as_f32(), i32_max_as_f32());
    // Safe: `capped` is finite and within i32 range; truncation drops the fraction.
    #[allow(clippy::cast_possible_truncation)]
    let out = capped as i32;
    out
}

/// Rounds a finite, non-negative `f64` to the nearest `u32` (half away from zero).
///
/// Returns `None` for NaN, infinities, negative values and values above `u32::MAX`.
#[must_use]
pub(crate) fn f64_round_to_u32(value: f64) -> Option<u32> {
    let rounded = value.round();
    if !rounded.is_finite() || rounded < 0.0 || rounded > f64::from(u32::MAX) {
        return None;
    }
    // Safe: `rounded` is an integer-valued float inside `0..=u32::MAX`, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = rounded as u32;
    Some(out)
}

// Every supported target (64-bit desktop, wasm32) has a `usize` of at least 32 bits, which is
// what makes `idx` lossless.
const _: () = assert!(usize::BITS >= u32::BITS);

/// Lossless `u32` -> `usize` for buffer indexing (see the `usize::BITS` assertion above).
#[must_use]
pub(crate) fn idx(value: u32) -> usize {
    value as usize
}

/// `i32::MIN` as `f32` (lower clamp bound for [`f32_to_i32_trunc`]).
fn i32_min_as_f32() -> f32 {
    // Rounding of the bound to the nearest f32 is irrelevant for a saturating clamp.
    #[allow(clippy::cast_precision_loss)]
    let out = i32::MIN as f32;
    out
}

/// `i32::MAX` as `f32` (upper clamp bound for [`f32_to_i32_trunc`]).
fn i32_max_as_f32() -> f32 {
    // Rounding of the bound to the nearest f32 is irrelevant for a saturating clamp.
    #[allow(clippy::cast_precision_loss)]
    let out = i32::MAX as f32;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_saturate_and_truncate() {
        assert_eq!(f32_to_i32_trunc(-2.7), -2);
        assert_eq!(f32_to_i32_trunc(2.7), 2);
        assert_eq!(f32_to_i32_trunc(f32::NAN), 0);
        assert_eq!(f32_to_i32_trunc(1e12), i32::MAX);
        assert_eq!(f32_to_i32_trunc(-1e12), i32::MIN);
    }

    #[test]
    fn f64_rounding_is_checked() {
        assert_eq!(f64_round_to_u32(2.5), Some(3));
        assert_eq!(f64_round_to_u32(2.49), Some(2));
        assert_eq!(f64_round_to_u32(-0.4), Some(0));
        assert_eq!(f64_round_to_u32(-0.6), None);
        assert_eq!(f64_round_to_u32(f64::NAN), None);
        assert_eq!(f64_round_to_u32(f64::INFINITY), None);
        assert_eq!(f64_round_to_u32(f64::from(u32::MAX)), Some(u32::MAX));
        assert_eq!(f64_round_to_u32(f64::from(u32::MAX) + 1.0), None);
        assert_eq!(idx(u32::MAX), 4_294_967_295_usize);
    }
}
