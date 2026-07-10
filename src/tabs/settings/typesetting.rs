/*
FILE OVERVIEW: src/tabs/settings/typesetting.rs
"Тайп" settings pane: typesetting-related options that affect text rendering and
form iteration across the app.

Main responsibilities:
- Render and persist the app-wide hanging-punctuation list, applied live via
  `crate::text_punctuation`.
- Host two collapsed self-contained typing-panel blocks: the per-effect-kind default
  parameters editor (`EffectDefaultsEditorState`) and the font-settings block
  (`FontSettingsEditorState`: font categories + system-font import/removal).

Key types:
- `SettingsTabState`

Key functions:
- `SettingsTabState::draw_typesetting`

Notes:
- Persistence is delegated to a background thread to avoid blocking the GUI thread;
  the live set is updated synchronously so new renders pick up the change immediately.
*/

use super::{
    SettingsTabState, save_hanging_punctuation, save_rotation_ctrl_wheel_mode, save_text_language,
};
use crate::tabs::typing::rotation_ctrl_wheel::{
    RotationCtrlWheelMode, rotation_ctrl_wheel_mode, set_rotation_ctrl_wheel_mode,
};
use crate::i18n_resolve::resolve_key;
use crate::runtime_log;
use crate::text_punctuation;
use crate::widgets::WheelComboBox;
use ms_text_util::language::{ScriptGroup, set_text_language, text_language};
use ms_thread as thread;

impl SettingsTabState {
    /// Renders the "Тайп" settings pane.
    ///
    /// Currently exposes the app-wide hanging-punctuation editor: saving applies
    /// the value to the live `text_punctuation` set and persists it to
    /// `user_config.json` (`TextTab.hanging_punctuation`) on a background thread.
    pub(super) fn draw_typesetting(&mut self, ui: &mut egui::Ui) {
        ui.heading(t!("settings.typesetting.heading"));

        ui.add_space(8.0);
        self.draw_rotation_ctrl_wheel_setting(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        self.draw_text_language_setting(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(t!("settings.typesetting.hanging_punctuation_label"));
        ui.small(
            t!("settings.typesetting.hanging_punctuation_hint"),
        );

        let mut should_save_punctuation = false;
        ui.horizontal_wrapped(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.hanging_punctuation_input)
                    .font(egui::FontId::new(16.0, egui::FontFamily::Monospace))
                    .desired_width(520.0)
                    .hint_text(text_punctuation::DEFAULT_HANGING_PUNCTUATION),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                should_save_punctuation = true;
            }
            if ui
                .add_enabled(
                    self.hanging_punctuation_input != self.saved_hanging_punctuation,
                    egui::Button::new(t!("settings.typesetting.hanging_punctuation_save_button")),
                )
                .clicked()
            {
                should_save_punctuation = true;
            }
            if ui
                .add_enabled(
                    self.hanging_punctuation_input != text_punctuation::DEFAULT_HANGING_PUNCTUATION,
                    egui::Button::new(t!("settings.typesetting.hanging_punctuation_reset_button")),
                )
                .clicked()
            {
                self.hanging_punctuation_input =
                    text_punctuation::DEFAULT_HANGING_PUNCTUATION.to_string();
                should_save_punctuation = true;
            }
        });

