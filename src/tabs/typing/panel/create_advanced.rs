/*
File: panel/create_advanced.rs

Purpose:
Part of `impl TypingCreatePanelState`, extracted verbatim from `panel.rs`.
Advanced text-params UI: formula/shape layout controls, formula spacing, the
competing text accordion, and the "advanced form" enumeration window.

Main responsibilities:
- draw the advanced text-params section and its formula/shape layout controls;
- draw formula spacing controls and the competing text accordion;
- drive the advanced-form window: buttons, preview font, source text, metric
  signature and glyph-width cache rebuild, and applying a chosen form.

Notes:
Extracted verbatim from `panel.rs`; methods are `pub(super)` so the module root
and sibling `panel` submodules can call them. `use super::*;` pulls in the
parent module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {

    pub(super) fn draw_advanced_text_params_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        id_salt: &'static str,
    ) {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(t!("typing.advanced.section_header")).id_salt("typing.advanced.section_header")
            .id_salt((id_salt, self.preview_enabled))
            .default_open(false)
            .show(ui, |ui| {
                let prev_mode = self.text_line_mode;
                let line_mode_combo = WheelComboBox::from_label(t!("typing.advanced.line_mode_combo_label")).id_salt("typing.advanced.line_mode_combo_label")
                    .selected_text(match self.text_line_mode {
                        TextLineMode::Horizontal => t!("typing.params.line_mode_horizontal"),
                        TextLineMode::Vertical => t!("typing.params.line_mode_vertical"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_line_mode,
                            TextLineMode::Horizontal,
                            t!("typing.params.line_mode_horizontal"),
                        );
                        ui.selectable_value(
                            &mut self.text_line_mode,
                            TextLineMode::Vertical,
                            t!("typing.params.line_mode_vertical"),
                        );
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &line_mode_combo.inner.response,
                );
                if let Some(steps) = line_mode_combo.wheel_steps {
                    *changed |= cycle_text_line_mode(&mut self.text_line_mode, steps);
                }
                if self.text_line_mode != prev_mode {
                    *changed = true;
                }
                if self.text_line_mode == TextLineMode::Vertical {
                    let prev_direction = self.vertical_line_direction;
                    let direction_combo = WheelComboBox::from_label(t!("typing.advanced.line_arrangement_combo_label")).id_salt("typing.advanced.line_arrangement_combo_label")
                        .selected_text(match self.vertical_line_direction {
                            VerticalLineDirection::LeftToRight => t!("typing.params.direction_left_to_right"),
                            VerticalLineDirection::RightToLeft => t!("typing.params.direction_right_to_left"),
                        })
                        .show_ui_with_wheel(ui, |ui| {
                            ui.selectable_value(
                                &mut self.vertical_line_direction,
                                VerticalLineDirection::LeftToRight,
                                t!("typing.params.direction_left_to_right"),
                            );
                            ui.selectable_value(
                                &mut self.vertical_line_direction,
                                VerticalLineDirection::RightToLeft,
                                t!("typing.params.direction_right_to_left"),
                            );
                        });
                    mark_hscroll_block_on_hover(
                        block_hscroll_by_hovered_param,
                        &direction_combo.inner.response,
                    );
                    if let Some(steps) = direction_combo.wheel_steps {
                        *changed |=
                            cycle_vertical_line_direction(&mut self.vertical_line_direction, steps);
                    }
                    if self.vertical_line_direction != prev_direction {
                        *changed = true;
                    }
                }

                let prev_layout_mode = self.text_layout_mode;
                let layout_mode_combo = WheelComboBox::from_label(t!("typing.advanced.layout_combo_label")).id_salt("typing.advanced.layout_combo_label")
                    .selected_text(match self.text_layout_mode {
                        TextLayoutMode::Normal => t!("typing.advanced.layout_kind_standard"),
                        TextLayoutMode::Formula => t!("typing.advanced.layout_kind_formula"),
                        TextLayoutMode::Shape => t!("typing.advanced.layout_kind_shape"),
                        TextLayoutMode::CustomRasterLines => t!("typing.advanced.layout_kind_vector_lines"),
                        TextLayoutMode::CustomVectorLines => t!("typing.advanced.layout_kind_vector_lines"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::Normal,
                            t!("typing.advanced.layout_kind_standard"),
                        );
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::Formula,
                            t!("typing.advanced.layout_kind_formula"),
                        );
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::CustomVectorLines,
                            t!("typing.advanced.layout_kind_vector_lines"),
                        );
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &layout_mode_combo.inner.response,
                );
                if let Some(steps) = layout_mode_combo.wheel_steps {
                    *changed |= cycle_text_layout_mode(&mut self.text_layout_mode, steps);
                }
                if self.text_layout_mode != prev_layout_mode {
                    *changed = true;
                }

                match self.text_layout_mode {
                    TextLayoutMode::Normal => {}
                    TextLayoutMode::Formula => {
                        self.draw_formula_layout_controls(
                            ui,
                            changed,
                            block_hscroll_by_hovered_param,
                        );
                    }
                    TextLayoutMode::Shape => {
                        self.draw_shape_layout_controls(
                            ui,
                            changed,
                            block_hscroll_by_hovered_param,
                        );
                    }
                    TextLayoutMode::CustomRasterLines => {}
                    TextLayoutMode::CustomVectorLines => {
                        ui.add_space(4.0);
                        ui.label(
                            t!("typing.advanced.vector_layout_hint"),
                        );
                    }
                }
            });
    }

    pub(super) fn draw_formula_layout_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        ui.add_space(4.0);
        let mut formula_direct_edit_changed = false;
        ui.horizontal(|ui| {
            ui.label(t!("typing.advanced.formula_preset_label"));
            let mut names: Vec<String> = self.formula_presets_by_name.keys().cloned().collect();
            names.sort();
            let prev_selected = self.selected_formula_preset_name.clone();
            let selected_text = self
                .selected_formula_preset_name
                .as_deref()
                .unwrap_or(text_preset_none_label());
            let preset_len = names.len() + 1;
            let mut preset_idx = self
                .selected_formula_preset_name
                .as_ref()
                .and_then(|selected| names.iter().position(|name| name == selected))
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let combo_resp =
                WheelComboBox::from_id_salt(("typing_formula_preset_combo", self.preview_enabled))
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(preset_idx == 0, text_preset_none_label())
                            .clicked()
                        {
                            preset_idx = 0;
                        }
                        for (idx, name) in names.iter().enumerate() {
                            if ui.selectable_label(preset_idx == idx + 1, name).clicked() {
                                preset_idx = idx + 1;
                            }
                        }
                    });
            if let Some(steps) = combo_resp.wheel_steps {
                cycle_wrapped_index(&mut preset_idx, preset_len, steps);
            }
            self.selected_formula_preset_name = if preset_idx == 0 {
                None
            } else {
                names.get(preset_idx - 1).cloned()
            };
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &combo_resp.inner.response);
            if self.selected_formula_preset_name != prev_selected
                && let Some(name) = self.selected_formula_preset_name.clone()
                && self.apply_formula_preset_by_name(name)
            {
                *changed = true;
            }
        });
        ui.horizontal(|ui| {
            let preset_name_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_preset_name_input)
                    .id_salt(("typing_formula_preset_name_input", self.preview_enabled))
                    .hint_text(t!("typing.presets.save_preset_button"))
                    .desired_width((ui.available_width() - 96.0).max(120.0)),
            );
            self.track_text_input(&preset_name_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &preset_name_resp);
            if ui.button(t!("typing.presets.save_button")).clicked() {
                self.save_current_formula_preset();
            }
        });

        ui.horizontal(|ui| {
            ui.label(t!("typing.advanced.formula_label"));
            let x_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.x_expr)
                    .hint_text("x(t, ...)")
                    .desired_width(150.0),
            );
            self.track_text_input(&x_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &x_resp);
            formula_direct_edit_changed |= x_resp.changed();
            *changed |= x_resp.changed();

            let swap_resp = ui
                .small_button("⇄")
                .on_hover_text(t!("typing.advanced.swap_xy_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &swap_resp);
            if swap_resp.clicked() {
                self.swap_formula_xy_expressions();
                formula_direct_edit_changed = true;
                *changed = true;
            }

            let y_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.y_expr)
                    .hint_text("y(t, ...)")
                    .desired_width(150.0),
            );
            self.track_text_input(&y_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &y_resp);
            formula_direct_edit_changed |= y_resp.changed();
            *changed |= y_resp.changed();
        });

        ui.horizontal(|ui| {
            ui.label("rotation:");
            let rot_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.rotation_expr)
                    .hint_text("rot (rad)")
                    .desired_width(110.0),
            );
            self.track_text_input(&rot_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &rot_resp);
            formula_direct_edit_changed |= rot_resp.changed();
            *changed |= rot_resp.changed();

            if ui.small_button("?").clicked() {
                self.formula_help_open = !self.formula_help_open;
            }
        });

        if self.formula_help_open {
            ui.label(t!("typing.advanced.formula_variables_hint"));
            ui.label(t!("typing.advanced.formula_functions_hint"));
            ui.label(t!("typing.advanced.formula_t_range_hint"));
            ui.label(t!("typing.advanced.formula_curve_length_hint"));
        }

        let tangent_resp = ui.checkbox(
            &mut self.formula_layout.use_tangent_rotation,
            t!("typing.advanced.tangent_rotation"),
        );
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &tangent_resp);
        formula_direct_edit_changed |= tangent_resp.changed();
        *changed |= tangent_resp.changed();

        ui.horizontal(|ui| {
            let t_start_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.t_start)
                    .speed(0.01)
                    .prefix(t!("typing.advanced.formula_t_start_label")),
            );
            let t_start_resp =
                t_start_resp.on_hover_text(t!("typing.advanced.formula_t_start_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &t_start_resp);
            formula_direct_edit_changed |= t_start_resp.changed();
            *changed |= t_start_resp.changed();
            let t_end_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.t_end)
                    .speed(0.01)
                    .prefix(t!("typing.advanced.formula_t_end_label")),
            );
            let t_end_resp = t_end_resp.on_hover_text(t!("typing.advanced.formula_t_end_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &t_end_resp);
            formula_direct_edit_changed |= t_end_resp.changed();
            *changed |= t_end_resp.changed();
        });
        ui.horizontal(|ui| {
            let offset_x_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.offset_x_px)
                    .speed(1.0)
                    .prefix(t!("typing.advanced.formula_offset_x_label")),
            );
            let offset_x_resp =
                offset_x_resp.on_hover_text(t!("typing.advanced.formula_offset_x_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &offset_x_resp);
            formula_direct_edit_changed |= offset_x_resp.changed();
            *changed |= offset_x_resp.changed();
            let offset_y_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.offset_y_px)
                    .speed(1.0)
                    .prefix(t!("typing.advanced.formula_offset_y_label")),
            );
            let offset_y_resp =
                offset_y_resp.on_hover_text(t!("typing.advanced.formula_offset_y_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &offset_y_resp);
            formula_direct_edit_changed |= offset_y_resp.changed();
            *changed |= offset_y_resp.changed();
        });
        ui.horizontal(|ui| {
            let scale_x_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.scale_x)
                    .speed(0.01)
                    .prefix(t!("typing.advanced.formula_scale_x_label")),
            );
            let scale_x_resp = scale_x_resp.on_hover_text(t!("typing.advanced.formula_scale_x_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &scale_x_resp);
            formula_direct_edit_changed |= scale_x_resp.changed();
            *changed |= scale_x_resp.changed();
            let scale_y_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.scale_y)
                    .speed(0.01)
                    .prefix(t!("typing.advanced.formula_scale_y_label")),
            );
            let scale_y_resp = scale_y_resp.on_hover_text(t!("typing.advanced.formula_scale_y_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &scale_y_resp);
            formula_direct_edit_changed |= scale_y_resp.changed();
            *changed |= scale_y_resp.changed();
        });
        self.draw_formula_spacing_controls(
            ui,
            changed,
            block_hscroll_by_hovered_param,
            &mut formula_direct_edit_changed,
        );

        ui.label(t!("typing.advanced.formula_constants_label"));
        egui::Grid::new(("typing_formula_vars_grid", self.preview_enabled)).show(ui, |ui| {
            for idx in 0..TEXT_FORMULA_USER_VAR_COUNT {
                ui.label(format!("{} =", (b'a' + idx as u8) as char));
                let resp = ui.add(
                    WheelSpinBox::new(&mut self.formula_layout.vars[idx])
                        .speed(0.05)
                        .range(-100000.0..=100000.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &resp);
                formula_direct_edit_changed |= resp.changed();
                *changed |= resp.changed();
                if idx % 2 == 1 {
                    ui.end_row();
                }
            }
        });
        if formula_direct_edit_changed {
            self.selected_formula_preset_name = None;
        }
    }

    pub(super) fn draw_shape_layout_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(t!("typing.advanced.shape_combo_label"));
            let prev_kind = self.shape_layout_kind;
            let mut kind_idx = match self.shape_layout_kind {
                TypingShapeLayoutKind::Arc => 0,
                TypingShapeLayoutKind::Circle => 1,
                TypingShapeLayoutKind::Spiral => 2,
                TypingShapeLayoutKind::Polygon => 3,
                TypingShapeLayoutKind::Zigzag => 4,
                TypingShapeLayoutKind::SCurve => 5,
            };
            let combo_resp =
                WheelComboBox::from_id_salt(("typing_shape_layout_kind", self.preview_enabled))
                    .selected_text(match self.shape_layout_kind {
                        TypingShapeLayoutKind::Arc => t!("typing.advanced.shape_kind_arc"),
                        TypingShapeLayoutKind::Circle => t!("typing.advanced.shape_kind_circle"),
                        TypingShapeLayoutKind::Spiral => t!("typing.advanced.shape_kind_spiral"),
                        TypingShapeLayoutKind::Polygon => t!("typing.advanced.shape_kind_polygon"),
                        TypingShapeLayoutKind::Zigzag => t!("typing.advanced.shape_kind_zigzag"),
                        TypingShapeLayoutKind::SCurve => t!("typing.advanced.shape_kind_scurve"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        for (idx, label) in [
                            t!("typing.advanced.shape_kind_arc"),
                            t!("typing.advanced.shape_kind_circle"),
                            t!("typing.advanced.shape_kind_spiral"),
                            t!("typing.advanced.shape_kind_polygon"),
                            t!("typing.advanced.shape_kind_zigzag"),
                            t!("typing.advanced.shape_kind_scurve"),
                        ]
                        .iter()
                        .enumerate()
                        {
                            if ui.selectable_label(kind_idx == idx, *label).clicked() {
                                kind_idx = idx;
                            }
                        }
                    });
            if let Some(steps) = combo_resp.wheel_steps {
                cycle_wrapped_index(&mut kind_idx, 6, steps);
            }
            self.shape_layout_kind = match kind_idx {
                0 => TypingShapeLayoutKind::Arc,
                1 => TypingShapeLayoutKind::Circle,
                2 => TypingShapeLayoutKind::Spiral,
                3 => TypingShapeLayoutKind::Polygon,
                4 => TypingShapeLayoutKind::Zigzag,
                _ => TypingShapeLayoutKind::SCurve,
            };
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &combo_resp.inner.response);
            if self.shape_layout_kind != prev_kind {
                *changed = true;
            }
        });

        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => {
                ui.horizontal(|ui| {
                    ui.label(t!("typing.advanced.orientation_label"));
                    let prev_orientation = self.arc_shape_layout.orientation;
                    let mut orientation_idx = match self.arc_shape_layout.orientation {
                        TypingArcOrientation::Horizontal => 0,
                        TypingArcOrientation::Vertical => 1,
                    };
                    let combo_resp = WheelComboBox::from_id_salt((
                        "typing_arc_shape_orientation",
                        self.preview_enabled,
                    ))
                    .selected_text(self.arc_shape_layout.orientation.label())
                    .show_ui_with_wheel(ui, |ui| {
                        for (idx, orientation) in [
                            TypingArcOrientation::Horizontal,
                            TypingArcOrientation::Vertical,
                        ]
                        .iter()
                        .enumerate()
                        {
                            if ui
                                .selectable_label(orientation_idx == idx, orientation.label())
                                .clicked()
                            {
                                orientation_idx = idx;
                            }
                        }
                    });
                    if let Some(steps) = combo_resp.wheel_steps {
                        cycle_wrapped_index(&mut orientation_idx, 2, steps);
                    }
                    self.arc_shape_layout.orientation = match orientation_idx {
                        0 => TypingArcOrientation::Horizontal,
                        _ => TypingArcOrientation::Vertical,
                    };
                    mark_hscroll_block_on_hover(
                        block_hscroll_by_hovered_param,
                        &combo_resp.inner.response,
                    );
                    if self.arc_shape_layout.orientation != prev_orientation {
                        *changed = true;
                    }
                });

                let width_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.length_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_length_label")),
                );
                let width_resp =
                    width_resp.on_hover_text(t!("typing.advanced.arc_length_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.amplitude_px, -800.0..=800.0)
                        .text(t!("typing.advanced.shape_amplitude_label")),
                );
                let height_resp = height_resp.on_hover_text(
                    t!("typing.advanced.arc_amplitude_tooltip"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let freq_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.frequency, 0.25..=6.0)
                        .text(t!("typing.advanced.shape_frequency_label")),
                );
                let freq_resp = freq_resp.on_hover_text(
                    t!("typing.advanced.arc_frequency_tooltip"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &freq_resp);
                *changed |= freq_resp.changed();
            }
            TypingShapeLayoutKind::Circle => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.circle_shape_layout.width_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_width_label")),
                );
                let width_resp =
                    width_resp.on_hover_text(t!("typing.advanced.circle_width_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.circle_shape_layout.height_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_height_label")),
                );
                let height_resp = height_resp
                    .on_hover_text(t!("typing.advanced.circle_height_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();
            }
            TypingShapeLayoutKind::Spiral => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.width_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_width_label")),
                );
                let width_resp =
                    width_resp.on_hover_text(t!("typing.advanced.spiral_width_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.height_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_height_label")),
                );
                let height_resp =
                    height_resp.on_hover_text(t!("typing.advanced.spiral_height_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let turns_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.turns, 0.25..=8.0)
                        .text(t!("typing.advanced.spiral_turns_label")),
                );
                let turns_resp =
                    turns_resp.on_hover_text(t!("typing.advanced.spiral_turns_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &turns_resp);
                *changed |= turns_resp.changed();

                let inner_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.inner_ratio, 0.0..=0.95)
                        .text(t!("typing.advanced.spiral_inner_radius_label")),
                );
                let inner_resp =
                    inner_resp.on_hover_text(t!("typing.advanced.spiral_inner_radius_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inner_resp);
                *changed |= inner_resp.changed();
            }
            TypingShapeLayoutKind::Polygon => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.width_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_width_label")),
                );
                let width_resp = width_resp.on_hover_text(t!("typing.advanced.polygon_width_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.height_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_height_label")),
                );
                let height_resp = height_resp.on_hover_text(t!("typing.advanced.polygon_height_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let sides_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.sides, 3..=12).text(t!("typing.advanced.polygon_sides_label")),
                );
                let sides_resp =
                    sides_resp.on_hover_text(t!("typing.advanced.polygon_sides_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sides_resp);
                *changed |= sides_resp.changed();
            }
            TypingShapeLayoutKind::Zigzag => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.width_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_width_label")),
                );
                let width_resp = width_resp.on_hover_text(t!("typing.advanced.zigzag_width_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.height_px, -800.0..=800.0)
                        .text(t!("typing.advanced.shape_height_label")),
                );
                let height_resp = height_resp.on_hover_text(
                    t!("typing.advanced.zigzag_height_tooltip"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let segments_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.segments, 0.5..=12.0)
                        .text(t!("typing.advanced.zigzag_segments_label")),
                );
                let segments_resp =
                    segments_resp.on_hover_text(t!("typing.advanced.zigzag_segments_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &segments_resp);
                *changed |= segments_resp.changed();
            }
            TypingShapeLayoutKind::SCurve => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.width_px, 32.0..=2000.0)
                        .text(t!("typing.advanced.shape_width_label")),
                );
                let width_resp = width_resp.on_hover_text(t!("typing.advanced.scurve_width_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.height_px, -800.0..=800.0)
                        .text(t!("typing.advanced.shape_height_label")),
                );
                let height_resp = height_resp.on_hover_text(
                    t!("typing.advanced.scurve_height_tooltip"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let bends_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.bends, 0.5..=4.0)
                        .text(t!("typing.advanced.scurve_curves_label")),
                );
                let bends_resp = bends_resp.on_hover_text(t!("typing.advanced.scurve_curves_tooltip"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &bends_resp);
                *changed |= bends_resp.changed();
            }
        }

        let mut shape_changed = false;
        let tangent_resp = ui.checkbox(
            &mut self.formula_layout.use_tangent_rotation,
            t!("typing.advanced.tangent_rotation"),
        );
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &tangent_resp);
        shape_changed |= tangent_resp.changed();
        *changed |= tangent_resp.changed();
        self.draw_formula_spacing_controls(
            ui,
            changed,
            block_hscroll_by_hovered_param,
            &mut shape_changed,
        );
    }

    pub(super) fn draw_formula_spacing_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        local_changed: &mut bool,
    ) {
        ui.horizontal(|ui| {
            let normal_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.normal_offset_px)
                    .speed(0.5)
                    .prefix(t!("typing.advanced.spacing_offset_label")),
            );
            let normal_resp = normal_resp.on_hover_text(
                t!("typing.advanced.spacing_offset_tooltip"),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &normal_resp);
            *local_changed |= normal_resp.changed();
            *changed |= normal_resp.changed();
            let spacing_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.letter_spacing_mul)
                    .range(0.0..=8.0)
                    .speed(0.01)
                    .prefix(t!("typing.advanced.spacing_tracking_label")),
            );
            let spacing_resp = spacing_resp
                .on_hover_text(t!("typing.advanced.spacing_tracking_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &spacing_resp);
            *local_changed |= spacing_resp.changed();
            *changed |= spacing_resp.changed();
        });
        ui.horizontal(|ui| {
            let spacing_px_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.letter_spacing_px)
                    .speed(0.25)
                    .range(-1000.0..=1000.0)
                    .prefix(t!("typing.advanced.spacing_interval_label")),
            );
            let spacing_px_resp = spacing_px_resp.on_hover_text(
                t!("typing.advanced.spacing_interval_tooltip"),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &spacing_px_resp);
            *local_changed |= spacing_px_resp.changed();
            *changed |= spacing_px_resp.changed();
        });
    }

    /// Конкурирующий аккордеон «Изначальный текст» / «Сформированный текст»:
    /// развёрнут ровно один. Без сформированного текста развёрнут исходный.
    /// Возвращает `true`, если что-то изменилось.
    pub(super) fn draw_text_accordion(
        &mut self,
        ui: &mut egui::Ui,
        id_suffix: &str,
        block_hscroll: &mut bool,
    ) -> bool {
        let mut changed = false;
        // Без сформированного текста всегда развёрнут исходный.
        if self.formed_text.trim().is_empty() {
            self.advanced_text_show_formed = false;
        }
        let show_formed = self.advanced_text_show_formed;

        // Заголовок «Изначальный текст»: ▼ если развёрнут, ◀ если свёрнут.
        let source_arrow = if show_formed { "◀" } else { "▼" };
        if ui
            .selectable_label(!show_formed, tf!("typing.advanced.source_text_accordion", source_arrow = source_arrow))
            .clicked()
            && show_formed
        {
            // Переключение пана: старое выделение относилось к другому буферу.
            self.clear_inline_text_selection();
            self.advanced_text_show_formed = false;
        }
        if !show_formed {
            self.inline_text_target = InlineTextTarget::Source;
            let text_colors = build_inline_tag_editor_text_colors(&self.text);
            let text_output = TextEditPlus::multiline(&mut self.text)
                .id_salt(format!("typing_edit_text_source_{id_suffix}"))
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(ui.available_width(), EDIT_TEXT_FIELD_HEIGHT_PX))
                .text_colors(text_colors)
                .show(ui);
            self.paint_persistent_text_selection_if_needed(ui, &text_output);
            self.track_text_input(&text_output.response);
            self.sync_text_selection_from_text_edit(
                ui.ctx(),
                text_output.response.id,
                &text_output.response,
                text_output.cursor_range,
            );
            mark_hscroll_block_on_hover(block_hscroll, &text_output.response);
            changed |= text_output.response.changed();
        }

        // Сформированный текст раскрывается НАД своим заголовком (поэтому ▲).
        if show_formed {
            self.inline_text_target = InlineTextTarget::Formed;
            let text_colors = build_inline_tag_editor_text_colors(&self.formed_text);
            let formed_output = TextEditPlus::multiline(&mut self.formed_text)
                .id_salt(format!("typing_edit_text_formed_{id_suffix}"))
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(ui.available_width(), EDIT_TEXT_FIELD_HEIGHT_PX))
                .text_colors(text_colors)
                .show(ui);
            self.paint_persistent_text_selection_if_needed(ui, &formed_output);
            self.track_text_input(&formed_output.response);
            self.sync_text_selection_from_text_edit(
                ui.ctx(),
                formed_output.response.id,
                &formed_output.response,
                formed_output.cursor_range,
            );
            mark_hscroll_block_on_hover(block_hscroll, &formed_output.response);
            changed |= formed_output.response.changed();
        }

        // Заголовок «Сформированный текст»: ▲ если развёрнут (поле над ним), ◀ если свёрнут.
        let formed_arrow = if show_formed { "▲" } else { "◀" };
        if ui
            .selectable_label(show_formed, tf!("typing.advanced.formed_text_accordion", formed_arrow = formed_arrow))
            .clicked()
            && !show_formed
            && !self.formed_text.trim().is_empty()
        {
            // Переключение пана: старое выделение относилось к другому буферу.
            self.clear_inline_text_selection();
            self.advanced_text_show_formed = true;
        }

        ui.add_space(6.0);
        changed |= self.draw_advanced_form_buttons(ui);
        changed
    }

    /// Кнопки «Продвинутая форма текста» и «Вернуть исходный» под полем текста.
    pub(super) fn draw_advanced_form_buttons(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button(t!("typing.advanced.advanced_form_button")).clicked() {
                self.advanced_form_open = true;
                self.advanced_form_cache = None;
                self.advanced_form_centered = false;
            }
            // «Вернуть исходный» просто очищает сформированный текст и
            // разворачивает исходный.
            let has_formed = !self.formed_text.is_empty();
            let revert = ui.add_enabled(has_formed, egui::Button::new(t!("typing.advanced.restore_source_button")));
            if revert.clicked() {
                self.formed_text.clear();
                self.advanced_text_show_formed = false;
                self.queue_preview_render();
                changed = true;
            }
        });
        changed
    }

    /// Шрифт для отображения форм (тот же, что выбран в панели), или дефолтный.
    pub(super) fn advanced_form_preview_font(&mut self, ctx: &egui::Context) -> egui::FontId {
        const PREVIEW_FONT_SIZE_PX: f32 = 22.0;
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let face_index = font
                .faces
                .get(self.selected_face_idx)
                .map_or(0, |face| face.face_index);
            let path = font.path.clone();
            if let Some(family) = self.ensure_combo_font_family(ctx, &path, face_index) {
                return egui::FontId::new(PREVIEW_FONT_SIZE_PX, family);
            }
        }
        egui::FontId::new(PREVIEW_FONT_SIZE_PX, egui::FontFamily::Proportional)
    }

    /// Текст, по которому перебираются формы — всегда исходный (`text`).
    pub(super) fn advanced_form_source_text(&self) -> String {
        forms::prepare_inline_no_break_text(&self.text)
    }

    /// От чего зависят пиксельные ширины глифов в окне форм.
    pub(super) fn advanced_form_metric_signature(&self) -> AdvancedFormMetricSignature {
        let font = self.fonts.get(self.selected_font_idx);
        AdvancedFormMetricSignature {
            font_path: font.map(|font| font.path.to_string_lossy().to_string()),
            face_index: font
                .and_then(|font| font.faces.get(self.selected_face_idx))
                .map_or(0, |face| face.face_index),
            force_bold: self.force_bold,
            force_italic: self.force_italic,
            hanging_punctuation: self.hanging_punctuation,
        }
    }

    /// Строит попиксельную метрику ширины (`GlyphWidths`) выбранным шрифтом для
    /// символов `source_text`. `None`, если шрифт не выбран/не читается — тогда
    /// падаем на посимвольную метрику.
    pub(super) fn build_advanced_form_glyph_widths(&self, source_text: &str) -> Option<forms::GlyphWidths> {
        // Единицы на em для замеров (должно совпадать с метрикой внутри forms).
        const METRIC_EM: f32 = 1000.0;
        let font = self.fonts.get(self.selected_font_idx)?;
        let face_index = font
            .faces
            .get(self.selected_face_idx)
            .map_or(0, |face| face.face_index);
        let path = font.path.clone();
        // Лёгкая система шрифтов: пустая БД + только нужный файл (без системных шрифтов).
        let mut font_system =
            FontSystem::new_with_locale_and_db("en-US".to_string(), fontdb::Database::new());
        // One-shot throwaway system: use a fresh, empty cache. This path is not
        // pooled (it deliberately avoids the system-font scan for metric-only
        // measurement), so the cache only satisfies the load API.
        let mut font_cache = FontFaceCache::new();
        let selected_face =
            load_selected_font_from_path(&mut font_system, &mut font_cache, &path, face_index)
                .ok()?;
        let mut attrs = Attrs::new().metrics(Metrics::new(METRIC_EM, METRIC_EM));
        attrs = selected_face.apply_to_attrs(attrs);
        if self.force_bold {
            attrs = attrs.weight(cosmic_text::Weight::BOLD);
        }
        if self.force_italic {
            attrs = attrs.style(cosmic_text::Style::Italic);
        }
        Some(forms::GlyphWidths::build(
            &mut font_system,
            &attrs,
            source_text,
            self.hanging_punctuation,
            forms::DEFAULT_WIDTH_TOLERANCE,
        ))
    }

    pub(super) fn rebuild_advanced_form_cache_if_needed(&mut self) {
        let source_text = self.advanced_form_source_text();
        let signature = self.advanced_form_metric_signature();
        let stale = match &self.advanced_form_cache {
            Some(cache) => {
                cache.source_text != source_text
                    || cache.preset != self.advanced_form_preset
                    || cache.metric_signature != signature
            }
            None => true,
        };
        if !stale {
            return;
        }
        // Попиксельная метрика выбранным шрифтом; при отсутствии шрифта —
        // посимвольная (с учётом висящей пунктуации).
        let glyph_widths = self.build_advanced_form_glyph_widths(&source_text);
        let char_metric = forms::CharWidthMetric::new(self.hanging_punctuation);
        let metric: &dyn forms::LineWidthMetric = match &glyph_widths {
            Some(glyph_widths) => glyph_widths,
            None => &char_metric,
        };
        // Храним ВСЕ удачные формы (перебор ограничен лишь бюджетом узлов
        // рекурсии). Фильтры применяются ко всему набору; ограничение на 600 —
        // только в отрисовке (`ADVANCED_FORM_DISPLAY_LIMIT`).
        let enumeration = forms::enumerate_forms(
            &source_text,
            self.advanced_form_preset,
            usize::MAX,
            metric,
        );
        let mut forms = enumeration.forms;
        sort_advanced_forms(&mut forms);
        let mut group_counts: Vec<usize> =
            forms.iter().map(|form| form.word_break_count).collect();
        group_counts.sort_unstable();
        group_counts.dedup();
        // Сбрасываем выбор группы, если такого числа переносов больше нет.
        if let Some(selected) = self.advanced_form_group
            && !group_counts.contains(&selected)
        {
            self.advanced_form_group = None;
        }
        let line_bounds = inclusive_bounds(forms.iter().map(|form| form.line_count()));
        let width_bounds = inclusive_bounds(forms.iter().map(|form| form.max_width));
        let peak_max_bound_min = forms
            .iter()
            .map(|form| form.peakiness_pct(PeakBase::Min))
            .max()
            .unwrap_or(0);
        let peak_max_bound_median = forms
            .iter()
            .map(|form| form.peakiness_pct(PeakBase::Median))
            .max()
            .unwrap_or(0);
        let uneven_max_bound = forms.iter().map(|form| form.unevenness_pct).max().unwrap_or(0);
        let conservatism_bound = forms
            .iter()
            .map(|form| form.conservatism)
            .max()
            .unwrap_or(Conservatism::Safe);
        // Диапазоны фильтров заново раскрываются на всю ширину данных; пороги
        // пиковости и неравномерности — на максимум (показываем всё).
        self.advanced_form_line_range = line_bounds;
        self.advanced_form_width_range = width_bounds;
        self.advanced_form_peak_max = match self.advanced_form_peak_base {
            PeakBase::Min => peak_max_bound_min,
            PeakBase::Median => peak_max_bound_median,
        };
        self.advanced_form_uneven_max = uneven_max_bound;
        // Консервативность по умолчанию строгая (`Safe`): показываем только формы
        // без отрыва служебных слов, как раньше. Пользователь ослабляет вручную.
        self.advanced_form_conservatism_max = Conservatism::Safe;
        self.advanced_form_cache = Some(AdvancedFormCache {
            source_text,
            preset: self.advanced_form_preset,
            forms,
            group_counts,
            line_bounds,
            width_bounds,
            metric_signature: signature,
            peak_max_bound_min,
            peak_max_bound_median,
            uneven_max_bound,
            conservatism_bound,
            truncated: enumeration.truncated,
        });
    }

    /// Применяет выбранную форму: записывает её как сформированный текст (исходный
    /// `text` не трогаем) и разворачивает сформированный пан.
    pub(super) fn apply_advanced_form(&mut self, form: &TextForm) {
        self.formed_text = form.to_text();
        self.advanced_text_show_formed = true;
        self.queue_preview_render();
    }

    /// Плавающее окно перебора форм текста.
    pub(super) fn draw_advanced_form_window(&mut self, ctx: &egui::Context) -> bool {
        if !self.advanced_form_open {
            return false;
        }
        self.rebuild_advanced_form_cache_if_needed();
        let font_id = self.advanced_form_preview_font(ctx);
        let current_preset = self.advanced_form_preset;
        let current_group = self.advanced_form_group;
        let cache = self.advanced_form_cache.take();

        // Окно центрируется по вьюпорту по итоговому размеру. На первых кадрах
        // (пока размер ещё не измерен) окно скрыто, чтобы не дёргалось.
        let centering = !self.advanced_form_centered;
        let viewport = ctx.content_rect();
        let screen_center = viewport.center();
        let default_size = egui::vec2(viewport.width() * 0.8, viewport.height() * 0.8);

        let mut line_range = self.advanced_form_line_range;
        let mut width_range = self.advanced_form_width_range;
        let mut peak_max = self.advanced_form_peak_max;
        let mut peak_base = self.advanced_form_peak_base;
        let mut uneven_max = self.advanced_form_uneven_max;
        let mut conservatism_max = self.advanced_form_conservatism_max;

        let mut open = true;
        let mut new_preset = current_preset;
        let mut new_group = current_group;
        let mut clicked: Option<usize> = None;

        let mut window = egui::Window::new(t!("typing.advanced.advanced_form_window_title")).id(egui::Id::new("typing.advanced.advanced_form_window_title"))
            .open(&mut open)
            .resizable(true)
            // Над панелями параметров/действий (они на `Order::Foreground`).
            .order(egui::Order::Tooltip)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_size(default_size);
        if centering {
            window = window.current_pos(screen_center);
        }

        let inner = window.show(ctx, |ui| {
            if centering {
                // Прячем содержимое, пока окно не встанет по центру.
                ui.set_opacity(0.0);
            }
            ui.small(
                t!("typing.advanced.form_preview_hint"),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(t!("typing.advanced.form_shape_label"));
                for preset in TextFormPreset::all() {
                    // The crate returns a key for the prose preset and a literal
                    // ASCII shape for the others; resolve the key, paint the shape.
                    let label = match preset.label() {
                        PresetLabel::Key(key) => crate::i18n_resolve::resolve_key(key),
                        PresetLabel::Shape(shape) => shape,
                    };
                    if ui
                        .selectable_label(preset == current_preset, label)
                        .clicked()
                    {
                        new_preset = preset;
                    }
                }
            });
            ui.separator();
            match cache.as_ref() {
                Some(cache) if !cache.forms.is_empty() => {
                    if cache.group_counts.len() > 1 {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(t!("typing.advanced.form_hyphenation_label"));
                            if ui
                                .selectable_label(current_group.is_none(), t!("typing.advanced.form_hyphenation_all"))
                                .clicked()
                            {
                                new_group = None;
                            }
                            for &count in &cache.group_counts {
                                if ui
                                    .selectable_label(
                                        current_group == Some(count),
                                        count.to_string(),
                                    )
                                    .clicked()
                                {
                                    new_group = Some(count);
                                }
                            }
                        });
                    }
                    // Диапазонные фильтры: число строк и ширина строки.
                    let has_line = advanced_form_range_row(
                        ui,
                        t!("typing.advanced.form_lines_label"),
                        "",
                        &mut line_range,
                        cache.line_bounds,
                    );
                    let has_width = advanced_form_range_row(
                        ui,
                        t!("typing.advanced.form_width_label"),
                        "",
                        &mut width_range,
                        cache.width_bounds,
                    );
                    // Порог пиковости: насколько % самая длинная строка длиннее
                    // базовой (минимальной/медианной). Один верхний предел.
                    let peak_bound = match peak_base {
                        PeakBase::Min => cache.peak_max_bound_min,
                        PeakBase::Median => cache.peak_max_bound_median,
                    };
                    let has_peak = peak_bound > 0;
                    if has_peak {
                        ui.add(
                            WheelSlider::new(&mut peak_max, 0..=peak_bound)
                                .text(t!("typing.advanced.form_longer_than_base_label"))
                                .suffix("%"),
                        );
                        ui.horizontal(|ui| {
                            ui.label(t!("typing.advanced.form_peakiness_base_label"));
                            if ui
                                .selectable_label(peak_base == PeakBase::Min, t!("typing.advanced.form_peakiness_min"))
                                .clicked()
                            {
                                peak_base = PeakBase::Min;
                            }
                            if ui
                                .selectable_label(peak_base == PeakBase::Median, t!("typing.advanced.form_peakiness_median"))
                                .clicked()
                            {
                                peak_base = PeakBase::Median;
                            }
                        });
                    }
                    // Порог неравномерности: средний разброс ширин строк от
                    // медианы. Меньше — ровнее форма.
                    let uneven_bound = cache.uneven_max_bound;
                    let has_uneven = uneven_bound > 0;
                    if has_uneven {
                        ui.add(
                            WheelSlider::new(&mut uneven_max, 0..=uneven_bound)
                                .text(t!("typing.advanced.form_unevenness_label"))
                                .suffix("%"),
                        );
                    }
                    // Порог консервативности: какие отрывы служебных слов допускать.
                    // `Safe` («нет») — только безопасные переносы; каждая следующая
                    // категория добавляет более рискованные отрывы.
                    let has_conservatism = cache.conservatism_bound > Conservatism::Safe;
                    if has_conservatism {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(t!("typing.advanced.form_orphan_words_label"));
                            for level in Conservatism::all() {
                                if level > cache.conservatism_bound {
                                    break;
                                }
                                let text = if level == Conservatism::Safe {
                                    t!("typing.advanced.form_orphan_words_none").to_string()
                                } else {
                                    // Crate hands a catalog key; resolve to active-locale label.
                                    format!("+ {}", crate::i18n_resolve::resolve_key(level.label_key()))
                                };
                                if ui
                                    .selectable_label(conservatism_max == level, text)
                                    .clicked()
                                {
                                    conservatism_max = level;
                                }
                            }
                        });
                    }
                    if (has_line || has_width || has_peak || has_uneven || has_conservatism)
                        && ui.small_button(t!("typing.advanced.form_reset_filters_button")).clicked()
                    {
                        line_range = cache.line_bounds;
                        width_range = cache.width_bounds;
                        peak_max = peak_bound;
                        uneven_max = uneven_bound;
                        conservatism_max = Conservatism::Safe;
                        new_group = None;
                    }

                    let passes = |form: &TextForm| {
                        new_group.is_none_or(|c| form.word_break_count == c)
                            && (line_range.0..=line_range.1).contains(&form.line_count())
                            && (width_range.0..=width_range.1).contains(&form.max_width)
                            && form.peakiness_pct(peak_base) <= peak_max
                            && form.unevenness_pct <= uneven_max
                            && form.conservatism <= conservatism_max
                    };

                    let visible = cache.forms.iter().filter(|form| passes(form)).count();
                    let shown = visible.min(ADVANCED_FORM_DISPLAY_LIMIT);
                    let mut status = if shown < visible {
                        tf!("typing.advanced.form_variants_shown_status", visible = visible, shown = shown)
                    } else {
                        tf!("typing.advanced.form_variants_status", visible = visible)
                    };
                    if cache.truncated {
                        status.push_str(t!("typing.advanced.form_variants_incomplete_status"));
                    }
                    ui.small(status);
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                let mut drawn = 0usize;
                                for (idx, form) in cache.forms.iter().enumerate() {
                                    if !passes(form) {
                                        continue;
                                    }
                                    if drawn >= ADVANCED_FORM_DISPLAY_LIMIT {
                                        break;
                                    }
                                    drawn += 1;
                                    if draw_advanced_form_card(ui, &font_id, &form.lines)
                                        .clicked()
                                    {
                                        clicked = Some(idx);
                                    }
                                }
                            });
                        });
                }
                Some(_) => {
                    ui.label(t!("typing.advanced.form_no_variants_status"));
                }
                None => {
                    ui.label(t!("typing.advanced.form_enter_text_status"));
                }
            }
        });

        // Как только окно отрисовалось и знает свой размер — на следующем кадре
        // оно уже стоит по центру; делаем его видимым.
        if centering {
            if inner.is_some_and(|inner| {
                inner.response.rect.width() > 1.0 && inner.response.rect.height() > 1.0
            }) {
                self.advanced_form_centered = true;
            }
            ctx.request_repaint();
        }

        self.advanced_form_line_range = line_range;
        self.advanced_form_width_range = width_range;
        // Смена базы делает старый порог несопоставимым — раскрываем его на
        // максимум для новой базы.
        if peak_base != self.advanced_form_peak_base {
            self.advanced_form_peak_base = peak_base;
            if let Some(cache) = cache.as_ref() {
                peak_max = match peak_base {
                    PeakBase::Min => cache.peak_max_bound_min,
                    PeakBase::Median => cache.peak_max_bound_median,
                };
            }
        }
        self.advanced_form_peak_max = peak_max;
        self.advanced_form_uneven_max = uneven_max;
        self.advanced_form_conservatism_max = conservatism_max;

        let mut changed = false;
        if let Some(idx) = clicked
            && let Some(cache) = cache.as_ref()
            && let Some(form) = cache.forms.get(idx)
        {
            self.apply_advanced_form(form);
            // После выбора формы окно закрывается.
            open = false;
            changed = true;
        }
        self.advanced_form_cache = cache;
        if new_preset != self.advanced_form_preset {
            self.advanced_form_preset = new_preset;
            self.advanced_form_cache = None;
        }
        self.advanced_form_group = new_group;
        self.advanced_form_open = open;
        changed
    }
}
