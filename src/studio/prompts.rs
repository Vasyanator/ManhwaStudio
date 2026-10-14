/*
File: src/studio/prompts.rs

Purpose:
The two modal prompts the studio shows over every tab: the comic-type choice (a project
without a `comic_type` asks once whether it is pages or a ribbon) and the AI device selection
(the connected backend reports that a PyTorch and/or DirectML device must be chosen).

Key structures:
- `ComicTypePrompt`: open flag and last save error; `draw` returns the clicked `ComicType`.
- `AiDevicePrompt`: open flag, the per-opening selection state and the "applying" latch.

Key functions:
- `ComicTypePrompt::{new, draw, fail, resolve, is_open}`
- `AiDevicePrompt::{refresh, draw}`
- `comic_choice`, `needs_device_selection`, `directml_options`, `initial_pytorch_device`,
  `initial_directml_device`, `device_commands`, `option_label`: the pure decisions, tested here.

Notes:
- Unlike `shutdown.rs` / `incoming_open.rs`, these are owned structs with a narrow API: they do
  not reach `StudioApp`'s fields. Applying a comic type (`StudioApp::apply_comic_type_preset`)
  stays in `studio.rs` because it writes the project, the canvas, both shared models and the
  Settings tab; it reports back through `fail` / `resolve`.
- The device prompt only sends `AiBackendProbeCommand`s; it never blocks or waits for the
  backend. It closes itself once the pushed health snapshot no longer asks for a selection.
- The two device pickers are stock `egui::ComboBox`es with a stable `id_salt`
  (`egui-docs/05-ids-and-i18n.md`).
*/

use crate::project::ComicType;
use crate::runtime_log;
use crate::tabs::translation::backend_health::{
    AiBackendDeviceOption, AiBackendHealthSnapshot, AiBackendProbeCommand,
};
use eframe::egui;
use egui::Align2;
use std::sync::mpsc::Sender;

/// ONNX execution provider the device prompt selects a device for.
const DIRECTML_PROVIDER: &str = "DmlExecutionProvider";

/// The blocking "pages or ribbon?" prompt of a project that has no `comic_type` yet.
///
/// While it is open the studio dispatches no hotkeys. It closes only through [`Self::resolve`],
/// which the caller invokes after the chosen preset was persisted.
#[derive(Debug, Default)]
pub(crate) struct ComicTypePrompt {
    open: bool,
    /// User-facing message of the last failed save of the choice; shown above the buttons.
    error: Option<String>,
}

impl ComicTypePrompt {
    /// A prompt that starts open exactly when the project has no comic type.
    pub(crate) fn new(open: bool) -> Self {
        Self { open, error: None }
    }

    /// Whether the prompt is shown (and blocks hotkeys) this frame.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Records a failed save: the prompt stays open and shows `message`.
    pub(crate) fn fail(&mut self, message: String) {
        self.error = Some(message);
    }

    /// Closes the prompt after the choice was applied and saved; clears any old error.
    pub(crate) fn resolve(&mut self) {
        self.error = None;
        self.open = false;
    }

    /// Draws the prompt window when open and returns the type the user clicked this frame.
    ///
    /// Returns `None` when closed or when nothing was clicked; the caller applies the choice
    /// and then calls [`Self::resolve`] or [`Self::fail`].
    pub(crate) fn draw(&mut self, ctx: &egui::Context) -> Option<ComicType> {
        if !self.open {
            return None;
        }

        let mut pages_clicked = false;
        let mut ribbon_clicked = false;
        egui::Window::new(t!("app.comic_type.window_title")).id(egui::Id::new("app.comic_type.window_title"))
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                if let Some(message) = self.error.as_ref() {
                    ui.colored_label(ms_theme::status::ERROR, message);
                    ui.add_space(8.0);
                }
                pages_clicked = ui.button(t!("app.comic_type.pages_option")).clicked();
                ribbon_clicked = ui.button(t!("app.comic_type.ribbon_option")).clicked();
            });
        comic_choice(pages_clicked, ribbon_clicked)
    }
}

