/*
File: tabs/typing/panel/create_edit.rs

Purpose:
Part of the `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
the edit-mode parameters section and the inline text-selection / inline-tag
styling logic.

Main responsibilities:
- draw the edit-mode text parameters section for a selected overlay;
- sync and repaint the persistent inline text selection from the text editor;
- track the active inline text and its selection context;
- resolve the effective inline-tag style and apply an inline style to the
  current selection, normalizing the desired style against the base params.

Notes:
Methods are `pub(super)` so the module root `panel.rs` and sibling submodules
can call them. `use super::*;` pulls in the parent module types and imports.
*/

use super::*;

impl TypingCreatePanelState {

    pub(super) fn draw_edit_params_section(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
    ) -> bool {
        let mut changed = self.draw_advanced_form_window(ui.ctx());
        let mut block_hscroll_by_hovered_param = false;

        if stacked_columns {
            let font_missing = self.missing_font.is_some();
            ui.vertical(|ui| {
                if let Some(missing) = self.missing_font.clone() {
                    ui.colored_label(
                        Color32::from_rgb(240, 110, 110),
                        tf!("typing.edit.font_missing_warning", missing = missing),
                    );
                    ui.add_space(4.0);
                }
                ui.add_enabled_ui(!font_missing, |ui| {
                    changed |= self.draw_text_accordion(
                        ui,
                        "stacked",
                        &mut block_hscroll_by_hovered_param,
                    );
                });
                ui.add_space(6.0);

                let selection_mode = self.inline_selection_context().is_some();
                // «Слой» (layer transform): collapsible section (default open) for
                // the overlay width / scale / rotation. Gating condition unchanged.
                let preview_enabled = self.preview_enabled;
                let layer_summary = format!(
                    "{}px · ×{:.2} · {}°",
                    self.width_px,
                    self.overlay_scale,
                    self.overlay_rotation_deg.round() as i32
                );
                collapsing_param_section(
                    ui,
                    "typing.section.layer",
                    preview_enabled,
                    t!("typing.section.layer"),
                    true,
                    Some(layer_summary.as_str()),
                    |ui| {
                ui.add_enabled_ui(!selection_mode && !font_missing, |ui| {
                    let width_resp = ui
                        .add(WheelSlider::new(&mut self.width_px, 16..=4096).text(t!("typing.params.width_px_label")));
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &width_resp);
                    changed |= width_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &width_resp) {
                        changed |= apply_wheel_step_u32(&mut self.width_px, steps, 10, 16, 4096);
                    }

                    let scale_resp = ui.add(
                        WheelSlider::new(&mut self.overlay_scale, 0.05..=20.0).text(t!("typing.params.scale_label")),
                    );
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &scale_resp);
                    changed |= scale_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &scale_resp) {
                        changed |= apply_wheel_step_f32(
                            &mut self.overlay_scale,
                            steps,
                            0.05,
                            0.05,
                            20.0,
                        );
                    }

                    let angle_resp = ui.add(
                        WheelSlider::new(&mut self.overlay_rotation_deg, -180.0..=180.0)
                            .text(t!("typing.params.angle_label")),
                    );
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &angle_resp);
                    changed |= angle_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &angle_resp) {
                        changed |= apply_wheel_step_f32(
                            &mut self.overlay_rotation_deg,
                            steps,
                            1.0,
                            -180.0,
                            180.0,
                        );
                    }
                });
                    },
                );

                ui.separator();
                changed |= self.draw_main_text_params(
                    ui,
                    true,
                    remap_wheel_to_horizontal,
                    false,
                    font_missing,
                );
                if selection_mode {
                    ui.add_space(4.0);
                    ui.small(
                        t!("typing.edit.inline_tags_hint"),
                    );
                }
            });
            if remap_wheel_to_horizontal {
                apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
            } else if block_hscroll_by_hovered_param {
                consume_wheel_scroll_delta(ui);
            }
            if changed {
                self.queue_preview_render();
            }
            return changed;
        }

        let inline_selection = self.inline_selection_context();
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            let spacing_x = ui.spacing().item_spacing.x;
            let available_w = ui.available_width().max(1.0);
            let columns_w = (available_w - spacing_x).max(1.0);
            let left_ratio = 0.34;
            let min_left_w = 170.0;
            let min_right_w = 300.0;
            let mut left_w = columns_w * left_ratio;
            let mut right_w = columns_w - left_w;
            if columns_w >= (min_left_w + min_right_w) {
                if left_w < min_left_w {
                    left_w = min_left_w;
                    right_w = columns_w - left_w;
                }
                if right_w < min_right_w {
                    right_w = min_right_w;
                    left_w = columns_w - right_w;
                }
            }

            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(left_w, 0.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        changed |= self.draw_text_accordion(
                            ui,
                            "columns",
                            &mut block_hscroll_by_hovered_param,
                        );
                    },
                );

                ui.allocate_ui_with_layout(
                    Vec2::new(right_w, 0.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        ui.horizontal_top(|ui| {
                            let inner_spacing_x = ui.spacing().item_spacing.x;
                            let inner_available_w = ui.available_width().max(1.0);
                            let mut right_col_w = (inner_available_w * 0.28).max(165.0);
                            let mut left_cluster_w =
                                (inner_available_w - inner_spacing_x - right_col_w).max(1.0);
                            if inner_available_w >= 480.0 && left_cluster_w < 280.0 {
                                left_cluster_w = 280.0;
                                right_col_w =
                                    (inner_available_w - inner_spacing_x - left_cluster_w).max(1.0);
                            }

                            ui.allocate_ui_with_layout(
                                Vec2::new(left_cluster_w, 0.0),
                                egui::Layout::top_down(Align::Min),
                                |ui| {
                                    ui.group(|ui| {
                                        ui.set_width(ui.available_width());
                                        ui.set_min_width(ui.available_width());
                                        ui.set_max_width(ui.available_width());
                                        ui.label(egui::RichText::new(t!("typing.params.font_label")).strong());
                                        ui.horizontal(|ui| {
                                            let prev_font_idx = self.selected_font_idx;
                                            let selected_font_text = inline_style
                                                .as_ref()
                                                .and_then(|style| style.font_label.as_deref())
                                                .or_else(|| {
                                                    self.fonts
                                                        .get(self.selected_font_idx)
                                                        .map(|font| font.label.as_str())
                                                })
                                                .unwrap_or(t!("typing.params.font_placeholder"));
                                            let mut font_idx = inline_style
                                                .as_ref()
                                                .and_then(|style| {
                                                    self.find_font_idx_by_path_or_label(
                                                        None,
                                                        style.font_label.as_deref(),
                                                    )
                                                })
                                                .unwrap_or(self.selected_font_idx);
                                            let font_count = self.fonts.len();
                                            let font_combo = WheelComboBox::from_label(t!("typing.edit.font_combo_id")).id_salt("typing.edit.font_combo_id")
                                                .selected_text(selected_font_text)
                                                .show_ui_with_wheel(ui, |ui| {
                                                    for idx in 0..self.fonts.len() {
                                                        let (label, path, face_index, coverage) = {
                                                            let font = &self.fonts[idx];
                                                            (
                                                                font.label.clone(),
                                                                font.path.clone(),
                                                                font.faces
                                                                    .first()
                                                                    .map(|face| face.face_index)
                                                                    .unwrap_or(0),
                                                                font.coverage.clone(),
                                                            )
                                                        };
                                                        let selected = font_idx == idx;
                                                        if self.draw_font_combo_option(
                                                            ui,
                                                            &label,
                                                            path.as_path(),
                                                            face_index,
                                                            selected,
                                                            &coverage,
                                                        ) {
                                                            font_idx = idx;
                                                        }
                                                    }
                                                });
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &font_combo.inner.response,
                                            );
                                            if let Some(steps) = font_combo.wheel_steps {
                                                cycle_wrapped_index(&mut font_idx, font_count, steps);
                                            }
                                            if let Some(style) = inline_style.as_mut() {
                                                if let Some(label) = self.font_label_by_idx(font_idx) {
                                                    style.font_label = Some(label);
                                                }
                                            } else {
                                                self.selected_font_idx = font_idx;
                                                if self.selected_font_idx != prev_font_idx {
                                                    self.selected_face_idx = 0;
                                                    changed = true;
                                                }
                                            }

                                            ui.add_enabled_ui(!selection_mode, |ui| {
                                                let prev_face_idx = self.selected_face_idx;
                                                let selected_face_text = self
                                                    .fonts
                                                    .get(self.selected_font_idx)
                                                    .and_then(|font| {
                                                        font.faces.get(self.selected_face_idx)
                                                    })
                                                    .map(|face| face.label.as_str())
                                                    .unwrap_or("<face>");
                                                let face_count = self
                                                    .fonts
                                                    .get(self.selected_font_idx)
                                                    .map(|font| font.faces.len())
                                                    .unwrap_or(0);
                                                let mut face_idx = self.selected_face_idx;
                                                let face_combo = WheelComboBox::from_label("Face")
                                                    .selected_text(selected_face_text)
                                                    .show_ui_with_wheel(ui, |ui| {
                                                        if let Some(font) =
                                                            self.fonts.get(self.selected_font_idx)
                                                        {
                                                            for (idx, face) in
                                                                font.faces.iter().enumerate()
                                                            {
                                                                ui.selectable_value(
                                                                    &mut face_idx,
                                                                    idx,
                                                                    &face.label,
                                                                );
                                                            }
                                                        }
                                                    });
                                                mark_hscroll_block_on_hover(
                                                    &mut block_hscroll_by_hovered_param,
                                                    &face_combo.inner.response,
                                                );
                                                if let Some(steps) = face_combo.wheel_steps {
                                                    cycle_wrapped_index(
                                                        &mut face_idx,
                                                        face_count,
                                                        steps,
                                                    );
                                                }
                                                self.selected_face_idx = face_idx;
                                                if self.selected_face_idx != prev_face_idx {
                                                    changed = true;
                                                }
                                            });
                                        });
                                    });

                                    ui.add_space(4.0);

                                    let mid_available_w = ui.available_width().max(1.0);
                                    let mut mid_col_w = (mid_available_w - inner_spacing_x) / 2.0;
                                    if mid_col_w <= 0.0 {
                                        mid_col_w = 1.0;
                                    }

                                    ui.horizontal_top(|ui| {
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(mid_col_w, 0.0),
                                            egui::Layout::top_down(Align::Min),
                                            |ui| {
                                                ui.add_enabled_ui(!selection_mode, |ui| {
                                                    let width_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.width_px,
                                                            16..=4096,
                                                        )
                                                        .text(t!("typing.params.width_px_label")),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &width_resp,
                                                    );
                                                    changed |= width_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &width_resp)
                                                    {
                                                        changed |= apply_wheel_step_u32(
                                                            &mut self.width_px,
                                                            steps,
                                                            10,
                                                            16,
                                                            4096,
                                                        );
                                                    }

                                                    let scale_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.overlay_scale,
                                                            0.05..=20.0,
                                                        )
                                                        .text(t!("typing.params.scale_label")),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &scale_resp,
                                                    );
                                                    changed |= scale_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &scale_resp)
                                                    {
                                                        changed |= apply_wheel_step_f32(
                                                            &mut self.overlay_scale,
                                                            steps,
                                                            0.05,
                                                            0.05,
                                                            20.0,
                                                        );
                                                    }

                                                    let angle_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.overlay_rotation_deg,
                                                            -180.0..=180.0,
                                                        )
                                                        .text(t!("typing.params.angle_label")),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &angle_resp,
                                                    );
                                                    changed |= angle_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &angle_resp)
                                                    {
                                                        changed |= apply_wheel_step_f32(
                                                            &mut self.overlay_rotation_deg,
                                                            steps,
                                                            1.0,
                                                            -180.0,
                                                            180.0,
                                                        );
                                                    }
                                                });
                                            },
                                        );

                                        ui.allocate_ui_with_layout(
                                            Vec2::new(mid_col_w, 0.0),
                                            egui::Layout::top_down(Align::Min),
                                            |ui| {
                                                let color_resp = self
                                                    .text_color_selector
                                                    .draw(ui, &mut self.text_color);
                                                changed |= color_resp.changed;
                                                if let Some(style) = inline_style.as_mut() {
                                                    let mut font_size_px = style
                                                        .font_size_px
                                                        .unwrap_or(self.font_size_px)
                                                        .clamp(1.0, 256.0);
                                                    let font_size_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut font_size_px,
                                                            1.0..=256.0,
                                                        )
                                                        .text(t!("typing.params.size_px_label"))
                                                        .wheel_step(1.0),
                                                    );
                                                    changed |= font_size_resp.changed();
                                                    style.font_size_px = Some(font_size_px);
                                                } else {
                                                    let font_size_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.font_size_px,
                                                            1.0..=256.0,
                                                        )
                                                        .text(t!("typing.params.size_px_label"))
                                                        .wheel_step(1.0),
                                                    );
                                                    changed |= font_size_resp.changed();
                                                }

                                                let base_font_size_px = self.font_size_px.max(1.0);
                                                if let Some(style) = inline_style.as_mut() {
                                                    let inline_font_size_px = style
                                                        .font_size_px
                                                        .unwrap_or(base_font_size_px)
                                                        .max(1.0);
                                                    let mut line_spacing = style
                                                        .line_spacing
                                                        .unwrap_or(self.line_spacing);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.line_spacing_label"),
                                                        &mut line_spacing,
                                                        PxOrPercentRowCfg {
                                                            range: -300.0..=300.0,
                                                            wheel_step: 2.0,
                                                            font_size_px: inline_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.line_spacing = Some(line_spacing);

                                                    ui.horizontal(|ui| {
                                                        ui.label(t!("typing.params.kerning_label"));
                                                        // Read-only global kerning-mode indicator; Optical not offered.
                                                        ui.add_enabled(
                                                            false,
                                                            egui::Button::new(t!("typing.params.kerning_metric"))
                                                                .selected(self.kerning_mode == KerningMode::Fixed),
                                                        );
                                                        ui.add_enabled(
                                                            false,
                                                            egui::Button::new(t!("typing.params.kerning_auto"))
                                                                .selected(self.kerning_mode == KerningMode::Auto),
                                                        );
                                                    });

                                                    let mut kerning = style
                                                        .kerning
                                                        .unwrap_or(self.kerning);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.kerning_label"),
                                                        &mut kerning,
                                                        PxOrPercentRowCfg {
                                                            range: -300.0..=300.0,
                                                            wheel_step: 2.0,
                                                            font_size_px: inline_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.kerning = Some(kerning);

                                                    let mut stretching = style
                                                        .glyph_stretching
                                                        .unwrap_or([self.glyph_width, self.glyph_height]);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.char_height_label"),
                                                        &mut stretching[1],
                                                        PxOrPercentRowCfg {
                                                            range: 1.0..=300.0,
                                                            wheel_step: 5.0,
                                                            font_size_px: inline_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.char_width_label"),
                                                        &mut stretching[0],
                                                        PxOrPercentRowCfg {
                                                            range: 1.0..=300.0,
                                                            wheel_step: 5.0,
                                                            font_size_px: inline_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.glyph_stretching = Some(stretching);
                                                    self.draw_inline_offset_controls(
                                                        ui,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                        Some(style),
                                                    );
                                                } else {
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.line_spacing_label"),
                                                        &mut self.line_spacing,
                                                        PxOrPercentRowCfg {
                                                            range: -300.0..=300.0,
                                                            wheel_step: 2.0,
                                                            font_size_px: base_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    ui.horizontal(|ui| {
                                                        ui.label(t!("typing.params.kerning_label"));
                                                        // Optical is implemented but not offered here; only Fixed/Auto are user-selectable.
                                                        changed |= ui.selectable_value(&mut self.kerning_mode, KerningMode::Fixed, t!("typing.params.kerning_metric")).changed();
                                                        changed |= ui.selectable_value(&mut self.kerning_mode, KerningMode::Auto, t!("typing.params.kerning_auto")).changed();
                                                    });
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.kerning_label"),
                                                        &mut self.kerning,
                                                        PxOrPercentRowCfg {
                                                            range: -300.0..=300.0,
                                                            wheel_step: 2.0,
                                                            font_size_px: base_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.char_height_label"),
                                                        &mut self.glyph_height,
                                                        PxOrPercentRowCfg {
                                                            range: 1.0..=300.0,
                                                            wheel_step: 5.0,
                                                            font_size_px: base_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        t!("typing.params.char_width_label"),
                                                        &mut self.glyph_width,
                                                        PxOrPercentRowCfg {
                                                            range: 1.0..=300.0,
                                                            wheel_step: 5.0,
                                                            font_size_px: base_font_size_px,
                                                        },
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                }
                                            },
                                        );
                                    });
                                },
                            );

                            ui.allocate_ui_with_layout(
                                Vec2::new(right_col_w, 0.0),
                                egui::Layout::top_down(Align::Min),
                                |ui| {
                                        if let Some(style) = inline_style.as_mut() {
                                            let mut align = style.align.unwrap_or(self.align);
                                            Self::draw_alignment_controls(
                                                ui,
                                                &mut align,
                                                &mut changed,
                                                &mut block_hscroll_by_hovered_param,
                                            );
                                            style.align = Some(align);
                                        } else {
                                            Self::draw_alignment_controls(
                                                ui,
                                                &mut self.align,
                                                &mut changed,
                                                &mut block_hscroll_by_hovered_param,
                                            );
                                        }

                                        let prev_shape = self.text_shape;
                                        let shape_combo = WheelComboBox::from_label(t!("typing.edit.shape_combo_id")).id_salt("typing.edit.shape_combo_id")
                                            .selected_text(match self.text_shape {
                                                TextShape::Free => t!("typing.params.shape_free_option"),
                                                TextShape::Rectangle => "[  ]",
                                                TextShape::Oval => "(  )",
                                                TextShape::Hexagon => "<  >",
                                                TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
                                            })
                                            .show_ui_with_wheel(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Free,
                                                    t!("typing.params.shape_free_option"),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Rectangle,
                                                    "[  ]",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Oval,
                                                    "(  )",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Hexagon,
                                                    "<  >",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::SoftPeak,
                                                    t!("typing.params.shape_soft_option"),
                                                );
                                            });
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &shape_combo.inner.response,
                                        );
                                        if let Some(steps) = shape_combo.wheel_steps {
                                            changed |=
                                                cycle_text_shape(&mut self.text_shape, steps);
                                        }
                                        if self.text_shape != prev_shape {
                                            changed = true;
                                        }

                                        let prev_wrap_mode = self.text_wrap_mode;
                                        let wrap_combo = WheelComboBox::from_label(t!("typing.edit.wrap_combo_id")).id_salt("typing.edit.wrap_combo_id")
                                            .selected_text(text_wrap_mode_label(
                                                self.text_wrap_mode,
                                            ))
                                            .show_ui_with_wheel(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::None,
                                                    text_wrap_mode_label(TextWrapMode::None),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::WholeWords,
                                                    text_wrap_mode_label(TextWrapMode::WholeWords),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Minimal,
                                                    text_wrap_mode_label(TextWrapMode::Minimal),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Moderate,
                                                    text_wrap_mode_label(TextWrapMode::Moderate),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Aggressive,
                                                    text_wrap_mode_label(TextWrapMode::Aggressive),
                                                );
                                            });
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &wrap_combo.inner.response,
                                        );
                                        if let Some(steps) = wrap_combo.wheel_steps {
                                            changed |=
                                                cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
                                        }
                                        if self.text_wrap_mode != prev_wrap_mode {
                                            self.sync_wrap_mode_constraints();
                                            changed = true;
                                        }

                                        let prev_anti_aliasing = self.anti_aliasing;
                                        let aa_combo = WheelComboBox::from_label(t!("typing.edit.antialias_combo_id")).id_salt("typing.edit.antialias_combo_id")
                                            .selected_text(anti_aliasing_label(self.anti_aliasing))
                                            .show_ui_with_wheel(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.anti_aliasing,
                                                    AntiAliasingMode::None,
                                                    anti_aliasing_label(AntiAliasingMode::None),
                                                );
                                                ui.selectable_value(
                                                    &mut self.anti_aliasing,
                                                    AntiAliasingMode::Sharp,
                                                    anti_aliasing_label(AntiAliasingMode::Sharp),
                                                );
                                                ui.selectable_value(
                                                    &mut self.anti_aliasing,
                                                    AntiAliasingMode::Crisp,
                                                    anti_aliasing_label(AntiAliasingMode::Crisp),
                                                );
                                                ui.selectable_value(
                                                    &mut self.anti_aliasing,
                                                    AntiAliasingMode::Strong,
                                                    anti_aliasing_label(AntiAliasingMode::Strong),
                                                );
                                                ui.selectable_value(
                                                    &mut self.anti_aliasing,
                                                    AntiAliasingMode::Smooth,
                                                    anti_aliasing_label(AntiAliasingMode::Smooth),
                                                );
                                            });
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &aa_combo.inner.response,
                                        );
                                        if let Some(steps) = aa_combo.wheel_steps {
                                            changed |= cycle_anti_aliasing(
                                                &mut self.anti_aliasing,
                                                steps,
                                            );
                                        }
                                        if self.anti_aliasing != prev_anti_aliasing {
                                            changed = true;
                                        }
                                        let moderate_trees_resp = ui.add_enabled(
                                            self.moderate_trees_checkbox_enabled(),
                                            egui::Checkbox::new(
                                                &mut self.allow_moderate_trees,
                                                t!("typing.params.allow_moderate_herringbone"),
                                            ),
                                        );
                                        changed |= moderate_trees_resp.changed();

                                        if matches!(
                                            self.text_shape,
                                            TextShape::Oval | TextShape::Hexagon
                                        ) {
                                            let min_width_resp = ui.add(
                                                WheelSlider::new(
                                                    &mut self.shape_min_width_percent,
                                                    5.0..=100.0,
                                                )
                                                .text(t!("typing.params.min_width_percent_label")),
                                            );
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &min_width_resp,
                                            );
                                            changed |= min_width_resp.changed();
                                            if let Some(steps) =
                                                wheel_steps_if_hovered(ui, &min_width_resp)
                                            {
                                                changed |= apply_wheel_step_f32(
                                                    &mut self.shape_min_width_percent,
                                                    steps,
                                                    1.0,
                                                    5.0,
                                                    100.0,
                                                );
                                            }
                                        }
                                        if self.text_shape == TextShape::SoftPeak {
                                            let variant_resp = ui.add(
                                                WheelSlider::new(&mut self.shape_variant, 1..=9)
                                                    .text(t!("typing.params.shape_variant_label")),
                                            );
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &variant_resp,
                                            );
                                            changed |= variant_resp.changed();
                                            if let Some(steps) =
                                                wheel_steps_if_hovered(ui, &variant_resp)
                                            {
                                                changed |= apply_wheel_step_u8(
                                                    &mut self.shape_variant,
                                                    steps,
                                                    1,
                                                    1,
                                                    9,
                                                );
                                            }
                                        }
                                        if let Some(style) = inline_style.as_mut() {
                                            let mut bold = style.bold;
                                            let mut italic = style.italic;
                                            let faux = style.faux_bold.unwrap_or_default();
                                            let mut thicken = faux.thicken_percent;
                                            let mut expand = faux.expand_percent;
                                            let mut sharp = faux.sharp_corners;
                                            let mut outward = faux.outward_only;
                                            let mut faux_bold = style.faux_bold.is_some();
                                            let mut slant = style.faux_italic_slant.unwrap_or(14.0);
                                            let mut faux_italic = style.faux_italic_slant.is_some();
                                            draw_faux_style_controls(
                                                ui,
                                                &mut bold,
                                                &mut italic,
                                                FauxStyleControlValues {
                                                    faux_bold: &mut faux_bold,
                                                    faux_bold_thicken_percent: &mut thicken,
                                                    faux_bold_expand_percent: &mut expand,
                                                    faux_bold_sharp_corners: &mut sharp,
                                                    faux_bold_outward_only: &mut outward,
                                                    faux_italic: &mut faux_italic,
                                                    faux_italic_slant_deg: &mut slant,
                                                },
                                                &mut changed,
                                                &mut block_hscroll_by_hovered_param,
                                                "typing_edit_inline_faux",
                                            );
                                            style.bold = bold;
                                            style.italic = italic;
                                            style.faux_bold = (bold && faux_bold).then_some(FauxBoldParams {
                                                thicken_percent: thicken,
                                                expand_percent: expand,
                                                sharp_corners: sharp,
                                                outward_only: outward,
                                            });
                                            style.faux_italic_slant = (italic && faux_italic).then_some(slant);

                                            let mut no_break = style.no_break;
                                            let no_break_resp =
                                                ui.checkbox(&mut no_break, t!("typing.params.no_break"));
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &no_break_resp,
                                            );
                                            changed |= no_break_resp.changed();
                                            style.no_break = no_break;
                                        } else {
                                            draw_faux_style_controls(
                                                ui,
                                                &mut self.force_bold,
                                                &mut self.force_italic,
                                                FauxStyleControlValues {
                                                    faux_bold: &mut self.faux_bold,
                                                    faux_bold_thicken_percent: &mut self.faux_bold_thicken_percent,
                                                    faux_bold_expand_percent: &mut self.faux_bold_expand_percent,
                                                    faux_bold_sharp_corners: &mut self.faux_bold_sharp_corners,
                                                    faux_bold_outward_only: &mut self.faux_bold_outward_only,
                                                    faux_italic: &mut self.faux_italic,
                                                    faux_italic_slant_deg: &mut self.faux_italic_slant_deg,
                                                },
                                                &mut changed,
                                                &mut block_hscroll_by_hovered_param,
                                                "typing_edit_faux",
                                            );
                                        }
                                        let hanging_punct_resp = ui.checkbox(
                                            &mut self.hanging_punctuation,
                                            t!("typing.params.hanging_punctuation"),
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &hanging_punct_resp,
                                        );
                                        changed |= hanging_punct_resp.changed();
                                        let trim_spaces_resp = ui.checkbox(
                                            &mut self.trim_extra_spaces,
                                            t!("typing.params.strip_extra_spaces"),
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &trim_spaces_resp,
                                        );
                                        changed |= trim_spaces_resp.changed();
                                        let sentence_nl_resp = ui.checkbox(
                                            &mut self.new_line_after_sentence,
                                            t!("typing.params.newline_after_sentence"),
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &sentence_nl_resp,
                                        );
                                        changed |= sentence_nl_resp.changed();
                                        let uppercase_text_resp = ui.checkbox(
                                            &mut self.uppercase_text,
                                            t!("typing.params.all_uppercase"),
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &uppercase_text_resp,
                                        );
                                        changed |= uppercase_text_resp.changed();
                                        let inline_tags_resp = ui.checkbox(
                                            &mut self.enable_inline_style_tags,
                                            t!("typing.params.parse_bi_tags"),
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &inline_tags_resp,
                                        );
                                        changed |= inline_tags_resp.changed();

                                        self.draw_advanced_text_params_section(
                                            ui,
                                            &mut changed,
                                            &mut block_hscroll_by_hovered_param,
                                            "typing_advanced_text_params_edit_columns",
                                        );
                                },
                            );
                        });
                    },
                );
            });

            if selection_mode {
                ui.add_space(4.0);
                ui.small(
                    t!("typing.edit.inline_tags_hint_with_color"),
                );
            }

            // Extra bottom padding so the horizontal scrollbar doesn't overlap the last checkbox text.
            ui.add_space(ui.spacing().scroll.allocated_width() + 4.0);
        });
        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        if let (Some(selection), Some(style)) = (inline_selection, inline_style) {
            changed |= self.apply_inline_style_to_selection(selection, style);
        }
        if changed {
            self.queue_preview_render();
        }
        changed
    }

    pub(super) fn sync_text_selection_from_text_edit(
        &mut self,
        ctx: &egui::Context,
        text_edit_id: Id,
        response: &egui::Response,
        cursor_range: Option<CCursorRange>,
    ) {
        if let Some(range) = self.pending_text_selection_restore.take() {
            let clamped = clamp_char_range(self.active_inline_text(), range);
            let mut state = egui::TextEdit::load_state(ctx, text_edit_id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(clamped.start),
                CCursor::new(clamped.end),
            )));
            state.store(ctx, text_edit_id);
            self.text_selection_char_range = Some(clamped);
            return;
        }

        // egui 0.35 returns a `Range<CharIndex>` from `as_sorted_char_range`; the stored
        // selection range is plain `usize` char offsets, so unwrap the `CharIndex` newtype.
        if let Some(range) = cursor_range
            .map(|range| range.as_sorted_char_range())
            .map(|range| range.start.0..range.end.0)
        {
            if range.start < range.end {
                self.text_selection_char_range = Some(range);
            } else if response.clicked() || response.dragged() {
                self.text_selection_char_range = None;
            }
        }
    }

    pub(super) fn paint_persistent_text_selection_if_needed(
        &self,
        ui: &egui::Ui,
        text_output: &egui::text_edit::TextEditOutput,
    ) {
        if text_output.response.has_focus() {
            return;
        }

        let Some(char_range) = self.text_selection_char_range.as_ref() else {
            return;
        };
        if char_range.start >= char_range.end {
            return;
        }

        let clamped = clamp_char_range(self.active_inline_text(), char_range.clone());
        if clamped.start >= clamped.end {
            return;
        }

        let mut galley = text_output.galley.clone();
        paint_text_selection(
            &mut galley,
            ui.visuals(),
            &CCursorRange::two(CCursor::new(clamped.start), CCursor::new(clamped.end)),
            None,
        );

        ui.painter()
            .with_clip_rect(text_output.text_clip_rect)
            .galley(text_output.galley_pos, galley, ui.visuals().text_color());
    }

    /// Активный буфер для выделения и инлайн-тегов (исходный/сформированный).
    pub(super) fn active_inline_text(&self) -> &str {
        match self.inline_text_target {
            InlineTextTarget::Source => &self.text,
            InlineTextTarget::Formed => &self.formed_text,
        }
    }

    pub(super) fn set_active_inline_text(&mut self, value: String) {
        match self.inline_text_target {
            InlineTextTarget::Source => self.text = value,
            InlineTextTarget::Formed => self.formed_text = value,
        }
    }

    /// Сбрасывает сохранённое инлайн-выделение текста. Вызывается при
    /// переключении панов аккордеона и при смене редактируемого слоя, чтобы
    /// выделение оставалось привязанным к одному оверлею.
    pub(super) fn clear_inline_text_selection(&mut self) {
        self.text_selection_char_range = None;
        self.pending_text_selection_restore = None;
    }

    pub(super) fn inline_selection_context(&self) -> Option<TypingInlineSelectionContext> {
        let char_range = self.text_selection_char_range.as_ref()?.clone();
        if char_range.start >= char_range.end {
            return None;
        }
        let text = self.active_inline_text();
        let text_byte_range = char_range_to_byte_range(text, &char_range)?;
        if text_byte_range.start >= text_byte_range.end {
            return None;
        }

        let opening_tags = collect_adjacent_opening_inline_tags(text, text_byte_range.start);
        let closing_tags = collect_adjacent_closing_inline_tags(text, text_byte_range.end);
        let matched_count = opening_tags
            .iter()
            .zip(closing_tags.iter())
            .take_while(|(open_tag, close_tag)| {
                inline_tag_kinds_match(&open_tag.kind, &close_tag.kind)
            })
            .count();

        let opening_wrapper_range = if matched_count > 0 {
            let start = opening_tags
                .get(matched_count.saturating_sub(1))
                .map(|tag| tag.byte_range.start)
                .unwrap_or(text_byte_range.start);
            start..text_byte_range.start
        } else {
            text_byte_range.start..text_byte_range.start
        };
        let closing_wrapper_range = if matched_count > 0 {
            let end = closing_tags
                .get(matched_count.saturating_sub(1))
                .map(|tag| tag.byte_range.end)
                .unwrap_or(text_byte_range.end);
            text_byte_range.end..end
        } else {
            text_byte_range.end..text_byte_range.end
        };

        let mut style = TypingInlineTagStyle::default();
        for tag in opening_tags.iter().take(matched_count) {
            match &tag.kind {
                TypingInlineTagKind::Bold => style.bold = true,
                TypingInlineTagKind::Italic => style.italic = true,
                TypingInlineTagKind::FauxBold(params) => { style.bold = true; style.faux_bold = Some(*params); }
                TypingInlineTagKind::FauxItalic(slant) => { style.italic = true; style.faux_italic_slant = Some(*slant); }
                TypingInlineTagKind::NoBreak => style.no_break = true,
                TypingInlineTagKind::Align(align) => style.align = Some(*align),
                TypingInlineTagKind::Font(label) => style.font_label = Some(label.clone()),
                TypingInlineTagKind::Size(size_px) => style.font_size_px = Some(*size_px),
                TypingInlineTagKind::Color(color) => style.text_color = Some(*color),
                TypingInlineTagKind::LineSpacing(value) => style.line_spacing = Some(*value),
                TypingInlineTagKind::Kerning(value) => style.kerning = Some(*value),
                TypingInlineTagKind::Stretching(value) => style.glyph_stretching = Some(*value),
                TypingInlineTagKind::Offset(offset) => style.glyph_offset = Some(*offset),
                TypingInlineTagKind::Machine(machine) => {
                    if machine.bold {
                        style.bold = true;
                    }
                    if machine.faux_bold.is_some() { style.faux_bold = machine.faux_bold; }
                    if machine.italic {
                        style.italic = true;
                    }
                    if machine.faux_italic_slant.is_some() { style.faux_italic_slant = machine.faux_italic_slant; }
                    if machine.no_break {
                        style.no_break = true;
                    }
                    if machine.align.is_some() {
                        style.align = machine.align;
                    }
                    if machine.font_label.is_some() {
                        style.font_label = machine.font_label.clone();
                    }
                    if machine.font_size_px.is_some() {
                        style.font_size_px = machine.font_size_px;
                    }
                    if machine.text_color.is_some() {
                        style.text_color = machine.text_color;
                    }
                    if machine.line_spacing.is_some() {
                        style.line_spacing = machine.line_spacing;
                    }
                    if machine.kerning.is_some() {
                        style.kerning = machine.kerning;
                    }
                    if machine.glyph_stretching.is_some() {
                        style.glyph_stretching = machine.glyph_stretching;
                    }
                    if machine.glyph_offset.is_some() {
                        style.glyph_offset = machine.glyph_offset;
                    }
                }
            }
        }

        Some(TypingInlineSelectionContext {
            char_range,
            text_byte_range,
            opening_wrapper_range,
            closing_wrapper_range,
            style,
        })
    }

    /// The whole-overlay effective faux-bold params, as the renderer sees them:
    /// `Some` only when the overlay bold style is forced AND faux is enabled;
    /// `None` means the overlay is off or requests the real Bold face. This is the
    /// global fallback a spanless offset resolves to, and the reference a span is
    /// compared against when deciding whether it needs its own tag.
    fn overlay_effective_faux_bold(&self) -> Option<FauxBoldParams> {
        (self.force_bold && self.faux_bold).then_some(FauxBoldParams {
            thicken_percent: self.faux_bold_thicken_percent,
            expand_percent: self.faux_bold_expand_percent,
            sharp_corners: self.faux_bold_sharp_corners,
            outward_only: self.faux_bold_outward_only,
        })
    }

    /// Whole-overlay effective faux-italic slant, analogous to
    /// [`Self::overlay_effective_faux_bold`]. `None` = overlay italic off or real
    /// Italic face requested.
    fn overlay_effective_faux_italic_slant(&self) -> Option<f32> {
        (self.force_italic && self.faux_italic).then_some(self.faux_italic_slant_deg)
    }

    pub(super) fn effective_inline_tag_style(
        &self,
        selection: &TypingInlineSelectionContext,
    ) -> TypingInlineTagStyle {
        let base_font_label = self
            .font_label_by_idx(self.selected_font_idx)
            .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string());
        TypingInlineTagStyle {
            bold: selection.style.bold || self.force_bold,
            italic: selection.style.italic || self.force_italic,
            // Mirror the renderer's `faux_bold_params_at_offset`: a span that sets
            // bold decides for itself INCLUDING `None` (bare `<b>` = real face,
            // never the overlay's faux). Only a spanless offset falls back to the
            // whole-overlay faux, and only while `force_bold` is on.
            faux_bold: if selection.style.bold {
                selection.style.faux_bold
            } else {
                self.overlay_effective_faux_bold()
            },
            faux_italic_slant: if selection.style.italic {
                selection.style.faux_italic_slant
            } else {
                self.overlay_effective_faux_italic_slant()
            },
            no_break: selection.style.no_break,
            align: Some(selection.style.align.unwrap_or(self.align)),
            font_label: Some(
                selection
                    .style
                    .font_label
                    .clone()
                    .unwrap_or(base_font_label),
            ),
            font_size_px: Some(selection.style.font_size_px.unwrap_or(self.font_size_px)),
            text_color: Some(selection.style.text_color.unwrap_or(self.text_color)),
            line_spacing: Some(selection.style.line_spacing.unwrap_or(self.line_spacing)),
            kerning: Some(selection.style.kerning.unwrap_or(self.kerning)),
            glyph_stretching: Some(
                selection
                    .style
                    .glyph_stretching
                    .unwrap_or([self.glyph_width, self.glyph_height]),
            ),
            glyph_offset: Some(
                selection
                    .style
                    .glyph_offset
                    .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0])),
            ),
        }
    }

    pub(super) fn apply_inline_style_to_selection(
        &mut self,
        selection: TypingInlineSelectionContext,
        desired_effective_style: TypingInlineTagStyle,
    ) -> bool {
        let desired_tag_style = self.normalize_desired_inline_tag_style(desired_effective_style);
        // По умолчанию панель пишет компактный машиночитаемый тег `<m ...>`.
        // Настройка `use_legacy_inline_tags` (пока не подключена к UI) вернёт обычные теги.
        let (opening_tags, closing_tags) = if self.use_legacy_inline_tags {
            (
                build_inline_opening_tags(&desired_tag_style),
                build_inline_closing_tags(&desired_tag_style),
            )
        } else {
            let opening = build_inline_machine_tag(&desired_tag_style);
            let closing = if opening.is_empty() {
                String::new()
            } else {
                "</m>".to_string()
            };
            (opening, closing)
        };

        let (new_text, new_selection_start_byte, new_selection_end_byte) = {
            let text = self.active_inline_text();
            let selected_text = text[selection.text_byte_range.clone()].to_string();
            let mut new_text = String::with_capacity(
                text.len()
                    + opening_tags.len()
                    + closing_tags.len()
                    + selection
                        .opening_wrapper_range
                        .len()
                        .saturating_sub(selection.closing_wrapper_range.len()),
            );
            new_text.push_str(&text[..selection.opening_wrapper_range.start]);
            new_text.push_str(&opening_tags);
            new_text.push_str(selected_text.as_str());
            new_text.push_str(&closing_tags);
            new_text.push_str(&text[selection.closing_wrapper_range.end..]);
            let start = selection.opening_wrapper_range.start + opening_tags.len();
            let end = start + selected_text.len();
            (new_text, start, end)
        };

        if new_text == self.active_inline_text() {
            return false;
        }

        self.set_active_inline_text(new_text);
        self.enable_inline_style_tags = true;
        self.pending_text_selection_restore = Some(
            byte_range_to_char_range(
                self.active_inline_text(),
                &(new_selection_start_byte..new_selection_end_byte),
            )
            .unwrap_or(selection.char_range),
        );
        self.queue_preview_render();
        true
    }

    pub(super) fn normalize_desired_inline_tag_style(
        &self,
        desired_effective_style: TypingInlineTagStyle,
    ) -> TypingInlineTagStyle {
        let base_font_label = self.font_label_by_idx(self.selected_font_idx);
        let desired_font_label = desired_effective_style
            .font_label
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty());
        let font_label = desired_font_label.and_then(|label| {
            if base_font_label
                .as_deref()
                .is_some_and(|base| base.eq_ignore_ascii_case(label.as_str()))
            {
                None
            } else {
                Some(label)
            }
        });
        let font_size_px = desired_effective_style
            .font_size_px
            .map(|value| value.clamp(1.0, 256.0))
            .filter(|value| (value - self.font_size_px).abs() > 0.05);
        let text_color = desired_effective_style
            .text_color
            .filter(|value| *value != self.text_color);
        let line_spacing = desired_effective_style
            .line_spacing
            .map(|value| clamp_px_or_percent(value, 300.0))
            .filter(|value| px_or_percent_differs(*value, self.line_spacing));
        let kerning = desired_effective_style
            .kerning
            .map(|value| clamp_px_or_percent(value, 300.0))
            .filter(|value| px_or_percent_differs(*value, self.kerning));
        let glyph_stretching = desired_effective_style
            .glyph_stretching
            .map(|value| {
                [
                    clamp_stretch_px_or_percent(value[0]),
                    clamp_stretch_px_or_percent(value[1]),
                ]
            })
            .filter(|value| {
                px_or_percent_differs(value[0], self.glyph_width)
                    || px_or_percent_differs(value[1], self.glyph_height)
            });
        let glyph_offset = desired_effective_style
            .glyph_offset
            .map(normalize_inline_offset_style)
            .filter(inline_offset_style_is_non_default);

        // Bold/faux-bold: a span is emitted only when it differs from the overlay's
        // effective bold state. When `force_bold` is off, the span carries the
        // desired state verbatim. When it is on, we strip the span ONLY if the
        // desired faux state equals the overlay's effective faux (both real, or the
        // same faux params); a differing faux state still emits a `bold: true` span
        // so it overrides the global per the renderer precedence (a bold span wins
        // INCLUDING a bare `<b>` = real face under a global faux overlay).
        let (bold, faux_bold) = if self.force_bold
            && desired_effective_style.faux_bold == self.overlay_effective_faux_bold()
        {
            (false, None)
        } else {
            (desired_effective_style.bold, desired_effective_style.faux_bold)
        };
        let (italic, faux_italic_slant) = if self.force_italic
            && desired_effective_style.faux_italic_slant
                == self.overlay_effective_faux_italic_slant()
        {
            (false, None)
        } else {
            (
                desired_effective_style.italic,
                desired_effective_style.faux_italic_slant,
            )
        };

        TypingInlineTagStyle {
            bold,
            italic,
            faux_bold,
            faux_italic_slant,
            font_label,
            font_size_px,
            text_color,
            line_spacing,
            kerning,
            glyph_stretching,
            glyph_offset,
            no_break: desired_effective_style.no_break,
            align: desired_effective_style
                .align
                .filter(|align| *align != self.align),
        }
    }
}
