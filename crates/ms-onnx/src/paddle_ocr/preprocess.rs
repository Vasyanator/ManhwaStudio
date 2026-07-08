/*
File: crates/ms-onnx/src/paddle_ocr/preprocess.rs

Purpose:
PaddleOCR detection and recognition input preprocessing. Faithful port of
`preprocess_det_image` / `resize_image_for_det` and
`preprocess_rec_image_to_width` / `plan_rec_input_width` in
`modules/ai_backend/paddle_onnx_runtime.py`.

Key functions:
- resize_dims_for_det : longest-side<=960, snapped to a stride-32 multiple, no upscale.
- preprocess_det      : image -> DetInput (NCHW f32, ImageNet-normalized) + model/src dims.
- plan_rec_width      : target batch width for one crop (dynamic width, [320, 3200]).
- preprocess_rec      : crop -> NCHW f32 (height 48, (x/255-0.5)/0.5, right zero-pad).

Notes:
Parity risk: cv2.resize uses INTER_LINEAR; the closest `image` kernel is
`FilterType::Triangle` (bilinear). Sub-pixel values may differ slightly from the
Python path, which is acceptable since the tensors feed robust CNNs. The
stride-snap uses round-half-away-from-zero (`f32::round`); Python's built-in
`round` is round-half-to-even, so results differ only for the rare exact-.5 case.
*/

use image::{RgbaImage, imageops};

use super::{nonneg_f32_to_u32, u32_to_f32};
use crate::OrtError;

/// Detection: longest source side is resized to at most this many pixels.
const DET_RESIZE_LONG: u32 = 960;
/// Detection: resized dimensions are snapped to a multiple of this stride.
const DET_STRIDE: u32 = 32;
/// Detection ImageNet normalization mean, RGB order (`(rgb/255 - mean)/std`).
const DET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
/// Detection ImageNet normalization std, RGB order.
const DET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Recognition: fixed model input height (crops are resized to this height).
pub const REC_HEIGHT: u32 = 48;
/// Recognition: minimum dynamic batch width.
pub const REC_MIN_WIDTH: u32 = 320;
/// Recognition: maximum dynamic batch width.
pub const REC_MAX_WIDTH: u32 = 3200;

/// Preprocessed detection input: the NCHW tensor plus the model and source dims.
#[derive(Debug, Clone)]
pub struct DetInput {
    /// Row-major NCHW f32 data, shape `[1, 3, model_h, model_w]`, channel-major.
    pub data: Vec<f32>,
    /// Model input height (stride-snapped resized height).
    pub model_h: u32,
    /// Model input width (stride-snapped resized width).
    pub model_w: u32,
    /// Original source image width (used to rescale detection boxes back).
    pub src_w: u32,
    /// Original source image height.
    pub src_h: u32,
}

/// Computes the stride-snapped detection resize dimensions `(model_w, model_h)`.
///
/// The longest side is scaled down to at most [`DET_RESIZE_LONG`] (never upscaled),
/// then each side is snapped to the nearest multiple of [`DET_STRIDE`] (>= one
/// stride). Matches `resize_image_for_det`.
#[must_use]
pub fn resize_dims_for_det(src_w: u32, src_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (DET_STRIDE, DET_STRIDE);
    }
    let longest = src_w.max(src_h);
    let ratio = if longest > DET_RESIZE_LONG {
        u32_to_f32(DET_RESIZE_LONG) / u32_to_f32(longest)
    } else {
        1.0
    };

    // Python truncates (int()) before the stride snap, then rounds to a stride
    // multiple; both stay >= one stride.
    let snap = |side: u32| -> u32 {
        let scaled = nonneg_f32_to_u32((u32_to_f32(side) * ratio).trunc()).max(1);
        let snapped = nonneg_f32_to_u32((u32_to_f32(scaled) / u32_to_f32(DET_STRIDE)).round())
            * DET_STRIDE;
        snapped.max(DET_STRIDE)
    };

    (snap(src_w), snap(src_h))
}

