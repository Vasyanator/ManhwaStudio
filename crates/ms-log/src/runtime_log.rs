/*
FILE OVERVIEW: src/runtime_log.rs
Session log writer with file rotation for desktop runtime diagnostics.

Main responsibilities:
- `init_session_logs`: rotates `last.log -> previous.log`, creates new `last.log`,
  starts async writer thread and installs panic hook.
- `log_info` / `log_warn` / `log_error`: generic log entry points for Rust-side events.
- `log_ai_backend`: dedicated tag for lines coming from AI backend process manager/output.
- Internals keep file I/O off GUI thread using channel + background writer thread.
*/

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

pub fn init_session_logs(log_dir: &Path) -> Result<(), String> {
    let _guard = match INIT_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if LOG_TX.get().is_some() {
        return Ok(());
    }

    prepare_log_files(log_dir)?;
    let last_log_path = log_dir.join("last.log");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("runtime-log-writer".to_string())
        .spawn(move || run_writer(last_log_path, rx))
        .map_err(|err| format!("failed to spawn runtime log thread: {err}"))?;

    let _ = LOG_TX.set(tx);
    install_panic_hook_once();
    log_info("runtime logging started");
    Ok(())
}

pub fn log_info(message: impl AsRef<str>) {
    push("INFO", message.as_ref());
}

pub fn log_warn(message: impl AsRef<str>) {
    push("WARN", message.as_ref());
}

pub fn log_error(message: impl AsRef<str>) {
    push("ERROR", message.as_ref());
}

pub fn log_ai_backend(message: impl AsRef<str>) {
    push("AI_BACKEND", message.as_ref());
}

fn prepare_log_files(log_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(log_dir)
        .map_err(|err| format!("failed to create log dir '{}': {err}", log_dir.display()))?;
    let last_path = log_dir.join("last.log");
    let previous_path = log_dir.join("previous.log");

    match fs::remove_file(&previous_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to remove previous log '{}': {err}",
                previous_path.display()
            ));
        }
    }

    if last_path.is_file() {
        fs::rename(&last_path, &previous_path).map_err(|err| {
            format!(
                "failed to rotate log '{}' -> '{}': {err}",
                last_path.display(),
                previous_path.display()
            )
        })?;
    }

    File::create(&last_path)
        .map_err(|err| format!("failed to create '{}': {err}", last_path.display()))?;
    Ok(())
}

fn run_writer(last_log_path: PathBuf, rx: mpsc::Receiver<String>) {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&last_log_path);
    let Ok(file) = file else {
        return;
    };
    let mut writer = BufWriter::new(file);
    while let Ok(line) = rx.recv() {
        if writeln!(writer, "{line}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

fn install_panic_hook_once() {
    if PANIC_HOOK_INSTALLED.set(()).is_err() {
        return;
    }
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        log_error(format!("panic: {panic_info}"));
        previous_hook(panic_info);
    }));
}

fn push(level: &str, message: &str) {
    let line = format!("[{}] [{level}] {message}", unix_timestamp_millis());
    if let Some(tx) = LOG_TX.get() {
        let _ = tx.send(line);
    }
}

fn unix_timestamp_millis() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "0.000".to_string(),
    }
}
