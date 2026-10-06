# ARCHITECTURE — ManhwaStudio

Whole-project architecture map: layers, crate dependency direction, cross-layer data flow,
shared models, background pipelines and the invariants that span crates. It is not a changelog
and not a module guide: every directory with source code has a `MODULE_README.md` that owns its
detail, and this file only points there. Working rules for agents live in `PROJECT_RULES.md`.

## 1. System overview

ManhwaStudio is a desktop editor for translating comics (manga / manhwa): a chapter is
translated (bubbles, OCR, machine translation), cleaned (clean layers, inpainting), typeset
(rendered text layers) and exported.

- **Rust application** — a Cargo workspace: the thin binary `manhwastudio_rs` (`src/`) over 38
  `ms-*` library crates in `crates/`. Builds for Linux and Windows (mandatory), macOS, and
  `wasm32` (web entry, `src/web_entry.rs`).
- **Python AI backend** — an optional separate process (`ai_backend.py` + `modules/ai_backend/`)
  for torch-based models. The app also has an in-process native ONNX runtime, so most basic AI
  features work without Python.
- `old_or_test/` holds the archived 2.x Python UI: never an architecture reference.

```text
main.rs / args.rs  (CLI, startup routing, process-global services)
  -> ms-launcher (project catalogue, New Project, import)   -- separate eframe window
  -> studio_bootstrap.rs (loads ProjectData off the GUI thread behind a loading screen)
  -> MangaApp (src/app.rs: editor root, owns tabs, shared models, loader pool, panel dock)
       -> shared models: BubblesModel, CleanOverlaysModel, TextMaskModel, LayerDoc
       -> tabs: CanvasView + CanvasHooks (Translation, Cleaning, Typing) and own-UI tabs
       -> background workers; native ONNX runtime; Python backend over framed IPC
```

## 2. Layers and crates

Crates exist so the build compiles in parallel; the compiler, not convention, enforces the
layering. Dependencies point DOWN only (`<-` = "depends on"); a level may use any lower level;
crates on one line do not know each other.

```text
bin (src/: main.rs, app.rs, studio_bootstrap.rs, tabs/settings/, web_entry.rs)
  <- ms-launcher / ms-tab-page-manager
  <- ms-settings-ui / ms-tab-cleaning / ms-tab-ps-editor
  <- ms-tab-translation / ms-tab-typing
  <- ms-tools
  <- ms-canvas
  <- ms-models / ms-tabs-simple / ms-installer / ms-ai-api
  <- ms-project / ms-widgets / ms-native-runtime
  <- ms-page-ops / ms-sysprobe / ms-onnx-runtime / ms-window-geometry / ms-os-integration
  <- ms-config / ms-text-render
  <- ms-backend-ipc / ms-docstore / ms-fonts / ms-memory / ms-onnx
  <- ms-log / ms-text-detect
  <- ms-actions / ms-gifs / ms-i18n / ms-raster / ms-storage / ms-text-util / ms-theme / ms-thread
```

Non-obvious edges: `ms-tools` sits ABOVE `ms-canvas`; `ms-widgets` does NOT depend on
`ms-models`; `ms-models` depends on `ms-text-render` (layer effects). Tab-to-tab edges exist and
point down only: page-manager -> ps-editor -> typing; cleaning -> translation -> tabs-simple;
`ms-settings-ui` and `ms-launcher` also use `ms-tab-translation`. The studio never references
`ms-launcher`; only `main.rs` / `web_entry.rs` start it.

**Foundation (levels 0-2)**
- `ms-thread` — `std::thread` surface; Web Workers on wasm.
- `ms-storage` — object-safe sync `Storage` seam (desktop passthrough, sandboxed native,
  in-memory web). Raw writes are not atomic.
- `ms-i18n` — UI localization: `t!` / `tf!` / `tp!`, embedded catalogs (en, ru, es, fr, pt);
  `en` is reference and fallback.
- `ms-text-util` — hanging-punctuation set, segmentation, typesetting language, hangul.
- `ms-actions` — GUI-free undo/redo engine (`ReversibleAction`, `ActionHistory`, optional
  `RasterDiff`).
