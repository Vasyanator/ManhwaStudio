# Module: src

## Purpose
Primary Rust source tree for ManhwaStudio. This directory contains the desktop entry point,
launcher, project editor runtime, shared canvas engine, tab implementations, shared state models,
runtime configuration, Python/AI integration, installer flows, and reusable egui widgets.

`src/` is the authoritative implementation for current application behavior. Legacy Python UI code
outside this tree is not an architecture reference for new work.

## Architecture
The top-level flow is:

```text
main.rs / args.rs
    -> config.rs + python_manager.rs + ms_log (runtime_log/trace)
    -> launcher/ or ProjectData::load()
    -> MangaApp
    -> shared models: BubblesModel, CleanOverlaysModel, TextMaskModel
    -> tabs/* through shared CanvasView + CanvasHooks
    -> background workers and optional Python AI backend
```

`main.rs` owns process startup and hidden service flags. It prepares runtime logging, validates or
discovers a project, starts installer/update/launcher flows when needed, handles direct `--update`
entry, custom/existing install update entry, and hidden `--continue-update` continuation into the
update window, and constructs `MangaApp` for an opened chapter.

`MangaApp` in `app.rs` is the editor root. It creates shared models, wires them into tabs and
canvas instances, starts page/overlay loaders, seeds the source-page CPU cache from the initial
page decode when both canvas caching and the memory profile allow it, throttles GPU uploads, routes
the active tab, and dispatches global hotkeys. It should coordinate subsystems, not absorb
feature-specific domain logic.

Project data enters through `project.rs`. `ProjectData` and `ProjectPaths` define the chapter
filesystem contract, including source pages, bubbles, settings, clean overlays, text detection,
text images, ImageBubble media, notes, terms, characters, wiki data, alternate versions, and
unsaved staging paths.

The canvas layer is shared by translation, cleaning, and typing. Tab-specific behavior must be
added through `CanvasHooks` instead of forking the canvas or duplicating page/bubble interaction
logic.

Long-running work is worker-driven. GUI code may poll channels, upload already prepared textures,
and draw state, but it must not perform blocking I/O, model downloads, Python probes, archive
extraction, image decoding, text rendering, export composition, or AI inference on the GUI thread.

## Files and submodules
- `main.rs`: process entry point, startup routing, installer/update service flags, direct update
  window and update continuation entry, project
  validation, launcher handoff, direct project opening, and Linux/Windows integration hooks.
- `args.rs`: `clap` CLI contract, including visible startup/update flags, the update-check test
  override, and hidden installer/update continuation flags.
- `app.rs`: root `eframe::App`, shared model construction, tab wiring, page/overlay worker
  polling, source-page geometry metadata, incremental texture upload and source GPU trimming,
  shared viewport sync, AI backend health wiring, and global hotkey dispatch.
- `project.rs`: chapter data models, project path discovery, project/settings loading,
  legacy `scr`/`src` and `cleaned`/`clean_layers` folder normalization, magic-byte JPEG->PNG
  conversion in `src`/`cleaned`/`clean_layers`, clean-layer filename normalization (including the
  legacy `<group>_<page>` cleaned numbering, e.g. `1_1.png` -> `001.png`), legacy
  absolute-coordinate bubble migration (`LegacyRibbonGeometry`), unsaved staging paths, and
  filesystem helpers.
- `config.rs`: runtime path roots, project/user config defaults, `JsonConfig`, application data
  directories, model root helpers, and `AiInstallType`. The runtime root is normally the portable
  launch/exe directory, except on macOS when the executable runs inside a `*.app` bundle: there the
  read-only bundle forces the writable root to `~/Library/Application Support/ManhwaStudio`
  (`#[cfg(target_os = "macos")]`, no effect on Linux/Windows).
- `memory_manager.rs`: image-cache memory profile, pressure classification, budget policy, and
  typed eviction ordering for cache owners; it does not own image data or GPU handles.
