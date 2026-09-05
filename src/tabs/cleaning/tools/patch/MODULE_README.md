# Module: src/tabs/cleaning/tools/patch

## Purpose
The «Заплатка» cleaning tool — Photoshop's Patch Tool. The user draws a free-form (lasso) or
rectangular selection directly on the page canvas; the closed selection stays live on the canvas
for the whole session; dragging from inside it onto a clean SOURCE area copies those pixels into
the selection and colour-adapts them to the destination's contour by a gradient-domain (Poisson)
solve. The result is committed into the clean overlay as ONE undo step.

It belongs to the FIRST tool category, next to the brushes (`BRUSH_TOOL_INDICES` in `../../tab.rs`),
and is NOT a region-editor tool: no floating window, no main dock panel.

## Architecture

### The maths
With `Ω` the selection, `g` the source pixels and `f*` the destination surroundings, seamless
cloning minimizes `∫∫_Ω |∇f − ∇g|²` subject to `f|∂Ω = f*|∂Ω`. Substituting `f = g + u` turns that
into `Δu = 0` inside `Ω` with `u|∂Ω = (f* − g)|∂Ω`: the correction `u` is the harmonic "membrane"
spanned by the boundary difference. A harmonic function is low-frequency, so the source's texture
survives intact while its colour and luminance are pulled onto the destination's.

The Laplace solve reuses `../gradient.rs`'s shared red-black SOR kernel (`red_black_sor_sweeps`,
`pub(super)`); `membrane.rs` is its second consumer and only builds the `lam`/`denom`/`u0` buffers
and the iteration schedule around it. `lam = 0` inside `Ω` is the harmonic average; a large `lam`
outside it is the soft Dirichlet pin.

Plain SOR needs iterations on the order of the region's DIAMETER, which would stall a worker for
seconds on a large selection. The schedule is therefore a CASCADIC MULTIGRID: a pyramid built by
2x downsampling until the shorter side reaches `COARSEST_MIN_DIM`, the coarsest level solved with
`COARSEST_SWEEPS`, then bilinear prolongation plus `SWEEPS_PER_LEVEL` smoothing sweeps on each
finer level. No V-cycles.

### Data flow of one patch
1. `stroke_begin` decides the gesture with `gesture_for_press`: inside the current selection starts
   the SOURCE drag, anything else starts a new selection.
2. `stroke_end` PARKS the finished gesture in `pending_end` and decides nothing (see the contract
   below). `resolve_pending_end` then either commits the drawn polygon as the selection or parks
   the drag offset in `pending_offset` — or cancels the gesture.
3. The next `draw_overlay_ui` consumes that offset in `start_job`: it measures the ROI, captures the
   clean-overlay chunk and sends TWO requests to `base.rs`'s SHARED region loader.
4. `poll_region_loads` waits for both answers and spawns the solve worker.
5. `run_patch_job` (worker) validates every ROI-sized buffer, rasterizes the polygon with
   `crate::tools::fill_polygon_spans`, builds the `PatchRequest`, calls `solve_patch`, and turns the
   result into a clean-overlay chunk through `base::overlay_pixel_for_final_color`.
6. `poll_solve` re-checks the chunk against the LIVE overlay (`check_chunk_fits`) and writes it with
   `CanvasView::replace_overlay_region_px`, which syncs the shared model and records the single undo
   diff by itself. No full-page commit follows.

### Why TWO region loads
The membrane needs the COMPOSITED destination (page + clean overlay); `overlay_pixel_for_final_color`
needs the ORIGINAL page pixel under it as its `base`. `RegionLoadRequest` answers one or the other
depending on whether an `overlay_chunk` is supplied, so the tool asks the same loader twice for the
same rectangle. The loader caches the decoded page per worker and through `CleanOverlaysModel`, so
the second request costs a crop, and both answers are derived from one rectangle and therefore
cannot disagree on size.

## Files and submodules
- `mod.rs`: the `CleaningTool` implementation — state machine, gesture handling, geometry and its
  refusal rules, the two worker hand-offs, the dashed-outline painting, and the `draw_ui` controls.
