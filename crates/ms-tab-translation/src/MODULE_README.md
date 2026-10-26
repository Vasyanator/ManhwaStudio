# Module: crates/ms-tab-translation/src

## Purpose
Translation tab runtime for OCR, text detection, machine translation, translation-focused bubble
editing, composed text export, and translation-specific canvas hooks.

This directory is the crate root of `ms-tab-translation`, re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::translation`, so every existing
`crate::tabs::translation::…` path stays valid. Layer: near the top of the library stack —
above `ms-canvas` / `ms-models` / `ms-widgets` / `ms-tools`, `ms-local-ai` (backend health,
local-AI availability) and `ms-tabs-simple` (character and term note entries feed the MT
glossary), below `studio.rs` and the `cleaning` tab. It must never name `studio`, `launcher`,
`ms-settings-ui` or a tab above it. Items reachable from outside the crate
(`machine_translation::translate_texts_via_translator`, `text_detector`'s
mask helpers, `panels::machine_translation::{MtLanguage, MT_SOURCE_LANGUAGES}`) are `pub`
for that reason and carry a note saying so.

## Architecture
`TranslationTab` in `host.rs` is the program tab as the studio root holds it: it owns the tab's
`CanvasView` and its `TranslationTabState` (so the split borrow `(&mut canvas, &mut state)` the
canvas draw needs stays inside this crate), draws them (`draw`: this frame's input handed to the
state, the tools' on-set restored from the dock (first frame), fired commands, bottom-hint rows,
lazy settings load, the tools' per-frame preparation, per-frame canvas flags, then the canvas
inside the root's `CentralPanel`), and implements `ms_studio_tab::StudioTab` —
the tab's input declarations and contexts, the dock builder, GPU eviction (canvas clean overlay, then detector masks), the source-page window, NEAREST
sampling, the background drain while hidden (`poll_background`) and the exit-only
`Canvas.translation_hint_collapsed` key.

`TranslationTabState` in `tab.rs` (behaviour in `tab/`, see `tab/MODULE_README.md`) is the tab orchestrator and `CanvasHooks` implementation. It owns
panel state, controller instances, footer metadata caches, text-detection storage, OCR selection
state, and debounced settings persistence.

Long work is delegated to focused controllers:

- `ocr.rs` owns the OCR worker, backend IPC transport (framed `shared_client()`), AI API OCR
  transport, page crop/cache handling, and per-engine load state. The AI API layer itself
  (services, credential-store keys, client, async bridge, model metadata, the connection widget
  and its request runner) lives in `ms-ai-api`.
  `OcrEngine` has eight engines: MangaOCR, EasyOCR, PaddleOCR, PaddleOCR-VL, Surya and Baberu OCR
  (backend IPC methods `ocr.*`; MangaOCR/PaddleOCR/Baberu also native ONNX), AI API (`genai`,
  no backend) and NeuroLate (cloud HTTP, paid per block; worker step in `ocr/neurolate.rs`, see
  `ocr/MODULE_README.md`).
- `ocr_model_download.rs` owns the install status and the explicit, user-started downloads of the
  EXTERNAL model bundles (Baberu OCR, the three PaddleOCR-VL variants, the EasyOCR model of each
  script, the Surya detection + recognition checkpoints; pinned specs in
  `ms_sysprobe::ai_models::{external_catalog, easyocr}`). The tab polls it per frame; the OCR and
  text-detector panels draw its state through `panels/ocr_model_download.rs`. It is `pub`: the
  Cleaning tab reuses controller and block for its SDXL configs.
  It also ADOPTS, automatically and once per process per spec, a spec whose files are already on
  disk with no marker at all (hash + marker, no network, nothing deleted); a marker of another
  identity is an older install and goes through the regular download.
- `text_detector/` owns the text detector worker and returns page boxes plus editable binary
  masks. Detector modes (`TextDetectorRunMode`): `Classic` (local Otsu threshold + dilation +
  connected components), `PaddleOcr` (native ONNX or backend `textdetector.paddle.forward`),
  `AiCtd` (backend `textdetector.ctd.forward`) and `Surya` (backend `textdetector.surya.forward`).
  The model modes run `ms_text_detect`'s plan / tile / stitch / postprocess pipeline in Rust; the
  runners here only run the forward pass (in-process or over IPC). The inline
  `detect_*_mask_for_image` helpers are the same pipeline reused by Cleaning's mask generation.
  The detector panel announces the current page's plan (resize, tile count) by planning with the
  same `TextDetectorRunMode::plan_inputs` the worker uses; the page sizes the root lends
  (`page_infos`, `TranslationDrawParams`) reach it as the size the host caches each frame
  (`prepare_tool_frame` -> `current_page_size`).
  Dilation and Otsu come from `ms-raster` (see `text_detector/MODULE_README.md`).
  PaddleOCR, Surya and NeuroLate, which are also detectors, each have a "create mask and text
  blocks" parameter (`OcrPanelOptions::{paddle,surya}_detect_mask`, per title in
  `OCR.params.{paddle,surya}.detect_mask`; NeuroLate's in `NeurolateSettings::detect_mask`,
  `OCR.params.neurolate.detect_mask`; all carried in `OcrRuntimeOptions`). With it on, a quick
  OCR selection (once its OCR is dispatched) and the advanced window's Create bubble (on its
  ORIGINAL page selection, never the edited preview) also run that engine's detector on the same
  crop (`text_detector::RegionDetectEngine::selection_detects`, the one trigger rule; block OCR
  never does) and replace the detector mask and blocks inside the selection in both stores
  (`tab/detector_region.rs`). A quick NeuroLate selection runs no detector: its OCR request
  carries an `OcrRegionWant` and the SAME paid `MASK_AND_TEXT` answer yields the text and the
  region, which enters the same wait / merge path (`receive_ocr_region`); while the detector is
  busy the OCR reads text only (`TEXT`) and the usual busy refusal is shown. The advanced
  window's Create bubble with NeuroLate sends a separate `MASK` request on the original
  selection through the detector's region command. A region dispatch starts the chapter's stored-detection load when
  it has not run, and an answer arriving while that load (or a save) runs, or before a load has
  succeeded (a dead load is retried by the next dispatch or panel draw), waits and is applied
  after it, in order, so a region never stands in for a stored page; Clear and a replacing page
  batch drop the waiting answers. The detector's Save button
  writes the masks the user sees — the shared `TextMaskModel`'s, brush edits and region merges
  included — with the tab's blocks, plus pages that only have a hand-painted mask
  (`detector_region::detection_pages_for_save`; the button is enabled for either). A legacy page
  (stored blocks, no stored mask) the user never edited is written exactly as before, without a
  mask; a region merge onto it first freezes its shown block raster into the result, and the
  shared model then gets the resolved (expanded) boxes that raster was built from outside the
  selection (`text_detector::merge_region_blocks`, the one block rule), while the tab keeps the
  raw lines. A page whose stored files exist but could not be read at load
  (`TranslationTabState::load_failed_detection_pages`, logged per page) is never deleted nor
  rewritten by Save — what the session holds for it may be only a merged region — and the status
  line says how many were kept; a full page detection of it (`PageDetected`) lifts that. Line edits
  (move, resize, add, delete) reach the shared model's boxes once their drag ends
  (`canvas_input::flush_line_edits_to_text_mask_model`: a raw-mask page keeps its mask and takes
  the raw lines, a legacy page's display raster is rebuilt, a hand-edited mask keeps unknown
  boxes); a batch result for a page drops a line selection or drag on it. A new detector entry
  (a line on an undetected page) takes its size from the page loader's `PageImageInfo`
  (`known_page_sizes`, refreshed by the host), never from disk. The mask is never autosaved. The detector's mask and blocks are drawn on the canvas while the Detector
  panel is open OR while the selected OCR engine's "create mask" parameter is on
  (`detector_region::detector_overlay_visible_for`, the one owner; the same draw path either way,
  and the stored detection's lazy load is started for it). Display only: the mask brush and the
  lines editor stay tied to the panel.
- `machine_translation.rs` owns MT run threads, AI API MT chat batching/context pruning,
  cancellation, and stale-event filtering. A plain (Google / Yandex / DeepL) run builds ONE
  provider backend (`machine_translators::run_session` for the service and its live transport,
  `mt_transport::current`: built-in HTTP or the user's browser extension) and ONE limiter call
  (`mt_limits::call::MtCall::for_run`: the run's cancel flag + a wait reporter) per run, hands the
  provider `preferred_batch_size` consecutive bubbles per call and still emits one result and one
  progress event per bubble in order. Limiter waits reach the panel as
  `MtControllerEvent::RateLimitWait` (`MtPanelProgress::rate_limit_wait`, one status line); a
  service throttled for longer than a run waits stops the run with `RunRateLimited` (Warning
  toast), never `RunFailed`, so it is not mistaken for an AI credit/quota error. A stop-kind
  item (`Cancelled` / `CoolingDown` / `ServiceUnavailable`) ends the run only after the rest of
  its batch is applied; `ServiceUnavailable` (browser extension not connected / outdated /
  disconnected) ends it with `RunFailed` and that message. A Stop seen right after a provider
  call returned ends the run with `RunCancelled` WITHOUT applying that call's results
  (`PlainMtRun::run`); with the browser transport's batches of up to 20 texts that drops the
  translations already received in the call. A browser tab needing the user
  reaches the panel as a `RateLimitWait` whose `reason` is `MtRateLimitWaitReason::
  BrowserAttention` (the panel's "solve the check in the browser tab" line; Stop still works).
  The transport is chosen per service in the MT panel: the click sets `mt_transport` at once on
  the GUI thread and `BrowserExtensionController::save_transport` queues it for the controller's
  ONE saver worker (serialized, latest-wins per service, so the persisted value is the last
  click; a failure is a toast); a click to the browser transport also starts the bridge on a
  worker. The MT panel skips the transport row for the one frame right after a service switch
  (the tab built it for the previous service). Ordinary spacing waits are not shown (see `mt_limits/MODULE_README.md`).
- Live AI API generation progress: every AI API OCR / MT request runs through
  `ms_ai_api::exec_chat_validated` (the tracked executor plus ONE format-repair retry, whose
  reason the generation widget shows). The OCR controller owns one `GenerationTracker` shared with its
  worker; each MT run owns a fresh one inside its cancel switch (`MtRunCancel`). Both controllers
  expose `generation_snapshot()`; the tab state only lends it to the panels, which draw
  `ms_ai_api::draw_generation_status` outside their collapsible sections. OCR "Stop" carries the
  clicked snapshot -> `TranslationOcrController::cancel_generation(run_id)` (a stale click never
  stops a later queued request), plus `abort_textdetector_ocr` with a neutral status when the
  snapshot's `request_tag` (= OCR request id) is the detector batch's active request; the stopped request comes back as
  `OcrControllerEvent::RecognizeCancelled` (Info toast, no error, no detector retry). MT "Stop" is
  the cancel button's action: `request_cancel` sets the run flag and closes its tracker, which
  aborts the HTTP request in flight; the run ends through the normal `RunCancelled` path.
  Dropping the OCR controller closes its tracker before joining the worker: the AI API request
  in flight is aborted and queued AI API requests return before their keyring read; queued
  non-AI requests still run before the worker stops (pre-existing).
- The AI API connection blocks of the OCR and MT panels (key save/delete, metadata refresh) do
  not go through either controller: the tab state owns one `ms_ai_api::AiApiTaskRunner` per panel
  (`ocr_ai_api_tasks`, `mt_ai_api_tasks`), submits the panel's `AiApiConnectionActions` to it and
  applies its events in `poll_ai_api_events` (a model replaced by a refresh marks that panel's
  settings dirty; notices become toasts).
- The shared Python backend probe snapshot (`AiBackendHealthSnapshot`) and the local-AI
  availability rule come from `ms-local-ai` (`ms_local_ai::{backend_health, local_ai}`); the tab
  only reads them.

UI panels under `panels/` expose typed options/actions and avoid owning worker lifecycles. The tab
converts panel actions into controller requests, canvas changes, persistent settings, or storage
jobs.

This tab also HOSTS the shared panel dock (`crates/ms-widgets/src/panel_dock`). It declares eight tabs — the
canvas' own «Лента» (`canvas::CANVAS_RIBBON_TAB`, body `CanvasView::draw_ribbon_tab_body`, declared
through `canvas::declare_ribbon_tab`, the canvas' one declaration of it), its own «Последние
персонажи» (`TRANSLATION_RECENT_CHARACTERS_TAB`, body `draw_recent_characters_tab_body`), the
«Инструменты» toolbar (`TRANSLATION_TOOLS_TAB`) and one tab per tool
(`TranslationTool::tab_id`: Bubbles, OCR, Composition, MT, Text detector) — through the per-frame
context `TranslationDockCx`, which carries the whole `&mut TranslationTabState` (the tool bodies act
on it), the canvas and the project; it is built only after every tab is declared, so declarations
borrow nothing of the state. Its default arrangement is its OWN `translation_default_dock_layout`:
the ribbon panel at the dock area's left edge, the cards docked to its right, top-aligned (where the
row of cards floated before the migration), the toolbar flush with the bottom edge at its right end
(where the floating tools bar sat) and ONE panel of the five tool tabs docked on top of the toolbar,
right-aligned — one chain, so the solver shrinks them together and never lays the tools over the
bar. Sharing that default panel is also what makes the dock seed the five tools as one tabbed panel
into a stored layout that predates them. The same `fn` item is handed to the dock both through `StudioTab::default_dock_layout` (which the
studio's `restore_panel_dock` restores against) and by `ensure_default_layout`, since the dock state keeps it to rebuild the arrangement for the header's
reset item.

«Последние персонажи» is declared on EVERY frame with `visible(!recent_characters.is_empty())` and
`transparent_until_hover(true)`. Visibility, not declaration, is what an empty history switches: a
hidden tab keeps its slot in the layout, so the cards return to wherever the user put them, while a
skipped declaration would make the dock treat the tab as another program tab's and seed it a fresh
panel. Transparency is what keeps the cards looking exactly as they did as a bare `egui::Area` —
the panel paints no frame, border, arrow or grip until the pointer is over it, and geometry is
identical in both states. The cards are display-only in both states too: a character is activated by
its recent-character binding (shown on the card) together with the bubble-create command or the
quick OCR gesture, never by a click.

**Tools.** Each tool is switched on and off independently by its latching toolbar button
(`egui::Button::selected`, caption plus the binding text from the input registry) or its hotkey
(P S K M D); several may be on at once. Whether a tool is on is a STATE flag (`tools:
ToolToggles`), never read back from the dock: the detector overlay is decided inside the canvas draw
(`detector_overlay_visible`), before the dock runs. Each tool tab is declared EVERY frame with
`.visible(on)`, so switching it off keeps its panel, position, detached window and size. Switching
a tool ON queues a focus request that `apply_tool_dock_requests` applies before the next
`PanelDock::begin` (`PanelDockState::focus_tab`: front tab of its panel, panel expanded; a tab not
yet seeded keeps its request for the next frame) — only on that switch, never on later frames,
and never for a tool the user docked into the toolbar's own panel (that would bury the toolbar).
Switching the detector OFF turns its lines / mask edit modes off; switching Composition on requests
a rebuild. The on-set PERSISTS as the toolbar tab's `TabExtras` flags (`tool.<name>`, default off):
read once per state by the host before the first hotkey (`restore_tool_toggles`, the PS editor's
read-before-declare pattern) and written host-side before `begin` on change
(`PanelDockState::set_tab_extras_flag`), so it persists even while the toolbar is collapsed. The
toolbar's `initial_size` width is measured from its captions every frame (`tools_tab_sizes`): a dock
panel never re-measures its own width, so a bar started narrower would stay wrapped. Per-frame tool
work that used to assume one visible tool runs outside the bodies: the host's `prepare_tool_frame`
caches the current page size for the detector body (the hook gets no `page_infos`), refreshes the
backend health snapshot every frame (the canvas quick-OCR gesture gates on it with every tool off;
cloned only on a revision change) and, while any tool is on, the OCR route inputs; the hook polls and draws
the MT request preview every frame, rebuilds Composition while it is on and a rebuild is pending,
and loads the stored detection while the detector is on. The OCR body asks for a frame every
350 ms only while it is actually drawn (its status lines change without an egui event), so an OCR
tool left on behind a sibling, collapsed or detached-and-hidden costs no idle frames.
Only the front tab of a panel runs its body.

A card is PAINTED, not built from an `egui::Frame`, and that is a contract of the body rather than a
style choice. `Frame` reserves its space through `Ui::allocate_rect`, which advances the cursor past
an explicit rect and never reaches `Placer::next_space` — the only place `Layout::next_frame` starts
a new row — so a framed card cannot wrap and the whole history runs off the right edge of the panel
into a horizontal scrollbar. `draw_recent_characters_tab_body` therefore lays the caption out first
(unwrapped, so a name never breaks across two lines), derives the card size from that galley
(`recent_character_card_size`), allocates that exact size through `Ui::allocate_exact_size`, and
paints the background, the border and the text into the rect the layout returns
(`recent_character_card_caption_pos`). The two size helpers are each other's inverse and are tested
as such: they are what keeps the hand-painted card pixel-identical to the framed one it replaced.

The dock state is app-owned (`StudioApp::panel_dock`, one per studio
window) and reaches this tab through `CanvasDrawParams::panel_dock`, which the canvas passes into
`draw_canvas_overlay_top_left`; the tab owns none of it. `draw_panel_dock` runs at the very END of
that hook, and what that placement buys is ONE thing: running inside `CanvasView::draw` keeps a
«Лента» edit ahead of `publish_canvas_settings`. It buys no z-order — within one `Order` egui's
layer list is persistent and re-sorted stably each pass, so creation order inside the frame decides
nothing (`egui-docs/06-overlays.md` §1.1). The canvas-wide selections need no area of this tab:
the canvas raises its own capture area on `Order::Background` while a selection is armed, below the
dock's panels, so a held selection mode no longer takes a panel's clicks. The panel's rect is stored in `ribbon_panel_rect` for the NEXT
frame, because the two text-detector edit boxes that hang under it are drawn before the dock runs;
it is `None` while the tab is hidden or detached, since `PanelDockOutput::tab_rect` answers for the
main window alone.

## Input
The tab reads no key, modifier, wheel or raw pointer button itself: the studio's
`InputRouter::begin_frame` decides everything before any tab draws, and this crate reads the
resulting `InputFrame` (`TranslationDrawParams::input`). `input.rs` is the one owner of what does
what (ids are persistence keys of the user's overrides, never renamed):
- **Commands** (scope `Tab(Translation)`): bubble delete (Del), create (T, accepts the
  recent-character parameter at the press), create hint (H), create image bubble (Q), copy / paste
  original / translation (unbound); all under `NotContext` of the two detector edit modes and the
  advanced-recognition window — the one place that keeps bubble commands out of those states.
  Panel toggles P S K M D (exact modifiers: Shift+S is not S). Executed in `host/hotkeys.rs`.
- **Modes**: quick OCR (Shift), advanced OCR (Alt, beats quick), image crop (Shift+Q, beats both),
  the recent-character parameter modes `translation.recent_char.1..6` (digits 1-6), and in the
  `Sub` scope the brush erase mode (Shift, under `brush.active`, beats quick OCR: Shift in a brush
  erases and never arms OCR).
- **Canvas-wide gestures** on `ms_input::view::SURFACE`: quick OCR rect (Shift + LMB drag, or a
  finger long-press drag with no key), advanced rect, crop rect (their modes + LMB drag or + finger
  drag); they sample the recent-character parameter during the gesture. The router claims them at
  the press; the canvas raises its capture area while one is armed; `tab/canvas_input.rs` drives the
  claim (`advance_selection`: starts on a page, commits on the button or mode release, a cancel drops
  it, rects under 4x4 points are dropped; an OCR start on an unavailable route is refused with a
  toast). Quick OCR is off in the mask edit and with the advanced window open, advanced OCR with the
  window open. Which OCR a finger makes is a binding (rebind the touch alternative), not an OCR
  panel setting; a title's stored `OCR.finger_selection` is ignored and left untouched.
- **Lines edit** (`Sub`, context `translation.detector.lines_edit`): commands Delete / N, gestures
  select / deselect (LMB click on a line / outside every line), move (LMB drag on a line), resize
  (LMB drag on a handle of the selected line); targets handle 50 > line 40 > lines page 5, hits
  taken at the PRESS point (`line_edit_hits`).
- **Mask edit** (`Sub`, context `translation.detector.mask_edit`): paint (LMB drag), erase (RMB drag
  or erase mode + LMB drag) on the mask page target; the brush size is the shared `brush.*` set
  (`MaskBrush::apply_input`, `BRUSH_ACTIVE` published while a brush is on).
- **Advanced-recognition window** (`Window`, context `translation.adv_rec.open`, brush context
  `translation.adv_rec.brush`): select rect (brush off), paint / erase (brush on) on the preview
  target. The window publishes its own view surface (`adv_rec_view_id`, the window rect, its layer
  as capture layer) and zooms by the shared `view.*` set when the router's focus is that view, so
  its zoom keys need the zoom modifier and stay quiet while a text field has focus.
- **Bubble card menu** (`translation.bubble_card.menu`, RMB click): the card's own `Ui` senses
  clicks behind its widgets; the menu opens on the gesture through `ms_input::touch::context_menu_popup`.
- **Contexts** (`TranslationTab::input_contexts`): the canvas's (`EDITABLE`), then the tab's
  (`crate::input::contexts`). The canvas's zoom is never blocked by this tab; the canvas locks it
  itself while an OCR / crop rectangle claim runs (the page must not move under the rectangle).
- **Labels**: every hint (bottom-hint rows, panel titles and open buttons, OCR panel selection
  hints, mask edit hints, the advanced window's controls hint, the recent-character cards) is
  built from `TranslationInputLabels::from_registry`, rebuilt every frame by the host.
- **Hooks see the frame** through `TranslationTabState::input_frame`, a copy the host sets before
  its draw (`begin_input_frame`) and drops after it (`end_input_frame`), like `CanvasView` does.
- **Page menu label**: the canvas asks the tab for the "Create bubble" binding text through
  `CanvasHooks::page_menu_create_bubble_shortcut` (`TranslationInputLabels::create_bubble_menu`,
  `None` while unbound). Widget-owned input (text editing, `Wheel*` widgets, buttons, egui's own
  context menus of the MT buttons) is not a binding.

Text-detector mask GPU textures are a reconstructable display cache. The tab exposes memory
snapshots and coarse eviction methods for those textures, while detector result masks and stored
text-mask data remain intact for redraw, editing, and disk persistence.

Python-backed OCR/detector routes call the backend service over the framed IPC protocol (AF_UNIX
socket on unix, token-authenticated loopback WebSocket on Windows; the transport is chosen inside
`ms-backend-ipc`). All backend transport goes through `ms_backend_ipc::shared_client()`;
`ms_local_ai::backend_health` owns no TCP/port state. OCR requests use `begin_call`/`CallHandle` for explicit
cancellation. App-managed
model files must be resolved through `ms_sysprobe::ai_models` before backend initialization; EasyOCR
and Surya weights are external models too (below): the backend loads them offline and never
downloads (protocol v12). PaddleOCR
detector-only downloads only detection files, while full PaddleOCR also downloads the selected
recognition language. Those app-managed downloads are cancellable and report progress: every
`ai_models::ensure_*` call takes an `EnsureCtx` (cancel flag + event sink). The OCR controller
gives each Load / Recognize its own cancel token (a newer command sets the previous one, the drop
sets the newest; `TranslationOcrController::command_cancel`) and forwards the thinned events as
`OcrControllerEvent::ModelDownloadProgress` (state `DownloadingModel`); a cancelled download
ends as `RecognizeCancelled` (a load: `StateChanged(NotLoaded)` + `RecognizeCancelled` for the
waiting request), never as an error. Every request ends in exactly one terminal event: a request
waiting for a load that a newer one replaces, and a backend recognize superseded by a newer
selection, end as `RecognizeCancelled`; a request the worker can no longer take (send to a dead
worker, a worker that disconnects with requests in flight) ends as `RecognizeFailed`; so the tab's
pending targets (`pending_ocr_regions` via `drop_ocr_region`, bubble inserts, the detector batch)
are always released. The detector batch has one flag per batch (set on drop,
checked before every page) and emits `TextDetectorControllerEvent::{ModelDownloadStarted,
ModelDownloadProgress, DetectCancelled}`; the region helpers used by Cleaning take the caller's
`EnsureCtx` and return `DetectFailure::{Cancelled, Failed}`. The tab keeps the latest event per
job (`model_download_status::ModelDownloadStatus`) and the OCR / detector panels draw it with a
Cancel (`TranslationOcrController::cancel_model_download`, active only in `DownloadingModel`;
`TranslationTextDetectorController::cancel_detection`). How a download reads — file, `a / b`, the
filled bar when the size is known, the retry wait as progress — has ONE owner,
`model_download_status.rs`, which the Cleaning tools draw too. PaddleOCR-VL (IPC method `ocr.paddle_vl`) is a PyTorch/Transformers OCR
engine that needs no text detection and no language selection; it is shown on a second engine row in
the OCR panel so the tool panel stays narrow, next to Baberu OCR (`ocr.baberu` / native).
EXTERNAL models are downloaded as BUNDLES (`ocr::external_model_specs`): Baberu OCR, the selected
PaddleOCR-VL variant (`OcrRuntimeOptions::paddle_vl_model`, persisted per title as
`OCR.params.paddle_vl.model`, missing/unknown -> `official_1_6`), the EasyOCR model of the script
the selected languages pick (`ms_sysprobe::ai_models::easyocr`, the one owner of the code table
and the script choice; all scripts share one directory), and Surya OCR = detection +
recognition checkpoints. They are NEVER downloaded implicitly: `ocr.rs::gate_external_models` is
the one pre-dispatch gate (load warmup before the route decision, every non-AI-API recognize).
For EasyOCR it first refuses a language list easyocr cannot read (`easyocr_lang_error_message`,
one message per `EasyOcrLangError` variant); then it turns "not installed" into a localized error
naming the panel's download/resume button for the whole bundle (caption owner:
`ocr_model_download::offered_download_label`, the same aggregation through
`download_label_for_statuses`); a miss also emits `OcrControllerEvent::ExternalModelMissing(bundle)`,
on which the tab re-probes the panel state, so the button the error names is the one drawn. The
resolved paths travel in the request header (`paddle_vl_model` key + absolute
`paddle_vl_model_dir`; `baberu_model_files` `{vision, prefill, step, vocab}`; v12
`easyocr_model_dir`, `surya_recognition_dir` + `surya_detection_dir`, key names from
`ms_backend_ipc::protocol`), and `easy_langs` is sent canonical, so the backend owns no layout and
no aliasing. The Surya text detector uses the same detection checkpoint
(`text_detector::SURYA_DETECTOR_SPECS`, its own download block in the detector panel). The only
downloader is a panel's Download button -> `OcrModelDownloadController` (one download at a time
across both panels, worker threads, one byte progress over the bundle, Cancel;
`ms_sysprobe::ai_models::external::download_external_model` per spec, which retries transient
network failures itself and reports the wait as the `Retrying` phase). A download that ends
without installing (cancel, failure) leaves a `DownloadNotice` and re-probes the bundle's specs; a
second bundle's Download is disabled while one runs (busy line names the running one).
A download interrupted by a tab rebuild (controller drop) or a crash is recognized by its staged
bytes: the next probe reports `Partial`, shown as "download was interrupted" + Resume; no
process-global state keeps it.
`ocr::external_model_specs` is the single owner of the engine -> external bundle mapping.
AI API OCR bypasses the Python backend and uses Rust `genai` (through `ms_ai_api`, streamed by
`exec_chat_validated`) from the OCR worker thread; its answer is cleaned
(`structured::strip_think_blocks` + `clean_plain_text`: `<think>` blocks and a code fence
wrapping the whole answer; quotes are kept) before line splitting — the split and "keep line breaks"
join is the same `assemble_line_ocr_result` the native routes use (reflection is part of the
prompt, never applied to the answer) — and an answer left empty is asked for once
more (`ocr.rs::ai_api_ocr_validation`; nothing else is retried, plain text has no structure); provider API keys are read/written only through `ms_ai_api::keys` (OS credential
store) and never persisted to project or user JSON settings. For an OpenAI-/Anthropic-compatible
service the request goes to the panel's base URL (`ms_ai_api::AiApiTarget`) and a missing key is
not an error; OCR and MT raise their "API key missing" errors only when `requires_key()`.
The default AI API OCR system instruction has ONE owner, `ocr.rs::default_ai_api_ocr_instruction`,
worded for the OCR input language (the MT panel's source-language code; `auto` names none). The
instruction stays user-editable and persisted: a stored text that `is_default_ai_api_ocr_instruction`
recognizes as untouched (blank, any generated default, the legacy hard-coded Korean text) follows
the language — at request time (`OcrRuntimeOptions::ai_api_source_lang`) and in the panel
(`OcrPanelOptions::sync_default_ai_api_instruction`); any other text is a user edit and is sent as is.
The native ONNX Runtime OCR path (MangaOCR + PaddleOCR + Baberu OCR) is selected in `ocr.rs` by the
pure `ocr_route` helper: with `General.ai_runtime == "native"` and a non-`Suspect` provider-scope
SIGILL guard, MangaOCR with an ONNX export (`base_onnx`/`2025_onnx`; `base_torch` has no native path)
routes to `NativeManga`, PaddleOCR routes to `NativePaddle` (any language) and Baberu OCR to
`NativeBaberu` (`ms_native_runtime::recognize_baberu`, vision on the selected EP, decoders on CPU, on the gate's resolved files);
every other case
(other engines, torch-only model, Suspect guard, backend runtime) routes to the Python backend. On
the native route the OCR worker decodes the crop to RGBA and calls
`ms_native_runtime::recognize_manga` / `recognize_paddle` / `recognize_baberu` (desktop-only),
assembling lines/text
with the same `join_newlines`/`reflect_strings` rules as the backend (Paddle's lines are joined with
`\n` first). The guard scope uses `native_runtime::native_load_scope_key()`
(`{build}:{provider}[:{device}]@{version}`) so the pre-check matches the build/provider/adapter that
will actually load. Native PaddleOCR text detection is selected by
`text_detector::pipeline::detector_native_route` (native runtime + non-Suspect guard, read by
`text_detector/native.rs`): pages and the inline `detect_paddle_mask_for_image` then run the
shared detection pipeline on `NativePaddleRunner` (`native_runtime::paddle_det_forward`), and a
native runner failure re-runs the page on the backend runner.
A native error (OCR or detection) is always logged. On the OCR recognize dispatch it falls back to
the backend ONLY when the backend is up; when the backend is offline the native path is the only
path, so `run_recognize_command` surfaces the real native error (`Нативный ONNX: <reason>` from
`NativeRuntimeError`) instead of the misleading "backend offline" string
(`try_native_ocr` → `NativeOcrOutcome`, `native_failure_should_surface`). These paths are compiled
out on the web build.
Native OCR/detection run WITHOUT the Python backend: BOTH the controller readiness gate AND the UI
trigger gate are route-aware. `ocr.rs::warmup_ocr_engine` and `text_detector/mod.rs::run_detect_batch`
skip `ensure_v2_backend_ready` (and the backend warmup) when the active route is native — the
in-process runtime/model load lazily on first use and the controller reaches `Ready` with the
backend offline. The UI trigger gate `ocr::ocr_route_availability(engine, model, runtime, guard,
local_ai)` is the single source of truth for "can this OCR selection run here, and on what?":
`ocr_route_kind` maps the selection to `AiRouteKind::{Cloud, Native, Backend}` (native ONNX routes,
backend routes, and `AiApi` over `genai` = `Cloud`), and `AiRouteKind::availability` checks it
against the process's `LocalAiAvailability`; `tab/ocr_routing.rs` (`current_ocr_route_availability`,
`ocr_request_refusal`) calls it, reading the runtime+guard from a per-tab snapshot that a background worker
(`tab/route_inputs.rs`) re-reads at most every `OCR_ROUTE_INPUTS_CACHE_TTL`, one read in flight,
so the GUI thread never touches the disk for it (until the first read lands the snapshot is
unknown and the gates pass when any possible input would, `route_inputs::gate_over_inputs`, so no
false refusal; the workers route with their own fresh read; each landed read republishes the
native-ORT capability slot). Every OCR trigger
(drag-box, advanced/quick recognize, the load button, block OCR, the proactive health check)
consults it, so a native selection dispatches with the backend offline. Only backend routes warm
the backend and probe backend health. The native→backend fallback still applies when native
inference fails AND the backend is up (the fallback path re-checks readiness itself).
AI API MT also bypasses the Python backend. Plain untranslated text batches are sent grouped by
character and require flat JSON responses containing every bubble ID. When the "existing translation
in context" option is on, a scope translation also includes already-translated replicas as ordered
read-only context (`MtTranslateItem.needs_translation == false`, flagged `"context": true` with
their existing translation); such batches switch to the ordered per-item representation so the model
sees the exact reading-order interleaving. `split_ai_mt_batches` keeps context around the
translatable replicas but cuts each batch right after its `batch_size`-th translatable replica, so a
context replica beyond a reached per-batch limit is deferred until translation reaches its window;
context replicas are never returned, counted, or reported as failures. When ImageBubble inclusion is enabled for a
model not listed as text-only (`ms_ai_api::image_input_support` != `NotSupported`), the MT worker attaches each image in ordered message parts using the selected
visual detail level and requires `original_text` plus `translation` for those IDs. A multi-area
ImageBubble is sent as one item whose `MtImageInput.areas` lists every text area (description,
current original, image-relative bbox); the model must return
`{id, areas:[{original_text, translation}, ...]}` and the per-area result is applied through
`CanvasView::apply_machine_translation_areas`. Binary image
parts are sent only in the active API request; retained chat history stores a text-only batch
record so image payload bytes do not consume the normal context budget. It keeps chat history
between batches and prunes old user/assistant turns when the configured context-fill budget is
exceeded. Progress events expose translated/error counts, approximate context usage, and the number
of replicas removed during context pruning; batches may include existing bubble translations when
the user enables that context option.
AI API MT answers are read by `ai_mt_response::BatchCollector` (only reasoning BEFORE the answer
is stripped; of the JSON candidates found inside prose or fences with safe repairs the one that
validly answers the most requested ids wins; a cut-off or broken array still yields its complete
elements, ids as integers or numeric strings, every element
validated against the request's expected ids and shapes, multi-area counts checked). Valid items
are applied as soon as an answer arrives; when ids are missing or invalid (or the answer was cut
off at the output limit) the ONE repair retry re-asks only those ids and its valid items are
merged (first valid answer per id wins); what is still missing becomes a per-item failure with
its localized reason. The retained history of a batch is exactly its user message plus ONE
assistant turn with the collector's canonical JSON of the accepted items (`history_json`), so
pruning (`prune_ai_mt_chat_context`, two messages per batch) and the text-only history rewrite
(`replace_last_ai_mt_user_message_with_history`, `last_mut()`) stay correct and later batches
never see a malformed turn. Content checks (identical to the source, still in the source writing
system, more than 4x longer) only MARK an applied item (`ItemWarning`), never retry. Per-item
failures and warnings are kept by `TranslationMtController::run_issues` (cleared per run, logged
once there) and listed by the MT panel. The opt-in strict JSON mode (`AiMtOptions::strict_json`,
persisted as `machine_translation.ai_api.strict_json`, default off) sends a provider JSON schema
(`ai_mt_response::strict_answer_schema`; batches answer `{"items": [...]}`, which the system
prompt then asks for) and stops sending it for the rest of the run once a provider rejected it.
`BubbleClass::Hint` never enters an MT batch — not as a translatable item and not as context — in
any of the three collection paths (scope run, per-ImageBubble mode, explicit per-id request): a hint
is an author note addressed to the translator, not a replica.

## Files and submodules
- `lib.rs`: module wiring and public re-exports for `TranslationTab` (+ `TranslationDrawParams`)
  and `TranslationTabState`; everything else reaches the studio through
  `ms_studio_tab::StudioTab`.
- `input.rs`: every input action of the tab (ids, contexts, targets, `declarations`), the
  context rule (`contexts`), the recent-character parameter (`recent_char_rank`) and the binding
  texts of all hints (`TranslationInputLabels`, incl. the bottom-hint rows). `input/tests.rs`:
  the declarations golden (ids, kinds, scopes, default labels), conflict-freedom next to the view,
  canvas and brush sets, mode precedence, contexts per state, label-built hint rows.
- `host.rs`: `TranslationTab` (canvas + state, `new` seeding only the scroll salt, bubbles model
  and bottom-hint collapsed flag — canvas settings are pulled from `BubblesModel` on the first
  draw), `TranslationDrawParams` / `draw` and the `StudioTab` impl. `host/hotkeys.rs`: the execute
  dispatch of every fired command. `host/tests.rs`: declarations identity, execute coverage of
  every declared command, contexts, hint-collapsed round trip, dock builder.
- `tab.rs` + `tab/`: `TranslationTabState`, the tab orchestrator and `CanvasHooks`
  implementation. `tab.rs` holds the shared types, constants, the struct, its lifecycle and host
  API; behaviour is split by concern into `tab/` children (OCR routing, characters / footer, OCR
  flow, detector storage / mask / block OCR, MT, canvas hooks, settings IO, tools, dock, canvas
  input, region detection, background drain, scrollbar marks, browser extension) — map and
  contracts in `tab/MODULE_README.md`. The title `settings.json` is
  written only via `ms_docstore::update` touching the tab's four sections (`OCR`,
  `machine_translation`, `composition`, `text_detector`); a malformed file makes the save fail
  and is left untouched. Retired `text_detector.params` keys (`STALE_TEXT_DETECTOR_PARAM_KEYS`:
  the old CTD font-size and rearrange-batch params) are ignored on read and stripped on write.
  The `characters.json` watch uses `ms_docstore::signature`.
- `ocr.rs`: `TranslationOcrController`, OCR load/recognize worker, framed IPC calls via
  `shared_client()` (with `begin_call`/`CallHandle` for cancel), AI API OCR request building over
  `ms_ai_api` (tracked + stoppable via the controller's `GenerationTracker`), crop encoding (and
  its page placement, `crop_image_with_placement`), page image LRU cache, and
  model-download/load-state events (per-command cancel tokens for the app-managed model
  downloads). `ocr/` holds its child modules (`ocr/MODULE_README.md`): `ocr/neurolate.rs`, the
  NeuroLate worker step (one paid request, text + optional region).
- `model_download_status.rs` (`pub`): `ModelDownloadStatus` (latest event of a job, empty once
  its last file is in) and `draw_model_download_status` (localized line / per-file bar); used by
  the OCR and detector panels and by `ms-tab-cleaning`.
- `page_crop.rs`: the one uv selection -> page pixel rect rule (`uv_rect_to_pixel_rect`), used by
  the OCR crop and the detector's region detection of the same selection.
- `neurolate.rs` + `neurolate/view.rs` (map: `neurolate/MODULE_README.md`): the tab's NeuroLate settings (`NeurolateSettings`:
  language `Auto` / code, comic type `Auto` / webtoon / manga, the OCR engine's "create mask")
  persisted under `OCR.params.neurolate`, the one owner of their "Auto" rules
  (`effective_language`: MT source language when it maps, else KO; `resolve_comic_type`:
  Ribbon / Custom / unknown -> webtoon, Pages -> manga) and of `request_params`, plus the shared
  settings block `draw_neurolate_settings` (drawn by the detector panel; the OCR panel reuses it).
- `ocr_case_fix.rs`: pure, GUI-free post-OCR ALL-CAPS normalization (detector +
  character state machine) used by `ocr.rs`'s post-processing helper.
- `ocr_model_download.rs` (`pub`, reused by `ms-tab-cleaning`): `OcrModelDownloadController`
  (stat-only per-spec status worker + at most one job thread — a download of a bundle's
  not-installed specs in order, or a verify (`start_verify`, `ms_sysprobe` `verify_external_model`
  per spec: damaged files and the markers sharing them are deleted, then the bundle and every
  invalidated spec are re-probed and an intact / N-removed notice is left), or an automatic
  ADOPTION (`Adoption` bodies: the status worker runs `unmarked_files_present` for every `Missing`
  answer; a bundle registered by `probe_unknown` / `request_bundle_status` whose specs are all
  probed starts one `adopt_external_model` job over its adoptable specs, shown as `Verifying` with
  phase `Adopting`, `JobKind::Adopt` to other bundles; a refusal leaves `AdoptMismatch` /
  `AdoptFailed` / `AdoptCancelled` and Download repairs only the differing files; the spec ids
  tried are recorded process-wide in `AUTO_ADOPT_ATTEMPTED`, so a tab rebuild never re-hashes;
  a decided bundle is not re-registered by the per-frame `probe_unknown` until a spec of it is
  probed again or `request_bundle_status` re-asks it; `running_job` names the running job kind)
  — with progress over the whole bundle coalesced in a mutex snapshot,
  injected worker bodies for tests; a running download is cancelled and DETACHED on drop, never
  joined on the GUI thread), `bundle_key`, `OcrModelDownloadState` + `DownloadNotice` +
  `OcrModelPanelSnapshot` (what the panel block draws: aggregated state, missing bytes),
  `offered_download_label` / `download_label_for_statuses` (the one owner of the visible download
  caption, also named by the gate errors), the `ExternalModelError` -> localized message mapping
  (no English detail reaches the user; the full `Display` is logged) and the localized byte
  formatter. Test fakes (`fakes`) are shared with the tab's wiring tests; `with_jobs` (tests)
  disables adoption, `with_parts` injects adoption bodies and a fresh record.
  Constraint: `OcrModelDownloadState` is matched exhaustively and `OcrModelPanelSnapshot` built by
  literal in `ms-tab-cleaning` (SDXL), so a new job kind is expressed through `JobKind` and the
  progress phase, not a new state variant or snapshot field.
- `text_detector/`: `TranslationTextDetectorController` and batch worker (`mod.rs`), the
  model-based entry point and fallback policy (`pipeline.rs`), the backend forward runner
  (`backend.rs`), native Paddle routing and runner (`native.rs`, desktop only), local classic
  detector (`classic.rs`), the NeuroLate cloud detector and its answer -> mask + blocks
  conversion (`neurolate.rs`), and the region helpers reused by cleaning tools (`region.rs`). Own
  `MODULE_README.md`.
- `machine_translation.rs`: `TranslationMtController`, `MtService`, AI API MT options, batch
  item/request types including optional ImageBubble image payloads, per-run worker thread
  lifecycle, cancellation (`MtRunCancel`: flag + per-run generation tracker), chat context
  pruning, the validated AI exchange per batch / per ImageBubble, the run's issue list
  (`MtRunIssue`), run-level context notices (`MtControllerEvent::ContextNotice`: an unreadable
  notes template, character roster or term list is logged with its path and all of a prompt's
  failures are shown as one warning toast; the request goes without those blocks), the plain-MT
  run loop (`PlainMtRun`: batching, per-bubble events, cancel, rate-limit stop), and backend
  dispatch (`translate_texts_via_translator`: the String-typed entry of the cleaning tab, a
  detached limiter call).
- `ai_mt_response.rs` (native): pure reading / validation / merging of AI MT answers
  (`BatchCollector`), the repair message, per-item failure reasons, canonical history JSON,
  content checks (`content_warnings`) and the strict-output schema (`strict_answer_schema`).
- `machine_translators/`: UI-agnostic Google, Yandex, and DeepL MT provider implementations behind
  `MachineTranslatorBackend` (typed per-text `MtItemError`, per-run `run_session` factory, shared
  HTTP transport facts in `http.rs`; DeepL's one process-wide HTTP agent, a read-only global),
  plus `browser.rs`, the same three services driven in the user's browser through the
  ManhwaStudio extension (`ms-browser-bridge`). Own `MODULE_README.md`.
- `mt_transport.rs`: the live per-service transport (`current` / `set` /
  `seed_from_user_settings`, relaxed atomics seeded by the root's `SettingsStep::MtTransport`);
  the persisted value and its writer are `ms_config::mt_transport`.
- `mt_limits/`: the process-wide MT request limiter (pacing, cooldowns, cancellable reported
  waits, long-text split) every provider request goes through, shared by Translation runs and
  the Cleaning tab's prompt translator. Its one static registry is a documented exception to
  global §5 (header of `mt_limits/registry.rs`). Own `MODULE_README.md`.
- `panels/`: side-panel UI modules for OCR, bubble cards/footer fields, machine translation,
  text detector controls, composed text, and OCR language catalogs.
- `adv_rec.rs`: floating advanced-recognition crop preview/editor with async crop preparation,
  brush overlay, rotation, zoom, and local quick-selection OCR actions. Its brush and the detector
  mask-edit brush take the shared brush actions (`ms_tools::mask_brush`): the host publishes
  `BRUSH_ACTIVE` while either is on (`TranslationTabState::brush_input_active`) and applies them
  before drawing (`apply_brush_input`); its view zoom and its preview gestures come from the input
  frame (see "Input").
## Contracts and invariants
- Input action ids are persistence keys (`user_config` -> `Hotkeys`): never rename one. Every
  declared command has an execute arm in `host/hotkeys.rs` (`host/tests.rs` checks it); where a
  command may run is its declared conditions, never a check in the arm. `input/tests.rs`
  (`declarations_golden`) pins the set; the root's `input_registry_golden` only checks that the
  merged studio registry stays valid and conflict-free.
- Only `TranslationTab` touches the Translation canvas on the root's behalf; the root reaches it
  through `canvas()` / `canvas_mut()` (view sync, bubble flush) and the `StudioTab` hooks.
- OCR, detector, MT, storage load/save, crop preparation, and backend health work must not block
  the GUI thread.
- Background results reach the shared models whether or not the tab is active: the hidden-tab
  drain (`tab/background.rs`) and the draw (`draw_canvas_overlay_top_left`) call the SAME poll
  methods, which consume their channels, so a result is applied exactly once whichever runs
  first. A result that needs a drawn UI (canvas selection, advanced window, panels) stays in the
  draw; a new background job whose results write a shared model must be added to
  `BackgroundWork` and drained in both places. Toasts and the OCR clipboard copy may fire while
  the tab is hidden (accepted).
- Canvas integration must stay behind `CanvasHooks` or typed `CanvasView` APIs; do not duplicate
  canvas bubble/viewport state machines in translation code.
- `draw_canvas_overlay_top_left` must reach its dock block on EVERY frame this tab is active: the
  dock's detached OS windows are immediate viewports and exist only while `PanelDock::end` shows
  them, so an early return above it would close the user's windows for that frame.
- The default dock layout is the DICTIONARY of this tab's dock tabs: a `TabId` missing from
  `translation_default_dock_layout` is dropped from the user's stored arrangement on every load, so
  adding a dock tab here means adding it to that builder too (all eight are pinned by
  `host/tests.rs` and `tab/dock.rs`). It is registered under
  `AppTab::Translation.key()` through `StudioTab::default_dock_layout` as well; a builder wired in only one
  of the two places silently resets the stored arrangement.
- A tool is open iff its `ToolToggles` flag is on; nothing reads that from the dock's output. The
  toolbar is always visible (the only way back to a switched-off tool), and the tool tabs are
  declared on every frame. Focus happens only on the frame a tool is switched on.
- The `tool.<name>` extras flag keys and the `translation.tool*` tab ids are persistence keys:
  never rename one without migrating stored layouts.
- Text detector page results carry source size, block rectangles in source pixels, mask size, and
  mask alpha bytes. Mask buffers must match `width * height` and invalid geometry must be rejected.
- Text-detector mask GPU eviction must drop only tiled `TextureHandle` pages and preserve detector
  result data, text-mask model data, and pending storage jobs.
- When mirroring a detection into `TextMaskModel`, the tab passes the detector boxes that match the
  stored mask: `sync_text_mask_page_from_result` stores the RAW backend glyph mask and so passes the
  RAW `result.blocks`; `materialize_text_mask_page_from_blocks_if_missing` rasterizes the RESOLVED
  (expanded/merged) blocks and so passes those resolved blocks. `detector_rects_to_blocks_px`
  converts float `TextDetectorRect`s to integer source-pixel `[x1,y1,x2,y2]` covering rects; boxes
  stay in source-page pixel space (the model is responsible for no scaling).
- Persisted detector results live under `ProjectPaths::text_detection_dir`; storage jobs must load
  and save real block/mask data rather than synthesizing success.
- Footer metadata and ImageBubble metadata live in `Bubble.extra` fields and are synchronized
  through debounced `CanvasView::patch_bubble_extra_fields` calls. Image bubbles do not expose the
  translation character autocomplete; their panel controls edit source type, image path/crop page,
  description, original text, and translation. Translation owns the ImageBubble actions: the
  create-image command (Q) creates an empty external-image bubble under the pointer, and the crop
  gesture (Shift+Q + drag) captures a rectangle as the page crop for the selected ImageBubble or
  creates one at the crop center. External ImageBubble
  files are picked through ONE tab-level file-dialog slot (`TranslationTabState::image_bubble_pick`,
  an `ImageBubbleFilePick` started by the canvas footer or a bubbles-panel card and resolved per
  frame by `poll_image_bubble_pick`, which keeps both «choose file» buttons disabled while it is
  open). They are written to the chapter unsaved staging `image_bubbles/` directory and stored as
  chapter-relative paths so the saved chapter can resolve them after commit. The create command
  fires on the press completing its exact chord (Shift+Q never fires it); the press then cannot
  activate the crop mode until Q is released, so pressing Q before Shift creates a bubble and
  starts no crop.
- Translation also owns the hint-bubble command (`crate::input::CREATE_HINT_BUBBLE`, default H):
  it creates a `BubbleClass::Hint` under the pointer.
- HINT ATTACHMENT is the reason a hint exists, and it is a contract of this tab, not a formatting
  detail. A hint is an author note addressed to the translator: it carries ONE line in
  `Bubble.text`, points at an anchor and owns an area exactly like an ordinary aside text bubble
  (canvas geometry, `crates/ms-canvas/src/MODULE_README.md`), and its only job is to be injected into the
  composed prompt next to the replica it comments on. Binding is FORWARD, in composition sort
  order (`img_v`, or the replica-order spin box the hint shares with every other class): a hint
  attaches to the next ORDINARY replica after it, skipping image bubbles and other hints. Two
  consequences that must not be re-derived by hand: (1) the authoring rule is "place a hint ABOVE
  the replica it annotates" — a hint below its replica binds to the wrong one or becomes trailing;
  (2) a hint is emitted only if its target replica is emitted, so a hint in front of an
  already-translated (filtered) replica emits NOTHING, while a hint with no following replica at
  all is trailing and is force-emitted at the very end past the character limit. That asymmetry is
  deliberate. The full rule — the barrier model, `classify_hint_bindings`, the atomic
  hint+replica limit admission, and the `merge_same_character` interaction — is documented once,
  in `panels/MODULE_README.md`; do not restate it elsewhere.
- `build_bubble_footer` is a class dispatch placed AFTER the shared replica-order spin box, so every
  bubble class inherits the order control: `Image` gets the image source/crop controls, `Hint` gets
  only the `hint_show_outside_translation` checkbox, and `Text` gets the character autocomplete.
  A checkbox/combo in the footer must read a still-pending `pending_footer_patches` entry before
  falling back to `Bubble.extra`, because the patch is debounced and the model lags a few frames.
  `bubble_status_style` exempts `BubbleClass::Hint`: a hint has no original, translation, or
  character, so the incomplete-replica rules would border every hint. `canvas_scrollbar_marks`
  carries the same exemption for the same reason — a hint's single line lives in `text`, so a
  translation-status stripe for it would always read "translated".
- The character list (`character_names`) is ASSEMBLED, not loaded. It is
  `build_translation_character_names(project_roster, chapter_names)` and its order IS the suggestion
  priority: the "no character" sentinel, the `characters.json` roster
  (`character_names_project`), the free-form names actually typed on this chapter's bubbles
  (`character_names_chapter`), then the built-in service names — trimmed, deduplicated
  case-insensitively, first spelling kept.
- Its two consumers do NOT get the same slice, and that difference is a correctness contract, not
  cosmetics. The bubbles panel takes the list whole: its character control is a combo box, the
  sentinel is a selectable row and `character_names[0]` is the panel's fallback value. The footer
  takes `footer_autocomplete_candidates(&character_names)` — the same slice with a LEADING
  `footer_no_character()` removed (conditionally, never a blind `[1..]`) — because its control is a
  free-text `AutocompleteLine` that splices the tail of a candidate from a word boundary: with
  "(не указан)" among the candidates, typing "ук" writes the fragment `указан)` into the field with
  `changed = true`, and one debounce later that fragment is a persisted character name no sentinel
  filter can recognize. Anything added to the assembled list must be safe to splice tails from.
  Both consumers borrow — the list stays ONE `Vec<String>` and is never cloned per frame.
- `collect_chapter_character_names` reads the whole-chapter bubble snapshot with the same
  `extra["character_name"]` reader the footer state uses, ignores `is_known_character`, and drops
  blanks, the `footer_no_character()` / `footer_no_characters()` sentinels, and the `Image`/`Hint`
  classes (neither owns a character field). The sentinel filter is REQUIRED: `panels/bubbles.rs`
  persists `footer_no_character()` into `Bubble.extra` when the "И.П." checkbox is switched on for
  a bubble whose name is not in the list.
- The chapter scan is kept off the per-frame path by `sync_footer_tracking`'s
  `hook_bubbles_key` gate (the `project.bubbles` allocation + `hook_bubbles_revision`) and reuses the snapshot already taken there; inside the gate the
  scan always runs and only the reassembly is skipped when the collected set is unchanged. The
  revision moves on every footer patch flush (~`FOOTER_PATCH_DEBOUNCE_SECS` while a name is being
  typed), so that scan must stay linear and allocation-light. The same debounce is why a name
  typed in the footer joins the list one flush later rather than on the keystroke. The chapter
  scan reads the runtime-inclusive snapshot while `collect_recent_character_history` beside it
  reads `project.bubbles`; the asymmetry is deliberate and documented at the call site.
- Post-OCR processing is ONE helper, `ocr.rs::apply_post_ocr_processing`, and all three
  recognize routes (AI API, native ONNX, backend IPC) must call it — engine parity of the
  published text is a contract of this module. The caps-lock decision and the caps-lock
  rewrite straddle the user's `CharReplacementRule`s: `ocr_case_fix::looks_like_caps_lock`
  runs on the RAW engine output (a repair rule such as `"0" -> "o"` must not be able to
  introduce a lowercase letter and veto the fix), the substitutions run next, and
  `ocr_case_fix::apply_caps_lock_fix` — which is unconditional and trusts the caller's
  gate — runs last so it sees the final punctuation (the default `…` -> `...` rule creates
  real sentence terminators). The detector accepts a result only when the whole of it (both
  `text` and `lines`) holds at least one uppercase Latin/Cyrillic letter and no lowercase
  one. Characters outside those two scripts never take part in the decision and are copied
  through unchanged, so byte-identity is guaranteed only for a result with no
  Latin/Cyrillic letters at all or one the detector rejected — `"HELLO 日本"` becomes
  `"Hello 日本"`. The rewrite keeps only the first letter of a stream and the first letter
  after `.`/`!`/`?`/`…` uppercase, with ONE exception: the English pronoun `I` and its
  contractions (`I'm`, `I'll`, `I've`, `I'd`, either apostrophe) stay capitalized anywhere.
  That exception is a whole-word match applied as a second pass per fragment, so `is`, `in`
  and `i18n` are untouched and it re-cases exactly one ASCII character. The rewrite threads
  one sentence state across the elements of `lines` so a sentence continues over a line
  break, but rewrites each element in place: `lines.len()` is preserved structurally, and an
  element that already contains a `\n` is never split.
- App-managed OCR/detector model downloads must go through `ms_sysprobe::ai_models`. Missing models,
  backend failures, unavailable credential stores, AI API request failures, and unsupported engines
  must surface clear errors.
- MT worker events are accepted only for the active run id; cancelled/detached run output must not
  mutate canvas state. AI API MT responses must be matched by bubble ID before applying text; for
  ImageBubble results, original text and translation are applied together, and a multi-area
  result is applied only with exactly `area_count` areas. The per-ImageBubble mode alone accepts
  a wrong-id element for its single target.
- A failed AI run whose error matches `is_probable_quota_or_limit_error` (keyword/HTTP-code scan)
  stops quietly: instead of an `Error` toast the panel shows the sticky `MtStopNotice` with the
  full provider error available behind a toggle. Other run failures keep the `Error` toast.
- Status colours are never hand-typed: canvas toasts (`push_toast`) and the detector status line
  (`set_text_detector_status`) take an `ms_theme::Severity` chosen by the MEANING of the message
  (`Info` in progress, `Success`, `Warning` nothing done/partial/retry, `Error` failed); panel
  status labels use `ms_theme::status::*`; the OCR / advanced-recognition selection rectangles use
  `ms_theme::canvas::SELECTION`. Detector/debug overlay colours stay local.
- `build_ai_mt_request_preview` assembles the first AI request (system prompt + first batch user
  message with decoded inline images) without contacting the provider, reusing the exact item sort,
  batch split, and `build_ai_mt_user_parts` ordering as a real run. `tab/mt.rs` runs it on a worker
  thread (it loads/decodes images) and shows the result in the debug "Полный запрос" window; the
  per-button right-click action only applies to the AI API tab.
- The AI MT worker emits diagnostic lines to `runtime_log` (session `last.log`) tagged
  `[MT][run <id>]`: a run-start summary (service, model, batch counts, translate/context/total item
  counts, image-bubble count, image detail, context budget), one line per batch before the request
  (replica/translate/context counts, attached image count and KiB, history depth, pruned total) and
  one after the response (response length, parsed entry count or parse error), plus the per-batch
  and final translated/errors totals. Raw provider request errors are logged before being wrapped
  into `RunFailed`. Logging must never include API keys or image binary contents.
- Shared model locks must be short-lived and released before image decoding, HTTP calls,
  composition/export, storage I/O, or callbacks.
- Single-image session (one picture edited through a scratch chapter that is deleted on exit, no
  title): the tab never asks the project for its session kind; it reads the
  `ms_models::SessionCapabilities` the root pushes once (`set_session_capabilities`, also copied
  into `AiMtOptions::session_caps` for the worker). It reads no title document and offers nothing
  that only lives in the scratch. Text-detection storage is off (`chapter_storage`: no lazy load,
  no «Сохранить» button via `TextDetectorPanelView::storage_available`); the character roster is
  not read (`title_documents`: empty roster, no mtime probe); the MT notes / characters / terms
  toggles are hidden (`draw_machine_translation_panel`'s `project_context_available`, fed from
  `title_documents`) AND ignored by the prompt (`project_context_sources`), so their
  persisted values stay untouched; the composed-text export dialog starts in
  `ProjectData::user_facing_dir`, never in the scratch. Every file dialog of this crate that sets a
  starting directory must use `user_facing_dir`.

- Local-AI gating (`ms_local_ai::local_ai`): `--no-ai` means "no LOCAL AI runtimes" (Python backend and
  native ONNX). Cloud AI API OCR/MT and the plain machine translators are NEVER gated by it or by
  the platform: the OCR and MT panels are never disabled as a whole, MT has no availability gate,
  and only a route needing an off runtime is refused, with `LocalAiOff::user_message` as the
  text. Every route decision goes through `AiRouteKind::availability` (OCR via
  `ocr::ocr_route_availability`, detection via `TextDetectorAlgorithm::availability` +
  `DetectorRoute::route_kind`); never test the flag or the platform anywhere else. Every router
  reads the runtime through `local_ai::effective_ai_runtime`, never `AiRuntime::from_user_settings`
  directly. The tab publishes native ORT as armed only when `local_ai.native_onnx` is `Ok`.
  The NeuroLate detector is a cloud route too (`AiRouteKind::Cloud`): never gated by `--no-ai`.
- NeuroLate (cloud, paid): the tab holds ONE `NeurolateSettings` store and ONE
  `NeurolateKeyPanel`; both panels edit / draw those, never copies. Every request's language
  and comic type come from `crate::neurolate::request_params` (via `neurolate_params`), never
  re-derived. The key panel is pumped only where NeuroLate UI is drawn or while its work is in
  flight (its first pump reads the OS credential store). The API key is read on workers only
  (`text_detector::neurolate::NeurolateKey::read`, once per batch / region) and never logged. A
  paid request is never retried; the detector's batch cancel flag (download Cancel, controller
  drop) abandons its wait, the user's Stop does not (the page in flight still finishes). The OCR
  engine follows the same rules: its key is read on the OCR worker (load and every request), its
  wait runs under the OCR `GenerationTracker` (panel Stop, controller drop: abandoned at once,
  the request may still be charged), and block OCR's automatic retry is skipped for it
  (`OcrEngine::retries_failed_block_request`).
## Editing map
- To add or change an input action (its default binding, conditions, scope) or a hint text built
  from bindings, edit `input.rs` (and its golden in `input/tests.rs`; the root's
  `input_registry_golden` checks the merged registry stays conflict-free); what a command does, `host/hotkeys.rs`; how a canvas selection, the
  lines editor or the mask brush reacts, `tab/canvas_input.rs`; the advanced window's gestures and
  zoom, `adv_rec.rs`. Zoom itself is the shared `view.*` set (`ms-input`, executed by the canvas).
  To change the per-frame canvas configuration or which caches the tab evicts, edit
  `TranslationTab::draw` / the `StudioTab` impl in `host.rs`.
- To change what `--no-ai` or a platform switches off, or which route kind needs which
  runtime, edit `crates/ms-local-ai/src/local_ai.rs` (`LocalAiAvailability::resolve`,
  `AiRouteKind::availability`);
  to change which OCR selection runs on what, edit `ocr::ocr_route_kind` / `ocr_route`.
- To change canvas overlays/hooks, OCR selection behavior, detector storage, footer sync, or
  settings persistence, edit the matching `tab/` child (`canvas_hooks.rs`, `ocr_flow.rs`,
  `detection_storage.rs`, `characters.rs`, `settings_io.rs`). To change the toolbar, the tools' on/off rules, focus or
  persistence, or what a tool's body does with its panel's actions, edit `tab/tool_panels.rs`;
  where the tools start and how big, `translation_default_dock_layout` in `tab/dock.rs` and the
  `TOOL_TAB_*_SIZE_PX` constants.
- To change which dock tabs this program tab declares, where its panels start, or what the
  text-detector edit boxes anchor to, edit `draw_panel_dock` / `text_detector_edit_panel_pos` /
  `translation_default_dock_layout` in `tab/dock.rs`; the «Лента» tab's own content, sizes, title and
  declaration (`canvas::declare_ribbon_tab`) live in `crates/ms-canvas/src/`.
- To change what the recent-character cards LOOK like or how they wrap, edit
  `draw_recent_characters_tab_body` in `tab/dock.rs`; to change how big their panel starts or how small
  it may get, edit the two `TRANSLATION_RECENT_CHARACTERS_TAB_*_SIZE_PX` constants there. The
  history itself is `collect_recent_character_history` and the digit lookup is
  `recent_character_entry_for_rank`, both in `tab/characters.rs`.
- To change what the character list holds or in what order, edit
  `build_translation_character_names` (group order/dedup) and `collect_chapter_character_names`
  (which bubbles contribute a name) in `tab/characters.rs`; to change WHEN it is rebuilt, edit
  `sync_chapter_character_names` and its call site inside `sync_footer_tracking`; to change what
  the FOOTER field is allowed to suggest (as opposed to the panel combo box), edit
  `footer_autocomplete_candidates`.
- To change OCR engine options, loading, recognition requests, IPC method names, crop handling, or
  page image caching, edit `ocr.rs` and the OCR panel in `panels/ocr.rs`.
- To change which external models an OCR engine needs or how a missing one is reported, edit
  `external_model_specs` / `resolve_external_model_dirs_in` / `gate_external_models_in` in `ocr.rs`
  (the Surya detector's: `text_detector/backend.rs`); to change how such a model
  is probed or downloaded, `ocr_model_download.rs` (pins and files live in `ms-sysprobe`); to change
  the download block's look, `panels/ocr_model_download.rs`.
- To change text detector algorithms, masks, backend IPC methods, or cleaning-tool detector
  helpers, edit `text_detector/` (map in its `MODULE_README.md`) and `panels/text_detector.rs`.
  Block order/cap and mask normalization rules live in `ms-text-detect`, not here.
- To change MT run lifecycle, provider selection, cancellation, or batch dispatch, edit
  `machine_translation.rs`, `machine_translators/`, and `panels/machine_translation.rs`.
- To change how MT reaches a service through the browser (packing, error meaning, timeouts),
  edit `machine_translators/browser.rs`; the bridge itself is `ms-browser-bridge`. What the MT
  panel shows about the extension and its install / start workers: `tab/browser_extension.rs`;
  the transport row and extension block UI: `panels/machine_translation.rs`.
- To change how often an MT service is asked, its cooldowns or the long-text split, edit
  `mt_limits/` (numbers only in `mt_limits/table.rs`); providers never pace themselves.
- To change bubble list editing, footer fields, search/filter behavior, or text write-through, edit
  `panels/bubbles.rs` and footer sync helpers in `tab/characters.rs`.
- To change composed text generation or export, edit `panels/composition.rs`.
- To change advanced manual OCR crop behavior, edit `adv_rec.rs`; keep OCR dispatch in `tab/ocr_flow.rs`.
- To change NeuroLate's "Auto" rules, persisted keys or settings widgets, edit `neurolate.rs` /
  `neurolate/view.rs`; the key panel step and request parameters, `tab/neurolate.rs`; the
  detector mode and the answer -> mask + blocks conversion, `text_detector/neurolate.rs`.
  The NeuroLate OCR engine's request / text join, `ocr/neurolate.rs`; when a selection rides on
  its OCR answer, `RegionDetectEngine::{selection_detects, ocr_region_want}`
  (`text_detector/region_detect.rs`) and `tab/detector_region.rs` (`ocr_region_want_for`,
  `receive_ocr_region`).
