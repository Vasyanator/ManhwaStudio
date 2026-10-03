/*
File: crates/ms-text-detect/src/db.rs

Purpose:
The DB (differentiable binarization) postprocess: probability map -> binarized bitmap ->
contours -> scored, unclipped quadrilaterals in source pixels. One implementation, two parameter
presets: `DbParams::PADDLE` (port of `DBPostProcess` in the former
`modules/ai_backend/engines/paddle_onnx.py`) and `DbParams::CTD` (port of the comic-text-detector
`SegDetectorRepresenter.boxes_from_bitmap`, former `detection/textdetector/db_utils.py:127-170`,
plus the `score > 0.6` filter of `ctd/inference.py:258-260`).

Key structures:
- Quad        : four corner points `[TL, TR, BR, BL]` in image pixels.
- ProbValue   : a probability-map sample, `f32` (OCR path, raw model output) or `u8` (`p * 255`,
                the stitched detector maps of the pipeline).
- DbParams    : thresholds, score region and gate, unclip ratio, size gates, contour set and the
                geometry switch; `PADDLE` and `CTD` presets.
- ScoreRegion / ScoreGate / ContourSet : the per-preset choices inside `DbParams`.

Key functions:
- boxes_from_bitmap        : full DB pipeline with `DbParams::PADDLE`.
- boxes_from_bitmap_with   : full DB pipeline with explicit parameters.
- box_score                : mean probability inside a rasterized quad.
- unclip_quad              : rotated-rectangle outward expansion (pyclipper replacement).
- block_from_quad          : axis-aligned `[x1, y1, x2, y2]` box of a quad, clamped to the image.

Submodule:
- db/geometry.rs : float `minAreaRect` (the crate's one owner, also used by `surya.rs`) and the
                   pyclipper round offset (OpenCV geometry path).

Notes:
Two geometry paths, chosen by `DbParams::opencv_geometry`:
- Float path (Paddle): imageproc's integer `min_area_rect`, then a direct rotated-rectangle
  expansion instead of pyclipper: for a convex quad, offsetting the polygon by `distance`
  (`JT_ROUND`) and refitting a minimum-area rectangle is equivalent to moving each corner outward
  along the rectangle's two edge normals by `distance`. Rescale `round(x * dest / map)`. Pinned by
  the characterization tests below.
- OpenCV path (CTD): float `minAreaRect`, the real pyclipper offset on the truncated corners
  (integer output path), a second float `minAreaRect`, rescale `round_ties_even(x / map * dest)`
  in `f32` like `np.round` on float32. Matches the CTD golden fixtures to the pixel.

Polygon interiors (the score region) are filled by `ms_raster::fill_polygon_spans`, the project's
one polygon rasterizer (scanline centre `y + 0.5`, inclusive `ceil`/`floor` column ends). It
covers slightly fewer edge pixels than `cv2.fillPoly` (which also paints the polygon outline), so
scores differ from the Python ones in the third decimal; the CTD golden fixtures pin the effect.

Parity risks: imageproc's `find_contours` (Suzuki-Abe) traces the same borders as
`cv2.findContours`, in a different order (only matters past `max_candidates`). On the float path
imageproc's `min_area_rect` (integer rotating calipers, floor/ceil corners) differs from
`cv2.minAreaRect` by <= 1 px per corner.
*/

use image::{GrayImage, Luma};
use imageproc::contours::{BorderType, find_contours};
use imageproc::geometry::min_area_rect;
use imageproc::point::Point;
use ms_raster::fill_polygon_spans;

use crate::num::{f32_from_i32, f32_to_i32_trunc, u32_to_f32};

// The OpenCV / pyclipper geometry of `DbParams::opencv_geometry`; crate-visible because its float
// `minAreaRect` (`min_area_rect_f`) is also the Surya postprocess's fit (`db/geometry.rs`).
pub(crate) mod geometry;

/// A detected text region: four corner points `[TL, TR, BR, BL]` in image pixels.
pub type Quad = [[f32; 2]; 4];

