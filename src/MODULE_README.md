# Module: src

## Purpose
The binary crate `manhwastudio_rs`: the desktop entry point and startup routing, the studio
window shell, the editor root (`MangaApp`), the `settings/` tab and the crate-shim map in
`main.rs`. Everything else — the launcher, the installer, the settings UI, the tabs, the canvas,
the models, the widgets, the config hub — lives in `crates/*`; `main.rs` re-exports those crates
under binary-local `crate::…` module names (see `ARCHITECTURE.md` for the crate layers).

`src/` is the entry point for application behaviour; follow the shim comments in `main.rs` to the
owning crate. Legacy Python UI code outside this tree is not an architecture reference for new work.

## Architecture
The top-level flow is:

```text
main.rs / args.rs
    -> ms_config (config) + ms_sysprobe (python_manager) + ms_log (runtime_log/trace)
    -> ms_launcher (launcher) or studio_bootstrap.rs (background ProjectData::load, or the
       single-image open into a scratch chapter, behind a loading screen)
    -> MangaApp
    -> shared models: BubblesModel, CleanOverlaysModel, TextMaskModel, LayerDoc
    -> tabs/* through shared CanvasView + CanvasHooks
    -> background workers and optional Python AI backend
```

`main.rs` owns process startup and hidden service flags. It prepares runtime logging, validates or
discovers a project, starts installer/update/launcher flows when needed, handles direct `--update`
entry, custom/existing install update entry, and hidden `--continue-update` continuation into the
update window, and opens the studio window for an opened chapter. The studio window starts as
`StudioBootstrapApp` (`studio_bootstrap.rs`), which runs `ProjectData::load*` on a background
thread behind a loading screen and swaps in `MangaApp` once the project snapshot is ready.

`MangaApp` in `app.rs` is the editor root. It creates shared models, wires them into tabs and
canvas instances, starts one unified background loader pool that decodes source pages and clean
overlays from a single page-ordered queue (a page's clean is resolved "staged over committed",
`clean_assign::CleanTreeScope::StagedOverCommitted`, the view the save-merge produces), seeds the source-page CPU cache from the initial
page decode when both canvas caching and the memory profile allow it, throttles GPU uploads, routes
the active tab, and dispatches global hotkeys. It should coordinate subsystems, not absorb
feature-specific domain logic.

Project data enters through `project` (crate `ms-project`). `ProjectData` and `ProjectPaths` define the chapter
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
- `args.rs`: `clap` CLI contract, including visible startup/update flags, the open target
  (`--project` XOR `--image` XOR a positional image path, clap group `open_target`), the
  update-check test override, the environment-check and run-from-sources flags, and hidden
  installer/update continuation flags; plus the pure flag-combination rules
  (`conflicting_installed_copy_flags`, `conflicting_image_flags`) and the image path pre-check.
- `version_format` (crate `ms-config`, re-exported by `main.rs`): the pure composition and
  stripping of the application version string. Compiled twice — as a module of `ms-config` and,
  through `include!("crates/ms-config/src/version_format.rs")`, as part of `build.rs` — so that
  the code the build script runs is the code `cargo test` covers. It must stay std-only: no `t!`,
  no logging, no other crate item. Edit it when the shape of the extended version or the "strip
  build metadata" rule changes; edit `build.rs` when the git probing or the rerun watches change.
  NOTE: a library cannot read the binary's version. `MS_APP_VERSION` is a `rustc-env` emitted for
  the ROOT crate only, and `CARGO_PKG_VERSION` inside a crate is that crate's own — which is why
  `main.rs` hands the pair to `ms-installer` as `HostVersion` instead of the installer reading it.
- `venv_check` (crate `ms-installer`, re-exported by `main.rs`): native-only, GUI-free readiness
  check of the managed Python environment behind `--check-venv`. Reads `General.ai_install_type`
  from the root's `user_config.json` (never writing it), resolves the interpreter through
  `ms_sysprobe::python_manager`, and compares the dependency set required for that install type
  (`installer::utils::required_dependency_specs`) against `pip freeze` via
  `missing_specs_for_readiness` (which accepts interchangeable distributions of the same module)
  plus `installed_torch_is_current` for the `Full` PyTorch minimum version. Those two predicates
  are SHARED with the repair worker, so "ready" always implies the worker would have nothing to
  do. Any failure to VERIFY readiness (unreadable config, unresolvable interpreter, failed probe)
  is reported as NOT ready — the check never claims readiness it could not confirm. It performs no
  installation and opens no window; the window and the exit code belong to `main.rs`. It lives in
  `ms-installer` and NOT in `ms-sysprobe` with its siblings: it reads the installer's own package
  requirements, and `ms-sysprobe` sits BELOW `ms-installer`, so hosting it there would make the
  two circular.
- `app.rs`: root `eframe::App`, shared model construction, tab wiring, unified source-page +
  clean-overlay loader polling, source-page geometry metadata, incremental texture upload and
  source GPU trimming, shared viewport sync, AI backend health wiring, global hotkey dispatch, and
  ownership of the window's single panel-dock state (restore before the first frame, lend to the
  drawing tab, poll for persistence, keep the detached windows alive while no host tab draws). The
  dock hosts are the three CANVAS tabs — translation, cleaning and typing — each of which runs the
  dock inside `CanvasHooks::draw_canvas_overlay_top_left`; the state reaches them through
  `CanvasDrawParams::panel_dock`, which the canvas only carries. It also owns the clean-overlay
  autosave thread and its `OverlayAutosaveControl`: save-to-project pauses it and takes the dirty
  клин snapshots on its worker, page ops and discard stop it without writing (joined off the GUI
  thread), `on_exit` flushes and stops it; `refresh_unsaved_changes_cache` also reports enqueued but
  unacknowledged layer-saver writes (`LayerDoc::has_pending_saves`) and клин pages still held dirty.
  It creates and owns the project's single `AutosaveGate` and hands it to all three staging writers
  (layer saver, bubbles model, клин model + autosave thread); it forces the gate on save-to-project
  success and before a page operation (gate semantics: `crates/ms-models/src/MODULE_README.md`;
  the app-side ordering: "Save / page-op / discard / exit choreography" below). It turns off
  egui's keyboard zoom (`zoom_with_keyboard = false`) every frame, so the only UI zoom is the
  interface-scale setting.
