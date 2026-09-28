# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/sdxl

## Purpose
The «SDXL Inpaint» ENGINE of the «ИИ-редактор области» tool: two channel modes behind one backend
method, a prompt, a streamed progress bar with a live latent preview, and one run button. It
implements `../../engine.rs`'s `AiEngine` and owns everything model-specific — the parameters of
both modes and their persistence, the wire contract, the worker thread and the progress state.

It owns NOTHING about the canvas. The rectangle, the painted mask stack, the pending result and
Применить/Отменить belong to the HOST (`../../MODULE_README.md`) and the frame
(`../../../region_edit_v2/MODULE_README.md`). This module never sees `CanvasView`, `ProjectData` or
the frame, and never blocks the GUI thread.

It is NOT a `CleaningTool`: no selection, no editor window, no canvas hooks, no entry in `tab.rs`.
It reaches the user only through the host's panels.

## Architecture
```
engine.rs   SdxlEngine  -> AiEngine: constraints, mask layer, run gate, start/poll/cancel,
                           and the parameter panel body
settings.rs SdxlMode / SdxlSettings / SdxlPersisted -> the on-disk document and the save gate
progress.rs SdxlSharedProgress -> the worker<->panel progress state, its generation stamp,
                           the bar and the live latent preview
wire.rs     run_sdxl / unload_sdxl -> the request header, the blob encoding, the streaming call
```
`mod.rs` is the module ROOT and holds no logic: the file header, the module declarations, the
imports every submodule shares through `use super::*;`, and the constants (timeout, size grid,
sampler list, parameter ranges, the `-1` random-seed sentinel).

TWO MODES, ONE BACKEND METHOD. `inpaint.sdxl` takes a `mode` field and behaves as two pipelines
behind it, which is why the mode is a PARAMETER of one engine rather than two picker entries:
- `nine_channel` — a dedicated 9-channel inpainting UNet. The clean masked image travels as its own
  conditioning channel, so the default denoise is 1.0 and there is no prefill.
- `four_channel` — an ordinary 4-channel SDXL checkpoint. LaMa prefills the hole first, which
  removes the text from the context, so a moderate denoise (0.75) keeps the surrounding texture.

The LaMa prefill catalog is NOT duplicated here: `../lama/` owns it and this module reads its
LaMa-v2 SUBSET (`lama_v2_model_catalog`). That is the only cross-engine dependency in the `engines`
subtree and it is one-directional.

## Files and submodules
- `mod.rs`: the module root — header, submodule declarations, shared imports and the constants.
  Edit it to change a limit: the call timeout, the selection grid, the offered
  sampler list, a parameter range, or the preview width.
- `settings.rs`: `SdxlMode` and its wire spelling, `SdxlSettings` (one full parameter set per mode)
  with the per-mode defaults, `SdxlPersisted` (the document), `SdxlRunConfig` (the worker snapshot),
  the file IO and `settings_save_due`. Edit it to add, rename or re-default a persisted field.
- `progress.rs`: `SdxlSharedProgress`, the poison-tolerant lock, the generation claim/retire/publish
  helpers, the bar fraction and `draw_sdxl_progress_ui`. Edit it to change what a run reports while
  it is in flight.
- `wire.rs`: `lama_model_for_run` (the prefill gate), `sdxl_run_header`, `run_sdxl`, `unload_sdxl`,
  the streaming call, the `CallError` mapping and the PNG/L8 encoders. Edit it to change what
  travels on the wire — and read the protocol contract below first.
- `engine.rs`: `SdxlEngine`, its `AiEngine` impl and its parameter panel. Edit it to change the
  panel layout, the run gate or the polling.

## Contracts and invariants
- **The mask is MANDATORY and means REGENERATE.** `allows_empty_mask()` is unconditionally `false`:
  SDXL inpainting has no whole-region mode the way FLUX.2 klein does, so an empty mask describes no
  work. The host disables «Обработать» and draws no «работает без маски» hint. The single layer
  carries the shared removal-mask tint `ms_theme::canvas::MASK_TINT` (the mask-inpaint editor's
  yellow), the same colour `../lama/` uses, because the meaning is the inpainting hole in both.
- **The wire is the backend's contract, not this engine's.** One `inpaint.sdxl` method,
  `image_len` / `mask_len` header ints, an `image_png ++ mask_png` request blob, the result PNG in
  the RESPONSE BLOB, `progress` frames (counters in the frame HEADER, latent preview PNG in the
  frame BLOB) and a 20 minute timeout. It is shared with `modules/ai_backend/inpaint/sdxl.py` and
  with `PROTOCOL_VERSION`: changing any of it is a protocol change, not an engine-local one.
