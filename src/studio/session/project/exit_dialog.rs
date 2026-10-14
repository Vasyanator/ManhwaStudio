/*
File: src/studio/session/project/exit_dialog.rs

Purpose:
The project session's exit / leave dialog: the unsaved-changes window shown for the OS close
(`PendingCloseAction::Exit`), the in-app «to launcher» button (`ReturnToLauncher`) and images
another app delivered (`OpenIncoming`), with Cancel / «Сохранить главу» / «Не сохранять». Save
arms the save-then-close (`pending_save_completion_action`, ended by `ProjectSession::tick` with
that save) and requests the save; Cancel clears the arm; Discard starts the DISCARD cleanup.

Key structures:
- `UnsavedChoice`: the button pressed in the dialog's open-incoming variant.

Key functions:
- `ProjectSession::draw_exit_dialog`, `ProjectSession::draw_incoming_unsaved_dialog_body`.

Notes:
Which dialog is shown and the received-images question live in `CloseFlow`
(`studio/close_flow.rs`); the image-set session has its own dialog (`image_set/dialogs.rs`), chosen by
`SessionMode::draw_exit_dialog`. The save-then-close is armed ONLY here.
*/

use super::ProjectSession;
use super::save::save_blocker_text;
use crate::studio::PendingCloseAction;
use crate::studio::busy::SaveBlocker;
use crate::studio::session::SessionParts;
use eframe::egui;

/// The button pressed in the open-incoming variant of the unsaved-changes dialog (already acted on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnsavedChoice {
    Cancel,
    Save,
    Discard,
}

impl ProjectSession {
    /// Draws the project exit dialog for `kind` (the close it would end in) under `title` and acts
    /// on its buttons. Returns true when a button started a close flow (save-then-close or the
    /// DISCARD cleanup); the dialog then stays open while that flow runs.
    pub(in crate::studio) fn draw_exit_dialog(&mut self, ctx: &egui::Context, kind: PendingCloseAction, title: &str, parts: &mut SessionParts<'_>) -> bool {
        let mut close_dialog = false;
        let mut handled = false;
        // Save is offered only when a save could start now or one already runs (its end then closes):
        // never during the DISCARD cleanup, a page op, a clean op or an export.
        let save_blocked = save_blocked_in_dialog(self.activity(parts).save_blocker());

        // A stable id: the title is localized and differs per `kind` (PROJECT_RULES "Localization").
        egui::Window::new(title)
            .id(egui::Id::new("app.exit.unsaved_warning"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match kind {
                    PendingCloseAction::Exit => {
                        ui.label(t!("app.exit.unsaved_warning"));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("app.exit.cancel_button")).clicked() {
                                close_dialog = true;
                            }
                            if save_button(ui, save_blocked).clicked() {
                                // The dialog stays open while the save runs; `tick` closes once
                                // it ends (or drops the arm when it fails).
                                self.save_then_close(PendingCloseAction::Exit, parts);
                                handled = true;
                            }
                            if ui.button(t!("app.exit.discard_button")).clicked() {
                                self.start_exit_cleanup(PendingCloseAction::Exit, parts);
                                close_dialog = true;
                                handled = true;
                            }
                        });
                    }
                    PendingCloseAction::ReturnToLauncher => {
                        ui.label(t!("app.exit.unsaved_warning"));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t!("app.exit.cancel_button")).clicked() {
                                close_dialog = true;
                            }
                            if save_button(ui, save_blocked).clicked() {
                                self.save_then_close(PendingCloseAction::ReturnToLauncher, parts);
                                handled = true;
                            }
                            if ui.button(t!("app.exit.discard_button")).clicked() {
                                self.start_exit_cleanup(PendingCloseAction::ReturnToLauncher, parts);
                                close_dialog = true;
                                handled = true;
                            }
                        });
                    }
                    // Same three choices as the launcher exit; the body is `draw_incoming_unsaved_dialog_body` below.
                    PendingCloseAction::OpenIncoming => match self.draw_incoming_unsaved_dialog_body(ui, save_blocked, parts) {
                        None => {}
                        Some(UnsavedChoice::Cancel) => close_dialog = true,
                        Some(UnsavedChoice::Save | UnsavedChoice::Discard) => {
                            close_dialog = true;
                            handled = true;
                        }
                    },
                }
            });

        if close_dialog && !handled {
            // Cancel: the user stays, so no save-then-close may stay armed (a save already running
            // finishes as a plain save).
            self.pending_save_completion_action = None;
            parts.close.close_dialog();
        }
        handled
    }

    /// The dialog's «Сохранить главу»: arms the save-then-close for `action` and requests the save.
    /// `tick` ends the arm with that save — a close on a clean success, otherwise the dialog closes
    /// and the session keeps running (also when the request was refused).
    fn save_then_close(&mut self, action: PendingCloseAction, parts: &mut SessionParts<'_>) {
        self.pending_save_completion_action = Some(action);
        self.request_save_to_project(parts);
    }

    /// The body of the project exit dialog in its `OpenIncoming` variant: the unsaved warning,
    /// the count, and Cancel / «Сохранить главу» / «Не сохранять», each acted on here (Cancel
    /// declines; Save closes after the save-to-project merge; Discard runs the DISCARD cleanup).
    /// The caller closes the dialog on Cancel and keeps it during Save / Discard, as for the
    /// launcher exit.
    fn draw_incoming_unsaved_dialog_body(&mut self, ui: &mut egui::Ui, save_blocked: Option<SaveBlocker>, parts: &mut SessionParts<'_>) -> Option<UnsavedChoice> {
        let count = parts.close.incoming_count();
        ui.label(t!("app.exit.unsaved_warning"));
        ui.label(tf!("app.exit.open_incoming_label", count = count));
        ui.add_space(8.0);
        let mut choice = None;
        ui.horizontal(|ui| {
            if ui.button(t!("app.incoming.cancel_button")).clicked() {
                choice = Some(UnsavedChoice::Cancel);
            }
            if save_button(ui, save_blocked).clicked() {
                choice = Some(UnsavedChoice::Save);
            }
            if ui.button(t!("app.exit.discard_button")).clicked() {
                choice = Some(UnsavedChoice::Discard);
            }
        });
        match choice {
            None => {}
            Some(UnsavedChoice::Cancel) => parts.close.decline_incoming(),
            Some(UnsavedChoice::Save) => self.save_then_close(PendingCloseAction::OpenIncoming, parts),
            Some(UnsavedChoice::Discard) => self.start_exit_cleanup(PendingCloseAction::OpenIncoming, parts),
        }
        choice
    }
}

