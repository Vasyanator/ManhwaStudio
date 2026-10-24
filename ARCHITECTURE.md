# ARCHITECTURE — ManhwaStudio

Whole-project architecture map: layers, crate dependency direction, cross-layer data flow,
shared models, background pipelines and the invariants that span crates. It is not a changelog
and not a module guide: every directory with source code has a `MODULE_README.md` that owns its
detail, and this file only points there. Working rules for agents live in `PROJECT_RULES.md`.

## 1. System overview

ManhwaStudio is a desktop editor for translating comics (manga / manhwa): a chapter is
translated (bubbles, OCR, machine translation), cleaned (clean layers, inpainting), typeset
(rendered text layers) and exported.

- **Rust application** — a Cargo workspace: the root package `manhwastudio_rs` (`src/`: a
  library holding the application, plus a thin binary entry) over 47 `ms-*` library crates in
  `crates/`. Builds for Linux and Windows (mandatory), macOS, `wasm32` (web entry,
  `src/web_entry.rs`; currently inactive and not kept building, `PROJECT_RULES.md`) and, in progress, Android (`dev-docs/ANDROID_PORT.md`; the library target
  exists so that build can export `android_main`).
- **Python AI backend** — an optional separate process (`ai_backend.py` + `modules/ai_backend/`)
  for torch-based models. The app also has an in-process native ONNX runtime, so most basic AI
  features work without Python.
- `old_or_test/` holds the archived 2.x Python UI: never an architecture reference.

```text
startup/ + args.rs  (CLI, RunMode, ProcessServices, startup routing; lib.rs + main.rs are thin entries)
  -> startup prompts (installer, missing environment, update)  -- own windows, once
  -> shell/ (THE main window: one eframe::run_native, screens switched in place)
       -> launcher screen: ms-launcher (project catalogue, New Project, import)
       -> studio screen: loads ProjectData off the GUI thread behind a loading screen,
          tears StudioApp down on a worker behind a Closing screen
  -> StudioApp (src/studio.rs + src/studio/: editor root; owns PageResidency, StagingWriters,
               SessionMode, Tabs, CloseFlow, panel dock; outbound StudioOutput queue)
       -> shared models: BubblesModel, CleanOverlaysModel, TextMaskModel, LayerDoc
       -> tabs: CanvasView + CanvasHooks (Translation, Cleaning, Typing) and own-UI tabs
       -> background workers; native ONNX runtime; Python backend over framed IPC
```

## 2. Layers and crates

Crates exist so the build compiles in parallel; the compiler, not convention, enforces the
layering. Dependencies point DOWN only (`<-` = "depends on"); a level may use any lower level;
crates on one line do not know each other.

```text
root package (src/: lib.rs + thin main.rs, startup/, shell/, studio.rs, tabs/settings/, web_entry.rs)
  <- ms-launcher / ms-tab-page-manager
  <- ms-settings-ui / ms-tab-cleaning / ms-tab-ps-editor
  <- ms-tab-translation / ms-tab-typing
  <- ms-tools
  <- ms-canvas
  <- ms-models / ms-tabs-simple / ms-installer / ms-ai-api
  <- ms-studio-tab / ms-region-postprocess
  <- ms-project / ms-widgets / ms-native-runtime / ms-local-ai
  <- ms-page-ops / ms-sysprobe / ms-onnx-runtime / ms-window-geometry / ms-os-integration / ms-input
  <- ms-config / ms-text-render
  <- ms-backend-ipc / ms-docstore / ms-download / ms-fonts / ms-memory / ms-onnx / ms-platform / ms-browser-bridge
  <- ms-log / ms-text-detect
  <- ms-actions / ms-browser-ext / ms-custom-fonts / ms-gifs / ms-i18n / ms-raster / ms-storage / ms-text-util / ms-theme / ms-thread
```

Non-obvious edges: `ms-tools` sits ABOVE `ms-canvas`; `ms-widgets` does NOT depend on
`ms-models`; `ms-models` depends on `ms-text-render` (layer effects); the browser bridge is used
only by `ms-tab-translation` (with `ms-browser-ext`) and by the root (version registration), and
does not depend on `ms-platform` (its consumers gate on `PlatformFeature::BrowserExtension`). Tab-to-tab edges exist and
point down only: page-manager -> ps-editor -> typing; cleaning -> translation -> tabs-simple;
`ms-settings-ui` depends on no tab crate and `ms-launcher` only on `ms-tabs-simple` (the
shared wiki view). The studio never references
`ms-launcher`; only the main window (`src/shell/`) / `web_entry.rs` host it.

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
  (re-exported as `ms_tools::fill_polygon_spans`), square binary dilation, enclosed-hole fill,
  Otsu threshold,
  integer replicate upscale / box downscale, single-channel box blur, RGBA-over-white to RGB
  (used by PDF export and JPEG encoding), exact Euclidean distance transform (text-render
  effects, region-edit feather).
