/*
File: src/tabs/typing/render_next/effects/stroke_shadow.rs

Purpose:
Contour-based stroke/shadow эффекты нового рендера typing.

Main responsibilities:
- строить обводку поверх alpha-контура текста;
- рендерить отдельный shadow-layer с optional blur и source-color mode;
- переиспользовать общий image helper-слой без привязки к центральному pipeline.

Notes:
The stroke layer always lies UNDER the source and is composited by inverting the desired total
alpha (`image_ops::required_under_alpha_for_total_alpha`), never by pre-multiplying the layer by
the source coverage. In the `Static` opacity mode the layer's own alpha is constant across the
whole dilated shape, so it is the source COVERAGE — the buffer alpha normalized by
`image_ops::source_peak_alpha`, not the raw buffer alpha — that decides how much of it survives
under the glyph; see `static_stroke_desired_total_alpha`. `Shadow` has no opacity mode and is
unaffected.
*/

use super::super::raster::blend_pixel_over;
use super::super::types::RenderedTextImage;
use super::image_ops::{
    blend_full_image_over, gaussian_blur_alpha_in_place, gaussian_blur_rgba_in_place,
    required_under_alpha_for_total_alpha, source_peak_alpha,
};
use super::parse::{ShadowEffectParams, StrokeEffectParams, StrokeOpacityMode};
use rayon::prelude::*;

/// Draws a contour stroke under the source text and composites the source back over it.
///
/// Dilates the source alpha with a round kernel of `width_px`, optionally smooths the resulting
/// layer, and composites it under the source. `FromContour` scales the layer by the source alpha;
/// `Static` gives it one constant alpha and then lets only the part the contour does not already
/// cover through (`static_stroke_desired_total_alpha`), so translucent text keeps its
/// transparency instead of being backed by an opaque plate.
///
/// Returns without touching the image for a non-positive width, an empty image, or an empty
/// kernel. The canvas is never resized: the stroke is clipped to the existing buffer.
pub(crate) fn apply_stroke_effect(image: &mut RenderedTextImage, stroke: &StrokeEffectParams) {
    let width_px = stroke.width_px;
    if width_px <= 0.0 {
        return;
    }
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let radius = width_px.ceil().max(1.0);
    let radius_i = radius as i32;
    let kernel_radius = radius + 0.5;
    let mut kernel = Vec::<(i32, i32, u8)>::new();
    for oy in -radius_i..=radius_i {
        for ox in -radius_i..=radius_i {
            let dist = ((ox * ox + oy * oy) as f32).sqrt();
            let coverage = (kernel_radius - dist).clamp(0.0, 1.0);
            if coverage <= f32::EPSILON {
                continue;
            }
            let alpha = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
            if alpha > 0 {
                kernel.push((ox, oy, alpha));
            }
        }
    }
    if kernel.is_empty() {
        return;
    }

    let mut stroke_alpha = vec![0u8; width * height];
    let source = image.rgba.clone();
    let mut source_alpha = vec![0u8; width * height];
    // Rescales a buffer alpha into "the alpha this pixel would have had with an opaque
    // source", which is the coverage the static layer has to respect. Zero for an empty
    // buffer: there is no coverage information then, and no stroke is drawn anyway.
    let peak_a = source_peak_alpha(&source);
    let peak_scale = if peak_a == 0 {
        0.0
    } else {
        255.0 / peak_a as f32
    };
    let static_opacity =
        (1.0 - stroke.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let static_alpha = (static_opacity * 255.0).round().clamp(0.0, 255.0) as u8;
    let static_tinted_alpha = ((static_alpha as u16 * stroke.color[3] as u16) / 255) as u8;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            source_alpha[y * width + x] = src_a;
            if src_a == 0 {
                continue;
            }

            for (ox, oy, kernel_alpha) in kernel.iter().copied() {
                let tx = x as i32 + ox;
                let ty = y as i32 + oy;
                if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                    continue;
                }
                let tidx = ty as usize * width + tx as usize;
                let blended = match stroke.opacity_mode {
                    StrokeOpacityMode::FromContour => {
                        ((src_a as u16 * kernel_alpha as u16) / 255) as u8
                    }
                    StrokeOpacityMode::Static => kernel_alpha,
                };
                stroke_alpha[tidx] = stroke_alpha[tidx].max(blended);
            }
        }
    }

    if stroke.smoothing_enabled {
        let smoothing_factor = (stroke.smoothing_strength_percent / 100.0).clamp(0.0, 1.0);
        let sigma = ((width_px * 0.35 + 0.35) * smoothing_factor).clamp(0.0, 1.6);
        if sigma > f32::EPSILON {
            gaussian_blur_alpha_in_place(&mut stroke_alpha, image.width, image.height, sigma);
            for idx in 0..stroke_alpha.len() {
                stroke_alpha[idx] = stroke_alpha[idx].max(source_alpha[idx]);
            }
        }
    }

    let mut out = vec![0u8; source.len()];
    // Each output pixel composites stroke-under-source using only the read-only
    // `source`, `source_alpha`, and `stroke_alpha` at its own index, so the final
    // compositing pass is parallelized per pixel with no shared mutable state.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let rgba_idx = idx * 4;
        let src_a = source_alpha[idx];
        let desired_total_a = match stroke.opacity_mode {
            StrokeOpacityMode::FromContour => {
                f32::from(((stroke_alpha[idx] as u16 * stroke.color[3] as u16) / 255) as u8)
            }
            StrokeOpacityMode::Static => {
                let stroke_target_a =
                    ((stroke_alpha[idx] as u16 * static_tinted_alpha as u16) / 255) as u8;
                static_stroke_desired_total_alpha(stroke_target_a, src_a, peak_scale)
            }
        };
        let stroke_out_a = required_under_alpha_for_total_alpha(desired_total_a, src_a);
        if stroke_out_a > 0 {
            blend_pixel_over(
                dst,
                stroke.color[0],
                stroke.color[1],
                stroke.color[2],
                stroke_out_a,
            );
        }
        blend_pixel_over(
            dst,
            source[rgba_idx],
            source[rgba_idx + 1],
            source[rgba_idx + 2],
            source[rgba_idx + 3],
        );
    });

    image.rgba = out;
}

