/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/classic.rs
The local "classic" text detector: no model, no backend. Builds both block rects and a real
binary mask from accepted connected components, so classic mode feeds the same editable mask
pipeline as the AI modes.

Key functions:
- `detect_page_classic()`: decodes a page file to grayscale and runs the pure pipeline.
- `detect_classic_from_gray()`: the pure pipeline (downscale to 1600, Otsu, dilation,
  components, mask promotion to source size).
- `classic_otsu_threshold()` / `dilate_binary()`: thin adapters over `ms_raster` that keep the
  classic defaults (127 when no split) and output format (0/1).

Notes:
Block order and cap come from `ms_text_detect::blocks::finalize_blocks`, mask normalization and
the pixel guard from `ms_text_detect::mask`.
*/

use super::{TextDetectorPageResult, TextDetectorRect};
use image::GrayImage;
use image::imageops::FilterType;
use ms_raster::RasterError;
use ms_text_detect::blocks::finalize_blocks;
use ms_text_detect::mask;
use std::path::Path;

/// Longest working side of the classic detector; larger pages are downscaled (Triangle).
pub(super) const MAX_DETECTOR_DIM: u32 = 1600;

/// The classic threshold when the histogram has no Otsu split (empty or single-valued).
const CLASSIC_OTSU_NO_SPLIT: u8 = 127;

/// Decodes `path` to grayscale and runs [`detect_classic_from_gray`]. Worker-thread only (I/O).
///
/// # Errors
/// A localized message when the file cannot be decoded or decodes to an empty image.
pub(super) fn detect_page_classic(path: &Path) -> Result<TextDetectorPageResult, String> {
    let img = image::open(path).map_err(|err| {
        tf!("translation.text_detector.open_image_error", path = path.display(), err = err)
    })?;
    let gray = img.to_luma8();
    if gray.width() == 0 || gray.height() == 0 {
        return Err(tf!("translation.text_detector.empty_image_error", path = path.display()));
    }
    Ok(detect_classic_from_gray(gray))
}

