# Module: crates/ms-tab-ps-editor/src/tools

## Purpose
Tool subsystem for the PS-like editor. Defines the `PsTool` trait and the concrete tools the tab
routes pointer input to. The tab owns a `Vec<Box<dyn PsTool>>`, so adding a tool does not touch the
tab orchestration beyond registration.

## Architecture
`mod.rs` defines the contract:
- `PsToolId`: stable tool identity (toolbar selection + hotkeys).
- `PsToolSection`: toolbar grouping (`Brushes` / `Selection` / `Manipulation`). `PsToolSection::ORDER`
  is the toolbar's top-to-bottom order and `PsToolId::section` the assignment, so the toolbar renders
  headings without a hard-coded tool list; a new tool only picks a section.
- `PsToolContext`: per-frame mutable access to the active `LayerStack` and page `Selection`, plus
  the `page_idx` that stack is on, resolved pointer/button state in image pixel coordinates, the
  frame's `egui::Modifiers`, the two
  gesture-control keys (`cancel_pressed` = Esc, `remove_point_pressed` = Backspace/Delete), and —
  for gestures measured in DRAG DISTANCE rather than canvas position — the raw `secondary_down`
  button state and the screen-space `pointer_delta`. Those two are raw on purpose: the canvas senses
  `click_and_drag`, so `Response::dragged()` is delayed by the click/drag ambiguity and
  `drag_delta()` stays zero until it resolves, the same reason the tab's pan gate reads
  `middle_down`.
- `ToolOutcome`: what changed (`DirtyRect` in the ACTIVE LAYER's own pixels — the space a
  `TiledTexture` and the undo diff both use — plus `selection_changed`).
- `PsHotkeyRow`: one `(action, keys)` row of the tab's «Горячие клавиши» panel. Both halves are
  ALREADY LOCALIZED by the tool that built the row, so a language-dependent key name stays owned by
  the tool; the panel only lays the pair out as a two-column grid.
- `PsToolAction` / `ToolRegionWrite`: the tool → tab OUT-CHANNEL (see «Deferred tool actions»).
- `PsToolOverlayCx`: the page/view geometry `draw_overlay_ui` is handed (see «Region-tool hooks»).
- `PsTool`: `interact` (one frame of input), `draw_overlay` (screen-space cursor/preview),
  `has_options` + `options_ui` (the «Выбранный инструмент» panel's pair), `hotkey_rows` (the tool's
  own shortcut inventory), `gesture_in_flight` (REQUIRED, see below), the two gesture-lifecycle
  hooks `reset` / `freeze` (both default to a no-op), the `as_brush_mut` downcast hook used by
  the tab to forward the brush's wheel and key gestures without a full `Any` downcast, and the
  REGION-TOOL block — `poll_workers`, `take_actions`, `set_panel_rects`, `wants_main_panel`,
  `draw_main_panel`, `draw_overlay_ui`, `captures_canvas_pointer` — all defaulted to inert.

Tools never touch GPU textures, files, shared models, or the backend. They mutate the in-memory
stack/selection only; the tab converts `ToolOutcome::dirty` into `TiledTexture` re-uploads. A tool
that needs a pixel edit RECORDED and PUSHED (an undo entry, the shared `LayerDoc`, the `Клин`
write-back) asks for it through `PsToolAction` instead — see «Deferred tool actions» below.

The brush's own keys and its Alt+click eyedropper are dispatched by the TAB, not here — a tool never
sees an `egui::Context`, and the eyedropper needs the band Z order. See `../MODULE_README.md`; the
tool's side of the contract is that it stands aside (`suppress_stroke`) for the whole button hold
whenever Alt was down at the press. That covers Alt held BEFORE the press only, so the tab owns the
other half and refuses to sample while `gesture_in_flight` — Alt pressed in the MIDDLE of a stroke
would otherwise resample the colour every frame while the stroke re-composites with it. The
eyedropper and a stroke never both act on one gesture, but it takes both guards to say so.

**A gesture may not START outside the canvas viewport, but one already in flight may leave it.**
Since the tab's panels FLOAT over the canvas, `PsToolContext::pointer_in_viewport` is false wherever
a dock panel covers the pointer, and a tool that only checks the press frame paints under a panel:
the tab already suppresses `primary_pressed` off-viewport, so such a check is dead code and the
gesture starts anyway. Gate on the tool's own in-flight marker instead (`BrushTool::stroke`),
which blocks a start on EVERY frame while letting a stroke crossing a panel continue — the same rule
the tab applies to panning.

