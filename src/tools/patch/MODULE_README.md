# Module: src/tools/patch

## Purpose
The host-neutral core of the «Заплатка» (patch) tool — Photoshop's Patch Tool. The user draws a
free-form (lasso) or rectangular selection directly on a page; the closed selection stays live for
the whole session; dragging from inside it onto a clean SOURCE area copies those pixels into the
selection and colour-adapts them to the destination's contour by a gradient-domain (Poisson) solve.

The core owns the gesture, the geometry, the refusal rules, the solve and the outline painting. It
STOPS at `(page_idx, roi, rgb, coverage)`. How those pixels are stored — over which backdrop, into
which layer, as which undo step — is the host's business and is deliberately absent from here,
together with every canvas, project and overlay type.

## Architecture

### The maths
With `Ω` the selection, `g` the source pixels and `f*` the destination surroundings, seamless
cloning minimizes `∫∫_Ω |∇f − ∇g|²` subject to `f|∂Ω = f*|∂Ω`. Substituting `f = g + u` turns that
into `Δu = 0` inside `Ω` with `u|∂Ω = (f* − g)|∂Ω`: the correction `u` is the harmonic "membrane"
spanned by the boundary difference. A harmonic function is low-frequency, so the source's texture
survives intact while its colour and luminance are pulled onto the destination's.

The Laplace solve reuses the project's shared red-black SOR kernel (`crate::tools::sor`);
`membrane.rs` only builds the `lam`/`denom`/`u0` buffers and the iteration schedule around it.
`lam = 0` inside `Ω` is the harmonic average; a large `lam` outside it is the soft Dirichlet pin.

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
3. The next `draw_overlay_ui` consumes that offset in `start_job`: it measures the ROI with
   `drag_geometry` and calls `PatchHost::start_region_load`.
4. `poll_region_load` asks the host for the composited destination and spawns the solve worker.
5. `run_patch_job` (worker) validates the ROI-sized composite, rasterizes the polygon with
   `crate::tools::fill_polygon_spans`, builds the `PatchRequest`, calls `solve_patch`, and blends
   the solution into the destination by its own coverage.
6. `poll_solve` hands the host a `PatchCommit` — `(page_idx, roi, rgb, coverage)` — and then calls
   `PatchHost::discard_region_load`, so nothing the host retained for the job outlives it.

### The host boundary
`PatchHost` has three groups of hooks and nothing else:
- geometry: `page_source_size`, `page_projection`, `usable_viewport`, `scene_pos_to_page_pos`,
  `ensure_page_pixels`;
- the region load: `start_region_load`, `poll_region_load`, `discard_region_load`;
- the commit: `commit_patch`.

A host that needs a storage BACKDROP under the ROI (both shipped hosts do, for
`overlay_pixel_for_final_color`) obtains it itself inside `start_region_load` / `commit_patch` and
retains it until the store step. The core never asks for it, because it has no consumer for it.

### The two hosts, and what each one requires
- `tabs/cleaning/tools/patch/`: stores into the clean overlay. Its backdrop is the ORIGINAL page,
  which it must DECODE, so it owns a loader thread and issues a second region load for it.
- `tabs/ps_editor/tools/patch.rs`: stores into the PS editor's active layer through
  `PsToolAction::WriteRegion`. Its pixels are already in memory, so it composites the ROI
  synchronously and needs no thread of its own. Being a LAYERED host, it must also answer the plane
  question the core does not ask: the region it hands over is the composite up to and INCLUDING its
  active layer, and its backdrop is the composite strictly BELOW it (see «The solve plane» below).
  Its `PsTool::draw_overlay_ui` hook is handed no layer stack at all, so it PARKS
  `start_region_load` and `commit_patch` and services them on its next `interact` — which is why
  `discard_region_load` must be understood strictly: it drops what the LOAD retained, and a host
  must not use it to drop a commit the core has already accepted (the core calls it immediately
  after every accepted `commit_patch`).

A third host must therefore satisfy two things this trait does not spell out in types: it must be
able to answer the geometry hooks from wherever the core calls them, and it must keep an accepted
`commit_patch` alive across the `discard_region_load` that follows it.

