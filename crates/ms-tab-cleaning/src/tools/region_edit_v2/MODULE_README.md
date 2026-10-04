# Module: crates/ms-tab-cleaning/src/tools/region_edit_v2

## Purpose
The reusable on-canvas region-editing framework of the cleaning tab, and the GENERIC host tool
built on it. The framework replaces the detached region-editor window flow (not
`RegionEditToolBase` itself, which stays untouched) with a selection FRAME drawn over the page
strip: eight resize handles, a drag strip above it, N mask layers and a processed-result layer
inside it, and a button row plus a status line below it. The host (`RegionEditHost`) turns that
frame plus a catalog of AI engines (`AiEngine`) into a whole `CleaningTool`: it loads the source
region, runs the selected engine, fills a mask layer from a backend detector and merges a result
into the clean overlay. A consumer tool is only a `HostSpec` — id, title, egui id salts, log tag
and engine catalog — beside this directory (`../ai_editor/` and `../ai_api_editor/`).
Design and the decisions behind every rule here: `dev-docs/region_edit_v2_plan.md`.

## Architecture
Five framework layers, deliberately separated so that almost everything is testable without a
window, and the host on top of them:

```
geometry.rs        pure maths      size constraints, hitbox, viewport clamp, page transition, arrow
layers.rs          pixels          MaskStack (N L8 layers + tinted previews) and ResultLayer
input.rs           hit geometry    handle rects, and the move/resize maths a drag performs
render.rs          paint only      strokes, handles, chrome plates, status text, off-screen arrow
frame.rs           the pass        RegionFrame: state, the per-frame pass, the reported intent
engine.rs          the contract    AiEngine + run request/poll; the one size-violation wording
engine_settings.rs engine helper   the settings save gate shared by every hosted engine
host.rs            the tool        HostSpec, RegionEditHost: run, mask generation, apply, tests
host_panels.rs     the panels      the host's compact panel and its half of the main panel
```

Dependency direction: the framework files know nothing of the host; `host*.rs`, `engine.rs` and
`engine_settings.rs` know nothing of any consumer tool or engine — a consumer depends on them,
never the reverse.

The authoritative frame state is `(page_idx, rect_px: OverlayRectPx)` in SOURCE PAGE PIXELS.
The screen rectangle is re-derived every frame from `CanvasView::page_scene_rect` and
`CanvasView::zoom()` and is never stored as truth, which is what makes the frame's page-pixel
footprint stable across zoom and scroll.

One pass per frame, run by the host tool from `CleaningTool::draw_overlay_ui` — the only hook
that owns the context, the canvas and the project at once:

1. `usable_viewport_for(hitbox, canvas.visible_scene_rect(), panel_rects)` — the dock panels are
   cut out RELATIVE to the frame's current hitbox (see the contract below). Before the frame has
   been placed there is no hitbox yet, so the placement step passes the viewport as its own
   hitbox.
2. Place the frame if it has none yet (centred on the current page).
3. Free frame only: `choose_page` may re-anchor it, then `keep_in_view_delta` clamps it and the
   correction is written back into `rect_px`.
4. If the hitbox left the viewport entirely, paint the off-screen arrow and stop.
5. Otherwise one `egui::Area` sized to the HITBOX senses the strip, the handles, the body and
   the chrome, applies what they did, and paints the result at the rectangle it settled on.

## Files and submodules
- `geometry.rs`: every rule of the design that is maths. GUI-free; uses `Rect`/`Pos2`/`Vec2` as
  plain geometry only and must never touch `Ui`, `Context`, `Painter` or a texture. The ONE
  owner of what a legal frame size is: `FrameConstraints` / `AspectLimit` (the data),
  `check_size`, `upscale_factor_for` and `snap_size` (the semantics).
- `size_oracle.rs`: TEST ONLY. A frozen copy of the size rules from before the extended rules
  existed, and the sweep `assert_equivalent_to_legacy`; permanent, never refactored with
  `geometry.rs`.
- `layers.rs`: `MaskLayerSpec` (what a consumer declares about one layer: its tint and the
  catalog key of its name), `MaskStack` (per-layer L8 buffer, O(1) set-pixel counter, tinted
  preview, partial texture upload, per-stroke undo) and `ResultLayer`. No brush radius policy
  lives here.
- `input.rs`: `HandleKind`, the handle hit rects and arcs, and `moved_rect_px` /
  `resized_rect_px`. Every drag is measured from an anchor captured on `drag_started`, never
  accumulated per frame.
