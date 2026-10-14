/*
File: src/shell/mod.rs

Purpose:
The main window of the program: ONE `eframe::run_native` that hosts the launcher and the studio
as screens and is never closed while switching between them. The shell owns everything about
the window — geometry and its persistence, icon, app id, title, minimum size, UI fonts, UI
scale, the GL shader-layer backend, egui `Options`, the global style and the root close — and
the screens are plain state machines that report typed exits (`LauncherApp::take_outcome`,
`StudioExit`). Where the window goes next is decided by the pure table in `screen.rs`.

Key structures:
- `ShellConfig` / `InitialScreen`: what `run_main` hands the window (startup prompts already ran).
- `AppShell`: the `eframe::App`; `Active` holds the current screen's data next to its pure
  `ScreenKind`.
- `ShellExit` (from `screen.rs`): what `run_main` does after the window closed.

Key functions:
- `run(config)`: builds the viewport from the stored main-window geometry and runs the window.
- `AppShell::logic`: polls every worker (launcher entry, studio load, studio teardown, update
  check, incoming-images take) and owns the root close decision, so a minimized window still
  progresses.
- `AppShell::ui`: geometry, the Windows first-frame maximize, the active screen, UI exits.
- `AppShell::raw_input_hook`: feeds every frame's raw input to the canvas `TouchRouter`.

Notes:
No blocking work on the GUI thread at any switch: the launcher entry is prepared on a worker
(PreparingLauncher screen), the studio loads on a worker (loading screen) and is torn down on a
worker (Closing screen). Every switch rebuilds the style from scratch (`style.rs`), sets the
title and minimum size, and re-syncs the secondary-window policy (Android). Images another app
hands over (Android Share / "Open with") are taken on a worker when `ms_platform::incoming`
raises its flag and dispatched as `ShellEvent::ExternalOpen`; a target the table queues or asks
about waits in `AppShell::pending_open` (`incoming.rs` moves the data). `on_exit` is the
process-shutdown path only (eframe ended without the shell's consent, e.g. the Android Activity
went away): it tears a running studio down inline. Finger input on canvases is routed here, in
`raw_input_hook`, by ONE `ms_widgets::touch_nav::TouchRouter` per window (enabled by
`PlatformFeature::CanvasTouchGestures`), before egui builds its pointer state. Native only,
except `style`, which the web entry uses too.
*/

