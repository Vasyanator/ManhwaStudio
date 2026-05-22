/*
File: src/tabs/typing/render_next/effects/image_ops.rs

Purpose:
Общие image/math helper'ы для post-effects нового рендера typing.

Main responsibilities:
- держать blur/dilate/blend/EDT helpers отдельно от конкретных эффектов;
- переиспользоваться несколькими effect-модулями без копипасты;
- содержать локальные тесты на безопасные image helper'ы.
*/

use super::super::raster::blend_pixel_over;
#[cfg(test)]
use super::super::types::RenderedTextImage;
use image::{GrayImage, RgbaImage};

pub(crate) fn blend_full_image_over(dst: &mut [u8], src: &[u8]) {
    let pixel_count = (dst.len() / 4).min(src.len() / 4);
    for idx in 0..pixel_count {
        let base = idx * 4;
        let src_a = src[base + 3];
        if src_a == 0 {
            continue;
        }
        blend_pixel_over(
            &mut dst[base..base + 4],
            src[base],
            src[base + 1],
            src[base + 2],
            src_a,
        );
    }
}

pub(crate) fn gaussian_blur_rgba_in_place(rgba: &mut Vec<u8>, width: u32, height: u32, sigma: f32) {
    if sigma <= f32::EPSILON || width == 0 || height == 0 {
        return;
    }
    let expected_len = width as usize * height as usize * 4;
    if rgba.len() != expected_len {
        return;
    }
    let Some(src_image) = RgbaImage::from_raw(width, height, rgba.clone()) else {
        return;
    };
    let blurred = image::imageops::blur(&src_image, sigma);
    *rgba = blurred.into_raw();
}

pub(crate) fn gaussian_blur_alpha_in_place(
    alpha: &mut Vec<u8>,
    width: u32,
    height: u32,
    sigma: f32,
) {
    if sigma <= f32::EPSILON || width == 0 || height == 0 {
        return;
    }
    let expected_len = width as usize * height as usize;
    if alpha.len() != expected_len {
        return;
    }
    let Some(src_image) = GrayImage::from_raw(width, height, alpha.clone()) else {
        return;
    };
    let blurred = image::imageops::blur(&src_image, sigma);
    *alpha = blurred.into_raw();
}

pub(crate) fn dilate_alpha_max_filter3(
    alpha: &mut [u8],
    width: usize,
    height: usize,
    iterations: usize,
) {
    if iterations == 0 || width == 0 || height == 0 || alpha.len() != width * height {
        return;
    }
    let mut tmp = vec![0u8; alpha.len()];
    for _ in 0..iterations {
        for y in 0..height {
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(height - 1);
            for x in 0..width {
                let x0 = x.saturating_sub(1);
                let x1 = (x + 1).min(width - 1);
                let mut max_a = 0u8;
                for ny in y0..=y1 {
                    let row = ny * width;
                    for nx in x0..=x1 {
                        max_a = max_a.max(alpha[row + nx]);
                    }
                }
                tmp[y * width + x] = max_a;
            }
        }
        alpha.copy_from_slice(tmp.as_slice());
    }
}

pub(crate) fn sample_rgba_premultiplied_bilinear(
    src: &[u8],
    width: usize,
    height: usize,
    x: f32,
    y: f32,
) -> (f32, f32, f32, f32) {
    if width == 0 || height == 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;

    let mut accum_r = 0.0f32;
    let mut accum_g = 0.0f32;
    let mut accum_b = 0.0f32;
    let mut accum_a = 0.0f32;

    let bilinear_points = [
        (x0, y0, (1.0 - tx) * (1.0 - ty)),
        (x0 + 1, y0, tx * (1.0 - ty)),
        (x0, y0 + 1, (1.0 - tx) * ty),
        (x0 + 1, y0 + 1, tx * ty),
    ];
    for (sample_x, sample_y, weight) in bilinear_points {
        if weight <= f32::EPSILON
            || sample_x < 0
            || sample_y < 0
            || sample_x >= width as i32
            || sample_y >= height as i32
        {
            continue;
        }
        let idx = (sample_y as usize * width + sample_x as usize) * 4;
        let alpha = src[idx + 3] as f32 / 255.0;
        if alpha <= f32::EPSILON {
            continue;
        }
        accum_r += (src[idx] as f32 / 255.0) * alpha * weight;
        accum_g += (src[idx + 1] as f32 / 255.0) * alpha * weight;
        accum_b += (src[idx + 2] as f32 / 255.0) * alpha * weight;
        accum_a += alpha * weight;
    }

    (accum_r, accum_g, accum_b, accum_a)
}

// All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_image_with_opacity(
    dst: &mut [u8],
    dst_width: usize,
    dst_height: usize,
    src: &[u8],
    src_width: usize,
    src_height: usize,
    offset_x: i32,
    offset_y: i32,
    opacity: f32,
) {
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= f32::EPSILON
        || dst_width == 0
        || dst_height == 0
        || src_width == 0
        || src_height == 0
    {
        return;
    }

    for sy in 0..src_height {
        let dy = offset_y + sy as i32;
        if dy < 0 || dy >= dst_height as i32 {
            continue;
        }
        for sx in 0..src_width {
            let dx = offset_x + sx as i32;
            if dx < 0 || dx >= dst_width as i32 {
                continue;
            }

            let src_idx = (sy * src_width + sx) * 4;
            let src_a = src[src_idx + 3];
            if src_a == 0 {
                continue;
            }
            let out_a = ((src_a as f32) * opacity).round().clamp(0.0, 255.0) as u8;
            if out_a == 0 {
                continue;
            }

            let dst_idx = (dy as usize * dst_width + dx as usize) * 4;
            blend_pixel_over(
                &mut dst[dst_idx..dst_idx + 4],
                src[src_idx],
                src[src_idx + 1],
                src[src_idx + 2],
                out_a,
            );
        }
    }
}

