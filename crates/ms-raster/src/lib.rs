/*
File: crates/ms-raster/src/lib.rs

Purpose:
Crate root of `ms-raster`: generic raster primitives that the project implements exactly once.

Exports:
- `fill_polygon_spans`: even-odd scanline rasterization of a closed polygon into horizontal spans
  (the PS-editor lasso, the cleaning tools and the patch core; re-exported by `ms-tools` at its
  historical path `ms_tools::fill_polygon_spans`).
- `dilate_square`: square (Chebyshev) binary dilation with independent x/y radii, O(w*h) for any
  radius.
- `otsu_threshold`: the Otsu threshold of a `u8` sample set, `None` when no split exists.
- `upscale_replicate` / `downscale_box`: integer-factor pixel replication and box-average
  downscale of interleaved 1..=4-channel `u8` rasters; an exact (bit-for-bit) inverse pair.
- `box_blur_u8`: clamp-to-edge separable box blur of a single-channel `u8` plane, O(w*h).
- `rgba_over_white_to_rgb`: straight RGBA8 composited over opaque white into RGB8 (PDF image
  streams, JPEG saves).
- `RasterError`: the typed buffer-shape / parameter error.

Notes:
Level 0 crate: std + `thiserror` only, GUI-free and wasm-safe. Callers own output formats
(0/1, 0/255, in-place, `bool`) through thin adapters; the rules themselves live only here.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

mod alpha;
mod blur;
mod morph;
mod otsu;
mod polygon;
mod scale;

pub use alpha::rgba_over_white_to_rgb;
pub use blur::box_blur_u8;
pub use morph::dilate_square;
pub use otsu::otsu_threshold;
pub use polygon::fill_polygon_spans;
pub use scale::{downscale_box, upscale_replicate};

/// Errors of the buffer-taking primitives in this crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RasterError {
    /// `len` does not equal `width * height`. Also returned when `width * height` overflows
    /// `usize`, since no buffer can have that length.
    #[error("raster buffer length {len} does not match {width}x{height}")]
    LengthMismatch {
        /// Declared width in pixels.
        width: usize,
        /// Declared height in pixels.
        height: usize,
        /// Actual buffer length in elements.
        len: usize,
    },
    /// `len` does not equal `width * height * channels` for an interleaved buffer. Also returned
    /// when that product overflows `usize`.
    #[error("raster buffer length {len} does not match {width}x{height} with {channels} channels")]
    InterleavedLengthMismatch {
        /// Declared width in pixels.
        width: usize,
        /// Declared height in pixels.
        height: usize,
        /// Declared interleaved channels per pixel.
        channels: usize,
        /// Actual buffer length in bytes.
        len: usize,
    },
    /// The channel count is outside the supported `1..=4`.
    #[error("unsupported channel count {channels}, expected 1..=4")]
    InvalidChannels {
        /// The rejected channel count.
        channels: usize,
    },
    /// The integer scale factor is zero.
    #[error("invalid scale factor {factor}, expected at least 1")]
    InvalidFactor {
        /// The rejected factor.
        factor: usize,
    },
    /// A box downscale was asked for a size that is not a multiple of the factor; the remainder
    /// is never cropped silently.
    #[error("raster size {width}x{height} is not divisible by the scale factor {factor}")]
    NotDivisible {
        /// Source width in pixels.
        width: usize,
        /// Source height in pixels.
        height: usize,
        /// The scale factor.
        factor: usize,
    },
    /// The scaled output size overflows `usize`.
    #[error("scaling {width}x{height} with {channels} channels by {factor} overflows the address space")]
    SizeOverflow {
        /// Source width in pixels.
        width: usize,
        /// Source height in pixels.
        height: usize,
        /// Interleaved channels per pixel.
        channels: usize,
        /// The scale factor.
        factor: usize,
    },
    /// An interleaved buffer of unknown size is not a whole number of `channels`-byte pixels.
    #[error("raster buffer length {len} is not a whole number of {channels}-byte pixels")]
    NotWholePixels {
        /// Actual buffer length in bytes.
        len: usize,
        /// Bytes per pixel the primitive expects.
        channels: usize,
    },
    /// The blur radius is so large that the exact window sum does not fit in `u64`.
    #[error("blur radius {radius} is too large")]
    RadiusTooLarge {
        /// The rejected radius.
        radius: usize,
    },
}
