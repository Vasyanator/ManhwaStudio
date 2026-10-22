/*
File: crates/ms-launcher/src/pages/system_info/mod.rs

Purpose:
The launcher settings page's "System info" tab: what this machine is (OS, device, CPU, RAM,
graphics adapters) and which AI acceleration it gets (the native ONNX build, the PyTorch build
the installer would offer, the PyTorch build the running backend reports). Native only (the
module is declared `cfg(not(target_arch = "wasm32"))` in `pages/mod.rs`).

Key structures:
- SystemInfoState: probe lifecycle (Idle / Running / Ready / Error), the worker receiver and
  the "report copied" flag; owned by `SettingsPageState`.

Key functions:
- SystemInfoState::show(): starts the probe on first show, polls it, draws the tab.
- spawn_probe(): the `launcher-system-info-probe` worker (hardware facts, native ONNX
  verdicts, PyTorch preflight); its `WakeOnDrop` guard requests one repaint on every exit,
  panic included, after the sender is gone (result or disconnect is then visible to `poll`).

Notes:
The worker runs only the probes of the local runtimes this process may use
(`AiBackendHandle::local_ai`). The backend's PyTorch row is not probed: it is read from the
pushed health snapshot every frame (a clone, no wait). Layout lives in `view.rs`, wording and
the copyable text in `report.rs`.
*/

mod report;
mod view;

use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::{Align, Layout, Ui, Vec2};
use ms_installer::torch::detect_torch_preflight;
use ms_log::runtime_log;
use ms_native_runtime::NativeHardwareFacts;
use ms_settings_ui::ai_backend_supervisor::AiBackendHandle;
use ms_settings_ui::native_builds::NativeBuildAvailability;
use ms_thread as thread;

use crate::theme;
use report::{BackendTorch, NativeProbe, NativeSelection, ProbeResult, TorchOffer};

/// Size of the header buttons (same as the other launcher-only tabs' refresh buttons).
const HEADER_BUTTON_SIZE: Vec2 = Vec2::new(112.0, 34.0);
/// Width of the "Copy report" button: wider than the refresh button for its longer label.
const COPY_BUTTON_WIDTH: f32 = 180.0;

/// Lifecycle of the tab's background probe.
#[derive(Debug, Default)]
enum ProbeStatus {
    /// Never started (the tab was not shown yet).
    #[default]
    Idle,
    /// The worker runs.
    Running,
    /// The last probe's result.
    Ready(Box<ProbeResult>),
    /// The worker could not start or vanished without an answer (localized text).
    Error(String),
}

/// State of the "System info" tab, owned by the launcher settings page.
#[derive(Debug, Default)]
pub(crate) struct SystemInfoState {
    status: ProbeStatus,
    rx: Option<Receiver<ProbeResult>>,
    /// The report was copied since the last probe started; shows a confirmation line.
    copied: bool,
}

/// Which probes the worker runs: only those of the local runtimes this process may use.
#[derive(Debug, Clone, Copy)]
struct ProbeRequest {
    /// Probe the native ONNX runtime (hardware facts + effective selection).
    native: bool,
    /// Run the installer's PyTorch preflight.
    torch: bool,
}

impl SystemInfoState {
    /// Draws the tab: header (hint, Refresh, Copy report), then the report cards. Starts the
    /// probe on the first call and polls it every call; never blocks.
    pub(crate) fn show(&mut self, ui: &mut Ui, backend: &AiBackendHandle) {
        if matches!(self.status, ProbeStatus::Idle) {
            self.start(ui, backend);
        }
        self.poll();

        let cards = match &self.status {
            ProbeStatus::Ready(probe) => {
                let backend_torch = BackendTorch::from_snapshot(&backend.health_snapshot());
                Some(report::build_report(probe, &backend_torch, &backend.local_ai))
            }
            ProbeStatus::Idle | ProbeStatus::Running | ProbeStatus::Error(_) => None,
        };

        let mut refresh = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::status(t!("launcher.settings.system_info.hint_label"), theme::TEXT_MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let idle = !matches!(self.status, ProbeStatus::Running);
                if theme::launcher_button(ui, t!("launcher.common.refresh_button"), HEADER_BUTTON_SIZE, idle).clicked() {
                    refresh = true;
                }
                let copy_size = Vec2::new(COPY_BUTTON_WIDTH, HEADER_BUTTON_SIZE.y);
                if theme::launcher_button(ui, t!("launcher.settings.system_info.copy_report_button"), copy_size, cards.is_some()).clicked()
                    && let Some(cards) = &cards
                {
                    ui.ctx().copy_text(report::plain_text(cards));
                    self.copied = true;
                }
            });
        });
        if self.copied {
            ui.label(theme::status(t!("launcher.settings.system_info.copied_status"), theme::TEXT_MUTED));
        }
        ui.add_space(12.0);

        match (&self.status, &cards) {
            (_, Some(cards)) => view::draw_cards(ui, cards),
            (ProbeStatus::Error(message), None) => {
                ui.label(theme::status(message, theme::STATUS_ERROR));
            }
            (ProbeStatus::Idle | ProbeStatus::Running | ProbeStatus::Ready(_), None) => {
                ui.label(theme::status(t!("launcher.settings.checking_status"), theme::TEXT_MUTED));
            }
        }

        if refresh {
            self.start(ui, backend);
        }
    }

    /// Starts a new probe (the previous result is dropped; a still-running worker's late
    /// answer is ignored with its receiver).
    fn start(&mut self, ui: &Ui, backend: &AiBackendHandle) {
        let request = ProbeRequest { native: backend.local_ai.native_onnx.is_ok(), torch: backend.local_ai.backend.is_ok() };
        self.copied = false;
        match spawn_probe(request, ui.ctx().clone()) {
            Ok(rx) => {
                self.status = ProbeStatus::Running;
                self.rx = Some(rx);
            }
            Err(err) => {
                runtime_log::log_error(format!("[launcher-system-info] failed to start the probe thread: {err}"));
                self.status = ProbeStatus::Error(tf!("launcher.settings.system_info.start_error", err = err));
                self.rx = None;
            }
        }
    }

    /// Takes the worker's result when it arrived. The worker requests the repaint itself, so
    /// nothing here repaints while it runs.
    fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(probe) => {
                self.status = ProbeStatus::Ready(Box::new(probe));
                self.rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error("[launcher-system-info] probe worker ended without a result");
                self.status = ProbeStatus::Error(t!("launcher.settings.system_info.no_response_error").to_string());
                self.rx = None;
            }
        }
    }
}

