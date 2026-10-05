/*
File: src/single_image/dialogs.rs

Purpose:
The two dialog windows of the single-image mode, as stateless draw functions that return the
user's choice for the frame: the exit / leave dialog (Cancel / «Сохранить» / «Не сохранять») and
the «Параметры JPEG» dialog (quality + Save / Cancel). Both are task-scoped `egui::Window` dialogs
(allowed by PROJECT_RULES; they are not panels), anchored at the window centre, with a stable
`.id` equal to their i18n key so a language switch keeps their state.

Key functions:
- `draw_exit_dialog()`
- `draw_jpeg_options_dialog()` (native: only the native controller shows it)

Notes:
No state and no I/O here; the caller owns what the choice triggers.
*/

use super::ExitChoice;
use eframe::egui;

/// What the JPEG options dialog answered this frame.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JpegOptionsChoice {
    /// Save with the edited quality.
    Save,
    /// Close the dialog without saving.
    Cancel,
}

/// Draws the single-image exit dialog (`title` names the close action: exit, or exit to the
/// launcher) and returns the button clicked this frame, if any. While the file is being written
/// (`write_in_flight`) only «Cancel» is enabled: closing mid-write could end the process before the
/// atomic replace finished. While the native «Сохранить как» dialog is open (`picker_open`)
/// «Не сохранять» is disabled: that dialog cannot be closed programmatically and would be orphaned
/// over the launcher; «Сохранить» stays, it adopts the running save.
pub(crate) fn draw_exit_dialog(ctx: &egui::Context, title: &str, write_in_flight: bool, picker_open: bool) -> Option<ExitChoice> {
    let mut choice = None;
    egui::Window::new(title)
        .id(egui::Id::new("app.exit.single_image_unsaved_warning"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(t!("app.exit.single_image_unsaved_warning"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t!("app.exit.cancel_button")).clicked() {
                    choice = Some(ExitChoice::Cancel);
                }
                let save = ui.add_enabled(!write_in_flight, egui::Button::new(t!("app.exit.save_image_button")));
                if save.on_disabled_hover_text(t!("app.save.saving")).clicked() {
                    choice = Some(ExitChoice::Save);
                }
                let dont_save = ui.add_enabled(!write_in_flight && !picker_open, egui::Button::new(t!("app.exit.dont_save_button")));
                let disabled_hint = if write_in_flight { t!("app.save.saving").to_owned() } else { tf!("single_image.exit.picker_open_hint", button = t!("app.menu.save_image_as_button")) };
                if dont_save.on_disabled_hover_text(disabled_hint).clicked() {
                    choice = Some(ExitChoice::DontSave);
                }
            });
        });
    choice
}

/// Draws the «Параметры JPEG» dialog over `quality` (edited in place, `1..=100`) and returns the
/// button clicked this frame, if any.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_jpeg_options_dialog(ctx: &egui::Context, quality: &mut u8) -> Option<JpegOptionsChoice> {
    use ms_config::single_image::{JPEG_QUALITY_MAX, JPEG_QUALITY_MIN};
    let mut choice = None;
    egui::Window::new(t!("single_image.jpeg_options.title"))
        .id(egui::Id::new("single_image.jpeg_options.title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(t!("single_image.jpeg_options.quality_label"));
                ui.add(crate::widgets::WheelSlider::new(quality, JPEG_QUALITY_MIN..=JPEG_QUALITY_MAX));
            });
            ui.small(t!("single_image.jpeg_options.lossy_hint"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t!("single_image.jpeg_options.save_button")).clicked() {
                    choice = Some(JpegOptionsChoice::Save);
                }
                if ui.button(t!("single_image.jpeg_options.cancel_button")).clicked() {
                    choice = Some(JpegOptionsChoice::Cancel);
                }
            });
        });
    choice
}
