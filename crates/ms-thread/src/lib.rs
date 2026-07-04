/*
File: lib.rs

Purpose:
Cross-target thread-spawning shim. Exposes a `std::thread`-compatible API so the
rest of the workspace can spawn background threads with the same call sites on
desktop and in the browser.

Why:
On `wasm32-unknown-unknown` (even with the atomics build) `std::thread`'s spawn
path is `unsupported` and panics at runtime — the browser can only create threads
as Web Workers. This shim forwards to `std::thread` on native (identical
behavior) and to `wasm_thread` on wasm (Web Worker backed).

Surface (the subset the app uses):
- spawn, Builder, JoinHandle  — thread creation (backend-specific)
- sleep, yield_now, current    — non-spawning helpers (always from std)
- Result                       — `std::thread::Result` alias for `join()` returns

Notes:
Import as `use ms_thread as thread;` at a call site to make its existing
`thread::spawn(...)` / `thread::Builder::new()...` code target-agnostic with no
other change. `Builder`/`JoinHandle` are the backend's own types; both backends
provide `Builder::new().name(..).stack_size(..).spawn(..) -> io::Result<JoinHandle>`
and `JoinHandle::join() -> Result<T>`.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

// Non-spawning helpers work under std on both targets (the atomics build
// provides `sleep`; `current`/`yield_now`/`available_parallelism` never create
// OS threads). `scope`/`Scope`/`ScopedJoinHandle` are std's scoped-thread API:
// they compile everywhere and run natively; on wasm a scoped `spawn` would hit
// std's unsupported path at runtime (only the native-oriented batch executor
// uses it, and not on the web path).
pub use std::thread::{
    Result, Scope, ScopedJoinHandle, Thread, available_parallelism, current, scope, sleep,
    yield_now,
};

// Thread creation: native uses std, wasm uses Web Workers via wasm_thread.
#[cfg(not(target_arch = "wasm32"))]
pub use std::thread::{Builder, JoinHandle, spawn};

#[cfg(target_arch = "wasm32")]
pub use wasm_thread::{Builder, JoinHandle, spawn};
