# Module: src/tabs/cleaning/tools/patch

## Purpose
The cleaning tab's HOST for the «Заплатка» tool — Photoshop's Patch Tool. The user draws a
free-form (lasso) or rectangular selection directly on the page canvas; the closed selection stays
live on the canvas for the whole session; dragging from inside it onto a clean SOURCE area copies
those pixels into the selection and colour-adapts them to the destination's contour. The result is
committed into the CLEAN OVERLAY as ONE undo step.

The tool itself is not here. Selection, gesture, ROI and refusal geometry, the gradient-domain
solve and the dashed-outline painting all live in `crate::tools::patch`, which is host-neutral.
This directory is the adapter: the `CleaningTool` impl, the `PatchHost` impl (canvas geometry, the
region loads, the store step) and the tab registration.

It belongs to the FIRST tool category, next to the brushes (`BRUSH_TOOL_INDICES` in `../../tab.rs`),
and is NOT a region-editor tool: no floating window, no main dock panel.

## Architecture

### What this file owns
- `PatchTool`: a `PatchToolCore` plus this host's state (`PatchLoader`, `panel_rects`). Every
  `CleaningTool` hook forwards to the core; nothing about the gesture is decided here.
- `CleaningPatchHost`: the short-lived `PatchHost` view built for one hook out of `&mut CanvasView`,
  an optional `&ProjectData` and `&mut PatchLoader`. Only `start_region_load` needs the project, and
  it is reached from `draw_overlay_ui`, which has one.
- `PatchLoader`: the shared region-loader thread (`base::spawn_region_loader_thread`) and the ONE
  job it has in flight — the two job ids, the clean-overlay chunk under the ROI, and the two answers.
- `build_overlay_chunk`: the STORE step.
- `check_chunk_fits` / `ApplyError`: the bounds guard run against the LIVE overlay before the write.

### Why TWO region loads
The membrane needs the COMPOSITED destination (page + clean overlay); the store step needs the
ORIGINAL page pixel under it as `overlay_pixel_for_final_color`'s `base`. `RegionLoadRequest`
answers one or the other depending on whether an `overlay_chunk` is supplied, so this host asks the
same loader twice for the same rectangle. The core asks only for the composite; the second load is
this host's, because the store step is its only consumer. The loader caches the decoded page per
worker and through `CleanOverlaysModel`, so the second request costs a crop, and both answers are
derived from one rectangle and therefore cannot disagree on size.

`poll_region_load` hands the core the composite only once BOTH have arrived, and keeps the page
half for `commit_patch`. It DRAINS the loader channel on every call, and the core calls it on every
frame — including when it awaits nothing, which is how an abandoned job's decoded region is released
promptly instead of sitting in the channel until the next job. Two consequences: an answer with no
job in flight is dropped, and a loader that dies AFTER the composite was handed over must not fail
the load (`InFlightLoad::fails_on_worker_death`) — what is left of it is the store step's backdrop,
and dropping that would lose a solved patch.

### The store step
`build_overlay_chunk` starts from the clean-overlay chunk as it was captured with the load and,
for every pixel the patch covers, replaces it with `overlay_pixel_for_final_color(page_pixel,
final_colour, coverage)`. That is the only cleaning-specific thing about this tool: what the core
hands over is a colour and a coverage, and turning those into a clean-overlay pixel is a property
of the clean-overlay model.

## Files and submodules
- `mod.rs`: everything above. There is no other file here; the maths lives in
  `crate::tools::patch::membrane` and has its own `MODULE_README.md` next to it.

## Contracts and invariants
- **`block_canvas_zoom()` is `false`, always.** It also disables the clean-overlay Ctrl+Z /
  Ctrl+Shift+Z shortcuts (`../../tab.rs::handle_history_hotkeys`), which is acceptable for a modal
  editor window but not for a surface that lives on the canvas for a whole session. Pinned by a test.
- **`captures_canvas_pointer()` is `false`, always.** The tab ORs it into `canvas_pointer_occluded`
  and then calls `finish_stroke()` and returns, so a capturing tool receives no stroke, key or
  cursor callbacks — and this tool's entire gesture rides those. Pinned by a test.
- Before the write, `check_chunk_fits` re-validates the chunk against the page's LIVE overlay, not
  the one measured when the job started (`cleaning/MODULE_README.md`'s overlay-edit rule, and D7 in
  `../region_edit_v2/frame.rs`). `replace_overlay_region_px` REPAIRS instead of refusing — it clips
  the target rectangle and nearest-rescales the chunk into the remainder — so a page that changed
  under a running solve would otherwise be corrupted silently.
- The store step validates BOTH of its ROI-sized buffers up front, through the core's shared
  `check_roi_buffer`, and refuses with the core's `PatchInputError`. Nothing is substituted for a
  buffer whose size it cannot explain: a transparent stand-in for the clean-overlay chunk would
  erase every pre-existing cleaning pixel of the ROI on commit. Pinned by a test.
- Pixels whose coverage is zero keep the EXISTING clean-overlay pixel: the commit replaces the whole
  ROI, so anything else would erase the surrounding cleaning work.
- The committed overlay is DENSE, not minimum-alpha, for the same reason `../stamp.rs`'s
  current-page mode is: the page and the overlay are separate `TextureOptions::LINEAR` quads, so a
  low-alpha stencil ghosts the page back through itself. That is why the feather defaults to 0 and
  the selection's interior commits fully opaque.
- `replace_overlay_region_px` syncs the shared model and records ONE region diff by itself
  (`CleanOverlaysModel::replace_region`), so the whole patch is a single Ctrl+Z and no full-page
  commit follows.
- The page-pixel space is the clean overlay's, so `PatchHost::ensure_page_pixels` calls
  `BrushToolBase::ensure_overlay_under_point`, and `page_projection` reports `overlay_size` while
  `page_source_size` reports what the loader crops against.
- `usable_viewport` cuts this frame's dock-panel rectangles out of the canvas viewport with
  `region_edit_v2::geometry::usable_viewport_for`, so the outline never paints over a panel. The
  panel rectangles are this host's state (`set_panel_rects`), not the core's.
- The shared region loader's ownership contract is honoured by `PatchLoader::drop`: send `None` and
  join, or the thread and the page it decoded outlive the tool.
- `commit_patch` LOGS its own technical detail and returns only the localized sentence; the core
  shows that sentence and deliberately does not log it a second time. Changing one side without the
  other either loses the detail or duplicates the line.
- Registration is FOUR sites (`../mod.rs`, the `use` list in `../../tab.rs`, the
  `CleaningTabState::default` vector, and `BRUSH_TOOL_INDICES`); the partition test in `../../tab.rs`
  fails loudly if one is missed.

## Editing map
- To change how a patch is STORED — the backdrop, the alpha, what an uncovered pixel keeps — edit
  `build_overlay_chunk` here.
- To change how the destination or the source pixels are loaded, edit `start_region_load` /
  `poll_region_load` here and `../base.rs`'s region loader; do not add a private decoder.
- To change the gesture, the refusal rules, the ROI geometry, the controls or the outline, edit
  `crate::tools::patch` — none of that lives here, and a second copy of it would drift.
- To change the seamless-cloning maths, edit `crate::tools::patch::membrane`.
- To change which canvas values the tool is projected with, edit the `PatchHost` impl here; the
  trait itself is in `crate::tools::patch`.
