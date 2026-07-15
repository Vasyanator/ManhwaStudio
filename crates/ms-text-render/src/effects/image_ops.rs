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

/// Gaussian-blurs a single-channel `f32` alpha field in place using the separable kernel.
///
/// Same kernel construction and replicate (clamp-to-edge) handling as
/// `gaussian_blur_alpha_in_place`, but the data stays in `f32` end to end: there is no
/// intermediate or final quantization to `u8`. Callers that must blur a pre-composite
/// glow/alpha field without introducing banding blur here and round to `u8` only once,
/// at composite time. Determinism and the parallel/sequential equivalence hold because
/// every output element is a pure function of the immutable input.
pub(crate) fn gaussian_blur_alpha_f32_in_place(
    alpha: &mut [f32],
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
    separable_gaussian_blur_f32(alpha, width as usize, height as usize, sigma);
}

/// Half-width, in pixels, of the separable Gaussian kernel used for `sigma`.
///
/// A caller that blurs a padded canvas must expand its padding by this many pixels so the
/// blur tail is not clipped at the canvas rim. Returns `0` for a non-positive `sigma`
/// (no blur is applied there). The `usize -> u32` conversion saturates, so a required
/// padding can never be silently dropped to zero; production glow sigmas (clamped <= 2.0)
/// yield single-digit radii, far from the saturation point.
#[must_use]
pub(crate) fn gaussian_blur_kernel_radius(sigma: f32) -> u32 {
    if sigma <= f32::EPSILON {
        return 0;
    }
    u32::try_from(image_kernel_size_from_sigma(sigma) / 2).unwrap_or(u32::MAX)
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

/// Runs a separable Gaussian blur over a single-channel `f32` buffer in place.
///
/// Identical kernel and replicate-edge handling to `separable_gaussian_blur_interleaved`
/// (channels = 1) but keeps full `f32` precision throughout: the horizontal pass writes an
/// `f32` scratch, the vertical pass reads it and writes back into `buffer` with no rounding
/// or clamping. Both passes are parallelized over rows and read from an immutable source, so
/// the result is deterministic and bit-identical between the parallel and a sequential pass.
fn separable_gaussian_blur_f32(buffer: &mut [f32], width: usize, height: usize, sigma: f32) {
    let kernel = gaussian_kernel_1d(sigma);
    let radius = kernel.len() / 2;

    // Horizontal pass: read `buffer`, write `f32` scratch.
    let mut scratch = vec![0.0f32; buffer.len()];
    scratch
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, out_row)| {
            let in_row = &buffer[y * width..(y + 1) * width];
            for (x, out_px) in out_row.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (k, &w) in kernel.iter().enumerate() {
                    let sx = (x as isize + k as isize - radius as isize)
                        .clamp(0, width as isize - 1) as usize;
                    acc += w * in_row[sx];
                }
                *out_px = acc;
            }
        });

    // Vertical pass: read the immutable `scratch`, write `buffer`.
    buffer
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, out_row)| {
            for (x, out_px) in out_row.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (k, &w) in kernel.iter().enumerate() {
                    let sy = (y as isize + k as isize - radius as isize)
                        .clamp(0, height as isize - 1)
                        as usize;
                    acc += w * scratch[sy * width + x];
                }
                *out_px = acc;
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

/// Sentinel initial cost marking a non-seed pixel for the cost-seeded EDT.
///
/// A valid seed cost lies in `[0.0, EDT_COST_INF)`. Any other input — `>= EDT_COST_INF`,
/// negative, infinite, or NaN — is treated as "no seed here"; pixels with no reachable
/// seed keep this value in the output.
pub(crate) const EDT_COST_INF: f32 = 1.0e15;

/// Maps a caller-supplied EDT cost onto the valid seed range.
///
/// Values in `[0.0, EDT_COST_INF)` pass through unchanged; anything else (negative,
/// `>= EDT_COST_INF`, infinite, or NaN — the range check rejects NaN) becomes the
/// non-seed sentinel `EDT_COST_INF`.
fn normalize_edt_cost(value: f32) -> f32 {
    if (0.0..EDT_COST_INF).contains(&value) {
        value
    } else {
        EDT_COST_INF
    }
}

/// Felzenszwalb-Huttenlocher squared-distance transform of a binary mask.
///
/// Covered pixels (`mask[i] > 0`) are seeds at squared distance `0.0`; empty pixels are
/// non-seeds. Returns each pixel's squared Euclidean distance (px²) to the nearest covered
/// pixel; unreachable pixels keep `EDT_COST_INF`. Thin wrapper over
/// `euclidean_distance_transform_with_costs` with `{0.0, EDT_COST_INF}` seeding; it also
/// inherits that function's shape contract (`mask.len()` must equal `width * height`).
pub(crate) fn euclidean_distance_transform_to_mask(
    mask: &[u8],
    width: usize,
    height: usize,
) -> Vec<f32> {
    let costs: Vec<f32> = mask
        .iter()
        .map(|&m| if m > 0 { 0.0 } else { EDT_COST_INF })
        .collect();
    euclidean_distance_transform_with_costs(&costs, width, height)
}

/// Felzenszwalb-Huttenlocher squared-distance transform, evaluated in `f32`, over an
/// arbitrary squared-distance cost field.
///
/// `costs` is row-major, length `width * height`, in px² units: `0.0` is a fully-covered
/// seed, a finite value in `(0.0, EDT_COST_INF)` is a sub-pixel seed whose squared edge
/// distance is already known (e.g. `d0*d0` for a partially covered pixel), and any other
/// value — `>= EDT_COST_INF`, negative, infinite, or NaN — is normalized to the non-seed
/// sentinel. Returns, for each pixel, `min_p (dx*dx + dy*dy + costs[p])` — the squared
/// Euclidean distance to the nearest seed, biased by that seed's own initial cost. Pixels
/// with no reachable seed keep `EDT_COST_INF`. Runs in `O(width * height)`.
///
/// The lower-envelope algorithm is exact in real arithmetic and supports an arbitrary
/// finite initial `f` naturally (the second pass already consumes finite squared distances
/// from the first). Evaluating it in `f32` can shift envelope intersections by rounding at
/// large magnitudes; at the px²-scale costs used by the effects in this module the error
/// is negligible.
///
/// # Shape contract
/// `width * height` must not overflow `usize` and `costs.len()` must equal it. Following
/// this module's guard style (see the blur helpers), an invalid shape does not panic: the
/// function returns an all-`EDT_COST_INF` buffer of the expected size, or an empty buffer
/// when the size itself overflows.
pub(crate) fn euclidean_distance_transform_with_costs(
    costs: &[f32],
    width: usize,
    height: usize,
) -> Vec<f32> {
    const INF: f32 = EDT_COST_INF;

    let Some(expected_len) = width.checked_mul(height) else {
        // Dimension product overflows usize: no valid buffer shape exists.
        return Vec::new();
    };
    if costs.len() != expected_len {
        // Invalid shape: keep the documented output size, mark everything unreachable.
        return vec![INF; expected_len];
    }

    let mut tmp = vec![INF; expected_len];
    let mut dist2 = vec![INF; expected_len];
    let mut f = vec![0.0f32; width.max(height)];
    let mut d = vec![0.0f32; width.max(height)];

    for x in 0..width {
        for (y, fi) in f[..height].iter_mut().enumerate() {
            *fi = normalize_edt_cost(costs[y * width + x]);
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
    // Inputs are normalized upstream to [0, inf) for seeds and exactly `inf` for non-seeds
    // (first-pass outputs are either `inf` exactly or a finite seed-derived value), so a
    // strict `< inf` comparison identifies "this scanline has at least one seed" precisely.
    if !f.iter().any(|&value| value < inf) {
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

/// Deterministic value noise in `[-1, 1]`, bilinearly interpolated over an integer lattice.
///
/// `x`/`y` are pixel coordinates; `scale_px` is the lattice cell size in pixels (clamped to a
/// small positive floor). At `scale_px == 1.0` the sample coordinates land exactly on lattice
/// nodes (fractional part `0`), so the result collapses to the per-pixel hash
/// [`hash_noise_signed`]; larger scales blend neighboring nodes into smooth grain. Seed-stable:
/// identical `(seed, x, y, scale_px)` always yields the same value. Shared by `dry_media` grain
/// and `interference` static so both use one tested noise implementation.
pub(crate) fn value_noise_signed(seed: u64, x: f32, y: f32, scale_px: f32) -> f32 {
    let scale_px = scale_px.max(0.001);
    let sample_x = x / scale_px;
    let sample_y = y / scale_px;
    let x0 = sample_x.floor();
    let y0 = sample_y.floor();
    let tx = smoothstep01(sample_x - x0);
    let ty = smoothstep01(sample_y - y0);
    let ix = x0 as i32;
    let iy = y0 as i32;
    let v00 = hash_noise_signed(seed, ix, iy);
    let v10 = hash_noise_signed(seed, ix.saturating_add(1), iy);
    let v01 = hash_noise_signed(seed, ix, iy.saturating_add(1));
    let v11 = hash_noise_signed(seed, ix.saturating_add(1), iy.saturating_add(1));
    let top = lerp_f32(v00, v10, tx);
    let bottom = lerp_f32(v01, v11, tx);
    lerp_f32(top, bottom, ty)
}

/// Deterministic per-lattice-node hash mapped to `[-1, 1)`.
///
/// A splitmix64-style avalanche of `(seed, x, y)`; the top 24 bits form the unit mantissa, so
/// the result is a stable pseudo-random value with no spatial correlation between adjacent
/// nodes. Used directly for per-pixel / per-band / per-row decorrelated decisions and as the
/// lattice sampler for [`value_noise_signed`]. Decorrelate independent features by adding
/// distinct odd constants to `seed`.
pub(crate) fn hash_noise_signed(seed: u64, x: i32, y: i32) -> f32 {
    let mut value = seed
        .wrapping_add(i32_to_u64_wrapping(x).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(i32_to_u64_wrapping(y).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    let unit = ((value >> 40) as f32) / ((1u64 << 24) as f32);
    unit * 2.0 - 1.0
}

/// Linear interpolation `a + (b - a) * t`.
pub(crate) fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Reinterprets an `i32` as a `u64` via sign-extension, for mixing signed coordinates into the
/// hash without a value-changing numeric cast.
pub(crate) fn i32_to_u64_wrapping(value: i32) -> u64 {
    u64::from_ne_bytes(i64::from(value).to_ne_bytes())
}

/// Smoothstep on `[0, 1]`: `3x^2 - 2x^3`, with the input clamped to the unit interval.
pub(crate) fn smoothstep01(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
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
        EDT_COST_INF, blend_full_image_over, dilate_alpha_max_filter3,
        euclidean_distance_transform_with_costs, gaussian_blur_alpha_in_place,
        gaussian_blur_rgba_in_place, image_has_alpha_on_edge, sample_rgba_premultiplied_bilinear,
    };
    use crate::raster::blend_pixel_over;
    use crate::types::RenderedTextImage;

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

    /// Verifies the cost-seeded EDT against a brute-force `O(n^2 m^2)` reference on a small
    /// grid mixing exact seeds (`0.0`), fractional sub-pixel seeds, and non-seeds (`INF`).
    /// The lower-envelope algorithm is exact in real arithmetic; the tolerance absorbs the
    /// f32 rounding of envelope intersections.
    #[test]
    fn edt_with_costs_matches_bruteforce() {
        let width = 7usize;
        let height = 5usize;
        let inf = EDT_COST_INF;

        // Seed field: a mix of exact and fractional sub-pixel seeds; everything else empty.
        let mut costs = vec![inf; width * height];
        costs[width + 1] = 0.0;
        costs[3 * width + 5] = 0.25;
        costs[4 * width + 2] = 0.16;
        costs[6] = 0.09;

        let got = euclidean_distance_transform_with_costs(&costs, width, height);

        for y in 0..height {
            for x in 0..width {
                let mut best = inf;
                for sy in 0..height {
                    for sx in 0..width {
                        let c = costs[sy * width + sx];
                        if c >= inf {
                            continue;
                        }
                        let dx = x as f32 - sx as f32;
                        let dy = y as f32 - sy as f32;
                        let candidate = dx * dx + dy * dy + c;
                        if candidate < best {
                            best = candidate;
                        }
                    }
                }
                let g = got[y * width + x];
                if best >= inf {
                    assert!(
                        g >= inf * 0.5,
                        "pixel ({x},{y}) should be unreachable but got {g}"
                    );
                } else {
                    assert!(
                        (g - best).abs() <= 1e-2,
                        "pixel ({x},{y}): EDT={g}, brute-force={best}"
                    );
                }
            }
        }
    }

    /// A single zero-cost seed must produce the plain squared Euclidean distance everywhere.
    #[test]
    fn edt_with_costs_single_zero_seed_is_squared_distance() {
        let width = 6usize;
        let height = 5usize;
        let (sx, sy) = (2usize, 1usize);
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[sy * width + sx] = 0.0;

        let got = euclidean_distance_transform_with_costs(&costs, width, height);

        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - sx as f32;
                let dy = y as f32 - sy as f32;
                let expected = dx * dx + dy * dy;
                let g = got[y * width + x];
                assert!(
                    (g - expected).abs() <= 1e-3,
                    "pixel ({x},{y}): EDT={g}, expected={expected}"
                );
            }
        }
    }

    /// A field with no seeds must keep every pixel at the unreachable sentinel.
    #[test]
    fn edt_with_costs_all_inf_stays_unreachable() {
        let width = 5usize;
        let height = 4usize;
        let costs = vec![EDT_COST_INF; width * height];

        let got = euclidean_distance_transform_with_costs(&costs, width, height);

        assert!(got.iter().all(|&v| v >= EDT_COST_INF), "expected all-INF, got {got:?}");
    }

    /// A cost just below the sentinel is still a valid seed per the contract: the whole
    /// grid must resolve as reachable (below `EDT_COST_INF`) with roughly that cost. This
    /// guards the seed-detection threshold (a `< INF * 0.5` style check would misclassify
    /// this field as all-INF).
    #[test]
    fn edt_with_costs_accepts_cost_just_below_sentinel() {
        let width = 4usize;
        let height = 3usize;
        let near_inf_cost = EDT_COST_INF * 0.999;
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[width + 1] = near_inf_cost;

        let got = euclidean_distance_transform_with_costs(&costs, width, height);

        for (idx, &v) in got.iter().enumerate() {
            assert!(
                v < EDT_COST_INF,
                "pixel {idx} treated as unreachable despite a valid near-sentinel seed"
            );
            // The px^2-scale grid offsets vanish in f32 next to the huge seed cost.
            assert!(
                (v - near_inf_cost).abs() <= near_inf_cost * 1e-3,
                "pixel {idx}: got {v}, expected ~{near_inf_cost}"
            );
        }
    }

    /// NaN and negative costs are invalid and must be normalized to non-seeds: the output
    /// is driven solely by the remaining valid seed and contains no NaN.
    #[test]
    fn edt_with_costs_ignores_nan_and_negative_costs() {
        let width = 5usize;
        let height = 4usize;
        let (sx, sy) = (4usize, 3usize);
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[width + 1] = f32::NAN;
        costs[2 * width + 3] = -5.0;
        costs[sy * width + sx] = 0.0;

        let got = euclidean_distance_transform_with_costs(&costs, width, height);

        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - sx as f32;
                let dy = y as f32 - sy as f32;
                let expected = dx * dx + dy * dy;
                let g = got[y * width + x];
                assert!(!g.is_nan(), "pixel ({x},{y}) is NaN");
                assert!(
                    (g - expected).abs() <= 1e-3,
                    "pixel ({x},{y}): EDT={g}, expected={expected} (only the 0.0 seed counts)"
                );
            }
        }
    }

    /// A cost buffer whose length does not match `width * height` must not panic: the
    /// documented guard returns an all-sentinel buffer of the expected size.
    #[test]
    fn edt_with_costs_rejects_mismatched_length() {
        let got = euclidean_distance_transform_with_costs(&[0.0f32; 7], 4, 3);
        assert_eq!(got.len(), 12);
        assert!(got.iter().all(|&v| v >= EDT_COST_INF));
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
