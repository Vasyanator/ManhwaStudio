/*
File: crates/ms-text-detect/src/mask.rs

Purpose:
Detector mask normalization with one owner: a decoded grayscale detector mask becomes a 0/255
binary alpha mask, refusing masks above the pixel guard.

Key items:
- `MAX_MASK_PIXELS`: the largest detector mask any path accepts (100 M pixels).
- `exceeds_pixel_limit()`: the guard as a predicate, for callers that check before allocating.
- `normalize_binary_alpha()`: in-place 0 -> 0, nonzero -> 255.
- `binary_alpha_from_gray()` / `BinaryMask` / `MaskError`: guard + normalization in one call.

Notes:
Detector masks are logically binary, but engines encode them as 1-bit, 8-bit or 16-bit PNGs
or as raw glyph masks; normalizing to 0/255 keeps every consumer (display, cleaning tools,
storage) on one contract.
*/

use image::GrayImage;

/// Largest detector mask, in pixels, that any detector path accepts or produces.
pub const MAX_MASK_PIXELS: usize = 100_000_000;

/// A binary detector mask: `alpha` is row-major, `size[0] * size[1]` bytes, each 0 or 255.
/// A zero-area mask is `size == [0, 0]` with an empty `alpha`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryMask {
    /// `[width, height]` in pixels.
    pub size: [u32; 2],
    /// One byte per pixel, 0 (background) or 255 (text).
    pub alpha: Vec<u8>,
}

/// Errors of the mask normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MaskError {
    /// The mask has more than [`MAX_MASK_PIXELS`] pixels.
    #[error("detector mask {width}x{height} exceeds the {MAX_MASK_PIXELS}-pixel limit")]
    TooLarge {
        /// Mask width in pixels.
        width: u32,
        /// Mask height in pixels.
        height: u32,
    },
}

/// Whether a `width x height` mask has more than [`MAX_MASK_PIXELS`] pixels.
#[must_use]
pub fn exceeds_pixel_limit(width: u32, height: u32) -> bool {
    // u32 * u32 always fits u64; a product that does not fit `usize` (32-bit targets) is
    // over the limit by definition.
    usize::try_from(u64::from(width) * u64::from(height)).map_or(true, |pixels| pixels > MAX_MASK_PIXELS)
}

/// Normalizes a mask in place: 0 stays 0, every nonzero value becomes 255.
pub fn normalize_binary_alpha(alpha: &mut [u8]) {
    for px in alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
}

/// Turns a decoded grayscale detector mask into a [`BinaryMask`] (consumes the buffer, no copy).
///
/// A mask with zero width or height yields the empty mask (`[0, 0]`, no bytes), never an error.
///
/// # Errors
/// [`MaskError::TooLarge`] when the mask has more than [`MAX_MASK_PIXELS`] pixels.
pub fn binary_alpha_from_gray(gray: GrayImage) -> Result<BinaryMask, MaskError> {
    let (width, height) = gray.dimensions();
    if width == 0 || height == 0 {
        return Ok(BinaryMask { size: [0, 0], alpha: Vec::new() });
    }
    if exceeds_pixel_limit(width, height) {
        return Err(MaskError::TooLarge { width, height });
    }
    let mut alpha = gray.into_raw();
    normalize_binary_alpha(&mut alpha);
    Ok(BinaryMask { size: [width, height], alpha })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_every_nonzero_level_to_255() {
        let gray = GrayImage::from_fn(4, 2, |x, y| image::Luma([[0u8, 1, 127, 254, 255, 0, 2, 0][usize::try_from(y * 4 + x).unwrap_or(0)]]));
        let mask = binary_alpha_from_gray(gray).unwrap_or_else(|err| panic!("in-bounds mask: {err}"));
        assert_eq!(mask.size, [4, 2]);
        assert_eq!(mask.alpha, vec![0, 255, 255, 255, 255, 0, 255, 0]);
    }

    #[test]
    fn zero_area_mask_is_empty_not_an_error() {
        for (width, height) in [(0, 0), (5, 0), (0, 5)] {
            let mask = binary_alpha_from_gray(GrayImage::new(width, height)).unwrap_or_else(|err| panic!("empty mask: {err}"));
            assert_eq!(mask, BinaryMask { size: [0, 0], alpha: Vec::new() });
        }
    }

    #[test]
    fn pixel_guard_accepts_the_bound_and_rejects_above_it() {
        assert!(!exceeds_pixel_limit(10_000, 10_000));
        assert!(exceeds_pixel_limit(10_001, 10_000));
        assert!(exceeds_pixel_limit(u32::MAX, u32::MAX));
        assert!(!exceeds_pixel_limit(0, u32::MAX));
        let err = binary_alpha_from_gray(GrayImage::new(10_001, 10_000)).expect_err("oversize mask");
        assert_eq!(err, MaskError::TooLarge { width: 10_001, height: 10_000 });
        let mask = binary_alpha_from_gray(GrayImage::new(10_000, 10_000)).unwrap_or_else(|err| panic!("mask at the bound: {err}"));
        assert_eq!(mask.size, [10_000, 10_000]);
        assert_eq!(mask.alpha.len(), MAX_MASK_PIXELS);
    }
}
