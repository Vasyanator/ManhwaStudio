# Module: crates/ms-text-detect/src (crate `ms-text-detect`)

## Purpose
The text-detection domain: everything between a page image and the final detected blocks and
mask EXCEPT the model forward pass. It owns the detector rules that used to be duplicated across
the translation tab and `ms-onnx` (block sort and cap, mask normalization, the DB postprocess,
the glyph mask) and the per-engine plan / tile / stitch pipeline for CTD, PaddleOCR and Surya.
Every runner engine has its postprocess here (`pipeline::postprocess_for`); Classic never runs
through a runner and is refused with `DetectError::Unsupported`.

## Architecture
Level 1: depends only on `ms-raster` (level 0), `image`, `imageproc` (workspace entry, defaults
off) and `thiserror`. No egui, no `ms-*` service crate, no logging: callers log, map errors to
their own i18n keys and decide threading. It must build for every target including wasm32.

Data flow (detection pipeline): `plan` decides scale and tiles for an engine and source size ->
`pipeline` resizes the page once and cuts padded tiles -> a caller-supplied `ProbMapRunner`
produces per-tile `ProbMap`s in batches of its `max_batch` -> the pipeline validates count,
channels and size (Surya's quarter maps are upsampled into tile space) -> `stitch` merges each
channel in scaled space -> the engine postprocess runs ONCE (Paddle: `db` on the u8 map +
`glyph_mask` on the source page; CTD: `ctd`; Surya: `surya` on the heatmap alone) ->
`blocks::finalize_blocks` and the 0/255
`mask`.

Callers: `ms-onnx` (DB postprocess and glyph mask for native Paddle OCR detection and, as a
producer of `ProbMap`, the native Paddle forward), `ms-tab-translation` (detection entry points,
runners, region helpers, the panel's plan notice).

## Files and submodules
- `lib.rs`: crate root and module map.
- `blocks.rs`: `DetectRect` (finite, non-empty xyxy; re-exported at the crate root and by the
  translation tab as `TextDetectorRect`), `sort_reading_order` (`(y1, x1, y2, x2)` by
  `total_cmp`, stable) and `finalize_blocks` (sort, then cap at `MAX_DETECTED_BLOCKS` = 2500).
  Used by every detector path and by the tab's block merge and stored-result reload.
- `mask.rs`: `binary_alpha_from_gray` (decoded `GrayImage` -> `BinaryMask`, zero area -> empty,
  `MaskError::TooLarge` above `MAX_MASK_PIXELS` = 100 M), plus `exceeds_pixel_limit` and
  `normalize_binary_alpha` for callers that guard before allocating (classic mask promotion).
- `db.rs` (+ `db/`): `Quad` (re-exported at the crate root) and the DB postprocess
  (`boxes_from_bitmap_with(.., &DbParams)`: binarize, contours, min-area box, box score, unclip,
  rescale; `boxes_from_bitmap` = `DbParams::PADDLE`), generic over `ProbValue` (`f32` raw
  probabilities for OCR, `u8` = `p * 255` for stitched maps; the f32 impl is the identity), plus
  `block_from_quad` (clamped xyxy box of a quad). Presets `DbParams::PADDLE` / `DbParams::CTD`;
  `db/geometry.rs` holds the OpenCV / pyclipper geometry of the CTD preset, including the
  crate's one float `minAreaRect` fit (`min_area_rect_f`, also used by `surya.rs`).
- `glyph_mask.rs`: `build_glyph_mask` from quads over any 8-bit RGB(A) buffer (per-quad
  saturation / dark / light Otsu via `ms_raster::otsu_threshold`, "no split" -> 0;
  polygon-gated; 3x3-cross close); the crate-visible `erode_3x3` is shared with the CTD
  refinement. Paddle golden-fixture parity test.
- `num.rs` (private): the float <-> integer conversions (incl. checked `f64` rounding, `idx`).
- `plan.rs`: `plan_detection(EngineKind, [w, h], &DetectParams) -> DetectionPlan` (private
  fields, accessors), the one owner of scale, tile grid, tile input, padding and filter per
  engine (engine table in its header), the CTD detect-size rule (`effective_ctd_detect_size`:
  clamp 896..=2048, snap down to 64) and `PlanNotice` (mode, per-axis scale %, cols/rows/tiles,
  tile size, effective CTD size) that the panel formats without recomputing.
- `runner.rs`: `ProbMap` (validated u8 map, re-exported at the root), `TileMaps`, the
  `ProbMapRunner` trait (`max_batch`, `forward`) and `RunnerError` (localized message).
- `stitch.rs`: `stitch_tiles`, gather-stitching of one channel with feathered, exactly
  normalized integer weights.
- `pipeline.rs`: `run_detection`, `forward_and_stitch`, `Detection`, `DetectError`; the
  postprocess dispatch `postprocess_for`.
- `ctd.rs` (+ `ctd/`): `ctd::postprocess` on the stitched `[seg, shrink]` maps: DB boxes with
  `DbParams::CTD`, OpenCV-exact seg resize to source, the `refine_mask` port, threshold `> 30`,
  NO dilation (the caller dilates). Parity-tested exactly against `fixtures/ctd`; internals and
  replicated quirks in `ctd/MODULE_README.md`.
- `surya.rs` (+ `surya/tests.rs`): `postprocess` (pipeline entry), `postprocess_heatmap` (bare
  heatmap + source size) and `dynamic_thresholds`: float32 thresholds over the WHOLE stitched
  map, 4-connected components (imageproc), per-component rect dilation, float min-area box,
  `int()` rescale, `clean_boxes`, y-expand, cv2-nearest mask resize. Parity-tested exactly
  against `fixtures/surya` (README there lists every replicated quirk).

## Contracts and invariants
- One owner per rule: block sort/cap, mask normalization and the scale/tiling plan live only
  here; polygon fill, square dilation and Otsu live only in `ms-raster`.
- Plan: tiles cover the scaled page, adjacent tiles overlap by at least `overlap_min`, the last
  tile is flush with the edge, all tiles share one aligned `tile_input`; sources or scaled pages
  above `MAX_MASK_PIXELS` are refused. Classic is planned (for the notice, same f32 formula as the
  tab's classic path) but never runs through a runner.
- Stitch: per-axis quantized weights sum to exactly 256, so a constant input stitches exactly;
  rounding error otherwise stays below one level.
- Pipeline: the postprocess is selected before any forward pass; runner output is validated
  (tile count, channel count, map size) into typed errors, never trusted.
- Callers own threading: `run_detection` is CPU-heavy (resize, DB, glyph mask) and blocking, so
  it runs on a worker, never the GUI thread.
- Public APIs return typed errors on invalid shapes or lengths, never panic.
- An unsupported engine path returns an explicit error, never a silent fallback.
- No I/O, no logging, no global state.
- DB postprocess, one implementation with two presets. `DbParams::PADDLE` (Python
  `DBPostProcess`: thresh 0.3, quad score >= 0.6, unclip 2.0, `max_candidates` 1000, short side
  3 -> 5 after unclip; float geometry: imageproc integer min-area quad, rotated-rectangle
  expansion, <= 1 px from OpenCV) is pinned by the characterization tests. `DbParams::CTD`
  (`SegDetectorRepresenter`: thresh 0.3, CONTOUR score > 0.6, unclip 1.5, short side >= 2, all
  `RETR_LIST` contours, OpenCV / pyclipper geometry) reproduces the CTD fixtures exactly.
- Polygon fill: the DB score region (quad or contour) is filled with
  `ms_raster::fill_polygon_spans`. The Paddle glyph-mask gate is NOT: the spans rule failed the
  Paddle fixture parity there (`fixtures/paddle/light_on_dark` flips a fill-fraction branch, mask
  IoU 0.38), so `glyph_mask.rs` keeps imageproc's `draw_polygon_mut` pending a decision (file
  header). The characterization tests in both files pin today's output.
- Surya postprocess: thresholds are global (computed after stitching, never per tile) and
  float32; the heatmap's own size is the processor size the rescale starts from (equal to
  `plan.scaled_size()` in the pipeline). The symmetric dilation part goes through
  `ms_raster::dilate_square`; only the even-kernel one-pixel right/down extension (cv2 anchor)
  is local, because the shared square dilation is symmetric by contract. The min-area box is the
  shared float fit `db::geometry::min_area_rect_f` (imageproc's integer corners would shift
  truncated rotated boxes); the near-square axis-box rule and the corner roll stay in `surya.rs`.
- One float `cv2.minAreaRect` fit per crate: `db::geometry::min_area_rect_f` (CTD and Surya).

## Editing map
- To change how blocks are ordered or capped, edit `blocks.rs`.
- To change mask normalization or the pixel guard, edit `mask.rs`.
- To change DB box extraction, edit `db.rs` (preset knobs in `DbParams`); for the Paddle glyph
  mask, `glyph_mask.rs`.
- To change the CTD mask refinement or seg resize, edit `ctd/` and keep the fixture parity exact.
- To change scale or tiling per engine, edit `plan.rs` (the panel notice follows automatically).
- To change seam blending, edit `stitch.rs`; to change how a model is called, the caller's runner.
- To change Surya thresholds, boxes or its mask, edit `surya.rs` and keep the fixture parity
  test exact.
- To add an engine postprocess, implement it in its module and return it from
  `pipeline::postprocess_for`.
