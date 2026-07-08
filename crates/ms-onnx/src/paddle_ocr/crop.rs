/*
File: crates/ms-onnx/src/paddle_ocr/crop.rs

Purpose:
Perspective-cropping and reading-order sorting of detected text quads. Faithful
port of `get_rotate_crop_image` and `sort_quad_indices` in
`modules/ai_backend/paddle_onnx_runtime.py`.

Key functions:
- crop_dimensions   : axis-aligned crop (width, height) for a quad.
- rotate_crop       : perspective-warp a quad from the source image into a crop.
- sort_quad_indices : reading-order permutation of detected quads.

Notes:
Parity risk: imageproc's bicubic sampler is not bit-identical to cv2 INTER_CUBIC,
so crop pixels differ slightly from the Python path; this feeds a robust
recognition CNN, so tests use tolerances rather than exact pixel equality. The
`crop_h/crop_w >= 1.5` vertical-text rotation uses `imageops::rotate270`, which
rotates 270 degrees clockwise == 90 degrees counter-clockwise == NumPy's
`np.rot90(k=1)`.
*/

use image::{RgbaImage, imageops};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};

use super::{Quad, nonneg_f32_to_u32};

/// Aspect ratio at/above which a crop is treated as vertical text and rotated.
const VERTICAL_ROTATE_RATIO: f32 = 1.5;

/// Euclidean distance between two 2D points.
fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Computes the axis-aligned crop `(width, height)` for a quad (`p0..p3`).
///
/// `width = max(|p0-p1|, |p2-p3|)`, `height = max(|p0-p3|, |p1-p2|)`, truncated to
/// integers (matching Python's `int(...)`). Returns `(0, 0)` for a degenerate quad.
#[must_use]
pub fn crop_dimensions(quad: &Quad) -> (u32, u32) {
    let width = distance(quad[0], quad[1]).max(distance(quad[2], quad[3]));
    let height = distance(quad[0], quad[3]).max(distance(quad[1], quad[2]));
    (
        nonneg_f32_to_u32(width.trunc()),
        nonneg_f32_to_u32(height.trunc()),
    )
}

/// Perspective-warps `quad` from `source` into an axis-aligned crop.
///
/// Returns `None` when the crop would be smaller than 1x1 or the projection is
/// degenerate. When the crop is at least [`VERTICAL_ROTATE_RATIO`] times taller
/// than wide, it is rotated with `rotate270` (vertical-text handling).
#[must_use]
pub fn rotate_crop(source: &RgbaImage, quad: &Quad) -> Option<RgbaImage> {
    let (crop_w, crop_h) = crop_dimensions(quad);
    if crop_w < 1 || crop_h < 1 {
        return None;
    }

    let from = [
        (quad[0][0], quad[0][1]),
        (quad[1][0], quad[1][1]),
        (quad[2][0], quad[2][1]),
        (quad[3][0], quad[3][1]),
    ];
    let (w_f, h_f) = (
        crate::paddle_ocr::u32_to_f32(crop_w),
        crate::paddle_ocr::u32_to_f32(crop_h),
    );
    let to = [(0.0, 0.0), (w_f, 0.0), (w_f, h_f), (0.0, h_f)];

    // Projection maps source-image locations -> crop locations; warp_into inverts
    // it internally to sample each crop pixel from the source (cf. cv2
    // getPerspectiveTransform(src, dst) + warpPerspective).
    let projection = Projection::from_control_points(from, to)?;

    let mut crop = RgbaImage::new(crop_w, crop_h);
    warp_into(
        source,
        projection,
        Interpolation::Bicubic,
        Border::Replicate,
        &mut crop,
    );

    // Vertical text: rotate so the recognizer reads it left-to-right. rotate270 ==
    // np.rot90(k=1) (see file header).
    if h_f / w_f >= VERTICAL_ROTATE_RATIO {
        crop = imageops::rotate270(&crop);
    }
    Some(crop)
}

