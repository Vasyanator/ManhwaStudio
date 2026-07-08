/*
File: crates/ms-onnx/src/paddle_ocr/glyph_mask.rs

Purpose:
Builds a glyph-shaped binary mask (0/255) over the original image from detected
text quads, for the `textdetector.paddle` op. Faithful port of
`_extract_text_mask_in_roi` / `_build_glyph_mask` in
`modules/ai_backend/paddle_text_detector_service.py`.

Key functions:
- otsu_threshold  : histogram-based Otsu threshold (self-contained).
- close_3x3       : binary morphological close with a 3x3 cross kernel.
- build_glyph_mask: per-quad ROI text extraction OR-ed into a full-image mask.

Notes:
Otsu, dilate, and erode are implemented locally so the crate stays self-contained
(no OpenCV). The 3x3 structuring element is the cross (4-neighborhood + center),
matching cv2.getStructuringElement(MORPH_ELLIPSE, (3,3)); morphology samples
out-of-bounds neighbors by clamping (BORDER_REPLICATE-like). Per ROI the text
pixels are taken from a saturation-Otsu, then a dark-Otsu, then a light-Otsu pass
(same fallback order as Python), AND-ed with the filled-quad polygon mask.
*/

use image::{GrayImage, Luma, RgbaImage};
use imageproc::drawing::draw_polygon_mut;
use imageproc::point::Point;

use super::{Quad, f32_to_i32_trunc, u32_to_f32};

/// Saturation ROI mean above which the saturation-Otsu pass is attempted.
const MEAN_SAT_MIN: f32 = 20.0;
/// Lower fill fraction for an accepted binarization pass.
const FILL_MIN: f32 = 0.01;
/// Upper fill fraction for an accepted binarization pass.
const FILL_MAX: f32 = 0.85;

/// sRGB luma (`0.299 R + 0.587 G + 0.114 B`, rounded), matching cv2 BGR2GRAY.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let value = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    let rounded = value.round().clamp(0.0, 255.0);
    u8::try_from(f32_to_i32_trunc(rounded)).unwrap_or(u8::MAX)
}

/// HSV saturation channel (`(max-min)/max * 255`, rounded), matching cv2 BGR2HSV.
fn saturation(r: u8, g: u8, b: u8) -> u8 {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max == 0 {
        return 0;
    }
    let s = f32::from(max - min) / f32::from(max) * 255.0;
    u8::try_from(f32_to_i32_trunc(s.round().clamp(0.0, 255.0))).unwrap_or(u8::MAX)
}

/// Lossless-in-practice `u64` -> `f64` for pixel-histogram counts (< 2^52).
fn count_to_f64(count: u64) -> f64 {
    // Histogram counts stay far below f64's 2^52 exact-integer limit.
    #[allow(clippy::cast_precision_loss)]
    let out = count as f64;
    out
}

/// Computes the Otsu threshold `t` from a 256-bin histogram of `values`.
///
/// Returns the intensity that maximizes between-class variance; classification is
/// `value > t` (foreground). An empty input returns 0.
#[must_use]
pub fn otsu_threshold(values: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in values {
        hist[usize::from(v)] += 1;
    }
    let total = count_to_f64(u64::try_from(values.len()).unwrap_or(u64::MAX));
    if total == 0.0 {
        return 0;
    }

    // Sum of intensity*count over all bins (for the running class mean).
    let mut sum_total = 0.0_f64;
    for (intensity, &count) in hist.iter().enumerate() {
        let intensity = u8::try_from(intensity).unwrap_or(u8::MAX);
        sum_total += f64::from(intensity) * count_to_f64(count);
    }

    let mut bg_weight = 0.0_f64;
    let mut bg_sum = 0.0_f64;
    let mut best_variance = -1.0_f64;
    let mut best_t = 0u8;

    for (intensity, &count) in hist.iter().enumerate() {
        let intensity = u8::try_from(intensity).unwrap_or(u8::MAX);
        bg_weight += count_to_f64(count);
        if bg_weight == 0.0 {
            continue;
        }
        let fg_weight = total - bg_weight;
        if fg_weight == 0.0 {
            break;
        }
        bg_sum += f64::from(intensity) * count_to_f64(count);
        let bg_mean = bg_sum / bg_weight;
        let fg_mean = (sum_total - bg_sum) / fg_weight;
        let diff = bg_mean - fg_mean;
        let variance = bg_weight * fg_weight * diff * diff;
        if variance > best_variance {
            best_variance = variance;
            best_t = intensity;
        }
    }
    best_t
}

/// One binary dilate with a 3x3 cross kernel (out-of-bounds neighbors clamped).
fn dilate_3x3(image: &GrayImage) -> GrayImage {
    morph_3x3(image, true)
}

/// One binary erode with a 3x3 cross kernel (out-of-bounds neighbors clamped).
fn erode_3x3(image: &GrayImage) -> GrayImage {
    morph_3x3(image, false)
}

