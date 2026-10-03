/*
File: crates/ms-tools/src/png_wire.rs

Purpose:
The one PNG codec for raster payloads exchanged with the AI backend over the wire: the encoder
pair for requests (region image + mask) and the mask decoder for responses. The cleaning tools
AOT and Flux-Fill, the AI-editor engines LaMa, SDXL and FLUX.2 klein and the watermark region
source / mask generation all go through here.

Key structures:
- `PngWireError`: typed, GUI-free encode failure; callers map it onto their own localized text.
- `PngDecodeError`: typed, GUI-free decode failure.

Key functions:
- `encode_rgba_png()`: `egui::ColorImage` -> RGBA8 PNG of the UNMULTIPLIED sRGB bytes.
- `encode_mask_png_l8()`: `width * height` gray bytes -> L8 PNG, bytes written as given.
- `decode_png_luma8()`: any decodable PNG (8- or 16-bit, gray or colour) -> 8-bit gray image.

Notes:
The output bytes are part of a cross-process contract (the backend decodes them), and every
caller sends exactly these bytes, so the encoder settings (the `image` crate's default PNG
compression and filter) must not change here without re-pinning the characterization hashes in
this file's tests and in the callers. The decoder only decodes: turning gray levels into a
binary mask, and the mask pixel limit, belong to the caller (`ms_text_detect::mask`).
*/

use eframe::egui;
use image::{ColorType, ImageEncoder};

/// Why a wire PNG could not be produced. Carries no user-facing text: each caller maps the
/// variant onto its own localized message (it knows whether it encoded an image or a mask).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PngWireError {
    /// The width does not fit the PNG's `u32` dimension field.
    #[error("PNG width {0} does not fit u32")]
    WidthTooLarge(usize),
    /// The height does not fit the PNG's `u32` dimension field.
    #[error("PNG height {0} does not fit u32")]
    HeightTooLarge(usize),
    /// The buffer does not hold exactly `width * height` pixels (also when that product
    /// overflows `usize`). Checked up front because `ImageEncoder::write_image` panics on it.
    #[error("PNG buffer holds {len} pixels, expected {width}x{height}")]
    SizeMismatch {
        /// Declared width in pixels.
        width: usize,
        /// Declared height in pixels.
        height: usize,
        /// Actual number of pixels in the buffer.
        len: usize,
    },
    /// The PNG encoder itself failed; the payload is its error text.
    #[error("PNG encoding failed: {0}")]
    Encode(String),
}

/// Why a wire PNG could not be decoded. Carries no user-facing text; the payload is the
/// decoder's technical message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("PNG decoding failed: {0}")]
pub struct PngDecodeError(pub String);

