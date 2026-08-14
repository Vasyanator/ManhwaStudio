/*
File: panel/create_advanced.rs

Purpose:
Part of `impl TypingCreatePanelState`, extracted verbatim from `panel.rs`.
Advanced text-params UI: formula/shape layout controls, formula spacing, the
competing text accordion, and the "advanced form" enumeration window.

Main responsibilities:
- draw the advanced text-params section and its formula/shape layout controls;
- draw formula spacing controls and the competing text accordion;
- drive the advanced-form window: buttons, preview font, source text, the metric
  signature, the BACKGROUND form search and its «Параметры поиска» section, the
  presentation order, and applying a chosen form.

Notes:
Extracted verbatim from `panel.rs`; methods are `pub(super)` so the module root
and sibling `panel` submodules can call them. `use super::*;` pulls in the
parent module's types and imports.

The advanced-form width metric must describe the text the RENDERER draws, so
`apply_metric_real_bold_italic` mirrors `ms_text_render`'s
`base_attrs_real_bold_italic` (real Bold/Italic face only when forced WITHOUT
faux). Its `FontSystem` holds an empty fontdb plus the one selected font FILE, so
the request is additionally gated by `metric_real_face_availability`: a face that
file does not contain is never requested (cosmic-text would otherwise find no
match at all and panic). See `panel/MODULE_README.md` for the fidelity trade-off.

THE METRIC'S FONT BYTES ARE OBTAINED OFF THE GUI THREAD. `poll_advanced_form_font`
dispatches a `FontProvider::resolve` (a cache miss is an `fs::read`, forbidden here —
`CLAUDE.md` §5) and caches the result in `AdvancedFormFont`. The FIRST search WAITS for
them (`schedule_advanced_form_search` returns while a resolve is in flight): their
arrival changes `AdvancedFormMetricSignature::font_content_id`, so enumerating before
they land means enumerating the very same text twice. The same two-step shape as the
on-canvas editor font (`tab/create_upload.rs`). The own-typeface PREVIEW of the form
cards goes through `widgets::request_font_family`, which reads its file off-thread for
the same reason.

NEITHER IS THE SEARCH ITSELF ON THE GUI THREAD. `AdvancedFormSearchKey` describes the
whole input; a change to it arms a ~200 ms debounce and then a named
`typing-form-search` worker that builds the width metric AND runs
`forms::search_forms`. Replacing the job cancels the previous one through its `Drop`
(`AdvancedFormSearchJob`), the same cancel-on-supersede shape as
`tab::TypingShapeVariantPreviewState`. While a search is in flight the window keeps
drawing the PREVIOUS result plus a "recomputing" line — never an empty grid.
The presentation order (`text_forms::order_advanced_forms`) is applied on the GUI
thread, because it is a sort over a few hundred cards and its two knobs (quality
floor, narrow lean) must NOT re-run the search.
*/

use super::*;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use super::advanced_form_params::{
    ASPECT_MAX_MAX, ASPECT_MAX_MIN, EVENNESS_MAX, EVENNESS_MIN, HYPHEN_RATIO_MAX,
    HYPHEN_RATIO_MIN, HYPHEN_RELAX_SLACK_MAX, HYPHEN_RELAX_SLACK_MIN, NARROW_SLOTS_MAX,
    NARROW_SLOTS_MIN, PER_BUCKET_MAX, PER_BUCKET_MIN, QUALITY_FLOOR_MAX, QUALITY_FLOOR_MIN,
    advanced_form_params, set_advanced_form_params,
};

/// Пауза без изменений входа, после которой запускается перебор форм.
///
/// Ключ поиска меняется на КАЖДОМ нажатии клавиши и на каждом шаге слайдера;
/// без этой паузы серия нажатий запускала бы (и тут же отменяла) по перебору на
/// кадр. 200 мс — порядок величины самого перебора на худшей реплике корпуса
/// (~50 мс, план §2.5), то есть цена ожидания ощутимо ниже цены рестарта.
const ADVANCED_FORM_SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// Пауза без изменений ручек поиска, после которой они пишутся в
/// `user_config.json`. Слайдер отдаёт новое значение каждый кадр перетаскивания,
/// поэтому запись «на каждое изменение» была бы чистой амплификацией; значение
/// применяется к процесс-глобальному состоянию СРАЗУ, ждёт только диск.
const ADVANCED_FORM_PARAMS_SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

/// Единиц метрики на em у [`forms::GlyphWidths`] — она меряет глифы в 1/1000 em.
const GLYPH_METRIC_UNITS_PER_EM: f32 = 1000.0;

/// Единиц метрики на em у [`forms::CharWidthMetric`]: она считает СИМВОЛЫ, а
/// строчный латинско-кириллический символ занимает примерно пол-em, то есть ~2
/// символа на em. Число приблизительное по своей природе — приблизительна и сама
/// метрика; оно нужно лишь для того, чтобы потолок пропорции формы имел
/// осмысленный масштаб, пока байты шрифта не пришли.
const CHAR_METRIC_UNITS_PER_EM: f32 = 2.0;

