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
- `AiBackendSupervisor`: owns the worker thread + probe thread; built once in `run_main`.

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
- Process manager/status/output lines are mirrored into runtime file logs via
  `crate::runtime_log`.
*/

use crate::backend_ipc;
use crate::config;
// The Python backend PROCESS (spawn/stop via `std::process` + `python_manager`) is
// native-only; the handle/snapshot types and autostart persistence stay
// target-neutral so shared UI/launcher call sites compile on wasm.
#[cfg(not(target_arch = "wasm32"))]
use crate::python_manager;
#[cfg(not(target_arch = "wasm32"))]
use crate::python_manager::ManagedPythonChild;
use crate::runtime_log;
use crate::tabs::translation::backend_health::{
    AiBackendHealthSnapshot, AiBackendProbeCommand, spawn_ai_backend_probe,
};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::fs;
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
        // Prime the global capability hints exactly as the studio used to do at
        // construction: unknown until the first honest signal, or forced
        // unavailable under --no-ai (which gates every AI capability off).
        let primed = if ai_enabled { None } else { Some(false) };
        crate::ai_backend_capabilities::set_torch_available(primed);
        crate::ai_backend_capabilities::set_backend_available(primed);
        crate::ai_backend_capabilities::set_backend_ort_available(primed);
        crate::ai_backend_capabilities::set_native_ort_available(primed);

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
                auto_start,
            ));
            #[cfg(target_arch = "wasm32")]
            let process_runtime: Option<AiBackendProcessRuntime> = {
                // Touch the otherwise-native inputs so they are not flagged unused.
                let _ = (&process_snapshot, &user_settings_file, auto_start);
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

#[cfg(not(target_arch = "wasm32"))]
fn spawn_ai_backend_process_worker(
    snapshot: Arc<Mutex<AiBackendProcessSnapshot>>,
    user_settings_file: PathBuf,
    auto_start: bool,
) -> AiBackendProcessRuntime {
    let (tx, rx) = mpsc::channel::<AiBackendProcessCommand>();
    let thread = thread::spawn(move || {
        let (output_tx, output_rx) = mpsc::channel::<AiBackendOutputEvent>();
        let mut child: Option<ManagedPythonChild> = None;

        if auto_start && let Err(err) = start_ai_backend_process(&mut child, &snapshot, &output_tx)
        {
            update_process_status(
                &snapshot,
                false,
                tf!("ai_backend.supervisor.autostart_failed_status", err = err),
            );
            append_process_log(
                &snapshot,
                tf!("ai_backend.supervisor.autostart_failed_log", err = err),
            );
        }

        let mut should_exit = false;
        while !should_exit {
            match rx.recv_timeout(AI_BACKEND_WORKER_TICK) {
                Ok(command) => {
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
    let backend_script = app_dir.join("ai_backend.py");
    if !backend_script.is_file() {
        return Err(tf!("ai_backend.supervisor.script_not_found", app_dir = app_dir.display()));
    }

    let python = python_manager::resolve_python_executable(&app_dir)?;
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
                crate::runtime_log::log_warn(format!(
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

pub fn load_ai_backend_autostart(user_settings_file: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return true;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return true;
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(AI_BACKEND_AUTOSTART_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub fn save_ai_backend_autostart(user_settings_file: &Path, enabled: bool) -> Result<(), String> {
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    general_obj.insert(AI_BACKEND_AUTOSTART_KEY.to_string(), Value::Bool(enabled));
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{WsPortLine, parse_ws_port_line};

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
