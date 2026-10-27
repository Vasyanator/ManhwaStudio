# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux3_klein/ui

## Purpose
The parameter panel of the FLUX.3 klein engine — the scrolled body of «Редактор области». The
ONLY part of `flux3_klein` that draws. The progress bar and the run line are drawn by
`AiEngine::draw_progress` (`../engine/mod.rs`, the shared `draw_stream_progress_staged` with
`flux3_stage_label`), pinned above this body by the host.

## Architecture
`mod.rs` owns `Flux3PanelCtx` (the one-frame borrow) and `Flux3PanelIntents` (what the controls
ask for, folded by `Flux3KleinEngine::apply_intents` after the frame), and the body ORDER:

```
settings-file notice -> prompt (any language) -> preset name + «Кэшировать» / «Переименовать»
-> the ONE cache line (the `vl_refs` line in its place) -> «Библиотека промптов» toggle + fold
-> «Референсы» (N of 9, add / remove / remove all)
-> steps + seed (blank = random; the last seed with «Повторить этот сид»)
-> the ONE readiness line («Установить» / «Обновить»)
-> «Установка модели» (install.rs) -> «Память и скорость» (shared widget + encoder rows)
-> «Для экспертов»
   (advanced.rs) -> the mask note
-> «Постобработка» (shared `draw_postprocess_section`, LAST, bottom-docked, collapsed)
```

What is touched per edit is never folded; what is set once per machine always is; no section
nests another.

## Files and submodules
- `mod.rs`: the body, the prompt block (the field, then the shared prompt-cache block of
  `../../shared/prompt_cache_ui.rs` fed with `flux3_cache_context`, `flux3_encoder_caption`, the
  id salts `cleaning_flux3_klein_prompt_cache_name` / `cleaning_flux3_klein_prompt_library` and
  no notes; `track_uncached` keeps the «Нет» stash, raising `uncached_changed`), references,
  steps/seed, readiness and memory blocks (the
  shared widget with the DiT format rows and «Энкодер на CPU», then the shared encoder format
  rows; a picked encoder row goes through `flux3_apply_encoder_pick`, known wire ids only, and
  raises `settings_changed`), and the
  catalog test over every `cleaning.tools.flux3_klein.*` key and the shared section's
  `cleaning.region_postprocess.*` keys (en and ru).
- `install.rs`: «Установка модели» — the two format pickers («Формат весов», «Веса текстового
  энкодера»; rows from `decisions.rs`: name, catalog size, ✓), source-mode switch, the package
  folder row (manual) or the catalog block (repository + licence page, destination, the parts
  present, the token row with `ms_widgets::HfAccessHint` while a gated spec of the bundle is not
  installed, the shared `draw_ocr_model_download` drawn only while its snapshot is the current
  bundle's), the residency line and «Выгрузить из памяти». It opens itself once on the first
  definite non-ready verdict unless the user already moved the fold.
- `advanced.rs`: «Для экспертов» — «Референсы и в текстовый энкодер» (`vl_refs`; closed with the
  reason while a GGUF encoder is selected, D6), «Автоэнкодер в bfloat16», the fp8 path (drawn for
  fp8r only) and «Ограничить референсы». Feather and colour
  match are the shared «Постобработка» section (`../../shared/postprocess_section.rs`, body owned
  by `ms_region_postprocess`), drawn last by `mod.rs` under the stable id `cleaning_flux3_klein`; its edits raise
  `Flux3PanelIntents::postprocess_changed` (a save, no forecast question), never `settings_changed`.

## Contracts and invariants
- **No literals**: every text is a `cleaning.tools.flux3_klein.*` key (shared parts use
  `cleaning.tools.prompt_library.*` (the prompt library),
  `cleaning.tools.hf_download.*`, `translation.ocr_model.*` (the catalog block), `large_models.*`,
  `cleaning.common.*`, `cleaning.region_postprocess.*`); the quant tokens and the ✓ glyph are
  technical literals (`dev-docs/i18n_exclusions.md`); glyph buttons carry
  their meaning on hover. Localized collapsibles and the mode buttons carry stable salts; the
  whole body sits under `push_id("flux3_klein")`.
- **Project widgets only** (`WheelSlider`, `WheelComboBox`; no `egui::Slider` / `ComboBox` /
  `DragValue`).
- **Nothing blocking**: presence marks come from the last `.status` answer, never a filesystem
  probe; file dialogs and reads are intents the engine runs on workers.
- **Decides nothing**: lines and gates come from `../decisions.rs` and the shared
  `prompt_cache_gates` / `prompt_cache_line`; a library press is returned as
  `Flux3PanelIntents::prompt_cache_action` for the engine to start.

## Editing map
- Order or content of the body: `mod.rs`; installation and download: `install.rs`; expert
  switches: `advanced.rs`; what a line says: `../decisions.rs`.
- Feather / colour-match controls: `ms_region_postprocess::draw_postprocess_body` (never here).
