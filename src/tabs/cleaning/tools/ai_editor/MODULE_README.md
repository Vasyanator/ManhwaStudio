# Module: src/tabs/cleaning/tools/ai_editor

## Purpose
The «ИИ-редактор области» cleaning tool — the FIRST consumer of the on-canvas region-editing
framework (`../region_edit_v2/`). It owns one `RegionFrame` with two mask layers, drives its
per-frame pass, and performs the two things the frame is deliberately not allowed to do
itself: run the consumer, and merge a result into the clean overlay.

**The processing in this directory is a labelled PLACEHOLDER.** There is no AI backend here,
no IPC method, no model and no worker thread: `stub.rs` fills the painted mask with flat
colours. It exists so the whole path — paint → process → preview → apply → undo — runs for
real while the framework is young, and the main panel says so in words the user reads
(`cleaning.tools.area_editor.placeholder_notice`). It will be replaced by a backend call
that runs on a worker and reports through a channel; nothing else in this directory has to
change when that happens, because the frame already reports intent rather than performing it.

## Architecture
```
CleaningTool::draw_overlay_ui   RegionFrame::update -> FrameOutcome -> run_stub / apply_result
CleaningTool::draw_ui           compact panel: brush radius, paint/erase, layer, undo, erase
CleaningTool::draw_main_panel   «Редактор области»: geometry, limits, layer counts, «Обработать»
CleaningTool::on_key_event      `-` / `=` / `+`, for the pointer OUTSIDE the frame
CleaningTool::on_wheel_event    Shift+wheel, for the pointer OUTSIDE the frame
```
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

## Files and submodules
- `mod.rs`: `AiEditorTool` (the `CleaningTool` impl), the layer table `AI_EDITOR_LAYERS`, the
  placeholder consumer's `STUB_CONSTRAINTS`, the D7 size check `check_result_fits`, and the
  two panel bodies.
- `stub.rs`: `build_stub_result` — the placeholder itself, a pure function over the captured
  clean-overlay chunk, the mask stack and one fill colour per layer. GUI-free and fully
  tested. This is the file that goes away when a real consumer arrives.

## Contracts and invariants
- **Apply validates the size and refuses (D7).** `CanvasView::replace_overlay_region_px`
  silently nearest-rescales a chunk of the wrong size into the target and clips a target that
  leaves the overlay, overwriting alpha wholesale. `check_result_fits` rejects both cases with
  a typed error before the call, and the user gets a message while the log gets the numbers.
  Never relax this into a rescale.
- **`block_canvas_zoom()` stays `false` (D5).** That flag also disables the clean-overlay undo
  shortcuts for the whole session, and this tool lives on the canvas for the whole session.
  Blocking is precise instead: `captures_canvas_pointer` over the frame's hitbox, and
  `block_canvas_drag_scroll_on_primary` only while a frame gesture is in flight. Canvas
  drag-scroll additionally needs Space held (`canvas/scene.rs`), which is why mask painting
  needs no gate of its own. A test pins this (`the_area_editor_never_blocks_canvas_zoom_...`):
  ten of the twelve registered tools override the flag to `true`, so copying a sibling is the
  likely edit and nothing else in the suite would notice it.
- **The base is transparent only for a page with no overlay.** `capture_base` answers a fully
  transparent chunk exactly when `CanvasView::overlay_size` is `None` — that page has no clean
  pixels, so transparency is its true state. Once an overlay EXISTS, a capture that fails or
  returns the wrong size is a `CaptureError` with a user message and a log carrying the page
  index and the region: substituting transparency there would make apply erase real clean
  pixels with nothing, unnoticed.
- **«Применить» and «Отменить» live in the main panel as well.** A result-pending frame is
  locked and its own button row is only as wide as the frame is on screen, so the panel is the
  reachable surface at a low zoom. Both go through `RegionFrame::request_apply` /
  `request_cancel` and are enabled from `FrameButtons`, so the two surfaces cannot disagree.
- **`wants_primary_stroke` is `false`.** Every gesture this tool has belongs to the frame's own
  `egui::Area` and is sensed through a `Response`; the tab must not open a canvas stroke for it.
- **The layer table is the single source of truth for the layer count.** `AI_EDITOR_LAYERS`
  carries a preview tint, a placeholder fill and a name key per layer, and its LENGTH is what
  the frame is built with. Adding a layer means adding a row, nothing else — do not hardcode
  `2` anywhere.
- **User message and technical detail are separate.** `report_error` shows a localized
  sentence and logs the English `Display` of the typed error. A technical reason must never be
  interpolated into a translated string, and the user-facing half must never carry buffer
  lengths.
- **The mask may not be edited while a result waits or work runs** (`mask_editable`): the mask
  then describes work already handed over. The compact panel's undo and clear are disabled in
  those states, which mirrors the frame's own painting rule.
- **Nothing here blocks the GUI thread.** The placeholder is one image clone plus one pass per
  mask layer over the region, with no decode, no file and no network. A real consumer must NOT
  inherit the synchronous call — it goes on a worker with `set_processing(true)` around it.
- Every `t!` key of this tool lives under `cleaning.tools.area_editor.*`; the frame's own
  chrome uses `cleaning.region_frame.*` and the dock tab caption is
  `cleaning.tab.area_editor_tab`.

## Editing map
- To change what the placeholder produces, or to replace it with a real consumer:
  `stub.rs` plus `AiEditorTool::run_stub`. A real consumer also needs `set_processing` around
  its run and a channel poll at the top of `draw_overlay_ui`.
- To change what the compact panel offers: `draw_brush_controls`, `draw_layer_picker`,
  `draw_mask_actions`.
- To change what the main panel shows: `draw_geometry_section`, `draw_layers_section`,
  `draw_result_actions` and `draw_main_panel`. Where that panel SITS is
  `cleaning_default_dock_layout` in `../../tab.rs`.
- To change the size requirements the frame validates against: `STUB_CONSTRAINTS`.
- To add or re-colour a mask layer: `AI_EDITOR_LAYERS`, plus its name key in all five locales.
- To change the frame itself — handles, clamping, page transition, status line, chrome:
  `../region_edit_v2/`, never here.
