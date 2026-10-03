/*
File: crates/ms-text-detect/src/glyph_mask.rs

Purpose:
Builds a glyph-shaped binary mask (0/255) over the original image from detected
text quads, for the Paddle text detector. Faithful port of
`_extract_text_mask_in_roi` / `_build_glyph_mask` in
`modules/ai_backend/detection/paddle.py`.

Key functions:
- close_3x3       : binary morphological close with a 3x3 cross kernel.
- build_glyph_mask: per-quad ROI text extraction OR-ed into a full-image mask.

Notes:
Otsu comes from `ms_raster::otsu_threshold`, the project's one Otsu; "no split" maps to
`OTSU_NO_SPLIT` (0). Dilate and erode are local: the 3x3 structuring element is the cross
(4-neighborhood + center), matching `cv2.getStructuringElement(MORPH_ELLIPSE, (3,3))`, and
morphology samples out-of-bounds neighbors by clamping (`BORDER_REPLICATE`-like), which the
square, zero-padded `ms_raster::dilate_square` does not reproduce. Per ROI the text pixels are
taken from a saturation-Otsu, then a dark-Otsu, then a light-Otsu pass (same fallback order as
Python), AND-ed with the filled-quad polygon mask. The 3x3-cross erode is shared with the CTD mask
refinement (`erode_3x3`, crate-visible).

Polygon fill: the quad is still filled with imageproc's `draw_polygon_mut` (outline included, like
`cv2.fillPoly`). Moving it onto `ms_raster::fill_polygon_spans` (decision Q4) FAILED the Paddle
golden-fixture parity: the spans rule leaves out the bottom row and part of the outline, which
moves the ROI fill fractions; on `fixtures/paddle/light_on_dark` poly 1 the saturation fill
crosses the 0.85 gate (0.856 in Python), a different branch wins and the mask IoU drops from
0.9995 to 0.384. The switch is held back pending a decision (fill rule vs. imageproc exception);
`paddle_fixture_parity` guards the current state.
*/

use image::{GrayImage, ImageBuffer, Luma, Pixel};
use imageproc::drawing::draw_polygon_mut;
use imageproc::point::Point;
use ms_raster::otsu_threshold;

use crate::db::Quad;
use crate::num::{f32_to_i32_trunc, u32_to_f32};

/// Saturation ROI mean above which the saturation-Otsu pass is attempted.
const MEAN_SAT_MIN: f32 = 20.0;
/// Lower fill fraction for an accepted binarization pass.
const FILL_MIN: f32 = 0.01;
/// Upper fill fraction for an accepted binarization pass.
const FILL_MAX: f32 = 0.85;
/// Threshold used when Otsu finds no split (empty or single-valued plane). 0 is the value the
/// Paddle glyph pass always used; the classic detector's 127 is a different, independent default.
const OTSU_NO_SPLIT: u8 = 0;

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

/// One binary dilate with a 3x3 cross kernel (out-of-bounds neighbors clamped).
fn dilate_3x3(image: &GrayImage) -> GrayImage {
    morph_3x3(image, true)
}

/// One erode (per-pixel minimum) with a 3x3 cross kernel, out-of-bounds neighbors clamped.
///
/// For a cross, clamping an out-of-bounds neighbor yields the centre pixel itself, so this equals
/// `cv2.erode` with its default border (out-of-bounds pixels ignored); the CTD refinement relies on
/// that. Works on any gray values, not only 0/255.
pub(crate) fn erode_3x3(image: &GrayImage) -> GrayImage {
    morph_3x3(image, false)
}

