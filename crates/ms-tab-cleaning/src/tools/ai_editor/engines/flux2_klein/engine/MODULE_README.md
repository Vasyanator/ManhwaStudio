# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/engine

## Purpose
`Flux2KleinEngine` itself: the state it keeps between frames, its `AiEngine` contract with
the host, and every long operation it starts on a worker thread. This is the only place in
`flux2_klein` that owns mutable state; the panel (`../ui/`) borrows it for one frame and
the decisions (`../decisions.rs`) only read values handed to them.

## Architecture
Two files, one type. `mod.rs` declares `Flux2KleinEngine`, implements `AiEngine`, and
keeps the per-frame housekeeping that must run whether the panel is visible or not —
loading the settings once, saving them when they settle, re-querying the `.status` catalog
when the paths change, re-arming the forecast when the region SIZE settles, and dropping
the undo history when the region moves. `actions.rs` is a second inherent `impl` block
holding the `start_*` / `poll_*` pair of every long operation.

The split is by lifetime, not by topic: `mod.rs` answers questions the panel asks on the
frame it is drawn in (`pipeline_busy`, `model_readiness`, `status_for_current_paths`,
`prompt_cache_state`, `download_check_current`, `switch_block_reason`), while `actions.rs`
owns work that outlives the frame that started it.

Every one of those accessors is THE way the panel asks its question, never an expression
inlined at a call site: `download_check_current` in particular must compare the EFFECTIVE
encoder toggle, because that is what `start_download_check` stamps into the answer, and the
raw persisted flag rejected every answer forever on a 4B document that carried it.

Every action follows the same two-step shape:

```
start_x()  -> validate, claim the progress generation, ms_thread::spawn, keep the Receiver
poll_x()   -> try_recv, apply the outcome, clear the Receiver
```

## Files and submodules
- `mod.rs`: `Flux2KleinEngine`, its `new(variant)` / `at_rest` / `Default` trio, the
  inherent methods that keep the derived state honest, `impl AiEngine`, and the
  `same_region` / `same_region_size` helpers the region rules are written in. Edit it for
  the host contract, for what the engine remembers, and for when a query or a forecast is
  re-armed.
- `actions.rs`: the per-component load/unload actions, the Hugging Face token, the model
  download and the derivation it applies when it finishes, the prompt translation, the
  native file pickers, and the six `.prompt_cache.*` jobs. Edit it to add an operation or
  to change what one does with its answer.

## Contracts and invariants
- **Nothing here blocks the GUI thread.** Every `start_*` hands its work to an
  `ms_thread` worker and every `poll_*` only drains a channel.
- **`poll` runs every frame, `draw_parameters` may not.** Nothing the panel does may be a
  precondition of polling; the intents the panel raises are folded back at the end of
  `draw_parameters` itself, so a click acts on the frame it happened in.
- **One progress bar, claimed by GENERATION.** Four operations share it — a run,
  `.prompt_cache.build`, `.component_action` and `.download.start` — so `pipeline_busy`
  reads all four receivers, and the RUN gate reads `non_run_pipeline_busy` instead, which
  is the same rule with the generation taken out.
- **At most one query of a kind is ever in flight.** `.status`, `.estimate` and
  `.prompt_cache.list` are re-armed by a flag, never issued per keystroke.
- **`variant` is set at construction and never changes.** The picker holds one instance per
  `config::Flux2Variant`, each with its own settings file, model directory and
  `.download.*` requests. `new(variant)` stamps it into the in-memory settings before the
  load worker starts, so everything derived from them describes the right checkpoint from
  the first frame; `Default` is `new(Klein9B)` and exists for the tests' sake.
- The frame rectangle, the painted mask and the pending result belong to the HOST and
  reach this type only through `AiEngine`. This module never touches `CanvasView` or
  `CleanOverlaysModel`.

## Editing map
- To change what the host sees — the sections, the mask layer, the size constraints, what
  a run answers with — see `impl AiEngine` in `mod.rs`.
- To change when the catalog, the forecast or the library listing is re-queried, see
  `poll_and_maybe_query_*` and `note_settings_changed` in `mod.rs`.
- To add a button that starts something long, add its `start_*` / `poll_*` pair in
  `actions.rs`, drain it from `AiEngine::poll` and add its receiver to `pipeline_busy`.
- Tests sit at the end of each file and use the fixtures of `../test_support.rs`.
