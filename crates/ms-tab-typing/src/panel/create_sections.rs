/*
File: panel/create_sections.rs

Purpose:
Part of impl TypingCreatePanelState extracted verbatim from panel.rs.
Top-level section drawing for the create panel: preview, params, and
effects sections plus the right-side actions column (mask toggle, export,
image insert, clean-overlay visibility), together with the default effect
card factory and the effects JSON serializer.

The preview section also hosts the per-render font diagnostic rows (which
characters the renderer's fallback chain drew instead of the selected font,
and which it could not draw at all) — see `MODULE_README.md`, "Two font
diagnostics" — and the sticky warning of an applied advanced form that could
not carry its inline tags back (`advanced_form_tags_lost_status`). Both are
drawn here, and not in the status row above it, because that row belongs to
the preview render and is overwritten by it.

The right-hand column's file dialogs (export destination, image file) are non-blocking
`ms_platform::file_dialog` requests (`export_dialog_request`, `image_file_request`): the
pending handle lives in `TypingCreatePanelState::right_section_dialog`, is polled each frame
by `poll_right_section_dialog`, and both triggers stay disabled while it is open. A dialog
failure is shown in the panel's status line.

Notes:
Extracted verbatim from panel.rs. Methods are pub(super) because
TypingCreatePanelState is used only inside panel.rs. use super::* pulls in
the parent module's types and imports.
*/

use super::*;
// The per-render font diagnostic is mapped to colors/wording in one place only
// (`create_presets.rs`, together with the static coverage tooltip).
use super::create_presets::font_fallback_status_lines;
use ms_platform::file_dialog::{self, DialogKind, DialogResult, FileDialogRequest, PendingDialog};

/// A file dialog opened by the create panel's right-hand column.
pub(super) enum TypingPanelDialog {
    /// The export destination dialog; `kind` is the destination SHAPE it was opened for.
    Export { kind: TypingExportDialogKind, dialog: PendingDialog },
    /// The «pick image from file» dialog.
    ImageFile(PendingDialog),
}

impl TypingPanelDialog {
    /// `None` while the dialog is open; its result exactly once.
    fn poll(&mut self) -> Option<DialogResult> {
        match self {
            TypingPanelDialog::Export { dialog, .. } | TypingPanelDialog::ImageFile(dialog) => dialog.poll(),
        }
    }
}

/// The export destination dialog for `format`: a folder for the per-page formats, a `.pdf`
/// file (prefilled from `base_name`) for PDF. Returns the destination kind with the request so
/// the result is mapped by the same decision that chose the dialog.
pub(super) fn export_dialog_request(format: TypingExportFormat, default_dir: Option<&Path>, base_name: &str) -> (TypingExportDialogKind, FileDialogRequest) {
    let kind = export_dialog_kind(format);
    let request = match kind {
        TypingExportDialogKind::Folder => FileDialogRequest::new(DialogKind::SaveIntoFolder),
        TypingExportDialogKind::PdfFile => FileDialogRequest::new(DialogKind::SaveFile)
            .suggested_name(pdf_export_file_name(base_name))
            .filter(t!("typing.export.pdf_files_label"), &["pdf"]),
    };
    let request = match default_dir {
        Some(dir) => request.start_dir(dir),
        None => request,
    };
    (kind, request)
}

/// The «pick image from file» dialog, starting in `default_dir` when known.
pub(super) fn image_file_request(default_dir: Option<&Path>) -> FileDialogRequest {
    let request = FileDialogRequest::new(DialogKind::OpenFile)
        .filter(t!("typing.export.images_label"), &["png", "jpg", "jpeg", "webp", "bmp"]);
    match default_dir {
        Some(dir) => request.start_dir(dir),
        None => request,
    }
}

impl TypingCreatePanelState {

