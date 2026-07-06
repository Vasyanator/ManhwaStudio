/*
File: panel/create_render_data.rs

Purpose:
Part of `impl TypingCreatePanelState`, extracted verbatim from `panel.rs`.
Holds render-data / effects / font-profile / shape-layout JSON building for the
create panel plus the font-profile memory sync used on font selection changes.

Main responsibilities:
- build image-effect and full render-data JSON (per-font and per-index profiles);
- serialize and apply shape / formula / drawn-lines layout parameters;
- store and sync the current font profile in per-font memory and react to a font
  selection change.

Notes:
`use super::*;` pulls in the parent module's types and imports. Methods are
`pub(super)` because `TypingCreatePanelState` is used only inside `panel.rs`.
*/

use super::*;

impl TypingCreatePanelState {
    /// render-data для image-оверлея: только список эффектов (без text_params).
    pub(super) fn build_image_effects_render_data(&self) -> Value {
        json!({ "effects": self.effects_value_array() })
    }

    /// Загружает только эффекты из render-data (для image-оверлеев без text_params).
    pub(super) fn load_effects_only_from_render_data(&mut self, render_data: &Value) {
        self.effects = render_data
            .as_object()
            .and_then(|obj| obj.get("effects"))
            .and_then(Value::as_array)
            .map(|effects| parse_effect_cards(effects, self.text_color))
            .unwrap_or_default();
    }