- `ms-theme` — sole owner of semantic colours over egui's dark theme.
- `ms-gifs` — embedded animated hints, streaming decoder.
- `ms-raster` — generic raster primitives with one owner each: polygon scanline fill
  (re-exported as `ms_tools::fill_polygon_spans`), square binary dilation, Otsu threshold,
  integer replicate upscale / box downscale, single-channel box blur, RGBA-over-white to RGB
  (used by PDF export and JPEG encoding).
- `ms-log` — session log (`runtime_log`) and opt-in trace log (`trace_log!` / `trace_scope!`).
- `ms-text-detect` — GUI-free text-detection domain (block sort and cap, mask normalization, DB
  postprocess, glyph mask, per-engine scale/tiling plan, tile stitching and the runner pipeline
  with the CTD, Paddle and Surya postprocess); the model forward pass is supplied by the caller
  through `ProbMapRunner`. No logging, wasm-buildable.
- `ms-docstore` — named logical documents (`DocRef`): per-document lock, atomic writes, JSON or
  SQLite. Must not depend on `ms-config`.
- `ms-fonts` — the bundled `fonts/ui` stack, shared by egui UI fonts and the text renderer.
- `ms-memory` — image-memory profiles and eviction policy; owns no pixels.
- `ms-onnx` — onnxruntime (`ort`, load-dynamic) inference: MangaOCR, PaddleOCR.
- `ms-backend-ipc` — framed IPC client for the Python backend.

**Core (levels 3-6)**
- `ms-config` — runtime root (`data_dir` / `program_dir`), `user_config` sections and defaults,
  `ConfigSaver`, storage-mode startup, `AppTab`, version formatting.
- `ms-text-render` — the production text renderer (cosmic-text shaping, vector-first glyph
  raster, effects); re-exported to the typing crate as `render_next`.
- `ms-page-ops` — journalled structural page operations; owns `Page` and `ProjectPaths`.
- `ms-sysprobe` — system probes, app-managed AI model catalogue, Hugging Face token;
  `python_manager` is the only way to find or spawn Python.
- `ms-onnx-runtime` — locates, downloads and loads the native onnxruntime library per build.
- `ms-window-geometry` — startup monitor and window geometry; the only direct `winit` user.
- `ms-os-integration` — the OS records of a program copy (Windows Uninstall / App Paths /
  "Open with" keys, `.lnk` shortcuts, elevation probe and UAC relaunch; Linux `.desktop` entry
  and icon): names, expected values, writes and removal, the read-only probe (`report`) and the
  action rules + elevated helper (`actions`). GUI-free. Consumers: the installer (per install
  kind), the binary (Linux startup entry, helper routing), `ms-settings-ui` (registration
  warnings) and `ms-launcher` (System registration tab).
- `ms-project` — `ProjectData`, `ComicType`, `CanvasSettings`, project scan, save-merge.
- `ms-widgets` — project widget set (`panel_dock`, `Wheel*`, `AiButton`, spellchecked edits),
  `ui_fonts`, hotkey registry (`InputManagerV2`).
- `ms-native-runtime` — process-global native ONNX manager (sessions, LRU, crash guard).
- `ms-models` — shared cross-tab state (`BubblesModel`, `CleanOverlaysModel`, `TextMaskModel`,
  `layer_model::LayerDoc`, `page_view`, `AutosaveGate`, `clean_assign`). Holds egui data types
  but never takes `Ui` / `Painter`.
