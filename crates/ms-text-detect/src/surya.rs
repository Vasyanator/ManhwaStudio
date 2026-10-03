/*
File: crates/ms-text-detect/src/surya.rs

Purpose:
The Surya text-detection postprocess on the stitched channel-0 heatmap (scaled space): dynamic
thresholds, 4-connected components, per-component rect dilation and box, rescale to source
pixels, `clean_boxes`, the vertical box expansion and the source-size 0/255 mask.

Key items:
- `postprocess()`: the pipeline entry (`pipeline::postprocess_for` returns it for Surya).
- `postprocess_heatmap()`: the same on a bare heatmap and source size (parity tests, callers
  that already hold a stitched map).
- `dynamic_thresholds()` / `Thresholds`: the float32 threshold rule over the WHOLE map.

Reference (ported 1:1, every quirk below is replicated on purpose):
- the service `_extract_mask_and_boxes` and `_detect_with_predictor` of the removed
  `modules/ai_backend/detection/surya.py` (git history), on surya 0.17.1:
  `detection/heatmap.py:13-23` (dynamic thresholds), `common/polygon.py:59-81` (rescale,
  `fit_to_bounds`) and `:100-113` (`expand`), `common/util.py:11-38` (`clean_boxes`),
  `settings.py:61-75` (`DETECTOR_TEXT_THRESHOLD` 0.6, `DETECTOR_BLANK_THRESHOLD` 0.35,
  `DETECTOR_BOX_Y_EXPAND_MARGIN` 0.05).
- Quirks: thresholds are float32 under numpy 2 (NEP 50) rules, so a component whose max level is
  153 meets 0.6f32 exactly and is KEPT; components are 4-connected; an EVEN dilation kernel
  extends `k/2` right/down but `k/2 - 1` left/up (cv2 default anchor); a near-square box is
  replaced by the axis box with no +1; rescale and y-expand use `int()` truncation;
  `clean_boxes` containment is inclusive and identical boxes both survive; the mask resize is
  cv2 `INTER_NEAREST` (`src = floor(dst * (1 / (dst_len / src_len)))`), not `image`'s Nearest.

Notes:
- The heatmap's own size is the processor size the rescale starts from (Python used
  `heatmap.shape`); in the pipeline it equals `plan.scaled_size()`.
- Connected components come from imageproc (raster-order labels). The symmetric part of the
  dilation is `ms_raster::dilate_square`; an even kernel adds a private one-pixel right/down
  extension (`extend_right_down`), because the shared square dilation is symmetric by contract.
- The minimum-area rectangle is the crate's one float `cv2.minAreaRect` fit,
  `db::geometry::min_area_rect_f` (shared with the CTD preset): imageproc's `min_area_rect`
  returns integer corners, and the reference truncates OpenCV's FLOAT corners on rescale, so
  rounded corners would shift rotated boxes by a pixel. The near-square axis-box rule and the
  corner roll stay here. Known residual vs OpenCV: float noise in rotated corners (sub-pixel),
  the orientation chosen among equal-area rectangles, and the starting corner when two corners
  tie on `x + y` (only at exactly 45 degrees).
*/

use image::GrayImage;
use imageproc::point::Point;
use imageproc::region_labelling::{Connectivity, connected_components};

use crate::blocks::DetectRect;
use crate::db::geometry::min_area_rect_f;
use crate::mask::{BinaryMask, MaskError, exceeds_pixel_limit};
use crate::num::{f32_from_i32, f64_round_to_u32, idx, u32_to_f32};
use crate::pipeline::DetectError;
use crate::plan::DetectionPlan;
use crate::runner::ProbMap;

/// `DETECTOR_TEXT_THRESHOLD` (surya `settings.py:61-63`): a component must reach it.
const TEXT_THRESHOLD: f32 = 0.6;
/// `DETECTOR_BLANK_THRESHOLD` (surya `settings.py:64-66`): pixels above it form components.
const LOW_TEXT: f32 = 0.35;
/// `typical_top10_avg` of `get_dynamic_thresholds` (surya `detection/heatmap.py:13`).
const TYPICAL_TOP10_AVG: f32 = 0.7;
/// `DETECTOR_BOX_Y_EXPAND_MARGIN` (surya `settings.py:73-75`), a fraction of the box height.
const Y_EXPAND_MARGIN: f64 = 0.05;
/// Components with a smaller stats area are skipped.
const MIN_COMPONENT_AREA: u32 = 10;
/// The near-square rule: `|1 - long / (short + 1e-5)| <= 0.1` takes the axis box.
const SQUARE_RATIO_TOLERANCE: f64 = 0.1;