- `membrane.rs`: the maths. GUI-free and I/O-free (`PatchRequest` in, `PatchResult` out), fully
  unit-tested, and the only place the pyramid, the Dirichlet weight and the iteration counts live.

## Contracts and invariants
- **`block_canvas_zoom()` is `false`, always.** It also disables the clean-overlay Ctrl+Z /
  Ctrl+Shift+Z shortcuts (`../../tab.rs::handle_history_hotkeys`), which is acceptable for a modal
  editor window but not for a surface that lives on the canvas for a whole session. Pinned by a test.
- **`captures_canvas_pointer()` is `false`, always.** The tab ORs it into `canvas_pointer_occluded`
  and then calls `finish_stroke()` and returns, so a capturing tool receives no stroke, key or
  cursor callbacks — and this tool's entire gesture rides those. Pinned by a test.
- The outline is painted from `draw_overlay_ui` through `ctx.layer_painter(LayerId::new(Order::Middle, …))`,
  never from `draw_cursor`, which is pointer-gated and would make a session-long selection blink out.
  A bare layer painter registers no interactable `Area`, so it steals no canvas input. Its dashes are
  STATIC — no phase, and a repaint is requested only while a job runs — so they do not march.
- **A `stroke_end` is NOT proof of a pointer release.** The tab issues one as half of the end+begin
  pair it uses when a drag crosses onto another page (`../../tab.rs::handle_active_tool_input`), and
  one when the pointer leaves the canvas rectangle with the button still held. For every other
  cleaning tool that is harmless ("end this page's stroke"); for this one it would mean "commit".
  `stroke_end` therefore only PARKS the gesture, and `resolve_pending_end` acts on it where the
  frame's real pointer state is available — the following `stroke_begin`, or `draw_overlay_ui`.
  Pinned by two tests.
- A gesture that crosses onto a DIFFERENT page is cancelled with a localized status and a log line,
  never applied and never truncated: a selection and the source it is dragged onto must live on one
  page. A COMPLETED selection survives that cancellation; a lasso still being drawn is discarded.
- Dismissing the selection dismisses the WORK: Escape and the «clear selection» button both go
  through `clear_selection`, which calls `cancel_in_flight` before it drops the selection, the live
  gesture and any parked end. A surviving job would land a patch for a selection the user has
  explicitly dismissed, over whatever was drawn in the meantime, and would keep `busy()` true so
  the next selection is refused with `status_busy`. The two entry points cannot be fixed apart
  because they share that one method; both are pinned by a test.
- `deactivate` abandons everything in flight (`cancel_in_flight`). `draw_overlay_ui` runs for the
  ACTIVE tool only, so a job left running would finish unpolled and later land on an overlay the
  user has meanwhile changed — the hazard `ai_editor::cancel_run` exists for. The loader THREAD is
  not stopped there: it belongs to the tool, and `Drop` owns its shutdown.
- Before the write, `check_chunk_fits` re-validates the chunk against the page's LIVE overlay, not
  the one measured when the job started (`cleaning/MODULE_README.md`'s overlay-edit rule, and D7 in
  `../region_edit_v2/frame.rs`). `replace_overlay_region_px` REPAIRS instead of refusing — it clips
  the target rectangle and nearest-rescales the chunk into the remainder — so a page that changed
  under a running solve would otherwise be corrupted silently.
- The worker validates all three ROI-sized inputs ONCE, up front (`validate_job_input`), and refuses
  with a typed `PatchInputError`. Nothing is substituted for a buffer whose size it cannot explain:
  a transparent stand-in for the clean-overlay chunk would erase every pre-existing cleaning pixel
  of the ROI on commit. The per-pixel pass then zips pre-checked buffers, so it can never skip a
  pixel over a length disagreement.
- The selection is stored in PAGE pixels and re-projected every frame. A stored screen rectangle
  would drift the moment the canvas scrolls or zooms. It is always projected onto its OWN page, so
  it can never be painted over a different one.
- The page-pixel space is the clean overlay's, so `BrushToolBase::ensure_overlay_under_point` is
  called before the first vertex is taken; `page_source_size` (from `../region_edit_v2/frame.rs`)
  then reports the same size the loader crops against.