/// Which polygon a candidate's score is averaged over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreRegion {
    /// The minimum-area quad, corners truncated toward zero (Paddle `_box_score_fast`).
    Quad,
    /// The traced contour itself (CTD `box_score_fast(pred, contour)`).
    Contour,
}

/// How the candidate score decides whether a box is kept.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScoreGate {
    /// Keep when `score >= value` (Paddle: `box_thresh > score` skips).
    AtLeast(f32),
    /// Keep when `score > value` (CTD: `scores > box_thresh` filter after the representer).
    Above(f32),
}

impl ScoreGate {
    /// Whether a candidate with `score` passes the gate.
    fn keeps(self, score: f32) -> bool {
        match self {
            Self::AtLeast(value) => score >= value,
            Self::Above(value) => score > value,
        }
    }
}

/// Which traced contours become candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContourSet {
    /// Outer borders only.
    Outer,
    /// Outer and hole borders (`cv2.RETR_LIST`).
    All,
}

/// Parameters of one DB postprocess flavour. Use the presets; the fields are public so a test or
/// diagnostic can vary one knob.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DbParams {
    /// Probability above which a pixel is foreground in the bitmap (`prob > thresh`).
    pub thresh: f32,
    /// Polygon the score is averaged over.
    pub score_region: ScoreRegion,
    /// Score gate.
    pub score_gate: ScoreGate,
    /// Unclip distance ratio (`distance = area * ratio / perimeter` of the min-area quad).
    pub unclip_ratio: f32,
    /// Maximum number of KEPT quads; the scan stops once reached (contour order is imageproc's,
    /// not `OpenCV`'s, so which candidates fall past the cap is not reproduced anyway).
    pub max_candidates: usize,
    /// Candidates whose min-area quad has a shorter side below this are skipped.
    pub min_side_pre: f32,
    /// After unclip, boxes with a shorter side below this are dropped (`None`: no gate).
    pub min_side_post: Option<f32>,
    /// Which contours are candidates.
    pub contours: ContourSet,
    /// Reproduce the `OpenCV` / pyclipper geometry (`db/geometry.rs`): float `minAreaRect`, the
    /// pyclipper round offset of the truncated corners, a second `minAreaRect`, and the rescale
    /// `round_ties_even(x / map * dest)`. `false` keeps imageproc's integer min-area quad, the
    /// rotated-rectangle expansion and `round(x * (dest / map))` (half away from zero).
    pub opencv_geometry: bool,
}

impl DbParams {
    /// `PaddleOCR` detection (`parse_det_config` defaults): thresh 0.3, quad score `>= 0.6` before
    /// unclip, unclip 2.0, at most 1000 boxes, short side >= 3 before and >= 5 after unclip,
    /// outer contours, float geometry path.
    pub const PADDLE: Self = Self {
        thresh: 0.3,
        score_region: ScoreRegion::Quad,
        score_gate: ScoreGate::AtLeast(0.6),
        unclip_ratio: 2.0,
        max_candidates: 1000,
        min_side_pre: 3.0,
        min_side_post: Some(5.0),
        contours: ContourSet::Outer,
        opencv_geometry: false,
    };

    /// Comic-text-detector line map (`SegDetectorRepresenter(thresh=0.3)`, unclip 1.5,
    /// `max_candidates` 1000): contour score with the `> 0.6` filter of `inference.py`, candidates
    /// with a min-area short side below 2 skipped, no post-unclip size gate, every contour of
    /// `RETR_LIST`, `OpenCV` / pyclipper geometry.
    pub const CTD: Self = Self {
        thresh: 0.3,
        score_region: ScoreRegion::Contour,
        score_gate: ScoreGate::Above(0.6),
        unclip_ratio: 1.5,
        max_candidates: 1000,
        min_side_pre: 2.0,
        min_side_post: None,
        contours: ContourSet::All,
        opencv_geometry: true,
    };
}

