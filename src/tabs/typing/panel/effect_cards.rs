/*
File: panel/effect_cards.rs

Purpose:
Free-function helpers extracted verbatim from `panel.rs`: effect-card title
lookup, the per-effect card control drawing UI, and the background
preview-render worker spawner.

Main responsibilities:
- map an `EffectCard` variant to its localized title;
- draw the editable controls for a single effect card and report changes;
- spawn the background thread that renders text preview images off the GUI thread.

Key functions:
- effect_card_title()
- draw_effect_card_controls()
- spawn_preview_render_worker()

Notes:
Extracted verbatim from `panel.rs`. Free fns are `pub(super)` so `panel.rs`
can use them. `use super::*;` pulls in the parent module's types and imports.
*/

use super::*;

pub(super) fn effect_card_title(effect: &EffectCard) -> &'static str {
    match effect {
        EffectCard::TextShake(_) => t!("typing.effects.text_shake_title"),
        EffectCard::Stroke(_) => t!("typing.effects.stroke_title"),
        EffectCard::Shadow(_) => t!("typing.effects.shadow_title"),
        EffectCard::Blur(_) => t!("typing.effects.blur_title"),
        EffectCard::MotionBlur(_) => t!("typing.effects.motion_blur_title"),
        EffectCard::DryMedia(_) => t!("typing.effects.dry_media_title"),
        EffectCard::Glow(glow) => match glow.version {
            GlowEffectVersion::V1 => t!("typing.effects.glow_v1_title"),
            GlowEffectVersion::V2 => t!("typing.effects.glow_v2_title"),
            GlowEffectVersion::Soft => t!("typing.effects.soft_glow_title"),
        },
        EffectCard::Gradient2(_) => t!("typing.effects.gradient2_title"),
        EffectCard::Gradient4(_) => t!("typing.effects.gradient4_title"),
        EffectCard::Reflect(_) => t!("typing.effects.reflection_title"),
        EffectCard::Shake(_) => t!("typing.effects.shake_title"),
    }
}