- `app_tab` (crate `ms-config`): declaration of the `AppTab` tab selector with BOTH its persistence
  half (`ALL`, `key()`) and the localized display `title()`. Re-exported by `main.rs` as
  `crate::app_tab` and again by `tabs/mod.rs`, so `crate::tabs::AppTab` stays the path everything
  uses while the config crate builds the `enabled_tabs` default without touching `tabs`. It sits in
  `ms-config` because that default is built from `key()`; `title()` had to follow, since an inherent
  `impl` may only be written in the crate that defines the type.
- `page_view` (crate `ms-models`, re-exported by `main.rs`): source-page view model shared by app
  shell, canvas and tabs — `PageImageInfo` / `SourcePageLoadState` (geometry + load state) and
  `PageTexture` / `TextureTile` (tiled GPU residency keeping the decoded bytes). `app.rs` produces
  and evicts them; nothing there draws.
- `rotation_ctrl_wheel` (crate `ms-config`): app-wide runtime global for the typing tab's Ctrl+wheel
  rotation mode (Vector/Raster). Lives there only so the config crate can read its default;
  `main.rs` re-exports it as `crate::rotation_ctrl_wheel` and `tabs::typing` re-exports it again and
  remains its conceptual owner.
- `studio_bootstrap.rs`: startup shell for the studio window — opens the window immediately, runs
  the background project load behind a loading screen (or an error screen with exit/return-to-
  launcher actions), then swaps in `MangaApp` and delegates `ui`/`on_exit` to it. It also owns
  the window itself from the first frame, so the `WindowGeometryTracker` (monitor/position/size
  persistence) and the Windows first-frame maximize workaround live here, as does the window-wide
  `egui-shader-layers` glow backend (installed from the app creator, destroyed in `on_exit`).
  `StudioOpenRequest` (project dir, or image + `Arc<SingleImageScratch>`) is what a window opens;
  it also derives the window title and the `fonts/ui` probe roots.
- `single_image/`: app-shell half of the single-image mode — `SingleImageController` (one
  `MangaApp` field, `Some` only for `SessionKind::SingleImage`) behind «Сохранить» / «Сохранить как»,
  its pure save state machine, the exit and «Параметры JPEG» dialogs, and the tab filter, window
  title and save-hotkey rules of the mode. `app.rs` holds hooks only; see
  `single_image/MODULE_README.md`.
- `project` (crate `ms-project`, re-exported by `main.rs`): chapter data models, project path
  discovery, project/settings loading, legacy `scr`/`src` and `cleaned`/`clean_layers` folder
  normalization, magic-byte JPEG->PNG conversion in `src`/`cleaned`/`clean_layers`, clean-layer
  filename normalization (including the legacy `<group>_<page>` cleaned numbering, e.g.
  `1_1.png` -> `001.png`), legacy absolute-coordinate bubble migration (`LegacyRibbonGeometry`),
  unsaved staging paths, filesystem helpers, and the "save to project" staging→committed merge
  (`save_merge::merge_unsaved_into_project`; `app.rs::start_save_to_project` calls it on its worker
  and injects the per-page layer-manifest merge from `models::layer_model::persist`). `Page` and
  `ProjectPaths` are re-exported from `ms-page-ops`, which declares them. See
  `crates/ms-project/src/MODULE_README.md`.
- `project_scan` (crate `ms-project`, re-exported by `main.rs`): filesystem scan of the projects
  ROOT shared by the launcher and startup — title/chapter enumeration, openability validation
  (`ProjectValidationState`) and unsaved-chapter detection. Plain I/O: no UI, no app state. It is
  the same domain one level up from the chapter load, which is why it sits in `ms-project`.
- `config` (crate `ms-config`, re-exported by `main.rs`): runtime path roots, project/user config defaults, `JsonConfig`, application data
  directories, model root helpers, `AiInstallType`, and `Flux2Variant` (the FLUX.2 klein
  checkpoint that keys its model directory, component paths and settings file). The runtime root is normally the portable
  launch/exe directory, except on macOS when the executable runs inside a `*.app` bundle: there the
  read-only bundle forces the writable root to `~/Library/Application Support/ManhwaStudio`
  (`#[cfg(target_os = "macos")]`, no effect on Linux/Windows).
  `JsonConfig` backfills missing defaults but does not rewrite a semantically complete JSON file;
  user-config load/default/write transactions remain serialized by the config-layer lock.
  `General.enabled_tabs` object keys are the stable English `AppTab::key()` ids
  (`tabs/mod.rs`) — the persistence contract, byte-stable across releases and UI languages, so they
  are NEVER localized (`dev-docs/i18n_exclusions.md` A3/B1). The field has NO reader: it is a key
  set coupled 1:1 to `AppTab::key()`. `merge_missing` adds keys but never removes, so an old user
  config may still carry legacy Russian tab labels as keys next to the English ids; they are inert
  and left on disk. A feature that starts reading `enabled_tabs` owns the one-time cleanup migration
  (keyed by `AppTab::key()`).