/// Per-quad reading-order metrics: `(vertical_midpoint, left_x, height)`.
fn quad_metrics(quad: &Quad) -> (f32, f32, f32) {
    let ys = [quad[0][1], quad[1][1], quad[2][1], quad[3][1]];
    let xs = [quad[0][0], quad[1][0], quad[2][0], quad[3][0]];
    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
    (f32::midpoint(min_y, max_y), min_x, max_y - min_y)
}

/// Returns the reading-order permutation of `quads` (top-to-bottom, left-to-right).
///
/// Sorts by vertical midpoint then left-x, then applies Python's same-line
/// insertion-swap pass: adjacent quads within a `max(h_prev, h_curr, 10) * 0.5`
/// vertical tolerance are reordered so the more-left one comes first. Matches
/// `sort_quad_indices`; returns indices into `quads`.
#[must_use]
pub fn sort_quad_indices(quads: &[Quad]) -> Vec<usize> {
    let metrics: Vec<(f32, f32, f32)> = quads.iter().map(quad_metrics).collect();

    let mut order: Vec<usize> = (0..quads.len()).collect();
    // Primary stable sort by (vertical midpoint, left-x).
    order.sort_by(|&a, &b| {
        let (ya, xa, _) = metrics[a];
        let (yb, xb, _) = metrics[b];
        ya.partial_cmp(&yb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal))
    });

    // Same-line refinement: swap left-ward neighbors that share a line (Python's
    // insertion-style pass with a height-based tolerance).
    for idx in 0..order.len() {
        let mut pos = idx;
        while pos > 0 {
            let (prev_y, prev_x, prev_h) = metrics[order[pos - 1]];
            let (curr_y, curr_x, curr_h) = metrics[order[pos]];
            let tolerance = prev_h.max(curr_h).max(10.0) * 0.5;
            if (curr_y - prev_y).abs() <= tolerance && curr_x < prev_x {
                order.swap(pos - 1, pos);
                pos -= 1;
            } else {
                break;
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_dimensions_axis_aligned_rect() {
        // Axis-aligned 40x10 quad: p0=TL, p1=TR, p2=BR, p3=BL.
        let quad: Quad = [[10.0, 5.0], [50.0, 5.0], [50.0, 15.0], [10.0, 15.0]];
        assert_eq!(crop_dimensions(&quad), (40, 10));
    }

    #[test]
    fn crop_dimensions_rotation_threshold() {
        // A tall quad (10 wide, 40 tall) crosses the 1.5 vertical ratio.
        let quad: Quad = [[0.0, 0.0], [10.0, 0.0], [10.0, 40.0], [0.0, 40.0]];
        let (w, h) = crop_dimensions(&quad);
        assert_eq!((w, h), (10, 40));
        let ratio = f64::from(h) / f64::from(w);
        assert!(ratio >= 1.5);
    }

    #[test]
    fn sort_orders_top_to_bottom_then_left_to_right() {
        // Three quads: two on the top line (right then left), one below.
        let top_right: Quad = [[100.0, 0.0], [140.0, 0.0], [140.0, 10.0], [100.0, 10.0]];
        let top_left: Quad = [[0.0, 0.0], [40.0, 0.0], [40.0, 10.0], [0.0, 10.0]];
        let bottom: Quad = [[0.0, 100.0], [40.0, 100.0], [40.0, 110.0], [0.0, 110.0]];
        let order = sort_quad_indices(&[top_right, top_left, bottom]);
        // Expected reading order: top_left (idx 1), top_right (idx 0), bottom (idx 2).
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn rotate_crop_extracts_axis_aligned_region() {
        // Build a 20x20 image; a quad selecting the left 10x10 block should crop it.
        let mut img = RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 0, 255]));
        for y in 0..20 {
            for x in 0..10 {
                img.put_pixel(x, y, image::Rgba([200, 100, 50, 255]));
            }
        }
        let quad: Quad = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let crop = rotate_crop(&img, &quad).expect("crop must succeed");
        assert_eq!(crop.dimensions(), (10, 10));
        // Center pixel of the crop should be the colored block (tolerant of sampling).
        let center = crop.get_pixel(5, 5).0;
        assert!(center[0] > 100, "expected colored region, got {center:?}");
    }
}
