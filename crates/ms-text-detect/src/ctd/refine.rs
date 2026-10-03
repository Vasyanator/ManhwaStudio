/*
File: crates/ms-text-detect/src/ctd/refine.rs

Purpose:
Port of the comic-text-detector mask refinement `refine_mask` (former
`modules/ai_backend/detection/textdetector/ctd/textmask.py:27-168`, with `enlarge_window` from
`textdetector/td_utlis.py:109-132`): per block, binarization candidates of the page window are
merged component by component wherever they agree better with the predicted seg mask.

Key functions:
- `refine_mask()`: blocks + page + source-size seg map -> refined 0/255 mask.
- `enlarge_window()`: the 2.5x-area block window, clamped symmetrically.
- `topk_colors()`: the "colours" of the grey histogram (left bin edges, quirk 2).
- `grey_bgr_quirk()`: the grey value upstream computes from a BGR pixel.

Notes:
The page here is RGB; upstream worked on the BGR `cv2.imdecode` buffer. Quirks reproduced on
purpose (numbered as in `fixtures/README.md`): (1) grey = `COLOR_RGB2GRAY` applied to BGR, (2) the
histogram "colours" are float left edges of 255 bins over `[min, max]`, (3) a STABLE descending
count sort (upstream's argsort is unstable; the fixtures avoid ties), (4) `inRange` bounds rounded
half to even and saturated, (5) Otsu over B, G, R in that order, lowest channel on ties,
non-inverted form on xor ties, (6) 8-connectivity, (7) cross erode + `> 60` of the prediction,
`w * h < 3` component skip, 5x5 square dilate, hole fill under the second-largest area,
(8) `enlarge_window` with half-to-even `round` and a symmetric clamp.
Every keep test of a component reduces to its own pixels: OR-ing the component changes the xor
against the binarized prediction only where the merged mask was 0, by +255 on a 0 prediction
pixel and -255 on a 255 one, so it is evaluated as a signed pixel count.
*/

use image::GrayImage;
use ms_raster::{dilate_square, otsu_threshold};

use super::components::label_8;
use crate::glyph_mask::erode_3x3;
use crate::num::idx;

/// Area ratio of the refinement window over the block (`enlarge_window(ratio=2.5)`).
const WINDOW_AREA_RATIO: f64 = 2.5;
/// Half width of the grey band around a histogram colour (`color_range`).
const COLOR_RANGE: f64 = 30.0;
/// Minimum distance between two picked colours (`color_var`).
const COLOR_VAR: f64 = 10.0;
/// Colours picked from the histogram at most (`k`).
const TOP_K: usize = 3;
/// Bin count share below which the colour scan stops (`bin_tol`).
const BIN_TOL: f64 = 0.001;
/// Histogram bin count (`np.histogram(bins=255)`).
const BINS: usize = 255;
/// Seg level above which the eroded prediction counts as text in the merge (`threshold(.., 60)`).
const PRED_MERGE_THRESHOLD: u8 = 60;
/// Seg level above which the square-eroded prediction selects histogram samples (`> 127`).
const PRED_SAMPLE_THRESHOLD: u8 = 127;
/// Components whose bounding box covers fewer pixels are ignored (`w * h < 3`).
const MIN_COMPONENT_BOX: usize = 3;
/// Radius of the 5x5 square dilate of the merged mask (`REFINEMASK_INPAINT`).
const MERGE_DILATE_RADIUS: usize = 2;

/// Grey of an RGB page pixel as upstream computed it: the pixel was BGR in memory and
/// `COLOR_RGB2GRAY` weighted it as RGB, i.e. `0.299 B + 0.587 G + 0.114 R` in `OpenCV`'s 15-bit fixed
/// point (quirk 1).
pub(super) fn grey_bgr_quirk(rgb: [u8; 3]) -> u8 {
    let [r, g, b] = rgb.map(u32::from);
    u8::try_from((b * 9798 + g * 19235 + r * 3735 + 16384) >> 15).unwrap_or(u8::MAX)
}