/// Maps the two button clicks of one frame to the chosen comic type.
///
/// Both clicked in one frame resolves to `Ribbon`: the buttons used to apply in drawing order,
/// so the later one (ribbon) was the state left behind.
fn comic_choice(pages_clicked: bool, ribbon_clicked: bool) -> Option<ComicType> {
    if ribbon_clicked {
        Some(ComicType::Ribbon)
    } else if pages_clicked {
        Some(ComicType::Pages)
    } else {
        None
    }
}

/// The blocking AI device selection prompt.
///
/// Opened by [`Self::refresh`] when the connected backend reports a PyTorch and/or DirectML
/// device that must be chosen; closed by [`Self::draw`] once the health snapshot no longer asks.
/// The selection is seeded from the snapshot once per opening.
#[derive(Debug, Default)]
pub(crate) struct AiDevicePrompt {
    open: bool,
    /// The two selections below were seeded from the snapshot for this opening.
    initialized: bool,
    /// Selected PyTorch device id (`AiBackendDeviceOption::id`).
    pytorch_device: String,
    /// Selected DirectML device id.
    directml_device_id: String,
    /// A selection was sent; the confirm button stays disabled until the prompt closes.
    applying: bool,
    /// User-facing message of the last failed confirm.
    error: Option<String>,
}

impl AiDevicePrompt {
    /// Opens the prompt when `health` asks for a device selection and it is not open yet.
    ///
    /// On opening it resets the per-opening state and asks the probe (if any) for fresh device
    /// info; a failed send is logged and otherwise ignored, the prompt still opens.
    pub(crate) fn refresh(&mut self, health: &AiBackendHealthSnapshot, probe_tx: Option<&Sender<AiBackendProbeCommand>>) {
        if self.open || !needs_device_selection(health) {
            return;
        }

        self.open = true;
        self.initialized = false;
        self.applying = false;
        self.error = None;
        if let Some(tx) = probe_tx
            && let Err(err) = tx.send(AiBackendProbeCommand::RefreshDeviceInfo)
        {
            runtime_log::log_warn(format!(
                "[app] failed to request AI backend device refresh: {err}"
            ));
        }
    }

