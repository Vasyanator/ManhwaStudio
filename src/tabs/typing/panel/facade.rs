/*
File: panel/facade.rs

Purpose:
Holds the `impl TypingTopPanelState` inherent block extracted verbatim from
`panel.rs`. This is the public-facing top-panel state facade: mode/layout
management, selected-overlay edit sync + request queue, auto-typing settings,
and the vertical parameters/actions panel plus the create-preview panel drawing.

Notes:
Extracted verbatim from `panel.rs`. Method visibility was escalated one level
because the impl moved a directory deeper: the former `pub(super)` methods are
now `pub(in crate::tabs::typing)` so the sibling `tab.rs` can still call them,
and the former private methods are now `pub(super)`. `use super::*;` pulls in the
parent module's types and imports.
*/

use super::*;

impl TypingTopPanelState {
    pub(in crate::tabs::typing) fn draw(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        text_overlays: &mut TypingTextOverlayLayer,
        page_idx: usize,
        layout_editor_active: bool,
    ) {
        // Cached font coverage (`FontEntry.coverage`) is computed at load time against the
        // then-current typesetting language. If the user has since changed the language, that
        // cache is stale, so reload both font lists off-thread to recompute coverage. The
        // atomic load is cheap; the reload only fires on an actual language change.
        let current_language = text_language();
        if current_language != self.coverage_language {
            self.coverage_language = current_language;
            self.create_panel.spawn_font_reload();
            self.edit_panel.spawn_font_reload();
        }
        // Pick up a settings-side font import/remove (revision-driven) and apply it live to
        // both open panels before polling this frame's reload results.
        self.create_panel.poll_font_settings_changes();
        self.edit_panel.poll_font_settings_changes();
        self.create_panel.poll_font_reload_results();
        self.edit_panel.poll_font_reload_results();
        self.create_panel.reset_text_input_focus_tracking();
        self.edit_panel.reset_text_input_focus_tracking();
        if self.create_panel.fonts_reload_in_flight() || self.edit_panel.fonts_reload_in_flight() {
            ctx.request_repaint();
        }
        // Синхронизация выбранной группы шрифтов между панелями создания и
        // редактирования: запрос с любой панели применяется к обеим.
        if let Some(group) = self
            .create_panel
            .take_font_group_request()
            .or_else(|| self.edit_panel.take_font_group_request())
        {
            self.create_panel.set_font_group(group.clone());
            self.edit_panel.set_font_group(group);
        }
        if self.mode == TypingTopPanelMode::CreateText {
            self.create_panel.poll_preview_render_results(ctx);
            self.create_panel.ensure_initial_preview_request();
            if self.create_panel.render_in_flight {
                ctx.request_repaint();
            }
        }

        self.draw_vertical_panel(ctx, canvas_rect, text_overlays, page_idx, layout_editor_active);

        // Drain a settings deep-link request (font-group "?" help icon) from either
        // sub-panel AFTER the panels have drawn, so a click bubbles up in the SAME
        // frame the app polls `take_settings_link`. Draining before the draw would
        // leave a click dormant for a frame — and a rapid second click would then
        // survive the tab switch and unexpectedly re-navigate to Settings when the
        // user later returns to the typing tab.
        if let Some(link) = self
            .create_panel
            .take_settings_link_request()
            .or_else(|| self.edit_panel.take_settings_link_request())
        {
            self.pending_settings_link = Some(link);
        }
    }

    pub(in crate::tabs::typing) fn set_panel_layout(&mut self, layout: TypingPanelLayout) {
        let _ = layout;
    }

    pub(in crate::tabs::typing) fn has_focused_text_input(&self, ctx: &egui::Context) -> bool {
        self.create_panel.has_focused_text_input(ctx) || self.edit_panel.has_focused_text_input(ctx)
    }

    pub(in crate::tabs::typing) fn eyedropper_active(&self) -> bool {
        self.create_panel.eyedropper_active() || self.edit_panel.eyedropper_active()
    }