The tool's CURSOR PREVIEW obeys the same rule, and `PsTool::gesture_in_flight` is how it does.
`draw_overlay`'s pointer is dropped by the tab (`../mod.rs`, `overlay_pointer`) whenever the pointer
is occluded AND no gesture is running: a brush circle that keeps tracking under a panel advertises a
stroke the tools already refuse to start there. `gesture_in_flight` is REQUIRED with no default body
(like `hotkey_rows` and `has_options`) because only the tool knows what an accepted gesture is:
`BrushTool::stroke`, `SelectTool::gesture`, `TransformTool`/`DeformTool::drag` — never a per-frame
overlay cache (`gizmo`, `handles`) and never `SelectTool::last_pointer`, which outlives a commit.
Two answers must not be given: `true` for a press the tool itself REFUSED (the button is held there
too, which is why the tab must not substitute `primary_down` for this method — that is the exact bug
it was added to fix), and `false` for the lasso's PENDING POLYGON, which is a live gesture with the
button up and would otherwise lose its rubber band every time the cursor crossed a panel.

## Deferred tool actions (`take_actions`) — the tool's only durable write path
A tool mutates the in-memory stack and selection and NOTHING else. Everything that makes a pixel
edit survive is tab-side and a tool can reach none of it: the undo entry (`PsEditOp`), the push to
the shared `LayerDoc`, and the `Клин` write-back to `CleanOverlaysModel`. A tool that must commit
pixels therefore DESCRIBES the commit and hands it over.

`PsTool::take_actions(&mut self) -> Vec<PsToolAction>` is that out-channel: the tool owns the queue,
pushes into it from wherever the decision is made (`interact`, `poll_workers`, `draw_overlay_ui` —
the last two see no `PsToolContext`, which is why the queue is on the TOOL rather than a `&mut` slot
in the context), and the tab drains it once per frame in `PsEditorTabState::apply_tool_actions`
(`../mod.rs`). It is the same pattern as the layers panel's own `PanelActions` one level up: a body
that may not perform a contract-bearing mutation in place parks it for the owner.

Rules:
- **Drained from EVERY tool, not only the active one.** A worker result that landed after a tool
  switch still represents work the user did, and stranding it in the queue would silently lose it.
- **`take_actions` DRAINS.** Handing the tab the same request twice would apply it twice and cost
  two undo steps to take back one edit; the unit tests pin this.
- **Exactly ONE variant today**: `PsToolAction::WriteRegion(ToolRegionWrite)` — "write this pixel
  rect into the ACTIVE EDITABLE layer as ONE undo step, with this label". Add a variant only
  together with the tool that raises it and the tab arm that performs it (`CLAUDE.md` §14).
- **`ToolRegionWrite` geometry is in the ACTIVE LAYER's own pixels**, the same space as `DirtyRect`
  and as the undo diff — never in page pixels. `pixels` are PREMULTIPLIED RGBA (the `Layer::image`
  convention); `coverage`, when present, is one byte per source pixel and blends
  `dst += (src - dst) * coverage / 255` per premultiplied channel, so `None` and an all-255 buffer
  are bit-identical. A wrongly-sized coverage buffer is REFUSED and logged, never padded.
- **`page_idx` is mandatory and is checked.** The tab drops a request naming a page that is no
  longer resident: a worker result easily outlives a page switch, and landing it would paint one
  page's patch onto another. Stamp it from `PsToolContext::page_idx`.
- The tab's apply path clips the rect to the layer, marks the affected tiles dirty, and routes the
  commit through the one `commit_pixel_region` helper the brush stroke also uses — so a queued write
  is as undoable, as durable and as cross-tab-visible as a brush stroke, with no second copy of
  those rules.

## Region-tool hooks (`poll_workers`, panels, `draw_overlay_ui`, `captures_canvas_pointer`)
A REGION tool — one that owns a worker thread, an on-canvas surface and its own dock panel — needs
more than one frame of pointer input can express. The block at the end of the trait is what it uses.
Every hook is DEFAULTED to inert, so the four shipped tools implement none of them and are
unaffected.

- **`poll_workers(&mut self) -> bool`** — the tool's own channels, drained once per frame for EVERY
  tool from `PsEditorTabState::poll_tools`, next to `poll_loader`. The GUI thread never blocks
  (`CLAUDE.md` §5), so anything expensive runs on a worker and its result arrives here, never inside
  `interact` (which is skipped on a pan frame and on a frame the pointer sits over a panel).
  Returning `true` asks for the next frame — that is what makes an off-thread result appear without
  any pointer movement. It runs for INACTIVE tools on purpose: a job dispatched before a tool switch
  still finishes.
- **`set_panel_rects(&[Rect])`** — this frame's dock-panel rects in screen points, pushed by the tab
  right after `PanelDock::end` and before the canvas is drawn, so a tool that PLACES something on
  the canvas can cut the floating panels out of the viewport. Main-window rects only, by
  construction. Pushed to the ACTIVE tool, which is the only one that draws.
- **`wants_main_panel` / `draw_main_panel`** — the tool's own dock tab (`ps_editor.tool_panel`,
  declared every frame by the tab, `visible` driven by this answer, captioned with the tool's own
  `title()`). The body may mutate the TOOL and nothing else: it runs inside the dock frame, before
  the canvas exists this frame, so a button there raises a flag the tool acts on in its next
  `draw_overlay_ui`. Ordinary parameters still belong in `options_ui`, shortcuts in `hotkey_rows`.