- `render.rs`: the local chrome colour constants and every paint call; the state colours
  (backing ring, refused red, occupied green) are `ms_theme::canvas` tokens. Registers no hitbox, ever.
- `frame.rs`: `RegionFrame` and the per-frame pass; `FrameLock`, `FrameVisual`, `FrameHost`,
  `FrameOutcome`, `FrameButtons`.
- `engine.rs`: the `AiEngine` trait (`EngineSection`, `EngineRunRequest`, `EnginePoll`,
  `MaskLayerSpec` re-exported from `layers`), plus `violation_text` — the ONE mapping from a
  `SizeViolation` to its sentence — and `region_size_refusal`, the engines' run-path size
  re-check built on it. Mask generation is deliberately ABSENT from the trait: it is the host's.
- `engine_settings.rs`: pure, engine-agnostic helpers for an engine's own settings file —
  `settings_save_due(dirty, settings_loaded, save_in_flight)`, the ONE save gate every engine's
  `poll` saver consults. Each engine still owns its document, path and IO; add a rule here only
  when it is the same for every engine, never a copy in an engine.
- `host.rs`: `HostSpec`, `RegionEditHost` (the `CleaningTool` impl, built by
  `RegionEditHost::new(&'static HostSpec)`), the run and mask-generation pipelines, the D7 size
  check `check_result_fits`, `ApplyError` / `CaptureError`, and the host's tests (over a test
  spec of recording engines).
- `host_panels.rs`: `impl RegionEditHost` panel bodies — engine picker, brush, layer picker, mask
  generation, mask actions and summary, and the host actions under the engine's parameters. Split
  from `host.rs` for size only; the fields it reads are `pub(super)` for that reason.
- `mod.rs`: submodule declarations. No flattening re-exports.

## Contracts and invariants
- **The lock is derived, never assigned.** `Processing` > `ResultPending` > `MaskPainted` >
  `Free`. A locked frame cannot be moved, cannot be resized, and is NOT kept in view — it
  scrolls away and grows an arrow. This is what makes `MaskStack::resize` free to clear: a
  resize can never reach a stack that holds work.
- **A gesture in flight locks the frame too.** A paint or erase stroke whose button is still
  down derives `MaskPainted` even when the mask is empty, and `drag_active()` reports it. Both
  exist because erasing the last painted pixel mid-stroke would otherwise free the frame
  BEFORE the button is released, and the page transition, the keep-in-view clamp and the
  resize a page change performs would then run under the live stroke and throw its undo
  snapshot away. A move/resize drag keeps the keep-in-view clamp on purpose — that clamp IS
  "manual dragging stops at the viewport border" — and never coexists with a stroke.
- **A dock panel is cut from the viewport RELATIVE to the hitbox, per axis.** A cut is a
  full-width or full-height band, so it may only be charged to a panel that could actually hide
  the frame: a panel sharing neither the hitbox's columns nor its rows costs nothing, one sharing
  exactly one axis is cut from the edge it lies on, and one genuinely over the hitbox is cut from
  the edge that removes the LEAST AREA (`slab × viewport height` against `slab × viewport width`).
  Choosing by smallest SLAB instead is the defect this rule replaced: a right-docked panel that
  starts near the top of the viewport had a top slab shorter than its right slab, so it cut a
  full-width band and the frame could not be dragged above the panel's bottom edge. The cut set
  therefore depends on where the frame is and can change after a correction; that converges rather
  than oscillating, because a correction only pushes the hitbox AWAY from the edge that cut it and
  the clamp never pulls it back when a cut disappears (`geometry.rs`, the fixed-point test).
- **The keep-in-view correction is truncated toward zero.** Rounding it to the nearest page
  pixel overshoots, and at exactly half a pixel the frame alternates between two origins every
  frame; a sub-pixel residual overhang is tolerated instead, and tolerating it is what makes
  the clamp terminate. Drag deltas still round to nearest — the two live in separate functions
  in `input.rs`.
