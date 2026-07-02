/*
File: src/tabs/typing/render_next/raster.rs

Purpose:
Будущие низкоуровневые raster/image helper'ы нового рендера typing.

Main responsibilities:
- держать swash sampling и alpha blending вне pipeline;
- собрать общие pixel/image helper'ы для horizontal/vertical/formula режимов;
- стать целевым местом для переноса общих RGBA helper-функций из старого рендера.
*/

use super::pipeline::GlyphScaleSettings;
use super::types::RenderedTextImage;
use cosmic_text::SwashContent;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PixelBounds {
    pub(crate) min_x: i32,
    pub(crate) min_y: i32,
    pub(crate) max_x: i32,
    pub(crate) max_y: i32,
    pub(crate) initialized: bool,
}

impl PixelBounds {
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            min_x: 0,
            min_y: 0,
            max_x: 0,
            max_y: 0,
            initialized: false,
        }
    }

    pub(crate) fn include_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }

        let rect_max_x = x.saturating_add(width);
        let rect_max_y = y.saturating_add(height);
        if !self.initialized {
            self.min_x = x;
            self.min_y = y;
            self.max_x = rect_max_x;
            self.max_y = rect_max_y;
            self.initialized = true;
            return;
        }

        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(rect_max_x);
        self.max_y = self.max_y.max(rect_max_y);
    }
}