pub(crate) fn apply_shadow_effect(image: &mut RenderedTextImage, shadow: &ShadowEffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let shadow_opacity =
        (1.0 - shadow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    if shadow_opacity <= f32::EPSILON {
        return;
    }

    let blur_pad = (shadow.blur_radius_px.max(0.0) * 3.0).ceil() as u32;
    let left_pad = ((-shadow.offset_x).max(0) as u32).saturating_add(blur_pad);
    let right_pad = (shadow.offset_x.max(0) as u32).saturating_add(blur_pad);
    let top_pad = ((-shadow.offset_y).max(0) as u32).saturating_add(blur_pad);
    let bottom_pad = (shadow.offset_y.max(0) as u32).saturating_add(blur_pad);

    let out_width = image
        .width
        .saturating_add(left_pad)
        .saturating_add(right_pad);
    let out_height = image
        .height
        .saturating_add(top_pad)
        .saturating_add(bottom_pad);
    if out_width == 0 || out_height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut shadow_layer = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let source_origin_x = left_pad as i32;
    let source_origin_y = top_pad as i32;
    let shadow_origin_x = source_origin_x + shadow.offset_x;
    let shadow_origin_y = source_origin_y + shadow.offset_y;
    let solid_alpha_factor = shadow.color[3] as f32 / 255.0;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let dst_x = shadow_origin_x + x as i32;
            let dst_y = shadow_origin_y + y as i32;
            if dst_x < 0 || dst_y < 0 || dst_x >= out_width as i32 || dst_y >= out_height as i32 {
                continue;
            }

            let (shadow_r, shadow_g, shadow_b, color_alpha_factor) = if shadow.use_source_color {
                (
                    source[src_idx],
                    source[src_idx + 1],
                    source[src_idx + 2],
                    1.0,
                )
            } else {
                (
                    shadow.color[0],
                    shadow.color[1],
                    shadow.color[2],
                    solid_alpha_factor,
                )
            };
            let shadow_a = ((src_a as f32) * shadow_opacity * color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if shadow_a == 0 {
                continue;
            }

            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut shadow_layer[dst_idx..dst_idx + 4],
                shadow_r,
                shadow_g,
                shadow_b,
                shadow_a,
            );
        }
    }

    if shadow.blur_radius_px > f32::EPSILON {
        gaussian_blur_rgba_in_place(
            &mut shadow_layer,
            out_width,
            out_height,
            shadow.blur_radius_px,
        );
    }

    blend_full_image_over(&mut out, shadow_layer.as_slice());

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = source_origin_x + x as i32;
            let dst_y = source_origin_y + y as i32;
            if dst_x < 0 || dst_y < 0 || dst_x >= out_width as i32 || dst_y >= out_height as i32 {
                continue;
            }

            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut out[dst_idx..dst_idx + 4],
                source[src_idx],
                source[src_idx + 1],
                source[src_idx + 2],
                src_a,
            );
        }
    }

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    // Исходный контент сдвинут на (left_pad, top_pad) внутри увеличенного буфера.
    image.content_origin_x = image.content_origin_x.saturating_add(left_pad);
    image.content_origin_y = image.content_origin_y.saturating_add(top_pad);
}

