/*
File: src/tabs/typing/render_next/effects/glow.rs

Purpose:
Glow-based post-effects нового рендера typing.

Main responsibilities:
- применять contour glow двух вариантов и soft outline glow;
- держать glow falloff math рядом с glow-реализациями;
- переиспользовать EDT/dilate/blur helper'ы из `image_ops`.
*/

use super::super::raster::blend_pixel_over;
use super::super::types::RenderedTextImage;
use super::image_ops::{
    dilate_alpha_max_filter3, euclidean_distance_transform_to_mask, gaussian_blur_alpha_in_place,
};
use super::parse::{GlowEffectParams, SoftGlowEffectParams, StrokeOpacityMode};
use rayon::prelude::*;

pub(crate) fn apply_glow_effect_v1(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
    let radius = glow.radius_px.max(0.0);
    if radius <= f32::EPSILON {
        return;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = radius.ceil().max(1.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }

    let static_opacity =
        (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let color_alpha_factor = glow.color[3] as f32 / 255.0;
    if color_alpha_factor <= f32::EPSILON {
        return;
    }

    let mut offsets = Vec::<(i32, i32, f32)>::new();
    let radius_i = radius.ceil() as i32;
    for oy in -radius_i..=radius_i {
        for ox in -radius_i..=radius_i {
            let dist = ((ox * ox + oy * oy) as f32).sqrt();
            if dist > radius {
                continue;
            }
            let dist_norm = (dist / radius).clamp(0.0, 1.0);
            let falloff = glow_falloff_alpha(dist_norm, glow.fade_strength, glow.fade_shift);
            if falloff <= f32::EPSILON {
                continue;
            }
            offsets.push((ox, oy, falloff));
        }
    }
    if offsets.is_empty() {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
    let mut glow_alpha = vec![0u8; out_width as usize * out_height as usize];
    let origin_x = pad as i32;
    let origin_y = pad as i32;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let base_x = origin_x + x as i32;
            let base_y = origin_y + y as i32;
            let base_idx = base_y as usize * out_width as usize + base_x as usize;
            source_alpha_expanded[base_idx] = src_a;
            let contour_alpha = src_a as f32 / 255.0;

            for (ox, oy, falloff) in offsets.iter() {
                let tx = base_x + *ox;
                let ty = base_y + *oy;
                if tx < 0 || ty < 0 || tx >= out_width as i32 || ty >= out_height as i32 {
                    continue;
                }
                let alpha_f = match glow.opacity_mode {
                    StrokeOpacityMode::FromContour => contour_alpha * *falloff,
                    StrokeOpacityMode::Static => static_opacity * *falloff,
                };
                if alpha_f <= f32::EPSILON {
                    continue;
                }

                let alpha_u8 = (alpha_f * 255.0).round().clamp(0.0, 255.0) as u8;
                let idx = ty as usize * out_width as usize + tx as usize;
                glow_alpha[idx] = glow_alpha[idx].max(alpha_u8);
            }
        }
    }

    // Each output pixel composites its own glow color from read-only `glow_alpha` and
    // `source_alpha_expanded`, so the glow layer is parallelized per pixel.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
        let glow_only_a = ((glow_alpha[idx] as f32) * overlap)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_only_a == 0 {
            return;
        }
        let glow_a = ((glow_only_a as f32) * color_alpha_factor)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_a == 0 {
            return;
        }
        blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
    });

    blend_source_text(
        &source,
        width,
        height,
        origin_x,
        origin_y,
        out_width as usize,
        &mut out,
    );

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    // Контент сдвинут на pad по обеим осям внутри увеличенного буфера.
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
    image.content_origin_y = image.content_origin_y.saturating_add(pad);
}

