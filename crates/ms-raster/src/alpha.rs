/*
File: crates/ms-raster/src/alpha.rs

Purpose:
The project's flattening of straight (un-premultiplied) RGBA8 onto an opaque WHITE backdrop, for
every output format that has no alpha channel (the typing tab's PDF image streams, JPEG saves).

Key functions:
- `rgba_over_white_to_rgb()`: appends the RGB8 composite of an RGBA8 span to a buffer.
- `component_over_white()` (private): the per-component rule and its rounding.

Notes:
White because a page without transparency shows unpainted area as paper: compositing over black
or dropping alpha would leave a dark fringe on every antialiased edge. The rounding (`+ 127`,
then integer division by 255) is part of the contract: exported bytes must not change when a
caller moves onto this owner.
*/

use crate::RasterError;

/// Bytes per interleaved RGBA8 pixel.
const RGBA_CHANNELS: usize = 4;

/// Composites the straight-RGBA8 pixels of `rgba` over opaque white and APPENDS them to `dst` as
/// RGB8, three bytes per input pixel, in input order. Existing bytes of `dst` are kept, so a
/// caller can stream a page row by row through one reused buffer.
///
/// Each component becomes `round(c * a / 255 + 255 * (255 - a) / 255)` in integer arithmetic
/// (`(c * a + 255 * (255 - a) + 127) / 255`): `a == 255` passes `c` through, `a == 0` yields 255.
///
/// # Errors
/// `RasterError::NotWholePixels` when `rgba.len()` is not a multiple of 4; `dst` is then left
/// unchanged.
pub fn rgba_over_white_to_rgb(rgba: &[u8], dst: &mut Vec<u8>) -> Result<(), RasterError> {
    if !rgba.len().is_multiple_of(RGBA_CHANNELS) {
        return Err(RasterError::NotWholePixels { len: rgba.len(), channels: RGBA_CHANNELS });
    }
    dst.reserve(rgba.len() / RGBA_CHANNELS * 3);
    for pixel in rgba.chunks_exact(RGBA_CHANNELS) {
        // Indexing is safe: `chunks_exact(4)` yields slices of exactly four bytes.
        let alpha = pixel[3];
        dst.push(component_over_white(pixel[0], alpha));
        dst.push(component_over_white(pixel[1], alpha));
        dst.push(component_over_white(pixel[2], alpha));
    }
    Ok(())
}

/// Composites one 8-bit colour component with straight `alpha` over white, rounded to nearest.
#[must_use]
fn component_over_white(component: u8, alpha: u8) -> u8 {
    let alpha = u32::from(alpha);
    // Largest possible numerator is 255 * 255 + 127, so u32 cannot overflow; `+ 127` makes the
    // division by 255 round to nearest instead of truncating.
    let weighted = u32::from(component) * alpha + 255 * (255 - alpha);
    let rounded = (weighted + 127) / 255;
    // `rounded` is provably <= 255 (the numerator is at most 255 * 255 + 127); the fallback only
    // keeps this path panic-free.
    u8::try_from(rounded).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-move formula from the typing tab's PDF writer, copied verbatim as the reference
    /// the single owner must reproduce bit for bit.
    fn reference_component_over_white(component: u8, alpha: u8) -> u8 {
        let alpha = u32::from(alpha);
        let weighted = u32::from(component) * alpha + 255 * (255 - alpha);
        let rounded = (weighted + 127) / 255;
        u8::try_from(rounded).unwrap_or(u8::MAX)
    }

    /// Runs the public entry point on one pixel and returns its three RGB bytes.
    fn one_pixel(rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        assert_eq!(rgba_over_white_to_rgb(&rgba, &mut out), Ok(()));
        out
    }

    #[test]
    fn every_component_and_alpha_matches_the_reference_formula() {
        for alpha in 0..=u8::MAX {
            for component in 0..=u8::MAX {
                let expected = reference_component_over_white(component, alpha);
                assert_eq!(component_over_white(component, alpha), expected, "component {component}, alpha {alpha}");
                // Through the public entry point too, with the component in each colour slot.
                assert_eq!(one_pixel([component, 0, 255, alpha])[0], expected);
                assert_eq!(one_pixel([0, component, 255, alpha])[1], expected);
                assert_eq!(one_pixel([255, 0, component, alpha])[2], expected);
            }
        }
    }

    #[test]
    fn alpha_is_composited_over_white() {
        // Opaque pixels pass through untouched.
        assert_eq!(one_pixel([10, 20, 30, 255]), vec![10, 20, 30]);
        // Fully transparent pixels become paper white.
        assert_eq!(one_pixel([10, 20, 30, 0]), vec![255, 255, 255]);
        // Half-transparent red: red stays saturated, the other channels rise halfway to white.
        assert_eq!(one_pixel([255, 0, 0, 128]), vec![255, 127, 127]);
        // Two pixels in one call keep their order.
        let mut pair = Vec::new();
        assert_eq!(rgba_over_white_to_rgb(&[0, 0, 0, 255, 255, 255, 255, 0], &mut pair), Ok(()));
        assert_eq!(pair, vec![0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn output_is_appended_after_existing_bytes() {
        let mut dst = vec![1, 2];
        assert_eq!(rgba_over_white_to_rgb(&[9, 8, 7, 255], &mut dst), Ok(()));
        assert_eq!(dst, vec![1, 2, 9, 8, 7]);
        // An empty span is a valid whole number of pixels and appends nothing.
        assert_eq!(rgba_over_white_to_rgb(&[], &mut dst), Ok(()));
        assert_eq!(dst, vec![1, 2, 9, 8, 7]);
    }

    #[test]
    fn partial_pixel_is_rejected_and_leaves_dst_unchanged() {
        let mut dst = vec![42];
        assert_eq!(rgba_over_white_to_rgb(&[1, 2, 3, 4, 5], &mut dst), Err(RasterError::NotWholePixels { len: 5, channels: 4 }));
        assert_eq!(dst, vec![42]);
    }
}