- **`lama_model` reaches the wire only in `four_channel`.** `lama_model_for_run` is the one place
  that rule lives and the header builder itself calls it, so no call site can bypass it. The
  9-channel pipeline has no prefill step and would not know the field. The name is resolved through
  `ensure_lama_model_for_external`, which refuses anything outside the LaMa-v2 subset and fetches
  the checkpoint when it is missing — a download, and therefore worker-thread only.
- **Each mode owns a COMPLETE parameter set and both are persisted.** Switching mode switches the
  whole set and never merges the two. The document holds both sets plus the selected mode, in ONE
  file — a file per mode could not record which mode to restore.
- **The settings document is `{mode, nine_channel, four_channel}` in
  `config::sdxl_inpaint_settings_path()`, and its field names are part of the contract** — renaming
  one silently drops that value from every existing user's file. There is deliberately NO
  `normalized()` clamp, unlike `../lama/settings.rs`: persisted values travel to the wire verbatim
  and clamping on load would rewrite a hand-edited value. The panel's ranges bound everything
  entered through the UI.
- **Nothing blocks the GUI thread.** The settings load and save, the unload call and the run all
  live on `ms_thread` workers; `poll` only drains channels. The saver lives INSIDE `AiEngine::poll`
  — which the host calls every frame whether the panel is drawn or not, so without that call
  nothing ever writes the file and the mode and parameters are lost on exit, with no error anywhere
  — and it writes on the first poll a save is due on: `settings_save_due` is a plain gate, there is
  no time debounce.
- **A user edit outranks a settings load that lands after it.** `poll_settings_load` applies the
  file only while `dirty` is clear, because the host draws the panel body earlier in a frame than it
  polls the engine: a value changed before the load landed would otherwise be replaced silently. The
  whole loaded document is then dropped, mode and untouched fields included — the accepted price of
  never losing a visible edit.
- **The progress bar is claimed by GENERATION.** A cancelled run is DETACHED: `call_streaming` never
  hands out the request id `Client::cancel` would need, so the backend finishes the pass and there
  is no backend-side cancel. Every publication into the shared progress therefore carries the
  generation that claimed the bar, and writes from a retired run are dropped.
- **The result size is checked, never rescaled.** `run_sdxl` refuses a response image that is not
  exactly the region it asked about, and `start` refuses a request whose region or mask does not
  match `rect_px` rather than encoding it onto the wire.
- **Selection contract: multiple of 8 (the VAE downscale), shortest side >= 8 px, no area and no
  aspect limit.** Published through `constraints()`, which the frame snaps and validates against.
  `start` re-checks the ACTUAL size it is handed against those same constraints — through the shared
  `engines::region_size_refusal`, i.e. through `geometry::check_size`, the one authority on what a
  valid size is — rather than trusting the host: the frame's rectangle and the region handed over
  can disagree, and the user must be told which rule broke.
- **The run gate is the engine's own half only.** A run already in flight, and a missing weights
  path — SDXL has no default checkpoint, so a run without one could only fail in the backend. The
  non-empty-mask rule is `allows_empty_mask()`, and the rectangle is the frame's to validate; the
  size re-check above is the run path's own last guard, not part of this gate.
- **Literals that are wire tokens stay literal.** The sampler names, the mode tokens, «CFG» and
  «Denoise» are the backend's own spellings; a translated one would name no scheduler and no field.
  Everything the user reads as prose goes through `t!` / `tf!`, and every widget whose id would be
  derived from a localized caption carries a pinned `id_salt`.

## Editing map
- To change a limit (timeout, selection grid, sampler list, a parameter range, the
  preview width, the random-seed sentinel): `mod.rs`.
- To add or re-default a persisted field: `settings.rs` — and keep the field NAME, or an existing
  user's file silently loses that value.
- To change what travels on the wire, or the prefill gate: `wire.rs` (`sdxl_run_header`,
  `lama_model_for_run`). The method names live in `backend_ipc::protocol`.
- To change the panel layout, the run gate or the polling: `engine.rs`.
- To change what a run reports while in flight: `progress.rs`.
- To change which LaMa checkpoints the prefill picker offers: `../lama/catalog.rs`, never here.
- To change what the host does with the engine (the picker, the panels, the frame): `../../mod.rs`
  and `../../../region_edit_v2/`, never here.
