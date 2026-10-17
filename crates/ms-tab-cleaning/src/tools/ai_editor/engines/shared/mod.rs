/*
File: cleaning/tools/ai_editor/engines/shared/mod.rs

Purpose:
The model-agnostic building blocks the LARGE-model engines of the «ИИ-редактор области» share
(FLUX.2 klein and FLUX.3 klein): the streamed progress bar and its generation guard, the
Hugging Face download (access check, plan, transfer, token controls) with its drawing half, the
settings-file rules (quarantine of a damaged file, save with retry backoff) and the
mask -> working-mode rule. An engine composes these and adds only what is its own.

Submodules (re-exported flat, so an engine writes `use super::shared::*;`):
- `stream_progress`: `EngineProgress`, its generation guard, the rate window, the frame
  parser, `stream_call`, `spawn_backend_cancel`.
- `stream_progress_ui`: `draw_stream_progress`.
- `hf_download`: the check/plan/state vocabulary, `call_hf_download_check`,
  `run_hf_download`, `HfTokenControl`.
- `hf_download_ui`: `draw_hf_token_row`, `draw_hf_repo_row`, `draw_hf_download_controls`,
  `gated_button`.
- `settings_file`: `read_settings_file`, `write_settings_json`, `SettingsSaver`,
  `SettingsProblem`.
- `region_mask`: `mask_for_run`.

Notes:
Nothing here names a model; log lines take a `log_label`, texts are
`cleaning.tools.{engine_progress,hf_download}.*` keys, and anything a model says in its own
words (its settings-file sentences, its download heading) stays in that engine. Visibility
ceiling: `pub(in crate::tools::ai_editor::engines)`.
*/

mod hf_download;
mod hf_download_ui;
mod region_mask;
mod settings_file;
mod stream_progress;
mod stream_progress_ui;

pub(in crate::tools::ai_editor::engines) use hf_download::*;
pub(in crate::tools::ai_editor::engines) use hf_download_ui::*;
pub(in crate::tools::ai_editor::engines) use region_mask::*;
pub(in crate::tools::ai_editor::engines) use settings_file::*;
pub(in crate::tools::ai_editor::engines) use stream_progress::*;
pub(in crate::tools::ai_editor::engines) use stream_progress_ui::*;
