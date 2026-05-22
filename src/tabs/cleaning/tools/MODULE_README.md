# Module: src/tabs/cleaning/tools

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
binary masks, optional sample masks, text-detector mask generation, and worker-thread run
closures.

AI-backed tools (`lama.rs`, `lama_mpe.rs`, `aot.rs`) encode the selected region and mask as PNG
base64, ensure required app-managed models through `ai_models.rs`, verify backend health, call the
Python AI backend endpoint, validate the returned PNG size, and surface backend errors in the
region editor status.

## Files and submodules
- `mod.rs`: module exports for the cleaning tab.
- `base.rs`: `CleaningTool`, stroke/cursor types, brush scratch pipeline, region editor pipeline,
  mask-inpaint editor, text-detector mask generation, and region loader worker.
- `zamazka.rs`: primary paint/erase/eyedropper/rectangle tool for direct clean-overlay edits.
- `stamp.rs`: copies pixels from `project/alt_vers/<name>` into clean overlays using lazy
  background source-page loading.
- `gradient.rs`: local mask fill using Lab scanline estimation and smoothing.
- `texture_synthesis.rs`: local inpaint through the `texture-synthesis` crate, with optional
  sample mask limiting the texture source area.
- `lama.rs`: LaMa V2 backend inpaint, fixed supported model catalog, model scan, model ensure, and
  `/inpaint/lama_v2` calls.
- `lama_mpe.rs`: LaMa MPE backend inpaint and `/inpaint/lama_mpe` calls.
- `aot.rs`: AOT backend inpaint and `/inpaint/aot` calls.
- `region_edit_test.rs`: development-only mask-inpaint pipeline test tool; it is not exported by
  `mod.rs`.

## Contracts and invariants
- Tools must mutate clean overlays through `CanvasView` APIs such as `replace_overlay_region*` and
  `commit_overlay_page_to_model`; they must not write `CleanOverlaysModel` storage directly.
- Region, mask, and output image dimensions must match before processing or applying a result.
  Empty images or empty masks should return the original region or a clear user-facing error.
- File decode, source-page loading, AI calls, model scans/downloads, and CPU-heavy inpaint must run
  off the GUI thread. GUI code may poll channels, update textures, and apply prepared patches.
- Shared model locks must be held only long enough to snapshot or apply data. Do not hold them
  while decoding images, running detectors, calling Python, or building textures.
- AI tools that require Torch must honor backend availability supplied by the tab and fail visibly
  when the backend or model is unavailable.
- Text-detector mask generation inside the region editor must use the typed detector helpers from
  the translation module and must treat returned masks as binary alpha data in region coordinates.
- Tool pointer capture and zoom/scroll blocking are part of the canvas contract. An open region
  editor must block canvas zoom and capture pointer input inside its window.

## Editing map
- To add a new cleaning tool, implement `CleaningTool`, export it from `mod.rs`, and register it in
  `CleaningTabState::default`.
- To change common brush radius, scratch preview, stroke commit, or dirty-tile behavior, edit
  `BrushToolBase` in `base.rs`.
- To change region selection, composited-region loading, editor zoom/scroll, or apply behavior,
  edit `RegionEditToolBase` in `base.rs`.
- To change mask editor controls, text-detector mask generation, sample-mask handling, or
  worker-run lifecycle, edit `RegionMaskInpaintToolBase` in `base.rs`.
- To change direct paint behavior, edit `zamazka.rs`; to change alt-version stamping, edit
  `stamp.rs`.
- To change local fill/inpaint algorithms, edit `gradient.rs` or `texture_synthesis.rs`.
- To change Python backend payloads, endpoint names, model selection, unload behavior, or model
  ensure logic, edit the relevant AI tool file and keep `ai_models.rs` as the model boundary.
