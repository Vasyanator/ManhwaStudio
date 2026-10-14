/*
File: src/shell/studio/open.rs

Purpose:
The studio's open worker: the named background thread that turns a `StartupTarget` into a
`StudioSession` plus a loaded `ProjectData`, and the channel protocol between it and the
loading screen (progress, the large-set confirmation, cancel, the final outcome).

Key structures:
- `OpenedTarget`: what `open_target` produced (session, or `None` when the scratch reservation
  failed; load result). The ONE place the session kind is decided: the session and the loaded
  `ProjectData` always agree on it (`StudioSession::is_image_set` == `is_single_image`).
- `OpenOutcome`: the worker's delivery: the same session plus, on success, a `LoadedStudio` —
  the project AND the `StudioBootstrap` (every disk read `StudioApp::new` needs), read right after
  the load so the GUI thread constructs the studio without I/O.
- `OpenEvent`: the worker's event stream (`Progress`*, at most one `NeedsConfirmation`, then
  exactly one `Finished`).
- `OpenWorker` / `WorkerLink`: the GUI end (events, confirmation answer, cancel flag) and the
  worker end of one open.

Key functions:
- `spawn_open_thread(target, scratch_base, settings)`: the named open worker.
- `open_target`: the worker body (scratch reservation, settings re-read, project or image-set
  open); `open_images`, `load_project_dir` (context over `chapter_load::load_chapter`).
- `OpenedTarget::into_outcome`: completes a successful open with its `StudioBootstrap`
  (`StudioBootstrap::build_blocking` on the worker; tests of failing opens pass a builder that
  is never called).

Notes:
Only WHERE the load runs is decided here, not the load itself: whether a project resumes its
unsaved session is decided by `super::super::chapter_load` (the one owner; the launcher passes
no such choice). A load failure carries the
greppable wording "failed to load project at …". The worker performs non-atomic chapter writes
(`cleaned/` placeholders, JPEG->PNG re-encode + source removal), which is why the shell defers a
close until it delivered; an image-set open writes only to its throwaway scratch and honours the
cancel flag. Native-only (gated with the whole shell host off wasm).
*/

use super::super::target::{ImageSources, StartupTarget, release_inbox_batches_blocking};
use super::session::StudioSession;
use crate::studio::StudioBootstrap;
use crate::project::single_image::{ImageSetEstimate, SingleImageError, SingleImageScratch};
use crate::project::ProjectData;
use crate::runtime_log;
use anyhow::Context;
use ms_thread as thread;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

/// What [`open_target`] produced: the session it created (`None` when the scratch reservation
/// failed) and the load result. Invariant: with an `Ok` result the session is present and
/// `session.is_image_set() == project.is_single_image()`.
pub(super) struct OpenedTarget {
    pub session: Option<StudioSession>,
    pub result: anyhow::Result<ProjectData>,
}

impl OpenedTarget {
    /// The worker's delivery: a successful load is completed with `build(&project)` — in
    /// production `StudioBootstrap::build_blocking`, blocking, so this runs on the worker; a
    /// failed one is passed through and `build` is not called.
    pub(super) fn into_outcome(self, build: impl FnOnce(&ProjectData) -> StudioBootstrap) -> OpenOutcome {
        let result = self.result.map(|project| {
            let bootstrap = build(&project);
            LoadedStudio { project, bootstrap }
        });
        OpenOutcome { session: self.session, result }
    }
}

/// A successful open: the loaded project and the inputs `StudioApp::new` pre-reads for it.
pub(super) struct LoadedStudio {
    pub project: ProjectData,
    pub bootstrap: StudioBootstrap,
}

/// What the open worker delivers at its end: the session it created (`None` when the scratch
/// reservation failed) and the load result with, on success, the studio's pre-read inputs. The
/// invariant of [`OpenedTarget`] holds for `LoadedStudio::project`.
pub(super) struct OpenOutcome {
    pub session: Option<StudioSession>,
    pub result: anyhow::Result<LoadedStudio>,
}

/// One message of the open worker, in order: any number of `Progress` and at most one
/// `NeedsConfirmation`, then exactly one `Finished` (unless the worker died).
pub(super) enum OpenEvent {
    /// `done` of `total` image files processed (skipped ones included). Image sets only.
    Progress { done: usize, total: usize },
    /// The set is large (`ms_project::single_image::needs_size_confirmation`): the worker waits
    /// for an answer on `OpenWorker::answer_confirmation`.
    NeedsConfirmation(ImageSetEstimate),
    /// The worker's result; nothing follows. Boxed: the project snapshot is large and only
    /// moves through.
    Finished(Box<OpenOutcome>),
}

/// The GUI end of a running open worker.
pub(super) struct OpenWorker {
    /// The worker's events, drained by `StudioScreen::poll_load`.
    pub(super) events: Receiver<OpenEvent>,
    /// The large-set answer (`true` = open). Dropping it while the worker waits is a cancel.
    confirm: Sender<bool>,
    /// Shared cancel flag, checked by `open_image_set` before every file. Project loads ignore it.
    pub(super) cancel: Arc<AtomicBool>,
}