/// Encodes `image` as an RGBA8 PNG of its UNMULTIPLIED sRGB bytes
/// (`Color32::to_srgba_unmultiplied` per pixel), row-major as stored.
///
/// # Errors
/// `WidthTooLarge` / `HeightTooLarge` when a side does not fit `u32` (checked in that order),
/// `SizeMismatch` when `image.pixels.len() != width * height`, `Encode` when the encoder fails.
pub fn encode_rgba_png(image: &egui::ColorImage) -> Result<Vec<u8>, PngWireError> {
    let [width, height] = image.size;
    let (width_u32, height_u32) = png_dimensions(width, height)?;
    check_pixel_count(width, height, image.pixels.len())?;
    let mut raw = Vec::<u8>::with_capacity(image.pixels.len().saturating_mul(4));
    for px in &image.pixels {
        raw.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    write_png(&raw, width_u32, height_u32, ColorType::Rgba8)
}

/// Encodes `mask` (`width * height` bytes, row-major) as an L8 PNG. The bytes are written as
/// given; a binary mask must already be 0/255.
///
/// # Errors
/// `WidthTooLarge` / `HeightTooLarge` when a side does not fit `u32` (checked in that order,
/// before the length), `SizeMismatch` when `mask.len() != width * height`, `Encode` when the
/// encoder fails.
pub fn encode_mask_png_l8(mask: &[u8], width: usize, height: usize) -> Result<Vec<u8>, PngWireError> {
    let (width_u32, height_u32) = png_dimensions(width, height)?;
    check_pixel_count(width, height, mask.len())?;
    write_png(mask, width_u32, height_u32, ColorType::L8)
}

/// Decodes a PNG (or any format the `image` crate recognizes) from a response blob and
/// converts it to 8-bit gray with the `image` crate's `to_luma8` (16-bit levels are reduced to
/// 8 bits, colour is converted to luma). The gray levels are returned as decoded; a caller that
/// needs a binary mask normalizes them itself.
///
/// # Errors
/// `PngDecodeError` when the bytes are not a decodable image (an empty blob included).
pub fn decode_png_luma8(blob: &[u8]) -> Result<image::GrayImage, PngDecodeError> {
    image::load_from_memory(blob).map(|decoded| decoded.to_luma8()).map_err(|err| PngDecodeError(err.to_string()))
}

/// Converts both sides to the PNG's `u32` dimension fields, width first.
fn png_dimensions(width: usize, height: usize) -> Result<(u32, u32), PngWireError> {
    let width_u32 = u32::try_from(width).map_err(|_| PngWireError::WidthTooLarge(width))?;
    let height_u32 = u32::try_from(height).map_err(|_| PngWireError::HeightTooLarge(height))?;
    Ok((width_u32, height_u32))
}

/// Rejects a buffer that does not hold exactly `width * height` pixels.
fn check_pixel_count(width: usize, height: usize, len: usize) -> Result<(), PngWireError> {
    if width.checked_mul(height) == Some(len) {
        Ok(())
    } else {
        Err(PngWireError::SizeMismatch { width, height, len })
    }
}

/// Runs the `image` crate's PNG encoder with its default settings; `raw` was size-checked by
/// the caller, so `write_image`'s length panic cannot fire.
fn write_png(raw: &[u8], width: u32, height: u32, color: ColorType) -> Result<Vec<u8>, PngWireError> {
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(raw, width, height, color.into())
        .map_err(|err| PngWireError::Encode(err.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The (len, FNV) pairs below are the characterization values OBSERVED from the five
    // pre-refactor cleaning encoder copies, so these tests prove the owner writes byte-identical
    // output. This is the only place they are pinned; the cleaning adapter
    // (`ms-tab-cleaning` `tools/region_png.rs`) tests only its error mapping and alpha threshold.

    /// FNV-1a 64 over `bytes`: the fingerprint the characterization tests pinned.
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

    /// The characterization 7x5 RGBA image with `alpha` on every pixel.
    fn rgba_test_image(alpha: u8) -> egui::ColorImage {
        let mut state = 0x0e9c_0de5_u64;
        let rgba = (0..7 * 5)
            .flat_map(|_| {
                let [r, g, b, _] = lcg_next(&mut state).to_le_bytes();
                [r, g, b, alpha]
            })
            .collect::<Vec<_>>();
        egui::ColorImage::from_rgba_unmultiplied([7, 5], &rgba)
    }

    /// The characterization 7x5 binary (0/255) mask.
    fn mask_test_bytes() -> Vec<u8> {
        let mut state = 0x3a5c_u64;
        (0..7 * 5).map(|_| if lcg_next(&mut state).is_multiple_of(3) { 255 } else { 0 }).collect()
    }

    #[test]
    fn rgba_png_matches_pre_refactor_bytes() {
        let png = encode_rgba_png(&rgba_test_image(255)).expect("encode opaque");
        assert_eq!((png.len(), fnv1a64(&png)), (213, 12_688_046_720_201_646_502));
        let half = encode_rgba_png(&rgba_test_image(128)).expect("encode half alpha");
        assert_eq!((half.len(), fnv1a64(&half)), (213, 7_508_081_844_856_187_298));
    }

    #[test]
    fn rgba_png_writes_unmultiplied_bytes() {
        let image = rgba_test_image(128);
        let png = encode_rgba_png(&image).expect("encode half alpha");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8().into_raw();
        let unmultiplied = image.pixels.iter().flat_map(|px| px.to_srgba_unmultiplied()).collect::<Vec<_>>();
        assert_eq!(decoded, unmultiplied);
    }

    #[test]
    fn mask_png_l8_matches_pre_refactor_bytes() {
        let png = encode_mask_png_l8(&mask_test_bytes(), 7, 5).expect("encode mask");
        assert_eq!((png.len(), fnv1a64(&png)), (108, 8_381_052_489_750_066_891));
    }

    #[test]
    fn size_mismatch_is_an_error_not_a_panic() {
        assert_eq!(
            encode_mask_png_l8(&mask_test_bytes(), 7, 4),
            Err(PngWireError::SizeMismatch { width: 7, height: 4, len: 35 })
        );
        assert_eq!(encode_mask_png_l8(&[0u8; 5], 2, 2), Err(PngWireError::SizeMismatch { width: 2, height: 2, len: 5 }));
        let mut image = rgba_test_image(255);
        image.pixels.pop();
        assert_eq!(encode_rgba_png(&image), Err(PngWireError::SizeMismatch { width: 7, height: 5, len: 34 }));
    }

    #[test]
    fn oversized_sides_are_rejected_width_first() {
        // A side above `u32::MAX` with the other side 0 is a zero-area buffer, so the test needs
        // no allocation. Skipped where `usize` cannot hold it (32-bit targets).
        let Ok(huge) = usize::try_from(u64::from(u32::MAX) + 1) else {
            return;
        };
        assert_eq!(encode_mask_png_l8(&[], huge, 0), Err(PngWireError::WidthTooLarge(huge)));
        assert_eq!(encode_mask_png_l8(&[], 0, huge), Err(PngWireError::HeightTooLarge(huge)));
        assert_eq!(encode_mask_png_l8(&[], huge, huge), Err(PngWireError::WidthTooLarge(huge)));
        let mut image = egui::ColorImage::filled([0, 0], egui::Color32::BLACK);
        image.size = [huge, 0];
        assert_eq!(encode_rgba_png(&image), Err(PngWireError::WidthTooLarge(huge)));
        image.size = [0, huge];
        assert_eq!(encode_rgba_png(&image), Err(PngWireError::HeightTooLarge(huge)));
    }

    #[test]
    fn decode_round_trips_the_l8_mask_encoder() {
        let mask = mask_test_bytes();
        let png = encode_mask_png_l8(&mask, 7, 5).expect("encode mask");
        let gray = decode_png_luma8(&png).expect("decode mask");
        assert_eq!(gray.dimensions(), (7, 5));
        assert_eq!(gray.into_raw(), mask);
    }

    #[test]
    fn decode_keeps_gray_levels_and_reduces_16_bit() {
        let levels = image::GrayImage::from_fn(4, 2, |x, y| image::Luma([[0u8, 1, 127, 254, 255, 0, 2, 0][usize::try_from(y * 4 + x).unwrap_or(0)]]));
        let mut png = Vec::new();
        levels.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).expect("encode test PNG");
        assert_eq!(decode_png_luma8(&png).expect("levels").into_raw(), vec![0, 1, 127, 254, 255, 0, 2, 0]);

        let wide = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_fn(3, 1, |x, _| image::Luma([[0u16, 1, 300][usize::try_from(x).unwrap_or(0)]]));
        let mut png16 = Vec::new();
        wide.write_to(&mut std::io::Cursor::new(&mut png16), image::ImageFormat::Png).expect("encode 16-bit test PNG");
        // `to_luma8` keeps the high byte: 1 decays to 0, 300 becomes 1.
        assert_eq!(decode_png_luma8(&png16).expect("16-bit").into_raw(), vec![0, 0, 1]);
    }

    #[test]
    fn decode_rejects_non_images() {
        assert!(decode_png_luma8(b"not a png at all").is_err());
        assert!(decode_png_luma8(&[]).is_err());
    }
}
