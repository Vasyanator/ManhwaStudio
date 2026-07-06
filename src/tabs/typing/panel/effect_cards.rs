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
        EffectCard::TextShake(_) => "Тряска текста",
        EffectCard::Stroke(_) => "Обводка",
        EffectCard::Shadow(_) => "Тень",
        EffectCard::Blur(_) => "Размытие",
        EffectCard::MotionBlur(_) => "Размытие в движении",
        EffectCard::DryMedia(_) => "Мел/Карандаш",
        EffectCard::Glow(glow) => match glow.version {
            GlowEffectVersion::V1 => "Свечение V1",
            GlowEffectVersion::V2 => "Свечение V2",
            GlowEffectVersion::Soft => "Мягкое свечение",
        },
        EffectCard::Gradient2(_) => "Градиент 2",
        EffectCard::Gradient4(_) => "Градиент 4",
        EffectCard::Reflect(_) => "Отражение",
        EffectCard::Shake(_) => "Тряска",
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
                .add(WheelSlider::new(&mut shake.spread_x_px, 0.0..=256.0).text("Разброс по X"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.spread_y_px, 0.0..=256.0).text("Разброс по Y"))
                .changed();
            changed |= SeedSpinBox::new(&mut shake.seed)
                .prefix("Сид ")
                .draw(ui)
                .changed();
        }
        EffectCard::Stroke(stroke) => {
            changed |= ui
                .add(WheelSlider::new(&mut stroke.width_px, 0.0..=24.0).text("Ширина (px)"))
                .changed();
            changed |= stroke.color.draw(ui, "Цвет:");
            changed |= ui.checkbox(&mut stroke.smoothing, "Сглаживание").changed();
            ui.add_enabled_ui(stroke.smoothing, |ui| {
                changed |= ui
                    .add(
                        WheelSlider::new(&mut stroke.smoothing_strength_percent, 0.0..=100.0)
                            .text("Сила сглаживания (%)"),
                    )
                    .changed();
            });
            let mut opacity_idx = if stroke.opacity_mode == StrokeOpacityMode::Static {
                0
            } else {
                1
            };
            let stroke_opacity_prev = opacity_idx;
            let stroke_opacity_combo = WheelComboBox::from_label("Прозрачность контура")
                .selected_text(match stroke.opacity_mode {
                    StrokeOpacityMode::Static => "Статическая",
                    StrokeOpacityMode::FromContour => "От контура",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(opacity_idx == 0, "Статическая")
                        .clicked()
                    {
                        opacity_idx = 0;
                    }
                    if ui
                        .selectable_label(opacity_idx == 1, "От контура")
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
                            .text("Прозрачность (%)"),
                    )
                    .changed();
            });
        }
        EffectCard::Shadow(shadow) => {
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_x_px, -400..=400).text("Смещение X (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_y_px, -400..=400).text("Смещение Y (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.blur_radius_px, 0.0..=64.0).text("Размытие (px)"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut shadow.transparency_percent, 0.0..=100.0)
                        .text("Прозрачность (%)"),
                )
                .changed();
            let mut color_mode_idx = if shadow.color_mode == ShadowColorMode::SingleColor {
                0
            } else {
                1
            };
            let color_mode_prev = color_mode_idx;
            let color_mode_combo = WheelComboBox::from_label("Режим цвета")
                .selected_text(match shadow.color_mode {
                    ShadowColorMode::SingleColor => "Один цвет",
                    ShadowColorMode::SourceColors => "Исходные цвета",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(color_mode_idx == 0, "Один цвет")
                        .clicked()
                    {
                        color_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(color_mode_idx == 1, "Исходные цвета")
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
                changed |= shadow.color.draw(ui, "Цвет:");
            });
        }
        EffectCard::Blur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.radius_px, 0.0..=128.0).text("Радиус (px)"))
                .changed();
        }
        EffectCard::MotionBlur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut blur.distance_px, 0.0..=512.0).text("Смещение (px)"))
                .changed();
            let mut sharp_copy_idx = match blur.sharp_copy_mode {
                MotionBlurSharpCopyMode::None => 0,
                MotionBlurSharpCopyMode::Over => 1,
                MotionBlurSharpCopyMode::Under => 2,
            };
            let sharp_copy_prev = sharp_copy_idx;
            let sharp_copy_combo = WheelComboBox::from_label("Неразмытая копия")
                .selected_text(match blur.sharp_copy_mode {
                    MotionBlurSharpCopyMode::None => "Нет",
                    MotionBlurSharpCopyMode::Over => "Сверху",
                    MotionBlurSharpCopyMode::Under => "Снизу",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(sharp_copy_idx == 0, "Нет").clicked() {
                        sharp_copy_idx = 0;
                    }
                    if ui.selectable_label(sharp_copy_idx == 1, "Сверху").clicked() {
                        sharp_copy_idx = 1;
                    }
                    if ui.selectable_label(sharp_copy_idx == 2, "Снизу").clicked() {
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
            let material_combo = WheelComboBox::from_label("Материал")
                .selected_text(match dry_media.material {
                    DryMediaMaterial::Pencil => "Карандаш",
                    DryMediaMaterial::Chalk => "Мел",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(material_idx == 0, "Карандаш").clicked() {
                        material_idx = 0;
                    }
                    if ui.selectable_label(material_idx == 1, "Мел").clicked() {
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
                .add(WheelSlider::new(&mut dry_media.strength, 0.0..=1.0).text("Сила"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.seed, 0..=u64::MAX).text("Сид"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.grain_scale_px, 0.5..=32.0)
                        .text("Размер зерна (px)"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.grain_amount, 0.0..=1.0).text("Зернистость"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.edge_roughness, 0.0..=1.0)
                        .text("Рваность края"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.porosity, 0.0..=1.0).text("Пористость"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.direction_deg, -360.0..=360.0)
                        .text("Угол штриха (°)"),
                )
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.directional_amount, 0.0..=1.0)
                        .text("Сила штриховки"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.dust_amount, 0.0..=1.0).text("Пыль"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.dust_radius_px, 0.0..=32.0)
                        .text("Радиус пыли (px)"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.softness_px, 0.0..=16.0).text("Мягкость (px)"))
                .changed();
            changed |= ui
                .checkbox(&mut dry_media.use_source_color, "Сохранить исходный цвет")
                .changed();
            ui.add_enabled_ui(!dry_media.use_source_color, |ui| {
                changed |= dry_media.color.draw(ui, "Цвет:");
            });
        }
        EffectCard::Glow(glow) => {
            changed |= ui
                .add(WheelSlider::new(&mut glow.radius_px, 0.0..=300.0).text("Радиус (px)"))
                .changed();
            if glow.version == GlowEffectVersion::Soft {
                changed |= ui
                    .add(WheelSlider::new(&mut glow.softness_px, 0.0..=100.0).text("Мягкость (px)"))
                    .changed();
                changed |= glow.color.draw(ui, "Цвет:");
            } else {
                changed |= glow.color.draw(ui, "Цвет:");
                let mut opacity_idx = if glow.opacity_mode == StrokeOpacityMode::Static {
                    0
                } else {
                    1
                };
                let glow_opacity_prev = opacity_idx;
                let glow_opacity_combo = WheelComboBox::from_label("Прозрачность")
                    .selected_text(match glow.opacity_mode {
                        StrokeOpacityMode::Static => "Статическая",
                        StrokeOpacityMode::FromContour => "От контура",
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(opacity_idx == 0, "Статическая")
                            .clicked()
                        {
                            opacity_idx = 0;
                        }
                        if ui
                            .selectable_label(opacity_idx == 1, "От контура")
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
                                .text("Прозрачность (%)"),
                        )
                        .changed();
                });
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_strength, -100.0..=100.0)
                            .text("Сила затухания"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_shift, -100.0..=100.0)
                            .text("Смещение затухания"),
                    )
                    .changed();
            }
        }
        EffectCard::Gradient2(gradient) => {
            changed |= gradient.color1.draw(ui, "Цвет 1:");
            changed |= gradient.color2.draw(ui, "Цвет 2:");
            changed |= ui
                .add(WheelSlider::new(&mut gradient.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text("Ширина градиента (%)"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, "Учитывать прозрачность")
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient2FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient2_fill_prev = fill_mode_idx;
            let gradient2_fill_combo = WheelComboBox::from_label("Тип заполнения")
                .selected_text(match gradient.fill_mode {
                    Gradient2FillMode::AllOpaque => "Всё непрозрачное",
                    Gradient2FillMode::SpecificColor => "Конкретный цвет",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, "Всё непрозрачное")
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, "Конкретный цвет")
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
                    changed |= gradient.target_color.draw(ui, "Заменяемый:");
                },
            );
        }
        EffectCard::Gradient4(gradient) => {
            changed |= gradient.color_top_left.draw(ui, "Левый верх:");
            changed |= gradient.color_top_right.draw(ui, "Правый верх:");
            changed |= gradient.color_bottom_left.draw(ui, "Левый низ:");
            changed |= gradient.color_bottom_right.draw(ui, "Правый низ:");
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text("Ширина градиента (%)"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, "Учитывать прозрачность")
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient4FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient4_fill_prev = fill_mode_idx;
            let gradient4_fill_combo = WheelComboBox::from_label("Тип заполнения")
                .selected_text(match gradient.fill_mode {
                    Gradient4FillMode::AllOpaque => "Всё непрозрачное",
                    Gradient4FillMode::SpecificColor => "Конкретный цвет",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, "Всё непрозрачное")
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, "Конкретный цвет")
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
                    changed |= gradient.target_color.draw(ui, "Заменяемый:");
                },
            );
        }
        EffectCard::Reflect(reflect) => {
            let mut axis_idx = if reflect.axis == ReflectAxis::X { 0 } else { 1 };
            let reflect_axis_prev = axis_idx;
            let reflect_axis_combo = WheelComboBox::from_label("Ось отражения")
                .selected_text(match reflect.axis {
                    ReflectAxis::X => "X (верх-низ)",
                    ReflectAxis::Y => "Y (лево-право)",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(axis_idx == 0, "X (верх-низ)").clicked() {
                        axis_idx = 0;
                    }
                    if ui
                        .selectable_label(axis_idx == 1, "Y (лево-право)")
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
                .add(WheelSlider::new(&mut shake.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.up_px, 0.0..=1000.0).text("Ампл. вверх (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.down_px, 0.0..=1000.0).text("Ампл. вниз (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.steps, 0..=128).text("Шаги"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.base_fade, 0.0..=1.0).text("Базовое затухание"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.decay, 0.0..=1.0).text("Спад"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.blur_px, 0..=64).text("Blur (px)"))
                .changed();
            changed |= ui
                .checkbox(&mut shake.autogrow, "Auto-grow canvas")
                .changed();
            ui.add_enabled_ui(shake.autogrow, |ui| {
                changed |= ui
                    .add(WheelSlider::new(&mut shake.grow_margin_px, 0..=1024).text("Доп. отступ"))
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

                let result = render_text_to_image(&job.params, None);
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
