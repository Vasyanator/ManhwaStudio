# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux3_klein/engine

## Purpose
`Flux3KleinEngine` itself: the state it keeps between frames, its `AiEngine` contract with the
host, and every long operation it starts on a worker. The only place in `flux3_klein` that owns
mutable state; the panel (`../ui/`) borrows it for one frame.

## Architecture
Two files, one type. `mod.rs` declares the struct, `impl AiEngine`, and the per-frame
housekeeping that must run whether the panel is drawn or not (settings load once, save when
dirty, `.status` re-asked when the package root changes, the forecast re-armed on a settled
region SIZE, the run drained). `actions.rs` is a second inherent `impl` block: the fold of the
panel's intents (`apply_intents`) and the `start_*` / `poll_*` pair of every long operation
(token, access check, download and its cancel, file dialogs, reference reads, unload).

```
start_x()  -> validate, claim the progress generation if it streams, ms_thread::spawn
poll_x()   -> try_recv, apply the outcome, clear the receiver
```

## Files and submodules
- `mod.rs`: `Flux3KleinEngine`, `Flux3PickerPurpose`, `new` / `at_rest`, the accessors the panel
  and the gate read (`model_readiness`, `status_for_current_root`, `download_check_current`,
  `pipeline_busy`, `non_run_pipeline_busy`), `profiles_request`, `run_input`, `frame_constraints`,
  `impl AiEngine`.
- `actions.rs`: `apply_intents` and the long operations.

## Contracts and invariants
- **Nothing blocks the GUI thread**; `poll` only drains channels and is never conditional on
  the panel being drawn.
- **A user edit outranks a late settings load** (`poll_settings_load` adopts the file only while
  `dirty` is clear) and nothing is written before the load landed (`SettingsSaver`).
- **One query of a kind in flight**: `.status` by `status_wanted`, the forecast by the
  controller's arming; the status answer counts only for the root it was asked about.
- **The forecast asks** the `flux3_klein` adapter with the selected format, the normalized params
  and `shape.references` = the region first, then each user reference; its gate also needs a
  non-empty root (the backend refuses a forecast without one).
- **The run gate reads `non_run_pipeline_busy`** (a download), never the run itself; a run started
  while references are still being read is refused with its own sentence.
- **A moved frame cancels the run** (the answer would describe another place); a settled resize
  arms the forecast once, a pure move arms nothing.

## Editing map
- What the host sees (layers, constraints, marks modes, the run): `impl AiEngine` in `mod.rs`;
  what a request becomes: `run_input`.
- A new button that starts something long: a `start_*` / `poll_*` pair in `actions.rs`, drained
  from `AiEngine::poll`, its receiver in `pipeline_busy` if it holds the backend.
- Tests sit at the end of each file and use `../test_support.rs`.
