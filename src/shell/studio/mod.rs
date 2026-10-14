/*
File: src/shell/studio/mod.rs

Purpose:
The studio screen of the main window. The project load (unsaved-session detection +
`ProjectData::load*`, or the image-set open into a reserved scratch chapter) runs on a
background thread behind a loading screen, followed on the same thread by the reads the studio
itself needs (`StudioBootstrap`); a failure shows an error screen; once the project arrives
`StudioApp` is constructed in place (no I/O) and every frame is delegated to it. The screen never
closes, styles or titles the window: it reports how it wants to be left (`StudioExit`) and the
shell (`../mod.rs`) decides through the pure table in `screen.rs`.

Submodules:
- `session.rs`: `StudioSession` (the shell half of the session and the one shell-side owner of
  its kind; `release` at teardown).
- `open.rs`: the open worker (`spawn_open_thread`, `OpenWorker`, `OpenEvent`, `OpenOutcome`);
  a successful outcome carries the project AND its `StudioBootstrap` (`LoadedStudio`), so
  `StudioApp::new` reads nothing on the GUI thread.
- `ui.rs`: loading and error screen drawing.

Key structures:
- `StudioScreen`: phases `Loading` (polls the worker, shows progress and the large-set
  confirmation), `Loaded` / `LoadFailed` (the worker's delivery, held until the shell's effect
  consumes it: `start_running`, `fail`, or `into_parts` on a close during the load),
  `Failed` (error screen with back-to-launcher / exit buttons), `Cancelled` (the
  user cancelled the open: leaves for the launcher at once), `Running` (delegates `ui` to
  `StudioApp`, forwards the shell's inbound calls `request_os_close` /
  `request_close_for_incoming`, and hands the shell the studio's outbound queue,
  `take_outputs` -> `crate::studio::StudioOutput`; `reload_exit` turns a reload request into
  `StudioExit::Reload` after the session check).
- `LoadResult`: what a settled load reports to the shell.
- `StudioExit`: the screen's exits (reload, the two error-screen buttons). The running studio's
  close and reload arrive as `StudioOutput`s: the shell dispatches a close directly and asks
  `reload_exit` for the reload.

Notes:
A load failure is logged and rendered as the error screen. A close that arrives while the worker
runs is deferred by the shell (the worker may be mid-write, see `open.rs`); an image-set open is
cancelled instead (its writes go to the throwaway scratch only), including a pending large-set
confirmation, so the deferred close never waits on a question nobody will answer. A user cancel
of the loading screen ends as a cancelled load: the table routes it to the Failed phase, whose
screen here immediately exits with `FailedBack` (the existing route to the launcher and the
scratch release). A scratch reservation failure (B5) reaches the error screen. Before the worker
delivers, the session kind is the target's (`Loading::image_set`); afterwards it is the
session's (`StudioSession::is_image_set` / `reloadable`): a session that is not reloadable (an
image set) refuses the structural-operation reload with a logged error. A damaged `{chapter}_unsaved` session makes
`load_resume_unsaved` fail with a localized error naming the files; "Exit to launcher" is the
route to discarding it there. `StudioApp::new` needs no `eframe::CreationContext`, which is what
makes construction inside a frame possible. Native-only (gated with the whole shell host off
wasm).
*/

mod open;
mod session;
mod ui;

pub(crate) use session::StudioSession;

use super::target::StartupTarget;
use crate::ai_backend_supervisor::AiBackendHandle;
use crate::studio::{StudioApp, StudioOutput};
use crate::project::single_image::{ImageSetEstimate, SingleImageError};
use crate::runtime_log;
use crate::tabs::AppTab;
// `StudioApp` draws through its `eframe::App::ui`; the shell is the real eframe app.
use eframe::App as _;
use open::{LoadedStudio, OpenEvent, OpenOutcome, OpenWorker, spawn_open_thread};
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use ui::{LoadingAction, LoadingView, draw_error_screen, draw_loading_screen};