/// Высота строки в долях em при вырожденных параметрах текста (интерлиньяж
/// 120 %) — то же значение, что `forms::FormSearchParams::default()`.
const DEFAULT_LINE_HEIGHT_EM: f32 = 1.2;

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

        // The accordion is ONE surface with two mutually exclusive buffers, so the
        // extended-tier check goes here, on the buffer that is about to be drawn. The
        // field is a `TextEditPlus`, i.e. the `Proportional` family, which carries only
        // the `core` chain until something asks for more: without this a pasted Arabic /
        // Thai / Hebrew overlay showed tofu in the panel while the rendered page (whose
        // fallback chain is the renderer's, not egui's) was correct. Cheap and idempotent
        // — see `ui_fonts::ensure_covers`; the work happens on a worker thread.
        crate::ui_fonts::ensure_covers(
            ui.ctx(),
            if show_formed {
                &self.formed_text
            } else {
                &self.text
            },
        );

        // Заголовок «Изначальный текст»: ▼ если развёрнут, ◀ если свёрнут.
        // Кнопка «Таблица символов» живёт в ЭТОЙ же строке (у заголовка
        // сформированного текста её нет: вставка всегда идёт в активный буфер и
        // дублировать точку входа незачем).
        let source_arrow = if show_formed { "◀" } else { "▼" };
        ui.horizontal(|ui| {
            if ui
                .selectable_label(!show_formed, tf!("typing.advanced.source_text_accordion", source_arrow = source_arrow))
                .clicked()
                && show_formed
            {
                // Переключение пана: старое выделение относилось к другому буферу.
                self.clear_inline_text_selection();
                self.advanced_text_show_formed = false;
            }
            if ui
                .button(t!("typing.char_table.open_button"))
                .on_hover_text(t!("typing.char_table.open_button_tooltip"))
                .clicked()
            {
                self.char_table.toggle_open();
            }
        });
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
                // Кэш НЕ выбрасывается: если вход не изменился, окно откроется с
                // готовой сеткой, а если изменился — ключ поиска это заметит сам,
                // и прежний результат подержится ровно до прихода нового.
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
    ///
    /// Пока байты выбранного шрифта читаются фоном (`widgets::request_font_family`),
    /// возвращается интерфейсный шрифт — файл никогда не читается на GUI-потоке.
    pub(super) fn advanced_form_preview_font(&self, ctx: &egui::Context) -> egui::FontId {
        const PREVIEW_FONT_SIZE_PX: f32 = 22.0;
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let face_index = font
                .faces
                .get(self.selected_face_idx)
                .map_or(0, |face| face.face_index);
            // The path is only the BYTE SOURCE for the one-time egui registration; the
            // identity plus the content hash are what the family name and the cache key
            // are derived from (the hash expires a binding whose file was replaced).
            let path = font.path.clone();
            let identity = font.render_identity_name();
            let content_hash = font.content_hash();
            if let crate::widgets::PreviewFontFamily::Ready(family) =
                crate::widgets::request_font_family(ctx, &identity, content_hash, &path, face_index)
            {
                return egui::FontId::new(PREVIEW_FONT_SIZE_PX, family);
            }
        }
        egui::FontId::new(PREVIEW_FONT_SIZE_PX, egui::FontFamily::Proportional)
    }

    /// Текст, по которому перебираются формы — всегда исходный (`text`).
    pub(super) fn advanced_form_source_text(&self) -> String {
        forms::prepare_inline_no_break_text(&self.text)
    }

    /// The font bytes the width metric may measure with RIGHT NOW: the ones the
    /// background resolve produced for the currently selected font, or `None` while they
    /// are on their way (or when that identity resolves to nothing).
    ///
    /// The bytes come from the panel's own `FontProvider`, i.e. from the SAME resolution
    /// the renderer performs, so the metric can never measure a different file than the
    /// one the page is drawn with.
    fn advanced_form_font_content(&self) -> Option<&FontContent> {
        let identity = self.fonts.get(self.selected_font_idx)?.render_identity_name();
        let loaded = self.advanced_form_font.as_ref()?;
        if loaded.identity != identity {
            return None;
        }
        loaded.content.as_ref()
    }

    /// Starts (or picks up) the BACKGROUND resolve of the font the width metric measures
    /// with, and returns whether the panel is still waiting for it.
    ///
    /// A `FontProvider::resolve` miss reads the font file, which must never happen on the
    /// GUI thread (`CLAUDE.md` §5) — the same reason the on-canvas editor font goes
    /// through `create_upload::request_editor_font`. Until the bytes arrive the form
    /// window enumerates with the coarse per-CHARACTER metric; their arrival changes the
    /// cache signature (`font_content_id`), which is what rebuilds the forms with real
    /// glyph widths. Call once per frame while the window is open.
    pub(super) fn poll_advanced_form_font(&mut self, ctx: &egui::Context) {
        // 1. Pick up a finished resolve without blocking.
        if let Some(request) = self.advanced_form_font_request.as_ref() {
            match request.rx.try_recv() {
                Ok(content) => {
                    let identity = request.identity.clone();
                    self.advanced_form_font_request = None;
                    self.advanced_form_font = Some(AdvancedFormFont { identity, content });
                }
                Err(TryRecvError::Empty) => {
                    // The worker schedules no frame of its own.
                    ctx.request_repaint();
                }
                Err(TryRecvError::Disconnected) => {
                    // The worker died without sending; remember the failure so the next
                    // frame does not spawn another one for the same font.
                    let identity = request.identity.clone();
                    self.advanced_form_font_request = None;
                    self.advanced_form_font = Some(AdvancedFormFont {
                        identity,
                        content: None,
                    });
                }
            }
        }

        // 2. Ask for the current selection when nothing answers for it yet.
        let Some(identity) = self
            .fonts
            .get(self.selected_font_idx)
            .map(FontEntry::render_identity_name)
        else {
            return;
        };
        let answered = self
            .advanced_form_font
            .as_ref()
            .is_some_and(|loaded| loaded.identity == identity);
        let in_flight = self
            .advanced_form_font_request
            .as_ref()
            .is_some_and(|request| request.identity == identity);
        if answered || in_flight {
            return;
        }

        let (tx, rx) = mpsc::channel::<Option<FontContent>>();
        let provider = Arc::clone(&self.font_provider);
        let worker_identity = identity.clone();
        match thread::Builder::new()
            .name("typing-form-metric-font".to_string())
            .spawn(move || {
                // A failed send only means the window was closed or the selection changed
                // before the bytes arrived; the provider keeps them cached either way.
                let _ = tx.send(provider.resolve(&worker_identity));
            }) {
            Ok(_handle) => {
                self.advanced_form_font_request = Some(AdvancedFormFontRequest { identity, rx });
                ctx.request_repaint();
            }
            Err(error) => {
                // The window still works — it enumerates with per-character widths — but
                // the reason must be diagnosable rather than silent.
                self.advanced_form_font = Some(AdvancedFormFont {
                    identity: identity.clone(),
                    content: None,
                });
                crate::runtime_log::log_error(format!(
                    "typing advanced forms: failed to spawn the width-metric font resolver; \
                     the forms are enumerated with approximate per-character widths. \
                     Font: {identity} Error: {error}"
                ));
            }
        }
    }

    /// От чего зависят пиксельные ширины глифов в окне форм.
    ///
    /// The font axis is its IDENTITY: two list entries that happen to share a FILE (the
    /// built-in interface entry and a user import of `core[0]`) are measured against
    /// different font databases, so they must not share a cache signature. The CONTENT
    /// id of the resolved bytes is the second half of that axis — see the field.
    pub(super) fn advanced_form_metric_signature(&self) -> AdvancedFormMetricSignature {
        let font = self.fonts.get(self.selected_font_idx);
        AdvancedFormMetricSignature {
            font_identity: font.map(FontEntry::render_identity_name),
            font_content_id: self
                .advanced_form_font_content()
                .map(|content| content.content_id),
            face_index: font
                .and_then(|font| font.faces.get(self.selected_face_idx))
                .map_or(0, |face| face.face_index),
            force_bold: self.force_bold,
            force_italic: self.force_italic,
            faux_bold: self.faux_bold,
            faux_bold_thicken_percent: self.faux_bold_thicken_percent.to_bits(),
            faux_bold_expand_percent: self.faux_bold_expand_percent.to_bits(),
            faux_bold_sharp_corners: self.faux_bold_sharp_corners,
            faux_bold_outward_only: self.faux_bold_outward_only,
            // Faux italic flips the face Italic <-> Regular, which changes advances
            // for families that ship a real Italic face -> the width metric goes
            // stale. The slant magnitude is a pure shear and stays out.
            faux_italic: self.faux_italic,
            hanging_punctuation: self.hanging_punctuation,
        }
    }

    /// Снимок всего, что нужно для ПОСТРОЕНИЯ метрики ширины, — чтобы её строил
    /// фоновый воркер поиска, а не GUI-поток (см. [`AdvancedFormMetricSpec`]).
    ///
    /// Байты берутся из уже разрешённого кэша (`poll_advanced_form_font`); `None`
    /// в них означает «мерить посимвольно».
    pub(super) fn advanced_form_metric_spec(&self) -> AdvancedFormMetricSpec {
        let font = self.fonts.get(self.selected_font_idx);
        AdvancedFormMetricSpec {
            content: self.advanced_form_font_content().cloned(),
            // The FACE index is the panel's selection, not the content's representative
            // face: one file can hold several faces and the user picked one of them.
            face_index: font
                .and_then(|font| font.faces.get(self.selected_face_idx))
                .map_or(0, |face| face.face_index),
            bundled_stack: font.is_some_and(|font| font.bundled_stack_font().is_some()),
            path: font.map(|font| font.path.clone()).unwrap_or_default(),
            display_label: font
                .map(|font| font.display_label().to_string())
                .unwrap_or_default(),
            force_bold: self.force_bold,
            faux_bold: self.faux_bold,
            force_italic: self.force_italic,
            faux_italic: self.faux_italic,
            hanging_punctuation: self.hanging_punctuation,
        }
    }

    /// Строит попиксельную метрику ширины (`GlyphWidths`) выбранным шрифтом для
    /// символов `source_text`. `None`, если шрифт не выбран или его байты ещё не
    /// пришли/не читаются — тогда падаем на посимвольную метрику.
    ///
    /// Тонкая обёртка над [`build_advanced_form_glyph_widths_from_spec`]. Продовый
    /// путь строит метрику в фоновом воркере поиска прямо из снимка
    /// [`AdvancedFormMetricSpec`], поэтому обёртка существует ТОЛЬКО ради тестов
    /// метрики, которые формулируют условие через состояние панели.
    #[cfg(test)]
    pub(super) fn build_advanced_form_glyph_widths(
        &self,
        source_text: &str,
    ) -> Option<forms::GlyphWidths> {
        build_advanced_form_glyph_widths_from_spec(&self.advanced_form_metric_spec(), source_text)
    }
}

