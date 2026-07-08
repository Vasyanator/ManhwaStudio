/*
File: crates/ms-onnx/src/paddle_ocr/db_postprocess.rs

Purpose:
Differentiable-Binarization (DB) post-processing: turns the detector probability
map into a set of 4-point text quads in original-image coordinates. Faithful port
of `DBPostProcess` in `modules/ai_backend/paddle_onnx_runtime.py`.

Key functions:
- boxes_from_bitmap : full DB pipeline (binarize -> contours -> score -> unclip -> rescale).
- box_score         : mean probability inside a rasterized quad (box_thresh gate).
- unclip_quad       : rotated-rectangle outward expansion (pyclipper replacement).

Notes:
Unclip uses a direct rotated-rectangle expansion instead of pyclipper: for a convex
quad, offsetting the polygon by `distance` (JT_ROUND) and refitting a minimum-area
rectangle is equivalent to moving each corner outward along the rectangle's two edge
normals by `distance` (i.e. growing width and height each by `2 * distance`). This
avoids a Clipper dependency while matching the box the Python path would refit.

Parity risks: imageproc's `find_contours` (Suzuki-Abe) and `min_area_rect` (integer
rotating calipers, floor/ceil corners) are close but not bit-identical to
cv2.findContours / cv2.minAreaRect; corner coordinates can differ by <=1 px, so
tests use synthetic axis-aligned blobs where the result is exact.
*/

use image::{GrayImage, Luma};
use imageproc::contours::{BorderType, find_contours};
use imageproc::drawing::draw_polygon_mut;
use imageproc::geometry::min_area_rect;
use imageproc::point::Point;

use super::{Quad, f32_to_i32_trunc, u32_to_f32};

/// Probability above which a pixel is foreground in the DB bitmap.
const DB_THRESH: f32 = 0.3;
/// Minimum mean in-quad probability for a detection to be kept.
const DB_BOX_THRESH: f32 = 0.6;
/// Maximum number of contours considered (largest-first is not guaranteed).
const DB_MAX_CANDIDATES: usize = 1000;
/// Polygon outward-expansion ratio (`distance = area * ratio / perimeter`).
const DB_UNCLIP_RATIO: f32 = 2.0;
/// Minimum rotated-rect short side before unclip.
const DB_MIN_SIZE: f32 = 3.0;
/// Minimum rotated-rect short side after unclip (`DB_MIN_SIZE + 2`).
const DB_MIN_SIZE_UNCLIPPED: f32 = 5.0;

/// Euclidean distance between two 2D points.
fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Short side (min of the two adjacent edge lengths) of an ordered quad.
fn short_side(quad: &Quad) -> f32 {
    let width = distance(quad[0], quad[1]);
    let height = distance(quad[1], quad[2]);
    width.min(height)
}

/// Shoelace area of a quad (absolute value).
fn polygon_area(quad: &Quad) -> f32 {
    let mut acc = 0.0_f32;
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        acc += a[0] * b[1] - b[0] * a[1];
    }
    (acc / 2.0).abs()
}

/// Perimeter of a quad (sum of the four edge lengths).
fn polygon_perimeter(quad: &Quad) -> f32 {
    (0..4).map(|i| distance(quad[i], quad[(i + 1) % 4])).sum()
}

/// Binarizes the probability map into a 0/255 foreground bitmap (`prob > DB_THRESH`).
fn binarize(prob: &[f32], width: usize, height: usize) -> Option<GrayImage> {
    let w = u32::try_from(width).ok()?;
    let h = u32::try_from(height).ok()?;
    let mut bitmap = GrayImage::new(w, h);
    for (i, pixel) in bitmap.pixels_mut().enumerate() {
        // pixels iterate row-major, so index i aligns with prob[i].
        let value = prob.get(i).copied().unwrap_or(0.0);
        *pixel = Luma([if value > DB_THRESH { 255 } else { 0 }]);
    }
    Some(bitmap)
}

