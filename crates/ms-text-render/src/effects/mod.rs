/*
File: src/tabs/typing/render_next/effects/mod.rs

Purpose:
Корневой пакет effects нового рендера typing.

Main responsibilities:
- держать JSON-driven preprocess/post-effects pipeline отдельно от base layout/raster pipeline;
- маршрутизировать распарсенные эффекты в специализированные подмодули;
- изолировать новый `render_next` от effect-кода старого монолитного `render.rs`.

Notes:
- парсинг эффектов вынесен в `parse.rs`, чтобы JSON contract не смешивался с image math;
- preprocess-эффекты работают до inline-style parsing и генерируют inline-теги;
- общий image/math helper-слой лежит в `image_ops.rs`;
- конкретные эффекты разнесены по модулям `stroke_shadow`, `blur`, `glow`,
  `gradients`, `reflect_shake`, `dry_media`, `interference`.
*/

use super::types::RenderedTextImage;
use crate::raster::is_cancelled;
use ms_log::trace::cat;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

mod blur;
mod dry_media;
mod glow;
mod gradients;
mod image_ops;
mod interference;
mod parse;
mod reflect_shake;
mod stroke_shadow;

use blur::{apply_blur_effect, apply_motion_blur_effect};
use dry_media::apply_dry_media_effect;
use interference::apply_interference_effect;
use glow::{apply_glow_effect_v1, apply_glow_effect_v2, apply_soft_glow_effect};
use gradients::{apply_gradient2_effect, apply_gradient4_effect};
use parse::{
    EffectSpec, PreprocessEffectSpec, TextShakePreprocessParams, parse_effects_json,
    parse_preprocess_effects_json,
};
use reflect_shake::{apply_reflect_effect, apply_shake_effect};
use stroke_shadow::{apply_shadow_effect, apply_stroke_effect};

