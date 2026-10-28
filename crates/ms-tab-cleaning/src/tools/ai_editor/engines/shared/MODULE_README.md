# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/shared

## Purpose
The model-agnostic building blocks the LARGE-model engines of the «ИИ-редактор области» share
(FLUX.2 klein and FLUX.3 klein; both use the prompt library and can run on PyTorch or on
Stable-Diffusion.cpp). Each lives here so every large-model engine composes it
instead of copying it. An engine adds only what is its own: its
request headers, its settings document and migrations, its staleness rules, its sentences.

## Architecture
```
engine (flux2_klein/, flux3_klein/)
  -> stream_progress     EngineProgress + generation guard, rate window, frame parser,
                         stream_call (id-exposing streaming call), spawn_backend_cancel
  -> stream_progress_ui  draw_stream_progress[_staged] (one bar, a second while a download names
                         a file; the staged variant names a frame by the engine's stage label)
  -> hf_download         HfTokenControl + HfTokenAction (the token field's secret-store worker)
  -> hf_download_ui      draw_hf_token_row, token_save_button_label, gated_button
  -> (re-export)         read_settings_file, write_settings_json, SettingsSaver — owned by
                         region_edit_v2::engine_settings_file
  -> region_mask         mask_for_run
  -> postprocess_section draw_postprocess_section (the «Постобработка» chrome, bottom-docked)
  -> prompt_library      PromptLibraryListing / Text / Entry + parsers (protocol 23 EntryInfo),
                         find / derived_pick / track_uncached / apply_pick / sync_preset_name
  -> prompt_library_ui   draw_prompt_library_picker (SearchableComboBox, row «Нет» + presets)
  -> prompt_cache        PromptCacheController (operation in flight, .list arming, name field,
                         export capture) + PromptCacheEffects, cache_button / prompt_cache_gates
                         over the engine's CacheContext, prompt_cache_line, the .prompt_cache.*
                         header / parsers / jobs, the two file-dialog requests
  -> prompt_cache_ui     draw_cache_row, draw_prompt_cache_line, draw_prompt_library_toggle,
                         draw_prompt_cache_library (picker + Export / Import + outcome lines)
  -> catalog_download    catalog_gated_repos_missing, catalog_bundle_missing, catalog_job_running,
                         catalog_switch_block_reason (an engine fed by the app's model catalog)
  -> sdcpp_options       SdCppRunOptions + FlashAttentionChoice (persisted per engine), to_context
  -> sdcpp_run           SdCppRunJob (start / poll / cancel) over EngineProgress + AbandonedJobs,
                         SdCppGenerate seam, read_sdcpp_choice, spawn_sdcpp_unload   (native only)
  -> sdcpp_ui            draw_runtime_switch, draw_sdcpp_options, draw_sdcpp_unload_button,
                         SdCppReadiness + draw_sdcpp_readiness (native only)
```
`mod.rs` re-exports every submodule flat, so an engine writes `use super::shared::*;`. The
forecast / caps / learned-floor half of «Память и скорость» is NOT here: it is
`../../large_model_resources/` (`ResourcesController` and the widget), shared the same way.

## Files and submodules
- `mod.rs`: the module root and the flat re-exports.
- `stream_progress.rs`: `EngineProgress` (one per engine, claimed by GENERATION), `ProgressClaim`
  + `CallControl` (the sticky cancel handshake), `AbandonedJobs`,
  `RateEstimator` + `RateSource` (a time window over ONE byte counter: five seconds over a
  download's overall counter, one second over the per-file counter of any other frame that
  carries the byte fields, restarted per file
  and component), `ProgressFrame` / `parse_progress_frame` (the optional second level with its
  optional `file_component`, localized by `file_component_label`), `RunDegraded` / `parse_degraded`,
  `stream_call` + `map_call_error`, `spawn_backend_cancel`, and the rate / ETA formatters.
