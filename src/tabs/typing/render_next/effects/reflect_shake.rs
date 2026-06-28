/*
File: src/tabs/typing/render_next/effects/reflect_shake.rs

Purpose:
Reflect/shake post-effects нового рендера typing.

Main responsibilities:
- зеркалить растровый результат по оси X или Y;
- строить shake-trail с optional autogrow и blur;
- использовать общий image helper-слой для многократного blit/composite.
*/

use super::super::types::RenderedTextImage;
use super::image_ops::{
    blend_full_image_over, draw_image_with_opacity, gaussian_blur_rgba_in_place,
};
use super::parse::{ReflectAxis, ShakeEffectParams};
use rayon::prelude::*;

pub(crate) fn apply_reflect_effect(image: &mut RenderedTextImage, axis: ReflectAxis) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; source.len()];
    let row_stride = width * 4;

    // Pure gather: each output pixel copies exactly one read-only source pixel, so the
    // mirror is parallelized over output rows with no overlapping writes.
    out.par_chunks_mut(row_stride)
        .enumerate()
        .for_each(|(y, out_row)| {
            let src_y = match axis {
                ReflectAxis::X => height - 1 - y,
                ReflectAxis::Y => y,
            };
            for x in 0..width {
                let src_x = match axis {
                    ReflectAxis::X => x,
                    ReflectAxis::Y => width - 1 - x,
                };
                let src_idx = (src_y * width + src_x) * 4;
                let dst_idx = x * 4;
                out_row[dst_idx..dst_idx + 4].copy_from_slice(&source[src_idx..src_idx + 4]);
            }
        });

    image.rgba = out;
}

pub(crate) fn apply_shake_effect(image: &mut RenderedTextImage, shake: &ShakeEffectParams) {
    let source_width = image.width as usize;
    let source_height = image.height as usize;
    if source_width == 0 || source_height == 0 {
        return;
    }
    if shake.steps == 0 || (shake.up_px <= f32::EPSILON && shake.down_px <= f32::EPSILON) {
        return;
    }

    let theta = shake.angle_deg.rem_euclid(360.0).to_radians();
    let unit_x = theta.cos();
    let unit_y = theta.sin();

    let mut offsets = Vec::<(i32, i32)>::new();
    let steps_f = shake.steps as f32;
    let mut add_series = |sign: f32, amount: f32| {
        if amount <= f32::EPSILON {
            return;
        }
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (sign * unit_x * (amount * t)).round() as i32;
            let dy = (sign * unit_y * (amount * t)).round() as i32;
            offsets.push((dx, dy));
        }
    };
    add_series(1.0, shake.down_px.max(0.0));
    add_series(-1.0, shake.up_px.max(0.0));

    if offsets.is_empty() {
        return;
    }

    let mut min_dx = 0i32;
    let mut max_dx = 0i32;
    let mut min_dy = 0i32;
    let mut max_dy = 0i32;
    for (dx, dy) in offsets.iter().copied() {
        min_dx = min_dx.min(dx);
        max_dx = max_dx.max(dx);
        min_dy = min_dy.min(dy);
        max_dy = max_dy.max(dy);
    }

    let blur_pad = if shake.blur_px > 0 {
        ((shake.blur_px as f32) * 3.0).ceil() as i32
    } else {
        0
    };
    let extra_pad = blur_pad.saturating_add(shake.grow_margin_px as i32);

    let (left_pad, right_pad, top_pad, bottom_pad) = if shake.autogrow {
        (
            (-min_dx).max(0).saturating_add(extra_pad),
            max_dx.max(0).saturating_add(extra_pad),
            (-min_dy).max(0).saturating_add(extra_pad),
            max_dy.max(0).saturating_add(extra_pad),
        )
    } else {
        (0, 0, 0, 0)
    };

    let source = image.rgba.clone();
    let source_width_u32 = image.width;
    let source_height_u32 = image.height;

    if left_pad > 0 || right_pad > 0 || top_pad > 0 || bottom_pad > 0 {
        let out_width = image
            .width
            .saturating_add(left_pad as u32)
            .saturating_add(right_pad as u32);
        let out_height = image
            .height
            .saturating_add(top_pad as u32)
            .saturating_add(bottom_pad as u32);
        if out_width == 0 || out_height == 0 {
            return;
        }

        let mut base = vec![0u8; out_width as usize * out_height as usize * 4];
        draw_image_with_opacity(
            &mut base,
            out_width as usize,
            out_height as usize,
            source.as_slice(),
            source_width,
            source_height,
            left_pad,
            top_pad,
            1.0,
        );
        image.width = out_width;
        image.height = out_height;
        image.rgba = base;
        // Контент вставлен в (left_pad, top_pad) внутри увеличенного буфера
        // (только при autogrow; иначе паддинги нулевые и в этот блок не входим).
        image.content_origin_x = image.content_origin_x.saturating_add(left_pad as u32);
        image.content_origin_y = image.content_origin_y.saturating_add(top_pad as u32);
    }

    let trail_width = image.width as usize;
    let trail_height = image.height as usize;
    if trail_width == 0 || trail_height == 0 {
        return;
    }
    let mut trail = vec![0u8; trail_width * trail_height * 4];

    let opacity_start = (1.0 - shake.base_fade).clamp(0.0, 1.0);
    let step_factor = (1.0 - shake.decay).clamp(0.0, 1.0);

    if shake.down_px > f32::EPSILON {
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (unit_x * (shake.down_px * t)).round() as i32;
            let dy = (unit_y * (shake.down_px * t)).round() as i32;
            let opacity = (opacity_start * step_factor.powi((i - 1) as i32)).clamp(0.0, 1.0);
            draw_image_with_opacity(
                &mut trail,
                trail_width,
                trail_height,
                source.as_slice(),
                source_width_u32 as usize,
                source_height_u32 as usize,
                left_pad.saturating_add(dx),
                top_pad.saturating_add(dy),
                opacity,
            );
        }
    }

    if shake.up_px > f32::EPSILON {
        for i in 1..=shake.steps {
            let t = i as f32 / steps_f;
            let dx = (-unit_x * (shake.up_px * t)).round() as i32;
            let dy = (-unit_y * (shake.up_px * t)).round() as i32;
            let opacity = (opacity_start * step_factor.powi((i - 1) as i32)).clamp(0.0, 1.0);
            draw_image_with_opacity(
                &mut trail,
                trail_width,
                trail_height,
                source.as_slice(),
                source_width_u32 as usize,
                source_height_u32 as usize,
                left_pad.saturating_add(dx),
                top_pad.saturating_add(dy),
                opacity,
            );
        }
    }

    if shake.blur_px > 0 {
        gaussian_blur_rgba_in_place(
            &mut trail,
            trail_width as u32,
            trail_height as u32,
            shake.blur_px as f32,
        );
    }

    blend_full_image_over(&mut image.rgba, trail.as_slice());
}