/// What the open worker delivered, as seen by the shell. The data stays in the screen (the
/// `Loaded` / `LoadFailed` phase) until the effect the table picks consumes it: `start_running`,
/// `fail`, or `into_parts` (a close arrived during the load: the project is discarded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadResult {
    /// The project arrived (phase `Loaded`).
    Loaded,
    /// The load failed, already logged (phase `LoadFailed`, holding the error-screen text).
    Failed,
}

/// How the studio screen wants to be left; the shell maps it to a `ShellEvent`. A close the
/// running studio decided is not an exit here: it arrives as `StudioOutput::Close` and the shell
/// dispatches it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StudioExit {
    /// `StudioApp` asked to rebuild itself from disk after a structural page operation.
    Reload { project_dir: PathBuf },
    /// Error screen: "Exit to launcher".
    FailedBack,
    /// Error screen: "Exit".
    FailedExit,
}

/// The loading phase: the worker's GUI end plus what it reported so far.
struct Loading {
    worker: OpenWorker,
    /// The TARGET is an image set. The session (the owner of the kind) exists only once the
    /// worker delivered; until then this is what the loading screen and a deferred close ask.
    image_set: bool,
    /// Last `(done, total)` progress of an image-set open.
    progress: Option<(usize, usize)>,
    /// A large-set question waiting for the user's answer.
    confirmation: Option<ImageSetEstimate>,
}

/// The studio screen's phase data (the matching pure phase is `screen::StudioPhase`).
/// `Loaded` and `LoadFailed` are settled loads: `poll_load` enters them and the shell leaves them
/// in the same call (its effect consumes the data), so they map to the pure `Loading` phase and
/// are never drawn in practice.
enum Phase {
    /// Waiting on the open worker.
    Loading(Loading),
    /// The project arrived with its pre-read studio inputs; `start_running` turns it into
    /// `Running`, `into_parts` discards it.
    Loaded(Box<LoadedStudio>),
    /// The load failed with this error-screen text; `fail` turns it into `Failed` / `Cancelled`.
    LoadFailed(String),
    /// The load failed; the error screen shows `error_text`.
    Failed { error_text: String },
    /// The user cancelled the open; the screen exits with `FailedBack` (pure phase: `Failed`).
    Cancelled,
    /// The project arrived; every frame is delegated to the real app.
    Running(Box<StudioApp>),
}

/// The studio screen: owns the open worker's GUI end, the running `StudioApp` and the
/// [`StudioSession`] until the shell moves them onto the teardown worker ([`Self::into_parts`]).
pub(crate) struct StudioScreen {
    phase: Phase,
    /// The session, delivered by the open worker; released by the teardown.
    session: Option<StudioSession>,
    /// The user pressed Cancel on the loading screen: whatever the worker delivers ends as a
    /// cancelled load, and the failure leaves for the launcher at once.
    user_cancelled: bool,
    /// Tab restored after a structural-operation reload; selection intentionally does not persist.
    reload_tab: Option<AppTab>,
    ai_backend: AiBackendHandle,
}

impl StudioScreen {
    /// Starts the open worker for `target` and shows the loading screen. `reload_tab` is the tab a
    /// reload returns to. Spawns a thread only; no I/O on the caller.
    pub(crate) fn start(target: StartupTarget, reload_tab: Option<AppTab>, fallback_user_settings: serde_json::Value, ai_backend: AiBackendHandle) -> Self {
        let image_set = target.is_image_set();
        StudioSession::log_open(&target);
        let worker = spawn_open_thread(target, crate::config::single_image::scratch_base(), fallback_user_settings);
        Self::with_worker(worker, image_set, reload_tab, ai_backend)
    }