pub(crate) fn euclidean_distance_transform_to_mask(
    mask: &[u8],
    width: usize,
    height: usize,
) -> Vec<f32> {
    const INF: f32 = 1.0e15;

    let mut tmp = vec![INF; width * height];
    let mut dist2 = vec![INF; width * height];
    let mut f = vec![0.0f32; width.max(height)];
    let mut d = vec![0.0f32; width.max(height)];

    for x in 0..width {
        for (y, fi) in f[..height].iter_mut().enumerate() {
            let idx = y * width + x;
            *fi = if mask[idx] > 0 { 0.0 } else { INF };
        }
        edt_1d(f[..height].as_ref(), d[..height].as_mut(), INF);
        for y in 0..height {
            tmp[y * width + x] = d[y];
        }
    }

    for y in 0..height {
        let row = y * width;
        f[..width].copy_from_slice(&tmp[row..(width + row)]);
        edt_1d(f[..width].as_ref(), d[..width].as_mut(), INF);
        dist2[row..(width + row)].copy_from_slice(&d[..width]);
    }

    dist2
}

fn edt_1d(f: &[f32], out: &mut [f32], inf: f32) {
    let n = f.len();
    if n == 0 {
        return;
    }
    if !f.iter().any(|&value| value < inf * 0.5) {
        out.fill(inf);
        return;
    }

    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    let mut k = 0usize;

    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;

    for q in 1..n {
        let mut s = edt_intersection(f, q, v[k]);
        while s <= z[k] {
            if k == 0 {
                break;
            }
            k -= 1;
            s = edt_intersection(f, q, v[k]);
        }

        if s <= z[k] && k == 0 {
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            k = 0;
            continue;
        }

        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }

    let mut kk = 0usize;
    for (q, out_q) in out[..n].iter_mut().enumerate() {
        while z[kk + 1] < q as f32 {
            kk += 1;
        }
        let p = v[kk];
        let dq = q as f32 - p as f32;
        *out_q = dq * dq + f[p];
    }
}

fn edt_intersection(f: &[f32], q: usize, p: usize) -> f32 {
    let qf = q as f32;
    let pf = p as f32;
    ((f[q] + qf * qf) - (f[p] + pf * pf)) / (2.0 * (qf - pf))
}

pub(crate) fn average_opaque_rgba(rgba: &[u8]) -> [u8; 4] {
    let mut sum_r = 0.0f32;
    let mut sum_g = 0.0f32;
    let mut sum_b = 0.0f32;
    let mut sum_a = 0.0f32;
    let mut covered = 0.0f32;

    for chunk in rgba.chunks_exact(4) {
        let alpha = chunk[3] as f32 / 255.0;
        if alpha <= f32::EPSILON {
            continue;
        }
        sum_r += chunk[0] as f32 * alpha;
        sum_g += chunk[1] as f32 * alpha;
        sum_b += chunk[2] as f32 * alpha;
        sum_a += chunk[3] as f32;
        covered += alpha;
    }

    if covered <= f32::EPSILON {
        return [0, 0, 0, 255];
    }

    [
        (sum_r / covered).round().clamp(0.0, 255.0) as u8,
        (sum_g / covered).round().clamp(0.0, 255.0) as u8,
        (sum_b / covered).round().clamp(0.0, 255.0) as u8,
        (sum_a / covered).round().clamp(0.0, 255.0) as u8,
    ]
}

#[cfg(test)]
#[must_use]
pub(crate) fn image_has_alpha_on_edge(image: &RenderedTextImage, inset_px: u32) -> bool {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return false;
    }
    let inset = inset_px
        .min(image.width.saturating_sub(1))
        .min(image.height.saturating_sub(1));
    let inset = inset as usize;

    for x in inset..(width - inset) {
        if image.rgba[(inset * width + x) * 4 + 3] > 0 {
            return true;
        }
        if image.rgba[((height - 1 - inset) * width + x) * 4 + 3] > 0 {
            return true;
        }
    }

    for y in inset..(height - inset) {
        if image.rgba[(y * width + inset) * 4 + 3] > 0 {
            return true;
        }
        if image.rgba[(y * width + (width - 1 - inset)) * 4 + 3] > 0 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{image_has_alpha_on_edge, sample_rgba_premultiplied_bilinear};
    use crate::tabs::typing::render_next::types::RenderedTextImage;

    #[test]
    fn image_has_alpha_on_edge_detects_bottom_touch() {
        let mut image = RenderedTextImage {
            width: 5,
            height: 5,
            rgba: vec![0; 5 * 5 * 4],
            warnings: Vec::new(),
        };
        image.rgba[((4 * 5) + 2) * 4 + 3] = 255;

        assert!(image_has_alpha_on_edge(&image, 0));
    }

    #[test]
    fn image_has_alpha_on_edge_respects_inset() {
        let mut image = RenderedTextImage {
            width: 5,
            height: 5,
            rgba: vec![0; 5 * 5 * 4],
            warnings: Vec::new(),
        };
        image.rgba[(5 + 2) * 4 + 3] = 255;

        assert!(!image_has_alpha_on_edge(&image, 0));
        assert!(image_has_alpha_on_edge(&image, 1));
    }

    #[test]
    fn premultiplied_bilinear_preserves_unit_alpha_for_opaque_pixel() {
        let rgba = vec![255, 0, 0, 255];

        let (r, g, b, a) = sample_rgba_premultiplied_bilinear(rgba.as_slice(), 1, 1, 0.0, 0.0);

        assert_eq!(r, 1.0);
        assert_eq!(g, 0.0);
        assert_eq!(b, 0.0);
        assert_eq!(a, 1.0);
    }
}