    pub(super) fn effects_value_array(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.effects.len());
        for effect in self.effects.iter() {
            match effect {
                EffectCard::TextShake(shake) => out.push(json!({
                    "effect": "text_shake",
                    "effect_type": "preprocess",
                    "enabled": true,
                    "spread_x": shake.spread_x_px,
                    "spread_y": shake.spread_y_px,
                    "seed": shake.seed,
                })),
                EffectCard::Stroke(stroke) => out.push(json!({
                    "effect": "stroke",
                    "enabled": true,
                    "width": stroke.width_px,
                    "color": stroke.color.rgba(),
                    "opacity_mode": if stroke.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                    "transparency": stroke.transparency_percent,
                    "opacity": 100.0 - stroke.transparency_percent,
                    "smoothing": stroke.smoothing,
                    "smoothing_strength": stroke.smoothing_strength_percent,
                })),
                EffectCard::Shadow(shadow) => out.push(json!({
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
                })),
                EffectCard::Blur(blur) => out.push(json!({
                    "effect": "blur",
                    "enabled": true,
                    "radius": blur.radius_px,
                    "blur": blur.radius_px,
                })),
                EffectCard::MotionBlur(blur) => out.push(json!({
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
                })),
                EffectCard::DryMedia(dry_media) => out.push(json!({
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
                })),
                EffectCard::Glow(glow) => match glow.version {
                    GlowEffectVersion::V1 | GlowEffectVersion::V2 => out.push(json!({
                        "effect": if glow.version == GlowEffectVersion::V1 { "glow_v1" } else { "glow_v2" },
                        "enabled": true,
                        "radius": glow.radius_px,
                        "color": glow.color.rgba(),
                        "opacity_mode": if glow.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                        "transparency": glow.transparency_percent,
                        "opacity": 100.0 - glow.transparency_percent,
                        "fade_strength": glow.fade_strength,
                        "fade_shift": glow.fade_shift,
                    })),
                    GlowEffectVersion::Soft => out.push(json!({
                        "effect": "soft_glow",
                        "enabled": true,
                        "radius": glow.radius_px.round().max(0.0),
                        "softness": glow.softness_px,
                        "color": glow.color.rgba(),
                    })),
                },
                EffectCard::Gradient2(gradient) => out.push(json!({
                    "effect": "gradient2",
                    "enabled": true,
                    "color1": gradient.color1.rgba(),
                    "color2": gradient.color2.rgba(),
                    "angle_deg": gradient.angle_deg,
                    "width_percent": gradient.width_percent,
                    "respect_source_alpha": gradient.respect_source_alpha,
                    "fill_mode": if gradient.fill_mode == Gradient2FillMode::AllOpaque { "all_opaque" } else { "specific_color" },
                    "target_color": gradient.target_color.rgba(),
                })),
                EffectCard::Gradient4(gradient) => out.push(json!({
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
                })),
                EffectCard::Reflect(reflect) => out.push(json!({
                    "effect": "reflect",
                    "enabled": true,
                    "axis": if reflect.axis == ReflectAxis::X { "x" } else { "y" },
                })),
                EffectCard::Shake(shake) => out.push(json!({
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
                })),
            }
        }
        out
    }

    pub(super) fn build_current_font_profile_json(&self) -> Value {
        self.build_font_profile_json_for_idx(self.selected_font_idx)
    }

    pub(super) fn build_font_profile_json_for_idx(&self, font_idx: usize) -> Value {
        let font_path = self
            .fonts
            .get(font_idx)
            .map(|font| font.path.to_string_lossy().to_string())
            .unwrap_or_default();
        let font_label = self
            .fonts
            .get(font_idx)
            .map(|font| font.label.clone())
            .unwrap_or_default();
        self.build_render_data_json_with_font(
            self.text.clone(),
            self.width_px.max(1),
            Some(font_path),
            Some(font_label),
        )
    }

    pub(super) fn build_render_data_json_for(&self, text: String, width_px: u32) -> Option<Value> {
        let font = self.fonts.get(self.selected_font_idx)?;
        Some(self.build_render_data_json_with_font(
            text,
            width_px.max(1),
            Some(font.path.to_string_lossy().to_string()),
            Some(font.label.clone()),
        ))
    }

    pub(super) fn build_render_data_json_with_font(
        &self,
        text: String,
        width_px: u32,
        font_path: Option<String>,
        font_label: Option<String>,
    ) -> Value {
        let mut render_data = json!({
            "text_params": {
                "text": text,
                "text_color": [self.text_color.r(), self.text_color.g(), self.text_color.b(), self.text_color.a()],
                "font_size_px": self.font_size_px,
                "line_spacing": self.line_spacing.to_token(),
                "kerning_mode": match self.kerning_mode {
                    KerningMode::Fixed => "fixed",
                    KerningMode::Auto => "auto",
                    KerningMode::Optical => "optical",
                },
                "kerning": self.kerning.to_token(),
                "glyph_height": self.glyph_height.to_token(),
                "glyph_width": self.glyph_width.to_token(),
                "width_px": width_px.max(1),
                // `align` — легаси-совместимая строка (PSD-экспорт, старые ридеры),
                // `align_bias` — точное непрерывное смещение слайдера лево↔право.
                "align": self.align.legacy_str(),
                "align_bias": self.align.bias,
                "global_rotation_deg": self.global_rotation_deg,
                "line_placement_percent": self.line_placement_percent,
                "text_line_mode": match self.text_line_mode {
                    TextLineMode::Horizontal => "horizontal",
                    TextLineMode::Vertical => "vertical",
                },
                "vertical_line_direction": match self.vertical_line_direction {
                    VerticalLineDirection::LeftToRight => "left_to_right",
                    VerticalLineDirection::RightToLeft => "right_to_left",
                },
                "text_layout_mode": match self.text_layout_mode {
                    TextLayoutMode::Normal => "normal",
                    TextLayoutMode::Formula => "formula",
                    TextLayoutMode::Shape => "shape",
                    TextLayoutMode::CustomRasterLines => "custom_raster_lines",
                    TextLayoutMode::CustomVectorLines => "custom_vector_lines",
                },
                "formula_layout": text_formula_layout_to_value(&self.formula_layout),
                "shape_layout": self.shape_layout_to_value(),
                "drawn_lines_layout": text_drawn_lines_layout_to_value(&self.drawn_lines_layout_for_render()),
                "vector_lines_layout": text_vector_lines_layout_to_value(&self.vector_lines_layout),
                "selected_face_index": self.selected_face_idx,
                "force_bold": self.force_bold,
                "force_italic": self.force_italic,
                "uppercase_text": self.uppercase_text,
                "trim_extra_spaces": self.trim_extra_spaces,
                "hanging_punctuation": self.hanging_punctuation,
                "new_line_after_sentence": self.new_line_after_sentence,
                "enable_inline_style_tags": self.enable_inline_style_tags,
                "text_wrap_mode": match self.text_wrap_mode {
                    TextWrapMode::None => "none",
                    TextWrapMode::WholeWords => "whole_words",
                    TextWrapMode::Minimal => "minimal",
                    TextWrapMode::Moderate => "moderate",
                    TextWrapMode::Aggressive => "aggressive",
                },
                "anti_aliasing": match self.anti_aliasing {
                    AntiAliasingMode::None => "none",
                    AntiAliasingMode::Sharp => "sharp",
                    AntiAliasingMode::Crisp => "crisp",
                    AntiAliasingMode::Strong => "strong",
                    AntiAliasingMode::Smooth => "smooth",
                },
                "allow_moderate_trees": self.allow_moderate_trees,
                "text_shape": match self.text_shape {
                    TextShape::Free => "free",
                    TextShape::Rectangle => "rectangle",
                    TextShape::Oval => "oval",
                    TextShape::Hexagon => "hexagon",
                    TextShape::SoftPeak => "soft_peak",
                },
                "shape_min_width_percent": self.shape_min_width_percent,
                "shape_variant": self.shape_variant,
                "font_path": font_path,
                "font_label": font_label,
                // Сформированный (разбитый на строки) текст «продвинутой формы».
                // Если не пуст — именно он идёт в рендер; `text` остаётся исходным.
                // Переживает перезапуск.
                "formed_text": self.formed_text,
            },
            "effects": self.effects_value_array(),
        });
        // Carry the canvas-authored vector mesh warp verbatim (Phase 3). Only
        // emit the key when present so old overlays stay byte-stable/clean.
        if let Some(raster_transform) = self.pending_raster_transform.clone()
            && let Some(text_params) = render_data
                .get_mut("text_params")
                .and_then(Value::as_object_mut)
        {
            text_params.insert("raster_transform".to_string(), raster_transform);
        }
        render_data
    }

    pub(super) fn shape_layout_to_value(&self) -> Value {
        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => json!({
                "kind": "arc",
                "length_px": self.arc_shape_layout.length_px,
                "amplitude_px": self.arc_shape_layout.amplitude_px,
                "width_px": self.arc_shape_layout.length_px,
                "height_px": self.arc_shape_layout.amplitude_px,
                "frequency": self.arc_shape_layout.frequency,
                "orientation": self.arc_shape_layout.orientation.as_config_str(),
            }),
            TypingShapeLayoutKind::Circle => json!({
                "kind": "circle",
                "width_px": self.circle_shape_layout.width_px,
                "height_px": self.circle_shape_layout.height_px,
            }),
            TypingShapeLayoutKind::Spiral => json!({
                "kind": "spiral",
                "width_px": self.spiral_shape_layout.width_px,
                "height_px": self.spiral_shape_layout.height_px,
                "turns": self.spiral_shape_layout.turns,
                "inner_ratio": self.spiral_shape_layout.inner_ratio,
            }),
            TypingShapeLayoutKind::Polygon => json!({
                "kind": "polygon",
                "width_px": self.polygon_shape_layout.width_px,
                "height_px": self.polygon_shape_layout.height_px,
                "sides": self.polygon_shape_layout.sides,
            }),
            TypingShapeLayoutKind::Zigzag => json!({
                "kind": "zigzag",
                "width_px": self.zigzag_shape_layout.width_px,
                "height_px": self.zigzag_shape_layout.height_px,
                "segments": self.zigzag_shape_layout.segments,
            }),
            TypingShapeLayoutKind::SCurve => json!({
                "kind": "s_curve",
                "width_px": self.s_curve_shape_layout.width_px,
                "height_px": self.s_curve_shape_layout.height_px,
                "bends": self.s_curve_shape_layout.bends,
            }),
        }
    }

    pub(super) fn apply_shape_layout_json(&mut self, obj: &Map<String, Value>) {
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .map(|raw| raw.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "arc".to_string());
        self.shape_layout_kind = match kind.as_str() {
            "arc" => TypingShapeLayoutKind::Arc,
            "circle" | "ellipse" | "oval" => TypingShapeLayoutKind::Circle,
            "spiral" => TypingShapeLayoutKind::Spiral,
            "polygon" => TypingShapeLayoutKind::Polygon,
            "zigzag" => TypingShapeLayoutKind::Zigzag,
            "s_curve" | "s-curve" | "scurve" => TypingShapeLayoutKind::SCurve,
            _ => TypingShapeLayoutKind::Arc,
        };
        self.arc_shape_layout.length_px = obj
            .get("length_px")
            .and_then(value_as_f32)
            .or_else(|| obj.get("width_px").and_then(value_as_f32))
            .unwrap_or(self.arc_shape_layout.length_px)
            .clamp(1.0, 10_000.0);
        self.arc_shape_layout.amplitude_px = obj
            .get("amplitude_px")
            .and_then(value_as_f32)
            .or_else(|| obj.get("height_px").and_then(value_as_f32))
            .unwrap_or(self.arc_shape_layout.amplitude_px)
            .clamp(-10_000.0, 10_000.0);
        self.arc_shape_layout.frequency = obj
            .get("frequency")
            .and_then(value_as_f32)
            .unwrap_or(self.arc_shape_layout.frequency)
            .clamp(0.1, 32.0);
        self.arc_shape_layout.orientation = obj
            .get("orientation")
            .and_then(Value::as_str)
            .and_then(TypingArcOrientation::from_config_str)
            .unwrap_or(self.arc_shape_layout.orientation);
        self.circle_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.circle_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.circle_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.circle_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.turns = obj
            .get("turns")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.turns)
            .clamp(0.25, 16.0);
        self.spiral_shape_layout.inner_ratio = obj
            .get("inner_ratio")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.inner_ratio)
            .clamp(0.0, 0.98);
        self.polygon_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.polygon_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.polygon_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.polygon_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.polygon_shape_layout.sides = obj
            .get("sides")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(self.polygon_shape_layout.sides)
            .clamp(3, 12);
        self.zigzag_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.zigzag_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.height_px)
            .clamp(-10_000.0, 10_000.0);
        self.zigzag_shape_layout.segments = obj
            .get("segments")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.segments)
            .clamp(0.5, 32.0);
        self.s_curve_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.s_curve_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.height_px)
            .clamp(-10_000.0, 10_000.0);
        self.s_curve_shape_layout.bends = obj
            .get("bends")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.bends)
            .clamp(0.25, 8.0);
    }

    pub(super) fn formula_layout_for_render(&self) -> TextFormulaLayoutParams {
        match self.text_layout_mode {
            TextLayoutMode::Shape => self.shape_formula_layout(),
            _ => self.formula_layout.clone(),
        }
    }

    pub(super) fn drawn_lines_layout_for_render(&self) -> TextDrawnLinesLayoutParams {
        self.drawn_lines_layout.clone()
    }

    pub(super) fn shape_formula_layout(&self) -> TextFormulaLayoutParams {
        let mut layout = self.formula_layout.clone();
        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => {
                match self.arc_shape_layout.orientation {
                    TypingArcOrientation::Horizontal => {
                        layout.x_expr = "a * (t - 0.5)".to_string();
                        layout.y_expr = "b * sin(pi * c * t)".to_string();
                    }
                    TypingArcOrientation::Vertical => {
                        layout.x_expr = "b * sin(pi * c * t)".to_string();
                        layout.y_expr = "a * (t - 0.5)".to_string();
                    }
                }
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.arc_shape_layout.length_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .arc_shape_layout
                    .amplitude_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.arc_shape_layout.frequency.clamp(0.1, 32.0);
            }
            TypingShapeLayoutKind::Circle => {
                layout.x_expr = "a * cos(tau * t)".to_string();
                layout.y_expr = "b * sin(tau * t)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.circle_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.circle_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
            }
            TypingShapeLayoutKind::Spiral => {
                layout.x_expr = "(a * (d + (1 - d) * t)) * cos(tau * c * t)".to_string();
                layout.y_expr = "(b * (d + (1 - d) * t)) * sin(tau * c * t)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.spiral_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.spiral_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[2] = self.spiral_shape_layout.turns.clamp(0.25, 16.0);
                layout.vars[3] = self.spiral_shape_layout.inner_ratio.clamp(0.0, 0.98);
            }
            TypingShapeLayoutKind::Polygon => {
                layout.x_expr = "a * cos(tau * t) * cos(pi / c) / cos(atan2(sin(c * tau * t), cos(c * tau * t)) / c)".to_string();
                layout.y_expr = "b * sin(tau * t) * cos(pi / c) / cos(atan2(sin(c * tau * t), cos(c * tau * t)) / c)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.polygon_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.polygon_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[2] = self.polygon_shape_layout.sides.clamp(3, 12) as f32;
            }
            TypingShapeLayoutKind::Zigzag => {
                layout.x_expr = "a * (t - 0.5)".to_string();
                layout.y_expr = "b * (2 / pi) * asin(sin(pi * c * t))".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.zigzag_shape_layout.width_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .zigzag_shape_layout
                    .height_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.zigzag_shape_layout.segments.clamp(0.5, 32.0);
            }
            TypingShapeLayoutKind::SCurve => {
                layout.x_expr = "a * (t - 0.5)".to_string();
                layout.y_expr = "b * sin(pi * c * (t - 0.5))".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.s_curve_shape_layout.width_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .s_curve_shape_layout
                    .height_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.s_curve_shape_layout.bends.clamp(0.25, 8.0);
            }
        }
        layout
    }

    pub(super) fn store_current_font_profile_by_idx(&mut self, idx: usize) {
        if !self.preview_enabled {
            return;
        }
        let Some(font_key) = self.font_key_by_idx(idx) else {
            return;
        };
        self.font_profiles_by_key
            .insert(font_key.clone(), self.build_font_profile_json_for_idx(idx));
        self.active_font_key = Some(font_key);
    }

    pub(super) fn sync_current_font_profile_memory(&mut self) {
        if !self.preview_enabled {
            return;
        }
        self.store_current_font_profile_by_idx(self.selected_font_idx);
    }

    pub(super) fn handle_create_font_selection_change(&mut self, prev_font_idx: usize) -> bool {
        if !self.preview_enabled {
            return false;
        }
        self.store_current_font_profile_by_idx(prev_font_idx);
        let Some(new_font_key) = self.current_font_key() else {
            return false;
        };
        self.active_font_key = Some(new_font_key.clone());
        if let Some(profile) = self.font_profiles_by_key.get(&new_font_key).cloned() {
            self.apply_render_data_json_with_options(&profile, false);
            self.clamp_face_index();
            return true;
        }
        self.selected_face_idx = 0;
        self.sync_current_font_profile_memory();
        true
    }

}