    /// The loading screen over an already running `worker` opening a target of the given kind.
    fn with_worker(worker: OpenWorker, image_set: bool, reload_tab: Option<AppTab>, ai_backend: AiBackendHandle) -> Self {
        let loading = Loading { worker, image_set, progress: None, confirmation: None };
        Self { phase: Phase::Loading(loading), session: None, user_cancelled: false, reload_tab, ai_backend }
    }

    /// Drains the open worker's events (non-blocking; `None` while it runs or outside `Loading`):
    /// progress and the large-set question are kept for the loading screen; the final outcome
    /// settles the phase (`Loaded` with the project, `LoadFailed` with the error text), is
    /// reported by kind, and the session it carries is kept by the screen. A worker that died
    /// without an outcome is a failure. After a user cancel even a successful open is reported as
    /// a cancelled load (the session is still released by the teardown).
    pub(crate) fn poll_load(&mut self) -> Option<LoadResult> {
        let Phase::Loading(loading) = &mut self.phase else {
            return None;
        };
        loop {
            match loading.worker.events.try_recv() {
                Ok(OpenEvent::Progress { done, total }) => loading.progress = Some((done, total)),
                Ok(OpenEvent::NeedsConfirmation(estimate)) => loading.confirmation = Some(estimate),
                Ok(OpenEvent::Finished(outcome)) => {
                    let OpenOutcome { session, result } = *outcome;
                    self.session = session;
                    let (phase, settled) = match result {
                        Ok(_project) if self.user_cancelled => {
                            runtime_log::log_info("[studio] the image set opened after the user cancelled; discarded");
                            (Phase::LoadFailed(SingleImageError::Cancelled.user_message()), LoadResult::Failed)
                        }
                        Ok(loaded) => (Phase::Loaded(Box::new(loaded)), LoadResult::Loaded),
                        Err(err) => {
                            // `{err:#}` prints the whole anyhow chain, including the greppable
                            // "failed to load project at …" context added by the worker.
                            let error_text = format!("{err:#}");
                            runtime_log::log_error(format!("[studio] {error_text}"));
                            (Phase::LoadFailed(error_text), LoadResult::Failed)
                        }
                    };
                    self.phase = phase;
                    return Some(settled);
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    // Worker panicked before sending, or the spawn itself failed (logged in
                    // `spawn_open_thread`). A dead worker holds no file handles.
                    runtime_log::log_error("[studio] project load thread exited without a result");
                    self.phase = Phase::LoadFailed(t!("studio_bootstrap.load_thread_exited_error").to_string());
                    return Some(LoadResult::Failed);
                }
            }
        }
    }

    /// Answers the pending large-set question with "Open"; a no-op without one.
    fn confirm_open(&mut self) {
        if let Phase::Loading(loading) = &mut self.phase
            && loading.confirmation.take().is_some()
        {
            runtime_log::log_info("[studio] the user confirmed the large image set");
            loading.worker.answer_confirmation(true);
        }
    }

    /// The user's Cancel on the loading screen: stops the open (answers a pending question with
    /// "no", raises the cancel flag) and marks the load as user-cancelled, so its failure leaves
    /// for the launcher (`StudioExit::FailedBack`).
    fn cancel_open(&mut self) {
        if let Phase::Loading(loading) = &mut self.phase {
            runtime_log::log_info("[studio] the user cancelled the open");
            self.user_cancelled = true;
            stop_worker(loading);
        }
    }

    /// The shell deferred an OS close until the worker delivers: an image-set open is stopped
    /// (its pending question answered "no") so the close never waits on the user. Not a user
    /// cancel: the cancelled load then ends the program through the table's close-after-load row.
    /// Idempotent; a project load is left to finish (it may be mid-write). Called from the shell's
    /// `logic` (so a minimized window stops the open too) and from `ui`.
    pub(crate) fn cancel_for_close(&mut self) {
        if let Phase::Loading(loading) = &mut self.phase
            && loading.image_set
        {
            stop_worker(loading);
        }
    }