/// Apply the JSON post-effects pipeline to `image`.
///
/// Canvas-growing effects (stroke/shadow/glow/blur/...) advance
/// `content_origin_x/y`. This wrapper snapshots the origin at entry and, at the
/// SINGLE exit, shifts the image's extra-info centers by the accumulated origin
/// delta so they stay fixed relative to the glyphs no matter which effect (or
/// mid-chain cancellation) ended the pass. Individual effect modules never touch
/// `extra`.
pub(crate) fn apply_effects_pipeline(
    image: &mut RenderedTextImage,
    effects_json: &str,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<(), String> {
    let origin_before_x = image.content_origin_x;
    let origin_before_y = image.content_origin_y;
    let result = apply_effects_pipeline_inner(image, effects_json, cancel);
    // Shift the extra centers by however far the content origin moved during this
    // pass (0 for empty effects / immediate cancellation -> a no-op). Origins only
    // ever GROW, so compute the delta in integer space first: above 2^24 two adjacent
    // u32 values collapse to the same f32, so subtracting after the cast could drop a
    // 1px pad. `saturating_sub` yields a non-negative image-dimension delta, far below
    // f32's 2^24 integer-exact range, so the `as f32` is lossless.
    let dx = image.content_origin_x.saturating_sub(origin_before_x) as f32;
    let dy = image.content_origin_y.saturating_sub(origin_before_y) as f32;
    image.extra.shift(dx, dy);
    result
}

/// Run the parsed post-effects in order. Returns early (leaving the origin at its
/// last value) on empty effects or cooperative cancellation; the wrapper
/// [`apply_effects_pipeline`] performs the extra-center shift for every exit.
fn apply_effects_pipeline_inner(
    image: &mut RenderedTextImage,
    effects_json: &str,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<(), String> {
    let effects = parse_effects_json(effects_json)?;
    if effects.is_empty() {
        return Ok(());
    }
    let _effects_span = ms_log::trace_scope!(
        cat::RENDER,
        "apply_effects_pipeline count={} w={} h={}",
        effects.len(),
        image.width,
        image.height
    );
    for effect in effects {
        if is_cancelled(cancel) {
            return Ok(());
        }

        if ms_log::trace::trace_enabled() {
            let name = match &effect {
                EffectSpec::Stroke(_) => "stroke",
                EffectSpec::Shadow(_) => "shadow",
                EffectSpec::Blur(_) => "blur",
                EffectSpec::MotionBlur(_) => "motion_blur",
                EffectSpec::DryMedia(_) => "dry_media",
                EffectSpec::GlowV1(_) => "glow_v1",
                EffectSpec::GlowV2(_) => "glow_v2",
                EffectSpec::SoftGlow(_) => "soft_glow",
                EffectSpec::Gradient2(_) => "gradient2",
                EffectSpec::Gradient4(_) => "gradient4",
                EffectSpec::Reflect(_) => "reflect",
                EffectSpec::Shake(_) => "shake",
                EffectSpec::Interference(_) => "interference",
            };
            ms_log::trace_log!(cat::RENDER, "effect apply={}", name);
        }

        match effect {
            EffectSpec::Stroke(params) => apply_stroke_effect(image, &params),
            EffectSpec::Shadow(params) => apply_shadow_effect(image, &params),
            EffectSpec::Blur(params) => apply_blur_effect(image, &params),
            EffectSpec::MotionBlur(params) => apply_motion_blur_effect(image, &params),
            EffectSpec::DryMedia(params) => apply_dry_media_effect(image, &params),
            EffectSpec::GlowV1(params) => apply_glow_effect_v1(image, &params),
            EffectSpec::GlowV2(params) => apply_glow_effect_v2(image, &params),
            EffectSpec::SoftGlow(params) => apply_soft_glow_effect(image, &params),
            EffectSpec::Gradient2(params) => apply_gradient2_effect(image, &params),
            EffectSpec::Gradient4(params) => apply_gradient4_effect(image, &params),
            EffectSpec::Reflect(axis) => apply_reflect_effect(image, axis),
            EffectSpec::Shake(params) => apply_shake_effect(image, &params),
            EffectSpec::Interference(params) => apply_interference_effect(image, &params),
        }
    }

    Ok(())
}

pub(crate) fn apply_text_preprocess_effects(
    text: &str,
    effects_json: &str,
) -> Result<(String, bool), String> {
    let effects = parse_preprocess_effects_json(effects_json)?;
    let mut output = text.to_string();
    let mut generated_inline_tags = false;
    for effect in effects {
        match effect {
            PreprocessEffectSpec::TextShake(params) => {
                let should_generate_tags =
                    params.spread_x_px > f32::EPSILON || params.spread_y_px > f32::EPSILON;
                output = apply_text_shake_preprocess(output.as_str(), &params);
                generated_inline_tags |= should_generate_tags;
            }
        }
    }
    Ok((output, generated_inline_tags))
}

fn apply_text_shake_preprocess(text: &str, params: &TextShakePreprocessParams) -> String {
    if params.spread_x_px <= f32::EPSILON && params.spread_y_px <= f32::EPSILON {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len().saturating_mul(2));
    let mut char_idx = 0usize;
    let mut byte_idx = 0usize;
    while byte_idx < text.len() {
        let rest = &text[byte_idx..];
        let Some(ch) = rest.chars().next() else {
            break;
        };

        if ch == '<'
            && let Some(rel_end) = text[byte_idx + 1..].find('>')
        {
            let end = byte_idx + 1 + rel_end;
            out.push_str(&text[byte_idx..=end]);
            byte_idx = end + 1;
            continue;
        }

        if ch == '\n' || ch == '\r' {
            out.push(ch);
        } else {
            let offset_x = deterministic_text_shake_offset(
                params.seed,
                char_idx,
                0x9E37_79B9,
                params.spread_x_px,
            );
            let offset_y = deterministic_text_shake_offset(
                params.seed,
                char_idx,
                0x85EB_CA6B,
                params.spread_y_px,
            );
            out.push_str(format!("<offset={offset_x:.2},{offset_y:.2}>").as_str());
            out.push(ch);
            out.push_str("</offset>");
            char_idx = char_idx.saturating_add(1);
        }

        byte_idx += ch.len_utf8();
    }

    out
}

fn deterministic_text_shake_offset(seed: u64, char_idx: usize, salt: u64, spread_px: f32) -> f32 {
    if spread_px <= f32::EPSILON {
        return 0.0;
    }

    let mut value = seed ^ u64::try_from(char_idx).unwrap_or(u64::MAX) ^ salt;
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    let low_bits = u16::try_from(value & 0xFFFF).unwrap_or(0);
    let unit = f32::from(low_bits) / f32::from(u16::MAX);
    (unit * 2.0 - 1.0) * spread_px
}