/// Строит попиксельную метрику ширины по снимку [`AdvancedFormMetricSpec`].
///
/// `None`, если байт шрифта нет или фейс не загрузился — вызывающий тогда мерит
/// посимвольно. Чтения файла здесь НЕТ (байты уже разрешены), но разбор фейса,
/// сборка `fontdb` и шейпинг алфавита — работа для фонового потока; вызывать её
/// на GUI-потоке нельзя нигде, кроме тестов.
fn build_advanced_form_glyph_widths_from_spec(
    spec: &AdvancedFormMetricSpec,
    source_text: &str,
) -> Option<forms::GlyphWidths> {
    // Единицы на em для замеров (должно совпадать с метрикой внутри forms).
    const METRIC_EM: f32 = 1000.0;
    let content = spec.content.as_ref()?;
    let face_index = spec.face_index;
    let path = spec.path.as_path();
    // Лёгкая система шрифтов: пустая БД + только нужный файл (без системных шрифтов).
    let mut font_system =
        FontSystem::new_with_locale_and_db("en-US".to_string(), fontdb::Database::new());
    // One-shot throwaway system: use a fresh, empty cache. This path is not
    // pooled (it deliberately avoids the system-font scan for metric-only
    // measurement), so the cache only satisfies the load API.
    let mut font_cache = FontFaceCache::new();
    let selected_face =
        load_font_content(&mut font_system, &mut font_cache, content, face_index).ok()?;
    let mut attrs = Attrs::new().metrics(Metrics::new(METRIC_EM, METRIC_EM));
    attrs = selected_face.apply_to_attrs(attrs);
    // The metric must measure the SAME face the renderer draws, so the
    // real-face request is gated exactly like the renderer's
    // `ms_text_render::pipeline::base_attrs_real_bold_italic`, AND by what this
    // database can actually provide: it holds only the selected font FILE, and
    // cosmic-text treats style as a hard `Attrs::matches` filter, so requesting a
    // face the file does not contain would leave the fallback iterator empty and
    // panic (`shape.rs`: `expect("no default font found")`).
    let wants_real_bold = wants_metric_real_face(spec.force_bold, spec.faux_bold);
    let wants_real_italic = wants_metric_real_face(spec.force_italic, spec.faux_italic);
    let available = metric_real_face_availability(
        font_system.db(),
        attrs.style,
        attrs.stretch,
        wants_real_italic,
    );
    if wants_real_italic && !available.italic {
        crate::runtime_log::log_warn(format!(
            "typing advanced forms: real Italic face requested for font '{}', but the font \
             file provides no Italic face; measuring the selected face instead (the \
             enumerated forms use upright widths). Path: {} Face index: {face_index}",
            spec.display_label,
            path.display()
        ));
    }
    if wants_real_bold && !available.bold {
        crate::runtime_log::log_warn(format!(
            "typing advanced forms: real Bold face requested for font '{}', but the font \
             file provides no Bold face at the requested style; measuring the selected face \
             instead. Path: {} Face index: {face_index}",
            spec.display_label,
            path.display()
        ));
    }
    // The built-in entry stands for the WHOLE bundled chain, not for the one `core`
    // file it points at, so measuring only that file would size CJK / Arabic forms
    // against `.notdef` boxes of Noto Sans while the renderer draws them with real
    // advances. Registered AFTER `metric_real_face_availability`, which must keep
    // seeing ONLY the selected file: it decides whether a real Bold/Italic face may be
    // requested, and a chain face must never make an unsatisfiable request look
    // satisfiable.
    if spec.bundled_stack {
        register_bundled_core_fallback(font_system.db_mut(), path);
    }
    attrs = apply_metric_real_bold_italic(
        attrs,
        spec.force_bold,
        spec.faux_bold,
        spec.force_italic,
        spec.faux_italic,
        available,
    );
    Some(forms::GlyphWidths::build(
        &mut font_system,
        &attrs,
        source_text,
        spec.hanging_punctuation,
        forms::DEFAULT_WIDTH_TOLERANCE,
    ))
}

/// Высота строки текста В ДОЛЯХ EM — множитель, которым окно форм переводит
/// высоту строки в единицы ЛЮБОЙ метрики ширин
/// (`forms::FormSearchParams::line_height_units` = `units_per_em × это`).
///
/// Зеркало `ms_text_render::pipeline` (`pipeline.rs:432-437`) вместе с его
/// `effective_spacing_percent` (`pipeline.rs:2628-2630`), которая внутри крейта
/// `pub(crate)` и потому воспроизведена здесь дословно:
///
/// ```text
/// spacing%       = clamp(line_spacing_percent + (glyph_height_percent − 100), −300, 300)
/// line_height_px = max(font_size_px + line_spacing_px + font_size_px·spacing%/100, 1)
/// em             = line_height_px / font_size_px / (glyph_width_percent / 100)
/// ```
///
/// ГОРИЗОНТАЛЬНЫЙ масштаб глифов обязан входить в делитель: ширины метрики
/// меряются БЕЗ него, поэтому иначе потолок пропорции формы молча разъехался бы
/// с тем, что видит пользователь.
///
/// Все проценты — в тех же единицах, что одноимённые поля `TextRenderParams`
/// (`create_apply::build_render_params_for`): `100` = натуральный масштаб.
/// Результат ВСЕГДА конечен и строго положителен ([`DEFAULT_LINE_HEIGHT_EM`] при
/// вырожденном входе) — он попадает в ключ поиска, а `NaN` там сделал бы ключ
/// не равным самому себе и зациклил перезапуск перебора.
#[must_use]
pub(super) fn advanced_form_line_height_em(
    font_size_px: f32,
    line_spacing_px: f32,
    line_spacing_percent: f32,
    glyph_height_percent: f32,
    glyph_width_percent: f32,
) -> f32 {
    let font_size_px = if font_size_px.is_finite() {
        font_size_px.max(1.0)
    } else {
        1.0
    };
    let line_spacing_px = if line_spacing_px.is_finite() {
        line_spacing_px
    } else {
        0.0
    };
    // `f32::clamp` пропускает `NaN` насквозь, поэтому нечисловая сумма снимается
    // сразу после него, а не до (порядок важен: до клампа она ещё не сложена).
    let spacing_percent =
        (line_spacing_percent + (glyph_height_percent - 100.0)).clamp(-300.0, 300.0);
    let spacing_percent = if spacing_percent.is_finite() {
        spacing_percent
    } else {
        0.0
    };
    let line_height_px =
        (font_size_px + line_spacing_px + font_size_px * (spacing_percent / 100.0)).max(1.0);
    let width_scale = glyph_width_percent / 100.0;
    // Нулевой, отрицательный или нечисловой горизонтальный масштаб обнулил бы
    // делитель; при нём вырождены и сами ширины, так что берём натуральный.
    let width_scale = if width_scale.is_finite() && width_scale > 0.0 {
        width_scale
    } else {
        1.0
    };
    let em = line_height_px / font_size_px / width_scale;
    if em.is_finite() && em > 0.0 {
        em
    } else {
        DEFAULT_LINE_HEIGHT_EM
    }
}

/// Снимок процесс-глобальных ручок поиска, прижатый к поддерживаемым диапазонам.
///
/// Прижатие здесь — не дубль `to_search_params`: ключ поиска сравнивается по
/// ЭТИМ значениям, и неприжатое (в пределе — `NaN` из руками правленого конфига)
/// значение сделало бы ключ не равным самому себе.
#[must_use]
fn advanced_form_knobs() -> AdvancedFormParams {
    let mut knobs = advanced_form_params();
    knobs.clamp_to_supported_range();
    knobs
}

/// Ручки, влияющие только на порядок показа карточек.
#[must_use]
fn advanced_form_order_key(knobs: &AdvancedFormParams) -> AdvancedFormOrderKey {
    AdvancedFormOrderKey {
        quality_floor_milli: knobs.quality_floor_milli(),
        narrow_slots: knobs.narrow_slots,
    }
}

