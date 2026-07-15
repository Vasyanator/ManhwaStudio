/*
File: src/tabs/typing/render_next/effects/interference.rs

Purpose:
Interference (помехи/glitch) post-effect for the new typing renderer.

Main responsibilities:
- apply one of four seed-stable interference sub-kinds over a finished raster;
- keep every pass a pure gather from an immutable source snapshot so the rayon
  row parallelization is bit-identical to a sequential reference;
- reuse the shared noise helpers in `image_ops` (splitmix64 hash / value noise).

Key structures:
- InterferenceEffectParams / InterferenceKind (defined in `parse.rs`).

Key functions:
- apply_interference_effect(): kind dispatcher.
- apply_white_noise / apply_digital / apply_rgb_split / apply_scanlines.
- *_fill_row kernels: one output row each, shared by the parallel loop and the
  sequential test references so the two are provably identical.

Notes:
- White-noise modulates existing pixels only; the other kinds gather displaced
  content and grow the canvas (digital/scanlines: horizontal pad; rgb_split: all
  sides) so shifts and fringes are not clipped. Whole-pixel gathers are used for
  the digital band shift / RGB split and the scanline jitter (crisp, integer,
  platform-stable); rgb_split chromatic aberration uses premultiplied bilinear
  sampling for sub-pixel offsets.
*/

use super::super::types::RenderedTextImage;
use super::image_ops::{
    draw_image_with_opacity, hash_noise_signed, sample_rgba_premultiplied_bilinear,
    value_noise_signed,
};
use super::parse::{InterferenceEffectParams, InterferenceKind};
use rayon::prelude::*;

// Distinct odd constants added to the seed to decorrelate independent noise features
// (dry_media pattern). Each feature must hash from a different seed offset or the
// decisions would be spatially correlated.
const WN_DENSITY_SALT: u64 = 0x1357_9BDF_0246_8ACE_u64 | 1;
const WN_LIGHT_SALT: u64 = 0x9E37_79B9_7F4A_7C15;
const WN_LIGHT_R_SALT: u64 = 0xC2B2_AE3D_27D4_EB4F;
const WN_LIGHT_G_SALT: u64 = 0x1656_67B1_9E37_79F9;
const WN_LIGHT_B_SALT: u64 = 0xD6E8_FD17_85EB_CA77;
const WN_ALPHA_SALT: u64 = 0xBF58_476D_1CE4_E5B9;
const DIG_HEIGHT_SALT: u64 = 0x2545_F491_4F6C_DD1D;
const DIG_PROB_SALT: u64 = 0x94D0_49BB_1331_11EB;
const DIG_SHIFT_SALT: u64 = 0x8EBC_6AF0_9C88_C6E3;
const RGB_JITTER_SALT: u64 = 0x589B_5C0A_1A2B_3C4D;
const SCAN_JITTER_SALT: u64 = 0x736F_6D65_5F6A_6974;

