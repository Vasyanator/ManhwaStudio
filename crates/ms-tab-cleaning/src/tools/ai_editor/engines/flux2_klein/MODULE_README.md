# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein

## Purpose
The FLUX.2 klein engine of the «ИИ-редактор области» tool: one implementation of
`region_edit_v2::engine::AiEngine`. It owns everything model-specific — the persisted
parameters, the two memory steps and their large-model forecast, its side of the model's shared
prompt library, the model download (the app's catalog), the per-component residency, the request/response wire contracts, the
OOM recovery, the worker threads and its own progress bar — and nothing host-specific:
the on-canvas frame, the painted mask stack and the pending result belong to the host.

The module path is `engines::flux2_klein`. `Flux2KleinEngine` is the only `pub` item;
everything else is `pub(super)` at most and is re-exported into the module root, so the
rest of the tree sees one flat `flux2_klein::*` namespace.

TWO ENGINES, ONE IMPLEMENTATION. `config::Flux2Variant` (`Klein9B` / `Klein4B`) is the
parameter, and `all_engines` builds one `Flux2KleinEngine::new(variant)` per value. The
variant keys the engine id, the picker caption, the model directory, the derived component
paths and the settings FILE. It is fixed at construction and never changes for an instance.
The enum itself and every fact that keys a PATH, a persisted document or an id (`wire`,
`from_wire`, `dir_name`, `settings_file_name`, `engine_id`, `size_token`, `all`) are declared in
ONE inherent `impl` in the `ms-config` crate (`crate::config`); what a variant OFFERS (encoder
sources, weight formats per component and source) is `ms_config::flux2_weight_format`; which
repositories are gated is catalog data. Its PRESENTATION — the localized picker caption —
lives HERE, in `variant_presentation.rs`, as the `Flux2VariantPresentation` extension trait (an
inherent `impl` may only be written in the crate that DEFINES the type).

BOTH PACKAGES COME FROM THE APP'S CATALOG (`ms_sysprobe::ai_models::external_catalog`,
`flux2_klein_package_specs`); the backend downloads nothing for FLUX.2 (its `.download.*` were
retired at protocol 22). The variants differ only by what they offer: 9B has the uncensored
text encoder (`Flux2EncoderSource::Uncensored`, persisted as the historic bool
`uncensored_text_encoder`; its own formats lack `Q5_K_M`) and gated repositories (BFL 9B,
ponpoke), so its panel draws the «Расцензуренный энкодер» toggle and — while a gated spec of
the bundle is not installed — the Hugging Face token row; 4B offers the stock encoder only and
its repositories are open, so neither is drawn and the flag is inert (`encoder_source`). The
9B bf16 transformer is the BFL single file, unless the probe found an install of the backend's
former download (diffusers `transformer/`), which is then used through the catalog's adopt-only
legacy spec (`flux2_klein_transformer_layout`, runtime `installed_formats.transformer_layout`).