/// The refinement window of block `[x1, y1, x2, y2]` on an `im_w x im_h` page: grown by `delta`
/// on every side so the area is about 2.5x, where `delta = round_half_even(root / 2)` and `root`
/// is the larger root of `x^2 + (w + h) x - 1.5 w h = 0` (closed form instead of `np.roots`), then
/// clamped SYMMETRICALLY per axis by the distance to the nearer border (quirk 8). `None` for an
/// empty block or window.
pub(super) fn enlarge_window(block: [u32; 4], im_w: u32, im_h: u32) -> Option<[u32; 4]> {
    let [x1, y1, x2, y2] = block.map(i64::from);
    let (w, h) = (x2 - x1, y2 - y1);
    if w <= 0 || h <= 0 {
        return None;
    }
    let (wf, hf) = (i64_to_f64(w), i64_to_f64(h));
    let b = wf + hf;
    let root = f64::midpoint(-b, (b * b + 4.0 * (WINDOW_AREA_RATIO - 1.0) * wf * hf).sqrt());
    let delta = f64_to_i64((root / 2.0).round_ties_even());
    let delta_x = delta.min(x1).min(i64::from(im_w) - x2);
    let delta_y = delta.min(y1).min(i64::from(im_h) - y2);
    let clamp = |v: i64, max: u32| u32::try_from(v.clamp(0, i64::from(max))).unwrap_or(0);
    let window = [clamp(x1 - delta_x, im_w), clamp(y1 - delta_y, im_h), clamp(x2 + delta_x, im_w), clamp(y2 + delta_y, im_h)];
    (window[2] > window[0] && window[3] > window[1]).then_some(window)
}

/// The top-k "colours" of the grey samples `px` (quirk 2): `np.histogram(px, bins=255)` edges over
/// `[min, max]` (`[min - 0.5, max + 0.5]` for one value, `[0, 1]` for none), the left edges of the
/// bins ordered by count (descending, stable), each kept when it is more than 10 away from every
/// colour kept so far; the scan stops after 3 colours or at a bin below 0.1 % of the samples.
pub(super) fn topk_colors(px: &[u8]) -> Vec<f64> {
    let (first, last) = match (px.iter().min(), px.iter().max()) {
        (Some(&lo), Some(&hi)) if lo == hi => (f64::from(lo) - 0.5, f64::from(hi) + 0.5),
        (Some(&lo), Some(&hi)) => (f64::from(lo), f64::from(hi)),
        _ => (0.0, 1.0),
    };
    // np.linspace(first, last, 256): i * step + first, the last edge set to `last` exactly.
    let step = (last - first) / usize_to_f64(BINS);
    let mut edges: Vec<f64> = (0..=BINS).map(|i| usize_to_f64(i) * step + first).collect();
    edges[BINS] = last;
    let mut counts = [0_u64; BINS];
    for &v in px {
        let value = f64::from(v);
        // numpy's uniform-bin index, then its one-ulp corrections against the edges.
        let mut bin = f64_to_index((value - first) / (last - first) * usize_to_f64(BINS)).min(BINS - 1);
        if value < edges[bin] && bin > 0 {
            bin -= 1;
        }
        if bin + 1 < BINS && value >= edges[bin + 1] {
            bin += 1;
        }
        counts[bin] += 1;
    }
    let mut order: Vec<usize> = (0..BINS).collect();
    order.sort_by(|a, b| counts[*b].cmp(&counts[*a]));
    let total: u64 = counts.iter().sum();
    let tol = u64_to_f64(total) * BIN_TOL;
    let mut colors = vec![edges[order[0]]];
    for &bin in &order[1..] {
        let color = edges[bin];
        if colors.iter().map(|c| (c - color).abs()).fold(f64::INFINITY, f64::min) > COLOR_VAR {
            colors.push(color);
        }
        if colors.len() >= TOP_K || u64_to_f64(counts[bin]) < tol {
            break;
        }
    }
    colors
}

/// The page window of one block: per-pixel planes the candidates are built from.
struct Window {
    /// Window width and height.
    w: usize,
    h: usize,
    /// Quirk grey (see [`grey_bgr_quirk`]).
    grey: Vec<u8>,
    /// Colour planes in upstream channel order B, G, R.
    bgr: [Vec<u8>; 3],
    /// The seg map (prediction) over the window, raw levels.
    pred: Vec<u8>,
}

/// A binarization candidate and its xor sum against the raw prediction.
type Candidate = (Vec<u8>, u64);

