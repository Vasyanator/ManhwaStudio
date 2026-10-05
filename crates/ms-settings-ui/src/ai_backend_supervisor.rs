/*
FILE OVERVIEW: src/ai_backend_supervisor.rs
App-global supervisor that owns the lifecycle of the single Python AI backend
process (`ai_backend.py`) and the shared health/device probe.

Why this lives above launcher and studio:
`run_main()` runs launcher and studio as separate, sequential `eframe::run_native`
windows. Previously the backend process was owned by the studio's settings tab and
killed when the studio window closed, so it died on every studio->launcher
transition and could never be used from the launcher. The supervisor is created
once in `run_main()` before the launcher/studio loop and shut down only when the
whole app exits, so the backend persists across transitions and both UIs can drive
it through a cloneable [`AiBackendHandle`].

Main types:
- `AiBackendProcessSnapshot`: thread-safe process state (running/status/logs/autostart).
- `AiBackendProcessCommand`: control messages for the process worker thread.
- `AiBackendHandle`: cloneable handle the launcher and studio settings UIs talk to
  (process control + health snapshot + device-probe commands).
- `AiBackendSupervisor`: owns the worker thread + probe thread; built once in `run_main`
  (`start_with_autostart_gate` there, so autostart waits for storage-mode conversions).
- `AutostartGate` / `AutostartDeferral`: the one-shot, gate-deferred autostart polled on the
  worker tick; a user Start/Restart/Stop or autostart-off cancels it.
- `check_backend_spawnable` / `BackendSpawnBlocker` (native-only): the spawn preconditions,
  silent, shared by the spawn and by inspection callers (settings checks).

Notes:
- The backend speaks the framed multiplexed IPC protocol over a per-platform
  transport (see `backend_ipc::transport`):
    * unix: AF_UNIX socket from `backend_ipc::backend_socket_path()`; the worker
      launches `ai_backend.py` with `--socket <path>` and the client dials the
      same path.
    * windows: loopback WebSocket. The worker mints a random token and launches
      the backend with `--transport ws --ws-port 0 --ws-token <token>`; the child
      binds an ephemeral 127.0.0.1 port and prints exactly one
      `MS_BACKEND_WS_PORT=<port>` line to stdout. The stdout reader parses that
      line and publishes the endpoint via `backend_ipc::set_ws_endpoint(port,
      token)`, which `current_backend_endpoint()` then hands to the client. The
      token is never written to any log.
- `start_ai_backend_process` is the single spawn point; its preconditions (script present,
  payload with `docstore.py` per KG-017, a resolvable interpreter) are owned by
  `check_backend_spawnable` / `BackendSpawnBlocker`, which inspection callers also use.
- Process manager/status/output lines are mirrored into runtime file logs via
  `ms_log::runtime_log`.
*/

use ms_backend_ipc as backend_ipc;
use ms_config as config;
// The Python backend PROCESS (spawn/stop via `std::process` + `python_manager`) is
// native-only; the handle/snapshot types and autostart persistence stay
// target-neutral so shared UI/launcher call sites compile on wasm.
#[cfg(not(target_arch = "wasm32"))]
use ms_sysprobe::python_manager;
#[cfg(not(target_arch = "wasm32"))]
use ms_sysprobe::python_manager::ManagedPythonChild;
use ms_log::runtime_log;
use ms_tab_translation::backend_health::{
    AiBackendHealthSnapshot, AiBackendProbeCommand, spawn_ai_backend_probe,
};
use serde_json::Value;
use std::collections::VecDeque;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use ms_thread::{self as thread, JoinHandle};
use web_time::{Duration, Instant};

/// Process-wide handle to the app-global backend, published once the supervisor
/// starts. Lets code far from `run_main` (e.g. the launcher's browser downloader)
/// request a backend start without threading the handle through every call site.
static GLOBAL_HANDLE: OnceLock<AiBackendHandle> = OnceLock::new();

/// The app-global backend handle, if the supervisor has started. `None` only
/// before `AiBackendSupervisor::start` runs (or in tooling that never starts it).
pub fn global_handle() -> Option<AiBackendHandle> {
    GLOBAL_HANDLE.get().cloned()
}

pub const AI_BACKEND_LOG_LIMIT: usize = 400;
pub const AI_BACKEND_WORKER_TICK: Duration = Duration::from_millis(150);
pub const AI_BACKEND_AUTOSTART_KEY: &str = "ai_backend_autostart";

#[derive(Debug, Clone)]
pub struct AiBackendProcessSnapshot {
    running: bool,
    status: String,
    auto_start: bool,
    updated_at: Option<Instant>,
    logs: VecDeque<String>,
}

impl AiBackendProcessSnapshot {
    pub fn new(auto_start: bool) -> Self {
        Self {
            running: false,
            status: t!("ai_backend.supervisor.process_not_started").to_string(),
            auto_start,
            updated_at: None,
            logs: VecDeque::new(),
        }
    }

