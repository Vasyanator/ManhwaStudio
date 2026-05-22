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

    for idx in 0..glow_alpha.len() {
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
        let rgba_idx = idx * 4;
        blend_pixel_over(
            &mut out[rgba_idx..rgba_idx + 4],
            glow.color[0],
            glow.color[1],
            glow.color[2],
            glow_a,
        );
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

    for idx in 0..dist2_map.len() {
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
        let rgba_idx = idx * 4;
        blend_pixel_over(
            &mut out[rgba_idx..rgba_idx + 4],
            glow.color[0],
            glow.color[1],
            glow.color[2],
            glow_a,
        );
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

    for (idx, &alpha_val) in outline_alpha.iter().enumerate() {
        let glow_a = ((alpha_val as f32) * color_alpha_factor)
            .round()
            .clamp(0.0, 255.0) as u8;
        if glow_a == 0 {
            continue;
        }
        let rgba_idx = idx * 4;
        blend_pixel_over(
            &mut out[rgba_idx..rgba_idx + 4],
            glow.color[0],
            glow.color[1],
            glow.color[2],
            glow_a,
        );
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
