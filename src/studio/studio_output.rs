/*
File: src/studio/studio_output.rs

Purpose:
The ONE outbound channel from a running studio (`StudioApp`) to the window that hosts it
(`src/shell/`, `src/web_entry.rs`): `StudioOutput`, drained by `StudioApp::take_outputs`.

Key structures:
- `StudioOutput`: a close the session decided, a reload request after a page op, the user's
  decline of received images, a new window title.

Key functions:
- `collect_outputs`: the pure ordering and precedence of one drain.

Notes:
The sources stay latches where they are decided (`CloseFlow` close request and decline latch,
the project session's reload request, the image-set controller's pending title): the close
request is also READ by later decisions in the same frame (`CloseFlow::request_os_close`,
`CloseFlow::incoming_facts`), so it cannot become a fire-and-forget event. `take_outputs` turns
the latches into this queue at the moment the host polls. `StudioApp` never sends a root viewport
command itself (no close, no title): the host applies every output.
*/

use super::PendingCloseAction;
use crate::runtime_log;

/// One thing the running studio asks of its window host. A drain is ordered as
/// [`collect_outputs`] documents; the host applies the outputs in that order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StudioOutput {
    /// The window title changed (image set: the pages actually opened, or «Сохранить как» moved
    /// page 0's file). The host sets it through its own title path.
    Title(String),
    /// The session may now close with this action (exit, back to the launcher, or open the
    /// received images the host holds).
    Close(PendingCloseAction),
    /// The user declined to close the session for the received images; the host drops them.
    IncomingDeclined,
    /// A structural page op finished: rebuild the studio from the project on disk (project
    /// sessions only).
    ReloadRequested,
}

/// Orders one drain of the studio's latches: `Title`, `Close`, `IncomingDeclined`,
/// `ReloadRequested`. Precedence: a `Close` suppresses `ReloadRequested` (logged), because the
/// session is torn down by the close and a rebuild would be for nothing. `Close` comes before
/// `IncomingDeclined`: every close resolves the host's pending received images first (it opens
/// them or drops them), so a decline in the same drain finds nothing left to drop.
pub(super) fn collect_outputs(title: Option<String>, close: Option<PendingCloseAction>, incoming_declined: bool, reload_requested: bool) -> Vec<StudioOutput> {
    let mut outputs = Vec::new();
    if let Some(title) = title {
        outputs.push(StudioOutput::Title(title));
    }
    if let Some(action) = close {
        outputs.push(StudioOutput::Close(action));
    }
    if incoming_declined {
        outputs.push(StudioOutput::IncomingDeclined);
    }
    if reload_requested {
        match close {
            Some(action) => runtime_log::log_info(format!("[app] project reload request dropped: the session closes with {action:?} in the same frame")),
            None => outputs.push(StudioOutput::ReloadRequested),
        }
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::{PendingCloseAction, StudioOutput, collect_outputs};

    #[test]
    fn an_empty_drain_reports_nothing() {
        assert!(collect_outputs(None, None, false, false).is_empty());
    }

    /// Every output at once comes in the documented order; the close suppresses the reload.
    #[test]
    fn a_full_drain_is_ordered_and_the_close_wins_over_the_reload() {
        let outputs = collect_outputs(Some("t".to_string()), Some(PendingCloseAction::Exit), true, true);
        assert_eq!(outputs, vec![StudioOutput::Title("t".to_string()), StudioOutput::Close(PendingCloseAction::Exit), StudioOutput::IncomingDeclined]);
    }

    #[test]
    fn a_reload_without_a_close_is_reported_last() {
        let outputs = collect_outputs(Some("t".to_string()), None, true, true);
        assert_eq!(outputs, vec![StudioOutput::Title("t".to_string()), StudioOutput::IncomingDeclined, StudioOutput::ReloadRequested]);
        assert_eq!(collect_outputs(None, None, false, true), vec![StudioOutput::ReloadRequested]);
    }

    /// The close comes before a same-drain decline (the host resolves its pending images on the close).
    #[test]
    fn the_close_precedes_a_decline() {
        for action in [PendingCloseAction::Exit, PendingCloseAction::ReturnToLauncher, PendingCloseAction::OpenIncoming] {
            assert_eq!(collect_outputs(None, Some(action), true, false), vec![StudioOutput::Close(action), StudioOutput::IncomingDeclined]);
        }
    }
}
