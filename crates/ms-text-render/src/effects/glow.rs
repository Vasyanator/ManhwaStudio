/*
File: src/tabs/typing/render_next/effects/glow.rs

Purpose:
Glow-based post-effects нового рендера typing.

Main responsibilities:
- применять contour glow двух вариантов и soft outline glow;
- держать glow falloff math рядом с glow-реализациями;
- переиспользовать EDT/dilate/blur helper'ы из `image_ops`.

Notes:
Soft glow owns two pieces of math kept here next to its only caller: the per-side
dilation dispatch (rectangle vs. ellipse element, `image_ops`) and
`soft_glow_response_curve`, the bias/knee remap applied to the blurred outline. The
curve is the identity at bias 0, which is what keeps pre-curve projects unchanged.
*/

use super::super::raster::blend_pixel_over;
use super::super::types::RenderedTextImage;
use super::image_ops::{
    EDT_COST_INF, dilate_alpha_ellipse, dilate_alpha_rect, euclidean_distance_transform_with_costs,
    gaussian_blur_alpha_f32_in_place, gaussian_blur_kernel_radius,
};
use super::parse::{GlowEffectParams, SoftGlowEffectParams, SoftGlowShape, StrokeOpacityMode};
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
///
/// The glow layer carries its own alpha, unreduced by the source alpha; the source is then
/// composited over it, which is what accounts for the source coverage. See the alpha contract
/// on `blend_source_text`.
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

    // Each output pixel composites its own glow color from the read-only `glow_alpha`, so the
    // glow layer is parallelized per pixel; the color-alpha factor and the single final u8
    // rounding happen here, after the blur. The glow alpha is NOT reduced by the source alpha:
    // `blend_source_text` puts the source over this layer right after, which already accounts
    // for the source coverage exactly once (see the `overlap` note on `blend_source_text`).
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let intensity = glow_alpha[idx];
        if intensity <= 0.0 {
            return;
        }
        let glow_a = (intensity * color_alpha_factor * 255.0)
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
/// removes any residual ~1px banding; the color-alpha factor is applied after the blur with a
/// single final `u8` rounding. The hard `dist2 > radius^2` cutoff is kept — the blur softens
/// its rim.
///
/// As in `glow_v1`, the glow layer is not reduced by the source alpha — see the alpha contract
/// on `blend_source_text`.
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

    // Each output pixel composites its glow color from the read-only glow field, so the glow
    // layer is parallelized per pixel; the color-alpha factor and the single final u8 rounding
    // happen here, after the blur. As in `glow_v1`, the glow alpha is NOT reduced by the source
    // alpha — `blend_source_text` accounts for the source coverage right after.
    out.par_chunks_mut(4).enumerate().for_each(|(idx, dst)| {
        let intensity = glow_alpha[idx];
        if intensity <= 0.0 {
            return;
        }
        let glow_a = (intensity * color_alpha_factor * 255.0)
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

/// Applies the soft outline glow (`soft_glow`).
///
/// Dilates the source alpha by the four per-side extents (`SoftGlowEffectParams::extents`)
/// with either the rectangle or the ellipse element, subtracts the source to get the outline
/// ring, blurs that ring with `blur_radius_px`, remaps it through `soft_glow_response_curve`,
/// and composites the result under the source text. The outline is held in `f32` from the
/// subtraction through the blur and the curve, with a single `u8` rounding at composite time,
/// so the ring does not band (the same pattern `glow_v1`/`glow_v2` use).
///
/// The canvas grows PER SIDE — each side by its own extent plus the blur kernel half-width —
/// so an asymmetric glow is not clipped and does not waste margin on the opposite side;
/// `content_origin_x/y` therefore advance by the LEFT and TOP padding respectively.
/// Returns without touching the image when all four extents are zero, when the glow color is
/// fully transparent, or when the image (or the padded canvas) is empty.
///
/// The glow layer is deliberately NOT reduced by the source alpha (no `shaped - source_alpha`
/// term): this is the behavior every persisted soft-glow effect was authored against, and
/// subtracting would visibly change antialiased glyph rims (partial coverage there is worth up
/// to ~64/255 alpha). Nothing is lost visually because `blend_source_text` composites the
/// source on top of the glow afterwards. `glow_v1`/`glow_v2` follow the same rule.
///
/// Cost note: `Round` dilation is `O(width * height * (up + down + 1))` while `Square` is
/// `O(width * height)`; a very large round radius therefore runs for seconds and cannot be
/// interrupted, since cancellation is only checked between effects.
pub(crate) fn apply_soft_glow_effect(image: &mut RenderedTextImage, glow: &SoftGlowEffectParams) {
    let (left, right, up, down) = glow.extents();
    if left == 0 && right == 0 && up == 0 && down == 0 {
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

    // Per-side padding: the dilation reach on that side plus the blur tail, so neither the
    // ring nor its blur is clipped at the canvas rim.
    let blur_sigma = glow.blur_radius_px.max(0.0);
    let blur_pad = gaussian_blur_kernel_radius(blur_sigma);
    let pad_left = left.saturating_add(blur_pad);
    let pad_right = right.saturating_add(blur_pad);
    let pad_top = up.saturating_add(blur_pad);
    let pad_bottom = down.saturating_add(blur_pad);
    let out_width = image
        .width
        .saturating_add(pad_left)
        .saturating_add(pad_right);
    let out_height = image
        .height
        .saturating_add(pad_top)
        .saturating_add(pad_bottom);
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;

    let source = image.rgba.clone();
    let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
    let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
    let origin_x = pad_left as i32;
    let origin_y = pad_top as i32;

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
    match glow.shape {
        SoftGlowShape::Square => dilate_alpha_rect(
            dilated.as_mut_slice(),
            out_width_usize,
            out_height_usize,
            left as usize,
            right as usize,
            up as usize,
            down as usize,
        ),
        SoftGlowShape::Round => dilate_alpha_ellipse(
            dilated.as_mut_slice(),
            out_width_usize,
            out_height_usize,
            left as usize,
            right as usize,
            up as usize,
            down as usize,
        ),
    }

    // Outline ring normalized to [0, 1] and kept in f32 through the blur and the curve.
    let mut outline: Vec<f32> = dilated
        .iter()
        .zip(source_alpha_expanded.iter())
        .map(|(&dilated_a, &source_a)| f32::from(dilated_a.saturating_sub(source_a)) / 255.0)
        .collect();
    if blur_sigma > f32::EPSILON {
        gaussian_blur_alpha_f32_in_place(&mut outline, out_width, out_height, blur_sigma);
    }

    // Each output pixel composites the soft-glow color from its own read-only outline value,
    // so the glow layer is parallelized per pixel; the curve, the color-alpha factor, and the
    // single u8 rounding all happen here.
    out.par_chunks_mut(4)
        .zip(outline.par_iter())
        .for_each(|(dst, &outline_value)| {
            let shaped = soft_glow_response_curve(outline_value, glow.bias, glow.knee);
            let glow_a = (shaped * color_alpha_factor * 255.0)
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
    // Контент сдвинут на левый/верхний pad внутри увеличенного буфера (padding асимметричен).
    image.content_origin_x = image.content_origin_x.saturating_add(pad_left);
    image.content_origin_y = image.content_origin_y.saturating_add(pad_top);
}

/// Remaps a normalized soft-glow intensity `x` through the bias/knee response curve.
///
/// The curve is pinned at `(0, 0)` and `(1, 1)` and bends around a corner point that slides
/// along the anti-diagonal: `bias` in `-100..=100` places the corner at
/// `(0.5 - 0.5t, 0.5 + 0.5t)` with `t = bias/100`, so `+100` puts it at `(0, 1)` (the glow
/// saturates immediately), `0` leaves the corner on the diagonal (the curve is EXACTLY the
/// identity — this is what keeps legacy projects rendering unchanged), and `-100` puts it at
/// `(1, 0)` (the glow stays dark until the very end).
///
/// `knee` in `0..=100` blends between two ways of passing through that corner: `0` is the
/// sharp two-segment polyline `(0,0) -> corner -> (1,1)`, `100` is the quadratic Bezier that
/// uses the corner as its control point. The Bezier is evaluated by inverting
/// `x(t) = (1 - 2cx)t^2 + 2cx t` analytically; when `1 - 2cx` vanishes the three control
/// points are collinear and the inversion is linear in `t`, which collapses the curve onto
/// the identity. Inputs are clamped to `[0, 1]` and so is the result.
fn soft_glow_response_curve(x: f32, bias: f32, knee: f32) -> f32 {
    /// Degeneracy threshold for the corner position and the Bezier leading coefficient.
    const CURVE_EPS: f32 = 1e-6;

    let t = bias.clamp(-100.0, 100.0) / 100.0;
    let k = knee.clamp(0.0, 100.0) / 100.0;
    let corner_x = 0.5 - 0.5 * t;
    let corner_y = 0.5 + 0.5 * t;
    let x = x.clamp(0.0, 1.0);

    // Sharp polyline through the corner; a degenerate corner collapses it to a step, which
    // is exactly what bias = +-100 means.
    let sharp = if corner_x <= CURVE_EPS {
        if x > 0.0 { 1.0 } else { 0.0 }
    } else if corner_x >= 1.0 - CURVE_EPS {
        if x >= 1.0 { 1.0 } else { 0.0 }
    } else if x < corner_x {
        x * (corner_y / corner_x)
    } else {
        corner_y + (x - corner_x) * ((1.0 - corner_y) / (1.0 - corner_x))
    };

    // Quadratic Bezier P0=(0,0) P1=(corner) P2=(1,1), inverted for the Bezier parameter.
    let quad_a = 1.0 - 2.0 * corner_x;
    let bezier_t = if quad_a.abs() < CURVE_EPS {
        x / (2.0 * corner_x).max(CURVE_EPS)
    } else {
        (-corner_x + (corner_x * corner_x + quad_a * x).max(0.0).sqrt()) / quad_a
    };
    let bezier_t = bezier_t.clamp(0.0, 1.0);
    let bezier = 2.0 * bezier_t * (1.0 - bezier_t) * corner_y + bezier_t * bezier_t;

    ((1.0 - k) * sharp + k * bezier).clamp(0.0, 1.0)
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

/// Composites the source image over the already-drawn glow layer at (`origin_x`, `origin_y`).
///
/// Alpha contract: this straight-alpha source-over is the ONLY place the source coverage may
/// enter the result. A glow layer must therefore carry its own unreduced alpha — multiplying it
/// by `1 - src_a` beforehand counts the coverage twice and leaves a translucent ring on every
/// antialiased rim (`out_a = a + g(1-a)²` instead of `a + g(1-a)`, i.e. up to a quarter of the
/// alpha missing at half coverage), which reads as a light halo hugging the glyphs.
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
    use super::super::parse::{
        GlowEffectParams, SoftGlowEffectParams, SoftGlowShape, StrokeOpacityMode,
    };
    use super::{
        apply_glow_effect_v1, apply_glow_effect_v2, apply_soft_glow_effect,
        soft_glow_response_curve,
    };
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
            font_fallbacks: crate::types::FontFallbackReport::default(),
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
            let glow_a = (intensity * color_alpha_factor * 255.0)
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
            let glow_a = (intensity * color_alpha_factor * 255.0)
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
        use super::super::image_ops::{
            dilate_alpha_ellipse, dilate_alpha_rect, gaussian_blur_alpha_f32_in_place,
            gaussian_blur_kernel_radius,
        };
        use super::super::parse::SoftGlowShape;
        use super::{blend_source_text, soft_glow_response_curve};

        let (left, right, up, down) = glow.extents();
        if left == 0 && right == 0 && up == 0 && down == 0 {
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
        let blur_sigma = glow.blur_radius_px.max(0.0);
        let blur_pad = gaussian_blur_kernel_radius(blur_sigma);
        let pad_left = left.saturating_add(blur_pad);
        let pad_right = right.saturating_add(blur_pad);
        let pad_top = up.saturating_add(blur_pad);
        let pad_bottom = down.saturating_add(blur_pad);
        let out_width = image
            .width
            .saturating_add(pad_left)
            .saturating_add(pad_right);
        let out_height = image
            .height
            .saturating_add(pad_top)
            .saturating_add(pad_bottom);
        if out_width == 0 || out_height == 0 {
            return;
        }
        let out_width_usize = out_width as usize;
        let out_height_usize = out_height as usize;
        let source = image.rgba.clone();
        let mut out = vec![0u8; out_width_usize * out_height_usize * 4];
        let mut source_alpha_expanded = vec![0u8; out_width_usize * out_height_usize];
        let origin_x = pad_left as i32;
        let origin_y = pad_top as i32;
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
        match glow.shape {
            SoftGlowShape::Square => dilate_alpha_rect(
                dilated.as_mut_slice(),
                out_width_usize,
                out_height_usize,
                left as usize,
                right as usize,
                up as usize,
                down as usize,
            ),
            SoftGlowShape::Round => dilate_alpha_ellipse(
                dilated.as_mut_slice(),
                out_width_usize,
                out_height_usize,
                left as usize,
                right as usize,
                up as usize,
                down as usize,
            ),
        }
        let mut outline: Vec<f32> = dilated
            .iter()
            .zip(source_alpha_expanded.iter())
            .map(|(&dilated_a, &source_a)| f32::from(dilated_a.saturating_sub(source_a)) / 255.0)
            .collect();
        if blur_sigma > f32::EPSILON {
            gaussian_blur_alpha_f32_in_place(&mut outline, out_width, out_height, blur_sigma);
        }
        for (dst, &outline_value) in out.chunks_mut(4).zip(outline.iter()) {
            let shaped = soft_glow_response_curve(outline_value, glow.bias, glow.knee);
            let glow_a = (shaped * color_alpha_factor * 255.0)
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

    /// Baseline soft-glow configuration: legacy-shaped (square, symmetric, identity curve).
    fn sample_soft_glow_params() -> SoftGlowEffectParams {
        SoftGlowEffectParams {
            radius_px: 2,
            expand_x_plus: 0,
            expand_x_minus: 0,
            expand_y_plus: 0,
            expand_y_minus: 0,
            shape: SoftGlowShape::Square,
            blur_radius_px: 1.4,
            bias: 0.0,
            knee: 100.0,
            color: [10, 200, 255, 180],
        }
    }

    #[test]
    fn soft_glow_parallel_composite_matches_sequential() {
        let glow = sample_soft_glow_params();
        let mut parallel = sample_glyph_image();
        let mut sequential = sample_glyph_image();
        apply_soft_glow_effect(&mut parallel, &glow);
        apply_soft_glow_effect_seq(&mut sequential, &glow);
        assert_eq!(parallel.rgba, sequential.rgba);
        assert_eq!(parallel.width, sequential.width);
        assert_eq!(parallel.height, sequential.height);
    }

    /// Same bit-identity check on the new geometry: round outline, asymmetric per-side
    /// expansion (including a negative side) and a non-zero bias/knee curve.
    #[test]
    fn soft_glow_round_asymmetric_parallel_composite_matches_sequential() {
        let glow = SoftGlowEffectParams {
            radius_px: 4,
            expand_x_plus: 3,
            expand_x_minus: -2,
            expand_y_plus: 1,
            expand_y_minus: 5,
            shape: SoftGlowShape::Round,
            blur_radius_px: 2.1,
            bias: 45.0,
            knee: 60.0,
            color: [10, 200, 255, 180],
        };
        let mut parallel = sample_glyph_image();
        let mut sequential = sample_glyph_image();
        apply_soft_glow_effect(&mut parallel, &glow);
        apply_soft_glow_effect_seq(&mut sequential, &glow);
        assert_eq!(parallel.rgba, sequential.rgba);
        assert_eq!(parallel.width, sequential.width);
        assert_eq!(parallel.height, sequential.height);
    }

    /// Per-side padding must follow the per-side extents: the canvas grows by
    /// `extent + blur kernel radius` on each side and `content_origin` moves by the LEFT/TOP
    /// padding only.
    #[test]
    fn soft_glow_pads_each_side_by_its_own_extent() {
        let glow = SoftGlowEffectParams {
            radius_px: 3,
            expand_x_plus: 6,
            expand_x_minus: 0,
            expand_y_plus: 0,
            expand_y_minus: 4,
            shape: SoftGlowShape::Square,
            blur_radius_px: 0.0,
            bias: 0.0,
            knee: 100.0,
            color: [255, 255, 255, 255],
        };
        let base = sample_glyph_image();
        let (base_width, base_height) = (base.width, base.height);
        let mut image = base;
        apply_soft_glow_effect(&mut image, &glow);

        // extents: left 3, right 9, up 7, down 3; blur radius 0 adds nothing.
        assert_eq!(image.width, base_width + 3 + 9);
        assert_eq!(image.height, base_height + 7 + 3);
        assert_eq!(image.content_origin_x, 3);
        assert_eq!(image.content_origin_y, 7);
        assert_eq!(
            image.rgba.len(),
            image.width as usize * image.height as usize * 4
        );
    }

    /// With `bias = 0` the response curve is the identity, so the composited glow must equal
    /// the plain blurred outline: the reference below reproduces the outline field with the
    /// shared helpers and compares the alpha of every pixel the source text does not cover.
    #[test]
    fn soft_glow_bias_zero_matches_plain_blurred_outline() {
        use super::super::image_ops::{dilate_alpha_rect, gaussian_blur_alpha_f32_in_place};

        let glow = SoftGlowEffectParams {
            knee: 55.0,
            ..sample_soft_glow_params()
        };
        let source = sample_glyph_image();
        let (src_width, src_height) = (source.width as usize, source.height as usize);
        let src_rgba = source.rgba.clone();
        let mut image = source;
        apply_soft_glow_effect(&mut image, &glow);

        let out_width = image.width as usize;
        let out_height = image.height as usize;
        let origin_x = image.content_origin_x as usize;
        let origin_y = image.content_origin_y as usize;

        let mut expanded = vec![0u8; out_width * out_height];
        for y in 0..src_height {
            for x in 0..src_width {
                expanded[(origin_y + y) * out_width + origin_x + x] =
                    src_rgba[(y * src_width + x) * 4 + 3];
            }
        }
        let mut dilated = expanded.clone();
        dilate_alpha_rect(dilated.as_mut_slice(), out_width, out_height, 2, 2, 2, 2);
        let mut outline: Vec<f32> = dilated
            .iter()
            .zip(expanded.iter())
            .map(|(&d, &s)| f32::from(d.saturating_sub(s)) / 255.0)
            .collect();
        gaussian_blur_alpha_f32_in_place(
            &mut outline,
            image.width,
            image.height,
            glow.blur_radius_px,
        );

        let color_alpha_factor = f32::from(glow.color[3]) / 255.0;
        for (idx, &outline_value) in outline.iter().enumerate() {
            if expanded[idx] != 0 {
                // Covered by the source text, whose own composite dominates the alpha there.
                continue;
            }
            let expected = (outline_value * color_alpha_factor * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            assert_eq!(
                image.rgba[idx * 4 + 3],
                expected,
                "pixel {idx}: curve must be a no-op at bias 0"
            );
        }
    }

    /// The round element is inscribed in the square one, so an equal-radius round glow may
    /// never exceed the square glow anywhere, and must fall strictly short at the corners.
    #[test]
    fn soft_glow_round_is_contained_in_square() {
        let square = SoftGlowEffectParams {
            radius_px: 6,
            blur_radius_px: 0.0,
            shape: SoftGlowShape::Square,
            ..sample_soft_glow_params()
        };
        let round = SoftGlowEffectParams {
            shape: SoftGlowShape::Round,
            ..square.clone()
        };
        let mut square_image = sample_glyph_image();
        let mut round_image = sample_glyph_image();
        apply_soft_glow_effect(&mut square_image, &square);
        apply_soft_glow_effect(&mut round_image, &round);

        assert_eq!(square_image.width, round_image.width);
        assert_eq!(square_image.height, round_image.height);
        let mut strictly_smaller = 0usize;
        for (square_px, round_px) in square_image
            .rgba
            .chunks_exact(4)
            .zip(round_image.rgba.chunks_exact(4))
        {
            assert!(
                round_px[3] <= square_px[3],
                "round glow alpha {} exceeds square glow alpha {}",
                round_px[3],
                square_px[3]
            );
            if round_px[3] < square_px[3] {
                strictly_smaller += 1;
            }
        }
        assert!(
            strictly_smaller > 0,
            "round glow must be strictly smaller than the square glow at the corners"
        );
    }

    /// The response curve must be pinned at both ends for every bias/knee combination.
    #[test]
    fn soft_glow_curve_is_pinned_at_both_ends() {
        for bias in [-100.0f32, -60.0, -1.0, 0.0, 1.0, 60.0, 100.0] {
            for knee in [0.0f32, 25.0, 50.0, 75.0, 100.0] {
                let at_zero = soft_glow_response_curve(0.0, bias, knee);
                let at_one = soft_glow_response_curve(1.0, bias, knee);
                assert!(
                    at_zero.abs() <= 1e-6,
                    "curve(0) = {at_zero} at bias {bias}, knee {knee}"
                );
                assert!(
                    (at_one - 1.0).abs() <= 1e-6,
                    "curve(1) = {at_one} at bias {bias}, knee {knee}"
                );
            }
        }
    }

    /// Bias 0 must be the exact identity at every knee — this is what keeps projects saved
    /// before the curve existed rendering unchanged.
    #[test]
    fn soft_glow_curve_is_identity_at_zero_bias() {
        for knee in [0.0f32, 50.0, 100.0] {
            for step in 0..=200 {
                let x = step as f32 / 200.0;
                let y = soft_glow_response_curve(x, 0.0, knee);
                assert!(
                    (y - x).abs() <= 1e-6,
                    "curve({x}) = {y} at bias 0, knee {knee}"
                );
            }
        }
    }

    /// The curve must be monotonic non-decreasing everywhere it is used: a dip would make a
    /// thicker outline produce a fainter glow.
    #[test]
    fn soft_glow_curve_is_monotonic() {
        for bias_step in -20..=20 {
            let bias = bias_step as f32 * 5.0;
            for knee in [0.0f32, 50.0, 100.0] {
                let mut previous = soft_glow_response_curve(0.0, bias, knee);
                for step in 1..=400 {
                    let x = step as f32 / 400.0;
                    let value = soft_glow_response_curve(x, bias, knee);
                    assert!(
                        value >= previous - 1e-6,
                        "curve dips at x = {x} (bias {bias}, knee {knee}): {previous} -> {value}"
                    );
                    previous = value;
                }
            }
        }
    }

    /// The analytically inverted Bezier (knee 100) must agree with a forward evaluation of the
    /// same quadratic Bezier, sampled by its own parameter.
    #[test]
    fn soft_glow_curve_matches_forward_bezier() {
        for bias in [-80.0f32, -40.0, 40.0, 80.0] {
            let t = f64::from(bias) / 100.0;
            let corner_x = 0.5 - 0.5 * t;
            let corner_y = 0.5 + 0.5 * t;
            for step in 0..=100 {
                let param = f64::from(step) / 100.0;
                let x = (1.0 - 2.0 * corner_x) * param * param + 2.0 * corner_x * param;
                let y = 2.0 * param * (1.0 - param) * corner_y + param * param;
                let got = soft_glow_response_curve(x as f32, bias, 100.0);
                assert!(
                    (f64::from(got) - y).abs() <= 1e-5,
                    "bias {bias}, t {param}: curve({x}) = {got}, forward Bezier = {y}"
                );
            }
        }
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

    /// Asserts the composited alpha never dips as a ray leaves the source through its
    /// antialiased rim.
    ///
    /// The glow layer carries its own alpha and `blend_source_text` puts the source over it,
    /// which accounts for the rim coverage exactly once. Multiplying the glow by `1 - src_a`
    /// beforehand counted it twice and punched a translucent notch into the rim pixel (v1
    /// 186/255, v2 190/255 where the next pixel outward already read 221/233), which shows up
    /// as a light halo hugging the glyphs over a light background.
    ///
    /// Rays run inward-to-outward, so a correct profile is non-increasing; a single alpha level
    /// of slack absorbs the `u8` rounding, exactly as `assert_profile_smooth` does.
    fn assert_no_rim_alpha_notch(name: &str, apply: impl Fn(&mut RenderedTextImage)) {
        let (mut image, bx0, _bx1, by0, _by1) = block_image(true);
        apply(&mut image);

        let out_width = image.width as usize;
        // The effect grows the canvas symmetrically and advances the content origin by the
        // padding, so this maps source coordinates into the padded buffer.
        let pad_x = image.content_origin_x as usize;
        let pad_y = image.content_origin_y as usize;
        let alpha_at = |x: usize, y: usize| image.rgba[(y * out_width + x) * 4 + 3];

        // Leftward from 2px inside the solid core, and upward from the same depth.
        let horizontal: Vec<u8> = (0..10)
            .map(|k| alpha_at(pad_x + bx0 + 2 - k, pad_y + by0 + 10))
            .collect();
        let vertical: Vec<u8> = (0..10)
            .map(|k| alpha_at(pad_x + bx0 + 20, pad_y + by0 + 2 - k))
            .collect();

        for (axis, profile) in [("horizontal", &horizontal), ("vertical", &vertical)] {
            for pair in profile.windows(2) {
                assert!(
                    pair[1] <= pair[0].saturating_add(1),
                    "{name}/{axis}: alpha dips at the antialiased rim: {profile:?}"
                );
            }
        }
    }

    #[test]
    fn glow_v1_leaves_no_alpha_notch_on_the_antialiased_rim() {
        let glow = smoothness_glow_params();
        assert_no_rim_alpha_notch("glow_v1", |image| apply_glow_effect_v1(image, &glow));
    }

    #[test]
    fn glow_v2_leaves_no_alpha_notch_on_the_antialiased_rim() {
        let glow = smoothness_glow_params();
        assert_no_rim_alpha_notch("glow_v2", |image| apply_glow_effect_v2(image, &glow));
    }

    #[test]
    fn soft_glow_leaves_no_alpha_notch_on_the_antialiased_rim() {
        let glow = sample_soft_glow_params();
        assert_no_rim_alpha_notch("soft_glow", |image| apply_soft_glow_effect(image, &glow));
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
            font_fallbacks: crate::types::FontFallbackReport::default(),
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