- `python_manager.rs`: the only Rust-side owner of Python environment discovery, Python command
  construction, hidden-window/UTF-8 setup, shell activation snippets, and managed spawning for
  long-lived Python children that should be killed with the Rust parent on Windows.
- `gpu_utils.rs`: shared GPU/accelerator capability probes used by installer and launcher/runtime
  settings. Call it from workers, not from frame drawing.
- logging/tracing live in the `ms-log` crate (`crates/ms-log`), re-exported as
  `crate::runtime_log` / `crate::trace` (+ `trace_log!` / `trace_scope!` macros) from `main.rs`.
  Text utilities (`text_punctuation`, `segmentation`) live in `ms-text-util`, and the typing text
  renderer (`render_next`) in `ms-text-render`; all three are re-exported at their old paths.
- `backend_ipc/`: directory module for the Rust<->Python AI-backend framed IPC. Submodules:
  `transport` (socket path `backend_socket_path()`, `connect_path`, `BackendStream`), `protocol`
  (Rust mirror of `ipc/protocol.py` constants), `frame` (`Frame`, `read_frame`, `write_frame`
  implementing the `[u32 BE header_len][header_json][u32 BE blob_len][blob]` wire format), and
  `client` (`BackendClient` with background reader thread, id demultiplexing, hello handshake,
  reconnect, event subscriptions, and the process-wide `shared_client()` singleton).
  `CallHandle::{id,cancel,wait,wait_streaming}` supports explicit cancellation and SDXL streaming.
  The framed protocol is the single, sole IPC transport; the legacy HTTP helpers have been removed.
- `ai_backend_capabilities.rs`: process-wide mirrored capability slot for cheap Torch availability
  checks after backend health probing.
- `ai_install_probe.rs`: shared Python package probe that resolves and persists
  `General.ai_install_type`.
- `ai_models.rs`: app-managed AI model catalog, lazy Hugging Face file resolution, direct model
  downloads into `ManhwaStudio_AI_Models`, and typed local path helpers for Rust callers.
- `onnx_runtime/`: native-only (`#[cfg(not(wasm32))]`) app-layer loader that resolves/downloads the
  official onnxruntime dynamic library for `ms-onnx` (probe/download/verify/extract, `ORT_VERSION`).
  Worker-thread only. See `onnx_runtime/MODULE_README.md`.
- `native_runtime.rs`: native-only (`#[cfg(not(wasm32))]`) process-global lazy manager for the
  in-process ONNX Runtime path (`General.ai_runtime = "native"`). Owns one `OrtRuntime`, ONE
  always-resident shared `PaddleDetector` (used by the detector op and every PaddleOCR language via
  `ms_onnx::paddle_recognize`), and an LRU-bounded engine cache (`MangaOcrEngine` Base/2025 +
  per-language `PaddleRecognizer`) keyed by `NativeModelId`, capacity = `General.ai_max_loaded_models`
  (read once, clamped to ≥1, default 3; the shared detector is not counted, LRU evicts the
  least-recently-used engine). The execution provider + adapter index are read once from the UNIFIED
  ONNX keys `General.ai_onnx_provider` (ORT token, mapped by `execution_provider_from_ort_token`) +
  `General.ai_onnx_device_id` — the same keys the Python backend uses, so one selection drives both
  (impossible-for-OS → CPU fallback, fixed per process). When CUDA is selected, a system CUDA
  12.x/cuDNN 9.x probe (`gpu_utils::native_cuda_runtime_available`) gates it: available → CUDA,
  otherwise DirectML (Windows) or CPU, logged, never a wrong result. The adapter index is passed to
  `OrtRuntime::load` and folded into the per-`provider[:device]@version` SIGILL crash-guard scope
  (`run_guarded` shares the fsync'd attempt-before-dlopen / succeeded-after-first-inference /
  graceful-reset sequence across ops). `recognize_manga`, `recognize_paddle`, `detect_paddle`,
  `execution_provider_from_ort_token`, `native_load_scope_key`, and `reset_load_latch` are the public
  surface (the guard/scope helpers are worker-thread only — they do disk I/O + a CUDA probe).