    pub(in crate::tabs::typing) fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        self.create_panel
            .eyedropper_consumed_primary_click_this_frame()
            || self
                .edit_panel
                .eyedropper_consumed_primary_click_this_frame()
    }

    pub(in crate::tabs::typing) fn auto_typing_settings(&self) -> TypingAutoTypingSettings {
        TypingAutoTypingSettings {
            debug_visuals: self.auto_typing_debug_visuals,
            extra_downward_shift_percent: self.auto_typing_extra_downward_shift_percent,
        }
    }

    /// Draws the floating vertical panels (Параметры/Эффекты + Действия/Слои).
    ///
    /// When `layout_editor_active` is true the top-left layout-editor panel is on screen; the
    /// Actions/Layers panel is then pushed below that panel's bottom edge if the two would
    /// overlap horizontally, so it is not hidden underneath it.
    pub(super) fn draw_vertical_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        text_overlays: &mut TypingTextOverlayLayer,
        page_idx: usize,
        layout_editor_active: bool,
    ) {
        // Для image-оверлея вкладка «Параметры» показывает только трансформацию, но вкладка
        // «Эффекты» доступна так же, как для текста — эффекты применяются к сторонней картинке.
        let image_edit_only = self.mode == TypingTopPanelMode::EditText
            && self.edit_overlay_kind == Some(TypingOverlayKind::Image);
        if self.vertical_panel_tab != self.vertical_panel_last_tab {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            self.vertical_panel_last_tab = self.vertical_panel_tab;
            ctx.request_repaint();
        }
        if self.last_canvas_height_px > 0.0
            && (canvas_rect.height() - self.last_canvas_height_px).abs() >= 1.0
        {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            ctx.request_repaint();
        }
        self.last_canvas_height_px = canvas_rect.height();
        let panel_w = TYPING_VERTICAL_PANEL_DEFAULT_WIDTH_PX
            .clamp(
                TYPING_VERTICAL_PANEL_MIN_WIDTH_PX,
                TYPING_VERTICAL_PANEL_MAX_WIDTH_PX,
            )
            .min((canvas_rect.width() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0).max(220.0));
        let actions_panel_w = TYPING_VERTICAL_ACTIONS_DEFAULT_WIDTH_PX
            .clamp(
                TYPING_VERTICAL_ACTIONS_MIN_WIDTH_PX,
                TYPING_VERTICAL_ACTIONS_MAX_WIDTH_PX,
            )
            .min((canvas_rect.width() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0).max(220.0));
        let viewport_rect = ctx.content_rect();
        let min_x = viewport_rect.left();
        let right_limit = viewport_rect.right() - TYPING_VERTICAL_PANEL_SCROLLBAR_RESERVE_PX;
        let max_x = (right_limit - panel_w).max(min_x);
        let actions_min_x = canvas_rect.left();
        let actions_max_x = (canvas_rect.right() - actions_panel_w).max(actions_min_x);
        let min_y = canvas_rect.top();
        let max_y = (canvas_rect.bottom() - 48.0).max(min_y);
        let default_panel_top = canvas_rect.top() + TYPING_VERTICAL_PANEL_GAP_PX;
        let default_pos = egui::pos2(
            (right_limit - panel_w - TYPING_VERTICAL_PANEL_GAP_PX).max(min_x),
            default_panel_top,
        );
        let panel_pos = self
            .vertical_panel
            .pos
            .filter(|_| self.vertical_panel.user_positioned)
            .unwrap_or(default_pos)
            .clamp(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));
        let viewport_target_height =
            (canvas_rect.height() * TYPING_VERTICAL_PANEL_INITIAL_HEIGHT_RATIO).clamp(
                TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX,
                (canvas_rect.height() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0)
                    .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX),
            );
        let available_panel_height = (canvas_rect.height() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX);
        let current_content_height = match self.vertical_panel_tab {
            TypingVerticalMainTab::Parameters => self.vertical_panel_params_content_height_px,
            TypingVerticalMainTab::Effects => self.vertical_panel_effects_content_height_px,
        };
        let panel_default_height = if current_content_height > 0.0 {
            current_content_height
                .min(viewport_target_height)
                .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
        } else {
            viewport_target_height.max(TYPING_VERTICAL_PANEL_DEFAULT_HEIGHT_PX)
        };
        let panel_max_height = if current_content_height > 0.0 {
            current_content_height
                .min(available_panel_height)
                .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
        } else {
            available_panel_height
        };
        let auto_target_height = compute_typing_vertical_panel_auto_height(
            current_content_height,
            viewport_target_height,
            available_panel_height,
        );
        if self.vertical_panel_last_auto_target_height_px > 0.0
            && (auto_target_height - self.vertical_panel_last_auto_target_height_px).abs() >= 1.0
        {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            ctx.request_repaint();
        }
        self.vertical_panel_last_auto_target_height_px = auto_target_height;

        let mut changed = false;
        let params_area_response = egui::Area::new(TYPING_VERTICAL_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(panel_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w);
                ui.set_min_width(panel_w);
                ui.set_max_width(panel_w);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_min_width(panel_w);
                    ui.set_max_width(panel_w);
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.collapsed { "▶" } else { "▼" };
                        let toggle_hint = if self.collapsed {
                            t!("typing.panel.expand_text_panel_tooltip")
                        } else {
                            t!("typing.panel.collapse_text_panel_tooltip")
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.collapsed = !self.collapsed;
                        }
                        ui.selectable_value(
                            &mut self.vertical_panel_tab,
                            TypingVerticalMainTab::Parameters,
                            TypingVerticalMainTab::Parameters.label(),
                        );
                        ui.selectable_value(
                            &mut self.vertical_panel_tab,
                            TypingVerticalMainTab::Effects,
                            TypingVerticalMainTab::Effects.label(),
                        );
                    });
                    if self.collapsed {
                        return;
                    }

                    ui.add_space(4.0);
                    egui::Resize::default()
                        .id_salt((
                            "typing_vertical_main_resize",
                            self.vertical_panel_resize_revision,
                        ))
                        .resizable([false, true])
                        .default_size(egui::vec2(ui.available_width(), panel_default_height))
                        .min_size(egui::vec2(0.0, TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX))
                        .max_size(egui::vec2(ui.available_width(), panel_max_height))
                        .show(ui, |ui| {
                            let mut content_height_px = 0.0;
                            egui::ScrollArea::vertical()
                                .id_salt("typing_vertical_main_vscroll")
                                .show(ui, |ui| match self.vertical_panel_tab {
                                    TypingVerticalMainTab::Parameters => {
                                        if self.mode == TypingTopPanelMode::CreateText {
                                            self.create_panel.draw_create_presets_section(ui);
                                            ui.add_space(6.0);
                                        }
                                        // The text-params panel is now grouped into
                                        // labelled collapsible sections, so a floating
                                        // heading above them would group nothing and is
                                        // dropped. The image-only panel is NOT sectioned,
                                        // so it keeps its heading.
                                        if image_edit_only {
                                            ui.label(
                                                egui::RichText::new(
                                                    t!("typing.panel.image_params_heading"),
                                                )
                                                .strong(),
                                            );
                                        }
                                        ui.scope(|ui| {
                                            ui.style_mut().always_scroll_the_only_direction = true;
                                            egui::ScrollArea::horizontal()
                                                .id_salt("typing_vertical_params_hscroll")
                                                .scroll_source(egui::scroll_area::ScrollSource {
                                                    scroll_bar: true,
                                                    drag: egui::scroll_area::DragScroll::Always,
                                                    mouse_wheel: false,
                                                })
                                                .auto_shrink([false, true])
                                                .show(ui, |ui| match self.mode {
                                                    TypingTopPanelMode::CreateText => {
                                                        self.create_panel.clamp_face_index();
                                                        self.create_panel
                                                            .draw_params_section(ui, true, false);
                                                    }
                                                    TypingTopPanelMode::EditText => {
                                                        if image_edit_only {
                                                            changed |= self
                                                                .edit_panel
                                                                .draw_image_transform_only_section(
                                                                    ui, false,
                                                                );
                                                        } else {
                                                            changed |= self
                                                                .edit_panel
                                                                .draw_edit_params_section(
                                                                    ui, true, false,
                                                                );
                                                        }
                                                    }
                                                });
                                        });
                                        content_height_px = ui.min_rect().height();
                                    }
                                    TypingVerticalMainTab::Effects => {
                                        changed |= match self.mode {
                                            TypingTopPanelMode::CreateText => {
                                                self.create_panel.draw_effects_section(ui, true)
                                            }
                                            TypingTopPanelMode::EditText => {
                                                // Эффекты тоже вызывают перерендер:
                                                // при ненайденном шрифте блокируем их
                                                // вместе с остальными параметрами.
                                                let font_missing =
                                                    self.edit_panel.missing_font.is_some();
                                                ui.add_enabled_ui(!font_missing, |ui| {
                                                    self.edit_panel.draw_effects_section(ui, true)
                                                })
                                                .inner
                                            }
                                        };
                                        content_height_px = ui.min_rect().height();
                                    }
                                });
                            match self.vertical_panel_tab {
                                TypingVerticalMainTab::Parameters => {
                                    self.vertical_panel_params_content_height_px =
                                        content_height_px;
                                }
                                TypingVerticalMainTab::Effects => {
                                    self.vertical_panel_effects_content_height_px =
                                        content_height_px;
                                }
                            }
                            let measured_auto_target_height =
                                compute_typing_vertical_panel_auto_height(
                                    content_height_px,
                                    viewport_target_height,
                                    available_panel_height,
                                );
                            if (measured_auto_target_height
                                - self.vertical_panel_last_auto_target_height_px)
                                .abs()
                                >= 1.0
                            {
                                self.vertical_panel_last_auto_target_height_px =
                                    measured_auto_target_height;
                                self.vertical_panel_resize_revision =
                                    self.vertical_panel_resize_revision.wrapping_add(1);
                                ctx.request_repaint();
                            }
                            if content_height_px > 0.0 && content_height_px < panel_max_height {
                                ctx.request_repaint();
                            }
                        });
                });
            });
        if params_area_response.response.dragged() {
            self.vertical_panel.user_positioned = true;
        }
        if self.vertical_panel.user_positioned {
            self.vertical_panel.pos = Some(params_area_response.response.rect.min);
        }

        let params_rect = params_area_response.response.rect;
        let preview_rect =
            self.draw_create_preview_panel(ctx, canvas_rect, panel_pos.x, panel_pos.y, panel_w);
        let actions_default_anchor = preview_rect.unwrap_or(params_rect);
        let actions_default_pos = egui::pos2(
            actions_default_anchor.min.x,
            actions_default_anchor.max.y + TYPING_VERTICAL_ACTIONS_PANEL_PREVIEW_GAP_PX,
        );
        let mut actions_pos = self
            .vertical_actions_panel
            .pos
            .unwrap_or(actions_default_pos)
            .clamp(
                egui::pos2(actions_min_x, min_y),
                egui::pos2(actions_max_x, max_y),
            );
        // On the «Слои» tab the layer list's inner width-resize (persisted `layers_panel_width`) must be
        // able to widen the panel, so let the Frame grow to at least that width; the «Действия» tab keeps
        // the fixed actions width. (Both tabs share the resulting width.)
        let panel_w_for_tab = if self.actions_panel_tab == TypingActionsPanelTab::Layers
            && !self.vertical_actions_panel.collapsed
        {
            actions_panel_w.max(text_overlays.layers_panel_width())
        } else {
            actions_panel_w
        };
        // While the layout-editor panel floats at the top-left, keep the Actions/Layers panel
        // from being hidden under it: if the two overlap horizontally, drop the panel's top just
        // below the layout panel's bottom edge. Uses the layout panel's last-frame rect from
        // memory (it is drawn after this panel); its Id matches `Area::new("...mode_panel")`.
        if layout_editor_active
            && let Some(layout_rect) =
                ctx.memory(|mem| mem.area_rect(Id::new("typing_layout_editor_mode_panel")))
        {
            let overlaps_x =
                actions_pos.x < layout_rect.right() && actions_pos.x + panel_w_for_tab > layout_rect.left();
            let min_top = layout_rect.bottom() + TYPING_VERTICAL_PANEL_GAP_PX;
            if overlaps_x && actions_pos.y < min_top {
                actions_pos.y = min_top.clamp(min_y, max_y);
            }
        }
        let actions_area_response = egui::Area::new(TYPING_VERTICAL_ACTIONS_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(actions_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w_for_tab);
                ui.set_min_width(panel_w_for_tab);
                ui.set_max_width(panel_w_for_tab);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w_for_tab);
                    ui.set_min_width(panel_w_for_tab);
                    ui.set_max_width(panel_w_for_tab);
                    // 2-tab header (mirrors the Параметры/Эффекты panel): collapse toggle + «Действия» /
                    // «Слои» tabs.
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.vertical_actions_panel.collapsed {
                            "▶"
                        } else {
                            "▼"
                        };
                        let toggle_hint = if self.vertical_actions_panel.collapsed {
                            t!("typing.panel.expand_panel_tooltip")
                        } else {
                            t!("typing.panel.collapse_panel_tooltip")
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.vertical_actions_panel.collapsed =
                                !self.vertical_actions_panel.collapsed;
                        }
                        ui.selectable_value(
                            &mut self.actions_panel_tab,
                            TypingActionsPanelTab::Actions,
                            TypingActionsPanelTab::Actions.label(),
                        );
                        ui.selectable_value(
                            &mut self.actions_panel_tab,
                            TypingActionsPanelTab::Layers,
                            TypingActionsPanelTab::Layers.label(),
                        );
                    });
                    if self.vertical_actions_panel.collapsed {
                        return;
                    }
                    ui.add_space(4.0);
                    match self.actions_panel_tab {
                        TypingActionsPanelTab::Actions => {
                            let actions = match self.mode {
                                TypingTopPanelMode::CreateText => {
                                    self.create_panel.draw_right_section(
                                        ui,
                                        TypingRightSectionInputs {
                                            mask_panel_open: self.mask_panel_open,
                                            clean_overlays_visible: self.clean_overlays_visible,
                                            strict_pixel_movement: self.strict_pixel_movement,
                                            export_default_dir: self.export_default_dir.as_deref(),
                                            export_status: &self.export_status,
                                            export_format: self.export_format,
                                        },
                                    )
                                }
                                TypingTopPanelMode::EditText => self.edit_panel.draw_right_section(
                                    ui,
                                    TypingRightSectionInputs {
                                        mask_panel_open: self.mask_panel_open,
                                        clean_overlays_visible: self.clean_overlays_visible,
                                        strict_pixel_movement: self.strict_pixel_movement,
                                        export_default_dir: self.export_default_dir.as_deref(),
                                        export_status: &self.export_status,
                                        export_format: self.export_format,
                                    },
                                ),
                            };
                            if actions.toggle_mask {
                                self.mask_panel_open = !self.mask_panel_open;
                            }
                            if let Some(visible) = actions.changed_clean_overlays {
                                self.clean_overlays_visible = visible;
                                self.pending_clean_overlays_visible = Some(visible);
                            }
                            if let Some(format) = actions.changed_export_format {
                                self.export_format = format;
                            }
                            if let Some(path) = actions.export_to_folder {
                                self.pending_export_to_folder = Some(path);
                            }
                            if actions.round_text_positions {
                                self.pending_round_text_positions = true;
                            }
                            if actions.create_image_request.is_some() {
                                self.pending_create_image_request = actions.create_image_request;
                            }
                            if let Some(strict_pixel_movement) =
                                actions.changed_strict_pixel_movement
                            {
                                self.strict_pixel_movement = strict_pixel_movement;
                            }
                            self.draw_auto_typing_controls(ui);
                            // Centering assist ("Помочь с центровкой"): a page-anchored guide frame the
                            // user drags to a bubble; the selected text layer stays centered in it.
                            let centering_toggle = ui.checkbox(
                                &mut self.centering_assist_enabled,
                                t!("typing.panel.centering_assist_toggle"),
                            );
                            if centering_toggle.changed()
                                && self.centering_assist_enabled
                                && self.mode == TypingTopPanelMode::EditText
                            {
                                // Toggled ON while editing: re-render the selected overlay immediately so
                                // its mean/median centers are computed and the frame appears without
                                // another edit.
                                self.emit_edit_request();
                            }
                            if self.centering_assist_enabled {
                                // Indented (visible only when enabled): the bound-center selector. Both
                                // centers are already computed while enabled, so switching does NOT
                                // re-render — the reconciliation re-binds the layer to the new center.
                                ui.indent(Id::new("typing.panel.centering_kind_combo_label"), |ui| {
                                    // "Показывать центр": gates ONLY the drawn bound-center marker; the
                                    // frame, handles, and binding stay governed by the assist toggle.
                                    ui.checkbox(
                                        &mut self.centering_show_center,
                                        t!("typing.panel.centering_show_center"),
                                    );
                                    let combo = WheelComboBox::from_label(t!(
                                        "typing.panel.centering_kind_combo_label"
                                    ))
                                    .id_salt("typing.panel.centering_kind_combo_label")
                                    .selected_text(match self.centering_assist_kind {
                                        CenteringAssistCenterKind::Image => {
                                            t!("typing.panel.centering_kind_image")
                                        }
                                        CenteringAssistCenterKind::Mean => {
                                            t!("typing.panel.centering_kind_mean")
                                        }
                                        CenteringAssistCenterKind::Median => {
                                            t!("typing.panel.centering_kind_median")
                                        }
                                    })
                                    .show_ui_with_wheel(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.centering_assist_kind,
                                            CenteringAssistCenterKind::Image,
                                            t!("typing.panel.centering_kind_image"),
                                        );
                                        ui.selectable_value(
                                            &mut self.centering_assist_kind,
                                            CenteringAssistCenterKind::Mean,
                                            t!("typing.panel.centering_kind_mean"),
                                        );
                                        ui.selectable_value(
                                            &mut self.centering_assist_kind,
                                            CenteringAssistCenterKind::Median,
                                            t!("typing.panel.centering_kind_median"),
                                        );
                                    });
                                    if let Some(steps) = combo.wheel_steps {
                                        self.centering_assist_kind = cycle_centering_assist_kind(
                                            self.centering_assist_kind,
                                            steps,
                                        );
                                    }
                                });
                            }
                        }
                        TypingActionsPanelTab::Layers => {
                            text_overlays.draw_layers_tab_body(ui, page_idx);
                        }
                    }
                });
            });
        self.vertical_actions_panel.pos = Some(actions_area_response.response.rect.min);

        if self.mode == TypingTopPanelMode::EditText && changed {
            self.emit_edit_request();
        }
    }

    pub(in crate::tabs::typing) fn build_create_text_render_bundle(
        &self,
        text: String,
        width_px: u32,
    ) -> Result<(TextRenderParams, Value), String> {
        let mut render_params = self
            .create_panel
            .build_render_params_for(text.clone(), width_px.max(1))
            .ok_or_else(|| {
                tf!("typing.errors.fonts_not_found", arg = self.create_panel.fonts_dir.display())
            })?;
        // Request the renderer's mean/median centers only while centering assist is on (the default is
        // the byte-identical no-compute fast path). Both centers are always requested so the bound-center
        // selector can switch without a re-render.
        if self.centering_assist_enabled {
            render_params.extra_info = RenderExtraInfoRequest {
                mean_center: true,
                median_center: true,
            };
        }
        let render_data_json = self
            .create_panel
            .build_render_data_json_for(text, width_px.max(1))
            .ok_or_else(|| {
                tf!("typing.errors.fonts_not_found", arg = self.create_panel.fonts_dir.display())
            })?;
        Ok((render_params, render_data_json))
    }

    pub(in crate::tabs::typing) fn create_editor_font_spec(&self) -> Option<TypingEditorFontSpec> {
        self.create_panel.editor_font_spec()
    }

    /// Shared font source for tab-side renders, built from the create panel's font
    /// list (the create and edit panels load the same fonts). Handed to the tab so
    /// its background render workers resolve fonts by name.
    pub(in crate::tabs::typing) fn font_provider(&self) -> Arc<dyn FontProvider> {
        self.create_panel.font_provider()
    }

    pub(in crate::tabs::typing) fn adjust_create_font_size_by_wheel_steps(&mut self, steps: i32) -> bool {
        if self.mode != TypingTopPanelMode::CreateText {
            return false;
        }
        self.create_panel.adjust_font_size_by_wheel_steps(steps)
    }

    pub(in crate::tabs::typing) fn adjust_selected_text_overlay_font_size_by_wheel_steps(
        &mut self,
        steps: i32,
    ) -> bool {
        if self.mode != TypingTopPanelMode::EditText {
            return false;
        }
        if self.edit_overlay_kind != Some(TypingOverlayKind::Text) {
            return false;
        }
        if !self.edit_panel.adjust_font_size_by_wheel_steps(steps) {
            return false;
        }
        self.emit_edit_request();
        true
    }

    pub(in crate::tabs::typing) fn sync_selected_overlay_for_edit(
        &mut self,
        selected: Option<TypingSelectedOverlayForEdit>,
    ) {
        match selected {
            Some(selected) => {
                let render_data_changed =
                    self.edit_render_data_snapshot != selected.render_data_json;
                let target_changed = self.edit_target.as_ref() != Some(&selected.target);
                // Сохранённое инлайн-выделение текста персонально для одного слоя.
                // Сравниваем выбранный слой с владельцем выделения (а не с
                // `edit_target`, который обнуляется при снятии выбора): иначе повторный
                // выбор того же слоя после потери фокуса выглядел бы как смена слоя и
                // терял бы выделение. Сбрасываем только при переходе на другой слой.
                if self.inline_selection_owner.as_ref() != Some(&selected.target) {
                    self.edit_panel.clear_inline_text_selection();
                    self.inline_selection_owner = Some(selected.target.clone());
                }
                if target_changed || render_data_changed {
                    match selected.overlay_kind {
                        TypingOverlayKind::Text => {
                            self.edit_panel.load_from_selected_overlay(&selected);
                        }
                        TypingOverlayKind::Image => {
                            self.edit_panel
                                .sync_overlay_transform_from_selected_overlay(&selected);
                            if let Some(render_data) = selected.render_data_json.as_ref() {
                                self.edit_panel.load_effects_only_from_render_data(render_data);
                            }
                        }
                    }
                    self.pending_edit_request = None;
                } else {
                    self.edit_panel
                        .sync_overlay_transform_from_selected_overlay(&selected);
                }
                self.edit_overlay_idx = Some(selected.overlay_idx);
                self.edit_target = Some(selected.target.clone());
                self.edit_overlay_kind = Some(selected.overlay_kind);
                self.edit_render_data_snapshot = selected.render_data_json.clone();
                self.mode = TypingTopPanelMode::EditText;
            }
            None => {
                // Снятие выбора НЕ сбрасывает инлайн-выделение: оно остаётся за своим
                // слоем (см. `inline_selection_owner`), пока не выбран другой слой.
                self.edit_overlay_idx = None;
                self.edit_target = None;
                self.edit_overlay_kind = None;
                self.edit_render_data_snapshot = None;
                self.pending_edit_request = None;
                self.mode = TypingTopPanelMode::CreateText;
            }
        }
    }

    pub(in crate::tabs::typing) fn take_edit_request(&mut self) -> Option<TypingOverlayEditRequest> {
        self.pending_edit_request.take()
    }

    /// Drains the pending settings deep-link request raised by a font-group "?" help icon
    /// on either sub-panel (`Some` once until taken), forwarded from `draw`.
    pub(in crate::tabs::typing) fn take_settings_link(
        &mut self,
    ) -> Option<crate::settings_shared::SettingsDeepLink> {
        self.pending_settings_link.take()
    }

    pub(in crate::tabs::typing) fn is_mask_panel_open(&self) -> bool {
        self.mask_panel_open
    }

    pub(in crate::tabs::typing) fn strict_pixel_movement(&self) -> bool {
        self.strict_pixel_movement
    }

    /// Whether centering assist ("Помочь с центровкой") is enabled. When `true`, production text
    /// renders request the renderer's mean/median centers and the canvas draws the page-anchored guide
    /// frame over the selected text layer.
    pub(in crate::tabs::typing) fn centering_assist_enabled(&self) -> bool {
        self.centering_assist_enabled
    }

    /// Which overlay center the assist frame currently binds to (image / mean / median).
    pub(in crate::tabs::typing) fn centering_assist_kind(&self) -> CenteringAssistCenterKind {
        self.centering_assist_kind
    }

    /// Whether the bound-center marker ("Показывать центр") is drawn. Gates ONLY the marker; the guide
    /// frame, handles, and binding are governed by `centering_assist_enabled`. Read on exit to persist.
    pub(in crate::tabs::typing) fn centering_show_center(&self) -> bool {
        self.centering_show_center
    }

    /// Seeds BOTH persisted centering-assist flags ONCE at startup from `user_config.json`
    /// (`TextTab.centering_assist_enabled` / `centering_show_center`). Must not be called every frame
    /// (would override the user's live toggles). The bound-center KIND stays session-only.
    pub(in crate::tabs::typing) fn set_centering_assist_persisted_state(
        &mut self,
        enabled: bool,
        show_center: bool,
    ) {
        self.centering_assist_enabled = enabled;
        self.centering_show_center = show_center;
    }

    pub(in crate::tabs::typing) fn sync_clean_overlays_visible_from_canvas(&mut self, visible: bool) {
        if self.clean_overlays_initialized {
            return;
        }
        self.clean_overlays_visible = visible;
        self.clean_overlays_initialized = true;
    }

    pub(in crate::tabs::typing) fn take_clean_overlays_visible_request(&mut self) -> Option<bool> {
        self.pending_clean_overlays_visible.take()
    }

    pub(in crate::tabs::typing) fn take_export_to_folder_request(&mut self) -> Option<(PathBuf, TypingExportFormat)> {
        self.pending_export_to_folder
            .take()
            .map(|path| (path, self.export_format))
    }

    pub(in crate::tabs::typing) fn take_round_text_positions_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_round_text_positions)
    }

    pub(in crate::tabs::typing) fn take_create_image_request(&mut self) -> Option<TypingCreateImageRequest> {
        self.pending_create_image_request.take()
    }

    pub(in crate::tabs::typing) fn set_export_default_dir(&mut self, path: PathBuf) {
        self.export_default_dir = Some(path);
    }

    pub(in crate::tabs::typing) fn sync_export_status(&mut self, status: TypingExportUiStatus) {
        self.export_status = status;
    }

    pub(super) fn emit_edit_request(&mut self) {
        let Some(target) = self.edit_target.clone() else {
            return;
        };
        let overlay_kind = self.edit_overlay_kind.unwrap_or(TypingOverlayKind::Text);
        self.pending_edit_request = match overlay_kind {
            TypingOverlayKind::Text => {
                // Text editing only applies to overlays.
                let TypingEditTarget::Overlay(overlay_idx) = target else {
                    return;
                };
                // Шрифт оверлея не найден: рендер заблокирован, пока пользователь не
                // выберет другой доступный шрифт. Иначе текст отрисовался бы чужим
                // (подставленным) шрифтом.
                if self.edit_panel.missing_font.is_some() {
                    return;
                }
                let Some(mut render_params) = self.edit_panel.build_render_params() else {
                    return;
                };
                // Request the renderer's mean/median centers only while centering assist is on (the
                // default is the byte-identical no-compute fast path).
                if self.centering_assist_enabled {
                    render_params.extra_info = RenderExtraInfoRequest {
                        mean_center: true,
                        median_center: true,
                    };
                }
                let Some(render_data_json) = self.edit_panel.build_render_data_json_for(
                    self.edit_panel.text.clone(),
                    self.edit_panel.width_px.max(1),
                ) else {
                    return;
                };
                Some(TypingOverlayEditRequest::Text {
                    overlay_idx,
                    render_params: Box::new(render_params),
                    render_data_json,
                    user_scale: self.edit_panel.overlay_scale.clamp(0.05, 20.0),
                    rotation_deg: normalize_angle_deg(self.edit_panel.overlay_rotation_deg),
                })
            }
            TypingOverlayKind::Image => {
                let user_scale = self.edit_panel.overlay_scale.clamp(0.05, 20.0);
                let rotation_deg = normalize_angle_deg(self.edit_panel.overlay_rotation_deg);
                // Изменения во вкладке «Эффекты» требуют перерендера картинки; чистая
                // трансформация (масштаб/угол) применяется на показе без перерендера.
                if self.vertical_panel_tab == TypingVerticalMainTab::Effects {
                    Some(TypingOverlayEditRequest::ImageEffects {
                        target,
                        render_data_json: self.edit_panel.build_image_effects_render_data(),
                        user_scale,
                        rotation_deg,
                    })
                } else {
                    Some(TypingOverlayEditRequest::ImageTransform {
                        target,
                        user_scale,
                        rotation_deg,
                    })
                }
            }
        };
    }

    pub(super) fn draw_create_preview_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        panel_left: f32,
        panel_top: f32,
        panel_width: f32,
    ) -> Option<Rect> {
        if self.mode != TypingTopPanelMode::CreateText {
            return None;
        }

        let min_x = canvas_rect.left();
        let max_x = (canvas_rect.right() - 80.0).max(min_x);
        let min_y = canvas_rect.top();
        let max_y = (canvas_rect.bottom() - 40.0).max(min_y);
        let controls_rect =
            ctx.memory(|mem| mem.area_rect(Id::new(CANVAS_LEFT_TOP_CONTROLS_AREA_ID)));
        let default_pos = controls_rect
            .map(|rect| {
                egui::pos2(
                    rect.left(),
                    rect.bottom() + TYPING_PREVIEW_PANEL_CONTROLS_GAP_PX,
                )
            })
            .unwrap_or(egui::pos2(
                panel_left,
                panel_top + TYPING_PREVIEW_PANEL_CONTROLS_GAP_PX,
            ));
        let panel_pos = self
            .create_preview_panel
            .pos
            .unwrap_or(default_pos)
            .clamp(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));
        let panel_w = TYPING_PREVIEW_PANEL_DEFAULT_WIDTH_PX.min(panel_width.max(220.0));

        let area_response = egui::Area::new(TYPING_PREVIEW_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(panel_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w);
                ui.set_min_width(panel_w);
                ui.set_max_width(panel_w);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_min_width(panel_w);
                    ui.set_max_width(panel_w);
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.create_preview_panel.collapsed {
                            "▶"
                        } else {
                            "▼"
                        };
                        let toggle_hint = if self.create_preview_panel.collapsed {
                            t!("typing.preview.expand_tooltip")
                        } else {
                            t!("typing.preview.collapse_tooltip")
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.create_preview_panel.collapsed =
                                !self.create_preview_panel.collapsed;
                        }
                        ui.label(t!("typing.preview.panel_heading"));
                    });
                    if self.create_preview_panel.collapsed {
                        return;
                    }
                    ui.add_space(4.0);
                    self.create_panel.draw_preview_section(ui);
                });
            });

        self.create_preview_panel.pos = Some(area_response.response.rect.min);
        Some(area_response.response.rect)
    }

    pub(super) fn draw_auto_typing_controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let toggle_label = if self.auto_typing_panel_open {
            t!("typing.autotype.close_tooltip")
        } else {
            t!("typing.autotype.open_tooltip")
        };
        if ui.button(toggle_label).clicked() {
            self.auto_typing_panel_open = !self.auto_typing_panel_open;
        }

        if !self.auto_typing_panel_open {
            return;
        }

        ui.add_space(4.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new(t!("typing.autotype.heading")).strong());
            ui.label(t!("typing.autotype.hotkey_hint"));
            ui.checkbox(&mut self.auto_typing_debug_visuals, t!("typing.autotype.show_debug"));
            ui.add(
                WheelSlider::new(
                    &mut self.auto_typing_extra_downward_shift_percent,
                    -25.0..=50.0,
                )
                .text(t!("typing.autotype.extra_down_offset_label")),
            );
        });
    }
}