/// Serializes a single `EffectCard` to its stored JSON object (the exact shape one
/// element of `effects_value_array` produces). This is the single source of truth for
/// per-card serialization: `effects_value_array` maps it over its cards, and the effect
/// defaults store persists one card with it. Round-trips with `parse_effect_cards`.
pub(super) fn effect_card_to_value(effect: &EffectCard) -> Value {
    match effect {
        EffectCard::TextShake(shake) => json!({
            "effect": "text_shake",
            "effect_type": "preprocess",
            "enabled": true,
            "spread_x": shake.spread_x_px,
            "spread_y": shake.spread_y_px,
            "seed": shake.seed,
        }),
        EffectCard::Stroke(stroke) => json!({
            "effect": "stroke",
            "enabled": true,
            "width": stroke.width_px,
            "color": stroke.color.rgba(),
            "opacity_mode": if stroke.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
            "transparency": stroke.transparency_percent,
            "opacity": 100.0 - stroke.transparency_percent,
            "smoothing": stroke.smoothing,
            "smoothing_strength": stroke.smoothing_strength_percent,
        }),
        EffectCard::Shadow(shadow) => json!({
            "effect": "shadow",
            "enabled": true,
            "offset_x": shadow.offset_x_px,
            "offset_y": shadow.offset_y_px,
            "transparency": shadow.transparency_percent,
            "opacity": 100.0 - shadow.transparency_percent,
            "blur": shadow.blur_radius_px,
            "blur_radius": shadow.blur_radius_px,
            "blur_px": shadow.blur_radius_px,
            "mode": if shadow.color_mode == ShadowColorMode::SourceColors { "source" } else { "single" },
            "use_source_color": shadow.color_mode == ShadowColorMode::SourceColors,
            "color": shadow.color.rgba(),
        }),
        EffectCard::Blur(blur) => json!({
            "effect": "blur",
            "enabled": true,
            "radius": blur.radius_px,
            "blur": blur.radius_px,
        }),
        EffectCard::MotionBlur(blur) => json!({
            "effect": "motion_blur",
            "enabled": true,
            "angle_deg": blur.angle_deg,
            "distance": blur.distance_px,
            "distance_px": blur.distance_px,
            "sharp_copy": match blur.sharp_copy_mode {
                MotionBlurSharpCopyMode::None => "none",
                MotionBlurSharpCopyMode::Over => "over",
                MotionBlurSharpCopyMode::Under => "under",
            },
        }),
        EffectCard::DryMedia(dry_media) => json!({
            "effect": "dry_media",
            "enabled": true,
            "material": match dry_media.material {
                DryMediaMaterial::Pencil => "pencil",
                DryMediaMaterial::Chalk => "chalk",
            },
            "strength": dry_media.strength,
            "seed": dry_media.seed,
            "grain_scale_px": dry_media.grain_scale_px,
            "grain_amount": dry_media.grain_amount,
            "edge_roughness": dry_media.edge_roughness,
            "porosity": dry_media.porosity,
            "direction_deg": dry_media.direction_deg,
            "directional_amount": dry_media.directional_amount,
            "dust_amount": dry_media.dust_amount,
            "dust_radius_px": dry_media.dust_radius_px,
            "softness_px": dry_media.softness_px,
            "use_source_color": dry_media.use_source_color,
            "color": dry_media.color.rgba(),
        }),
        EffectCard::Glow(glow) => match glow.version {
            GlowEffectVersion::V1 | GlowEffectVersion::V2 => json!({
                "effect": if glow.version == GlowEffectVersion::V1 { "glow_v1" } else { "glow_v2" },
                "enabled": true,
                "radius": glow.radius_px,
                "color": glow.color.rgba(),
                "opacity_mode": if glow.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                "transparency": glow.transparency_percent,
                "opacity": 100.0 - glow.transparency_percent,
                "fade_strength": glow.fade_strength,
                "fade_shift": glow.fade_shift,
            }),
            GlowEffectVersion::Soft => json!({
                "effect": "soft_glow",
                "enabled": true,
                "radius": glow.radius_px.round().max(0.0),
                "softness": glow.softness_px,
                "color": glow.color.rgba(),
            }),
        },
        EffectCard::Gradient2(gradient) => json!({
            "effect": "gradient2",
            "enabled": true,
            "color1": gradient.color1.rgba(),
            "color2": gradient.color2.rgba(),
            "angle_deg": gradient.angle_deg,
            "width_percent": gradient.width_percent,
            "respect_source_alpha": gradient.respect_source_alpha,
            "fill_mode": if gradient.fill_mode == Gradient2FillMode::AllOpaque { "all_opaque" } else { "specific_color" },
            "target_color": gradient.target_color.rgba(),
        }),
        EffectCard::Gradient4(gradient) => json!({
            "effect": "gradient4",
            "enabled": true,
            "color_top_left": gradient.color_top_left.rgba(),
            "color_top_right": gradient.color_top_right.rgba(),
            "color_bottom_left": gradient.color_bottom_left.rgba(),
            "color_bottom_right": gradient.color_bottom_right.rgba(),
            "width_percent": gradient.width_percent,
            "respect_source_alpha": gradient.respect_source_alpha,
            "fill_mode": if gradient.fill_mode == Gradient4FillMode::AllOpaque { "all_opaque" } else { "specific_color" },
            "target_color": gradient.target_color.rgba(),
        }),
        EffectCard::Reflect(reflect) => json!({
            "effect": "reflect",
            "enabled": true,
            "axis": if reflect.axis == ReflectAxis::X { "x" } else { "y" },
        }),
        EffectCard::Shake(shake) => json!({
            "effect": "shake",
            "enabled": true,
            "angle_deg": shake.angle_deg,
            "up": shake.up_px,
            "down": shake.down_px,
            "steps": shake.steps,
            "base_fade": shake.base_fade,
            "decay": shake.decay,
            "blur": shake.blur_px,
            "autogrow": shake.autogrow,
            "grow_margin": shake.grow_margin_px,
        }),
    }
}