- `input_manager_v2.rs`: keyboard shortcut and modifier-only hotkey registry, user overrides, and
  command lookup.
- `bubble_status.rs`: configurable bubble status rules, condition evaluation, and border painting
  helpers.
- `paste_image.rs`: clipboard/image paste helpers used by UI workflows.
- `screen_capture.rs`: viewport/screen capture helpers for color picking and related tools.
- `tools/`: small shared tool modules that are not tied to a specific tab, currently including mask
  brush behavior.
- `models/`: shared mutable chapter models used across tabs and workers. See
  `models/MODULE_README.md`.
- `canvas/`: shared canvas engine for page layout, viewport navigation, bubble editing, overlays,
  settings sync, and canvas workers. See `canvas/MODULE_README.md`.
- `tabs/`: project editor tab modules: translation, cleaning, typing, characters, terms, notes,
  settings, and wiki. Feature-heavy tabs have nested module readmes.
- `launcher/`: pre-project launcher, project open/import/export/settings pages, detached new
  project window, PSD import, and batch/download/stitching flows. See `launcher/MODULE_README.md`.
- `installer/`: installer, update window shell, dependency setup, elevation helpers, shortcuts,
  registry/uninstall helpers, and installer workers. See `installer/MODULE_README.md`.
- `widgets/`: reusable egui widgets with narrow typed APIs. See `widgets/MODULE_README.md`.
- `bin/`: diagnostic and development binaries for renderer/widget/layout testing. These are not
  production entry points.

## Runtime data flow
Startup first resolves config and runtime paths, initializes logging, handles hidden service flags,
and either opens a validated project or starts the Rust launcher. The launcher returns a typed
outcome to startup; it does not start the editor on its own.

When a chapter opens, `ProjectData::load` builds the typed project snapshot. `MangaApp::new`
constructs `BubblesModel`, `CleanOverlaysModel`, and `TextMaskModel`, shares them with tabs, and
starts page/overlay decode workers. Page image decode and clean overlay preparation happen off the
GUI thread; the GUI thread uploads texture tiles incrementally with a per-frame budget. Source-page
dimensions are kept separately from source GPU texture handles so canvas layout can remain stable
after GPU cache eviction.

Tabs own feature state. Translation owns OCR/detector/MT controllers; cleaning owns overlay editing
tools; typing owns text/image overlay placement, text rendering, masks, and export composition.
They interact with shared page/bubble/overlay behavior through `CanvasView` and `CanvasHooks`.

Python AI calls are split between Rust and Python boundaries. Rust resolves app-managed model files
through `ai_models.rs` before calling backend methods. Python process discovery and command setup go
through `python_manager.rs`. Backend health is push-driven via `TOPIC_HEALTH` events (with a
one-shot `health` pull as a startup/liveness fallback); Torch availability is mirrored through
`ai_backend_capabilities.rs`. Device state is queried via `device.get`/`device.set` IPC methods.
Unresolved backend device choices reported by `device.get` are surfaced by the editor as startup
prompts instead of blocking the GUI thread.

## Contracts and invariants
- Current application behavior belongs in Rust under `src/`; do not copy architecture from legacy
  Python UI unless the user explicitly asks for that code.
- GUI thread work must stay responsive. Move filesystem traversal, image decode, archive work,
  downloads, model probes, rendering, export, AI calls, and command execution to workers.
- Runtime path decisions belong in `config.rs`. Do not hard-code writable data, model, config, log,
  or project paths in feature modules.
- Image cache retention decisions should use `memory_manager.rs` policy objects. Cache owners keep
  pixels and texture handles local and must not move them into the manager.
- Rust code that discovers Python, starts Python scripts/daemons, or builds activation snippets
  must go through `python_manager.rs`. Long-lived Python daemons must use its managed spawn helper
  so Windows assigns them to a kill-on-close Job Object.
- App-managed model downloads must go through `ai_models.rs`, write real files directly into
  `ManhwaStudio_AI_Models`, and fail with explicit errors when required files cannot be resolved.