- `config_saver` (crate `ms-config`, re-exported by `main.rs`): the ONE debouncing writer thread behind every `user_config.json` section that
  is written from the GUI thread by a user gesture — today the `PanelLayout` section
  (`ms-widgets`' `panel_dock/persist.rs`) and the `Window` section (crate `ms-window-geometry`). It owns the
  durability policy of those sections: 700 ms coalescing into one write, a failed write HELD and
  retried with a capped backoff instead of dropped, newer payloads folded over the held one by the
  section's own `SaverPayload::coalesce`, a final attempt on `flush_and_join` and on a disconnected
  channel, and a definitive loss logged as an error naming cause, path and context. A section
  supplies only its payload's fold rule, its typed error's `SaverError::is_retryable` verdict and
  its write step; change the policy here, not in a consumer.
- Dockable-panel arrangement: `ms-widgets`'s `panel_dock/persist.rs` owns the self-versioned `PanelLayout`
  section of `user_config.json` (one entry per program tab, keyed by `AppTab::key()`). It is the
  only writer of that section and does all of its I/O on a `config_saver` thread
  (`PanelLayoutWriter`, owned by `MangaApp`, flushed in `on_exit`). The dock STATE is app-owned too
  — `MangaApp::panel_dock`, exactly one per studio window, lent to the drawing tab for the frame —
  because sub-window indices, the persisted window list and the layouts of every program tab are one
  shared thing; `app.rs` polls it for a dirty layout once per frame. Loading happens once before the
  first frame, from the startup `user_settings` snapshot (`app.rs::restore_panel_dock`), so a stored
  layout wins over a tab's default one. The section also carries the dock's detached OS windows
  (`sub_windows`), which a panel addresses through its `host`; `app.rs` additionally calls
  `PanelDockState::show_idle_sub_windows` on every frame whose active tab is not a dock host
  (`tab_hosts_panel_dock`), because those windows are immediate viewports and exist only while they
  are shown.
- `window_geometry` (crate `ms-window-geometry`, re-exported by `main.rs`, native-only
  `#[cfg(not(wasm32))]`): owner of the self-versioned `Window` section of `user_config.json` —
  the user's primary-monitor choice, the largest monitor seen last run, and the studio window's
  restored position/size/maximized state. Provides the pure startup planner
  (`plan_startup_placement` / `apply_placement`, fed to `ViewportBuilder` before any window
  exists), the pure monitor resolver (`resolve_monitor`, degrading to the largest monitor with a
  typed reason), the process-wide monitor mirror for the settings selector, and
  `WindowGeometryTracker` (per-frame `ViewportInfo` sampling + a `config_saver` writer thread +
  `on_exit` flush; the tracker only compares each sample against the last one it queued, so the
  saver is the last owner of a queued sample and must not drop it on a failed write). It is the
  project's ONLY direct `winit` dependency — egui/eframe expose no monitor list. Wayland is detected and refused explicitly (no geometry
  persisted, no relocation, a message in the settings UI) instead of failing silently.
  See `crates/ms-window-geometry/src/MODULE_README.md`.
- `memory_manager` (crate `ms-memory`): image-cache memory profile, pressure classification, budget policy, and
  typed eviction ordering for cache owners; it does not own image data or GPU handles.
- `python_manager` (crate `ms-sysprobe`): the only Rust-side owner of Python environment discovery, Python command
  construction, hidden-window/UTF-8 setup, shell activation snippets, and managed spawning for
  long-lived Python children that should be killed with the Rust parent on Windows.
- `hf_token` (crate `ms-sysprobe`): the process-wide Hugging Face access token. A runtime global in the shape of
  `rotation_ctrl_wheel.rs` (cached value + free get/set/clear), backed by the OS secret
  store under its OWN service name `"ManhwaStudio Hugging Face"` — never the OCR key entry, which
  belongs to the translation tab and is keyed by service. Seeded once at startup
  (`main.rs::seed_hf_token_from_secret_store`, on a worker thread) so reads are lock acquisitions,
  not OS round trips; `store`/`clear`/`read` are BLOCKING and callers spawn for them. Tri-state:
  `HfTokenState::Unknown` (not read yet) is distinct from `Missing` and must never be rendered as
  it. The token is never written to `user_config.json`, to any settings JSON, or to a log line, and
  no message in the tree interpolates its value.
- `gpu_utils` (crate `ms-sysprobe`): shared GPU/accelerator capability probes used by installer and launcher/runtime
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
  The same shim pattern carries four more extracted leaves, all mounted from `main.rs` under their
  historical module names: `ms-backend-ipc` (`crate::backend_ipc`), `ms-sysprobe`
  (`crate::gpu_utils`, `crate::python_manager`, `crate::screen_capture`) and `ms-memory`
  (`crate::memory_manager`). Their sources are under `crates/<name>/src/`, each with its own
  `MODULE_README.md`; the entries below describe the same items at their unchanged call paths.
- `backend_ipc` (crate `ms-backend-ipc`): the Rust<->Python AI-backend framed IPC. Submodules:
  `transport` (socket path `backend_socket_path()`, `connect_path`, `BackendStream`), `protocol`
  (Rust mirror of `ipc/protocol.py` constants), `frame` (`Frame`, `read_frame`, `write_frame`
  implementing the `[u32 BE header_len][header_json][u32 BE blob_len][blob]` wire format), and
  `client` (`BackendClient` with background reader thread, id demultiplexing, hello handshake,
  reconnect, event subscriptions, and the process-wide `shared_client()` singleton).
  `CallHandle::{id,cancel,wait,wait_streaming}` supports explicit cancellation and SDXL streaming.
  The framed protocol is the only IPC transport to the backend.
- `ai_backend_capabilities` (crate `ms-sysprobe`): process-wide mirrored capability slot for cheap Torch availability
  checks after backend health probing.
- `ai_install_probe` (crate `ms-sysprobe`, re-exported by `main.rs`): shared Python package
  probe that classifies the machine into `config::AiInstallType`; `main.rs` persists the
  result into `General.ai_install_type`.
- `ai_models` (crate `ms-sysprobe`): app-managed AI model catalog, lazy Hugging Face file resolution, direct model
  downloads into `ManhwaStudio_AI_Models`, and typed local path helpers for Rust callers.
- `onnx_runtime` (crate `ms-onnx-runtime`, re-exported by `main.rs`, native-only
  `#[cfg(not(wasm32))]`): loader that resolves/downloads the official onnxruntime dynamic library
  for `ms-onnx` (probe/download/verify/extract, `ORT_VERSION`). Worker-thread only.
  See `crates/ms-onnx-runtime/src/MODULE_README.md`.
- `native_runtime` (crate `ms-native-runtime`, re-exported by `main.rs`, native-only
  `#[cfg(not(wasm32))]`): process-global lazy manager for the
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
- `input_manager_v2` (crate `ms-widgets`, re-exported by `main.rs`): keyboard shortcut and
  modifier-only hotkey registry, user overrides, and command lookup. It moved down into the
  widget layer because it is a pure egui-input primitive over `AppTab` and the `translation` tab
  crate registers specs with it; the settings hotkeys pane reaches it through the re-export.