/// `minxor_thresh`: the candidate or its inverse, whichever xors less with `pred` (the
/// non-inverted form on a tie). Sums are over raw bytes.
fn minxor(threshed: Vec<u8>, pred: &[u8]) -> Candidate {
    let xor_sum = |inverted: bool| -> u64 { threshed.iter().zip(pred).map(|(&t, &p)| u64::from(if inverted { 255 - t } else { t } ^ p)).sum() };
    let (plain, inverse) = (xor_sum(false), xor_sum(true));
    if inverse < plain { (threshed.into_iter().map(|t| 255 - t).collect(), inverse) } else { (threshed, plain) }
}

/// `get_topk_masklist`: one `inRange` band per histogram colour of the grey samples under the
/// square-eroded prediction (`> 127`).
fn topk_candidates(win: &Window) -> Vec<Candidate> {
    let eroded = erode_square_3x3(&win.pred, win.w, win.h);
    let samples: Vec<u8> = win.grey.iter().zip(&eroded).filter(|(_, e)| **e > PRED_SAMPLE_THRESHOLD).map(|(g, _)| *g).collect();
    topk_colors(&samples)
        .into_iter()
        .map(|color| {
            let top = (color + COLOR_RANGE).min(255.0);
            let (lo, hi) = (inrange_bound(top - 2.0 * COLOR_RANGE), inrange_bound(top));
            let threshed = win.grey.iter().map(|&g| if (lo..=hi).contains(&g) { 255 } else { 0 }).collect();
            minxor(threshed, &win.pred)
        })
        .collect()
}

/// `get_otsuthresh_masklist(per_channel=False)`: the Otsu binarization (`v > t`, no split -> 0)
/// of the B, G, R planes, each in its better form; the best of the three (lowest channel on ties).
fn otsu_candidate(win: &Window) -> Candidate {
    let mut best: Option<Candidate> = None;
    for plane in &win.bgr {
        let t = otsu_threshold(plane).unwrap_or(0);
        let candidate = minxor(plane.iter().map(|&v| if v > t { 255 } else { 0 }).collect(), &win.pred);
        if best.as_ref().is_none_or(|b| candidate.1 < b.1) {
            best = Some(candidate);
        }
    }
    best.unwrap_or_default()
}

/// Fixture intermediates of one window: the top-k colours, the B, G, R Otsu thresholds and the
/// candidate xor sums in merge order (`refine_blocks` of the CTD `case.json`).
#[cfg(test)]
pub(super) fn window_trace(page: &image::RgbImage, seg: &[u8], window: [u32; 4]) -> (Vec<f64>, [u8; 3], Vec<u64>) {
    let win = crop(page, seg, window);
    let eroded = erode_square_3x3(&win.pred, win.w, win.h);
    let samples: Vec<u8> = win.grey.iter().zip(&eroded).filter(|(_, e)| **e > PRED_SAMPLE_THRESHOLD).map(|(g, _)| *g).collect();
    let otsu = [0, 1, 2].map(|c| otsu_threshold(&win.bgr[c]).unwrap_or(0));
    let mut sums: Vec<u64> = topk_candidates(&win).into_iter().map(|c| c.1).collect();
    sums.push(otsu_candidate(&win).1);
    sums.sort_unstable();
    (topk_colors(&samples), otsu, sums)
}