- `ms-log` — session log (`runtime_log`) and opt-in trace log (`trace_log!` / `trace_scope!`).
- `ms-text-detect` — GUI-free text-detection domain (block sort and cap, mask normalization, DB
  postprocess, glyph mask, per-engine scale/tiling plan, tile stitching and the runner pipeline
  with the CTD, Paddle and Surya postprocess, and the polygons-with-holes -> 0/255 mask raster
  `polygon_mask` used for cloud detector answers); the model forward pass is supplied by the
  caller through `ProbMapRunner`. No logging, wasm-buildable.
- `ms-custom-fonts` — GUI-free custom fonts: the kind-tagged recipe (hybrid = weighted blend of
  2-4 real fonts by one of five blend algorithms, optionally split shape / spacing / kerning
  weights; combined planned), its SHA-256 key and the compiler that turns a recipe plus
  source bytes into a real TrueType face (blend, metrics, inherited `kern`, sfnt writer), and the
  export-time own-kerning rewrite. Bytes in, bytes out; no I/O, no logging.
- `ms-docstore` — named logical documents (`DocRef`): per-document lock, atomic writes, JSON or
  SQLite. Must not depend on `ms-config`.
- `ms-download` — native-only resumable HTTP download engine (`RangeFetcher` / `UreqFetcher`,
  `RetryPolicy`, `ResumableFetch::fetch_part` into a `.part`, process-wide `BusyGuard`); GUI-free, no
  i18n; consumers own threads, URLs, verification, publishing and UI text.
- `ms-fonts` — the bundled `fonts/ui` stack, shared by egui UI fonts and the text renderer.
- `ms-memory` — image-memory profiles and eviction policy, host RAM total / available on every
  platform (`current_memory_availability`); owns no pixels.
- `ms-onnx` — onnxruntime (`ort`, load-dynamic) inference: MangaOCR, PaddleOCR.
- `ms-backend-ipc` — framed IPC client for the Python backend.
- `ms-platform` — GUI-free platform seams: the capability table
  (`PlatformFeature::is_available()`, typed `Unavailable`), the async-shaped file-dialog seam
  (rfd on desktop, Storage Access Framework on Android), clipboard reads, the pre-window
  error box, the Android credential store (`credentials`) and the Android package's native
  library directory (`native_libs`), opening URLs and folders in other programs (`shell_open`).
  The only crate naming `rfd` / `arboard`, and the one place JNI is used for platform services.
- `ms-browser-ext` — the ManhwaStudio browser extension as part of the app: its pinned Chromium
  id (from the manifest `key`), the bundle its build script builds from `browser-extension/`
  with npm (absent when node/npm are missing; `NotIncluded` says why) and the atomic, stamped
  unpack into `<data_dir>/browser_extension/chromium`. No `ms-*` dependencies; GUI-free.
- `ms-browser-bridge` — the feature-agnostic loopback WebSocket bridge to that extension:
  `bridge.json` pairing, mutual HMAC-SHA256 handshake, module capabilities, blocking
  request / cancel / events for worker threads, reload; `app` is the one place that builds the
  app's config (`ensure_started_for_app`), re-unpacks a stale folder and reloads stale peers.

**Core (levels 3-7)**
- `ms-config` — runtime root (`data_dir` / `program_dir`), `user_config` sections and defaults,
  `ConfigSaver`, storage-mode startup, `AppTab`, version formatting.
- `ms-text-render` — the production text renderer (cosmic-text shaping, vector-first glyph
  raster, effects); re-exported to the typing crate as `render_next`.
- `ms-page-ops` — journalled structural page operations; owns `Page` and `ProjectPaths`.
- `ms-sysprobe` — system probes, app-managed AI model catalogue, Hugging Face token, the one
  PCI display-device walk and AMD PCI device id -> gfx table (`amd_adapters`), the OS / CPU /
  GPU fact sheet behind the launcher's system-information view (`system_facts`);
  `python_manager` is the only way to find or spawn Python.
- `ms-onnx-runtime` — locates, downloads and loads the native onnxruntime library per build.
- `ms-window-geometry` — startup monitor and window geometry; the only direct `winit` user.
- `ms-os-integration` — the OS records of a program copy (Windows Uninstall / App Paths /
  "Open with" keys, `.lnk` shortcuts, elevation probe and UAC relaunch; Linux `.desktop` entry
  and icon): names, expected values, writes and removal, the read-only probe (`report`) and the
  action rules + elevated helper (`actions`). GUI-free. Consumers: the installer (per install
  kind), the binary (Linux startup entry, helper routing), `ms-settings-ui` (registration
  warnings) and `ms-launcher` (System registration tab).
- `ms-input` — the unified input system: buttons, code-declared actions (Command / Mode / Axis /
  Gesture) with any number of user alternatives, the registry, matcher, conflicts, binding labels,
  the `Hotkeys` codec, the per-frame router (`InputRouter` -> `InputFrame`) and the canvas touch
  router (`ms_input::touch`). Used by `ms-canvas`, `ms-tools`, `ms-studio-tab`, the tabs and the
  root; never by `ms-widgets`.
- `ms-project` — `ProjectData`, `ComicType`, `CanvasSettings`, project scan, save-merge.
- `ms-widgets` — project widget set (`panel_dock`, `Wheel*`, `AiButton`, spellchecked edits),
  `ui_fonts`, `ui_scale`.