pub(crate) fn apply_glow_effect_v2(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
    let radius = glow.radius_px.max(0.0);
    if radius <= f32::EPSILON {
        return;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = radius.ceil().max(1.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }

    let static_opacity =
        (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
    let color_alpha_factor = glow.color[3] as f32 / 255.0;
    if color_alpha_factor <= f32::EPSILON {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
    let mut contour_mask = vec![0u8; out_width as usize * out_height as usize];
    let origin_x = pad as i32;
    let origin_y = pad as i32;
    let mut has_contour = false;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }

            let base_x = origin_x + x as i32;
            let base_y = origin_y + y as i32;
            let base_idx = base_y as usize * out_width as usize + base_x as usize;
            source_alpha_expanded[base_idx] = src_a;
            contour_mask[base_idx] = 1;
            has_contour = true;
        }
    }
    if !has_contour {
        return;
    }

    let dist2_map = euclidean_distance_transform_to_mask(
        contour_mask.as_slice(),
        out_width as usize,
        out_height as usize,
    );
    let radius2 = radius * radius;

    // Each output pixel composites its glow color from the read-only distance map and
    // expanded source alpha, so the EDT-based glow layer is parallelized per pixel.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let dist2 = dist2_map[idx];
        if !dist2.is_finite() || dist2 > radius2 {
            return;
        }
        let dist = dist2.sqrt();
        let falloff = glow_falloff_alpha(
            (dist / radius).clamp(0.0, 1.0),
            glow.fade_strength,
            glow.fade_shift,
        );
        if falloff <= f32::EPSILON {
            return;
        }

        let base_opacity = match glow.opacity_mode {
            StrokeOpacityMode::FromContour => 1.0,
            StrokeOpacityMode::Static => static_opacity,
        };
        let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
        let glow_a = (base_opacity * falloff * overlap * color_alpha_factor * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_a == 0 {
            return;
        }
        blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
    });

    blend_source_text(
        &source,
        width,
        height,
        origin_x,
        origin_y,
        out_width as usize,
        &mut out,
    );

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    // Контент сдвинут на pad по обеим осям внутри увеличенного буфера.
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
    image.content_origin_y = image.content_origin_y.saturating_add(pad);
}