/// The dynamic thresholds of one heatmap, in float32 exactly as the reference compares them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// A component is kept only when its maximum is NOT below this (`max < text` skips).
    pub text: f32,
    /// Pixels strictly above this form the components.
    pub low: f32,
}

/// Runs the Surya postprocess for the pipeline: `maps[0]` is the stitched scaled-space heatmap.
///
/// Returns blocks in source pixels (not yet sorted/capped by the pipeline) and the source-size
/// 0/255 mask. `_page` is unused: the Surya mask comes from the heatmap alone.
///
/// # Errors
/// [`DetectError::ChannelCount`] when `maps` is empty; [`DetectError::Mask`] when the source
/// size exceeds the mask pixel limit.
pub fn postprocess(plan: &DetectionPlan, _page: &image::RgbImage, maps: &[ProbMap]) -> Result<(Vec<DetectRect>, BinaryMask), DetectError> {
    let heat = maps.first().ok_or(DetectError::ChannelCount { tile: 0, expected: 1, got: 0 })?;
    postprocess_heatmap(heat, plan.source_size()).map_err(DetectError::from)
}

/// Runs the Surya postprocess on `heat` (processor space; its size is the processor size) for a
/// page of `source_size` `[w, h]`.
///
/// Returns the blocks in source pixels, in the reference order (sorted by `(y1, x1, y2, x2)`,
/// degenerate boxes dropped), and the source-size 0/255 mask. A heatmap with no component above
/// the thresholds yields no blocks and an all-zero mask; a zero-area source yields the empty mask.
///
/// # Errors
/// [`MaskError::TooLarge`] when `source_size` exceeds the mask pixel limit.
pub fn postprocess_heatmap(heat: &ProbMap, source_size: [u32; 2]) -> Result<(Vec<DetectRect>, BinaryMask), MaskError> {
    let [src_w, src_h] = source_size;
    if exceeds_pixel_limit(src_w, src_h) {
        return Err(MaskError::TooLarge { width: src_w, height: src_h });
    }
    let raw = extract_boxes(heat);
    let processor_size = heat.size();
    let mut boxes: Vec<Polygon> = raw.boxes.iter().map(|corners| rescale_and_fit(corners, processor_size, source_size)).collect();
    boxes = clean_boxes(&boxes);
    for polygon in &mut boxes {
        let [x1, y1, x2, y2] = bbox(polygon);
        if u64::from(y2 - y1) < 3 * u64::from(x2 - x1) {
            *polygon = expand_y_and_fit(polygon, source_size);
        }
    }
    let mut ordered: Vec<[u32; 4]> = boxes.iter().map(bbox).collect();
    // Python sorts by the bbox `(y1, x1, y2, x2)` with a stable sort.
    ordered.sort_by_key(|&[x1, y1, x2, y2]| (y1, x1, y2, x2));
    let blocks = ordered.iter().filter_map(|&[x1, y1, x2, y2]| DetectRect::from_xyxy(u32_to_f32(x1), u32_to_f32(y1), u32_to_f32(x2), u32_to_f32(y2))).collect();
    let mask = resize_mask_nearest(&raw.proc_mask, processor_size, source_size);
    Ok((blocks, mask))
}

/// Computes the dynamic thresholds over the WHOLE heatmap (surya `detection/heatmap.py:13-23`):
/// the mean of the top 10 % of values (the `n - int(0.9 n)` largest), the factor
/// `sqrt(clip(mean / 0.7, 0, 1))`, then `low = clip(0.35 f, 0.1, 0.6)` and
/// `text = clip(0.6 f, 0.15, 0.8)`, all in float32 (numpy 2 promotion rules).
///
/// A level `k` stands for `k as f32 / 255.0`. Returns `None` for an empty map (the reference's
/// mean is NaN there and no pixel passes any comparison) and for a map of more than `u32::MAX`
/// values, which the pipeline never produces (the plan refuses maps above 100 M pixels).
#[must_use]
pub fn dynamic_thresholds(heat: &[u8]) -> Option<Thresholds> {
    let n = u32::try_from(heat.len()).ok()?;
    if n == 0 {
        return None;
    }
    let mut histogram = [0_u32; 256];
    for &level in heat {
        histogram[usize::from(level)] += 1;
    }
    // `int(len * 0.9)`: Python multiplies in f64 and truncates; the product is non-negative.
    let skipped = f64_round_to_u32((f64::from(n) * 0.9).floor())?;
    let top = n - skipped;
    // The mean of the `top` largest values, summed per level in f64 (at most 256 terms, so the
    // f64 rounding is negligible); numpy's float32 pairwise sum differs from it by float32
    // rounding noise only, well under the 1e-6 parity tolerance.
    let mut remaining = top;
    let mut sum = 0.0_f64;
    for level in (0..=255_u8).rev() {
        if remaining == 0 {
            break;
        }
        let take = histogram[usize::from(level)].min(remaining);
        sum += f64::from(level_value(level)) * f64::from(take);
        remaining -= take;
    }
    let mean = round_to_f32(sum / f64::from(top));
    let factor = (mean / TYPICAL_TOP10_AVG).clamp(0.0, 1.0).sqrt();
    Some(Thresholds { text: (TEXT_THRESHOLD * factor).clamp(0.15, 0.8), low: (LOW_TEXT * factor).clamp(0.1, 0.6) })
}