- **The handles live entirely OUTSIDE the frame.** Each of the eight is the part of a
  `HANDLE_RADIUS` disc that falls outside the frame — a half disc on a side midpoint, a
  three-quarter disc on a corner — and its hit rectangles cover exactly that and nothing more.
  The interior belongs to mask painting: a handle centred on the border reached half-way into
  it and swallowed strokes. Two consequences carry through the module. The hitbox grows by
  `FrameChrome::handle_margin` on every side, so the keep-in-view clamp cannot park a handle
  beyond the viewport border and the handles stay inside the `Area` that senses them; and the
  chrome rows are measured from the handles' outer edge, never from the frame's. A CORNER's
  area is L-shaped, so it is sensed through TWO `ui.interact` rectangles with distinct ids —
  the drag state is keyed by `HandleKind`, so either of them starts the same resize.
- **The chrome does not inherit the frame's screen width.** The rows are at least
  `FrameChrome::min_row_w` wide, widened symmetrically about the frame's centre, and the
  hitbox grows with them; a status sentence that still does not fit is elided, never spilled
  over the artwork. The rows are laid out from the HITBOX, which is also what the viewport
  clamp holds on screen.
- **The frame is never the only way to resolve a pending result.** A result-pending frame is
  locked and its own button row is only as wide as the frame is on screen, so the host tool's
  main dock panel must offer «Применить» and «Отменить» as well. Both surfaces go through
  `FrameButtons` and `FrameOutcome`; a panel queues through `request_apply` / `request_cancel`
  and the next pass re-checks the same enablement table. Queued requests are folded at the TOP
  of the pass, so they survive the off-screen early return — which is exactly the state that
  strands the user otherwise. «Обработать» has no chrome button at all: the row holds exactly
  «Применить», «Сравнить», «Отменить» and «Стереть маску», so starting a run is always a panel
  action. «Сравнить» is the one chrome button a panel must NOT repeat — a frame that scrolled
  out of view has nothing to look at.
- **«Сравнить» is a MOMENTARY HOLD, not a toggle, and it is the only button that is not a
  request.** It is enabled by exactly the condition «Применить» is (a result is pending), and
  while its pointer button is down the pending result layer is not painted, so the original
  pixels show through. `compare_held` is recomputed from scratch in every drawn pass as
  "enabled AND `Response::is_pointer_button_down_on`" — never toggled, never accumulated — and
  `result_hidden` re-checks the enablement again when the contents are painted, so a result
  applied or cancelled mid-hold can never leave the frame hiding a layer that is gone. The MASK
  layers keep drawing in both states: they are the user's own marking, equally present before
  and after a run, and blinking them would disturb the comparison rather than help it. This is
  also why the button row is sensed BEFORE the frame's contents are painted — a hold sensed
  after them would apply one frame late, and egui may not repaint again until the release.
- **`block_canvas_zoom()` must stay `false` in the host tool.** That flag also disables the
  clean-overlay undo shortcuts for the whole session (`tab.rs`). Block precisely instead:
  `RegionFrame::captures_pointer` over the hitbox, and `drag_active()` for
  `block_canvas_drag_scroll_on_primary`.
- **The `Area` is sized to the hitbox, never to the viewport.** A viewport-sized area makes
  egui report the pointer as "over an area" everywhere and kills canvas wheel scrolling. It
  sits on `Order::Middle`, below the dock panels on `Order::Foreground`.
- **Hover and drags go through a `Response`.** Never test a raw pointer position against a rect
  to decide hover (`egui-docs/06-overlays.md` §5); the two raw reads that remain — the pointer
  of a drag already claimed through a `Response`, and the mouse button state — are gated on one.
- **Colours: red wins over green.** A locked frame whose size stopped satisfying the consumer is
  drawn red and its status line says it must be released first.
- **The size rules have ONE owner: `geometry.rs`.** A consumer declares DATA
  (`FrameConstraints`: grid, side floor and ceiling, area floor and ceiling, an `AspectLimit` of
  two independent maxima, an allowed-size table, an upscale allowance `max_upscale` = K); what
  the data means — validity, the upscale factor a valid region is sent at, snapping and page
  fitting — is decided only by `check_size`, `upscale_factor_for` and `snap_size`. A size is
  valid when SOME `k` in `1..=K` makes `k·w × k·h` pass every rule, so `check_size(..).is_none()`
  exactly when `upscale_factor_for(..).is_some()`, and a consumer that sends the region upscaled
  takes `k` from `upscale_factor_for`, never from its own evaluation. `snap_size` never returns
  a size larger than the page; a rule set no size on the page satisfies yields a red frame, not
  an error. Declarations are written as `..FrameConstraints::UNCONSTRAINED` updates, so a new
  rule reads as absent everywhere. The test-only `size_oracle.rs` pins the original four rules
  (grid, minimum side, maximum area, symmetric aspect) to their legacy answers; every hosted
  engine is swept against it.
