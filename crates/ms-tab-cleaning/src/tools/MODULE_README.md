# Module: crates/ms-tab-cleaning/src/tools

## Purpose
This directory contains the concrete tools used by the Cleaning tab and the shared bases that
connect tool input, preview state, region editing, mask editing, and final overlay commits.

## Architecture
`base.rs` defines the `CleaningTool` trait consumed by `CleaningTabState`. The tab owns tool
selection and dispatches canvas pointer events, wheel/key events, floating-window drawing, cursor
painting, and backend availability into the active tool.

Brush tools use `BrushToolBase` to draw into a local scratch overlay. Scratch previews are tiled
for large images and are committed to `CanvasView` only at stroke boundaries. Region tools use
`RegionEditToolBase` to select an overlay rectangle, load the composited source page plus current
clean overlay on a worker thread, show a detached region editor, and insert the accepted result
back into the page overlay. Mask-inpaint tools use `RegionMaskInpaintToolBase` to add editable
binary masks, optional sample masks, «Сгенерировать маску», and worker-thread run closures.

Mask generation itself is HOST-NEUTRAL and lives in `mask_generation.rs`, not in any host: it owns
the four sources — the text detectors ComicTextDetector, PaddleOCR and Surya (through the
translation module's typed helpers) and the watermark detector (`watermark.detect`, streamed) —
their availability rules, the detection worker, and the two shared controls. A host owns only the
`MaskGenerationState` it hands that module and the mask it writes the answer into. Both hosts read
it and neither may fork it: `RegionMaskInpaintToolBase` writes into its single editor mask, and
`ai_editor/` writes into the selected layer of its `RegionFrame` mask stack. Every mask-carrying
tool therefore gains watermark removal with a user-editable mask without a tool of its own, and a
new source or requirement rule reaches both hosts at once.

A tool whose mask is not the mask-inpaint one builds on `RegionEditToolBase` ALONE. That base
gives the selection, the loader, the editor window, zoom/scroll and Apply; everything the mask base
keeps in its private `RegionInpaintEditorState` — the run channel, the undo stack, the result
preview and the Escape handling — is then owned by the tool itself. `watermark_removal.rs` does
this because it has NO mask to paint: the network predicts its own.

The SELECTION GESTURE is Shift+ЛКМ, with ONE documented exception: while a tool has armed a
DIVERSION (`set_selection_diverted`), a plain ЛКМ drag starts the selection too. Arming is the
explicit statement of intent Shift stands for otherwise, and a plain drag is inert on a
region-edit tool (the canvas pans only with Space held, and the cleaning tab claims no
Shift-drag hook), so accepting it takes nothing from any other gesture. The pair
`wants_primary_stroke` / `selection_hint` must move together: the hint names the gesture
accepted IN THAT STATE, and a hint naming a gesture the tool refuses is the defect this rule
exists to prevent. A tool that never diverts keeps the Shift-only gate unchanged.

A finished drag that produces NO rectangle is REFUSED WITH A REASON, never dropped: `load_error`
holds it for `draw_ui_hint`, and `take_selection_refusal` is the one-shot drain a diverting tool
uses to repeat it on the surface it drew the user's attention onto. The same applies to a
selection the size limits refuse. Those limits (`min_selection_px`, `max_selection_area_px2`,
`max_selection_aspect`, enforced by `check_selection_limits`) have NO setter: `new` leaves all
three unset and only the in-module tests assign them, so no production tool is constrained by
them today; a tool that needs one must add a setter rather than assume it exists.

A size contract is NOT something `RegionEditToolBase` can guarantee: `snap_selection_end` clamps
to the page edge AFTER snapping to the multiple, and `build_composited_region_image` re-derives
the crop by ratio from the DECODED page, whose dimensions may differ from the overlay's. A tool
or engine with a hard size contract must therefore re-validate the loaded region's own size on
its run path, and both consumers do (`ai_editor::hand_region_to_engine`, `Flux2KleinEngine::start`).

The region LOADER is shared, not copied. `spawn_region_loader_thread` + `RegionLoadRequest` /
`RegionLoadResult` are `pub(super)` so any tool in this subtree can hand a page path, a source
rect and a clean-overlay chunk to one worker and get the composited region back off the GUI
thread; the decoded page is cached per worker and through `CleanOverlaysModel`. An owner MUST
send `None` and join the handle on drop, or the thread and its decoded page outlive the tool.

The region-editor WINDOW is not the only shape a region tool can take. `region_edit_v2/` is the
alternative: a selection FRAME that lives on the canvas for the whole editing session, with its
own handles, N mask layers, a result preview and a chrome of its own, driven from
`CleaningTool::draw_overlay_ui` instead of a floating window. It replaces the window flow for the
tools built on it; `RegionEditToolBase` and every tool on it stay exactly as they are. Its first
and so far only consumer is `ai_editor/`, which also carries the UI split the framework assumes:
a compact part in «Выбранный инструмент» (`draw_ui`) and a main part in its own dock panel
(`draw_main_panel`).

AI-backed tools (`aot.rs`, `flux_fill.rs`, and the engines under `ai_editor/engines/`) send region
and mask as raw PNG bytes in the IPC request blob (no base64). Every one of them encodes through
`region_png.rs`, the localized face of the one wire encoder `ms_tools::png_wire` (unmultiplied
RGBA8 region, L8 mask); no tool or engine owns a PNG encoder. They also ensure required app-managed models through `ai_models.rs`,
verify backend health, call the Python AI backend via `backend_ipc::shared_client()`, validate the
returned PNG size (from the response blob), and surface backend errors in the region editor status.
All backend transport goes through `crate::backend_ipc` (framed IPC over the AF_UNIX socket), in
three shapes. A method that answers once uses `shared_client().call(...)` — `aot`, the two
`inpaint.lama_*` methods, and every query/unload method of the other tools and engines. A method
that streams progress or preview frames uses `call_streaming` with a progress callback —
`inpaint.sdxl`, `inpaint.flux_fill`, the `watermark.*` run and the streaming detector behind mask
generation. A method that must also be STOPPABLE backend-side uses `begin_call` + `wait_streaming`
instead — `flux2_klein` only: `call_streaming` never exposes the request id `Client::cancel` needs,
so the id is what buys a real cancel rather than a detached answer (`flux2_klein/wire.rs`).

## Files and submodules
- `mod.rs`: module exports for the cleaning tab.
- `base.rs`: `CleaningTool`, stroke/cursor types, brush scratch pipeline, region editor pipeline,
  mask-inpaint editor and region loader worker. Of mask generation it owns only the host side —
  the `MaskGenerationState` it hands `mask_generation.rs` and the editor mask the answer lands in;
  the sources, the worker and the watermark plumbing are NOT here. It also owns the
  overlay<->scene POINT mapping (`scene_pos_to_overlay_pos` / `overlay_pos_to_scene_pos`) and
  re-exports the dense-overlay colour solver `overlay_pixel_for_final_color` from
  `crate::tools::overlay_pixel`, so the whole subtree keeps reaching it as
  `base::overlay_pixel_for_final_color`.
- `mask_generation.rs`: the host-neutral core of «Сгенерировать маску», shared by `base.rs` and
  `ai_editor/`. Owns the source catalog (`MaskSource`, `MASK_SOURCES`), the availability rule
  (`MaskSource::is_available` / `requires_torch`), the per-host parameters and progress
  (`MaskGenerationParams`, `MaskGenerationState`), the worker lifecycle
  (`spawn_mask_generation` / `poll_mask_generation`, answering with a validated 0/255
  `GeneratedMask` in REGION pixels), the streaming `watermark.detect` call with its model catalog
  and `watermark.status` query, and the two reusable controls (`draw_source_picker`,
  `draw_source_params`). GUI-free apart from the `draw_*`, `poll_*` and `spawn_*` helpers —
  everything else does a blocking IPC round trip and may only run on the worker.
- `zamazka.rs`: primary paint/erase/eyedropper/rectangle tool for direct clean-overlay edits.
- `stamp.rs`: copies pixels into clean overlays either from `project/alt_vers/<name>` or from the
  current page image/clean overlay using a Photoshop-like source point, with lazy background
  source-page loading where file decode is needed. Defaults to the current-page mode with
  «Исходник + клин» as the sampled layer. A background-loaded source page must match BOTH overlay
  dimensions; an expected dimension of `0` means the overlay size is not known yet, not "unchecked".
  In current-page mode the committed overlay is DENSE, not minimum-alpha: the solver is
  `base::overlay_pixel_for_final_color` (shared, not a stamp-local function), which solves the pixel
  that reproduces the desired final colour over the original page and raises its alpha to at least
  the brush dab coverage, so a 100 %-hardness dab commits a fully opaque patch.
  The minimum-alpha solution is exact only at 1:1 sampling — page and overlay are separate
  `TextureOptions::LINEAR` quads, so colour and alpha are filtered independently and a
  stencil-shaped overlay ghosts the page's own content back through it.
  Current-page mode shows two canvas markers: the fixed anchor beacon and a moving indicator of the
  point currently sampled, whose position is derived from the sampling mapping (`stamp_source_xy`)
  and therefore cannot disagree with the pixels being copied. The stroke origin is runtime stroke
  state, so between strokes the offset is zero and the moving marker rests on the beacon.
- `gradient.rs`: local mask fill using Lab scanline estimation and smoothing. Its screened-Poisson
  L-channel consolidation calls the project's SHARED red-black SOR kernel
  `crate::tools::red_black_sor_sweeps`; the other consumer is `crate::tools::patch::membrane`.
- `texture_synthesis.rs`: local inpaint through the `texture-synthesis` crate, with optional
  sample mask limiting the texture source area.
- `flux_fill.rs`: FLUX.1-Fill-dev tool (IPC methods `inpaint.flux_fill` streaming, `.unload`,
  `.status`) with two modes — `object_removal` (default) and `inpaint`. The GGUF quant (catalog from
  `.status`, with a ✓/«скачать» hint) and diffusers components are downloaded on demand by the
  backend into `side_models/`; the streamed `progress` frames carry a `phase` (`download` bytes /
  `generate` steps) + `label`, rendered as a single progress bar over a collapsible "Параметры
  (FLUX.1 Fill)" section (collapsed by default; default mode = object removal). Poisson seam
  matching is a toggle. Settings persist to `flux_fill_inpaint_settings.json` (see
  `config::flux_fill_inpaint_settings_path`) on background threads.
- `watermark_removal.rs`: the standalone «Удаление водяных знаков» tool. Built on
  `RegionEditToolBase` alone — nothing here needs a painted mask — and it hosts THREE modes,
  picked in the editor window above the parameter sections:
  - `mask_only` (default, streams `watermark.detect`, draws the predicted mask over the region and
    leaves pixels untouched — Apply is then a visual no-op);
  - `clean` (streams `watermark.remove`, replaces the region with the network's reconstruction).
    EXPLICITLY experimental and says so in the UI: on manhwa line art it softens strokes and leaves
    residue, which is why the mask-first flow is the default
    (`dev-docs/watermark_removal_plan.md` §1.2, §7.4);
  - `chapter` — «По главе (точное вычитание)»: no backend, no Torch, no weights. The catalog of
    marks, the calibration samples, the chapter scan, the apply and the reports around
    `../watermark_chapter.rs`, plus the on-disk library in `watermark_library.rs`. Its editing UI
    lives inside the SAME region-editor window; only the library MANAGEMENT screen is a surface
    of its own, and that one is a dock tab (`watermark_library_window.rs`).
  `pytorch_required()` is a CONSTANT `false` here: the chapter mode needs nothing, so the picker
  button must never be AI-gated, and that answer also feeds `ensure_active_tool_available` every
  frame. The Torch gate is inside the tool instead — the two network entries of the mode picker
  and the run button, both worded from the same keys as `mask_generation.rs` and reading the same
  flag the tab pushes through `set_ai_backend_torch_available` (unknown resolves to available).
  `WatermarkMode::requires_torch` is what those two gates ask; nothing else consumes it.
  The tool also owns the library panel's reach into the chapter and the canvas: the library
  SELECTION of a card is chapter catalog MEMBERSHIP (a mark whose `library_entry` names that
  entry), never `pinned_entry`, which answers the different question of whose calibration
  supplies an already-discovered mark; and `library_arm` — an ENTRY ID, because an entry is the
  only thing a selection can be reserved for — reserves the NEXT completed canvas
  selection for the library, DIVERTED away from the region editor by the base
  (`set_selection_diverted`, kept in step with the reservation by `sync_selection_diversion`)
  and cut out by
  `run_library_capture` on a lane of its own, so the CUT-OUT never queues behind an icon batch
  or a chapter scan. An armed drag therefore spawns no loader job, shows no loading window and
  opens no editor — and the canvas says so, because `draw_cursor` tints the crosshair and the
  rubber band with the cancel button's own red while a reservation is in force. Writing the crop does share the panel's single channel, and is therefore
  QUEUED there rather than refused (`WatermarkLibraryWindow::deferred`): a capture is never
  lost. `next_library_arm` is the reducer that keeps exactly one reservation live, and
  `arm_survives` is the rule that ENDS one — a reservation may not outlive the screen that
  draws its cancel button, so the tool drops it when the panel is hidden, when the armed entry
  disappears from the listing, and when the user walks to another screen. Every one of those
  endings is REPORTED (`drop_reservation_without_a_screen`), as is a parked rect that reaches
  the tool with no reservation left to claim it (`take_reservation_for_diverted`) and a drag
  that produced no rectangle at all (`report_selection_refusal`): a gesture dropped in silence
  is indistinguishable from one that did nothing.
  While a reservation is live the canvas ALSO accepts a plain ЛКМ drag, and the panel says so —
  `library_arm_gesture_hint` is drawn beside the cancel button, because the only other place
  that names the gesture (`cleaning.region.selection_hint`) lives in a different dock tab from
  the one the user is reading while arming.
  A capture is also refused outright when the ring could not be MEASURED at all: no stated
  level can license such a crop, so it must not be offered a colour control.
  A capture is NOT refused for sitting against the page border. `grow_page_rect` takes the ring
  the page can give on each side and reports the real per-side margins
  (`GrownCrop::margins` -> `CapturedCrop::margins` -> `ReferenceIntakeRequest::crop_margins`), so
  the intake reads the asymmetry as the GEOMETRY the page forced instead of assuming a symmetric
  inset the crop does not have. Those margins say what was clipped; they never say which rectangle
  the entry's footprint should be (see the footprint contract below). The admission floor stays the engine's own `min_ring_pixels`, a pixel COUNT; the
  partialness is recorded with the measurement (see `../MODULE_README.md`) and never treated as
  an assertion. A full-margin demand on all four sides would make a mark stamped flush against
  the image edge permanently unsamplable, and the user's marks sit there.
  Membership has THREE states, not two (`chapter_library_selection`, `chapter_saved_entries`,
  `chapter_entry_index`): an entry can also be in the chapter because a DISCOVERED mark was
  saved to it, keeping its `mark-{n}` kind id — such an entry is neither loadable (a second
  kind for one physical mark) nor removable (the scan paid for its measurements).
  `WatermarkMode` is the three-way user selection; `WatermarkNetworkMode` is the two-way one that
  reaches the backend, so "which IPC method does the chapter mode call" cannot be asked. The model
  catalog, the ✓/«скачать» hint, the status query, the progress bar and the `CallError` mapping are
  REUSED from `base.rs` (`pub(super)` items), never duplicated. Settings persist to
  `watermark_removal_settings.json` (see `config::watermark_removal_settings_path`) on background
  threads; the chapter parameters in that file are normalized by the ENGINE's own
  `DetectionParams`/`SampleParams`, so the file and the values used cannot disagree.
- `watermark_library.rs`: the reusable library of measured watermarks under
  `config::watermark_library_dir()` — one self-contained directory per entry (`entry.json`,
  `template.png`, `planes/c.png` + `planes/s.png` as 16-bit PNGs, `samples/NNN.png`), so an entry
  can be copied or shared as a folder. An EMPTY entry is `entry.json` alone: no template, zero
  footprint, no samples, no planes, and `validate_entry_dir` refuses any mixture of the two
  (a footprint or a sample with no template behind it describes geometry no image backs). Pure I/O and serde: engine types never cross its boundary,
  `watermark_entry.rs` maps between them. The document's format version is the LOWEST one that
  can express it (`required_format`): an entry whose backgrounds were all MEASURED stays a
  format-1 document an older build can read, one carrying a hand-ASSERTED level
  (`StoredSampleBackground::Manual`) is format 2 and one with NO TEMPLATE is format 3, both of
  which an older build refuses outright — its
  tagged `background` enum has no catch-all, so an unknown `kind` is a parse failure, and that
  refusal is the point: the alternative was reading a claim as a measurement. The version is read
  from the UNTYPED document before it is interpreted, so a newer entry is named as a version
  problem instead of surfacing as a JSON error. `load_entry_planes` reads the fitted planes back
  without decoding a single crop — the cheap source for a per-entry preview. `(source key, page width, anchor key, variant id)` is
  SEARCH METADATA inside an entry, not its storage key — one entry may legitimately serve several
  sources — and matching an open chapter to an entry goes through `MarkSignature` /
  `find_matching_kind` first. It also owns the INTERCHANGE boundary: `export_entry_zip` /
  `export_entry_dir` write exactly the members `entry.json` declares, and `import_entry` stages an
  incoming entry beside the library, runs `validate_entry_dir` on it, and only then renames it
  into place under a free id.
- `watermark_entry.rs`: the bridge between the engine and the library. It owns the literal wire
  tags of a verdict / fit method / alpha source and their inverse (`stored_calibration`,
  `conditioning_from_stored`), the one mapping between the stored and engine background enums
  (`engine_background` — the only place the measured/asserted distinction may be resolved), the
  REFERENCE-CROP INTAKE (`run_reference_intake`, which accepts a crop whose ring is not flat if
  and only if `manual_levels` states its background, and whose refusals carry a
  `ReferenceRefusal` tag beside the message so a UI can add the advice only it can give) and the auto-match ranking
  (`rank_library_candidates`, `candidate_improves`; a fully measured entry outranks one resting
  on an assertion whenever the verdict cannot separate them). It also produces the LIBRARY
  CARD's material: `render_mark_on_white` (the mark composited on white as a plain `RgbaImage`,
  `Ok(None)` for an entry with no model, built from the stored planes rather than a refit) and
  `entry_warnings` (the typed `EntryWarnings` a card warns from, never a pre-formatted string —
  localization stays in the UI layer). It also owns `empty_entry_request` — the write that
  creates an entry with a name, an id and nothing else, which is what «+ новый» makes — and
  `CropMargins`, the per-side background a cutter states when the image border stopped it from
  leaving the symmetric margin. It owns the JOINT FOOTPRINT rule (`measure_mark_extents`,
  `joint_footprint`, `centre_on_extent`, `centre_in_stated_rect`, `crop_holds`, `AlignSearch`) —
  see the contracts below. And it owns the FOOTPRINT TRIM: `trim_entry_footprint` re-derives
  a stored entry's footprint from the mark it actually holds (the engine measures the extent), and
  hands back the rewrite that replaces it — the template and every stored crop re-cropped, every
  anchor shifted by the trim's horizontal offset, and the `c`/`s` planes REFITTED from the
  re-cropped crops rather than cropped themselves, so the planes can never describe a geometry the
  samples do not have. Its three outcomes are typed (`FootprintTrimOutcome`: `NoMark` for an empty
  entry, `AlreadyTight`, `Trimmed`), because "nothing to do" is a state and not a failure. Its
  measuring half is `trim_entry_geometry` (`EntryTrim`), which the reference intake calls directly
  so a trim forced by an incoming crop and that crop's own write are one operation.
  GUI-free.
- `watermark_library_window.rs`: the library management screen — the BODY of the «Библиотека
  знаков» dock tab, supplied by the tool through `CleaningTool::draw_library_panel`. Every
  floating surface of the cleaning tab is a dock tab, so it owns no window, reports no rect and
  has no close affordance; the two «Библиотека знаков…» buttons of the tool TOGGLE it and its
  own `is_open` is the single source of truth behind the tab's `.visible(..)`. It has TWO
  screens (`LibraryPanelMode`): the CARD LIST, where each entry shows the mark composited on
  white, its verbatim name, its verdict with its warnings, and the two decisions a list is for —
  chapter membership and "open this one"; and ONE ENTRY, which owns the name editor, the stored
  calibration samples, the crops captured for it but not yet written, and every per-entry action
  (add a level, export, delete). There are only those two: «+ новый» CREATES the entry on disk
  at once (`EntryAction::CreateEmpty` -> `empty_entry_request` -> `LibraryEvent::Created`) and
  opens its screen, so there is no nameless draft a crop could be aimed at and no
  two-crops-upfront requirement on that path. Every job runs on a worker; `poll` is called from the tool's `draw_overlay_ui`, NOT from
  the body, because a hidden panel draws nothing and a body-side poll would strand a job that
  finished after the user toggled the panel shut.
  The body may not touch the chapter catalog or the canvas: the tool hands it a read-only
  `LibraryPanelContext` (measurement settings, which entries the chapter is hunting, the live
  canvas reservation) and the body answers with at most one `LibraryPanelRequest`, consumed by
  `draw_overlay_ui` in the SAME frame. Two contracts live here rather than in the drawing:
  `entry_verdict_line` is pure and refuses to describe an entry resting on a hand-stated
  background as measured, and `pending_ready` refuses to commit a captured crop whose level the
  user has not actually stated — the colour control's starting value is not an answer. The
  first rule binds the REGION EDITOR too: `describe_conditioning` in `watermark_removal.rs`
  takes the same assertion flag (read off `ModelProvenance::manual_backgrounds`) and switches
  to the same `_asserted` wording, so the card and the editor cannot disagree about one entry.
  `entry_verdict_line` matches `ModelConditioning` EXHAUSTIVELY: its fallback means "a stored
  tag this build cannot map", never "a verdict this build renders elsewhere".
  A stored crop carries its own two-click delete. That is a REFIT plus a full rewrite
  (`watermark_entry::drop_entry_sample`), never a file deletion, and the arm is an index into
  the row list on screen, so it dies with that list. Dropping to one crop is a legitimate
  degrade to `deposit_exact`; dropping the last one leaves a template-only entry and says so
  before the second click.
  EVERY ARMED SELECTION IS ANSWERED VISIBLY, and the answer is attached to the ENTRY the drag
  was aimed at. That guarantee has two halves, because a refusal can happen before a crop
  exists. The LATE half is the pending row: `accept_captured_crop` pushes one for every crop —
  including the one it commits straight away because its ring measured flat — the row leaves
  only when a worker CONFIRMS the entry was written (`committing` / `drop_committed_pending`),
  and a refused intake keeps the row, the scratch file (the user's only copy of that selection)
  and the REASON, written onto it by `mark_committing_refused`. The EARLY half is
  `reject_capture`: every early return of `start_library_capture` and every error of
  `run_library_capture` lands there as a `CaptureRefusal` row on the armed entry, drawn in the
  same red block with the same advice, plus a `log_warn` — a lane that logged nothing is what
  made this class of failure undiagnosable from a log the user sends, which is itself the defect
  and not a detail. `ReferenceRefusal` is what makes a reason actionable — `run_reference_intake`
  and `LibraryCaptureRefusal` both tag one `TooSmall` / `Misaligned` / `Background` / `Other`,
  and `refusal_advice` turns the first three into the sentence that says which drag would have
  worked. Anything recorded outside the draw also sets `dirty`, and the tool drains it through
  `request_repaint_if_dirty` at the END of `draw_overlay_ui`: the dock body runs inside
  `CanvasView::draw`, so a row or a status recorded after it has no frame to appear in, and
  after a finished drag there may be no next input event at all. The panel-wide status is drawn
  by `draw_status` INSIDE each screen's scroll area next to the control the user just used,
  never as a grey `ui.small` above it.
  SEPARABILITY is a rule about BUILDING an entry, not about improving one: `run_reference_intake`
  refuses a non-separable crop set only when `base.is_none()`. The store already holds
  one-sample, non-separable entries on purpose (`drop_entry_sample` writes exactly that, verdict
  `deposit_exact`), so an existing entry's FIRST crop — necessarily one level — must be accepted
  or a zero-sample entry could never take one. The verdict is then reported as whatever the
  engine reached, never as a separability the data does not support.
  Card icons are cached per entry AND per `EntrySummary::updated_unix`, so an entry whose model
  changed gets a fresh icon instead of the mark it used to have. An EMPTY entry has neither a
  model nor a template, so `icon_plan` answers `IconPlan::Empty` and the card says «знака ещё
  нет» in the icon slot instead of inventing a picture; its «Выбрать» control is disabled too,
  because there is no template to correlate a chapter against.
- `aot.rs`: AOT backend inpaint and `inpaint.aot` IPC calls.
- `region_png.rs`: the cleaning face of `ms_tools::png_wire` — `encode_color_image_png_rgba`,
  `encode_mask_png_luma` (`alpha > 0 -> 255`), `encode_mask_png_l8` and `decode_mask_png`
  (response mask PNG -> 0/255 alpha), mapping the typed `PngWireError` / `PngDecodeError` /
  `MaskError` onto the `cleaning.png.*` / `cleaning.inpaint.size_mismatch_error` texts. The
  only PNG codec of backend payloads in this crate (the watermark sources included); the wire bytes are pinned once,
  in the `ms_tools::png_wire` tests, and `region_png.rs` tests its error mapping and threshold.
- `patch/`: this tab's HOST for the «Заплатка» tool — Photoshop's Patch Tool. A free-form or
  rectangular selection drawn straight onto the page canvas, dragged onto a clean source area; the
  copied pixels are colour-adapted to the destination's contour by a gradient-domain (Poisson)
  membrane and committed into the clean overlay as one undo step. The tool ITSELF — selection,
  gesture, ROI/refusal geometry, the membrane solve and the outline painting — lives in
  `crate::tools::patch`; what is here is the `CleaningTool` impl, the `PatchHost` impl (canvas
  geometry, the two region loads, the store step) and the tab registration. Built DIRECTLY on
  `CleaningTool` (a lasso has no radius, no hardness and no axis-aligned scratch rect, so
  `BrushToolBase` fits none of it), it rides the tab's ordinary stroke pipeline. Own
  `MODULE_README.md`.
- `region_edit_test.rs`: development-only mask-inpaint pipeline test tool; it is not exported by
  `mod.rs`.
- `region_edit_v2/`: the on-canvas region-editing FRAMEWORK (frame, mask layers, geometry,
  painting, input). It has its own `MODULE_README.md`; a tool built on it drives `RegionFrame`
  from `draw_overlay_ui` and must not duplicate the helpers `base.rs` already exposes.
- `ai_editor/`: the «ИИ-редактор области» tool — the framework's only consumer, and the only tool
  with a MAIN dock panel (`wants_main_panel`). It HOSTS the AI engines behind the `AiEngine` trait
  (`ai_editor/engine.rs`, `ai_editor/engines/`), which is where FLUX.2 klein lives; a model is an
  engine of this tool and not a `CleaningTool` of its own. Own `MODULE_README.md`, and a second one under
  `engines/`.

## Contracts and invariants
- Tools must mutate clean overlays through `CanvasView` APIs such as `replace_overlay_region*` and
  `commit_overlay_page_to_model`; they must not write `CleanOverlaysModel` storage directly.
- Region, mask, and output image dimensions must match before processing or applying a result.
  Empty images or empty masks should return the original region or a clear user-facing error.
  The one deliberate exception is the area editor's FLUX.2 klein engine, where an empty PAINTED
  mask is a working mode rather than missing input: it is turned into a solid all-`255` wire mask
  before the request is built, so what reaches the backend is never an empty mask either. The rule
  above is about the WIRE, and it still holds without exception there.
- File decode, source-page loading, AI calls, model scans/downloads, and CPU-heavy inpaint must run
  off the GUI thread. GUI code may poll channels, update textures, and apply prepared patches.
- Shared model locks must be held only long enough to snapshot or apply data. Do not hold them
  while decoding images, running detectors, calling Python, or building textures.
- AI tools that require Torch must honor backend availability supplied by the tab and fail visibly
  when the backend or model is unavailable.
- Text-detector mask generation inside the region editor must use the typed detector helpers from
  the translation module and must treat returned masks as binary alpha data in region coordinates.
  The watermark source calls `watermark.detect` itself but obeys the same contract: the response
  blob is an L8 mask PNG at the region resolution, decoded through `region_png::decode_mask_png`
  (the `ms_tools::png_wire` decoder + the `ms_text_detect::mask` 0/255 normalization and guard).
- Watermark model ids (`slbr`/`wdnet`/`splitnet`) are wire values and the persisted selection
  identity, so they stay literals; only the display label is an i18n key resolved at render time
  (same split as `LamaModelSpec`). The catalog lives ONCE, in `base.rs`; the mask source and the
  standalone tool both read it from there.
- `watermark.remove` answers with `clean_png ++ mask_png` in a single response blob. `image_len`
  and `mask_len` from the response header must be validated with STRICT equality against the blob
  length before slicing, so a truncated or padded frame is rejected instead of sliced into garbage.
- Every request parameter the watermark tool sends is passed through
  `WatermarkRemovalSettings::normalized()` first: a hand-edited settings file cannot push an
  out-of-range tile, overlap, threshold, dilation, mode or model id onto the network, nor an
  out-of-range anchor tolerance, background radius or ring width onto the chapter engine.
- Chapter mode owns no maths. Sample validation, the `c`/`s` fit, the conditioning verdict, mark
  identity, anchor discovery, detection and removal all belong to `../watermark_chapter.rs`; this
  layer decodes pages, runs jobs, applies patches, persists and reports.
- Chapter jobs decode ONE page at a time (a chapter is several strips of ~700x18000) and take a
  COPY of the catalog; the GUI keeps the previous copy to draw and holds the catalog UI read-only
  until the worker hands its version back. `ChapterCatalog::kinds` and `::marks` are INDEX-ALIGNED,
  and a mark's crops are kept in LOCKSTEP with its kind's calibration samples — the engine does not
  hand sample pixels back and the library needs them.
- Catalog identity is `WatermarkKind::id` and lives in exactly one place; `ChapterMark` does not
  duplicate it. A new selection offered as a mark is resolved with `find_matching_kind`, never by
  comparing templates.
- Chapter removal is applied per page through `CanvasView::replace_overlay_region_px` on the GUI
  thread. Occurrences only correlation vouched for, and marks with no model, are COUNTED as refused
  and reported; they are never subtracted, because subtracting a mark that is not there injects an
  inverse one.
- Honest reporting is a contract, not a wording preference
  (`dev-docs/watermark_chapter_decomposition_plan.md`, corrections): the UI says the IMPRINT is
  measured exactly and never that «c точен»; the stated ±% bounds the alpha SCALE only; the
  exact/clipped shares are labelled a quantization-and-clipping report, and model quality is
  reported by the detection gain and the t-statistic instead.
- Library entries store the user-visible name VERBATIM (no trim, no normalization) and never an
  ESTIMATED per-pixel background, so folding a new chapter into an entry can never overwrite a
  measurement with an estimate. A sample's stored background is therefore either measured
  (`Flat`, with its ring std) or asserted by the user (`Manual`, with no ring std, because no
  ring was measured); the second forces format 2 and downgrades the entry's verdict through the
  engine, and no report may call such an entry measured. The calibration crops are the reconstruction
  source — a loaded entry is refitted from them — and the plane PNGs are an inspection and
  interchange artifact.
- Every library write is ATOMIC at the FILE level (sibling temp + `write_all` + `sync_all` +
  close + rename + directory fsync, the `tabs/typing/panel/doc_store.rs` recipe, which is not
  reachable from here) and at the DIRECTORY level: an entry is several files, so `save_entry`
  builds the whole new directory in a staging sibling and swaps it in at the end. A failed or
  interrupted rewrite therefore leaves the previous entry intact and valid instead of metadata
  naming crops a `samples/` wipe already removed. It also keeps two guards: a document of a NEWER schema is never overwritten, and a document changed
  since it was read is MERGED — the on-disk document is re-read at write time and its additive
  parts (creation time, source list, and every top-level field this build does not know) are
  carried forward. `rename_entry` is merge-only for exactly that reason.
- An entry's member paths are UNTRUSTED input once an entry can be imported: every relative path
  out of `entry.json` goes through `entry_member_path`, which refuses anything but plain
  components, and an imported archive's members are size-capped and counted.
- REFERENCE-CROP INTAKE never decides the spread question itself: it builds the kind, refits, and
  accepts only `ModelConditioning::Separable`. Two crops on one background — or on backgrounds
  too close together — are refused with the measured levels and the background
  `suggested_background()` names. Alignment correlates GRADIENT MAGNITUDE, because raw luma flips
  sign between a white-background crop and a black-background one.
- WHAT A REFERENCE CROP MUST PROVIDE is coverage of the MARK plus a measuring ring — never
  coverage of a rectangle some earlier drag or some detector box defined. Which of three rules
  gives the footprint is decided by whether anything already on disk binds the geometry, and by
  nothing else — not by which lane the crop arrived through, and not by its order in the list.
  * NO TEMPLATE YET (a brand-new entry, or an empty one taking its first crop): the footprint is
    derived from ALL of the crops at once, with no designated primary. Each crop's own mark extent
    is measured against its own frame (no registration needed), the footprint is the UNION of those
    extents plus one safety border, and every crop is then aligned starting from where its own
    extent sits. Never an intersection — that needs the registration it is meant to bootstrap and
    shrinks towards the crop showing LEAST of the mark. What it replaces is the rule that made the
    footprint the FIRST crop inset by a fixed margin: the margin cancelled, so every later crop had
    to be at least as large as the first in BOTH axes, and a pair where neither crop covers the
    other (the user's 259x258 and 226x266) could not be built in any order. It is capped by the
    first crop's own inset rule, so it can only ever make a footprint SMALLER, and it stands down
    entirely when a crop's extent spans a whole axis (a background that is not flat makes every
    pixel deviate), which is the engine's own self-limiting behaviour.
  * A STORED TEMPLATE: the entry keeps its footprint, because its anchors were measured against it
    — unless that rectangle is what BLOCKS a supplied crop, in which case the intake re-derives it
    from the mark inside the entry's own template before refusing anything. That is the same
    measurement, the same re-crop and the same anchor shift the manual «Обрезать отпечаток» button
    performs (`trim_entry_geometry`, shared by both), and it lands in the SAME atomic write as the
    crops being added, so the entry is never observable half-trimmed. It runs only when the stored
    rectangle blocks a crop — a footprint that works is never moved, because moving one silently is
    what the anchors cannot survive — and the write is reported to the user
    (`ReferenceIntakeOutcome::trimmed` -> `library_trimmed_status` on the saved line).
  * Once the footprint is MARK-SHAPED (jointly derived or trimmed), the per-side margin is gone
    with it: the window may sit flush against a crop's border and the ring is admitted by the
    engine's pixel COUNT, as it already is at a page border. A footprint the crop's own geometry
    states keeps the background it states on every side.
  Stated `CropMargins` do NOT veto any of this. They cap the joint footprint, they say which side
  may legitimately carry the mark right up to the crop's border (a stated `0` is a page border the
  cutter could not get past; anything larger is background it claims to have left, and an extent
  crossing into it means the crop CUT the mark), and they are the background a non-mark-shaped
  window must keep. Treating them as a footprint the intake must adopt is what kept the CANVAS lane
  — the one the user works in — on a drag-shaped footprint after the picker lane had stopped.
  Three refusals belong to this area, each naming what the user can act on: a crop holding no mark
  (tagged `Misaligned` — the rectangle was not put on the mark), a crop whose RAW extent runs past
  the background its side claims (tagged `TooSmall`, naming the SIDE: the crop cut the mark), and a
  crop the aligner placed further than `REFERENCE_REGISTRATION_DRIFT_PX` from its own extent — the
  last matters because a two-sample closed form is exactly determined and a model fitted over
  mis-registered planes still reports a confident verdict.
- A stored FOOTPRINT is the mark's own extent plus a safety border, not the chapter detector's
  box (the contract is in `../MODULE_README.md`). Re-deriving it stays an EXPLICIT per-entry action
  («Обрезать отпечаток», `EntryAction::TrimFootprint`) and is never an automatic rewrite on load:
  the rewrite swaps the whole entry directory, so the trimmed-away pixels are gone with no undo.
  The ONE place it happens without a click is the reference intake above, and only there because
  the alternative is refusing a crop that does cover the mark with a number the user cannot act on
  — the trim is bounded to that case, it is the engine's own measurement (which errs large and
  stands down on a background it cannot measure against), and the write says it happened.
