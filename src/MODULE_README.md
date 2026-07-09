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
  settings. Call it from workers, not from frame drawing. Includes `detect_webgpu_adapters`, which
  enumerates the WebGPU GPU adapters per-OS with Dawn's backend (DXGI/Windows, Vulkan/Linux,
  empty/default on macOS) so the returned index is the Dawn `device_id`; the Vulkan path parses
  `vulkaninfo --summary` (pure `parse_vulkaninfo_devices`) and returns empty — never fabricated
  adapters — when the tool is missing/unparseable. Also gates the build-aware native runtime:
  `native_cuda_build_available(build)` (per CUDA-major: `cuda12` iff a CUDA 12.x runtime, `cuda13`
  iff CUDA 13.x, plus cuDNN 9) and `native_openvino_runtime_available()` (Intel-device gate;
  ASYMMETRIC — Linux needs only an Intel GPU because the wheel bundles the OpenVINO runtime, Windows
  ALSO needs a system `openvino*.dll` on the library path because its wheel does not bundle it).
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
  least-recently-used engine). The (build, execution provider, device) triple is resolved once from
  the UNIFIED ONNX keys `General.ai_onnx_build` (an `onnx_runtime::builds` catalog slug picking the
  dylib/version; unset/unknown → `default_build_for_current_os()`) + `General.ai_onnx_provider` (ORT
  token, mapped by `execution_provider_from_ort_token`, validated to belong to the build's EP set else
  the build's headline EP) + `General.ai_onnx_device_id` (per-EP: numeric adapter index for
  DirectML/CUDA/TensorRT/WebGPU, an OpenVINO device-TYPE string for OpenVINO, else `Default`) — the
  same keys the Python backend uses, fixed per process. `decide_selection` (pure) applies the
  availability fallback using the `gpu_utils` probes off the GUI thread: a CUDA build with no matching
  CUDA-major runtime, an OpenVINO build with no Intel device/runtime, a WebGPU EP with no capable GPU,
  an EP unsupported on this OS, or an informational build with no EP → the `cpu` build + CPU EP,
  logged, never a wrong result (the real backstop for a genuine registration failure is
  `error_on_failure` at EP load time). The build is passed to `resolve_or_download_ort_dylib(build)`
  and folded into the `{build}:{provider}[:{device}]@{version}` SIGILL crash-guard scope, where
  `version` is the build's ACTUAL onnxruntime version (`onnx_runtime::build_version`) so a crash on one
  build cannot block another — including two builds sharing a version (cpu/coreml/webgpu are all
  1.27.0). `ORT_DYLIB_COMMITTED` (set after the first successful `OrtRuntime::load`, NEVER reset —
  mirrors ort's un-swappable process-global environment) is the hot-swap-vs-restart signal:
  `reset_load_latch` clears the attempt/success latch + cached runtime/engines for a SAME-build retry
  but leaves it set; a DIFFERENT build needs an app restart. `run_guarded` shares the fsync'd
  attempt-before-dlopen / succeeded-after-first-inference / graceful-reset sequence across ops.
  `recognize_manga`, `recognize_paddle`, `detect_paddle`, `execution_provider_from_ort_token`,
  `native_load_scope_key`, `ort_dylib_committed`, `active_build`, and `reset_load_latch` are the public
  surface (the guard/scope helpers are worker-thread only — they do disk I/O + hardware probes).
- `input_manager_v2.rs`: keyboard shortcut and modifier-only hotkey registry, user overrides, and
  command lookup.
- `locale_store.rs`: native-only (`#[cfg(not(wasm32))]`) on-disk layer for the UI localization
  catalog. Unpacks the `ms-i18n` embedded catalogs into an editable `config::data_dir()/locale`
  folder and reconciles each file on every launch (verbatim on absence; add only missing keys on
  presence, from the embedded catalog for embedded locales and from `en.json` for custom-language
  files; never overwrite or delete user values; `_meta` reserved). `General.ui_language` holds a raw
  OPEN tag string resolved to an `ms_i18n::LocaleTag`, so ANY `locale/<tag>.json` (custom languages
  included) loads; missing keys fall back to English and a tag with no hand-written CLDR plural rules
  uses English plural rules (reported once at install via `ms_i18n::plural_rules_for_tag`). ENGLISH is
  the reference: every error path (absent/invalid tag; a tag with neither disk file nor embedded
  catalog; a corrupt file) installs English, NOT Russian — Russian is only the shipped default config
  value. The reconcile core (`reconcile_locale_map`) is a pure function over two JSON maps; an
  unwritable `locale/` folder or a corrupt file is a logged, bounded degradation to the embedded/English
  catalog, never fatal (a corrupt file is left byte-for-byte intact). On wasm the module is compiled out
  and `web_entry.rs` installs the embedded catalog directly.
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
- UI localization on disk (editable `locale/` folder, embedded-catalog reconcile, active UI-language
  install at startup): `locale_store.rs`. The in-memory catalog/lookup layer is the `ms-i18n` crate;
  the UI language is `General.ui_language`.
- Python environment lookup, Python command construction, shell activation, or process spawning
  contracts: `python_manager.rs`.
- GPU/accelerator detection shared by installer/settings/runtime: `gpu_utils.rs`.
- General settings editor (projects directory + global memory profile) shared by the studio settings
  tab AND the launcher settings page: `general_settings_panel.rs`. Per-UI `GeneralSettingsPanelState`
  + a returned `GeneralSettingsOutcome`; synchronous persistence to `user_config.json` serialized on
  `config::lock_user_config_write()`.
- AI install-type detection from installed Python packages: `ai_install_probe.rs`.
- App-managed AI model coverage, Hugging Face paths, or lazy download behavior: `ai_models.rs`.
- Native ONNX Runtime path (MangaOCR + PaddleOCR OCR, PaddleOCR text detection; runtime/engine
  loading, provider selection, SIGILL crash-guard), or the onnxruntime dylib resolver/downloader:
  `native_runtime.rs` and `onnx_runtime/`. Runtime via `General.ai_runtime`, provider/device via the
  unified `General.ai_onnx_provider`/`ai_onnx_device_id` (shared with the backend); OCR routing lives
  in `tabs/translation/ocr.rs::ocr_route`, detection routing in
  `tabs/translation/text_detector.rs::detector_native_route`.
- ONNX selection UI (shared Settings + launcher panel): `ai_backend_panel.rs`. The section is
  RUNTIME-BRANCHED on `General.ai_runtime`:
  - Native → the BUILD-based selection (Билд → EP → Устройство). The "Билд" combo lists the
    `onnx_runtime::builds` catalog grouped by availability — Базовые (available Basic), Специфичные
    (available Specific), Недоступные (everything unavailable + the informational QNN, which is
    display-only/non-selectable; other unavailable builds stay selectable for a forced download). The
    EP combo comes from `builds::build_execution_providers(build)` (token via `ep_ort_token`,
    round-tripping through `execution_provider_from_ort_token`); the device combo adapts per EP
    (DirectML/WebGPU adapter indices, CUDA/TensorRT `GPU 0`, CPU/CoreML default, OpenVINO device-TYPE
    strings `CPU`/`GPU`/`NPU` written verbatim to `ai_onnx_device_id`). The selected build persists via
    `settings::save_onnx_build` (`General.ai_onnx_build`); the EP/device via `save_onnx_provider_device`.
    An AVAILABLE build's dylib auto-downloads via `resolve_or_download_ort_dylib(build)` off-thread; the
    build-action button is a PURE decision `ort_build_action(committed, active_build, selected, present)`
    → {Retry | LoadOtherBuild | RestartNote}: not-committed + present → same-build "Повторить попытку
    ORT"; not-committed + absent (or forced unavailable) → "Загрузить другую сборку ort" (download +
    `reset_load_latch`); committed + different `native_runtime::active_build()` → "Перезапустите
    программу" (the process-global ort dylib can't hot-swap). Per-build catalog grouping, EP labels, and
    the button decision are pure + unit-tested; build availability + dylib presence are probed off-thread
    (`start_onnx_caps_probe` extended with cuda12/cuda13/openvino flags; `ensure_build_presence_probe`).
  - Backend (or runtime not yet known) → the UNIFIED provider/device combos: the UNION of the
    local-native set (`gpu_utils` probes) and the backend-reported `available_onnx_providers` (deduped by
    ORT token), labelled per runtime, so backend-only providers (e.g. MIGraphX/ROCm) stay selectable.
    See `build_onnx_provider_options` (pure/tested) and `provider_runtime_state`.
  The WebGPU device combo is populated from real GPU adapters enumerated per-OS by the SAME backend Dawn
  uses (DXGI on Windows via `detect_directml_accelerators_windows`, Vulkan on Linux via
  `vulkaninfo --summary`, Metal/default on macOS), so the device id = adapter index = Dawn `device_id`
  passed to `WebGPU::with_device_id`; enumeration runs off-thread in `start_onnx_caps_probe`. This
  index→adapter alignment is best-effort (the backstop is `error_on_failure` at EP registration), and
  an empty adapter list falls back to a single default device.
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