pub(crate) fn sample_swash_pixel(
    content: &SwashContent,
    data: &[u8],
    glyph_width: usize,
    x: usize,
    y: usize,
    text_color: [u8; 4],
) -> (u8, u8, u8, u8) {
    let pixel_idx = y.saturating_mul(glyph_width).saturating_add(x);
    let tint_alpha = f32::from(text_color[3]) / 255.0;
    match content {
        SwashContent::Mask | SwashContent::SubpixelMask => {
            let alpha = sample_swash_alpha(content, data, glyph_width, x, y);
            let out_a = (f32::from(alpha) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            (text_color[0], text_color[1], text_color[2], out_a)
        }
        SwashContent::Color => {
            let base = pixel_idx.saturating_mul(4);
            let r = data.get(base).copied().unwrap_or(u8::MAX);
            let g = data.get(base + 1).copied().unwrap_or(u8::MAX);
            let b = data.get(base + 2).copied().unwrap_or(u8::MAX);
            let a = sample_swash_alpha(content, data, glyph_width, x, y);
            let tint_r = ((u16::from(r) * u16::from(text_color[0])) / 255) as u8;
            let tint_g = ((u16::from(g) * u16::from(text_color[1])) / 255) as u8;
            let tint_b = ((u16::from(b) * u16::from(text_color[2])) / 255) as u8;
            let out_a = (f32::from(a) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            (tint_r, tint_g, tint_b, out_a)
        }
    }
}

#[must_use]
pub(crate) fn sample_swash_alpha(
    content: &SwashContent,
    data: &[u8],
    glyph_width: usize,
    x: usize,
    y: usize,
) -> u8 {
    let pixel_idx = y.saturating_mul(glyph_width).saturating_add(x);
    match content {
        SwashContent::Mask => data.get(pixel_idx).copied().unwrap_or(0),
        SwashContent::SubpixelMask => {
            let base = pixel_idx.saturating_mul(3);
            let r = data.get(base).copied().unwrap_or(0);
            let g = data.get(base + 1).copied().unwrap_or(0);
            let b = data.get(base + 2).copied().unwrap_or(0);
            r.max(g).max(b)
        }
        SwashContent::Color => {
            let base = pixel_idx.saturating_mul(4);
            data.get(base + 3).copied().unwrap_or(0)
        }
    }
}

pub(crate) fn blend_pixel_over(dst: &mut [u8], src_r: u8, src_g: u8, src_b: u8, src_a: u8) {
    let src_a_f = f32::from(src_a) / 255.0;
    if src_a_f <= f32::EPSILON {
        return;
    }

    let dst_r_f = f32::from(dst[0]) / 255.0;
    let dst_g_f = f32::from(dst[1]) / 255.0;
    let dst_b_f = f32::from(dst[2]) / 255.0;
    let dst_a_f = f32::from(dst[3]) / 255.0;

    let src_r_f = f32::from(src_r) / 255.0;
    let src_g_f = f32::from(src_g) / 255.0;
    let src_b_f = f32::from(src_b) / 255.0;

    let out_a = src_a_f + dst_a_f * (1.0 - src_a_f);
    if out_a <= f32::EPSILON {
        return;
    }

    let out_r = (src_r_f * src_a_f + dst_r_f * dst_a_f * (1.0 - src_a_f)) / out_a;
    let out_g = (src_g_f * src_a_f + dst_g_f * dst_a_f * (1.0 - src_a_f)) / out_a;
    let out_b = (src_b_f * src_a_f + dst_b_f * dst_a_f * (1.0 - src_a_f)) / out_a;

    dst[0] = (out_r * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[1] = (out_g * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[2] = (out_b * 255.0).round().clamp(0.0, 255.0) as u8;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[must_use]
pub(crate) fn is_cancelled(cancel: Option<(&Arc<AtomicU64>, u64)>) -> bool {
    cancel.is_some_and(|(token, expected)| token.load(Ordering::Acquire) != expected)
}

#[must_use]
pub(crate) fn build_glyph_rgba_buffer(
    content: &SwashContent,
    data: &[u8],
    glyph_w: usize,
    glyph_h: usize,
    text_color: [u8; 4],
) -> Vec<u8> {
    let mut out = vec![0u8; glyph_w.saturating_mul(glyph_h).saturating_mul(4)];
    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            let (r, g, b, a) = sample_swash_pixel(content, data, glyph_w, gx, gy, text_color);
            let idx = (gy * glyph_w + gx) * 4;
            out[idx] = r;
            out[idx + 1] = g;
            out[idx + 2] = b;
            out[idx + 3] = a;
        }
    }
    out
}

pub(crate) fn include_scaled_rect_bounds(
    bounds: &mut PixelBounds,
    left_px: f32,
    top_px: f32,
    width_px: f32,
    height_px: f32,
    glyph_scale: GlyphScaleSettings,
) {
    let (scaled_left, scaled_top, scaled_width, scaled_height) =
        glyph_scale.scaled_rect(left_px, top_px, width_px, height_px);
    bounds.include_rect(
        scaled_left.floor() as i32,
        scaled_top.floor() as i32,
        scaled_width.ceil() as i32,
        scaled_height.ceil() as i32,
    );
}

pub(crate) struct RgbaCanvasView<'a> {
    pub(crate) rgba: &'a mut [u8],
    pub(crate) width: usize,
    pub(crate) height: usize,
}

pub(crate) struct GlyphRgbaView<'a> {
    pub(crate) rgba: &'a [u8],
    pub(crate) width: usize,
    pub(crate) height: usize,
}

pub(crate) fn draw_scaled_glyph_rgba(
    canvas: &mut RgbaCanvasView<'_>,
    glyph: GlyphRgbaView<'_>,
    left_px: f32,
    top_px: f32,
    glyph_scale: GlyphScaleSettings,
) {
    let (scaled_left, scaled_top, scaled_width, scaled_height) =
        glyph_scale.scaled_rect(left_px, top_px, glyph.width as f32, glyph.height as f32);
    let dst_min_x = scaled_left.floor() as i32;
    let dst_max_x = (scaled_left + scaled_width).ceil() as i32;
    let dst_min_y = scaled_top.floor() as i32;
    let dst_max_y = (scaled_top + scaled_height).ceil() as i32;
    let src_center_x = glyph.width as f32 * 0.5;
    let src_center_y = glyph.height as f32 * 0.5;
    let scaled_center_x = scaled_left + scaled_width * 0.5;
    let scaled_center_y = scaled_top + scaled_height * 0.5;

    for dst_y in dst_min_y..dst_max_y {
        if dst_y < 0 || dst_y >= canvas.height as i32 {
            continue;
        }
        for dst_x in dst_min_x..dst_max_x {
            if dst_x < 0 || dst_x >= canvas.width as i32 {
                continue;
            }

            let local_x = ((dst_x as f32 + 0.5 - scaled_center_x) / glyph_scale.width_mul)
                + src_center_x
                - 0.5;
            let local_y = ((dst_y as f32 + 0.5 - scaled_center_y) / glyph_scale.height_mul)
                + src_center_y
                - 0.5;
            let (src_r, src_g, src_b, src_a) =
                bilinear_sample_rgba(glyph.rgba, glyph.width, glyph.height, local_x, local_y);
            if src_a == 0 {
                continue;
            }
            let dst_idx = ((dst_y as usize * canvas.width) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut canvas.rgba[dst_idx..dst_idx + 4],
                src_r,
                src_g,
                src_b,
                src_a,
            );
        }
    }
}

/// Включить в bounds повёрнутый (и масштабированный) прямоугольник глифа.
#[allow(clippy::too_many_arguments)]
pub(crate) fn include_rotated_rect_bounds(
    bounds: &mut PixelBounds,
    src_left: f32,
    src_top: f32,
    src_width: f32,
    src_height: f32,
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
) {
    let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
        src_left,
        src_top,
        src_width,
        src_height,
        dst_center_x,
        dst_center_y,
        rotation_rad,
    );
    let min_x_i = min_x.floor() as i32;
    let min_y_i = min_y.floor() as i32;
    let max_x_i = max_x.ceil() as i32;
    let max_y_i = max_y.ceil() as i32;
    bounds.include_rect(
        min_x_i,
        min_y_i,
        (max_x_i - min_x_i).max(1),
        (max_y_i - min_y_i).max(1),
    );
}