- AUTO-MATCH is shape-independent (`MarkSignature`) and additionally requires the footprint to
  agree, because `c`/`s` are per pixel. That rule is why TRIMMING an entry takes it out of
  auto-match for a chapter that still measured the loose box — see `dev-docs/known_gaps.md`
  KG-012; the fix is to trim the chapter's own footprint too, never to loosen the filter. `ChapterMark::pinned_entry` is the user's explicit
  override and survives a rescan; `adopted_entry` is what is in effect and is rebuilt by every
  scan, which is why a scan releases adopted entries before pass 1 and re-applies them in pass
  1.5. The chapter's own crops are PARKED, never dropped, so an override is reversible.
- Tool registration is FOUR steps, none optional: `mod.rs` export, the `use` list in `tab.rs`, the
  `CleaningTabState::default` vector, and the matching index group array (`BRUSH_*`,
  `MASK_REMOVAL_*`, `AREA_EDIT_TOOL_INDICES`). An index missing from the group arrays is registered
  but never drawn.
- Mask generation is gated by the availability flags the tab pushes into the host: every source
  except PaddleOCR requires Torch (`MaskSource::requires_torch`), and the whole section requires a
  reachable backend. `MaskSource::is_available` in `mask_generation.rs` is the ONLY place that rule
  lives; a host that also blocks for a reason of its own (a busy worker, a locked frame) adds that
  reason beside it, never instead of it.