/// The classic detection pipeline on an in-memory grayscale page.
///
/// Pages whose longest side exceeds [`MAX_DETECTOR_DIM`] are processed downscaled; block rects
/// are scaled back to source pixels and the mask is promoted (nearest) to source size. Blocks
/// come back in reading order, capped by `finalize_blocks`. A zero-area page yields no blocks
/// and an empty mask.
pub(super) fn detect_classic_from_gray(gray: GrayImage) -> TextDetectorPageResult {
    let source_w = gray.width();
    let source_h = gray.height();

    let (proc, scale_x, scale_y) = if source_w.max(source_h) > MAX_DETECTOR_DIM {
        let scale = MAX_DETECTOR_DIM as f32 / source_w.max(source_h) as f32;
        let dst_w = ((source_w as f32 * scale).round() as u32).max(1);
        let dst_h = ((source_h as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&gray, dst_w, dst_h, FilterType::Triangle);
        let sx = source_w as f32 / dst_w as f32;
        let sy = source_h as f32 / dst_h as f32;
        (resized, sx, sy)
    } else {
        (gray, 1.0, 1.0)
    };

    let proc_w = proc.width() as usize;
    let proc_h = proc.height() as usize;
    let threshold = classic_otsu_threshold(proc.as_raw()).clamp(45, 210);
    let fg = proc
        .as_raw()
        .iter()
        .map(|&px| u8::from(px < threshold))
        .collect::<Vec<_>>();

    // Небольшая дилатация склеивает символы в блоки без тяжёлых зависимостей.
    // Invariant: `fg` has one byte per pixel of `proc` (an `image` buffer always holds exactly
    // width * height luma bytes), so the only error of `dilate_square` cannot occur.
    let fg = dilate_binary(&fg, proc_w, proc_h, 2, 1)
        .expect("classic foreground holds exactly proc_w * proc_h bytes");
    let (blocks, proc_mask_alpha) =
        extract_components_as_rects_and_mask(&fg, proc_w, proc_h, scale_x, scale_y);
    let (mask_size, mask_alpha) = promote_classic_mask_to_source_size(
        proc_mask_alpha,
        [proc.width(), proc.height()],
        [source_w, source_h],
    );

    TextDetectorPageResult {
        source_size: [source_w, source_h],
        blocks,
        mask_size,
        mask_alpha,
    }
}

/// Otsu threshold of `gray` with the classic default [`CLASSIC_OTSU_NO_SPLIT`] when the
/// histogram has no split (empty or single-valued input).
pub(super) fn classic_otsu_threshold(gray: &[u8]) -> u8 {
    ms_raster::otsu_threshold(gray).unwrap_or(CLASSIC_OTSU_NO_SPLIT)
}

/// Square dilation in the classic 0/1 output format (nonzero input counts as set).
///
/// # Errors
/// `RasterError::LengthMismatch` when `src.len() != width * height`.
pub(super) fn dilate_binary(src: &[u8], width: usize, height: usize, rx: usize, ry: usize) -> Result<Vec<u8>, RasterError> {
    ms_raster::dilate_square(src, width, height, rx, ry, 1)
}

/// Nearest-upscales the working-size classic mask to `source_size`, normalized to 0/255.
///
/// Returns the mask unchanged when it is empty, already at source size, or when the source
/// size is zero or above the `mask::MAX_MASK_PIXELS` guard; an inconsistent buffer becomes an
/// empty mask at `mask_size`.
fn promote_classic_mask_to_source_size(
    mask_alpha: Vec<u8>,
    mask_size: [u32; 2],
    source_size: [u32; 2],
) -> ([u32; 2], Vec<u8>) {
    if mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 || mask_size == source_size {
        return (mask_size, mask_alpha);
    }
    if source_size[0] == 0 || source_size[1] == 0 || mask::exceeds_pixel_limit(source_size[0], source_size[1]) {
        return (mask_size, mask_alpha);
    }
    let Some(mask_img) = image::GrayImage::from_vec(mask_size[0], mask_size[1], mask_alpha) else {
        return (mask_size, Vec::new());
    };
    let resized = image::imageops::resize(
        &mask_img,
        source_size[0],
        source_size[1],
        FilterType::Nearest,
    );
    let mut alpha = resized.into_raw();
    mask::normalize_binary_alpha(&mut alpha);
    (source_size, alpha)
}

/// Labels 8-connected foreground components of `fg` and keeps the text-like ones (area, side,
/// size, density and aspect filters scaled to the page). Returns their rects scaled by
/// `(scale_x, scale_y)` to source pixels, finalized (reading order, capped), and the working-size
/// 0/255 mask of the accepted components.
fn extract_components_as_rects_and_mask(
    fg: &[u8],
    width: usize,
    height: usize,
    scale_x: f32,
    scale_y: f32,
) -> (Vec<TextDetectorRect>, Vec<u8>) {
    if fg.is_empty() || width == 0 || height == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut visited = vec![0u8; fg.len()];
    let mut stack = Vec::<usize>::new();
    let mut component_pixels = Vec::<usize>::new();
    let mut rects = Vec::<TextDetectorRect>::new();
    let mut mask_alpha = vec![0u8; fg.len()];

    let img_area = width * height;
    let min_side = (width.min(height) / 240).max(3) as u32;
    let min_area = (img_area / 25_000).max(24) as u32;
    let max_area = ((img_area / 7).max(min_area as usize + 1)) as u32;
    let max_w = (width as f32 * 0.85) as u32;
    let max_h = (height as f32 * 0.40) as u32;

    for seed in 0..fg.len() {
        if fg[seed] == 0 || visited[seed] != 0 {
            continue;
        }
        visited[seed] = 1;
        stack.clear();
        stack.push(seed);
        component_pixels.clear();

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut area = 0u32;

        while let Some(idx) = stack.pop() {
            component_pixels.push(idx);
            let x = (idx % width) as u32;
            let y = (idx / width) as u32;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            area = area.saturating_add(1);

            let y0 = y.saturating_sub(1) as usize;
            let y1 = (y as usize + 1).min(height - 1);
            let x0 = x.saturating_sub(1) as usize;
            let x1 = (x as usize + 1).min(width - 1);

            for ny in y0..=y1 {
                let row = ny * width;
                for nx in x0..=x1 {
                    let nidx = row + nx;
                    if fg[nidx] == 0 || visited[nidx] != 0 {
                        continue;
                    }
                    visited[nidx] = 1;
                    stack.push(nidx);
                }
            }
        }

        if area < min_area || area > max_area {
            continue;
        }
        let bw = max_x.saturating_sub(min_x) + 1;
        let bh = max_y.saturating_sub(min_y) + 1;
        if bw < min_side || bh < min_side {
            continue;
        }
        if bw > max_w || bh > max_h {
            continue;
        }

        let bbox_area = bw.saturating_mul(bh).max(1);
        let density = area as f32 / bbox_area as f32;
        if !(0.06..=0.95).contains(&density) {
            continue;
        }
        let aspect = bw as f32 / bh as f32;
        if !(0.08..=20.0).contains(&aspect) {
            continue;
        }

        let x1 = min_x as f32 * scale_x;
        let y1 = min_y as f32 * scale_y;
        let x2 = (max_x + 1) as f32 * scale_x;
        let y2 = (max_y + 1) as f32 * scale_y;
        if let Some(rect) = TextDetectorRect::from_xyxy(x1, y1, x2, y2) {
            rects.push(rect);
            for &idx in &component_pixels {
                mask_alpha[idx] = 255;
            }
        }
    }

    finalize_blocks(&mut rects);
    (rects, mask_alpha)
}
