/*
File: src/shell/mod.rs

Purpose:
The main window of the program: ONE `eframe::run_native` that hosts the launcher and the studio
as screens and is never closed while switching between them. The shell owns everything about
the window — geometry and its persistence, icon, app id, title, minimum size, UI fonts, UI
scale, the GL shader-layer backend, egui `Options`, the global style and the root close — and
the screens are plain state machines that report typed exits (`LauncherApp::take_outcome`,
`StudioExit`). Where the window goes next is decided by the pure table in `screen.rs`. Events
and effects carry no worker data: a delivered launcher entry, loaded project or load error waits
in the active screen until the effect the table picks consumes it.

Key structures:
- `ShellConfig` / `InitialScreen`: what `startup::run_native` hands the window (startup prompts already ran).
- `AppShell`: the `eframe::App`; `Active` holds the current screen's data next to its pure
  `ScreenKind`; `LauncherPrepare` is the PreparingLauncher screen's worker or delivered entry.
- `ShellExit` (from `screen.rs`): what `startup::run_native` does after the window closed.

Key functions:
- `run(config)`: builds the viewport from the stored main-window geometry and runs the window.
- `AppShell::logic`: polls every worker (launcher entry, studio load, studio teardown, update
  check, incoming-images take) and owns the root close decision, so a minimized window still
  progresses.
- `AppShell::ui`: geometry, the Windows first-frame maximize, the active screen, UI exits.
- `AppShell::drain_studio_outputs`: applies the running studio's one outbound queue
  (`crate::studio::StudioOutput`: title through `apply_title`, close, received-images decline,
  reload); called after the studio draw, from `poll_workers` and after the OS close hand-over.
- `AppShell::raw_input_hook`: feeds every frame's raw input to the canvas `TouchRouter`.
- `resolve_open_event(pending, event)`: the `pending_open` bookkeeping before the table.

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
`raw_input_hook`, by ONE `ms_widgets::touch_nav::TouchRouter` per window, before egui builds its
pointer state; it is on while `PlatformFeature::CanvasTouchGestures` is available AND the
`General.touch_gestures` setting is (`ms_config::touch_gestures`, re-read every frame). Native only,
except `style` and `chapter_load` (the one owner of the resume-unsaved decision), which the web
entry uses too.
*/

