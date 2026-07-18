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
    EDT_COST_INF, dilate_alpha_max_filter3, euclidean_distance_transform_with_costs,
    gaussian_blur_alpha_f32_in_place, gaussian_blur_alpha_in_place, gaussian_blur_kernel_radius,
};
use super::parse::{GlowEffectParams, SoftGlowEffectParams, StrokeOpacityMode};
use rayon::prelude::*;

/// Applies the legacy disc-splat contour glow (`glow_v1`).
///
/// Precomputes an integer disc of `(dx, dy, falloff)` offsets, splats each source-contour
/// pixel's glow contribution into a glow-only alpha field with `max`, then composites the
/// glow color under the source text. The glow-only alpha is held in `f32` end to end (no
/// per-offset or intermediate `u8` rounding) and Gaussian-blurred with a small sigma before
/// compositing, so the disc-quantized iso-distance plateaus no longer band; a single `u8`
/// rounding happens at composite time. This is the legacy variant: unlike `glow_v2` it does
/// NOT use sub-pixel EDT seeding — offsets are quantized to the integer grid by construction.
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

    // Small blur removes the ~1px iso-distance plateaus; sigma scales gently with radius so
    // large glows stay smooth without visibly shrinking. Pad by the glow reach plus the blur
    // kernel half-width so the blur tail is not clipped at the canvas rim.
    let sigma = glow_smoothing_sigma(radius);
    let blur_pad = gaussian_blur_kernel_radius(sigma);
    let pad = (radius.ceil().max(1.0) as u32).saturating_add(blur_pad);
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
    // Glow-only intensity in [0, 1]; kept in f32 through splat + blur, rounded once at composite.
    let mut glow_alpha = vec![0.0f32; out_width as usize * out_height as usize];
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

                let idx = ty as usize * out_width as usize + tx as usize;
                glow_alpha[idx] = glow_alpha[idx].max(alpha_f);
            }
        }
    }

    // Smooth the plateau-structured glow field before compositing (deterministic, in f32).
    gaussian_blur_alpha_f32_in_place(&mut glow_alpha, out_width, out_height, sigma);

    // Each output pixel composites its own glow color from read-only `glow_alpha` and
    // `source_alpha_expanded`, so the glow layer is parallelized per pixel. Overlap and the
    // color-alpha factor are applied here, after the blur, with a single final u8 rounding.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let intensity = glow_alpha[idx];
        if intensity <= 0.0 {
            return;
        }
        let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
        let glow_a = (intensity * overlap * color_alpha_factor * 255.0)
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

/// Applies the EDT-based contour glow (`glow_v2`).
///
/// Seeds a sub-pixel cost field from source alpha (fully opaque → `0.0`, partially covered →
/// `d0*d0` with `d0 = (0.5 - a/255).max(0.0)` approximating the pixel-center-to-edge sub-pixel
/// distance, empty → non-seed), runs a Felzenszwalb-Huttenlocher EDT (evaluated in `f32`),
/// maps the distance
/// through `glow_falloff_alpha`, then Gaussian-blurs the glow-only alpha with a small sigma
/// before compositing. Sub-pixel seeding breaks the integer iso-distance plateaus and the blur
/// removes any residual ~1px banding; overlap and the color-alpha factor are applied after the
/// blur with a single final `u8` rounding. The hard `dist2 > radius^2` cutoff is kept — the
/// blur softens its rim.
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

    // See `apply_glow_effect_v1` for the sigma/padding rationale (identical formula).
    let sigma = glow_smoothing_sigma(radius);
    let blur_pad = gaussian_blur_kernel_radius(sigma);
    let pad = (radius.ceil().max(1.0) as u32).saturating_add(blur_pad);
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
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;
    let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
    // Sub-pixel squared-distance cost field: non-seed pixels stay at EDT_COST_INF.
    let mut cost_field = vec![EDT_COST_INF; out_width_usize * out_height_usize];
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
            let base_idx = base_y as usize * out_width_usize + base_x as usize;
            source_alpha_expanded[base_idx] = src_a;
            // Approximate the sub-pixel distance from the pixel center to the true glyph edge:
            // fully covered (a=255) sits on the edge (0.0); partial coverage pushes the edge
            // outward by up to half a pixel, so the seed carries a small squared-distance cost.
            let coverage = src_a as f32 / 255.0;
            let d0 = (0.5 - coverage).max(0.0);
            cost_field[base_idx] = d0 * d0;
            has_contour = true;
        }
    }
    if !has_contour {
        return;
    }

    let dist2_map =
        euclidean_distance_transform_with_costs(&cost_field, out_width_usize, out_height_usize);
    let radius2 = radius * radius;

    let base_opacity = match glow.opacity_mode {
        StrokeOpacityMode::FromContour => 1.0,
        StrokeOpacityMode::Static => static_opacity,
    };

    // Glow-only intensity in [0, 1] from the distance falloff, kept in f32 for the blur.
    let mut glow_alpha = vec![0.0f32; out_width_usize * out_height_usize];
    for (idx, slot) in glow_alpha.iter_mut().enumerate() {
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
        *slot = base_opacity * falloff;
    }

    // Smooth the (now sub-pixel-seeded) glow field before compositing (deterministic, in f32).
    gaussian_blur_alpha_f32_in_place(&mut glow_alpha, out_width, out_height, sigma);

    // Each output pixel composites its glow color from the read-only glow field and expanded
    // source alpha, so the glow layer is parallelized per pixel. Overlap and the color-alpha
    // factor are applied here, after the blur, with a single final u8 rounding.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let intensity = glow_alpha[idx];
        if intensity <= 0.0 {
            return;
        }
        let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
        let glow_a = (intensity * overlap * color_alpha_factor * 255.0)
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

