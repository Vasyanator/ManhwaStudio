/*
File: src/studio/session/image_set/dialogs.rs

Purpose:
The dialog windows of the image-set (single-image) mode, as stateless draw functions that return
the user's choice for the frame: the exit / leave dialog (Cancel / «Сохранить» / «Не сохранять»),
the «Параметры JPEG» dialog (quality + Save / Cancel), the confirmation for saving into a folder
that already holds files, and the notice listing the files an open skipped. All are task-scoped
`egui::Window` dialogs (allowed by PROJECT_RULES; they are not panels), anchored at the window
centre, with a stable `.id` equal to their i18n key so a language switch keeps their state.

Key functions:
- `draw_exit_dialog()`, `draw_open_incoming_exit_dialog()` (same body, open-received-images wording)
- `draw_jpeg_options_dialog()`, `draw_folder_overwrite_dialog()`, `draw_skipped_notice()`
  (native: only the native controller shows them)

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
    draw_exit_dialog_body(ctx, title, None, t!("app.exit.cancel_button"), write_in_flight, picker_open)
}

/// The same dialog when the session closes to open `count` received images
/// (`PendingCloseAction::OpenIncoming`): its own title, a line naming the count and a neutral
/// «Отмена» instead of «Не выходить». Same button rules as [`draw_exit_dialog`].
pub(crate) fn draw_open_incoming_exit_dialog(ctx: &egui::Context, count: usize, write_in_flight: bool, picker_open: bool) -> Option<ExitChoice> {
    let note = tf!("app.exit.open_incoming_label", count = count);
    draw_exit_dialog_body(ctx, t!("app.exit.open_incoming_title"), Some(note), t!("app.incoming.cancel_button"), write_in_flight, picker_open)
}

/// Body shared by the exit dialogs: `note` is an extra line above the unsaved warning,
/// `cancel_label` the label of the button that keeps the window open.
fn draw_exit_dialog_body(ctx: &egui::Context, title: &str, note: Option<String>, cancel_label: &str, write_in_flight: bool, picker_open: bool) -> Option<ExitChoice> {
    let mut choice = None;
    egui::Window::new(title)
        .id(egui::Id::new("app.exit.single_image_unsaved_warning"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            if let Some(note) = note {
                ui.label(note);
                ui.add_space(4.0);
            }
            ui.label(t!("app.exit.single_image_unsaved_warning"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(cancel_label).clicked() {
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

/// Draws the confirmation shown when the folder chosen for the pages already holds entries
/// (`folder` = its display name): same-named files will be replaced. Returns `Some(true)` for
/// «Сохранить с заменой», `Some(false)` for «Отмена», `None` while unanswered.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_folder_overwrite_dialog(ctx: &egui::Context, folder: &str) -> Option<bool> {
    let mut choice = None;
    egui::Window::new(t!("single_image.save.folder_not_empty_title"))
        .id(egui::Id::new("single_image.save.folder_not_empty_title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(tf!("single_image.save.folder_not_empty_label", folder = folder));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t!("single_image.save.folder_replace_button")).clicked() {
                    choice = Some(true);
                }
                if ui.button(t!("single_image.save.folder_cancel_button")).clicked() {
                    choice = Some(false);
                }
            });
        });
    choice
}

/// Draws the notice listing the files the open left out (`skipped`, each with its localized
/// reason) in a scrollable list. Returns `true` on the frame its «OK» was clicked.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn draw_skipped_notice(ctx: &egui::Context, skipped: &[crate::project::SkippedSource]) -> bool {
    /// Height of the list before it scrolls, in points.
    const LIST_MAX_HEIGHT: f32 = 240.0;
    let mut closed = false;
    egui::Window::new(t!("single_image.skipped.title"))
        .id(egui::Id::new("single_image.skipped.title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(tp!("single_image.skipped.intro_label", skipped.len()));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("single_image.skipped.list").max_height(LIST_MAX_HEIGHT).show(ui, |ui| {
                for file in skipped {
                    let name = file.path.file_name().map_or_else(|| file.path.display().to_string(), |name| name.to_string_lossy().into_owned());
                    ui.strong(name);
                    ui.small(file.message.as_str());
                    ui.add_space(4.0);
                }
            });
            ui.add_space(8.0);
            if ui.button(t!("single_image.skipped.ok_button")).clicked() {
                closed = true;
            }
        });
    closed
}