- **The consumer's shape is PUSHED in, and a change never resizes the frame.** A host publishes
  `set_constraints`, `set_mask_layers` and `set_allows_empty_mask` when the active consumer
  changes, and re-pushes `set_allows_empty_mask` and `set_constraints` (never the mask layers)
  every frame because a consumer may derive either from one of its own parameters.
  `set_constraints` only re-validates: a rectangle the new consumer refuses
  turns the frame red and blocks «Обработать» rather than being snapped, because the user placed
  that rectangle by hand. `set_mask_layers` RE-CREATES the stack whenever the declaration
  differs, so it is refused (logged, no-op) unless the frame is free — the same protection the
  lock gives painted work everywhere else. `set_allows_empty_mask` relaxes the non-empty-mask
  requirement of «Обработать» and NOTHING else: the size check and the lock still apply.
- **The frame applies nothing.** `update` borrows the canvas SHARED and reports intent through
  `FrameOutcome`; the tool performs it with `&mut CanvasView`, and must refuse a result whose
  size differs from `rect_px` — `replace_overlay_region_px` silently rescales.
- **Reuse, never copy.** Pointer-to-pixel and overlay-chunk conversions come from `tools/base.rs`
  (`pub(super)`); brush radius policy comes from `crate::tools::MaskBrush`.