    /// Draws the prompt when open; returns whether it is still open after this frame.
    ///
    /// Closes (returning `false`) as soon as `health` no longer asks for a selection. Confirm
    /// sends `SetDevice` / `SetOnnxDevice` through `probe_tx`; without a sender, or when every
    /// send failed, it shows an error instead of latching "applying".
    pub(crate) fn draw(&mut self, ctx: &egui::Context, health: &AiBackendHealthSnapshot, probe_tx: Option<&Sender<AiBackendProbeCommand>>) -> bool {
        if !self.open {
            return false;
        }

        if !needs_device_selection(health) {
            self.open = false;
            self.initialized = false;
            self.applying = false;
            return false;
        }

        let directml_options = directml_options(health);

        if !self.initialized {
            self.pytorch_device = initial_pytorch_device(health);
            self.directml_device_id = initial_directml_device(health, &directml_options);
            self.initialized = true;
        }

        egui::Window::new(t!("app.gpu_prompt.window_title")).id(egui::Id::new("app.gpu_prompt.window_title"))
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                ui.label(t!("app.gpu_prompt.hint"));
                ui.add_space(8.0);

                if let Some(message) = self.error.as_ref() {
                    ui.colored_label(ms_theme::status::ERROR, message);
                    ui.add_space(8.0);
                }
                if self.applying {
                    ui.small(t!("app.gpu_prompt.applying"));
                    ui.add_space(8.0);
                }

                if health.torch_device_needs_selection && !health.device_options.is_empty() {
                    let selected_text = option_label(&health.device_options, &self.pytorch_device);
                    // Stable id_salt so a live UI-language switch does not reset the
                    // combo state (native `ComboBox` derives its id from the label text
                    // unless given an explicit salt).
                    egui::ComboBox::new("ai_device_prompt_pytorch_combo", t!("app.gpu_prompt.pytorch_device_label"))
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for option in &health.device_options {
                                ui.selectable_value(
                                    &mut self.pytorch_device,
                                    option.id.clone(),
                                    option.label.as_str(),
                                );
                            }
                        });
                    ui.add_space(6.0);
                }

                if health.onnx_device_needs_selection {
                    let selected_text = option_label(&directml_options, &self.directml_device_id);
                    // Stable id_salt (see the PyTorch combo above).
                    egui::ComboBox::new("ai_device_prompt_directml_combo", t!("app.gpu_prompt.directml_device_label"))
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            for option in &directml_options {
                                ui.selectable_value(
                                    &mut self.directml_device_id,
                                    option.id.clone(),
                                    option.label.as_str(),
                                );
                            }
                        });
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.applying,
                            egui::Button::new(t!("app.gpu_prompt.confirm_button")),
                        )
                        .clicked()
                    {
                        self.confirm(health, probe_tx);
                    }
                });
            });
        self.open
    }

    /// Validates the selection and sends it; updates `error` / `applying` with the result.
    fn confirm(&mut self, health: &AiBackendHealthSnapshot, probe_tx: Option<&Sender<AiBackendProbeCommand>>) {
        // Validation comes before the sender check, so a missing DirectML choice is reported
        // even while the backend probe is unavailable.
        let Some(commands) = device_commands(health, &self.pytorch_device, &self.directml_device_id) else {
            self.error = Some(t!("app.gpu_prompt.select_directml_hint").to_string());
            return;
        };
        let Some(tx) = probe_tx else {
            self.error = Some(t!("app.gpu_prompt.backend_unavailable").to_string());
            return;
        };
        let mut command_sent = false;
        for (device_kind, command) in commands {
            match tx.send(command) {
                Ok(()) => {
                    command_sent = true;
                }
                Err(err) => {
                    runtime_log::log_warn(format!(
                        "[app] failed to send {device_kind} device selection: {err}"
                    ));
                }
            }
        }
        if command_sent {
            self.error = None;
            self.applying = true;
        } else {
            self.error = Some(t!("app.gpu_prompt.send_failed").to_string());
        }
    }
}

/// The backend is connected and asks for a PyTorch or a DirectML device choice.
fn needs_device_selection(health: &AiBackendHealthSnapshot) -> bool {
    health.connected && (health.torch_device_needs_selection || health.onnx_device_needs_selection)
}

/// The DirectML device list: the provider-specific list when the backend reported one, else the
/// generic ONNX device list.
fn directml_options(health: &AiBackendHealthSnapshot) -> Vec<AiBackendDeviceOption> {
    health
        .onnx_devices_by_provider
        .get(DIRECTML_PROVIDER)
        .cloned()
        .unwrap_or_else(|| health.onnx_device_options.clone())
}

/// Seed of the PyTorch selection: the backend's current device, else the first option, else "".
fn initial_pytorch_device(health: &AiBackendHealthSnapshot) -> String {
    health
        .selected_device
        .clone()
        .or_else(|| health.device_options.first().map(|option| option.id.clone()))
        .unwrap_or_default()
}

/// Seed of the DirectML selection: the backend's current ONNX device, else the first of
/// `directml_options`, else "".
fn initial_directml_device(health: &AiBackendHealthSnapshot, directml_options: &[AiBackendDeviceOption]) -> String {
    health
        .selected_onnx_device_id
        .clone()
        .or_else(|| directml_options.first().map(|option| option.id.clone()))
        .unwrap_or_default()
}