    /// Constructs `StudioApp` from the loaded project and its `StudioBootstrap` (phase `Loaded`)
    /// and starts running it (on the GUI thread: every read was done by the open worker, the app
    /// constructor only wires state and spawns workers). Starts the deferred
    /// storage-mode reconciliation of a direct `--project` start, now that the project is loaded.
    /// The table emits `StartStudio` only on the `StudioLoaded` that `poll_load` reported after
    /// entering `Loaded`; any other phase is a shell defect, logged and left unchanged.
    pub(crate) fn start_running(&mut self) {
        let loaded = match std::mem::replace(&mut self.phase, Phase::Cancelled) {
            Phase::Loaded(loaded) => loaded,
            other @ (Phase::Loading(_) | Phase::LoadFailed(_) | Phase::Failed { .. } | Phase::Cancelled | Phase::Running(_)) => {
                self.phase = other;
                runtime_log::log_error("[studio] defect: the studio was started without its loaded project");
                return;
            }
        };
        let LoadedStudio { project, bootstrap } = *loaded;
        let mut app = StudioApp::new(project, bootstrap, self.ai_backend.clone());
        if let Some(tab) = self.reload_tab.take() {
            app.set_active_tab(tab);
        }
        // No-op when nothing is pending or it already ran; progress and failures show in the
        // General settings pane and the log, and an incomplete run raises the studio's top-bar
        // notice (`StudioApp::draw_storage_reconcile_notice`).
        if let Some(job) = crate::storage_mode_job::start_pending_reconciliation() {
            runtime_log::log_info(format!("[studio] started the deferred storage-mode reconciliation (job {job})"));
        }
        self.phase = Phase::Running(Box::new(app));
    }

    /// Shows the error screen with the failed load's text (phase `LoadFailed`), or — after a user
    /// cancel — leaves for the launcher on the next frame. The table emits `ShowLoadError` only on
    /// the `StudioLoadFailed` that `poll_load` reported after entering `LoadFailed`; any other
    /// phase is a shell defect, logged and left unchanged.
    pub(crate) fn fail(&mut self) {
        let error_text = match std::mem::replace(&mut self.phase, Phase::Cancelled) {
            Phase::LoadFailed(error_text) => error_text,
            other @ (Phase::Loading(_) | Phase::Loaded(_) | Phase::Failed { .. } | Phase::Cancelled | Phase::Running(_)) => {
                self.phase = other;
                runtime_log::log_error("[studio] defect: the studio error screen was entered without its error");
                return;
            }
        };
        self.phase = if self.user_cancelled { Phase::Cancelled } else { Phase::Failed { error_text } };
    }

    /// Hands the OS close button to the running studio (`StudioApp::request_os_close`); a no-op in
    /// the other phases.
    pub(crate) fn request_os_close(&mut self) {
        if let Phase::Running(app) = &mut self.phase {
            app.request_os_close();
        }
    }

    /// Asks the running studio whether it may close for `count` externally delivered images
    /// (`StudioApp::request_close_for_incoming`). `false` outside `Running`, or when the studio is
    /// already closing for another reason: the caller then drops the images.
    pub(crate) fn request_close_for_incoming(&mut self, count: usize) -> bool {
        match &mut self.phase {
            Phase::Running(app) => app.request_close_for_incoming(count),
            Phase::Loading(_) | Phase::Loaded(_) | Phase::LoadFailed(_) | Phase::Failed { .. } | Phase::Cancelled => false,
        }
    }

    /// Drains what the running studio asks of the window since the last call
    /// (`StudioApp::take_outputs`, in its documented order); always empty outside `Running`.
    pub(crate) fn take_outputs(&mut self) -> Vec<StudioOutput> {
        match &mut self.phase {
            Phase::Running(app) => app.take_outputs(),
            Phase::Loading(_) | Phase::Loaded(_) | Phase::LoadFailed(_) | Phase::Failed { .. } | Phase::Cancelled => Vec::new(),
        }
    }