/// Preprocesses an image into the detection model's NCHW input.
///
/// Resizes to the stride-snapped dimensions (bilinear), reads RGB (alpha ignored),
/// normalizes with the ImageNet mean/std, and lays the tensor out channel-major.
///
/// # Errors
/// [`OrtError::ImagePreprocess`] if the image has zero width or height.
pub fn preprocess_det(image: &RgbaImage) -> Result<DetInput, OrtError> {
    let (src_w, src_h) = image.dimensions();
    if src_w == 0 || src_h == 0 {
        return Err(OrtError::ImagePreprocess {
            detail: format!("пустое изображение {src_w}x{src_h} для детектора PaddleOCR"),
        });
    }

    let (model_w, model_h) = resize_dims_for_det(src_w, src_h);
    // Triangle (bilinear) is the closest kernel to cv2 INTER_LINEAR.
    let resized = imageops::resize(image, model_w, model_h, imageops::FilterType::Triangle);

    let plane = usize::try_from(model_w)
        .ok()
        .and_then(|w| usize::try_from(model_h).ok().map(|h| w * h))
        .ok_or_else(|| OrtError::ImagePreprocess {
            detail: "размеры детектора не помещаются в usize".to_owned(),
        })?;

    // Channel-major NCHW: fill R plane, then G, then B.
    let mut data = vec![0.0_f32; 3 * plane];
    for (i, pixel) in resized.pixels().enumerate() {
        let [r, g, b, _a] = pixel.0;
        let channels = [r, g, b];
        for (c, &value) in channels.iter().enumerate() {
            let normalized = (f32::from(value) / 255.0 - DET_MEAN[c]) / DET_STD[c];
            data[c * plane + i] = normalized;
        }
    }

    Ok(DetInput {
        data,
        model_h,
        model_w,
        src_w,
        src_h,
    })
}

/// Computes the width-by-ratio for a crop at the fixed recognition height.
///
/// `ceil(REC_HEIGHT * crop_w / crop_h)`, clamped to at least 1. Both dims are
/// treated as at least 1 (matching Python's `max(..., 1)`).
#[must_use]
fn width_by_ratio(crop_w: u32, crop_h: u32) -> u32 {
    let w = u32_to_f32(crop_w.max(1));
    let h = u32_to_f32(crop_h.max(1));
    nonneg_f32_to_u32((u32_to_f32(REC_HEIGHT) * (w / h)).ceil()).max(1)
}

/// Plans the dynamic batch width for a single recognition crop.
///
/// Returns `min(max(REC_MIN_WIDTH, width_by_ratio), REC_MAX_WIDTH)`, matching
/// `plan_rec_input_width` for the dynamic-width recognizer.
#[must_use]
pub fn plan_rec_width(crop_w: u32, crop_h: u32) -> u32 {
    width_by_ratio(crop_w, crop_h).clamp(REC_MIN_WIDTH, REC_MAX_WIDTH)
}

