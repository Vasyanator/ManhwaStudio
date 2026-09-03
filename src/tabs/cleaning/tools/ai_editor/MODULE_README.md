# Module: src/tabs/cleaning/tools/ai_editor

## Purpose
The «ИИ-редактор области» cleaning tool: the HOST that joins the on-canvas region-editing
framework (`../region_edit_v2/`) to a catalog of AI engines (`engines/`). It owns one
`RegionFrame`, drives its per-frame pass, and performs the things the frame is deliberately
not allowed to do itself — load the source region, run the selected engine, and merge a
result into the clean overlay.

The split of duties is fixed and is the reason this directory exists: an ENGINE owns its
parameters, its settings file, its wire protocol, its worker threads and its OWN progress bar
(there is no shared progress vocabulary — `dev-docs/region_edit_v2_plan.md` §13.2 D13); this
HOST owns the rectangle, the mask stack, the source region, the pending result and the apply
path, and learns only Running / Done / Failed.

## Architecture
```
CleaningTool::draw_ui           compact panel: engine picker, brush, mask layer, mask actions
CleaningTool::draw_main_panel   «Редактор области»: engine.draw_parameters + host actions
CleaningTool::draw_overlay_ui   the per-frame pass (order below), the run, the apply
CleaningTool::on_key_event      `-` / `=` / `+`, for the pointer OUTSIDE the frame
CleaningTool::on_wheel_event    Shift+wheel, for the pointer OUTSIDE the frame
```

`draw_overlay_ui` is the whole per-frame pass and its ORDER is load-bearing:

1. read `allows_empty_mask()` back from the engine into the frame — the engine may derive it
   from a parameter the user changed in this same frame's panel body, which ran earlier inside
   `CanvasView::draw`; a stale copy silently blocks or allows a run (§13.5);
2. `RegionFrame::update` — the frame settles its rectangle and reports a `FrameOutcome`;
3. push `set_backend_available` / `set_torch_available` / `set_region` — after the pass, so the
   rectangle is THIS frame's. Pushing it after a run has started would look like a moved frame
   to an engine that treats that as "a different image" and cancels the run. `set_region` also
   carries `geometry_settled` = `!RegionFrame::drag_active()`, read after the pass for the same
   reason: the frame on which the pointer is released already reports settled;
4. act on the outcome: clear mask, cancel, process, apply;
5. `poll_region_load()`, then `poll_engine(ctx)`.

One run is three steps, and none of them touches the GUI thread with real work:

```
«Обработать» -> start_run()             capture the clean-overlay chunk, send a RegionLoadRequest
             -> poll_region_load()      the loader worker answers -> hand_region_to_engine()
             -> AiEngine::start()       the engine's own worker runs the model
             -> poll_engine() each frame -> Done -> accept_result() -> ResultLayer
                                          -> Failed -> a user message and a log
```

The source region is the SOURCE PAGE crop composited with the current clean overlay (D14), not
the bare overlay chunk: a page with no clean edits would otherwise hand the model pure
transparency. It is produced by `base.rs::spawn_region_loader_thread`, reused rather than
copied (D10) — the page decode happens there, never here, and the tool must send `None` and
join that worker on drop.

Both input hooks are half of the brush's gestures on purpose: `tab.rs` drops a tool's key,
wheel and cursor hooks while the canvas pointer is occluded, and this tool's own frame
occludes exactly its hitbox (`captures_canvas_pointer`). Over the frame the identical
gestures — and the brush ring, which is why this tool implements no `draw_cursor` — are
handled inside `RegionFrame`'s pass. Both halves route into the frame's single `MaskBrush`,
so there is one radius, not two.

The tool holds no geometry of its own. `(page_idx, rect_px)`, the masks, the pending result
and the lock all live in `RegionFrame`; this file reads them and never caches them, so a
panel can never show a rectangle the frame has already moved.

A dock panel body runs inside `CanvasView::draw` and may mutate only the tool, so
«Обработать» does not process: it calls `RegionFrame::request_process`, and the next
`draw_overlay_ui` folds that into the outcome — but only while `FrameButtons::process` still
allows it, so a panel can never start a run the frame refuses.

## Panel split (§13.1)
- «Выбранный инструмент» (`draw_ui`): the engine picker as toggle buttons in two sections,
  «Без промпта» and «С промптом» (a section with no engine is not drawn), then the brush
  radius and paint/erase mode, the mask-layer switch (drawn only for two or more layers), the
  mask actions, and the painted-pixel count per layer while anything is painted.
- «Редактор области» (`draw_main_panel`): `AiEngine::draw_parameters` for the selected engine,
  and under it the host's own row — «Обработать», «Применить», «Отменить» — the frame's status
  line, the size requirements while the size is invalid, and the last message.
- The frame's own chrome keeps «Применить» / «Отменить» / «Стереть маску» and the status line.

«Обработать» exists ONLY in the left panel: the frame's chrome row has three buttons and none
of them is the run. «Применить» / «Отменить» are repeated there because a frame holding a
result is LOCKED, and a locked frame may scroll out of view entirely — its chrome row is then
unreachable and the panel is the only way to resolve the result.

## Files and submodules
- `mod.rs`: `AiEditorTool` (the `CleaningTool` impl), the run path, the D7 size check
  `check_result_fits`, and the two panel bodies.
- `engine.rs`: the `AiEngine` contract — `EngineSection`, `EngineRunRequest`, `EnginePoll`, and
  `MaskLayerSpec` re-exported from the framework.
- `engines/`: the engine catalog — one module per engine plus `all_engines()`. Own
  `MODULE_README.md`.

