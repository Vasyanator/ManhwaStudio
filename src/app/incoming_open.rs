/*
File: src/app/incoming_open.rs

Purpose:
`MangaApp`'s answer to images another app delivered while the session runs (Android Share /
"Open with"). The window host (`src/shell/`) holds the images and asks through
`request_close_for_incoming(count)`; the session either closes for them
(`PendingCloseAction::OpenIncoming`, taken like every other close) or the user declines
(`take_incoming_declined`). The images themselves never reach `MangaApp`.

Key structures:
- `IncomingOpenRequest`: the open question (count, which question is shown).
- `UnsavedChoice`: the button pressed in the unsaved-changes dialog's open-incoming variant.

Key functions:
- `MangaApp::request_close_for_incoming`, `MangaApp::take_incoming_declined` (window host side).
- `MangaApp::draw_incoming_confirm` (clean session: one-line confirmation) and
  `MangaApp::draw_incoming_unsaved_dialog_body` (dirty session: the body of the existing exit
  dialog, `ExitDialogKind::OpenIncoming`).
- `incoming_decision`: the pure policy.

Notes:
A child module of `app` so it reaches `MangaApp`'s private close state without widening it;
`app.rs` (over the 5000-line gate) only holds the hooks. A dirty session gets the unsaved-changes
dialog only (no second question); a project «Сохранить главу» closes once the save finished
(`pending_save_completion_action`), «Не сохранять» runs the regular DISCARD cleanup; a
single-image session uses its own exit dialog (`crate::single_image`). A close already decided
for another reason wins: the request is refused and the host drops the images.
*/

use super::{ExitDialogKind, MangaApp, PendingCloseAction};
use crate::runtime_log;
use eframe::egui;

/// Which question the open request currently shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IncomingAsk {
    /// Clean session: the one-line "close and open N images?" window.
    Confirm,
    /// Unsaved changes: the exit dialog in its `ExitDialogKind::OpenIncoming` variant.
    UnsavedDialog,
    /// The question was answered with a close; its flow (save, cleanup, close request) runs.
    Answered,
}

/// The window host's open question, alive while it is shown or its close flow runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IncomingOpenRequest {
    /// Received images (files plus already-rejected documents), for the texts.
    count: usize,
    ask: IncomingAsk,
}

/// The button pressed in the open-incoming variant of the unsaved-changes dialog (already acted on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnsavedChoice {
    Cancel,
    Save,
    Discard,
}

/// What [`MangaApp::request_close_for_incoming`] does.
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

/// Pure policy of [`MangaApp::request_close_for_incoming`]. `other_close`: a close for another
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

impl MangaApp {
    /// How many received images the active open-incoming question is about (0 when none).
    pub(crate) fn incoming_open_count(&self) -> usize {
        self.incoming_open.map_or(0, |request| request.count)
    }

    /// The window host received `count` images from another app and asks whether this session
    /// may close for them. Returns `false` when a close for another reason is already pending,
    /// asked or cleaning up (that close wins; the host drops the images). Otherwise shows the
    /// one-line confirmation (clean session) or the unsaved-changes dialog (dirty), or — when
    /// that question or its close flow is already active — only updates the count. GUI thread,
    /// no I/O.
    pub(crate) fn request_close_for_incoming(&mut self, count: usize) -> bool {
        let incoming = PendingCloseAction::OpenIncoming;
        let other_close = self.close_request.is_some_and(|action| action != incoming)
            || self.pending_exit_cleanup.as_ref().is_some_and(|cleanup| cleanup.action != incoming)
            || self.exit_dialog.is_some_and(|kind| kind != ExitDialogKind::OpenIncoming);
        // A request whose dialog went away without a decision (a failed save or discard cleanup)
        // is not active: it is asked again.
        let incoming_active = self.incoming_open.is_some_and(|request| request.ask == IncomingAsk::Confirm)
            || self.exit_dialog == Some(ExitDialogKind::OpenIncoming)
            || self.close_request == Some(incoming)
            || self.pending_exit_cleanup.as_ref().is_some_and(|cleanup| cleanup.action == incoming);
        match incoming_decision(other_close, incoming_active, self.has_unsaved_changes()) {
            IncomingDecision::Refuse => {
                runtime_log::log_info(format!("[app] {count} received image(s) refused: the session is already closing for another reason"));
                return false;
            }
            IncomingDecision::KeepAsking => {
                if let Some(request) = self.incoming_open.as_mut() {
                    request.count = count;
                } else {
                    // Active through the close flow alone: keep the count with it.
                    self.incoming_open = Some(IncomingOpenRequest { count, ask: IncomingAsk::Answered });
                }
            }
            IncomingDecision::UnsavedDialog => self.ask_unsaved_for_incoming(count),
            IncomingDecision::Confirm => self.incoming_open = Some(IncomingOpenRequest { count, ask: IncomingAsk::Confirm }),
        }
        runtime_log::log_info(format!("[app] asking whether to close for {count} received image(s)"));
        true
    }

