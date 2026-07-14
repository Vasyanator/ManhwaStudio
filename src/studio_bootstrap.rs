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
- `spawn_project_load_thread`: named worker that mirrors the previous synchronous startup
  sequence (`detect_unsaved_for_project` choosing `load_resume_unsaved` vs `load`).

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
`MangaApp::new` does not need `eframe::CreationContext`, which is what makes late
construction inside a frame possible. Native-only: compiled together with the native
windowed startup flow (`run_main_window`), gated off wasm at the module declaration.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::app::MangaApp;
use crate::project::ProjectData;
use crate::runtime_log;
use anyhow::Context;
use ms_thread as thread;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// How often the loading screen polls the load worker's receiver (the frame only repaints
/// on input otherwise).
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Spawns the named background worker that loads the project for the studio window.
///
/// Mirrors the previous synchronous startup path byte-for-byte: detect a
/// `{chapter}_unsaved` folder next to the chapter, then pick `load_resume_unsaved` vs
/// `load` accordingly. The result (or the spawn failure, as a disconnected channel) is
/// observed by `StudioBootstrapApp` through the returned receiver.
pub fn spawn_project_load_thread(
    project_dir: PathBuf,
    user_settings: serde_json::Value,
) -> Receiver<anyhow::Result<ProjectData>> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new()
        .name("studio-project-load".to_string())
        .spawn(move || {
            let resume_unsaved = crate::detect_unsaved_for_project(&project_dir);
            let result = if resume_unsaved {
                ProjectData::load_resume_unsaved(&project_dir, &user_settings)
            } else {
                ProjectData::load(&project_dir, &user_settings)
            }
            // Same greppable wording as the old startup-path context, now attached where
            // the load actually runs.
            .with_context(|| format!("failed to load project at {}", project_dir.display()));
            let _ = tx.send(result);
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
    return_to_launcher_flag: Arc<AtomicBool>,
    /// Set when the user tried to close the window during `Loading`; the close is deferred
    /// until the load worker delivers its result (see the file-header corruption note).
    close_after_load: bool,
    /// Windows-only workaround mirrored from `MangaApp`: `with_maximized` is skipped in the
    /// viewport builder there, so the shell maximizes the root window on its first frame
    /// (otherwise the loading screen shows in the unmaximized 1400x900 window).
    #[cfg(target_os = "windows")]
    maximize_root_window_on_first_frame: bool,
}

impl StudioBootstrapApp {
    pub fn new(
        rx: Receiver<anyhow::Result<ProjectData>>,
        ai_backend: AiBackendHandle,
        return_to_launcher_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            state: BootstrapState::Loading { rx },
            ai_backend,
            return_to_launcher_flag,
            close_after_load: false,
            #[cfg(target_os = "windows")]
            maximize_root_window_on_first_frame: true,
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
                    BootstrapState::Running(Box::new(MangaApp::new(
                        project,
                        self.ai_backend.clone(),
                        Arc::clone(&self.return_to_launcher_flag),
                    )))
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
                // `spawn_project_load_thread`). A dead worker holds no file handles, so a
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
                ui.colored_label(egui::Color32::from_rgb(230, 120, 120), error_text);
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
        #[cfg(target_os = "windows")]
        if self.maximize_root_window_on_first_frame {
            self.maximize_root_window_on_first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
        }
        self.poll_load_result();
        match &mut self.state {
            BootstrapState::Running(app) => app.ui(ui, frame),
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
            }
            BootstrapState::Failed { error_text } => {
                Self::draw_error_screen(ui, &ctx, error_text, &self.return_to_launcher_flag);
            }
            BootstrapState::ClosingDiscarded => {
                // The deferred close can proceed now; re-sending `Close` every frame until
                // the window actually closes is harmless.
                Self::draw_loading_screen(ui, true);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Forwards eframe's shutdown hook to the real app once it exists: `MangaApp::on_exit`
    /// drains the background layer saver, and skipping it would lose queued layer writes.
    /// Before the project arrives there is nothing to drain.
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let BootstrapState::Running(app) = &mut self.state {
            app.on_exit(gl);
        }
    }
}