/// Мировой bbox прямоугольника, повёрнутого вокруг своего центра и помещённого в `dst_center`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rotated_rect_world_bounds(
    src_left: f32,
    src_top: f32,
    src_width: f32,
    src_height: f32,
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
) -> (f32, f32, f32, f32) {
    let src_center_x = src_left + src_width * 0.5;
    let src_center_y = src_top + src_height * 0.5;
    let corners = [
        (src_left, src_top),
        (src_left + src_width, src_top),
        (src_left + src_width, src_top + src_height),
        (src_left, src_top + src_height),
    ];
    let cos_a = rotation_rad.cos();
    let sin_a = rotation_rad.sin();
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
        let rel_x = x - src_center_x;
        let rel_y = y - src_center_y;
        let tx = dst_center_x + rel_x * cos_a - rel_y * sin_a;
        let ty = dst_center_y + rel_x * sin_a + rel_y * cos_a;
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }
    (min_x, min_y, max_x, max_y)
}

/// A glyph placement that can be rotated rigidly as part of a whole-block turn.
///
/// Implementors expose their world center and own rotation so the shared
/// `rotate_placements_about_centroid` pass can apply `global_rotation_deg`
/// identically on every layout mode (horizontal, vertical, formula/on-path,
/// custom lines). The rotation convention is the standard screen (y-down)
/// matrix `[cos -sin; sin cos]` on a positive angle — the SAME one used by the
/// Ctrl+wheel overlay post-rotation (`default_overlay_quad_scene`), so a
/// positive angle turns text the same visual direction, only crisper.
pub(crate) trait RigidPlacement {
    /// World-space rotation center (the point the placement is pinned to).
    fn placement_center(&self) -> (f32, f32);
    /// Replace the world-space center after the block rotation.
    fn set_placement_center(&mut self, x: f32, y: f32);
    /// Add `angle_rad` to the placement's own rotation.
    fn add_placement_rotation(&mut self, angle_rad: f32);
}

