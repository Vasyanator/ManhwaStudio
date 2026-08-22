/*
File: panel/facade.rs

Purpose:
Holds the `impl TypingTopPanelState` inherent block extracted from `panel.rs`.
This is the public-facing top-panel state facade: mode management,
selected-overlay edit sync + request queue, auto-typing settings, and the BODIES
of the panel-dock tabs this state owns.

Main responsibilities:
- bracket the frame: `begin_frame` (font upkeep, background-job polling, preview
  render pump) before the dock, `end_frame` (settings deep-link drain) after it;
- draw the four dock tab bodies — `draw_preview_tab_body`, `draw_params_tab_body`,
  `draw_effects_tab_body`, `draw_actions_tab_body` — into the `Ui` the dock gives
  them. The surrounding panel (position, size, collapse, tab header) belongs to
  the dock layout; nothing here creates an `Area` or a `Frame` of its own;
- turn what a section reports into a queued request for `tab.rs` to drain.

Notes:
Method visibility was escalated one level because the impl moved a directory
deeper: the former `pub(super)` methods are now `pub(in crate::tabs::typing)` so
the sibling `tab.rs` can still call them, and the former private methods are now
`pub(super)`. `use super::*;` pulls in the parent module's types and imports.
*/

use super::*;
use crate::tabs::typing::psd_export::FontPostScriptNames;

impl TypingTopPanelState {
    /// Per-frame prologue: font-list upkeep, background-job polling and the
    /// preview render pump.
    ///
    /// Separate from the tab bodies because every one of them lives in the panel
    /// dock now: the dock must see THIS frame's polled state, not the previous
    /// frame's, so the caller runs this first, then the dock, then
    /// [`TypingTopPanelState::end_frame`]. Must be called exactly once per frame.
    pub(in crate::tabs::typing) fn begin_frame(&mut self, ctx: &egui::Context) {
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
        // Presets belong to the CREATE panel only (`preview_enabled`): install the
        // off-thread seed, finish the one-shot `user_config` -> `fonts/presets.json`
        // migration once its background read lands, adopt what another app instance wrote,
        // and surface any background save failure instead of losing it.
        self.create_panel.poll_preset_store_events();
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
    }

