/*
File: src/studio/incoming_open.rs

Purpose:
`StudioApp`'s answer to images another app delivered while the session runs (Android Share /
"Open with"). The window host (`src/shell/`) holds the images and asks through
`request_close_for_incoming(count)`; the session either closes for them
(`PendingCloseAction::OpenIncoming`, reported like every other close) or the user declines
(`StudioOutput::IncomingDeclined`); both reach the host through `StudioApp::take_outputs`. The
images themselves never reach `StudioApp`.

Key structures:
- The question's state itself (count, which form is shown, the decline latch) is part of the
  session's close intent, `CloseFlow` (`studio/close_flow.rs`).

Key functions:
- `StudioApp::request_close_for_incoming` (window host side).
- `StudioApp::draw_incoming_confirm` (clean session: one-line confirmation).
- `incoming_decision`: the pure policy.

Notes:
A child module of `studio`: the decisions here need the session's dirty state and its running
DISCARD cleanup (`has_unsaved_changes`, `SessionMode::exit_cleanup_action`), so they stay on
`StudioApp` and act on `CloseFlow` through its methods. A dirty session gets the unsaved-changes
dialog only (no second question): its `PendingCloseAction::OpenIncoming` variant is drawn by the
session (`SessionMode::draw_exit_dialog`) — for a project the body is
`ProjectSession::draw_incoming_unsaved_dialog_body` (`studio/session/project/exit_dialog.rs`:
«Сохранить главу» closes once the save finished, «Не сохранять» runs the regular DISCARD
cleanup); an image set uses its own exit dialog (`studio/session/image_set/`). A close already
decided for another reason wins: the request is refused and the host drops the images.
*/

use super::StudioApp;
use crate::runtime_log;
use eframe::egui;

/// What [`StudioApp::request_close_for_incoming`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingDecision {
    /// A close for another reason is already decided or asked: refuse, the host drops the images.
    Refuse,
    /// The open question (or its close flow) is already active: only the count changes.
    KeepAsking,
    /// Clean session: the one-line confirmation.
    Confirm,
    /// Unsaved changes: the unsaved-changes dialog in its open-incoming variant.
    UnsavedDialog,
}

/// Pure policy of [`StudioApp::request_close_for_incoming`]. `other_close`: a close for another
/// reason is pending, asked or cleaning up; `incoming_active`: the open question is shown or its
/// close flow runs.
fn incoming_decision(other_close: bool, incoming_active: bool, has_unsaved_changes: bool) -> IncomingDecision {
    if other_close {
        IncomingDecision::Refuse
    } else if incoming_active {
        IncomingDecision::KeepAsking
    } else if has_unsaved_changes {
        IncomingDecision::UnsavedDialog
    } else {
        IncomingDecision::Confirm
    }
}

impl StudioApp {
    /// The window host received `count` images from another app and asks whether this session
    /// may close for them. Returns `false` when a close for another reason is already pending,
    /// asked or cleaning up (that close wins; the host drops the images). Otherwise shows the
    /// one-line confirmation (clean session) or the unsaved-changes dialog (dirty), or — when
    /// that question or its close flow is already active — only updates the count. GUI thread,
    /// no I/O.
    pub(crate) fn request_close_for_incoming(&mut self, count: usize) -> bool {
        let cleanup_action = self.session.exit_cleanup_action();
        let (other_close, incoming_active) = self.close.incoming_facts(cleanup_action);
        match incoming_decision(other_close, incoming_active, self.has_unsaved_changes()) {
            IncomingDecision::Refuse => {
                runtime_log::log_info(format!("[app] {count} received image(s) refused: the session is already closing for another reason"));
                return false;
            }
            IncomingDecision::KeepAsking => self.close.update_incoming_count(count),
            IncomingDecision::UnsavedDialog => self.close.ask_unsaved_for_incoming(count),
            IncomingDecision::Confirm => self.close.ask_incoming_confirm(count),
        }
        runtime_log::log_info(format!("[app] asking whether to close for {count} received image(s)"));
        true
    }

    /// Draws the clean-session confirmation while it is asked and acts on its buttons: «Открыть»
    /// closes the session for the images — unless it gained unsaved changes meanwhile, which
    /// switches to the unsaved-changes dialog — and «Отмена» declines.
    pub(super) fn draw_incoming_confirm(&mut self, ctx: &egui::Context) {
        let Some(count) = self.close.incoming_confirm_count() else {
            return;
        };
        match draw_confirm(ctx, count) {
            None => {}
            Some(false) => self.close.decline_incoming(),
            Some(true) if self.has_unsaved_changes() => self.close.ask_unsaved_for_incoming(count),
            Some(true) => self.close.answer_incoming_confirm(),
        }
    }
}

/// The one-line confirmation window; `Some(true)` = «Открыть», `Some(false)` = «Отмена».
fn draw_confirm(ctx: &egui::Context, count: usize) -> Option<bool> {
    let mut answer = None;
    egui::Window::new(t!("app.exit.open_incoming_title"))
        .id(egui::Id::new("app.incoming.confirm_label"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(tf!("app.incoming.confirm_label", count = count));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t!("app.incoming.cancel_button")).clicked() {
                    answer = Some(false);
                }
                if ui.button(t!("app.incoming.open_button")).clicked() {
                    answer = Some(true);
                }
            });
        });
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_decision_covers_every_state() {
        // Another close wins over everything.
        for incoming_active in [false, true] {
            for unsaved in [false, true] {
                assert_eq!(incoming_decision(true, incoming_active, unsaved), IncomingDecision::Refuse);
            }
        }
        // An active question only takes the new count (no second dialog).
        assert_eq!(incoming_decision(false, true, false), IncomingDecision::KeepAsking);
        assert_eq!(incoming_decision(false, true, true), IncomingDecision::KeepAsking);
        // Otherwise: the unsaved dialog when dirty, the one-line confirmation when clean.
        assert_eq!(incoming_decision(false, false, true), IncomingDecision::UnsavedDialog);
        assert_eq!(incoming_decision(false, false, false), IncomingDecision::Confirm);
    }
}
