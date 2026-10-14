/*
File: src/shell/studio/ui.rs

Purpose:
Drawing of the studio screen's own (non-`StudioApp`) phases: the loading screen (spinner,
image-set progress bar, large-set confirmation, Cancel) and the load error screen. Pure egui
drawing over a snapshot; the phase logic lives in `mod.rs`.

Key structures:
- `LoadingView`: what the loading screen shows (a snapshot of the loading phase + flags).
- `LoadingAction`: the loading screen's button pressed this frame.

Key functions:
- `draw_loading_screen`, `draw_error_screen`.
- `gib_one_decimal`, `progress_fraction`: integer-only formatting helpers.

Notes:
All texts are `studio_bootstrap.*` i18n keys. Native-only (gated with the whole shell host off
wasm).
*/

use super::StudioExit;
use crate::project::single_image::ImageSetEstimate;

/// What the loading screen's buttons asked for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoadingAction {
    /// "Open" on the large-set confirmation.
    OpenLargeSet,
    /// "Cancel" on the confirmation or under the progress bar.
    Cancel,
}

/// What the loading screen shows (a snapshot of the loading phase plus the screen's flags).
#[derive(Debug, Clone, Copy)]
pub(super) struct LoadingView {
    /// The shell deferred an OS close until the worker delivered.
    pub(super) closing: bool,
    /// The target is an image set (progress and Cancel exist only there).
    pub(super) image_set: bool,
    /// The user already pressed Cancel: no more buttons.
    pub(super) cancel_requested: bool,
    pub(super) progress: Option<(usize, usize)>,
    pub(super) confirmation: Option<ImageSetEstimate>,
}

/// Bytes as gibibytes with one decimal (`"2.5"`), rounded to the nearest tenth, integer-only.
fn gib_one_decimal(bytes: u64) -> String {
    const GIB: u128 = 1 << 30;
    let tenths = (u128::from(bytes) * 10 + GIB / 2) / GIB;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// `done / total` as a `0..=1` fraction for a progress bar (`0` for an empty total).
fn progress_fraction(done: usize, total: usize) -> f32 {
    if total == 0 {
        return 0.0;
    }
    // Counts go through `u32` (saturating an absurd count) and then `f64`, which represents every
    // `u32` exactly; the quotient is clamped into `0..=1`, so the one narrowing step below cannot
    // lose anything that matters.
    let done = f64::from(u32::try_from(done).unwrap_or(u32::MAX));
    let total = f64::from(u32::try_from(total).unwrap_or(u32::MAX));
    (done / total).clamp(0.0, 1.0) as f32
}

/// The loading screen: a centered spinner and status label; for an image set the progress bar
/// with a Cancel button, or the large-set confirmation with Open / Cancel. With `view.closing`
/// the label explains that the pending close waits for file operations to finish and no button
/// is offered. Returns the button pressed this frame.
pub(super) fn draw_loading_screen(ui: &mut egui::Ui, view: LoadingView) -> Option<LoadingAction> {
    let mut action = None;
    egui::CentralPanel::default().show(ui, |ui| {
        // Push the block to the vertical center of the window.
        let offset = (ui.available_height() * 0.5 - 60.0).max(0.0);
        ui.add_space(offset);
        ui.vertical_centered(|ui| {
            let buttons = view.image_set && !view.closing && !view.cancel_requested;
            if let (Some(estimate), true) = (view.confirmation, buttons) {
                ui.label(tf!("studio_bootstrap.confirm_large_set_label", count = estimate.count, size = gib_one_decimal(estimate.decoded_bytes)));
                ui.add_space(14.0);
                if ui.add_sized([280.0, 34.0], egui::Button::new(t!("studio_bootstrap.open_button"))).clicked() {
                    action = Some(LoadingAction::OpenLargeSet);
                }
                ui.add_space(6.0);
                if ui.add_sized([280.0, 34.0], egui::Button::new(t!("studio_bootstrap.cancel_button"))).clicked() {
                    action = Some(LoadingAction::Cancel);
                }
                return;
            }
            ui.spinner();
            ui.add_space(10.0);
            if view.closing {
                ui.label(t!("studio_bootstrap.finishing_before_close"));
            } else {
                ui.label(t!("studio_bootstrap.loading"));
            }
            if let Some((done, total)) = view.progress.filter(|&(_, total)| total > 0) {
                ui.add_space(10.0);
                ui.add(egui::ProgressBar::new(progress_fraction(done, total)).desired_width(320.0).text(tf!("studio_bootstrap.opening_images_progress", done = done, total = total)));
            }
            if buttons {
                ui.add_space(14.0);
                if ui.add_sized([280.0, 34.0], egui::Button::new(t!("studio_bootstrap.cancel_button"))).clicked() {
                    action = Some(LoadingAction::Cancel);
                }
            }
        });
    });
    action
}

/// Error screen: the failure text plus "Exit to launcher" / "Exit" buttons; returns the button
/// clicked this frame.
pub(super) fn draw_error_screen(ui: &mut egui::Ui, error_text: &str) -> Option<StudioExit> {
    let mut exit = None;
    egui::CentralPanel::default().show(ui, |ui| {
        let offset = (ui.available_height() * 0.5 - 90.0).max(0.0);
        ui.add_space(offset);
        ui.vertical_centered(|ui| {
            ui.heading(t!("studio_bootstrap.load_failed"));
            ui.add_space(8.0);
            ui.colored_label(ms_theme::status::ERROR, error_text);
            ui.add_space(14.0);
            if ui.add_sized([280.0, 34.0], egui::Button::new(t!("studio_bootstrap.back_to_launcher_button"))).clicked() {
                exit = Some(StudioExit::FailedBack);
            }
            ui.add_space(6.0);
            if ui.add_sized([280.0, 34.0], egui::Button::new(t!("studio_bootstrap.exit_button"))).clicked() {
                exit = Some(StudioExit::FailedExit);
            }
        });
    });
    exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gib_and_progress_helpers() {
        assert_eq!(gib_one_decimal(0), "0.0");
        assert_eq!(gib_one_decimal(5 << 29), "2.5");
        assert_eq!(gib_one_decimal(u64::MAX), "17179869184.0");
        assert!((progress_fraction(1, 4) - 0.25).abs() < f32::EPSILON);
        assert!(progress_fraction(3, 0).abs() < f32::EPSILON);
        assert!((progress_fraction(9, 4) - 1.0).abs() < f32::EPSILON);
    }
}
