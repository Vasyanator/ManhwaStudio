/*
File: src/tabs/typing/render_next/effects/blur.rs

Purpose:
Blur-based post-effects нового рендера typing.

Main responsibilities:
- применять gaussian blur к итоговому RGBA-изображению текста;
- строить motion blur через bilinear sampling и optional sharp-copy compose;
- использовать общий image helper-слой без знания о layout pipeline.
*/

use super::super::types::RenderedTextImage;
use super::image_ops::{
    blend_full_image_over, draw_image_with_opacity, gaussian_blur_rgba_in_place,
    sample_rgba_premultiplied_bilinear,
};
use super::parse::{BlurEffectParams, MotionBlurEffectParams, MotionBlurSharpCopyMode};

pub(crate) fn apply_blur_effect(image: &mut RenderedTextImage, blur: &BlurEffectParams) {
    let radius_px = blur.radius_px.max(0.0);
    if radius_px <= f32::EPSILON || image.width == 0 || image.height == 0 {
        return;
    }

    let pad = (radius_px * 3.0).ceil() as i32;
    if pad > 0 {
        let out_width = image.width.saturating_add((pad as u32).saturating_mul(2));
        let out_height = image.height.saturating_add((pad as u32).saturating_mul(2));
        if out_width > 0 && out_height > 0 {
            let mut expanded = vec![0u8; out_width as usize * out_height as usize * 4];
            draw_image_with_opacity(
                expanded.as_mut_slice(),
                out_width as usize,
                out_height as usize,
                image.rgba.as_slice(),
                image.width as usize,
                image.height as usize,
                pad,
                pad,
                1.0,
            );
            image.width = out_width;
            image.height = out_height;
            image.rgba = expanded;
        }
    }

    gaussian_blur_rgba_in_place(&mut image.rgba, image.width, image.height, radius_px);
}

pub(crate) fn apply_motion_blur_effect(
    image: &mut RenderedTextImage,
    blur: &MotionBlurEffectParams,
) {
    let source_width = image.width as usize;
    let source_height = image.height as usize;
    if source_width == 0 || source_height == 0 {
        return;
    }

    let distance_px = blur.distance_px.max(0.0);
    if distance_px <= f32::EPSILON {
        return;
    }

    let theta = blur.angle_deg.rem_euclid(360.0).to_radians();
    let half_distance = distance_px * 0.5;
    let pad_x = (theta.cos().abs() * half_distance).ceil() as i32 + 1;
    let pad_y = (theta.sin().abs() * half_distance).ceil() as i32 + 1;
    let out_width = image
        .width
        .saturating_add((pad_x.max(0) as u32).saturating_mul(2));
    let out_height = image
        .height
        .saturating_add((pad_y.max(0) as u32).saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut base = vec![0u8; out_width as usize * out_height as usize * 4];
    draw_image_with_opacity(
        base.as_mut_slice(),
        out_width as usize,
        out_height as usize,
        source.as_slice(),
        source_width,
        source_height,
        pad_x,
        pad_y,
        1.0,
    );

    let sample_count = motion_blur_sample_count(distance_px);
    let dir_x = theta.cos();
    let dir_y = theta.sin();
    let mut blurred = vec![0u8; base.len()];

    for y in 0..out_height as usize {
        for x in 0..out_width as usize {
            let mut accum_r = 0.0f32;
            let mut accum_g = 0.0f32;
            let mut accum_b = 0.0f32;
            let mut accum_a = 0.0f32;
            let mut total_weight = 0.0f32;

            for sample_idx in 0..sample_count {
                let sample_t = motion_blur_sample_t(sample_idx, sample_count, distance_px);
                let sample_weight =
                    motion_blur_sample_weight(sample_idx, sample_count).max(f32::EPSILON);
                let sample_x = x as f32 + dir_x * sample_t;
                let sample_y = y as f32 + dir_y * sample_t;
                let (src_r, src_g, src_b, src_a) = sample_rgba_premultiplied_bilinear(
                    base.as_slice(),
                    out_width as usize,
                    out_height as usize,
                    sample_x,
                    sample_y,
                );
                accum_r += src_r * sample_weight;
                accum_g += src_g * sample_weight;
                accum_b += src_b * sample_weight;
                accum_a += src_a * sample_weight;
                total_weight += sample_weight;
            }

            if total_weight <= f32::EPSILON {
                continue;
            }

            let out_a = (accum_a / total_weight).clamp(0.0, 1.0);
            if out_a <= f32::EPSILON {
                continue;
            }

            let dst_idx = (y * out_width as usize + x) * 4;
            blurred[dst_idx] = ((accum_r / total_weight) / out_a * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            blurred[dst_idx + 1] = ((accum_g / total_weight) / out_a * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            blurred[dst_idx + 2] = ((accum_b / total_weight) / out_a * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            blurred[dst_idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    let rgba = match blur.sharp_copy_mode {
        MotionBlurSharpCopyMode::None => blurred,
        MotionBlurSharpCopyMode::Over => {
            let mut composed = blurred;
            blend_full_image_over(&mut composed, base.as_slice());
            composed
        }
        MotionBlurSharpCopyMode::Under => {
            let mut composed = base;
            blend_full_image_over(&mut composed, blurred.as_slice());
            composed
        }
    };

    image.width = out_width;
    image.height = out_height;
    image.rgba = rgba;
}

fn motion_blur_sample_count(distance_px: f32) -> usize {
    distance_px.ceil().clamp(8.0, 128.0) as usize
}

fn motion_blur_sample_t(sample_idx: usize, sample_count: usize, distance_px: f32) -> f32 {
    if sample_count <= 1 {
        return 0.0;
    }
    let span = distance_px.max(0.0);
    let denom = (sample_count - 1) as f32;
    let normalized = sample_idx as f32 / denom;
    (normalized - 0.5) * span
}

fn motion_blur_sample_weight(sample_idx: usize, sample_count: usize) -> f32 {
    if sample_count <= 1 {
        return 1.0;
    }
    let denom = (sample_count - 1) as f32;
    let normalized = sample_idx as f32 / denom;
    let center_emphasis = 1.0 - (normalized * 2.0 - 1.0).abs();
    0.35 + center_emphasis * 0.65
}