/// Cross structuring element offsets: center + 4-connected neighbors (dx, dy).
/// Matches `cv2.getStructuringElement(MORPH_ELLIPSE, (3, 3))`.
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
        let t = otsu_threshold(sat).unwrap_or(OTSU_NO_SPLIT);
        let sat_bin = binarize_plane(sat, w, h, |v| v > t);
        let anded = and_mask(&sat_bin, poly);
        let fill = fill_fraction(&anded, total);
        if fill > FILL_MIN && fill < FILL_MAX {
            return anded;
        }
    }

    // Pass 2: dark text (BINARY_INV: pixel <= Otsu).
    let t_gray = otsu_threshold(gray).unwrap_or(OTSU_NO_SPLIT);
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
///
/// `source` is any 8-bit colour buffer (`RgbaImage` for the native OCR path, `RgbImage` for the
/// detection pipeline); only its RGB channels are read (`Pixel::to_rgb`), alpha is ignored.
#[must_use]
pub fn build_glyph_mask<P: Pixel<Subpixel = u8>>(source: &ImageBuffer<P, Vec<u8>>, quads: &[Quad]) -> GrayImage {
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
                    .to_rgb()
                    .0;
                sat.push(saturation(px[0], px[1], px[2]));
                gray.push(luma(px[0], px[1], px[2]));
            }
        }

        // Filled-quad polygon mask in ROI-local coordinates.
        // imageproc fill on purpose: `ms_raster::fill_polygon_spans` failed the Paddle fixture
        // parity here (file header), so the switch is pending a decision.
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
    use image::RgbaImage;

    #[test]
    fn otsu_separates_two_tones() {
        // A bimodal plane: half at 20, half at 200. Threshold must sit between them.
        let mut values = vec![20u8; 50];
        values.extend(std::iter::repeat_n(200u8, 50));
        let t = otsu_threshold(&values).unwrap_or(OTSU_NO_SPLIT);
        assert!((20..200).contains(&t), "otsu threshold out of expected band: {t}");
    }

    #[test]
    fn otsu_empty_is_zero() {
        assert_eq!(otsu_threshold(&[]).unwrap_or(OTSU_NO_SPLIT), 0);
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

    // Characterization tests (detector Phase 1 refactor): the expected values were
    // OBSERVED from the pre-refactor code, never derived by hand. They pin today's
    // Otsu and glyph mask so the move into `ms-raster` / `ms-text-detect` can prove
    // it changed nothing.

    /// FNV-1a 64 over `bytes`: a compact, dependency-free fingerprint for pinned buffers.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Deterministic 64-bit LCG (Knuth MMIX constants); returns the high 31 bits.
    fn lcg_next(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // `>> 33` leaves 31 significant bits, so the conversion cannot fail.
        u32::try_from(*state >> 33).unwrap_or(0)
    }

    /// characterization: the glyph pass's Otsu (`ms_raster::otsu_threshold` with the
    /// `OTSU_NO_SPLIT` default) on the same fixed vectors as the classic detector
    /// (`ms-tab-translation` tests). The two differ whenever no split exists — "empty"
    /// AND `single_value` — because each keeps its own default (0 here, 127 there);
    /// every other vector agrees. Values pinned from the pre-move glyph-mask copy.
    #[test]
    fn characterization_otsu_fixed_histograms() {
        let mut state = 0x0750_u64;
        let random: Vec<u8> = (0..1000).map(|_| u8::try_from(lcg_next(&mut state) % 256).unwrap_or(0)).collect();
        let mut bimodal = vec![20u8; 50];
        bimodal.extend(std::iter::repeat_n(200u8, 50));
        let mut three_level = vec![10u8; 30];
        three_level.extend(std::iter::repeat_n(128u8, 40));
        three_level.extend(std::iter::repeat_n(240u8, 30));
        let vectors: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("single_value", vec![77u8; 10]),
            ("two_adjacent", vec![100, 101, 100, 101]),
            ("bimodal", bimodal),
            ("three_level", three_level),
            ("extremes", vec![0, 255]),
            ("random_1000", random),
        ];
        let observed: Vec<(&str, u8)> = vectors.iter().map(|(name, values)| (*name, otsu_threshold(values).unwrap_or(OTSU_NO_SPLIT))).collect();
        assert_eq!(
            observed,
            vec![
                ("empty", 0),
                ("single_value", 0),
                ("two_adjacent", 100),
                ("bimodal", 20),
                ("three_level", 10),
                ("extremes", 0),
                ("random_1000", 129),
            ],
            "observed={observed:?}"
        );
    }

    /// characterization: `build_glyph_mask` on a noisy synthetic page with dark
    /// text, light-on-dark text, saturated (red) text, a rotated quad, a quad
    /// partly outside the image and a degenerate quad: mask fingerprint.
    #[test]
    fn characterization_build_glyph_mask_fingerprint() {
        let mut state = 0x0911_7a5c_u64;
        let (w, h) = (120_u32, 90_u32);
        let mut img = RgbaImage::from_fn(w, h, |_, _| {
            let v = 215 + u8::try_from(lcg_next(&mut state) % 40).unwrap_or(0);
            image::Rgba([v, v, v, 255])
        });
        let mut fill = |img: &mut RgbaImage, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>, rgb: [u8; 3]| {
            for y in ys {
                for x in xs.clone() {
                    let jitter = u8::try_from(lcg_next(&mut state) % 12).unwrap_or(0);
                    img.put_pixel(x, y, image::Rgba([rgb[0].saturating_add(jitter), rgb[1].saturating_add(jitter), rgb[2].saturating_add(jitter), 255]));
                }
            }
        };
        // Dark "glyph strokes" in a light region.
        for gx in [10_u32, 18, 26, 34] {
            fill(&mut img, gx..gx + 2, 10..22, [15, 15, 15]);
            fill(&mut img, gx..gx + 6, 15..17, [15, 15, 15]);
        }
        // Light text on a dark plate.
        fill(&mut img, 60..110, 8..28, [30, 30, 30]);
        for gx in [64_u32, 74, 84, 94] {
            fill(&mut img, gx..gx + 3, 12..24, [235, 235, 235]);
        }
        // Saturated red text on the light background.
        for gx in [12_u32, 22, 32] {
            fill(&mut img, gx..gx + 3, 45..60, [210, 20, 25]);
        }
        // Dark bar under a rotated quad, and one near the right/bottom border.
        fill(&mut img, 60..100, 50..56, [20, 20, 20]);
        fill(&mut img, 108..120, 80..90, [10, 10, 10]);

        let quads: Vec<Quad> = vec![
            [[8.0, 8.0], [42.0, 8.0], [42.0, 24.0], [8.0, 24.0]],
            [[60.0, 8.0], [110.0, 8.0], [110.0, 28.0], [60.0, 28.0]],
            [[10.0, 43.0], [38.0, 43.0], [38.0, 62.0], [10.0, 62.0]],
            [[58.0, 52.0], [98.0, 44.0], [101.5, 56.0], [61.5, 64.0]],
            [[104.0, 76.0], [130.0, 76.0], [130.0, 95.0], [104.0, 95.0]],
            [[5.0, 5.0], [5.0, 5.0], [5.0, 5.0], [5.0, 5.0]],
        ];
        let mask = build_glyph_mask(&img, &quads);
        assert_eq!(mask.dimensions(), (120, 90));
        let set = mask.pixels().filter(|p| p.0[0] == 255).count();
        let other = mask.pixels().filter(|p| p.0[0] != 0 && p.0[0] != 255).count();
        let observed = (set, other, fnv1a64(mask.as_raw()));
        // (pixels at 255, pixels neither 0 nor 255, FNV of the mask bytes)
        assert_eq!(observed, (1570, 0, 789_239_080_924_908_449), "observed={observed:?}");
    }
    /// Paddle golden-fixture parity (`fixtures/paddle/`) of the DB postprocess (span-filled score
    /// quads) + glyph mask (imageproc-filled gate, see the file header), at the plan tolerances:
    /// equal block count, every block `IoU` >= 0.9 with edges within 2 px, mask `IoU` >= 0.97 and `XOR` <= 3 % of
    /// the union. Achieved values print with `--nocapture`.
    #[test]
    fn paddle_fixture_parity() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/paddle");
        for name in ["dark_on_light", "saturated_text", "light_on_dark"] {
            let dir = root.join(name);
            let json: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("case.json")).unwrap_or_else(|err| panic!("{name}: {err}"))).unwrap_or_else(|err| panic!("{name}: {err}"));
            let page = image::open(dir.join("input.png")).unwrap_or_else(|err| panic!("{name}: {err}")).to_rgb8();
            let prob = image::open(dir.join("prob.png")).unwrap_or_else(|err| panic!("{name}: {err}")).to_luma8();
            let expected_mask = image::open(dir.join("expected_mask.png")).unwrap_or_else(|err| panic!("{name}: {err}")).to_luma8();
            let (w, h) = page.dimensions();
            let quads = crate::db::boxes_from_bitmap(prob.as_raw(), crate::num::idx(prob.width()), crate::num::idx(prob.height()), w, h);
            let got: Vec<[f32; 4]> = quads.iter().map(|q| crate::db::block_from_quad(q, w, h)).collect();
            let coord = |v: &serde_json::Value| u32_to_f32(u32::try_from(v.as_u64().unwrap_or_else(|| panic!("{name}: {v}"))).unwrap_or(u32::MAX));
            let want: Vec<[f32; 4]> = json["expected"]["blocks"].as_array().unwrap_or_else(|| panic!("{name}: blocks")).iter().map(|b| [coord(&b["x1"]), coord(&b["y1"]), coord(&b["x2"]), coord(&b["y2"])]).collect();
            assert_eq!(got.len(), want.len(), "{name}: {got:?} vs {want:?}");
            let iou = |a: [f32; 4], b: [f32; 4]| {
                let inter = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
                inter / ((a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter)
            };
            let (mut min_iou, mut max_edge) = (1.0_f32, 0.0_f32);
            for wb in &want {
                let best = got.iter().copied().max_by(|a, b| iou(*a, *wb).total_cmp(&iou(*b, *wb))).unwrap_or([0.0; 4]);
                min_iou = min_iou.min(iou(best, *wb));
                max_edge = best.iter().zip(wb).map(|(g, e)| (g - e).abs()).fold(max_edge, f32::max);
            }
            let mask = build_glyph_mask(&page, &quads);
            let (mut inter, mut union, mut xor) = (0_u32, 0_u32, 0_u32);
            for (g, e) in mask.as_raw().iter().zip(expected_mask.as_raw()) {
                let (g, e) = (*g != 0, *e != 0);
                inter += u32::from(g && e);
                union += u32::from(g || e);
                xor += u32::from(g != e);
            }
            let mask_iou = if union == 0 { 1.0 } else { u32_to_f32(inter) / u32_to_f32(union) };
            let xor_ratio = if union == 0 { 0.0 } else { u32_to_f32(xor) / u32_to_f32(union) };
            eprintln!("paddle parity {name}: blocks {} min IoU {min_iou:.4} max edge {max_edge} px; mask IoU {mask_iou:.5} xor/union {xor_ratio:.5} ({xor} px)", got.len());
            assert!(min_iou >= 0.9 && max_edge <= 2.0, "{name}: blocks {got:?} vs {want:?}");
            assert!(mask_iou >= 0.97 && xor_ratio <= 0.03, "{name}: mask IoU {mask_iou} xor/union {xor_ratio}");
        }
    }
}
