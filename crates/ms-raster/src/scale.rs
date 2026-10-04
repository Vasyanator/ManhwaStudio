/*
File: crates/ms-raster/src/scale.rs

Purpose:
The project's integer-factor scaling of interleaved `u8` rasters: nearest (pixel replication)
upscale and box-average downscale, the exact inverse pair used around an external image edit
(send `k*W x k*H`, map the answer back to `W x H`).

Key functions:
- `upscale_replicate()`: every source pixel becomes a `factor x factor` block of itself.
- `downscale_box()`: every `factor x factor` block becomes its rounded channel-wise mean.

Notes:
Buffers are row-major, interleaved, `channels` bytes per pixel (1..=4: mask, gray, RGB, RGBA);
channels are treated independently, alpha is not premultiplied or special-cased. Because a
replicated block averages back to its own value, `downscale_box(upscale_replicate(x, k), k) == x`
bit for bit. `downscale_box` refuses sizes that are not multiples of `factor` instead of
silently cropping the remainder. Sizes use checked arithmetic; no public path panics.
*/

use crate::RasterError;

/// Largest supported number of interleaved channels per pixel (RGBA).
const MAX_CHANNELS: usize = 4;

/// Validates `channels` and `factor` and returns the row length in bytes of a `width`-pixel row,
/// after checking that `src.len() == width * height * channels`.
fn validate(src: &[u8], width: usize, height: usize, channels: usize, factor: usize) -> Result<usize, RasterError> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(RasterError::InvalidChannels { channels });
    }
    if factor == 0 {
        return Err(RasterError::InvalidFactor { factor });
    }
    let mismatch = || RasterError::InterleavedLengthMismatch { width, height, channels, len: src.len() };
    let row_len = width.checked_mul(channels).ok_or_else(mismatch)?;
    let len = row_len.checked_mul(height).ok_or_else(mismatch)?;
    if len != src.len() {
        return Err(mismatch());
    }
    Ok(row_len)
}

/// Upscales `src` (`width x height`, `channels` interleaved bytes per pixel) by the integer
/// `factor` with pixel replication: output pixel `(x, y)` equals source pixel
/// `(x / factor, y / factor)`. The result is `(width * factor) x (height * factor)` with the
/// same channel layout. `factor == 1` returns an identical copy; a zero-area input returns an
/// empty `Vec`.
///
/// # Errors
/// - `RasterError::InvalidChannels` when `channels` is not in `1..=4`.
/// - `RasterError::InvalidFactor` when `factor == 0`.
/// - `RasterError::InterleavedLengthMismatch` when `src.len() != width * height * channels`
///   (including when that product overflows `usize`).
/// - `RasterError::SizeOverflow` when the output size overflows `usize`.
pub fn upscale_replicate(src: &[u8], width: usize, height: usize, channels: usize, factor: usize) -> Result<Vec<u8>, RasterError> {
    let row_len = validate(src, width, height, channels, factor)?;
    let overflow = || RasterError::SizeOverflow { width, height, channels, factor };
    let out_row_len = row_len.checked_mul(factor).ok_or_else(overflow)?;
    let out_height = height.checked_mul(factor).ok_or_else(overflow)?;
    let out_len = out_row_len.checked_mul(out_height).ok_or_else(overflow)?;
    // Zero area: `chunks_exact(0)` would panic on a zero-width row, and there is nothing to copy.
    if out_len == 0 || factor == 1 {
        return Ok(src.to_vec());
    }

    let mut out = Vec::with_capacity(out_len);
    for src_row in src.chunks_exact(row_len) {
        // Build one widened row, then repeat it `factor - 1` more times from the output itself.
        let row_start = out.len();
        for pixel in src_row.chunks_exact(channels) {
            for _ in 0..factor {
                out.extend_from_slice(pixel);
            }
        }
        for _ in 1..factor {
            out.extend_from_within(row_start..row_start + out_row_len);
        }
    }
    Ok(out)
}

