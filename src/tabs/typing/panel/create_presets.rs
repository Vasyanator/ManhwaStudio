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

It is also the ONE place that maps font diagnostics to colors and wording: the
STATIC per-font coverage classification (`font_coverage.rs`, combobox option
colors + `font_coverage_tooltip`) and the FACTUAL per-render fallback report the
renderer returns (`font_fallback_status_lines`, shown next to the preview).

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so the `panel`
module root and its sibling submodules can call them. `use super::*;` pulls in
the parent module's types and imports.
*/

use super::*;

/// Maximum characters listed in one user-facing character list before it is
/// truncated with a "+N more" suffix. Shared by the static coverage tooltip and
/// the per-render fallback status so a long text can never blow up the panel.
const MAX_SHOWN_CHARS: usize = 15;

/// "Works, but not the way you asked": a font that only partially covers the
/// typesetting language, or a character drawn by a fallback font instead of the
/// selected one. Deliberately not red — both cases still render.
pub(super) const FONT_DIAGNOSTIC_WARNING_COLOR: egui::Color32 =
    egui::Color32::from_rgb(240, 200, 60);

/// "This will not be readable": a font that lacks the writing system entirely, or
/// a character no font in the render base could draw (tofu).
pub(super) const FONT_DIAGNOSTIC_ERROR_COLOR: egui::Color32 =
    egui::Color32::from_rgb(230, 96, 92);

impl TypingCreatePanelState {
    pub(super) fn draw_create_presets_section(&mut self, ui: &mut egui::Ui) {
        if !self.preview_enabled {
            return;
        }
        // The section title comes from the collapsing header (below); the summary
        // is the currently selected preset display name (or the "none" label).
        let preview_enabled = self.preview_enabled;
        let preset_summary = self
            .selected_preset_name
            .clone()
            .unwrap_or_else(|| text_preset_none_label().to_string());
        collapsing_param_section(
            ui,
            "typing.section.presets",
            preview_enabled,
            t!("typing.presets.section_heading"),
            false,
            Some(preset_summary.as_str()),
            |ui| {
                self.draw_create_presets_body(ui);
            },
        );
    }