/// Rotate every placement rigidly about the shared centroid by `angle_rad`.
///
/// The centroid is the mean of all placement centers. Each center is rotated
/// about it and `angle_rad` is added to each placement's own rotation, so the
/// whole laid-out block turns as one rigid body. Empty input or a zero angle is
/// a no-op. The pivot (centroid) only translates the result; since the caller
/// grows the canvas to the rotated bounds and trims to alpha, the pivot choice
/// does not change the final trimmed pixels.
pub(crate) fn rotate_placements_about_centroid(
    mut placements: Vec<&mut dyn RigidPlacement>,
    angle_rad: f32,
) {
    if placements.is_empty() || angle_rad == 0.0 {
        return;
    }
    let count = placements.len() as f32;
    let (mut sum_x, mut sum_y) = (0.0f32, 0.0f32);
    for placement in &placements {
        let (x, y) = placement.placement_center();
        sum_x += x;
        sum_y += y;
    }
    let centroid_x = sum_x / count;
    let centroid_y = sum_y / count;
    let (sin_a, cos_a) = angle_rad.sin_cos();
    for placement in &mut placements {
        let (x, y) = placement.placement_center();
        let rel_x = x - centroid_x;
        let rel_y = y - centroid_y;
        placement.set_placement_center(
            centroid_x + rel_x * cos_a - rel_y * sin_a,
            centroid_y + rel_x * sin_a + rel_y * cos_a,
        );
        placement.add_placement_rotation(angle_rad);
    }
}

/// Нарисовать глиф с масштабом и поворотом вокруг точки `dst_center` (обратная выборка).
/// `src_left`/`src_top` — левый верх немасштабированного глифа в координатах контента;
/// `x_offset`/`y_offset` переводят координаты контента в пиксели холста.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_rotated_scaled_glyph_rgba(
    canvas: &mut RgbaCanvasView<'_>,
    glyph: GlyphRgbaView<'_>,
    src_left: f32,
    src_top: f32,
    glyph_scale: GlyphScaleSettings,
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
    x_offset: i32,
    y_offset: i32,
) {
    if glyph.width == 0 || glyph.height == 0 {
        return;
    }
    let glyph_w = glyph.width;
    let glyph_h = glyph.height;
    let src_center_x = src_left + glyph_w as f32 * 0.5;
    let src_center_y = src_top + glyph_h as f32 * 0.5;
    let cos_a = rotation_rad.cos();
    let sin_a = rotation_rad.sin();
    let (scaled_left, scaled_top, scaled_width, scaled_height) =
        glyph_scale.scaled_rect(src_left, src_top, glyph_w as f32, glyph_h as f32);
    let (min_x, min_y, max_x, max_y) = rotated_rect_world_bounds(
        scaled_left,
        scaled_top,
        scaled_width,
        scaled_height,
        dst_center_x,
        dst_center_y,
        rotation_rad,
    );
    let out_width = canvas.width as i32;
    let out_height = canvas.height as i32;
    let dst_min_x = ((min_x + x_offset as f32).floor() as i32 - 1).max(0);
    let dst_max_x = ((max_x + x_offset as f32).ceil() as i32 + 1).min(out_width);
    let dst_min_y = ((min_y + y_offset as f32).floor() as i32 - 1).max(0);
    let dst_max_y = ((max_y + y_offset as f32).ceil() as i32 + 1).min(out_height);
    for dst_y in dst_min_y..dst_max_y {
        for dst_x in dst_min_x..dst_max_x {
            let world_x = dst_x as f32 + 0.5 - x_offset as f32;
            let world_y = dst_y as f32 + 0.5 - y_offset as f32;
            let rel_x = world_x - dst_center_x;
            let rel_y = world_y - dst_center_y;
            let rotated_x = rel_x * cos_a + rel_y * sin_a;
            let rotated_y = -rel_x * sin_a + rel_y * cos_a;
            let src_x = src_center_x + rotated_x / glyph_scale.width_mul;
            let src_y = src_center_y + rotated_y / glyph_scale.height_mul;
            let local_x = src_x - src_left - 0.5;
            let local_y = src_y - src_top - 0.5;
            let (src_r, src_g, src_b, src_a) =
                bilinear_sample_rgba(glyph.rgba, glyph_w, glyph_h, local_x, local_y);
            if src_a == 0 {
                continue;
            }
            let dst_idx = ((dst_y as usize * canvas.width) + dst_x as usize) * 4;
            blend_pixel_over(
                &mut canvas.rgba[dst_idx..dst_idx + 4],
                src_r,
                src_g,
                src_b,
                src_a,
            );
        }
    }
}