- `locale_store` (crate `ms-config`, re-exported by `main.rs`): native-only (`#[cfg(not(wasm32))]`) on-disk layer for the UI localization
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
- `ui_fonts` (crate `ms-widgets`, re-exported by `main.rs`): the single owner of the UI font stack. Installs the bundled `fonts/ui` chain
  into an `egui::Context` from a worker thread, taking the manifest and the process-wide
  `'static` bytes from `ms-fonts` (so epaint borrows one copy instead of keeping a second),
  and owns the `BUBBLE_TEXT_FAMILY_NAME` / `UI_BOLD_FAMILY_NAME` family names plus the
  system-font emergency fallback. Called exactly once per `run_native` constructor closure.
  `Tier::Full` only ARMS the large `ext/` tier; `ensure_covers(ctx, text)` installs it on
  the first character the chain cannot draw and is called from the canvas while a frame
  runs (it reads `Context::fonts_mut`, so never from a loader or a worker thread); the
  canvas bubble cards and the typing tab's user-text surfaces all offer it their strings.
  The title-local `fonts/ui` override is resolved here and nowhere else — the `ms-fonts`
  manifest stays project-independent so a project cannot change a finished render.
  Override files come from an arbitrary opened project and are therefore UNTRUSTED: each
  one is parsed and must yield a family name before it can be installed
  (`validate_font_bytes`), because epaint parses every registered file eagerly and PANICS
  on a failure it cannot recover from. A candidate whose core files all fail that check is
  treated as an absent override and the bundled stack wins.
- `bubble_status` (crate `ms-widgets`, re-exported by `main.rs`): the egui half of the bubble status feature — border painting helpers and the
  `BubbleBorderPaintColor` colour view — plus a wholesale re-export of the GUI-free rule model in
  `ms_config::bubble_status` (rules, conditions, default preset, JSON, evaluation), which had to
  move down because `user_config_defaults()` embeds the default preset. `crate::bubble_status::…`
  still names both halves.
- `paste_image` (crate `ms-sysprobe`): clipboard image reader shared by the launcher and the
  typing/translation tabs. A host-environment probe like `screen_capture`, which is why it sits
  in that crate; `main.rs` re-exports it as `crate::paste_image`.
- `screen_capture` (crate `ms-sysprobe`): viewport/screen capture helpers for color picking and related tools.
- `tools` (crate `ms-tools`, re-exported by `main.rs`): shared tool primitives not tied to a
  specific tab — the mask brush, the polygon scanline rasterizer, the shared red-black SOR
  kernel, the dense overlay-pixel solve — plus `tools::patch`, the host-neutral patch-tool core
  the tabs drive through its `PatchHost` trait. `crate::tools::…` still names all of it.
- `page_ops` (crate `ms-page-ops`, re-exported by `main.rs`): GUI-free engine for structural
  page operations (`PageOpKind`: move / insert files / create blank / delete / split / crop-rotate /
  stitch) executed as a journaled crash-safe transaction over both the committed chapter tree and the `_unsaved` mirror;
  `recover_pending_page_op` is called at the start of `ProjectData::load_internal`. It also
  DECLARES `Page` and `ProjectPaths`, which `ms-project` re-exports — the direction that keeps
  the two crates acyclic. `MangaApp` quiesces all page-indexed writers before dispatching the
  engine on a worker; `StudioBootstrapApp` then rebuilds a fresh app from disk, never remapping
  runtime state in place. See `crates/ms-page-ops/src/MODULE_README.md`.
- `models` (crate `ms-models`, re-exported by `main.rs`): shared mutable chapter models used across
  tabs and workers, plus `page_view`. See `crates/ms-models/src/MODULE_README.md`.
- `canvas` (crate `ms-canvas`, re-exported by `main.rs`): shared canvas engine for page layout,
  viewport navigation, bubble editing, overlays, settings sync, and canvas workers. It DECLARES
  `CanvasHooks`, which the tabs implement, so it never names a tab. See
  `crates/ms-canvas/src/MODULE_README.md`.
- `tabs/`: tab wiring. Only the `settings/` tab is still a module of the binary; every other tab
  is a crate re-exported by `tabs/mod.rs` under its old module name, so no `crate::tabs::<tab>::…`
  call site changed: `ms-tab-typing` (`typing`), `ms-tabs-simple` (`characters`, `terms`, `notes`,
  `wiki`), `ms-tab-translation`, `ms-tab-ps-editor`, `ms-tab-page-manager`, `ms-tab-cleaning`.
  See `tabs/MODULE_README.md` for the shim map and the layering rules.
- `launcher` (crate `ms-launcher`, re-exported by `main.rs`): pre-project launcher, project
  open/import/export/settings pages, detached new-project window, PSD import, and
  batch/download/stitching flows. Effectively a SECOND application next to the studio: the editor
  never names it — only `main.rs` and `web_entry.rs` do. See
  `crates/ms-launcher/src/MODULE_README.md`.
- `installer` (crate `ms-installer`, re-exported by `main.rs`, native-only): installer, update
  window shell, dependency setup, elevation helpers, shortcuts, registry/uninstall helpers, the
  installer workers and `venv_check`. It takes the host application's version pair as an explicit
  `HostVersion` parameter. See `crates/ms-installer/src/MODULE_README.md`.
- The settings surface shared by BOTH shells — `settings_shared`, `general_settings_panel`,
  `ai_backend_panel`, `ai_backend_supervisor` and the `tutorial` subsystem — is the crate
  `ms-settings-ui`, re-exported by `main.rs` under those same names. The launcher's settings page
  and the studio `settings/` tab render the same panes, so neither could own them. Its `tutorial`
  feature is FORWARDED from the root `tutorial` feature (features are not inherited); the demo bin
  `src/bin/tutorial_test` still mounts `tutorial/engine.rs` through `#[path]`. See
  `crates/ms-settings-ui/src/MODULE_README.md`.
- `i18n_resolve.rs`: not a re-export — every caller names `ms_i18n::resolve_key` directly.
  The module exists only to HOST the cross-crate tests guarding it: they assert against
  `ms-text-util`'s key sets and hold `ms_config::locale_store::GLOBAL_LOCALE_LOCK`, and `ms-i18n`
  sits below both, so the binary is the lowest place that can see all three at once.
- `widgets` (crate `ms-widgets`, re-exported by `main.rs`): reusable egui widgets with narrow typed
  APIs, plus the three crate-root leaves of the same layer re-exported alongside it —
  `input_util`, `ui_fonts` and `bubble_status`. Knows nothing of the project domain: canvas, tabs
  and launcher depend on it, never the reverse. See `crates/ms-widgets/src/MODULE_README.md`.
