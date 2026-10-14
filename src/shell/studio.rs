/*
File: src/shell/studio.rs

Purpose:
The studio screen of the main window. The project load (unsaved-session detection +
`ProjectData::load*`, or the image-set open into a reserved scratch chapter) runs on a
background thread behind a loading screen; a failure shows an error screen; once the project
arrives `MangaApp` is constructed in place and every frame is delegated to it. The screen never
closes, styles or titles the window: it reports how it wants to be left (`StudioExit`) and the
shell (`mod.rs`) decides through the pure table in `screen.rs`.

Key structures:
- `StartupTarget`: what the studio opens (a chapter directory or an ordered image set), the
  output of startup routing (CLI) or of the launcher; also derives the window title.
- `ImageSources`: an ordered image set before it is opened: the files, the files a deliverer
  already rejected (`SkippedSource`), and the Android inbox batches backing the files.
- `StudioOpenRequest`: an opened target — a project, or an image set with its reserved
  `SingleImageScratch`. Created BY THE OPEN WORKER (the scratch reservation is I/O) and owned by
  the screen until the studio teardown releases it (`release`: deletes the scratch and releases
  the inbox batches; never on the GUI thread).
- `OpenEvent` / `OpenWorker`: the open worker's event stream (progress, the large-set
  confirmation request, the outcome) and the GUI end of it (confirmation answer, cancel flag).
- `StudioScreen`: phases `Loading` (polls the worker, shows progress and the large-set
  confirmation), `Failed` (error screen with back-to-launcher / exit buttons), `Cancelled` (the
  user cancelled the open: leaves for the launcher at once), `Running` (delegates `ui` to
  `MangaApp`).
- `StudioExit`: the screen's exits (studio close, reload, the two error-screen buttons).

Key functions:
- `spawn_open_thread(target, scratch_base, settings)`: the named open worker.
- `install_shader_layers(cc)`: installs the `egui-shader-layers` glow backend from the app
  creator; the shell's `on_exit` destroys it.

Notes:
Only WHERE the load runs is decided here, not the load itself: the worker performs the
detect-unsaved/load sequence the synchronous startup used to run. A load failure is logged with
the greppable wording "failed to load project at …" and rendered as the error screen. A close
that arrives while the worker runs is deferred by the shell (the worker performs non-atomic
chapter writes — `cleaned/` placeholders, JPEG->PNG re-encode + source removal — and killing it
mid-write can corrupt the chapter); an image-set open is cancelled instead (its writes go to the
throwaway scratch only), including a pending large-set confirmation, so the deferred close
never waits on a question nobody will answer. A user cancel of the loading screen ends as a
cancelled load: the table routes it to the Failed phase, whose screen here immediately exits
with `FailedBack` (the existing route to the launcher and the scratch release). A scratch
reservation failure (B5) reaches the error screen. An image-set session refuses the
structural-operation reload with a logged error (the Page Manager is hidden there, so the
request is unreachable). A damaged `{chapter}_unsaved` session makes `load_resume_unsaved` fail
with a localized error naming the files; "Exit to launcher" is the route to discarding it there.
`MangaApp::new` needs no `eframe::CreationContext`, which is what makes construction inside a
frame possible. Native-only (gated with the whole shell host off wasm).
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::app::{MangaApp, PendingCloseAction};
use crate::project::single_image::{ImageSetEstimate, SingleImageError, SingleImageScratch};
use crate::project::{ProjectData, SkippedSource};
use crate::runtime_log;
use crate::tabs::AppTab;
use anyhow::Context;
// `MangaApp` draws through its `eframe::App::ui`; the shell is the real eframe app.
use eframe::App as _;
use ms_thread as thread;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// An ordered set of picture files to open as ONE image-set session, before it is opened.
///
/// Invariant (enforced by [`ImageSources::new`]): `files` and `skipped` are not both empty.
/// `files` may be empty when every delivered document was already rejected (the open then fails
/// with the list of skipped files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSources {
    /// Readable paths in page order (as received; the launcher sorts its pick first).
    files: Vec<PathBuf>,
    /// Documents the deliverer could not turn into a file (e.g. a failed Android copy), listed
    /// before the files the open itself skips.
    skipped: Vec<SkippedSource>,
    /// Android inbox batch folders holding `files`; released with the session
    /// (`StudioOpenRequest::release`). Empty for desktop paths.
    inbox_batches: Vec<PathBuf>,
}

impl ImageSources {
    /// A set over `files` (page order), the already-rejected `skipped` documents and the inbox
    /// batch folders backing the files. `None` when `files` and `skipped` are both empty: there
    /// is nothing to open or to report.
    #[must_use]
    pub fn new(files: Vec<PathBuf>, skipped: Vec<SkippedSource>, inbox_batches: Vec<PathBuf>) -> Option<Self> {
        if files.is_empty() && skipped.is_empty() {
            return None;
        }
        Some(Self { files, skipped, inbox_batches })
    }

    /// The paths to open, in page order.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The documents rejected before the open, in delivery order.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedSource] {
        &self.skipped
    }

    /// The inbox batch folders the files live in (Android; empty elsewhere).
    #[must_use]
    pub fn inbox_batches(&self) -> &[PathBuf] {
        &self.inbox_batches
    }

    /// This set followed by `later`: files, skipped documents and inbox batches are each
    /// concatenated in order, so pages delivered later come after the earlier ones.
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code, reason = "consumed by the shell's external-delivery merge (`screen::merge_open`, plan WP7); remove this attribute when WP7 lands"))]
    pub fn merged(mut self, later: Self) -> Self {
        self.files.extend(later.files);
        self.skipped.extend(later.skipped);
        self.inbox_batches.extend(later.inbox_batches);
        self
    }

    /// The path that names the set in the window title: the first file, else the first skipped
    /// document (a set of rejected documents only).
    fn title_path(&self) -> Option<&Path> {
        self.files.first().map(PathBuf::as_path).or_else(|| self.skipped.first().map(|skipped| skipped.path.as_path()))
    }
}

/// What the user asked the studio to open, before any scratch session exists: the output of
/// startup routing (CLI) or of the launcher, turned into a [`StudioOpenRequest`] by the open
/// worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupTarget {
    /// A chapter directory (`--project` or a launcher pick).
    Project(PathBuf),
    /// An ordered image set to edit in image-set (single-image) mode (`--image`, positional
    /// paths, launcher multi-pick). One file is the classic single-image mode.
    Images(ImageSources),
}

impl StartupTarget {
    /// Studio window title: `ManhwaStudio v{version} - {chapter path}` for a project; for an
    /// image set the one owner `crate::single_image::image_window_title` (first file name, plus
    /// the count of the others). `version` is the display version (`MS_APP_VERSION`).
    #[must_use]
    pub fn window_title(&self, version: &str) -> String {
        match self {
            Self::Project(project_dir) => format!("ManhwaStudio v{version} - {}", project_dir.display()),
            Self::Images(sources) => match sources.title_path() {
                Some(first) => crate::single_image::image_window_title(version, first, sources.files().len().max(1)),
                // Unreachable: `ImageSources::new` refuses a set with neither files nor skips.
                None => format!("ManhwaStudio v{version}"),
            },
        }
    }

    /// `true` for an image-set target (a session that never reloads from disk).
    #[must_use]
    pub fn is_image_set(&self) -> bool {
        match self {
            Self::Project(_) => false,
            Self::Images(_) => true,
        }
    }
}

/// What one studio session opened. Created by the open worker, owned by the studio screen for
/// the session's whole life (plan D13) and ended with [`StudioOpenRequest::release`] by the
/// teardown worker.
#[derive(Debug)]
pub enum StudioOpenRequest {
    /// A chapter directory, loaded with the regular unsaved-detection + `ProjectData::load*`.
    Project(PathBuf),
    /// An image set opened through a reserved scratch chapter. The scratch is shared (`Arc`)
    /// only while the open worker uses it; `release` deletes it once it is the sole owner.
    Images { sources: ImageSources, scratch: Arc<SingleImageScratch> },
}

impl StudioOpenRequest {
    /// Maps a routing target to a request. A project maps 1:1; an image set reserves a fresh
    /// scratch session under `scratch_base` (production: `ms_config::single_image::scratch_base()`).
    /// Blocking (mkdir + lock + marker): the open worker calls it, never the GUI thread.
    ///
    /// # Errors
    /// The `SingleImageScratch::reserve` error; nothing is left behind on failure (the inbox
    /// batches of the set are the caller's to release).
    pub fn from_target(target: StartupTarget, scratch_base: &Path) -> Result<Self, SingleImageError> {
        match target {
            StartupTarget::Project(project_dir) => Ok(Self::Project(project_dir)),
            StartupTarget::Images(sources) => {
                let scratch = SingleImageScratch::reserve(scratch_base)?;
                Ok(Self::Images { sources, scratch: Arc::new(scratch) })
            }
        }
    }

    /// Ends the request after its session: deletes an image-set scratch session (blocking
    /// recursive delete — a worker or the process-shutdown hook, never a regular frame), then
    /// releases the set's inbox batches (`ms_platform::incoming::release_batch_blocking`, whose
    /// files were copied into the scratch at open). A delete failure, or a scratch still shared
    /// elsewhere, is logged; the next startup sweep removes it.
    pub fn release(self) {
        match self {
            Self::Project(_) => {}
            Self::Images { sources, scratch } => {
                match Arc::try_unwrap(scratch) {
                    Ok(scratch) => {
                        if let Err(err) = scratch.remove() {
                            runtime_log::log_warn(format!(
                                "[studio] could not delete the image-set scratch for {} file(s) starting with '{}': {err}; the next startup sweep retries",
                                sources.files().len(),
                                sources.title_path().map_or_else(String::new, |path| path.display().to_string())
                            ));
                        }
                    }
                    Err(shared) => runtime_log::log_warn(format!(
                        "[studio] image-set scratch {} is still shared; left for the next startup sweep",
                        shared.root().display()
                    )),
                }
                release_inbox_batches(sources.inbox_batches());
            }
        }
    }
}

/// Releases Android inbox batch folders (blocking JNI + file I/O: worker threads only). A failure
/// is logged by the platform layer; leftovers are swept at the next process start.
fn release_inbox_batches(batches: &[PathBuf]) {
    for batch in batches {
        ms_platform::incoming::release_batch_blocking(batch);
    }
}

/// What the open worker delivers at its end: the request it created (`None` when the scratch
/// reservation failed) and the load result.
pub(crate) struct OpenOutcome {
    pub request: Option<StudioOpenRequest>,
    pub result: anyhow::Result<ProjectData>,
}

/// One message of the open worker, in order: any number of `Progress` and at most one
/// `NeedsConfirmation`, then exactly one `Finished` (unless the worker died).
pub(crate) enum OpenEvent {
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
pub(crate) struct OpenWorker {
    events: Receiver<OpenEvent>,
    /// The large-set answer (`true` = open). Dropping it while the worker waits is a cancel.
    confirm: Sender<bool>,
    /// Shared cancel flag, checked by `open_image_set` before every file. Project loads ignore it.
    cancel: Arc<AtomicBool>,
}

impl OpenWorker {
    /// Answers a pending large-set confirmation. A worker that is already gone is logged.
    fn answer_confirmation(&self, open: bool) {
        if self.confirm.send(open).is_err() {
            runtime_log::log_warn(format!("[studio] the open worker ended before the large-set answer (open={open}) arrived"));
        }
    }

    /// Raises the cancel flag: an image-set open stops before its next file.
    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The worker end of [`OpenWorker`].
struct WorkerLink {
    events: Sender<OpenEvent>,
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
fn open_channels() -> (OpenWorker, WorkerLink) {
    let (events_tx, events_rx) = mpsc::channel();
    let (confirm_tx, confirm_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    (OpenWorker { events: events_rx, confirm: confirm_tx, cancel: Arc::clone(&cancel) }, WorkerLink { events: events_tx, confirm: confirm_rx, cancel })
}

/// Spawns the named background worker that opens `target` for the studio screen.
///
/// - `Project`: detect a `{chapter}_unsaved` folder next to the chapter, then
///   `load_resume_unsaved` vs `load`.
/// - `Images`: reserve a scratch session under `scratch_base` (a failure is the B5 error
///   screen), then — for a set of two or more files — estimate its decoded size from the file
///   headers and, above the soft limits, ask the screen and wait for the answer; then
///   `open_image_set` into the scratch with progress events and the shared cancel flag (no
///   unsaved detection: a scratch is always fresh). A one-file set is opened exactly as the
///   classic single-image mode: no estimate, no question. Errors are logged in technical form
///   here and reported as `SingleImageError::user_message`, the one owner of decode/open text.
///
/// The user settings are re-read on the worker so a reload observes settings changed during the
/// session; an unreadable config falls back to `fallback_user_settings` (logged). The events (or
/// the spawn failure, as a disconnected channel) are observed by `StudioScreen::poll_load`.
pub(crate) fn spawn_open_thread(target: StartupTarget, scratch_base: PathBuf, fallback_user_settings: serde_json::Value) -> OpenWorker {
    let (worker, link) = open_channels();
    let spawn_result = thread::Builder::new().name("studio-project-load".to_string()).spawn(move || {
        let outcome = open_target(target, &scratch_base, fallback_user_settings, &link, crate::project::single_image::estimate_image_set);
        if let Err(mpsc::SendError(event)) = link.events.send(OpenEvent::Finished(Box::new(outcome))) {
            runtime_log::log_info("[studio] the studio screen was left before the load finished; result dropped");
            // Nobody will own the request: release it here (still on the worker).
            if let OpenEvent::Finished(outcome) = event
                && let Some(request) = outcome.request
            {
                request.release();
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
fn open_target(target: StartupTarget, scratch_base: &Path, fallback_user_settings: serde_json::Value, link: &WorkerLink, estimate: fn(&[PathBuf]) -> ImageSetEstimate) -> OpenOutcome {
    let inbox_batches = match &target {
        StartupTarget::Project(_) => Vec::new(),
        StartupTarget::Images(sources) => sources.inbox_batches().to_vec(),
    };
    let request = match StudioOpenRequest::from_target(target, scratch_base) {
        Ok(request) => request,
        Err(err) => {
            runtime_log::log_error(format!("[studio] could not reserve a single-image scratch session: {err}"));
            // No request will own the delivered files: release their inbox batches now.
            release_inbox_batches(&inbox_batches);
            return OpenOutcome { request: None, result: Err(anyhow::Error::msg(err.user_message())) };
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
    let result = match &request {
        StudioOpenRequest::Project(project_dir) => load_project_dir(project_dir, &user_settings),
        StudioOpenRequest::Images { sources, scratch } => open_images(sources, scratch, &user_settings, link, estimate),
    };
    OpenOutcome { request: Some(request), result }
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

/// Returns true when there is a `{chapter}_unsaved` folder next to the chapter dir.
fn detect_unsaved_for_project(project_dir: &Path) -> bool {
    let Some(chapter_name) = project_dir.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(title_dir) = project_dir.parent() else {
        return false;
    };
    title_dir.join(format!("{chapter_name}_unsaved")).is_dir()
}

/// Loads a chapter directory: `load_resume_unsaved` when a `{chapter}_unsaved` folder exists
/// next to it, else `load`. Blocking; open worker only.
fn load_project_dir(project_dir: &Path, user_settings: &serde_json::Value) -> anyhow::Result<ProjectData> {
    if detect_unsaved_for_project(project_dir) {
        ProjectData::load_resume_unsaved(project_dir, user_settings)
    } else {
        ProjectData::load(project_dir, user_settings)
    }
    // Same greppable wording as the old startup-path context.
    .with_context(|| format!("failed to load project at {}", project_dir.display()))
}

/// Installs the `egui-shader-layers` glow backend on the main window's egui context.
///
/// Called once from the eframe app creator, the only place the `glow::Context` is available
/// before the first frame. Failure never aborts startup: the library registers a failed backend,
/// the PS editor's «Коррекция» panel then reports the correction as unavailable, and the cause is
/// logged here with the GL context's version. The matching teardown is the shell's `on_exit`.
pub(crate) fn install_shader_layers(cc: &eframe::CreationContext<'_>) {
    let Some(gl) = cc.gl.as_deref() else {
        runtime_log::log_error(
            "[shell] eframe created the main window without a glow context; shader layers (the PS \
             editor's «Коррекция») are unavailable. Possible cause: a non-glow renderer",
        );
        return;
    };
    if let Err(error) = egui_shader_layers::install_glow(&cc.egui_ctx, gl) {
        use eframe::glow::HasContext as _;
        let version = gl.version();
        runtime_log::log_error(format!(
            "[shell] shader layers disabled: {error}. GL context: {}.{} (embedded: {}). \
             The PS editor's «Коррекция» panel reports the correction as unavailable",
            version.major, version.minor, version.is_embedded
        ));
    }
}

/// What the open worker delivered, as seen by the shell.
pub(crate) enum LoadResult {
    /// The project; the shell decides whether to run it (`start_running`) or discard it. Boxed:
    /// the snapshot is large and only moves through.
    Loaded(Box<ProjectData>),
    /// The load failed (already logged); the text for the error screen.
    Failed(String),
}

/// How the studio screen wants to be left; the shell maps it to a `ShellEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StudioExit {
    /// `MangaApp` decided to close (exit dialog, save-then-close, launcher button, OS close).
    Close(PendingCloseAction),
    /// `MangaApp` asked to rebuild itself from disk after a structural page operation.
    Reload { project_dir: PathBuf },
    /// Error screen: "Exit to launcher".
    FailedBack,
    /// Error screen: "Exit".
    FailedExit,
}

/// The loading phase: the worker's GUI end plus what it reported so far.
struct Loading {
    worker: OpenWorker,
    /// Last `(done, total)` progress of an image-set open.
    progress: Option<(usize, usize)>,
    /// A large-set question waiting for the user's answer.
    confirmation: Option<ImageSetEstimate>,
}

/// What the loading screen's buttons asked for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadingAction {
    /// "Open" on the large-set confirmation.
    OpenLargeSet,
    /// "Cancel" on the confirmation or under the progress bar.
    Cancel,
}

/// What the loading screen shows (a snapshot of [`Loading`] plus the screen's flags).
#[derive(Debug, Clone, Copy)]
struct LoadingView {
    /// The shell deferred an OS close until the worker delivered.
    closing: bool,
    /// The target is an image set (progress and Cancel exist only there).
    image_set: bool,
    /// The user already pressed Cancel: no more buttons.
    cancel_requested: bool,
    progress: Option<(usize, usize)>,
    confirmation: Option<ImageSetEstimate>,
}

/// The studio screen's phase data (the matching pure phase is `screen::StudioPhase`).
enum Phase {
    /// Waiting on the open worker.
    Loading(Loading),
    /// The load failed; the error screen shows `error_text`.
    Failed { error_text: String },
    /// The user cancelled the open; the screen exits with `FailedBack` (pure phase: `Failed`).
    Cancelled,
    /// The project arrived; every frame is delegated to the real app.
    Running(Box<MangaApp>),
}

/// The studio screen: owns the open worker's GUI end, the running `MangaApp` and the session's
/// [`StudioOpenRequest`] until the shell moves them onto the teardown worker ([`Self::into_parts`]).
pub(crate) struct StudioScreen {
    phase: Phase,
    /// The session's request, delivered by the open worker; released by the teardown.
    request: Option<StudioOpenRequest>,
    /// The session edits an image set through a scratch chapter; such a session never reloads.
    image_set: bool,
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
        // A set can hold hundreds of paths: log its size and first file, not the whole list.
        match &target {
            StartupTarget::Project(project_dir) => runtime_log::log_info(format!("[studio] opening project {}", project_dir.display())),
            StartupTarget::Images(sources) => runtime_log::log_info(format!(
                "[studio] opening an image set: {} file(s), {} already skipped, {} inbox batch(es), first {:?}",
                sources.files().len(),
                sources.skipped().len(),
                sources.inbox_batches().len(),
                sources.title_path()
            )),
        }
        let worker = spawn_open_thread(target, crate::config::single_image::scratch_base(), fallback_user_settings);
        Self::with_worker(worker, image_set, reload_tab, ai_backend)
    }

    /// The loading screen over an already running `worker`.
    fn with_worker(worker: OpenWorker, image_set: bool, reload_tab: Option<AppTab>, ai_backend: AiBackendHandle) -> Self {
        let loading = Loading { worker, progress: None, confirmation: None };
        Self { phase: Phase::Loading(loading), request: None, image_set, user_cancelled: false, reload_tab, ai_backend }
    }

    /// Drains the open worker's events (non-blocking; `None` while it runs or outside `Loading`):
    /// progress and the large-set question are kept for the loading screen; the final outcome is
    /// returned and the request it carries is kept by the screen. A worker that died without an
    /// outcome is a failure. After a user cancel even a successful open is reported as a
    /// cancelled load (the request is still released by the teardown).
    pub(crate) fn poll_load(&mut self) -> Option<LoadResult> {
        let Phase::Loading(loading) = &mut self.phase else {
            return None;
        };
        loop {
            match loading.worker.events.try_recv() {
                Ok(OpenEvent::Progress { done, total }) => loading.progress = Some((done, total)),
                Ok(OpenEvent::NeedsConfirmation(estimate)) => loading.confirmation = Some(estimate),
                Ok(OpenEvent::Finished(outcome)) => {
                    let OpenOutcome { request, result } = *outcome;
                    self.request = request;
                    return Some(match result {
                        Ok(_project) if self.user_cancelled => {
                            runtime_log::log_info("[studio] the image set opened after the user cancelled; discarded");
                            LoadResult::Failed(SingleImageError::Cancelled.user_message())
                        }
                        Ok(project) => LoadResult::Loaded(Box::new(project)),
                        Err(err) => {
                            // `{err:#}` prints the whole anyhow chain, including the greppable
                            // "failed to load project at …" context added by the worker.
                            let error_text = format!("{err:#}");
                            runtime_log::log_error(format!("[studio] {error_text}"));
                            LoadResult::Failed(error_text)
                        }
                    });
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    // Worker panicked before sending, or the spawn itself failed (logged in
                    // `spawn_open_thread`). A dead worker holds no file handles.
                    runtime_log::log_error("[studio] project load thread exited without a result");
                    return Some(LoadResult::Failed(t!("studio_bootstrap.load_thread_exited_error").to_string()));
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
    /// Idempotent; a project load is left to finish (it may be mid-write).
    fn cancel_for_close(&mut self) {
        if self.image_set
            && let Phase::Loading(loading) = &mut self.phase
        {
            stop_worker(loading);
        }
    }

    /// Constructs `MangaApp` from `project` and starts running it (on the GUI thread, as the app
    /// constructor only spawns workers). Starts the deferred storage-mode reconciliation of a
    /// direct `--project` start, now that the project is loaded.
    pub(crate) fn start_running(&mut self, project: Box<ProjectData>) {
        let mut app = MangaApp::new(*project, self.ai_backend.clone());
        if let Some(tab) = self.reload_tab.take() {
            app.set_active_tab(tab);
        }
        // No-op when nothing is pending or it already ran; progress and failures show in the
        // General settings pane and the log, and an incomplete run raises the studio's top-bar
        // notice (`MangaApp::draw_storage_reconcile_notice`).
        if let Some(job) = crate::storage_mode_job::start_pending_reconciliation() {
            runtime_log::log_info(format!("[studio] started the deferred storage-mode reconciliation (job {job})"));
        }
        self.phase = Phase::Running(Box::new(app));
    }

    /// Shows the error screen with `error_text`, or — after a user cancel — leaves for the
    /// launcher on the next frame.
    pub(crate) fn fail(&mut self, error_text: String) {
        self.phase = if self.user_cancelled { Phase::Cancelled } else { Phase::Failed { error_text } };
    }

    /// Hands the OS close button to the running studio (`MangaApp::request_os_close`); a no-op in
    /// the other phases.
    pub(crate) fn request_os_close(&mut self) {
        if let Phase::Running(app) = &mut self.phase {
            app.request_os_close();
        }
    }

    /// The close the running studio decided on, if any (`MangaApp::take_close_request`).
    pub(crate) fn take_close_request(&mut self) -> Option<PendingCloseAction> {
        match &mut self.phase {
            Phase::Running(app) => app.take_close_request(),
            Phase::Loading(_) | Phase::Failed { .. } | Phase::Cancelled => None,
        }
    }

    /// The exit a phase produces without any input: `FailedBack` for a cancelled open.
    fn automatic_exit(&self) -> Option<StudioExit> {
        match self.phase {
            Phase::Cancelled => Some(StudioExit::FailedBack),
            Phase::Loading(_) | Phase::Failed { .. } | Phase::Running(_) => None,
        }
    }

    /// Draws the active phase and returns the exit it produced this frame. `closing_after_load`
    /// labels the loading screen as "finishing before close" (the shell deferred an OS close) and
    /// stops an image-set open.
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, closing_after_load: bool) -> Option<StudioExit> {
        if closing_after_load {
            self.cancel_for_close();
        }
        let automatic_exit = self.automatic_exit();
        let mut loading_action = None;
        let exit = match &mut self.phase {
            Phase::Loading(loading) => {
                let view = LoadingView {
                    closing: closing_after_load,
                    image_set: self.image_set,
                    cancel_requested: self.user_cancelled,
                    progress: loading.progress,
                    confirmation: loading.confirmation,
                };
                loading_action = draw_loading_screen(ui, view);
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
                if let Some(action) = app.take_close_request() {
                    return Some(StudioExit::Close(action));
                }
                if !app.take_project_reload_request() {
                    None
                } else if self.image_set {
                    // Unreachable by design: the reload follows a structural page operation and the
                    // Page Manager is hidden in image-set mode. Refuse instead of re-loading the
                    // scratch as a project; the session keeps running.
                    runtime_log::log_error("[studio] project reload requested in an image-set session; refused, the session keeps running");
                    None
                } else {
                    Some(StudioExit::Reload { project_dir: app.project_dir() })
                }
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
    /// session's request (if the worker delivered one). A project still loading is never split:
    /// the shell defers until the worker delivered.
    pub(crate) fn into_parts(self) -> (Option<Box<MangaApp>>, Option<StudioOpenRequest>) {
        let app = match self.phase {
            Phase::Running(app) => Some(app),
            Phase::Loading(_) | Phase::Failed { .. } | Phase::Cancelled => None,
        };
        (app, self.request)
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
fn draw_loading_screen(ui: &mut egui::Ui, view: LoadingView) -> Option<LoadingAction> {
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
fn draw_error_screen(ui: &mut egui::Ui, error_text: &str) -> Option<StudioExit> {
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

    /// Unique scratch base under the OS temp dir; each test removes it.
    fn temp_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ms-studio-open-request-{tag}-{}", std::process::id()))
    }

    /// An image-set target over `files` (no skips, no inbox).
    fn images(files: &[&str]) -> StartupTarget {
        StartupTarget::Images(ImageSources::new(files.iter().map(PathBuf::from).collect(), Vec::new(), Vec::new()).expect("non-empty set"))
    }

    fn skipped(path: &str) -> SkippedSource {
        SkippedSource { path: PathBuf::from(path), message: format!("cannot open {path}") }
    }

    #[test]
    fn project_target_maps_one_to_one_without_touching_the_scratch_base() {
        let base = temp_base("project");
        let dir = PathBuf::from("/home/u/titles/Title/Chapter 1");
        let target = StartupTarget::Project(dir.clone());
        assert!(!target.is_image_set());
        assert_eq!(target.window_title("1.2.3"), "ManhwaStudio v1.2.3 - /home/u/titles/Title/Chapter 1");
        let request = StudioOpenRequest::from_target(target, &base).expect("project maps");
        assert!(matches!(&request, StudioOpenRequest::Project(mapped) if *mapped == dir));
        assert!(!base.exists(), "a project start must not create the scratch base");
        request.release();
    }

    #[test]
    fn image_target_reserves_a_scratch_and_release_deletes_it() {
        let base = temp_base("image");
        let source = PathBuf::from("/home/u/Картинки/page one.jpg");
        let target = images(&["/home/u/Картинки/page one.jpg"]);
        assert!(target.is_image_set());
        assert_eq!(target.window_title("1.2.3"), "ManhwaStudio v1.2.3 - page one.jpg", "a one-file set keeps the single-image title");
        let request = StudioOpenRequest::from_target(target, &base).expect("scratch reserved");
        let root = match &request {
            StudioOpenRequest::Images { sources, scratch } => {
                assert_eq!(sources.files(), [source].as_slice());
                scratch.root().to_path_buf()
            }
            StudioOpenRequest::Project(dir) => panic!("image mapped to project {}", dir.display()),
        };
        assert!(root.starts_with(&base) && root.is_dir(), "scratch root {} must live under the base", root.display());
        request.release();
        assert!(!root.exists(), "release must delete the scratch session");
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn release_while_the_scratch_is_still_shared_leaves_it_for_the_sweep() {
        let base = temp_base("shared");
        let request = StudioOpenRequest::from_target(images(&["/home/u/a.png"]), &base).expect("scratch reserved");
        let other_share = match &request {
            StudioOpenRequest::Images { scratch, .. } => Arc::clone(scratch),
            StudioOpenRequest::Project(dir) => panic!("image mapped to project {}", dir.display()),
        };
        let root = other_share.root().to_path_buf();
        request.release();
        assert!(root.is_dir(), "a scratch still held elsewhere must not be deleted under its holder");
        // The last holder dropping the scratch releases the lock without I/O; the startup sweep
        // then owns the cleanup.
        drop(other_share);
        let report = crate::project::single_image::sweep_stale_sessions(&base);
        assert_eq!(report.removed, 1);
        assert!(!root.exists());
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn image_sources_need_a_file_or_a_skip_and_merge_in_order() {
        assert_eq!(ImageSources::new(Vec::new(), Vec::new(), Vec::new()), None);
        let only_skipped = ImageSources::new(Vec::new(), vec![skipped("/home/u/x.heic")], Vec::new()).expect("a skip alone is a set");
        assert!(only_skipped.files().is_empty());
        assert_eq!(StartupTarget::Images(only_skipped.clone()).window_title("1.2.3"), "ManhwaStudio v1.2.3 - x.heic");

        let first = ImageSources::new(vec![PathBuf::from("/in/1/b.png"), PathBuf::from("/in/1/a.png")], vec![skipped("/in/1/z.heic")], vec![PathBuf::from("/in/1")]).expect("set");
        let later = ImageSources::new(vec![PathBuf::from("/in/2/c.png")], Vec::new(), vec![PathBuf::from("/in/2")]).expect("set");
        let merged = first.merged(later).merged(only_skipped);
        assert_eq!(merged.files(), [PathBuf::from("/in/1/b.png"), PathBuf::from("/in/1/a.png"), PathBuf::from("/in/2/c.png")].as_slice());
        assert_eq!(merged.skipped(), [skipped("/in/1/z.heic"), skipped("/home/u/x.heic")].as_slice());
        assert_eq!(merged.inbox_batches(), [PathBuf::from("/in/1"), PathBuf::from("/in/2")].as_slice());
    }

    #[test]
    fn several_images_name_the_first_file_and_count_the_others() {
        let one = images(&["/home/u/b.png"]).window_title("1.2.3");
        let three = images(&["/home/u/b.png", "/home/u/a.jpg", "/home/u/c.webp"]).window_title("1.2.3");
        assert_eq!(one, "ManhwaStudio v1.2.3 - b.png");
        // The suffix text is a localized template (`single_image.window_title_more`); the prefix
        // is the fixed program name, and N files must not read like one.
        assert!(three.starts_with("ManhwaStudio v1.2.3 - "), "{three}");
        assert_ne!(three, one);
    }

    /// The open worker's body reports a scratch reservation failure (B5) as a failed load
    /// without a request, so the studio error screen shows it.
    #[test]
    fn scratch_reservation_failure_is_a_failed_load_without_a_request() {
        let base = temp_base("unreservable");
        // A FILE where the scratch base directory must be: the reservation cannot create it.
        std::fs::write(&base, b"not a directory").expect("write blocker file");
        let (_worker, link) = open_channels();
        let outcome = open_target(images(&["/home/u/a.png"]), &base, serde_json::Value::Null, &link, never_estimated);
        assert!(outcome.request.is_none());
        assert!(outcome.result.is_err());
        std::fs::remove_file(&base).expect("remove blocker file");
    }

    /// An estimate a one-file set must never ask for.
    fn never_estimated(_: &[PathBuf]) -> ImageSetEstimate {
        panic!("a one-file set must open without a size estimate")
    }

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
            let outcome = open_target(target, &scratch_base, serde_json::Value::Null, &link, large_estimate);
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
        let LoadResult::Failed(error_text) = wait_for_load(&mut screen) else {
            panic!("a cancelled set must not load");
        };
        handle.join().expect("worker thread");
        // The table routes the failed load to the Failed phase, which calls `fail`.
        screen.fail(error_text);
        assert_eq!(screen.automatic_exit(), Some(StudioExit::FailedBack));
        let (app, request) = screen.into_parts();
        assert!(app.is_none());
        let Some(request @ StudioOpenRequest::Images { .. }) = request else {
            panic!("the reserved scratch must reach the teardown");
        };
        request.release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn a_confirmed_large_set_opens_and_reports_its_skips() {
        let base = temp_base("large-open");
        let (mut screen, handle) = large_set_screen(&base);
        wait_for_question(&mut screen);
        screen.confirm_open();
        // Both files are missing: the open runs and fails listing them (not a cancel).
        let LoadResult::Failed(error_text) = wait_for_load(&mut screen) else {
            panic!("a set of missing files cannot load");
        };
        handle.join().expect("worker thread");
        assert!(matches!(&screen.phase, Phase::Loading(Loading { progress: Some((2, 2)), .. })), "progress reached the screen");
        screen.fail(error_text);
        assert_eq!(screen.automatic_exit(), None, "a real failure shows the error screen");
        let (_, request) = screen.into_parts();
        request.expect("scratch reserved").release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn a_deferred_close_cancels_the_question_without_a_user_cancel() {
        let base = temp_base("large-close");
        let (mut screen, handle) = large_set_screen(&base);
        wait_for_question(&mut screen);
        screen.cancel_for_close();
        let LoadResult::Failed(_) = wait_for_load(&mut screen) else {
            panic!("a set cancelled by the close must not load");
        };
        handle.join().expect("worker thread");
        let Phase::Loading(loading) = &screen.phase else {
            panic!("still loading until the shell moves on");
        };
        assert!(loading.worker.cancel.load(Ordering::Relaxed), "the open is cancelled");
        assert!(!screen.user_cancelled, "the close is not a user cancel");
        // The table sends a close-after-load failure to Closing(Exit); `fail` is not called then.
        let (_, request) = screen.into_parts();
        request.expect("scratch reserved").release();
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

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