/// Folds a non-negative pixel index into the `i32` key domain of `hash_noise_signed`
/// without a lossy `as` cast: the low 32 bits are reinterpreted as two's complement
/// (bit-exact and platform-independent). Indexes at or above 2^31 cannot occur for a
/// real canvas (the RGBA buffer would not fit in memory), so for every reachable input
/// the key equals the index.
fn hash_key(index: usize) -> i32 {
    // `index & 0xFFFF_FFFF` always fits u32; `unwrap_or` only satisfies the type system.
    let low = u32::try_from(index & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    i32::from_ne_bytes(low.to_ne_bytes())
}

/// Converts a pixel dimension or coordinate to `i64` for signed gather arithmetic.
/// Saturates above `i64::MAX`, which is unreachable for a real buffer (its byte length
/// `w * h * 4` with `h >= 1` must fit `usize`); a saturated value would only push
/// gathers out of bounds, i.e. read transparent.
fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Applies the `interference` post-effect to `image`, dispatching on the sub-kind.
///
/// Each kind is deterministic in `params.seed`. Empty / zero-sized images are a no-op. The
/// RGBA buffer stays `width * height * 4` bytes; growing kinds update `width`, `height`, and
/// `content_origin_x`/`content_origin_y` to keep the source content addressable.
pub(crate) fn apply_interference_effect(
    image: &mut RenderedTextImage,
    params: &InterferenceEffectParams,
) {
    match params.kind {
        InterferenceKind::WhiteNoise => apply_white_noise(image, params),
        InterferenceKind::Digital => apply_digital(image, params),
        InterferenceKind::RgbSplit => apply_rgb_split(image, params),
        InterferenceKind::Scanlines => apply_scanlines(image, params),
    }
}

// --------------------------------------------------------------------------------------------
// White noise
// --------------------------------------------------------------------------------------------

/// Per-pixel static: modulates lightness (and optionally alpha) of existing pixels in place.
///
/// Only pixels with source alpha `> 0` are touched; the `(1 - density)` fraction selected by a
/// decorrelated per-pixel hash is left untouched. Fully transparent pixels are never affected,
/// so the silhouette is preserved. No canvas growth.
fn apply_white_noise(image: &mut RenderedTextImage, params: &InterferenceEffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    // Nothing modulates the pixels when both amplitudes are zero: avoid the clone/alloc.
    if params.amount <= f32::EPSILON && params.alpha_noise <= f32::EPSILON {
        return;
    }

    let source = image.rgba.clone();
    let mut out = vec![0u8; source.len()];
    let row_stride = width * 4;
    // Each output pixel is a pure function of the matching source pixel and its (x, y); rows are
    // independent, so the parallel pass is bit-identical to the sequential reference.
    out.par_chunks_mut(row_stride)
        .zip(source.par_chunks(row_stride))
        .enumerate()
        .for_each(|(y, (out_row, src_row))| {
            white_noise_fill_row(out_row, y, src_row, width, params);
        });
    image.rgba = out;
}

/// Fills one output row of the white-noise pass from the matching source row.
fn white_noise_fill_row(
    out_row: &mut [u8],
    y: usize,
    src_row: &[u8],
    width: usize,
    params: &InterferenceEffectParams,
) {
    let seed = params.seed;
    let amount = params.amount;
    let scale = params.scale_px;
    for x in 0..width {
        let base = x * 4;
        // Default: copy the source pixel through unchanged (transparent / untouched pixels).
        out_row[base..base + 4].copy_from_slice(&src_row[base..base + 4]);
        let src_a = src_row[base + 3];
        if src_a == 0 {
            continue;
        }
        // Density gate: a decorrelated hash in [0, 1) leaves `1 - density` of pixels untouched.
        let density_h =
            hash_noise_signed(seed.wrapping_add(WN_DENSITY_SALT), hash_key(x), hash_key(y)) * 0.5
                + 0.5;
        if density_h >= params.density {
            continue;
        }

        let (noise_r, noise_g, noise_b) = if params.monochrome {
            let n = value_noise_signed(seed.wrapping_add(WN_LIGHT_SALT), x as f32, y as f32, scale);
            (n, n, n)
        } else {
            (
                value_noise_signed(seed.wrapping_add(WN_LIGHT_R_SALT), x as f32, y as f32, scale),
                value_noise_signed(seed.wrapping_add(WN_LIGHT_G_SALT), x as f32, y as f32, scale),
                value_noise_signed(seed.wrapping_add(WN_LIGHT_B_SALT), x as f32, y as f32, scale),
            )
        };

        out_row[base] = modulate_channel(src_row[base], noise_r, amount);
        out_row[base + 1] = modulate_channel(src_row[base + 1], noise_g, amount);
        out_row[base + 2] = modulate_channel(src_row[base + 2], noise_b, amount);

        if params.alpha_noise > f32::EPSILON {
            let erosion_h = value_noise_signed(
                seed.wrapping_add(WN_ALPHA_SALT),
                x as f32,
                y as f32,
                scale,
            ) * 0.5
                + 0.5;
            let erosion = params.alpha_noise * erosion_h;
            out_row[base + 3] = ((f32::from(src_a)) * (1.0 - erosion))
                .round()
                .clamp(0.0, 255.0) as u8;
        }
    }
}

/// Scales a color channel by `1 + noise * amount` (noise in [-1, 1]) and quantizes back to u8.
fn modulate_channel(channel: u8, noise: f32, amount: f32) -> u8 {
    (f32::from(channel) * (1.0 + noise * amount))
        .round()
        .clamp(0.0, 255.0) as u8
}

// --------------------------------------------------------------------------------------------
// Digital
// --------------------------------------------------------------------------------------------

/// Digital glitch: partitions the canvas into horizontal bands, displaces a hash-selected
/// subset horizontally, and splits R/B channels inside displaced bands.
///
/// `autogrow` pads left/right by `ceil(max_shift_px + rgb_split_px)` so shifts never clip;
/// with `autogrow=false` shifts clip at the canvas edge. Band shift and channel split are
/// whole-pixel gathers (crisp and platform-stable). Result alpha follows the base (band-shift)
/// sample so the band silhouette is the shifted source; R/B come from the extra split offsets.
fn apply_digital(image: &mut RenderedTextImage, params: &InterferenceEffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = if params.autogrow {
        // Parse clamps max_shift_px to 0..=512 and rgb_split_px to 0..=64, so the padded
        // sum is at most 576 and the f32 -> u32 conversion is exact (no truncation).
        (params.max_shift_px + params.rgb_split_px).ceil().max(0.0) as u32
    } else {
        0
    };
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height;
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;
    // pad <= 576 (bounded above), so it always fits i32; try_from keeps the conversion checked.
    let pad_i32 = i32::try_from(pad).unwrap_or(i32::MAX);

    let source = image.rgba.clone();
    let mut expanded = vec![0u8; out_width_usize * out_height_usize * 4];
    draw_image_with_opacity(
        expanded.as_mut_slice(),
        out_width_usize,
        out_height_usize,
        source.as_slice(),
        width,
        height,
        pad_i32,
        0,
        1.0,
    );

    let (row_shift, row_split) = digital_row_transforms(params, out_height_usize);

    let mut out = vec![0u8; expanded.len()];
    out.par_chunks_mut(out_width_usize * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            digital_fill_row(
                out_row,
                y,
                expanded.as_slice(),
                out_width_usize,
                row_shift[y],
                row_split[y],
            );
        });

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
}

/// Computes the per-row horizontal shift and RGB-split offset for the digital pass.
///
/// Bands are laid out top-to-bottom: each band's height is `slice_height_px` modulated by a
/// per-band hash in `[-1, 1]` scaled by `height_jitter` (min height 1). A band is displaced
/// when its probability hash falls below `probability`; a displaced band gets a signed shift in
/// `[-max_shift_px, max_shift_px]` and the fixed whole-pixel `rgb_split_px`. Returns two
/// `height`-length vectors (shift px, split px) indexed by output row.
fn digital_row_transforms(
    params: &InterferenceEffectParams,
    height: usize,
) -> (Vec<i32>, Vec<i32>) {
    let mut row_shift = vec![0i32; height];
    let mut row_split = vec![0i32; height];
    if height == 0 {
        return (row_shift, row_split);
    }
    let seed = params.seed;
    // Parse clamps slice_height_px to 1..=256: exact in f32.
    let base_h = params.slice_height_px.max(1) as f32;
    // Parse clamps rgb_split_px to 0..=64: the rounded value fits i32 exactly.
    let split = params.rgb_split_px.round().max(0.0) as i32;

    let mut y = 0usize;
    let mut band: i32 = 0;
    while y < height {
        let jitter = hash_noise_signed(seed.wrapping_add(DIG_HEIGHT_SALT), band, 0);
        // base_h <= 256 and the jitter factor is in [0, 2] (jitter in [-1, 1],
        // height_jitter in [0, 1]), so the rounded band height is <= 512: exact in usize.
        let band_h = (base_h * (1.0 + jitter * params.height_jitter))
            .round()
            .max(1.0) as usize;
        let prob_h =
            hash_noise_signed(seed.wrapping_add(DIG_PROB_SALT), band, 0) * 0.5 + 0.5;
        let displaced = prob_h < params.probability;
        let (shift, band_split) = if displaced {
            let s = hash_noise_signed(seed.wrapping_add(DIG_SHIFT_SALT), band, 0);
            // |s| <= 1 and parse clamps max_shift_px to 0..=512, so the rounded shift
            // is within +-512: exact in i32.
            ((s * params.max_shift_px).round() as i32, split)
        } else {
            (0, 0)
        };

        let end = (y + band_h).min(height);
        for row in &mut row_shift[y..end] {
            *row = shift;
        }
        for row in &mut row_split[y..end] {
            *row = band_split;
        }
        y = end;
        band = band.saturating_add(1);
    }

    (row_shift, row_split)
}

