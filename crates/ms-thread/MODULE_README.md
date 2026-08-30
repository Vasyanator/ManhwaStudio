# Module: crates/ms-thread

## Purpose
Cross-target thread-spawning shim. It exposes a `std::thread`-compatible surface so the
rest of the workspace can create background threads from ONE set of call sites on desktop
and in the browser.

The reason it exists: on `wasm32-unknown-unknown` — even in the atomics/threads build the
web port uses (`build-wasm.sh`, Variant A: nightly + `-Z build-std` with the atomics and
bulk-memory features) — `std::thread`'s spawn path is `unsupported` and panics at runtime.
A browser can only create a thread as a Web Worker. Routing every spawn through this crate
is what keeps a single `thread::spawn(...)` call site valid on both targets.

## Architecture
One file, no logic of its own: the crate is a re-export surface split along one `cfg`.

- Non-spawning helpers always come from `std::thread` — `sleep`, `yield_now`, `current`,
  `available_parallelism`, `Thread`, `Result`, and the scoped-thread API (`scope`, `Scope`,
  `ScopedJoinHandle`). They compile and work under the atomics build because none of them
  creates an OS thread.
- Thread CREATION is target-specific: `spawn`, `Builder` and `JoinHandle` come from
  `std::thread` on native and from `wasm_thread` (Web Workers) on `wasm32`.

`Builder` and `JoinHandle` are therefore the BACKEND's own types, not a wrapper. The shim
works because both backends offer the same shape:
`Builder::new().name(..).stack_size(..).spawn(..) -> io::Result<JoinHandle>` and
`JoinHandle::join() -> Result<T>`.

## Files and submodules
- `src/lib.rs`: the whole crate — the two `pub use` groups described above, plus the file
  header explaining the wasm rationale. Edit it only to widen the exported surface when a
  call site needs a `std::thread` item the shim does not yet re-export.
- `Cargo.toml`: no unconditional dependencies at all; `wasm_thread` (feature `es_modules`)
  is pulled in only under `cfg(target_arch = "wasm32")`.

## Contracts and invariants
- **The intended import form is `use ms_thread as thread;`.** That single line makes an
  existing `thread::spawn(...)` / `thread::Builder::new()...` body target-agnostic with no
  other change, which is why nearly every consumer file starts with exactly it.
- **A thread on a path that also runs on the web must be created through this crate.** A
  direct `std::thread::spawn` there compiles and then panics in the browser. Direct
  `std::thread` stays legitimate in code excluded from the wasm build
  (`#[cfg(not(target_arch = "wasm32"))]`, e.g. `src/ui_fonts.rs::install_with_roots`) and in
  tests.
- **`scope` is native-oriented.** It is re-exported from `std` on both targets, so it
  compiles everywhere, but a scoped `spawn` would hit std's unsupported path in the browser.
  The one consumer is the batch executor
  (`src/launcher/new_project/batch_processing/executor.rs`), which is not on the web path.
- **The crate stays dependency-free on native.** Anything heavier than a re-export belongs
  in the caller; this is a compatibility shim, not a threading library. It must keep
  compiling for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu` and
  `wasm32-unknown-unknown`.
- **Native behavior must stay byte-identical to `std::thread`.** The native arm is a plain
  re-export precisely so adopting the shim is never a behavioral change; do not introduce a
  wrapper type or extra bookkeeping on that arm.
- Serena's reference search resolves only the active target, so it cannot see the wasm arm.
  Verify anything about the `wasm32` re-exports with `grep`, not with a reference query.

## Relationships
- **Consumers:** the binary crate (76 files under `src/`, including `app.rs`,
  `config_saver.rs`, `ai_backend_supervisor.rs`, the launcher workers and the typing panel)
  and one workspace crate, `ms-log`, whose `runtime_log` and `trace` writer threads are
  started with `ms_thread::Builder`.
- **Dependencies:** none on native; `wasm_thread` on wasm. It depends on no other workspace
  crate, so it sits at the bottom of the dependency graph and anything may use it.
- CPU-bound fan-out still goes to `rayon`, not here; this crate is about creating a named,
  long-lived worker thread.

## Editing map
- To let a call site use a `std::thread` item the shim does not export yet, add it to the
  matching `pub use` group in `src/lib.rs` — the always-std group if it creates no thread,
  the cfg-split group if it does, and then check that `wasm_thread` really provides it.
- To change the wasm backend or its features, edit the
  `[target.'cfg(target_arch = "wasm32")'.dependencies]` block in `Cargo.toml`; the atomics
  build flags themselves live in `.cargo/config.toml` and the build in `build-wasm.sh`.
- To adopt the shim in a file that still calls `std::thread` on a web-reachable path,
  replace its `use std::thread;` with `use ms_thread as thread;` — the bodies stay as they
  are.
