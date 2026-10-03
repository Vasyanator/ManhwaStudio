# Module: crates/ms-text-detect/src/ctd (CTD postprocess internals)

## Purpose
Internals of the comic-text-detector postprocess whose entry is `../ctd.rs`
(`ctd::postprocess`, plugged into `pipeline::postprocess_for`): everything after DB boxing that
turns the stitched seg map into the source-size text mask.

## Architecture
`ctd.rs` runs: `db::boxes_from_bitmap_with(shrink, DbParams::CTD)` -> integer xyxy blocks ->
`resize::resize_linear_u8(seg -> source)` -> `refine::refine_mask(page, seg_source, blocks)` ->
`> 30` -> `BinaryMask`. No mask dilation (the caller dilates later). Everything here is private
to the `ctd` module; only `ctd::postprocess` and `ctd::MASK_THRESHOLD` are public.

## Files and submodules
- `refine.rs`: `refine_mask` port (per block: `enlarge_window`, top-k grey bands + best Otsu
  channel candidates, `minxor`, component merge, 5x5 dilate, hole fill) and its upstream quirks.
- `resize.rs`: OpenCV 8-bit `INTER_LINEAR` port (fixed-point taps, SIMD/scalar column split).
- `components.rs`: 8-connected labelling with per-component box and area.
- `parity_tests.rs` (tests only): golden-fixture parity (`fixtures/ctd`), intermediate checks,
  map validation and an end-to-end `run_detection` with a fake runner.

## Contracts and invariants
- Parity target: the Python reference recorded in `fixtures/ctd` (quirk list in
  `fixtures/README.md`). The port reproduces all three cases exactly (blocks, seg resize,
  refined mask); `ctd_postprocess_is_exact_on_the_fixtures` pins that, the tolerance test pins
  the plan's contract (block IoU >= 0.9 / edges +-2 px, mask IoU >= 0.97 / XOR <= 3 %).
- The page is RGB; upstream's BGR-only arithmetic (grey weights, Otsu channel order) is
  reproduced on purpose and commented at its site. Do not "fix" it without regenerating the
  fixtures from a reference that has the fix.
- The colour sort is stable; upstream's was unstable, so on real pages a tied third colour may
  differ from Python. Fixtures avoid ties by construction.
- Polygon fill, square dilation and Otsu come from `ms-raster`; the 3x3-cross erode is shared
  with `glyph_mask.rs` (`erode_3x3`).
- Pure CPU work, no logging, no I/O; callers run it on a worker.

## Editing map
- Block windows, candidates, merge rules: `refine.rs`.
- Seg map resampling: `resize.rs`.
- DB parameters of CTD: `DbParams::CTD` in `../db.rs`.
- The final threshold and block conversion: `../ctd.rs`.