/// The commands a confirm sends, in send order (PyTorch first, then DirectML), each with the
/// device kind named in its send-failure log line.
///
/// `None` when a DirectML choice is required but `directml_device_id` is blank. A PyTorch command
/// is included only when that selection is required AND non-blank; the list may be empty.
fn device_commands(health: &AiBackendHealthSnapshot, pytorch_device: &str, directml_device_id: &str) -> Option<Vec<(&'static str, AiBackendProbeCommand)>> {
    if health.onnx_device_needs_selection && directml_device_id.trim().is_empty() {
        return None;
    }
    let mut commands = Vec::with_capacity(2);
    if health.torch_device_needs_selection && !pytorch_device.trim().is_empty() {
        commands.push(("PyTorch", AiBackendProbeCommand::SetDevice(pytorch_device.to_string())));
    }
    if health.onnx_device_needs_selection {
        commands.push((
            "DirectML",
            AiBackendProbeCommand::SetOnnxDevice {
                provider: DIRECTML_PROVIDER.to_string(),
                device_id: directml_device_id.to_string(),
            },
        ));
    }
    Some(commands)
}

/// Combo text for `selected_id`: the matching option's label, the raw id when no option matches,
/// or the localized "no selection" text when the id is blank.
fn option_label(options: &[AiBackendDeviceOption], selected_id: &str) -> String {
    options
        .iter()
        .find(|option| option.id == selected_id)
        .map(|option| option.label.clone())
        .unwrap_or_else(|| {
            if selected_id.trim().is_empty() {
                t!("app.hotkey.no_binding").to_string()
            } else {
                selected_id.to_string()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, label: &str) -> AiBackendDeviceOption {
        AiBackendDeviceOption { id: id.to_string(), label: label.to_string() }
    }

    fn health(torch: bool, onnx: bool) -> AiBackendHealthSnapshot {
        AiBackendHealthSnapshot {
            connected: true,
            torch_device_needs_selection: torch,
            onnx_device_needs_selection: onnx,
            ..AiBackendHealthSnapshot::default()
        }
    }

    /// Renders commands as comparable strings (`AiBackendProbeCommand` has no `PartialEq`).
    fn rendered(commands: &[AiBackendProbeCommand]) -> Vec<String> {
        commands.iter().map(|command| format!("{command:?}")).collect()
    }

    /// Renders labelled commands as `"<kind> <command>"` strings.
    fn rendered_labelled(commands: Option<Vec<(&'static str, AiBackendProbeCommand)>>) -> Vec<String> {
        commands.unwrap_or_default().iter().map(|(kind, command)| format!("{kind} {command:?}")).collect()
    }

    #[test]
    fn comic_choice_outcome_table() {
        let cases = [
            (false, false, None),
            (true, false, Some(ComicType::Pages)),
            (false, true, Some(ComicType::Ribbon)),
            (true, true, Some(ComicType::Ribbon)),
        ];
        for (pages, ribbon, expected) in cases {
            assert_eq!(comic_choice(pages, ribbon), expected, "pages={pages} ribbon={ribbon}");
        }
    }

    #[test]
    fn comic_prompt_fail_keeps_open_and_resolve_closes() {
        let mut prompt = ComicTypePrompt::new(true);
        assert!(prompt.is_open());
        prompt.fail("save failed".to_string());
        assert!(prompt.is_open());
        assert_eq!(prompt.error.as_deref(), Some("save failed"));
        prompt.resolve();
        assert!(!prompt.is_open());
        assert_eq!(prompt.error, None);
    }

    #[test]
    fn closed_comic_prompt_draws_nothing() {
        let ctx = egui::Context::default();
        let mut prompt = ComicTypePrompt::new(false);
        assert_eq!(prompt.draw(&ctx), None);
    }

    #[test]
    fn needs_device_selection_requires_connection_and_a_request() {
        assert!(!needs_device_selection(&AiBackendHealthSnapshot::default()));
        assert!(!needs_device_selection(&health(false, false)));
        assert!(needs_device_selection(&health(true, false)));
        assert!(needs_device_selection(&health(false, true)));
        let disconnected = AiBackendHealthSnapshot { connected: false, ..health(true, true) };
        assert!(!needs_device_selection(&disconnected));
    }

    #[test]
    fn refresh_opens_once_and_requests_device_info() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut prompt = AiDevicePrompt::default();
        prompt.refresh(&health(true, false), Some(&tx));
        assert!(prompt.open);
        prompt.refresh(&health(true, false), Some(&tx));
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(rendered(&sent), vec!["RefreshDeviceInfo".to_string()]);
    }

    #[test]
    fn refresh_ignores_snapshot_without_request() {
        let mut prompt = AiDevicePrompt::default();
        prompt.refresh(&health(false, false), None);
        assert!(!prompt.open);
    }

    #[test]
    fn draw_closes_when_selection_no_longer_needed() {
        let ctx = egui::Context::default();
        let mut prompt = AiDevicePrompt::default();
        prompt.refresh(&health(true, false), None);
        prompt.applying = true;
        prompt.initialized = true;
        assert!(!prompt.draw(&ctx, &health(false, false), None));
        assert!(!prompt.open);
        assert!(!prompt.applying);
        assert!(!prompt.initialized);
    }

    #[test]
    fn directml_options_prefer_provider_list() {
        let mut snapshot = health(false, true);
        snapshot.onnx_device_options = vec![option("generic", "Generic")];
        assert_eq!(directml_options(&snapshot)[0].id, "generic");
        snapshot.onnx_devices_by_provider.insert(DIRECTML_PROVIDER.to_string(), vec![option("dml1", "GPU 1")]);
        assert_eq!(directml_options(&snapshot)[0].id, "dml1");
    }

    #[test]
    fn initial_selection_prefers_backend_choice_then_first_option() {
        let mut snapshot = health(true, true);
        assert_eq!(initial_pytorch_device(&snapshot), "");
        assert_eq!(initial_directml_device(&snapshot, &[]), "");
        snapshot.device_options = vec![option("cuda:0", "CUDA 0"), option("cpu", "CPU")];
        let dml = [option("1", "GPU 1"), option("2", "GPU 2")];
        assert_eq!(initial_pytorch_device(&snapshot), "cuda:0");
        assert_eq!(initial_directml_device(&snapshot, &dml), "1");
        snapshot.selected_device = Some("cpu".to_string());
        snapshot.selected_onnx_device_id = Some("2".to_string());
        assert_eq!(initial_pytorch_device(&snapshot), "cpu");
        assert_eq!(initial_directml_device(&snapshot, &dml), "2");
    }

    #[test]
    fn device_commands_table() {
        // Blank DirectML choice while one is required: refused before anything is sent.
        assert!(device_commands(&health(true, true), "cuda:0", "  ").is_none());
        assert_eq!(
            rendered_labelled(device_commands(&health(true, true), "cuda:0", "1")),
            vec![
                "PyTorch SetDevice(\"cuda:0\")".to_string(),
                "DirectML SetOnnxDevice { provider: \"DmlExecutionProvider\", device_id: \"1\" }".to_string(),
            ]
        );
        // A blank PyTorch choice is skipped, not refused.
        assert_eq!(rendered_labelled(device_commands(&health(true, false), " ", "")), Vec::<String>::new());
        // A PyTorch selection that is not requested is not sent.
        assert_eq!(
            rendered_labelled(device_commands(&health(false, true), "cuda:0", "2")),
            vec!["DirectML SetOnnxDevice { provider: \"DmlExecutionProvider\", device_id: \"2\" }".to_string()]
        );
    }

    #[test]
    fn option_label_prefers_label_then_raw_id() {
        let options = [option("cuda:0", "CUDA 0")];
        assert_eq!(option_label(&options, "cuda:0"), "CUDA 0");
        assert_eq!(option_label(&options, "cpu"), "cpu");
    }
}