/// The blocker that disables the exit dialog's «Сохранить главу», if any: every `SaveBlocker`
/// except a save already under way, whose end the save-then-close then waits for.
#[must_use]
fn save_blocked_in_dialog(blocker: Option<SaveBlocker>) -> Option<SaveBlocker> {
    match blocker {
        None | Some(SaveBlocker::SaveInFlight) => None,
        Some(blocked @ (SaveBlocker::PageOperation | SaveBlocker::CleanOperation | SaveBlocker::ExitCleanup | SaveBlocker::Export)) => Some(blocked),
    }
}

/// The dialog's «Сохранить главу» button, disabled with the blocker's hover text while `blocked`.
fn save_button(ui: &mut egui::Ui, blocked: Option<SaveBlocker>) -> egui::Response {
    let response = ui.add_enabled(blocked.is_none(), egui::Button::new(t!("app.exit.save_chapter_button")));
    match blocked.and_then(save_blocker_text) {
        Some(text) => response.on_disabled_hover_text(text),
        None => response,
    }
}

#[cfg(test)]
mod tests {
    use super::{SaveBlocker, save_blocked_in_dialog};

    /// The dialog's Save stays live for a free session and while a save runs (the save-then-close
    /// waits for it); every other blocker — the DISCARD cleanup among them — disables it.
    #[test]
    fn dialog_save_is_disabled_by_every_blocker_but_a_running_save() {
        assert_eq!(save_blocked_in_dialog(None), None);
        assert_eq!(save_blocked_in_dialog(Some(SaveBlocker::SaveInFlight)), None);
        for blocker in [SaveBlocker::PageOperation, SaveBlocker::CleanOperation, SaveBlocker::ExitCleanup, SaveBlocker::Export] {
            assert_eq!(save_blocked_in_dialog(Some(blocker)), Some(blocker));
        }
    }
}