/// Total alpha a `Static` stroke pixel must end up with, given the stroke layer's own target
/// alpha, the source alpha at that pixel, and `peak_scale = 255 / peak_a` from
/// [`source_peak_alpha`](super::image_ops::source_peak_alpha).
///
/// `src_a * peak_scale` is the alpha the source would have had if it were opaque, i.e. the
/// contour coverage the stroke does not need to paint. Only the EXCESS of the stroke's target
/// over that coverage is added, on top of the source's own alpha, so a translucent source keeps
/// its transparency instead of being backed by an opaque plate. Returns an alpha in `[0, 255]`,
/// never below `src_a`.
///
/// At `peak_scale == 1.0` (an opaque pixel exists somewhere in the source) this is exactly
/// `max(stroke_target_a, src_a)` — the behavior every persisted stroke effect was authored
/// against — and reproduces it bit for bit.
#[must_use]
fn static_stroke_desired_total_alpha(stroke_target_a: u8, src_a: u8, peak_scale: f32) -> f32 {
    let src_norm = src_a as f32 * peak_scale;
    let layer_excess = (stroke_target_a as f32 - src_norm).max(0.0);
    (src_a as f32 + layer_excess).min(255.0)
}

#[cfg(test)]
mod tests {
    use super::super::image_ops::rgba_digest;
    use super::super::parse::{StrokeEffectParams, StrokeOpacityMode};
    use super::{apply_stroke_effect, static_stroke_desired_total_alpha};
    use crate::types::RenderedTextImage;

    fn sample_glyph_image() -> RenderedTextImage {
        let width = 21usize;
        let height = 15usize;
        let mut rgba = vec![0u8; width * height * 4];
        for y in 4..11 {
            for x in 5..16 {
                let idx = (y * width + x) * 4;
                rgba[idx] = 200;
                rgba[idx + 1] = 40;
                rgba[idx + 2] = 90;
                rgba[idx + 3] = if (x + y) % 2 == 0 { 255 } else { 160 };
            }
        }
        RenderedTextImage {
            width: width as u32,
            height: height as u32,
            rgba,
            warnings: Vec::new(),
            content_origin_x: 0,
            content_origin_y: 0,
            extra: crate::types::RenderedTextExtraInfo::default(),
            font_fallbacks: crate::types::FontFallbackReport::default(),
        }
    }

