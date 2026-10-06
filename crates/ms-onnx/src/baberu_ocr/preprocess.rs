/*
File: crates/ms-onnx/src/baberu_ocr/preprocess.rs

Purpose:
Image -> `pixel_values` for the Baberu OCR vision model, bit-exact with the upstream
reference `img.convert("RGB").resize((224, 224), Image.BICUBIC)` followed by
`(x / 255 - mean) / std` in float32, CHW layout.

Key functions:
- preprocess             : RGBA crop -> CHW f32 `[3 * 224 * 224]` (the full pipeline).
- rgba_to_rgb            : PIL `convert("RGB")` from RGBA (alpha dropped, not composited).
- resize_bicubic_pillow  : integer port of Pillow's `Resample.c` BICUBIC 8bpc resample.
- pixel_values           : ImageNet normalization of a 224x224 RGB image, CHW f32.

Notes:
The resize is a port of Pillow's algorithm, not a generic bicubic: coefficients in f64
(`a = -0.5`, support `2 * max(scale, 1)`, normalized by their sum), quantized to
22-bit fixed point with C `(int)(k +- 0.5)` rounding; horizontal pass first, u8
clamp between passes, a pass skipped when its dimension is unchanged, and Pillow's
`Image.resize` special case of a vertical-first order for images over 100x taller than
wide that shrink vertically. The `image`
crate's CatmullRom differs from Pillow by up to 22/255 per pixel, which changes OCR
output, so it must not be substituted. Parity is pinned by the golden cases in
`fixtures/baberu/resize_cases.json` (generator: `tools/make_baberu_fixtures.py`).
*/

use image::{RgbImage, RgbaImage};

use crate::OrtError;

/// Side of the square image the Baberu vision model takes (`pixel_values [1,3,224,224]`).
pub(crate) const BABERU_IMAGE_SIDE: u32 = 224;

/// Channels of `pixel_values` (RGB).
const CHANNELS: usize = 3;

/// Fixed-point precision of Pillow's 8bpc resample (`PRECISION_BITS = 32 - 8 - 2`).
const PRECISION_BITS: u32 = 22;

/// ImageNet mean, per RGB channel (reference `_MEAN`).
const MEAN: [f32; CHANNELS] = [0.485, 0.456, 0.406];

/// ImageNet standard deviation, per RGB channel (reference `_STD`).
const STD: [f32; CHANNELS] = [0.229, 0.224, 0.225];

/// Prepares one bubble crop for the vision model: drop alpha, Pillow-BICUBIC resize of
/// the whole crop to 224x224 (no aspect preservation, no crop), normalize.
///
/// Returns the CHW f32 tensor data of length `3 * 224 * 224`.
///
/// # Errors
/// [`OrtError::BaberuPreprocess`] if the image is empty.
pub(crate) fn preprocess(image: &RgbaImage) -> Result<Vec<f32>, OrtError> {
    let rgb = rgba_to_rgb(image);
    let resized = resize_bicubic_pillow(&rgb, BABERU_IMAGE_SIDE, BABERU_IMAGE_SIDE)?;
    pixel_values(&resized)
}

/// PIL `convert("RGB")` of an RGBA image: the alpha channel is DROPPED (not composited
/// over any background), exactly as Pillow does it.
#[must_use]
pub(crate) fn rgba_to_rgb(image: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b, _alpha] = image.get_pixel(x, y).0;
        image::Rgb([r, g, b])
    })
}