- **A mask that did not come from the brush enters through `MaskStack::set_active_from_alpha`,
  and through the same door a stroke uses.** It writes the ACTIVE layer only, binarizes its
  input to `0`/`255` (the layer's own invariant, and what keeps the set-pixel counter exact),
  and SNAPSHOTS the layer first, so a whole generated mask is one undo step and erases exactly
  like painted work. A buffer that is not exactly `width * height` bytes is refused, having
  written nothing and taken no snapshot: every consumer indexes the mask and the region with
  the same stride, so a mask of the wrong shape must never be stretched over a layer. The
  consumer checks the SHAPE (a transposed buffer has the right length); this checks the length.
  The host's «Сгенерировать маску» is the only caller today.
- **Painting is refused while a result is pending or work is running**: the mask then describes
  work already handed over. It is also refused while a canvas zoom modifier (Ctrl/Cmd/`Z`) is
  held, because Ctrl+drag over the frame zooms the page and must not leave a stroke behind.
- **The brush is the region editor's brush, and the frame answers its gestures itself.**
  Radius, wheel and the `-`/`=`/`+` shortcuts all live in `crate::tools::MaskBrush`; erasing
  follows the same rule the region editor uses (`stroke_erases`: the right button erases unless
  the left is held too, Shift+left erases, and the panel's `set_erase` mode erases). The
  gestures are handled INSIDE the pass — over the frame's hitbox for the shortcuts and
  Shift+wheel, and on the frame body for the brush ring — because `tab.rs` refuses to deliver
  a tool's key, wheel or cursor hook while the canvas pointer is occluded, and the frame
  occludes exactly its own hitbox (`captures_pointer`). The host tool's `on_key_event` /
  `on_wheel_event` cover the pointer OUTSIDE the frame; neither surface can be dropped.
- Every `t!` key of this module lives under `cleaning.region_frame.*`.

## Editing map
- To change what a size must satisfy, how the frame is clamped, or when it changes page:
  `geometry.rs` (all of it is unit-tested; add the test with the rule). A NEW size rule also
  needs its `SizeViolation` sentence in `violation_text` (`engine.rs`) and its panel sentence in
  `constraint_lines` (`host_panels.rs`), keys under `cleaning.tools.area_editor.*`; leave
  `size_oracle.rs` alone — it guards the old rules, it does not follow the new ones.
- To change what a handle drag does to the rectangle: `resized_rect_px` in `input.rs`.
- To change how big a handle is, where it may be grabbed, or how much of a disc it shows:
  `HANDLE_RADIUS`, `handle_hit_rects` and `handle_arc` in `input.rs` — the three agree by
  construction, and `render.rs` only turns the arc into a polygon.
- To change a chrome colour, a plate, the grip or the arrow: `render.rs` only; the state colours
  are retuned studio-wide in `crates/ms-theme/src/canvas.rs`.
- To change the lock rules, the button enablement, the status line or the pass order:
  `frame.rs`.
- To change how a mask layer stores, previews or uploads its pixels: `layers.rs`.
- To change how a mask arrives from somewhere other than the brush: `set_active_from_alpha` in
  `layers.rs`. `place_for_test` in `frame.rs` is how a CONSUMER's own tests reach a placed
  frame — the rectangle stays unsettable from production code, which is what keeps every
  placement inside the pass with its constraints and its clamp.
- To change what a consumer may declare about a layer, or how a consumer switch re-shapes the
  frame: `MaskLayerSpec` in `layers.rs` and the three setters in `frame.rs`.
- To add an AI tool, add a directory beside this one holding a `static HostSpec` (unique
  `tool_id`, its own title key, its own pair of id salts, its own log tag) and an engine catalog,
  expose `RegionEditHost::new(&SPEC)` and register it in `../../tab.rs`; do not add tool-specific
  state here. The worked examples are `../ai_editor/` (four local engines) and `../ai_api_editor/` (one cloud engine).
- To build a region tool that is NOT an engine host, drive `RegionFrame` from its own
  `CleaningTool::draw_overlay_ui`, as `host.rs` does.
- To change the host's run path: `start_run`, `poll_region_load`, `hand_region_to_engine`,
  `poll_engine`, `accept_result` in `host.rs` — in that order, they are one pipeline.
- To change the mask-generation path: `start_mask_generation`, `start_mask_detection`,
  `poll_mask_generation`, `accept_generated_mask` in `host.rs`. To change a SOURCE, a requirement
  rule, the detection call or the shared controls: `../mask_generation.rs`, never here.
- To change what the compact panel offers or what the main panel shows under the engine:
  `host_panels.rs`. Where the main panel SITS is `cleaning_default_dock_layout` in `../../tab.rs`.
- To change what the host may ASK of an engine: `engine.rs`, and record the change in
  `dev-docs/region_edit_v2_plan.md` §13.3. To change how a size violation is worded:
  `violation_text` in `engine.rs`, the one owner.
- To change an engine, its size requirements or its mask layers: the consumer's `engines/`
  (`constraints()` / `mask_layers()`). The host only forwards them.

## The generic host (`host.rs`, `host_panels.rs`)
The split of duties is fixed: an ENGINE owns its parameters, its settings file, its wire
protocol, its worker threads and its OWN progress bar (no shared progress vocabulary — design
§13.2 D13); the HOST owns the rectangle, the mask stack, the source region, mask GENERATION into
that stack, the pending result and the apply path, and learns only Running / Done / Failed.

```
CleaningTool::draw_ui           compact panel: engine picker, brush, mask layer, mask generation,
                                mask actions
CleaningTool::draw_main_panel   «Редактор области»: engine.draw_parameters + host actions
CleaningTool::draw_overlay_ui   the per-frame pass (order below), the run, the generation, the apply
CleaningTool::on_key_event      `-` / `=` / `+`, for the pointer OUTSIDE the frame
CleaningTool::on_wheel_event    Shift+wheel, for the pointer OUTSIDE the frame
```

`draw_overlay_ui` is the whole per-frame pass and its ORDER is load-bearing:

1. `push_engine_rules`: read `allows_empty_mask()` and `constraints()` back from the engine into
   the frame — the engine may derive either from a parameter the user changed in this same
   frame's panel body, which ran earlier inside `CanvasView::draw`; a stale copy silently blocks
   or allows a run (§13.5) or validates against the previous model's size rules;
2. `RegionFrame::update` — the frame settles its rectangle and reports a `FrameOutcome`;
3. push `set_backend_available` / `set_torch_available` / `set_region` — after the pass, so the
   rectangle is THIS frame's (an engine may treat a moved rectangle as "a different image" and
   cancel its run). `set_region` also carries `geometry_settled` = `!drag_active()`, read after
   the pass, so the frame on which the pointer is released already reports settled;
4. act on the outcome: clear mask, cancel, process, apply, and last the mask generation the
   compact panel may have queued — `cancel_run` clears that flag, so a cancel and a generate
   clicked in the same frame resolve as the cancel;
5. `poll_region_load()`, `poll_mask_generation(ctx)`, then `poll_engine(ctx)`.

```
«Обработать» -> start_run()             capture the clean-overlay chunk, send a RegionLoadRequest
             -> poll_region_load()      the loader worker answers -> hand_region_to_engine()
             -> AiEngine::start()       the engine's own worker runs the model
             -> poll_engine() each frame -> Done -> accept_result() -> ResultLayer
«Сгенерировать маску» -> start_mask_generation() -> poll_region_load() (purpose MaskGeneration)
             -> start_mask_detection() -> poll_mask_generation() -> accept_generated_mask()
```

The source region is the SOURCE PAGE crop composited with the current clean overlay (D14),
produced by `base.rs::spawn_region_loader_thread`, reused rather than copied (D10); each host
instance owns one such worker and sends `None` and joins it on drop.

Host contracts:
- **Only the spec differs between hosted tools.** `tool_id`, `title`, `log_tag`, both id salts and
  `catalog` come from the `HostSpec`; a salt must be unique per tool (two tools sharing one share
  stored widget state) and frozen once shipped (changing it resets that state).
- **What is pushed when.** Mask layers reach the frame on construction (engine 0) and on
  `select_engine` ONLY; `constraints()` and `allows_empty_mask()` are re-read every frame (pass
  step 1, `push_engine_rules`; also on construction and `select_engine`); the backend/Torch flags
  and the rectangle are pushed every frame (step 3). Re-pushing mask layers per frame would be
  wrong: `set_mask_layers` refuses and logs on a locked frame. A per-frame constraint push is
  free because `set_constraints` only re-validates.
- **Apply validates the size and refuses (D7).** `check_result_fits` rejects a wrong size or an
  out-of-overlay rectangle before `replace_overlay_region_px` could rescale or clip; the same
  check runs on the engine's answer (`accept_result`) and on the loaded region
  (`hand_region_to_engine`). Never relax any of them into a rescale.
- **The engine picker exists only for a choice.** A one-engine catalog draws no picker (and no
  separator after it): `engine_picker_shown` in `host_panels.rs`, the one rule; the empty-catalog
  error line is still drawn.
- **The engine picker is disabled while the frame is locked (D15)** — a switch re-creates the
  mask stack — and while the SELECTED engine's `switch_block_reason()` stands (work no frame
  state describes, e.g. a model download). The frame lock is tested first and keeps its wording;
  `select_engine` refuses on the same answers, so the control and the action cannot drift apart.