pub(crate) mod chapter_load;
#[cfg(not(target_arch = "wasm32"))]
mod closing;
#[cfg(not(target_arch = "wasm32"))]
mod gl;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod incoming;
#[cfg(not(target_arch = "wasm32"))]
mod screen;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod studio;
pub(crate) mod style;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod target;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{InitialScreen, ShellConfig, run};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use screen::ShellExit;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::closing::{self, ClosingScreen};
    use super::incoming::{self, IncomingTake};
    use super::screen::{self, Effect, ScreenKind, ShellEvent, ShellExit, StudioPhase};
    use super::studio::{LoadResult, StudioExit, StudioScreen};
    use crate::studio::StudioOutput;
    use super::style;
    use super::target::{StartupTarget, received_count};
    use crate::ai_backend_supervisor::AiBackendHandle;
    use crate::studio::PendingCloseAction;
    use crate::launcher::app::{LauncherApp, LauncherEntry};
    use crate::launcher::state::{LauncherHost, UpdateNotification};
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

    /// What `startup::run_native` hands the main window.
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

    /// Runs the main window until it closes and returns what `startup::run_native` must do next. The only
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
        // the live setting was seeded by `startup::run_native`. The shell re-syncs it on every switch.
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

    /// Whether canvas finger gestures run now: the platform capability AND the live
    /// `General.touch_gestures` setting (a relaxed atomic read, cheap enough for every frame).
    fn touch_gestures_enabled() -> bool {
        ms_config::touch_gestures::enabled_with(PlatformFeature::CanvasTouchGestures.is_available())
    }

    /// The launcher screen's window title.
    fn launcher_title(test_mode: bool) -> &'static str {
        if test_mode { TEST_LAUNCHER_TITLE } else { LAUNCHER_TITLE }
    }

    /// Shell-side bookkeeping of the pending open target (`AppShell::pending_open`) before an
    /// event reaches the table:
    /// - `ExternalOpen(t)` absorbs the target still pending (`screen::merge_open`), so one
    ///   delivery queue never splits;
    /// - the studio's `StudioClose(OpenIncoming)` becomes `StudioCloseForOpen` with the pending
    ///   target (none pending is a defect: logged, treated as a return to the launcher);
    /// - any other studio close drops the pending target (`incoming::discard_target`: logged,
    ///   inbox batches released on a worker).
    ///
    /// Every other event passes through unchanged and leaves `pending` alone.
    fn resolve_open_event(pending: &mut Option<StartupTarget>, event: ShellEvent) -> ShellEvent {
        match event {
            ShellEvent::ExternalOpen(target) => ShellEvent::ExternalOpen(screen::merge_open(pending.take(), target)),
            ShellEvent::StudioClose(PendingCloseAction::OpenIncoming) => match pending.take() {
                Some(target) => ShellEvent::StudioCloseForOpen(target),
                None => {
                    runtime_log::log_error("[shell] defect: the studio closed to open received images, but none is pending; returning to the launcher");
                    ShellEvent::StudioClose(PendingCloseAction::ReturnToLauncher)
                }
            },
            ShellEvent::StudioClose(action @ (PendingCloseAction::Exit | PendingCloseAction::ReturnToLauncher)) => {
                if let Some(target) = pending.take() {
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

    /// The PreparingLauncher screen's data: its launcher-entry worker, or what it delivered.
    /// The entry stays here until `run_effect(ShowLauncher)` takes it.
    enum LauncherPrepare {
        /// The worker runs; its entry arrives on the channel.
        Waiting(Receiver<LauncherEntry>),
        /// The worker died without an entry or could not be spawned; the screen offers a retry.
        WorkerLost,
        /// The entry arrived (boxed: large, it only moves through) and `LauncherPrepared` was
        /// dispatched.
        Ready(Box<LauncherEntry>),
    }

    impl LauncherPrepare {
        /// Takes the delivered entry; `None` (state kept) unless it is `Ready`.
        fn take_entry(&mut self) -> Option<Box<LauncherEntry>> {
            match std::mem::replace(self, Self::WorkerLost) {
                Self::Ready(entry) => Some(entry),
                other @ (Self::Waiting(_) | Self::WorkerLost) => {
                    *self = other;
                    None
                }
            }
        }
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
        /// frames and every screen); off where `CanvasTouchGestures` is unavailable or the
        /// user turned `General.touch_gestures` off, and then it only drops the per-frame
        /// surface publications. Toggled live, never recreated (`TouchRouter::set_enabled`).
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
            super::gl::install(cc);
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
                touch_router: TouchRouter::new(touch_gestures_enabled()),
            };
            let effect = match initial {
                InitialScreen::Launcher => Effect::PrepareLauncher,
                InitialScreen::Studio(target) => Effect::LoadStudio { target, tab: None },
            };
            shell.run_effect(ctx, effect);
            shell.enter_screen(ctx);
            shell
        }

        /// Feeds `event` through the pure table and performs the result: the OS-close verdict,
        /// the effect, and — when the screen changes — the per-screen window attributes. An event
        /// the table rejects is logged and ignored. Data an effect consumes (the prepared launcher
        /// entry, the loaded project, the load error) waits in the active screen that received it.
        fn dispatch(&mut self, ctx: &egui::Context, event: ShellEvent) {
            let event = resolve_open_event(&mut self.pending_open, event);
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
            self.run_effect(ctx, transition.effect);
            if switched {
                self.enter_screen(ctx);
            }
            if refeed && let Some(target) = self.pending_open.take() {
                runtime_log::log_info(format!("[shell] re-feeding {} queued received image(s) on {:?}", received_count(&target), self.kind));
                self.dispatch(ctx, ShellEvent::ExternalOpen(target));
            }
        }

        /// Performs `effect`, taking the data it consumes from the active screen.
        fn run_effect(&mut self, ctx: &egui::Context, effect: Effect) {
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
                // The table emits this only on `LauncherPrepared`, which `poll_workers` dispatches
                // right after storing the entry; anything else is a shell defect, logged instead of
                // acted on.
                Effect::ShowLauncher => {
                    let entry = match &mut self.active {
                        Active::PreparingLauncher(prepare) => prepare.take_entry(),
                        Active::Launcher(_) | Active::Studio(_) | Active::Closing(_) | Active::Gone => None,
                    };
                    match entry {
                        Some(entry) => {
                            let mut launcher = LauncherApp::new(*entry);
                            launcher.set_update_notification(self.update_notification.clone());
                            self.active = Active::Launcher(Box::new(launcher));
                        }
                        None => runtime_log::log_error("[shell] the launcher screen was entered without its entry"),
                    }
                }
                Effect::LoadStudio { target, tab } => {
                    self.studio_title = target.window_title(env!("MS_APP_VERSION"));
                    self.active = Active::Studio(Box::new(StudioScreen::start(target, tab, self.user_settings.clone(), self.ai_backend.clone())));
                }
                // The table emits these two only for a loading studio, on the result `poll_load`
                // just settled in the screen (which checks its own phase); another screen is a
                // shell defect, logged instead of acted on.
                Effect::StartStudio => match &mut self.active {
                    Active::Studio(studio) => studio.start_running(),
                    Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => runtime_log::log_error("[shell] the studio was started without a studio screen"),
                },
                Effect::ShowLoadError => match &mut self.active {
                    Active::Studio(studio) => studio.fail(),
                    Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => runtime_log::log_error("[shell] the studio error screen was entered without a studio screen"),
                },
                Effect::Shutdown => {
                    // A project loaded after the window was asked to close is still in the screen
                    // and is discarded by `into_parts`: constructing `StudioApp` would spawn loaders
                    // for nothing.
                    let (app, session) = match std::mem::replace(&mut self.active, Active::Gone) {
                        Active::Studio(studio) => studio.into_parts(),
                        // The table emits `Shutdown` only from a studio screen; anything else is a
                        // shell bug. The empty teardown then finishes at once and the window moves on.
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => {
                            runtime_log::log_error("[shell] studio teardown requested while no studio is active");
                            (None, None)
                        }
                    };
                    self.active = Active::Closing(ClosingScreen::start(app, session));
                }
                Effect::RequestOsClose => {
                    match &mut self.active {
                        Active::Studio(studio) => studio.request_os_close(),
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => {}
                    }
                    // The exit dialog opened after this frame's draw; show it without waiting.
                    ctx.request_repaint();
                    // The close it may have decided is performed now, not after the next draw:
                    // a minimized window gets no `ui` call.
                    self.drain_studio_outputs(ctx);
                }
                Effect::CloseWindow => {
                    self.active = Active::Gone;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Effect::QueueOpen(target) => {
                    runtime_log::log_info(format!("[shell] {} received image(s) queued until {:?} can take them", received_count(&target), self.kind));
                    self.pending_open = Some(target);
                }
                Effect::AskStudioToOpen(target) => {
                    let count = received_count(&target);
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
            self.apply_title(ctx);
            if let Some([width, height]) = screen::min_inner_size(&self.kind) {
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(width, height)));
            }
            window_geometry::sync_secondary_window_policy(ctx, ms_config::secondary_windows::current());
            ctx.request_repaint();
        }

        /// The window title path: sets the current screen's title (launcher, or the studio's
        /// `studio_title`); Closing and Exiting keep the title they inherited.
        fn apply_title(&self, ctx: &egui::Context) {
            let title = match &self.kind {
                ScreenKind::PreparingLauncher | ScreenKind::Launcher => Some(launcher_title(self.test_mode).to_string()),
                ScreenKind::Studio(_) => Some(self.studio_title.clone()),
                ScreenKind::Closing { .. } | ScreenKind::Exiting { .. } => None,
            };
            if let Some(title) = title {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
            }
        }

        /// Drains the running studio's outbound queue (`StudioScreen::take_outputs`) and applies it
        /// in its order: a title through `apply_title`, a close as `ShellEvent::StudioClose`, a
        /// decline by dropping the pending received images, a reload as the screen's
        /// `StudioExit::Reload`. Called after every studio draw, from `poll_workers` (what arrived
        /// while no draw ran, e.g. a minimized window) and right after the OS close was handed
        /// over. A close switches the screen, after which the later outputs of the same drain find
        /// nothing left to act on (`collect_outputs` already drops a reload behind a close).
        fn drain_studio_outputs(&mut self, ctx: &egui::Context) {
            let outputs = match &mut self.active {
                Active::Studio(studio) => studio.take_outputs(),
                Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => return,
            };
            for output in outputs {
                self.apply_studio_output(ctx, output);
            }
        }

        /// Applies one studio output (see `drain_studio_outputs`).
        fn apply_studio_output(&mut self, ctx: &egui::Context, output: StudioOutput) {
            match output {
                StudioOutput::Title(title) => {
                    runtime_log::log_info(format!("[shell] studio window title: {title}"));
                    self.studio_title = title;
                    self.apply_title(ctx);
                }
                StudioOutput::Close(action) => self.dispatch(ctx, ShellEvent::StudioClose(action)),
                StudioOutput::IncomingDeclined => {
                    if let Some(target) = self.pending_open.take() {
                        incoming::discard_target(target, "the user declined to close the studio for them");
                    }
                }
                StudioOutput::ReloadRequested => {
                    let exit = match &self.active {
                        Active::Studio(studio) => studio.reload_exit(),
                        Active::PreparingLauncher(_) | Active::Launcher(_) | Active::Closing(_) | Active::Gone => {
                            runtime_log::log_warn(format!("[shell] studio reload request arrived on {:?}; ignored", self.kind));
                            None
                        }
                    };
                    if let Some(exit) = exit {
                        self.dispatch(ctx, Self::studio_event(exit));
                    }
                }
            }
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
                    LauncherPrepare::Waiting(rx)
                }
                Err(err) => {
                    runtime_log::log_error(format!("[shell] could not start the launcher-prepare thread: {err}"));
                    LauncherPrepare::WorkerLost
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
                    let LauncherPrepare::Waiting(rx) = prepare else {
                        return;
                    };
                    match rx.try_recv() {
                        Ok(entry) => {
                            *prepare = LauncherPrepare::Ready(Box::new(entry));
                            self.dispatch(ctx, ShellEvent::LauncherPrepared);
                        }
                        Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL_INTERVAL),
                        Err(TryRecvError::Disconnected) => {
                            runtime_log::log_error("[shell] the launcher-prepare worker exited without an entry");
                            *prepare = LauncherPrepare::WorkerLost;
                        }
                    }
                }
                Active::Studio(studio) => {
                    // A deferred OS close stops an image-set open even while the window is
                    // minimized (`ui` is not called then).
                    if matches!(self.kind, ScreenKind::Studio(StudioPhase::Loading { close_after_load: true })) {
                        studio.cancel_for_close();
                    }
                    // Only a running studio has outputs and only a loading one has a load result, so
                    // the two never both act in one poll.
                    if let Some(result) = studio.poll_load() {
                        let event = match result {
                            LoadResult::Loaded => ShellEvent::StudioLoaded,
                            LoadResult::Failed => ShellEvent::StudioLoadFailed,
                        };
                        self.dispatch(ctx, event);
                    } else if matches!(self.kind, ScreenKind::Studio(StudioPhase::Loading { .. })) {
                        ctx.request_repaint_after(POLL_INTERVAL);
                    } else {
                        self.drain_studio_outputs(ctx);
                    }
                }
                Active::Closing(closing) => {
                    if closing.poll(ctx) {
                        self.dispatch(ctx, ShellEvent::ShutdownFinished);
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
                        runtime_log::log_info(format!("[shell] received {} image(s) from another app on {:?}", received_count(&target), self.kind));
                        self.dispatch(ctx, ShellEvent::ExternalOpen(target));
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
                self.dispatch(ctx, ShellEvent::OsClose);
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
                    retry_prepare = draw_preparing_screen(ui, matches!(prepare, LauncherPrepare::WorkerLost));
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
                self.dispatch(&ctx, event);
            }
            // The running studio's close / reload / decline / title decided during its draw, in
            // the same frame (a no-op on every other screen).
            self.drain_studio_outputs(&ctx);
        }

        /// Routes canvas finger input before egui sees it (`ms_widgets::touch_nav`): claimed
        /// touches lose their mouse emulation and become navigation / long-press outputs that the
        /// canvases take this frame. Child viewports pass through untouched (the router checks).
        fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
            // The General settings pane flips the live setting; the router applies a change at
            // a safe frame start (never in the middle of a claimed gesture).
            self.touch_router.set_enabled(touch_gestures_enabled());
            self.touch_router.route(ctx, raw_input);
        }

        /// The process-shutdown path (eframe ended the window): finishes or performs the studio
        /// teardown inline, flushes the window geometry and frees the shader-layer GL objects.
        fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
            match std::mem::replace(&mut self.active, Active::Gone) {
                Active::Studio(studio) => {
                    let (app, session) = studio.into_parts();
                    if app.is_some() || session.is_some() {
                        runtime_log::log_info("[shell] the main window ended with the studio open; tearing it down inline");
                    }
                    // The report is dropped on purpose: its items are logged by the teardown and
                    // no frame is left to show them.
                    drop(closing::teardown_blocking(app, session));
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

    #[cfg(test)]
    mod tests {
        use super::super::screen::AfterClose;
        use super::super::target::ImageSources;
        use super::*;
        use std::path::PathBuf;

        /// An image-set target over `files` (no skips, no inbox batches: dropping it spawns
        /// nothing).
        fn images(files: &[&str]) -> StartupTarget {
            StartupTarget::Images(ImageSources::new(files.iter().map(PathBuf::from).collect(), Vec::new(), Vec::new()).expect("non-empty set"))
        }

        /// A shell on `kind` / `active` without a window: what `AppShell::new` builds minus the
        /// creator-only work (monitors, fonts, GL, waker) and the initial effect. The geometry
        /// tracker's writer gets no sample, so it never writes `user_config`. `exit` starts at
        /// `StartUpdate` so a test sees whether the shell wrote it.
        fn test_shell(kind: ScreenKind, active: Active) -> AppShell {
            let ctx = egui::Context::default();
            AppShell {
                kind,
                active,
                user_settings: serde_json::Value::Null,
                ai_backend: AiBackendHandle::disabled(),
                host: LauncherHost { ignore_installed: false, version_core: "0.0.0" },
                test_mode: false,
                update_check: None,
                update_rx: None,
                update_notification: None,
                studio_title: String::new(),
                geometry: WindowGeometryTracker::new(&window_geometry::WindowSettings::default()),
                #[cfg(target_os = "windows")]
                maximize_on_first_frame: false,
                egui_ctx: ctx,
                exit: Rc::new(Cell::new(ShellExit::StartUpdate)),
                pending_open: None,
                incoming_take: None,
                incoming_take_unavailable: false,
                incoming_error: None,
                touch_router: TouchRouter::new(false),
            }
        }

        #[test]
        fn external_open_absorbs_the_pending_target() {
            let mut pending = Some(images(&["/home/u/a.png"]));
            let event = resolve_open_event(&mut pending, ShellEvent::ExternalOpen(images(&["/home/u/b.png"])));
            assert_eq!(event, ShellEvent::ExternalOpen(images(&["/home/u/a.png", "/home/u/b.png"])));
            assert_eq!(pending, None);
            let event = resolve_open_event(&mut pending, ShellEvent::ExternalOpen(images(&["/home/u/c.png"])));
            assert_eq!(event, ShellEvent::ExternalOpen(images(&["/home/u/c.png"])), "nothing pending: the target passes");
        }

        #[test]
        fn studio_close_for_incoming_takes_the_pending_target() {
            let mut pending = Some(images(&["/home/u/a.png"]));
            let event = resolve_open_event(&mut pending, ShellEvent::StudioClose(PendingCloseAction::OpenIncoming));
            assert_eq!(event, ShellEvent::StudioCloseForOpen(images(&["/home/u/a.png"])));
            assert_eq!(pending, None);
            // Nothing pending is a defect: the studio still closes, back to the launcher.
            let event = resolve_open_event(&mut pending, ShellEvent::StudioClose(PendingCloseAction::OpenIncoming));
            assert_eq!(event, ShellEvent::StudioClose(PendingCloseAction::ReturnToLauncher));
        }

        #[test]
        fn any_other_studio_close_drops_the_pending_target() {
            for action in [PendingCloseAction::Exit, PendingCloseAction::ReturnToLauncher] {
                let mut pending = Some(images(&["/home/u/a.png"]));
                let event = resolve_open_event(&mut pending, ShellEvent::StudioClose(action));
                assert_eq!(event, ShellEvent::StudioClose(action));
                assert_eq!(pending, None, "{action:?} drops the pending target");
            }
        }

        #[test]
        fn other_events_pass_through_and_keep_the_pending_target() {
            let events = [
                ShellEvent::LauncherPrepared,
                ShellEvent::LauncherExit(screen::LauncherExit::Quit),
                ShellEvent::StudioLoaded,
                ShellEvent::StudioLoadFailed,
                ShellEvent::StudioReload { project_dir: PathBuf::from("/home/u/Title/Chapter 1") },
                ShellEvent::FailedBack,
                ShellEvent::FailedExit,
                ShellEvent::ShutdownFinished,
                ShellEvent::OsClose,
                ShellEvent::StudioCloseForOpen(images(&["/home/u/b.png"])),
            ];
            for event in events {
                let mut pending = Some(images(&["/home/u/a.png"]));
                assert_eq!(resolve_open_event(&mut pending, event.clone()), event);
                assert_eq!(pending, Some(images(&["/home/u/a.png"])), "{event:?} must not touch the pending target");
            }
        }

        /// PreparingLauncher queues delivered targets (merged in arrival order); the OS close
        /// lets the window go and drops what was queued.
        #[test]
        fn preparing_launcher_queues_targets_and_an_os_close_drops_them() {
            let ctx = egui::Context::default();
            let mut shell = test_shell(ScreenKind::PreparingLauncher, Active::PreparingLauncher(LauncherPrepare::WorkerLost));
            shell.dispatch(&ctx, ShellEvent::ExternalOpen(images(&["/home/u/a.png"])));
            shell.dispatch(&ctx, ShellEvent::ExternalOpen(images(&["/home/u/b.png"])));
            assert_eq!(shell.kind, ScreenKind::PreparingLauncher);
            assert_eq!(shell.pending_open, Some(images(&["/home/u/a.png", "/home/u/b.png"])));
            shell.dispatch(&ctx, ShellEvent::OsClose);
            assert_eq!(shell.kind, ScreenKind::Exiting { exit: ShellExit::Quit });
            assert_eq!(shell.exit.get(), ShellExit::Quit, "the exit is published on entering Exiting");
            assert_eq!(shell.pending_open, None, "the window is closing: the queued target is dropped");
        }

        /// An event the table rejects changes nothing; a target delivered to an exiting window is
        /// dropped instead of queued.
        #[test]
        fn rejected_events_leave_the_shell_unchanged() {
            let ctx = egui::Context::default();
            let mut shell = test_shell(ScreenKind::PreparingLauncher, Active::PreparingLauncher(LauncherPrepare::WorkerLost));
            shell.dispatch(&ctx, ShellEvent::FailedBack);
            assert_eq!(shell.kind, ScreenKind::PreparingLauncher);
            assert!(matches!(shell.active, Active::PreparingLauncher(LauncherPrepare::WorkerLost)));

            let mut shell = test_shell(ScreenKind::Exiting { exit: ShellExit::Quit }, Active::Gone);
            shell.dispatch(&ctx, ShellEvent::ExternalOpen(images(&["/home/u/a.png"])));
            assert_eq!(shell.kind, ScreenKind::Exiting { exit: ShellExit::Quit });
            assert_eq!(shell.pending_open, None);
            assert_eq!(shell.exit.get(), ShellExit::StartUpdate, "a rejected event publishes nothing");
        }

        /// A launcher-prepare worker that died without an entry turns the screen into the retry
        /// state and dispatches nothing.
        #[test]
        fn a_lost_launcher_worker_offers_the_retry() {
            let ctx = egui::Context::default();
            let (tx, rx) = mpsc::channel::<LauncherEntry>();
            let mut shell = test_shell(ScreenKind::PreparingLauncher, Active::PreparingLauncher(LauncherPrepare::Waiting(rx)));
            shell.poll_workers(&ctx);
            assert!(matches!(shell.active, Active::PreparingLauncher(LauncherPrepare::Waiting(_))), "the worker is still running");
            drop(tx);
            shell.poll_workers(&ctx);
            assert!(matches!(shell.active, Active::PreparingLauncher(LauncherPrepare::WorkerLost)));
            assert_eq!(shell.kind, ScreenKind::PreparingLauncher);
        }

        /// Root-viewport commands `apply` sent inside one frame of `ctx`.
        fn root_commands(ctx: &egui::Context, apply: impl FnOnce()) -> Vec<egui::ViewportCommand> {
            let mut apply = Some(apply);
            let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
                if let Some(apply) = apply.take() {
                    apply();
                }
            });
            let commands = output.viewport_output.get(&egui::ViewportId::ROOT).map(|viewport| viewport.commands.clone()).unwrap_or_default();
            output.drop_without_applying_deltas();
            commands
        }

        /// A studio `Title` output goes through the shell's title path: it becomes the studio title
        /// and is sent to the window while the studio screen is shown, in the same frame.
        #[test]
        fn a_studio_title_output_retitles_the_window() {
            let ctx = egui::Context::default();
            let shell = std::cell::RefCell::new(test_shell(ScreenKind::Studio(StudioPhase::Running), Active::Gone));
            let commands = root_commands(&ctx, || shell.borrow_mut().apply_studio_output(&ctx, StudioOutput::Title("ManhwaStudio v1 - a.png".to_string())));
            assert!(commands.contains(&egui::ViewportCommand::Title("ManhwaStudio v1 - a.png".to_string())), "{commands:?}");
            assert_eq!(shell.borrow().studio_title, "ManhwaStudio v1 - a.png");
        }

        /// A decline drops the received images the shell held for the studio's answer.
        #[test]
        fn a_studio_decline_output_drops_the_pending_target() {
            let ctx = egui::Context::default();
            let mut shell = test_shell(ScreenKind::Studio(StudioPhase::Running), Active::Gone);
            shell.pending_open = Some(images(&["/home/u/a.png"]));
            shell.apply_studio_output(&ctx, StudioOutput::IncomingDeclined);
            assert_eq!(shell.pending_open, None);
            assert_eq!(shell.kind, ScreenKind::Studio(StudioPhase::Running), "a decline keeps the studio running");
        }

        /// Closing: an OS close turns the destination into an exit; the finished teardown, seen
        /// by `poll_workers`, closes the window with that exit.
        #[test]
        fn closing_os_close_then_teardown_exits() {
            let ctx = egui::Context::default();
            let mut shell = test_shell(ScreenKind::Closing { then: AfterClose::Launcher }, Active::Closing(ClosingScreen::start(None, None)));
            shell.dispatch(&ctx, ShellEvent::OsClose);
            assert_eq!(shell.kind, ScreenKind::Closing { then: AfterClose::Exit });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !matches!(shell.kind, ScreenKind::Exiting { .. }) {
                assert!(std::time::Instant::now() < deadline, "the empty teardown did not finish");
                shell.poll_workers(&ctx);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(shell.kind, ScreenKind::Exiting { exit: ShellExit::Quit });
            assert_eq!(shell.exit.get(), ShellExit::Quit);
            assert!(matches!(shell.active, Active::Gone), "CloseWindow leaves nothing to draw");
        }
    }
}
