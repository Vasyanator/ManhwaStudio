/*
File: src/studio/close_flow.rs

Purpose:
The close intent of one studio session, as ONE owned struct: which exit / leave dialog is shown
(keyed by the `PendingCloseAction` it would end in), the outbound close request the window host
takes, and the state of the window host's "close for N received images?" question.

Key structures:
- `CloseFlow`
- `OsCloseDecision` + `os_close_decision`: the pure policy of the OS close button;
  `OsClosePolicy`: the per-session-kind answer to a second OS close over the exit dialog.
- `IncomingOpenRequest` / `IncomingAsk`: the received-images question and which form it shows.

Key functions:
- `CloseFlow::request_os_close`, `finalize`, `take_close_request`, the dialog accessors.
- `CloseFlow::end_save_then_close` + `SaveThenCloseEnd`: how a save-then-close ends.
- `CloseFlow::incoming_*` / `ask_*` / `decline_incoming` / `take_incoming_declined`.

Notes:
Owned struct with a narrow API: it never reaches `StudioApp`'s fields. Decisions that need the
session's dirty state or the persistence jobs (`StudioApp::request_os_close`,
`request_close_for_incoming`, the exit dialog buttons) stay on `StudioApp` and pass those facts in.
`pending_save_completion_action` (the armed save-then-close) is NOT here: it stays with the
project save state (`studio/session/project/`), which reports the save's end here.
*/

use super::PendingCloseAction;
use crate::runtime_log;

/// What an OS close-button press does to a running session ([`CloseFlow::request_os_close`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OsCloseDecision {
    /// A close is already decided this frame; keep it.
    KeepPending,
    /// Unsaved work and no exit dialog yet: ask save / discard / cancel first.
    OpenExitDialog,
    /// The exit dialog is open and the session's policy keeps it: this close is ignored.
    KeepDialog,
    /// Nothing to ask (no unsaved work, or the exit dialog is already open): exit.
    Exit,
}

/// What a second OS close does while the session's exit dialog is open; per session kind
/// (`SessionMode::os_close_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OsClosePolicy {
    /// Exit without asking again (project: the staging survives an exit, nothing is lost).
    SecondCloseExits,
    /// Ignore it and keep the dialog (image set: an exit discards the scratch with the unsaved edits).
    SecondCloseKeepsDialog,
}

/// Pure core of [`CloseFlow::request_os_close`]. A second OS close while the exit dialog is open
/// follows `policy`: exit without asking again, or keep the dialog.
fn os_close_decision(close_pending: bool, has_unsaved_changes: bool, exit_dialog_open: bool, policy: OsClosePolicy) -> OsCloseDecision {
    if close_pending {
        OsCloseDecision::KeepPending
    } else if exit_dialog_open && policy == OsClosePolicy::SecondCloseKeepsDialog {
        OsCloseDecision::KeepDialog
    } else if has_unsaved_changes && !exit_dialog_open {
        OsCloseDecision::OpenExitDialog
    } else {
        OsCloseDecision::Exit
    }
}

/// How the save a save-then-close waited for ended ([`CloseFlow::end_save_then_close`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveThenCloseEnd {
    /// The save succeeded and nothing is left unsaved: the close may proceed.
    Saved,
    /// The save succeeded, but the session gained unsaved work meanwhile (edits during the merge).
    StillUnsaved,
    /// The save failed, was refused, or aborted before its worker ran.
    Failed,
}

/// Which question the received-images request currently shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingAsk {
    /// Clean session: the one-line "close and open N images?" window.
    Confirm,
    /// Unsaved changes: the exit dialog in its `PendingCloseAction::OpenIncoming` variant.
    UnsavedDialog,
    /// The question was answered with a close; its flow (save, cleanup, close request) runs.
    Answered,
}

/// The window host's open question, alive while it is shown or its close flow runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IncomingOpenRequest {
    /// Received images (files plus already-rejected documents), for the texts.
    count: usize,
    ask: IncomingAsk,
}