/// A probability-map sample accepted by the DB postprocess.
///
/// `f32` is a raw probability (the native OCR detection path); `u8` is a quantized probability
/// `round(p * 255)` (the stitched maps of the detection pipeline). The `f32` impl is the identity,
/// so the f32 path computes exactly what it did before the trait existed.
pub trait ProbValue: Copy {
    /// The sample as a probability in `0.0..=1.0` (for `f32`, the value itself, unclamped).
    fn to_prob(self) -> f32;
}

impl ProbValue for f32 {
    fn to_prob(self) -> f32 {
        self
    }
}

impl ProbValue for u8 {
    fn to_prob(self) -> f32 {
        f32::from(self) / 255.0
    }
}

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

/// Binarizes the probability map into a 0/255 foreground bitmap (`prob > thresh`).
fn binarize<P: ProbValue>(prob: &[P], width: usize, height: usize, thresh: f32) -> Option<GrayImage> {
    let w = u32::try_from(width).ok()?;
    let h = u32::try_from(height).ok()?;
    let mut bitmap = GrayImage::new(w, h);
    for (i, pixel) in bitmap.pixels_mut().enumerate() {
        // pixels iterate row-major, so index i aligns with prob[i].
        let value = prob.get(i).map_or(0.0, |v| v.to_prob());
        *pixel = Luma([if value > thresh { 255 } else { 0 }]);
    }
    Some(bitmap)
}

/// Mean of `prob` over the pixels `fill_polygon_spans` fills for `points` (map coordinates,
/// clamped to the map). Returns 0.0 for an empty map or when no pixel is filled.
fn polygon_mean<P: ProbValue>(prob: &[P], width: usize, height: usize, points: &[(f32, f32)]) -> f32 {
    let mut sum = 0.0_f32;
    let mut count = 0_u32;
    fill_polygon_spans(points, width, height, |y, x0, x1| {
        let row = y * width;
        if let Some(values) = prob.get(row + x0..=row + x1) {
            for value in values {
                sum += value.to_prob();
                count += 1;
            }
        }
    });
    if count == 0 { 0.0 } else { sum / u32_to_f32(count) }
}

/// Mean probability-map value inside the rasterized quad.
///
/// The quad's corners are truncated to integers (Python `astype(np.int32)`) and the polygon is
/// filled with `ms_raster::fill_polygon_spans`, clamped to the map. Returns 0.0 for an empty map
/// or when no pixel is filled. Mirrors `_box_score_fast` up to the fill rule (file header).
#[must_use]
pub fn box_score<P: ProbValue>(prob: &[P], width: usize, height: usize, quad: &Quad) -> f32 {
    let points: Vec<(f32, f32)> = quad.iter().map(|p| (f32_from_i32(f32_to_i32_trunc(p[0])), f32_from_i32(f32_to_i32_trunc(p[1])))).collect();
    polygon_mean(prob, width, height, &points)
}