/// A box in source pixels after rescale and `fit_to_bounds` (the reference holds Python ints
/// there, all within `0..=W` / `0..=H`): four `[x, y]` corners, clockwise on screen, starting at
/// the corner with the smallest `x + y`.
type Polygon = [[u32; 2]; 4];

/// Statistics of one connected component (cv2 `connectedComponentsWithStats` columns).
#[derive(Debug, Clone, Copy)]
struct Component {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
    area: u32,
    max_level: u8,
}

/// The processor-space result of the box extraction.
struct RawBoxes {
    /// Accepted boxes (float32 corners, rolled), in label order.
    boxes: Vec<[[f32; 2]; 4]>,
    /// Processor-space mask: 255 on every accepted component's dilated pixels.
    proc_mask: Vec<u8>,
    /// Per component in label order: its stats and what happened to it. Parity diagnostics read
    /// only by the golden-fixture tests, so release builds neither collect nor allocate them.
    #[cfg(test)]
    components: Vec<(Component, Outcome)>,
}

/// What happened to one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Stats area below [`MIN_COMPONENT_AREA`].
    AreaTooSmall,
    /// Maximum below the text threshold.
    BelowTextThreshold,
    /// Became a box and part of the mask.
    Kept,
}

/// The float32 value a stored level stands for (`float32(k) / float32(255)`).
fn level_value(level: u8) -> f32 {
    f32::from(level) / 255.0
}

/// Port of `_extract_mask_and_boxes`: thresholds, 4-connected components of `heat > low`, the
/// per-component dilation, mask and box.
fn extract_boxes(heat: &ProbMap) -> RawBoxes {
    let [w, h] = heat.size();
    let mut proc_mask = vec![0_u8; heat.data().len()];
    let Some(thresholds) = dynamic_thresholds(heat.data()) else {
        return RawBoxes {
            boxes: Vec::new(),
            proc_mask,
            #[cfg(test)]
            components: Vec::new(),
        };
    };
    // The comparisons happen in float32 (`heat > low`, `max < text`), decided per level once.
    let above_low: Vec<bool> = (0..=255_u8).map(|level| level_value(level) > thresholds.low).collect();
    let binary = GrayImage::from_fn(w, h, |x, y| image::Luma([u8::from(above_low[usize::from(heat.data()[idx(y) * idx(w) + idx(x)])])]));
    let labels = connected_components(&binary, Connectivity::Four, image::Luma([0]));
    let labels = labels.as_raw();
    let components = component_stats(labels, heat.data(), w);

    let mut boxes = Vec::new();
    #[cfg(test)]
    let mut outcomes = Vec::with_capacity(components.len());
    for (offset, component) in components.iter().enumerate() {
        let outcome = if component.area < MIN_COMPONENT_AREA {
            Outcome::AreaTooSmall
        } else if level_value(component.max_level) < thresholds.text {
            Outcome::BelowTextThreshold
        } else {
            Outcome::Kept
        };
        if outcome == Outcome::Kept {
            // Labels start at 1; `offset` < label count <= pixel count < 2^32.
            let label = u32::try_from(offset + 1).unwrap_or(u32::MAX);
            boxes.push(component_box(labels, [w, h], label, component, &mut proc_mask));
        }
        #[cfg(test)]
        outcomes.push((*component, outcome));
    }
    RawBoxes {
        boxes,
        proc_mask,
        #[cfg(test)]
        components: outcomes,
    }
}