- `stream_progress_ui.rs`: `draw_stream_progress` (the backend's `label` text; a load's file bar
  names its component, and the speed line shows under the file bar of ANY frame that carries the byte
  fields — download, load, encode or a denoise re-read) and
  `draw_stream_progress_staged` (a load / generate frame named by the engine's localization of
  its stable `stage` id, the `label` as fallback for an unknown id).
- `hf_download.rs`: the token field's worker (`HfTokenControl`: the entry buffer, the store /
  delete worker, the line about the last operation) and `HfTokenAction`. Both engines download
  through the app's model catalog, which reads the stored token itself.
- `hf_download_ui.rs`: the token row (`HfTokenView`, caller-given field id, an optional
  `ms_widgets::HfAccessHint` beside the badge whose last step names `token_save_button_label`)
  and `gated_button`.
- the settings-file rules (`read_settings_file`, `write_settings_json`, `SettingsSaver` +
  `SaveGate`) are re-exported by `mod.rs` from their owner
  `../../../region_edit_v2/engine_settings_file.rs` (shared with the engines outside
  `engines/`); see `region_edit_v2/MODULE_README.md`.
- `region_mask.rs`: `mask_for_run` — painted -> verbatim + `whole_region = false`; empty -> a
  SOLID buffer + `whole_region = true`.
- `postprocess_section.rs`: `draw_postprocess_section(ui, id, &mut PostprocessSettings) -> bool` —
  `ms_widgets::panel_dock::elastic::bottom_aligned` around the region-editor section chrome
  (collapsed by default, titled `ms_region_postprocess::section_title()`) around
  `ms_region_postprocess::draw_postprocess_body`. For an engine whose runs answer raw pixels
  (`AiEngine::postprocess_settings`); the host re-derives a pending result from what it edits.

- `prompt_library.rs`: the model-agnostic half of a model's prompt-cache LIBRARY (GUI-free): the
  `EntryInfo` / `.prompt_cache.list` shapes and their tolerant parsers (every field optional; a
  row with nothing to pick is dropped), the exact-text preset match (`find`: trimmed, case- and
  inner-whitespace-sensitive), the derived picker selection, the «Нет» stash rule
  (`track_uncached`), the pick rule (`apply_pick`), the name-field sync (`sync_preset_name`) and
  the display helpers (`default_preset_name`, `preview_line`, `format_library_created`).
