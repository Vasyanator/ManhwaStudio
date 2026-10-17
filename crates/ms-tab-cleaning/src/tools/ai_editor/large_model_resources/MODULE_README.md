# Module: crates/ms-tab-cleaning/src/tools/ai_editor/large_model_resources

## Purpose
The two-axis memory widget of a LARGE-model cleaning engine: «Видеопамять» ×
«Оперативная память» pickers, the per-step forecast rows, the selected forecast, the text
encoder's line, the reclaim note, the refusal with the backend's suggestions, the learned
floor with «Забыть», and the folded «Пределы» cap editor. Drawn inside an engine's own
«Память и скорость» section from one `large_models.profiles` answer — plus the engine-side
state behind it (`ResourcesController`), which every large-model engine owns one of.

## Architecture
```
engine owns one ResourcesController (ProfilesQuery + arming, LimitsWriter, reset worker)
  -> controller.view(connected) -> ResourcesView for one frame
  -> draw_large_model_resources(ui, id_salt, &mut StepChoice, Option<&mut format>,
                                Option<&mut encoder_on_cpu>, view)
  -> Vec<ResourcesAction>   -> controller.fold(actions, &mut choice, format) after the frame
engine poll: controller.poll_side_jobs(now), controller.poll_query(region, gate, build)
```
The wire types, labels and step vocabulary come from `ms_local_ai::large_models` (the one Rust
client of `large_models.*`); the cap editor is `ms_widgets::ByteCapEdit`.

## Files and submodules
- `controller.rs`: `ResourcesController` — the forecast query and its one-shot arming
  (`arm` for a changed QUESTION, `invalidate` for a changed MACHINE), `poll_query` (asks only
  with a frame and the engine's gate open; the question is the engine's `build` closure),
  `poll_side_jobs` (cap writer + floor reset; a landed one invalidates the forecast), `view`,
  `fold` (step suggestions into the engine's pair, a format only where the engine has formats,
  a cap edit for its own axis, «Забыть»), `wants_repaint`. Its tests use an offline cap writer.
- `mod.rs`: `ResourcesView` (+ `reborrow`), `ResourcesAction`, `draw_large_model_resources`, and the pure
  line decisions `step_row_label`, `auto_item_label`, `forecast_line` with their tests plus the
  catalog test over every `large_models.*` key (en and ru agree on placeholders; the Russian axis
  names are pinned verbatim).

## Contracts and invariants
- **The widget draws only.** No IPC, no thread, no disk in `mod.rs`: everything the user does
  leaves as a `ResourcesAction`; the step pair is written into the `StepChoice` it was handed.
  The workers are the controller's, and none of them blocks the GUI thread.
- **Step semantics are the backend's** (`runtime/large_models/tiers.py`). Nothing here decides
  whether a step fits; the rows say what the answer says. The forecast never blocks a run.
- **Model-agnostic.** A model with one weight format passes `format: None` (FLUX.2); one with
  several passes `Some(&mut format)` (FLUX.3 klein) and gets the format rows and the format suggestions. A model
  with a separate text encoder passes `Some(&mut encoder_on_cpu)` and gets «Энкодер на CPU»
  (raises `EncoderOnCpuChanged`; the engine persists the flag and sends it as `encoder_on_cpu` in
  its run AND forecast `params`, honouring it is the backend's). With the flag on, the encoder's
  forecast line names RAM only (`encoder_line`). No model name appears here.
- **A stale answer is shown weak, not hidden**, so the panel does not flicker while the next one
  is asked; the busy spinner says a newer one is coming.
- **A cap edit names its axis** (`LimitsEdited(Axis)`, `cap_changes_for`): the engine's
  `LimitsWriter` sends that axis only and `Keep` for the other, so two editors (this widget and
  the settings pane) never undo each other's change (`ResourcesController::fold`); the display reconciles from the limits in
  force the backend answers.
- **The caps are edited only while the backend runs** (it is their only writer); offline the
  fold says so and draws no editor. A raise-limits suggestion opens the fold on the frame it is
  shown.
- Stored widget state is namespaced by the caller's `id_salt` (both pickers and the fold).
- `forecast_line` is the one formatter of a plan's forecast; an engine's readiness line prints
  it too, so the two surfaces cannot read differently. `{total}` is a REPEATED run
  (`Forecast::warm_total_s`), `{cold}` the first run end to end.

## Editing map
- To change what a picker row or the forecast line SAYS: `step_row_label` / `forecast_line`
  and their tests; the texts are `large_models.*` keys in `crates/ms-i18n/locales/{en,ru}.json`.
- To add an action the widget offers: a `ResourcesAction` variant, then every engine's fold.
- To change the wire shape or a label of a step: `crates/ms-local-ai/src/large_models.rs`.