/// One pass over the label image: per label (index `label - 1`) its bbox, area and max level.
fn component_stats(labels: &[u32], heat: &[u8], width: u32) -> Vec<Component> {
    let mut components: Vec<Component> = Vec::new();
    let width = idx(width);
    for (i, (&label, &level)) in labels.iter().zip(heat).enumerate() {
        if label == 0 {
            continue;
        }
        // Coordinates of a pixel of a `u32 x u32` image fit `u32`.
        let x = u32::try_from(i % width).unwrap_or(u32::MAX);
        let y = u32::try_from(i / width).unwrap_or(u32::MAX);
        let slot = idx(label) - 1;
        if slot == components.len() {
            // Labels are numbered in raster order of their first pixel, so a new label is
            // always the next index.
            components.push(Component { min_x: x, min_y: y, max_x: x, max_y: y, area: 0, max_level: 0 });
        }
        let c = &mut components[slot];
        c.min_x = c.min_x.min(x);
        c.max_x = c.max_x.max(x);
        c.max_y = y;
        c.area += 1;
        c.max_level = c.max_level.max(level);
    }
    components
}

/// Dilates one accepted component inside its window, ORs it into `proc_mask` and returns its box.
///
/// Window: the bbox grown by `niter + 1` per side, clipped, `niter = int(sqrt(min(w, h)))`; only
/// pixels of `label` inside it count. Kernel `MORPH_RECT` of side `k = max(1, 1 + niter)`.
fn component_box(labels: &[u32], size: [u32; 2], label: u32, component: &Component, proc_mask: &mut [u8]) -> [[f32; 2]; 4] {
    let [w, h] = size;
    let comp_w = component.max_x - component.min_x + 1;
    let comp_h = component.max_y - component.min_y + 1;
    let niter = comp_w.min(comp_h).isqrt();
    let sx = component.min_x.saturating_sub(niter + 1);
    let sy = component.min_y.saturating_sub(niter + 1);
    let ex = (component.min_x + comp_w + niter + 1).min(w);
    let ey = (component.min_y + comp_h + niter + 1).min(h);
    let (win_w, win_h) = (idx(ex - sx), idx(ey - sy));
    let mut window = vec![0_u8; win_w * win_h];
    for (row, dst) in window.chunks_exact_mut(win_w).enumerate() {
        let start = (idx(sy) + row) * idx(w) + idx(sx);
        for (d, &l) in dst.iter_mut().zip(&labels[start..start + win_w]) {
            *d = u8::from(l == label);
        }
    }
    let ksize = idx(niter + 1).max(1);
    // An odd kernel is symmetric (radius (k-1)/2). An even one, with cv2's default anchor k/2,
    // reaches k/2 - 1 left/up and k/2 right/down: the symmetric k/2 - 1 dilation followed by a
    // one-pixel right/down extension (a Minkowski sum of the two windows).
    let even = ksize.is_multiple_of(2);
    let radius = if even { ksize / 2 - 1 } else { (ksize - 1) / 2 };
    // Invariant: `window` holds exactly `win_w * win_h` bytes (built above), so the only error
    // of `dilate_square` (a length mismatch) cannot occur.
    let mut dilated = ms_raster::dilate_square(&window, win_w, win_h, radius, radius, 1).expect("window holds exactly win_w * win_h bytes");
    if even {
        dilated = extend_right_down(&dilated, win_w);
    }

    // Mask, and the per-row extremes of the dilated set: they carry its convex hull and bbox.
    let mut extremes: Vec<Point<i32>> = Vec::new();
    for (row, values) in dilated.chunks_exact(win_w).enumerate() {
        let mask_row = (idx(sy) + row) * idx(w) + idx(sx);
        for (col, &v) in values.iter().enumerate() {
            if v != 0 {
                proc_mask[mask_row + col] = 255;
            }
        }
        let first = values.iter().position(|&v| v != 0);
        let last = values.iter().rposition(|&v| v != 0);
        if let (Some(first), Some(last)) = (first, last) {
            // Map coordinates are below 2^32 and in practice far below i32::MAX (the plan
            // refuses maps above 100 M pixels); saturate rather than wrap.
            let y = i32::try_from(idx(sy) + row).unwrap_or(i32::MAX);
            extremes.push(Point::new(i32::try_from(idx(sx) + first).unwrap_or(i32::MAX), y));
            extremes.push(Point::new(i32::try_from(idx(sx) + last).unwrap_or(i32::MAX), y));
        }
    }
    let corners = min_area_box(&extremes);
    let edge_w = dist(corners[0], corners[1]);
    let edge_h = dist(corners[1], corners[2]);
    let ratio = edge_w.max(edge_h) / (edge_w.min(edge_h) + 1e-5);
    let corners = if (1.0 - ratio).abs() <= SQUARE_RATIO_TOLERANCE { axis_box(&extremes) } else { corners };
    roll_to_top_left(corners)
}

