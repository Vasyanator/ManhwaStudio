# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/shared

## Purpose
The model-agnostic building blocks the LARGE-model engines of the «ИИ-редактор области» share
(FLUX.2 klein and FLUX.3 klein; the prompt library is used by FLUX.2 klein only so far). Each lives here so every large-model engine composes it
instead of copying it. An engine adds only what is its own: its
request headers, its settings document and migrations, its staleness rules, its sentences.

## Architecture
```
engine (flux2_klein/, flux3_klein/)
  -> stream_progress     EngineProgress + generation guard, rate window, frame parser,
                         stream_call (id-exposing streaming call), spawn_backend_cancel
  -> stream_progress_ui  draw_stream_progress[_staged] (one bar, a second while a download names
                         a file; the staged variant names a frame by the engine's stage label)
  -> hf_download         HfDownloadCheck<Q> + verdict vocabulary, hf_download_readiness,
                         call_hf_download_check / run_hf_download, HfTokenControl
  -> hf_download_ui      draw_hf_token_row, draw_hf_repo_row, draw_hf_download_controls,
                         gated_button
  -> (re-export)         read_settings_file, write_settings_json, SettingsSaver — owned by
                         region_edit_v2::engine_settings_file
  -> region_mask         mask_for_run
  -> postprocess_section draw_postprocess_section (the «Постобработка» chrome, bottom-docked)
  -> prompt_library      PromptLibraryListing / Text / Entry + parsers (protocol 23 EntryInfo),
                         find / derived_pick / track_uncached / apply_pick / sync_preset_name
  -> prompt_library_ui   draw_prompt_library_picker (SearchableComboBox, row «Нет» + presets)
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
- `hf_download.rs`: the per-repository verdicts (`HfDownloadState`, `HfDownloadLink`,
  `HfDownloadRepo::link`), the check generic over the engine's QUESTION (`HfDownloadCheck<Q>`,
  `parse_hf_download_check(header, question)`), the nullable plan and `hf_download_readiness`,
  the two calls, the token field's worker (`HfTokenControl`; FLUX.2 klein uses only this part,
  its download being the app's model catalog).
- `hf_download_ui.rs`: the token row (`HfTokenView`, caller-given field id, an optional
  `ms_widgets::HfAccessHint` beside the badge whose last step names `token_save_button_label`),
  one repository row,
  the check / plan / «Скачать» / «Отмена» block (`HfDownloadControlsView`; the download hover is
  the engine's), and `gated_button`.
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

## Contracts and invariants
- **Nothing names a model.** Log lines take a `log_label`; user texts are
  `cleaning.tools.engine_progress.*` / `cleaning.tools.hf_download.*`; a sentence that names the
  model (its settings-file notices, its download heading and hover, its switch-block reasons)
  stays in the engine and reaches shared code only as an already-localized string or a
  `describe` closure.
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
- **An absent plan forbids a size and a completion state**, whatever the verdicts say
  (`hf_download_readiness`); `SizeUnknown` keeps «Скачать» open (`startable`).
- **An unknown verdict literal is "not known"**, keeps the literal for the hover and offers no
  link — never folded onto one of the six.
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
- Visibility ceiling: `pub(in crate::tools::ai_editor::engines)`.

## Editing map
- To change how the bar looks or what its lines say: `stream_progress_ui.rs` and the
  `cleaning.tools.engine_progress.*` keys; its arithmetic: `stream_progress.rs`.
- To change the verdict vocabulary, a verdict's link, or the readiness rule:
  `hf_download.rs` (the wire literals are pinned by the `state` table of `modules/ai_backend/ipc/PROTOCOL.md` and matched by the
  backend's download module).
- To change the token row or the download controls for every engine: `hf_download_ui.rs`.
- To change the quarantine naming, the temp naming, the save debounce or backoff, or the
  teardown flush: `../../../region_edit_v2/engine_settings_file.rs`.
- To change where the «Постобработка» section sits or its chrome: `postprocess_section.rs`; its
  dials and texts: `ms-region-postprocess` (`ui.rs`).
- To change how a library listing is read, what the typed text matches or what «Нет» keeps:
  `prompt_library.rs`; how the picker looks: `prompt_library_ui.rs` and the
  `cleaning.tools.prompt_library.*` keys.
- Tests sit at the end of each file; the catalog tests (every key in every shipped locale, with
  its placeholders) are in `hf_download_ui.rs` and `stream_progress_ui.rs`.