#[cfg(test)]
mod tests {
    use super::super::parse::ReflectAxis;
    use super::apply_reflect_effect;
    use crate::tabs::typing::render_next::types::RenderedTextImage;

    fn sample_image() -> RenderedTextImage {
        let width = 13usize;
        let height = 9usize;
        let mut rgba = vec![0u8; width * height * 4];
        for (idx, byte) in rgba.iter_mut().enumerate() {
            *byte = ((idx * 53 + idx / 3 + 7) % 256) as u8;
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

    /// Verbatim sequential reference of the reflect gather (pre-parallelization).
    fn apply_reflect_seq(image: &mut RenderedTextImage, axis: ReflectAxis) {
        let width = image.width as usize;
        let height = image.height as usize;
        let source = image.rgba.clone();
        let mut out = vec![0u8; source.len()];
        for y in 0..height {
            for x in 0..width {
                let src_x = match axis {
                    ReflectAxis::X => x,
                    ReflectAxis::Y => width - 1 - x,
                };
                let src_y = match axis {
                    ReflectAxis::X => height - 1 - y,
                    ReflectAxis::Y => y,
                };
                let src_idx = (src_y * width + src_x) * 4;
                let dst_idx = (y * width + x) * 4;
                out[dst_idx..dst_idx + 4].copy_from_slice(&source[src_idx..src_idx + 4]);
            }
        }
        image.rgba = out;
    }

    #[test]
    fn reflect_x_parallel_matches_sequential() {
        let mut parallel = sample_image();
        let mut sequential = sample_image();
        apply_reflect_effect(&mut parallel, ReflectAxis::X);
        apply_reflect_seq(&mut sequential, ReflectAxis::X);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn reflect_y_parallel_matches_sequential() {
        let mut parallel = sample_image();
        let mut sequential = sample_image();
        apply_reflect_effect(&mut parallel, ReflectAxis::Y);
        apply_reflect_seq(&mut sequential, ReflectAxis::Y);
        assert_eq!(parallel.rgba, sequential.rgba);
    }
}