- Library-managed model caches such as EasyOCR/Surya cache paths must not be redirected through
  `ai_models.rs` unless their ownership contract changes.
- Shared model locks must be short-lived. Snapshot data, release locks, then render, save, decode,
  call hooks, or run image processing.
- Page pixels, scene coordinates, screen coordinates, UV coordinates, width/height, row/column, and
  RGBA/mask buffer lengths must remain explicit at public boundaries.
- Unsupported features must return clear errors. Do not add fake fallback behavior, placeholder
  outputs, or inferred support from filenames when typed metadata exists.
- Public tab behavior that touches canvas interaction must use `CanvasHooks` or typed canvas APIs,
  not duplicated canvas state machines.
- Errors should have user-facing status and diagnostic logging context without secrets or large data
  dumps.

## Editing map
- Startup, service flags, project-open flow, launcher handoff, or update routing: start in
  `main.rs` and `args.rs`.
- User/project config defaults, runtime roots, model root paths, or global path helpers:
  `config.rs`.
- Memory profile, pressure thresholds, budgets, or cache eviction ordering policy:
  `memory_manager.rs`.
- Python environment lookup, Python command construction, shell activation, or process spawning
  contracts: `python_manager.rs`.
- GPU/accelerator detection shared by installer/settings/runtime: `gpu_utils.rs`.
- AI install-type detection from installed Python packages: `ai_install_probe.rs`.
- App-managed AI model coverage, Hugging Face paths, or lazy download behavior: `ai_models.rs`.
- Native ONNX Runtime path (MangaOCR + PaddleOCR OCR, PaddleOCR text detection; runtime/engine
  loading, provider selection, SIGILL crash-guard), or the onnxruntime dylib resolver/downloader:
  `native_runtime.rs` and `onnx_runtime/`. Runtime via `General.ai_runtime`, provider/device via the
  unified `General.ai_onnx_provider`/`ai_onnx_device_id` (shared with the backend); OCR routing lives
  in `tabs/translation/ocr.rs::ocr_route`, detection routing in
  `tabs/translation/text_detector.rs::detector_native_route`.
- ONNX provider/device selection UI (shared Settings + launcher panel): `ai_backend_panel.rs`. The
  provider list is the UNION of the local-native set (`gpu_utils` probes) and the backend-reported
  `available_onnx_providers` (deduped by ORT token), labelled per active `General.ai_runtime`, so
  backend-only providers (e.g. MIGraphX/ROCm) stay selectable for backend ONNX while native falls back
  to CPU for them. See `build_onnx_provider_options` (pure/tested) and `provider_runtime_state`.
- Chapter filesystem shape, project load/save contracts, page discovery, staged unsaved paths, or
  legacy bubble format migration: `project.rs`.
- Root editor wiring, shared model setup, texture upload budgets, page/overlay loader behavior,
  active tab routing, viewport sync, or global hotkeys: `app.rs`.
- Canvas layout, zoom, scrolling, bubble interaction, overlay runtime, or canvas settings:
  `canvas/`.
- Shared bubble, clean overlay, or text detector mask state: `models/`.
- Translation OCR, text detection, machine translation, backend health panels, or translation
  canvas hooks: `tabs/translation/`.
- Cleaning tools, quick-clean, overlay edit commits, mask loading, or cleaning canvas behavior:
  `tabs/cleaning/`.
- Typing overlays, text rendering integration, text masks, deformation, image/text export, or
  auto-typing: `tabs/typing/`.
- Characters, terms, notes, wiki, or settings UI: the corresponding file or module under `tabs/`.
- Launcher pages, project import/export, settings, detached new-project workflow, PSD import, or
  launcher theme/state: `launcher/`.
- Installer/update worker behavior, dependency setup, elevation, shortcuts, or uninstall:
  `installer/`.
- Reusable UI controls: `widgets/`; keep them independent of durable project state.
- Diagnostic binaries: `bin/`; keep production runtime dependencies in library modules instead of
  hiding behavior in test binaries.