/// The session's close intent. `Default` = nothing asked, nothing decided.
#[derive(Debug, Default)]
pub(super) struct CloseFlow {
    /// The exit / leave dialog currently shown, keyed by the close it would end in: `Exit` for the
    /// OS close button, `ReturnToLauncher` for the in-app button, `OpenIncoming` for received images.
    dialog: Option<PendingCloseAction>,
    /// The close this session decided on, waiting for the window host to take it
    /// (`take_close_request`, drained by `StudioApp::take_outputs`). `StudioApp` never closes the
    /// window itself.
    close_request: Option<PendingCloseAction>,
    /// The window host's question "close for N received images?" while it is open or its close
    /// flow runs (`studio/incoming_open.rs`); `None` otherwise.
    incoming: Option<IncomingOpenRequest>,
    /// Latched when the user declined the received images; taken by the window host
    /// (`take_incoming_declined`, drained by `StudioApp::take_outputs`), which then drops them.
    incoming_declined: bool,
}

impl CloseFlow {
    /// The exit dialog currently shown, as the close it would end in.
    pub(super) fn dialog(&self) -> Option<PendingCloseAction> {
        self.dialog
    }

    /// Shows the exit dialog for `action` (replacing any other variant).
    pub(super) fn open_dialog(&mut self, action: PendingCloseAction) {
        self.dialog = Some(action);
    }

    /// Hides the exit dialog.
    pub(super) fn close_dialog(&mut self) {
        self.dialog = None;
    }

    /// Records that the session may now close with `action`; the window host acts on it through
    /// [`CloseFlow::take_close_request`]. A later decision in the same frame replaces an earlier one.
    pub(super) fn finalize(&mut self, action: PendingCloseAction) {
        self.close_request = Some(action);
    }

    /// The close decided and not yet taken by the window host (tests read it without taking it).
    #[cfg(test)]
    fn close_request(&self) -> Option<PendingCloseAction> {
        self.close_request
    }

    /// Takes the close this session decided on, if any; `None` means keep running.
    pub(super) fn take_close_request(&mut self) -> Option<PendingCloseAction> {
        self.close_request.take()
    }

    /// Applies the OS close button: keeps a close already pending; with the exit dialog open and
    /// `policy` keeping it, ignores the close (logged); with unsaved work and no exit dialog open,
    /// opens the `Exit` dialog (declining a shown received-images question first, because that
    /// dialog never asked about exiting); otherwise requests `Exit`. Returns the decision taken.
    /// Arms nothing: only the dialog's Save starts a save-then-close.
    pub(super) fn request_os_close(&mut self, has_unsaved_changes: bool, policy: OsClosePolicy) -> OsCloseDecision {
        // The open-incoming dialog never asked about exiting: an OS close over it asks again
        // (and drops the received images, `studio/incoming_open.rs`).
        let exit_dialog_open = self.dialog.is_some_and(|kind| kind != PendingCloseAction::OpenIncoming);
        let decision = os_close_decision(self.close_request.is_some(), has_unsaved_changes, exit_dialog_open, policy);
        match decision {
            OsCloseDecision::KeepPending => {}
            OsCloseDecision::KeepDialog => {
                runtime_log::log_info(format!("[app] OS close ignored: the exit dialog ({:?}) is open and this session keeps it until a button is pressed", self.dialog));
            }
            OsCloseDecision::OpenExitDialog => {
                if self.dialog == Some(PendingCloseAction::OpenIncoming) {
                    self.decline_incoming();
                }
                self.dialog = Some(PendingCloseAction::Exit);
            }
            OsCloseDecision::Exit => self.finalize(PendingCloseAction::Exit),
        }
        decision
    }

    /// Ends a save-then-close for `action` (armed by an exit dialog's Save). The dialog closes either
    /// way. `Saved` finalizes `action`; otherwise the session keeps running and the next close asks
    /// again. Returns whether the close was finalized.
    pub(super) fn end_save_then_close(&mut self, action: PendingCloseAction, end: SaveThenCloseEnd) -> bool {
        self.dialog = None;
        match end {
            SaveThenCloseEnd::Saved => {
                self.finalize(action);
                true
            }
            SaveThenCloseEnd::StillUnsaved | SaveThenCloseEnd::Failed => {
                runtime_log::log_info(format!("[app] save-then-close for {action:?} ended without a close ({end:?}); the session keeps running"));
                false
            }
        }
    }

    /// How many received images the active open-incoming question is about (0 when none).
    pub(super) fn incoming_count(&self) -> usize {
        self.incoming.map_or(0, |request| request.count)
    }