- **A switch never resizes the frame.** A rectangle the new engine refuses turns red, blocks
  «Обработать» and gets its requirements spelled out in the main panel (`constraint_lines`, one
  sentence per declared rule, plus the nearest legal size from `snap_size` when one exists).
- **A per-frame push is not permission to work per frame.** `set_region` carries
  `geometry_settled`; an engine may DISPLAY an unsettled rectangle but must not start work off it.
- **The engine is polled every frame, panel visible or not** — `poll` is where an engine drains
  its channels and runs its settings saver. A test pins it.
- **`pytorch_required()` follows the SELECTED engine**: the tab gates the tool button on it.
- **Mask generation belongs to the host; what the mask MEANS belongs to the engine.** The block is
  drawn for every engine and says nothing about meaning; `../mask_generation.rs` owns the
  sources, rules, worker and validation. A generated mask enters through
  `MaskStack::set_active_from_alpha` into the SELECTED layer (one undo step) and is refused, never
  rescaled, on a size mismatch. `mask_generation_block_reason` is read by the button AND the
  click; a detection locks the frame for its whole duration and `cancel_run` drops its receiver.
  It is started through the `spawn_detection` field so tests can stub the real detector.
- **A stale load answer is dropped on its job id**, and `PendingLoad::purpose` says whether a
  load is for the run or for a detection — the loader has one slot for both.
- **A missing clean overlay is `None`, not a fabricated chunk**; once an overlay exists, a failed
  or wrong-size capture is a `CaptureError` with a user message and a log.
- **User message and technical detail are separate.** `report_error` shows a localized sentence
  and logs `"{log_tag} {text} | {detail}"`; a technical reason never enters a translated string.
- **`block_canvas_zoom()` stays `false` (D5)** and `wants_primary_stroke` is `false`; a test pins
  the former (`the_area_editor_never_blocks_canvas_zoom_...`).
- **Nothing blocks the GUI thread**: the only per-frame work is capturing an in-memory overlay
  chunk when a job starts.
- Every `t!` key of the host lives under `cleaning.tools.area_editor.*` (generation-block keys are
  the mask editor's own `cleaning.mask_editor.*`); the dock tab caption is
  `cleaning.tab.area_editor_tab`.
