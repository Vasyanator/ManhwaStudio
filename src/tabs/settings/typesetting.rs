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

use super::{SettingsTabState, save_hanging_punctuation, save_rotation_ctrl_wheel_mode};
use crate::tabs::typing::rotation_ctrl_wheel::{
    RotationCtrlWheelMode, rotation_ctrl_wheel_mode, set_rotation_ctrl_wheel_mode,
};
use crate::runtime_log;
use crate::text_punctuation;
use ms_thread as thread;

impl SettingsTabState {
    /// Renders the "Тайп" settings pane.
    ///
    /// Currently exposes the app-wide hanging-punctuation editor: saving applies
    /// the value to the live `text_punctuation` set and persists it to
    /// `user_config.json` (`TextTab.hanging_punctuation`) on a background thread.
    pub(super) fn draw_typesetting(&mut self, ui: &mut egui::Ui) {
        ui.heading("Тайп");

        ui.add_space(8.0);
        self.draw_rotation_ctrl_wheel_setting(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label("Висящая пунктуация:");
        ui.small(
            "Символы, которые при включённой висящей пунктуации выносятся за края \
             строки и не идут в счёт её ширины (в т.ч. дефис переноса). Список общий \
             для рендера текста и перебора форм. Пробелы игнорируются.",
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
                    egui::Button::new("Сохранить"),
                )
                .clicked()
            {
                should_save_punctuation = true;
            }
            if ui
                .add_enabled(
                    self.hanging_punctuation_input != text_punctuation::DEFAULT_HANGING_PUNCTUATION,
                    egui::Button::new("По умолчанию"),
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
        egui::CollapsingHeader::new("Стандартные параметры эффектов")
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
        egui::CollapsingHeader::new("Настройки шрифтов")
            .default_open(false)
            .show(ui, |ui| {
                self.font_settings_editor.ui(ui);
            });
    }

    /// Renders the "Поворот Ctrl+колесо" chooser: which rotation mechanism the typing
    /// tab's Ctrl+wheel gesture drives. Applies the choice to the runtime global
    /// immediately (so the typing tab picks it up on the next wheel event) and persists
    /// it to `user_config.json` (`TextTab.rotation_ctrl_wheel_mode`) on a background
    /// thread.
    fn draw_rotation_ctrl_wheel_setting(&self, ui: &mut egui::Ui) {
        ui.label("Поворот Ctrl+колесо:");
        ui.small("Как жест Ctrl+колесо во вкладке «Текст» поворачивает выбранный оверлей.");

        let mut mode = rotation_ctrl_wheel_mode();
        let previous = mode;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Vector, "Векторный")
                .on_hover_text(
                    "На уровне рендера текста. Менее удобный, но идеальная чёткость \
                     после поворота.",
                );
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Raster, "Растровый")
                .on_hover_text("Быстрее, но даёт менее чёткую картинку.");
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