/// Mean probability-map value inside the rasterized quad, over its bounding box.
///
/// Rasterizes `quad` (truncated to integer corners) into a local mask spanning the
/// quad's clamped bounding box and averages `prob` over the filled pixels. Returns
/// 0.0 for a degenerate box or an empty mask. Mirrors `_box_score_fast`.
#[must_use]
pub fn box_score(prob: &[f32], width: usize, height: usize, quad: &Quad) -> f32 {
    if width == 0 || height == 0 {
        return 0.0;
    }
    let w_i = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i = i32::try_from(height).unwrap_or(i32::MAX);

    let xs = [quad[0][0], quad[1][0], quad[2][0], quad[3][0]];
    let ys = [quad[0][1], quad[1][1], quad[2][1], quad[3][1]];
    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    // Clamp the bbox to [0, dim-1] exactly as Python (floor min, ceil max).
    let xmin = f32_to_i32_trunc(min_x.floor()).clamp(0, w_i - 1);
    let xmax = f32_to_i32_trunc(max_x.ceil()).clamp(0, w_i - 1);
    let ymin = f32_to_i32_trunc(min_y.floor()).clamp(0, h_i - 1);
    let ymax = f32_to_i32_trunc(max_y.ceil()).clamp(0, h_i - 1);
    if xmax < xmin || ymax < ymin {
        return 0.0;
    }

    let local_w = u32::try_from(xmax - xmin + 1).unwrap_or(0);
    let local_h = u32::try_from(ymax - ymin + 1).unwrap_or(0);
    if local_w == 0 || local_h == 0 {
        return 0.0;
    }

    // Rasterize the quad into a local mask; draw_polygon_mut fills the interior.
    let mut mask = GrayImage::new(local_w, local_h);
    let poly: Vec<Point<i32>> = quad
        .iter()
        .map(|p| {
            Point::new(
                f32_to_i32_trunc(p[0]) - xmin,
                f32_to_i32_trunc(p[1]) - ymin,
            )
        })
        .collect();
    // draw_polygon_mut panics if the first and last vertex coincide; our four
    // rotated-rect corners are distinct, but guard defensively.
    if poly.first() == poly.last() {
        return 0.0;
    }
    draw_polygon_mut(&mut mask, &poly, Luma([255]));

    let mut sum = 0.0_f32;
    let mut count = 0_u32;
    for local_y in 0..local_h {
        for local_x in 0..local_w {
            if mask.get_pixel(local_x, local_y).0[0] == 0 {
                continue;
            }
            let gx = usize::try_from(xmin).unwrap_or(0) + usize::try_from(local_x).unwrap_or(0);
            let gy = usize::try_from(ymin).unwrap_or(0) + usize::try_from(local_y).unwrap_or(0);
            if let Some(&value) = prob.get(gy * width + gx) {
                sum += value;
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / u32_to_f32(count)
    }
}

/// Expands a rotated-rectangle quad outward by `distance` on all four sides.
///
/// Moves each corner along the rectangle's two edge normals so width and height
/// each grow by `2 * distance` (see the file header for the pyclipper equivalence).
/// Returns `None` for a degenerate quad whose edges have no direction.
#[must_use]
pub fn unclip_quad(quad: &Quad, distance: f32) -> Option<Quad> {
    // Edge directions: u along the top edge (p0->p1), v along the left edge (p0->p3).
    let u_len = self::distance(quad[0], quad[1]);
    let v_len = self::distance(quad[0], quad[3]);
    if u_len <= f32::EPSILON || v_len <= f32::EPSILON {
        return None;
    }
    let u = [(quad[1][0] - quad[0][0]) / u_len, (quad[1][1] - quad[0][1]) / u_len];
    let v = [(quad[3][0] - quad[0][0]) / v_len, (quad[3][1] - quad[0][1]) / v_len];

    let d = distance;
    // p0 (TL): -u -v; p1 (TR): +u -v; p2 (BR): +u +v; p3 (BL): -u +v.
    Some([
        [quad[0][0] - d * u[0] - d * v[0], quad[0][1] - d * u[1] - d * v[1]],
        [quad[1][0] + d * u[0] - d * v[0], quad[1][1] + d * u[1] - d * v[1]],
        [quad[2][0] + d * u[0] + d * v[0], quad[2][1] + d * u[1] + d * v[1]],
        [quad[3][0] - d * u[0] + d * v[0], quad[3][1] - d * u[1] + d * v[1]],
    ])
}

/// Fits an ordered `[TL, TR, BR, BL]` quad to the integer contour points.
///
/// Uses imageproc's minimum-area rectangle (rotating calipers). Returns `None` when
/// the contour has no points.
fn mini_box(points: &[Point<i32>]) -> Option<Quad> {
    if points.is_empty() {
        return None;
    }
    let rect = min_area_rect(points);
    Some([
        [f32_from_i32(rect[0].x), f32_from_i32(rect[0].y)],
        [f32_from_i32(rect[1].x), f32_from_i32(rect[1].y)],
        [f32_from_i32(rect[2].x), f32_from_i32(rect[2].y)],
        [f32_from_i32(rect[3].x), f32_from_i32(rect[3].y)],
    ])
}

/// Lossless `i32` -> `f32` for small pixel coordinates (well within 2^24).
fn f32_from_i32(v: i32) -> f32 {
    // Pixel coordinates are far below f32's 2^24 exact-integer limit.
    #[allow(clippy::cast_precision_loss)]
    let out = v as f32;
    out
}

/// Rescales an original-space quad from bitmap coordinates and clamps to the image.
fn rescale_quad(quad: &Quad, width_scale: f32, height_scale: f32, dest_w: f32, dest_h: f32) -> Quad {
    let map = |p: [f32; 2]| {
        [
            (p[0] * width_scale).round().clamp(0.0, dest_w),
            (p[1] * height_scale).round().clamp(0.0, dest_h),
        ]
    };
    [map(quad[0]), map(quad[1]), map(quad[2]), map(quad[3])]
}

/// Runs the full DB post-processing pipeline on a probability map.
///
/// `prob` is the row-major `width * height` DB probability map; `dest_w`/`dest_h`
/// are the ORIGINAL image dimensions the returned quads are rescaled to. Quads are
/// `[TL, TR, BR, BL]` in original-image f32 coordinates, filtered by the box-score
/// gate and expanded (unclipped). Mirrors `_boxes_from_bitmap`.
#[must_use]
pub fn boxes_from_bitmap(
    prob: &[f32],
    width: usize,
    height: usize,
    dest_w: u32,
    dest_h: u32,
) -> Vec<Quad> {
    let Some(bitmap) = binarize(prob, width, height) else {
        return Vec::new();
    };

    let width_scale = u32_to_f32(dest_w) / u32_to_f32(u32::try_from(width.max(1)).unwrap_or(1));
    let height_scale = u32_to_f32(dest_h) / u32_to_f32(u32::try_from(height.max(1)).unwrap_or(1));
    let dest_width_f = u32_to_f32(dest_w);
    let dest_height_f = u32_to_f32(dest_h);

    let contours = find_contours::<i32>(&bitmap);
    let mut quads = Vec::new();
    for contour in contours.iter().filter(|c| c.border_type == BorderType::Outer) {
        if quads.len() >= DB_MAX_CANDIDATES {
            break;
        }
        let Some(quad) = mini_box(&contour.points) else {
            continue;
        };
        if short_side(&quad) < DB_MIN_SIZE {
            continue;
        }
        if box_score(prob, width, height, &quad) < DB_BOX_THRESH {
            continue;
        }

        let perimeter = polygon_perimeter(&quad);
        if perimeter < 1e-6 {
            continue;
        }
        let distance = polygon_area(&quad) * DB_UNCLIP_RATIO / perimeter;
        let Some(expanded) = unclip_quad(&quad, distance) else {
            continue;
        };
        if short_side(&expanded) < DB_MIN_SIZE_UNCLIPPED {
            continue;
        }

        quads.push(rescale_quad(&expanded, width_scale, height_scale, dest_width_f, dest_height_f));
    }

    quads
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `width*height` probability map with a filled rectangular blob of
    /// value `p` inside `[x0, x1) x [y0, y1)` and 0 elsewhere.
    fn blob_map(
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
        p: f32,
    ) -> Vec<f32> {
        let mut m = vec![0.0_f32; width * height];
        for y in y0..y1 {
            for x in x0..x1 {
                m[y * width + x] = p;
            }
        }
        m
    }

    #[test]
    fn box_score_averages_prob_inside_quad() {
        // 10x10 map, left half = 0.8. Quad over columns [0,5) rows [0,10).
        let map = blob_map(10, 10, 0, 0, 5, 10, 0.8);
        let quad: Quad = [[0.0, 0.0], [4.0, 0.0], [4.0, 9.0], [0.0, 9.0]];
        let score = box_score(&map, 10, 10, &quad);
        // Every sampled pixel is 0.8.
        assert!((score - 0.8).abs() < 1e-6, "score={score}");
    }

    #[test]
    fn unclip_grows_rect_by_twice_distance() {
        // Axis-aligned 40x10 quad, distance 3 -> width 46, height 16.
        let quad: Quad = [[10.0, 10.0], [50.0, 10.0], [50.0, 20.0], [10.0, 20.0]];
        let expanded = unclip_quad(&quad, 3.0).expect("non-degenerate quad");
        let new_w = (expanded[1][0] - expanded[0][0]).abs();
        let new_h = (expanded[3][1] - expanded[0][1]).abs();
        assert!((new_w - 46.0).abs() < 1e-4, "new_w={new_w}");
        assert!((new_h - 16.0).abs() < 1e-4, "new_h={new_h}");
        // Corners moved outward: TL up-left, BR down-right.
        assert!(expanded[0][0] < 10.0 && expanded[0][1] < 10.0);
        assert!(expanded[2][0] > 50.0 && expanded[2][1] > 20.0);
    }

    #[test]
    fn boxes_from_bitmap_extracts_single_blob() {
        // 60x40 map with a strong 20x12 rectangular blob at (10,8)-(30,20).
        let (w, h) = (60_usize, 40_usize);
        let map = blob_map(w, h, 10, 8, 30, 20, 0.95);
        let quads = boxes_from_bitmap(&map, w, h, 60, 40);
        assert_eq!(quads.len(), 1, "expected exactly one detection");
        // The unclipped box must contain the original blob and stay in-image.
        let q = &quads[0];
        let min_x = q.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min);
        let max_x = q.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max);
        let min_y = q.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
        let max_y = q.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max);
        // Unclip expands outward from the blob edges (10..30, 8..20).
        assert!(min_x <= 10.0 && max_x >= 29.0, "x range {min_x}..{max_x}");
        assert!(min_y <= 8.0 && max_y >= 19.0, "y range {min_y}..{max_y}");
        assert!(min_x >= 0.0 && max_x <= 60.0 && min_y >= 0.0 && max_y <= 40.0);
    }

    #[test]
    fn boxes_from_bitmap_rejects_low_score_blob() {
        // Blob probability below box_thresh (0.6) -> rejected even though > thresh.
        let (w, h) = (40_usize, 30_usize);
        let map = blob_map(w, h, 5, 5, 25, 18, 0.45);
        let quads = boxes_from_bitmap(&map, w, h, 40, 30);
        assert!(quads.is_empty(), "low-score blob must be filtered out");
    }
}