### The solve plane
The core is plane-agnostic: it asks for ONE ROI-sized composite and solves against it. Which layers
that composite covers is the host's decision, and it is the one decision a layered host cannot get
wrong without visibly corrupting the page. The rule both hosts obey:

**The region handed to the solver and the backdrop the result is stored against must describe the
same page, split at the target layer.** The region is everything the page shows up to and INCLUDING
the layer the patch is written into; the backdrop is everything strictly BELOW it. Anything above
the target is in NEITHER, because the renderer composites it over the stored result on its own — put
it in the solve region and it is copied into the target and then painted a second time over its own
copy. The cleaning host satisfies this trivially (the clean overlay is the top of its two-layer
model); the PS host names the two bounds `CompositeBound::UpTo` / `CompositeBound::Below`
(`tabs/ps_editor/mod.rs`) so the one-index difference between them cannot be written by accident,
and REFUSES a target layer that the composite would drop — a hidden or fully transparent active
layer — instead of solving in a plane that does not contain it.

## Files and submodules
- `mod.rs`: `PatchToolCore` — state machine, gesture handling, geometry and its refusal rules, the
  worker hand-off, the dashed-outline painting, `draw_ui`, and the `PatchHost` trait itself.
- `membrane.rs`: the maths. GUI-free and I/O-free (`PatchRequest` in, `PatchResult` out), fully
  unit-tested, and the only place the pyramid, the Dirichlet weight and the iteration counts live.

## Contracts and invariants
- **The core stores nothing.** Everything about storage happens on the far side of `commit_patch`;
  what crosses the seam is exactly `(page_idx, roi, rgb, coverage)`, `rgb` and `coverage` both
  `roi.w * roi.h` entries in row-major order. Pinned by a test with a fake host.
- Pixels whose `coverage` is `0.0` are NOT part of the patch: a host must leave whatever is already
  stored there untouched, because the commit replaces the whole ROI and anything else would erase
  the surrounding work. Their `rgb` entry is the destination colour and carries no information.
- **A `stroke_end` is NOT proof of a pointer release.** A host issues one when a drag crosses onto
  another page and one when the pointer leaves the canvas with the button still held. For an
  ordinary brush that is harmless ("end this page's stroke"); for this tool it would mean "commit".
  `stroke_end` therefore only PARKS the gesture, and `resolve_pending_end` acts on it where the
  frame's real pointer state is available — the following `stroke_begin`, or `draw_overlay_ui`'s
  `released` argument. Pinned by two tests.
- A gesture that crosses onto a DIFFERENT page is cancelled with a localized status and a log line,
  never applied and never truncated: a selection and the source it is dragged onto must live on one
  page. A COMPLETED selection survives that cancellation; a lasso still being drawn is discarded.
- Dismissing the selection dismisses the WORK: Escape and the «clear selection» button both go
  through `clear_selection`, which calls `cancel_in_flight` before it drops the selection, the live
  gesture and any parked end. A surviving job would land a patch for a selection the user has
  explicitly dismissed, over whatever was drawn in the meantime, and would keep `busy()` true so
  the next selection is refused with `status_busy`. Both entry points are pinned by a test.
- `cancel_in_flight` is reachable from hooks that are handed no host (Escape, the controls pane),
  so it parks `host_cancel_pending` instead of calling the host; `draw_overlay_ui` and `deactivate`
  flush it. A host must therefore also make `start_region_load` reset its own retained state.
- `deactivate` abandons everything in flight AND tells the host to drop what it retained: a host's
  `draw_overlay_ui` runs for the ACTIVE tool only, so a job left running would finish unpolled and
  later land on a page the user has meanwhile changed.
- The worker validates the ROI-sized composite ONCE, up front (`validate_job_input`), and refuses
  with a typed `PatchInputError`. Nothing is substituted for a buffer whose size it cannot explain.
  A host checks its own store-step buffers against the same ROI with the shared `check_roi_buffer`,
  so both sides refuse with one vocabulary and one set of localized sentences.
- The selection is stored in PAGE pixels and re-projected every frame. A stored screen rectangle
  would drift the moment the canvas scrolls or zooms. It is always projected onto its OWN page.
- A padded ROI that leaves the page is REFUSED with a localized message and a structured log. It is
  never clamped: a clamped ROI would silently sample pixels the user did not point at.