- `ms-native-runtime` — process-global native ONNX manager (sessions, LRU, crash guard).
- `ms-local-ai` — GUI-free local-AI facts: `local_ai` (which local runtimes the process may use:
  `--no-ai` + platform, the route-availability rule, the effective runtime) and
  `backend_health` (push-driven backend health snapshot, probe thread, device control).
- `ms-region-postprocess` — the one owner of region-edit result post-processing for engines
  that answer RAW pixels (FLUX klein): persisted settings (defaults, ranges, whole-region rule),
  global and local CIELAB colour match, inward feather, composite over the original, the cached
  re-derivation `RegionPostprocessor`, the cv2-exact ellipse dilation (also the FLUX.2 latent mask
  built before a run) and the «Постобработка» section body UI. Core GUI-free, no I/O, no logging.
- `ms-studio-tab` — the `StudioTab` trait: order-free per-tab hooks (input declarations and
  contexts, dock default
  layout, GPU eviction, source-page window, background poll, exit settings, teardown store
  flush, committed-tree change) that every tab implements and the studio root iterates. Draw
  and ordered sequences stay explicit in the root.
- `ms-models` — shared cross-tab state (`BubblesModel`, `CleanOverlaysModel`, `TextMaskModel`,
  `layer_model::LayerDoc`, `page_view`, `AutosaveGate`, `clean_assign`). Holds egui data types
  but never takes `Ui` / `Painter`.
- `ms-tabs-simple` — Characters, Terms, Notes, Wiki tabs.
- `ms-installer` — install/update (release selection + `SHA256SUMS` verification in `release`),
  Python environment, `venv_check`, PyTorch flavours (`torch/`: CPU / CUDA from PyPI and
  pytorch.org, ROCm from AMD's whl-next index with a per-gfx device extra; the persisted choice
  `General.torch_flavour` / `torch_rocm_gfx`, vocabulary in `ms_config::torch_choice`).
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
  `neurolate/` is the GUI-free NeuroLate cloud OCR / text-detection client (typed form-field
  protocol, a paid POST that carries a fresh `Idempotency-Key`, is never retried and is abandoned — not aborted — on cancel, with
  a process-wide cap on unfinished paid requests; the free key/limit fetch; the key + limit
  panel). Crate-private `http/` is the one native transport, multipart builder and
  key-origin check shared by `image_edit` and `neurolate`.

**UI engines and tabs (levels 8-12)**
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
- `crates/winit`, `crates/egui` — Android-port forks of winit 0.30 and the egui family
  (separate git repositories, gitignored), wired in by root `[patch.crates-io]`; fork changes are
  Android-only, so desktop builds compile the released code.

## 3. Startup and process lifetime

- **One main window.** `src/startup/` (`run_native`, called by `lib.rs::native_main`) decides one
  `RunMode` right after parsing (`mode.rs`: service flags, test launcher, main run), builds the
  `ProcessServices` in the fixed startup order (`services.rs`), runs the startup prompt windows
  (installer, missing environment, `--update`) once, then ONE `eframe::run_native` — the main window
  (`src/shell/`, `AppShell`) — that hosts the launcher and the studio as SCREENS and stays open
  while switching between them; a launcher "Update" runs the update window after it closed. The
  shell owns the window (geometry, identity, title, minimum size, fonts, UI scale, GL lifetime,
  `egui::Options`, the per-screen global style, the root close); screens are state machines that
  return typed exits, and where the window goes next is one pure table (`shell/screen.rs`). No
  switch blocks the GUI thread: the launcher entry is prepared, the studio loaded and torn down
  on workers. Process-global services created at startup outlive the window: the
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
- The studio screen (`src/shell/studio/`) loads the project off the GUI thread, then delegates
  to `StudioApp`. It opens a typed `StartupTarget`: a project directory, or an ordered image set
  (`Images(ImageSources)`: CLI `--image` / positional paths, launcher multi-pick, OS "Open with",
  Android Share); the open worker turns it into a `StudioSession` (reserving the scratch), loads the
  `ProjectData` and pre-reads the `StudioBootstrap` (every disk input of `StudioApp::new`).
- **External opens while running** (Android Share / "Open with" via the `ms_platform::incoming`
  inbox; cold start reads it in `startup::run_native`): the main window's screen table decides — the
  launcher opens them at once, the studio asks (its unsaved-changes flow or a confirmation),
  Loading/Closing screens queue and merge them in arrival order.
- **Every `StudioApp` and every launcher screen lives inside the same `egui::Context`** (reload,
  launcher <-> studio). Any egui resource a crate registers (font families, textures, ids) must be
  named by content, never by an instance counter; fonts are installed once per process.
- **Android:** `src/android_entry.rs::android_main` (cdylib built by `build-android.sh`) chdirs
  into `<internal data>/ms_root` and runs the same `startup::run_native` — one `eframe::run_native` there too. The
  main window sets `General.secondary_windows` (`ms_config::secondary_windows`, the one owner of
  the separate-vs-embedded decision on every platform) on the winit fork before it, and re-syncs
  it at creation and on every screen switch (`ms_window_geometry::sync_secondary_window_policy`);
  the fork decides real per-Activity windows vs embedded egui windows.
