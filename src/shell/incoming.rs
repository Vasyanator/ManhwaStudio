/*
File: src/shell/incoming.rs

Purpose:
The shell's end of external image delivery (Android Share / "Open with", `ms_platform::incoming`):
turns taken inbox batches into ONE `StartupTarget::Images`, takes warm deliveries on a worker,
takes the cold-start batches before the window exists, and drops targets the window cannot open
(releasing their inbox batches off the GUI thread).

Key structures:
- `IncomingTake`: the GUI end of one running take worker.

Key functions:
- `sources_from_batches(batches)`: pure mapping (files in arrival order, Kotlin skips as
  localized `SkippedSource`s, batch folders as inbox batches) plus the folders nothing refers to.
- `spawn_take()`: `take_pending_blocking` + mapping on the `incoming-take` worker.
- `cold_start_target()`: `ms_platform::incoming::init` + mapping, for `startup::run_native` (pre-window).
- `discard_target(target, why)`: logs and releases a target that will not be opened.

Notes:
Where a delivered target goes (open, queue, ask, drop) is decided by the pure table in
`screen.rs`; this file only moves data. Every blocking platform call here runs on a worker, or
before the first window frame (`cold_start_target`, the documented pre-window exception).
*/

use super::target::{ImageSources, StartupTarget, received_count, release_inbox_batches_blocking};
use crate::project::SkippedSource;
use crate::runtime_log;
use ms_platform::incoming::{self, IncomingBatch, IncomingError};
use ms_thread as thread;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// What one take delivered: the merged target (`None` when the batches held nothing to open or
/// report), or the platform error.
pub(super) type TakeResult = Result<Option<StartupTarget>, IncomingError>;

/// Maps taken `batches` (arrival order) onto one ordered image set: each batch's files in the
/// sender's order, its skipped documents as `SkippedSource { path: <sender's name>, message:
/// <localized reason> }` (the technical detail is logged), its folder as an inbox batch. Returns
/// the set (`None` when no batch holds a file or a skip) and the batch folders that ended in no
/// set, which the caller must release.
pub(super) fn sources_from_batches(batches: Vec<IncomingBatch>) -> (Option<ImageSources>, Vec<PathBuf>) {
    let mut merged: Option<ImageSources> = None;
    let mut unused_batches = Vec::new();
    for batch in batches {
        let IncomingBatch { files, skipped, batch_dir } = batch;
        let skipped = skipped
            .into_iter()
            .map(|skip| {
                runtime_log::log_warn(format!("[incoming] '{}' was not received ({:?}): {}", skip.name, skip.kind, skip.detail));
                SkippedSource { path: PathBuf::from(&skip.name), message: skip.kind.user_reason().to_string() }
            })
            .collect();
        match ImageSources::new(files, skipped, batch_dir.clone().into_iter().collect()) {
            Some(sources) => merged = Some(match merged {
                Some(earlier) => earlier.merged(sources),
                None => sources,
            }),
            None => unused_batches.extend(batch_dir),
        }
    }
    (merged, unused_batches)
}

/// [`sources_from_batches`] as a target, releasing the unused batch folders (blocking: a worker,
/// or the pre-window cold start).
fn target_from_batches_blocking(batches: Vec<IncomingBatch>) -> Option<StartupTarget> {
    let (sources, unused_batches) = sources_from_batches(batches);
    for batch in &unused_batches {
        incoming::release_batch_blocking(batch);
    }
    sources.map(StartupTarget::Images)
}

/// The GUI end of a running take worker.
pub(super) struct IncomingTake {
    rx: Receiver<TakeResult>,
}

impl IncomingTake {
    /// The worker's result, once (`None` while it runs). A worker that died without a result is
    /// reported as a bridge error, so the shell never waits forever.
    pub(super) fn poll(&self) -> Option<TakeResult> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(IncomingError::Bridge("the incoming-take worker ended without a result".to_string()))),
        }
    }
}

/// Starts the `incoming-take` worker: `take_pending_blocking` (blocking JNI), then the mapping.
///
/// # Errors
/// The thread could not be spawned (the caller logs it; the pending flag stays raised).
pub(super) fn spawn_take() -> std::io::Result<IncomingTake> {
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new().name("incoming-take".to_string()).spawn(move || {
        let result = incoming::take_pending_blocking().map(target_from_batches_blocking);
        if let Err(mpsc::SendError(result)) = tx.send(result) {
            // The shell is gone (the window closed): nobody will open the images.
            if let Ok(Some(StartupTarget::Images(sources))) = result {
                release_inbox_batches_blocking(sources.inbox_batches());
            }
            runtime_log::log_info("[incoming] a take finished after the main window closed; its images were released");
        }
    })?;
    // Detached: the worker reports through the channel and never needs a join.
    drop(handle);
    Ok(IncomingTake { rx })
}