/// Cross structuring element offsets: center + 4-connected neighbors (dx, dy).
/// Matches cv2.getStructuringElement(MORPH_ELLIPSE, (3, 3)).
const CROSS_OFFSETS: [(i32, i32); 5] = [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)];

/// Shared 3x3-cross morphology: `dilate` picks the max, erode picks the min.
fn morph_3x3(image: &GrayImage, dilate: bool) -> GrayImage {
    let (w, h) = image.dimensions();
    let mut out = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let mut acc: u8 = if dilate { 0 } else { 255 };
            for (dx, dy) in CROSS_OFFSETS {
                // Clamp to the image edge (BORDER_REPLICATE-like sampling).
                let nx = (i64::from(x) + i64::from(dx)).clamp(0, i64::from(w) - 1);
                let ny = (i64::from(y) + i64::from(dy)).clamp(0, i64::from(h) - 1);
                let sample = image
                    .get_pixel(u32::try_from(nx).unwrap_or(0), u32::try_from(ny).unwrap_or(0))
                    .0[0];
                acc = if dilate { acc.max(sample) } else { acc.min(sample) };
            }
            out.put_pixel(x, y, Luma([acc]));
        }
    }
    out
}

/// Binary morphological close (dilate then erode) with a 3x3 cross kernel.
#[must_use]
pub fn close_3x3(image: &GrayImage) -> GrayImage {
    erode_3x3(&dilate_3x3(image))
}

/// Bitwise-AND of two equal-size 0/255 masks into a new mask.
fn and_mask(lhs: &GrayImage, rhs: &GrayImage) -> GrayImage {
    let (w, h) = lhs.dimensions();
    let mut out = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let set = lhs.get_pixel(x, y).0[0] != 0 && rhs.get_pixel(x, y).0[0] != 0;
            out.put_pixel(x, y, Luma([if set { 255 } else { 0 }]));
        }
    }
    out
}

/// Fraction of set pixels in a 0/255 mask over `total` pixels.
fn fill_fraction(mask: &GrayImage, total: u32) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let count = mask.pixels().filter(|p| p.0[0] != 0).count();
    u32_to_f32(u32::try_from(count).unwrap_or(u32::MAX)) / u32_to_f32(total)
}

/// Extracts the text-pixel mask inside one ROI, gated by the polygon mask.
///
/// `sat`/`gray` are the ROI's per-pixel saturation and luma planes (row-major);
/// `poly` is the filled-quad mask. Tries saturation-Otsu, then dark-Otsu, then
/// light-Otsu (as fallback), returning the first pass whose fill fraction is in
/// range, AND-ed with `poly`. Mirrors `_extract_text_mask_in_roi`.
fn extract_text_mask(sat: &[u8], gray: &[u8], poly: &GrayImage) -> GrayImage {
    let (w, h) = poly.dimensions();
    let total = w * h;

    // Mean saturation over the polygon region only.
    let mut sat_sum = 0.0_f32;
    let mut poly_count = 0_u32;
    for (i, pixel) in poly.pixels().enumerate() {
        if pixel.0[0] != 0 {
            sat_sum += f32::from(sat.get(i).copied().unwrap_or(0));
            poly_count += 1;
        }
    }
    let mean_sat = if poly_count == 0 {
        0.0
    } else {
        sat_sum / u32_to_f32(poly_count)
    };

    // Pass 1: saturation Otsu (colored text on a low-saturation bubble).
    if mean_sat > MEAN_SAT_MIN {
        let t = otsu_threshold(sat);
        let sat_bin = binarize_plane(sat, w, h, |v| v > t);
        let anded = and_mask(&sat_bin, poly);
        let fill = fill_fraction(&anded, total);
        if fill > FILL_MIN && fill < FILL_MAX {
            return anded;
        }
    }

    // Pass 2: dark text (BINARY_INV: pixel <= Otsu).
    let t_gray = otsu_threshold(gray);
    let dark_bin = binarize_plane(gray, w, h, |v| v <= t_gray);
    let dark = and_mask(&dark_bin, poly);
    let fill_dark = fill_fraction(&dark, total);
    if fill_dark > FILL_MIN && fill_dark < FILL_MAX {
        return dark;
    }

    // Pass 3 (fallback): light text (BINARY: pixel > Otsu).
    let light_bin = binarize_plane(gray, w, h, |v| v > t_gray);
    and_mask(&light_bin, poly)
}

/// Binarizes a row-major plane into a 0/255 mask via `keep`.
fn binarize_plane(plane: &[u8], w: u32, h: u32, keep: impl Fn(u8) -> bool) -> GrayImage {
    let mut out = GrayImage::new(w, h);
    for (i, pixel) in out.pixels_mut().enumerate() {
        let v = plane.get(i).copied().unwrap_or(0);
        *pixel = Luma([if keep(v) { 255 } else { 0 }]);
    }
    out
}