/// Fills one output row of the digital pass by gathering shifted source columns.
///
/// The whole band moves right by `shift` (source column = `x - shift`); R and B are additionally
/// read `+split` and `-split` columns away. Alpha comes from the base (G) sample. Out-of-bounds
/// columns read as fully transparent (the clip path when `autogrow=false`).
fn digital_fill_row(
    out_row: &mut [u8],
    y: usize,
    source: &[u8],
    out_width: usize,
    shift: i32,
    split: i32,
) {
    let row_base = y * out_width;
    // All column arithmetic is done in i64 so no width/coordinate can wrap: i64 covers
    // every u32 dimension plus the bounded (<= 576) shift/split offsets.
    let width_i64 = to_i64(out_width);
    let shift = i64::from(shift);
    let split = i64::from(split);
    let get = |col: i64, channel: usize| -> u8 {
        if col < 0 || col >= width_i64 {
            return 0;
        }
        // In-range col < out_width fits usize; unwrap_or only satisfies the type system.
        let col = usize::try_from(col).unwrap_or(0);
        source[(row_base + col) * 4 + channel]
    };
    for x in 0..out_width {
        let base = x * 4;
        let sx_g = to_i64(x).saturating_sub(shift);
        out_row[base] = get(sx_g.saturating_add(split), 0);
        out_row[base + 1] = get(sx_g, 1);
        out_row[base + 2] = get(sx_g.saturating_sub(split), 2);
        out_row[base + 3] = get(sx_g, 3);
    }
}

// --------------------------------------------------------------------------------------------
// RGB split (chromatic aberration)
// --------------------------------------------------------------------------------------------

/// Chromatic aberration: samples the R channel `+offset` and the B channel `-offset` along
/// `angle_deg`, keeping G and the base position fixed.
///
/// The canvas is padded by `ceil(offset_px)` on all sides; `per_row_jitter` only ERODES the
/// per-row magnitude (`offset_px * (1 - per_row_jitter * n01)`), so the fringe never exceeds the
/// base offset and the fixed padding always contains it. Result alpha is the max of the three
/// sampled alphas so R/B fringes stay visible where G is transparent. Sub-pixel offsets use
/// premultiplied bilinear sampling.
fn apply_rgb_split(image: &mut RenderedTextImage, params: &InterferenceEffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    // Parse clamps offset_px to 0..=64, so the f32 -> u32 conversion is exact.
    let pad = params.offset_px.ceil().max(0.0) as u32;
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;
    // pad <= 64 (bounded above), so it always fits i32; try_from keeps the conversion checked.
    let pad_i32 = i32::try_from(pad).unwrap_or(i32::MAX);

    let source = image.rgba.clone();
    let mut expanded = vec![0u8; out_width_usize * out_height_usize * 4];
    draw_image_with_opacity(
        expanded.as_mut_slice(),
        out_width_usize,
        out_height_usize,
        source.as_slice(),
        width,
        height,
        pad_i32,
        pad_i32,
        1.0,
    );

    let mut out = vec![0u8; expanded.len()];
    out.par_chunks_mut(out_width_usize * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            rgb_split_fill_row(
                out_row,
                y,
                expanded.as_slice(),
                out_width_usize,
                out_height_usize,
                params,
            );
        });

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
    image.content_origin_y = image.content_origin_y.saturating_add(pad);
}