- `ms-tabs-simple` — Characters, Terms, Notes, Wiki tabs.
- `ms-installer` — install/update, Python environment, `venv_check`.
- `ms-ai-api` — the multi-provider LLM API layer over `genai` (re-exported): services
  (including OpenAI- / Anthropic-compatible endpoints at a user base URL, key optional),
  client, keyring key storage, model listing, account status, the hard-coded image-input
  capability table (`model_caps`: supported / not supported / unknown), and the shared connection widget
  (`AiApiConnectionState`, `draw_connection`, `AiApiTaskRunner`). Consumers build their own
  requests; persistence of the selected service/model stays with the consumer.
  `image_edit/` is the GUI-free cloud image-EDIT layer: provider/model catalog (size rule per
  model, mask support, extra reference images, Russia availability), one adapter per API shape, a single native
  executor (no retry of paid POSTs, key only to the provider's own hosts) and the exact-size
  pipeline — integer ×k replicate up / box down only, output must be exactly k·W×k·H, composite
  only inside the mask. The size VALIDITY decision stays with the cleaning frame
  (`region_edit_v2::geometry`); `image_edit` carries rule data mapped 1:1 onto it.

**UI engines and tabs (levels 7-11)**
- `ms-canvas` — the shared page + bubble canvas; declares `CanvasHooks`.
- `ms-tools` — raster tool primitives (mask brush, polygon spans, SOR kernel, `patch` core).
- `ms-tab-typing`, `ms-tab-translation`, `ms-tab-cleaning`, `ms-tab-ps-editor`,
  `ms-tab-page-manager` — one tab each, UI included.
- `ms-settings-ui` — settings panes shared by studio and launcher, the AI backend supervisor,
  the storage-mode job, the `tutorial` engine.
- `ms-launcher` — the launcher application.

**Outside the `ms-*` graph**
- `crates/puffin_egui` — vendored profiler UI, compiled only with the root `profiling` feature.
- `egui-shader-layers`, `egui-large-image`, `ag-psd` — standalone external crates (crates.io
  dependencies in `[workspace.dependencies]`); their local checkouts under `crates/` are
  gitignored. `egui-large-image` does the tiled display of very large images (launcher ribbon
  and crop editor, page-manager viewer).

## 3. Startup and process lifetime

- `src/main.rs::run_main` runs the launcher and the studio as **sequential `eframe::run_native`
  windows in one process**. Process-global services created there outlive both windows: the
  `AiBackendSupervisor` (backend process and health/device probe), the UI-scale slot, the
  `ms-fonts` byte cache, the Hugging Face token cache.
- Startup order that later code relies on: `ms_config::storage_mode::init_storage_mode_at_startup`
  is the process's FIRST document access (seeds the docstore default format); the on-disk
  locale catalog is reconciled before user settings load.
- **Runtime root:** `ms_config::data_dir()` / `program_dir()` = the launch directory if it holds
  the program markers, else the executable directory if it does, else — for a repository build
  (`<repo>/target/<profile>/<exe>`) — `<repo>` if it does (rules: `ms_config::runtime_root`,
  also the one owner of the repository-build rule `ms-os-integration` writes records with;
  macOS `.app` bundles use Application Support). Bundled resources, the Python
  environment, `user_config`, logs and app-managed models all resolve from it.
- `StudioBootstrapApp` (`src/studio_bootstrap.rs`) loads the project off the GUI thread, owns
  window geometry and the GL lifetime, then delegates to `MangaApp`. It receives a typed
  `StudioOpenRequest`: a project directory, or a single image (CLI `--image` / positional path,
  launcher `OpenImage`, OS "Open with").
- **Reload rebuilds `MangaApp` inside the same `egui::Context`.** Any egui resource a crate
  registers (font families, textures, ids) must be named by content, never by an instance
  counter.
- **GL lifetime:** the binary calls `egui_shader_layers::install_glow` in the app creator and
  `destroy_glow` on exit; no `ms-*` crate owns GL objects.

## 4. Project data model

- A project opened in the studio is one **chapter** inside a **title** directory.
  `ms_project::ProjectData::load` returns the chapter directories, `pages: Vec<Page>`, the
  bubble list, `ProjectPaths`, `comic_type`, `canvas_settings` and `settings_data`.
- **Single image mode:** `ProjectData::session()` is a typed `SessionKind` set by the caller,
  never inferred from paths. For `SingleImage`, `ms_project::single_image` decodes the image
  (EXIF orientation applied) into a throwaway scratch chapter under the OS temp dir, and the
  regular load runs on that scratch — never on the user's folder. Writers stay live against the
  scratch; the user's file is written only by explicit Save / Save As (flattened composite via
  the typing flatten, atomic write). `run_main` owns the scratch and deletes it after the window
  closes; stale scratches (marker + released lock) are swept at startup. UI branches
  (tabs, top bar, exit dialog) live in `src/single_image/`.