- **A unit test never starts a real detection.** `spawn_mask_generation` reaches the backend and,
  for the Torch sources, downloads model weights into `config::models_dir()` — which for a test
  binary is whatever directory it was launched from. A host that starts detections therefore holds
  the spawner in a field of type `MaskGenerationSpawner`, defaulted to `spawn_mask_generation`, and
  its tests install a stub instead of calling the host method that would spawn the real one. The
  default is always the real spawner: a test opts OUT, and no product code path changes.
- Tool pointer capture and zoom/scroll blocking are part of the canvas contract. An open region
  editor must block canvas zoom and capture pointer input inside its window.
- `CleaningTool` carries five additive, defaulted methods for tools that own something beyond
  «Выбранный инструмент»: `set_panel_rects` (pushed by the tab each frame after `canvas.draw` and
  before `draw_overlay_ui`, so a tool can cut the dock panels out of the viewport), plus two
  visibility/body PAIRS — `wants_main_panel` / `draw_main_panel` for «Редактор области» and
  `wants_library_panel` / `draw_library_panel` for «Библиотека знаков». They are separate pairs
  because the two tabs carry different captions and a tool may want either, both or neither; a
  second tab must never be squeezed into the first pair. Both bodies run inside `CanvasView::draw`
  and may therefore mutate only the tool itself — a button there raises a flag consumed by the
  next `draw_overlay_ui`, the `CleaningDockOut` rule at tool scope. Each `wants_*` answer is the
  tool's OWN single source of truth and is re-asked every frame; the tab never caches it and
  never keeps a second copy.
