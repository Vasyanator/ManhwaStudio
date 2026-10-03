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
- `RasterError`: the typed buffer-shape error.

Notes:
Level 0 crate: std + `thiserror` only, GUI-free and wasm-safe. Callers own output formats
(0/1, 0/255, in-place, `bool`) through thin adapters; the rules themselves live only here.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

mod morph;
mod otsu;
mod polygon;

pub use morph::dilate_square;
pub use otsu::otsu_threshold;
pub use polygon::fill_polygon_spans;

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
}
