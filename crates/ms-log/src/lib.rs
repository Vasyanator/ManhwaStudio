/*
File: crates/ms-log/src/lib.rs

Purpose:
Crate root of `ms-log` — the standalone logging and tracing layer extracted from
the ManhwaStudio binary. It owns the runtime session log (`last.log`/`previous.log`)
and the optional flat/nesting trace log (`trace-last.log`), and nothing else.

Main responsibilities:
- expose `runtime_log` (structured session logging) and `trace` (opt-in trace events);
- export the `trace_log!` / `trace_scope!` macros at the crate root via `#[macro_export]`.

Contract:
- No dependency on the application crate or its config. Log directories are passed
  in by the caller (`init_session_logs(log_dir)`, `init_trace(log_dir, enabled)`),
  so this crate never resolves paths on its own.
*/

#![warn(clippy::all)]

pub mod runtime_log;
pub mod trace;