- `capture_overlay_chunk`, `extract_overlay_chunk`, `overlay_rect_to_scene_rect`,
  `scene_pointer_to_image_px`, `scene_pos_to_source_xy`, `scene_pos_to_overlay_pos`,
  `overlay_pos_to_scene_pos` and `overlay_pixel_for_final_color` are reachable as `pub(super)` items
  of `base.rs` (the last one re-exported from `crate::tools::overlay_pixel`): the whole `tools`
  subtree reuses them, and a copy of any of them in a tool is a defect.
  `scene_pos_to_overlay_pos` / `overlay_pos_to_scene_pos` are the FRACTIONAL point pair and exact
  inverses of each other; `scene_pos_to_source_xy` is the rounding, non-`Option` variant that takes
  an explicit page rect and source size instead of resolving them from the canvas.
- An on-canvas tool must NOT return `true` from `block_canvas_zoom()`: that flag also disables
  the clean-overlay undo shortcuts (`tab.rs`), which is acceptable for a modal editor window but
  not for a surface that lives on the canvas for a whole session. Block precisely instead
  (`captures_canvas_pointer` over the surface, drag-scroll only while a drag is live).
- A tool that drives its gesture through the TAB'S STROKE PIPELINE must also leave
  `captures_canvas_pointer()` `false`: the tab ORs that flag into `canvas_pointer_occluded` and
  then calls `finish_stroke()` and returns, so a capturing tool receives no stroke, key or cursor
  callback at all. `region_edit_v2` can capture only because it senses everything through its own
  `Area` and sets `wants_primary_stroke() = false`; `patch/` does the opposite and therefore pins
  BOTH flags to `false` with tests.