    pub fn disabled(auto_start: bool) -> Self {
        Self {
            running: false,
            status: t!("ai_backend.supervisor.launch_disabled_no_ai").to_string(),
            auto_start,
            updated_at: None,
            logs: VecDeque::new(),
        }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn auto_start(&self) -> bool {
        self.auto_start
    }

    pub fn updated_at(&self) -> Option<Instant> {
        self.updated_at
    }

    pub fn logs(&self) -> &VecDeque<String> {
        &self.logs
    }
}

#[derive(Debug)]
struct AiBackendProcessRuntime {
    tx: Sender<AiBackendProcessCommand>,
    thread: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub enum AiBackendProcessCommand {
    Start,
    Stop,
    Restart,
    SetAutoStart(bool),
    Shutdown,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum AiBackendOutputEvent {
    Stdout(String),
    Stderr(String),
    StreamError(&'static str, String),
    /// The backend printed a `MS_BACKEND_WS_PORT=` line whose value did not parse
    /// as a port. Carries the offending raw line (never the auth token) so the
    /// worker can log a structured warning without aborting the process.
    WsPortParseError(String),
}

/// Describes how the just-spawned backend is reached, computed once at the spawn
/// site so the argv and the stdout-parsing hookup stay in sync.
///
/// The variants are platform-gated because the two supported targets use
/// different transports: AF_UNIX on unix, loopback WebSocket on windows.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum BackendLaunch {
    /// Launched with `--socket <socket_arg>`; the client dials the same AF_UNIX
    /// path. No stdout port line is expected on this transport.
    #[cfg(unix)]
    Unix { socket_arg: String },
    /// Launched with `--transport ws --ws-port 0 --ws-token <token>`; the client
    /// dials the ephemeral port parsed from the child's `MS_BACKEND_WS_PORT=` line.
    #[cfg(windows)]
    Ws { token: String },
}

/// Result of scanning one backend stdout line for the loopback-WS port marker.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq)]
enum WsPortLine {
    /// The line is not a `MS_BACKEND_WS_PORT=` marker line.
    NotPortLine,
    /// The marker is present but its value is not a valid `u16` port.
    Malformed,
    /// A valid loopback port parsed from the marker line.
    Port(u16),
}

/// Parses one backend stdout line for the `MS_BACKEND_WS_PORT=<digits>` marker the
/// Python backend prints exactly once when its loopback WebSocket server is
/// listening.
///
/// Returns [`WsPortLine::NotPortLine`] for any line without the marker prefix,
/// [`WsPortLine::Malformed`] when the marker is present but the value does not
/// parse as a `u16`, and [`WsPortLine::Port`] with the parsed port otherwise.
/// Uses checked `str::parse` (no lossy casts) and never panics.
#[cfg(not(target_arch = "wasm32"))]
fn parse_ws_port_line(line: &str) -> WsPortLine {
    const MARKER: &str = "MS_BACKEND_WS_PORT=";
    let Some(value) = line.strip_prefix(MARKER) else {
        return WsPortLine::NotPortLine;
    };
    match value.trim().parse::<u16>() {
        Ok(port) => WsPortLine::Port(port),
        Err(_) => WsPortLine::Malformed,
    }
}

/// Cloneable handle the launcher and studio settings UIs use to observe and drive
/// the app-global backend. Cloning shares the same underlying snapshots/channels.
#[derive(Debug, Clone)]
pub struct AiBackendHandle {
    pub ai_enabled: bool,
    pub health: Arc<Mutex<AiBackendHealthSnapshot>>,
    pub probe_tx: Option<Sender<AiBackendProbeCommand>>,
    pub process_snapshot: Arc<Mutex<AiBackendProcessSnapshot>>,
    pub process_tx: Option<Sender<AiBackendProcessCommand>>,
}

impl AiBackendHandle {
    /// An inert handle for `--no-ai` / default contexts: no worker, no probe.
    pub fn disabled() -> Self {
        Self {
            ai_enabled: false,
            health: Arc::new(Mutex::new(AiBackendHealthSnapshot::disabled())),
            probe_tx: None,
            process_snapshot: Arc::new(Mutex::new(AiBackendProcessSnapshot::disabled(false))),
            process_tx: None,
        }
    }

    pub fn send_probe(&self, command: AiBackendProbeCommand) {
        if let Some(tx) = self.probe_tx.as_ref() {
            let _ = tx.send(command);
        }
    }

    pub fn send_process(&self, command: AiBackendProcessCommand) {
        if let Some(tx) = self.process_tx.as_ref() {
            let _ = tx.send(command);
        }
    }

    pub fn process_snapshot(&self) -> AiBackendProcessSnapshot {
        match self.process_snapshot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn health_snapshot(&self) -> AiBackendHealthSnapshot {
        match self.health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Owns the worker + probe threads. Built once in `run_main()` and torn down via
/// [`AiBackendSupervisor::shutdown`] when the whole app exits.
pub struct AiBackendSupervisor {
    handle: AiBackendHandle,
    process_runtime: Option<AiBackendProcessRuntime>,
    probe_thread: Option<JoinHandle<()>>,
}

impl AiBackendSupervisor {
    /// Starts the supervisor. When `ai_enabled`, spawns the process worker
    /// (auto-starting the backend if the persisted autostart toggle is on) and the
    /// health/device probe. When disabled (`--no-ai`), everything stays inert.
    ///
    /// Also primes the process-global AI capability hints (backend / torch /
    /// onnxruntime) with the same policy: `None` (unknown until the first honest
    /// signal — health snapshot for backend/torch/backend-ORT, route-inputs refresh
    /// for native-ORT) when `ai_enabled`, or forced `Some(false)` under `--no-ai`.
    pub fn start(ai_enabled: bool) -> Self {
        Self::start_with_autostart_gate(ai_enabled, None)
    }

    /// [`Self::start`] with a deferred-autostart gate. When the persisted autostart toggle
    /// is on, the worker starts the backend on the FIRST worker tick
    /// ([`AI_BACKEND_WORKER_TICK`]) on which `gate` returns `true` (immediately when `gate`
    /// is `None` or already true). The gate runs on the worker thread, never on the GUI
    /// thread, and must be cheap and non-blocking. A user Start/Restart/Stop (or turning
    /// autostart off) while the autostart is still deferred cancels it: user intent wins.
    pub fn start_with_autostart_gate(ai_enabled: bool, gate: Option<AutostartGate>) -> Self {
        // Prime the global capability hints exactly as the studio used to do at
        // construction: unknown until the first honest signal, or forced
        // unavailable under --no-ai (which gates every AI capability off).
        let primed = if ai_enabled { None } else { Some(false) };
        ms_sysprobe::ai_backend_capabilities::set_torch_available(primed);
        ms_sysprobe::ai_backend_capabilities::set_backend_available(primed);
        ms_sysprobe::ai_backend_capabilities::set_backend_ort_available(primed);
        ms_sysprobe::ai_backend_capabilities::set_native_ort_available(primed);

        let user_settings_file = config::user_config_path();
        let auto_start = load_ai_backend_autostart(&user_settings_file);

        let health = Arc::new(Mutex::new(if ai_enabled {
            AiBackendHealthSnapshot::default()
        } else {
            AiBackendHealthSnapshot::disabled()
        }));
        let process_snapshot = Arc::new(Mutex::new(if ai_enabled {
            AiBackendProcessSnapshot::new(auto_start)
        } else {
            AiBackendProcessSnapshot::disabled(auto_start)
        }));

        let (probe_tx, probe_thread, process_runtime) = if ai_enabled {
            let (probe_tx, probe_thread) = spawn_ai_backend_probe(Arc::clone(&health));
            // The OS backend process is native-only. On web there is no process to
            // spawn (the whole AI backend is compiled out), so the supervisor keeps a
            // probe channel but no process runtime.
            #[cfg(not(target_arch = "wasm32"))]
            let process_runtime = Some(spawn_ai_backend_process_worker(
                Arc::clone(&process_snapshot),
                user_settings_file,
                AutostartDeferral::new(auto_start, gate),
            ));
            #[cfg(target_arch = "wasm32")]
            let process_runtime: Option<AiBackendProcessRuntime> = {
                // Touch the otherwise-native inputs so they are not flagged unused.
                let _ = (&process_snapshot, &user_settings_file, auto_start, gate);
                None
            };
            (Some(probe_tx), Some(probe_thread), process_runtime)
        } else {
            (None, None, None)
        };

        let handle = AiBackendHandle {
            ai_enabled,
            health,
            probe_tx,
            process_snapshot,
            process_tx: process_runtime.as_ref().map(|runtime| runtime.tx.clone()),
        };

        // Publish the handle process-wide (first start wins; ignored on re-entry).
        let _ = GLOBAL_HANDLE.set(handle.clone());

        Self {
            handle,
            process_runtime,
            probe_thread,
        }
    }

    pub fn handle(&self) -> AiBackendHandle {
        self.handle.clone()
    }
}

/// Stops the backend process, stops the probe, and joins both threads when the
/// supervisor goes out of scope (i.e. when the whole app exits). Implemented as
/// `Drop` so it runs on every `run_main` return path, including the early returns
/// inside the launcher/studio loop.
impl Drop for AiBackendSupervisor {
    fn drop(&mut self) {
        if let Some(runtime) = self.process_runtime.take() {
            let _ = runtime.tx.send(AiBackendProcessCommand::Shutdown);
            let _ = runtime.thread.join();
        }
        if let Some(tx) = self.handle.probe_tx.as_ref() {
            let _ = tx.send(AiBackendProbeCommand::Stop);
        }
        if let Some(thread) = self.probe_thread.take() {
            let _ = thread.join();
        }
    }
}

/// Deferred-autostart gate for [`AiBackendSupervisor::start_with_autostart_gate`]:
/// returns `true` once the backend may autostart. Polled on the supervisor worker.
pub type AutostartGate = Box<dyn Fn() -> bool + Send>;

/// What one [`AutostartDeferral::poll`] decided.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutostartPoll {
    /// Nothing to do: autostart is off, already fired, or was cancelled.
    Idle,
    /// Autostart is armed but the gate is still closed.
    Deferred,
    /// Start the backend now (returned at most once).
    Fire,
}

/// The worker's one-shot autostart decision: armed when the persisted toggle is on, fires
/// on the first poll where the gate (if any) is open, and can be cancelled by user intent.
#[cfg(not(target_arch = "wasm32"))]
struct AutostartDeferral {
    /// `true` while the autostart has neither fired nor been cancelled.
    armed: bool,
    gate: Option<AutostartGate>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for AutostartDeferral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutostartDeferral").field("armed", &self.armed).field("gated", &self.gate.is_some()).finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl AutostartDeferral {
    /// Arms the autostart when `auto_start`; `gate == None` means "always open".
    fn new(auto_start: bool, gate: Option<AutostartGate>) -> Self {
        Self { armed: auto_start, gate }
    }

    /// Evaluates the gate; returns [`AutostartPoll::Fire`] at most once over the lifetime.
    fn poll(&mut self) -> AutostartPoll {
        if !self.armed {
            return AutostartPoll::Idle;
        }
        if self.gate.as_ref().is_none_or(|gate| gate()) {
            self.armed = false;
            // The gate is not needed any more; drop whatever it captured.
            self.gate = None;
            return AutostartPoll::Fire;
        }
        AutostartPoll::Deferred
    }

    /// Cancels a still-armed autostart; returns whether one was pending.
    fn cancel(&mut self) -> bool {
        let was_armed = self.armed;
        self.armed = false;
        self.gate = None;
        was_armed
    }
}

/// Whether a user command overrides a still-deferred autostart: an explicit start, restart
/// or stop decides the process state itself, and turning autostart off revokes it.
#[cfg(not(target_arch = "wasm32"))]
fn command_cancels_autostart(command: &AiBackendProcessCommand) -> bool {
    match command {
        AiBackendProcessCommand::Start | AiBackendProcessCommand::Stop | AiBackendProcessCommand::Restart => true,
        AiBackendProcessCommand::SetAutoStart(enabled) => !enabled,
        AiBackendProcessCommand::Shutdown => false,
    }
}

/// Runs one autostart poll on the worker: starts the backend when the gate opened, and
/// reports the deferral once (status + log) the first time the gate is found closed.
#[cfg(not(target_arch = "wasm32"))]
fn run_autostart_poll(
    deferral: &mut AutostartDeferral,
    deferral_reported: &mut bool,
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_tx: &Sender<AiBackendOutputEvent>,
) {
    match deferral.poll() {
        AutostartPoll::Idle => {}
        AutostartPoll::Deferred => {
            if !*deferral_reported {
                *deferral_reported = true;
                runtime_log::log_info("[ai-backend] autostart deferred: a storage-mode conversion is pending or running");
                update_process_status(snapshot, false, t!("ai_backend.supervisor.autostart_deferred_status").to_string());
                append_process_log(snapshot, t!("ai_backend.supervisor.autostart_deferred_log").to_string());
            }
        }
        AutostartPoll::Fire => {
            if *deferral_reported {
                runtime_log::log_info("[ai-backend] deferred autostart released: starting the backend");
            }
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(
                    snapshot,
                    false,
                    tf!("ai_backend.supervisor.autostart_failed_status", err = err),
                );
                append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.autostart_failed_log", err = err),
                );
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_ai_backend_process_worker(
    snapshot: Arc<Mutex<AiBackendProcessSnapshot>>,
    user_settings_file: PathBuf,
    mut autostart: AutostartDeferral,
) -> AiBackendProcessRuntime {
    let (tx, rx) = mpsc::channel::<AiBackendProcessCommand>();
    let thread = thread::spawn(move || {
        let (output_tx, output_rx) = mpsc::channel::<AiBackendOutputEvent>();
        let mut child: Option<ManagedPythonChild> = None;
        let mut deferral_reported = false;

        run_autostart_poll(&mut autostart, &mut deferral_reported, &mut child, &snapshot, &output_tx);

        let mut should_exit = false;
        while !should_exit {
            match rx.recv_timeout(AI_BACKEND_WORKER_TICK) {
                Ok(command) => {
                    // User intent wins over a still-deferred autostart (KG-016 keeps the
                    // residual window of a manual start during a conversion).
                    if command_cancels_autostart(&command) && autostart.cancel() && deferral_reported {
                        runtime_log::log_info("[ai-backend] deferred autostart cancelled by a user command");
                        append_process_log(&snapshot, t!("ai_backend.supervisor.autostart_cancelled_log").to_string());
                    }
                    should_exit = handle_process_command(
                        command,
                        &mut child,
                        &snapshot,
                        &output_tx,
                        &user_settings_file,
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            if !should_exit {
                run_autostart_poll(&mut autostart, &mut deferral_reported, &mut child, &snapshot, &output_tx);
            }
            drain_backend_output(&snapshot, &output_rx);
            poll_backend_exit(&snapshot, &mut child);
        }

        stop_ai_backend_process(&mut child, &snapshot, t!("ai_backend.supervisor.app_closing"));
    });

    AiBackendProcessRuntime { tx, thread }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_process_command(
    command: AiBackendProcessCommand,
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_tx: &Sender<AiBackendOutputEvent>,
    user_settings_file: &Path,
) -> bool {
    match command {
        AiBackendProcessCommand::Start => {
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(snapshot, false, tf!("ai_backend.supervisor.start_failed_status", err = err));
                append_process_log(snapshot, tf!("ai_backend.supervisor.start_failed_log", err = err));
            }
            false
        }
        AiBackendProcessCommand::Stop => {
            stop_ai_backend_process(child, snapshot, t!("ai_backend.supervisor.stopped_by_user"));
            false
        }
        AiBackendProcessCommand::Restart => {
            stop_ai_backend_process(child, snapshot, t!("ai_backend.supervisor.restart_by_user"));
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(
                    snapshot,
                    false,
                    tf!("ai_backend.supervisor.restart_failed_status", err = err),
                );
                append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.restart_failed_log", err = err),
                );
            }
            false
        }
        AiBackendProcessCommand::SetAutoStart(enabled) => {
            set_autostart_value(snapshot, enabled);
            match save_ai_backend_autostart(user_settings_file, enabled) {
                Ok(()) => {
                    // The state word is localized separately from the message
                    // template so both words live in the catalog as their own keys.
                    let state_word = if enabled {
                        t!("ai_backend.supervisor.autostart_enabled_word")
                    } else {
                        t!("ai_backend.supervisor.autostart_disabled_word")
                    };
                    append_process_log(
                        snapshot,
                        tf!("ai_backend.supervisor.autostart_changed_log", arg = state_word),
                    )
                }
                Err(err) => append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.autostart_save_failed_log", enabled = enabled, err = err),
                ),
            }
            false
        }
        AiBackendProcessCommand::Shutdown => true,
    }
}

/// A file every Python payload that speaks the document-store format ships next to
/// `ai_backend.py`; its absence identifies a pre-docstore payload.
#[cfg(not(target_arch = "wasm32"))]
const BACKEND_PAYLOAD_MARKER: &str = "docstore.py";

/// Whether the Python payload in `app_dir` is new enough to be spawned: it must contain
/// [`BACKEND_PAYLOAD_MARKER`] (a pre-docstore `config.py` writes `user_config.json` on
/// import, which would collide with a `.db` user config — KG-017). Stats the disk: worker only.
#[cfg(not(target_arch = "wasm32"))]
fn backend_payload_is_current(app_dir: &Path) -> bool {
    app_dir.join(BACKEND_PAYLOAD_MARKER).is_file()
}

/// Why the Python backend cannot be spawned from a program directory. Produced by
/// [`check_backend_spawnable`], the one owner of the spawn preconditions.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpawnBlocker {
    /// `ai_backend.py` is not a file in the program directory.
    ScriptMissing,
    /// The Python payload predates the document store (no `docstore.py`, KG-017).
    PayloadOutdated,
    /// No supported Python environment / interpreter was found; carries the localized
    /// message of `python_manager::resolve_python_executable`.
    PythonMissing(String),
}

/// The spawn preconditions of the backend, in the order the spawn checks them:
/// `ai_backend.py` exists, the payload carries [`BACKEND_PAYLOAD_MARKER`]
/// ([`backend_payload_is_current`], KG-017), and `python_manager` resolves an interpreter
/// under `app_dir`. Silent (the caller decides what to log). Stats the disk: worker
/// threads only.
///
/// # Errors
/// The first failing precondition as a [`BackendSpawnBlocker`]. `Ok` carries the
/// interpreter the spawn would use.
#[cfg(not(target_arch = "wasm32"))]
pub fn check_backend_spawnable(app_dir: &Path) -> Result<PathBuf, BackendSpawnBlocker> {
    if !app_dir.join("ai_backend.py").is_file() {
        return Err(BackendSpawnBlocker::ScriptMissing);
    }
    if !backend_payload_is_current(app_dir) {
        return Err(BackendSpawnBlocker::PayloadOutdated);
    }
    python_manager::resolve_python_executable(app_dir).map_err(BackendSpawnBlocker::PythonMissing)
}

/// Spawns the backend. The single spawn point: autostart, Start and Restart all route here,
/// so the spawn preconditions ([`check_backend_spawnable`]) cover every launch.
#[cfg(not(target_arch = "wasm32"))]
fn start_ai_backend_process(
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_tx: &Sender<AiBackendOutputEvent>,
) -> Result<(), String> {
    if let Some(existing) = child.as_mut() {
        match existing.try_wait() {
            Ok(None) => {
                let pid = existing.id();
                update_process_status(snapshot, true, tf!("ai_backend.supervisor.already_running", pid = pid));
                return Ok(());
            }
            Ok(Some(status)) => {
                append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.detected_exited_log", format_exit_code = format_exit_code(status.code())),
                );
                *child = None;
            }
            Err(err) => {
                append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.state_check_failed_log", err = err),
                );
                *child = None;
            }
        }
    }

    let app_dir = config::program_dir();
    let python = match check_backend_spawnable(&app_dir) {
        Ok(python) => python,
        Err(BackendSpawnBlocker::ScriptMissing) => {
            return Err(tf!("ai_backend.supervisor.script_not_found", app_dir = app_dir.display()));
        }
        // KG-017: a payload older than the storage-mode feature (an exe-only replacement
        // keeps the old Python files) recreates `user_config.json` on import next to
        // `user_config.db`, before the IPC version check could refuse it. Refuse to spawn it
        // at all.
        Err(BackendSpawnBlocker::PayloadOutdated) => {
            runtime_log::log_error(format!(
                "[ai-backend] refusing to start the backend: the Python payload predates the document store; app_dir={}; missing={BACKEND_PAYLOAD_MARKER}; possible cause: only the executable was replaced during an update",
                app_dir.display()
            ));
            return Err(tf!("ai_backend.supervisor.payload_outdated", app_dir = app_dir.display()));
        }
        Err(BackendSpawnBlocker::PythonMissing(message)) => return Err(message),
    };
    let mut command = python_manager::build_python_command(&app_dir)?;
    command
        .current_dir(&app_dir)
        .env("PYTHONUNBUFFERED", "1")
        .arg("-u")
        .arg("ai_backend.py");

    // Transport differs by platform. Build the argv and remember how to reach the
    // child in one place (`BackendLaunch`), so the stdout-parsing hookup below
    // cannot drift from the arguments we actually passed.
    #[cfg(unix)]
    let launch = {
        // The backend listens on a fixed AF_UNIX socket (no free-port reservation).
        // `--socket` is optional on the Python side and defaults to the same path;
        // we pass it explicitly so a non-default build still agrees with the client.
        let socket_arg = backend_ipc::backend_socket_path()
            .to_string_lossy()
            .to_string();
        command.arg("--socket").arg(&socket_arg);
        BackendLaunch::Unix { socket_arg }
    };
    #[cfg(windows)]
    let launch = {
        // AF_UNIX is unreliable on windows here, so the backend serves the same
        // framed protocol over a loopback WebSocket. Mint a fresh random token per
        // launch; the child binds an ephemeral 127.0.0.1 port (`--ws-port 0`) and
        // reports it back on stdout. The token authenticates the client handshake
        // and is never logged.
        let token = uuid::Uuid::new_v4().to_string();
        command
            .arg("--transport")
            .arg("ws")
            .arg("--ws-port")
            .arg("0")
            .arg("--ws-token")
            .arg(&token);
        BackendLaunch::Ws { token }
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    python_manager::apply_windows_no_window(&mut command);

    let mut spawned = python_manager::spawn_kill_with_parent(command).map_err(|err| {
        tf!("ai_backend.supervisor.spawn_failed", python = python.display(), err = err)
    })?;

    let pid = spawned.id();
    let stdout = spawned.stdout.take();
    let stderr = spawned.stderr.take();
    *child = Some(spawned);

    // Derive the stdout-reader token and a log-safe transport description from the
    // single `launch` value. The token is paired with the parsed port on windows;
    // the description NEVER contains the token.
    let (ws_token, transport_desc): (Option<String>, String) = match &launch {
        #[cfg(unix)]
        BackendLaunch::Unix { socket_arg } => (None, format!("socket {socket_arg}")),
        #[cfg(windows)]
        BackendLaunch::Ws { token } => (
            Some(token.clone()),
            "transport ws (loopback, ephemeral port)".to_string(),
        ),
    };

    update_process_status(snapshot, true, tf!("ai_backend.supervisor.started_status", pid = pid));
    append_process_log(
        snapshot,
        tf!("ai_backend.supervisor.starting_log", python = python.display(), app_dir = app_dir.display(), pid = pid, transport_desc = transport_desc),
    );

    if let Some(stdout) = stdout {
        // Only the stdout reader watches for the WS port line; move the token in so
        // it can publish the endpoint once the backend reports its port.
        spawn_backend_output_reader("stdout", stdout, output_tx.clone(), ws_token);
    } else {
        append_process_log(snapshot, t!("ai_backend.supervisor.stdout_unavailable_log").to_string());
    }
    if let Some(stderr) = stderr {
        spawn_backend_output_reader("stderr", stderr, output_tx.clone(), None);
    } else {
        append_process_log(snapshot, t!("ai_backend.supervisor.stderr_unavailable_log").to_string());
    }

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn stop_ai_backend_process(
    child: &mut Option<ManagedPythonChild>,
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    reason: &str,
) {
    let Some(mut running_child) = child.take() else {
        update_process_status(
            snapshot,
            false,
            t!("ai_backend.supervisor.already_stopped").to_string(),
        );
        return;
    };

    let pid = running_child.id();
    append_process_log(
        snapshot,
        tf!("ai_backend.supervisor.stopping_log", pid = pid, reason = reason),
    );

    if let Err(err) = running_child.kill() {
        append_process_log(
            snapshot,
            tf!("ai_backend.supervisor.kill_failed_log", pid = pid, err = err),
        );
    }

    match running_child.wait() {
        Ok(status) => {
            update_process_status(
                snapshot,
                false,
                tf!("ai_backend.supervisor.stopped_status", pid = pid, format_exit_code = format_exit_code(status.code())),
            );
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                tf!("ai_backend.supervisor.wait_error_status", pid = pid, err = err),
            );
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn poll_backend_exit(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    child: &mut Option<ManagedPythonChild>,
) {
    let Some(running_child) = child.as_mut() else {
        return;
    };

    match running_child.try_wait() {
        Ok(None) => {}
        Ok(Some(status)) => {
            let pid = running_child.id();
            update_process_status(
                snapshot,
                false,
                tf!("ai_backend.supervisor.exited_status", pid = pid, format_exit_code = format_exit_code(status.code())),
            );
            append_process_log(
                snapshot,
                tf!("ai_backend.supervisor.exited_self_log", pid = pid, format_exit_code = format_exit_code(status.code())),
            );
            *child = None;
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                tf!("ai_backend.supervisor.poll_error_status", err = err),
            );
            append_process_log(
                snapshot,
                tf!("ai_backend.supervisor.try_wait_error_log", err = err),
            );
            *child = None;
        }
    }
}

/// Spawns a detached reader that forwards each line of a backend stream to the
/// worker as an [`AiBackendOutputEvent`].
///
/// `ws_token` is `Some` only for the stdout reader on the loopback-WS (windows)
/// launch: when set, each stdout line is scanned for the `MS_BACKEND_WS_PORT=`
/// marker and, on a valid port, the endpoint is published via
/// [`backend_ipc::set_ws_endpoint`] using this token. The token is captured by the
/// reader closure and never sent through an event or written to a log; a malformed
/// marker line yields a [`AiBackendOutputEvent::WsPortParseError`] and does not stop
/// the reader. It is `None` for stderr and for the unix (`--socket`) launch.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_backend_output_reader<R: std::io::Read + Send + 'static>(
    stream_name: &'static str,
    stream: R,
    tx: Sender<AiBackendOutputEvent>,
    ws_token: Option<String>,
) {
    let _ = thread::Builder::new()
        .name(format!("ai-backend-{stream_name}-reader"))
        .spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line_buf = Vec::with_capacity(1024);
            loop {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        while matches!(line_buf.last(), Some(b'\n' | b'\r')) {
                            line_buf.pop();
                        }
                        let payload = String::from_utf8_lossy(&line_buf).into_owned();
                        // On the WS launch, watch stdout for the one-shot port line
                        // and publish the endpoint. `ws_token` is None on unix and
                        // for stderr, so this branch never runs there.
                        if let Some(token) = ws_token.as_ref() {
                            match parse_ws_port_line(&payload) {
                                WsPortLine::Port(port) => {
                                    // `token` stays local to this closure; only the
                                    // (non-secret) port is ever logged, by
                                    // `set_ws_endpoint`.
                                    backend_ipc::set_ws_endpoint(port, token.clone());
                                }
                                WsPortLine::Malformed => {
                                    let _ = tx.send(AiBackendOutputEvent::WsPortParseError(
                                        payload.clone(),
                                    ));
                                }
                                WsPortLine::NotPortLine => {}
                            }
                        }
                        let event = if stream_name == "stderr" {
                            AiBackendOutputEvent::Stderr(payload)
                        } else {
                            AiBackendOutputEvent::Stdout(payload)
                        };
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(AiBackendOutputEvent::StreamError(
                            stream_name,
                            err.to_string(),
                        ));
                        break;
                    }
                }
            }
        });
}

#[cfg(not(target_arch = "wasm32"))]
fn drain_backend_output(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    output_rx: &Receiver<AiBackendOutputEvent>,
) {
    while let Ok(event) = output_rx.try_recv() {
        match event {
            AiBackendOutputEvent::Stdout(line) => {
                append_process_log(snapshot, format!("[stdout] {line}"));
            }
            AiBackendOutputEvent::Stderr(line) => {
                append_process_log(snapshot, format!("[stderr] {line}"));
            }
            AiBackendOutputEvent::StreamError(stream, err) => {
                append_process_log(snapshot, tf!("ai_backend.supervisor.stream_read_error_log", stream = stream, err = err));
            }
            AiBackendOutputEvent::WsPortParseError(line) => {
                // The backend keeps running; the client simply cannot connect until a
                // valid port line arrives. The structured warning stays a stable literal
                // so logs remain grep-able across locales (logging policy); only the
                // UI-visible manager line (shown in the backend-output panel) is localized.
                ms_log::runtime_log::log_warn(format!(
                    "[ai_backend_supervisor] Не удалось разобрать порт WS backend из строки stdout: {line}"
                ));
                append_process_log(
                    snapshot,
                    tf!("ai_backend.supervisor.ws_port_parse_error_log", line = line),
                );
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn append_process_log(snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>, line: String) {
    let now = Instant::now();
    let file_log_line = line.clone();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.logs.push_back(line);
            while guard.logs.len() > AI_BACKEND_LOG_LIMIT {
                guard.logs.pop_front();
            }
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.logs.push_back(line);
            while guard.logs.len() > AI_BACKEND_LOG_LIMIT {
                guard.logs.pop_front();
            }
            guard.updated_at = Some(now);
        }
    }
    runtime_log::log_ai_backend(file_log_line);
}

#[cfg(not(target_arch = "wasm32"))]
fn update_process_status(
    snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>,
    running: bool,
    status: String,
) {
    let now = Instant::now();
    let status_for_file_log = status.clone();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.running = running;
            guard.status = status;
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.running = running;
            guard.status = status;
            guard.updated_at = Some(now);
        }
    }
    runtime_log::log_ai_backend(format!(
        "[status] {status_for_file_log} (running={running})"
    ));
}

#[cfg(not(target_arch = "wasm32"))]
fn set_autostart_value(snapshot: &Arc<Mutex<AiBackendProcessSnapshot>>, auto_start: bool) {
    let now = Instant::now();
    match snapshot.lock() {
        Ok(mut guard) => {
            guard.auto_start = auto_start;
            guard.updated_at = Some(now);
        }
        Err(mut poisoned) => {
            let guard = poisoned.get_mut();
            guard.auto_start = auto_start;
            guard.updated_at = Some(now);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn format_exit_code(code: Option<i32>) -> String {
    code.map(|value| tf!("ai_backend.supervisor.exit_code", value = value))
        .unwrap_or_else(|| t!("ai_backend.supervisor.exit_signal").to_string())
}

/// Reads `General.<AI_BACKEND_AUTOSTART_KEY>` from `user_settings_file`.
///
/// Defaults to `true` when the file is missing, unreadable, malformed, or the key is
/// absent / not a boolean. A read or parse failure is logged (the file is never
/// touched here); the read goes through the ms-config user-config reader so the
/// storage owner decides how the document is located and parsed.
pub fn load_ai_backend_autostart(user_settings_file: &Path) -> bool {
    let payload = match config::ort_load_guard::read_user_config_root(user_settings_file) {
        Ok(payload) => payload,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[ai-backend] failed to read AI backend autostart flag; path={}; using default=true; error={err}",
                user_settings_file.display()
            ));
            return true;
        }
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(AI_BACKEND_AUTOSTART_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Persists `General.<AI_BACKEND_AUTOSTART_KEY> = enabled` through the serialized
/// user-config read-modify-write ([`config::update_user_config_file`]); every other key
/// is preserved and a malformed file is never overwritten.
///
/// # Errors
/// Returns the formatted read/parse/write failure; nothing is written on error.
pub fn save_ai_backend_autostart(user_settings_file: &Path, enabled: bool) -> Result<(), String> {
    config::update_user_config_file(user_settings_file, |root| {
        let root_obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("user config root must be an object"))?;
        let general = root_obj
            .entry("General".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !general.is_object() {
            *general = Value::Object(serde_json::Map::new());
        }
        let general_obj = general
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("General must be an object"))?;
        general_obj.insert(AI_BACKEND_AUTOSTART_KEY.to_string(), Value::Bool(enabled));
        Ok(())
    })
    .map_err(|err| err.to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{
        AiBackendProcessCommand, AutostartDeferral, AutostartPoll, BackendSpawnBlocker, WsPortLine, backend_payload_is_current,
        check_backend_spawnable, command_cancels_autostart, load_ai_backend_autostart, parse_ws_port_line, save_ai_backend_autostart,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Without a gate the autostart fires on the first poll, exactly once.
    #[test]
    fn ungated_autostart_fires_once() {
        let mut deferral = AutostartDeferral::new(true, None);
        assert_eq!(deferral.poll(), AutostartPoll::Fire);
        assert_eq!(deferral.poll(), AutostartPoll::Idle);
        let mut off = AutostartDeferral::new(false, Some(Box::new(|| true)));
        assert_eq!(off.poll(), AutostartPoll::Idle, "autostart off never fires");
    }

    /// A closed gate defers; the first poll after it opens fires, and only once.
    #[test]
    fn gated_autostart_waits_for_the_gate() {
        let open = Arc::new(AtomicBool::new(false));
        let gate_open = Arc::clone(&open);
        let mut deferral = AutostartDeferral::new(true, Some(Box::new(move || gate_open.load(Ordering::SeqCst))));
        assert_eq!(deferral.poll(), AutostartPoll::Deferred);
        assert_eq!(deferral.poll(), AutostartPoll::Deferred);
        open.store(true, Ordering::SeqCst);
        assert_eq!(deferral.poll(), AutostartPoll::Fire);
        assert_eq!(deferral.poll(), AutostartPoll::Idle);
    }

    /// A user command cancels a deferred autostart; it never fires afterwards.
    #[test]
    fn user_intent_cancels_a_deferred_autostart() {
        let mut deferral = AutostartDeferral::new(true, Some(Box::new(|| false)));
        assert_eq!(deferral.poll(), AutostartPoll::Deferred);
        assert!(deferral.cancel(), "a pending autostart was cancelled");
        assert!(!deferral.cancel(), "a second cancel finds nothing pending");
        assert_eq!(deferral.poll(), AutostartPoll::Idle);

        for command in [AiBackendProcessCommand::Start, AiBackendProcessCommand::Stop, AiBackendProcessCommand::Restart, AiBackendProcessCommand::SetAutoStart(false)] {
            assert!(command_cancels_autostart(&command), "{command:?} must cancel");
        }
        assert!(!command_cancels_autostart(&AiBackendProcessCommand::SetAutoStart(true)));
        assert!(!command_cancels_autostart(&AiBackendProcessCommand::Shutdown));
    }

    /// A payload without `docstore.py` next to `ai_backend.py` is refused.
    #[test]
    fn payload_without_docstore_is_outdated() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("ai_backend.py"), b"")?;
        assert!(!backend_payload_is_current(dir.path()));
        // A directory with the marker's name is not the module.
        std::fs::create_dir(dir.path().join("docstore.py"))?;
        assert!(!backend_payload_is_current(dir.path()));
        std::fs::remove_dir(dir.path().join("docstore.py"))?;
        std::fs::write(dir.path().join("docstore.py"), b"")?;
        assert!(backend_payload_is_current(dir.path()));
        Ok(())
    }

    /// The spawn preconditions fail in the spawn's order: script, payload, interpreter.
    #[test]
    fn spawn_preconditions_report_the_first_blocker() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(check_backend_spawnable(dir.path()), Err(BackendSpawnBlocker::ScriptMissing));
        std::fs::write(dir.path().join("ai_backend.py"), b"")?;
        assert_eq!(check_backend_spawnable(dir.path()), Err(BackendSpawnBlocker::PayloadOutdated));
        std::fs::write(dir.path().join("docstore.py"), b"")?;
        // The interpreter lookup is confined to `app_dir` (venv / installer_files), so an
        // empty temp dir has none whatever Python the machine has.
        assert!(matches!(check_backend_spawnable(dir.path()), Err(BackendSpawnBlocker::PythonMissing(_))));
        Ok(())
    }

    #[test]
    fn autostart_defaults_true_and_round_trips_through_user_config() {
        let dir = std::env::temp_dir().join(format!("ms-settings-ui-p2a-autostart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("user_config.json");

        assert!(load_ai_backend_autostart(&path), "missing file defaults to true");
        std::fs::write(&path, "{not json").expect("seed broken config");
        assert!(load_ai_backend_autostart(&path), "malformed file defaults to true");

        std::fs::write(&path, r#"{"General":{"keep":"x"}}"#).expect("seed config");
        save_ai_backend_autostart(&path, false).expect("save");
        assert!(!load_ai_backend_autostart(&path));
        let root: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse back");
        assert_eq!(root["General"]["keep"], serde_json::json!("x"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// A well-formed marker line yields the parsed port.
    #[test]
    fn parses_valid_ws_port_line() {
        assert_eq!(parse_ws_port_line("MS_BACKEND_WS_PORT=54321"), WsPortLine::Port(54321));
        // Ephemeral resolution never reports 0, but 0 is still a valid u16 and must
        // not be treated as malformed.
        assert_eq!(parse_ws_port_line("MS_BACKEND_WS_PORT=0"), WsPortLine::Port(0));
    }

    /// The marker with a non-numeric or out-of-range value is malformed, not a
    /// silent 0 and not a panic.
    #[test]
    fn rejects_malformed_ws_port_line() {
        assert_eq!(parse_ws_port_line("MS_BACKEND_WS_PORT="), WsPortLine::Malformed);
        assert_eq!(parse_ws_port_line("MS_BACKEND_WS_PORT=abc"), WsPortLine::Malformed);
        // 70000 > u16::MAX: checked parse rejects it instead of truncating.
        assert_eq!(parse_ws_port_line("MS_BACKEND_WS_PORT=70000"), WsPortLine::Malformed);
    }

    /// Ordinary backend log lines without the marker are ignored.
    #[test]
    fn ignores_non_marker_lines() {
        assert_eq!(parse_ws_port_line("hello world"), WsPortLine::NotPortLine);
        // The marker must be a prefix; an embedded occurrence does not match.
        assert_eq!(
            parse_ws_port_line("info: MS_BACKEND_WS_PORT=8080"),
            WsPortLine::NotPortLine
        );
    }
}