/// Выполняет поиск форм целиком: строит метрику ширины и запускает
/// `forms::search_forms`. Вызывается ТОЛЬКО из фонового воркера.
///
/// Единицы высоты строки собираются здесь, а не у вызывающего, потому что
/// масштаб задаёт та метрика, которую удалось построить: 1/1000 em у
/// `GlyphWidths` и ~2 символа на em у запасной `CharWidthMetric`.
#[must_use]
fn run_advanced_form_search(
    key: &AdvancedFormSearchKey,
    spec: &AdvancedFormMetricSpec,
    knobs: AdvancedFormParams,
) -> AdvancedFormSearchResult {
    let glyph_widths = build_advanced_form_glyph_widths_from_spec(spec, &key.base.source_text);
    let char_metric = forms::CharWidthMetric::new(spec.hanging_punctuation);
    let (metric, units_per_em): (&dyn forms::LineWidthMetric, f32) = match glyph_widths.as_ref() {
        Some(glyph_widths) => (glyph_widths, GLYPH_METRIC_UNITS_PER_EM),
        None => (&char_metric, CHAR_METRIC_UNITS_PER_EM),
    };
    let params = knobs.to_search_params(
        key.base.line_height_em * units_per_em,
        key.line_range,
        key.width_range,
    );
    let enumeration = forms::search_forms(
        &key.base.source_text,
        key.base.preset,
        metric,
        &params,
    );
    AdvancedFormSearchResult {
        forms: enumeration.forms,
        truncated: enumeration.truncated,
    }
}

/// Собирает кэш окна из результата поиска и текущих ручек показа.
///
/// `carried_bounds` — границы диапазонных фильтров предыдущего кэша. Они
/// переносятся, когда прогон был СУЖЕН этими же фильтрами (`key.line_range` /
/// `key.width_range` заданы): наблюдения такого прогона описывают лишь
/// запрошенное окно, и взять их за границы значило бы запереть пользователя в
/// им же выбранном сужении без возможности расширить его обратно.
#[must_use]
pub(super) fn build_advanced_form_cache(
    key: AdvancedFormSearchKey,
    result: AdvancedFormSearchResult,
    knobs: &AdvancedFormParams,
    carried_bounds: Option<((usize, usize), (u32, u32))>,
) -> AdvancedFormCache {
    let forms = order_advanced_forms(result.forms.clone(), knobs);

    let mut group_counts: Vec<usize> = forms.iter().map(|form| form.word_break_count).collect();
    group_counts.sort_unstable();
    group_counts.dedup();

    // Пустой набор ничего не наблюдает: `inclusive_bounds` отдал бы `(0, 0)`, а
    // это не «границы данных», а «данных нет».
    let observed = (!forms.is_empty()).then(|| {
        (
            inclusive_bounds(forms.iter().map(TextForm::line_count)),
            inclusive_bounds(forms.iter().map(|form| form.max_width)),
        )
    });
    let carried_lines = key.line_range.and(carried_bounds).map(|bounds| bounds.0);
    let carried_widths = key.width_range.and(carried_bounds).map(|bounds| bounds.1);
    let line_bounds = merge_advanced_form_bounds(carried_lines, observed.map(|bounds| bounds.0));
    let width_bounds = merge_advanced_form_bounds(carried_widths, observed.map(|bounds| bounds.1));

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
    let uneven_max_bound = forms
        .iter()
        .map(|form| form.unevenness_pct)
        .max()
        .unwrap_or(0);
    let conservatism_bound = forms
        .iter()
        .map(|form| form.conservatism)
        .max()
        .unwrap_or(Conservatism::Safe);

    AdvancedFormCache {
        key,
        searched_forms: result.forms,
        forms,
        order_key: advanced_form_order_key(knobs),
        group_counts,
        line_bounds,
        width_bounds,
        peak_max_bound_min,
        peak_max_bound_median,
        uneven_max_bound,
        conservatism_bound,
        truncated: result.truncated,
    }
}

/// Границы диапазонного фильтра: перенесённые расширяются наблюдёнными, а при
/// отсутствии тех и других остаются схлопнутыми (`advanced_form_range_row` тогда
/// строку фильтра не рисует).
#[must_use]
fn merge_advanced_form_bounds<T: Ord + Copy + Default>(
    carried: Option<(T, T)>,
    observed: Option<(T, T)>,
) -> (T, T) {
    match (carried, observed) {
        (Some(carried), Some(observed)) => (
            carried.0.min(observed.0),
            carried.1.max(observed.1),
        ),
        (Some(bounds), None) | (None, Some(bounds)) => bounds,
        (None, None) => (T::default(), T::default()),
    }
}

/// Порядковый барьер отложенных записей ручек поиска форм.
///
/// Каждая правка ручек порождает СВОЙ поток записи, а общий замок
/// `config::lock_user_config_write()` упорядочивает записи, но не спасает от
/// ИНВЕРСИИ: два потока, стартовавшие в порядке «старое, новое», вправе взять
/// замок в порядке «новое, старое», и на диск ляжет устаревший снимок — ручка
/// молча откатилась бы при следующем запуске.
///
/// Барьер выдаёт монотонный номер поколения на КАЖДЫЙ старт записи
/// ([`AdvancedFormParamsSaveGate::claim`]) и пропускает к диску только поток с
/// НАИБОЛЬШИМ выданным номером ([`AdvancedFormParamsSaveGate::write_if_current`]),
/// причём проверка и сама запись идут под ОДНИМ замком барьера — иначе устаревший
/// поток мог бы проскочить между проверкой и записью более свежего.
pub(super) struct AdvancedFormParamsSaveGate {
    /// Последнее выданное поколение; `0` — не выдано ни одного.
    latest: AtomicU64,
    /// Держится на всё время «проверка + запись». Внутри него берётся
    /// `config::lock_user_config_write()`, и НИКОГДА наоборот — обратного порядка
    /// в проекте нет, поэтому пара замков не образует цикла.
    write_lock: Mutex<()>,
}

impl AdvancedFormParamsSaveGate {
    pub(super) const fn new() -> Self {
        Self {
            latest: AtomicU64::new(0),
            write_lock: Mutex::new(()),
        }
    }

