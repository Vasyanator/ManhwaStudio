# Module: crates/ms-text-detect/src/db (DB postprocess internals)

## Purpose
Private submodules of `../db.rs` (the DB postprocess shared by the Paddle and CTD presets).

## Files and submodules
- `geometry.rs`: the OpenCV / pyclipper geometry. `min_area_rect_f` is the crate's ONE float
  `cv2.minAreaRect` fit (rotating calipers over a monotone-chain convex hull, corners clockwise
  on screen), crate-visible and used by the CTD preset and by `../surya.rs`; `round_offset` is
  the Clipper 6.4 `JT_ROUND` closed-polygon offset of the CTD preset (ArcTolerance 0.25,
  truncated input, integer output).

## Contracts and invariants
- Within the DB postprocess it is used only when `DbParams::opencv_geometry` is set; the Paddle
  preset keeps imageproc's integer `min_area_rect` and the rotated-rectangle expansion (pinned by
  the characterization tests in `db.rs`).
- `min_area_rect_f` returns the bare rectangle; caller rules stay with the callers (Surya's
  near-square axis box and `x + y` corner roll in `../surya.rs`, CTD's offset and refit in
  `../db.rs`). Do not add a second float min-area fit elsewhere in the crate.
- On equal-area orientations the first hull edge wins; OpenCV may pick another orientation of the
  same rectangle, which only reorders corners.
- No I/O, no logging; pure geometry on small point sets.

## Editing map
- To change how CTD or Surya boxes are fitted, or CTD boxes unclipped, edit `geometry.rs` and
  keep both fixture parities exact (`ctd/parity_tests.rs`, `surya/tests.rs`).
