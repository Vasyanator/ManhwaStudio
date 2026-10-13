/*
FILE OVERVIEW: crates/ms-launcher/src/lib.rs
Crate root of `ms-launcher`, re-exported by the binary as `crate::launcher`
(`src/lib.rs`), so every `crate::launcher::…` call site keeps working unchanged.

Purpose:
Runtime for the Rust launcher UI.

Main responsibilities:
- expose the launcher screen API (`app::{LauncherEntry, prepare_entry, LauncherApp}`);
- keep the temporary own-window entry points (`run_launcher`, `run_test_launcher`) working
  until the persistent main window hosts the launcher screen;
- keep launcher-specific code isolated in this crate.

Key modules:
- `app`: the launcher screen (`LauncherApp`) and its entry preparation
- `background`: background catalog/validation workers
- `confirm_modal`: in-app Yes/No confirmation (`egui::Modal`) for overwrite questions
- `first_run_language`: first-run interface/typesetting language-selection modal
- `main_page`: rendering of the current main page
- `new_project`: detached "New Project" window runtime and UI
- `open_image`: main-menu "Open image" picker worker, path validation, outcome mapping (native only)
- `pages`: animated fullscreen launcher subpages
- `psd_import_window`: detached PSD/PSB import window, parsed in-process by `ag-psd`
- `state`: shared launcher UI state and page enum
- `theme`: dark theme styling helpers
- `tutorial`: launcher main-menu tutorial step script

Notes:
Launcher returns the selected or newly saved `project_dir` (or a picked image path for
single-image mode) back into startup flow instead of spawning a second app process.
*/

#![warn(clippy::all)]
// `new_without_default` fires on ten launcher controller/state structs
// (`QuickDownloadController`, `RelineController`, `RibbonState`, `Waifu2xController`, …).
// It is a LIBRARY-API lint: it only started applying when these types stopped being
// crate-private and became exported items of this crate. It does not apply here for an
// architectural reason: each of them has exactly ONE construction point, called from one
// place in this same crate, and `new()` is where the owning window documents what the
// controller starts out holding. A `Default` impl would add a second, equivalent
// constructor to the public surface of a crate that no one outside the binary consumes —
// pure API for no caller. The lint stays on everywhere else in the workspace.
#![allow(clippy::new_without_default)]

// `t!` / `tf!` / `tp!` for every user-visible string of the launcher.
#[macro_use]
extern crate ms_i18n;

pub mod app;
pub mod background;
mod confirm_modal;
pub mod first_run_language;
pub mod main_page;
pub mod new_project;
#[cfg(not(target_arch = "wasm32"))]
pub mod open_image;
pub mod pages;
pub mod psd_import_window;
pub mod state;
pub mod theme;
#[cfg(feature = "tutorial")]
pub mod tutorial;

use ms_settings_ui::ai_backend_supervisor::AiBackendHandle;
use crate::state::{LauncherHost, LauncherOutcome, UpdateNotification};
use std::sync::mpsc::Receiver;

pub use app::{LauncherApp, LauncherEntry, prepare_entry, prepare_entry_with_settings};

/// Inner size of the launcher window, in logical pixels. The launcher never restores a stored
/// size; this is also the size the startup placement centers on the chosen monitor.
#[cfg(not(target_arch = "wasm32"))]
const LAUNCHER_DEFAULT_INNER_SIZE: [f32; 2] = [1360.0, 860.0];

/// Runs the launcher in its own window until it closes and returns why it closed (`None` =
/// the user closed it). `host` carries the process facts the launcher cannot read itself.
///
/// # Errors
/// The window could not be created.
pub fn run_launcher(
    user_settings: &serde_json::Value,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    ai_backend: &AiBackendHandle,
    host: LauncherHost,
) -> anyhow::Result<Option<LauncherOutcome>> {
    run_launcher_internal(user_settings, false, update_check_rx, ai_backend, host)
}

/// [`run_launcher`] in test mode (own window title and app id); the outcome is dropped.
///
/// # Errors
/// Same as [`run_launcher`].
pub fn run_test_launcher(
    user_settings: &serde_json::Value,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    ai_backend: &AiBackendHandle,
    host: LauncherHost,
) -> anyhow::Result<()> {
    // Test mode routes nowhere: whatever the launcher returned is intentionally dropped.
    run_launcher_internal(user_settings, true, update_check_rx, ai_backend, host).map(drop)
}

