/*
File: src/tabs/typing/render_next/effects/gradients.rs

Purpose:
Градиентные post-effects нового рендера typing.

Main responsibilities:
- применять двухцветный и четырёхугловой градиенты к уже растеризованному тексту;
- переиспользовать общий alpha-bbox и fill-mode contract из `parse.rs`;
- держать локальные тесты на math helper'ы градиентов.
*/

use super::super::types::RenderedTextImage;
use super::parse::{
    Gradient2EffectParams, Gradient2FillMode, Gradient4EffectParams, Gradient4FillMode,
};
use rayon::prelude::*;

pub(crate) fn apply_gradient2_effect(
    image: &mut RenderedTextImage,
    gradient: &Gradient2EffectParams,
) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let Some((min_x, min_y, max_x, max_y)) = alpha_bounds(source.as_slice(), width, height) else {
        return;
    };

    let bbox_w = (max_x - min_x + 1) as usize;
    let bbox_h = (max_y - min_y + 1) as usize;
    if bbox_w == 0 || bbox_h == 0 {
        return;
    }

    let angle_rad = gradient.angle_deg.to_radians();
    let dir_x = angle_rad.cos();
    let dir_y = angle_rad.sin();
    let center_x = (bbox_w as f32 - 1.0) * 0.5;
    let center_y = (bbox_h as f32 - 1.0) * 0.5;

    let mut min_proj = f32::INFINITY;
    let mut max_proj = f32::NEG_INFINITY;
    for (x, y) in [
        (0.0f32, 0.0f32),
        ((bbox_w as f32 - 1.0).max(0.0), 0.0f32),
        (0.0f32, (bbox_h as f32 - 1.0).max(0.0)),
        (
            (bbox_w as f32 - 1.0).max(0.0),
            (bbox_h as f32 - 1.0).max(0.0),
        ),
    ] {
        let proj = (x - center_x) * dir_x + (y - center_y) * dir_y;
        min_proj = min_proj.min(proj);
        max_proj = max_proj.max(proj);
    }
    let proj_range = (max_proj - min_proj).max(f32::EPSILON);

    let mut out = source.clone();
    let row_stride = width * 4;
    // Each output pixel writes only its own slot from the read-only `source`, so the
    // gradient fill is parallelized over the bbox rows of `out` with no shared state.
    out.par_chunks_mut(row_stride)
        .enumerate()
        .for_each(|(image_y, out_row)| {
            if (image_y as i32) < min_y || (image_y as i32) > max_y {
                return;
            }
            let y = image_y - min_y as usize;
            for x in 0..bbox_w {
                let image_x = min_x as usize + x;
                let idx = image_x * 4;
                let src_idx = image_y * row_stride + idx;
                let src_a = source[src_idx + 3];
                if src_a == 0 || !should_replace_gradient2(&source, src_idx, gradient) {
                    continue;
                }

                let proj = (x as f32 - center_x) * dir_x + (y as f32 - center_y) * dir_y;
                let centered_proj = proj - ((min_proj + max_proj) * 0.5);
                let t = gradient2_mix_factor(centered_proj, proj_range, gradient.width_percent);
                let inv_t = 1.0 - t;

                let grad_r =
                    ((gradient.color1[0] as f32) * inv_t + (gradient.color2[0] as f32) * t).round();
                let grad_g =
                    ((gradient.color1[1] as f32) * inv_t + (gradient.color2[1] as f32) * t).round();
                let grad_b =
                    ((gradient.color1[2] as f32) * inv_t + (gradient.color2[2] as f32) * t).round();
                let grad_a =
                    ((gradient.color1[3] as f32) * inv_t + (gradient.color2[3] as f32) * t).round();
                let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
                if gradient.respect_source_alpha {
                    out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
                }

                out_row[idx] = grad_r.clamp(0.0, 255.0) as u8;
                out_row[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
                out_row[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
                out_row[idx + 3] = out_a;
            }
        });

    image.rgba = out;
}

pub(crate) fn gradient2_mix_factor(centered_proj: f32, base_range: f32, width_percent: f32) -> f32 {
    let gradient_range = (base_range.max(f32::EPSILON) * (width_percent / 100.0).max(f32::EPSILON))
        .max(f32::EPSILON);
    ((centered_proj + gradient_range * 0.5) / gradient_range).clamp(0.0, 1.0)
}

pub(crate) fn apply_gradient4_effect(
    image: &mut RenderedTextImage,
    gradient: &Gradient4EffectParams,
) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let source = image.rgba.clone();
    let Some((min_x, min_y, max_x, max_y)) = alpha_bounds(source.as_slice(), width, height) else {
        return;
    };

    let bbox_w = (max_x - min_x + 1) as usize;
    let bbox_h = (max_y - min_y + 1) as usize;
    if bbox_w == 0 || bbox_h == 0 {
        return;
    }

    let mut out = source.clone();
    let denom_x = (bbox_w.saturating_sub(1)).max(1) as f32;
    let denom_y = (bbox_h.saturating_sub(1)).max(1) as f32;
    let row_stride = width * 4;

    // Each output pixel is computed independently from the read-only `source`, so the
    // bilinear corner blend is parallelized over the bbox rows of `out`.
    out.par_chunks_mut(row_stride)
        .enumerate()
        .for_each(|(image_y, out_row)| {
            if (image_y as i32) < min_y || (image_y as i32) > max_y {
                return;
            }
            let y = image_y - min_y as usize;
            for x in 0..bbox_w {
                let image_x = min_x as usize + x;
                let idx = image_x * 4;
                let src_idx = image_y * row_stride + idx;
                let src_a = source[src_idx + 3];
                if src_a == 0 || !should_replace_gradient4(&source, src_idx, gradient) {
                    continue;
                }

                let u = if bbox_w > 1 { x as f32 / denom_x } else { 0.5 };
                let v = if bbox_h > 1 { y as f32 / denom_y } else { 0.5 };
                let u = gradient4_mix_factor(u, gradient.width_percent);
                let v = gradient4_mix_factor(v, gradient.width_percent);
                let inv_u = 1.0 - u;
                let inv_v = 1.0 - v;

                let grad_r = ((gradient.color_top_left[0] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[0] as f32) * u * inv_v
                    + (gradient.color_bottom_left[0] as f32) * inv_u * v
                    + (gradient.color_bottom_right[0] as f32) * u * v)
                    .round();
                let grad_g = ((gradient.color_top_left[1] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[1] as f32) * u * inv_v
                    + (gradient.color_bottom_left[1] as f32) * inv_u * v
                    + (gradient.color_bottom_right[1] as f32) * u * v)
                    .round();
                let grad_b = ((gradient.color_top_left[2] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[2] as f32) * u * inv_v
                    + (gradient.color_bottom_left[2] as f32) * inv_u * v
                    + (gradient.color_bottom_right[2] as f32) * u * v)
                    .round();
                let grad_a = ((gradient.color_top_left[3] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[3] as f32) * u * inv_v
                    + (gradient.color_bottom_left[3] as f32) * inv_u * v
                    + (gradient.color_bottom_right[3] as f32) * u * v)
                    .round();
                let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
                if gradient.respect_source_alpha {
                    out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
                }

                out_row[idx] = grad_r.clamp(0.0, 255.0) as u8;
                out_row[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
                out_row[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
                out_row[idx + 3] = out_a;
            }
        });

    image.rgba = out;
}

pub(crate) fn gradient4_mix_factor(coord: f32, width_percent: f32) -> f32 {
    let scale = (width_percent / 100.0).max(f32::EPSILON);
    (((coord - 0.5) / scale) + 0.5).clamp(0.0, 1.0)
}

fn alpha_bounds(source: &[u8], width: usize, height: usize) -> Option<(i32, i32, i32, i32)> {
    let mut min_x = width as i32;
    let mut min_y = height as i32;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            if source[idx + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x as i32);
            min_y = min_y.min(y as i32);
            max_x = max_x.max(x as i32);
            max_y = max_y.max(y as i32);
        }
    }

    (max_x >= min_x && max_y >= min_y).then_some((min_x, min_y, max_x, max_y))
}

