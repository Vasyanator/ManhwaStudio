/*
File: src/tabs/typing/render_next/effects/stroke_shadow.rs

Purpose:
Contour-based stroke/shadow эффекты нового рендера typing.

Main responsibilities:
- строить обводку поверх alpha-контура текста;
- рендерить отдельный shadow-layer с optional blur и source-color mode;
- переиспользовать общий image helper-слой без привязки к центральному pipeline.
*/

use super::super::raster::blend_pixel_over;
use super::super::types::RenderedTextImage;
use super::image_ops::{
    blend_full_image_over, gaussian_blur_alpha_in_place, gaussian_blur_rgba_in_place,
};
use super::parse::{ShadowEffectParams, StrokeEffectParams, StrokeOpacityMode};

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
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let rgba_idx = idx * 4;
            let src_a = source_alpha[idx];
            let desired_total_a = match stroke.opacity_mode {
                StrokeOpacityMode::FromContour => {
                    ((stroke_alpha[idx] as u16 * stroke.color[3] as u16) / 255) as u8
                }
                StrokeOpacityMode::Static => {
                    let stroke_target_a =
                        ((stroke_alpha[idx] as u16 * static_tinted_alpha as u16) / 255) as u8;
                    stroke_target_a.max(src_a)
                }
            };
            let stroke_out_a = required_under_alpha_for_total_alpha(desired_total_a, src_a);
            if stroke_out_a > 0 {
                blend_pixel_over(
                    &mut out[rgba_idx..rgba_idx + 4],
                    stroke.color[0],
                    stroke.color[1],
                    stroke.color[2],
                    stroke_out_a,
                );
            }
            blend_pixel_over(
                &mut out[rgba_idx..rgba_idx + 4],
                source[rgba_idx],
                source[rgba_idx + 1],
                source[rgba_idx + 2],
                source[rgba_idx + 3],
            );
        }
    }

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
}

fn required_under_alpha_for_total_alpha(desired_total_a: u8, source_a: u8) -> u8 {
    if desired_total_a <= source_a || source_a == u8::MAX {
        return 0;
    }

    let source_cov = source_a as f32 / 255.0;
    let uncovered = 1.0 - source_cov;
    if uncovered <= f32::EPSILON {
        return 0;
    }

    let desired_cov = desired_total_a as f32 / 255.0;
    let needed_cov = ((desired_cov - source_cov) / uncovered).clamp(0.0, 1.0);
    (needed_cov * 255.0).ceil().clamp(0.0, 255.0) as u8
}