/// Builds a full-image glyph mask (0/255) by unioning per-quad text masks.
///
/// For each quad: compute its clamped bounding box, extract the ROI's text pixels
/// (saturation/dark/light Otsu passes gated by the filled polygon), morphologically
/// close the result, and OR it into the full-image mask. Returns a [`GrayImage`] at
/// the source image size. Mirrors `_build_glyph_mask`.
#[must_use]
pub fn build_glyph_mask(source: &RgbaImage, quads: &[Quad]) -> GrayImage {
    let (img_w, img_h) = source.dimensions();
    let mut mask = GrayImage::new(img_w, img_h);
    if img_w == 0 || img_h == 0 {
        return mask;
    }
    let img_cols = i32::try_from(img_w).unwrap_or(i32::MAX);
    let img_rows = i32::try_from(img_h).unwrap_or(i32::MAX);

    for quad in quads {
        let xs = quad.iter().map(|p| f32_to_i32_trunc(p[0]));
        let ys = quad.iter().map(|p| f32_to_i32_trunc(p[1]));
        let min_x = xs.clone().min().unwrap_or(0);
        let max_x = xs.max().unwrap_or(0);
        let min_y = ys.clone().min().unwrap_or(0);
        let max_y = ys.max().unwrap_or(0);

        // cv2.boundingRect covers [min, max]; clamp to the image.
        let x1 = min_x.max(0);
        let y1 = min_y.max(0);
        let x2 = (max_x + 1).min(img_cols);
        let y2 = (max_y + 1).min(img_rows);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        let roi_w = u32::try_from(x2 - x1).unwrap_or(0);
        let roi_h = u32::try_from(y2 - y1).unwrap_or(0);
        if roi_w == 0 || roi_h == 0 {
            continue;
        }

        // ROI saturation/luma planes.
        let mut sat = Vec::with_capacity((roi_w * roi_h) as usize);
        let mut gray = Vec::with_capacity((roi_w * roi_h) as usize);
        for ly in 0..roi_h {
            for lx in 0..roi_w {
                let px = source
                    .get_pixel(
                        u32::try_from(x1).unwrap_or(0) + lx,
                        u32::try_from(y1).unwrap_or(0) + ly,
                    )
                    .0;
                sat.push(saturation(px[0], px[1], px[2]));
                gray.push(luma(px[0], px[1], px[2]));
            }
        }

        // Filled-quad polygon mask in ROI-local coordinates.
        let mut poly = GrayImage::new(roi_w, roi_h);
        let poly_pts: Vec<Point<i32>> = quad
            .iter()
            .map(|p| Point::new(f32_to_i32_trunc(p[0]) - x1, f32_to_i32_trunc(p[1]) - y1))
            .collect();
        if poly_pts.first() != poly_pts.last() {
            draw_polygon_mut(&mut poly, &poly_pts, Luma([255]));
        }

        let text = extract_text_mask(&sat, &gray, &poly);
        let closed = close_3x3(&text);

        // OR the ROI text mask into the full mask.
        for ly in 0..roi_h {
            for lx in 0..roi_w {
                if closed.get_pixel(lx, ly).0[0] != 0 {
                    mask.put_pixel(
                        u32::try_from(x1).unwrap_or(0) + lx,
                        u32::try_from(y1).unwrap_or(0) + ly,
                        Luma([255]),
                    );
                }
            }
        }
    }

    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otsu_separates_two_tones() {
        // A bimodal plane: half at 20, half at 200. Threshold must sit between them.
        let mut values = vec![20u8; 50];
        values.extend(std::iter::repeat_n(200u8, 50));
        let t = otsu_threshold(&values);
        assert!((20..200).contains(&t), "otsu threshold out of expected band: {t}");
    }

    #[test]
    fn otsu_empty_is_zero() {
        assert_eq!(otsu_threshold(&[]), 0);
    }

    #[test]
    fn close_3x3_fills_single_pixel_hole() {
        // 5x5 solid block with a single 0 hole in the center; close should fill it.
        let mut img = GrayImage::from_pixel(5, 5, Luma([255]));
        img.put_pixel(2, 2, Luma([0]));
        let closed = close_3x3(&img);
        assert_eq!(closed.get_pixel(2, 2).0[0], 255, "hole must be closed");
    }

    #[test]
    fn build_glyph_mask_marks_dark_text_region() {
        // White 20x20 image with a dark 6x2 bar; a quad over it should mark pixels.
        let mut img = RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        for y in 9..11 {
            for x in 7..13 {
                img.put_pixel(x, y, image::Rgba([10, 10, 10, 255]));
            }
        }
        let quad: Quad = [[6.0, 8.0], [14.0, 8.0], [14.0, 12.0], [6.0, 12.0]];
        let mask = build_glyph_mask(&img, &[quad]);
        assert_eq!(mask.dimensions(), (20, 20));
        // At least one dark-text pixel must be set in the mask.
        let set = mask.pixels().filter(|p| p.0[0] != 0).count();
        assert!(set > 0, "glyph mask should mark the dark text");
    }
}