/// Fills one output row of the RGB-split pass by bilinear-sampling three offset positions.
///
/// Straight-alpha recovery rule: the output alpha is `max(a_r, a_c, a_b)` and every color
/// channel is un-premultiplied against that SHARED alpha, not its own sample alpha — a
/// low-coverage fringe keeps its true (dim) contribution instead of being renormalized to
/// full saturation by its own tiny alpha.
fn rgb_split_fill_row(
    out_row: &mut [u8],
    y: usize,
    source: &[u8],
    out_width: usize,
    out_height: usize,
    params: &InterferenceEffectParams,
) {
    let seed = params.seed;
    let jitter_n =
        hash_noise_signed(seed.wrapping_add(RGB_JITTER_SALT), hash_key(y), 0) * 0.5 + 0.5;
    let mag = (params.offset_px * (1.0 - params.per_row_jitter * jitter_n)).max(0.0);
    let angle = params.angle_deg.to_radians();
    let dx = angle.cos() * mag;
    let dy = angle.sin() * mag;

    for x in 0..out_width {
        let base = x * 4;
        let cx = x as f32;
        let cy = y as f32;
        // R sampled at +offset, B at -offset, G and the alpha anchor at the centre.
        let (r_pm, _, _, a_r) =
            sample_rgba_premultiplied_bilinear(source, out_width, out_height, cx + dx, cy + dy);
        let (_, g_pm, _, a_c) =
            sample_rgba_premultiplied_bilinear(source, out_width, out_height, cx, cy);
        let (_, _, b_pm, a_b) =
            sample_rgba_premultiplied_bilinear(source, out_width, out_height, cx - dx, cy - dy);

        let out_a = a_r.max(a_c).max(a_b).clamp(0.0, 1.0);
        if out_a <= f32::EPSILON {
            out_row[base..base + 4].copy_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        // Un-premultiply against the shared output alpha so each channel keeps its true
        // premultiplied contribution (see the function doc). `x_pm <= a_x <= out_a`, so
        // the ratios are already in [0, 1]; the clamp only absorbs float noise.
        let r = (r_pm / out_a).clamp(0.0, 1.0);
        let g = (g_pm / out_a).clamp(0.0, 1.0);
        let b = (b_pm / out_a).clamp(0.0, 1.0);

        out_row[base] = (r * 255.0).round().clamp(0.0, 255.0) as u8;
        out_row[base + 1] = (g * 255.0).round().clamp(0.0, 255.0) as u8;
        out_row[base + 2] = (b * 255.0).round().clamp(0.0, 255.0) as u8;
        out_row[base + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

// --------------------------------------------------------------------------------------------
// Scanlines
// --------------------------------------------------------------------------------------------

/// Periodic darkened scanlines: rows in the "line" phase of the `line_height_px + gap_px` cycle
/// have their alpha scaled by `1 - darken`; gap rows pass through.
///
/// `jitter_px > 0` shifts each line block horizontally by a seeded per-line whole-pixel offset in
/// `[-jitter_px, jitter_px]`, padding left/right by `ceil(jitter_px)` so nothing clips. Alpha
/// (not lightness) is darkened so the effect reads over any background.
fn apply_scanlines(image: &mut RenderedTextImage, params: &InterferenceEffectParams) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let pad = if params.jitter_px > f32::EPSILON {
        // Parse clamps jitter_px to 0..=32, so the f32 -> u32 conversion is exact.
        params.jitter_px.ceil().max(0.0) as u32
    } else {
        0
    };
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height;
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;
    // pad <= 32 (bounded above), so it always fits i32; try_from keeps the conversion checked.
    let pad_i32 = i32::try_from(pad).unwrap_or(i32::MAX);

    let source = image.rgba.clone();
    let mut expanded = vec![0u8; out_width_usize * out_height_usize * 4];
    draw_image_with_opacity(
        expanded.as_mut_slice(),
        out_width_usize,
        out_height_usize,
        source.as_slice(),
        width,
        height,
        pad_i32,
        0,
        1.0,
    );

    let mut out = vec![0u8; expanded.len()];
    out.par_chunks_mut(out_width_usize * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            let (is_line, shift) = scanline_row_state(params, y);
            scanlines_fill_row(
                out_row,
                y,
                expanded.as_slice(),
                out_width_usize,
                is_line,
                shift,
                params.darken,
            );
        });

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
}

/// Returns `(is_line, shift)` for output row `y`: whether it lies in the darkened line phase and
/// its whole-pixel horizontal jitter offset (0 for gap rows or when `jitter_px == 0`).
fn scanline_row_state(params: &InterferenceEffectParams, y: usize) -> (bool, i32) {
    // Parse clamps line_height_px and gap_px to 1..=64; the max(1) guard plus checked
    // conversion keep the function total for any hand-built params. Cycle math stays in
    // usize so no row index can wrap.
    let line_h = usize::try_from(params.line_height_px.max(1)).unwrap_or(1);
    let gap = usize::try_from(params.gap_px.max(1)).unwrap_or(1);
    let period = line_h + gap; // >= 2
    let phase = y % period;
    let is_line = phase < line_h;
    let shift = if is_line && params.jitter_px > f32::EPSILON {
        let line_index = y / period;
        let n = hash_noise_signed(
            params.seed.wrapping_add(SCAN_JITTER_SALT),
            hash_key(line_index),
            0,
        );
        // |n| <= 1 and parse clamps jitter_px to 0..=32, so the rounded shift fits i32.
        (n * params.jitter_px).round() as i32
    } else {
        0
    };
    (is_line, shift)
}

/// Fills one output row of the scanline pass: gathers source columns shifted by `shift` and, on
/// line rows, scales the alpha by `1 - darken`.
fn scanlines_fill_row(
    out_row: &mut [u8],
    y: usize,
    source: &[u8],
    out_width: usize,
    is_line: bool,
    shift: i32,
    darken: f32,
) {
    let row_base = y * out_width;
    // Column arithmetic in i64: covers any u32 width plus the bounded (<= 32) jitter shift.
    let width_i64 = to_i64(out_width);
    let shift = i64::from(shift);
    for x in 0..out_width {
        let base = x * 4;
        let sx = to_i64(x).saturating_sub(shift);
        if sx < 0 || sx >= width_i64 {
            out_row[base..base + 4].copy_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        // In-range sx < out_width fits usize; unwrap_or only satisfies the type system.
        let sx = usize::try_from(sx).unwrap_or(0);
        let src_base = (row_base + sx) * 4;
        out_row[base] = source[src_base];
        out_row[base + 1] = source[src_base + 1];
        out_row[base + 2] = source[src_base + 2];
        out_row[base + 3] = if is_line {
            (f32::from(source[src_base + 3]) * (1.0 - darken))
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            source[src_base + 3]
        };
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::{InterferenceEffectParams, InterferenceKind};
    use super::{
        apply_interference_effect, digital_fill_row, digital_row_transforms, rgb_split_fill_row,
        scanline_row_state, scanlines_fill_row, white_noise_fill_row,
    };
    use crate::types::RenderedTextImage;

    /// Builds a small opaque test image with a spatial color/alpha pattern that exercises every
    /// channel plus a transparent border (so silhouette-preserving behavior is observable).
    fn sample_image(width: usize, height: usize) -> RenderedTextImage {
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let idx = (y * width + x) * 4;
                // Transparent 1px border, opaque interior with a varying color.
                let interior = x > 0 && x + 1 < width && y > 0 && y + 1 < height;
                rgba[idx] = ((x * 20 + 30) % 256) as u8;
                rgba[idx + 1] = ((y * 25 + 40) % 256) as u8;
                rgba[idx + 2] = ((x * 7 + y * 11 + 50) % 256) as u8;
                rgba[idx + 3] = if interior { 255 } else { 0 };
            }
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

    fn base_params(kind: InterferenceKind) -> InterferenceEffectParams {
        InterferenceEffectParams {
            kind,
            seed: 7,
            amount: 0.6,
            scale_px: 1.0,
            density: 1.0,
            monochrome: true,
            alpha_noise: 0.3,
            slice_height_px: 3,
            height_jitter: 0.5,
            max_shift_px: 6.0,
            probability: 0.5,
            rgb_split_px: 3.0,
            autogrow: true,
            offset_px: 3.0,
            angle_deg: 0.0,
            per_row_jitter: 0.25,
            line_height_px: 2,
            gap_px: 2,
            darken: 0.5,
            jitter_px: 2.0,
        }
    }

    // ----------------------------------------------------------------------------------------
    // Parallel-vs-sequential bit-identical references (one per kind).
    // ----------------------------------------------------------------------------------------

    fn white_noise_seq(image: &RenderedTextImage, params: &InterferenceEffectParams) -> Vec<u8> {
        let width = image.width as usize;
        let source = image.rgba.clone();
        let mut out = vec![0u8; source.len()];
        let stride = width * 4;
        for (y, (out_row, src_row)) in out
            .chunks_mut(stride)
            .zip(source.chunks(stride))
            .enumerate()
        {
            white_noise_fill_row(out_row, y, src_row, width, params);
        }
        out
    }

    fn digital_seq(image: &RenderedTextImage, params: &InterferenceEffectParams) -> RenderedTextImage {
        let mut clone = image.clone();
        // Reconstruct the same expansion + gather as production, but sequentially.
        let width = image.width as usize;
        let height = image.height as usize;
        let pad = if params.autogrow {
            (params.max_shift_px + params.rgb_split_px).ceil().max(0.0) as u32
        } else {
            0
        };
        // Mirror the production arithmetic exactly (saturating ops, checked pad conversion)
        // so the reference cannot diverge on the growth math itself.
        let out_width = image.width.saturating_add(pad.saturating_mul(2)) as usize;
        let out_height = height;
        let mut expanded = vec![0u8; out_width * out_height * 4];
        super::draw_image_with_opacity(
            expanded.as_mut_slice(),
            out_width,
            out_height,
            image.rgba.as_slice(),
            width,
            height,
            i32::try_from(pad).unwrap_or(i32::MAX),
            0,
            1.0,
        );
        let (row_shift, row_split) = digital_row_transforms(params, out_height);
        let mut out = vec![0u8; expanded.len()];
        for (y, out_row) in out.chunks_mut(out_width * 4).enumerate() {
            digital_fill_row(
                out_row,
                y,
                expanded.as_slice(),
                out_width,
                row_shift[y],
                row_split[y],
            );
        }
        clone.width = out_width as u32;
        clone.height = out_height as u32;
        clone.rgba = out;
        clone.content_origin_x = clone.content_origin_x.saturating_add(pad);
        clone
    }

    fn rgb_split_seq(image: &RenderedTextImage, params: &InterferenceEffectParams) -> RenderedTextImage {
        let mut clone = image.clone();
        let width = image.width as usize;
        let height = image.height as usize;
        let pad = params.offset_px.ceil().max(0.0) as u32;
        // Mirror the production arithmetic exactly (saturating ops, checked pad conversion).
        let out_width = image.width.saturating_add(pad.saturating_mul(2)) as usize;
        let out_height = image.height.saturating_add(pad.saturating_mul(2)) as usize;
        let pad_i32 = i32::try_from(pad).unwrap_or(i32::MAX);
        let mut expanded = vec![0u8; out_width * out_height * 4];
        super::draw_image_with_opacity(
            expanded.as_mut_slice(),
            out_width,
            out_height,
            image.rgba.as_slice(),
            width,
            height,
            pad_i32,
            pad_i32,
            1.0,
        );
        let mut out = vec![0u8; expanded.len()];
        for (y, out_row) in out.chunks_mut(out_width * 4).enumerate() {
            rgb_split_fill_row(out_row, y, expanded.as_slice(), out_width, out_height, params);
        }
        clone.width = out_width as u32;
        clone.height = out_height as u32;
        clone.rgba = out;
        clone.content_origin_x = clone.content_origin_x.saturating_add(pad);
        clone.content_origin_y = clone.content_origin_y.saturating_add(pad);
        clone
    }

    fn scanlines_seq(image: &RenderedTextImage, params: &InterferenceEffectParams) -> RenderedTextImage {
        let mut clone = image.clone();
        let width = image.width as usize;
        let height = image.height as usize;
        let pad = if params.jitter_px > f32::EPSILON {
            params.jitter_px.ceil().max(0.0) as u32
        } else {
            0
        };
        // Mirror the production arithmetic exactly (saturating ops, checked pad conversion).
        let out_width = image.width.saturating_add(pad.saturating_mul(2)) as usize;
        let out_height = height;
        let mut expanded = vec![0u8; out_width * out_height * 4];
        super::draw_image_with_opacity(
            expanded.as_mut_slice(),
            out_width,
            out_height,
            image.rgba.as_slice(),
            width,
            height,
            i32::try_from(pad).unwrap_or(i32::MAX),
            0,
            1.0,
        );
        let mut out = vec![0u8; expanded.len()];
        for (y, out_row) in out.chunks_mut(out_width * 4).enumerate() {
            let (is_line, shift) = scanline_row_state(params, y);
            scanlines_fill_row(
                out_row,
                y,
                expanded.as_slice(),
                out_width,
                is_line,
                shift,
                params.darken,
            );
        }
        clone.width = out_width as u32;
        clone.height = out_height as u32;
        clone.rgba = out;
        clone.content_origin_x = clone.content_origin_x.saturating_add(pad);
        clone
    }

    #[test]
    fn white_noise_parallel_matches_sequential() {
        let image = sample_image(11, 9);
        let params = base_params(InterferenceKind::WhiteNoise);
        let mut parallel = image.clone();
        apply_interference_effect(&mut parallel, &params);
        let seq = white_noise_seq(&image, &params);
        assert_eq!(parallel.rgba, seq);
    }

    #[test]
    fn digital_parallel_matches_sequential() {
        let image = sample_image(12, 10);
        let params = base_params(InterferenceKind::Digital);
        let mut parallel = image.clone();
        apply_interference_effect(&mut parallel, &params);
        let seq = digital_seq(&image, &params);
        assert_eq!(parallel.width, seq.width);
        assert_eq!(parallel.height, seq.height);
        assert_eq!(parallel.rgba, seq.rgba);
    }

    #[test]
    fn rgb_split_parallel_matches_sequential() {
        let image = sample_image(10, 8);
        let params = base_params(InterferenceKind::RgbSplit);
        let mut parallel = image.clone();
        apply_interference_effect(&mut parallel, &params);
        let seq = rgb_split_seq(&image, &params);
        assert_eq!(parallel.width, seq.width);
        assert_eq!(parallel.height, seq.height);
        assert_eq!(parallel.rgba, seq.rgba);
    }

    #[test]
    fn scanlines_parallel_matches_sequential() {
        let image = sample_image(9, 12);
        let params = base_params(InterferenceKind::Scanlines);
        let mut parallel = image.clone();
        apply_interference_effect(&mut parallel, &params);
        let seq = scanlines_seq(&image, &params);
        assert_eq!(parallel.width, seq.width);
        assert_eq!(parallel.height, seq.height);
        assert_eq!(parallel.rgba, seq.rgba);
    }

    // ----------------------------------------------------------------------------------------
    // Determinism: same seed -> identical, different seed -> different.
    // ----------------------------------------------------------------------------------------

    #[test]
    fn white_noise_same_seed_identical_diff_seed_differs() {
        let image = sample_image(14, 12);
        let params = base_params(InterferenceKind::WhiteNoise);

        let mut a = image.clone();
        let mut b = image.clone();
        apply_interference_effect(&mut a, &params);
        apply_interference_effect(&mut b, &params);
        assert_eq!(a.rgba, b.rgba);

        let mut other = image.clone();
        let mut other_params = params.clone();
        other_params.seed = params.seed + 1000;
        apply_interference_effect(&mut other, &other_params);
        assert_ne!(a.rgba, other.rgba);
    }

    #[test]
    fn digital_same_seed_identical_diff_seed_differs() {
        let image = sample_image(16, 14);
        let params = base_params(InterferenceKind::Digital);

        let mut a = image.clone();
        let mut b = image.clone();
        apply_interference_effect(&mut a, &params);
        apply_interference_effect(&mut b, &params);
        assert_eq!(a.rgba, b.rgba);

        let mut other = image.clone();
        let mut other_params = params.clone();
        other_params.seed = params.seed + 1000;
        apply_interference_effect(&mut other, &other_params);
        assert_ne!(a.rgba, other.rgba);
    }

    // ----------------------------------------------------------------------------------------
    // Canvas-grow invariants.
    // ----------------------------------------------------------------------------------------

    #[test]
    fn digital_autogrow_pads_and_updates_origin() {
        let image = sample_image(10, 8);
        let mut params = base_params(InterferenceKind::Digital);
        params.autogrow = true;
        let expected_pad = (params.max_shift_px + params.rgb_split_px).ceil() as u32;

        let mut grown = image.clone();
        apply_interference_effect(&mut grown, &params);

        assert_eq!(grown.width, image.width + expected_pad * 2);
        assert_eq!(grown.height, image.height);
        assert_eq!(grown.content_origin_x, expected_pad);
        assert_eq!(grown.content_origin_y, 0);
        assert_eq!(
            grown.rgba.len(),
            grown.width as usize * grown.height as usize * 4
        );
    }

    #[test]
    fn digital_no_autogrow_keeps_dimensions() {
        let image = sample_image(10, 8);
        let mut params = base_params(InterferenceKind::Digital);
        params.autogrow = false;

        let mut kept = image.clone();
        apply_interference_effect(&mut kept, &params);

        assert_eq!(kept.width, image.width);
        assert_eq!(kept.height, image.height);
        assert_eq!(kept.content_origin_x, 0);
        assert_eq!(kept.rgba.len(), image.rgba.len());
    }

    #[test]
    fn rgb_split_pads_all_sides_and_updates_origin() {
        let image = sample_image(9, 7);
        let params = base_params(InterferenceKind::RgbSplit);
        let expected_pad = params.offset_px.ceil() as u32;

        let mut grown = image.clone();
        apply_interference_effect(&mut grown, &params);

        assert_eq!(grown.width, image.width + expected_pad * 2);
        assert_eq!(grown.height, image.height + expected_pad * 2);
        assert_eq!(grown.content_origin_x, expected_pad);
        assert_eq!(grown.content_origin_y, expected_pad);
        assert_eq!(
            grown.rgba.len(),
            grown.width as usize * grown.height as usize * 4
        );
    }

    // ----------------------------------------------------------------------------------------
    // Zero-size safety.
    // ----------------------------------------------------------------------------------------

    #[test]
    fn zero_size_image_does_not_panic() {
        for kind in [
            InterferenceKind::WhiteNoise,
            InterferenceKind::Digital,
            InterferenceKind::RgbSplit,
            InterferenceKind::Scanlines,
        ] {
            let mut image = RenderedTextImage::transparent(0, 0);
            apply_interference_effect(&mut image, &base_params(kind));
            assert_eq!(image.width, 0);
            assert_eq!(image.height, 0);
            assert!(image.rgba.is_empty());
        }
    }

    // ----------------------------------------------------------------------------------------
    // Behavioral sanity.
    // ----------------------------------------------------------------------------------------

    #[test]
    fn white_noise_changes_opaque_but_never_transparent_pixels() {
        let image = sample_image(12, 12);
        let params = base_params(InterferenceKind::WhiteNoise);
        let mut result = image.clone();
        apply_interference_effect(&mut result, &params);

        // Dimensions preserved (no growth).
        assert_eq!(result.width, image.width);
        assert_eq!(result.height, image.height);

        let mut changed_opaque = false;
        for (before, after) in image
            .rgba
            .chunks_exact(4)
            .zip(result.rgba.chunks_exact(4))
        {
            if before[3] == 0 {
                // Fully transparent source pixels must stay untouched (silhouette preserved).
                assert_eq!(before, after, "a transparent pixel was modified");
            } else if before != after {
                changed_opaque = true;
            }
        }
        assert!(changed_opaque, "white noise modified no opaque pixel");
    }

    #[test]
    fn scanlines_darken_reduces_line_row_alpha_only() {
        let image = sample_image(8, 8);
        let mut params = base_params(InterferenceKind::Scanlines);
        params.line_height_px = 1;
        params.gap_px = 1;
        params.jitter_px = 0.0; // no shift -> pixel-aligned comparison
        params.darken = 0.5;

        let mut result = image.clone();
        apply_interference_effect(&mut result, &params);
        // No jitter -> no growth.
        assert_eq!(result.width, image.width);
        assert_eq!(result.height, image.height);

        let width = image.width as usize;
        for y in 0..image.height as usize {
            for x in 0..width {
                let idx = (y * width + x) * 4;
                let before_a = image.rgba[idx + 3];
                let after_a = result.rgba[idx + 3];
                if y % 2 == 0 {
                    // Line phase: alpha scaled by (1 - darken).
                    let expected = ((f32::from(before_a)) * 0.5).round() as u8;
                    assert_eq!(after_a, expected, "line row alpha mismatch at ({x},{y})");
                } else {
                    // Gap phase: alpha untouched. RGB is only meaningful for non-transparent
                    // source pixels (the canvas expansion drops the color of alpha==0 pixels,
                    // which is visually irrelevant), so only compare RGB where alpha > 0.
                    assert_eq!(after_a, before_a, "gap row alpha changed at ({x},{y})");
                    if before_a > 0 {
                        assert_eq!(result.rgba[idx], image.rgba[idx]);
                    }
                }
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // Hand-computed golden cases (independent of the production kernels' own hash decisions:
    // they call the row kernels with explicit inputs, or fix parameters so the expected
    // output is derivable by hand).
    // ----------------------------------------------------------------------------------------

    /// Golden: digital row gather with explicit shifts. A +1 shift moves the row right and
    /// leaves a transparent left edge; a -1 shift moves it left with a transparent right
    /// edge; split=1 reads R one column right and B one column left of the base sample.
    #[test]
    fn digital_fill_row_golden_edge_shifts_and_split() {
        // 4-px single row: P0..P3 with distinct channel values.
        let source: Vec<u8> = vec![
            10, 11, 12, 255, //
            20, 21, 22, 255, //
            30, 31, 32, 255, //
            40, 41, 42, 255,
        ];
        let mut out = vec![0u8; 16];

        // shift = +1: out[x] = src[x - 1]; x = 0 falls off the left edge -> transparent.
        digital_fill_row(&mut out, 0, &source, 4, 1, 0);
        assert_eq!(
            out,
            vec![
                0, 0, 0, 0, //
                10, 11, 12, 255, //
                20, 21, 22, 255, //
                30, 31, 32, 255,
            ]
        );

        // shift = -1: out[x] = src[x + 1]; x = 3 falls off the right edge -> transparent.
        digital_fill_row(&mut out, 0, &source, 4, -1, 0);
        assert_eq!(
            out,
            vec![
                20, 21, 22, 255, //
                30, 31, 32, 255, //
                40, 41, 42, 255, //
                0, 0, 0, 0,
            ]
        );

        // shift = 0, split = 1: R from x+1, G/A from x, B from x-1 (0 outside).
        digital_fill_row(&mut out, 0, &source, 4, 0, 1);
        assert_eq!(
            out,
            vec![
                20, 11, 0, 255, //
                30, 21, 12, 255, //
                40, 31, 22, 255, //
                0, 41, 32, 255,
            ]
        );
    }

    /// Golden: with height_jitter = 0 every band is exactly `slice_height_px` rows and with
    /// probability = 1 every band is displaced — the per-row transform must be constant
    /// within each band, carry the full rgb split, and stay within max_shift.
    #[test]
    fn digital_row_transforms_bands_are_uniform_and_cover_all_rows() {
        let mut params = base_params(InterferenceKind::Digital);
        params.slice_height_px = 3;
        params.height_jitter = 0.0;
        params.probability = 1.0;
        params.max_shift_px = 6.0;
        params.rgb_split_px = 2.0;

        let height = 10usize;
        let (shift, split) = digital_row_transforms(&params, height);
        assert_eq!(shift.len(), height);
        assert_eq!(split.len(), height);

        // Bands: rows [0..3), [3..6), [6..9), [9..10) (last band truncated by the canvas).
        for band_start in [0usize, 3, 6, 9] {
            let band_end = (band_start + 3).min(height);
            for y in band_start..band_end {
                assert_eq!(
                    shift[y], shift[band_start],
                    "row {y} shift differs within band starting at {band_start}"
                );
                assert_eq!(split[y], 2, "row {y}: displaced band must carry the rgb split");
                assert!(shift[y].abs() <= 6, "row {y}: |shift| exceeds max_shift_px");
            }
        }
    }

    /// Golden: scanline cycle phase. line_height=2, gap=3 -> rows 0,1 / 5,6 darken
    /// (pattern period 5); jitter 0 -> shift always 0.
    #[test]
    fn scanline_row_state_matches_hand_computed_phase() {
        let mut params = base_params(InterferenceKind::Scanlines);
        params.line_height_px = 2;
        params.gap_px = 3;
        params.jitter_px = 0.0;

        let expected = [
            true, true, false, false, false, true, true, false, false, false,
        ];
        for (y, &want_line) in expected.iter().enumerate() {
            let (is_line, shift) = scanline_row_state(&params, y);
            assert_eq!(is_line, want_line, "phase mismatch at row {y}");
            assert_eq!(shift, 0, "jitter 0 must give zero shift at row {y}");
        }
    }

    /// Golden: scanline row gather with an explicit shift and darken. shift = +1 moves the
    /// row right (transparent left edge) and line rows halve alpha exactly.
    #[test]
    fn scanlines_fill_row_golden_shift_and_darken() {
        let source: Vec<u8> = vec![
            100, 110, 120, 200, //
            50, 60, 70, 100, //
            200, 210, 220, 255,
        ];
        let mut out = vec![0u8; 12];

        // Line row, shift +1, darken 0.5: alpha exactly halves (200 -> 100, 100 -> 50).
        scanlines_fill_row(&mut out, 0, &source, 3, true, 1, 0.5);
        assert_eq!(
            out,
            vec![
                0, 0, 0, 0, //
                100, 110, 120, 100, //
                50, 60, 70, 50,
            ]
        );

        // Gap row, no shift: identity copy.
        scanlines_fill_row(&mut out, 0, &source, 3, false, 0, 0.5);
        assert_eq!(out, source);
    }

    /// Golden: rgb_split bilinear border sampling. Width-2 row, offset 0.5 along angle 0:
    /// at x=0 the R sample straddles both pixels (premultiplied mix -> 128) and the B sample
    /// half-falls off the left border; at x=1 mirrored. Hand-computed straight RGBA.
    #[test]
    fn rgb_split_fill_row_golden_bilinear_border() {
        let mut params = base_params(InterferenceKind::RgbSplit);
        params.offset_px = 0.5;
        params.angle_deg = 0.0;
        params.per_row_jitter = 0.0;

        // P0 = (255, 40, 0, 255), P1 = (0, 80, 255, 255).
        let source: Vec<u8> = vec![255, 40, 0, 255, 0, 80, 255, 255];
        let mut out = vec![0u8; 8];
        rgb_split_fill_row(&mut out, 0, &source, 2, 1, &params);

        // x=0: r_pm = 0.5*1.0 + 0.5*0.0 = 0.5 (a_r = 1), g_pm = 40/255 (a_c = 1),
        //      b sample at -0.5: only the in-bounds tap contributes 0.5 * 0 = 0 (a_b = 0.5).
        //      out_a = 1 -> [round(127.5)=128, 40, 0, 255].
        assert_eq!(&out[0..4], &[128, 40, 0, 255]);
        // x=1: r sample at 1.5 contributes only the in-bounds tap 0.5 * 0 = 0 (a_r = 0.5),
        //      g_pm = 80/255 (a_c = 1), b_pm = 0.5*0 + 0.5*1.0 = 0.5 (a_b = 1).
        //      out_a = 1 -> [0, 80, 128, 255].
        assert_eq!(&out[4..8], &[0, 80, 128, 255]);
    }

    /// Regression fixture for the straight-alpha recovery rule: with whole-pixel offsets the
    /// R sample hits a 20%-alpha pixel and the B sample a 40%-alpha pixel while the centre is
    /// opaque. Un-premultiplying against the SHARED max alpha keeps the fringes dim
    /// (R=51, B=102); the old per-sample-alpha division renormalized R to a fully saturated
    /// 255 (a 0.2-coverage red fringe rendered at full intensity).
    #[test]
    fn rgb_split_recovers_straight_alpha_against_shared_max_alpha() {
        let mut params = base_params(InterferenceKind::RgbSplit);
        params.offset_px = 1.0;
        params.angle_deg = 0.0;
        params.per_row_jitter = 0.0;

        // P0 = blue at 40% alpha, P1 = opaque green, P2 = red at 20% alpha.
        let source: Vec<u8> = vec![
            0, 0, 255, 102, //
            0, 255, 0, 255, //
            255, 0, 0, 51,
        ];
        let mut out = vec![0u8; 12];
        rgb_split_fill_row(&mut out, 0, &source, 3, 1, &params);

        // x=1: R from P2 (r_pm = 0.2), G from P1 (g_pm = 1.0), B from P0 (b_pm = 0.4);
        // out_a = max(0.2, 1.0, 0.4) = 1.0 -> straight [51, 255, 102, 255].
        assert_eq!(&out[4..8], &[51, 255, 102, 255]);
    }

    #[test]
    fn digital_probability_one_moves_content() {
        let image = sample_image(16, 12);
        let mut params = base_params(InterferenceKind::Digital);
        params.probability = 1.0;
        params.max_shift_px = 8.0;
        params.rgb_split_px = 0.0;
        params.autogrow = false; // compare against source in-place

        let mut result = image.clone();
        apply_interference_effect(&mut result, &params);

        assert_eq!(result.width, image.width);
        assert_eq!(result.height, image.height);
        assert_ne!(result.rgba, image.rgba, "displacement did not move content");
    }
}
