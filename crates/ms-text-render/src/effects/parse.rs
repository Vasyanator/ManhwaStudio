/*
File: src/tabs/typing/render_next/effects/parse.rs

Purpose:
Парсинг JSON-конфига effects для нового рендера typing.

Main responsibilities:
- валидировать внешний effects JSON contract отдельно от effect-реализаций;
- преобразовывать JSON-объекты в typed params/enum для конкретных effect-модулей;
- отделять preprocess/base-render/post-effect стадии без поломки старых JSON;
- держать совместимые имена полей и alias'ы из старого `render.rs`.
*/

use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) enum EffectSpec {
    Stroke(StrokeEffectParams),
    Shadow(ShadowEffectParams),
    Blur(BlurEffectParams),
    MotionBlur(MotionBlurEffectParams),
    DryMedia(DryMediaEffectParams),
    GlowV1(GlowEffectParams),
    GlowV2(GlowEffectParams),
    SoftGlow(SoftGlowEffectParams),
    Gradient2(Gradient2EffectParams),
    Gradient4(Gradient4EffectParams),
    Reflect(ReflectAxis),
    Shake(ShakeEffectParams),
    Interference(InterferenceEffectParams),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectStage {
    Preprocess,
    BaseRender,
    PostEffect,
}

#[derive(Debug, Clone)]
pub(crate) enum PreprocessEffectSpec {
    TextShake(TextShakePreprocessParams),
}

#[derive(Debug, Clone)]
pub(crate) struct TextShakePreprocessParams {
    pub(crate) spread_x_px: f32,
    pub(crate) spread_y_px: f32,
    pub(crate) seed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReflectAxis {
    X,
    Y,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrokeOpacityMode {
    Static,
    FromContour,
}

#[derive(Debug, Clone)]
pub(crate) struct StrokeEffectParams {
    pub(crate) width_px: f32,
    pub(crate) color: [u8; 4],
    pub(crate) opacity_mode: StrokeOpacityMode,
    pub(crate) transparency_percent: f32,
    pub(crate) smoothing_enabled: bool,
    pub(crate) smoothing_strength_percent: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct ShadowEffectParams {
    pub(crate) offset_x: i32,
    pub(crate) offset_y: i32,
    pub(crate) transparency_percent: f32,
    pub(crate) blur_radius_px: f32,
    pub(crate) use_source_color: bool,
    pub(crate) color: [u8; 4],
}

#[derive(Debug, Clone)]
pub(crate) struct ShakeEffectParams {
    pub(crate) angle_deg: f32,
    pub(crate) up_px: f32,
    pub(crate) down_px: f32,
    pub(crate) steps: u32,
    pub(crate) base_fade: f32,
    pub(crate) decay: f32,
    pub(crate) blur_px: u32,
    pub(crate) autogrow: bool,
    pub(crate) grow_margin_px: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct BlurEffectParams {
    pub(crate) radius_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MotionBlurSharpCopyMode {
    None,
    Over,
    Under,
}

#[derive(Debug, Clone)]
pub(crate) struct MotionBlurEffectParams {
    pub(crate) angle_deg: f32,
    pub(crate) distance_px: f32,
    pub(crate) sharp_copy_mode: MotionBlurSharpCopyMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DryMediaEffectMaterial {
    Pencil,
    Chalk,
}

#[derive(Debug, Clone)]
pub(crate) struct DryMediaEffectParams {
    pub(crate) material: DryMediaEffectMaterial,
    pub(crate) strength: f32,
    pub(crate) seed: u64,
    pub(crate) grain_scale_px: f32,
    pub(crate) grain_amount: f32,
    pub(crate) edge_roughness: f32,
    pub(crate) porosity: f32,
    pub(crate) direction_deg: f32,
    pub(crate) directional_amount: f32,
    pub(crate) dust_amount: f32,
    pub(crate) dust_radius_px: f32,
    pub(crate) softness_px: f32,
    pub(crate) use_source_color: bool,
    pub(crate) color: [u8; 4],
}

/// Sub-kind of the `interference` (помехи/glitch) post-effect. Every kind reads the full
/// `InterferenceEffectParams` (the UI serializes all keys); only the fields relevant to the
/// selected kind take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterferenceKind {
    /// Per-pixel static: modulates lightness (and optionally alpha) of existing pixels.
    WhiteNoise,
    /// Horizontal band displacement with per-band RGB channel split (digital glitch).
    Digital,
    /// Chromatic aberration: R/B channels sampled offset along an angle.
    RgbSplit,
    /// Periodic darkened horizontal scanlines with optional per-line horizontal jitter.
    Scanlines,
}

/// Parameters of the `interference` post-effect. Holds the fields of all four sub-kinds; the
/// active `kind` selects which subset the renderer applies. Seed-stable: a given `seed`
/// reproduces the same output.
#[derive(Debug, Clone)]
pub(crate) struct InterferenceEffectParams {
    pub(crate) kind: InterferenceKind,
    pub(crate) seed: u64,
    // white_noise
    pub(crate) amount: f32,
    pub(crate) scale_px: f32,
    pub(crate) density: f32,
    pub(crate) monochrome: bool,
    pub(crate) alpha_noise: f32,
    // digital
    pub(crate) slice_height_px: i32,
    pub(crate) height_jitter: f32,
    pub(crate) max_shift_px: f32,
    pub(crate) probability: f32,
    pub(crate) rgb_split_px: f32,
    pub(crate) autogrow: bool,
    // rgb_split
    pub(crate) offset_px: f32,
    pub(crate) angle_deg: f32,
    pub(crate) per_row_jitter: f32,
    // scanlines
    pub(crate) line_height_px: i32,
    pub(crate) gap_px: i32,
    pub(crate) darken: f32,
    pub(crate) jitter_px: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct GlowEffectParams {
    pub(crate) radius_px: f32,
    pub(crate) color: [u8; 4],
    pub(crate) opacity_mode: StrokeOpacityMode,
    pub(crate) transparency_percent: f32,
    pub(crate) fade_strength: f32,
    pub(crate) fade_shift: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct SoftGlowEffectParams {
    pub(crate) radius_steps: u32,
    pub(crate) softness_px: f32,
    pub(crate) color: [u8; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gradient2FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Debug, Clone)]
pub(crate) struct Gradient2EffectParams {
    pub(crate) color1: [u8; 4],
    pub(crate) color2: [u8; 4],
    pub(crate) angle_deg: f32,
    pub(crate) width_percent: f32,
    pub(crate) respect_source_alpha: bool,
    pub(crate) fill_mode: Gradient2FillMode,
    pub(crate) target_color: [u8; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gradient4FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Debug, Clone)]
pub(crate) struct Gradient4EffectParams {
    pub(crate) color_top_left: [u8; 4],
    pub(crate) color_top_right: [u8; 4],
    pub(crate) color_bottom_left: [u8; 4],
    pub(crate) color_bottom_right: [u8; 4],
    pub(crate) width_percent: f32,
    pub(crate) respect_source_alpha: bool,
    pub(crate) fill_mode: Gradient4FillMode,
    pub(crate) target_color: [u8; 4],
}

pub(crate) fn parse_effects_json(effects_json: &str) -> Result<Vec<EffectSpec>, String> {
    let effects_json = effects_json.trim();
    if effects_json.is_empty() {
        return Ok(Vec::new());
    }

    let root: Value = serde_json::from_str(effects_json)
        .map_err(|error| format!("ошибка парсинга effects json: {error}"))?;
    let effects = root
        .as_array()
        .ok_or_else(|| "effects json должен быть массивом".to_string())?;

    let mut parsed = Vec::with_capacity(effects.len());
    for (idx, effect) in effects.iter().enumerate() {
        let obj = effect
            .as_object()
            .ok_or_else(|| format!("effects[{idx}] должен быть объектом"))?;
        let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if !enabled {
            continue;
        }
        if parse_effect_stage(obj) != EffectStage::PostEffect {
            continue;
        }

        let effect_name = obj
            .get("effect")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .map(|value| value.trim().to_lowercase())
            .ok_or_else(|| format!("effects[{idx}] должен содержать поле effect/type"))?;

        let effect = match effect_name.as_str() {
            "stroke" => EffectSpec::Stroke(parse_stroke_effect_params(obj)?),
            "shadow" => EffectSpec::Shadow(parse_shadow_effect_params(obj)?),
            "blur" | "gaussian_blur" => EffectSpec::Blur(parse_blur_effect_params(obj)?),
            "motion_blur" | "directional_blur" => {
                EffectSpec::MotionBlur(parse_motion_blur_effect_params(obj)?)
            }
            "dry_media" | "chalk_pencil" | "dry_brush" => {
                EffectSpec::DryMedia(parse_dry_media_effect_params(obj)?)
            }
            "glow_v1" => EffectSpec::GlowV1(parse_glow_effect_params(obj)?),
            "glow_v2" | "glow" => EffectSpec::GlowV2(parse_glow_effect_params(obj)?),
            "soft_glow" | "glow_soft" => EffectSpec::SoftGlow(parse_soft_glow_effect_params(obj)?),
            "gradient2" => EffectSpec::Gradient2(parse_gradient2_effect_params(obj)?),
            "gradient4" => EffectSpec::Gradient4(parse_gradient4_effect_params(obj)?),
            "reflect" | "mirror" | "flip" => EffectSpec::Reflect(parse_reflect_axis(obj)?),
            "shake" => EffectSpec::Shake(parse_shake_effect_params(obj)?),
            "interference" => {
                EffectSpec::Interference(parse_interference_effect_params(obj)?)
            }
            other => {
                return Err(format!(
                    "effects[{idx}]: эффект '{other}' пока не поддержан"
                ));
            }
        };

        parsed.push(effect);
    }

    Ok(parsed)
}

pub(crate) fn parse_preprocess_effects_json(
    effects_json: &str,
) -> Result<Vec<PreprocessEffectSpec>, String> {
    let effects_json = effects_json.trim();
    if effects_json.is_empty() {
        return Ok(Vec::new());
    }

    let root: Value = serde_json::from_str(effects_json)
        .map_err(|error| format!("ошибка парсинга effects json: {error}"))?;
    let effects = root
        .as_array()
        .ok_or_else(|| "effects json должен быть массивом".to_string())?;

    let mut parsed = Vec::new();
    for (idx, effect) in effects.iter().enumerate() {
        let obj = effect
            .as_object()
            .ok_or_else(|| format!("effects[{idx}] должен быть объектом"))?;
        let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if !enabled || parse_effect_stage(obj) != EffectStage::Preprocess {
            continue;
        }

        let effect_name = obj
            .get("effect")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .map(|value| value.trim().to_lowercase())
            .ok_or_else(|| format!("effects[{idx}] должен содержать поле effect/type"))?;

        let effect = match effect_name.as_str() {
            "text_shake" | "text_jitter" | "character_shake" => {
                PreprocessEffectSpec::TextShake(parse_text_shake_preprocess_params(obj)?)
            }
            other => {
                return Err(format!(
                    "effects[{idx}]: preprocess-эффект '{other}' пока не поддержан"
                ));
            }
        };

        parsed.push(effect);
    }

    Ok(parsed)
}

pub(crate) fn parse_effect_stage(obj: &serde_json::Map<String, Value>) -> EffectStage {
    obj.get("effect_type")
        .or_else(|| obj.get("stage"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .map_or(EffectStage::PostEffect, |value| match value.as_str() {
            "preprocess" | "pre_processor" | "pre-processing" | "text_preprocess" => {
                EffectStage::Preprocess
            }
            "base_render" | "base-render" | "base" | "render" => EffectStage::BaseRender,
            _ => EffectStage::PostEffect,
        })
}

fn parse_text_shake_preprocess_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<TextShakePreprocessParams, String> {
    Ok(TextShakePreprocessParams {
        spread_x_px: parse_effect_f32_range(
            obj.get("spread_x")
                .or_else(|| obj.get("spread_x_px"))
                .or_else(|| obj.get("x")),
            "text_shake.spread_x",
            2.0,
            0.0,
            256.0,
        )?,
        spread_y_px: parse_effect_f32_range(
            obj.get("spread_y")
                .or_else(|| obj.get("spread_y_px"))
                .or_else(|| obj.get("y")),
            "text_shake.spread_y",
            2.0,
            0.0,
            256.0,
        )?,
        seed: parse_effect_u64(obj.get("seed"), "text_shake.seed", 1)?,
    })
}

fn parse_stroke_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<StrokeEffectParams, String> {
    let width_px = parse_effect_f32_range(
        obj.get("width").or_else(|| obj.get("width_px")),
        "stroke.width",
        2.0,
        0.0,
        64.0,
    )?;
    Ok(StrokeEffectParams {
        width_px,
        color: parse_effect_color(obj.get("color"))?.unwrap_or([0, 0, 0, 255]),
        opacity_mode: parse_stroke_opacity_mode(obj)?,
        transparency_percent: parse_stroke_transparency_percent(obj)?,
        smoothing_enabled: parse_stroke_smoothing_enabled(obj)?,
        smoothing_strength_percent: parse_stroke_smoothing_strength_percent(obj)?,
    })
}

fn parse_shadow_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<ShadowEffectParams, String> {
    Ok(ShadowEffectParams {
        offset_x: parse_effect_i32(
            obj.get("offset_x")
                .or_else(|| obj.get("x"))
                .or_else(|| obj.get("dx")),
            "offset_x",
            4,
            -8192,
            8192,
        )?,
        offset_y: parse_effect_i32(
            obj.get("offset_y")
                .or_else(|| obj.get("y"))
                .or_else(|| obj.get("dy")),
            "offset_y",
            4,
            -8192,
            8192,
        )?,
        transparency_percent: parse_shadow_transparency_percent(obj)?,
        blur_radius_px: parse_shadow_blur_radius_px(obj)?,
        use_source_color: parse_shadow_use_source_color(obj),
        color: parse_effect_color(obj.get("color"))?.unwrap_or([0, 0, 0, 255]),
    })
}

fn parse_reflect_axis(obj: &serde_json::Map<String, Value>) -> Result<ReflectAxis, String> {
    let Some(axis) = obj.get("axis").and_then(Value::as_str) else {
        return Ok(ReflectAxis::Y);
    };

    match axis.trim().to_ascii_lowercase().as_str() {
        "x" | "horizontal_axis" => Ok(ReflectAxis::X),
        "y" | "vertical_axis" => Ok(ReflectAxis::Y),
        other => Err(format!(
            "reflect.axis: неизвестное значение '{other}', ожидалось x/y"
        )),
    }
}

fn parse_blur_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<BlurEffectParams, String> {
    Ok(BlurEffectParams {
        radius_px: parse_effect_f32_range(
            obj.get("radius")
                .or_else(|| obj.get("radius_px"))
                .or_else(|| obj.get("blur"))
                .or_else(|| obj.get("blur_px"))
                .or_else(|| obj.get("sigma")),
            "blur.radius",
            4.0,
            0.0,
            512.0,
        )?,
    })
}

fn parse_motion_blur_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<MotionBlurEffectParams, String> {
    Ok(MotionBlurEffectParams {
        angle_deg: parse_effect_f32_range(
            obj.get("angle_deg")
                .or_else(|| obj.get("angle"))
                .or_else(|| obj.get("direction_deg")),
            "motion_blur.angle_deg",
            20.0,
            -3600.0,
            3600.0,
        )?,
        distance_px: parse_effect_f32_range(
            obj.get("distance")
                .or_else(|| obj.get("distance_px"))
                .or_else(|| obj.get("offset"))
                .or_else(|| obj.get("offset_px")),
            "motion_blur.distance",
            11.0,
            0.0,
            8192.0,
        )?,
        sharp_copy_mode: parse_motion_blur_sharp_copy_mode(obj)?,
    })
}

fn parse_motion_blur_sharp_copy_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<MotionBlurSharpCopyMode, String> {
    let Some(mode) = obj
        .get("sharp_copy")
        .or_else(|| obj.get("unblurred_copy"))
        .or_else(|| obj.get("sharp_copy_mode"))
        .and_then(Value::as_str)
    else {
        return Ok(MotionBlurSharpCopyMode::None);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" => Ok(MotionBlurSharpCopyMode::None),
        "over" | "top" | "above" => Ok(MotionBlurSharpCopyMode::Over),
        "under" | "bottom" | "below" => Ok(MotionBlurSharpCopyMode::Under),
        other => Err(format!(
            "motion_blur.sharp_copy: неизвестное значение '{other}', ожидалось none/over/under"
        )),
    }
}

fn parse_dry_media_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<DryMediaEffectParams, String> {
    let material = match obj
        .get("material")
        .and_then(Value::as_str)
        .unwrap_or("pencil")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "chalk" | "mel" => DryMediaEffectMaterial::Chalk,
        "pencil" | "karandash" | "graphite" => DryMediaEffectMaterial::Pencil,
        other => {
            return Err(format!(
                "dry_media.material: неизвестное значение '{other}', ожидалось pencil/chalk"
            ));
        }
    };

    Ok(DryMediaEffectParams {
        material,
        strength: parse_effect_f32_range(
            obj.get("strength"),
            "dry_media.strength",
            0.65,
            0.0,
            1.0,
        )?,
        seed: parse_effect_u64(obj.get("seed"), "dry_media.seed", 1)?,
        grain_scale_px: parse_effect_f32_range(
            obj.get("grain_scale_px")
                .or_else(|| obj.get("grain_scale"))
                .or_else(|| obj.get("grain_size_px")),
            "dry_media.grain_scale_px",
            2.0,
            0.1,
            256.0,
        )?,
        grain_amount: parse_effect_f32_range(
            obj.get("grain_amount").or_else(|| obj.get("grain")),
            "dry_media.grain_amount",
            0.35,
            0.0,
            1.0,
        )?,
        edge_roughness: parse_effect_f32_range(
            obj.get("edge_roughness").or_else(|| obj.get("roughness")),
            "dry_media.edge_roughness",
            0.45,
            0.0,
            1.0,
        )?,
        porosity: parse_effect_f32_range(
            obj.get("porosity").or_else(|| obj.get("holes")),
            "dry_media.porosity",
            0.20,
            0.0,
            1.0,
        )?,
        direction_deg: parse_effect_f32_range(
            obj.get("direction_deg")
                .or_else(|| obj.get("angle_deg"))
                .or_else(|| obj.get("angle")),
            "dry_media.direction_deg",
            82.0,
            -3600.0,
            3600.0,
        )?,
        directional_amount: parse_effect_f32_range(
            obj.get("directional_amount")
                .or_else(|| obj.get("stroke_amount"))
                .or_else(|| obj.get("hatching")),
            "dry_media.directional_amount",
            0.30,
            0.0,
            1.0,
        )?,
        dust_amount: parse_effect_f32_range(
            obj.get("dust_amount").or_else(|| obj.get("dust")),
            "dry_media.dust_amount",
            0.08,
            0.0,
            1.0,
        )?,
        dust_radius_px: parse_effect_f32_range(
            obj.get("dust_radius_px").or_else(|| obj.get("dust_radius")),
            "dry_media.dust_radius_px",
            2.0,
            0.0,
            128.0,
        )?,
        softness_px: parse_effect_f32_range(
            obj.get("softness_px")
                .or_else(|| obj.get("softness"))
                .or_else(|| obj.get("blur")),
            "dry_media.softness_px",
            0.6,
            0.0,
            64.0,
        )?,
        use_source_color: obj
            .get("use_source_color")
            .or_else(|| obj.get("respect_source_color"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        color: parse_effect_color(obj.get("color"))?.unwrap_or([0, 0, 0, 255]),
    })
}

fn parse_shake_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<ShakeEffectParams, String> {
    let steps_i32 = parse_effect_i32(obj.get("steps"), "shake.steps", 10, 0, 2048)?;
    let blur_px_i32 = parse_effect_i32(
        obj.get("blur")
            .or_else(|| obj.get("blur_radius"))
            .or_else(|| obj.get("blur_px")),
        "shake.blur",
        0,
        0,
        1024,
    )?;
    let grow_margin_px_i32 = parse_effect_i32(
        obj.get("grow_margin")
            .or_else(|| obj.get("grow_margin_px"))
            .or_else(|| obj.get("extra_margin")),
        "shake.grow_margin",
        0,
        0,
        8192,
    )?;

    Ok(ShakeEffectParams {
        angle_deg: parse_effect_f32_range(
            obj.get("angle_deg")
                .or_else(|| obj.get("angle"))
                .or_else(|| obj.get("direction_deg")),
            "shake.angle_deg",
            90.0,
            -3600.0,
            3600.0,
        )?,
        up_px: parse_effect_f32_range(
            obj.get("up").or_else(|| obj.get("up_px")),
            "shake.up",
            0.0,
            0.0,
            8192.0,
        )?,
        down_px: parse_effect_f32_range(
            obj.get("down").or_else(|| obj.get("down_px")),
            "shake.down",
            0.0,
            0.0,
            8192.0,
        )?,
        steps: u32::try_from(steps_i32).map_err(|_| "shake.steps должен быть >= 0".to_string())?,
        base_fade: parse_effect_f32_range(
            obj.get("base_fade").or_else(|| obj.get("fade")),
            "shake.base_fade",
            0.30,
            0.0,
            1.0,
        )?,
        decay: parse_effect_f32_range(obj.get("decay"), "shake.decay", 0.15, 0.0, 1.0)?,
        blur_px: u32::try_from(blur_px_i32)
            .map_err(|_| "shake.blur должен быть >= 0".to_string())?,
        autogrow: obj
            .get("autogrow")
            .or_else(|| obj.get("auto_grow"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        grow_margin_px: u32::try_from(grow_margin_px_i32)
            .map_err(|_| "shake.grow_margin должен быть >= 0".to_string())?,
    })
}

/// Parses the `interference` post-effect. Reads `kind` (defaulting to `white_noise`, with
/// aliases; an unknown kind is a hard error — no silent fallback) and every sub-kind field,
/// applying the documented defaults and ranges regardless of the selected kind.
fn parse_interference_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<InterferenceEffectParams, String> {
    let kind = match obj
        .get("kind")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
    {
        None => InterferenceKind::WhiteNoise,
        Some(kind) => match kind.as_str() {
            "white_noise" | "noise" => InterferenceKind::WhiteNoise,
            "digital" | "glitch" | "slices" => InterferenceKind::Digital,
            "rgb_split" | "chromatic" => InterferenceKind::RgbSplit,
            "scanlines" | "scan_lines" => InterferenceKind::Scanlines,
            other => {
                return Err(format!(
                    "interference.kind: неизвестное значение '{other}', ожидалось \
                     white_noise/digital/rgb_split/scanlines"
                ));
            }
        },
    };

    Ok(InterferenceEffectParams {
        kind,
        seed: parse_effect_u64(obj.get("seed"), "interference.seed", 1)?,
        amount: parse_effect_f32_range(obj.get("amount"), "interference.amount", 0.5, 0.0, 1.0)?,
        scale_px: parse_effect_f32_range(
            obj.get("scale_px"),
            "interference.scale_px",
            1.0,
            0.5,
            64.0,
        )?,
        density: parse_effect_f32_range(obj.get("density"), "interference.density", 1.0, 0.0, 1.0)?,
        monochrome: parse_effect_bool(obj.get("monochrome"), "interference.monochrome", true)?,
        alpha_noise: parse_effect_f32_range(
            obj.get("alpha_noise"),
            "interference.alpha_noise",
            0.0,
            0.0,
            1.0,
        )?,
        slice_height_px: parse_effect_i32(
            obj.get("slice_height_px"),
            "interference.slice_height_px",
            8,
            1,
            256,
        )?,
        height_jitter: parse_effect_f32_range(
            obj.get("height_jitter"),
            "interference.height_jitter",
            0.5,
            0.0,
            1.0,
        )?,
        max_shift_px: parse_effect_f32_range(
            obj.get("max_shift_px"),
            "interference.max_shift_px",
            16.0,
            0.0,
            512.0,
        )?,
        probability: parse_effect_f32_range(
            obj.get("probability"),
            "interference.probability",
            0.4,
            0.0,
            1.0,
        )?,
        rgb_split_px: parse_effect_f32_range(
            obj.get("rgb_split_px"),
            "interference.rgb_split_px",
            4.0,
            0.0,
            64.0,
        )?,
        autogrow: parse_effect_bool(obj.get("autogrow"), "interference.autogrow", true)?,
        offset_px: parse_effect_f32_range(
            obj.get("offset_px"),
            "interference.offset_px",
            3.0,
            0.0,
            64.0,
        )?,
        angle_deg: parse_effect_f32_range(
            obj.get("angle_deg"),
            "interference.angle_deg",
            0.0,
            -3600.0,
            3600.0,
        )?,
        per_row_jitter: parse_effect_f32_range(
            obj.get("per_row_jitter"),
            "interference.per_row_jitter",
            0.0,
            0.0,
            1.0,
        )?,
        line_height_px: parse_effect_i32(
            obj.get("line_height_px"),
            "interference.line_height_px",
            2,
            1,
            64,
        )?,
        gap_px: parse_effect_i32(obj.get("gap_px"), "interference.gap_px", 2, 1, 64)?,
        darken: parse_effect_f32_range(obj.get("darken"), "interference.darken", 0.35, 0.0, 1.0)?,
        jitter_px: parse_effect_f32_range(
            obj.get("jitter_px"),
            "interference.jitter_px",
            0.0,
            0.0,
            32.0,
        )?,
    })
}

fn parse_glow_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<GlowEffectParams, String> {
    Ok(GlowEffectParams {
        radius_px: parse_effect_f32_range(
            obj.get("radius")
                .or_else(|| obj.get("radius_px"))
                .or_else(|| obj.get("width")),
            "glow.radius",
            16.0,
            0.0,
            1024.0,
        )?,
        color: parse_effect_color(obj.get("color"))?.unwrap_or([0, 0, 0, 255]),
        opacity_mode: parse_stroke_opacity_mode(obj)?,
        transparency_percent: parse_stroke_transparency_percent(obj)?,
        fade_strength: parse_effect_f32_range(
            obj.get("fade_strength")
                .or_else(|| obj.get("decay_strength"))
                .or_else(|| obj.get("falloff_strength")),
            "glow.fade_strength",
            0.0,
            -100.0,
            100.0,
        )?,
        fade_shift: parse_effect_f32_range(
            obj.get("fade_shift")
                .or_else(|| obj.get("decay_shift"))
                .or_else(|| obj.get("falloff_shift")),
            "glow.fade_shift",
            0.0,
            -100.0,
            100.0,
        )?,
    })
}

fn parse_soft_glow_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<SoftGlowEffectParams, String> {
    let radius_steps_i32 = parse_effect_i32(
        obj.get("radius")
            .or_else(|| obj.get("radius_px"))
            .or_else(|| obj.get("glow_radius")),
        "soft_glow.radius",
        8,
        0,
        512,
    )?;

    Ok(SoftGlowEffectParams {
        radius_steps: u32::try_from(radius_steps_i32)
            .map_err(|_| "soft_glow.radius должен быть >= 0".to_string())?,
        softness_px: parse_effect_f32_range(
            obj.get("softness")
                .or_else(|| obj.get("softness_px"))
                .or_else(|| obj.get("glow_softness"))
                .or_else(|| obj.get("blur"))
                .or_else(|| obj.get("blur_px")),
            "soft_glow.softness",
            4.0,
            0.0,
            256.0,
        )?,
        color: parse_effect_color(obj.get("color"))?.unwrap_or([0, 0, 0, 255]),
    })
}

fn parse_gradient2_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient2EffectParams, String> {
    Ok(Gradient2EffectParams {
        color1: parse_effect_color(
            obj.get("color1")
                .or_else(|| obj.get("start_color"))
                .or_else(|| obj.get("from_color")),
        )?
        .unwrap_or([255, 255, 255, 255]),
        color2: parse_effect_color(
            obj.get("color2")
                .or_else(|| obj.get("end_color"))
                .or_else(|| obj.get("to_color")),
        )?
        .unwrap_or([0, 0, 0, 255]),
        angle_deg: parse_effect_f32_range(
            obj.get("angle_deg")
                .or_else(|| obj.get("angle"))
                .or_else(|| obj.get("rotation")),
            "gradient2.angle_deg",
            90.0,
            -3600.0,
            3600.0,
        )?,
        width_percent: parse_effect_f32_range(
            obj.get("width_percent")
                .or_else(|| obj.get("gradient_width_percent"))
                .or_else(|| obj.get("width")),
            "gradient2.width_percent",
            100.0,
            1.0,
            400.0,
        )?,
        respect_source_alpha: obj
            .get("respect_source_alpha")
            .or_else(|| obj.get("consider_alpha"))
            .or_else(|| obj.get("use_alpha"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        fill_mode: parse_gradient2_fill_mode(obj)?,
        target_color: parse_effect_color(
            obj.get("target_color")
                .or_else(|| obj.get("source_color"))
                .or_else(|| obj.get("mask_color")),
        )?
        .unwrap_or([255, 255, 255, 255]),
    })
}

fn parse_gradient2_fill_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient2FillMode, String> {
    let Some(mode) = obj
        .get("fill_mode")
        .or_else(|| obj.get("mode"))
        .or_else(|| obj.get("fill"))
        .and_then(Value::as_str)
    else {
        return Ok(Gradient2FillMode::AllOpaque);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "all_opaque" | "all_non_transparent" | "all" | "opaque" => Ok(Gradient2FillMode::AllOpaque),
        "specific_color" | "color" | "target_color" => Ok(Gradient2FillMode::SpecificColor),
        other => Err(format!(
            "gradient2.fill_mode: неизвестное значение '{other}', ожидалось all_opaque/specific_color"
        )),
    }
}

fn parse_gradient4_effect_params(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient4EffectParams, String> {
    Ok(Gradient4EffectParams {
        color_top_left: parse_effect_color(
            obj.get("color_top_left")
                .or_else(|| obj.get("color_tl"))
                .or_else(|| obj.get("top_left_color")),
        )?
        .unwrap_or([255, 255, 255, 255]),
        color_top_right: parse_effect_color(
            obj.get("color_top_right")
                .or_else(|| obj.get("color_tr"))
                .or_else(|| obj.get("top_right_color")),
        )?
        .unwrap_or([255, 255, 255, 255]),
        color_bottom_left: parse_effect_color(
            obj.get("color_bottom_left")
                .or_else(|| obj.get("color_bl"))
                .or_else(|| obj.get("bottom_left_color")),
        )?
        .unwrap_or([0, 0, 0, 255]),
        color_bottom_right: parse_effect_color(
            obj.get("color_bottom_right")
                .or_else(|| obj.get("color_br"))
                .or_else(|| obj.get("bottom_right_color")),
        )?
        .unwrap_or([0, 0, 0, 255]),
        width_percent: parse_effect_f32_range(
            obj.get("width_percent")
                .or_else(|| obj.get("gradient_width_percent"))
                .or_else(|| obj.get("width")),
            "gradient4.width_percent",
            100.0,
            1.0,
            400.0,
        )?,
        respect_source_alpha: obj
            .get("respect_source_alpha")
            .or_else(|| obj.get("consider_alpha"))
            .or_else(|| obj.get("use_alpha"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        fill_mode: parse_gradient4_fill_mode(obj)?,
        target_color: parse_effect_color(
            obj.get("target_color")
                .or_else(|| obj.get("source_color"))
                .or_else(|| obj.get("mask_color")),
        )?
        .unwrap_or([255, 255, 255, 255]),
    })
}

fn parse_gradient4_fill_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<Gradient4FillMode, String> {
    let Some(mode) = obj
        .get("fill_mode")
        .or_else(|| obj.get("mode"))
        .or_else(|| obj.get("fill"))
        .and_then(Value::as_str)
    else {
        return Ok(Gradient4FillMode::AllOpaque);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "all_opaque" | "all_non_transparent" | "all" | "opaque" => Ok(Gradient4FillMode::AllOpaque),
        "specific_color" | "color" | "target_color" => Ok(Gradient4FillMode::SpecificColor),
        other => Err(format!(
            "gradient4.fill_mode: неизвестное значение '{other}', ожидалось all_opaque/specific_color"
        )),
    }
}

fn parse_shadow_use_source_color(obj: &serde_json::Map<String, Value>) -> bool {
    if let Some(value) = obj.get("use_source_color").and_then(Value::as_bool) {
        return value;
    }

    let Some(mode) = obj.get("mode").and_then(Value::as_str) else {
        return false;
    };

    matches!(
        mode.trim().to_ascii_lowercase().as_str(),
        "source" | "source_colors" | "original" | "original_colors"
    )
}

fn parse_shadow_transparency_percent(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    if let Some(value) = obj.get("transparency") {
        let Some(raw) = value.as_f64() else {
            return Err("shadow.transparency должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("shadow.transparency должен быть в диапазоне 0..100".to_string());
        }
        return Ok(raw as f32);
    }

    if let Some(value) = obj.get("opacity") {
        let Some(raw) = value.as_f64() else {
            return Err("shadow.opacity должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("shadow.opacity должен быть в диапазоне 0..100".to_string());
        }
        return Ok((100.0 - raw as f32).clamp(0.0, 100.0));
    }

    Ok(40.0)
}

fn parse_shadow_blur_radius_px(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    parse_effect_f32_range(
        obj.get("blur")
            .or_else(|| obj.get("blur_radius"))
            .or_else(|| obj.get("blur_px")),
        "shadow.blur",
        0.0,
        0.0,
        256.0,
    )
}

fn parse_stroke_opacity_mode(
    obj: &serde_json::Map<String, Value>,
) -> Result<StrokeOpacityMode, String> {
    let Some(mode) = obj
        .get("opacity_mode")
        .or_else(|| obj.get("alpha_mode"))
        .and_then(Value::as_str)
    else {
        return Ok(StrokeOpacityMode::FromContour);
    };

    match mode.trim().to_ascii_lowercase().as_str() {
        "from_contour" | "contour" | "auto" => Ok(StrokeOpacityMode::FromContour),
        "static" | "fixed" => Ok(StrokeOpacityMode::Static),
        other => Err(format!(
            "stroke.opacity_mode: неизвестное значение '{other}', ожидалось static/from_contour"
        )),
    }
}

fn parse_stroke_transparency_percent(obj: &serde_json::Map<String, Value>) -> Result<f32, String> {
    if let Some(value) = obj.get("transparency") {
        let Some(raw) = value.as_f64() else {
            return Err("stroke.transparency должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("stroke.transparency должен быть в диапазоне 0..100".to_string());
        }
        return Ok(raw as f32);
    }

    if let Some(value) = obj.get("opacity") {
        let Some(raw) = value.as_f64() else {
            return Err("stroke.opacity должен быть числом".to_string());
        };
        if !(0.0..=100.0).contains(&raw) {
            return Err("stroke.opacity должен быть в диапазоне 0..100".to_string());
        }
        return Ok((100.0 - raw as f32).clamp(0.0, 100.0));
    }

    Ok(0.0)
}

fn parse_stroke_smoothing_enabled(obj: &serde_json::Map<String, Value>) -> Result<bool, String> {
    let Some(value) = obj
        .get("smoothing")
        .or_else(|| obj.get("smooth"))
        .or_else(|| obj.get("antialias"))
    else {
        return Ok(true);
    };

    value
        .as_bool()
        .ok_or_else(|| "stroke.smoothing должен быть булевым значением".to_string())
}

fn parse_stroke_smoothing_strength_percent(
    obj: &serde_json::Map<String, Value>,
) -> Result<f32, String> {
    parse_effect_f32_range(
        obj.get("smoothing_strength")
            .or_else(|| obj.get("smoothing_strength_percent"))
            .or_else(|| obj.get("smooth_strength"))
            .or_else(|| obj.get("antialias_strength")),
        "stroke.smoothing_strength",
        100.0,
        0.0,
        100.0,
    )
}

fn parse_effect_i32(
    value: Option<&Value>,
    label: &str,
    default: i32,
    min: i32,
    max: i32,
) -> Result<i32, String> {
    let Some(value) = value else {
        return Ok(default.clamp(min, max));
    };
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if raw < min as f64 || raw > max as f64 {
        return Err(format!("{label} должен быть в диапазоне {min}..{max}"));
    }
    Ok((raw.round() as i32).clamp(min, max))
}

fn parse_effect_f32_range(
    value: Option<&Value>,
    label: &str,
    default: f32,
    min: f32,
    max: f32,
) -> Result<f32, String> {
    let Some(value) = value else {
        return Ok(default.clamp(min, max));
    };
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if raw < min as f64 || raw > max as f64 {
        return Err(format!("{label} должен быть в диапазоне {min}..{max}"));
    }
    Ok((raw as f32).clamp(min, max))
}

/// Parses an optional `u64` effect field (seeds), preserving all 64 bits.
///
/// Missing value -> `default`. JSON integers within `0..=u64::MAX` take the exact
/// `as_u64` path (no `f64` round-trip, which would corrupt seeds above 2^53). Other
/// JSON numbers (floats like `5.0`, negatives, integers above `u64::MAX` which
/// serde_json stores as `f64`) are accepted only when finite, integral, and strictly
/// below 2^64; anything else is a typed error (no rounding, no saturation).
fn parse_effect_u64(value: Option<&Value>, label: &str, default: u64) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    // Exact path: serde_json keeps JSON integers up to u64::MAX losslessly.
    if let Some(raw) = value.as_u64() {
        return Ok(raw);
    }
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    // 2^64 exactly; `u64::MAX as f64` rounds UP to this same value, so the bound must be
    // exclusive or u64::MAX + 1 would saturate instead of erroring.
    const U64_RANGE_END: f64 = 18_446_744_073_709_551_616.0;
    // The range check also rejects NaN and both infinities (contains is false for them).
    if !(0.0..U64_RANGE_END).contains(&raw) || raw.fract() != 0.0 {
        return Err(format!(
            "{label} должен быть целым числом в диапазоне 0..=u64::MAX"
        ));
    }
    // Safe cast: raw is a non-negative integral f64 strictly below 2^64, so every such
    // value is exactly representable in u64 (f64 integrals below 2^64 have <= 64 bits).
    Ok(raw as u64)
}

/// Parses an optional boolean effect field: missing -> `default`, a JSON bool -> its
/// value, any other type -> a typed error naming `label` (a malformed value like the
/// string `"false"` must not silently become the default).
fn parse_effect_bool(value: Option<&Value>, label: &str, default: bool) -> Result<bool, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .as_bool()
        .ok_or_else(|| format!("{label} должен быть булевым значением"))
}

fn parse_effect_color(value: Option<&Value>) -> Result<Option<[u8; 4]>, String> {
    let Some(color) = value else {
        return Ok(None);
    };

    if let Some(arr) = color.as_array() {
        if arr.len() == 3 || arr.len() == 4 {
            let r = value_to_u8(&arr[0], "color[0]")?;
            let g = value_to_u8(&arr[1], "color[1]")?;
            let b = value_to_u8(&arr[2], "color[2]")?;
            let a = if arr.len() == 4 {
                value_to_u8(&arr[3], "color[3]")?
            } else {
                255
            };
            return Ok(Some([r, g, b, a]));
        }
        return Err("color-массив должен быть [r,g,b] или [r,g,b,a]".to_string());
    }

    if let Some(obj) = color.as_object() {
        let r = obj
            .get("r")
            .ok_or_else(|| "color.r отсутствует".to_string())
            .and_then(|value| value_to_u8(value, "color.r"))?;
        let g = obj
            .get("g")
            .ok_or_else(|| "color.g отсутствует".to_string())
            .and_then(|value| value_to_u8(value, "color.g"))?;
        let b = obj
            .get("b")
            .ok_or_else(|| "color.b отсутствует".to_string())
            .and_then(|value| value_to_u8(value, "color.b"))?;
        let a = obj
            .get("a")
            .map(|value| value_to_u8(value, "color.a"))
            .transpose()?
            .unwrap_or(255);
        return Ok(Some([r, g, b, a]));
    }

    Err("color должен быть объектом или массивом".to_string())
}

fn value_to_u8(value: &Value, label: &str) -> Result<u8, String> {
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if !(0.0..=255.0).contains(&raw) {
        return Err(format!("{label} должен быть в диапазоне 0..255"));
    }
    Ok(raw.round() as u8)
}

#[cfg(test)]
mod tests {
    use super::{EffectSpec, InterferenceKind, parse_effects_json};

    /// Extracts the single `Interference` params from a one-effect JSON array.
    ///
    /// Test-only helper: the fixture invariant is "this JSON parses to exactly one
    /// Interference effect", so any deviation panics with a diagnostic — the panic IS
    /// the test failure, not swallowed error handling.
    fn parse_single_interference(json: &str) -> super::InterferenceEffectParams {
        let effects = match parse_effects_json(json) {
            Ok(effects) => effects,
            Err(error) => panic!("fixture json must parse: {error}"),
        };
        assert_eq!(effects.len(), 1, "fixture must contain exactly one effect");
        match effects.into_iter().next() {
            Some(EffectSpec::Interference(params)) => params,
            other => panic!("fixture must parse to Interference, got {other:?}"),
        }
    }

    #[test]
    fn interference_defaults_when_only_name_present() {
        let params = parse_single_interference(r#"[{"effect":"interference"}]"#);
        assert_eq!(params.kind, InterferenceKind::WhiteNoise);
        assert_eq!(params.seed, 1);
        assert!((params.amount - 0.5).abs() < 1e-6);
        assert!((params.scale_px - 1.0).abs() < 1e-6);
        assert!((params.density - 1.0).abs() < 1e-6);
        assert!(params.monochrome);
        assert!((params.alpha_noise - 0.0).abs() < 1e-6);
        assert_eq!(params.slice_height_px, 8);
        assert!((params.max_shift_px - 16.0).abs() < 1e-6);
        assert!((params.probability - 0.4).abs() < 1e-6);
        assert!(params.autogrow);
        assert!((params.offset_px - 3.0).abs() < 1e-6);
        assert!((params.angle_deg - 0.0).abs() < 1e-6);
        assert_eq!(params.line_height_px, 2);
        assert_eq!(params.gap_px, 2);
        assert!((params.darken - 0.35).abs() < 1e-6);
        assert!((params.jitter_px - 0.0).abs() < 1e-6);
    }

    #[test]
    fn interference_kind_aliases_resolve() {
        for (alias, expected) in [
            ("white_noise", InterferenceKind::WhiteNoise),
            ("noise", InterferenceKind::WhiteNoise),
            ("digital", InterferenceKind::Digital),
            ("glitch", InterferenceKind::Digital),
            ("slices", InterferenceKind::Digital),
            ("rgb_split", InterferenceKind::RgbSplit),
            ("chromatic", InterferenceKind::RgbSplit),
            ("scanlines", InterferenceKind::Scanlines),
            ("scan_lines", InterferenceKind::Scanlines),
        ] {
            let json = format!(r#"[{{"effect":"interference","kind":"{alias}"}}]"#);
            let params = parse_single_interference(&json);
            assert_eq!(params.kind, expected, "alias '{alias}' mismatch");
        }
    }

    #[test]
    fn interference_kind_is_case_insensitive_and_trimmed() {
        let params = parse_single_interference(r#"[{"effect":"interference","kind":"  Digital "}]"#);
        assert_eq!(params.kind, InterferenceKind::Digital);
    }

    #[test]
    fn interference_unknown_kind_is_error() {
        let err = parse_effects_json(r#"[{"effect":"interference","kind":"plasma"}]"#)
            .expect_err("unknown kind must error");
        assert!(err.contains("interference.kind"), "unexpected error: {err}");
    }

    #[test]
    fn interference_out_of_range_field_is_error() {
        // amount range is 0..1; 5.0 is out of range and must be rejected (no silent clamp).
        let err = parse_effects_json(r#"[{"effect":"interference","amount":5.0}]"#)
            .expect_err("out-of-range amount must error");
        assert!(err.contains("interference.amount"), "unexpected error: {err}");
    }

    #[test]
    fn interference_malformed_bool_is_error() {
        // A string "false" must NOT silently fall back to the default (true).
        let err = parse_effects_json(r#"[{"effect":"interference","monochrome":"false"}]"#)
            .expect_err("string monochrome must error");
        assert!(
            err.contains("interference.monochrome"),
            "unexpected error: {err}"
        );

        let err = parse_effects_json(r#"[{"effect":"interference","autogrow":1}]"#)
            .expect_err("numeric autogrow must error");
        assert!(
            err.contains("interference.autogrow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effect_seed_preserves_full_u64_precision() {
        // 2^53 + 1: the first integer an f64 round-trip corrupts.
        let params =
            parse_single_interference(r#"[{"effect":"interference","seed":9007199254740993}]"#);
        assert_eq!(params.seed, 9_007_199_254_740_993);

        // u64::MAX must round-trip exactly.
        let params = parse_single_interference(
            r#"[{"effect":"interference","seed":18446744073709551615}]"#,
        );
        assert_eq!(params.seed, u64::MAX);
    }

    #[test]
    fn effect_seed_rejects_overflow_negative_and_fractional() {
        // u64::MAX + 1 must be a hard error, not a saturation to u64::MAX.
        let err =
            parse_effects_json(r#"[{"effect":"interference","seed":18446744073709551616}]"#)
                .expect_err("u64::MAX + 1 must error");
        assert!(err.contains("interference.seed"), "unexpected error: {err}");

        let err = parse_effects_json(r#"[{"effect":"interference","seed":-1}]"#)
            .expect_err("negative seed must error");
        assert!(err.contains("interference.seed"), "unexpected error: {err}");

        let err = parse_effects_json(r#"[{"effect":"interference","seed":1.5}]"#)
            .expect_err("fractional seed must error");
        assert!(err.contains("interference.seed"), "unexpected error: {err}");
    }

    #[test]
    fn effect_seed_accepts_integral_float() {
        // `5.0` is stored by serde_json as f64; integral floats stay accepted.
        let params = parse_single_interference(r#"[{"effect":"interference","seed":5.0}]"#);
        assert_eq!(params.seed, 5);
    }

    #[test]
    fn interference_parses_all_fields() {
        let json = r#"[{
            "effect":"interference","kind":"digital","seed":42,
            "amount":0.25,"scale_px":4.0,"density":0.7,"monochrome":false,"alpha_noise":0.5,
            "slice_height_px":12,"height_jitter":0.8,"max_shift_px":20.0,"probability":0.9,
            "rgb_split_px":6.0,"autogrow":false,
            "offset_px":5.0,"angle_deg":45.0,"per_row_jitter":0.5,
            "line_height_px":3,"gap_px":4,"darken":0.6,"jitter_px":8.0
        }]"#;
        let params = parse_single_interference(json);
        assert_eq!(params.kind, InterferenceKind::Digital);
        assert_eq!(params.seed, 42);
        assert!(!params.monochrome);
        assert_eq!(params.slice_height_px, 12);
        assert!(!params.autogrow);
        assert!((params.angle_deg - 45.0).abs() < 1e-6);
        assert_eq!(params.gap_px, 4);
        assert!((params.jitter_px - 8.0).abs() < 1e-6);
    }
}