- A surface that must stay visible while the pointer is elsewhere is painted from
  `draw_overlay_ui` through `ctx.layer_painter(LayerId::new(Order::Middle, …))`, never from
  `draw_cursor`, which is pointer-gated. A bare layer painter registers no interactable `Area`, so
  it neither occludes canvas input nor trips the tab's z-order check.
- On-canvas geometry is stored in PAGE pixels and re-projected every frame. A stored screen
  rectangle drifts the moment the canvas scrolls or zooms — the same rule `region_edit_v2` states
  for its frame, and the reason `patch/` keeps its polygon in page pixels.
- Gradient-domain / Laplace maths belongs to ONE kernel: `crate::tools::red_black_sor_sweeps`.
  A tool wanting a harmonic or screened-Poisson solve builds `lam`/`denom`/`u0` around it
  (`lam = 0` inside the region, a large `lam` to pin known values) and pads its region by at least
  one pixel, because the kernel never writes its border. A second SOR implementation is a defect.
- Polygon/lasso rasterization belongs to `crate::tools::fill_polygon_spans`, shared with the
  PS-editor's selection; its sampling rule (scanline centre, even-odd, inclusive clamped span ends)
  is a contract, so a tool that also tests "is this point inside the selection" must use the same
  even-odd rule or the two answers disagree at the edges.