/// `merge_mask_list(refine_mode=REFINEMASK_INPAINT)` for one window (quirk 7).
fn merge(mut candidates: Vec<Candidate>, win: &Window) -> Vec<u8> {
    let (w, h) = (win.w, win.h);
    candidates.sort_by_key(|c| c.1);
    // `crop` fills `pred` with exactly `w * h` levels, so `from_raw` cannot fail here.
    let pred_gray = GrayImage::from_raw(u32_len(w), u32_len(h), win.pred.clone()).unwrap_or_default();
    let pred: Vec<u8> = erode_3x3(&pred_gray).into_raw().into_iter().map(|v| if v > PRED_MERGE_THRESHOLD { 255 } else { 0 }).collect();
    let mut merged = vec![0_u8; w * h];
    for (candidate, _) in &candidates {
        let comps = label_8(candidate, w, h);
        let mut gain = vec![0_i64; comps.stats.len()];
        for (i, &label) in comps.labels.iter().enumerate() {
            if label != 0 && merged[i] == 0 {
                gain[idx(label) - 1] += if pred[i] == 0 { 1 } else { -1 };
            }
        }
        let keep: Vec<bool> = comps.stats.iter().zip(&gain).map(|(s, &g)| s.box_area() >= MIN_COMPONENT_BOX && g < 0).collect();
        for (i, &label) in comps.labels.iter().enumerate() {
            if label != 0 && keep[idx(label) - 1] {
                merged[i] = 255;
            }
        }
    }
    // `dilate_square` fails only on `len != w * h`, impossible for `merged` (built as `w * h`).
    let mut merged = dilate_square(&merged, w, h, MERGE_DILATE_RADIUS, MERGE_DILATE_RADIUS, 255).unwrap_or(merged);
    // Hole fill: components of the inverse; OpenCV's stats include label 0 (the merged pixels),
    // and the area threshold is the second largest of all areas.
    let inverse: Vec<u8> = merged.iter().map(|&v| 255 - v).collect();
    let holes = label_8(&inverse, w, h);
    let mut areas: Vec<usize> = std::iter::once(holes.background_area).chain(holes.stats.iter().map(|s| s.area)).collect();
    areas.sort_unstable();
    let area_thresh = if areas.len() > 1 { areas[areas.len() - 2] } else { areas[areas.len() - 1] };
    let mut gain = vec![0_i64; holes.stats.len()];
    for (i, &label) in holes.labels.iter().enumerate() {
        if label != 0 {
            gain[idx(label) - 1] += if pred[i] == 0 { 1 } else { -1 };
        }
    }
    let fill: Vec<bool> = holes.stats.iter().zip(&gain).map(|(s, &g)| s.area < area_thresh && g < 0).collect();
    for (i, &label) in holes.labels.iter().enumerate() {
        if label != 0 && fill[idx(label) - 1] {
            merged[i] = 255;
        }
    }
    merged
}

/// Copies the window planes out of the page and the source-size seg map (`seg` may be empty for
/// callers that only need the colour planes).
fn crop(page: &image::RgbImage, seg: &[u8], window: [u32; 4]) -> Window {
    let [x1, y1, x2, y2] = window.map(idx);
    let (w, h) = (x2 - x1, y2 - y1);
    let page_w = idx(page.width());
    let raw = page.as_raw();
    let mut win = Window { w, h, grey: Vec::with_capacity(w * h), bgr: [Vec::with_capacity(w * h), Vec::with_capacity(w * h), Vec::with_capacity(w * h)], pred: Vec::with_capacity(w * h) };
    for y in y1..y2 {
        let row = &raw[(y * page_w + x1) * 3..(y * page_w + x2) * 3];
        for px in row.chunks_exact(3) {
            let rgb = [px[0], px[1], px[2]];
            win.grey.push(grey_bgr_quirk(rgb));
            win.bgr[0].push(rgb[2]);
            win.bgr[1].push(rgb[1]);
            win.bgr[2].push(rgb[0]);
        }
        if let Some(pred_row) = seg.get(y * page_w + x1..y * page_w + x2) {
            win.pred.extend_from_slice(pred_row);
        }
    }
    win
}

/// Refines the CTD seg mask over the page: for every block, the merged candidates of its
/// enlarged window are OR-ed into a page-size 0/255 mask (row-major, `page` size).
///
/// `seg` is the seg map resized to the page (`page.width() * page.height()` levels); a shorter
/// buffer yields an all-zero mask. Blocks are integer `[x1, y1, x2, y2]` in page pixels; empty
/// ones are skipped. Order of blocks does not matter.
pub(super) fn refine_mask(page: &image::RgbImage, seg: &[u8], blocks: &[[u32; 4]]) -> Vec<u8> {
    let (im_w, im_h) = (page.width(), page.height());
    let page_w = idx(im_w);
    let mut refined = vec![0_u8; page_w * idx(im_h)];
    if seg.len() != refined.len() {
        return refined;
    }
    for &block in blocks {
        let Some(window) = enlarge_window(block, im_w, im_h) else {
            continue;
        };
        let win = crop(page, seg, window);
        let mut candidates = topk_candidates(&win);
        candidates.push(otsu_candidate(&win));
        let merged = merge(candidates, &win);
        let [x1, y1, ..] = window.map(idx);
        for (row, values) in merged.chunks_exact(win.w).enumerate() {
            let start = (y1 + row) * page_w + x1;
            for (dst, &v) in refined[start..start + win.w].iter_mut().zip(values) {
                *dst |= v;
            }
        }
    }
    refined
}