    /// Verbatim sequential reference of `apply_stroke_effect`: identical body with the final
    /// `par_chunks_mut(4).for_each(...)` composite pass replaced by a plain per-pixel loop.
    /// Asserts the rayon path is bit-identical to the pre-parallelization loop (a stronger
    /// oracle than running the same code inside a single-thread rayon pool).
    fn apply_stroke_effect_seq(image: &mut RenderedTextImage, stroke: &StrokeEffectParams) {
        use super::super::super::raster::blend_pixel_over;
        use super::super::image_ops::{
            gaussian_blur_alpha_in_place, required_under_alpha_for_total_alpha, source_peak_alpha,
        };
        use super::static_stroke_desired_total_alpha;

        let width_px = stroke.width_px;
        if width_px <= 0.0 {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }

        let radius = width_px.ceil().max(1.0);
        let radius_i = radius as i32;
        let kernel_radius = radius + 0.5;
        let mut kernel = Vec::<(i32, i32, u8)>::new();
        for oy in -radius_i..=radius_i {
            for ox in -radius_i..=radius_i {
                let dist = ((ox * ox + oy * oy) as f32).sqrt();
                let coverage = (kernel_radius - dist).clamp(0.0, 1.0);
                if coverage <= f32::EPSILON {
                    continue;
                }
                let alpha = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
                if alpha > 0 {
                    kernel.push((ox, oy, alpha));
                }
            }
        }
        if kernel.is_empty() {
            return;
        }

        let mut stroke_alpha = vec![0u8; width * height];
        let source = image.rgba.clone();
        let mut source_alpha = vec![0u8; width * height];
        // Rescales a buffer alpha into "the alpha this pixel would have had with an opaque
        // source", which is the coverage the static layer has to respect. Zero for an empty
        // buffer: there is no coverage information then, and no stroke is drawn anyway.
        let peak_a = source_peak_alpha(&source);
        let peak_scale = if peak_a == 0 {
            0.0
        } else {
            255.0 / peak_a as f32
        };
        let static_opacity =
            (1.0 - stroke.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
        let static_alpha = (static_opacity * 255.0).round().clamp(0.0, 255.0) as u8;
        let static_tinted_alpha = ((static_alpha as u16 * stroke.color[3] as u16) / 255) as u8;

        for y in 0..height {
            for x in 0..width {
                let src_idx = (y * width + x) * 4;
                let src_a = source[src_idx + 3];
                source_alpha[y * width + x] = src_a;
                if src_a == 0 {
                    continue;
                }

                for (ox, oy, kernel_alpha) in kernel.iter().copied() {
                    let tx = x as i32 + ox;
                    let ty = y as i32 + oy;
                    if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                        continue;
                    }
                    let tidx = ty as usize * width + tx as usize;
                    let blended = match stroke.opacity_mode {
                        StrokeOpacityMode::FromContour => {
                            ((src_a as u16 * kernel_alpha as u16) / 255) as u8
                        }
                        StrokeOpacityMode::Static => kernel_alpha,
                    };
                    stroke_alpha[tidx] = stroke_alpha[tidx].max(blended);
                }
            }
        }

        if stroke.smoothing_enabled {
            let smoothing_factor = (stroke.smoothing_strength_percent / 100.0).clamp(0.0, 1.0);
            let sigma = ((width_px * 0.35 + 0.35) * smoothing_factor).clamp(0.0, 1.6);
            if sigma > f32::EPSILON {
                gaussian_blur_alpha_in_place(&mut stroke_alpha, image.width, image.height, sigma);
                for idx in 0..stroke_alpha.len() {
                    stroke_alpha[idx] = stroke_alpha[idx].max(source_alpha[idx]);
                }
            }
        }

        let mut out = vec![0u8; source.len()];
        for (idx, dst) in out.chunks_mut(4).enumerate() {
            let rgba_idx = idx * 4;
            let src_a = source_alpha[idx];
            let desired_total_a = match stroke.opacity_mode {
                StrokeOpacityMode::FromContour => {
                    f32::from(((stroke_alpha[idx] as u16 * stroke.color[3] as u16) / 255) as u8)
                }
                StrokeOpacityMode::Static => {
                    let stroke_target_a =
                        ((stroke_alpha[idx] as u16 * static_tinted_alpha as u16) / 255) as u8;
                    static_stroke_desired_total_alpha(stroke_target_a, src_a, peak_scale)
                }
            };
            let stroke_out_a = required_under_alpha_for_total_alpha(desired_total_a, src_a);
            if stroke_out_a > 0 {
                blend_pixel_over(
                    dst,
                    stroke.color[0],
                    stroke.color[1],
                    stroke.color[2],
                    stroke_out_a,
                );
            }
            blend_pixel_over(
                dst,
                source[rgba_idx],
                source[rgba_idx + 1],
                source[rgba_idx + 2],
                source[rgba_idx + 3],
            );
        }

        image.rgba = out;
    }

