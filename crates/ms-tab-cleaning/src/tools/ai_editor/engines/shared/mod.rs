/*
File: cleaning/tools/ai_editor/engines/shared/mod.rs

Purpose:
The model-agnostic building blocks the LARGE-model engines of the «ИИ-редактор области» share
(FLUX.2 klein and FLUX.3 klein): the streamed progress bar and its generation guard, the
Hugging Face token controls with their drawing half, the mask -> working-mode rule, the
bottom-docked «Постобработка» section, the prompt-cache library's parse / pick rules with its
preset picker, the prompt-cache controller (gates, cache line, wire, jobs) with its drawing
half, the model-catalog download decisions, and the Stable-Diffusion.cpp runtime pieces (run
options, run job, runtime switch, readiness and options widgets). An engine composes these and adds only
what is its own.

Submodules (re-exported flat, so an engine writes `use super::shared::*;`):
- `stream_progress`: `EngineProgress`, its generation guard, the rate window, the frame
  parser, `stream_call`, `spawn_backend_cancel`.
- `stream_progress_ui`: `draw_stream_progress`.
- `hf_download`: `HfTokenControl` + `HfTokenAction` (the token field's secret-store worker).
- `hf_download_ui`: `draw_hf_token_row`, `token_save_button_label`, `gated_button`.
- the settings-file rules (`read_settings_file`, `write_settings_json`, `SettingsSaver`,
  `SettingsProblem`) are NOT a submodule here: they are re-exported from their owner
  `region_edit_v2::engine_settings_file`, which every hosted engine (also the cloud
  «ИИ редактирование (API)» one outside `engines/`) uses.
- `region_mask`: `mask_for_run`.
- `postprocess_section`: `draw_postprocess_section`.
- `prompt_library`: the `.prompt_cache.list` / `EntryInfo` shapes and parsers, the exact-text
  preset match, the «Нет» stash and the pick rule (GUI-free).
- `prompt_library_ui`: `draw_prompt_library_picker` (the preset `SearchableComboBox`); not
  re-exported — engines draw it through `prompt_cache_ui`'s library fold.
- `prompt_cache`: `PromptCacheController` (the operation in flight, the listing query, the
  name field, the export capture), the «Кэшировать» / export / import gates, the cache line,
  the `.prompt_cache.*` header and parsers, the two file-dialog requests (GUI-free).
- `prompt_cache_ui`: `draw_cache_row`, `draw_prompt_cache_line`,
  `draw_prompt_library_toggle`, `draw_prompt_cache_library`.
- `catalog_download`: the pure decisions of an engine whose weights come from the app's model
  catalog — `catalog_gated_repos_missing`, `catalog_bundle_missing`, `catalog_job_running`,
  `catalog_switch_block_reason` (GUI-free).
- `sdcpp_options`: `SdCppRunOptions` + `FlashAttentionChoice` (the persisted per-engine sd.cpp
  options; `to_context` natively).
- `sdcpp_run` (native only): `SdCppRunJob` (start / poll / cancel over the shared progress and
  `AbandonedJobs`; `poll` answers a ready `egui::ColorImage`), the `SdCppGenerate` seam,
  `read_sdcpp_choice`, `spawn_sdcpp_unload`, `sdcpp_route_available`, `rgb_to_color_image`.
- `sdcpp_ui`: `draw_runtime_switch`, `draw_sdcpp_options`, `draw_sdcpp_unload_button`, and
  natively `SdCppReadiness` + `draw_sdcpp_readiness`.

Notes:
Nothing here names a model; log lines take a `log_label`, texts are
`cleaning.tools.{engine_progress,hf_download,prompt_library,catalog_download,sdcpp}.*` keys, and anything a model says in its own
words (its settings-file sentences, its download heading) stays in that engine. Visibility
ceiling: `pub(in crate::tools::ai_editor::engines)`.
*/

mod catalog_download;
mod hf_download;
mod hf_download_ui;
mod postprocess_section;
mod prompt_cache;
mod prompt_cache_ui;
mod prompt_library;
mod prompt_library_ui;
mod region_mask;
mod sdcpp_options;
// `ms-sdcpp` is empty on wasm (its `lib.rs` is `cfg(not(wasm32))`).
#[cfg(not(target_arch = "wasm32"))]
mod sdcpp_run;
mod sdcpp_ui;
mod stream_progress;
mod stream_progress_ui;

pub(in crate::tools::ai_editor::engines) use catalog_download::*;
pub(in crate::tools::ai_editor::engines) use hf_download::*;
pub(in crate::tools::ai_editor::engines) use hf_download_ui::*;
pub(in crate::tools::ai_editor::engines) use postprocess_section::*;
pub(in crate::tools::ai_editor::engines) use prompt_cache::*;
pub(in crate::tools::ai_editor::engines) use prompt_cache_ui::*;
pub(in crate::tools::ai_editor::engines) use prompt_library::*;
pub(in crate::tools::ai_editor::engines) use region_mask::*;
pub(in crate::tools::ai_editor::engines) use sdcpp_options::*;
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::tools::ai_editor::engines) use sdcpp_run::*;
pub(in crate::tools::ai_editor::engines) use sdcpp_ui::*;
// Owned by the host framework (shared with the engines outside `engines/`); re-exported so an
// engine keeps writing `use super::shared::*;`.
pub(in crate::tools::ai_editor::engines) use crate::tools::region_edit_v2::engine_settings_file::*;
pub(in crate::tools::ai_editor::engines) use stream_progress::*;
pub(in crate::tools::ai_editor::engines) use stream_progress_ui::*;