## Editing map
- To add a new cleaning tool, implement `CleaningTool`, export it from `mod.rs`, and register it in
  `CleaningTabState::default`.
- To change common brush radius, scratch preview, stroke commit, or dirty-tile behavior, edit
  `BrushToolBase` in `base.rs`.
- To change region selection, composited-region loading, editor zoom/scroll, or apply behavior,
  edit `RegionEditToolBase` in `base.rs`. A tool that wants a finished selection WITHOUT the
  region editor arms `set_selection_diverted` and collects the rect with
  `take_diverted_selection`; the diverted rect is dropped whenever the diversion is turned off
  or the selection is cancelled, so the flag must track the tool's own reason every frame.
- To change mask editor controls, sample-mask handling, or worker-run lifecycle, edit
  `RegionMaskInpaintToolBase` in `base.rs`.
- To change mask generation — a source, its parameters or availability rule, the watermark model
  catalog, the streaming progress, the detection call or the source picker — edit
  `mask_generation.rs`. It is shared, so the change reaches both the mask-inpaint editor and
  `ai_editor/`; edit a host only for where the answer is written (`base.rs` for the single editor
  mask, `ai_editor/mod.rs` for the selected `RegionFrame` layer) or for a block reason of that
  host's own.
- To change direct paint behavior, edit `zamazka.rs`; to change alt-version or current-page
  stamping, edit `stamp.rs`.