/// 3x3 square erode with out-of-bounds neighbours ignored (`cv2.erode(msk, np.ones((3, 3)))`).
fn erode_square_3x3(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0_u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = u8::MAX;
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    acc = acc.min(src[ny * w + nx]);
                }
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// An `inRange` scalar bound for 8-bit input: rounded half to even, saturated (quirk 4).
fn inrange_bound(value: f64) -> u8 {
    let rounded = value.round_ties_even().clamp(0.0, 255.0);
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "integral value clamped to 0..=255")]
    let out = rounded as u8;
    out
}

/// Window side as `u32` (windows are cut from a `u32`-sized page).
fn u32_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// `i64` pixel extent to `f64` (exact below 2^53).
fn i64_to_f64(value: i64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "pixel extents stay far below 2^53")]
    let out = value as f64;
    out
}

/// Integral `f64` to `i64` (saturating; the window delta is a small pixel count).
fn f64_to_i64(value: f64) -> i64 {
    #[expect(clippy::cast_possible_truncation, reason = "integral pixel delta far inside the i64 range")]
    let out = value as i64;
    out
}

/// `usize` count to `f64` (exact below 2^53).
fn usize_to_f64(value: usize) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "bin indices stay tiny")]
    let out = value as f64;
    out
}

/// `u64` sample count to `f64` (exact below 2^53).
fn u64_to_f64(value: u64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "sample counts stay far below 2^53")]
    let out = value as f64;
    out
}

/// Non-negative `f64` bin coordinate truncated to an index (numpy `astype(np.intp)`).
fn f64_to_index(value: f64) -> usize {
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "bin coordinate in 0..=255, truncation is numpy's astype")]
    let out = value as usize;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enlarge_window_grows_area_and_clamps_symmetrically() {
        // 20x10 block: root of x^2 + 30x - 300 = 0 is 8.0277, delta = round(4.01) = 4.
        assert_eq!(enlarge_window([50, 50, 70, 60], 200, 200), Some([46, 46, 74, 64]));
        // Near the left border the x growth is limited to x1 on BOTH sides.
        assert_eq!(enlarge_window([2, 50, 22, 60], 200, 200), Some([0, 46, 24, 64]));
        assert_eq!(enlarge_window([5, 5, 5, 9], 20, 20), None);
    }

    #[test]
    fn topk_colors_follow_numpy_histogram_edges() {
        assert_eq!(topk_colors(&[]), vec![0.0]);
        // One value: edges over [39.5, 40.5]; 40 falls in bin 127, whose left edge is reported.
        assert_eq!(topk_colors(&[40; 7]), vec![127.0 * (1.0 / 255.0) + 39.5]);
        // Two clusters: 0 (x5) and 200 (x3); edges over [0, 200], step 200/255.
        let colors = topk_colors(&[0, 0, 0, 0, 0, 200, 200, 200]);
        assert_eq!(colors.len(), 2);
        assert!((colors[0] - 0.0).abs() < 1e-12);
        assert!((colors[1] - 254.0 * 200.0 / 255.0).abs() < 1e-9, "{colors:?}");
    }

    #[test]
    fn minxor_prefers_the_plain_form_on_ties() {
        let (mask, sum) = minxor(vec![255, 0], &[255, 255]);
        assert_eq!((mask, sum), (vec![255, 0], 255));
        let (mask, sum) = minxor(vec![0, 0, 255], &[255, 255, 0]);
        assert_eq!((mask, sum), (vec![255, 255, 0], 0));
    }

    #[test]
    fn grey_uses_bgr_weights_on_rgb_input() {
        assert_eq!(grey_bgr_quirk([255, 0, 0]), 29);
        assert_eq!(grey_bgr_quirk([0, 0, 255]), 76);
        assert_eq!(grey_bgr_quirk([255, 255, 255]), 255);
    }
}