    /// The exit for a `StudioOutput::ReloadRequested`: `Reload` of the running project, or `None`
    /// (logged) when this screen cannot reload — an image-set session (not reloadable by design,
    /// see `StudioSession::reloadable`), a running studio without its session (a defect), or no
    /// running studio at all.
    pub(crate) fn reload_exit(&self) -> Option<StudioExit> {
        let Phase::Running(app) = &self.phase else {
            runtime_log::log_error("[studio] project reload requested while no studio is running; ignored");
            return None;
        };
        match &self.session {
            Some(session) if session.reloadable() => Some(StudioExit::Reload { project_dir: app.project_dir() }),
            // Unreachable by design (see `StudioSession::reloadable`); the session keeps running.
            Some(_) => {
                runtime_log::log_error("[studio] project reload requested in an image-set session; refused, the session keeps running");
                None
            }
            // A running studio always holds its session (an `Ok` open carries one).
            None => {
                runtime_log::log_error("[studio] defect: project reload requested by a studio without its session; refused, the session keeps running");
                None
            }
        }
    }

    /// The exit a phase produces without any input: `FailedBack` for a cancelled open.
    fn automatic_exit(&self) -> Option<StudioExit> {
        match self.phase {
            Phase::Cancelled => Some(StudioExit::FailedBack),
            Phase::Loading(_) | Phase::Loaded(_) | Phase::LoadFailed(_) | Phase::Failed { .. } | Phase::Running(_) => None,
        }
    }

    /// Draws the active phase and returns the exit it produced this frame. A running studio
    /// produces none here: its close / reload arrive through `take_outputs`, which the shell
    /// drains right after this call. `closing_after_load`
    /// labels the loading screen as "finishing before close" (the shell deferred an OS close) and
    /// stops an image-set open.
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, closing_after_load: bool) -> Option<StudioExit> {
        if closing_after_load {
            self.cancel_for_close();
        }
        let automatic_exit = self.automatic_exit();
        // Kind of a settled load: the delivered session's (the target was the same kind).
        let session_image_set = self.session.as_ref().is_some_and(StudioSession::is_image_set);
        let mut loading_action = None;
        let exit = match &mut self.phase {
            Phase::Loading(loading) => {
                let view = LoadingView {
                    closing: closing_after_load,
                    image_set: loading.image_set,
                    cancel_requested: self.user_cancelled,
                    progress: loading.progress,
                    confirmation: loading.confirmation,
                };
                loading_action = draw_loading_screen(ui, view);
                None
            }
            // Settled loads: the shell leaves them in the poll that settled them, before any draw.
            Phase::Loaded(_) | Phase::LoadFailed(_) => {
                draw_loading_screen(ui, LoadingView { closing: closing_after_load, image_set: session_image_set, cancel_requested: self.user_cancelled, progress: None, confirmation: None });
                None
            }
            Phase::Failed { error_text } => draw_error_screen(ui, error_text),
            Phase::Cancelled => {
                // One frame at most: the shell leaves this screen on the exit returned below.
                draw_loading_screen(ui, LoadingView { closing: false, image_set: false, cancel_requested: true, progress: None, confirmation: None });
                automatic_exit
            }
            Phase::Running(app) => {
                app.ui(ui, frame);
                None
            }
        };
        match loading_action {
            Some(LoadingAction::OpenLargeSet) => self.confirm_open(),
            Some(LoadingAction::Cancel) => self.cancel_open(),
            None => {}
        }
        exit
    }

    /// Splits the screen into what the teardown worker needs: the running app (if any) and the
    /// session (if the worker delivered one). A project still loading is never split:
    /// the shell defers until the worker delivered. A project that loaded after the window was
    /// asked to close (phase `Loaded`) is dropped here, on the caller's thread: constructing
    /// `StudioApp` would spawn loaders for nothing.
    pub(crate) fn into_parts(self) -> (Option<Box<StudioApp>>, Option<StudioSession>) {
        let app = match self.phase {
            Phase::Running(app) => Some(app),
            Phase::Loaded(loaded) => {
                drop(loaded);
                None
            }
            Phase::Loading(_) | Phase::LoadFailed(_) | Phase::Failed { .. } | Phase::Cancelled => None,
        };
        (app, self.session)
    }
}