fn should_replace_gradient2(source: &[u8], idx: usize, gradient: &Gradient2EffectParams) -> bool {
    match gradient.fill_mode {
        Gradient2FillMode::AllOpaque => true,
        Gradient2FillMode::SpecificColor => {
            source[idx] == gradient.target_color[0]
                && source[idx + 1] == gradient.target_color[1]
                && source[idx + 2] == gradient.target_color[2]
        }
    }
}

fn should_replace_gradient4(source: &[u8], idx: usize, gradient: &Gradient4EffectParams) -> bool {
    match gradient.fill_mode {
        Gradient4FillMode::AllOpaque => true,
        Gradient4FillMode::SpecificColor => {
            source[idx] == gradient.target_color[0]
                && source[idx + 1] == gradient.target_color[1]
                && source[idx + 2] == gradient.target_color[2]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::{
        Gradient2EffectParams, Gradient2FillMode, Gradient4EffectParams, Gradient4FillMode,
    };
    use super::{
        alpha_bounds, apply_gradient2_effect, apply_gradient4_effect, gradient2_mix_factor,
        gradient4_mix_factor, should_replace_gradient2, should_replace_gradient4,
    };
    use crate::types::RenderedTextImage;

    fn sample_text_image() -> RenderedTextImage {
        let width = 23usize;
        let height = 19usize;
        let mut rgba = vec![0u8; width * height * 4];
        // Diagonal opaque band plus a couple of fully transparent gaps to exercise the
        // alpha-bounds path and the `src_a == 0` skip branch.
        for y in 0..height {
            for x in 0..width {
                if (x + y) % 3 != 0 && x >= 2 && x < width - 2 && y >= 2 && y < height - 2 {
                    let idx = (y * width + x) * 4;
                    rgba[idx] = ((x * 11) % 256) as u8;
                    rgba[idx + 1] = ((y * 7) % 256) as u8;
                    rgba[idx + 2] = ((x * y) % 256) as u8;
                    rgba[idx + 3] = 200;
                }
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
        }
    }

    /// Verbatim sequential reference for the gradient2 fill (pre-parallelization logic).
    fn apply_gradient2_seq(image: &mut RenderedTextImage, gradient: &Gradient2EffectParams) {
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let source = image.rgba.clone();
        let Some((min_x, min_y, max_x, max_y)) = alpha_bounds(source.as_slice(), width, height)
        else {
            return;
        };
        let bbox_w = (max_x - min_x + 1) as usize;
        let bbox_h = (max_y - min_y + 1) as usize;
        if bbox_w == 0 || bbox_h == 0 {
            return;
        }
        let angle_rad = gradient.angle_deg.to_radians();
        let dir_x = angle_rad.cos();
        let dir_y = angle_rad.sin();
        let center_x = (bbox_w as f32 - 1.0) * 0.5;
        let center_y = (bbox_h as f32 - 1.0) * 0.5;
        let mut min_proj = f32::INFINITY;
        let mut max_proj = f32::NEG_INFINITY;
        for (x, y) in [
            (0.0f32, 0.0f32),
            ((bbox_w as f32 - 1.0).max(0.0), 0.0f32),
            (0.0f32, (bbox_h as f32 - 1.0).max(0.0)),
            (
                (bbox_w as f32 - 1.0).max(0.0),
                (bbox_h as f32 - 1.0).max(0.0),
            ),
        ] {
            let proj = (x - center_x) * dir_x + (y - center_y) * dir_y;
            min_proj = min_proj.min(proj);
            max_proj = max_proj.max(proj);
        }
        let proj_range = (max_proj - min_proj).max(f32::EPSILON);
        let mut out = source.clone();
        for y in 0..bbox_h {
            for x in 0..bbox_w {
                let image_x = min_x + x as i32;
                let image_y = min_y + y as i32;
                let idx = ((image_y as usize * width) + image_x as usize) * 4;
                let src_a = source[idx + 3];
                if src_a == 0 || !should_replace_gradient2(&source, idx, gradient) {
                    continue;
                }
                let proj = (x as f32 - center_x) * dir_x + (y as f32 - center_y) * dir_y;
                let centered_proj = proj - ((min_proj + max_proj) * 0.5);
                let t = gradient2_mix_factor(centered_proj, proj_range, gradient.width_percent);
                let inv_t = 1.0 - t;
                let grad_r =
                    ((gradient.color1[0] as f32) * inv_t + (gradient.color2[0] as f32) * t).round();
                let grad_g =
                    ((gradient.color1[1] as f32) * inv_t + (gradient.color2[1] as f32) * t).round();
                let grad_b =
                    ((gradient.color1[2] as f32) * inv_t + (gradient.color2[2] as f32) * t).round();
                let grad_a =
                    ((gradient.color1[3] as f32) * inv_t + (gradient.color2[3] as f32) * t).round();
                let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
                if gradient.respect_source_alpha {
                    out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
                }
                out[idx] = grad_r.clamp(0.0, 255.0) as u8;
                out[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
                out[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
                out[idx + 3] = out_a;
            }
        }
        image.rgba = out;
    }

    /// Verbatim sequential reference for the gradient4 fill (pre-parallelization logic).
    fn apply_gradient4_seq(image: &mut RenderedTextImage, gradient: &Gradient4EffectParams) {
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let source = image.rgba.clone();
        let Some((min_x, min_y, max_x, max_y)) = alpha_bounds(source.as_slice(), width, height)
        else {
            return;
        };
        let bbox_w = (max_x - min_x + 1) as usize;
        let bbox_h = (max_y - min_y + 1) as usize;
        if bbox_w == 0 || bbox_h == 0 {
            return;
        }
        let mut out = source.clone();
        let denom_x = (bbox_w.saturating_sub(1)).max(1) as f32;
        let denom_y = (bbox_h.saturating_sub(1)).max(1) as f32;
        for y in 0..bbox_h {
            for x in 0..bbox_w {
                let image_x = min_x + x as i32;
                let image_y = min_y + y as i32;
                let idx = ((image_y as usize * width) + image_x as usize) * 4;
                let src_a = source[idx + 3];
                if src_a == 0 || !should_replace_gradient4(&source, idx, gradient) {
                    continue;
                }
                let u = if bbox_w > 1 { x as f32 / denom_x } else { 0.5 };
                let v = if bbox_h > 1 { y as f32 / denom_y } else { 0.5 };
                let u = gradient4_mix_factor(u, gradient.width_percent);
                let v = gradient4_mix_factor(v, gradient.width_percent);
                let inv_u = 1.0 - u;
                let inv_v = 1.0 - v;
                let grad_r = ((gradient.color_top_left[0] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[0] as f32) * u * inv_v
                    + (gradient.color_bottom_left[0] as f32) * inv_u * v
                    + (gradient.color_bottom_right[0] as f32) * u * v)
                    .round();
                let grad_g = ((gradient.color_top_left[1] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[1] as f32) * u * inv_v
                    + (gradient.color_bottom_left[1] as f32) * inv_u * v
                    + (gradient.color_bottom_right[1] as f32) * u * v)
                    .round();
                let grad_b = ((gradient.color_top_left[2] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[2] as f32) * u * inv_v
                    + (gradient.color_bottom_left[2] as f32) * inv_u * v
                    + (gradient.color_bottom_right[2] as f32) * u * v)
                    .round();
                let grad_a = ((gradient.color_top_left[3] as f32) * inv_u * inv_v
                    + (gradient.color_top_right[3] as f32) * u * inv_v
                    + (gradient.color_bottom_left[3] as f32) * inv_u * v
                    + (gradient.color_bottom_right[3] as f32) * u * v)
                    .round();
                let mut out_a = grad_a.clamp(0.0, 255.0) as u8;
                if gradient.respect_source_alpha {
                    out_a = ((out_a as u16 * src_a as u16) / 255) as u8;
                }
                out[idx] = grad_r.clamp(0.0, 255.0) as u8;
                out[idx + 1] = grad_g.clamp(0.0, 255.0) as u8;
                out[idx + 2] = grad_b.clamp(0.0, 255.0) as u8;
                out[idx + 3] = out_a;
            }
        }
        image.rgba = out;
    }

    #[test]
    fn gradient2_parallel_matches_sequential() {
        let gradient = Gradient2EffectParams {
            color1: [255, 0, 0, 255],
            color2: [0, 0, 255, 128],
            angle_deg: 37.0,
            width_percent: 80.0,
            fill_mode: Gradient2FillMode::AllOpaque,
            target_color: [0, 0, 0, 255],
            respect_source_alpha: true,
        };

        let mut parallel = sample_text_image();
        let mut sequential = sample_text_image();
        apply_gradient2_effect(&mut parallel, &gradient);
        apply_gradient2_seq(&mut sequential, &gradient);

        assert_eq!(parallel.width, sequential.width);
        assert_eq!(parallel.height, sequential.height);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn gradient4_parallel_matches_sequential() {
        let gradient = Gradient4EffectParams {
            color_top_left: [255, 0, 0, 255],
            color_top_right: [0, 255, 0, 255],
            color_bottom_left: [0, 0, 255, 200],
            color_bottom_right: [255, 255, 0, 64],
            width_percent: 120.0,
            fill_mode: Gradient4FillMode::AllOpaque,
            target_color: [0, 0, 0, 255],
            respect_source_alpha: true,
        };

        let mut parallel = sample_text_image();
        let mut sequential = sample_text_image();
        apply_gradient4_effect(&mut parallel, &gradient);
        apply_gradient4_seq(&mut sequential, &gradient);

        assert_eq!(parallel.width, sequential.width);
        assert_eq!(parallel.height, sequential.height);
        assert_eq!(parallel.rgba, sequential.rgba);
    }

    #[test]
    fn gradient2_width_percent_changes_mix_zone() {
        let left_at_default = gradient2_mix_factor(-5.0, 10.0, 100.0);
        let left_at_wide = gradient2_mix_factor(-5.0, 10.0, 200.0);
        let right_at_narrow = gradient2_mix_factor(5.0, 10.0, 50.0);

        assert_eq!(left_at_default, 0.0);
        assert_eq!(left_at_wide, 0.25);
        assert_eq!(right_at_narrow, 1.0);
    }

    #[test]
    fn gradient4_width_percent_changes_mix_zone() {
        let left_at_default = gradient4_mix_factor(0.0, 100.0);
        let left_at_wide = gradient4_mix_factor(0.0, 200.0);
        let right_at_narrow = gradient4_mix_factor(1.0, 50.0);

        assert_eq!(left_at_default, 0.0);
        assert_eq!(left_at_wide, 0.25);
        assert_eq!(right_at_narrow, 1.0);
    }
}
