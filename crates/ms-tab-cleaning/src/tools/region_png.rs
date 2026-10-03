/*
File: crates/ms-tab-cleaning/src/tools/region_png.rs

Purpose:
The cleaning tab's localized face of `ms_tools::png_wire`, the one codec of the region and
mask PNGs exchanged with the AI backend. Every cleaning call site (AOT, Flux-Fill, the AI-editor
engines LaMa, SDXL, FLUX.2 klein, and the watermark sources) encodes and decodes through these
functions; none owns a PNG codec of its own.

Key functions:
- `encode_color_image_png_rgba()`: region `ColorImage` -> unmultiplied RGBA8 PNG.
- `encode_mask_png_luma()`: mask `ColorImage` -> L8 PNG of `alpha > 0 -> 255`.
- `encode_mask_png_l8()`: `width * height` mask bytes -> L8 PNG, bytes as given.
- `decode_mask_png()`: response mask PNG -> `([w, h], 0/255 alpha)`, normalized and guarded by
  `ms_text_detect::mask::binary_alpha_from_gray`.

Notes:
Only the error mapping lives here: `PngWireError` carries no text, and these adapters turn it
into the existing `cleaning.png.*` / `cleaning.inpaint.size_mismatch_error` messages (and a
runtime-log line), so the user-visible strings are unchanged by the move to one owner.
*/

use eframe::egui;
use ms_text_detect::mask::{self, MaskError};
use ms_tools::png_wire::{self, PngWireError};

/// Encodes the region image as an unmultiplied RGBA8 PNG for the backend wire.
///
/// # Errors
/// A localized message: a side does not fit `u32` (`cleaning.png.image_*_too_large_error`), or
/// the pixel buffer disagrees with `size` or the encoder fails (`cleaning.png.encode_image_error`).
pub(crate) fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    png_wire::encode_rgba_png(image).map_err(|err| {
        log_encode_failure("image", &err);
        match err {
            PngWireError::WidthTooLarge(_) => t!("cleaning.png.image_width_too_large_error").to_string(),
            PngWireError::HeightTooLarge(_) => t!("cleaning.png.image_height_too_large_error").to_string(),
            // A pixel count that disagrees with `size` is a malformed `ColorImage`, not a user
            // error; report it as an encode failure carrying the technical detail.
            PngWireError::SizeMismatch { .. } => tf!("cleaning.png.encode_image_error", err = err),
            PngWireError::Encode(detail) => tf!("cleaning.png.encode_image_error", err = detail),
        }
    })
}

/// Encodes a mask `ColorImage` as an L8 PNG: a pixel with any nonzero alpha becomes 255, every
/// other pixel 0 (RGB is ignored).
///
/// # Errors
/// The same localized messages as [`encode_mask_png_l8`].
pub(crate) fn encode_mask_png_luma(mask: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let raw = mask.pixels.iter().map(|px| if px.a() > 0 { 255 } else { 0 }).collect::<Vec<u8>>();
    encode_mask_png_l8(&raw, mask.size[0], mask.size[1])
}

/// Encodes `mask` (`width * height` bytes, row-major) as an L8 PNG for the backend wire.
///
/// # Errors
/// A localized message: a side does not fit `u32` (`cleaning.png.mask_*_too_large_error`,
/// checked first), `mask` is not `width * height` bytes (`cleaning.inpaint.size_mismatch_error`),
/// or the encoder fails (`cleaning.png.encode_mask_error`).
pub(crate) fn encode_mask_png_l8(mask: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    png_wire::encode_mask_png_l8(mask, width, height).map_err(|err| {
        log_encode_failure("mask", &err);
        match err {
            PngWireError::WidthTooLarge(_) => t!("cleaning.png.mask_width_too_large_error").to_string(),
            PngWireError::HeightTooLarge(_) => t!("cleaning.png.mask_height_too_large_error").to_string(),
            PngWireError::SizeMismatch { .. } => t!("cleaning.inpaint.size_mismatch_error").to_string(),
            PngWireError::Encode(detail) => tf!("cleaning.png.encode_mask_error", err = detail),
        }
    })
}

/// Decodes a mask PNG returned by the backend into `([w, h], alpha)` in the mask's own
/// resolution, every nonzero gray level normalized to 255. An empty blob or a zero-area image
/// is the empty mask `([0, 0], [])`.
///
/// # Errors
/// A localized message when the bytes are not a decodable image
/// (`cleaning.png.decode_mask_error`) or the mask exceeds `ms_text_detect::mask::MAX_MASK_PIXELS`
/// (`cleaning.png.mask_too_large_error`); both are also logged.
pub(crate) fn decode_mask_png(blob: &[u8]) -> Result<([u32; 2], Vec<u8>), String> {
    if blob.is_empty() {
        return Ok(([0, 0], Vec::new()));
    }
    let gray = png_wire::decode_png_luma8(blob).map_err(|err| {
        ms_log::runtime_log::log_warn(format!("[cleaning] backend wire mask PNG decode failed: {err} ({} bytes)", blob.len()));
        tf!("cleaning.png.decode_mask_error", err = err)
    })?;
    match mask::binary_alpha_from_gray(gray) {
        Ok(mask) => Ok((mask.size, mask.alpha)),
        Err(MaskError::TooLarge { width, height }) => {
            ms_log::runtime_log::log_warn(format!("[cleaning] backend wire mask {width}x{height} rejected: over the detector mask pixel limit"));
            Err(tf!("cleaning.png.mask_too_large_error", w = width, h = height))
        }
    }
}

