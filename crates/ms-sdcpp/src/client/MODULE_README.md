# Module: crates/ms-sdcpp/src/client

## Purpose
The app side of the sd.cpp worker: one process-wide `SdCppService` (`service()`) that runs
generations in a long-lived worker process, probes builds in short-lived ones, cancels, kills,
maps a dead worker to `WorkerCrashed`, and unloads.

## Architecture
- `mod.rs`: `SdCppService` (`generate`, `probe`, `unload`, `is_worker_running`), `service()`
  (real launcher + installed packages; registers the before-remove hook; publishes
  `status`), constants `CANCEL_GRACE` (15 s) and `SHUTDOWN_WAIT` (5 s).
- `transport.rs`: `LiveWorker` — request pipe, event reader thread, stderr drain (logged, last
  64 lines kept), crash description, shutdown / kill (also on drop).
- `spawn.rs`: `WorkerLauncher` / `WorkerPipes` / `WorkerChild` seam, `ProcessLauncher`
  (`<exe> --sdcpp-worker`, piped stdio, `CREATE_NO_WINDOW`), `describe_exit`, `WORKER_FLAG`.
- `tests.rs`: an in-process fake worker (`worker::serve` + fake backend over OS pipes).

## Contracts and invariants
- Blocking API: worker threads only. One generation at a time (`GenerationBusy`); a cancelled
  generation is `GenerationCancelled` (`Cancelled` / `Busy` are the package job's).
- Model files are validated before a worker is involved; a worker sees only valid pairs.
- Cancel: `Cancel` first, kill after `CANCEL_GRACE`; a killed or crashed worker is never reused.
  A worker that answered with a typed error stays warm.
- A write failure or EOF on the worker's pipes is a crash: `WorkerCrashed { exit, stderr_tail }`.
- `probe` never disturbs a running generation (separate worker).
- Unload = cancel, kill after `CANCEL_GRACE`, reap. The running generation is registered
  (`ActiveRun`) before validation, so `unload` / `unload_build` stop it at any stage.
  `unload_build(build)` (the before-remove hook) touches only that build and returns only when
  no worker of it is left, else `WorkerStillRunning` and `install::remove` aborts.
- Spawned workers get the app's pid (`PARENT_PID_ENV`); a worker whose parent changed before it
  armed `PR_SET_PDEATHSIG` exits. A launch that fails after `spawn` kills and reaps the child.
- A cancel is checked between image frames; a half-sent request kills the worker.

## Editing map
- New public operation: `mod.rs`, plus a request in `protocol.rs` and `worker/`.
- Process launch details (flags, environment): `spawn.rs`.