pub(super) fn draw_effect_card_controls(ui: &mut egui::Ui, effect: &mut EffectCard) -> bool {
    let mut changed = false;
    match effect {
        EffectCard::TextShake(shake) => {
            changed |= ui
                .add(WheelSlider::new(&mut shake.spread_x_px, 0.0..=256.0).text(t!("typing.effects.shake_spread_x_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.spread_y_px, 0.0..=256.0).text(t!("typing.effects.shake_spread_y_label")))
                .changed();
            changed |= SeedSpinBox::new(&mut shake.seed)
                .prefix(t!("typing.effects.shake_seed_label"))
                .draw(ui)
                .changed();
        }
        EffectCard::Stroke(stroke) => {
            changed |= ui
                .add(WheelSlider::new(&mut stroke.width_px, 0.0..=24.0).text(t!("typing.params.width_px_label")))
                .changed();
            changed |= stroke.color.draw(ui, t!("typing.effects.color_label"));
            changed |= ui.checkbox(&mut stroke.smoothing, t!("typing.effects.antialias_toggle")).changed();
            ui.add_enabled_ui(stroke.smoothing, |ui| {
                changed |= ui
                    .add(
                        WheelSlider::new(&mut stroke.smoothing_strength_percent, 0.0..=100.0)
                            .text(t!("typing.effects.antialias_strength_label")),
                    )
                    .changed();
            });
            let mut opacity_idx = if stroke.opacity_mode == StrokeOpacityMode::Static {
                0
            } else {
                1
            };
            let stroke_opacity_prev = opacity_idx;
            let stroke_opacity_combo = WheelComboBox::from_label(t!("typing.effects.stroke_opacity_combo_id")).id_salt("typing.effects.stroke_opacity_combo_id")
                .selected_text(match stroke.opacity_mode {
                    StrokeOpacityMode::Static => t!("typing.effects.opacity_static_option"),
                    StrokeOpacityMode::FromContour => t!("typing.effects.opacity_from_outline_option"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(opacity_idx == 0, t!("typing.effects.opacity_static_option"))
                        .clicked()
                    {
                        opacity_idx = 0;
                    }
                    if ui
                        .selectable_label(opacity_idx == 1, t!("typing.effects.opacity_from_outline_option"))
                        .clicked()
                    {
                        opacity_idx = 1;
                    }
                });
            if let Some(steps) = stroke_opacity_combo.wheel_steps {
                cycle_wrapped_index(&mut opacity_idx, 2, steps);
            }
            changed |= opacity_idx != stroke_opacity_prev;
            stroke.opacity_mode = if opacity_idx == 0 {
                StrokeOpacityMode::Static
            } else {
                StrokeOpacityMode::FromContour
            };
            ui.add_enabled_ui(stroke.opacity_mode == StrokeOpacityMode::Static, |ui| {
                changed |= ui
                    .add(
                        WheelSlider::new(&mut stroke.transparency_percent, 0.0..=100.0)
                            .text(t!("typing.effects.opacity_percent_label")),
                    )
                    .changed();
            });
        }
        EffectCard::Shadow(shadow) => {
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_x_px, -400..=400).text(t!("typing.effects.offset_x_px_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_y_px, -400..=400).text(t!("typing.effects.offset_y_px_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.blur_radius_px, 0.0..=64.0).text(t!("typing.effects.blur_px_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut shadow.transparency_percent, 0.0..=100.0)
                        .text(t!("typing.effects.opacity_percent_label")),
                )
                .changed();
            let mut color_mode_idx = if shadow.color_mode == ShadowColorMode::SingleColor {
                0
            } else {
                1
            };
            let color_mode_prev = color_mode_idx;
            let color_mode_combo = WheelComboBox::from_label(t!("typing.effects.color_mode_combo_id")).id_salt("typing.effects.color_mode_combo_id")
                .selected_text(match shadow.color_mode {
                    ShadowColorMode::SingleColor => t!("typing.effects.color_mode_single"),
                    ShadowColorMode::SourceColors => t!("typing.effects.color_mode_source"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(color_mode_idx == 0, t!("typing.effects.color_mode_single"))
                        .clicked()
                    {
                        color_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(color_mode_idx == 1, t!("typing.effects.color_mode_source"))
                        .clicked()
                    {
                        color_mode_idx = 1;
                    }
                });
            if let Some(steps) = color_mode_combo.wheel_steps {
                cycle_wrapped_index(&mut color_mode_idx, 2, steps);
            }
            changed |= color_mode_idx != color_mode_prev;
            shadow.color_mode = if color_mode_idx == 0 {
                ShadowColorMode::SingleColor
            } else {
                ShadowColorMode::SourceColors
            };
            ui.add_enabled_ui(shadow.color_mode == ShadowColorMode::SingleColor, |ui| {
                changed |= shadow.color.draw(ui, t!("typing.effects.color_label"));
            });
        }
        EffectCard::Blur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.radius_px, 0.0..=128.0).text(t!("typing.effects.radius_px_label")))
                .changed();
        }
        EffectCard::MotionBlur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.angle_deg, -360.0..=360.0).text(t!("typing.params.angle_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut blur.distance_px, 0.0..=512.0).text(t!("typing.effects.offset_px_label")))
                .changed();
            let mut sharp_copy_idx = match blur.sharp_copy_mode {
                MotionBlurSharpCopyMode::None => 0,
                MotionBlurSharpCopyMode::Over => 1,
                MotionBlurSharpCopyMode::Under => 2,
            };
            let sharp_copy_prev = sharp_copy_idx;
            let sharp_copy_combo = WheelComboBox::from_label(t!("typing.effects.unblurred_copy_combo_id")).id_salt("typing.effects.unblurred_copy_combo_id")
                .selected_text(match blur.sharp_copy_mode {
                    MotionBlurSharpCopyMode::None => t!("typing.effects.unblurred_copy_none"),
                    MotionBlurSharpCopyMode::Over => t!("typing.effects.unblurred_copy_above"),
                    MotionBlurSharpCopyMode::Under => t!("typing.effects.unblurred_copy_below"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(sharp_copy_idx == 0, t!("typing.effects.unblurred_copy_none")).clicked() {
                        sharp_copy_idx = 0;
                    }
                    if ui.selectable_label(sharp_copy_idx == 1, t!("typing.effects.unblurred_copy_above")).clicked() {
                        sharp_copy_idx = 1;
                    }
                    if ui.selectable_label(sharp_copy_idx == 2, t!("typing.effects.unblurred_copy_below")).clicked() {
                        sharp_copy_idx = 2;
                    }
                });
            if let Some(steps) = sharp_copy_combo.wheel_steps {
                cycle_wrapped_index(&mut sharp_copy_idx, 3, steps);
            }
            changed |= sharp_copy_idx != sharp_copy_prev;
            blur.sharp_copy_mode = match sharp_copy_idx {
                1 => MotionBlurSharpCopyMode::Over,
                2 => MotionBlurSharpCopyMode::Under,
                _ => MotionBlurSharpCopyMode::None,
            };
        }
        EffectCard::DryMedia(dry_media) => {
            let mut material_idx = if dry_media.material == DryMediaMaterial::Pencil {
                0
            } else {
                1
            };
            let material_prev = material_idx;
            let material_combo = WheelComboBox::from_label(t!("typing.effects.material_combo_id")).id_salt("typing.effects.material_combo_id")
                .selected_text(match dry_media.material {
                    DryMediaMaterial::Pencil => t!("typing.effects.material_pencil"),
                    DryMediaMaterial::Chalk => t!("typing.effects.material_chalk"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(material_idx == 0, t!("typing.effects.material_pencil")).clicked() {
                        material_idx = 0;
                    }
                    if ui.selectable_label(material_idx == 1, t!("typing.effects.material_chalk")).clicked() {
                        material_idx = 1;
                    }
                });
            if let Some(steps) = material_combo.wheel_steps {
                cycle_wrapped_index(&mut material_idx, 2, steps);
            }
            changed |= material_idx != material_prev;
            dry_media.material = if material_idx == 0 {
                DryMediaMaterial::Pencil
            } else {
                DryMediaMaterial::Chalk
            };

            changed |= ui
                .add(WheelSlider::new(&mut dry_media.strength, 0.0..=1.0).text(t!("typing.effects.strength_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.seed, 0..=u64::MAX).text(t!("typing.effects.dry_media_seed_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.grain_scale_px, 0.5..=32.0)
                        .text(t!("typing.effects.grain_size_px_label")),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.grain_amount, 0.0..=1.0).text(t!("typing.effects.graininess_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.edge_roughness, 0.0..=1.0)
                        .text(t!("typing.effects.edge_roughness_label")),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.porosity, 0.0..=1.0).text(t!("typing.effects.porosity_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.direction_deg, -360.0..=360.0)
                        .text(t!("typing.effects.stroke_angle_label")),
                )
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.directional_amount, 0.0..=1.0)
                        .text(t!("typing.effects.hatching_strength_label")),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.dust_amount, 0.0..=1.0).text(t!("typing.effects.dust_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.dust_radius_px, 0.0..=32.0)
                        .text(t!("typing.effects.dust_radius_px_label")),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.softness_px, 0.0..=16.0).text(t!("typing.effects.softness_px_label")))
                .changed();
            changed |= ui
                .checkbox(&mut dry_media.use_source_color, t!("typing.effects.keep_source_color"))
                .changed();
            ui.add_enabled_ui(!dry_media.use_source_color, |ui| {
                changed |= dry_media.color.draw(ui, t!("typing.effects.color_label"));
            });
        }
        EffectCard::Glow(glow) => {
            changed |= ui
                .add(WheelSlider::new(&mut glow.radius_px, 0.0..=300.0).text(t!("typing.effects.radius_px_label")))
                .changed();
            if glow.version == GlowEffectVersion::Soft {
                changed |= ui
                    .add(WheelSlider::new(&mut glow.softness_px, 0.0..=100.0).text(t!("typing.effects.softness_px_label")))
                    .changed();
                changed |= glow.color.draw(ui, t!("typing.effects.color_label"));
            } else {
                changed |= glow.color.draw(ui, t!("typing.effects.color_label"));
                let mut opacity_idx = if glow.opacity_mode == StrokeOpacityMode::Static {
                    0
                } else {
                    1
                };
                let glow_opacity_prev = opacity_idx;
                let glow_opacity_combo = WheelComboBox::from_label(t!("typing.effects.glow_opacity_combo_id")).id_salt("typing.effects.glow_opacity_combo_id")
                    .selected_text(match glow.opacity_mode {
                        StrokeOpacityMode::Static => t!("typing.effects.opacity_static_option"),
                        StrokeOpacityMode::FromContour => t!("typing.effects.opacity_from_outline_option"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(opacity_idx == 0, t!("typing.effects.opacity_static_option"))
                            .clicked()
                        {
                            opacity_idx = 0;
                        }
                        if ui
                            .selectable_label(opacity_idx == 1, t!("typing.effects.opacity_from_outline_option"))
                            .clicked()
                        {
                            opacity_idx = 1;
                        }
                    });
                if let Some(steps) = glow_opacity_combo.wheel_steps {
                    cycle_wrapped_index(&mut opacity_idx, 2, steps);
                }
                changed |= opacity_idx != glow_opacity_prev;
                glow.opacity_mode = if opacity_idx == 0 {
                    StrokeOpacityMode::Static
                } else {
                    StrokeOpacityMode::FromContour
                };
                ui.add_enabled_ui(glow.opacity_mode == StrokeOpacityMode::Static, |ui| {
                    changed |= ui
                        .add(
                            WheelSlider::new(&mut glow.transparency_percent, 0.0..=100.0)
                                .text(t!("typing.effects.opacity_percent_label")),
                        )
                        .changed();
                });
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_strength, -100.0..=100.0)
                            .text(t!("typing.effects.falloff_strength_label")),
                    )
                    .changed();
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_shift, -100.0..=100.0)
                            .text(t!("typing.effects.falloff_offset_label")),
                    )
                    .changed();
            }
        }
        EffectCard::Gradient2(gradient) => {
            changed |= gradient.color1.draw(ui, t!("typing.effects.gradient_color1_label"));
            changed |= gradient.color2.draw(ui, t!("typing.effects.gradient_color2_label"));
            changed |= ui
                .add(WheelSlider::new(&mut gradient.angle_deg, -360.0..=360.0).text(t!("typing.params.angle_label")))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text(t!("typing.effects.gradient_width_percent_label")),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, t!("typing.effects.respect_alpha"))
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient2FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient2_fill_prev = fill_mode_idx;
            let gradient2_fill_combo = WheelComboBox::from_label(t!("typing.effects.gradient2_fill_type_combo_id")).id_salt("typing.effects.gradient2_fill_type_combo_id")
                .selected_text(match gradient.fill_mode {
                    Gradient2FillMode::AllOpaque => t!("typing.effects.fill_type_all_opaque"),
                    Gradient2FillMode::SpecificColor => t!("typing.effects.fill_type_specific_color"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, t!("typing.effects.fill_type_all_opaque"))
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, t!("typing.effects.fill_type_specific_color"))
                        .clicked()
                    {
                        fill_mode_idx = 1;
                    }
                });
            if let Some(steps) = gradient2_fill_combo.wheel_steps {
                cycle_wrapped_index(&mut fill_mode_idx, 2, steps);
            }
            changed |= fill_mode_idx != gradient2_fill_prev;
            gradient.fill_mode = if fill_mode_idx == 0 {
                Gradient2FillMode::AllOpaque
            } else {
                Gradient2FillMode::SpecificColor
            };
            ui.add_enabled_ui(
                gradient.fill_mode == Gradient2FillMode::SpecificColor,
                |ui| {
                    changed |= gradient.target_color.draw(ui, t!("typing.effects.fill_replaceable_label"));
                },
            );
        }
        EffectCard::Gradient4(gradient) => {
            changed |= gradient.color_top_left.draw(ui, t!("typing.effects.gradient_corner_top_left_label"));
            changed |= gradient.color_top_right.draw(ui, t!("typing.effects.gradient_corner_top_right_label"));
            changed |= gradient.color_bottom_left.draw(ui, t!("typing.effects.gradient_corner_bottom_left_label"));
            changed |= gradient.color_bottom_right.draw(ui, t!("typing.effects.gradient_corner_bottom_right_label"));
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text(t!("typing.effects.gradient_width_percent_label")),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, t!("typing.effects.respect_alpha"))
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient4FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient4_fill_prev = fill_mode_idx;
            let gradient4_fill_combo = WheelComboBox::from_label(t!("typing.effects.gradient4_fill_type_combo_id")).id_salt("typing.effects.gradient4_fill_type_combo_id")
                .selected_text(match gradient.fill_mode {
                    Gradient4FillMode::AllOpaque => t!("typing.effects.fill_type_all_opaque"),
                    Gradient4FillMode::SpecificColor => t!("typing.effects.fill_type_specific_color"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, t!("typing.effects.fill_type_all_opaque"))
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, t!("typing.effects.fill_type_specific_color"))
                        .clicked()
                    {
                        fill_mode_idx = 1;
                    }
                });
            if let Some(steps) = gradient4_fill_combo.wheel_steps {
                cycle_wrapped_index(&mut fill_mode_idx, 2, steps);
            }
            changed |= fill_mode_idx != gradient4_fill_prev;
            gradient.fill_mode = if fill_mode_idx == 0 {
                Gradient4FillMode::AllOpaque
            } else {
                Gradient4FillMode::SpecificColor
            };
            ui.add_enabled_ui(
                gradient.fill_mode == Gradient4FillMode::SpecificColor,
                |ui| {
                    changed |= gradient.target_color.draw(ui, t!("typing.effects.fill_replaceable_label"));
                },
            );
        }
        EffectCard::Reflect(reflect) => {
            let mut axis_idx = if reflect.axis == ReflectAxis::X { 0 } else { 1 };
            let reflect_axis_prev = axis_idx;
            let reflect_axis_combo = WheelComboBox::from_label(t!("typing.effects.reflection_axis_combo_id")).id_salt("typing.effects.reflection_axis_combo_id")
                .selected_text(match reflect.axis {
                    ReflectAxis::X => t!("typing.effects.reflection_axis_x"),
                    ReflectAxis::Y => t!("typing.effects.reflection_axis_y"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(axis_idx == 0, t!("typing.effects.reflection_axis_x")).clicked() {
                        axis_idx = 0;
                    }
                    if ui
                        .selectable_label(axis_idx == 1, t!("typing.effects.reflection_axis_y"))
                        .clicked()
                    {
                        axis_idx = 1;
                    }
                });
            if let Some(steps) = reflect_axis_combo.wheel_steps {
                cycle_wrapped_index(&mut axis_idx, 2, steps);
            }
            changed |= axis_idx != reflect_axis_prev;
            reflect.axis = if axis_idx == 0 {
                ReflectAxis::X
            } else {
                ReflectAxis::Y
            };
        }
        EffectCard::Shake(shake) => {
            changed |= ui
                .add(WheelSlider::new(&mut shake.angle_deg, -360.0..=360.0).text(t!("typing.params.angle_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.up_px, 0.0..=1000.0).text(t!("typing.effects.shake_amplitude_up_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.down_px, 0.0..=1000.0).text(t!("typing.effects.shake_amplitude_down_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.steps, 0..=128).text(t!("typing.effects.shake_steps_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.base_fade, 0.0..=1.0).text(t!("typing.effects.shake_base_falloff_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.decay, 0.0..=1.0).text(t!("typing.effects.shake_decay_label")))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.blur_px, 0..=64).text("Blur (px)"))
                .changed();
            changed |= ui
                .checkbox(&mut shake.autogrow, "Auto-grow canvas")
                .changed();
            ui.add_enabled_ui(shake.autogrow, |ui| {
                changed |= ui
                    .add(WheelSlider::new(&mut shake.grow_margin_px, 0..=1024).text(t!("typing.effects.shake_extra_padding_label")))
                    .changed();
            });
        }
    }

    changed
}

pub(super) fn spawn_preview_render_worker() -> (Sender<PreviewRenderJob>, Receiver<PreviewRenderResult>) {
    let (request_tx, request_rx) = mpsc::channel::<PreviewRenderJob>();
    let (result_tx, result_rx) = mpsc::channel::<PreviewRenderResult>();

    let _ = thread::Builder::new()
        .name("typing-text-preview-render-worker".to_string())
        .spawn(move || {
            while let Ok(mut job) = request_rx.recv() {
                let mut dropped = 0u32;
                while let Ok(newer_job) = request_rx.try_recv() {
                    job = newer_job;
                    dropped += 1;
                }
                crate::trace_log!(
                    cat::RENDER,
                    "preview_render_worker start token={} dropped_stale={}",
                    job.token,
                    dropped
                );

                let result = render_text_to_image(&job.params, job.fonts.as_ref(), None);
                if let Err(err) = result.as_ref() {
                    eprintln!(
                        "ERROR typing::preview_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} err={}",
                        job.params.text_layout_mode,
                        job.params.text_shape,
                        job.params.text_wrap_mode,
                        job.params.text_line_mode,
                        job.params.width_px,
                        err
                    );
                }
                if result_tx
                    .send(PreviewRenderResult {
                        token: job.token,
                        image: result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

    (request_tx, result_rx)
}