/// Preprocesses one crop into a recognizer NCHW plane padded to `target_width`.
///
/// The crop is resized to height [`REC_HEIGHT`] keeping aspect (width capped at
/// `min(target_width, width_by_ratio)`), normalized `(x/255 - 0.5)/0.5` over RGB,
/// and right-zero-padded to `target_width`. Returns a flat `3 * REC_HEIGHT *
/// target_width` channel-major buffer. Matches `preprocess_rec_image_to_width`.
///
/// # Errors
/// [`OrtError::ImagePreprocess`] if the crop is empty or `target_width` is 0.
pub fn preprocess_rec(crop: &RgbaImage, target_width: u32) -> Result<Vec<f32>, OrtError> {
    let (crop_w, crop_h) = crop.dimensions();
    if crop_w == 0 || crop_h == 0 || target_width == 0 {
        return Err(OrtError::ImagePreprocess {
            detail: format!(
                "некорректный кроп {crop_w}x{crop_h} или ширина {target_width} для распознавателя PaddleOCR"
            ),
        });
    }

    // Clamp target width into [REC_MIN_WIDTH, REC_MAX_WIDTH] as Python does before
    // choosing the resized width.
    let target_width = target_width.clamp(REC_MIN_WIDTH, REC_MAX_WIDTH);
    let resized_width = width_by_ratio(crop_w, crop_h).min(target_width).max(1);

    let resized = imageops::resize(crop, resized_width, REC_HEIGHT, imageops::FilterType::Triangle);

    let target_usize = usize::try_from(target_width).map_err(|_| OrtError::ImagePreprocess {
        detail: "ширина распознавателя не помещается в usize".to_owned(),
    })?;
    let height_usize = usize::try_from(REC_HEIGHT).unwrap_or(48);
    let plane = height_usize * target_usize;

    // Zero-initialized so the right pad past `resized_width` stays 0 (Python pads
    // the raw tensor with zeros, i.e. before the -0.5/0.5 normalization is NOT
    // applied to the pad region).
    let mut data = vec![0.0_f32; 3 * plane];
    for (y, row) in resized.rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let [r, g, b, _a] = pixel.0;
            let channels = [r, g, b];
            let offset = y * target_usize + x;
            for (c, &value) in channels.iter().enumerate() {
                let normalized = (f32::from(value) / 255.0 - 0.5) / 0.5;
                data[c * plane + offset] = normalized;
            }
        }
    }

    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_no_upscale_and_stride_multiple() {
        // Small image: no upscaling, but snapped to a stride-32 multiple.
        let (w, h) = resize_dims_for_det(100, 40);
        assert_eq!(w % DET_STRIDE, 0);
        assert_eq!(h % DET_STRIDE, 0);
        // 100 -> round(100/32)*32 = 3*32 = 96; 40 -> round(40/32)*32 = 32.
        assert_eq!(w, 96);
        assert_eq!(h, 32);
    }

    #[test]
    fn resize_downscales_long_side_to_960_multiple() {
        // 4000x2000: ratio = 960/4000 = 0.24 -> 960x480, both already stride-32.
        let (w, h) = resize_dims_for_det(4000, 2000);
        assert!(w <= DET_RESIZE_LONG);
        assert!(h <= DET_RESIZE_LONG);
        assert_eq!(w % DET_STRIDE, 0);
        assert_eq!(h % DET_STRIDE, 0);
        assert_eq!(w, 960);
        assert_eq!(h, 480);
    }

    #[test]
    fn plan_rec_width_clamps_into_bounds() {
        // Very wide crop -> capped at REC_MAX_WIDTH.
        assert_eq!(plan_rec_width(100_000, 48), REC_MAX_WIDTH);
        // Normal crop -> at least REC_MIN_WIDTH.
        assert_eq!(plan_rec_width(50, 48), REC_MIN_WIDTH);
        // Aspect-driven width between the bounds: 48*20/1... use a tall/wide ratio.
        // width_by_ratio(1000, 48) = ceil(48*1000/48) = 1000 -> within [320,3200].
        assert_eq!(plan_rec_width(1000, 48), 1000);
    }

    #[test]
    fn preprocess_rec_shape_and_pad_zeroed() {
        let crop = RgbaImage::from_pixel(60, 20, image::Rgba([255, 255, 255, 255]));
        let target = REC_MIN_WIDTH;
        let data = preprocess_rec(&crop, target).expect("rec preprocess");
        let plane = usize::try_from(REC_HEIGHT).unwrap() * usize::try_from(target).unwrap();
        assert_eq!(data.len(), 3 * plane);
        // White pixel -> (1.0-0.5)/0.5 = 1.0 in the resized region; pad stays 0.
        let wbr = width_by_ratio(60, 20); // ceil(48*60/20) = 144
        let resized_w = wbr.min(target);
        // Column 0 of row 0 is inside the resized region -> ~1.0.
        assert!((data[0] - 1.0).abs() < 1e-4);
        // A column well past resized_w is pad -> exactly 0.0.
        let pad_x = usize::try_from(resized_w).unwrap() + 1;
        assert!((data[pad_x] - 0.0).abs() < 1e-9);
    }
}
