/*
FILE OVERVIEW: src/trace.rs
Opt-in detailed execution tracing, gated behind the `--trace` CLI flag.

Main responsibilities:
- `init_trace`: when enabled, rotates `trace-last.log -> trace-previous.log`,
  creates a fresh `trace-last.log`, spawns an async writer thread (mirroring the
  design of `runtime_log.rs`) and flips the global enabled flag. When disabled it
  is a cheap no-op and tracing stays off.
- `trace_enabled`: inline atomic check; near-zero cost when tracing is off.
- `mod cat`: the set of trace categories (`&'static str`) used to tag events.
- `trace_log!` / `trace_scope!`: the two macros instrumenters call (see below).
- Internals keep file I/O off the calling thread via channel + background writer,
  and track per-thread span depth for indented, visually-nested output.

USAGE FOR INSTRUMENTERS
-----------------------
Both macros are `#[macro_export]`, so call them fully-qualified from anywhere in
the crate:

    crate::trace_log!(crate::trace::cat::LAYER_MODEL, "add_node page={} uid={}", page, uid);

    // RAII span: MUST be bound to a variable, otherwise it drops immediately and
    // the EXIT line is written with ~0µs.
    let _s = crate::trace_scope!(crate::trace::cat::RENDER, "render_text w={} h={}", w, h);

Tip: `use crate::trace::cat;` then `crate::trace_log!(cat::TYPING, "...")`.

Both macros check `trace_enabled()` BEFORE formatting their arguments, so when
`--trace` is not passed they cost a single relaxed atomic load and an early return
(the `trace_scope!` "guard" returned in that case does nothing on drop).

Categories (`crate::trace::cat::*`):
- LAYER_MODEL — layer/document model mutations (add/remove/reorder nodes, etc.).
- PS_EDITOR   — Photoshop-like editor operations and state changes.
- TYPING      — text input / typing pipeline.
- RENDER      — rendering passes (text-to-image, compositing, baking).
- SYNC        — synchronization between models / threads / views.
- INPUT       — raw user input events (mouse/keyboard/pointer).
- PERSIST     — persistence (load/save/serialize of documents & layer models).
- FRAME       — per-frame GUI loop markers.
- STARTUP     — application startup / initialization steps.
*/

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static TRACE_TX: OnceLock<Sender<String>> = OnceLock::new();
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static INIT_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static SPAN_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Trace categories used to tag events. Stable `&'static str` identifiers.
/// `allow(dead_code)`: these are the public vocabulary for instrumenters; not
/// every category is referenced yet while instrumentation is being rolled out.
#[allow(dead_code)]
pub mod cat {
    pub const LAYER_MODEL: &str = "LAYER_MODEL";
    pub const PS_EDITOR: &str = "PS_EDITOR";
    pub const TYPING: &str = "TYPING";
    pub const RENDER: &str = "RENDER";
    pub const SYNC: &str = "SYNC";
    pub const INPUT: &str = "INPUT";
    pub const PERSIST: &str = "PERSIST";
    pub const FRAME: &str = "FRAME";
    pub const STARTUP: &str = "STARTUP";
}

/// Initialize tracing. When `enabled` is false this is a cheap no-op and tracing
/// stays disabled. When true: rotates trace logs, spawns the writer thread and
/// flips the global enabled flag.
pub fn init_trace(log_dir: &Path, enabled: bool) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }

    let _guard = match INIT_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if TRACE_TX.get().is_some() {
        TRACE_ENABLED.store(true, Ordering::Relaxed);
        return Ok(());
    }

    prepare_trace_files(log_dir)?;
    let last_path = log_dir.join("trace-last.log");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("trace-writer".to_string())
        .spawn(move || run_writer(last_path, rx))
        .map_err(|err| format!("failed to spawn trace writer thread: {err}"))?;

    let _ = TRACE_TX.set(tx);
    TRACE_ENABLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Fast inline check of whether tracing is active. Cheap when disabled.
#[inline(always)]
pub fn trace_enabled() -> bool {
    TRACE_ENABLED.load(Ordering::Relaxed)
}