- `Page` and `ProjectPaths` are declared in `ms-page-ops` and re-exported by `ms-project`, so a
  pending page-op journal is resolved (`recover_pending_page_op`) before any reconcile or load
  pass. Every path is an `ms_config` name joined onto the chapter or title directory.
- **Staging rule:** all in-session writes go to `{title}/{chapter}_unsaved/` (bubbles, clean
  layers, layer document, image bubbles). The committed chapter changes only through the
  explicit save-to-project merge (`ms_project::save_merge`). Loaders read staged-over-committed.
- **Owned documents** (the ten `ms_docstore::DocKind`s: user config, fonts data, font presets,
  project settings, characters, terms, character favourites, colour presets, layers, bubbles)
  are touched only through `ms-docstore`.
- **`Bubble` wire contract** (shared by `ms-project`, `ms-models`, `ms-canvas`, the translation
  and typing tabs and `ms-page-ops::json_remap`): `id`, `img_idx`, image-normalized anchor
  `img_u` / `img_v`, `side`, `bubble_class` (`text` / `image` / `hint`; unknown reads as `text`),
  `bubble_type`, `text`, `original_text` and a flattened `extra` map. Class-specific data lives
  in `extra`; its semantics are owned by `ms-canvas` (see its MODULE_README).
- **Layer document:** `ms_models::layer_model::LayerDoc` is the per-chapter layer tree
  (`layers.json` document): raster layers of the PS editor and inline text nodes of the typing
  tab. Legacy `text_images/` + `text_info.json` is read-only input, migrated in
  `ms-models::layer_model`.
- `ComicType` (`Pages` / `Ribbon` / `Custom`) is a preset over two `CanvasSettings` fields.

## 5. Root runtime and shared models

- `MangaApp` (`src/app.rs`) creates the chapter models, each `Arc<Mutex<_>>`, and injects them
  into tabs and canvases through typed setters: `BubblesModel`, `CleanOverlaysModel`,
  `TextMaskModel`, `LayerDoc`. Models carry revision (or per-node generation) counters; tabs pull
  changes by revision and never copy shared state by hand. Lock briefly, snapshot, release.
- **Page loader pipeline:** one background decode pool for source pages AND clean overlays (page
  order) -> strictly ordered promotion on the GUI thread -> tiled GPU upload under a per-frame
  budget. Page dimensions come from `PageImageInfo`; GPU textures are a droppable cache.
- **Texture lending:** `MangaApp` owns the resident `PageTexture` map and lends it (`&mut`) to
  the active tab each frame — canvas tabs through the canvas draw params, the page-manager
  viewer through its `draw`. Each tab reports the source-page window it needs
  (`active_source_page_window`), which drives residency and eviction.
- `BubblesModel` — bubble list, `revision`, `canvas_revision` and `SharedCanvasSettings`
  (cross-tab canvas settings persisted to the project settings and to `user_config`).
- `CleanOverlaysModel` — per-page clean overlay kept twice and updated together: premultiplied
  `egui::ColorImage` for display and straight-alpha `RgbaImage` for tools, export and PNG.
  Several canvases (Cleaning, Typing) read it: deltas are NON-destructive, each consumer asks
  for pages changed since its own revision, so no reader can steal another's changes. Writers
  (canvases, PS editor) adopt the model revision only when no foreign bump happened.
- `TextMaskModel` — detector masks and boxes per page; the detector writes, Cleaning reads.
- **Page <-> clean binding has one owner:** `ms_page_ops::clean_binding` (pure rule: canonical
  `<stem>.png`, exact-size fit) + `ms_models::clean_assign` (worker I/O, staged-over-committed).
  Loader, writers, page ops, Cleaning status, page manager and typing export all route through
  it.
- **Undo** everywhere uses `ms-actions`: bubbles as snapshot ops, clean overlays and PS-editor
  rasters as tiled `RasterDiff`s bounded by count and a memory-profile byte budget.
- **Panel dock:** one `PanelDockState` per studio window, owned by `MangaApp` and lent to the
  active tab per frame; layouts are keyed by `AppTab::key()` and persisted through the
  `PanelLayout` config section.
- **Hotkeys:** `ms_widgets::InputManagerV2` (code-declared specs, user overrides in
  `user_config` -> `Hotkeys`), dispatched by `MangaApp` per active tab.

