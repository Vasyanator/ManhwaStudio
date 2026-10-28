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
when the paths change, re-arming the forecast when the region SIZE settles, and cancelling
the run in flight when the region moves. `actions.rs` is a second inherent `impl` block
holding the `start_*` / `poll_*` pair of every long operation.

The split is by lifetime, not by topic: `mod.rs` answers questions the panel asks on the
frame it is drawn in (`pipeline_busy`, `model_readiness`, `status_for_current_paths`,
`prompt_cache_state`, `prompt_cache_status_entry`, `catalog_snapshot`, `switch_block_reason`), while `actions.rs`
owns work that outlives the frame that started it.

Every one of those accessors is THE way the panel asks its question, never an expression
inlined at a call site.

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
  re-armed. It also drives the catalog download of both variants (`downloads`,
  `poll_catalog_bundle`, `poll_installed_formats`, `catalog_snapshot`, `apply_catalog_action`)
  and folds a change of the weight selection — formats or encoder source
  (`fold_resources_and_formats` over `Flux2WeightSelection`, `note_weight_formats_changed`).
- `sdcpp.rs`: the Stable-Diffusion.cpp state (`Flux2SdcppState`): the sd.cpp status worker
  (the files of `../sdcpp_files.rs` and the saved sd.cpp device, re-asked when the settings' key,
  the `ms_sdcpp` status revision or a refresh asks), and the run in two stages — `Preparing` (the
  request built on a worker by `../sdcpp_request.rs`) then `Running` (the shared `SdCppRunJob`,
  started on the GUI thread so it claims the progress in click order); `cancel` parks either in
  `abandoned`; `flux2_sdcpp_raw_result` pairs the answer exactly like the PyTorch one.
- `actions.rs`: the per-component load/unload actions, the Hugging Face token field, the
  prompt translation, the native file pickers, and the `.prompt_cache.*` jobs (build — also a
  rename — export, import, and the library listing), which run through the shared
  `PromptCacheController` (`prompt_cache` in `mod.rs`) with FLUX.2's header and methods; what an
  answer invalidated comes back as `PromptCacheEffects` and is folded into `.status` and the
  forecast (`apply_prompt_cache_effects`). Edit it to add an operation or to change
  what one does with its answer.

## Contracts and invariants
- **Nothing here blocks the GUI thread.** Every `start_*` hands its work to an
  `ms_thread` worker and every `poll_*` only drains a channel.
- **`poll` runs every frame, `draw_parameters` may not.** Nothing the panel does may be a
  precondition of polling; the intents the panel raises are folded back at the end of
  `draw_parameters` itself, so a click acts on the frame it happened in.
- **One progress bar, claimed by GENERATION.** Three operations share it — a run,
  `.prompt_cache.build` and `.component_action` — so `pipeline_busy` reads all three
  receivers, and the RUN gate reads `non_run_pipeline_busy` instead, which is the same rule
  with the generation taken out.
- **At most one query of a kind is ever in flight.** `.status`, the forecast
  (`large_models.profiles`, through the shared `ResourcesController` — its `ProfilesQuery` also
  keeps the last answer as a STALE one while the next is asked) and `.prompt_cache.list` are
  re-armed by a flag (`resources.arm()` for the forecast), never issued per keystroke. Events
  that change the machine rather than the question (a finished run, a component action, a cap
  write, a floor reset, a reconnect, a download, a finished prompt-cache build, a listing that
  differs from the one held — the first one included, because the first `.list` of a process
  is what migrates the old caches and `.status` never does — go through `invalidate_forecast`;
  a finished build and such a listing also re-arm `.status` (whether the prompt is served). The caps are
  sent by the controller's debounced `LimitsWriter`, drained from `poll` (`poll_side_jobs`).
- **A failed settings write is not success**: the shared `SettingsSaver` (`saver`) drives the
  write; a failure restores `dirty`, shows `saver.error` and retries after `save_retry_delay`
  (2 s doubling to 60 s); the writer function is `saver.write`, so a test can inject a failing
  one.
- **Settings are written once they settle**: a change waits out the saver's quiet time
  (`SETTINGS_SAVE_DEBOUNCE`, 700 ms after the LAST change; a dragged dial changes the settings
  every frame), `poll` wakes the GUI per `saver.wake_after(now)`, and a value still owed when
  the engine is dropped is flushed by the saver on a worker. `poll_and_maybe_save(now)` takes
  the clock, so tests drive the quiet time explicitly.
- **A «Постобработка» edit is not a forecast question**: the panel reports it separately
  (`Flux2PanelCtx::postprocess_changed`) and `note_postprocess_changed` only owes a save;
  every other edit goes through `note_settings_changed`, which also re-arms the forecast.
- **A blocked settings file is never written** (`settings_save_blocked`): the edits stay in
  memory and `poll_and_maybe_save` returns early. A MIGRATED load is marked dirty, so the
  file is rewritten once without the retired placement fields.
- **`variant` is set at construction and never changes.** The picker holds one instance per
  `config::Flux2Variant`, each with its own settings file, model directory and catalog
  bundle. `new(variant)` stamps it into the in-memory settings before the load worker starts,
  so everything derived from them describes the right checkpoint from the first frame;
  `Default` is `new(Klein9B)` and exists for the tests' sake.
- **Every product engine owns a catalog controller** (`new` creates it for both variants; a
  TEST build creates none, because its automatic adoption would hash and mark the developer's
  real model files). It is polled from `poll` every frame, never only from the panel, so
  adoption and a finished download land with the panel closed. The installed-formats probe
  (per component and source, plus the bf16 transformer layout) runs on an `ms_thread` worker
  (stat-only, still disk I/O); its answer lives in `settings.installed_formats`, which a
  settings load carries over because the file never holds it, and a changed layout re-asks
  `.status`. Any running job of the controller (download, verify, automatic adoption:
  `running_job`) keeps the engine picker closed through `switch_block_reason` (the shared
  `catalog_switch_block_reason`): the host polls
  only the selected engine, so a job left behind by a switch would run unseen.
- The frame rectangle, the painted mask, the marks and the pending result belong to the
  HOST and reach this type only through `AiEngine`. The marks arrive already packaged per
  `marks_support()`: a region-sized reference (sent as the run's extra reference image),
  or nothing; a transparent layer is unsupported and refused by `run_input`. This module never touches `CanvasView` or
  `CleanOverlaysModel`.

- **On Stable-Diffusion.cpp nothing is asked of the Python backend**: `.status`, the forecast
  and `.prompt_cache.list` are gated on `ImageEditRuntime::PyTorch`; `note_runtime_changed`
  re-arms them on the way back and re-probes the catalog bundle (it follows the runtime).

## Editing map
- To change what the host sees — the sections, the mask layer, the size constraints, the
  accepted marks modes, what a run answers with (`DoneRaw`) and the live post-process
  settings (`postprocess_settings`) — see `impl AiEngine` in `mod.rs`; to change
  what a run request is validated and turned into, see `Flux2KleinEngine::run_input`.
- To change when the catalog, the forecast or the library listing is re-queried, see
  `poll_and_maybe_query_*`, `invalidate_forecast` and `note_settings_changed` in `mod.rs`;
  what the forecast is asked about is `profiles_request`.
- To add a button that starts something long, add its `start_*` / `poll_*` pair in
  `actions.rs`, drain it from `AiEngine::poll` and add its receiver to `pipeline_busy`.
- Tests sit at the end of each file and use the fixtures of `../test_support.rs`.