    /// Translucent counterpart of `sample_glyph_image`: the same block with NO fully opaque
    /// pixel, so `peak_a` is 128 and the static coverage normalization actually does something.
    ///
    /// At `peak_a == 255` `static_stroke_desired_total_alpha` degenerates to the legacy
    /// `max(target, src_a)`, so an opaque source cannot exercise the new branch — in the
    /// sequential mirror least of all, which is what this image exists for.
    fn translucent_glyph_image() -> RenderedTextImage {
        let mut image = sample_glyph_image();
        for pixel in image.rgba.chunks_exact_mut(4) {
            // Two coverage levels under one peak: the opaque body becomes 128, the 160 rim 96.
            pixel[3] = match pixel[3] {
                255 => 128,
                160 => 96,
                other => other,
            };
        }
        image
    }

    /// Bit-identity of the parallel and sequential composites on the STATIC branch of a
    /// TRANSLUCENT source. The test below covers `Static` too, but only at `peak_a == 255`,
    /// where the formula collapses to the legacy one and the new branch is never taken.
    #[test]
    fn stroke_static_parallel_composite_matches_sequential_on_a_translucent_source() {
        let stroke = StrokeEffectParams {
            width_px: 2.5,
            color: [0, 0, 0, 220],
            opacity_mode: StrokeOpacityMode::Static,
            transparency_percent: 15.0,
            smoothing_enabled: true,
            smoothing_strength_percent: 60.0,
        };
        let mut parallel = translucent_glyph_image();
        let mut sequential = translucent_glyph_image();
        apply_stroke_effect(&mut parallel, &stroke);
        apply_stroke_effect_seq(&mut sequential, &stroke);
        assert_eq!(parallel.width, sequential.width);
        assert_eq!(parallel.height, sequential.height);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn stroke_parallel_composite_matches_sequential() {
        for opacity_mode in [StrokeOpacityMode::FromContour, StrokeOpacityMode::Static] {
            let stroke = StrokeEffectParams {
                width_px: 2.5,
                color: [0, 0, 0, 220],
                opacity_mode,
                transparency_percent: 15.0,
                smoothing_enabled: true,
                smoothing_strength_percent: 60.0,
            };
            let mut parallel = sample_glyph_image();
            let mut sequential = sample_glyph_image();
            apply_stroke_effect(&mut parallel, &stroke);
            apply_stroke_effect_seq(&mut sequential, &stroke);
            assert_eq!(parallel.width, sequential.width);
            assert_eq!(parallel.height, sequential.height);
            assert_eq!(parallel.rgba, sequential.rgba);
        }
    }

    /// 17x13 canvas holding a solid 7x5 block whose pixels ALL share one alpha.
    ///
    /// That is what a translucent text color produces away from the glyph rim: the peak alpha
    /// equals the block alpha, so every block pixel is at full contour coverage. Returns the
    /// image and the block bounds `(x0, x1, y0, y1)` (half-open).
    fn uniform_block_image(alpha: u8) -> (RenderedTextImage, usize, usize, usize, usize) {
        let width = 17usize;
        let height = 13usize;
        let (bx0, bx1, by0, by1) = (5usize, 12usize, 4usize, 9usize);
        let mut rgba = vec![0u8; width * height * 4];
        for y in by0..by1 {
            for x in bx0..bx1 {
                let idx = (y * width + x) * 4;
                rgba[idx] = 200;
                rgba[idx + 1] = 40;
                rgba[idx + 2] = 90;
                rgba[idx + 3] = alpha;
            }
        }
        let image = RenderedTextImage {
            width: width as u32,
            height: height as u32,
            rgba,
            warnings: Vec::new(),
            content_origin_x: 0,
            content_origin_y: 0,
            extra: crate::types::RenderedTextExtraInfo::default(),
            font_fallbacks: crate::types::FontFallbackReport::default(),
        };
        (image, bx0, bx1, by0, by1)
    }

    /// A fully opaque static stroke must not turn 50 %-transparent text opaque.
    ///
    /// Before the coverage normalization the static layer covered the whole dilated shape, glyph
    /// body included, so the text was composited over an opaque plate: `(200,40,90,128)` came out
    /// as `(100,20,45,255)`. The ring around the glyph must still reach full alpha.
    #[test]
    fn stroke_static_keeps_a_uniformly_translucent_source_unchanged() {
        let stroke = StrokeEffectParams {
            width_px: 2.0,
            color: [0, 0, 0, 255],
            opacity_mode: StrokeOpacityMode::Static,
            transparency_percent: 0.0,
            smoothing_enabled: false,
            smoothing_strength_percent: 0.0,
        };
        let (source, bx0, bx1, by0, by1) = uniform_block_image(128);
        let expected = source.rgba.clone();
        let mut image = source;
        apply_stroke_effect(&mut image, &stroke);

        let width = image.width as usize;
        for y in by0..by1 {
            for x in bx0..bx1 {
                let idx = (y * width + x) * 4;
                assert_eq!(
                    image.rgba[idx..idx + 4],
                    expected[idx..idx + 4],
                    "body pixel ({x}, {y}) was repainted by the stroke"
                );
            }
        }
        // One pixel outside the block on each side is solidly inside the 2px stroke ring.
        for (x, y) in [(bx0 - 1, by0), (bx1, by1 - 1), (bx0, by0 - 1), (bx1 - 1, by1)] {
            assert_eq!(image.rgba[(y * width + x) * 4 + 3], 255, "ring pixel ({x}, {y})");
        }
    }

    /// Frozen output: a source containing at least one opaque pixel must render byte for byte as
    /// it did before the coverage normalization, because that is every project authored so far.
    ///
    /// `sample_glyph_image` mixes alpha 255 and 160, so its peak is 255 and the correction has to
    /// collapse to the identity across BOTH. Re-capture the digest only for a deliberate visual
    /// change to already-saved stroke effects.
    #[test]
    fn stroke_static_output_is_frozen_for_a_source_with_an_opaque_pixel() {
        let stroke = StrokeEffectParams {
            width_px: 2.5,
            color: [0, 0, 0, 220],
            opacity_mode: StrokeOpacityMode::Static,
            transparency_percent: 15.0,
            smoothing_enabled: true,
            smoothing_strength_percent: 60.0,
        };
        let mut image = sample_glyph_image();
        apply_stroke_effect(&mut image, &stroke);
        assert_eq!(rgba_digest(&image.rgba), 7_436_238_293_734_861_052);
    }

    /// Exhaustive proof that the static formula is bit-identical to the legacy
    /// `max(stroke_target, src_a)` whenever the source has an opaque pixel (`peak_scale == 1.0`).
    #[test]
    fn static_desired_total_matches_the_legacy_maximum_at_an_opaque_peak() {
        for stroke_target_a in 0..=255u8 {
            for src_a in 0..=255u8 {
                let got = static_stroke_desired_total_alpha(stroke_target_a, src_a, 1.0);
                let legacy = f32::from(stroke_target_a.max(src_a));
                assert_eq!(
                    got.to_bits(),
                    legacy.to_bits(),
                    "target {stroke_target_a}, source {src_a}"
                );
            }
        }
    }

    /// With a translucent source the static layer only fills what the contour does not cover:
    /// nothing under the glyph body, and the pixel's uncovered share elsewhere.
    #[test]
    fn static_desired_total_only_adds_what_the_contour_leaves_uncovered() {
        // Uniform 50 %-transparent source: peak == src, so the body keeps its own alpha.
        let peak_scale = 255.0 / 128.0;
        assert_eq!(static_stroke_desired_total_alpha(255, 128, peak_scale), 128.0);
        // Half-covered rim pixel of the same source: the stroke fills the other half.
        assert_eq!(static_stroke_desired_total_alpha(255, 64, peak_scale), 64.0 + 127.5);
        // Outside the glyph the layer is unobstructed.
        assert_eq!(static_stroke_desired_total_alpha(255, 0, peak_scale), 255.0);
        // An empty source carries no coverage information (peak_scale 0) and cannot be dimmed.
        assert_eq!(static_stroke_desired_total_alpha(200, 0, 0.0), 200.0);
    }
}