- **GL lifetime:** the main window calls `egui_shader_layers::install_glow` in its creator and
  `destroy_glow` in its `on_exit`; no `ms-*` crate owns GL objects.

## 4. Project data model

- A project opened in the studio is one **chapter** inside a **title** directory.
  `ms_project::ProjectData::load` returns the chapter directories, `pages: Vec<Page>`, the
  bubble list, `ProjectPaths`, `comic_type`, `canvas_settings` and `settings_data`.
- **Image-set mode** (`SessionKind::SingleImage`, one or more images): `ProjectData::session()`
  is a typed `SessionKind` set by the caller, never inferred from paths. `ms_project::single_image`
  decodes the images in the given order (EXIF orientation applied; unreadable files are skipped
  and listed) into a throwaway scratch chapter under the OS temp dir, one page per image, and the
  regular load runs on that scratch — never on the user's folders. Writers stay live against the
  scratch; user files are written only by explicit Save / Save As (flattened composite per page
  via the typing flatten, atomic write): Save writes each CHANGED page back to its own source
  (unchanged pixels — `PixelDigest` of the opened image — are skipped), pages without a writable
  source and Save As of a set go to one chosen folder. The studio screen owns the scratch; the teardown worker
  deletes it after the app was dropped; stale scratches (marker + released lock) are swept at startup. UI branches
  (tabs, top bar, exit dialog) live in `src/studio/session/image_set/`.
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

- `StudioApp` (`src/studio.rs` + `src/studio/`) owns one opened session: `PageResidency`
  (page decode and GPU residency), `StagingWriters` (the three `_unsaved` writers, their
  `AutosaveGate`, the DISCARD latch), `SessionMode` (`Project(ProjectSession)` /
  `ImageSet(ImageSetSession)`, chosen once), `Tabs` (the tab states), `CloseFlow` (close intent)
  and the panel dock. Everything it asks of the window leaves through one `StudioOutput` queue
  (title, close, received-images decline, reload after a page op).
- `StudioApp` creates the chapter models, each `Arc<Mutex<_>>`, and injects them
  into tabs and canvases through typed setters: `BubblesModel`, `CleanOverlaysModel`,
  `TextMaskModel`, `LayerDoc`. Models carry revision (or per-node generation) counters; tabs pull
  changes by revision and never copy shared state by hand. Lock briefly, snapshot, release.
- `StudioApp` is a screen of the main window, not a window: it never closes, maximizes or styles
  it. A close it decides (exit dialog, launcher button, OS close via `request_os_close`) is a
  `PendingCloseAction` the shell takes; its teardown (`shutdown_blocking`, `src/studio/shutdown.rs`)
  runs on the shell's worker with the whole `Box<StudioApp>` moved there (`StudioApp: Send` is a
  compile-time guard).
- **Page loader pipeline** (`PageResidency`, `src/studio/residency/`): one background decode pool for source pages AND clean overlays (page
  order) -> strictly ordered promotion on the GUI thread -> tiled GPU upload under a per-frame
  budget. Page dimensions come from `PageImageInfo`; GPU textures are a droppable cache.
- **Texture lending:** `PageResidency` owns the resident `PageTexture` map; `StudioApp` lends it (`&mut`) to
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
- **Panel dock:** one `PanelDockState` per studio window, owned by `StudioApp` and lent to the
  active tab per frame; layouts are keyed by `AppTab::key()` and persisted through the
  `PanelLayout` config section.
- **Touch on canvases:** one router (`ms_input::touch`), run from the main window's
  `raw_input_hook` (gated by `PlatformFeature::CanvasTouchGestures`, off on Windows/wasm),
  claims a finger that lands on a published canvas surface and strips egui-winit's simulated
  mouse press for it, so a finger never reaches a tool. Canvases (`CanvasView`, PS editor)
  publish their surface + holes each frame and take a typed `TouchOutput` (pan/pinch, long-press
  menu point, long-press-then-drag selection). On the migrated canvases the selection reaches
  the active mode as a `Touch(LongPressDrag)` alternative of a canvas-wide gesture (claimed by
  the input router); the PS editor reads it from its `TouchOutput`. Mouse and pen (Android
  stylus is routed as a mouse by the winit fork) keep tool behaviour.
- **Input (bindings):** `ms-input`. Actions are declared in code (the shared `view.*` set, the
  brush set, each tab's `StudioTab::input_declarations`, the session's own) and merged into one
  `InputRegistry` with the user's overrides (`user_config` -> `Hotkeys`, format 2; any number of
  alternatives per action). `StudioApp` calls `InputRouter::begin_frame` ONCE per frame, before
  the tab draw, with the active tab's `StudioTab::input_contexts`; the session and the drawing tab
  then read the same `InputFrame` (fired commands, active modes, axes, gesture claims). Viewers
  publish their surfaces for the next frame. Hint text names a binding only through
  `InputRegistry::label` (alternatives joined by " / "); conflicts and cross-level shadowing come
  from the registry and are shown in Settings › Hotkeys, which edits the bindings through the
  router (capture).

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
- A canvas that is not drawn pulls the shared models only through
  `CanvasView::sync_shared_models_while_hidden`, which its tab calls before applying background
  results through that canvas (`StudioTab::poll_background`); the studio's per-frame flush of
  inactive canvases writes the resulting edits.