#[cfg(not(target_arch = "wasm32"))]
mod closing;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod incoming;
#[cfg(not(target_arch = "wasm32"))]
mod screen;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod studio;
pub(crate) mod style;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{InitialScreen, ShellConfig, run};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use screen::ShellExit;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::closing::{self, ClosingScreen};
    use super::incoming::{self, IncomingTake};
    use super::screen::{self, Effect, ScreenKind, ShellEvent, ShellExit, StudioPhase};
    use super::studio::{LoadResult, StartupTarget, StudioExit, StudioScreen};
    use super::style;
    use crate::ai_backend_supervisor::AiBackendHandle;
    use crate::app::PendingCloseAction;
    use crate::launcher::app::{LauncherApp, LauncherEntry};
    use crate::launcher::state::{LauncherHost, UpdateNotification};
    use crate::project::ProjectData;
    use crate::runtime_log;
    use crate::window_geometry::{self, WindowGeometryTracker};
    use ms_platform::PlatformFeature;
    use ms_thread as thread;
    use ms_widgets::touch_nav::TouchRouter;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, TryRecvError};
    use std::time::Duration;

    /// Inner size of the main window on a first run, in logical pixels. Also the size the startup
    /// placement centers on a monitor when no geometry has been stored yet.
    const MAIN_WINDOW_DEFAULT_INNER_SIZE: [f32; 2] = [1400.0, 900.0];
    /// Window title of the launcher screen (the product name; not a translatable UI string).
    const LAUNCHER_TITLE: &str = "ManhwaStudio";
    /// Window title of the launcher screen in a `--test-launcher` run.
    const TEST_LAUNCHER_TITLE: &str = "ManhwaStudio - Rust Launcher Test";
    /// eframe's application name (its persistence namespace; the visible title is set separately).
    const APP_NAME: &str = "ManhwaStudio";
    /// How often a waiting screen polls its worker when nothing else repaints.
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    /// How often the pending update check is polled when nothing else repaints (a network
    /// request: a slower cadence is enough to show its notice without input).
    const UPDATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// The first screen of the window.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum InitialScreen {
        /// The launcher (prepared on a worker first).
        Launcher,
        /// The studio, opening a CLI target (or the images that started the process) directly.
        Studio(StartupTarget),
    }

    /// What `run_main` hands the main window.
    pub(crate) struct ShellConfig {
        pub initial: InitialScreen,
        /// The startup user-settings snapshot: the fallback when a worker cannot re-read them.
        pub user_settings: serde_json::Value,
        pub ai_backend: AiBackendHandle,
        /// Process facts for the launcher (`--ignore-installed`, version core).
        pub host: LauncherHost,
        /// `--test-launcher`: test title and app id; any launcher exit quits.
        pub test_mode: bool,
        /// `Some(force)`: run the startup update check (once, on the first launcher entry);
        /// `force` is `--test-ver-check`. `None`: no check (source checkout, no installed-copy
        /// management).
        pub update_check: Option<bool>,
        /// A localized error to show once on the first screen (the cold-start take of incoming
        /// images failed; the details are logged).
        pub incoming_error: Option<String>,
    }

    /// Runs the main window until it closes and returns what `run_main` must do next. The only
    /// product `run_native` of the launcher and the studio; the separate prompt windows
    /// (installer, missing environment, update) run before or after it.
    ///
    /// # Errors
    /// eframe could not create or run the window.
    pub(crate) fn run(config: ShellConfig) -> anyhow::Result<ShellExit> {
        // Startup geometry from the `Window` config section, which describes the main window.
        // Monitors cannot be enumerated before the window exists (see `window_geometry`'s header),
        // so the placement comes from the stored monitor rect; the live list is reconciled once
        // the window is up. Read here, before the window, never in a frame.
        let window_settings = window_geometry::load_window_settings();
        let placement = window_geometry::plan_startup_placement(&window_settings, MAIN_WINDOW_DEFAULT_INNER_SIZE);
        let initial_kind = initial_kind(&config.initial);
        let title = match &config.initial {
            InitialScreen::Launcher => launcher_title(config.test_mode).to_string(),
            InitialScreen::Studio(target) => target.window_title(env!("MS_APP_VERSION")),
        };
        let min_size = screen::min_inner_size(&initial_kind).unwrap_or(screen::STUDIO_MIN_INNER_SIZE);
        let mut viewport = window_geometry::identity::with_identity(
            egui::ViewportBuilder::default().with_title(title.as_str()).with_inner_size(MAIN_WINDOW_DEFAULT_INNER_SIZE).with_min_inner_size(min_size),
            config.test_mode,
        );
        viewport = window_geometry::apply_placement(viewport, &placement);
        // Windows keeps the historical workaround: `with_maximized` in the builder leaves the
        // window unmaximized there, so the shell sends the command on its first frame instead.
        #[cfg(not(target_os = "windows"))]
        {
            viewport = viewport.with_maximized(placement.maximized);
        }
        let native_options = eframe::NativeOptions {
            viewport,
            // winit cannot build an event loop on Android without the Activity handle.
            #[cfg(target_os = "android")]
            android_app: window_geometry::android_app::current(),
            ..Default::default()
        };
        // eframe reads the forked winit's secondary-window policy when it creates the event loop;
        // the live setting was seeded by `run_main`. The shell re-syncs it on every switch.
        #[cfg(target_os = "android")]
        window_geometry::android_app::set_secondary_window_policy(ms_config::secondary_windows::current());

        let exit = Rc::new(Cell::new(ShellExit::Quit));
        let exit_for_window = Rc::clone(&exit);
        runtime_log::log_info(format!("[shell] opening the main window on {initial_kind:?}{}", if config.test_mode { " (test mode)" } else { "" }));
        eframe::run_native(APP_NAME, native_options, Box::new(move |cc| Ok(Box::new(AppShell::new(cc, config, &window_settings, exit_for_window)))))
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(exit.get())
    }

    /// The screen a window starts on.
    fn initial_kind(initial: &InitialScreen) -> ScreenKind {
        match initial {
            InitialScreen::Launcher => ScreenKind::PreparingLauncher,
            InitialScreen::Studio(_) => ScreenKind::Studio(StudioPhase::Loading { close_after_load: false }),
        }
    }

    /// The launcher screen's window title.
    fn launcher_title(test_mode: bool) -> &'static str {
        if test_mode { TEST_LAUNCHER_TITLE } else { LAUNCHER_TITLE }
    }

    /// The launcher-entry worker of the PreparingLauncher screen.
    struct LauncherPrepare {
        /// `None` once the worker died without an entry (the screen then offers a retry).
        rx: Option<Receiver<LauncherEntry>>,
    }

    /// Data an event brings along for its effect (the large ones boxed: they only move through).
    enum Payload {
        None,
        Entry(Box<LauncherEntry>),
        Project(Box<ProjectData>),
        LoadError(String),
    }

    /// The data of the active screen (its pure counterpart is `AppShell::kind`).
    enum Active {
        PreparingLauncher(LauncherPrepare),
        Launcher(Box<LauncherApp>),
        Studio(Box<StudioScreen>),
        Closing(ClosingScreen),
        /// Exiting: the shell sent the root `Close`; nothing is left to draw.
        Gone,
    }

    /// The main window's `eframe::App`.
    struct AppShell {
        kind: ScreenKind,
        active: Active,
        /// Startup user-settings snapshot (worker fallback).
        user_settings: serde_json::Value,
        ai_backend: AiBackendHandle,
        host: LauncherHost,
        test_mode: bool,
        /// `Some(force)` until the update check is started (once per process).
        update_check: Option<bool>,
        /// The running update check.
        update_rx: Option<Receiver<Option<UpdateNotification>>>,
        /// The update check's answer, handed to every launcher entry.
        update_notification: Option<UpdateNotification>,
        /// Title of the current studio target (set when the studio starts loading).
        studio_title: String,
        /// Observes the window's monitor/position/size and persists them (`Window` section).
        geometry: WindowGeometryTracker,
        /// Windows-only workaround: `with_maximized` is skipped in the viewport builder, so the
        /// shell maximizes the root window on its first frame when the stored state says so.
        #[cfg(target_os = "windows")]
        maximize_on_first_frame: bool,
        /// The window's context, kept for `on_exit` (the shader-layer backend lives in it).
        egui_ctx: egui::Context,
        /// Read by `run` after the window closed.
        exit: Rc<Cell<ShellExit>>,
        /// An externally delivered target waiting for its screen: queued (PreparingLauncher,
        /// loading studio) or asked about (running studio). Folded into every later
        /// `ExternalOpen`; dropped (inbox batches released) when the studio closes for another
        /// reason, the user declines it, or the window exits.
        pending_open: Option<StartupTarget>,
        /// The running incoming-images take worker.
        incoming_take: Option<IncomingTake>,
        /// The take worker could not be spawned; polling stops (logged once) and the received
        /// images stay in the inbox for the next start.
        incoming_take_unavailable: bool,
        /// A localized incoming-images error shown in a small window until dismissed.
        incoming_error: Option<String>,
        /// The canvas finger router, fed by `raw_input_hook`. One per window (its claims span
        /// frames and every screen); disabled where `CanvasTouchGestures` is unavailable, where
        /// it only drops the per-frame surface publications.
        touch_router: TouchRouter,
    }

    impl AppShell {
        /// Creator: everything that belongs to the window for its whole life, then the initial
        /// screen. Runs before the first frame; no blocking I/O (the geometry settings were read
        /// by `run`).
        fn new(cc: &eframe::CreationContext<'_>, config: ShellConfig, window_settings: &window_geometry::WindowSettings, exit: Rc<Cell<ShellExit>>) -> Self {
            let ctx = &cc.egui_ctx;
            // First point in the process where monitors can be listed at all (a second winit
            // `EventLoop` cannot exist). Publishes them for the settings UI and refreshes the
            // "largest monitor" fallback the NEXT start places on.
            window_geometry::refresh_monitors(cc.winit_window().map(std::sync::Arc::as_ref));
            // One full font stack per process: this is the process's only product context.
            crate::ui_fonts::install(ctx, crate::ui_fonts::Tier::Full);
            // Global interface scale (`General.ui_scale_percent`), per `Context`.
            crate::general_settings_panel::apply_ui_scale_to_context(ctx);
            // GPU shader layers (the PS editor's «Коррекция»): needs `cc.gl`; destroyed in `on_exit`.
            super::studio::install_shader_layers(cc);
            // The only UI zoom is the interface-scale setting.
            ctx.options_mut(|options| options.zoom_with_keyboard = false);
            let ShellConfig { initial, user_settings, ai_backend, host, test_mode, update_check, incoming_error } = config;
            // The platform wake-up repaints the window, so `logic` sees `has_pending` at once.
            let waker_ctx = ctx.clone();
            ms_platform::incoming::set_waker(Arc::new(move || waker_ctx.request_repaint()));
            let mut shell = Self {
                kind: initial_kind(&initial),
                active: Active::Gone,
                user_settings,
                ai_backend,
                host,
                test_mode,
                update_check,
                update_rx: None,
                update_notification: None,
                studio_title: String::new(),
                geometry: WindowGeometryTracker::new(window_settings),
                #[cfg(target_os = "windows")]
                maximize_on_first_frame: window_settings.maximized.unwrap_or(true),
                egui_ctx: ctx.clone(),
                exit,
                pending_open: None,
                incoming_take: None,
                incoming_take_unavailable: false,
                incoming_error,
                touch_router: TouchRouter::new(PlatformFeature::CanvasTouchGestures.is_available()),
            };
            let effect = match initial {
                InitialScreen::Launcher => Effect::PrepareLauncher,
                InitialScreen::Studio(target) => Effect::LoadStudio { target, tab: None },
            };
            shell.run_effect(ctx, effect, Payload::None);
            shell.enter_screen(ctx);
            shell
        }

        /// Feeds `event` through the pure table and performs the result: the OS-close verdict,
        /// the effect, and — when the screen changes — the per-screen window attributes. An event
        /// the table rejects is logged and ignored.
        fn dispatch(&mut self, ctx: &egui::Context, event: ShellEvent, payload: Payload) {
            let event = self.resolve_open_event(event);
            let Some(transition) = screen::on_event(&self.kind, event.clone()) else {
                match event {
                    ShellEvent::ExternalOpen(target) | ShellEvent::StudioCloseForOpen(target) => {
                        incoming::discard_target(target, &format!("the screen {:?} cannot take them", self.kind));
                    }
                    other => runtime_log::log_warn(format!("[shell] event {other:?} does not apply to screen {:?}; ignored", self.kind)),
                }
                return;
            };
            if transition.cancel_os_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            let switched = std::mem::discriminant(&self.kind) != std::mem::discriminant(&transition.next);
            if switched {
                runtime_log::log_info(format!("[shell] {:?} -> {:?} on {event:?}", self.kind, transition.next));
            }
            self.kind = transition.next;
            if let ScreenKind::Exiting { exit } = &self.kind {
                self.exit.set(*exit);
                if let Some(target) = self.pending_open.take() {
                    incoming::discard_target(target, "the window is closing");
                }
            }
            // A queued target is re-fed once the screen that can take it exists.
            let refeed = matches!(transition.effect, Effect::ShowLauncher | Effect::StartStudio);
            self.run_effect(ctx, transition.effect, payload);
            if switched {
                self.enter_screen(ctx);
            }
            if refeed && let Some(target) = self.pending_open.take() {
                runtime_log::log_info(format!("[shell] re-feeding {} queued received image(s) on {:?}", incoming::received_count(&target), self.kind));
                self.dispatch(ctx, ShellEvent::ExternalOpen(target), Payload::None);
            }
        }

        /// Shell-side bookkeeping of `pending_open` before an event reaches the table:
        /// - `ExternalOpen(t)` absorbs the target still pending (`screen::merge_open`), so one
        ///   delivery queue never splits;
        /// - the studio's `StudioClose(OpenIncoming)` becomes `StudioCloseForOpen` with the pending
        ///   target (none pending is a defect: logged, treated as a return to the launcher);
        /// - any other studio close drops the pending target (logged, inbox batches released).
        fn resolve_open_event(&mut self, event: ShellEvent) -> ShellEvent {
            match event {
                ShellEvent::ExternalOpen(target) => ShellEvent::ExternalOpen(screen::merge_open(self.pending_open.take(), target)),
                ShellEvent::StudioClose(PendingCloseAction::OpenIncoming) => match self.pending_open.take() {
                    Some(target) => ShellEvent::StudioCloseForOpen(target),
                    None => {
                        runtime_log::log_error("[shell] defect: the studio closed to open received images, but none is pending; returning to the launcher");
                        ShellEvent::StudioClose(PendingCloseAction::ReturnToLauncher)
                    }
                },
                ShellEvent::StudioClose(action @ (PendingCloseAction::Exit | PendingCloseAction::ReturnToLauncher)) => {
                    if let Some(target) = self.pending_open.take() {
                        incoming::discard_target(target, &format!("the studio closed for {action:?}"));
                    }
                    ShellEvent::StudioClose(action)
                }
                other @ (ShellEvent::LauncherPrepared
                | ShellEvent::LauncherExit(_)
                | ShellEvent::StudioLoaded
                | ShellEvent::StudioLoadFailed
                | ShellEvent::StudioReload { .. }
                | ShellEvent::FailedBack
                | ShellEvent::FailedExit
                | ShellEvent::ShutdownFinished
                | ShellEvent::OsClose
                | ShellEvent::StudioCloseForOpen(_)) => other,
            }
        }

        /// Performs `effect` with the data `payload` brought along.
        fn run_effect(&mut self, ctx: &egui::Context, effect: Effect, payload: Payload) {
            match effect {
                Effect::None => {}
                Effect::PrepareLauncher => {
                    if let Some(force) = self.update_check.take() {
                        self.update_rx = Some(crate::update_check::spawn(force));
                    }
                    if let Some(job) = crate::storage_mode_job::start_pending_reconciliation() {
                        runtime_log::log_info(format!("[shell] started the pending storage-mode reconciliation (job {job})"));
                    }
                    self.active = Active::PreparingLauncher(self.spawn_launcher_prepare());
                }
                Effect::ShowLauncher => match payload {
                    Payload::Entry(entry) => {
                        let mut launcher = LauncherApp::new(*entry);
                        launcher.set_update_notification(self.update_notification.clone());
                        self.active = Active::Launcher(Box::new(launcher));
                    }
                    Payload::None | Payload::Project(_) | Payload::LoadError(_) => runtime_log::log_error("[shell] the launcher screen was entered without its entry"),
                },
                Effect::LoadStudio { target, tab } => {
                    self.studio_title = target.window_title(env!("MS_APP_VERSION"));
                    self.active = Active::Studio(Box::new(StudioScreen::start(target, tab, self.user_settings.clone(), self.ai_backend.clone())));
                }
                // The table emits these two only for a studio that is loading, with the payload the
                // poll delivered; anything else is a shell bug, logged instead of acted on.
                Effect::StartStudio => {
                    if let (Active::Studio(studio), Payload::Project(project)) = (&mut self.active, payload) {
                        studio.start_running(project);
                    } else {
                        runtime_log::log_error("[shell] the studio was started without its loaded project");
                    }
                }
                Effect::ShowLoadError => {
                    if let (Active::Studio(studio), Payload::LoadError(error_text)) = (&mut self.active, payload) {
                        studio.fail(error_text);
                    } else {
                        runtime_log::log_error("[shell] the studio error screen was entered without its error");
                    }
                }
                Effect::Shutdown => {
                    // A project loaded after the window was asked to close arrives in `payload`
                    // and is discarded here: constructing `MangaApp` would spawn loaders for nothing.
                    drop(payload);
                    let (app, request) = match std::mem::replace(&mut self.active, Active::Gone) {
                        Active::Studio(studio) => studio.into_parts(),
                        // The table emits `Shutdown` only from a studio screen; anything else is a
                        // shell bug. The empty teardown then finishes at once and the window moves on.
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => {
                            runtime_log::log_error("[shell] studio teardown requested while no studio is active");
                            (None, None)
                        }
                    };
                    self.active = Active::Closing(ClosingScreen::start(app, request));
                }
                Effect::RequestOsClose => {
                    let close = match &mut self.active {
                        Active::Studio(studio) => {
                            studio.request_os_close();
                            studio.take_close_request()
                        }
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => None,
                    };
                    // The exit dialog opened after this frame's draw; show it without waiting.
                    ctx.request_repaint();
                    if let Some(action) = close {
                        self.dispatch(ctx, ShellEvent::StudioClose(action), Payload::None);
                    }
                }
                Effect::CloseWindow => {
                    self.active = Active::Gone;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Effect::QueueOpen(target) => {
                    runtime_log::log_info(format!("[shell] {} received image(s) queued until {:?} can take them", incoming::received_count(&target), self.kind));
                    self.pending_open = Some(target);
                }
                Effect::AskStudioToOpen(target) => {
                    let count = incoming::received_count(&target);
                    let asked = match &mut self.active {
                        Active::Studio(studio) => studio.request_close_for_incoming(count),
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => false,
                    };
                    if asked {
                        runtime_log::log_info(format!("[shell] asked the studio to close for {count} received image(s)"));
                        self.pending_open = Some(target);
                    } else {
                        incoming::discard_target(target, "the studio is already closing for another reason");
                    }
                    // The question opened after this frame's draw; show it without waiting.
                    ctx.request_repaint();
                }
            }
        }

        /// Per-screen window attributes, applied on every screen switch: the complete style built
        /// from scratch, the title, the minimum size and the secondary-window policy.
        fn enter_screen(&mut self, ctx: &egui::Context) {
            if let Some(style_screen) = screen::style_screen(&self.kind) {
                style::apply(ctx, style_screen);
            }
            let title = match &self.kind {
                ScreenKind::PreparingLauncher | ScreenKind::Launcher => Some(launcher_title(self.test_mode).to_string()),
                ScreenKind::Studio(_) => Some(self.studio_title.clone()),
                ScreenKind::Closing { .. } | ScreenKind::Exiting { .. } => None,
            };
            if let Some(title) = title {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
            }
            if let Some([width, height]) = screen::min_inner_size(&self.kind) {
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(width, height)));
            }
            window_geometry::sync_secondary_window_policy(ctx, ms_config::secondary_windows::current());
            ctx.request_repaint();
        }

        /// Starts the launcher-entry worker: the model folders, then `prepare_entry` (blocking
        /// config reads, scans and probes), never on the GUI thread.
        fn spawn_launcher_prepare(&self) -> LauncherPrepare {
            let (tx, rx) = mpsc::channel();
            let fallback = self.user_settings.clone();
            let ai_backend = self.ai_backend.clone();
            let host = self.host;
            let test_mode = self.test_mode;
            let spawn_result = thread::Builder::new().name("launcher-prepare".to_string()).spawn(move || {
                if let Err(err) = crate::config::ensure_model_dirs() {
                    runtime_log::log_warn(format!("[shell] could not create the AI model folders before the launcher: {err:#}"));
                }
                let entry = crate::launcher::prepare_entry(&fallback, ai_backend, host, test_mode);
                if tx.send(entry).is_err() {
                    runtime_log::log_info("[shell] the launcher entry arrived after the main window moved on; dropped");
                }
            });
            match spawn_result {
                Ok(handle) => {
                    // Detached: the worker reports through the channel and never needs a join.
                    drop(handle);
                    LauncherPrepare { rx: Some(rx) }
                }
                Err(err) => {
                    runtime_log::log_error(format!("[shell] could not start the launcher-prepare thread: {err}"));
                    LauncherPrepare { rx: None }
                }
            }
        }

        /// Polls the update check and hands its answer to the launcher (cached for later entries).
        /// While the check runs it keeps a slow repaint scheduled, so the notice appears without
        /// input.
        fn poll_update_check(&mut self, ctx: &egui::Context) {
            let Some(rx) = &self.update_rx else {
                return;
            };
            let notification = match rx.try_recv() {
                Ok(notification) => notification,
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(UPDATE_POLL_INTERVAL);
                    return;
                }
                Err(TryRecvError::Disconnected) => None,
            };
            self.update_rx = None;
            self.update_notification = notification;
            if let Active::Launcher(launcher) = &mut self.active {
                launcher.set_update_notification(self.update_notification.clone());
                ctx.request_repaint();
            }
        }

        /// Polls the active screen's worker and dispatches what it delivered.
        fn poll_workers(&mut self, ctx: &egui::Context) {
            match &mut self.active {
                Active::PreparingLauncher(prepare) => {
                    let Some(rx) = &prepare.rx else {
                        return;
                    };
                    match rx.try_recv() {
                        Ok(entry) => self.dispatch(ctx, ShellEvent::LauncherPrepared, Payload::Entry(Box::new(entry))),
                        Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL_INTERVAL),
                        Err(TryRecvError::Disconnected) => {
                            runtime_log::log_error("[shell] the launcher-prepare worker exited without an entry");
                            prepare.rx = None;
                        }
                    }
                }
                Active::Studio(studio) => {
                    // A deferred OS close stops an image-set open even while the window is
                    // minimized (`ui` is not called then).
                    if matches!(self.kind, ScreenKind::Studio(StudioPhase::Loading { close_after_load: true })) {
                        studio.cancel_for_close();
                    }
                    if studio.take_incoming_declined()
                        && let Some(target) = self.pending_open.take()
                    {
                        incoming::discard_target(target, "the user declined to close the studio for them");
                    }
                    if let Some(result) = studio.poll_load() {
                        match result {
                            LoadResult::Loaded(project) => self.dispatch(ctx, ShellEvent::StudioLoaded, Payload::Project(project)),
                            LoadResult::Failed(error_text) => self.dispatch(ctx, ShellEvent::StudioLoadFailed, Payload::LoadError(error_text)),
                        }
                    } else if let Some(action) = studio.take_close_request() {
                        self.dispatch(ctx, ShellEvent::StudioClose(action), Payload::None);
                    } else if matches!(self.kind, ScreenKind::Studio(StudioPhase::Loading { .. })) {
                        ctx.request_repaint_after(POLL_INTERVAL);
                    }
                }
                Active::Closing(closing) => {
                    if closing.poll(ctx) {
                        self.dispatch(ctx, ShellEvent::ShutdownFinished, Payload::None);
                    }
                }
                Active::Launcher(_) | Active::Gone => {}
            }
        }

        /// Takes images other apps handed over: starts the take worker when the platform raised
        /// its flag (an atomic read, no JNI here), and dispatches the worker's target as
        /// `ExternalOpen`. A failed take is logged and shown once in a small window.
        fn poll_incoming(&mut self, ctx: &egui::Context) {
            if let Some(take) = &self.incoming_take {
                let Some(result) = take.poll() else {
                    ctx.request_repaint_after(POLL_INTERVAL);
                    return;
                };
                self.incoming_take = None;
                match result {
                    Ok(Some(target)) => {
                        runtime_log::log_info(format!("[shell] received {} image(s) from another app on {:?}", incoming::received_count(&target), self.kind));
                        self.dispatch(ctx, ShellEvent::ExternalOpen(target), Payload::None);
                    }
                    Ok(None) => runtime_log::log_info("[shell] the incoming-images take found nothing to open"),
                    Err(err) => {
                        runtime_log::log_error(format!("[shell] {err}"));
                        self.incoming_error = Some(err.user_message().to_string());
                    }
                }
            }
            // A batch published while the worker ran raised the flag again: take it next.
            if self.incoming_take.is_none() && !self.incoming_take_unavailable && ms_platform::incoming::has_pending() {
                match incoming::spawn_take() {
                    Ok(take) => {
                        self.incoming_take = Some(take);
                        ctx.request_repaint_after(POLL_INTERVAL);
                    }
                    Err(err) => {
                        runtime_log::log_error(format!("[shell] could not start the incoming-images take worker: {err}; received images stay in the inbox until the next start"));
                        self.incoming_take_unavailable = true;
                    }
                }
            }
        }

        /// Draws the incoming-images error window while one is set; OK dismisses it.
        fn draw_incoming_error(&mut self, ctx: &egui::Context) {
            let Some(message) = &self.incoming_error else {
                return;
            };
            let mut dismissed = false;
            egui::Window::new(t!("shell.incoming.error_title"))
                .id(egui::Id::new("shell.incoming.error_title"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(message.as_str());
                    ui.add_space(8.0);
                    dismissed = ui.button(t!("shell.incoming.ok_button")).clicked();
                });
            if dismissed {
                self.incoming_error = None;
            }
        }

        /// Maps a studio exit to its shell event.
        fn studio_event(exit: StudioExit) -> ShellEvent {
            match exit {
                StudioExit::Close(action) => ShellEvent::StudioClose(action),
                StudioExit::Reload { project_dir } => ShellEvent::StudioReload { project_dir },
                StudioExit::FailedBack => ShellEvent::FailedBack,
                StudioExit::FailedExit => ShellEvent::FailedExit,
            }
        }
    }

    /// Spinner while the launcher entry is prepared; after a failed worker, the error and a retry.
    /// Returns `true` when Retry was pressed.
    fn draw_preparing_screen(ui: &mut egui::Ui, failed: bool) -> bool {
        let mut retry = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let offset = (ui.available_height() * 0.5 - 40.0).max(0.0);
            ui.add_space(offset);
            ui.vertical_centered(|ui| {
                if failed {
                    ui.colored_label(ms_theme::status::ERROR, t!("shell.preparing.failed_error"));
                    ui.add_space(10.0);
                    retry = ui.add_sized([220.0, 34.0], egui::Button::new(t!("shell.preparing.retry_button"))).clicked();
                } else {
                    ui.spinner();
                    ui.add_space(10.0);
                    ui.label(t!("shell.preparing.loading_label"));
                }
            });
        });
        retry
    }

    impl eframe::App for AppShell {
        /// Runs before every `ui` and also while the window is hidden: worker polling and the root
        /// close decision live here, so a minimized window still finishes a load or a teardown and
        /// still answers the close button (eframe honours a `CancelClose` sent here,
        /// eframe-0.36.2/src/native/epi_integration.rs:305-310, :340-346).
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll_update_check(ctx);
            self.poll_workers(ctx);
            self.poll_incoming(ctx);
            if ctx.input(|input| input.viewport().close_requested()) {
                self.dispatch(ctx, ShellEvent::OsClose, Payload::None);
            }
        }

        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            // `Ui::ctx` borrows the `Ui`; the clone (an `Arc`) detaches it for the screens below.
            let ctx = ui.ctx().clone();
            // Cloned out of `frame` first: the studio takes `frame` mutably below.
            let window = frame.winit_window().cloned();
            self.geometry.observe(&ctx, window.as_deref());
            #[cfg(target_os = "windows")]
            if self.maximize_on_first_frame {
                self.maximize_on_first_frame = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                ctx.request_repaint();
            }
            let mut retry_prepare = false;
            let event = match &mut self.active {
                Active::PreparingLauncher(prepare) => {
                    retry_prepare = draw_preparing_screen(ui, prepare.rx.is_none());
                    None
                }
                Active::Launcher(launcher) => {
                    launcher.show(ui);
                    launcher.take_outcome().map(|outcome| ShellEvent::LauncherExit(screen::launcher_exit_for(outcome, self.test_mode)))
                }
                Active::Studio(studio) => {
                    let closing_after_load = matches!(self.kind, ScreenKind::Studio(StudioPhase::Loading { close_after_load: true }));
                    studio.ui(ui, frame, closing_after_load).map(Self::studio_event)
                }
                Active::Closing(closing) => closing.ui(ui).then_some(ShellEvent::ShutdownFinished),
                Active::Gone => {
                    egui::CentralPanel::default().show(ui, |_ui| {});
                    None
                }
            };
            self.draw_incoming_error(&ctx);
            if retry_prepare {
                runtime_log::log_info("[shell] retrying the launcher preparation");
                self.active = Active::PreparingLauncher(self.spawn_launcher_prepare());
            }
            if let Some(event) = event {
                self.dispatch(&ctx, event, Payload::None);
            }
        }

        /// Routes canvas finger input before egui sees it (`ms_widgets::touch_nav`): claimed
        /// touches lose their mouse emulation and become navigation / long-press outputs that the
        /// canvases take this frame. Child viewports pass through untouched (the router checks).
        fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
            self.touch_router.route(ctx, raw_input);
        }

        /// The process-shutdown path (eframe ended the window): finishes or performs the studio
        /// teardown inline, flushes the window geometry and frees the shader-layer GL objects.
        fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
            match std::mem::replace(&mut self.active, Active::Gone) {
                Active::Studio(studio) => {
                    let (app, request) = studio.into_parts();
                    if app.is_some() || request.is_some() {
                        runtime_log::log_info("[shell] the main window ended with the studio open; tearing it down inline");
                    }
                    // The report is dropped on purpose: its items are logged by the teardown and
                    // no frame is left to show them.
                    drop(closing::teardown_blocking(app, request));
                }
                Active::Closing(closing) => closing.finish_blocking(),
                Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Gone => {}
            }
            // The writer thread coalesces samples behind a debounce: a resize in the last moments
            // before closing would otherwise be dropped.
            self.geometry.flush_and_join();
            match gl {
                Some(gl) => egui_shader_layers::destroy_glow(&self.egui_ctx, gl),
                None => runtime_log::log_warn("[shell] no GL context at shutdown; the shader-layer GL objects are released together with the context itself"),
            }
            if let ScreenKind::Exiting { exit } = &self.kind {
                self.exit.set(*exit);
            }
        }
    }
}