## 6. Canvas

- `ms-canvas` is the one page + bubble viewer used by Translation, Cleaning and Typing.
  Tabs extend it only through `CanvasHooks` (declared in `ms-canvas`, implemented by tabs):
  page overlays, bubble chrome, context menus, status style, scrollbar marks, gesture capture.
  Hooks are cheap and never mutate shared models while the canvas holds them.
- Canvas bubble edits are pushed into `BubblesModel` (debounced upserts); the canvas pulls model
  changes by revision. `SharedCanvasSettings` flows the same way and is persisted by a canvas
  settings saver thread.
- Layout is in stable world coordinates (unscaled page sizes); zoom is a camera transform.
  Cross-tab viewport sync passes `CanvasViewportSnapshot { zoom, scroll_x_from_center,
  scroll_y, laid_out }`; the horizontal position is measured from the centered offset, so it is
  independent of each canvas' strip width.
- Overlays upload tiled through a background prepare thread; the GUI thread never decodes or
  composes.
- Bubble text spellcheck follows the typesetting language, not the UI language. Bubble status
  borders: rules in `ms_config::bubble_status` (GUI-free), painting in `ms_widgets::bubble_status`.

Detail: `crates/ms-canvas/src/MODULE_README.md`.

## 7. Tabs

`AppTab` (`crates/ms-config/src/app_tab.rs`) fixes order and persistence ids. `MangaApp` owns
every tab state, lends it shared models, textures and the panel dock each frame, and executes
the actions tabs return. Canvas tabs implement `CanvasHooks`; the others draw their own UI.

- **Page Manager** — `ms-tab-page-manager` (+ engine `ms-page-ops`): page grid, clean cards,
  full-resolution viewer. Returns `PageManagerAction` (structural op, open page in a tab).
  **Structural ops (move, insert, blank, delete, stitch, split, crop) are not staged:** pages are
  position-keyed, so each op is a journalled crash-safe transaction that remaps every page-keyed
  artifact in both the committed and `_unsaved` trees. The app quiesces every chapter writer,
  runs the op on a worker, then rebuilds the app from disk via `StudioBootstrapApp`. Save,
  export and page ops gate each other.
- **Translation** — `ms-tab-translation`: bubbles, OCR, text detection, machine translation
  (AI API OCR and MT go through `ms-ai-api`).
  Talks to the backend only through `ms-backend-ipc`; owns the backend health probe.
- **Cleaning** — `ms-tab-cleaning`: clean overlays, brush / region tools, two region-edit tools
  on ONE generic host (`tools/region_edit_v2`: `RegionEditHost` + `HostSpec`, engines behind
  `AiEngine`) — the local AI region editor (`tools/ai_editor`) and the cloud «ИИ редактирование
  (API)» (`tools/ai_api_editor`, no Torch, over `ms_ai_api::image_edit`) — and the watermark
  engine. Writes only through `CleanOverlaysModel`.
- **Typing** — `ms-tab-typing` (+ renderer `ms-text-render`): text layers in `LayerDoc`, masks,
  export (PNG, PSD, lossless PDF).
- **PS Editor** — `ms-tab-ps-editor`: single-page layered raster editor, NOT a `CanvasView`
  (own camera, layer stack, tools, tiled GPU cache). Base layers are the source page and its
  clean; user layers live in `LayerDoc`. Colour correction is a GPU shader pass
  (`egui-shader-layers`).
- **Characters / Terms / Notes / Wiki** — `ms-tabs-simple`: title documents via `ms-docstore`;
  Notes assembles a prompt from characters and terms; `CharactersChanged` makes the app notify
  Notes and Translation. Wiki is shared with the launcher.
- **Settings** — `src/tabs/settings/` + panes in `ms-settings-ui` (General, AI backend,
  Tutorials are double-interface widgets also used by the launcher); in-app deep links via
  `navigate_to(SettingsDeepLink)`.

## 8. Text rendering and fonts

- `ms-text-render` is GUI-free. Fonts reach it BY IDENTITY NAME (PostScript name) through
  `FontProvider`, never by path.
