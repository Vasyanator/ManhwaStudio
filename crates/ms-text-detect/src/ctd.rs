/*
File: crates/ms-text-detect/src/ctd.rs

Purpose:
The comic-text-detector (CTD) postprocess on the stitched scaled-space maps `[seg, shrink]`:
DB boxes on the shrink map with `DbParams::CTD`, one block per box, the seg map resized to the
source, the mask refinement, and the final `> 30` threshold. Port of `TextDetector.__call__`
after the forward pass (former `detection/textdetector/ctd/inference.py:240-273`) and the mask
step of the backend service (former `detection/ctd.py:400-412`).

Key functions:
- `postprocess()`: plan + page + stitched maps -> source-px blocks and the 0/255 source mask
  (the `pipeline::postprocess_for` entry).
- `postprocess_maps()`: the same with the map size taken from the maps (crate-private; the parity
  tests drive it with the golden fixtures, whose map size is not one the plan would choose).

Submodules:
- `ctd/refine.rs`: `refine_mask` with `enlarge_window` and the upstream quirks.
- `ctd/resize.rs`: OpenCV `INTER_LINEAR` port for the seg map.
- `ctd/components.rs`: 8-connected component labelling.
- `ctd/parity_tests.rs` (tests only): golden-fixture parity and an end-to-end pipeline run.

Notes:
- No mask dilation here: the user's mask dilation happens later on the caller's side, so the
  returned mask is the refined mask itself (the old backend's sticky ellipse dilation, default
  k = 2, is gone). The golden fixtures were recorded with dilation 0, so they compare this stage.
- Blocks are the min/max of the integer box corners in source pixels, empty ones dropped; the
  pipeline sorts and caps them afterwards (`blocks::finalize_blocks`).
- The page is RGB; upstream's BGR-specific arithmetic is reproduced in `refine.rs`.
- CPU-heavy (per-block window histograms, components, morphology): callers run it on a worker.
*/

mod components;
#[cfg(test)]
mod parity_tests;
mod refine;
mod resize;

use image::{GrayImage, RgbImage};

use crate::blocks::DetectRect;
use crate::db::{DbParams, Quad, boxes_from_bitmap_with};
use crate::mask::{BinaryMask, binary_alpha_from_gray};
use crate::num::{f32_to_i32_trunc, idx};
use crate::pipeline::DetectError;
use crate::plan::DetectionPlan;
use crate::runner::ProbMap;

/// Refined-mask level above which a source pixel is text (`cv2.threshold(.., 30, 255, BINARY)`).
pub const MASK_THRESHOLD: u8 = 30;

/// CTD postprocess: `maps` are the stitched `[seg, shrink]` maps at `plan.scaled_size()`; `page`
/// is the full-resolution source page. Returns blocks in source pixels (unsorted) and the
/// source-size 0/255 mask.
///
/// # Errors
/// [`DetectError::ChannelCount`] unless exactly two maps are given; [`DetectError::MapShape`] when
/// a map is not `plan.scaled_size()`; [`DetectError::PageSize`] when `page` is not
/// `plan.source_size()`; [`DetectError::Mask`] for a mask above the pixel limit.
pub fn postprocess(plan: &DetectionPlan, page: &RgbImage, maps: &[ProbMap]) -> Result<(Vec<DetectRect>, BinaryMask), DetectError> {
    let [seg, shrink] = maps else {
        return Err(DetectError::ChannelCount { tile: 0, expected: 2, got: maps.len() });
    };
    let expected = plan.scaled_size();
    for (channel, map) in maps.iter().enumerate() {
        if map.size() != expected {
            return Err(DetectError::MapShape { tile: 0, channel, expected, got: map.size() });
        }
    }
    let got = [page.width(), page.height()];
    if got != plan.source_size() {
        return Err(DetectError::PageSize { expected: plan.source_size(), got });
    }
    postprocess_maps(page, seg, shrink)
}

/// [`postprocess`] without the plan: the map size is the maps' own, the source size the page's.
///
/// # Errors
/// [`DetectError::MapShape`] when the two maps differ in size; [`DetectError::Mask`] for a mask
/// above the pixel limit.
pub(crate) fn postprocess_maps(page: &RgbImage, seg: &ProbMap, shrink: &ProbMap) -> Result<(Vec<DetectRect>, BinaryMask), DetectError> {
    if seg.size() != shrink.size() {
        return Err(DetectError::MapShape { tile: 0, channel: 1, expected: seg.size(), got: shrink.size() });
    }
    let (src_w, src_h) = page.dimensions();
    let [map_w, map_h] = shrink.size();
    let quads = boxes_from_bitmap_with(shrink.data(), idx(map_w), idx(map_h), src_w, src_h, &DbParams::CTD);
    let boxes: Vec<[u32; 4]> = quads.iter().map(|quad| integer_box(quad, src_w, src_h)).collect();
    let seg_source = resize::resize_linear_u8(seg.data(), map_w, map_h, src_w, src_h);
    let refined = refine::refine_mask(page, &seg_source, &boxes);
    let thresholded: Vec<u8> = refined.into_iter().map(|v| if v > MASK_THRESHOLD { 255 } else { 0 }).collect();
    // `refine_mask` returns exactly `src_w * src_h` levels, so `from_raw` cannot fail; an empty
    // image would only make the mask empty, never wrong-sized.
    let gray = GrayImage::from_raw(src_w, src_h, thresholded).unwrap_or_default();
    let mask = binary_alpha_from_gray(gray)?;
    let blocks = boxes.iter().filter_map(|b| DetectRect::from_xyxy(u32_coord(b[0]), u32_coord(b[1]), u32_coord(b[2]), u32_coord(b[3]))).collect();
    Ok((blocks, mask))
}

/// `group_output`: the integer xyxy (min/max of the corners) of a box whose corners are integral
/// and clamped to `[0, src]` by the CTD DB parameters.
fn integer_box(quad: &Quad, src_w: u32, src_h: u32) -> [u32; 4] {
    let coord = |v: f32, max: u32| u32::try_from(f32_to_i32_trunc(v).max(0)).unwrap_or(0).min(max);
    let xs = quad.map(|p| coord(p[0], src_w));
    let ys = quad.map(|p| coord(p[1], src_h));
    let min = |v: [u32; 4]| v.into_iter().min().unwrap_or(0);
    let max = |v: [u32; 4]| v.into_iter().max().unwrap_or(0);
    [min(xs), min(ys), max(xs), max(ys)]
}

/// A source pixel coordinate as `f32` (page sides stay below 2^24, exact).
fn u32_coord(value: u32) -> f32 {
    crate::num::u32_to_f32(value)
}