- `bin/`: diagnostic and development binaries for renderer/widget/layout testing. These are not
  production entry points.

## Runtime data flow
Startup first resolves config and runtime paths, initializes logging, handles hidden service flags,
and either opens a validated project or starts the Rust launcher. The launcher returns a typed
outcome to startup; it does not start the editor on its own.

Startup routing order in `run_main`: CLI parse -> (Linux desktop integration and the isolated
backend-socket seed, both decided by `--ignore-installed`) -> Windows service flags -> storage-mode
probe (`init_storage_mode_at_startup`: the FIRST document access; seeds the docstore default format)
-> config seeding -> on-disk locale reconcile (`locale_store::reconcile_disk_catalog`, BEFORE
`load_user_settings_for_startup`) -> UI-locale install / UI-scale / autosave-policy seeding -> `--check-venv` (terminal) -> `--continue-update` ->
`--update` -> `--test-launcher` -> AI backend supervisor -> open-target resolution
(`StartupTarget`) -> `StudioOpenRequest` -> studio window -> `StudioOpenRequest::release`.
A pending storage reconciliation (user_config still in the other format than its recorded mode) is
started on a worker right before the launcher, or — on a direct `--project` or image start — by
`studio_bootstrap` once the project has loaded (`ms-settings-ui`'s `storage_mode_job`); an
incomplete reconciliation is surfaced in the studio's top bar (`MangaApp::draw_storage_reconcile_notice`).

Before any of that, `reject_conflicting_startup_flags` validates the command line: combining
`--ignore-installed` with a flag that manages an installed copy (`args::INSTALLED_COPY_FLAGS` —
visible `--update` and the hidden install/update/uninstall/shortcut service flags) exits with code
2. An image to open combined with `--check-venv`, `--test-launcher` or any installed-copy flag
exits the same way. This runs FIRST because several of those flags act immediately; the decisions
themselves are the pure, unit-tested `args::conflicting_installed_copy_flags` /
`args::conflicting_image_flags`.

Single-image start (plan `dev-docs/single_image_mode_plan.md`, D13): `--image <PATH>`, the
positional path (what Linux `.desktop` `%f` and Windows "Open with" `"%1"` pass) or the launcher's
`LauncherOutcome::OpenImage` all become `StartupTarget::Image`. A CLI image skips the launcher
like `--project`; a missing path, a directory (the message names `--project`) or a non-file shows
`show_startup_error_dialog` and ends startup. The `run_main` loop then reserves a scratch session
(`SingleImageScratch::reserve` under `ms_config::single_image::scratch_base()`), OWNS it for the
window's whole life and deletes it after `run_main_window` returned (no window then, so never on a
GUI thread); a reservation failure ends a CLI start, or returns a launcher pick to the launcher.
The load worker runs `open_single_image` (no unsaved detection) and drops its scratch share
before reporting; an open/decode failure lands on the bootstrap error screen as
`SingleImageError::user_message` with the usual "Exit to launcher" / "Exit". A single-image
session refuses the structural-operation reload (logged). `ReturnToLauncher` clears every CLI open
target. Stale scratch sessions of crashed runs are swept once per start on a worker
(`spawn_stale_scratch_sweep`, right after the flag check; lock-guarded, so another live instance
is safe). The Linux desktop entry (`linux_desktop_entry_text`, pure and tested) uses `Exec=… %f`
and a `MimeType=` list built from `ms_config::single_image::INPUT_FILE_TYPES`; it only OFFERS the
program in "Open with" (best-effort `update-desktop-database` afterwards), never sets a default.