- Rendering never consults OS fonts: one process font database over the bundled `fonts/ui`
  stack plus caller-registered fonts, with a fixed `en-US` locale, so output is
  machine-independent.
- The `ms-fonts` manifest is process-global and project-independent; only `ui_fonts` honours a
  title-local `fonts/ui` override, so a project can never change final render output.
- UI language (`General.ui_language`, `ms_i18n::LocaleTag`) and typesetting language
  (`TextTab.text_language`, `ms_text_util::language::TextLanguage`) are independent;
  hyphenation, segmentation and font coverage follow the TEXT language.

Detail: `crates/ms-text-render/src/MODULE_README.md`, `crates/ms-tab-typing/src/MODULE_README.md`.

## 9. AI: native runtime and Python backend

- **Two runtimes.** In-process native ONNX (`ms-onnx` sessions, `ms-onnx-runtime` library
  resolver/downloader, `ms-native-runtime` manager) and the out-of-process Python backend.
  `ms_config::AiRuntime::from_user_settings` picks the effective one: native unless the user
  explicitly chose the backend (`General.ai_runtime_configured`).
- **Backend boundary.** Framed, multiplexed IPC (`ms-backend-ipc`): AF_UNIX socket on unix,
  token-authenticated loopback WebSocket on Windows. Every Rust caller uses
  `ms_backend_ipc::shared_client()`. Method and topic names live in
  `crates/ms-backend-ipc/src/protocol.rs`. `PROTOCOL_VERSION` (mirrored in
  `modules/ai_backend/ipc/protocol.py`, a test guards the mirror) is the only compatibility
  gate, compared in the `hello` handshake; it also covers the on-disk `user_config` semantics.
- **Process ownership.** One app-global supervisor (`ms_settings_ui::ai_backend_supervisor`)
  starts and stops the backend for studio and launcher; autostart waits for storage-mode
  conversions. `ms_tab_translation::backend_health` keeps the pushed health snapshot that AI
  actions (`AiButton`) gate on.
- **Backend package.** `server.py` is the composition root; handlers reach services only through
  `AppState` fields (their names are contract). Dependency direction: domains (`ocr`,
  `detection`, `inpaint`, `reline`, `translate`, `watermark`, `browser`) -> `engines/` ->
  `runtime/`. Detail: `modules/ai_backend/MODULE_README.md`.
- **One ONNX selection** (`General.ai_onnx_build` / `ai_onnx_provider` / `ai_onnx_device_id`)
  drives both runtimes. An unavailable accelerator falls back to the CPU build with a log; a
  different build needs an app restart.
- **Crash guard.** Every onnxruntime library load is bracketed by persisted markers
  (`ms_config::ort_load_guard`); a scope that crashed is `Suspect` at next launch and routes to
  the backend.
- **Routing.** Native covers MangaOCR, PaddleOCR recognition, PaddleOCR detection and Baberu OCR
  (vision on the selected EP, int8 decoders always on CPU; backend `ocr.baberu` is its fallback); pure
  routers in `ms-tab-translation` decide. Native routes need no backend; a native failure is
  logged and falls back to the backend when it is up. Native code runs only on worker threads
  and is compiled out on wasm.
- **Text detection is forward-only on the backend.** Rust (`ms-text-detect`) plans scale and
  tiling, cuts equal-size RGB tiles and post-processes the stitched maps;
  `textdetector.{ctd,paddle,surya}.forward` only runs the network and returns u8 probability
  maps (wire codec: `ms_backend_ipc::textdetector`).
- **App-managed models** live under `ManhwaStudio_AI_Models/` and are fetched by Rust through
  `ms_sysprobe::ai_models` before a backend feature initializes them. Third-party OCR models
  (Baberu OCR, the PaddleOCR-VL variants) come from the pinned external catalog
  (`ai_models::external_catalog`: repo, commit, per-file size + sha256, `side_models/` dir) — the
  one owner of those facts; Rust downloads them explicitly (never implicitly on use) and sends the
  backend absolute paths, so the backend only loads them offline. Other `side_models/` models
  (FLUX, watermark, Reline) are still downloaded by the backend.
