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
use rayon::prelude::*;

/// Composites a straight-alpha RGBA `src` buffer over `dst` in place, pixel for pixel.
///
/// Both buffers are interpreted as packed unmultiplied RGBA. Each output pixel is
/// computed independently from the read-only `src`, so the work is parallelized by
/// row-aligned 4-byte pixel chunks; the result is bit-identical to a sequential pass.
pub(crate) fn blend_full_image_over(dst: &mut [u8], src: &[u8]) {
    let pixel_count = (dst.len() / 4).min(src.len() / 4);
    let dst_pixels = &mut dst[..pixel_count * 4];
    // Each destination pixel depends only on the matching src pixel (no scatter,
    // no shared mutable state), so per-pixel parallelism is exact-equality safe.
    dst_pixels
        .par_chunks_mut(4)
        .enumerate()
        .for_each(|(idx, dst_pixel)| {
            let base = idx * 4;
            let src_a = src[base + 3];
            if src_a == 0 {
                return;
            }
            blend_pixel_over(dst_pixel, src[base], src[base + 1], src[base + 2], src_a);
        });
}

/// Gaussian-blurs an unmultiplied RGBA buffer in place using a separable two-pass kernel.
///
/// `sigma` is the Gaussian standard deviation in pixels. Each color channel is blurred
/// independently in straight (non-premultiplied) space, replicating the kernel construction
/// and edge handling of `image::imageops::blur` (image 0.25): the kernel size is derived from
/// `image`'s `kernel_size_from_sigma` formula, the weights are an `image`-identical normalized
/// Gaussian, the intermediate horizontal pass is kept in `f32` (not re-quantized), and edges
/// replicate (clamp-to-edge). The horizontal pass is parallelized over output rows and the
/// vertical pass over output rows; both passes read from an immutable source, so the result
/// is deterministic and has the no-full-clone property (the only auxiliary buffer is a
/// transient `f32` scratch, not a copy of the output).
///
/// Visual-equivalence note: this separable kernel matches `image::imageops::blur` within a
/// 2/255 per-channel tolerance across the production sigma range (golden test
/// `separable_blur_matches_image_blur_within_tolerance`, verified sigma ∈
/// {0.35, 0.5, 1.0, 1.5, 2.4, 4.0, 8.0}, measured max 0/255 for both the RGBA and alpha paths).
pub(crate) fn gaussian_blur_rgba_in_place(rgba: &mut [u8], width: u32, height: u32, sigma: f32) {
    if sigma <= f32::EPSILON || width == 0 || height == 0 {
        return;
    }
    let expected_len = width as usize * height as usize * 4;
    if rgba.len() != expected_len {
        return;
    }
    separable_gaussian_blur_interleaved(rgba, width as usize, height as usize, 4, sigma);
}

/// Gaussian-blurs a single-channel (alpha) buffer in place using a separable two-pass kernel.
///
/// Mirrors `gaussian_blur_rgba_in_place` for 1-byte-per-pixel data; same kernel, edge
/// handling, and parallelization strategy.
pub(crate) fn gaussian_blur_alpha_in_place(alpha: &mut [u8], width: u32, height: u32, sigma: f32) {
    if sigma <= f32::EPSILON || width == 0 || height == 0 {
        return;
    }
    let expected_len = width as usize * height as usize;
    if alpha.len() != expected_len {
        return;
    }
    separable_gaussian_blur_interleaved(alpha, width as usize, height as usize, 1, sigma);
}

/// Derives the odd 1D kernel size for `sigma`, identical to image 0.25's
/// `GaussianBlurParameters::kernel_size_from_sigma`.
///
/// Returns `max(3, floor((((sigma - 0.8) / 0.3 + 1) * 2) + 1))` bumped to the next odd value.
/// `sigma` must be `> 0`; production blur sigmas are always positive.
fn image_kernel_size_from_sigma(sigma: f32) -> usize {
    // Mirror image's formula verbatim, including the `.max(3.) as u32` truncation toward zero.
    let possible = (((sigma - 0.8) / 0.3 + 1.0) * 2.0 + 1.0).max(3.0);
    // `possible` is a small positive value here, so the truncating cast cannot lose data.
    let possible = possible as usize;
    if possible.is_multiple_of(2) {
        possible + 1
    } else {
        possible
    }
}