/// Spawns the `launcher-system-info-probe` worker. It sends one [`ProbeResult`] and requests
/// a repaint of `ctx` on EVERY exit, including a panic: the [`WakeOnDrop`] guard fires after
/// the sender is dropped, so the woken frame sees either the result or the disconnect that
/// `poll` maps to the localized "no result" error.
///
/// # Errors
/// The OS refused to create the thread.
fn spawn_probe(request: ProbeRequest, ctx: egui::Context) -> std::io::Result<Receiver<ProbeResult>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new().name("launcher-system-info-probe".to_string()).spawn(move || {
        // Declared before `tx` is rebound so it drops AFTER the sender (locals drop in reverse
        // order, on unwind too): the repaint always observes a sent result or a disconnect.
        let _wake = WakeOnDrop::new(move || ctx.request_repaint());
        let tx = tx;
        runtime_log::log_info(format!("[launcher-system-info] probe started (native: {}, torch: {})", request.native, request.torch));
        let probe = run_probe(request);
        report::log_probe_summary(&probe);
        if tx.send(probe).is_err() {
            // The tab started a newer probe (or the page closed) and dropped this receiver.
            runtime_log::log_info("[launcher-system-info] probe result dropped: the tab no longer waits for it");
        }
    })?;
    Ok(rx)
}

/// Completion guard of the probe worker: runs `wake` exactly once when dropped, on a normal
/// return and during a panic unwind alike, so the GUI is never left waiting on an idle frame.
struct WakeOnDrop<F: FnMut()> {
    wake: F,
}

impl<F: FnMut()> WakeOnDrop<F> {
    /// Arms the guard with the wake-up action (`egui::Context::request_repaint` in production).
    fn new(wake: F) -> Self {
        Self { wake }
    }
}

impl<F: FnMut()> Drop for WakeOnDrop<F> {
    fn drop(&mut self) {
        (self.wake)();
    }
}

/// The blocking probe body (worker thread only).
fn run_probe(request: ProbeRequest) -> ProbeResult {
    let facts = ms_sysprobe::system_facts::probe_system_facts();
    let native = request.native.then(probe_native);
    let torch = request.torch.then(|| TorchOffer::from_preflight(detect_torch_preflight()));
    ProbeResult { facts, native, torch }
}

/// Probes the native ONNX runtime: every catalog build's verdict and the selection the next
/// native load would resolve from `user_config` (`None`, logged, when the config is
/// unreadable).
fn probe_native() -> NativeProbe {
    let availability = NativeBuildAvailability::evaluate(&NativeHardwareFacts::probe_all());
    let selection = match ms_config::load_raw_user_settings_for_startup() {
        Ok(cfg) => {
            let report = ms_native_runtime::evaluate_native_selection(&cfg);
            Some(NativeSelection { requested: report.requested_build, effective: report.build, fallback: report.fallback })
        }
        Err(err) => {
            runtime_log::log_error(format!(
                "[launcher-system-info] could not read user config for the native ONNX selection; the row shows it as unknown; error: {err:#}"
            ));
            None
        }
    };
    NativeProbe { availability, selection }
}

#[cfg(test)]
mod tests {
    use super::WakeOnDrop;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, TryRecvError};
    use std::sync::Arc;

    /// A panicking worker still wakes the GUI once, and by then the channel already reports the
    /// disconnect `poll` turns into the localized error (the guard drops after the sender).
    #[test]
    fn wake_fires_after_disconnect_on_panic() {
        let (tx, rx) = mpsc::channel::<u32>();
        let woken = Arc::new(AtomicUsize::new(0));
        let seen_disconnect = Arc::new(AtomicUsize::new(0));
        let worker = {
            let woken = Arc::clone(&woken);
            let seen_disconnect = Arc::clone(&seen_disconnect);
            std::thread::spawn(move || {
                let _wake = WakeOnDrop::new(move || {
                    woken.fetch_add(1, Ordering::SeqCst);
                    if matches!(rx.try_recv(), Err(TryRecvError::Disconnected)) {
                        seen_disconnect.fetch_add(1, Ordering::SeqCst);
                    }
                });
                let _tx = tx;
                panic!("probe failure under test");
            })
        };
        assert!(worker.join().is_err(), "the worker must have panicked");
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        assert_eq!(seen_disconnect.load(Ordering::SeqCst), 1, "the sender must be gone when the GUI is woken");
    }

    /// A normal return wakes exactly once.
    #[test]
    fn wake_fires_once_on_normal_exit() {
        let woken = AtomicUsize::new(0);
        {
            let _wake = WakeOnDrop::new(|| {
                woken.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(woken.load(Ordering::SeqCst), 1);
    }
}