/// Internal: emit a fully-formatted trace event at the current span depth.
/// Should only be reached when tracing is enabled (macros gate on
/// `trace_enabled()` first), but it re-checks defensively.
#[doc(hidden)]
pub fn emit(category: &str, message: &str) {
    let Some(tx) = TRACE_TX.get() else {
        return;
    };
    let depth = SPAN_DEPTH.with(|d| d.get());
    let indent = "  ".repeat(depth);
    let thread = std::thread::current();
    let tid = format!("{:?}", thread.id());
    let tname = thread.name().unwrap_or("unnamed");
    let line = format!(
        "[{}] [T{tid} {tname}] [{category}] {indent}{message}",
        unix_timestamp_micros()
    );
    let _ = tx.send(line);
}

/// RAII guard returned by `trace_scope!`. On creation it logs `ENTER <name>` and
/// increments the per-thread depth; on drop it decrements depth and logs
/// `EXIT <name> (<duration>µs)`. The disabled variant carries no data and does
/// nothing on drop.
#[doc(hidden)]
#[allow(dead_code)] // consumed by `trace_scope!` once instrumenters add spans
pub struct TraceSpan {
    inner: Option<TraceSpanInner>,
}

#[allow(dead_code)]
struct TraceSpanInner {
    category: &'static str,
    name: String,
    start: Instant,
}

#[allow(dead_code)] // called by `trace_scope!` once instrumenters add spans
impl TraceSpan {
    #[inline]
    pub fn disabled() -> Self {
        TraceSpan { inner: None }
    }

    pub fn enter(category: &'static str, name: String) -> Self {
        emit(category, &format!("ENTER {name}"));
        SPAN_DEPTH.with(|d| d.set(d.get() + 1));
        TraceSpan {
            inner: Some(TraceSpanInner {
                category,
                name,
                start: Instant::now(),
            }),
        }
    }
}

impl Drop for TraceSpan {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let micros = inner.start.elapsed().as_micros();
            SPAN_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            emit(
                inner.category,
                &format!("EXIT {} ({}µs)", inner.name, micros),
            );
        }
    }
}

fn prepare_trace_files(log_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(log_dir)
        .map_err(|err| format!("failed to create trace dir '{}': {err}", log_dir.display()))?;
    let last_path = log_dir.join("trace-last.log");
    let previous_path = log_dir.join("trace-previous.log");

    match fs::remove_file(&previous_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to remove previous trace log '{}': {err}",
                previous_path.display()
            ));
        }
    }

    if last_path.is_file() {
        fs::rename(&last_path, &previous_path).map_err(|err| {
            format!(
                "failed to rotate trace log '{}' -> '{}': {err}",
                last_path.display(),
                previous_path.display()
            )
        })?;
    }

    File::create(&last_path)
        .map_err(|err| format!("failed to create '{}': {err}", last_path.display()))?;
    Ok(())
}

fn run_writer(last_path: PathBuf, rx: mpsc::Receiver<String>) {
    let file = OpenOptions::new().create(true).append(true).open(&last_path);
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

fn unix_timestamp_micros() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:06}", duration.as_secs(), duration.subsec_micros()),
        Err(_) => "0.000000".to_string(),
    }
}

/// Emit a flat trace event. Checks `trace_enabled()` BEFORE formatting args, so
/// it is near-free when tracing is disabled.
///
/// `crate::trace_log!(crate::trace::cat::RENDER, "blit x={} y={}", x, y);`
#[macro_export]
macro_rules! trace_log {
    ($cat:expr, $($arg:tt)*) => {{
        if $crate::trace::trace_enabled() {
            $crate::trace::emit($cat, &format!($($arg)*));
        }
    }};
}

/// Open a timed, nesting-aware RAII span. MUST be bound to a variable:
/// `let _s = crate::trace_scope!(crate::trace::cat::RENDER, "name n={}", n);`
/// When tracing is disabled it returns a no-op guard without formatting args.
#[macro_export]
macro_rules! trace_scope {
    ($cat:expr, $($arg:tt)*) => {{
        if $crate::trace::trace_enabled() {
            $crate::trace::TraceSpan::enter($cat, format!($($arg)*))
        } else {
            $crate::trace::TraceSpan::disabled()
        }
    }};
}
