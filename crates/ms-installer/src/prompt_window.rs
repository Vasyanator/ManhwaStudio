/*
File: prompt_window.rs

Purpose:
The missing-environment prompt: the small pre-window startup window shown when no usable Python
environment exists, offering "install", "update a custom install folder" and "open the
launcher".

Key items:
- `MissingPythonEnvAction`: the user's answer.
- `run_missing_python_env_prompt()`: runs the window and returns the answer.
- `MissingPythonEnvPromptApp`: the window state.

Notes:
Runs before the main window exists, ONCE per process (the binary's `src/startup/prompts.rs`
calls it). The custom-folder picker goes through `ms_platform::file_dialog` without blocking the
window; the picked folder is checked with `install::resolve_installed_program_copy_path`. Unlike
the installer windows it uses the program's shared window icon and, on Android, the Activity
handle (`ms_window_geometry`).
*/

use eframe::egui;
use ms_log::runtime_log;
use ms_platform::file_dialog::{self, DialogKind, FileDialogRequest, PendingDialog};
use std::sync::{Arc, Mutex};

use crate::install::resolve_installed_program_copy_path;
use crate::utils::ExternalUpdateTarget;
use crate::window::{WindowLook, prepare_window_context};

/// The user's answer in the missing-environment prompt; closing the window counts as
/// `LaunchLauncher`.
pub enum MissingPythonEnvAction {
    Install,
    UpdateCustom(ExternalUpdateTarget),
    LaunchLauncher,
}

/// Runs the missing-environment prompt as its own blocking window and returns the user's choice.
///
/// # Errors
/// The window could not run (`eframe::run_native` failed); the text is the eframe error. The
/// caller decides the fallback.
pub fn run_missing_python_env_prompt() -> Result<MissingPythonEnvAction, String> {
    let output = Arc::new(Mutex::new(MissingPythonEnvAction::LaunchLauncher));
    let output_for_app = Arc::clone(&output);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 250.0])
        .with_min_inner_size([480.0, 230.0])
        .with_resizable(false);
    if let Some(icon) = ms_window_geometry::identity::app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        // winit cannot build an event loop on Android without the Activity handle.
        #[cfg(target_os = "android")]
        android_app: ms_window_geometry::android_app::current(),
        ..Default::default()
    };

    eframe::run_native(
        "ManhwaStudio",
        options,
        Box::new(move |cc| {
            prepare_window_context(&cc.egui_ctx, WindowLook::StudioTheme);
            Ok(Box::new(MissingPythonEnvPromptApp {
                output: output_for_app,
                error_text: None,
                folder_pick: None,
            }))
        }),
    )
    .map_err(|err| err.to_string())?;

    // A poisoned slot (a panic inside the window) answers the same as a closed window.
    Ok(output
        .lock()
        .map(|mut guard| std::mem::replace(&mut *guard, MissingPythonEnvAction::LaunchLauncher))
        .unwrap_or(MissingPythonEnvAction::LaunchLauncher))
}

/// The prompt window's state; the chosen action is handed back through `output`.
struct MissingPythonEnvPromptApp {
    output: Arc<Mutex<MissingPythonEnvAction>>,
    error_text: Option<String>,
    /// The custom-update folder picker while it is open; every button is disabled meanwhile.
    folder_pick: Option<PendingDialog>,
}

impl MissingPythonEnvPromptApp {
    fn set_output_and_close(&self, action: MissingPythonEnvAction, ctx: &egui::Context) {
        if let Ok(mut output) = self.output.lock() {
            *output = action;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Opens the folder picker for «update a custom install folder» without blocking the window.
    fn start_custom_update_pick(&mut self, ctx: &egui::Context) {
        let request = FileDialogRequest::new(DialogKind::PickDirectoryPath).title(t!("startup.custom_update.folder_picker_title"));
        let repaint_ctx = ctx.clone();
        self.error_text = None;
        self.folder_pick = Some(file_dialog::request(request, Some(Arc::new(move || repaint_ctx.request_repaint()))));
    }

    /// Takes the folder picker's answer once it arrived: a cancel does nothing, an error is
    /// shown in the prompt (and logged), a folder is checked for an installed program copy.
    fn poll_custom_update_pick(&mut self, ctx: &egui::Context) {
        let Some(result) = self.folder_pick.as_mut().and_then(PendingDialog::poll) else {
            return;
        };
        self.folder_pick = None;
        let folder = match result {
            // `PickDirectoryPath` always answers `DirectoryPath` (the ms-platform kind contract).
            Ok(picked) => match picked.and_then(file_dialog::Picked::into_directory_path) {
                Some(folder) => folder,
                None => return,
            },
            Err(err) => {
                runtime_log::log_warn(format!("[startup] custom update folder picker failed: {err:?}"));
                self.error_text = Some(err.user_message());
                return;
            }
        };
        match resolve_installed_program_copy_path(&folder) {
            Ok(executable_path) => {
                self.set_output_and_close(
                    MissingPythonEnvAction::UpdateCustom(ExternalUpdateTarget {
                        root_dir: folder,
                        executable_path,
                    }),
                    ctx,
                );
            }
            Err(err) => {
                self.error_text = Some(err);
            }
        }
    }
}

impl eframe::App for MissingPythonEnvPromptApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 hands `App::ui` the window-root `Ui`; keep a borrowed `Context` handle for
        // the calls (viewport commands, helper methods) that still operate on the context.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_custom_update_pick(ctx);
        let picking = self.folder_pick.is_some();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(t!("startup.install_prompt.title"));
                ui.add_space(8.0);
                ui.label(
                    t!("startup.install_prompt.message"),
                );
                if let Some(error_text) = &self.error_text {
                    ui.add_space(8.0);
                    ui.colored_label(ms_theme::status::ERROR, error_text);
                }
                ui.add_space(14.0);
                // While the folder picker is open the window keeps repainting but takes no
                // other choice: its answer may still close the window.
                ui.add_enabled_ui(!picking, |ui| {
                    if ui
                        .add_sized([360.0, 34.0], egui::Button::new(t!("startup.install_prompt.install_button")))
                        .clicked()
                    {
                        self.set_output_and_close(MissingPythonEnvAction::Install, ctx);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [360.0, 34.0],
                            egui::Button::new(t!("startup.install_prompt.update_custom_button")),
                        )
                        .clicked()
                    {
                        self.start_custom_update_pick(ctx);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add_sized([360.0, 34.0], egui::Button::new(t!("startup.install_prompt.open_basic_launcher_button")))
                        .clicked()
                    {
                        self.set_output_and_close(MissingPythonEnvAction::LaunchLauncher, ctx);
                    }
                });
            });
        });
    }
}
