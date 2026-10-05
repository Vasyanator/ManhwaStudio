/*
File: studio_bootstrap.rs

Purpose:
Startup shell for the studio window. The window opens immediately while the project load
(unsaved-session detection + `ProjectData::load*`) runs on a background thread; a minimal
loading screen is shown until the project arrives, then `MangaApp` is constructed in place
and every subsequent frame is delegated to it. Previously the load ran synchronously on the
main thread with no window on screen, so a first open of a JPEG-content chapter (full
re-encode of all pages) or a legacy migration left the user staring at nothing for seconds.

Key structures:
- `StudioBootstrapApp`: `eframe::App` wrapper with four states — `Loading` (polls the
  worker's receiver, draws a centered spinner), `Failed` (error screen with
  return-to-launcher / exit buttons), `Running` (delegates `ui` and `on_exit` to `MangaApp`),
  and `ClosingDiscarded` (a close arrived during `Loading`; the worker's result has been
  received and discarded, so the deferred close proceeds).
- Deferred storage-mode reconciliation: on a direct `--project` start, the pending
  conversion (`storage_mode_job`) is started once the project has loaded.
- `StartupTarget` / `StudioOpenRequest`: what a studio window opens — a chapter directory, or
  one image through a reserved single-image scratch session (`ms_project::single_image`). The
  `run_main` loop owns the request; `StudioOpenRequest::release` deletes the scratch after the
  window has closed (plan D13). Also derives the window title and the `fonts/ui` probe roots.
- `spawn_open_thread`: named load worker. Project: `detect_unsaved_for_project` choosing
  `load_resume_unsaved` vs `load` (the previous synchronous startup sequence). Image:
  `open_single_image`; its error reaches the error screen as `SingleImageError::user_message`.
- `install_shader_layers`: installs the `egui-shader-layers` glow backend (the PS editor's
  «Коррекция» shader) from the app creator; `StudioBootstrapApp::on_exit` destroys it.

Notes:
Only WHERE the load runs changed, not the load itself: the worker performs exactly the
detect-unsaved/load sequence `run_main` used to run before creating the window. A load
failure is logged with the same greppable wording as the old startup `with_context`
("failed to load project at …") and rendered as an error screen; "Exit to launcher" raises
the shared `return_to_launcher_flag` before closing the window, so the outer `run_main`
loop resolves the existing `RunResult` mechanism unchanged for both buttons.
Closing the window during `Loading` is intercepted (`CancelClose`): the load worker performs
non-atomic filesystem writes (`cleaned/` seeding placeholders, JPEG->PNG re-encode +
source removal) and killing it mid-write can permanently corrupt the chapter, so the shell
waits for the worker's result, discards it, and only then really closes.
A single-image session takes the same error screen on a decode/open failure ("Exit to
launcher" or "Exit"), and refuses the structural-operation reload with a logged error (the
Page Manager is hidden there, so the request is unreachable).
A damaged `{chapter}_unsaved` session (a staging document that does not parse) makes
`load_resume_unsaved` fail with a localized error naming the files; the error screen's
"Exit to launcher" is the route to discarding it there. Nothing is deleted automatically.
`MangaApp::new` does not need `eframe::CreationContext`, which is what makes late
construction inside a frame possible. Native-only: compiled together with the native
windowed startup flow (`run_main_window`), gated off wasm at the module declaration.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::app::MangaApp;
use crate::project::ProjectData;
use crate::project::single_image::{SingleImageError, SingleImageScratch};
use crate::runtime_log;
use crate::tabs::AppTab;
use crate::window_geometry::{self, WindowGeometryTracker};
use anyhow::Context;
use ms_thread as thread;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// How often the loading screen polls the load worker's receiver (the frame only repaints
/// on input otherwise).
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the user asked the studio to open, before any scratch session exists: the output of
/// startup routing (CLI or launcher), turned into a [`StudioOpenRequest`] by the `run_main` loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupTarget {
    /// A chapter directory (`--project` or a launcher pick).
    Project(PathBuf),
    /// One picture file to edit in single-image mode (`--image`, positional, launcher button).
    Image(PathBuf),
}

/// What one studio window opens. Owned by the `run_main` loop for the whole window life
/// (plan D13): the loop creates it before the window, lends it to `run_main_window`, and calls
/// [`StudioOpenRequest::release`] after the window has closed.
#[derive(Debug)]
pub enum StudioOpenRequest {
    /// A chapter directory, loaded with the regular unsaved-detection + `ProjectData::load*`.
    Project(PathBuf),
    /// A single image opened through a reserved scratch chapter. The scratch is shared (`Arc`)
    /// only with the load worker, which drops its clone before reporting its result, so the
    /// loop is the sole owner again once the window can close.
    Image { source: PathBuf, scratch: Arc<SingleImageScratch> },
}

impl StudioOpenRequest {
    /// Maps a routing target to a request. A project maps 1:1; an image reserves a fresh
    /// scratch session under `scratch_base` (production: `ms_config::single_image::scratch_base()`).
    /// Blocking but cheap (mkdir + lock + marker); runs before any window exists.
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

    /// `true` for a single-image session (decides the reload refusal and the font roots).
    #[must_use]
    pub fn is_single_image(&self) -> bool {
        match self {
            Self::Project(_) => false,
            Self::Image { .. } => true,
        }
    }

    /// Studio window title: `ManhwaStudio v{version} - {chapter path}` for a project,
    /// `ManhwaStudio v{version} - {file name}` for an image (the full path when it has no
    /// file-name component). `version` is the display version (`MS_APP_VERSION`).
    #[must_use]
    pub fn window_title(&self, version: &str) -> String {
        match self {
            Self::Project(project_dir) => format!("ManhwaStudio v{version} - {}", project_dir.display()),
            Self::Image { source, .. } => crate::single_image::image_window_title(version, source),
        }
    }

    /// Extra `fonts/ui` roots probed before the app directories: a project's title and chapter
    /// folders (a title may ship its own UI font chain). Empty for an image: the folder of a
    /// loose picture is not a title, and the scratch never holds fonts.
    #[must_use]
    pub fn font_roots(&self) -> Vec<PathBuf> {
        match self {
            Self::Project(project_dir) => project_dir
                .parent()
                .map(Path::to_path_buf)
                .into_iter()
                .chain(std::iter::once(project_dir.clone()))
                .collect(),
            Self::Image { .. } => Vec::new(),
        }
    }

    /// Ends the request after its window has closed: deletes a single-image scratch session
    /// (blocking recursive delete — call it only on the main thread after `run_native`
    /// returned, never on a GUI thread). A delete failure, or a scratch still shared with a
    /// load worker that has not finished, is logged; the next startup sweep removes it then.
    pub fn release(self) {
        match self {
            Self::Project(_) => {}
            Self::Image { source, scratch } => match Arc::try_unwrap(scratch) {
                Ok(scratch) => {
                    if let Err(err) = scratch.remove() {
                        runtime_log::log_warn(format!(
                            "[studio-bootstrap] could not delete the single-image scratch for '{}': {err}; the next startup sweep retries",
                            source.display()
                        ));
                    }
                }
                Err(shared) => runtime_log::log_warn(format!(
                    "[studio-bootstrap] single-image scratch {} is still held by the load worker; left for the next startup sweep",
                    shared.root().display()
                )),
            },
        }
    }
}

/// Spawns the named background worker that opens `request` for the studio window.
///
/// - `Project`: the previous synchronous startup path byte-for-byte: detect a
///   `{chapter}_unsaved` folder next to the chapter, then `load_resume_unsaved` vs `load`.
/// - `Image`: `open_single_image` into the reserved scratch (no unsaved detection: a scratch is
///   always fresh). Its error is logged in technical form here and reported to the loading
///   screen as `SingleImageError::user_message`, the one owner of decode/open error text.
///
/// The result (or the spawn failure, as a disconnected channel) is observed by
/// `StudioBootstrapApp` through the returned receiver.
pub fn spawn_open_thread(
    request: &StudioOpenRequest,
    fallback_user_settings: serde_json::Value,
) -> Receiver<anyhow::Result<ProjectData>> {
    let (tx, rx) = mpsc::channel();
    let job = match request {
        StudioOpenRequest::Project(project_dir) => LoadJob::Project(project_dir.clone()),
        StudioOpenRequest::Image { source, scratch } => LoadJob::Image { source: source.clone(), scratch: Arc::clone(scratch) },
    };
    let spawn_result = thread::Builder::new()
        .name("studio-project-load".to_string())
        .spawn(move || {
            // Reloads must observe settings changed during the current studio session. If the
            // config became unreadable, retain the last known-good startup snapshot and log the
            // degradation instead of making an otherwise recoverable project reload fail.
            let user_settings = match crate::config::load_user_settings_for_startup() {
                Ok(settings) => settings,
                Err(err) => {
                    runtime_log::log_error(format!(
                        "[studio-bootstrap] failed to refresh user_config.json before project load; using last known settings; error={err}"
                    ));
                    fallback_user_settings
                }
            };
            let result = match job {
                LoadJob::Project(project_dir) => load_project_dir(&project_dir, &user_settings),
                LoadJob::Image { source, scratch } => {
                    let result = crate::project::single_image::open_single_image(&source, &scratch, &user_settings);
                    // Give the scratch back BEFORE reporting: once the shell has the result the
                    // window may close, and `StudioOpenRequest::release` needs sole ownership.
                    drop(scratch);
                    result.map_err(|err| {
                        runtime_log::log_error(format!("[studio-bootstrap] failed to open single image: {err}"));
                        anyhow::Error::msg(err.user_message())
                    })
                }
            };
            if tx.send(result).is_err() {
                runtime_log::log_info("[studio-bootstrap] studio window closed before the load finished; result dropped");
            }
        });
    if let Err(err) = spawn_result {
        // The sender is dropped here, so the UI observes `Disconnected` and shows the
        // error screen instead of spinning forever.
        runtime_log::log_error(format!(
            "[studio-bootstrap] failed to spawn studio-project-load thread: {err}"
        ));
    }
    rx
}

/// The load worker's owned copy of a [`StudioOpenRequest`].
enum LoadJob {
    Project(PathBuf),
    Image { source: PathBuf, scratch: Arc<SingleImageScratch> },
}

/// Loads a chapter directory: `load_resume_unsaved` when a `{chapter}_unsaved` folder exists
/// next to it, else `load`. Blocking; load worker only.
fn load_project_dir(project_dir: &Path, user_settings: &serde_json::Value) -> anyhow::Result<ProjectData> {
    let resume_unsaved = crate::detect_unsaved_for_project(project_dir);
    if resume_unsaved {
        ProjectData::load_resume_unsaved(project_dir, user_settings)
    } else {
        ProjectData::load(project_dir, user_settings)
    }
    // Same greppable wording as the old startup-path context, now attached where
    // the load actually runs.
    .with_context(|| format!("failed to load project at {}", project_dir.display()))
}

/// Lifecycle of the studio window shell.
enum BootstrapState {
    /// Waiting on the load worker; the loading screen polls `rx` every frame.
    Loading {
        rx: Receiver<anyhow::Result<ProjectData>>,
    },
    /// The load failed (already logged); the error screen shows `error_text`.
    Failed { error_text: String },
    /// The project arrived; every frame is delegated to the real app.
    Running(Box<MangaApp>),
    /// The user asked to close during `Loading` and the worker has since delivered its
    /// (discarded) result; the window may now close for real.
    ClosingDiscarded,
}

/// `eframe::App` wrapper that owns the studio window from the first frame and swaps in
/// `MangaApp` once the background load completes.
pub struct StudioBootstrapApp {
    state: BootstrapState,
    ai_backend: AiBackendHandle,
    /// Retained for a safe project reload after a structural page operation.
    user_settings: serde_json::Value,
    return_to_launcher_flag: Arc<AtomicBool>,
    /// Set when the user tried to close the window during `Loading`; the close is deferred
    /// until the load worker delivers its result (see the file-header corruption note).
    close_after_load: bool,
    /// Tab restored after a structural-operation reload; selection intentionally does not persist.
    reload_tab: Option<AppTab>,
    /// The window edits one image through a scratch chapter (`StudioOpenRequest::Image`). Such
    /// a session never reloads from disk: the reload path would re-run a PROJECT load on the
    /// scratch and lose the session kind.
    single_image: bool,
    /// Observes this window's monitor/position/size and persists them (`Window` config
    /// section). Lives here rather than in `MangaApp` because the shell owns the window from
    /// the first frame, including the loading and error screens.
    geometry: WindowGeometryTracker,
    /// Windows-only workaround mirrored from `MangaApp`: `with_maximized` is skipped in the
    /// viewport builder there, so the shell maximizes the root window on its first frame
    /// (otherwise the loading screen shows in the unmaximized 1400x900 window). Now gated on
    /// the persisted state: a window left un-maximized must not be maximized again on Windows.
    #[cfg(target_os = "windows")]
    maximize_root_window_on_first_frame: bool,
    /// The window's egui context, kept for `on_exit`: the `egui-shader-layers` glow backend lives
    /// in it (installed by `install_shader_layers` at window creation) and must be destroyed with
    /// the GL context eframe hands `on_exit`, whichever state the shell is in.
    egui_ctx: egui::Context,
}

/// Installs the `egui-shader-layers` glow backend on the studio window's egui context.
///
/// Called once from the eframe app creator, the only place the `glow::Context` is available
/// before the first frame. Failure never aborts startup: the library registers a failed backend,
/// the PS editor's «Коррекция» panel then reports the correction as unavailable, and the cause is
/// logged here with the GL context's version for diagnosis. The matching teardown is
/// `StudioBootstrapApp::on_exit`.
pub fn install_shader_layers(cc: &eframe::CreationContext<'_>) {
    let Some(gl) = cc.gl.as_deref() else {
        runtime_log::log_error(
            "[studio-bootstrap] eframe created the studio window without a glow context; shader \
             layers (the PS editor's «Коррекция») are unavailable. Possible cause: a non-glow renderer",
        );
        return;
    };
    if let Err(error) = egui_shader_layers::install_glow(&cc.egui_ctx, gl) {
        use eframe::glow::HasContext as _;
        let version = gl.version();
        runtime_log::log_error(format!(
            "[studio-bootstrap] shader layers disabled: {error}. GL context: {}.{} (embedded: {}). \
             The PS editor's «Коррекция» panel reports the correction as unavailable",
            version.major, version.minor, version.is_embedded
        ));
    }
}

impl StudioBootstrapApp {
    /// `single_image` must be `StudioOpenRequest::is_single_image` of the request whose load
    /// `rx` delivers.
    pub fn new(
        rx: Receiver<anyhow::Result<ProjectData>>,
        single_image: bool,
        user_settings: serde_json::Value,
        ai_backend: AiBackendHandle,
        return_to_launcher_flag: Arc<AtomicBool>,
        egui_ctx: egui::Context,
    ) -> Self {
        // Read from disk, not from `user_settings`: that snapshot is taken once per session in
        // `run_main` and reused for every window, so a monitor chosen in a previous window of
        // the same session would be invisible here.
        let window_settings = window_geometry::load_window_settings();
        #[cfg(target_os = "windows")]
        let maximize_root_window_on_first_frame = window_settings.maximized.unwrap_or(true);
        Self {
            state: BootstrapState::Loading { rx },
            ai_backend,
            user_settings,
            return_to_launcher_flag,
            close_after_load: false,
            reload_tab: None,
            single_image,
            geometry: WindowGeometryTracker::new(&window_settings),
            #[cfg(target_os = "windows")]
            maximize_root_window_on_first_frame,
            egui_ctx,
        }
    }

    /// Drains the load worker's channel and advances the state machine. No-op unless
    /// currently `Loading`.
    fn poll_load_result(&mut self) {
        let BootstrapState::Loading { rx } = &self.state else {
            return;
        };
        let next = match rx.try_recv() {
            Ok(Ok(project)) => {
                if self.close_after_load {
                    // The user already closed the window; the worker only had to finish its
                    // filesystem writes. Constructing `MangaApp` would spawn loader threads
                    // for nothing, so the loaded project is discarded.
                    runtime_log::log_info(
                        "[studio-bootstrap] window closed during load; discarding loaded project",
                    );
                    BootstrapState::ClosingDiscarded
                } else {
                    let mut app = MangaApp::new(
                        project,
                        self.ai_backend.clone(),
                        Arc::clone(&self.return_to_launcher_flag),
                    );
                    if let Some(tab) = self.reload_tab.take() {
                        app.set_active_tab(tab);
                    }
                    // A direct `--project` start deferred the storage-mode reconciliation
                    // (see `run_main`) until the project is loaded, so the conversion never
                    // competes with the load. No-op when nothing is pending or it already ran;
                    // progress and failures show in the General settings pane and the log, and an
                    // incomplete run raises the studio's top-bar notice
                    // (`MangaApp::draw_storage_reconcile_notice`).
                    if let Some(job) = crate::storage_mode_job::start_pending_reconciliation() {
                        runtime_log::log_info(format!("[studio-bootstrap] started the deferred storage-mode reconciliation (job {job})"));
                    }
                    BootstrapState::Running(Box::new(app))
                }
            }
            Ok(Err(err)) => {
                // `{err:#}` prints the whole anyhow chain, including the greppable
                // "failed to load project at …" context added by the worker.
                let error_text = format!("{err:#}");
                runtime_log::log_error(format!("[studio-bootstrap] {error_text}"));
                if self.close_after_load {
                    BootstrapState::ClosingDiscarded
                } else {
                    BootstrapState::Failed { error_text }
                }
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                // Worker panicked before sending, or the spawn itself failed (logged in
                // `spawn_open_thread`). A dead worker holds no file handles, so a
                // deferred close needs no further waiting either.
                runtime_log::log_error(
                    "[studio-bootstrap] project load thread exited without a result",
                );
                if self.close_after_load {
                    BootstrapState::ClosingDiscarded
                } else {
                    BootstrapState::Failed {
                        error_text: t!("studio_bootstrap.load_thread_exited_error").to_string(),
                    }
                }
            }
        };
        self.state = next;
    }

    /// Centered spinner + status label while the load worker runs. With `closing` the label
    /// explains that the pending close waits for file operations to finish.
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

    /// Error screen: the failure text plus "Exit to launcher" / "Exit" buttons. Both close
    /// the window; the launcher button additionally raises `return_to_launcher_flag`, which
    /// `run_main_window` translates into `RunResult::ReturnToLauncher` as usual.
    fn draw_error_screen(
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        error_text: &str,
        return_to_launcher_flag: &AtomicBool,
    ) {
        egui::CentralPanel::default().show(ui, |ui| {
            let offset = (ui.available_height() * 0.5 - 90.0).max(0.0);
            ui.add_space(offset);
            ui.vertical_centered(|ui| {
                ui.heading(t!("studio_bootstrap.load_failed"));
                ui.add_space(8.0);
                ui.colored_label(ms_theme::status::ERROR, error_text);
                ui.add_space(14.0);
                if ui
                    .add_sized(
                        [280.0, 34.0],
                        egui::Button::new(t!("studio_bootstrap.back_to_launcher_button")),
                    )
                    .clicked()
                {
                    return_to_launcher_flag.store(true, AtomicOrdering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.add_space(6.0);
                if ui
                    .add_sized(
                        [280.0, 34.0],
                        egui::Button::new(t!("studio_bootstrap.exit_button")),
                    )
                    .clicked()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

impl eframe::App for StudioBootstrapApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // egui 0.35: `App::ui` receives the window-root `Ui`. Keep a borrowed `Context`
        // handle for the context-level calls (viewport commands, repaint scheduling) below.
        let ctx = ui.ctx().clone();
        // The winit handle is cloned out of `frame` before the borrow below: `MangaApp::ui`
        // takes `frame` mutably, and a live `&Window` borrowed from it would conflict.
        let window = frame.winit_window().cloned();
        self.geometry.observe(&ctx, window.as_deref());
        #[cfg(target_os = "windows")]
        if self.maximize_root_window_on_first_frame {
            self.maximize_root_window_on_first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
        }
        self.poll_load_result();
        let reload = match &mut self.state {
            BootstrapState::Running(app) => {
                app.ui(ui, frame);
                if !app.take_project_reload_request() {
                    None
                } else if self.single_image {
                    // Unreachable by design: the reload follows a structural page operation and
                    // the Page Manager is hidden in single-image mode. Refuse instead of
                    // re-loading the scratch as a project; the session keeps running.
                    runtime_log::log_error(
                        "[studio-bootstrap] project reload requested in a single-image session; refused, the session keeps running",
                    );
                    None
                } else {
                    let project_dir = app.project_dir();
                    app.on_exit(None);
                    Some(project_dir)
                }
            }
            BootstrapState::Loading { .. } => {
                // Never let the OS close kill the load worker mid-write (chapter corruption
                // risk — see the file header): cancel the close and defer it until the
                // worker delivers its result.
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.close_after_load = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                Self::draw_loading_screen(ui, self.close_after_load);
                // Poll the worker at ~10 Hz; nothing else triggers repaints while loading.
                ctx.request_repaint_after(LOAD_POLL_INTERVAL);
                None
            }
            BootstrapState::Failed { error_text } => {
                Self::draw_error_screen(ui, &ctx, error_text, &self.return_to_launcher_flag);
                None
            }
            BootstrapState::ClosingDiscarded => {
                // The deferred close can proceed now; re-sending `Close` every frame until
                // the window actually closes is harmless.
                Self::draw_loading_screen(ui, true);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                None
            }
        };
        if let Some(project_dir) = reload {
            self.reload_tab = Some(AppTab::PageManager);
            self.state = BootstrapState::Loading {
                rx: spawn_open_thread(&StudioOpenRequest::Project(project_dir), self.user_settings.clone()),
            };
            ctx.request_repaint();
        }
    }

    /// Forwards eframe's shutdown hook to the real app once it exists: `MangaApp::on_exit`
    /// drains the background layer saver, and skipping it would lose queued layer writes.
    /// Before the project arrives there is nothing to drain.
    ///
    /// The window geometry is flushed here too: the writer thread coalesces samples behind a
    /// debounce, so a resize in the last moments before closing would otherwise be dropped.
    ///
    /// Last, the `egui-shader-layers` glow backend (the PS editor's «Коррекция» shader) is freed:
    /// this is the one shutdown hook eframe hands a `glow::Context`, and the backend was installed
    /// for the whole window, not for one `MangaApp`, so the shell owns its teardown.
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let BootstrapState::Running(app) = &mut self.state {
            app.on_exit(gl);
        }
        self.geometry.flush_and_join();
        match gl {
            Some(gl) => egui_shader_layers::destroy_glow(&self.egui_ctx, gl),
            None => runtime_log::log_warn(
                "[studio-bootstrap] no GL context at shutdown; the shader-layer GL objects are \
                 released together with the context itself",
            ),
        }
    }
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
        let request = StudioOpenRequest::from_target(StartupTarget::Project(dir.clone()), &base).expect("project maps");
        assert!(matches!(&request, StudioOpenRequest::Project(mapped) if *mapped == dir));
        assert!(!request.is_single_image());
        assert_eq!(request.window_title("1.2.3"), "ManhwaStudio v1.2.3 - /home/u/titles/Title/Chapter 1");
        assert_eq!(request.font_roots(), vec![PathBuf::from("/home/u/titles/Title"), dir]);
        assert!(!base.exists(), "a project start must not create the scratch base");
        request.release();
    }

    #[test]
    fn image_target_reserves_a_scratch_and_release_deletes_it() {
        let base = temp_base("image");
        let source = PathBuf::from("/home/u/Картинки/page one.jpg");
        let request = StudioOpenRequest::from_target(StartupTarget::Image(source.clone()), &base).expect("scratch reserved");
        let root = match &request {
            StudioOpenRequest::Image { source: mapped, scratch } => {
                assert_eq!(*mapped, source);
                scratch.root().to_path_buf()
            }
            StudioOpenRequest::Project(dir) => panic!("image mapped to project {}", dir.display()),
        };
        assert!(root.starts_with(&base) && root.is_dir(), "scratch root {} must live under the base", root.display());
        assert!(request.is_single_image());
        assert_eq!(request.window_title("1.2.3"), "ManhwaStudio v1.2.3 - page one.jpg");
        assert!(request.font_roots().is_empty(), "an image never probes title-local fonts");
        request.release();
        assert!(!root.exists(), "release must delete the scratch session");
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn release_while_the_worker_still_shares_the_scratch_leaves_it_for_the_sweep() {
        let base = temp_base("shared");
        let request = StudioOpenRequest::from_target(StartupTarget::Image(PathBuf::from("/home/u/a.png")), &base).expect("scratch reserved");
        let worker_share = match &request {
            StudioOpenRequest::Image { scratch, .. } => Arc::clone(scratch),
            StudioOpenRequest::Project(dir) => panic!("image mapped to project {}", dir.display()),
        };
        let root = worker_share.root().to_path_buf();
        request.release();
        assert!(root.is_dir(), "a scratch still held elsewhere must not be deleted under its holder");
        // The last holder dropping the scratch releases the lock without I/O; the startup sweep
        // then owns the cleanup.
        drop(worker_share);
        let report = crate::project::single_image::sweep_stale_sessions(&base);
        assert_eq!(report.removed, 1);
        assert!(!root.exists());
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }
}