/// Stops a loading image-set open: answers a pending large-set question with "no" and raises
/// the cancel flag (the open stops before its next file). Idempotent.
fn stop_worker(loading: &mut Loading) {
    if loading.confirmation.take().is_some() {
        loading.worker.answer_confirmation(false);
    }
    loading.worker.request_cancel();
}

/// Helpers shared by the studio submodules' tests.
#[cfg(test)]
mod test_support {
    use super::super::target::{ImageSources, StartupTarget};
    use crate::project::single_image::ImageSetEstimate;
    use std::path::PathBuf;

    /// Unique scratch base under the OS temp dir; each test removes it.
    pub(super) fn temp_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ms-studio-open-request-{tag}-{}", std::process::id()))
    }

    /// An image-set target over `files` (no skips, no inbox).
    pub(super) fn images(files: &[&str]) -> StartupTarget {
        StartupTarget::Images(ImageSources::new(files.iter().map(PathBuf::from).collect(), Vec::new(), Vec::new()).expect("non-empty set"))
    }

    /// An estimate a one-file set must never ask for.
    pub(super) fn never_estimated(_: &[PathBuf]) -> ImageSetEstimate {
        panic!("a one-file set must open without a size estimate")
    }
}

#[cfg(test)]
mod tests {
    use super::open::{open_channels, open_target};
    use super::test_support::{images, temp_base};
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    /// A fake estimate over the confirmation limit (no file is read).
    fn large_estimate(sources: &[PathBuf]) -> ImageSetEstimate {
        ImageSetEstimate { count: sources.len(), decoded_bytes: 3 << 30 }
    }

    /// Runs the open worker's body on a thread over a fake large estimate and returns the
    /// studio screen watching it plus the thread handle.
    fn large_set_screen(base: &Path) -> (StudioScreen, std::thread::JoinHandle<()>) {
        let (worker, link) = open_channels();
        let target = images(&["/home/u/missing-1.png", "/home/u/missing-2.png"]);
        let scratch_base = base.to_path_buf();
        let handle = std::thread::spawn(move || {
            // Both files are missing or the set is cancelled: the open fails, so no bootstrap is read.
            let outcome = open_target(target, &scratch_base, serde_json::Value::Null, &link, large_estimate).into_outcome(|_| panic!("a failed open must not read a studio bootstrap"));
            assert!(link.events.send(OpenEvent::Finished(Box::new(outcome))).is_ok(), "the test screen is alive");
        });
        (StudioScreen::with_worker(worker, true, None, AiBackendHandle::disabled()), handle)
    }

