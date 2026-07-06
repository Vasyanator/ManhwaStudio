/*
File: panel/create_state.rs

Purpose:
Holds part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
create-panel lifecycle/construction, focus and eyedropper tracking, font-group
management, and font-index lookup helpers.

Main responsibilities:
- construct the create-panel state and (re)load fonts and font groups;
- track focused text inputs and eyedropper activation per frame;
- manage the selected font group and pending toggle/group requests;
- spawn and poll background font reloads;
- resolve fonts by key, index, path, or label and filter them by group.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so sibling child
modules under `panel/` can call them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {
    pub(super) fn new(preview_enabled: bool, use_system_fonts: bool) -> Self {
        let fonts_dir = resolve_fonts_dir();
        let local_fonts = load_fonts_from_dir(&fonts_dir);
        let font_groups = load_font_groups(&fonts_dir);
        let auto_enable_system_fonts = local_fonts.is_empty();
        let effective_use_system_fonts = use_system_fonts || auto_enable_system_fonts;
        let fonts = local_fonts;
        let presets_by_name = if preview_enabled {
            load_text_tab_create_presets()
        } else {
            HashMap::new()
        };
        let formula_presets_by_name = load_text_tab_formula_presets();
        let (request_tx, result_rx) = spawn_preview_render_worker();
        let status_line = if auto_enable_system_fonts {
            format!(
                "Локальные шрифты не найдены в {}, загружаю системные",
                fonts_dir.display()
            )
        } else if fonts.is_empty() {
            format!("Не найдено шрифтов в {}", fonts_dir.display())
        } else {
            "Готово к рендеру".to_string()
        };
        let mut state = Self {
            fonts_dir,
            fonts,
            font_groups,
            selected_font_group: None,
            use_system_fonts: effective_use_system_fonts,
            pending_use_system_fonts_toggle_request: None,
            pending_font_group_request: None,
            font_reload_rx: None,
            latest_font_reload_token: 0,
            fonts_reload_in_flight: false,
            combo_font_family_cache: HashMap::new(),
            font_profiles_by_key: HashMap::new(),
            active_font_key: None,
            missing_font: None,
            presets_by_name,
            selected_preset_name: None,
            preset_name_input: String::new(),
            formula_presets_by_name,
            selected_formula_preset_name: None,
            formula_preset_name_input: String::new(),
            preview_enabled,
            selected_font_idx: 0,
            selected_face_idx: 0,
            text: DEFAULT_PREVIEW_TEXT.to_string(),
            text_color: Color32::BLACK,
            text_color_selector: ViewportColorSelector::default(),
            font_size_px: 24.0,
            line_spacing: PxOrPercent::percent(0.0),
            // Default keeps font-pair kerning (byte-identical to the historical
            // `Metric` default), now named `Auto`.
            kerning_mode: KerningMode::Auto,
            kerning: PxOrPercent::percent(0.0),
            glyph_height: PxOrPercent::percent(100.0),
            glyph_width: PxOrPercent::percent(100.0),
            width_px: DEFAULT_PREVIEW_WIDTH_PX,
            align: HorizontalAlign::CENTER,
            global_rotation_deg: 0.0,
            line_placement_percent: 0.0,
            pending_raster_transform: None,
            text_line_mode: TextLineMode::Horizontal,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            shape_layout_kind: TypingShapeLayoutKind::Arc,
            arc_shape_layout: TypingArcShapeLayoutParams::default(),
            circle_shape_layout: TypingCircleShapeLayoutParams::default(),
            spiral_shape_layout: TypingSpiralShapeLayoutParams::default(),
            polygon_shape_layout: TypingPolygonShapeLayoutParams::default(),
            zigzag_shape_layout: TypingZigzagShapeLayoutParams::default(),
            s_curve_shape_layout: TypingSCurveShapeLayoutParams::default(),
            formula_help_open: false,
            text_shape: TextShape::Free,
            text_wrap_mode: TextWrapMode::Aggressive,
            anti_aliasing: AntiAliasingMode::Strong,
            allow_moderate_trees: false,
            shape_min_width_percent: 50.0,
            shape_variant: 5,
            force_bold: false,
            force_italic: false,
            uppercase_text: false,
            trim_extra_spaces: true,
            hanging_punctuation: true,
            new_line_after_sentence: false,
            enable_inline_style_tags: false,
            use_legacy_inline_tags: load_text_tab_use_legacy_inline_tags(),
            overlay_scale: 1.0,
            overlay_rotation_deg: 0.0,
            effect_to_add: AvailableEffectKind::Stroke,
            effects: Vec::new(),
            request_tx,
            result_rx,
            latest_token: 0,
            render_in_flight: false,
            needs_initial_preview: true,
            status_line,
            preview_texture: None,
            preview_size: [1, 1],
            tracked_text_input_ids: Vec::new(),
            text_selection_char_range: None,
            pending_text_selection_restore: None,
            inline_text_target: InlineTextTarget::Source,
            advanced_form_open: false,
            advanced_form_preset: TextFormPreset::FreeNoTree,
            advanced_form_group: None,
            advanced_form_cache: None,
            formed_text: String::new(),
            advanced_text_show_formed: false,
            advanced_form_line_range: (0, 0),
            advanced_form_width_range: (0, 0),
            advanced_form_peak_max: 0,
            advanced_form_peak_base: PeakBase::Min,
            advanced_form_uneven_max: 0,
            advanced_form_conservatism_max: Conservatism::Safe,
            advanced_form_centered: false,
        };
        state.active_font_key = state.current_font_key();
        state.sync_current_font_profile_memory();
        state.sync_selected_formula_preset_by_layout();
        if state.use_system_fonts {
            state.spawn_font_reload();
        }
        state
    }

    pub(super) fn use_system_fonts(&self) -> bool {
        self.use_system_fonts
    }

    pub(super) fn reset_text_input_focus_tracking(&mut self) {
        self.tracked_text_input_ids.clear();
    }

    pub(super) fn track_text_input(&mut self, response: &egui::Response) {
        self.tracked_text_input_ids.push(response.id);
    }

    pub(super) fn has_focused_text_input(&self, ctx: &egui::Context) -> bool {
        let Some(focused) = ctx.memory(|mem| mem.focused()) else {
            return false;
        };
        self.tracked_text_input_ids.contains(&focused)
    }

    pub(super) fn eyedropper_active(&self) -> bool {
        if self.text_color_selector.eyedropper_active() {
            return true;
        }
        self.effects.iter().any(EffectCard::eyedropper_active)
    }

    pub(super) fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        if self.text_color_selector.primary_click_consumed_this_frame() {
            return true;
        }
        self.effects
            .iter()
            .any(EffectCard::eyedropper_consumed_primary_click_this_frame)
    }

    pub(super) fn set_use_system_fonts(&mut self, use_system_fonts: bool) {
        if self.use_system_fonts == use_system_fonts {
            return;
        }
        self.use_system_fonts = use_system_fonts;
        self.spawn_font_reload();
    }

    pub(super) fn take_use_system_fonts_toggle_request(&mut self) -> Option<bool> {
        self.pending_use_system_fonts_toggle_request.take()
    }

    pub(super) fn take_font_group_request(&mut self) -> Option<Option<String>> {
        self.pending_font_group_request.take()
    }

    /// Применяет выбранную группу шрифтов (для синхронизации между панелями).
    /// Возвращает `true`, если группа изменилась.
    pub(super) fn set_font_group(&mut self, group: Option<String>) -> bool {
        if self.selected_font_group == group {
            return false;
        }
        self.selected_font_group = group;
        self.sync_selected_font_group();
        self.ensure_selected_font_in_group();
        if self.preview_enabled {
            self.queue_preview_render();
        }
        true
    }

    pub(super) fn spawn_font_reload(&mut self) {
        self.latest_font_reload_token = self.latest_font_reload_token.wrapping_add(1);
        let token = self.latest_font_reload_token;
        let fonts_dir = self.fonts_dir.clone();
        let use_system_fonts = self.use_system_fonts;
        let (tx, rx) = mpsc::channel::<FontReloadResult>();
        self.font_reload_rx = Some(rx);
        self.fonts_reload_in_flight = true;
        self.status_line = "Обновление списка шрифтов...".to_string();
        let _ = thread::Builder::new()
            .name("typing-font-reload-worker".to_string())
            .spawn(move || {
                let fonts = load_fonts(fonts_dir.as_path(), use_system_fonts);
                let font_groups = load_font_groups(fonts_dir.as_path());
                let _ = tx.send(FontReloadResult {
                    token,
                    fonts,
                    font_groups,
                });
            });
    }

    pub(super) fn poll_font_reload_results(&mut self) {
        let Some(rx) = self.font_reload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if result.token == self.latest_font_reload_token {
                    let previous_font_key = self
                        .active_font_key
                        .clone()
                        .or_else(|| self.current_font_key());
                    self.fonts = result.fonts;
                    self.font_groups = result.font_groups;
                    self.sync_selected_font_group();
                    self.selected_font_idx = previous_font_key
                        .as_deref()
                        .and_then(|font_key| self.find_font_idx_by_key(font_key))
                        .unwrap_or_else(|| {
                            self.selected_font_idx
                                .min(self.fonts.len().saturating_sub(1))
                        });
                    self.ensure_selected_font_in_group();
                    self.clamp_face_index();
                    self.active_font_key = self.current_font_key();
                    self.status_line = if self.fonts.is_empty() {
                        if self.use_system_fonts {
                            "Не найдены ни локальные, ни системные шрифты".to_string()
                        } else {
                            format!("Не найдено шрифтов в {}", self.fonts_dir.display())
                        }
                    } else {
                        "Готово к рендеру".to_string()
                    };
                    if self.preview_enabled
                        && let Some(font_key) = self.current_font_key()
                    {
                        if let Some(profile) = self.font_profiles_by_key.get(&font_key).cloned() {
                            self.apply_render_data_json_with_options(&profile, false);
                            self.clamp_face_index();
                        } else {
                            self.sync_current_font_profile_memory();
                        }
                    }
                    self.queue_preview_render();
                }
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
                self.status_line = "Ошибка обновления списка шрифтов".to_string();
            }
        }
    }

    pub(super) fn fonts_reload_in_flight(&self) -> bool {
        self.fonts_reload_in_flight
    }

    pub(super) fn current_font_key(&self) -> Option<String> {
        self.font_key_by_idx(self.selected_font_idx)
    }

    pub(super) fn font_key_by_idx(&self, idx: usize) -> Option<String> {
        self.fonts
            .get(idx)
            .map(|font| font.path.to_string_lossy().to_string())
    }

    pub(super) fn font_label_by_idx(&self, idx: usize) -> Option<String> {
        self.fonts.get(idx).map(|font| font.label.clone())
    }

    /// Имя шрифта для показа в списке: с уточнением в скобках, только когда
    /// выбраны «Все группы» и имя неоднозначно; при конкретной группе — без скобок.
    pub(super) fn font_display_label(&self, font: &FontEntry) -> String {
        match (self.selected_font_group.is_none(), font.disambig.as_deref()) {
            (true, Some(suffix)) => format!("{} ({})", font.label, suffix),
            _ => font.label.clone(),
        }
    }

    pub(super) fn find_font_idx_by_key(&self, font_key: &str) -> Option<usize> {
        self.fonts
            .iter()
            .position(|font| font_matches_path(font, font_key))
    }

    pub(super) fn filtered_font_indices(&self) -> Vec<usize> {
        self.fonts
            .iter()
            .enumerate()
            .filter_map(|(idx, font)| {
                if self
                    .selected_font_group
                    .as_deref()
                    .is_none_or(|group_name| font_in_group(font, group_name))
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn sync_selected_font_group(&mut self) {
        if self
            .selected_font_group
            .as_ref()
            .is_some_and(|selected| !self.font_groups.iter().any(|group| group == selected))
        {
            self.selected_font_group = None;
        }
    }

    pub(super) fn ensure_selected_font_in_group(&mut self) {
        if self.selected_font_group.as_deref().is_none() {
            return;
        }

        let selected_group_matches = self
            .selected_font_group
            .as_deref()
            .zip(self.fonts.get(self.selected_font_idx))
            .is_some_and(|(group, font)| font_in_group(font, group));
        if selected_group_matches {
            return;
        }

        if let Some(filtered_idx) = self.filtered_font_indices().into_iter().next() {
            self.selected_font_idx = filtered_idx;
            self.selected_face_idx = 0;
        }
    }

    pub(super) fn find_font_idx_by_path_or_label(
        &self,
        font_path: Option<&str>,
        font_label: Option<&str>,
    ) -> Option<usize> {
        let mut selected_idx = None;
        if let Some(path_raw) = font_path {
            selected_idx = self
                .fonts
                .iter()
                .position(|font| font_matches_path(font, path_raw));
        }
        if selected_idx.is_none()
            && let Some(label_raw) = font_label
        {
            let label_norm = label_raw.trim().to_ascii_lowercase();
            if !label_norm.is_empty() {
                selected_idx = self.fonts.iter().position(|font| {
                    font.label.to_ascii_lowercase() == label_norm
                        || font
                            .path
                            .file_stem()
                            .and_then(|v| v.to_str())
                            .map(|stem| stem.to_ascii_lowercase() == label_norm)
                            .unwrap_or(false)
                });
            }
        }
        selected_idx
    }
}
