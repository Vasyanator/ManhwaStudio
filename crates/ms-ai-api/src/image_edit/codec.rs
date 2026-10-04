/*
File: crates/ms-ai-api/src/image_edit/codec.rs

Purpose:
Image byte formats of the image-edit layer: the PNG of the image that is sent, the PNG of a
native mask in the polarity a provider expects, and the bounded decode of the provider's
answer (PNG / JPEG / WebP).

Key structures:
- MaskPolarity

Key functions:
- encode_rgb_png()
- encode_reference_png()
- encode_mask_png()
- decode_rgba()

Notes:
The sent image is RGB (alpha dropped) because `OpenAI` treats image alpha as an edit mask
when no mask is sent. A reference image keeps its alpha (a transparent marks layer means
"nothing here"); a fully opaque one is sent as RGB like the edited image. Decoding is bounded by `image::Limits` (16384 px per side, 512 MiB
allocations) so a hostile or broken answer cannot exhaust memory; the real decoded size is
returned and never trusted from a header field.
*/

use std::io::Cursor;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageReader, Limits};

use super::error::ImageEditError;
use super::request::{pixel_count, rgba_len};

/// Largest decoded side accepted from a provider.
pub const MAX_DECODE_SIDE: u32 = 16_384;
/// Largest decoder allocation accepted from a provider, in bytes.
pub const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// Which mask pixels a provider edits. The internal mask is always 255 = editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskPolarity {
    /// Grayscale PNG, white (255) = edit, black = keep (BFL, fal, Runware, Recraft).
    WhiteEdits,
    /// Grayscale PNG, black (0) = edit, white = keep (Ideogram).
    BlackEdits,
    /// RGBA PNG, transparent (alpha 0) = edit, opaque = keep (`OpenAI` images edits).
    TransparentEdits,
}

/// Encodes `rgb` (`width * height * 3` bytes) as an RGB8 PNG.
///
/// # Errors
/// `ImageEditError::ShapeMismatch` for a wrong buffer length, `Encode` when the encoder fails.
pub fn encode_rgb_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ImageEditError> {
    let expected = pixel_count(width, height)?.checked_mul(3).ok_or_else(|| ImageEditError::ShapeMismatch { detail: format!("RGB buffer of {width}x{height} overflows") })?;
    if rgb.len() != expected {
        return Err(ImageEditError::ShapeMismatch { detail: format!("RGB buffer of {width}x{height} must be {expected} bytes, got {}", rgb.len()) });
    }
    encode_png(rgb, width, height, ExtendedColorType::Rgb8)
}

/// Encodes a straight-alpha RGBA8 reference image (`width * height * 4` bytes) as a PNG: RGBA8
/// when any pixel is not fully opaque (the alpha is kept), else RGB8.
///
/// # Errors
/// `ImageEditError::ShapeMismatch` for a wrong buffer length, `Encode` when the encoder fails.
pub fn encode_reference_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ImageEditError> {
    let expected = rgba_len(width, height)?;
    if rgba.len() != expected {
        return Err(ImageEditError::ShapeMismatch { detail: format!("RGBA buffer of {width}x{height} must be {expected} bytes, got {}", rgba.len()) });
    }
    if rgba.chunks_exact(4).all(|pixel| pixel[3] == u8::MAX) {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
        return encode_png(&rgb, width, height, ExtendedColorType::Rgb8);
    }
    encode_png(rgba, width, height, ExtendedColorType::Rgba8)
}