- A padded ROI that leaves the page is REFUSED with a localized message and a structured log. It is
  never clamped: a clamped ROI would silently sample pixels the user did not point at.
- **The outline never lies about the source.** The source-drag outline is projected with the
  UNCLAMPED page-pixel -> scene map (`scene_pos_in_page`, the point form of
  `../base.rs::overlay_rect_to_scene_rect`), not with `../base.rs::overlay_pos_to_scene_pos`, which
  CLAMPS and would pin a source dragged past the page edge to the border — showing pixels that are
  not where the patch would read from. A drag the release would refuse is stroked in the sibling
  region editor's invalid red (`FRAME_INVALID_COLOR`) instead of white, so the refusal is visible
  DURING the drag rather than after it.
- The refusal shown on screen and the refusal performed at the release are ONE predicate:
  `PatchTool::drag_geometry` (selection bounds -> `page_source_size` -> `roi_for`). `paint` reads
  only whether it answered, `start_job` takes the rectangle and the page size it answered with.
  Two copies of that rule would drift, and a preview disagreeing with the outcome is precisely the
  defect this factoring removes; pinned by a test that also makes the reason -> tone mapping total.
- A degenerate selection (fewer than three vertices, or a bounding box thinner than
  `MIN_SELECTION_SPAN_PX`) leaves NO selection behind, so a half-built outline can never be dragged.
- The drag offset is whole page pixels. The source is SAMPLED at `destination + offset`, and a
  fractional offset would need resampling, blurring the very texture the membrane exists to preserve.
- Pixels whose coverage is zero keep the EXISTING clean-overlay pixel: the commit replaces the whole
  ROI, so anything else would erase the surrounding cleaning work.
- The committed overlay is DENSE, not minimum-alpha, for the same reason `stamp.rs`'s current-page
  mode is: the page and the overlay are separate `TextureOptions::LINEAR` quads, so a low-alpha
  stencil ghosts the page back through itself. That is why the feather defaults to 0 and the
  selection's interior commits fully opaque.
- Every heavy step — decode, composite, solve — runs off the GUI thread. What `draw_overlay_ui`
  itself does is bounded and ROI-sized: resolve the parked gesture end, measure the geometry and
  copy the ROI-sized clean-overlay chunk (`start_job`), poll the two channels, blit one finished
  chunk into the overlay, paint the outlines, and request a repaint while a job is live. No
  full-page clone, conversion or diff happens there.
- The shared region loader's ownership contract is honoured: `Drop` sends `None` and joins, or the
  thread and the page it decoded outlive the tool.
- `membrane::solve_patch` refuses a contract violation instead of repairing it: a buffer-length
  mismatch, an ROI below 3x3, or a mask touching the ROI border. An EMPTY mask is a no-op, not an
  error.
- Registration is FOUR sites (`../mod.rs`, the `use` list in `../../tab.rs`, the
  `CleaningTabState::default` vector, and `BRUSH_TOOL_INDICES`); the partition test in `../../tab.rs`
  fails loudly if one is missed.

## Editing map
- To change when a source drag is refused — on screen and at the release alike — edit
  `PatchTool::drag_geometry` in `mod.rs`; it is the only place the rule lives, and `roi_for` is the
  only place its geometry does.
- To change the gesture, the refusal rules, the ROI geometry or the commit, edit `mod.rs`. How a
  gesture ENDS — committed, parked or cancelled — lives in `stroke_end` / `resolve_pending_end` /
  `stroke_begin` together; changing one without the other two breaks the release/crossing contract.
- To change the solve — the pyramid, the sweep counts, the Dirichlet weight, the blend modes or the
  feather ramp — edit `membrane.rs`. Every constant there is named and carries its rationale.
- To change the shared SOR kernel itself, edit `../gradient.rs`; both consumers must be re-verified.
- To change the polygon rasterization rule, edit `crate::tools::polygon_mask` — it is shared with the
  PS-editor's selection and its sampling rule is a contract, not an implementation detail.
- To change how the destination or the source pixels are loaded, edit `../base.rs`'s region loader;
  do not add a private decoder here.