/// Downscales `src` (`width x height`, `channels` interleaved bytes per pixel) by the integer
/// `factor` with a box average: output pixel `(x, y)` channel `c` is the mean of channel `c`
/// over the source block `[x*factor, (x+1)*factor) x [y*factor, (y+1)*factor)`, rounded half up.
/// The result is `(width / factor) x (height / factor)`. `factor == 1` returns an identical
/// copy; a zero-area input returns an empty `Vec`. Exact inverse of [`upscale_replicate`].
///
/// # Errors
/// - `RasterError::InvalidChannels` when `channels` is not in `1..=4`.
/// - `RasterError::InvalidFactor` when `factor == 0`.
/// - `RasterError::InterleavedLengthMismatch` when `src.len() != width * height * channels`
///   (including when that product overflows `usize`).
/// - `RasterError::NotDivisible` when `width` or `height` is not a multiple of `factor`.
pub fn downscale_box(src: &[u8], width: usize, height: usize, channels: usize, factor: usize) -> Result<Vec<u8>, RasterError> {
    let row_len = validate(src, width, height, channels, factor)?;
    if !width.is_multiple_of(factor) || !height.is_multiple_of(factor) {
        return Err(RasterError::NotDivisible { width, height, factor });
    }
    if src.is_empty() || factor == 1 {
        return Ok(src.to_vec());
    }
    let overflow = || RasterError::SizeOverflow { width, height, channels, factor };
    let out_width = width / factor;
    let out_row_len = out_width * channels;
    // Nonzero area and divisibility give `factor <= width` and `factor <= height`, so
    // `factor * factor <= src.len()` and the block size fits; it is still checked, not assumed.
    let block = u64::try_from(factor.checked_mul(factor).ok_or_else(overflow)?).map_err(|_| overflow())?;
    let half = block / 2;

    let mut out = Vec::with_capacity(out_row_len * (height / factor));
    // One accumulator row: the sum of every channel over the block rows of the current output row.
    let mut sums = vec![0u64; out_row_len];
    for block_rows in src.chunks_exact(row_len * factor) {
        sums.fill(0);
        for src_row in block_rows.chunks_exact(row_len) {
            for (x, pixel) in src_row.chunks_exact(channels).enumerate() {
                let base = (x / factor) * channels;
                for (sum, &value) in sums[base..base + channels].iter_mut().zip(pixel) {
                    *sum += u64::from(value);
                }
            }
        }
        // A rounded mean of `u8` values is at most 255, so the saturating fallback never fires.
        out.extend(sums.iter().map(|&sum| u8::try_from((sum + half) / block).unwrap_or(u8::MAX)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes (LCG) so the round trip covers arbitrary content.
    fn noise(len: usize, mut state: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                u8::try_from(state >> 56).unwrap_or(0)
            })
            .collect()
    }

    #[test]
    fn round_trip_is_bit_exact_for_one_and_four_channels() {
        for channels in [1usize, 2, 3, 4] {
            for factor in [1usize, 2, 3, 4] {
                for &(width, height) in &[(1usize, 1usize), (5, 3), (3, 7), (16, 9)] {
                    let src = noise(width * height * channels, 0xabcd ^ u64::try_from(channels * 31 + factor).unwrap_or(0));
                    let up = upscale_replicate(&src, width, height, channels, factor).expect("valid shape");
                    assert_eq!(up.len(), width * factor * height * factor * channels);
                    let back = downscale_box(&up, width * factor, height * factor, channels, factor).expect("divisible shape");
                    assert_eq!(back, src, "channels {channels} factor {factor} size {width}x{height}");
                }
            }
        }
    }

    #[test]
    fn factor_one_is_identity() {
        let src = noise(4 * 3 * 4, 7);
        assert_eq!(upscale_replicate(&src, 4, 3, 4, 1).expect("valid"), src);
        assert_eq!(downscale_box(&src, 4, 3, 4, 1).expect("valid"), src);
        let mask = noise(4 * 3, 9);
        assert_eq!(upscale_replicate(&mask, 4, 3, 1, 1).expect("valid"), mask);
        assert_eq!(downscale_box(&mask, 4, 3, 1, 1).expect("valid"), mask);
    }

    #[test]
    fn upscale_replicates_pixels_into_blocks() {
        // 2x1 RGBA image: red, blue.
        let src = [255, 0, 0, 255, 0, 0, 255, 128];
        let up = upscale_replicate(&src, 2, 1, 4, 2).expect("valid");
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 128];
        let row: Vec<u8> = [red, red, blue, blue].concat();
        assert_eq!(up, [row.clone(), row].concat());
    }

    #[test]
    fn downscale_rounds_the_block_mean_half_up() {
        // Four 2x2 single-channel blocks laid out in a 8x2 strip: sums 3, 2, 1, 1020.
        let src = [0, 1, 0, 0, 0, 0, 255, 255, 1, 1, 1, 1, 0, 1, 255, 255];
        assert_eq!(downscale_box(&src, 8, 2, 1, 2).expect("valid"), [1, 1, 0, 255]);
    }

    #[test]
    fn downscale_keeps_channels_independent() {
        // One 2x2 block of 2-channel pixels: channel 0 averages to 10, channel 1 to 200.
        let src = [0, 200, 20, 200, 10, 200, 10, 200];
        assert_eq!(downscale_box(&src, 2, 2, 2, 2).expect("valid"), [10, 200]);
    }

    #[test]
    fn zero_area_returns_empty() {
        assert_eq!(upscale_replicate(&[], 0, 5, 4, 3).expect("valid"), Vec::<u8>::new());
        assert_eq!(upscale_replicate(&[], 5, 0, 1, 3).expect("valid"), Vec::<u8>::new());
        assert_eq!(downscale_box(&[], 0, 6, 4, 3).expect("valid"), Vec::<u8>::new());
    }

    #[test]
    fn shape_errors_are_typed() {
        let src = [0u8; 12];
        for channels in [0usize, 5] {
            assert_eq!(upscale_replicate(&src, 3, 1, channels, 2), Err(RasterError::InvalidChannels { channels }));
            assert_eq!(downscale_box(&src, 3, 1, channels, 2), Err(RasterError::InvalidChannels { channels }));
        }
        assert_eq!(upscale_replicate(&src, 3, 1, 4, 0), Err(RasterError::InvalidFactor { factor: 0 }));
        assert_eq!(downscale_box(&src, 3, 1, 4, 0), Err(RasterError::InvalidFactor { factor: 0 }));
        let mismatch = RasterError::InterleavedLengthMismatch { width: 2, height: 2, channels: 4, len: 12 };
        assert_eq!(upscale_replicate(&src, 2, 2, 4, 2), Err(mismatch.clone()));
        assert_eq!(downscale_box(&src, 2, 2, 4, 2), Err(mismatch));
        let overflowing = RasterError::InterleavedLengthMismatch { width: usize::MAX, height: 2, channels: 4, len: 12 };
        assert_eq!(upscale_replicate(&src, usize::MAX, 2, 4, 2), Err(overflowing));
        assert_eq!(downscale_box(&src, 3, 4, 1, 2), Err(RasterError::NotDivisible { width: 3, height: 4, factor: 2 }));
        assert_eq!(downscale_box(&src, 4, 3, 1, 2), Err(RasterError::NotDivisible { width: 4, height: 3, factor: 2 }));
    }

    #[test]
    fn upscale_output_overflow_is_an_error() {
        let width = usize::MAX / 2 + 1;
        assert_eq!(upscale_replicate(&[], width, 0, 1, 2), Err(RasterError::SizeOverflow { width, height: 0, channels: 1, factor: 2 }));
    }
}