/// Encodes the internal mask (`width * height` bytes, nonzero = editable) as a PNG in the
/// provider's `polarity`. Nonzero bytes are normalized to fully editable.
///
/// # Errors
/// `ImageEditError::ShapeMismatch` for a wrong buffer length, `Encode` when the encoder fails.
pub fn encode_mask_png(mask: &[u8], width: u32, height: u32, polarity: MaskPolarity) -> Result<Vec<u8>, ImageEditError> {
    let expected = pixel_count(width, height)?;
    if mask.len() != expected {
        return Err(ImageEditError::ShapeMismatch { detail: format!("mask of {width}x{height} must be {expected} bytes, got {}", mask.len()) });
    }
    match polarity {
        MaskPolarity::WhiteEdits => {
            let gray: Vec<u8> = mask.iter().map(|&value| if value != 0 { 255 } else { 0 }).collect();
            encode_png(&gray, width, height, ExtendedColorType::L8)
        }
        MaskPolarity::BlackEdits => {
            let gray: Vec<u8> = mask.iter().map(|&value| if value != 0 { 0 } else { 255 }).collect();
            encode_png(&gray, width, height, ExtendedColorType::L8)
        }
        MaskPolarity::TransparentEdits => {
            // Black RGB everywhere; only the alpha channel carries the mask.
            let rgba: Vec<u8> = mask.iter().flat_map(|&value| [0, 0, 0, if value != 0 { 0 } else { 255 }]).collect();
            encode_png(&rgba, width, height, ExtendedColorType::Rgba8)
        }
    }
}

/// PNG-encodes a buffer whose length was already checked against `color`.
fn encode_png(bytes: &[u8], width: u32, height: u32, color: ExtendedColorType) -> Result<Vec<u8>, ImageEditError> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(bytes, width, height, color).map_err(|error| ImageEditError::Encode { detail: error.to_string() })?;
    Ok(out)
}

/// Decodes a provider's image bytes (format sniffed from the content) into RGBA8, within the
/// decoder limits above. Returns `(width, height, rgba)`.
///
/// # Errors
/// `ImageEditError::Decode` for an unknown format, corrupt data or a limit violation.
pub fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageEditError> {
    let decode_error = |detail: String| ImageEditError::Decode { detail };
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|error| decode_error(error.to_string()))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| decode_error(error.to_string()))?.into_rgba8();
    let (width, height) = image.dimensions();
    Ok((width, height, image.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::{MaskPolarity, decode_rgba, encode_mask_png, encode_reference_png, encode_rgb_png};
    use crate::image_edit::error::ImageEditError;

    #[test]
    fn rgb_png_round_trips_with_opaque_alpha() {
        let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let png = encode_rgb_png(&rgb, 2, 2).unwrap_or_default();
        let decoded = decode_rgba(&png).ok();
        assert_eq!(decoded, Some((2, 2, vec![10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255])));
        assert!(matches!(encode_rgb_png(&rgb, 3, 2), Err(ImageEditError::ShapeMismatch { .. })));
    }

    #[test]
    fn reference_png_keeps_alpha_only_when_translucent() {
        let translucent = [255, 0, 0, 128, 0, 0, 0, 0];
        let png = encode_reference_png(&translucent, 2, 1).unwrap_or_default();
        assert_eq!(decode_rgba(&png).ok(), Some((2, 1, translucent.to_vec())));
        // An opaque reference is sent as RGB: the IHDR colour type byte (offset 25) is 2.
        let opaque = [1, 2, 3, 255, 4, 5, 6, 255];
        let png = encode_reference_png(&opaque, 2, 1).unwrap_or_default();
        assert_eq!(png.get(25), Some(&2));
        assert_eq!(decode_rgba(&png).ok(), Some((2, 1, opaque.to_vec())));
        assert!(matches!(encode_reference_png(&opaque, 3, 1), Err(ImageEditError::ShapeMismatch { .. })));
    }

    #[test]
    fn mask_polarity_table() {
        let mask = [0, 255, 7, 0];
        let decode = |polarity| encode_mask_png(&mask, 2, 2, polarity).ok().and_then(|png| decode_rgba(&png).ok()).map(|(_, _, rgba)| rgba);
        assert_eq!(decode(MaskPolarity::WhiteEdits), Some(vec![0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255]));
        assert_eq!(decode(MaskPolarity::BlackEdits), Some(vec![255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255]));
        assert_eq!(decode(MaskPolarity::TransparentEdits), Some(vec![0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255]));
        assert!(matches!(encode_mask_png(&mask, 3, 2, MaskPolarity::WhiteEdits), Err(ImageEditError::ShapeMismatch { .. })));
    }

    #[test]
    fn garbage_is_a_decode_error() {
        assert!(matches!(decode_rgba(b"not an image"), Err(ImageEditError::Decode { .. })));
        assert!(matches!(decode_rgba(&[]), Err(ImageEditError::Decode { .. })));
    }
}