- **The outline never lies about the source.** It is projected with the UNCLAMPED page-pixel ->
  scene map (`scene_pos_in_page`), not with the clamping `PatchHost::scene_pos_to_page_pos`
  inverted, which would pin a source dragged past the page edge to the border. A drag the release
  would refuse is stroked in `OUTLINE_REFUSED` instead of white, so the refusal is visible DURING
  the drag rather than after it.
- The refusal shown on screen and the refusal performed at the release are ONE predicate:
  `PatchToolCore::drag_geometry` (selection bounds -> `PatchHost::page_source_size` -> `roi_for`).
  `paint` reads only whether it answered, `start_job` takes the rectangle and the page size it
  answered with. Pinned by a test that also makes the reason -> tone mapping total.
- A degenerate selection (fewer than three vertices, or a bounding box thinner than
  `MIN_SELECTION_SPAN_PX`) leaves NO selection behind, so a half-built outline can never be dragged.
- The drag offset is whole page pixels. The source is SAMPLED at `destination + offset`, and a
  fractional offset would need resampling, blurring the very texture the membrane exists to preserve.
- The outline is painted through `ctx.layer_painter(LayerId::new(Order::Middle, …))`, never from a
  pointer-gated cursor hook, which would make a session-long selection blink out. A bare layer
  painter registers no interactable `Area`, so it steals no canvas input. Its dashes are STATIC —
  no phase, and a repaint is requested only while a job runs — so they do not march.
- `PatchHost::poll_region_load` is called on EVERY frame pass, including while nothing is pending,
  and what it answers then is DROPPED. A host whose answer arrives on a channel can only release an
  abandoned job's buffers — for the cleaning host a decoded page region of several megabytes — when
  it is polled, so a poll that happened only while a job was outstanding would pin that memory until
  the next job drained the channel. An implementation must therefore be idempotent with nothing in
  flight, and must not treat "the worker went away" as a failure of a load whose composite it has
  already handed over: what remains of such a load is the store step's backdrop, not a pending
  answer.
- Every heavy step — decode, composite, solve — runs off the GUI thread. `PatchHost` implementors
  must honour that for the region load too (AGENTS.md §5). What `draw_overlay_ui` itself does is
  bounded and ROI-sized.
- `membrane::solve_patch` refuses a contract violation instead of repairing it: a buffer-length
  mismatch, an ROI below 3x3, or a mask touching the ROI border. An EMPTY mask is a no-op, not an
  error.
- `tag` and `outline_layer_id` must be unique per host: two hosts sharing a layer id would paint
  into one layer, and one log tag would make two surfaces' log lines indistinguishable.
- Localization: every string the CORE shows lives under the host-neutral `tools.patch.*` prefix,
  because both hosts show the same sentences. A host's own strings keep the host's prefix
  (`cleaning.tools.patch.*`, `ps_editor.tools.patch_*`) — the split is "who prints it", not "who
  ships it".

## Editing map
- To change when a source drag is refused — on screen and at the release alike — edit
  `PatchToolCore::drag_geometry` in `mod.rs`; it is the only place the rule lives, and `roi_for` is
  the only place its geometry does.
- To change the gesture, the refusal rules or the ROI geometry, edit `mod.rs`. How a gesture ENDS —
  committed, parked or cancelled — lives in `stroke_end` / `resolve_pending_end` / `stroke_begin`
  together; changing one without the other two breaks the release/crossing contract.
- To change the solve — the pyramid, the sweep counts, the Dirichlet weight, the blend modes or the
  feather ramp — edit `membrane.rs`. Every constant there is named and carries its rationale.
- To change the shared SOR kernel itself, edit `../sor.rs`; both consumers must be re-verified.
- To change the polygon rasterization rule, edit `../polygon_mask.rs` — it is shared with the
  PS-editor's selection and its sampling rule is a contract, not an implementation detail.
- To change what the core needs from a host, edit the `PatchHost` trait; every implementor has to
  be revisited, and today that is `src/tabs/cleaning/tools/patch/` and
  `src/tabs/ps_editor/tools/patch.rs`.
- To change how a patch is STORED, edit the host, never this module.