- `prompt_library_ui.rs`: `draw_prompt_library_picker` — the labelled `SearchableComboBox` of
  presets (row 0 «Нет»; primary = preset name, secondary = the encoder of the entry in use and its
  date, hover = the prompt and every entry's encoder); the encoder caption is the engine's closure.
  Not re-exported: an engine draws it through `prompt_cache_ui.rs`'s library fold.
- `prompt_cache.rs`: the model-agnostic CONTROLLER of the library (GUI-free). The engine hands
  in `PromptCacheMethods` (its four method names, the build / query timeouts, its log label)
  and a `CacheContext` (the typed text, whether an encoder is configured, the current encoder's
  provenance when known, its precision ranking and fp8 id, an optional engine-specific block
  reason). It decides «Кэшировать» / «Переименовать» / closed (`cache_button`; an entry of the
  current encoder, or a strictly more precise one of the same source, closes it unless `.status`
  says the prompt is not served) and the export / import gates, picks the one cache line
  (`prompt_cache_line`), reads the cache half of `.status` (`parse_prompt_cache_status`) and
  owns the jobs: `PromptCacheController` keeps the one operation in flight (build streams on the
  engine's progress bar), the `.list` query and its one-shot arming, the name field, the export
  file captured when its dialog opened, the fold flag and the outcome lines; `poll` /
  `poll_list` return `PromptCacheEffects` (re-ask `.status`, invalidate the forecast), which the
  engine folds into its own queries.
- `prompt_cache_ui.rs`: the drawing half — the name field with «Кэшировать» /
  «Переименовать» (`draw_cache_row`, returns the press), the cache line, the
  «Библиотека промптов» toggle and the fold (`draw_prompt_cache_library`: picker, «Экспорт» /
  «Импорт», listing error, outcome, warning, the engine's `PromptCacheNote`s, skipped files)
  over a one-frame `PromptCacheBlockView` (the caller's two id salts and encoder caption).

- `catalog_download.rs`: the pure decisions of an engine whose weights come from the app's model
  catalog through the shared `ms_tab_translation::ocr_model_download` controller (GUI-free): the
  gated repositories of a bundle not seen installed (`catalog_gated_repos_missing`: what draws
  the token row), whether a bundle state says files are absent (`catalog_bundle_missing`),
  whether any controller job runs (`catalog_job_running`: its falling edge re-arms the engine's
  probes) and why a running job closes the engine picker (`catalog_switch_block_reason`, texts
  `cleaning.tools.catalog_download.*`). The engine keeps its own bundle (which specs a selection
  needs).

- `sdcpp_options.rs`: `SdCppRunOptions` — what ONE engine persists for its Stable-Diffusion.cpp
  runs (text encoder on the CPU, default on; weights in RAM; VRAM budget `None` = «Авто», else
  1..=128 GiB; VAE tiling; `FlashAttentionChoice` with frozen ids `auto` / `on` / `off`, an unknown
  value reading as auto). `#[serde(default)]`, unknown fields ignored. `to_context(&SdCppChoice)`
  joins them with the APP-WIDE device and thread count (Settings › ИИ бэкенд ›
  Stable-Diffusion.cpp). The types compile everywhere; `to_context` only natively.
- `sdcpp_run.rs` (native only; `ms-sdcpp` is empty on wasm): `SdCppRunJob::start(input, &progress,
  log_label)` claims the engine's progress generation on the GUI thread and runs on a worker:
  `AiRouteKind::SdCpp.availability(local_ai::process())` (the `--no-ai` / platform gate), the
  `user_config` `SdCppChoice` (`read_sdcpp_choice`, worker-only), the catalog build, the
  `GenerateJob` (request `vae_tiling` from the options), `ms_sdcpp::service().generate`. Stages
  become counter-less bar labels (`cleaning.tools.sdcpp.stage_*`), steps the step counter.
  `poll()` answers `Option<Result<egui::ColorImage, String>>` (converted on the job's worker by
  `rgb_to_color_image`, never on the GUI thread; localized error); `cancel(self,
  &mut AbandonedJobs, log_label)` raises the flag, parks the receiver and retires the generation;
  dropping a job raises the flag too. `SdCppGenerate` is the service seam (implemented for
  `SdCppService`, faked in the tests). `spawn_sdcpp_unload` stops the worker process off the GUI
  thread. `sdcpp_route_available()` is THE "can this process run sd.cpp at all" answer every
  engine uses (whether a Torch hint may suggest sd.cpp, whether the switch is offered, whether
  the automatic switch may move there): `AiRouteKind::SdCpp.availability(local_ai::process())`
  plus a catalog build offered here (`ms_sdcpp::offers_a_build_here`). It says nothing about the
  install; the picker's `AiButton` gate reads `AiCaps` (route + `ms_sdcpp::ready`).
- `sdcpp_ui.rs`: `draw_runtime_switch(ui, &mut ImageEditRuntime, blocked, pytorch_unavailable,
  id_salt)` — the PyTorch / Stable-Diffusion.cpp toggle pair at the TOP of an engine's panel,
  disabled with a hover reason while `blocked`; the PyTorch side alone also while
  `pytorch_unavailable_reason(&AiCaps)` says so (strict, the `AiButton` signal; the sd.cpp side
  stays open, its readiness line offers the install); `sdcpp_auto_switch` (pure) +
  `log_sdcpp_auto_switch` — the ONE rule both FLUX engines apply in `AiEngine::on_selected`:
  on PyTorch, PyTorch OUT OF REACH (Torch known absent, or the backend down —
  `AiCaps.backend == Some(false)` — while no start is underway, `AiCaps.backend_starting`),
  sd.cpp offered and the switch open -> move to sd.cpp through the click's own path. Never per
  frame, never on unknown, and never on settings that have not been loaded yet (an engine defers
  the decision until its settings file landed: a change before it would discard the file); `draw_sdcpp_options(ui, &mut SdCppRunOptions, id_salt)` (no heading: the caller's section
  is titled `cleaning.tools.sdcpp.options_heading`);
  `draw_sdcpp_unload_button(ui, enabled)` (the caller passes `worker_running` and no run of its
  own in flight); natively `SdCppReadiness::of(status, local, device)` (disabled, checking, no
  build for this machine, not installed, requirement, probe failed, device missing, ready;
  `run_block_reason` closes the run on all but the last two) and `draw_sdcpp_readiness`, which also
  asks for the first status refresh when none ever ran.

## Contracts and invariants
- **Nothing names a model.** Log lines take a `log_label`; user texts are
  `cleaning.tools.engine_progress.*` / `cleaning.tools.hf_download.*` /
  `cleaning.tools.prompt_library.*` / `cleaning.tools.catalog_download.*`; a sentence that names the
  model (its settings-file notices, its download heading and hover) stays in the engine and
  reaches shared code only as an already-localized string or a `describe` closure.
- **Nothing blocks the GUI thread.** Every call, keyring access and file write runs on an
  `ms_thread` worker; the GUI side only drains channels. The progress lock is held for a field
  copy and nothing more (`draw_stream_progress` copies out, then draws).
- **A frame's `stage` is an optional stable id** (`ProgressFrame::stage`, FLUX.3 klein): it is
  cleared by every claim and by every frame without one, and only an engine that knows the id
  localizes it; the backend's `label` is log text and stays the fallback.
- **Cancellation is sticky and per operation** (`CallControl`, inside the `ProgressClaim`
  `begin_progress_generation` returns): the worker publishes the request id to its own
  control, `retire_progress_generation(progress, label)` requests the cancel, and whichever comes
  second sends EXACTLY ONE backend cancel — a cancel before `begin_call` returned is no longer
  lost. Every claim gets a fresh control. The id never travels through a generation-filtered
  display write.
- **A cancelled operation is waited for** (`AbandonedJobs`): the engine keeps the abandoned
  receiver and counts it as busy until the worker ends, so quick start/cancel cycles cannot stack
  backend requests.
- **One progress bar per engine, claimed by generation.** A write from a retired generation —
  including its terminal "the bar is done" — is dropped (`update_progress`).
- **The HF token is never persisted, logged or formatted here.** It reaches the wire only through
  the header the ENGINE builds; `HfTokenControl::input` is the entry buffer, cleared when the
  store worker takes it.
- **A damaged settings file is never overwritten**, and an earlier aside copy is never replaced.
- **A settings file is replaced atomically** (temp + sync + rename; `std::fs::rename` replaces an
  existing file on Windows too: `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`), so a crash leaves
  the old or the new complete document, and a dragged dial costs ONE write after it settles.
  An engine wakes its GUI per `SettingsSaver::wake_after` instead of repainting every frame.
  The value still owed at teardown is written synchronously by the studio teardown
  (`AiEngine::flush_settings_blocking` -> `SettingsSaver::flush_blocking`); every engine with a
  saver implements that hook.
- **The «Постобработка» section is the LAST thing of the scrolled body**, drawn directly in the
  `Ui` the host hands `draw_parameters` (a `push_id` child is fine), never in a nested
  `ScrollArea` or a pinned section: `bottom_aligned` measures the free room against that `Ui`.
  Its `id` is stable and never localized. The body, defaults and ranges belong to
  `ms-region-postprocess`; never copy a dial into an engine.
- **The prompt library renders the backend's selection, never re-derives it.** Which entry a run
  uses is `selected` / `prompt_cache_entry`; this side only matches the typed text to a preset.
  The picker's selection is DERIVED from the prompt field every frame and never stored; choosing a
  row writes the preset's TEXT (or the «Нет» stash) into the field and nothing else — the
  embeddings are read by the run.
- **A preset name is normalized exactly as the backend stores it** (`normalize_preset_name`:
  the explicit `NAME_REMOVED_RANGES` table, trim, 100-character cap). The table mirrors
  `modules/ai_backend/runtime/prompt_library.py`; both sides' tests read
  `modules/ai_backend/runtime/fixtures/preset_name_normalization.json`, so change the rule only
  together with that file and the Python side.
- **One prompt-cache operation at a time, and the controller never re-asks the engine's
  queries itself.** `PromptCacheController` refuses a second build / export / import while one is
  in flight (the engine additionally gates on its whole-pipeline busy flag); a finished build
  re-arms `.list`, and `.status` / the forecast are the ENGINE's — the controller only returns
  `PromptCacheEffects`. A listing that differs from the one held (the first one included: the
  first `.list` of a process migrates old caches) is such an effect.
- **sd.cpp runs never touch the Python backend.** A run's progress claim never publishes a
  backend request id, so retiring it sends no backend cancel; cancellation is the run's own flag
  (the service sends `Cancel` to its worker process and kills it after the grace period). The
  run is still parked in `AbandonedJobs` until its worker ends: `ms_sdcpp` runs one generation at
  a time and answers a second one `Busy`.
- **No sd.cpp I/O on the GUI thread.** The `user_config` choice is read on the run worker (and,
  for the readiness device, on the engine's status worker through `read_sdcpp_choice`); the
  readiness reads the status snapshot the caller took; unload goes through `spawn_sdcpp_unload`.
  The studio teardown stops a running sd.cpp worker (`../../../../studio_tab.rs`).
- **The runtime choice is per engine and persisted by `ImageEditRuntime::wire`**; the device and
  thread count are app-wide and never copied into an engine's file.
- Visibility ceiling: `pub(in crate::tools::ai_editor::engines)`.

## Editing map
- To change how the bar looks or what its lines say: `stream_progress_ui.rs` and the
  `cleaning.tools.engine_progress.*` keys; its arithmetic: `stream_progress.rs`.
- To change the token row for every engine: `hf_download_ui.rs`; the token field's worker:
  `hf_download.rs`.
- To change the quarantine naming, the temp naming, the save debounce or backoff, or the
  teardown flush: `../../../region_edit_v2/engine_settings_file.rs`.
- To change where the «Постобработка» section sits or its chrome: `postprocess_section.rs`; its
  dials and texts: `ms-region-postprocess` (`ui.rs`).
- To change how a library listing is read, what the typed text matches or what «Нет» keeps:
  `prompt_library.rs`; how the picker looks: `prompt_library_ui.rs` and the
  `cleaning.tools.prompt_library.*` keys.
- To change when «Кэшировать» / «Экспорт» / «Импорт» open, which line the prompt block shows,
  what a finished operation re-asks or how a `.prompt_cache.*` call is made: `prompt_cache.rs`;
  how the cache row and the library fold look: `prompt_cache_ui.rs`. An engine adds only its
  `PromptCacheMethods`, its `CacheContext` builder and its request `params`.
- To change which catalog states count as "missing" or "running", or the engine-picker block of
  a catalog job: `catalog_download.rs` and the `cleaning.tools.catalog_download.*` keys.
- To change what an sd.cpp run sends or how it reports: `sdcpp_run.rs`; the persisted sd.cpp
  options: `sdcpp_options.rs`; the switch, readiness line and options widgets: `sdcpp_ui.rs` and
  the `cleaning.tools.sdcpp.*` keys (catalog test in `sdcpp_ui.rs`).
- Tests sit at the end of each file; the catalog tests (every key in every shipped locale, with
  its placeholders; the retired download-block keys absent) are in `hf_download_ui.rs`,
  `stream_progress_ui.rs`, `catalog_download.rs` and `prompt_cache.rs` (the moved prompt-library
  keys present, their former `cleaning.tools.flux2_klein.*` ids gone).
