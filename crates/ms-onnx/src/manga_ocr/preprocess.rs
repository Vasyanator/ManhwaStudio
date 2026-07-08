/*
File: crates/ms-onnx/src/manga_ocr/preprocess.rs

Purpose:
Faithful port of the MangaOCR image preprocessing (`_prepare_ocr_image` +
`ViTImageProcessor`) that turns an input image into the encoder's `pixel_values`
tensor data (NCHW f32, shape [1, 3, 224, 224], values in [-1, 1]).

Key functions:
- luma_from_rgb   : PIL "L" (ITU-R 601-2) grayscale of an RGB pixel.
- normalize_pixel : rescale/normalize a grayscale byte to [-1, 1].
- preprocess      : full pipeline image -> NCHW f32 vector.

Notes:
Pipeline (matches Python exactly except the resampling kernel):
  1. grayscale via PIL's integer L24 formula, then replicate to 3 RGB channels
     (kept implicit: all channels are identical, so the grayscale plane is copied
     three times at the end);
  2. images smaller than 2x2 are upscaled to >= 2x2 with NEAREST first;
  3. UNCONDITIONAL resize to 224x224;
  4. rescale by 1/255, then normalize with mean=std=0.5 -> (x/255 - 0.5) / 0.5.
Parity risk: PIL uses BILINEAR (`resample=2`) resizing; the `image` crate's closest
equivalent is `FilterType::Triangle`. Kernels differ slightly at sub-pixel level,
so resized pixels may differ by a small amount from the Python path.
*/

use image::{GrayImage, Luma, RgbaImage, imageops};

use crate::OrtError;

/// Fixed model input side length (ViTImageProcessor `size` = 224x224).
pub const OCR_IMAGE_SIDE: u32 = 224;
/// Number of channels in the NCHW tensor (grayscale replicated to RGB).
pub const OCR_CHANNELS: usize = 3;

/// PIL "L" grayscale of an RGB pixel (ITU-R 601-2 luma with rounding).
///
/// Uses the exact integer coefficients PIL applies for `convert("L")`:
/// `(R*19595 + G*38470 + B*7471 + 32768) >> 16`. The alpha channel is ignored,
/// matching PIL's RGBA->L behavior.
#[must_use]
pub fn luma_from_rgb(r: u8, g: u8, b: u8) -> u8 {
    let acc = u32::from(r) * 19595 + u32::from(g) * 38470 + u32::from(b) * 7471 + 32768;
    // acc >> 16 is at most 255 (all-white pixel), so it always fits in u8.
    u8::try_from(acc >> 16).unwrap_or(u8::MAX)
}

/// Rescales and normalizes a grayscale byte to the model's [-1, 1] range.
///
/// Matches ViTImageProcessor: rescale by 1/255, then normalize with mean=std=0.5,
/// i.e. `(x/255 - 0.5) / 0.5`.
#[must_use]
pub fn normalize_pixel(value: u8) -> f32 {
    (f32::from(value) / 255.0 - 0.5) / 0.5
}

