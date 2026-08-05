/*
File: src/tabs/typing/render_next/effects/gradients.rs

Purpose:
Градиентные post-effects нового рендера typing.

Main responsibilities:
- применять двухцветный и четырёхугловой градиенты к уже растеризованному тексту;
- переиспользовать общий alpha-bbox и fill-mode contract из `parse.rs`;
- считать область градиента (вся непрозрачная картинка либо только заменяемые
  пиксели) и допуск сравнения с заменяемым цветом;
- держать локальные тесты на math helper'ы градиентов.
*/

use super::super::types::RenderedTextImage;
use super::parse::{
    Gradient2EffectParams, Gradient2FillMode, Gradient4EffectParams, Gradient4FillMode,
    GradientAreaMode,
};
use rayon::prelude::*;

/// Applies the two-color gradient over the area selected by `gradient.area_mode`.
///
/// Only pixels accepted by the fill mode (and its color tolerance) are rewritten; the
/// ramp is stretched over the bounding box of either all non-transparent pixels or
/// only those replaced pixels.
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
    let threshold_sq = color_tolerance_threshold_sq(gradient.color_tolerance_percent);
    let bounds = match gradient.area_mode {
        GradientAreaMode::FullImage => alpha_bounds(source.as_slice(), width, height),
        // The ramp must span only what is actually replaced, so the bbox is measured
        // over the same predicate the fill loop below uses.
        GradientAreaMode::AffectedArea => {
            bounds_where(source.as_slice(), width, height, |idx| {
                should_replace_gradient2(source.as_slice(), idx, gradient, threshold_sq)
            })
        }
    };
    let Some((min_x, min_y, max_x, max_y)) = bounds else {
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
                if src_a == 0 || !should_replace_gradient2(&source, src_idx, gradient, threshold_sq)
                {
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

/// Applies the four-corner gradient over the area selected by `gradient.area_mode`.
///
/// Only pixels accepted by the fill mode (and its color tolerance) are rewritten; the
/// bilinear corner blend is stretched over the bounding box of either all
/// non-transparent pixels or only those replaced pixels.
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
    let threshold_sq = color_tolerance_threshold_sq(gradient.color_tolerance_percent);
    let bounds = match gradient.area_mode {
        GradientAreaMode::FullImage => alpha_bounds(source.as_slice(), width, height),
        // The corner blend must span only what is actually replaced, so the bbox is
        // measured over the same predicate the fill loop below uses.
        GradientAreaMode::AffectedArea => {
            bounds_where(source.as_slice(), width, height, |idx| {
                should_replace_gradient4(source.as_slice(), idx, gradient, threshold_sq)
            })
        }
    };
    let Some((min_x, min_y, max_x, max_y)) = bounds else {
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
                if src_a == 0 || !should_replace_gradient4(&source, src_idx, gradient, threshold_sq)
                {
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

/// Bounding box (`min_x`, `min_y`, `max_x`, `max_y`, inclusive) of every
/// non-transparent pixel, or `None` when the image is fully transparent.
fn alpha_bounds(source: &[u8], width: usize, height: usize) -> Option<(i32, i32, i32, i32)> {
    bounds_where(source, width, height, |_| true)
}

/// Bounding box (inclusive) of the non-transparent pixels `accept` returns `true` for.
///
/// `accept` receives the byte index of the pixel's red channel. Fully transparent
/// pixels are rejected before `accept` runs, so a predicate only has to judge color.
/// Returns `None` when no pixel qualifies.
fn bounds_where(
    source: &[u8],
    width: usize,
    height: usize,
    accept: impl Fn(usize) -> bool,
) -> Option<(i32, i32, i32, i32)> {
    let mut min_x = width as i32;
    let mut min_y = height as i32;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            if source[idx + 3] == 0 || !accept(idx) {
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

/// Longest possible distance between two RGB colors: the diagonal of the 0..=255 cube.
const MAX_RGB_DISTANCE: f32 = 441.672_96;

/// Converts a color tolerance in percent into a squared RGB distance threshold.
///
/// The percentage is a share of the RGB cube diagonal, so 0 % accepts only a
/// byte-exact color (`0.0` threshold, integer-exact in `f32`) and 100 % accepts any
/// color. Squared, so the per-pixel test needs no `sqrt`.
fn color_tolerance_threshold_sq(tolerance_percent: f32) -> f32 {
    let distance = (tolerance_percent.clamp(0.0, 100.0) / 100.0) * MAX_RGB_DISTANCE;
    distance * distance
}

/// Whether the pixel at `idx` is within `threshold_sq` of `target` in RGB space.
///
/// Alpha is deliberately ignored: antialiased text keeps its color and only varies in
/// alpha, so it must match its own target color at any coverage.
fn color_within_tolerance(source: &[u8], idx: usize, target: [u8; 4], threshold_sq: f32) -> bool {
    let dr = f32::from(source[idx]) - f32::from(target[0]);
    let dg = f32::from(source[idx + 1]) - f32::from(target[1]);
    let db = f32::from(source[idx + 2]) - f32::from(target[2]);
    dr.mul_add(dr, dg.mul_add(dg, db * db)) <= threshold_sq
}

/// Whether the two-color gradient replaces the pixel at `idx`.
///
/// `threshold_sq` comes from `color_tolerance_threshold_sq` and is only consulted in
/// `SpecificColor` mode.
fn should_replace_gradient2(
    source: &[u8],
    idx: usize,
    gradient: &Gradient2EffectParams,
    threshold_sq: f32,
) -> bool {
    match gradient.fill_mode {
        Gradient2FillMode::AllOpaque => true,
        Gradient2FillMode::SpecificColor => {
            color_within_tolerance(source, idx, gradient.target_color, threshold_sq)
        }
    }
}

/// Whether the four-corner gradient replaces the pixel at `idx`.
///
/// `threshold_sq` comes from `color_tolerance_threshold_sq` and is only consulted in
/// `SpecificColor` mode.
fn should_replace_gradient4(
    source: &[u8],
    idx: usize,
    gradient: &Gradient4EffectParams,
    threshold_sq: f32,
) -> bool {
    match gradient.fill_mode {
        Gradient4FillMode::AllOpaque => true,
        Gradient4FillMode::SpecificColor => {
            color_within_tolerance(source, idx, gradient.target_color, threshold_sq)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::{
        Gradient2EffectParams, Gradient2FillMode, Gradient4EffectParams, Gradient4FillMode,
        GradientAreaMode,
    };
    use super::{
        alpha_bounds, apply_gradient2_effect, apply_gradient4_effect, bounds_where,
        color_tolerance_threshold_sq, color_within_tolerance, gradient2_mix_factor,
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
            font_fallbacks: crate::types::FontFallbackReport::default(),
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
        let threshold_sq = color_tolerance_threshold_sq(gradient.color_tolerance_percent);
        let bounds = match gradient.area_mode {
            GradientAreaMode::FullImage => alpha_bounds(source.as_slice(), width, height),
            GradientAreaMode::AffectedArea => {
                bounds_where(source.as_slice(), width, height, |idx| {
                    should_replace_gradient2(source.as_slice(), idx, gradient, threshold_sq)
                })
            }
        };
        let Some((min_x, min_y, max_x, max_y)) = bounds else {
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
                if src_a == 0 || !should_replace_gradient2(&source, idx, gradient, threshold_sq) {
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
        let threshold_sq = color_tolerance_threshold_sq(gradient.color_tolerance_percent);
        let bounds = match gradient.area_mode {
            GradientAreaMode::FullImage => alpha_bounds(source.as_slice(), width, height),
            GradientAreaMode::AffectedArea => {
                bounds_where(source.as_slice(), width, height, |idx| {
                    should_replace_gradient4(source.as_slice(), idx, gradient, threshold_sq)
                })
            }
        };
        let Some((min_x, min_y, max_x, max_y)) = bounds else {
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
                if src_a == 0 || !should_replace_gradient4(&source, idx, gradient, threshold_sq) {
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
            color_tolerance_percent: 0.0,
            area_mode: GradientAreaMode::FullImage,
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
            color_tolerance_percent: 0.0,
            area_mode: GradientAreaMode::FullImage,
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

    /// Left half opaque black, right half opaque red — a stand-in for the "black text
    /// plus red text" case the affected-area mode exists for.
    fn two_block_image(width: usize, height: usize) -> RenderedTextImage {
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let idx = (y * width + x) * 4;
                if x >= width / 2 {
                    rgba[idx] = 255;
                }
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
            extra: crate::types::RenderedTextExtraInfo::default(),
            font_fallbacks: crate::types::FontFallbackReport::default(),
        }
    }

    fn red_replacing_gradient2(area_mode: GradientAreaMode) -> Gradient2EffectParams {
        Gradient2EffectParams {
            color1: [0, 0, 255, 255],
            color2: [0, 255, 0, 255],
            angle_deg: 0.0,
            width_percent: 100.0,
            fill_mode: Gradient2FillMode::SpecificColor,
            target_color: [255, 0, 0, 255],
            respect_source_alpha: false,
            color_tolerance_percent: 0.0,
            area_mode,
        }
    }

    fn pixel(image: &RenderedTextImage, x: usize, y: usize) -> [u8; 4] {
        let idx = (y * image.width as usize + x) * 4;
        [
            image.rgba[idx],
            image.rgba[idx + 1],
            image.rgba[idx + 2],
            image.rgba[idx + 3],
        ]
    }

    #[test]
    fn gradient2_affected_area_spans_only_replaced_pixels() {
        let mut full = two_block_image(20, 3);
        let mut affected = two_block_image(20, 3);
        apply_gradient2_effect(&mut full, &red_replacing_gradient2(GradientAreaMode::FullImage));
        apply_gradient2_effect(
            &mut affected,
            &red_replacing_gradient2(GradientAreaMode::AffectedArea),
        );

        // The black half is not the target color, so neither mode may touch it.
        assert_eq!(pixel(&full, 0, 1), [0, 0, 0, 255]);
        assert_eq!(pixel(&affected, 0, 1), [0, 0, 0, 255]);

        // Affected-area mode stretches the whole ramp over the red block alone.
        assert_eq!(pixel(&affected, 10, 1), [0, 0, 255, 255]);
        assert_eq!(pixel(&affected, 19, 1), [0, 255, 0, 255]);

        // Full-image mode maps the same block onto the tail of a ramp that starts at
        // the left edge of the picture, so its first red pixel is already mid-ramp.
        assert_eq!(pixel(&full, 19, 1), [0, 255, 0, 255]);
        assert_ne!(pixel(&full, 10, 1), [0, 0, 255, 255]);
    }

    #[test]
    fn gradient4_affected_area_spans_only_replaced_pixels() {
        let gradient = |area_mode| Gradient4EffectParams {
            color_top_left: [10, 20, 30, 255],
            color_top_right: [200, 0, 0, 255],
            color_bottom_left: [0, 200, 0, 255],
            color_bottom_right: [0, 0, 200, 255],
            width_percent: 100.0,
            fill_mode: Gradient4FillMode::SpecificColor,
            target_color: [255, 0, 0, 255],
            respect_source_alpha: false,
            color_tolerance_percent: 0.0,
            area_mode,
        };

        let mut full = two_block_image(20, 4);
        let mut affected = two_block_image(20, 4);
        apply_gradient4_effect(&mut full, &gradient(GradientAreaMode::FullImage));
        apply_gradient4_effect(&mut affected, &gradient(GradientAreaMode::AffectedArea));

        assert_eq!(pixel(&affected, 0, 0), [0, 0, 0, 255]);
        // The top-left corner of the red block is the top-left corner of the ramp.
        assert_eq!(pixel(&affected, 10, 0), [10, 20, 30, 255]);
        assert_ne!(pixel(&full, 10, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn gradient2_affected_area_without_matches_leaves_image_untouched() {
        let mut image = two_block_image(20, 3);
        let untouched = image.rgba.clone();
        let mut gradient = red_replacing_gradient2(GradientAreaMode::AffectedArea);
        gradient.target_color = [1, 2, 3, 255];

        apply_gradient2_effect(&mut image, &gradient);

        assert_eq!(image.rgba, untouched);
    }

    #[test]
    fn zero_tolerance_keeps_byte_exact_color_match() {
        let threshold_sq = color_tolerance_threshold_sq(0.0);
        let source = [200u8, 100, 50, 255, 200, 100, 51, 255];

        assert!(color_within_tolerance(&source, 0, [200, 100, 50, 255], threshold_sq));
        assert!(!color_within_tolerance(&source, 4, [200, 100, 50, 255], threshold_sq));
    }

    #[test]
    fn tolerance_accepts_colors_within_the_rgb_distance() {
        // Distance to the target is exactly 20, i.e. ~4.53 % of the cube diagonal.
        let source = [180u8, 0, 0, 255];
        let target = [200u8, 0, 0, 255];

        assert!(!color_within_tolerance(
            &source,
            0,
            target,
            color_tolerance_threshold_sq(4.0)
        ));
        assert!(color_within_tolerance(
            &source,
            0,
            target,
            color_tolerance_threshold_sq(5.0)
        ));
        // 100 % is the whole cube: even opposite corners match.
        assert!(color_within_tolerance(
            &[0u8, 0, 0, 255],
            0,
            [255, 255, 255, 255],
            color_tolerance_threshold_sq(100.0)
        ));
    }

    #[test]
    fn gradient2_tolerance_widens_the_replaced_set() {
        let mut image = two_block_image(6, 1);
        // Nudge one red pixel off the exact target color.
        image.rgba[5 * 4] = 235;

        let mut gradient = red_replacing_gradient2(GradientAreaMode::FullImage);
        gradient.color1 = [0, 0, 255, 255];
        gradient.color2 = [0, 0, 255, 255];

        let mut exact = RenderedTextImage {
            rgba: image.rgba.clone(),
            ..two_block_image(6, 1)
        };
        apply_gradient2_effect(&mut exact, &gradient);
        assert_eq!(pixel(&exact, 5, 0), [235, 0, 0, 255]);

        gradient.color_tolerance_percent = 10.0;
        apply_gradient2_effect(&mut image, &gradient);
        assert_eq!(pixel(&image, 5, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn bounds_where_narrows_to_the_accepted_pixels() {
        let image = two_block_image(8, 2);
        let source = image.rgba.as_slice();

        assert_eq!(alpha_bounds(source, 8, 2), Some((0, 0, 7, 1)));
        assert_eq!(
            bounds_where(source, 8, 2, |idx| source[idx] == 255),
            Some((4, 0, 7, 1))
        );
        assert_eq!(bounds_where(source, 8, 2, |_| false), None);
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