    /// Body of the create-presets section (moved verbatim from the former
    /// `ui.group(...)`): the preset selector combo plus the save-preset name
    /// input and button. The strong section title is now shown in the collapsing
    /// header, so it is no longer drawn inline here.
    fn draw_create_presets_body(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                let mut names: Vec<String> = self.presets_by_name.keys().cloned().collect();
                names.sort();
                let selected_text = self
                    .selected_preset_name
                    .as_deref()
                    .unwrap_or(text_preset_none_label());
                let prev_selected = self.selected_preset_name.clone();
                let preset_len = names.len() + 1;
                let mut preset_idx = self
                    .selected_preset_name
                    .as_ref()
                    .and_then(|selected| names.iter().position(|name| name == selected))
                    .map(|idx| idx + 1)
                    .unwrap_or(0);
                let preset_combo = WheelComboBox::from_label(t!("typing.presets.current_preset_combo_id")).id_salt("typing.presets.current_preset_combo_id")
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
                        .hint_text(t!("typing.presets.save_preset_button"))
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                self.track_text_input(&preset_name_resp);
                if ui.button(t!("typing.presets.save_button")).clicked() {
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
        // Persist the font's canonical render IDENTITY (original family name), matching
        // the render_data flip; `primary_font_key`/`primary_font_path` stay path-based.
        let primary_font_label = self.font_identity_name_by_idx(self.selected_font_idx);
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
        // Регистрация и детерминированное имя семейства (по путь+индекс начертания)
        // живут в общем виджете `widgets::font_preview`: критично, что `create_panel` и
        // `edit_panel` — две независимые панели с общим egui-`Context`, и один и тот же
        // файл всегда даёт одно имя, а egui хранит данные шрифта по имени (иначе поздняя
        // регистрация затирала бы раннюю, и панель рисовала бы чужой шрифт).
        let family = crate::widgets::ensure_font_family(ctx, font_path, face_index)?;
        self.combo_font_family_cache.insert(
            cache_key,
            crate::widgets::combo_font_family_name(font_path, face_index),
        );
        Some(family)
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
                egui::RichText::new(label).color(FONT_DIAGNOSTIC_WARNING_COLOR)
            }
            FontLanguageSupport::Unsupported => {
                egui::RichText::new(label).color(FONT_DIAGNOSTIC_ERROR_COLOR)
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
/// fully supports the selected typesetting language (no highlight, no tooltip).
///
/// The writing-system name and language name are derived from the currently
/// selected `TextLanguage` (`ms_text_util::language::text_language()`), so the
/// wording is factually correct for any typesetting language, not just Russian.
/// This matches the language `coverage` was classified against: `facade.rs`
/// reloads coverage whenever the typesetting language changes.
fn font_coverage_tooltip(coverage: &FontLanguageCoverage) -> Option<String> {
    let language = ms_text_util::language::text_language();
    // The crate hands us catalog keys (it is GUI-free); resolve them here.
    let language_name = crate::i18n_resolve::resolve_key(language.name_key());
    let script_name = crate::i18n_resolve::resolve_key(language.group().script_name_key());
    match coverage.support {
        FontLanguageSupport::Full => None,
        FontLanguageSupport::Unsupported => Some(tf!("typing.font_coverage.unsupported_tooltip", script_name = script_name, language_name = language_name)),
        FontLanguageSupport::Partial => {
            let list = truncated_char_list(coverage.missing.as_slice());
            Some(tf!("typing.font_coverage.partial_tooltip", language_name = language_name, list = list))
        }
    }
}

/// Renders `chars` as a space-separated list, truncated to [`MAX_SHOWN_CHARS`]
/// with a "+N more" suffix.
///
/// The suffix reuses `typing.font_coverage.more_chars_tooltip`: it is a pure
/// "{shown} … (and N more)" fragment with exactly this meaning, and duplicating
/// the literal into a second key would only let the two drift per locale.
fn truncated_char_list(chars: &[char]) -> String {
    let shown: String = chars
        .iter()
        .take(MAX_SHOWN_CHARS)
        .map(|ch| ch.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let extra = chars.len().saturating_sub(MAX_SHOWN_CHARS);
    if extra > 0 {
        tf!("typing.font_coverage.more_chars_tooltip", shown = shown, extra = extra)
    } else {
        shown
    }
}

/// One user-facing status row of the per-render font diagnostic: the text, the
/// color it is painted in, and the tooltip explaining what it means.
#[derive(Debug)]
pub(super) struct FontFallbackStatusLine {
    pub(super) text: String,
    pub(super) color: egui::Color32,
    pub(super) tooltip: &'static str,
}

/// Turns the renderer's factual fallback report into at most two status rows.
///
/// Row 1 (warning color) lists the characters the deterministic fallback chain
/// drew and the font that drew each group — INFORMATION, not an error: the result
/// is correct and identical on every machine, it just is not the selected
/// typeface. Row 2 (error color) lists characters nothing could draw, which the
/// reader really does lose (a tofu box).
///
/// Returns an empty vector when the selected font served the whole text, so the
/// caller draws nothing at all. Both character lists are truncated by
/// [`truncated_char_list`].
///
/// This is the FACTUAL counterpart of [`font_coverage_tooltip`]: that one judges a
/// FONT against the typesetting LANGUAGE before anything is typed, this one reports
/// what happened to THIS text. Both are kept; they answer different questions.
pub(super) fn font_fallback_status_lines(
    report: &FontFallbackReport,
) -> Vec<FontFallbackStatusLine> {
    let mut lines = Vec::new();
    if !report.fallbacks.is_empty() {
        let list = report
            .fallbacks
            .iter()
            .map(|used| {
                tf!(
                    "typing.font_fallback.entry_label",
                    chars = truncated_char_list(used.chars.as_slice()),
                    font = used.family
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(FontFallbackStatusLine {
            text: tf!("typing.font_fallback.used_status", list = list),
            color: FONT_DIAGNOSTIC_WARNING_COLOR,
            tooltip: t!("typing.font_fallback.used_tooltip"),
        });
    }
    if !report.missing.is_empty() {
        lines.push(FontFallbackStatusLine {
            text: tf!(
                "typing.font_fallback.missing_status",
                chars = truncated_char_list(report.missing.as_slice())
            ),
            color: FONT_DIAGNOSTIC_ERROR_COLOR,
            tooltip: t!("typing.font_fallback.missing_tooltip"),
        });
    }
    lines
}