    /// Opens `request` without blocking and keeps its handle; `wrap` names what it is for.
    fn open_right_section_dialog(&mut self, ui: &egui::Ui, request: FileDialogRequest, wrap: impl FnOnce(PendingDialog) -> TypingPanelDialog) {
        let repaint_ctx = ui.ctx().clone();
        let dialog = file_dialog::request(request, Some(Arc::new(move || repaint_ctx.request_repaint())));
        self.right_section_dialog = Some(wrap(dialog));
    }

    /// Turns a closed right-column dialog into this frame's action: an export destination or an
    /// image-file request. A cancel is a no-op; a failure lands in the status line.
    fn poll_right_section_dialog(&mut self, out: &mut TypingRightSectionActions) {
        let Some(result) = self.right_section_dialog.as_mut().and_then(TypingPanelDialog::poll) else {
            return;
        };
        let Some(dialog) = self.right_section_dialog.take() else {
            return;
        };
        let picked = match result {
            Ok(Some(picked)) => picked,
            Ok(None) => return,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[typing] create panel file dialog failed: {err:?}"));
                self.status_line = err.user_message();
                return;
            }
        };
        // The `Picked` variant always matches the request kind; a mismatch is a platform-layer
        // bug, logged and treated as a cancel.
        let mismatch = || ms_log::runtime_log::log_error("[typing] create panel file dialog answered with the wrong result kind");
        match dialog {
            TypingPanelDialog::Export { kind: TypingExportDialogKind::Folder, .. } => match picked.into_folder_target() {
                Some(target) => out.export_destination = Some(TypingExportDestination::Folder(target)),
                None => mismatch(),
            },
            TypingPanelDialog::Export { kind: TypingExportDialogKind::PdfFile, .. } => match picked.into_save_target() {
                Some(target) => out.export_destination = Some(TypingExportDestination::PdfFile(target)),
                None => mismatch(),
            },
            TypingPanelDialog::ImageFile(_) => match picked.into_file() {
                Some(file) => out.create_image_request = Some(TypingCreateImageRequest::FromFile(file.path)),
                None => mismatch(),
            },
        }
    }

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

            // Sticky warning of the LAST APPLIED advanced form: its inline tags could not
            // be put back and the form is in effect without them. It is drawn here, and
            // not in the status row above, because the status row belongs to the preview
            // render — `queue_preview_render` claims it inside the very call that applies
            // the form — while a silently dropped `<m …>` style has to stay readable until
            // the formed text is replaced.
            if let Some(warning) = self.advanced_form_tags_lost_status() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(warning).color(ui.visuals().warn_fg_color),
                    )
                    .wrap(),
                );
            }

            // Factual per-render font diagnostic of the image shown below: which
            // characters the fallback chain drew instead of the selected font, and
            // which nothing could draw. It lives HERE and not on the font combo
            // because it is a property of THIS render of THIS text, while the combo's
            // coloring is a per-font, per-language classification computed once at
            // font-load time for the whole list. Nothing is drawn when the selected
            // font served the whole text.
            for line in font_fallback_status_lines(&self.preview_font_fallbacks) {
                ui.add(
                    egui::Label::new(egui::RichText::new(line.text).color(line.color)).wrap(),
                )
                .on_hover_text(line.tooltip);
            }

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
                            ui.label(t!("typing.preview.not_ready_status"));
                        },
                    );
                }
            });
        });
    }

    /// Draws the create panel's text parameters. `presets` is the color-preset set
    /// its color swatches offer (see [`ColorPresetsBinding`]); `extras` is the
    /// «Параметры» dock tab's persisted state, which holds the sections'
    /// expanded/collapsed flags.
    pub(super) fn draw_params_section(
        &mut self,
        ui: &mut egui::Ui,
        extras: &mut TabExtras,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
        presets: &mut ColorPresetsBinding<'_>,
    ) {
        let mut params_changed = false;
        params_changed |= self.draw_main_text_params(
            ui,
            extras,
            stacked_columns,
            remap_wheel_to_horizontal,
            presets,
            FontSectionGates {
                memory_enabled: self.preview_enabled,
                // The create panel can enter the missing-font state too — a preset naming a
                // font that is not loaded (`create_presets::apply_preset_by_name`) and a
                // background reload whose selected identity vanished
                // (`create_state::poll_font_reload_results`) both set it. Passing a hardcoded
                // `false` here left that state INVISIBLE: the parameter widgets stayed live
                // over a font the panel had already refused to keep. The font combo and the
                // preset combo are drawn outside this gate, so the user can always recover by
                // picking an available font (which clears the flag).
                font_missing: self.missing_font.is_some(),
            },
        );

        if params_changed {
            self.sync_current_font_profile_memory();
            self.queue_preview_render();
        }
    }

    /// Draws the effects panel (the card strip and every card's controls) and
    /// reports whether anything changed. `presets` is the color-preset set the
    /// cards' color swatches offer (see [`ColorPresetsBinding`]).
    pub(super) fn draw_effects_section(
        &mut self,
        ui: &mut egui::Ui,
        vertical_cards: bool,
        presets: &mut ColorPresetsBinding<'_>,
    ) -> bool {
        let mut changed = false;
        ui.label(if vertical_cards {
            t!("typing.effects.order_top_to_bottom_hint")
        } else {
            t!("typing.effects.order_left_to_right_hint")
        });
        ui.horizontal(|ui| {
            let effect_kinds = [
                AvailableEffectKind::TextShake,
                AvailableEffectKind::Stroke,
                AvailableEffectKind::Shadow,
                AvailableEffectKind::Blur,
                AvailableEffectKind::MotionBlur,
                AvailableEffectKind::DryMedia,
                AvailableEffectKind::Interference,
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
            let effect_combo = WheelComboBox::from_label(t!("typing.effects.add_effect_combo_id")).id_salt("typing.effects.add_effect_combo_id")
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

            if ui.button(t!("typing.effects.add_button")).clicked() {
                // Prefer the user-configured per-kind default; fall back to the built-in
                // when no override is stored (byte-identical to the historical behavior).
                let card = effect_defaults::effect_default_card(self.effect_to_add, self.text_color)
                    .unwrap_or_else(|| {
                        Self::default_effect_card(self.effect_to_add, self.text_color)
                    });
                self.effects.push(card);
                changed = true;
            }
        });

        ui.add_space(4.0);
        if self.effects.is_empty() {
            ui.label(t!("typing.effects.none_added_hint"));
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
                                        .on_hover_text(t!("typing.effects.move_up_tooltip"))
                                        .clicked()
                                    {
                                        move_up = Some(idx);
                                    }
                                    if ui
                                        .add_enabled(idx + 1 < effects_len, egui::Button::new("↓"))
                                        .on_hover_text(t!("typing.effects.move_down_tooltip"))
                                        .clicked()
                                    {
                                        move_down = Some(idx);
                                    }
                                    if ui.button("X").on_hover_text(t!("typing.common.delete_button")).clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                                ui.separator();
                                changed |= draw_effect_card_controls(
                                    ui,
                                    &mut self.effects[idx],
                                    presets,
                                );
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
                                drag: egui::scroll_area::DragScroll::Always,
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
                                                                .on_hover_text(t!("typing.effects.move_left_tooltip"))
                                                                .clicked()
                                                            {
                                                                move_up = Some(idx);
                                                            }
                                                            if ui
                                                                .add_enabled(
                                                                    idx + 1 < effects_len,
                                                                    egui::Button::new("→"),
                                                                )
                                                                .on_hover_text(t!("typing.effects.move_right_tooltip"))
                                                                .clicked()
                                                            {
                                                                move_down = Some(idx);
                                                            }
                                                            if ui
                                                                .button("X")
                                                                .on_hover_text(t!("typing.common.delete_button"))
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
                                                                        presets,
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
            AvailableEffectKind::Interference => EffectCard::Interference(InterferenceEffectCard {
                kind: InterferenceKind::WhiteNoise,
                seed: random_seed(),
                amount: 0.5,
                scale_px: 1.0,
                density: 1.0,
                monochrome: true,
                alpha_noise: 0.0,
                slice_height_px: 8,
                height_jitter: 0.5,
                max_shift_px: 16.0,
                probability: 0.4,
                rgb_split_px: 4.0,
                autogrow: true,
                offset_px: 3.0,
                angle_deg: 0.0,
                per_row_jitter: 0.0,
                line_height_px: 2,
                gap_px: 2,
                darken: 0.35,
                jitter_px: 0.0,
            }),
            AvailableEffectKind::GlowV1 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V1,
                radius_px: 16.0,
                // Soft-glow-only fields: inert for V1, never serialized.
                blur_radius_px: 0.0,
                blur_bias: 0.0,
                blur_knee: 100.0,
                expand_x_plus_px: 0,
                expand_x_minus_px: 0,
                expand_y_plus_px: 0,
                expand_y_minus_px: 0,
                outline_shape: GlowOutlineShape::Square,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::GlowV2 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V2,
                radius_px: 16.0,
                // Soft-glow-only fields: inert for V2, never serialized.
                blur_radius_px: 0.0,
                blur_bias: 0.0,
                blur_knee: 100.0,
                expand_x_plus_px: 0,
                expand_x_minus_px: 0,
                expand_y_plus_px: 0,
                expand_y_minus_px: 0,
                outline_shape: GlowOutlineShape::Square,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::SoftGlow => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::Soft,
                radius_px: 8.0,
                blur_radius_px: 4.0,
                blur_bias: 0.0,
                blur_knee: 100.0,
                expand_x_plus_px: 0,
                expand_x_minus_px: 0,
                expand_y_plus_px: 0,
                expand_y_minus_px: 0,
                outline_shape: GlowOutlineShape::Square,
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
                color_tolerance_percent: 0.0,
                area_mode: GradientAreaMode::FullImage,
            }),
            AvailableEffectKind::Gradient4 => EffectCard::Gradient4(Box::new(Gradient4EffectCard {
                color_top_left: ColorField::new(Color32::WHITE),
                color_top_right: ColorField::new(Color32::WHITE),
                color_bottom_left: ColorField::new(Color32::BLACK),
                color_bottom_right: ColorField::new(Color32::BLACK),
                width_percent: 100.0,
                respect_source_alpha: true,
                fill_mode: Gradient4FillMode::AllOpaque,
                target_color: ColorField::new(text_color),
                color_tolerance_percent: 0.0,
                area_mode: GradientAreaMode::FullImage,
            })),
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
            project_export,
            export_status,
            export_format,
            comic_type,
            repaginate,
            repaginate_pages_warning,
            export_base_name,
            psd_bundle_custom_fonts,
        } = inputs;
        let mut out = TypingRightSectionActions {
            toggle_mask: false,
            changed_clean_overlays: None,
            export_destination: None,
            changed_export_format: None,
            changed_repaginate: None,
            changed_repaginate_pages_warning: None,
            repaginate_block_drawn: false,
            changed_psd_bundle_custom_fonts: None,
            round_text_positions: false,
            create_image_request: None,
            changed_strict_pixel_movement: None,
        };
        self.poll_right_section_dialog(&mut out);
        // One dialog at a time: both file-dialog triggers wait while one is open.
        let dialog_open = self.right_section_dialog.is_some();
        ui.vertical(|ui| {
            let mask_button_label = if mask_panel_open {
                t!("typing.export.close_clip_mask_button")
            } else {
                t!("typing.export.open_clip_mask_button")
            };
            if ui.button(mask_button_label).clicked() {
                out.toggle_mask = true;
            }
            // Project-export controls exist only when the session offers a project export: a
            // single-image session writes its picture through the app's «Сохранить» instead.
            let export_controls_visible = self.preview_enabled && project_export;
            if export_controls_visible {
                let mut format = export_format;
                ui.horizontal(|ui| {
                    ui.label(t!("typing.export.format_label"));
                    // Format NAMES are proper nouns and stay literal (the i18n exclusion for
                    // protocol/format identifiers); everything else in this block is a key.
                    //
                    // Each button is drawn into its own binding on purpose: a `||` chain
                    // short-circuits, so a click on the first option would skip DRAWING the
                    // remaining ones for that frame and the row would flicker.
                    let png_clicked = ui.selectable_value(&mut format, TypingExportFormat::Png, "PNG").clicked();
                    let psd_clicked = ui.selectable_value(&mut format, TypingExportFormat::Psd, "PSD").clicked();
                    let pdf_clicked = ui.selectable_value(&mut format, TypingExportFormat::Pdf, "PDF").clicked();
                    if png_clicked || psd_clicked || pdf_clicked {
                        out.changed_export_format = Some(format);
                    }
                });
            }
            // SINGLE owner of "is the re-pagination block on screen": the same decision is
            // reported to the facade, which expires the page-title confirmation whenever the
            // block that hosts it was not drawn. Re-deciding it there would be a second copy
            // of this condition, free to drift from this one.
            out.repaginate_block_drawn = export_controls_visible && repaginate_block_visible(export_format);
            if out.repaginate_block_drawn {
                draw_repaginate_block(ui, comic_type, repaginate, repaginate_pages_warning, &mut out);
            }
            // PSD only: the other formats are raster, a custom font needs no file there.
            if export_controls_visible && export_format == TypingExportFormat::Psd {
                let mut bundle = psd_bundle_custom_fonts;
                if ui
                    .checkbox(&mut bundle, t!("typing.export.psd_bundle_custom_fonts_checkbox"))
                    .on_hover_text(t!("typing.export.psd_bundle_custom_fonts_tooltip"))
                    .changed()
                {
                    out.changed_psd_bundle_custom_fonts = Some(bundle);
                }
            }
            if export_controls_visible {
                // Primary export action: painted green to stand out among the
                // neutral secondary buttons in this panel. The surrounding group
                // keeps the theme's default stroke.
                let clicked = egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        // The caption depends on the destination KIND (a folder of page
                        // files vs. one document), which `export_button_label_key` decides
                        // together with the dialog below so the two can never disagree.
                        let caption = ms_i18n::resolve_key(export_button_label_key(export_format));
                        let label = egui::RichText::new(caption)
                            .color(Color32::from_rgb(230, 230, 230));
                        ui.add_enabled(!dialog_open, egui::Button::new(label).fill(Color32::from_rgb(46, 140, 56)))
                            .clicked()
                    })
                    .inner;
                if clicked {
                    let (kind, request) = export_dialog_request(export_format, export_default_dir, export_base_name);
                    self.open_right_section_dialog(ui, request, |dialog| TypingPanelDialog::Export { kind, dialog });
                }
            }
            if self.preview_enabled {
                if ui.button(t!("typing.export.paste_image_from_clipboard_button")).clicked()
                {
                    out.create_image_request = Some(TypingCreateImageRequest::FromClipboard);
                }
                if ui
                    .add_enabled(!dialog_open, egui::Button::new(t!("typing.export.pick_image_from_file_button")))
                    .clicked()
                {
                    self.open_right_section_dialog(ui, image_file_request(export_default_dir), TypingPanelDialog::ImageFile);
                }
            }
            if self.preview_enabled {
                match export_status {
                    TypingExportUiStatus::Hidden => {}
                    TypingExportUiStatus::Preparing { done, total } => {
                        // Non-blocking "preparing pages" indicator while the whole-project preload
                        // runs before a deferred export (Phase 2). Same slot/shape as Running.
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tf!("typing.export.preparing_pages_status", done = done, total = total));
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
                    TypingExportUiStatus::Running { done, total } => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tf!("typing.export.processing_pages_status", done = done, total = total));
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
                    TypingExportUiStatus::Success {
                        done,
                        total,
                        warnings,
                    } => {
                        ui.add_space(4.0);
                        let text = tf!("typing.export.done_pages_status", done = done, total = total);
                        let rich = egui::RichText::new(text).color(ms_theme::status::SUCCESS);
                        ui.label(rich);
                        ui.add(
                            egui::ProgressBar::new(1.0)
                                .desired_width(ui.available_width())
                                .show_percentage()
                                .fill(ms_theme::status::SUCCESS),
                        );
                        // The export SUCCEEDED, but the format could not express something
                        // (today: a PSD font name claimed by several installed fonts). It
                        // is drawn in the studio warning colour (as font diagnostics are), next to
                        // the result it qualifies, so an approximation is never silent.
                        for warning in warnings {
                            ui.colored_label(
                                ms_theme::status::WARNING,
                                warning,
                            );
                        }
                    }
                    TypingExportUiStatus::Error { message } => {
                        ui.add_space(4.0);
                        ui.colored_label(ms_theme::status::ERROR, message);
                    }
                }
            }
            // Чекбокс видимости клина доступен в обоих режимах (создание и
            // редактирование); остальные действия ниже — только при создании.
            ui.separator();
            let mut show_clean = clean_overlays_visible;
            if ui
                .checkbox(&mut show_clean, t!("typing.export.show_clean_overlay"))
                .on_hover_text(tf!("typing.export.show_clean_overlay_shortcut_hint", binding = self.input_labels.toggle_clean.as_str()))
                .changed()
            {
                out.changed_clean_overlays = Some(show_clean);
            }
            if self.preview_enabled {
                let mut strict_pixel_movement_value = strict_pixel_movement;
                if ui
                    .checkbox(
                        &mut strict_pixel_movement_value,
                        t!("typing.export.strict_pixel_move"),
                    )
                    .changed()
                {
                    out.changed_strict_pixel_movement = Some(strict_pixel_movement_value);
                }
                if ui.button(t!("typing.export.round_text_positions")).clicked() {
                    out.round_text_positions = true;
                }
            }
        });
        out
    }
}