- **`draw_overlay_ui(&mut self, &egui::Context, PsToolOverlayCx)`** — the on-canvas pass, run LAST
  in `draw_canvas` for the ACTIVE tool only. Unlike `draw_overlay` (a painter-only decoration on the
  canvas' background layer) it owns the `Context`, so it may open its own `egui::Area`, sense
  pointer input there and paint through `Context::layer_painter`. `PsToolOverlayCx` carries exactly
  what an on-canvas surface needs to place itself and nothing else: `viewport` (the canvas rect,
  the outer bound an `Area` must stay inside), `view` (this frame's image↔screen transform),
  `page_size` and `page_idx`. The layer stack is deliberately absent — pixel work is QUEUED. It runs
  only while a page is resident, which is exactly why worker results are consumed in `poll_workers`
  instead.
- **`captures_canvas_pointer(Pos2) -> bool`** — the tab ORs this into `canvas_pointer_occluded`
  (`../mod.rs`), so a `true` withholds the wheel, the zoom anchor and the routing to `interact`, and
  hides the tool's own cursor preview (the same gate). Two rules:
  - **On-canvas geometry is stored in PAGE pixels and re-projected every frame.** A stored screen
    rect drifts the moment the canvas pans or zooms — the same rule the cleaning tab states for its
    own on-canvas surfaces.
  - **It must stay NARROW, and must never grow a whole-canvas sibling.** The cleaning tab paid for
    that: its `block_canvas_zoom` flag also disables the clean-overlay undo shortcuts, which is
    tolerable for a modal window and not for a surface that lives on the canvas for a whole session.
    Here undo/redo runs in `PsEditorTabState::handle_hotkeys`, which never consults the canvas gate,
    so this hook cannot disable them — and nothing that can may be added beside it. Panning is
    likewise ungated, for the reason the tab already states: a pan begun on bare canvas must survive
    the pointer crossing anything.

## The patch tool (`patch.rs`) — the editor's first REGION tool
`PatchTool` implements `PatchHost` for `crate::tools::patch::PatchToolCore`. The core owns everything
about the patch itself and stops at `(page_idx, roi, rgb, coverage)`; this file decides where those
pixels are stored, and nothing else. It is the SECOND host of that core — `tabs/cleaning/tools/patch`
is the first — so a change to the gesture, the geometry or the solve belongs in the core and affects
both, while a change to storage belongs here and affects only this editor.

**The host is split in two, across a frame boundary.** `PsTool::draw_overlay_ui` — the hook the
core's whole frame pass runs from — is handed no `PsToolContext` and therefore no `LayerStack`,
deliberately (see «Region-tool hooks»). So:
- the GEOMETRY hooks (`page_source_size`, `page_projection`, `usable_viewport`,
  `scene_pos_to_page_pos`, `ensure_page_pixels`) answer from a `PageGeometry` cached on every frame
  that has one — the same per-frame overlay-cache pattern `BrushTool` uses for its cursor;
- the PIXEL hooks (`start_region_load`, `commit_patch`) PARK their request in `HostState` and the
  next `interact` services it against `&mut LayerStack`. One frame of latency on an operation that
  is already asynchronous — the same trade the tab makes for `apply_tool_actions`.
- `discard_region_load` drops only the LOAD's buffers. It must NOT drop a parked COMMIT: the core
  calls it immediately after every `commit_patch` it accepted, so sweeping the commit up there
  would silently lose every patch. `pending_commit` is cleared by `start_region_load` and by
  `reset` only, and `reset` REPORTS what it drops — the one-frame window in which a tool or page
  switch can still lose a solved patch.

**The region load has no worker thread, on purpose.** The cleaning host needs one because it decodes
a PNG from disk; here the pixels are already in memory, so `service_pending_region` composites the
ROI synchronously with `composite_rect` (`../mod.rs`). The heavy step — the membrane solve — is the
core's own worker, so `CLAUDE.md` §5 holds. The ROI is the selection box plus the drag offset, padded:
bounded, and small relative to the solve it feeds.

**THE SOLVE PLANE, and why it must agree with the backdrop.** A patch is solved in the plane its
TARGET LAYER is part of — the visible layers from the bottom up to and INCLUDING the active one
(`CompositeBound::UpTo`, `../mod.rs`) — and stored over the plane strictly BELOW it
(`CompositeBound::Below`, `backdrop_rect`). The two are complementary by construction: the store
step solves each pixel so that "the active layer over the backdrop" shows the colour the membrane
asked for, and the tab then composites everything ABOVE the active layer on top, exactly as it does
for any other pixel of that layer. Handing the solver the WHOLE visible stack instead would copy
the layers above the target into the target, and the render would paint them a second time over
their own copy — a layer applied twice on screen, reachable in the default state because every user
raster sits above `Клин`. Working in the active layer's own plane is also what Photoshop's Patch
Tool does: it patches the active layer, not a flattened view. The two bounds are named variants
rather than an index arithmetic at the call site precisely because they are one index apart and
mean opposite things.

**The store step uses the shared `crate::tools::overlay_pixel_for_final_color`**, the same solver the
cleaning host's store step uses, whose `base` is the BACKDROP — the composite of everything strictly
BELOW the write target (`backdrop_rect`):
- active layer `Клин`: the backdrop is `Исходник` and nothing else, read straight out of
  `stack.layers()[0]`. Its VISIBILITY is ignored — hiding `Исходник` in this editor is a view toggle,
  while the clean overlay's storage contract ("these pixels sit over the page source") is not.
- active layer a user raster: the ordered composite of the layers visible below it.
The solved pixel is premultiplied (egui's `Color32::from_rgba_unmultiplied` premultiplies), which is
`ToolRegionWrite::pixels`' convention. The returned coverage is BINARY — 255 where the patch claims
the pixel, 0 where it does not: the feather is already inside `rgb` and inside the solved alpha, so
a second blend would attenuate it twice, and the zero bytes are what leave the surrounding work
untouched.

**Six refusals, each with its own message, none of them silent** (`CLAUDE.md` §6). `ToolRegionWrite`
geometry is LAYER-local while the patch works in PAGE pixels, so the two grids must coincide up to a
whole-pixel translation. `patch_target_for` (pure, one case per variant in the tests) refuses: no
active layer; `Исходник`; an INVISIBLE active layer (hidden, in a hidden group, or at zero effective
opacity — the same predicate the composite filters on, so the solve plane always contains the layer
being written); a raster with an unbaked effects chain; a `deform` mesh (following `transform.rs`,
which refuses for the same reason); and a rotated, scaled or fractionally-placed layer.
`PatchTarget::local_origin_for` is the sixth — an ROI that leaves the layer's own footprint — kept
separate because it needs an ROI, which does not exist while the options pane renders the verdict.
The reason is shown in the tool's own options pane, so a drag that does nothing can always be
explained. The DEFAULT active layer of a freshly loaded page is `Клин`, which passes all six.

**What it does NOT implement, and why.** `wants_main_panel` / `draw_main_panel`: its controls are
ordinary parameters and belong in `options_ui` (which delegates to the core's `draw_ui`).
`set_panel_rects`: the dock panels float on `Order::Foreground` while the core paints its outline on
`Order::Middle`, so a panel already covers the outline by z-order and `usable_viewport` returns the
plain canvas rect. `captures_canvas_pointer`: it stays `false` because the whole gesture rides
`interact`, which a `true` would withhold — the same reason the cleaning host keeps its equivalent
`false`. `freeze`: the gesture END is detected as a LEVEL (`!primary_down`) on every routed frame,
like `BrushTool`'s Shift anchor, so a release swallowed by a canvas pan is still seen.

## Shortcut inventory (`hotkey_rows`) — required, never defaulted
`hotkey_rows` has NO default body on purpose. A tool is the only place that knows which keys and
mouse gestures it interprets, so a new tool is forced by the compiler to state them; a defaulted
empty list would silently ship an undocumented tool, and that is exactly the failure this method
exists to prevent. Returning an empty `Vec` is the explicit way to say "this tool has none".

**Hint text lives in `hotkey_rows`, never in `options_ui`.** `options_ui` is for controls that
change a parameter (the brush colour/diameter/hardness/opacity/flow/erase, the selection
`mode_row`); a `ui.label` that
merely describes a key or a drag belongs in a row instead, because the panel renders rows as a
two-column grid and a sentence like "Shift adds, Alt subtracts" cannot be laid out there. Split
such a sentence into one row per shortcut. Both halves of a row must be non-empty — the unit tests
in `mod.rs` assert it against the reference (`en`) catalog, so an empty translation there fails
that test. A missing translation in another catalog falls back to `en` and is not caught here.

Rows are rebuilt on every call, so a runtime language switch is reflected without any invalidation.

## Parameters (`has_options` / `options_ui`) — the pair, and which half is required
`has_options` is REQUIRED with no default body, for the same reason as `hotkey_rows`: only the tool
knows whether it owns a parameter, so a new tool is forced by the compiler to answer. It is the
«Выбранный инструмент» panel's ONLY question — a `false` makes the panel print
`ps_editor.active_tool.no_options` and never call `options_ui`.

`options_ui` therefore DEFAULTS to a no-op, and a tool answering `false` does not implement it at
all. `TransformTool` and `DeformTool` are exactly that case: they have no parameters, only
positional gizmo drags, and those are `hotkey_rows`. `BrushTool` (colour / diameter / hardness /
opacity / flow / eraser) and
`SelectTool` (the persistent combination mode) answer `true`.

Answer `true` only for a control that CHANGES a parameter. A label describing a key or a drag is a
row, not an option — see the section above.

Scope boundary: a tool lists only keys it handles ITSELF. Tab-level shortcuts (the B/M/L/V tool
letters, Ctrl+D, undo/redo, pan, zoom) belong to `PsEditorTabState::handle_hotkeys` in `../mod.rs`
and are appended by the panel, not by a tool.

## Gesture lifecycle (`reset` / `freeze`)
A gesture can span many frames and can end with the button UP (the pending lasso polygon), so the
tab — not the tool — owns the two transitions the tool cannot observe:
- **`reset`**: abandon the in-progress gesture; the tool must be usable again immediately, and the
  committed page selection must be left exactly as it was. Abandonment is not free for a tool whose
  gesture already changed PIXELS: see the brush's `end_stroke_for_commit` below, which the tab runs
  before every `reset` so painted pixels are never left without an undo entry. Called on a tool switch (the OUTGOING
  tool, from `PsEditorTabState::set_active_tool`, the single writer of `active_tool_idx`), on a page
  switch (`request_page`), and on Esc pressed while input is suppressed. Without it a pending
  outline commits minutes later, with a stale combination mode and — across a page switch — the
  previous page's coordinates.
- **`freeze`**: called once per frame whose canvas input went to a pan or a text-layer drag instead
  of the tool. Those frames hide the button events, so the next routed frame can see "button up, no
  release" and mistake a swallowed release for an interrupted drag. `SelectTool` records the
  suppression and commits on resume instead of aborting; the abort branch stays reachable for the
  real case (focus loss, another widget grabbing the button). Tools whose gesture survives a
  suppressed frame unharmed keep the default no-op, so panning does not change their behavior.
  `BrushTool` is the third answer: it ENDS the stroke there. Its pixels are already in
  `layer.image`, so there is nothing to resume, and the release the pan swallowed would otherwise
  never be observed — see the commit contract below.

`BrushTool::reset` drops the stroke buffer and the chain anchor (otherwise the next stamp draws a
segment from another page); it cannot restore the pixels already composited — see the brush model
below. `TransformTool` / `DeformTool` drop their control drag and per-frame caches.

## Files and submodules
- `mod.rs`: trait, context, outcome, and `PsToolId`.
- `brush.rs`: `BrushTool` — Photoshop-like round brush on the active editable raster layer, clipped
  by the selection. Owns its own parameters (it no longer uses `crate::tools::MaskBrush`, whose
  radius-only integer sizing cannot express an odd diameter).
- `select.rs`: `SelectTool` — rectangle marquee or freehand/polygonal lasso (one struct,
  `SelectMode`), building the page `Selection` with a Photoshop combination mode.
- `transform.rs`: `TransformTool` — move / rotate / uniform scale of the active raster layer's
  `LayerTransform` (no pixels, so no tile re-upload); base and deformed layers are refused.
- `deform.rs`: `DeformTool` — grid-point drag over the active raster's `deform` mesh (page px),
  initializing an identity grid on first use.
- `patch.rs`: `PatchTool` — the «Заплатка» region tool. A HOST for the shared, host-neutral core in
  `crate::tools::patch`; the gesture, the selection, the ROI geometry, the membrane solve and the
  outline painting all live there. See «The patch tool» below.

## Selection gesture model (`select.rs`)
- **Combination mode.** `SelectionOp` (`super::selection`) decides how a finished shape meets the
  existing mask: `Replace` / `Add` / `Subtract` / `Intersect`. `base_op` is the persistent options-row
  choice; `gesture_op(base, mods)` overrides it per gesture — Shift = add, Alt = subtract,
  Shift+Alt = intersect. It is sampled **once, on the press frame**, and never re-read: Alt held
  BEFORE the press means "subtract", Alt held AFTER it means "straight segment", and only a
  press-time sample can tell the two apart. The two meanings must not overlap, so `Gesture::alt_armed`
  gates the second one: Alt already down at the press stays the mode latch until it is RELEASED once
  inside the gesture. Without that gate an Alt-drag subtracts in polyline steps instead of freehand —
  the one place Photoshop's own key assignment is ambiguous.
- **State machine.** `SelectTool::step(GestureInput) -> GestureAction` is pure (no `PsToolContext`,
  no `Ui`, no `Selection`) and owns the whole lifecycle; `interact` is a thin adapter that performs
  the commit. States: idle → dragging (freehand sampling every `FREEHAND_MIN_STEP` image px, skipped
  while Alt is held AND armed) → either finished, or **pending polygon** (release with Alt: an anchor is
  committed and the outline stays alive with the button UP; a further press adds an anchor, releasing
  Alt closes the path). Esc aborts without touching the selection; Backspace/Delete pops the last
  lasso vertex and aborts at zero. `Gesture::rubber_band` is recomputed every frame because
  `draw_overlay` receives no modifiers.
- **A click is not a special case.** A degenerate rect / a sub-3-point polygon is committed like any
  other shape: `Replace` clears the mask (click to deselect), the other ops leave it alone. Do not
  add an early return for it.

## Brush model (`brush.rs`)
- **Sizes are DIAMETERS in page px** (`MIN_DIAMETER..=MAX_DIAMETER`), not radii: Photoshop sizes by
  diameter, and a radius-only integer parameter cannot reach an odd diameter at all. A stamp centre
  is snapped to a pixel CENTRE for an odd diameter and to a pixel BOUNDARY for an even one, so
  diameter 1 paints exactly one pixel and diameter 2 a symmetric 2x2.
- **WHICH stamps are snapped is a contract with the cursor** (`stamp_step`, `snap_center`). The
  stroke's opening stamp sits on the pointer sample the pixel-exact outline previews, so it is
  snapped at every diameter — a click therefore paints exactly the outlined pixels. The stamps the
  arc-length walk interpolates are snapped only up to `SNAP_ALL_MAX_DIAMETER` (2.5 local px), where
  the grid contract above still depends on it; rounding them at a wider tip only quantizes the walk
  and ripples a thin diagonal stroke. Relaxing the snap for the opening stamp would invalidate the
  outline cache, which is built in a frame centred on `snap_center(0.0, odd)`.
- **One coverage formula** (`stamp_coverage`), with the tip's outer extent at `R + 0.5`:
  `core = 1 - (r / extent)^(1 / (1 - hardness))`, `edge = clamp(extent - r, 0, 1)`,
  `coverage = min(clamp(core, 0, 1), edge)`. The `edge` term IS the antialiasing — a one-pixel rim
  no hardness can remove, and the whole profile at hardness 1, where the exponent is floored by
  `HARDNESS_EPS` instead of diverging. `core` is PLATEAU-FREE below hardness 1, which is the point:
  a flat full-coverage core hands a centreline pixel an integer number of saturated stamps, and that
  integer flips with the walk phase and beads the stroke. There is no supersampling. The cursor
  marks the 50 %-coverage contour (`coverage_radius_50`, the closed-form inverse of the same
  kernel), not the outer extent.
- **A stroke accumulates in an f32 alpha buffer, never directly in the layer.** Stamps are placed
  along the pointer path at constant arc length (`stamp_step`: `spacing = max(0.10 * d, floor)`,
  deliberately NOT exposed) with the leftover distance carried ACROSS frames, and each stamp does
  `A' = A + flow * coverage * (opacity - A)` while `A < opacity`. That is what keeps a self-crossing
  stroke from darkening where it overlaps itself.
- **The spacing is 10 % of the diameter, NOT Photoshop's 25 %, and only reciprocals of whole numbers
  are usable.** A centreline pixel's alpha is a sum of tip profiles sampled once per spacing, so an
  edge falling off over less than one spacing aliases: the number of contributing stamps flips by
  one as the phase slides, and at a low flow that is a visible periodic scallop. Measured worst-case
  ripple over hardness x flow x diameter: 1.27x at 25 %, 1.07x at 12.5 %, 1.04x at 10 %; at
  hardness 1 the tip is a rectangle of width `d` whose stamp count is constant exactly when
  `1 / SPACING_RATIO` is integral (1/6 measures 1.00x where 1/6.67 measures 1.16x). The bound is
  asserted by `the_stroke_centreline_has_no_periodic_ripple`. Cost is linear in `1 / SPACING_RATIO`.
  The floor is one local px for a snapped tip (a finer step cannot move it) and half a px otherwise.
- **The Shift straight line is scoped to ONE Shift hold, and that is NOT Photoshop's rule.**
  A stamp becomes the line anchor (`last_stamp_world`) only if Shift was held when it was placed;
  the anchor survives while Shift stays down — so Shift+click, Shift+click chains segments — and is
  dropped the moment Shift comes up, so the next Shift+click starts fresh instead of drawing back
  across the page. A stroke made without Shift leaves no anchor at all. Photoshop chains from the
  previous stroke's last stamp whatever the modifiers did in between; the tighter scope was chosen
  deliberately, so do not "restore" the Photoshop behaviour. The release is detected as a LEVEL on
  every routed frame (an edge the tool never saw would leave a stale anchor), and `freeze` drops the
  anchor outright because a suppressed frame carries no modifiers at all. This is separate from the
  Shift AXIS constraint during a drag, which is re-read every frame from the stroke's own press
  anchor (`StrokeState::anchor_world`).
- **The buffer composites over a per-stroke SNAPSHOT of `layer.image`, not over `base_image`.** The
  two are equal at stroke start (a paintable layer carries no effects, so `image == base_image`),
  but the snapshot does not depend on when the tab re-syncs `base_image` from the shared doc after a
  commit — so a second stroke can never composite over stale pixels and undo the first. `base_image`
  is never written here; the tab's stroke commit still reads the undo "before" from it.
- **The buffer is SPARSE: fixed 128x128 tiles keyed by tile coordinate**, allocated on first touch
  and never grown, moved or copied. Its memory therefore tracks the stroke's footprint, not its
  bounding box. Neither alternative is usable on a ribbon page (~800x19000 px): a layer-sized buffer
  costs a visible memset up front, and a growing bounding box reaches ~137 MB for one long diagonal
  stroke and memcpys all of it on the GUI thread every time it grows, which §5 forbids. The
  selection clip is rasterized ONCE per tile instead of re-tested per stamp, and its presence is
  latched at stroke start so a selection dropped mid-stroke cannot un-clip the second half.
- **`dirty` is THIS FRAME's segment box, never the growing stroke union.** The tab re-uploads whole
  1024 px tiles on a per-frame budget; a growing union exhausts it. The tab accumulates the union
  itself (`brush_stroke_dirty`) for the undo bound. Boxes are in the LAYER's own pixels, like every
  `DirtyRect`.
- **A finished stroke is announced by a LATCH, not by the pointer release** (`stroke_finished`,
  consumed through `take_stroke_finished`). The tab's commit — the undo entry plus the push to the
  shared doc or `CleanOverlaysModel` — hangs off it. A release delivered on a frame that went to a
  canvas pan never reaches `interact` at all, so a commit keyed on the raw release edge is simply
  never run: the stroke would get no undo entry and a `Клин` stroke would never reach the shared
  overlay model, vanishing on the next page switch. Exactly two transitions set the latch, both
  through `end_stroke`: the first frame with the button up, and `freeze`. `reset` deliberately does
  not, so a commit can never fire on a later frame against a page that has already been replaced.
- **Abandoning a painted stroke COMMITS it first.** `reset` cannot take painted pixels back, so the
  tab calls `end_stroke_for_commit` before every abandonment (Esc under input suppression, the
  outgoing tool of a tool switch, a page switch) — same `end_stroke`, same one-shot latch, so a
  stroke still commits exactly once. An idle brush latches nothing there, which is what keeps the
  lasso's Esc — abandon the pending polygon, commit nothing — behaving exactly as before.
- **`reset` cannot restore pixels** — it receives no layer access — so it drops the stroke buffer and
  leaves the pixels already composited into `layer.image` exactly as the user saw them painted. This
  is reachable only through a tool/page switch or a suppressed-frame Esc, and `base_image` is
  untouched either way. Each of those callers commits the stroke before it gets here, so what
  `reset` drops is an already-committed (or never-painted) buffer.
- **The hardness kernel is deliberately NOT shared with `tabs/cleaning/tools/base.rs`.** That brush
  has its own hardness implementation with a different ACCUMULATION CONTRACT: it combines coverage
  into a mask with `max()` for an opaque fill, where this one lerps toward `opacity` at `flow`. A
  shared kernel would have to satisfy both and would silently change one of them; the duplication is
  a decision, not an oversight.
- **The Alt+click eyedropper does not sample TEXT overlays.** It samples the visible RASTER composite
  (`PsEditorTabState::sample_visible_composite`, `../mod.rs`) because text is drawn by the typing
  renderer and has no `Layer` buffer to read. Accepted limitation.

## Brush cursor contract (`brush.rs`, `draw_overlay`)
- **The cursor outlines PIXELS, not a circle.** It traces the boundary of
  `{ pixel : stamp_coverage(..) >= 0.5 }` — the same 50 %-coverage contour the smooth ring used to
  draw, resolved per pixel, so per-pixel work at a high zoom shows exactly which pixels a stamp
  lands on. That set is asked of `stamp_coverage` and placed with `snap_center` at the position the
  paint path gives the stroke's opening stamp — the very functions the paint path uses, and the one
  stamp it snaps at every diameter. **Neither may be re-derived for the cursor**: a second copy of the formula
  drifts, and a drifted cursor advertises pixels the brush does not paint — the failure this
  outline exists to prevent. `build_coverage_outline` and its helpers are pure and unit-tested
  against the kernel directly.
- **Two documented fallbacks to the smooth ring**, both because a staircase would otherwise lie or
  be unreadable: the active editable layer is rotated or scaled (the brush paints in LAYER-local
  pixels, which are then neither axis-aligned on screen nor one page pixel wide), or the view shows
  fewer than `PIXEL_OUTLINE_MIN_SCALE` screen px per image px. No editable layer is the same case.
- **`draw_overlay` cannot see the layer stack**, so `interact` captures the destination grid into
  `BrushTool::cursor_grid_origin` and the outline into `cursor_outline` — the per-frame overlay
  cache pattern `TransformTool` already uses for its gizmo. The frames the tab skips `interact` on
  (a pan, a text drag) cannot change a layer transform, so the cache cannot be seen stale; a cache
  whose key no longer matches the tip is ignored rather than drawn.
- **Cost is paid on parameter change, not per frame.** The outline is `O(diameter²)` coverage
  evaluations, cached by `(diameter, hardness)`, with collinear edges merged; an idle frame, a pan
  or a plain drag only remaps the cached vertices through the `ViewTransform`. The one gesture that
  changes a parameter every frame is the Alt + right-drag HUD, which therefore does rebuild once per
  frame while it runs — measured at ~0.3 ms release / ~1.9 ms debug at `MAX_DIAMETER`.
- **It is drawn as alternating black/white `Shape::line_segment` runs** through the marquee's own
  `walk_dash_runs` (`../mod.rs`), for two reasons: the phase must accumulate along the whole loop or
  a staircase of one-pixel steps paints solid, and only `tessellate_line_segment` snaps a line to
  the physical pixel grid — `Shape::closed_line` goes through `tessellate_path` and blurs. Do not
  replace the segments with a polyline.
- **A soft tip also gets an outer SMOOTH ring** at its nominal radius, marking the reach the 50 %
  contour hides. Smooth on purpose: the rendering difference is what tells the two contours apart.
- **The HUD gesture draws a diameter/hardness readout** (`ps_editor.tools.brush_hud_readout`);
  the cursor circle alone cannot show hardness.

## Contracts and invariants
- The brush only paints when `LayerStack::active_editable_mut()` returns a layer (locked base
  layers are skipped). Erasing scales the pre-stroke pixel's premultiplied channels down; painting
  is premultiplied source-over with the brush colour.
- Selection-clip: when a selection is active, pixels outside it are left untouched.
- **Empty means `None`.** `ensure_selection` allocates a page-sized mask eagerly, so every commit
  path must end in `PsToolContext::normalize_selection` (pure core: `normalize_selection_slot`),
  which drops an all-zero mask. A `Some(all-zero)` selection is invisible (`!any()` hides the
  marquee) yet still blocks the brush, which only tests `Option::is_some`. The rule holds for the
  tab too: every store into `PsEditorTabState::selection` goes through its `set_selection`.
- **A gesture never outlives its context.** `reset`/`freeze` above are part of the contract, not an
  optimization: a tool that keeps cross-frame state must implement them.
- `interact` must report the modified region via `ToolOutcome::dirty` so only affected tiles
  re-upload; reporting too small a rect leaves stale pixels on screen.
- Coordinates in `PsToolContext::pointer_image` are image pixels (fractional); tools round as
  needed. `draw_overlay` receives the `ViewTransform` to map image→screen.

## Editing map
- To change when the tool's cursor preview is hidden, edit `overlay_pointer` in `../mod.rs` (the
  pure decision, unit-tested there) and the tools' `gesture_in_flight`.
- To add a tool: implement `PsTool` (including the REQUIRED `hotkey_rows` and `gesture_in_flight`),
  add a `PsToolId`, give
  it a `PsToolSection`, register it in `PsEditorTabState::default`, and add it to the
  `registered_tools` fixture in `mod.rs`'s tests. Add a hotkey in
  `PsEditorTabState::handle_hotkeys` if wanted.
- To add a REGION tool (worker thread + on-canvas surface + own dock panel), implement the block
  above on top of that: `poll_workers` for its channels, `take_actions` for its pixel commits,
  `draw_overlay_ui` + `set_panel_rects` + `captures_canvas_pointer` for its surface, and
  `wants_main_panel` + `draw_main_panel` for its panel. Nothing in `../mod.rs` needs a new call
  site — every hook is already wired.
- To change what a tool may ask the tab to commit, edit `PsToolAction` / `ToolRegionWrite` here AND
  the matching arm of `PsEditorTabState::apply_tool_actions` in `../mod.rs`; the write itself must
  keep going through `commit_pixel_region`, never through a second copy of the undo/route rules.
- To change what the «Горячие клавиши» panel shows for a tool, edit that tool's `hotkey_rows` —
  never `options_ui`.
- To change brush behavior (colour, diameter, hardness, opacity, flow, erase, clipping), edit
  `brush.rs`; to change which KEY reaches it, edit `PsEditorTabState::brush_shortcuts` in `../mod.rs`
  AND `BrushTool::hotkey_rows` together.
- To change the brush cursor, edit `BrushTool::draw_overlay` / `draw_pixel_outline` in `brush.rs`;
  the geometry is `build_coverage_outline` and the fallback rules are `refresh_cursor_cache` plus
  `PIXEL_OUTLINE_MIN_SCALE`. Changing what the outline encloses means changing `stamp_coverage`,
  not the cursor.
- To change selection shapes or mask combination, edit `select.rs` and `super::selection`.
- To change the patch GESTURE, its ROI geometry or the membrane solve, edit `crate::tools::patch` —
  never `patch.rs`, which would only change this editor and leave the cleaning tab behind. To change
  where a patch is STORED, which layers may receive one, or what the backdrop is, edit `patch.rs`.
  To change how a page rectangle is composited for it, edit `composite_rect` in `../mod.rs`.
- To change how a gesture starts/ends (keys, Alt semantics, pending polygon, the frozen-resume
  branch), edit `SelectTool::step` — it is the single decision point, and its unit tests live
  beside it.
- To change when a gesture is abandoned or frozen, edit `PsEditorTabState::set_active_tool` /
  `reset_active_tool` and the canvas routing gate in `../mod.rs`, plus the tools' `reset`/`freeze`.
- The selection-mode row is a label plus a WRAPPING button row (`ui.horizontal_wrapped`): the tool
  panel is narrow and the four localized mode names overflow a single line in several languages.