    /// The `(other_close, incoming_active)` facts of `StudioApp::request_close_for_incoming`, given
    /// the action of a running discard cleanup (`cleanup_action`). `other_close`: a close for
    /// another reason is pending, asked or cleaning up; `incoming_active`: the received-images
    /// question is shown or its close flow runs. A request whose dialog went away without a decision
    /// (a failed save or discard cleanup) is not active: it is asked again.
    pub(super) fn incoming_facts(&self, cleanup_action: Option<PendingCloseAction>) -> (bool, bool) {
        let incoming = PendingCloseAction::OpenIncoming;
        let other_close = self.close_request.is_some_and(|action| action != incoming)
            || cleanup_action.is_some_and(|action| action != incoming)
            || self.dialog.is_some_and(|kind| kind != incoming);
        let incoming_active = self.incoming.is_some_and(|request| request.ask == IncomingAsk::Confirm)
            || self.dialog == Some(incoming)
            || self.close_request == Some(incoming)
            || cleanup_action == Some(incoming);
        (other_close, incoming_active)
    }

    /// An active question takes the new `count`; when it is active through its close flow alone
    /// (no request recorded), the count is kept with an `Answered` request.
    pub(super) fn update_incoming_count(&mut self, count: usize) {
        if let Some(request) = self.incoming.as_mut() {
            request.count = count;
        } else {
            // Active through the close flow alone: keep the count with it.
            self.incoming = Some(IncomingOpenRequest { count, ask: IncomingAsk::Answered });
        }
    }

    /// Shows the clean-session one-line confirmation for `count` images.
    pub(super) fn ask_incoming_confirm(&mut self, count: usize) {
        self.incoming = Some(IncomingOpenRequest { count, ask: IncomingAsk::Confirm });
    }

    /// Shows the unsaved-changes dialog in its open-incoming variant.
    pub(super) fn ask_unsaved_for_incoming(&mut self, count: usize) {
        self.incoming = Some(IncomingOpenRequest { count, ask: IncomingAsk::UnsavedDialog });
        self.dialog = Some(PendingCloseAction::OpenIncoming);
    }

    /// The image count while the one-line confirmation is shown; `None` otherwise.
    pub(super) fn incoming_confirm_count(&self) -> Option<usize> {
        self.incoming.filter(|request| request.ask == IncomingAsk::Confirm).map(|request| request.count)
    }

    /// The confirmation was answered «Открыть» on a clean session: the question becomes `Answered`
    /// and the session closes for the images.
    pub(super) fn answer_incoming_confirm(&mut self) {
        if let Some(request) = self.incoming.as_mut() {
            request.ask = IncomingAsk::Answered;
        }
        self.finalize(PendingCloseAction::OpenIncoming);
    }

    /// The user declined the received images: the question ends and the host is told.
    pub(super) fn decline_incoming(&mut self) {
        runtime_log::log_info("[app] the user declined to close the session for the received images");
        self.incoming = None;
        self.incoming_declined = true;
    }

    /// Whether the user declined the received images since the last call.
    pub(super) fn take_incoming_declined(&mut self) -> bool {
        std::mem::take(&mut self.incoming_declined)
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseFlow, OsCloseDecision, OsClosePolicy, PendingCloseAction, SaveThenCloseEnd, os_close_decision};

    const EXITS: OsClosePolicy = OsClosePolicy::SecondCloseExits;
    const KEEPS: OsClosePolicy = OsClosePolicy::SecondCloseKeepsDialog;

    /// OS close button: a close decided earlier in the frame wins; unsaved work asks once (the
    /// exit dialog); a clean session exits. A second press while the dialog is open exits for a
    /// project (`SecondCloseExits`) and is ignored for an image set (`SecondCloseKeepsDialog`).
    #[test]
    fn os_close_decision_covers_every_state() {
        for policy in [EXITS, KEEPS] {
            for unsaved in [false, true] {
                for dialog in [false, true] {
                    assert_eq!(os_close_decision(true, unsaved, dialog, policy), OsCloseDecision::KeepPending);
                }
            }
            assert_eq!(os_close_decision(false, true, false, policy), OsCloseDecision::OpenExitDialog);
            assert_eq!(os_close_decision(false, false, false, policy), OsCloseDecision::Exit);
        }
        assert_eq!(os_close_decision(false, true, true, EXITS), OsCloseDecision::Exit);
        assert_eq!(os_close_decision(false, false, true, EXITS), OsCloseDecision::Exit);
        assert_eq!(os_close_decision(false, true, true, KEEPS), OsCloseDecision::KeepDialog);
        assert_eq!(os_close_decision(false, false, true, KEEPS), OsCloseDecision::KeepDialog);
    }