- **Downloaded code.** PaddleOCR-VL model code is vendored (`ocr/paddle_vl_vendor/`, no
  `trust_remote_code`). `modules/ai_backend/watermark/` is the only place that executes
  network-downloaded code (pinned commit, SHA-256 verified, hashed bytes are executed bytes).

## 10. Cross-cutting services

- **Config** (`ms-config`) — paths, defaults trees, section writers. Each `user_config`
  mutation is one `ms_docstore::update`. GUI-driven sections (`Window`, `PanelLayout`) are
  self-versioned, owned by their crates, and written through the debouncing, retrying
  `ConfigSaver`.
- **Storage mode** — `General.storage_mode`: `prod` -> SQLite fragment store `.db` (default),
  `dev` -> `.json`; wasm is dev-only. The global switch (`ms_project::storage_mode::convert_globals`,
  one job slot in `ms_settings_ui::storage_mode_job`) converts globals and titles, `user_config`
  last as the sentinel; chapters keep their format. Python touches only `user_config`, through
  repo-root `docstore.py`, which mirrors the Rust codec (golden fixtures in
  `crates/ms-docstore/fixtures/`). Known limits: `dev-docs/known_gaps.md`.
- **i18n** (`ms-i18n`) — UI text only via `t!` / `tf!` / `tp!`; the editable on-disk catalog is
  reconciled at startup by `ms_config::locale_store` (adds missing keys, never overwrites or
  deletes).
- **Log** (`ms-log`) — session log `last.log` / `previous.log` and opt-in trace log, each with
  its own writer thread; callers pass the directory in.
- **Credentials** — secrets live only in the OS keyring, never in a config file: AI API keys
  (`ms_ai_api::keys`; per service, per service + base URL for compatible endpoints, and frozen
  `image_edit:{provider}[@{region}]` names for image-edit providers) and the Hugging Face token (`ms_sysprobe::hf_token`, a cached
  process-wide value seeded off-thread). Keyring I/O never runs on the GUI thread; a token is
  never logged and reaches the backend only as a per-request field.
- **Threads** — spawned through `ms_thread` on any path that also runs on wasm; `rayon` for CPU
  work; `tokio` only in the binary and `ms-ai-api`.
- **Versions** — `MS_APP_VERSION` (git-derived in `build.rs`) is display-only;
  `CARGO_PKG_VERSION` is what is compared and parsed; cross-process comparisons reduce both via
  `version_format::version_core`. Libraries receive the version as `ms_installer::HostVersion`.

## 11. Background pipelines

Rule: any heavy operation runs on a worker with an explicit lifecycle (start, poll, cancel,
logged error); the GUI thread only polls and applies results.

| Pipeline | Owner |
|---|---|
| Page + clean-overlay decode pool, ordered promotion, GPU upload | `src/app.rs` |
| Studio project load behind the loading screen | `src/studio_bootstrap.rs` |
| Bubble and layer persistence (coalescing savers) | `ms-models` (`bubbles_model.rs`, `layer_model/saver.rs`) |
| Clean overlay autosave, canvas settings saver, overlay tile prepare | `ms-canvas/src/workers.rs` |
| Config sections `Window` / `PanelLayout` | `ms-config::config_saver` |
| OCR, text detection, MT, crop recognition, backend health | `ms-tab-translation` |
| Text detection plan / tiles / stitch / postprocess (one plan for worker and panel notice) | `ms-text-detect`, driven by `ms-tab-translation/src/text_detector/` |
| Native ONNX load and inference | `ms-native-runtime` (called from those workers) |
| AI backend process supervisor | `ms-settings-ui/src/ai_backend_supervisor.rs` |
| Storage-mode conversion job | `ms-settings-ui/src/storage_mode_job.rs` |
| Structural page ops, thumbnails, viewer clean decode | `ms-tab-page-manager` + `ms-page-ops` |
| Typing live render (latest-wins), save, export | `ms-tab-typing/src/tab/` |
| PS editor page decode, raster effects | `ms-tab-ps-editor` |
| Cleaning region loader, inpaint runs, quick text clean | `ms-tab-cleaning` |
| Cloud image-edit run (submit, poll, download, exact-size finish) | `ms-tab-cleaning/src/tools/ai_api_editor` + `ms_ai_api::image_edit` |
| Characters / Notes / Wiki background work | `ms-tabs-simple` |
| Batch processing of new projects | `ms-launcher/src/new_project/batch_processing/` |
| System-registration probe / actions / elevated helper (`ShellExecuteExW` round trip + result file) | `ms-os-integration` (`report`, `actions`, `windows/elevation.rs`), driven by `ms-launcher/src/pages/system_registration.rs` and `settings_warnings` |
| Settings warnings checks (full run per launcher entry, per-unit rechecks) | `ms-settings-ui/src/settings_warnings/`, driven by `ms-launcher/src/pages/settings_page.rs` |