- Bubble text spellcheck follows the typesetting language, not the UI language. Bubble status
  borders: rules in `ms_config::bubble_status` (GUI-free), painting in `ms_widgets::bubble_status`.

Detail: `crates/ms-canvas/src/MODULE_README.md`.

## 7. Tabs

`AppTab` (`crates/ms-config/src/app_tab.rs`) fixes order and persistence ids. `StudioApp` owns
every tab state, lends it shared models, textures and the panel dock each frame, and executes
the actions tabs return. Canvas tabs implement `CanvasHooks` and own their canvas; the others
draw their own UI. Every tab answers the root's order-free questions (input declarations and
contexts, dock host, GPU
eviction, page window, background poll, exit settings, teardown store flush, committed-tree
change) through `ms_studio_tab::StudioTab`; drawing and every ordered sequence stay explicit.

- **Page Manager** — `ms-tab-page-manager` (+ engine `ms-page-ops`): page grid, clean cards,
  full-resolution viewer. Returns `PageManagerAction` (structural op, open page in a tab).
  **Structural ops (move, insert, blank, delete, stitch, split, crop) are not staged:** pages are
  position-keyed, so each op is a journalled crash-safe transaction that remaps every page-keyed
  artifact in both the committed and `_unsaved` trees. The app quiesces every chapter writer,
  runs the op on a worker, then the main window tears the app down and rebuilds it from disk.
  Save,
  export and page ops gate each other.
- **Translation** — `ms-tab-translation`: bubbles, OCR, text detection, machine translation
  (AI API OCR and MT go through `ms-ai-api`). Its host `TranslationTab` owns the Translation
  canvas, its hotkeys and its bottom hint.
  Talks to the backend only through `ms-backend-ipc`; reads the pushed backend health snapshot (`ms_local_ai::backend_health`).
  Google / DeepL / Yandex each have a transport (`user_config`, live in `mt_transport`): the
  built-in HTTP requests or the user's Chromium through the extension (`ms-browser-bridge`).
- **Cleaning** — `ms-tab-cleaning`: clean overlays, brush / region tools, two region-edit tools
  on ONE generic host (`tools/region_edit_v2`: `RegionEditHost` + `HostSpec`, engines behind
  `AiEngine`) — the local AI region editor (`tools/ai_editor`) and the cloud «ИИ редактирование
  (API)» (`tools/ai_api_editor`, no Torch, over `ms_ai_api::image_edit`) — and the watermark
  engine. FLUX klein engines answer RAW model output; the host finishes it through
  `ms-region-postprocess` and re-derives the pending result live when post settings change. Writes only through `CleanOverlaysModel`.
- **Typing** — `ms-tab-typing` (+ renderer `ms-text-render`): text layers in `LayerDoc`, masks,
  export (PNG, PSD, lossless PDF), custom fonts (compile service, the shared hybrid editor hosted
  as a dock panel here and as a dialog in Settings › Тайп).
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
- The `ms-fonts` manifest is process-global and project-independent, and it is the only UI
  font source (`ui_fonts`): a title's `fonts/` folder carries typesetting fonts only, so a
  project can change neither the UI fonts nor final render output.
- UI language (`General.ui_language`, `ms_i18n::LocaleTag`) and typesetting language
  (`TextTab.text_language`, `ms_text_util::language::TextLanguage`) are independent;
  hyphenation, segmentation and font coverage follow the TEXT language.
- **Custom fonts are recipes, never files.** A saved one lives in `fonts_data` (`custom_fonts`,
  kind-tagged; unknown kinds kept verbatim), an ad-hoc one in the layer's
  `text_params.font_recipe`. The blend algorithm and property weights are recipe facts; a
  recipe that differs from the plain weighted average carries kind `hybrid2`, which older
  builds cannot read (an unknown kind: unavailable, never compiled as the baseline). `ms-custom-fonts` compiles a recipe into an in-memory face; the
  typing `custom_fonts` service (memory LRU + disk cache `fonts/custom_cache/<key>.ttf`, a
  disposable artefact skipped by the font scanner) serves it to the typing `FontProvider`, so
  the renderer sees an ordinary face. A missing source font blocks the layer (`missing_font`),
  never a substitute.

Detail: `crates/ms-text-render/src/MODULE_README.md`, `crates/ms-tab-typing/src/MODULE_README.md`.

## 9. AI: native runtime and Python backend

