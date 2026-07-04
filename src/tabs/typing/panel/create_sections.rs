/*
File: panel/create_sections.rs

Purpose:
Part of impl TypingCreatePanelState extracted verbatim from panel.rs.
Top-level section drawing for the create panel: preview, params, and
effects sections plus the right-side actions column (mask toggle, export,
image insert, clean-overlay visibility), together with the default effect
card factory and the effects JSON serializer.

Notes:
Extracted verbatim from panel.rs. Methods are pub(super) because
TypingCreatePanelState is used only inside panel.rs. use super::* pulls in
the parent module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {

    pub(super) fn draw_preview_section(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if self.render_in_flight || self.fonts_reload_in_flight {
                    ui.spinner();
                }
                let status_width = ui.available_width().max(1.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(status_width, 0.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_max_width(status_width);
                        ui.add(egui::Label::new(self.status_line.as_str()).wrap());
                    },
                );
            });

            ui.add_space(4.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                let box_size = egui::vec2(ui.available_width(), CREATE_PREVIEW_HEIGHT_PX);
                let (preview_rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
                if let Some(texture) = &self.preview_texture {
                    let image_size = fit_size_to_box(texture.size(), preview_rect.size());
                    let image_rect = Rect::from_center_size(preview_rect.center(), image_size);
                    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter()
                        .image(texture.id(), image_rect, uv, Color32::WHITE);
                } else {
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(preview_rect).layout(
                            egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        ),
                        |ui| {
                            ui.label("Превью ещё не готово.");
                        },
                    );
                }
            });
        });
    }

    pub(super) fn draw_params_section(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
    ) {
        let mut params_changed = false;
        params_changed |= self.draw_main_text_params(
            ui,
            stacked_columns,
            remap_wheel_to_horizontal,
            self.preview_enabled,
            // Панель создания всегда работает с доступным шрифтом.
            false,
        );

        if params_changed {
            self.sync_current_font_profile_memory();
            self.queue_preview_render();
        }
    }

    pub(super) fn draw_effects_section(&mut self, ui: &mut egui::Ui, vertical_cards: bool) -> bool {
        let mut changed = false;
        ui.label(if vertical_cards {
            "Порядок применения: сверху вниз"
        } else {
            "Порядок применения: слева направо"
        });
        ui.horizontal(|ui| {
            let effect_kinds = [
                AvailableEffectKind::TextShake,
                AvailableEffectKind::Stroke,
                AvailableEffectKind::Shadow,
                AvailableEffectKind::Blur,
                AvailableEffectKind::MotionBlur,
                AvailableEffectKind::DryMedia,
                AvailableEffectKind::GlowV1,
                AvailableEffectKind::GlowV2,
                AvailableEffectKind::SoftGlow,
                AvailableEffectKind::Gradient2,
                AvailableEffectKind::Gradient4,
                AvailableEffectKind::Reflect,
                AvailableEffectKind::Shake,
            ];
            let mut effect_idx = effect_kinds
                .iter()
                .position(|kind| *kind == self.effect_to_add)
                .unwrap_or(0);
            let effect_combo = WheelComboBox::from_label("Добавить эффект")
                .selected_text(self.effect_to_add.label())
                .show_ui_with_wheel(ui, |ui| {
                    for (idx, kind) in effect_kinds.iter().enumerate() {
                        if ui
                            .selectable_label(effect_idx == idx, kind.label())
                            .clicked()
                        {
                            effect_idx = idx;
                        }
                    }
                });
            if let Some(steps) = effect_combo.wheel_steps {
                cycle_wrapped_index(&mut effect_idx, effect_kinds.len(), steps);
            }
            self.effect_to_add = effect_kinds[effect_idx];

            if ui.button("+ Добавить").clicked() {
                self.effects.push(Self::default_effect_card(
                    self.effect_to_add,
                    self.text_color,
                ));
                changed = true;
            }
        });

        ui.add_space(4.0);
        if self.effects.is_empty() {
            ui.label("Эффекты не добавлены.");
        } else {
            let mut move_up: Option<usize> = None;
            let mut move_down: Option<usize> = None;
            let mut remove_idx: Option<usize> = None;
            if vertical_cards {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let effects_len = self.effects.len();
                    for idx in 0..effects_len {
                        ui.push_id(("typing_effect_card_vertical", idx), |ui| {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "#{} {}",
                                        idx + 1,
                                        effect_card_title(&self.effects[idx])
                                    ));
                                    if ui
                                        .add_enabled(idx > 0, egui::Button::new("↑"))
                                        .on_hover_text("Переместить выше")
                                        .clicked()
                                    {
                                        move_up = Some(idx);
                                    }
                                    if ui
                                        .add_enabled(idx + 1 < effects_len, egui::Button::new("↓"))
                                        .on_hover_text("Переместить ниже")
                                        .clicked()
                                    {
                                        move_down = Some(idx);
                                    }
                                    if ui.button("X").on_hover_text("Удалить").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                                ui.separator();
                                changed |= draw_effect_card_controls(ui, &mut self.effects[idx]);
                            });
                        });
                        if idx + 1 < effects_len {
                            ui.add_space(4.0);
                        }
                    }
                });
            } else {
                let cards_viewport_h = ui.available_height().clamp(170.0, 260.0);
                let card_w = 320.0;

                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_max_height(cards_viewport_h);

                    ui.scope(|ui| {
                        ui.style_mut().always_scroll_the_only_direction = true;
                        egui::ScrollArea::horizontal()
                            .id_salt("typing_create_effects_hscroll")
                            .scroll_source(egui::scroll_area::ScrollSource {
                                scroll_bar: true,
                                drag: true,
                                mouse_wheel: false,
                            })
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.horizontal_top(|ui| {
                                    let effects_len = self.effects.len();
                                    for idx in 0..effects_len {
                                        ui.group(|ui| {
                                            ui.set_width(card_w);
                                            ui.set_min_width(card_w);
                                            ui.set_max_width(card_w);
                                            ui.set_max_height(cards_viewport_h - 12.0);

                                            ui.with_layout(
                                                egui::Layout::top_down(Align::Min),
                                                |ui| {
                                                    ui.push_id(("typing_effect_card", idx), |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(format!(
                                                                "#{} {}",
                                                                idx + 1,
                                                                effect_card_title(
                                                                    &self.effects[idx]
                                                                )
                                                            ));
                                                            if ui
                                                                .add_enabled(
                                                                    idx > 0,
                                                                    egui::Button::new("←"),
                                                                )
                                                                .on_hover_text("Переместить влево")
                                                                .clicked()
                                                            {
                                                                move_up = Some(idx);
                                                            }
                                                            if ui
                                                                .add_enabled(
                                                                    idx + 1 < effects_len,
                                                                    egui::Button::new("→"),
                                                                )
                                                                .on_hover_text("Переместить вправо")
                                                                .clicked()
                                                            {
                                                                move_down = Some(idx);
                                                            }
                                                            if ui
                                                                .button("X")
                                                                .on_hover_text("Удалить")
                                                                .clicked()
                                                            {
                                                                remove_idx = Some(idx);
                                                            }
                                                        });
                                                        ui.separator();

                                                        egui::ScrollArea::vertical()
                                                            .id_salt((
                                                                "typing_effect_card_vscroll",
                                                                idx,
                                                            ))
                                                            .auto_shrink([false, false])
                                                            .max_height(cards_viewport_h - 82.0)
                                                            .show(ui, |ui| {
                                                                changed |=
                                                                    draw_effect_card_controls(
                                                                        ui,
                                                                        &mut self.effects[idx],
                                                                    );
                                                            });
                                                    });
                                                },
                                            );
                                        });
                                    }
                                });
                            });
                    });
                });
            }

            if let Some(idx) = remove_idx {
                self.effects.remove(idx);
                changed = true;
            }
            if let Some(idx) = move_up {
                self.effects.swap(idx - 1, idx);
                changed = true;
            }
            if let Some(idx) = move_down {
                self.effects.swap(idx, idx + 1);
                changed = true;
            }
        }

        if changed {
            self.sync_current_font_profile_memory();
            self.queue_preview_render();
        }
        changed
    }

    pub(super) fn default_effect_card(kind: AvailableEffectKind, text_color: Color32) -> EffectCard {
        match kind {
            AvailableEffectKind::TextShake => EffectCard::TextShake(TextShakeEffectCard {
                spread_x_px: 2.0,
                spread_y_px: 2.0,
                seed: random_seed(),
            }),
            AvailableEffectKind::Stroke => EffectCard::Stroke(StrokeEffectCard {
                width_px: 2.96,
                color: ColorField::new(Color32::WHITE),
                opacity_mode: StrokeOpacityMode::Static,
                transparency_percent: 0.0,
                smoothing: false,
                smoothing_strength_percent: 100.0,
            }),
            AvailableEffectKind::Shadow => EffectCard::Shadow(ShadowEffectCard {
                offset_x_px: 4,
                offset_y_px: 4,
                transparency_percent: 40.0,
                blur_radius_px: 0.0,
                color_mode: ShadowColorMode::SingleColor,
                color: ColorField::new(Color32::BLACK),
            }),
            AvailableEffectKind::Blur => EffectCard::Blur(BlurEffectCard { radius_px: 4.0 }),
            AvailableEffectKind::MotionBlur => EffectCard::MotionBlur(MotionBlurEffectCard {
                angle_deg: 20.0,
                distance_px: 11.0,
                sharp_copy_mode: MotionBlurSharpCopyMode::None,
            }),
            AvailableEffectKind::DryMedia => EffectCard::DryMedia(DryMediaEffectCard {
                material: DryMediaMaterial::Pencil,
                strength: 0.65,
                seed: 1,
                grain_scale_px: 2.0,
                grain_amount: 0.35,
                edge_roughness: 0.45,
                porosity: 0.20,
                direction_deg: 82.0,
                directional_amount: 0.30,
                dust_amount: 0.08,
                dust_radius_px: 2.0,
                softness_px: 0.6,
                use_source_color: true,
                color: ColorField::new(text_color),
            }),
            AvailableEffectKind::GlowV1 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V1,
                radius_px: 16.0,
                softness_px: 0.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::GlowV2 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V2,
                radius_px: 16.0,
                softness_px: 0.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::SoftGlow => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::Soft,
                radius_px: 8.0,
                softness_px: 4.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::Gradient2 => EffectCard::Gradient2(Gradient2EffectCard {
                color1: ColorField::new(Color32::WHITE),
                color2: ColorField::new(Color32::BLACK),
                angle_deg: 90.0,
                width_percent: 100.0,
                respect_source_alpha: true,
                fill_mode: Gradient2FillMode::AllOpaque,
                target_color: ColorField::new(text_color),
            }),
            AvailableEffectKind::Gradient4 => EffectCard::Gradient4(Gradient4EffectCard {
                color_top_left: ColorField::new(Color32::WHITE),
                color_top_right: ColorField::new(Color32::WHITE),
                color_bottom_left: ColorField::new(Color32::BLACK),
                color_bottom_right: ColorField::new(Color32::BLACK),
                width_percent: 100.0,
                respect_source_alpha: true,
                fill_mode: Gradient4FillMode::AllOpaque,
                target_color: ColorField::new(text_color),
            }),
            AvailableEffectKind::Reflect => EffectCard::Reflect(ReflectEffectCard {
                axis: ReflectAxis::Y,
            }),
            AvailableEffectKind::Shake => EffectCard::Shake(ShakeEffectCard {
                angle_deg: 90.0,
                up_px: 0.0,
                down_px: 40.0,
                steps: 12,
                base_fade: 0.30,
                decay: 0.15,
                blur_px: 2,
                autogrow: true,
                grow_margin_px: 0,
            }),
        }
    }

    pub(super) fn effects_json(&self) -> String {
        serde_json::to_string(&self.effects_value_array()).unwrap_or_else(|_| "[]".to_string())
    }

    pub(super) fn draw_right_section(
        &mut self,
        ui: &mut egui::Ui,
        inputs: TypingRightSectionInputs<'_>,
    ) -> TypingRightSectionActions {
        let TypingRightSectionInputs {
            mask_panel_open,
            clean_overlays_visible,
            strict_pixel_movement,
            export_default_dir,
            export_status,
            export_format,
        } = inputs;
        let mut out = TypingRightSectionActions {
            toggle_mask: false,
            changed_clean_overlays: None,
            export_to_folder: None,
            changed_export_format: None,
            round_text_positions: false,
            create_image_request: None,
            changed_strict_pixel_movement: None,
        };
        ui.vertical(|ui| {
            let mask_button_label = if mask_panel_open {
                "Закрыть маску обрезки"
            } else {
                "Открыть маску обрезки"
            };
            if ui.button(mask_button_label).clicked() {
                out.toggle_mask = true;
            }
            if self.preview_enabled {
                let mut format = export_format;
                ui.horizontal(|ui| {
                    ui.label("Формат:");
                    if ui
                        .selectable_value(&mut format, TypingExportFormat::Png, "PNG")
                        .clicked()
                        || ui
                            .selectable_value(&mut format, TypingExportFormat::Psd, "PSD")
                            .clicked()
                    {
                        out.changed_export_format = Some(format);
                    }
                });
            }
            if self.preview_enabled && ui.button("Наложить и сохранить в папку").clicked()
            {
                // Native folder picker; on web there is no OS dialog (folder export
                // is handled by a zip download in the web-glue phase).
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let mut dialog = FileDialog::new();
                    if let Some(path) = export_default_dir {
                        dialog = dialog.set_directory(path);
                    }
                    out.export_to_folder = dialog.pick_folder();
                }
            }
            if self.preview_enabled {
                if ui.button("Вставить картинку из буфера обмена").clicked()
                {
                    out.create_image_request = Some(TypingCreateImageRequest::FromClipboard);
                }
                if ui.button("Выбрать картинку из файла").clicked() {
                    // Native file picker; web file selection arrives via an
                    // <input type=file> in the web-glue phase.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let mut dialog = FileDialog::new();
                        if let Some(path) = export_default_dir {
                            dialog = dialog.set_directory(path);
                        }
                        if let Some(path) = dialog
                            .add_filter("Картинки", &["png", "jpg", "jpeg", "webp", "bmp"])
                            .pick_file()
                        {
                            out.create_image_request =
                                Some(TypingCreateImageRequest::FromFile(path));
                        }
                    }
                }
            }
            if self.preview_enabled {
                match export_status {
                    TypingExportUiStatus::Hidden => {}
                    TypingExportUiStatus::Running { done, total } => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Обработка страниц: {done}/{total}"));
                        });
                        let progress = if *total == 0 {
                            0.0
                        } else {
                            (*done as f32 / *total as f32).clamp(0.0, 1.0)
                        };
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width(ui.available_width())
                                .show_percentage(),
                        );
                    }
                    TypingExportUiStatus::Success { done, total } => {
                        ui.add_space(4.0);
                        let text = format!("Готово: {done}/{total}");
                        let rich = egui::RichText::new(text).color(Color32::from_rgb(90, 230, 120));
                        ui.label(rich);
                        ui.add(
                            egui::ProgressBar::new(1.0)
                                .desired_width(ui.available_width())
                                .show_percentage()
                                .fill(Color32::from_rgb(90, 230, 120)),
                        );
                    }
                    TypingExportUiStatus::Error { message } => {
                        ui.add_space(4.0);
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    }
                }
            }
            // Чекбокс видимости клина доступен в обоих режимах (создание и
            // редактирование); остальные действия ниже — только при создании.
            ui.separator();
            let mut show_clean = clean_overlays_visible;
            if ui.checkbox(&mut show_clean, "Показывать клин").changed() {
                out.changed_clean_overlays = Some(show_clean);
            }
            if self.preview_enabled {
                let mut strict_pixel_movement_value = strict_pixel_movement;
                if ui
                    .checkbox(
                        &mut strict_pixel_movement_value,
                        "Перемещение строго по пикселям",
                    )
                    .changed()
                {
                    out.changed_strict_pixel_movement = Some(strict_pixel_movement_value);
                }
                if ui.button("Округлить позиции текста").clicked() {
                    out.round_text_positions = true;
                }
            }
        });
        out
    }
}
