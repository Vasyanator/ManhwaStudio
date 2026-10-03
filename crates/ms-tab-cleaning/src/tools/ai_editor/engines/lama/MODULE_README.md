# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/lama

## Purpose
The «Lama» engine of the «ИИ-редактор области» tool: mask-driven inpainting with no prompt.
The user paints what must be REMOVED, the engine sends the region and that mask to the Python
AI backend, and the backend answers with the same rectangle, background restored.

It is an ENGINE, not a `CleaningTool`: no selection, no editor window, no canvas hooks, no
entry in `tab.rs`. It reaches the user only through the host's picker («Без промпта» section)
and the host's «Редактор области» panel. Everything about the canvas — the rectangle, the
painted mask stack, the pending result, Применить/Отменить — belongs to the host
(`../../MODULE_README.md`) and the frame (`../../../region_edit_v2/MODULE_README.md`).

## Architecture
```
engine.rs   LamaEngine ── impl AiEngine ── draw_parameters / start / poll / cancel
                │
                ├─ settings.rs  LamaSettings  ── load / save (workers, driven from poll)
                ├─ catalog.rs   LamaModelSpec ── which method, which params, refine or not
                ├─ scan.rs      LamaModelScan ── which checkpoints are on disk (worker)
                └─ wire.rs      run_lama / unload_lama ── the two IPC methods (worker)
```
FOUR MODELS, TWO BACKEND METHODS, ONE RUN BUTTON. `inpaint.lama_v2` and `inpaint.lama_mpe` are
different architectures, so the backend keeps both methods and the dispatch is entirely on this
side: each catalog entry names the method it runs, and the parameter panel draws only the
parameters that method reads.

| Checkpoint | Method | Directory | Parameters | Refine |
|---|---|---|---|---|
| `best.ckpt` | `inpaint.lama_v2` | `Torch/LaMa/models` | `refine`, `n_iters`, `max_scales`, `px_budget` | yes |
| `lama_large_512px.ckpt` | `inpaint.lama_v2` | `Torch/LaMa/models` | same | yes |
| `anime-manga-big-lama.pt` (default) | `inpaint.lama_v2` | `Torch/LaMa/models` | same | no (TorchScript) |
| `inpainting_lama_mpe.ckpt` | `inpaint.lama_mpe` | `Torch/LaMa_MPE` | `inpaint_size` | no |

Panel body order: the mask-meaning note → the model picker with its presence status and
«Проверить модели» → the parameters of the SELECTED entry's method → the unload button and the
status lines. Nothing here is behind a fold except the parameters, which open by default.

## Files and submodules
- `mod.rs`: module root — the header, the module declarations, the shared imports every
  submodule pulls in with `use super::*;`, and the constants (call timeout, selection rule,
  parameter ranges). No logic. The mask tint is `ms_theme::canvas::MASK_TINT`.
- `catalog.rs`: `LamaMethod`, `LamaModelSpec`, the four entries, the default, the name → spec
  resolution and the ensure-before-run path. Edit here to offer another checkpoint, to change
  which method one runs, or to change whether it can refine.
- `scan.rs`: the presence scan of BOTH model directories (a worker) and the one status line it
  feeds. Informational only — the picker always offers the full catalog.
- `settings.rs`: `LamaSettings` (the selected model and every parameter of both methods), its
  clamping, its file IO and `settings_save_due`.
- `wire.rs`: `effective_refine`, `lama_run_header`, `run_lama`, `unload_lama`, the
  request-blob packing and the `CallError` mapping (the PNGs come from `tools/region_png.rs`). Edit here to change what travels on the wire.
- `engine.rs`: `LamaEngine`, its `AiEngine` impl, its channel polling and its parameter panel.

## Contracts and invariants
- **The mask is MANDATORY and means the INPAINT thing.** `allows_empty_mask()` is
  unconditionally `false`: the mask says what to REMOVE, so an empty one describes no work. The
  host disables «Обработать» and draws no green "works without a mask" hint. The meaning is the
  inverse of FLUX.2 klein's ("you MAY change what is under it"), the two engines share one frame
  and one brush, and that is why the single layer carries the inpaint YELLOW of the
  mask-inpaint editor (`../../../base.rs`) and why the panel body states the meaning in words.