- To change the patch tool's selection gesture or its ROI/refusal geometry, edit
  `crate::tools::patch`; to change the seamless-cloning maths (pyramid, sweep schedule, Dirichlet
  weight, blend modes, feather ramp), edit `crate::tools::patch::membrane`. To change how a patch is
  STORED in the clean overlay, or how its region is loaded, edit `patch/mod.rs` here. Read
  `patch/MODULE_README.md` and `crate::tools::patch`'s `MODULE_README.md` first.
- To change local fill/inpaint algorithms, edit `gradient.rs` or `texture_synthesis.rs`. Their
  dilation is `ms_raster::dilate_square` behind a `bool` adapter; change the morphology there.
- To change how request PNGs are encoded, edit `ms_tools::png_wire` (bytes) or `region_png.rs`
  (error texts); never add an encoder in a tool or engine.
- To change the standalone watermark tool (modes, tiling/threshold parameters, mask preview, its
  settings file), edit `watermark_removal.rs`; the shared model catalog, status query and progress
  bar it reuses live in `base.rs`.
- To change FLUX.2 klein, Lama or SDXL Inpaint, edit `ai_editor/engines/flux2_klein/`,
  `ai_editor/engines/lama/` or `ai_editor/engines/sdxl/` — they are engines of the «ИИ-редактор
  области» tool, not tools; see `ai_editor/engines/MODULE_README.md`. The wire names of their
  methods live in `backend_ipc::protocol`.