## 12. Key invariants

**Threading**
- The GUI thread never blocks: file and network I/O, image decode, saver barriers and worker
  joins run on workers. Backend failures never hang the GUI; AI actions gate on the pushed
  health snapshot (`AiButton`); long runs do a one-shot `check_ai_backend_health` first.
- No model lock is held across I/O, rendering, callbacks or a worker wait.

**Shared state**
- Cross-tab state lives only in the `Arc<Mutex<_>>` models owned by `MangaApp`.
- `CanvasView` is the one canvas engine; tab behaviour plugs in via `CanvasHooks`.
- One owner per rule: the page <-> clean binding (`clean_binding` + `clean_assign`), Python
  lookup and spawn (`ms_sysprobe::python_manager`), semantic colours (`ms-theme`), `winit`
  (`ms-window-geometry`), OS integration records (`ms-os-integration`), glyph rendering fonts
  (`ms-fonts` + `ms-text-render`), layer composite order / visibility / group fold
  (`ms_models::layer_model::ordering`, consumed by the typing canvas, the typing
  flatten/export and the PS composite, layers tree and structural order).
  Layer and group visibility/opacity are `LayerDoc` facts, never one tab's view state.

**Documents**
- Owned documents are touched only through `ms-docstore`: one `update` per read-modify-write;
  the per-document lock is non-reentrant; a malformed document is never overwritten (only
  `quarantine` moves it aside).
- An unparsable staging document marks the session damaged: resume is refused naming the file,
  nothing is auto-deleted.
- `serde_json` `preserve_order` must never be enabled anywhere in the workspace (feature
  unification would change every crate's document bytes); `float_roundtrip` stays on.

**Staging and autosave** (`{chapter}_unsaved`)
- Three background writers — layer saver, bubbles saver, clean-overlay autosave — share ONE
  `AutosaveGate` per project instance. They hold edits in memory and write when the gate is due
  (interval since the first pending action, or an action threshold; one action = one gesture).
  A crash loses at most the open window.
- Durability of a queued write comes ONLY from a saver barrier, which never runs on the GUI
  thread. Force points: save-to-project, page op, typing export, exit.
- A flush that could not run is never "nothing to save": save-to-project ABORTS on any
  unverified flush or barrier (nothing merged, staging kept, user told); a page op aborts on a
  failed lock or overlay flush, but only logs a warning when the layer-saver barrier reports
  failed pages (see `src/MODULE_README.md`, save/page-op choreography).
- DISCARD never flushes: it drops pending state in all three writers before deleting staging
  (a docstore write would re-create the directory) and latches `discarding_unsaved_changes`; a
  failed cleanup releases the latch and restarts all three writers.
- Exit: deferred typing edits are enqueued inline BEFORE the exit barrier; failed pages are
  reported.
- A single-image session never merges staging: every close takes the DISCARD quiesce (no
  flush, no delete job); `run_main` removes the scratch afterwards. Its dirty state is
  `AutosaveGate::action_count()` against the baseline of the last successful file write.

Detail: `src/MODULE_README.md`, `crates/ms-models/src/MODULE_README.md`,
`crates/ms-models/src/layer_model/MODULE_README.md`, `crates/ms-tab-typing/src/MODULE_README.md`.

**Versions and protocol**
- Program versions are compared only via `CARGO_PKG_VERSION`; `MS_APP_VERSION` is display-only.
- Rust <-> Python compatibility is decided solely by `PROTOCOL_VERSION` in `hello`; any contract
  change bumps it on both sides (rule: `PROJECT_RULES.md`).
