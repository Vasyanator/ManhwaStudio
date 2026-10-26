/*
File: cleaning/tools/ai_editor/engines/shared/mod.rs

Purpose:
The model-agnostic building blocks the LARGE-model engines of the «ИИ-редактор области» share
(FLUX.2 klein and FLUX.3 klein): the streamed progress bar and its generation guard, the
Hugging Face download (access check, plan, transfer, token controls) with its drawing half, the
mask -> working-mode rule, the bottom-docked «Постобработка» section and the prompt-cache
library's parse / pick rules with its preset picker. An engine composes these and adds only what is its own.

Submodules (re-exported flat, so an engine writes `use super::shared::*;`):
- `stream_progress`: `EngineProgress`, its generation guard, the rate window, the frame
  parser, `stream_call`, `spawn_backend_cancel`.
- `stream_progress_ui`: `draw_stream_progress`.
- `hf_download`: the check/plan/state vocabulary, `call_hf_download_check`,
  `run_hf_download`, `HfTokenControl`.
- `hf_download_ui`: `draw_hf_token_row`, `draw_hf_repo_row`, `draw_hf_download_controls`,
  `gated_button`.
- the settings-file rules (`read_settings_file`, `write_settings_json`, `SettingsSaver`,
  `SettingsProblem`) are NOT a submodule here: they are re-exported from their owner
  `region_edit_v2::engine_settings_file`, which every hosted engine (also the cloud
  «ИИ редактирование (API)» one outside `engines/`) uses.
- `region_mask`: `mask_for_run`.
- `postprocess_section`: `draw_postprocess_section`.
- `prompt_library`: the `.prompt_cache.list` / `EntryInfo` shapes and parsers, the exact-text
  preset match, the «Нет» stash and the pick rule (GUI-free).
- `prompt_library_ui`: `draw_prompt_library_picker` (the preset `SearchableComboBox`).

Notes:
Nothing here names a model; log lines take a `log_label`, texts are
`cleaning.tools.{engine_progress,hf_download,prompt_library}.*` keys, and anything a model says in its own
words (its settings-file sentences, its download heading) stays in that engine. Visibility
ceiling: `pub(in crate::tools::ai_editor::engines)`.
*/

mod hf_download;
mod hf_download_ui;
mod postprocess_section;
mod prompt_library;
mod prompt_library_ui;
mod region_mask;
mod stream_progress;
mod stream_progress_ui;

pub(in crate::tools::ai_editor::engines) use hf_download::*;
pub(in crate::tools::ai_editor::engines) use hf_download_ui::*;
pub(in crate::tools::ai_editor::engines) use postprocess_section::*;
pub(in crate::tools::ai_editor::engines) use prompt_library::*;
pub(in crate::tools::ai_editor::engines) use prompt_library_ui::*;
pub(in crate::tools::ai_editor::engines) use region_mask::*;
// Owned by the host framework (shared with the engines outside `engines/`); re-exported so an
// engine keeps writing `use super::shared::*;`.
pub(in crate::tools::ai_editor::engines) use crate::tools::region_edit_v2::engine_settings_file::*;
pub(in crate::tools::ai_editor::engines) use stream_progress::*;
pub(in crate::tools::ai_editor::engines) use stream_progress_ui::*;