    /// Polls the screen until the worker delivers its outcome (bounded).
    fn wait_for_load(screen: &mut StudioScreen) -> LoadResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(result) = screen.poll_load() {
                return result;
            }
            assert!(std::time::Instant::now() < deadline, "the open worker never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Polls the screen until the worker asks the large-set question (bounded).
    fn wait_for_question(screen: &mut StudioScreen) -> ImageSetEstimate {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            assert!(screen.poll_load().is_none(), "the worker must wait for the answer");
            if let Phase::Loading(Loading { confirmation: Some(estimate), .. }) = &screen.phase {
                return *estimate;
            }
            assert!(std::time::Instant::now() < deadline, "the open worker never asked");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_cancelled_large_set_leaves_for_the_launcher_and_releases_the_scratch() {
        let base = temp_base("large-cancel");
        let (mut screen, handle) = large_set_screen(&base);
        let estimate = wait_for_question(&mut screen);
        assert_eq!(estimate.count, 2);
        assert_eq!(screen.automatic_exit(), None, "a question is not an exit");
        screen.cancel_open();
        assert_eq!(wait_for_load(&mut screen), LoadResult::Failed, "a cancelled set must not load");
        handle.join().expect("worker thread");
        // The table routes the failed load to the Failed phase, which calls `fail`.
        screen.fail();
        assert_eq!(screen.automatic_exit(), Some(StudioExit::FailedBack));
        let (app, session) = screen.into_parts();
        assert!(app.is_none());
        let Some(session @ StudioSession::ImageSet { .. }) = session else {
            panic!("the reserved scratch must reach the teardown");
        };
        session.release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn a_confirmed_large_set_opens_and_reports_its_skips() {
        let base = temp_base("large-open");
        let (mut screen, handle) = large_set_screen(&base);
        wait_for_question(&mut screen);
        screen.confirm_open();
        // Both files are missing: the open runs and fails listing them (not a cancel).
        assert_eq!(wait_for_load(&mut screen), LoadResult::Failed, "a set of missing files cannot load");
        handle.join().expect("worker thread");
        assert!(matches!(&screen.phase, Phase::LoadFailed(_)), "the failure stays in the screen until the shell's effect");
        screen.fail();
        assert!(matches!(&screen.phase, Phase::Failed { .. }), "the error screen shows the stored failure");
        assert_eq!(screen.automatic_exit(), None, "a real failure shows the error screen");
        let (_, session) = screen.into_parts();
        session.expect("scratch reserved").release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn a_deferred_close_cancels_the_question_without_a_user_cancel() {
        let base = temp_base("large-close");
        let (mut screen, handle) = large_set_screen(&base);
        wait_for_question(&mut screen);
        screen.cancel_for_close();
        let Phase::Loading(loading) = &screen.phase else {
            panic!("still loading until the worker delivers");
        };
        assert!(loading.worker.cancel.load(Ordering::Relaxed), "the open is cancelled");
        assert_eq!(wait_for_load(&mut screen), LoadResult::Failed, "a set cancelled by the close must not load");
        handle.join().expect("worker thread");
        assert!(matches!(&screen.phase, Phase::LoadFailed(_)), "the failure waits for the shell's effect");
        assert!(!screen.user_cancelled, "the close is not a user cancel");
        // The table sends a close-after-load failure to Closing(Exit); `fail` is not called then.
        let (_, session) = screen.into_parts();
        session.expect("scratch reserved").release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    /// `fail` / `start_running` outside their settled phase are shell defects: logged, the
    /// phase is kept and no app is constructed.
    #[test]
    fn settling_effects_outside_their_phase_keep_the_phase() {
        let (worker, _link) = open_channels();
        let mut screen = StudioScreen::with_worker(worker, false, None, AiBackendHandle::disabled());
        screen.fail();
        assert!(matches!(&screen.phase, Phase::Loading(_)), "no failure was delivered");
        screen.start_running();
        assert!(matches!(&screen.phase, Phase::Loading(_)), "no project was delivered");
        let (app, session) = screen.into_parts();
        assert!(app.is_none() && session.is_none());
    }

    /// A worker that dies without an outcome settles as a failed load carrying the localized
    /// error, which `fail` turns into the error screen.
    #[test]
    fn a_lost_worker_settles_as_a_failed_load() {
        let (worker, link) = open_channels();
        let mut screen = StudioScreen::with_worker(worker, false, None, AiBackendHandle::disabled());
        assert_eq!(screen.poll_load(), None, "the worker is still alive");
        drop(link);
        assert_eq!(screen.poll_load(), Some(LoadResult::Failed));
        assert!(matches!(&screen.phase, Phase::LoadFailed(_)));
        assert_eq!(screen.poll_load(), None, "a settled load reports once");
        screen.fail();
        assert!(matches!(&screen.phase, Phase::Failed { .. }));
    }
}
