# Module: crates/ms-raster/src (crate `ms-raster`)

## Purpose
Generic, GUI-free raster primitives that the project implements exactly ONCE: the even-odd polygon
scanline rasterizer, the square (Chebyshev) binary dilation, the Otsu threshold, integer-factor
replicate upscale / box downscale of interleaved `u8` rasters and the `u8` box blur. Every crate
that needs one of these rules calls this owner; a second implementation anywhere is a defect.

## Architecture
Level 0 (foundation): std plus `thiserror`, no `ms-*` dependency, no egui, no `image`, wasm-safe.
It sits this low so that `ms-tools`, the cleaning and translation tabs and the text-detection
domain (`ms-text-detect`) can all share the rules without depending on each other.

The primitives work on plain slices and closures. Output formats that differ between callers
(0/1 vs 0/255 masks, in-place `GrayImage` updates keeping original values, `bool` masks, a
caller-specific Otsu default) are thin adapters at the call site, never variants here.

## Files and submodules
- `lib.rs`: crate root, re-exports, `RasterError`.
- `polygon.rs`: `fill_polygon_spans`, the even-odd scanline polygon rasterizer. Re-exported by
  `ms-tools` as `ms_tools::fill_polygon_spans`, the path the PS-editor selection, the cleaning
  tools and the patch core use.
- `morph.rs`: `dilate_square`, separable sliding-window square dilation with independent x/y radii.
- `otsu.rs`: `otsu_threshold`, the 256-bin Otsu threshold.
- `scale.rs`: `upscale_replicate` / `downscale_box`, integer-factor nearest upscale and
  box-average downscale of interleaved 1..=4-channel `u8` rasters (the cloud image-edit pipeline
  in `ms-ai-api` sends `k*W x k*H` and maps the answer back with them).
- `blur.rs`: `box_blur_u8`, the clamp-to-edge separable box blur of a single-channel plane (the
  image-edit mask feather).

## Contracts and invariants
- `fill_polygon_spans` emits `span(y, x0, x1)` with an INCLUSIVE `x0..=x1`, clamped to the
  caller's `width`/`height`, in increasing `y`, never inverted; insideness is sampled at the
  scanline centre `y + 0.5` under the even-odd rule. This sampling rule is a contract: callers
  rasterizing the same polygon into differently sized buffers must agree pixel for pixel.
- `dilate_square` treats nonzero input as set, writes `on`/`0`, clips the window to the buffer
  (out-of-bounds neighbours are absent), costs O(w*h) for any radius and returns
  `RasterError::LengthMismatch` when `len != w*h` (overflow included). Radius `r` equals `r`
  iterated 3x3 passes; tests pin it against a brute-force reference and the pre-refactor copies'
  characterization vectors.
- `otsu_threshold` returns `None` when no split exists (empty input or one distinct value); ties
  keep the FIRST maximum. Each caller keeps its historical default with `unwrap_or` (the classic
  detector 127, the Paddle glyph mask 0).
- `upscale_replicate` / `downscale_box` take row-major interleaved buffers with `channels` in
  `1..=4`, `factor >= 1`, `len == width*height*channels`; channels are independent (no alpha
  premultiplication). `downscale_box` rounds the block mean half up and rejects sizes that are
  not multiples of `factor` (`NotDivisible`, never a silent crop), so
  `downscale_box(upscale_replicate(x, k), k) == x` bit for bit; tests pin that round trip.
- `box_blur_u8` reads out-of-bounds samples from the nearest edge pixel, divides by `(2r+1)^2`
  always and rounds once (half up) from exact `u64` sums: a constant plane stays constant, a
  pixel farther than `r` from every nonzero pixel stays 0, radius 0 is the identity, the output
  has the input's length. O(w*h) for any radius; `RadiusTooLarge` only when the exact sum cannot
  fit in `u64`. Tests pin it against a brute-force 2-D reference.
- Shape and parameter errors are typed `RasterError` variants; sizes use checked arithmetic and
  no public function panics.
- No I/O, no logging, no threads, no global state: callers log.

## Editing map
- To change polygon fill semantics, edit `polygon.rs` and re-verify the PS-editor selection, the
  cleaning tools and the patch core.
- To change the dilation rule, edit `morph.rs`; every caller's adapter test must stay green.
- To change the threshold rule, edit `otsu.rs`; the classic detector and the glyph mask use it.
- To change integer scaling or the blur, edit `scale.rs` / `blur.rs`; the image-edit pipeline's
  bit-exact "untouched outside the mask" guarantee depends on the round trip and on the blur's
  zero-stays-zero reach, so keep those tests green.
- A new primitive belongs here only if it is generic (no detector, tab or UI semantics) and has,
  or replaces, more than one implementation.