- **Two runtimes.** In-process native ONNX (`ms-onnx` sessions, `ms-onnx-runtime` library
  resolver/downloader, `ms-native-runtime` manager) and the out-of-process Python backend.
  On Android the native runtime loads the onnxruntime library bundled in the APK (official
  `onnxruntime-android` AAR; `bundled` rows of `ort_manifest.json`, located through
  `ms_platform::native_libs`); builds `cpu` and `xnnpack` (default), nothing is downloaded.
  `ms_local_ai::local_ai::effective_ai_runtime` picks the effective one: native unless the
  user explicitly chose the backend (`General.ai_runtime_configured`) on a platform that has it.
  `--no-ai` turns off the LOCAL runtimes only (backend and native ONNX); cloud AI API OCR / MT and
  non-AI machine translation are never gated by it. Per-route availability has one owner,
  `ms_local_ai::local_ai::AiRouteKind::availability` (cloud / native / backend).
- **Backend boundary.** Framed, multiplexed IPC (`ms-backend-ipc`): AF_UNIX socket on unix,
  token-authenticated loopback WebSocket on Windows. Every Rust caller uses
  `ms_backend_ipc::shared_client()`. Method and topic names live in
  `crates/ms-backend-ipc/src/protocol.rs`. `PROTOCOL_VERSION` (mirrored in
  `modules/ai_backend/ipc/protocol.py`, a test guards the mirror) is the only compatibility
  gate, compared in the `hello` handshake; it also covers the on-disk `user_config` semantics.
  The `health` payload carries the typed torch build (flavour, versions, visible GPUs with their
  gfx, installed ROCm device kernels; backend owner `runtime/torch_flavour.py`), parsed into
  `ms_local_ai::backend_health`.
- **Process ownership.** One app-global supervisor (`ms_settings_ui::ai_backend_supervisor`)
  starts and stops the backend for studio and launcher; autostart waits for storage-mode
  conversions. `ms_local_ai::backend_health` keeps the pushed health snapshot that AI
  actions (`AiButton`) gate on.
- **Backend package.** `server.py` is the composition root; handlers reach services only through
  `AppState` fields (their names are contract). Dependency direction: domains (`ocr`,
  `detection`, `inpaint`, `reline`, `translate`, `watermark`, `browser`) -> `engines/` ->
  `runtime/`. Detail: `modules/ai_backend/MODULE_README.md`.
