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

fn parse_effect_u64(value: Option<&Value>, label: &str, default: u64) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    let Some(raw) = value.as_f64() else {
        return Err(format!("{label} должен быть числом"));
    };
    if !raw.is_finite() || raw < 0.0 || raw > u64::MAX as f64 {
        return Err(format!("{label} должен быть в диапазоне 0..u64::MAX"));
    }
    Ok(raw.round() as u64)
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