/// Gaussian sigma (in pixels) for the post-glow smoothing blur applied by `glow_v1`/`glow_v2`.
///
/// The iso-distance plateaus this blur removes are ~1px wide, so a ~1px sigma suffices; the
/// value scales gently with `radius` and is clamped to `[0.8, 2.0]` so large glows stay smooth
/// without the blur visibly changing the glow extent. Shared by both variants so their padding
/// and smoothing stay consistent.
fn glow_smoothing_sigma(radius: f32) -> f32 {
    (radius / 8.0).clamp(0.8, 2.0)
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
            extra: crate::types::RenderedTextExtraInfo::default(),
        }
    }

    /// Verbatim sequential reference of `apply_glow_effect_v1`: identical body with the
    /// `out.par_chunks_mut(4).for_each(...)` glow composite pass replaced by a plain per-pixel
    /// loop. Asserts the rayon path is bit-identical to the pre-parallelization loop.
    fn apply_glow_effect_v1_seq(image: &mut RenderedTextImage, glow: &GlowEffectParams) {
        use super::super::super::raster::blend_pixel_over;
        use super::super::image_ops::{gaussian_blur_alpha_f32_in_place, gaussian_blur_kernel_radius};
        use super::{blend_source_text, glow_falloff_alpha, glow_smoothing_sigma};

        let radius = glow.radius_px.max(0.0);
        if radius <= f32::EPSILON {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let sigma = glow_smoothing_sigma(radius);
        let blur_pad = gaussian_blur_kernel_radius(sigma);
        let pad = (radius.ceil().max(1.0) as u32).saturating_add(blur_pad);
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
        let mut glow_alpha = vec![0.0f32; out_width as usize * out_height as usize];
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
                    let idx = ty as usize * out_width as usize + tx as usize;
                    glow_alpha[idx] = glow_alpha[idx].max(alpha_f);
                }
            }
        }
        gaussian_blur_alpha_f32_in_place(&mut glow_alpha, out_width, out_height, sigma);
        for (idx, dst) in out.chunks_mut(4).enumerate() {
            let intensity = glow_alpha[idx];
            if intensity <= 0.0 {
                continue;
            }
            let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
            let glow_a = (intensity * overlap * color_alpha_factor * 255.0)
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
        use super::super::image_ops::{
            EDT_COST_INF, euclidean_distance_transform_with_costs, gaussian_blur_alpha_f32_in_place,
            gaussian_blur_kernel_radius,
        };
        use super::{blend_source_text, glow_falloff_alpha, glow_smoothing_sigma};

        let radius = glow.radius_px.max(0.0);
        if radius <= f32::EPSILON {
            return;
        }
        let width = image.width as usize;
        let height = image.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        let sigma = glow_smoothing_sigma(radius);
        let blur_pad = gaussian_blur_kernel_radius(sigma);
        let pad = (radius.ceil().max(1.0) as u32).saturating_add(blur_pad);
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
        let out_width_usize = out_width as usize;
        let out_height_usize = out_height as usize;
        let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
        let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
        let mut cost_field = vec![EDT_COST_INF; out_width_usize * out_height_usize];
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
                let base_idx = base_y as usize * out_width_usize + base_x as usize;
                source_alpha_expanded[base_idx] = src_a;
                let coverage = src_a as f32 / 255.0;
                let d0 = (0.5 - coverage).max(0.0);
                cost_field[base_idx] = d0 * d0;
                has_contour = true;
            }
        }
        if !has_contour {
            return;
        }
        let dist2_map =
            euclidean_distance_transform_with_costs(&cost_field, out_width_usize, out_height_usize);
        let radius2 = radius * radius;
        let base_opacity = match glow.opacity_mode {
            StrokeOpacityMode::FromContour => 1.0,
            StrokeOpacityMode::Static => static_opacity,
        };
        let mut glow_alpha = vec![0.0f32; out_width_usize * out_height_usize];
        for (idx, slot) in glow_alpha.iter_mut().enumerate() {
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
            *slot = base_opacity * falloff;
        }
        gaussian_blur_alpha_f32_in_place(&mut glow_alpha, out_width, out_height, sigma);
        for (idx, dst) in out.chunks_mut(4).enumerate() {
            let intensity = glow_alpha[idx];
            if intensity <= 0.0 {
                continue;
            }
            let overlap = 1.0 - (source_alpha_expanded[idx] as f32 / 255.0);
            let glow_a = (intensity * overlap * color_alpha_factor * 255.0)
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

    /// Default-falloff, radius-16 glow used by the smoothness goldens: opaque white so the
    /// composited alpha directly reflects the glow-only alpha (no color-alpha attenuation),
    /// `FromContour` with zero transparency, and the linear falloff (`fade_*` = 0).
    fn smoothness_glow_params() -> GlowEffectParams {
        GlowEffectParams {
            radius_px: 16.0,
            color: [255, 255, 255, 255],
            opacity_mode: StrokeOpacityMode::FromContour,
            transparency_percent: 0.0,
            fade_strength: 0.0,
            fade_shift: 0.0,
        }
    }

    /// Builds an 80x60 canvas with a solid opaque 40x20 block; with `aa`, the block gets a
    /// 1px antialiased fringe (alpha 96) so partial coverage reaches the glow seeding.
    fn block_image(aa: bool) -> (RenderedTextImage, usize, usize, usize, usize) {
        let width = 80usize;
        let height = 60usize;
        let (bx0, bx1, by0, by1) = (20usize, 60usize, 20usize, 40usize);
        let mut rgba = vec![0u8; width * height * 4];
        for y in by0..by1 {
            for x in bx0..bx1 {
                let idx = (y * width + x) * 4;
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
        if aa {
            // 1px fringe alpha 96 around the solid core.
            for y in (by0 - 1)..=by1 {
                for x in (bx0 - 1)..=bx1 {
                    let inside = x >= bx0 && x < bx1 && y >= by0 && y < by1;
                    if inside {
                        continue;
                    }
                    let idx = (y * width + x) * 4;
                    rgba[idx] = 255;
                    rgba[idx + 1] = 255;
                    rgba[idx + 2] = 255;
                    rgba[idx + 3] = 96;
                }
            }
        }
        let img = RenderedTextImage {
            width: width as u32,
            height: height as u32,
            rgba,
            warnings: Vec::new(),
            content_origin_x: 0,
            content_origin_y: 0,
            extra: crate::types::RenderedTextExtraInfo::default(),
        };
        (img, bx0, bx1, by0, by1)
    }

    fn ray(img: &RenderedTextImage, start: (usize, usize), step: (usize, usize), n: usize) -> Vec<u8> {
        let ow = img.width as usize;
        let oh = img.height as usize;
        let mut out = Vec::new();
        let (mut x, mut y) = start;
        for _ in 0..n {
            if x >= ow || y >= oh {
                break;
            }
            out.push(img.rgba[(y * ow + x) * 4 + 3]);
            x += step.0;
            y += step.1;
        }
        out
    }

    /// Asserts one sampled alpha profile is a smooth outward ramp.
    ///
    /// `slope_bound` is the max allowed adjacent-pixel delta and `curvature_bound` the max
    /// allowed second difference; see `assert_glow_profiles_smooth` for how both are derived.
    fn assert_profile_smooth(name: &str, samples: &[u8], slope_bound: u8, curvature_bound: u32) {
        assert!(samples.len() >= 14, "{name}: glow ray too short: {samples:?}");

        // (a) Monotone non-increasing within ±1 alpha level: distance from a convex source
        // grows strictly along any outward ray, so the falloff may never step back up.
        for pair in samples.windows(2) {
            assert!(
                pair[1] <= pair[0].saturating_add(1),
                "{name}: alpha profile not monotone non-increasing: {samples:?}"
            );
        }

        // (b) Adjacent-pixel delta bounded by the ideal ramp slope plus a 2-level margin.
        let max_delta = samples
            .windows(2)
            .map(|pair| pair[0].abs_diff(pair[1]))
            .max()
            .unwrap_or(0);
        assert!(
            max_delta <= slope_bound,
            "{name}: adjacent-alpha delta {max_delta} exceeds slope bound {slope_bound}: \
             {samples:?}"
        );

        // (c) Second difference (discrete curvature) — the assertion that actually catches
        // the pre-fix banding (see assert_glow_profiles_smooth for the measured numbers).
        let max_d2 = samples
            .windows(3)
            .map(|w| (i32::from(w[0]) - 2 * i32::from(w[1]) + i32::from(w[2])).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_d2 <= curvature_bound,
            "{name}: profile curvature {max_d2} exceeds bound {curvature_bound} \
             (plateau-then-jump / falloff kink banding): {samples:?}"
        );
    }

    /// Smoothness golden for a glow variant at the default falloff and radius 16.
    ///
    /// Renders `apply` around the `block_image(aa)` block and samples the composited alpha
    /// along two rays that start just outside all source alpha (including the AA fringe):
    /// horizontal from the right edge at the block's center row, and diagonal (+1,+1) from
    /// past the bottom-right corner. Per ray it asserts monotonicity, a slope bound, and a
    /// curvature bound.
    ///
    /// Numeric justification (all values measured on this exact scene, radius 16, opaque
    /// white glow, linear falloff):
    /// - The mean ramp slope is fixed by "255 alpha over 16 px": 255/16 ≈ 16 per horizontal
    ///   sample and 255·√2/16 ≈ 23 per diagonal sample (√2 px spacing). Both the pre-fix and
    ///   the fixed pipeline ride this slope (measured max deltas 16 and 22..23), so the slope
    ///   bounds are ceil + 2 margin: 18 horizontal, 25 diagonal. A plateau-then-jump band
    ///   would need a jump of ~2 bands ≈ 32/46 to show up here — the real 1D fingerprint of
    ///   the banding is curvature, below.
    /// - Curvature: the pre-fix pipeline ends its linear falloff with a hard kink at the
    ///   `dist > radius` cutoff — the outermost visible ridge ring. Measured pre-fix maximum
    ///   second difference: 16 on BOTH rays for v2 (hard and AA blocks alike) and for v1 on
    ///   the hard block (its AA fringe partially fills the rim: 6/11). The post-blur pipeline
    ///   measures 4 (horizontal) and 7 (diagonal). Bounds sit between the two populations
    ///   with margin on each side: 8 horizontal, 10 diagonal — the pre-fix algorithm fails
    ///   both, the fixed one passes with headroom.
    fn assert_glow_profiles_smooth(aa: bool, apply: impl Fn(&mut RenderedTextImage)) {
        let (mut image, _bx0, bx1, by0, by1) = block_image(aa);
        apply(&mut image);

        let ox = image.content_origin_x as usize;
        let oy = image.content_origin_y as usize;
        let mid_row = oy + (by0 + by1) / 2;

        // Both rays start one pixel past the block bounds so even the AA fringe (which the
        // composite darkens by source overlap) stays out of the pure-glow samples.
        let horiz = ray(&image, (ox + bx1 + 1, mid_row), (1, 0), 20);
        let diag = ray(&image, (ox + bx1 + 1, oy + by1 + 1), (1, 1), 16);

        let horiz_slope_bound = (255.0f32 / 16.0).ceil() as u8 + 2; // 18
        let diag_slope_bound = (255.0f32 * std::f32::consts::SQRT_2 / 16.0).ceil() as u8 + 2; // 25
        assert_profile_smooth("horizontal", &horiz, horiz_slope_bound, 8);
        assert_profile_smooth("diagonal", &diag, diag_slope_bound, 10);
    }

    /// v2 smoothness golden on the ANTIALIASED block, so the fractional-cost EDT seeding is
    /// exercised (fringe alpha 96 → d0 = 0.5 - 96/255 ≈ 0.12, cost ≈ 0.015). The pre-fix v2
    /// fails the curvature bounds on this same scene (measured 16 vs bounds 8/10).
    #[test]
    fn glow_v2_alpha_profile_is_smooth() {
        let glow = smoothness_glow_params();
        assert_glow_profiles_smooth(true, |image| apply_glow_effect_v2(image, &glow));
    }

    /// v1 smoothness golden on the HARD-EDGED block: v1 has no sub-pixel seeding to exercise,
    /// and the hard edge maximizes the pre-fix rim kink (measured pre-fix curvature 16 on both
    /// rays vs bounds 8/10; an AA fringe would soften the old kink to 6/11 and weaken the
    /// regression signal).
    #[test]
    fn glow_v1_alpha_profile_is_smooth() {
        let glow = smoothness_glow_params();
        assert_glow_profiles_smooth(false, |image| apply_glow_effect_v1(image, &glow));
    }
}