/// Cold start, for `startup::run_native` BEFORE the main window exists: binds the platform wake-up and takes
/// the batches that started (or reached) the process, as one image set. A platform failure is
/// logged and returned as the localized message for the user; the program continues without the
/// images. Outside Android: `(None, None)`.
pub(crate) fn cold_start_target() -> (Option<StartupTarget>, Option<String>) {
    match incoming::init() {
        Ok(batches) => {
            let count = batches.len();
            let target = target_from_batches_blocking(batches);
            if let Some(target) = &target {
                runtime_log::log_info(format!("[incoming] cold start: {count} batch(es) with {} image(s) to open", received_count(target)));
            }
            (target, None)
        }
        Err(err) => {
            runtime_log::log_error(format!("[incoming] cold start: {err}; the program starts without the received images"));
            (None, Some(err.user_message().to_string()))
        }
    }
}

/// Drops a delivered target that will not be opened (`why` names the reason in the log) and
/// releases its inbox batches on a worker. A spawn failure is logged; the next start sweeps them.
pub(crate) fn discard_target(target: StartupTarget, why: &str) {
    runtime_log::log_info(format!("[incoming] {} received image(s) dropped: {why}", received_count(&target)));
    let sources = match target {
        StartupTarget::Project(_) => return,
        StartupTarget::Images(sources) => sources,
    };
    if sources.inbox_batches().is_empty() {
        return;
    }
    let spawn_result = thread::Builder::new().name("incoming-release".to_string()).spawn(move || release_inbox_batches_blocking(sources.inbox_batches()));
    match spawn_result {
        // Detached: release failures are logged by the platform layer.
        Ok(handle) => drop(handle),
        Err(err) => runtime_log::log_warn(format!("[incoming] could not start the inbox release worker: {err}; the next start sweeps the batches")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_platform::incoming::{IncomingSkip, IncomingSkipKind};

    fn batch(dir: &str, files: &[&str], skipped: &[(&str, IncomingSkipKind)]) -> IncomingBatch {
        IncomingBatch {
            files: files.iter().map(|name| PathBuf::from(format!("/home/u/inbox/{dir}/{name}"))).collect(),
            skipped: skipped.iter().map(|(name, kind)| IncomingSkip { name: (*name).to_string(), kind: *kind, detail: "detail".to_string() }).collect(),
            batch_dir: Some(PathBuf::from(format!("/home/u/inbox/{dir}"))),
        }
    }

    #[test]
    fn batches_merge_in_arrival_order_with_localized_skips() {
        let batches = vec![
            batch("b1", &["p1.png", "p2.png"], &[("broken.heic", IncomingSkipKind::CopyFailed)]),
            batch("b2", &["p3.jpg"], &[("2", IncomingSkipKind::NoStream)]),
        ];
        let (sources, unused) = sources_from_batches(batches);
        let sources = sources.expect("two non-empty batches");
        assert!(unused.is_empty());
        let names: Vec<_> = sources.files().iter().map(|path| path.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or_default().to_string()).collect();
        assert_eq!(names, ["p1.png", "p2.png", "p3.jpg"]);
        assert_eq!(
            sources.skipped(),
            [
                SkippedSource { path: PathBuf::from("broken.heic"), message: IncomingSkipKind::CopyFailed.user_reason().to_string() },
                SkippedSource { path: PathBuf::from("2"), message: IncomingSkipKind::NoStream.user_reason().to_string() },
            ]
        );
        assert_eq!(sources.inbox_batches(), [PathBuf::from("/home/u/inbox/b1"), PathBuf::from("/home/u/inbox/b2")]);
    }

    #[test]
    fn a_batch_of_skips_only_still_opens_to_report_them() {
        let (sources, unused) = sources_from_batches(vec![batch("b1", &[], &[("x.png", IncomingSkipKind::CopyFailed)])]);
        let sources = sources.expect("skips alone are reported");
        assert!(sources.files().is_empty());
        assert_eq!(sources.skipped().len(), 1);
        assert!(unused.is_empty());
    }

    #[test]
    fn empty_batches_are_returned_for_release() {
        let (sources, unused) = sources_from_batches(vec![batch("b1", &[], &[]), batch("b2", &["a.png"], &[])]);
        assert_eq!(sources.map(|sources| sources.files().len()), Some(1));
        assert_eq!(unused, [PathBuf::from("/home/u/inbox/b1")]);
        let (nothing, unused) = sources_from_batches(Vec::new());
        assert!(nothing.is_none());
        assert!(unused.is_empty());
    }

    #[test]
    fn received_count_includes_rejected_documents() {
        let (sources, _) = sources_from_batches(vec![batch("b1", &["a.png", "b.png"], &[("c.png", IncomingSkipKind::CopyFailed)])]);
        let target = StartupTarget::Images(sources.expect("non-empty"));
        assert_eq!(received_count(&target), 3);
        assert_eq!(received_count(&StartupTarget::Project(PathBuf::from("/home/u/titles/T/C"))), 1);
    }
}
