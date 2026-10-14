/*
File: src/shell/studio.rs

Purpose:
The studio screen of the main window. The project load (unsaved-session detection +
`ProjectData::load*`, or the single-image open into a reserved scratch chapter) runs on a
background thread behind a loading screen; a failure shows an error screen; once the project
arrives `MangaApp` is constructed in place and every frame is delegated to it. The screen never
closes, styles or titles the window: it reports how it wants to be left (`StudioExit`) and the
shell (`mod.rs`) decides through the pure table in `screen.rs`.

Key structures:
- `StartupTarget`: what the studio opens (a chapter directory or one image), the output of
  startup routing (CLI) or of the launcher; also derives the window title.
- `StudioOpenRequest`: an opened target — a project, or an image with its reserved
  `SingleImageScratch`. Created BY THE OPEN WORKER (the scratch reservation is I/O) and owned by
  the screen until the studio teardown releases it (`release`, deletes the scratch: never on
  the GUI thread).
- `StudioScreen`: phases `Loading` (polls the worker), `Failed` (error screen with
  back-to-launcher / exit buttons), `Running` (delegates `ui` to `MangaApp`).
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
mid-write can corrupt the chapter). A scratch reservation failure (B5) reaches the same error
screen. A single-image session refuses the structural-operation reload with a logged error (the
Page Manager is hidden there, so the request is unreachable). A damaged `{chapter}_unsaved`
session makes `load_resume_unsaved` fail with a localized error naming the files; "Exit to
launcher" is the route to discarding it there. `MangaApp::new` needs no
`eframe::CreationContext`, which is what makes construction inside a frame possible.
Native-only (gated with the whole shell host off wasm).
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::app::{MangaApp, PendingCloseAction};
use crate::project::ProjectData;
use crate::project::single_image::{SingleImageError, SingleImageScratch};
use crate::runtime_log;
use crate::tabs::AppTab;
use anyhow::Context;
// `MangaApp` draws through its `eframe::App::ui`; the shell is the real eframe app.
use eframe::App as _;
use ms_thread as thread;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// What the user asked the studio to open, before any scratch session exists: the output of
/// startup routing (CLI) or of the launcher, turned into a [`StudioOpenRequest`] by the open
/// worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupTarget {
    /// A chapter directory (`--project` or a launcher pick).
    Project(PathBuf),
    /// One picture file to edit in single-image mode (`--image`, positional, launcher button).
    Image(PathBuf),
}

impl StartupTarget {
    /// Studio window title: `ManhwaStudio v{version} - {chapter path}` for a project,
    /// `ManhwaStudio v{version} - {file name}` for an image (the full path when it has no
    /// file-name component). `version` is the display version (`MS_APP_VERSION`).
    #[must_use]
    pub fn window_title(&self, version: &str) -> String {
        match self {
            Self::Project(project_dir) => format!("ManhwaStudio v{version} - {}", project_dir.display()),
            Self::Image(source) => crate::single_image::image_window_title(version, source),
        }
    }

    /// `true` for a single-image target (a session that never reloads from disk).
    #[must_use]
    pub fn is_single_image(&self) -> bool {
        match self {
            Self::Project(_) => false,
            Self::Image(_) => true,
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
    /// A single image opened through a reserved scratch chapter. Shared (`Arc`) only while the
    /// open worker uses it; `release` deletes it once it is the sole owner.
    Image { source: PathBuf, scratch: Arc<SingleImageScratch> },
}

impl StudioOpenRequest {
    /// Maps a routing target to a request. A project maps 1:1; an image reserves a fresh
    /// scratch session under `scratch_base` (production: `ms_config::single_image::scratch_base()`).
    /// Blocking (mkdir + lock + marker): the open worker calls it, never the GUI thread.
    ///
    /// # Errors
    /// The `SingleImageScratch::reserve` error; nothing is left behind on failure.
    pub fn from_target(target: StartupTarget, scratch_base: &Path) -> Result<Self, SingleImageError> {
        match target {
            StartupTarget::Project(project_dir) => Ok(Self::Project(project_dir)),
            StartupTarget::Image(source) => {
                let scratch = SingleImageScratch::reserve(scratch_base)?;
                Ok(Self::Image { source, scratch: Arc::new(scratch) })
            }
        }
    }

    /// Ends the request after its session: deletes a single-image scratch session (blocking
    /// recursive delete — a worker or the process-shutdown hook, never a regular frame). A delete
    /// failure, or a scratch still shared elsewhere, is logged; the next startup sweep removes it.
    pub fn release(self) {
        match self {
            Self::Project(_) => {}
            Self::Image { source, scratch } => match Arc::try_unwrap(scratch) {
                Ok(scratch) => {
                    if let Err(err) = scratch.remove() {
                        runtime_log::log_warn(format!(
                            "[studio] could not delete the single-image scratch for '{}': {err}; the next startup sweep retries",
                            source.display()
                        ));
                    }
                }
                Err(shared) => runtime_log::log_warn(format!(
                    "[studio] single-image scratch {} is still shared; left for the next startup sweep",
                    shared.root().display()
                )),
            },
        }
    }
}

/// What the open worker delivers: the request it created (`None` when the scratch reservation
/// failed) and the load result.
pub(crate) struct OpenOutcome {
    pub request: Option<StudioOpenRequest>,
    pub result: anyhow::Result<ProjectData>,
}

/// Spawns the named background worker that opens `target` for the studio screen.
///
/// - `Project`: detect a `{chapter}_unsaved` folder next to the chapter, then
///   `load_resume_unsaved` vs `load`.
/// - `Image`: reserve a scratch session under `scratch_base` (a failure is the B5 error screen),
///   then `open_image_set` of that one file into it (no unsaved detection: a scratch is always
///   fresh). Errors are logged in technical form here and reported as
///   `SingleImageError::user_message`, the one owner of decode/open error text.
///
/// The user settings are re-read on the worker so a reload observes settings changed during the
/// session; an unreadable config falls back to `fallback_user_settings` (logged). The outcome (or
/// the spawn failure, as a disconnected channel) is observed by `StudioScreen::poll_load`.
pub(crate) fn spawn_open_thread(target: StartupTarget, scratch_base: PathBuf, fallback_user_settings: serde_json::Value) -> Receiver<OpenOutcome> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new().name("studio-project-load".to_string()).spawn(move || {
        let outcome = open_target(target, &scratch_base, fallback_user_settings);
        if tx.send(outcome).is_err() {
            runtime_log::log_info("[studio] the studio screen was left before the load finished; result dropped");
        }
    });
    if let Err(err) = spawn_result {
        // The sender is dropped here, so the screen observes `Disconnected` and shows the error
        // screen instead of spinning forever.
        runtime_log::log_error(format!("[studio] failed to spawn studio-project-load thread: {err}"));
    }
    rx
}

/// The open worker's body (blocking; see [`spawn_open_thread`]).
fn open_target(target: StartupTarget, scratch_base: &Path, fallback_user_settings: serde_json::Value) -> OpenOutcome {
    let request = match StudioOpenRequest::from_target(target, scratch_base) {
        Ok(request) => request,
        Err(err) => {
            runtime_log::log_error(format!("[studio] could not reserve a single-image scratch session: {err}"));
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
        // A one-file image set: no progress observer and no cancel yet (the loading screen has a
        // spinner only), so the flag never flips.
        StudioOpenRequest::Image { source, scratch } => crate::project::single_image::open_image_set(std::slice::from_ref(source), Vec::new(), scratch, &user_settings, &mut |_, _| {}, &AtomicBool::new(false)).map_err(|err| {
            runtime_log::log_error(format!("[studio] failed to open single image: {err}"));
            anyhow::Error::msg(err.user_message())
        }),
    };
    OpenOutcome { request: Some(request), result }
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

/// The studio screen's phase data (the matching pure phase is `screen::StudioPhase`).
enum Phase {
    /// Waiting on the open worker.
    Loading { rx: Receiver<OpenOutcome> },
    /// The load failed; the error screen shows `error_text`.
    Failed { error_text: String },
    /// The project arrived; every frame is delegated to the real app.
    Running(Box<MangaApp>),
}

/// The studio screen: owns the open worker's receiver, the running `MangaApp` and the session's
/// [`StudioOpenRequest`] until the shell moves them onto the teardown worker ([`Self::into_parts`]).
pub(crate) struct StudioScreen {
    phase: Phase,
    /// The session's request, delivered by the open worker; released by the teardown.
    request: Option<StudioOpenRequest>,
    /// The session edits one image through a scratch chapter; such a session never reloads.
    single_image: bool,
    /// Tab restored after a structural-operation reload; selection intentionally does not persist.
    reload_tab: Option<AppTab>,
    ai_backend: AiBackendHandle,
}

impl StudioScreen {
    /// Starts the open worker for `target` and shows the loading screen. `reload_tab` is the tab a
    /// reload returns to. Spawns a thread only; no I/O on the caller.
    pub(crate) fn start(target: StartupTarget, reload_tab: Option<AppTab>, fallback_user_settings: serde_json::Value, ai_backend: AiBackendHandle) -> Self {
        let single_image = target.is_single_image();
        runtime_log::log_info(format!("[studio] opening {target:?}"));
        let rx = spawn_open_thread(target, crate::config::single_image::scratch_base(), fallback_user_settings);
        Self { phase: Phase::Loading { rx }, request: None, single_image, reload_tab, ai_backend }
    }

    /// Takes the open worker's outcome when it has arrived (non-blocking; `None` while it runs or
    /// outside `Loading`). The request it carries is kept by the screen. A worker that died
    /// without an outcome is a failure.
    pub(crate) fn poll_load(&mut self) -> Option<LoadResult> {
        let Phase::Loading { rx } = &self.phase else {
            return None;
        };
        match rx.try_recv() {
            Ok(OpenOutcome { request, result }) => {
                self.request = request;
                Some(match result {
                    Ok(project) => LoadResult::Loaded(Box::new(project)),
                    Err(err) => {
                        // `{err:#}` prints the whole anyhow chain, including the greppable
                        // "failed to load project at …" context added by the worker.
                        let error_text = format!("{err:#}");
                        runtime_log::log_error(format!("[studio] {error_text}"));
                        LoadResult::Failed(error_text)
                    }
                })
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // Worker panicked before sending, or the spawn itself failed (logged in
                // `spawn_open_thread`). A dead worker holds no file handles.
                runtime_log::log_error("[studio] project load thread exited without a result");
                Some(LoadResult::Failed(t!("studio_bootstrap.load_thread_exited_error").to_string()))
            }
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

    /// Shows the error screen with `error_text`.
    pub(crate) fn fail(&mut self, error_text: String) {
        self.phase = Phase::Failed { error_text };
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
            Phase::Loading { .. } | Phase::Failed { .. } => None,
        }
    }

    /// Draws the active phase and returns the exit it produced this frame. `closing_after_load`
    /// labels the loading screen as "finishing before close" (the shell deferred an OS close).
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, closing_after_load: bool) -> Option<StudioExit> {
        match &mut self.phase {
            Phase::Loading { .. } => {
                draw_loading_screen(ui, closing_after_load);
                None
            }
            Phase::Failed { error_text } => draw_error_screen(ui, error_text),
            Phase::Running(app) => {
                app.ui(ui, frame);
                if let Some(action) = app.take_close_request() {
                    return Some(StudioExit::Close(action));
                }
                if !app.take_project_reload_request() {
                    None
                } else if self.single_image {
                    // Unreachable by design: the reload follows a structural page operation and the
                    // Page Manager is hidden in single-image mode. Refuse instead of re-loading the
                    // scratch as a project; the session keeps running.
                    runtime_log::log_error("[studio] project reload requested in a single-image session; refused, the session keeps running");
                    None
                } else {
                    Some(StudioExit::Reload { project_dir: app.project_dir() })
                }
            }
        }
    }

    /// Splits the screen into what the teardown worker needs: the running app (if any) and the
    /// session's request (if the worker delivered one). A project still loading is never split:
    /// the shell defers until the worker delivered.
    pub(crate) fn into_parts(self) -> (Option<Box<MangaApp>>, Option<StudioOpenRequest>) {
        let app = match self.phase {
            Phase::Running(app) => Some(app),
            Phase::Loading { .. } | Phase::Failed { .. } => None,
        };
        (app, self.request)
    }
}

/// Centered spinner + status label while the open worker runs. With `closing` the label explains
/// that the pending close waits for file operations to finish.
fn draw_loading_screen(ui: &mut egui::Ui, closing: bool) {
    egui::CentralPanel::default().show(ui, |ui| {
        // Push the spinner block to the vertical center of the window.
        let offset = (ui.available_height() * 0.5 - 40.0).max(0.0);
        ui.add_space(offset);
        ui.vertical_centered(|ui| {
            ui.spinner();
            ui.add_space(10.0);
            if closing {
                ui.label(t!("studio_bootstrap.finishing_before_close"));
            } else {
                ui.label(t!("studio_bootstrap.loading"));
            }
        });
    });
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

    #[test]
    fn project_target_maps_one_to_one_without_touching_the_scratch_base() {
        let base = temp_base("project");
        let dir = PathBuf::from("/home/u/titles/Title/Chapter 1");
        let target = StartupTarget::Project(dir.clone());
        assert!(!target.is_single_image());
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
        let target = StartupTarget::Image(source.clone());
        assert!(target.is_single_image());
        assert_eq!(target.window_title("1.2.3"), "ManhwaStudio v1.2.3 - page one.jpg");
        let request = StudioOpenRequest::from_target(target, &base).expect("scratch reserved");
        let root = match &request {
            StudioOpenRequest::Image { source: mapped, scratch } => {
                assert_eq!(*mapped, source);
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
        let request = StudioOpenRequest::from_target(StartupTarget::Image(PathBuf::from("/home/u/a.png")), &base).expect("scratch reserved");
        let other_share = match &request {
            StudioOpenRequest::Image { scratch, .. } => Arc::clone(scratch),
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

    /// The open worker's body reports a scratch reservation failure (B5) as a failed load
    /// without a request, so the studio error screen shows it.
    #[test]
    fn scratch_reservation_failure_is_a_failed_load_without_a_request() {
        let base = temp_base("unreservable");
        // A FILE where the scratch base directory must be: the reservation cannot create it.
        std::fs::write(&base, b"not a directory").expect("write blocker file");
        let outcome = open_target(StartupTarget::Image(PathBuf::from("/home/u/a.png")), &base, serde_json::Value::Null);
        assert!(outcome.request.is_none());
        assert!(outcome.result.is_err());
        std::fs::remove_file(&base).expect("remove blocker file");
    }
}
