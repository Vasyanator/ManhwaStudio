# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux3_klein/engine

## Purpose
`Flux3KleinEngine` itself: the state it keeps between frames, its `AiEngine` contract with the
host, and every long operation it starts on a worker. The only place in `flux3_klein` that owns
mutable state; the panel (`../ui/`) borrows it for one frame.

## Architecture
Two files, one type. `mod.rs` declares the struct, `impl AiEngine`, and the per-frame
housekeeping that must run whether the panel is drawn or not (settings load once, save when
dirty, `.status` re-asked when the package root or the cache question changes, the forecast
re-armed on a settled region SIZE, the run and the prompt-library jobs drained). `actions.rs` is
a second inherent `impl` block: the fold of the panel's intents (`apply_intents`) and the
`start_*` / `poll_*` pair of every long operation (token, file dialogs, reference reads, unload,
the prompt library through the shared `PromptCacheController`) and the fold of the catalog
block's click.

```
start_x()  -> validate, claim the progress generation if it streams, ms_thread::spawn
poll_x()   -> try_recv, apply the outcome, clear the receiver
```

## Files and submodules
- `mod.rs`: `Flux3KleinEngine`, `Flux3PickerPurpose`, `new` / `at_rest`, the accessors the panel
  and the gate read (`model_readiness`, `status_for_current_root`, `prompt_cache_state`,
  `prompt_cache_status_entry`, `pipeline_busy`, `catalog_snapshot`), the catalog drive (`poll_catalog_bundle`, `catalog_selection`,
  `request_catalog_status`, `apply_catalog_action`), `fold_panel_frame` (the panel frame's
  intents plus the selection re-probe), `profiles_request`, `run_input`,
  `frame_constraints`, `impl AiEngine`.
- `actions.rs`: `apply_intents` and the long operations, including the prompt library
  (`start_prompt_cache_build`, the export / import picker purposes, `poll_prompt_cache`,
  `poll_and_maybe_query_prompt_cache_list`, `apply_prompt_cache_effects`).

## Contracts and invariants
- **Nothing blocks the GUI thread**; `poll` only drains channels and is never conditional on
  the panel being drawn.
- **A user edit outranks a late settings load** (`poll_settings_load` adopts the file only while
  `dirty` is clear) and nothing is written before the load landed (`SettingsSaver`).
- **Settings are written once they settle**: the saver waits `SETTINGS_SAVE_DEBOUNCE` (700 ms)
  after the last change, `poll` wakes the GUI per `saver.wake_after(now)`, and a value still
  owed on drop is flushed by the saver on a worker. A «Постобработка» edit
  (`Flux3PanelIntents::postprocess_changed`) only owes a save (`note_postprocess_changed`); any
  other edit (`note_settings_changed`) also re-arms the forecast.
- **One query of a kind in flight**: `.status` by `status_wanted` (never without a package
  root), the forecast by the controller's arming, `.prompt_cache.list` by the shared
  controller's `list_wanted` (never without a package root); the status answer counts only for
  the root it was asked about, and its cache half only for the prompt it was asked about
  (`prompt_cache_state`). `status_cache_question` (trimmed prompt, TEXT mode) is what the last
  `.status` carried: a change re-asks it, so a keystroke re-arms a flag and never fires a call.
- **The prompt library follows the FLUX.2 policy**: a prompt edit or pick re-asks `.status`; a
  finished build and a changed listing re-ask `.status` and invalidate the forecast
  (`PromptCacheEffects`); the listing is re-asked after a build / import, a catalog job, an
  encoder or source-mode change and a root change. A served prompt in TEXT mode waives the
  encoder in `model_readiness`.
- **The catalog controller is driven every frame** (`poll_catalog_bundle`, panel drawn or not):
  never-probed specs of the current bundle are probed (which registers them for the automatic
  adoption), a job's falling edge re-asks `.status` and the forecast, and a selection change in
  a frame (install pickers, the forecast's DiT or encoder rows, a format suggestion;
  `fold_panel_frame`) re-probes the new bundle and re-asks `.status`. `switch_block_reason` is the shared
  `catalog_switch_block_reason` over the controller's running job.
- **The forecast asks** the `flux3_klein` adapter with the selected format, the normalized params
  and `shape.references` = the region first, then each user reference; its gate also needs a
  non-empty root (the backend refuses a forecast without one).
- **The run gate** names a cancelled run still ending (`cancel_pending_error`) and a
  prompt-cache operation in flight (`prompt_cache_busy_error`; it is part of `pipeline_busy`),
  never the run itself; the catalog download never blocks a run (it is not a backend operation); a run started
  while references are still being read is refused with its own sentence.
- **A run answers raw pixels**: `poll` returns `EnginePoll::DoneRaw` (raw region + the mask and
  mode `run_input` derived, moved back from the worker); `postprocess_settings` answers the
  persisted «Постобработка» values, normalized, every frame and for the engine's whole life.
- **A moved frame cancels the run** (the answer would describe another place); a settled resize
  arms the forecast once, a pure move arms nothing.

## Editing map
- What the host sees (layers, constraints, marks modes, the run, the post-process settings):
  `impl AiEngine` in `mod.rs`;
  what a request becomes: `run_input`.
- A new button that starts something long: a `start_*` / `poll_*` pair in `actions.rs`, drained
  from `AiEngine::poll`, its receiver in `pipeline_busy` if it holds the backend.
- Tests sit at the end of each file and use `../test_support.rs`.
