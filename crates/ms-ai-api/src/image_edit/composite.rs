/*
File: crates/ms-ai-api/src/image_edit/composite.rs

Purpose:
The feathered composite of an edited region back onto its source: the provider re-renders the
whole frame (colour drift, shifted line art, watermarks), so only the painted mask, grown by
a dilation and softened by a box blur, takes the edited pixels.

Key functions:
- composite_feathered()

Notes:
alpha = box_blur(dilate_square(mask, dilate), feather) (the `ms-raster` owners of both
primitives); out = src * (255 - alpha) + edited * alpha, rounded, per RGB channel; output alpha
255. A pixel with alpha 0 reproduces the source exactly and alpha 255 the edit exactly, so
pixels farther than `dilate + feather` from paint are bit-identical to the source. An empty
mask (`None` or all zero) means the whole region: the result is the edit.
*/

use super::error::ImageEditError;
use super::request::{MaskBlend, RgbaRegion, pixel_count};

/// Blends `edited` (RGBA8, the size of `source`) into `source` inside `mask` (one byte per
/// pixel, nonzero = editable) and returns RGBA8 bytes of the source size with alpha 255.
///
/// # Errors
/// `ImageEditError::ShapeMismatch` when `edited` or `mask` does not match the source size, or
/// when a blend radius does not fit the raster primitives.
pub fn composite_feathered(source: &RgbaRegion, edited: &[u8], mask: Option<&[u8]>, blend: MaskBlend) -> Result<Vec<u8>, ImageEditError> {
    let (width, height) = (source.width(), source.height());
    let count = pixel_count(width, height)?;
    if edited.len() != source.pixels().len() {
        return Err(ImageEditError::ShapeMismatch { detail: format!("edited RGBA of {width}x{height} must be {} bytes, got {}", source.pixels().len(), edited.len()) });
    }
    let alpha = match mask {
        Some(mask) if mask.len() != count => return Err(ImageEditError::ShapeMismatch { detail: format!("mask of {width}x{height} must be {count} bytes, got {}", mask.len()) }),
        Some(mask) if mask.iter().any(|&value| value != 0) => Some(blend_alpha(mask, width, height, blend)?),
        Some(_) | None => None,
    };
    let mut out = Vec::with_capacity(source.pixels().len());
    for (index, (src, dst)) in source.pixels().chunks_exact(4).zip(edited.chunks_exact(4)).enumerate() {
        let weight = alpha.as_ref().map_or(255, |alpha| alpha[index]);
        for channel in 0..3 {
            out.push(mix(src[channel], dst[channel], weight));
        }
        out.push(255);
    }
    Ok(out)
}

/// The blend alpha of a non-empty mask: dilated by `dilate_px`, then box-blurred by
/// `feather_px`.
fn blend_alpha(mask: &[u8], width: u32, height: u32, blend: MaskBlend) -> Result<Vec<u8>, ImageEditError> {
    let shape = |detail: String| ImageEditError::ShapeMismatch { detail };
    let to_usize = |value: u32| usize::try_from(value).map_err(|_| shape(format!("value {value} does not fit usize")));
    let (w, h) = (to_usize(width)?, to_usize(height)?);
    let dilate = to_usize(blend.dilate_px)?;
    let grown = ms_raster::dilate_square(mask, w, h, dilate, dilate, 255).map_err(|error| shape(error.to_string()))?;
    ms_raster::box_blur_u8(&grown, w, h, to_usize(blend.feather_px)?).map_err(|error| shape(error.to_string()))
}

/// `(src * (255 - weight) + edited * weight) / 255`, rounded half up; exact at 0 and 255.
fn mix(src: u8, edited: u8, weight: u8) -> u8 {
    let weight = u32::from(weight);
    let value = (u32::from(src) * (255 - weight) + u32::from(edited) * weight + 127) / 255;
    // A convex combination of two bytes never exceeds 255.
    u8::try_from(value).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::{composite_feathered, mix};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::request::{MaskBlend, RgbaRegion};

    fn region(width: u32, height: u32, value: u8) -> RgbaRegion {
        RgbaRegion::new(width, height, vec![value; usize::try_from(width * height * 4).unwrap_or(0)]).unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn mix_is_exact_at_the_ends() {
        for src in [0, 1, 128, 254, 255] {
            for edited in [0, 3, 200, 255] {
                assert_eq!(mix(src, edited, 0), src);
                assert_eq!(mix(src, edited, 255), edited);
            }
        }
        assert_eq!(mix(0, 255, 128), 128);
    }

    #[test]
    fn empty_mask_replaces_the_whole_region_with_opaque_alpha() {
        let source = region(3, 2, 10);
        let edited = vec![200; 24];
        for mask in [None, Some(vec![0; 6])] {
            let out = composite_feathered(&source, &edited, mask.as_deref(), MaskBlend::default()).unwrap_or_default();
            assert_eq!(out, [200, 200, 200, 255].repeat(6));
        }
    }

    #[test]
    fn pixels_beyond_dilate_plus_feather_are_untouched() {
        let (width, height) = (32u32, 8u32);
        let source = region(width, height, 10);
        let edited = vec![250; 32 * 8 * 4];
        let mut mask = vec![0u8; 32 * 8];
        mask[3 * 32 + 2] = 255;
        let blend = MaskBlend { dilate_px: 4, feather_px: 2 };
        let out = composite_feathered(&source, &edited, Some(&mask), blend).unwrap_or_default();
        assert_eq!(out.len(), 32 * 8 * 4);
        // Just inside the reach the blend has started.
        assert_ne!(&out[(3 * 32 + 8) * 4..(3 * 32 + 8) * 4 + 4], [10, 10, 10, 255]);
        for y in 0..8usize {
            for x in 0..32usize {
                let pixel = &out[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
                if x > 2 + 4 + 2 {
                    assert_eq!(pixel, [10, 10, 10, 255], "({x},{y})");
                }
            }
        }
        // The painted pixel itself is fully edited: the dilation (4) covers its whole blur window (2).
        assert_eq!(&out[(3 * 32 + 2) * 4..(3 * 32 + 2) * 4 + 4], [250, 250, 250, 255]);
    }

    #[test]
    fn shape_errors() {
        let source = region(2, 2, 0);
        assert!(matches!(composite_feathered(&source, &[0; 15], None, MaskBlend::default()), Err(ImageEditError::ShapeMismatch { .. })));
        assert!(matches!(composite_feathered(&source, &[0; 16], Some(&[1; 3]), MaskBlend::default()), Err(ImageEditError::ShapeMismatch { .. })));
    }
}