/// Web stub for the launcher entry point.
///
/// The launcher owns a native OS window through `eframe::run_native`, which has no
/// browser equivalent (the web build uses a single `WebRunner` entry). The stub keeps
/// the same signature so shared startup code compiles, logs the dropped capability, and
/// surfaces a clear "unavailable on web" error instead of faking an outcome.
#[cfg(target_arch = "wasm32")]
fn run_launcher_internal(
    _user_settings: &serde_json::Value,
    test_mode: bool,
    _update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    _ai_backend: &AiBackendHandle,
    _host: LauncherHost,
) -> anyhow::Result<Option<LauncherOutcome>> {
    ms_log::runtime_log::log_warn(format!(
        "Rust launcher window{} is unavailable on the web build",
        if test_mode { " test mode" } else { "" }
    ));
    Err(anyhow::anyhow!(t!("launcher.web.window_launcher_unavailable")))
}

#[cfg(not(target_arch = "wasm32"))]
fn run_launcher_internal(
    user_settings: &serde_json::Value,
    test_mode: bool,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    ai_backend: &AiBackendHandle,
    host: LauncherHost,
) -> anyhow::Result<Option<LauncherOutcome>> {
    // Blocking entry preparation runs here, before the window exists, so the GUI thread of
    // the window never waits on it.
    let entry = prepare_entry(user_settings, ai_backend.clone(), host, test_mode);
    ms_log::runtime_log::log_info(format!(
        "starting Rust launcher{} with projects root '{}'",
        if test_mode { " test mode" } else { "" },
        entry.projects_root.display()
    ));
    let outcome = std::rc::Rc::new(std::cell::Cell::new(None::<LauncherOutcome>));
    let outcome_for_window = std::rc::Rc::clone(&outcome);

    // The launcher follows the primary-monitor choice but NOT the studio's stored size: the
    // `Window` geometry section describes the studio window, and the launcher keeps its own
    // dimensions (`SizePolicy::KeepDefault`).
    let placement = ms_window_geometry::plan_startup_placement(
        &ms_window_geometry::load_window_settings(),
        LAUNCHER_DEFAULT_INNER_SIZE,
        ms_window_geometry::SizePolicy::KeepDefault,
    );
    let viewport = ms_window_geometry::identity::with_identity(
        egui::ViewportBuilder::default()
            .with_inner_size(LAUNCHER_DEFAULT_INNER_SIZE)
            .with_min_inner_size([980.0, 680.0]),
        test_mode,
    );
    let viewport = ms_window_geometry::apply_placement(viewport, &placement);

    let native_options = eframe::NativeOptions {
        viewport,
        // winit cannot build an event loop on Android without the Activity handle.
        #[cfg(target_os = "android")]
        android_app: ms_window_geometry::android_app::current(),
        ..Default::default()
    };

    // eframe decides native vs embedded secondary viewports once per `run_native` (Android:
    // `General.secondary_windows`; a no-op elsewhere).
    ms_window_geometry::apply_android_native_window_policy();
    eframe::run_native(
        if test_mode {
            "ManhwaStudio - Rust Launcher Test"
        } else {
            "ManhwaStudio"
        },
        native_options,
        Box::new(move |cc| {
            // Publish the monitor list for the settings page and refresh the "largest monitor"
            // fallback: the launcher is often the only window a user opens before choosing a
            // monitor, so it must not depend on the studio having run first.
            ms_window_geometry::refresh_monitors(cc.winit_window().map(std::sync::Arc::as_ref));
            ms_widgets::ui_fonts::install(&cc.egui_ctx, ms_widgets::ui_fonts::Tier::Core);
            theme::configure_context(&cc.egui_ctx);
            // Global interface scale (`General.ui_scale_percent`): applied per-`Context`,
            // so the launcher window honors it independently of the studio window.
            ms_settings_ui::general_settings_panel::apply_ui_scale_to_context(&cc.egui_ctx);
            Ok(Box::new(LauncherWindow {
                screen: LauncherApp::new(entry),
                update_check_rx,
                outcome: outcome_for_window,
            }))
        }),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;

    Ok(outcome.take())
}

/// TEMPORARY own window around the launcher screen, so `run_launcher` keeps working until the
/// persistent main window (`src/shell/`, plan WP-6) hosts the screen; delete it then. It polls
/// the startup update check into the screen, and closes the window on the screen's first exit.
#[cfg(not(target_arch = "wasm32"))]
struct LauncherWindow {
    screen: LauncherApp,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    /// The first exit the screen produced, read by `run_launcher_internal` after the window
    /// closed. A `Cell` is enough: the window and the reader live on the same thread.
    outcome: std::rc::Rc<std::cell::Cell<Option<LauncherOutcome>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::App for LauncherWindow {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.update_check_rx {
            match rx.try_recv() {
                Ok(notification) => {
                    self.screen.set_update_notification(notification);
                    self.update_check_rx = None;
                    ui.ctx().request_repaint();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.update_check_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        self.screen.show(ui);
        if let Some(outcome) = self.screen.take_outcome() {
            // Keep the first exit if the close takes more than one frame to land.
            let pending = self.outcome.take();
            self.outcome.set(Some(pending.unwrap_or(outcome)));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
