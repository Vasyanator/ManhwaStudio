# Module: crates/ms-tab-cleaning/src

## Purpose
This directory implements the Cleaning tab. It provides the canvas-facing UI for editing
per-page clean overlays, quick text-mask cleanup, save/history controls, and the tool picker
backed by reusable cleaning tools. All of that UI lives in dock tabs; the tab owns no floating
surface of its own.

This directory is the crate root of `ms-tab-cleaning`, re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::cleaning`, so every existing
`crate::tabs::cleaning::…` path stays valid. Layer: the TOP of the library stack — above
`ms-canvas` / `ms-models` / `ms-tools` / `ms-widgets` and `ms-tab-translation` (backend
health, the text detector and the MT service), and below only `app.rs`, which calls
`CleaningTabState::draw`. It must never name `app` or `launcher`. There is NO dependency on
`ms-tab-typing`: the only thing shared with it is the atomic document-write RECIPE that
`tools/watermark_library.rs` reimplements, because that crate's `panel/doc_store.rs` is
crate-private and unreachable from here.

## Architecture
`CleaningTabState` owns a dedicated `CanvasView`, the active `CleaningTool`, cleaning UI state,
and optional shared models injected by `MangaApp`. The tab routes pointer, keyboard, wheel,
and overlay-window events into the active tool. Tools edit canvas overlay scratch state and
commit through `CanvasView`, which synchronizes committed pages into `CleanOverlaysModel`
and its diff-based undo/redo history.

Text mask data flows from `TextMaskModel` when available, or from `text_detection/` files through
a background load job. The tab uploads mask tiles to egui textures and exposes them to canvas via
`CanvasHooks::draw_canvas_mask_overlay_on_page`. Those mask textures are a reconstructable display
cache with memory snapshots and eviction; the underlying mask data stays in `TextMaskModel` or on
disk.

Quick text cleanup builds per-page jobs from source pages plus text masks, runs page processing in
workers, and applies prepared `ColorImage` patches into `CleanOverlaysModel` as results arrive.
Save operations collect overlay snapshots from the shared model and write `clean_layers/` in a
worker thread.

This tab HOSTS the shared panel dock (`crates/ms-widgets/src/panel_dock`), and every floating surface it has
is a dock tab. It declares SEVEN: the canvas' own «Лента» (`canvas::CANVAS_RIBBON_TAB`, body
`CanvasView::draw_ribbon_tab_body`, declared through `canvas::declare_ribbon_tab` — the canvas' one
declaration of it) plus six of its own — «Клин» (`cleaning.clean`: layer visibility, clear/save,
the quick-clean toggle and the save status), «Инструменты клина» (`cleaning.tools`: the tool picker,
rows wrapping to the panel width), «Выбранный инструмент» (`cleaning.active_tool`:
`CleaningTool::draw_ui`), «Быстрый клин найденного текста» (`cleaning.quick_clean`: the
quick-clean parameters, its two run buttons and its progress), «Редактор области»
(`cleaning.area_editor`: `CleaningTool::draw_main_panel`, the MAIN interface of a tool that edits a
region on the canvas — for «ИИ-редактор области» that is the SELECTED engine's own parameter panel
plus the run/apply/cancel row and the frame's status line) and «Библиотека знаков»
(`cleaning.watermark_library`: `CleaningTool::draw_library_panel`, a SECOND tool-owned panel — for
the watermark tool that is the management screen of its on-disk library). Its default arrangement
is `cleaning_default_dock_layout` — seven panels,
handed to the dock both by `app.rs::restore_panel_dock` and by `ensure_default_layout`. A tab body cannot mutate the tab: the
dock runs inside `CanvasView::draw`, so a body only raises a flag on `CleaningDockOut` and
`CleaningTabState::apply_dock_out` performs every mutation after that call returns, in the order the
three floating surfaces these tabs replaced performed theirs. The dock state is NOT owned here: it is
app-owned (`MangaApp::panel_dock`, one per studio window) and lent in for the frame through
`CleaningDrawParams::panel_dock`, which `tab.rs` passes on to `CanvasDrawParams`. The dock runs in
`CleaningHooks::draw_canvas_overlay_top_left` — inside `CanvasView::draw`, so a «Лента» edit still
lands before `publish_canvas_settings` — and it must run on EVERY frame this tab is active,
because the dock's detached OS windows are immediate viewports that only exist while
`PanelDock::end` shows them. The panels it drew are collected into `CleaningHooks::dock_panel_rects`
and folded into `panel_rects` after `canvas.draw` returns (the tab clears that list there), or the
active tool would paint under a panel. `PanelDockOutput::drawn_panels` reports MAIN-WINDOW panels
only, so a panel the user detached into a sub-window cannot enter that list — its rect is in that
window's own frame and would blank out this window's top-left corner.

Long-running AI, image processing, mask loading, and save work runs on worker threads.
The GUI thread polls job receivers and applies already prepared results.
AI-backed tools receive backend health/Torch availability from the tab, then run model checks and
backend requests inside tool worker paths. App-managed inpaint weights must be resolved through
`ms_sysprobe::ai_models` before calling Python backend endpoints.

## Files and submodules
- `tab.rs`: tab state, canvas orchestration, the dock tabs and their default arrangement, mask
  loading, save jobs, quick text-clean job orchestration, and history hotkeys.
- `autoclean.rs`: quick text-clean image engine. GUI-free core (`run_autoclean_engine`)
  clusters the text mask, then per cluster runs: `has_text_structure` gate -> two candidates
  (A = strokes via `fill_holes`+dilate, B = detector-box union / cluster bbox) ->
  `evolve_mask_to_homogeneous` on both in parallel (`rayon::join`) -> coverage/area selection
  -> universal `clip_fill_to_bubble_interior` -> conditional background-only padding ->
  `final_sanity_trim`. The thin `autoclean_page` wrapper is the only egui-touching part; it
  rasterizes the winning `RegionFill`s into the overlay patch. Includes synthetic pipeline and
  characterization tests. Detector boxes arrive from `tab.rs` already in page-pixel space.
- `watermark_chapter.rs`: GUI-free chapter-level watermark decomposition engine — the exact,
  AI-free counterpart of the neural watermark path. A semi-transparent mark composites as
  `I = c + s*B` (`c = alpha*W`, `s = 1 - alpha`), constant across every occurrence of one mark, so
  observing it over different backgrounds determines `c`/`s` and removal is the division
  `B = (I - c)/s`. Stages, all per `WatermarkKind` (a chapter may carry several distinct marks,
  and two of them may share their artwork pixel for pixel): `validate_calibration_sample` (ring
  flatness -> calibration target vs template-only) -> `estimate_model` (least squares over
  separated flat samples; Theil-Sen against per-pixel background estimates; otherwise the graded
  deposit-exact fit) -> `discover_anchors` (the anchor SET, coarse pyramid scan then full-res
  refinement) -> `find_occurrences` / `scan_page` / `scan_chapter` (anchor-band NCC, then the
  per-pixel-background gain test) -> `remove_occurrence` / `remove_occurrences_on_page`.
  `refit_with_refined_backgrounds` is the estimated-background refinement loop, with a fixed,
  named iteration count. `trimmed_footprint_from_model` / `trimmed_footprint_from_template`
  MEASURE where the mark actually ends inside a footprint (see the footprint contract below). Design and the measurements it rests on:
  `dev-docs/watermark_chapter_decomposition_plan.md`. Consumed by the «По главе (точное
  вычитание)» mode of `tools/watermark_removal.rs`; `mod.rs` keeps an `allow(dead_code)` for the
  refinement surface the tool deliberately does not use (see the comment there).
- `tools/`: cleaning tool trait — including the five additive, defaulted methods a tool that owns
  more than «Выбранный инструмент» uses (`set_panel_rects`, fed from `panel_rects` after
  `canvas.draw`; and two visibility/body PAIRS — `wants_main_panel` / `draw_main_panel` for
  «Редактор области» and `wants_library_panel` / `draw_library_panel` for «Библиотека знаков»,
  both bodies bound by the same "a body may not mutate the tab" rule as
  `draw_ui`) — brush/region-edit bases, the on-canvas region frame (`tools/region_edit_v2/`) and
  its only consumer `tools/ai_editor/`, which HOSTS the AI engines (FLUX.2 klein is the first) and
  splits their UI across those two tabs, local fill tools, stamp tool, the on-canvas patch tool
  (`tools/patch/`, gradient-domain seamless cloning), AI-backed
  inpaint tools, and the watermark tool that hosts the chapter-decomposition UI plus its on-disk
  watermark library, the library management panel and the reference-crop intake that builds an
  entry from the mark supplied on two known uniform backgrounds. See `tools/MODULE_README.md`.
- `mod.rs`: module wiring and public re-export of `CleaningTabState`.

## Contracts and invariants
- The cleaning tab uses shared clean-overlay visibility from `CleanOverlaysModel`; typing
  tab visibility toggles must not change this state.
- Tool operations must not block the GUI thread. CPU-heavy or AI-backed work must use
  background jobs and report explicit errors.
- App-managed cleaning/inpaint model checks and downloads must stay inside tool worker paths
  and go through `ai_models.rs` before Python backend requests.
- Overlay edits must validate page index, dimensions, and region bounds before mutating
  shared state.
- Shared model locks must be short-lived and released before image processing or file I/O.
- Text-mask overlays are display state only until quick-clean applies explicit overlay patches.
- Watermark decomposition never emits a model it cannot justify, and `ModelConditioning` is a
  GRADED verdict rather than a binary one. With all calibration samples on one exactly known
  background level the deposit `D = B - I` is still measured exactly, so a model IS produced and
  removal at that level is exact; only the alpha scale is an assumption, and the verdict carries
  the levels, their spread and an `AlphaUncertainty` (percent plus the LSB cost, including on dark
  backgrounds) together with the sample that would collapse it. `estimate_model` refuses — no
  model, and `WatermarkKind::refit` drops any previous one — only when not even the deposit was
  measured. That invariant has exactly ONE named exception, and it is a downgrade rather than a
  loophole: a background level the USER asserts (`SampleBackground::Manual`, for an occurrence
  whose ring the engine refused to call flat) feeds the fit exactly like a measured one, and the
  model pays for it in typed provenance — `AlphaSource::ManualBackgrounds`, its uncertainty
  floored at the no-information figure, and the count in `ModelProvenance::manual_backgrounds`.
  Nothing built on such a model may say the imprint was measured. The automatic flatness test is
  NOT weakened by it: an assertion is only ever consulted where the measurement refused, and it
  is stated, never derived. Identity stays measurement-only — `MarkSignature::from_flat_sample`
  ignores an asserted level, because one wrong claim must not be able to redirect auto-match
  onto the wrong entry — and `refit_with_refined_backgrounds` leaves an asserted level alone for
  the same reason it leaves a measured one alone: overwriting it would replace the user's
  instruction with a guess.
  The ring a level is measured from is CLIPPED to the page, and a truncated ring is a valid
  measurement rather than a refusal: `validate_calibration_sample` admits a sample on a PIXEL
  COUNT (`SampleParams::min_ring_pixels`), never on a per-side margin, so a mark stamped flush
  against the image border — which is where real marks sit — is measured from the sides that
  exist. Three things hold it honest. The flatness thresholds are NOT relaxed to pay for it: a
  ring with real structure is still `TemplateOnly` whatever its coverage, which is what keeps a
  one-sided ring from averaging one end of a gradient into a level the mark never sat on.
  The coverage travels with the measurement (`RingCoverage` on the verdict and on
  `SampleBackground::Flat`) and is counted out of a fit in `ModelProvenance::partial_rings`, so
  no report can call such a measurement complete. And NO numeric penalty is attached to it: the
  alpha uncertainty is calibrated on `FIT_NOISE_LSB`, documented as a per-occurrence
  rasterization bias that does not average down with sample count, and nothing in `dev-docs/`
  measures how a ring's pixel count moves it — inventing a figure would be exactly the
  fabricated number the plan's "honest reporting" section forbids. A partial ring is therefore
  REPORTED, not priced. It is also categorically NOT the `Manual` case: the level was measured.
  Its `c`/`s` are per pixel PER CHANNEL: per channel is mandatory for `c`, while alpha
  measured channel-neutral on both chapters and the graded fit deliberately ties the channels
  together. Removal is licensed only for occurrences the gain test verified: a correlation-only
  accept is refused, because subtracting a mark that is not there injects an inverse mark.
- A mark's FOOTPRINT is the rectangle the mark actually deposits in plus
  `FOOTPRINT_TRIM_SAFETY_PX` of background — never the loose box a detector handed over. Outside
  the deposit the model is `c = 0`, `s = 1`: fully transparent, no information and no removable
  signal, yet every such pixel is stored twice in the planes AND demanded of every future
  reference crop, which is how a 319x236 footprint around a 127x34 mark made a 224x170 drag
  unusable and made the mark read as flush against an 800 px page's right edge.
  `trimmed_footprint_from_model` and `trimmed_footprint_from_template` measure the real extent:
  from `c`/`s` where the entry has a model, otherwise from the template against the mean of its
  own `RING_WIDTH_PX` inner frame. `mark_extent_from_template` is that same template measurement
  WITHOUT the safety border, and the two must not drift apart: the raw box is what says a crop
  CUT the mark (it reaches the crop's own border), while the grown one reaches that border for a
  mark merely near it and says nothing. Both err LARGE by construction — the thresholds sit at the
  8-bit quantization floor (`1/255` of alpha, one LSB of deviation), so a template whose padding
  is page content rather than flat background yields no trim at all instead of a cut mark. A
  footprint with NO mark in it is `WatermarkError::FlatTemplate`, never a zero-sized rect.
  The safety border is wider than `RING_WIDTH_PX`, which is what makes a second trim a no-op
  rather than a slow erosion, and wider than `MAX_SUBPIXEL_SHIFT`. It is also the slack a
  reference intake registers within: the extent and the gradient alignment are two independent
  measurements of where one mark is, and on the measured reference pair they disagree by 3 px
  because the mark is a glyph with a soft glow — the glyph is what deviates against white, the
  glow is what deviates against black, so the 1-LSB extent measures a different part of the same
  mark on each background, and the GRADIENT is the one that is right. Whoever MOVES a footprint owes
  the anchors: an anchor is the page COLUMN of the footprint's left edge, so a trim shifts every
  anchor by exactly the horizontal offset and by nothing else (the anchor set is columns only),
  and an occurrence then lands at the same absolute page pixels as before the trim. There is
  exactly ONE rule about who may derive a footprint, and it is about EVIDENCE, not about which
  screen a crop came from: material with no stored geometry behind it takes the footprint its own
  marks measure, material with stored geometry keeps it — and may re-derive it only through the
  measured trim above, which owes the anchors. A rectangle a user dragged, or a detector boxed, is
  a CEILING on that measurement and never its shape. `tools/watermark_entry.rs` is where both ends
  of that rule live.
- Watermark KIND identity is `MarkSignature` (deposit chroma plus opacity gain), never the
  template's shape: a colour mark and its greyscale twin can be pixel-identical in shape and
  still need different `c`/`s`. A catalog must resolve a new sample with `find_matching_kind`,
  and the same rule governs matching an open chapter against the on-disk library — with the
  footprint required to agree on top of it, because `c`/`s` are per pixel.
- A mark's anchor is a SET of columns discovered from the data (`discover_anchors` ->
  `MarkTemplate::set_anchors`), not the one column the picked sample sat at, and anything that
  keys or persists a model must include `MarkTemplate::anchor_key`. The accept rule additionally
  requires the occurrence to sit within `ANCHOR_TOLERANCE_PX` of an anchor and to reach
  `FALSE_ACCEPT_GAIN_FLOOR`; no `DetectionParams` value can widen past either.
- Text-mask GPU cache eviction must not mutate `TextMaskModel`, loaded mask data, quick-clean jobs,
  or committed clean-overlay edits.
- Canvas zoom, drag-scroll, and context menus must respect active tool capture/blocking signals.
- `panel_rects` is a SAME-FRAME list, cleared once per frame after `canvas.draw`. Every floating
  surface that may swallow a tool click has to be in it, and they are all dock panels now, which is
  why their rects are carried out of the hook rather than pushed straight into the field. A rect is
  never pushed into it directly: it arrives through `PanelDockOutput::drawn_panels`, which answers
  for the MAIN window alone.
- «Лента» holds the LEFT viewport edge, «Библиотека знаков» hangs UNDER it and «Редактор области»
  under THAT. Not beside the ribbon: it is itself `ViewportEdge::Left`, so anchoring a panel to
  the ribbon's `Left` would place that panel outside the dock area and the solver's whole-chain
  translation would un-flush the ribbon. And not the other way round either — both tool panels are
  hidden for every tool but one, and a ribbon anchored to either would depend on a panel that
  usually is not there. A CHAIN rather than two children of the ribbon, because two panels sharing
  a `target` + `edge` + `align` solve to the very same rect and the model does not refuse it, so
  the second one would be buried. Dropping either for a frame is free: `remove_panel` hands a
  dropped panel's own anchor down, so a frame without the library panel gives «Редактор области»
  the ribbon's `Bottom` slot, nothing is anchored to the area editor at all, and the other panels
  land at exactly the rects they get without the tabs. Both took a fresh `PanelId` rather than a
  renumbering, so «Лента» keeps the id every canvas program tab gives it.
- Three of the seven tabs are CONDITIONAL and all three are declared on every frame, hidden
  through `.visible(..)`: the quick-clean tab follows `quick_text_mask_panel_open`, «Редактор
  области» follows the ACTIVE TOOL's `CleaningTool::wants_main_panel()` and «Библиотека знаков»
  its `CleaningTool::wants_library_panel()`, both re-asked every frame and never cached — a cached
  answer would keep the panel of the tool the app started with. Their `min_size` and
  `initial_size` are FIXED constants, not caption-derived ones, for the same reason as «Выбранный
  инструмент»: the body is opaque per-tool UI whose captions this tab cannot measure. The width of
  everything on the left chain is bounded by the ribbon's own width plus the dock gap, which is
  where «Клин» starts.
- The default dock layout is the DICTIONARY of this tab's dock tabs: a `TabId` missing from
  `cleaning_default_dock_layout` is dropped from the user's stored arrangement on every load, so
  adding a dock tab here means adding it to that builder too. It is this tab's OWN builder — every
  canvas program tab has one, there is no shared ribbon-only builder to fall back on — and
  it is registered under `AppTab::Cleaning.key()` in `app.rs::restore_panel_dock` as well; a builder
  wired in only one of the two places silently resets the stored arrangement.
- A dock tab body runs INSIDE `CanvasView::draw`. It edits the state its own widgets own — the
  active tool's UI mutates that tool, exactly as it did inside the tool window — but it may not
  perform, or invalidate the inputs of, anything the tab defers: the canvas' overlay edits, the job
  starters (`start_save_job`, `start_text_mask_load_job_if_needed`, `start_quick_text_clean_job`)
  and the tool switch (`activate_tool`) all need `&mut CleaningTabState` and would land
  mid-canvas-frame. Those are raised as flags on `CleaningDockOut` and run by `apply_dock_out`
  after the canvas draw returns, in the order the surfaces they came from ran them. The worker/job
  code itself never moves into a body. The same rule applies one level down, INSIDE a tool: the
  «Библиотека знаков» body may mutate the library panel's own state, but anything reaching the
  chapter catalog, the canvas or a worker leaves as a `LibraryPanelRequest` that the tool's
  `draw_overlay_ui` runs later in the same frame.
- Anything a tab body READS is polled BEFORE `canvas.draw`, not after it: `CleaningHooks` snapshots
  the save state and the active tool index when it is built, so `poll_save_job` and
  `ensure_active_tool_available` run at the top of `CleaningTabState::draw`. Polling after the draw
  showed the previous frame's answer for one frame — a spinner outliving its save, a tool that had
  just become unavailable still drawn selected — with nothing requesting the correcting repaint.
- The tool buttons' captions are resolved at DRAW time from `CleaningTool::title()`, never cached in
  the tab state: a cached caption keeps the language the app started in.
- Every tab whose width is caption-driven («Инструменты клина», «Клин», «Быстрый клин найденного
  текста») derives its `min_size` — and the last two their `initial_size` too — from the captions
  they are about to draw, per frame and therefore per locale. The dock never re-measures a tab's
  WIDTH (it stores the width the panel ASKED for), so a hardcoded width is permanent, and one sized
  for Russian opens the French panel on a horizontal scrollbar.
- `quick_text_mask_panel_open` is the ONE source of truth for the quick-clean tab: it is the tab's
  `.visible(..)` and it gates the canvas' text-mask overlay, so it must never be forked into a
  second flag. The tab is DECLARED on every frame regardless — a hidden tab keeps its slot in the
  layout and only its panel is skipped, while an undeclared one would be re-seeded a fresh panel on
  the next open. Its only affordance is the «Быстрый клин найденного текста» button in «Клин»,
  which is drawn `selected` while the tab is open: the `egui::Window` it replaced had a title-bar
  ✕, and a dock tab has no close affordance by design (a tab is only ever MOVED). The same rule
  governs «Библиотека знаков», whose one source of truth lives in the TOOL: its two «Библиотека
  знаков…» buttons toggle it and are drawn `selected` while it is open.

## Editing map
- To change top-level cleaning UI, save behavior, history, or quick-clean orchestration,
  edit `tab.rs`.
- To change which dock tabs this program tab declares or how big they start, edit
  `CleaningHooks::draw_canvas_overlay_top_left` in `tab.rs`; where their panels sit by default is
  `cleaning_default_dock_layout` in the same file, and both places have to agree. The «Лента» tab's
  own content, sizes, title and declaration (`canvas::declare_ribbon_tab`) live in `crates/ms-canvas/src/`.
- To change what a dock tab SHOWS, edit `draw_clean_tab_body` / `draw_tools_tab_body` /
  `draw_active_tool_tab_body` / `draw_quick_clean_tab_body` / `draw_area_editor_tab_body` /
  `draw_watermark_library_tab_body` in
  `tab.rs`; to change what a click
  there DOES, add a field to `CleaningDockOut` and apply it in `apply_dock_out`.
- To change how the tool buttons are laid out or how narrow the tool panel may get, edit
  `draw_tool_button_rows` (rows wrap automatically; a caption never wraps) and the pair
  `cleaning_tool_button_width` / `cleaning_tools_tab_min_width`.
- To change quick text-clean pixel classification, mask evolution (grow/shrink), candidate
  selection, bubble-interior clipping, or conditional padding, edit `autoclean.rs`; keep
  worker/job coordination, mask resize, and detector-box source->page scaling
  (`scale_blocks_source_to_page`) in `tab.rs`. The engine core must stay GUI-free; only the
  `autoclean_page` boundary and `paint_patch_from_mask` may touch egui.
- To change watermark decomposition — sample validation, the `c`/`s` fit, the conditioning
  verdict and its alpha uncertainty, mark identity, anchor discovery, detection thresholds or the
  removal/residual maths — edit `watermark_chapter.rs`. Its named constants carry their own
  rationale; change one only against the measurements in
  `dev-docs/watermark_chapter_decomposition_plan.md`, and note that the two chapters measured
  there disagree on several of them, so a constant is source-evidence, not a universal. The engine
  must stay GUI-free: the mark catalog, the region editor, the jobs, `CanvasView` patches, i18n
  and the watermark library belong to `tools/watermark_removal.rs`,
  `tools/watermark_library.rs`, `tools/watermark_entry.rs` and
  `tools/watermark_library_window.rs`. In particular, whether a set of samples separates the
  model is answered by `estimate_model`'s own verdict; no caller may re-derive it from a copied
  threshold.
- To change brush, stamp, inpaint, or fill behavior, edit the relevant file under `tools/`.
- To change text-mask loading or tiled mask drawing, start in `tab.rs` and check
  `TextMaskModel` contracts in `crates/ms-models/src/`.
- To change committed overlay mutation/history semantics, use `CanvasView` overlay APIs and
  `CleanOverlaysModel`; do not mutate shared overlay storage directly from tools.
