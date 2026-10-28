# Module: crates/ms-sdcpp/src/worker

## Purpose
The sd.cpp worker PROCESS: `<app exe> --sdcpp-worker` (routed by `src/startup/`) runs
`run_worker_main`, loads an installed sd.cpp package and serves the app's requests over
stdin / stdout frames (`crate::protocol`). A library abort ends only this process.

## Architecture
- `mod.rs`: `run_worker_main` (frame stdout, parent-death signal plus the parent-pid check,
  `GGML_BACKEND_PATH` removed, then `serve`), `serve` (reader thread + main loop), `SdBackend` / `Canceller` (the library
  seam), `EventSink` (the one serialized frame writer), test doubles under `fake`.
- `session.rs`: `RealBackend` over `ffi::api::SdApi` — global log / progress callbacks, one warm
  context keyed by (files, context options), `generate_image`, results freed by the library.
- `stdio.rs`: OS setup before any library is loaded (stdout -> stderr, Linux
  `PR_SET_PDEATHSIG`, Windows `SetDllDirectoryW`).

## Contracts and invariants
- The original stdout carries ONLY frames; fd 1 / `STD_OUTPUT_HANDLE` point at stderr before the
  library is loaded. The worker never opens the session log; stderr is drained by the client.
- The reader thread answers `Cancel` immediately and resets the canceller as it forwards a
  `Generate` (stream order). `Shutdown` / EOF cancel the running work and end the main loop.
- A `Generate` is followed by exactly its announced `Image` frames. The result size is checked
  against the request (`OutputSize`).
- Context options map like the official CLI (`te=cpu` prefix, `*=cpu` params backend,
  `eager_load` + `auto_fit` on); a named device must be listed by `sd_list_devices`.
- C callbacks run inside `catch_unwind`; their state is leaked once (the library keeps it).

## Editing map
- New request: `serve` (+ `protocol.rs`), then the `SdBackend` method and both backends.
- New context / generation option: `session.rs` (`ensure_ctx` / `generate`).