    /// Per-frame epilogue: drains the in-app settings deep-link a font-group "?"
    /// help icon raised on either sub-panel.
    ///
    /// Must run AFTER the dock drew this frame's tabs, so a click bubbles up in
    /// the SAME frame the app polls `take_settings_link`. Draining before the
    /// draw would leave a click dormant for a frame — and a rapid second click
    /// would then survive the tab switch and unexpectedly re-navigate to
    /// Settings when the user later returns to the typing tab.
    pub(in crate::tabs::typing) fn end_frame(&mut self) {
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

    /// Body of the «Параметры» dock tab (`typing.params`).
    ///
    /// In «Создание» it draws the preset row plus the full text parameters; in
    /// «Редактирование» it draws the selected layer's parameters, or — for an
    /// IMAGE layer — only its transform, because a foreign picture has no text
    /// parameters. A settled edit emits the re-render request itself: the panel
    /// that changed is the one that knows something changed.
    ///
    /// `extras` is the tab's own persisted state, handed in by the dock (the tab
    /// is declared with `PanelTab::show_with_extras`). It carries the
    /// expanded/collapsed flag of every collapsible section drawn below, which is
    /// how that state survives a restart — egui memory alone cannot, this build
    /// compiles eframe without the `persistence` feature.
    pub(in crate::tabs::typing) fn draw_params_tab_body(
        &mut self,
        ui: &mut egui::Ui,
        extras: &mut TabExtras,
    ) {
        self.active_main_tab = TypingMainTab::Parameters;
        // Для image-оверлея вкладка «Параметры» показывает только трансформацию, но вкладка
        // «Эффекты» доступна так же, как для текста — эффекты применяются к сторонней картинке.
        let image_edit_only = self.mode == TypingTopPanelMode::EditText
            && self.edit_overlay_kind == Some(TypingOverlayKind::Image);
        if self.mode == TypingTopPanelMode::CreateText {
            self.create_panel.draw_create_presets_section(ui, extras);
            ui.add_space(6.0);
        }
        // The text-params panel is grouped into labelled collapsible sections, so a
        // floating heading above them would group nothing and is dropped. The
        // image-only panel is NOT sectioned, so it keeps its heading.
        if image_edit_only {
            ui.label(egui::RichText::new(t!("typing.panel.image_params_heading")).strong());
        }
        let mut changed = false;
        // ONE preset binding per pass: the pickers below report a confirmed cell
        // edit into it, and the set is persisted once, after the pass.
        let mut presets = ColorPresetsBinding::new(Some(self.color_presets.presets_mut()));
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
                            .draw_params_section(ui, extras, true, false, &mut presets);
                    }
                    TypingTopPanelMode::EditText => {
                        if image_edit_only {
                            changed |= self
                                .edit_panel
                                .draw_image_transform_only_section(ui, false);
                        } else {
                            changed |= self.edit_panel.draw_edit_params_section(
                                ui,
                                extras,
                                true,
                                false,
                                &mut presets,
                            );
                        }
                    }
                });
        });
        // Reading the verdict is the binding's last use, so its borrow of the store
        // ends here and the store can be asked to persist itself.
        if presets.presets_changed() {
            self.color_presets.save();
        }
        if changed && self.mode == TypingTopPanelMode::EditText {
            self.emit_edit_request();
        }
    }

    /// Body of the «Эффекты» dock tab (`typing.effects`).
    ///
    /// Available for both text and image layers. While the edited layer's font is
    /// missing the whole section is disabled: effects re-render the layer, and a
    /// re-render with a substituted font would silently change what is on the page.
    pub(in crate::tabs::typing) fn draw_effects_tab_body(&mut self, ui: &mut egui::Ui) {
        self.active_main_tab = TypingMainTab::Effects;
        // ONE preset binding per pass; see `draw_params_tab_body`.
        let mut presets = ColorPresetsBinding::new(Some(self.color_presets.presets_mut()));
        let changed = match self.mode {
            TypingTopPanelMode::CreateText => {
                self.create_panel
                    .draw_effects_section(ui, true, &mut presets)
            }
            TypingTopPanelMode::EditText => {
                let font_missing = self.edit_panel.missing_font.is_some();
                ui.add_enabled_ui(!font_missing, |ui| {
                    self.edit_panel.draw_effects_section(ui, true, &mut presets)
                })
                .inner
            }
        };
        // Reading the verdict is the binding's last use, so its borrow of the store
        // ends here and the store can be asked to persist itself.
        if presets.presets_changed() {
            self.color_presets.save();
        }
        if changed && self.mode == TypingTopPanelMode::EditText {
            self.emit_edit_request();
        }
    }

    /// Body of the «Действия» dock tab (`typing.actions`): mask / clean-overlay /
    /// import / export actions, the «Авто-тайп» block, and the centering assist.
    ///
    /// Everything the section reports is turned into a queued request here; the
    /// tab itself performs no project work.
    pub(in crate::tabs::typing) fn draw_actions_tab_body(&mut self, ui: &mut egui::Ui) {
        let inputs = TypingRightSectionInputs {
            mask_panel_open: self.mask_panel_open,
            clean_overlays_visible: self.clean_overlays_visible,
            strict_pixel_movement: self.strict_pixel_movement,
            export_default_dir: self.export_default_dir.as_deref(),
            export_status: &self.export_status,
            export_format: self.export_format,
        };
        let actions = match self.mode {
            TypingTopPanelMode::CreateText => self.create_panel.draw_right_section(ui, inputs),
            TypingTopPanelMode::EditText => self.edit_panel.draw_right_section(ui, inputs),
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
        if let Some(strict_pixel_movement) = actions.changed_strict_pixel_movement {
            self.strict_pixel_movement = strict_pixel_movement;
        }
        self.draw_auto_typing_controls(ui);
        self.draw_centering_assist_controls(ui);
    }

    /// Centering assist ("Помочь с центровкой"): a page-anchored guide frame the
    /// user drags to a bubble; the selected text layer stays centered in it.
    fn draw_centering_assist_controls(&mut self, ui: &mut egui::Ui) {
        let centering_toggle = ui
            .checkbox(
                &mut self.centering_assist_enabled,
                t!("typing.panel.centering_assist_toggle"),
            )
            .on_hover_text(t!("typing.panel.centering_assist_hotkey_hint"));
        if centering_toggle.changed()
            && self.centering_assist_enabled
            && self.mode == TypingTopPanelMode::EditText
        {
            // Toggled ON while editing: re-render the selected overlay immediately so
            // its mean/median centers are computed and the frame appears without
            // another edit.
            self.emit_edit_request();
        }
        if !self.centering_assist_enabled {
            return;
        }
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
            let combo = WheelComboBox::from_label(t!("typing.panel.centering_kind_combo_label"))
                .id_salt("typing.panel.centering_kind_combo_label")
                .selected_text(match self.centering_assist_kind {
                    CenteringAssistCenterKind::Image => t!("typing.panel.centering_kind_image"),
                    CenteringAssistCenterKind::Mean => t!("typing.panel.centering_kind_mean"),
                    CenteringAssistCenterKind::Median => t!("typing.panel.centering_kind_median"),
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
                self.centering_assist_kind =
                    cycle_centering_assist_kind(self.centering_assist_kind, steps);
            }
        });
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

    /// `identity -> per-face PostScript names` for the current font list, for the PSD
    /// export (whose job carries neither the font list nor the provider). Built from the
    /// create panel's list, like `font_provider`.
    pub(in crate::tabs::typing) fn font_post_script_names(&self) -> FontPostScriptNames {
        self.create_panel.font_post_script_names()
    }

    /// Resolves a font reference persisted by an OLDER build (path and/or name in any
    /// historical form) to the current render identity; `None` when nothing matches.
    /// Used by the tab's schema-1 -> schema-2 conversion of stored overlays.
    pub(in crate::tabs::typing) fn resolve_legacy_font_identity(
        &self,
        font_path: Option<&str>,
        font_name: Option<&str>,
    ) -> Option<String> {
        self.create_panel
            .resolve_legacy_font_identity(font_path, font_name)
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

    /// Flips centering assist ("Помочь с центровкой"), exactly as the panel checkbox does.
    ///
    /// Used by the `H` hotkey. Turning it ON while editing re-emits the edit request so the
    /// renderer computes the mean/median centers and the guide frame appears without a further
    /// text/param edit — the same side effect the checkbox has.
    pub(in crate::tabs::typing) fn toggle_centering_assist(&mut self) {
        self.centering_assist_enabled = !self.centering_assist_enabled;
        if self.centering_assist_enabled && self.mode == TypingTopPanelMode::EditText {
            self.emit_edit_request();
        }
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

    /// Flips the «Показывать клин» state and queues the canvas request, exactly as the panel
    /// checkbox does.
    ///
    /// Used by the `C` hotkey so the checkbox and the hotkey share one source of truth: the panel
    /// field is the authority after the one-shot [`Self::sync_clean_overlays_visible_from_canvas`]
    /// seed, and the queued request is drained by the tab into the canvas in the same frame.
    pub(in crate::tabs::typing) fn toggle_clean_overlays_visible(&mut self) {
        self.clean_overlays_visible = !self.clean_overlays_visible;
        self.pending_clean_overlays_visible = Some(self.clean_overlays_visible);
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

    /// Binds the character table's PROJECT favorite list to the open title's
    /// document (`ProjectPaths::char_favorites_file`, TITLE-scoped).
    ///
    /// Mirrors [`Self::set_export_default_dir`]: called every frame from
    /// `TypingTabState::draw`. The store ignores a repeated identical path, so
    /// only a real title change re-reads the file.
    ///
    /// Only the EDIT panel hosts the character-table window (its button lives on
    /// the edit accordion), so the create panel's table is deliberately left
    /// unbound — binding it would read a document for a window never opened there.
    pub(in crate::tabs::typing) fn set_project_favorites_path(&mut self, path: PathBuf) {
        self.edit_panel
            .char_table
            .set_project_favorites_path(Some(path));
    }

    /// Binds the tab's color presets to the open title's document
    /// (`ProjectPaths::color_presets_file`, TITLE-scoped) and drives its loader.
    ///
    /// Mirrors [`Self::set_project_favorites_path`]: called every frame from
    /// `TypingTabState::draw`. The store ignores a repeated identical path, so only
    /// a real title change re-reads the file; the `poll` in the same call is what
    /// delivers a finished background read, since the load is started from here and
    /// nowhere else.
    pub(in crate::tabs::typing) fn set_color_presets_path(&mut self, path: PathBuf) {
        self.color_presets.set_path(Some(path));
        self.color_presets.poll();
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
                if self.active_main_tab == TypingMainTab::Effects {
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

    /// `true` while the panel is in «Создание» mode — the only mode in which the
    /// «Превью текста» dock tab has anything to show.
    #[must_use]
    pub(in crate::tabs::typing) fn is_create_text_mode(&self) -> bool {
        self.mode == TypingTopPanelMode::CreateText
    }

    /// Body of the «Превью текста» dock tab: the create panel's preview section
    /// (status line, per-render font diagnostics, the rendered image).
    ///
    /// Called by the panel dock from `TypingHooks::draw_canvas_overlay_top_left`;
    /// the panel owns no position, size or collapse state of its own any more —
    /// the dock's layout does.
    pub(in crate::tabs::typing) fn draw_preview_tab_body(&mut self, ui: &mut egui::Ui) {
        self.create_panel.draw_preview_section(ui);
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