        if should_save_punctuation {
            let punctuation = self.hanging_punctuation_input.clone();
            self.saved_hanging_punctuation = punctuation.clone();
            // Сразу применяем к живому набору, чтобы новые рендеры учли изменение.
            text_punctuation::set_hanging_punctuation(&punctuation);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-hanging-punctuation-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_hanging_punctuation(&path, &punctuation) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist hanging punctuation to {}; error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start hanging punctuation save thread; error={err}"
                ));
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        // Per-effect-kind default parameters, collapsed by default. Self-contained
        // typing-panel widget; it owns its own live-apply + background persistence to
        // `TextTab.effect_defaults`.
        egui::CollapsingHeader::new(t!("settings.typesetting.effect_defaults_header")).id_salt("settings.typesetting.effect_defaults_header")
            .default_open(false)
            .show(ui, |ui| {
                self.effect_defaults_editor.ui(ui);
            });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        // Font-settings block, collapsed by default. Self-contained typing-panel widget;
        // it loads the font category lists off-thread and drives the runtime-global
        // imported-fonts store for system-font import/removal.
        egui::CollapsingHeader::new(t!("settings.typesetting.font_settings_header")).id_salt("settings.typesetting.font_settings_header")
            .default_open(false)
            .show(ui, |ui| {
                self.font_settings_editor.ui(ui);
            });
    }

    /// Renders the typesetting-language selector: a `ScriptGroup` combo followed by
    /// the concrete `TextLanguage` combo within that group.
    ///
    /// Selecting a group switches to that group's first language. Both selections
    /// apply live via `ms_text_util::language::set_text_language` (the typing tab's
    /// `facade.rs` observes `text_language()` each frame and re-runs font-coverage
    /// classification off-thread) and persist `TextTab.text_language` (as
    /// `lang.tag()`) on a background thread. The current language is the single
    /// source of truth (the process-global atomic), so no selection state is stored
    /// on `SettingsTabState`.
    fn draw_text_language_setting(&self, ui: &mut egui::Ui) {
        ui.label(t!("settings.typesetting.text_language_label"));
        ui.small(
            t!("settings.typesetting.text_language_hint"),
        );

        let current = text_language();
        let current_group = current.group();

        // Group combo: selecting a different group switches to that group's first
        // language.
        let mut selected_group = current_group;
        ui.horizontal_wrapped(|ui| {
            WheelComboBox::from_label(t!("settings.typesetting.script_group_label")).id_salt("settings.typesetting.script_group_label")
                .selected_text(resolve_key(current_group.name_key()))
                .show_ui(ui, |ui| {
                    for group in ScriptGroup::all() {
                        ui.selectable_value(&mut selected_group, group, resolve_key(group.name_key()));
                    }
                });
        });

        // Language combo lists only the (possibly new) group's languages. When the
        // group changed this frame, offer that group's first language as selected.
        let mut selected_language = if selected_group == current_group {
            current
        } else {
            selected_group.first_language()
        };
        ui.horizontal_wrapped(|ui| {
            WheelComboBox::from_label(t!("settings.typesetting.language_label")).id_salt("settings.typesetting.language_label")
                .selected_text(resolve_key(selected_language.name_key()))
                .show_ui(ui, |ui| {
                    for language in selected_group.languages() {
                        ui.selectable_value(
                            &mut selected_language,
                            *language,
                            resolve_key(language.name_key()),
                        );
                    }
                });
        });

        if selected_language != current {
            // Apply live first so the change takes effect even if the disk write
            // fails; the typing tab picks it up next frame and recomputes coverage.
            set_text_language(selected_language);
            let path = self.user_settings_file.clone();
            let tag = selected_language.tag().to_string();
            if let Err(err) = thread::Builder::new()
                .name("settings-text-language-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_text_language(&path, &tag) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist text language to {}; error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start text language save thread; error={err}"
                ));
            }
        }
    }

    /// Renders the "Поворот Ctrl+колесо" chooser: which rotation mechanism the typing
    /// tab's Ctrl+wheel gesture drives. Applies the choice to the runtime global
    /// immediately (so the typing tab picks it up on the next wheel event) and persists
    /// it to `user_config.json` (`TextTab.rotation_ctrl_wheel_mode`) on a background
    /// thread.
    fn draw_rotation_ctrl_wheel_setting(&self, ui: &mut egui::Ui) {
        ui.label(t!("settings.typesetting.rotation_ctrl_wheel_label"));
        ui.small(t!("settings.typesetting.rotation_ctrl_wheel_hint"));

        let mut mode = rotation_ctrl_wheel_mode();
        let previous = mode;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Vector, t!("settings.typesetting.rotation_mode_vector"))
                .on_hover_text(
                    t!("settings.typesetting.rotation_mode_vector_hint"),
                );
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Raster, t!("settings.typesetting.rotation_mode_raster"))
                .on_hover_text(t!("settings.typesetting.rotation_mode_raster_hint"));
        });

        if mode != previous {
            // Apply live first so the change takes effect even if the disk write fails.
            set_rotation_ctrl_wheel_mode(mode);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-rotation-ctrl-wheel-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_rotation_ctrl_wheel_mode(&path, mode) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist rotation ctrl-wheel mode to {}; \
                             error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start rotation ctrl-wheel mode save thread; error={err}"
                ));
            }
        }
    }
}