    /// Image set: a second OS close over its exit dialog keeps the dialog and requests nothing; the
    /// dialog's buttons still decide. An OS close over the received-images variant still asks.
    #[test]
    fn second_os_close_keeps_the_image_set_dialog() {
        let exit = Some(PendingCloseAction::Exit);
        let mut flow = CloseFlow::default();
        assert_eq!(flow.request_os_close(true, KEEPS), OsCloseDecision::OpenExitDialog);
        assert_eq!(flow.request_os_close(true, KEEPS), OsCloseDecision::KeepDialog);
        assert_eq!((flow.dialog(), flow.close_request()), (exit, None));
        assert_eq!(flow.request_os_close(false, KEEPS), OsCloseDecision::KeepDialog);
        assert_eq!((flow.dialog(), flow.close_request()), (exit, None));
        // The in-app «to launcher» dialog is kept the same way.
        let mut launcher = CloseFlow::default();
        launcher.open_dialog(PendingCloseAction::ReturnToLauncher);
        assert_eq!(launcher.request_os_close(true, KEEPS), OsCloseDecision::KeepDialog);
        assert_eq!(launcher.close_request(), None);
        // Not an exit dialog: the received-images variant asks about exiting instead.
        let mut incoming = CloseFlow::default();
        incoming.ask_unsaved_for_incoming(2);
        assert_eq!(incoming.request_os_close(true, KEEPS), OsCloseDecision::OpenExitDialog);
        assert_eq!(incoming.dialog(), exit);
    }

    /// The close intent of the project exit dialog across its lifetime, as `StudioApp` and
    /// `ProjectSession` drive it. `armed` mirrors `ProjectSession::pending_save_completion_action`:
    /// armed ONLY by the dialog's Save, cleared by Cancel, and taken when its save ends
    /// (`end_save_then_close`), whatever the outcome. Rows: (step, dialog, close request, armed).
    #[test]
    fn close_intent_lifetime() {
        let exit = Some(PendingCloseAction::Exit);
        let mut flow = CloseFlow::default();
        let mut armed: Option<PendingCloseAction> = None;
        let mut rows = Vec::new();
        // The dialog's Save arms; the save's end takes the arm and tells the flow.
        let save = |armed: &mut Option<PendingCloseAction>| *armed = Some(PendingCloseAction::Exit);
        let end = |flow: &mut CloseFlow, armed: &mut Option<PendingCloseAction>, end: SaveThenCloseEnd| {
            if let Some(action) = armed.take() {
                flow.end_save_then_close(action, end);
            }
        };

        // OS close with unsaved work opens the dialog and arms nothing.
        assert_eq!(flow.request_os_close(true, EXITS), OsCloseDecision::OpenExitDialog);
        rows.push(("open dialog", flow.dialog(), flow.close_request(), armed));
        // Cancel closes the dialog; nothing stays armed.
        flow.close_dialog();
        armed = None;
        rows.push(("cancel", flow.dialog(), flow.close_request(), armed));
        // Re-open, then Save: the dialog stays open while the save runs.
        flow.request_os_close(true, EXITS);
        save(&mut armed);
        rows.push(("save", flow.dialog(), flow.close_request(), armed));
        // The save fails: the arm is gone, the dialog closes, no close is requested.
        end(&mut flow, &mut armed, SaveThenCloseEnd::Failed);
        rows.push(("save failed", flow.dialog(), flow.close_request(), armed));
        // Save again; it succeeds but the session gained unsaved work meanwhile: same as a failure.
        flow.request_os_close(true, EXITS);
        save(&mut armed);
        end(&mut flow, &mut armed, SaveThenCloseEnd::StillUnsaved);
        rows.push(("save left unsaved", flow.dialog(), flow.close_request(), armed));
        // Save, then Cancel while it runs: the save finishes as a plain save, nothing closes.
        flow.request_os_close(true, EXITS);
        save(&mut armed);
        flow.close_dialog();
        armed = None;
        end(&mut flow, &mut armed, SaveThenCloseEnd::Saved);
        rows.push(("cancel during save", flow.dialog(), flow.close_request(), armed));
        // Save and a clean success: the dialog closes and the close is requested.
        flow.request_os_close(true, EXITS);
        save(&mut armed);
        end(&mut flow, &mut armed, SaveThenCloseEnd::Saved);
        rows.push(("saved", flow.dialog(), flow.close_request(), armed));
        let taken = flow.take_close_request();
        rows.push(("taken", flow.dialog(), flow.close_request(), armed));
        assert_eq!(taken, exit);

        // A second OS close while the project dialog is open exits without asking.
        let mut second = CloseFlow::default();
        let mut second_armed = None;
        second.request_os_close(true, EXITS);
        save(&mut second_armed);
        assert_eq!(second.request_os_close(true, EXITS), OsCloseDecision::Exit);
        rows.push(("second os close", second.dialog(), second.close_request(), second_armed));
        // Discard: the dialog stays open while the delete job runs; on success the app closes the
        // dialog and finalizes the cleanup's action.
        let mut discard = CloseFlow::default();
        discard.request_os_close(true, EXITS);
        rows.push(("discard running", discard.dialog(), discard.close_request(), None));
        discard.close_dialog();
        discard.finalize(PendingCloseAction::Exit);
        rows.push(("discard done", discard.dialog(), discard.close_request(), None));

        assert_eq!(
            rows,
            vec![
                ("open dialog", exit, None, None),
                ("cancel", None, None, None),
                ("save", exit, None, exit),
                ("save failed", None, None, None),
                ("save left unsaved", None, None, None),
                ("cancel during save", None, None, None),
                ("saved", None, exit, None),
                ("taken", None, None, None),
                ("second os close", exit, exit, exit),
                ("discard running", exit, None, None),
                ("discard done", None, exit, None),
            ]
        );
    }