/// Grows a 0/1 window by one pixel to the right and down (`out(x, y) = OR of in at (x, y),
/// (x-1, y), (x, y-1), (x-1, y-1)`), clipped to the window: the asymmetric half of cv2's
/// even-kernel dilation, which the symmetric `ms_raster::dilate_square` cannot express.
fn extend_right_down(src: &[u8], width: usize) -> Vec<u8> {
    let mut out = src.to_vec();
    let height = src.len() / width;
    for y in 0..height {
        for x in 0..width {
            let up = y > 0 && src[(y - 1) * width + x] != 0;
            let left = x > 0 && src[y * width + x - 1] != 0;
            let diag = x > 0 && y > 0 && src[(y - 1) * width + x - 1] != 0;
            if up || left || diag {
                out[y * width + x] = 1;
            }
        }
    }
    out
}

/// Euclidean distance of two float32 corners (computed in f64).
fn dist(a: [f32; 2], b: [f32; 2]) -> f64 {
    (f64::from(a[0]) - f64::from(b[0])).hypot(f64::from(a[1]) - f64::from(b[1]))
}

/// The axis box `[minx, miny], [maxx, miny], [maxx, maxy], [minx, maxy]` (no +1) of the points.
fn axis_box(points: &[Point<i32>]) -> [[f32; 2]; 4] {
    let min_x = points.iter().map(|p| p.x).min().unwrap_or(0);
    let max_x = points.iter().map(|p| p.x).max().unwrap_or(0);
    let min_y = points.iter().map(|p| p.y).min().unwrap_or(0);
    let max_y = points.iter().map(|p| p.y).max().unwrap_or(0);
    let [l, r, t, b] = [min_x, max_x, min_y, max_y].map(f32_from_i32);
    [[l, t], [r, t], [r, b], [l, b]]
}

/// The minimum-area rectangle of `points` (cv2 `minAreaRect` + `boxPoints`) as float32 corners,
/// clockwise on screen, through the shared fit `min_area_rect_f`. An empty input gives four zero
/// corners; a single point a zero-size box on it.
fn min_area_box(points: &[Point<i32>]) -> [[f32; 2]; 4] {
    let points: Vec<[i64; 2]> = points.iter().map(|p| [i64::from(p.x), i64::from(p.y)]).collect();
    min_area_rect_f(&points).map_or([[0.0; 2]; 4], |corners| corners.map(|[x, y]| [round_to_f32(x), round_to_f32(y)]))
}

/// `np.roll(box, 4 - argmin(x + y))`: the corner with the smallest float32 `x + y` comes first
/// (the first such corner on a tie), the cyclic order is kept.
fn roll_to_top_left(corners: [[f32; 2]; 4]) -> [[f32; 2]; 4] {
    let mut start = 0;
    for (i, c) in corners.iter().enumerate() {
        if c[0] + c[1] < corners[start][0] + corners[start][1] {
            start = i;
        }
    }
    [corners[start], corners[(start + 1) % 4], corners[(start + 2) % 4], corners[(start + 3) % 4]]
}

/// `PolygonBox.rescale` + `fit_to_bounds`: each corner becomes `int(c * src / proc)` (truncation
/// toward zero), then clamped to `[0, W] x [0, H]` (inclusive bounds).
fn rescale_and_fit(corners: &[[f32; 2]; 4], processor_size: [u32; 2], source_size: [u32; 2]) -> Polygon {
    let sx = f64::from(source_size[0]) / f64::from(processor_size[0]);
    let sy = f64::from(source_size[1]) / f64::from(processor_size[1]);
    corners.map(|[x, y]| [int_and_fit(f64::from(x) * sx, source_size[0]), int_and_fit(f64::from(y) * sy, source_size[1])])
}

/// Python `int(value)` (truncation toward zero) followed by `fit_to_bounds` on one axis: the
/// result is clamped to `0..=bound`.
fn int_and_fit(value: f64, bound: u32) -> u32 {
    // After the clamp the value is an integer-valued float inside `0..=bound`, so the conversion
    // is exact and cannot fail; NaN (impossible for finite corners) maps to 0.
    f64_round_to_u32(value.trunc().clamp(0.0, f64::from(bound))).unwrap_or(0)
}