/// Mean probability-map value inside a traced contour (integer boundary pixels in order).
fn contour_score<P: ProbValue>(prob: &[P], width: usize, height: usize, contour: &[Point<i32>]) -> f32 {
    let points: Vec<(f32, f32)> = contour.iter().map(|p| (f32_from_i32(p.x), f32_from_i32(p.y))).collect();
    polygon_mean(prob, width, height, &points)
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

/// Rescales a quad from bitmap coordinates to the destination and clamps it to `[0, dest]`.
///
/// Float grid: `round(p * (dest / map))`, half away from zero (today's Paddle path). Integer
/// grid: `round_ties_even(p / map * dest)` in `f32`, the `np.round` of the CTD representer.
fn rescale_quad(quad: &Quad, map_size: [f32; 2], dest: [f32; 2], opencv_geometry: bool) -> Quad {
    let map = |p: [f32; 2]| {
        if opencv_geometry {
            [(p[0] / map_size[0] * dest[0]).round_ties_even().clamp(0.0, dest[0]), (p[1] / map_size[1] * dest[1]).round_ties_even().clamp(0.0, dest[1])]
        } else {
            [(p[0] * (dest[0] / map_size[0])).round().clamp(0.0, dest[0]), (p[1] * (dest[1] / map_size[1])).round().clamp(0.0, dest[1])]
        }
    };
    [map(quad[0]), map(quad[1]), map(quad[2]), map(quad[3])]
}

/// Runs the DB postprocess with [`DbParams::PADDLE`]; see [`boxes_from_bitmap_with`].
#[must_use]
pub fn boxes_from_bitmap<P: ProbValue>(prob: &[P], width: usize, height: usize, dest_w: u32, dest_h: u32) -> Vec<Quad> {
    boxes_from_bitmap_with(prob, width, height, dest_w, dest_h, &DbParams::PADDLE)
}

/// Runs the full DB post-processing pipeline on a probability map.
///
/// `prob` is the row-major `width * height` DB probability map (`f32` or quantized `u8`, see
/// [`ProbValue`]); `dest_w`/`dest_h` are the ORIGINAL image dimensions the returned quads are
/// rescaled to. Quads are `[TL, TR, BR, BL]` in original-image f32 coordinates (integral values
/// under `params.opencv_geometry`), clamped to `[0, dest]`, filtered by the size and score gates of
/// `params` and expanded (unclipped). A map shorter than `width * height` reads as 0 past its end.
#[must_use]
pub fn boxes_from_bitmap_with<P: ProbValue>(prob: &[P], width: usize, height: usize, dest_w: u32, dest_h: u32, params: &DbParams) -> Vec<Quad> {
    let Some(bitmap) = binarize(prob, width, height, params.thresh) else {
        return Vec::new();
    };

    let map_size = [u32_to_f32(u32::try_from(width.max(1)).unwrap_or(1)), u32_to_f32(u32::try_from(height.max(1)).unwrap_or(1))];
    let dest = [u32_to_f32(dest_w), u32_to_f32(dest_h)];

    let contours = find_contours::<i32>(&bitmap);
    let mut quads = Vec::new();
    for contour in &contours {
        if params.contours == ContourSet::Outer && contour.border_type != BorderType::Outer {
            continue;
        }
        if quads.len() >= params.max_candidates {
            break;
        }
        let expanded = if params.opencv_geometry { opencv_candidate(prob, width, height, &contour.points, params) } else { float_candidate(prob, width, height, &contour.points, params) };
        if let Some(expanded) = expanded {
            quads.push(rescale_quad(&expanded, map_size, dest, params.opencv_geometry));
        }
    }

    quads
}

/// Score of one candidate over the region `params` selects.
fn candidate_score<P: ProbValue>(prob: &[P], width: usize, height: usize, contour: &[Point<i32>], quad: &Quad, params: &DbParams) -> f32 {
    match params.score_region {
        ScoreRegion::Quad => box_score(prob, width, height, quad),
        ScoreRegion::Contour => contour_score(prob, width, height, contour),
    }
}

/// One contour through the float path (imageproc min-area quad, rotated-rectangle expansion):
/// the expanded quad in map coordinates, or `None` when a gate drops it.
fn float_candidate<P: ProbValue>(prob: &[P], width: usize, height: usize, contour: &[Point<i32>], params: &DbParams) -> Option<Quad> {
    let quad = mini_box(contour)?;
    if short_side(&quad) < params.min_side_pre || !params.score_gate.keeps(candidate_score(prob, width, height, contour, &quad, params)) {
        return None;
    }
    let perimeter = polygon_perimeter(&quad);
    if perimeter < 1e-6 {
        return None;
    }
    let expanded = unclip_quad(&quad, polygon_area(&quad) * params.unclip_ratio / perimeter)?;
    if params.min_side_post.is_some_and(|min| short_side(&expanded) < min) {
        return None;
    }
    Some(expanded)
}

/// One contour through the `OpenCV` path (`geometry`): float `minAreaRect`, shapely distance on the
/// float box, pyclipper round offset of the truncated box, a second `minAreaRect` over the integer
/// offset path. Returns that second box in map coordinates, or `None` when a gate drops it.
fn opencv_candidate<P: ProbValue>(prob: &[P], width: usize, height: usize, contour: &[Point<i32>], params: &DbParams) -> Option<Quad> {
    let points: Vec<[i64; 2]> = contour.iter().map(|p| [i64::from(p.x), i64::from(p.y)]).collect();
    let rect = geometry::min_area_rect_f(&points)?;
    let quad = to_quad(&rect);
    if short_side(&quad) < params.min_side_pre || !params.score_gate.keeps(candidate_score(prob, width, height, contour, &quad, params)) {
        return None;
    }
    let perimeter = polygon_perimeter(&quad);
    if perimeter < 1e-6 {
        return None;
    }
    let distance = f64::from(polygon_area(&quad)) * f64::from(params.unclip_ratio) / f64::from(perimeter);
    let path = geometry::round_offset(&rect, distance)?;
    let expanded = to_quad(&geometry::min_area_rect_f(&path)?);
    if params.min_side_post.is_some_and(|min| short_side(&expanded) < min) {
        return None;
    }
    Some(expanded)
}

/// A float rectangle as the `f32` quad `cv2.boxPoints` returns.
fn to_quad(rect: &[[f64; 2]; 4]) -> Quad {
    rect.map(|p| p.map(f64_to_f32))
}

/// `f64` geometry to `f32` (`OpenCV`'s `boxPoints` are `float32`).
fn f64_to_f32(value: f64) -> f32 {
    #[expect(clippy::cast_possible_truncation, reason = "box corners are pixel coordinates; float32 is what OpenCV returns")]
    let out = value as f32;
    out
}

/// Axis-aligned `[x1, y1, x2, y2]` bounding box of a quad, clamped to `[0, img_w] x [0, img_h]`.
#[must_use]
pub fn block_from_quad(quad: &Quad, img_w: u32, img_h: u32) -> [f32; 4] {
    let xs = [quad[0][0], quad[1][0], quad[2][0], quad[3][0]];
    let ys = [quad[0][1], quad[1][1], quad[2][1], quad[3][1]];
    let w = u32_to_f32(img_w);
    let h = u32_to_f32(img_h);
    [
        xs.iter().copied().fold(f32::INFINITY, f32::min).clamp(0.0, w),
        ys.iter().copied().fold(f32::INFINITY, f32::min).clamp(0.0, h),
        xs.iter().copied().fold(f32::NEG_INFINITY, f32::max).clamp(0.0, w),
        ys.iter().copied().fold(f32::NEG_INFINITY, f32::max).clamp(0.0, h),
    ]
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

    #[test]
    fn block_from_quad_is_clamped_bbox() {
        // Quad partly outside the image: bbox clamps to [0, dims].
        let quad: Quad = [[-5.0, 10.0], [30.0, 8.0], [32.0, 40.0], [-2.0, 42.0]];
        let block = block_from_quad(&quad, 25, 35);
        let expected = [0.0_f32, 8.0, 25.0, 35.0];
        for (got, want) in block.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-6, "block {block:?} != {expected:?}");
        }
    }

    #[test]
    fn u8_maps_match_the_equivalent_f32_maps() {
        // Quantized u8 maps (stitched detector maps) must behave exactly like f32 maps holding
        // v / 255, including the box-score gate (153 / 255 = 0.6 sits on it; 152 is below).
        let (w, h) = (80_usize, 60_usize);
        let mut quantized = vec![0_u8; w * h];
        for (i, value) in quantized.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            if (6..34).contains(&x) && (5..17).contains(&y) {
                *value = 242;
            } else if (40..72).contains(&x) && (30..41).contains(&y) {
                *value = 153;
            } else if (10..30).contains(&x) && (45..58).contains(&y) {
                *value = 152;
            }
        }
        let float: Vec<f32> = quantized.iter().map(|&v| f32::from(v) / 255.0).collect();
        let from_u8 = boxes_from_bitmap(&quantized, w, h, 160, 120);
        assert_eq!(from_u8, boxes_from_bitmap(&float, w, h, 160, 120));
        assert_eq!(from_u8.len(), 2, "{from_u8:?}");
        assert!(u8::to_prob(76) <= DbParams::PADDLE.thresh && u8::to_prob(77) > DbParams::PADDLE.thresh);
    }

    // Characterization tests (detector Phase 1 refactor): the expected quads were
    // OBSERVED from the pre-refactor code, never derived by hand. They pin today's
    // DB postprocess so its move into `ms-text-detect` can prove it changed nothing.

    /// Sets every pixel of `map` (row stride `width`) for which `inside(x, y)` holds to `p`.
    fn paint(map: &mut [f32], width: usize, p: f32, inside: impl Fn(usize, usize) -> bool) {
        for (idx, value) in map.iter_mut().enumerate() {
            if inside(idx % width, idx / width) {
                *value = p;
            }
        }
    }

    /// characterization: two axis-aligned blobs of different strength, one blob
    /// below the size gate and one below the box-score gate, rescaled to a 2x
    /// destination: exact output quads.
    #[test]
    fn characterization_boxes_from_bitmap_axis_aligned_blobs() {
        let (w, h) = (80_usize, 60_usize);
        let mut map = vec![0.0_f32; w * h];
        paint(&mut map, w, 0.95, |x, y| (6..34).contains(&x) && (5..17).contains(&y));
        paint(&mut map, w, 0.7, |x, y| (40..72).contains(&x) && (30..41).contains(&y));
        paint(&mut map, w, 0.9, |x, y| (10..12).contains(&x) && (50..52).contains(&y));
        paint(&mut map, w, 0.5, |x, y| (10..30).contains(&x) && (25..45).contains(&y));
        let quads = boxes_from_bitmap(&map, w, h, 160, 120);
        // Only the two strong blobs survive (speck: size gate; 0.5 blob: score gate).
        let expected: Vec<Quad> = vec![
            [[0.0, 0.0], [82.0, 0.0], [82.0, 48.0], [0.0, 48.0]],
            [[65.0, 45.0], [157.0, 45.0], [157.0, 95.0], [65.0, 95.0]],
        ];
        assert_eq!(quads, expected, "observed={quads:?}");
    }

    /// characterization: a diagonal band (rotated min-area rect) and a ring (only
    /// the outer contour counts) at destination == map size: exact output quads.
    #[test]
    fn characterization_boxes_from_bitmap_rotated_band_and_ring() {
        let (w, h) = (96_usize, 72_usize);
        let mut map = vec![0.0_f32; w * h];
        // Band of thickness ~8 along the line y = x / 2 + 10, for x in 8..60.
        paint(&mut map, w, 0.9, |x, y| {
            let (xf, yf) = (f32::from(u16::try_from(x).unwrap_or(0)), f32::from(u16::try_from(y).unwrap_or(0)));
            (8.0..60.0).contains(&xf) && (yf - (xf / 2.0 + 10.0)).abs() <= 4.0
        });
        // Ring: 20x20 square at (66, 40) with a 10x10 hole.
        paint(&mut map, w, 0.85, |x, y| {
            (66..86).contains(&x) && (40..60).contains(&y) && !((71..81).contains(&x) && (45..55).contains(&y))
        });
        let quads = boxes_from_bitmap(&map, w, h, 96, 72);
        let expected: Vec<Quad> = vec![
            [[5.0, 0.0], [73.0, 34.0], [62.0, 54.0], [0.0, 20.0]],
            [[57.0, 31.0], [95.0, 31.0], [95.0, 69.0], [57.0, 69.0]],
        ];
        assert_eq!(quads, expected, "observed={quads:?}");
    }
}