Two startup flags change that routing:
- `--check-venv` is TERMINAL: it checks the environment (`ms-installer`'s `venv_check`), exits 0 with a printed
  message when it is complete, otherwise opens the installer in environment-repair mode and exits
  0 (repaired AND re-verified) or 1 (cancelled / failed / still not ready). A repair that reports
  success is re-checked before exiting 0, so the caller never receives a broken environment. It
  never opens the launcher or studio and never starts the AI backend. `main.rs` calls
  `std::process::exit` there so the code is exact instead of the generic error exit of an `anyhow`
  return.
- `--ignore-installed` marks a run from a source checkout: no Linux desktop-entry write, no Windows
  existing-install discovery, no missing-environment prompt (straight to the launcher), no startup
  update check (so the launcher's update notice never appears) and a refusal at every self-update
  entry point, plus a per-root backend socket name
  (`backend_ipc::seed_isolated_backend_socket_name`, seeded right after CLI parsing). The flag is
  threaded explicitly through `StartupRoutingFlags`; the socket name is the only process-global,
  because the supervisor and the IPC client must agree on it without a shared call path.

When a chapter opens, `ProjectData::load` builds the typed project snapshot on a background thread
while `StudioBootstrapApp` shows a loading screen. `MangaApp::new` then constructs `BubblesModel`,
`CleanOverlaysModel`, `TextMaskModel` and the chapter `LayerDoc` (all `Arc<Mutex<_>>`), shares them with tabs, and starts one unified decode
pool that interleaves source pages and clean overlays in page order; overlays are applied to
`CleanOverlaysModel` as they arrive (no in-order promotion), while source pages keep strict
in-order promotion. Page image decode and clean overlay preparation happen off the GUI thread; the
GUI thread uploads texture tiles incrementally with a per-frame budget. Source-page
dimensions are kept separately from source GPU texture handles so canvas layout can remain stable
after GPU cache eviction. The source textures are lent per frame to the canvas tabs and to the page
manager (its page viewer); the per-frame GPU trim keeps the pages of `active_source_page_window`,
which for the page manager is the one page its viewer draws.

Tabs own feature state. Translation owns OCR/detector/MT controllers; cleaning owns overlay editing
tools; typing owns text/image overlay placement, text rendering, masks, and export composition.
They interact with shared page/bubble/overlay behavior through `CanvasView` and `CanvasHooks`.

Python AI calls are split between Rust and Python boundaries. Rust resolves app-managed model files
through `ai_models` (crate `ms-sysprobe`) before calling backend methods. Python process discovery and command setup go
through `python_manager.rs`. Backend health is push-driven via `TOPIC_HEALTH` events (with a
one-shot `health` pull as a startup/liveness fallback); Torch availability is mirrored through
`ai_backend_capabilities` (crate `ms-sysprobe`). Device state is queried via `device.get`/`device.set` IPC methods.
Unresolved backend device choices reported by `device.get` are surfaced by the editor as startup
prompts instead of blocking the GUI thread.

## Save / page-op / discard / exit choreography
`app.rs` sequences the three staging writers (layer saver, bubbles saver, клин autosave with its
`OverlayAutosaveControl`) around every destructive point. The writers' own contracts live in
`crates/ms-models/src/MODULE_README.md` and `crates/ms-canvas/src/MODULE_README.md`; typing's
deferred text edits in `crates/ms-tab-typing/src/MODULE_README.md`.
- `start_save_to_project`: PS `flush_layers`, then typing `flush_text_layers`. A flush `Err` or any
  `failed_pages` ABORTS the save (nothing merged, staging kept, status shown) — an `Err` is never
  degraded to an empty owned-page set. The worker then barriers the bubbles saver (holding it), the
  layer saver (a failed TEXT write aborts), pauses the клин autosave and takes the dirty клин
  snapshots (a failed write restores them via `restore_dirty_save_snapshots` and aborts), merges,
  resumes, and re-barriers; success calls `AutosaveGate::force_flush`.
- `start_page_op`: flushes canvas upserts, PS layers and typing text; `page_op_text_quiesce` refuses
  the op when edits stay unwritten or the doc lock is poisoned (`NoLayersDir`/`NoLayerDoc` pass only
  when nothing was pending). Then `force_flush`, клин autosave `request_stop_now`; the worker joins
  it, takes клин snapshots, pauses the bubbles saver, barriers the layer saver, writes the snapshots
  to staging and dispatches the engine. Page ops are not staged; the app is rebuilt from disk.
- DISCARD (`start_exit_cleanup` = `quiesce_writers_discarding` + delete job): the quiesce latches
  `discarding_unsaved_changes` first, DROPS (never flushes) typing's deferred edits,
  `shutdown_saver_discarding` on the layer saver, pauses the bubbles saver, `request_stop_now` on the
  клин autosave; then staging is deleted on a job that joins that thread.
- Single-image session (`single_image/`): staging is a scratch that is never merged and that
  `run_main` deletes after the window closed (plan D13), so EVERY close quiesces the discard way
  and nothing is flushed: «Не сохранять» and a successful save-then-close go through
  `close_single_image_discarding` (quiesce + `finalize_close`, no delete job, клин thread joined in
  `on_exit`); `on_exit` itself quiesces first when no dialog did. Dirty is the controller's
  (`AutosaveGate::action_count` vs the last-write baseline, plus deferred typing edits, plus a write
  in flight), never the staging probe. The save-then-close keeps the window open on a failed write
  and re-opens the exit dialog when the user backed out of the picker / JPEG options.
- Failed discard (`abort_discard_after_failed_cleanup`): releases the latch, marks the session
  unsaved, and restarts ALL three writers on the same `AutosaveGate` (`enable_background_saver`,
  `resume_saver_after_failed_discard`, a fresh `OverlayAutosaveControl` + autosave thread).
- `on_exit`: flush-and-stop the клин autosave (stop-now when discarding) and join it, then (unless discarding) enqueue typing's deferred edits INLINE,
  then barrier the layer saver and report its failed pages, then shut down layer and bubbles savers.

## Contracts and invariants
- Current application behavior belongs in Rust under `src/`; do not copy architecture from legacy
  Python UI unless the user explicitly asks for that code.
- GUI thread work must stay responsive. Move filesystem traversal, image decode, archive work,
  downloads, model probes, rendering, export, AI calls, and command execution to workers.
- Runtime path decisions belong in the `config` crate (`crates/ms-config`). Do not hard-code writable data, model, config, log,
  or project paths in feature modules.
- The application version has TWO forms and the split is a safety rule, not a preference.
  `MS_APP_VERSION` (composed at build time by `build.rs`, e.g. `3.6.0+1cd9638-83-dirty`) is the
  HUMAN form: window title, version labels, `--version`, diagnostic logs. `CARGO_PKG_VERSION` is
  the MACHINE form: every comparison and every value parsed by another process — all three
  release comparators (`main.rs`, `ms-installer`'s `update.rs` / `utils.rs`). A suffix on a
  compared value makes a build outrank its own release tag. Neither form is compared with the
  Python backend: compatibility there is `PROTOCOL_VERSION` in the `hello` handshake
  (`backend_ipc/`), and the backend's own version is diagnostic only.
  Where an extended string unavoidably crosses a process boundary (an installed copy probed with
  `--version`), reduce BOTH sides with `version_format::version_core` before comparing.
- Image cache retention decisions should use `ms_memory` policy objects. Cache owners keep
  pixels and texture handles local and must not move them into the manager.
- Floating panels are declared as tabs of the panel dock (`crates/ms-widgets/src/panel_dock/`, widgets
  `CollapsiblePanel` + `PanelTab`), never as a hand-rolled `Area + Frame::popup` panel or an
  `egui::Window`. Edge-glued `egui::Panel` and overlay `Area`s (toasts, tooltips, scene overlays)
  are unaffected. See `crates/ms-widgets/src/MODULE_README.md` and `egui-docs/01-app-shell.md` §3.1.
- Rust code that discovers Python, starts Python scripts/daemons, or builds activation snippets
  must go through `python_manager.rs`. Long-lived Python daemons must use its managed spawn helper
  so Windows assigns them to a kill-on-close Job Object.
- App-managed model downloads must go through `ms_sysprobe::ai_models`, write real files directly into
  `ManhwaStudio_AI_Models`, and fail with explicit errors when required files cannot be resolved.
- Library-managed model caches such as EasyOCR/Surya cache paths must not be redirected through
  `ms_sysprobe::ai_models` unless their ownership contract changes.
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
- Every studio window (and the web launcher→editor swap) installs its theme ONLY through
  `ms_theme::apply`, never a bare `set_theme`; semantic colours come from `ms-theme` (see
  `crates/ms-theme/MODULE_README.md`). The launcher keeps its own `crates/ms-launcher/src/theme.rs`.
- Fonts are installed ONLY through `ms_widgets::ui_fonts`, and only with `egui::Context::add_font`.
  `Context::set_fonts` replaces the whole definition set and would drop the families other
  subsystems add at runtime (typing font previews/editors), which then panics in epaint;
  `Context::fonts` panics before the first frame and must not be called from a loader.
- PARAMETER-CLUMP CONSOLIDATION HAS A SETTLED SCOPE, and the boundary is recorded here so it is not
  re-litigated. A 2026-07 multi-agent review of `src/` (four external reviewers plus one cross-pool
  reviewer) inventoried every function threading a large loose-argument clump. Its general principle
  stands: consolidate into an owned `Copy` snapshot/context struct built AT the call boundary, never
  into a context that borrows a whole tab or the whole `CanvasView` — most argument explosions in
  `crates/ms-tab-cleaning/src/` are deliberate disjoint-borrow workarounds and need a durable STATE split, not a
  call-site bag. Two of its recommendations shipped and are the exemplars to copy: `PageView`
  (`crates/ms-tab-typing/src/tab/mesh_geometry.rs`, the `(page_idx, image_rect, zoom)` viewport triple) and the
  canvas' `BubbleMenuContext` / `BubbleMenuOutcome` / `BubbleMenuCommand` (`canvas/types.rs`), which
  replaced a 16-parameter menu call with seven `&mut bool` out-flags.
  The following were examined at the same time and DELIBERATELY left as they are. Do not re-propose
  them as cleanups without new evidence; a wide signature is not by itself a defect.
  - `export_dispatch_ready` (`crates/ms-tab-typing/src/tab.rs`, three `bool`s): a pure, `#[must_use]`, documented
    and unit-tested predicate. A struct would add a type without removing a failure mode.
  - The typing mask kernels — `flood_fill_mask_from_seed` (`crates/ms-tab-typing/src/mask.rs`) and the mask
    painters: owned-buffer worker kernels whose parameters have no natural grouping; `TypingPageMask`
    is already used where it fits and the local `#[allow(clippy::too_many_arguments)]` carries its
    justification at the site. The one carve-out still worth taking opportunistically is
    `erase: bool` → a `MaskStrokeMode` enum (not done as of this writing).
  - Numeric kernels and rasterizers where the parameters ARE the algorithm: the SOR/Poisson/Lab
    kernels in `crates/ms-tab-cleaning/src/tools/gradient.rs`, the circle rasterizers in `ms-tools`' `mask_brush.rs`,
    the pure leaves of `crates/ms-tab-typing/src/tab/mesh_geometry.rs`, `pack_aside_slots` and
    `reserve_canvas_page_frame` in `canvas/`, `evaluate_bubble_shape`'s scoring internals
    (`crates/ms-tab-typing/src/auto_typing.rs`), and the `studio_bootstrap.rs` spawns.
  - `TypingExportPageJob` and the render-request structs in `crates/ms-tab-typing/src/tab/render_store.rs`: both
    review pools independently found these already ARE the target pattern. Use them as the model;
    do not "consolidate" them further.
  - The `use super::*` re-export style in typing's descendant modules is a consequence of the
    documented descendant-module design, not a defect to fix on its own. Unwinding it into explicit
    imports is only possible after the panel state splits it depends on, and is not worth doing
    before them. Import breadth in `app.rs`, `crates/ms-tab-translation/src/tab.rs` and `layer_model/persist.rs`
    is legitimate — they are composition roots and a persistence boundary.

## Editing map
- Startup, service flags, project-open flow, launcher handoff, or update routing: start in
  `main.rs` and `args.rs`.
- Single-image saving, its top bar, exit dialog, hidden tabs and save hotkeys: `single_image/`
  (hooks in `app.rs`: `tab_visible`, `draw_tab_bar`, `draw_exit_dialog`, `ui` tick, `on_exit`).
- Single-image start (CLI, launcher pick, scratch lifetime, desktop entry): `main.rs`
  (`resolve_startup_target`, the `run_main` loop, `linux_desktop_entry_text`), `args.rs`
  (`open_target` group), `studio_bootstrap.rs` (`StudioOpenRequest`, `spawn_open_thread`).
- What the application reports as its version, or which sites may see the git suffix:
  `crates/ms-config/src/version_format.rs` (the pure rules) + `build.rs` (git probing and rerun
  watches); then the
  human/machine split in "Contracts and invariants" above before touching any call site.
- What counts as a complete Python environment, or the `--check-venv` exit contract:
  `crates/ms-installer/src/venv_check.rs` (decision) + `main.rs::run_check_venv_flow` (window +
  exit code) + `crates/ms-installer/src/utils.rs::required_dependency_specs` (the required set).
- User/project config defaults, runtime roots, model root paths, or global path helpers:
  `crates/ms-config/src/lib.rs` (re-exported as `crate::config`).
- Which monitor a window opens on, restoring/persisting the main window's position, size or
  maximized state, or the primary-monitor selector: crate `ms-window-geometry`. The startup call
  sites are `main.rs::run_main_window` and `ms-launcher`'s `lib.rs::run_launcher_internal` (viewport
  builder), the runtime owner is `studio_bootstrap.rs` (`WindowGeometryTracker`), and the UI is
  the monitor row in `ms-settings-ui`'s `general_settings_panel.rs`.
- Memory profile, pressure thresholds, budgets, or cache eviction ordering policy:
  crate `ms-memory` (re-exported as `crate::memory_manager`).
- UI localization on disk (editable `locale/` folder, embedded-catalog reconcile, active UI-language
  install at startup): `locale_store.rs`. The in-memory catalog/lookup layer is the `ms-i18n` crate;
  the UI language is `General.ui_language`.
- Python environment lookup, Python command construction, shell activation, or process spawning
  contracts: `python_manager.rs`.
- GPU/accelerator detection shared by installer/settings/runtime: `ms_sysprobe::gpu_utils`.
- The Hugging Face access token (reading it, adding a second UI surface for it, changing where it is
  stored): `ms_sysprobe::hf_token`. Its first UI surface is the FLUX.2 klein download block
  (`crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/`, drawn by its `ui/install.rs`), which is a
  CONSUMER, not the owner.
- General settings editor (projects directory, global memory profile, interface scale, primary
  monitor, UI language, and a duplicate surface for the typesetting-language selector owned by
  `tabs/settings/typesetting/`) shared by the studio settings tab AND the launcher settings page:
  `ms-settings-ui`'s `general_settings_panel.rs`. Per-UI
  `GeneralSettingsPanelState` + a returned `GeneralSettingsOutcome`; synchronous persistence to
  `user_config.json` through `config::update_user_config_file`, except the typesetting
  language, which is written off-thread through `ms_config::save_text_language`.
- Global interface scale (`General.ui_scale_percent`, 50-200 %): also `ms-settings-ui`'s `general_settings_panel.rs`.
  It is a `Context::set_zoom_factor` call, so it rescales a whole window (fonts, spacing, widget
  sizes) without touching the OS window size. The live value is the process-global
  `ui_scale_percent()`, seeded once in `run_main` (`seed_ui_scale_from_user_settings`) and updated by
  the slider — NOT re-read from the startup `user_settings` snapshot, which `run_main` reuses for
  every window of the session. Each `run_native` constructor closure that should honor it calls
  `apply_ui_scale_to_context` next to `ui_fonts::install*` (today: studio `run_main_window` +
  `launcher::run`); a window that does not call it renders at native size. The shared
  typesetting-language selector itself is the public `general_settings_panel::draw_text_language_setting(ui, id_salt)`,
  called by both this widget and the studio "Тайп" pane (`tabs/settings/typesetting/`).
- Menu-level shared layer for the two settings surfaces (launcher settings page + studio settings
  tab): `ms-settings-ui`'s `settings_shared.rs`. Holds the section registry (`SettingsSectionId`, `SettingsSurface`,
  `SettingsSectionDescriptor`, `SECTIONS`, `sections_for`, `title_key` — the existing per-surface
  localization keys) and `SharedSettingsPanels`, which owns the three shared double-interface panel
  states (General / AiBackend / Tutorials) and renders them via `draw`. It also RE-EXPORTS
  `SettingsDeepLink` (declared in `ms_config::settings_deep_link`): the requester is crate
  `ms-tab-typing` and the consumer is `SettingsTabState::navigate_to`, so the enum itself had to
  sit below both. It does NOT own the
  `AiBackendHandle` (passed to `draw` by reference) and does NOT merge the two per-surface state
  containers; each surface keeps its exclusive sections and renders them itself.
- AI install-type detection from installed Python packages: `ai_install_probe.rs`.
- App-managed AI model coverage, Hugging Face paths, or lazy download behavior: `crates/ms-sysprobe/src/ai_models.rs`.
- Native ONNX Runtime path (MangaOCR + PaddleOCR OCR, PaddleOCR text detection; runtime/engine
  loading, provider selection, SIGILL crash-guard), or the onnxruntime dylib resolver/downloader:
  crates `ms-native-runtime` and `ms-onnx-runtime`. Runtime via `General.ai_runtime`, provider/device via the
  unified `General.ai_onnx_provider`/`ai_onnx_device_id` (shared with the backend); OCR routing lives
  in `crates/ms-tab-translation/src/ocr.rs::ocr_route`, detection routing in
  `crates/ms-tab-translation/src/text_detector/pipeline.rs::detector_native_route` (its disk
  inputs are read by `text_detector/native.rs::current_detector_route`).
- ONNX selection UI (shared Settings + launcher panel): `ms-settings-ui`'s `ai_backend_panel.rs`. The section is
  RUNTIME-BRANCHED on `General.ai_runtime`:
  - Native → the BUILD-based selection (Билд → EP → Устройство). The "Билд" combo lists the
    `onnx_runtime::builds` catalog grouped by availability — Базовые (available Basic), Специфичные
    (available Specific), Недоступные (everything unavailable + the informational QNN, which is
    display-only/non-selectable; other unavailable builds stay selectable for a forced download). The
    EP combo comes from `builds::build_execution_providers(build)` (token via `ep_ort_token`,
    round-tripping through `execution_provider_from_ort_token`); the device combo adapts per EP
    (DirectML/WebGPU adapter indices, CUDA/TensorRT `GPU 0`, CPU/CoreML default, OpenVINO device-TYPE
    strings `CPU`/`GPU`/`NPU` written verbatim to `ai_onnx_device_id`). The selected build persists via
    `ms_config::save_onnx_build` (`General.ai_onnx_build`); the EP/device via `save_onnx_provider_device`.
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
  legacy bubble format migration: `crates/ms-project/src/lib.rs`.
- Root editor wiring, shared model setup, texture upload budgets, page/overlay loader behavior,
  active tab routing, viewport sync, or global hotkeys: `app.rs`.
- Studio window startup shell, background project load, loading/error screens: `studio_bootstrap.rs`.
- Canvas layout, zoom, scrolling, bubble interaction, overlay runtime, or canvas settings:
  `canvas/`.
- Shared bubble, clean overlay, or text detector mask state: `models/`.
- Translation OCR, text detection, machine translation, backend health panels, or translation
  canvas hooks: `crates/ms-tab-translation/src/`.
- Cleaning tools, quick-clean, overlay edit commits, mask loading, or cleaning canvas behavior:
  `crates/ms-tab-cleaning/src/`.
- Typing overlays, text rendering integration, text masks, deformation, image/text export, or
  auto-typing: `crates/ms-tab-typing/src/`.
- The layered single-page editor: `crates/ms-tab-ps-editor/src/`. The page grid and structural
  page dialogs: `crates/ms-tab-page-manager/src/`.
- Characters, terms, notes, or wiki UI: `crates/ms-tabs-simple/src/`. Settings UI: `tabs/settings/`.
- Launcher pages, project import/export, settings, detached new-project workflow, PSD import, or
  launcher theme/state: crate `ms-launcher`.
- Installer/update worker behavior, dependency setup, elevation, shortcuts, or uninstall:
  crate `ms-installer`.
- UI fonts (which files a window loads, family names, fallback order, or a new `run_native`
  entry point that needs fonts): `ms_widgets::ui_fonts` and `fonts/ui/MODULE_README.md`. Never
  `Context::set_fonts` — see the contract note above.
- Reusable UI controls: the `ms-widgets` crate; keep them independent of durable project state.
- Diagnostic binaries: `bin/`; keep production runtime dependencies in library modules instead of
  hiding behavior in test binaries.