    /// A clean session's OS close exits at once and arms nothing; an OS close over the
    /// received-images dialog declines the images and asks about exiting instead.
    #[test]
    fn os_close_over_the_incoming_dialog_declines_and_asks_to_exit() {
        let mut clean = CloseFlow::default();
        assert_eq!(clean.request_os_close(false, EXITS), OsCloseDecision::Exit);
        assert_eq!(clean.close_request(), Some(PendingCloseAction::Exit));
        assert_eq!(clean.dialog(), None);

        let mut flow = CloseFlow::default();
        flow.ask_unsaved_for_incoming(3);
        assert_eq!(flow.dialog(), Some(PendingCloseAction::OpenIncoming));
        assert_eq!(flow.incoming_count(), 3);
        assert_eq!(flow.request_os_close(true, EXITS), OsCloseDecision::OpenExitDialog);
        assert_eq!(flow.dialog(), Some(PendingCloseAction::Exit));
        assert_eq!(flow.incoming_count(), 0);
        assert!(flow.take_incoming_declined());
        assert!(!flow.take_incoming_declined());
    }

    /// The received-images facts: another close wins; the question is active while asked, while its
    /// dialog is shown, while its close is pending and while its cleanup runs.
    #[test]
    fn incoming_facts_track_the_question_and_its_close_flow() {
        let incoming = Some(PendingCloseAction::OpenIncoming);
        let mut flow = CloseFlow::default();
        assert_eq!(flow.incoming_facts(None), (false, false));
        assert_eq!(flow.incoming_facts(Some(PendingCloseAction::Exit)), (true, false));
        assert_eq!(flow.incoming_facts(incoming), (false, true));

        flow.ask_incoming_confirm(2);
        assert_eq!(flow.incoming_confirm_count(), Some(2));
        assert_eq!(flow.incoming_facts(None), (false, true));
        flow.answer_incoming_confirm();
        assert_eq!(flow.incoming_confirm_count(), None);
        assert_eq!(flow.close_request(), incoming);
        assert_eq!(flow.incoming_count(), 2);
        assert_eq!(flow.incoming_facts(None), (false, true));
        flow.update_incoming_count(5);
        assert_eq!(flow.incoming_count(), 5);

        let mut other = CloseFlow::default();
        other.open_dialog(PendingCloseAction::ReturnToLauncher);
        assert_eq!(other.incoming_facts(None), (true, false));
        // Active through the close flow alone: the count is kept with an `Answered` request.
        let mut flow_only = CloseFlow::default();
        flow_only.update_incoming_count(4);
        assert_eq!(flow_only.incoming_count(), 4);
        assert_eq!(flow_only.incoming_confirm_count(), None);
    }
}