// The glyph blit call site naturally carries raster target, glyph bitmap and draw position.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_unscaled_glyph(
    rgba: &mut [u8],
    out_width: u32,
    out_height: u32,
    content: SwashContent,
    data: &[u8],
    glyph_w: usize,
    glyph_h: usize,
    draw_x: i32,
    draw_y: i32,
    text_color: [u8; 4],
) {
    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            let dst_x = draw_x + i32::try_from(gx).unwrap_or(i32::MAX);
            let dst_y = draw_y + i32::try_from(gy).unwrap_or(i32::MAX);
            if dst_x < 0 || dst_y < 0 || dst_x >= out_width as i32 || dst_y >= out_height as i32 {
                continue;
            }

            let (src_r, src_g, src_b, src_a) =
                sample_swash_pixel(&content, data, glyph_w, gx, gy, text_color);
            if src_a == 0 {
                continue;
            }

            let dst_idx = ((dst_y as usize * out_width as usize) + dst_x as usize) * 4;
            blend_pixel_over(&mut rgba[dst_idx..dst_idx + 4], src_r, src_g, src_b, src_a);
        }
    }
}

#[must_use]
pub(crate) fn trim_rendered_image_to_alpha_bounds(
    image: RenderedTextImage,
    keep_pad_px: u32,
) -> RenderedTextImage {
    if image.width == 0 || image.height == 0 {
        return image;
    }

    let width = image.width as usize;
    let height = image.height as usize;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut found = false;

    for y in 0..height {
        for x in 0..width {
            let alpha = image.rgba[(y * width + x) * 4 + 3];
            if alpha == 0 {
                continue;
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            found = true;
        }
    }

    if !found {
        return image;
    }

    let keep_pad = keep_pad_px as usize;
    let crop_min_x = min_x.saturating_sub(keep_pad);
    let crop_min_y = min_y.saturating_sub(keep_pad);
    let crop_max_x = max_x.saturating_add(keep_pad).min(width.saturating_sub(1));
    let crop_max_y = max_y.saturating_add(keep_pad).min(height.saturating_sub(1));
    let crop_width = crop_max_x.saturating_sub(crop_min_x).saturating_add(1);
    let crop_height = crop_max_y.saturating_sub(crop_min_y).saturating_add(1);

    if crop_width == width && crop_height == height {
        return image;
    }

    let mut rgba = vec![0u8; crop_width.saturating_mul(crop_height).saturating_mul(4)];
    for y in 0..crop_height {
        let src_start = ((crop_min_y + y) * width + crop_min_x) * 4;
        let src_end = src_start + crop_width * 4;
        let dst_start = y * crop_width * 4;
        let dst_end = dst_start + crop_width * 4;
        rgba[dst_start..dst_end].copy_from_slice(&image.rgba[src_start..src_end]);
    }

    RenderedTextImage {
        width: crop_width as u32,
        height: crop_height as u32,
        rgba,
        warnings: image.warnings,
        content_origin_x: 0,
        content_origin_y: 0,
    }
}

pub(crate) fn bilinear_sample_rgba(
    rgba: &[u8],
    width: usize,
    height: usize,
    x: f32,
    y: f32,
) -> (u8, u8, u8, u8) {
    if width == 0 || height == 0 {
        return (0, 0, 0, 0);
    }
    if x < -0.5 || y < -0.5 || x > width as f32 - 0.5 || y > height as f32 - 0.5 {
        return (0, 0, 0, 0);
    }

    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = x0 + 1;
    let y1 = y0 + 1;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let c00 = rgba_pixel_at(rgba, width, height, x0, y0);
    let c10 = rgba_pixel_at(rgba, width, height, x1, y0);
    let c01 = rgba_pixel_at(rgba, width, height, x0, y1);
    let c11 = rgba_pixel_at(rgba, width, height, x1, y1);

    let w00 = (1.0 - fx) * (1.0 - fy);
    let w10 = fx * (1.0 - fy);
    let w01 = (1.0 - fx) * fy;
    let w11 = fx * fy;

    let mut out = [0u8; 4];
    for channel in 0..4 {
        let value = f32::from(c00[channel]) * w00
            + f32::from(c10[channel]) * w10
            + f32::from(c01[channel]) * w01
            + f32::from(c11[channel]) * w11;
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    (out[0], out[1], out[2], out[3])
}

fn rgba_pixel_at(rgba: &[u8], width: usize, height: usize, x: i32, y: i32) -> [u8; 4] {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return [0, 0, 0, 0];
    }

    let idx = ((y as usize * width) + x as usize) * 4;
    [
        *rgba.get(idx).unwrap_or(&0),
        *rgba.get(idx + 1).unwrap_or(&0),
        *rgba.get(idx + 2).unwrap_or(&0),
        *rgba.get(idx + 3).unwrap_or(&0),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        RigidPlacement, rotate_placements_about_centroid, sample_swash_alpha,
        trim_rendered_image_to_alpha_bounds,
    };
    use crate::types::RenderedTextImage;
    use cosmic_text::SwashContent;

    /// Minimal `RigidPlacement` for exercising the centroid rotation math.
    struct TestPlacement {
        x: f32,
        y: f32,
        rot: f32,
    }

    impl RigidPlacement for TestPlacement {
        fn placement_center(&self) -> (f32, f32) {
            (self.x, self.y)
        }
        fn set_placement_center(&mut self, x: f32, y: f32) {
            self.x = x;
            self.y = y;
        }
        fn add_placement_rotation(&mut self, angle_rad: f32) {
            self.rot += angle_rad;
        }
    }

    #[test]
    fn rotate_placements_about_centroid_turns_block_90_degrees() {
        // Two points on a horizontal line about centroid (1,0). A +90° screen
        // rotation (y-down matrix) maps the horizontal pair to a vertical pair
        // and adds the angle to each own rotation.
        let mut a = TestPlacement { x: 0.0, y: 0.0, rot: 0.0 };
        let mut b = TestPlacement { x: 2.0, y: 0.0, rot: 0.0 };
        let angle = std::f32::consts::FRAC_PI_2;
        rotate_placements_about_centroid(
            vec![&mut a as &mut dyn RigidPlacement, &mut b as &mut dyn RigidPlacement],
            angle,
        );
        // Centroid (1,0): a=(-1,0)->(1,-1), b=(1,0)->(1,1) under [cos -sin; sin cos].
        assert!((a.x - 1.0).abs() < 1e-5 && (a.y - -1.0).abs() < 1e-5, "a={:?}", (a.x, a.y));
        assert!((b.x - 1.0).abs() < 1e-5 && (b.y - 1.0).abs() < 1e-5, "b={:?}", (b.x, b.y));
        assert!((a.rot - angle).abs() < 1e-5 && (b.rot - angle).abs() < 1e-5);
    }

    #[test]
    fn rotate_placements_about_centroid_zero_angle_is_noop() {
        let mut a = TestPlacement { x: 3.0, y: 5.0, rot: 0.25 };
        rotate_placements_about_centroid(vec![&mut a as &mut dyn RigidPlacement], 0.0);
        assert_eq!((a.x, a.y, a.rot), (3.0, 5.0, 0.25));
    }

    #[test]
    fn sample_swash_alpha_handles_subpixel_content() {
        let alpha = sample_swash_alpha(&SwashContent::SubpixelMask, &[10, 25, 18], 1, 0, 0);
        assert_eq!(alpha, 25);
    }

    #[test]
    fn trim_rendered_image_crops_transparent_edges() {
        let image = RenderedTextImage {
            content_origin_x: 0,
            content_origin_y: 0,
            width: 4,
            height: 3,
            rgba: vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
                0, 0, 0, 0, 1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, //
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
            ],
            warnings: vec!["kept".to_string()],
        };

        let trimmed = trim_rendered_image_to_alpha_bounds(image, 0);
        assert_eq!(trimmed.width, 2);
        assert_eq!(trimmed.height, 1);
        assert_eq!(trimmed.warnings, vec!["kept".to_string()]);
    }
}
