/*
File: panel/effect_parse.rs

Purpose:
Free-function helper `parse_effect_cards`, extracted verbatim from `panel.rs`.
Builds the panel's `EffectCard` UI models from the stored effects JSON array
(each entry's kind + numeric/color fields) used by the Effects tab.

Notes:
Extracted verbatim from `panel.rs`. `use super::*;` pulls in the parent
`panel` module's types and imports; the fn is `pub(super)` so `panel.rs` and
sibling `panel` submodules can use it.
*/

use super::*;

pub(super) fn parse_effect_cards(effects: &[Value], text_color: Color32) -> Vec<EffectCard> {
    let mut out = Vec::<EffectCard>::new();
    for effect in effects {
        let Some(obj) = effect.as_object() else {
            continue;
        };
        let kind = obj
            .get("effect")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match kind.as_str() {
            "text_shake" | "text_jitter" | "character_shake" => {
                out.push(EffectCard::TextShake(TextShakeEffectCard {
                    spread_x_px: obj
                        .get("spread_x")
                        .or_else(|| obj.get("spread_x_px"))
                        .or_else(|| obj.get("x"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 256.0),
                    spread_y_px: obj
                        .get("spread_y")
                        .or_else(|| obj.get("spread_y_px"))
                        .or_else(|| obj.get("y"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 256.0),
                    seed: obj
                        .get("seed")
                        .and_then(value_as_u64)
                        .unwrap_or_else(random_seed),
                }));
            }
            "stroke" => out.push(EffectCard::Stroke(StrokeEffectCard {
                width_px: obj
                    .get("width")
                    .and_then(value_as_f32)
                    .unwrap_or(2.0)
                    .clamp(0.0, 24.0),
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: match obj
                    .get("opacity_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "from_contour" => StrokeOpacityMode::FromContour,
                    _ => StrokeOpacityMode::Static,
                },
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(0.0)
                    .clamp(0.0, 100.0),
                smoothing: obj
                    .get("smoothing")
                    .or_else(|| obj.get("smooth"))
                    .or_else(|| obj.get("antialias"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                smoothing_strength_percent: obj
                    .get("smoothing_strength")
                    .or_else(|| obj.get("smoothing_strength_percent"))
                    .or_else(|| obj.get("smooth_strength"))
                    .or_else(|| obj.get("antialias_strength"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(0.0, 100.0),
            })),
            "shadow" => out.push(EffectCard::Shadow(ShadowEffectCard {
                offset_x_px: obj
                    .get("offset_x")
                    .and_then(value_as_f32)
                    .map(|v| v.round() as i32)
                    .unwrap_or(4)
                    .clamp(-400, 400),
                offset_y_px: obj
                    .get("offset_y")
                    .and_then(value_as_f32)
                    .map(|v| v.round() as i32)
                    .unwrap_or(4)
                    .clamp(-400, 400),
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(40.0)
                    .clamp(0.0, 100.0),
                blur_radius_px: obj
                    .get("blur")
                    .or_else(|| obj.get("blur_radius"))
                    .or_else(|| obj.get("blur_px"))
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(0.0, 128.0),
                color_mode: if obj
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("source")
                    || obj
                        .get("use_source_color")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    ShadowColorMode::SourceColors
                } else {
                    ShadowColorMode::SingleColor
                },
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
            })),
            "blur" | "gaussian_blur" => out.push(EffectCard::Blur(BlurEffectCard {
                radius_px: obj
                    .get("radius")
                    .or_else(|| obj.get("radius_px"))
                    .or_else(|| obj.get("blur"))
                    .or_else(|| obj.get("blur_px"))
                    .or_else(|| obj.get("sigma"))
                    .and_then(value_as_f32)
                    .unwrap_or(4.0)
                    .clamp(0.0, 128.0),
            })),
            "motion_blur" | "directional_blur" => {
                out.push(EffectCard::MotionBlur(MotionBlurEffectCard {
                    angle_deg: obj
                        .get("angle_deg")
                        .or_else(|| obj.get("angle"))
                        .and_then(value_as_f32)
                        .unwrap_or(20.0)
                        .clamp(-360.0, 360.0),
                    distance_px: obj
                        .get("distance")
                        .or_else(|| obj.get("distance_px"))
                        .or_else(|| obj.get("offset"))
                        .or_else(|| obj.get("offset_px"))
                        .and_then(value_as_f32)
                        .unwrap_or(11.0)
                        .clamp(0.0, 512.0),
                    sharp_copy_mode: match obj
                        .get("sharp_copy")
                        .or_else(|| obj.get("unblurred_copy"))
                        .or_else(|| obj.get("sharp_copy_mode"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "over" | "top" | "above" => MotionBlurSharpCopyMode::Over,
                        "under" | "bottom" | "below" => MotionBlurSharpCopyMode::Under,
                        _ => MotionBlurSharpCopyMode::None,
                    },
                }))
            }
            "dry_media" | "chalk_pencil" | "dry_brush" => {
                out.push(EffectCard::DryMedia(DryMediaEffectCard {
                    material: match obj
                        .get("material")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "chalk" | "mel" => DryMediaMaterial::Chalk,
                        _ => DryMediaMaterial::Pencil,
                    },
                    strength: obj
                        .get("strength")
                        .and_then(value_as_f32)
                        .unwrap_or(0.65)
                        .clamp(0.0, 1.0),
                    seed: obj.get("seed").and_then(value_as_u64).unwrap_or(1),
                    grain_scale_px: obj
                        .get("grain_scale_px")
                        .or_else(|| obj.get("grain_scale"))
                        .or_else(|| obj.get("grain_size_px"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.5, 32.0),
                    grain_amount: obj
                        .get("grain_amount")
                        .or_else(|| obj.get("grain"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.35)
                        .clamp(0.0, 1.0),
                    edge_roughness: obj
                        .get("edge_roughness")
                        .or_else(|| obj.get("roughness"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.45)
                        .clamp(0.0, 1.0),
                    porosity: obj
                        .get("porosity")
                        .or_else(|| obj.get("holes"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.20)
                        .clamp(0.0, 1.0),
                    direction_deg: obj
                        .get("direction_deg")
                        .or_else(|| obj.get("angle_deg"))
                        .or_else(|| obj.get("angle"))
                        .and_then(value_as_f32)
                        .unwrap_or(82.0)
                        .clamp(-360.0, 360.0),
                    directional_amount: obj
                        .get("directional_amount")
                        .or_else(|| obj.get("stroke_amount"))
                        .or_else(|| obj.get("hatching"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.30)
                        .clamp(0.0, 1.0),
                    dust_amount: obj
                        .get("dust_amount")
                        .or_else(|| obj.get("dust"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.08)
                        .clamp(0.0, 1.0),
                    dust_radius_px: obj
                        .get("dust_radius_px")
                        .or_else(|| obj.get("dust_radius"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 32.0),
                    softness_px: obj
                        .get("softness_px")
                        .or_else(|| obj.get("softness"))
                        .or_else(|| obj.get("blur"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.6)
                        .clamp(0.0, 16.0),
                    use_source_color: obj
                        .get("use_source_color")
                        .or_else(|| obj.get("respect_source_color"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    color: ColorField::new(
                        obj.get("color")
                            .and_then(parse_color32_value)
                            .unwrap_or(text_color),
                    ),
                }))
            }
            "interference" => out.push(EffectCard::Interference(InterferenceEffectCard {
                kind: match obj
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    // Mirror the renderer's kind aliases (ms-text-render effects/parse.rs)
                    // so externally authored JSON keeps its kind when edited in the UI.
                    "digital" | "glitch" | "slices" => InterferenceKind::Digital,
                    "rgb_split" | "chromatic" => InterferenceKind::RgbSplit,
                    "scanlines" | "scan_lines" => InterferenceKind::Scanlines,
                    _ => InterferenceKind::WhiteNoise,
                },
                seed: obj
                    .get("seed")
                    .and_then(value_as_u64)
                    .unwrap_or_else(random_seed),
                amount: obj.get("amount").and_then(value_as_f32).unwrap_or(0.5).clamp(0.0, 1.0),
                scale_px: obj.get("scale_px").and_then(value_as_f32).unwrap_or(1.0).clamp(0.5, 64.0),
                density: obj.get("density").and_then(value_as_f32).unwrap_or(1.0).clamp(0.0, 1.0),
                monochrome: obj.get("monochrome").and_then(Value::as_bool).unwrap_or(true),
                alpha_noise: obj.get("alpha_noise").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 1.0),
                slice_height_px: obj
                    .get("slice_height_px")
                    .and_then(value_as_u64)
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(8)
                    .clamp(1, 256),
                height_jitter: obj.get("height_jitter").and_then(value_as_f32).unwrap_or(0.5).clamp(0.0, 1.0),
                max_shift_px: obj.get("max_shift_px").and_then(value_as_f32).unwrap_or(16.0).clamp(0.0, 512.0),
                probability: obj.get("probability").and_then(value_as_f32).unwrap_or(0.4).clamp(0.0, 1.0),
                rgb_split_px: obj.get("rgb_split_px").and_then(value_as_f32).unwrap_or(4.0).clamp(0.0, 64.0),
                autogrow: obj.get("autogrow").and_then(Value::as_bool).unwrap_or(true),
                offset_px: obj.get("offset_px").and_then(value_as_f32).unwrap_or(3.0).clamp(0.0, 64.0),
                angle_deg: obj.get("angle_deg").and_then(value_as_f32).unwrap_or(0.0).clamp(-3_600.0, 3_600.0),
                per_row_jitter: obj.get("per_row_jitter").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 1.0),
                line_height_px: obj
                    .get("line_height_px")
                    .and_then(value_as_u64)
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(2)
                    .clamp(1, 64),
                gap_px: obj
                    .get("gap_px")
                    .and_then(value_as_u64)
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(2)
                    .clamp(1, 64),
                darken: obj.get("darken").and_then(value_as_f32).unwrap_or(0.35).clamp(0.0, 1.0),
                jitter_px: obj.get("jitter_px").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 32.0),
            })),
            "glow_v1" | "glow_v2" => out.push(EffectCard::Glow(GlowEffectCard {
                version: if kind == "glow_v2" {
                    GlowEffectVersion::V2
                } else {
                    GlowEffectVersion::V1
                },
                radius_px: obj
                    .get("radius")
                    .and_then(value_as_f32)
                    .unwrap_or(16.0)
                    .clamp(0.0, 300.0),
                softness_px: 0.0,
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: match obj
                    .get("opacity_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "from_contour" => StrokeOpacityMode::FromContour,
                    _ => StrokeOpacityMode::Static,
                },
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(0.0)
                    .clamp(0.0, 100.0),
                fade_strength: obj
                    .get("fade_strength")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(-100.0, 100.0),
                fade_shift: obj
                    .get("fade_shift")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(-100.0, 100.0),
            })),
            "soft_glow" | "glow_soft" => out.push(EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::Soft,
                radius_px: obj
                    .get("radius")
                    .or_else(|| obj.get("glow_radius"))
                    .and_then(value_as_f32)
                    .unwrap_or(8.0)
                    .clamp(0.0, 300.0),
                softness_px: obj
                    .get("softness")
                    .or_else(|| obj.get("softness_px"))
                    .or_else(|| obj.get("glow_softness"))
                    .or_else(|| obj.get("blur"))
                    .and_then(value_as_f32)
                    .unwrap_or(4.0)
                    .clamp(0.0, 100.0),
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            })),
            "gradient2" => out.push(EffectCard::Gradient2(Gradient2EffectCard {
                color1: ColorField::new(
                    obj.get("color1")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color2: ColorField::new(
                    obj.get("color2")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                angle_deg: obj
                    .get("angle_deg")
                    .and_then(value_as_f32)
                    .unwrap_or(90.0)
                    .clamp(-360.0, 360.0),
                width_percent: obj
                    .get("width_percent")
                    .or_else(|| obj.get("gradient_width_percent"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(1.0, 400.0),
                respect_source_alpha: obj
                    .get("respect_source_alpha")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                fill_mode: match obj
                    .get("fill_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "specific_color" => Gradient2FillMode::SpecificColor,
                    _ => Gradient2FillMode::AllOpaque,
                },
                target_color: ColorField::new(
                    obj.get("target_color")
                        .and_then(parse_color32_value)
                        .unwrap_or(text_color),
                ),
            })),
            "gradient4" => out.push(EffectCard::Gradient4(Gradient4EffectCard {
                color_top_left: ColorField::new(
                    obj.get("color_top_left")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color_top_right: ColorField::new(
                    obj.get("color_top_right")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color_bottom_left: ColorField::new(
                    obj.get("color_bottom_left")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                color_bottom_right: ColorField::new(
                    obj.get("color_bottom_right")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                width_percent: obj
                    .get("width_percent")
                    .or_else(|| obj.get("gradient_width_percent"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(1.0, 400.0),
                respect_source_alpha: obj
                    .get("respect_source_alpha")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                fill_mode: match obj
                    .get("fill_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "specific_color" => Gradient4FillMode::SpecificColor,
                    _ => Gradient4FillMode::AllOpaque,
                },
                target_color: ColorField::new(
                    obj.get("target_color")
                        .and_then(parse_color32_value)
                        .unwrap_or(text_color),
                ),
            })),
            "reflect" => out.push(EffectCard::Reflect(ReflectEffectCard {
                axis: match obj
                    .get("axis")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "x" => ReflectAxis::X,
                    _ => ReflectAxis::Y,
                },
            })),
            "shake" => out.push(EffectCard::Shake(ShakeEffectCard {
                angle_deg: obj
                    .get("angle_deg")
                    .and_then(value_as_f32)
                    .unwrap_or(90.0)
                    .clamp(-360.0, 360.0),
                up_px: obj
                    .get("up")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(0.0, 1000.0),
                down_px: obj
                    .get("down")
                    .and_then(value_as_f32)
                    .unwrap_or(40.0)
                    .clamp(0.0, 1000.0),
                steps: obj
                    .get("steps")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(12)
                    .min(128),
                base_fade: obj
                    .get("base_fade")
                    .and_then(value_as_f32)
                    .unwrap_or(0.30)
                    .clamp(0.0, 1.0),
                decay: obj
                    .get("decay")
                    .and_then(value_as_f32)
                    .unwrap_or(0.15)
                    .clamp(0.0, 1.0),
                blur_px: obj
                    .get("blur")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(2)
                    .min(64),
                autogrow: obj.get("autogrow").and_then(Value::as_bool).unwrap_or(true),
                grow_margin_px: obj
                    .get("grow_margin")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0)
                    .min(1024),
            })),
            _ => {}
        }
    }
    out
}