/// Draws the re-pagination («перенарезка») options of the export block and reports every
/// edit through `out`.
///
/// Free function rather than a method: it touches no panel state at all — the settings come
/// in as a value (`repaginate`) and go back out as `TypingRightSectionActions`, exactly like
/// the rest of this section's inputs. The caller decides whether the block is shown at all
/// (`repaginate_block_visible`), so this function never re-checks the format.
///
/// `comic_type` is the open title's kind and only reaches [`apply_repaginate_toggle`];
/// `repaginate_pages_warning` is the current state of the page-title confirmation.
fn draw_repaginate_block(
    ui: &mut egui::Ui,
    comic_type: Option<ms_project::ComicType>,
    repaginate: TypingRepaginateSettings,
    repaginate_pages_warning: bool,
    out: &mut TypingRightSectionActions,
) {
    let mut settings = repaginate;
    let mut changed = false;
    // Mirror of the confirmation state AFTER this frame's click, so a warning raised by the
    // click is painted in the same frame that refused the toggle.
    let mut warning_visible = repaginate_pages_warning;

    ui.add_space(4.0);
    // The checkbox is driven through a scratch copy: on a page-based title the first attempt
    // to enable must be REFUSED, so the authoritative value is whatever
    // `apply_repaginate_toggle` returns, never what the widget wrote.
    let mut enabled_request = settings.enabled;
    if ui
        .checkbox(&mut enabled_request, t!("typing.export.repaginate_toggle"))
        .on_hover_text(t!("typing.export.repaginate_toggle_tooltip"))
        .changed()
    {
        let (enabled, warning) = apply_repaginate_toggle(enabled_request, comic_type, repaginate_pages_warning);
        if enabled != settings.enabled {
            settings.enabled = enabled;
            changed = true;
        }
        warning_visible = warning;
        if warning != repaginate_pages_warning {
            out.changed_repaginate_pages_warning = Some(warning);
        }
    }
    if warning_visible {
        ui.add(
            egui::Label::new(
                egui::RichText::new(t!("typing.export.repaginate_pages_warning"))
                    .color(ms_theme::status::WARNING),
            )
            .wrap(),
        );
    }

    if settings.enabled {
        ui.indent("typing.export.repaginate_options", |ui| {
            let mut mode = settings.mode;
            ui.horizontal(|ui| {
                let ratio_clicked = ui
                    .selectable_value(&mut mode, TypingRepaginateMode::Ratio, t!("typing.export.repaginate_mode_ratio"))
                    .clicked();
                let fixed_clicked = ui
                    .selectable_value(&mut mode, TypingRepaginateMode::FixedHeight, t!("typing.export.repaginate_mode_fixed_height"))
                    .clicked();
                if ratio_clicked || fixed_clicked {
                    settings.mode = mode;
                    changed = true;
                }
            });
            match settings.mode {
                TypingRepaginateMode::Ratio => {
                    ui.horizontal(|ui| {
                        // Both terms are >= 1: a zero makes `target_height_px` return `None`,
                        // i.e. silently no re-pagination at all.
                        changed |= ui
                            .add(
                                WheelSpinBox::new(&mut settings.ratio_width)
                                    .range(1..=10_000)
                                    .prefix(t!("typing.export.repaginate_ratio_width_label")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                WheelSpinBox::new(&mut settings.ratio_height)
                                    .range(1..=10_000)
                                    .prefix(t!("typing.export.repaginate_ratio_height_label")),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(t!("typing.export.repaginate_ratio_presets_label"));
                        // The preset buttons compare against the CURRENT pair, so the active
                        // preset is highlighted and a hand-typed ratio highlights none.
                        let mut ratio = (settings.ratio_width, settings.ratio_height);
                        for (width, height) in REPAGINATE_RATIO_PRESETS {
                            if ui.selectable_value(&mut ratio, (width, height), format!("{width}:{height}")).clicked() {
                                settings.ratio_width = width;
                                settings.ratio_height = height;
                                changed = true;
                            }
                        }
                    });
                }
                TypingRepaginateMode::FixedHeight => {
                    changed |= ui
                        .add(
                            WheelSpinBox::new(&mut settings.fixed_height_px)
                                .range(1..=100_000)
                                .prefix(t!("typing.export.repaginate_fixed_height_label")),
                        )
                        .changed();
                }
            }
        });
    }

    if changed {
        out.changed_repaginate = Some(settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The export dialog follows the destination SHAPE of the format: a folder to write into
    /// for the per-page formats, a `.pdf` save for PDF (filtered to pdf only).
    #[test]
    fn export_dialog_request_matches_the_destination_kind() {
        for format in [TypingExportFormat::Png, TypingExportFormat::Psd] {
            let (kind, request) = export_dialog_request(format, None, "Тайтл 1");
            assert_eq!(kind, TypingExportDialogKind::Folder);
            assert_eq!(request.kind(), DialogKind::SaveIntoFolder);
        }
        let (kind, request) = export_dialog_request(TypingExportFormat::Pdf, Some(Path::new("/projects/t")), "Тайтл 1");
        assert_eq!(kind, TypingExportDialogKind::PdfFile);
        assert_eq!(request.kind(), DialogKind::SaveFile);
        assert_eq!(request.filters()[0].extensions(), ["pdf".to_string()]);
    }

    /// «Pick image from file» opens one existing image.
    #[test]
    fn image_file_request_opens_one_image() {
        let request = image_file_request(None);
        assert_eq!(request.kind(), DialogKind::OpenFile);
        assert!(request.filters()[0].extensions().contains(&"webp".to_string()));
    }
}
