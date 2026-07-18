/*
File: src/tabs/typing/render_next/effects/dry_media.rs

Purpose:
Dry-media post-effects нового рендера typing.

Main responsibilities:
- применять pencil/chalk texture erosion поверх готового raster output;
- генерировать grain/dust noise без зависимости от старого `render.rs`;
- держать локальные regression tests на deterministic/noise-sensitive helper'ы.
*/

use super::super::raster::blend_pixel_over;
use super::super::types::RenderedTextImage;
use super::image_ops::{
    average_opaque_rgba, blend_full_image_over, draw_image_with_opacity,
    euclidean_distance_transform_to_mask, gaussian_blur_alpha_in_place, smoothstep01,
    value_noise_signed,
};
use super::parse::{DryMediaEffectMaterial, DryMediaEffectParams};
use rayon::prelude::*;

#[derive(Clone, Copy)]
struct DryMediaMaterialFactors {
    interior_erosion_mul: f32,
    pore_erosion_mul: f32,
    directional_mul: f32,
    dust_mul: f32,
    brightness_mul: f32,
}

pub(crate) fn apply_dry_media_effect(
    image: &mut RenderedTextImage,
    dry_media: &DryMediaEffectParams,
) {
    let strength = dry_media.strength.clamp(0.0, 1.0);
    if strength <= f32::EPSILON || image.width == 0 || image.height == 0 {
        return;
    }

    let pad = dry_media_required_pad_px(dry_media);
    let out_width = image.width.saturating_add(pad.saturating_mul(2));
    let out_height = image.height.saturating_add(pad.saturating_mul(2));
    if out_width == 0 || out_height == 0 {
        return;
    }
    let out_width_usize = out_width as usize;
    let out_height_usize = out_height as usize;

    let source = image.rgba.clone();
    let source_width = image.width as usize;
    let source_height = image.height as usize;
    let mut expanded_source = vec![0u8; out_width_usize * out_height_usize * 4];
    draw_image_with_opacity(
        expanded_source.as_mut_slice(),
        out_width_usize,
        out_height_usize,
        source.as_slice(),
        source_width,
        source_height,
        pad as i32,
        pad as i32,
        1.0,
    );

    let mut source_alpha = vec![0u8; out_width_usize * out_height_usize];
    let mut opaque_mask = vec![0u8; out_width_usize * out_height_usize];
    let mut has_source_alpha = false;
    for idx in 0..source_alpha.len() {
        let alpha = expanded_source[idx * 4 + 3];
        source_alpha[idx] = alpha;
        if alpha > 0 {
            opaque_mask[idx] = 1;
            has_source_alpha = true;
        }
    }
    if !has_source_alpha {
        return;
    }

    let mut inverse_mask = vec![0u8; opaque_mask.len()];
    for (idx, value) in inverse_mask.iter_mut().enumerate() {
        *value = if opaque_mask[idx] == 0 { 1 } else { 0 };
    }

    let outside_dist2 = euclidean_distance_transform_to_mask(
        opaque_mask.as_slice(),
        out_width_usize,
        out_height_usize,
    );
    let inside_dist2 = euclidean_distance_transform_to_mask(
        inverse_mask.as_slice(),
        out_width_usize,
        out_height_usize,
    );
    let source_average_color = average_opaque_rgba(expanded_source.as_slice());
    let tint_alpha_factor = if dry_media.use_source_color {
        1.0
    } else {
        dry_media.color[3] as f32 / 255.0
    };
    if !dry_media.use_source_color && tint_alpha_factor <= f32::EPSILON {
        return;
    }

    let factors = dry_media_material_factors(dry_media.material);
    let angle = dry_media.direction_deg.to_radians();
    let dir_x = angle.cos();
    let dir_y = angle.sin();
    let edge_band_px =
        (0.65 + dry_media.grain_scale_px * (0.45 + dry_media.edge_roughness * 0.9)).max(0.35);
    let stroke_period_px = (dry_media.grain_scale_px * 1.35).max(0.8);

    let mut out = vec![0u8; expanded_source.len()];
    let mut dust_alpha = vec![0u8; out_width_usize * out_height_usize];
    let out_row_stride = out_width_usize * 4;
    // Each output pixel writes only its own `out` slot and its own `dust_alpha` slot
    // from read-only inputs (noise, distance maps, source alpha), so the per-pixel
    // erosion/dust pass is parallelized over rows by zipping the two output buffers.
    out.par_chunks_mut(out_row_stride)
        .zip(dust_alpha.par_chunks_mut(out_width_usize))
        .enumerate()
        .for_each(|(y, (out_row, dust_row))| {
            for x in 0..out_width_usize {
                let idx = y * out_width_usize + x;
                // Row-local indices for the chunked output buffers; `expanded_idx` is the
                // global offset into the read-only full `expanded_source` buffer.
                let rgba_idx = x * 4;
                let dust_idx = x;
                let expanded_idx = idx * 4;
                let src_a = source_alpha[idx];
                let grain_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0x9E37_79B9),
                    x as f32,
                    y as f32,
                    dry_media.grain_scale_px.max(0.1),
                );
                let contour_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0xBF58_476D),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 2.4).max(0.2),
                );
                let blotch_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0x94D0_49BB),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 3.8).max(0.3),
                );

                if src_a > 0 {
                    let inside_dist = inside_dist2[idx].sqrt();
                    let edge_factor = 1.0 - smoothstep01(inside_dist / edge_band_px);
                    let grain_term = grain_noise * 0.5 + 0.5;
                    let contour_term = contour_noise * 0.5 + 0.5;
                    let blotch_term = blotch_noise * 0.5 + 0.5;
                    let stripe_factor = dry_media_stripe_factor(
                        x as f32,
                        y as f32,
                        dir_x,
                        dir_y,
                        stroke_period_px,
                        contour_noise,
                    );

                    let interior_erosion = strength
                        * dry_media.grain_amount
                        * factors.interior_erosion_mul
                        * (0.10 + grain_term * 0.20 + edge_factor * 0.15);
                    let edge_erosion = strength
                        * dry_media.edge_roughness
                        * edge_factor
                        * (0.12 + contour_term * 0.60);
                    let pore_threshold = (0.82 - dry_media.porosity * 0.42).clamp(0.10, 0.95);
                    let pore_factor =
                        ((blotch_term - pore_threshold) / (1.0 - pore_threshold)).clamp(0.0, 1.0);
                    let pore_erosion = strength
                        * dry_media.porosity
                        * factors.pore_erosion_mul
                        * pore_factor
                        * (0.30 + edge_factor * 0.45);
                    let directional_erosion = strength
                        * dry_media.directional_amount
                        * factors.directional_mul
                        * (1.0 - stripe_factor)
                        * (0.08 + grain_term * 0.18);
                    let erosion =
                        (interior_erosion + edge_erosion + pore_erosion + directional_erosion)
                            .clamp(0.0, 0.96);

                    let alpha_factor = if dry_media.use_source_color {
                        1.0
                    } else {
                        tint_alpha_factor
                    };
                    let out_a = (((src_a as f32) * (1.0 - erosion)) * alpha_factor)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    if out_a == 0 {
                        continue;
                    }

                    let brightness = (1.0
                        - strength
                            * factors.brightness_mul
                            * (0.10 * (grain_term - 0.5) + 0.10 * (stripe_factor - 0.5)))
                        .clamp(0.72, 1.20);
                    let (base_r, base_g, base_b) = if dry_media.use_source_color {
                        (
                            expanded_source[expanded_idx],
                            expanded_source[expanded_idx + 1],
                            expanded_source[expanded_idx + 2],
                        )
                    } else {
                        (dry_media.color[0], dry_media.color[1], dry_media.color[2])
                    };
                    out_row[rgba_idx] =
                        ((base_r as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out_row[rgba_idx + 1] =
                        ((base_g as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out_row[rgba_idx + 2] =
                        ((base_b as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out_row[rgba_idx + 3] = out_a;
                    continue;
                }

                let effective_dust = strength * dry_media.dust_amount * factors.dust_mul;
                if effective_dust <= f32::EPSILON || dry_media.dust_radius_px <= f32::EPSILON {
                    continue;
                }

                let outside_dist = outside_dist2[idx].sqrt();
                if outside_dist > dry_media.dust_radius_px.max(0.001) {
                    continue;
                }

                let dust_band =
                    1.0 - smoothstep01(outside_dist / dry_media.dust_radius_px.max(0.001));
                let dust_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0xD6E8_FD17),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 1.6).max(0.35),
                );
                let dust_term = (dust_noise * 0.5 + 0.5) * 0.55 + (blotch_noise * 0.5 + 0.5) * 0.45;
                let dust_threshold = match dry_media.material {
                    DryMediaEffectMaterial::Pencil => 0.72,
                    DryMediaEffectMaterial::Chalk => 0.58,
                };
                let sparse_dust =
                    ((dust_term - dust_threshold) / (1.0 - dust_threshold)).clamp(0.0, 1.0);
                let dust_alpha_f = (effective_dust * dust_band * sparse_dust).clamp(0.0, 1.0);
                dust_row[dust_idx] = (dust_alpha_f * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        });

    if dry_media.softness_px > f32::EPSILON && dust_alpha.iter().any(|&value| value > 0) {
        gaussian_blur_alpha_in_place(
            &mut dust_alpha,
            out_width,
            out_height,
            dry_media.softness_px,
        );
    }

    let text_layer = out.clone();
    out.fill(0);
    let dust_color = if dry_media.use_source_color {
        source_average_color
    } else {
        dry_media.color
    };
    let dust_color_alpha_factor = if dry_media.use_source_color {
        1.0
    } else {
        dry_media.color[3] as f32 / 255.0
    };
    if dust_color_alpha_factor > f32::EPSILON {
        for (idx, alpha) in dust_alpha.iter().copied().enumerate() {
            if alpha == 0 {
                continue;
            }
            let dust_a = ((alpha as f32) * dust_color_alpha_factor)
                .round()
                .clamp(0.0, 255.0) as u8;
            if dust_a == 0 {
                continue;
            }
            let rgba_idx = idx * 4;
            blend_pixel_over(
                &mut out[rgba_idx..rgba_idx + 4],
                dust_color[0],
                dust_color[1],
                dust_color[2],
                dust_a,
            );
        }
    }
    blend_full_image_over(&mut out, text_layer.as_slice());

    image.width = out_width;
    image.height = out_height;
    image.rgba = out;
    // Контент вставлен в (pad, pad) внутри увеличенного буфера.
    image.content_origin_x = image.content_origin_x.saturating_add(pad);
    image.content_origin_y = image.content_origin_y.saturating_add(pad);
}

fn dry_media_material_factors(material: DryMediaEffectMaterial) -> DryMediaMaterialFactors {
    match material {
        DryMediaEffectMaterial::Pencil => DryMediaMaterialFactors {
            interior_erosion_mul: 0.75,
            pore_erosion_mul: 0.80,
            directional_mul: 1.00,
            dust_mul: 0.65,
            brightness_mul: 0.70,
        },
        DryMediaEffectMaterial::Chalk => DryMediaMaterialFactors {
            interior_erosion_mul: 1.15,
            pore_erosion_mul: 1.20,
            directional_mul: 0.35,
            dust_mul: 1.20,
            brightness_mul: 1.00,
        },
    }
}

fn dry_media_required_pad_px(dry_media: &DryMediaEffectParams) -> u32 {
    if dry_media.dust_amount <= f32::EPSILON || dry_media.dust_radius_px <= f32::EPSILON {
        return 0;
    }

    (dry_media.dust_radius_px + dry_media.softness_px.max(0.0) * 3.0)
        .ceil()
        .max(0.0) as u32
}

fn dry_media_stripe_factor(
    x: f32,
    y: f32,
    dir_x: f32,
    dir_y: f32,
    period_px: f32,
    jitter: f32,
) -> f32 {
    let period_px = period_px.max(0.25);
    let phase = ((x * dir_x + y * dir_y) / period_px) + jitter * 1.35;
    (phase.sin() * 0.5 + 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::super::image_ops::{
        draw_image_with_opacity, euclidean_distance_transform_to_mask, smoothstep01,
        value_noise_signed,
    };
    use super::super::parse::{DryMediaEffectMaterial, DryMediaEffectParams};
    use super::{
        apply_dry_media_effect, dry_media_material_factors, dry_media_required_pad_px,
        dry_media_stripe_factor,
    };
    use crate::types::RenderedTextImage;

    /// Sequential reference for dry-media erosion/dust, reusing the same private kernels
    /// the production loop calls. Only the loop's parallelization differs, so the output
    /// must be bit-identical. Dust compositing is not part of the parallel loop, so the
    /// comparison checks the per-pixel erosion buffer and the raw dust-alpha buffer.
    // Test-only helper returning the two compared buffers; a named type would add indirection
    // without clarifying this single local return, so the tuple is left inline.
    #[allow(clippy::type_complexity)]
    fn dry_media_erosion_and_dust_seq(
        image: &RenderedTextImage,
        dry_media: &DryMediaEffectParams,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let strength = dry_media.strength.clamp(0.0, 1.0);
        if strength <= f32::EPSILON || image.width == 0 || image.height == 0 {
            return None;
        }
        let pad = dry_media_required_pad_px(dry_media);
        let out_width = image.width.saturating_add(pad.saturating_mul(2));
        let out_height = image.height.saturating_add(pad.saturating_mul(2));
        let out_width_usize = out_width as usize;
        let out_height_usize = out_height as usize;
        let source = image.rgba.clone();
        let mut expanded_source = vec![0u8; out_width_usize * out_height_usize * 4];
        draw_image_with_opacity(
            expanded_source.as_mut_slice(),
            out_width_usize,
            out_height_usize,
            source.as_slice(),
            image.width as usize,
            image.height as usize,
            pad as i32,
            pad as i32,
            1.0,
        );
        let mut source_alpha = vec![0u8; out_width_usize * out_height_usize];
        let mut opaque_mask = vec![0u8; out_width_usize * out_height_usize];
        for idx in 0..source_alpha.len() {
            let alpha = expanded_source[idx * 4 + 3];
            source_alpha[idx] = alpha;
            if alpha > 0 {
                opaque_mask[idx] = 1;
            }
        }
        let mut inverse_mask = vec![0u8; opaque_mask.len()];
        for (idx, value) in inverse_mask.iter_mut().enumerate() {
            *value = if opaque_mask[idx] == 0 { 1 } else { 0 };
        }
        let outside_dist2 = euclidean_distance_transform_to_mask(
            opaque_mask.as_slice(),
            out_width_usize,
            out_height_usize,
        );
        let inside_dist2 = euclidean_distance_transform_to_mask(
            inverse_mask.as_slice(),
            out_width_usize,
            out_height_usize,
        );
        let tint_alpha_factor = if dry_media.use_source_color {
            1.0
        } else {
            dry_media.color[3] as f32 / 255.0
        };
        let factors = dry_media_material_factors(dry_media.material);
        let angle = dry_media.direction_deg.to_radians();
        let dir_x = angle.cos();
        let dir_y = angle.sin();
        let edge_band_px =
            (0.65 + dry_media.grain_scale_px * (0.45 + dry_media.edge_roughness * 0.9)).max(0.35);
        let stroke_period_px = (dry_media.grain_scale_px * 1.35).max(0.8);

        let mut out = vec![0u8; expanded_source.len()];
        let mut dust_alpha = vec![0u8; out_width_usize * out_height_usize];
        for y in 0..out_height_usize {
            for x in 0..out_width_usize {
                let idx = y * out_width_usize + x;
                let rgba_idx = idx * 4;
                let src_a = source_alpha[idx];
                let grain_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0x9E37_79B9),
                    x as f32,
                    y as f32,
                    dry_media.grain_scale_px.max(0.1),
                );
                let contour_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0xBF58_476D),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 2.4).max(0.2),
                );
                let blotch_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0x94D0_49BB),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 3.8).max(0.3),
                );
                if src_a > 0 {
                    let inside_dist = inside_dist2[idx].sqrt();
                    let edge_factor = 1.0 - smoothstep01(inside_dist / edge_band_px);
                    let grain_term = grain_noise * 0.5 + 0.5;
                    let contour_term = contour_noise * 0.5 + 0.5;
                    let blotch_term = blotch_noise * 0.5 + 0.5;
                    let stripe_factor = dry_media_stripe_factor(
                        x as f32,
                        y as f32,
                        dir_x,
                        dir_y,
                        stroke_period_px,
                        contour_noise,
                    );
                    let interior_erosion = strength
                        * dry_media.grain_amount
                        * factors.interior_erosion_mul
                        * (0.10 + grain_term * 0.20 + edge_factor * 0.15);
                    let edge_erosion = strength
                        * dry_media.edge_roughness
                        * edge_factor
                        * (0.12 + contour_term * 0.60);
                    let pore_threshold = (0.82 - dry_media.porosity * 0.42).clamp(0.10, 0.95);
                    let pore_factor =
                        ((blotch_term - pore_threshold) / (1.0 - pore_threshold)).clamp(0.0, 1.0);
                    let pore_erosion = strength
                        * dry_media.porosity
                        * factors.pore_erosion_mul
                        * pore_factor
                        * (0.30 + edge_factor * 0.45);
                    let directional_erosion = strength
                        * dry_media.directional_amount
                        * factors.directional_mul
                        * (1.0 - stripe_factor)
                        * (0.08 + grain_term * 0.18);
                    let erosion =
                        (interior_erosion + edge_erosion + pore_erosion + directional_erosion)
                            .clamp(0.0, 0.96);
                    let alpha_factor = if dry_media.use_source_color {
                        1.0
                    } else {
                        tint_alpha_factor
                    };
                    let out_a = (((src_a as f32) * (1.0 - erosion)) * alpha_factor)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    if out_a == 0 {
                        continue;
                    }
                    let brightness = (1.0
                        - strength
                            * factors.brightness_mul
                            * (0.10 * (grain_term - 0.5) + 0.10 * (stripe_factor - 0.5)))
                        .clamp(0.72, 1.20);
                    let (base_r, base_g, base_b) = if dry_media.use_source_color {
                        (
                            expanded_source[rgba_idx],
                            expanded_source[rgba_idx + 1],
                            expanded_source[rgba_idx + 2],
                        )
                    } else {
                        (dry_media.color[0], dry_media.color[1], dry_media.color[2])
                    };
                    out[rgba_idx] = ((base_r as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out[rgba_idx + 1] =
                        ((base_g as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out[rgba_idx + 2] =
                        ((base_b as f32) * brightness).round().clamp(0.0, 255.0) as u8;
                    out[rgba_idx + 3] = out_a;
                    continue;
                }
                let effective_dust = strength * dry_media.dust_amount * factors.dust_mul;
                if effective_dust <= f32::EPSILON || dry_media.dust_radius_px <= f32::EPSILON {
                    continue;
                }
                let outside_dist = outside_dist2[idx].sqrt();
                if outside_dist > dry_media.dust_radius_px.max(0.001) {
                    continue;
                }
                let dust_band =
                    1.0 - smoothstep01(outside_dist / dry_media.dust_radius_px.max(0.001));
                let dust_noise = value_noise_signed(
                    dry_media.seed.wrapping_add(0xD6E8_FD17),
                    x as f32,
                    y as f32,
                    (dry_media.grain_scale_px * 1.6).max(0.35),
                );
                let dust_term = (dust_noise * 0.5 + 0.5) * 0.55 + (blotch_noise * 0.5 + 0.5) * 0.45;
                let dust_threshold = match dry_media.material {
                    DryMediaEffectMaterial::Pencil => 0.72,
                    DryMediaEffectMaterial::Chalk => 0.58,
                };
                let sparse_dust =
                    ((dust_term - dust_threshold) / (1.0 - dust_threshold)).clamp(0.0, 1.0);
                let dust_alpha_f = (effective_dust * dust_band * sparse_dust).clamp(0.0, 1.0);
                dust_alpha[idx] = (dust_alpha_f * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        Some((out, dust_alpha))
    }

    fn sample_dry_media_image() -> RenderedTextImage {
        let width = 5usize;
        let height = 5usize;
        let mut rgba = vec![0u8; width * height * 4];
        for y in 1..4 {
            for x in 1..4 {
                let idx = (y * width + x) * 4;
                rgba[idx] = 32;
                rgba[idx + 1] = 32;
                rgba[idx + 2] = 32;
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

    fn sample_dry_media_params() -> DryMediaEffectParams {
        DryMediaEffectParams {
            material: DryMediaEffectMaterial::Pencil,
            strength: 0.65,
            seed: 7,
            grain_scale_px: 2.0,
            grain_amount: 0.35,
            edge_roughness: 0.45,
            porosity: 0.2,
            direction_deg: 82.0,
            directional_amount: 0.3,
            dust_amount: 0.08,
            dust_radius_px: 2.0,
            softness_px: 0.6,
            use_source_color: true,
            color: [0, 0, 0, 255],
        }
    }

    #[test]
    fn dry_media_parallel_loop_matches_sequential_reference() {
        use super::super::image_ops::{
            average_opaque_rgba, blend_full_image_over, gaussian_blur_alpha_in_place,
        };
        use crate::raster::blend_pixel_over;

        let image = sample_dry_media_image();
        let params = sample_dry_media_params();

        // Parallel production output.
        let mut parallel = image.clone();
        apply_dry_media_effect(&mut parallel, &params);

        // Sequential reconstruction: same erosion/dust loop plus identical post-steps.
        let (out_seq, mut dust_alpha) =
            dry_media_erosion_and_dust_seq(&image, &params).expect("effect active for params");
        let pad = dry_media_required_pad_px(&params);
        let out_width = image.width.saturating_add(pad.saturating_mul(2));
        let out_height = image.height.saturating_add(pad.saturating_mul(2));

        // Rebuild the expanded source to recover the average color used for dust tint.
        let mut expanded_source = vec![0u8; out_width as usize * out_height as usize * 4];
        super::super::image_ops::draw_image_with_opacity(
            expanded_source.as_mut_slice(),
            out_width as usize,
            out_height as usize,
            image.rgba.as_slice(),
            image.width as usize,
            image.height as usize,
            pad as i32,
            pad as i32,
            1.0,
        );
        let source_average_color = average_opaque_rgba(expanded_source.as_slice());

        let mut out = out_seq;
        if params.softness_px > f32::EPSILON && dust_alpha.iter().any(|&value| value > 0) {
            gaussian_blur_alpha_in_place(
                &mut dust_alpha,
                out_width,
                out_height,
                params.softness_px,
            );
        }
        let text_layer = out.clone();
        out.fill(0);
        let dust_color = if params.use_source_color {
            source_average_color
        } else {
            params.color
        };
        let dust_color_alpha_factor = if params.use_source_color {
            1.0
        } else {
            params.color[3] as f32 / 255.0
        };
        if dust_color_alpha_factor > f32::EPSILON {
            for (idx, alpha) in dust_alpha.iter().copied().enumerate() {
                if alpha == 0 {
                    continue;
                }
                let dust_a = ((alpha as f32) * dust_color_alpha_factor)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                if dust_a == 0 {
                    continue;
                }
                let rgba_idx = idx * 4;
                blend_pixel_over(
                    &mut out[rgba_idx..rgba_idx + 4],
                    dust_color[0],
                    dust_color[1],
                    dust_color[2],
                    dust_a,
                );
            }
        }
        blend_full_image_over(&mut out, text_layer.as_slice());

        assert_eq!(parallel.width, out_width);
        assert_eq!(parallel.height, out_height);
        assert_eq!(parallel.rgba, out);
    }

    #[test]
    fn dry_media_effect_is_deterministic_for_same_seed() {
        let mut first = sample_dry_media_image();
        let mut second = sample_dry_media_image();
        let params = sample_dry_media_params();

        apply_dry_media_effect(&mut first, &params);
        apply_dry_media_effect(&mut second, &params);

        assert_eq!(first.width, second.width);
        assert_eq!(first.height, second.height);
        assert_eq!(first.rgba, second.rgba);
    }

    #[test]
    fn dry_media_effect_grows_canvas_when_dust_is_enabled() {
        let mut image = sample_dry_media_image();
        let params = sample_dry_media_params();
        let original_width = image.width;
        let original_height = image.height;

        apply_dry_media_effect(&mut image, &params);

        assert!(image.width > original_width);
        assert!(image.height > original_height);
    }

    #[test]
    fn dry_media_effect_reduces_some_fill_alpha() {
        let mut image = sample_dry_media_image();
        let params = sample_dry_media_params();

        apply_dry_media_effect(&mut image, &params);

        assert!(
            image
                .rgba
                .chunks_exact(4)
                .any(|chunk| chunk[3] > 0 && chunk[3] < 255),
            "expected partially eroded alpha"
        );
    }
}