/// Preprocesses an image into the encoder's `pixel_values` data (NCHW f32).
///
/// Returns a `3 * 224 * 224` vector laid out channel-major (all of channel 0, then
/// channel 1, then channel 2); the three channels are identical (grayscale
/// replicated to RGB). Pair it with shape `[1, 3, 224, 224]` when building the
/// input tensor.
///
/// # Errors
/// [`OrtError::ImagePreprocess`] if the input image has zero width or height (no
/// source pixels to resample).
pub fn preprocess(image: &RgbaImage) -> Result<Vec<f32>, OrtError> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(OrtError::ImagePreprocess {
            detail: format!("пустое изображение {width}x{height}"),
        });
    }

    // Step 1: grayscale (RGB replication stays implicit; alpha ignored).
    let mut gray = GrayImage::new(width, height);
    for (dst, src) in gray.pixels_mut().zip(image.pixels()) {
        let [r, g, b, _a] = src.0;
        *dst = Luma([luma_from_rgb(r, g, b)]);
    }

    // Step 2: upscale sub-2x2 images to >= 2x2 with NEAREST before the real resize.
    let gray = if width < 2 || height < 2 {
        imageops::resize(&gray, width.max(2), height.max(2), imageops::FilterType::Nearest)
    } else {
        gray
    };

    // Step 3: unconditional resize to 224x224. Triangle is the closest bilinear
    // kernel to PIL's BILINEAR (documented parity risk in the file header).
    let resized = imageops::resize(
        &gray,
        OCR_IMAGE_SIDE,
        OCR_IMAGE_SIDE,
        imageops::FilterType::Triangle,
    );

    // Step 4: rescale + normalize into a single-channel plane...
    let plane: Vec<f32> = resized.pixels().map(|p| normalize_pixel(p.0[0])).collect();
    // ...then replicate to 3 channels (NCHW: channel-major).
    let mut data = Vec::with_capacity(OCR_CHANNELS * plane.len());
    for _ in 0..OCR_CHANNELS {
        data.extend_from_slice(&plane);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_matches_pil_reference_values() {
        // Ground truth from PIL Image.convert("L") on solid colors.
        assert_eq!(luma_from_rgb(255, 0, 0), 76);
        assert_eq!(luma_from_rgb(0, 255, 0), 150);
        assert_eq!(luma_from_rgb(0, 0, 255), 29);
        assert_eq!(luma_from_rgb(128, 128, 128), 128);
        assert_eq!(luma_from_rgb(10, 200, 90), 131);
        assert_eq!(luma_from_rgb(255, 255, 255), 255);
        assert_eq!(luma_from_rgb(1, 2, 3), 2);
    }

    #[test]
    fn normalize_maps_endpoints_and_midpoint() {
        assert!((normalize_pixel(0) - (-1.0)).abs() < 1e-6);
        assert!((normalize_pixel(255) - 1.0).abs() < 1e-6);
        // (128/255 - 0.5) / 0.5 = 0.00392156...
        assert!((normalize_pixel(128) - 0.003_921_57).abs() < 1e-5);
    }

    #[test]
    fn preprocess_solid_image_shape_channels_and_values() {
        // A solid-color image stays solid through resize, so every output pixel is
        // the normalized luma of that color, and all 3 channels are identical.
        let color = image::Rgba([10, 200, 90, 255]);
        let img = RgbaImage::from_pixel(32, 48, color);
        let data = preprocess(&img).expect("preprocess must succeed on a valid image");

        let side = usize::try_from(OCR_IMAGE_SIDE).unwrap_or(224);
        let plane_len = side * side;
        assert_eq!(data.len(), OCR_CHANNELS * plane_len);

        let expected = normalize_pixel(luma_from_rgb(10, 200, 90));
        // All values equal the expected normalized luma (constant image).
        for &v in &data {
            assert!((v - expected).abs() < 1e-6);
        }
        // Channels are identical: sample the same offset in each channel plane.
        for channel in 0..OCR_CHANNELS {
            assert!((data[channel * plane_len] - data[0]).abs() < 1e-9);
        }
    }

    #[test]
    fn preprocess_rejects_empty_image() {
        let img = RgbaImage::new(0, 4);
        assert!(matches!(
            preprocess(&img),
            Err(OrtError::ImagePreprocess { .. })
        ));
    }

    #[test]
    fn preprocess_upscales_sub_2x2_without_panic() {
        // 1x1 image: nearest-upscaled to 2x2, then resized to 224x224.
        let img = RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let data = preprocess(&img).expect("1x1 image must preprocess");
        let side = usize::try_from(OCR_IMAGE_SIDE).unwrap_or(224);
        assert_eq!(data.len(), OCR_CHANNELS * side * side);
        // Solid black -> luma 0 -> normalized -1.0 everywhere.
        for &v in &data {
            assert!((v - (-1.0)).abs() < 1e-6);
        }
    }
}
