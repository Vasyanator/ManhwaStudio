# Module: crates/ms-log

## Purpose
Standalone logging + tracing layer for ManhwaStudio, so that every crate (and the app)
can log without depending on the application crate.

## Architecture
Two independent, self-contained modules. Both are configured with an explicit log
directory supplied by the caller; the crate never reads config or resolves paths.

- Session logging (`runtime_log`) writes `last.log` (rotating the prior run to
  `previous.log`) through a background writer thread fed over an mpsc channel.
- Tracing (`trace`) is opt-in (`init_trace(dir, enabled)`); when enabled it writes
  `trace-last.log` (rotating to `trace-previous.log`) via its own writer thread.

## Files and submodules
- `src/lib.rs`: crate root; re-exports the two modules; `#[macro_export]` puts
  `trace_log!` / `trace_scope!` at the crate root (`ms_log::trace_log!`).
- `src/runtime_log.rs`: `init_session_logs`, `log_info/warn/error/ai_backend`, panic hook.
- `src/trace.rs`: `init_trace`, `emit`, `trace_enabled`, `TraceSpan`, `cat::*`, macros.

## Contracts and invariants
- Dependencies stay minimal: `web-time` (wasm-safe clock) and `ms-thread` (writer
  threads, Web Workers on wasm); no dependency on the app crate or any other `ms-*` crate besides `ms-thread`.
- Callers pass the log directory in. The crate must not call into `config` or
  discover paths itself.
- The `trace_log!` / `trace_scope!` macros use `$crate::trace::...`, so they resolve
  against `ms_log` regardless of the calling crate. Callers may re-export them
  (`pub use ms_log::{trace_log, trace_scope};`) to keep short call paths.

## Editing map
- To change session log format/rotation, see `runtime_log.rs`.
- To add a trace category or change trace output, see `trace.rs` (`cat` + `emit`).
