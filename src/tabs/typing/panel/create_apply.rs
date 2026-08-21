/*
File: tabs/typing/panel/create_apply.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
applying selected-overlay data into the panel (load and sync transform,
apply render-data JSON with options), font selection, the preview render
queue and poll, and the render-param builders for create and edit.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so the panel
module root and its siblings can call them. `use super::*;` pulls in the
parent module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {
    pub(super) fn draw_image_transform_only_section(
        &mut self,
        ui: &mut egui::Ui,
        remap_wheel_to_horizontal: bool,
    ) -> bool {
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        ui.vertical(|ui| {
            let scale_resp =
                ui.add(WheelSlider::new(&mut self.overlay_scale, 0.05..=20.0).text(t!("typing.params.scale_label")));
            mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &scale_resp);
            changed |= scale_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &scale_resp) {
                changed |= apply_wheel_step_f32(&mut self.overlay_scale, steps, 0.05, 0.05, 20.0);
            }

            let angle_resp = ui.add(
                WheelSlider::new(&mut self.overlay_rotation_deg, -180.0..=180.0).text(t!("typing.params.angle_label")),
            );
            mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &angle_resp);
            changed |= angle_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &angle_resp) {
                changed |=
                    apply_wheel_step_f32(&mut self.overlay_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
        });
        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        changed
    }

    pub(super) fn load_from_selected_overlay(&mut self, selected: &TypingSelectedOverlayForEdit) {
        self.overlay_scale = selected.user_scale.max(0.05);
        self.overlay_rotation_deg = normalize_angle_deg(selected.rotation_deg);
        self.width_px = selected.width_px_hint.max(1);

        // Сбрасываем флаг ненайденного шрифта: его заново выставит
        // `apply_render_data_json_with_options`/`select_font_by_path_or_label`,
        // если шрифт нового оверлея отсутствует среди доступных.
        self.missing_font = None;

        // Сформированный текст персонален для оверлея: сбрасываем перед загрузкой,
        // чтобы он не «наследовался» от ранее выбранного оверлея.
        // `apply_render_data_json_with_options` восстановит его из JSON, если есть.
        self.formed_text.clear();
        // Предупреждение о потерянных тегах относилось к форме ПРОШЛОГО оверлея.
        self.advanced_form_tags_lost = false;
        self.advanced_text_show_formed = false;
        // Кэш окна форм относится к прошлому оверлею — инвалидируем.
        self.advanced_form_cache = None;
        if let Some(render_data) = selected.render_data_json.as_ref() {
            self.apply_render_data_json_with_options(render_data, true);
        }
        self.clamp_face_index();
    }

    pub(super) fn sync_overlay_transform_from_selected_overlay(
        &mut self,
        selected: &TypingSelectedOverlayForEdit,
    ) {
        self.overlay_scale = selected.user_scale.max(0.05);
        self.overlay_rotation_deg = normalize_angle_deg(selected.rotation_deg);
    }

    pub(super) fn apply_render_data_json_with_options(
        &mut self,
        render_data: &Value,
        apply_font_selection: bool,
    ) {
        let Some(render_data_obj) = render_data.as_object() else {
            return;
        };
        let Some(stored_text_params) = render_data_obj
            .get("text_params")
            .and_then(Value::as_object)
        else {
            return;
        };
        // Fill in the defaults of the schema the DOCUMENT declares before reading a
        // single field: a schema-2 payload omits everything equal to its frozen default,
        // while a schema-1 payload is handed through untouched so the legacy
        // absent-key rules below (which differ from today's defaults for e.g.
        // `line_placement_reference`) keep applying to it.
        let filled_text_params = text_params_schema::read_text_params(stored_text_params);
        let text_params_obj = &*filled_text_params;

        if let Some(text) = text_params_obj.get("text").and_then(Value::as_str) {
            self.text = text.to_string();
        }
        if let Some(text_color) = text_params_obj
            .get("text_color")
            .and_then(parse_color32_value)
        {
            self.text_color = text_color;
        }
        self.font_size_px = text_params_obj
            .get("font_size_px")
            .and_then(value_as_f32)
            .unwrap_or(self.font_size_px)
            .clamp(1.0, 256.0);
        self.line_spacing = clamp_px_or_percent(
            read_legacy_or_token_px_or_percent(
                text_params_obj,
                "line_spacing",
                "line_spacing_px",
                "line_spacing_percent",
                self.line_spacing,
            ),
            300.0,
        );
        self.kerning_mode = text_params_obj
            .get("kerning_mode")
            .and_then(Value::as_str)
            .and_then(parse_kerning_mode_str)
            .unwrap_or(KerningMode::Auto);
        self.kerning = clamp_px_or_percent(
            read_legacy_or_token_px_or_percent(
                text_params_obj,
                "kerning",
                "kerning_px",
                "kerning_percent",
                self.kerning,
            ),
            300.0,
        );
        self.glyph_height = clamp_stretch_px_or_percent(read_legacy_or_token_px_or_percent(
            text_params_obj,
            "glyph_height",
            "",
            "glyph_height_percent",
            self.glyph_height,
        ));
        self.glyph_width = clamp_stretch_px_or_percent(read_legacy_or_token_px_or_percent(
            text_params_obj,
            "glyph_width",
            "",
            "glyph_width_percent",
            self.glyph_width,
        ));
        self.width_px = text_params_obj
            .get("width_px")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(self.width_px)
            .max(1);
        if text_params_obj.get("align").is_some() || text_params_obj.get("align_bias").is_some() {
            self.align = HorizontalAlign::from_config(
                text_params_obj.get("align").and_then(Value::as_str),
                text_params_obj.get("align_bias").and_then(value_as_f32),
            );
        }
        // Absent in projects saved before global rotation existed -> keep 0.0.
        if let Some(global_rotation_deg) = text_params_obj
            .get("global_rotation_deg")
            .and_then(value_as_f32)
        {
            self.global_rotation_deg = global_rotation_deg;
        }
        // Carry the canvas-authored vector mesh warp verbatim so it survives an
        // edit (the panel rebuilds render_data on every change). Absent -> None.
        self.pending_raster_transform = text_params_obj.get("raster_transform").cloned();
        // Absent in projects saved before perpendicular line placement -> keep 0.0.
        if let Some(line_placement_percent) = text_params_obj
            .get("line_placement_percent")
            .and_then(value_as_f32)
        {
            self.line_placement_percent = line_placement_percent;
        }
        // Absent in projects saved before the reference toggle -> legacy GlyphHeight.
        self.line_placement_reference = text_params_obj
            .get("line_placement_reference")
            .and_then(Value::as_str)
            .map_or(LinePlacementReference::GlyphHeight, |token| {
                LinePlacementReference::from_json_str(token)
            });
        if let Some(text_line_mode) = text_params_obj
            .get("text_line_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_line_mode_str)
        {
            self.text_line_mode = text_line_mode;
        }
        if let Some(vertical_line_direction) = text_params_obj
            .get("vertical_line_direction")
            .and_then(Value::as_str)
            .and_then(parse_vertical_line_direction_str)
        {
            self.vertical_line_direction = vertical_line_direction;
        } else {
            self.vertical_line_direction = VerticalLineDirection::RightToLeft;
        }
        if let Some(text_layout_mode) = text_params_obj
            .get("text_layout_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_layout_mode_str)
        {
            self.text_layout_mode = text_layout_mode;
        } else {
            self.text_layout_mode = TextLayoutMode::Normal;
        }
        if let Some(formula_obj) = text_params_obj
            .get("formula_layout")
            .and_then(Value::as_object)
        {
            if let Some(x_expr) = formula_obj.get("x_expr").and_then(Value::as_str) {
                self.formula_layout.x_expr = x_expr.to_string();
            }
            if let Some(y_expr) = formula_obj.get("y_expr").and_then(Value::as_str) {
                self.formula_layout.y_expr = y_expr.to_string();
            }
            if let Some(rotation_expr) = formula_obj.get("rotation_expr").and_then(Value::as_str) {
                self.formula_layout.rotation_expr = rotation_expr.to_string();
            }
            self.formula_layout.use_tangent_rotation = formula_obj
                .get("use_tangent_rotation")
                .and_then(Value::as_bool)
                .unwrap_or(self.formula_layout.use_tangent_rotation);
            self.formula_layout.t_start = formula_obj
                .get("t_start")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.t_start);
            self.formula_layout.t_end = formula_obj
                .get("t_end")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.t_end);
            self.formula_layout.offset_x_px = formula_obj
                .get("offset_x_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.offset_x_px);
            self.formula_layout.offset_y_px = formula_obj
                .get("offset_y_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.offset_y_px);
            self.formula_layout.scale_x = formula_obj
                .get("scale_x")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.scale_x);
            self.formula_layout.scale_y = formula_obj
                .get("scale_y")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.scale_y);
            self.formula_layout.normal_offset_px = formula_obj
                .get("normal_offset_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.normal_offset_px);
            self.formula_layout.letter_spacing_mul = formula_obj
                .get("letter_spacing_mul")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.letter_spacing_mul)
                .clamp(0.0, 8.0);
            self.formula_layout.letter_spacing_px = formula_obj
                .get("letter_spacing_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0);
            if let Some(vars_arr) = formula_obj.get("vars").and_then(Value::as_array) {
                for (idx, value) in vars_arr
                    .iter()
                    .take(TEXT_FORMULA_USER_VAR_COUNT)
                    .enumerate()
                {
                    if let Some(parsed) = value_as_f32(value) {
                        self.formula_layout.vars[idx] = parsed;
                    }
                }
            }
        } else {
            self.formula_layout = TextFormulaLayoutParams::default();
        }
        self.drawn_lines_layout = text_params_obj
            .get("drawn_lines_layout")
            .and_then(text_drawn_lines_layout_from_value)
            .unwrap_or_default();
        self.vector_lines_layout = text_params_obj
            .get("vector_lines_layout")
            .and_then(text_vector_lines_layout_from_value)
            .unwrap_or_default();
        if let Some(shape_layout_obj) = text_params_obj
            .get("shape_layout")
            .and_then(Value::as_object)
        {
            self.apply_shape_layout_json(shape_layout_obj);
        } else {
            self.shape_layout_kind = TypingShapeLayoutKind::Arc;
            self.arc_shape_layout = TypingArcShapeLayoutParams::default();
            self.circle_shape_layout = TypingCircleShapeLayoutParams::default();
            self.spiral_shape_layout = TypingSpiralShapeLayoutParams::default();
            self.polygon_shape_layout = TypingPolygonShapeLayoutParams::default();
            self.zigzag_shape_layout = TypingZigzagShapeLayoutParams::default();
            self.s_curve_shape_layout = TypingSCurveShapeLayoutParams::default();
        }
        self.selected_face_idx = text_params_obj
            .get("selected_face_index")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0usize);
        self.force_bold = text_params_obj
            .get("force_bold")
            .and_then(Value::as_bool)
            .unwrap_or(self.force_bold);
        self.force_italic = text_params_obj
            .get("force_italic")
            .and_then(Value::as_bool)
            .unwrap_or(self.force_italic);
        self.faux_bold = text_params_obj.get("faux_bold").and_then(Value::as_bool).unwrap_or(false);
        // SIGNED: a negative stored value is a legitimate THINNING request.
        self.faux_bold_thicken_percent = text_params_obj.get("faux_bold_thicken_percent").and_then(value_as_f32).unwrap_or(3.0).clamp(FAUX_THICKEN_PERCENT_MIN, FAUX_THICKEN_PERCENT_MAX);
        self.faux_bold_expand_percent = text_params_obj.get("faux_bold_expand_percent").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 50.0);
        self.faux_bold_sharp_corners = text_params_obj.get("faux_bold_sharp_corners").and_then(Value::as_bool).unwrap_or(true);
        self.faux_bold_outward_only = text_params_obj.get("faux_bold_outward_only").and_then(Value::as_bool).unwrap_or(true);
        self.faux_italic = text_params_obj.get("faux_italic").and_then(Value::as_bool).unwrap_or(false);
        self.faux_italic_slant_deg = text_params_obj.get("faux_italic_slant_deg").and_then(value_as_f32).unwrap_or(14.0).clamp(-45.0, 45.0);
        self.hanging_punctuation = text_params_obj
            .get("hanging_punctuation")
            .and_then(Value::as_bool)
            .unwrap_or(self.hanging_punctuation);
        self.trim_extra_spaces = text_params_obj
            .get("trim_extra_spaces")
            .and_then(Value::as_bool)
            .unwrap_or(self.trim_extra_spaces);
        self.replace_ellipsis_with_dots = text_params_obj
            .get("replace_ellipsis_with_dots")
            .and_then(Value::as_bool)
            .unwrap_or(self.replace_ellipsis_with_dots);
        self.force_remove_ellipsis_glyph = text_params_obj
            .get("force_remove_ellipsis_glyph")
            .and_then(Value::as_bool)
            .unwrap_or(self.force_remove_ellipsis_glyph);
        self.new_line_after_sentence = text_params_obj
            .get("new_line_after_sentence")
            .and_then(Value::as_bool)
            .unwrap_or(self.new_line_after_sentence);
        self.enable_inline_style_tags = text_params_obj
            .get("enable_inline_style_tags")
            .and_then(Value::as_bool)
            .unwrap_or(self.enable_inline_style_tags);
        self.uppercase_text = text_params_obj
            .get("uppercase_text")
            .and_then(Value::as_bool)
            .unwrap_or(self.uppercase_text);
        if let Some(shape) = text_params_obj
            .get("text_shape")
            .and_then(Value::as_str)
            .and_then(parse_text_shape_str)
        {
            self.text_shape = shape;
        }
        if let Some(wrap_mode) = text_params_obj
            .get("text_wrap_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_wrap_mode_str)
        {
            self.text_wrap_mode = wrap_mode;
        }
        if let Some(anti_aliasing) = text_params_obj
            .get("anti_aliasing")
            .and_then(Value::as_str)
            .and_then(parse_anti_aliasing_str)
        {
            self.anti_aliasing = anti_aliasing;
        }
        // Сформированный текст (если был применён «продвинутый» перенос).
        // Разворачиваем сформированный, если он есть, иначе исходный.
        self.formed_text = text_params_obj
            .get("formed_text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Текст пришёл из документа, а не из применения формы: прошлое предупреждение о
        // потерянных тегах к нему не относится.
        self.advanced_form_tags_lost = false;
        self.advanced_text_show_formed = !self.formed_text.trim().is_empty();
        self.allow_moderate_trees = text_params_obj
            .get("allow_moderate_trees")
            .and_then(Value::as_bool)
            .unwrap_or(self.allow_moderate_trees);
        self.sync_wrap_mode_constraints();
        self.shape_min_width_percent = text_params_obj
            .get("shape_min_width_percent")
            .and_then(value_as_f32)
            .unwrap_or(self.shape_min_width_percent)
            .clamp(5.0, 100.0);
        self.shape_variant = text_params_obj
            .get("shape_variant")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(self.shape_variant)
            .clamp(1, 9);

        if apply_font_selection {
            if text_params_schema::text_params_schema_version(text_params_obj)
                >= text_params_schema::TEXT_PARAMS_SCHEMA_VERSION
            {
                // Schema 2: exactly one font key, holding the identity. No legacy form is
                // consulted — accepting one here would resurrect the ambiguity the
                // identity exists to remove.
                let identity = text_params_obj
                    .get("font")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                self.select_font_by_identity(identity);
            } else {
                // Schema 1: EVERY stored name is tried, in the one read order the schema
                // owns (`font_original_name` → `font_label` → `font_family` → `font` →
                // stem of `font_path`), so a blob whose first name is a family this
                // machine no longer has still resolves through a later one. Taking only
                // the FIRST name flagged such a blob as missing although the panel could
                // resolve it. The path comes along only as the weak hint the resolver
                // refuses to select on.
                let font_path = text_params_schema::legacy_font_path(text_params_obj);
                let candidates =
                    text_params_schema::legacy_font_name_candidates(text_params_obj);
                let names: Vec<&str> = candidates.iter().map(String::as_str).collect();
                self.select_font_by_legacy_reference(font_path, &names);
            }
        }

        self.effects = render_data_obj
            .get("effects")
            .and_then(Value::as_array)
            .map(|effects| parse_effect_cards(effects, self.text_color))
            .unwrap_or_default();
        self.sync_selected_formula_preset_by_layout();
    }

    /// Selects the font a schema-2 overlay names by its IDENTITY.
    ///
    /// On a hit the selection and the profile anchor move to that font and the
    /// missing-font block is cleared. On a miss — including a payload with no `font` key
    /// at all — the identity is remembered in `missing_font`, which blocks re-rendering
    /// until the user picks an available font: an overlay must never be silently
    /// re-rendered with a substitute font.
    pub(super) fn select_font_by_identity(&mut self, identity: Option<&str>) {
        match identity.and_then(|identity| self.find_font_idx_by_identity(identity)) {
            Some(idx) => {
                self.selected_font_idx = idx;
                self.active_font_identity = self.current_font_identity();
                self.missing_font = None;
            }
            None => {
                self.missing_font = Some(
                    identity
                        .map(str::to_string)
                        .unwrap_or_else(|| t!("typing.params.font_unknown_placeholder").to_string()),
                );
            }
        }
    }

    /// Selects the font a PERSISTED overlay names, from every stored font NAME (in the
    /// schema-1 read order) plus the stored `font_path` — the legacy READ path, see
    /// [`Self::match_font_by_legacy_reference`].
    ///
    /// **A NAME selects; a PATH never does.** Only a [`LegacyFontMatch::ByName`] hit moves
    /// the selection and clears the block. When nothing but the stored path resolves, the
    /// overlay lands in `missing_font` exactly as if the font were absent, and a warning
    /// names the font that now occupies that path. The file sitting at a remembered
    /// location is not proof of identity: replacing `dialogue.ttf` with another face used
    /// to make this method select the NEW font silently, after which the next edit
    /// re-rendered the layer in a typeface the user never chose. This mirrors
    /// `codec::upgrade_text_params_to_v2` (safety rule D), which already refuses to
    /// CONVERT such a layer — the selection side was the remaining hole.
    ///
    /// On a miss the panel remembers the name it could not resolve in `missing_font`,
    /// which blocks re-rendering until the user picks an available font.
    pub(super) fn select_font_by_legacy_reference(
        &mut self,
        font_path: Option<&str>,
        font_names: &[&str],
    ) {
        let matched = self.match_font_by_legacy_reference(font_path, font_names);
        if let Some(LegacyFontMatch::ByName(idx)) = matched {
            self.selected_font_idx = idx;
            self.active_font_identity = self.current_font_identity();
            self.missing_font = None;
            return;
        }
        // Шрифт оверлея отсутствует среди доступных: запоминаем его имя, чтобы
        // показать предупреждение и заблокировать рендер до выбора другого шрифта.
        let name = font_names
            .iter()
            .find(|name| !name.trim().is_empty())
            .map(|name| (*name).to_string())
            .or_else(|| {
                font_path.map(|path| {
                    Path::new(path)
                        .file_name()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or(path)
                        .to_string()
                })
            })
            .unwrap_or_else(|| t!("typing.params.font_unknown_placeholder").to_string());
        if let Some(LegacyFontMatch::PathOnly(idx)) = matched {
            // Diagnosable, not silent: the user sees "font missing" while the file is
            // right there, so the log must say which font actually lives at that path.
            let occupant = self
                .font_identity_name_by_idx(idx)
                .unwrap_or_else(|| name.clone());
            crate::runtime_log::log_warn(format!(
                "typing: overlay font '{name}' is not installed; the file it remembered \
                 ({}) now holds '{occupant}'. The path is not proof of identity, so the \
                 overlay is reported as missing instead of being re-rendered in that font.",
                font_path.unwrap_or_default()
            ));
        }
        self.missing_font = Some(name);
    }

    pub(super) fn queue_preview_render(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let Some(params) = self.build_render_params() else {
            self.render_in_flight = false;
            self.status_line = tf!("typing.errors.fonts_not_found", arg = self.fonts_dir.display());
            return;
        };

        self.latest_token = self.latest_token.saturating_add(1);
        crate::trace_log!(
            cat::SYNC,
            "preview_render dispatch token={} layout={:?} line_mode={:?} width_px={} preempting_inflight={}",
            self.latest_token,
            params.text_layout_mode,
            params.text_line_mode,
            params.width_px,
            self.render_in_flight
        );
        let job = PreviewRenderJob {
            token: self.latest_token,
            params,
            fonts: self.font_provider(),
        };
        match self.request_tx.send(job) {
            Ok(()) => {
                self.render_in_flight = true;
                self.status_line = t!("typing.preview.rendering_status").to_string();
            }
            Err(err) => {
                crate::trace_log!(cat::SYNC, "preview_render dispatch send_err token={} err={}", self.latest_token, err);
                self.render_in_flight = false;
                self.status_line = tf!("typing.preview.render_dispatch_error", err = err);
            }
        }
    }

    pub(super) fn poll_preview_render_results(&mut self, ctx: &egui::Context) {
        if !self.preview_enabled {
            return;
        }
        let mut has_updates = false;
        while let Ok(result) = self.result_rx.try_recv() {
            if result.token != self.latest_token {
                crate::trace_log!(
                    cat::SYNC,
                    "preview_render result=stale_dropped token={} latest={}",
                    result.token,
                    self.latest_token
                );
                continue;
            }
            has_updates = true;
            self.render_in_flight = false;
            match result.image {
                Ok(image) => {
                    crate::trace_log!(
                        cat::SYNC,
                        "preview_render result=ok token={} size={}x{}",
                        result.token,
                        image.width,
                        image.height
                    );
                    self.preview_size = [image.width as usize, image.height as usize];
                    let color_image = ColorImage::from_rgba_unmultiplied(
                        self.preview_size,
                        image.rgba.as_slice(),
                    );
                    if let Some(texture) = &mut self.preview_texture {
                        texture.set(color_image, TextureOptions::LINEAR);
                    } else {
                        self.preview_texture = Some(ctx.load_texture(
                            PREVIEW_TEXTURE_ID,
                            color_image,
                            TextureOptions::LINEAR,
                        ));
                    }
                    // Replaced together with the texture: both describe this one
                    // finished render, so the diagnostic can never outlive the
                    // pixels it explains.
                    self.preview_font_fallbacks = image.font_fallbacks;
                    self.status_line = if image.warnings.is_empty() {
                        t!("typing.preview.render_done_status").to_string()
                    } else {
                        tf!("typing.preview.render_warning_status", image = image.warnings.join("; "))
                    };
                }
                Err(err) => {
                    crate::trace_log!(cat::SYNC, "preview_render result=err token={} err={}", result.token, err);
                    // No render, no diagnostic: keeping the previous one would
                    // attribute it to a text/font that was never drawn.
                    self.preview_font_fallbacks = FontFallbackReport::default();
                    self.status_line = tf!("typing.preview.render_error_status", err = err);
                }
            }
        }
        if has_updates {
            ctx.request_repaint();
        }
    }

    /// В рендер идёт сформированный текст, если он не пуст, иначе исходный.
    pub(super) fn uses_formed_text(&self) -> bool {
        !self.formed_text.trim().is_empty()
    }

    pub(super) fn effective_render_text(&self) -> String {
        if self.uses_formed_text() {
            self.formed_text.clone()
        } else {
            self.text.clone()
        }
    }

    pub(super) fn build_render_params(&self) -> Option<TextRenderParams> {
        self.build_render_params_for(self.effective_render_text(), self.width_px.max(1))
    }

    pub(super) fn adjust_font_size_by_wheel_steps(&mut self, steps: i32) -> bool {
        if steps == 0 {
            return false;
        }
        if !apply_wheel_step_f32(&mut self.font_size_px, steps, 1.0, 1.0, 256.0) {
            return false;
        }
        self.sync_current_font_profile_memory();
        self.queue_preview_render();
        true
    }

    /// Font the on-canvas create-text editor draws in: the selected font's IDENTITY plus
    /// the selected face. `None` when no font is selected. The tab resolves the identity
    /// through this panel's `FontProvider` to obtain the bytes.
    pub(super) fn editor_font_spec(&self) -> Option<TypingEditorFontSpec> {
        let font = self.fonts.get(self.selected_font_idx)?;
        let face_index = font
            .faces
            .get(self.selected_face_idx)
            .map(|face| face.face_index)
            .unwrap_or(0usize);
        Some(TypingEditorFontSpec {
            font_identity: font.render_identity_name(),
            face_index,
            ui_font_size_px: self.font_size_px.clamp(8.0, 128.0),
        })
    }

    pub(super) fn build_render_params_for(&self, text: String, width_px: u32) -> Option<TextRenderParams> {
        let font = self.fonts.get(self.selected_font_idx)?;
        let selected_face_index = font
            .faces
            .get(self.selected_face_idx)
            .map(|face| face.face_index)
            .unwrap_or(0usize);
        let formula_layout = self.formula_layout_for_render();
        let drawn_lines_layout = self.drawn_lines_layout_for_render();
        let vector_lines_layout = self.vector_lines_layout.clone();

        Some(TextRenderParams {
            text,
            text_color: [
                self.text_color.r(),
                self.text_color.g(),
                self.text_color.b(),
                self.text_color.a(),
            ],
            // The renderer resolves the main font by name through the provider
            // handed to `render_text_to_image`; inline `<font=...>` tags resolve
            // against the same provider. The name is the font's canonical render
            // IDENTITY — its original family name (fallback file-stem label) — which
            // the provider keys as its primary lookup (label/stem kept as aliases).
            font_name: font.render_identity_name(),
            font_size_px: self.font_size_px.max(1.0),
            line_spacing_px: self.line_spacing.as_px_percent().0,
            line_spacing_percent: self.line_spacing.as_px_percent().1,
            kerning_mode: self.kerning_mode,
            kerning_px: self.kerning.as_px_percent().0,
            kerning_percent: self.kerning.as_px_percent().1,
            glyph_height_percent: self.glyph_height.as_percent_of(self.font_size_px.max(1.0)),
            glyph_width_percent: self.glyph_width.as_percent_of(self.font_size_px.max(1.0)),
            width_px: width_px.max(1),
            align: self.align,
            global_rotation_deg: self.global_rotation_deg,
            line_placement_percent: self.line_placement_percent,
            line_placement_reference: self.line_placement_reference,
            // Decode the carried canvas-authored warp (Phase 3) for the live
            // preview render; absent/invalid -> None (identity / no warp).
            raster_transform: self
                .pending_raster_transform
                .as_ref()
                .and_then(decode_vector_mesh_warp),
            text_line_mode: self.text_line_mode,
            vertical_line_direction: self.vertical_line_direction,
            text_layout_mode: self.text_layout_mode,
            formula_layout,
            drawn_lines_layout,
            vector_lines_layout,
            selected_face_index,
            force_bold: self.force_bold,
            force_italic: self.force_italic,
            faux_bold: (self.force_bold && self.faux_bold).then_some(FauxBoldParams {
                thicken_percent: self.faux_bold_thicken_percent,
                expand_percent: self.faux_bold_expand_percent,
                sharp_corners: self.faux_bold_sharp_corners,
                outward_only: self.faux_bold_outward_only,
            }),
            faux_italic_slant_deg: (self.force_italic && self.faux_italic)
                .then_some(self.faux_italic_slant_deg),
            uppercase_text: self.uppercase_text,
            trim_extra_spaces: self.trim_extra_spaces,
            replace_ellipsis_with_dots: self.replace_ellipsis_with_dots,
            // Sub-parameter: the GSUB patch is a no-op without the substitution that
            // produces the three dots, so it is handed to the renderer only when BOTH
            // flags are on. Keeping the AND here means a stored `true` under a disabled
            // parent never leaks into a render.
            force_remove_ellipsis_glyph: self.replace_ellipsis_with_dots
                && self.force_remove_ellipsis_glyph,
            hanging_punctuation: self.hanging_punctuation,
            new_line_after_sentence: self.new_line_after_sentence,
            enable_inline_style_tags: self.enable_inline_style_tags,
            // Сформированный текст уже разбит на строки — не переносим заново.
            text_wrap_mode: if self.uses_formed_text() {
                TextWrapMode::None
            } else {
                self.text_wrap_mode
            },
            text_shape: self.text_shape,
            shape_min_width_percent: self.shape_min_width_percent,
            shape_variant: self.shape_variant,
            compare_shape_with: None,
            allow_moderate_trees: self.allow_moderate_trees,
            effects_json: self.effects_json(),
            anti_aliasing: self.anti_aliasing,
            // Live preview never requests extra render info (mean/median centers).
            extra_info: crate::tabs::typing::render_next::types::RenderExtraInfoRequest::default(),
        })
    }

    pub(super) fn sync_wrap_mode_constraints(&mut self) {
        if !self.moderate_trees_checkbox_enabled() {
            self.allow_moderate_trees = false;
        }
    }

    pub(super) fn moderate_trees_checkbox_enabled(&self) -> bool {
        matches!(
            self.text_wrap_mode,
            TextWrapMode::WholeWords | TextWrapMode::Minimal
        )
    }
}
