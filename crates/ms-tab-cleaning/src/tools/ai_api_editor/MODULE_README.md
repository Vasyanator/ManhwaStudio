# Module: crates/ms-tab-cleaning/src/tools/ai_api_editor

## Purpose
The «ИИ редактирование (API)» cleaning tool: a `HostSpec` for the generic region-editing host
(`../region_edit_v2/host.rs`) whose catalog is ONE engine, «Облачные модели» — hosted image-edit
models (OpenAI, Gemini, FLUX, Qwen, resellers, the user's own OpenAI-compatible server) reached
over their HTTP APIs through `ms_ai_api::image_edit`. Torch-free and backend-free: a run needs
only the network and the provider's key. The frame, the mask, mask generation, the pending
result and the apply path are the host's.

## Architecture
```
tab.rs -> tools::ai_api_editor_tool() -> ai_api_editor::tool() -> RegionEditHost::new(&AI_API_EDITOR_SPEC)
AI_API_EDITOR_SPEC.catalog = [CloudEditEngine]     (one engine: the host hides its picker)

CloudEditEngine (engine.rs)
  draw_parameters  draw_image_edit_picker (provider + Russia badge, endpoint, key block, model,
                   offer notes) -> prompt -> blend -> size line -> billing/privacy
  draw_progress    run state: spinner + stage + cancel-billing note, or the last outcome
                   (pinned above the scrolled parameters by the host)
  constraints()    selection.offer() -> constraints::frame_constraints(rule)  (host re-reads per frame)
  marks_support()  offer.accepts_references() ? reference (preferred) + layer + overlay : overlay only
  run_block_reason decisions::run_block_reason(RunGate snapshot)
  start            geometry::upscale_factor_for -> k; worker::spawn_run(RunJob, CancelFlag)
  poll             settings load -> key slot + ImageEditKeyRunner::pump -> settings save -> run events
worker.rs          read_key -> RgbaRegion (+ marks_reference) -> run_image_edit(&req, key, cancel, on_stage) -> ColorImage
```

## Files and submodules
- `mod.rs`: `AI_API_EDITOR_SPEC` (tool id `"ai_api_editor"`, title key
  `cleaning.tools.ai_api_editor.title`, id salts `cleaning_ai_api_editor_mask_generation_section` /
  `cleaning_ai_api_editor_mask_source_picker`, log tag `[cleaning/ai_api_editor]`), `tool()`.
- `engine.rs`: `CloudEditEngine` — state, `AiEngine`, the panel body, the per-frame polling of the
  settings, key-block and run workers.
- `constraints.rs`: `frame_constraints(&ImageSizeRule) -> FrameConstraints`, a field-for-field copy.
- `decisions.rs`: `RunGate` and the pure `run_block_reason` (order pinned by a test).
- `settings.rs`: `ApiEditSettings` in `ms_config::ai_api_edit_settings_path()` — selected provider,
  per-provider `{model_id, endpoint}`, prompt, dilate, feather; load/save on workers; the test
  persistence latch.
- `worker.rs`: `RunJob`, `WorkerEvent`, `spawn_run`, `marks_reference` — one run per
  `ms_thread` worker.

## Contracts and invariants
- **The size rule has one owner: `region_edit_v2::geometry`.** The selected offer's rule is DATA
  copied 1:1; `k` comes only from `upscale_factor_for`, and `ms_ai_api` only asserts the answer is
  exactly `k·W × k·H`. A wrong size is an error (`SizeMismatch`), never a resample; the host's D7
  check of the result stays in force.
- **A model switch re-validates, never resizes.** `constraints()` follows the selection and the
  host pushes it every frame: an incompatible rectangle turns red and blocks «Обработать».
- **Marks follow the model.** `marks_support()` is derived from the selected offer every frame:
  a model with `max_extra_references > 0` takes the marks as the reference image (the region
  with the marks composited, opaque; or the marks layer alone, straight alpha), else only drawn
  onto the region (the panel says so). Separate marks for a model without references are
  refused in `start` (localized, logged), never sent without them; the conversion to the
  reference raster runs on the worker, the PNG encoding in the pipeline.
- **The mask means "may change here"** (stated in the panel); `allows_empty_mask` is always true
  and an empty mask means the whole region. One mask layer, constant.
- **No blocking work on the GUI thread.** Settings IO, every credential-store operation (key check,
  save, delete through `ImageEditKeyRunner`; the run's key read in `worker.rs`) and the run itself
  are on workers. Key-block clicks are queued in the panel and pumped in `poll`, because the panel
  is not drawn every frame. The key check starts only on the first `poll` (tool active).
- **Secrets and privacy.** The API key never enters the settings file, a log line or a message; the
  prompt is never logged here (the pipeline logs its length). Errors reach the user as the
  localized `ImageEditError` text and the log as one line with the provider, model (the run's
  own snapshot in `RunInFlight`, not the picker's current selection) and error.
- **Cancel detaches.** The cancel flag stops the pipeline between steps (the executor then sends
  the provider's cancel request); an HTTP call already in flight completes and may be billed —
  the panel says so while a run is in flight. The flag is raised by `cancel`, by dropping the
  engine (`Drop`: the tab torn down without `deactivate`), and by the worker itself when its
  event channel is closed.
- **Settings.** A user edit outranks a load that lands later; nothing is written before the load
  landed (`settings_save_due`); unknown provider keys are skipped, never remapped; a test process
  never writes the file: `save_api_edit_settings` refuses under `cfg!(test)` OR once the sticky
  latch is armed by the test-only door `suppress_settings_persistence_for_tests` (re-exported at
  the crate root behind the `test-support` feature for other crates' tests that build the tab).
  The tests never `poll` the engine, so they never touch the credential store either.
- **Frozen values**: the spec's id salts and `PICKER_ID_SALT` / `PROMPT_ID_SALT` /
  `BLEND_SECTION_ID_SALT` key stored egui state and must stay distinct from `ai_editor`'s.

## Editing map
- To change the tool's id, title, salts or log tag: `AI_API_EDITOR_SPEC` in `mod.rs`.
- To change what the panel shows or how a run is started / polled / cancelled: `engine.rs`.
- To change when a run is refused, or the order of the reasons: `decisions.rs`.
- To change what is persisted: `settings.rs` (keep old documents loadable: every field defaults).
- To change providers, models, size rules, key slots, the picker widget or the HTTP pipeline:
  `crates/ms-ai-api/src/image_edit/`, never here.
- To change what the frame does, the picker-hiding rule or the apply check: `../region_edit_v2/`.
