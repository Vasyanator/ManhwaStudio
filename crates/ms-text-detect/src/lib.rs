/*
File: crates/ms-text-detect/src/lib.rs

Purpose:
Crate root of `ms-text-detect`, the GUI-free text-detection domain shared by the CTD, PaddleOCR
and Surya engines and by the classic detector's block rules. Model forward passes stay outside:
callers supply them through the runner seam (native ONNX or the Python backend over IPC).

Modules:
- `blocks`     : detected-block rectangle, reading-order sort, block cap.
- `mask`       : detector mask normalization to 0/255 with the pixel guard.
- `db`         : DB postprocess (probability map, f32 or u8 -> quads).
- `glyph_mask` : glyph mask from quads over the full-resolution page.
- `plan`       : the one owner of the per-engine scale and tiling decision and the panel notice.
- `stitch`     : feathered stitching of per-tile probability maps.
- `runner`     : the forward-pass seam (`ProbMapRunner`) and the `ProbMap` it returns.
- `pipeline`   : plan -> tiles -> runner -> stitch -> postprocess -> blocks; the engine
                 postprocess is picked by `postprocess_for` (Classic is `Unsupported`).
- `ctd`        : CTD postprocess on the stitched `[seg, shrink]` maps (DB boxes, seg resize,
                 mask refinement), a parity port of the removed Python postprocess.
- `surya`      : Surya postprocess on the stitched heatmap (dynamic thresholds, components,
                 boxes, source mask), a parity port of the removed Python postprocess.

Notes:
Level 1, directly above `ms-raster` (polygon fill, square dilation, Otsu). No egui, no logging
(callers log), no threads of its own; builds for every target including wasm32.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod blocks;
pub mod ctd;
pub mod db;
pub mod glyph_mask;
// Crate-private float <-> integer conversions shared by the modules.
mod num;

pub use db::Quad;

pub mod mask;

pub use blocks::DetectRect;

pub mod pipeline;
pub mod plan;
pub mod runner;
pub mod stitch;
pub mod surya;

pub use pipeline::{DetectError, Detection, DetectionStats, forward_and_stitch, run_detection};
pub use plan::{DetectParams, DetectionPlan, EngineKind, PlanError, PlanMode, PlanNotice, ResizeFilter, TileRect, effective_ctd_detect_size, plan_detection};
pub use runner::{ProbMap, ProbMapError, ProbMapRunner, RunnerError, TileMaps};