pub(crate) fn apply_soft_glow_effect(image: &mut RenderedTextImage, glow: &SoftGlowEffectParams) {
    if glow.radius_steps == 0 {
        return;
    }
    let color_alpha_factor = glow.color[3] as f32 / 255.0;
    if color_alpha_factor <= f32::EPSILON {
        return;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = ((glow.radius_steps as f32) + glow.softness_px.max(0.0) * 3.0)
        .ceil()
        .max(1.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
    let origin_x = pad as i32;
    let origin_y = pad as i32;

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = origin_x + x as i32;
            let dst_y = origin_y + y as i32;
            let alpha_idx = dst_y as usize * out_width_usize + dst_x as usize;
            source_alpha_expanded[alpha_idx] = src_a;
        }
    }

    let mut dilated = source_alpha_expanded.clone();
    dilate_alpha_max_filter3(
        dilated.as_mut_slice(),
        out_width_usize,
        out_height_usize,
        glow.radius_steps as usize,
    );

    let mut outline_alpha = vec![0u8; out_width_usize * out_height_usize];
    for idx in 0..outline_alpha.len() {
        outline_alpha[idx] = dilated[idx].saturating_sub(source_alpha_expanded[idx]);
    }
    if glow.softness_px > f32::EPSILON {
        gaussian_blur_alpha_in_place(&mut outline_alpha, out_width, out_height, glow.softness_px);
    }

    // Each output pixel composites the soft-glow color from its own read-only outline
    // alpha, so the glow layer is parallelized per pixel.
    out.par_chunks_mut(4)
        .zip(outline_alpha.par_iter())
        .for_each(|(dst, &alpha_val)| {
            let glow_a = ((alpha_val as f32) * color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if glow_a == 0 {
                return;
            }
            blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
        });

    blend_source_text(
        &source,
        width,
        height,
        origin_x,
        origin_y,
        out_width_usize,
        &mut out,
    );

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    // Контент сдвинут на pad по обеим осям внутри увеличенного буфера.
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
    image.content_origin_y = image.content_origin_y.saturating_add(pad);
}

fn glow_falloff_alpha(distance_norm: f32, fade_strength: f32, fade_shift: f32) -> f32 {
    let dist = distance_norm.clamp(0.0, 1.0);
    let shifted = bias01(dist, (0.5 - (fade_shift / 100.0) * 0.49).clamp(0.01, 0.99));
    let shaped = shape_falloff_progress(shifted, fade_strength);
    (1.0 - shaped).clamp(0.0, 1.0)
}

fn shape_falloff_progress(t: f32, fade_strength: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let strength = (fade_strength / 100.0).clamp(-1.0, 1.0);
    if strength.abs() <= f32::EPSILON {
        return t;
    }

    const K_MAX: f32 = 12.0;
    if strength < 0.0 {
        let k = (-strength) * K_MAX;
        ((1.0 + k * t).ln() / (1.0 + k).ln()).clamp(0.0, 1.0)
    } else {
        let k = strength * K_MAX;
        (((1.0 + k).powf(t) - 1.0) / k).clamp(0.0, 1.0)
    }
}

fn bias01(t: f32, bias: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 || t >= 1.0 {
        return t;
    }
    let bias = bias.clamp(0.01, 0.99);
    let k = (1.0 / bias) - 2.0;
    (t / (k * (1.0 - t) + 1.0)).clamp(0.0, 1.0)
}

fn blend_source_text(
    source: &[u8],
    width: usize,
    height: usize,
    origin_x: i32,
    origin_y: i32,
    out_width: usize,
    out: &mut [u8],
) {
    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 4;
            let src_a = source[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let dst_x = origin_x + x as i32;
            let dst_y = origin_y + y as i32;
            let dst_idx = ((dst_y as usize * out_width) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut out[dst_idx..dst_idx + 4],
                source[src_idx],
                source[src_idx + 1],
                source[src_idx + 2],
                src_a,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::{GlowEffectParams, SoftGlowEffectParams, StrokeOpacityMode};
    use super::{apply_glow_effect_v1, apply_glow_effect_v2, apply_soft_glow_effect};
    use crate::types::RenderedTextImage;

    fn sample_glyph_image() -> RenderedTextImage {
        let width = 19usize;
        let height = 17usize;
        let mut rgba = vec![0u8; width * height * 4];
        for y in 6..11 {
            for x in 7..13 {
                let idx = (y * width + x) * 4;
                rgba[idx] = 250;
                rgba[idx + 1] = 250;
                rgba[idx + 2] = 250;
                rgba[idx + 3] = 255;
            }
        }
        RenderedTextImage {
            width: width as u32,
            height: height as u32,
            rgba,
            warnings: Vec::new(),
            content_origin_x: 0,
            content_origin_y: 0,
        }
    }

    /// Verbatim sequential reference of `apply_glow_effect_v1`: identical body with the
    /// `out.par_chunks_mut(4).for_each(...)` glow composite pass replaced by a plain per-pixel
    /// loop. Asserts the rayon path is bit-identical to the pre-parallelization loop.
    fn apply_glow_effect_v1_seq(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
        use super::super::super::raster::blend_pixel_over;
        use super::{blend_source_text, glow_falloff_alpha};

        let radius = glow.radius_px.max(0.0);
        if radius <= f32::EPSILON {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let pad = radius.ceil().max(1.0) as u32;
        let out_width = image.width.saturating_add(pad.saturating_mul(2));
        let out_height = image.height.saturating_add(pad.saturating_mul(2));
        if out_width == 0 || out_height == 0 {
            return;
        }
        let static_opacity =
            (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
        let color_alpha_factor = glow.color[3] as f32 / 255.0;
        if color_alpha_factor <= f32::EPSILON {
            return;
        }
        let mut offsets = Vec::<(i32, i32, f32)>::new();
        let radius_i = radius.ceil() as i32;
        for oy in -radius_i..=radius_i {
            for ox in -radius_i..=radius_i {
                let dist = ((ox * ox + oy * oy) as f32).sqrt();
                if dist > radius {
                    continue;
                }
                let dist_norm = (dist / radius).clamp(0.0, 1.0);
                let falloff = glow_falloff_alpha(dist_norm, glow.fade_strength, glow.fade_shift);
                if falloff <= f32::EPSILON {
                    continue;
                }
                offsets.push((ox, oy, falloff));
            }
        }
        if offsets.is_empty() {
            return;
        }
        let source = image.rgba.clone();
        let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
        let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
        let mut glow_alpha = vec![0u8; out_width as usize * out_height as usize];
        let origin_x = pad as i32;
        let origin_y = pad as i32;
        for y in 0..height {
            for x in 0..width {
                let src_idx = (y * width + x) * 4;
                let src_a = source[src_idx + 3];
                if src_a == 0 {
                    continue;
                }
                let base_x = origin_x + x as i32;
                let base_y = origin_y + y as i32;
                let base_idx = base_y as usize * out_width as usize + base_x as usize;
                source_alpha_expanded[base_idx] = src_a;
                let contour_alpha = src_a as f32 / 255.0;
                for (ox, oy, falloff) in offsets.iter() {
                    let tx = base_x + *ox;
                    let ty = base_y + *oy;
                    if tx < 0 || ty < 0 || tx >= out_width as i32 || ty >= out_height as i32 {
                        continue;
                    }
                    let alpha_f = match glow.opacity_mode {
                        StrokeOpacityMode::FromContour => contour_alpha * *falloff,
                        StrokeOpacityMode::Static => static_opacity * *falloff,
                    };
                    if alpha_f <= f32::EPSILON {
                        continue;
                    }
                    let alpha_u8 = (alpha_f * 255.0).round().clamp(0.0, 255.0) as u8;
                    let idx = ty as usize * out_width as usize + tx as usize;
                    glow_alpha[idx] = glow_alpha[idx].max(alpha_u8);
                }
            }
        }
        for (idx, dst) in out.chunks_mut(4).enumerate() {
            let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
            let glow_only_a = ((glow_alpha[idx] as f32) * overlap)
                .round()
                .clamp(0.0, 255.0) as u8;
            if glow_only_a == 0 {
                continue;
            }
            let glow_a = ((glow_only_a as f32) * color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if glow_a == 0 {
                continue;
            }
            blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
        }
        blend_source_text(
            &source,
            width,
            height,
            origin_x,
            origin_y,
            out_width as usize,
            &mut out,
        );
        image.width = out_width;
        image.height = out_height;
        image.rgba = out;
    }

    /// Verbatim sequential reference of `apply_glow_effect_v2`: identical body with the
    /// EDT-based glow composite `for_each` replaced by a plain per-pixel loop.
    fn apply_glow_effect_v2_seq(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
        use super::super::super::raster::blend_pixel_over;
        use super::super::image_ops::euclidean_distance_transform_to_mask;
        use super::{blend_source_text, glow_falloff_alpha};

        let radius = glow.radius_px.max(0.0);
        if radius <= f32::EPSILON {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let pad = radius.ceil().max(1.0) as u32;
        let out_width = image.width.saturating_add(pad.saturating_mul(2));
        let out_height = image.height.saturating_add(pad.saturating_mul(2));
        if out_width == 0 || out_height == 0 {
            return;
        }
        let static_opacity =
            (1.0 - glow.transparency_percent.clamp(0.0, 100.0) / 100.0).clamp(0.0, 1.0);
        let color_alpha_factor = glow.color[3] as f32 / 255.0;
        if color_alpha_factor <= f32::EPSILON {
            return;
        }
        let source = image.rgba.clone();
        let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
        let mut source_alpha_expanded = vec![0u8; out_width as usize * out_height as usize];
        let mut contour_mask = vec![0u8; out_width as usize * out_height as usize];
        let origin_x = pad as i32;
        let origin_y = pad as i32;
        let mut has_contour = false;
        for y in 0..height {
            for x in 0..width {
                let src_idx = (y * width + x) * 4;
                let src_a = source[src_idx + 3];
                if src_a == 0 {
                    continue;
                }
                let base_x = origin_x + x as i32;
                let base_y = origin_y + y as i32;
                let base_idx = base_y as usize * out_width as usize + base_x as usize;
                source_alpha_expanded[base_idx] = src_a;
                contour_mask[base_idx] = 1;
                has_contour = true;
            }
        }
        if !has_contour {
            return;
        }
        let dist2_map = euclidean_distance_transform_to_mask(
            contour_mask.as_slice(),
            out_width as usize,
            out_height as usize,
        );
        let radius2 = radius * radius;
        for (idx, dst) in out.chunks_mut(4).enumerate() {
            let dist2 = dist2_map[idx];
            if !dist2.is_finite() || dist2 > radius2 {
                continue;
            }
            let dist = dist2.sqrt();
            let falloff = glow_falloff_alpha(
                (dist / radius).clamp(0.0, 1.0),
                glow.fade_strength,
                glow.fade_shift,
            );
            if falloff <= f32::EPSILON {
                continue;
            }
            let base_opacity = match glow.opacity_mode {
                StrokeOpacityMode::FromContour => 1.0,
                StrokeOpacityMode::Static => static_opacity,
            };
            let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
            let glow_a = (base_opacity * falloff * overlap * color_alpha_factor * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            if glow_a == 0 {
                continue;
            }
            blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
        }
        blend_source_text(
            &source,
            width,
            height,
            origin_x,
            origin_y,
            out_width as usize,
            &mut out,
        );
        image.width = out_width;
        image.height = out_height;
        image.rgba = out;
    }

    /// Verbatim sequential reference of `apply_soft_glow_effect`: identical body with the
    /// `out.par_chunks_mut(4).zip(...).for_each(...)` composite replaced by a plain loop.
    fn apply_soft_glow_effect_seq(image: &mut RenderedTextImage, glow: &SoftGlowEffectParams) {
        use super::super::super::raster::blend_pixel_over;
        use super::super::image_ops::{dilate_alpha_max_filter3, gaussian_blur_alpha_in_place};
        use super::blend_source_text;

        if glow.radius_steps == 0 {
            return;
        }
        let color_alpha_factor = glow.color[3] as f32 / 255.0;
        if color_alpha_factor <= f32::EPSILON {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let pad = ((glow.radius_steps as f32) + glow.softness_px.max(0.0) * 3.0)
            .ceil()
            .max(1.0) as u32;
        let out_width = image.width.saturating_add(pad.saturating_mul(2));
        let out_height = image.height.saturating_add(pad.saturating_mul(2));
        if out_width == 0 || out_height == 0 {
            return;
        }
        let out_width_usize = out_width as usize;
        let out_height_usize = out_height as usize;
        let source = image.rgba.clone();
        let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
        let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
        let origin_x = pad as i32;
        let origin_y = pad as i32;
        for y in 0..height {
            for x in 0..width {
                let src_idx = (y * width + x) * 4;
                let src_a = source[src_idx + 3];
                if src_a == 0 {
                    continue;
                }
                let dst_x = origin_x + x as i32;
                let dst_y = origin_y + y as i32;
                let alpha_idx = dst_y as usize * out_width_usize + dst_x as usize;
                source_alpha_expanded[alpha_idx] = src_a;
            }
        }
        let mut dilated = source_alpha_expanded.clone();
        dilate_alpha_max_filter3(
            dilated.as_mut_slice(),
            out_width_usize,
            out_height_usize,
            glow.radius_steps as usize,
        );
        let mut outline_alpha = vec![0u8; out_width_usize * out_height_usize];
        for idx in 0..outline_alpha.len() {
            outline_alpha[idx] = dilated[idx].saturating_sub(source_alpha_expanded[idx]);
        }
        if glow.softness_px > f32::EPSILON {
            gaussian_blur_alpha_in_place(
                &mut outline_alpha,
                out_width,
                out_height,
                glow.softness_px,
            );
        }
        for (dst, &alpha_val) in out.chunks_mut(4).zip(outline_alpha.iter()) {
            let glow_a = ((alpha_val as f32) * color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if glow_a == 0 {
                continue;
            }
            blend_pixel_over(dst, glow.color[0], glow.color[1], glow.color[2], glow_a);
        }
        blend_source_text(
            &source,
            width,
            height,
            origin_x,
            origin_y,
            out_width_usize,
            &mut out,
        );
        image.width = out_width;
        image.height = out_height;
        image.rgba = out;
    }

    fn sample_glow_params() -> GlowEffectParams {
        GlowEffectParams {
            radius_px: 3.0,
            color: [255, 60, 10, 200],
            opacity_mode: StrokeOpacityMode::FromContour,
            transparency_percent: 20.0,
            fade_strength: 30.0,
            fade_shift: 10.0,
        }
    }

    #[test]
    fn glow_v1_parallel_composite_matches_sequential() {
        let glow = sample_glow_params();
        let mut parallel = sample_glyph_image();
        let mut sequential = sample_glyph_image();
        apply_glow_effect_v1(&mut parallel, &glow);
        apply_glow_effect_v1_seq(&mut sequential, &glow);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn glow_v2_parallel_composite_matches_sequential() {
        let glow = sample_glow_params();
        let mut parallel = sample_glyph_image();
        let mut sequential = sample_glyph_image();
        apply_glow_effect_v2(&mut parallel, &glow);
        apply_glow_effect_v2_seq(&mut sequential, &glow);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn soft_glow_parallel_composite_matches_sequential() {
        let glow = SoftGlowEffectParams {
            radius_steps: 2,
            softness_px: 1.4,
            color: [10, 200, 255, 180],
        };
        let mut parallel = sample_glyph_image();
        let mut sequential = sample_glyph_image();
        apply_soft_glow_effect(&mut parallel, &glow);
        apply_soft_glow_effect_seq(&mut sequential, &glow);
        assert_eq!(parallel.rgba, sequential.rgba);
    }
}
