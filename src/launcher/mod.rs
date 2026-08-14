/*
File: src/launcher/mod.rs

Purpose:
Runtime for the new Rust launcher UI.

Main responsibilities:
- expose launcher entrypoints for default startup and explicit test mode;
- assemble native window options and icon;
- keep launcher-specific code isolated under `src/launcher/`.

Key modules:
- `app`: root `eframe::App` implementation
- `background`: background catalog/validation workers
- `first_run_language`: first-run interface/typesetting language-selection modal
- `main_page`: rendering of the current main page
- `new_project`: detached "New Project" window runtime and UI
- `pages`: animated fullscreen launcher subpages
- `psd_import_window`: detached PSD/PSB import window, parsed in-process by `ag-psd`
- `state`: shared launcher UI state and page enum
- `theme`: dark theme styling helpers
- `tutorial`: launcher main-menu tutorial step script

Notes:
Launcher returns the selected or newly saved `project_dir` back into startup flow instead of
spawning a second app process.
*/

pub mod app;
pub mod background;
pub mod first_run_language;
pub mod main_page;
pub mod new_project;
pub mod pages;
pub mod psd_import_window;
pub mod state;
pub mod theme;
#[cfg(feature = "tutorial")]
pub mod tutorial;

use crate::ai_backend_supervisor::AiBackendHandle;
#[cfg(not(target_arch = "wasm32"))]
use crate::config;
use crate::launcher::state::{LauncherOutcome, UpdateNotification};
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};

const EMBEDDED_APP_ICON_ICO: &[u8] = include_bytes!("../../app_icon.ico");
const EMBEDDED_APP_ICON_PNG: &[u8] = include_bytes!("../../app_icon_512.png");
const LAUNCHER_APP_ID: &str = "manhwastudio_rs.launcher";
const LAUNCHER_TEST_APP_ID: &str = "manhwastudio_rs.launcher_test";
/// Inner size of the launcher window, in logical pixels. The launcher never restores a stored
/// size; this is also the size the startup placement centers on the chosen monitor.
#[cfg(not(target_arch = "wasm32"))]
const LAUNCHER_DEFAULT_INNER_SIZE: [f32; 2] = [1360.0, 860.0];

pub fn run_launcher(
    user_settings: &serde_json::Value,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    ai_backend: &AiBackendHandle,
) -> anyhow::Result<Option<LauncherOutcome>> {
    run_launcher_internal(user_settings, false, update_check_rx, ai_backend)
}

pub fn run_test_launcher(
    user_settings: &serde_json::Value,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    ai_backend: &AiBackendHandle,
) -> anyhow::Result<()> {
    let _ = run_launcher_internal(user_settings, true, update_check_rx, ai_backend)?;
    Ok(())
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
) -> anyhow::Result<Option<LauncherOutcome>> {
    crate::runtime_log::log_warn(format!(
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
) -> anyhow::Result<Option<LauncherOutcome>> {
    let projects_root = config::projects_root_from_user_settings(user_settings);
    let user_settings = user_settings.clone();
    let ai_backend = ai_backend.clone();
    let output_outcome = Arc::new(Mutex::new(None::<LauncherOutcome>));
    let output_outcome_for_app = Arc::clone(&output_outcome);
    crate::runtime_log::log_info(format!(
        "starting Rust launcher{} with projects root '{}'",
        if test_mode { " test mode" } else { "" },
        projects_root.display()
    ));

    // The launcher follows the primary-monitor choice but NOT the studio's stored size: the
    // `Window` geometry section describes the studio window, and the launcher keeps its own
    // dimensions (`SizePolicy::KeepDefault`).
    let placement = crate::window_geometry::plan_startup_placement(
        &crate::window_geometry::load_window_settings(),
        LAUNCHER_DEFAULT_INNER_SIZE,
        crate::window_geometry::SizePolicy::KeepDefault,
    );
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size(LAUNCHER_DEFAULT_INNER_SIZE)
        .with_min_inner_size([980.0, 680.0])
        .with_app_id(launcher_app_id(test_mode));
    let viewport = crate::window_geometry::apply_placement(viewport, &placement);
    #[cfg(not(target_os = "windows"))]
    let viewport = viewport.with_maximized(true);
    let viewport = apply_launcher_window_metadata(viewport);

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        if test_mode {
            "ManhwaStudio - Rust Launcher Test"
        } else {
            "ManhwaStudio"
        },
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // Publish the monitor list for the settings page and refresh the "largest monitor"
            // fallback: the launcher is often the only window a user opens before choosing a
            // monitor, so it must not depend on the studio having run first.
            crate::window_geometry::refresh_monitors(cc.winit_window().map(Arc::as_ref));
            crate::ui_fonts::install(&cc.egui_ctx, crate::ui_fonts::Tier::Core);
            theme::configure_context(&cc.egui_ctx);
            // Global interface scale (`General.ui_scale_percent`): applied per-`Context`,
            // so the launcher window honors it independently of the studio window.
            crate::general_settings_panel::apply_ui_scale_to_context(&cc.egui_ctx);
            Ok(Box::new(app::LauncherApp::new(
                projects_root.clone(),
                launcher_app_id(test_mode).to_owned(),
                &user_settings,
                Arc::clone(&output_outcome_for_app),
                update_check_rx,
                ai_backend.clone(),
            )))
        }),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;

    let outcome = output_outcome
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to read launcher outcome"))?
        .clone();
    Ok(outcome)
}

pub(crate) fn apply_launcher_window_metadata(
    viewport: egui::ViewportBuilder,
) -> egui::ViewportBuilder {
    if let Some(icon) = load_embedded_icon_data() {
        viewport.with_icon(icon)
    } else {
        viewport
    }
}

pub(crate) fn launcher_app_id(test_mode: bool) -> &'static str {
    if test_mode {
        LAUNCHER_TEST_APP_ID
    } else {
        LAUNCHER_APP_ID
    }
}

fn load_embedded_icon_data() -> Option<egui::IconData> {
    let image = image::load_from_memory(EMBEDDED_APP_ICON_ICO)
        .or_else(|_| image::load_from_memory(EMBEDDED_APP_ICON_PNG))
        .ok()?;
    let rgba = image.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}
