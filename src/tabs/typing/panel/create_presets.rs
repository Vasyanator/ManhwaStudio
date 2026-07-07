/*
File: panel/create_presets.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
create-panel preset and formula-preset apply/save UI, combo-box font-family
binding, the initial preview request, and the face-index clamp.

Main responsibilities:
- draw and apply/save named create presets and formula-layout presets;
- bind an egui font family for combo-box option rendering;
- issue the initial preview render request and clamp the selected face index.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so the `panel`
module root and its sibling submodules can call them. `use super::*;` pulls in
the parent module's types and imports.
*/

use super::*;

impl TypingCreatePanelState {
    pub(super) fn draw_create_presets_section(&mut self, ui: &mut egui::Ui) {
        if !self.preview_enabled {
            return;
        }
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Пресеты").strong());
                let mut names: Vec<String> = self.presets_by_name.keys().cloned().collect();
                names.sort();
                let selected_text = self
                    .selected_preset_name
                    .as_deref()
                    .unwrap_or(TEXT_PRESET_NONE_LABEL);
                let prev_selected = self.selected_preset_name.clone();
                let preset_len = names.len() + 1;
                let mut preset_idx = self
                    .selected_preset_name
                    .as_ref()
                    .and_then(|selected| names.iter().position(|name| name == selected))
                    .map(|idx| idx + 1)
                    .unwrap_or(0);
                let preset_combo = WheelComboBox::from_label("Текущий пресет")
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(preset_idx == 0, TEXT_PRESET_NONE_LABEL)
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
                if let Some(steps) = preset_combo.wheel_steps {
                    cycle_wrapped_index(&mut preset_idx, preset_len, steps);
                }
                self.selected_preset_name = if preset_idx == 0 {
                    None
                } else {
                    names.get(preset_idx - 1).cloned()
                };
                if self.selected_preset_name != prev_selected
                    && let Some(name) = self.selected_preset_name.clone()
                {
                    self.apply_preset_by_name(name);
                    self.queue_preview_render();
                }
            });
            ui.horizontal(|ui| {
                let preset_name_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name_input)
                        .id_salt("typing_preset_name_input")
                        .hint_text("Сохранить пресет")
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                self.track_text_input(&preset_name_resp);
                if ui.button("Сохранить").clicked() {
                    self.save_current_preset();
                }
            });
        });
    }

    pub(super) fn apply_preset_by_name(&mut self, name: String) {
        let Some(preset) = self.presets_by_name.get(&name).cloned() else {
            return;
        };
        self.font_profiles_by_key = preset.font_profiles;

        let target_idx = self
            .find_font_idx_by_key(&preset.primary_font_key)
            .or_else(|| {
                self.find_font_idx_by_path_or_label(
                    preset.primary_font_path.as_deref(),
                    preset.primary_font_label.as_deref(),
                )
            });
        if let Some(idx) = target_idx {
            self.selected_font_idx = idx;
        }
        self.active_font_key = self.current_font_key();
        if let Some(font_key) = self.current_font_key() {
            if let Some(profile) = self.font_profiles_by_key.get(&font_key).cloned() {
                self.apply_render_data_json_with_options(&profile, false);
            } else {
                self.selected_face_idx = 0;
                self.sync_current_font_profile_memory();
            }
        }
        self.clamp_face_index();
        self.selected_preset_name = Some(name);
    }

    pub(super) fn save_current_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let preset_name = self.preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }

        self.sync_current_font_profile_memory();

        let mut font_profiles = self.font_profiles_by_key.clone();
        let current_profile = self.build_current_font_profile_json();
        for idx in 0..self.fonts.len() {
            if let Some(key) = self.font_key_by_idx(idx) {
                font_profiles
                    .entry(key)
                    .or_insert_with(|| current_profile.clone());
            }
        }
        let primary_font_key = self.current_font_key().unwrap_or_default();
        let primary_font_path = self
            .fonts
            .get(self.selected_font_idx)
            .map(|font| font.path.to_string_lossy().to_string());
        let primary_font_label = self.font_label_by_idx(self.selected_font_idx);
        self.presets_by_name.insert(
            preset_name.clone(),
            TypingCreatePreset {
                primary_font_key,
                primary_font_path,
                primary_font_label,
                font_profiles,
            },
        );
        self.selected_preset_name = Some(preset_name.clone());

        let presets = self.presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-create-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_create_presets(&presets);
            });
    }

    pub(super) fn apply_formula_preset_by_name(&mut self, name: String) -> bool {
        let Some(preset) = self.formula_presets_by_name.get(&name).cloned() else {
            return false;
        };
        self.formula_layout = preset.layout;
        self.selected_formula_preset_name = Some(name);
        true
    }

    pub(super) fn save_current_formula_preset(&mut self) {
        let preset_name = self.formula_preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }
        self.formula_presets_by_name.insert(
            preset_name.clone(),
            TypingFormulaPreset {
                layout: self.formula_layout.clone(),
            },
        );
        self.selected_formula_preset_name = Some(preset_name);
        let presets = self.formula_presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-formula-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_formula_presets(&presets);
            });
    }

    pub(super) fn swap_formula_xy_expressions(&mut self) {
        std::mem::swap(
            &mut self.formula_layout.x_expr,
            &mut self.formula_layout.y_expr,
        );
        self.selected_formula_preset_name = None;
    }

    pub(super) fn sync_selected_formula_preset_by_layout(&mut self) {
        self.selected_formula_preset_name =
            self.formula_presets_by_name
                .iter()
                .find_map(|(name, preset)| {
                    if formula_layout_approx_eq(&self.formula_layout, &preset.layout) {
                        Some(name.clone())
                    } else {
                        None
                    }
                });
    }

    pub(super) fn ensure_combo_font_family(
        &mut self,
        ctx: &egui::Context,
        font_path: &Path,
        face_index: usize,
    ) -> Option<egui::FontFamily> {
        let cache_key = (font_path.to_path_buf(), face_index);
        // Имя egui-семейства детерминированно выводится из (путь, индекс начертания):
        // один и тот же файл всегда даёт одно имя, разные файлы — разные имена. Это
        // критично, потому что `create_panel` и `edit_panel` — две независимые панели
        // с общим egui-`Context`. При последовательной нумерации обе генерировали
        // совпадающие имена (`typing-panel-combo-font-1` …) для РАЗНЫХ файлов, а egui
        // хранит данные шрифта по имени — поздняя регистрация затирала раннюю, и одна
        // панель начинала рисовать чужой шрифт (в т.ч. в окне продвинутой формы).
        let font_name = combo_font_family_name(font_path, face_index);
        let family = egui::FontFamily::Name(font_name.clone().into());
        if is_font_family_bound(ctx, &family) {
            self.combo_font_family_cache.insert(cache_key, font_name);
            return Some(family);
        }

        let font_bytes = fs::read(font_path).ok()?;
        let mut font_data = egui::FontData::from_owned(font_bytes);
        font_data.index = face_index as u32;
        ctx.add_font(egui::epaint::text::FontInsert::new(
            font_name.as_str(),
            font_data,
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Name(font_name.clone().into()),
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
        self.combo_font_family_cache.insert(cache_key, font_name);
        if is_font_family_bound(ctx, &family) {
            Some(family)
        } else {
            None
        }
    }

    pub(super) fn draw_font_combo_option(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        font_path: &Path,
        face_index: usize,
        selected: bool,
        coverage: &FontLanguageCoverage,
    ) -> bool {
        let prev_override = ui.style().override_font_id.clone();
        if let Some(family) = self.ensure_combo_font_family(ui.ctx(), font_path, face_index) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(14.0, family));
        }
        // Highlight fonts that do not fully support the program language.
        let text = match coverage.support {
            FontLanguageSupport::Full => egui::RichText::new(label),
            FontLanguageSupport::Partial => {
                egui::RichText::new(label).color(egui::Color32::from_rgb(240, 200, 60))
            }
            FontLanguageSupport::Unsupported => {
                egui::RichText::new(label).color(egui::Color32::from_rgb(230, 96, 92))
            }
        };
        let mut response = ui.selectable_label(selected, text);
        if let Some(tooltip) = font_coverage_tooltip(coverage) {
            response = response.on_hover_text(tooltip);
        }
        let clicked = response.clicked();
        ui.style_mut().override_font_id = prev_override;
        clicked
    }

    pub(super) fn ensure_initial_preview_request(&mut self) {
        if !self.preview_enabled {
            return;
        }
        if !self.needs_initial_preview {
            return;
        }
        self.needs_initial_preview = false;
        self.queue_preview_render();
    }

    pub(super) fn clamp_face_index(&mut self) {
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let max_idx = font.faces.len().saturating_sub(1);
            self.selected_face_idx = self.selected_face_idx.min(max_idx);
        } else {
            self.selected_face_idx = 0;
        }
    }
}

/// Build the hover tooltip for a font dropdown item, or `None` when the font
/// fully supports the program language (no highlight, no tooltip).
fn font_coverage_tooltip(coverage: &FontLanguageCoverage) -> Option<String> {
    match coverage.support {
        FontLanguageSupport::Full => None,
        FontLanguageSupport::Unsupported => Some(
            "Шрифт не поддерживает кириллицу — систему письма русского языка. \
             Текст этим шрифтом не отобразится и будет заменён другим шрифтом."
                .to_string(),
        ),
        FontLanguageSupport::Partial => {
            const MAX_SHOWN: usize = 15;
            let shown: String = coverage
                .missing
                .iter()
                .take(MAX_SHOWN)
                .collect::<Vec<_>>()
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let extra = coverage.missing.len().saturating_sub(MAX_SHOWN);
            let list = if extra > 0 {
                format!("{shown} … (и ещё {extra})")
            } else {
                shown
            };
            Some(format!(
                "Шрифт частично поддерживает русский язык. Не хватает символов: {list}."
            ))
        }
    }
}