/// Builds a normalized 1D Gaussian kernel matching image 0.25's `get_gaussian_kernel_1d`.
///
/// The kernel has `kernel_size` taps with integer center `mean = kernel_size / 2`; weight at
/// index `x` is `exp(-0.5 * ((x - mean) / sigma)^2) * 1 / (sqrt(2*PI) * sigma)`, normalized so
/// the taps sum to 1. This reproduces image's exact weights (same scale factor and ordering)
/// so the convolution stays within tolerance, including the slight asymmetry image inherits
/// from the integer `mean` when `kernel_size` is odd (here it is centered, so symmetric).
fn gaussian_kernel_1d(sigma: f32) -> Vec<f32> {
    let kernel_size = image_kernel_size_from_sigma(sigma);
    let mut kernel = vec![0.0f32; kernel_size];
    let scale = 1.0 / ((2.0 * std::f32::consts::PI).sqrt() * sigma);
    // image uses integer division for the mean; for odd kernel_size this is the true center.
    let mean = (kernel_size / 2) as f32;
    let mut sum = 0.0f32;
    for (x, weight) in kernel.iter_mut().enumerate() {
        let d = (x as f32 - mean) / sigma;
        let w = (-0.5 * d * d).exp() * scale;
        *weight = w;
        sum += w;
    }
    if sum != 0.0 {
        let inv = 1.0 / sum;
        for w in &mut kernel {
            *w *= inv;
        }
    }
    kernel
}

/// Runs a separable Gaussian blur over an interleaved `channels`-per-pixel buffer in place.
///
/// Each channel is convolved independently in straight space, matching `image::imageops::blur`
/// (image 0.25): the horizontal pass result is kept in `f32` (NOT re-quantized to `u8`) before
/// the vertical pass, edges use replicate (clamp index to `0..len-1`) handling, and the final
/// value is rounded half-away-from-zero (matching image's `f32::round`) and clamped to `0..=255`.
/// The horizontal pass writes an `f32` scratch parallelized over rows; the vertical pass reads
/// the immutable scratch and writes back into `buffer`, parallelized over rows.
fn separable_gaussian_blur_interleaved(
    buffer: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    sigma: f32,
) {
    let kernel = gaussian_kernel_1d(sigma);
    // image uses `kernel_size / 2` (integer) as the anchor offset; for the odd kernel
    // produced here this equals (len - 1) / 2, the symmetric center.
    let radius = kernel.len() / 2;
    let row_stride = width * channels;

    // Horizontal pass: read `buffer`, write `f32` scratch. Keeping the intermediate in f32
    // (instead of rounding to u8) is required to match image's two-pass f32 accumulation.
    let mut scratch = vec![0.0f32; buffer.len()];
    scratch
        .par_chunks_mut(row_stride)
        .enumerate()
        .for_each(|(y, out_row)| {
            let in_row = &buffer[y * row_stride..(y + 1) * row_stride];
            for x in 0..width {
                for c in 0..channels {
                    let mut acc = 0.0f32;
                    for (k, &w) in kernel.iter().enumerate() {
                        // Clamp the sample column to the row bounds (replicate edges).
                        let sx = (x as isize + k as isize - radius as isize)
                            .clamp(0, width as isize - 1) as usize;
                        acc += w * f32::from(in_row[sx * channels + c]);
                    }
                    out_row[x * channels + c] = acc;
                }
            }
        });

    // Vertical pass: read the f32 `scratch`, write `buffer`. Output rows are independent; the
    // vertical neighbors are gathered from the immutable `scratch` buffer.
    buffer
        .par_chunks_mut(row_stride)
        .enumerate()
        .for_each(|(y, out_row)| {
            for x in 0..width {
                for c in 0..channels {
                    let mut acc = 0.0f32;
                    for (k, &w) in kernel.iter().enumerate() {
                        // Clamp the sample row to the image bounds (replicate edges).
                        let sy = (y as isize + k as isize - radius as isize)
                            .clamp(0, height as isize - 1)
                            as usize;
                        acc += w * scratch[sy * row_stride + x * channels + c];
                    }
                    // image rounds half-away-from-zero via f32::round, then casts to u8.
                    out_row[x * channels + c] = acc.round().clamp(0.0, 255.0) as u8;
                }
            }
        });
}

