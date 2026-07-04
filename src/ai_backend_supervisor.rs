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
- The backend speaks the framed multiplexed IPC protocol over the AF_UNIX socket
  from `backend_ipc::backend_socket_path()`; the worker passes that path to
  `ai_backend.py` via `--socket`.
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
            status: "Процесс не запущен.".to_string(),
            auto_start,
            updated_at: None,
            logs: VecDeque::new(),
        }
    }

    pub fn disabled(auto_start: bool) -> Self {
        Self {
            running: false,
            status: "Управление запуском отключено (--no-ai).".to_string(),
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
    pub fn start(ai_enabled: bool) -> Self {
        // Prime the global torch-capability hint exactly as the studio used to do
        // at construction: unknown until the first health snapshot, or forced
        // unavailable under --no-ai.
        crate::ai_backend_capabilities::set_torch_available(if ai_enabled {
            None
        } else {
            Some(false)
        });

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
                format!("Не удалось автозапустить backend: {err}"),
            );
            append_process_log(
                &snapshot,
                format!("[manager] Ошибка автозапуска backend: {err}"),
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

        stop_ai_backend_process(&mut child, &snapshot, "Приложение закрывается.");
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
                update_process_status(snapshot, false, format!("Ошибка запуска backend: {err}"));
                append_process_log(snapshot, format!("[manager] Ошибка запуска backend: {err}"));
            }
            false
        }
        AiBackendProcessCommand::Stop => {
            stop_ai_backend_process(child, snapshot, "Остановлено пользователем.");
            false
        }
        AiBackendProcessCommand::Restart => {
            stop_ai_backend_process(child, snapshot, "Перезапуск по запросу пользователя.");
            if let Err(err) = start_ai_backend_process(child, snapshot, output_tx) {
                update_process_status(
                    snapshot,
                    false,
                    format!("Ошибка перезапуска backend: {err}"),
                );
                append_process_log(
                    snapshot,
                    format!("[manager] Ошибка перезапуска backend: {err}"),
                );
            }
            false
        }
        AiBackendProcessCommand::SetAutoStart(enabled) => {
            set_autostart_value(snapshot, enabled);
            match save_ai_backend_autostart(user_settings_file, enabled) {
                Ok(()) => append_process_log(
                    snapshot,
                    format!(
                        "[settings] Автозапуск backend {}",
                        if enabled {
                            "включен"
                        } else {
                            "выключен"
                        }
                    ),
                ),
                Err(err) => append_process_log(
                    snapshot,
                    format!(
                        "[settings] Не удалось сохранить автозапуск backend ({enabled}): {err}"
                    ),
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
                update_process_status(snapshot, true, format!("Backend уже запущен (PID {pid})."));
                return Ok(());
            }
            Ok(Some(status)) => {
                append_process_log(
                    snapshot,
                    format!(
                        "[manager] Обнаружен уже завершенный backend: {}",
                        format_exit_code(status.code())
                    ),
                );
                *child = None;
            }
            Err(err) => {
                append_process_log(
                    snapshot,
                    format!("[manager] Ошибка проверки состояния процесса: {err}"),
                );
                *child = None;
            }
        }
    }

    let app_dir = config::program_dir();
    let backend_script = app_dir.join("ai_backend.py");
    if !backend_script.is_file() {
        return Err(format!(
            "в директории приложения '{}' не найден ai_backend.py",
            app_dir.display()
        ));
    }

    let python = python_manager::resolve_python_executable(&app_dir)?;
    // The backend listens on a fixed AF_UNIX socket (no free-port reservation).
    // `--socket` is optional on the Python side and defaults to the same path;
    // we pass it explicitly so a non-default build still agrees with the client.
    let socket_path = backend_ipc::backend_socket_path();
    let socket_arg = socket_path.to_string_lossy().to_string();
    let mut command = python_manager::build_python_command(&app_dir)?;
    command
        .current_dir(&app_dir)
        .env("PYTHONUNBUFFERED", "1")
        .arg("-u")
        .arg("ai_backend.py")
        .arg("--socket")
        .arg(&socket_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    python_manager::apply_windows_no_window(&mut command);

    let mut spawned = python_manager::spawn_kill_with_parent(command).map_err(|err| {
        format!(
            "не удалось запустить backend через '{}' ({})",
            python.display(),
            err
        )
    })?;

    let pid = spawned.id();
    let stdout = spawned.stdout.take();
    let stderr = spawned.stderr.take();
    *child = Some(spawned);

    update_process_status(snapshot, true, format!("Backend запущен (PID {pid})."));
    append_process_log(
        snapshot,
        format!(
            "[manager] Запуск ai_backend.py через '{}' (cwd '{}', pid {pid}, socket {socket_arg})",
            python.display(),
            app_dir.display()
        ),
    );

    if let Some(stdout) = stdout {
        spawn_backend_output_reader("stdout", stdout, output_tx.clone());
    } else {
        append_process_log(snapshot, "[manager] stdout backend недоступен.".to_string());
    }
    if let Some(stderr) = stderr {
        spawn_backend_output_reader("stderr", stderr, output_tx.clone());
    } else {
        append_process_log(snapshot, "[manager] stderr backend недоступен.".to_string());
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
            "Процесс backend уже остановлен.".to_string(),
        );
        return;
    };

    let pid = running_child.id();
    append_process_log(
        snapshot,
        format!("[manager] Остановка backend (pid {pid}): {reason}"),
    );

    if let Err(err) = running_child.kill() {
        append_process_log(
            snapshot,
            format!("[manager] Не удалось отправить kill backend (pid {pid}): {err}"),
        );
    }

    match running_child.wait() {
        Ok(status) => {
            update_process_status(
                snapshot,
                false,
                format!(
                    "Backend остановлен (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                format!("Backend завершился с ошибкой ожидания (pid {pid}): {err}"),
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
                format!(
                    "Backend завершился (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
            append_process_log(
                snapshot,
                format!(
                    "[manager] Backend завершился сам (pid {pid}, {}).",
                    format_exit_code(status.code())
                ),
            );
            *child = None;
        }
        Err(err) => {
            update_process_status(
                snapshot,
                false,
                format!("Ошибка проверки backend-процесса: {err}"),
            );
            append_process_log(
                snapshot,
                format!("[manager] Ошибка try_wait backend-процесса: {err}"),
            );
            *child = None;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_backend_output_reader<R: std::io::Read + Send + 'static>(
    stream_name: &'static str,
    stream: R,
    tx: Sender<AiBackendOutputEvent>,
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
                append_process_log(snapshot, format!("[manager] Ошибка чтения {stream}: {err}"));
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
    code.map(|value| format!("код выхода {value}"))
        .unwrap_or_else(|| "завершён сигналом".to_string())
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