/// Records the technical failure (which payload, typed reason) before it becomes UI text.
fn log_encode_failure(payload: &str, err: &PngWireError) {
    ms_log::runtime_log::log_warn(format!("[cleaning] backend wire {payload} PNG encode failed: {err}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_errors_keep_their_localized_keys() {
        assert_eq!(encode_mask_png_l8(&[0u8; 5], 2, 2), Err(t!("cleaning.inpaint.size_mismatch_error").to_string()));
        let Ok(huge) = usize::try_from(u64::from(u32::MAX) + 1) else {
            return;
        };
        assert_eq!(encode_mask_png_l8(&[], huge, 0), Err(t!("cleaning.png.mask_width_too_large_error").to_string()));
        assert_eq!(encode_mask_png_l8(&[], 0, huge), Err(t!("cleaning.png.mask_height_too_large_error").to_string()));
    }

    #[test]
    fn image_errors_keep_their_localized_keys() {
        let Ok(huge) = usize::try_from(u64::from(u32::MAX) + 1) else {
            return;
        };
        let mut image = egui::ColorImage::filled([0, 0], egui::Color32::BLACK);
        image.size = [huge, 0];
        assert_eq!(encode_color_image_png_rgba(&image), Err(t!("cleaning.png.image_width_too_large_error").to_string()));
        image.size = [0, huge];
        assert_eq!(encode_color_image_png_rgba(&image), Err(t!("cleaning.png.image_height_too_large_error").to_string()));
    }

    /// The luma adapter maps any nonzero alpha (whatever the RGB) to 255 and zero alpha to 0,
    /// then writes exactly the owner's L8 bytes for that 0/255 mask (the bytes themselves are
    /// pinned in the `ms_tools::png_wire` tests).
    #[test]
    fn mask_luma_thresholds_alpha_then_encodes_like_the_owner() {
        let alphas = [0u8, 1, 128, 255, 0, 7];
        let rgba = alphas.iter().flat_map(|&alpha| [40, 90, 200, alpha]).collect::<Vec<_>>();
        let image = egui::ColorImage::from_rgba_unmultiplied([3, 2], &rgba);
        let expected = png_wire::encode_mask_png_l8(&[0, 255, 255, 255, 0, 255], 3, 2).expect("owner encode");
        assert_eq!(encode_mask_png_luma(&image), Ok(expected));
    }

    #[test]
    fn malformed_image_is_an_encode_error_not_a_panic() {
        let mut image = egui::ColorImage::filled([3, 2], egui::Color32::BLACK);
        image.pixels.pop();
        let err = encode_color_image_png_rgba(&image).expect_err("pixel count mismatch");
        let detail = PngWireError::SizeMismatch { width: 3, height: 2, len: 5 };
        assert_eq!(err, tf!("cleaning.png.encode_image_error", err = detail));
    }

    /// Builds an 8-bit gray PNG of `width x height` filled with `value`.
    fn gray_png(width: u32, height: u32, value: u8) -> Vec<u8> {
        let mut png = Vec::new();
        image::GrayImage::from_pixel(width, height, image::Luma([value]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode test PNG");
        png
    }

    #[test]
    fn decoded_mask_is_normalized_and_empty_blob_is_empty_mask() {
        assert_eq!(decode_mask_png(&[]), Ok(([0, 0], Vec::new())));
        assert_eq!(decode_mask_png(&gray_png(2, 1, 42)), Ok(([2, 1], vec![255, 255])));
        assert_eq!(decode_mask_png(&gray_png(1, 1, 0)), Ok(([1, 1], vec![0])));
        let levels = png_wire::encode_mask_png_l8(&[0, 1, 127, 255], 2, 2).expect("owner encode");
        assert_eq!(decode_mask_png(&levels), Ok(([2, 2], vec![0, 255, 255, 255])));
    }

    #[test]
    fn decode_errors_keep_their_localized_keys() {
        let err = decode_mask_png(b"not a png at all").expect_err("non-PNG blob");
        let detail = png_wire::decode_png_luma8(b"not a png at all").expect_err("non-PNG blob");
        assert_eq!(err, tf!("cleaning.png.decode_mask_error", err = detail));
        let oversize = gray_png(10_001, 10_000, 0);
        assert_eq!(decode_mask_png(&oversize), Err(tf!("cleaning.png.mask_too_large_error", w = 10_001, h = 10_000)));
    }
}