- To change the LaMa model catalog — the offered checkpoints, which backend method each one runs,
  or whether it supports the refine pass — edit `ai_editor/engines/lama/catalog.rs`. It is the ONE
  place both that engine and the SDXL engine's 4-channel prefill picker read; the SDXL side takes
  the `lama_v2_model_catalog()` view and must never be handed the LaMa-MPE entry.
- To change what the library CARD can warn about or draw without recomputing anything, edit
  `EntryWarnings` / `entry_warnings` / `render_mark_on_white` in `watermark_entry.rs`; the raw
  material they read comes from `EntrySummary` in `watermark_library.rs`.
- To change the chapter mode's UI, jobs, reports or overlay patches, edit `watermark_removal.rs`
  (`ChapterState` and the `run_chapter_*` workers); to change the maths behind them, edit
  `../watermark_chapter.rs`; to change what a stored watermark holds on disk, or how it is
  exported/imported, edit `watermark_library.rs` and `config::watermark_library_dir`.
- To change reference-crop intake (background measurement, alignment, the refusal wording) or the
  auto-match ranking, or how a stored footprint is trimmed to the mark, edit `watermark_entry.rs`
  (the extent measurement itself lives in `../watermark_chapter.rs`); to change the library screen itself — either of
  its two modes, the card, the sample list or the pending crops — edit
  `watermark_library_window.rs`, and where its panel sits by default is
  `cleaning_default_dock_layout` in `../tab.rs`.
- To change what a library card does to the open chapter (select / unselect / already here) or
  what an armed canvas selection becomes, edit `run_library_panel_request`,
  `unload_library_entry`, `chapter_library_selection`, `chapter_saved_entries`,
  `chapter_entry_index`, `next_library_arm` and `run_library_capture` in
  `watermark_removal.rs`; the card's three-way state itself is `entry_membership` in
  `watermark_library_window.rs`. The panel only raises the request.
- To change when a reservation ENDS, edit `WatermarkLibraryWindow::arm_survives` (which screen
  draws the cancel) and the one place the tool consults it in `draw_overlay_ui`.
- To change what editing a stored entry's sample list does, edit `drop_entry_sample` and the two
  halves of the round trip it is built on — `rebuild_stored_kind` and `save_request_from_kind`
  in `watermark_entry.rs`, which the reference intake and the chapter's library load/write share.
- To change how long session scratch crops live, edit `sweep_stale_scratch_crops` /
  `scratch_crop_is_stale` in `watermark_removal.rs`; the panel's own delete-on-commit /
  delete-on-discard is `remove_scratch_crop` in `watermark_library_window.rs`.
- To change the on-canvas region frame (its geometry, lock rules, painting or input), edit
  `region_edit_v2/` and read its `MODULE_README.md` first. To change what the area editor DOES
  with that frame — its engine picker, its run path, its apply check or either of its panels —
  edit `ai_editor/`; where its main panel sits by default is `cleaning_default_dock_layout` in
  `../tab.rs`. Which ENGINES it offers, and what each of them requires of the frame, is
  `ai_editor/engines/`.
- To change Python backend IPC method names, request/response blob layout, model selection, unload
  behavior, or model ensure logic, edit the relevant AI tool file and keep `ai_models.rs` as the
  model boundary.
