/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/region.rs
In-memory region detection for the Cleaning tools (`pub`, re-exported from the detector
module): the region runs through the same plan / runner / postprocess pipeline as a page
(`pipeline::detect_image`) and only its mask comes back, dilated here in Rust.

Key functions:
- `detect_ai_ctd_mask_for_image()`: CTD on the backend.
- `detect_paddle_mask_for_image()`: PaddleOCR, native route first, backend fallback.
- `detect_surya_mask_for_image()`: Surya on the backend.
- `color_image_to_rgb()`: region `ColorImage` -> RGB of its UNMULTIPLIED colour (alpha dropped).
- `region_mask()`: the shared shape of the three helpers (empty-region rule, conversion,
  detection, dilation).

Notes:
Every helper returns `([w, h], alpha)` at the REGION size with 0/255 alpha, and the empty mask
for an empty region. The one square dilation owner is `ms_raster` (via `dilate_mask_alpha`).
*/

use super::pipeline::{self, DetectorRoute};
use super::{TextDetectorAiCtdOptions, TextDetectorPaddleOcrOptions, dilate_mask_alpha};
use eframe::egui;
use image::RgbImage;
use ms_text_detect::{DetectParams, Detection, EngineKind};

/// Runs CTD on an in-memory region and returns its mask dilated in Rust by
/// `options.mask_dilate_size` (clamped 0..=30). Worker-thread only (blocking IPC).
///
/// # Errors
/// A localized message on conversion, model, IPC or pipeline failure.
// `pub`, not crate-private: consumed by the `cleaning` tab, which lives in another crate.
pub fn detect_ai_ctd_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorAiCtdOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    let params = pipeline::ctd_detect_params(options);
    region_mask(image, options.mask_dilate_size, |page| {
        pipeline::detect_image(EngineKind::Ctd, page, &params, DetectorRoute::Backend)
    })
}

/// Runs PaddleOCR detection on an in-memory region (native route first, backend fallback) and
/// returns its glyph mask dilated in Rust by `options.mask_dilate_size`. Worker-thread only.
///
/// # Errors
/// A localized message on conversion, model, inference, IPC or pipeline failure.
// `pub`, not crate-private: consumed by the `cleaning` tab, which lives in another crate.
pub fn detect_paddle_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorPaddleOcrOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    region_mask(image, options.mask_dilate_size, |page| {
        pipeline::detect_image(EngineKind::Paddle, page, &DetectParams::default(), pipeline::paddle_route())
    })
}

/// Runs Surya detection on an in-memory region through the backend and returns its mask
/// dilated in Rust by `dilate_size` (clamped 0..=30). Worker-thread only.
///
/// # Errors
/// A localized message on conversion, IPC or pipeline failure.
// `pub`, not crate-private: consumed by the `cleaning` tab, which lives in another crate.
pub fn detect_surya_mask_for_image(
    image: &egui::ColorImage,
    dilate_size: i32,
) -> Result<([u32; 2], Vec<u8>), String> {
    region_mask(image, dilate_size, |page| {
        pipeline::detect_image(EngineKind::Surya, page, &DetectParams::default(), DetectorRoute::Backend)
    })
}

/// The common body of the region helpers: an empty region is the empty mask (no detection);
/// otherwise the region is converted to RGB, detected by `detect`, and the source-size mask is
/// dilated by `dilate_size` (clamped 0..=30).
///
/// # Errors
/// The conversion error or `detect`'s error.
pub(super) fn region_mask(
    image: &egui::ColorImage,
    dilate_size: i32,
    detect: impl FnOnce(&RgbImage) -> Result<Detection, String>,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }
    let page = color_image_to_rgb(image)?;
    let detection = detect(&page)?;
    let mask_size = detection.mask.size;
    let mut mask_alpha = detection.mask.alpha;
    dilate_mask_alpha(&mut mask_alpha, mask_size, dilate_size);
    Ok((mask_size, mask_alpha))
}

/// Converts a region `ColorImage` to an RGB image of its UNMULTIPLIED sRGB colour
/// (`Color32::to_srgba_unmultiplied`, alpha dropped), the same colour the backend wire PNGs
/// carry (`ms_tools::png_wire`).
///
/// # Errors
/// A localized message when a side does not fit `u32` or the pixel buffer does not match the
/// declared size.
pub(super) fn color_image_to_rgb(image: &egui::ColorImage) -> Result<RgbImage, String> {
    let width = u32::try_from(image.size[0]).map_err(|_| t!("translation.text_detector.image_width_too_large_error").to_string())?;
    let height = u32::try_from(image.size[1]).map_err(|_| t!("translation.text_detector.image_height_too_large_error").to_string())?;
    let size_error = || {
        ms_log::runtime_log::log_error(format!(
            "[text-detector] region image {width}x{height} has {} pixels",
            image.pixels.len()
        ));
        tf!("translation.text_detector.region_image_size_error", w = width, h = height, len = image.pixels.len())
    };
    // `RgbImage::from_raw` accepts an over-long buffer, so the exact pixel count is checked here.
    if image.size[0].checked_mul(image.size[1]) != Some(image.pixels.len()) {
        return Err(size_error());
    }
    let mut raw = Vec::with_capacity(image.pixels.len().saturating_mul(3));
    for px in &image.pixels {
        let [r, g, b, _] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b]);
    }
    RgbImage::from_raw(width, height, raw).ok_or_else(size_error)
}