Everything else, on screen and on the wire, is identical: every method is keyed by the three
component paths, and each variant has its own prompt LIBRARY (`params.prompt_library` =
`Flux2Variant::prompt_library_id`, protocol 23) that every encoder source and format of that
variant shares. In download mode both
variants send the selected weight formats with their candidates (see "Contracts and
invariants").

Only ONE variant is resident at a time — the backend holds one pipeline and one text
encoder, keyed by those paths — so selecting the other engine unloads this one. This side
orchestrates nothing and only says so, in one line at the top of «Установка модели».

TWO RUNTIMES, ONE ENGINE. `settings.runtime` (`ms_config::ImageEditRuntime`, persisted as
`pytorch` / `sdcpp`, absent = PyTorch) is the switch at the TOP of the panel. On PyTorch
everything below holds as written. On Stable-Diffusion.cpp the run goes to the sd.cpp worker
process through the shared `SdCppRunJob` (`../shared/sdcpp_run.rs`): the files come from
`sdcpp_files.rs` (resolved on the engine's sd.cpp status worker), the request from
`sdcpp_request.rs`, and nothing is asked of the Python backend (`.status`, the forecast and the
prompt-cache listing are not armed). PyTorch-only features are HIDDEN there, never faked: the
memory forecast, the component list and unload, the prompt library and «Кэшировать», dtype /
fp8 / guidance, «Внимание только к маске» (a diffusers attention option). The «Stable-Diffusion.cpp» section (the shared options and «Выгрузить
модель») takes the place of «Память и скорость».

## Architecture
`mod.rs` is the root and holds NO logic: the module header, the module declarations and
the constants everything below shares (the selection limits, the parameter ranges, the
mask tint and `FLUX2_DEFAULT_PROMPT`; status lines use the shared `ms_theme::status` colours).
Every other item lives
in a submodule.

The root declares each child as `mod x; use x::*;`, and each child starts with
`use super::*;`. A private `use` is visible to a module's descendants, so the glob chain
carries the root's imports and every sibling's items down through `ui/` and `engine/`
too: a file here may name any item of this module regardless of which file holds it, and
never through a `super::sibling::` path. The one exception to the glob is
`Flux2KleinEngine`, which is `pub` and is re-exported explicitly — a private glob would
not carry it out of the module.

Three layers, and the direction between them is one-way:

```
ui/            draws            -- calls decisions.rs and the engine's accessors
decisions.rs   decides          -- pure, no Ui, no channel, no disk
engine/        owns the state   -- builds the panel context, runs the workers
```

Data flow of one edit:

```
host frame + painted mask (+ marks reference)
        -> AiEngine::start           (engine/mod.rs)
        -> run_input                 (engine/mod.rs) validates sizes, accepts the marks
        -> mask_for_run              (../shared)    derives the working mode
        -> settings.normalized()     (settings.rs)  the ONLY value put on the wire
        -> run_flux2_klein           (wire.rs)      worker thread: latent mask (dilate_ellipse
                                                    by mask_dilate_px), streaming call
        -> publish_progress_frame    (../shared)    generation-guarded bar writes
        -> AiEngine::poll            (engine/mod.rs) drains the channel, answers DoneRaw
        -> host post-process         (region_edit_v2) composite, feather, colour match with
                                                    AiEngine::postprocess_settings, live
```

Queries the panel arms are one-shot and independent of the run: `.status`
(`status.rs` + `wire.rs`), the memory forecast `large_models.profiles`
(the engine's `ResourcesController` from `../../large_model_resources/`: one request in
flight, latest wins, plus the debounced cap writer and the floor reset), `.prompt_cache.list`
(the shared `PromptCacheController`, driven with FLUX.2's methods from `prompt_cache.rs`). The catalog download is the engine's `OcrModelDownloadController`
(status probes, the download / verify job with its gated-repository access check, the
automatic adoption of files already on disk), polled from `AiEngine::poll`.
The forecast is drawn by the shared widget `../../large_model_resources/` (steps, per-step
rows, floor, «Пределы»).

MODEL-AGNOSTIC PARTS LIVE IN `../shared/` (`use super::shared::*` in `mod.rs`): the progress
state of all three long operations and its streaming call, the Hugging Face token controls
and their drawing, the settings-file quarantine and save backoff, `mask_for_run`, the
prompt library's listing shapes, preset rules and picker (`prompt_library*`), the prompt-cache
controller — the «Кэшировать» / export / import gates, the cache line, the `.prompt_cache.*`
jobs, the cache row and the library fold (`prompt_cache*`) — and the
model-catalog download decisions (`catalog_download.rs`: gated repositories still missing, bundle
state, running job, the engine-picker block and its texts). What
stays here is what is FLUX.2's own: its headers, its settings document and migration, its
sentences.

## Files and submodules
- `mod.rs`: the module root — the header, the `mod`/`use` wiring and the shared constants.
  Edit it to add a submodule or a constant several files share, and nothing else.
- `variant_presentation.rs`: the `Flux2VariantPresentation` extension trait — the picker
  caption of a `config::Flux2Variant`. Every OTHER fact of the enum is in `ms-config`.
- `engine/`: the engine. `mod.rs` holds `Flux2KleinEngine`, its `AiEngine` impl and the
  polling that keeps its derived state current; `actions.rs` holds the `start_*`/`poll_*`
  pair of every long operation. See `engine/MODULE_README.md`.
- `ui/`: the parameter panel. `mod.rs` is the body and its ORDER; `install.rs`,
  `components.rs`, `advanced.rs` and `mask_hint.rs` draw the blocks it calls into. See
  `ui/MODULE_README.md`.
- `decisions.rs`: the PURE decisions the panel renders — `flux2_readiness_line`,
  `flux2_encoder_caption` (the localized name of the encoder an entry was made by, which the
  shared cache line and picker show), `flux2_guidance_supported`, `flux2_run_block_reason`,
  `region_block_reason`, the catalog bundle of the selection (`flux2_catalog_bundle`; its
  state, the gated repositories still missing and the engine-picker block of a running catalog
  job are the shared `catalog_bundle_missing` / `catalog_job_running` /
  `catalog_gated_repos_missing` / `catalog_switch_block_reason` of `../shared/catalog_download.rs`)
  and the weight-format row captions (`flux2_weight_format_label` / `_hint`, `flux2_encoder_format_token`)
  — with the line/tone types
  they answer in. Edit it to change what a
  line or a refusal SAYS, or which control the backend's answer closes.
- `settings.rs`: `Flux2KleinSettings`, its defaults, `normalized()`, `params()` (with the
  protocol-23 library fields; the persisted «Нет» stash `uncached_prompt` never travels), the
  dtype vocabulary, the two step ids (`step_choice` / `set_step_choice`), the one-time
  migration of the retired placement (`legacy_memory_steps`, the one owner of the placement ->
  step-pair table; an untouched default document goes to «Авто» / «Авто»),
  `Flux2SourceMode`, the persisted `variant` and `effective_paths`, the encoder source
  (`encoder_source` over the historic `uncensored_text_encoder` bool; `set_encoder_source` maps
  the encoder format through `closest_offered`), the two weight formats
  (`transformer_format` / `text_encoder_format`: `ms_config::Flux2WeightFormat` wire ids, absent
  reads as `bf16`, unknown or not offered for the source normalizes to `bf16`; `weight_format`,
  `weight_selection`, `format_candidates`, the runtime-only `installed_formats` — per component
  and source, plus the bf16 transformer layout — and its worker probe
  `probe_flux2_installed_formats`), «Энкодер на CPU»
  (`encoder_on_cpu`, default on — also for a file without the field — and sent in every
  `params`, the forecast's included), «Постобработка» (`postprocess`: the shared
  `ms_region_postprocess::PostprocessSettings`, embedded `#[serde(flatten)]` so the file keeps
  its flat `mask_feather_px` / `color_match` / `color_match_local_pct` / `color_match_scale_px`
  keys; defaults and ranges are that crate's, and none of it is ever sent), and the settings-file
  load/save pair (`load_flux2_settings_from` / `save_flux2_settings`, over the shared
  `read_settings_file` / `write_settings_json`: a damaged file is moved aside, never
  overwritten, and when that fails saving is BLOCKED). Edit it to add or change a persisted field. ONE FILE PER VARIANT: 9B keeps `flux2_klein_settings.json` unchanged,
  the loader stamps the owning variant over whatever it read, and the saver writes back to
  the file the document itself names.
- `status.rs`: `Flux2Status` and everything derived from it — `Flux2ModelReadiness`,
  `flux2_status_for_paths`, the merged `flux2_component_rows`, `flux2_install_seed`,
  `flux2_pipeline_busy`. Edit it to change what a catalog MEANS; the parsers that build
  one live in `wire.rs`.
- `prompt_cache.rs`: the FLUX.2 BINDINGS of the shared prompt-cache controller — the method
  family and timeouts (`FLUX2_PROMPT_CACHE_METHODS`), the encoder context «Кэшировать» is
  decided on (`flux2_cache_context`: the effective encoder path, the download-mode provenance,
  the FLUX.2 precision ranking; an entry of the current encoder, or a strictly more precise one
  of the same source, closes it) and the request header (`flux2_prompt_cache_header`). The gates,
  the cache line, the four `.prompt_cache.*` calls, the controller state and the drawing are
  `../shared/prompt_cache*.rs`; the listing shapes, the exact-text preset match, the «Нет» stash
  and the picker are `../shared/prompt_library*`.
- `run_outcome.rs`: what one run answers with (`Flux2RunOutcome`: the raw decode and the latent
  mask; `Flux2JobResult`: plus the run's mask and `whole_region`) and the one
  memory flag it writes back (`parse_applied_flags`: `vae_tiling` only, tolerant of the v13 and
  v14 shapes). The progress state, its generation guard and `parse_degraded` are
  `../shared/stream_progress.rs`.
- `session.rs`: `Flux2SessionState` (run channel + cancelled workers), `Flux2RunPoll` (a
  finished run is the host's `RawRegionResult`),
  `apply_backend_flags` (writes `vae_tiling` only), `flux2_run_done_status`
  (a degraded run names its effective steps) and `flux2_picker_request` (the dialog request per
  picker purpose — the two prompt-cache dialogs are the shared `prompt_cache_export_dialog` /
  `prompt_cache_import_dialog`; the engine opens it through `ms_platform::file_dialog::request`
  and the shared controller commits the export's save target after the backend wrote it; the weight-file filters are
  per component — transformer safetensors or GGUF, VAE safetensors only, encoder FILE
  (`TextEncoderFile`) GGUF only).
- `wire.rs`: `Flux2RunInput` (the pixels of one run), `run_flux2_klein` (which builds the
  latent mask on the worker), the `.status` and
  `.component_action` calls (streaming through the shared `stream_call`) and their
  parsers, the prompt translation and the run request packing `flux2_run_request`
  (header + `image ++ mask [++ reference]` blob; the PNGs come from `tools/region_png.rs`).
- `sdcpp_files.rs`: which three FILES sd.cpp loads (`flux2_sdcpp_model_files`, worker only, it
  stats the disk): download mode = `external_catalog::flux2_klein_sdcpp_paths`; manual mode maps
  an encoder folder to its `model.safetensors.index.json` (or single `model.safetensors`), a VAE
  folder to `diffusion_pytorch_model.safetensors`, and refuses a transformer FOLDER (diffusers)
  with the hint to pick a single file or a GGUF; the found files are then checked with
  `ms_sdcpp::validate_model_files` (family, encoder pairing). `Flux2SdcppFilesKey` says when the answer is
  stale; `Flux2SdcppFilesError::message` is the refusal.
- `sdcpp_request.rs`: the sd.cpp request of a run (`flux2_sdcpp_prepare`, worker only): the
  SAME validation and latent mask as the PyTorch pass (`wire.rs`), init image and first
  reference = the region, second reference = the marks, guidance 1.0, the seed rule
  (`flux2_sdcpp_seed`: pinned or `-1` random); `color_image_from_rgb` turns the answer back.
- `test_support.rs`: `cfg(test)` only. The fixtures every `mod tests` here is built from
  (`runnable_settings`, `cacheable_settings`, `status_with_present`,
  `status_with_components`, `FLUX2_ALL_COMPONENTS`, `region_rect`,
  `engine_with_settled_region`). A fixture lives here as soon as a second file needs it;
  it is never copied.

## Contracts and invariants
- **The GUI thread never blocks.** Every function in `wire.rs`,
  `engine/actions.rs` (and the shared prompt-cache jobs it starts) and the IO in `settings.rs`
  and `session.rs` runs on an `ms_thread::spawn` worker; `AiEngine::poll` only drains channels.
- **`normalized()` is the only value that reaches the wire** (`settings.rs`), and
  `effective_paths` is the ONE answer to which three model paths a call uses. Both are
  variant-aware: the derived paths sit under the variant's own model directory, and a
  refused `variant`/`uncensored` pairing is normalized away before it can travel.
- **Weight formats (protocol 21) are in effect in download mode, both variants**
  (`weight_formats_in_effect`). There the derived transformer / encoder paths come from
  `external_catalog::flux2_klein_weights_path` per selected format, encoder source and probed
  bf16 transformer layout — for bf16 the very folders (or the 9B single file) an existing
  install has, so it stays valid — and `params` gain `transformer_format` / `transformer_formats`
  and `text_encoder_format` / `text_encoder_formats` (`[{id, path}]`, best quality first: the
  formats of the current source whose catalog files are installed plus the selected one, whose
  `path` equals the component's `*_path`). Manual mode sends none of the four (a picked `.gguf`
  path travels as-is and the backend derives its format). "Installed" is runtime state carried
  IN the settings value (`installed_formats`, `serde(skip)`) because every request builder
  receives that value and some run on the GUI thread, which must not stat the disk; the engine
  fills it from `probe_flux2_installed_formats` on a worker. Until it does, each list holds the
  selected format alone and the 9B bf16 transformer path is the single file (`Primary`); a
  probe that finds the legacy layout moves that path and re-asks `.status`.
- **The encoder source is a typed axis** (`ms_config::Flux2EncoderSource`): the toggle is drawn
  only where the variant offers more than one source; a switch stores the bool and maps the
  stored encoder format with `closest_offered` (Q5_K_M -> Q4_K_M on the uncensored encoder),
  saying so in one line under the toggle (`encoder_format_notice`).
- **A GGUF encoder excludes fp8 (D8)**: while a GGUF encoder format is in effect `normalized()`
  sends `text_encoder_fp8 = false`; the stored choice is kept. A manual-mode `.gguf` encoder with
  fp8 is refused by the backend by name (Rust does not guess a format from a file name).
- **Both variants download through the app's catalog** (no backend method): the engine owns an
  `OcrModelDownloadController` (`downloads`; `None` only in a test build, where its automatic
  adoption could hash and mark the developer's real model files) and drives it from `poll`
  every frame (`poll_catalog_bundle`): it probes the bundle of the SELECTED source and formats
  (`flux2_catalog_bundle`, empty outside download mode), which also triggers the one-time
  automatic adoption of an unmarked existing download (the backend's former 9B download
  included); the panel draws that bundle through the shared `draw_ocr_model_download` (which
  also draws the per-repository access lines of its gated sources) only while its snapshot
  still describes the current selection. The token row is drawn while a gated spec of the bundle
  is not seen installed (`catalog_gated_repos_missing`); the controller notices a changed token
  itself. The end of a controller job (download / verify / adoption) re-runs the
  installed-formats probe and re-asks `.status`, the forecast and the prompt-cache listing. A
  changed selection (format or source) saves the setting and re-arms the status, the forecast,
  the installed formats and the bundle probe (`fold_resources_and_formats` over
  `Flux2WeightSelection`). The readiness LINE says when the bundle is incomplete
  (`Flux2ReadinessLine::BundleMissing`); the RUN gate stays the `.status` readiness.
- **Format rows on both axes** («Память и скорость», download mode): the shared widget gets
  `Some(&mut transformer_format)` (its rows and a format suggestion) and
  `draw_encoder_formats` draws the encoder's rows under it; the forecast request keeps
  `format: None`, so the backend plans the format the params select. Manual mode passes none.
- **The memory steps are the user's and the wire always carries them** (`vram_step` /
  `ram_step` in every `params`; none of the retired placement fields ever does). A run never
  rewrites them: after a memory failure the backend degrades the run and learns a floor,
  which «Память и скорость» shows with «Забыть»; only `applied.vae_tiling` is written back.
  The forecast never decides anything: the backend's admission is the guard.
- **A damaged settings file is never overwritten.** It is moved aside before defaults load
  and the panel says where; if it cannot be moved (or cannot be read at all), saving is
  blocked for the session and the panel says so (the shared `SettingsProblem`).
- **One latent mask, two runtimes.** `wire.rs::flux2_latent_mask` (and
  `validate_flux2_run_input`) is used by the PyTorch pass AND the sd.cpp request builder, so the
  mask sd.cpp gets equals the PyTorch wire mask byte for byte (pinned by a test), and both
  answer `DoneRaw` with that mask as `sample_mask`.
- **The runtime switch is closed while a catalog job or any operation of either runtime is in
  flight** (`flux2_runtime_switch_block_reason`; the bundle follows the runtime, and after a
  switch the running job would show only as "another download" without its cancel), and an sd.cpp run (or a cancelled one still ending)
  also closes the engine picker (the sd.cpp service runs one generation at a time).
- **`requires_torch()` is the runtime**; `ai_requirement()` is `TorchOrSdcpp` whichever runtime
  is selected (the picker's `AiButton` gate), and `on_selected` moves an out-of-reach PyTorch to
  sd.cpp (the shared `sdcpp_auto_switch`, through `switch_runtime`: `set_runtime` +
  `note_settings_changed` + `note_runtime_changed`, a click's path). It never decides on
  unloaded settings: while the load is in flight the event is kept (`selection_pending`) and
  decided in `poll_settings_load_with` right after the load lands (dropped if the loader died: the defaults are never decided on) — a change before it would make
  the load discard the user's file. On PyTorch without Torch
  the engine's own gate refuses with `cleaning.tools.sdcpp.torch_missing_hint` only where sd.cpp
  can run; elsewhere it says nothing and the host's plain «PyTorch required» line applies.
- **`mask_for_run` is the only place the working mode is decided** (`../shared/region_mask.rs`): a
  painted mask is used with `whole_region = false`; an empty one becomes a
  SOLID mask, because the backend refuses `whole_region = true` otherwise.
- **The wire carries the LATENT mask, the answer is RAW** (protocol 17): the run worker
  dilates the run's mask by `mask_dilate_px` (0 under `whole_region`) with the cv2-exact
  `ms_region_postprocess::dilate_ellipse` and sends that; the backend answers its raw decode
  and `params` carries none of the five mask/post fields (the backend refuses them by name).
  `poll` answers `EnginePoll::DoneRaw` with the run's mask and the latent mask as
  `sample_mask`, and `postprocess_settings()` is `Some` for the engine's whole life (the
  in-memory `settings.postprocess`, normalized): the HOST composites, feathers and
  colour-matches, and re-derives a pending result live when the user edits «Постобработка».
- **«Внимание только к маске» is a persisted CHOICE with an effective wire value**
  (`text_attention_in_mask`, default on). `to_params` sends `setting && !whole_region`, and
  the forecast sends `setting && mask_painted` (`to_profiles_params`) with the
  painted state the host last handed `draw_run_options`; a change of that state re-arms the
  forecast while the option is on, because the backend then counts the attention mask. The backend echoes the effective value as a TOP-LEVEL response field,
  deliberately outside `applied`: `apply_backend_flags` must never write it back, or a
  whole-region run would switch the user's choice off. The checkbox is drawn only in
  `draw_run_options` (pinned under «Обработать»), closed while no mask is painted
  (`flux2_text_attention_block_reason`) and never locked by a run, like every other parameter.
- **Marks travel as a SEPARATE REFERENCE by preference** (`marks_support` in
  `engine/mod.rs`): the host composites the user's marks over a copy of the region and
  hands it over as `RunMarks::Reference`; `run_input` checks it is the region's size and
  the wire appends it as the optional third blob segment (`reference_len`), which the
  backend passes to diffusers as `image_reference`. The region itself stays clean.
  Compositing onto the region (`OverlayOnRegion`) is offered too and needs nothing here.
  `TransparentLayer` is NOT supported — the pipeline flattens every condition image to
  RGB — so a `RunMarks::Layer` that arrives anyway is refused and logged, never dropped.
  The forecast takes no reference (the engine learns of the marks only at
  `start`), so for a marked run the backend's pre-load guard counts a little more than
  the forecast on screen — the reference's condition tokens.
- **One progress bar, claimed by generation** (`../shared/stream_progress.rs`): three operations — a run,
  `.prompt_cache.build`, `.component_action` — share it, and a write from a retired
  generation is dropped. Gates that mean "wait for the current operation" read
  `flux2_pipeline_busy` over all three. The catalog download has its own progress.
- **`Flux2ModelReadiness` is three-state on purpose** (`status.rs`): `Unknown` never
  blocks a run, and a catalog counts only while it still describes the configured paths.
- **An ABSENT `guidance_supported` means SUPPORTED** (`status.rs`, `wire.rs`,
  `decisions.rs`). `.status` reports whether the loaded checkpoint can use guidance at all:
  a checkpoint declaring `"is_distilled": true` makes diffusers switch classifier-free
  guidance off, so a `guidance_scale` above 1.0 only doubles the per-step compute. The
  field is `Option<bool>` like `prompt_cached` and `text_encoder_available`, but its safe
  default is INVERTED — only a positive `false` closes the control, because an older
  backend reports nothing and must not lose a working parameter. The one place that rule
  lives is `flux2_guidance_supported`. The stored `guidance_scale` is never clamped,
  rewritten or hidden by this: the backend owns the run's semantics and the setting has to
  come back on a checkpoint that is not distilled.
- **`FLUX2_MAX_SEQ` (wire.rs) is part of every library entry's key.** Lowering it orphans
  every saved `.msprompt` entry at once.
- **The prompt library (protocol 23).** Every `params` carries `prompt_library`,
  `prompt_cache_precision_order` (`ms_config::flux2_prompt_cache_precision_order`, Rust-owned)
  and, in download mode only, `text_encoder_source`. Which entry a run uses is the BACKEND's
  selection (`selected` of a listing row, `.status.prompt_cache_entry`); this side renders it
  and never re-derives it. The typed text IS a preset when it equals the preset's text after
  trimming (`PromptLibraryListing::find`); the picker's selection, the name field
  (`sync_preset_name`) and the cache line all follow from that every frame, nothing stores it.
  The «Нет» stash (`uncached_prompt`) is persisted in the settings file and never sent; its
  change alone owes a save and re-asks nothing. A rename is a `.build` with a different `name`;
  an empty name field sends none. Choosing a preset only writes its TEXT into the field (the
  settings-changed path); the embeddings are read by the run.
- **The HF token never enters the settings file** and must never be logged; this module only
  edits it (the token row, `HfTokenControl`).
- Layering: only `ui/` may draw, and it decides nothing a test would want to assert —
  the verdicts come from `decisions.rs` and the state belongs to `engine/`. No file here
  reaches another through a `super::sibling::` path, so none depends on another's private
  detail.
- Visibility: nothing in this directory is `pub` except `Flux2KleinEngine`. `pub(super)`
  is the ceiling for everything else, which inside `ui/` and `engine/` already confines an
  item to its own directory.

## Editing map
- To add or change a persisted parameter, see `settings.rs`, then the `ui/` file that
  draws it.
- To add a CHECKPOINT, or any fact that differs between the checkpoints, see
  `config::Flux2Variant` in `crates/ms-config/src/lib.rs` (identity, paths),
  `ms_config::flux2_weight_format` (what it offers), its pinned specs in
  `external_catalog/flux2_klein.rs`, and `variant_presentation.rs` here for its caption;
  `all_engines` then picks it up without a new module.
- To change what differs between the checkpoints on screen, see `ui/install.rs` — the
  uncensored toggle follows `Flux2EncoderSource::offered_for`, the token row the gated specs of
  the bundle (`draw_catalog_download_block`); the format rows of «Память и скорость» are
  `draw_memory_section`, the fp8 gate is `ui/advanced.rs` and the manual encoder FILE button is
  offered where the variant has GGUF encoder formats.
- To change when the catalog bundle is probed, re-probed or downloaded, see
  `poll_catalog_bundle`, `note_weight_formats_changed` and `apply_catalog_action` in
  `engine/mod.rs`.
- To change what the readiness line, the prompt-cache line or a run refusal SAYS, see
  `decisions.rs`; to change what a `.status` answer MEANS, see `status.rs`; to change how
  either LOOKS, see `ui/`.
- To change a request or response shape, see `wire.rs`, or `../shared/prompt_cache.rs` for the
  `.prompt_cache.*` family (its FLUX.2 method names and header are `prompt_cache.rs`). A change to the run blob
  (`flux2_run_request`) is a protocol change: the Python split is `_split_request_blob` in
  `modules/ai_backend/ipc/handlers/flux2_klein.py`, and `PROTOCOL_VERSION` is bumped on
  both sides.
- To change which marks modes the engine accepts or how a marks reference is validated,
  see `marks_support` and `run_input` in `engine/mod.rs`.
- To change what an sd.cpp run loads or sends, see `sdcpp_files.rs` / `sdcpp_request.rs`; its
  lifecycle (status worker, two-stage run, cancel) is `engine/sdcpp.rs`; what the panel shows
  on sd.cpp is `ui/mod.rs` (`draw` by runtime) and `ui/install.rs` (`draw_sdcpp_derived_paths`).
- To change the progress bar's arithmetic or how it looks, see `../shared/` (it is every
  large-model engine's bar). The bar and the run status are drawn by `AiEngine::draw_progress`
  (`draw_flux2_progress_section`), which the host pins above the scrolled parameters.
- To change how a finished run is handed to the host, see `session.rs` (`poll_run`); to change
  the latent mask, see `run_flux2_klein_pass` in `wire.rs`. The post-process itself is
  `ms-region-postprocess` and the host (`region_edit_v2`), not this module.
- To change what the engine DOES when a control is pressed, see `engine/actions.rs`; to
  change what it keeps between frames, see `engine/mod.rs`.
- Tests live beside the code they exercise, in a `#[cfg(test)] mod tests` at the end of
  each file, and their shared fixtures live in `test_support.rs`. A test that genuinely
  spans two files stays in `mod.rs`.