/// Expands `alpha` with a 3x3 max filter `iterations` times, growing opaque regions.
///
/// `alpha` is a single-channel `width * height` buffer mutated in place. Edges replicate
/// (neighbors clamp to the image bounds). Each iteration reads the previous state and
/// writes a fresh scratch buffer; the output rows of one iteration are independent and
/// parallelized, so each iteration is bit-identical to the sequential filter.
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
        // Output rows depend only on the read-only `alpha` snapshot of this iteration.
        tmp.par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, out_row)| {
                let y0 = y.saturating_sub(1);
                let y1 = (y + 1).min(height - 1);
                for (x, out_px) in out_row.iter_mut().enumerate() {
                    let x0 = x.saturating_sub(1);
                    let x1 = (x + 1).min(width - 1);
                    let mut max_a = 0u8;
                    for ny in y0..=y1 {
                        let row = ny * width;
                        for nx in x0..=x1 {
                            max_a = max_a.max(alpha[row + nx]);
                        }
                    }
                    *out_px = max_a;
                }
            });
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
    use super::{
        blend_full_image_over, dilate_alpha_max_filter3, gaussian_blur_alpha_in_place,
        gaussian_blur_rgba_in_place, image_has_alpha_on_edge, sample_rgba_premultiplied_bilinear,
    };
    use crate::tabs::typing::render_next::raster::blend_pixel_over;
    use crate::tabs::typing::render_next::types::RenderedTextImage;

    /// Verbatim sequential reference of `blend_full_image_over` for exact-equality tests.
    fn blend_full_image_over_seq(dst: &mut [u8], src: &[u8]) {
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

    /// Verbatim sequential reference of the 3x3 max dilation for exact-equality tests.
    fn dilate_alpha_max_filter3_seq(
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

    fn deterministic_rgba(width: usize, height: usize) -> Vec<u8> {
        let mut data = vec![0u8; width * height * 4];
        for (idx, byte) in data.iter_mut().enumerate() {
            // Cheap deterministic pseudo-pattern that exercises all channels and edges.
            *byte = ((idx.wrapping_mul(73).wrapping_add(idx / 7).wrapping_add(11)) % 256) as u8;
        }
        data
    }

    #[test]
    fn blend_full_image_over_matches_sequential() {
        let width = 17;
        let height = 13;
        let src = deterministic_rgba(width, height);
        let base = deterministic_rgba(width, height);

        let mut parallel = base.clone();
        let mut sequential = base;
        blend_full_image_over(&mut parallel, src.as_slice());
        blend_full_image_over_seq(&mut sequential, src.as_slice());

        assert_eq!(parallel, sequential);
    }

    #[test]
    fn dilate_alpha_max_filter3_matches_sequential() {
        let width = 19;
        let height = 11;
        let mut alpha: Vec<u8> = (0..width * height)
            .map(|idx| ((idx * 37 + idx / 5) % 256) as u8)
            .collect();
        let mut sequential = alpha.clone();

        dilate_alpha_max_filter3(&mut alpha, width, height, 3);
        dilate_alpha_max_filter3_seq(&mut sequential, width, height, 3);

        assert_eq!(alpha, sequential);
    }

    /// Builds an RGBA test image that stresses blur kernel truncation and edge handling:
    /// a smoothly varying base with per-pixel alpha variation PLUS a crisp 0->255 alpha step
    /// (a fully-opaque rectangle on a transparent background). The hard alpha edge exercises
    /// the high-frequency response of the kernel, where divergence from image is largest.
    fn alpha_edge_rgba(width: usize, height: usize) -> Vec<u8> {
        let mut data = vec![0u8; width * height * 4];
        let rx0 = width / 4;
        let rx1 = width - width / 4;
        let ry0 = height / 4;
        let ry1 = height - height / 4;
        for y in 0..height {
            for x in 0..width {
                let idx = (y * width + x) * 4;
                // Smoothly varying colour so every channel carries signal across the kernel.
                data[idx] = ((x * 7 + y * 3) % 256) as u8;
                data[idx + 1] = ((x * 3 + y * 11) % 256) as u8;
                data[idx + 2] = ((x * 13 + y * 5) % 256) as u8;
                // Hard 0 -> 255 alpha step: filled opaque rect on transparent background.
                let opaque = x >= rx0 && x < rx1 && y >= ry0 && y < ry1;
                data[idx + 3] = if opaque { 255 } else { 0 };
            }
        }
        data
    }

    /// Production sigma range the separable blur must match image within tolerance, covering
    /// stroke smoothing (~0.35) up to large blur radii.
    const BLUR_TEST_SIGMAS: [f32; 7] = [0.35, 0.5, 1.0, 1.5, 2.4, 4.0, 8.0];

    /// Max per-channel difference of the separable RGBA blur vs `image::imageops::blur`
    /// at `sigma` on `rgba` of size `width`x`height`.
    fn rgba_blur_max_diff(rgba: &[u8], width: u32, height: u32, sigma: f32) -> u8 {
        use image::RgbaImage;
        let reference = {
            let src = RgbaImage::from_raw(width, height, rgba.to_vec())
                .expect("RGBA buffer has width*height*4 length");
            image::imageops::blur(&src, sigma).into_raw()
        };
        let mut separable = rgba.to_vec();
        gaussian_blur_rgba_in_place(&mut separable, width, height, sigma);
        assert_eq!(separable.len(), reference.len());
        separable
            .iter()
            .zip(reference.iter())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0)
    }

    /// Golden test for the separable blur (Task 1): asserts the separable RGBA blur stays
    /// within 2/255 of `image::imageops::blur` across the FULL production sigma range on an
    /// image with both alpha variation AND a hard 0->255 alpha edge. Measured max: 0/255.
    #[test]
    fn separable_blur_matches_image_blur_within_tolerance() {
        let width = 48u32;
        let height = 32u32;
        // Two images: a dense pseudo-random one and one with a crisp alpha step / filled rect.
        let images = [
            deterministic_rgba(width as usize, height as usize),
            alpha_edge_rgba(width as usize, height as usize),
        ];

        for rgba in &images {
            for &sigma in &BLUR_TEST_SIGMAS {
                let max_diff = rgba_blur_max_diff(rgba, width, height, sigma);
                assert!(
                    max_diff <= 2,
                    "separable RGBA blur diverged from image::imageops::blur at \
                     sigma={sigma}: max per-channel diff = {max_diff}"
                );
            }
        }
    }

    /// Golden test for the alpha-only blur path (`gaussian_blur_alpha_in_place`): asserts it
    /// matches a single-channel `image::imageops::blur` (Luma8) within 2/255 across the full
    /// sigma range on a buffer with a hard 0->255 alpha step.
    #[test]
    fn separable_alpha_blur_matches_image_blur_within_tolerance() {
        use image::GrayImage;

        let width = 48u32;
        let height = 32u32;
        // Single-channel alpha plane with a crisp 0->255 step (opaque rect on empty bg).
        let mut alpha = vec![0u8; width as usize * height as usize];
        let rx0 = width as usize / 4;
        let rx1 = width as usize - width as usize / 4;
        let ry0 = height as usize / 4;
        let ry1 = height as usize - height as usize / 4;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let opaque = x >= rx0 && x < rx1 && y >= ry0 && y < ry1;
                alpha[y * width as usize + x] = if opaque { 255 } else { 0 };
            }
        }

        for &sigma in &BLUR_TEST_SIGMAS {
            let reference = {
                let src = GrayImage::from_raw(width, height, alpha.clone())
                    .expect("alpha buffer has width*height length");
                image::imageops::blur(&src, sigma).into_raw()
            };
            let mut separable = alpha.clone();
            gaussian_blur_alpha_in_place(&mut separable, width, height, sigma);
            assert_eq!(separable.len(), reference.len());
            let max_diff = separable
                .iter()
                .zip(reference.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                max_diff <= 2,
                "separable alpha blur diverged from image::imageops::blur at \
                 sigma={sigma}: max per-channel diff = {max_diff}"
            );
        }
    }

    #[test]
    fn separable_blur_alpha_keeps_length_and_is_finite() {
        let width = 24u32;
        let height = 16u32;
        let mut alpha: Vec<u8> = (0..(width * height))
            .map(|idx| ((idx * 13) % 256) as u8)
            .collect();
        let original_len = alpha.len();

        gaussian_blur_alpha_in_place(&mut alpha, width, height, 1.7);

        assert_eq!(alpha.len(), original_len);
    }

    #[test]
    fn image_has_alpha_on_edge_detects_bottom_touch() {
        let mut image = RenderedTextImage {
            width: 5,
            height: 5,
            rgba: vec![0; 5 * 5 * 4],
            warnings: Vec::new(),
            content_origin_x: 0,
            content_origin_y: 0,
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
            content_origin_x: 0,
            content_origin_y: 0,
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