## Contracts and invariants
- **Apply validates the size and refuses (D7).** `CanvasView::replace_overlay_region_px`
  silently nearest-rescales a chunk of the wrong size into the target and clips a target that
  leaves the overlay, overwriting alpha wholesale. `check_result_fits` rejects both cases with
  a typed error before the call, and the user gets a message while the log gets the numbers.
  The same check runs on the engine's answer in `accept_result` and on the loaded region in
  `hand_region_to_engine`. Never relax any of them into a rescale.
- **The engine picker is disabled while the frame is locked (D15).** Engines declare different
  mask layers, so a switch re-creates the mask stack and would discard painted work. The
  disabled tooltip says exactly that, and `RegionFrame::set_mask_layers` refuses the switch on
  its own side as well.
- **A switch never resizes the frame.** `set_constraints` publishes the new requirements and
  nothing else: a rectangle the new engine refuses turns the frame RED, blocks «Обработать»
  and gets its requirements spelled out in the left panel. That is the designed behaviour, not
  a gap.
- **`allows_empty_mask()` and the three setters are re-read EVERY frame.** FLUX.2 klein derives
  the empty-mask rule from a checkbox in its own panel, so a copy taken at the switch goes
  stale on the next click.
- **A per-frame push is not permission to work per frame.** Because `set_region` runs every
  frame and the rectangle moves on every scrolled pixel (the keep-in-view clamp), the host tells
  the engine whether the geometry has SETTLED (`!drag_active()`, which covers a move drag, a
  resize drag and a mask stroke). An engine may DISPLAY an unsettled rectangle but must not start
  work off it — FLUX.2 klein defers its `.estimate` query to the first settled frame, which is
  what keeps a scroll or a drag from becoming one IPC round trip per rendered frame.
- **The engine is polled every frame, panel visible or not.** `AiEngine::poll` is where an
  engine drains its channels; skipping it strands a finished run AND — for FLUX.2 klein, whose
  debounced settings saver lives inside `poll` — silently loses model paths, memory presets and
  prompts on exit. A test pins the per-frame poll.
- **`block_canvas_zoom()` stays `false` (D5).** That flag also disables the clean-overlay undo
  shortcuts for the whole session, and this tool lives on the canvas for the whole session.
  Blocking is precise instead: `captures_canvas_pointer` over the frame's hitbox, and
  `block_canvas_drag_scroll_on_primary` only while a frame gesture is in flight. Canvas
  drag-scroll additionally needs Space held (`canvas/scene.rs`), which is why mask painting
  needs no gate of its own. A test pins this (`the_area_editor_never_blocks_canvas_zoom_...`):
  ten of the twelve registered tools override the flag to `true`, so copying a sibling is the
  likely edit and nothing else in the suite would notice it.
- **A missing clean overlay is `None`, not a fabricated chunk.** `capture_clean_overlay`
  answers `None` exactly when `CanvasView::overlay_size` is `None` — that page has no clean
  pixels, so the loader composites nothing over the page crop. Once an overlay EXISTS, a
  capture that fails or returns the wrong size is a `CaptureError` with a user message and a
  log carrying the page index and the region: going on without it would hand the model a
  region that does not show the user's own clean edits.
- **`pytorch_required()` follows the SELECTED engine.** The tab gates the tool's own button on
  it (`AiButton` with `AiRequirement::Torch`) and auto-switches away from an unavailable tool,
  so answering for the tool as a category would mis-gate every engine but one.
- **A stale load answer is dropped on its job id.** «Отменить» and an engine switch clear
  `pending_load`; a region that arrives afterwards belongs to an abandoned run and must never
  start one.
- **`wants_primary_stroke` is `false`.** Every gesture this tool has belongs to the frame's own
  `egui::Area` and is sensed through a `Response`; the tab must not open a canvas stroke for it.
- **User message and technical detail are separate.** `report_error` shows a localized
  sentence and logs the English `Display` of the typed error. A technical reason must never be
  interpolated into a translated string, and the user-facing half must never carry buffer
  lengths.
- **The mask may not be edited while a result waits or work runs** (`mask_editable`): the mask
  then describes work already handed over. The compact panel's undo and clear are disabled in
  those states, which mirrors the frame's own painting rule.
- **Nothing here blocks the GUI thread.** No decode, no file read and no network call happens
  on it; the only per-frame work is capturing an in-memory overlay chunk when a run starts.
- Every `t!` key of this tool lives under `cleaning.tools.area_editor.*`; the frame's own
  chrome uses `cleaning.region_frame.*` and the dock tab caption is
  `cleaning.tab.area_editor_tab`.

## Editing map
- To change the run path: `start_run`, `poll_region_load`, `hand_region_to_engine`,
  `poll_engine`, `accept_result` — in that order, they are one pipeline.
- To change what the compact panel offers: `draw_engine_picker`, `draw_brush_controls`,
  `draw_layer_picker`, `draw_mask_actions`, `draw_mask_summary`.
- To change what the main panel shows: `draw_main_panel` and `draw_host_actions`. Where that
  panel SITS is `cleaning_default_dock_layout` in `../../tab.rs`.
- To change an engine, or to add one: `engines/`, never here. To change what the host may ASK
  of an engine: `engine.rs`, and record the change in `dev-docs/region_edit_v2_plan.md` §13.3.
- To change the size requirements or the mask layers a run uses: the ENGINE's `constraints()`
  and `mask_layers()`. The host only forwards them.
- To change the frame itself — handles, clamping, page transition, status line, chrome:
  `../region_edit_v2/`, never here.