/// Pillow's BICUBIC kernel (`a = -0.5`), `bicubic_filter` in `Resample.c`, with the same
/// operation order so the f64 results match bit for bit.
fn bicubic_filter(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

/// C `(int)` cast of a finite value: truncation toward zero.
///
/// Every caller passes a value bounded by the image size plus the filter support, or a
/// filter weight times `2^22`; the range check turns an impossible out-of-range value
/// into a typed error instead of a saturating cast.
fn c_int_cast(value: f64) -> Result<i64, OrtError> {
    if !value.is_finite() || value.abs() >= f64::from(i32::MAX) {
        return Err(OrtError::BaberuPreprocess {
            detail: format!("значение вне диапазона при расчёте коэффициентов ресайза: {value}"),
        });
    }
    // Checked above: finite and within i32, so the cast is exact truncation toward zero,
    // which is precisely C's `(int)` semantics that Pillow's coefficient code relies on.
    #[expect(clippy::cast_possible_truncation, reason = "range checked above; truncation is the C (int) semantics being ported")]
    Ok(value as i64)
}

/// Fixed-point taps of one output sample: the first input index and its weights.
#[derive(Debug)]
struct Taps {
    /// First input index the weights apply to (`xmin` in `Resample.c`).
    start: usize,
    /// 22-bit fixed-point weights for inputs `start..start + weights.len()`.
    weights: Vec<i64>,
}

/// Pillow `precompute_coeffs` + `normalize_coeffs_8bpc` for BICUBIC, one axis.
///
/// `in_size` / `out_size` are both non-zero (checked by the caller).
fn precompute_taps(in_size: u32, out_size: u32) -> Result<Vec<Taps>, OrtError> {
    let in_len = usize::try_from(in_size).map_err(|_| size_error(in_size))?;
    let scale = f64::from(in_size) / f64::from(out_size);
    let filter_scale = if scale < 1.0 { 1.0 } else { scale };
    // BICUBIC support is 2.0, widened by the downscale factor.
    let support = 2.0 * filter_scale;
    let inv_scale = 1.0 / filter_scale;
    let fixed_one = f64::from(1_u32 << PRECISION_BITS);

    let mut taps = Vec::with_capacity(usize::try_from(out_size).map_err(|_| size_error(out_size))?);
    for xx in 0..out_size {
        let center = (f64::from(xx) + 0.5) * scale;
        // `(int)(center -/+ support + 0.5)`, clamped to the input, exactly as Resample.c.
        let xmin = c_int_cast(center - support + 0.5)?.max(0);
        let xmax = c_int_cast(center + support + 0.5)?.min(i64::from(in_size));
        let start = usize::try_from(xmin).map_err(|_| size_error(in_size))?;
        let end = usize::try_from(xmax).map_err(|_| size_error(in_size))?.min(in_len);

        let mut weights_f: Vec<f64> = Vec::with_capacity(end.saturating_sub(start));
        let mut sum = 0.0_f64;
        for input in start..end {
            // `(x + xmin - center + 0.5) * ss`: the integer index first, then the f64 math.
            let input_f = f64::from(u32::try_from(input).map_err(|_| size_error(in_size))?);
            let w = bicubic_filter((input_f - center + 0.5) * inv_scale);
            weights_f.push(w);
            sum += w;
        }
        let mut weights = Vec::with_capacity(weights_f.len());
        for w in weights_f {
            let normalized = if sum == 0.0 { w } else { w / sum };
            // `normalize_coeffs_8bpc`: round half away from zero via C truncation.
            let fixed = if normalized < 0.0 {
                c_int_cast(-0.5 + normalized * fixed_one)?
            } else {
                c_int_cast(0.5 + normalized * fixed_one)?
            };
            weights.push(fixed);
        }
        taps.push(Taps { start, weights });
    }
    Ok(taps)
}

/// The typed error for a dimension that does not fit the index math.
fn size_error(size: u32) -> OrtError {
    OrtError::BaberuPreprocess {
        detail: format!("размер изображения {size} не помещается в индексную арифметику"),
    }
}

/// Pillow `clip8`: `acc >> 22` clamped to `0..=255`.
fn clip8(acc: i64) -> u8 {
    // Clamped to 0..=255 first, so the conversion can never fail; `u8::MAX` is unreachable.
    u8::try_from((acc >> PRECISION_BITS).clamp(0, 255)).unwrap_or(u8::MAX)
}

/// One fixed-point pass over interleaved RGB bytes along one axis.
///
/// `lines` is the number of lines along the OTHER axis (rows for the horizontal pass,
/// columns for the vertical one); `in_index(line, pos)` / `out_index(line, pos)` give the
/// pixel index (not byte index) of position `pos` on `line` in the input / output
/// buffer, and `out_pixels` is the output pixel count. Accumulates from `1 << 21` (the
/// rounding half) in i64, which cannot overflow: |sum| <= 255 * sum|weights| < 2^31.
fn resample_pass(
    src: &[u8],
    taps: &[Taps],
    lines: usize,
    out_index: impl Fn(usize, usize) -> usize,
    in_index: impl Fn(usize, usize) -> usize,
    out_pixels: usize,
) -> Result<Vec<u8>, OrtError> {
    let out_bytes = out_pixels.checked_mul(CHANNELS).ok_or_else(|| OrtError::BaberuPreprocess {
        detail: "размер буфера ресайза переполняет usize".to_owned(),
    })?;
    let mut dst = vec![0_u8; out_bytes];
    for line in 0..lines {
        for (out_pos, tap) in taps.iter().enumerate() {
            let mut acc = [1_i64 << (PRECISION_BITS - 1); CHANNELS];
            for (offset, &weight) in tap.weights.iter().enumerate() {
                let base = in_index(line, tap.start + offset) * CHANNELS;
                let px = src.get(base..base + CHANNELS).ok_or_else(|| OrtError::BaberuPreprocess {
                    detail: "индекс ресайза вне буфера".to_owned(),
                })?;
                for (sum, &channel) in acc.iter_mut().zip(px) {
                    *sum += i64::from(channel) * weight;
                }
            }
            let base = out_index(line, out_pos) * CHANNELS;
            let out = dst.get_mut(base..base + CHANNELS).ok_or_else(|| OrtError::BaberuPreprocess {
                detail: "индекс результата ресайза вне буфера".to_owned(),
            })?;
            for (slot, sum) in out.iter_mut().zip(acc) {
                *slot = clip8(sum);
            }
        }
    }
    Ok(dst)
}

/// Pillow's `Image.resize` takes the vertical pass FIRST (as two separate resizes) for an
/// image more than 100 times taller than wide that shrinks vertically
/// (`size[1] > size[0] * 100 and new_h < size[1]`), and horizontal first otherwise.
/// The pass order changes the u8 rounding between passes, so it is part of parity.
const VERTICAL_FIRST_ASPECT: u64 = 100;

/// Resizes `src` to `out_w` x `out_h` exactly like Pillow's
/// `Image.resize((out_w, out_h), Image.BICUBIC)` on an 8-bit RGB image.
///
/// Horizontal pass first, then vertical (vertical first for the extreme-tall case of
/// [`VERTICAL_FIRST_ASPECT`]), with u8 rounding/clamping between them; a pass whose
/// dimension is unchanged is skipped (so an equal size returns a copy).
///
/// # Errors
/// [`OrtError::BaberuPreprocess`] if any input or output dimension is zero or a buffer
/// size overflows.
pub(crate) fn resize_bicubic_pillow(
    src: &RgbImage,
    out_w: u32,
    out_h: u32,
) -> Result<RgbImage, OrtError> {
    let (in_w, in_h) = src.dimensions();
    if in_w == 0 || in_h == 0 || out_w == 0 || out_h == 0 {
        return Err(OrtError::BaberuPreprocess {
            detail: format!("пустой размер ресайза {in_w}x{in_h} -> {out_w}x{out_h}"),
        });
    }
    let image = Raster { data: src.as_raw().clone(), cols: in_w, rows: in_h };
    let vertical_first = u64::from(in_h) > u64::from(in_w) * VERTICAL_FIRST_ASPECT && out_h < in_h;
    let image = if vertical_first {
        resize_cols(resize_rows(image, out_h)?, out_w)?
    } else {
        resize_rows(resize_cols(image, out_w)?, out_h)?
    };
    RgbImage::from_raw(out_w, out_h, image.data).ok_or_else(|| OrtError::BaberuPreprocess {
        detail: "длина буфера ресайза не совпадает с размером".to_owned(),
    })
}

/// An interleaved RGB buffer between two resample passes.
#[derive(Debug)]
struct Raster {
    /// Row-major RGB bytes, `cols * rows * 3` long.
    data: Vec<u8>,
    /// Width in pixels.
    cols: u32,
    /// Height in pixels.
    rows: u32,
}

/// Converts a dimension for index math.
fn dim(value: u32) -> Result<usize, OrtError> {
    usize::try_from(value).map_err(|_| size_error(value))
}

/// `a * b` pixels, or a typed overflow error.
fn pixel_count(a: usize, b: usize) -> Result<usize, OrtError> {
    a.checked_mul(b).ok_or_else(|| OrtError::BaberuPreprocess {
        detail: "размер буфера ресайза переполняет usize".to_owned(),
    })
}

/// Horizontal pass to `out_cols` columns (skipped when the width is unchanged).
fn resize_cols(image: Raster, out_cols: u32) -> Result<Raster, OrtError> {
    if out_cols == image.cols {
        return Ok(image);
    }
    let taps = precompute_taps(image.cols, out_cols)?;
    let (src_cols, dst_cols, rows) = (dim(image.cols)?, dim(out_cols)?, dim(image.rows)?);
    let data = resample_pass(
        &image.data,
        &taps,
        rows,
        |row, x| row * dst_cols + x,
        |row, x| row * src_cols + x,
        pixel_count(dst_cols, rows)?,
    )?;
    Ok(Raster { data, cols: out_cols, rows: image.rows })
}

/// Vertical pass to `out_rows` rows (skipped when the height is unchanged).
fn resize_rows(image: Raster, out_rows: u32) -> Result<Raster, OrtError> {
    if out_rows == image.rows {
        return Ok(image);
    }
    let taps = precompute_taps(image.rows, out_rows)?;
    let cols = dim(image.cols)?;
    let data = resample_pass(
        &image.data,
        &taps,
        cols,
        |col, y| y * cols + col,
        |col, y| y * cols + col,
        pixel_count(cols, dim(out_rows)?)?,
    )?;
    Ok(Raster { data, cols: image.cols, rows: out_rows })
}

/// ImageNet normalization of a 224x224 RGB image into CHW f32:
/// `(f32(px) / 255 - mean) / std`, every step in f32 like the NumPy reference.
///
/// # Errors
/// [`OrtError::BaberuPreprocess`] if `img` is not 224x224.
pub(crate) fn pixel_values(img: &RgbImage) -> Result<Vec<f32>, OrtError> {
    if img.dimensions() != (BABERU_IMAGE_SIDE, BABERU_IMAGE_SIDE) {
        return Err(OrtError::BaberuPreprocess {
            detail: format!(
                "ожидалось изображение {BABERU_IMAGE_SIDE}x{BABERU_IMAGE_SIDE}, получено {}x{}",
                img.width(),
                img.height()
            ),
        });
    }
    let plane = img.as_raw().len() / CHANNELS;
    let mut out = vec![0.0_f32; img.as_raw().len()];
    for (i, px) in img.as_raw().chunks_exact(CHANNELS).enumerate() {
        for (c, &value) in px.iter().enumerate() {
            out[c * plane + i] = (f32::from(value) / 255.0 - MEAN[c]) / STD[c];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// The documented xorshift32 stream of `tools/make_baberu_fixtures.py`: each step
    /// `x ^= x << 13; x ^= x >> 17; x ^= x << 5`, byte = new state `>> 24`.
    fn xorshift32_bytes(seed: u32, count: usize) -> Vec<u8> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_be_bytes()[0]
            })
            .collect()
    }

    fn noise(seed: u32, w: u32, h: u32) -> RgbImage {
        let len = usize::try_from(w * h * 3).unwrap_or(0);
        RgbImage::from_raw(w, h, xorshift32_bytes(seed, len)).unwrap_or_default()
    }

    #[test]
    fn matches_pillow_bicubic_golden_cases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/baberu/resize_cases.json"))
                .unwrap_or(serde_json::Value::Null);
        let cases = fixture["cases"].as_array().cloned().unwrap_or_default();
        assert!(cases.len() >= 10, "fixture must list the golden resize cases");
        for case in cases {
            let field = |k: &str| u32::try_from(case[k].as_u64().unwrap_or(0)).unwrap_or(0);
            let name = case["name"].as_str().unwrap_or("?");
            let src = noise(field("seed"), field("in_w"), field("in_h"));
            assert_eq!(src.width(), field("in_w"), "{name}: input build");
            let out = resize_bicubic_pillow(&src, field("out_w"), field("out_h"));
            let Ok(out) = out else {
                panic!("{name}: resize failed: {out:?}");
            };
            assert_eq!(out.dimensions(), (field("out_w"), field("out_h")), "{name}");
            let hex = format!("{:x}", Sha256::digest(out.as_raw()));
            assert_eq!(hex, case["sha256"].as_str().unwrap_or(""), "{name}: differs from Pillow");
        }
    }

    #[test]
    fn identity_size_is_a_copy() {
        let src = noise(42, 9, 4);
        let out = resize_bicubic_pillow(&src, 9, 4).ok();
        assert_eq!(out.as_ref().map(RgbImage::as_raw), Some(src.as_raw()));
    }

    #[test]
    fn constant_image_stays_constant_up_and_down() {
        // Normalized weights sum to 2^22 (+- rounding), so a flat field must survive
        // both upscaling and downscaling, including 1xN / Nx1 inputs.
        for (w, h, ow, oh) in [(3, 5, 224, 224), (400, 90, 224, 224), (1, 30, 7, 224), (30, 1, 224, 3)] {
            let src = RgbImage::from_pixel(w, h, image::Rgb([200, 17, 99]));
            let out = resize_bicubic_pillow(&src, ow, oh);
            let Ok(out) = out else {
                panic!("{w}x{h}: {out:?}");
            };
            assert!(out.pixels().all(|p| p.0 == [200, 17, 99]), "{w}x{h} -> {ow}x{oh}");
        }
    }

    #[test]
    fn zero_sizes_are_typed_errors() {
        let src = noise(1, 4, 4);
        assert!(matches!(resize_bicubic_pillow(&src, 0, 4), Err(OrtError::BaberuPreprocess { .. })));
        let empty = RgbImage::new(0, 3);
        assert!(matches!(resize_bicubic_pillow(&empty, 4, 4), Err(OrtError::BaberuPreprocess { .. })));
        assert!(matches!(
            preprocess(&RgbaImage::new(0, 0)),
            Err(OrtError::BaberuPreprocess { .. })
        ));
    }

    #[test]
    fn rgba_conversion_drops_alpha() {
        let rgba = RgbaImage::from_pixel(2, 1, image::Rgba([10, 20, 30, 0]));
        assert_eq!(rgba_to_rgb(&rgba).get_pixel(1, 0).0, [10, 20, 30]);
    }

    #[test]
    fn pixel_values_is_chw_normalized_f32() {
        let mut img = RgbImage::from_pixel(224, 224, image::Rgb([0, 128, 255]));
        img.put_pixel(1, 0, image::Rgb([255, 0, 51]));
        let Ok(values) = pixel_values(&img) else {
            panic!("224x224 must be accepted");
        };
        let plane = 224 * 224;
        assert_eq!(values.len(), 3 * plane);
        // Same f32 operation order as NumPy: (x / 255 - mean) / std.
        assert_eq!(values[0].to_bits(), ((0.0_f32 / 255.0 - 0.485) / 0.229).to_bits());
        assert_eq!(values[plane].to_bits(), ((128.0_f32 / 255.0 - 0.456) / 0.224).to_bits());
        assert_eq!(values[2 * plane].to_bits(), ((255.0_f32 / 255.0 - 0.406) / 0.225).to_bits());
        assert_eq!(values[1].to_bits(), ((255.0_f32 / 255.0 - 0.485) / 0.229).to_bits());
        assert_eq!(values[2 * plane + 1].to_bits(), ((51.0_f32 / 255.0 - 0.406) / 0.225).to_bits());
        assert!(matches!(pixel_values(&RgbImage::new(223, 224)), Err(OrtError::BaberuPreprocess { .. })));
    }
}