- **A catalog entry's method and refine support are CONSTANTS, never re-derived.** The backend
  refuses its refine pass on a TorchScript `.pt` outright, so `supports_refine` is a field of the
  entry rather than a suffix test at a call site — a second copy of that rule is a copy that
  drifts. `effective_refine(spec, settings)` is the ONE place the flag is decided and
  `lama_run_header` calls it itself, so `refine: true` cannot reach the wire for a model that
  cannot refine. The UI closes the control with a tooltip that says why.
- **The wire.** Two methods, `image_len` / `mask_len` header ints, an `image_png ++ mask_png`
  request blob, the result PNG in the RESPONSE blob and a 300 s call timeout. The contract is
  the backend's (`modules/ai_backend/inpaint/lama.py`) and is shared with `PROTOCOL_VERSION`:
  changing any of it is a protocol change, not an engine-local one.
- **Selection contract: multiple of 8, shortest side >= 8 px, no area and no aspect limit.**
  Published through `constraints()`, which the frame snaps and validates against. `start`
  re-checks the ACTUAL size it is handed against those same constraints — through the shared
  `engines::region_size_refusal`, i.e. through `geometry::check_size`, the one authority on
  what a valid size is — rather than trusting the host: the frame's rectangle and the region
  handed over can disagree, and the user must be told which rule broke.
- **Nothing blocks the GUI thread.** The settings load and save, the model scan, the unload call
  and the run itself all live on `ms_thread::spawn` workers; `poll` only drains channels. The
  ensure-before-run step is on the run worker because it may download hundreds of megabytes.
- **`poll` is the only writer of the settings file.** The saver runs there and writes on the
  first poll a save is due on — there is no time debounce — gated by
  `settings_save_due(dirty, settings_loaded, save_in_flight)`: a write before the initial load
  lands would overwrite the user's file with the in-memory defaults, so `dirty` stays pending
  instead of being dropped, and at most one writer touches the path.
- **A user edit outranks a settings load that lands after it.** `poll_settings_load` applies the
  file only while `dirty` is clear, because the host draws the panel body earlier in a frame than
  it polls the engine: a value changed before the load landed would otherwise be replaced
  silently. The whole loaded document is then dropped, untouched fields included — the accepted
  price of never losing a visible edit.
- **The parameters of the UNSELECTED method are persisted too.** Switching model and back must
  not reset the other method's values, so one settings file carries all of them.
- **There is no progress and no backend-side cancel.** Both methods are plain request/response
  calls: no streamed frames to draw a bar from, and the plain `Client::call` never hands out the
  request id `Client::cancel` needs. `cancel` detaches the answer; the backend finishes the pass.
  `poll` calls `ctx.request_repaint()` while a run is in flight, or a finished run would sit in
  the channel until the next click.
- **The catalog is shared with the sibling `../sdxl/` engine, and only in its LaMa-v2 VIEW.** Its
  4-channel prefill sends a checkpoint FILE NAME the backend resolves inside `Torch/LaMa/models`,
  so it reads `lama_v2_model_catalog()` and `ensure_lama_model_for_external` refuses any name
  outside that subset — the MPE entry included. That single consumer is why those four items are
  `pub(in crate::tabs::cleaning::tools::ai_editor::engines)` instead of private to this module;
  nothing outside `engines` sees them, and this module knows nothing about `../sdxl/`.

## Editing map
- To offer another checkpoint, or to change a method / refine flag: `catalog.rs` (and add the
  display key to all five `crates/ms-i18n/locales/*.json`). Remember `ai_models.rs`'s own
  `canonical_lama_model_file`, which maps a v2 download request to a known file name and
  SUBSTITUTES the default for anything it does not know: a new checkpoint added only here would
  silently download the default instead. `catalog.rs` refuses an unknown name before it gets
  that far, which is what keeps the substitution invisible today.
- To change what travels on the wire, or the request header of either method: `wire.rs`.
- To add or re-range a parameter: the constants in `mod.rs` plus `LamaSettings` in `settings.rs`
  plus `draw_method_parameters` in `engine.rs`.
- To change the selection rule: `LamaEngine::constraints` and the two constants in `mod.rs`.
- To change the mask label: `LamaEngine::mask_layers`; the tint itself is the shared
  `ms_theme::canvas::MASK_TINT`.
- To change where the settings file lives: `config::lama_engine_settings_path`.
- To change the picker, the panels, the frame or the apply path: the HOST (`../../mod.rs`),
  never here.