/// `PolygonBox.bbox`: `[min x, min y, max x, max y]`.
fn bbox(polygon: &Polygon) -> [u32; 4] {
    let xs = polygon.map(|c| c[0]);
    let ys = polygon.map(|c| c[1]);
    [xs.into_iter().min().unwrap_or(0), ys.into_iter().min().unwrap_or(0), xs.into_iter().max().unwrap_or(0), ys.into_iter().max().unwrap_or(0)]
}

/// surya `clean_boxes`: drops boxes with zero width or height, and boxes whose bbox lies inside
/// another box's bbox (inclusive); a box with an identical polygon or bbox is not compared, so
/// duplicates both survive. Every box of the input, degenerate ones included, can contain.
fn clean_boxes(boxes: &[Polygon]) -> Vec<Polygon> {
    boxes
        .iter()
        .filter(|polygon| {
            let b = bbox(polygon);
            if b[2] == b[0] || b[3] == b[1] {
                return false;
            }
            !boxes.iter().any(|other| {
                if other == *polygon {
                    return false;
                }
                let o = bbox(other);
                o != b && b[0] >= o[0] && b[1] >= o[1] && b[2] <= o[2] && b[3] <= o[3]
            })
        })
        .copied()
        .collect()
}

/// `PolygonBox.expand(0, Y_EXPAND_MARGIN)` + `fit_to_bounds`: corners 0 and 1 move up, 2 and 3
/// down, by `margin * height` (f64), each through `int()` and then clamped; x is unchanged.
fn expand_y_and_fit(polygon: &Polygon, source_size: [u32; 2]) -> Polygon {
    let [_, y1, _, y2] = bbox(polygon);
    let margin = Y_EXPAND_MARGIN * f64::from(y2 - y1);
    let mut out = *polygon;
    for (i, corner) in out.iter_mut().enumerate() {
        let moved = if i < 2 { f64::from(corner[1]) - margin } else { f64::from(corner[1]) + margin };
        corner[1] = int_and_fit(moved, source_size[1]);
    }
    out
}

/// cv2 `INTER_NEAREST` resize of the processor mask to the source: `src = min(floor(dst * ifx),
/// src_len - 1)` with `ifx = 1 / (dst_len / src_len)` in f64, exactly as `OpenCV`'s `resizeNN`.
fn resize_mask_nearest(proc_mask: &[u8], processor_size: [u32; 2], source_size: [u32; 2]) -> BinaryMask {
    let [pw, ph] = processor_size;
    let [sw, sh] = source_size;
    if sw == 0 || sh == 0 {
        return BinaryMask { size: [0, 0], alpha: Vec::new() };
    }
    if pw == 0 || ph == 0 {
        return BinaryMask { size: source_size, alpha: vec![0; idx(sw) * idx(sh)] };
    }
    let offsets = |dst_len: u32, src_len: u32| -> Vec<usize> {
        let inverse = 1.0 / (f64::from(dst_len) / f64::from(src_len));
        // `floor(d * inverse)` is a non-negative integer-valued float below about `src_len`, so
        // the conversion is exact; the `min` is OpenCV's own clamp.
        (0..dst_len).map(|d| idx(f64_round_to_u32((f64::from(d) * inverse).floor()).unwrap_or(src_len - 1).min(src_len - 1))).collect()
    };
    let x_ofs = offsets(sw, pw);
    let y_ofs = offsets(sh, ph);
    let mut alpha = Vec::with_capacity(idx(sw) * idx(sh));
    for &sy in &y_ofs {
        let row = &proc_mask[sy * idx(pw)..(sy + 1) * idx(pw)];
        alpha.extend(x_ofs.iter().map(|&sx| if row[sx] == 0 { 0 } else { 255 }));
    }
    BinaryMask { size: source_size, alpha }
}

/// Rounds an `f64` to the nearest `f32`: the reference keeps these values in float32 (numpy's
/// float32 mean, cv2's float corners), so the rounding is the intended semantics.
fn round_to_f32(value: f64) -> f32 {
    #[allow(clippy::cast_possible_truncation, reason = "deliberate f64 -> f32 rounding: the reference computes this value in float32")]
    let out = value as f32;
    out
}

#[cfg(test)]
mod tests;
