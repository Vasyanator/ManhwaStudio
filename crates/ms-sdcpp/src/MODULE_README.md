# Module: crates/ms-sdcpp/src

## Purpose
The app's Stable-Diffusion.cpp (sd.cpp) runtime layer: which pinned release build runs on this
machine, its installation as a pinned native package, the host requirement checks, the
process-global "is sd.cpp ready" status, and the model-family-agnostic request types that the
image-edit engines (FLUX.2 klein, FLUX.3 klein) build their sd.cpp runs on. GUI-free,
native-only (the crate is empty on wasm; on macOS / Android the platform gate refuses and the
catalog has no row). The sd.cpp library itself is never loaded into the app process: a separate
worker process (the app's own executable with `--sdcpp-worker`) loads it.

## Architecture
```
catalog.rs        PINNED release + ROWS (os, arch, build -> zip url/size/sha256, members,
                  sd / ggml library, glibc / VC++ floors, Vulkan loader need)
   |  row_here / builds_here / offers_a_build_here / default_build_here / build_from_choice(SdCppChoice)
install.rs        data_dir()/sdcpp/<build>/<tag>/  over ms_native_package
                  (marker .sdcpp-package-complete, "ms-sdcpp package v1", busy sdcpp:<build>:<tag>)
platform_check.rs glibc (gnu_get_libc_version), libvulkan.so.1 (dlopen) | System32 VC++ DLLs +
                  msvcp140.dll version, vulkan-1.dll
status.rs         process-global SdCppStatus <- one refresh worker (reads user_config read-only,
                  is_installed + check_requirements) <- publish_probe / publish_worker_running
error.rs          SdCppError (localized Display), PackageFailure
request.rs        RgbImage / GrayImage, ModelFamily, ComponentRole, ModelFiles, ContextOptions,
                  GenerateRequest, GenerateJob, RunStage / RunEvent, DeviceInfo, ProbeReport
families.rs       family -> ctx paths, required roles, text conditioning (taps x encoder hidden,
                  encoder archs); validate_model_files (DiT <-> encoder pairing) <- model_probe.rs
model_probe.rs    bounded GGUF / safetensors / index / config.json header readers
protocol.rs       app <-> worker wire over ms_backend_ipc::frame (Request / Event / WireError)
ffi/              #[repr(C)] mirrors of vendor/stable-diffusion.h + SdApi loader (worker only)
worker/           `<app exe> --sdcpp-worker`: stdout -> frames, library via ffi, serve()
client/           service(): generate / probe / unload over worker processes (spawn, cancel,
                  kill, crash -> WorkerCrashed with the stderr tail)
../examples/      sdcpp_smoke: opt-in real-library smoke (MS_SDCPP_SMOKE_* env vars)
../vendor/        the pinned header and the project LICENSE (MIT)
```
Dependencies: `ms-native-package` + `ms-download` (package transaction, HTTP), `ms-config`
(`data_dir`, `SdCppChoice`), `ms-platform` (`PlatformFeature::SdCppRuntime`), `ms-log`,
`ms-i18n`, `ms-thread`, `ms-backend-ipc` (frame codec only), `libloading`; `libc` (Linux) /
`windows-sys` (Windows). Forbidden: `ms-local-ai`
(the `--no-ai` gate is the consumers', via `AiRouteKind::SdCpp.availability`), any GUI crate.

## Files and submodules
- `lib.rs`: crate root, re-exports.
- `catalog.rs`: the pinned release and rows. Edit for a new release / build / platform; its
  header records how every pin was obtained (GitHub digest + local `sha256sum`, `unzip -l`).
- `install.rs`: package directory, install / remove / is-installed, the before-remove hook.
- `platform_check.rs`: requirement probes and the pure version comparisons.
- `status.rs`: the process-global snapshot and its refresh worker; `SdCppStatus::ready` /
  `ready()` is the ONE "is sd.cpp installed and usable here" decision (platform, catalog,
  install, requirements; `None` before a refresh; devices and probes are not part of it). The
  `AiButton` gate (`ms-widgets`) and the region editor's readiness line both build on it.
- `error.rs`: `SdCppError` and its localized texts (`sdcpp.error.*`).
- `request.rs`: request / report data types (serde).
- `families.rs`: the one owner of "model family -> sd.cpp context" and of the pre-flight file
  validation (`ModelFilesError`, `sdcpp.files.*`). Add a family here (and in `request.rs`).
- `model_probe.rs`: header readers with hard caps (GGUF 128 MiB of metadata, safetensors header
  16 MiB, index 4 MiB, config 1 MiB) and typed, bounded JSON parsing (entry count, name length,
  dimensions); shapes normalized to PyTorch order.
- `protocol.rs`: worker wire messages, `WORKER_PROTOCOL`, request split / reassembly.
- `ffi/`: the bindings (see its `MODULE_README.md`).
- `worker/`: the worker process (see its `MODULE_README.md`).
- `client/`: the app side of the worker (see its `MODULE_README.md`).

## Contracts and invariants
- Frozen spellings: build wire slugs `vulkan` / `cpu` (persisted in `user_config`
  `General.sdcpp_build`, used in the package path, marker and busy key), the marker file and
  format line. A change of an installed package's meaning bumps `MARKER_FORMAT`.
- Pins: every row's size and sha256 come from GitHub's asset digest and were re-hashed locally;
  never typed by hand. A new release replaces `PINNED` and every row together, and re-checks
  the requirement floors and member lists.
- Nothing downloads implicitly: only `install` (an explicit user action) fetches. Network
  behaviour (retry, resume, cancel) is `ms-download`'s, per `PROJECT_RULES.md`.
- `remove` never waits for an install: one of the same build makes it fail with `Busy`. It runs
  the registered before-remove hook first (the worker client stops and reaps every worker of
  that build, cancelling a running generation); a hook failure (`WorkerStillRunning`) aborts the
  removal with nothing touched.
- Status: one mutex, no I/O under it; `snapshot()` is safe on the GUI thread. At most one
  refresh worker runs; a request during a run schedules one more. A probe result is kept only
  for the selected build. `install` / `remove` request a refresh themselves.
- Threading: catalog, request and `snapshot` are pure; every other entry point does blocking
  I/O and runs on a worker thread.
- Errors: `SdCppError` is `Clone` (it sits in the snapshot); `Display` is localized, `Debug`
  is the English log form. Package failures keep the English diagnostic text as `detail`.
- Bindings: field-exact mirrors of the vendored header, whose sha256 is
  `PINNED.header_sha256`; layout constants measured from C (gcc / mingw) and pinned by tests.
  The library is loaded only in the worker process, never in the app.
- Model files are validated before any worker sees them: a mis-paired text encoder aborts the
  library (`GGML_ASSERT`), so the width rule (`taps x hidden == txt_in` width) is enforced here.
- Worker wire: one frame per message, images one per frame (<= 32 MiB); `WireError` is the
  lossless twin of the worker-side `SdCppError` variants.
- Process isolation: the library runs only in a child process (`--sdcpp-worker`); a library
  abort becomes `WorkerCrashed`, never an app crash. One generation at a time per process
  (`GenerationBusy`); the model stays loaded in the worker between runs with the same files and
  options. Removing a package first stops and reaps every worker of that build, or aborts.
- Tests: no network, no real sd.cpp library (the worker and client tests use a fake backend
  over OS pipes; the real library is exercised only by the opt-in `examples/sdcpp_smoke.rs`); packages are installed from a zip built in the
  test into self-removing temp directories with test-only busy keys.

## Editing map
- New release or build: `catalog.rs` (rows + header notes), then the vendored header.
- New host requirement: `platform_check.rs` (`RequirementProblem` variant + i18n key).
- New status fact: `status.rs` (`SdCppStatus` field + its publisher).
- New failure kind: `error.rs` (variant + key; keep it `Clone`).
- New request option: `request.rs`, then `protocol.rs` (`GenerateHeader`) and the bindings
  mapping.
- New release: also re-vendor the header, re-measure `ffi/layout_tests.rs`, update `ffi/types.rs`.
