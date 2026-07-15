/*
File: panel/create_main_text.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
the main text-parameter UI. Draws the left and right parameter columns, the
inline offset controls, the alignment controls, and computes the
selected-inline character count.

Main responsibilities:
- draw the main text params container and its left/right columns;
- draw inline per-selection offset controls and alignment controls;
- report how many characters the current inline selection covers.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so `panel.rs`
and sibling submodules can call them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {

    pub(super) fn draw_main_text_params(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
        font_memory_enabled: bool,
        font_missing: bool,
    ) -> bool {
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        let inline_selection = if self.preview_enabled {
            None
        } else {
            self.inline_selection_context()
        };
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            // Комбобокс группы шрифтов показывается на обеих панелях (создание и
            // редактирование); выбор синхронизируется между ними через
            // `pending_font_group_request` (см. обработку во внешнем цикле).
            {
                let mut selected_group_idx = self
                    .selected_font_group
                    .as_ref()
                    .and_then(|selected| {
                        self.font_groups.iter().position(|group| group == selected)
                    })
                    .map_or(0usize, |idx| idx + 1);
                let group_count = self.font_groups.len() + 1;
                let selected_group_text =
                    self.selected_font_group.as_deref().unwrap_or(t!("typing.params.font_group_all"));
                let group_combo = WheelComboBox::from_label(t!("typing.create.font_group_combo_id")).id_salt("typing.create.font_group_combo_id")
                    .selected_text(selected_group_text)
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(&mut selected_group_idx, 0usize, t!("typing.params.font_group_all"));
                        for (idx, group_name) in self.font_groups.iter().enumerate() {
                            ui.selectable_value(&mut selected_group_idx, idx + 1, group_name);
                        }
                    });
                mark_hscroll_block_on_hover(
                    &mut block_hscroll_by_hovered_param,
                    &group_combo.inner.response,
                );
                if let Some(steps) = group_combo.wheel_steps {
                    cycle_wrapped_index(&mut selected_group_idx, group_count, steps);
                }
                let previous_group = self.selected_font_group.clone();
                self.selected_font_group = if selected_group_idx == 0 {
                    None
                } else {
                    self.font_groups.get(selected_group_idx - 1).cloned()
                };
                if self.selected_font_group != previous_group {
                    self.ensure_selected_font_in_group();
                    self.pending_font_group_request = Some(self.selected_font_group.clone());
                    changed = true;
                }
            }

            let prev_font_idx = self.selected_font_idx;
            let filtered_font_indices = self.filtered_font_indices();
            let selected_font_text: String = if font_missing {
                // Шрифт оверлея не найден: показываем его имя, чтобы было понятно,
                // какой именно шрифт отсутствует и какой надо заменить.
                self.missing_font
                    .as_ref()
                    .map(|name| tf!("typing.params.font_not_found_option", name = name))
                    .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
            } else {
                inline_style
                    .as_ref()
                    .and_then(|style| style.font_label.clone())
                    .or_else(|| {
                        self.fonts
                            .get(self.selected_font_idx)
                            .map(|font| self.font_display_label(font))
                    })
                    .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
            };
            let mut font_idx = inline_style
                .as_ref()
                .and_then(|style| {
                    self.find_font_idx_by_path_or_label(None, style.font_label.as_deref())
                })
                .unwrap_or(self.selected_font_idx);
            if !filtered_font_indices.contains(&font_idx)
                && let Some(first_filtered_idx) = filtered_font_indices.first().copied()
            {
                font_idx = first_filtered_idx;
            }
            let font_combo = WheelComboBox::from_label(t!("typing.create.font_combo_id")).id_salt("typing.create.font_combo_id")
                .selected_text(selected_font_text)
                .show_ui_with_wheel(ui, |ui| {
                    for idx in filtered_font_indices.iter().copied() {
                        let (label, path, face_index, coverage) = {
                            let font = &self.fonts[idx];
                            (
                                self.font_display_label(font),
                                font.path.clone(),
                                font.faces.first().map(|face| face.face_index).unwrap_or(0),
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
                cycle_wrapped_index_in_values(&mut font_idx, &filtered_font_indices, steps);
            }
            if let Some(style) = inline_style.as_mut() {
                if let Some(label) = self.font_label_by_idx(font_idx) {
                    style.font_label = Some(label);
                }
            } else {
                self.selected_font_idx = font_idx;
                if self.selected_font_idx != prev_font_idx {
                    // Любой выбор из списка — это доступный шрифт, поэтому снимаем
                    // блокировку рендера по ненайденному шрифту.
                    self.missing_font = None;
                    if font_memory_enabled {
                        changed |= self.handle_create_font_selection_change(prev_font_idx);
                    } else {
                        self.selected_face_idx = 0;
                        changed = true;
                    }
                }
            }

            if font_missing {
                ui.colored_label(
                    Color32::from_rgb(240, 200, 60),
                    t!("typing.params.pick_available_font_hint"),
                );
            }

            ui.add_enabled_ui(!selection_mode, |ui| {
                let prev_face_idx = self.selected_face_idx;
                let selected_face_text = self
                    .fonts
                    .get(self.selected_font_idx)
                    .and_then(|font| font.faces.get(self.selected_face_idx))
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
                        if let Some(font) = self.fonts.get(self.selected_font_idx) {
                            for (idx, face) in font.faces.iter().enumerate() {
                                ui.selectable_value(&mut face_idx, idx, &face.label);
                            }
                        }
                    });
                mark_hscroll_block_on_hover(
                    &mut block_hscroll_by_hovered_param,
                    &face_combo.inner.response,
                );
                if let Some(steps) = face_combo.wheel_steps {
                    cycle_wrapped_index(&mut face_idx, face_count, steps);
                }
                self.selected_face_idx = face_idx;
                if self.selected_face_idx != prev_face_idx {
                    changed = true;
                }
            });

            ui.add_space(4.0);

            let spacing_x = ui.spacing().item_spacing.x;
            let available_w = ui.available_width().max(1.0);
            let columns_w = (available_w - spacing_x).max(1.0);
            let left_ratio = 1.3 / 2.3;
            let min_left_w = 160.0;
            let min_right_w = 120.0;
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

            // Остальные параметры влияют на рендер: при ненайденном шрифте они
            // блокируются, доступным остаётся только выбор шрифта выше.
            ui.add_enabled_ui(!font_missing, |ui| {
                if stacked_columns {
                    ui.allocate_ui_with_layout(
                        Vec2::new(columns_w, 0.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            self.draw_main_text_left_column(
                                ui,
                                &mut changed,
                                &mut block_hscroll_by_hovered_param,
                                inline_style.as_mut(),
                            )
                        },
                    );
                    ui.add_space(6.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(columns_w, 0.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            self.draw_main_text_right_column(
                                ui,
                                &mut changed,
                                &mut block_hscroll_by_hovered_param,
                                inline_style.as_mut(),
                            )
                        },
                    );
                } else {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(left_w, 0.0),
                            egui::Layout::top_down(Align::Min),
                            |ui| {
                                self.draw_main_text_left_column(
                                    ui,
                                    &mut changed,
                                    &mut block_hscroll_by_hovered_param,
                                    inline_style.as_mut(),
                                )
                            },
                        );

                        ui.allocate_ui_with_layout(
                            Vec2::new(right_w, 0.0),
                            egui::Layout::top_down(Align::Min),
                            |ui| {
                                self.draw_main_text_right_column(
                                    ui,
                                    &mut changed,
                                    &mut block_hscroll_by_hovered_param,
                                    inline_style.as_mut(),
                                )
                            },
                        );
                    });
                }
            });

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
        changed
    }

    pub(super) fn draw_inline_offset_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let inline_font_size_px = inline_style
            .as_ref()
            .and_then(|style| style.font_size_px)
            .unwrap_or(self.font_size_px)
            .max(1.0);
        ui.add_enabled_ui(inline_style.is_some(), |ui| {
            let mut offset = inline_style
                .as_ref()
                .and_then(|style| style.glyph_offset)
                .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0]));
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_x_label"),
                &mut offset.global_x,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_y_label"),
                &mut offset.global_y,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_along_line_label"),
                &mut offset.line,
                PxOrPercentRowCfg {
                    range: -300.0..=300.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                },
                changed,
                block_hscroll_by_hovered_param,
            );

            *changed |= ui
                .checkbox(&mut offset.shift_following, t!("typing.params.inline_shift_following"))
                .changed();

            let group_enabled = inline_style
                .as_ref()
                .is_some_and(|_| self.selected_inline_char_count() > 1);
            ui.add_enabled_ui(group_enabled, |ui| {
                let group_resp = ui.add(
                    WheelSlider::new(&mut offset.group_rotation_deg, -180.0..=180.0)
                        .text(t!("typing.params.inline_group_rotation_label"))
                        .wheel_step(1.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &group_resp);
                *changed |= group_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &group_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut offset.group_rotation_deg,
                        steps,
                        1.0,
                        -180.0,
                        180.0,
                    );
                }
            });
            if !group_enabled {
                offset.group_rotation_deg = 0.0;
            }

            let glyph_resp = ui.add(
                WheelSlider::new(&mut offset.glyph_rotation_deg, -180.0..=180.0)
                    .text(t!("typing.params.inline_char_rotation_label"))
                    .wheel_step(1.0),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &glyph_resp);
            *changed |= glyph_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &glyph_resp) {
                *changed |=
                    apply_wheel_step_f32(&mut offset.glyph_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
            if let Some(style) = inline_style {
                style.glyph_offset = Some(offset);
            }
        });
    }

    pub(super) fn selected_inline_char_count(&self) -> usize {
        self.text_selection_char_range
            .as_ref()
            .map(|range| range.end.saturating_sub(range.start))
            .unwrap_or(0)
    }

    pub(super) fn draw_main_text_left_column(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let selection_mode = inline_style.is_some();
        if let Some(style) = inline_style.as_mut() {
            let mut text_color = style.text_color.unwrap_or(self.text_color);
            let color_resp = self.text_color_selector.draw(ui, &mut text_color);
            *changed |= color_resp.changed;
            style.text_color = Some(text_color);
            let mut font_size_px = style
                .font_size_px
                .unwrap_or(self.font_size_px)
                .clamp(1.0, 256.0);
            let font_size_resp = ui.add(
                WheelSlider::new(&mut font_size_px, 1.0..=256.0)
                    .text(t!("typing.params.size_px_label"))
                    .wheel_step(1.0),
            );
            *changed |= font_size_resp.changed();
            style.font_size_px = Some(font_size_px);
        } else {
            let color_resp = self.text_color_selector.draw(ui, &mut self.text_color);
            *changed |= color_resp.changed;
            let font_size_resp = ui.add(
                WheelSlider::new(&mut self.font_size_px, 1.0..=256.0)
                    .text(t!("typing.params.size_px_label"))
                    .wheel_step(1.0),
            );
            *changed |= font_size_resp.changed();
        }

        let base_font_size_px = self.font_size_px.max(1.0);
        if let Some(style) = inline_style.as_mut() {
            let inline_font_size_px = style.font_size_px.unwrap_or(base_font_size_px).max(1.0);
            let mut line_spacing = style.line_spacing.unwrap_or(self.line_spacing);
            px_or_percent_param_row(
                ui,
                t!("typing.params.line_spacing_label"),
                &mut line_spacing,
                PxOrPercentRowCfg {
                    range: -300.0..=300.0,
                    wheel_step: 2.0,
                    font_size_px: inline_font_size_px,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            style.line_spacing = Some(line_spacing);

            ui.horizontal(|ui| {
                ui.label(t!("typing.params.kerning_label"));
                // Read-only indicator of the global kerning mode (kerning is not a
                // per-span inline override). Optical is not offered as a choice.
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
            let mut kerning = style.kerning.unwrap_or(self.kerning);
            px_or_percent_param_row(
                ui,
                t!("typing.params.kerning_label"),
                &mut kerning,
                PxOrPercentRowCfg {
                    range: -300.0..=300.0,
                    wheel_step: 2.0,
                    font_size_px: inline_font_size_px,
                },
                changed,
                block_hscroll_by_hovered_param,
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
                changed,
                block_hscroll_by_hovered_param,
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
                changed,
                block_hscroll_by_hovered_param,
            );
            style.glyph_stretching = Some(stretching);
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
                changed,
                block_hscroll_by_hovered_param,
            );

            ui.horizontal(|ui| {
                ui.label(t!("typing.params.kerning_label"));
                // Optical is implemented but intentionally not offered here; only
                // Fixed ("Метрический") and Auto ("Авто") are user-selectable.
                *changed |= ui
                    .selectable_value(&mut self.kerning_mode, KerningMode::Fixed, t!("typing.params.kerning_metric"))
                    .changed();
                *changed |= ui
                    .selectable_value(&mut self.kerning_mode, KerningMode::Auto, t!("typing.params.kerning_auto"))
                    .changed();
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
                changed,
                block_hscroll_by_hovered_param,
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
                changed,
                block_hscroll_by_hovered_param,
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
                changed,
                block_hscroll_by_hovered_param,
            );
        }

        if selection_mode {
            self.draw_inline_offset_controls(
                ui,
                changed,
                block_hscroll_by_hovered_param,
                inline_style,
            );
        }
    }

    /// Управление выравниванием на ОДНОЙ строке: слайдер лево↔право (`-100..100`),
    /// быстрые кнопки (⬅ влево / ⬇ по центру / ➡ вправо) и зажимаемая кнопка-тоггл
    /// ⬌ (justify, «Растягивать по ширине блока»). Слайдер и стрелки отключаются при
    /// включённом justify; кнопка ⬌ остаётся активной, чтобы его можно было выключить.
    pub(super) fn draw_alignment_controls(
        ui: &mut egui::Ui,
        align: &mut HorizontalAlign,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let free_align = align.justify;
        ui.horizontal(|ui| {
            // Слайдер + стрелки отключаются при включённом justify.
            ui.add_enabled_ui(!free_align, |ui| {
                let mut bias_percent = (align.bias.clamp(-1.0, 1.0) * 100.0).round() as i32;
                let slider_resp = ui.add(
                    WheelSlider::new(&mut bias_percent, -100..=100)
                        .text(t!("typing.params.alignment_label"))
                        .wheel_step(5),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
                if slider_resp.changed() {
                    align.bias = bias_percent as f32 / 100.0;
                    *changed = true;
                }

                if ui.button("⬅").on_hover_text(t!("typing.params.align_left")).clicked() {
                    align.bias = -1.0;
                    *changed = true;
                }
                if ui.button("⬇").on_hover_text(t!("typing.params.align_center")).clicked() {
                    align.bias = 0.0;
                    *changed = true;
                }
                if ui.button("➡").on_hover_text(t!("typing.params.align_right")).clicked() {
                    align.bias = 1.0;
                    *changed = true;
                }
            });

            // Зажимаемая кнопка-тоггл justify — остаётся активной даже при включённом
            // justify, чтобы его можно было снять.
            if ui
                .add(egui::Button::new("⬌").selected(align.justify))
                .on_hover_text(t!("typing.params.justify_lines"))
                .clicked()
            {
                align.justify = !align.justify;
                *changed = true;
            }
        });
    }

    pub(super) fn draw_main_text_right_column(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!selection_mode, |ui| {
            Self::draw_alignment_controls(
                ui,
                &mut self.align,
                changed,
                block_hscroll_by_hovered_param,
            );

            // Глобальный поворот всего блока: применяется к векторным контурам
            // глифов ДО растеризации, поэтому получается чётче, чем поворот уже
            // готового растра оверлея.
            let rotation_resp = ui
                .add(
                    WheelSlider::new(&mut self.global_rotation_deg, -180.0..=180.0)
                        .text(t!("typing.params.global_rotation_label"))
                        .wheel_step(1.0),
                )
                .on_hover_text(t!("typing.params.global_rotation_tooltip"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &rotation_resp);
            *changed |= rotation_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &rotation_resp) {
                *changed |=
                    apply_wheel_step_f32(&mut self.global_rotation_deg, steps, 1.0, -180.0, 180.0);
            }

            // Размещение по линии: перпендикулярный сдвиг глифов относительно
            // линии/пути. Показывается только для линейных раскладок (формула и
            // векторные линии); для остальных режимов параметр скрыт и игнорируется
            // рендером.
            if matches!(
                self.text_layout_mode,
                TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines
            ) {
                ui.horizontal(|ui| {
                    let placement_resp = ui
                        .add(
                            WheelSlider::new(&mut self.line_placement_percent, -100.0..=100.0)
                                .text(t!("typing.params.line_placement_label"))
                                .wheel_step(5.0),
                        )
                        .on_hover_text(
                            t!("typing.params.line_placement_tooltip"),
                        );
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &placement_resp);
                    *changed |= placement_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &placement_resp) {
                        *changed |= apply_wheel_step_f32(
                            &mut self.line_placement_percent,
                            steps,
                            5.0,
                            -100.0,
                            100.0,
                        );
                    }

                    if ui.button("⬇").on_hover_text(t!("typing.params.line_placement_bottom")).clicked() {
                        self.line_placement_percent = -100.0;
                        *changed = true;
                    }
                    if ui.button("⬍").on_hover_text(t!("typing.params.line_placement_center")).clicked() {
                        self.line_placement_percent = 0.0;
                        *changed = true;
                    }
                    if ui.button("⬆").on_hover_text(t!("typing.params.line_placement_top")).clicked() {
                        self.line_placement_percent = 100.0;
                        *changed = true;
                    }
                });
            }

            let prev_shape = self.text_shape;
            let shape_combo = WheelComboBox::from_label(t!("typing.create.shape_combo_id")).id_salt("typing.create.shape_combo_id")
                .selected_text(match self.text_shape {
                    TextShape::Free => t!("typing.params.shape_free_option"),
                    TextShape::Rectangle => "[  ]",
                    TextShape::Oval => "(  )",
                    TextShape::Hexagon => "<  >",
                    TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
                })
                .show_ui_with_wheel(ui, |ui| {
                    ui.selectable_value(&mut self.text_shape, TextShape::Free, t!("typing.params.shape_free_option"));
                    ui.selectable_value(&mut self.text_shape, TextShape::Rectangle, "[  ]");
                    ui.selectable_value(&mut self.text_shape, TextShape::Oval, "(  )");
                    ui.selectable_value(&mut self.text_shape, TextShape::Hexagon, "<  >");
                    ui.selectable_value(&mut self.text_shape, TextShape::SoftPeak, t!("typing.params.shape_soft_option"));
                });
            mark_hscroll_block_on_hover(
                block_hscroll_by_hovered_param,
                &shape_combo.inner.response,
            );
            if let Some(steps) = shape_combo.wheel_steps {
                *changed |= cycle_text_shape(&mut self.text_shape, steps);
            }
            if self.text_shape != prev_shape {
                *changed = true;
            }

            let prev_wrap_mode = self.text_wrap_mode;
            let wrap_combo = WheelComboBox::from_label(t!("typing.create.wrap_combo_id")).id_salt("typing.create.wrap_combo_id")
                .selected_text(text_wrap_mode_label(self.text_wrap_mode))
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
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &wrap_combo.inner.response);
            if let Some(steps) = wrap_combo.wheel_steps {
                *changed |= cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
            }
            if self.text_wrap_mode != prev_wrap_mode {
                self.sync_wrap_mode_constraints();
                *changed = true;
            }

            let prev_anti_aliasing = self.anti_aliasing;
            let aa_combo = WheelComboBox::from_label(t!("typing.create.antialias_combo_id")).id_salt("typing.create.antialias_combo_id")
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
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &aa_combo.inner.response);
            if let Some(steps) = aa_combo.wheel_steps {
                *changed |= cycle_anti_aliasing(&mut self.anti_aliasing, steps);
            }
            if self.anti_aliasing != prev_anti_aliasing {
                *changed = true;
            }
            let moderate_trees_resp = ui.add_enabled(
                self.moderate_trees_checkbox_enabled(),
                egui::Checkbox::new(&mut self.allow_moderate_trees, t!("typing.params.allow_moderate_herringbone")),
            );
            *changed |= moderate_trees_resp.changed();

            if matches!(self.text_shape, TextShape::Oval | TextShape::Hexagon) {
                let min_width_resp = ui.add(
                    WheelSlider::new(&mut self.shape_min_width_percent, 5.0..=100.0)
                        .text(t!("typing.params.min_width_percent_label")),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &min_width_resp);
                *changed |= min_width_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &min_width_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut self.shape_min_width_percent,
                        steps,
                        1.0,
                        5.0,
                        100.0,
                    );
                }
            }
            if self.text_shape == TextShape::SoftPeak {
                let variant_resp =
                    ui.add(WheelSlider::new(&mut self.shape_variant, 1..=9).text(t!("typing.params.shape_variant_label")));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &variant_resp);
                *changed |= variant_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &variant_resp) {
                    *changed |= apply_wheel_step_u8(&mut self.shape_variant, steps, 1, 1, 9);
                }
            }
        });
        if let Some(style) = inline_style {
            let mut align = style.align.unwrap_or(self.align);
            Self::draw_alignment_controls(ui, &mut align, changed, block_hscroll_by_hovered_param);
            style.align = Some(align);

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
                changed,
                block_hscroll_by_hovered_param,
                "typing_main_inline_faux",
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
            let no_break_resp = ui.checkbox(&mut no_break, t!("typing.params.no_break"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &no_break_resp);
            *changed |= no_break_resp.changed();
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
                changed,
                block_hscroll_by_hovered_param,
                "typing_main_faux",
            );
        }
        ui.add_enabled_ui(!selection_mode, |ui| {
            let hanging_punct_resp =
                ui.checkbox(&mut self.hanging_punctuation, t!("typing.params.hanging_punctuation"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &hanging_punct_resp);
            *changed |= hanging_punct_resp.changed();
            let trim_spaces_resp =
                ui.checkbox(&mut self.trim_extra_spaces, t!("typing.params.strip_extra_spaces"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &trim_spaces_resp);
            *changed |= trim_spaces_resp.changed();
            let sentence_nl_resp = ui.checkbox(
                &mut self.new_line_after_sentence,
                t!("typing.params.newline_after_sentence"),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sentence_nl_resp);
            *changed |= sentence_nl_resp.changed();
            let uppercase_text_resp =
                ui.checkbox(&mut self.uppercase_text, t!("typing.params.all_uppercase"));
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &uppercase_text_resp);
            *changed |= uppercase_text_resp.changed();
            let inline_tags_resp = ui.checkbox(
                &mut self.enable_inline_style_tags,
                t!("typing.params.parse_bi_tags"),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inline_tags_resp);
            *changed |= inline_tags_resp.changed();

            self.draw_advanced_text_params_section(
                ui,
                changed,
                block_hscroll_by_hovered_param,
                "typing_advanced_text_params_right_column",
            );
        });
    }

}