    /// Регистрирует новую запись и отдаёт её поколение (строго новее всех ранее
    /// выданных).
    pub(super) fn claim(&self) -> u64 {
        self.latest.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    /// Отзывает поколение записи, которая так и НЕ стартовала (поток не создался).
    ///
    /// Без отзыва несостоявшаяся запись навсегда объявила бы себя «самой свежей», и
    /// уже запущенная ПРЕДЫДУЩАЯ запись отказалась бы писать — на диск не легло бы
    /// вообще ничего. Если поколение уже перекрыто ещё более свежим, отзывать
    /// нечего, и вызов ничего не делает.
    pub(super) fn release(&self, generation: u64) {
        // `Err` = нас уже перекрыли; победитель новее, и он же и запишет.
        let _ = self.latest.compare_exchange(
            generation,
            generation.wrapping_sub(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Выполняет `write`, только если `generation` всё ещё самое новое; иначе
    /// запись отменяется (её уже перекрыла более свежая) и возвращается `false`.
    ///
    /// Отравленный замок восстанавливается: полезной нагрузки у него нет, а
    /// потеря сохранения хуже, чем работа после чужой паники.
    pub(super) fn write_if_current(&self, generation: u64, write: impl FnOnce()) -> bool {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.latest.load(Ordering::SeqCst) != generation {
            return false;
        }
        write();
        true
    }
}

/// Процесс-глобальный барьер записи ручек поиска форм.
static ADVANCED_FORM_PARAMS_SAVE_GATE: AdvancedFormParamsSaveGate =
    AdvancedFormParamsSaveGate::new();

/// Пишет ручки поиска форм в `user_config.json` на именованном фоновом потоке —
/// той же схемой, что `settings::draw_rotation_ctrl_wheel_setting`.
///
/// Путь конфига вычисляется ВНУТРИ воркера: `config::user_config_path` щупает
/// файловую систему, а вызывающий — GUI-поток. Ошибка записи только логируется:
/// значение уже применено к процесс-глобальному состоянию, поэтому сессия
/// работает как ни в чём не бывало и терять кадр на всплывающее сообщение не за что.
///
/// Запись ПОРЯДКОВО БЕЗОПАСНА: поколение выдаётся здесь, в момент старта, а поток
/// сверяется с ним под замком [`ADVANCED_FORM_PARAMS_SAVE_GATE`] — перекрытый
/// снимок на диск не попадает (см. [`AdvancedFormParamsSaveGate`]).
fn persist_advanced_form_search_params(params: AdvancedFormParams) {
    let generation = ADVANCED_FORM_PARAMS_SAVE_GATE.claim();
    if let Err(error) = thread::Builder::new()
        .name("typing-form-search-params-save".to_string())
        .spawn(move || {
            let path = config::user_config_path();
            // Возвращённый `false` — не ошибка: этот снимок перекрыт более
            // свежим, который и лежит (или ляжет) на диске.
            ADVANCED_FORM_PARAMS_SAVE_GATE.write_if_current(generation, || {
                if let Err(error) =
                    crate::tabs::settings::save_advanced_form_search_params(&path, params)
                {
                    crate::runtime_log::log_error(format!(
                        "typing advanced forms: failed to persist the search parameters. \
                         Path: {} Error: {error}",
                        path.display()
                    ));
                }
            });
        })
    {
        // Поток не стартовал — снимаем своё поколение, иначе уже запущенная
        // предыдущая запись сочла бы себя перекрытой и не записала бы ничего.
        ADVANCED_FORM_PARAMS_SAVE_GATE.release(generation);
        crate::runtime_log::log_error(format!(
            "typing advanced forms: failed to start the search-parameters save thread; the \
             values stay in effect for this session only. Error: {error}"
        ));
    }
}

impl TypingCreatePanelState {
    /// Всё, от чего зависит НАБОР найденных форм, кроме диапазонов фильтров окна.
    ///
    /// `knobs` передаются, а не читаются здесь: вызывающий берёт снимок
    /// процесс-глобальных ручек ОДИН раз за кадр и строит по нему и базу, и ключ,
    /// иначе два чтения могли бы разойтись правкой из другого потока.
    /// [`AdvancedFormParams::filters_prune`] в базу не входит намеренно — см.
    /// [`AdvancedFormSearchBase`].
    pub(super) fn advanced_form_search_base(
        &self,
        knobs: &AdvancedFormParams,
    ) -> AdvancedFormSearchBase {
        // Те же величины и в тех же единицах, что уходят в рендер
        // (`create_apply::build_render_params_for`) — иначе потолок пропорции
        // формы описывал бы не тот текст, который увидит пользователь.
        let font_size_px = self.font_size_px.max(1.0);
        let (line_spacing_px, line_spacing_percent) = self.line_spacing.as_px_percent();
        AdvancedFormSearchBase {
            source_text: self.advanced_form_source_text(),
            preset: self.advanced_form_preset,
            metric: self.advanced_form_metric_signature(),
            evenness: knobs.evenness,
            aspect_max: knobs.aspect_max,
            hyphen_ratio: knobs.hyphen_ratio,
            hyphen_relax_slack: knobs.hyphen_relax_slack,
            per_bucket: knobs.per_bucket,
            line_height_em: advanced_form_line_height_em(
                font_size_px,
                line_spacing_px,
                line_spacing_percent,
                self.glyph_height.as_percent_of(font_size_px),
                self.glyph_width.as_percent_of(font_size_px),
            ),
        }
    }

    /// База набора форм, который окно ПОКАЗЫВАЕТ прямо сейчас; `None`, пока не
    /// показан ни один.
    ///
    /// Именно с ней сравнивается текущая база, а не с той, что уже считается или
    /// ждёт debounce: диапазонные фильтры описывают ПОКАЗАННЫЙ набор, и пока он
    /// не относится к текущему входу, их полагается держать раскрытыми. Сброс от
    /// этого идемпотентен и повторяется каждый кадр набора текста — это и есть
    /// правильное поведение, а не издержка.
    fn advanced_form_shown_search_base(&self) -> Option<&AdvancedFormSearchBase> {
        self.advanced_form_cache.as_ref().map(|cache| &cache.key.base)
    }

    /// Идёт ли прямо сейчас пересчёт форм (ожидание байт метрики, debounce или
    /// сам перебор). Пока он идёт, окно рисует ПРЕЖНИЙ результат и сообщает, что
    /// он устарел.
    fn advanced_form_search_in_progress(&self) -> bool {
        self.advanced_form_font_request.is_some()
            || self.advanced_form_search_debounce.is_some()
            || self.advanced_form_search.is_some()
    }

    /// Принимает готовый результат фонового поиска. Раз за кадр, ДО планирования
    /// нового: только что пришедший результат может оказаться ровно тем, что
    /// нужно, и тогда планировать нечего.
    fn poll_advanced_form_search(&mut self, ctx: &egui::Context) {
        let outcome = match self.advanced_form_search.as_ref() {
            Some(job) => job.rx.try_recv(),
            None => return,
        };
        match outcome {
            Ok(result) => {
                let Some(job) = self.advanced_form_search.take() else {
                    return;
                };
                // Клон, а не перемещение: у задачи есть `Drop` (он и отменяет
                // предыдущую), поэтому её поля частично не вынимаются. Копия
                // ключа стоит одну строку текста и делается раз на поиск.
                self.install_advanced_form_search_result(
                    job.key.clone(),
                    job.reset_display_filters,
                    result,
                );
            }
            Err(TryRecvError::Empty) => {
                // Воркер не планирует кадров сам.
                ctx.request_repaint();
            }
            Err(TryRecvError::Disconnected) => {
                let Some(job) = self.advanced_form_search.take() else {
                    return;
                };
                crate::runtime_log::log_error(
                    "typing advanced forms: the background form search ended without a result; \
                     the window shows an empty set until the input changes",
                );
                // Пустой результат ПОД ЭТИМ ЖЕ ключом: иначе следующий кадр
                // запустил бы ту же задачу заново и окно закольцевалось бы на
                // падающем воркере.
                self.install_advanced_form_search_result(
                    job.key.clone(),
                    job.reset_display_filters,
                    AdvancedFormSearchResult {
                        forms: Vec::new(),
                        truncated: false,
                    },
                );
            }
        }
    }

    /// Ставит результат поиска в кэш и приводит к нему фильтры показа.
    fn install_advanced_form_search_result(
        &mut self,
        key: AdvancedFormSearchKey,
        reset_display_filters: bool,
        result: AdvancedFormSearchResult,
    ) {
        let knobs = advanced_form_knobs();
        let carried_bounds = self
            .advanced_form_cache
            .as_ref()
            .map(|cache| (cache.line_bounds, cache.width_bounds));
        let cache = build_advanced_form_cache(key, result, &knobs, carried_bounds);
        if reset_display_filters {
            // Пороги пиковости и неравномерности — на максимум (показываем всё).
            self.advanced_form_peak_max = match self.advanced_form_peak_base {
                PeakBase::Min => cache.peak_max_bound_min,
                PeakBase::Median => cache.peak_max_bound_median,
            };
            self.advanced_form_uneven_max = cache.uneven_max_bound;
            // Консервативность по умолчанию строгая (`Safe`): показываем только
            // формы без отрыва служебных слов. Пользователь ослабляет вручную.
            self.advanced_form_conservatism_max = Conservatism::Safe;
            self.advanced_form_group = None;
        }
        // Выбранного числа переносов может не быть в новом наборе — и тогда
        // фильтр показывал бы пустую сетку без единой подсказки почему.
        if let Some(selected) = self.advanced_form_group
            && !cache.group_counts.contains(&selected)
        {
            self.advanced_form_group = None;
        }
        self.advanced_form_cache = Some(cache);
    }

    /// Планирует перебор, когда вход изменился: сбрасывает диапазоны при смене
    /// базы, выдерживает debounce и запускает воркер, отменяя предыдущий.
    ///
    /// Переключение `filters_prune` базой НЕ является: оба диапазона переживают
    /// его в любую сторону, меняется лишь то, попадают ли они в ключ (то есть в
    /// перебор) или остаются фильтром показа.
    pub(super) fn schedule_advanced_form_search(&mut self, ctx: &egui::Context) {
        // Один снимок ручек на кадр: база и ключ обязаны быть согласованы.
        let knobs = advanced_form_knobs();
        let base = self.advanced_form_search_base(&knobs);
        // Смена базы — это другой набор форм: сужённые под прошлый набор
        // диапазоны не должны ни сужать первый прогон нового текста, ни держать
        // фильтр в пустом окне.
        if self.advanced_form_shown_search_base() != Some(&base) {
            self.advanced_form_line_range = None;
            self.advanced_form_width_range = None;
        }
        let key = AdvancedFormSearchKey {
            line_range: knobs
                .filters_prune
                .then_some(self.advanced_form_line_range)
                .flatten(),
            width_range: knobs
                .filters_prune
                .then_some(self.advanced_form_width_range)
                .flatten(),
            base,
        };
        if self
            .advanced_form_cache
            .as_ref()
            .is_some_and(|cache| cache.key == key)
        {
            self.advanced_form_search_debounce = None;
            // Показанное уже отвечает текущему входу. Задача, запущенная ради
            // входа, от которого пользователь успел откатиться, теперь только
            // перезаписала бы кэш устаревшим набором — роняем её (`Drop` взводит
            // отмену).
            self.advanced_form_search = None;
            return;
        }
        if self
            .advanced_form_search
            .as_ref()
            .is_some_and(|job| job.key == key)
        {
            self.advanced_form_search_debounce = None;
            return;
        }
        // Ждём байты метрики: их приход меняет `font_content_id`, то есть базу
        // ключа, и перебор пришлось бы повторить целиком — два полных прохода на
        // каждое открытие окна.
        if self.advanced_form_font_request.is_some() {
            ctx.request_repaint();
            return;
        }
        let now = Instant::now();
        let restart = self
            .advanced_form_search_debounce
            .as_ref()
            .is_none_or(|(pending, _)| *pending != key);
        if restart {
            self.advanced_form_search_debounce = Some((key.clone(), now));
        }
        let Some((_, since)) = self.advanced_form_search_debounce.as_ref() else {
            return;
        };
        let elapsed = now.saturating_duration_since(*since);
        if elapsed < ADVANCED_FORM_SEARCH_DEBOUNCE {
            // Ни одно другое событие не разбудит окно: ввод уже обработан.
            ctx.request_repaint_after(ADVANCED_FORM_SEARCH_DEBOUNCE - elapsed);
            return;
        }
        self.advanced_form_search_debounce = None;
        let reset_display_filters = self
            .advanced_form_cache
            .as_ref()
            .is_none_or(|cache| cache.key.base != key.base);
        self.spawn_advanced_form_search(ctx, key, knobs, reset_display_filters);
    }

    /// Запускает фоновый поиск форм, отменяя предыдущий.
    ///
    /// `knobs` — ТОТ ЖЕ снимок ручек, по которому собран `key`: воркер решает по
    /// `filters_prune`, пускать ли диапазоны ключа в перебор, и второе чтение
    /// процесс-глобального значения могло бы разойтись с ключом.
    ///
    /// Отмена не требует явного вызова: присвоение поля роняет прежнюю задачу, а
    /// её `Drop` взводит общий с воркером флаг.
    fn spawn_advanced_form_search(
        &mut self,
        ctx: &egui::Context,
        key: AdvancedFormSearchKey,
        knobs: AdvancedFormParams,
        reset_display_filters: bool,
    ) {
        let spec = self.advanced_form_metric_spec();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_key = key.clone();
        let (tx, rx) = mpsc::channel::<AdvancedFormSearchResult>();
        match thread::Builder::new()
            .name("typing-form-search".to_string())
            .spawn(move || {
                // Задачу могли заменить, пока поток стартовал.
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let result = run_advanced_form_search(&worker_key, &spec, knobs);
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                // Неудачная отправка значит лишь, что окно закрыли или вход
                // сменился, пока шёл перебор.
                let _ = tx.send(result);
            }) {
            Ok(_handle) => {
                self.advanced_form_search = Some(AdvancedFormSearchJob {
                    key,
                    cancel,
                    rx,
                    reset_display_filters,
                });
                ctx.request_repaint();
            }
            Err(error) => {
                crate::runtime_log::log_error(format!(
                    "typing advanced forms: failed to spawn the form search worker; the window \
                     shows no variants for this input. Error: {error}"
                ));
                // Пустой результат под этим ключом — иначе окно пыталось бы
                // стартовать поток каждый кадр.
                self.advanced_form_search = None;
                self.install_advanced_form_search_result(
                    key,
                    reset_display_filters,
                    AdvancedFormSearchResult {
                        forms: Vec::new(),
                        truncated: false,
                    },
                );
            }
        }
    }

    /// Пересобирает ПОРЯДОК показа, когда изменились только его ручки (порог
    /// качества, приоритет узких форм). Перебор при этом не повторяется:
    /// пересортировка нескольких сотен карточек стоит доли миллисекунды, а
    /// перебор — десятки.
    fn reorder_advanced_form_cache_if_needed(&mut self) {
        let knobs = advanced_form_knobs();
        let order_key = advanced_form_order_key(&knobs);
        if self
            .advanced_form_cache
            .as_ref()
            .is_none_or(|cache| cache.order_key == order_key)
        {
            return;
        }
        let Some(cache) = self.advanced_form_cache.take() else {
            return;
        };
        let carried_bounds = Some((cache.line_bounds, cache.width_bounds));
        let result = AdvancedFormSearchResult {
            forms: cache.searched_forms,
            truncated: cache.truncated,
        };
        self.advanced_form_cache = Some(build_advanced_form_cache(
            cache.key,
            result,
            &knobs,
            carried_bounds,
        ));
    }

    /// Дописывает ручки поиска на диск, когда пользователь перестал их крутить.
    fn poll_advanced_form_params_save(&mut self, ctx: &egui::Context) {
        let Some(since) = self.advanced_form_params_save_pending else {
            return;
        };
        let elapsed = Instant::now().saturating_duration_since(since);
        if elapsed < ADVANCED_FORM_PARAMS_SAVE_DEBOUNCE {
            ctx.request_repaint_after(ADVANCED_FORM_PARAMS_SAVE_DEBOUNCE - elapsed);
            return;
        }
        self.flush_advanced_form_params_save();
    }

    /// Немедленно дописывает отложенную правку ручек поиска, если она есть.
    /// Зовётся при закрытии окна, чтобы правка «за секунду до закрытия» не
    /// пропала вместе с окном.
    fn flush_advanced_form_params_save(&mut self) {
        if self.advanced_form_params_save_pending.take().is_some() {
            persist_advanced_form_search_params(advanced_form_knobs());
        }
    }

    /// Секция «Параметры поиска» окна форм (план §3c) — свёрнута по умолчанию.
    ///
    /// Каждый контрол привязан к константам `*_MIN`/`*_MAX` модуля
    /// `advanced_form_params`, поэтому предложить значение, которое поиск
    /// откажется принять, физически нельзя. Правка применяется к
    /// процесс-глобальному значению СРАЗУ (следующий кадр запланирует перебор
    /// через общий debounce), а запись на диск откладывается
    /// [`ADVANCED_FORM_PARAMS_SAVE_DEBOUNCE`].
    fn draw_advanced_form_search_params_section(&mut self, ui: &mut egui::Ui) {
        let preview_enabled = self.preview_enabled;
        let mut edited: Option<AdvancedFormParams> = None;
        collapsing_param_section(
            ui,
            "typing.advanced.form_search_section",
            preview_enabled,
            t!("typing.advanced.form_search_section"),
            false,
            None,
            |ui| {
                let before = advanced_form_knobs();
                let mut knobs = before;
                ui.add(
                    WheelSlider::new(&mut knobs.evenness, EVENNESS_MIN..=EVENNESS_MAX)
                        .text(t!("typing.advanced.form_search_evenness_label"))
                        .fixed_decimals(2)
                        .wheel_step(0.05),
                )
                .on_hover_text(t!("typing.advanced.form_search_evenness_tooltip"));
                ui.add(
                    WheelSlider::new(&mut knobs.aspect_max, ASPECT_MAX_MIN..=ASPECT_MAX_MAX)
                        .text(t!("typing.advanced.form_search_aspect_label"))
                        .fixed_decimals(2)
                        .wheel_step(0.05),
                )
                .on_hover_text(t!("typing.advanced.form_search_aspect_tooltip"));
                ui.add(
                    WheelSlider::new(&mut knobs.hyphen_ratio, HYPHEN_RATIO_MIN..=HYPHEN_RATIO_MAX)
                        .text(t!("typing.advanced.form_search_hyphen_ratio_label"))
                        // Хранится долей, показывается процентом — как и всё
                        // остальное «сколько строк» в панели.
                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                        .wheel_step(0.05),
                )
                .on_hover_text(t!("typing.advanced.form_search_hyphen_ratio_tooltip"));
                ui.add(
                    WheelSlider::new(
                        &mut knobs.hyphen_relax_slack,
                        HYPHEN_RELAX_SLACK_MIN..=HYPHEN_RELAX_SLACK_MAX,
                    )
                    .text(t!("typing.advanced.form_search_hyphen_relax_label"))
                    .fixed_decimals(2)
                    .wheel_step(0.05),
                )
                .on_hover_text(t!("typing.advanced.form_search_hyphen_relax_tooltip"));
                ui.add(
                    WheelSlider::new(&mut knobs.quality_floor, QUALITY_FLOOR_MIN..=QUALITY_FLOOR_MAX)
                        .text(t!("typing.advanced.form_search_quality_floor_label"))
                        .fixed_decimals(2)
                        .wheel_step(0.05),
                )
                .on_hover_text(t!("typing.advanced.form_search_quality_floor_tooltip"));
                ui.add(
                    WheelSlider::new(&mut knobs.per_bucket, PER_BUCKET_MIN..=PER_BUCKET_MAX)
                        .text(t!("typing.advanced.form_search_per_bucket_label")),
                )
                .on_hover_text(t!("typing.advanced.form_search_per_bucket_tooltip"));
                ui.add(
                    WheelSlider::new(&mut knobs.narrow_slots, NARROW_SLOTS_MIN..=NARROW_SLOTS_MAX)
                        .text(t!("typing.advanced.form_search_narrow_bias_label")),
                )
                .on_hover_text(t!("typing.advanced.form_search_narrow_bias_tooltip"));
                ui.checkbox(
                    &mut knobs.filters_prune,
                    t!("typing.advanced.form_search_filters_prune_checkbox"),
                )
                .on_hover_text(t!("typing.advanced.form_search_filters_prune_tooltip"));
                if ui
                    .small_button(t!("typing.advanced.form_search_reset_button"))
                    .clicked()
                {
                    knobs = AdvancedFormParams::default();
                }
                if knobs != before {
                    edited = Some(knobs);
                }
            },
        );
        if let Some(knobs) = edited {
            // Применяем сразу: следующий кадр увидит новый ключ поиска. Диск ждёт
            // паузы — слайдер отдаёт новое значение на каждом кадре перетаскивания.
            set_advanced_form_params(knobs);
            self.advanced_form_params_save_pending = Some(Instant::now());
        }
    }

    /// Применяет выбранную форму: записывает её как сформированный текст (исходный
    /// `text` не трогаем) и разворачивает сформированный пан.
    pub(super) fn apply_advanced_form(&mut self, form: &TextForm) {
        self.formed_text = form.to_text();
        self.advanced_text_show_formed = true;
        self.queue_preview_render();
    }

    /// Плавающее окно поиска форм текста.
    ///
    /// Состояние КАЖДОГО кадра, ровно в этом порядке:
    /// 1. `poll_advanced_form_font` — фоновый резолв байт метрики;
    /// 2. `poll_advanced_form_search` — приём готового результата перебора;
    /// 3. `schedule_advanced_form_search` — сравнение входа с ключом кэша,
    ///    debounce и запуск воркера (с отменой предыдущего);
    /// 4. `reorder_advanced_form_cache_if_needed` — только порядок показа;
    /// 5. `poll_advanced_form_params_save` — отложенная запись ручек на диск;
    /// 6. отрисовка ПОСЛЕДНЕГО известного результата (плюс строка «пересчёт»,
    ///    пока идут шаги 1–3) — пустая сетка не показывается никогда.
    pub(super) fn draw_advanced_form_window(&mut self, ctx: &egui::Context) -> bool {
        if !self.advanced_form_open {
            return false;
        }
        self.poll_advanced_form_font(ctx);
        self.poll_advanced_form_search(ctx);
        self.schedule_advanced_form_search(ctx);
        self.reorder_advanced_form_cache_if_needed();
        self.poll_advanced_form_params_save(ctx);
        let recomputing = self.advanced_form_search_in_progress();
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
            // Ручки перебора живут ЗДЕСЬ, а не в панели настроек: они бессмысленны
            // без сетки результатов перед глазами (план §3c).
            self.draw_advanced_form_search_params_section(ui);
            ui.separator();
            if recomputing {
                ui.small(t!("typing.advanced.form_recomputing_status"));
            }
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
                    // Диапазонные фильтры: число строк и ширина строки. `None` —
                    // «весь диапазон»; сужение записывается обратно только когда
                    // оно действительно уже границ, иначе полный диапазон снова и
                    // снова сужал бы перебор до им же наблюдённых границ.
                    let mut line_value = line_range.unwrap_or(cache.line_bounds);
                    let has_line = advanced_form_range_row(
                        ui,
                        t!("typing.advanced.form_lines_label"),
                        "",
                        &mut line_value,
                        cache.line_bounds,
                    );
                    line_range = (line_value != cache.line_bounds).then_some(line_value);
                    let mut width_value = width_range.unwrap_or(cache.width_bounds);
                    let has_width = advanced_form_range_row(
                        ui,
                        t!("typing.advanced.form_width_label"),
                        "",
                        &mut width_value,
                        cache.width_bounds,
                    );
                    width_range = (width_value != cache.width_bounds).then_some(width_value);
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
                        line_range = None;
                        width_range = None;
                        peak_max = peak_bound;
                        uneven_max = uneven_bound;
                        conservatism_max = Conservatism::Safe;
                        new_group = None;
                    }

                    let line_filter = line_range.unwrap_or(cache.line_bounds);
                    let width_filter = width_range.unwrap_or(cache.width_bounds);
                    let passes = |form: &TextForm| {
                        new_group.is_none_or(|c| form.word_break_count == c)
                            && (line_filter.0..=line_filter.1).contains(&form.line_count())
                            && (width_filter.0..=width_filter.1).contains(&form.max_width)
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
                // Первый перебор ещё идёт: строка «пересчёт» выше уже всё сказала,
                // а «введите текст» здесь было бы ложью.
                _ if recomputing => {}
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
        // Кэш возвращается на место ВСЕГДА: смена пресета его не выбрасывает —
        // пресет входит в ключ поиска, и прежний результат продолжает рисоваться,
        // пока новый не приедет.
        self.advanced_form_cache = cache;
        self.advanced_form_preset = new_preset;
        self.advanced_form_group = new_group;
        if !open {
            // Правку ручек, сделанную за мгновение до закрытия, дописываем сразу:
            // следующего кадра окна уже не будет.
            self.flush_advanced_form_params_save();
        }
        self.advanced_form_open = open;
        changed
    }
}

/// Adds the rest of the bundled `core` tier to the advanced-form metric's font database.
///
/// Only for the built-in interface font (`FontEntryKind::BundledUiStack`). That entry
/// points at `core[0]` as its selected face and gets everything else from the renderer's
/// `MsFallback::common_fallback`, which IS the `core` chain
/// (`dev-docs/unicode_base_font_plan.md`, layers 2 and 3). The metric's throwaway
/// `FontSystem` has no `MsFallback`, but cosmic-text's last fallback stage tries every
/// remaining face of the database (`cosmic-text-0.14.2/src/font/fallback/mod.rs:445-457`),
/// so registering the chain in `NN-` order reproduces the renderer's advances for the
/// scripts `core` covers.
///
/// `selected_path` is the file already registered as the selected face and is skipped, so
/// no face is duplicated.
///
/// Deliberately `core` ONLY. The `ext` tier is ~80 MB across ~44 files, and this database
/// is rebuilt from scratch on every form search (a font, style, text or knob change with
/// the advanced-form window open): registering `ext` would open and memory-map every one
/// of those files each time, just to read a `name` table. That the rebuild now happens on
/// a worker thread rather than on the GUI thread lowers the stakes but not the waste. The
/// `core` files, by contrast, cost no I/O at all here — their bytes are the process-
/// resident `'static` buffers `ms_fonts::bytes` already handed to the egui UI, so this is
/// four in-memory face parses. Residual, accepted divergence: a script only `ext` covers
/// is still measured as `.notdef`, exactly as it was before.
fn register_bundled_core_fallback(db: &mut fontdb::Database, selected_path: &Path) {
    let Some(stack) = ms_fonts::stack() else {
        crate::runtime_log::log_warn(
            "typing advanced forms: the built-in interface font is selected, but the bundled \
             fonts/ui stack cannot be resolved; the enumerated form widths are measured with \
             the single selected file and may be wrong for scripts it does not cover",
        );
        return;
    };
    for font in stack.core() {
        if font.path == selected_path {
            continue;
        }
        // A file whose bytes are unreadable is reported by `ms_fonts`; dropping it here
        // only costs the fidelity of the scripts that one file covered.
        let Some(bytes) = ms_fonts::bytes(font) else {
            continue;
        };
        let ids = db.load_font_source(fontdb::Source::Binary(std::sync::Arc::new(bytes)));
        if ids.is_empty() {
            crate::runtime_log::log_warn(format!(
                "typing advanced forms: the bundled core font '{}' yielded no face for the \
                 width metric; forms containing the scripts it covers are measured without \
                 it. Path: {}",
                font.family_name,
                font.path.display()
            ));
        }
    }
}

/// Whether the width metric should request the REAL Bold (resp. Italic) face.
///
/// The real face is wanted when the force flag is set WITHOUT its faux companion;
/// `faux` alone is ignored. Shared by `apply_metric_real_bold_italic` and its caller
/// (which logs the skipped requests) so the two can never drift apart.
#[must_use]
pub(super) const fn wants_metric_real_face(force: bool, faux: bool) -> bool {
    force && !faux
}

/// Which REAL faces the advanced-form metric's font database can actually provide.
///
/// Produced by `metric_real_face_availability` from the throwaway database that holds
/// ONLY the selected font FILE, and consumed by `apply_metric_real_bold_italic`.
#[derive(Debug, Clone, Copy)]
pub(super) struct MetricRealFaceAvailability {
    /// A face at `Weight::BOLD` exists among the faces the resolved style admits.
    pub(super) bold: bool,
    /// A face with `Style::Italic` (or an emoji face, which cosmic-text matches
    /// regardless of style) exists in the database.
    pub(super) italic: bool,
}

impl MetricRealFaceAvailability {
    /// Availability of everything — for tests of the pure flag gate, which must not
    /// depend on a font database.
    #[cfg(test)]
    pub(super) const ALL: Self = Self {
        bold: true,
        italic: true,
    };
}

/// Mirrors `cosmic_text::Attrs::matches` (cosmic-text 0.14.2, `attrs.rs:323-327`): a face
/// enters the match set only when its style AND stretch equal the request, except emoji
/// faces, which are admitted regardless. Weight is NOT a filter here — cosmic-text uses it
/// only to rank the faces that already passed this test (`font/system.rs:328`).
fn metric_face_matches_style(
    face: &fontdb::FaceInfo,
    style: cosmic_text::Style,
    stretch: cosmic_text::Stretch,
) -> bool {
    face.post_script_name.contains("Emoji") || (face.style == style && face.stretch == stretch)
}

/// Probes the metric's font database for the REAL Bold/Italic faces it can satisfy.
///
/// `db` is the throwaway database holding only the selected font file (possibly a
/// multi-face collection), which is exactly the set cosmic-text can match. `base_style`
/// and `stretch` are the selected face's own attributes, i.e. what the request falls back
/// to. `italic_requested` selects the style the weight probe runs under, because
/// cosmic-text filters by style/stretch FIRST and only then ranks by weight: a file whose
/// only Bold face is italic must not report Bold as available for an upright request.
///
/// Bold availability requires an EXACT `Weight::BOLD` face; a file whose heaviest face is,
/// say, Semibold reports `bold: false`, so the metric keeps the selected face rather than
/// letting cosmic-text silently rank its way onto a different weight.
#[must_use]
pub(super) fn metric_real_face_availability(
    db: &fontdb::Database,
    base_style: cosmic_text::Style,
    stretch: cosmic_text::Stretch,
    italic_requested: bool,
) -> MetricRealFaceAvailability {
    let italic = db
        .faces()
        .any(|face| metric_face_matches_style(face, cosmic_text::Style::Italic, stretch));
    // The style the request will actually carry decides which faces the weight search sees.
    let resolved_style = if italic_requested && italic {
        cosmic_text::Style::Italic
    } else {
        base_style
    };
    let bold = db.faces().any(|face| {
        metric_face_matches_style(face, resolved_style, stretch)
            && face.weight == cosmic_text::Weight::BOLD
    });
    MetricRealFaceAvailability { bold, italic }
}

/// Applies the advanced-form width metric's REAL Bold/Italic face request to `attrs`.
///
/// MIRRORS `ms_text_render::pipeline::base_attrs_real_bold_italic` and must not
/// drift from it: the real Bold (resp. Italic) face is requested ONLY when the
/// force flag is set WITHOUT its faux companion. With `force_* && faux_*` the
/// renderer keeps the selected Regular/upright face and synthesizes the style
/// geometrically at the glyph seam, so the metric has to measure that same face —
/// otherwise the enumerated forms would be sized against a face that is never
/// drawn. `faux_*` without `force_*` is ignored on both sides. Attributes the gate
/// does not touch (family, stretch, and the face's own weight/style) pass through
/// unchanged.
///
/// The request is ADDITIONALLY conditioned on `available`: a face the metric's font
/// database does not contain is never requested, and the selected face's attributes are
/// kept instead. Unlike the renderer's pooled `FontSystem`, this database holds only the
/// selected font file, and cosmic-text treats style as an exact `Attrs::matches` filter —
/// an unsatisfiable Italic request would leave the fallback iterator empty and panic
/// (`shape.rs`: `expect("no default font found")`). Callers report the skipped request;
/// this function only enforces it.
#[must_use]
pub(super) fn apply_metric_real_bold_italic<'a>(
    mut attrs: Attrs<'a>,
    force_bold: bool,
    faux_bold: bool,
    force_italic: bool,
    faux_italic: bool,
    available: MetricRealFaceAvailability,
) -> Attrs<'a> {
    if wants_metric_real_face(force_bold, faux_bold) && available.bold {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if wants_metric_real_face(force_italic, faux_italic) && available.italic {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }
    attrs
}