    /// Whether the user declined the received images since the last call (the window host then
    /// drops them).
    pub(crate) fn take_incoming_declined(&mut self) -> bool {
        std::mem::take(&mut self.incoming_declined)
    }

    /// Shows the unsaved-changes dialog in its open-incoming variant.
    fn ask_unsaved_for_incoming(&mut self, count: usize) {
        self.incoming_open = Some(IncomingOpenRequest { count, ask: IncomingAsk::UnsavedDialog });
        self.exit_dialog = Some(ExitDialogKind::OpenIncoming);
    }

    /// The user declined the received images: the question ends and the host is told.
    pub(super) fn decline_incoming_open(&mut self) {
        runtime_log::log_info("[app] the user declined to close the session for the received images");
        self.incoming_open = None;
        self.incoming_declined = true;
    }

    /// Draws the clean-session confirmation while it is asked and acts on its buttons: «Открыть»
    /// closes the session for the images — unless it gained unsaved changes meanwhile, which
    /// switches to the unsaved-changes dialog — and «Отмена» declines.
    pub(super) fn draw_incoming_confirm(&mut self, ctx: &egui::Context) {
        let Some(request) = self.incoming_open else {
            return;
        };
        if request.ask != IncomingAsk::Confirm {
            return;
        }
        match draw_confirm(ctx, request.count) {
            None => {}
            Some(false) => self.decline_incoming_open(),
            Some(true) if self.has_unsaved_changes() => self.ask_unsaved_for_incoming(request.count),
            Some(true) => {
                self.incoming_open = Some(IncomingOpenRequest { ask: IncomingAsk::Answered, ..request });
                self.finalize_close(PendingCloseAction::OpenIncoming);
            }
        }
    }

    /// The body of the project exit dialog in its `OpenIncoming` variant: the unsaved warning,
    /// the count, and Cancel / «Сохранить главу» / «Не сохранять», each acted on here (Cancel
    /// declines; Save closes after the save-to-project merge; Discard runs the DISCARD cleanup).
    /// The caller closes the dialog on Cancel and keeps it during Save / Discard, as for the
    /// launcher exit.
    pub(super) fn draw_incoming_unsaved_dialog_body(&mut self, ui: &mut egui::Ui) -> Option<UnsavedChoice> {
        let count = self.incoming_open.map_or(0, |request| request.count);
        ui.label(t!("app.exit.unsaved_warning"));
        ui.label(tf!("app.exit.open_incoming_label", count = count));
        ui.add_space(8.0);
        let mut choice = None;
        ui.horizontal(|ui| {
            if ui.button(t!("app.incoming.cancel_button")).clicked() {
                choice = Some(UnsavedChoice::Cancel);
            }
            if ui.button(t!("app.exit.save_chapter_button")).clicked() {
                choice = Some(UnsavedChoice::Save);
            }
            if ui.button(t!("app.exit.discard_button")).clicked() {
                choice = Some(UnsavedChoice::Discard);
            }
        });
        match choice {
            None => {}
            Some(UnsavedChoice::Cancel) => self.decline_incoming_open(),
            Some(UnsavedChoice::Save) => {
                self.pending_save_completion_action = Some(PendingCloseAction::OpenIncoming);
                self.request_save_to_project();
            }
            Some(UnsavedChoice::Discard) => self.start_exit_cleanup(PendingCloseAction::OpenIncoming),
        }
        choice
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