- **Backend memory: two peer pools, one arbiter.** Every model the backend holds is a lease of
  one `ResidencyArbiter` (`runtime/residency/`, the backend's memory-budget ledger; usable
  host/device memory is read only by `runtime/memory_budget.py`): the small pool
  (`SmallModelManager`, byte-aware, `General.ai_max_loaded_models` a secondary count cap) and the
  large pool (`LargeModelManager`, `runtime/large_models/`) reclaim idle holdings from each other.
  The large-model manager alone decides placement, forecast (per-device-class calibration tables)
  and degrade for large models; their services declare specs and phases and never move a weight.
  The user steers it on two axes, «Видеопамять» (max / phase / half / min) and «Оперативная
  память» (keep / run / min), each with auto, under hard byte caps (`large_models.*`,
  `PROTOCOL.md` §5.10). FLUX.2 klein and FLUX.3 klein (`inpaint.flux3_klein`, region editing with
  the region as reference #0) run on it. A model service may pin components to host execution
  per request (`LargeModelSpec.host_pin`; «Энкодер на CPU», `encoder_on_cpu`, default on for both
  klein models): a pinned component runs on the CPU at every VRAM step and only the RAM step
  places it; the pin is part of the residency and learned-floor keys. Detail:
  `modules/ai_backend/runtime/large_models/MODULE_README.md`.
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
  `ms_sysprobe::ai_models` (engine: `ms-download`) before a backend feature initializes them; every
  download is explicit (never implicit on use), resumable and cancellable. The main model repo is
  unpinned (size + server identity, LFS sha256 when announced). Third-party models — Baberu OCR,
  the PaddleOCR-VL variants, EasyOCR (one shared dir, detector stored once), Surya, the SDXL configs
  and the one curated SDXL inpaint repo — come from the pinned external catalog
  (`ai_models::external_catalog`: source, commit/URL, per-file size + sha256; regenerated with
  `tools/model_pins/`), the one owner of those facts. Rust sends the backend absolute paths and the
  backend loads them offline only (`local_files_only`, no library downloads); EasyOCR / Surya
  library versions are pinned to match the catalog. The backend itself still downloads only FLUX,
  watermark and Reline weights, through `engines/model_download.py` (same network rules).
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
- **Credentials** — secrets live only in the platform credential store, never in a config file
  (desktop: the OS keyring; Android: the Keystore-backed `ms_platform::credentials` store over the
  Kotlin `CredentialBridge`, where an undecryptable value is an error, never "no key"): AI API keys
  (`ms_ai_api::keys`; per service, per service + base URL for compatible endpoints, and frozen
  `image_edit:{provider}[@{region}]` names for image-edit providers, frozen `neurolate:api` for
  the NeuroLate key) and the Hugging Face token (`ms_sysprobe::hf_token`, a cached
  process-wide value seeded off-thread). Keyring I/O never runs on the GUI thread; a token is
  never logged and reaches the backend only as a per-request field.
- **Browser bridge** (`ms-browser-bridge`, native) — one process-wide `BridgeService`, started
  on first use through `app::ensure_started_for_app`: a loopback (`127.0.0.1`) WebSocket server
  that the user's MV3 extension (`browser-extension/`, unpacked from `ms-browser-ext`) connects
  to. Pairing is the `bridge.json` file the app writes into the unpacked folder (port + secret);
  both sides authenticate with HMAC-SHA256 over that secret, and the `Origin` is pinned to the
  extension id. Compatibility is `PROTOCOL_VERSION` only, owned by `ms_browser_bridge::protocol`
  and mirrored in `browser-extension/src/core/protocol.ts` (a test guards the mirror); the
  bundle `build_id` only keeps the folder current and triggers reloads. Feature-agnostic:
  consumers call modules by name (machine translation's `translate` is the first). The unpacked
  folder is shared by every app process on the machine: the bridge owner holds an inter-process
  lock for as long as it listens (a second process starts no server and reports
  `OwnedByOtherProcess`), and every unpack holds a separate install lock.
- **Threads** — spawned through `ms_thread` on any path that also runs on wasm; `rayon` for CPU
  work; `tokio` only in `ms-ai-api`.
- **Versions** — the program version is the commit count of the build (`MS_APP_VERSION`,
  `git rev-list --count HEAD` in `build.rs`; full `CARGO_PKG_VERSION` when git is unavailable or
  the clone is shallow). It is the one value shown, compared and sent across processes; libraries
  receive it as `ms_installer::HostVersion`. A release is tag `N` + `Cargo.toml` `N.0.0`
  (`CARGO_PKG_VERSION` is packaging metadata only: Windows FILEVERSION, Android versionCode, macOS
  plist). Other copies' `--version` output is reduced with `version_format::version_core` (old
  builds print `3.6.0+hash-n`). Release selection and its `SHA256SUMS` check have one owner,
  `ms_installer::release`.

## 11. Background pipelines

Rule: any heavy operation runs on a worker with an explicit lifecycle (start, poll, cancel,
logged error); the GUI thread only polls and applies results.

| Pipeline | Owner |
|---|---|
| Page + clean-overlay decode pool, ordered promotion, GPU upload | `src/studio/residency/` (`PageResidency`) |
| Launcher entry preparation (config reads, scans, probes) | `src/shell/mod.rs` (`ms_launcher::prepare_entry` on a worker) |
| Studio project load + `StudioBootstrap` pre-read behind the loading screen | `src/shell/studio/open.rs` |
| Save-to-project merge, page-op quiesce + dispatch, DISCARD delete job | `src/studio/session/project/` (writer steps: `src/studio/staging_writers.rs`) |
| Image-set save (flatten, file dialog, commit, JPEG quality persistence) | `src/studio/session/image_set/` |
| Studio teardown behind the Closing screen (`StudioApp::shutdown_blocking`, scratch release) | `src/shell/closing.rs` |
| Startup update check (once per process) | `src/update_check.rs`, started by `src/shell/` |
| Bubble and layer persistence (coalescing savers) | `ms-models` (`bubbles_model.rs`, `layer_model/saver.rs`) |
| Clean overlay autosave, canvas settings saver, overlay tile prepare | `ms-canvas/src/workers.rs` |
| Config sections `Window` / `PanelLayout` | `ms-config::config_saver` |
| OCR, text detection, MT, crop recognition | `ms-tab-translation` |
| Backend health snapshot (pushed by the supervisor's probe) | `ms-local-ai` (`backend_health`), probe thread in `ms-settings-ui/src/ai_backend_supervisor.rs` |
| Text detection plan / tiles / stitch / postprocess (one plan for worker and panel notice) | `ms-text-detect`, driven by `ms-tab-translation/src/text_detector/` |
| Native ONNX load and inference | `ms-native-runtime` (called from those workers) |
| AI backend process supervisor | `ms-settings-ui/src/ai_backend_supervisor.rs` |
| Large-model run (admission, per-phase loads and block moves with prefetch, degrade retries) | `modules/ai_backend/runtime/large_models/` (`Session`), driven by the FLUX klein services |
| Storage-mode conversion job | `ms-settings-ui/src/storage_mode_job.rs` |
| Structural page ops, thumbnails, viewer clean decode | `ms-tab-page-manager` + `ms-page-ops` |
| Typing live render (latest-wins), save, export | `ms-tab-typing/src/tab/` |
| Custom font compile (one job per recipe key, memory LRU, disk cache, prune) and the editor's latest-wins subset preview | `ms-tab-typing/src/custom_fonts/` + `hybrid_editor/`, compiler `ms-custom-fonts` |
| PS editor page decode, raster effects | `ms-tab-ps-editor` |
| Cleaning region loader, inpaint runs, quick text clean | `ms-tab-cleaning` |
| Region-edit post-process re-derivation (latest-wins, one worker per pending raw result) | `ms-tab-cleaning/src/tools/region_edit_v2/postprocess.rs` + `ms-region-postprocess` |
| Cloud image-edit run (submit, poll, download, exact-size finish) | `ms-tab-cleaning/src/tools/ai_api_editor` + `ms_ai_api::image_edit` |
| Characters / Notes / Wiki background work | `ms-tabs-simple` |
| Batch processing of new projects | `ms-launcher/src/new_project/batch_processing/` |
| System-registration probe / actions / elevated helper (`ShellExecuteExW` round trip + result file) | `ms-os-integration` (`report`, `actions`, `windows/elevation.rs`), driven by `ms-launcher/src/pages/system_registration.rs` and `settings_warnings` |
| Settings warnings checks (full run per launcher entry, per-unit rechecks) | `ms-settings-ui/src/settings_warnings/`, driven by `ms-launcher/src/pages/settings_page.rs` |
| Browser bridge: start (pairing file, bind, stale-folder re-unpack), listener + one thread per extension connection, stale-peer reload watcher | `ms-browser-bridge` (`app.rs`, `server/`) |
| Browser extension install job (unpack, start, open the folder, reload stale peers), folder probe, MT transport save | `ms-tab-translation/src/tab/browser_extension.rs` (+ `ms_browser_bridge::app::install_guarded`) |

## 12. Key invariants

**Threading**
- The GUI thread never blocks: file and network I/O, image decode, saver barriers and worker
  joins run on workers — including every screen switch of the main window (launcher entry
  preparation, studio load, studio teardown). Backend failures never hang the GUI; AI actions gate on the pushed
  health snapshot (`AiButton`); long runs do a one-shot `check_ai_backend_health` first.
- No model lock is held across I/O, rendering, callbacks or a worker wait.

**Shared state**
- Cross-tab state lives only in the `Arc<Mutex<_>>` models `StudioApp` creates and shares with the tabs.
- `CanvasView` is the one canvas engine; tab behaviour plugs in via `CanvasHooks`.
- One owner per rule: the page <-> clean binding (`clean_binding` + `clean_assign`), Python
  lookup and spawn (`ms_sysprobe::python_manager`), semantic colours (`ms-theme`), `winit`
  (`ms-window-geometry`), OS integration records (`ms-os-integration`), glyph rendering fonts
  (`ms-fonts` + `ms-text-render`), layer composite order / visibility / group fold
  (`ms_models::layer_model::ordering`, consumed by the typing canvas, the typing
  flatten/export and the PS composite, layers tree and structural order), platform
  capability decisions (`ms_platform::PlatformFeature`), file dialogs
  (`ms_platform::file_dialog`), system clipboard reads (`ms_platform::clipboard`), input
  bindings, their labels and conflicts (`ms-input`), finger gestures on canvases
  (`ms_input::touch`), custom-font synthesis and compiled-face naming (`ms-custom-fonts`) and the
  custom-font identity namespace (`ms_tab_typing` `custom_fonts::identity`), the browser-bridge
  wire protocol (`ms_browser_bridge::protocol` + `browser-extension/src/core/protocol.ts`), the
  app's bridge config and the extension unpack (`ms_browser_bridge::app`; one serialized
  `install_guarded`), the extension id, bundle and folder (`ms-browser-ext`), opening URLs and
  folders in other programs (`ms_platform::shell_open`).
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
  failed pages (see `src/studio/MODULE_README.md`, save/page-op choreography).
- DISCARD never flushes: it drops pending state in all three writers before deleting staging
  (a docstore write would re-create the directory) and latches the discard first (`StagingWriters`); a
  failed cleanup releases the latch and restarts all three writers.
- Exit: deferred typing edits are enqueued inline BEFORE the exit barrier; failed pages are
  reported.
- An image-set session never merges staging: every close takes the DISCARD quiesce (no
  flush, no delete job); the main window's teardown worker removes the scratch afterwards. Its dirty state is
  `AutosaveGate::action_count()` against the baseline of the last successful file write.

Detail: `src/studio/MODULE_README.md`, `crates/ms-models/src/MODULE_README.md`,
`crates/ms-models/src/layer_model/MODULE_README.md`, `crates/ms-tab-typing/src/MODULE_README.md`.

**AI backend memory**
- One owner of the backend memory budget: usable memory `runtime/memory_budget.py`, ledger and
  reclaim `runtime/residency/`; no pool or service plans against a probe of its own.
- Never a host OOM: a large-model plan must fit the host budget before admission and every load
  or move is guarded live (`HostWatch`); device plans never exceed `total - reserve`. On Linux
  the backend sets its own `oom_score_adj` to 1000, so the kernel's last resort hits the backend,
  not the app.
- Single copy of weights: a weight block lives in exactly one place (device, host or file) apart
  from the blocks in flight; nothing keeps a RAM copy of what sits in VRAM.

**Versions and protocol**
- Program versions are compared only via `MS_APP_VERSION` (commit count); the asset names,
  `--continue-update` and the one-token `--version` are frozen by the installed 3.6.0 updater.
- Rust <-> Python compatibility is decided solely by `PROTOCOL_VERSION` in `hello`; any contract
  change bumps it on both sides (rule: `PROJECT_RULES.md`).
- App <-> browser-extension compatibility is decided solely by the bridge's own
  `PROTOCOL_VERSION` (independent of the backend's); the handshake messages are frozen forever.