impl OpenWorker {
    /// Answers a pending large-set confirmation. A worker that is already gone is logged.
    pub(super) fn answer_confirmation(&self, open: bool) {
        if self.confirm.send(open).is_err() {
            runtime_log::log_warn(format!("[studio] the open worker ended before the large-set answer (open={open}) arrived"));
        }
    }

    /// Raises the cancel flag: an image-set open stops before its next file.
    pub(super) fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The worker end of [`OpenWorker`].
pub(super) struct WorkerLink {
    /// Where the worker sends its events; the final `Finished` goes out through it directly.
    pub(super) events: Sender<OpenEvent>,
    confirm: Receiver<bool>,
    cancel: Arc<AtomicBool>,
}

impl WorkerLink {
    /// Sends a progress or confirmation event. When the screen is gone nobody watches the open
    /// any more, so the cancel flag is raised to stop it early.
    fn send(&self, event: OpenEvent) -> bool {
        if self.events.send(event).is_err() {
            self.cancel.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Asks the screen to confirm a large set and blocks until it answers. A dropped screen (the
    /// sender side) counts as a cancel.
    fn confirm_large_set(&self, estimate: ImageSetEstimate) -> bool {
        if !self.send(OpenEvent::NeedsConfirmation(estimate)) {
            return false;
        }
        self.confirm.recv().unwrap_or_else(|_disconnected| {
            runtime_log::log_info("[studio] the loading screen was left during the large-set confirmation; treated as cancel");
            false
        })
    }
}

/// A connected worker/screen channel pair.
pub(super) fn open_channels() -> (OpenWorker, WorkerLink) {
    let (events_tx, events_rx) = mpsc::channel();
    let (confirm_tx, confirm_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    (OpenWorker { events: events_rx, confirm: confirm_tx, cancel: Arc::clone(&cancel) }, WorkerLink { events: events_tx, confirm: confirm_rx, cancel })
}

/// Spawns the named background worker that opens `target` for the studio screen.
///
/// - `Project`: `chapter_load::load_chapter`, the one owner of the resume-unsaved decision (a
///   `{chapter}_unsaved` folder next to the chapter -> `load_resume_unsaved`, else `load`).
/// - `Images`: reserve a scratch session under `scratch_base` (a failure is the B5 error
///   screen), then — for a set of two or more files — estimate its decoded size from the file
///   headers and, above the soft limits, ask the screen and wait for the answer; then
///   `open_image_set` into the scratch with progress events and the shared cancel flag (no
///   unsaved detection: a scratch is always fresh). A one-file set is opened exactly as the
///   classic single-image mode: no estimate, no question. Errors are logged in technical form
///   here and reported as `SingleImageError::user_message`, the one owner of decode/open text.
///
/// The user settings are re-read on the worker so a reload observes settings changed during the
/// session; an unreadable config falls back to `fallback_user_settings` (logged). A successful
/// load is then completed with its `StudioBootstrap` on the same worker. The events (or
/// the spawn failure, as a disconnected channel) are observed by `StudioScreen::poll_load`.
pub(super) fn spawn_open_thread(target: StartupTarget, scratch_base: PathBuf, fallback_user_settings: serde_json::Value) -> OpenWorker {
    let (worker, link) = open_channels();
    let spawn_result = thread::Builder::new().name("studio-project-load".to_string()).spawn(move || {
        let opened = open_target(target, &scratch_base, fallback_user_settings, &link, crate::project::single_image::estimate_image_set);
        // The studio's own reads (user config, settings seeds, staging probe) happen here too,
        // so `StudioApp::new` on the GUI thread performs none.
        let outcome = opened.into_outcome(StudioBootstrap::build_blocking);
        if let Err(mpsc::SendError(event)) = link.events.send(OpenEvent::Finished(Box::new(outcome))) {
            runtime_log::log_info("[studio] the studio screen was left before the load finished; result dropped");
            // Nobody will own the session: release it here (still on the worker).
            if let OpenEvent::Finished(outcome) = event
                && let Some(session) = outcome.session
            {
                session.release();
            }
        }
    });
    if let Err(err) = spawn_result {
        // The link (and its sender) is dropped here, so the screen observes `Disconnected` and
        // shows the error screen instead of spinning forever.
        runtime_log::log_error(format!("[studio] failed to spawn studio-project-load thread: {err}"));
    }
    worker
}

/// The open worker's body (blocking; see [`spawn_open_thread`]). `estimate` is
/// `ms_project::single_image::estimate_image_set` in production; tests pass a fake.
pub(super) fn open_target(target: StartupTarget, scratch_base: &Path, fallback_user_settings: serde_json::Value, link: &WorkerLink, estimate: fn(&[PathBuf]) -> ImageSetEstimate) -> OpenedTarget {
    let inbox_batches = match &target {
        StartupTarget::Project(_) => Vec::new(),
        StartupTarget::Images(sources) => sources.inbox_batches().to_vec(),
    };
    let session = match StudioSession::from_target(target, scratch_base) {
        Ok(session) => session,
        Err(err) => {
            runtime_log::log_error(format!("[studio] could not reserve a single-image scratch session: {err}"));
            // No session will own the delivered files: release their inbox batches now.
            release_inbox_batches_blocking(&inbox_batches);
            return OpenedTarget { session: None, result: Err(anyhow::Error::msg(err.user_message())) };
        }
    };
    let user_settings = match crate::config::load_user_settings_for_startup() {
        Ok(settings) => settings,
        Err(err) => {
            runtime_log::log_error(format!(
                "[studio] failed to refresh user_config.json before project load; using last known settings; error={err}"
            ));
            fallback_user_settings
        }
    };
    let result = match &session {
        StudioSession::Project { dir } => load_project_dir(dir, &user_settings),
        StudioSession::ImageSet { sources, scratch } => open_images(sources, scratch, &user_settings, link, estimate),
    };
    OpenedTarget { session: Some(session), result }
}

/// Opens an image set into its reserved scratch (the `Images` arm of [`open_target`]): the
/// large-set question for two or more files, then `open_image_set` with progress and cancel.
fn open_images(sources: &ImageSources, scratch: &SingleImageScratch, user_settings: &serde_json::Value, link: &WorkerLink, estimate: fn(&[PathBuf]) -> ImageSetEstimate) -> anyhow::Result<ProjectData> {
    use crate::project::single_image::{needs_size_confirmation, open_image_set};
    let files = sources.files();
    if files.len() > 1 {
        let set_estimate = estimate(files);
        if needs_size_confirmation(&set_estimate) {
            runtime_log::log_info(format!(
                "[studio] large image set: {} file(s), about {} decoded bytes; asking before the open",
                set_estimate.count, set_estimate.decoded_bytes
            ));
            if !link.confirm_large_set(set_estimate) {
                runtime_log::log_info("[studio] the large image set was not confirmed; open cancelled");
                return Err(anyhow::Error::msg(SingleImageError::Cancelled.user_message()));
            }
        }
    }
    let mut progress = |done: usize, total: usize| {
        // A gone screen raises the cancel flag inside `send`; the open stops before its next file.
        link.send(OpenEvent::Progress { done, total });
    };
    open_image_set(files, sources.skipped().to_vec(), scratch, user_settings, &mut progress, &link.cancel).map_err(|err| {
        runtime_log::log_error(format!("[studio] failed to open the image set: {err}"));
        anyhow::Error::msg(err.user_message())
    })
}

/// Loads a chapter directory through `chapter_load::load_chapter` (which decides resume vs
/// fresh) and adds the path context. Blocking; open worker only.
fn load_project_dir(project_dir: &Path, user_settings: &serde_json::Value) -> anyhow::Result<ProjectData> {
    super::super::chapter_load::load_chapter(project_dir, user_settings)
    // Same greppable wording as the old startup-path context.
    .with_context(|| format!("failed to load project at {}", project_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{images, never_estimated, temp_base};
    use super::*;

    /// The open worker's body reports a scratch reservation failure (B5) as a failed load
    /// without a session, so the studio error screen shows it.
    #[test]
    fn scratch_reservation_failure_is_a_failed_load_without_a_session() {
        let base = temp_base("unreservable");
        // A FILE where the scratch base directory must be: the reservation cannot create it.
        std::fs::write(&base, b"not a directory").expect("write blocker file");
        let (_worker, link) = open_channels();
        let outcome = open_target(images(&["/home/u/a.png"]), &base, serde_json::Value::Null, &link, never_estimated);
        assert!(outcome.session.is_none());
        assert!(outcome.result.is_err());
        std::fs::remove_file(&base).expect("remove blocker file");
    }

    /// Writes a small PNG at `path` (a real, decodable page or source image).
    fn write_png(path: &Path) {
        image::RgbImage::from_pixel(4, 3, image::Rgb([200, 100, 50])).save(path).expect("write test png");
    }

    /// Runs the open worker's body over `target` (scratch base under `dir`) and checks the one
    /// invariant of the session kind: the shell half and the data half agree.
    fn open_and_check_kind(target: StartupTarget, dir: &Path, expect_image_set: bool) {
        let (_worker, link) = open_channels();
        let outcome = open_target(target, &dir.join("scratch"), serde_json::Value::Null, &link, never_estimated);
        let project = outcome.result.expect("the fixture opens");
        let session = outcome.session.expect("a successful open carries its session");
        assert_eq!(session.is_image_set(), expect_image_set);
        assert_eq!(session.is_image_set(), project.is_single_image(), "the session and the project data must agree on the kind");
        drop(project);
        session.release();
    }

    #[test]
    fn a_project_directory_opens_as_a_project_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let chapter = dir.path().join("Title").join("Chapter 1");
        std::fs::create_dir_all(chapter.join("src")).expect("chapter dir");
        write_png(&chapter.join("src").join("001.png"));
        open_and_check_kind(StartupTarget::Project(chapter), dir.path(), false);
    }

    #[test]
    fn an_image_set_opens_as_an_image_set_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("page one.png");
        write_png(&source);
        let target = StartupTarget::Images(ImageSources::new(vec![source], Vec::new(), Vec::new()).expect("non-empty set"));
        open_and_check_kind(target, dir.path(), true);
    }
}
